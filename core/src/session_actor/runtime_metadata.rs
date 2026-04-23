use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub(crate) const IDENTITY_PROMPT_COMPONENT: &str = "identity";
pub(crate) const REMOTE_ALIASES_PROMPT_COMPONENT: &str = "ssh_remote_aliases";
pub(crate) const SKILLS_METADATA_PROMPT_COMPONENT: &str = "skills_metadata";
pub(crate) const USER_META_PROMPT_COMPONENT: &str = "user_meta";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_meta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_metadata: Option<String>,
    #[serde(default)]
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
    pub(crate) fn initialize_from(
        &mut self,
        metadata: Option<&SessionRuntimeMetadata>,
        remote_aliases_prompt: String,
    ) {
        if let Some(metadata) = metadata {
            initialize_component(
                &mut self.prompt_components,
                IDENTITY_PROMPT_COMPONENT,
                metadata.identity.clone().unwrap_or_default(),
            );
            initialize_component(
                &mut self.prompt_components,
                USER_META_PROMPT_COMPONENT,
                metadata.user_meta.clone().unwrap_or_default(),
            );
            initialize_component(
                &mut self.prompt_components,
                SKILLS_METADATA_PROMPT_COMPONENT,
                metadata.skills_metadata.clone().unwrap_or_default(),
            );

            for skill in &metadata.skills {
                self.skill_states
                    .entry(skill.name.clone())
                    .or_insert_with(|| SessionSkillState {
                        description: skill.description.clone(),
                        content: skill.content.clone(),
                        last_loaded_turn: None,
                    });
            }
        }

        initialize_component(
            &mut self.prompt_components,
            REMOTE_ALIASES_PROMPT_COMPONENT,
            remote_aliases_prompt,
        );
    }

    pub(crate) fn observe_for_user_turn(
        &mut self,
        metadata: Option<&SessionRuntimeMetadata>,
        remote_aliases_prompt: String,
    ) -> Vec<String> {
        let mut prompt_notices = Vec::new();
        if let Some(metadata) = metadata {
            observe_component(
                &mut self.prompt_components,
                IDENTITY_PROMPT_COMPONENT,
                metadata.identity.clone().unwrap_or_default(),
                &mut prompt_notices,
            );
            observe_component(
                &mut self.prompt_components,
                USER_META_PROMPT_COMPONENT,
                metadata.user_meta.clone().unwrap_or_default(),
                &mut prompt_notices,
            );
            observe_component(
                &mut self.prompt_components,
                SKILLS_METADATA_PROMPT_COMPONENT,
                metadata.skills_metadata.clone().unwrap_or_default(),
                &mut prompt_notices,
            );
        }
        observe_component(
            &mut self.prompt_components,
            REMOTE_ALIASES_PROMPT_COMPONENT,
            remote_aliases_prompt,
            &mut prompt_notices,
        );

        let skill_notices = metadata
            .map(|metadata| self.observe_skill_changes(&metadata.skills))
            .unwrap_or_default();

        let mut rendered = Vec::new();
        let prompt_text = render_prompt_component_change_notices(&prompt_notices);
        if !prompt_text.is_empty() {
            rendered.push(prompt_text);
        }
        let skill_text = render_skill_change_notices(&skill_notices);
        if !skill_text.is_empty() {
            rendered.push(skill_text);
        }
        rendered
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

    fn observe_skill_changes(
        &mut self,
        observed_skills: &[SessionSkillObservation],
    ) -> Vec<SkillChangeNotice> {
        let mut notices = Vec::new();
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

pub(crate) fn current_ssh_remote_aliases_prompt() -> String {
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
    use super::*;

    #[test]
    fn detects_prompt_component_change_once() {
        let mut state = RuntimeMetadataState::default();
        let first = SessionRuntimeMetadata {
            user_meta: Some("tier: pro".to_string()),
            ..SessionRuntimeMetadata::default()
        };
        state.initialize_from(Some(&first), String::new());

        let changed = SessionRuntimeMetadata {
            user_meta: Some("tier: enterprise".to_string()),
            ..SessionRuntimeMetadata::default()
        };
        let notices = state.observe_for_user_turn(Some(&changed), String::new());
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("[Runtime Prompt Updates]"));
        assert!(notices[0].contains("tier: enterprise"));

        assert!(state
            .observe_for_user_turn(Some(&changed), String::new())
            .is_empty());
    }

    #[test]
    fn reports_loaded_skill_content_change() {
        let mut state = RuntimeMetadataState::default();
        let first = SessionRuntimeMetadata {
            skills: vec![SessionSkillObservation {
                name: "demo".to_string(),
                description: "old desc".to_string(),
                content: "old body".to_string(),
            }],
            ..SessionRuntimeMetadata::default()
        };
        state.initialize_from(Some(&first), String::new());
        state.mark_loaded_skills(&["demo".to_string()], 1);

        let changed = SessionRuntimeMetadata {
            skills: vec![SessionSkillObservation {
                name: "demo".to_string(),
                description: "new desc".to_string(),
                content: "new body".to_string(),
            }],
            ..SessionRuntimeMetadata::default()
        };
        let notices = state.observe_for_user_turn(Some(&changed), String::new());
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("[Runtime Skill Updates]"));
        assert!(notices[0].contains("new body"));
    }
}
