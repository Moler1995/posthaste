//! Async HTTP execution for Posthaste requests.
//!
//! [`HttpExecutor`] owns one reusable [`reqwest::Client`] and turns a
//! [`RequestSpec`] into a [`ResponseRecord`]. The contract is:
//!
//! - Only absolute `http`/`https` URLs are accepted; enabled query rows are
//!   appended to any query string already present in the URL.
//! - Enabled headers are validated, and a `Content-Type` is supplied for
//!   `Json` (`application/json`) and `Text` (`text/plain`) bodies when the
//!   request does not define one. `Form` bodies are URL-encoded.
//! - [`generated_headers`] reports the headers the client adds on its own
//!   (content type and length, host, accepted types and encodings) so the
//!   editor can present the effective request.
//! - The connect timeout defaults to 10 seconds. The whole request (send plus
//!   body read) is bounded by `RequestSpec::timeout_ms`, or 30 seconds when
//!   unset.
//! - At most 10 redirects are followed.
//! - At most 20 MiB of response body is retained. A larger body is cut at the
//!   cap and [`ResponseRecord::truncated`] is set.
//! - Dropping or signalling the cancellation receiver stops the request and
//!   returns [`PosthasteError::Cancelled`].
//!
//! [`HttpExecutor::execute_streaming`] reports the response as it arrives: one
//! [`ResponseEvent::Started`] after the headers, then one
//! [`ResponseEvent::Chunk`] per read, and the same [`ResponseRecord`] at the
//! end. [`HttpExecutor::execute`] is the same path with the events dropped.
//!
//! Failures are reported as [`PosthasteError::InvalidUrl`],
//! [`PosthasteError::InvalidHeader`], [`PosthasteError::Timeout`],
//! [`PosthasteError::TooManyRedirects`], [`PosthasteError::Cancelled`], or
//! [`PosthasteError::Http`] for connect, TLS, and protocol failures.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::{Bytes, BytesMut};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Method, Url, redirect};
use tokio::sync::oneshot;

use crate::cookies::{CookieJar, describe_response_cookies};
use crate::error::PosthasteError;
use crate::history::REDACTED_VALUE;
use crate::model::{
    ApiKeyLocation, HttpMethod, MultipartPart, RequestAuth, RequestBody, RequestProtocol,
    RequestSpec, ResponseRecord,
};
use crate::storage::WorkspaceStore;

/// Default connect timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default whole-request timeout.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum number of redirects followed.
pub const DEFAULT_REDIRECT_LIMIT: usize = 10;
/// Maximum response body retained in memory.
pub const DEFAULT_BODY_LIMIT: usize = 20 * 1024 * 1024;
/// `Accept` the HTTP client writes when the request does not define one.
pub const DEFAULT_ACCEPT: &str = "*/*";
/// `Accept-Encoding` sent when the request does not define one. It mirrors the
/// decompression features enabled on the workspace's `reqwest` dependency.
pub const DEFAULT_ACCEPT_ENCODING: &str = "gzip,deflate,br";

/// One step of a response as it arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseEvent {
    /// The status line and headers arrived. The body is still being received.
    Started {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// One body chunk, in arrival order. Concatenating every chunk yields
    /// [`ResponseRecord::body`], so a body cut at the cap ends with the
    /// retained tail.
    Chunk(Bytes),
}

/// A header the client supplies itself rather than the request definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedHeader {
    /// Canonical spelling of the header name.
    pub name: String,
    pub value: String,
}

/// Where one composed header came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum HeaderOrigin {
    /// An enabled header row written on the request.
    User,
    /// The request's active credential.
    Auth,
    /// A default the composer or the transport supplies.
    Generated,
}

/// One header in an inspected composition, with the place it came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComposedHeader {
    pub name: String,
    pub value: String,
    pub origin: HeaderOrigin,
}

/// Limits applied to every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub connect_timeout: Duration,
    pub default_timeout: Duration,
    pub redirect_limit: usize,
    pub body_limit: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            default_timeout: DEFAULT_REQUEST_TIMEOUT,
            redirect_limit: DEFAULT_REDIRECT_LIMIT,
            body_limit: DEFAULT_BODY_LIMIT,
        }
    }
}

/// Sends requests with a shared client and configuration.
#[derive(Debug, Clone)]
pub struct HttpExecutor {
    client: Client,
    config: HttpConfig,
    cookies: Arc<CookieJar>,
    /// The workspace a recorded upload path resolves against. An absolute
    /// legacy path never needs it; a relative path without it cannot be sent.
    workspace: Option<WorkspaceStore>,
}

impl HttpExecutor {
    /// Builds an executor with a fresh empty jar. Tests and callers that do not
    /// need to inspect cookies use this; the app shares one jar through
    /// [`Self::with_cookie_jar`].
    pub fn new(config: HttpConfig) -> Result<Self, PosthasteError> {
        Self::with_cookie_jar(config, Arc::new(CookieJar::default()))
    }

    /// Builds an executor whose client attaches and captures cookies in `jar`.
    pub fn with_cookie_jar(
        config: HttpConfig,
        cookies: Arc<CookieJar>,
    ) -> Result<Self, PosthasteError> {
        let redirect_policy = if config.redirect_limit == 0 {
            redirect::Policy::none()
        } else {
            redirect::Policy::limited(config.redirect_limit)
        };

        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(redirect_policy)
            .cookie_provider(cookies.clone())
            .build()
            .map_err(map_reqwest_error)?;

        Ok(Self {
            client,
            config,
            cookies,
            workspace: None,
        })
    }

    /// Returns this executor with upload paths resolved against `workspace`.
    ///
    /// The app sets this per Send, so switching workspaces needs no executor
    /// rebuild; tests without a workspace still send absolute paths.
    pub fn with_workspace(mut self, workspace: WorkspaceStore) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn config(&self) -> &HttpConfig {
        &self.config
    }

    /// The jar the client stores and reads cookies in.
    pub fn cookies(&self) -> &Arc<CookieJar> {
        &self.cookies
    }

    /// Sends `spec`, returning the response or a typed failure.
    ///
    /// This is [`Self::execute_streaming`] with the events dropped, so both
    /// paths share one transport, one set of limits, and one record shape.
    pub async fn execute(
        &self,
        spec: &RequestSpec,
        cancel: oneshot::Receiver<()>,
    ) -> Result<ResponseRecord, PosthasteError> {
        self.execute_streaming(spec, cancel, |_| {}).await
    }

    /// Sends `spec`, reporting the response as it arrives.
    ///
    /// `sink` receives [`ResponseEvent::Started`] once the headers are in and
    /// one [`ResponseEvent::Chunk`] per body read; the return value is the same
    /// record [`Self::execute`] produces. Signalling `cancel` before the body
    /// ends returns [`PosthasteError::Cancelled`] and no record, so nothing
    /// partial is presented as a response.
    pub async fn execute_streaming(
        &self,
        spec: &RequestSpec,
        cancel: oneshot::Receiver<()>,
        mut sink: impl FnMut(ResponseEvent) + Send,
    ) -> Result<ResponseRecord, PosthasteError> {
        let timeout = spec
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.config.default_timeout);

        match tokio::time::timeout(timeout, self.execute_inner(spec, cancel, &mut sink)).await {
            Ok(result) => result,
            Err(_) => Err(PosthasteError::Timeout),
        }
    }

    async fn execute_inner(
        &self,
        spec: &RequestSpec,
        mut cancel: oneshot::Receiver<()>,
        sink: &mut (impl FnMut(ResponseEvent) + ?Sized),
    ) -> Result<ResponseRecord, PosthasteError> {
        let request = compose_request(spec, self.workspace.as_ref()).await?;
        let started = Instant::now();
        let request = request.into_builder(&self.client);

        let mut response = tokio::select! {
            biased;
            _ = &mut cancel => return Err(PosthasteError::Cancelled),
            result = request.send() => result.map_err(map_reqwest_error)?,
        };

        let status = response.status().as_u16();
        // The final URL resolves the default domain and path of any cookie the
        // response sets, and it is what later requests carry the host of.
        let final_url = response.url().clone();
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
        sink(ResponseEvent::Started {
            status,
            headers: headers.clone(),
        });

        let mut body = BytesMut::new();
        let mut truncated = false;
        let limit = self.config.body_limit;

        loop {
            let chunk = tokio::select! {
                biased;
                _ = &mut cancel => return Err(PosthasteError::Cancelled),
                chunk = response.chunk() => chunk.map_err(map_reqwest_error)?,
            };

            let Some(chunk) = chunk else {
                break;
            };

            if body.len() == limit {
                if chunk.is_empty() {
                    continue;
                }
                truncated = true;
                break;
            }

            let remaining = limit - body.len();
            if chunk.len() > remaining {
                let kept = chunk.slice(..remaining);
                body.extend_from_slice(&kept);
                sink(ResponseEvent::Chunk(kept));
                truncated = true;
                break;
            }

            body.extend_from_slice(&chunk);
            sink(ResponseEvent::Chunk(chunk));
        }

        // reqwest's cookie service has already stored these by the time `send`
        // resolves, so `stored` reflects the jar as it stands now.
        let cookies = describe_response_cookies(&headers, &final_url, &self.cookies);

        Ok(ResponseRecord {
            status,
            headers,
            body: body.freeze(),
            duration: started.elapsed(),
            truncated,
            cookies,
        })
    }
}

/// Validates everything that can be checked without touching files or the
/// network.
///
/// An empty or missing upload file is deliberately not an error here: the
/// editor calls this while a file row is half-filled, and Send reports the
/// file problem from [`validate_upload_files`] instead.
pub fn validate_request(spec: &RequestSpec) -> Result<(), PosthasteError> {
    // A WebSocket handshake carries no body, so a body kept from an HTTP draft
    // must not block the connection; only the URL and headers are checked.
    let bodyless;
    let probed = match spec.protocol {
        RequestProtocol::Http => spec,
        RequestProtocol::WebSocket => {
            bodyless = {
                let mut probe = spec.clone();
                probe.body = RequestBody::None;
                probe
            };
            &bodyless
        }
    };
    let schemes = match probed.protocol {
        RequestProtocol::Http => HTTP_SCHEMES,
        RequestProtocol::WebSocket => WS_SCHEMES,
    };
    let canonical = compose_canonical_with_schemes(probed, schemes)?;
    if probed.protocol == RequestProtocol::Http
        && let RequestBody::Json(body) = &probed.body
    {
        validate_json_body(body)?;
    }
    for (name, value) in &canonical.headers {
        validate_header(name, value)?;
    }
    Ok(())
}

/// The headers the client adds to `spec` beyond the rows the request defines,
/// in wire order. These are never persisted: the editor shows them read-only so
/// the effective request is visible.
///
/// A user header with the same name (matched without regard to case) replaces
/// the generated one, matching how [`compose_request`] and the transport layer
/// treat an existing header.
pub fn generated_headers(spec: &RequestSpec, assets_root: Option<&Path>) -> Vec<GeneratedHeader> {
    generated_headers_with_file_length(spec, body_length(&spec.body, assets_root))
}

/// The same headers, with the upload file's length supplied by a caller that
/// has already learned it.
///
/// A file body's `Content-Length` otherwise needs a metadata call, so an
/// editor that caches the file's facts passes them here instead of checking
/// the file again on every repaint. Any other body is measured directly.
pub fn generated_headers_with_file_length(
    spec: &RequestSpec,
    file_length: Option<u64>,
) -> Vec<GeneratedHeader> {
    let user_supplies = |name: &str| {
        spec.headers
            .iter()
            .any(|row| row.enabled && !row.key.is_empty() && row.key.eq_ignore_ascii_case(name))
    };
    let mut headers = Vec::new();

    if matches!(spec.body, RequestBody::Multipart(_)) {
        // reqwest generates the boundary at send, so this row is always the
        // one that goes out: an editable Content-Type row never replaces it.
        headers.push(GeneratedHeader {
            name: "Content-Type".to_owned(),
            value: "multipart/form-data; boundary=<generated at send>".to_owned(),
        });
    } else if let Some(content_type) = default_content_type(&spec.body)
        && !user_supplies("content-type")
    {
        headers.push(GeneratedHeader {
            name: "Content-Type".to_owned(),
            value: content_type,
        });
    }

    let length = match &spec.body {
        RequestBody::File { .. } => file_length,
        body => body_bytes(body).map(|bytes| bytes.len() as u64),
    };
    if let Some(length) = length
        && !user_supplies("content-length")
    {
        headers.push(GeneratedHeader {
            name: "Content-Length".to_owned(),
            value: length.to_string(),
        });
    }

    if let Ok(url) = parse_url(&spec.url)
        && !user_supplies("host")
    {
        headers.push(GeneratedHeader {
            name: "Host".to_owned(),
            // The authority without the user info: host plus a non-default
            // port, exactly what the transport sends.
            value: url[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
        });
    }

    // The HTTP client writes `Accept: */*` itself; a WebSocket handshake is
    // sent by another transport, which does not.
    if spec.protocol == RequestProtocol::Http && !user_supplies("accept") {
        headers.push(GeneratedHeader {
            name: "Accept".to_owned(),
            value: DEFAULT_ACCEPT.to_owned(),
        });
    }

    if !user_supplies("accept-encoding") {
        headers.push(GeneratedHeader {
            name: "Accept-Encoding".to_owned(),
            value: DEFAULT_ACCEPT_ENCODING.to_owned(),
        });
    }

    // The credential is displayed as sent-but-secret: the editor needs to know
    // which header will carry it without printing the value twice.
    if let Some(name) = auth_header_name(&spec.auth)
        && !user_supplies(name)
    {
        headers.push(GeneratedHeader {
            name: name.to_owned(),
            value: REDACTED_VALUE.to_owned(),
        });
    }

    headers
}

/// The header name the active credential supplies, or `None` when it supplies
/// no header. A query-located API key is never a header.
fn auth_header_name(auth: &RequestAuth) -> Option<&str> {
    match auth {
        RequestAuth::None => None,
        RequestAuth::Bearer { token } => (!token.is_empty()).then_some("Authorization"),
        RequestAuth::Basic { username, password } => {
            (!(username.is_empty() && password.is_empty())).then_some("Authorization")
        }
        RequestAuth::ApiKey { key, location, .. } => {
            (!key.is_empty() && *location == ApiKeyLocation::Header).then_some(key.as_str())
        }
    }
}

/// A request after Posthaste's composition rules have been applied, expressed
/// with plain strings so the transport and the script preview share one shape.
///
/// The URL already carries the enabled query rows, the headers already carry
/// the active credential and the default content type, and the body is in its
/// mode. Header names and values stay as written: [`compose_request`] turns
/// them into `reqwest` types, which is where transport validation happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: CanonicalBody,
}

