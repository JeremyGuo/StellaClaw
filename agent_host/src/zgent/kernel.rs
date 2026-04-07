use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::message::ChatMessage;
use agent_frame::{SessionExecutionControl, SessionRunReport, Tool};
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::backend::AgentBackendKind;
use crate::backend::BackendExecutionOptions;
use crate::zgent::app_bridge::ZgentAppProfileBundle;
use crate::zgent::app_bridge::prepare_agent_host_profile;
use crate::zgent::client::{
    ZgentInstallation, ZgentRpcClient, ZgentServerLaunchConfig, ZgentSessionCreateResult,
    ZgentSessionSummary, ZgentSharedRpcClient,
};
use crate::zgent::context::{ZgentContextBridge, ZgentConversationSnapshot};
use crate::zgent::subagent::prepare_subagent_models_file;
use crate::zgent::tools::plan_agent_host_tool_injection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentKernelSpec {
    pub workspace_root: PathBuf,
    pub runtime_state_root: PathBuf,
    pub backend: AgentBackendKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentKernelRuntimeSpec {
    pub workspace_root: PathBuf,
    pub runtime_state_root: PathBuf,
    pub model: String,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug)]
pub struct ZgentKernelSession {
    client: ZgentRpcClient,
    remote_session: ZgentSessionCreateResult,
    conversation_hash: Option<String>,
    _app_profile: Option<ZgentAppProfileBundle>,
}

pub struct PersistentZgentKernelSession {
    client: ZgentSharedRpcClient,
    remote_session: ZgentSessionCreateResult,
    conversation_hash: Arc<Mutex<Option<String>>>,
    _app_profile: Option<ZgentAppProfileBundle>,
}

impl ZgentKernelSpec {
    pub fn new(workspace_root: PathBuf, runtime_state_root: PathBuf) -> Self {
        Self {
            workspace_root,
            runtime_state_root,
            backend: AgentBackendKind::Zgent,
        }
    }
}

impl ZgentKernelRuntimeSpec {
    pub fn from_frame_config(config: &FrameAgentConfig) -> Self {
        Self {
            workspace_root: config.workspace_root.clone(),
            runtime_state_root: config.runtime_state_root.clone(),
            model: config.upstream.model.clone(),
            api_base: Some(config.upstream.base_url.clone()),
            api_key: config
                .upstream
                .api_key
                .clone()
                .or_else(|| std::env::var(&config.upstream.api_key_env).ok()),
        }
    }

    pub fn launch_config(&self, subagent_models_path: Option<PathBuf>) -> ZgentServerLaunchConfig {
        ZgentServerLaunchConfig {
            workspace_root: Some(self.workspace_root.clone()),
            data_root: Some(self.runtime_state_root.join("zgent-server")),
            model: Some(self.model.clone()),
            api_base: self.api_base.clone(),
            api_key: self.api_key.clone(),
            subagent_models_path,
            no_persist: false,
        }
    }
}

impl ZgentKernelSession {
    pub fn spawn(
        runtime: &ZgentKernelRuntimeSpec,
        extra_tools: &[Tool],
        options: &BackendExecutionOptions,
    ) -> Result<Self> {
        let installation = ZgentInstallation::detect()?;
        let server_root = runtime.runtime_state_root.join("zgent-server");
        fs::create_dir_all(&server_root)
            .with_context(|| format!("failed to create {}", server_root.display()))?;
        let subagent_models_path =
            prepare_subagent_models_file(&server_root, &options.zgent_allowed_subagent_models)?;
        let launch = runtime.launch_config(subagent_models_path);
        let forwarded_extra_tools = match plan_agent_host_tool_injection(&installation, extra_tools)
        {
            Ok(plan) => {
                if !plan.shadowed_native_tool_names.is_empty() {
                    warn!(
                        native_tools = ?plan.shadowed_native_tool_names,
                        "skipping AgentHost bridge injection for tools already provided by zgent native kernel"
                    );
                }
                plan.forwarded_tools
            }
            Err(error) => {
                warn!(
                    error = %error,
                    "failed to inspect zgent native tool catalog; forwarding all AgentHost extra tools to bridge"
                );
                extra_tools.to_vec()
            }
        };
        let app_profile = prepare_agent_host_profile(&server_root, &forwarded_extra_tools)?;
        let session_profile = app_profile
            .as_ref()
            .map(|bundle| bundle.profile_name().to_string());

        let mut client = ZgentRpcClient::spawn_stdio(&installation, &launch)?;
        if let Some(profile_name) = session_profile.as_deref() {
            let discovered = client
                .profile_list()
                .context("failed to list zgent profiles before session/create")?;
            if !discovered
                .iter()
                .any(|profile| profile.name == profile_name)
            {
                return Err(anyhow!(
                    "zgent did not discover generated AgentHost profile: {}",
                    profile_name
                ));
            }
            let _profile = client
                .profile_get(profile_name)
                .context("failed to fetch generated AgentHost profile metadata")?;
        }
        let remote_session =
            client.session_create(Some("AgentHost ZGent Kernel"), session_profile.as_deref())?;
        Ok(Self {
            client,
            remote_session,
            conversation_hash: None,
            _app_profile: app_profile,
        })
    }

