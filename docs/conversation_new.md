# Conversation New Workbook

This workbook tracks the new conversation/service design and implementation progress. It is intentionally separate from `ROAD_MAP.md`: this file records working decisions, open questions, and incremental implementation state.

## Current Direction

The new conversation implementation is a fresh service-tree runtime, not a migration of `stellaclaw/src/conversation.rs`.

Core idea:

```text
Conversation Kernel Thread
  - owns the dynamic router table
  - owns service lifecycle handles
  - receives ServiceOutput from services
  - dispatches ServiceCall by concrete ServiceAddr
  - handles reserved calls to kernel

Service Threads
  - own their own state and persistence
  - receive ServiceCall on an inbox
  - send ServiceOutput back to the kernel
  - stop through a kernel-created stop channel
```

Host startup order:

```text
HostRuntime
  1. starts global managers/resources
  2. restores and starts every ConversationKernel
  3. starts Channel adapters after conversation routing is ready
  4. keeps only host-level lifecycle/registry responsibilities in the outer loop
```

## Decisions

- `ServiceCall` stays minimal: `source`, `target`, `payload`.
- `payload` protocol is owned by the target service; method/type fields, if needed, live inside payload.
- `ServiceAddr` is assigned by the creator/kernel, not by the service implementation.
- `kernel` is a reserved local service address handled by the kernel thread itself.
- Dynamic routing is a concrete `ServiceAddr -> service inbox` table. No alias/semantic route table for the first version.
- `channel/<id>` is the channel endpoint paired with `agent/foreground/<id>`. `channel/main` is the default main channel.
- All channel-facing requests must go through the `channel` service protocol. Kernel and other services must not directly send Web/Telegram events.
- Platform ingress is owned by `ChannelService` through its own receiver/adapter; it is not a kernel input path.
- Channel ingress supports normalized `ChatMessage` input, foreground context/status queries, foreground session create/delete, cancel/continue/compact, host-coordination resolution, and conversation-wide runtime config updates. Platform adapters should normalize raw Web/Telegram payloads into `ChannelIngress` instead of calling AgentSession/Kernel directly.
- Channel ingress also accepts typed workspace and terminal requests and forwards them to `WorkspaceService` / `TerminalService`; platform adapters receive typed channel events instead of reading workspace files or owning PTY managers directly.
- Incoming channel messages that contain `data:` `FileItem` values are first sent to `WorkspaceService` for materialization under `.stellaclaw/attachments/incoming`, then Channel forwards the resulting standard `ChatMessage` to the foreground AgentSession.
- New agent sessions are created by sending a typed kernel protocol payload to target `kernel`.
- Any service may request agent creation through `kernel`; kernel enforces policy.
- Background and subagent services use concrete dynamic addresses such as `agent/background/<id>` and `agent/subagent/<id>`.
- `background_agent_start` host bridge requests are handled by the parent AgentSession. They create a Kernel-managed background AgentSession with the parent as event sink; successful background completion is delivered to the channel and reinserted into the parent session as an actor message.
- Global shared resources are injected as refs/clients when a service is created; they are not modeled as a `Host` service scope for now.
- `AgentSessionService` owns session requests, context snapshots, lifecycle state, and raw session events.
- `AgentSessionService` maintains the current task plan in memory only. `QueryStatus` returns the current plan, and terminal session events clear it; `service_state.json` intentionally does not persist plan state.
- `AgentSessionService` persists only lightweight service state in its own storage (`service_state.json`): kind, binding, lifecycle state, active turn id, last error, message count, and last message snapshot. Full history/session persistence remains owned by core SessionActor.
- `AgentSessionService` now has explicit mapping helpers between the new service protocol and core `SessionRequest` / `SessionEvent`.
- `AgentSessionService` intercepts `subagent_start` / `subagent_kill` / `subagent_join` host bridge requests. Tool-created subagents use Kernel dynamic session creation with the parent AgentSession as event sink, so the parent can maintain subagent lifecycle state and resolve join requests without exposing subagent events directly to the channel. Pending subagent joins are interrupted and resolved when a new user message or explicit cancel reaches the parent AgentSession.
- When Kernel provides an agent server path and a chat-capable model, `AgentSessionService` starts a real `agent_server`, initializes core `SessionActor`, forwards service requests as core `SessionRequest`, and projects core `SessionEvent` back to the configured event sink. Without an agent server path it keeps the skeleton runner for isolated protocol tests.
- Kernel owns the runtime AgentSession launch configuration view and injects a snapshot when mounting an AgentSession. This view is built from existing concepts only: conversation root, optional agent server path, optional session profile, model map, session defaults, memory enablement, tool remote mode, sandbox override, and reasoning effort. `workspace_root` remains a core launch parameter, but Kernel derives it from `conversation_root` plus `tool_remote_mode`; it is not a separately configurable runtime field. There is no separate `session_root`; core session persistence should use the conversation root-derived location.
- Runtime config updates are a single typed Kernel request, `UpdateRuntimeConfig`. The config applies to the whole conversation and affects future AgentSession creation; Kernel owns applying and persisting the update under workdir-level `services/<conversation_id>/runtime_config.json`.
- Kernel injects a runtime config snapshot into `WorkspaceService` when mounting it, then broadcasts later runtime config updates to WorkspaceService as well as AgentSession. WorkspaceService owns interpreting that snapshot for local overlay / FixedSSH workspace behavior.
- `WorkspaceService` is the workspace view boundary. Its protocol includes list, read, write, delete, and move operations with a `WorkspaceTarget` selector: `auto`, `local_workspace`, `local_overlay`, or `remote`. In Selectable mode, `auto` resolves to the local conversation workspace. In FixedSSH mode, `auto` resolves `.stellaclaw/**` to the local overlay and all other paths to the fixed remote workspace. Cross-boundary moves are rejected.
- `TerminalService` owns PTY lifecycle, terminal IO, replay, attach/detach subscriptions, and runtime-mode cleanup. Web/Telegram adapters must not directly own terminal managers; they should project terminal HTTP/WebSocket operations into typed terminal service requests and events.
- `ChannelService` owns platform projection: platform ingress, foreground context query forwarding, terminal/workspace request forwarding, and conversion of AgentSession events into channel-facing events/status.
- `ChannelService` can emit typed `ChannelEvent` values to an owned platform adapter queue; status updates remain available for kernel logs/tests, but Web/Telegram should consume channel events instead of reverse-engineering generic status labels.
- Kernel foreground creation errors are projected as `ChannelEvent::Error`, so platform adapters can report failed foreground lifecycle commands without inspecting kernel internals.
- `channel/<id>` rejects session events from a different foreground session id. Subagent events are allowed because subagent ownership is carried by AgentSession binding.
- `AgentSessionService` does not infer where to send output from address shape. Kernel creates each AgentSession with explicit binding metadata.
- AgentSession binding currently includes a single `event_sink` and optional `parent_addr`; foreground sinks to its paired channel, Cron-owned background sinks to `cron`, and subagents inherit the parent's sink.
- Cron tasks record both the registering session address and the channel address they should report back to.
- `CronRequest::TriggerTaskNow` is the explicit manual/debug trigger for a registered task. Real scheduling should call the same internal path from CronService's own scheduler.
- Cron schedule is typed as `CronSchedule::Manual`, `CronSchedule::IntervalSeconds { seconds }`, or `CronSchedule::CronExpression { expression, timezone }`. Interval tasks and cron-expression wall-clock tasks are triggered internally by `CronService` and reuse the same background execution path as `TriggerTaskNow`.
- Cron scheduling is non-backfilling. The service computes the next future wakeup from current time on each loop; persisted tasks restored after downtime schedule from now forward and do not execute missed runs.
- Conversations are eager runtime instances, not message-triggered cache entries. Host startup restores every known ConversationKernel before accepting channel ingress so CronService timers and foreground/background AgentSessions have a live owner immediately.
- Standard conversation bootstrap includes `channel/main` and `agent/foreground/main`, plus cron, memory, skill, tool_binary, workspace, terminal, and status services.
- HostRuntime keeps the `channel/main` ingress sender and is the only reader of the `ChannelService` event receiver for each conversation. It fans out typed channel events to Web/Telegram subscribers so platform code never competes for the same receiver.
- Cron-owned background AgentSessions set `event_sink=cron`, so Cron receives session events, records run status, and decides whether to forward results to the foreground session.
- Cron task payload is now typed as `CronTaskPayload::Prompt { prompt, output_policy }`; `forward_result_to_foreground` wraps successful background output into a user-role actor message for the foreground session, while `store_only` only records the run result/status inside Cron.
- Cron validates task registration before persistence: `task_id` must be non-empty, interval schedules must be finite and positive, and foreground/channel ids must match when provided by local addresses.
- Cron tracks pending Kernel background creation requests and marks the corresponding task run failed if Kernel rejects AgentSession creation.
- Cron does not start a second run for the same task while that task has a pending or active background run. Manual trigger returns rejected; interval trigger skips that occurrence.
- Cron list/get/update/remove/trigger requests are owner-scoped for AgentSession callers. Each session can only see and mutate tasks it registered.
- When Kernel deletes an AgentSession service, it sends Cron a kernel-origin `DisableTasksForOwner` call before stopping the session; Cron disables matching tasks and persists the disabled state.
- CronService persists registered task state to its own `tasks.json` under service storage. Runtime wakeups are derived from current wall-clock time plus in-memory interval anchors; no `next_due` queue state is persisted.