/// The body of a [`CanonicalRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalBody {
    /// The request sends no body.
    None,
    /// JSON or text content, verbatim.
    Raw(String),
    /// Form pairs, already filtered to enabled rows with a non-empty key.
    Form(Vec<(String, String)>),
    /// A single file, by its recorded path. The bytes are read at send.
    File { path: String },
    /// Multipart parts, already filtered to enabled rows with a non-empty key.
    Multipart(Vec<CanonicalPart>),
}

/// One part of a canonical multipart body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalPart {
    Text {
        key: String,
        value: String,
    },
    /// A file part, by its recorded path. The bytes are read at send.
    File {
        key: String,
        path: String,
    },
}

/// Applies every composition rule Send uses, without building `reqwest` types.
///
/// The rendered script must describe the request Send would perform, so both
/// paths consume this function. Failures are the same URL errors
/// [`validate_request`] reports; header validation is left to
/// [`compose_request`] because the preview may show a draft the transport
/// would reject.
pub fn compose_canonical(spec: &RequestSpec) -> Result<CanonicalRequest, PosthasteError> {
    compose_canonical_with_schemes(spec, HTTP_SCHEMES)
}

/// [`compose_canonical`] for a request whose transport accepts other schemes.
///
/// A WebSocket handshake is composed by the same rules and differs only in
/// accepting `ws`/`wss`, so the transport passes [`WS_SCHEMES`] rather than
/// duplicating query, header, and authentication composition.
pub fn compose_canonical_with_schemes(
    spec: &RequestSpec,
    schemes: &[&str],
) -> Result<CanonicalRequest, PosthasteError> {
    compose_with_schemes_and_origins(spec, schemes).map(|(request, _)| request)
}

/// [`compose_canonical_with_schemes`] plus each header's origin and the
/// transport defaults the editor already presents.
///
/// The composed rows are identical to [`compose_canonical_with_schemes`]; the
/// transport defaults reported by [`generated_headers`] are appended only when
/// no composed row already carries the name, so the result is the full set the
/// request would carry.
pub fn compose_canonical_inspected_with_schemes(
    spec: &RequestSpec,
    schemes: &[&str],
    assets_root: Option<&Path>,
) -> Result<(CanonicalRequest, Vec<ComposedHeader>), PosthasteError> {
    let (mut request, mut composed) = compose_with_schemes_and_origins(spec, schemes)?;
    append_transport_defaults(spec, assets_root, &mut request.headers, &mut composed);
    Ok((request, composed))
}

/// [`compose_canonical_inspected_with_schemes`] for HTTP requests.
pub fn compose_canonical_inspected(
    spec: &RequestSpec,
    assets_root: Option<&Path>,
) -> Result<(CanonicalRequest, Vec<ComposedHeader>), PosthasteError> {
    compose_canonical_inspected_with_schemes(spec, HTTP_SCHEMES, assets_root)
}

fn compose_with_schemes_and_origins(
    spec: &RequestSpec,
    schemes: &[&str],
) -> Result<(CanonicalRequest, Vec<ComposedHeader>), PosthasteError> {
    let mut url = UrlComposition::strict_with(spec, schemes)?;
    append_query_rows(&mut url, spec);
    let (headers, composed) = compose_headers(spec, &mut url);

    Ok((
        CanonicalRequest {
            method: spec.method.as_str().to_owned(),
            url: url.finish(),
            headers,
            body: canonical_body(&spec.body),
        },
        composed,
    ))
}

/// Composes the header rows and records where each one came from.
///
/// The composer only ever appends rows — user rows first, then the credential,
/// then the default content type — so the origin list stays in step with the
/// header list without matching names after the fact.
fn compose_headers(
    spec: &RequestSpec,
    url: &mut UrlComposition,
) -> (Vec<(String, String)>, Vec<ComposedHeader>) {
    let mut headers = header_rows(spec);
    let mut composed: Vec<ComposedHeader> = headers
        .iter()
        .map(|(name, value)| ComposedHeader {
            name: name.clone(),
            value: value.clone(),
            origin: HeaderOrigin::User,
        })
        .collect();

    let before = headers.len();
    apply_auth(spec, url, &mut headers);
    push_origins(&mut composed, &headers[before..], HeaderOrigin::Auth);

    let before = headers.len();
    push_default_content_type(spec, &mut headers);
    push_origins(&mut composed, &headers[before..], HeaderOrigin::Generated);

    (headers, composed)
}

fn push_origins(
    composed: &mut Vec<ComposedHeader>,
    added: &[(String, String)],
    origin: HeaderOrigin,
) {
    composed.extend(added.iter().map(|(name, value)| ComposedHeader {
        name: name.clone(),
        value: value.clone(),
        origin,
    }));
}

/// Appends the transport's own rows when no composed row already carries the
/// name. [`generated_headers`] reports them in wire order.
fn append_transport_defaults(
    spec: &RequestSpec,
    assets_root: Option<&Path>,
    headers: &mut Vec<(String, String)>,
    composed: &mut Vec<ComposedHeader>,
) {
    for generated in generated_headers(spec, assets_root) {
        if has_header(headers, &generated.name) {
            continue;
        }
        headers.push((generated.name.clone(), generated.value.clone()));
        composed.push(ComposedHeader {
            name: generated.name,
            value: generated.value,
            origin: HeaderOrigin::Generated,
        });
    }
}

/// Composes a request for the script preview, tolerating a draft that cannot
/// be sent yet.
///
/// When the URL cannot parse — or still holds an unresolved placeholder — the
/// query rows are appended textually with the same encoding Send uses, and
/// braces stay readable instead of turning into `%7B%7B`. Headers are kept as
/// written, so an invalid one does not hide the rest of the preview. The
/// returned notice names the first reason the preview is not a valid request.
pub fn compose_canonical_lenient(spec: &RequestSpec) -> (CanonicalRequest, Option<String>) {
    let (request, _, notice) = compose_lenient_inner(spec, HTTP_SCHEMES);
    (request, notice)
}

/// [`compose_canonical_lenient`] plus header origins and the transport
/// defaults, for a draft that may not be sendable yet.
pub fn compose_canonical_lenient_inspected_with_schemes(
    spec: &RequestSpec,
    schemes: &[&str],
    assets_root: Option<&Path>,
) -> (CanonicalRequest, Vec<ComposedHeader>, Option<String>) {
    let (mut request, mut composed, notice) = compose_lenient_inner(spec, schemes);
    append_transport_defaults(spec, assets_root, &mut request.headers, &mut composed);
    (request, composed, notice)
}

/// [`compose_canonical_lenient_inspected_with_schemes`] for HTTP requests.
pub fn compose_canonical_lenient_inspected(
    spec: &RequestSpec,
    assets_root: Option<&Path>,
) -> (CanonicalRequest, Vec<ComposedHeader>, Option<String>) {
    compose_canonical_lenient_inspected_with_schemes(spec, HTTP_SCHEMES, assets_root)
}

fn compose_lenient_inner(
    spec: &RequestSpec,
    schemes: &[&str],
) -> (CanonicalRequest, Vec<ComposedHeader>, Option<String>) {
    let mut url = UrlComposition::preview_with(spec, schemes);
    let textual = matches!(url, UrlComposition::Textual(_));
    append_query_rows(&mut url, spec);
    let (headers, composed) = compose_headers(spec, &mut url);

    let mut notice = None;
    if textual {
        notice = Some(if spec.url.trim().is_empty() {
            "The URL is empty; this preview is not a valid request yet.".to_owned()
        } else if spec.url.contains("{{") {
            "Unresolved placeholders remain; this preview is not a valid request yet.".to_owned()
        } else {
            "The URL is not valid; the preview shows it as written.".to_owned()
        });
    }
    for (name, value) in &headers {
        if let Err(problem) = validate_header(name, value) {
            notice.get_or_insert_with(|| problem.to_string());
            break;
        }
    }

    (
        CanonicalRequest {
            method: spec.method.as_str().to_owned(),
            url: url.finish(),
            headers,
            body: canonical_body(&spec.body),
        },
        composed,
        notice,
    )
}

/// The canonical URL after the enabled query rows have been appended.
///
/// The strict path is a real [`Url`], so valid drafts get Send's exact
/// encoding. The textual path only exists for the preview: it reproduces the
/// form encoding but keeps `{` and `}` so a missing placeholder stays legible.
enum UrlComposition {
    Parsed(Url),
    Textual(String),
}

impl UrlComposition {
    fn strict_with(spec: &RequestSpec, schemes: &[&str]) -> Result<Self, PosthasteError> {
        Ok(Self::Parsed(parse_url_with_schemes(&spec.url, schemes)?))
    }

    fn preview_with(spec: &RequestSpec, schemes: &[&str]) -> Self {
        // A brace means an unresolved placeholder; parsing would percent-encode
        // it and the preview would no longer show what the user wrote.
        if spec.url.contains("{{") {
            Self::Textual(spec.url.trim().to_owned())
        } else {
            match parse_url_with_schemes(&spec.url, schemes) {
                Ok(url) => Self::Parsed(url),
                Err(_) => Self::Textual(spec.url.trim().to_owned()),
            }
        }
    }

    fn append_rows<'a>(&mut self, rows: impl IntoIterator<Item = (&'a str, &'a str)>) {
        match self {
            Self::Parsed(url) => {
                let mut pairs = url.query_pairs_mut();
                for (key, value) in rows {
                    pairs.append_pair(key, value);
                }
            }
            Self::Textual(text) => {
                for (key, value) in rows {
                    append_textual_pair(text, key, value);
                }
            }
        }
    }

    fn append_pair(&mut self, key: &str, value: &str) {
        match self {
            Self::Parsed(url) => {
                url.query_pairs_mut().append_pair(key, value);
            }
            Self::Textual(text) => append_textual_pair(text, key, value),
        }
    }

    fn finish(self) -> String {
        match self {
            Self::Parsed(url) => url.to_string(),
            Self::Textual(text) => text,
        }
    }
}

fn append_query_rows(url: &mut UrlComposition, spec: &RequestSpec) {
    // An unfinished row — added but not named yet — is not part of the
    // request, so it never turns into a bare `=` pair. Asking a parsed URL for
    // a query at all appends a bare `?`, so skip that when there is nothing to
    // append and leave the request target untouched.
    let query: Vec<(&str, &str)> = enabled_query_pairs(spec);
    if !query.is_empty() {
        url.append_rows(query);
    }
}

fn enabled_query_pairs(spec: &RequestSpec) -> Vec<(&str, &str)> {
    spec.query
        .iter()
        .filter(|row| row.enabled && !row.key.is_empty())
        .map(|row| (row.key.as_str(), row.value.as_str()))
        .collect()
}

/// Appends one pair to a URL string without parsing it, preserving a fragment
/// at the end. The query goes before `#`, as it does on the parsed path.
fn append_textual_pair(url: &mut String, key: &str, value: &str) {
    let (base, fragment) = match url.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (url.as_str(), None),
    };
    let mut composed = String::with_capacity(url.len() + key.len() + value.len() + 4);
    composed.push_str(base);
    if !composed.ends_with(['?', '&']) {
        composed.push(if composed.contains('?') { '&' } else { '?' });
    }
    composed.push_str(&encode_form_component(key));
    composed.push('=');
    composed.push_str(&encode_form_component(value));
    if let Some(fragment) = fragment {
        composed.push('#');
        composed.push_str(fragment);
    }
    *url = composed;
}

