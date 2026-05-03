use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    schema::{object_schema, properties},
    ToolBackend, ToolConcurrency, ToolDefinition, ToolExecutionMode,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostToolScope {
    MainForeground,
    MainBackground,
    SubAgent,
}

pub fn host_tool_definitions(scope: HostToolScope) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if matches!(
        scope,
        HostToolScope::MainForeground | HostToolScope::MainBackground
    ) {
        tools.extend(cron_tools());
        tools.extend(managed_agent_tools());
    }

    tools.push(user_tell_tool(scope));
    tools.push(update_plan_tool());
    tools.extend(subagent_tools());

    match scope {
        HostToolScope::MainForeground => {
            tools.push(start_background_agent_tool());
        }
        HostToolScope::MainBackground => {
            tools.push(terminate_tool());
        }
        HostToolScope::SubAgent => {}
    }

    tools
}

fn user_tell_tool(scope: HostToolScope) -> ToolDefinition {
    let description = match scope {
        HostToolScope::MainBackground => {
            "Immediately send a short progress or coordination message to the current user conversation without waiting for the current background turn to finish; do not use user_tell for the primary result. Put that primary user-facing message in your final answer instead. To include files or images, append one or more <attachment>relative/path/from/workspace_root</attachment> tags inside text."
        }
        HostToolScope::MainForeground | HostToolScope::SubAgent => {
            "Immediately send a short progress or coordination message to the current user conversation without waiting for the current turn to finish. To include files or images, append one or more <attachment>relative/path/from/workspace_root</attachment> tags inside text."
        }
    };

    bridge_tool(
        "user_tell",
        description,
        object_schema(properties([("text", json!({"type": "string"}))]), &["text"]),
        ToolExecutionMode::Immediate,
    )
}

fn update_plan_tool() -> ToolDefinition {
    bridge_tool(
        "update_plan",
        "Replace the current task plan shown to the user.",
        object_schema(
            properties([
                ("explanation", json!({"type": "string"})),
                (
                    "plan",
                    json!({
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {"type": "string"},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }),
                ),
            ]),
            &["plan"],
        ),
        ToolExecutionMode::Immediate,
    )
}

fn subagent_tools() -> Vec<ToolDefinition> {
    vec![
        bridge_tool(
            "subagent_start",
            "Start a session-bound subagent for a small delegated task. Requires description. The subagent always inherits this conversation's current model.",
            object_schema(
                properties([("description", json!({"type": "string"}))]),
                &["description"],
            ),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "subagent_kill",
            "Kill a running subagent and clean up its state.",
            object_schema(
                properties([("agent_id", json!({"type": "string"}))]),
                &["agent_id"],
            ),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "subagent_join",
            "Wait until a subagent finishes or fails. Supports an optional timeout_seconds; timing out returns a still-running result without killing the subagent. Finished or failed subagents are destroyed immediately after join returns them.",
            object_schema(
                properties([
                    ("agent_id", json!({"type": "string"})),
                    ("timeout_seconds", json!({"type": "number"})),
                ]),
                &["agent_id"],
            ),
            ToolExecutionMode::Interruptible,
        ),
    ]
}

fn start_background_agent_tool() -> ToolDefinition {
    bridge_tool(
        "start_background_agent",
        "Start a main background agent. Requires task. The background agent inherits this conversation's current model. The final user-facing reply is delivered to the current foreground conversation and inserted into the main foreground context.",
        object_schema(properties([("task", json!({"type": "string"}))]), &["task"]),
        ToolExecutionMode::Immediate,
    )
}

fn terminate_tool() -> ToolDefinition {
    bridge_tool(
        "terminate",
        "Terminate this main background agent silently. Use this when the task should stop without sending any user-facing reply or inserting anything into the main foreground context.",
        object_schema(properties([]), &[]),
        ToolExecutionMode::Immediate,
    )
}

fn cron_tools() -> Vec<ToolDefinition> {
    vec![
        bridge_tool(
            "list_cron_tasks",
            "List configured cron tasks. Returns summaries including enabled state and next_run_at.",
            object_schema(properties([]), &[]),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "get_cron_task",
            "Get full details for a cron task by id.",
            object_schema(properties([("id", json!({"type": "string"}))]), &["id"]),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "create_cron_task",
            "Create a persisted cron task. Provide each cron time field as a named argument; the host builds a seconds-first cron expression in the task timezone. Set exactly one of task or script_command. task launches a background agent with that prompt. script_command runs as sh -lc inside the conversation sandbox; exit 0 parses stdout as up to three JSONL messages like {\"to\":[\"user\",\"foreground\"],\"text\":\"...\"}, while non-zero disables the cron and notifies the user plus foreground agent. Before saving a script, verify it with the shell tool.",
            cron_create_schema(),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "update_cron_task",
            "Update a cron task. To change timing, provide all named cron fields together: cron_second, cron_minute, cron_hour, cron_day_of_month, cron_month, cron_day_of_week, plus optional cron_year. Use timezone to change the IANA timezone and enabled to pause or resume it. Setting task switches to prompt mode and clears the script; setting script_command switches to script mode and clears task. clear_script removes the script, and clear_task removes the prompt.",
            cron_update_schema(),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "remove_cron_task",
            "Remove a cron task permanently.",
            object_schema(properties([("id", json!({"type": "string"}))]), &["id"]),
            ToolExecutionMode::Immediate,
        ),
    ]
}

