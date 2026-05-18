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

pub fn host_tool_definitions(
    scope: HostToolScope,
    enable_memory_tools: bool,
) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if matches!(
        scope,
        HostToolScope::MainForeground | HostToolScope::MainBackground
    ) {
        tools.extend(cron_tools());
        tools.extend(managed_agent_tools());
    }

    tools.push(update_plan_tool());
    if enable_memory_tools {
        tools.extend(memory_tools());
    }
    tools.extend(subagent_tools());

    match scope {
        HostToolScope::MainForeground => {
            tools.push(background_agent_start_tool());
        }
        HostToolScope::MainBackground => {
            tools.push(terminate_tool());
        }
        HostToolScope::SubAgent => {}
    }

    tools
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
            "Start a session-bound subagent for a small delegated task. For multi-step tasks that require more than 3 sequential tool operations and can be clearly scoped, such as exploring a codebase module, running benchmarks, or setting up a dependency, prefer this tool to keep the main conversation context lean. Do not batch tool calls that could cause irreversible damage if an earlier step produces unexpected results, such as destructive shell commands, production deploys, or database mutations; use this tool for those instead so intermediate results can be inspected. Requires description. The subagent always inherits this conversation's current model.",
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

fn background_agent_start_tool() -> ToolDefinition {
    bridge_tool(
        "background_agent_start",
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
            "cron_tasks_list",
            "List configured cron tasks. Returns summaries including enabled state and next_run_at.",
            object_schema(properties([]), &[]),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "cron_task_get",
            "Get full details for a cron task by id.",
            object_schema(properties([("id", json!({"type": "string"}))]), &["id"]),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "cron_task_create",
            "Create a persisted cron task owned by this session. Provide each cron time field as a named argument; the host builds a seconds-first cron expression in the task timezone. task launches a background agent with that prompt.",
            cron_create_schema(),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "cron_task_update",
            "Update a cron task owned by this session. To change timing, provide all named cron fields together: cron_second, cron_minute, cron_hour, cron_day_of_month, cron_month, cron_day_of_week, plus optional cron_year. Use timezone to change the IANA timezone and enabled to pause or resume it. Setting task changes the background-agent prompt.",
            cron_update_schema(),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "cron_task_remove",
            "Remove a cron task permanently.",
            object_schema(properties([("id", json!({"type": "string"}))]), &["id"]),
            ToolExecutionMode::Immediate,
        ),
    ]
}

fn memory_tools() -> Vec<ToolDefinition> {
    vec![
        bridge_tool(
            "memory_search",
            "Search long memory for durable facts from conversation and public scopes.",
            object_schema(
                properties([
                    ("query", json!({"type": "string", "description": "Natural language search query."})),
                    ("limit", json!({"type": "number", "description": "Optional maximum result count. Defaults to 5 and is capped by the host."})),
                    (
                        "scopes",
                        json!({
                            "type": "array",
                            "items": {"type": "string", "enum": ["conversation", "public"]},
                            "description": "Optional scopes to search. Defaults to conversation and public."
                        }),
                    ),
                ]),
                &["query"],
            ),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "memory_write",
            "Persist one concise long-memory entry. The host may deduplicate or merge conflicting entries and returns success or failure.",
            object_schema(
                properties([
                    (
                        "scope",
                        json!({
                            "type": "string",
                            "enum": ["user", "public", "conversation"],
                            "description": "Memory scope: user, conversation, or public."
                        }),
                    ),
                    ("subject", json!({"type": "string", "description": "Optional short subject or entity name."})),
                    ("text", json!({"type": "string", "description": "Compact durable memory text. About 1KB maximum."})),
                    ("tags", json!({"type": "array", "items": {"type": "string"}, "description": "Optional compact tags."})),
                ]),
                &["scope", "text"],
            ),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "memory_update",
            "Replace a memory entry by id.",
            object_schema(
                properties([
                    ("memory_id", json!({"type": "string"})),
                    ("text", json!({"type": "string"})),
                ]),
                &["memory_id", "text"],
            ),
            ToolExecutionMode::Immediate,
        ),
        bridge_tool(
            "memory_delete",
            "Delete or tombstone a memory entry by id.",
            object_schema(
                properties([("memory_id", json!({"type": "string"}))]),
                &["memory_id"],
            ),
            ToolExecutionMode::Immediate,
        ),
    ]
}

fn managed_agent_tools() -> Vec<ToolDefinition> {
    vec![bridge_tool(
        "background_agents_list",
        "List tracked background agents and subagents with status, task, latest message, and latest error.",
        object_schema(properties([]), &[]),
        ToolExecutionMode::Immediate,
    )]
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
            "task",
        ],
    )
}

fn cron_update_schema() -> serde_json::Value {
    let mut schema_properties = cron_common_properties();
    schema_properties.insert("id".to_string(), json!({"type": "string"}));
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
            json!({"type": "string", "description": "Prompt for a background agent."}),
        ),
        ("enabled", json!({"type": "boolean"})),
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
