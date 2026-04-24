use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, FileState, SessionInitial,
    SessionType, ToolRemoteMode,
};

use super::{WorkdirUpgrader, PARTYCLAW_LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_2};
use crate::{
    config::{SandboxConfig, SandboxMode, SessionProfile, StellaclawConfig},
    conversation::{ConversationSessionBinding, ConversationState},
    conversation_id_manager::ConversationIdManager,
    workspace::ensure_workspace_seed,
};

pub struct LegacyUpgrade;
pub struct PartyClawUpgrade;

impl WorkdirUpgrader for LegacyUpgrade {
    fn from_version(&self) -> &'static str {
        "0.1"
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_2
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        fs::create_dir_all(workdir.join("conversations")).with_context(|| {
            format!(
                "failed to create {}",
                workdir.join("conversations").display()
            )
        })?;
        fs::create_dir_all(workdir.join(".log").join("stellaclaw").join("channels")).with_context(
            || {
                format!(
                    "failed to create {}",
                    workdir.join(".log/stellaclaw/channels").display()
                )
            },
        )?;
        maybe_migrate_legacy_host_log(workdir)
    }
}

impl WorkdirUpgrader for PartyClawUpgrade {
    fn from_version(&self) -> &'static str {
        PARTYCLAW_LATEST_WORKDIR_VERSION
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_2
    }

    fn upgrade(&self, workdir: &Path, config: &StellaclawConfig) -> Result<()> {
        fs::create_dir_all(workdir.join("conversations")).with_context(|| {
            format!(
                "failed to create {}",
                workdir.join("conversations").display()
            )
        })?;
        fs::create_dir_all(workdir.join(".log").join("stellaclaw").join("channels")).with_context(
            || {
                format!(
                    "failed to create {}",
                    workdir.join(".log/stellaclaw/channels").display()
                )
            },
        )?;
        maybe_migrate_legacy_host_log(workdir)?;
        migrate_shared_runtime_assets(workdir)?;
        migrate_channel_authorizations(workdir)?;
        migrate_conversations_and_sessions(workdir, config)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyConversation {
    #[serde(default)]
    address: LegacyAddress,
    #[serde(default)]
    settings: LegacyConversationSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyConversationSettings {
    #[serde(default)]
    main_model: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    context_compaction_enabled: Option<bool>,
    #[serde(default)]
    remote_workpaths: Vec<Value>,
    #[serde(default)]
    local_mounts: Vec<Value>,
    #[serde(default)]
    remote_execution: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LegacyRemoteExecutionBinding {
    Local { path: PathBuf },
    Ssh { host: String, path: String },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
struct LegacyAddress {
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacySession {
    #[serde(default)]
    kind: LegacySessionKind,
    id: String,
    #[serde(default)]
    address: LegacyAddress,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    history: Vec<LegacySessionMessage>,
    #[serde(default)]
    turn_count: u64,
    #[serde(default)]
    last_user_message_at: Option<String>,
    #[serde(default)]
    last_agent_returned_at: Option<String>,
    #[serde(default)]
    skill_states: BTreeMap<String, LegacySkillState>,
    #[serde(default)]
    session_state: LegacyDurableSessionState,
    #[serde(default)]
    closed_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LegacySessionKind {
    #[default]
    Foreground,
    Background,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyDurableSessionState {
    #[serde(default)]
    prompt_components: BTreeMap<String, LegacyPromptComponentState>,
    #[serde(default)]
    actor_mailbox: Vec<Value>,
    #[serde(default)]
    user_mailbox: Vec<Value>,
    #[serde(default)]
    pending_messages: Vec<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct LegacyPromptComponentState {
    #[serde(default)]
    system_prompt_value: String,
    #[serde(default)]
    system_prompt_hash: String,
    #[serde(default)]
    notified_value: String,
    #[serde(default)]
    notified_hash: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct LegacySkillState {
    #[serde(default)]
    description: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    last_loaded_turn: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacySessionMessage {
    #[serde(default)]
    role: LegacyMessageRole,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    attachments: Vec<LegacyStoredAttachment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyMessageRole {
    #[default]
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyStoredAttachment {
    #[serde(default)]
    original_name: Option<String>,
    #[serde(default)]
    media_type: Option<String>,
    path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyChannelAuthStore {
    #[serde(default)]
    channels: HashMap<String, LegacyChannelAuthRecord>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyChannelAuthRecord {
    #[serde(default)]
    admin_user_id: Option<String>,
    #[serde(default)]
    conversations: HashMap<String, LegacyConversationApproval>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyConversationApproval {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    state: LegacyApprovalState,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyApprovalState {
    #[default]
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Serialize)]
struct MigratedSecurityState {
    admin_user_ids: Vec<i64>,
    chats: BTreeMap<String, MigratedChatAuthorization>,
}

#[derive(Debug, Serialize)]
struct MigratedChatAuthorization {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_user: Option<String>,
}

#[derive(Debug, Serialize)]
struct MigratedSessionState<'a> {
    version: u32,
    initial: &'a SessionInitial,
    all_messages: &'a [ChatMessage],
    current_messages: &'a [ChatMessage],
    next_turn_id: u64,
    next_batch_id: u64,
    runtime_metadata_state: MigratedRuntimeMetadataState<'a>,
}

#[derive(Debug, Serialize)]
struct MigratedRuntimeMetadataState<'a> {
    prompt_components: &'a BTreeMap<String, LegacyPromptComponentState>,
    skill_states: &'a BTreeMap<String, LegacySkillState>,
}

#[derive(Debug, Clone)]
struct LegacySessionEntry {
    session: LegacySession,
}

fn maybe_migrate_legacy_host_log(workdir: &Path) -> Result<()> {
    let legacy = workdir.join("logs").join("host.log");
    let current = workdir.join(".log").join("stellaclaw").join("host.log");
    if legacy.is_file() && !current.exists() {
        if let Some(parent) = current.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&legacy, &current).with_context(|| {
            format!(
                "failed to migrate legacy host log {} -> {}",
                legacy.display(),
                current.display()
            )
        })?;
    }
    Ok(())
}

fn migrate_shared_runtime_assets(workdir: &Path) -> Result<()> {
    let runtime_root = workdir.join("rundir");
    let runtime_profile_root = runtime_root.join(".stellaclaw");
    let runtime_skill_root = runtime_root.join(".skill");
    let runtime_skill_memory_root = runtime_root.join("skill_memory");
    let runtime_shared_root = runtime_root.join("shared");

    fs::create_dir_all(&runtime_profile_root)
        .with_context(|| format!("failed to create {}", runtime_profile_root.display()))?;
    fs::create_dir_all(&runtime_skill_root)
        .with_context(|| format!("failed to create {}", runtime_skill_root.display()))?;
    fs::create_dir_all(&runtime_skill_memory_root)
        .with_context(|| format!("failed to create {}", runtime_skill_memory_root.display()))?;
    fs::create_dir_all(&runtime_shared_root)
        .with_context(|| format!("failed to create {}", runtime_shared_root.display()))?;

    copy_file_if_missing(
        &workdir.join("agent").join("USER.md"),
        &runtime_profile_root.join("USER.md"),
    )?;
    copy_file_if_missing(
        &workdir.join("agent").join("IDENTITY.md"),
        &runtime_profile_root.join("IDENTITY.md"),
    )?;
    merge_directory_contents_if_missing(
        &workdir.join("rundir").join(".skills"),
        &runtime_skill_root,
    )?;
    merge_directory_contents_if_missing(
        &workdir.join("rundir").join("skill_memory"),
        &runtime_skill_memory_root,
    )?;
    merge_directory_contents_if_missing(
        &workdir.join("rundir").join("shared"),
        &runtime_shared_root,
    )?;
    Ok(())
}

fn migrate_channel_authorizations(workdir: &Path) -> Result<()> {
    let legacy_path = workdir.join("channel_auth").join("authorizations.json");
    if !legacy_path.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(&legacy_path)
        .with_context(|| format!("failed to read {}", legacy_path.display()))?;
    let legacy: LegacyChannelAuthStore = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", legacy_path.display()))?;

    for (channel_id, record) in legacy.channels {
        let mut admin_user_ids = Vec::new();
        if let Some(admin_user_id) = record.admin_user_id.as_deref() {
            admin_user_ids.push(admin_user_id.parse::<i64>().with_context(|| {
                format!(
                    "legacy channel {} has non-numeric admin_user_id '{}'",
                    channel_id, admin_user_id
                )
            })?);
        }
        let chats = record
            .conversations
            .into_iter()
            .map(|(conversation_id, item)| {
                (
                    conversation_id,
                    MigratedChatAuthorization {
                        state: match item.state {
                            LegacyApprovalState::Pending => "Pending",
                            LegacyApprovalState::Approved => "Approved",
                            LegacyApprovalState::Rejected => "Rejected",
                        },
                        last_title: None,
                        last_user: item.display_name,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let migrated = MigratedSecurityState {
            admin_user_ids,
            chats,
        };
        let path = workdir
            .join(".log")
            .join("stellaclaw")
            .join("channels")
            .join(&channel_id)
            .join("security.json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(
            &path,
            serde_json::to_string_pretty(&migrated)
                .context("failed to serialize migrated telegram security state")?,
        )
        .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn migrate_conversations_and_sessions(workdir: &Path, config: &StellaclawConfig) -> Result<()> {
    let conversations_root = workdir.join("conversations");
    let sessions_by_key = scan_legacy_sessions(&workdir.join("sessions"))?;
    let mut id_manager = ConversationIdManager::load_under(workdir).map_err(anyhow::Error::msg)?;

    for entry in fs::read_dir(&conversations_root)
        .with_context(|| format!("failed to read {}", conversations_root.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
        let legacy_root = entry.path();
        let legacy_state_path = legacy_root.join("conversation.json");
        if !legacy_state_path.is_file() {
            continue;
        }

        let Ok(raw) = fs::read_to_string(&legacy_state_path) else {
            continue;
        };
        let Ok(legacy) = serde_json::from_str::<LegacyConversation>(&raw) else {
            continue;
        };
        validate_legacy_conversation(&legacy, &legacy_state_path)?;

        let conversation_id = id_manager
            .get_or_create(&legacy.address.channel_id, &legacy.address.conversation_id)
            .map_err(anyhow::Error::msg)?;
        let conversation_root = conversations_root.join(&conversation_id);
        fs::create_dir_all(&conversation_root)
            .with_context(|| format!("failed to create {}", conversation_root.display()))?;

        let main_model = match legacy.settings.main_model.as_deref() {
            Some(name) => config
                .resolve_named_model(name)
                .or_else(|| config.initial_main_model())
                .ok_or_else(|| anyhow!("missing fallback main model"))?,
            None => config
                .initial_main_model()
                .ok_or_else(|| anyhow!("missing fallback main model"))?,
        };
        let session_profile = SessionProfile { main_model };
        let tool_remote_mode = migrated_tool_remote_mode(&legacy, &legacy_state_path)?;

        let session_key = format!(
            "{}::{}",
            legacy.address.channel_id, legacy.address.conversation_id
        );
        let legacy_session = sessions_by_key.get(&session_key);
        let workspace_id = legacy
            .settings
            .workspace_id
            .clone()
            .or_else(|| legacy_session.and_then(|entry| entry.session.workspace_id.clone()));

        if let Some(workspace_id) = workspace_id.as_deref() {
            migrate_workspace(workdir, workspace_id, &conversation_root)?;
        }
        ensure_workspace_seed(workdir, &conversation_root)?;

        let foreground_session_id = legacy_session
            .map(|entry| entry.session.id.clone())
            .unwrap_or_else(|| format!("{conversation_id}.foreground"));
        let conversation_state = ConversationState {
            version: 1,
            conversation_id: conversation_id.clone(),
            channel_id: legacy.address.channel_id.clone(),
            platform_chat_id: legacy.address.conversation_id.clone(),
            session_profile: session_profile.clone(),
            model_selection_pending: false,
            tool_remote_mode: tool_remote_mode.clone(),
            sandbox: migrated_conversation_sandbox(&legacy),
            reasoning_effort: legacy.settings.reasoning_effort.clone(),
            session_binding: ConversationSessionBinding {
                foreground_session_id: foreground_session_id.clone(),
                next_background_index: 1,
                next_subagent_index: 1,
                background_sessions: BTreeMap::new(),
                subagent_sessions: BTreeMap::new(),
            },
        };
        fs::write(
            conversation_root.join("conversation.json"),
            serde_json::to_string_pretty(&conversation_state)
                .context("failed to serialize migrated conversation state")?,
        )
        .with_context(|| {
            format!(
                "failed to write {}",
                conversation_root.join("conversation.json").display()
            )
        })?;

        if let Some(legacy_session) = legacy_session {
            migrate_foreground_session(
                workdir,
                config,
                &tool_remote_mode,
                legacy_session,
                &conversation_root,
            )?;
        }
    }

    Ok(())
}

fn validate_legacy_conversation(legacy: &LegacyConversation, path: &Path) -> Result<()> {
    let _ = (
        legacy.settings.remote_workpaths.len(),
        legacy.settings.local_mounts.len(),
        legacy.settings.context_compaction_enabled,
        path,
    );
    Ok(())
}

fn scan_legacy_sessions(root: &Path) -> Result<HashMap<String, LegacySessionEntry>> {
    let mut by_key = HashMap::new();
    if !root.exists() {
        return Ok(by_key);
    }
    for session_root in find_session_roots(root)? {
        let state_path = session_root.join("session.json");
        let Ok(raw) = fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<LegacySession>(&raw) else {
            continue;
        };
        if session.closed_at.is_some() || session.kind != LegacySessionKind::Foreground {
            continue;
        }
        validate_legacy_session(&session, &state_path)?;
        let key = format!(
            "{}::{}",
            session.address.channel_id, session.address.conversation_id
        );
        let replace = match by_key.get(&key) {
            Some(existing) => is_better_foreground_candidate(&session, &existing.session),
            None => true,
        };
        if replace {
            by_key.insert(key, LegacySessionEntry { session });
        }
    }
    Ok(by_key)
}

fn validate_legacy_session(session: &LegacySession, path: &Path) -> Result<()> {
    if !session.session_state.actor_mailbox.is_empty()
        || !session.session_state.user_mailbox.is_empty()
        || !session.session_state.pending_messages.is_empty()
    {
        return Err(anyhow!(
            "{} contains pending runtime state that stellaclaw cannot migrate losslessly yet",
            path.display()
        ));
    }
    Ok(())
}

fn is_better_foreground_candidate(left: &LegacySession, right: &LegacySession) -> bool {
    left.turn_count > right.turn_count
        || (left.turn_count == right.turn_count
            && left.last_agent_returned_at > right.last_agent_returned_at)
        || (left.turn_count == right.turn_count
            && left.last_agent_returned_at == right.last_agent_returned_at
            && left.last_user_message_at > right.last_user_message_at)
}

fn find_session_roots(root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    walk_session_roots(root, &mut roots)?;
    Ok(roots)
}

fn walk_session_roots(root: &Path, roots: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to enumerate {}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            if path.join("session.json").is_file() {
                roots.push(path);
            } else {
                walk_session_roots(&path, roots)?;
            }
        }
    }
    Ok(())
}

fn migrate_workspace(workdir: &Path, workspace_id: &str, conversation_root: &Path) -> Result<()> {
    let source_root = workdir.join("workspaces").join(workspace_id).join("files");
    if !source_root.is_dir() {
        return Ok(());
    }
    merge_directory_contents_if_missing(&source_root, conversation_root)?;

    let workspace_profile_root = conversation_root.join(".stellaclaw");
    fs::create_dir_all(&workspace_profile_root)
        .with_context(|| format!("failed to create {}", workspace_profile_root.display()))?;
    move_file_if_present(
        &conversation_root.join("USER.md"),
        &workspace_profile_root.join("USER.md"),
    )?;
    move_file_if_present(
        &conversation_root.join("IDENTITY.md"),
        &workspace_profile_root.join("IDENTITY.md"),
    )?;

    move_directory_if_present(
        &conversation_root.join(".skills"),
        &conversation_root.join(".skill"),
    )?;
    move_directory_if_present(
        &conversation_root.join("shared"),
        &workdir.join("rundir").join("shared"),
    )?;
    move_directory_if_present(
        &conversation_root.join("skill_memory"),
        &conversation_root.join(".skill_memory"),
    )?;
    Ok(())
}

fn migrate_foreground_session(
    workdir: &Path,
    config: &StellaclawConfig,
    tool_remote_mode: &ToolRemoteMode,
    legacy: &LegacySessionEntry,
    conversation_root: &Path,
) -> Result<()> {
    let workspace_root = legacy
        .session
        .workspace_id
        .as_deref()
        .map(|id| workdir.join("workspaces").join(id).join("files"));
    let history = legacy
        .session
        .history
        .iter()
        .filter_map(|message| {
            convert_legacy_session_message(message, workspace_root.as_deref(), conversation_root)
        })
        .collect::<Vec<_>>();
    let current_messages = truncate_current_messages_for_context(&history, config);
    let initial = SessionInitial {
        session_id: legacy.session.id.clone(),
        session_type: SessionType::Foreground,
        tool_remote_mode: tool_remote_mode.clone(),
        compression_threshold_tokens: config.session_defaults.compression_threshold_tokens,
        compression_retain_recent_tokens: config.session_defaults.compression_retain_recent_tokens,
        image_tool_model: config.session_defaults.image_tool_model.clone(),
        pdf_tool_model: config.session_defaults.pdf_tool_model.clone(),
        audio_tool_model: config.session_defaults.audio_tool_model.clone(),
        image_generation_tool_model: config.session_defaults.image_generation_tool_model.clone(),
        search_tool_model: config.session_defaults.search_tool_model.clone(),
    };
    let persisted = MigratedSessionState {
        version: 1,
        initial: &initial,
        all_messages: &history,
        current_messages: &current_messages,
        next_turn_id: legacy.session.turn_count.saturating_add(1).max(1),
        next_batch_id: 1,
        runtime_metadata_state: MigratedRuntimeMetadataState {
            prompt_components: &legacy.session.session_state.prompt_components,
            skill_states: &legacy.session.skill_states,
        },
    };
    let session_dir = conversation_root
        .join(".log")
        .join("stellaclaw")
        .join(sanitize_session_id(&legacy.session.id));
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("failed to create {}", session_dir.display()))?;
    fs::write(
        session_dir.join("session.json"),
        serde_json::to_string_pretty(&persisted)
            .context("failed to serialize migrated session state")?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            session_dir.join("session.json").display()
        )
    })?;
    write_messages_jsonl(&session_dir.join("all_messages.jsonl"), &history)?;
    write_messages_jsonl(
        &session_dir.join("current_messages.jsonl"),
        &current_messages,
    )?;

    Ok(())
}

fn truncate_current_messages_for_context(
    history: &[ChatMessage],
    config: &StellaclawConfig,
) -> Vec<ChatMessage> {
    let budget = config
        .session_defaults
        .compression_threshold_tokens
        .unwrap_or_else(|| {
            config
                .initial_main_model()
                .map(|model| model.token_max_context.saturating_mul(8) / 10)
                .unwrap_or(128_000)
        })
        .max(1) as usize;
    let mut retained = Vec::new();
    let mut total = 0usize;
    for message in history.iter().rev() {
        let estimate = estimate_message_tokens(message);
        if total.saturating_add(estimate) > budget && !retained.is_empty() {
            break;
        }
        total = total.saturating_add(estimate);
        retained.push(message.clone());
    }
    retained.reverse();
    retained
}

fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let mut chars = 16usize;
    if let Some(user_name) = &message.user_name {
        chars = chars.saturating_add(user_name.len());
    }
    if let Some(message_time) = &message.message_time {
        chars = chars.saturating_add(message_time.len());
    }
    for item in &message.data {
        chars = chars.saturating_add(match item {
            ChatMessageItem::Reasoning(reasoning) => reasoning.text.len(),
            ChatMessageItem::Context(context) => context.text.len(),
            ChatMessageItem::File(file) => estimate_file_item_chars(file),
            ChatMessageItem::ToolCall(tool_call) => {
                tool_call.tool_call_id.len()
                    + tool_call.tool_name.len()
                    + tool_call.arguments.text.len()
            }
            ChatMessageItem::ToolResult(tool_result) => {
                let context_len = tool_result
                    .result
                    .context
                    .as_ref()
                    .map(|context| context.text.len())
                    .unwrap_or(0);
                let file_len = tool_result
                    .result
                    .file
                    .as_ref()
                    .map(estimate_file_item_chars)
                    .unwrap_or(0);
                tool_result.tool_call_id.len()
                    + tool_result.tool_name.len()
                    + context_len
                    + file_len
            }
        });
    }
    (chars / 4).saturating_add(32).max(1)
}

fn estimate_file_item_chars(file: &FileItem) -> usize {
    let mut chars = file.uri.len();
    if let Some(name) = &file.name {
        chars = chars.saturating_add(name.len());
    }
    if let Some(media_type) = &file.media_type {
        chars = chars.saturating_add(media_type.len());
    }
    if let Some(FileState::Crashed { reason }) = &file.state {
        chars = chars.saturating_add(reason.len());
    }
    chars
}

fn migrated_tool_remote_mode(legacy: &LegacyConversation, _path: &Path) -> Result<ToolRemoteMode> {
    let Some(remote_execution) = legacy.settings.remote_execution.as_ref() else {
        return Ok(ToolRemoteMode::Selectable);
    };
    let Ok(binding) =
        serde_json::from_value::<LegacyRemoteExecutionBinding>(remote_execution.clone())
    else {
        return Ok(ToolRemoteMode::Selectable);
    };
    match binding {
        LegacyRemoteExecutionBinding::Ssh { host, path } => Ok(ToolRemoteMode::FixedSsh {
            host,
            cwd: Some(path),
        }),
        LegacyRemoteExecutionBinding::Local { .. } => Ok(ToolRemoteMode::Selectable),
    }
}

fn migrated_conversation_sandbox(legacy: &LegacyConversation) -> Option<SandboxConfig> {
    let mode = legacy.settings.sandbox_mode.as_deref()?;
    let mode = match mode {
        "subprocess" | "disabled" => SandboxMode::Subprocess,
        "bubblewrap" => SandboxMode::Bubblewrap,
        _ => return None,
    };
    Some(SandboxConfig {
        mode,
        ..SandboxConfig::default()
    })
}

fn convert_legacy_session_message(
    legacy: &LegacySessionMessage,
    legacy_workspace_root: Option<&Path>,
    conversation_root: &Path,
) -> Option<ChatMessage> {
    let role = match legacy.role {
        LegacyMessageRole::User => ChatRole::User,
        LegacyMessageRole::Assistant => ChatRole::Assistant,
        LegacyMessageRole::System => ChatRole::User,
    };
    let mut data = Vec::new();
    if let Some(text) = legacy
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        let text = if matches!(legacy.role, LegacyMessageRole::System) {
            format!("[Legacy system message]\n{text}")
        } else {
            text.to_string()
        };
        data.push(ChatMessageItem::Context(ContextItem { text }));
    }
    for attachment in &legacy.attachments {
        data.push(ChatMessageItem::File(remap_attachment(
            attachment,
            legacy_workspace_root,
            conversation_root,
        )));
    }
    (!data.is_empty()).then_some(ChatMessage::new(role, data))
}

fn remap_attachment(
    attachment: &LegacyStoredAttachment,
    legacy_workspace_root: Option<&Path>,
    conversation_root: &Path,
) -> FileItem {
    let remapped = legacy_workspace_root
        .and_then(|workspace_root| attachment.path.strip_prefix(workspace_root).ok())
        .map(|relative| conversation_root.join(relative));
    let path = remapped.unwrap_or_else(|| attachment.path.clone());
    if path.exists() {
        FileItem {
            uri: format!("file://{}", path.display()),
            name: attachment.original_name.clone().or_else(|| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .map(ToOwned::to_owned)
            }),
            media_type: attachment.media_type.clone(),
            width: None,
            height: None,
            state: None,
        }
    } else {
        FileItem {
            uri: format!("file://{}", path.display()),
            name: attachment.original_name.clone().or_else(|| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .map(ToOwned::to_owned)
            }),
            media_type: attachment.media_type.clone(),
            width: None,
            height: None,
            state: Some(FileState::Crashed {
                reason: format!("legacy attachment missing at {}", path.display()),
            }),
        }
    }
}

fn copy_file_if_missing(source: &Path, target: &Path) -> Result<()> {
    if !source.is_file() || target.exists() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn move_file_if_present(source: &Path, target: &Path) -> Result<()> {
    if !source.is_file() {
        return Ok(());
    }
    if target.exists() {
        fs::remove_file(source)
            .with_context(|| format!("failed to remove {}", source.display()))?;
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::rename(source, target).or_else(|_| {
        fs::copy(source, target).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                target.display()
            )
        })?;
        fs::remove_file(source).with_context(|| format!("failed to remove {}", source.display()))
    })
}

fn move_directory_if_present(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    merge_directory_contents_if_missing(source, target)?;
    remove_path_if_exists(source)
}

fn merge_directory_contents_if_missing(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if metadata.is_file() {
        copy_file_if_missing(source, target)?;
        return Ok(());
    }
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?;
        if file_type.is_dir() {
            merge_directory_contents_if_missing(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            copy_symlink_if_missing(&source_path, &target_path)?;
        } else {
            copy_file_if_missing(&source_path, &target_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink_if_missing(source: &Path, target: &Path) -> Result<()> {
    if fs::symlink_metadata(target).is_ok() {
        return Ok(());
    }
    let link = match fs::read_link(source) {
        Ok(link) => link,
        Err(_) => return Ok(()),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::os::unix::fs::symlink(&link, target).with_context(|| {
        format!(
            "failed to copy symlink {} to {}",
            source.display(),
            target.display()
        )
    })
}

#[cfg(not(unix))]
fn copy_symlink_if_missing(_source: &Path, _target: &Path) -> Result<()> {
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn write_messages_jsonl(path: &Path, messages: &[ChatMessage]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    for message in messages {
        writeln!(
            file,
            "{}",
            serde_json::to_string(message).context("failed to serialize message")?
        )
        .with_context(|| format!("failed to write {}", path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", path.display()))
}

fn sanitize_session_id(session_id: &str) -> String {
    let safe = session_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() || safe == "." || safe == ".." {
        "session".to_string()
    } else {
        safe
    }
}
