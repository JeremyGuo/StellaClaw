use super::*;

impl AgentRuntimeView {
    pub(super) fn build_extra_tools(
        &self,
        session: &SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        control: Option<SessionExecutionControl>,
    ) -> Vec<Tool> {
        let mut tools = Vec::new();
        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            tools.push(Tool::new(
                "workspaces_list",
                "Call this tool to get historical information, including earlier chat content and the corresponding workspace. It lists known workspaces by id, title, summary, state, and timestamps. Archived workspaces are hidden by default.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "include_archived": {"type": "boolean"}
                    },
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.list_workspaces(
                        optional_string_arg(object, "query")?,
                        object
                            .get("include_archived")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "workspace_content_list",
                "Call this tool after selecting a historical workspace to inspect what content exists there at a high level, without reading file bodies. Returns files and directories under the requested path.",
                json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string"},
                        "path": {"type": "string"},
                        "depth": {"type": "integer"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["workspace_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let workspace_id = string_arg_required(object, "workspace_id")?;
                    let depth = object.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize;
                    let limit = object.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
                    runtime.list_workspace_contents(
                        workspace_id,
                        optional_string_arg(object, "path")?,
                        depth,
                        limit.clamp(1, 500),
                    )
                },
            ));

            let runtime = self.clone();
            let mount_session = session.clone();
            tools.push(Tool::new(
                "workspace_mount",
                "Call this tool to bring a historical workspace into the current workspace as a read-only mount so you can inspect or read its content safely. Returns the mount path relative to the current workspace root.",
                json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string"},
                        "mount_name": {"type": "string"}
                    },
                    "required": ["workspace_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.mount_workspace(
                        &mount_session,
                        string_arg_required(object, "workspace_id")?,
                        optional_string_arg(object, "mount_name")?,
                    )
                },
            ));

