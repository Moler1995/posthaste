//! Test-only helpers shared by the app's test modules.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tokio_tungstenite::tungstenite::accept;
use tokio_tungstenite::tungstenite::protocol::Message;

/// A tiny HTTP server that records every complete request and answers `200`.
pub(crate) struct LocalServer {
    pub(crate) addr: SocketAddr,
    captured: Arc<Mutex<Vec<String>>>,
}

impl LocalServer {
    pub(crate) fn spawn(delay: Duration) -> Self {
        Self::spawn_with(delay, "text/plain", b"local-body".to_vec())
    }

    /// Serves `body` with the given content type. The response is captured as
    /// `body` bytes so callers can compare clipboard contents exactly.
    pub(crate) fn spawn_with(delay: Duration, content_type: &'static str, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().expect("local test server address");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server_captured = captured.clone();
        let body = Arc::new(body);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
                let request = read_http_request(&mut stream);
                server_captured
                    .lock()
                    .expect("capture lock")
                    .push(String::from_utf8_lossy(&request).into_owned());

                thread::sleep(delay);

                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });

        Self { addr, captured }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}/test", self.addr)
    }

    pub(crate) fn last_request(&self) -> String {
        self.captured
            .lock()
            .expect("capture lock")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.captured.lock().expect("capture lock").len()
    }
}

/// A chunked-response server: the status and headers go out after
/// `head_delay`, then each body chunk after `chunk_delay`. A test can observe
/// the response mid-stream and know the chunks arrive in this order.
pub(crate) struct ChunkedServer {
    pub(crate) addr: SocketAddr,
}

impl ChunkedServer {
    pub(crate) fn spawn(head_delay: Duration, chunk_delay: Duration, chunks: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind chunked test server");
        let addr = listener.local_addr().expect("chunked server address");

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
                let _ = read_http_request(&mut stream);

                thread::sleep(head_delay);
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                            Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                if stream.write_all(head.as_bytes()).is_err() {
                    continue;
                }
                let _ = stream.flush();

                for chunk in &chunks {
                    thread::sleep(chunk_delay);
                    let frame = format!("{:x}\r\n", chunk.len());
                    let sent = stream
                        .write_all(frame.as_bytes())
                        .and_then(|_| stream.write_all(chunk))
                        .and_then(|_| stream.write_all(b"\r\n"))
                        .and_then(|_| stream.flush());
                    if sent.is_err() {
                        break;
                    }
                }
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.flush();
            }
        });

        Self { addr }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}/stream", self.addr)
    }
}

/// A WebSocket server that greets each connection with one text frame and
/// records what the client sends. Every connection is served on its own
/// thread, so two tabs can hold sessions at the same time.
pub(crate) struct WsServer {
    pub(crate) addr: SocketAddr,
    received: Arc<Mutex<Vec<(usize, String)>>>,
    connections: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
}

impl WsServer {
    pub(crate) fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ws test server");
        let addr = listener.local_addr().expect("ws test server address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));

        let server_received = received.clone();
        let server_connections = connections.clone();
        let server_closes = closes.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    break;
                };
                let number = server_connections.fetch_add(1, Ordering::SeqCst) + 1;
                let received = server_received.clone();
                let closes = server_closes.clone();
                thread::spawn(move || {
                    let mut socket = accept(stream).expect("ws handshake");
                    socket
                        .send(Message::text(format!("hello on connection {number}")))
                        .expect("send greeting");
                    loop {
                        match socket.read() {
                            Ok(Message::Text(text)) => {
                                received
                                    .lock()
                                    .expect("ws capture lock")
                                    .push((number, text.as_str().to_owned()));
                                let _ =
                                    socket.send(Message::text(format!("echo:{}", text.as_str())));
                            }
                            Ok(Message::Binary(bytes)) => received
                                .lock()
                                .expect("ws capture lock")
                                .push((number, format!("binary:{}", bytes.len()))),
                            Ok(Message::Close(_)) | Err(_) => {
                                closes.fetch_add(1, Ordering::SeqCst);
                                break;
                            }
                            Ok(_) => {}
                        }
                    }
                });
            }
        });

        Self {
            addr,
            received,
            connections,
            closes,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("ws://{}/socket", self.addr)
    }

    pub(crate) fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub(crate) fn close_count(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }

    pub(crate) fn received(&self) -> Vec<(usize, String)> {
        self.received.lock().expect("ws capture lock").clone()
    }
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let content_length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= head_end + 4 + content_length {
            break;
        }
    }
    buffer
}
