#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::select;
use serde_json::{json, Value};

use crate::{
    conversation_new::{
        ConversationService, ServiceAddr, ServiceCall, ServiceOutput, ServiceRunContext,
        ServiceStatusUpdate, ServiceStopped,
    },
    memory::{
        shared_workdir_memory_client, MemoryClient, MemoryClientAction, MemoryContextRequest,
        MemoryOptions, MemoryScope, MemorySearchRequest, MemoryService as BackendMemoryService,
        MemorySource, MemoryUpdateRequest, MemoryWriteRequest,
    },
    service_protos::memory::{
        decode_request, encode_response, MemoryRequest, MemoryResponse, MemorySearchResult,
        MemorySourceRef,
    },
};

pub struct MemoryService {
    options: MemoryOptions,
}

impl MemoryService {
    pub fn new() -> Self {
        Self {
            options: MemoryOptions::default(),
        }
    }

    pub fn with_options(options: MemoryOptions) -> Self {
        Self { options }
    }
}

impl ConversationService for MemoryService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        let client =
            shared_workdir_memory_client(ctx.conversation.workdir.clone(), self.options.clone());
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
                        Ok(request) => {
                            let response = handle_memory_request(
                                &ctx,
                                &self.options,
                                &client,
                                &call.source,
                                request,
                            );
                            let response = match response {
                                Ok(response) => response,
                                Err(error) => MemoryResponse::Failure {
                                    reason: memory_failure_reason(&error),
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
                            ctx.outbox.send(ServiceOutput::Failed(crate::conversation_new::ServiceFailure {
                                addr: ctx.addr.clone(),
                                error: format!("bad memory payload: {error}"),
                            }))?;
                        }
                    }
                }
            }
        }
    }
}

fn handle_memory_request(
    ctx: &ServiceRunContext,
    options: &MemoryOptions,
    client: &MemoryClient,
    call_source: &ServiceAddr,
    request: MemoryRequest,
) -> Result<MemoryResponse> {
    match request {
        MemoryRequest::Search {
            source,
            query,
            scopes,
            limit,
        } => {
            let source = memory_source(ctx, call_source, source);
            let limit = limit.unwrap_or(5).clamp(1, 20);
            let scopes = match normalize_search_scopes(scopes) {
                Ok(scopes) => scopes,
                Err(error) => {
                    return Ok(MemoryResponse::Failure {
                        reason: memory_failure_reason(&error),
                    });
                }
            };
            let mut outputs = Vec::new();
            if scopes.contains(&MemoryScope::Conversation) {
                let local = backend(ctx, options, source.clone());
                outputs.push(local.search(MemorySearchRequest {
                    query: query.clone(),
                    limit: Some(limit),
                    scopes: vec![scope_name(MemoryScope::Conversation).to_string()],
                })?);
            }
            if scopes.contains(&MemoryScope::Public) {
                outputs.push(client.execute(
                    ctx.conversation.conversation_root.clone(),
                    source,
                    MemoryClientAction::Search(MemorySearchRequest {
                        query,
                        limit: Some(limit),
                        scopes: vec![scope_name(MemoryScope::Public).to_string()],
                    }),
                )?);
            }
            let response = parse_merged_search_outputs(outputs, limit)?;
            let _ = ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "search_completed".to_string(),
                detail: json!({
                    "result_count": match &response {
                        MemoryResponse::SearchResults { results, .. } => results.len(),
                        _ => 0,
                    },
                }),
            }));
            Ok(response)
        }
        MemoryRequest::Write {
            source,
            scope,
            subject,
            text,
            tags,
        } => {
            let source = memory_source(ctx, call_source, source);
            let output = if memory_scope_uses_workdir_manager(scope) {
                client.execute(
                    ctx.conversation.conversation_root.clone(),
                    source,
                    MemoryClientAction::Write(MemoryWriteRequest {
                        scope: scope_name(scope).to_string(),
                        subject,
                        text,
                        tags,
                    }),
                )?
            } else {
                backend(ctx, options, source).write(MemoryWriteRequest {
                    scope: scope_name(scope).to_string(),
                    subject,
                    text,
                    tags,
                })?
            };
            accepted_or_failure(output)
        }
        MemoryRequest::Update {
            source,
            memory_id,
            text,
        } => {
            let source = memory_source(ctx, call_source, source);
            let action = MemoryClientAction::Update(MemoryUpdateRequest {
                memory_id: memory_id.clone(),
                text: text.clone(),
            });
            let output = if memory_id_uses_workdir_manager(&memory_id) {
                client.execute(ctx.conversation.conversation_root.clone(), source, action)?
            } else {
                backend(ctx, options, source).update(MemoryUpdateRequest { memory_id, text })?
            };
            accepted_or_failure(output)
        }
        MemoryRequest::Delete { source, memory_id } => {
            let source = memory_source(ctx, call_source, source);
            let action = MemoryClientAction::Delete(crate::memory::MemoryDeleteRequest {
                memory_id: memory_id.clone(),
            });
            let output = if memory_id_uses_workdir_manager(&memory_id) {
                client.execute(ctx.conversation.conversation_root.clone(), source, action)?
            } else {
                backend(ctx, options, source)
                    .delete(crate::memory::MemoryDeleteRequest { memory_id })?
            };
            accepted_or_failure(output)
        }
        MemoryRequest::PromptContext { scope, max_bytes } => {
            let source = MemorySource {
                conversation_id: ctx.conversation.conversation_id.clone(),
                agent_id: None,
                session_type: "memory_service".to_string(),
            };
            let block = backend(ctx, options, source)
                .prompt_context(MemoryContextRequest { scope, max_bytes })?;
            Ok(MemoryResponse::PromptContext {
                scope: block.scope,
                text: block.text,
                entries_hash: block.entries_hash,
                rendered_size_bytes: block.rendered_size_bytes,
                truncated: block.truncated,
            })
        }
        MemoryRequest::Maintain => {
            let source = MemorySource {
                conversation_id: ctx.conversation.conversation_id.clone(),
                agent_id: None,
                session_type: "memory_service".to_string(),
            };
            backend(ctx, options, source).maintain_user_memory()?;
            Ok(MemoryResponse::MaintenanceCompleted)
        }
    }
}

