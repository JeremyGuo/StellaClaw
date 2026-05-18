use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use jieba_rs::Jieba;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use stellaclaw_core::{
    model_config::ModelConfig,
    providers::{provider_from_model_config, send_provider_request_with_retry, ProviderRequest},
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem, TokenUsage},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    User,
    Public,
    Conversation,
}

impl MemoryScope {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "public" => Ok(Self::Public),
            "conversation" => Ok(Self::Conversation),
            other => Err(anyhow!("unsupported memory scope {other}")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Public => "public",
            Self::Conversation => "conversation",
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::User => "u",
            Self::Public => "p",
            Self::Conversation => "c",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        if id.starts_with("u_") {
            Some(Self::User)
        } else if id.starts_with("p_") {
            Some(Self::Public)
        } else if id.starts_with("c_") {
            Some(Self::Conversation)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemoryState {
    Active,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub scope: MemoryScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_session_type: Option<String>,
    state: MemoryState,
}

#[derive(Debug, Clone)]
pub struct MemorySource {
    pub conversation_id: String,
    pub agent_id: Option<String>,
    pub session_type: String,
}

pub fn shared_workdir_memory_client(workdir: PathBuf, options: MemoryOptions) -> MemoryClient {
    static CLIENTS: OnceLock<Mutex<BTreeMap<PathBuf, MemoryClient>>> = OnceLock::new();
    let clients = CLIENTS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut clients = clients.lock().expect("memory client registry poisoned");
    if let Some(client) = clients.get(&workdir) {
        return client.clone();
    }
    let client = WorkdirMemoryManager::start(workdir.clone(), options);
    clients.insert(workdir, client.clone());
    client
}

impl WorkdirMemoryManager {
    pub fn start(workdir: PathBuf, options: MemoryOptions) -> MemoryClient {
        let (tx, rx) = mpsc::channel::<MemoryClientCommand>();
        thread::Builder::new()
            .name("stellaclaw-memory-manager".to_string())
            .spawn(move || loop {
                match rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(MemoryClientCommand::Execute {
                        conversation_root,
                        source,
                        action,
                        response,
                    }) => {
                        let service = MemoryService::with_options(
                            workdir.clone(),
                            conversation_root,
                            source,
                            options.clone(),
                        );
                        let _ = service.maintain_user_memory();
                        let result = match action {
                            MemoryClientAction::Write(request) => service.write(request),
                            MemoryClientAction::Search(request) => service.search(request),
                            MemoryClientAction::Update(request) => service.update(request),
                            MemoryClientAction::Delete(request) => service.delete(request),
                        };
                        let _ = response.send(result);
                        let _ = service.maintain_user_memory();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let service = MemoryService::with_options(
                            workdir.clone(),
                            workdir.clone(),
                            MemorySource {
                                conversation_id: "workdir".to_string(),
                                agent_id: None,
                                session_type: "memory_manager".to_string(),
                            },
                            options.clone(),
                        );
                        let _ = service.maintain_user_memory();
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            })
            .expect("failed to spawn memory manager");
        MemoryClient { tx }
    }
}

impl MemoryClient {
    pub fn execute(
        &self,
        conversation_root: PathBuf,
        source: MemorySource,
        action: MemoryClientAction,
    ) -> Result<Value> {
        let (response_tx, response_rx) = mpsc::channel();
        self.tx
            .send(MemoryClientCommand::Execute {
                conversation_root,
                source,
                action,
                response: response_tx,
            })
            .map_err(|error| anyhow!("memory_manager_unavailable: {error}"))?;
        response_rx
            .recv()
            .map_err(|error| anyhow!("memory_manager_unavailable: {error}"))?
    }
}

pub struct MemoryService {
    workdir: PathBuf,
    conversation_root: PathBuf,
    source: MemorySource,
    options: MemoryOptions,
}

#[derive(Clone)]
pub struct MemoryClient {
    tx: mpsc::Sender<MemoryClientCommand>,
}

pub struct WorkdirMemoryManager;

enum MemoryClientCommand {
    Execute {
        conversation_root: PathBuf,
        source: MemorySource,
        action: MemoryClientAction,
        response: mpsc::Sender<Result<Value>>,
    },
}

pub enum MemoryClientAction {
    Write(MemoryWriteRequest),
    Search(MemorySearchRequest),
    Update(MemoryUpdateRequest),
    Delete(MemoryDeleteRequest),
}

#[allow(dead_code)]
pub trait MemoryBackend {
    fn write(&self, request: MemoryWriteRequest) -> Result<Value>;
    fn update(&self, request: MemoryUpdateRequest) -> Result<Value>;
    fn delete(&self, request: MemoryDeleteRequest) -> Result<Value>;
    fn search(&self, request: MemorySearchRequest) -> Result<Value>;
    fn prompt_context(&self, request: MemoryContextRequest) -> Result<MemoryPromptBlock>;
}

#[derive(Debug, Clone)]
pub struct MemoryOptions {
    pub write_candidate_limit: usize,
    pub tool_result_max_bytes: usize,
    pub user_soft_threshold_bytes: u64,
    pub user_hard_threshold_bytes: u64,
    pub user_retry_after_failed_hard_compaction_secs: u64,
    pub user_compaction_model: Option<ModelConfig>,
    pub dedupe_model: Option<ModelConfig>,
    pub user_soft_compaction_schedule: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryWriteRequest {
    pub scope: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemorySearchRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryUpdateRequest {
    pub memory_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryDeleteRequest {
    pub memory_id: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MemoryContextRequest {
    pub scope: MemoryScope,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct MemoryPromptBlock {
    pub scope: MemoryScope,
    pub text: String,
    pub entries_hash: String,
    pub rendered_size_bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct MemoryManifest {
    #[serde(default = "default_next_id")]
    next_id: u64,
    #[serde(default)]
    entries_hash: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendered_size_bytes: Option<u64>,
    #[serde(default)]
    last_updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserMemoryCompactionStatus {
    state: String,
    attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_retry_at: Option<String>,
    #[serde(default)]
    last_input_hash: String,
    #[serde(default)]
    last_output_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold_override_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_soft_compaction_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

#[derive(Debug)]
struct ScopeStore {
    scope: MemoryScope,
    root: PathBuf,
}

#[derive(Debug, Clone)]
struct MemoryCandidate {
    entry: MemoryEntry,
    score: f64,
}

#[derive(Debug)]
struct SearchDocument {
    index: usize,
    id: String,
    scope: MemoryScope,
    subject: Option<String>,
    aliases: Vec<String>,
    text: String,
    tags: Vec<String>,
    conversation_id: Option<String>,
    updated_at: String,
    entry: MemoryEntry,
    tokens: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SubjectCatalogItem {
    subject: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    entry_ids: Vec<String>,
    last_seen_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct UserMemoryCompactionOutput {
    #[serde(default)]
    entries: Vec<UserMemoryCompactionOutputEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct UserMemoryCompactionOutputEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    text: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
struct UserMemoryCompactionApplyReport {
    input_rendered_bytes: u64,
    output_rendered_bytes: u64,
    output_hash: String,
    active_entry_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserCompactionTrigger {
    Hard,
    Soft,
}

impl UserCompactionTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MemoryAction {
    Touch { id: String },
    Update { id: String, text: String },
    Delete { id: String },
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MemoryConsistencyDecision {
    decision: MemoryConsistencyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    actions: Vec<MemoryAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum MemoryConsistencyStatus {
    Success,
    Failure,
}

const ENTRY_TEXT_MAX_BYTES: usize = 1024;
const SUBJECT_MAX_CHARS: usize = 120;
const TAG_MAX_COUNT: usize = 8;
const TAG_MAX_CHARS: usize = 48;
const DEFAULT_WRITE_CANDIDATE_LIMIT: usize = 10;
const MAX_WRITE_CANDIDATE_LIMIT: usize = 10;
const SEARCH_DEFAULT_LIMIT: usize = 5;
const SEARCH_MAX_LIMIT: usize = 20;
const SEARCH_ENTRY_TEXT_MAX_BYTES: usize = 700;
const DEFAULT_SEARCH_TOTAL_TEXT_MAX_BYTES: usize = 4_096;
const MAX_SEARCH_TOTAL_TEXT_MAX_BYTES: usize = 12_000;
const MAX_ACTIVE_ENTRIES_PER_SCOPE: usize = 512;
const ENTRIES_JSONL_MAX_BYTES: usize = 2 * 1024 * 1024;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const BM25_TOP_K: usize = 50;
const DENSE_TOP_K: usize = 50;
const DENSE_VECTOR_DIM: usize = 256;
const RRF_K: f64 = 60.0;
const USER_MEMORY_COMPACTION_SYSTEM_PROMPT: &str = "\
You compact StellaClaw user memory. Return strict JSON only, with no Markdown.\n\
Keep durable collaboration preferences, general corrections, and reusable work habits.\n\
Remove duplicate, obsolete, one-off, or non-user-scope items. Do not drop useful facts just to shorten.\n\
Schema: {\"entries\":[{\"id\":\"u_1\",\"subject\":\"short optional subject\",\"text\":\"compact durable memory under 1KB\",\"tags\":[\"tag\"]}]}.\n\
Reuse an existing id when preserving or rewriting an existing entry. Omit id only for a truly new merged entry.";
const MEMORY_DEDUPE_SYSTEM_PROMPT: &str = "\
You judge StellaClaw memory writes. Return strict JSON only, with no Markdown.\n\
Given one draft entry and a short candidate list, output actions that keep memory consistent.\n\
Newer draft facts win over conflicting old candidates. Touch exact duplicates. Update partial conflicts or merged details. Delete obsolete or fully conflicting candidates. Insert only if the draft is truly new.\n\
Use only candidate ids. At most one insert action. If you cannot decide safely, return {\"decision\":\"failure\",\"reason\":\"short_reason\",\"actions\":[]}.\n\
Schema: {\"decision\":\"success\",\"reason\":null,\"actions\":[{\"type\":\"touch\",\"id\":\"u_1\"},{\"type\":\"update\",\"id\":\"u_1\",\"text\":\"...\"},{\"type\":\"delete\",\"id\":\"u_2\"},{\"type\":\"insert\"}]}";

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            write_candidate_limit: DEFAULT_WRITE_CANDIDATE_LIMIT,
            tool_result_max_bytes: DEFAULT_SEARCH_TOTAL_TEXT_MAX_BYTES,
            user_soft_threshold_bytes: 4096,
            user_hard_threshold_bytes: 8192,
            user_retry_after_failed_hard_compaction_secs: 21_600,
            user_compaction_model: None,
            dedupe_model: None,
            user_soft_compaction_schedule: "daily".to_string(),
        }
    }
}

impl MemoryOptions {
    pub fn normalized(mut self) -> Self {
        self.write_candidate_limit = self
            .write_candidate_limit
            .clamp(1, MAX_WRITE_CANDIDATE_LIMIT);
        self.tool_result_max_bytes = self
            .tool_result_max_bytes
            .clamp(512, MAX_SEARCH_TOTAL_TEXT_MAX_BYTES);
        self.user_soft_threshold_bytes = self.user_soft_threshold_bytes.max(512);
        self.user_hard_threshold_bytes = self
            .user_hard_threshold_bytes
            .max(self.user_soft_threshold_bytes);
        self.user_retry_after_failed_hard_compaction_secs =
            self.user_retry_after_failed_hard_compaction_secs.max(60);
        if self.user_soft_compaction_schedule.trim().is_empty() {
            self.user_soft_compaction_schedule = "daily".to_string();
        }
        self
    }
}

impl MemoryService {
    #[cfg(test)]
    pub fn new(workdir: PathBuf, conversation_root: PathBuf, source: MemorySource) -> Self {
        Self::with_options(workdir, conversation_root, source, MemoryOptions::default())
    }

    pub fn with_options(
        workdir: PathBuf,
        conversation_root: PathBuf,
        source: MemorySource,
        options: MemoryOptions,
    ) -> Self {
        Self {
            workdir,
            conversation_root,
            source,
            options: options.normalized(),
        }
    }

    pub fn write(&self, request: MemoryWriteRequest) -> Result<Value> {
        let scope = MemoryScope::parse(request.scope.as_str())?;
        let draft = self.validate_draft(scope, request)?;
        let store = self.store(scope);
        ensure_scope_dirs(&store)?;
        let mut entries = read_entries(&store)?;

        let exact_hash = entry_hash(&draft.subject, &draft.text, &draft.tags);
        if let Some(index) = entries.iter().position(|entry| {
            entry.state == MemoryState::Active
                && entry_hash(&entry.subject, &entry.text, &entry.tags) == exact_hash
        }) {
            let now = now_string();
            entries[index].updated_at = now.clone();
            entries[index].last_accessed_at = Some(now);
            write_entries_and_manifest(&store, &entries)?;
            self.refresh_user_compaction_status(&store, &entries)?;
            self.run_user_hard_compaction_if_pending(&store)?;
            self.write_audit(&store, "write", json!({"decision": "success", "actions": [MemoryAction::Touch { id: entries[index].id.clone() }]}))?;
            return Ok(json!({"status": "success"}));
        }

        let candidates = search_entries(
            entries.clone(),
            &format!(
                "{} {}",
                draft.subject.as_deref().unwrap_or_default(),
                draft.text
            ),
            self.options.write_candidate_limit,
            Some(scope),
        );
        let decision = match self.consistency_decision(&store, &draft, &candidates) {
            Ok(decision) => decision,
            Err(error) => {
                let reason = format!("dedupe_model_failed: {error}");
                self.write_audit(
                    &store,
                    "write",
                    json!({
                        "decision": "failure",
                        "reason": reason,
                        "candidate_ids": candidates.iter().map(|item| item.entry.id.clone()).collect::<Vec<_>>(),
                    }),
                )?;
                return Ok(json!({
                    "status": "failure",
                    "reason": reason,
                }));
            }
        };
        if matches!(decision.decision, MemoryConsistencyStatus::Failure) {
            let reason = decision
                .reason
                .clone()
                .unwrap_or_else(|| "memory_consistency_failure".to_string());
            self.write_audit(
                &store,
                "write",
                json!({
                    "decision": "failure",
                    "reason": reason,
                    "candidate_ids": candidates.iter().map(|item| item.entry.id.clone()).collect::<Vec<_>>(),
                }),
            )?;
            return Ok(json!({
                "status": "failure",
                "reason": reason,
            }));
        }
        validate_actions(&decision.actions, &candidates)?;
        apply_actions(&mut entries, &store, draft, &decision.actions)?;
        write_entries_and_manifest(&store, &entries)?;
        self.refresh_user_compaction_status(&store, &entries)?;
        self.run_user_hard_compaction_if_pending(&store)?;
        self.write_audit(
            &store,
            "write",
            json!({
                "decision": decision,
                "candidate_ids": candidates.iter().map(|item| item.entry.id.clone()).collect::<Vec<_>>(),
            }),
        )?;
        Ok(json!({"status": "success"}))
    }

    pub fn search(&self, request: MemorySearchRequest) -> Result<Value> {
        let limit = request
            .limit
            .unwrap_or(SEARCH_DEFAULT_LIMIT)
            .clamp(1, SEARCH_MAX_LIMIT);
        let mut scoped_entries = Vec::new();
        let scopes = search_scopes(&request.scopes)?;
        for scope in &scopes {
            scoped_entries.push((*scope, read_entries(&self.store(*scope))?));
        }
        let mut entries = Vec::new();
        for (_, scope_entries) in &scoped_entries {
            entries.extend(scope_entries.clone());
        }
        let hits = search_entries(entries, &request.query, SEARCH_MAX_LIMIT, None);
        let hits = dedupe_search_hits(hits);
        let (results, returned_ids, truncated) =
            render_search_results(hits, limit, self.options.tool_result_max_bytes);
        if !returned_ids.is_empty() {
            let now = now_string();
            for (scope, mut entries) in scoped_entries {
                let mut changed = false;
                for entry in &mut entries {
                    if returned_ids.contains(entry.id.as_str()) {
                        entry.last_accessed_at = Some(now.clone());
                        changed = true;
                    }
                }
                if changed {
                    write_entries_and_manifest(&self.store(scope), &entries)?;
                }
            }
        }
        Ok(json!({
            "status": "success",
            "results": results,
            "truncated": truncated,
        }))
    }

    pub fn update(&self, request: MemoryUpdateRequest) -> Result<Value> {
        validate_text(&request.text)?;
        let scope = MemoryScope::from_id(&request.memory_id)
            .ok_or_else(|| anyhow!("invalid memory id {}", request.memory_id))?;
        let store = self.store(scope);
        ensure_scope_dirs(&store)?;
        let mut entries = read_entries(&store)?;
        let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.id == request.memory_id && entry.state == MemoryState::Active)
        else {
            return Ok(json!({"status": "failure", "reason": "memory_not_found"}));
        };
        entry.text = request.text.trim().to_string();
        entry.updated_at = now_string();
        write_entries_and_manifest(&store, &entries)?;
        self.refresh_user_compaction_status(&store, &entries)?;
        self.run_user_hard_compaction_if_pending(&store)?;
        self.write_audit(
            &store,
            "update",
            json!({"id": request.memory_id, "status": "success"}),
        )?;
        Ok(json!({"status": "success"}))
    }
    pub fn delete(&self, request: MemoryDeleteRequest) -> Result<Value> {
        let scope = MemoryScope::from_id(&request.memory_id)
            .ok_or_else(|| anyhow!("invalid memory id {}", request.memory_id))?;
        let store = self.store(scope);
        ensure_scope_dirs(&store)?;
        let mut entries = read_entries(&store)?;
        let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.id == request.memory_id && entry.state == MemoryState::Active)
        else {
            return Ok(json!({"status": "failure", "reason": "memory_not_found"}));
        };
        entry.state = MemoryState::Deleted;
        entry.updated_at = now_string();
        write_entries_and_manifest(&store, &entries)?;
        self.refresh_user_compaction_status(&store, &entries)?;
        self.run_user_hard_compaction_if_pending(&store)?;
        self.write_audit(
            &store,
            "delete",
            json!({"id": request.memory_id, "status": "success"}),
        )?;
        Ok(json!({"status": "success"}))
    }

    pub fn maintain_user_memory(&self) -> Result<Value> {
        let store = self.store(MemoryScope::User);
        ensure_scope_dirs(&store)?;
        let entries = read_entries(&store)?;
        self.refresh_user_compaction_status(&store, &entries)?;
        let Some(mut status) = read_user_compaction_status(&store) else {
            return Ok(json!({"status": "success", "action": "none"}));
        };
        if status.state == "retry_waiting"
            && status
                .next_retry_at
                .as_deref()
                .and_then(parse_timestamp)
                .is_some_and(|retry_at| retry_at <= Utc::now())
        {
            status.state = "hard_pending".to_string();
            status.next_retry_at = None;
            status.updated_at = Some(now_string());
            write_user_compaction_status(&store, &status)?;
        }
        self.run_user_hard_compaction_if_pending(&store)?;
        let Some(status) = read_user_compaction_status(&store) else {
            return Ok(json!({"status": "success"}));
        };
        if status.state == "dirty" && self.user_soft_compaction_due(&status) {
            if let Err(error) =
                self.run_provider_user_compaction(&store, UserCompactionTrigger::Soft)
            {
                let status = record_user_soft_compaction_failure(&store, error.to_string())?;
                self.write_audit(
                    &store,
                    "user_compaction",
                    json!({
                        "status": "failure",
                        "mode": "provider_soft",
                        "reason": error.to_string(),
                        "last_soft_compaction_at": status.last_soft_compaction_at,
                    }),
                )?;
            }
        }
        Ok(json!({"status": "success"}))
    }

    #[allow(dead_code)]
    pub fn prompt_context(&self, request: MemoryContextRequest) -> Result<MemoryPromptBlock> {
        let store = self.store(request.scope);
        let entries = read_entries(&store)?;
        let active_entries = entries
            .iter()
            .filter(|entry| entry.state == MemoryState::Active)
            .collect::<Vec<_>>();
        let full_text = render_memory_prompt_entries(active_entries.iter().copied());
        let entries_hash = rendered_memory_hash(&full_text);
        let rendered_size_bytes = full_text.len();
        let (text, truncated) = truncate_prompt_block(full_text, request.max_bytes);
        Ok(MemoryPromptBlock {
            scope: request.scope,
            text,
            entries_hash,
            rendered_size_bytes,
            truncated,
        })
    }

    fn validate_draft(
        &self,
        scope: MemoryScope,
        request: MemoryWriteRequest,
    ) -> Result<MemoryEntry> {
        validate_text(&request.text)?;
        let subject = request
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, SUBJECT_MAX_CHARS));
        let mut tags = Vec::new();
        tags.extend(normalize_tags(request.tags));
        let now = now_string();
        Ok(MemoryEntry {
            id: String::new(),
            scope,
            subject,
            text: request.text.trim().to_string(),
            tags,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            source_conversation_id: Some(self.source.conversation_id.clone()),
            source_agent_id: self.source.agent_id.clone(),
            source_session_type: Some(self.source.session_type.clone()),
            state: MemoryState::Active,
        })
    }

    fn store(&self, scope: MemoryScope) -> ScopeStore {
        let root = match scope {
            MemoryScope::User => self.workdir.join("rundir").join("memory_v1").join("user"),
            MemoryScope::Public => self.workdir.join("rundir").join("memory_v1").join("public"),
            MemoryScope::Conversation => self
                .conversation_root
                .join(".stellaclaw")
                .join("memory_v1")
                .join("conversation"),
        };
        ScopeStore { scope, root }
    }

    fn write_audit(&self, store: &ScopeStore, event: &str, data: Value) -> Result<()> {
        ensure_scope_dirs(store)?;
        let path = store.root.join("audit.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let line = json!({
            "ts": now_string(),
            "event": event,
            "data": data,
        });
        writeln!(file, "{line}").with_context(|| format!("failed to write {}", path.display()))
    }

    fn refresh_user_compaction_status(
        &self,
        store: &ScopeStore,
        entries: &[MemoryEntry],
    ) -> Result<()> {
        refresh_user_compaction_status(store, entries, &self.options)
    }

    fn run_user_hard_compaction_if_pending(&self, store: &ScopeStore) -> Result<()> {
        if store.scope != MemoryScope::User {
            return Ok(());
        }
        let Some(status) = read_user_compaction_status(store) else {
            return Ok(());
        };
        if status.state != "hard_pending" {
            return Ok(());
        }

        if self.options.user_compaction_model.is_some() {
            if let Err(error) =
                self.run_provider_user_compaction(store, UserCompactionTrigger::Hard)
            {
                let status =
                    record_user_compaction_failure(store, &self.options, error.to_string())?;
                self.write_audit(
                    store,
                    "user_compaction",
                    json!({
                        "status": "failure",
                        "mode": "provider",
                        "reason": error.to_string(),
                        "attempts": status.attempts,
                        "next_retry_at": status.next_retry_at,
                    }),
                )?;
            }
            return Ok(());
        }

        for _ in 0..2 {
            let entries = read_entries(store)?;
            let active_entries = entries
                .iter()
                .filter(|entry| entry.state == MemoryState::Active)
                .collect::<Vec<_>>();
            let input_bytes = rendered_user_memory_bytes(active_entries.iter().copied()) as u64;
            let compacted = compact_user_memory_entries(entries);
            let output_entries = compacted
                .iter()
                .filter(|entry| entry.state == MemoryState::Active)
                .collect::<Vec<_>>();
            let output_bytes = rendered_user_memory_bytes(output_entries.iter().copied()) as u64;
            if output_bytes < input_bytes {
                let output_hash = rendered_user_memory_hash(output_entries.iter().copied());
                write_entries_and_manifest(store, &compacted)?;
                record_user_compaction_success(store, output_hash)?;
                self.write_audit(
                    store,
                    "user_compaction",
                    json!({
                        "status": "success",
                        "mode": "local_duplicate_filter",
                        "input_bytes": input_bytes,
                        "output_bytes": output_bytes,
                    }),
                )?;
                return Ok(());
            }

            let status =
                record_user_compaction_no_shrink(store, &self.options, input_bytes, output_bytes)?;
            self.write_audit(
                store,
                "user_compaction",
                json!({
                    "status": "no_shrink",
                    "mode": "local_duplicate_filter",
                    "input_bytes": input_bytes,
                    "output_bytes": output_bytes,
                    "attempts": status.attempts,
                }),
            )?;
            if status.state != "hard_pending" {
                break;
            }
        }
        Ok(())
    }

    fn consistency_decision(
        &self,
        store: &ScopeStore,
        draft: &MemoryEntry,
        candidates: &[MemoryCandidate],
    ) -> Result<MemoryConsistencyDecision> {
        let Some(model_config) = self.options.dedupe_model.clone() else {
            return Ok(local_consistency_decision(draft, candidates));
        };
        self.provider_consistency_decision(store, model_config, draft, candidates)
    }

    fn user_soft_compaction_due(&self, status: &UserMemoryCompactionStatus) -> bool {
        if self.options.user_compaction_model.as_ref().is_none() {
            return false;
        }
        if self.options.user_soft_compaction_schedule.trim() != "daily" {
            return false;
        }
        let today = today_string();
        status.last_soft_compaction_at.as_deref() != Some(today.as_str())
    }

    fn run_provider_user_compaction(
        &self,
        store: &ScopeStore,
        trigger: UserCompactionTrigger,
    ) -> Result<()> {
        let model_config = self
            .options
            .user_compaction_model
            .clone()
            .ok_or_else(|| anyhow!("user_compaction_model_unavailable"))?;
        let entries = read_entries(store)?;
        let active_entries = entries
            .iter()
            .filter(|entry| entry.state == MemoryState::Active)
            .collect::<Vec<_>>();
        let input_rendered_bytes =
            rendered_user_memory_bytes(active_entries.iter().copied()) as u64;
        let request_text = render_user_compaction_request(active_entries.iter().copied())?;
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem { text: request_text })],
        )];
        let provider = provider_from_model_config(model_config.clone());
        let response = send_provider_request_with_retry(
            provider.as_ref(),
            ProviderRequest::new(&messages)
                .with_system_prompt(Some(USER_MEMORY_COMPACTION_SYSTEM_PROMPT)),
            |_| {},
        )
        .map_err(|error| anyhow!("user_compaction_provider_failed: {error}"))?;
        let _ = self.write_provider_usage(
            store,
            "user_memory_compaction",
            Some(trigger.as_str()),
            &model_config,
            &response,
        );
        let response_text = chat_message_text(&response);
        let output = parse_user_compaction_output(&response_text)?;
        let compacted = build_user_compaction_entries(&self.source, store, &entries, output)?;
        let output_entries = compacted
            .iter()
            .filter(|entry| entry.state == MemoryState::Active)
            .collect::<Vec<_>>();
        let output_rendered_bytes =
            rendered_user_memory_bytes(output_entries.iter().copied()) as u64;
        if output_rendered_bytes >= input_rendered_bytes {
            let status = match trigger {
                UserCompactionTrigger::Hard => record_user_compaction_no_shrink(
                    store,
                    &self.options,
                    input_rendered_bytes,
                    output_rendered_bytes,
                )?,
                UserCompactionTrigger::Soft => record_user_soft_compaction_no_shrink(
                    store,
                    input_rendered_bytes,
                    output_rendered_bytes,
                )?,
            };
            self.write_audit(
                store,
                "user_compaction",
                json!({
                    "status": "no_shrink",
                    "mode": match trigger {
                        UserCompactionTrigger::Hard => "provider",
                        UserCompactionTrigger::Soft => "provider_soft",
                    },
                    "input_bytes": input_rendered_bytes,
                    "output_bytes": output_rendered_bytes,
                    "attempts": status.attempts,
                }),
            )?;
            return Ok(());
        }

        let output_hash = rendered_user_memory_hash(output_entries.iter().copied());
        write_entries_and_manifest(store, &compacted)?;
        record_user_compaction_success(store, output_hash)?;
        self.write_audit(
            store,
            "user_compaction",
            json!({
                "status": "success",
                "mode": match trigger {
                    UserCompactionTrigger::Hard => "provider",
                    UserCompactionTrigger::Soft => "provider_soft",
                },
                "input_bytes": input_rendered_bytes,
                "output_bytes": output_rendered_bytes,
                "active_entry_count": output_entries.len(),
            }),
        )
    }

    fn provider_consistency_decision(
        &self,
        store: &ScopeStore,
        model_config: ModelConfig,
        draft: &MemoryEntry,
        candidates: &[MemoryCandidate],
    ) -> Result<MemoryConsistencyDecision> {
        let request_text = render_consistency_decision_request(draft, candidates)?;
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem { text: request_text })],
        )];
        let provider = provider_from_model_config(model_config.clone());
        let response = send_provider_request_with_retry(
            provider.as_ref(),
            ProviderRequest::new(&messages).with_system_prompt(Some(MEMORY_DEDUPE_SYSTEM_PROMPT)),
            |_| {},
        )
        .map_err(|error| anyhow!("dedupe_provider_failed: {error}"))?;
        let _ = self.write_provider_usage(store, "memory_dedupe", None, &model_config, &response);
        parse_memory_consistency_decision(&chat_message_text(&response))
    }

    fn write_provider_usage(
        &self,
        store: &ScopeStore,
        kind: &str,
        trigger: Option<&str>,
        model_config: &ModelConfig,
        response: &ChatMessage,
    ) -> Result<()> {
        ensure_scope_dirs(store)?;
        let path = store.root.join("usage.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut record = json!({
            "ts": now_string(),
            "date": today_string(),
            "kind": kind,
            "scope": store.scope,
            "conversation_id": &self.source.conversation_id,
            "source_agent_id": self.source.agent_id.as_ref(),
            "source_session_type": &self.source.session_type,
            "provider_type": &model_config.provider_type,
            "model_name": &model_config.model_name,
        });
        if let Some(trigger) = trigger {
            record["trigger"] = Value::String(trigger.to_string());
        }
        if let Some(token_usage) = &response.token_usage {
            record["token_usage"] = token_usage_json(token_usage);
        }
        writeln!(file, "{record}").with_context(|| format!("failed to write {}", path.display()))
    }

    #[allow(dead_code)]
    fn apply_user_compaction_output(
        &self,
        output: UserMemoryCompactionOutput,
    ) -> Result<UserMemoryCompactionApplyReport> {
        let store = self.store(MemoryScope::User);
        ensure_scope_dirs(&store)?;
        let entries = read_entries(&store)?;
        let active_entries = entries
            .iter()
            .filter(|entry| entry.state == MemoryState::Active)
            .collect::<Vec<_>>();
        let input_rendered_bytes =
            rendered_user_memory_bytes(active_entries.iter().copied()) as u64;
        let compacted = build_user_compaction_entries(&self.source, &store, &entries, output)?;

        write_entries_and_manifest(&store, &compacted)?;
        let output_entries = compacted
            .iter()
            .filter(|entry| entry.state == MemoryState::Active)
            .collect::<Vec<_>>();
        let output_rendered_bytes =
            rendered_user_memory_bytes(output_entries.iter().copied()) as u64;
        let output_hash = rendered_user_memory_hash(output_entries.iter().copied());
        record_user_compaction_success(&store, output_hash.clone())?;
        self.write_audit(
            &store,
            "user_compaction",
            json!({
                "status": "success",
                "mode": "provider_output_apply",
                "input_bytes": input_rendered_bytes,
                "output_bytes": output_rendered_bytes,
                "active_entry_count": output_entries.len(),
            }),
        )?;
        Ok(UserMemoryCompactionApplyReport {
            input_rendered_bytes,
            output_rendered_bytes,
            output_hash,
            active_entry_count: output_entries.len(),
        })
    }
}

fn normalize_tags(raw_tags: Vec<String>) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen = BTreeSet::new();
    for tag in raw_tags.into_iter().take(TAG_MAX_COUNT) {
        let tag = truncate_chars(tag.trim(), TAG_MAX_CHARS);
        if tag.is_empty() || !seen.insert(normalize_for_hash(&tag)) {
            continue;
        }
        tags.push(tag);
    }
    tags
}

