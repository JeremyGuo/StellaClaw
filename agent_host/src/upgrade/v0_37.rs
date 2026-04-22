use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.36"
    }

    fn to_version(&self) -> &'static str {
        "0.37"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        rewrite_jsonl_logs(&workdir.join("logs").join("agents"))?;
        rewrite_jsonl_logs(&workdir.join("logs").join("api"))?;
        Ok(())
    }
}

fn rewrite_jsonl_logs(root: &Path) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            rewrite_jsonl_logs(&path)?;
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let ended_with_newline = raw.ends_with('\n');
        let mut changed = false;
        let mut rewritten = Vec::new();
        for line in raw.lines() {
            let Ok(mut value) = serde_json::from_str::<Value>(line) else {
                rewritten.push(line.to_string());
                continue;
            };
            if rewrite_log_event(&mut value) {
                changed = true;
                rewritten.push(serde_json::to_string(&value)?);
            } else {
                rewritten.push(line.to_string());
            }
        }

        if !changed {
            continue;
        }

        let mut output = rewritten.join("\n");
        if ended_with_newline {
            output.push('\n');
        }
        fs::write(&path, output).with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn rewrite_log_event(value: &mut Value) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    let Some(kind) = object.get("kind").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(
        kind,
        "turn_token_usage"
            | "agent_frame_model_call_completed"
            | "upstream_api_request_completed"
            | "idle_context_compaction_completed"
            | "idle_context_compaction_retry_completed"
    ) {
        return false;
    }
    normalize_token_usage_fields(object)
}

fn normalize_token_usage_fields(object: &mut Map<String, Value>) -> bool {
    let input_total_tokens = first_u64_in_map(
        object,
        &[
            "input_total_tokens",
            "prompt_tokens",
            "legacy_prompt_tokens",
        ],
    );
    let output_total_tokens = first_u64_in_map(
        object,
        &[
            "output_total_tokens",
            "completion_tokens",
            "legacy_completion_tokens",
        ],
    );
    let context_total_tokens = first_u64_in_map(
        object,
        &[
            "context_total_tokens",
            "total_tokens",
            "legacy_total_tokens",
        ],
    )
    .or_else(|| {
        Some(
            input_total_tokens
                .unwrap_or(0)
                .saturating_add(output_total_tokens.unwrap_or(0)),
        )
    });
    let cache_read_input_tokens = first_u64_in_map(
        object,
        &[
            "cache_read_input_tokens",
            "cache_read_tokens",
            "legacy_cache_read_tokens",
        ],
    );
    let cache_write_input_tokens = first_u64_in_map(
        object,
        &[
            "cache_write_input_tokens",
            "cache_write_tokens",
            "legacy_cache_write_tokens",
        ],
    );
    let cache_uncached_input_tokens = first_u64_in_map(
        object,
        &[
            "cache_uncached_input_tokens",
            "cache_miss_tokens",
            "legacy_cache_miss_tokens",
        ],
    )
    .or_else(|| {
        Some(
            input_total_tokens
                .unwrap_or(0)
                .saturating_sub(cache_read_input_tokens.unwrap_or(0)),
        )
    });
    let normal_billed_input_tokens = first_u64_in_map(object, &["normal_billed_input_tokens"])
        .or_else(|| {
            Some(
                cache_uncached_input_tokens
                    .unwrap_or(0)
                    .saturating_sub(cache_write_input_tokens.unwrap_or(0)),
            )
        });

    let mut changed = false;
    changed |= upsert_u64_field(object, "input_total_tokens", input_total_tokens);
    changed |= upsert_u64_field(object, "output_total_tokens", output_total_tokens);
    changed |= upsert_u64_field(object, "context_total_tokens", context_total_tokens);
    changed |= upsert_u64_field(object, "cache_read_input_tokens", cache_read_input_tokens);
    changed |= upsert_u64_field(object, "cache_write_input_tokens", cache_write_input_tokens);
    changed |= upsert_u64_field(
        object,
        "cache_uncached_input_tokens",
        cache_uncached_input_tokens,
    );
    changed |= upsert_u64_field(
        object,
        "normal_billed_input_tokens",
        normal_billed_input_tokens,
    );

    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "cache_hit_tokens",
        "cache_miss_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "legacy_prompt_tokens",
        "legacy_completion_tokens",
        "legacy_total_tokens",
        "legacy_cache_hit_tokens",
        "legacy_cache_miss_tokens",
        "legacy_cache_read_tokens",
        "legacy_cache_write_tokens",
    ] {
        changed |= object.remove(key).is_some();
    }

    changed
}

fn upsert_u64_field(object: &mut Map<String, Value>, key: &str, value: Option<u64>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match object.get(key).and_then(Value::as_u64) {
        Some(existing) if existing == value => false,
        _ => {
            object.insert(key.to_string(), Value::from(value));
            true
        }
    }
}

fn first_u64_in_map(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
}
