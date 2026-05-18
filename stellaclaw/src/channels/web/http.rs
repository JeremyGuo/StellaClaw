use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpStream,
};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_HTTP_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct HttpRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: HashMap<String, String>,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Vec<u8>,
}

impl HttpRequest {
    pub(super) fn is_websocket(&self) -> bool {
        self.headers
            .get("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }
}

pub(super) struct HttpResponse {
    pub(super) status: u16,
    pub(super) content_type: String,
    pub(super) body: Vec<u8>,
}

impl HttpResponse {
    pub(super) fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8".to_string(),
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    pub(super) fn bytes(status: u16, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
        }
    }

    pub(super) fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: Vec::new(),
        }
    }
}

pub(super) type HttpResult<T = HttpResponse> = Result<T, HttpError>;

#[derive(Debug)]
pub(super) struct HttpError {
    status: u16,
    pub(super) message: String,
}

impl HttpError {
    pub(super) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(super) fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(500, error.to_string())
    }

    pub(super) fn upgrade_required() -> Self {
        Self::new(426, "upgrade required")
    }

    pub(super) fn into_response(self) -> HttpResponse {
        HttpResponse::json(self.status, json!({"error": self.message}))
    }
}

pub(super) fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("connection closed while reading headers");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 64 * 1024 {
            anyhow::bail!("http headers too large");
        }
    }
    let header_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap_or(buffer.len());
    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let target = request_parts.next().unwrap_or_default();
    let (path, query) = parse_target(target);
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_HTTP_BODY_BYTES {
        anyhow::bail!("http body too large");
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

pub(super) fn write_response(stream: &mut TcpStream, response: &HttpResponse) -> Result<()> {
    let status_text = reason_phrase(response.status);
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS\r\nConnection: close\r\n\r\n",
        response.status,
        status_text,
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

pub(super) fn split_path(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    (path.to_string(), parse_query(query))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if let Some(decoded_key) = percent_decode(key) {
            result.insert(decoded_key, percent_decode(value).unwrap_or_default());
        }
    }
    result
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'%' => {
                let high = chars.next()?;
                let low = chars.next()?;
                let hex = [high, low];
                let text = std::str::from_utf8(&hex).ok()?;
                bytes.push(u8::from_str_radix(text, 16).ok()?);
            }
            b'+' => bytes.push(b' '),
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).ok()
}

pub(super) fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> HttpResult<T> {
    serde_json::from_slice(body).map_err(|error| HttpError::new(400, error.to_string()))
}

pub(super) fn parse_optional_json<T: for<'de> Deserialize<'de> + Default>(
    body: &[u8],
) -> HttpResult<T> {
    if body.is_empty() {
        return Ok(T::default());
    }
    parse_json(body)
}

pub(super) fn query_usize(query: &HashMap<String, String>, key: &str, default: usize) -> usize {
    query
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(super) fn query_u64(query: &HashMap<String, String>, key: &str) -> Option<u64> {
    query.get(key).and_then(|value| value.parse().ok())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        426 => "Upgrade Required",
        500 => "Internal Server Error",
        _ => "OK",
    }
}
