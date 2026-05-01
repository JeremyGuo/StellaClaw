use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossbeam_channel::Sender;
use serde_json::json;
#[cfg(test)]
use serde_json::{Map, Value};

#[cfg(test)]
use super::tool_runtime::ExecutionTarget;
use super::{
    tool_catalog::{
        execute_download_tool, execute_file_tool, execute_media_tool, execute_process_tool,
        execute_provider_backed_media_tool, execute_skill_load_tool, execute_web_tool,
    },
    tool_runtime::{
        normalize_tool_value, parse_arguments, LocalToolError, ToolCancellationToken,
        ToolExecutionContext,
    },
    ChatMessage, ContextItem, ConversationBridge, ToolBatch, ToolBatchCompletion, ToolBatchError,
    ToolBatchExecutor, ToolBatchHandle, ToolExecutionOp, ToolRemoteMode, ToolResultContent,
    ToolResultItem,
};

const TOOL_INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const TOOL_COOPERATIVE_INTERRUPT_GRACE: Duration = Duration::from_millis(250);
const MAX_TOOL_RESULT_CONTEXT_CHARS: usize = 100_000;
const DEFAULT_TRUNCATED_TOOL_RESULT_PREVIEW_CHARS: usize = 80_000;

pub struct LocalToolBatchExecutor {
    workspace_root: PathBuf,
    data_root: PathBuf,
    remote_mode: ToolRemoteMode,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    running_batches: Mutex<HashMap<String, RunningToolBatch>>,
}

impl LocalToolBatchExecutor {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        let data_root = match std::env::var_os("STELLACLAW_DATA_ROOT") {
            Some(value) => PathBuf::from(value),
            None => workspace_root.clone(),
        };
        Self {
            workspace_root,
            data_root,
            remote_mode: ToolRemoteMode::Selectable,
            conversation_bridge: None,
            running_batches: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_remote_mode(mut self, remote_mode: ToolRemoteMode) -> Self {
        self.remote_mode = remote_mode;
        self
    }

    pub fn with_conversation_bridge(
        mut self,
        conversation_bridge: Arc<dyn ConversationBridge + Send + Sync>,
    ) -> Self {
        self.conversation_bridge = Some(conversation_bridge);
        self
    }

    fn spawn_batch_worker(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
    ) -> (ToolCancellationToken, JoinHandle<()>) {
        let batch_id = batch.batch_id.clone();
        let cancel_token = ToolCancellationToken::default();
        let runner = ToolBatchRunner {
            workspace_root: self.workspace_root.clone(),
            data_root: self.data_root.clone(),
            remote_mode: self.remote_mode.clone(),
            conversation_bridge: self.conversation_bridge.clone(),
            cancel_token: cancel_token.clone(),
        };
        let join_handle = thread::spawn(move || {
            let result = runner
                .execute_batch(batch)
                .map_err(|error| error.to_string());
            let _ = completion_tx.send(ToolBatchCompletion { batch_id, result });
        });

        (cancel_token, join_handle)
    }

    #[cfg(test)]
    fn execution_target(
        &self,
        arguments: &Map<String, Value>,
    ) -> Result<ExecutionTarget, LocalToolError> {
        self.context().execution_target(arguments)
    }

    #[cfg(test)]
    fn context(&self) -> ToolExecutionContext<'_> {
        ToolExecutionContext {
            workspace_root: &self.workspace_root,
            data_root: &self.data_root,
            remote_mode: &self.remote_mode,
            cancel_token: ToolCancellationToken::default(),
        }
    }
}

struct RunningToolBatch {
    cancel_token: ToolCancellationToken,
    join_handle: JoinHandle<()>,
}

struct ToolBatchRunner {
    workspace_root: PathBuf,
    data_root: PathBuf,
    remote_mode: ToolRemoteMode,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    cancel_token: ToolCancellationToken,
}

impl ToolBatchRunner {
    fn execute_batch(&self, batch: ToolBatch) -> Result<ChatMessage, LocalToolError> {
        if batch.is_empty() {
            return Err(LocalToolError::EmptyBatch(batch.batch_id));
        }

        let mut results = Vec::with_capacity(batch.operations.len());
        for (index, operation) in batch.operations.iter().enumerate() {
            if self.is_interrupted() {
                results.extend(interrupted_results(&batch.operations[index..]));
                break;
            }

            match self.execute_operation_interruptibly(operation.clone()) {
                Ok(result) => results.push(result),
                Err(OperationOutcome::ToolError(error)) => {
                    results.push(tool_error_result(operation, error))
                }
                Err(OperationOutcome::Interrupted) => {
                    results.extend(interrupted_results(&batch.operations[index..]));
                    break;
                }
            }
        }

        Ok(batch.into_result_message(results))
    }

