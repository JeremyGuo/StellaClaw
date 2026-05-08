use std::{
    collections::BTreeSet,
    env, fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::session_actor::tool_runtime::{
    bool_arg_with_default, clamp_tool_output_chars, run_command_with_timeout,
    run_remote_command_with_stdin, shell_quote, string_arg, string_arg_with_default,
    truncate_tool_text, usize_arg_with_default, ExecutionTarget, LocalToolError,
    ToolExecutionContext,
};

const APPLY_PATCH_TOOL_NAME: &str = "stellaclaw-apply-patch";
const APPLY_PATCH_TOOL_VERSION: &str = "0.1.2";
const APPLY_PATCH_MANIFEST_URL: &str = "https://github.com/JeremyGuo/StellaClaw/releases/download/stellaclaw-apply-patch-v0.1.2/tools-manifest.json";
const REMOTE_SAFE_PATH_PREFIX: &str =
    "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}; export PATH;";

pub(super) fn execute_patch_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if tool_name != "apply_patch" {
        return Ok(None);
    }

    let result = match apply_patch_execution_target(arguments, context)? {
        ExecutionTarget::Local => apply_patch_local(arguments, context.workspace_root)?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            apply_patch_remote(arguments, &host, cwd.as_deref())?
        }
    };
    Ok(Some(result))
}

fn apply_patch_execution_target(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<ExecutionTarget, LocalToolError> {
    if !matches!(
        context.remote_mode,
        crate::session_actor::ToolRemoteMode::FixedSsh { .. }
    ) {
        return context.execution_target(arguments);
    }
    let patch = string_arg(arguments, "patch")?;
    let format = patch_format(arguments, &patch)?;
    match classify_patch_target_paths(format, &patch, context.workspace_root)? {
        PatchTargetPaths::LocalSpecial => Ok(ExecutionTarget::Local),
        PatchTargetPaths::RemoteDefault | PatchTargetPaths::Unknown => {
            context.execution_target(arguments)
        }
    }
}

fn apply_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);
    match patch_format(arguments, &patch)? {
        PatchFormat::Codex => apply_codex_patch_local(arguments, workspace_root),
        PatchFormat::Unified => {
            apply_unified_patch_local(arguments, workspace_root, check, max_output_chars)
        }
    }
}

fn apply_unified_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    check: bool,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    let patch = normalize_unified_patch_paths(
        &string_arg(arguments, "patch")?,
        Some(workspace_root),
        "local workspace",
    )?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;

    let mut command = Command::new("git");
    command
        .arg("apply")
        .arg("--recount")
        .arg("--whitespace=nowarn")
        .arg(format!("-p{strip}"))
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if reverse {
        command.arg("--reverse");
    }
    if check {
        command.arg("--check");
    }

    let mut child = command
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn git apply: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| LocalToolError::Io("failed to open git apply stdin".to_string()))?
        .write_all(patch.as_bytes())
        .map_err(|error| LocalToolError::Io(format!("failed to write patch: {error}")))?;
    let _ = child.stdin.take();
    let output = child
        .wait_with_output()
        .map_err(|error| LocalToolError::Io(format!("failed to wait for git apply: {error}")))?;
    Ok(patch_result(output, None, max_output_chars))
}

fn apply_patch_remote(
    arguments: &Map<String, Value>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let format = patch_format(arguments, &patch)?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);

    let remote_binary = ensure_remote_apply_patch_tool(host)?;
    let mut args = vec![
        remote_binary,
        "--workspace".to_string(),
        ".".to_string(),
        "--format".to_string(),
        format.cli_name().to_string(),
        "--max-output-chars".to_string(),
        max_output_chars.to_string(),
    ];
    if check {
        args.push("--check".to_string());
    }
    if reverse {
        args.push("--reverse".to_string());
    }
    if strip != 0 {
        args.push("--strip".to_string());
        args.push(strip.to_string());
    }

    let remote_command = args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let remote_command = match cwd {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), remote_command),
        None => remote_command,
    };
    let remote_command = remote_shell_command(&remote_command);
    let output = run_remote_command_with_stdin(host, &remote_command, patch.as_bytes())?;
    if let Some(result) = parse_remote_apply_patch_json(&output, host) {
        return Ok(result);
    }
    Ok(patch_result(output, Some(host), max_output_chars))
}

fn parse_remote_apply_patch_json(output: &std::process::Output, host: &str) -> Option<Value> {
    let mut value = serde_json::from_slice::<Value>(&output.stdout).ok()?;
    if let Value::Object(object) = &mut value {
        object.insert("remote".to_string(), Value::String(host.to_string()));
    }
    Some(value)
}

