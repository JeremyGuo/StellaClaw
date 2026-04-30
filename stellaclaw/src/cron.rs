use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTaskRecord {
    pub id: String,
    pub conversation_id: String,
    pub channel_id: String,
    pub platform_chat_id: String,
    pub name: String,
    pub description: String,
    pub schedule: String,
    pub timezone: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_timeout_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_cwd: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreateCronTaskRequest {
    pub conversation_id: String,
    pub channel_id: String,
    pub platform_chat_id: String,
    pub name: String,
    pub description: String,
    pub schedule: String,
    pub timezone: String,
    pub task: String,
    pub model: Option<String>,
    pub script_command: Option<String>,
    pub script_timeout_seconds: Option<f64>,
    pub script_cwd: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateCronTaskRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub task: Option<String>,
    pub model: Option<Option<String>>,
    pub script_command: Option<Option<String>>,
    pub script_timeout_seconds: Option<Option<f64>>,
    pub script_cwd: Option<Option<String>>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CronTaskSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub timezone: String,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CronStore {
    #[serde(default = "default_next_index")]
    next_index: u64,
    #[serde(default)]
    tasks: BTreeMap<String, CronTaskRecord>,
}

pub struct CronManager {
    path: PathBuf,
    store: Mutex<CronStore>,
}

impl CronManager {
    pub fn load_under(workdir: &Path) -> Result<Self> {
        let dir = workdir.join(".log").join("stellaclaw");
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let path = dir.join("cron_tasks.json");
        let store = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display()))?
        } else {
            CronStore::default()
        };
        let manager = Self {
            path,
            store: Mutex::new(store),
        };
        manager.recompute_all()?;
        Ok(manager)
    }

    pub fn list_for_conversation(&self, conversation_id: &str) -> Result<Vec<CronTaskSummary>> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let mut tasks = store
            .tasks
            .values()
            .filter(|task| task.conversation_id == conversation_id)
            .map(|task| CronTaskSummary {
                id: task.id.clone(),
                name: task.name.clone(),
                description: task.description.clone(),
                timezone: task.timezone.clone(),
                enabled: task.enabled,
                next_run_at: task.next_run_at.clone(),
                model: task.model.clone(),
            })
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(tasks)
    }

    pub fn get_for_conversation(
        &self,
        conversation_id: &str,
        id: &str,
    ) -> Result<Option<CronTaskRecord>> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        Ok(store
            .tasks
            .get(id)
            .filter(|task| task.conversation_id == conversation_id)
            .cloned())
    }

    pub fn create_task(&self, request: CreateCronTaskRequest) -> Result<CronTaskRecord> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let id = format!("cron_{:04}", store.next_index);
        store.next_index = store.next_index.saturating_add(1);
        let mut task = CronTaskRecord {
            id: id.clone(),
            conversation_id: request.conversation_id,
            channel_id: request.channel_id,
            platform_chat_id: request.platform_chat_id,
            name: request.name,
            description: request.description,
            schedule: request.schedule,
            timezone: request.timezone,
            task: request.task,
            model: request.model,
            script_command: request.script_command,
            script_timeout_seconds: request.script_timeout_seconds,
            script_cwd: request.script_cwd,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            last_error: None,
        };
        validate_task_mode(&task)?;
        refresh_next_run_at(&mut task, None)?;
        store.tasks.insert(id, task.clone());
        drop(store);
        self.save()?;
        Ok(task)
    }

    pub fn update_task(
        &self,
        conversation_id: &str,
        id: &str,
        update: UpdateCronTaskRequest,
    ) -> Result<CronTaskRecord> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let task = store
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("unknown cron task {id}"))?;
        if task.conversation_id != conversation_id {
            return Err(anyhow!(
                "cron task {id} does not belong to this conversation"
            ));
        }
        if let Some(name) = update.name {
            task.name = name;
        }
        if let Some(description) = update.description {
            task.description = description;
        }
        if let Some(schedule) = update.schedule {
            task.schedule = schedule;
        }
        if let Some(timezone) = update.timezone {
            task.timezone = timezone;
        }
        if let Some(task_text) = update.task {
            task.task = task_text;
            if !task.task.trim().is_empty() {
                task.script_command = None;
                task.script_timeout_seconds = None;
                task.script_cwd = None;
            }
        }
        if let Some(model) = update.model {
            task.model = model;
        }
        if let Some(script_command) = update.script_command {
            task.script_command = script_command;
            if task.script_command.is_some() {
                task.task.clear();
            } else {
                task.script_timeout_seconds = None;
                task.script_cwd = None;
            }
        }
        if let Some(script_timeout_seconds) = update.script_timeout_seconds {
            task.script_timeout_seconds = script_timeout_seconds;
        }
        if let Some(script_cwd) = update.script_cwd {
            task.script_cwd = script_cwd;
        }
        if let Some(enabled) = update.enabled {
            task.enabled = enabled;
        }
        validate_task_mode(task)?;
        refresh_next_run_at(task, None)?;
        let updated = task.clone();
        drop(store);
        self.save()?;
        Ok(updated)
    }

    pub fn disable_task(
        &self,
        conversation_id: &str,
        id: &str,
        reason: String,
    ) -> Result<CronTaskRecord> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let task = store
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("unknown cron task {id}"))?;
        if task.conversation_id != conversation_id {
            return Err(anyhow!(
                "cron task {id} does not belong to this conversation"
            ));
        }
        task.enabled = false;
        task.next_run_at = None;
        task.last_error = Some(reason);
        let updated = task.clone();
        drop(store);
        self.save()?;
        Ok(updated)
    }

    pub fn remove_task(&self, conversation_id: &str, id: &str) -> Result<Option<CronTaskRecord>> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let belongs = store
            .tasks
            .get(id)
            .map(|task| task.conversation_id == conversation_id)
            .unwrap_or(false);
        if !belongs {
            return Ok(None);
        }
        let removed = store.tasks.remove(id);
        drop(store);
        self.save()?;
        Ok(removed)
    }

    pub fn remove_tasks_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<CronTaskRecord>> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let ids = store
            .tasks
            .iter()
            .filter(|(_, task)| task.conversation_id == conversation_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut removed = Vec::new();
        for id in ids {
            if let Some(task) = store.tasks.remove(&id) {
                removed.push(task);
            }
        }
        drop(store);
        if !removed.is_empty() {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn collect_due_tasks(&self, now: DateTime<Utc>) -> Result<Vec<CronTaskRecord>> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let mut due = Vec::new();
        for task in store.tasks.values_mut() {
            if !task.enabled {
                continue;
            }
            let Some(next_run_at) = parse_timestamp(task.next_run_at.as_deref()) else {
                continue;
            };
            if next_run_at > now {
                continue;
            }
            task.last_run_at = Some(now.to_rfc3339());
            refresh_next_run_at(task, Some(now))?;
            due.push(task.clone());
        }
        if !due.is_empty() {
            drop(store);
            self.save()?;
        }
        Ok(due)
    }

    fn recompute_all(&self) -> Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        for task in store.tasks.values_mut() {
            refresh_next_run_at(task, None)?;
        }
        drop(store);
        self.save()
    }

    fn save(&self) -> Result<()> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("cron store lock poisoned"))?;
        let raw =
            serde_json::to_string_pretty(&*store).context("failed to serialize cron store")?;
        fs::write(&self.path, raw)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

