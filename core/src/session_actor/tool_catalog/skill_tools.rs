use serde_json::json;

use crate::session_actor::{tool_runtime::LocalToolError, SessionSkillObservation};

use super::{
    schema::{object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode,
};

pub fn skill_tool_definitions(
    _skill_names: &[String],
    enable_skill_persistence_tools: bool,
) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    tools.push(ToolDefinition::new(
        "skill_load",
        "Load the SKILL.md instructions for a named skill from the current workspace .skill directory. Use exact skill names that currently exist under .skill/.",
        object_schema(
            properties([("skill_name", json!({"type": "string"}))]),
            &["skill_name"],
        ),
        ToolExecutionMode::Immediate,
        ToolBackend::Local,
    ));

    if enable_skill_persistence_tools {
        tools.push(ToolDefinition::new(
            "skill_create",
            "Persist a staged skill directory from .skill/<skill_name>/ in the current workspace into the runtime skills store as a new skill. Validate SKILL.md and fail with the validation reason if invalid.",
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
            "Persist a staged skill directory from .skill/<skill_name>/ in the current workspace into the runtime skills store as an update to an existing skill. Validate SKILL.md and fail with the validation reason if invalid.",
            object_schema(
                properties([("skill_name", json!({"type": "string"}))]),
                &["skill_name"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::ConversationBridge {
                action: "skill_update".to_string(),
            },
        ));
        tools.push(ToolDefinition::new(
            "skill_delete",
            "Persist deletion of an existing skill by removing .skill/<skill_name>/ from the runtime skills store and active local workspaces.",
            object_schema(
                properties([("skill_name", json!({"type": "string"}))]),
                &["skill_name"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::ConversationBridge {
                action: "skill_delete".to_string(),
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