            let runtime = self.clone();
            let move_session = session.clone();
            tools.push(Tool::new(
                "workspace_content_move",
                "Call this tool to carry forward selected content from an older workspace into the current workspace. Source and target summaries can be updated when the move changes what the workspaces represent.",
                json!({
                    "type": "object",
                    "properties": {
                        "source_workspace_id": {"type": "string"},
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "target_dir": {"type": "string"},
                        "source_summary_update": {"type": "string"},
                        "target_summary_update": {"type": "string"}
                    },
                    "required": ["source_workspace_id", "paths"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let paths = object
                        .get("paths")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow!("paths must be an array"))?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(ToOwned::to_owned)
                                .filter(|value| !value.trim().is_empty())
                                .ok_or_else(|| anyhow!("each path must be a non-empty string"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    runtime.move_workspace_contents(
                        &move_session,
                        string_arg_required(object, "source_workspace_id")?,
                        paths,
                        optional_string_arg(object, "target_dir")?,
                        optional_string_arg(object, "source_summary_update")?,
                        optional_string_arg(object, "target_summary_update")?,
                    )
                },
            ));

            let runtime = self.clone();
            let workpath_session = session.clone();
            tools.push(Tool::new(
                "workpath_add",
                "Register the remote SSH workpath for a host in this whole conversation. Each host can have only one workpath; adding the same host replaces the previous path and description. Use this when a remote directory should become durable shared context for foreground/background agents. The host must be an SSH alias, path is the remote directory, and description must explain what the directory is for. On success, the tool immediately tries to load path/AGENTS.md and future rebuilt prompts will include the host/path/description and reload AGENTS.md automatically.",
                json!({
                    "type": "object",
                    "properties": {
                        "host": {"type": "string"},
                        "path": {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["host", "path", "description"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.add_remote_workpath(
                        &workpath_session,
                        string_arg_required(object, "host")?,
                        string_arg_required(object, "path")?,
                        string_arg_required(object, "description")?,
                    )
                },
            ));

            let runtime = self.clone();
            let workpath_session = session.clone();
            tools.push(Tool::new(
                "workpath_modify",
                "Modify the description for an existing conversation-level remote workpath. The host identifies the existing remote workpath because each host can have only one.",
                json!({
                    "type": "object",
                    "properties": {
                        "host": {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["host", "description"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.modify_remote_workpath(
                        &workpath_session,
                        string_arg_required(object, "host")?,
                        string_arg_required(object, "description")?,
                    )
                },
            ));

            let runtime = self.clone();
            let workpath_session = session.clone();
            tools.push(Tool::new(
                "workpath_remove",
                "Remove an existing conversation-level remote workpath by host.",
                json!({
                    "type": "object",
                    "properties": {
                        "host": {"type": "string"}
                    },
                    "required": ["host"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.remove_remote_workpath(
                        &workpath_session,
                        string_arg_required(object, "host")?,
                    )
                },
            ));
        }

        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            if self.main_agent.memory_system == agent_frame::config::MemorySystem::Layered {
                let runtime = self.clone();
                let memory_session = session.clone();
                tools.push(Tool::new(
                    "memory_search",
                    "Search the current conversation memory layers. Use this before opening rollout summaries or transcript snippets when you need older conversation context.",
                    json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    move |arguments| {
                        let object = arguments
                            .as_object()
                            .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                        runtime.memory_search(
                            &memory_session,
                            string_arg_required(object, "query")?,
                            object.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize,
                        )
                    },
                ));

                let runtime = self.clone();
                let rollout_search_session = session.clone();
                tools.push(Tool::new(
                    "rollout_search",
                    "Search rollout transcripts for exact historical evidence. Prefer passing rollout_id when you already know which rollout is relevant.",
                    json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "rollout_id": {"type": "string"},
                            "kinds": {
                                "type": "array",
                                "items": {"type": "string"}
                            },
                            "limit": {"type": "integer"}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    move |arguments| {
                        let object = arguments
                            .as_object()
                            .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                        let kinds = object
                            .get("kinds")
                            .and_then(Value::as_array)
                            .map(|items| {
                                items.iter()
                                    .filter_map(Value::as_str)
                                    .map(ToOwned::to_owned)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        runtime.rollout_search(
                            &rollout_search_session,
                            string_arg_required(object, "query")?,
                            optional_string_arg(object, "rollout_id")?,
                            kinds,
                            object.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize,
                        )
                    },
                ));

                let runtime = self.clone();
                let rollout_read_session = session.clone();
                tools.push(Tool::new(
                    "rollout_read",
                    "Read a small snippet around one rollout transcript event. Use this after rollout_search instead of opening the whole transcript.",
                    json!({
                        "type": "object",
                        "properties": {
                            "rollout_id": {"type": "string"},
                            "anchor_event_id": {"type": "integer"},
                            "mode": {"type": "string"},
                            "before": {"type": "integer"},
                            "after": {"type": "integer"}
                        },
                        "required": ["rollout_id", "anchor_event_id"],
                        "additionalProperties": false
                    }),
                    move |arguments| {
                        let object = arguments
                            .as_object()
                            .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                        runtime.rollout_read(
                            &rollout_read_session,
                            string_arg_required(object, "rollout_id")?,
                            object
                                .get("anchor_event_id")
                                .and_then(Value::as_u64)
                                .ok_or_else(|| anyhow!("anchor_event_id must be an integer"))?
                                as usize,
                            optional_string_arg(object, "mode")?,
                            object.get("before").and_then(Value::as_u64).unwrap_or(3) as usize,
                            object.get("after").and_then(Value::as_u64).unwrap_or(3) as usize,
                        )
                    },
                ));
            }

            let runtime = self.clone();
            let tell_session = session.clone();
            tools.push(Tool::new(
                "shared_profile_upload",
                "Upload the workspace copies of USER.md and IDENTITY.md back to the shared profile files.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |arguments| {
                    let _ = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.upload_shared_profile_files(&tell_session)
                },
            ));

            let runtime = self.clone();
            let tell_session = session.clone();
            let user_tell_description = match kind {
                AgentPromptKind::MainBackground => {
                    "Immediately send a short progress or coordination message to the current user conversation without waiting for the current background turn to finish; do not use user_tell for the primary result. Put that primary user-facing message in your final answer instead. To include files or images, append one or more <attachment>relative/path/from/workspace_root</attachment> tags inside text."
                }
                AgentPromptKind::MainForeground | AgentPromptKind::SubAgent => {
                    "Immediately send a short progress or coordination message to the current user conversation without waiting for the current turn to finish. To include files or images, append one or more <attachment>relative/path/from/workspace_root</attachment> tags inside text."
                }
            };
            tools.push(Tool::new(
                "user_tell",
                user_tell_description,
                json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.tell_user_now(&tell_session, string_arg_required(object, "text")?)
                },
            ));

            let runtime = self.clone();
            let plan_session = session.clone();
            tools.push(Tool::new(
                "update_plan",
                "Replace the current task plan shown to the user.",
                json!({
                    "type": "object",
                    "properties": {
                        "explanation": {"type": "string"},
                        "plan": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "step": {"type": "string"},
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed"]
                                    }
                                },
                                "required": ["step", "status"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["plan"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let plan = parse_session_plan_tool_args(object)?;
                    let actor = runtime
                        .with_sessions(|sessions| sessions.resolve_snapshot(&plan_session))?;
                    actor.update_plan(plan)?;
                    Ok(json!({"updated": true}))
                },
            ));

            let runtime = self.clone();
            let create_session = session.clone();
            tools.push(Tool::new(
                "subagent_start",
                "Start a session-bound subagent for a small delegated task. Requires description. Optionally set model.",
                json!({
                    "type": "object",
                    "properties": {
                        "description": {"type": "string"},
                        "model": {"type": "string"}
                    },
                    "required": ["description"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.start_subagent(
                        agent_id,
                        create_session.clone(),
                        string_arg_required(object, "description")?,
                        optional_string_arg(object, "model")?,
                    )
                },
            ));

            let runtime = self.clone();
            let destroy_session = session.clone();
            tools.push(Tool::new(
                "subagent_kill",
                "Kill a running subagent and clean up its state.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.kill_subagent(&destroy_session, parse_uuid_arg(object, "agent_id")?)
                },
            ));

            let runtime = self.clone();
            let wait_session = session.clone();
            let wait_control = control.clone();
            tools.push(Tool::new_interruptible(
                "subagent_join",
                "Wait until a subagent finishes or fails. Supports an optional timeout_seconds; timing out returns a still-running result without killing the subagent. Finished or failed subagents are destroyed immediately after join returns them.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "timeout_seconds": {"type": "number"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.join_subagent(
                        &wait_session,
                        parse_uuid_arg(object, "agent_id")?,
                        object.get("timeout_seconds").and_then(Value::as_f64).unwrap_or(0.0),
                        wait_control.clone(),
                    )
                },
            ));
        }

        if matches!(kind, AgentPromptKind::MainForeground) {
            let runtime = self.clone();
            let session = session.clone();
            tools.push(Tool::new(
                "start_background_agent",
                "Start a main background agent. Arguments: task (string), optional model (string). The final user-facing reply is delivered to the current foreground conversation and inserted into the main foreground context.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string"},
                        "model": {"type": "string"}
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let task = object
                        .get("task")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow!("task must be a non-empty string"))?;
                    let model_key = object
                        .get("model")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    runtime.start_background_agent(
                        agent_id,
                        session.clone(),
                        model_key,
                        task.to_string(),
                    )
                },
            ));
        }

        if matches!(kind, AgentPromptKind::MainBackground) {
            let runtime = self.clone();
            let terminate_control = control.clone();
            tools.push(Tool::new(
                "terminate",
                "Terminate this main background agent silently. Use this when the task should stop without sending any user-facing reply or inserting anything into the main foreground context.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| {
                    runtime.request_background_terminate(agent_id)?;
                    if let Some(control) = &terminate_control {
                        control.request_yield();
                    }
                    Ok(json!({
                        "terminated": true,
                        "instruction": "Stop now without sending a final answer."
                    }))
                },
            ));
        }

        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            tools.push(Tool::new(
                "list_cron_tasks",
                "List configured cron tasks. Returns summaries including enabled state and next_run_at.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_cron_tasks(),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "get_cron_task",
                "Get full details for a cron task by id.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    runtime.get_cron_task(id)
                },
            ));

            let runtime = self.clone();
            let create_session = session.clone();
            tools.push(Tool::new(
                "create_cron_task",
                "Create a persisted cron task that later launches a main background agent. Provide each cron time field as a named argument; the host builds a seconds-first cron expression in the task timezone. Use timezone as an IANA name such as 'Asia/Shanghai'; if omitted, Asia/Shanghai is used. Example hourly at minute 0: cron_second='0', cron_minute='0', cron_hour='*', cron_day_of_month='*', cron_month='*', cron_day_of_week='*'. Example every minute: cron_second='0', cron_minute='*', cron_hour='*', cron_day_of_month='*', cron_month='*', cron_day_of_week='*'. Use cron_year for exact one-off calendar years when needed. The checker is optional: checker exit code 0 triggers the LLM, non-zero skips the run, and checker execution errors or timeouts still trigger the LLM.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "cron_second": {"type": "string", "description": "Seconds field. Examples: '0', '*/30', '*'."},
                        "cron_minute": {"type": "string", "description": "Minutes field. Examples: '0', '*/5', '*'."},
                        "cron_hour": {"type": "string", "description": "Hours field in the task timezone. Examples: '13', '9-17', '*'."},
                        "cron_day_of_month": {"type": "string", "description": "Day-of-month field in the task timezone. Examples: '17', '1,15', '*'."},
                        "cron_month": {"type": "string", "description": "Month field in the task timezone. Examples: '4', '1-12', '*'."},
                        "cron_day_of_week": {"type": "string", "description": "Day-of-week field in the task timezone. Examples: '*', 'Mon-Fri', '0'."},
                        "cron_year": {"type": "string", "description": "Optional year field in the task timezone. Example: '2026'."},
                        "timezone": {"type": "string", "description": "IANA timezone for these cron fields, e.g. 'Asia/Shanghai'. Defaults to 'Asia/Shanghai'."},
                        "task": {"type": "string"},
                        "enabled": {"type": "boolean"},
                        "checker_command": {"type": "string"},
                        "checker_timeout_seconds": {"type": "number"},
                        "checker_cwd": {"type": "string"}
                    },
                    "required": ["name", "description", "cron_second", "cron_minute", "cron_hour", "cron_day_of_month", "cron_month", "cron_day_of_week", "task"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let checker = parse_checker_from_tool_args(object)?;
                    runtime.create_cron_task(
                        create_session.clone(),
                        CronCreateRequest {
                            name: string_arg_required(object, "name")?,
                            description: string_arg_required(object, "description")?,
                            schedule: cron_schedule_from_required_tool_args(object)?,
                            timezone: optional_string_arg(object, "timezone")?
                                .unwrap_or_else(crate::cron::default_cron_timezone),
                            agent_backend: runtime.effective_agent_backend()?,
                            model_key: runtime.effective_main_model_key()?,
                            prompt: string_arg_required(object, "task")?,
                            sink: SinkTarget::Direct(create_session.address.clone()),
                            address: create_session.address.clone(),
                            enabled: object
                                .get("enabled")
                                .and_then(Value::as_bool)
                                .unwrap_or(true),
                            checker,
                        },
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "update_cron_task",
                "Update a cron task. To change timing, provide all named cron fields together: cron_second, cron_minute, cron_hour, cron_day_of_month, cron_month, cron_day_of_week, plus optional cron_year. Cron fields are interpreted in the task timezone. Use timezone to change the IANA timezone, enabled to pause or resume it, and clear_checker=true to remove the checker.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "cron_second": {"type": "string"},
                        "cron_minute": {"type": "string"},
                        "cron_hour": {"type": "string"},
                        "cron_day_of_month": {"type": "string"},
                        "cron_month": {"type": "string"},
                        "cron_day_of_week": {"type": "string"},
                        "cron_year": {"type": "string"},
                        "timezone": {"type": "string"},
                        "task": {"type": "string"},
                        "model": {"type": "string"},
                        "enabled": {"type": "boolean"},
                        "checker_command": {"type": "string"},
                        "checker_timeout_seconds": {"type": "number"},
                        "checker_cwd": {"type": "string"},
                        "clear_checker": {"type": "boolean"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    let checker = if object
                        .get("clear_checker")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        Some(None)
                    } else if object.contains_key("checker_command")
                        || object.contains_key("checker_timeout_seconds")
                        || object.contains_key("checker_cwd")
                    {
                        Some(parse_checker_from_tool_args(object)?)
                    } else {
                        None
                    };
                    runtime.update_cron_task(
                        id,
                        CronUpdateRequest {
                            name: optional_string_arg(object, "name")?,
                            description: optional_string_arg(object, "description")?,
                            schedule: optional_cron_schedule_from_tool_args(object)?,
                            timezone: optional_string_arg(object, "timezone")?,
                            agent_backend: None,
                            model_key: optional_string_arg(object, "model")?,
                            prompt: optional_string_arg(object, "task")?,
                            sink: None,
                            enabled: object.get("enabled").and_then(Value::as_bool),
                            checker,
                        },
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "remove_cron_task",
                "Remove a cron task permanently.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    runtime.remove_cron_task(id)
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "background_agents_list",
                "List tracked background agents with status, model, and token usage statistics.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_managed_agents(ManagedAgentKind::Background),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "get_agent_stats",
                "Get detailed status and token usage statistics for a tracked background agent or subagent by agent_id.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let agent_id = parse_uuid_arg(object, "agent_id")?;
                    runtime.get_managed_agent(agent_id)
                },
            ));
        }

        tools
    }
}

fn parse_session_plan_tool_args(object: &serde_json::Map<String, Value>) -> Result<SessionPlan> {
    let explanation = optional_string_arg(object, "explanation")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.chars().count() > 240 {
                value.chars().take(240).collect::<String>()
            } else {
                value
            }
        });
    let raw_steps = object
        .get("plan")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("plan must be an array"))?;
    if raw_steps.len() > 7 {
        return Err(anyhow!("plan must contain at most 7 steps"));
    }

    let mut in_progress_count = 0usize;
    let mut steps = Vec::with_capacity(raw_steps.len());
    for raw_step in raw_steps {
        let step_object = raw_step
            .as_object()
            .ok_or_else(|| anyhow!("each plan item must be an object"))?;
        let step = string_arg_required(step_object, "step")?.trim().to_string();
        if step.is_empty() {
            return Err(anyhow!("plan step must not be empty"));
        }
        let step = if step.chars().count() > 80 {
            step.chars().take(80).collect::<String>()
        } else {
            step
        };
        let status = match string_arg_required(step_object, "status")?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "pending" => SessionPlanStepStatus::Pending,
            "in_progress" => {
                in_progress_count += 1;
                SessionPlanStepStatus::InProgress
            }
            "completed" => SessionPlanStepStatus::Completed,
            other => {
                return Err(anyhow!(
                    "invalid plan status '{other}'; expected pending, in_progress, or completed"
                ));
            }
        };
        steps.push(SessionPlanStep { step, status });
    }

    if in_progress_count > 1 {
        return Err(anyhow!("plan may contain at most one in_progress step"));
    }

    Ok(SessionPlan {
        explanation,
        steps,
        updated_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_session_plan_tool_args;
    use crate::session::SessionPlanStepStatus;
    use serde_json::json;

    #[test]
    fn parses_valid_session_plan_tool_args() {
        let value = json!({
            "explanation": "normal progress update",
            "plan": [
                {"step": "Read context", "status": "completed"},
                {"step": "Patch code", "status": "in_progress"},
                {"step": "Run tests", "status": "pending"}
            ]
        });
        let plan = parse_session_plan_tool_args(value.as_object().unwrap()).unwrap();

        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[1].status, SessionPlanStepStatus::InProgress);
    }

    #[test]
    fn rejects_multiple_in_progress_plan_steps() {
        let value = json!({
            "plan": [
                {"step": "Patch code", "status": "in_progress"},
                {"step": "Run tests", "status": "in_progress"}
            ]
        });
        let error = parse_session_plan_tool_args(value.as_object().unwrap()).unwrap_err();

        assert!(error.to_string().contains("at most one in_progress"));
    }
}
