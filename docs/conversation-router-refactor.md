# Conversation Router Refactor Draft

This note records the current refactor direction for `stellaclaw` conversation handling. It is a discussion draft, not a finalized roadmap item.

## Current Problem

`ConversationRuntime` currently owns the serial conversation boundary, but it also directly contains many concrete responsibilities:

- Channel input handling.
- Session event polling and dispatch.
- Host service bridge routing.
- Foreground/background/subagent session lifecycle.
- Channel event rendering and progress feedback.
- Durable `ConversationState` persistence.
- Workspace, remote, sandbox, cron, memory, skill, and tool-binary coordination.

This makes the conversation layer more than a router. It becomes a large coordinator with service-specific logic embedded in the same file.

## Target Shape

Keep `Conversation` as the serial durable owner, but make it look more like an event router:

```rust
loop {
    select! {
        recv(channel_rx) -> msg => handle(ConversationEvent::Channel(msg)),
        recv(session_rx) -> msg => handle(ConversationEvent::Session(msg)),
        recv(service_rx) -> msg => handle(ConversationEvent::Service(msg)),
        recv(heartbeat_rx) -> _ => handle(ConversationEvent::Heartbeat),
    }
}
```

The important distinction is that `Conversation` should still own durable conversation state. It should not become a stateless pass-through. It should own:

- `ConversationState`.
- Session binding state for foreground, background, and subagent sessions.
- Workspace root and effective remote/sandbox/runtime binding.
- Active foreground progress state.
- Pending coordination responses and lifecycle transitions.
- State persistence and conversation update publication.

The router should normalize incoming events, call focused handlers, then apply explicit effects.

## Unified Event Type

A possible internal event shape:

```rust
enum ConversationEvent {
    Channel(IncomingConversationMessage),
    Session {
        agent_id: Option<String>,
        session_type: SessionType,
        event: SessionEvent,
    },
    Service(ServiceEvent),
    Cron(CronTaskRecord),
    Heartbeat,
    Shutdown {
        reason: &'static str,
        ack_tx: Sender<()>,
    },
}
```

Notes:

- Existing external `ConversationCommand` can be converted into `ConversationEvent`.
- Session event receivers can be forwarded into a single internal event channel.
- Heartbeat can replace the current periodic polling/keepalive path.
- Service events are for async service completions or future service-local message flow.

## Unified Effects

Handlers should return effects rather than directly mutating every external boundary:

```rust
enum ConversationEffect {
    SendSession {
        target: SessionTarget,
        request: SessionRequest,
    },
    EmitChannel(ChannelEvent),
    PersistState,
    PublishConversationUpdated,
    RestartForegroundSession,
    StopManagedSessions {
        reason: String,
    },
    StartManagedSession {
        kind: ManagedSessionType,
        task: String,
        model: Option<String>,
    },
    ResolveHostCoordination {
        target: SessionTarget,
        response: ConversationBridgeResponse,
    },
}
```

`ConversationRuntime` applies effects serially. This preserves ordering and durable ownership while moving decision logic out of the runtime monolith.

## Services

Conversation-owned services should move into focused modules:

```text
stellaclaw/src/conversation/
  store.rs
  events.rs
  effects.rs
  router.rs
  services/
    core.rs
    managed_session.rs
    skill.rs
    cron.rs
    memory.rs
    tool_binary.rs
```

Service handlers should avoid taking `&mut ConversationRuntime` directly. Prefer a narrow context plus explicit effects:

```rust
trait ConversationService {
    fn handle(
        &self,
        request: ServiceRequest,
        context: ServiceContext,
    ) -> Result<Vec<ConversationEffect>>;
}
```

For example:

- Memory service returns a `ResolveHostCoordination` effect with the tool result.
- Managed session service can return `StartManagedSession`, `StopManagedSessions`, or `ResolveHostCoordination`.
- Skill service can perform store operations through a narrow store/context API and return a structured coordination response.

## Implementation Path

Suggested sequence:

1. Add `events.rs` and `effects.rs`.
2. Convert current `ConversationCommand` into `ConversationEvent`.
3. Introduce a single internal conversation event channel.
4. Forward session event receivers into that event channel.
5. Replace `pump_session_events + recv_timeout + keepalive` with one event loop.
6. Keep existing handler functions initially, but call them through the router.
7. Move host service handlers into `conversation/services/*` one service at a time.
8. Move channel output rendering/progress feedback into an outbox-like module after routing is stable.

This keeps the migration incremental and avoids changing all service behavior at once.

## Open Questions

- Should service handlers be fully synchronous under the conversation owner, or should slow services return async completions through `ConversationEvent::Service`?
- What is the right boundary between `ConversationEffect::EmitChannel` and a later `outbox` module?
- Should Web updates such as nickname changes become channel events into the conversation owner instead of direct state writes?
- How much state should `ServiceContext` expose without recreating `&mut ConversationRuntime` under another name?
- Should `SessionTarget` use `SessionType + agent_id`, or a stronger typed enum for foreground/background/subagent?