fn normalize_unified_patch_paths(
    patch: &str,
    base: Option<&Path>,
    base_label: &str,
) -> Result<String, LocalToolError> {
    let mut changed = false;
    let mut output = String::with_capacity(patch.len());
    for segment in patch.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let normalized = if let Some(rest) = line.strip_prefix("--- ") {
            normalize_unified_file_header("--- ", rest, base, base_label, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            normalize_unified_file_header("+++ ", rest, base, base_label, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            normalize_diff_git_header(rest, base, base_label, &mut changed)?
        } else {
            line.to_string()
        };
        output.push_str(&normalized);
        output.push_str(newline);
    }
    if changed {
        Ok(output)
    } else {
        Ok(patch.to_string())
    }
}

fn normalize_unified_file_header(
    prefix: &str,
    rest: &str,
    base: Option<&Path>,
    base_label: &str,
    changed: &mut bool,
) -> Result<String, LocalToolError> {
    let (path, suffix) = split_unified_header_path(rest);
    let normalized = normalize_unified_path_token(path, base, base_label)?;
    if normalized != path {
        *changed = true;
    }
    Ok(format!("{prefix}{normalized}{suffix}"))
}

fn normalize_diff_git_header(
    rest: &str,
    base: Option<&Path>,
    base_label: &str,
    changed: &mut bool,
) -> Result<String, LocalToolError> {
    let mut parts = rest.split_whitespace();
    let Some(old_path) = parts.next() else {
        return Ok("diff --git ".to_string());
    };
    let Some(new_path) = parts.next() else {
        return Ok(format!("diff --git {rest}"));
    };
    if parts.next().is_some() {
        return Ok(format!("diff --git {rest}"));
    }
    let normalized_old = normalize_unified_path_token(old_path, base, base_label)?;
    let normalized_new = normalize_unified_path_token(new_path, base, base_label)?;
    if normalized_old != old_path || normalized_new != new_path {
        *changed = true;
    }
    Ok(format!("diff --git {normalized_old} {normalized_new}"))
}

fn split_unified_header_path(rest: &str) -> (&str, &str) {
    if let Some(index) = rest.find('\t') {
        return rest.split_at(index);
    }
    if rest.starts_with('/') {
        if let Some(index) = rest.find(char::is_whitespace) {
            return rest.split_at(index);
        }
    }
    (rest, "")
}

fn normalize_unified_path_token(
    path: &str,
    base: Option<&Path>,
    base_label: &str,
) -> Result<String, LocalToolError> {
    if path == "/dev/null" || !Path::new(path).is_absolute() {
        return Ok(path.to_string());
    }
    let Some(base) = base else {
        return Err(LocalToolError::InvalidArguments(format!(
            "unified patch path {path:?} is absolute; use a workspace-relative path"
        )));
    };
    let relative = Path::new(path).strip_prefix(base).map_err(|_| {
        LocalToolError::InvalidArguments(format!(
            "unified patch path {path:?} is absolute and outside the {base_label} {}; use a relative patch path",
            base.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Err(LocalToolError::InvalidArguments(format!(
            "unified patch path {path:?} points at the {base_label} root; use a file path"
        )));
    }
    Ok(relative.display().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchFormat {
    Codex,
    Unified,
}

impl PatchFormat {
    fn cli_name(self) -> &'static str {
        match self {
            PatchFormat::Codex => "codex",
            PatchFormat::Unified => "unified",
        }
    }
}

fn patch_format(
    arguments: &Map<String, Value>,
    patch: &str,
) -> Result<PatchFormat, LocalToolError> {
    match string_arg_with_default(arguments, "format", "auto")?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => {
            if patch.trim_start().starts_with("*** Begin Patch") {
                Ok(PatchFormat::Codex)
            } else {
                Ok(PatchFormat::Unified)
            }
        }
        "codex" => Ok(PatchFormat::Codex),
        "unified" => Ok(PatchFormat::Unified),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported patch format {other}; expected auto, codex, or unified"
        ))),
    }
}

fn ensure_remote_apply_patch_tool(host: &str) -> Result<String, LocalToolError> {
    let platform = detect_remote_apply_patch_platform(host)?;
    let local_binary = ensure_local_apply_patch_binary(&platform)?;
    let remote_state = remote_apply_patch_install_state(host, &platform)?;
    match remote_state {
        RemoteApplyPatchState::Ready { path } => Ok(path),
        RemoteApplyPatchState::Missing { path, tmp_path } => {
            copy_apply_patch_binary_to_remote(host, &local_binary, &tmp_path)?;
            let install = format!(
                "set -e; chmod 755 {tmp}; mv {tmp} {path}; printf '%s' {path}",
                tmp = shell_quote(&tmp_path),
                path = shell_quote(&path),
            );
            let install = remote_shell_command(&install);
            let output = run_remote_command_with_stdin(host, &install, b"")?;
            if !output.status.success() {
                return Err(LocalToolError::Remote(format!(
                    "failed to install remote {APPLY_PATCH_TOOL_NAME}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(path)
        }
    }
}

enum RemoteApplyPatchState {
    Ready { path: String },
    Missing { path: String, tmp_path: String },
}

fn detect_remote_apply_patch_platform(host: &str) -> Result<String, LocalToolError> {
    let command = remote_shell_command("printf '%s\\n%s\\n' \"$(uname -s)\" \"$(uname -m)\"");
    let output = run_remote_command_with_stdin(host, &command, b"")?;
    if !output.status.success() {
        return Err(LocalToolError::Remote(format!(
            "failed to detect remote platform: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let os = lines.next().unwrap_or_default();
    let arch = lines.next().unwrap_or_default();
    remote_platform_from_uname(os, arch).ok_or_else(|| {
        LocalToolError::Remote(format!(
            "unsupported remote platform for {APPLY_PATCH_TOOL_NAME}: {os} {arch}"
        ))
    })
}

fn remote_platform_from_uname(os: &str, arch: &str) -> Option<String> {
    let os = os.trim().to_ascii_lowercase();
    let arch = arch.trim().to_ascii_lowercase();
    match (os.as_str(), arch.as_str()) {
        ("linux", "x86_64") | ("linux", "amd64") => Some("linux-x64".to_string()),
        ("linux", "aarch64") | ("linux", "arm64") => Some("linux-arm64".to_string()),
        ("darwin", "x86_64") | ("darwin", "amd64") => Some("macos-x64".to_string()),
        ("darwin", "aarch64") | ("darwin", "arm64") => Some("macos-arm64".to_string()),
        _ => None,
    }
}

fn remote_apply_patch_install_state(
    host: &str,
    platform: &str,
) -> Result<RemoteApplyPatchState, LocalToolError> {
    let script = format!(
        "set -e; root=\"${{STELLACLAW_TOOL_CACHE_DIR:-${{HOME:-/tmp}}/.cache/stellaclaw/tools}}\"; dir=\"$root/{name}/{version}/{platform}\"; path=\"$dir/{name}\"; mkdir -p \"$dir\"; if [ -x \"$path\" ]; then printf 'ready\\n%s\\n' \"$path\"; else tmp=\"$dir/.{name}.incoming.$$\"; rm -f \"$tmp\"; printf 'missing\\n%s\\n%s\\n' \"$path\" \"$tmp\"; fi",
        name = APPLY_PATCH_TOOL_NAME,
        version = APPLY_PATCH_TOOL_VERSION,
        platform = platform,
    );
    let script = remote_shell_command(&script);
    let output = run_remote_command_with_stdin(host, &script, b"")?;
    if !output.status.success() {
        return Err(LocalToolError::Remote(format!(
            "failed to prepare remote {APPLY_PATCH_TOOL_NAME} cache: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    match lines.next().unwrap_or_default() {
        "ready" => Ok(RemoteApplyPatchState::Ready {
            path: lines.next().unwrap_or_default().to_string(),
        }),
        "missing" => Ok(RemoteApplyPatchState::Missing {
            path: lines.next().unwrap_or_default().to_string(),
            tmp_path: lines.next().unwrap_or_default().to_string(),
        }),
        other => Err(LocalToolError::Remote(format!(
            "unexpected remote {APPLY_PATCH_TOOL_NAME} cache response: {other}"
        ))),
    }
}

fn remote_shell_command(script: &str) -> String {
    format!("{REMOTE_SAFE_PATH_PREFIX} {script}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchTargetPaths {
    LocalSpecial,
    RemoteDefault,
    Unknown,
}

fn classify_patch_target_paths(
    format: PatchFormat,
    patch: &str,
    workspace_root: &Path,
) -> Result<PatchTargetPaths, LocalToolError> {
    let mut saw_local = false;
    let mut saw_remote = false;
    match format {
        PatchFormat::Codex => {
            for op in parse_codex_patch(patch)? {
                for path in codex_patch_op_paths(&op) {
                    if is_local_special_patch_path(path, workspace_root) {
                        saw_local = true;
                    } else {
                        saw_remote = true;
                    }
                }
            }
        }
        PatchFormat::Unified => {
            for path in unified_patch_header_paths(patch) {
                if path == "/dev/null" {
                    continue;
                }
                if is_local_special_patch_path(Path::new(path), workspace_root) {
                    saw_local = true;
                } else {
                    saw_remote = true;
                }
            }
        }
    }
    match (saw_local, saw_remote) {
        (true, false) => Ok(PatchTargetPaths::LocalSpecial),
        (false, true) => Ok(PatchTargetPaths::RemoteDefault),
        (false, false) => Ok(PatchTargetPaths::Unknown),
        (true, true) => Err(LocalToolError::InvalidArguments(
            "apply_patch cannot mix local .stellaclaw/workspace absolute paths and remote workspace paths in one patch while fixed remote mode is active".to_string(),
        )),
    }
}

fn codex_patch_op_paths(op: &CodexPatchOp) -> Vec<&Path> {
    match op {
        CodexPatchOp::Add { path, .. } | CodexPatchOp::Delete { path } => vec![path.as_path()],
        CodexPatchOp::Update { path, move_to, .. } => {
            let mut paths = vec![path.as_path()];
            if let Some(move_to) = move_to {
                paths.push(move_to.as_path());
            }
            paths
        }
    }
}

fn unified_patch_header_paths(patch: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            paths.push(split_unified_header_path(rest).0);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            paths.push(split_unified_header_path(rest).0);
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut parts = rest.split_whitespace();
            if let Some(path) = parts.next() {
                paths.push(path);
            }
            if let Some(path) = parts.next() {
                paths.push(path);
            }
        }
    }
    paths
}

fn is_local_special_patch_path(path: &Path, workspace_root: &Path) -> bool {
    if path.is_absolute() {
        return path.starts_with(workspace_root);
    }
    let path = strip_unified_side_prefix(path);
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return false;
    };
    first.to_string_lossy() == ".stellaclaw"
}

fn strip_unified_side_prefix(path: &Path) -> &Path {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return path;
    };
    let first = first.to_string_lossy();
    if first == "a" || first == "b" {
        components.as_path()
    } else {
        path
    }
}

fn copy_apply_patch_binary_to_remote(
    host: &str,
    local_binary: &Path,
    remote_tmp_path: &str,
) -> Result<(), LocalToolError> {
    let command = format!(
        "scp -p -o BatchMode=yes -o ConnectTimeout=10 {} {}:{}",
        shell_quote(&local_binary.display().to_string()),
        shell_quote(host),
        shell_quote(remote_tmp_path),
    );
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    let output = run_command_with_timeout(shell, Duration::from_secs(120), None, "scp")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(LocalToolError::Remote(format!(
            "failed to copy {APPLY_PATCH_TOOL_NAME}: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn ensure_local_apply_patch_binary(platform: &str) -> Result<PathBuf, LocalToolError> {
    let cache_dir = local_apply_patch_cache_dir(platform)?;
    let binary = cache_dir.join(APPLY_PATCH_TOOL_NAME);
    if binary.is_file() {
        return Ok(binary);
    }
    fs::create_dir_all(&cache_dir).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to create local {APPLY_PATCH_TOOL_NAME} cache {}: {error}",
            cache_dir.display()
        ))
    })?;
    install_local_apply_patch_binary(platform, &binary)?;
    Ok(binary)
}

fn local_apply_patch_cache_dir(platform: &str) -> Result<PathBuf, LocalToolError> {
    let root = env::var_os("STELLACLAW_SOFTWARE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/stellaclaw/tools"))
        })
        .ok_or_else(|| {
            LocalToolError::Io(
                "HOME or STELLACLAW_SOFTWARE_DIR is required for the local tool cache".to_string(),
            )
        })?;
    Ok(root
        .join(APPLY_PATCH_TOOL_NAME)
        .join(APPLY_PATCH_TOOL_VERSION)
        .join(platform))
}

fn install_local_apply_patch_binary(platform: &str, binary: &Path) -> Result<(), LocalToolError> {
    let manifest = fetch_apply_patch_manifest()?;
    let asset = manifest.asset_for_platform(platform).ok_or_else(|| {
        LocalToolError::Remote(format!("release manifest has no {platform} asset"))
    })?;
    let temp_dir = local_temp_dir(format!("{}-{platform}", APPLY_PATCH_TOOL_NAME))?;
    let archive = temp_dir.join(format!(
        "{APPLY_PATCH_TOOL_NAME}-{platform}.{}",
        asset.archive
    ));
    download_file(&asset.url, &archive)?;
    verify_sha256(&archive, &asset.sha256)?;
    extract_apply_patch_archive(&archive, &temp_dir, &asset.archive)?;
    let extracted = temp_dir
        .join(format!(
            "{APPLY_PATCH_TOOL_NAME}-v{APPLY_PATCH_TOOL_VERSION}-{platform}"
        ))
        .join(&asset.binary);
    if !extracted.is_file() {
        return Err(LocalToolError::Remote(format!(
            "downloaded {APPLY_PATCH_TOOL_NAME} archive did not contain {}",
            extracted.display()
        )));
    }
    let tmp_binary = binary.with_extension("incoming");
    if let Some(parent) = binary.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
        })?;
    }
    fs::copy(&extracted, &tmp_binary).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to stage {} to {}: {error}",
            extracted.display(),
            tmp_binary.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_binary, fs::Permissions::from_mode(0o755)).map_err(|error| {
            LocalToolError::Io(format!("failed to chmod {}: {error}", tmp_binary.display()))
        })?;
    }
    fs::rename(&tmp_binary, binary).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to install {} to {}: {error}",
            tmp_binary.display(),
            binary.display()
        ))
    })?;
    let _ = fs::remove_dir_all(temp_dir);
    Ok(())
}

