use agent_frame::compaction::COMPACTION_MARKER;
use agent_frame::message::ChatMessage;
use agent_frame::skills::{build_skills_meta_prompt, discover_skills};
use agent_frame::tool;
use agent_frame::tooling::{
    build_tool_registry, execute_tool_call, terminate_all_managed_processes,
};
use agent_frame::{
    AgentConfig, ExecutionProgressPhase, ExternalWebSearchConfig, NativeWebSearchConfig,
    SessionEvent, SessionExecutionControl, SessionState, Tool, ToolExecutionStatus, UpstreamConfig,
    compact_session_messages_with_report, extract_assistant_text, load_config_value, run_session,
    run_session_state, run_session_state_controlled,
};
use anyhow::Result;
use assert_cmd::Command;
use image::ImageFormat;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct TestServer {
    address: String,
    responses: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

fn process_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn acquire_process_test_lock() -> std::sync::MutexGuard<'static, ()> {
    process_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

#[cfg(unix)]
fn write_fake_ssh(temp_dir: &TempDir) -> PathBuf {
    let path = temp_dir.path().join("fake-ssh");
    fs::write(
        &path,
        r#"#!/bin/sh
while [ "$1" = "-o" ]; do
  shift 2
done
if [ "$1" = "-T" ] || [ "$1" = "-tt" ]; then
  shift
fi
shift
remote_command="$*"
exec sh -c "$remote_command"
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn structured_compaction_response_text() -> String {
    json!({
        "old_summary": "",
        "new_summary": "- Keep working on the task\n- Stay concise",
        "keywords": ["task"],
        "important_refs": {
            "paths": [],
            "commands": [],
            "errors": [],
            "urls": [],
            "ids": []
        },
        "memory_hints": [],
        "next_step": "Continue the task."
    })
    .to_string()
}

fn long_distinct_text(prefix: &str, count: usize) -> String {
    (0..count)
        .map(|index| format!("{prefix}-{index:04}-payload "))
        .collect()
}

impl TestServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on test server");
        let address = format!("http://{}", listener.local_addr().expect("local addr"));
        let responses = Arc::new(Mutex::new(VecDeque::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => handle_stream(stream, &responses, &requests),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept failed: {}", error),
                    }
                }
            })
        };

        Self {
            address,
            responses,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn push_response(&self, value: Value) {
        self.responses.lock().unwrap().push_back(value);
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address.trim_start_matches("http://"));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_session_state_until_end(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionState> {
    let mut state = run_session_state_controlled(
        previous_messages,
        prompt,
        config.clone(),
        extra_tools.clone(),
        control.clone(),
    )?;
    let mut usage = state.usage.clone();
    let mut compaction = state.compaction.clone();
    for _ in 0..16 {
        if state.phase != agent_frame::SessionPhase::Yielded || state.errno.is_some() {
            state.usage = usage;
            state.compaction = compaction;
            return Ok(state);
        }
        state = run_session_state_controlled(
            state.messages.clone(),
            "",
            config.clone(),
            extra_tools.clone(),
            control.clone(),
        )?;
        usage.add_assign(&state.usage);
        compaction.run_count = compaction
            .run_count
            .saturating_add(state.compaction.run_count);
        compaction.compacted_run_count = compaction
            .compacted_run_count
            .saturating_add(state.compaction.compacted_run_count);
        compaction.estimated_tokens_before = compaction
            .estimated_tokens_before
            .saturating_add(state.compaction.estimated_tokens_before);
        compaction.estimated_tokens_after = compaction
            .estimated_tokens_after
            .saturating_add(state.compaction.estimated_tokens_after);
        compaction.usage.add_assign(&state.compaction.usage);
    }
    anyhow::bail!("session did not reach End after auto-resume attempts")
}

fn handle_stream(
    mut stream: TcpStream,
    responses: &Arc<Mutex<VecDeque<Value>>>,
    requests: &Arc<Mutex<Vec<Value>>>,
) {
    stream
        .set_nonblocking(false)
        .expect("set blocking mode on accepted stream");
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        let bytes_read = stream.read(&mut chunk).expect("read request");
        if bytes_read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..bytes_read]);
        if header_end.is_none()
            && let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
        {
            header_end = Some(index);
            let headers = String::from_utf8_lossy(&buffer[..index]).to_string();
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if !name.eq_ignore_ascii_case("content-length") {
                        return None;
                    }
                    value.trim().parse::<usize>().ok()
                })
                .unwrap_or(0);
        }
        if let Some(index) = header_end {
            let body_len = buffer.len().saturating_sub(index + 4);
            if body_len >= content_length {
                break;
            }
        }
    }
    let header_end = header_end.expect("header end");
    let request_text = String::from_utf8_lossy(&buffer).to_string();
    let headers = &request_text[..header_end];
    let body = &request_text[header_end + 4..header_end + 4 + content_length];
    let request_line = headers.lines().next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("method");
    let path = parts.next().expect("path");

    if method == "GET" {
        if path == "/binary" {
            let mut body = Vec::new();
            image::DynamicImage::new_rgba8(1, 1)
                .write_to(&mut Cursor::new(&mut body), ImageFormat::Png)
                .expect("encode binary get response body");
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(header.as_bytes())
                .expect("write binary get response header");
            stream
                .write_all(&body)
                .expect("write binary get response body");
            return;
        }
        let body = json!({"ok": true, "path": path}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write get response");
        return;
    }

    let body_json: Value = serde_json::from_str(body).expect("parse request body");
    requests.lock().unwrap().push(body_json);
    let response_json = responses
        .lock()
        .unwrap()
        .pop_front()
        .expect("queued response");
    let response_body = response_json.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    stream
        .write_all(response.as_bytes())
        .expect("write post response");
}

