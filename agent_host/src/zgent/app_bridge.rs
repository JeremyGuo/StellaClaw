use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use agent_frame::Tool;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::sandbox::resolve_spawnable_current_exe;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZgentAppToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug)]
pub struct ZgentAppToolBridgeServer {
    address: SocketAddr,
    token: String,
    shutdown: Arc<AtomicBool>,
    join_handle: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentAppToolBridgeLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug)]
pub struct ZgentAppToolBundle {
    tools_file: PathBuf,
    bridge_server: ZgentAppToolBridgeServer,
}

#[derive(Debug)]
pub struct ZgentAppProfileBundle {
    profile_name: String,
    profile_dir: PathBuf,
    app_tools: ZgentAppToolBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct BridgeToolCallRequest {
    token: String,
    name: String,
    arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct BridgeToolCallResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

pub fn tool_definitions_from_tools(extra_tools: &[Tool]) -> Vec<ZgentAppToolDefinition> {
    extra_tools
        .iter()
        .map(|tool| ZgentAppToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.parameters.clone(),
        })
        .collect()
}

pub fn write_tool_definitions_file(path: &Path, extra_tools: &[Tool]) -> Result<()> {
    let definitions = tool_definitions_from_tools(extra_tools);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(&definitions).context("failed to serialize tool definitions")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

pub fn prepare_app_tool_bundle(
    runtime_state_root: &Path,
    extra_tools: &[Tool],
) -> Result<Option<ZgentAppToolBundle>> {
    if extra_tools.is_empty() {
        return Ok(None);
    }
    let bundle_root = runtime_state_root
        .join("zgent-app-bridge")
        .join(Uuid::new_v4().to_string());
    fs::create_dir_all(&bundle_root)
        .with_context(|| format!("failed to create {}", bundle_root.display()))?;
    let tools_file = bundle_root.join("tools.json");
    write_tool_definitions_file(&tools_file, extra_tools)?;
    let bridge_server = spawn_tool_bridge_server(extra_tools)?;
    Ok(Some(ZgentAppToolBundle {
        tools_file,
        bridge_server,
    }))
}

pub fn prepare_agent_host_profile(
    server_root: &Path,
    extra_tools: &[Tool],
) -> Result<Option<ZgentAppProfileBundle>> {
    let Some(app_tools) = prepare_app_tool_bundle(server_root, extra_tools)? else {
        return Ok(None);
    };
    let profile_name = format!("agenthost-bridge-{}", Uuid::new_v4().simple());
    let profile_dir = server_root.join("profiles").join(&profile_name);
    fs::create_dir_all(&profile_dir)
        .with_context(|| format!("failed to create {}", profile_dir.display()))?;
    let launch_spec = app_tools.launch_spec()?;
    let manifest = render_agent_host_profile_manifest(&profile_name, &launch_spec)?;
    fs::write(profile_dir.join("profile.toml"), manifest).with_context(|| {
        format!(
            "failed to write {}",
            profile_dir.join("profile.toml").display()
        )
    })?;
    Ok(Some(ZgentAppProfileBundle {
        profile_name,
        profile_dir,
        app_tools,
    }))
}

pub fn spawn_tool_bridge_server(extra_tools: &[Tool]) -> Result<ZgentAppToolBridgeServer> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("failed to bind zgent app tool bridge server")?;
    listener
        .set_nonblocking(true)
        .context("failed to configure zgent app tool bridge listener")?;
    let address = listener
        .local_addr()
        .context("failed to read zgent app tool bridge local address")?;
    let token = Uuid::new_v4().to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_flag = Arc::clone(&shutdown);
    let tools = Arc::new(
        extra_tools
            .iter()
            .cloned()
            .map(|tool| (tool.name.clone(), tool))
            .collect::<BTreeMap<_, _>>(),
    );
    let expected_token = token.clone();
    let join_handle = thread::spawn(move || {
        while !shutdown_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = handle_bridge_connection(stream, &tools, &expected_token);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    Ok(ZgentAppToolBridgeServer {
        address,
        token,
        shutdown,
        join_handle: Some(join_handle),
    })
}

impl ZgentAppToolBridgeServer {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl ZgentAppToolBundle {
    pub fn tools_file(&self) -> &Path {
        &self.tools_file
    }

    pub fn bridge_address(&self) -> SocketAddr {
        self.bridge_server.address()
    }

    pub fn bridge_token(&self) -> &str {
        self.bridge_server.token()
    }

    pub fn launch_spec(&self) -> Result<ZgentAppToolBridgeLaunchSpec> {
        Ok(ZgentAppToolBridgeLaunchSpec {
            program: resolve_app_bridge_program()?,
            args: vec![
                "run-zgent-app-bridge".to_string(),
                "--tools-file".to_string(),
                self.tools_file.display().to_string(),
                "--bridge-address".to_string(),
                self.bridge_address().to_string(),
                "--bridge-token".to_string(),
                self.bridge_token().to_string(),
            ],
        })
    }
}

fn resolve_app_bridge_program() -> Result<PathBuf> {
    let current = resolve_spawnable_current_exe()?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "agent_host" || name == "agent_host.exe")
    {
        return Ok(current);
    }

    if let (Some(parent), Some(file_name)) = (current.parent(), current.file_name()) {
        let in_deps_dir = parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "deps");
        let is_test_binary = file_name.to_str().is_some_and(|name| name.contains('-'));
        if in_deps_dir && is_test_binary {
            let candidate = parent.parent().unwrap_or(parent).join(if cfg!(windows) {
                "agent_host.exe"
            } else {
                "agent_host"
            });
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Ok(current)
}

impl ZgentAppProfileBundle {
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    pub fn tools_file(&self) -> &Path {
        self.app_tools.tools_file()
    }
}

impl Drop for ZgentAppToolBridgeServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub fn run_zgent_app_bridge_stdio(
    tools_file: &Path,
    bridge_address: &str,
    bridge_token: &str,
) -> Result<()> {
    let tool_definitions = load_tool_definitions_file(tools_file)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    loop {
        let request = read_framed_jsonrpc_message(&mut reader)
            .context("failed to read zgent app bridge request")?;
        let response = match request.method.as_str() {
            "app/initialize" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(json!({
                    "state": Value::Null,
                    "tools": tool_definitions,
                })),
                error: None,
            },
            "app/toolCall" => {
                let name = request
                    .params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("app/toolCall missing name"))?;
                let arguments = request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let bridge_response =
                    call_tool_bridge(bridge_address, bridge_token, name, arguments)?;
                if bridge_response.ok {
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: Some(bridge_response.result.unwrap_or(Value::Null)),
                        error: None,
                    }
                } else {
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32000,
                            message: bridge_response
                                .error
                                .unwrap_or_else(|| "tool bridge call failed".to_string()),
                        }),
                    }
                }
            }
            "app/shutdown" => {
                let response = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: Some(json!({ "ok": true })),
                    error: None,
                };
                write_framed_jsonrpc_message(&mut writer, &response)?;
                break;
            }
            _ => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", request.method),
                }),
            },
        };
        write_framed_jsonrpc_message(&mut writer, &response)?;
    }

    Ok(())
}