#[derive(Debug)]
struct ApplyPatchManifest {
    assets: Vec<ApplyPatchAsset>,
}

#[derive(Debug)]
struct ApplyPatchAsset {
    platform: String,
    archive: String,
    url: String,
    sha256: String,
    binary: String,
}

impl ApplyPatchManifest {
    fn asset_for_platform(&self, platform: &str) -> Option<&ApplyPatchAsset> {
        self.assets.iter().find(|asset| asset.platform == platform)
    }
}

fn fetch_apply_patch_manifest() -> Result<ApplyPatchManifest, LocalToolError> {
    let value: Value = reqwest::blocking::get(APPLY_PATCH_MANIFEST_URL)
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json())
        .map_err(|error| {
            LocalToolError::Remote(format!(
                "failed to fetch {APPLY_PATCH_TOOL_NAME} manifest {APPLY_PATCH_MANIFEST_URL}: {error}"
            ))
        })?;
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version != APPLY_PATCH_TOOL_VERSION {
        return Err(LocalToolError::Remote(format!(
            "{APPLY_PATCH_TOOL_NAME} manifest version {version:?} does not match expected {APPLY_PATCH_TOOL_VERSION}"
        )));
    }
    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| LocalToolError::Remote("apply-patch manifest missing assets".to_string()))?
        .iter()
        .filter_map(|asset| {
            Some(ApplyPatchAsset {
                platform: asset.get("platform")?.as_str()?.to_string(),
                archive: asset.get("archive")?.as_str()?.to_string(),
                url: asset.get("url")?.as_str()?.to_string(),
                sha256: asset.get("sha256")?.as_str()?.to_string(),
                binary: asset.get("binary")?.as_str()?.to_string(),
            })
        })
        .collect::<Vec<_>>();
    Ok(ApplyPatchManifest { assets })
}

