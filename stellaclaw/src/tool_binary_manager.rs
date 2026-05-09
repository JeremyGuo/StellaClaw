use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stellaclaw_core::session_actor::{ToolBinaryEnsureRequest, ToolBinaryEnsureResponse};

use crate::config::{SandboxConfig, SandboxMode};

const SAFE_REMOTE_PATH: &str =
    "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}; export PATH;";

const FS_TOOL_NAME: &str = "stellaclaw-fs-tool";
const FS_TOOL_VERSION: &str = "0.2.0";
const FS_TOOL_MANIFEST_URL: &str = "https://github.com/JeremyGuo/StellaClaw/releases/download/stellaclaw-fs-tool-v0.2.0/tools-manifest.json";

const RIPGREP_TOOL_NAME: &str = "ripgrep";
const RIPGREP_VERSION: &str = "15.1.0";

#[derive(Clone)]
pub struct ToolBinaryClient {
    tx: mpsc::Sender<ToolBinaryCommand>,
}

struct ToolBinaryCommand {
    request: ToolBinaryEnsureRequest,
    runtime: ToolBinaryRuntime,
    response: mpsc::Sender<Result<ToolBinaryEnsureResponse, String>>,
}

pub fn shared_tool_binary_client() -> ToolBinaryClient {
    static CLIENT: OnceLock<ToolBinaryClient> = OnceLock::new();
    CLIENT.get_or_init(ToolBinaryManager::start).clone()
}

impl ToolBinaryClient {
    pub fn ensure(
        &self,
        request: ToolBinaryEnsureRequest,
        sandbox: &SandboxConfig,
    ) -> Result<ToolBinaryEnsureResponse, String> {
        let (response_tx, response_rx) = mpsc::channel();
        self.tx
            .send(ToolBinaryCommand {
                request,
                runtime: ToolBinaryRuntime::from_sandbox(sandbox),
                response: response_tx,
            })
            .map_err(|error| format!("tool binary manager is unavailable: {error}"))?;
        response_rx
            .recv()
            .map_err(|error| format!("tool binary manager response failed: {error}"))?
    }
}

struct ToolBinaryManager {
    lock: Mutex<()>,
}

#[derive(Clone)]
struct ToolBinaryRuntime {
    local_cache_root: PathBuf,
    local_visible_root: PathBuf,
}

impl ToolBinaryRuntime {
    fn from_sandbox(sandbox: &SandboxConfig) -> Self {
        let software_root = sandbox_software_dir_or_default(sandbox);
        let cache_root = tool_cache_root(&software_root);
        let visible_root = match sandbox.mode {
            SandboxMode::Bubblewrap => {
                tool_cache_root(Path::new(sandbox.software_mount_path.trim()))
            }
            SandboxMode::Subprocess => cache_root.clone(),
        };
        Self {
            local_cache_root: cache_root,
            local_visible_root: visible_root,
        }
    }
}

impl ToolBinaryManager {
    fn start() -> ToolBinaryClient {
        let (tx, rx) = mpsc::channel::<ToolBinaryCommand>();
        thread::Builder::new()
            .name("stellaclaw-tool-binary-manager".to_string())
            .spawn(move || {
                let manager = ToolBinaryManager {
                    lock: Mutex::new(()),
                };
                while let Ok(command) = rx.recv() {
                    let result = manager
                        .ensure(command.request, command.runtime)
                        .map_err(|error| format!("{error:#}"));
                    let _ = command.response.send(result);
                }
            })
            .expect("failed to spawn tool binary manager");
        ToolBinaryClient { tx }
    }

    fn ensure(
        &self,
        request: ToolBinaryEnsureRequest,
        runtime: ToolBinaryRuntime,
    ) -> Result<ToolBinaryEnsureResponse> {
        let _guard = self.lock.lock().expect("tool binary manager lock poisoned");
        let spec = spec_for_tool(&request.tool)?;
        match request
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
        {
            Some(host) => ensure_remote(spec, host, &runtime),
            None => ensure_local(spec, &runtime),
        }
    }
}

#[derive(Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    version: &'static str,
    assets: ToolAssets,
}

