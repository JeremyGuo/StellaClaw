use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::ToolRemoteMode;

pub(crate) const IDENTITY_PROMPT_COMPONENT: &str = "identity";
pub(crate) const REMOTE_ALIASES_PROMPT_COMPONENT: &str = "ssh_remote_aliases";
pub(crate) const REMOTE_WORKSPACE_PROMPT_COMPONENT: &str = "remote_workspace";
pub(crate) const SKILLS_METADATA_PROMPT_COMPONENT: &str = "skills_metadata";
pub(crate) const USER_META_PROMPT_COMPONENT: &str = "user_meta";
pub(crate) const USER_MEMORY_PROMPT_COMPONENT: &str = "user_memory";

const USER_META_PATH: &str = ".stellaclaw/USER.md";
const IDENTITY_PATH: &str = ".stellaclaw/IDENTITY.md";
const SKILL_ROOT: &str = ".stellaclaw/skill";
const SKILL_ENTRY_FILE: &str = "SKILL.md";
const USER_MEMORY_ENTRIES_PATH: &str = "rundir/memory_v1/user/entries.jsonl";
const USER_MEMORY_ENTRY_MAX_CHARS: usize = 700;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceRuntimeMetadata {
    pub identity: String,
    pub user_meta: String,
    pub user_memory: String,
    pub skills_metadata: String,
    pub skills: Vec<SessionSkillObservation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSkillObservation {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionSkillState {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub last_loaded_turn: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionPromptComponentState {
    #[serde(default)]
    pub system_prompt_value: String,
    #[serde(default)]
    pub system_prompt_hash: String,
    #[serde(default)]
    pub notified_value: String,
    #[serde(default)]
    pub notified_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeMetadataState {
    #[serde(default)]
    pub prompt_components: BTreeMap<String, SessionPromptComponentState>,
    #[serde(default)]
    pub skill_states: BTreeMap<String, SessionSkillState>,
}

impl RuntimeMetadataState {
    pub(crate) fn initialize_from_workspace(
        &mut self,
        workspace_root: &Path,
        data_root: &Path,
        remote_aliases_prompt: String,
        remote_workspace_prompt: String,
        memory_enabled: bool,
    ) -> Result<(), String> {
        let metadata = load_workspace_runtime_metadata(workspace_root, data_root)?;
        initialize_component(
            &mut self.prompt_components,
            IDENTITY_PROMPT_COMPONENT,
            metadata.identity,
        );
        initialize_component(
            &mut self.prompt_components,
            USER_META_PROMPT_COMPONENT,
            metadata.user_meta,
        );
        if memory_enabled {
            initialize_component(
                &mut self.prompt_components,
                USER_MEMORY_PROMPT_COMPONENT,
                metadata.user_memory,
            );
        } else {
            self.prompt_components.remove(USER_MEMORY_PROMPT_COMPONENT);
        }
        initialize_component(
            &mut self.prompt_components,
            SKILLS_METADATA_PROMPT_COMPONENT,
            metadata.skills_metadata,
        );

        for skill in metadata.skills {
            self.skill_states
                .entry(skill.name)
                .or_insert_with(|| SessionSkillState {
                    description: skill.description,
                    content: skill.content,
                    last_loaded_turn: None,
                });
        }

        initialize_component(
            &mut self.prompt_components,
            REMOTE_ALIASES_PROMPT_COMPONENT,
            remote_aliases_prompt,
        );
        initialize_component(
            &mut self.prompt_components,
            REMOTE_WORKSPACE_PROMPT_COMPONENT,
            remote_workspace_prompt,
        );
        Ok(())
    }

    pub(crate) fn observe_for_user_turn_from_workspace(
        &mut self,
        workspace_root: &Path,
        data_root: &Path,
        remote_aliases_prompt: String,
        remote_workspace_prompt: String,
        memory_enabled: bool,
    ) -> Result<Vec<String>, String> {
        let metadata = load_workspace_runtime_metadata(workspace_root, data_root)?;
        let mut prompt_notices = Vec::new();
        observe_component(
            &mut self.prompt_components,
            IDENTITY_PROMPT_COMPONENT,
            metadata.identity,
            &mut prompt_notices,
        );
        observe_component(
            &mut self.prompt_components,
            USER_META_PROMPT_COMPONENT,
            metadata.user_meta,
            &mut prompt_notices,
        );
        if memory_enabled {
            observe_component(
                &mut self.prompt_components,
                USER_MEMORY_PROMPT_COMPONENT,
                metadata.user_memory,
                &mut prompt_notices,
            );
        } else {
            self.prompt_components.remove(USER_MEMORY_PROMPT_COMPONENT);
        }
        observe_component(
            &mut self.prompt_components,
            SKILLS_METADATA_PROMPT_COMPONENT,
            metadata.skills_metadata,
            &mut prompt_notices,
        );
        observe_component(
            &mut self.prompt_components,
            REMOTE_ALIASES_PROMPT_COMPONENT,
            remote_aliases_prompt,
            &mut prompt_notices,
        );
        observe_component(
            &mut self.prompt_components,
            REMOTE_WORKSPACE_PROMPT_COMPONENT,
            remote_workspace_prompt,
            &mut prompt_notices,
        );

        let skill_notices = self.observe_skill_changes(&metadata.skills);
        let mut rendered = Vec::new();
        let prompt_text = render_prompt_component_change_notices(&prompt_notices);
        if !prompt_text.is_empty() {
            rendered.push(prompt_text);
        }
        let skill_text = render_skill_change_notices(&skill_notices);
        if !skill_text.is_empty() {
            rendered.push(skill_text);
        }
        Ok(rendered)
    }

    pub(crate) fn mark_loaded_skills(&mut self, skill_names: &[String], turn_number: u64) {
        for skill_name in skill_names {
            self.skill_states
                .entry(skill_name.clone())
                .or_default()
                .last_loaded_turn = Some(turn_number);
        }
    }

    pub(crate) fn initialize_missing_component(&mut self, key: &str, value: String) {
        initialize_component(&mut self.prompt_components, key, value);
    }

    pub(crate) fn promote_notified_components_to_system_snapshot(&mut self) {
        for state in self.prompt_components.values_mut() {
            if state.notified_hash.is_empty() && state.notified_value.is_empty() {
                continue;
            }
            state.system_prompt_value = state.notified_value.clone();
            state.system_prompt_hash = state.notified_hash.clone();
        }
    }

    pub(crate) fn snapshot_value(&self, key: &str) -> Option<&str> {
        let value = self.prompt_components.get(key)?.system_prompt_value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    fn observe_skill_changes(
        &mut self,
        observed_skills: &[SessionSkillObservation],
    ) -> Vec<SkillChangeNotice> {
        let mut notices = Vec::new();
        let observed_names = observed_skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<BTreeSet<_>>();

        for observed in observed_skills {
            match self.skill_states.get_mut(&observed.name) {
                Some(state) => {
                    if state.description != observed.description {
                        notices.push(SkillChangeNotice::DescriptionChanged {
                            name: observed.name.clone(),
                            description: observed.description.clone(),
                        });
                    }
                    if state.content != observed.content && state.last_loaded_turn.is_some() {
                        notices.push(SkillChangeNotice::ContentChanged {
                            name: observed.name.clone(),
                            description: observed.description.clone(),
                            content: observed.content.clone(),
                        });
                    }
                    state.description = observed.description.clone();
                    state.content = observed.content.clone();
                }
                None => {
                    self.skill_states.insert(
                        observed.name.clone(),
                        SessionSkillState {
                            description: observed.description.clone(),
                            content: observed.content.clone(),
                            last_loaded_turn: None,
                        },
                    );
                }
            }
        }

        self.skill_states
            .retain(|name, _| observed_names.contains(name));

        notices
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptComponentChangeNotice {
    key: String,
    previous_value: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillChangeNotice {
    DescriptionChanged {
        name: String,
        description: String,
    },
    ContentChanged {
        name: String,
        description: String,
        content: String,
    },
}

pub(crate) fn load_workspace_runtime_metadata(
    _workspace_root: &Path,
    data_root: &Path,
) -> Result<WorkspaceRuntimeMetadata, String> {
    Ok(WorkspaceRuntimeMetadata {
        identity: read_optional_workspace_file(data_root, IDENTITY_PATH)?,
        user_meta: load_user_meta_snapshot(data_root)?,
        user_memory: load_user_memory_snapshot(data_root)?,
        skills: load_workspace_skills(data_root)?,
        skills_metadata: String::new(),
    })
    .map(|mut metadata| {
        metadata.skills_metadata = render_skills_metadata(&metadata.skills);
        metadata
    })
}

fn read_optional_workspace_file(
    workspace_root: &Path,
    relative_path: &str,
) -> Result<String, String> {
    let path = workspace_root.join(relative_path);
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(&path)
        .map(|content| content.trim().to_string())
        .map_err(|error| format!("failed to read {}: {error}", path.display()))
}

fn load_user_meta_snapshot(data_root: &Path) -> Result<String, String> {
    let path = data_root.join(USER_META_PATH);
    if !path.exists() {
        return Ok(String::new());
    }
    let raw =
        fs::read(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let hash = prompt_component_hash(&String::from_utf8_lossy(&raw));
    Ok(format!(
        "Profile metadata file: {USER_META_PATH}\nstatus: present\nbytes: {}\nhash: {hash}\nInspect {USER_META_PATH} with shell_exec when exact profile details are needed.",
        raw.len()
    ))
}

fn load_user_memory_snapshot(data_root: &Path) -> Result<String, String> {
    let Some(workdir_root) = discover_workdir_root(data_root) else {
        return Ok(String::new());
    };
    let path = workdir_root.join(USER_MEMORY_ENTRIES_PATH);
    if !path.exists() {
        return Ok(String::new());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut lines = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
        if value.get("state").and_then(serde_json::Value::as_str) != Some("active") {
            continue;
        }
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let subject = value
            .get("subject")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" ({value})"))
            .unwrap_or_default();
        let text = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        if text.is_empty() {
            continue;
        }
        let mut rendered = format!(
            "* [{id}]{subject} {}",
            truncate_user_memory_text(text, USER_MEMORY_ENTRY_MAX_CHARS).replace('\n', " ")
        );
        let tags = value
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .take(4)
                    .map(|tag| format!("#{tag}"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !tags.is_empty() {
            rendered.push(' ');
            rendered.push_str(&tags.join(" "));
        }
        lines.push(rendered);
    }
    Ok(lines.join("\n"))
}

fn discover_workdir_root(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join(USER_MEMORY_ENTRIES_PATH).exists() {
            return Some(candidate.to_path_buf());
        }
        if candidate.join("rundir").exists() && candidate.join("conversations").exists() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn truncate_user_memory_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut output = trimmed.chars().take(max_chars).collect::<String>();
    output.push_str("...");
    output
}

fn load_workspace_skills(workspace_root: &Path) -> Result<Vec<SessionSkillObservation>, String> {
    let skill_root = workspace_root.join(SKILL_ROOT);
    if !skill_root.exists() {
        return Ok(Vec::new());
    }
    let mut skills = Vec::new();
    let entries = fs::read_dir(&skill_root)
        .map_err(|error| format!("failed to read {}: {error}", skill_root.display()))?;

    let mut by_name = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to enumerate skill directories under {}: {error}",
                skill_root.display()
            )
        })?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().trim().to_string();
        if name.is_empty() {
            continue;
        }
        by_name.insert(name, entry.path());
    }

    for (name, path) in by_name {
        let skill_path = path.join(SKILL_ENTRY_FILE);
        if !skill_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&skill_path)
            .map_err(|error| format!("failed to read {}: {error}", skill_path.display()))?;
        skills.push(SessionSkillObservation {
            name,
            description: infer_skill_description(&content),
            content,
        });
    }

    Ok(skills)
}

fn infer_skill_description(content: &str) -> String {
    if let Some(frontmatter) = extract_yaml_frontmatter(content) {
        if let Some(description) = frontmatter_scalar(frontmatter, "description") {
            return description;
        }
    }

    let mut paragraph = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with('#') && paragraph.is_empty() {
            continue;
        }
        paragraph.push(trimmed);
    }
    paragraph.join(" ").trim().to_string()
}

fn extract_yaml_frontmatter(content: &str) -> Option<&str> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    let body_start = 4;
    let end = content[body_start..].find("\n---")?;
    Some(&content[body_start..body_start + end])
}

fn frontmatter_scalar(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].trim();
        let Some(value) = line.strip_prefix(&prefix) else {
            index += 1;
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if value == "|" || value == ">" || value.starts_with("|-") || value.starts_with(">-") {
            let folded = value.starts_with('>');
            let mut block = Vec::new();
            for next in lines.iter().skip(index + 1) {
                if !next.trim().is_empty() && !next.starts_with(char::is_whitespace) {
                    break;
                }
                let trimmed = next.trim();
                if !trimmed.is_empty() {
                    block.push(trimmed);
                }
            }
            let joined = if folded {
                block.join(" ")
            } else {
                block.join("\n")
            };
            let joined = joined.trim().to_string();
            return (!joined.is_empty()).then_some(joined);
        }
        return Some(unquote_yaml_scalar(value));
    }
    None
}

fn unquote_yaml_scalar(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
        {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn render_skills_metadata(skills: &[SessionSkillObservation]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "Available skills from .stellaclaw/skill/ in the current workspace:".to_string(),
        "Load a skill by exact name before relying on its detailed instructions.".to_string(),
    ];
    for skill in skills {
        if skill.description.trim().is_empty() {
            lines.push(format!("- {}", skill.name));
        } else {
            lines.push(format!("- {}: {}", skill.name, skill.description.trim()));
        }
    }
    lines.join("\n")
}

fn initialize_component(
    components: &mut BTreeMap<String, SessionPromptComponentState>,
    key: &str,
    value: String,
) {
    let state = components.entry(key.to_string()).or_default();
    if state.system_prompt_hash.is_empty()
        && state.system_prompt_value.is_empty()
        && state.notified_hash.is_empty()
        && state.notified_value.is_empty()
    {
        let hash = prompt_component_hash(&value);
        state.system_prompt_value = value.clone();
        state.system_prompt_hash = hash.clone();
        state.notified_value = value;
        state.notified_hash = hash;
    }
}

fn observe_component(
    components: &mut BTreeMap<String, SessionPromptComponentState>,
    key: &str,
    value: String,
    notices: &mut Vec<PromptComponentChangeNotice>,
) {
    let hash = prompt_component_hash(&value);
    let state = components.entry(key.to_string()).or_default();
    if state.system_prompt_hash.is_empty()
        && state.system_prompt_value.is_empty()
        && state.notified_hash.is_empty()
        && state.notified_value.is_empty()
    {
        state.system_prompt_value = value.clone();
        state.system_prompt_hash = hash.clone();
        state.notified_value = value;
        state.notified_hash = hash;
        return;
    }

    if state.notified_hash != hash {
        let previous_value = state.notified_value.clone();
        state.notified_value = value.clone();
        state.notified_hash = hash;
        notices.push(PromptComponentChangeNotice {
            key: key.to_string(),
            previous_value,
            value,
        });
    }
}

fn prompt_component_hash(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn render_prompt_component_change_notices(notices: &[PromptComponentChangeNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut sections = vec![
        "[Runtime Prompt Updates]".to_string(),
        "Some durable profile context changed since the current canonical system prompt snapshot. Apply these updates for this user turn; they will be folded into the canonical system prompt after compaction.".to_string(),
    ];
    for notice in notices {
        match notice.key.as_str() {
            IDENTITY_PROMPT_COMPONENT => {
                if notice.value.trim().is_empty() {
                    sections.push(
                        "Identity is now empty. Ignore earlier Identity prompt content."
                            .to_string(),
                    );
                } else {
                    sections.push(format!(
                        "Identity changed. Treat this refreshed identity as authoritative for this turn:\n{}",
                        notice.value.trim()
                    ));
                }
            }
            USER_META_PROMPT_COMPONENT => {
                if notice.value.trim().is_empty() {
                    sections.push(
                        "User metadata file is now absent. Ignore earlier USER.md metadata."
                            .to_string(),
                    );
                } else {
                    sections.push(format!(
                        "USER.md metadata changed. The full file content is not included in this notice; inspect .stellaclaw/USER.md with shell_exec if exact profile details are needed. Refreshed metadata:\n{}",
                        notice.value.trim()
                    ));
                }
            }
            USER_MEMORY_PROMPT_COMPONENT => {
                if notice.value.trim().is_empty() {
                    sections.push(
                        "User memory is now empty. Apply this diff:\n".to_string()
                            + &render_prompt_component_line_diff(&notice.previous_value, ""),
                    );
                } else {
                    sections.push(format!(
                        "User memory changed. Apply this diff to the current User Memory snapshot for this turn:\n{}",
                        render_prompt_component_line_diff(&notice.previous_value, &notice.value)
                    ));
                }
            }
            SKILLS_METADATA_PROMPT_COMPONENT => {
                sections.push(format!(
                    "The available skill metadata changed. Treat this refreshed metadata as authoritative for this turn:\n{}",
                    notice.value.trim()
                ));
            }
            REMOTE_ALIASES_PROMPT_COMPONENT => {
                sections.push(format!(
                    "The available SSH remote alias list changed. Treat this refreshed list as authoritative for remote tool calls in this turn:\n{}",
                    notice.value.trim()
                ));
            }
            REMOTE_WORKSPACE_PROMPT_COMPONENT => {
                if notice.value.trim().is_empty() {
                    sections.push(
                        "Remote workspace instructions are now absent. Ignore earlier remote workspace instruction snapshots."
                            .to_string(),
                    );
                } else {
                    sections.push(format!(
                        "Remote workspace instructions changed. Treat this refreshed remote workspace snapshot as authoritative for this turn:\n{}",
                        notice.value.trim()
                    ));
                }
            }
            key => {
                sections.push(format!(
                    "Prompt component `{key}` changed. Treat this refreshed value as authoritative for this turn:\n{}",
                    notice.value.trim()
                ));
            }
        }
    }
    sections.join("\n\n")
}

fn render_prompt_component_line_diff(previous: &str, current: &str) -> String {
    let previous_lines = previous
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<BTreeSet<_>>();
    let current_lines = current
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<BTreeSet<_>>();
    let mut lines = vec!["--- previous".to_string(), "+++ current".to_string()];
    for removed in previous_lines.difference(&current_lines) {
        lines.push(format!("- {removed}"));
    }
    for added in current_lines.difference(&previous_lines) {
        lines.push(format!("+ {added}"));
    }
    if lines.len() == 2 {
        lines.push(" unchanged".to_string());
    }
    lines.join("\n")
}

fn render_skill_change_notices(notices: &[SkillChangeNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut sections = vec![
        "[Runtime Skill Updates]".to_string(),
        "The global skill registry changed since earlier in this session. Apply these updates before handling the user's new request.".to_string(),
    ];
    for notice in notices {
        match notice {
            SkillChangeNotice::DescriptionChanged { name, description } => {
                sections.push(format!(
                    "Skill \"{name}\" has an updated description:\n{description}"
                ));
            }
            SkillChangeNotice::ContentChanged {
                name,
                description,
                content,
            } => {
                sections.push(format!(
                    "Skill \"{name}\" changed after it was loaded earlier in this session. Use the refreshed skill immediately.\nUpdated description: {description}\nRefreshed SKILL.md content:\n{content}"
                ));
            }
        }
    }
    sections.join("\n\n")
}

pub(crate) fn remote_aliases_prompt_for_mode(remote_mode: &ToolRemoteMode) -> String {
    match remote_mode {
        ToolRemoteMode::Selectable => current_ssh_remote_aliases_prompt(),
        ToolRemoteMode::FixedSsh { .. } => String::new(),
    }
}

fn current_ssh_remote_aliases_prompt() -> String {
    let aliases = discover_ssh_remote_aliases();
    render_ssh_remote_aliases_for_prompt(&aliases)
}

fn render_ssh_remote_aliases_for_prompt(aliases: &[String]) -> String {
    if aliases.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "Available SSH remote aliases from ~/.ssh/config:".to_string(),
        "Use these exact Host aliases in tool `remote` arguments. Do not invent remote aliases."
            .to_string(),
    ];
    for alias in aliases {
        lines.push(format!("- `{alias}`"));
    }
    lines.join("\n")
}

fn discover_ssh_remote_aliases() -> Vec<String> {
    let Some(config_path) = default_ssh_config_path() else {
        return Vec::new();
    };
    discover_ssh_remote_aliases_from_path(&config_path)
}

fn default_ssh_config_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("AGENT_HOST_SSH_CONFIG") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".ssh").join("config"))
}

fn discover_ssh_remote_aliases_from_path(config_path: &Path) -> Vec<String> {
    let mut aliases = BTreeSet::new();
    let mut visited = BTreeSet::new();
    collect_ssh_config_aliases(config_path, &mut visited, &mut aliases, 0);
    aliases.into_iter().collect()
}

fn collect_ssh_config_aliases(
    config_path: &Path,
    visited: &mut BTreeSet<PathBuf>,
    aliases: &mut BTreeSet<String>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(config_path) = config_path.canonicalize() else {
        return;
    };
    if !visited.insert(config_path.clone()) {
        return;
    }
    let Ok(content) = fs::read_to_string(&config_path) else {
        return;
    };
    aliases.extend(parse_ssh_config_aliases(&content));
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    for include in parse_ssh_config_includes(&content) {
        let include_path = expand_tilde_path(&include);
        let include_path = if include_path.is_absolute() {
            include_path
        } else {
            base_dir.join(include_path)
        };
        collect_ssh_config_aliases(&include_path, visited, aliases, depth + 1);
    }
}

fn parse_ssh_config_aliases(content: &str) -> Vec<String> {
    let mut aliases = BTreeSet::new();
    for line in content.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("Host") {
            continue;
        }
        for alias in parts {
            if is_concrete_ssh_alias(alias) {
                aliases.insert(alias.to_string());
            }
        }
    }
    aliases.into_iter().collect()
}