fn download_file(url: &str, path: &Path) -> Result<(), LocalToolError> {
    let bytes = reqwest::blocking::get(url)
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
        .map_err(|error| LocalToolError::Remote(format!("failed to download {url}: {error}")))?;
    fs::write(path, &bytes)
        .map_err(|error| LocalToolError::Io(format!("failed to write {}: {error}", path.display())))
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), LocalToolError> {
    let bytes = fs::read(path).map_err(|error| {
        LocalToolError::Io(format!("failed to read {}: {error}", path.display()))
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(LocalToolError::Remote(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected,
            actual
        )))
    }
}

fn extract_apply_patch_archive(
    archive: &Path,
    destination: &Path,
    archive_kind: &str,
) -> Result<(), LocalToolError> {
    let command = match archive_kind {
        "tar.gz" => {
            let mut command = Command::new("tar");
            command.arg("-xzf").arg(archive).arg("-C").arg(destination);
            command
        }
        "zip" => {
            let mut command = Command::new("unzip");
            command.arg("-q").arg(archive).arg("-d").arg(destination);
            command
        }
        other => {
            return Err(LocalToolError::Remote(format!(
                "unsupported {APPLY_PATCH_TOOL_NAME} archive format {other}"
            )));
        }
    };
    let output = run_command_with_timeout(
        command,
        Duration::from_secs(60),
        None,
        &format!("extract {APPLY_PATCH_TOOL_NAME}"),
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(LocalToolError::Remote(format!(
            "failed to extract {}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn local_temp_dir(label: String) -> Result<PathBuf, LocalToolError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path = env::temp_dir().join(format!("{label}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to create temp dir {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

#[derive(Debug, Clone)]
enum CodexPatchOp {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<CodexPatchChunk>,
    },
}

#[derive(Debug, Clone)]
struct CodexPatchChunk {
    old: String,
    new: String,
}

fn apply_codex_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
) -> Result<Value, LocalToolError> {
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    if strip != 0 || reverse {
        return Err(LocalToolError::InvalidArguments(
            "format=codex does not support strip or reverse".to_string(),
        ));
    }
    let patch = string_arg(arguments, "patch")?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let ops = parse_codex_patch(&patch)?;
    let mut files_changed = BTreeSet::new();
    for op in &ops {
        verify_codex_patch_op(op, workspace_root)?;
    }
    if !check {
        for op in &ops {
            apply_codex_patch_op(op, workspace_root, &mut files_changed)?;
        }
    } else {
        for op in &ops {
            collect_codex_patch_paths(op, &mut files_changed);
        }
    }

    let summary = json!({
        "format": "codex",
        "applied": true,
        "check": check,
        "files_changed": files_changed.iter().cloned().collect::<Vec<_>>(),
        "operation_count": ops.len(),
    });
    Ok(summary)
}

fn parse_codex_patch(patch: &str) -> Result<Vec<CodexPatchOp>, LocalToolError> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    expect_line(&lines, index, "*** Begin Patch")?;
    index += 1;
    let mut ops = Vec::new();
    loop {
        let Some(line) = lines.get(index).copied() else {
            return Err(LocalToolError::InvalidArguments(
                "codex patch missing *** End Patch".to_string(),
            ));
        };
        if line == "*** End Patch" {
            index += 1;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = safe_patch_path(path)?;
            index += 1;
            let mut content = String::new();
            while let Some(line) = lines.get(index).copied() {
                if is_codex_patch_header(line) {
                    break;
                }
                let Some(line) = line.strip_prefix('+') else {
                    return Err(LocalToolError::InvalidArguments(
                        "add file lines must start with +".to_string(),
                    ));
                };
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            if content.is_empty() {
                return Err(LocalToolError::InvalidArguments(
                    "add file section must contain at least one + line".to_string(),
                ));
            }
            ops.push(CodexPatchOp::Add { path, content });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(CodexPatchOp::Delete {
                path: safe_patch_path(path)?,
            });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = safe_patch_path(path)?;
            index += 1;
            let move_to = if let Some(line) = lines.get(index).copied() {
                if let Some(path) = line.strip_prefix("*** Move to: ") {
                    index += 1;
                    Some(safe_patch_path(path)?)
                } else {
                    None
                }
            } else {
                None
            };
            let mut chunks = Vec::new();
            let mut current = CodexPatchChunk {
                old: String::new(),
                new: String::new(),
            };
            while let Some(line) = lines.get(index).copied() {
                if is_codex_patch_header(line) {
                    break;
                }
                if line == "*** End of File" {
                    index += 1;
                    continue;
                }
                if line == "@@" || line.starts_with("@@ ") {
                    push_non_empty_chunk(&mut chunks, &mut current);
                    index += 1;
                    continue;
                }
                let Some((kind, text)) = split_patch_line(line) else {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "invalid update line: {line}"
                    )));
                };
                match kind {
                    ' ' => {
                        current.old.push_str(text);
                        current.old.push('\n');
                        current.new.push_str(text);
                        current.new.push('\n');
                    }
                    '-' => {
                        current.old.push_str(text);
                        current.old.push('\n');
                    }
                    '+' => {
                        current.new.push_str(text);
                        current.new.push('\n');
                    }
                    _ => unreachable!(),
                }
                index += 1;
            }
            push_non_empty_chunk(&mut chunks, &mut current);
            if move_to.is_none() && chunks.is_empty() {
                return Err(LocalToolError::InvalidArguments(
                    "update file section must contain changes or a move".to_string(),
                ));
            }
            ops.push(CodexPatchOp::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        return Err(LocalToolError::InvalidArguments(format!(
            "unknown codex patch header: {line}"
        )));
    }
    if lines[index..].iter().any(|line| !line.trim().is_empty()) {
        return Err(LocalToolError::InvalidArguments(
            "unexpected content after *** End Patch".to_string(),
        ));
    }
    if ops.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "codex patch must contain at least one file operation".to_string(),
        ));
    }
    Ok(ops)
}