impl MemoryBackend for MemoryService {
    fn write(&self, request: MemoryWriteRequest) -> Result<Value> {
        MemoryService::write(self, request)
    }

    fn update(&self, request: MemoryUpdateRequest) -> Result<Value> {
        MemoryService::update(self, request)
    }

    fn delete(&self, request: MemoryDeleteRequest) -> Result<Value> {
        MemoryService::delete(self, request)
    }

    fn search(&self, request: MemorySearchRequest) -> Result<Value> {
        MemoryService::search(self, request)
    }

    fn prompt_context(&self, request: MemoryContextRequest) -> Result<MemoryPromptBlock> {
        MemoryService::prompt_context(self, request)
    }
}

fn search_scopes(raw: &[String]) -> Result<Vec<MemoryScope>> {
    if raw.is_empty() {
        return Ok(vec![MemoryScope::Conversation, MemoryScope::Public]);
    }
    let mut scopes = Vec::new();
    let mut seen = BTreeSet::new();
    for value in raw {
        let scope = MemoryScope::parse(value.trim())?;
        if !matches!(scope, MemoryScope::Conversation | MemoryScope::Public) {
            continue;
        }
        if seen.insert(scope) {
            scopes.push(scope);
        }
    }
    Ok(scopes)
}

fn validate_text(text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("memory text must not be empty"));
    }
    if text.len() > ENTRY_TEXT_MAX_BYTES {
        return Err(anyhow!("entry_too_large"));
    }
    Ok(())
}