## Service Inventory

- `channel/<id>`: a foreground-specific outward platform boundary; renders/sends channel-facing output and receives normalized channel input for `agent/foreground/<id>`.
- `agent/foreground/<id>`: a foreground AgentSession. `agent/foreground/main` is the default main foreground, and one conversation may mount multiple foreground sessions.
- `agent/background/<id>`: a dynamic AgentSession for long-running or scheduled background work.
- `agent/subagent/<id>`: a dynamic AgentSession for delegated bounded work.
- `cron`: owns scheduled task state and asks `kernel` to create background agents when needed.
- `memory`: owns long-term memory search/write/update/delete state and protocol.
- `MemoryService` uses the existing Memory v1 backend internally instead of defining a second storage format. Conversation memory stays under `.stellaclaw/memory_v1/conversation`, while user/public memory stay under workdir `rundir/memory_v1/{user,public}` and are reached through a workdir memory manager owned by the service.
- AgentSession intercepts core `memory_search` / `memory_write` / `memory_update` / `memory_delete` host bridge requests and routes them to `MemoryService`; MemoryService responses are converted back into `ConversationBridgeResponse` with the legacy memory tool JSON result shape.
- `skill`: owns runtime skill persistence and workspace synchronization semantics.
- `SkillService` reuses the existing runtime skill store at `rundir/.stellaclaw/skill`; on startup it reconciles runtime skills into the current conversation workspace under `.stellaclaw/skill`, and its typed protocol supports reconcile, persist create/update/delete, list, and load.
- AgentSession intercepts core `skill_create` / `skill_update` / `skill_delete` host bridge requests and routes them to `SkillService`; SkillService responses are converted back into `ConversationBridgeResponse` with the legacy skill tool JSON result shape.
- `tool_binary`: owns managed tool binary ensure/download/upload protocol for this conversation.
- `ToolBinaryService` reuses the existing global ToolBinaryManager and exposes a typed ensure protocol. It does not receive runtime sandbox updates; managed tool directories are treated as a stable mounted boundary.
- AgentSession intercepts core `tool_binary_ensure` host bridge requests and routes them to `ToolBinaryService`; ToolBinaryService responses are converted back into the core `ToolBinaryEnsureResponse` JSON shape.
- AgentSession intercepts core cron host bridge requests (`cron_tasks_list`, `cron_task_get`, `cron_task_create`, `cron_task_update`, `cron_task_remove`) and routes them to CronService with the AgentSession address as the owner boundary.
- AgentSession handles `background_agents_list` from its own tracked child-agent state for parent-bound background agents and subagents.
- `workspace`: owns workspace and attachment-related conversation state.
- `terminal`: owns local/fixed-SSH terminal process lifecycle, terminal input/resize/replay, and attach output subscriptions.
- `status`: aggregates service status for channel queries.

