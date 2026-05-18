use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    thread::{self, JoinHandle},
};

use crossbeam_channel::{select, Receiver, Sender};
#[cfg(test)]
use serde_json::Map;
use serde_json::{json, Value};

#[cfg(test)]
use super::tool_runtime::ExecutionTarget;
use super::{
    tool_catalog::{ToolCallContext, ToolCatalog},
    tool_runtime::{
        normalize_tool_value, parse_arguments, LocalToolError, ToolCancellationToken,
        ToolExecutionContext,
    },
    ChatMessage, ConversationBridge, ProviderBackedToolModels, SearchToolModels, TokenEstimator,
    ToolBatch, ToolBatchCompletion, ToolBatchError, ToolBatchExecutor, ToolBatchHandle,
    ToolBatchItem, ToolBatchOperation, ToolBatchProgress, ToolConcurrency, ToolRemoteMode,
    ToolResultContent, ToolResultItem,
};

const MAX_TOOL_RESULT_CONTEXT_CHARS: usize = 100_000;
const DEFAULT_TRUNCATED_TOOL_RESULT_PREVIEW_CHARS: usize = 80_000;

pub struct LocalToolBatchExecutor {
    workspace_root: PathBuf,
    data_root: PathBuf,
    remote_mode: ToolRemoteMode,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    token_estimator: Option<Arc<TokenEstimator>>,
    search_tool_models: Option<SearchToolModels>,
    provider_backed_tool_models: Option<ProviderBackedToolModels>,
    tool_catalog: Option<ToolCatalog>,
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
            token_estimator: None,
            search_tool_models: None,
            provider_backed_tool_models: None,
            tool_catalog: None,
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

    pub fn with_token_estimator(mut self, token_estimator: Arc<TokenEstimator>) -> Self {
        self.token_estimator = Some(token_estimator);
        self
    }

    pub fn with_search_tool_models(mut self, search_tool_models: SearchToolModels) -> Self {
        self.search_tool_models = Some(search_tool_models);
        self
    }

    pub fn with_provider_backed_tool_models(
        mut self,
        provider_backed_tool_models: ProviderBackedToolModels,
    ) -> Self {
        self.provider_backed_tool_models = Some(provider_backed_tool_models);
        self
    }

    pub fn with_tool_catalog(mut self, tool_catalog: ToolCatalog) -> Self {
        self.tool_catalog = Some(tool_catalog);
        self
    }

    fn spawn_batch_worker(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        progress_tx: Sender<ToolBatchProgress>,
    ) -> (Sender<()>, JoinHandle<()>) {
        let batch_id = batch.batch_id.clone();
        let (interrupt_tx, interrupt_rx) = crossbeam_channel::bounded(1);
        let runner = ToolBatchRunner {
            workspace_root: self.workspace_root.clone(),
            data_root: self.data_root.clone(),
            remote_mode: self.remote_mode.clone(),
            conversation_bridge: self.conversation_bridge.clone(),
            token_estimator: self.token_estimator.clone(),
            search_tool_models: self.search_tool_models.clone(),
            provider_backed_tool_models: self.provider_backed_tool_models.clone(),
            tool_catalog: self.tool_catalog.clone(),
            interrupt_rx,
            operation_lock: Arc::new(RwLock::new(())),
            progress_tx,
        };
        let join_handle = thread::spawn(move || {
            let result = runner
                .execute_batch(batch)
                .map_err(|error| error.to_string());
            let _ = completion_tx.send(ToolBatchCompletion { batch_id, result });
        });

        (interrupt_tx, join_handle)
    }

    #[cfg(test)]
    fn execution_target(
        &self,
        arguments: &Map<String, Value>,
    ) -> Result<ExecutionTarget, LocalToolError> {
        self.context().execution_target(arguments)
    }

    #[cfg(test)]
    fn execution_target_for_path(
        &self,
        arguments: &Map<String, Value>,
        path_keys: &[&str],
    ) -> Result<ExecutionTarget, LocalToolError> {
        self.context()
            .execution_target_for_path(arguments, path_keys)
    }

    #[cfg(test)]
    fn context(&self) -> ToolExecutionContext<'_> {
        ToolExecutionContext {
            workspace_root: &self.workspace_root,
            data_root: &self.data_root,
            remote_mode: &self.remote_mode,
            conversation_bridge: self.conversation_bridge.as_ref(),
            token_estimator: self.token_estimator.as_deref(),
            search_tool_models: self.search_tool_models.as_ref(),
            provider_backed_tool_models: self.provider_backed_tool_models.as_ref(),
            cancel_token: ToolCancellationToken::default(),
        }
    }
}

struct RunningToolBatch {
    interrupt_tx: Sender<()>,
    join_handle: JoinHandle<()>,
}

struct ToolBatchRunner {
    workspace_root: PathBuf,
    data_root: PathBuf,
    remote_mode: ToolRemoteMode,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    token_estimator: Option<Arc<TokenEstimator>>,
    search_tool_models: Option<SearchToolModels>,
    provider_backed_tool_models: Option<ProviderBackedToolModels>,
    tool_catalog: Option<ToolCatalog>,
    interrupt_rx: Receiver<()>,
    operation_lock: Arc<RwLock<()>>,
    progress_tx: Sender<ToolBatchProgress>,
}

impl ToolBatchRunner {
    fn execute_batch(&self, batch: ToolBatch) -> Result<ChatMessage, LocalToolError> {
        if batch.is_empty() {
            return Err(LocalToolError::EmptyBatch(batch.batch_id));
        }

        let mut results = Vec::with_capacity(batch.operations.len());
        let mut index = 0;
        while index < batch.operations.len() {
            if self.interrupt_rx.try_recv().is_ok() {
                let interrupted = interrupted_results(&batch.operations[index..]);
                self.emit_progress_results(&batch.batch_id, &interrupted);
                results.extend(interrupted);
                break;
            }

            if batch.operations[index].concurrency == ToolConcurrency::Serial {
                match self.execute_operation_interruptibly(batch.operations[index].clone()) {
                    OperationOutcome::Completed(result) => {
                        self.emit_progress_result(&batch.batch_id, &result);
                        results.push(result);
                    }
                    OperationOutcome::ToolError(error) => {
                        let result = tool_error_result(&batch.operations[index], error);
                        self.emit_progress_result(&batch.batch_id, &result);
                        results.push(result);
                    }
                    OperationOutcome::Interrupted(result) => {
                        self.emit_progress_result(&batch.batch_id, &result);
                        results.push(result);
                        let interrupted = interrupted_results(&batch.operations[index + 1..]);
                        self.emit_progress_results(&batch.batch_id, &interrupted);
                        results.extend(interrupted);
                        break;
                    }
                }
                index += 1;
                continue;
            }

            let parallel_end = next_serial_operation_index(&batch.operations, index);
            let outcome = self.execute_parallel_operations_interruptibly(
                &batch.batch_id,
                &batch.operations[index..parallel_end],
            );
            results.extend(outcome.results);
            if outcome.interrupted {
                let interrupted = interrupted_results(&batch.operations[parallel_end..]);
                self.emit_progress_results(&batch.batch_id, &interrupted);
                results.extend(interrupted);
                break;
            }
            index = parallel_end;
        }

        Ok(batch.into_result_message(results))
    }

