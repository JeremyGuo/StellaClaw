use std::{net::TcpStream, thread, time::Duration};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::RecvTimeoutError;
use serde_json::{json, Value};

use crate::{
    conversation_host::ConversationHostRuntime,
    service_protos::{
        channel::{ChannelEvent as KernelChannelEvent, ChannelIngress},
        terminal::{TerminalDataEncoding, TerminalRequest, TerminalResponse},
    },
    services::terminal_runtime::{TerminalCreateRequest, TerminalResizeRequest},
};

use super::{
    channel::{wait_for_event, WebChannel},
    http::{parse_optional_json, query_u64, HttpError, HttpRequest, HttpResponse, HttpResult},
    protocol::HEARTBEAT_INTERVAL_SECS,
    time_utils::{generated_request_id, now_rfc3339},
    websocket::{
        accept_websocket, read_client_websocket_frame, send_websocket_json, ClientWebSocketFrame,
    },
};

impl WebChannel {
    pub(super) fn list_terminals(&self, conversation_id: &str) -> HttpResult {
        self.terminal_response(conversation_id, TerminalRequest::List)
            .and_then(|response| match response {
                TerminalResponse::Terminals { terminals } => {
                    Ok(HttpResponse::json(200, json!({ "terminals": terminals })))
                }
                TerminalResponse::Error { message, .. } => Err(HttpError::new(400, message)),
                other => Ok(HttpResponse::json(
                    200,
                    serde_json::to_value(other).map_err(HttpError::internal)?,
                )),
            })
    }

    pub(super) fn create_terminal(&self, conversation_id: &str, body: &[u8]) -> HttpResult {
        let request: TerminalCreateRequest = parse_optional_json(body)?;
        self.terminal_response(conversation_id, TerminalRequest::Create { request })
            .and_then(terminal_http_response)
    }

    pub(super) fn get_terminal(&self, conversation_id: &str, terminal_id: &str) -> HttpResult {
        self.terminal_response(
            conversation_id,
            TerminalRequest::Get {
                terminal_id: terminal_id.to_string(),
            },
        )
        .and_then(terminal_http_response)
    }

    pub(super) fn terminate_terminal(
        &self,
        conversation_id: &str,
        terminal_id: &str,
    ) -> HttpResult {
        self.terminal_response(
            conversation_id,
            TerminalRequest::Terminate {
                terminal_id: terminal_id.to_string(),
            },
        )
        .and_then(terminal_http_response)
    }