    fn execute_operation_interruptibly(
        &self,
        operation: ToolExecutionOp,
    ) -> Result<ToolResultItem, OperationOutcome> {
        let (result_tx, result_rx) = mpsc::channel();
        let runner = ToolOperationRunner {
            workspace_root: self.workspace_root.clone(),
            data_root: self.data_root.clone(),
            remote_mode: self.remote_mode.clone(),
            conversation_bridge: self.conversation_bridge.clone(),
            cancel_token: self.cancel_token.clone(),
        };
        let join_handle = thread::spawn(move || {
            let result = runner
                .execute_operation(&operation)
                .map_err(|error| error.to_string());
            let _ = result_tx.send(result);
        });

        loop {
            if self.is_interrupted() {
                return wait_for_cooperative_interrupt(result_rx, join_handle);
            }

            match result_rx.recv_timeout(TOOL_INTERRUPT_POLL_INTERVAL) {
                Ok(result) => return finish_operation_result(result, join_handle),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return finish_disconnected_operation(join_handle)
                }
            }
        }
    }

    fn is_interrupted(&self) -> bool {
        self.cancel_token.is_cancelled()
    }
}

enum OperationOutcome {
    ToolError(String),
    Interrupted,
}

fn wait_for_cooperative_interrupt(
    result_rx: mpsc::Receiver<Result<ToolResultItem, String>>,
    join_handle: JoinHandle<()>,
) -> Result<ToolResultItem, OperationOutcome> {
    match result_rx.recv_timeout(TOOL_COOPERATIVE_INTERRUPT_GRACE) {
        Ok(result) => finish_operation_result(result, join_handle),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            drop(join_handle);
            Err(OperationOutcome::Interrupted)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => finish_disconnected_operation(join_handle),
    }
}

fn finish_operation_result(
    result: Result<ToolResultItem, String>,
    join_handle: JoinHandle<()>,
) -> Result<ToolResultItem, OperationOutcome> {
    join_handle
        .join()
        .map_err(|_| OperationOutcome::ToolError("tool panicked".to_string()))?;
    result.map_err(OperationOutcome::ToolError)
}

fn finish_disconnected_operation(
    join_handle: JoinHandle<()>,
) -> Result<ToolResultItem, OperationOutcome> {
    let _ = join_handle.join();
    Err(OperationOutcome::ToolError("tool stopped".to_string()))
}

struct ToolOperationRunner {
    workspace_root: PathBuf,
    data_root: PathBuf,
    remote_mode: ToolRemoteMode,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    cancel_token: ToolCancellationToken,
}

impl ToolOperationRunner {
    fn execute_operation(
        &self,
        operation: &ToolExecutionOp,
    ) -> Result<ToolResultItem, LocalToolError> {
        let result = match operation {
            ToolExecutionOp::LocalTool(tool_call) => self.execute_local_tool(tool_call),
            ToolExecutionOp::SkillLoad { tool_call, skill } => {
                self.execute_skill_load_tool(tool_call, skill)
            }
            ToolExecutionOp::ProviderBacked {
                tool_call,
                kind,
                model_config,
            } => self.execute_provider_backed_tool(tool_call, *kind, model_config),
            ToolExecutionOp::WebSearch { tool_call, models } => {
                self.execute_web_search_tool(tool_call, models)
            }
            ToolExecutionOp::ConversationBridge(request) => match self.conversation_bridge.as_ref()
            {
                Some(bridge) => bridge
                    .call(request.clone())
                    .map(|response| response.result)
                    .map_err(|error| LocalToolError::Bridge(error.to_string())),
                None => Err(LocalToolError::Bridge(
                    "conversation bridge is not configured".to_string(),
                )),
            },
        }?;
        Ok(self.cap_tool_result_context(result))
    }

    fn execute_local_tool(
        &self,
        tool_call: &super::ToolCallItem,
    ) -> Result<ToolResultItem, LocalToolError> {
        let arguments = parse_arguments(&tool_call.arguments.text)?;
        let context = self.context();
        let result = match execute_file_tool(&tool_call.tool_name, &arguments, &context)? {
            Some(result) => result,
            None => match execute_process_tool(&tool_call.tool_name, &arguments, &context)? {
                Some(result) => result,
                None => match execute_download_tool(&tool_call.tool_name, &arguments, &context)? {
                    Some(result) => result,
                    None => match execute_web_tool(&tool_call.tool_name, &arguments, None)? {
                        Some(result) => result,
                        None => {
                            match execute_media_tool(&tool_call.tool_name, &arguments, &context)? {
                                Some(result) => {
                                    return Ok(ToolResultItem {
                                        tool_call_id: tool_call.tool_call_id.clone(),
                                        tool_name: tool_call.tool_name.clone(),
                                        result,
                                    });
                                }
                                None => {
                                    return Err(LocalToolError::UnsupportedTool(
                                        tool_call.tool_name.clone(),
                                    ));
                                }
                            }
                        }
                    },
                },
            },
        };

        Ok(ToolResultItem {
            tool_call_id: tool_call.tool_call_id.clone(),
            tool_name: tool_call.tool_name.clone(),
            result: ToolResultContent {
                context: Some(ContextItem {
                    text: normalize_tool_value(result),
                }),
                file: None,
            },
        })
    }