    pub fn remote_session_id(&self) -> &str {
        &self.remote_session.session_id
    }

    pub fn remote_workspace_path(&self) -> &str {
        &self.remote_session.workspace_path
    }

    pub fn synchronize_conversation(&mut self, messages: &[ChatMessage]) -> Result<String> {
        let snapshot = ZgentConversationSnapshot {
            messages: host_messages_to_zgent_conversation(messages),
            hash: String::new(),
        };
        let session_id = self.remote_session.session_id.clone();
        let if_hash = self.conversation_hash.clone();
        let new_hash = self
            .client
            .set_conversation(&session_id, &snapshot, if_hash.as_deref())?;
        self.conversation_hash = Some(new_hash.clone());
        Ok(new_hash)
    }

    pub fn fetch_conversation(&mut self) -> Result<ZgentConversationSnapshot> {
        let session_id = self.remote_session.session_id.clone();
        let snapshot = self.client.get_conversation(&session_id)?;
        self.conversation_hash = Some(snapshot.hash.clone());
        Ok(snapshot)
    }

    pub fn run_immediate_turn(
        &mut self,
        previous_messages: &[ChatMessage],
        prompt: &str,
    ) -> Result<Vec<ChatMessage>> {
        self.synchronize_conversation(previous_messages)?;
        let session_id = self.remote_session.session_id.clone();
        self.client
            .chat_send_immediate(&session_id, prompt)
            .context("zgent chat/send immediate failed")?;
        let snapshot = self.fetch_conversation()?;
        zgent_conversation_to_host_messages(&snapshot.messages)
    }
}

impl PersistentZgentKernelSession {
    pub fn spawn_or_attach(
        runtime: &ZgentKernelRuntimeSpec,
        extra_tools: &[Tool],
        options: &BackendExecutionOptions,
        existing_remote_session_id: Option<&str>,
    ) -> Result<Self> {
        let installation = ZgentInstallation::detect()?;
        let server_root = runtime.runtime_state_root.join("zgent-server");
        fs::create_dir_all(&server_root)
            .with_context(|| format!("failed to create {}", server_root.display()))?;
        let subagent_models_path =
            prepare_subagent_models_file(&server_root, &options.zgent_allowed_subagent_models)?;
        let launch = runtime.launch_config(subagent_models_path);
        let forwarded_extra_tools = match plan_agent_host_tool_injection(&installation, extra_tools)
        {
            Ok(plan) => {
                if !plan.shadowed_native_tool_names.is_empty() {
                    warn!(
                        native_tools = ?plan.shadowed_native_tool_names,
                        "skipping AgentHost bridge injection for tools already provided by zgent native kernel"
                    );
                }
                plan.forwarded_tools
            }
            Err(error) => {
                warn!(
                    error = %error,
                    "failed to inspect zgent native tool catalog; forwarding all AgentHost extra tools to bridge"
                );
                extra_tools.to_vec()
            }
        };
        let app_profile = prepare_agent_host_profile(&server_root, &forwarded_extra_tools)?;
        let session_profile = app_profile
            .as_ref()
            .map(|bundle| bundle.profile_name().to_string());

        let client = ZgentSharedRpcClient::spawn_stdio(&installation, &launch)?;
        if let Some(profile_name) = session_profile.as_deref() {
            let discovered = client
                .profile_list()
                .context("failed to list zgent profiles before session/create")?;
            if !discovered
                .iter()
                .any(|profile| profile.name == profile_name)
            {
                return Err(anyhow!(
                    "zgent did not discover generated AgentHost profile: {}",
                    profile_name
                ));
            }
            let _profile = client
                .profile_get(profile_name)
                .context("failed to fetch generated AgentHost profile metadata")?;
        }
        let remote_session = match existing_remote_session_id {
            Some(remote_session_id) => match client.session_get(remote_session_id) {
                Ok(summary) => summary_into_remote_session(summary),
                Err(error) => {
                    warn!(
                        remote_session_id,
                        error = %error,
                        "failed to reattach persisted zgent session; creating a new remote session"
                    );
                    client.session_create(
                        Some("AgentHost ZGent Kernel"),
                        session_profile.as_deref(),
                    )?
                }
            },
            None => {
                client.session_create(Some("AgentHost ZGent Kernel"), session_profile.as_deref())?
            }
        };
        Ok(Self {
            client,
            remote_session,
            conversation_hash: Arc::new(Mutex::new(None)),
            _app_profile: app_profile,
        })
    }

