//! Network integration tests against a local TCP server.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use posthaste_core::error::PosthasteError;
use posthaste_core::http::{DEFAULT_BODY_LIMIT, HttpConfig, HttpExecutor};
use posthaste_core::model::{HttpMethod, KeyValue, RequestBody, RequestSpec};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
struct CapturedRequest {
    head: String,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|line| {
            let (candidate, value) = line.split_once(':')?;
            candidate.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

#[derive(Debug, Clone)]
struct TestResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl TestResponse {
    fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl TestServer {
    fn spawn(handler: impl Fn(&CapturedRequest) -> TestResponse + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let Ok(request) = read_request(&mut stream) else {
                    continue;
                };
                let response = handler(&request);
                captured.lock().unwrap().push(request);

                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let _ = write_response(&mut stream, &response);
            }
        });

        Self { addr, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<CapturedRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    let mut buffer = Vec::new();
    let mut temp = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut temp)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed before sending headers",
            ));
        }
        buffer.extend_from_slice(&temp[..read]);

        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut temp)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&temp[..read]);
    }

    let end = (header_end + content_length).min(buffer.len());
    Ok(CapturedRequest {
        head,
        body: buffer[header_end..end].to_vec(),
    })
}

fn write_response(stream: &mut TcpStream, response: &TestResponse) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} Test\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));

    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn executor(config: HttpConfig) -> HttpExecutor {
    HttpExecutor::new(config).expect("executor")
}

fn spec_for(server: &TestServer, path: &str) -> RequestSpec {
    let mut spec = RequestSpec::new("test");
    spec.url = server.url(path);
    spec
}

#[tokio::test]
async fn sends_request_and_preserves_a_404_response() {
    let server = TestServer::spawn(|_| {
        TestResponse::new(404, "missing")
            .with_header("Content-Type", "text/plain")
            .with_header("X-Test", "yes")
    });

    let mut spec = spec_for(&server, "/items?keep=1");
    spec.method = HttpMethod::Post;
    spec.query = vec![
        KeyValue::new("page", "2"),
        KeyValue {
            key: "skip".into(),
            value: "1".into(),
            enabled: false,
        },
    ];
    spec.headers = vec![
        KeyValue::new("Accept", "application/json"),
        KeyValue {
            key: "X-Skip".into(),
            value: "yes".into(),
            enabled: false,
        },
    ];
    spec.body = RequestBody::Json(r#"{"ok":true}"#.into());

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let response = executor(HttpConfig::default())
        .execute(&spec, cancel_rx)
        .await
        .expect("request succeeds");
    drop(cancel_tx);

    assert_eq!(response.status, 404);
    assert_eq!(response.body.as_ref(), b"missing");
    assert!(!response.truncated);
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name == "x-test" && value == "yes")
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.request_line(), "POST /items?keep=1&page=2 HTTP/1.1");
    assert_eq!(request.header("accept"), Some("application/json"));
    assert_eq!(request.header("x-skip"), None);
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.body, br#"{"ok":true}"#);
}

#[tokio::test]
async fn preserves_an_empty_body() {
    let server = TestServer::spawn(|_| TestResponse::new(204, Vec::new()));

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let response = executor(HttpConfig::default())
        .execute(&spec_for(&server, "/empty"), cancel_rx)
        .await
        .expect("request succeeds");
    drop(cancel_tx);

    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert!(!response.truncated);
}

#[tokio::test]
async fn truncates_a_body_at_the_configured_limit() {
    let server = TestServer::spawn(|_| TestResponse::new(200, "0123456789abcdef"));

    let config = HttpConfig {
        body_limit: 10,
        ..HttpConfig::default()
    };

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let response = executor(config)
        .execute(&spec_for(&server, "/large"), cancel_rx)
        .await
        .expect("request succeeds");
    drop(cancel_tx);

    assert_eq!(response.body.as_ref(), b"0123456789");
    assert!(response.truncated);
    assert_eq!(DEFAULT_BODY_LIMIT, 20 * 1024 * 1024);
}

#[tokio::test]
async fn rejects_an_invalid_url_without_a_request() {
    let mut spec = RequestSpec::new("bad");
    spec.url = "not a url".into();

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let error = executor(HttpConfig::default())
        .execute(&spec, cancel_rx)
        .await
        .expect_err("invalid URL is rejected");
    drop(cancel_tx);

    assert!(matches!(error, PosthasteError::InvalidUrl(_)));
}

#[tokio::test]
async fn reports_a_refused_connection_as_a_transport_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind temporary listener");
    let addr = listener.local_addr().expect("temporary address");
    drop(listener);

    let mut spec = RequestSpec::new("refused");
    spec.url = format!("http://{addr}/");
    spec.timeout_ms = Some(5_000);

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let error = executor(HttpConfig::default())
        .execute(&spec, cancel_rx)
        .await
        .expect_err("connection is refused");
    drop(cancel_tx);

    assert!(matches!(error, PosthasteError::Http(_)), "got {error:?}");
}

#[tokio::test]
async fn times_out_a_slow_response() {
    let server =
        TestServer::spawn(|_| TestResponse::new(200, "late").delayed(Duration::from_secs(2)));

    let mut spec = spec_for(&server, "/slow");
    spec.timeout_ms = Some(100);

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let started = Instant::now();
    let error = executor(HttpConfig::default())
        .execute(&spec, cancel_rx)
        .await
        .expect_err("request times out");
    drop(cancel_tx);

    assert!(matches!(error, PosthasteError::Timeout), "got {error:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "timeout took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn cancellation_stops_an_in_flight_request() {
    let server =
        TestServer::spawn(|_| TestResponse::new(200, "late").delayed(Duration::from_secs(5)));

    let spec = spec_for(&server, "/slow");
    let executor = executor(HttpConfig::default());
    let (cancel_tx, cancel_rx) = oneshot::channel();

    let task = tokio::spawn(async move { executor.execute(&spec, cancel_rx).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    cancel_tx.send(()).expect("cancel receiver is alive");
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("cancel completes promptly")
        .expect("task does not panic");

    assert!(
        matches!(result, Err(PosthasteError::Cancelled)),
        "got {result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn stops_after_the_redirect_limit() {
    let server =
        TestServer::spawn(|_| TestResponse::new(302, Vec::new()).with_header("Location", "/loop"));

    let config = HttpConfig {
        redirect_limit: 2,
        ..HttpConfig::default()
    };
    let mut spec = spec_for(&server, "/start");
    spec.timeout_ms = Some(3_000);

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let error = executor(config)
        .execute(&spec, cancel_rx)
        .await
        .expect_err("redirect limit is enforced");
    drop(cancel_tx);

    assert!(
        matches!(error, PosthasteError::TooManyRedirects),
        "got {error:?}"
    );
    assert!(!server.requests().is_empty());
}