    fn emit_progress_result(&self, batch_id: &str, result: &ToolResultItem) {
        let _ = self.progress_tx.send(ToolBatchProgress {
            batch_id: batch_id.to_string(),
            result: result.clone(),
        });
    }

    fn emit_progress_results(&self, batch_id: &str, results: &[ToolResultItem]) {
        for result in results {
            self.emit_progress_result(batch_id, result);
        }
    }

    fn execute_operation_interruptibly(&self, scheduled: ToolBatchOperation) -> OperationOutcome {
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        let (operation_interrupt_tx, operation_interrupt_rx) = crossbeam_channel::bounded(1);
        let runner = self.operation_runner(ToolCancellationToken::from_interrupt_rx(
            operation_interrupt_rx,
        ));
        let operation_lock = self.operation_lock.clone();
        let join_handle = thread::spawn(move || {
            let result = execute_scheduled_operation(runner, scheduled, operation_lock);
            let _ = result_tx.send(result);
        });

        loop {
            select! {
                recv(result_rx) -> result => {
                    return finish_operation_result(result, join_handle);
                }
                recv(self.interrupt_rx) -> _ => {
                    if let Ok(result) = result_rx.try_recv() {
                        return finish_received_operation_result(result, join_handle);
                    }
                    let _ = operation_interrupt_tx.send(());
                    return match result_rx.recv() {
                        Ok(result) => finish_received_operation_result(result, join_handle).into_interrupted(),
                        Err(_) => finish_disconnected_operation(join_handle).into_interrupted(),
                    };
                }
            }
        }
    }