fn ensure_scope_dirs(store: &ScopeStore) -> Result<()> {
    fs::create_dir_all(&store.root)
        .with_context(|| format!("failed to create {}", store.root.display()))?;
    if store.scope == MemoryScope::User {
        ensure_user_compaction_status(store)?;
    }
    Ok(())
}

fn read_entries(store: &ScopeStore) -> Result<Vec<MemoryEntry>> {
    let path = store.root.join("entries.jsonl");
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", path.display()))
        }
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        entries.push(
            serde_json::from_str(&line)
                .with_context(|| format!("failed to parse {}", path.display()))?,
        );
    }
    Ok(entries)
}

fn write_entries_and_manifest(store: &ScopeStore, entries: &[MemoryEntry]) -> Result<()> {
    ensure_scope_dirs(store)?;
    let path = store.root.join("entries.jsonl");
    let tmp = store.root.join("entries.jsonl.tmp");
    let persisted_entries = entries
        .iter()
        .filter(|entry| entry.state == MemoryState::Active)
        .collect::<Vec<_>>();
    if persisted_entries.len() > MAX_ACTIVE_ENTRIES_PER_SCOPE {
        return Err(anyhow!("memory_store_entry_limit"));
    }
    let mut raw = String::new();
    for entry in &persisted_entries {
        raw.push_str(&serde_json::to_string(entry)?);
        raw.push('\n');
    }
    if raw.len() > ENTRIES_JSONL_MAX_BYTES {
        return Err(anyhow!("memory_store_too_large"));
    }
    fs::write(&tmp, raw.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            tmp.display()
        )
    })?;

    let mut hasher = Sha1::new();
    hasher.update(raw.as_bytes());
    let next_id = entries
        .iter()
        .filter_map(|entry| parse_id_number(&entry.id))
        .max()
        .unwrap_or(0)
        + 1;
    let manifest = MemoryManifest {
        next_id,
        entries_hash: format!("{:x}", hasher.finalize()),
        size_bytes: raw.len() as u64,
        rendered_size_bytes: (store.scope == MemoryScope::User)
            .then(|| rendered_user_memory_bytes(persisted_entries.iter().copied()) as u64),
        last_updated_at: Some(now_string()),
    };
    let manifest_name = if store.scope == MemoryScope::User {
        "manifest.json"
    } else {
        "index.json"
    };
    fs::write(
        store.root.join(manifest_name),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    if store.scope == MemoryScope::Public {
        let active_entries = persisted_entries.into_iter().cloned().collect::<Vec<_>>();
        write_subjects(store, &active_entries)?;
    }
    Ok(())
}

