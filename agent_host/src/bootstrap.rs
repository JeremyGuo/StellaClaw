use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const USER_TEMPLATE: &str = r#"---
name: workspace-user
description: Persistent notes about the human behind this workspace.
experience:
  - Product thinking
  - Fast iteration
  - Prefers direct technical communication
---

# User

Write durable facts about the user here.

Suggested topics:
- Background and experience
- Current goals
- Communication preferences
- Things the agent should remember across sessions
"#;

const IDENTITY_TEMPLATE: &str = r#"# Your name - What should they call you?
# Your nature - What kind of creature are you? AI assistant is fine, but maybe you are something weirder.
# Your vibe - Formal, casual, sharp, warm, or something else?
# Your emoji - Everyone needs a signature.
# Your mission - How should you help this user in this workspace?
"#;

const AGENTS_TEMPLATE: &str = "";

const SKILL_CREATOR_TEMPLATE: &str = r#"---
name: skill-creator
description: Create new skills or improve existing skills. Use when the user wants to turn a workflow into a reusable skill, revise a skill's trigger wording, reorganize skill resources, or persist a staged skill with skill_create or skill_update.
---

# Skill Creator

Use this skill when creating or revising a skill under the local runtime skills directory.

## Workflow

1. Capture intent before writing.
Ask what the skill should do, when it should trigger, what outputs it should produce, and whether the user wants simple vibe-based iteration or explicit test prompts.

2. Write the trigger description carefully.
The `description` field is the main trigger mechanism. It should say both:
- what the skill does
- when to use it

Be slightly pushy rather than timid. If a skill should trigger on dashboards, charts, reports, metrics, migrations, or similar contexts, say that directly in `description`.

3. Keep the main skill file focused.
Put the reusable workflow in `SKILL.md`. Keep it procedural, concrete, and easy to scan.

4. Use progressive disclosure.
Keep `SKILL.md` relatively compact. Put bulky material into bundled resources and reference them only when needed.

5. Persist only after editing the staged skill.
Edit the staged skill directory inside the current workspace first, then call:
- `skill_create` for a new skill
- `skill_update` for an existing skill

## Required structure

Every skill must contain `SKILL.md` with YAML frontmatter containing at least:
- `name`
- `description`

Recommended layout:
- `SKILL.md`
- `references/` for large docs or reference material loaded only when needed
- `scripts/` for deterministic or repetitive work
- `assets/` for templates or bundled output files when useful

## Writing guidance

- Prefer imperative instructions.
- Explain why an instruction matters when that improves judgment.
- Avoid overfitting the skill to one example conversation.
- Prefer a general reusable workflow over brittle rules.
- Keep examples realistic.
- If the skill supports multiple domains or frameworks, organize by variant and clearly say when to read each reference file.

## Skill memory rule

If a skill needs durable shared data across runs, store that data under `./.skill_memory`.

Rules:
- Skill-owned persistent data belongs in `./.skill_memory/<skill-name>/...`
- Do not store durable skill state in normal workspace files unless the user explicitly wants user-visible artifacts there
- Do not tell the agent to use `./.skill_memory` for ordinary task output
- Only use `./.skill_memory` when the skill itself explicitly requires it

Examples of appropriate `.skill_memory` contents:
- cached indexes
- reusable extracted metadata
- persistent registries owned by the skill
- scratch data that should survive across sessions for that skill

Examples of inappropriate `.skill_memory` contents:
- normal reports for the user
- task outputs that belong in the current workspace
- arbitrary agent notes unrelated to the skill's operation

## Evaluation and iteration

After drafting a skill, propose 2-3 realistic test prompts when testing would be useful.
Use them to check:
- whether the skill triggers in the right situations
- whether the instructions are clear enough
- whether the output shape is what the user expects

When improving an existing skill, look for:
- weak or timid trigger wording in `description`
- instructions that are too narrow or too vague
- bulky content that should move into `references/` or `scripts/`
- state that should live in `./.skill_memory` instead of the workspace

## Before persisting

Before calling `skill_create` or `skill_update`, verify:
- `SKILL.md` exists
- frontmatter `name` matches the folder name
- frontmatter `description` clearly states trigger contexts
- bundled resources are placed intentionally
- any durable skill-owned data design points to `./.skill_memory/<skill-name>/...`
"#;

#[derive(Clone, Debug)]
pub struct AgentWorkspace {
    pub root_dir: PathBuf,
    pub agent_dir: PathBuf,
    pub rundir: PathBuf,
    pub tmp_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub skill_creator_dir: PathBuf,
    pub user_md_path: PathBuf,
    pub identity_md_path: PathBuf,
    pub agents_md_path: PathBuf,
    pub user_profile_markdown: String,
    pub raw_identity_markdown: String,
    pub identity_prompt: String,
    pub agents_markdown: String,
}

