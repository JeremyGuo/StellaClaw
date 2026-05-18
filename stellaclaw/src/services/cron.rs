#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use crossbeam_channel::select;
use serde::{Deserialize, Serialize};
use stellaclaw_core::session_actor::{ChatMessage, ChatRole};

use crate::{
    conversation_new::{
        ConversationService, ServiceAddr, ServiceCall, ServiceOutput, ServiceRunContext,
        ServiceScope, ServiceStatusUpdate, ServiceStopped,
    },
    service_protos::{
        agent_session::{
            self, AgentMessageOrigin, AgentSessionBinding, AgentSessionEvent, AgentSessionKind,
        },
        cron::{
            decode_request, encode_response, CronRequest, CronResponse, CronRunStatus,
            CronSchedule, CronTaskOutputPolicy, CronTaskPatch, CronTaskPayload,
            CronTaskRegistration, CronTaskStatus,
        },
        kernel::{self, decode_response as decode_kernel_response, KernelResponse},
    },
};

pub struct CronService;

impl CronService {
    pub fn new() -> Self {
        Self
    }
}

impl ConversationService for CronService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        let (mut tasks, mut next_run_id) = load_tasks(&ctx.storage, Instant::now())?;
        let mut pending_runs = HashMap::<ServiceAddr, PendingCronRun>::new();
        let mut pending_order = VecDeque::<ServiceAddr>::new();
        let mut active_runs = HashMap::<ServiceAddr, ActiveCronRun>::new();
        loop {
            let wakeup = next_wakeup(&tasks, Instant::now(), Utc::now());
            let timer_rx = wakeup
                .as_ref()
                .map(|wakeup| {
                    crossbeam_channel::after(wakeup.at.saturating_duration_since(Instant::now()))
                })
                .unwrap_or_else(crossbeam_channel::never);
            select! {
                recv(ctx.stop_rx) -> stop => {
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason: stop.ok().map(|stop| stop.reason),
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload.clone()) {
                        Ok(CronRequest::RegisterTask { mut task }) => {
                            if task.registered_by != call.source {
                                task.registered_by = call.source.clone();
                            }
                            if let Err(reason) = validate_task_registration(&task, &call.source) {
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "task_register_rejected".to_string(),
                                    detail: serde_json::json!({
                                        "task_id": &task.task_id,
                                        "registered_by": &task.registered_by,
                                        "channel_addr": &task.channel_addr,
                                        "foreground_session_addr": &task.foreground_session_addr,
                                        "reason": &reason,
                                    }),
                                }))?;
                                ctx.outbox.send(ServiceOutput::Call(reply(
                                    &ctx.addr,
                                    &call.source,
                                    encode_response(CronResponse::Rejected { reason })?,
                                    call.request_id.clone(),
                                )))?;
                                continue;
                            }
                            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                addr: ctx.addr.clone(),
                                label: "task_registered".to_string(),
                                detail: serde_json::json!({
                                    "task_id": &task.task_id,
                                    "registered_by": &task.registered_by,
                                    "channel_addr": &task.channel_addr,
                                    "foreground_session_addr": &task.foreground_session_addr,
                                }),
                            }))?;
                            let now = Instant::now();
                            tasks.insert(task.task_id.clone(), ScheduledCronTask::new(task, now));
                            persist_tasks(&ctx.storage, &tasks, next_run_id)?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(CronResponse::Accepted)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(CronRequest::UpdateTask { task_id, patch }) => {
                            match update_task(&mut tasks, &task_id, &call.source, patch, Instant::now()) {
                                Ok(task) => {
                                    persist_tasks(&ctx.storage, &tasks, next_run_id)?;
                                    ctx.outbox.send(ServiceOutput::Call(reply(
                                        &ctx.addr,
                                        &call.source,
                                        encode_response(CronResponse::Task { task: Some(task) })?,
                                        call.request_id.clone(),
                                    )))?;
                                }
                                Err(reason) => {
                                    ctx.outbox.send(ServiceOutput::Call(reply(
                                        &ctx.addr,
                                        &call.source,
                                        encode_response(CronResponse::Rejected { reason })?,
                                        call.request_id.clone(),
                                    )))?;
                                }
                            }
                        }
                        Ok(CronRequest::RemoveTask { task_id }) => {
                            match remove_task(&mut tasks, &task_id, &call.source) {
                                Ok(task) => {
                                    persist_tasks(&ctx.storage, &tasks, next_run_id)?;
                                    ctx.outbox.send(ServiceOutput::Call(reply(
                                        &ctx.addr,
                                        &call.source,
                                        encode_response(CronResponse::Task { task })?,
                                        call.request_id.clone(),
                                    )))?;
                                }
                                Err(reason) => {
                                    ctx.outbox.send(ServiceOutput::Call(reply(
                                        &ctx.addr,
                                        &call.source,
                                        encode_response(CronResponse::Rejected { reason })?,
                                        call.request_id.clone(),
                                    )))?;
                                }
                            }
                        }
                        Ok(CronRequest::DisableTasksForOwner { owner, reason }) => {
                            let disabled = disable_tasks_for_owner(&mut tasks, &owner);
                            persist_tasks(&ctx.storage, &tasks, next_run_id)?;
                            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                addr: ctx.addr.clone(),
                                label: "owner_tasks_disabled".to_string(),
                                detail: serde_json::json!({
                                    "owner": owner,
                                    "disabled": disabled,
                                    "reason": reason,
                                }),
                            }))?;
                            if !call.source.is_kernel() {
                                ctx.outbox.send(ServiceOutput::Call(reply(
                                    &ctx.addr,
                                    &call.source,
                                    encode_response(CronResponse::Accepted)?,
                                    call.request_id.clone(),
                                )))?;
                            }
                        }
                        Ok(CronRequest::ListTasks { owner }) => {
                            let mut task_list = tasks
                                .values()
                                .filter(|task| owner.as_ref().is_none_or(|owner| task.registration.registered_by == *owner))
                                .map(|task| task.registration.clone())
                                .collect::<Vec<_>>();
                            task_list.sort_by_key(|task| task.task_id.clone());
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(CronResponse::Tasks { tasks: task_list })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(CronRequest::GetTaskStatus { task_id, owner }) => {
                            let status = task_status(
                                &tasks,
                                &pending_runs,
                                &active_runs,
                                &task_id,
                                Instant::now(),
                                Utc::now(),
                            ).filter(|status| {
                                owner
                                    .as_ref()
                                    .is_none_or(|owner| status.registration.registered_by == *owner)
                            });
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(CronResponse::TaskStatus { status })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(CronRequest::TriggerTaskNow { task_id }) => {
                            let Some(task) = tasks.get(&task_id).map(|task| task.registration.clone()) else {
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "task_trigger_rejected".to_string(),
                                    detail: serde_json::json!({
                                        "task_id": task_id,
                                        "reason": "unknown task",
                                    }),
                                }))?;
                                ctx.outbox.send(ServiceOutput::Call(reply(
                                    &ctx.addr,
                                    &call.source,
                                    encode_response(CronResponse::Rejected {
                                        reason: "unknown task".to_string(),
                                    })?,
                                    call.request_id.clone(),
                                )))?;
                                continue;
                            };
                            if task.registered_by != call.source {
                                let reason = "task is not owned by caller".to_string();
                                ctx.outbox.send(ServiceOutput::Call(reply(
                                    &ctx.addr,
                                    &call.source,
                                    encode_response(CronResponse::Rejected { reason })?,
                                    call.request_id.clone(),
                                )))?;
                                continue;
                            }
                            if !task.enabled {
                                let reason = "task is disabled".to_string();
                                ctx.outbox.send(ServiceOutput::Call(reply(
                                    &ctx.addr,
                                    &call.source,
                                    encode_response(CronResponse::Rejected { reason })?,
                                    call.request_id.clone(),
                                )))?;
                                continue;
                            }
                            if let Some(background_addr) =
                                task_run_in_progress(&pending_runs, &active_runs, &task_id)
                            {
                                let reason = "task is already running".to_string();
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "task_trigger_rejected".to_string(),
                                    detail: serde_json::json!({
                                        "task_id": task_id,
                                        "background_addr": background_addr,
                                        "reason": &reason,
                                    }),
                                }))?;
                                ctx.outbox.send(ServiceOutput::Call(reply(
                                    &ctx.addr,
                                    &call.source,
                                    encode_response(CronResponse::Rejected { reason })?,
                                    call.request_id.clone(),
                                )))?;
                                continue;
                            }
                            let background_addr = trigger_task(
                                &ctx,
                                &mut pending_runs,
                                &mut pending_order,
                                &mut next_run_id,
                                task,
                                call.source.clone(),
                            )?;
                            mark_task_running(&mut tasks, &task_id, background_addr);
                            persist_tasks(&ctx.storage, &tasks, next_run_id)?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(CronResponse::Accepted)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(CronRequest::AgentSessionEvent { session_addr, event }) => {
                            handle_agent_session_event(
                                &ctx,
                                &mut tasks,
                                &mut active_runs,
                                next_run_id,
                                session_addr,
                                event,
                            )?;
                        }
                        Err(error) => {
                            match decode_kernel_response(call.payload) {
                                Ok(KernelResponse::AgentSessionCreated { addr }) => {
                                    if let Some(pending) = pending_runs.remove(&addr) {
                                        remove_pending_order(&mut pending_order, &addr);
                                        active_runs.insert(
                                            addr.clone(),
                                            ActiveCronRun {
                                                task_id: pending.task.task_id.clone(),
                                                triggered_by: pending.triggered_by.clone(),
                                                channel_addr: pending.task.channel_addr.clone(),
                                                foreground_session_addr: pending
                                                    .task
                                                    .foreground_session_addr
                                                    .clone(),
                                                output_policy: task_output_policy(&pending.task),
                                            },
                                        );
                                        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                            addr: ctx.addr.clone(),
                                            label: "task_background_created".to_string(),
                                            detail: serde_json::json!({
                                                "task_id": &pending.task.task_id,
                                                "background_addr": &addr,
                                                "triggered_by": &pending.triggered_by,
                                                "channel_addr": &pending.task.channel_addr,
                                            }),
                                        }))?;
                                        ctx.outbox.send(ServiceOutput::Call(
                                            agent_session::enqueue_message_call(
                                                ctx.addr.clone(),
                                                addr,
                                                AgentMessageOrigin::System,
                                                cron_task_message(&pending.task),
                                                None,
                                            )?,
                                        ))?;
                                        continue;
                                    }
                                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                        addr: ctx.addr.clone(),
                                        label: "agent_session_created".to_string(),
                                        detail: serde_json::json!({"addr": addr}),
                                    }))?;
                                }
                                Ok(KernelResponse::Error { code, message }) => {
                                    record_pending_run_failed(
                                        &ctx,
                                        &mut tasks,
                                        &mut pending_runs,
                                        &mut pending_order,
                                        next_run_id,
                                        "kernel_create_agent_session_failed",
                                        format!("{code}: {message}"),
                                        serde_json::json!({
                                            "code": code,
                                            "message": message,
                                        }),
                                    )?;
                                }
                                Ok(response) => {
                                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                        addr: ctx.addr.clone(),
                                        label: "kernel_response".to_string(),
                                        detail: serde_json::to_value(response)?,
                                    }))?;
                                }
                                Err(_) => {
                                    ctx.outbox.send(ServiceOutput::Failed(crate::conversation_new::ServiceFailure {
                                        addr: ctx.addr.clone(),
                                        error: format!("bad cron payload: {error}"),
                                    }))?;
                                }
                            }
                        }
                    }
                }
                recv(timer_rx) -> _ => {
                    let task_ids = wakeup
                        .as_ref()
                        .map(|wakeup| due_task_ids(&mut tasks, wakeup, Instant::now()))
                        .unwrap_or_default();
                    for task_id in task_ids {
                        if let Some(task) = tasks.get(&task_id).map(|task| task.registration.clone()) {
                            if let Some(background_addr) =
                                task_run_in_progress(&pending_runs, &active_runs, &task_id)
                            {
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "task_trigger_skipped".to_string(),
                                    detail: serde_json::json!({
                                        "task_id": task_id,
                                        "background_addr": background_addr,
                                        "reason": "task is already running",
                                    }),
                                }))?;
                                continue;
                            }
                            let background_addr = trigger_task(
                                &ctx,
                                &mut pending_runs,
                                &mut pending_order,
                                &mut next_run_id,
                                task.clone(),
                                ServiceAddr::cron(),
                            )?;
                            mark_task_running(&mut tasks, &task_id, background_addr);
                            persist_tasks(&ctx.storage, &tasks, next_run_id)?;
                        }
                    }
                }
            }
        }
    }
}

