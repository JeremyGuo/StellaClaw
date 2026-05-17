use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use super::protocol::HEARTBEAT_INTERVAL_SECS;
use super::{http::HttpRequest, time_utils::now_rfc3339};

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const WEBSOCKET_MAX_FRAME_BYTES: usize = 128 * 1024;

pub(super) fn accept_websocket(stream: &mut TcpStream, request: &HttpRequest) -> Result<()> {
    let key = request
        .headers
        .get("sec-websocket-key")
        .ok_or_else(|| anyhow!("missing websocket key"))?;
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    let accept = general_purpose::STANDARD.encode(hasher.finalize());
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(())
}

pub(super) fn websocket_event_loop(
    mut stream: TcpStream,
    rx: Receiver<Value>,
    heartbeat_type: &'static str,
) -> Result<()> {
    loop {
        match rx.recv_timeout(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)) {
            Ok(value) => send_websocket_json(&mut stream, &value)?,
            Err(RecvTimeoutError::Timeout) => {
                send_websocket_json(
                    &mut stream,
                    &json!({"type": heartbeat_type, "server_time": now_rfc3339()}),
                )?;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

pub(super) fn send_websocket_json(stream: &mut TcpStream, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    send_websocket_frame(stream, 0x1, &payload)
}

fn send_websocket_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<()> {
    if payload.len() > WEBSOCKET_MAX_FRAME_BYTES {
        anyhow::bail!("websocket payload too large");
    }
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | opcode);
    if payload.len() < 126 {
        header.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

pub(super) enum ClientWebSocketFrame {
    Text(String),
    Close,
    Ping,
    Pong,
    Binary(Vec<u8>),
}

pub(super) fn read_client_websocket_frame(stream: &mut TcpStream) -> Result<ClientWebSocketFrame> {
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header)?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7f) as u64;
    if length == 126 {
        let mut bytes = [0_u8; 2];
        stream.read_exact(&mut bytes)?;
        length = u16::from_be_bytes(bytes) as u64;
    } else if length == 127 {
        let mut bytes = [0_u8; 8];
        stream.read_exact(&mut bytes)?;
        length = u64::from_be_bytes(bytes);
    }
    if length as usize > WEBSOCKET_MAX_FRAME_BYTES {
        return Err(anyhow!("websocket frame too large"));
    }
    let mask = if masked {
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask)?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0_u8; length as usize];
    if length > 0 {
        stream.read_exact(&mut payload)?;
    }
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    match opcode {
        0x1 => Ok(ClientWebSocketFrame::Text(String::from_utf8(payload)?)),
        0x2 => Ok(ClientWebSocketFrame::Binary(payload)),
        0x8 => Ok(ClientWebSocketFrame::Close),
        0x9 => Ok(ClientWebSocketFrame::Ping),
        0xA => Ok(ClientWebSocketFrame::Pong),
        other => Err(anyhow!("unsupported websocket opcode {other}")),
    }
}
