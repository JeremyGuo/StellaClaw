# FEATURES.md

This file records the project features that should be protected by tests.

When adding a new non-bugfix capability, decide whether it is a feature. If it is, add or update an entry here and add focused tests that protect the behavior from future regressions.

## Features

### Interruptible Conversations

- A foreground conversation can be interrupted by a newer user message while the current agent turn is still running.
- When the running turn yields, the interrupting user message is inserted as the next conversation input instead of being lost or auto-resumed past.
- Interrupted follow-up text is marked distinctly so runtime-change notices and normal user-prefix logic do not accidentally rewrite it as ordinary context.
- Regression coverage should include pending interrupt delivery to the next foreground control, interrupted follow-up coalescing, and slash/control messages not leaking into user context.

### Tool Execution Lifecycle

- Tools have only two execution modes:
  - immediate: the tool returns promptly and does not require turn-level waiting semantics.
  - interruptible: the tool may wait, but must return promptly when a newer user message interrupts the turn or timeout observation asks it to yield.
- Long-running stateful work should use an explicit lifecycle:
  - start: create the job/process/task, and optionally wait for completion when that reduces API round trips.
  - wait/observe/progress: check or wait for completion; waits must be interruptible and must not kill the underlying job merely because the user interrupted the agent turn.
  - kill/cancel: explicitly terminate the job/process/task.
- `exec_start` follows the start shape while supporting default wait-until-complete; `exec_wait` is interruptible; `exec_kill` terminates explicitly.
- Download/image-style background jobs follow start + wait/progress + cancel where applicable.
- Regression coverage should protect tool execution mode annotations and the start/wait/terminate schemas for long-running tool families.

### System Prompt Refresh Semantics

- Fixed/static system prompt content is checked on every new turn and must be rewritten immediately when it changes.
- Dynamic system prompt components from both agent host and agent frame must not rewrite the canonical system prompt immediately during normal turns.
- When dynamic components change, the user-facing turn receives system notifications that describe the change instead of invalidating the existing canonical prompt prefix.
- After context compaction, the full current system prompt is rebuilt and persisted, including the latest dynamic components.
- Regression coverage should protect static prompt immediate rewrite, dynamic component notifications, and compaction-only persistence of dynamic prompt content.