struct ScheduledCronTask {
    registration: CronTaskRegistration,
    interval_anchor: Instant,
    consecutive_failures: u32,
    last_error: Option<String>,
    last_result_summary: Option<String>,
    last_run_status: Option<CronRunStatus>,
}

impl ScheduledCronTask {
    fn new(registration: CronTaskRegistration, now: Instant) -> Self {
        Self {
            registration,
            interval_anchor: now,
            consecutive_failures: 0,
            last_error: None,
            last_result_summary: None,
            last_run_status: None,
        }
    }

    fn from_persisted(persisted: PersistedCronTask, now: Instant) -> Self {
        Self {
            registration: persisted.registration,
            interval_anchor: now,
            consecutive_failures: persisted.consecutive_failures,
            last_error: persisted.last_error,
            last_result_summary: persisted.last_result_summary,
            last_run_status: persisted.last_run_status,
        }
    }

    fn persisted(&self) -> PersistedCronTask {
        PersistedCronTask {
            registration: self.registration.clone(),
            consecutive_failures: self.consecutive_failures,
            last_error: self.last_error.clone(),
            last_result_summary: self.last_result_summary.clone(),
            last_run_status: self.last_run_status.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CronStateFile {
    version: u32,
    next_run_id: u64,
    tasks: Vec<PersistedCronTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCronTask {
    registration: CronTaskRegistration,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run_status: Option<CronRunStatus>,
}

struct PendingCronRun {
    task: CronTaskRegistration,
    triggered_by: ServiceAddr,
}

struct ActiveCronRun {
    task_id: String,
    triggered_by: ServiceAddr,
    channel_addr: ServiceAddr,
    foreground_session_addr: Option<ServiceAddr>,
    output_policy: CronTaskOutputPolicy,
}

fn trigger_task(
    ctx: &ServiceRunContext,
    pending_runs: &mut HashMap<ServiceAddr, PendingCronRun>,
    pending_order: &mut VecDeque<ServiceAddr>,
    next_run_id: &mut u64,
    task: CronTaskRegistration,
    triggered_by: ServiceAddr,
) -> Result<ServiceAddr> {
    let run_id = *next_run_id;
    *next_run_id += 1;
    let background_id = format!("cron_{}_{}", storage_id(&task.task_id), run_id);
    let background_addr = ServiceAddr::agent_background(background_id.clone());
    let parent_addr = task
        .foreground_session_addr
        .clone()
        .or_else(|| Some(task.registered_by.clone()));
    pending_runs.insert(
        background_addr.clone(),
        PendingCronRun {
            task: task.clone(),
            triggered_by: triggered_by.clone(),
        },
    );
    pending_order.push_back(background_addr.clone());
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "task_triggered".to_string(),
        detail: serde_json::json!({
            "task_id": &task.task_id,
            "background_addr": &background_addr,
            "triggered_by": &triggered_by,
            "registered_by": &task.registered_by,
            "channel_addr": &task.channel_addr,
            "foreground_session_addr": &task.foreground_session_addr,
        }),
    }))?;
    ctx.outbox.send(ServiceOutput::Call(
        kernel::create_agent_session_with_binding_call(
            ctx.addr.clone(),
            AgentSessionKind::Background,
            Some(background_id),
            AgentSessionBinding {
                event_sink: ctx.addr.clone(),
                parent_addr,
            },
        )?,
    ))?;
    Ok(background_addr)
}

fn remove_pending_order(pending_order: &mut VecDeque<ServiceAddr>, addr: &ServiceAddr) {
    pending_order.retain(|pending_addr| pending_addr != addr);
}

fn mark_task_running(
    tasks: &mut HashMap<String, ScheduledCronTask>,
    task_id: &str,
    background_addr: ServiceAddr,
) {
    if let Some(task) = tasks.get_mut(task_id) {
        task.last_run_status = Some(CronRunStatus::Running);
        task.last_error = None;
        task.last_result_summary = Some(format!("Running in {background_addr}"));
    }
}

fn task_run_in_progress(
    pending_runs: &HashMap<ServiceAddr, PendingCronRun>,
    active_runs: &HashMap<ServiceAddr, ActiveCronRun>,
    task_id: &str,
) -> Option<ServiceAddr> {
    pending_runs
        .iter()
        .find_map(|(addr, run)| (run.task.task_id == task_id).then(|| addr.clone()))
        .or_else(|| {
            active_runs
                .iter()
                .find_map(|(addr, run)| (run.task_id == task_id).then(|| addr.clone()))
        })
}

fn handle_agent_session_event(
    ctx: &ServiceRunContext,
    tasks: &mut HashMap<String, ScheduledCronTask>,
    active_runs: &mut HashMap<ServiceAddr, ActiveCronRun>,
    next_run_id: u64,
    session_addr: ServiceAddr,
    event: AgentSessionEvent,
) -> Result<()> {
    match event {
        AgentSessionEvent::TurnCompleted { message } => {
            let Some(run) = active_runs.remove(&session_addr) else {
                return Ok(());
            };
            if let Some(task) = tasks.get_mut(&run.task_id) {
                task.consecutive_failures = 0;
                task.last_error = None;
                task.last_run_status = Some(CronRunStatus::Completed);
                task.last_result_summary = Some("completed".to_string());
            }
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "task_run_completed".to_string(),
                detail: serde_json::json!({
                    "task_id": run.task_id,
                    "background_addr": session_addr,
                    "triggered_by": run.triggered_by,
                    "channel_addr": run.channel_addr,
                }),
            }))?;
            if run.output_policy == CronTaskOutputPolicy::ForwardResultToForeground {
                if let Some(foreground_session_addr) = run.foreground_session_addr {
                    ctx.outbox
                        .send(ServiceOutput::Call(agent_session::enqueue_message_call(
                            ctx.addr.clone(),
                            foreground_session_addr,
                            AgentMessageOrigin::Actor,
                            cron_result_message(message),
                            None,
                        )?))?;
                }
            }
            persist_tasks(&ctx.storage, tasks, next_run_id)?;
        }
        AgentSessionEvent::TurnFailed {
            error,
            error_detail,
            can_continue,
        } => {
            record_failed_run(
                ctx,
                tasks,
                active_runs,
                session_addr,
                "turn_failed",
                error,
                serde_json::json!({
                    "error_detail": error_detail,
                    "can_continue": can_continue,
                }),
                next_run_id,
            )?;
        }
        AgentSessionEvent::RuntimeCrashed {
            error,
            error_detail,
        } => {
            record_failed_run(
                ctx,
                tasks,
                active_runs,
                session_addr,
                "runtime_crashed",
                error,
                serde_json::json!({"error_detail": error_detail}),
                next_run_id,
            )?;
        }
        AgentSessionEvent::CompactFailed { phase, reason } => {
            record_failed_run(
                ctx,
                tasks,
                active_runs,
                session_addr,
                "compact_failed",
                reason,
                serde_json::json!({"phase": phase}),
                next_run_id,
            )?;
        }
        AgentSessionEvent::ControlRejected { reason, payload } => {
            record_failed_run(
                ctx,
                tasks,
                active_runs,
                session_addr,
                "control_rejected",
                reason,
                serde_json::json!({"payload": payload}),
                next_run_id,
            )?;
        }
        event => {
            if let Some(run) = active_runs.get(&session_addr) {
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "task_run_event".to_string(),
                    detail: serde_json::json!({
                        "task_id": run.task_id,
                        "background_addr": session_addr,
                        "event": event,
                    }),
                }))?;
            }
        }
    }
    Ok(())
}

