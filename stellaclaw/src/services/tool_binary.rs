#![allow(dead_code)]

use anyhow::{Context, Result};
use crossbeam_channel::select;
use stellaclaw_core::session_actor::ToolBinaryEnsureRequest;

use crate::{
    config::SandboxConfig,
    conversation_new::{
        ConversationService, ServiceAddr, ServiceCall, ServiceFailure, ServiceOutput,
        ServiceRunContext, ServiceStopped,
    },
    service_protos::tool_binary::{
        decode_request, encode_response, ToolBinaryRequest, ToolBinaryResponse,
    },
    tool_binary_manager::{shared_tool_binary_client, ToolBinaryClient},
};

pub struct ToolBinaryService {
    sandbox: SandboxConfig,
    client: ToolBinaryClient,
}

impl ToolBinaryService {
    pub fn new() -> Self {
        Self::with_sandbox(SandboxConfig::default())
    }

    pub fn with_sandbox(sandbox: SandboxConfig) -> Self {
        Self {
            sandbox,
            client: shared_tool_binary_client(),
        }
    }

    fn with_client(sandbox: SandboxConfig, client: ToolBinaryClient) -> Self {
        Self { sandbox, client }
    }
}

impl ConversationService for ToolBinaryService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        loop {
            select! {
                recv(ctx.stop_rx) -> stop => {
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason: stop.ok().map(|stop| stop.reason),
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload) {
                        Ok(ToolBinaryRequest::Ensure { tool, host }) => {
                            let response = self.ensure_tool(&tool, host);
                            let response = match response {
                                Ok(response) => response,
                                Err(error) => ToolBinaryResponse::Failure {
                                    reason: error.to_string(),
                                },
                            };
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Err(error) => {
                            ctx.outbox.send(ServiceOutput::Failed(ServiceFailure {
                                addr: ctx.addr.clone(),
                                error: format!("bad tool binary payload: {error}"),
                            }))?;
                        }
                    }
                }
            }
        }
    }
}

impl ToolBinaryService {
    fn ensure_tool(&self, tool: &str, host: Option<String>) -> Result<ToolBinaryResponse> {
        let request = ToolBinaryEnsureRequest {
            tool: tool.to_string(),
            host,
        };
        let response = self
            .client
            .ensure(request, &self.sandbox)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("failed to ensure managed tool binary {tool}"))?;
        Ok(ToolBinaryResponse::Ready {
            tool: response.tool,
            version: response.version,
            platform: response.platform,
            local_path: response.local_path,
            remote_path: response.remote_path,
            path_dir: response.path_dir,
        })
    }
}

fn reply(
    source: &ServiceAddr,
    target: &ServiceAddr,
    payload: serde_json::Value,
    response_id: Option<String>,
) -> ServiceCall {
    ServiceCall::response_to(source.clone(), target.clone(), payload, response_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_protos::tool_binary::{decode_response, tool_binary_call};

    #[test]
    fn request_accepts_legacy_field_names() {
        let request = decode_request(serde_json::json!({
            "type": "ensure",
            "name": "ripgrep",
            "remote_host": "example-host",
        }))
        .expect("legacy request decodes");

        assert!(matches!(
            request,
            ToolBinaryRequest::Ensure {
                tool,
                host: Some(host),
            } if tool == "ripgrep" && host == "example-host"
        ));
    }

    #[test]
    fn ready_response_matches_core_tool_binary_shape() {
        let response = ToolBinaryResponse::Ready {
            tool: "ripgrep".to_string(),
            version: "15.1.0".to_string(),
            platform: Some("macos-arm64".to_string()),
            local_path: Some("/tmp/rg".to_string()),
            remote_path: None,
            path_dir: Some("/tmp".to_string()),
        };

        let encoded = encode_response(response).expect("response encodes");
        let decoded = decode_response(encoded.clone()).expect("response decodes");
        assert!(matches!(decoded, ToolBinaryResponse::Ready { .. }));
        assert_eq!(encoded["tool"], "ripgrep");
        assert_eq!(encoded["version"], "15.1.0");
        assert_eq!(encoded["local_path"], "/tmp/rg");
    }

    #[test]
    fn call_builder_targets_tool_binary_service() {
        let call = tool_binary_call(
            ServiceAddr::agent_foreground(),
            ToolBinaryRequest::Ensure {
                tool: "ripgrep".to_string(),
                host: None,
            },
        )
        .expect("call builds");

        assert_eq!(call.target, ServiceAddr::tool_binary());
    }

    #[test]
    fn ensure_unsupported_tool_returns_failure_before_download() {
        let service = ToolBinaryService::new();

        let error = service
            .ensure_tool("unsupported-test-tool", None)
            .expect_err("unsupported tool should fail");

        assert!(
            format!("{error:#}").contains("unsupported managed tool binary unsupported-test-tool")
        );
    }
}