    fn terminal_response(
        &self,
        conversation_id: &str,
        request: TerminalRequest,
    ) -> HttpResult<TerminalResponse> {
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let request_id = generated_request_id("terminal");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::Terminal {
                    request_id: request_id.clone(),
                    request,
                },
            )
            .map_err(HttpError::internal)?;
        wait_for_event(&rx, Duration::from_secs(30), |event| match event {
            KernelChannelEvent::Terminal {
                request_id: Some(id),
                response,
            } if id == request_id => Some(response),
            _ => None,
        })
    }

    pub(super) fn accept_terminal_stream(
        &self,
        mut stream: TcpStream,
        request: &HttpRequest,
        conversation_id: &str,
        terminal_id: &str,
    ) -> Result<()> {
        accept_websocket(&mut stream, request)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)?;
        let offset = query_u64(&request.query, "offset").unwrap_or(0);
        let request_id = generated_request_id("terminal-attach");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::Terminal {
                    request_id: request_id.clone(),
                    request: TerminalRequest::Attach {
                        terminal_id: terminal_id.to_string(),
                        offset,
                    },
                },
            )
            .map_err(|error| anyhow!("{error:#}"))?;
        let attached = wait_for_event(&rx, Duration::from_secs(30), |event| match event {
            KernelChannelEvent::Terminal {
                request_id: Some(id),
                response,
            } if id == request_id => Some(response),
            _ => None,
        })
        .map_err(|error| anyhow!("{}", error.message))?;
        let (replay, subscriber_id) = match attached {
            TerminalResponse::Attached {
                replay,
                subscriber_id,
            } => (replay, subscriber_id),
            TerminalResponse::Error { message, .. } => {
                send_websocket_json(
                    &mut stream,
                    &json!({"type": "terminal.error", "message": message}),
                )?;
                return Ok(());
            }
            other => {
                send_websocket_json(
                    &mut stream,
                    &json!({"type": "terminal.error", "message": format!("unexpected terminal response: {other:?}")}),
                )?;
                return Ok(());
            }
        };

        send_websocket_json(
            &mut stream,
            &json!({
                "type": "terminal.snapshot",
                "terminal_id": replay.terminal_id,
                "requested_offset": replay.requested_offset,
                "replay_start_offset": replay.replay_start_offset,
                "buffer_start_offset": replay.buffer_start_offset,
                "next_offset": replay.next_offset,
                "dropped_bytes": replay.dropped_bytes,
                "running": replay.running,
            }),
        )?;
        if replay.dropped_bytes > 0 {
            send_websocket_json(
                &mut stream,
                &json!({
                    "type": "terminal.dropped",
                    "buffer_start_offset": replay.buffer_start_offset,
                    "dropped_bytes": replay.dropped_bytes,
                }),
            )?;
        }
        for chunk in replay.chunks {
            send_websocket_json(
                &mut stream,
                &json!({
                    "type": "terminal.output",
                    "terminal_id": terminal_id,
                    "encoding": chunk.encoding,
                    "data": chunk.data,
                }),
            )?;
        }

        let read_stream = stream.try_clone()?;
        let runtime = self.conversation_runtime.clone();
        let conversation_id_for_reader = conversation_id.to_string();
        let terminal_id_for_reader = terminal_id.to_string();
        thread::spawn(move || {
            let mut read_stream = read_stream;
            while let Ok(frame) = read_client_websocket_frame(&mut read_stream) {
                match frame {
                    ClientWebSocketFrame::Binary(bytes) if !bytes.is_empty() => {
                        let _ = runtime.send_main_channel_ingress(
                            &conversation_id_for_reader,
                            ChannelIngress::Terminal {
                                request_id: generated_request_id("terminal-input"),
                                request: TerminalRequest::Input {
                                    terminal_id: terminal_id_for_reader.clone(),
                                    encoding: TerminalDataEncoding::Base64,
                                    data: general_purpose::STANDARD.encode(bytes),
                                },
                            },
                        );
                    }
                    ClientWebSocketFrame::Text(text) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            handle_terminal_control_frame(
                                &runtime,
                                &conversation_id_for_reader,
                                &terminal_id_for_reader,
                                value,
                            );
                        }
                    }
                    ClientWebSocketFrame::Close => break,
                    _ => {}
                }
            }
            if let Some(subscriber_id) = subscriber_id {
                let _ = runtime.send_main_channel_ingress(
                    &conversation_id_for_reader,
                    ChannelIngress::Terminal {
                        request_id: generated_request_id("terminal-detach"),
                        request: TerminalRequest::Detach {
                            terminal_id: terminal_id_for_reader,
                            subscriber_id,
                        },
                    },
                );
            }
        });

        loop {
            match rx.recv_timeout(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)) {
                Ok(KernelChannelEvent::Terminal { response, .. }) => match response {
                    TerminalResponse::Output {
                        terminal_id: output_terminal_id,
                        subscriber_id: output_subscriber_id,
                        encoding,
                        data,
                    } if output_terminal_id == terminal_id
                        && output_subscriber_id == subscriber_id =>
                    {
                        send_websocket_json(
                            &mut stream,
                            &json!({
                                "type": "terminal.output",
                                "terminal_id": terminal_id,
                                "encoding": encoding,
                                "data": data,
                            }),
                        )?;
                    }
                    TerminalResponse::Detached {
                        terminal_id: detached_terminal_id,
                        subscriber_id: detached_subscriber_id,
                    } if detached_terminal_id == terminal_id
                        && Some(detached_subscriber_id) == subscriber_id =>
                    {
                        send_websocket_json(
                            &mut stream,
                            &json!({"type": "terminal.closed", "terminal_id": terminal_id}),
                        )?;
                        break;
                    }
                    TerminalResponse::Terminal { terminal }
                        if terminal.terminal_id == terminal_id =>
                    {
                        if !terminal.running {
                            send_websocket_json(
                                &mut stream,
                                &json!({"type": "terminal.closed", "terminal_id": terminal_id}),
                            )?;
                            break;
                        }
                    }
                    TerminalResponse::Error { message, .. } => {
                        send_websocket_json(
                            &mut stream,
                            &json!({"type": "terminal.error", "message": message}),
                        )?;
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    send_websocket_json(
                        &mut stream,
                        &json!({"type": "terminal.heartbeat", "server_time": now_rfc3339()}),
                    )?;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
}

fn handle_terminal_control_frame(
    runtime: &ConversationHostRuntime,
    conversation_id: &str,
    terminal_id: &str,
    value: Value,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("resize") => {
            let cols = value
                .get("cols")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(120);
            let rows = value
                .get("rows")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(30);
            let _ = runtime.send_main_channel_ingress(
                conversation_id,
                ChannelIngress::Terminal {
                    request_id: generated_request_id("terminal-resize"),
                    request: TerminalRequest::Resize {
                        terminal_id: terminal_id.to_string(),
                        request: TerminalResizeRequest { cols, rows },
                    },
                },
            );
        }
        Some("input") => {
            let data = value
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if data.is_empty() {
                return;
            }
            let encoding = match value.get("encoding").and_then(Value::as_str) {
                Some("base64") => TerminalDataEncoding::Base64,
                _ => TerminalDataEncoding::Utf8,
            };
            let _ = runtime.send_main_channel_ingress(
                conversation_id,
                ChannelIngress::Terminal {
                    request_id: generated_request_id("terminal-input"),
                    request: TerminalRequest::Input {
                        terminal_id: terminal_id.to_string(),
                        encoding,
                        data,
                    },
                },
            );
        }
        _ => {}
    }
}

fn terminal_http_response(response: TerminalResponse) -> HttpResult {
    match response {
        TerminalResponse::Terminal { terminal } => Ok(HttpResponse::json(
            200,
            serde_json::to_value(terminal).map_err(HttpError::internal)?,
        )),
        TerminalResponse::Error { message, .. } => Err(HttpError::new(400, message)),
        other => Ok(HttpResponse::json(
            200,
            serde_json::to_value(other).map_err(HttpError::internal)?,
        )),
    }
}