fn record_failed_run(
    ctx: &ServiceRunContext,
    tasks: &mut HashMap<String, ScheduledCronTask>,
    active_runs: &mut HashMap<ServiceAddr, ActiveCronRun>,
    session_addr: ServiceAddr,
    code: &'static str,
    message: String,
    detail: serde_json::Value,
    next_run_id: u64,
) -> Result<()> {
    let Some(run) = active_runs.remove(&session_addr) else {
        return Ok(());
    };
    let failure_count = if let Some(task) = tasks.get_mut(&run.task_id) {
        task.consecutive_failures += 1;
        task.last_error = Some(message.clone());
        task.last_run_status = Some(CronRunStatus::Failed);
        task.last_result_summary = None;
        task.consecutive_failures
    } else {
        1
    };
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "task_run_failed".to_string(),
        detail: serde_json::json!({
            "task_id": run.task_id,
            "background_addr": session_addr,
            "triggered_by": run.triggered_by,
            "channel_addr": run.channel_addr,
            "code": code,
            "message": message,
            "detail": detail,
            "consecutive_failures": failure_count,
        }),
    }))?;
    persist_tasks(&ctx.storage, tasks, next_run_id)?;
    Ok(())
}

fn record_pending_run_failed(
    ctx: &ServiceRunContext,
    tasks: &mut HashMap<String, ScheduledCronTask>,
    pending_runs: &mut HashMap<ServiceAddr, PendingCronRun>,
    pending_order: &mut VecDeque<ServiceAddr>,
    next_run_id: u64,
    code: &'static str,
    message: String,
    detail: serde_json::Value,
) -> Result<()> {
    let Some(background_addr) = pending_order.pop_front() else {
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "kernel_response".to_string(),
            detail,
        }))?;
        return Ok(());
    };
    let Some(run) = pending_runs.remove(&background_addr) else {
        return Ok(());
    };
    let failure_count = if let Some(task) = tasks.get_mut(&run.task.task_id) {
        task.consecutive_failures += 1;
        task.last_error = Some(message.clone());
        task.last_run_status = Some(CronRunStatus::Failed);
        task.last_result_summary = None;
        task.consecutive_failures
    } else {
        1
    };
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "task_run_failed".to_string(),
        detail: serde_json::json!({
            "task_id": run.task.task_id,
            "background_addr": background_addr,
            "triggered_by": run.triggered_by,
            "channel_addr": run.task.channel_addr,
            "code": code,
            "message": message,
            "detail": detail,
            "consecutive_failures": failure_count,
        }),
    }))?;
    persist_tasks(&ctx.storage, tasks, next_run_id)?;
    Ok(())
}