fn write_subjects(store: &ScopeStore, entries: &[MemoryEntry]) -> Result<()> {
    let mut subjects: BTreeMap<String, SubjectCatalogItem> = BTreeMap::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.state == MemoryState::Active)
    {
        if let Some(subject) = entry.subject.as_ref() {
            let item = subjects
                .entry(subject.clone())
                .or_insert_with(|| SubjectCatalogItem {
                    subject: subject.clone(),
                    aliases: Vec::new(),
                    entry_ids: Vec::new(),
                    last_seen_at: entry.updated_at.clone(),
                    summary: Some(truncate_chars(entry.text.trim(), 160)),
                });
            item.entry_ids.push(entry.id.clone());
            if entry.updated_at > item.last_seen_at {
                item.last_seen_at = entry.updated_at.clone();
                item.summary = Some(truncate_chars(entry.text.trim(), 160));
            }
            let mut seen_aliases = item
                .aliases
                .iter()
                .map(|alias| normalize_for_hash(alias))
                .collect::<BTreeSet<_>>();
            for alias in entry_aliases(entry) {
                let normalized = normalize_for_hash(&alias);
                if normalized.is_empty() || !seen_aliases.insert(normalized) {
                    continue;
                }
                item.aliases.push(alias);
            }
        }
    }
    fs::write(
        store.root.join("subjects.json"),
        serde_json::to_string_pretty(&subjects)?,
    )?;
    Ok(())
}

fn ensure_user_compaction_status(store: &ScopeStore) -> Result<()> {
    if store.scope != MemoryScope::User {
        return Ok(());
    }
    fs::create_dir_all(&store.root)
        .with_context(|| format!("failed to create {}", store.root.display()))?;
    let path = store.root.join("compaction.json");
    if path.exists() {
        return Ok(());
    }
    let status = UserMemoryCompactionStatus::idle();
    fs::write(&path, serde_json::to_string_pretty(&status)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn refresh_user_compaction_status(
    store: &ScopeStore,
    entries: &[MemoryEntry],
    options: &MemoryOptions,
) -> Result<()> {
    if store.scope != MemoryScope::User {
        return Ok(());
    }
    let active_entries = entries
        .iter()
        .filter(|entry| entry.state == MemoryState::Active)
        .collect::<Vec<_>>();
    let rendered_size_bytes = rendered_user_memory_bytes(active_entries.iter().copied()) as u64;
    let mut status =
        read_user_compaction_status(store).unwrap_or_else(UserMemoryCompactionStatus::idle);
    status.last_input_hash = rendered_user_memory_hash(active_entries.iter().copied());
    status.updated_at = Some(now_string());
    let hard_threshold = status
        .threshold_override_bytes
        .unwrap_or(options.user_hard_threshold_bytes);
    let retry_waiting_for_hard = status.state == "retry_waiting"
        && rendered_size_bytes >= hard_threshold
        && status
            .next_retry_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|retry_at| retry_at > Utc::now());
    if retry_waiting_for_hard {
        write_user_compaction_status(store, &status)?;
        return Ok(());
    }
    if rendered_size_bytes >= hard_threshold {
        if status.state != "running" {
            status.state = "hard_pending".to_string();
        }
    } else if rendered_size_bytes >= options.user_soft_threshold_bytes {
        if status.state != "running" {
            status.state = "dirty".to_string();
        }
    } else if status.state != "running" {
        status.state = "idle".to_string();
        status.attempts = 0;
        status.last_error = None;
        status.next_retry_at = None;
        status.threshold_override_bytes = None;
    }
    write_user_compaction_status(store, &status)
}

#[allow(dead_code)]
fn record_user_compaction_failure(
    store: &ScopeStore,
    options: &MemoryOptions,
    error: impl Into<String>,
) -> Result<UserMemoryCompactionStatus> {
    ensure_user_compaction_status(store)?;
    let mut status =
        read_user_compaction_status(store).unwrap_or_else(UserMemoryCompactionStatus::idle);
    apply_user_compaction_failure(&mut status, options, error.into());
    write_user_compaction_status(store, &status)?;
    Ok(status)
}

#[allow(dead_code)]
fn record_user_compaction_no_shrink(
    store: &ScopeStore,
    options: &MemoryOptions,
    input_rendered_bytes: u64,
    output_rendered_bytes: u64,
) -> Result<UserMemoryCompactionStatus> {
    ensure_user_compaction_status(store)?;
    let mut status =
        read_user_compaction_status(store).unwrap_or_else(UserMemoryCompactionStatus::idle);
    apply_user_compaction_no_shrink(
        &mut status,
        options,
        input_rendered_bytes,
        output_rendered_bytes,
    );
    write_user_compaction_status(store, &status)?;
    Ok(status)
}

#[allow(dead_code)]
fn record_user_compaction_success(
    store: &ScopeStore,
    output_hash: impl Into<String>,
) -> Result<UserMemoryCompactionStatus> {
    ensure_user_compaction_status(store)?;
    let mut status =
        read_user_compaction_status(store).unwrap_or_else(UserMemoryCompactionStatus::idle);
    apply_user_compaction_success(&mut status, output_hash.into());
    write_user_compaction_status(store, &status)?;
    Ok(status)
}

fn record_user_soft_compaction_failure(
    store: &ScopeStore,
    error: impl Into<String>,
) -> Result<UserMemoryCompactionStatus> {
    ensure_user_compaction_status(store)?;
    let mut status =
        read_user_compaction_status(store).unwrap_or_else(UserMemoryCompactionStatus::idle);
    status.state = "dirty".to_string();
    status.last_error = Some(error.into());
    status.last_soft_compaction_at = Some(today_string());
    status.updated_at = Some(now_string());
    write_user_compaction_status(store, &status)?;
    Ok(status)
}

fn record_user_soft_compaction_no_shrink(
    store: &ScopeStore,
    input_rendered_bytes: u64,
    output_rendered_bytes: u64,
) -> Result<UserMemoryCompactionStatus> {
    ensure_user_compaction_status(store)?;
    let mut status =
        read_user_compaction_status(store).unwrap_or_else(UserMemoryCompactionStatus::idle);
    status.state = "dirty".to_string();
    status.last_error = Some(format!(
        "soft_compaction_output_not_smaller: input={input_rendered_bytes} output={output_rendered_bytes}"
    ));
    status.last_soft_compaction_at = Some(today_string());
    status.updated_at = Some(now_string());
    write_user_compaction_status(store, &status)?;
    Ok(status)
}

#[allow(dead_code)]
fn apply_user_compaction_failure(
    status: &mut UserMemoryCompactionStatus,
    options: &MemoryOptions,
    error: String,
) {
    status.state = "retry_waiting".to_string();
    status.attempts = status.attempts.saturating_add(1);
    status.last_error = Some(error);
    status.next_retry_at = Some(
        (Utc::now()
            + ChronoDuration::seconds(options.user_retry_after_failed_hard_compaction_secs as i64))
        .to_rfc3339(),
    );
    status.updated_at = Some(now_string());
}

#[allow(dead_code)]
fn apply_user_compaction_no_shrink(
    status: &mut UserMemoryCompactionStatus,
    options: &MemoryOptions,
    input_rendered_bytes: u64,
    output_rendered_bytes: u64,
) {
    status.attempts = status.attempts.saturating_add(1);
    status.last_error = Some(format!(
        "compaction_output_not_smaller: input={input_rendered_bytes} output={output_rendered_bytes}"
    ));
    status.next_retry_at = None;
    status.updated_at = Some(now_string());
    if status.attempts == 1 {
        status.state = "hard_pending".to_string();
        return;
    }
    status.state = "idle".to_string();
    status.threshold_override_bytes = Some(
        input_rendered_bytes
            .saturating_add(1)
            .max(options.user_hard_threshold_bytes),
    );
}

#[allow(dead_code)]
fn apply_user_compaction_success(status: &mut UserMemoryCompactionStatus, output_hash: String) {
    status.state = "idle".to_string();
    status.attempts = 0;
    status.last_error = None;
    status.next_retry_at = None;
    status.last_output_hash = output_hash;
    status.threshold_override_bytes = None;
    status.last_soft_compaction_at = Some(today_string());
    status.updated_at = Some(now_string());
}

fn read_user_compaction_status(store: &ScopeStore) -> Option<UserMemoryCompactionStatus> {
    let raw = fs::read_to_string(store.root.join("compaction.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_user_compaction_status(
    store: &ScopeStore,
    status: &UserMemoryCompactionStatus,
) -> Result<()> {
    let path = store.root.join("compaction.json");
    fs::write(&path, serde_json::to_string_pretty(status)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn rendered_user_memory_bytes<'a>(entries: impl IntoIterator<Item = &'a MemoryEntry>) -> usize {
    render_memory_prompt_entries(entries).len()
}

fn rendered_user_memory_hash<'a>(entries: impl IntoIterator<Item = &'a MemoryEntry>) -> String {
    let rendered = render_memory_prompt_entries(entries);
    rendered_memory_hash(&rendered)
}

fn rendered_memory_hash(rendered: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(rendered.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn compact_user_memory_entries(entries: Vec<MemoryEntry>) -> Vec<MemoryEntry> {
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        if entry.state != MemoryState::Active {
            continue;
        }
        let key = entry_hash(&entry.subject, &entry.text, &entry.tags);
        if !seen.insert(key) {
            continue;
        }
        output.push(entry);
    }
    output
}

fn build_user_compaction_entries(
    source: &MemorySource,
    store: &ScopeStore,
    entries: &[MemoryEntry],
    output: UserMemoryCompactionOutput,
) -> Result<Vec<MemoryEntry>> {
    if store.scope != MemoryScope::User {
        return Err(anyhow!("user_compaction_requires_user_scope"));
    }
    let active_entries = entries
        .iter()
        .filter(|entry| entry.state == MemoryState::Active)
        .collect::<Vec<_>>();
    let active_by_id = active_entries
        .iter()
        .map(|entry| (entry.id.as_str(), *entry))
        .collect::<BTreeMap<_, _>>();
    let mut next_id = entries
        .iter()
        .filter_map(|entry| parse_id_number(&entry.id))
        .max()
        .unwrap_or(0)
        + 1;
    let mut seen_ids = BTreeSet::new();
    let mut seen_hashes = BTreeSet::new();
    let now = now_string();
    let mut compacted = Vec::new();

    for item in output.entries {
        validate_text(&item.text)?;
        let subject = item
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, SUBJECT_MAX_CHARS));
        let tags = normalize_tags(item.tags);
        let text = item.text.trim().to_string();
        let hash = entry_hash(&subject, &text, &tags);
        if !seen_hashes.insert(hash) {
            continue;
        }

        let id = if let Some(id) = item
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if MemoryScope::from_id(id) != Some(MemoryScope::User) {
                return Err(anyhow!("invalid_user_memory_compaction_id: {id}"));
            }
            if !active_by_id.contains_key(id) {
                return Err(anyhow!("unknown_user_memory_compaction_id: {id}"));
            }
            if !seen_ids.insert(id.to_string()) {
                return Err(anyhow!("duplicate_user_memory_compaction_id: {id}"));
            }
            id.to_string()
        } else {
            loop {
                let id = format!("{}_{}", MemoryScope::User.id_prefix(), next_id);
                next_id += 1;
                if seen_ids.insert(id.clone()) {
                    break id;
                }
            }
        };

        let existing = active_by_id.get(id.as_str()).copied();
        compacted.push(MemoryEntry {
            id,
            scope: MemoryScope::User,
            subject,
            text,
            tags,
            created_at: existing
                .map(|entry| entry.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now.clone(),
            last_accessed_at: existing.and_then(|entry| entry.last_accessed_at.clone()),
            source_conversation_id: existing
                .and_then(|entry| entry.source_conversation_id.clone())
                .or_else(|| Some(source.conversation_id.clone())),
            source_agent_id: existing
                .and_then(|entry| entry.source_agent_id.clone())
                .or_else(|| source.agent_id.clone()),
            source_session_type: existing
                .and_then(|entry| entry.source_session_type.clone())
                .or_else(|| Some(source.session_type.clone())),
            state: MemoryState::Active,
        });
    }
    Ok(compacted)
}

fn render_user_compaction_request<'a>(
    entries: impl IntoIterator<Item = &'a MemoryEntry>,
) -> Result<String> {
    let entries = entries
        .into_iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "subject": entry.subject,
                "text": entry.text,
                "tags": entry.tags,
                "updated_at": entry.updated_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(format!(
        "Compact and filter these active user memory entries. Return the full replacement active entry list as strict JSON matching the schema.\n\n{}",
        serde_json::to_string_pretty(&json!({"entries": entries}))?
    ))
}

fn parse_user_compaction_output(text: &str) -> Result<UserMemoryCompactionOutput> {
    serde_json::from_str(text.trim())
        .map_err(|error| anyhow!("invalid_user_compaction_json: {error}"))
}

fn chat_message_text(message: &ChatMessage) -> String {
    let mut output = String::new();
    for item in &message.data {
        if let ChatMessageItem::Context(context) = item {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&context.text);
        }
    }
    output
}

