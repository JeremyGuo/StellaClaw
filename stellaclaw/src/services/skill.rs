#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::select;
use serde::Serialize;

use crate::{
    conversation_metadata::WorkdirLayout,
    conversation_new::{
        ConversationService, ServiceAddr, ServiceCall, ServiceFailure, ServiceOutput,
        ServiceRunContext, ServiceStatusUpdate, ServiceStopped,
    },
    service_protos::skill::{
        decode_request, encode_response, SkillInfo, SkillPersistMode, SkillRequest, SkillResponse,
    },
    services::skill_sync::{
        copy_skill_atomically, sync_skill_to_conversation_workspaces, validate_skill_directory,
        validate_skill_name,
    },
};

pub struct SkillService;

impl SkillService {
    pub fn new() -> Self {
        Self
    }
}

impl ConversationService for SkillService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        match reconcile_skills(&ctx) {
            Ok(summary) => {
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "skills_reconciled".to_string(),
                    detail: serde_json::to_value(summary)?,
                }))?;
            }
            Err(error) => {
                ctx.outbox.send(ServiceOutput::Failed(ServiceFailure {
                    addr: ctx.addr.clone(),
                    error: format!("skill startup reconcile failed: {error:#}"),
                }))?;
            }
        }

        loop {
            select! {
                recv(ctx.stop_rx) -> stop => {
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason: stop.ok().map(|stop| stop.reason),
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload) {
                        Ok(request) => {
                            let response = match handle_skill_request(&ctx, request) {
                                Ok(response) => response,
                                Err(error) => SkillResponse::Failure {
                                    reason: error.to_string(),
                                },
                            };
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                            )))?;
                        }
                        Err(error) => {
                            ctx.outbox.send(ServiceOutput::Failed(ServiceFailure {
                                addr: ctx.addr.clone(),
                                error: format!("bad skill payload: {error}"),
                            }))?;
                        }
                    }
                }
            }
        }
    }
}

fn handle_skill_request(ctx: &ServiceRunContext, request: SkillRequest) -> Result<SkillResponse> {
    match request {
        SkillRequest::Reconcile => {
            let summary = reconcile_skills(ctx)?;
            Ok(SkillResponse::Reconciled {
                runtime_skills: summary.runtime_skills,
                synced_skills: summary.synced_skills,
            })
        }
        SkillRequest::Persist { skill_name, mode } => persist_skill(ctx, &skill_name, mode),
        SkillRequest::List => Ok(SkillResponse::Skills {
            skills: list_skills(ctx)?,
        }),
        SkillRequest::Load { skill_name } => load_skill(ctx, &skill_name),
    }
}

#[derive(Debug, Clone, Serialize)]
struct ReconcileSummary {
    runtime_skills: usize,
    synced_skills: usize,
}

fn reconcile_skills(ctx: &ServiceRunContext) -> Result<ReconcileSummary> {
    let runtime_root = runtime_skill_root(ctx);
    let workspace_root = workspace_skill_root(ctx);
    fs::create_dir_all(&runtime_root)
        .with_context(|| format!("failed to create {}", runtime_root.display()))?;
    fs::create_dir_all(&workspace_root)
        .with_context(|| format!("failed to create {}", workspace_root.display()))?;

    let mut runtime_skills = 0usize;
    let mut synced_skills = 0usize;
    for skill_dir in sorted_skill_dirs(&runtime_root)? {
        let skill_name = skill_dir_name(&skill_dir)?;
        validate_skill_name(&skill_name)?;
        validate_skill_directory(&skill_dir, &skill_name)
            .with_context(|| format!("invalid runtime skill {skill_name}"))?;
        runtime_skills += 1;

        let destination = workspace_root.join(&skill_name);
        copy_skill_atomically(&skill_dir, &destination).with_context(|| {
            format!(
                "failed to sync runtime skill {} into {}",
                skill_dir.display(),
                destination.display()
            )
        })?;
        synced_skills += 1;
    }

    Ok(ReconcileSummary {
        runtime_skills,
        synced_skills,
    })
}

