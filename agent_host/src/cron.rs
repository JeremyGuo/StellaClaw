use crate::backend::AgentBackendKind;
use crate::domain::ChannelAddress;
use crate::sink::SinkTarget;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronCheckerConfig {
    pub command: String,
    pub timeout_seconds: f64,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronTaskRecord {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub schedule: String,
    #[serde(default = "default_cron_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub agent_backend: AgentBackendKind,
    pub model_key: String,
    pub prompt: String,
    pub sink: SinkTarget,
    pub address: ChannelAddress,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub checker: Option<CronCheckerConfig>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_scheduled_for: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_check_outcome: Option<String>,
    #[serde(default)]
    pub last_triggered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_trigger_outcome: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronTaskView {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub schedule: String,
    pub timezone: String,
    #[serde(default)]
    pub agent_backend: AgentBackendKind,
    pub model_key: String,
    pub enabled: bool,
    #[serde(default)]
    pub checker: Option<CronCheckerConfig>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_scheduled_for: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_check_outcome: Option<String>,
    #[serde(default)]
    pub last_triggered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_trigger_outcome: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CronCreateRequest {
    pub name: String,
    pub description: String,
    pub schedule: String,
    pub timezone: String,
    pub agent_backend: AgentBackendKind,
    pub model_key: String,
    pub prompt: String,
    pub sink: SinkTarget,
    pub address: ChannelAddress,
    pub enabled: bool,
    pub checker: Option<CronCheckerConfig>,
}

#[derive(Clone, Debug, Default)]
pub struct CronUpdateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub agent_backend: Option<AgentBackendKind>,
    pub model_key: Option<String>,
    pub prompt: Option<String>,
    pub sink: Option<SinkTarget>,
    pub enabled: Option<bool>,
    pub checker: Option<Option<CronCheckerConfig>>,
}

#[derive(Clone, Debug)]
pub struct ClaimedCronTask {
    pub task: CronTaskRecord,
    pub scheduled_for: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct CronStoreFile {
    #[serde(default)]
    tasks: Vec<CronTaskRecord>,
}

pub struct CronManager {
    store_path: PathBuf,
    tasks: BTreeMap<Uuid, CronTaskRecord>,
}

impl CronManager {
    pub fn load_or_create(workdir: impl AsRef<Path>) -> Result<Self> {
        let cron_dir = workdir.as_ref().join("cron");
        fs::create_dir_all(&cron_dir)
            .with_context(|| format!("failed to create {}", cron_dir.display()))?;
        let store_path = cron_dir.join("tasks.json");
        if !store_path.exists() {
            let manager = Self {
                store_path,
                tasks: BTreeMap::new(),
            };
            manager.persist()?;
            return Ok(manager);
        }

        let raw = fs::read_to_string(&store_path)
            .with_context(|| format!("failed to read {}", store_path.display()))?;
        let file: CronStoreFile =
            serde_json::from_str(&raw).context("failed to parse cron task store")?;
        let mut tasks = BTreeMap::new();
        let mut normalized_stale_running = false;
        let now = Utc::now();
        for mut task in file.tasks {
            validate_task(&task)?;
            if trigger_outcome_is_running(task.last_trigger_outcome.as_deref()) {
                task.last_trigger_outcome = Some("interrupted_by_restart".to_string());
                task.updated_at = now;
                normalized_stale_running = true;
            }
            tasks.insert(task.id, task);
        }

        let manager = Self { store_path, tasks };
        if normalized_stale_running {
            manager.persist()?;
        }
        Ok(manager)
    }

    pub fn list(&self) -> Result<Vec<CronTaskView>> {
        self.tasks.values().map(task_view).collect()
    }

    pub fn get(&self, id: Uuid) -> Result<CronTaskView> {
        let task = self
            .tasks
            .get(&id)
            .ok_or_else(|| anyhow!("cron task {} not found", id))?;
        task_view(task)
    }

    pub fn create(&mut self, request: CronCreateRequest) -> Result<CronTaskView> {
        validate_schedule(&request.schedule)?;
        let timezone = normalize_timezone(&request.timezone)?;
        validate_name(&request.name)?;
        validate_description(&request.description)?;
        if request.prompt.trim().is_empty() {
            return Err(anyhow!("cron task prompt must not be empty"));
        }
        if let Some(checker) = &request.checker {
            validate_checker(checker)?;
        }

        let now = Utc::now();
        let task = CronTaskRecord {
            id: Uuid::new_v4(),
            name: request.name.trim().to_string(),
            description: request.description.trim().to_string(),
            schedule: request.schedule.trim().to_string(),
            timezone,
            agent_backend: request.agent_backend,
            model_key: request.model_key,
            prompt: request.prompt,
            sink: request.sink,
            address: request.address,
            enabled: request.enabled,
            checker: request.checker,
            created_at: now,
            updated_at: now,
            last_scheduled_for: None,
            last_checked_at: None,
            last_check_outcome: None,
            last_triggered_at: None,
            last_trigger_outcome: None,
        };
        let view = task_view(&task)?;
        self.tasks.insert(task.id, task);
        self.persist()?;
        Ok(view)
    }

    pub fn update(&mut self, id: Uuid, request: CronUpdateRequest) -> Result<CronTaskView> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| anyhow!("cron task {} not found", id))?;

        if let Some(name) = request.name {
            validate_name(&name)?;
            task.name = name.trim().to_string();
        }
        if let Some(description) = request.description {
            validate_description(&description)?;
            task.description = description.trim().to_string();
        }
        if let Some(schedule) = request.schedule {
            validate_schedule(&schedule)?;
            task.schedule = schedule.trim().to_string();
            task.last_scheduled_for = None;
        }
        if let Some(timezone) = request.timezone {
            task.timezone = normalize_timezone(&timezone)?;
            task.last_scheduled_for = None;
        }
        if let Some(agent_backend) = request.agent_backend {
            task.agent_backend = agent_backend;
        }
        if let Some(model_key) = request.model_key {
            task.model_key = model_key;
        }
        if let Some(prompt) = request.prompt {
            if prompt.trim().is_empty() {
                return Err(anyhow!("cron task prompt must not be empty"));
            }
            task.prompt = prompt;
        }
        if let Some(sink) = request.sink {
            task.sink = sink;
        }
        if let Some(enabled) = request.enabled {
            task.enabled = enabled;
        }
        if let Some(checker) = request.checker {
            if let Some(checker) = &checker {
                validate_checker(checker)?;
            }
            task.checker = checker;
        }
        task.updated_at = Utc::now();
        let view = task_view(task)?;
        self.persist()?;
        Ok(view)
    }

    pub fn disable_for_address(&mut self, address: &ChannelAddress) -> Result<usize> {
        let mut disabled = 0usize;
        for task in self.tasks.values_mut() {
            if task.address.session_key() == address.session_key() && task.enabled {
                task.enabled = false;
                task.updated_at = Utc::now();
                task.last_trigger_outcome = Some("conversation closed; auto-disabled".to_string());
                disabled += 1;
            }
        }
        if disabled > 0 {
            self.persist()?;
        }
        Ok(disabled)
    }

    pub fn remove(&mut self, id: Uuid) -> Result<CronTaskView> {
        let task = self
            .tasks
            .remove(&id)
            .ok_or_else(|| anyhow!("cron task {} not found", id))?;
        let view = task_view(&task)?;
        self.persist()?;
        Ok(view)
    }

    pub fn claim_due_tasks(
        &mut self,
        window_start: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<ClaimedCronTask>> {
        let mut claimed = Vec::new();
        let mut touched = false;
        for task in self.tasks.values_mut() {
            if !task.enabled {
                continue;
            }
            let base = task
                .last_scheduled_for
                .unwrap_or_else(|| task.created_at - chrono::Duration::seconds(1));
            let Some(mut next_run) = next_run_after(&task.schedule, &task.timezone, base)
                .with_context(|| format!("invalid cron schedule for task {}", task.id))?
            else {
                continue;
            };
            if next_run <= window_start {
                next_run = match next_run_after(&task.schedule, &task.timezone, window_start)
                    .with_context(|| format!("invalid cron schedule for task {}", task.id))?
                {
                    Some(next_run) => next_run,
                    None => {
                        task.last_scheduled_for = Some(window_start);
                        task.updated_at = now;
                        touched = true;
                        continue;
                    }
                };
                task.last_scheduled_for = Some(window_start);
                task.updated_at = now;
                touched = true;
            }
            if next_run <= now {
                task.last_scheduled_for = Some(next_run);
                task.updated_at = now;
                touched = true;
                if trigger_outcome_is_running(task.last_trigger_outcome.as_deref()) {
                    continue;
                }
                claimed.push(ClaimedCronTask {
                    task: task.clone(),
                    scheduled_for: next_run,
                });
            }
        }
        if touched {
            self.persist()?;
        }
        Ok(claimed)
    }

    pub fn record_check_result(
        &mut self,
        id: Uuid,
        checked_at: DateTime<Utc>,
        outcome: String,
    ) -> Result<()> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| anyhow!("cron task {} not found", id))?;
        task.last_checked_at = Some(checked_at);
        task.last_check_outcome = Some(outcome);
        task.updated_at = checked_at;
        self.persist()
    }

    pub fn record_trigger_result(
        &mut self,
        id: Uuid,
        triggered_at: DateTime<Utc>,
        outcome: String,
    ) -> Result<()> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| anyhow!("cron task {} not found", id))?;
        task.last_triggered_at = Some(triggered_at);
        task.last_trigger_outcome = Some(outcome);
        task.updated_at = triggered_at;
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let file = CronStoreFile {
            tasks: self.tasks.values().cloned().collect(),
        };
        let raw = serde_json::to_string_pretty(&file).context("failed to serialize cron store")?;
        fs::write(&self.store_path, raw)
            .with_context(|| format!("failed to write {}", self.store_path.display()))
    }
}

