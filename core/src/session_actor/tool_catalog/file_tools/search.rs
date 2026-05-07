use std::{collections::BTreeSet, fs, path::Path};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{
    resolve_local_path, string_arg, string_arg_with_default, ExecutionTarget, LocalToolError,
    ToolExecutionContext,
};

use super::common::{
    build_glob_matcher, collect_walk_paths, file_mtime_ms, relative_display_path, remote_file_tool,
    sort_search_matches, SearchMatch, SEARCH_MAX_RESULTS,
};

const GREP_MAX_CONTEXT_LINES: usize = 10;
const GREP_DEFAULT_TOTAL_MAX_MATCHES: usize = SEARCH_MAX_RESULTS;
const GREP_MAX_TOTAL_MATCHES: usize = 1000;
const GREP_MAX_MATCHES_PER_FILE: usize = 1000;
const GREP_MAX_LINE_CHARS: usize = 600;

pub(super) fn execute_search_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "grep" => grep(arguments, context)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn grep(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    match context.execution_target_for_path(arguments, &["path"])? {
        ExecutionTarget::Local => grep_local(arguments, context.workspace_root),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_file_tool("grep", arguments, &host, cwd.as_deref())
        }
    }
}

fn grep_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let pattern = string_arg(arguments, "pattern")?;
    let base_path = resolve_local_path(
        workspace_root,
        &string_arg_with_default(arguments, "path", ".")?,
    );
    if !base_path.exists() {
        return Err(LocalToolError::Io(format!(
            "{} does not exist",
            base_path.display()
        )));
    }

    let regex = Regex::new(&pattern).map_err(|error| {
        LocalToolError::InvalidArguments(format!("invalid regex pattern: {error}"))
    })?;
    let include = match arguments.get("include").and_then(Value::as_str) {
        Some(include) => Some(build_glob_matcher(include)?),
        None => None,
    };
    let exclude = match arguments.get("exclude").and_then(Value::as_str) {
        Some(exclude) => Some(build_glob_matcher(exclude)?),
        None => None,
    };
    let context_lines =
        bounded_usize_arg(arguments, "context_lines", 0, 0, GREP_MAX_CONTEXT_LINES)?;
    let max_matches_per_file = bounded_usize_arg(
        arguments,
        "max_matches_per_file",
        1,
        GREP_MAX_MATCHES_PER_FILE,
        GREP_MAX_MATCHES_PER_FILE,
    )?;
    let total_max_matches = bounded_usize_arg(
        arguments,
        "total_max_matches",
        1,
        GREP_DEFAULT_TOTAL_MAX_MATCHES,
        GREP_MAX_TOTAL_MATCHES,
    )?;
    let names_only = bool_arg(arguments, "names_only", false)?;

    let mut file_matches = Vec::new();
    let mut result_matches = Vec::new();
    let mut matched_files = BTreeSet::new();
    let mut total_matches = 0usize;
    for path in collect_walk_paths(&base_path, true)? {
        let relative = relative_display_path(&path, &base_path);
        if let Some(include) = &include {
            if !include.is_match(&relative) {
                continue;
            }
        }
        if let Some(exclude) = &exclude {
            if exclude.is_match(&relative) {
                continue;
            }
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        let mut file_match_count = 0usize;
        for (line_index, line) in lines.iter().enumerate() {
            let Some(match_item) = regex.find(line) else {
                continue;
            };
            if file_match_count == 0 {
                matched_files.insert(path.display().to_string());
                file_matches.push(SearchMatch {
                    path: path.display().to_string(),
                    mtime_ms: file_mtime_ms(&path),
                });
            }
            file_match_count += 1;
            total_matches += 1;
            if file_match_count > max_matches_per_file || result_matches.len() >= total_max_matches
            {
                continue;
            }
            if !names_only {
                result_matches.push(grep_match_json(
                    &path,
                    line_index,
                    match_item.start(),
                    line,
                    &lines,
                    context_lines,
                ));
            }
        }
    }
    sort_search_matches(&mut file_matches);
    let match_truncated = !names_only && total_matches > result_matches.len();
    let truncated = match_truncated || file_matches.len() > SEARCH_MAX_RESULTS;
    let filenames = file_matches
        .iter()
        .take(SEARCH_MAX_RESULTS)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let mut result = Map::new();
    result.insert("pattern".to_string(), Value::String(pattern));
    result.insert(
        "path".to_string(),
        Value::String(base_path.display().to_string()),
    );
    if let Some(include) = arguments.get("include").and_then(Value::as_str) {
        result.insert("include".to_string(), Value::String(include.to_string()));
    }
    if let Some(exclude) = arguments.get("exclude").and_then(Value::as_str) {
        result.insert("exclude".to_string(), Value::String(exclude.to_string()));
    }
    result.insert("context_lines".to_string(), Value::from(context_lines));
    result.insert("num_files".to_string(), Value::from(matched_files.len()));
    result.insert("num_matches".to_string(), Value::from(total_matches));
    result.insert("filenames".to_string(), json!(filenames));
    if truncated {
        result.insert("truncated".to_string(), Value::Bool(true));
    }
    if names_only {
        result.insert("names_only".to_string(), Value::Bool(true));
    } else {
        result.insert("matches".to_string(), Value::Array(result_matches));
    }
    Ok(Value::Object(result))
}