fn write_config(path: &Path, body: Value) {
    fs::write(path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
}

fn test_upstream(base_url: &str) -> UpstreamConfig {
    UpstreamConfig {
        base_url: base_url.to_string(),
        model: "test-model".to_string(),
        api_kind: agent_frame::config::UpstreamApiKind::ChatCompletions,
        auth_kind: agent_frame::config::UpstreamAuthKind::ApiKey,
        supports_vision_input: false,
        supports_pdf_input: false,
        supports_audio_input: false,
        api_key: None,
        api_key_env: "TEST_API_KEY".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        codex_home: None,
        codex_auth: None,
        auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
        timeout_seconds: 10.0,
        retry_mode: Default::default(),
        context_window_tokens: 128_000,
        cache_control: None,
        prompt_cache_retention: None,
        prompt_cache_key: None,
        reasoning: None,
        headers: serde_json::Map::new(),
        native_web_search: None,
        external_web_search: None,
        native_image_input: false,
        native_pdf_input: false,
        native_audio_input: false,
        native_image_generation: false,
        token_estimation: None,
    }
}

#[test]
fn discover_skills_and_build_meta_prompt() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let skill_root = temp_dir.path().join("skills");
    let skill_dir = skill_root.join("demo-skill");
    fs::create_dir_all(&skill_dir)?;
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo workflow\n---\n\n# Demo\n",
    )?;

    let skills = discover_skills(&[skill_root.clone()])?;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "demo-skill");
    let prompt = build_skills_meta_prompt(&skills);
    assert!(prompt.contains("demo-skill"));
    assert!(prompt.contains("skill_load"));
    assert!(!prompt.contains(&skill_dir.display().to_string()));
    Ok(())
}

#[test]
fn builtin_tools_work() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Search answer"
            }
        }],
        "citations": [
            "https://example.com/a",
            "https://example.com/b"
        ]
    }));
    let mut upstream = test_upstream(&server.address);
    upstream.supports_vision_input = true;
    upstream.native_image_input = true;
    upstream.external_web_search = Some(ExternalWebSearchConfig {
        base_url: server.address.clone(),
        model: "perplexity/sonar".to_string(),
        supports_vision_input: true,
        api_key: None,
        api_key_env: "TEST_API_KEY".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        timeout_seconds: 10.0,
        headers: serde_json::Map::new(),
    });
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &upstream,
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let write_result = execute_tool_call(
        &registry,
        "file_write",
        Some(r#"{"file_path":"note.txt","content":"alpha\nbeta\n"}"#),
    );
    assert!(write_result.contains("note.txt"));

    let read_result = execute_tool_call(
        &registry,
        "file_read",
        Some(r#"{"file_path":"note.txt","offset":1,"limit":10}"#),
    );
    assert!(read_result.contains("1: alpha"));
    assert!(read_result.contains("2: beta"));

    let edit_result = execute_tool_call(
        &registry,
        "edit",
        Some(r#"{"path":"note.txt","old_text":"alpha","new_text":"gamma"}"#),
    );
    assert!(edit_result.contains("\"replacements\": 1"));

    let read_after_edit = execute_tool_call(
        &registry,
        "file_read",
        Some(r#"{"file_path":"note.txt","offset":1,"limit":10}"#),
    );
    assert!(read_after_edit.contains("1: gamma"));

    let shell_result = execute_tool_call(&registry, "shell", Some(r#"{"command":"printf 123"}"#));
    let shell_json: Value = serde_json::from_str(&shell_result)?;
    assert!(shell_json.get("session_id").is_some());
    assert!(shell_json.get("process_id").is_some());
    assert_eq!(shell_json["running"], json!(false));
    assert_eq!(shell_json["interactive"], json!(false));
    assert_eq!(shell_json["stdout"], json!("123"));
    assert_eq!(shell_json["stderr"], json!(""));
    assert_eq!(shell_json["exit_code"], json!(0));
    let out_path = temp_dir
        .path()
        .join(shell_json["out_path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(out_path.join("stdout"))?, "123");

    let duplicate_edit = execute_tool_call(
        &registry,
        "edit",
        Some(r#"{"path":"note.txt","old_text":"a","new_text":"z"}"#),
    );
    assert!(duplicate_edit.contains("matched"));
    assert!(duplicate_edit.contains("include more surrounding context"));

    let timeout_process = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 1; printf late","wait_ms":0}"#),
    );
    let timeout_json: Value = serde_json::from_str(&timeout_process)?;
    assert_eq!(timeout_json["running"], json!(true));
    let timeout_session_id = timeout_json["session_id"].as_str().unwrap();
    let timeout_kill = execute_tool_call(
        &registry,
        "shell_close",
        Some(&format!(r#"{{"session_id":"{}"}}"#, timeout_session_id)),
    );
    assert!(timeout_kill.contains("\"closed\": true"));

    let large_output = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"python3 -c \"print('a'*700 + 'b'*700, end='')\""}"#),
    );
    let large_output_json: Value = serde_json::from_str(&large_output)?;
    assert_eq!(large_output_json["running"], json!(false), "{large_output}");
    assert_eq!(large_output_json["stdout_truncated"], json!(true));
    assert!(
        large_output_json["stdout"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .count()
            <= 1000
    );
    let stdout_path = temp_dir
        .path()
        .join(large_output_json["out_path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(stdout_path.join("stdout"))?
            .chars()
            .count(),
        1400
    );

    let background_process = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 0.2; printf bg","wait_ms":0}"#),
    );
    let background_json: Value = serde_json::from_str(&background_process)?;
    let background_session_id = background_json["session_id"].as_str().unwrap();
    let process_result = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","wait_ms":1000}}"#,
            background_session_id
        )),
    );
    assert!(process_result.contains("\"process_id\""));
    assert!(process_result.contains("\"stdout\": \"bg\""));

    let stdin_process = execute_tool_call(
        &registry,
        "shell",
        Some(
            r#"{"command":"read line; printf 'got:%s\n' \"$line\"","interactive":true,"wait_ms":0}"#,
        ),
    );
    let stdin_json: Value = serde_json::from_str(&stdin_process)?;
    let stdin_wait = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","input":"hello\n","wait_ms":5000}}"#,
            stdin_json["session_id"].as_str().unwrap()
        )),
    );
    assert!(stdin_wait.contains("got:hello"));

    let patch_path = temp_dir.path().join("patch.txt");
    fs::write(&patch_path, "before\n")?;
    let patch = "\
--- a/patch.txt\n\
+++ b/patch.txt\n\
@@ -1 +1 @@\n\
-before\n\
+after\n";
    let patch_result = execute_tool_call(
        &registry,
        "apply_patch",
        Some(&format!(
            r#"{{"patch":{},"strip":1}}"#,
            serde_json::to_string(patch)?
        )),
    );
    assert!(patch_result.contains("\"applied\": true"));
    assert_eq!(fs::read_to_string(&patch_path)?, "after\n");

    let fetch_result = execute_tool_call(
        &registry,
        "web_fetch",
        Some(&format!(
            r#"{{"url":"{}/ping","timeout_seconds":2}}"#,
            server.address
        )),
    );
    assert!(fetch_result.contains("\"status\": 200"), "{fetch_result}");
    assert!(fetch_result.contains("/ping"), "{fetch_result}");

    let download_start = execute_tool_call(
        &registry,
        "file_download_start",
        Some(&format!(
            r#"{{"url":"{}/binary","path":"downloads/cat.png"}}"#,
            server.address
        )),
    );
    let download_start_json: Value = serde_json::from_str(&download_start)?;
    let download_result = execute_tool_call(
        &registry,
        "file_download_wait",
        Some(&format!(
            r#"{{"download_id":"{}"}}"#,
            download_start_json["download_id"].as_str().unwrap()
        )),
    );
    assert!(download_result.contains("\"content_type\": \"image/png\""));
    let downloaded = temp_dir.path().join("downloads/cat.png");
    assert!(downloaded.exists());
    assert!(fs::read(&downloaded)?.starts_with(b"\x89PNG"));

    let search_result = execute_tool_call(
        &registry,
        "web_search",
        Some(r#"{"query":"demo query","timeout_seconds":2,"max_results":2}"#),
    );
    assert!(search_result.contains("Search answer"));
    assert!(search_result.contains("example.com/a"));

    let image_path = temp_dir.path().join("diagram.png");
    fs::copy(&downloaded, &image_path)?;
    let image_load = execute_tool_call(&registry, "image_load", Some(r#"{"path":"diagram.png"}"#));
    assert!(
        image_load.contains("\"kind\": \"synthetic_user_multimodal\""),
        "{image_load}"
    );
    assert!(image_load.contains("\"path\":"));

    Ok(())
}

#[test]
fn shell_sessions_report_clear_error_after_runtime_shutdown() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let started = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 10","wait_ms":0}"#),
    );
    let started_json: Value = serde_json::from_str(&started)?;
    let session_id = started_json["session_id"].as_str().unwrap();

    terminate_all_managed_processes()?;

    let observe = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(r#"{{"session_id":"{}","wait_ms":0}}"#, session_id)),
    );
    assert!(observe.contains("no longer exists"));
    Ok(())
}

#[test]
fn shell_accepts_input_from_a_fresh_registry_instance() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let starter = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let started = execute_tool_call(
        &starter,
        "shell",
        Some(
            r#"{"command":"read line; printf 'got:%s\n' \"$line\"","interactive":true,"wait_ms":0}"#,
        ),
    );
    let started_json: Value = serde_json::from_str(&started)?;
    let session_id = started_json["session_id"].as_str().unwrap();

    let waiter = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;
    let waited = execute_tool_call(
        &waiter,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","input":"hello\n","wait_ms":5000}}"#,
            session_id
        )),
    );
    let waited_json: Value = serde_json::from_str(&waited)?;
    assert_eq!(waited_json["running"], json!(false));
    assert_eq!(waited_json["exit_code"], json!(0));
    assert!(
        waited_json["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("got:hello")
    );
    Ok(())
}

#[test]
fn shell_discards_unreturned_finished_result_when_starting_next_command() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let started = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 0.2; printf first","wait_ms":0}"#),
    );
    let started_json: Value = serde_json::from_str(&started)?;
    let session_id = started_json["session_id"].as_str().unwrap();

    thread::sleep(Duration::from_millis(350));

    let next = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","command":"printf second"}}"#,
            session_id
        )),
    );
    let next_json: Value = serde_json::from_str(&next)?;
    assert_eq!(next_json["running"], json!(false));
    assert_eq!(next_json["stdout"], json!("second"));
    assert_eq!(next_json["exit_code"], json!(0));

    let observe = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(r#"{{"session_id":"{}","wait_ms":0}}"#, session_id)),
    );
    let observe_json: Value = serde_json::from_str(&observe)?;
    assert_eq!(observe_json["running"], json!(false));
    assert!(observe_json.get("process_id").is_none(), "{observe_json}");
    assert!(observe_json.get("stdout").is_none(), "{observe_json}");

    Ok(())
}