fn managed_agent_tools() -> Vec<ToolDefinition> {
    vec![
        bridge_tool(
            "background_agents_list",
            "List tracked background agents with status, model, and token usage statistics.",
            object_schema(properties([]), &[]),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "get_agent_stats",
            "Get detailed status and token usage statistics for a tracked background agent or subagent by agent_id.",
            object_schema(
                properties([("agent_id", json!({"type": "string"}))]),
                &["agent_id"],
            ),
            ToolExecutionMode::Immediate,
        ),
    ]
}

fn cron_create_schema() -> serde_json::Value {
    object_schema(
        cron_common_properties(),
        &[
            "name",
            "description",
            "cron_second",
            "cron_minute",
            "cron_hour",
            "cron_day_of_month",
            "cron_month",
            "cron_day_of_week",
        ],
    )
}

fn cron_update_schema() -> serde_json::Value {
    let mut schema_properties = cron_common_properties();
    schema_properties.insert("id".to_string(), json!({"type": "string"}));
    schema_properties.insert("model".to_string(), json!({"type": "string"}));
    schema_properties.insert("clear_script".to_string(), json!({"type": "boolean"}));
    schema_properties.insert("clear_task".to_string(), json!({"type": "boolean"}));
    object_schema(schema_properties, &["id"])
}

fn cron_common_properties() -> serde_json::Map<String, serde_json::Value> {
    properties([
        ("name", json!({"type": "string"})),
        ("description", json!({"type": "string"})),
        (
            "cron_second",
            json!({"type": "string", "description": "Seconds field. Examples: '0', '*/30', '*'."}),
        ),
        (
            "cron_minute",
            json!({"type": "string", "description": "Minutes field. Examples: '0', '*/5', '*'."}),
        ),
        (
            "cron_hour",
            json!({"type": "string", "description": "Hours field in the task timezone. Examples: '13', '9-17', '*'."}),
        ),
        (
            "cron_day_of_month",
            json!({"type": "string", "description": "Day-of-month field in the task timezone. Examples: '17', '1,15', '*'."}),
        ),
        (
            "cron_month",
            json!({"type": "string", "description": "Month field in the task timezone. Examples: '4', '1-12', '*'."}),
        ),
        (
            "cron_day_of_week",
            json!({"type": "string", "description": "Day-of-week field in the task timezone. Examples: '*', 'Mon-Fri', '0'."}),
        ),
        (
            "cron_year",
            json!({"type": "string", "description": "Optional year field in the task timezone. Example: '2026'."}),
        ),
        (
            "timezone",
            json!({"type": "string", "description": "IANA timezone for these cron fields, e.g. 'Asia/Shanghai'. Defaults to 'Asia/Shanghai'."}),
        ),
        (
            "task",
            json!({"type": "string", "description": "Prompt for a background agent. Mutually exclusive with script_command."}),
        ),
        ("enabled", json!({"type": "boolean"})),
        (
            "script_command",
            json!({"type": "string", "description": "Shell script executed as sh -lc inside the conversation sandbox. Mutually exclusive with task. Exit 0 parses stdout as JSONL delivery messages; non-zero disables the cron and notifies user plus foreground agent."}),
        ),
        (
            "script_timeout_seconds",
            json!({"type": "number", "description": "Optional positive timeout for script_command. Defaults to 30 seconds."}),
        ),
        (
            "script_cwd",
            json!({"type": "string", "description": "Optional relative working directory inside the conversation workspace for script_command. Absolute paths and '..' are rejected."}),
        ),
    ])
}

fn bridge_tool(
    name: &'static str,
    description: &'static str,
    parameters: serde_json::Value,
    execution_mode: ToolExecutionMode,
) -> ToolDefinition {
    ToolDefinition::new(
        name,
        description,
        parameters,
        execution_mode,
        ToolBackend::ConversationBridge {
            action: name.to_string(),
        },
    )
    .with_concurrency(ToolConcurrency::Serial)
}