    pub fn remote_session_id(&self) -> &str {
        &self.remote_session.session_id
    }

    pub fn remote_workspace_path(&self) -> &str {
        &self.remote_session.workspace_path
    }

    pub fn synchronize_conversation(&self, messages: &[ChatMessage]) -> Result<String> {
        let snapshot = ZgentConversationSnapshot {
            messages: host_messages_to_zgent_conversation(messages),
            hash: String::new(),
        };
        let session_id = self.remote_session.session_id.clone();
        let if_hash = self
            .conversation_hash
            .lock()
            .map_err(|_| anyhow!("zgent conversation hash lock poisoned"))?
            .clone();
        let mut client = self.client.clone();
        let new_hash = client.set_conversation(&session_id, &snapshot, if_hash.as_deref())?;
        *self
            .conversation_hash
            .lock()
            .map_err(|_| anyhow!("zgent conversation hash lock poisoned"))? =
            Some(new_hash.clone());
        Ok(new_hash)
    }

    pub fn fetch_conversation(&self) -> Result<ZgentConversationSnapshot> {
        let session_id = self.remote_session.session_id.clone();
        let mut client = self.client.clone();
        let snapshot = client.get_conversation(&session_id)?;
        *self
            .conversation_hash
            .lock()
            .map_err(|_| anyhow!("zgent conversation hash lock poisoned"))? =
            Some(snapshot.hash.clone());
        Ok(snapshot)
    }

    pub fn fetch_session_summary(&self) -> Result<ZgentSessionSummary> {
        let client = self.client.clone();
        client.session_get(self.remote_session_id())
    }

    pub fn run_immediate_turn(
        &self,
        previous_messages: &[ChatMessage],
        prompt: &str,
    ) -> Result<Vec<ChatMessage>> {
        self.synchronize_conversation(previous_messages)?;
        self.client
            .chat_send_immediate(self.remote_session_id(), prompt)
            .context("zgent chat/send immediate failed")?;
        let snapshot = self.fetch_conversation()?;
        zgent_conversation_to_host_messages(&snapshot.messages)
    }

    pub fn send_steer(&self, prompt: &str) -> Result<()> {
        self.client
            .chat_send_steer(self.remote_session_id(), prompt)
            .context("zgent chat/send steer failed")?;
        Ok(())
    }

    pub fn send_queue(&self, prompt: &str) -> Result<()> {
        self.client
            .chat_send_queue(self.remote_session_id(), prompt)
            .context("zgent chat/send queue failed")?;
        Ok(())
    }
}

pub trait ZgentKernelAdapter {
    fn initialize_kernel(&self, spec: &ZgentKernelSpec) -> Result<()>;
}