#[test]
fn shell_treats_empty_command_as_observe() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let started = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 0.2; printf empty-ok","wait_ms":0}"#),
    );
    let started_json: Value = serde_json::from_str(&started)?;
    let session_id = started_json["session_id"].as_str().unwrap();

    let observed = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","command":"","wait_ms":1000}}"#,
            session_id
        )),
    );
    let observed_json: Value = serde_json::from_str(&observed)?;
    assert_eq!(observed_json["running"], json!(false));
    assert_eq!(observed_json["stdout"], json!("empty-ok"));
    assert_eq!(observed_json["exit_code"], json!(0));

    Ok(())
}

#[test]
fn shell_rejects_unknown_session_id_even_with_command() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let result = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","command":"printf custom-ok"}}"#,
            "custom-shell-001"
        )),
    );
    assert!(result.contains("no longer exists"), "{result}");
    Ok(())
}

#[test]
fn shell_rejects_invalid_session_id() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let result = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"session_id":"../bad","command":"printf nope"}"#),
    );
    assert!(
        result.contains("ASCII letters, digits, '_' and '-'"),
        "{result}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn shell_supports_per_tool_remote_ssh() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let fake_ssh = write_fake_ssh(&temp_dir);
    let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
    let workspace_root = temp_dir.path().join("workspace");
    let runtime_state_root = temp_dir.path().join("runtime");
    fs::create_dir_all(&workspace_root)?;
    fs::create_dir_all(&runtime_state_root)?;

    let registry = build_tool_registry(
        &workspace_root,
        &runtime_state_root,
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;

    let completed = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"printf remote-ok","remote":"fake-host"}"#),
    );
    let completed_json: Value = serde_json::from_str(&completed)?;
    assert_eq!(completed_json["running"], json!(false));
    assert_eq!(completed_json["stdout"], json!("remote-ok"));

    let started = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 0.2; printf remote-bg","remote":"fake-host","wait_ms":0}"#),
    );
    let started_json: Value = serde_json::from_str(&started)?;
    let session_id = started_json["session_id"].as_str().unwrap();

    let waited = execute_tool_call(
        &registry,
        "shell",
        Some(&format!(
            r#"{{"session_id":"{}","wait_ms":2000}}"#,
            session_id
        )),
    );
    let waited_json: Value = serde_json::from_str(&waited)?;
    assert_eq!(waited_json["running"], json!(false));
    assert_eq!(waited_json["stdout"], json!("remote-bg"));
    Ok(())
}