/// The form-urlencoded encoding, except that braces survive so an unresolved
/// placeholder can be read. The unchanged set matches `form_urlencoded`.
fn encode_form_component(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'*' | b'-' | b'.' | b'0'..=b'9' | b'A'..=b'Z' | b'_' | b'a'..=b'z' | b'{' | b'}' => {
                output.push(byte as char);
            }
            b' ' => output.push('+'),
            _ => {
                output.push('%');
                output.push(hex_digit(byte >> 4));
                output.push(hex_digit(byte & 0x0f));
            }
        }
    }
    output
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + nibble - 10) as char,
    }
}

/// The enabled header rows, as written. An empty key is an unfinished row, not
/// an invalid header.
fn header_rows(spec: &RequestSpec) -> Vec<(String, String)> {
    spec.headers
        .iter()
        .filter(|row| row.enabled && !row.key.is_empty())
        .map(|row| (row.key.clone(), row.value.clone()))
        .collect()
}

fn push_default_content_type(spec: &RequestSpec, headers: &mut Vec<(String, String)>) {
    if let Some(content_type) = default_content_type(&spec.body)
        && !has_header(headers, "content-type")
    {
        headers.push(("Content-Type".to_owned(), content_type));
    }
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
}

fn canonical_body(body: &RequestBody) -> CanonicalBody {
    match body {
        RequestBody::None => CanonicalBody::None,
        RequestBody::Json(text) | RequestBody::Text(text) => CanonicalBody::Raw(text.clone()),
        RequestBody::Form(rows) => CanonicalBody::Form(
            rows.iter()
                .filter(|row| row.enabled && !row.key.is_empty())
                .map(|row| (row.key.clone(), row.value.clone()))
                .collect(),
        ),
        RequestBody::File { path } => CanonicalBody::File { path: path.clone() },
        RequestBody::Multipart(parts) => CanonicalBody::Multipart(
            parts
                .iter()
                .filter(|part| part.enabled() && !part.key().is_empty())
                .map(|part| match part {
                    MultipartPart::Text { key, value, .. } => CanonicalPart::Text {
                        key: key.clone(),
                        value: value.clone(),
                    },
                    MultipartPart::File { key, path, .. } => CanonicalPart::File {
                        key: key.clone(),
                        path: path.clone(),
                    },
                })
                .collect(),
        ),
    }
}

#[derive(Debug)]
struct ComposedRequest {
    method: Method,
    url: Url,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: ComposedBody,
}

/// The body as the transport will send it.
#[derive(Debug)]
enum ComposedBody {
    None,
    Bytes(Bytes),
    /// A single file, streamed from disk as the request is written.
    Stream(reqwest::Body),
    /// A multipart form; reqwest owns the boundary and any computed length.
    Multipart(reqwest::multipart::Form),
}

impl ComposedRequest {
    fn into_builder(self, client: &Client) -> reqwest::RequestBuilder {
        let mut builder = client
            .request(self.method, self.url)
            .headers(self.headers.into_iter().collect());
        match self.body {
            ComposedBody::None => {}
            ComposedBody::Bytes(bytes) => builder = builder.body(bytes),
            ComposedBody::Stream(body) => builder = builder.body(body),
            ComposedBody::Multipart(form) => builder = builder.multipart(form),
        }
        builder
    }
}

async fn compose_request(
    spec: &RequestSpec,
    workspace: Option<&WorkspaceStore>,
) -> Result<ComposedRequest, PosthasteError> {
    let canonical = compose_canonical(spec)?;
    let assets_root = workspace.map(WorkspaceStore::root);

    // The canonical request keeps header strings as written; this is the one
    // place the transport rules are enforced, so Send's errors are unchanged.
    let mut headers = Vec::with_capacity(canonical.headers.len());
    for (name, value) in &canonical.headers {
        // A multipart request's content type and length are written by reqwest
        // around the generated boundary; a user row of either name is ignored
        // rather than duplicated.
        if matches!(spec.body, RequestBody::Multipart(_))
            && (name.eq_ignore_ascii_case("content-type")
                || name.eq_ignore_ascii_case("content-length"))
        {
            continue;
        }
        validate_header(name, value)?;
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| PosthasteError::InvalidHeader(format!("{name} ({error})")))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|error| PosthasteError::InvalidHeader(format!("{name} ({error})")))?;
        headers.push((header_name, header_value));
    }

    if let RequestBody::Json(body) = &spec.body {
        validate_json_body(body)?;
    }

    let body = match &spec.body {
        RequestBody::Multipart(parts) => {
            let mut form = reqwest::multipart::Form::new();
            for part in parts
                .iter()
                .filter(|part| part.enabled() && !part.key().is_empty())
            {
                match part {
                    MultipartPart::Text { key, value, .. } => {
                        form = form.text(key.clone(), value.clone());
                    }
                    MultipartPart::File { key, path, .. } => {
                        let resolved = resolve_upload_path(path, assets_root)?;
                        form = form.file(key.clone(), resolved).await.map_err(|error| {
                            PosthasteError::InvalidBody(format!("{key}: {path}: {error}"))
                        })?;
                    }
                }
            }
            ComposedBody::Multipart(form)
        }
        RequestBody::File { path } => {
            let resolved = resolve_upload_path(path, assets_root)?;
            let file = tokio::fs::File::open(&resolved)
                .await
                .map_err(|error| PosthasteError::InvalidBody(format!("{path}: {error}")))?;
            if !has_header(&canonical.headers, "content-length") {
                let length = file
                    .metadata()
                    .await
                    .map_err(|error| PosthasteError::InvalidBody(format!("{path}: {error}")))?
                    .len();
                headers.push((
                    HeaderName::from_static("content-length"),
                    HeaderValue::from_str(&length.to_string())
                        .map_err(|error| PosthasteError::InvalidBody(error.to_string()))?,
                ));
            }
            ComposedBody::Stream(reqwest::Body::from(file))
        }
        _ => match canonical.body {
            CanonicalBody::None => ComposedBody::None,
            CanonicalBody::Raw(text) => {
                ComposedBody::Bytes(Bytes::copy_from_slice(text.as_bytes()))
            }
            CanonicalBody::Form(pairs) => {
                ComposedBody::Bytes(Bytes::from(encode_form_pairs(&pairs)))
            }
            CanonicalBody::File { .. } | CanonicalBody::Multipart(_) => {
                unreachable!("an upload body is composed from the request itself")
            }
        },
    };

    Ok(ComposedRequest {
        method: to_method(spec.method),
        url: parse_url(&canonical.url)?,
        headers,
        body,
    })
}

/// Rejects an upload the workspace cannot read before any network work starts.
///
/// This is Send's preflight: it names the row and path so the response pane's
/// error points at the file that needs fixing, and a failure here records no
/// history entry.
pub fn validate_upload_files(
    spec: &RequestSpec,
    workspace: &WorkspaceStore,
) -> Result<(), PosthasteError> {
    match &spec.body {
        RequestBody::File { path } => check_upload_file(path, None, workspace),
        RequestBody::Multipart(parts) => {
            for part in parts {
                let MultipartPart::File { key, path, enabled } = part else {
                    continue;
                };
                if *enabled && !key.is_empty() {
                    check_upload_file(path, Some(key), workspace)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_upload_file(
    path: &str,
    key: Option<&str>,
    workspace: &WorkspaceStore,
) -> Result<(), PosthasteError> {
    let label = match key {
        Some(key) => format!("{key}: {path}"),
        None => path.to_owned(),
    };
    let resolved = if path.trim().is_empty() {
        return Err(PosthasteError::InvalidBody(match key {
            Some(key) => format!("{key}: no file chosen"),
            None => "no file chosen".to_owned(),
        }));
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace.resolve_asset(path).ok_or_else(|| {
            PosthasteError::InvalidBody(format!("{label}: not a path inside the workspace"))
        })?
    };

    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| PosthasteError::InvalidBody(format!("{label}: {error}")))?;
    if !metadata.is_file() {
        return Err(PosthasteError::InvalidBody(format!(
            "{label}: not a regular file"
        )));
    }
    std::fs::File::open(&resolved)
        .map(|_| ())
        .map_err(|error| PosthasteError::InvalidBody(format!("{label}: {error}")))
}

/// The message an invalid header produces, shared by Send and the preview.
pub(crate) fn validate_header(name: &str, value: &str) -> Result<(), PosthasteError> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| PosthasteError::InvalidHeader(format!("{name} ({error})")))?;
    HeaderValue::from_str(value)
        .map_err(|error| PosthasteError::InvalidHeader(format!("{name} ({error})")))?;
    Ok(())
}

/// Applies the request's active credential to the composed URL and headers.
///
/// The request's own enabled rows win: a header the user already supplies is
/// never duplicated, and an API key query parameter yields to a query row with
/// the same key.
fn apply_auth(spec: &RequestSpec, url: &mut UrlComposition, headers: &mut Vec<(String, String)>) {
    match &spec.auth {
        RequestAuth::None => {}
        RequestAuth::Bearer { token } => {
            if !token.is_empty() {
                push_auth_header(headers, "Authorization", format!("Bearer {token}"));
            }
        }
        RequestAuth::Basic { username, password } => {
            if !(username.is_empty() && password.is_empty()) {
                let encoded = BASE64.encode(format!("{username}:{password}"));
                push_auth_header(headers, "Authorization", format!("Basic {encoded}"));
            }
        }
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => {
            if !key.is_empty() {
                match location {
                    ApiKeyLocation::Header => push_auth_header(headers, key, value.clone()),
                    ApiKeyLocation::Query => {
                        let user_supplies =
                            spec.query.iter().any(|row| row.enabled && row.key == *key);
                        if !user_supplies {
                            url.append_pair(key, value);
                        }
                    }
                }
            }
        }
    }
}

fn push_auth_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if has_header(headers, name) {
        return;
    }
    headers.push((name.to_owned(), value));
}

/// The content type a body mode supplies when the request defines none.
///
/// Multipart returns `None`: reqwest writes the boundary-bearing header itself
/// when the form is attached, so composing one here would duplicate it.
fn default_content_type(body: &RequestBody) -> Option<String> {
    match body {
        RequestBody::None | RequestBody::Multipart(_) => None,
        RequestBody::Json(_) => Some("application/json".to_owned()),
        RequestBody::Text(_) => Some("text/plain".to_owned()),
        RequestBody::Form(_) => Some("application/x-www-form-urlencoded".to_owned()),
        RequestBody::File { path } => Some(file_media_type(path)),
    }
}

/// The media type a recorded file path's extension implies, matching the part
/// type reqwest derives for a multipart file. Unknown extensions are
/// `application/octet-stream`.
pub fn file_media_type(path: &str) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

/// The exact bytes an in-memory body mode sends. `None` for a bodyless request
/// and for upload bodies, whose bytes are read at send.
fn body_bytes(body: &RequestBody) -> Option<Bytes> {
    match canonical_body(body) {
        CanonicalBody::None => None,
        CanonicalBody::Raw(text) => Some(Bytes::copy_from_slice(text.as_bytes())),
        CanonicalBody::Form(pairs) => Some(Bytes::from(encode_form_pairs(&pairs))),
        CanonicalBody::File { .. } | CanonicalBody::Multipart(_) => None,
    }
}

/// The encoded body length when it can be known before sending, for the
/// generated-header preview. A file's length comes from its metadata; a
/// multipart body's framing is unknown until reqwest builds it.
pub(crate) fn body_length(body: &RequestBody, assets_root: Option<&Path>) -> Option<u64> {
    match body {
        RequestBody::File { path } => {
            let resolved = resolve_upload_path(path, assets_root).ok()?;
            let metadata = std::fs::metadata(resolved).ok()?;
            metadata.is_file().then_some(metadata.len())
        }
        RequestBody::Multipart(_) => None,
        body => body_bytes(body).map(|bytes| bytes.len() as u64),
    }
}

/// Resolves a recorded upload path: a legacy absolute path as given, a
/// workspace-relative one inside `assets_root` only.
pub(crate) fn resolve_upload_path(
    path: &str,
    assets_root: Option<&Path>,
) -> Result<PathBuf, PosthasteError> {
    if path.trim().is_empty() {
        return Err(PosthasteError::InvalidBody("no file chosen".to_owned()));
    }
    let raw = Path::new(path);
    if raw.is_absolute() {
        return Ok(raw.to_path_buf());
    }
    assets_root
        .and_then(|root| crate::storage::resolve_asset_under(root, path))
        .ok_or_else(|| {
            PosthasteError::InvalidBody(format!("{path} is not a path inside the workspace"))
        })
}

/// Rejects a JSON body that is present but malformed. An empty body is allowed
/// so a request can be sent before its JSON content is filled in.
pub(crate) fn validate_json_body(body: &str) -> Result<(), PosthasteError> {
    if body.trim().is_empty() {
        return Ok(());
    }
    serde_json::from_str::<serde_json::Value>(body)
        .map(|_| ())
        .map_err(|error| PosthasteError::InvalidBody(error.to_string()))
}

/// Schemes an HTTP request accepts.
pub const HTTP_SCHEMES: &[&str] = &["http", "https"];
/// Schemes a WebSocket handshake accepts.
pub const WS_SCHEMES: &[&str] = &["ws", "wss"];

fn parse_url(raw: &str) -> Result<Url, PosthasteError> {
    parse_url_with_schemes(raw, HTTP_SCHEMES)
}

/// Parses an absolute URL whose scheme is one of `schemes`.
///
/// The scheme check is the only transport-specific rule: query, header, and
/// authentication composition run on the parsed [`Url`] afterwards.
pub fn parse_url_with_schemes(raw: &str, schemes: &[&str]) -> Result<Url, PosthasteError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PosthasteError::InvalidUrl("URL is empty".to_owned()));
    }

    let url = Url::parse(trimmed)
        .map_err(|error| PosthasteError::InvalidUrl(format!("{trimmed} ({error})")))?;

    if !schemes.contains(&url.scheme()) {
        return Err(PosthasteError::InvalidUrl(format!(
            "{trimmed} (unsupported scheme '{}'; expected {})",
            url.scheme(),
            schemes.join(" or ")
        )));
    }

    if url.host_str().is_none() {
        return Err(PosthasteError::InvalidUrl(format!(
            "{trimmed} (missing host)"
        )));
    }

    Ok(url)
}

