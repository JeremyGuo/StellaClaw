use serde_json::json;

use crate::session_actor::{tool_runtime::LocalToolError, SessionSkillObservation};

use super::{
    schema::{object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode,
};

pub fn skill_tool_definitions(
    skill_names: &[String],
    enable_skill_persistence_tools: bool,
) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if !skill_names.is_empty() {
        tools.push(ToolDefinition::new(
            "skill_load",
            "Load the SKILL.md instructions for a named skill. Use exact skill names from the preloaded metadata.",
            object_schema(
                properties([(
                    "skill_name",
                    json!({"type": "string", "enum": skill_names}),
                )]),
                &["skill_name"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ));
    }

    if enable_skill_persistence_tools {
        tools.push(ToolDefinition::new(
            "skill_create",
            "Persist a staged skill directory from .skills/<skill_name>/ in the current workspace into the runtime skills store as a new skill. Validate SKILL.md and fail with the validation reason if invalid.",
            object_schema(
                properties([("skill_name", json!({"type": "string"}))]),
                &["skill_name"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::ConversationBridge {
                action: "skill_create".to_string(),
            },
        ));
        tools.push(ToolDefinition::new(
            "skill_update",
            "Persist a staged skill directory from .skills/<skill_name>/ in the current workspace into the runtime skills store as an update to an existing skill. Validate SKILL.md and fail with the validation reason if invalid.",
            object_schema(
                properties([("skill_name", json!({"type": "string"}))]),
                &["skill_name"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::ConversationBridge {
                action: "skill_update".to_string(),
            },
        ));
    }

    tools
}

pub(crate) fn execute_skill_load_tool(
    skill: &SessionSkillObservation,
) -> Result<serde_json::Value, LocalToolError> {
    if skill.name.trim().is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "skill name must not be empty".to_string(),
        ));
    }

    Ok(json!({
        "name": skill.name,
        "description": skill.description,
        "content": skill.content,
    }))
}