pub fn running_trigger_outcome(agent_id: Uuid) -> String {
    format!("running:{agent_id}")
}

fn trigger_outcome_is_running(outcome: Option<&str>) -> bool {
    outcome.is_some_and(|value| value.starts_with("running:"))
}

fn task_view(task: &CronTaskRecord) -> Result<CronTaskView> {
    Ok(CronTaskView {
        id: task.id,
        name: task.name.clone(),
        description: task.description.clone(),
        schedule: task.schedule.clone(),
        timezone: task.timezone.clone(),
        agent_backend: task.agent_backend,
        model_key: task.model_key.clone(),
        enabled: task.enabled,
        checker: task.checker.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        next_run_at: if task.enabled {
            next_run_at(task)?
        } else {
            None
        },
        last_scheduled_for: task.last_scheduled_for,
        last_checked_at: task.last_checked_at,
        last_check_outcome: task.last_check_outcome.clone(),
        last_triggered_at: task.last_triggered_at,
        last_trigger_outcome: task.last_trigger_outcome.clone(),
    })
}

fn next_run_at(task: &CronTaskRecord) -> Result<Option<DateTime<Utc>>> {
    let base = task
        .last_scheduled_for
        .unwrap_or_else(|| task.created_at - chrono::Duration::seconds(1));
    next_run_after(&task.schedule, &task.timezone, base)
        .with_context(|| format!("invalid cron schedule for task {}", task.id))
}