fn to_method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Options => Method::OPTIONS,
    }
}

fn encode_form_pairs(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn map_reqwest_error(error: reqwest::Error) -> PosthasteError {
    classify_reqwest_error(
        error.is_timeout(),
        error.is_redirect(),
        describe_error(&error),
    )
}

fn classify_reqwest_error(is_timeout: bool, is_redirect: bool, message: String) -> PosthasteError {
    if is_timeout {
        PosthasteError::Timeout
    } else if is_redirect {
        PosthasteError::TooManyRedirects
    } else {
        PosthasteError::Http(message)
    }
}

fn describe_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Mutex;
    use std::thread;

    use super::*;
    use crate::model::KeyValue;
    use reqwest::header::CONTENT_TYPE;

    /// Serves `chunks` as one HTTP chunked body, one write per chunk. The
    /// headers go out immediately, so a client sees them before any body bytes.
    struct ChunkedServer {
        addr: SocketAddr,
    }

    impl ChunkedServer {
        fn spawn(delay: Duration, chunks: Vec<Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
            let addr = listener.local_addr().expect("local test server address");
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    if read_request_head(&mut stream).is_err() {
                        continue;
                    }
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                                Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
                        continue;
                    }
                    thread::sleep(delay);
                    for chunk in &chunks {
                        let size = format!("{:X}\r\n", chunk.len());
                        let written = stream.write_all(size.as_bytes()).is_ok()
                            && stream.write_all(chunk).is_ok()
                            && stream.write_all(b"\r\n").is_ok()
                            && stream.flush().is_ok();
                        if !written {
                            break;
                        }
                        thread::sleep(delay);
                    }
                    let _ = stream.write_all(b"0\r\n\r\n");
                    let _ = stream.flush();
                }
            });
            Self { addr }
        }

        fn url(&self) -> String {
            format!("http://{}/stream", self.addr)
        }
    }

    fn read_request_head(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        while let Ok(read) = stream.read(&mut chunk) {
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        Ok(())
    }

    fn streaming_spec(server: &ChunkedServer) -> RequestSpec {
        let mut spec = RequestSpec::new("stream");
        spec.url = server.url();
        spec.timeout_ms = Some(5_000);
        spec
    }

    fn joined_chunks(events: &[ResponseEvent]) -> Vec<u8> {
        let mut joined = Vec::new();
        for event in events {
            if let ResponseEvent::Chunk(bytes) = event {
                joined.extend_from_slice(bytes);
            }
        }
        joined
    }

    /// Cookie tests name their module so `cargo test -p posthaste-core cookies`
    /// runs the store's unit tests and every transport case together.
    mod cookies {
        use super::*;

        /// A one-request-at-a-time server that records each request's head and
        /// answers with whatever `responder` returns.
        struct CaptureServer {
            addr: SocketAddr,
            requests: Arc<Mutex<Vec<String>>>,
        }

        impl CaptureServer {
            fn spawn(responder: impl Fn(&str) -> String + Send + 'static) -> Self {
                let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind capture server");
                let addr = listener.local_addr().expect("capture server address");
                let requests = Arc::new(Mutex::new(Vec::new()));
                let captured = requests.clone();
                thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else {
                            break;
                        };
                        let Ok(request) = read_request_head_text(&mut stream) else {
                            continue;
                        };
                        captured
                            .lock()
                            .expect("requests lock")
                            .push(request.clone());
                        let answer = responder(&request);
                        if stream.write_all(answer.as_bytes()).is_ok() {
                            let _ = stream.flush();
                        }
                    }
                });
                Self { addr, requests }
            }

            fn url(&self, path: &str) -> String {
                format!("http://{}{}", self.addr, path)
            }

            fn requests(&self) -> Vec<String> {
                self.requests.lock().expect("requests lock").clone()
            }
        }

        fn read_request_head_text(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Ok(String::from_utf8_lossy(&buffer).into_owned())
        }

        fn response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
            let mut answer = format!("HTTP/1.1 {status}\r\n");
            for (name, value) in headers {
                answer.push_str(&format!("{name}: {value}\r\n"));
            }
            answer.push_str(&format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ));
            answer
        }

        fn request_header(request: &str, name: &str) -> Option<String> {
            request
                .lines()
                .skip(1)
                .take_while(|line| !line.is_empty())
                .find_map(|line| {
                    let (header, value) = line.split_once(':')?;
                    header
                        .eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_owned())
                })
        }

        fn cookie_test_spec(server: &CaptureServer, path: &str) -> RequestSpec {
            let mut spec = RequestSpec::new("cookie");
            spec.url = server.url(path);
            spec.timeout_ms = Some(5_000);
            spec
        }

        fn cookie_test_executor() -> (HttpExecutor, Arc<CookieJar>) {
            let jar = Arc::new(CookieJar::default());
            let executor = HttpExecutor::with_cookie_jar(HttpConfig::default(), jar.clone())
                .expect("executor");
            (executor, jar)
        }

        #[test]
        fn new_creates_an_empty_jar_and_with_cookie_jar_shares_one() {
            let executor = HttpExecutor::new(HttpConfig::default()).expect("executor");
            assert!(executor.cookies().snapshot().is_empty());

            let jar = Arc::new(CookieJar::default());
            let executor = HttpExecutor::with_cookie_jar(HttpConfig::default(), jar.clone())
                .expect("executor");
            assert!(Arc::ptr_eq(executor.cookies(), &jar));
        }

        #[tokio::test]
        async fn a_login_cookie_is_reused_by_the_next_request() {
            let server = CaptureServer::spawn(|request| {
                if request.starts_with("POST /login ") {
                    response(
                        "200 OK",
                        &[("Set-Cookie", "session=abc; Path=/")],
                        "logged in",
                    )
                } else {
                    response("200 OK", &[], "profile")
                }
            });
            let (executor, jar) = cookie_test_executor();

            let mut login = cookie_test_spec(&server, "/login");
            login.method = HttpMethod::Post;
            let (_cancel, cancel_rx) = oneshot::channel();
            executor.execute(&login, cancel_rx).await.expect("login");

            assert!(jar.contains("127.0.0.1", "/", "session"));
            let (_cancel, cancel_rx) = oneshot::channel();
            executor
                .execute(&cookie_test_spec(&server, "/profile"), cancel_rx)
                .await
                .expect("profile");

            let requests = server.requests();
            assert!(
                request_header(&requests[0], "cookie").is_none(),
                "the login request starts with an empty jar"
            );
            assert_eq!(
                request_header(&requests[1], "cookie").as_deref(),
                Some("session=abc")
            );
        }

        #[tokio::test]
        async fn an_empty_jar_adds_no_cookie_header() {
            let server = CaptureServer::spawn(|_| response("200 OK", &[], "ok"));
            let (executor, jar) = cookie_test_executor();

            let (_cancel, cancel_rx) = oneshot::channel();
            executor
                .execute(&cookie_test_spec(&server, "/plain"), cancel_rx)
                .await
                .expect("request");

            assert!(jar.snapshot().is_empty());
            let requests = server.requests();
            assert!(request_header(&requests[0], "cookie").is_none());
        }

        #[tokio::test]
        async fn a_redirect_hop_stores_and_carries_its_cookie() {
            let server = CaptureServer::spawn(|request| {
                if request.starts_with("GET /start ") {
                    response(
                        "302 Found",
                        &[("Location", "/next"), ("Set-Cookie", "hop=1; Path=/")],
                        "",
                    )
                } else {
                    response("200 OK", &[], "done")
                }
            });
            let (executor, _jar) = cookie_test_executor();

            let (_cancel, cancel_rx) = oneshot::channel();
            executor
                .execute(&cookie_test_spec(&server, "/start"), cancel_rx)
                .await
                .expect("redirected request");

            let requests = server.requests();
            assert_eq!(requests.len(), 2, "the redirect is one extra hop");
            assert!(request_header(&requests[0], "cookie").is_none());
            assert_eq!(
                request_header(&requests[1], "cookie").as_deref(),
                Some("hop=1"),
                "the hop receives the cookie its predecessor stored"
            );
        }

        #[tokio::test]
        async fn a_cross_host_redirect_sends_only_the_new_hosts_cookies() {
            // `localhost` and `127.0.0.1` are different cookie domains even
            // though both reach the same listener, and unlike a second loopback
            // address every platform can resolve and connect to `localhost`.
            let target = CaptureServer::spawn(|_| response("200 OK", &[], "landed"));
            let target_base = format!("http://localhost:{}/", target.addr.port());
            let location = format!("{target_base}landing");
            let origin = CaptureServer::spawn(move |_| {
                response(
                    "302 Found",
                    &[
                        ("Location", location.as_str()),
                        ("Set-Cookie", "origin=1; Path=/"),
                    ],
                    "",
                )
            });
            let (executor, jar) = cookie_test_executor();
            let target_url = crate::cookies::draft_url(&target_base).expect("target URL");
            jar.add(&target_url, "target", "2")
                .expect("seed target cookie");

            let (_cancel, cancel_rx) = oneshot::channel();
            executor
                .execute(&cookie_test_spec(&origin, "/start"), cancel_rx)
                .await
                .expect("redirected request");

            assert!(jar.contains("127.0.0.1", "/", "origin"));
            let requests = target.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                request_header(&requests[0], "cookie").as_deref(),
                Some("target=2"),
                "the original host's cookie is not sent to the new host"
            );
        }

        #[tokio::test]
        async fn an_enabled_cookie_header_replaces_the_jar() {
            let server = CaptureServer::spawn(|_| response("200 OK", &[], "ok"));
            let (executor, jar) = cookie_test_executor();
            let target = crate::cookies::draft_url(&server.url("/")).expect("target URL");
            jar.add(&target, "session", "abc").expect("seed cookie");

            let mut spec = cookie_test_spec(&server, "/");
            spec.headers = vec![KeyValue::new("Cookie", "manual=1")];
            let (_cancel, cancel_rx) = oneshot::channel();
            executor.execute(&spec, cancel_rx).await.expect("request");

            let requests = server.requests();
            assert_eq!(
                request_header(&requests[0], "cookie").as_deref(),
                Some("manual=1")
            );
        }

        #[tokio::test]
        async fn a_disabled_cookie_header_does_not_suppress_the_jar() {
            let server = CaptureServer::spawn(|_| response("200 OK", &[], "ok"));
            let (executor, jar) = cookie_test_executor();
            let target = crate::cookies::draft_url(&server.url("/")).expect("target URL");
            jar.add(&target, "session", "abc").expect("seed cookie");

            let mut spec = cookie_test_spec(&server, "/");
            spec.headers = vec![KeyValue {
                key: "Cookie".to_owned(),
                value: "manual=1".to_owned(),
                enabled: false,
            }];
            let (_cancel, cancel_rx) = oneshot::channel();
            executor.execute(&spec, cancel_rx).await.expect("request");

            let requests = server.requests();
            assert_eq!(
                request_header(&requests[0], "cookie").as_deref(),
                Some("session=abc")
            );
        }

        #[tokio::test]
        async fn a_non_successful_response_still_stores_its_cookie() {
            let server = CaptureServer::spawn(|_| {
                response(
                    "401 Unauthorized",
                    &[("Set-Cookie", "denied=1; Path=/")],
                    "no",
                )
            });
            let (executor, jar) = cookie_test_executor();

            let (_cancel, cancel_rx) = oneshot::channel();
            let record = executor
                .execute(&cookie_test_spec(&server, "/private"), cancel_rx)
                .await
                .expect("a 401 is still a response");

            assert_eq!(record.status, 401);
            assert!(jar.contains("127.0.0.1", "/", "denied"));
            assert_eq!(record.cookies.len(), 1);
            let denied = &record.cookies[0];
            assert_eq!(denied.name, "denied");
            assert_eq!(denied.value.as_deref(), Some("1"));
            assert!(
                denied.stored,
                "a 401 response still describes what the jar kept"
            );
        }

        #[tokio::test]
        async fn the_final_record_describes_every_set_cookie_header() {
            let server = CaptureServer::spawn(|_| {
                response(
                    "200 OK",
                    &[
                        ("Set-Cookie", "session=abc; Path=/; HttpOnly; SameSite=Lax"),
                        ("Set-Cookie", "theme=dark"),
                        ("Set-Cookie", "wide=1; Domain=example.com"),
                        ("Set-Cookie", "not-a-cookie"),
                        ("Set-Cookie", "gone=1; Path=/; Max-Age=0"),
                    ],
                    "ok",
                )
            });
            let (executor, _jar) = cookie_test_executor();

            let (_cancel, cancel_rx) = oneshot::channel();
            let record = executor
                .execute(&cookie_test_spec(&server, "/login"), cancel_rx)
                .await
                .expect("request");

            assert_eq!(
                record.cookies.len(),
                4,
                "the malformed value is skipped: {:?}",
                record.cookies
            );
            let described = |name: &str| {
                record
                    .cookies
                    .iter()
                    .find(|cookie| cookie.name == name)
                    .unwrap_or_else(|| panic!("{name} is described"))
            };

            let session = described("session");
            assert_eq!(session.value.as_deref(), Some("abc"));
            assert_eq!(session.domain, "127.0.0.1");
            assert!(session.host_only);
            assert_eq!(session.path, "/");
            assert!(session.http_only);
            assert_eq!(session.same_site.as_deref(), Some("Lax"));
            assert!(session.stored);

            let theme = described("theme");
            assert_eq!(theme.path, "/", "the default path comes from the request");
            assert!(theme.host_only, "no Domain attribute defaults to the host");
            assert!(theme.stored);

            let wide = described("wide");
            assert_eq!(wide.domain, "example.com");
            assert!(!wide.host_only);
            assert!(
                !wide.stored,
                "the jar rejects a cookie for a foreign domain"
            );

            let gone = described("gone");
            assert!(
                !gone.stored,
                "an expired cookie never enters the jar but is still described"
            );
            assert!(gone.expires_at_ms.is_some());
        }

        #[tokio::test]
        async fn a_redirect_record_describes_only_the_final_hop() {
            let server = CaptureServer::spawn(|request| {
                if request.starts_with("GET /start ") {
                    response(
                        "302 Found",
                        &[("Location", "/next"), ("Set-Cookie", "hop=1; Path=/")],
                        "",
                    )
                } else {
                    response("200 OK", &[("Set-Cookie", "landed=1; Path=/")], "done")
                }
            });
            let (executor, _jar) = cookie_test_executor();

            let (_cancel, cancel_rx) = oneshot::channel();
            let record = executor
                .execute(&cookie_test_spec(&server, "/start"), cancel_rx)
                .await
                .expect("redirected request");

            let names: Vec<&str> = record
                .cookies
                .iter()
                .map(|cookie| cookie.name.as_str())
                .collect();
            assert_eq!(
                names,
                ["landed"],
                "a hop's cookie belongs to the hop that set it"
            );
        }
    }

    #[tokio::test]
    async fn streaming_reports_the_headers_and_each_chunk() {
        let server = ChunkedServer::spawn(
            Duration::from_millis(20),
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()],
        );
        let executor = HttpExecutor::new(HttpConfig::default()).expect("executor");
        let spec = streaming_spec(&server);

        let events = Mutex::new(Vec::new());
        let (_cancel, cancel_rx) = oneshot::channel();
        let record = executor
            .execute_streaming(&spec, cancel_rx, |event| {
                events.lock().expect("events lock").push(event);
            })
            .await
            .expect("streaming request");

        let events = events.into_inner().expect("events lock");
        match &events[0] {
            ResponseEvent::Started { status, headers } => {
                assert_eq!(*status, 200);
                assert!(
                    headers
                        .iter()
                        .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
                            && value == "text/plain"),
                    "the started event carries the response headers"
                );
            }
            other => panic!("the first event must be Started, got {other:?}"),
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ResponseEvent::Chunk(_)))
                .count(),
            3,
            "one event per HTTP chunk"
        );
        assert_eq!(joined_chunks(&events), b"alphabetagamma");
        assert_eq!(record.body.as_ref(), b"alphabetagamma".as_slice());
        assert_eq!(record.status, 200);
        assert!(!record.truncated);
    }

    #[tokio::test]
    async fn execute_returns_the_streamed_record_without_events() {
        let server = ChunkedServer::spawn(
            Duration::from_millis(5),
            vec![b"alpha".to_vec(), b"beta".to_vec()],
        );
        let executor = HttpExecutor::new(HttpConfig::default()).expect("executor");
        let spec = streaming_spec(&server);

        let (_cancel, cancel_rx) = oneshot::channel();
        let record = executor.execute(&spec, cancel_rx).await.expect("request");

        assert_eq!(record.body.as_ref(), b"alphabeta".as_slice());
        assert_eq!(record.status, 200);
        assert!(!record.truncated);
    }

    #[tokio::test]
    async fn streaming_stops_at_the_body_cap() {
        let server = ChunkedServer::spawn(
            Duration::from_millis(5),
            vec![b"aaaa".to_vec(), b"bbbb".to_vec(), b"cccc".to_vec()],
        );
        let executor = HttpExecutor::new(HttpConfig {
            body_limit: 6,
            ..HttpConfig::default()
        })
        .expect("executor");
        let spec = streaming_spec(&server);

        let events = Mutex::new(Vec::new());
        let (_cancel, cancel_rx) = oneshot::channel();
        let record = executor
            .execute_streaming(&spec, cancel_rx, |event| {
                events.lock().expect("events lock").push(event);
            })
            .await
            .expect("streaming request");

        let events = events.into_inner().expect("events lock");
        assert_eq!(joined_chunks(&events), b"aaaabb");
        assert_eq!(record.body.as_ref(), b"aaaabb".as_slice());
        assert!(record.truncated, "the cap reports truncation");
    }

    #[tokio::test]
    async fn streaming_times_out_after_the_headers() {
        let server = ChunkedServer::spawn(
            Duration::from_millis(1_500),
            vec![b"aa".to_vec(), b"bb".to_vec()],
        );
        let executor = HttpExecutor::new(HttpConfig::default()).expect("executor");
        let mut spec = streaming_spec(&server);
        spec.timeout_ms = Some(250);

        let events = Mutex::new(Vec::new());
        let (_cancel, cancel_rx) = oneshot::channel();
        let outcome = executor
            .execute_streaming(&spec, cancel_rx, |event| {
                events.lock().expect("events lock").push(event);
            })
            .await;

        assert!(matches!(outcome, Err(PosthasteError::Timeout)));
        let events = events.into_inner().expect("events lock");
        assert!(matches!(
            events.first(),
            Some(ResponseEvent::Started { status: 200, .. })
        ));
        assert!(
            joined_chunks(&events).is_empty(),
            "no body bytes arrive before the timeout"
        );
    }

    #[tokio::test]
    async fn streaming_cancels_between_chunks() {
        let server = ChunkedServer::spawn(
            Duration::from_millis(80),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()],
        );
        let executor = HttpExecutor::new(HttpConfig::default()).expect("executor");
        let spec = streaming_spec(&server);

        let events = Mutex::new(Vec::new());
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let canceller = Mutex::new(Some(cancel_tx));
        let outcome = executor
            .execute_streaming(&spec, cancel_rx, |event| {
                // Cancel once the first body chunk is in: the Started event
                // arrives before any body byte, and cancelling there would
                // never reach the between-chunks path this test covers.
                let first_chunk = matches!(event, ResponseEvent::Chunk(_));
                events.lock().expect("events lock").push(event);
                if first_chunk && let Some(cancel) = canceller.lock().expect("cancel lock").take() {
                    let _ = cancel.send(());
                }
            })
            .await;

        assert!(matches!(outcome, Err(PosthasteError::Cancelled)));
        let events = events.into_inner().expect("events lock");
        assert_eq!(
            joined_chunks(&events),
            b"one",
            "cancelling stops the body between chunks"
        );
    }

    fn header<'a>(headers: &'a [(HeaderName, HeaderValue)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(candidate, _)| candidate.as_str() == name)
            .and_then(|(_, value)| value.to_str().ok())
    }

    #[test]
    fn config_defaults_match_the_documented_limits() {
        let config = HttpConfig::default();
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.default_timeout, Duration::from_secs(30));
        assert_eq!(config.redirect_limit, 10);
        assert_eq!(config.body_limit, 20 * 1024 * 1024);
    }

    #[tokio::test]
    async fn composes_query_headers_and_json_body() {
        let mut spec = RequestSpec::new("json");
        spec.method = HttpMethod::Post;
        spec.url = "https://example.com/api?keep=1".into();
        spec.query = vec![
            KeyValue::new("page", "2"),
            KeyValue {
                key: "disabled".into(),
                value: "nope".into(),
                enabled: false,
            },
        ];
        spec.headers = vec![
            KeyValue::new("accept", "application/json"),
            KeyValue {
                key: "x-skip".into(),
                value: "yes".into(),
                enabled: false,
            },
        ];
        spec.body = RequestBody::Json(r#"{"ok":true}"#.into());

        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.url.path(), "/api");

        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            query,
            vec![("keep".into(), "1".into()), ("page".into(), "2".into())]
        );

        assert_eq!(header(&request.headers, "accept"), Some("application/json"));
        assert_eq!(header(&request.headers, "x-skip"), None);
        assert_eq!(
            header(&request.headers, "content-type"),
            Some("application/json")
        );
        assert_eq!(
            composed_body_bytes(&request),
            Some(r#"{"ok":true}"#.as_bytes().to_vec())
        );
    }

    #[tokio::test]
    async fn rows_with_an_empty_key_are_not_sent() {
        let mut spec = RequestSpec::new("unnamed rows");
        spec.method = HttpMethod::Post;
        spec.url = "https://example.com/api".into();
        spec.query = vec![KeyValue::new("", ""), KeyValue::new("page", "2")];
        spec.headers = vec![KeyValue::new("", "ignored"), KeyValue::new("x-keep", "1")];
        spec.body = RequestBody::Form(vec![KeyValue::new("", ""), KeyValue::new("field", "value")]);

        let request = compose_request(&spec, None)
            .await
            .expect("an unnamed row does not block the request");
        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query, vec![("page".into(), "2".into())]);
        assert_eq!(header(&request.headers, "x-keep"), Some("1"));
        assert_eq!(header(&request.headers, "accept"), None);
        assert_eq!(composed_body_bytes(&request), Some(b"field=value".to_vec()));
    }

    #[tokio::test]
    async fn user_content_type_wins_and_text_body_is_unchanged() {
        let mut spec = RequestSpec::new("text");
        spec.url = "https://example.com".into();
        spec.headers = vec![KeyValue::new("Content-Type", "text/csv")];
        spec.body = RequestBody::Text("a,b".into());

        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        let content_types: Vec<_> = request
            .headers
            .iter()
            .filter(|(name, _)| name == CONTENT_TYPE)
            .collect();
        assert_eq!(content_types.len(), 1);
        assert_eq!(content_types[0].1, "text/csv");
        assert_eq!(composed_body_bytes(&request), Some(b"a,b".to_vec()));
    }

    #[tokio::test]
    async fn form_body_is_url_encoded_and_skips_disabled_rows() {
        let mut spec = RequestSpec::new("form");
        spec.url = "https://example.com".into();
        spec.body = RequestBody::Form(vec![
            KeyValue::new("name", "posthaste bird"),
            KeyValue::new("symbol", "&"),
            KeyValue {
                key: "skip".into(),
                value: "1".into(),
                enabled: false,
            },
        ]);

        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(
            header(&request.headers, "content-type"),
            Some("application/x-www-form-urlencoded")
        );
        let body = String::from_utf8(composed_body_bytes(&request).unwrap()).unwrap();
        let decoded: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            decoded,
            vec![
                ("name".into(), "posthaste bird".into()),
                ("symbol".into(), "&".into())
            ]
        );
    }

    #[tokio::test]
    async fn none_body_sends_no_body_and_no_content_type() {
        let mut spec = RequestSpec::new("empty");
        spec.url = "https://example.com".into();
        spec.body = RequestBody::None;

        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert!(matches!(request.body, ComposedBody::None));
        assert_eq!(header(&request.headers, "content-type"), None);
    }

    #[test]
    fn generated_headers_describe_what_the_client_supplies() {
        let mut spec = RequestSpec::new("defaults");
        spec.method = HttpMethod::Post;
        spec.url = "https://example.com:8443/api?page=1".into();
        spec.body = RequestBody::Json(r#"{"ok":true}"#.into());

        let headers = generated_headers(&spec, None);
        let value = |name: &str| {
            headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .map(|header| header.value.as_str())
        };
        assert_eq!(value("Content-Type"), Some("application/json"));
        assert_eq!(value("Content-Length"), Some("11"));
        assert_eq!(value("Host"), Some("example.com:8443"));
        assert_eq!(value("Accept"), Some(DEFAULT_ACCEPT));
        assert_eq!(value("Accept-Encoding"), Some(DEFAULT_ACCEPT_ENCODING));
        assert_eq!(headers.len(), 5);
    }

    #[test]
    fn generated_headers_yield_to_enabled_user_rows() {
        let mut spec = RequestSpec::new("user wins");
        spec.url = "https://example.com".into();
        spec.headers = vec![
            KeyValue::new("content-type", "text/csv"),
            KeyValue::new("host", "proxy.internal"),
            KeyValue {
                key: "accept-encoding".into(),
                value: "identity".into(),
                enabled: false,
            },
            KeyValue::new("content-length", "99"),
        ];
        spec.body = RequestBody::Text("a,b".into());

        let headers = generated_headers(&spec, None);
        let names: Vec<_> = headers.iter().map(|header| header.name.as_str()).collect();
        assert_eq!(
            names,
            ["Accept", "Accept-Encoding"],
            "a disabled row is not sent, so the default encoding still applies"
        );
        assert_eq!(headers[1].value, DEFAULT_ACCEPT_ENCODING);
    }

    #[test]
    fn generated_headers_cover_a_bodyless_request_and_an_invalid_url() {
        let mut spec = RequestSpec::new("empty");
        spec.url = "https://example.com".into();
        let headers = generated_headers(&spec, None);
        let names: Vec<_> = headers.iter().map(|header| header.name.as_str()).collect();
        assert_eq!(names, ["Host", "Accept", "Accept-Encoding"]);

        spec.url = "not a url".into();
        let headers = generated_headers(&spec, None);
        let names: Vec<_> = headers.iter().map(|header| header.name.as_str()).collect();
        assert_eq!(
            names,
            ["Accept", "Accept-Encoding"],
            "no host without a URL"
        );
    }

    #[test]
    fn inspected_composition_extends_the_canonical_request() {
        fn fixtures() -> Vec<RequestSpec> {
            let mut url_and_query = RequestSpec::new("url");
            url_and_query.method = HttpMethod::Post;
            url_and_query.url = "https://example.com:8443/api?page=1".into();
            url_and_query.query = vec![
                KeyValue::new("q", "posthaste bird"),
                KeyValue {
                    key: "skipped".into(),
                    value: "1".into(),
                    enabled: false,
                },
            ];
            url_and_query.body = RequestBody::Json(r#"{"ok":true}"#.into());

            let mut header_and_auth = RequestSpec::new("auth");
            header_and_auth.url = "https://example.com/items".into();
            header_and_auth.headers = vec![KeyValue::new("X-Trace", "1")];
            header_and_auth.auth = RequestAuth::Bearer {
                token: "token".into(),
            };

            let mut basic = RequestSpec::new("basic");
            basic.url = "https://example.com".into();
            basic.auth = RequestAuth::Basic {
                username: "user".into(),
                password: "pass".into(),
            };

            let mut api_key_header = RequestSpec::new("api key header");
            api_key_header.url = "https://example.com".into();
            api_key_header.auth = RequestAuth::ApiKey {
                key: "X-Api-Key".into(),
                value: "secret".into(),
                location: ApiKeyLocation::Header,
            };

            let mut api_key_query = RequestSpec::new("api key query");
            api_key_query.url = "https://example.com".into();
            api_key_query.auth = RequestAuth::ApiKey {
                key: "api_key".into(),
                value: "secret".into(),
                location: ApiKeyLocation::Query,
            };

            let mut multipart = RequestSpec::new("multipart");
            multipart.method = HttpMethod::Post;
            multipart.url = "https://example.com/upload".into();
            multipart.body = RequestBody::Multipart(vec![MultipartPart::text("field", "value")]);

            let mut user_rows = RequestSpec::new("user rows");
            user_rows.url = "https://example.com".into();
            user_rows.headers = vec![
                KeyValue::new("host", "mirror.internal"),
                KeyValue::new("accept-encoding", "identity"),
                KeyValue::new("content-type", "text/csv"),
            ];
            user_rows.body = RequestBody::Text("a,b".into());

            vec![
                url_and_query,
                header_and_auth,
                basic,
                api_key_header,
                api_key_query,
                multipart,
                user_rows,
            ]
        }

        for spec in fixtures() {
            let canonical = compose_canonical(&spec).expect("the fixture composes");
            let (inspected, composed) =
                compose_canonical_inspected(&spec, None).expect("the fixture inspects");

            assert_eq!(inspected.method, canonical.method);
            assert_eq!(inspected.url, canonical.url, "{}", spec.name);
            assert_eq!(inspected.body, canonical.body);
            assert_eq!(
                &inspected.headers[..canonical.headers.len()],
                canonical.headers.as_slice(),
                "{} keeps the composed request as its prefix",
                spec.name
            );

            let expected: Vec<(String, String)> = generated_headers(&spec, None)
                .into_iter()
                .filter(|generated| {
                    !canonical
                        .headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(&generated.name))
                })
                .map(|generated| (generated.name, generated.value))
                .collect();
            assert_eq!(
                inspected.headers[canonical.headers.len()..],
                expected[..],
                "{} appends exactly the transport defaults",
                spec.name
            );

            let user_rows = spec
                .headers
                .iter()
                .filter(|row| row.enabled && !row.key.is_empty())
                .count();
            let credential = auth_header_name(&spec.auth);
            for (index, origin) in composed.iter().enumerate() {
                let (name, value) = &inspected.headers[index];
                assert_eq!((&origin.name, &origin.value), (name, value));
                let expected_origin = if index >= canonical.headers.len() {
                    HeaderOrigin::Generated
                } else if index < user_rows {
                    HeaderOrigin::User
                } else if credential.is_some_and(|credential| credential.eq_ignore_ascii_case(name))
                {
                    HeaderOrigin::Auth
                } else {
                    HeaderOrigin::Generated
                };
                assert_eq!(origin.origin, expected_origin, "{}: {name}", spec.name);
            }
        }
    }

    #[test]
    fn generated_content_length_matches_the_form_encoding() {
        let mut spec = RequestSpec::new("form");
        spec.url = "https://example.com".into();
        spec.body = RequestBody::Form(vec![
            KeyValue::new("name", "posthaste bird"),
            KeyValue::new("disabled", "1"),
        ]);

        let headers = generated_headers(&spec, None);
        let length = headers
            .iter()
            .find(|header| header.name == "Content-Length")
            .map(|header| header.value.parse::<usize>().expect("a byte count"))
            .expect("a form body has a length");
        let sent = body_bytes(&spec.body).expect("form bytes");
        assert_eq!(length, sent.len());
    }

    #[tokio::test]
    async fn bearer_and_basic_credentials_add_authorization() {
        let mut spec = RequestSpec::new("bearer");
        spec.url = "https://example.com".into();
        spec.auth = RequestAuth::Bearer {
            token: "abc".into(),
        };
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(
            header(&request.headers, "authorization"),
            Some("Bearer abc")
        );

        spec.auth = RequestAuth::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(
            header(&request.headers, "authorization"),
            Some("Basic dXNlcjpwYXNz"),
            "Basic credentials are base64-encoded user:password"
        );
        assert_eq!(
            request
                .headers
                .iter()
                .filter(|(name, _)| name == "authorization")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn an_api_key_goes_to_its_header_or_query() {
        let mut spec = RequestSpec::new("api key");
        spec.url = "https://example.com/api".into();
        spec.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".into(),
            value: "secret".into(),
            location: ApiKeyLocation::Header,
        };
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(header(&request.headers, "x-api-key"), Some("secret"));

        spec.auth = RequestAuth::ApiKey {
            key: "api_key".into(),
            value: "secret".into(),
            location: ApiKeyLocation::Query,
        };
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(header(&request.headers, "x-api-key"), None);
        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query, vec![("api_key".into(), "secret".into())]);
    }

    #[tokio::test]
    async fn user_rows_win_over_the_credential() {
        let mut spec = RequestSpec::new("mine");
        spec.url = "https://example.com/api".into();
        spec.headers = vec![KeyValue::new("Authorization", "Bearer mine")];
        spec.query = vec![KeyValue::new("api_key", "mine")];
        spec.auth = RequestAuth::Bearer {
            token: "theirs".into(),
        };
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        assert_eq!(
            header(&request.headers, "authorization"),
            Some("Bearer mine")
        );
        assert_eq!(
            request
                .headers
                .iter()
                .filter(|(name, _)| name == "authorization")
                .count(),
            1
        );

        spec.auth = RequestAuth::ApiKey {
            key: "api_key".into(),
            value: "theirs".into(),
            location: ApiKeyLocation::Query,
        };
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query, vec![("api_key".into(), "mine".into())]);

        // A disabled row is not sent, so the credential applies.
        spec.query[0].enabled = false;
        let request = compose_request(&spec, None)
            .await
            .expect("request composes");
        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query, vec![("api_key".into(), "theirs".into())]);
    }

    #[tokio::test]
    async fn empty_credentials_add_nothing() {
        let mut spec = RequestSpec::new("empty auth");
        spec.url = "https://example.com/api".into();

        for auth in [
            RequestAuth::Bearer {
                token: String::new(),
            },
            RequestAuth::Basic {
                username: String::new(),
                password: String::new(),
            },
            RequestAuth::ApiKey {
                key: String::new(),
                value: "secret".into(),
                location: ApiKeyLocation::Header,
            },
            RequestAuth::ApiKey {
                key: String::new(),
                value: "secret".into(),
                location: ApiKeyLocation::Query,
            },
        ] {
            spec.auth = auth;
            let request = compose_request(&spec, None)
                .await
                .expect("request composes");
            assert_eq!(header(&request.headers, "authorization"), None);
            assert!(request.url.query().is_none());
        }
    }

    #[test]
    fn generated_headers_show_the_credential_as_masked() {
        let mut spec = RequestSpec::new("masked");
        spec.url = "https://example.com".into();
        let value = |spec: &RequestSpec, name: &str| {
            generated_headers(spec, None)
                .into_iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .map(|header| header.value)
        };

        spec.auth = RequestAuth::Bearer {
            token: "secret".into(),
        };
        assert_eq!(
            value(&spec, "Authorization").as_deref(),
            Some(REDACTED_VALUE)
        );

        spec.auth = RequestAuth::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        assert_eq!(
            value(&spec, "Authorization").as_deref(),
            Some(REDACTED_VALUE)
        );

        spec.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".into(),
            value: "secret".into(),
            location: ApiKeyLocation::Header,
        };
        assert_eq!(value(&spec, "X-Api-Key").as_deref(), Some(REDACTED_VALUE));
        assert_eq!(value(&spec, "Authorization"), None);

        spec.auth = RequestAuth::ApiKey {
            key: "api_key".into(),
            value: "secret".into(),
            location: ApiKeyLocation::Query,
        };
        assert_eq!(
            generated_headers(&spec, None)
                .into_iter()
                .filter(|header| header.name.eq_ignore_ascii_case("api_key"))
                .count(),
            0,
            "a query credential is not a header"
        );

        // An enabled user row replaces the generated credential row.
        spec.auth = RequestAuth::Bearer {
            token: "secret".into(),
        };
        spec.headers = vec![KeyValue::new("authorization", "Bearer mine")];
        assert_eq!(value(&spec, "Authorization"), None);
    }

    #[test]
    fn rejects_empty_relative_and_unsupported_urls() {
        for url in ["", "example.com/api", "ftp://example.com/file"] {
            let mut spec = RequestSpec::new("bad");
            spec.url = url.into();
            assert!(
                matches!(validate_request(&spec), Err(PosthasteError::InvalidUrl(_))),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn each_protocol_accepts_only_its_own_schemes() {
        let cases = [
            (RequestProtocol::Http, "http://example.com/api", true),
            (RequestProtocol::Http, "https://example.com/api", true),
            (RequestProtocol::Http, "wss://example.com/socket", false),
            (RequestProtocol::WebSocket, "ws://example.com/socket", true),
            (RequestProtocol::WebSocket, "wss://example.com/socket", true),
            (RequestProtocol::WebSocket, "https://example.com/api", false),
        ];
        for (protocol, url, accepted) in cases {
            let mut spec = RequestSpec::new("scheme");
            spec.protocol = protocol;
            spec.url = url.to_owned();
            let result = validate_request(&spec);
            assert_eq!(
                result.is_ok(),
                accepted,
                "{protocol} should {} accept {url}: {result:?}",
                if accepted { "" } else { "not" }
            );
        }
    }

    #[test]
    fn a_websocket_draft_ignores_an_http_body() {
        let mut spec = RequestSpec::new("body");
        spec.protocol = RequestProtocol::WebSocket;
        spec.url = "wss://example.com/socket".to_owned();
        spec.body = RequestBody::Json("{not json".to_owned());

        assert!(
            validate_request(&spec).is_ok(),
            "the handshake never sends the body, so its JSON is not checked"
        );
    }

    #[test]
    fn rejects_invalid_header_names() {
        let mut spec = RequestSpec::new("bad header");
        spec.url = "https://example.com".into();
        spec.headers = vec![KeyValue::new("bad header", "value")];

        assert!(matches!(
            validate_request(&spec),
            Err(PosthasteError::InvalidHeader(_))
        ));
    }

    #[test]
    fn rejects_malformed_json_but_allows_empty_bodies() {
        let mut spec = RequestSpec::new("json");
        spec.url = "https://example.com".into();

        spec.body = RequestBody::Json(r#"{"ok":true}"#.into());
        assert!(validate_request(&spec).is_ok());

        spec.body = RequestBody::Json(String::new());
        assert!(validate_request(&spec).is_ok());

        spec.body = RequestBody::Json("   \n  ".into());
        assert!(validate_request(&spec).is_ok());

        spec.body = RequestBody::Json(r#"{"ok":"#.into());
        assert!(matches!(
            validate_request(&spec),
            Err(PosthasteError::InvalidBody(message)) if !message.is_empty()
        ));
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_before_any_network_request() {
        let executor = HttpExecutor::new(HttpConfig::default()).expect("executor");
        let mut spec = RequestSpec::new("json");
        spec.url = "http://127.0.0.1:1".into();
        spec.timeout_ms = Some(5_000);
        spec.body = RequestBody::Json("{".into());

        let (_cancel, cancel_rx) = oneshot::channel();
        let outcome = executor.execute(&spec, cancel_rx).await;

        assert!(matches!(outcome, Err(PosthasteError::InvalidBody(_))));
    }

    #[test]
    fn classifies_reqwest_failures() {
        assert!(matches!(
            classify_reqwest_error(true, false, "late".into()),
            PosthasteError::Timeout
        ));
        assert!(matches!(
            classify_reqwest_error(false, true, "loop".into()),
            PosthasteError::TooManyRedirects
        ));
        assert!(matches!(
            classify_reqwest_error(false, false, "refused".into()),
            PosthasteError::Http(message) if message == "refused"
        ));
        assert!(matches!(
            PosthasteError::Cancelled,
            PosthasteError::Cancelled
        ));
    }

    #[test]
    fn error_description_includes_sources() {
        let error = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(describe_error(&error), "refused");
    }

    #[tokio::test]
    async fn tokio_timeout_feature_is_available() {
        let outcome = tokio::time::timeout(Duration::from_millis(10), async {
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;

        assert!(outcome.is_err());
    }

    /// The drafts the equivalence test exercises: each one reaches a different
    /// composition rule (query rows, bodies, credentials).
    fn equivalence_drafts() -> Vec<RequestSpec> {
        let mut get = RequestSpec::new("get");
        get.url = "https://example.com/items?keep=1#frag".into();
        get.query = vec![
            KeyValue::new("a", "1"),
            KeyValue::new("a", "2"),
            KeyValue {
                key: "debug".into(),
                value: "1".into(),
                enabled: false,
            },
            KeyValue::new("", "ignored"),
        ];

        let mut json = RequestSpec::new("json");
        json.method = HttpMethod::Post;
        json.url = "https://example.com/api".into();
        json.headers = vec![KeyValue::new("Content-Type", "application/json")];
        json.body = RequestBody::Json(r#"{"ok":true}"#.into());

        let mut form = RequestSpec::new("form");
        form.method = HttpMethod::Put;
        form.url = "https://example.com/form".into();
        form.body = RequestBody::Form(vec![
            KeyValue::new("name", "posthaste bird"),
            KeyValue::new("", "ignored"),
            KeyValue {
                key: "disabled".into(),
                value: "1".into(),
                enabled: false,
            },
        ]);

        let mut text = RequestSpec::new("text");
        text.method = HttpMethod::Patch;
        text.url = "https://example.com/text".into();
        text.body = RequestBody::Text("a,b".into());

        let mut bearer = RequestSpec::new("bearer");
        bearer.url = "https://example.com/api".into();
        bearer.auth = RequestAuth::Bearer {
            token: "abc".into(),
        };

        let mut basic = RequestSpec::new("basic");
        basic.url = "https://example.com/api".into();
        basic.auth = RequestAuth::Basic {
            username: "user".into(),
            password: "pass".into(),
        };

        let mut api_header = RequestSpec::new("api header");
        api_header.url = "https://example.com/api".into();
        api_header.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".into(),
            value: "secret".into(),
            location: ApiKeyLocation::Header,
        };

        let mut api_query = RequestSpec::new("api query");
        api_query.url = "https://example.com/api".into();
        api_query.auth = RequestAuth::ApiKey {
            key: "api_key".into(),
            value: "secret".into(),
            location: ApiKeyLocation::Query,
        };

        vec![get, json, form, text, bearer, basic, api_header, api_query]
    }

    #[tokio::test]
    async fn canonical_composition_matches_the_sent_request() {
        for spec in equivalence_drafts() {
            let canonical = compose_canonical(&spec).expect("canonical composes");
            let request = compose_request(&spec, None)
                .await
                .expect("request composes");

            assert_eq!(canonical.method, request.method.as_str(), "{spec:?}");
            assert_eq!(canonical.url, request.url.to_string(), "{spec:?}");
            assert_eq!(
                canonical.headers.len(),
                request.headers.len(),
                "same header count for {spec:?}"
            );
            for ((canonical_name, canonical_value), (sent_name, sent_value)) in
                canonical.headers.iter().zip(&request.headers)
            {
                assert_eq!(
                    canonical_name.to_ascii_lowercase(),
                    sent_name.as_str(),
                    "same header name order for {spec:?}"
                );
                assert_eq!(
                    canonical_value.as_bytes(),
                    sent_value.as_bytes(),
                    "same header value for {canonical_name} in {spec:?}"
                );
            }
            assert_eq!(
                canonical_body_bytes(&canonical.body),
                composed_body_bytes(&request),
                "same body for {spec:?}"
            );
        }
    }

    /// Every script format describes the same URL and headers Send would
    /// transmit. The cookie jar is not consulted, so no cookie header appears.
    #[tokio::test]
    async fn every_script_format_matches_the_sent_request() {
        use crate::script::{ScriptFormat, render_script};

        for spec in equivalence_drafts() {
            let canonical = compose_canonical(&spec).expect("canonical composes");
            let request = compose_request(&spec, None)
                .await
                .expect("request composes");
            assert_eq!(canonical.url, request.url.to_string(), "{spec:?}");

            let basic = matches!(spec.auth, RequestAuth::Basic { .. });
            for format in ScriptFormat::ALL {
                let script = render_script(&canonical, format);
                assert!(
                    script.contains(&canonical.url),
                    "{format:?} carries the composed URL for {spec:?}: {script}"
                );
                for (name, value) in &canonical.headers {
                    // cURL and wget move Basic credentials into their own
                    // options instead of sending the Authorization header.
                    if basic
                        && format != ScriptFormat::Http
                        && name.eq_ignore_ascii_case("Authorization")
                    {
                        continue;
                    }
                    assert!(
                        script.contains(&format!("{name}: {value}")),
                        "{format:?} carries header {name} for {spec:?}: {script}"
                    );
                }
                assert!(
                    !script.contains("Cookie:") && !script.contains("cookie:"),
                    "the jar never contributes a cookie header: {script}"
                );
                for generated in ["Host:", "Content-Length:", "Accept-Encoding:"] {
                    assert!(
                        !script.contains(generated),
                        "{format:?} does not add the transport's {generated}: {script}"
                    );
                }
            }
        }
    }

    fn canonical_body_bytes(body: &CanonicalBody) -> Option<Vec<u8>> {
        match body {
            CanonicalBody::None => None,
            CanonicalBody::Raw(text) => Some(text.as_bytes().to_vec()),
            CanonicalBody::Form(pairs) => Some(encode_form_pairs(pairs).into_bytes()),
            CanonicalBody::File { .. } | CanonicalBody::Multipart(_) => None,
        }
    }

    /// The in-memory body a composed request carries. Upload bodies are
    /// asserted through their capture server instead.
    fn composed_body_bytes(request: &ComposedRequest) -> Option<Vec<u8>> {
        match &request.body {
            ComposedBody::None => None,
            ComposedBody::Bytes(bytes) => Some(bytes.to_vec()),
            ComposedBody::Stream(_) | ComposedBody::Multipart(_) => {
                panic!("this draft has an upload body")
            }
        }
    }

    #[test]
    fn canonical_query_rows_keep_order_and_drop_finished_rows() {
        let spec = &equivalence_drafts()[0];
        let canonical = compose_canonical(spec).expect("canonical composes");
        assert_eq!(
            canonical.url, "https://example.com/items?keep=1&a=1&a=2#frag",
            "enabled rows append once in order before the fragment"
        );
    }

    #[test]
    fn lenient_url_matches_strict_for_a_valid_draft() {
        for url in [
            "https://example.com/items",
            "https://example.com/items?keep=1",
            "https://example.com/items?keep=1#frag",
            "https://user:pass@example.com:8443/a/b",
        ] {
            let mut spec = RequestSpec::new("valid");
            spec.url = url.into();
            spec.query = vec![
                KeyValue::new("term", "two words & more"),
                KeyValue::new("tag", "a/b"),
            ];

            let strict = compose_canonical(&spec).expect("strict composes");
            let (lenient, notice) = compose_canonical_lenient(&spec);
            assert_eq!(strict, lenient, "valid URLs compose identically for {url}");
            assert_eq!(notice, None, "a valid draft has no notice for {url}");
        }
    }

    #[test]
    fn lenient_composition_keeps_an_unresolved_placeholder_readable() {
        let mut spec = RequestSpec::new("templated");
        spec.url = "https://{{host}}/items#frag".into();
        spec.query = vec![
            KeyValue::new("term", "{{term}}"),
            KeyValue::new("tag", "two words"),
        ];
        spec.auth = RequestAuth::ApiKey {
            key: "api_key".into(),
            value: "{{secret}}".into(),
            location: ApiKeyLocation::Query,
        };

        let (canonical, notice) = compose_canonical_lenient(&spec);
        assert_eq!(
            canonical.url,
            "https://{{host}}/items?term={{term}}&tag=two+words&api_key={{secret}}#frag",
            "braces stay readable and the fragment stays last"
        );
        assert!(
            notice
                .expect("a notice")
                .contains("Unresolved placeholders"),
            "the preview says it is not a valid request yet"
        );
    }

    /// Upload tests need the body, not just the head: a file body is sent with
    /// a known length, while a multipart form is chunked because a streamed
    /// file's length is not known up front.
    mod uploads {
        use super::*;
        use std::fs;
        use std::io::{BufRead, BufReader};

        #[derive(Clone)]
        struct UploadRequest {
            head: String,
            body: Vec<u8>,
        }

        struct UploadServer {
            addr: SocketAddr,
            requests: Arc<Mutex<Vec<UploadRequest>>>,
        }

        impl UploadServer {
            fn spawn() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind upload server");
                let addr = listener.local_addr().expect("upload server address");
                let requests = Arc::new(Mutex::new(Vec::new()));
                let captured = requests.clone();
                thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else {
                            break;
                        };
                        if let Ok(request) = read_upload_request(&mut stream) {
                            captured.lock().expect("requests lock").push(request);
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        let _ = stream.flush();
                    }
                });
                Self { addr, requests }
            }

            fn url(&self) -> String {
                format!("http://{}/upload", self.addr)
            }

            fn take_requests(&self) -> Vec<UploadRequest> {
                self.requests.lock().expect("requests lock").clone()
            }
        }

        fn read_upload_request(stream: &mut std::net::TcpStream) -> std::io::Result<UploadRequest> {
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 {
                    break;
                }
                let blank = line == "\r\n";
                head.push_str(&line);
                if blank {
                    break;
                }
            }

            let mut length = None;
            let mut chunked = false;
            for line in head.lines().skip(1) {
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                let value = value.trim();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse::<usize>().ok();
                }
                if name.eq_ignore_ascii_case("transfer-encoding")
                    && value.to_ascii_lowercase().contains("chunked")
                {
                    chunked = true;
                }
            }

            let mut body = Vec::new();
            if chunked {
                loop {
                    let mut size_line = String::new();
                    if reader.read_line(&mut size_line)? == 0 {
                        break;
                    }
                    let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                    if size == 0 {
                        let mut trailer = String::new();
                        let _ = reader.read_line(&mut trailer);
                        break;
                    }
                    let mut chunk = vec![0_u8; size + 2];
                    reader.read_exact(&mut chunk)?;
                    chunk.truncate(size);
                    body.extend_from_slice(&chunk);
                }
            } else if let Some(length) = length {
                body.resize(length, 0);
                reader.read_exact(&mut body)?;
            }

            Ok(UploadRequest { head, body })
        }

        fn request_header(request: &str, name: &str) -> Option<String> {
            request
                .lines()
                .skip(1)
                .take_while(|line| !line.is_empty())
                .find_map(|line| {
                    let (header, value) = line.split_once(':')?;
                    header
                        .eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_owned())
                })
        }

        fn header_value(headers: &[GeneratedHeader], name: &str) -> Option<String> {
            headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .map(|header| header.value.clone())
        }

        fn workspace_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, WorkspaceStore) {
            let dir = tempfile::tempdir().expect("temp workspace");
            for (path, bytes) in files {
                let path = dir.path().join(path);
                fs::create_dir_all(path.parent().expect("a parent directory"))
                    .expect("create directories");
                fs::write(&path, bytes).expect("write file");
            }
            let workspace = WorkspaceStore::at(dir.path());
            (dir, workspace)
        }

        async fn send(
            executor: HttpExecutor,
            spec: &RequestSpec,
        ) -> Result<ResponseRecord, PosthasteError> {
            let (_cancel, cancel) = oneshot::channel();
            executor.execute(spec, cancel).await
        }

        fn upload_spec(server: &UploadServer) -> RequestSpec {
            let mut spec = RequestSpec::new("upload");
            spec.method = HttpMethod::Post;
            spec.url = server.url();
            spec.timeout_ms = Some(5_000);
            spec
        }

        fn upload_executor(workspace: WorkspaceStore) -> HttpExecutor {
            HttpExecutor::new(HttpConfig::default())
                .expect("executor")
                .with_workspace(workspace)
        }

        #[tokio::test]
        async fn a_file_body_is_streamed_with_its_type_and_length() {
            let png = [
                0x89_u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 1, 2, 3,
            ];
            let (_dir, workspace) = workspace_with(&[("files/logo.png", &png)]);
            let server = UploadServer::spawn();
            let mut spec = upload_spec(&server);
            spec.body = RequestBody::File {
                path: "files/logo.png".to_owned(),
            };

            send(upload_executor(workspace), &spec)
                .await
                .expect("the file is sent");

            let requests = server.take_requests();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.body, png, "the server receives the file's bytes");
            assert_eq!(
                request_header(&request.head, "content-type").as_deref(),
                Some("image/png")
            );
            let length = png.len().to_string();
            assert_eq!(
                request_header(&request.head, "content-length").as_deref(),
                Some(length.as_str())
            );
        }

        #[tokio::test]
        async fn a_user_content_type_replaces_the_inferred_file_type() {
            let bytes = b"posthaste bytes";
            let (_dir, workspace) = workspace_with(&[("files/data.bin", bytes)]);
            let server = UploadServer::spawn();
            let mut spec = upload_spec(&server);
            spec.body = RequestBody::File {
                path: "files/data.bin".to_owned(),
            };
            spec.headers = vec![KeyValue::new("Content-Type", "application/x-custom")];

            send(upload_executor(workspace), &spec)
                .await
                .expect("the file is sent");

            let requests = server.take_requests();
            let request = &requests[0];
            assert_eq!(
                request_header(&request.head, "content-type").as_deref(),
                Some("application/x-custom")
            );
            assert_eq!(request.body, bytes, "the file bytes are unchanged");
        }

        #[tokio::test]
        async fn a_multipart_body_sends_text_and_file_parts() {
            let bytes = b"posthaste bytes";
            let (_dir, workspace) = workspace_with(&[("files/logo.png", bytes)]);
            let server = UploadServer::spawn();
            let mut spec = upload_spec(&server);
            spec.body = RequestBody::Multipart(vec![
                MultipartPart::text("title", "Posthaste"),
                MultipartPart::file("upload", "files/logo.png"),
            ]);
            // A user row of this name must not replace the generated boundary.
            spec.headers = vec![KeyValue::new("Content-Type", "multipart/form-data")];

            send(upload_executor(workspace), &spec)
                .await
                .expect("the form is sent");

            let requests = server.take_requests();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            let content_type =
                request_header(&request.head, "content-type").expect("a content type");
            assert!(
                content_type.starts_with("multipart/form-data; boundary="),
                "the boundary is generated: {content_type}"
            );
            assert_eq!(
                request
                    .head
                    .lines()
                    .filter(|line| line.to_ascii_lowercase().starts_with("content-type:"))
                    .count(),
                1,
                "the user row is not sent beside the generated header"
            );

            let body = String::from_utf8_lossy(&request.body);
            assert!(body.contains("name=\"title\""), "{body}");
            assert!(body.contains("\r\n\r\nPosthaste\r\n"), "{body}");
            assert!(
                body.contains("name=\"upload\"; filename=\"logo.png\""),
                "{body}"
            );
            assert!(
                body.contains("Content-Type: image/png"),
                "the part type comes from the filename: {body}"
            );
            assert!(
                request
                    .body
                    .windows(bytes.len())
                    .any(|window| window == bytes),
                "the file bytes are part of the body"
            );
        }

        #[tokio::test]
        async fn disabled_and_unnamed_multipart_rows_are_not_sent() {
            let (_dir, workspace) = workspace_with(&[("files/logo.png", b"png")]);
            let server = UploadServer::spawn();
            let mut spec = upload_spec(&server);
            spec.body = RequestBody::Multipart(vec![
                MultipartPart::Text {
                    key: "hidden".to_owned(),
                    value: "no".to_owned(),
                    enabled: false,
                },
                // An empty key skips the row, so a missing path is never read.
                MultipartPart::file("", "files/missing.bin"),
                MultipartPart::text("kept", "yes"),
            ]);

            send(upload_executor(workspace), &spec)
                .await
                .expect("the remaining rows are sent");

            let requests = server.take_requests();
            let body = String::from_utf8_lossy(&requests[0].body).into_owned();
            assert!(!body.contains("hidden"), "{body}");
            assert!(!body.contains("missing.bin"), "{body}");
            assert!(body.contains("name=\"kept\""), "{body}");
        }

        #[test]
        fn preflight_rejects_empty_and_escaping_paths() {
            let (_dir, workspace) = workspace_with(&[("files/logo.png", b"png")]);
            let mut spec = RequestSpec::new("upload");

            spec.body = RequestBody::File {
                path: "   ".to_owned(),
            };
            let error = validate_upload_files(&spec, &workspace).expect_err("no file chosen");
            assert!(error.to_string().contains("no file chosen"), "{error}");

            spec.body = RequestBody::File {
                path: "../outside.txt".to_owned(),
            };
            let error = validate_upload_files(&spec, &workspace).expect_err("escaping path");
            assert!(
                error
                    .to_string()
                    .contains("not a path inside the workspace"),
                "{error}"
            );
        }

        #[test]
        fn preflight_names_a_missing_file_and_a_directory() {
            let (dir, workspace) = workspace_with(&[("files/logo.png", b"png")]);
            let mut spec = RequestSpec::new("upload");

            spec.body = RequestBody::File {
                path: "files/missing.bin".to_owned(),
            };
            let error = validate_upload_files(&spec, &workspace).expect_err("missing file");
            assert!(error.to_string().contains("files/missing.bin"), "{error}");

            fs::create_dir_all(dir.path().join("folder")).expect("create folder");
            spec.body = RequestBody::File {
                path: "folder".to_owned(),
            };
            let error = validate_upload_files(&spec, &workspace).expect_err("a directory");
            assert!(error.to_string().contains("not a regular file"), "{error}");
        }

        #[test]
        fn preflight_names_the_row_for_a_missing_multipart_part() {
            let (_dir, workspace) = workspace_with(&[("files/logo.png", b"png")]);
            let mut spec = RequestSpec::new("upload");
            // The disabled row is never checked.
            spec.body = RequestBody::Multipart(vec![
                MultipartPart::file("upload", "files/missing.bin"),
                MultipartPart::File {
                    key: "hidden".to_owned(),
                    path: "files/missing-too.bin".to_owned(),
                    enabled: false,
                },
            ]);

            let error = validate_upload_files(&spec, &workspace).expect_err("missing part");
            let message = error.to_string();
            assert!(message.contains("upload"), "{message}");
            assert!(message.contains("files/missing.bin"), "{message}");
            assert!(!message.contains("missing-too.bin"), "{message}");
        }

        #[cfg(windows)]
        #[test]
        fn preflight_rejects_a_file_that_cannot_be_opened() {
            use std::os::windows::fs::OpenOptionsExt;

            let (dir, workspace) = workspace_with(&[("files/locked.bin", b"png")]);
            let path = dir.path().join("files/locked.bin");
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .expect("lock the file");

            let mut spec = RequestSpec::new("upload");
            spec.body = RequestBody::File {
                path: "files/locked.bin".to_owned(),
            };
            let error = validate_upload_files(&spec, &workspace).expect_err("the file is locked");
            assert!(error.to_string().contains("files/locked.bin"), "{error}");

            drop(lock);
        }

        #[test]
        fn validation_ignores_an_empty_or_half_filled_upload() {
            let mut spec = RequestSpec::new("upload");
            spec.url = "https://example.com/upload".to_owned();

            spec.body = RequestBody::File {
                path: String::new(),
            };
            validate_request(&spec).expect("an empty file is not a structure error");

            spec.body = RequestBody::Multipart(vec![MultipartPart::file("upload", "")]);
            validate_request(&spec).expect("a half-filled row is not a structure error");
        }

        #[test]
        fn generated_headers_describe_a_file_body() {
            let (dir, workspace) = workspace_with(&[("files/report.pdf", b"%PDF")]);
            let mut spec = RequestSpec::new("upload");
            spec.url = "https://example.com/upload".to_owned();
            spec.body = RequestBody::File {
                path: "files/report.pdf".to_owned(),
            };

            let headers = generated_headers(&spec, Some(workspace.root()));
            assert_eq!(
                header_value(&headers, "content-type").as_deref(),
                Some("application/pdf")
            );
            assert_eq!(
                header_value(&headers, "content-length").as_deref(),
                Some("4")
            );

            fs::create_dir_all(dir.path().join("files")).expect("files directory");
            fs::write(dir.path().join("files/notes.posthaste"), b"abc").expect("unknown type");
            spec.body = RequestBody::File {
                path: "files/notes.posthaste".to_owned(),
            };
            let headers = generated_headers(&spec, Some(workspace.root()));
            assert_eq!(
                header_value(&headers, "content-type").as_deref(),
                Some("application/octet-stream"),
                "an unknown extension falls back to octet-stream"
            );
            assert_eq!(
                header_value(&headers, "content-length").as_deref(),
                Some("3")
            );
        }

        #[test]
        fn generated_headers_keep_the_multipart_type_automatic() {
            let mut spec = RequestSpec::new("upload");
            spec.url = "https://example.com/upload".to_owned();
            spec.body = RequestBody::Multipart(vec![MultipartPart::text("a", "b")]);

            let headers = generated_headers(&spec, None);
            assert_eq!(
                header_value(&headers, "content-type").as_deref(),
                Some("multipart/form-data; boundary=<generated at send>")
            );
            assert_eq!(
                header_value(&headers, "content-length"),
                None,
                "the multipart length is unknown until the form is built"
            );

            spec.headers = vec![KeyValue::new("Content-Type", "application/x-custom")];
            let headers = generated_headers(&spec, None);
            assert_eq!(
                header_value(&headers, "content-type").as_deref(),
                Some("multipart/form-data; boundary=<generated at send>"),
                "an enabled user row does not replace the generated type"
            );
        }

        #[tokio::test]
        async fn a_failure_after_the_file_opens_is_a_transport_failure() {
            let (_dir, workspace) = workspace_with(&[("files/logo.png", b"png bytes")]);
            // The address is known but nothing listens on it anymore.
            let addr = {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind unused address");
                listener.local_addr().expect("unused address")
            };
            let mut spec = RequestSpec::new("upload");
            spec.method = HttpMethod::Post;
            spec.url = format!("http://{addr}/upload");
            spec.timeout_ms = Some(2_000);
            spec.body = RequestBody::File {
                path: "files/logo.png".to_owned(),
            };

            validate_upload_files(&spec, &workspace).expect("the file is readable");
            let error = send(upload_executor(workspace), &spec)
                .await
                .expect_err("the server is gone");
            // A closed port refuses the connection on most platforms but can
            // sit until the timeout expires on Windows.
            assert!(
                matches!(error, PosthasteError::Http(_) | PosthasteError::Timeout),
                "the run fails as a transport failure: {error:?}"
            );
        }
    }

    #[test]
    fn lenient_composition_tolerates_an_unparseable_url_and_invalid_header() {
        let mut spec = RequestSpec::new("unfinished");
        spec.url = "not a url".into();
        spec.query = vec![KeyValue::new("a", "1")];
        spec.headers = vec![KeyValue::new("bad header", "value")];

        let (canonical, notice) = compose_canonical_lenient(&spec);
        assert_eq!(canonical.url, "not a url?a=1");
        assert!(
            notice.expect("a notice").contains("URL is not valid"),
            "an unparseable URL is reported, not fatal"
        );

        spec.url = "https://example.com".into();
        let (canonical, notice) = compose_canonical_lenient(&spec);
        assert_eq!(
            canonical.headers,
            vec![("bad header".into(), "value".into())]
        );
        assert!(
            notice.expect("a notice").contains("bad header"),
            "an invalid header is reported and still shown"
        );
    }
}
