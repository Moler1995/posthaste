//! WebSocket handshake preparation and session transport.
//!
//! A session reuses the HTTP composition rules: [`prepare_handshake`] composes
//! the draft with [`compose_canonical_with_schemes`], drops the headers the
//! upgrade owns, attaches jar cookies, and offers the draft's subprotocols.
//! [`run_session`] then opens one connection, reports it through [`WsEvent`],
//! and drives frames in both directions until either side closes it.
//!
//! The contract is:
//!
//! - Only absolute `ws`/`wss` URLs are accepted, and only before a socket is
//!   opened; every rejection is a return value from [`prepare_handshake`].
//! - The handshake sends no request body.
//! - The draft's timeout bounds the opening handshake only.
//! - At most [`MAX_FRAME_BYTES`] may be sent in one frame, and at most
//!   [`MAX_FRAME_RETAINED`] of a received frame is reported; a larger frame is
//!   received whole, and the row says it was truncated.
//! - A session ends as user-closed, server-closed, or abnormal. An abnormal
//!   end is reported as [`WsEvent::Failed`], including a connection that ends
//!   without a close frame.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::{ClientRequestBuilder, IntoClientRequest as _};
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;

use crate::cookies::{CookieJar, describe_response_cookies, has_manual_cookie_header};
use crate::error::PosthasteError;
use crate::http::{WS_SCHEMES, compose_canonical_with_schemes};
use crate::model::{RequestSpec, ResponseCookie};

/// The largest payload one outgoing frame may carry.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// How much of one received frame is retained.
pub const MAX_FRAME_RETAINED: usize = 1024 * 1024;

/// Header rows the upgrade owns. A user row with one of these names would
/// duplicate or contradict the handshake tungstenite generates, so it is
/// dropped from the composed request.
const RESERVED_HANDSHAKE_HEADERS: &[&str] = &[
    "upgrade",
    "connection",
    "host",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
];

fn is_reserved_handshake_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("sec-websocket-") || RESERVED_HANDSHAKE_HEADERS.contains(&lower.as_str())
}

/// The handshake the server accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsHandshake {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The subprotocol the server selected, when it named one.
    pub protocol: Option<String>,
    /// The cookies the handshake response set, described against the socket's
    /// origin. They are already in the session jar; this is the record the
    /// pane shows, so the tab reports the handshake rather than the jar.
    pub cookies: Vec<ResponseCookie>,
}

/// Which side sent a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsDirection {
    Outgoing,
    Incoming,
}

/// Which side closed the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsCloseBy {
    User,
    Server,
}

/// One thing that happened on a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    /// The upgrade completed. Frames may follow.
    Connected { handshake: WsHandshake },
    /// One data frame, in arrival or send order.
    Frame {
        direction: WsDirection,
        binary: bool,
        /// The retained bytes: the frame itself, or its prefix when truncated.
        payload: Bytes,
        /// The frame's full size, even when only a prefix is retained.
        size: usize,
        truncated: bool,
        at_ms: u64,
    },
    /// The session ended with a closing handshake.
    Closed {
        by: WsCloseBy,
        code: Option<u16>,
        reason: Option<String>,
    },
    /// The session ended without one.
    Failed { message: String },
}

/// What the app asks a running session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsCommand {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

/// A composed handshake ready to open a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedHandshake {
    /// The resolved request URL, which scopes the cookies the response sets.
    pub url: Url,
    /// The header rows to send, including `Cookie` and the offered
    /// subprotocols.
    pub headers: Vec<(String, String)>,
}

