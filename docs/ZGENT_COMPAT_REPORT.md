# ZGent Compatibility Report

## Core Position

This time, `zgent` should be treated as a **full agent kernel**, not as a helper tool backend.

That means:

- `agent_frame` and `zgent` are two alternative agent kernels
- `AgentHost` remains the outer shell
- `AgentHost` still owns:
  - channels
  - conversations
  - sessions
  - workspaces
  - memory / rollouts / snapshots
  - user-facing commands and AgentHost-only tools
- `zgent` owns the inner execution loop when `backend = "zgent"`

In short:

- `AgentHost` = outer shell
- `agent_frame` = local kernel
- `zgent` = external kernel

## Non-Goal

We are **not** trying to:

- distill `zgent` into our codebase
- reimplement its runtime locally
- reduce it to just a file/exec tool provider

`agent_host/src/zgent/` should only contain **compatibility/adaptation code** for talking to the real unpacked `./zgent` runtime.

## Desired Runtime Model

### When `backend = "agent_frame"`

Nothing special:

- current local kernel behavior stays as-is

### When `backend = "zgent"`

The turn should run like this:

1. AgentHost prepares the normal conversation/session/workspace state.
2. AgentHost chooses `zgent` as the active kernel.
3. AgentHost synchronizes required context into the `zgent` runtime.
4. `zgent` performs the inner agent execution:
   - model loop
   - native tool loop
   - native sandbox
   - native subagent runtime
5. AgentHost receives the result and persists it into the normal local session/memory structures.

So `zgent` is not just a tool executor. It is the thing performing the inner agent run.

## What Remains Local And Shared

Even when `backend = "zgent"`, these should remain local and backend-agnostic:

- `ConversationManager`
- `SessionManager`
- `PendingContinueState`
- conversation/session persistence
- conversation memory
- rollout summaries and rollout transcripts
- snapshots
- workspace ownership
- user-visible outgoing messages

This implies:

- there should be no separate `zgent` session store
- there should be no separate `zgent` memory authority
- there should be no separate `zgent` rollout/snapshot authority

Backend choice affects execution, not the identity of a session.

## Workspace Model

The workspace must remain the AgentHost workspace.

That means:

- AgentHost workdir/workspace layout remains authoritative
- `zgent` must execute against the current AgentHost workspace root
- `zgent` should not invent its own independent persistent workspace tree

Recommended interpretation:

- `AgentHost` owns workspace lifecycle
- `zgent` is pointed at the current workspace root
- `zgent` uses its own sandbox around that workspace

So:

- workspace authority = AgentHost
- execution sandbox = ZGent

## Sandbox Model

For `backend = "zgent"`, the default sandbox semantics should come from `zgent`, not from our current Bubblewrap child path.

This should not mean:

- wrapping `zgent` inside another execution sandbox for the same inner tool loop

Instead, it should mean:

- when the active kernel is `zgent`, let `zgent` own the inner execution isolation
- AgentHost still controls top-level policy and workspace binding

So this should be a **policy handoff**, not a double-sandbox stack.

## Tool Model

Because `zgent` is a full kernel, the default assumption should be:

- preserve `zgent` native tools when possible
- inject only AgentHost-specific tools on top

This is different from the earlier “replace selected tool implementations with `zgent`-backed versions” idea.

The preferred shape is:

`visible_tools = zgent_native_tools + agenthost_only_tools`

AgentHost-only tools include:

- `user_tell`
- workspace history / mount tools
- `memory_search`
- `rollout_search`
- `rollout_read`
- `shared_profile_upload`
- cron tools
- background-agent tools
- AgentHost-specific status / coordination tools

ZGent-native tools should remain owned by `zgent` itself whenever they already exist there.

## Subagent Model

For `backend = "zgent"`, subagents should also be treated as part of the `zgent` kernel.

So:

- actual subagent runtime should be delegated to `zgent`
- AgentHost should not emulate that runtime locally for the `zgent` backend

But AgentHost still owns one important policy:

- only models with `agent_model_enabled = true` may be exposed to `zgent` as candidate agent/subagent models

This gives us:

- helper models remain helper-only
- remote `zgent` subagent choices stay aligned with local product rules

## Context APIs: `ctx/get`, `ctx/set`, `ctx/compact`

These should be treated as **kernel integration points**, not as replacements for our persistence layer.