fn handle_bridge_connection(
    mut stream: TcpStream,
    tools: &BTreeMap<String, Tool>,
    expected_token: &str,
) -> Result<()> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("failed to clone bridge stream for reading")?,
    );
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("failed to read bridge request line")?;
    if request_line.trim().is_empty() {
        return Ok(());
    }
    let request: BridgeToolCallRequest =
        serde_json::from_str(request_line.trim()).context("failed to parse bridge request")?;
    let response = if request.token != expected_token {
        BridgeToolCallResponse {
            ok: false,
            result: None,
            error: Some("invalid tool bridge token".to_string()),
        }
    } else if let Some(tool) = tools.get(&request.name) {
        match tool.invoke(request.arguments) {
            Ok(result) => BridgeToolCallResponse {
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => BridgeToolCallResponse {
                ok: false,
                result: None,
                error: Some(format!("{error:#}")),
            },
        }
    } else {
        BridgeToolCallResponse {
            ok: false,
            result: None,
            error: Some(format!("unknown tool: {}", request.name)),
        }
    };
    let payload =
        serde_json::to_string(&response).context("failed to serialize bridge response")?;
    stream
        .write_all(payload.as_bytes())
        .context("failed to write bridge response")?;
    stream
        .write_all(b"\n")
        .context("failed to write bridge response newline")?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn call_tool_bridge(
    bridge_address: &str,
    bridge_token: &str,
    name: &str,
    arguments: Value,
) -> Result<BridgeToolCallResponse> {
    let mut stream = TcpStream::connect(bridge_address)
        .with_context(|| format!("failed to connect to {bridge_address}"))?;
    let payload = serde_json::to_string(&BridgeToolCallRequest {
        token: bridge_token.to_string(),
        name: name.to_string(),
        arguments,
    })
    .context("failed to serialize bridge tool call")?;
    stream
        .write_all(payload.as_bytes())
        .context("failed to write bridge tool call")?;
    stream
        .write_all(b"\n")
        .context("failed to write bridge tool call newline")?;
    stream
        .shutdown(Shutdown::Write)
        .context("failed to close bridge client write half")?;

    let mut body = String::new();
    BufReader::new(stream)
        .read_to_string(&mut body)
        .context("failed to read bridge tool response")?;
    serde_json::from_str(body.trim()).context("failed to parse bridge tool response")
}

fn load_tool_definitions_file(path: &Path) -> Result<Vec<ZgentAppToolDefinition>> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn render_agent_host_profile_manifest(
    profile_name: &str,
    launch_spec: &ZgentAppToolBridgeLaunchSpec,
) -> Result<String> {
    let command = toml_basic_string(&launch_spec.program.display().to_string())?;
    let args = launch_spec
        .args
        .iter()
        .map(|arg| toml_basic_string(arg))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    Ok(format!(
        "[profile]\nname = {name}\nversion = \"0.1.0\"\ndescription = \"AgentHost bridge profile\"\n\n[process]\ncommand = {command}\nargs = [{args}]\ncwd = \".\"\n",
        name = toml_basic_string(profile_name)?,
        command = command,
        args = args,
    ))
}

fn toml_basic_string(value: &str) -> Result<String> {
    serde_json::to_string(value).context("failed to encode TOML string")
}

fn write_framed_jsonrpc_message(writer: &mut impl Write, value: &JsonRpcResponse) -> Result<()> {
    let body =
        serde_json::to_vec(value).context("failed to serialize app bridge JSON-RPC response")?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .context("failed to write app bridge JSON-RPC header")?;
    writer
        .write_all(&body)
        .context("failed to write app bridge JSON-RPC body")?;
    writer
        .flush()
        .context("failed to flush app bridge JSON-RPC response")
}

fn read_framed_jsonrpc_message(reader: &mut impl BufRead) -> Result<JsonRpcRequest> {
    let content_length = read_content_length(reader)?;
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .context("failed to read app bridge JSON-RPC body")?;
    serde_json::from_slice(&body).context("failed to parse app bridge JSON-RPC request")
}

fn read_content_length(reader: &mut impl BufRead) -> Result<usize> {
    let mut line = String::new();
    let mut content_length = None;
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .context("failed to read app bridge JSON-RPC header line")?;
        if bytes == 0 {
            bail!("unexpected EOF while reading app bridge JSON-RPC headers");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length header")?,
            );
        }
    }
    content_length.ok_or_else(|| anyhow!("missing Content-Length header"))
}