impl PreparedHandshake {
    /// Builds the request tungstenite writes. The upgrade's own headers are
    /// added by the client, so only the composed rows travel here.
    fn request(
        &self,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, PosthasteError> {
        let uri: Uri = self
            .url
            .as_str()
            .parse()
            .map_err(|error| PosthasteError::InvalidUrl(format!("{} ({error})", self.url)))?;
        let mut builder = ClientRequestBuilder::new(uri);
        for (name, value) in &self.headers {
            builder = builder.with_header(name.clone(), value.clone());
        }
        builder
            .into_client_request()
            .map_err(|error| PosthasteError::WebSocket {
                message: error.to_string(),
            })
    }
}

/// Composes the opening handshake for a WebSocket draft.
///
/// The body is composed as `None`, so a body kept from an HTTP draft cannot add
/// a `Content-Type` or payload to the upgrade. A `Cookie` row in the draft
/// replaces the jar; otherwise the jar's matching cookies are joined into one
/// header. The draft's subprotocols become the offered protocol list.
pub fn prepare_handshake(
    spec: &RequestSpec,
    cookies: &CookieJar,
) -> Result<PreparedHandshake, PosthasteError> {
    let mut handshake_spec = spec.clone();
    handshake_spec.body = crate::model::RequestBody::None;
    let canonical = compose_canonical_with_schemes(&handshake_spec, WS_SCHEMES)?;
    let url = Url::parse(&canonical.url)
        .map_err(|error| PosthasteError::InvalidUrl(format!("{} ({error})", canonical.url)))?;

    let mut headers: Vec<(String, String)> = canonical
        .headers
        .into_iter()
        .filter(|(name, _)| !is_reserved_handshake_header(name))
        .collect();

    if !has_manual_cookie_header(spec)
        && let Some(matched) = cookies.matching_text(cookie_scope(&url).as_str())
        && !matched.is_empty()
    {
        let joined = matched
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ");
        if !joined.is_empty() {
            headers.push(("Cookie".to_owned(), joined));
        }
    }

    let subprotocols = offered_subprotocols(spec);
    if !subprotocols.is_empty() {
        headers.push(("Sec-WebSocket-Protocol".to_owned(), subprotocols.join(", ")));
    }

    Ok(PreparedHandshake { url, headers })
}

/// The draft's subprotocol list, trimmed with empty entries dropped.
pub fn offered_subprotocols(spec: &RequestSpec) -> Vec<String> {
    spec.subprotocols
        .iter()
        .map(|protocol| protocol.trim())
        .filter(|protocol| !protocol.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Cookies are scoped by an HTTP URL; `ws`/`wss` name the same origin.
fn cookie_scope(url: &Url) -> Url {
    let mut scoped = url.clone();
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => other,
    };
    let _ = scoped.set_scheme(scheme);
    scoped
}

/// Decodes a Base64 composer payload into the bytes of a binary frame.
///
/// Whitespace is ignored so a wrapped payload still decodes; anything else
/// that is not standard Base64 is reported inline.
pub fn decode_base64_payload(text: &str) -> Result<Vec<u8>, String> {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.is_empty() {
        return Ok(Vec::new());
    }
    BASE64
        .decode(compact.as_bytes())
        .map_err(|error| format!("the payload is not valid Base64: {error}"))
}

/// Rejects an outgoing payload larger than [`MAX_FRAME_BYTES`].
pub fn check_outgoing_size(len: usize) -> Result<(), String> {
    if len > MAX_FRAME_BYTES {
        Err(format!(
            "the payload is {} bytes; the limit is {} KiB",
            len,
            MAX_FRAME_BYTES / 1024
        ))
    } else {
        Ok(())
    }
}

/// Opens `handshake` and runs the session until it ends.
///
/// Only the opening handshake is bounded by `timeout`. Every step is reported
/// through `sink`; the function itself returns when the session is over. The
/// cancel receiver aborts a pending connect and, once connected, closes the
/// session as the user's.
pub async fn run_session(
    handshake: PreparedHandshake,
    cookies: Arc<CookieJar>,
    timeout: Duration,
    mut cancel: oneshot::Receiver<()>,
    mut commands: mpsc::UnboundedReceiver<WsCommand>,
    mut sink: impl FnMut(WsEvent) + Send,
) {
    let request = match handshake.request() {
        Ok(request) => request,
        Err(error) => {
            sink(WsEvent::Failed {
                message: error.to_string(),
            });
            return;
        }
    };

    let connected = tokio::select! {
        biased;
        _ = &mut cancel => return,
        result = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(request)) => result,
    };

    let (socket, response) = match connected {
        Ok(Ok(connected)) => connected,
        Ok(Err(error)) => {
            sink(WsEvent::Failed {
                message: describe_tungstenite_error(error),
            });
            return;
        }
        Err(_) => {
            sink(WsEvent::Failed {
                message: "the WebSocket handshake timed out".to_owned(),
            });
            return;
        }
    };

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let protocol = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    // The handshake's cookies belong to the same jar an HTTP request reads, so
    // a login done over a socket is visible everywhere. The scope URL is the
    // HTTP form of the socket's origin.
    let scope = cookie_scope(&handshake.url);
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("set-cookie") {
            cookies.store_set_cookie(value, &scope);
        }
    }
    // Described after the jar took them, so each row says whether it was
    // stored. This is the same record an HTTP response carries.
    let response_cookies = describe_response_cookies(&headers, &scope, &cookies);
    sink(WsEvent::Connected {
        handshake: WsHandshake {
            status,
            headers,
            protocol,
            cookies: response_cookies,
        },
    });