fn token_usage_json(token_usage: &TokenUsage) -> Value {
    json!({
        "cache_read": token_usage.cache_read,
        "cache_write": token_usage.cache_write,
        "uncache_input": token_usage.uncache_input,
        "output": token_usage.output,
        "cost_usd": token_usage.cost_usd.as_ref().map(|cost| json!({
            "cache_read": cost.cache_read,
            "cache_write": cost.cache_write,
            "uncache_input": cost.uncache_input,
            "output": cost.output,
        })),
    })
}

fn render_memory_prompt_entries<'a>(entries: impl IntoIterator<Item = &'a MemoryEntry>) -> String {
    let mut output = String::new();
    for entry in entries {
        output.push_str("* [");
        output.push_str(&entry.id);
        output.push_str("] ");
        if let Some(subject) = entry.subject.as_ref().filter(|value| !value.is_empty()) {
            output.push('(');
            output.push_str(subject);
            output.push_str(") ");
        }
        output.push_str(entry.text.trim());
        for tag in &entry.tags {
            output.push(' ');
            output.push('#');
            output.push_str(tag);
        }
        output.push('\n');
    }
    output
}

#[allow(dead_code)]
fn truncate_prompt_block(text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let suffix = "\n[memory prompt truncated]\n";
    let budget = max_bytes.saturating_sub(suffix.len()).max(1);
    let mut output = String::new();
    for ch in text.chars() {
        if output.len() + ch.len_utf8() > budget {
            break;
        }
        output.push(ch);
    }
    output.push_str(suffix);
    (output, true)
}

impl UserMemoryCompactionStatus {
    fn idle() -> Self {
        Self {
            state: "idle".to_string(),
            attempts: 0,
            last_error: None,
            next_retry_at: None,
            last_input_hash: String::new(),
            last_output_hash: String::new(),
            threshold_override_bytes: None,
            last_soft_compaction_at: None,
            updated_at: Some(now_string()),
        }
    }
}

fn allocate_id(store: &ScopeStore, entries: &[MemoryEntry]) -> String {
    let next_id = read_manifest(store)
        .map(|manifest| manifest.next_id)
        .unwrap_or_else(|| {
            entries
                .iter()
                .filter_map(|entry| parse_id_number(&entry.id))
                .max()
                .unwrap_or(0)
                + 1
        });
    format!("{}_{}", store.scope.id_prefix(), next_id)
}