## Initial File Layout

```text
stellaclaw/src/conversation_new.rs
stellaclaw/src/services/
  mod.rs
  agent_session.rs
  channel.rs
  cron.rs
  memory.rs
  noop.rs
  skill.rs
  tool_binary.rs
  workspace.rs
  terminal.rs
  status.rs
stellaclaw/src/service_protos/
  mod.rs
  agent_session.rs
  channel.rs
  cron.rs
  memory.rs
  kernel.rs
  skill.rs
  tool_binary.rs
  workspace.rs
  terminal.rs
  status.rs
```

`services/*.rs` contains service runtime logic.

`service_protos/*.rs` contains typed payloads and call builders so services do not hand-roll raw JSON.

## Progress

- [ ] Research Codex / codex-rs harness behavior before changing the SessionActor execution loop. The target is a near 1:1 Codex CLI-compatible harness, not another local-only approximation.
  - [ ] Streaming messages are the top priority: understand how Codex emits assistant text, reasoning summaries, tool call starts/deltas/results, turn boundaries, and preamble/status updates.
  - [ ] Map timeout and fault-tolerance behavior across provider requests, stream reads, tool starts, tool output polling, bridge calls, remote SSH calls, downloads/uploads, and service shutdown.
  - [ ] Make requests interruptible end to end: provider request, stream reader, tool batch, individual tool execution, bridge/service calls, and pending joins must all have a coherent cancel path.
  - [ ] Remove the need for `ImmediateTool`; every tool should be interruptible and scheduled through one consistent tool execution path.
  - [ ] Harden remote-mode tools substantially, especially shell, file, patch, workspace, terminal, and managed binary behavior over SSH.
  - [ ] Bring our system prompt closer to Codex. First inspect how codex-rs constructs and layers its own prompts, then decide what belongs in Stellaclaw-specific overlays.
  - [ ] Bring tool names, schemas, execution semantics, streaming events, error shapes, and result rendering as close to Codex as practical.