#[test]
fn load_skill_tool_hides_paths_but_can_read_skill_content() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let skill_root = temp_dir.path().join("skills");
    let skill_dir = skill_root.join("demo-skill");
    fs::create_dir_all(&skill_dir)?;
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo workflow\n---\n\n# Demo\nUse this skill carefully.\n",
    )?;

    let skills = discover_skills(&[skill_root.clone()])?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[skill_root.clone()],
        &skills,
        &[],
        &[],
        true,
    )?;
    let result = execute_tool_call(
        &registry,
        "skill_load",
        Some(r#"{"skill_name":"demo-skill"}"#),
    );

    assert!(result.contains("\"name\": \"demo-skill\""));
    assert!(result.contains("Use this skill carefully."));
    assert!(!result.contains(&skill_dir.display().to_string()));
    Ok(())
}

#[test]
fn skill_create_persists_staged_skill_directory() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let skill_root = temp_dir.path().join("runtime-skills");
    let staged_dir = temp_dir.path().join(".skills").join("demo-skill");
    fs::create_dir_all(staged_dir.join("references"))?;
    fs::write(
        staged_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo workflow\n---\n\n# Demo\nUse this skill carefully.\n",
    )?;
    fs::write(staged_dir.join("references").join("note.txt"), "hello")?;

    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[skill_root.clone()],
        &[],
        &[],
        &[],
        true,
    )?;
    let result = execute_tool_call(
        &registry,
        "skill_create",
        Some(r#"{"skill_name":"demo-skill"}"#),
    );

    assert!(result.contains("\"created\": true"));
    assert!(skill_root.join("demo-skill").join("SKILL.md").exists());
    assert!(
        skill_root
            .join("demo-skill")
            .join("references")
            .join("note.txt")
            .exists()
    );
    Ok(())
}

#[test]
fn skill_update_validates_and_replaces_existing_skill_directory() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let skill_root = temp_dir.path().join("runtime-skills");
    let existing_dir = skill_root.join("demo-skill");
    fs::create_dir_all(&existing_dir)?;
    fs::write(
        existing_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Old workflow\n---\n\n# Old\n",
    )?;

    let staged_dir = temp_dir.path().join(".skills").join("demo-skill");
    fs::create_dir_all(&staged_dir)?;
    fs::write(
        staged_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: New workflow\n---\n\n# New\nUpdated instructions.\n",
    )?;

    let skills = discover_skills(&[skill_root.clone()])?;
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[skill_root.clone()],
        &skills,
        &[],
        &[],
        true,
    )?;
    let result = execute_tool_call(
        &registry,
        "skill_update",
        Some(r#"{"skill_name":"demo-skill"}"#),
    );

    assert!(result.contains("\"updated\": true"));
    let persisted = fs::read_to_string(skill_root.join("demo-skill").join("SKILL.md"))?;
    assert!(persisted.contains("New workflow"));
    assert!(persisted.contains("Updated instructions."));
    Ok(())
}

#[test]
fn native_web_search_disables_external_web_search_tool() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let mut upstream = test_upstream("http://127.0.0.1:1");
    upstream.native_web_search = Some(NativeWebSearchConfig {
        enabled: true,
        payload: serde_json::Map::new(),
    });
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &upstream,
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;
    assert!(!registry.contains_key("web_search"));
    assert!(registry.contains_key("web_fetch"));
    Ok(())
}

#[test]
fn run_session_registers_load_skill_without_exposing_skill_paths() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let skill_root = temp_dir.path().join("skills");
    let skill_dir = skill_root.join("demo-skill");
    fs::create_dir_all(&skill_dir)?;
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo workflow\n---\n\n# Demo\nUse this skill carefully.\n",
    )?;

    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "ok"
            }
        }]
    }));

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "skills_dirs": [skill_root]
        }),
        ".",
    )?;

    let messages = run_session(Vec::new(), "hello", config, Vec::new())?;
    assert_eq!(extract_assistant_text(&messages), "ok");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let system_prompt = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system_prompt.contains("demo-skill"));
    assert!(system_prompt.contains("skill_load"));
    assert!(!system_prompt.contains(&skill_dir.display().to_string()));
    let tool_names = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"skill_load".to_string()));
    Ok(())
}

#[test]
fn load_config_prefers_reasoning_object_over_reasoning_effort_shorthand() -> Result<()> {
    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": "https://openrouter.ai/api/v1",
                "model": "openai/gpt-5-mini",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h"
                },
                "reasoning": {
                    "effort": "medium",
                    "max_tokens": 2048,
                    "exclude": true
                },
                "reasoning_effort": "high"
            }
        }),
        ".",
    )?;

    assert_eq!(
        config.upstream.cache_control.as_ref().unwrap().cache_type,
        "ephemeral"
    );
    assert_eq!(
        config
            .upstream
            .cache_control
            .as_ref()
            .unwrap()
            .ttl
            .as_deref(),
        Some("1h")
    );
    let reasoning = config.upstream.reasoning.as_ref().unwrap();
    assert_eq!(reasoning.effort.as_deref(), Some("medium"));
    assert_eq!(reasoning.max_tokens, Some(2048));
    assert_eq!(reasoning.exclude, Some(true));
    Ok(())
}