fn expect_line(lines: &[&str], index: usize, expected: &str) -> Result<(), LocalToolError> {
    if lines.get(index).copied() == Some(expected) {
        Ok(())
    } else {
        Err(LocalToolError::InvalidArguments(format!(
            "codex patch must start with {expected}"
        )))
    }
}

fn is_codex_patch_header(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn split_patch_line(line: &str) -> Option<(char, &str)> {
    let mut chars = line.chars();
    let kind = chars.next()?;
    if !matches!(kind, ' ' | '-' | '+') {
        return None;
    }
    Some((kind, chars.as_str()))
}

fn push_non_empty_chunk(chunks: &mut Vec<CodexPatchChunk>, current: &mut CodexPatchChunk) {
    if current.old.is_empty() && current.new.is_empty() {
        return;
    }
    chunks.push(current.clone());
    current.old.clear();
    current.new.clear();
}

fn safe_patch_path(path: &str) -> Result<PathBuf, LocalToolError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "patch path must not be empty".to_string(),
        ));
    }
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(LocalToolError::InvalidArguments(
            "codex patch paths must be relative workspace paths without ..".to_string(),
        ));
    }
    Ok(path)
}

fn verify_codex_patch_op(op: &CodexPatchOp, workspace_root: &Path) -> Result<(), LocalToolError> {
    match op {
        CodexPatchOp::Add { path, .. } => {
            let target = workspace_root.join(path);
            if target.exists() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} already exists",
                    path.display()
                )));
            }
        }
        CodexPatchOp::Delete { path } => {
            let target = workspace_root.join(path);
            if !target.is_file() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} is not an existing file",
                    path.display()
                )));
            }
        }
        CodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_root.join(path);
            if !source.is_file() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} is not an existing file",
                    path.display()
                )));
            }
            if let Some(move_to) = move_to {
                let target = workspace_root.join(move_to);
                if target.exists() && move_to != path {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "{} already exists",
                        move_to.display()
                    )));
                }
            }
            let content = fs::read_to_string(&source).map_err(|error| {
                LocalToolError::Io(format!("failed to read {}: {error}", source.display()))
            })?;
            verify_chunks_match(path, &content, chunks)?;
        }
    }
    Ok(())
}