fn summary_into_remote_session(summary: ZgentSessionSummary) -> ZgentSessionCreateResult {
    ZgentSessionCreateResult {
        session_id: summary.session_id,
        created_at: String::new(),
        workspace_path: summary.workspace_path,
    }
}

pub fn run_session_with_report_controlled(
    previous_messages: Vec<ChatMessage>,
    prompt: String,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
    options: BackendExecutionOptions,
) -> Result<SessionRunReport> {
    if control.is_some() {
        return Err(anyhow!(
            "the zgent backend now only supports the native kernel path, which does not integrate with AgentHost execution control; use the persistent native foreground path instead"
        ));
    }
    if !should_use_native_kernel_path(&extra_tools, None) {
        return Err(anyhow!(
            "the zgent backend now only supports the native kernel path, but no built zgent-server binary is available"
        ));
    }
    run_session_with_native_kernel(previous_messages, prompt, &config, &extra_tools, &options)
}

pub fn zgent_native_kernel_requested() -> bool {
    true
}

pub fn zgent_native_kernel_runtime_available() -> bool {
    let Ok(installation) = ZgentInstallation::detect() else {
        return false;
    };
    if installation.native_kernel_runtime_ready() {
        return true;
    }
    warn!(
        root = %installation.root_dir.display(),
        "zgent backend now requires a built zgent-server binary, but none was found"
    );
    false
}

fn should_use_native_kernel_path(
    _extra_tools: &[Tool],
    control: Option<&SessionExecutionControl>,
) -> bool {
    if control.is_some() {
        return false;
    }
    zgent_native_kernel_runtime_available()
}

fn run_session_with_native_kernel(
    previous_messages: Vec<ChatMessage>,
    prompt: String,
    config: &FrameAgentConfig,
    extra_tools: &[Tool],
    options: &BackendExecutionOptions,
) -> Result<SessionRunReport> {
    let runtime = ZgentKernelRuntimeSpec::from_frame_config(config);
    let mut session = ZgentKernelSession::spawn(&runtime, extra_tools, options)?;
    require_workspace_binding(session.remote_workspace_path(), &config.workspace_root)?;
    let messages = session.run_immediate_turn(&previous_messages, &prompt)?;
    Ok(SessionRunReport {
        messages,
        usage: agent_frame::TokenUsage::default(),
        compaction: agent_frame::SessionCompactionStats::default(),
        yielded: false,
        response_checkpoint: None,
    })
}

fn host_messages_to_zgent_conversation(messages: &[ChatMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(host_message_to_zgent_message)
            .collect::<Vec<_>>(),
    )
}

fn host_message_to_zgent_message(message: &ChatMessage) -> Value {
    match message.role.as_str() {
        "system" => json!({
            "role": "system",
            "content": content_as_string(&message.content),
        }),
        "user" => json!({
            "role": "user",
            "content": content_as_zgent_content(&message.content),
        }),
        "assistant" => {
            let mut object = Map::new();
            object.insert("role".to_string(), Value::String("assistant".to_string()));
            if let Some(content) = &message.content {
                object.insert("content".to_string(), content_as_string_value(content));
            }
            if let Some(tool_calls) = &message.tool_calls {
                object.insert(
                    "tool_calls".to_string(),
                    serde_json::to_value(tool_calls).unwrap_or(Value::Array(Vec::new())),
                );
            }
            Value::Object(object)
        }
        "tool" => {
            let mut object = Map::new();
            object.insert("role".to_string(), Value::String("tool".to_string()));
            object.insert(
                "content".to_string(),
                content_as_zgent_content(&message.content),
            );
            if let Some(tool_call_id) = &message.tool_call_id {
                object.insert(
                    "tool_call_id".to_string(),
                    Value::String(tool_call_id.clone()),
                );
            }
            Value::Object(object)
        }
        _ => json!({
            "role": "user",
            "content": content_as_zgent_content(&message.content),
        }),
    }
}