- [x] Model foreground AgentSession addresses as `agent/foreground/<id>` instead of a single hard-coded foreground service; `main` remains the default.
- [x] Model channel addresses as `channel/<id>` paired one-to-one with `agent/foreground/<id>`.
- [x] Let `ChannelService` derive the target foreground from its own channel id, while keeping platform ingress ownership inside `ChannelService`.
- [x] Define a richer AgentSession protocol for enqueueing messages, cancel/continue/compact, host coordination resolution, context queries, status, shutdown, and session events.
- [x] Define Channel protocol boundaries for explicit delivery, session event projection, generic status/error, and foreground context query ingress.
- [x] Extend Cron protocol so registered tasks retain `registered_by`, `channel_addr`, optional foreground session address, schedule, and payload.
- [x] Move AgentSession output routing from address inference to explicit kernel-created binding metadata.
- [x] Rename manual cron execution to `TriggerTaskNow` and connect it to background AgentSession creation plus system-origin payload dispatch.
- [x] Add CronService-owned schedule timers for manual and interval task registrations.
- [x] Route Cron-owned background session terminal events back to CronService and record run completion/failure.
- [x] Simplify AgentSession binding to a single event sink; Cron forwards background results to the foreground session as a user-role actor message.
- [x] Persist Cron task registry/status in service storage and add task status query protocol.
- [x] Replace Cron's raw JSON task prompt payload with typed `CronTaskPayload` and explicit output policy.
- [x] Replace Cron's raw JSON schedule with typed `CronSchedule`.
- [x] Reject invalid Cron task registrations before they are persisted.
- [x] Mark Cron runs failed when Kernel rejects background AgentSession creation.
- [x] Reject or skip Cron triggers while the same task is already running.
- [x] Add owner-scoped Cron task list/get/create/update/remove bridge tools through AgentSession -> CronService.
- [x] Have Kernel disable a deleted AgentSession's Cron tasks before removing the session service.
- [x] Add AgentSession-owned `background_agents_list` bridge responses from tracked child-agent state.
- [x] Replace persisted Cron `next_due` style scheduling with non-backfilling runtime wakeup calculation and support cron-expression wall-clock timers.
- [x] Inject Kernel runtime config snapshots into WorkspaceService and broadcast runtime config updates to it.
- [x] Add WorkspaceService OverlayFS protocol and handlers for list/read/write/delete/move across Selectable local workspace, FixedSSH remote workspace, and local `.stellaclaw/**` overlay paths.
- [x] Route ChannelService workspace ingress through WorkspaceService and project typed workspace responses back as channel events.
- [x] Materialize `data:` file items from incoming channel messages through WorkspaceService before forwarding them to AgentSession.
- [x] Add TerminalService protocol/runtime for list/get/create/terminate/input/resize/replay/attach/detach, backed by existing PTY management and receiving Kernel runtime config broadcasts.
- [x] Route ChannelService terminal ingress through TerminalService and project typed terminal responses back as channel events.
- [x] Replace MemoryService skeleton with typed search/write/update/delete/prompt-context/maintain protocol backed by existing Memory v1 stores and workdir memory manager.
- [x] Route AgentSession memory host bridge requests through MemoryService and resolve them back into core conversation bridge responses.
- [x] Replace SkillService skeleton with runtime skill reconcile, typed persist/list/load protocol, and workspace sync backed by existing skill validation/copy helpers.
- [x] Route AgentSession skill create/update/delete host bridge requests through SkillService and resolve them back into core conversation bridge responses.
- [x] Replace ToolBinaryService skeleton with typed ensure protocol backed by the existing global ToolBinaryManager.
- [x] Route AgentSession tool_binary_ensure host bridge requests through ToolBinaryService and resolve them back into core conversation bridge responses.
- [x] Route AgentSession subagent_start/subagent_kill/subagent_join host bridge requests through Kernel/AgentSession service calls and preserve the legacy subagent tool result shape.
- [x] Interrupt pending subagent joins on new user messages or CancelTurn so foreground interaction does not leave a bridge call waiting on stale work.
- [x] Route AgentSession background_agent_start host bridge requests through Kernel dynamic background AgentSession creation and project completed background output back to channel plus parent AgentSession context.