impl AgentWorkspace {
    pub fn initialize(workdir: impl AsRef<Path>) -> Result<Self> {
        let root_dir = workdir.as_ref().to_path_buf();
        let agent_dir = root_dir.join("agent");
        let rundir = root_dir.join("rundir");
        let tmp_dir = rundir.join("tmp");
        let skills_dir = rundir.join(".skills");
        let skill_creator_dir = skills_dir.join("skill-creator");
        fs::create_dir_all(&agent_dir)
            .with_context(|| format!("failed to create {}", agent_dir.display()))?;
        fs::create_dir_all(&rundir)
            .with_context(|| format!("failed to create {}", rundir.display()))?;
        fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("failed to create {}", tmp_dir.display()))?;
        fs::create_dir_all(&skill_creator_dir)
            .with_context(|| format!("failed to create {}", skill_creator_dir.display()))?;

        let user_md_path = agent_dir.join("USER.md");
        let identity_md_path = agent_dir.join("IDENTITY.md");
        let agents_md_path = rundir.join("AGENTS.md");
        let skill_creator_md_path = skill_creator_dir.join("SKILL.md");

        ensure_seed_file(&user_md_path, USER_TEMPLATE)?;
        ensure_seed_file(&identity_md_path, IDENTITY_TEMPLATE)?;
        ensure_seed_file(&agents_md_path, AGENTS_TEMPLATE)?;
        ensure_seed_file(&skill_creator_md_path, SKILL_CREATOR_TEMPLATE)?;

        let user_profile_markdown = fs::read_to_string(&user_md_path)
            .with_context(|| format!("failed to read {}", user_md_path.display()))?;
        let raw_identity_markdown = fs::read_to_string(&identity_md_path)
            .with_context(|| format!("failed to read {}", identity_md_path.display()))?;
        let identity_prompt = render_identity_prompt_for_runtime(&raw_identity_markdown);
        let agents_markdown = fs::read_to_string(&agents_md_path)
            .with_context(|| format!("failed to read {}", agents_md_path.display()))?;

        Ok(Self {
            root_dir,
            agent_dir,
            rundir,
            tmp_dir,
            skills_dir,
            skill_creator_dir,
            user_md_path,
            identity_md_path,
            agents_md_path,
            user_profile_markdown,
            raw_identity_markdown,
            identity_prompt,
            agents_markdown,
        })
    }
}

fn ensure_seed_file(path: &Path, template: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, template)
            .with_context(|| format!("failed to write template {}", path.display()))?;
        return Ok(());
    }

    let existing = fs::read_to_string(path)
        .with_context(|| format!("failed to read existing file {}", path.display()))?;
    if existing.trim().is_empty() {
        fs::write(path, template)
            .with_context(|| format!("failed to write template {}", path.display()))?;
    }
    Ok(())
}

pub fn render_identity_prompt_for_runtime(markdown: &str) -> String {
    markdown
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                None
            } else if trimmed.is_empty() {
                Some(String::new())
            } else {
                Some(line.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::AgentWorkspace;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn initializes_workspace_templates_and_preserves_existing_identity() {
        let temp_dir = TempDir::new().unwrap();
        let identity_path = temp_dir.path().join("agent").join("IDENTITY.md");
        fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
        fs::write(&identity_path, "# Existing identity\n# Keep this\n").unwrap();

        let workspace = AgentWorkspace::initialize(temp_dir.path()).unwrap();
        assert!(workspace.user_md_path.exists());
        assert!(workspace.identity_md_path.exists());
        assert!(workspace.agents_md_path.exists());
        assert!(workspace.tmp_dir.exists());
        assert!(
            workspace
                .raw_identity_markdown
                .starts_with("# Existing identity")
        );
        assert!(workspace.identity_prompt.is_empty());
        assert!(workspace.user_profile_markdown.contains("experience:"));
        assert!(workspace.skill_creator_dir.join("SKILL.md").exists());
    }

    #[test]
    fn identity_prompt_ignores_commented_lines() {
        let temp_dir = TempDir::new().unwrap();
        let identity_path = temp_dir.path().join("agent").join("IDENTITY.md");
        fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
        fs::write(
            &identity_path,
            "# Commented heading\nYou are Claw.\n# Hidden note\nWarm and direct.\n",
        )
        .unwrap();

        let workspace = AgentWorkspace::initialize(temp_dir.path()).unwrap();
        assert_eq!(workspace.identity_prompt, "You are Claw.\nWarm and direct.");
        assert!(workspace.tmp_dir.ends_with("rundir/tmp"));
    }
}