fn content_as_string(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.as_object())
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part.get("text").and_then(Value::as_str),
                Some("input_text") => part.get("text").and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(_)) => content.as_ref().map(Value::to_string).unwrap_or_default(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn content_as_string_value(content: &Value) -> Value {
    match content {
        Value::String(text) => Value::String(text.clone()),
        _ => Value::String(content_as_string(&Some(content.clone()))),
    }
}

fn content_as_zgent_content(content: &Option<Value>) -> Value {
    match content {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .map(normalize_content_part_for_zgent)
                .collect::<Vec<_>>(),
        ),
        Some(Value::Null) | None => Value::String(String::new()),
        Some(other) => Value::String(other.to_string()),
    }
}

fn normalize_content_part_for_zgent(part: &Value) -> Value {
    let Some(object) = part.as_object() else {
        return part.clone();
    };
    let Some(part_type) = object.get("type").and_then(Value::as_str) else {
        return part.clone();
    };

    match part_type {
        "input_text" => json!({
            "type": "text",
            "text": object.get("text").cloned().unwrap_or(Value::String(String::new())),
        }),
        "input_image" => {
            if let Some(image_url) = object.get("image_url").and_then(Value::as_str) {
                json!({
                    "type": "image_url",
                    "image_url": { "url": image_url },
                })
            } else if let Some(image_url) = object.get("image_url").and_then(Value::as_object) {
                json!({
                    "type": "image_url",
                    "image_url": image_url,
                })
            } else {
                part.clone()
            }
        }
        _ => part.clone(),
    }
}

pub fn workspace_matches_local_root(
    remote_workspace_path: &str,
    local_workspace_root: &Path,
) -> bool {
    let normalized_remote = Path::new(remote_workspace_path);
    normalized_remote == local_workspace_root
}

pub fn require_workspace_binding(
    remote_workspace_path: &str,
    local_workspace_root: &Path,
) -> Result<()> {
    if workspace_matches_local_root(remote_workspace_path, local_workspace_root) {
        return Ok(());
    }
    Err(anyhow!(
        "zgent session workspace mismatch: remote={} local={}",
        remote_workspace_path,
        local_workspace_root.display()
    ))
}

fn zgent_conversation_to_host_messages(messages: &Value) -> Result<Vec<ChatMessage>> {
    let items = messages
        .as_array()
        .ok_or_else(|| anyhow!("zgent conversation payload is not an array"))?;
    items.iter().map(zgent_message_to_host).collect()
}

fn zgent_message_to_host(message: &Value) -> Result<ChatMessage> {
    let object = message
        .as_object()
        .ok_or_else(|| anyhow!("zgent conversation message is not an object"))?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("zgent conversation message missing role"))?;
    match role {
        "system" => Ok(ChatMessage::text(
            "system",
            object
                .get("content")
                .map(content_as_string_value)
                .map(|value| value.as_str().unwrap_or_default().to_string())
                .unwrap_or_default(),
        )),
        "user" => Ok(ChatMessage {
            role: "user".to_string(),
            content: Some(zgent_content_to_host(
                object.get("content").unwrap_or(&Value::Null),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }),
        "assistant" => Ok(ChatMessage {
            role: "assistant".to_string(),
            content: object.get("content").map(content_as_string_value),
            name: None,
            tool_call_id: None,
            tool_calls: object
                .get("tool_calls")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .context("failed to parse zgent assistant tool_calls")?,
        }),
        "tool" => Ok(ChatMessage {
            role: "tool".to_string(),
            content: Some(zgent_content_to_host(
                object.get("content").unwrap_or(&Value::Null),
            )),
            name: None,
            tool_call_id: object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            tool_calls: None,
        }),
        other => Ok(ChatMessage::text(
            "user",
            format!(
                "[zgent role {}]\n{}",
                other,
                content_as_string(&object.get("content").cloned())
            ),
        )),
    }
}

fn zgent_content_to_host(content: &Value) -> Value {
    match content {
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .map(normalize_content_part_from_zgent)
                .collect::<Vec<_>>(),
        ),
        other => other.clone(),
    }
}