pub fn cron_schedule_from_required_tool_args(
    arguments: &serde_json::Map<String, Value>,
) -> Result<String> {
    build_cron_schedule_from_tool_args(arguments, true)
        .and_then(|schedule| schedule.ok_or_else(|| anyhow!("cron schedule is required")))
}

pub fn optional_cron_schedule_from_tool_args(
    arguments: &serde_json::Map<String, Value>,
) -> Result<Option<String>> {
    build_cron_schedule_from_tool_args(arguments, false)
}

fn build_cron_schedule_from_tool_args(
    arguments: &serde_json::Map<String, Value>,
    required: bool,
) -> Result<Option<String>> {
    let required_fields = [
        "cron_second",
        "cron_minute",
        "cron_hour",
        "cron_day_of_month",
        "cron_month",
        "cron_day_of_week",
    ];
    let cron_year = optional_string_arg(arguments, "cron_year")?;
    let required_present = required_fields
        .iter()
        .filter(|key| arguments.contains_key(**key))
        .count();
    if required_present == 0 {
        if cron_year.is_some() {
            return Err(anyhow!(
                "cron schedule updates must include all required named fields together"
            ));
        }
        return Ok(None);
    }
    if required && required_present != required_fields.len() {
        return Err(anyhow!("missing required cron schedule fields"));
    }
    if !required && required_present != 0 && required_present != required_fields.len() {
        return Err(anyhow!(
            "cron schedule updates must include all required named fields together"
        ));
    }

    let mut parts = Vec::new();
    for key in &required_fields {
        parts.push(string_arg_required(arguments, key)?);
    }
    if let Some(cron_year) = cron_year {
        parts.push(cron_year);
    }
    let schedule = parts.join(" ");
    validate_schedule(&schedule)?;
    Ok(Some(schedule))
}