#[test]
fn run_session_with_extra_tool() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "multiply",
                        "arguments": "{\"a\":6,\"b\":7}"
                    }
                }]
            }
        }]
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "42"
            }
        }]
    }));

    let multiply = tool! {
        description: "Multiply two integers.",
        fn multiply(a: i64, b: i64) -> i64 {
            a * b
        }
    };

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let messages = run_session_state_until_end(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        None,
    )?
    .messages;
    assert_eq!(extract_assistant_text(&messages), "42");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["messages"][0]["role"], "system");
    assert!(
        requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "multiply")
    );
    Ok(())
}

#[test]
fn tool_macro_supports_name_override_and_optional_args() -> Result<()> {
    let add = tool! {
        name: "add",
        description: "Add two integers with an optional increment.",
        fn add_numbers(a: i64, b: i64, increment: Option<i64>) -> i64 {
            a + b + increment.unwrap_or(0)
        }
    };

    assert_eq!(add.name, "add");
    assert_eq!(add.parameters["properties"]["a"]["type"], "integer");
    assert_eq!(add.parameters["properties"]["b"]["type"], "integer");
    assert_eq!(add.parameters["properties"]["increment"]["type"], "integer");
    assert_eq!(add.parameters["required"], json!(["a", "b"]));

    let result = add.invoke(json!({"a": 1, "b": 2, "increment": 3}))?;
    assert_eq!(result, json!(6));

    let result = add.invoke(json!({"a": 1, "b": 2}))?;
    assert_eq!(result, json!(3));
    Ok(())
}

#[test]
fn run_session_auto_compacts_before_next_turn() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": structured_compaction_response_text()
            }
        }]
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": structured_compaction_response_text()
            }
        }]
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "compacted turn completed"
            }
        }]
    }));

    let previous_messages = vec![
        ChatMessage::text("user", long_distinct_text("auto-user-a", 900)),
        ChatMessage::text("assistant", long_distinct_text("auto-assistant-b", 900)),
        ChatMessage::text("user", long_distinct_text("auto-user-c", 900)),
        ChatMessage::text("assistant", long_distinct_text("auto-assistant-d", 900)),
        ChatMessage::text("user", long_distinct_text("auto-user-e", 900)),
        ChatMessage::text("assistant", long_distinct_text("auto-assistant-f", 900)),
    ];

    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "fake-model",
                "context_window_tokens": 3000
            },
            "system_prompt": "Test system prompt.",
            "context_compaction": {
                "trigger_ratio": 0.9,
                "token_limit_override": 2000
            }
        }),
        ".",
    )?;

    let messages = run_session(previous_messages, "next prompt", config, Vec::new())?;
    assert_eq!(
        extract_assistant_text(&messages),
        "compacted turn completed"
    );
    let requests = server.requests();
    assert!(requests.len() >= 2);
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .any(|content| content.contains("Compress the older conversation history"))
    );
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .any(|content| content.contains("if an older start-type task is still active"))
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .flat_map(|request| request["messages"].as_array().into_iter().flatten())
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .any(|content| content.contains(COMPACTION_MARKER))
    );
    Ok(())
}

#[test]
fn pending_prefix_rewrite_is_applied_before_next_model_call() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "rewrite_prefix",
                        "arguments": "{}"
                    }
                }]
            }
        }]
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "done"
            }
        }]
    }));

    let control = SessionExecutionControl::new();
    let rewrite_control = control.clone();
    let rewrite_tool = agent_frame::Tool::new(
        "rewrite_prefix",
        "Rewrite the stable prefix for the current run.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        move |_| {
            let expected_prefix = rewrite_control.stable_prefix_snapshot();
            let replacement_prefix = vec![
                expected_prefix
                    .first()
                    .cloned()
                    .expect("system message should exist"),
                ChatMessage::text(
                    "assistant",
                    format!("{}\n\ncompressed prefix", COMPACTION_MARKER),
                ),
            ];
            rewrite_control.request_prefix_rewrite(
                expected_prefix,
                replacement_prefix,
                agent_frame::TokenUsage {
                    llm_calls: 1,
                    prompt_tokens: 120,
                    completion_tokens: 8,
                    total_tokens: 128,
                    cache_hit_tokens: 40,
                    cache_miss_tokens: 80,
                    cache_read_tokens: 40,
                    cache_write_tokens: 60,
                },
                agent_frame::SessionCompactionStats::default(),
            );
            Ok(json!({"ok": true}))
        },
    );

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let report = run_session_state_until_end(
        vec![
            ChatMessage::text("user", "old question"),
            ChatMessage::text("assistant", "old answer"),
        ],
        "new question",
        config,
        vec![rewrite_tool],
        Some(control),
    )?;

    assert_eq!(extract_assistant_text(&report.messages), "done");
    assert_eq!(report.usage.llm_calls, 3);
    assert_eq!(report.usage.cache_write_tokens, 60);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second_messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(second_messages[1]["role"], "assistant");
    assert!(
        second_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains(COMPACTION_MARKER)
    );
    assert_eq!(second_messages[2]["role"], "tool");
    Ok(())
}

#[test]
fn compact_session_messages_with_report_compacts_and_reports_usage() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": structured_compaction_response_text()
            }
        }],
        "usage": {
            "prompt_tokens": 1200,
            "completion_tokens": 80,
            "total_tokens": 1280,
            "prompt_tokens_details": {
                "cached_tokens": 400,
                "cache_write_tokens": 200
            }
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": structured_compaction_response_text()
            }
        }],
        "usage": {
            "prompt_tokens": 1200,
            "completion_tokens": 80,
            "total_tokens": 1280,
            "prompt_tokens_details": {
                "cached_tokens": 400,
                "cache_write_tokens": 200
            }
        }
    }));

    let previous_messages = vec![
        ChatMessage::text("system", "Test system prompt."),
        ChatMessage::text("user", long_distinct_text("report-user-a", 900)),
        ChatMessage::text("assistant", long_distinct_text("report-assistant-b", 900)),
        ChatMessage::text("user", long_distinct_text("report-user-c", 900)),
        ChatMessage::text("assistant", long_distinct_text("report-assistant-d", 900)),
    ];

    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "fake-model",
                "context_window_tokens": 2500
            },
            "system_prompt": "Test system prompt.",
            "context_compaction": {
                "trigger_ratio": 0.9,
                "token_limit_override": 2000
            }
        }),
        ".",
    )?;

    let report = compact_session_messages_with_report(previous_messages, config, Vec::new())?;
    assert!(report.compacted);
    assert!(report.usage.llm_calls >= 1);
    assert!(report.usage.prompt_tokens >= 1200);
    assert!(report.usage.cache_read_tokens >= 400);
    assert!(report.usage.cache_write_tokens >= 200);
    assert!(report.estimated_tokens_before > report.token_limit);
    assert!(report.estimated_tokens_after < report.estimated_tokens_before);
    assert!(report.messages.iter().any(|message| {
        extract_assistant_text(std::slice::from_ref(message)).contains(COMPACTION_MARKER)
    }));
    Ok(())
}

