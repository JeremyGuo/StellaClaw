use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    schema::{object_schema, properties},
    PromptProtocol, ToolBackend, ToolConcurrency, ToolDefinition, ToolExecutionMode,
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

    tools.push(user_tell_tool(scope));
    tools.push(update_plan_tool());
    if enable_memory_tools {
        tools.extend(memory_tools());
    }
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

pub(crate) fn host_prompt_protocols() -> &'static [PromptProtocol] {
    HOST_PROMPT_PROTOCOLS
}

const HOST_PROMPT_PROTOCOLS: &[PromptProtocol] = &[
    PromptProtocol {
        id: "host.user_tell",
        priority: 300,
        required_tools: &["user_tell"],
        body: "Use user_tell only for mid-task progress or coordination that must become visible before the current turn is ready to finish. If you can return the final answer now, do not send an extra user_tell first. Positive example: a long-running edit, benchmark, or debug session is still in progress and the user needs an immediate visible status update. Negative example: you already have the result and are about to finish, so a separate 'working on it' or 'done' user_tell is unnecessary.",
    },
    PromptProtocol {
        id: "host.update_plan",
        priority: 310,
        required_tools: &["update_plan"],
        body: "Use update_plan for multi-step, long-running, or ambiguous work so the user can see the current checklist. Positive examples: a refactor across several files, a bug investigation with multiple plausible causes, or a task that needs several verification steps. Negative examples: a one-line fix, a single file read, or a straightforward reply that can be finished immediately without a visible plan.",
    },
    PromptProtocol {
        id: "host.memory",
        priority: 320,
        required_tools: &["memory_search", "memory_write", "memory_update", "memory_delete"],
        body: "Use memory_write only for stable information that is likely to be reused after this turn. Do not save transient execution steps, one-off tool output, guesses, ordinary chat, compacted summaries, or facts already present in memory. Save scope=user for durable collaboration preferences, response style, general corrections, and user work habits. Save scope=conversation for goals, constraints, key facts, durable state, and handoff that every session or agent in this conversation should know. Save scope=public for stable project, customer, data definition, or long-running task facts that may be useful across conversations. memory_write resolves duplicates and conflicts internally and returns only success or failure. If it fails, retry only when you can provide more specific, stable text. Long memory is not automatically injected into every turn; use memory_search when the current short context is insufficient, when the user asks about durable project/conversation facts, or when the task likely depends on prior cross-conversation or current-conversation agreements. Use memory_update or memory_delete only when search reveals an obviously stale, incomplete, duplicate, or wrong entry.",
    },
    PromptProtocol {
        id: "host.subagent",
        priority: 330,
        required_tools: &["subagent_start"],
        body: "For multi-step tasks that require more than 3 sequential tool operations and can be clearly scoped, such as exploring a codebase module, running benchmarks, or setting up a dependency, prefer subagent_start to keep the main conversation context lean. Do not batch tool calls that could cause irreversible damage if an earlier step produces unexpected results, such as destructive shell commands, production deploys, or database mutations; use subagent_start for those instead so intermediate results can be inspected.",
    },
];

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

fn memory_tools() -> Vec<ToolDefinition> {
    vec![
        bridge_tool(
            "memory_search",
            "Search long memory for stable facts from the current conversation and public cross-conversation memory. Use this before answering questions about durable facts that may be outside the current short context.",
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
            "Write stable long memory only when the information is durable, likely to be reused later, and still useful after the current turn. Do not store transient steps, one-off tool output, guesses, ordinary chat, compacted summaries, or facts already present in memory. Scope user is for durable collaboration preferences and general corrections; conversation is for goals, constraints, key facts, state, and handoff that all sessions in this conversation should know; public is for stable project/customer/data/task facts useful across conversations. The host resolves duplicates and conflicts internally and returns only success or failure.",
            object_schema(
                properties([
                    (
                        "scope",
                        json!({
                            "type": "string",
                            "enum": ["user", "public", "conversation"],
                            "description": "user for durable collaboration preferences; conversation for this conversation's shared long memory; public for cross-conversation long memory."
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
            "Replace a memory entry by id when search reveals an obviously stale, incomplete, or wrong entry.",
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
            "Delete or tombstone a memory entry by id when search reveals it is obsolete, duplicate, or wrong.",
            object_schema(
                properties([("memory_id", json!({"type": "string"}))]),
                &["memory_id"],
            ),
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