### `ctx/set`

Useful for pushing outer-shell state into the `zgent` kernel:

- workspace root
- cwd
- selected model key
- sandbox mode/policy
- local session identifiers
- compacted local summary if useful

### `ctx/get`

Useful for:

- reading back current remote kernel state
- debugging drift
- inspecting remote context/workspace binding
- reconciling interrupted runs

### `ctx/compact`

This should **not** become our primary compaction authority.

Instead:

- our compaction policy remains local
- our compaction output schema remains local
- our persisted compaction artifacts remain local
- `zgent` may be used as the execution engine for a compaction prompt/protocol

So the right shape is:

- local compaction contract
- remote compaction execution
- local persistence of the result

## Compaction Model

Compaction should not split into two competing systems.

Recommended rule:

- keep the current compaction trigger points
- keep the current structured compaction output format
- keep the current local memory/rollout persistence
- when `backend = "zgent"`, implement a `src/zgent/compaction.rs` adapter that runs the same compaction contract through `zgent`

So:

- compaction policy = ours
- compaction schema = ours
- compaction storage = ours
- compaction execution backend = `zgent` when selected

## What The Current Code Gets Wrong

The current compat path in [backend.rs](/home/jeremyguo/services/ClawParty2.0/agent_host/src/backend.rs):

- treats `zgent` like a text-only `/chat/completions` substitute
- strips multimodal inputs
- disables native search
- disables compaction
- bypasses checkpoint/event behavior
- hardcodes sampling settings

This is too shallow for a real kernel integration.

If `zgent` is a full kernel, the compat layer should not flatten it into a weak chat-completions shim.

## Recommended Code Layout

`agent_host/src/zgent/` should hold only integration code:

- `mod.rs`
  - public entrypoints for the `zgent` kernel integration
- `detect.rs`
  - detect unpacked `./zgent`
- `client.rs`
  - talk to the real `zgent` server/runtime APIs
- `kernel.rs`
  - run one turn through the `zgent` kernel
- `context.rs`
  - `ctx/get`, `ctx/set`, optional `ctx/compact` wrappers
- `tools.rs`
  - merge `zgent` native tools with AgentHost-only tools
- `workspace.rs`
  - bind current AgentHost workspace into `zgent`
- `sandbox.rs`
  - map AgentHost sandbox policy to `zgent` kernel semantics
- `subagent.rs`
  - delegate subagent lifecycle into `zgent`
- `compaction.rs`
  - execute the local compaction protocol through `zgent`

Notably absent:

- no `session.rs`
- no `memory.rs`

because those remain shared and local.

## Detection Model

The old submodule-specific detection is obsolete.

We should assume:

- users manually place `./zgent` under repo root
- if `./zgent` exists and has the expected runtime/server entrypoints, `zgent` backend is available
- otherwise it is unavailable

This should replace the current hardcoded probe for:

- `../zgent/crates/zgent-core/Cargo.toml`

## Revised Implementation Phases

### Phase 1: Kernel-aware foundation

- remove old submodule assumptions
- detect unpacked `./zgent`
- add `agent_host/src/zgent/` integration skeleton
- stop describing `zgent` as merely a tool backend in new code/docs

### Phase 2: Kernel entrypoint

- replace the current shallow chat-completions compat loop
- add a real `zgent` kernel entrypoint for one turn
- preserve normal AgentHost session/workspace/memory ownership around it

### Phase 3: Workspace + context binding

- bind AgentHost workspace into the `zgent` kernel
- implement `ctx/set`
- add optional `ctx/get` diagnostics

### Phase 4: Tool exposure merge

- preserve `zgent` native tools
- inject AgentHost-only tools
- keep model-facing tool catalog coherent

### Phase 5: Subagent + compaction integration

- delegate subagent runtime into `zgent`
- expose only `agent_model_enabled = true` models as remote agent/subagent candidates
- implement local-schema compaction through `zgent`

## Practical Recommendation

The right mental model is:

- **do not** treat `zgent` as a function call
- **do not** treat `zgent` as merely a tool provider
- **do** treat `zgent` as an external agent kernel
- **do** keep AgentHost as the durable outer shell

That is the cleanest way to gain `zgent`’s kernel/runtime/sandbox/subagent strengths without sacrificing our existing session, memory, and workspace model.
