use std::collections::BTreeMap;
use std::fs;

use agent_frame::Tool;
use anyhow::{Context, Result};

use crate::zgent::client::ZgentInstallation;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZgentNativeToolCatalog {
    pub tool_descriptions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentHostOnlyToolCatalog {
    pub tool_descriptions: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
pub struct NativeKernelToolInjectionPlan {
    pub forwarded_tools: Vec<Tool>,
    pub shadowed_native_tool_names: Vec<String>,
}

impl ZgentNativeToolCatalog {
    pub fn discover_from_source(installation: &ZgentInstallation) -> Result<Self> {
        let builtins_mod = installation
            .root_dir
            .join("crates/zgent-core/src/tools/builtins/mod.rs");
        let source = fs::read_to_string(&builtins_mod)
            .with_context(|| format!("failed to read {}", builtins_mod.display()))?;
        Ok(Self {
            tool_descriptions: parse_builtin_tool_catalog(&source),
        })
    }

    pub fn merge_agent_host_only_tools(
        &self,
        agent_host_only_tools: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut merged = self.tool_descriptions.clone();
        for (name, description) in agent_host_only_tools {
            merged
                .entry(name.clone())
                .or_insert_with(|| description.clone());
        }
        merged
    }
}

impl AgentHostOnlyToolCatalog {
    pub fn from_extra_tools(extra_tools: &[Tool]) -> Self {
        let mut tool_descriptions = BTreeMap::new();
        for tool in extra_tools {
            tool_descriptions.insert(tool.name.clone(), tool.description.clone());
        }
        Self { tool_descriptions }
    }

    pub fn is_empty(&self) -> bool {
        self.tool_descriptions.is_empty()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tool_descriptions.keys().cloned().collect()
    }
}

pub fn native_kernel_execution_supported(extra_tools: &[Tool]) -> bool {
    let _ = AgentHostOnlyToolCatalog::from_extra_tools(extra_tools);
    true
}

pub fn plan_agent_host_tool_injection(
    installation: &ZgentInstallation,
    extra_tools: &[Tool],
) -> Result<NativeKernelToolInjectionPlan> {
    let native_catalog = ZgentNativeToolCatalog::discover_from_source(installation)?;
    let mut forwarded_tools = Vec::new();
    let mut shadowed_native_tool_names = Vec::new();

    for tool in extra_tools {
        if native_catalog.tool_descriptions.contains_key(&tool.name) {
            shadowed_native_tool_names.push(tool.name.clone());
        } else {
            forwarded_tools.push(tool.clone());
        }
    }

    Ok(NativeKernelToolInjectionPlan {
        forwarded_tools,
        shadowed_native_tool_names,
    })
}

fn parse_builtin_tool_catalog(source: &str) -> BTreeMap<String, String> {
    let consts = parse_string_constants(source);
    let mut catalog = BTreeMap::new();
    let mut offset = 0usize;
    while let Some(index) = source[offset..].find("tool_def(") {
        let start = offset + index + "tool_def(".len();
        if let Some((name_expr, after_name)) = parse_argument_expr(source, start) {
            if let Some((desc_expr, after_desc)) = parse_argument_expr(source, after_name) {
                if let (Some(name), Some(description)) = (
                    resolve_string_expr(&name_expr, &consts),
                    resolve_string_expr(&desc_expr, &consts),
                ) {
                    catalog.insert(name, description);
                }
                offset = after_desc;
                continue;
            }
        }
        offset = start;
    }
    catalog
}

fn parse_string_constants(source: &str) -> BTreeMap<String, String> {
    let mut constants = BTreeMap::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if !(trimmed.starts_with("const ") || trimmed.starts_with("pub const ")) {
            continue;
        }
        let Some((name_part, value_part)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name_part
            .split_whitespace()
            .filter(|token| *token != "pub" && *token != "const")
            .find(|token| {
                token.contains(':')
                    || token
                        .chars()
                        .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
            })
            .unwrap_or_default()
            .trim_end_matches(':');
        if name.is_empty() {
            continue;
        }
        let value_expr = value_part.trim().trim_end_matches(';').trim();
        if let Some(value) = parse_string_literal(value_expr) {
            constants.insert(name.to_string(), value);
        }
    }
    constants
}

fn parse_argument_expr(source: &str, mut index: usize) -> Option<(String, usize)> {
    while let Some(ch) = source[index..].chars().next() {
        if ch.is_whitespace() {
            index += ch.len_utf8();
            continue;
        }
        break;
    }

    let mut expr = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut string_delim = '"';
    let mut chars = source[index..].char_indices().peekable();

    while let Some((rel, ch)) = chars.next() {
        let abs = index + rel;
        if in_string {
            expr.push(ch);
            if ch == '\\' {
                if let Some((_, escaped)) = chars.next() {
                    expr.push(escaped);
                }
                continue;
            }
            if ch == string_delim {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                string_delim = ch;
                expr.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                expr.push(ch);
            }
            ')' | ']' | '}' => {
                if depth == 0 {
                    return Some((expr.trim().to_string(), abs));
                }
                depth = depth.saturating_sub(1);
                expr.push(ch);
            }
            ',' if depth == 0 => {
                return Some((expr.trim().to_string(), abs + 1));
            }
            _ => expr.push(ch),
        }
    }

    if expr.trim().is_empty() {
        None
    } else {
        Some((expr.trim().to_string(), source.len()))
    }
}

fn resolve_string_expr(expr: &str, constants: &BTreeMap<String, String>) -> Option<String> {
    parse_string_literal(expr).or_else(|| constants.get(expr).cloned())
}

fn parse_string_literal(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') {
        return serde_json::from_str::<String>(trimmed).ok();
    }

    if let Some(stripped) = trimmed
        .strip_prefix("r#\"")
        .and_then(|value| value.strip_suffix("\"#"))
    {
        return Some(stripped.to_string());
    }

    if let Some(stripped) = trimmed
        .strip_prefix("r##\"")
        .and_then(|value| value.strip_suffix("\"##"))
    {
        return Some(stripped.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        AgentHostOnlyToolCatalog, ZgentNativeToolCatalog, native_kernel_execution_supported,
        parse_builtin_tool_catalog, plan_agent_host_tool_injection,
    };
    use crate::zgent::client::ZgentInstallation;
    use agent_frame::Tool;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parses_builtin_tool_catalog_from_source_snippet() {
        let source = r#"
pub const TOOL_LIST_AVAILABLE_MODELS: &str = "list_available_models";

let tools = vec![
    tool_def("read_file", "Read a file.", serde_json::json!({})),
    tool_def(TOOL_LIST_AVAILABLE_MODELS, "List models.", serde_json::json!({})),
];
"#;

        let catalog = parse_builtin_tool_catalog(source);
        assert_eq!(
            catalog.get("read_file").map(String::as_str),
            Some("Read a file.")
        );
        assert_eq!(
            catalog.get("list_available_models").map(String::as_str),
            Some("List models.")
        );
    }

    #[test]
    fn discovers_native_tools_from_real_zgent_source_tree() {
        let temp_dir = TempDir::new().unwrap();
        let builtins_dir = temp_dir.path().join("crates/zgent-core/src/tools/builtins");
        fs::create_dir_all(&builtins_dir).unwrap();
        fs::write(
            builtins_dir.join("mod.rs"),
            r#"
pub const TOOL_ASK_QUESTIONS: &str = "ask_questions";
let tools = vec![
    tool_def("read_file", "Read a file.", serde_json::json!({})),
];
registry.register(
    tool_def(TOOL_ASK_QUESTIONS, "Ask clarifying questions.", serde_json::json!({})),
    handler,
);
"#,
        )
        .unwrap();

        let installation = ZgentInstallation {
            root_dir: temp_dir.path().to_path_buf(),
        };
        let catalog = ZgentNativeToolCatalog::discover_from_source(&installation).unwrap();
        assert_eq!(
            catalog
                .tool_descriptions
                .get("read_file")
                .map(String::as_str),
            Some("Read a file.")
        );
        assert_eq!(
            catalog
                .tool_descriptions
                .get("ask_questions")
                .map(String::as_str),
            Some("Ask clarifying questions.")
        );
    }

    #[test]
    fn agent_host_only_catalog_collects_extra_tool_definitions() {
        let extra_tools = vec![Tool::new(
            "user_tell",
            "Send progress to the user.",
            json!({"type":"object","properties":{}}),
            |_| Ok(json!({"ok": true})),
        )];
        let catalog = AgentHostOnlyToolCatalog::from_extra_tools(&extra_tools);
        assert_eq!(catalog.tool_names(), vec!["user_tell".to_string()]);
        assert!(native_kernel_execution_supported(&extra_tools));
        assert!(native_kernel_execution_supported(&[]));
    }

    #[test]
    fn injection_plan_filters_out_tools_shadowed_by_native_catalog() {
        let temp_dir = TempDir::new().unwrap();
        let builtins_dir = temp_dir.path().join("crates/zgent-core/src/tools/builtins");
        fs::create_dir_all(&builtins_dir).unwrap();
        fs::write(
            builtins_dir.join("mod.rs"),
            r#"
let tools = vec![
    tool_def("read_file", "Read a file.", serde_json::json!({})),
    tool_def("exec", "Run a command.", serde_json::json!({})),
];
"#,
        )
        .unwrap();

        let installation = ZgentInstallation {
            root_dir: temp_dir.path().to_path_buf(),
        };
        let extra_tools = vec![
            Tool::new(
                "read_file",
                "Shadowed read_file.",
                json!({"type":"object","properties":{}}),
                |_| Ok(json!({"ok": true})),
            ),
            Tool::new(
                "user_tell",
                "Send progress to the user.",
                json!({"type":"object","properties":{}}),
                |_| Ok(json!({"ok": true})),
            ),
        ];

        let plan = plan_agent_host_tool_injection(&installation, &extra_tools).unwrap();
        assert_eq!(
            plan.shadowed_native_tool_names,
            vec!["read_file".to_string()]
        );
        assert_eq!(plan.forwarded_tools.len(), 1);
        assert_eq!(plan.forwarded_tools[0].name, "user_tell");
    }
}