fn persist_skill(
    ctx: &ServiceRunContext,
    skill_name: &str,
    mode: SkillPersistMode,
) -> Result<SkillResponse> {
    validate_skill_name(skill_name)?;
    let runtime_skill_path = runtime_skill_root(ctx).join(skill_name);
    let staged_skill_path = workspace_skill_root(ctx).join(skill_name);

    match mode {
        SkillPersistMode::Create => {
            if runtime_skill_path.exists() {
                return Err(anyhow!(
                    "skill {skill_name} already exists in runtime store"
                ));
            }
            validate_skill_directory(&staged_skill_path, skill_name)?;
            copy_skill_atomically(&staged_skill_path, &runtime_skill_path)?;
            let synced = sync_skill_to_conversation_workspaces(
                &ctx.conversation.workdir,
                skill_name,
                Some(&staged_skill_path),
            )?;
            Ok(SkillResponse::Persisted {
                skill_name: skill_name.to_string(),
                mode,
                synced_workspaces: synced,
            })
        }
        SkillPersistMode::Update => {
            if !runtime_skill_path.exists() {
                return Err(anyhow!(
                    "skill {skill_name} does not exist in runtime store"
                ));
            }
            validate_skill_directory(&staged_skill_path, skill_name)?;
            copy_skill_atomically(&staged_skill_path, &runtime_skill_path)?;
            let synced = sync_skill_to_conversation_workspaces(
                &ctx.conversation.workdir,
                skill_name,
                Some(&staged_skill_path),
            )?;
            Ok(SkillResponse::Persisted {
                skill_name: skill_name.to_string(),
                mode,
                synced_workspaces: synced,
            })
        }
        SkillPersistMode::Delete => {
            if !runtime_skill_path.exists() {
                return Err(anyhow!(
                    "skill {skill_name} does not exist in runtime store"
                ));
            }
            fs::remove_dir_all(&runtime_skill_path)
                .with_context(|| format!("failed to remove {}", runtime_skill_path.display()))?;
            let synced =
                sync_skill_to_conversation_workspaces(&ctx.conversation.workdir, skill_name, None)?;
            Ok(SkillResponse::Persisted {
                skill_name: skill_name.to_string(),
                mode,
                synced_workspaces: synced,
            })
        }
    }
}

fn list_skills(ctx: &ServiceRunContext) -> Result<Vec<SkillInfo>> {
    let runtime_root = runtime_skill_root(ctx);
    if !runtime_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut skills = Vec::new();
    for skill_dir in sorted_skill_dirs(&runtime_root)? {
        let name = skill_dir_name(&skill_dir)?;
        validate_skill_name(&name)?;
        validate_skill_directory(&skill_dir, &name)
            .with_context(|| format!("invalid runtime skill {name}"))?;
        let content = fs::read_to_string(skill_dir.join("SKILL.md"))
            .with_context(|| format!("failed to read {}", skill_dir.join("SKILL.md").display()))?;
        skills.push(SkillInfo {
            name: name.clone(),
            description: skill_description(&content),
            runtime_path: skill_dir.display().to_string(),
            workspace_path: workspace_skill_root(ctx).join(name).display().to_string(),
        });
    }
    Ok(skills)
}

fn load_skill(ctx: &ServiceRunContext, skill_name: &str) -> Result<SkillResponse> {
    validate_skill_name(skill_name)?;
    let workspace_skill_path = workspace_skill_root(ctx).join(skill_name);
    let runtime_skill_path = runtime_skill_root(ctx).join(skill_name);
    let skill_path = if workspace_skill_path.is_dir() {
        workspace_skill_path
    } else {
        runtime_skill_path
    };
    validate_skill_directory(&skill_path, skill_name)?;
    let content_path = skill_path.join("SKILL.md");
    let content = fs::read_to_string(&content_path)
        .with_context(|| format!("failed to read {}", content_path.display()))?;
    Ok(SkillResponse::Loaded {
        skill_name: skill_name.to_string(),
        description: skill_description(&content).unwrap_or_default(),
        content,
    })
}

