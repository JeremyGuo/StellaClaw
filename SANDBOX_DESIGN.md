# Sandbox Design

## Goals

- Give Main Agent and Sub-Agent a real sandboxed working area instead of a shared writable `rundir`.
- Make long-lived artifacts first-class `projects`, not loose files in a workspace.
- Support collaborative access: many readers, at most one writer.
- Keep project ownership and lifecycle explicit through tools instead of prompt conventions.
- Preserve enough metadata and recovery state that crashes, timeouts, and preemption do not silently corrupt shared work.

## Non-Goals

- This is not a hostile multi-tenant security boundary.
- This does not replace OS/container sandboxing.
- This does not try to make all project operations fully distributed; one local host is the source of truth.

## Design Summary

The recommended design is a hybrid:

- Keep **OpenClaw-style per-agent isolation** as the base model.
- Add a **Project Manager layer** for durable shared artifacts.
- Add a **Project Maintainer Agent** for automatic organization and summarization.

In other words:

- Agents should run in their own scratch workspace.
- Durable shared state should live in `projects/`.
- Agents should never treat the shared durable store as their default temp directory.

This is better than the current `shared rundir` model, and better than plain OpenClaw when you want explicit project collaboration. It is worse than OpenClaw if implemented only as soft conventions without a real lock and mount manager.

## Comparison With OpenClaw

OpenClaw's public design is simpler:

- One agent gets one workspace.
- Sandboxing can move tools into a sandbox workspace.
- Multi-agent mode usually means multiple isolated workspaces, not one shared project store.

References:

- Agent Runtime: https://docs.openclaw.ai/concepts/agent
- Agent Workspace: https://docs.openclaw.ai/agent-workspace
- Multi-Agent Routing: https://docs.openclaw.ai/concepts/multi-agent
- Sandboxing: https://docs.openclaw.ai/gateway/sandboxing
- Security: https://docs.openclaw.ai/gateway/security

OpenClaw is better at:

- Simplicity
- Predictable isolation
- Lower coordination complexity
- Easier failure recovery

This proposed design is better at:

- Shared long-lived project memory
- Explicit collaboration between agents
- Controlled writer ownership
- Durable organization above raw files

So the answer is:

- **Better than OpenClaw for collaborative artifact management**
- **Worse than OpenClaw for simplicity and implementation risk**

The design is only worth it if we commit to hard enforcement, not just prompt-level rules.

## Core Abstractions

### 1. Agent Scratch Workspace

Every running agent gets its own private workspace:

- `sandboxes/agents/<agent_id>/workspace/`

This is the agent's actual cwd.

The scratch workspace contains:

- private temp files
- mounted projects
- local intermediate outputs
- agent-private caches

The agent should not write directly into the durable project store except through mounted views.

### 2. Durable Project Store

Every project is durable and addressable:

- `projects/<project_id>/`

Recommended contents:

- `PROJECT.md` or `README.md`
- `ABSTRACT.md`
- project files

Project content directories should not expose internal control metadata to ordinary agents.
Metadata must live outside the project tree and be maintained only by the Project Manager layer.

Suggested external project registry record:

```json
{
  "id": "project-id",
  "name": "human-readable-name",
  "description": "one-sentence summary",
  "created_at": "2026-03-31T00:00:00Z",
  "updated_at": "2026-03-31T00:00:00Z",
  "state": "active",
  "writer_agent_id": null,
  "writer_epoch": 0
}
```

Recommended storage locations:

- `sandbox/projects.json`
- or a dedicated SQLite table

Use a stable project id internally. Human-readable `name` should be unique at the UI/tool level, but identity should not depend on directory renames alone.
Agents should see project metadata only through project tools, not by directly opening registry files.

### 3. Mount Table

A central mount/lease registry is required:

- `sandbox/mounts.json` is acceptable to start
- SQLite is better if concurrency grows

Each mount record should track:

- `project_id`
- `agent_id`
- `mode`: `read_only` or `read_write`
- `state`: `active`, `preempting`, `revoked`, `released`
- `epoch`
- `created_at`
- `heartbeat_at`
- `wait_queue`

This registry is the source of truth. Filesystem state is not.

## Project Manager Agent

This is the control-plane abstraction exposed as tools.

### Required tools

- `project_search`
- `project_create`
- `project_modify`
- `project_remove`
- `project_load`

The tool list in the request included `project_load` twice. The second one should remain `project_load`; no extra alias is needed.

### Tool semantics

#### `project_search`

Input:

- natural-language query

Output:

- project id
- name
- one-sentence description
- state
- maybe relevance score

This should search the external metadata registry first, not scan full project contents by default.

#### `project_create`

Input:

- `name`
- `description`

Behavior:

- create durable project metadata
- create empty project directory
- initialize `README.md` and `ABSTRACT.md`

#### `project_modify`

Input:

- `project_id` or unique `name`
- optional new `name`
- optional new `description`

Behavior:

- metadata update only
- reject invalid renames or name collisions
- if name changes, update the external registry first and then perform the durable directory rename under the same lock or transaction

#### `project_remove`

Input:

- `project_id`

Rules:

- only agents with write permission can request removal
- removal is tombstone-first, not immediate physical delete

Recommended lifecycle:

1. mark project as `deleting`
2. hide from normal `project_search`
3. reject new loads
4. wait until all active mounts are gone
5. physically delete

This is safer than immediate recursive deletion.

#### `project_load`

Input:

- `project_id` or `name`
- `mode`: `read_only` or `read_write`
- `wait`: `true` or `false`
- optional mount path override

Behavior:

- mount project into current agent scratch workspace
- read-only loads can coexist
- read-write load is exclusive

### Read-write lock rules

- many readers allowed
- at most one writer
- writer can be preempted only if not currently executing
- preemption increments the writer epoch
- a preempted writer is downgraded to read-only on next activation

