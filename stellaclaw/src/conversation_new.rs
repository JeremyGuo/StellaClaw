#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap},
    fmt, fs,
    path::PathBuf,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stellaclaw_core::{model_config::ModelConfig, session_actor::ToolRemoteMode};

use crate::{
    config::{SandboxConfig, SessionDefaults, SessionProfile},
    conversation_metadata::ConversationMetadata,
    logger::append_workdir_level_log,
    service_protos::{
        agent_session::{AgentSessionBinding, AgentSessionKind},
        cron as cron_proto,
        kernel::{
            decode_request as decode_kernel_request, encode_response, KernelMetadataPatch,
            KernelRequest, KernelResponse, KernelRuntimeConfigPatch,
        },
        terminal as terminal_proto, workspace as workspace_proto,
    },
    services::{
        agent_session::AgentSessionService, channel::ChannelService, cron::CronService,
        memory::MemoryService, noop::NoopService, skill::SkillService, terminal::TerminalService,
        tool_binary::ToolBinaryService, workspace::WorkspaceService,
    },
};

pub use crate::services::agent_session::AgentSessionLaunchConfig;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", content = "conversation_id", rename_all = "snake_case")]
pub enum ServiceScope {
    Local,
    Conversation(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServiceAddr {
    pub scope: ServiceScope,
    pub path: Vec<String>,
}

impl ServiceAddr {
    pub fn local_path<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            scope: ServiceScope::Local,
            path: segments.into_iter().map(Into::into).collect(),
        }
    }

    pub fn conversation_path<I, S>(conversation_id: impl Into<String>, segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            scope: ServiceScope::Conversation(conversation_id.into()),
            path: segments.into_iter().map(Into::into).collect(),
        }
    }

    pub fn kernel() -> Self {
        Self::local_path(["kernel"])
    }

    pub fn channel() -> Self {
        Self::channel_id("main")
    }

    pub fn channel_id(id: impl Into<String>) -> Self {
        Self::local_path(["channel".to_string(), id.into()])
    }

    pub fn agent_foreground() -> Self {
        Self::agent_foreground_id("main")
    }

    pub fn agent_foreground_id(id: impl Into<String>) -> Self {
        Self::local_path(["agent".to_string(), "foreground".to_string(), id.into()])
    }

    pub fn agent_background(id: impl Into<String>) -> Self {
        Self::local_path(["agent".to_string(), "background".to_string(), id.into()])
    }

    pub fn agent_subagent(id: impl Into<String>) -> Self {
        Self::local_path(["agent".to_string(), "subagent".to_string(), id.into()])
    }

    pub fn cron() -> Self {
        Self::local_path(["cron"])
    }

    pub fn memory() -> Self {
        Self::local_path(["memory"])
    }

    pub fn skill() -> Self {
        Self::local_path(["skill"])
    }

    pub fn tool_binary() -> Self {
        Self::local_path(["tool_binary"])
    }

    pub fn workspace() -> Self {
        Self::local_path(["workspace"])
    }

    pub fn terminal() -> Self {
        Self::local_path(["terminal"])
    }

    pub fn control() -> Self {
        Self::local_path(["control"])
    }

    pub fn is_kernel(&self) -> bool {
        self.scope == ServiceScope::Local && self.path == ["kernel"]
    }

    pub fn local_service_id(&self, service_name: &str) -> Option<&str> {
        if self.scope == ServiceScope::Local
            && self.path.first().map(String::as_str) == Some(service_name)
        {
            self.path.get(1).map(String::as_str)
        } else {
            None
        }
    }

    fn storage_component(&self) -> String {
        let scope = match &self.scope {
            ServiceScope::Local => "local".to_string(),
            ServiceScope::Conversation(conversation_id) => {
                format!("conversation_{conversation_id}")
            }
        };
        let path = self.path.join("__");
        format!("{scope}__{path}")
    }
}