#[test]
fn compact_session_messages_with_report_skips_when_below_threshold() -> Result<()> {
    let previous_messages = vec![
        ChatMessage::text("system", "Test system prompt."),
        ChatMessage::text("user", "short prompt"),
        ChatMessage::text("assistant", "short reply"),
    ];

    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": "http://127.0.0.1:1",
                "model": "fake-model",
                "context_window_tokens": 128000
            },
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let report =
        compact_session_messages_with_report(previous_messages.clone(), config, Vec::new())?;
    assert!(!report.compacted);
    assert_eq!(report.usage.llm_calls, 0);
    assert_eq!(
        report.estimated_tokens_before,
        report.estimated_tokens_after
    );
    assert_eq!(report.messages[0].role, "system");
    assert!(
        report.messages[0]
            .content
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("[AgentFrame Runtime]"))
    );
    assert_eq!(&report.messages[1..], &previous_messages[..]);
    Ok(())
}

#[test]
fn upstream_cache_control_and_reasoning_are_forwarded() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "ok"
            }
        }]
    }));

    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "anthropic/claude-sonnet-4.6",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h"
                },
                "reasoning_effort": "high"
            }
        }),
        ".",
    )?;

    let messages = run_session(Vec::new(), "hello", config, Vec::new())?;
    assert_eq!(extract_assistant_text(&messages), "ok");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(requests[0]["cache_control"]["ttl"], "1h");
    assert_eq!(requests[0]["reasoning"]["effort"], "high");
    Ok(())
}

#[test]
fn responses_upstream_cache_fields_are_forwarded() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "id": "resp_1",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "ok"
            }]
        }],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 2,
            "total_tokens": 12,
            "input_tokens_details": {
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 3
            }
        }
    }));

    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "anthropic/claude-opus-4.6",
                "api_kind": "responses",
                "chat_completions_path": "/responses",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h"
                },
                "prompt_cache_key": "conversation-key",
                "prompt_cache_retention": "1h"
            }
        }),
        ".",
    )?;

    let messages = run_session(Vec::new(), "hello", config, Vec::new())?;
    assert_eq!(extract_assistant_text(&messages), "ok");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(requests[0]["cache_control"]["ttl"], "1h");
    assert_eq!(requests[0]["prompt_cache_key"], "conversation-key");
    assert_eq!(requests[0]["prompt_cache_retention"], "1h");
    Ok(())
}

#[test]
fn claude_messages_provider_roundtrips_tools_and_block_cache_control() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "text", "text": "checking" },
            {
                "type": "tool_use",
                "id": "call_1",
                "name": "multiply",
                "input": { "a": 6, "b": 7 }
            }
        ],
        "usage": {
            "input_tokens": 5000,
            "cache_creation_input_tokens": 4990,
            "cache_read_input_tokens": 0,
            "output_tokens": 5
        }
    }));
    server.push_response(json!({
        "type": "message",
        "role": "assistant",
        "content": [
            { "type": "text", "text": "42" }
        ],
        "usage": {
            "input_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 4990,
            "output_tokens": 3
        }
    }));

    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "claude-opus-4-6",
                "api_kind": "claude_messages",
                "chat_completions_path": "/messages",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "5m"
                }
            },
            "system_prompt": "Stable cacheable prefix."
        }),
        ".",
    )?;

    let multiply = Tool::new(
        "multiply",
        "Multiply two numbers.",
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "integer" }
            },
            "required": ["a", "b"]
        }),
        |args| {
            let a = args["a"].as_i64().unwrap_or_default();
            let b = args["b"].as_i64().unwrap_or_default();
            Ok(json!({ "product": a * b }))
        },
    );

    let report = run_session_state_until_end(Vec::new(), "hello", config, vec![multiply], None)?;
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "requests={requests:?} messages={:?}",
        report.messages
    );
    assert!(
        requests[0]["system"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Stable cacheable prefix."))
    );
    assert_eq!(requests[0]["messages"][0]["role"], "user");
    assert_eq!(
        requests[0]["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(requests[1]["messages"][1]["role"], "assistant");
    assert_eq!(requests[1]["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(requests[1]["messages"][2]["role"], "user");
    assert_eq!(
        requests[1]["messages"][2]["content"][0]["type"],
        "tool_result"
    );
    assert_eq!(
        extract_assistant_text(&report.messages),
        "42",
        "requests={requests:?} messages={:?}",
        report.messages
    );
    Ok(())
}

#[test]
fn run_session_state_aggregates_usage_across_tool_roundtrips() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "multiply",
                        "arguments": "{\"a\":6,\"b\":7}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 3,
            "total_tokens": 14,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 2
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "42"
            }
        }],
        "usage": {
            "prompt_tokens": 13,
            "completion_tokens": 4,
            "total_tokens": 17,
            "prompt_tokens_details": {
                "cached_tokens": 7
            }
        }
    }));

    let multiply = tool! {
        description: "Multiply two integers.",
        fn multiply(a: i64, b: i64) -> i64 {
            a * b
        }
    };
    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "demo-model"
            },
            "workspace_root": temp_dir.path()
        }),
        temp_dir.path(),
    )?;

    let report = run_session_state_until_end(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        None,
    )?;
    assert_eq!(extract_assistant_text(&report.messages), "42");
    assert_eq!(report.usage.llm_calls, 2);
    assert_eq!(report.usage.prompt_tokens, 24);
    assert_eq!(report.usage.completion_tokens, 7);
    assert_eq!(report.usage.total_tokens, 31);
    assert_eq!(report.usage.cache_read_tokens, 12);
    assert_eq!(report.usage.cache_hit_tokens, 12);
    assert_eq!(report.usage.cache_write_tokens, 2);
    assert_eq!(report.usage.cache_miss_tokens, 12);
    Ok(())
}