#[derive(Clone, Copy)]
enum ToolAssets {
    Manifest(&'static str),
    Static(&'static [StaticToolAsset]),
}

#[derive(Clone)]
struct ToolAsset {
    archive: String,
    url: String,
    sha256: String,
    binary: String,
}

impl ToolSpec {
    fn asset_for_platform(&self, platform: &str) -> Result<ToolAsset> {
        match self.assets {
            ToolAssets::Static(assets) => assets
                .iter()
                .find(|asset| asset.platform == platform)
                .map(|asset| ToolAsset {
                    archive: asset.archive.to_string(),
                    url: asset.url.to_string(),
                    sha256: asset.sha256.to_string(),
                    binary: asset.binary.to_string(),
                })
                .ok_or_else(|| anyhow!("{} has no {platform} asset", self.name)),
            ToolAssets::Manifest(url) => fetch_manifest_asset(self, url, platform),
        }
    }
}

#[derive(Clone, Copy)]
struct StaticToolAsset {
    platform: &'static str,
    archive: &'static str,
    url: &'static str,
    sha256: &'static str,
    binary: &'static str,
}

fn spec_for_tool(tool: &str) -> Result<ToolSpec> {
    match tool {
        FS_TOOL_NAME => Ok(ToolSpec {
            name: FS_TOOL_NAME,
            version: FS_TOOL_VERSION,
            assets: ToolAssets::Manifest(FS_TOOL_MANIFEST_URL),
        }),
        RIPGREP_TOOL_NAME | "rg" => Ok(ToolSpec {
            name: RIPGREP_TOOL_NAME,
            version: RIPGREP_VERSION,
            assets: ToolAssets::Static(RIPGREP_ASSETS),
        }),
        other => Err(anyhow!("unsupported managed tool binary {other}")),
    }
}

const RIPGREP_ASSETS: &[StaticToolAsset] = &[
    StaticToolAsset {
        platform: "linux-x64",
        archive: "tar.gz",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz",
        sha256: "1c9297be4a084eea7ecaedf93eb03d058d6faae29bbc57ecdaf5063921491599",
        binary: "rg",
    },
    StaticToolAsset {
        platform: "linux-arm64",
        archive: "tar.gz",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-aarch64-unknown-linux-gnu.tar.gz",
        sha256: "2b661c6ef508e902f388e9098d9c4c5aca72c87b55922d94abdba830b4dc885e",
        binary: "rg",
    },
    StaticToolAsset {
        platform: "macos-x64",
        archive: "tar.gz",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-x86_64-apple-darwin.tar.gz",
        sha256: "64811cb24e77cac3057d6c40b63ac9becf9082eedd54ca411b475b755d334882",
        binary: "rg",
    },
    StaticToolAsset {
        platform: "macos-arm64",
        archive: "tar.gz",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-aarch64-apple-darwin.tar.gz",
        sha256: "378e973289176ca0c6054054ee7f631a065874a352bf43f0fa60ef079b6ba715",
        binary: "rg",
    },
    StaticToolAsset {
        platform: "windows-x64",
        archive: "zip",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-x86_64-pc-windows-msvc.zip",
        sha256: "124510b94b6baa3380d051fdf4650eaa80a302c876d611e9dba0b2e18d87493a",
        binary: "rg.exe",
    },
];

fn ensure_local(spec: ToolSpec, runtime: &ToolBinaryRuntime) -> Result<ToolBinaryEnsureResponse> {
    let platform = local_platform()?;
    let asset = spec.asset_for_platform(&platform)?;
    let binary = local_binary_path(&runtime.local_cache_root, spec, &platform, &asset);
    if !binary.is_file() {
        install_local_binary(spec, &platform, &asset, &binary)?;
    }
    let visible_binary = visible_binary_path(runtime, spec, &platform, &asset);
    Ok(ToolBinaryEnsureResponse {
        status: "success".to_string(),
        tool: spec.name.to_string(),
        version: spec.version.to_string(),
        platform: Some(platform),
        path_dir: visible_binary
            .parent()
            .map(|path| path.display().to_string()),
        local_path: Some(visible_binary.display().to_string()),
        remote_path: None,
    })
}

fn ensure_remote(
    spec: ToolSpec,
    host: &str,
    runtime: &ToolBinaryRuntime,
) -> Result<ToolBinaryEnsureResponse> {
    let platform = remote_platform(host)?;
    let asset = spec.asset_for_platform(&platform)?;
    let local_binary = local_binary_path(&runtime.local_cache_root, spec, &platform, &asset);
    if !local_binary.is_file() {
        install_local_binary(spec, &platform, &asset, &local_binary)?;
    }
    let remote_home = remote_home_dir(host)?;
    let remote_path = remote_binary_path(&remote_home, spec, &platform, &asset);
    let remote_dir = remote_parent(&remote_path);
    let check = format!("{SAFE_REMOTE_PATH} test -x {}", shell_quote(&remote_path));
    if !run_remote_shell(host, &check, Duration::from_secs(20))?
        .status
        .success()
    {
        let mkdir = format!("{SAFE_REMOTE_PATH} mkdir -p {}", shell_quote(&remote_dir));
        let output = run_remote_shell(host, &mkdir, Duration::from_secs(20))?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to prepare remote tool cache: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        copy_to_remote(host, &local_binary, &remote_path)?;
        let chmod = format!("{SAFE_REMOTE_PATH} chmod 755 {}", shell_quote(&remote_path));
        let output = run_remote_shell(host, &chmod, Duration::from_secs(20))?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to chmod remote tool binary: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(ToolBinaryEnsureResponse {
        status: "success".to_string(),
        tool: spec.name.to_string(),
        version: spec.version.to_string(),
        platform: Some(platform),
        local_path: None,
        remote_path: Some(remote_path.clone()),
        path_dir: Some(remote_dir),
    })
}

fn fetch_manifest_asset(spec: &ToolSpec, manifest_url: &str, platform: &str) -> Result<ToolAsset> {
    let value: Value = reqwest::blocking::get(manifest_url)
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json())
        .with_context(|| format!("failed to fetch {} manifest", spec.name))?;
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version != spec.version {
        return Err(anyhow!(
            "{} manifest version {version:?} does not match expected {}",
            spec.name,
            spec.version
        ));
    }
    let asset = value
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets
                .iter()
                .find(|asset| asset.get("platform").and_then(Value::as_str) == Some(platform))
        })
        .ok_or_else(|| anyhow!("{} manifest has no {platform} asset", spec.name))?;
    Ok(ToolAsset {
        archive: asset_string(asset, "archive")?,
        url: asset_string(asset, "url")?,
        sha256: asset_string(asset, "sha256")?,
        binary: asset_string(asset, "binary")?,
    })
}

