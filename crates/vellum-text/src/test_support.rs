//! Deterministic, offline test doubles.
//!
//! The HTTP tests must not depend on a provider being reachable, so every
//! request shape and error mapping is exercised against a loopback listener
//! whose answers are fixed in advance.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vellum_core::config::{DEFAULT_API_KEY_ENV, LEGACY_API_KEY_ENV};

/// One scripted answer.
pub(crate) enum Script {
    /// Status line and body of a normal response.
    Reply { status: u16, body: String },
    /// Accept the connection and close it without answering: the
    /// transport-failure path.
    Hangup,
    /// Answer only after a delay, to exercise the client timeout.
    Slow {
        delay: Duration,
        status: u16,
        body: String,
    },
}

impl Script {
    pub(crate) fn reply(status: u16, body: impl Into<String>) -> Self {
        Self::Reply {
            status,
            body: body.into(),
        }
    }

    pub(crate) fn hangup() -> Self {
        Self::Hangup
    }

    pub(crate) fn slow(delay: Duration, status: u16, body: impl Into<String>) -> Self {
        Self::Slow {
            delay,
            status,
            body: body.into(),
        }
    }
}

/// A listener that answers with a fixed script, in order.
///
/// The last scripted answer repeats, so a test that accidentally sends one
/// request too many gets an answer instead of hanging.
pub(crate) struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    pub(crate) fn start(script: Vec<Script>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback test port");
        let port = listener.local_addr().expect("bound address").port();
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || serve(&listener, &script, &requests, &stop))
        };
        Self {
            // /v1 is what an OpenAI-compatible service puts in front of
            // /chat/completions and /models.
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
            stop,
            handle: Some(handle),
        }
    }

    pub(crate) fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Raw requests in arrival order, headers and body included.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        // Non-blocking accept plus this flag is the whole shutdown protocol: no
        // self-connect trick, no leaked thread.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    listener: &TcpListener,
    script: &[Script],
    requests: &Mutex<Vec<String>>,
    stop: &AtomicBool,
) {
    let mut served = 0usize;
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut request = read_request(&mut stream);
                // ureq tunnels through an HTTP proxy even for plain-http
                // targets: acknowledge the CONNECT, then serve the real request
                // that follows inside the tunnel.
                if request.starts_with("CONNECT ") {
                    let _ = stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
                    let _ = stream.flush();
                    request = read_request(&mut stream);
                }
                requests
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(request);
                let answer = script.get(served).or_else(|| script.last());
                served += 1;
                match answer {
                    Some(Script::Reply { status, body }) => write_reply(&mut stream, *status, body),
                    Some(Script::Slow {
                        delay,
                        status,
                        body,
                    }) => {
                        thread::sleep(*delay);
                        write_reply(&mut stream, *status, body);
                    }
                    // Dropping the stream is the entire answer here.
                    Some(Script::Hangup) | None => {}
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(clone) = stream.try_clone() else {
        return String::new();
    };
    let mut reader = BufReader::new(clone);
    let mut raw = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let end_of_head = line == "\r\n" || line == "\n";
                raw.push_str(&line);
                if end_of_head {
                    break;
                }
            }
        }
    }
    let length = raw
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse::<usize>().ok()
        })
        .unwrap_or(0);
    if length > 0 {
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_ok() {
            raw.push_str(&String::from_utf8_lossy(&body));
        }
    }
    raw
}

fn write_reply(stream: &mut TcpStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        reason(status),
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Body of a recorded request, for shape assertions.
pub(crate) fn body_of(request: &str) -> &str {
    request.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// Parse a recorded request body as JSON.
pub(crate) fn json_body(request: &str) -> serde_json::Value {
    serde_json::from_str(body_of(request))
        .unwrap_or_else(|err| panic!("request body is not JSON: {err}\n{request}"))
}

/// Run the body with the well-known key variables removed, restoring whatever
/// was there afterwards.
///
/// A developer machine with OPENAI_API_KEY exported would otherwise decide by
/// accident whether the "no key configured" tests pass. Tests that never
/// consult the environment do not take this lock.
pub(crate) fn without_ambient_keys<T>(body: impl FnOnce() -> T) -> T {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let names = [DEFAULT_API_KEY_ENV, LEGACY_API_KEY_ENV];
    let saved: Vec<(&str, Option<String>)> = names
        .iter()
        .map(|name| (*name, std::env::var(name).ok()))
        .collect();
    for name in names {
        // SAFETY: the lock above serialises the tests that touch the
        // environment, and no other test reads these variables.
        unsafe { std::env::remove_var(name) };
    }
    let result = body();
    for (name, value) in saved {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
    result
}
