//! A small HTTP/1.1 server on loopback for the session's API and viewer.
//!
//! One thread per connection, one request per connection: the clients are
//! `dashr` commands and one browser tab polling once a second.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Lowercase names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: value.to_string().into_bytes(),
        }
    }

    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, &serde_json::json!({"error": message}))
    }

    pub fn text(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        _ => "Error",
    }
}

pub fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_target(target: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode_component(k), decode_component(v))
        })
        .collect();
    (path.to_owned(), query)
}

fn read_request(stream: &TcpStream) -> Result<Request, (u16, String)> {
    let bad = |m: &str| (400, m.to_owned());
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| bad(&e.to_string()))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or_else(|| bad("empty request"))?.to_owned();
    let target = parts.next().ok_or_else(|| bad("no target"))?;
    let (path, query) = parse_target(target);
    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        if reader
            .read_line(&mut header)
            .map_err(|e| bad(&e.to_string()))?
            == 0
        {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let mut body = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let mut size = String::new();
            reader
                .read_line(&mut size)
                .map_err(|e| bad(&e.to_string()))?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| bad("bad chunk"))?;
            if size == 0 {
                let mut trailer = String::new();
                let _ = reader.read_line(&mut trailer);
                break;
            }
            if body.len() + size > MAX_BODY {
                return Err((413, "body too large".into()));
            }
            let mut chunk = vec![0u8; size + 2];
            reader
                .read_exact(&mut chunk)
                .map_err(|e| bad(&e.to_string()))?;
            chunk.truncate(size);
            body.extend_from_slice(&chunk);
        }
    } else if let Some(length) = headers.get("content-length") {
        let length: usize = length.parse().map_err(|_| bad("bad content-length"))?;
        if length > MAX_BODY {
            return Err((413, "body too large".into()));
        }
        body = vec![0u8; length];
        reader
            .read_exact(&mut body)
            .map_err(|e| bad(&e.to_string()))?;
    }
    if headers
        .get("content-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
    {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .take(MAX_BODY as u64)
            .read_to_end(&mut decoded)
            .map_err(|e| bad(&format!("bad gzip body: {e}")))?;
        body = decoded;
    }
    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn write_response(mut stream: &TcpStream, response: &Response) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Serves `listener` until `stop` is set.
pub fn serve(listener: TcpListener, handler: Handler, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let response = match read_request(&stream) {
                        Ok(request) => handler(request),
                        Err((status, message)) => Response::error(status, &message),
                    };
                    write_response(&stream, &response);
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(raw: &[u8]) -> (Request, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Handler = {
            let seen = Arc::clone(&seen);
            Arc::new(move |request: Request| {
                *seen.lock().unwrap() = Some(request);
                Response::json(200, &serde_json::json!({"ok": true}))
            })
        };
        let server = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve(listener, handler, stop))
        };
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(raw).unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let request = seen.lock().unwrap().take().unwrap();
        (request, answer)
    }

    #[test]
    fn requests_with_query_length_and_chunks() {
        let (request, answer) = roundtrip(b"POST /api/x?a=1&b=two%20words HTTP/1.1\r\nContent-Length: 5\r\nX-Token: t\r\n\r\nhello");
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", "/api/x")
        );
        assert_eq!(request.query["b"], "two words");
        assert_eq!(request.headers["x-token"], "t");
        assert_eq!(request.body, b"hello");
        assert!(answer.starts_with("HTTP/1.1 200 OK") && answer.ends_with("{\"ok\":true}"));
        let (request, _) = roundtrip(b"POST /c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n");
        assert_eq!(request.body, b"abcde");
    }

    #[test]
    fn gzip_bodies_are_decoded() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"{\"a\":1}").unwrap();
        let body = encoder.finish().unwrap();
        let mut raw = format!(
            "POST /g HTTP/1.1\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(&body);
        let (request, _) = roundtrip(&raw);
        assert_eq!(request.body, b"{\"a\":1}");
    }

    #[test]
    fn components_decode() {
        assert_eq!(decode_component("a%3Db+c"), "a=b c");
        assert_eq!(decode_component("100%"), "100%");
    }
}