fn reply(
    source: &crate::conversation_new::ServiceAddr,
    target: &crate::conversation_new::ServiceAddr,
    payload: serde_json::Value,
    response_id: Option<String>,
) -> ServiceCall {
    ServiceCall::response_to(source.clone(), target.clone(), payload, response_id)
}

fn cron_task_message(task: &CronTaskRegistration) -> stellaclaw_core::session_actor::ChatMessage {
    match &task.payload {
        CronTaskPayload::Prompt { prompt, .. } => {
            agent_session::text_message(ChatRole::User, prompt.clone())
        }
    }
}

fn task_output_policy(task: &CronTaskRegistration) -> CronTaskOutputPolicy {
    match &task.payload {
        CronTaskPayload::Prompt { output_policy, .. } => output_policy.clone(),
    }
}

fn validate_task_registration(
    task: &CronTaskRegistration,
    source: &ServiceAddr,
) -> Result<(), String> {
    if task.task_id.trim().is_empty() {
        return Err("task_id must not be empty".to_string());
    }
    validate_schedule(&task.schedule)?;

    let Some(channel_id) = local_channel_id(&task.channel_addr) else {
        return Err("channel_addr must be a local channel address".to_string());
    };
    if let Some(foreground_session_addr) = &task.foreground_session_addr {
        let Some(foreground_id) = local_foreground_id(foreground_session_addr) else {
            return Err(
                "foreground_session_addr must be a local foreground agent address".to_string(),
            );
        };
        if foreground_id != channel_id {
            return Err("foreground_session_addr must match channel_addr id".to_string());
        }
    }
    if let Some(source_channel_id) = local_channel_id(source) {
        if source_channel_id != channel_id {
            return Err("registering channel must match channel_addr id".to_string());
        }
    }
    if let Some(source_foreground_id) = local_foreground_id(source) {
        if source_foreground_id != channel_id {
            return Err("registering foreground session must match channel_addr id".to_string());
        }
    }
    Ok(())
}