fn verify_chunks_match(
    path: &Path,
    content: &str,
    chunks: &[CodexPatchChunk],
) -> Result<(), LocalToolError> {
    let mut current = content.to_string();
    for chunk in chunks {
        if chunk.old.is_empty() {
            return Err(LocalToolError::InvalidArguments(format!(
                "update chunk for {} has no old/context lines",
                path.display()
            )));
        }
        let matches = current.matches(&chunk.old).count();
        if matches != 1 {
            return Err(LocalToolError::InvalidArguments(format!(
                "update chunk for {} matched {} locations; include more context",
                path.display(),
                matches
            )));
        }
        current = current.replacen(&chunk.old, &chunk.new, 1);
    }
    Ok(())
}

fn apply_codex_patch_op(
    op: &CodexPatchOp,
    workspace_root: &Path,
    files_changed: &mut BTreeSet<String>,
) -> Result<(), LocalToolError> {
    match op {
        CodexPatchOp::Add { path, content } => {
            let target = workspace_root.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
                })?;
            }
            fs::write(&target, content.as_bytes()).map_err(|error| {
                LocalToolError::Io(format!("failed to write {}: {error}", target.display()))
            })?;
            files_changed.insert(path.display().to_string());
        }
        CodexPatchOp::Delete { path } => {
            let target = workspace_root.join(path);
            fs::remove_file(&target).map_err(|error| {
                LocalToolError::Io(format!("failed to delete {}: {error}", target.display()))
            })?;
            files_changed.insert(path.display().to_string());
        }
        CodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_root.join(path);
            let mut content = fs::read_to_string(&source).map_err(|error| {
                LocalToolError::Io(format!("failed to read {}: {error}", source.display()))
            })?;
            for chunk in chunks {
                content = content.replacen(&chunk.old, &chunk.new, 1);
            }
            let output_path = move_to.as_ref().unwrap_or(path);
            let target = workspace_root.join(output_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
                })?;
            }
            fs::write(&target, content.as_bytes()).map_err(|error| {
                LocalToolError::Io(format!("failed to write {}: {error}", target.display()))
            })?;
            if move_to.as_ref().is_some_and(|move_to| move_to != path) {
                fs::remove_file(&source).map_err(|error| {
                    LocalToolError::Io(format!("failed to delete {}: {error}", source.display()))
                })?;
            }
            files_changed.insert(path.display().to_string());
            if let Some(move_to) = move_to {
                files_changed.insert(move_to.display().to_string());
            }
        }
    }
    Ok(())
}