    fn execute_provider_backed_tool(
        &self,
        tool_call: &super::ToolCallItem,
        kind: super::ProviderBackedToolKind,
        model_config: &crate::model_config::ModelConfig,
    ) -> Result<ToolResultItem, LocalToolError> {
        let arguments = parse_arguments(&tool_call.arguments.text)?;
        let context = self.context();
        let result = execute_provider_backed_media_tool(
            &tool_call.tool_name,
            kind,
            model_config,
            &arguments,
            &context,
        )?;
        Ok(ToolResultItem {
            tool_call_id: tool_call.tool_call_id.clone(),
            tool_name: tool_call.tool_name.clone(),
            result,
        })
    }

    fn execute_web_search_tool(
        &self,
        tool_call: &super::ToolCallItem,
        models: &super::SearchToolModels,
    ) -> Result<ToolResultItem, LocalToolError> {
        let arguments = parse_arguments(&tool_call.arguments.text)?;
        let result = execute_web_tool(&tool_call.tool_name, &arguments, Some(models))?
            .ok_or_else(|| LocalToolError::UnsupportedTool(tool_call.tool_name.clone()))?;
        Ok(ToolResultItem {
            tool_call_id: tool_call.tool_call_id.clone(),
            tool_name: tool_call.tool_name.clone(),
            result: ToolResultContent {
                context: Some(ContextItem {
                    text: normalize_tool_value(result),
                }),
                file: None,
            },
        })
    }

    fn execute_skill_load_tool(
        &self,
        tool_call: &super::ToolCallItem,
        skill: &super::SessionSkillObservation,
    ) -> Result<ToolResultItem, LocalToolError> {
        let result = execute_skill_load_tool(skill)?;
        Ok(ToolResultItem {
            tool_call_id: tool_call.tool_call_id.clone(),
            tool_name: tool_call.tool_name.clone(),
            result: ToolResultContent {
                context: Some(ContextItem {
                    text: normalize_tool_value(result),
                }),
                file: None,
            },
        })
    }

    fn context(&self) -> ToolExecutionContext<'_> {
        ToolExecutionContext {
            workspace_root: &self.workspace_root,
            data_root: &self.data_root,
            remote_mode: &self.remote_mode,
            cancel_token: self.cancel_token.clone(),
        }
    }

    fn cap_tool_result_context(&self, mut result: ToolResultItem) -> ToolResultItem {
        let Some(context) = result.result.context.as_mut() else {
            return result;
        };
        let total_chars = context.text.chars().count();
        if total_chars <= MAX_TOOL_RESULT_CONTEXT_CHARS {
            return result;
        }

        let saved_path = save_full_tool_result(
            &self.data_root,
            &result.tool_name,
            &result.tool_call_id,
            &context.text,
        );
        context.text = capped_truncated_tool_result_message(
            total_chars,
            saved_path.as_deref(),
            saved_path.is_none(),
            &context.text,
        );
        result
    }
}

fn capped_truncated_tool_result_message(
    total_chars: usize,
    full_output_path: Option<&str>,
    save_failed: bool,
    original: &str,
) -> String {
    let mut preview_chars = DEFAULT_TRUNCATED_TOOL_RESULT_PREVIEW_CHARS;
    loop {
        let (preview, _) = truncate_context_text(original, preview_chars);
        let message =
            truncated_tool_result_message(total_chars, full_output_path, save_failed, &preview);
        if message.chars().count() <= MAX_TOOL_RESULT_CONTEXT_CHARS || preview_chars == 0 {
            return message;
        }
        preview_chars = preview_chars.saturating_mul(4) / 5;
    }
}

fn truncated_tool_result_message(
    total_chars: usize,
    full_output_path: Option<&str>,
    save_failed: bool,
    preview: &str,
) -> String {
    let note = match full_output_path {
        Some(path) => format!(
            "Tool result exceeded the 100000 character runtime limit and was truncated to 100000 characters. The complete untruncated result was saved at: {path}."
        ),
        None if save_failed => {
            "Tool result exceeded the 100000 character runtime limit and was truncated to 100000 characters. Saving the complete result failed.".to_string()
        }
        None => {
            "Tool result exceeded the 100000 character runtime limit and was truncated to 100000 characters.".to_string()
        }
    };
    normalize_tool_value(json!({
        "truncated": true,
        "limit_chars": MAX_TOOL_RESULT_CONTEXT_CHARS,
        "original_chars": total_chars,
        "full_output_path": full_output_path,
        "note": note,
        "preview": preview,
    }))
}