fn asset_string(asset: &Value, key: &str) -> Result<String> {
    asset
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("manifest asset missing {key}"))
}

fn local_platform() -> Result<String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x64".to_string()),
        ("linux", "aarch64") => Ok("linux-arm64".to_string()),
        ("macos", "x86_64") => Ok("macos-x64".to_string()),
        ("macos", "aarch64") => Ok("macos-arm64".to_string()),
        ("windows", "x86_64") => Ok("windows-x64".to_string()),
        (os, arch) => Err(anyhow!("unsupported managed tool platform: {os} {arch}")),
    }
}

fn remote_platform(host: &str) -> Result<String> {
    let script = format!("{SAFE_REMOTE_PATH} os=$(uname -s 2>/dev/null || /usr/bin/uname -s); arch=$(uname -m 2>/dev/null || /usr/bin/uname -m); printf '%s %s' \"$os\" \"$arch\"");
    let output = run_remote_shell(host, &script, Duration::from_secs(20))?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to detect remote platform: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let os = parts.next().unwrap_or_default();
    let arch = parts.next().unwrap_or_default();
    match (os, arch) {
        ("Linux", "x86_64") | ("Linux", "amd64") => Ok("linux-x64".to_string()),
        ("Linux", "aarch64") | ("Linux", "arm64") => Ok("linux-arm64".to_string()),
        ("Darwin", "x86_64") => Ok("macos-x64".to_string()),
        ("Darwin", "arm64") | ("Darwin", "aarch64") => Ok("macos-arm64".to_string()),
        (os, arch) => Err(anyhow!(
            "unsupported remote managed tool platform on {host}: {os} {arch}"
        )),
    }
}

fn local_binary_path(root: &Path, spec: ToolSpec, platform: &str, asset: &ToolAsset) -> PathBuf {
    root.join(spec.name)
        .join(spec.version)
        .join(platform)
        .join(&asset.binary)
}

