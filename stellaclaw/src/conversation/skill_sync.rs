use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::{config::SkillSyncConfig, logger::StellaclawLogger};

#[derive(Debug, Clone, Copy)]
pub(super) enum SkillPersistMode {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SkillSyncPushResult {
    configured: bool,
    committed: bool,
    pushes: Vec<SkillSyncPushTargetResult>,
}

#[derive(Debug, Clone, Serialize)]
struct SkillSyncPushTargetResult {
    upstream: String,
    branch: String,
    pushed: bool,
    committed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartupSkillSyncResult {
    skill_name: String,
    found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    push: Option<SkillSyncPushResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

pub(crate) fn push_configured_skill_sync_on_startup(
    skill_sync: &[SkillSyncConfig],
    workdir: &Path,
    logger: &StellaclawLogger,
) -> Vec<StartupSkillSyncResult> {
    let mut skill_names = Vec::new();
    for entry in skill_sync {
        for skill_name in &entry.skill_name {
            if !skill_names.contains(skill_name) {
                skill_names.push(skill_name.clone());
            }
        }
    }
    if skill_names.is_empty() {
        return Vec::new();
    }

    let runtime_skill_root = workdir.join("rundir").join(".skill");
    let mut results = Vec::new();
    for skill_name in skill_names {
        let skill_path = runtime_skill_root.join(&skill_name);
        if !skill_path.exists() {
            let warning = format!("runtime skill {} does not exist", skill_path.display());
            logger.warn(
                "skill_sync_startup_skill_missing",
                json!({
                    "skill_name": skill_name,
                    "skill_path": skill_path.display().to_string(),
                    "warning": warning,
                }),
            );
            results.push(StartupSkillSyncResult {
                skill_name,
                found: false,
                push: None,
                warning: Some(warning),
            });
            continue;
        }

        let push = push_skill_sync_if_configured(skill_sync, &skill_name, &skill_path, logger);
        logger.info(
            "skill_sync_startup_skill_pushed",
            json!({
                "skill_name": skill_name,
                "push": &push,
            }),
        );
        results.push(StartupSkillSyncResult {
            skill_name,
            found: true,
            push: Some(push),
            warning: None,
        });
    }
    results
}

pub(super) fn push_skill_sync_if_configured(
    skill_sync: &[SkillSyncConfig],
    skill_name: &str,
    skill_path: &Path,
    logger: &StellaclawLogger,
) -> SkillSyncPushResult {
    let upstreams = configured_skill_sync_upstreams(skill_sync, skill_name);
    if upstreams.is_empty() {
        return SkillSyncPushResult {
            configured: false,
            committed: false,
            pushes: Vec::new(),
        };
    }

    let branch = "main";
    let mut pushes = Vec::new();

    for upstream in upstreams {
        let push_result = sync_skill_to_upstream_repo(skill_name, skill_path, &upstream, branch);
        let committed = push_result.as_ref().copied().unwrap_or(false);
        let warning = push_result.err().map(|error| error.to_string());
        let pushed = warning.is_none();
        if let Some(warning) = warning.as_deref() {
            logger.warn(
                "skill_sync_push_failed",
                json!({
                    "skill_name": skill_name,
                    "upstream": upstream,
                    "branch": branch,
                    "warning": warning,
                }),
            );
        }
        pushes.push(SkillSyncPushTargetResult {
            upstream,
            branch: branch.to_string(),
            pushed,
            committed,
            warning,
        });
    }

    SkillSyncPushResult {
        configured: true,
        committed: pushes.iter().any(|push| push.committed),
        pushes,
    }
}

fn configured_skill_sync_upstreams(
    skill_sync: &[SkillSyncConfig],
    skill_name: &str,
) -> Vec<String> {
    let mut upstreams = Vec::new();
    for entry in skill_sync {
        if entry.skill_name.iter().any(|name| name == skill_name) {
            for upstream in &entry.upstream {
                if !upstreams.contains(upstream) {
                    upstreams.push(upstream.clone());
                }
            }
        }
    }
    upstreams
}

fn sync_skill_to_upstream_repo(
    skill_name: &str,
    skill_path: &Path,
    upstream: &str,
    branch: &str,
) -> Result<bool> {
    validate_git_branch_name(branch)?;
    validate_skill_directory(skill_path, skill_name)?;

    let sync_root = std::env::temp_dir().join(format!(
        "stellaclaw-skill-sync-{}-{}-{}",
        std::process::id(),
        safe_temp_path_component(skill_name),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let repo_path = sync_root.join("repo");
    let result = (|| {
        fs::create_dir_all(&repo_path)
            .with_context(|| format!("failed to create {}", repo_path.display()))?;
        run_git(&repo_path, ["init"])?;
        ensure_git_identity(&repo_path)?;
        run_git(&repo_path, ["remote", "add", "origin", upstream])?;
        match run_git_with_timeout(
            &repo_path,
            ["fetch", "--depth=1", "origin", branch],
            Duration::from_secs(4),
        ) {
            Ok(()) => run_git(&repo_path, ["checkout", "-B", branch, "FETCH_HEAD"])?,
            Err(error) => {
                run_git(&repo_path, ["checkout", "--orphan", branch])?;
                if repo_path.join("SKILL.md").exists() {
                    return Err(error)
                        .context("upstream fetch failed after creating empty fallback branch");
                }
            }
        }

        remove_legacy_root_skill_payload_if_present(&repo_path, skill_name)?;
        copy_skill_payload_to_repo_subdir(skill_path, &repo_path.join(skill_name))?;
        run_git(&repo_path, ["add", "-A"])?;

        let committed = git_has_staged_changes(&repo_path)?;
        if committed {
            run_git(
                &repo_path,
                ["commit", "-m", &format!("Update skill {skill_name}")],
            )?;
        }
        run_git_push_with_timeout(&repo_path, upstream, branch, Duration::from_secs(4))?;
        Ok(committed)
    })();
    let cleanup = fs::remove_dir_all(&sync_root);
    if let Err(error) = cleanup {
        if result.is_ok() {
            return Err(error).with_context(|| format!("failed to remove {}", sync_root.display()));
        }
    }
    result
}

fn safe_temp_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn remove_legacy_root_skill_payload_if_present(repo_path: &Path, skill_name: &str) -> Result<()> {
    let root_skill = repo_path.join("SKILL.md");
    if !root_skill.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&root_skill)
        .with_context(|| format!("failed to read {}", root_skill.display()))?;
    let Some(frontmatter) = extract_yaml_frontmatter(&content) else {
        return Ok(());
    };
    if frontmatter_scalar(frontmatter, "name").as_deref() != Some(skill_name) {
        return Ok(());
    }

    for entry in fs::read_dir(repo_path)
        .with_context(|| format!("failed to read {}", repo_path.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to enumerate {}", repo_path.display()))?;
        let name = entry.file_name();
        if name == ".git" || name == skill_name {
            continue;
        }
        let path = entry.path();
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?
            .is_dir()
        {
            continue;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn ensure_git_identity(skill_path: &Path) -> Result<()> {
    if !git_config_has_value(skill_path, "user.name")? {
        run_git(skill_path, ["config", "user.name", "Stellaclaw"])?;
    }
    if !git_config_has_value(skill_path, "user.email")? {
        run_git(skill_path, ["config", "user.email", "stellaclaw@localhost"])?;
    }
    Ok(())
}

fn git_config_has_value(skill_path: &Path, key: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .current_dir(skill_path)
        .output()
        .with_context(|| format!("failed to run git config --get {key}"))?;
    Ok(output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn git_has_staged_changes(skill_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(skill_path)
        .output()
        .context("failed to run git diff --cached --quiet")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(anyhow!(
            "git diff --cached --quiet failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "git command failed in {}: {}\n{}",
        cwd.display(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn run_git_push_with_timeout(
    cwd: &Path,
    upstream: &str,
    branch: &str,
    timeout: Duration,
) -> Result<()> {
    let refspec = format!("HEAD:{branch}");
    run_git_with_timeout(cwd, ["push", upstream, refspec.as_str()], timeout)
}

fn run_git_with_timeout<const N: usize>(
    cwd: &Path,
    args: [&str; N],
    timeout: Duration,
) -> Result<()> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;
    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .with_context(|| format!("failed to wait for git in {}", cwd.display()))?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .with_context(|| format!("failed to read git output in {}", cwd.display()))?;
            if output.status.success() {
                return Ok(());
            }
            return Err(anyhow!(
                "git command failed in {}: {}\n{}",
                cwd.display(),
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("git push timed out after {}s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn validate_git_branch_name(branch: &str) -> Result<()> {
    let trimmed = branch.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("branch must not be empty"));
    }
    if trimmed != branch {
        return Err(anyhow!(
            "branch must not contain leading or trailing whitespace"
        ));
    }
    if branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains('@')
        || branch
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(anyhow!("branch is not a safe git branch name"));
    }
    Ok(())
}

pub(super) fn validate_skill_name(skill_name: &str) -> Result<()> {
    let name = skill_name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill_name must not be empty"));
    }
    if name != skill_name {
        return Err(anyhow!(
            "skill_name must not contain leading or trailing whitespace"
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(anyhow!(
            "skill_name may only contain ASCII letters, digits, '_' and '-'"
        ));
    }
    Ok(())
}

pub(super) fn validate_skill_directory(skill_path: &Path, skill_name: &str) -> Result<()> {
    if !skill_path.is_dir() {
        return Err(anyhow!(
            "staged skill directory {} does not exist",
            skill_path.display()
        ));
    }
    let entry_path = skill_path.join("SKILL.md");
    let content = fs::read_to_string(&entry_path)
        .with_context(|| format!("failed to read {}", entry_path.display()))?;
    let frontmatter = extract_yaml_frontmatter(&content)
        .ok_or_else(|| anyhow!("{} must start with YAML frontmatter", entry_path.display()))?;
    let name = frontmatter_scalar(frontmatter, "name")
        .ok_or_else(|| anyhow!("{} frontmatter must contain name", entry_path.display()))?;
    if name != skill_name {
        return Err(anyhow!(
            "{} frontmatter name `{}` does not match folder `{}`",
            entry_path.display(),
            name,
            skill_name
        ));
    }
    let description = frontmatter_scalar(frontmatter, "description").ok_or_else(|| {
        anyhow!(
            "{} frontmatter must contain description",
            entry_path.display()
        )
    })?;
    if description.trim().is_empty() {
        return Err(anyhow!(
            "{} frontmatter description must not be empty",
            entry_path.display()
        ));
    }
    Ok(())
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

pub(super) fn copy_skill_atomically(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let tmp = destination.with_extension("tmp-skill-copy");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).with_context(|| format!("failed to remove {}", tmp.display()))?;
    }
    copy_directory_recursive_local(source, &tmp)?;
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("failed to remove {}", destination.display()))?;
    }
    fs::rename(&tmp, destination).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp.display(),
            destination.display()
        )
    })
}

fn copy_skill_payload_to_repo_subdir(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let tmp = destination.with_extension("tmp-skill-sync");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).with_context(|| format!("failed to remove {}", tmp.display()))?;
    }
    copy_directory_recursive_local_excluding(source, &tmp, &[".git"])?;
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("failed to remove {}", destination.display()))?;
    }
    fs::rename(&tmp, destination).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp.display(),
            destination.display()
        )
    })
}

pub(super) fn sync_skill_to_conversation_workspaces(
    workdir: &Path,
    skill_name: &str,
    source: Option<&Path>,
) -> Result<usize> {
    let conversations_root = workdir.join("conversations");
    if !conversations_root.is_dir() {
        return Ok(0);
    }
    let mut synced = 0usize;
    for entry in fs::read_dir(&conversations_root)
        .with_context(|| format!("failed to read {}", conversations_root.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let skill_root = entry.path().join(".skill");
        if !skill_root.is_dir() {
            continue;
        }
        let destination = skill_root.join(skill_name);
        match source {
            Some(source) => {
                copy_skill_atomically(source, &destination)?;
                synced += 1;
            }
            None => {
                if destination.exists() {
                    fs::remove_dir_all(&destination)
                        .with_context(|| format!("failed to remove {}", destination.display()))?;
                    synced += 1;
                }
            }
        }
    }
    Ok(synced)
}

fn copy_directory_recursive_local(source: &Path, destination: &Path) -> Result<()> {
    copy_directory_recursive_local_excluding(source, destination, &[])
}

fn copy_directory_recursive_local_excluding(
    source: &Path,
    destination: &Path,
    excluded_names: &[&str],
) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        if excluded_names
            .iter()
            .any(|excluded| entry.file_name() == *excluded)
        {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?
            .is_dir()
        {
            copy_directory_recursive_local_excluding(
                &source_path,
                &destination_path,
                excluded_names,
            )?;
        } else {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_scalar_finds_description_after_name() {
        let frontmatter = "name: web-report-deploy\ndescription: Deploy reports\n";

        assert_eq!(
            frontmatter_scalar(frontmatter, "description").as_deref(),
            Some("Deploy reports")
        );
    }

    #[test]
    fn frontmatter_scalar_supports_quoted_and_folded_values() {
        let quoted = "name: demo\ndescription: \"Deploy reports: safely\"\n";
        assert_eq!(
            frontmatter_scalar(quoted, "description").as_deref(),
            Some("Deploy reports: safely")
        );

        let folded = "name: demo\ndescription: >\n  Deploy reports\n  safely\nnext: value\n";
        assert_eq!(
            frontmatter_scalar(folded, "description").as_deref(),
            Some("Deploy reports safely")
        );
    }

    #[test]
    fn configured_skill_sync_pushes_runtime_skill_to_git_repos() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-skill-sync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root should exist");
        let bare_repo_a = root.join("upstream-a.git");
        let bare_repo_b = root.join("upstream-b.git");
        for bare_repo in [&bare_repo_a, &bare_repo_b] {
            let init = Command::new("git")
                .args(["init", "--bare"])
                .arg(bare_repo)
                .output()
                .expect("git init --bare should run");
            assert!(
                init.status.success(),
                "{}",
                String::from_utf8_lossy(&init.stderr)
            );
        }

        let skill_path = root.join("rundir").join(".skill").join("demo");
        fs::create_dir_all(&skill_path).expect("skill path should exist");
        fs::write(
            skill_path.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nbody\n",
        )
        .expect("skill should be written");
        let logger = StellaclawLogger::open_under(&root, "test.log").expect("logger should open");
        let sync = vec![SkillSyncConfig {
            skill_name: vec!["demo".to_string()],
            upstream: vec![
                bare_repo_a.to_string_lossy().to_string(),
                bare_repo_b.to_string_lossy().to_string(),
            ],
        }];

        let result = push_skill_sync_if_configured(&sync, "demo", &skill_path, &logger);

        assert!(result.configured);
        assert!(result.committed);
        assert_eq!(result.pushes.len(), 2);
        assert!(result.pushes.iter().all(|push| push.pushed));
        for bare_repo in [&bare_repo_a, &bare_repo_b] {
            assert_git_path_exists(bare_repo, "main:demo/SKILL.md");
            assert_git_path_missing(bare_repo, "main:SKILL.md");
        }
        fs::remove_dir_all(&root).expect("temp root should be removed");
    }

    #[test]
    fn startup_skill_sync_pushes_configured_runtime_skills() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-startup-skill-sync-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root should exist");
        let bare_repo = root.join("upstream.git");
        let init = Command::new("git")
            .args(["init", "--bare"])
            .arg(&bare_repo)
            .output()
            .expect("git init --bare should run");
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );

        let skill_path = root.join("rundir").join(".skill").join("demo");
        fs::create_dir_all(&skill_path).expect("skill path should exist");
        fs::write(
            skill_path.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nbody\n",
        )
        .expect("skill should be written");
        let other_skill_path = root.join("rundir").join(".skill").join("other");
        fs::create_dir_all(&other_skill_path).expect("other skill path should exist");
        fs::write(
            other_skill_path.join("SKILL.md"),
            "---\nname: other\ndescription: Other skill\n---\nbody\n",
        )
        .expect("other skill should be written");
        let logger = StellaclawLogger::open_under(&root, "test.log").expect("logger should open");
        let sync = vec![SkillSyncConfig {
            skill_name: vec![
                "demo".to_string(),
                "other".to_string(),
                "missing".to_string(),
            ],
            upstream: vec![bare_repo.to_string_lossy().to_string()],
        }];

        let result = push_configured_skill_sync_on_startup(&sync, &root, &logger);

        assert_eq!(result.len(), 3);
        let demo = result
            .iter()
            .find(|entry| entry.skill_name == "demo")
            .expect("demo result should exist");
        assert!(demo.found);
        assert!(demo.push.as_ref().unwrap().committed);
        assert!(demo
            .push
            .as_ref()
            .unwrap()
            .pushes
            .iter()
            .all(|push| push.pushed));
        let other = result
            .iter()
            .find(|entry| entry.skill_name == "other")
            .expect("other result should exist");
        assert!(other.found);
        assert!(other.push.as_ref().unwrap().committed);
        assert!(other
            .push
            .as_ref()
            .unwrap()
            .pushes
            .iter()
            .all(|push| push.pushed));
        let missing = result
            .iter()
            .find(|entry| entry.skill_name == "missing")
            .expect("missing result should exist");
        assert!(!missing.found);
        assert!(missing.warning.is_some());

        assert_git_path_exists(&bare_repo, "main:demo/SKILL.md");
        assert_git_path_exists(&bare_repo, "main:other/SKILL.md");
        assert_git_path_missing(&bare_repo, "main:SKILL.md");
        fs::remove_dir_all(&root).expect("temp root should be removed");
    }

    fn assert_git_path_exists(bare_repo: &Path, pathspec: &str) {
        let output = Command::new("git")
            .args(["--git-dir"])
            .arg(bare_repo)
            .args(["show", pathspec])
            .output()
            .expect("git show should run");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_git_path_missing(bare_repo: &Path, pathspec: &str) {
        let output = Command::new("git")
            .args(["--git-dir"])
            .arg(bare_repo)
            .args(["show", pathspec])
            .output()
            .expect("git show should run");
        assert!(
            !output.status.success(),
            "expected {pathspec} to be absent, but git show succeeded"
        );
    }
}