#[test]
fn empty_final_assistant_response_yields_api_failure_state() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant"
            }
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "total_tokens": 12
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant"
            }
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "total_tokens": 12
        }
    }));
    let temp_dir = TempDir::new()?;
    let config = load_config_value(
        json!({
            "upstream": {
                "base_url": server.address,
                "model": "demo-model"
            },
            "workspace_root": temp_dir.path()
        }),
        temp_dir.path(),
    )?;

    let state = run_session_state(Vec::new(), "Look again", config, Vec::new())?;

    assert_eq!(state.phase, agent_frame::SessionPhase::Yielded);
    assert_eq!(state.errno, Some(agent_frame::SessionErrno::ApiFailure));
    assert!(
        state
            .errinfo
            .as_deref()
            .is_some_and(|text| text.contains("empty final assistant"))
    );
    assert_eq!(
        state.messages.last().map(|message| message.role.as_str()),
        Some("user")
    );
    assert_eq!(state.usage.completion_tokens, 4);
    Ok(())
}

#[test]
fn controlled_run_emits_checkpoint_and_honors_cancellation() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "draft answer",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "multiply",
                        "arguments": "{\"a\":6,\"b\":7}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 5,
            "total_tokens": 26
        }
    }));

    let multiply = tool! {
        description: "Multiply two integers.",
        fn multiply(a: i64, b: i64) -> i64 {
            a * b
        }
    };

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let control_holder = Arc::new(Mutex::new(None::<SessionExecutionControl>));
    let control = {
        let control_holder = Arc::clone(&control_holder);
        SessionExecutionControl::new().with_event_callback(move |event| {
            if matches!(event, SessionEvent::ToolCallStarted { .. })
                && let Some(control) = control_holder.lock().unwrap().as_ref()
            {
                control.request_cancel();
            }
        })
    };
    *control_holder.lock().unwrap() = Some(control.clone());

    let report = run_session_state_controlled(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        Some(control),
    )?;
    assert_eq!(report.phase, agent_frame::SessionPhase::Yielded);
    assert_eq!(
        report
            .messages
            .iter()
            .filter(|message| message.role == "tool")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn controlled_run_emits_process_events() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "draft answer",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "multiply",
                        "arguments": "{\"a\":6,\"b\":7}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 5,
            "total_tokens": 26
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "final answer"
            }
        }],
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 7,
            "total_tokens": 37
        }
    }));

    let multiply = tool! {
        description: "Multiply two integers.",
        fn multiply(a: i64, b: i64) -> i64 {
            a * b
        }
    };

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let events = Arc::new(Mutex::new(Vec::new()));
    let control = {
        let events = Arc::clone(&events);
        SessionExecutionControl::new().with_event_callback(move |event| {
            events.lock().unwrap().push(event);
        })
    };

    let report = run_session_state_until_end(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        Some(control),
    )?;
    assert_eq!(extract_assistant_text(&report.messages), "final answer");
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::ModelCallStarted { .. }))
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::ToolCallStarted { tool_name, .. } if tool_name == "multiply")));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::ToolCallCompleted { tool_name, errored, .. } if tool_name == "multiply" && !errored)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::SessionCompleted { .. }))
    );
    Ok(())
}

#[test]
fn controlled_run_does_not_emit_checkpoint_for_assistant_messages_with_tool_calls() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "draft answer",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "multiply",
                        "arguments": "{\"a\":6,\"b\":7}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 5,
            "total_tokens": 26
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "final answer"
            }
        }],
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 7,
            "total_tokens": 37
        }
    }));

    let multiply = tool! {
        description: "Multiply two integers.",
        fn multiply(a: i64, b: i64) -> i64 {
            a * b
        }
    };

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let control = SessionExecutionControl::new();

    let report = run_session_state_until_end(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        Some(control),
    )?;
    assert_eq!(extract_assistant_text(&report.messages), "final answer");
    Ok(())
}

#[test]
fn tool_calls_in_one_round_execute_in_parallel() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "working",
                "tool_calls": [
                    {
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "slow_a",
                            "arguments": "{}"
                        }
                    },
                    {
                        "id": "call-2",
                        "type": "function",
                        "function": {
                            "name": "slow_b",
                            "arguments": "{}"
                        }
                    }
                ]
            }
        }],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 5,
            "total_tokens": 26
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "done"
            }
        }],
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 7,
            "total_tokens": 37
        }
    }));

    let active_tools = Arc::new(AtomicUsize::new(0));
    let max_active_tools = Arc::new(AtomicUsize::new(0));
    let active_tools_a = Arc::clone(&active_tools);
    let max_active_tools_a = Arc::clone(&max_active_tools);
    let active_tools_b = Arc::clone(&active_tools);
    let max_active_tools_b = Arc::clone(&max_active_tools);

    let slow_a = tool! {
        description: "Sleep briefly and return A.",
        fn slow_a() -> String {
            let active = active_tools_a.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_tools_a.fetch_max(active, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(250));
            active_tools_a.fetch_sub(1, Ordering::SeqCst);
            "A".to_string()
        }
    };
    let slow_b = tool! {
        description: "Sleep briefly and return B.",
        fn slow_b() -> String {
            let active = active_tools_b.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_tools_b.fetch_max(active, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(250));
            active_tools_b.fetch_sub(1, Ordering::SeqCst);
            "B".to_string()
        }
    };

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let report = run_session_state_until_end(
        Vec::new(),
        "Run both tools.",
        config,
        vec![slow_a, slow_b],
        None,
    )?;
    assert_eq!(extract_assistant_text(&report.messages), "done");
    assert_eq!(max_active_tools.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn controlled_run_emits_execution_progress_snapshots() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "working",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "multiply",
                        "arguments": "{\"a\":6,\"b\":7}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 5,
            "total_tokens": 26
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "done"
            }
        }],
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 7,
            "total_tokens": 37
        }
    }));

    let multiply = tool! {
        description: "Multiply two integers.",
        fn multiply(a: i64, b: i64) -> i64 {
            a * b
        }
    };
    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;
    let progress = Arc::new(Mutex::new(Vec::new()));
    let control = SessionExecutionControl::new().with_progress_callback({
        let progress = Arc::clone(&progress);
        move |snapshot| progress.lock().unwrap().push(snapshot)
    });

    let report = run_session_state_until_end(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        Some(control),
    )?;

    assert_eq!(extract_assistant_text(&report.messages), "done");
    let progress = progress.lock().unwrap();
    assert!(
        progress
            .iter()
            .any(|item| item.phase == ExecutionProgressPhase::Thinking)
    );
    assert!(progress.iter().any(|item| {
        item.phase == ExecutionProgressPhase::Tools
            && item
                .tools
                .iter()
                .any(|tool| tool.status == ToolExecutionStatus::Running)
    }));
    assert_eq!(
        progress
            .iter()
            .filter(|item| item.phase == ExecutionProgressPhase::Tools)
            .count(),
        1
    );
    Ok(())
}