    let (mut write, mut read) = socket.split();
    loop {
        tokio::select! {
            biased;
            _ = &mut cancel => {
                let _ = write.send(Message::Close(None)).await;
                sink(WsEvent::Closed {
                    by: WsCloseBy::User,
                    code: None,
                    reason: None,
                });
                return;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    // The tab dropped the sender: close without reporting a
                    // user action the app no longer shows.
                    let _ = write.send(Message::Close(None)).await;
                    sink(WsEvent::Closed {
                        by: WsCloseBy::User,
                        code: None,
                        reason: None,
                    });
                    return;
                };
                match command {
                    WsCommand::Close => {
                        let _ = write.send(Message::Close(None)).await;
                        sink(WsEvent::Closed {
                            by: WsCloseBy::User,
                            code: None,
                            reason: None,
                        });
                        return;
                    }
                    WsCommand::Text(text) => {
                        let payload = Bytes::from(text.clone().into_bytes());
                        let size = payload.len();
                        if let Err(error) = write.send(Message::Text(text.into())).await {
                            sink(WsEvent::Failed {
                                message: describe_tungstenite_error(error),
                            });
                            return;
                        }
                        sink(WsEvent::Frame {
                            direction: WsDirection::Outgoing,
                            binary: false,
                            payload,
                            size,
                            truncated: false,
                            at_ms: unix_millis_now(),
                        });
                    }
                    WsCommand::Binary(bytes) => {
                        let payload = Bytes::from(bytes);
                        let size = payload.len();
                        if let Err(error) = write.send(Message::Binary(payload.clone())).await {
                            sink(WsEvent::Failed {
                                message: describe_tungstenite_error(error),
                            });
                            return;
                        }
                        sink(WsEvent::Frame {
                            direction: WsDirection::Outgoing,
                            binary: true,
                            payload,
                            size,
                            truncated: false,
                            at_ms: unix_millis_now(),
                        });
                    }
                }
            }
            message = read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let bytes = Bytes::copy_from_slice(text.as_bytes());
                        emit_incoming(&mut sink, bytes, false);
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        emit_incoming(&mut sink, bytes, true);
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // tungstenite queues the pong; flushing writes it on
                        // this loop turn, so no ping row is presented.
                        let _ = write.flush().await;
                    }
                    Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = match frame {
                            Some(frame) => (
                                Some(u16::from(frame.code)),
                                (!frame.reason.is_empty()).then(|| frame.reason.to_string()),
                            ),
                            None => (None, None),
                        };
                        let _ = write.send(Message::Close(None)).await;
                        sink(WsEvent::Closed {
                            by: WsCloseBy::Server,
                            code,
                            reason,
                        });
                        return;
                    }
                    Some(Err(error)) => {
                        sink(WsEvent::Failed {
                            message: describe_tungstenite_error(error),
                        });
                        return;
                    }
                    None => {
                        sink(WsEvent::Failed {
                            message: "the connection closed without a close frame".to_owned(),
                        });
                        return;
                    }
                }
            }
        }
    }
}

/// Reports one received payload, retaining at most [`MAX_FRAME_RETAINED`].
fn emit_incoming(sink: &mut (impl FnMut(WsEvent) + ?Sized), bytes: Bytes, binary: bool) {
    let size = bytes.len();
    let truncated = size > MAX_FRAME_RETAINED;
    let payload = if truncated {
        bytes.slice(..MAX_FRAME_RETAINED)
    } else {
        bytes
    };
    sink(WsEvent::Frame {
        direction: WsDirection::Incoming,
        binary,
        payload,
        size,
        truncated,
        at_ms: unix_millis_now(),
    });
}