fn next_run_after(
    schedule: &str,
    timezone: &str,
    base: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let schedule = Schedule::from_str(schedule).context("invalid cron schedule")?;
    let timezone = parse_timezone(timezone)?;
    let local_base = base.with_timezone(&timezone);
    Ok(schedule
        .after(&local_base)
        .next()
        .map(|value| value.with_timezone(&Utc)))
}

fn validate_task(task: &CronTaskRecord) -> Result<()> {
    validate_name(&task.name)?;
    validate_description(&task.description)?;
    validate_schedule(&task.schedule)?;
    parse_timezone(&task.timezone)?;
    if let Some(checker) = &task.checker {
        validate_checker(checker)?;
    }
    Ok(())
}

pub fn default_cron_timezone() -> String {
    "Asia/Shanghai".to_string()
}

fn normalize_timezone(timezone: &str) -> Result<String> {
    let timezone = timezone.trim();
    if timezone.is_empty() {
        return Ok(default_cron_timezone());
    }
    parse_timezone(timezone)?;
    Ok(timezone.to_string())
}

fn parse_timezone(timezone: &str) -> Result<Tz> {
    timezone
        .trim()
        .parse::<Tz>()
        .with_context(|| format!("invalid IANA timezone '{}'", timezone.trim()))
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(anyhow!("cron task name must not be empty"));
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<()> {
    if description.trim().is_empty() {
        return Err(anyhow!("cron task description must not be empty"));
    }
    Ok(())
}

fn validate_schedule(schedule: &str) -> Result<()> {
    Schedule::from_str(schedule.trim()).context("invalid cron schedule")?;
    Ok(())
}

fn validate_checker(checker: &CronCheckerConfig) -> Result<()> {
    if checker.command.trim().is_empty() {
        return Err(anyhow!("checker command must not be empty"));
    }
    if checker.timeout_seconds <= 0.0 {
        return Err(anyhow!("checker timeout_seconds must be positive"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CronCreateRequest, CronManager, default_cron_timezone, running_trigger_outcome};
    use crate::backend::AgentBackendKind;
    use crate::domain::ChannelAddress;
    use crate::sink::SinkTarget;
    use chrono::{Datelike, TimeZone, Timelike};
    use chrono_tz::Asia::Shanghai;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn cron_manager_roundtrip_and_claim() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let task = manager
            .create(CronCreateRequest {
                name: "heartbeat".to_string(),
                description: "send a heartbeat".to_string(),
                schedule: "*/5 * * * * * *".to_string(),
                timezone: default_cron_timezone(),
                agent_backend: AgentBackendKind::AgentFrame,
                model_key: "main".to_string(),
                prompt: "ping".to_string(),
                sink: SinkTarget::Direct(ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                }),
                address: ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                },
                enabled: true,
                checker: None,
            })
            .unwrap();
        assert_eq!(manager.list().unwrap().len(), 1);
        let first_due_at = manager.get(task.id).unwrap().next_run_at.unwrap();
        let due = manager
            .claim_due_tasks(first_due_at - chrono::Duration::seconds(1), first_due_at)
            .unwrap();
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn cron_manager_skips_due_task_while_previous_trigger_is_running() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let task = manager
            .create(CronCreateRequest {
                name: "progress".to_string(),
                description: "send progress".to_string(),
                schedule: "0 * * * * *".to_string(),
                timezone: default_cron_timezone(),
                agent_backend: AgentBackendKind::AgentFrame,
                model_key: "main".to_string(),
                prompt: "ping".to_string(),
                sink: SinkTarget::Direct(ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                }),
                address: ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                },
                enabled: true,
                checker: None,
            })
            .unwrap();

        let first_due_at = manager.get(task.id).unwrap().next_run_at.unwrap();
        let due = manager
            .claim_due_tasks(first_due_at - chrono::Duration::seconds(1), first_due_at)
            .unwrap();
        assert_eq!(due.len(), 1);
        manager
            .record_trigger_result(
                task.id,
                first_due_at,
                running_trigger_outcome(Uuid::new_v4()),
            )
            .unwrap();

        let skipped = manager
            .claim_due_tasks(
                first_due_at + chrono::Duration::minutes(1),
                first_due_at + chrono::Duration::minutes(2),
            )
            .unwrap();
        assert!(skipped.is_empty());
        let skipped_again = manager
            .claim_due_tasks(
                first_due_at + chrono::Duration::minutes(1),
                first_due_at + chrono::Duration::minutes(2),
            )
            .unwrap();
        assert!(skipped_again.is_empty());

        manager
            .record_trigger_result(task.id, first_due_at, "completed".to_string())
            .unwrap();
        let next_due = manager
            .claim_due_tasks(
                first_due_at + chrono::Duration::minutes(1),
                first_due_at + chrono::Duration::minutes(2),
            )
            .unwrap();
        assert!(next_due.is_empty());
        let future_due = manager
            .claim_due_tasks(
                first_due_at + chrono::Duration::minutes(2),
                first_due_at + chrono::Duration::minutes(3),
            )
            .unwrap();
        assert_eq!(future_due.len(), 1);
    }

    #[test]
    fn cron_manager_clears_running_trigger_after_restart_without_retrying_missed_slot() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let task = manager
            .create(CronCreateRequest {
                name: "progress".to_string(),
                description: "send progress".to_string(),
                schedule: "0 * * * * *".to_string(),
                timezone: default_cron_timezone(),
                agent_backend: AgentBackendKind::AgentFrame,
                model_key: "main".to_string(),
                prompt: "ping".to_string(),
                sink: SinkTarget::Direct(ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                }),
                address: ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                },
                enabled: true,
                checker: None,
            })
            .unwrap();

        let first_due_at = manager.get(task.id).unwrap().next_run_at.unwrap();
        {
            let stored = manager.tasks.get_mut(&task.id).unwrap();
            stored.last_scheduled_for = Some(first_due_at);
            stored.last_triggered_at = Some(first_due_at);
            stored.last_trigger_outcome = Some(running_trigger_outcome(Uuid::new_v4()));
        }
        manager.persist().unwrap();

        let mut restored = CronManager::load_or_create(temp_dir.path()).unwrap();
        assert_eq!(
            restored
                .get(task.id)
                .unwrap()
                .last_trigger_outcome
                .as_deref(),
            Some("interrupted_by_restart")
        );
        let due = restored
            .claim_due_tasks(
                first_due_at + chrono::Duration::minutes(1),
                first_due_at + chrono::Duration::minutes(2),
            )
            .unwrap();
        assert_eq!(due.len(), 1);
        assert!(due[0].scheduled_for > first_due_at + chrono::Duration::minutes(1));
    }

    #[test]
    fn cron_manager_skips_runs_missed_before_current_poll_window() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let created_local = Shanghai
            .with_ymd_and_hms(2026, 4, 21, 9, 0, 0)
            .single()
            .expect("test timezone should be valid");
        let scheduled_local = Shanghai
            .with_ymd_and_hms(2026, 4, 22, 8, 0, 0)
            .single()
            .expect("test timezone should be valid");
        let startup_local = Shanghai
            .with_ymd_and_hms(2026, 4, 22, 14, 2, 29)
            .single()
            .expect("test timezone should be valid");
        let poll_local = Shanghai
            .with_ymd_and_hms(2026, 4, 22, 14, 7, 4)
            .single()
            .expect("test timezone should be valid");
        let next_local = Shanghai
            .with_ymd_and_hms(2026, 4, 23, 8, 0, 0)
            .single()
            .expect("test timezone should be valid");
        let task = manager
            .create(CronCreateRequest {
                name: "daily-morning-news".to_string(),
                description: "send a morning digest".to_string(),
                schedule: "0 0 8 * * *".to_string(),
                timezone: default_cron_timezone(),
                agent_backend: AgentBackendKind::AgentFrame,
                model_key: "main".to_string(),
                prompt: "ping".to_string(),
                sink: SinkTarget::Direct(ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                }),
                address: ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                },
                enabled: true,
                checker: None,
            })
            .unwrap();
        let created_at = created_local.with_timezone(&chrono::Utc);
        let scheduled_at = scheduled_local.with_timezone(&chrono::Utc);
        let startup_at = startup_local.with_timezone(&chrono::Utc);
        let poll_at = poll_local.with_timezone(&chrono::Utc);
        let next_at = next_local.with_timezone(&chrono::Utc);
        {
            let task = manager.tasks.get_mut(&task.id).unwrap();
            task.created_at = created_at;
            task.updated_at = created_at;
            task.last_scheduled_for = None;
        }

        let due = manager.claim_due_tasks(startup_at, poll_at).unwrap();
        assert!(due.is_empty());
        assert_eq!(manager.get(task.id).unwrap().next_run_at, Some(next_at));
        assert_eq!(
            manager.get(task.id).unwrap().last_scheduled_for,
            Some(startup_at)
        );
        assert!(scheduled_at < startup_at);
    }

    #[test]
    fn exact_time_cron_schedule_uses_task_timezone() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let created_local = Shanghai
            .with_ymd_and_hms(2026, 4, 17, 13, 5, 35)
            .single()
            .expect("test timezone should be valid");
        let target_local = Shanghai
            .with_ymd_and_hms(2026, 4, 17, 13, 7, 0)
            .single()
            .expect("test timezone should be valid");
        let task = manager
            .create(CronCreateRequest {
                name: "local reminder".to_string(),
                description: "send a local-time reminder".to_string(),
                schedule: format!(
                    "{} {} {} {} {} *",
                    target_local.second(),
                    target_local.minute(),
                    target_local.hour(),
                    target_local.day(),
                    target_local.month()
                ),
                timezone: default_cron_timezone(),
                agent_backend: AgentBackendKind::AgentFrame,
                model_key: "main".to_string(),
                prompt: "ping".to_string(),
                sink: SinkTarget::Direct(ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                }),
                address: ChannelAddress {
                    channel_id: "telegram-main".to_string(),
                    conversation_id: "123".to_string(),
                    user_id: None,
                    display_name: None,
                },
                enabled: true,
                checker: None,
            })
            .unwrap();
        let created_at = created_local.with_timezone(&chrono::Utc);
        let target_at = target_local.with_timezone(&chrono::Utc);
        {
            let task = manager.tasks.get_mut(&task.id).unwrap();
            task.created_at = created_at;
            task.updated_at = created_at;
            task.last_scheduled_for = None;
        }

        assert_eq!(manager.get(task.id).unwrap().timezone, "Asia/Shanghai");
        assert_eq!(manager.get(task.id).unwrap().next_run_at, Some(target_at));
        assert!(
            manager
                .claim_due_tasks(
                    target_at - chrono::Duration::seconds(2),
                    target_at - chrono::Duration::seconds(1),
                )
                .unwrap()
                .is_empty()
        );
        let due = manager
            .claim_due_tasks(target_at - chrono::Duration::seconds(1), target_at)
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].scheduled_for, target_at);
    }
}
