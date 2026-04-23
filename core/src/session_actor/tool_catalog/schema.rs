use serde_json::{json, Map, Value};

use super::ToolRemoteMode;

pub(super) fn object_schema(schema_properties: Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": schema_properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub(super) fn properties(
    items: impl IntoIterator<Item = (&'static str, Value)>,
) -> Map<String, Value> {
    items
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

pub(super) fn file_tool_schema(
    mut schema_properties: Map<String, Value>,
    required: &[&str],
    remote_mode: &ToolRemoteMode,
) -> Value {
    add_remote_property(&mut schema_properties, remote_mode);
    object_schema(schema_properties, required)
}

pub(super) fn add_remote_property(
    schema_properties: &mut Map<String, Value>,
    remote_mode: &ToolRemoteMode,
) {
    if matches!(remote_mode, ToolRemoteMode::Selectable) {
        schema_properties.insert("remote".to_string(), remote_schema_property());
    }
}

pub(super) fn add_images_property(
    schema_properties: &mut Map<String, Value>,
    upstream_supports_vision: bool,
) {
    if upstream_supports_vision {
        schema_properties.insert(
            "images".to_string(),
            json!({
                "type": "array",
                "items": { "type": "string" }
            }),
        );
    }
}

fn remote_schema_property() -> Value {
    json!({
        "type": "string",
        "description": "Execution target: SSH Host alias from ~/.ssh/config. Omit for local work."
    })
}