pub fn string_arg_required(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{key} must be a non-empty string"))
}

pub fn optional_string_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    match arguments.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else {
                Ok(Some(value.to_string()))
            }
        }
        Some(_) => Err(anyhow!("{key} must be a string")),
        None => Ok(None),
    }
}

pub fn timezone_or_default(raw: Option<String>) -> Result<String> {
    let timezone = raw.unwrap_or_else(|| "Asia/Shanghai".to_string());
    validate_timezone(&timezone)?;
    Ok(timezone)
}

pub fn parse_enabled_flag(arguments: &serde_json::Map<String, Value>) -> Result<Option<bool>> {
    match arguments.get("enabled") {
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| anyhow!("enabled must be a boolean")),
        None => Ok(None),
    }
}

pub fn optional_positive_f64_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<f64>> {
    match arguments.get(key) {
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| anyhow!("{key} must be a number"))?;
            if !number.is_finite() || number <= 0.0 {
                return Err(anyhow!("{key} must be a positive finite number"));
            }
            Ok(Some(number))
        }
        None => Ok(None),
    }
}

fn refresh_next_run_at(task: &mut CronTaskRecord, from: Option<DateTime<Utc>>) -> Result<()> {
    if !task.enabled {
        task.next_run_at = None;
        return Ok(());
    }
    let timezone: Tz = validate_timezone(&task.timezone)?;
    let schedule = validate_schedule(&task.schedule)?;
    let base_utc = from.unwrap_or_else(Utc::now);
    let base_local = base_utc.with_timezone(&timezone);
    let next = schedule
        .after(&base_local)
        .next()
        .map(|next| next.with_timezone(&Utc));
    task.next_run_at = next.map(|value| value.to_rfc3339());
    Ok(())
}

fn validate_task_mode(task: &CronTaskRecord) -> Result<()> {
    let has_prompt = !task.task.trim().is_empty();
    let has_script = task
        .script_command
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    match (has_prompt, has_script) {
        (true, false) | (false, true) => Ok(()),
        (true, true) => Err(anyhow!(
            "cron task must set either task prompt or script_command, not both"
        )),
        (false, false) => Err(anyhow!(
            "cron task must set either task prompt or script_command"
        )),
    }
}

fn validate_schedule(schedule: &str) -> Result<Schedule> {
    schedule
        .parse::<Schedule>()
        .with_context(|| format!("invalid cron schedule '{schedule}'"))
}

fn validate_timezone(timezone: &str) -> Result<Tz> {
    timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid timezone '{timezone}'"))
}

