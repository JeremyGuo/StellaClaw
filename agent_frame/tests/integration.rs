use agent_frame::compaction::COMPACTION_MARKER;
use agent_frame::message::ChatMessage;
use agent_frame::skills::{build_skills_meta_prompt, discover_skills};
use agent_frame::tool;
use agent_frame::tooling::{
    build_tool_registry, execute_tool_call, terminate_all_managed_processes,
};
use agent_frame::{
    ExternalWebSearchConfig, NativeWebSearchConfig, SessionEvent, SessionExecutionControl,
    UpstreamConfig, compact_session_messages_with_report, extract_assistant_text,
    load_config_value, run_session, run_session_with_report, run_session_with_report_controlled,
};
use anyhow::Result;
use assert_cmd::Command;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

fn handle_stream(
    mut stream: TcpStream,
    responses: &Arc<Mutex<VecDeque<Value>>>,
    requests: &Arc<Mutex<Vec<Value>>>,
) {
    stream
        .set_nonblocking(false)
        .expect("set blocking mode on accepted stream");
    let mut buffer = vec![0_u8; 64 * 1024];
    let bytes_read = stream.read(&mut buffer).expect("read request");
    let request_text = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
    let header_end = request_text.find("\r\n\r\n").expect("header end");
    let headers = &request_text[..header_end];
    let body = &request_text[header_end + 4..];
    let request_line = headers.lines().next().expect("request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("method");
    let path = parts.next().expect("path");

    if method == "GET" {
        if path == "/binary" {
            let body = b"\x89PNG\r\n\x1a\nbinary-body";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(header.as_bytes())
                .expect("write binary get response header");
            stream
                .write_all(body)
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
        api_key: None,
        api_key_env: "TEST_API_KEY".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        codex_home: None,
        codex_auth: None,
        auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
        timeout_seconds: 10.0,
        context_window_tokens: 128_000,
        cache_control: None,
        reasoning: None,
        headers: serde_json::Map::new(),
        native_web_search: None,
        external_web_search: None,
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
    server.push_response(json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "The image shows a handwritten note."
            }
        }]
    }));
    let mut upstream = test_upstream(&server.address);
    upstream.supports_vision_input = true;
    upstream.external_web_search = Some(ExternalWebSearchConfig {
        base_url: server.address.clone(),
        model: "perplexity/sonar".to_string(),
        api_key: None,
        api_key_env: "TEST_API_KEY".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        timeout_seconds: 10.0,
        headers: serde_json::Map::new(),
    });
    let registry = build_tool_registry(
        &[
            "read_file".to_string(),
            "write_file".to_string(),
            "edit".to_string(),
            "apply_patch".to_string(),
            "exec_start".to_string(),
            "exec_observe".to_string(),
            "exec_wait".to_string(),
            "exec_kill".to_string(),
            "file_download_start".to_string(),
            "file_download_progress".to_string(),
            "file_download_wait".to_string(),
            "file_download_cancel".to_string(),
            "web_fetch".to_string(),
            "web_search".to_string(),
            "image_start".to_string(),
            "image_wait".to_string(),
            "image_cancel".to_string(),
        ],
        temp_dir.path(),
        temp_dir.path(),
        &upstream,
        None,
        &[],
        &[],
        &[],
    )?;

    let write_result = execute_tool_call(
        &registry,
        "write_file",
        Some(r#"{"path":"note.txt","content":"alpha\nbeta\n"}"#),
    );
    assert!(write_result.contains("note.txt"));

    let read_result = execute_tool_call(
        &registry,
        "read_file",
        Some(r#"{"path":"note.txt","offset_lines":0,"limit_lines":10}"#),
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
        "read_file",
        Some(r#"{"path":"note.txt","offset_lines":0,"limit_lines":10}"#),
    );
    assert!(read_after_edit.contains("1: gamma"));

    let shell_result =
        execute_tool_call(&registry, "exec_start", Some(r#"{"command":"printf 123"}"#));
    assert!(shell_result.contains("\"exec_id\""));

    let background_process = execute_tool_call(
        &registry,
        "exec_start",
        Some(r#"{"command":"sleep 0.2; printf bg"}"#),
    );
    let background_json: Value = serde_json::from_str(&background_process)?;
    let background_exec_id = background_json
        .get("exec_id")
        .and_then(Value::as_str)
        .or_else(|| {
            background_json
                .get("process")
                .and_then(Value::as_object)
                .and_then(|process| process.get("exec_id"))
                .and_then(Value::as_str)
        })
        .expect("background exec id");
    let process_result = execute_tool_call(
        &registry,
        "exec_wait",
        Some(&format!(
            r#"{{"exec_id":"{}","wait_timeout_seconds":1,"start":0,"limit":10}}"#,
            background_exec_id
        )),
    );
    assert!(process_result.contains("\"exec_id\""));
    assert!(process_result.contains("\"stdout\": \"bg\""));

    let cat_process = execute_tool_call(
        &registry,
        "exec_start",
        Some(r#"{"command":"cat","include_stdout":false}"#),
    );
    let cat_json: Value = serde_json::from_str(&cat_process)?;
    let cat_wait = execute_tool_call(
        &registry,
        "exec_wait",
        Some(&format!(
            r#"{{"exec_id":"{}","wait_timeout_seconds":0.2,"input":"hello\n","start":0,"limit":10}}"#,
            cat_json["exec_id"].as_str().unwrap()
        )),
    );
    assert!(
        cat_wait.contains("\"wait_timed_out\": true") || cat_wait.contains("\"stdout\": \"hello\"")
    );
    let cat_observe = execute_tool_call(
        &registry,
        "exec_observe",
        Some(&format!(
            r#"{{"exec_id":"{}","start":0,"limit":10}}"#,
            cat_json["exec_id"].as_str().unwrap()
        )),
    );
    assert!(cat_observe.contains("hello"));
    let cat_kill = execute_tool_call(
        &registry,
        "exec_kill",
        Some(&format!(
            r#"{{"exec_id":"{}"}}"#,
            cat_json["exec_id"].as_str().unwrap()
        )),
    );
    assert!(cat_kill.contains("\"killed\": true"));

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
    fs::write(&image_path, [1_u8, 2, 3, 4])?;
    let image_start = execute_tool_call(
        &registry,
        "image_start",
        Some(r#"{"path":"diagram.png","question":"What does this image show?"}"#),
    );
    let image_start_json: Value = serde_json::from_str(&image_start)?;
    let image_result = execute_tool_call(
        &registry,
        "image_wait",
        Some(&format!(
            r#"{{"image_id":"{}"}}"#,
            image_start_json["image_id"].as_str().unwrap()
        )),
    );
    assert!(image_result.contains("handwritten note"));

    let requests = server.requests();
    let image_request = requests.last().expect("image request");
    let messages = image_request["messages"]
        .as_array()
        .expect("messages array");
    let user_content = messages[1]["content"]
        .as_array()
        .expect("multimodal content");
    assert_eq!(user_content[0]["type"], "text");
    assert_eq!(user_content[1]["type"], "image_url");
    assert!(
        user_content[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );

    let timeout_target = temp_dir.path().join("timeout-side-effect.txt");
    let timeout_result = execute_tool_call(
        &registry,
        "exec_start",
        Some(&format!(
            r#"{{"command":"sleep 1; printf late > {}"}}"#,
            timeout_target.display()
        )),
    );
    assert!(timeout_result.contains("\"exec_id\""));
    thread::sleep(std::time::Duration::from_millis(1200));
    assert!(timeout_target.exists());
    Ok(())
}

#[test]
fn exec_processes_report_clear_error_after_runtime_shutdown() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let registry = build_tool_registry(
        &[
            "exec_start".to_string(),
            "exec_observe".to_string(),
            "exec_wait".to_string(),
        ],
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        None,
        &[],
        &[],
        &[],
    )?;

    let started = execute_tool_call(
        &registry,
        "exec_start",
        Some(r#"{"command":"sleep 10","include_stdout":false}"#),
    );
    let started_json: Value = serde_json::from_str(&started)?;
    let exec_id = started_json["exec_id"].as_str().unwrap();

    terminate_all_managed_processes()?;

    let observe = execute_tool_call(
        &registry,
        "exec_observe",
        Some(&format!(
            r#"{{"exec_id":"{}","start":0,"limit":5}}"#,
            exec_id
        )),
    );
    assert!(observe.contains("no longer exists"));

    let wait = execute_tool_call(
        &registry,
        "exec_wait",
        Some(&format!(
            r#"{{"exec_id":"{}","wait_timeout_seconds":0.1}}"#,
            exec_id
        )),
    );
    assert!(wait.contains("no longer exists"));
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
        &[],
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        None,
        &[skill_root.clone()],
        &skills,
        &[],
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
        &[],
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        None,
        &[skill_root.clone()],
        &[],
        &[],
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
        &[],
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        None,
        &[skill_root.clone()],
        &skills,
        &[],
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
        &["web_search".to_string(), "web_fetch".to_string()],
        temp_dir.path(),
        temp_dir.path(),
        &upstream,
        None,
        &[],
        &[],
        &[],
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
            "enabled_tools": [],
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
            "enabled_tools": [],
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
            "enabled_tools": [],
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let messages = run_session(Vec::new(), "What is 6 times 7?", config, vec![multiply])?;
    assert_eq!(extract_assistant_text(&messages), "42");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["messages"][0]["role"], "system");
    assert_eq!(requests[0]["tools"][0]["function"]["name"], "multiply");
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
                "content": "Goals\n- Keep working on the task\n\nConstraints\n- Stay concise"
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
        ChatMessage::text("user", "A".repeat(1800)),
        ChatMessage::text("assistant", "B".repeat(1800)),
        ChatMessage::text("user", "C".repeat(1800)),
        ChatMessage::text("assistant", "D".repeat(1800)),
        ChatMessage::text("user", "E".repeat(1800)),
        ChatMessage::text("assistant", "F".repeat(1800)),
    ];

    let config = load_config_value(
        json!({
            "enabled_tools": [],
            "upstream": {
                "base_url": server.address,
                "model": "fake-model",
                "context_window_tokens": 3000
            },
            "system_prompt": "Test system prompt.",
            "retain_recent_messages": 2
        }),
        ".",
    )?;

    let messages = run_session(previous_messages, "next prompt", config, Vec::new())?;
    assert_eq!(
        extract_assistant_text(&messages),
        "compacted turn completed"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("compress older conversation history")
    );
    assert!(
        requests[1]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains(COMPACTION_MARKER)
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
            "enabled_tools": [],
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let report = run_session_with_report_controlled(
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
                "content": "Goals\n- Keep working on the task\n\nConstraints\n- Stay concise"
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
        ChatMessage::text("user", "A".repeat(1800)),
        ChatMessage::text("assistant", "B".repeat(1800)),
        ChatMessage::text("user", "C".repeat(1800)),
        ChatMessage::text("assistant", "D".repeat(1800)),
    ];

    let config = load_config_value(
        json!({
            "enabled_tools": [],
            "upstream": {
                "base_url": server.address,
                "model": "fake-model",
                "context_window_tokens": 2500
            },
            "system_prompt": "Test system prompt.",
            "retain_recent_messages": 2
        }),
        ".",
    )?;

    let report = compact_session_messages_with_report(previous_messages, config, Vec::new())?;
    assert!(report.compacted);
    assert_eq!(report.usage.llm_calls, 1);
    assert_eq!(report.usage.prompt_tokens, 1200);
    assert_eq!(report.usage.cache_read_tokens, 400);
    assert_eq!(report.usage.cache_write_tokens, 200);
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
            "enabled_tools": [],
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
            "enabled_tools": [],
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
fn run_session_report_aggregates_usage_across_tool_roundtrips() -> Result<()> {
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
            "enabled_tools": [],
            "upstream": {
                "base_url": server.address,
                "model": "demo-model"
            },
            "workspace_root": temp_dir.path()
        }),
        temp_dir.path(),
    )?;

    let report = run_session_with_report(Vec::new(), "What is 6 times 7?", config, vec![multiply])?;
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
            "enabled_tools": [],
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    let control_holder = Arc::new(Mutex::new(None::<SessionExecutionControl>));
    let control = {
        let checkpoints = Arc::clone(&checkpoints);
        let control_holder = Arc::clone(&control_holder);
        SessionExecutionControl::with_checkpoint_callback(move |report| {
            checkpoints.lock().unwrap().push(report);
        })
        .with_event_callback(move |event| {
            if matches!(event, SessionEvent::ToolCallStarted { .. })
                && let Some(control) = control_holder.lock().unwrap().as_ref()
            {
                control.request_cancel();
            }
        })
    };
    *control_holder.lock().unwrap() = Some(control.clone());

    let error = run_session_with_report_controlled(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        Some(control),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    let checkpoints = checkpoints.lock().unwrap();
    assert_eq!(checkpoints.len(), 0);
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
            "enabled_tools": [],
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

    let report = run_session_with_report_controlled(
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
            "enabled_tools": [],
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    let control = {
        let checkpoints = Arc::clone(&checkpoints);
        SessionExecutionControl::with_checkpoint_callback(move |report| {
            checkpoints.lock().unwrap().push(report);
        })
    };

    let report = run_session_with_report_controlled(
        Vec::new(),
        "What is 6 times 7?",
        config,
        vec![multiply],
        Some(control),
    )?;
    assert_eq!(extract_assistant_text(&report.messages), "final answer");
    let checkpoints = checkpoints.lock().unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(
        extract_assistant_text(&checkpoints[0].messages),
        "final answer"
    );
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

    let slow_a = tool! {
        description: "Sleep briefly and return A.",
        fn slow_a() -> String {
            thread::sleep(Duration::from_millis(250));
            "A".to_string()
        }
    };
    let slow_b = tool! {
        description: "Sleep briefly and return B.",
        fn slow_b() -> String {
            thread::sleep(Duration::from_millis(250));
            "B".to_string()
        }
    };

    let config = load_config_value(
        json!({
            "enabled_tools": [],
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt."
        }),
        ".",
    )?;

    let started = std::time::Instant::now();
    let report = run_session_with_report_controlled(
        Vec::new(),
        "Run both tools.",
        config,
        vec![slow_a, slow_b],
        None,
    )?;
    let elapsed = started.elapsed();
    assert_eq!(extract_assistant_text(&report.messages), "done");
    assert!(elapsed < Duration::from_millis(430));
    Ok(())
}

#[test]
fn controlled_run_converts_tool_phase_timeout_into_observation_and_continues() -> Result<()> {
    let _guard = acquire_process_test_lock();
    let temp_dir = TempDir::new()?;
    let server = TestServer::start();
    let registry = build_tool_registry(
        &[
            "exec_start".to_string(),
            "exec_wait".to_string(),
            "exec_kill".to_string(),
        ],
        temp_dir.path(),
        temp_dir.path(),
        &test_upstream(&server.address),
        None,
        &[],
        &[],
        &[],
    )?;
    let exec_start = execute_tool_call(
        &registry,
        "exec_start",
        Some(r#"{"command":"sleep 10","include_stdout":false}"#),
    );
    let exec_start_json: Value = serde_json::from_str(&exec_start)?;
    let exec_id = exec_start_json["exec_id"]
        .as_str()
        .expect("exec_start should return exec_id")
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
                        "name": "exec_wait",
                        "arguments": serde_json::to_string(&json!({
                            "exec_id": exec_id,
                            "wait_timeout_seconds": 30,
                            "include_stdout": false
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
            "enabled_tools": ["exec_wait"],
            "upstream": {"base_url": server.address, "model": "fake-model"},
            "system_prompt": "Test system prompt.",
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

    let report = run_session_with_report_controlled(
        Vec::new(),
        "Check the environment",
        config,
        Vec::new(),
        Some(control),
    )?;
    let _ = execute_tool_call(
        &registry,
        "exec_kill",
        Some(&format!(r#"{{"exec_id":"{}"}}"#, exec_id)),
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
    assert_eq!(tool_json["tool"], json!("exec_wait"));
    Ok(())
}

#[test]
fn controlled_run_starts_tools_and_yields_after_tool_batch() -> Result<()> {
    let temp_dir = TempDir::new()?;
    fs::write(
        temp_dir.path().join("README.md"),
        "# Test Workspace\nThis file is here for read_file.\n",
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
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}"
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
            "enabled_tools": ["read_file"],
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

    let report = run_session_with_report_controlled(
        Vec::new(),
        "Check the environment",
        config,
        Vec::new(),
        Some(control),
    )?;
    assert!(report.yielded);
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
        tool_json["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("README.md"))
    );
    assert!(tool_json["content"].as_str().is_some());
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCallStarted { tool_name, .. } if tool_name == "read_file"))
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
            "enabled_tools": [],
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