fn save_full_tool_result(
    workspace_root: &Path,
    tool_name: &str,
    tool_call_id: &str,
    text: &str,
) -> Option<String> {
    let dir = workspace_root
        .join(".stellaclaw")
        .join("output")
        .join("tool_results");
    fs::create_dir_all(&dir).ok()?;
    let file_name = format!(
        "{}-{}-{}.txt",
        nonce(),
        sanitize_path_component(tool_name),
        sanitize_path_component(tool_call_id)
    );
    let path = dir.join(file_name);
    fs::write(&path, text).ok()?;
    Some(path.display().to_string())
}

fn sanitize_path_component(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() {
        "tool".to_string()
    } else {
        safe
    }
}

fn truncate_context_text(value: &str, max_chars: usize) -> (String, bool) {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return (value.to_string(), false);
    }
    if max_chars == 0 {
        return (String::new(), true);
    }

    let marker_template = format!("\n...<{total_chars} chars truncated>...\n");
    let marker_chars = marker_template.chars().count().min(max_chars);
    if marker_chars >= max_chars {
        return (value.chars().take(max_chars).collect(), true);
    }

    let available = max_chars - marker_chars;
    let head_chars = available / 2;
    let tail_chars = available - head_chars;
    let omitted = total_chars.saturating_sub(head_chars + tail_chars);
    let marker = format!("\n...<{omitted} chars truncated>...\n");
    let head = value.chars().take(head_chars).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    (format!("{head}{marker}{tail}"), true)
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

impl ToolBatchExecutor for LocalToolBatchExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
    ) -> Result<ToolBatchHandle, ToolBatchError> {
        if batch.is_empty() {
            return Err(ToolBatchError::EmptyBatch(batch.batch_id));
        }

        let handle = ToolBatchHandle::new(batch.batch_id.clone());
        let mut running_batches = self.running_batches.lock().expect("mutex poisoned");
        if running_batches.contains_key(&handle.batch_id) {
            return Err(ToolBatchError::Start(format!(
                "tool batch {} is already running",
                handle.batch_id
            )));
        }
        let (cancel_token, join_handle) = self.spawn_batch_worker(batch, completion_tx);
        running_batches.insert(
            handle.batch_id.clone(),
            RunningToolBatch {
                cancel_token,
                join_handle,
            },
        );
        Ok(handle)
    }

    fn interrupt(&self, handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
        let running_batches = self.running_batches.lock().expect("mutex poisoned");
        let running = running_batches.get(&handle.batch_id).ok_or_else(|| {
            ToolBatchError::Interrupt(format!("unknown tool batch {}", handle.batch_id))
        })?;
        running.cancel_token.cancel();
        Ok(())
    }

    fn finish(&self, batch_id: &str) -> Result<(), ToolBatchError> {
        let running = self
            .running_batches
            .lock()
            .expect("mutex poisoned")
            .remove(batch_id)
            .ok_or_else(|| ToolBatchError::Finish(format!("unknown tool batch {batch_id}")))?;

        running
            .join_handle
            .join()
            .map_err(|_| ToolBatchError::Finish(format!("tool batch {batch_id} panicked")))
    }
}

fn tool_error_result(operation: &ToolExecutionOp, error: String) -> ToolResultItem {
    let (tool_call_id, tool_name) = match operation {
        ToolExecutionOp::LocalTool(tool_call) => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
        ToolExecutionOp::SkillLoad { tool_call, .. } => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
        ToolExecutionOp::ProviderBacked { tool_call, .. } => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
        ToolExecutionOp::WebSearch { tool_call, .. } => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
        ToolExecutionOp::ConversationBridge(request) => {
            (request.tool_call_id.clone(), request.tool_name.clone())
        }
    };

    ToolResultItem {
        tool_call_id,
        tool_name,
        result: ToolResultContent {
            context: Some(ContextItem {
                text: normalize_tool_value(json!({ "error": error })),
            }),
            file: None,
        },
    }
}

