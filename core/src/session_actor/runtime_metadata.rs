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
pub(crate) const SKILLS_METADATA_PROMPT_COMPONENT: &str = "skills_metadata";
pub(crate) const STELLACLAW_MEMORY_PROMPT_COMPONENT: &str = "stellaclaw_memory";
pub(crate) const USER_META_PROMPT_COMPONENT: &str = "user_meta";

const USER_META_PATH: &str = ".stellaclaw/USER.md";
const IDENTITY_PATH: &str = ".stellaclaw/IDENTITY.md";
const STELLACLAW_MEMORY_PATH: &str = "STELLACLAW.md";
const SKILL_ROOT: &str = ".skill";
const SKILL_ENTRY_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceRuntimeMetadata {
    pub identity: String,
    pub stellaclaw_memory: String,
    pub user_meta: String,
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
        remote_aliases_prompt: String,
    ) -> Result<(), String> {
        let metadata = load_workspace_runtime_metadata(workspace_root)?;
        initialize_component(
            &mut self.prompt_components,
            IDENTITY_PROMPT_COMPONENT,
            metadata.identity,
        );
        initialize_component(
            &mut self.prompt_components,
            STELLACLAW_MEMORY_PROMPT_COMPONENT,
            metadata.stellaclaw_memory,
        );
        initialize_component(
            &mut self.prompt_components,
            USER_META_PROMPT_COMPONENT,
            metadata.user_meta,
        );
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
        Ok(())
    }

    pub(crate) fn observe_for_user_turn_from_workspace(
        &mut self,
        workspace_root: &Path,
        remote_aliases_prompt: String,
    ) -> Result<Vec<String>, String> {
        let metadata = load_workspace_runtime_metadata(workspace_root)?;
        let mut prompt_notices = Vec::new();
        observe_component(
            &mut self.prompt_components,
            IDENTITY_PROMPT_COMPONENT,
            metadata.identity,
            &mut prompt_notices,
        );
        observe_component_without_notice(
            &mut self.prompt_components,
            STELLACLAW_MEMORY_PROMPT_COMPONENT,
            metadata.stellaclaw_memory,
        );
        observe_component(
            &mut self.prompt_components,
            USER_META_PROMPT_COMPONENT,
            metadata.user_meta,
            &mut prompt_notices,
        );
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

    pub(crate) fn skill_observation(&self, skill_name: &str) -> Option<SessionSkillObservation> {
        let state = self.skill_states.get(skill_name)?;
        Some(SessionSkillObservation {
            name: skill_name.to_string(),
            description: state.description.clone(),
            content: state.content.clone(),
        })
    }

    pub(crate) fn mark_loaded_skills(&mut self, skill_names: &[String], turn_number: u64) {
        for skill_name in skill_names {
            self.skill_states
                .entry(skill_name.clone())
                .or_default()
                .last_loaded_turn = Some(turn_number);
        }
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
    workspace_root: &Path,
) -> Result<WorkspaceRuntimeMetadata, String> {
    Ok(WorkspaceRuntimeMetadata {
        identity: read_optional_workspace_file(workspace_root, IDENTITY_PATH)?,
        stellaclaw_memory: read_optional_workspace_file(workspace_root, STELLACLAW_MEMORY_PATH)?,
        user_meta: read_optional_workspace_file(workspace_root, USER_META_PATH)?,
        skills: load_workspace_skills(workspace_root)?,
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

fn render_skills_metadata(skills: &[SessionSkillObservation]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "Available skills from .skill/ in the current workspace:".to_string(),
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
        state.notified_value = value.clone();
        state.notified_hash = hash;
        notices.push(PromptComponentChangeNotice {
            key: key.to_string(),
            value,
        });
    }
}

fn observe_component_without_notice(
    components: &mut BTreeMap<String, SessionPromptComponentState>,
    key: &str,
    value: String,
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
        state.notified_value = value;
        state.notified_hash = hash;
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
                        "User meta is now empty. Ignore earlier User meta prompt content."
                            .to_string(),
                    );
                } else {
                    sections.push(format!(
                        "User meta changed. Treat this refreshed user metadata as authoritative for this turn:\n{}",
                        notice.value.trim()
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::session_actor::ToolRemoteMode;

    fn temp_root() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("stellaclaw_runtime_metadata_{id}"))
    }

    #[test]
    fn detects_prompt_component_change_once() {
        let root = temp_root();
        fs::create_dir_all(root.join(".stellaclaw")).unwrap();
        fs::write(root.join(".stellaclaw/USER.md"), "tier: pro").unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, String::new())
            .expect("initial metadata should load");

        fs::write(root.join(".stellaclaw/USER.md"), "tier: enterprise").unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, String::new())
            .expect("changed metadata should load");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("[Runtime Prompt Updates]"));
        assert!(notices[0].contains("tier: enterprise"));

        assert!(state
            .observe_for_user_turn_from_workspace(&root, String::new())
            .expect("metadata should still load")
            .is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reports_loaded_skill_content_change() {
        let root = temp_root();
        fs::create_dir_all(root.join(".skill/demo")).unwrap();
        fs::write(root.join(".skill/demo/SKILL.md"), "# Demo\n\nold body").unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, String::new())
            .expect("initial metadata should load");
        state.mark_loaded_skills(&["demo".to_string()], 1);

        fs::write(root.join(".skill/demo/SKILL.md"), "# Demo\n\nnew body").unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, String::new())
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
        fs::create_dir_all(root.join(".skill/example")).unwrap();
        fs::write(
            root.join(".skill/example/SKILL.md"),
            "# Example\n\nDo the important thing.",
        )
        .unwrap();

        let metadata = load_workspace_runtime_metadata(&root).expect("metadata should load");

        assert_eq!(metadata.skills.len(), 1);
        assert_eq!(metadata.skills[0].name, "example");
        assert_eq!(metadata.skills[0].description, "Do the important thing.");
        assert!(metadata
            .skills_metadata
            .contains("- example: Do the important thing."));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stellaclaw_memory_updates_without_runtime_notice() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("STELLACLAW.md"), "old durable note").unwrap();

        let mut state = RuntimeMetadataState::default();
        state
            .initialize_from_workspace(&root, String::new())
            .expect("initial metadata should load");

        fs::write(root.join("STELLACLAW.md"), "new durable note").unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(&root, String::new())
            .expect("updated metadata should load");

        assert!(notices.is_empty());
        assert_eq!(
            state
                .snapshot_value(STELLACLAW_MEMORY_PROMPT_COMPONENT)
                .expect("snapshot should exist"),
            "old durable note"
        );

        state.promote_notified_components_to_system_snapshot();
        assert_eq!(
            state
                .snapshot_value(STELLACLAW_MEMORY_PROMPT_COMPONENT)
                .expect("promoted snapshot should exist"),
            "new durable note"
        );

        let _ = fs::remove_dir_all(root);
    }
}