fn normalize_content_part_from_zgent(part: &Value) -> Value {
    let Some(object) = part.as_object() else {
        return part.clone();
    };
    let Some(part_type) = object.get("type").and_then(Value::as_str) else {
        return part.clone();
    };
    match part_type {
        "text" => json!({
            "type": "input_text",
            "text": object.get("text").cloned().unwrap_or(Value::String(String::new())),
        }),
        "image_url" => {
            if let Some(image_url) = object.get("image_url").and_then(Value::as_object) {
                if let Some(url) = image_url.get("url") {
                    json!({
                        "type": "input_image",
                        "image_url": url,
                    })
                } else {
                    part.clone()
                }
            } else {
                part.clone()
            }
        }
        _ => part.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ZgentKernelRuntimeSpec, content_as_string, host_message_to_zgent_message,
        should_use_native_kernel_path, workspace_matches_local_root,
        zgent_conversation_to_host_messages,
    };
    use crate::zgent::client::ZgentInstallation;
    use agent_frame::SessionExecutionControl;
    use agent_frame::Tool;
    use agent_frame::message::{ChatMessage, FunctionCall, ToolCall};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    #[test]
    fn runtime_spec_builds_local_server_launch_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let runtime = ZgentKernelRuntimeSpec {
            workspace_root: temp_dir.path().join("workspace"),
            runtime_state_root: temp_dir.path().join("runtime"),
            model: "test-model".to_string(),
            api_base: Some("https://example.invalid/v1".to_string()),
            api_key: Some("secret".to_string()),
        };

        let launch = runtime.launch_config(None);
        assert_eq!(launch.workspace_root, Some(runtime.workspace_root.clone()));
        assert_eq!(
            launch.data_root,
            Some(runtime.runtime_state_root.join("zgent-server"))
        );
        assert_eq!(launch.model.as_deref(), Some("test-model"));
        assert_eq!(
            launch.api_base.as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(launch.api_key.as_deref(), Some("secret"));
        assert!(!launch.no_persist);
    }

    #[test]
    fn converts_user_multimodal_content_to_zgent_shape() {
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let converted = host_message_to_zgent_message(&message);
        assert_eq!(converted["role"], "user");
        assert_eq!(converted["content"][0]["type"], "text");
        assert_eq!(converted["content"][0]["text"], "look");
        assert_eq!(converted["content"][1]["type"], "image_url");
        assert_eq!(
            converted["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn preserves_assistant_tool_calls_in_zgent_shape() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("working".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: Some("{\"path\":\"README.md\"}".to_string()),
                },
            }]),
        };

        let converted = host_message_to_zgent_message(&message);
        assert_eq!(converted["role"], "assistant");
        assert_eq!(converted["content"], "working");
        assert_eq!(converted["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn content_to_string_extracts_text_parts_only() {
        let value = json!([
            {"type": "input_text", "text": "hello"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
        ]);
        assert_eq!(content_as_string(&Some(value)), "hello");
    }

    #[test]
    fn workspace_matching_requires_same_root() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        assert!(workspace_matches_local_root(
            workspace.to_str().unwrap(),
            &workspace
        ));
        assert!(!workspace_matches_local_root("/different", &workspace));
    }

    #[test]
    fn converts_zgent_conversation_back_to_host_messages() {
        let conversation = json!([
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]
            },
            {
                "role": "assistant",
                "content": "done",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    }
                ]
            }
        ]);
        let messages = zgent_conversation_to_host_messages(&conversation).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].content,
            Some(json!([
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
            ]))
        );
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, Some(Value::String("done".to_string())));
        assert_eq!(
            messages[1]
                .tool_calls
                .as_ref()
                .and_then(|calls| calls.first())
                .map(|call| call.function.name.as_str()),
            Some("read_file")
        );
    }

    #[test]
    fn native_kernel_path_requires_runtime_and_no_execution_control() {
        let native_entry_available = ZgentInstallation::detect()
            .map(|installation| installation.server_entry().is_ok())
            .unwrap_or(false);
        assert_eq!(
            should_use_native_kernel_path(&[], None),
            native_entry_available
        );

        let extra_tools = vec![Tool::new(
            "user_tell",
            "Send progress to the user.",
            json!({"type":"object","properties":{}}),
            |_| Ok(json!({"ok": true})),
        )];
        assert_eq!(
            should_use_native_kernel_path(&extra_tools, None),
            native_entry_available
        );
        assert!(!should_use_native_kernel_path(
            &[],
            Some(&SessionExecutionControl::new())
        ));
    }
}