fn backend(
    ctx: &ServiceRunContext,
    options: &MemoryOptions,
    source: MemorySource,
) -> BackendMemoryService {
    BackendMemoryService::with_options(
        ctx.conversation.workdir.clone(),
        ctx.conversation.conversation_root.clone(),
        source,
        options.clone(),
    )
}

fn memory_source(
    ctx: &ServiceRunContext,
    call_source: &ServiceAddr,
    source: Option<MemorySourceRef>,
) -> MemorySource {
    let source = source.unwrap_or_else(|| MemorySourceRef {
        conversation_id: ctx.conversation.conversation_id.clone(),
        agent_addr: Some(call_source.clone()),
        session_type: "service_call".to_string(),
    });
    MemorySource {
        conversation_id: source.conversation_id,
        agent_id: source.agent_addr.map(|addr| addr.to_string()),
        session_type: source.session_type,
    }
}

fn normalize_search_scopes(scopes: Vec<MemoryScope>) -> Result<Vec<MemoryScope>> {
    let scopes = if scopes.is_empty() {
        vec![MemoryScope::Conversation, MemoryScope::Public]
    } else {
        scopes
    };
    for scope in &scopes {
        if !matches!(scope, MemoryScope::Conversation | MemoryScope::Public) {
            return Err(anyhow!(
                "memory_search only supports conversation and public scopes"
            ));
        }
    }
    Ok(scopes)
}

fn parse_merged_search_outputs(outputs: Vec<Value>, limit: usize) -> Result<MemoryResponse> {
    let mut results = Vec::new();
    let mut truncated = false;
    for output in outputs {
        if output.get("status").and_then(Value::as_str) != Some("success") {
            return Ok(MemoryResponse::Failure {
                reason: output
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("storage_error")
                    .to_string(),
            });
        }
        truncated |= output
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(items) = output.get("results").and_then(Value::as_array) {
            for item in items {
                results.push(
                    serde_json::from_value::<MemorySearchResult>(item.clone())
                        .context("failed to decode memory search result")?,
                );
            }
        }
    }
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| memory_scope_rank(left.scope).cmp(&memory_scope_rank(right.scope)))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    if results.len() > limit {
        results.truncate(limit);
        truncated = true;
    }
    Ok(MemoryResponse::SearchResults { results, truncated })
}

fn accepted_or_failure(output: Value) -> Result<MemoryResponse> {
    match output.get("status").and_then(Value::as_str) {
        Some("success") => Ok(MemoryResponse::Accepted),
        Some("failure") => Ok(MemoryResponse::Failure {
            reason: output
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("storage_error")
                .to_string(),
        }),
        _ => Err(anyhow!("memory backend returned invalid status: {output}")),
    }
}

fn memory_scope_uses_workdir_manager(scope: MemoryScope) -> bool {
    matches!(scope, MemoryScope::User | MemoryScope::Public)
}

fn memory_id_uses_workdir_manager(memory_id: &str) -> bool {
    memory_id.starts_with("u_") || memory_id.starts_with("p_")
}