fn grep_match_json(
    path: &Path,
    line_index: usize,
    column: usize,
    line: &str,
    lines: &[&str],
    context_lines: usize,
) -> Value {
    let line_number = line_index + 1;
    let before_start = line_index.saturating_sub(context_lines);
    let before = (before_start..line_index)
        .map(|index| {
            json!({
                "line": index + 1,
                "text": bounded_line(lines[index]),
            })
        })
        .collect::<Vec<_>>();
    let after_end = (line_index + 1 + context_lines).min(lines.len());
    let after = ((line_index + 1)..after_end)
        .map(|index| {
            json!({
                "line": index + 1,
                "text": bounded_line(lines[index]),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "file": path.display().to_string(),
        "line": line_number,
        "column": column + 1,
        "text": bounded_line(line),
        "before": before,
        "after": after,
    })
}

fn bounded_line(line: &str) -> String {
    let mut output = String::new();
    for (index, ch) in line.chars().enumerate() {
        if index >= GREP_MAX_LINE_CHARS {
            output.push_str("... [truncated]");
            return output;
        }
        output.push(ch);
    }
    output
}

fn bounded_usize_arg(
    arguments: &Map<String, Value>,
    key: &str,
    min: usize,
    default: usize,
    max: usize,
) -> Result<usize, LocalToolError> {
    match arguments.get(key) {
        Some(value) => {
            let Some(raw) = value.as_u64() else {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{key} must be an integer"
                )));
            };
            let value = usize::try_from(raw).unwrap_or(usize::MAX);
            if value < min {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{key} must be greater than or equal to {min}"
                )));
            }
            Ok(value.min(max))
        }
        None => Ok(default),
    }
}

fn bool_arg(
    arguments: &Map<String, Value>,
    key: &str,
    default: bool,
) -> Result<bool, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| LocalToolError::InvalidArguments(format!("{key} must be a boolean"))),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::grep_local;
    use serde_json::{json, Map, Value};

    fn temp_workspace(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-grep-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("workspace should be created");
        root
    }

    #[test]
    fn grep_returns_line_matches_with_context() {
        let root = temp_workspace("context");
        std::fs::write(
            root.join("src/lib.rs"),
            "fn main() {\n    let process_id = \"sh_1\";\n    println!(\"done\");\n}\n",
        )
        .expect("test file should be written");

        let mut args = Map::new();
        args.insert("pattern".to_string(), json!("process_id"));
        args.insert("path".to_string(), json!("src"));
        args.insert("include".to_string(), json!("**/*.rs"));
        args.insert("context_lines".to_string(), json!(1));

        let result = grep_local(&args, &root).expect("grep should succeed");
        assert_eq!(result["num_files"], json!(1));
        assert_eq!(result["num_matches"], json!(1));
        let matches = result["matches"].as_array().expect("matches should exist");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line"], json!(2));
        assert_eq!(matches[0]["column"], json!(9));
        assert_eq!(matches[0]["before"].as_array().unwrap().len(), 1);
        assert_eq!(matches[0]["after"].as_array().unwrap().len(), 1);

        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }

    #[test]
    fn grep_accepts_zero_context_lines() {
        let root = temp_workspace("zero-context");
        std::fs::write(root.join("src/lib.rs"), "alpha\nneedle\nomega\n")
            .expect("test file should be written");

        let mut args = Map::new();
        args.insert("pattern".to_string(), json!("needle"));
        args.insert("path".to_string(), json!("src"));
        args.insert("context_lines".to_string(), json!(0));

        let result = grep_local(&args, &root).expect("grep should succeed");
        let matches = result["matches"].as_array().expect("matches should exist");
        assert_eq!(matches[0]["before"], Value::Array(Vec::new()));
        assert_eq!(matches[0]["after"], Value::Array(Vec::new()));

        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }

    #[test]
    fn grep_names_only_omits_match_payloads() {
        let root = temp_workspace("names-only");
        std::fs::write(root.join("src/lib.rs"), "needle\nneedle\n")
            .expect("test file should be written");

        let mut args = Map::new();
        args.insert("pattern".to_string(), json!("needle"));
        args.insert("path".to_string(), json!("src"));
        args.insert("names_only".to_string(), json!(true));

        let result = grep_local(&args, &root).expect("grep should succeed");
        assert_eq!(result["num_files"], json!(1));
        assert_eq!(result["num_matches"], json!(2));
        assert_eq!(result["names_only"], json!(true));
        assert!(result.get("matches").is_none());
        assert_eq!(result["filenames"].as_array().unwrap().len(), 1);

        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }
}