#[cfg(test)]
mod tests {
    use super::{
        load_tool_definitions_file, prepare_agent_host_profile, prepare_app_tool_bundle,
        spawn_tool_bridge_server, tool_definitions_from_tools, write_tool_definitions_file,
    };
    use agent_frame::Tool;
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use tempfile::TempDir;

    #[test]
    fn tool_definitions_are_derived_from_extra_tools() {
        let extra_tools = vec![Tool::new(
            "user_tell",
            "Send progress to the user.",
            json!({"type":"object","properties":{"text":{"type":"string"}}}),
            |_| Ok(json!({"ok": true})),
        )];
        let defs = tool_definitions_from_tools(&extra_tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "user_tell");
        assert_eq!(defs[0].description, "Send progress to the user.");
        assert_eq!(defs[0].input_schema["type"], "object");
    }

    #[test]
    fn bridge_server_invokes_extra_tool() {
        let server = spawn_tool_bridge_server(&[Tool::new(
            "echo_tool",
            "Echo text back.",
            json!({"type":"object","properties":{"text":{"type":"string"}}}),
            |args| Ok(json!({"echo": args.get("text").cloned().unwrap_or(Value::Null)})),
        )])
        .unwrap();

        let mut stream = TcpStream::connect(server.address()).unwrap();
        let payload = json!({
            "token": server.token(),
            "name": "echo_tool",
            "arguments": {"text": "hello"}
        })
        .to_string();
        stream.write_all(payload.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut body = String::new();
        std::io::BufReader::new(stream)
            .read_to_string(&mut body)
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["echo"], "hello");
    }