### Blocking vs immediate failure

`project_load(mode=read_write)` should support:

- `wait=false`: fail immediately if writer lock unavailable
- `wait=true`: wait on a queue until granted or timeout

The queue should be persisted in the mount table, not kept only in RAM.

## Suggested Filesystem Layout

```text
<workdir>/
  projects/
    <project_id>/
      README.md
      ABSTRACT.md
      ...
  sandboxes/
    agents/
      <agent_id>/
        workspace/
          projects/
            <mounted-project-name> -> mount or overlay view
          tmp/
          output/
  sandbox/
    projects.json
    mounts.json
    events.jsonl
```

## Mount Implementation Options

### Option A: Symlink / bind mount for read-only

Good for:

- simple read-only access

Bad for:

- write isolation

### Option B: Direct writable bind mount

Bad default.

Why:

- no safe preemption
- hard to reason about stale writers
- weak rollback story

### Option C: Overlay / copy-on-write writable mount

Recommended.

For writable loads:

- lower layer = durable project
- upper layer = agent scratch writable layer
- work dir = agent scratch mount workdir

Commit path:

- when write lease is valid, merge upper layer into durable project

This gives better control for:

- preemption
- rollback
- crash cleanup
- partial write detection

If true overlayfs is too heavy initially, an acceptable v1 is:

- copy durable project into agent writable mount dir
- commit back under lock

That is slower, but far safer than shared writes.

## Preemption Semantics

Preemption is useful, but dangerous unless strict.

Recommended rules:

- only preempt a writer with no active execution lease
- mark current writer `preempting`
- deny further commit from stale writer epoch
- next time the old agent starts a turn, prepend a system notice:
  - list preempted projects
  - state that they are now mounted read-only

Never allow:

- stale writer to continue committing after epoch mismatch

This must be enforced in the commit path, not only in prompt text.

## Agent Execution Lease

To support safe preemption, agent runtime needs an execution lease table:

- when agent starts executing: `running=true`
- heartbeat every few seconds
- when agent finishes: release execution lease

Project write preemption is allowed only when:

- agent has no active execution lease
- or execution lease is stale past timeout

This avoids racing a currently running agent.

## Project Maintainer Agent

This is a background organizer, not the main control plane.

### Trigger

When an agent closes:

1. if there were new turns since the last maintainer pass
2. run context compression using the existing token-threshold logic
3. ignore time-based compaction gating
4. feed the compacted messages to Project Maintainer Agent
5. let it call `project_create` and `project_modify`

### Purpose

- discover durable projects from scratch outputs
- update project metadata
- keep `README.md` and `ABSTRACT.md` aligned
- avoid losing useful work when scratch workspaces disappear

### Important constraint

Project Maintainer should not directly mutate arbitrary mounted project files without taking a write lease first. It should use the same project tools as everyone else.

## Consistency Requirements

These are not optional.

### 1. Authoritative metadata store

Project metadata, lock state, and tombstones must be stored in one authoritative place outside the project content tree.
Ordinary agents must not rely on direct filesystem reads of internal metadata files.

### 2. Epoch-based write validation

Every writable mount must carry an epoch.

On commit:

- if local epoch != current project writer epoch
- reject commit as stale

### 3. Idempotent cleanup

On startup, recover:

- stale mount entries
- stale execution leases
- projects stuck in `deleting`
- sandboxes from dead agents

### 4. Crash-safe removal

Never `rm -rf` immediately when a writer requests delete. Use tombstone + finalizer.

### 5. Explicit commit boundary

If writable mounts are copy-on-write or overlay-based, writes are not durable until commit.

This is a feature, not a bug.

## Failure Modes To Design For

### Agent dies while holding read-write lease

Recovery:

- detect stale execution lease
- revoke write lease
- mark scratch mount recoverable or discardable

### Agent is preempted while inactive

Recovery:

- old writer epoch becomes stale
- next activation gets a system notice
- old scratch workspace becomes read-only or detached

### Project remove requested while readers still exist

Recovery:

- mark `deleting`
- deny new mounts
- final delete only after all mounts gone

### Project Maintainer makes bad inferences

Mitigation:

- restrict it to `project_create` and `project_modify`
- do not let it delete by default
- do not let it write internal registry files directly
- optionally require confidence threshold or human review for project creation in early versions

### Concurrent restarts

Mitigation:

- mount and project operations must take a file lock or DB transaction

## Why This Can Fail If Done Wrong

This design becomes worse than OpenClaw if:

- agents still share one writable cwd
- write ownership is only a prompt rule
- preemption does not invalidate stale commits
- removal is immediate delete
- mount state is only in memory

If that happens, the system gets all the complexity of project management with none of the safety.

## Recommended Version 1

Do not start with full overlayfs unless needed.

Start with:

- per-agent scratch workspace
- durable `projects/`
- persisted project registry
- explicit `project_load` read-only
- explicit `project_load` read-write with copy-to-scratch
- explicit commit-back with epoch check
- tombstone delete
- startup recovery of stale leases

This is implementable and testable.

## Recommended Version 2

After v1 is stable:

- overlayfs writable mounts
- background cleanup workers
- wait queues with fairness policy
- Project Maintainer Agent
- richer search index for projects

## Final Recommendation

This design is worth pursuing, but only as:

- **isolated scratch workspace + durable project store + enforced mount manager**

It is not worth pursuing as:

- **shared rundir + extra project tools**

OpenClaw's simpler isolation model is still better than a weak project system.

A strong project system can beat OpenClaw for collaborative coding workflows, but only if:

- writable access is lease-based
- commits are epoch-validated
- deletion is tombstoned
- crash recovery is explicit
- scratch and durable storage are clearly separated
