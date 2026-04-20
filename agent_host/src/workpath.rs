use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_REMOTE_AGENTS_MD_BYTES: usize = 20_000;
const MAX_REMOTE_AGENTS_ERROR_CHARS: usize = 1_000;
const MAX_SSH_REMOTE_ALIASES: usize = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteWorkpath {
    pub host: String,
    pub path: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAgentsMdLoad {
    Loaded(String),
    NotFound,
    Failed(String),
}

pub fn validate_remote_workpath(
    host: &str,
    path: &str,
    description: &str,
) -> Result<RemoteWorkpath> {
    let host = host.trim();
    let path = path.trim();
    let description = description.trim();
    if host.is_empty() {
        return Err(anyhow!("host must be a non-empty SSH alias"));
    }
    if host == "local" {
        return Err(anyhow!(
            "remote workpath host must be a remote SSH alias, not local"
        ));
    }
    if path.is_empty() {
        return Err(anyhow!("path must be a non-empty remote directory path"));
    }
    if description.is_empty() {
        return Err(anyhow!(
            "description must explain what this remote path is for"
        ));
    }
    Ok(RemoteWorkpath {
        host: host.to_string(),
        path: path.to_string(),
        description: description.to_string(),
    })
}

pub fn validate_remote_workpath_key(host: &str, path: &str) -> Result<(String, String)> {
    let host = validate_remote_workpath_host(host)?;
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("path must be a non-empty remote directory path"));
    }
    Ok((host, path.to_string()))
}

pub fn validate_remote_workpath_host(host: &str) -> Result<String> {
    let host = host.trim();
    if host.is_empty() {
        return Err(anyhow!("host must be a non-empty SSH alias"));
    }
    if host == "local" {
        return Err(anyhow!(
            "remote workpath host must be a remote SSH alias, not local"
        ));
    }
    if matches!(host, "host" | "<host>" | "<host>|local") {
        return Err(anyhow!(
            "remote workpath host must be an actual SSH alias, not a placeholder"
        ));
    }
    if host.starts_with('-') {
        return Err(anyhow!("remote workpath host must not start with '-'"));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(anyhow!("remote workpath host must not contain whitespace"));
    }
    if host.chars().any(|ch| ch.is_control()) {
        return Err(anyhow!(
            "remote workpath host must not contain control characters"
        ));
    }
    if host.chars().any(|ch| {
        matches!(
            ch,
            '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
        )
    }) {
        return Err(anyhow!(
            "remote workpath host must not contain shell metacharacters"
        ));
    }
    if host.contains('/') || host.contains('\\') {
        return Err(anyhow!(
            "remote workpath host must not contain path separators"
        ));
    }
    Ok(host.to_string())
}

pub fn load_remote_agents_md(host: &str, path: &str) -> RemoteAgentsMdLoad {
    let (host, path) = match validate_remote_workpath_key(host, path) {
        Ok(value) => value,
        Err(error) => return RemoteAgentsMdLoad::Failed(error.to_string()),
    };
    let ssh = env::var("AGENT_HOST_SSH_BIN")
        .or_else(|_| env::var("AGENT_FRAME_SSH_BIN"))
        .unwrap_or_else(|_| "ssh".to_string());
    let script = format!(
        r#"p={path};
case "$p" in
  "~") d="$HOME" ;;
  "~/"*) d="$HOME/${{p#~/}}" ;;
  *) d="$p" ;;
esac
if [ ! -d "$d" ]; then
  exit 4
fi
if [ ! -f "$d/AGENTS.md" ]; then
  exit 3
fi
head -c {max_bytes} "$d/AGENTS.md""#,
        path = shell_single_quote(&path),
        max_bytes = MAX_REMOTE_AGENTS_MD_BYTES,
    );
    let output = Command::new(&ssh)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-T")
        .arg(&host)
        .arg(script)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return RemoteAgentsMdLoad::Failed(format!(
                "failed to run ssh for {}:{}: {}",
                host, path, error
            ));
        }
    };
    if output.status.success() {
        return RemoteAgentsMdLoad::Loaded(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        );
    }
    if output.status.code() == Some(3) {
        return RemoteAgentsMdLoad::NotFound;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let detail = truncate_chars(detail, MAX_REMOTE_AGENTS_ERROR_CHARS);
    RemoteAgentsMdLoad::Failed(format!(
        "ssh {} AGENTS.md load failed with status {}: {}",
        host,
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        detail
    ))
}

