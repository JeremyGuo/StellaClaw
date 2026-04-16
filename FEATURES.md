# FEATURES.md

This file records the project features that should be protected by tests.

When adding a new non-bugfix capability, decide whether it is a feature. If it is, add or update an entry here and add focused tests that protect the behavior from future regressions.

## Features

### Interruptible Conversations

- A foreground conversation can be interrupted by a newer user message while the current agent turn is still running.
- When the running turn yields, the interrupting user message is inserted as the next conversation input instead of being lost or auto-resumed past.
- Interrupted follow-up text is marked distinctly so runtime-change notices and normal user-prefix logic do not accidentally rewrite it as ordinary context.
- Regression coverage should include pending interrupt delivery to the next foreground control, interrupted follow-up coalescing, and slash/control messages not leaking into user context.

### Parallel Conversations

- Different conversations must be able to run foreground work concurrently; a long-running turn in one conversation must not block normal message dispatch for another conversation.
- Foreground workers are scoped by conversation session key so messages from the same conversation remain ordered while messages from different conversations can progress independently.
- Interrupt, yield, compaction-phase, and pending-interrupt state must be scoped to the matching conversation session key only.
- Server maintenance work such as idle context compaction must not run inline on the incoming-message dispatch loop in a way that globally pauses new conversation dispatch.
- Regression coverage should protect per-conversation interrupt scoping and non-leaking conversation queues/control messages.

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

### Background Agent Delivery

- A main background agent final user-facing reply is delivered to the same foreground conversation that started or owns it.
- The same final reply is inserted into the Main Foreground Agent stable context as an assistant message so later foreground turns can see it without separate sink plumbing.
- If the foreground agent is currently running, background delivery waits for that foreground turn to finish or yield before inserting the reply.
- After inserting a background result, the runtime checks foreground context size and compacts when the normal compaction threshold is reached.
- Main Background Agents have a `terminate` tool that ends the background job silently without sending a user-facing reply or inserting foreground context.
- Regression coverage should protect final reply insertion, foreground-active waiting, compaction-after-insert behavior, and silent termination.

### DSL Orchestration Runtime

- DSL runs are exec-like long-running jobs with start/wait/kill lifecycle.
- Interrupting `dsl_start` or `dsl_wait` only interrupts the outer wait; the DSL job continues regardless of what it is doing internally.
- External DSL wait interruption does not cancel DSL code, DSL LLM calls, DSL tool calls, or child long-running tools.
- `dsl_kill` terminates the DSL job itself; child jobs continue by default unless explicit child killing is requested.
- DSL code runs in an isolated CPython worker, while DSL capabilities still flow through AgentFrame JSON-RPC callbacks.
- DSL syntax supports normal bounded Python expressions, assignments, `if` statements, f-strings, list/dict literals, attribute/index access, string methods, `type()`, `emit(text)`, `quit()`, `quit(value)`, `LLM()`, LLM handle calls, and `await tool({"name": "tool_name", "args": {"arg": value}})`.
- DSL LLM calls always use the same model as the `dsl_start` caller; model switching with `LLM(model=...)` or `handle.config(model=...)` is not allowed.
- DSL expressions use CPython semantics for arithmetic, comparisons, boolean operators, conditional expressions, builtin pure functions, slices, modulo, floor division, string operations, and JSON-like list/dict manipulation.
- DSL `select` accepts choices only as the second positional argument: `await handle.select("prompt", ["A", "B", "C"])`.
- DSL `emit(text)` appends visible DSL output; when no `quit(value)` is provided, the final result is emitted text joined by newlines, or `0` when nothing was emitted.
- DSL tool call results are assignable values and returned JSON can be accessed with normal Python dict/list syntax for later steps.
- DSL code must reject explicit or implicit loops, including `for`, `while`, `async for`, comprehensions, and generator expressions.
- DSL code must also reject imports, functions, classes, lambdas, private `_` names/attributes, recursive DSL tool calls, and other constructs that make execution unbounded, unsafe, or hard to reason about.
- DSL runtime enforces hard limits for runtime duration, LLM calls, tool calls, emitted messages, code size, and output size.
- DSL tool calls must use the single-dict `tool({"name": ..., "args": {...}})` shape and must go through the normal tool registry, preserving existing permissions, sandboxing, remote/workpath behavior, lifecycle semantics, and output limits.
- DSL cannot directly mutate canonical system prompts; dynamic prompt changes still use system notifications and compaction-time prompt rebuild.