/// A failure message that names the cause instead of tungstenite's debug form.
fn describe_tungstenite_error(error: tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;

    match error {
        // A socket that ends without a closing handshake is one abnormal end,
        // whether tungstenite reports it as EOF (`Protocol`) or as a reset.
        Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => "the connection closed without a close frame".to_owned(),
        Error::Http(response) => {
            format!("the server rejected the handshake ({})", response.status())
        }
        Error::Io(error) => format!("connection failed: {error}"),
        Error::Tls(error) => format!("TLS failed: {error}"),
        Error::Url(error) => format!("invalid WebSocket URL: {error}"),
        other => other.to_string(),
    }
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::CookieJar;
    use crate::model::{KeyValue, RequestAuth, RequestBody, RequestProtocol};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio_tungstenite::tungstenite::handshake::server::Response;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::protocol::Message;

    fn spec(url: &str) -> RequestSpec {
        let mut spec = RequestSpec::new("socket");
        spec.protocol = RequestProtocol::WebSocket;
        spec.url = url.to_owned();
        spec
    }

    fn header<'a>(handshake: &'a PreparedHandshake, name: &str) -> Option<&'a str> {
        handshake
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn the_handshake_keeps_user_headers_and_drops_the_upgrade_headers() {
        let mut spec = spec("wss://example.test/socket");
        spec.headers = vec![
            KeyValue::new("Upgrade", "websocket"),
            KeyValue::new("Connection", "keep-alive"),
            KeyValue::new("Host", "evil.test"),
            KeyValue::new("Sec-WebSocket-Key", "abc"),
            KeyValue::new("Sec-WebSocket-Version", "13"),
            KeyValue::new("Sec-WebSocket-Extensions", "permessage-deflate"),
            KeyValue::new("X-Keep", "yes"),
        ];
        let handshake = prepare_handshake(&spec, &CookieJar::default()).expect("compose");

        for reserved in [
            "upgrade",
            "connection",
            "host",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-extensions",
        ] {
            assert_eq!(header(&handshake, reserved), None, "{reserved} is dropped");
        }
        assert_eq!(header(&handshake, "x-keep"), Some("yes"));
        assert_eq!(
            header(&handshake, "cookie"),
            None,
            "an empty jar adds no Cookie row"
        );
    }

    #[test]
    fn the_handshake_carries_the_query_and_auth_but_no_body() {
        let mut spec = spec("wss://example.test/socket");
        spec.query = vec![KeyValue::new("room", "42")];
        spec.auth = RequestAuth::Bearer {
            token: "secret".to_owned(),
        };
        spec.body = RequestBody::Json(r#"{"hello":"world"}"#.to_owned());
        let handshake = prepare_handshake(&spec, &CookieJar::default()).expect("compose");

        assert!(handshake.url.as_str().contains("room=42"));
        assert_eq!(header(&handshake, "authorization"), Some("Bearer secret"));
        assert_eq!(
            header(&handshake, "content-type"),
            None,
            "a preserved body never adds a content type to the upgrade"
        );
    }

    #[test]
    fn offered_subprotocols_are_trimmed_and_skip_empty_entries() {
        let mut spec = spec("ws://example.test/socket");
        spec.subprotocols = vec![
            " graphql-ws ".to_owned(),
            String::new(),
            "   ".to_owned(),
            "json".to_owned(),
        ];

        assert_eq!(
            offered_subprotocols(&spec),
            vec!["graphql-ws".to_owned(), "json".to_owned()]
        );
        let handshake = prepare_handshake(&spec, &CookieJar::default()).expect("compose");
        assert_eq!(
            header(&handshake, "sec-websocket-protocol"),
            Some("graphql-ws, json")
        );
    }

    #[test]
    fn jar_cookies_are_attached_when_the_draft_has_no_cookie_row() {
        let jar = CookieJar::default();
        let scope = Url::parse("http://example.test/").expect("scope");
        jar.add(&scope, "sid", "abc").expect("cookie");

        let handshake =
            prepare_handshake(&spec("ws://example.test/socket"), &jar).expect("compose");

        assert_eq!(header(&handshake, "cookie"), Some("sid=abc"));
    }

    #[test]
    fn a_manual_cookie_row_replaces_the_jar() {
        let jar = CookieJar::default();
        let scope = Url::parse("http://example.test/").expect("scope");
        jar.add(&scope, "sid", "abc").expect("cookie");
        let mut spec = spec("ws://example.test/socket");
        spec.headers = vec![KeyValue::new("Cookie", "manual=1")];

        let handshake = prepare_handshake(&spec, &jar).expect("compose");

        assert_eq!(header(&handshake, "cookie"), Some("manual=1"));
    }

    #[test]
    fn base64_payloads_decode_with_whitespace_ignored() {
        assert_eq!(decode_base64_payload("aGVsbG8=").expect("decode"), b"hello");
        assert_eq!(
            decode_base64_payload("aGVs\n bG8=").expect("decode"),
            b"hello"
        );
        assert!(decode_base64_payload("").expect("empty").is_empty());
        assert!(decode_base64_payload("!!!").is_err());
    }

    #[test]
    fn an_oversized_outgoing_payload_is_rejected() {
        assert!(check_outgoing_size(MAX_FRAME_BYTES).is_ok());
        let error = check_outgoing_size(MAX_FRAME_BYTES + 1).expect_err("over the cap");
        assert!(error.contains("1024 KiB"), "{error}");
    }

    async fn next_event(rx: &mut UnboundedReceiver<WsEvent>) -> WsEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an event in time")
            .expect("the session task is still reporting")
    }

    /// Starts one echo server and returns its address plus the join handle.
    /// The server answers every data message with the same message.
    async fn echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            while let Some(Ok(message)) = socket.next().await {
                if message.is_close() {
                    let _ = socket.close(None).await;
                    break;
                }
                let _ = socket.send(message).await;
            }
        });
        (address, server)
    }

    fn session(
        handshake: PreparedHandshake,
        timeout: Duration,
    ) -> (
        UnboundedReceiver<WsEvent>,
        mpsc::UnboundedSender<WsCommand>,
        oneshot::Sender<()>,
    ) {
        let (events_tx, events_rx) = unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let cookies = Arc::new(CookieJar::default());
        tokio::spawn(async move {
            run_session(
                handshake,
                cookies,
                timeout,
                cancel_rx,
                commands_rx,
                move |event| {
                    let _ = events_tx.send(event);
                },
            )
            .await;
        });
        (events_rx, commands_tx, cancel_tx)
    }

    fn handshake(spec: &RequestSpec) -> PreparedHandshake {
        prepare_handshake(spec, &CookieJar::default()).expect("compose")
    }

    #[tokio::test]
    async fn a_session_connects_exchanges_text_and_closes_as_the_user() {
        let (address, server) = echo_server().await;
        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));

        match next_event(&mut events).await {
            WsEvent::Connected { handshake } => assert_eq!(handshake.status, 101),
            other => panic!("expected a connection, got {other:?}"),
        }

        commands
            .send(WsCommand::Text("ping".to_owned()))
            .expect("command channel");
        match next_event(&mut events).await {
            WsEvent::Frame {
                direction: WsDirection::Outgoing,
                binary: false,
                payload,
                ..
            } => assert_eq!(&payload[..], b"ping"),
            other => panic!("expected the outgoing frame, got {other:?}"),
        }
        match next_event(&mut events).await {
            WsEvent::Frame {
                direction: WsDirection::Incoming,
                binary: false,
                payload,
                ..
            } => assert_eq!(&payload[..], b"ping"),
            other => panic!("expected the echoed frame, got {other:?}"),
        }

        commands
            .send(WsCommand::Binary(vec![0, 1, 2]))
            .expect("command channel");
        match next_event(&mut events).await {
            WsEvent::Frame {
                direction: WsDirection::Outgoing,
                binary: true,
                size: 3,
                ..
            } => {}
            other => panic!("expected the outgoing binary frame, got {other:?}"),
        }
        match next_event(&mut events).await {
            WsEvent::Frame {
                direction: WsDirection::Incoming,
                binary: true,
                size: 3,
                ..
            } => {}
            other => panic!("expected the echoed binary frame, got {other:?}"),
        }

        commands.send(WsCommand::Close).expect("command channel");
        match next_event(&mut events).await {
            WsEvent::Closed { by, .. } => assert_eq!(by, WsCloseBy::User),
            other => panic!("expected a user close, got {other:?}"),
        }
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn a_ping_is_answered_without_a_row() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            socket
                .send(Message::Ping(b"live".to_vec().into()))
                .await
                .expect("ping");
            let answered = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("a pong in time")
                .expect("a frame")
                .expect("a readable frame");
            assert!(matches!(answered, Message::Pong(_)), "{answered:?}");
            let _ = socket.close(None).await;
            while socket.next().await.is_some() {}
        });

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, _commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));
        let _ = next_event(&mut events).await;
        // The next event is the server's close: the pong is answered, never
        // presented as a row of its own.
        match next_event(&mut events).await {
            WsEvent::Closed { by, .. } => assert_eq!(by, WsCloseBy::Server),
            other => panic!("expected the close, got {other:?}"),
        }
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn an_oversized_incoming_frame_is_truncated() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            socket
                .send(Message::Binary(vec![7u8; MAX_FRAME_RETAINED + 1].into()))
                .await
                .expect("binary frame");
            while socket.next().await.is_some() {}
        });

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));
        let _ = next_event(&mut events).await;
        match next_event(&mut events).await {
            WsEvent::Frame {
                direction: WsDirection::Incoming,
                binary: true,
                payload,
                size,
                truncated,
                ..
            } => {
                assert_eq!(size, MAX_FRAME_RETAINED + 1);
                assert_eq!(payload.len(), MAX_FRAME_RETAINED);
                assert!(truncated);
            }
            other => panic!("expected the truncated frame, got {other:?}"),
        }
        commands.send(WsCommand::Close).expect("command channel");
        server.abort();
    }

    #[tokio::test]
    // The handshake callback's error type is tungstenite's `Response`, whose
    // size the test cannot change.
    #[allow(clippy::result_large_err)]
    async fn the_server_selects_a_subprotocol() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        HeaderValue::from_static("graphql-ws"),
                    );
                    Ok(response)
                },
            )
            .await
            .expect("handshake");
            // Hold the socket until the client closes it.
            while socket.next().await.is_some() {}
        });

        let mut spec = spec(&format!("ws://{address}/socket"));
        spec.subprotocols = vec!["graphql-ws".to_owned()];
        let (mut events, commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));
        match next_event(&mut events).await {
            WsEvent::Connected { handshake } => {
                assert_eq!(handshake.protocol.as_deref(), Some("graphql-ws"));
            }
            other => panic!("expected a connection, got {other:?}"),
        }
        commands.send(WsCommand::Close).expect("command channel");
        let _ = next_event(&mut events).await;
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn a_server_close_reports_its_code_and_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            let _ = socket
                .send(Message::Close(Some(
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
                        reason: "going away".into(),
                    },
                )))
                .await;
            while socket.next().await.is_some() {}
        });

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, _commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));
        let _ = next_event(&mut events).await;
        match next_event(&mut events).await {
            WsEvent::Closed { by, code, reason } => {
                assert_eq!(by, WsCloseBy::Server);
                assert_eq!(code, Some(1001));
                assert_eq!(reason.as_deref(), Some("going away"));
            }
            other => panic!("expected a server close, got {other:?}"),
        }
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn a_connection_that_ends_without_a_close_frame_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            drop(socket);
        });

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, _commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));
        let _ = next_event(&mut events).await;
        match next_event(&mut events).await {
            WsEvent::Failed { message } => {
                assert!(message.contains("without a close frame"), "{message}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        server.await.expect("server finished");
    }

    #[tokio::test]
    async fn a_refused_connection_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, _commands, _cancel) = session(handshake(&spec), Duration::from_secs(5));
        match next_event(&mut events).await {
            WsEvent::Failed { message } => {
                assert!(message.contains("connection failed"), "{message}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_handshake_that_never_finishes_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            // Accept the TCP connection and never answer the upgrade.
            let (_stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, _commands, _cancel) =
            session(handshake(&spec), Duration::from_millis(200));
        match next_event(&mut events).await {
            WsEvent::Failed { message } => {
                assert!(message.contains("timed out"), "{message}");
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_pending_connect_is_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let spec = spec(&format!("ws://{address}/socket"));
        let (mut events, _commands, cancel) = session(handshake(&spec), Duration::from_secs(5));
        cancel.send(()).expect("cancel");
        // The task returns without reporting anything.
        let result = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        assert!(
            matches!(result, Ok(None)),
            "a cancelled attempt reports nothing: {result:?}"
        );
        server.abort();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn a_handshake_cookie_is_stored_in_the_jar() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: Response| {
                    response
                        .headers_mut()
                        .insert("set-cookie", HeaderValue::from_static("auth=1; Path=/"));
                    Ok(response)
                },
            )
            .await
            .expect("handshake");
            while socket.next().await.is_some() {}
        });

        let jar = Arc::new(CookieJar::default());
        let (events_tx, mut events_rx) = unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let spec = spec(&format!("ws://{address}/socket"));
        let handshake = prepare_handshake(&spec, &jar).expect("compose");
        let cookies = jar.clone();
        tokio::spawn(async move {
            run_session(
                handshake,
                cookies,
                Duration::from_secs(5),
                cancel_rx,
                commands_rx,
                move |event| {
                    let _ = events_tx.send(event);
                },
            )
            .await;
        });
        let _ = next_event(&mut events_rx).await;
        assert!(
            jar.contains("127.0.0.1", "/", "auth"),
            "the handshake cookie reaches the shared jar"
        );
        commands_tx.send(WsCommand::Close).expect("command channel");
        server.abort();
    }
}