fn collect_codex_patch_paths(op: &CodexPatchOp, files_changed: &mut BTreeSet<String>) {
    match op {
        CodexPatchOp::Add { path, .. }
        | CodexPatchOp::Delete { path }
        | CodexPatchOp::Update { path, .. } => {
            files_changed.insert(path.display().to_string());
        }
    }
    if let CodexPatchOp::Update {
        move_to: Some(move_to),
        ..
    } = op
    {
        files_changed.insert(move_to.display().to_string());
    }
}

fn patch_result(
    output: std::process::Output,
    remote: Option<&str>,
    max_output_chars: usize,
) -> Value {
    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, stdout_truncated) = truncate_tool_text(&stdout_text, max_output_chars);
    let (stderr, stderr_truncated) = truncate_tool_text(&stderr_text, max_output_chars);

    let mut result = Map::new();
    result.insert("applied".to_string(), Value::Bool(output.status.success()));
    if let Some(returncode) = output.status.code() {
        result.insert("returncode".to_string(), Value::from(returncode));
    }
    if let Some(remote) = remote {
        result.insert("remote".to_string(), Value::String(remote.to_string()));
    }
    if !stdout.is_empty() {
        result.insert("stdout".to_string(), Value::String(stdout));
    }
    if !stderr.is_empty() {
        result.insert("stderr".to_string(), Value::String(stderr));
    }
    if stdout_truncated {
        result.insert("stdout_truncated".to_string(), Value::Bool(true));
    }
    if stderr_truncated {
        result.insert("stderr_truncated".to_string(), Value::Bool(true));
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_absolute_unified_paths_under_base() {
        let patch = "\
diff --git /home/me/work/src/a.py /home/me/work/src/a.py
--- /home/me/work/src/a.py\t2026-05-08
+++ /home/me/work/src/a.py\t2026-05-08
@@ -1 +1 @@
-old
+new
";

        let normalized =
            normalize_unified_patch_paths(patch, Some(Path::new("/home/me/work")), "remote cwd")
                .expect("patch should normalize");

        assert!(normalized.contains("diff --git src/a.py src/a.py"));
        assert!(normalized.contains("--- src/a.py\t2026-05-08"));
        assert!(normalized.contains("+++ src/a.py\t2026-05-08"));
    }

    #[test]
    fn rejects_absolute_unified_paths_outside_base() {
        let patch = "\
--- /other/work/src/a.py
+++ /other/work/src/a.py
@@ -1 +1 @@
-old
+new
";

        let error =
            normalize_unified_patch_paths(patch, Some(Path::new("/home/me/work")), "remote cwd")
                .expect_err("outside path should be rejected");

        assert!(error
            .to_string()
            .contains("outside the remote cwd /home/me/work"));
    }

    #[test]
    fn maps_remote_uname_to_apply_patch_release_platforms() {
        assert_eq!(
            remote_platform_from_uname("Linux", "x86_64").as_deref(),
            Some("linux-x64")
        );
        assert_eq!(
            remote_platform_from_uname("Linux", "aarch64").as_deref(),
            Some("linux-arm64")
        );
        assert_eq!(
            remote_platform_from_uname("Darwin", "x86_64").as_deref(),
            Some("macos-x64")
        );
        assert_eq!(
            remote_platform_from_uname("Darwin", "arm64").as_deref(),
            Some("macos-arm64")
        );
        assert_eq!(remote_platform_from_uname("FreeBSD", "x86_64"), None);
    }

    #[test]
    fn remote_shell_command_sets_safe_path() {
        let command = remote_shell_command("mkdir -p \"$dir\"");
        assert!(command.starts_with("PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(command.contains("export PATH;"));
        assert!(command.ends_with("mkdir -p \"$dir\""));
    }

    #[test]
    fn classifies_absolute_workspace_patch_as_local_special() {
        let patch = "\
--- /home/me/work/.stellaclaw/apply_patch_smoke_test.txt
+++ /home/me/work/.stellaclaw/apply_patch_smoke_test.txt
@@ -1 +1 @@
-old
+new
";

        let classification =
            classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
                .expect("classification should succeed");

        assert_eq!(classification, PatchTargetPaths::LocalSpecial);
    }

    #[test]
    fn classifies_stellaclaw_git_style_patch_as_local_special() {
        let patch = "\
diff --git a/.stellaclaw/a.txt b/.stellaclaw/a.txt
--- a/.stellaclaw/a.txt
+++ b/.stellaclaw/a.txt
@@ -1 +1 @@
-old
+new
";

        let classification =
            classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
                .expect("classification should succeed");

        assert_eq!(classification, PatchTargetPaths::LocalSpecial);
    }

    #[test]
    fn classifies_ordinary_relative_patch_as_remote_default() {
        let patch = "\
--- src/main.rs
+++ src/main.rs
@@ -1 +1 @@
-old
+new
";

        let classification =
            classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
                .expect("classification should succeed");

        assert_eq!(classification, PatchTargetPaths::RemoteDefault);
    }

    #[test]
    fn rejects_mixed_local_special_and_remote_patch_paths() {
        let patch = "\
--- .stellaclaw/local.txt
+++ .stellaclaw/local.txt
@@ -1 +1 @@
-old
+new
--- src/remote.rs
+++ src/remote.rs
@@ -1 +1 @@
-old
+new
";

        let error =
            classify_patch_target_paths(PatchFormat::Unified, patch, Path::new("/home/me/work"))
                .expect_err("mixed paths should be rejected");

        assert!(error.to_string().contains("cannot mix local .stellaclaw"));
    }
}