    #[test]
    fn tool_definitions_file_round_trips() {
        let temp_dir = TempDir::new().unwrap();
        let tools_file = temp_dir.path().join("tools.json");
        let extra_tools = vec![Tool::new(
            "echo_tool",
            "Echo text back.",
            json!({"type":"object","properties":{"text":{"type":"string"}}}),
            |args| Ok(json!({"echo": args.get("text").cloned().unwrap_or(Value::Null)})),
        )];
        write_tool_definitions_file(&tools_file, &extra_tools).unwrap();
        let loaded = load_tool_definitions_file(&tools_file).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "echo_tool");
        assert_eq!(loaded[0].description, "Echo text back.");
        assert_eq!(loaded[0].input_schema["type"], "object");
    }

    #[test]
    fn prepare_bundle_writes_tools_file_and_builds_launch_spec() {
        let temp_dir = TempDir::new().unwrap();
        let extra_tools = vec![Tool::new(
            "user_tell",
            "Send progress to the user.",
            json!({"type":"object","properties":{"text":{"type":"string"}}}),
            |_| Ok(json!({"ok": true})),
        )];
        let bundle = prepare_app_tool_bundle(temp_dir.path(), &extra_tools)
            .unwrap()
            .expect("bundle");
        assert!(bundle.tools_file().is_file());
        let launch_spec = bundle.launch_spec().unwrap();
        assert!(launch_spec.program.exists());
        assert!(
            launch_spec
                .args
                .iter()
                .any(|arg| arg == "run-zgent-app-bridge")
        );
        assert!(launch_spec.args.iter().any(|arg| arg == "--tools-file"));
        assert!(launch_spec.args.iter().any(|arg| arg == "--bridge-address"));
        assert!(launch_spec.args.iter().any(|arg| arg == "--bridge-token"));
    }

    #[test]
    fn prepare_agent_host_profile_writes_manifest_and_keeps_bundle_alive() {
        let temp_dir = TempDir::new().unwrap();
        let extra_tools = vec![Tool::new(
            "user_tell",
            "Send progress to the user.",
            json!({"type":"object","properties":{"text":{"type":"string"}}}),
            |_| Ok(json!({"ok": true})),
        )];
        let bundle = prepare_agent_host_profile(temp_dir.path(), &extra_tools)
            .unwrap()
            .expect("profile bundle");
        let manifest = std::fs::read_to_string(bundle.profile_dir().join("profile.toml")).unwrap();
        assert!(manifest.contains("[profile]"));
        assert!(manifest.contains("AgentHost bridge profile"));
        assert!(manifest.contains("run-zgent-app-bridge"));
        assert!(bundle.tools_file().is_file());
        assert!(bundle.profile_name().starts_with("agenthost-bridge-"));
    }
}
