use crate::backend::AgentBackendKind;
use crate::domain::ChannelAddress;
use crate::sink::SinkTarget;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
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
        for task in file.tasks {
            validate_task(&task)?;
            tasks.insert(task.id, task);
        }

        Ok(Self { store_path, tasks })
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

    pub fn claim_due_tasks(&mut self, now: DateTime<Utc>) -> Result<Vec<ClaimedCronTask>> {
        let mut claimed = Vec::new();
        for task in self.tasks.values_mut() {
            if !task.enabled {
                continue;
            }
            let schedule = Schedule::from_str(&task.schedule)
                .with_context(|| format!("invalid cron schedule for task {}", task.id))?;
            let base = task
                .last_scheduled_for
                .unwrap_or_else(|| task.created_at - chrono::Duration::seconds(1));
            let Some(next_run) = schedule.after(&base).next() else {
                continue;
            };
            let next_run = Utc.from_utc_datetime(&next_run.naive_utc());
            if next_run <= now {
                task.last_scheduled_for = Some(next_run);
                task.updated_at = now;
                claimed.push(ClaimedCronTask {
                    task: task.clone(),
                    scheduled_for: next_run,
                });
            }
        }
        if !claimed.is_empty() {
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

fn task_view(task: &CronTaskRecord) -> Result<CronTaskView> {
    Ok(CronTaskView {
        id: task.id,
        name: task.name.clone(),
        description: task.description.clone(),
        schedule: task.schedule.clone(),
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
    let schedule = Schedule::from_str(&task.schedule)
        .with_context(|| format!("invalid cron schedule for task {}", task.id))?;
    let base = task
        .last_scheduled_for
        .unwrap_or_else(|| task.created_at - chrono::Duration::seconds(1));
    Ok(schedule
        .after(&base)
        .next()
        .map(|value| Utc.from_utc_datetime(&value.naive_utc())))
}

fn validate_task(task: &CronTaskRecord) -> Result<()> {
    validate_name(&task.name)?;
    validate_description(&task.description)?;
    validate_schedule(&task.schedule)?;
    if let Some(checker) = &task.checker {
        validate_checker(checker)?;
    }
    Ok(())
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
    use super::{CronCreateRequest, CronManager};
    use crate::backend::AgentBackendKind;
    use crate::domain::ChannelAddress;
    use crate::sink::SinkTarget;
    use tempfile::TempDir;

    #[test]
    fn cron_manager_roundtrip_and_claim() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let task = manager
            .create(CronCreateRequest {
                name: "heartbeat".to_string(),
                description: "send a heartbeat".to_string(),
                schedule: "*/5 * * * * * *".to_string(),
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
        let due = manager
            .claim_due_tasks(task.created_at + chrono::Duration::seconds(5))
            .unwrap();
        assert_eq!(due.len(), 1);
    }
}
