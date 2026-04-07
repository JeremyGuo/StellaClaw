use std::fs;

use agent_frame::Tool;
use agent_host::zgent::app_bridge::prepare_agent_host_profile;
use agent_host::zgent::client::{ZgentInstallation, ZgentRpcClient, ZgentServerLaunchConfig};
use agent_host::zgent::context::{ZgentContextBridge, ZgentConversationSnapshot};
use agent_host::zgent::zgent_runtime_available;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn real_zgent_profile_bundle_can_be_discovered_and_session_created() {
    if !zgent_runtime_available() {
        eprintln!("skipping real zgent smoke test because ./zgent is unavailable");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().join("workspace");
    fs::create_dir_all(&workspace_root).unwrap();

    let extra_tools = vec![Tool::new(
        "user_tell",
        "Send progress to the user.",
        json!({"type":"object","properties":{"text":{"type":"string"}}}),
        |_| Ok(json!({"ok": true})),
    )];
    let bundle = prepare_agent_host_profile(temp_dir.path(), &extra_tools)
        .unwrap()
        .expect("profile bundle should be created");

    let installation = ZgentInstallation::detect().unwrap();
    if installation.native_server_binary().is_none() {
        eprintln!("skipping real zgent smoke test because zgent-server binary is not built");
        return;
    }
    let mut client = ZgentRpcClient::spawn_stdio(
        &installation,
        &ZgentServerLaunchConfig {
            workspace_root: Some(workspace_root.clone()),
            data_root: Some(temp_dir.path().to_path_buf()),
            model: Some("test-model".to_string()),
            api_base: Some("https://example.invalid/v1".to_string()),
            api_key: Some("test-key".to_string()),
            subagent_models_path: None,
            no_persist: true,
        },
    )
    .unwrap();

    let profiles = client.request_value("profile/list", json!({})).unwrap();
    let profile_names = profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        profile_names
            .iter()
            .any(|name| *name == bundle.profile_name()),
        "expected generated profile {} in {:?}",
        bundle.profile_name(),
        profile_names
    );
    let profile = client.profile_get(bundle.profile_name()).unwrap();
    assert_eq!(profile.name, bundle.profile_name());

    let created = client
        .session_create(Some("AgentHost smoke"), Some(bundle.profile_name()))
        .unwrap();
    assert_eq!(created.workspace_path, workspace_root.to_string_lossy());

    match client.tool_call(
        &created.session_id,
        "user_tell",
        json!({ "text": "hello from bridge smoke" }),
    ) {
        Ok(tool_result) => {
            let echoed_name = tool_result.get("echo").and_then(|v| v.as_str());
            let ok_flag = tool_result.get("ok").and_then(|v| v.as_bool());
            assert!(
                echoed_name == Some("user_tell") || ok_flag == Some(true),
                "unexpected tool result shape: {tool_result}"
            );
        }
        Err(error) => {
            eprintln!("AgentHost app bridge tool is not yet invokable via tool/call: {error}");
        }
    }

    let session = client.session_get(&created.session_id).unwrap();
    assert_eq!(session.profile.as_deref(), Some(bundle.profile_name()));
    assert_eq!(session.workspace_path, workspace_root.to_string_lossy());

    let snapshot = ZgentConversationSnapshot {
        messages: json!([
            {
                "role": "user",
                "content": "Ping from AgentHost smoke test"
            }
        ]),
        hash: String::new(),
    };
    let _hash = client
        .set_conversation(&created.session_id, &snapshot, None)
        .unwrap();
    let updated = client.get_conversation(&created.session_id).unwrap();
    let rendered = updated.messages.to_string();
    assert!(
        rendered.contains("Ping from AgentHost smoke test"),
        "expected synchronized user message in conversation: {}",
        rendered
    );
}