    fn execute_parallel_operations_interruptibly(
        &self,
        batch_id: &str,
        operations: &[ToolBatchOperation],
    ) -> ParallelSegmentOutcome {
        let (result_tx, result_rx) = crossbeam_channel::unbounded();
        let mut join_handles = Vec::with_capacity(operations.len());
        let mut interrupt_txs = Vec::with_capacity(operations.len());
        for (index, scheduled) in operations.iter().cloned().enumerate() {
            let result_tx = result_tx.clone();
            let (operation_interrupt_tx, operation_interrupt_rx) = crossbeam_channel::bounded(1);
            interrupt_txs.push(Some(operation_interrupt_tx));
            let runner = self.operation_runner(ToolCancellationToken::from_interrupt_rx(
                operation_interrupt_rx,
            ));
            let operation_lock = self.operation_lock.clone();
            join_handles.push(Some(thread::spawn(move || {
                let result = execute_scheduled_operation(runner, scheduled, operation_lock);
                let _ = result_tx.send((index, result));
            })));
        }
        drop(result_tx);

        let mut results: Vec<Option<ToolResultItem>> =
            (0..operations.len()).map(|_| None).collect();
        let mut completed = 0usize;
        let mut interrupted = false;

        while completed < operations.len() {
            select! {
                recv(result_rx) -> result => {
                    let Ok((result_index, result)) = result else {
                        collect_disconnected_parallel_results(
                            operations,
                            &mut join_handles,
                            &mut results,
                        );
                        break;
                    };
                    collect_parallel_result(
                        operations,
                        &mut join_handles,
                        &mut results,
                        result_index,
                        result,
                    );
                    if let Some(result) = results[result_index].as_ref() {
                        self.emit_progress_result(batch_id, result);
                    }
                    completed += 1;
                }
                recv(self.interrupt_rx) -> _ => {
                    interrupted = true;
                    while let Ok((result_index, result)) = result_rx.try_recv() {
                        collect_parallel_result(
                            operations,
                            &mut join_handles,
                            &mut results,
                            result_index,
                            result,
                        );
                        if let Some(result) = results[result_index].as_ref() {
                            self.emit_progress_result(batch_id, result);
                        }
                        completed += 1;
                    }
                    for sender in interrupt_txs.iter_mut().filter_map(Option::take) {
                        let _ = sender.send(());
                    }
                    while completed < operations.len() {
                        match result_rx.recv() {
                            Ok((result_index, result)) => {
                                collect_parallel_result(
                                    operations,
                                    &mut join_handles,
                                    &mut results,
                                    result_index,
                                    result,
                                );
                                if let Some(result) = results[result_index].as_ref() {
                                    self.emit_progress_result(batch_id, result);
                                }
                                completed += 1;
                            }
                            Err(_) => {
                                collect_disconnected_parallel_results(
                                    operations,
                                    &mut join_handles,
                                    &mut results,
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }

        ParallelSegmentOutcome {
            results: results
                .into_iter()
                .enumerate()
                .map(|(index, result)| {
                    result.unwrap_or_else(|| {
                        tool_error_result(&operations[index], "tool stopped".to_string())
                    })
                })
                .collect(),
            interrupted,
        }
    }

    fn operation_runner(&self, cancel_token: ToolCancellationToken) -> ToolOperationRunner {
        ToolOperationRunner {
            workspace_root: self.workspace_root.clone(),
            data_root: self.data_root.clone(),
            remote_mode: self.remote_mode.clone(),
            conversation_bridge: self.conversation_bridge.clone(),
            token_estimator: self.token_estimator.clone(),
            search_tool_models: self.search_tool_models.clone(),
            provider_backed_tool_models: self.provider_backed_tool_models.clone(),
            tool_catalog: self.tool_catalog.clone(),
            cancel_token,
        }
    }
}

struct ParallelSegmentOutcome {
    results: Vec<ToolResultItem>,
    interrupted: bool,
}

fn next_serial_operation_index(operations: &[ToolBatchOperation], start: usize) -> usize {
    operations[start..]
        .iter()
        .position(|operation| operation.concurrency == ToolConcurrency::Serial)
        .map(|offset| start + offset)
        .unwrap_or(operations.len())
        .max(start + 1)
}

fn execute_scheduled_operation(
    runner: ToolOperationRunner,
    scheduled: ToolBatchOperation,
    operation_lock: Arc<RwLock<()>>,
) -> Result<ToolResultItem, String> {
    match scheduled.concurrency {
        ToolConcurrency::Parallel => {
            let _guard = operation_lock
                .read()
                .map_err(|_| "tool execution lock poisoned".to_string())?;
            runner
                .execute_operation(&scheduled.item)
                .map_err(|error| error.to_string())
        }
        ToolConcurrency::Serial => {
            let _guard = operation_lock
                .write()
                .map_err(|_| "tool execution lock poisoned".to_string())?;
            runner
                .execute_operation(&scheduled.item)
                .map_err(|error| error.to_string())
        }
    }
}

fn collect_parallel_result(
    operations: &[ToolBatchOperation],
    join_handles: &mut [Option<JoinHandle<()>>],
    results: &mut [Option<ToolResultItem>],
    index: usize,
    result: Result<ToolResultItem, String>,
) {
    let joined = join_handles
        .get_mut(index)
        .and_then(Option::take)
        .map(|handle| handle.join());
    let result = match joined {
        Some(Ok(())) => result.unwrap_or_else(|error| tool_error_result(&operations[index], error)),
        Some(Err(_)) => tool_error_result(&operations[index], "tool panicked".to_string()),
        None => result.unwrap_or_else(|error| tool_error_result(&operations[index], error)),
    };
    results[index] = Some(result);
}

fn collect_disconnected_parallel_results(
    operations: &[ToolBatchOperation],
    join_handles: &mut [Option<JoinHandle<()>>],
    results: &mut [Option<ToolResultItem>],
) {
    for index in 0..operations.len() {
        if results[index].is_some() {
            continue;
        }
        let result = match join_handles[index].take().map(|handle| handle.join()) {
            Some(Ok(())) => tool_error_result(&operations[index], "tool stopped".to_string()),
            Some(Err(_)) => tool_error_result(&operations[index], "tool panicked".to_string()),
            None => tool_error_result(&operations[index], "tool stopped".to_string()),
        };
        results[index] = Some(result);
    }
}

enum OperationOutcome {
    Completed(ToolResultItem),
    ToolError(String),
    Interrupted(ToolResultItem),
}

impl OperationOutcome {
    fn into_interrupted(self) -> Self {
        match self {
            Self::Completed(result) => Self::Interrupted(result),
            other => other,
        }
    }
}

fn finish_operation_result(
    result: Result<Result<ToolResultItem, String>, crossbeam_channel::RecvError>,
    join_handle: JoinHandle<()>,
) -> OperationOutcome {
    match result {
        Ok(result) => finish_received_operation_result(result, join_handle),
        Err(_) => finish_disconnected_operation(join_handle),
    }
}

fn finish_received_operation_result(
    result: Result<ToolResultItem, String>,
    join_handle: JoinHandle<()>,
) -> OperationOutcome {
    if join_handle.join().is_err() {
        return OperationOutcome::ToolError("tool panicked".to_string());
    }
    match result {
        Ok(result) => OperationOutcome::Completed(result),
        Err(error) => OperationOutcome::ToolError(error),
    }
}

fn finish_disconnected_operation(join_handle: JoinHandle<()>) -> OperationOutcome {
    let _ = join_handle.join();
    OperationOutcome::ToolError("tool stopped".to_string())
}

struct ToolOperationRunner {
    workspace_root: PathBuf,
    data_root: PathBuf,
    remote_mode: ToolRemoteMode,
    conversation_bridge: Option<Arc<dyn ConversationBridge + Send + Sync>>,
    token_estimator: Option<Arc<TokenEstimator>>,
    search_tool_models: Option<SearchToolModels>,
    provider_backed_tool_models: Option<ProviderBackedToolModels>,
    tool_catalog: Option<ToolCatalog>,
    cancel_token: ToolCancellationToken,
}

impl ToolOperationRunner {
    fn execute_operation(&self, item: &ToolBatchItem) -> Result<ToolResultItem, LocalToolError> {
        let result = match item {
            ToolBatchItem::RegisteredTool(tool_call) => self.execute_registered_tool(tool_call),
            ToolBatchItem::UnsupportedTool { reason, .. } => {
                Err(LocalToolError::UnsupportedTool(reason.clone()))
            }
        }?;
        Ok(self.cap_tool_result_context(result))
    }

    fn execute_registered_tool(
        &self,
        tool_call: &super::ToolCallItem,
    ) -> Result<ToolResultItem, LocalToolError> {
        let Some(tool_catalog) = &self.tool_catalog else {
            return Err(LocalToolError::UnsupportedTool(format!(
                "{} requires a registered tool catalog",
                tool_call.tool_name
            )));
        };
        let arguments = parse_arguments(&tool_call.arguments.text)?;
        let execution = self.context();
        let call_context = ToolCallContext { execution };
        let result = tool_catalog.call_tool(
            &tool_call.tool_name,
            &call_context,
            Value::Object(arguments),
        )?;
        Ok(ToolResultItem {
            tool_call_id: tool_call.tool_call_id.clone(),
            tool_name: tool_call.tool_name.clone(),
            result,
        })
    }

    fn context(&self) -> ToolExecutionContext<'_> {
        ToolExecutionContext {
            workspace_root: &self.workspace_root,
            data_root: &self.data_root,
            remote_mode: &self.remote_mode,
            conversation_bridge: self.conversation_bridge.as_ref(),
            token_estimator: self.token_estimator.as_deref(),
            search_tool_models: self.search_tool_models.as_ref(),
            provider_backed_tool_models: self.provider_backed_tool_models.as_ref(),
            cancel_token: self.cancel_token.clone(),
        }
    }

    fn cap_tool_result_context(&self, mut result: ToolResultItem) -> ToolResultItem {
        result.result.normalize_legacy_context();
        let rendered = crate::session_actor::tool_result_text(&result);
        let total_chars = rendered.chars().count();
        if total_chars <= MAX_TOOL_RESULT_CONTEXT_CHARS {
            return result;
        }

        let files = std::mem::take(&mut result.result.files);
        result.result = ToolResultContent::from_json(json!({
            "truncated": true,
            "limit_chars": MAX_TOOL_RESULT_CONTEXT_CHARS,
            "original_chars": total_chars,
            "note": "Tool result exceeded the 100000 character runtime limit and was truncated to 100000 characters. Tools should return bounded output; the complete untruncated result was not saved.",
            "preview": capped_truncated_tool_result_preview(total_chars, &rendered),
        }));
        result.result.files = files;
        result
    }
}

fn capped_truncated_tool_result_preview(total_chars: usize, original: &str) -> String {
    let mut preview_chars = DEFAULT_TRUNCATED_TOOL_RESULT_PREVIEW_CHARS;
    loop {
        let (preview, _) = truncate_context_text(original, preview_chars);
        let message = truncated_tool_result_message(total_chars, &preview);
        if message.chars().count() <= MAX_TOOL_RESULT_CONTEXT_CHARS || preview_chars == 0 {
            return preview;
        }
        preview_chars = preview_chars.saturating_mul(4) / 5;
    }
}

fn truncated_tool_result_message(total_chars: usize, preview: &str) -> String {
    normalize_tool_value(json!({
        "truncated": true,
        "limit_chars": MAX_TOOL_RESULT_CONTEXT_CHARS,
        "original_chars": total_chars,
        "note": "Tool result exceeded the 100000 character runtime limit and was truncated to 100000 characters. Tools should return bounded output; the complete untruncated result was not saved.",
        "preview": preview,
    }))
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

impl ToolBatchExecutor for LocalToolBatchExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        progress_tx: Sender<ToolBatchProgress>,
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
        let (interrupt_tx, join_handle) =
            self.spawn_batch_worker(batch, completion_tx, progress_tx);
        running_batches.insert(
            handle.batch_id.clone(),
            RunningToolBatch {
                interrupt_tx,
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
        let _ = running.interrupt_tx.send(());
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

fn tool_error_result(operation: &ToolBatchOperation, error: String) -> ToolResultItem {
    let (tool_call_id, tool_name) = match &operation.item {
        ToolBatchItem::RegisteredTool(tool_call) => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
        ToolBatchItem::UnsupportedTool { tool_call, .. } => {
            (tool_call.tool_call_id.clone(), tool_call.tool_name.clone())
        }
    };

    ToolResultItem {
        tool_call_id,
        tool_name,
        result: ToolResultContent::from_json(json!({ "error": error })),
    }
}

fn interrupted_results(operations: &[ToolBatchOperation]) -> Vec<ToolResultItem> {
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
        builtin_tool_catalog,
        tool_catalog::{
            BuiltinBaseTool, BuiltinToolCatalogOptions, ExtTool, ToolCallContext, ToolEntry,
            WebSearchOptions,
        },
        ChatMessageItem, ContextItem, ConversationBridgeRequest, ConversationBridgeResponse,
        ToolBackend, ToolCallItem, ToolDefinition, ToolExecutionMode,
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

    fn tool_call(name: &str, arguments: Value) -> ToolBatchItem {
        ToolBatchItem::RegisteredTool(ToolCallItem {
            tool_call_id: "call_1".to_string(),
            tool_name: name.to_string(),
            arguments: ContextItem {
                text: serde_json::to_string(&arguments).unwrap(),
            },
        })
    }

    fn builtin_test_catalog() -> ToolCatalog {
        builtin_tool_catalog(BuiltinToolCatalogOptions {
            web_search: WebSearchOptions {
                enabled: true,
                ..WebSearchOptions::default()
            },
            enable_native_image_view: true,
            enable_native_pdf_view: true,
            enable_native_audio_view: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("builtin test catalog should build")
    }

    fn test_executor(workspace: &PathBuf) -> LocalToolBatchExecutor {
        LocalToolBatchExecutor::new(workspace).with_tool_catalog(builtin_test_catalog())
    }

    fn registered_tool_call(call_id: &str, name: &str, arguments: Value) -> ToolBatchItem {
        ToolBatchItem::RegisteredTool(ToolCallItem {
            tool_call_id: call_id.to_string(),
            tool_name: name.to_string(),
            arguments: ContextItem {
                text: serde_json::to_string(&arguments).unwrap(),
            },
        })
    }

    fn bridge_catalog(tool_names: &[&str]) -> ToolCatalog {
        let mut catalog = ToolCatalog::new();
        for tool_name in tool_names {
            catalog
                .add(ToolDefinition::new(
                    *tool_name,
                    "Test conversation bridge tool.",
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": true
                    }),
                    ToolExecutionMode::Immediate,
                    ToolBackend::ConversationBridge {
                        action: (*tool_name).to_string(),
                    },
                ))
                .expect("bridge tool should register");
        }
        catalog
    }

    fn builtin_plus_bridge_catalog(tool_names: &[&str]) -> ToolCatalog {
        let mut catalog = builtin_test_catalog();
        for tool_name in tool_names {
            catalog
                .add(ToolDefinition::new(
                    *tool_name,
                    "Test conversation bridge tool.",
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": true
                    }),
                    ToolExecutionMode::Immediate,
                    ToolBackend::ConversationBridge {
                        action: (*tool_name).to_string(),
                    },
                ))
                .expect("bridge tool should register");
        }
        catalog
    }

    fn result_text(message: &ChatMessage, index: usize) -> String {
        match &message.data[index] {
            ChatMessageItem::ToolResult(result) => crate::session_actor::tool_result_text(result),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn process_id_from_shell_text(text: &str) -> &str {
        text.lines()
            .find_map(|line| line.strip_prefix("Process running with session ID "))
            .expect("shell result should include process id")
    }

    fn shell_quote_for_test(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn result_file_media_type(message: &ChatMessage, index: usize) -> Option<&str> {
        match &message.data[index] {
            ChatMessageItem::ToolResult(result) => result
                .result
                .files
                .first()
                .and_then(|file| file.media_type.as_deref()),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn result_file_is_crashed(message: &ChatMessage, index: usize) -> bool {
        match &message.data[index] {
            ChatMessageItem::ToolResult(result) => result
                .result
                .files
                .first()
                .and_then(|file| file.state.as_ref())
                .is_some(),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    fn start_and_wait(executor: &LocalToolBatchExecutor, batch: ToolBatch) -> ChatMessage {
        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let (progress_tx, _progress_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx, progress_tx)
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
    fn executes_registered_ext_tool_from_catalog() {
        struct ShellEchoExtTool;

        impl ExtTool for ShellEchoExtTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new(
                    "provider_shell_echo",
                    "Provider-specific shell echo facade.",
                    json!({
                        "type": "object",
                        "properties": {
                            "message": {"type": "string"}
                        },
                        "required": ["message"],
                        "additionalProperties": false
                    }),
                    ToolExecutionMode::Interruptible,
                    ToolBackend::Local,
                )
            }

            fn base_tool_id(&self) -> &'static str {
                "shell_exec"
            }

            fn call(
                &self,
                ctx: &ToolCallContext<'_>,
                args: Value,
            ) -> Result<ToolResultContent, LocalToolError> {
                let message = args.get("message").and_then(Value::as_str).ok_or_else(|| {
                    LocalToolError::InvalidArguments("missing message".to_string())
                })?;
                BuiltinBaseTool::call_local(
                    self.base_tool_id(),
                    ctx,
                    json!({
                        "command": format!("printf {}", shell_quote_for_test(message)),
                        "yield_time_ms": 250,
                        "max_output_chars": 1000,
                    }),
                )
            }
        }

        let workspace = temp_workspace();
        let mut catalog = ToolCatalog::new();
        catalog
            .add_tool_entry(ToolEntry::Ext(Arc::new(ShellEchoExtTool)))
            .expect("ext tool should register");
        let executor = LocalToolBatchExecutor::new(&workspace).with_tool_catalog(catalog);
        let batch = ToolBatch::new(
            "batch_ext",
            vec![ToolBatchItem::RegisteredTool(ToolCallItem {
                tool_call_id: "call_ext".to_string(),
                tool_name: "provider_shell_echo".to_string(),
                arguments: ContextItem {
                    text: serde_json::to_string(&json!({"message": "hello"})).unwrap(),
                },
            })],
        );

        let message = start_and_wait(&executor, batch);
        let ChatMessageItem::ToolResult(result) = &message.data[0] else {
            panic!("expected tool result");
        };
        assert_eq!(result.tool_call_id, "call_ext");
        assert_eq!(result.tool_name, "provider_shell_echo");
        assert!(crate::session_actor::tool_result_text(result).contains("hello"));
    }

    #[test]
    fn tool_results_are_capped_without_saving_full_output() {
        let workspace = temp_workspace();
        let huge_line = "x".repeat(120_000);
        let (request_tx, _request_rx) = mpsc::channel();
        let bridge = Arc::new(FakeBridge {
            request_tx,
            response: Mutex::new(Some(ConversationBridgeResponse {
                request_id: "req_huge".to_string(),
                tool_call_id: "call_huge".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                result: ToolResultItem {
                    tool_call_id: "call_huge".to_string(),
                    tool_name: "cron_tasks_list".to_string(),
                    result: ToolResultContent::from_text(format!("first\n{huge_line}\nlast\n")),
                },
            })),
        });
        let executor = LocalToolBatchExecutor::new(&workspace)
            .with_conversation_bridge(bridge)
            .with_tool_catalog(builtin_plus_bridge_catalog(&["cron_tasks_list"]));
        let batch = ToolBatch::new(
            "batch_huge_bridge",
            vec![registered_tool_call(
                "call_huge",
                "cron_tasks_list",
                json!({}),
            )],
        );

        let message = start_and_wait(&executor, batch);
        let value: Value =
            serde_json::from_str(&result_text(&message, 0)).expect("capped result should be JSON");

        assert!(value["truncated"].as_bool().unwrap());
        assert_eq!(value["limit_chars"], MAX_TOOL_RESULT_CONTEXT_CHARS);
        assert!(value["original_chars"].as_u64().unwrap() > MAX_TOOL_RESULT_CONTEXT_CHARS as u64);
        assert!(value["note"]
            .as_str()
            .unwrap()
            .contains("complete untruncated result was not saved"));
        assert!(value["preview"]
            .as_str()
            .unwrap()
            .contains("chars truncated"));
        assert!(value.get("full_output_path").is_none());
    }

    #[test]
    fn executes_apply_patch_locally() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
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
        assert!(!result_text(&message, 0).contains("\"out_path\":"));
        assert_eq!(
            fs::read_to_string(workspace.join("patch.txt")).unwrap(),
            "new\n"
        );

        fs::write(workspace.join("absolute_patch.txt"), "before\n").unwrap();
        let absolute_patch_path = workspace.join("absolute_patch.txt");
        let absolute_patch = format!(
            "--- {}\n+++ {}\n@@ -1 +1 @@\n-before\n+after\n",
            absolute_patch_path.display(),
            absolute_patch_path.display()
        );
        let absolute_patch_batch = ToolBatch::new(
            "batch_absolute_patch",
            vec![tool_call(
                "apply_patch",
                json!({"patch": absolute_patch, "format": "unified"}),
            )],
        );

        let message = start_and_wait(&executor, absolute_patch_batch);

        assert!(result_text(&message, 0).contains("\"applied\": true"));
        assert_eq!(
            fs::read_to_string(workspace.join("absolute_patch.txt")).unwrap(),
            "after\n"
        );

        fs::write(workspace.join("codex_patch.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let codex_patch = r#"*** Begin Patch
*** Update File: codex_patch.txt
@@
 alpha
-beta
+delta
 gamma
*** Add File: added.txt
+created
*** End Patch
"#;
        let codex_patch_batch = ToolBatch::new(
            "batch_codex_patch",
            vec![tool_call(
                "apply_patch",
                json!({"patch": codex_patch, "format": "codex"}),
            )],
        );

        let message = start_and_wait(&executor, codex_patch_batch);

        assert!(result_text(&message, 0).contains("\"format\": \"codex\""));
        assert!(result_text(&message, 0).contains("\"files_changed\""));
        assert_eq!(
            fs::read_to_string(workspace.join("codex_patch.txt")).unwrap(),
            "alpha\ndelta\ngamma\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("added.txt")).unwrap(),
            "created\n"
        );

        fs::write(workspace.join("freeform_patch.txt"), "red\nblue\n").unwrap();
        let freeform_patch = r#"*** Begin Patch
*** Update File: freeform_patch.txt
@@
 red
-blue
+green
*** End Patch
"#;
        let freeform_patch_batch = ToolBatch::new(
            "batch_freeform_patch",
            vec![tool_call(
                "apply_patch",
                json!({"patch": freeform_patch, "format": "freeform"}),
            )],
        );

        let message = start_and_wait(&executor, freeform_patch_batch);

        assert!(result_text(&message, 0).contains("\"format\": \"freeform\""));
        assert_eq!(
            fs::read_to_string(workspace.join("freeform_patch.txt")).unwrap(),
            "red\ngreen\n"
        );

        fs::write(workspace.join("move_me.txt"), "one\ntwo\n").unwrap();
        fs::write(workspace.join("delete_me.txt"), "remove\n").unwrap();
        let codex_patch_auto = r#"*** Begin Patch
*** Update File: move_me.txt
*** Move to: moved.txt
@@
 one
-two
+three
*** Delete File: delete_me.txt
*** End Patch
"#;
        let codex_patch_auto_batch = ToolBatch::new(
            "batch_codex_patch_auto",
            vec![tool_call("apply_patch", json!({"patch": codex_patch_auto}))],
        );

        let message = start_and_wait(&executor, codex_patch_auto_batch);

        assert!(result_text(&message, 0).contains("\"format\": \"codex\""));
        assert!(!workspace.join("move_me.txt").exists());
        assert!(!workspace.join("delete_me.txt").exists());
        assert_eq!(
            fs::read_to_string(workspace.join("moved.txt")).unwrap(),
            "one\nthree\n"
        );
    }

    #[test]
    fn executes_native_media_view_locally() {
        let workspace = temp_workspace();
        fs::write(workspace.join("image.png"), b"not validated image bytes").unwrap();
        let executor = test_executor(&workspace);
        let batch = ToolBatch::new(
            "batch_media",
            vec![tool_call("image_view", json!({"path": "image.png"}))],
        );

        let message = start_and_wait(&executor, batch);

        assert_eq!(result_file_media_type(&message, 0), Some("image/png"));
        assert!(result_file_is_crashed(&message, 0));
        assert!(result_text(&message, 0).contains("crashed"));
    }

    #[test]
    fn fixed_remote_mode_rejects_local_remote_argument_selection() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace).with_remote_mode(ToolRemoteMode::FixedSsh {
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
    fn fixed_remote_mode_routes_special_workspace_paths_locally() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace).with_remote_mode(ToolRemoteMode::FixedSsh {
            host: "fake-host".to_string(),
            cwd: Some("/remote/project".to_string()),
        });

        let local_target = executor
            .execution_target_for_path(
                &Map::from_iter([(
                    "file_path".to_string(),
                    Value::String(".stellaclaw/attachments/incoming/photo.png".to_string()),
                )]),
                &["file_path", "path"],
            )
            .expect("special path target should resolve");
        assert!(matches!(local_target, ExecutionTarget::Local));

        let remote_target = executor
            .execution_target_for_path(
                &Map::from_iter([(
                    "file_path".to_string(),
                    Value::String("src/main.rs".to_string()),
                )]),
                &["file_path", "path"],
            )
            .expect("ordinary path target should resolve");
        assert!(matches!(remote_target, ExecutionTarget::RemoteSsh { .. }));
    }

    #[test]
    fn fixed_remote_mode_applies_patch_to_stellaclaw_paths_locally() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".stellaclaw")).unwrap();
        fs::write(
            workspace.join(".stellaclaw/apply_patch_smoke_test.txt"),
            "old\n",
        )
        .unwrap();
        let executor = test_executor(&workspace).with_remote_mode(ToolRemoteMode::FixedSsh {
            host: "fake-host".to_string(),
            cwd: Some("/remote/project".to_string()),
        });
        let patch = "\
--- .stellaclaw/apply_patch_smoke_test.txt
+++ .stellaclaw/apply_patch_smoke_test.txt
@@ -1 +1 @@
-old
+new
";
        let batch = ToolBatch::new(
            "batch_local_overlay_patch",
            vec![tool_call(
                "apply_patch",
                json!({"patch": patch, "format": "unified"}),
            )],
        );

        let message = start_and_wait(&executor, batch);

        assert!(result_text(&message, 0).contains("\"applied\": true"));
        assert_eq!(
            fs::read_to_string(workspace.join(".stellaclaw/apply_patch_smoke_test.txt")).unwrap(),
            "new\n"
        );
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
        let executor = test_executor(&workspace);
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
    fn executes_shell_and_shell_stop_locally() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell",
                vec![tool_call(
                    "shell_exec",
                    json!({"command": "printf hello", "yield_time_ms": 1000}),
                )],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("Process exited with code 0"), "{text}");
        assert!(text.contains("hello"), "{text}");
        assert!(!text.contains("Stderr:\n"), "{text}");
        match &message.data[0] {
            ChatMessageItem::ToolResult(result) => {
                assert!(result.result.structured.is_some());
                assert_eq!(
                    result
                        .result
                        .structured
                        .as_ref()
                        .and_then(|value| value.get("kind"))
                        .and_then(Value::as_str),
                    Some("shell_result")
                );
            }
            other => panic!("expected tool result, got {other:?}"),
        }

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_running",
                vec![tool_call(
                    "shell_exec",
                    json!({"command": "sleep 5", "yield_time_ms": 250}),
                )],
            ),
        );
        let running_text = result_text(&message, 0);
        assert!(
            running_text.contains("Process running with session ID "),
            "{running_text}"
        );
        let process_id = process_id_from_shell_text(&running_text);

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_stop",
                vec![tool_call("shell_stop", json!({"process_id": process_id}))],
            ),
        );
        assert!(result_text(&message, 0).contains("\"stopped\": true"));
    }

    #[test]
    fn shell_exec_expands_tilde_workdir_locally() {
        let workspace = temp_workspace();
        let home = std::env::var("HOME").expect("HOME should be set");
        fs::create_dir_all(&home).expect("test HOME should be a directory");
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_home_workdir",
                vec![tool_call(
                    "shell_exec",
                    json!({
                        "command": "pwd",
                        "workdir": "~",
                        "yield_time_ms": 1000
                    }),
                )],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("Process exited with code 0"), "{text}");
        assert!(text.contains(&format!("Stdout:\n{home}")), "{text}");
    }

    #[test]
    fn shell_exec_reports_missing_command() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_missing_command",
                vec![tool_call("shell_exec", json!({}))],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("missing string argument command"));
        assert!(!text.contains("unknown shell session"));
    }