pub fn current_ssh_remote_aliases_prompt() -> String {
    let aliases = discover_ssh_remote_aliases();
    render_ssh_remote_aliases_for_prompt(&aliases)
}

pub fn discover_ssh_remote_aliases() -> Vec<String> {
    let Some(config_path) = default_ssh_config_path() else {
        return Vec::new();
    };
    discover_ssh_remote_aliases_from_path(&config_path)
}

fn default_ssh_config_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("AGENT_HOST_SSH_CONFIG")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
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
        for include_path in expand_ssh_include_path(base_dir, &include) {
            collect_ssh_config_aliases(&include_path, visited, aliases, depth + 1);
        }
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
        && validate_remote_workpath_host(alias).is_ok()
}

fn expand_ssh_include_path(base_dir: &Path, raw: &str) -> Vec<PathBuf> {
    let path = expand_tilde_path(raw);
    let path = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    if !raw.contains('*') && !raw.contains('?') {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    if parent
        .as_os_str()
        .to_string_lossy()
        .chars()
        .any(|ch| matches!(ch, '*' | '?'))
    {
        return Vec::new();
    }
    let Some(pattern) = path.file_name().and_then(|value| value.to_str()) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|entry_path| {
            entry_path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| wildcard_match(pattern, name))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn expand_tilde_path(raw: &str) -> PathBuf {
    if raw == "~" {
        return env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| "~".into());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut dp = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;
    for i in 1..=pattern.len() {
        if pattern[i - 1] == b'*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=pattern.len() {
        for j in 1..=text.len() {
            dp[i][j] = match pattern[i - 1] {
                b'*' => dp[i - 1][j] || dp[i][j - 1],
                b'?' => dp[i - 1][j - 1],
                value => value == text[j - 1] && dp[i - 1][j - 1],
            };
        }
    }
    dp[pattern.len()][text.len()]
}

pub fn render_ssh_remote_aliases_for_prompt(aliases: &[String]) -> String {
    let mut lines = vec![
        "Available SSH remote aliases detected from this host's SSH config:".to_string(),
        "Use exactly one of these values in a tool's remote=\"<alias>\" argument when operating on that SSH host. These are local SSH aliases normally defined in ~/.ssh/config; do not invent remote names. Omit remote for local work.".to_string(),
    ];
    if aliases.is_empty() {
        lines.push(
            "- none detected; ask the user or register a workpath before using remote tools."
                .to_string(),
        );
        return lines.join("\n");
    }
    let truncated = aliases.len() > MAX_SSH_REMOTE_ALIASES;
    for alias in aliases.iter().take(MAX_SSH_REMOTE_ALIASES) {
        lines.push(format!("- `{alias}`"));
    }
    if truncated {
        lines.push(format!(
            "- ... {} more aliases omitted.",
            aliases.len() - MAX_SSH_REMOTE_ALIASES
        ));
    }
    lines.join("\n")
}

pub fn render_remote_workpaths_for_prompt(workpaths: &[RemoteWorkpath]) -> Option<String> {
    if workpaths.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Remote workpaths registered for this conversation:".to_string(),
        "Use the tool remote=\"<host>\" option when operating in these remote directories; omit remote for local work.".to_string(),
    ];
    for workpath in workpaths {
        lines.push(format!(
            "- host=`{}` path=`{}` description: {}",
            workpath.host, workpath.path, workpath.description
        ));
        match load_remote_agents_md(&workpath.host, &workpath.path) {
            RemoteAgentsMdLoad::Loaded(markdown) if !markdown.trim().is_empty() => {
                lines.push("  AGENTS.md:".to_string());
                for line in markdown.trim().lines() {
                    lines.push(format!("  {}", line));
                }
            }
            RemoteAgentsMdLoad::Loaded(_) => {
                lines.push("  AGENTS.md: present but empty.".to_string());
            }
            RemoteAgentsMdLoad::NotFound => {
                lines.push(format!(
                    "  AGENTS.md: not found at {}/AGENTS.md.",
                    workpath.path
                ));
            }
            RemoteAgentsMdLoad::Failed(error) => {
                lines.push(format!("  AGENTS.md: failed to load via SSH: {}", error));
            }
        }
    }
    Some(lines.join("\n"))
}

pub fn load_result_to_json(load: &RemoteAgentsMdLoad) -> serde_json::Value {
    match load {
        RemoteAgentsMdLoad::Loaded(content) => serde_json::json!({
            "status": "loaded",
            "content": content,
        }),
        RemoteAgentsMdLoad::NotFound => serde_json::json!({
            "status": "not_found",
        }),
        RemoteAgentsMdLoad::Failed(error) => serde_json::json!({
            "status": "failed",
            "error": error,
        }),
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

pub fn load_remote_agents_md_for_workpath(workpath: &RemoteWorkpath) -> RemoteAgentsMdLoad {
    load_remote_agents_md(&workpath.host, &workpath.path)
}

pub fn replace_workpath_description(
    workpaths: &mut [RemoteWorkpath],
    host: &str,
    description: &str,
) -> Result<()> {
    let description = description.trim();
    if description.is_empty() {
        return Err(anyhow!(
            "description must explain what this remote path is for"
        ));
    }
    let Some(workpath) = workpaths.iter_mut().find(|item| item.host == host) else {
        return Err(anyhow!("remote workpath not found for {}", host));
    };
    workpath.description = description.to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        RemoteWorkpath, parse_ssh_config_aliases, render_ssh_remote_aliases_for_prompt,
        replace_workpath_description, validate_remote_workpath, validate_remote_workpath_host,
    };

    #[test]
    fn validate_remote_workpath_trims_values() {
        let workpath =
            validate_remote_workpath(" wuwen-dev6 ", " ~/repo ", " remote checkout ").unwrap();

        assert_eq!(workpath.host, "wuwen-dev6");
        assert_eq!(workpath.path, "~/repo");
        assert_eq!(workpath.description, "remote checkout");
    }

    #[test]
    fn validate_remote_workpath_rejects_placeholder_and_unsafe_hosts() {
        for host in [
            "host",
            "<host>",
            "<host>|local",
            "-oProxyCommand=bad",
            "dev host",
            "dev;rm",
            "dev/repo",
        ] {
            assert!(
                validate_remote_workpath_host(host).is_err(),
                "{host} should be rejected"
            );
        }
    }

    #[test]
    fn replace_description_requires_existing_key() {
        let mut workpaths = vec![RemoteWorkpath {
            host: "wuwen-dev6".to_string(),
            path: "/srv/app".to_string(),
            description: "old".to_string(),
        }];

        replace_workpath_description(&mut workpaths, "wuwen-dev6", "new").unwrap();

        assert_eq!(workpaths[0].description, "new");
        assert!(replace_workpath_description(&mut workpaths, "other", "new").is_err());
    }

    #[test]
    fn ssh_config_alias_parser_lists_only_concrete_hosts() {
        let aliases = parse_ssh_config_aliases(
            r#"
Host *
  ForwardAgent no
Host wuwen-dev3 wuwen-roce-check
Host *.example.com !banned placeholder?
Host root@dmit-los.jeremyguo.space
"#,
        );

        assert_eq!(
            aliases,
            vec![
                "root@dmit-los.jeremyguo.space".to_string(),
                "wuwen-dev3".to_string(),
                "wuwen-roce-check".to_string(),
            ]
        );
    }

    #[test]
    fn ssh_remote_alias_prompt_explains_remote_argument_source() {
        let rendered = render_ssh_remote_aliases_for_prompt(&[
            "wuwen-dev3".to_string(),
            "dmit-los".to_string(),
        ]);

        assert!(rendered.contains("~/.ssh/config"));
        assert!(rendered.contains("remote=\"<alias>\""));
        assert!(rendered.contains("do not invent remote names"));
        assert!(rendered.contains("`wuwen-dev3`"));
        assert!(rendered.contains("`dmit-los`"));
    }
}
