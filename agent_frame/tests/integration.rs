use agent_frame::compaction::COMPACTION_MARKER;
use agent_frame::message::ChatMessage;
use agent_frame::skills::{build_skills_meta_prompt, discover_skills};
use agent_frame::tool;
use agent_frame::tooling::{build_tool_registry, execute_tool_call};
use agent_frame::{
    ExternalWebSearchConfig, NativeWebSearchConfig, SessionExecutionControl, UpstreamConfig,
    compact_session_messages_with_report, extract_assistant_text, load_config_value, run_session,
    run_session_with_report, run_session_with_report_controlled,
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
use std::sync::{Arc, Mutex};
use std::thread;
use tempfile::TempDir;

struct TestServer {
    address: String,
    responses: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
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
        supports_vision_input: false,
        api_key: None,
        api_key_env: "TEST_API_KEY".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
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

    let skills = discover_skills(&[skill_root])?;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "demo-skill");
    let prompt = build_skills_meta_prompt(&skills);
    assert!(prompt.contains("demo-skill"));
    assert!(prompt.contains("load_skill"));
    assert!(!prompt.contains(&skill_dir.display().to_string()));
    Ok(())
}

#[test]
fn builtin_tools_work() -> Result<()> {
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
            "exec".to_string(),
            "process".to_string(),
            "web_fetch".to_string(),
            "web_search".to_string(),
            "image".to_string(),
        ],
        temp_dir.path(),
        &upstream,
        None,
        &[],
        &[],
    )?;

    let write_result = execute_tool_call(
        &registry,
        "write_file",
        Some(r#"{"path":"note.txt","content":"alpha\nbeta\n","timeout_seconds":2}"#),
    );
    assert!(write_result.contains("note.txt"));

    let read_result = execute_tool_call(
        &registry,
        "read_file",
        Some(r#"{"path":"note.txt","timeout_seconds":2,"offset_lines":0,"limit_lines":10}"#),
    );
    assert!(read_result.contains("1: alpha"));
    assert!(read_result.contains("2: beta"));

    let edit_result = execute_tool_call(
        &registry,
        "edit",
        Some(r#"{"path":"note.txt","old_text":"alpha","new_text":"gamma","timeout_seconds":2}"#),
    );
    assert!(edit_result.contains("\"replacements\": 1"));

    let read_after_edit = execute_tool_call(
        &registry,
        "read_file",
        Some(r#"{"path":"note.txt","timeout_seconds":2,"offset_lines":0,"limit_lines":10}"#),
    );
    assert!(read_after_edit.contains("1: gamma"));

    let shell_result = execute_tool_call(
        &registry,
        "exec",
        Some(r#"{"command":"printf 123","timeout_seconds":2}"#),
    );
    assert!(shell_result.contains("\"stdout\": \"123\""));

    let background_process = execute_tool_call(
        &registry,
        "exec",
        Some(r#"{"command":"printf bg","timeout_seconds":2,"wait":false}"#),
    );
    assert!(background_process.contains("process_id"));
    let background_json: Value = serde_json::from_str(&background_process)?;
    let process_result = execute_tool_call(
        &registry,
        "process",
        Some(&format!(
            r#"{{"action":"inspect","process_id":"{}","tail_bytes":1000}}"#,
            background_json["process_id"].as_str().unwrap()
        )),
    );
    assert!(process_result.contains("\"process_id\""));

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
            r#"{{"patch":{},"timeout_seconds":2,"strip":1}}"#,
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
    assert!(fetch_result.contains("\"status\": 200"));
    assert!(fetch_result.contains("/ping"));

    let search_result = execute_tool_call(
        &registry,
        "web_search",
        Some(r#"{"query":"demo query","timeout_seconds":2,"max_results":2}"#),
    );
    assert!(search_result.contains("Search answer"));
    assert!(search_result.contains("example.com/a"));

    let image_path = temp_dir.path().join("diagram.png");
    fs::write(&image_path, [1_u8, 2, 3, 4])?;
    let image_result = execute_tool_call(
        &registry,
        "image",
        Some(r#"{"path":"diagram.png","question":"What does this image show?","timeout_seconds":2}"#),
    );
    assert!(image_result.contains("handwritten note"));

    let requests = server.requests();
    let image_request = requests.last().expect("image request");
    let messages = image_request["messages"].as_array().expect("messages array");
    let user_content = messages[1]["content"].as_array().expect("multimodal content");
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
        "exec",
        Some(&format!(
            r#"{{"command":"sleep 1; printf late > {}","timeout_seconds":0.1}}"#,
            timeout_target.display()
        )),
    );
    assert!(timeout_result.contains("timed out"));
    thread::sleep(std::time::Duration::from_millis(1200));
    assert!(!timeout_target.exists());
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

    let skills = discover_skills(&[skill_root])?;
    let registry = build_tool_registry(
        &[],
        temp_dir.path(),
        &test_upstream("http://127.0.0.1:1"),
        None,
        &skills,
        &[],
    )?;
    let result = execute_tool_call(
        &registry,
        "load_skill",
        Some(r#"{"skill_name":"demo-skill","timeout_seconds":2}"#),
    );

    assert!(result.contains("\"name\": \"demo-skill\""));
    assert!(result.contains("Use this skill carefully."));
    assert!(!result.contains(&skill_dir.display().to_string()));
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
        &upstream,
        None,
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
    assert!(system_prompt.contains("load_skill"));
    assert!(!system_prompt.contains(&skill_dir.display().to_string()));
    assert_eq!(requests[0]["tools"][0]["function"]["name"], "load_skill");
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
            if let Some(control) = control_holder.lock().unwrap().as_ref() {
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
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(extract_assistant_text(&checkpoints[0].messages), "draft answer");
    assert_eq!(checkpoints[0].usage.total_tokens, 26);
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