fn scope_name(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::User => "user",
        MemoryScope::Public => "public",
        MemoryScope::Conversation => "conversation",
    }
}

fn memory_scope_rank(scope: MemoryScope) -> u8 {
    match scope {
        MemoryScope::Conversation => 0,
        MemoryScope::Public => 1,
        MemoryScope::User => 2,
    }
}

fn memory_failure_reason(error: &anyhow::Error) -> String {
    let reason = error.to_string();
    if reason.contains("entry_too_large") {
        "entry_too_large".to_string()
    } else if reason.contains("memory_store_entry_limit") {
        "memory_store_entry_limit".to_string()
    } else if reason.contains("memory_store_too_large") {
        "memory_store_too_large".to_string()
    } else if reason.contains("invalid_action") {
        "invalid_action".to_string()
    } else if reason.contains("memory text must not be empty") {
        "memory_too_vague".to_string()
    } else if reason.contains("memory_search only supports") {
        "unsupported_scope".to_string()
    } else {
        "storage_error".to_string()
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
    use std::fs;

    use super::*;
    use crate::{
        conversation_new::{ConversationRef, ServiceRefs},
        service_protos::memory::{decode_response, memory_call},
    };

    fn test_ctx(name: &str) -> ServiceRunContext {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-memory-service-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("conversation")).expect("conversation root should exist");
        let (_inbox_tx, inbox) = crossbeam_channel::unbounded();
        let (outbox, _outbox_rx) = crossbeam_channel::unbounded();
        let (_stop_tx, stop_rx) = crossbeam_channel::unbounded();
        ServiceRunContext {
            addr: ServiceAddr::memory(),
            conversation: ConversationRef {
                conversation_id: name.to_string(),
                workdir: root.clone(),
                conversation_root: root.join("conversation"),
            },
            storage: root.join("storage"),
            refs: ServiceRefs::default(),
            inbox,
            outbox,
            stop_rx,
        }
    }

    #[test]
    fn writes_and_searches_conversation_memory() {
        let ctx = test_ctx("conversation_search");
        let client = shared_workdir_memory_client(
            ctx.conversation.workdir.clone(),
            MemoryOptions::default(),
        );
        let source = ServiceAddr::agent_foreground();
        let write = handle_memory_request(
            &ctx,
            &MemoryOptions::default(),
            &client,
            &source,
            MemoryRequest::Write {
                source: None,
                scope: MemoryScope::Conversation,
                subject: Some("repo".to_string()),
                text: "The repository uses a service-tree conversation runtime.".to_string(),
                tags: vec!["architecture".to_string()],
            },
        )
        .expect("write should succeed");
        assert!(matches!(write, MemoryResponse::Accepted));

        let search = handle_memory_request(
            &ctx,
            &MemoryOptions::default(),
            &client,
            &source,
            MemoryRequest::Search {
                source: None,
                query: "service tree runtime".to_string(),
                scopes: vec![MemoryScope::Conversation],
                limit: Some(5),
            },
        )
        .expect("search should succeed");
        assert!(matches!(
            search,
            MemoryResponse::SearchResults { results, .. }
                if results.iter().any(|item| item.scope == MemoryScope::Conversation
                    && item.text.contains("service-tree"))
        ));
    }

    #[test]
    fn rejects_user_scope_search() {
        let ctx = test_ctx("reject_user_search");
        let client = shared_workdir_memory_client(
            ctx.conversation.workdir.clone(),
            MemoryOptions::default(),
        );
        let response = handle_memory_request(
            &ctx,
            &MemoryOptions::default(),
            &client,
            &ServiceAddr::agent_foreground(),
            MemoryRequest::Search {
                source: None,
                query: "anything".to_string(),
                scopes: vec![MemoryScope::User],
                limit: None,
            },
        )
        .expect("request should produce response");
        assert!(matches!(
            response,
            MemoryResponse::Failure { reason } if reason == "unsupported_scope"
        ));
    }

    #[test]
    fn protocol_round_trips_memory_response() {
        let call = memory_call(
            ServiceAddr::agent_foreground(),
            MemoryRequest::Search {
                source: None,
                query: "repo".to_string(),
                scopes: vec![MemoryScope::Conversation],
                limit: Some(3),
            },
        )
        .expect("call encodes");
        assert_eq!(call.target, ServiceAddr::memory());
        let response = MemoryResponse::SearchResults {
            results: Vec::new(),
            truncated: false,
        };
        let decoded = decode_response(encode_response(response).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            MemoryResponse::SearchResults {
                results,
                truncated: false
            } if results.is_empty()
        ));
    }
}