fn runtime_skill_root(ctx: &ServiceRunContext) -> PathBuf {
    WorkdirLayout::new(&ctx.conversation.workdir).runtime_skill_root()
}

fn workspace_skill_root(ctx: &ServiceRunContext) -> PathBuf {
    ctx.conversation
        .conversation_root
        .join(".stellaclaw")
        .join("skill")
}

fn sorted_skill_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to enumerate {}", root.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn skill_dir_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("invalid skill path {}", path.display()))
}

fn skill_description(content: &str) -> Option<String> {
    let frontmatter = extract_yaml_frontmatter(content)?;
    frontmatter_scalar(frontmatter, "description")
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

fn reply(source: &ServiceAddr, target: &ServiceAddr, payload: serde_json::Value) -> ServiceCall {
    ServiceCall {
        source: source.clone(),
        target: target.clone(),
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    use crate::conversation_new::{ConversationRef, ServiceRefs};

    #[test]
    fn startup_reconcile_copies_runtime_skills_to_workspace() {
        let ctx = test_run_context("startup_reconcile_copies_runtime_skills_to_workspace");
        let runtime_skill = runtime_skill_root(&ctx).join("demo");
        write_skill(&runtime_skill, "demo", "Demo skill");

        let summary = reconcile_skills(&ctx).expect("skills reconcile");

        assert_eq!(summary.runtime_skills, 1);
        assert_eq!(summary.synced_skills, 1);
        assert!(workspace_skill_root(&ctx)
            .join("demo")
            .join("SKILL.md")
            .is_file());
    }

    #[test]
    fn persist_create_copies_staged_skill_to_runtime_store() {
        let ctx = test_run_context("persist_create_copies_staged_skill_to_runtime_store");
        write_skill(
            &workspace_skill_root(&ctx).join("demo"),
            "demo",
            "Demo skill",
        );

        let response =
            persist_skill(&ctx, "demo", SkillPersistMode::Create).expect("skill persists");

        assert!(runtime_skill_root(&ctx)
            .join("demo")
            .join("SKILL.md")
            .is_file());
        assert!(matches!(
            response,
            SkillResponse::Persisted {
                mode: SkillPersistMode::Create,
                ..
            }
        ));
    }

    #[test]
    fn load_returns_workspace_skill_content() {
        let ctx = test_run_context("load_returns_workspace_skill_content");
        write_skill(
            &workspace_skill_root(&ctx).join("demo"),
            "demo",
            "Demo skill",
        );

        let response = load_skill(&ctx, "demo").expect("skill loads");

        match response {
            SkillResponse::Loaded {
                description,
                content,
                ..
            } => {
                assert_eq!(description, "Demo skill");
                assert!(content.contains("# Demo"));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn test_run_context(name: &str) -> ServiceRunContext {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw_skill_service_test_{name}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("workdir");
        let conversation_root = workdir.join("conversations").join(name);
        let storage = root.join("services").join("test").join("local__skill");
        fs::create_dir_all(&storage).expect("storage created");
        let (_in_tx, inbox) = unbounded();
        let (outbox, _out_rx) = unbounded();
        let (_stop_tx, stop_rx) = unbounded();
        ServiceRunContext {
            addr: ServiceAddr::skill(),
            conversation: ConversationRef {
                conversation_id: name.to_string(),
                workdir,
                conversation_root,
            },
            storage,
            refs: ServiceRefs::default(),
            inbox,
            outbox,
            stop_rx,
        }
    }

    fn write_skill(path: &Path, name: &str, description: &str) {
        fs::create_dir_all(path).expect("skill dir created");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n# Demo\n"),
        )
        .expect("skill written");
    }
}