fn visible_binary_path(
    runtime: &ToolBinaryRuntime,
    spec: ToolSpec,
    platform: &str,
    asset: &ToolAsset,
) -> PathBuf {
    local_binary_path(&runtime.local_visible_root, spec, platform, asset)
}

fn sandbox_software_dir_or_default(sandbox: &SandboxConfig) -> PathBuf {
    sandbox
        .software_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(default_software_dir)
        .unwrap_or_else(|| env::temp_dir().join("stellaclaw-software"))
}

fn default_software_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
}

fn tool_cache_root(software_root: &Path) -> PathBuf {
    software_root.join("stellaclaw").join("tools")
}

fn remote_home_dir(host: &str) -> Result<String> {
    let script = format!("{SAFE_REMOTE_PATH} printf '%s' \"${{HOME:-/tmp}}\"");
    let output = run_remote_shell(host, &script, Duration::from_secs(20))?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to detect remote home directory: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let home = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if home.is_empty() {
        Err(anyhow!("remote home directory is empty"))
    } else {
        Ok(home)
    }
}

fn remote_binary_path(home: &str, spec: ToolSpec, platform: &str, asset: &ToolAsset) -> String {
    format!(
        "{}/.cache/stellaclaw/tools/{}/{}/{}/{}",
        home.trim_end_matches('/'),
        spec.name,
        spec.version,
        platform,
        asset.binary
    )
}

fn remote_parent(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or(path)
        .to_string()
}

fn install_local_binary(
    spec: ToolSpec,
    platform: &str,
    asset: &ToolAsset,
    binary: &Path,
) -> Result<()> {
    let temp_dir = local_temp_dir(format!("{}-{platform}", spec.name))?;
    let archive = temp_dir.join(format!("{}-{platform}.{}", spec.name, asset.archive));
    download_file(&asset.url, &archive)?;
    verify_sha256(&archive, &asset.sha256)?;
    extract_archive(&archive, &temp_dir, &asset.archive)?;
    let extracted = find_extracted_binary(&temp_dir, &asset.binary)?;
    if let Some(parent) = binary.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp_binary = binary.with_extension("incoming");
    fs::copy(&extracted, &tmp_binary).with_context(|| {
        format!(
            "failed to stage {} to {}",
            extracted.display(),
            tmp_binary.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_binary, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to chmod {}", tmp_binary.display()))?;
    }
    fs::rename(&tmp_binary, binary).with_context(|| {
        format!(
            "failed to install {} to {}",
            tmp_binary.display(),
            binary.display()
        )
    })?;
    let _ = fs::remove_dir_all(temp_dir);
    Ok(())
}

fn download_file(url: &str, path: &Path) -> Result<()> {
    let bytes = reqwest::blocking::get(url)
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
        .with_context(|| format!("failed to download {url}"))?;
    fs::write(path, &bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(anyhow!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected,
            actual
        ))
    }
}

fn extract_archive(archive: &Path, destination: &Path, archive_kind: &str) -> Result<()> {
    let mut command = match archive_kind {
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
        other => return Err(anyhow!("unsupported archive format {other}")),
    };
    let output = run_command_with_timeout(&mut command, Duration::from_secs(60), None)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to extract {}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn find_extracted_binary(root: &Path, binary_name: &str) -> Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in
            fs::read_dir(&path).with_context(|| format!("failed to read {}", path.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
                return Ok(path);
            }
        }
    }
    Err(anyhow!("downloaded archive did not contain {binary_name}"))
}

fn local_temp_dir(label: String) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path = env::temp_dir().join(format!("{label}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

fn copy_to_remote(host: &str, local_binary: &Path, remote_path: &str) -> Result<()> {
    let mut command = Command::new("scp");
    command
        .arg("-p")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(local_binary)
        .arg(format!("{host}:{remote_path}"));
    let output = run_command_with_timeout(&mut command, Duration::from_secs(120), None)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to copy managed tool binary: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn run_remote_shell(host: &str, script: &str, timeout: Duration) -> Result<Output> {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("--")
        .arg(script);
    run_command_with_timeout(&mut command, timeout, None)
}

fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    if stdin.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().context("failed to spawn command")?;
    if let (Some(input), Some(mut child_stdin)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        child_stdin.write_all(input)?;
    }
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .context("failed to collect command output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .context("failed to collect timed out command output")?;
            return Err(anyhow!(
                "command timed out after {}s: {}",
                timeout.as_secs(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}