    #[test]
    fn shell_write_empty_observes_but_non_tty_stdin_is_closed() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_running_process",
                vec![tool_call(
                    "shell_exec",
                    json!({"command": "sleep 1; printf done", "yield_time_ms": 250}),
                )],
            ),
        );
        let started = result_text(&message, 0);
        assert!(
            started.contains("Process running with session ID "),
            "{started}"
        );
        let process_id = process_id_from_shell_text(&started);

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_stdin_closed",
                vec![tool_call(
                    "shell_write_stdin",
                    json!({"process_id": process_id, "chars": "ignored\n"}),
                )],
            ),
        );
        assert!(result_text(&message, 0).contains("stdin_closed"));

        thread::sleep(Duration::from_millis(1200));
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_empty_observe",
                vec![tool_call(
                    "shell_write_stdin",
                    json!({"process_id": process_id, "chars": "", "yield_time_ms": 250}),
                )],
            ),
        );
        let text = result_text(&message, 0);
        assert!(text.contains("Process exited with code 0"), "{text}");
        assert!(text.contains("done"), "{text}");
    }

    #[test]
    fn shell_write_stdin_supports_tty_processes() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_tty_start",
                vec![tool_call(
                    "shell_exec",
                    json!({
                        "command": "python3 -c \"import sys; print('ready'); print('got:' + sys.stdin.readline().strip())\"",
                        "tty": true,
                        "yield_time_ms": 250
                    }),
                )],
            ),
        );
        let initial_text = result_text(&message, 0).to_string();
        assert!(
            initial_text.contains("Process running with session ID "),
            "{initial_text}"
        );
        let process_id = process_id_from_shell_text(&initial_text);

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_tty_write",
                vec![tool_call(
                    "shell_write_stdin",
                    json!({"process_id": process_id, "chars": "hello\n", "yield_time_ms": 1000}),
                )],
            ),
        );
        let text = result_text(&message, 0);
        assert!(
            initial_text.contains("ready") || text.contains("ready"),
            "{initial_text}\n{text}"
        );
        assert!(text.contains("got:hello"), "{text}");
        assert!(text.contains("Process exited with code 0"), "{text}");
    }

    #[test]
    fn shell_truncates_agent_visible_output() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_truncated",
                vec![tool_call(
                    "shell_exec",
                    json!({
                        "command": "python3 -c \"print('x' * 4000)\"",
                        "yield_time_ms": 1000,
                        "max_output_tokens": 20
                    }),
                )],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("truncated"));
        assert!(text.contains("Output (truncated):") || text.contains("Stdout (truncated):"));
    }

    #[test]
    fn shell_exec_reports_stdout_and_stderr_separately() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_streams",
                vec![tool_call(
                    "shell_exec",
                    json!({
                        "command": "python3 -c \"import sys; print('out'); print('err', file=sys.stderr)\"",
                        "yield_time_ms": 1000
                    }),
                )],
            ),
        );

        let text = result_text(&message, 0);
        assert!(text.contains("Process exited with code 0"), "{text}");
        assert!(text.contains("Stdout:"), "{text}");
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("Stderr:"), "{text}");
        assert!(text.contains("err"), "{text}");
    }

    #[test]
    fn interrupting_running_shell_returns_snapshot_instead_of_error() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let batch = ToolBatch::new(
            "batch_shell_interrupt",
            vec![tool_call(
                "shell_exec",
                json!({
                    "command": "sleep 5",
                    "yield_time_ms": 10_000
                }),
            )],
        );
        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let (progress_tx, _progress_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx, progress_tx)
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
        assert!(text.contains("Process running with session ID "));
        let process_id = process_id_from_shell_text(&text);
        assert!(!text.contains("tool batch interrupted"));

        let message = start_and_wait(
            &executor,
            ToolBatch::new(
                "batch_shell_interrupt_close",
                vec![tool_call("shell_stop", json!({"process_id": process_id}))],
            ),
        );
        assert!(result_text(&message, 0).contains("\"stopped\": true"));
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
        let executor = test_executor(&workspace);
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
    fn executes_skill_load_through_conversation_bridge() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let bridge = Arc::new(FakeBridge {
            request_tx,
            response: Mutex::new(Some(ConversationBridgeResponse {
                request_id: "req_skill".to_string(),
                tool_call_id: "skill_load".to_string(),
                tool_name: "skill_load".to_string(),
                result: ToolResultItem {
                    tool_call_id: "skill_load".to_string(),
                    tool_name: "skill_load".to_string(),
                    result: ToolResultContent::from_json(json!({
                        "name": "demo",
                        "description": "Demo skill",
                        "content": "# Demo\nUse demo carefully.",
                    })),
                },
            })),
        });
        let executor = LocalToolBatchExecutor::new(&workspace)
            .with_conversation_bridge(bridge)
            .with_tool_catalog(bridge_catalog(&["skill_load"]));
        let batch = ToolBatch::new(
            "batch_skill",
            vec![registered_tool_call(
                "call_skill",
                "skill_load",
                json!({"skill_name": "demo"}),
            )],
        );

        let message = start_and_wait(&executor, batch);
        let request = request_rx.recv().unwrap();
        assert_eq!(request.action, "skill_load");
        assert_eq!(request.payload["skill_name"], "demo");

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

    struct CancelAwareJoinBridge {
        request_tx: mpsc::Sender<ConversationBridgeRequest>,
        original_response_tx: Mutex<Option<mpsc::Sender<ConversationBridgeResponse>>>,
    }

    impl ConversationBridge for CancelAwareJoinBridge {
        fn call(
            &self,
            request: ConversationBridgeRequest,
        ) -> Result<ConversationBridgeResponse, ToolBatchError> {
            self.request_tx.send(request.clone()).unwrap();
            if request.action == "subagent_join_cancel" {
                if let Some(sender) = self.original_response_tx.lock().unwrap().take() {
                    let _ = sender.send(ConversationBridgeResponse {
                        request_id: "req_cooperative".to_string(),
                        tool_call_id: request.tool_call_id.clone(),
                        tool_name: request.tool_name.clone(),
                        result: ToolResultItem {
                            tool_call_id: request.tool_call_id.clone(),
                            tool_name: request.tool_name.clone(),
                            result: ToolResultContent::from_json(json!({
                                "status": "interrupted",
                                "agent_id": "subagent_1",
                                "reason": "tool_interrupted",
                            })),
                        },
                    });
                }
                return Ok(ConversationBridgeResponse {
                    request_id: request.request_id,
                    tool_call_id: request.tool_call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    result: ToolResultItem {
                        tool_call_id: request.tool_call_id,
                        tool_name: request.tool_name,
                        result: ToolResultContent::from_json(json!({"status": "cancelled"})),
                    },
                });
            }

            let (response_tx, response_rx) = mpsc::channel();
            *self.original_response_tx.lock().unwrap() = Some(response_tx);
            response_rx
                .recv()
                .map_err(|error| ToolBatchError::Bridge(error.to_string()))
        }
    }

    #[test]
    fn executes_registered_conversation_bridge_tool() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let bridge = Arc::new(FakeBridge {
            request_tx,
            response: Mutex::new(Some(ConversationBridgeResponse {
                request_id: "req_1".to_string(),
                tool_call_id: "call_1".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                result: ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "cron_tasks_list".to_string(),
                    result: ToolResultContent::from_text("sent".to_string()),
                },
            })),
        });
        let executor = LocalToolBatchExecutor::new(&workspace)
            .with_conversation_bridge(bridge)
            .with_tool_catalog(builtin_plus_bridge_catalog(&["cron_tasks_list"]));
        let batch = ToolBatch::new(
            "batch_2",
            vec![registered_tool_call("call_1", "cron_tasks_list", json!({}))],
        );

        let message = start_and_wait(&executor, batch);

        assert_eq!(request_rx.recv().unwrap().tool_name, "cron_tasks_list");
        assert_eq!(message.data.len(), 1);
    }

    #[test]
    fn parallel_batch_operations_start_without_waiting_for_each_other() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let bridge = Arc::new(BlockingBridge {
            request_tx,
            response_rx: Mutex::new(response_rx),
        });
        let executor = LocalToolBatchExecutor::new(&workspace)
            .with_conversation_bridge(bridge)
            .with_tool_catalog(builtin_plus_bridge_catalog(&["cron_tasks_list"]));
        let batch = ToolBatch::new_scheduled(
            "batch_parallel_bridge",
            vec![
                ToolBatchOperation::new(
                    registered_tool_call("call_parallel_1", "cron_tasks_list", json!({})),
                    ToolConcurrency::Parallel,
                ),
                ToolBatchOperation::new(
                    registered_tool_call("call_parallel_2", "cron_tasks_list", json!({})),
                    ToolConcurrency::Parallel,
                ),
            ],
        );

        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let (progress_tx, _progress_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx, progress_tx)
            .expect("batch should start");
        let first = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first parallel request should start");
        let second = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second parallel request should start before first completes");
        assert_ne!(first.request_id, second.request_id);
        assert_eq!(first.tool_name, "cron_tasks_list");
        assert_eq!(second.tool_name, "cron_tasks_list");

        for request in [first, second] {
            response_tx
                .send(ConversationBridgeResponse {
                    request_id: request.request_id,
                    tool_call_id: request.tool_call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    result: ToolResultItem {
                        tool_call_id: request.tool_call_id,
                        tool_name: request.tool_name,
                        result: ToolResultContent::from_text("sent".to_string()),
                    },
                })
                .unwrap();
        }

        let completion = completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("parallel batch should complete");
        assert_eq!(completion.batch_id, handle.batch_id);
        executor
            .finish(&completion.batch_id)
            .expect("parallel batch should finish");
        let message = completion.result.expect("parallel batch result");
        assert_eq!(message.data.len(), 2);
    }

    #[test]
    fn failed_tool_does_not_skip_later_batch_operations() {
        let workspace = temp_workspace();
        let executor = test_executor(&workspace);
        let batch = ToolBatch::new(
            "batch_error_isolated",
            vec![
                tool_call("not_a_real_tool", json!({})),
                tool_call(
                    "shell_exec",
                    json!({"command": "printf 'still ran' > after_error.txt", "yield_time_ms": 1000}),
                ),
                tool_call(
                    "shell_exec",
                    json!({"command": "cat after_error.txt", "yield_time_ms": 1000}),
                ),
            ],
        );

        let message = start_and_wait(&executor, batch);

        assert_eq!(message.data.len(), 3);
        assert!(result_text(&message, 0).contains("unsupported tool"));
        assert!(result_text(&message, 1).contains("Process exited with code 0"));
        assert!(result_text(&message, 2).contains("still ran"));
        assert_eq!(
            fs::read_to_string(workspace.join("after_error.txt")).unwrap(),
            "still ran"
        );
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
        let executor = LocalToolBatchExecutor::new(&workspace)
            .with_conversation_bridge(bridge)
            .with_tool_catalog(builtin_plus_bridge_catalog(&["cron_tasks_list"]));
        let batch = ToolBatch::new(
            "batch_interrupt",
            vec![
                registered_tool_call(
                    "call_write_before",
                    "apply_patch",
                    json!({"patch": "*** Begin Patch\n*** Add File: before_interrupt.txt\n+written\n*** End Patch\n"}),
                ),
                registered_tool_call("call_bridge", "cron_tasks_list", json!({})),
                registered_tool_call(
                    "call_write",
                    "apply_patch",
                    json!({"patch": "*** Begin Patch\n*** Add File: after_interrupt.txt\n+should not write\n*** End Patch\n"}),
                ),
            ],
        );

        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let (progress_tx, _progress_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx, progress_tx)
            .expect("batch should start");
        let request = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bridge request should be running asynchronously");
        assert_eq!(request.tool_name, "cron_tasks_list");

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
        assert!(result_text(&message, 0).contains("\"applied\": true"));
        assert!(result_text(&message, 1).contains("conversation bridge response"));
        assert!(result_text(&message, 2).contains("tool batch interrupted"));
        assert_eq!(
            fs::read_to_string(workspace.join("before_interrupt.txt")).unwrap(),
            "written\n"
        );
        assert!(!workspace.join("after_interrupt.txt").exists());
        drop(response_tx);
    }

    #[test]
    fn interrupt_returns_bridge_interrupted_result_without_waiting_for_late_response() {
        let workspace = temp_workspace();
        let (request_tx, request_rx) = mpsc::channel();
        let bridge = Arc::new(CancelAwareJoinBridge {
            request_tx,
            original_response_tx: Mutex::new(None),
        });
        let executor = LocalToolBatchExecutor::new(&workspace)
            .with_conversation_bridge(bridge)
            .with_tool_catalog(bridge_catalog(&["subagent_join"]));
        let batch = ToolBatch::new(
            "batch_cooperative_interrupt",
            vec![registered_tool_call(
                "call_bridge",
                "subagent_join",
                json!({"agent_id": "subagent_1", "timeout_seconds": 30}),
            )],
        );

        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let (progress_tx, _progress_rx) = crossbeam_channel::unbounded();
        let handle = executor
            .start(batch, completion_tx, progress_tx)
            .expect("batch should start");
        request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bridge request should be running asynchronously");

        executor
            .interrupt(&handle)
            .expect("interrupt should mark batch cancelled");
        let cancel_request = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel bridge request should be sent");
        assert_eq!(cancel_request.action, "subagent_join_cancel");
        assert!(cancel_request.payload["request_id"]
            .as_str()
            .is_some_and(|request_id| request_id.starts_with("subagent_join_")));

        let completion = completion_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bridge interrupt result should be returned promptly");
        executor
            .finish(&completion.batch_id)
            .expect("interrupted bridge batch should finish");
        let message = completion.result.expect("interrupted bridge batch result");

        assert_eq!(message.data.len(), 1);
        assert!(result_text(&message, 0).contains("\"status\": \"interrupted\""));
    }
}