impl fmt::Display for ServiceAddr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.scope {
            ServiceScope::Local => write!(formatter, "local:{}", self.path.join("/")),
            ServiceScope::Conversation(conversation_id) => {
                write!(
                    formatter,
                    "conversation/{conversation_id}:{}",
                    self.path.join("/")
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCall {
    pub source: ServiceAddr,
    pub target: ServiceAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub payload: Value,
}

impl ServiceCall {
    pub fn new(source: ServiceAddr, target: ServiceAddr, payload: Value) -> Self {
        Self {
            source,
            target,
            request_id: None,
            response_id: None,
            payload,
        }
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn response_to(
        source: ServiceAddr,
        target: ServiceAddr,
        payload: Value,
        response_id: Option<String>,
    ) -> Self {
        Self {
            source,
            target,
            request_id: None,
            response_id,
            payload,
        }
    }

    pub fn response_to_call(source: ServiceAddr, request: &ServiceCall, payload: Value) -> Self {
        Self::response_to(
            source,
            request.source.clone(),
            payload,
            request.request_id.clone(),
        )
    }

    pub fn channel_target() -> ServiceAddr {
        ServiceAddr::channel()
    }

    pub fn channel_target_id(id: impl Into<String>) -> ServiceAddr {
        ServiceAddr::channel_id(id)
    }
}

#[derive(Debug)]
pub enum ServiceOutput {
    Call(ServiceCall),
    Status(ServiceStatusUpdate),
    Failed(ServiceFailure),
    Stopped(ServiceStopped),
}

#[derive(Debug)]
pub enum KernelInput {
    Shutdown { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatusUpdate {
    pub addr: ServiceAddr,
    pub label: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceFailure {
    pub addr: ServiceAddr,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStopped {
    pub addr: ServiceAddr,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceStop {
    pub reason: String,
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct ConversationRef {
    pub conversation_id: String,
    pub workdir: PathBuf,
    pub conversation_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ChannelServiceEndpoint {
    pub ingress_rx: Receiver<crate::service_protos::channel::ChannelIngress>,
    pub event_tx: Sender<crate::service_protos::channel::ChannelEvent>,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceRefs {
    channel_endpoints: HashMap<ServiceAddr, ChannelServiceEndpoint>,
}

impl ServiceRefs {
    pub fn with_channel_endpoint(
        mut self,
        addr: ServiceAddr,
        endpoint: ChannelServiceEndpoint,
    ) -> Self {
        self.channel_endpoints.insert(addr, endpoint);
        self
    }

    fn channel_endpoint(&self, addr: &ServiceAddr) -> Option<&ChannelServiceEndpoint> {
        self.channel_endpoints.get(addr)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_server_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_profile: Option<SessionProfile>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub session_defaults: SessionDefaults,
    #[serde(default)]
    pub memory_enabled: bool,
    #[serde(default)]
    pub tool_remote_mode: ToolRemoteMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_compact_enabled: Option<bool>,
}

impl ConversationRuntimeConfig {
    pub fn for_conversation(_conversation: &ConversationRef) -> Self {
        Self {
            agent_server_path: None,
            session_profile: None,
            models: BTreeMap::new(),
            session_defaults: SessionDefaults::default(),
            memory_enabled: false,
            tool_remote_mode: ToolRemoteMode::Selectable,
            sandbox: None,
            reasoning_effort: None,
            idle_timeout_compact_enabled: None,
        }
    }

    fn merge_host_defaults(&mut self, defaults: ConversationRuntimeConfig) -> bool {
        let mut changed = false;
        if self.agent_server_path.is_none() && defaults.agent_server_path.is_some() {
            self.agent_server_path = defaults.agent_server_path;
            changed = true;
        }
        if self.models.is_empty() && !defaults.models.is_empty() {
            self.models = defaults.models;
            changed = true;
        }
        if !session_defaults_has_values(&self.session_defaults)
            && session_defaults_has_values(&defaults.session_defaults)
        {
            self.session_defaults = defaults.session_defaults;
            changed = true;
        }
        if !self.memory_enabled && defaults.memory_enabled {
            self.memory_enabled = true;
            changed = true;
        }
        if self.sandbox.is_none() && defaults.sandbox.is_some() {
            self.sandbox = defaults.sandbox;
            changed = true;
        }
        if self.idle_timeout_compact_enabled.is_none()
            && defaults.idle_timeout_compact_enabled.is_some()
        {
            self.idle_timeout_compact_enabled = defaults.idle_timeout_compact_enabled;
            changed = true;
        }
        changed
    }
}

fn session_defaults_has_values(defaults: &SessionDefaults) -> bool {
    defaults.compression_threshold_tokens.is_some()
        || defaults.compression_retain_recent_tokens.is_some()
        || defaults.image_tool_model.is_some()
        || defaults.pdf_tool_model.is_some()
        || defaults.audio_tool_model.is_some()
        || defaults.image_generation_tool_model.is_some()
        || defaults.search_tool_model.is_some()
        || defaults.search_image_tool_model.is_some()
        || defaults.search_video_tool_model.is_some()
        || defaults.search_news_tool_model.is_some()
}

pub struct ServiceRunContext {
    pub addr: ServiceAddr,
    pub conversation: ConversationRef,
    pub storage: PathBuf,
    pub refs: ServiceRefs,
    pub inbox: Receiver<ServiceCall>,
    pub outbox: Sender<ServiceOutput>,
    pub stop_rx: Receiver<ServiceStop>,
}

pub trait ConversationService: Send + 'static {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceKind {
    Channel,
    AgentSession {
        kind: AgentSessionKind,
        binding: AgentSessionBinding,
    },
    Cron,
    Memory,
    Skill,
    ToolBinary,
    Workspace,
    Terminal,
    Control,
    Noop {
        name: String,
    },
}

pub struct ServiceHandle {
    pub addr: ServiceAddr,
    pub kind: ServiceKind,
    pub storage: PathBuf,
    pub inbox_tx: Sender<ServiceCall>,
    pub stop_tx: Sender<ServiceStop>,
    join: JoinHandle<Result<()>>,
}

pub struct ConversationKernelHandle {
    pub input_tx: Sender<KernelInput>,
    join: JoinHandle<Result<()>>,
}

impl ConversationKernelHandle {
    pub fn shutdown(self, reason: impl Into<String>) -> Result<()> {
        let _ = self.input_tx.send(KernelInput::Shutdown {
            reason: reason.into(),
        });
        self.join
            .join()
            .map_err(|_| anyhow!("conversation kernel thread panicked"))?
    }
}

#[derive(Debug, Clone)]
pub struct ServiceSpawnRecord {
    pub addr: ServiceAddr,
    pub kind: ServiceKind,
    pub storage: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceManifest {
    pub version: u32,
    pub services: Vec<ServiceManifestEntry>,
    pub next_background_id: u64,
    pub next_subagent_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceManifestEntry {
    pub addr: ServiceAddr,
    pub kind: ServiceKind,
    pub storage: PathBuf,
}

pub struct ConversationKernel {
    conversation: ConversationRef,
    refs: ServiceRefs,
    metadata: ConversationMetadata,
    runtime_config: ConversationRuntimeConfig,
    output_tx: Sender<ServiceOutput>,
    output_rx: Receiver<ServiceOutput>,
    services: HashMap<ServiceAddr, ServiceHandle>,
    status_log: Vec<ServiceStatusUpdate>,
    spawn_log: Vec<ServiceSpawnRecord>,
    next_background_id: u64,
    next_subagent_id: u64,
}

impl ConversationKernel {
    pub fn new(conversation: ConversationRef, refs: ServiceRefs) -> Self {
        let (output_tx, output_rx) = crossbeam_channel::unbounded();
        Self {
            runtime_config: ConversationRuntimeConfig::for_conversation(&conversation),
            metadata: ConversationMetadata::new(&conversation.conversation_id, "", ""),
            conversation,
            refs,
            output_tx,
            output_rx,
            services: HashMap::new(),
            status_log: Vec::new(),
            spawn_log: Vec::new(),
            next_background_id: 1,
            next_subagent_id: 1,
        }
    }

    pub fn open(conversation: ConversationRef, refs: ServiceRefs) -> Result<Self> {
        let mut kernel = Self::new(conversation, refs);
        if kernel.metadata_path().is_file() {
            kernel.metadata = kernel.load_metadata()?;
        } else {
            kernel.persist_metadata()?;
        }
        if kernel.runtime_config_path().is_file() {
            kernel.runtime_config = kernel.load_runtime_config()?;
        }
        if !kernel.manifest_path().is_file() {
            return Ok(kernel);
        }
        let manifest = kernel.load_manifest()?;
        kernel.next_background_id = manifest.next_background_id.max(1);
        kernel.next_subagent_id = manifest.next_subagent_id.max(1);
        for entry in manifest.services {
            kernel.mount_service(entry.addr, entry.kind)?;
        }
        Ok(kernel)
    }

    pub fn open_or_bootstrap(
        conversation: ConversationRef,
        refs: ServiceRefs,
        default_runtime_config: ConversationRuntimeConfig,
    ) -> Result<Self> {
        let mut kernel = Self::new(conversation, refs);
        if kernel.metadata_path().is_file() {
            kernel.metadata = kernel.load_metadata()?;
        } else {
            kernel.persist_metadata()?;
        }
        if kernel.runtime_config_path().is_file() {
            kernel.runtime_config = kernel.load_runtime_config()?;
            if kernel
                .runtime_config
                .merge_host_defaults(default_runtime_config)
            {
                kernel.persist_runtime_config()?;
            }
        } else {
            kernel.runtime_config = default_runtime_config;
            kernel.persist_runtime_config()?;
        }
        if kernel.manifest_path().is_file() {
            let manifest = kernel.load_manifest()?;
            kernel.next_background_id = manifest.next_background_id.max(1);
            kernel.next_subagent_id = manifest.next_subagent_id.max(1);
            for entry in manifest.services {
                kernel.mount_service(entry.addr, entry.kind)?;
            }
        }
        kernel.mount_standard_services()?;
        Ok(kernel)
    }

    pub fn spawn(self) -> Result<ConversationKernelHandle> {
        let (input_tx, input_rx) = crossbeam_channel::unbounded();
        let join = thread::Builder::new()
            .name(format!(
                "conversation-kernel-{}",
                self.conversation.conversation_id
            ))
            .spawn(move || self.run(input_rx))
            .context("failed to spawn conversation kernel")?;
        Ok(ConversationKernelHandle { input_tx, join })
    }

    pub fn runtime_config(&self) -> &ConversationRuntimeConfig {
        &self.runtime_config
    }

    pub fn metadata(&self) -> &ConversationMetadata {
        &self.metadata
    }

    pub fn set_runtime_config(&mut self, runtime_config: ConversationRuntimeConfig) {
        self.runtime_config = runtime_config;
    }

    pub fn run(mut self, input_rx: Receiver<KernelInput>) -> Result<()> {
        let output_rx = self.output_rx.clone();
        loop {
            crossbeam_channel::select! {
                recv(input_rx) -> input => {
                    match input {
                        Ok(KernelInput::Shutdown { reason }) => {
                            self.stop_all(reason)?;
                            return Ok(());
                        }
                        Err(_) => {
                            self.stop_all("kernel input channel closed")?;
                            return Ok(());
                        }
                    }
                }
                recv(output_rx) -> output => {
                    match output {
                        Ok(output) => {
                            if let Err(error) = self.handle_output(output) {
                                self.log_kernel_error("conversation_kernel_failed", &error);
                                let _ = self.stop_all(format!("conversation kernel failed: {error:#}"));
                                return Err(error);
                            }
                        }
                        Err(_) => {
                            self.stop_all("service output channel closed")?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn log_kernel_error(&self, event: &str, error: &anyhow::Error) {
        let _ = append_workdir_level_log(
            &self.conversation.workdir,
            "error",
            event,
            serde_json::json!({
                "conversation_id": &self.conversation.conversation_id,
                "error": error.to_string(),
                "error_debug": format!("{error:#}"),
            }),
        );
    }

    pub fn mount_service(&mut self, addr: ServiceAddr, kind: ServiceKind) -> Result<()> {
        let service = self.build_service(&addr, &kind);
        self.mount_service_instance(addr, kind, service)
    }

    pub fn mount_service_instance(
        &mut self,
        addr: ServiceAddr,
        kind: ServiceKind,
        service: Box<dyn ConversationService>,
    ) -> Result<()> {
        if addr.is_kernel() {
            return Err(anyhow!("kernel is a reserved address"));
        }
        if self.services.contains_key(&addr) {
            return Err(anyhow!("service already mounted at {addr}"));
        }
        self.ensure_agent_session_nickname(&addr, &kind)?;

        let storage = self.service_storage(&addr);
        fs::create_dir_all(&storage)
            .with_context(|| format!("failed to create service storage {}", storage.display()))?;

        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let service_addr = addr.clone();
        let service_outbox = self.output_tx.clone();
        let ctx = ServiceRunContext {
            addr: addr.clone(),
            conversation: self.conversation.clone(),
            storage: storage.clone(),
            refs: self.refs.clone(),
            inbox: inbox_rx,
            outbox: self.output_tx.clone(),
            stop_rx,
        };
        let join = thread::Builder::new()
            .name(format!("conversation-service-{addr}"))
            .spawn(move || {
                if let Err(error) = service.run(ctx) {
                    let _ = service_outbox.send(ServiceOutput::Failed(ServiceFailure {
                        addr: service_addr,
                        error: format!("{error:#}"),
                    }));
                }
                Ok(())
            })
            .context("failed to spawn conversation service")?;

        self.spawn_log.push(ServiceSpawnRecord {
            addr: addr.clone(),
            kind: kind.clone(),
            storage: storage.clone(),
        });
        self.services.insert(
            addr.clone(),
            ServiceHandle {
                addr,
                kind,
                storage,
                inbox_tx,
                stop_tx,
                join,
            },
        );
        self.persist_manifest()?;
        Ok(())
    }

    fn ensure_agent_session_nickname(
        &mut self,
        addr: &ServiceAddr,
        kind: &ServiceKind,
    ) -> Result<()> {
        if !matches!(kind, ServiceKind::AgentSession { .. }) {
            return Ok(());
        }
        let key = addr.storage_component();
        if self.metadata.session_nicknames.contains_key(&key) {
            return Ok(());
        }
        self.metadata
            .session_nicknames
            .insert(key, default_session_nickname(addr));
        self.persist_metadata()
    }

    pub fn mount_standard_services(&mut self) -> Result<()> {
        let services = [
            (ServiceAddr::channel(), ServiceKind::Channel),
            (
                ServiceAddr::agent_foreground(),
                ServiceKind::AgentSession {
                    kind: AgentSessionKind::Foreground,
                    binding: AgentSessionBinding {
                        event_sink: ServiceAddr::channel(),
                        parent_addr: None,
                    },
                },
            ),
            (ServiceAddr::cron(), ServiceKind::Cron),
            (ServiceAddr::memory(), ServiceKind::Memory),
            (ServiceAddr::skill(), ServiceKind::Skill),
            (ServiceAddr::tool_binary(), ServiceKind::ToolBinary),
            (ServiceAddr::workspace(), ServiceKind::Workspace),
            (ServiceAddr::terminal(), ServiceKind::Terminal),
        ];
        for (addr, kind) in services {
            if !self.has_service(&addr) {
                self.mount_service(addr, kind)?;
            }
        }
        Ok(())
    }

    pub fn dispatch_call(&mut self, call: ServiceCall) -> Result<()> {
        if call.target.is_kernel() {
            return self.handle_kernel_call(call);
        }
        let Some(handle) = self.services.get(&call.target) else {
            return Err(anyhow!("unknown service target {}", call.target));
        };
        handle
            .inbox_tx
            .send(call)
            .map_err(|_| anyhow!("service inbox closed"))?;
        Ok(())
    }

    pub fn drain_outputs(&mut self) -> Result<()> {
        while let Ok(output) = self.output_rx.try_recv() {
            self.handle_output(output)?;
        }
        Ok(())
    }

    pub fn pump_for(&mut self, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            match self.output_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(output) => self.handle_output(output)?,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.drain_outputs()
    }

    pub fn stop_service(&mut self, addr: &ServiceAddr, reason: impl Into<String>) -> Result<()> {
        self.stop_service_internal(addr, reason.into(), true)
    }

    fn stop_service_internal(
        &mut self,
        addr: &ServiceAddr,
        reason: String,
        persist: bool,
    ) -> Result<()> {
        if persist
            && addr.local_service_id("agent").is_some()
            && self.services.contains_key(&ServiceAddr::cron())
        {
            self.dispatch_call(cron_proto::disable_tasks_for_owner_call(
                ServiceAddr::kernel(),
                ServiceAddr::cron(),
                addr.clone(),
                reason.clone(),
            )?)?;
        }
        let Some(handle) = self.services.remove(addr) else {
            return Ok(());
        };
        let _ = handle.stop_tx.send(ServiceStop {
            reason,
            deadline: Some(Instant::now() + Duration::from_secs(5)),
        });
        join_service(handle)?;
        if persist {
            self.persist_manifest()?;
        }
        Ok(())
    }

    pub fn stop_all(&mut self, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let addrs = self.services.keys().cloned().collect::<Vec<_>>();
        for addr in addrs {
            self.stop_service_internal(&addr, reason.clone(), false)?;
        }
        Ok(())
    }

    pub fn has_service(&self, addr: &ServiceAddr) -> bool {
        self.services.contains_key(addr)
    }

    pub fn service_addrs(&self) -> Vec<ServiceAddr> {
        let mut addrs = self.services.keys().cloned().collect::<Vec<_>>();
        addrs.sort_by_key(|addr| addr.to_string());
        addrs
    }

    pub fn status_log(&self) -> &[ServiceStatusUpdate] {
        &self.status_log
    }

    pub fn spawn_log(&self) -> &[ServiceSpawnRecord] {
        &self.spawn_log
    }

    pub fn load_manifest(&self) -> Result<ServiceManifest> {
        let path = self.manifest_path();
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn persist_manifest(&self) -> Result<()> {
        let path = self.manifest_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let content = serde_json::to_string_pretty(&self.current_manifest())
            .context("failed to encode service manifest")?;
        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn load_runtime_config(&self) -> Result<ConversationRuntimeConfig> {
        let path = self.runtime_config_path();
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn persist_runtime_config(&self) -> Result<()> {
        let path = self.runtime_config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let content = serde_json::to_string_pretty(&self.runtime_config)
            .context("failed to encode runtime config")?;
        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn load_metadata(&self) -> Result<ConversationMetadata> {
        let path = self.metadata_path();
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn persist_metadata(&self) -> Result<()> {
        let path = self.metadata_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let content = serde_json::to_string_pretty(&self.metadata)
            .context("failed to encode conversation metadata")?;
        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn current_manifest(&self) -> ServiceManifest {
        let mut services = self
            .services
            .values()
            .map(|handle| ServiceManifestEntry {
                addr: handle.addr.clone(),
                kind: handle.kind.clone(),
                storage: handle.storage.clone(),
            })
            .collect::<Vec<_>>();
        services.sort_by_key(|entry| entry.addr.to_string());
        ServiceManifest {
            version: 1,
            services,
            next_background_id: self.next_background_id,
            next_subagent_id: self.next_subagent_id,
        }
    }

    fn manifest_path(&self) -> PathBuf {
        self.conversation
            .workdir
            .join("services")
            .join(&self.conversation.conversation_id)
            .join("manifest.json")
    }

    fn runtime_config_path(&self) -> PathBuf {
        self.conversation
            .workdir
            .join("services")
            .join(&self.conversation.conversation_id)
            .join("runtime_config.json")
    }

    fn metadata_path(&self) -> PathBuf {
        self.conversation
            .workdir
            .join("services")
            .join(&self.conversation.conversation_id)
            .join("conversation_metadata.json")
    }

    fn service_storage(&self, addr: &ServiceAddr) -> PathBuf {
        self.conversation
            .workdir
            .join("services")
            .join(&self.conversation.conversation_id)
            .join(addr.storage_component())
    }

    fn build_service(
        &self,
        addr: &ServiceAddr,
        kind: &ServiceKind,
    ) -> Box<dyn ConversationService> {
        match kind {
            ServiceKind::Channel => {
                if let Some(endpoint) = self.refs.channel_endpoint(addr) {
                    Box::new(ChannelService::with_platform_events(
                        endpoint.ingress_rx.clone(),
                        endpoint.event_tx.clone(),
                    ))
                } else {
                    Box::new(ChannelService::new())
                }
            }
            ServiceKind::AgentSession { kind, binding } => Box::new(AgentSessionService::new(
                kind.clone(),
                binding.clone(),
                self.agent_session_launch_config(addr),
            )),
            ServiceKind::Cron => Box::new(CronService::new()),
            ServiceKind::Memory => Box::new(MemoryService::new()),
            ServiceKind::Skill => Box::new(SkillService::new()),
            ServiceKind::ToolBinary => Box::new(ToolBinaryService::with_sandbox(
                self.runtime_config.sandbox.clone().unwrap_or_default(),
            )),
            ServiceKind::Workspace => Box::new(WorkspaceService::new(self.runtime_config.clone())),
            ServiceKind::Terminal => Box::new(TerminalService::new(self.runtime_config.clone())),
            ServiceKind::Control => Box::new(crate::services::control::ControlService::new()),
            ServiceKind::Noop { name } => Box::new(NoopService::new(name.clone())),
        }
    }

    fn agent_session_launch_config(&self, addr: &ServiceAddr) -> AgentSessionLaunchConfig {
        AgentSessionLaunchConfig {
            session_id: addr.storage_component(),
            conversation_root: self.conversation.conversation_root.clone(),
            workspace_root: self.agent_workspace_root(),
            agent_server_path: self.runtime_config.agent_server_path.clone(),
            session_profile: self.runtime_config.session_profile.clone(),
            models: self.runtime_config.models.clone(),
            session_defaults: self.runtime_config.session_defaults.clone(),
            memory_enabled: self.runtime_config.memory_enabled,
            tool_remote_mode: self.runtime_config.tool_remote_mode.clone(),
            sandbox: self.runtime_config.sandbox.clone(),
            reasoning_effort: self.runtime_config.reasoning_effort.clone(),
            idle_timeout_compact_enabled: self.runtime_config.idle_timeout_compact_enabled,
        }
    }

    fn agent_workspace_root(&self) -> PathBuf {
        match &self.runtime_config.tool_remote_mode {
            ToolRemoteMode::Selectable => self.conversation.conversation_root.clone(),
            ToolRemoteMode::FixedSsh { .. } => self.conversation.conversation_root.clone(),
        }
    }

    fn handle_output(&mut self, output: ServiceOutput) -> Result<()> {
        match output {
            ServiceOutput::Call(call) => self.dispatch_call(call),
            ServiceOutput::Status(status) => {
                self.status_log.push(status);
                Ok(())
            }
            ServiceOutput::Failed(failure) => Err(anyhow!(
                "service {} failed: {}",
                failure.addr,
                failure.error
            )),
            ServiceOutput::Stopped(stopped) => {
                if let Some(handle) = self.services.remove(&stopped.addr) {
                    join_service(handle)?;
                    self.persist_manifest()?;
                }
                self.status_log.push(ServiceStatusUpdate {
                    addr: stopped.addr,
                    label: "stopped".to_string(),
                    detail: serde_json::json!({"reason": stopped.reason}),
                });
                Ok(())
            }
        }
    }

    fn handle_kernel_call(&mut self, call: ServiceCall) -> Result<()> {
        let response_id = call.request_id.clone();
        match decode_kernel_request(call.payload) {
            Ok(KernelRequest::CreateAgentSession { kind, id, binding }) => {
                let source = call.source;
                let addr = match self.create_agent_session(&source, kind, id, binding) {
                    Ok(addr) => addr,
                    Err(error) => {
                        return self.reply_kernel_error(
                            source,
                            "permission_denied",
                            format!("source is not allowed to create this agent session: {error}"),
                            response_id,
                        );
                    }
                };
                self.dispatch_call(ServiceCall::response_to(
                    ServiceAddr::kernel(),
                    source,
                    encode_response(KernelResponse::AgentSessionCreated { addr })?,
                    response_id,
                ))
            }
            Ok(KernelRequest::StopService { addr, reason }) => {
                if !self.has_service(&addr) {
                    return self.dispatch_call(ServiceCall::response_to(
                        ServiceAddr::kernel(),
                        call.source,
                        encode_response(KernelResponse::Error {
                            code: "service_not_found".to_string(),
                            message: format!("service {addr} is not running"),
                        })?,
                        response_id,
                    ));
                }
                self.stop_service(
                    &addr,
                    reason.unwrap_or_else(|| "kernel stop request".to_string()),
                )?;
                self.dispatch_call(ServiceCall::response_to(
                    ServiceAddr::kernel(),
                    call.source,
                    encode_response(KernelResponse::ServiceStopped { addr })?,
                    response_id,
                ))
            }
            Ok(KernelRequest::QueryRuntimeConfig) => self.dispatch_call(ServiceCall::response_to(
                ServiceAddr::kernel(),
                call.source,
                encode_response(KernelResponse::RuntimeConfig {
                    config: self.runtime_config.clone(),
                })?,
                response_id,
            )),
            Ok(KernelRequest::UpdateRuntimeConfig { patch }) => {
                self.apply_runtime_config_patch(patch);
                self.persist_runtime_config()?;
                let updated_services = self.broadcast_runtime_config_to_services()?;
                self.dispatch_call(ServiceCall::response_to(
                    ServiceAddr::kernel(),
                    call.source,
                    encode_response(KernelResponse::RuntimeConfigUpdated {
                        config: self.runtime_config.clone(),
                        updated_services,
                    })?,
                    response_id,
                ))
            }
            Ok(KernelRequest::QueryMetadata) => self.dispatch_call(ServiceCall::response_to(
                ServiceAddr::kernel(),
                call.source,
                encode_response(KernelResponse::Metadata {
                    metadata: self.metadata.clone(),
                })?,
                response_id,
            )),
            Ok(KernelRequest::UpdateMetadata { patch }) => {
                self.apply_metadata_patch(patch);
                self.persist_metadata()?;
                self.dispatch_call(ServiceCall::response_to(
                    ServiceAddr::kernel(),
                    call.source,
                    encode_response(KernelResponse::MetadataUpdated {
                        metadata: self.metadata.clone(),
                    })?,
                    response_id,
                ))
            }
            Ok(KernelRequest::ListServices) => self.dispatch_call(ServiceCall::response_to(
                ServiceAddr::kernel(),
                call.source,
                encode_response(KernelResponse::Services {
                    addrs: self.service_addrs(),
                })?,
                response_id,
            )),
            Err(error) => self.reply_kernel_error(
                call.source,
                "bad_kernel_request",
                format!("kernel request was not understood: {error}"),
                response_id,
            ),
        }
    }

    fn reply_kernel_error(
        &mut self,
        target: ServiceAddr,
        code: impl Into<String>,
        message: impl Into<String>,
        response_id: Option<String>,
    ) -> Result<()> {
        self.dispatch_call(ServiceCall::response_to(
            ServiceAddr::kernel(),
            target,
            encode_response(KernelResponse::Error {
                code: code.into(),
                message: message.into(),
            })?,
            response_id,
        ))
    }

    fn apply_runtime_config_patch(&mut self, patch: KernelRuntimeConfigPatch) {
        if let Some(agent_server_path) = patch.agent_server_path {
            self.runtime_config.agent_server_path = agent_server_path;
        }
        if let Some(session_profile) = patch.session_profile {
            self.runtime_config.session_profile = session_profile;
        }
        if let Some(session_defaults) = patch.session_defaults {
            self.runtime_config.session_defaults = session_defaults;
        }
        if let Some(memory_enabled) = patch.memory_enabled {
            self.runtime_config.memory_enabled = memory_enabled;
        }
        if let Some(tool_remote_mode) = patch.tool_remote_mode {
            self.runtime_config.tool_remote_mode = tool_remote_mode;
        }
        if let Some(sandbox) = patch.sandbox {
            self.runtime_config.sandbox = sandbox;
        }
        if let Some(reasoning_effort) = patch.reasoning_effort {
            self.runtime_config.reasoning_effort = reasoning_effort;
        }
        if let Some(idle_timeout_compact_enabled) = patch.idle_timeout_compact_enabled {
            self.runtime_config.idle_timeout_compact_enabled = idle_timeout_compact_enabled;
        }
    }

    fn apply_metadata_patch(&mut self, patch: KernelMetadataPatch) {
        if let Some(nickname) = patch.conversation_nickname {
            let nickname = nickname.trim();
            self.metadata.nickname = if nickname.is_empty() {
                self.metadata.conversation_id.clone()
            } else {
                nickname.to_string()
            };
        }
        if let Some(model_selection_pending) = patch.model_selection_pending {
            self.metadata.model_selection_pending = model_selection_pending;
        }
        for (session_id, nickname) in patch.session_nicknames {
            match nickname {
                Some(nickname) => {
                    let nickname = nickname.trim();
                    if nickname.is_empty() {
                        self.metadata.session_nicknames.remove(&session_id);
                    } else {
                        self.metadata
                            .session_nicknames
                            .insert(session_id, nickname.to_string());
                    }
                }
                None => {
                    self.metadata.session_nicknames.remove(&session_id);
                }
            }
        }
    }

    fn broadcast_runtime_config_to_services(&mut self) -> Result<Vec<ServiceAddr>> {
        enum RuntimeConfigTarget {
            AgentSession,
            Workspace,
            Terminal,
        }

        let addrs = self
            .services
            .iter()
            .filter_map(|(addr, handle)| match &handle.kind {
                ServiceKind::AgentSession { .. } => {
                    Some((addr.clone(), RuntimeConfigTarget::AgentSession))
                }
                ServiceKind::Workspace => Some((addr.clone(), RuntimeConfigTarget::Workspace)),
                ServiceKind::Terminal => Some((addr.clone(), RuntimeConfigTarget::Terminal)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut updated_services = Vec::with_capacity(addrs.len());
        for (addr, target) in addrs {
            let call = match target {
                RuntimeConfigTarget::AgentSession => {
                    let launch = self.agent_session_launch_config(&addr);
                    crate::service_protos::agent_session::update_launch_config_call(
                        ServiceAddr::kernel(),
                        addr.clone(),
                        launch,
                    )?
                }
                RuntimeConfigTarget::Workspace => workspace_proto::update_runtime_config_call(
                    ServiceAddr::kernel(),
                    addr.clone(),
                    self.runtime_config.clone(),
                )?,
                RuntimeConfigTarget::Terminal => terminal_proto::update_runtime_config_call(
                    ServiceAddr::kernel(),
                    addr.clone(),
                    self.runtime_config.clone(),
                )?,
            };
            self.dispatch_call(call)?;
            updated_services.push(addr);
        }
        Ok(updated_services)
    }

    fn create_agent_session(
        &mut self,
        source: &ServiceAddr,
        kind: AgentSessionKind,
        id: Option<String>,
        requested_binding: Option<AgentSessionBinding>,
    ) -> Result<ServiceAddr> {
        let binding = self.resolve_agent_session_binding(source, &kind, requested_binding)?;
        let addr = match kind {
            AgentSessionKind::Foreground => {
                let id = id
                    .or_else(|| source.local_service_id("channel").map(str::to_string))
                    .unwrap_or_else(|| "main".to_string());
                ServiceAddr::agent_foreground_id(id)
            }
            AgentSessionKind::Background => {
                let id = id.unwrap_or_else(|| {
                    let next = self.next_background_id;
                    self.next_background_id += 1;
                    format!("bg_{next}")
                });
                ServiceAddr::agent_background(id)
            }
            AgentSessionKind::Subagent => {
                let id = id.unwrap_or_else(|| {
                    let next = self.next_subagent_id;
                    self.next_subagent_id += 1;
                    format!("sa_{next}")
                });
                ServiceAddr::agent_subagent(id)
            }
        };
        self.mount_service(addr.clone(), ServiceKind::AgentSession { kind, binding })?;
        Ok(addr)
    }

    fn resolve_agent_session_binding(
        &self,
        source: &ServiceAddr,
        kind: &AgentSessionKind,
        requested: Option<AgentSessionBinding>,
    ) -> Result<AgentSessionBinding> {
        match kind {
            AgentSessionKind::Foreground => {
                if source.local_service_id("channel").is_none() {
                    return Err(anyhow!("foreground sessions must be created by a channel"));
                }
                let binding = requested.unwrap_or_else(|| AgentSessionBinding {
                    event_sink: source.clone(),
                    parent_addr: None,
                });
                if binding.event_sink != *source {
                    return Err(anyhow!("foreground event sink must match creator channel"));
                }
                if binding.parent_addr.is_some() {
                    return Err(anyhow!("foreground sessions cannot have a parent session"));
                }
                Ok(binding)
            }
            AgentSessionKind::Background => {
                if *source == ServiceAddr::cron() {
                    return requested
                        .ok_or_else(|| anyhow!("background sessions require explicit binding"));
                }
                let Some(parent_binding) = self.agent_session_binding(source) else {
                    return Err(anyhow!(
                        "background sessions must be created by cron or an agent session"
                    ));
                };
                let binding = requested.unwrap_or_else(|| AgentSessionBinding {
                    event_sink: parent_binding.event_sink,
                    parent_addr: Some(source.clone()),
                });
                if binding.parent_addr.as_ref() != Some(source) {
                    return Err(anyhow!("background parent must match creator"));
                }
                Ok(binding)
            }
            AgentSessionKind::Subagent => {
                let Some(parent_binding) = self.agent_session_binding(source) else {
                    return Err(anyhow!("subagents must be created by an agent session"));
                };
                let binding = requested.unwrap_or_else(|| AgentSessionBinding {
                    event_sink: parent_binding.event_sink,
                    parent_addr: Some(source.clone()),
                });
                if binding.parent_addr.as_ref() != Some(source) {
                    return Err(anyhow!("subagent parent must match creator"));
                }
                Ok(binding)
            }
        }
    }

    fn agent_session_binding(&self, addr: &ServiceAddr) -> Option<AgentSessionBinding> {
        let handle = self.services.get(addr)?;
        match &handle.kind {
            ServiceKind::AgentSession { binding, .. } => Some(binding.clone()),
            _ => None,
        }
    }
}

fn default_session_nickname(addr: &ServiceAddr) -> String {
    match addr.path.as_slice() {
        [kind, _, id] if kind == "agent" => id.to_string(),
        _ => addr.to_string(),
    }
}

impl Drop for ConversationKernel {
    fn drop(&mut self) {
        let _ = self.stop_all("conversation kernel dropped");
    }
}

fn join_service(handle: ServiceHandle) -> Result<()> {
    handle
        .join
        .join()
        .map_err(|_| anyhow!("service {} panicked", handle.addr))?
}

#[cfg(test)]
mod tests {
    use std::{env, fs, thread, time::SystemTime};

    use super::*;
    use crate::{
        service_protos::{
            agent_session::{self, AgentSessionEvent},
            channel::{self, ChannelEvent, ChannelIngress},
            cron::{
                self, CronSchedule, CronTaskOutputPolicy, CronTaskPayload, CronTaskRegistration,
            },
            kernel,
            terminal::{TerminalRequest, TerminalResponse},
            workspace::{
                WorkspaceFileEncoding, WorkspaceRequest, WorkspaceResponse, WorkspaceTarget,
            },
        },
        services::channel::ChannelService,
    };
    use stellaclaw_core::session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem,
    };

    #[test]
    fn runtime_config_merges_missing_sandbox_from_host_defaults() {
        let current_conversation = conversation_ref("runtime_config_merge_sandbox");
        let defaults_conversation = conversation_ref("runtime_config_merge_sandbox_defaults");
        let mut current = ConversationRuntimeConfig::for_conversation(&current_conversation);
        let mut defaults = ConversationRuntimeConfig::for_conversation(&defaults_conversation);
        defaults.sandbox = Some(SandboxConfig {
            mode: crate::config::SandboxMode::Bubblewrap,
            software_mount_path: "/__software".to_string(),
            ..SandboxConfig::default()
        });

        assert!(current.merge_host_defaults(defaults));
        let sandbox = current.sandbox.expect("sandbox should merge");
        assert!(matches!(
            sandbox.mode,
            crate::config::SandboxMode::Bubblewrap
        ));
        assert_eq!(sandbox.software_mount_path, "/__software");
    }

    #[test]
    fn kernel_creates_dynamic_background_agent() {
        let mut kernel = test_kernel("create_background");
        kernel
            .mount_service(ServiceAddr::channel(), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");

        let call = kernel::create_agent_session_with_binding_call(
            ServiceAddr::cron(),
            AgentSessionKind::Background,
            Some("daily".to_string()),
            agent_session::AgentSessionBinding {
                event_sink: ServiceAddr::cron(),
                parent_addr: None,
            },
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("kernel handles call");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        assert!(kernel.has_service(&ServiceAddr::agent_background("daily")));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_can_create_named_foreground_agent() {
        let mut kernel = test_kernel("create_foreground");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");

        let call = kernel::create_agent_session_call(
            ServiceAddr::channel_id("scratch"),
            AgentSessionKind::Foreground,
            Some("scratch".to_string()),
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("kernel handles call");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        assert!(kernel.has_service(&ServiceAddr::agent_foreground_id("scratch")));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "agent_session_created"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn agent_session_output_goes_through_channel_service() {
        let mut kernel = test_kernel("agent_channel");
        kernel
            .mount_service(ServiceAddr::channel(), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground(),
                foreground_agent_kind("main"),
            )
            .expect("agent mounts");

        let call = agent_session::send_message_call(
            ServiceAddr::channel(),
            ServiceAddr::agent_foreground(),
            "hello",
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("agent receives call");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel()
                && status.label == "processing"
                && status.detail["active"] == serde_json::json!(false)
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_ingress_is_owned_by_channel_service() {
        let mut kernel = test_kernel("channel_ingress");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel(),
                ServiceKind::Channel,
                Box::new(ChannelService::with_ingress(ingress_rx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground(),
                foreground_agent_kind("main"),
            )
            .expect("agent mounts");

        ingress_tx
            .send(ChannelIngress::IncomingMessage {
                foreground_session_id: None,
                platform_message_id: Some("m_1".to_string()),
                origin: None,
                message: agent_session::text_message(ChatRole::User, "hello from platform"),
                metadata: serde_json::json!({}),
            })
            .expect("ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("outputs drain");

        assert!(kernel
            .status_log()
            .iter()
            .any(|status| status.addr == ServiceAddr::channel()
                && status.label == "incoming_message"));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel()
                && status.label == "processing"
                && status.detail["active"] == serde_json::json!(false)
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn conversation_can_have_multiple_foreground_sessions() {
        let mut kernel = test_kernel("multi_foreground");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_ingress(ingress_rx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground(),
                foreground_agent_kind("main"),
            )
            .expect("main foreground mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("scratch foreground mounts");

        ingress_tx
            .send(ChannelIngress::IncomingMessage {
                foreground_session_id: None,
                platform_message_id: Some("m_scratch".to_string()),
                origin: None,
                message: agent_session::text_message(ChatRole::User, "route to scratch"),
                metadata: serde_json::json!({}),
            })
            .expect("ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "message_received"
        }));
        assert!(!kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground() && status.label == "message_received"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_routes_structured_ingress_message_to_selected_session() {
        let mut kernel = test_kernel("structured_ingress_message");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_ingress(ingress_rx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        ingress_tx
            .send(ChannelIngress::IncomingMessage {
                foreground_session_id: None,
                platform_message_id: Some("m_structured".to_string()),
                origin: None,
                message: agent_session::text_message(ChatRole::User, "structured hello"),
                metadata: serde_json::json!({"source": "test"}),
            })
            .expect("ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "incoming_message"
                && status.detail["platform_message_id"] == "m_structured"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "message_received"
                && status.detail["origin"] == "user"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_routes_foreground_context_query_to_selected_session() {
        let mut kernel = test_kernel("foreground_context_query");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_ingress(ingress_rx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("scratch foreground mounts");

        ingress_tx
            .send(ChannelIngress::QueryForegroundContext {
                foreground_session_id: None,
                query_id: "q_ctx".to_string(),
                payload: serde_json::json!({"include_last_message": true}),
            })
            .expect("ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "context_queried"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "foreground_context"
                && status.detail["query_id"] == "q_ctx"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn kernel_injects_runtime_config_into_agent_session_launch() {
        let mut kernel = test_kernel("agent_launch_config");
        let mut runtime_config = ConversationRuntimeConfig::for_conversation(&kernel.conversation);
        runtime_config.memory_enabled = true;
        runtime_config.reasoning_effort = Some("high".to_string());
        runtime_config.session_defaults.compression_threshold_tokens = Some(64_000);
        runtime_config.idle_timeout_compact_enabled = Some(false);
        kernel.set_runtime_config(runtime_config);

        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        let call = agent_session::query_context_call(
            ServiceAddr::channel_id("scratch"),
            ServiceAddr::agent_foreground_id("scratch"),
            "launch",
            serde_json::json!({}),
        )
        .expect("query encodes");
        kernel.dispatch_call(call).expect("agent receives query");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("outputs drain");

        let context_status = kernel
            .status_log()
            .iter()
            .find(|status| {
                status.addr == ServiceAddr::channel_id("scratch")
                    && status.label == "foreground_context"
                    && status.detail["query_id"] == "launch"
            })
            .expect("context status exists");
        let metadata = &context_status.detail["context"]["metadata"];
        assert_eq!(metadata["session_id"], "local__agent__foreground__scratch");
        assert_eq!(
            metadata["workspace_root"],
            kernel.conversation.conversation_root.display().to_string()
        );
        assert_eq!(metadata["agent_server_configured"], false);
        assert_eq!(metadata["memory_enabled"], true);
        assert_eq!(metadata["reasoning_effort"], "high");
        assert_eq!(
            metadata["session_defaults"]["compression_threshold_tokens"],
            64_000
        );
        assert_eq!(metadata["idle_timeout_compact_enabled"], false);
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_projects_agent_session_message_events() {
        let mut kernel = test_kernel("session_event_message");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");

        let call = channel::session_event_call(
            ServiceAddr::agent_foreground_id("scratch"),
            ServiceAddr::channel_id("scratch"),
            AgentSessionEvent::MessageAppended {
                index: 7,
                message: agent_session::text_message(ChatRole::Assistant, "hello from session"),
            },
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("channel receives event");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "message_appended"
                && status.detail["index"] == 7
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_rejects_bad_payload_without_killing_kernel() {
        let mut kernel = test_kernel("bad_channel_payload");
        let workdir = kernel.conversation.workdir.clone();
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");

        kernel
            .dispatch_call(ServiceCall::new(
                ServiceAddr::local_path(["bad_payload_source"]),
                ServiceAddr::channel_id("scratch"),
                serde_json::json!({
                    "type": "not_a_channel_payload",
                    "debug": "should not kill kernel",
                }),
            ))
            .expect("bad payload reaches channel inbox");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("bad payload drains without fatal kernel error");

        assert!(kernel.has_service(&ServiceAddr::channel_id("scratch")));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "bad_channel_payload"
                && status.detail["source"]
                    == serde_json::json!(ServiceAddr::local_path(["bad_payload_source"]))
                && status.detail["payload"]["type"] == "not_a_channel_payload"
        }));
        let warn_log =
            fs::read_to_string(workdir.join("logs").join("warn.log")).expect("warn log exists");
        assert!(warn_log.contains("bad_channel_payload"));
        assert!(warn_log.contains("not_a_channel_payload"));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_ingress_controls_selected_foreground_session() {
        let mut kernel = test_kernel("channel_foreground_controls");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");
        ingress_tx
            .send(ChannelIngress::CreateForegroundSession { requested_id: None })
            .expect("create ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("create drains");
        assert!(kernel.has_service(&ServiceAddr::agent_foreground_id("scratch")));
        assert!(event_rx
            .try_iter()
            .any(|event| matches!(event, ChannelEvent::AgentSessionCreated { addr } if addr == ServiceAddr::agent_foreground_id("scratch"))));

        for ingress in [
            ChannelIngress::QueryForegroundStatus {
                foreground_session_id: None,
            },
            ChannelIngress::ContinueForegroundTurn {
                foreground_session_id: None,
                reason: Some("test continue".to_string()),
            },
            ChannelIngress::CompactForegroundNow {
                foreground_session_id: None,
            },
            ChannelIngress::ResolveHostCoordination {
                foreground_session_id: None,
                response: serde_json::json!({"approved": true}),
            },
            ChannelIngress::CancelForegroundTurn {
                foreground_session_id: None,
                reason: Some("test cancel".to_string()),
            },
        ] {
            ingress_tx.send(ingress).expect("control ingress sends");
        }
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("controls drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "agent_session_status"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "continue_requested"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch") && status.label == "compact_completed"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "host_coordination_resolved"
        }));

        ingress_tx
            .send(ChannelIngress::DeleteForegroundSession {
                foreground_session_id: None,
                reason: Some("test shutdown".to_string()),
            })
            .expect("shutdown ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("shutdown drains");
        assert!(!kernel.has_service(&ServiceAddr::agent_foreground_id("scratch")));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "agent_session_stopped"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_can_create_additional_foreground_session() {
        let mut kernel = test_kernel("channel_additional_foreground_session");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");

        ingress_tx
            .send(ChannelIngress::CreateForegroundSession {
                requested_id: Some("other".to_string()),
            })
            .expect("create ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("create drains");

        assert!(event_rx.try_iter().any(|event| {
            matches!(
                event,
                ChannelEvent::AgentSessionCreated { addr }
                    if addr == ServiceAddr::agent_foreground_id("other")
            )
        }));
        assert!(kernel.has_service(&ServiceAddr::agent_foreground_id("other")));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_ingress_updates_kernel_runtime_config() {
        let mut kernel = test_kernel("channel_runtime_config_ingress");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_ingress(ingress_rx)),
            )
            .expect("channel mounts");

        ingress_tx
            .send(ChannelIngress::UpdateRuntimeConfig {
                patch: kernel::KernelRuntimeConfigPatch {
                    memory_enabled: Some(true),
                    reasoning_effort: Some(Some("high".to_string())),
                    ..Default::default()
                },
            })
            .expect("runtime config ingress sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("runtime config drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "runtime_config_updated"
                && status.detail["memory_enabled"] == true
                && status.detail["updated_services"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
        }));
        assert!(kernel.runtime_config().memory_enabled);
        assert_eq!(
            kernel.runtime_config().reasoning_effort.as_deref(),
            Some("high")
        );
        assert!(kernel.runtime_config_path().is_file());
        let loaded = kernel
            .load_runtime_config()
            .expect("runtime config should reload");
        assert!(loaded.memory_enabled);
        assert_eq!(loaded.reasoning_effort.as_deref(), Some("high"));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_ingress_queries_kernel_runtime_config() {
        let mut kernel = test_kernel("channel_runtime_config_query");
        kernel.runtime_config.memory_enabled = true;
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");

        ingress_tx
            .send(ChannelIngress::QueryRuntimeConfig {
                request_id: "runtime-query-1".to_string(),
            })
            .expect("runtime config query sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("runtime config query drains");

        let event = event_rx
            .try_iter()
            .find(|event| matches!(event, ChannelEvent::KernelRuntimeConfig { .. }))
            .expect("runtime config event emitted");
        let ChannelEvent::KernelRuntimeConfig {
            request_id,
            response,
        } = event
        else {
            panic!("expected runtime config event");
        };
        assert_eq!(request_id, "runtime-query-1");
        let kernel::KernelResponse::RuntimeConfig { config } = response else {
            panic!("expected runtime config response");
        };
        assert!(config.memory_enabled);
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_ingress_projects_runtime_config_updates() {
        let mut kernel = test_kernel("channel_runtime_config_update_event");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");

        ingress_tx
            .send(ChannelIngress::UpdateRuntimeConfig {
                patch: kernel::KernelRuntimeConfigPatch {
                    memory_enabled: Some(true),
                    ..Default::default()
                },
            })
            .expect("runtime config update sends");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("runtime config update drains");

        let event = event_rx
            .try_iter()
            .find(|event| matches!(event, ChannelEvent::KernelRuntimeConfig { .. }))
            .expect("runtime config update event emitted");
        let ChannelEvent::KernelRuntimeConfig {
            request_id,
            response,
        } = event
        else {
            panic!("expected runtime config event");
        };
        assert!(request_id.is_empty());
        let kernel::KernelResponse::RuntimeConfigUpdated { config, .. } = response else {
            panic!("expected runtime config updated response");
        };
        assert!(config.memory_enabled);
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_forwards_workspace_requests_to_workspace_service() {
        let mut kernel = test_kernel("channel_workspace_forward");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::workspace(), ServiceKind::Workspace)
            .expect("workspace mounts");

        ingress_tx
            .send(ChannelIngress::Workspace {
                request_id: "write-1".to_string(),
                request: WorkspaceRequest::WriteFile {
                    path: ".stellaclaw/attachments/test.txt".to_string(),
                    target: WorkspaceTarget::LocalOverlay,
                    encoding: WorkspaceFileEncoding::Utf8,
                    data: "hello".to_string(),
                    create_parent_dirs: true,
                    overwrite: false,
                },
            })
            .expect("workspace ingress sends");
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("workspace request drains");

        assert!(event_rx.try_iter().any(|event| {
            matches!(
                event,
                ChannelEvent::Workspace {
                    request_id,
                    response: WorkspaceResponse::WriteCompleted { bytes_written: 5, .. },
                } if request_id == "write-1"
            )
        }));
        assert!(kernel
            .conversation
            .conversation_root
            .join(".stellaclaw/attachments/test.txt")
            .is_file());
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_forwards_workspace_list_responses_to_platform() {
        let mut kernel = test_kernel("channel_workspace_list_forward");
        fs::create_dir_all(&kernel.conversation.conversation_root).expect("create workspace root");
        fs::create_dir_all(kernel.conversation.conversation_root.join(".stellaclaw"))
            .expect("create overlay root");
        fs::write(
            kernel
                .conversation
                .conversation_root
                .join(".stellaclaw/visible.txt"),
            "hello",
        )
        .expect("write workspace file");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::workspace(), ServiceKind::Workspace)
            .expect("workspace mounts");

        ingress_tx
            .send(ChannelIngress::Workspace {
                request_id: "list-1".to_string(),
                request: WorkspaceRequest::List {
                    path: Some(".stellaclaw".to_string()),
                    target: WorkspaceTarget::LocalOverlay,
                    limit: None,
                },
            })
            .expect("workspace ingress sends");
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("workspace request drains");

        let events: Vec<_> = event_rx.try_iter().collect();
        assert!(
            events.iter().any(|event| {
                matches!(
                    event,
                    ChannelEvent::Workspace {
                        request_id,
                        response: WorkspaceResponse::Listing { entries, .. },
                    } if request_id == "list-1"
                        && entries.iter().any(|entry| entry.path == ".stellaclaw/visible.txt")
                )
            }),
            "events: {events:?}"
        );
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_forwards_terminal_requests_to_terminal_service() {
        let mut kernel = test_kernel("channel_terminal_forward");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::terminal(), ServiceKind::Terminal)
            .expect("terminal mounts");

        ingress_tx
            .send(ChannelIngress::Terminal {
                request_id: "term-list-1".to_string(),
                request: TerminalRequest::List,
            })
            .expect("terminal ingress sends");
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("terminal request drains");

        assert!(event_rx.try_iter().any(|event| {
            matches!(
                event,
                ChannelEvent::Terminal {
                    request_id: Some(request_id),
                    response: TerminalResponse::Terminals { terminals },
                } if request_id == "term-list-1" && terminals.is_empty()
            )
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_materializes_data_uri_files_before_forwarding_message() {
        let mut kernel = test_kernel("channel_materialize_incoming");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_ingress(ingress_rx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::workspace(), ServiceKind::Workspace)
            .expect("workspace mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        ingress_tx
            .send(ChannelIngress::IncomingMessage {
                foreground_session_id: None,
                platform_message_id: Some("platform-1".to_string()),
                origin: None,
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![
                        ChatMessageItem::Context(ContextItem {
                            text: "read this".to_string(),
                        }),
                        ChatMessageItem::File(FileItem {
                            uri: "data:text/plain;base64,aGVsbG8=".to_string(),
                            name: Some("note.txt".to_string()),
                            media_type: Some("text/plain".to_string()),
                            width: None,
                            height: None,
                            state: None,
                        }),
                    ],
                ),
                metadata: serde_json::json!({}),
            })
            .expect("incoming message sends");
        kernel
            .pump_for(Duration::from_millis(200))
            .expect("incoming message drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "incoming_message_materialized"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "message_received"
                && status.detail["ingress_id"] == "platform-1"
        }));
        let incoming_dir = kernel
            .conversation
            .conversation_root
            .join(".stellaclaw/attachments/incoming");
        let materialized = std::fs::read_dir(incoming_dir)
            .expect("incoming attachment dir exists")
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with("note.txt"));
        assert!(materialized);
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn kernel_runtime_config_update_reaches_existing_agent_sessions() {
        let mut kernel = test_kernel("runtime_config_broadcast");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        let call = kernel::update_runtime_config_call(
            ServiceAddr::channel_id("scratch"),
            kernel::KernelRuntimeConfigPatch {
                memory_enabled: Some(true),
                reasoning_effort: Some(Some("high".to_string())),
                ..Default::default()
            },
        )
        .expect("runtime config update encodes");
        kernel.dispatch_call(call).expect("kernel handles update");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("update drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "launch_config_applied"
                && status.detail["memory_enabled"] == true
                && status.detail["reasoning_effort"] == "high"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "runtime_config_updated"
                && status.detail["updated_services"]
                    .as_array()
                    .is_some_and(|sessions| sessions.len() == 1)
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn kernel_runtime_config_update_reaches_workspace_service() {
        let mut kernel = test_kernel("runtime_config_workspace_broadcast");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::workspace(), ServiceKind::Workspace)
            .expect("workspace mounts");

        let call = kernel::update_runtime_config_call(
            ServiceAddr::channel_id("scratch"),
            kernel::KernelRuntimeConfigPatch {
                tool_remote_mode: Some(ToolRemoteMode::FixedSsh {
                    host: "devbox".to_string(),
                    cwd: Some("/work/repo".to_string()),
                }),
                ..Default::default()
            },
        )
        .expect("runtime config update encodes");
        kernel.dispatch_call(call).expect("kernel handles update");
        kernel
            .pump_for(Duration::from_millis(100))
            .expect("update drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::workspace()
                && status.label == "runtime_config_updated"
                && status.detail["fixed_ssh"]["host"] == "devbox"
                && status.detail["fixed_ssh"]["cwd"] == "/work/repo"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "runtime_config_updated"
                && status.detail["updated_services"]
                    .as_array()
                    .is_some_and(|services| services.len() == 1)
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_emits_user_message_queued_events() {
        let mut kernel = test_kernel("channel_platform_events");
        let (ingress_tx, ingress_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        kernel
            .mount_service_instance(
                ServiceAddr::channel_id("scratch"),
                ServiceKind::Channel,
                Box::new(ChannelService::with_platform_events(ingress_rx, event_tx)),
            )
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                ServiceKind::Noop {
                    name: "foreground scratch".to_string(),
                },
            )
            .expect("noop agent mounts");

        ingress_tx
            .send(ChannelIngress::IncomingMessage {
                foreground_session_id: Some("scratch".to_string()),
                platform_message_id: Some("platform-message-1".to_string()),
                origin: Some(crate::service_protos::agent_session::AgentMessageOrigin::User),
                message: stellaclaw_core::session_actor::ChatMessage::new(
                    stellaclaw_core::session_actor::ChatRole::User,
                    vec![stellaclaw_core::session_actor::ChatMessageItem::Context(
                        stellaclaw_core::session_actor::ContextItem {
                            text: "hello platform".to_string(),
                        },
                    )],
                ),
                metadata: serde_json::json!({"client_message_id": "client-message-1"}),
            })
            .expect("ingress sends");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        let event = event_rx.try_recv().expect("platform event emitted");
        let ChannelEvent::UserMessageQueued {
            session_addr,
            platform_message_id,
            metadata,
            ..
        } = event
        else {
            panic!("expected user message queued event");
        };
        assert_eq!(session_addr, ServiceAddr::agent_foreground_id("scratch"));
        assert_eq!(platform_message_id.as_deref(), Some("platform-message-1"));
        assert_eq!(metadata["client_message_id"], "client-message-1");
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn channel_rejects_foreign_foreground_session_events() {
        let mut kernel = test_kernel("channel_foreign_foreground");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");

        let call = channel::session_event_call(
            ServiceAddr::agent_foreground(),
            ServiceAddr::channel_id("scratch"),
            AgentSessionEvent::MessageAppended {
                index: 1,
                message: agent_session::text_message(ChatRole::Assistant, "wrong channel"),
            },
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("channel receives event");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "error"
                && status.detail["code"] == "channel.foreign_session_event"
        }));
        assert!(!kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::channel_id("scratch")
                && status.label == "message_appended"
                && status.detail["index"] == 1
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_task_records_registering_session_and_channel() {
        let mut kernel = test_kernel("cron_task_binding");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        let call = cron::register_task_call(
            ServiceAddr::agent_foreground_id("scratch"),
            ServiceAddr::cron(),
            CronTaskRegistration {
                task_id: "daily".to_string(),
                registered_by: ServiceAddr::agent_foreground_id("ignored"),
                channel_addr: ServiceAddr::channel_id("scratch"),
                name: None,
                description: None,
                enabled: true,
                foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                schedule: CronSchedule::Manual,
                payload: prompt_payload("check"),
            },
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("cron receives call");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_registered"
                && status.detail["task_id"] == "daily"
                && status.detail["registered_by"]
                    == serde_json::json!(ServiceAddr::agent_foreground_id("scratch"))
                && status.detail["channel_addr"]
                    == serde_json::json!(ServiceAddr::channel_id("scratch"))
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn kernel_disables_cron_tasks_when_agent_session_is_deleted() {
        let mut kernel = test_kernel("cron_disable_on_session_delete");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    CronTaskRegistration {
                        task_id: "daily".to_string(),
                        registered_by: ServiceAddr::agent_foreground_id("scratch"),
                        channel_addr: ServiceAddr::channel_id("scratch"),
                        name: None,
                        description: None,
                        enabled: true,
                        foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                        schedule: CronSchedule::IntervalSeconds { seconds: 60.0 },
                        payload: prompt_payload("check"),
                    },
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("registration drains");

        kernel
            .stop_service(
                &ServiceAddr::agent_foreground_id("scratch"),
                "session deleted",
            )
            .expect("foreground stops");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("disable drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "owner_tasks_disabled"
                && status.detail["owner"]
                    == serde_json::json!(ServiceAddr::agent_foreground_id("scratch"))
                && status.detail["disabled"] == 1
        }));
        let state_path = kernel
            .service_storage(&ServiceAddr::cron())
            .join("tasks.json");
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state_path).expect("cron state exists"))
                .expect("cron state parses");
        assert_eq!(state["tasks"][0]["registration"]["enabled"], false);
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_rejects_invalid_task_registration() {
        let mut kernel = test_kernel("cron_reject_bad_registration");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    CronTaskRegistration {
                        task_id: "bad".to_string(),
                        registered_by: ServiceAddr::agent_foreground_id("scratch"),
                        channel_addr: ServiceAddr::channel_id("scratch"),
                        name: None,
                        description: None,
                        enabled: true,
                        foreground_session_addr: Some(ServiceAddr::agent_foreground_id("other")),
                        schedule: CronSchedule::Manual,
                        payload: prompt_payload("bad registration"),
                    },
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("registration drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_register_rejected"
                && status.detail["task_id"] == "bad"
        }));
        assert!(!kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_registered"
                && status.detail["task_id"] == "bad"
        }));

        kernel
            .dispatch_call(
                cron::trigger_task_now_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    "bad",
                )
                .expect("trigger call encodes"),
            )
            .expect("cron receives trigger");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("trigger drains");

        assert!(!kernel.has_service(&ServiceAddr::agent_background("cron_bad_1")));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_trigger_task_creates_background_and_dispatches_payload() {
        let mut kernel = test_kernel("cron_trigger_task");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        let task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload("check workspace"),
        };
        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    task,
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("registration drains");

        kernel
            .dispatch_call(
                cron::trigger_task_now_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    "daily",
                )
                .expect("trigger call encodes"),
            )
            .expect("cron receives trigger");
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("trigger drains");

        let background_addr = ServiceAddr::agent_background("cron_daily_1");
        assert!(kernel.has_service(&background_addr));
        let binding = kernel
            .agent_session_binding(&background_addr)
            .expect("background binding exists");
        assert_eq!(binding.event_sink, ServiceAddr::cron());
        assert_eq!(
            binding.parent_addr,
            Some(ServiceAddr::agent_foreground_id("scratch"))
        );
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_background_created"
                && status.detail["task_id"] == "daily"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_run_completed"
                && status.detail["task_id"] == "daily"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == background_addr
                && status.label == "message_received"
                && status.detail["origin"] == "system"
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "message_received"
                && status.detail["origin"] == "actor"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_manual_task_does_not_auto_trigger() {
        let mut kernel = test_kernel("cron_manual_no_auto");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    CronTaskRegistration {
                        task_id: "manual".to_string(),
                        registered_by: ServiceAddr::agent_foreground_id("scratch"),
                        channel_addr: ServiceAddr::channel_id("scratch"),
                        name: None,
                        description: None,
                        enabled: true,
                        foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                        schedule: CronSchedule::Manual,
                        payload: prompt_payload("manual only"),
                    },
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(80))
            .expect("outputs drain");

        assert!(!kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron() && status.label == "task_triggered"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_interval_task_auto_triggers() {
        let mut kernel = test_kernel("cron_interval_auto");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    CronTaskRegistration {
                        task_id: "fast".to_string(),
                        registered_by: ServiceAddr::agent_foreground_id("scratch"),
                        channel_addr: ServiceAddr::channel_id("scratch"),
                        name: None,
                        description: None,
                        enabled: true,
                        foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                        schedule: CronSchedule::IntervalSeconds { seconds: 0.02 },
                        payload: prompt_payload("auto run"),
                    },
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(180))
            .expect("outputs drain");

        let background_addr = ServiceAddr::agent_background("cron_fast_1");
        assert!(kernel.has_service(&background_addr));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_triggered"
                && status.detail["triggered_by"] == serde_json::json!(ServiceAddr::cron())
        }));
        assert!(kernel.status_log().iter().any(|status| {
            status.addr == background_addr
                && status.label == "message_received"
                && status.detail["origin"] == "system"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_store_only_task_records_result_without_forwarding() {
        let mut kernel = test_kernel("cron_store_only");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        let task = CronTaskRegistration {
            task_id: "store_only".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload_with_policy("store this", CronTaskOutputPolicy::StoreOnly),
        };
        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    task,
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("registration drains");

        kernel
            .dispatch_call(
                cron::trigger_task_now_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    "store_only",
                )
                .expect("trigger call encodes"),
            )
            .expect("cron receives trigger");
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("trigger drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_run_completed"
                && status.detail["task_id"] == "store_only"
        }));
        assert!(!kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::agent_foreground_id("scratch")
                && status.label == "message_received"
                && status.detail["origin"] == "actor"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn cron_marks_task_failed_when_background_creation_fails() {
        let mut kernel = test_kernel("cron_background_create_failed");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_background("cron_daily_1"),
                ServiceKind::Noop {
                    name: "occupied".to_string(),
                },
            )
            .expect("conflicting background address mounts");

        let task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload("check workspace"),
        };
        kernel
            .dispatch_call(
                cron::register_task_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    task,
                )
                .expect("register call encodes"),
            )
            .expect("cron receives register");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("registration drains");

        kernel
            .dispatch_call(
                cron::trigger_task_now_call(
                    ServiceAddr::agent_foreground_id("scratch"),
                    ServiceAddr::cron(),
                    "daily",
                )
                .expect("trigger call encodes"),
            )
            .expect("cron receives trigger");
        kernel
            .pump_for(Duration::from_millis(150))
            .expect("trigger drains");

        assert!(kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_run_failed"
                && status.detail["task_id"] == "daily"
                && status.detail["code"] == "kernel_create_agent_session_failed"
        }));
        assert!(!kernel.status_log().iter().any(|status| {
            status.addr == ServiceAddr::cron()
                && status.label == "task_background_created"
                && status.detail["task_id"] == "daily"
        }));
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn subagent_inherits_parent_channel_binding() {
        let mut kernel = test_kernel("subagent_binding");
        kernel
            .mount_service(ServiceAddr::channel_id("scratch"), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(
                ServiceAddr::agent_foreground_id("scratch"),
                foreground_agent_kind("scratch"),
            )
            .expect("foreground mounts");

        let call = kernel::create_agent_session_call(
            ServiceAddr::agent_foreground_id("scratch"),
            AgentSessionKind::Subagent,
            Some("research".to_string()),
        )
        .expect("call encodes");
        kernel.dispatch_call(call).expect("kernel handles call");
        kernel
            .pump_for(Duration::from_millis(50))
            .expect("outputs drain");

        let subagent = ServiceAddr::agent_subagent("research");
        let binding = kernel
            .agent_session_binding(&subagent)
            .expect("subagent binding exists");
        assert_eq!(binding.event_sink, ServiceAddr::channel_id("scratch"));
        assert_eq!(
            binding.parent_addr,
            Some(ServiceAddr::agent_foreground_id("scratch"))
        );
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn manifest_persists_and_restores_mounted_services() {
        let root = test_root("manifest_restore");
        let conversation = ConversationRef {
            conversation_id: "manifest_restore".to_string(),
            workdir: root.clone(),
            conversation_root: root,
        };

        {
            let mut kernel = ConversationKernel::new(conversation.clone(), ServiceRefs::default());
            kernel
                .mount_service(ServiceAddr::channel(), ServiceKind::Channel)
                .expect("channel mounts");
            kernel
                .mount_service(
                    ServiceAddr::agent_foreground(),
                    foreground_agent_kind("main"),
                )
                .expect("foreground mounts");
            let manifest = kernel.load_manifest().expect("manifest loads");
            assert_eq!(manifest.services.len(), 2);
        }

        let mut restored =
            ConversationKernel::open(conversation, ServiceRefs::default()).expect("opens");
        assert!(restored.has_service(&ServiceAddr::channel()));
        assert!(restored.has_service(&ServiceAddr::agent_foreground()));
        restored.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn standard_services_mount_with_stable_addresses() {
        let mut kernel = test_kernel("standard_services");
        kernel
            .mount_standard_services()
            .expect("standard services mount");

        for addr in [
            ServiceAddr::channel(),
            ServiceAddr::agent_foreground(),
            ServiceAddr::cron(),
            ServiceAddr::memory(),
            ServiceAddr::skill(),
            ServiceAddr::tool_binary(),
            ServiceAddr::workspace(),
            ServiceAddr::terminal(),
        ] {
            assert!(kernel.has_service(&addr), "{addr} should be mounted");
        }

        let manifest = kernel.load_manifest().expect("manifest loads");
        assert_eq!(manifest.services.len(), 8);
        assert_eq!(
            kernel
                .metadata()
                .session_nicknames
                .get(&ServiceAddr::agent_foreground().storage_component())
                .map(String::as_str),
            Some("Main")
        );
        kernel.stop_all("test finished").expect("services stop");
    }

    #[test]
    fn kernel_run_loop_stops_services_on_shutdown() {
        let root = test_root("run_loop");
        let conversation = ConversationRef {
            conversation_id: "run_loop".to_string(),
            workdir: root.clone(),
            conversation_root: root,
        };
        let mut kernel = ConversationKernel::new(conversation.clone(), ServiceRefs::default());
        kernel
            .mount_service(ServiceAddr::channel(), ServiceKind::Channel)
            .expect("channel mounts");
        kernel
            .mount_service(ServiceAddr::cron(), ServiceKind::Cron)
            .expect("cron mounts");
        let handle = kernel.spawn().expect("kernel spawns");
        handle.shutdown("test finished").expect("kernel shuts down");
    }

    #[test]
    fn kernel_run_loop_logs_fatal_service_failure() {
        let mut kernel = test_kernel("fatal_service_failure");
        let workdir = kernel.conversation.workdir.clone();
        kernel
            .mount_service_instance(
                ServiceAddr::local_path(["failing"]),
                ServiceKind::Noop {
                    name: "failing".to_string(),
                },
                Box::new(FailingService),
            )
            .expect("failing service mounts");

        let handle = kernel.spawn().expect("kernel spawns");
        thread::sleep(Duration::from_millis(50));
        let error = handle
            .shutdown("test finished")
            .expect_err("kernel should return the service failure");
        assert!(error.to_string().contains("service local:failing failed"));

        let error_log =
            fs::read_to_string(workdir.join("logs").join("error.log")).expect("error log exists");
        assert!(error_log.contains("conversation_kernel_failed"));
        assert!(error_log.contains("fatal_service_failure"));
        assert!(error_log.contains("service local:failing failed"));
    }

    struct FailingService;

    impl ConversationService for FailingService {
        fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
            ctx.outbox.send(ServiceOutput::Failed(ServiceFailure {
                addr: ctx.addr.clone(),
                error: "intentional failure".to_string(),
            }))?;
            Ok(())
        }
    }

    fn test_kernel(name: &str) -> ConversationKernel {
        ConversationKernel::new(conversation_ref(name), ServiceRefs::default())
    }

    fn conversation_ref(name: &str) -> ConversationRef {
        let root = test_root(name);
        ConversationRef {
            conversation_id: name.to_string(),
            workdir: root.clone(),
            conversation_root: root,
        }
    }

    fn foreground_agent_kind(id: &str) -> ServiceKind {
        ServiceKind::AgentSession {
            kind: AgentSessionKind::Foreground,
            binding: agent_session::AgentSessionBinding {
                event_sink: ServiceAddr::channel_id(id),
                parent_addr: None,
            },
        }
    }

    fn prompt_payload(prompt: &str) -> CronTaskPayload {
        prompt_payload_with_policy(prompt, CronTaskOutputPolicy::ForwardResultToForeground)
    }

    fn prompt_payload_with_policy(
        prompt: &str,
        output_policy: CronTaskOutputPolicy,
    ) -> CronTaskPayload {
        CronTaskPayload::Prompt {
            prompt: prompt.to_string(),
            output_policy,
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock works")
            .as_nanos();
        env::temp_dir().join(format!("stellaclaw-conversation-new-{name}-{unique}"))
    }
}