fn interrupted_results(operations: &[ToolExecutionOp]) -> Vec<ToolResultItem> {
    operations
        .iter()
        .map(|operation| {
            tool_error_result(
                operation,
                "tool batch interrupted before this tool completed".to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{mpsc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use crate::session_actor::{
        ChatMessageItem, ConversationBridgeRequest, ConversationBridgeResponse, ToolCallItem,
    };

    use super::*;

    fn temp_workspace() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("stellaclaw_tool_executor_{id}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn tool_call(name: &str, arguments: Value) -> ToolExecutionOp {
        ToolExecutionOp::LocalTool(ToolCallItem {
            tool_call_id: "call_1".to_string(),
            tool_name: name.to_string(),
            arguments: ContextItem {
                text: serde_json::to_string(&arguments).unwrap(),
            },
        })
    }

    fn result_text(message: &ChatMessage, index: usize) -> &str {
        match &message.data[index] {
            ChatMessageItem::ToolResult(result) => {
                result.result.context.as_ref().unwrap().text.as_str()
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn result_file_media_type(message: &ChatMessage, index: usize) -> Option<&str> {
        match &message.data[index] {
            ChatMessageItem::ToolResult(result) => result
                .result
                .file
                .as_ref()
                .and_then(|file| file.media_type.as_deref()),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn result_file_is_crashed(message: &ChatMessage, index: usize) -> bool {
        match &message.data[index] {
            ChatMessageItem::ToolResult(result) => result
                .result
                .file
                .as_ref()
                .and_then(|file| file.state.as_ref())
                .is_some(),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn start_and_wait(executor: &LocalToolBatchExecutor, batch: ToolBatch) -> ChatMessage {
        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx)
            .expect("batch should start");
        let completion = completion_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("batch should complete");
        assert_eq!(completion.batch_id, handle.batch_id);
        executor
            .finish(&completion.batch_id)
            .expect("batch should finish");
        completion.result.expect("batch result should be ok")
    }

    #[test]
    fn executes_file_read_and_write_locally() {
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_1",
            vec![
                tool_call(
                    "file_write",
                    json!({"file_path": "notes/demo.txt", "content": "one\ntwo\n"}),
                ),
                tool_call("file_read", json!({"file_path": "notes/demo.txt"})),
            ],
        );

        let message = start_and_wait(&executor, batch);

        assert_eq!(message.data.len(), 2);
        let text = result_text(&message, 1);
        assert!(text.contains("one"));
        assert!(text.contains("two"));
    }

    #[test]
    fn tool_results_are_capped_and_full_output_is_saved() {
        let workspace = temp_workspace();
        let huge_line = "x".repeat(120_000);
        fs::write(
            workspace.join("huge.txt"),
            format!("first\n{huge_line}\nlast\n"),
        )
        .unwrap();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_huge_read",
            vec![tool_call(
                "file_read",
                json!({"file_path": "huge.txt", "limit": 10}),
            )],
        );

        let message = start_and_wait(&executor, batch);
        let value: Value =
            serde_json::from_str(result_text(&message, 0)).expect("file_read should return JSON");

        assert!(value["truncated"].as_bool().unwrap());
        assert_eq!(value["limit_chars"], MAX_TOOL_RESULT_CONTEXT_CHARS);
        assert!(value["original_chars"].as_u64().unwrap() > MAX_TOOL_RESULT_CONTEXT_CHARS as u64);
        assert!(value["note"]
            .as_str()
            .unwrap()
            .contains("complete untruncated result was saved"));
        assert!(value["preview"]
            .as_str()
            .unwrap()
            .contains("chars truncated"));
        let full_output_path = value["full_output_path"].as_str().unwrap();
        assert!(std::path::Path::new(full_output_path).exists());
        let full_output = fs::read_to_string(full_output_path).unwrap();
        assert!(full_output.contains(&huge_line));
    }

    #[test]
    fn executes_search_list_edit_and_apply_patch_locally() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("src/main.rs"),
            "fn main() {}\nlet marker = 1;\n",
        )
        .unwrap();
        fs::write(workspace.join("src/readme.txt"), "marker text\n").unwrap();
        fs::write(workspace.join(".hidden"), "secret\n").unwrap();

        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_search",
            vec![
                tool_call("glob", json!({"pattern": "**/*.rs", "path": "."})),
                tool_call(
                    "grep",
                    json!({"pattern": "marker", "path": ".", "include": "src/*"}),
                ),
                tool_call("ls", json!({"path": "."})),
                tool_call(
                    "edit",
                    json!({
                        "path": "src/main.rs",
                        "old_text": "let marker = 1;",
                        "new_text": "let marker = 2;"
                    }),
                ),
            ],
        );

        let message = start_and_wait(&executor, batch);

        assert!(result_text(&message, 0).contains("main.rs"));
        assert!(result_text(&message, 1).contains("readme.txt"));
        assert!(result_text(&message, 2).contains("src/"));
        assert!(!result_text(&message, 2).contains(".hidden"));
        assert!(result_text(&message, 3).contains("\"replacements\": 1"));
        assert!(fs::read_to_string(workspace.join("src/main.rs"))
            .unwrap()
            .contains("let marker = 2;"));

        fs::write(workspace.join("patch.txt"), "old\n").unwrap();
        let patch = r#"diff --git a/patch.txt b/patch.txt
--- a/patch.txt
+++ b/patch.txt
@@ -1 +1 @@
-old
+new
"#;
        let patch_batch = ToolBatch::new(
            "batch_patch",
            vec![tool_call(
                "apply_patch",
                json!({"patch": patch, "strip": 1}),
            )],
        );

        let message = start_and_wait(&executor, patch_batch);

        assert!(result_text(&message, 0).contains("\"applied\": true"));
        assert!(result_text(&message, 0).contains("\"out_path\":"));
        assert_eq!(
            fs::read_to_string(workspace.join("patch.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn executes_native_media_load_locally() {
        let workspace = temp_workspace();
        fs::write(workspace.join("image.png"), b"not validated image bytes").unwrap();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_media",
            vec![tool_call("image_load", json!({"path": "image.png"}))],
        );

        let message = start_and_wait(&executor, batch);

        assert_eq!(result_file_media_type(&message, 0), Some("image/png"));
        assert!(result_file_is_crashed(&message, 0));
        assert!(result_text(&message, 0).contains("crashed"));
    }

    #[test]
    fn fixed_remote_mode_rejects_local_remote_argument_selection() {
        let workspace = temp_workspace();
        let executor =
            LocalToolBatchExecutor::new(&workspace).with_remote_mode(ToolRemoteMode::FixedSsh {
                host: "fake-host".to_string(),
                cwd: Some(String::new()),
            });

        let target = executor
            .execution_target(&Map::from_iter([(
                "remote".to_string(),
                Value::String("other-host".to_string()),
            )]))
            .expect("fixed remote target should resolve");

        assert!(matches!(
            &target,
            ExecutionTarget::RemoteSsh {
                host,
                cwd: None
            } if host == "fake-host"
        ));
    }

    #[test]
    fn directory_path_tools_default_to_workspace_root() {
        let workspace = temp_workspace();
        fs::write(workspace.join("root.txt"), "root marker\n").unwrap();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_default_path",
            vec![
                tool_call("ls", json!({})),
                tool_call("glob", json!({"pattern": "*.txt", "path": ""})),
                tool_call("grep", json!({"pattern": "root marker", "path": ""})),
            ],
        );

        let message = start_and_wait(&executor, batch);

        assert!(result_text(&message, 0).contains("root.txt"));
        assert!(result_text(&message, 1).contains("root.txt"));
        assert!(result_text(&message, 2).contains("root.txt"));
    }

    #[test]
    fn executes_web_fetch_locally() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/demo")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("hello web fetch")
            .create();
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_web",
            vec![tool_call(
                "web_fetch",
                json!({
                    "url": format!("{}/demo", server.url()),
                    "timeout_seconds": 3,
                    "max_chars": 100
                }),
            )],
        );

        let message = start_and_wait(&executor, batch);

        assert!(result_text(&message, 0).contains("hello web fetch"));
        assert!(result_text(&message, 0).contains("\"status\": 200"));
    }

    #[test]
    fn executes_shell_and_shell_close_locally() {
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell",
                vec![tool_call(
                    "shell",
                    json!({"session_id": "test_shell", "command": "printf hello", "wait_ms": 1000}),
                )],
            ),
        );

        assert!(result_text(&message, 0).contains("\"running\": false"));
        assert!(result_text(&message, 0).contains("hello"));
        assert!(!result_text(&message, 0).contains("\"stderr\":"));

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_running",
                vec![tool_call(
                    "shell",
                    json!({"session_id": "sleep_shell", "command": "sleep 5", "wait_ms": 1}),
                )],
            ),
        );
        assert!(result_text(&message, 0).contains("\"running\": true"));

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_close",
                vec![tool_call(
                    "shell_close",
                    json!({"session_id": "sleep_shell"}),
                )],
            ),
        );
        assert!(result_text(&message, 0).contains("\"closed\": true"));
    }

    #[test]
    fn shell_reports_missing_command_without_generated_unknown_session() {
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_missing_command",
                vec![tool_call("shell", json!({}))],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("missing command"));
        assert!(!text.contains("unknown shell session"));
    }

    #[test]
    fn shell_truncates_agent_visible_output_but_keeps_artifacts() {
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_truncated",
                vec![tool_call(
                    "shell",
                    json!({
                        "session_id": "truncated_shell",
                        "command": "python3 -c \"print('x' * 4000)\"",
                        "wait_ms": 1000,
                        "max_output_chars": 80
                    }),
                )],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("\"stdout_truncated\": true"));
        assert!(text.contains("\"out_path\":"));
    }

    #[test]
    fn interrupting_running_shell_returns_snapshot_instead_of_error() {
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_shell_interrupt",
            vec![tool_call(
                "shell",
                json!({
                    "session_id": "interrupt_shell",
                    "command": "sleep 5",
                    "wait_ms": 10_000
                }),
            )],
        );
        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx)
            .expect("batch should start");

        thread::sleep(Duration::from_millis(50));
        let wait_started = Instant::now();
        executor
            .interrupt(&handle)
            .expect("interrupt should mark batch cancelled");
        let completion = completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupted shell should return promptly");
        executor
            .finish(&completion.batch_id)
            .expect("interrupted shell batch should finish");
        let message = completion.result.expect("interrupted shell result");
        assert!(wait_started.elapsed() < Duration::from_secs(1));

        let text = result_text(&message, 0);
        assert!(text.contains("\"running\": true"));
        assert!(text.contains("\"session_id\": \"interrupt_shell\""));
        assert!(!text.contains("tool batch interrupted"));

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_interrupt_close",
                vec![tool_call(
                    "shell_close",
                    json!({"session_id": "interrupt_shell"}),
                )],
            ),
        );
        assert!(result_text(&message, 0).contains("\"closed\": true"));
    }

    #[test]
    fn executes_file_download_lifecycle_locally() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/file.txt")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("download body")
            .create();
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_download",
                vec![tool_call(
                    "file_download_start",
                    json!({
                        "url": format!("{}/file.txt", server.url()),
                        "path": "downloads/file.txt",
                        "overwrite": true,
                        "wait_timeout_seconds": 2
                    }),
                )],
            ),
        );

        assert!(result_text(&message, 0).contains("\"completed\": true"));
        assert!(result_text(&message, 0).contains("\"path\": \"downloads/file.txt\""));
        assert!(!result_text(&message, 0).contains(&workspace.display().to_string()));
        assert!(!result_text(&message, 0).contains("\"failed\": false"));
        assert_eq!(
            fs::read_to_string(workspace.join("downloads/file.txt")).unwrap(),
            "download body"
        );
    }

    #[test]
    fn executes_web_search_with_configured_json_endpoint() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"query":"demo","results":[{"title":"Demo","url":"https://example.com"}]}"#,
            )
            .create();
        std::env::set_var(
            "STELLACLAW_WEB_SEARCH_URL",
            format!("{}/search", server.url()),
        );
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_web_search",
                vec![tool_call(
                    "web_search",
                    json!({"query": "demo", "timeout_seconds": 2, "max_results": 3}),
                )],
            ),
        );
        std::env::remove_var("STELLACLAW_WEB_SEARCH_URL");

        assert!(result_text(&message, 0).contains("https://example.com"));
    }

    #[test]
    fn executes_skill_load_from_embedded_metadata() {
        let workspace = temp_workspace();
        let executor = LocalToolBatchExecutor::new(&workspace);
        let batch = ToolBatch::new(
            "batch_skill",
            vec![ToolExecutionOp::SkillLoad {
                tool_call: ToolCallItem {
                    tool_call_id: "call_skill".to_string(),
                    tool_name: "skill_load".to_string(),
                    arguments: ContextItem {
                        text: r#"{"skill_name":"demo"}"#.to_string(),
                    },
                },
                skill: super::super::SessionSkillObservation {
                    name: "demo".to_string(),
                    description: "Demo skill".to_string(),
                    content: "# Demo\nUse demo carefully.".to_string(),
                },
            }],
        );

        let message = start_and_wait(&executor, batch);

        assert!(result_text(&message, 0).contains("\"name\": \"demo\""));
        assert!(result_text(&message, 0).contains("Use demo carefully"));
    }

    struct FakeBridge {
        response: Mutex<Option<ConversationBridgeResponse>>,
        request_tx: mpsc::Sender<ConversationBridgeRequest>,
    }

    impl ConversationBridge for FakeBridge {
        fn call(
            &self,
            request: ConversationBridgeRequest,
        ) -> Result<ConversationBridgeResponse, ToolBatchError> {
            self.request_tx.send(request).unwrap();
            Ok(self.response.lock().unwrap().take().unwrap())
        }
    }

    struct BlockingBridge {
        request_tx: mpsc::Sender<ConversationBridgeRequest>,
        response_rx: Mutex<mpsc::Receiver<ConversationBridgeResponse>>,
    }

    impl ConversationBridge for BlockingBridge {
        fn call(
            &self,
            request: ConversationBridgeRequest,
        ) -> Result<ConversationBridgeResponse, ToolBatchError> {
            self.request_tx.send(request).unwrap();
            self.response_rx
                .lock()
                .unwrap()
                .recv()
                .map_err(|error| ToolBatchError::Bridge(error.to_string()))
        }
    }

    #[test]
    fn executes_conversation_bridge_operations() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let bridge = Arc::new(FakeBridge {
            request_tx,
            response: Mutex::new(Some(ConversationBridgeResponse {
                request_id: "req_1".to_string(),
                tool_call_id: "call_1".to_string(),
                tool_name: "user_tell".to_string(),
                result: ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: "sent".to_string(),
                        }),
                        file: None,
                    },
                },
            })),
        });
        let executor = LocalToolBatchExecutor::new(&workspace).with_conversation_bridge(bridge);
        let batch = ToolBatch::new(
            "batch_2",
            vec![ToolExecutionOp::ConversationBridge(
                ConversationBridgeRequest {
                    request_id: "req_1".to_string(),
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    action: "user_tell".to_string(),
                    payload: json!({"text": "working"}),
                },
            )],
        );

        let message = start_and_wait(&executor, batch);

        assert_eq!(request_rx.recv().unwrap().tool_name, "user_tell");
        assert_eq!(message.data.len(), 1);
    }

    #[test]
    fn interrupt_returns_completed_results_and_marks_remaining_tools_interrupted() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let bridge = Arc::new(BlockingBridge {
            request_tx,
            response_rx: Mutex::new(response_rx),
        });
        let executor = LocalToolBatchExecutor::new(&workspace).with_conversation_bridge(bridge);
        let batch = ToolBatch::new(
            "batch_interrupt",
            vec![
                ToolExecutionOp::LocalTool(ToolCallItem {
                    tool_call_id: "call_write_before".to_string(),
                    tool_name: "file_write".to_string(),
                    arguments: ContextItem {
                        text: serde_json::to_string(
                            &json!({"file_path": "before_interrupt.txt", "content": "written"}),
                        )
                        .unwrap(),
                    },
                }),
                ToolExecutionOp::ConversationBridge(ConversationBridgeRequest {
                    request_id: "req_blocking".to_string(),
                    tool_call_id: "call_bridge".to_string(),
                    tool_name: "user_tell".to_string(),
                    action: "user_tell".to_string(),
                    payload: json!({"text": "blocking"}),
                }),
                ToolExecutionOp::LocalTool(ToolCallItem {
                    tool_call_id: "call_write".to_string(),
                    tool_name: "file_write".to_string(),
                    arguments: ContextItem {
                        text: serde_json::to_string(
                            &json!({"file_path": "after_interrupt.txt", "content": "should not write"}),
                        )
                        .unwrap(),
                    },
                }),
            ],
        );

        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx)
            .expect("batch should start");
        let request = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bridge request should be running asynchronously");
        assert_eq!(request.request_id, "req_blocking");

        executor
            .interrupt(&handle)
            .expect("interrupt should mark batch cancelled");

        let wait_started = Instant::now();
        let completion = completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupted batch should return stable result message");
        executor
            .finish(&completion.batch_id)
            .expect("interrupted batch should finish");
        let message = completion.result.expect("interrupted batch result");
        assert!(wait_started.elapsed() < Duration::from_secs(1));

        assert_eq!(message.data.len(), 3);
        assert!(result_text(&message, 0).contains("\"bytes_written\": 7"));
        assert!(result_text(&message, 1).contains("tool batch interrupted"));
        assert!(result_text(&message, 2).contains("tool batch interrupted"));
        assert_eq!(
            fs::read_to_string(workspace.join("before_interrupt.txt")).unwrap(),
            "written"
        );
        assert!(!workspace.join("after_interrupt.txt").exists());
        drop(response_tx);
    }

    #[test]
    fn interrupt_uses_cooperative_result_if_current_tool_returns_quickly() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let bridge = Arc::new(BlockingBridge {
            request_tx,
            response_rx: Mutex::new(response_rx),
        });
        let executor = LocalToolBatchExecutor::new(&workspace).with_conversation_bridge(bridge);
        let batch = ToolBatch::new(
            "batch_cooperative_interrupt",
            vec![ToolExecutionOp::ConversationBridge(
                ConversationBridgeRequest {
                    request_id: "req_cooperative".to_string(),
                    tool_call_id: "call_bridge".to_string(),
                    tool_name: "user_tell".to_string(),
                    action: "user_tell".to_string(),
                    payload: json!({"text": "cooperative"}),
                },
            )],
        );

        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx)
            .expect("batch should start");
        request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bridge request should be running asynchronously");

        executor
            .interrupt(&handle)
            .expect("interrupt should mark batch cancelled");
        response_tx
            .send(ConversationBridgeResponse {
                request_id: "req_cooperative".to_string(),
                tool_call_id: "call_bridge".to_string(),
                tool_name: "user_tell".to_string(),
                result: ToolResultItem {
                    tool_call_id: "call_bridge".to_string(),
                    tool_name: "user_tell".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: "cooperative status".to_string(),
                        }),
                        file: None,
                    },
                },
            })
            .unwrap();

        let completion = completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cooperative result should be accepted during interrupt grace");
        executor
            .finish(&completion.batch_id)
            .expect("cooperative batch should finish");
        let message = completion.result.expect("cooperative batch result");

        assert_eq!(message.data.len(), 1);
        assert!(result_text(&message, 0).contains("cooperative status"));
    }
}