fn validate_schedule(schedule: &CronSchedule) -> Result<(), String> {
    match schedule {
        CronSchedule::Manual => Ok(()),
        CronSchedule::IntervalSeconds { seconds } => {
            if !seconds.is_finite() || *seconds <= 0.0 {
                return Err("interval_seconds schedule must be finite and positive".to_string());
            }
            Ok(())
        }
        CronSchedule::CronExpression {
            expression,
            timezone,
        } => {
            if expression.trim().is_empty() {
                return Err("cron expression must not be empty".to_string());
            }
            Schedule::from_str(expression)
                .map_err(|error| format!("invalid cron expression: {error}"))?;
            parse_timezone(timezone.as_deref())?;
            Ok(())
        }
    }
}

fn update_task(
    tasks: &mut HashMap<String, ScheduledCronTask>,
    task_id: &str,
    source: &ServiceAddr,
    patch: CronTaskPatch,
    now: Instant,
) -> Result<CronTaskRegistration, String> {
    let Some(task) = tasks.get_mut(task_id) else {
        return Err("unknown task".to_string());
    };
    if task.registration.registered_by != *source {
        return Err("task is not owned by caller".to_string());
    }
    if let Some(name) = patch.name {
        task.registration.name = name;
    }
    if let Some(description) = patch.description {
        task.registration.description = description;
    }
    if let Some(enabled) = patch.enabled {
        task.registration.enabled = enabled;
    }
    if let Some(schedule) = patch.schedule {
        validate_schedule(&schedule)?;
        task.registration.schedule = schedule;
    }
    if let Some(payload) = patch.payload {
        task.registration.payload = payload;
    }
    task.interval_anchor = now;
    Ok(task.registration.clone())
}

fn remove_task(
    tasks: &mut HashMap<String, ScheduledCronTask>,
    task_id: &str,
    source: &ServiceAddr,
) -> Result<Option<CronTaskRegistration>, String> {
    let Some(task) = tasks.get(task_id) else {
        return Ok(None);
    };
    if task.registration.registered_by != *source {
        return Err("task is not owned by caller".to_string());
    }
    Ok(tasks.remove(task_id).map(|task| task.registration))
}

fn disable_tasks_for_owner(
    tasks: &mut HashMap<String, ScheduledCronTask>,
    owner: &ServiceAddr,
) -> usize {
    let mut disabled = 0usize;
    for task in tasks.values_mut() {
        if task.registration.registered_by == *owner && task.registration.enabled {
            task.registration.enabled = false;
            disabled += 1;
        }
    }
    disabled
}

fn local_channel_id(addr: &ServiceAddr) -> Option<&str> {
    if matches!(&addr.scope, ServiceScope::Local)
        && addr.path.len() == 2
        && addr.path.first().map(String::as_str) == Some("channel")
    {
        addr.path.get(1).map(String::as_str)
    } else {
        None
    }
}

fn local_foreground_id(addr: &ServiceAddr) -> Option<&str> {
    if matches!(&addr.scope, ServiceScope::Local)
        && addr.path.len() == 3
        && addr.path.first().map(String::as_str) == Some("agent")
        && addr.path.get(1).map(String::as_str) == Some("foreground")
    {
        addr.path.get(2).map(String::as_str)
    } else {
        None
    }
}

fn cron_result_message(mut message: ChatMessage) -> ChatMessage {
    message.role = ChatRole::User;
    message.token_usage = None;
    message
}