fn parse_timestamp(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn default_true() -> bool {
    true
}

fn default_next_index() -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workdir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "stellaclaw_cron_test_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ))
    }

    #[test]
    fn optional_string_arg_treats_empty_as_absent() {
        let object = json!({
            "missing_prompt": "",
            "blank_prompt": "   ",
            "null_prompt": null,
            "script_command": "  python3 script.py  "
        });
        let object = object
            .as_object()
            .expect("test payload should be an object");

        assert_eq!(
            optional_string_arg(object, "missing_prompt").expect("empty should parse"),
            None
        );
        assert_eq!(
            optional_string_arg(object, "blank_prompt").expect("blank should parse"),
            None
        );
        assert_eq!(
            optional_string_arg(object, "null_prompt").expect("null should parse"),
            None
        );
        assert_eq!(
            optional_string_arg(object, "script_command").expect("script should parse"),
            Some("python3 script.py".to_string())
        );
    }

    #[test]
    fn cron_year_empty_is_ignored() {
        let object = json!({
            "cron_second": "0",
            "cron_minute": "30",
            "cron_hour": "8",
            "cron_day_of_month": "*",
            "cron_month": "*",
            "cron_day_of_week": "*",
            "cron_year": ""
        });
        let object = object
            .as_object()
            .expect("test payload should be an object");

        let schedule =
            cron_schedule_from_required_tool_args(object).expect("schedule should parse");
        assert_eq!(schedule, "0 30 8 * * *");
    }

    #[test]
    fn optional_schedule_rejects_year_without_required_fields() {
        let object = json!({
            "cron_year": "2026"
        });
        let object = object
            .as_object()
            .expect("test payload should be an object");

        let error = optional_cron_schedule_from_tool_args(object)
            .expect_err("year alone should not be a valid schedule update");
        assert!(format!("{error:#}")
            .contains("cron schedule updates must include all required named fields together"));
    }

    #[test]
    fn creates_lists_and_collects_due_tasks() {
        let workdir = temp_workdir();
        fs::create_dir_all(&workdir).expect("temp workdir should exist");
        let manager = CronManager::load_under(&workdir).expect("manager should load");
        let task = manager
            .create_task(CreateCronTaskRequest {
                conversation_id: "telegram-main-000001".to_string(),
                channel_id: "telegram-main".to_string(),
                platform_chat_id: "123".to_string(),
                name: "daily".to_string(),
                description: "run a task".to_string(),
                schedule: "* * * * * *".to_string(),
                timezone: "Asia/Shanghai".to_string(),
                task: "".to_string(),
                model: Some("main".to_string()),
                script_command: Some("python3 script.py".to_string()),
                script_timeout_seconds: Some(3.0),
                script_cwd: Some("checks".to_string()),
            })
            .expect("task should create");
        let stored = manager
            .get_for_conversation("telegram-main-000001", &task.id)
            .expect("task should load")
            .expect("task should exist");
        assert_eq!(stored.script_command.as_deref(), Some("python3 script.py"));
        assert_eq!(stored.script_timeout_seconds, Some(3.0));
        assert_eq!(stored.script_cwd.as_deref(), Some("checks"));
        assert!(stored.task.is_empty());

        let listed = manager
            .list_for_conversation("telegram-main-000001")
            .expect("tasks should list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, task.id);
        assert!(listed[0].next_run_at.is_some());

        manager
            .update_task(
                "telegram-main-000001",
                &task.id,
                UpdateCronTaskRequest {
                    enabled: Some(false),
                    task: Some("check status".to_string()),
                    ..UpdateCronTaskRequest::default()
                },
            )
            .expect("task should update");
        let cleared = manager
            .get_for_conversation("telegram-main-000001", &task.id)
            .expect("task should load")
            .expect("task should exist");
        assert!(cleared.script_command.is_none());
        assert!(cleared.script_timeout_seconds.is_none());
        assert!(cleared.script_cwd.is_none());
        assert_eq!(cleared.task, "check status");
        assert!(manager
            .collect_due_tasks(Utc::now() + chrono::Duration::minutes(1))
            .expect("collect should work")
            .is_empty());

        fs::remove_dir_all(&workdir).expect("temp workdir should be removed");
    }
}
