use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

fn parse_frontmatter(text: &str) -> Vec<(String, String)> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Vec::new();
    }

    let mut pairs = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            pairs.push((
                key.trim().to_string(),
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            ));
        }
    }
    pairs
}

pub fn validate_skill_markdown(text: &str) -> Result<(String, String)> {
    let frontmatter = parse_frontmatter(text);
    if frontmatter.is_empty() {
        return Err(anyhow!(
            "SKILL.md must begin with YAML frontmatter delimited by ---"
        ));
    }
    let name = frontmatter
        .iter()
        .find_map(|(key, value)| (key == "name").then_some(value.clone()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("SKILL.md frontmatter must include a non-empty name"))?;
    let description = frontmatter
        .iter()
        .find_map(|(key, value)| (key == "description").then_some(value.clone()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("SKILL.md frontmatter must include a non-empty description"))?;
    Ok((name, description))
}

fn iter_skill_dirs(candidate_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut discovered = Vec::new();
    for root in candidate_roots {
        if !root.exists() {
            continue;
        }
        if root.join("SKILL.md").is_file() {
            discovered.push(root.clone());
            continue;
        }

        if let Ok(read_dir) = fs::read_dir(root) {
            let mut children: Vec<PathBuf> = read_dir
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir() && path.join("SKILL.md").is_file())
                .collect();
            children.sort();
            discovered.extend(children);
        }
    }
    discovered
}

pub fn discover_skills(candidate_roots: &[PathBuf]) -> Result<Vec<SkillMetadata>> {
    let mut skills = Vec::new();
    for skill_dir in iter_skill_dirs(candidate_roots) {
        let skill_file = skill_dir.join("SKILL.md");
        let text = fs::read_to_string(&skill_file)
            .with_context(|| format!("failed to read {}", skill_file.display()))?;
        let frontmatter = parse_frontmatter(&text);
        let name = frontmatter
            .iter()
            .find_map(|(key, value)| (key == "name").then_some(value.clone()))
            .unwrap_or_else(|| {
                skill_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        let description = frontmatter
            .iter()
            .find_map(|(key, value)| (key == "description").then_some(value.clone()))
            .unwrap_or_default();
        skills.push(SkillMetadata {
            name,
            description,
            path: skill_dir,
        });
    }
    Ok(skills)
}

pub fn build_skills_meta_prompt(skills: &[SkillMetadata]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "[AgentFrame Skills]".to_string(),
        "Codex-style skills are available. Only metadata is preloaded here.".to_string(),
        "When a request matches a skill by name or description, call skill_load before using it."
            .to_string(),
        "After opening a skill, follow its instructions and only read referenced files that are actually needed."
            .to_string(),
        "Available skills:".to_string(),
    ];
    for skill in skills {
        let description = if skill.description.is_empty() {
            "No description provided.".to_string()
        } else {
            skill.description.clone()
        };
        lines.push(format!("- {}: {}", skill.name, description));
    }
    lines.join("\n")
}

pub fn build_skill_index(skills: &[SkillMetadata]) -> Result<BTreeMap<String, SkillMetadata>> {
    let mut index = BTreeMap::new();
    for skill in skills {
        if index.insert(skill.name.clone(), skill.clone()).is_some() {
            return Err(anyhow!("duplicate skill name: {}", skill.name));
        }
    }
    Ok(index)
}

pub fn load_skill_by_name(
    index: &BTreeMap<String, SkillMetadata>,
    skill_name: &str,
) -> Result<(SkillMetadata, String)> {
    let skill = index
        .get(skill_name)
        .cloned()
        .ok_or_else(|| anyhow!("unknown skill: {}", skill_name))?;
    let skill_file = skill.path.join("SKILL.md");
    let content = fs::read_to_string(&skill_file)
        .with_context(|| format!("failed to read {}", skill_file.display()))?;
    Ok((skill, content))
}

pub fn has_skill_file(path: &Path) -> bool {
    path.join("SKILL.md").is_file()
}
