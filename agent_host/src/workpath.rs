use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::env;
use std::process::Command;

const MAX_REMOTE_AGENTS_MD_BYTES: usize = 20_000;
const MAX_REMOTE_AGENTS_ERROR_CHARS: usize = 1_000;

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
        RemoteWorkpath, replace_workpath_description, validate_remote_workpath,
        validate_remote_workpath_host,
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
}