fn read_manifest(store: &ScopeStore) -> Option<MemoryManifest> {
    let name = if store.scope == MemoryScope::User {
        "manifest.json"
    } else {
        "index.json"
    };
    fs::read_to_string(store.root.join(name))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn parse_id_number(id: &str) -> Option<u64> {
    id.split_once('_')?.1.parse().ok()
}

fn search_entries(
    entries: Vec<MemoryEntry>,
    query: &str,
    limit: usize,
    scope_filter: Option<MemoryScope>,
) -> Vec<MemoryCandidate> {
    let query_tokens = tokenize(query);
    let mut documents = entries
        .into_iter()
        .filter(|entry| entry.state == MemoryState::Active)
        .filter(|entry| scope_filter.is_none_or(|scope| entry.scope == scope))
        .enumerate()
        .map(|(index, entry)| build_search_document(index, entry))
        .collect::<Vec<_>>();
    if query_tokens.is_empty() {
        documents.sort_by(|left, right| {
            scope_rank(left.entry.scope)
                .cmp(&scope_rank(right.entry.scope))
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| right.entry.updated_at.cmp(&left.entry.updated_at))
        });
        return documents
            .into_iter()
            .take(limit)
            .map(|document| MemoryCandidate {
                entry: document.entry,
                score: 1.0,
            })
            .collect();
    }

    let mut fused_scores: HashMap<usize, f64> = HashMap::new();
    add_rrf_scores(
        &mut fused_scores,
        bm25_rank(&query_tokens, &documents, BM25_TOP_K),
    );
    add_rrf_scores(
        &mut fused_scores,
        dense_feature_rank(&query_tokens, &documents, DENSE_TOP_K),
    );

    let normalized_query = normalize_for_hash(query);
    let mut candidates = documents
        .into_iter()
        .filter_map(|document| {
            let score = fused_scores.get(&document.index).copied().unwrap_or(0.0)
                + metadata_boost(&normalized_query, &query_tokens, &document);
            (score > 0.0).then_some(MemoryCandidate {
                entry: document.entry,
                score,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| scope_rank(left.entry.scope).cmp(&scope_rank(right.entry.scope)))
            .then_with(|| right.entry.updated_at.cmp(&left.entry.updated_at))
    });
    candidates.truncate(limit);
    candidates
}

fn add_rrf_scores(fused_scores: &mut HashMap<usize, f64>, ranked: Vec<(usize, f64)>) {
    for (rank, (document_index, _score)) in ranked.into_iter().enumerate() {
        *fused_scores.entry(document_index).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
}

fn build_search_document(index: usize, entry: MemoryEntry) -> SearchDocument {
    let aliases = entry_aliases(&entry);
    let tokens = searchable_tokens(&entry, &aliases);
    SearchDocument {
        index,
        id: entry.id.clone(),
        scope: entry.scope,
        subject: entry.subject.clone(),
        aliases,
        text: entry.text.clone(),
        tags: entry.tags.clone(),
        conversation_id: entry.source_conversation_id.clone(),
        updated_at: entry.updated_at.clone(),
        entry,
        tokens,
    }
}

fn bm25_rank(
    query_tokens: &[String],
    documents: &[SearchDocument],
    top_k: usize,
) -> Vec<(usize, f64)> {
    if documents.is_empty() {
        return Vec::new();
    }
    let doc_count = documents.len() as f64;
    let avg_doc_len = documents
        .iter()
        .map(|document| document.tokens.len() as f64)
        .sum::<f64>()
        / doc_count.max(1.0);
    let mut doc_freq: HashMap<&str, usize> = HashMap::new();
    for document in documents {
        let mut seen = BTreeSet::new();
        for token in &document.tokens {
            if seen.insert(token.as_str()) {
                *doc_freq.entry(token.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut ranked = documents
        .iter()
        .filter_map(|document| {
            let mut term_freq: HashMap<&str, usize> = HashMap::new();
            for token in &document.tokens {
                *term_freq.entry(token.as_str()).or_insert(0) += 1;
            }
            let doc_len = document.tokens.len() as f64;
            let mut score = 0.0;
            for query_token in query_tokens {
                let Some(freq) = term_freq.get(query_token.as_str()).copied() else {
                    continue;
                };
                let df = doc_freq.get(query_token.as_str()).copied().unwrap_or(0) as f64;
                let idf = (1.0 + (doc_count - df + 0.5) / (df + 0.5)).ln();
                let freq = freq as f64;
                let denominator =
                    freq + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / avg_doc_len.max(1.0));
                score += idf * (freq * (BM25_K1 + 1.0)) / denominator;
            }
            (score > 0.0).then_some((document.index, score))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
    ranked.truncate(top_k);
    ranked
}

fn dense_feature_rank(
    query_tokens: &[String],
    documents: &[SearchDocument],
    top_k: usize,
) -> Vec<(usize, f64)> {
    if query_tokens.is_empty() || documents.is_empty() {
        return Vec::new();
    }
    let query_vector = dense_feature_vector(query_tokens);
    let query_norm = vector_norm(&query_vector);
    if query_norm == 0.0 {
        return Vec::new();
    }

    let mut ranked = documents
        .iter()
        .filter_map(|document| {
            let document_vector = dense_feature_vector(&document.tokens);
            let document_norm = vector_norm(&document_vector);
            if document_norm == 0.0 {
                return None;
            }
            let dot = query_vector
                .iter()
                .zip(document_vector.iter())
                .map(|(query_weight, document_weight)| query_weight * document_weight)
                .sum::<f64>();
            let score = dot / (query_norm * document_norm);
            (score > 0.0).then_some((document.index, score))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
    ranked.truncate(top_k);
    ranked
}

fn dense_feature_vector(tokens: &[String]) -> Vec<f64> {
    let mut vector = vec![0.0; DENSE_VECTOR_DIM];
    for token in tokens {
        add_dense_feature(&mut vector, token, 1.0);
        let chars = token.chars().collect::<Vec<_>>();
        if chars.len() >= 3 {
            for window in chars.windows(3) {
                let gram = window.iter().collect::<String>();
                add_dense_feature(&mut vector, &gram, 0.35);
            }
        }
    }
    vector
}

fn add_dense_feature(vector: &mut [f64], feature: &str, weight: f64) {
    let normalized = normalize_for_hash(feature);
    if normalized.is_empty() {
        return;
    }
    let digest = Sha1::digest(normalized.as_bytes());
    let mut hash_bytes = [0u8; 8];
    hash_bytes.copy_from_slice(&digest[..8]);
    let hash = u64::from_be_bytes(hash_bytes);
    let index = (hash as usize) % vector.len();
    let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
    vector[index] += sign * weight;
}

fn vector_norm(vector: &[f64]) -> f64 {
    vector.iter().map(|value| value * value).sum::<f64>().sqrt()
}

fn metadata_boost(
    normalized_query: &str,
    query_tokens: &[String],
    document: &SearchDocument,
) -> f64 {
    let mut boost = 0.0;
    if let Some(subject) = document.subject.as_ref() {
        let subject = normalize_for_hash(subject);
        if !subject.is_empty()
            && (normalized_query.contains(&subject) || subject.contains(normalized_query))
        {
            boost += 0.02;
        }
    }
    for alias in &document.aliases {
        let alias = normalize_for_hash(alias);
        if !alias.is_empty()
            && (normalized_query.contains(&alias) || alias.contains(normalized_query))
        {
            boost += 0.015;
        }
    }
    let query_token_set = query_tokens
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for tag in &document.tags {
        let tag = normalize_for_hash(tag);
        if !tag.is_empty() && query_token_set.contains(tag.as_str()) {
            boost += 0.01;
        }
    }
    if let Some(conversation_id) = document.conversation_id.as_ref() {
        let conversation_id = normalize_for_hash(conversation_id);
        if !conversation_id.is_empty() && normalized_query.contains(&conversation_id) {
            boost += 0.005;
        }
    }
    if normalize_for_hash(&document.id) == normalized_query {
        boost += 0.02;
    }
    let text = normalize_for_hash(&document.text);
    if !text.is_empty() && text.contains(normalized_query) {
        boost += 0.005;
    }
    if document.scope == MemoryScope::Conversation {
        boost += 0.001;
    }
    boost
}

fn scope_rank(scope: MemoryScope) -> u8 {
    match scope {
        MemoryScope::Conversation => 0,
        MemoryScope::Public => 1,
        MemoryScope::User => 2,
    }
}

fn dedupe_search_hits(hits: Vec<MemoryCandidate>) -> Vec<MemoryCandidate> {
    let mut output = Vec::new();
    let mut seen_text = BTreeSet::new();
    let mut seen_subject_text = BTreeSet::new();
    for hit in hits {
        let normalized_text = normalize_for_hash(&hit.entry.text);
        if normalized_text.is_empty() || !seen_text.insert(normalized_text.clone()) {
            continue;
        }
        if let Some(subject) = hit.entry.subject.as_ref() {
            let key = format!("{}\n{}", normalize_for_hash(subject), normalized_text);
            if !seen_subject_text.insert(key) {
                continue;
            }
        }
        output.push(hit);
    }
    output
}

fn render_search_results(
    hits: Vec<MemoryCandidate>,
    limit: usize,
    total_text_max_bytes: usize,
) -> (Vec<Value>, BTreeSet<String>, bool) {
    let mut results = Vec::new();
    let mut returned_ids = BTreeSet::new();
    let mut used_text_bytes = 0usize;
    let mut truncated = hits.len() > limit;
    for hit in hits {
        if results.len() >= limit {
            truncated = true;
            break;
        }
        let remaining_bytes = total_text_max_bytes.saturating_sub(used_text_bytes);
        if remaining_bytes == 0 {
            truncated = true;
            break;
        }
        let per_entry_max = SEARCH_ENTRY_TEXT_MAX_BYTES.min(remaining_bytes);
        let text = truncate_text_for_search(&hit.entry.text, per_entry_max);
        let text_bytes = text.len();
        if used_text_bytes + text_bytes > total_text_max_bytes {
            truncated = true;
            break;
        }
        used_text_bytes += text_bytes;
        returned_ids.insert(hit.entry.id.clone());
        results.push(json!({
            "id": hit.entry.id,
            "scope": hit.entry.scope.as_str(),
            "subject": hit.entry.subject,
            "text": text,
            "tags": hit.entry.tags,
            "updated_at": hit.entry.updated_at,
            "score": hit.score,
        }));
    }
    (results, returned_ids, truncated)
}

fn truncate_text_for_search(text: &str, max_bytes: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }
    let suffix = "\n[memory text truncated]";
    let content_budget = max_bytes.saturating_sub(suffix.len()).max(1);
    let mut output = String::new();
    for ch in trimmed.chars() {
        if output.len() + ch.len_utf8() > content_budget {
            break;
        }
        output.push(ch);
    }
    output.push_str("\n[memory text truncated]");
    output
}

fn searchable_tokens(entry: &MemoryEntry, aliases: &[String]) -> Vec<String> {
    let mut document = tokenize(&entry.text);
    if let Some(subject) = entry.subject.as_ref() {
        document.extend(tokenize(subject));
        document.extend(tokenize(subject));
    }
    for alias in aliases {
        document.extend(tokenize(alias));
        document.extend(tokenize(alias));
    }
    for tag in &entry.tags {
        document.extend(tokenize(tag));
    }
    document
}

fn entry_aliases(entry: &MemoryEntry) -> Vec<String> {
    let mut aliases = Vec::new();
    let mut seen = BTreeSet::new();
    let mut push_alias = |value: String| {
        let value = value.trim().trim_matches('#').to_string();
        if value.is_empty() || value.len() > 80 {
            return;
        }
        let normalized = normalize_for_hash(&value);
        if normalized.is_empty() || !seen.insert(normalized) {
            return;
        }
        aliases.push(value);
    };

    if let Some(subject) = entry.subject.as_ref() {
        push_alias(subject.clone());
        let normalized = normalize_for_hash(subject);
        if normalized != subject.trim() {
            push_alias(normalized);
        }
        let subject_tokens = tokenize(subject);
        if subject_tokens.len() > 1 {
            push_alias(subject_tokens.join(""));
        }
        for token in subject_tokens {
            if token.len() >= 2 || token.chars().any(is_cjk) {
                push_alias(token);
            }
        }
    }
    for tag in &entry.tags {
        push_alias(tag.clone());
    }
    aliases
}

fn local_consistency_decision(
    draft: &MemoryEntry,
    candidates: &[MemoryCandidate],
) -> MemoryConsistencyDecision {
    let same_subject = draft
        .subject
        .as_ref()
        .map(|subject| normalize_for_hash(subject))
        .filter(|subject| !subject.is_empty());
    let mut same_subject_candidates = candidates
        .iter()
        .filter(|candidate| {
            same_subject.as_ref().is_some_and(|subject| {
                candidate
                    .entry
                    .subject
                    .as_ref()
                    .map(|value| normalize_for_hash(value) == *subject)
                    .unwrap_or(false)
            })
        })
        .collect::<Vec<_>>();

    if !same_subject_candidates.is_empty() {
        same_subject_candidates
            .sort_by(|left, right| right.entry.updated_at.cmp(&left.entry.updated_at));
        let target = &same_subject_candidates[0].entry;
        if normalize_for_hash(&target.text) == normalize_for_hash(&draft.text) {
            return MemoryConsistencyDecision::success(vec![MemoryAction::Touch {
                id: target.id.clone(),
            }]);
        }
        let mut actions = vec![MemoryAction::Update {
            id: target.id.clone(),
            text: draft.text.clone(),
        }];
        for stale in same_subject_candidates.iter().skip(1) {
            actions.push(MemoryAction::Delete {
                id: stale.entry.id.clone(),
            });
        }
        return MemoryConsistencyDecision::success(actions);
    }

    if let Some(best) = candidates.first() {
        if best.score >= 0.9 {
            return MemoryConsistencyDecision::success(vec![MemoryAction::Touch {
                id: best.entry.id.clone(),
            }]);
        }
        if best.score >= 0.65 {
            return MemoryConsistencyDecision::success(vec![MemoryAction::Update {
                id: best.entry.id.clone(),
                text: draft.text.clone(),
            }]);
        }
    }

    MemoryConsistencyDecision::success(vec![MemoryAction::Insert])
}

fn render_consistency_decision_request(
    draft: &MemoryEntry,
    candidates: &[MemoryCandidate],
) -> Result<String> {
    let candidates = candidates
        .iter()
        .map(|candidate| {
            json!({
                "id": candidate.entry.id,
                "subject": candidate.entry.subject,
                "text": candidate.entry.text,
                "tags": candidate.entry.tags,
                "updated_at": candidate.entry.updated_at,
                "score": candidate.score,
            })
        })
        .collect::<Vec<_>>();
    Ok(format!(
        "Judge this memory write. Return strict JSON matching the schema.\n\n{}",
        serde_json::to_string_pretty(&json!({
            "draft": {
                "scope": draft.scope,
                "subject": draft.subject,
                "text": draft.text,
                "tags": draft.tags,
            },
            "candidates": candidates,
        }))?
    ))
}

fn parse_memory_consistency_decision(text: &str) -> Result<MemoryConsistencyDecision> {
    let decision: MemoryConsistencyDecision = serde_json::from_str(text.trim())
        .map_err(|error| anyhow!("invalid_dedupe_model_json: {error}"))?;
    if matches!(decision.decision, MemoryConsistencyStatus::Failure) {
        return Ok(decision);
    }
    if decision.actions.is_empty() {
        return Err(anyhow!("invalid_dedupe_model_actions"));
    }
    Ok(decision)
}

impl MemoryConsistencyDecision {
    fn success(actions: Vec<MemoryAction>) -> Self {
        Self {
            decision: MemoryConsistencyStatus::Success,
            reason: None,
            actions,
        }
    }
}

fn validate_actions(actions: &[MemoryAction], candidates: &[MemoryCandidate]) -> Result<()> {
    if actions.is_empty() {
        return Err(anyhow!("invalid_action"));
    }
    let candidate_ids = candidates
        .iter()
        .map(|candidate| candidate.entry.id.as_str())
        .collect::<BTreeSet<_>>();
    let insert_count = actions
        .iter()
        .filter(|action| matches!(action, MemoryAction::Insert))
        .count();
    if insert_count > 1 {
        return Err(anyhow!("invalid_action"));
    }
    for action in actions {
        match action {
            MemoryAction::Touch { id } | MemoryAction::Delete { id } => {
                if !candidate_ids.contains(id.as_str()) {
                    return Err(anyhow!("invalid_action"));
                }
            }
            MemoryAction::Update { id, text } => {
                if !candidate_ids.contains(id.as_str()) {
                    return Err(anyhow!("invalid_action"));
                }
                validate_text(text)?;
            }
            MemoryAction::Insert => {}
        }
    }
    Ok(())
}

fn apply_actions(
    entries: &mut Vec<MemoryEntry>,
    store: &ScopeStore,
    mut draft: MemoryEntry,
    actions: &[MemoryAction],
) -> Result<()> {
    let now = now_string();
    for action in actions {
        match action {
            MemoryAction::Touch { id } => {
                if let Some(entry) = entries.iter_mut().find(|entry| entry.id == *id) {
                    entry.updated_at = now.clone();
                    entry.last_accessed_at = Some(now.clone());
                }
            }
            MemoryAction::Update { id, text } => {
                if let Some(entry) = entries.iter_mut().find(|entry| entry.id == *id) {
                    entry.text = text.trim().to_string();
                    entry.subject = draft.subject.clone();
                    entry.tags = draft.tags.clone();
                    entry.updated_at = now.clone();
                    entry.last_accessed_at = Some(now.clone());
                }
            }
            MemoryAction::Delete { id } => {
                if let Some(entry) = entries.iter_mut().find(|entry| entry.id == *id) {
                    entry.state = MemoryState::Deleted;
                    entry.updated_at = now.clone();
                }
            }
            MemoryAction::Insert => {
                draft.id = allocate_id(store, entries);
                draft.created_at = now.clone();
                draft.updated_at = now.clone();
                entries.push(draft.clone());
            }
        }
    }
    Ok(())
}

fn entry_hash(subject: &Option<String>, text: &str, tags: &[String]) -> String {
    let normalized = format!(
        "{}\n{}\n{}",
        subject
            .as_ref()
            .map(|value| normalize_for_hash(value))
            .unwrap_or_default(),
        normalize_for_hash(text),
        tags.iter()
            .map(|tag| normalize_for_hash(tag))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut hasher = Sha1::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn tokenize(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for segment in memory_jieba().cut(value, false) {
        let mut current = String::new();
        for ch in segment.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                current.push(ch.to_ascii_lowercase());
                continue;
            }
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
            if is_cjk(ch) {
                tokens.push(ch.to_string());
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
    }
    tokens
}

fn memory_jieba() -> &'static Jieba {
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    JIEBA.get_or_init(Jieba::new)
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
        || ('\u{3400}'..='\u{4dbf}').contains(&ch)
        || ('\u{f900}'..='\u{faff}').contains(&ch)
}

fn normalize_for_hash(value: &str) -> String {
    tokenize(value).join(" ")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn now_string() -> String {
    Utc::now().to_rfc3339()
}

fn today_string() -> String {
    Utc::now().date_naive().to_string()
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn default_next_id() -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellaclaw_core::model_config::{
        ModelCapability, ProviderType, RetryMode, TokenEstimatorType,
    };

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-memory-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn test_chat_model_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "test-model".to_string(),
            url: "http://127.0.0.1:9".to_string(),
            api_key_env: "TEST_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 8192,
            max_tokens: 1024,
            cache_timeout: 0,
            idle_timeout_compact_enabled: true,
            conn_timeout: 1,
            request_timeout: 1,
            max_request_size: 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    #[test]
    fn provider_usage_log_records_token_usage() {
        let workdir = temp_root("provider-usage");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: Some("agent-a".to_string()),
                session_type: "foreground".to_string(),
            },
        );
        let store = service.store(MemoryScope::Conversation);
        let response = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "{}".to_string(),
            })],
        )
        .with_token_usage(TokenUsage {
            cache_read: 1,
            cache_write: 2,
            uncache_input: 3,
            output: 4,
            cost_usd: None,
        });

        service
            .write_provider_usage(
                &store,
                "memory_dedupe",
                None,
                &test_chat_model_config(),
                &response,
            )
            .unwrap();

        let raw = fs::read_to_string(store.root.join("usage.jsonl")).unwrap();
        let value: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(value["kind"], "memory_dedupe");
        assert_eq!(value["scope"], "conversation");
        assert_eq!(value["conversation_id"], "c1");
        assert_eq!(value["source_agent_id"], "agent-a");
        assert_eq!(value["token_usage"]["cache_read"], 1);
        assert_eq!(value["token_usage"]["output"], 4);
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn write_search_update_delete_memory() {
        let workdir = temp_root("basic");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );

        assert_eq!(
            service
                .write(MemoryWriteRequest {
                    scope: "conversation".to_string(),
                    subject: Some("Project A".to_string()),
                    text: "Project A uses Chinese labels.".to_string(),
                    tags: vec!["project".to_string()],
                })
                .unwrap()["status"],
            "success"
        );
        let found = service
            .search(MemorySearchRequest {
                query: "Project A Chinese".to_string(),
                limit: Some(5),
                scopes: Vec::new(),
            })
            .unwrap();
        assert_eq!(found["results"][0]["id"], "c_1");

        service
            .update(MemoryUpdateRequest {
                memory_id: "c_1".to_string(),
                text: "Project A uses English labels.".to_string(),
            })
            .unwrap();
        let found = service
            .search(MemorySearchRequest {
                query: "English labels".to_string(),
                limit: Some(5),
                scopes: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            found["results"][0]["text"],
            "Project A uses English labels."
        );

        service
            .delete(MemoryDeleteRequest {
                memory_id: "c_1".to_string(),
            })
            .unwrap();
        let entries_path = workdir
            .join("conversations")
            .join("c1")
            .join(".stellaclaw/memory_v1/conversation/entries.jsonl");
        assert_eq!(fs::read_to_string(&entries_path).unwrap(), "");
        let found = service
            .search(MemorySearchRequest {
                query: "Project A".to_string(),
                limit: Some(5),
                scopes: Vec::new(),
            })
            .unwrap();
        assert!(found["results"].as_array().unwrap().is_empty());
        service
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Project B".to_string()),
                text: "Project B is active.".to_string(),
                tags: vec![],
            })
            .unwrap();
        let found = service
            .search(MemorySearchRequest {
                query: "Project B active".to_string(),
                limit: Some(5),
                scopes: Vec::new(),
            })
            .unwrap();
        assert_eq!(found["results"][0]["id"], "c_2");
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn same_subject_write_updates_existing_entry() {
        let workdir = temp_root("same-subject");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Project A".to_string()),
                text: "Project A status is old.".to_string(),
                tags: vec![],
            })
            .unwrap();
        service
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Project A".to_string()),
                text: "Project A status is new.".to_string(),
                tags: vec![],
            })
            .unwrap();

        let found = service
            .search(MemorySearchRequest {
                query: "Project A status".to_string(),
                limit: Some(10),
                scopes: Vec::new(),
            })
            .unwrap();
        let results = found["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], "c_1");
        assert_eq!(results[0]["text"], "Project A status is new.");
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn public_subject_catalog_records_aliases_and_summary() {
        let workdir = temp_root("subject-catalog");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "public".to_string(),
                subject: Some("Project Phoenix".to_string()),
                text: "Durable public fact for catalog generation.".to_string(),
                tags: vec!["phoenix-alpha".to_string()],
            })
            .unwrap();

        let raw = fs::read_to_string(workdir.join("rundir/memory_v1/public/subjects.json"))
            .expect("subjects catalog should be written");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let item = &catalog["Project Phoenix"];
        assert_eq!(item["subject"], "Project Phoenix");
        assert_eq!(item["entry_ids"][0], "p_1");
        assert!(item["last_seen_at"].as_str().unwrap().contains('T'));
        assert!(item["summary"]
            .as_str()
            .unwrap()
            .contains("Durable public fact"));
        let aliases = item["aliases"].as_array().unwrap();
        assert!(aliases
            .iter()
            .any(|alias| alias.as_str() == Some("projectphoenix")));
        assert!(aliases
            .iter()
            .any(|alias| alias.as_str() == Some("phoenix-alpha")));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn search_uses_subject_alias_tokens() {
        let workdir = temp_root("alias-search");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Project Phoenix".to_string()),
                text: "The durable detail is unrelated to the compact alias spelling.".to_string(),
                tags: Vec::new(),
            })
            .unwrap();

        let found = service
            .search(MemorySearchRequest {
                query: "projectphoenix".to_string(),
                limit: Some(5),
                scopes: vec!["conversation".to_string()],
            })
            .unwrap();
        assert_eq!(found["results"][0]["id"], "c_1");
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn memory_consistency_decision_parser_validates_provider_output() {
        let decision = parse_memory_consistency_decision(
            r#"{"decision":"success","actions":[{"type":"update","id":"c_1","text":"new fact"}]}"#,
        )
        .unwrap();
        assert_eq!(decision.decision, MemoryConsistencyStatus::Success);
        assert_eq!(
            decision.actions,
            vec![MemoryAction::Update {
                id: "c_1".to_string(),
                text: "new fact".to_string()
            }]
        );

        let failure =
            parse_memory_consistency_decision(r#"{"decision":"failure","reason":"unclear"}"#)
                .unwrap();
        assert_eq!(failure.decision, MemoryConsistencyStatus::Failure);
        assert_eq!(failure.reason.as_deref(), Some("unclear"));

        let error = parse_memory_consistency_decision(r#"{"decision":"success","actions":[]}"#)
            .unwrap_err();
        assert!(error.to_string().contains("invalid_dedupe_model_actions"));
    }

    #[test]
    fn search_dedupes_truncates_and_touches_results() {
        let workdir = temp_root("search-budget");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root.clone(),
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        let long_text = format!("Project A {}", "detail ".repeat(120));
        service
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Project A conversation".to_string()),
                text: long_text.clone(),
                tags: vec![],
            })
            .unwrap();
        service
            .write(MemoryWriteRequest {
                scope: "public".to_string(),
                subject: Some("Project A public".to_string()),
                text: long_text,
                tags: vec![],
            })
            .unwrap();

        let found = service
            .search(MemorySearchRequest {
                query: "Project A detail".to_string(),
                limit: Some(10),
                scopes: Vec::new(),
            })
            .unwrap();
        let results = found["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0]["text"]
            .as_str()
            .unwrap()
            .contains("[memory text truncated]"));

        let entries = read_entries(&service.store(MemoryScope::Conversation)).unwrap();
        assert!(entries[0].last_accessed_at.is_some());
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn write_entries_rejects_active_entry_limit() {
        let workdir = temp_root("entry-limit");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        let store = service.store(MemoryScope::Conversation);
        ensure_scope_dirs(&store).unwrap();
        let now = now_string();
        let entries = (0..=MAX_ACTIVE_ENTRIES_PER_SCOPE)
            .map(|index| MemoryEntry {
                id: format!("c_{}", index + 1),
                scope: MemoryScope::Conversation,
                subject: Some(format!("Subject {index}")),
                text: format!("Memory entry {index}"),
                tags: Vec::new(),
                created_at: now.clone(),
                updated_at: now.clone(),
                last_accessed_at: None,
                source_conversation_id: Some("c1".to_string()),
                source_agent_id: None,
                source_session_type: Some("foreground".to_string()),
                state: MemoryState::Active,
            })
            .collect::<Vec<_>>();

        let error = write_entries_and_manifest(&store, &entries).unwrap_err();
        assert!(error.to_string().contains("memory_store_entry_limit"));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn memory_options_control_candidate_and_result_budgets() {
        let options = MemoryOptions {
            write_candidate_limit: 99,
            tool_result_max_bytes: 1,
            ..MemoryOptions::default()
        }
        .normalized();
        assert_eq!(options.write_candidate_limit, MAX_WRITE_CANDIDATE_LIMIT);
        assert_eq!(options.tool_result_max_bytes, 512);

        let workdir = temp_root("options");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::with_options(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
            MemoryOptions {
                write_candidate_limit: 1,
                tool_result_max_bytes: 512,
                ..MemoryOptions::default()
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Budget".to_string()),
                text: format!("Budget {}", "detail ".repeat(140)),
                tags: vec![],
            })
            .unwrap();

        let found = service
            .search(MemorySearchRequest {
                query: "Budget detail".to_string(),
                limit: Some(5),
                scopes: Vec::new(),
            })
            .unwrap();
        let results = found["results"].as_array().unwrap();
        let total_text_bytes = results
            .iter()
            .map(|result| result["text"].as_str().unwrap().len())
            .sum::<usize>();
        assert!(total_text_bytes <= 512);
        assert!(results[0]["text"]
            .as_str()
            .unwrap()
            .contains("[memory text truncated]"));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_scope_creates_compaction_status_file() {
        let workdir = temp_root("user-compaction-status");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "user".to_string(),
                subject: Some("Language".to_string()),
                text: "用户偏好中文沟通。".to_string(),
                tags: vec![],
            })
            .unwrap();

        let raw = fs::read_to_string(workdir.join("rundir/memory_v1/user/compaction.json"))
            .expect("compaction status should be written");
        let status: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(status["state"], "idle");
        assert_eq!(status["attempts"], 0);
        assert!(status["last_input_hash"].as_str().unwrap().len() >= 40);
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_compaction_status_tracks_rendered_thresholds() {
        let workdir = temp_root("user-compaction-thresholds");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::with_options(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
            MemoryOptions {
                write_candidate_limit: 10,
                tool_result_max_bytes: 4096,
                user_soft_threshold_bytes: 512,
                user_hard_threshold_bytes: 512,
                ..MemoryOptions::default()
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "user".to_string(),
                subject: Some("Threshold".to_string()),
                text: format!("User threshold {}", "detail ".repeat(80)),
                tags: vec!["prefs".to_string()],
            })
            .unwrap();

        let status = fs::read_to_string(workdir.join("rundir/memory_v1/user/compaction.json"))
            .expect("compaction status should be written");
        let status: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["state"], "idle");
        assert_eq!(status["attempts"], 2);
        assert!(status["threshold_override_bytes"].as_u64().unwrap() >= 512);
        assert!(status["last_input_hash"].as_str().unwrap().len() >= 40);

        let manifest = fs::read_to_string(workdir.join("rundir/memory_v1/user/manifest.json"))
            .expect("manifest should be written");
        let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(
            manifest["rendered_size_bytes"].as_u64().unwrap()
                >= service.options.user_hard_threshold_bytes
        );
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn hard_user_compaction_filters_duplicate_entries() {
        let workdir = temp_root("hard-user-compaction");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::with_options(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
            MemoryOptions {
                user_soft_threshold_bytes: 512,
                user_hard_threshold_bytes: 512,
                ..MemoryOptions::default()
            },
        );
        let store = service.store(MemoryScope::User);
        ensure_scope_dirs(&store).unwrap();
        let now = now_string();
        let duplicate_text = format!("Duplicate user preference {}", "detail ".repeat(50));
        let entries = vec![
            MemoryEntry {
                id: "u_1".to_string(),
                scope: MemoryScope::User,
                subject: Some("Duplicate".to_string()),
                text: duplicate_text.clone(),
                tags: vec!["prefs".to_string()],
                created_at: now.clone(),
                updated_at: now.clone(),
                last_accessed_at: None,
                source_conversation_id: Some("c1".to_string()),
                source_agent_id: None,
                source_session_type: Some("foreground".to_string()),
                state: MemoryState::Active,
            },
            MemoryEntry {
                id: "u_2".to_string(),
                scope: MemoryScope::User,
                subject: Some("Duplicate".to_string()),
                text: duplicate_text,
                tags: vec!["prefs".to_string()],
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                source_conversation_id: Some("c1".to_string()),
                source_agent_id: None,
                source_session_type: Some("foreground".to_string()),
                state: MemoryState::Active,
            },
        ];
        write_entries_and_manifest(&store, &entries).unwrap();
        service
            .refresh_user_compaction_status(&store, &entries)
            .unwrap();
        service.run_user_hard_compaction_if_pending(&store).unwrap();

        let entries = read_entries(&store).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "u_1");
        let status = read_user_compaction_status(&store).unwrap();
        assert_eq!(status.state, "idle");
        assert_eq!(status.attempts, 0);
        assert!(status.last_output_hash.len() >= 40);
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn provider_user_compaction_output_rewrites_entries_safely() {
        let workdir = temp_root("provider-user-compaction");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: Some("agent".to_string()),
                session_type: "foreground".to_string(),
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "user".to_string(),
                subject: Some("Style".to_string()),
                text: "Prefer concise Chinese answers.".to_string(),
                tags: vec!["style".to_string()],
            })
            .unwrap();
        service
            .write(MemoryWriteRequest {
                scope: "user".to_string(),
                subject: Some("Noise".to_string()),
                text: "Temporary preference that should be filtered.".to_string(),
                tags: vec!["temp".to_string()],
            })
            .unwrap();

        let report = service
            .apply_user_compaction_output(UserMemoryCompactionOutput {
                entries: vec![
                    UserMemoryCompactionOutputEntry {
                        id: Some("u_1".to_string()),
                        subject: Some("Style".to_string()),
                        text: "Prefer concise Chinese answers; keep technical terms in English."
                            .to_string(),
                        tags: vec!["style".to_string(), "style".to_string()],
                    },
                    UserMemoryCompactionOutputEntry {
                        id: None,
                        subject: Some("Workflow".to_string()),
                        text: "Prefer implementation with focused verification.".to_string(),
                        tags: vec!["workflow".to_string()],
                    },
                ],
            })
            .unwrap();

        let store = service.store(MemoryScope::User);
        let entries = read_entries(&store).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "u_1");
        assert_eq!(entries[0].tags, vec!["style"]);
        assert!(entries[0].text.contains("technical terms"));
        assert_eq!(entries[1].id, "u_3");
        assert_eq!(entries[1].source_agent_id.as_deref(), Some("agent"));
        assert!(report.output_hash.len() >= 40);
        let status = read_user_compaction_status(&store).unwrap();
        assert_eq!(status.state, "idle");
        assert_eq!(status.last_output_hash, report.output_hash);
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn provider_user_compaction_output_rejects_unknown_ids() {
        let workdir = temp_root("provider-user-compaction-unknown-id");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        service
            .write(MemoryWriteRequest {
                scope: "user".to_string(),
                subject: Some("Style".to_string()),
                text: "Prefer concise answers.".to_string(),
                tags: vec![],
            })
            .unwrap();

        let error = service
            .apply_user_compaction_output(UserMemoryCompactionOutput {
                entries: vec![UserMemoryCompactionOutputEntry {
                    id: Some("u_99".to_string()),
                    subject: Some("Style".to_string()),
                    text: "Prefer concise answers.".to_string(),
                    tags: vec![],
                }],
            })
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("unknown_user_memory_compaction_id"));
        assert_eq!(
            read_entries(&service.store(MemoryScope::User))
                .unwrap()
                .len(),
            1
        );
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn maintain_user_memory_retries_expired_hard_compaction() {
        let workdir = temp_root("maintain-user-memory-retry");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::with_options(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
            MemoryOptions {
                user_soft_threshold_bytes: 512,
                user_hard_threshold_bytes: 512,
                ..MemoryOptions::default()
            },
        );
        let store = service.store(MemoryScope::User);
        ensure_scope_dirs(&store).unwrap();
        let now = now_string();
        let entries = vec![MemoryEntry {
            id: "u_1".to_string(),
            scope: MemoryScope::User,
            subject: Some("Large".to_string()),
            text: format!("Large user memory {}", "detail ".repeat(80)),
            tags: vec!["prefs".to_string()],
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            source_conversation_id: Some("c1".to_string()),
            source_agent_id: None,
            source_session_type: Some("foreground".to_string()),
            state: MemoryState::Active,
        }];
        write_entries_and_manifest(&store, &entries).unwrap();
        write_user_compaction_status(
            &store,
            &UserMemoryCompactionStatus {
                state: "retry_waiting".to_string(),
                attempts: 0,
                last_error: Some("previous failure".to_string()),
                next_retry_at: Some((Utc::now() - ChronoDuration::seconds(60)).to_rfc3339()),
                last_input_hash: String::new(),
                last_output_hash: String::new(),
                threshold_override_bytes: None,
                last_soft_compaction_at: None,
                updated_at: Some(now_string()),
            },
        )
        .unwrap();

        service.maintain_user_memory().unwrap();

        let status = read_user_compaction_status(&store).unwrap();
        assert_eq!(status.state, "idle");
        assert_eq!(status.attempts, 2);
        assert!(status.threshold_override_bytes.is_some());
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn maintain_user_memory_respects_future_hard_retry() {
        let workdir = temp_root("maintain-user-memory-future-retry");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::with_options(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
            MemoryOptions {
                user_soft_threshold_bytes: 512,
                user_hard_threshold_bytes: 512,
                ..MemoryOptions::default()
            },
        );
        let store = service.store(MemoryScope::User);
        ensure_scope_dirs(&store).unwrap();
        let now = now_string();
        let entries = vec![MemoryEntry {
            id: "u_1".to_string(),
            scope: MemoryScope::User,
            subject: Some("Large".to_string()),
            text: format!("Large user memory {}", "detail ".repeat(80)),
            tags: vec!["prefs".to_string()],
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            source_conversation_id: Some("c1".to_string()),
            source_agent_id: None,
            source_session_type: Some("foreground".to_string()),
            state: MemoryState::Active,
        }];
        write_entries_and_manifest(&store, &entries).unwrap();
        write_user_compaction_status(
            &store,
            &UserMemoryCompactionStatus {
                state: "retry_waiting".to_string(),
                attempts: 1,
                last_error: Some("previous failure".to_string()),
                next_retry_at: Some((Utc::now() + ChronoDuration::seconds(60)).to_rfc3339()),
                last_input_hash: String::new(),
                last_output_hash: String::new(),
                threshold_override_bytes: None,
                last_soft_compaction_at: None,
                updated_at: Some(now_string()),
            },
        )
        .unwrap();

        service.maintain_user_memory().unwrap();

        let status = read_user_compaction_status(&store).unwrap();
        assert_eq!(status.state, "retry_waiting");
        assert_eq!(status.attempts, 1);
        assert!(status.next_retry_at.is_some());
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn memory_backend_prompt_context_renders_budgeted_block() {
        let workdir = temp_root("backend-prompt");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let service = MemoryService::new(
            workdir.clone(),
            conversation_root,
            MemorySource {
                conversation_id: "c1".to_string(),
                agent_id: None,
                session_type: "foreground".to_string(),
            },
        );
        let backend: &dyn MemoryBackend = &service;
        backend
            .write(MemoryWriteRequest {
                scope: "conversation".to_string(),
                subject: Some("Project A".to_string()),
                text: "Project A keeps memory behind explicit memory tools.".to_string(),
                tags: vec!["design".to_string()],
            })
            .unwrap();

        let block = backend
            .prompt_context(MemoryContextRequest {
                scope: MemoryScope::Conversation,
                max_bytes: 80,
            })
            .unwrap();
        assert_eq!(block.scope, MemoryScope::Conversation);
        assert!(block.text.contains("* [c_1]"));
        assert!(block.truncated);
        assert!(block.rendered_size_bytes > block.text.len());
        assert!(block.entries_hash.len() >= 40);
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn workdir_memory_client_serializes_user_and_public_writes() {
        let workdir = temp_root("client");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let client = WorkdirMemoryManager::start(workdir.clone(), MemoryOptions::default());
        let source = MemorySource {
            conversation_id: "c1".to_string(),
            agent_id: None,
            session_type: "foreground".to_string(),
        };
        client
            .execute(
                conversation_root.clone(),
                source.clone(),
                MemoryClientAction::Write(MemoryWriteRequest {
                    scope: "user".to_string(),
                    subject: Some("Language".to_string()),
                    text: "用户偏好中文沟通。".to_string(),
                    tags: vec![],
                }),
            )
            .unwrap();
        client
            .execute(
                conversation_root,
                source,
                MemoryClientAction::Write(MemoryWriteRequest {
                    scope: "public".to_string(),
                    subject: Some("Project A".to_string()),
                    text: "Project A is a durable cross-conversation fact.".to_string(),
                    tags: vec!["project".to_string()],
                }),
            )
            .unwrap();

        assert!(workdir.join("rundir/memory_v1/user/entries.jsonl").exists());
        assert!(workdir
            .join("rundir/memory_v1/public/entries.jsonl")
            .exists());
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_compaction_failure_sets_retry_waiting() {
        let workdir = temp_root("compaction-failure");
        let store = ScopeStore {
            scope: MemoryScope::User,
            root: workdir.join("rundir/memory_v1/user"),
        };
        let options = MemoryOptions {
            user_retry_after_failed_hard_compaction_secs: 3_600,
            ..MemoryOptions::default()
        };

        let status =
            record_user_compaction_failure(&store, &options, "provider unavailable").unwrap();

        assert_eq!(status.state, "retry_waiting");
        assert_eq!(status.attempts, 1);
        assert_eq!(status.last_error.as_deref(), Some("provider unavailable"));
        assert!(status.next_retry_at.is_some());
        let raw = fs::read_to_string(store.root.join("compaction.json")).unwrap();
        assert!(raw.contains("retry_waiting"));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_compaction_no_shrink_retries_once_then_sets_threshold_override() {
        let workdir = temp_root("compaction-no-shrink");
        let store = ScopeStore {
            scope: MemoryScope::User,
            root: workdir.join("rundir/memory_v1/user"),
        };
        let options = MemoryOptions {
            user_hard_threshold_bytes: 8_192,
            ..MemoryOptions::default()
        };

        let first = record_user_compaction_no_shrink(&store, &options, 9_000, 9_000).unwrap();
        assert_eq!(first.state, "hard_pending");
        assert_eq!(first.attempts, 1);
        assert!(first.threshold_override_bytes.is_none());

        let second = record_user_compaction_no_shrink(&store, &options, 9_000, 9_100).unwrap();
        assert_eq!(second.state, "idle");
        assert_eq!(second.attempts, 2);
        assert_eq!(second.threshold_override_bytes, Some(9_001));
        assert!(second
            .last_error
            .as_deref()
            .unwrap()
            .contains("compaction_output_not_smaller"));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_soft_compaction_records_daily_attempt_without_hard_retry() {
        let workdir = temp_root("soft-compaction-state");
        let store = ScopeStore {
            scope: MemoryScope::User,
            root: workdir.join("rundir/memory_v1/user"),
        };

        let status = record_user_soft_compaction_failure(&store, "provider unavailable").unwrap();

        assert_eq!(status.state, "dirty");
        assert_eq!(status.attempts, 0);
        assert_eq!(status.last_error.as_deref(), Some("provider unavailable"));
        assert!(status.next_retry_at.is_none());
        assert_eq!(
            status.last_soft_compaction_at.as_deref(),
            Some(today_string().as_str())
        );
        let raw = fs::read_to_string(store.root.join("compaction.json")).unwrap();
        assert!(raw.contains("last_soft_compaction_at"));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_soft_compaction_due_requires_provider_and_runs_once_per_day() {
        let workdir = temp_root("soft-compaction-due");
        let conversation_root = workdir.join("conversations").join("c1");
        fs::create_dir_all(&conversation_root).unwrap();
        let source = MemorySource {
            conversation_id: "c1".to_string(),
            agent_id: None,
            session_type: "foreground".to_string(),
        };
        let no_provider = MemoryService::with_options(
            workdir.clone(),
            conversation_root.clone(),
            source.clone(),
            MemoryOptions {
                user_compaction_model: None,
                ..MemoryOptions::default()
            },
        );
        let mut status = UserMemoryCompactionStatus::idle();
        status.state = "dirty".to_string();
        assert!(!no_provider.user_soft_compaction_due(&status));

        let with_provider = MemoryService::with_options(
            workdir.clone(),
            conversation_root,
            source,
            MemoryOptions {
                user_compaction_model: Some(test_chat_model_config()),
                user_soft_compaction_schedule: "daily".to_string(),
                ..MemoryOptions::default()
            },
        );
        assert!(with_provider.user_soft_compaction_due(&status));
        status.last_soft_compaction_at = Some(today_string());
        assert!(!with_provider.user_soft_compaction_due(&status));
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn user_compaction_success_resets_retry_state() {
        let workdir = temp_root("compaction-success");
        let store = ScopeStore {
            scope: MemoryScope::User,
            root: workdir.join("rundir/memory_v1/user"),
        };
        let options = MemoryOptions::default();
        record_user_compaction_failure(&store, &options, "temporary failure").unwrap();

        let status = record_user_compaction_success(&store, "output_hash").unwrap();

        assert_eq!(status.state, "idle");
        assert_eq!(status.attempts, 0);
        assert!(status.last_error.is_none());
        assert!(status.next_retry_at.is_none());
        assert_eq!(status.last_output_hash, "output_hash");
        let _ = fs::remove_dir_all(workdir);
    }
}