- [x] Capture design decisions in `docs/conversation_new.md`.
- [x] Define `ServiceAddr`, `ServiceCall`, `ServiceOutput`, `ServiceStop`, and `ConversationService`.
- [x] Implement a kernel-owned dynamic route table using concrete `ServiceAddr`.
- [x] Implement service start/stop handles with kernel-created inbox/outbox/stop channels.
- [x] Add the real kernel main loop with shutdown input and service output `select`.
- [x] Add typed kernel protocol for `create_agent_session`, `stop_service`, and `list_services`.
- [x] Add typed channel protocol and enforce channel output through `channel` calls.
- [x] Give `ChannelService` its own optional ingress receiver for platform input ownership.
- [x] Add a minimal `AgentSessionService` skeleton.
- [x] Add a minimal `ChannelService` skeleton.
- [x] Add typed `ChannelEvent` output for platform adapters and reject foreign foreground session events.
- [x] Expand `ChannelIngress` into the platform-facing command surface for message, context/status, foreground lifecycle, turn control, host coordination, and runtime config update requests.
- [x] Add lightweight AgentSession service-state persistence and core SessionRequest/SessionEvent conversion boundaries.
- [x] Move AgentSession launch config ownership into Kernel and derive workspace root during AgentSession mount.
- [x] Add a generic `NoopService` for placeholder services.
- [x] Persist service manifests and per-service storage roots.
- [x] Restore mounted skeleton services from manifest with `ConversationKernel::open`.
- [x] Add skeleton files and typed protocols for `cron`, `memory`, `skill`, `tool_binary`, `workspace`, and `status`.
- [x] Add `mount_standard_services()` for the fixed conversation service set.
- [x] Include the default foreground AgentSession in standard conversation bootstrap.
- [x] Add a host-side ConversationHostRuntime that starts global managers and eagerly restores/spawns all existing ConversationKernel instances before channels are started.
- [x] Connect WebChannel to ConversationHostRuntime for `POST /conversations/{id}/foreground_sessions/main/messages`; HostRuntime is the single `ChannelService` event reader and fans events out to Web/Telegram subscribers.
- [x] Add `ConversationKernel::spawn` and `ConversationKernelHandle` for thread-based execution.
- [x] Bridge new `AgentSessionService` to `agent_server` / `SessionActor` when launch config is complete.
- [x] Route conversation-wide runtime config updates through Kernel instead of a ControlService.
- [ ] Replace remaining skeleton service bodies with real `status` implementation.
- [x] Add workdir `0.18 -> 0.19` upgrade to split legacy conversation state into workdir-level `services/<conversation_id>/` service storage.

## Open Questions

- Exact permission policy for `kernel.create_agent_session`.
- Whether cross-conversation addresses should be accepted in the first implementation or held until a dispatcher exists.
- How much service state the kernel should persist in the manifest versus leaving entirely inside service storage.