fn task_status(
    tasks: &HashMap<String, ScheduledCronTask>,
    pending_runs: &HashMap<ServiceAddr, PendingCronRun>,
    active_runs: &HashMap<ServiceAddr, ActiveCronRun>,
    task_id: &str,
    now_instant: Instant,
    now_utc: DateTime<Utc>,
) -> Option<CronTaskStatus> {
    let task = tasks.get(task_id)?;
    let active_background_addr = task_run_in_progress(pending_runs, active_runs, task_id);
    let next_due_in_ms = next_wakeup_for_task(task, now_instant, now_utc).map(|wakeup| {
        wakeup
            .saturating_duration_since(now_instant)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    });
    Some(CronTaskStatus {
        registration: task.registration.clone(),
        active_background_addr,
        next_due_in_ms,
        last_run_status: task.last_run_status.clone(),
        last_result_summary: task.last_result_summary.clone(),
        last_error: task.last_error.clone(),
        consecutive_failures: task.consecutive_failures,
    })
}

fn state_path(storage: &Path) -> PathBuf {
    storage.join("tasks.json")
}

fn load_tasks(storage: &Path, now: Instant) -> Result<(HashMap<String, ScheduledCronTask>, u64)> {
    let path = state_path(storage);
    if !path.is_file() {
        return Ok((HashMap::new(), 1));
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let state: CronStateFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let mut tasks = HashMap::new();
    for task in state.tasks {
        tasks.insert(
            task.registration.task_id.clone(),
            ScheduledCronTask::from_persisted(task, now),
        );
    }
    Ok((tasks, state.next_run_id.max(1)))
}

fn persist_tasks(
    storage: &Path,
    tasks: &HashMap<String, ScheduledCronTask>,
    next_run_id: u64,
) -> Result<()> {
    fs::create_dir_all(storage)
        .with_context(|| format!("failed to create {}", storage.display()))?;
    let mut persisted = tasks
        .values()
        .map(ScheduledCronTask::persisted)
        .collect::<Vec<_>>();
    persisted.sort_by_key(|task| task.registration.task_id.clone());
    let state = CronStateFile {
        version: 1,
        next_run_id,
        tasks: persisted,
    };
    let content = serde_json::to_string_pretty(&state).context("failed to encode cron state")?;
    let path = state_path(storage);
    let tmp_path = storage.join("tasks.json.tmp");
    fs::write(&tmp_path, content)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            path.display()
        )
    })
}

struct CronWakeup {
    at: Instant,
    task_ids: Vec<String>,
}

fn next_wakeup(
    tasks: &HashMap<String, ScheduledCronTask>,
    now_instant: Instant,
    now_utc: DateTime<Utc>,
) -> Option<CronWakeup> {
    let mut wakeup_at: Option<Instant> = None;
    let mut task_ids = Vec::new();
    for task in tasks.values() {
        let Some(next) = next_wakeup_for_task(task, now_instant, now_utc) else {
            continue;
        };
        match wakeup_at {
            None => {
                wakeup_at = Some(next);
                task_ids.push(task.registration.task_id.clone());
            }
            Some(current) if next < current => {
                wakeup_at = Some(next);
                task_ids.clear();
                task_ids.push(task.registration.task_id.clone());
            }
            Some(current) if next == current => {
                task_ids.push(task.registration.task_id.clone());
            }
            Some(_) => {}
        }
    }
    task_ids.sort();
    wakeup_at.map(|at| CronWakeup { at, task_ids })
}

fn next_wakeup_for_task(
    task: &ScheduledCronTask,
    now_instant: Instant,
    now_utc: DateTime<Utc>,
) -> Option<Instant> {
    if !task.registration.enabled {
        return None;
    }
    if validate_schedule(&task.registration.schedule).is_err() {
        return None;
    }
    match &task.registration.schedule {
        CronSchedule::Manual => None,
        CronSchedule::IntervalSeconds { seconds } => {
            let interval = Duration::from_secs_f64(*seconds).max(Duration::from_millis(10));
            Some(
                task.interval_anchor
                    .checked_add(interval)
                    .unwrap_or(now_instant + interval)
                    .max(now_instant),
            )
        }
        CronSchedule::CronExpression {
            expression,
            timezone,
        } => next_cron_instant(expression, timezone.as_deref(), now_instant, now_utc),
    }
}

fn due_task_ids(
    tasks: &mut HashMap<String, ScheduledCronTask>,
    wakeup: &CronWakeup,
    now: Instant,
) -> Vec<String> {
    let mut due = Vec::new();
    for task_id in &wakeup.task_ids {
        let Some(task) = tasks.get_mut(task_id) else {
            continue;
        };
        if !task.registration.enabled {
            continue;
        }
        if matches!(
            task.registration.schedule,
            CronSchedule::IntervalSeconds { .. }
        ) {
            task.interval_anchor = now;
        }
        due.push(task_id.clone());
    }
    due.sort();
    due
}

fn next_cron_instant(
    expression: &str,
    timezone: Option<&str>,
    now_instant: Instant,
    now_utc: DateTime<Utc>,
) -> Option<Instant> {
    let timezone = parse_timezone(timezone).ok()?;
    let schedule = Schedule::from_str(expression).ok()?;
    let next_utc = schedule
        .after(&now_utc.with_timezone(&timezone))
        .next()?
        .with_timezone(&Utc);
    let delay = next_utc
        .signed_duration_since(now_utc)
        .to_std()
        .unwrap_or(Duration::ZERO);
    Some(now_instant + delay)
}

fn parse_timezone(timezone: Option<&str>) -> Result<Tz, String> {
    timezone
        .map(str::trim)
        .filter(|timezone| !timezone.is_empty())
        .unwrap_or("Asia/Shanghai")
        .parse::<Tz>()
        .map_err(|error| format!("invalid timezone: {error}"))
}