fn parse_ssh_config_includes(content: &str) -> Vec<String> {
    let mut includes = Vec::new();
    for line in content.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("Include") {
            includes.extend(parts.map(ToOwned::to_owned));
        }
    }
    includes
}

fn is_concrete_ssh_alias(alias: &str) -> bool {
    !alias.starts_with('!')
        && !alias.contains('*')
        && !alias.contains('?')
        && !alias.contains('%')
        && !alias.trim().is_empty()
        && !alias.chars().any(char::is_whitespace)
}

fn expand_tilde_path(raw: &str) -> PathBuf {
    if raw == "~" {
        return env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return env::var("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|_| PathBuf::from(raw));
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::session_actor::ToolRemoteMode;

    fn temp_root() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_ID.fetch_add(1, AtomicOrdering::Relaxed);
        std::env::temp_dir().join(format!(
            "stellaclaw_runtime_metadata_{}_{}_{}",
            std::process::id(),
            id,
            sequence
        ))
    }

    #[test]
    fn detects_prompt_component_change_once() {
        let root = temp_root();
        fs::create_dir_all(root.join(".stellaclaw")).unwrap();
        fs::write(root.join(".stellaclaw/USER.md"), "tier: pro").unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("initial metadata should load");

        fs::write(root.join(".stellaclaw/USER.md"), "tier: enterprise").unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("changed metadata should load");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("[Runtime Prompt Updates]"));
        assert!(notices[0].contains("USER.md metadata changed"));
        assert!(notices[0].contains(".stellaclaw/USER.md"));
        assert!(!notices[0].contains("tier: enterprise"));

        assert!(state
            .observe_for_user_turn_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("metadata should still load")
            .is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn user_memory_change_notice_uses_diff() {
        let root = temp_root();
        let memory_dir = root.join("rundir/memory_v1/user");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(
            memory_dir.join("entries.jsonl"),
            r#"{"id":"u_1","scope":"user","text":"用户偏好中文沟通。","created_at":"t","updated_at":"t","state":"active"}"#,
        )
        .unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, &root, String::new(), String::new(), true)
            .expect("initial metadata should load");

        fs::write(
            memory_dir.join("entries.jsonl"),
            r#"{"id":"u_1","scope":"user","text":"用户偏好中文沟通。","created_at":"t","updated_at":"t","state":"active"}
{"id":"u_2","scope":"user","subject":"Style","text":"用户偏好直接、少废话的工程答复。","tags":["style"],"created_at":"t","updated_at":"t","state":"active"}"#,
        )
        .unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, &root, String::new(), String::new(), true)
            .expect("changed metadata should load");

        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("User memory changed. Apply this diff"));
        assert!(notices[0].contains("+++ current"));
        assert!(notices[0].contains("+ * [u_2] (Style)"));
        assert!(!notices[0].contains("Treat this refreshed user memory as authoritative"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_memory_does_not_observe_user_memory_notices() {
        let root = temp_root();
        let memory_dir = root.join("rundir/memory_v1/user");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(
            memory_dir.join("entries.jsonl"),
            r#"{"id":"u_1","scope":"user","text":"用户偏好中文沟通。","created_at":"t","updated_at":"t","state":"active"}"#,
        )
        .unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("initial metadata should load");
        assert!(!state
            .prompt_components
            .contains_key(USER_MEMORY_PROMPT_COMPONENT));

        fs::write(
            memory_dir.join("entries.jsonl"),
            r#"{"id":"u_2","scope":"user","text":"用户偏好直接答复。","created_at":"t","updated_at":"t","state":"active"}"#,
        )
        .unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("changed metadata should load");
        assert!(notices.is_empty());
        assert!(!state
            .prompt_components
            .contains_key(USER_MEMORY_PROMPT_COMPONENT));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reports_loaded_skill_content_change() {
        let root = temp_root();
        fs::create_dir_all(root.join(".stellaclaw/skill/demo")).unwrap();
        fs::write(
            root.join(".stellaclaw/skill/demo/SKILL.md"),
            "# Demo\n\nold body",
        )
        .unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("initial metadata should load");
        state.mark_loaded_skills(&["demo".to_string()], 1);

        fs::write(
            root.join(".stellaclaw/skill/demo/SKILL.md"),
            "# Demo\n\nnew body",
        )
        .unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, &root, String::new(), String::new(), false)
            .expect("changed metadata should load");
        assert!(notices
            .iter()
            .any(|notice| notice.contains("[Runtime Skill Updates]")));
        assert!(notices.iter().any(|notice| notice.contains("new body")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_aliases_prompt_only_exists_for_selectable_mode() {
        std::env::set_var("AGENT_HOST_SSH_CONFIG", "/path/that/does/not/exist");

        assert_eq!(
            remote_aliases_prompt_for_mode(&ToolRemoteMode::Selectable),
            String::new()
        );
        assert_eq!(
            remote_aliases_prompt_for_mode(&ToolRemoteMode::FixedSsh {
                host: "demo".to_string(),
                cwd: Some("/work".to_string()),
            }),
            String::new()
        );
    }

    #[test]
    fn renders_skills_metadata_from_workspace_skills() {
        let root = temp_root();
        fs::create_dir_all(root.join(".stellaclaw/skill/example")).unwrap();
        fs::write(
            root.join(".stellaclaw/skill/example/SKILL.md"),
            "# Example\n\nDo the important thing.",
        )
        .unwrap();

        let metadata = load_workspace_runtime_metadata(&root, &root).expect("metadata should load");

        assert_eq!(metadata.skills.len(), 1);
        assert_eq!(metadata.skills[0].name, "example");
        assert_eq!(metadata.skills[0].description, "Do the important thing.");
        assert!(metadata
            .skills_metadata
            .contains("- example: Do the important thing."));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn renders_skills_metadata_from_yaml_frontmatter_description() {
        let root = temp_root();
        fs::create_dir_all(root.join(".stellaclaw/skill/example")).unwrap();
        fs::write(
            root.join(".stellaclaw/skill/example/SKILL.md"),
            "---\nname: example\ndescription: >\n  Do the important thing\n  with care.\n---\n\n# Example\n\nBody.",
        )
        .unwrap();

        let metadata = load_workspace_runtime_metadata(&root, &root).expect("metadata should load");

        assert_eq!(metadata.skills.len(), 1);
        assert_eq!(
            metadata.skills[0].description,
            "Do the important thing with care."
        );
        assert!(metadata
            .skills_metadata
            .contains("- example: Do the important thing with care."));

        let _ = fs::remove_dir_all(root);
    }
}