#[test]
fn controlled_run_converts_tool_phase_timeout_into_observation_and_continues() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let server = TestServer::start();
    let registry = build_tool_registry(
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream(&server.address),
        &BTreeMap::new(),
        None,
        None,
        None,
        None,
        &[],
        &[],
        &[],
        &[],
        true,
    )?;
    let shell_start = execute_tool_call(
        &registry,
        "shell",
        Some(r#"{"command":"sleep 10","wait_ms":0}"#),
    );
    let shell_start_json: Value = serde_json::from_str(&shell_start)?;
    let session_id = shell_start_json["session_id"]
        .as_str()
        .expect("shell should return session_id")
        .to_string();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "checking environment",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": serde_json::to_string(&json!({
                            "session_id": session_id,
                            "wait_ms": 30000
                        })).unwrap()
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 5,
            "total_tokens": 26
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "the tool timed out; please retry with a longer timeout if needed"
            }
        }],
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 9,
            "total_tokens": 39
        }
    }));

    let config = load_config_value(
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt.",
            "timeout_observation_compaction": {"enabled": true},
            "workspace_root": temp_dir.path()
        }),
        temp_dir.path(),
    )?;

    let control_holder = Arc::new(Mutex::new(None::<SessionExecutionControl>));
    let control = {
        let control_holder = Arc::clone(&control_holder);
        SessionExecutionControl::new().with_event_callback(move |event| {
            if matches!(event, SessionEvent::ToolCallStarted { .. })
                && let Some(control) = control_holder.lock().unwrap().as_ref()
            {
                let control = control.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(100));
                    control.request_timeout_observation();
                });
            }
        })
    };
    *control_holder.lock().unwrap() = Some(control.clone());

    let report = run_session_state_until_end(
        Vec::new(),
        "Check the environment",
        config,
        Vec::new(),
        Some(control),
    )?;
    let _ = execute_tool_call(
        &registry,
        "shell_close",
        Some(&format!(r#"{{"session_id":"{}"}}"#, session_id)),
    );
    let assistant_text = extract_assistant_text(&report.messages);
    assert!(assistant_text.contains("tool timed out"));
    let tool_messages = report
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 1);
    let tool_content = tool_messages[0]
        .content
        .as_ref()
        .and_then(|value| value.as_str())
        .unwrap();
    let tool_json: Value = serde_json::from_str(tool_content)?;
    assert_eq!(tool_json["timed_out"], json!(true));
    assert_eq!(tool_json["tool"], json!("shell"));
    Ok(())
}

#[test]
fn controlled_run_starts_tools_and_yields_after_tool_batch() -> Result<()> {
    let temp_dir = TempDir::new()?;
    fs::write(
        temp_dir.path().join("README.md"),
        "# Test Workspace\nThis file is here for file_read.\n",
    )?;
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "checking environment",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "file_read",
                        "arguments": "{\"file_path\":\"README.md\"}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 18,
            "completion_tokens": 6,
            "total_tokens": 24
        }
    }));
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "unexpected second round"
            }
        }],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3,
            "total_tokens": 8
        }
    }));

    let config = load_config_value(
        json!({
            "enable_context_compression": false,
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt.",
            "workspace_root": temp_dir.path()
        }),
        temp_dir.path(),
    )?;

    let control_holder = Arc::new(Mutex::new(None::<SessionExecutionControl>));
    let events = Arc::new(Mutex::new(Vec::<SessionEvent>::new()));
    let control = {
        let control_holder = Arc::clone(&control_holder);
        let events = Arc::clone(&events);
        SessionExecutionControl::new().with_event_callback(move |event| {
            events.lock().unwrap().push(event.clone());
            if matches!(event, SessionEvent::ModelCallCompleted { .. })
                && let Some(control) = control_holder.lock().unwrap().as_ref()
            {
                control.request_yield();
            }
        })
    };
    *control_holder.lock().unwrap() = Some(control.clone());

    let report = run_session_state_controlled(
        Vec::new(),
        "Check the environment",
        config,
        Vec::new(),
        Some(control),
    )?;
    assert!(report.phase == agent_frame::SessionPhase::Yielded);
    let tool_messages = report
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 1);
    let tool_json: Value = serde_json::from_str(
        tool_messages[0]
            .content
            .as_ref()
            .and_then(|value| value.as_str())
            .unwrap(),
    )?;
    assert!(
        tool_json["file_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("README.md"))
    );
    assert!(tool_json["content"].as_str().is_some());
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCallStarted { tool_name, .. } if tool_name == "file_read"))
    );
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, SessionEvent::SessionYielded { phase, .. } if phase == "after_tool_batch"))
    );
    assert_eq!(server.requests().len(), 1);
    Ok(())
}

#[test]
fn cli_reads_prompt_argument() -> Result<()> {
    let server = TestServer::start();
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "hello from cli"
            }
        }]
    }));

    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("config.json");
    write_config(
        &config_path,
        json!({
            "upstream": {"base_url": server.address, "model": "fake-model"}
        }),
    );

    Command::cargo_bin("run_agent")?
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        .arg("--config")
        .arg(&config_path)
        .arg("hello")
        .assert()
        .success()
        .stdout("hello from cli\n");
    Ok(())
}