fn storage_id(value: &str) -> String {
    let mut id = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    if id.is_empty() {
        id.push_str("task");
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_new::{ConversationRef, ServiceRefs};
    use chrono::TimeZone;
    use stellaclaw_core::session_actor::SessionErrorDetail;

    #[test]
    fn records_background_run_failure() {
        let (outbox, output_rx) = crossbeam_channel::unbounded();
        let (_inbox_tx, inbox) = crossbeam_channel::unbounded();
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let ctx = ServiceRunContext {
            addr: ServiceAddr::cron(),
            conversation: ConversationRef {
                conversation_id: "cron_failure".to_string(),
                workdir: std::env::temp_dir(),
                conversation_root: std::env::temp_dir(),
            },
            storage: std::env::temp_dir(),
            refs: ServiceRefs::default(),
            inbox,
            outbox,
            stop_rx,
        };
        let task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload("check"),
        };
        let background_addr = ServiceAddr::agent_background("cron_daily_1");
        let mut tasks = HashMap::from([(
            task.task_id.clone(),
            ScheduledCronTask::new(task, Instant::now()),
        )]);
        let mut active_runs = HashMap::from([(
            background_addr.clone(),
            ActiveCronRun {
                task_id: "daily".to_string(),
                triggered_by: ServiceAddr::cron(),
                channel_addr: ServiceAddr::channel_id("scratch"),
                foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
            },
        )]);

        handle_agent_session_event(
            &ctx,
            &mut tasks,
            &mut active_runs,
            1,
            background_addr.clone(),
            AgentSessionEvent::TurnFailed {
                error: "boom".to_string(),
                error_detail: SessionErrorDetail::new("agent", "test", "boom"),
                can_continue: false,
            },
        )
        .expect("failure records");

        assert!(active_runs.is_empty());
        let task = tasks.get("daily").expect("task remains");
        assert_eq!(task.consecutive_failures, 1);
        assert_eq!(task.last_error.as_deref(), Some("boom"));
        let status = output_rx.try_recv().expect("status emitted");
        let ServiceOutput::Status(status) = status else {
            panic!("expected status");
        };
        assert_eq!(status.label, "task_run_failed");
        assert_eq!(status.detail["task_id"], "daily");
        assert_eq!(status.detail["consecutive_failures"], 1);
    }

    #[test]
    fn persists_and_loads_tasks() {
        let storage = std::env::temp_dir().join(format!(
            "stellaclaw-cron-persist-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock works")
                .as_nanos()
        ));
        let task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload("check"),
        };
        let mut tasks = HashMap::from([(
            task.task_id.clone(),
            ScheduledCronTask::new(task, Instant::now()),
        )]);
        if let Some(task) = tasks.get_mut("daily") {
            task.consecutive_failures = 2;
            task.last_error = Some("boom".to_string());
            task.last_run_status = Some(CronRunStatus::Failed);
        }

        persist_tasks(&storage, &tasks, 9).expect("state persists");
        let (loaded, next_run_id) = load_tasks(&storage, Instant::now()).expect("state loads");

        assert_eq!(next_run_id, 9);
        let loaded_task = loaded.get("daily").expect("task loaded");
        assert_eq!(loaded_task.registration.task_id, "daily");
        assert_eq!(loaded_task.consecutive_failures, 2);
        assert_eq!(loaded_task.last_error.as_deref(), Some("boom"));
        assert_eq!(loaded_task.last_run_status, Some(CronRunStatus::Failed));
        assert!(storage.join("tasks.json").is_file());
    }

    #[test]
    fn task_status_reports_active_run_and_last_result() {
        let task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::IntervalSeconds { seconds: 60.0 },
            payload: prompt_payload("check"),
        };
        let background_addr = ServiceAddr::agent_background("cron_daily_1");
        let mut scheduled = ScheduledCronTask::new(task, Instant::now());
        scheduled.last_run_status = Some(CronRunStatus::Running);
        scheduled.last_result_summary = Some(format!("Running in {background_addr}"));
        let tasks = HashMap::from([("daily".to_string(), scheduled)]);
        let pending_runs = HashMap::new();
        let active_runs = HashMap::from([(
            background_addr.clone(),
            ActiveCronRun {
                task_id: "daily".to_string(),
                triggered_by: ServiceAddr::cron(),
                channel_addr: ServiceAddr::channel_id("scratch"),
                foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
            },
        )]);

        let status = task_status(
            &tasks,
            &pending_runs,
            &active_runs,
            "daily",
            Instant::now(),
            Utc::now(),
        )
        .expect("status exists");

        assert_eq!(status.active_background_addr, Some(background_addr));
        assert_eq!(status.last_run_status, Some(CronRunStatus::Running));
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.next_due_in_ms.is_some());
    }

    #[test]
    fn detects_pending_or_active_task_run() {
        let task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload("check"),
        };
        let pending_addr = ServiceAddr::agent_background("cron_daily_1");
        let pending_runs = HashMap::from([(
            pending_addr.clone(),
            PendingCronRun {
                task: task.clone(),
                triggered_by: ServiceAddr::cron(),
            },
        )]);
        let active_addr = ServiceAddr::agent_background("cron_other_1");
        let active_runs = HashMap::from([(
            active_addr,
            ActiveCronRun {
                task_id: "other".to_string(),
                triggered_by: ServiceAddr::cron(),
                channel_addr: ServiceAddr::channel_id("scratch"),
                foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
            },
        )]);

        assert_eq!(
            task_run_in_progress(&pending_runs, &active_runs, "daily"),
            Some(pending_addr)
        );
    }

    #[test]
    fn update_remove_and_disable_are_owner_scoped() {
        let owner = ServiceAddr::agent_foreground_id("scratch");
        let other = ServiceAddr::agent_foreground_id("other");
        let mut tasks = HashMap::from([(
            "daily".to_string(),
            ScheduledCronTask::new(
                CronTaskRegistration {
                    task_id: "daily".to_string(),
                    registered_by: owner.clone(),
                    channel_addr: ServiceAddr::channel_id("scratch"),
                    name: Some("Daily".to_string()),
                    description: Some("old".to_string()),
                    enabled: true,
                    foreground_session_addr: Some(owner.clone()),
                    schedule: CronSchedule::IntervalSeconds { seconds: 60.0 },
                    payload: prompt_payload("check"),
                },
                Instant::now(),
            ),
        )]);

        let denied = update_task(
            &mut tasks,
            "daily",
            &other,
            CronTaskPatch {
                enabled: Some(false),
                ..CronTaskPatch::default()
            },
            Instant::now(),
        )
        .expect_err("other owner cannot update task");
        assert_eq!(denied, "task is not owned by caller");
        assert!(tasks["daily"].registration.enabled);

        let updated = update_task(
            &mut tasks,
            "daily",
            &owner,
            CronTaskPatch {
                description: Some(Some("new".to_string())),
                enabled: Some(false),
                ..CronTaskPatch::default()
            },
            Instant::now(),
        )
        .expect("owner updates task");
        assert_eq!(updated.description.as_deref(), Some("new"));
        assert!(!updated.enabled);
        assert!(next_wakeup_for_task(&tasks["daily"], Instant::now(), Utc::now()).is_none());

        update_task(
            &mut tasks,
            "daily",
            &owner,
            CronTaskPatch {
                enabled: Some(true),
                ..CronTaskPatch::default()
            },
            Instant::now(),
        )
        .expect("owner re-enables task");
        assert_eq!(disable_tasks_for_owner(&mut tasks, &other), 0);
        assert!(tasks["daily"].registration.enabled);
        assert_eq!(disable_tasks_for_owner(&mut tasks, &owner), 1);
        assert!(!tasks["daily"].registration.enabled);

        let denied_remove =
            remove_task(&mut tasks, "daily", &other).expect_err("other owner cannot remove task");
        assert_eq!(denied_remove, "task is not owned by caller");
        let removed = remove_task(&mut tasks, "daily", &owner)
            .expect("owner can remove task")
            .expect("task existed");
        assert_eq!(removed.task_id, "daily");
        assert!(!tasks.contains_key("daily"));
    }

    #[test]
    fn cron_expression_wakeup_uses_next_future_wall_clock_time() {
        let now_utc = Utc.with_ymd_and_hms(2026, 5, 14, 1, 4, 59).unwrap();
        let now_instant = Instant::now();
        let task = ScheduledCronTask::new(
            CronTaskRegistration {
                task_id: "morning".to_string(),
                registered_by: ServiceAddr::agent_foreground_id("scratch"),
                channel_addr: ServiceAddr::channel_id("scratch"),
                name: None,
                description: None,
                enabled: true,
                foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
                schedule: CronSchedule::CronExpression {
                    expression: "0 5 9 * * *".to_string(),
                    timezone: Some("Asia/Shanghai".to_string()),
                },
                payload: prompt_payload("check"),
            },
            now_instant,
        );

        let wakeup = next_wakeup_for_task(&task, now_instant, now_utc).expect("cron has wakeup");

        assert_eq!(
            wakeup.saturating_duration_since(now_instant),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn persisted_interval_tasks_restart_from_now_without_backfill() {
        let now = Instant::now();
        let task = CronTaskRegistration {
            task_id: "interval".to_string(),
            registered_by: ServiceAddr::agent_foreground_id("scratch"),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::IntervalSeconds { seconds: 60.0 },
            payload: prompt_payload("check"),
        };
        let persisted = PersistedCronTask {
            registration: task,
            consecutive_failures: 0,
            last_error: None,
            last_result_summary: None,
            last_run_status: None,
        };

        let loaded = ScheduledCronTask::from_persisted(persisted, now);
        let wakeup =
            next_wakeup_for_task(&loaded, now, Utc::now()).expect("interval has next wakeup");

        assert_eq!(
            wakeup.saturating_duration_since(now),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn rejects_invalid_task_registration() {
        let source = ServiceAddr::agent_foreground_id("scratch");
        let mut task = CronTaskRegistration {
            task_id: "daily".to_string(),
            registered_by: source.clone(),
            channel_addr: ServiceAddr::channel_id("scratch"),
            name: None,
            description: None,
            enabled: true,
            foreground_session_addr: Some(ServiceAddr::agent_foreground_id("scratch")),
            schedule: CronSchedule::Manual,
            payload: prompt_payload("check"),
        };

        task.task_id = "  ".to_string();
        assert_eq!(
            validate_task_registration(&task, &source).expect_err("empty task id rejected"),
            "task_id must not be empty"
        );

        task.task_id = "daily".to_string();
        task.schedule = CronSchedule::IntervalSeconds { seconds: 0.0 };
        assert_eq!(
            validate_task_registration(&task, &source).expect_err("bad interval rejected"),
            "interval_seconds schedule must be finite and positive"
        );

        task.schedule = CronSchedule::Manual;
        task.foreground_session_addr = Some(ServiceAddr::agent_foreground_id("other"));
        assert_eq!(
            validate_task_registration(&task, &source)
                .expect_err("foreground/channel mismatch rejected"),
            "foreground_session_addr must match channel_addr id"
        );
    }

    fn prompt_payload(prompt: &str) -> CronTaskPayload {
        CronTaskPayload::Prompt {
            prompt: prompt.to_string(),
            output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
        }
    }
}
