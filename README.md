<div align="center">

# 🦀 ClawParty

**A Rust-based multi-agent host and next-generation agentic framework.**

[![CI](https://img.shields.io/badge/CI-cargo_test-green?logo=github-actions&logoColor=white)](#ci--cd)
[![Rust](https://img.shields.io/badge/Rust-1.75+-orange?logo=rust&logoColor=white)](#)
[![License](https://img.shields.io/badge/license-MIT-blue)](#)
[![Status](https://img.shields.io/badge/status-Active_Development-brightgreen)](#status)

*Agents as services, not scripts.*

</div>

---

## 📚 Docs

- [部署说明](docs/DEPLOY.md)
- [版本与发布说明](VERSION)

---

## 🏗️ Architecture

<div align="center">
  <img src="docs/imgs/architecture.png" alt="System Architecture" width="780" />
  <br />
  <em>Layered architecture: Channels → Conversation → Session → Agent Topology → Cron / Sink</em>
</div>

---

## 📦 Repository Structure

```
ClawParty/
├── agent_frame/    # 🔧 Standalone agent runtime (tools, skills, compaction)
├── agent_host/     # 🚀 Long-running service host (channels, conversations, sessions, cron, recovery)
├── zgent/          # 🔌 Compatibility layer for zgent-core (soft dependency)
└── docs/           # 📄 Documentation & diagrams
```

---

## 🔧 `agent_frame` — Agent Runtime

A self-contained Rust library and CLI binary for running a single LLM agent session.

### ✨ Capabilities

| Feature | Details |
|:--------|:--------|
| 🛠️ **Built-in tools** | File I/O (`file_read` / `file_write` / `edit` / `apply_patch`), repo exploration (`glob` / `grep` / `ls`), shell execution (`exec_start` / `exec_observe` / `exec_wait` / `exec_kill`), interruptible `web_fetch` / `web_search`, image workers (`image_start` / `image_wait` / `image_cancel`), and file download workers (`file_download_start` / `file_download_progress` / `file_download_wait` / `file_download_cancel`) |
| 📚 **Skill system** | `SKILL.md`-based skill discovery with `skill_load` / `skill_create` / `skill_update` tools |
| 🗜️ **Context compaction** | Automatic compression when context approaches model limits; tool-wait compaction only runs after all outstanding tool results are appended, with running `exec` / `file_download` state preserved in summaries |
| 📊 **Token accounting** | Tracks `cache_read` / `cache_write` / `cache_hit` / `cache_miss` per request |
| ⏱️ **Tool execution modes** | Built-in tools are either `immediate` or `interruptible`; interruptible tools expose their own timeout or wait parameters when needed |
| ⚡ **Parallel tool calls** | Multiple independent tool calls in the same round execute concurrently |
| 🛑 **Cancellation** | New user input requests a `yield` and interrupts interruptible tools; immediate tools finish quickly and return the turn to a safe boundary |
| 💾 **Checkpoint callback** | Optional callback fired after each tool round for mid-session state persistence |
| 🧩 **Host extensions** | `agent_host` can inject higher-level runtime tools such as `user_tell`, `subagent_start` / `subagent_join` / `subagent_kill`, and cron task management into the same tool loop |
| 🎛️ **Modes** | CLI binary (`run_agent`) or embedded library |

### 🔨 Build & Test

```bash
cargo test --manifest-path agent_frame/Cargo.toml
```

### ⚙️ Configuration

See [`agent_frame/example_config.json`](agent_frame/example_config.json) and [`agent_frame/example_openrouter_config.json`](agent_frame/example_openrouter_config.json).

> **Web search**: set either `native_web_search.enabled = true` (suppresses standalone `web_search` tool) or configure an external search provider under `external_web_search`. Only one should be active per model.

### 🧭 Tool Behavior Notes

- `exec_start`, `image_start`, and `file_download_start` start long-lived work and return immediately with an id.
- `exec_start` accepts `tty=true` when a command needs terminal semantics; the runtime allocates a PTY for that process while keeping the same `exec_observe` / `exec_wait` / `exec_kill` flow.
- `exec_wait`, `image_wait`, `file_download_wait`, `web_fetch`, and `web_search` are interruptible.
- `image_wait` and `file_download_wait` return immediately when interrupted and leave the background task running.
- `web_search` and `web_fetch` are cancelled when interrupted.
- `file_download` / `image` work is executed in worker subprocesses so it can survive across turns and be cancelled explicitly.
- Active `exec`, `file_download`, and alive subagents are appended to compaction summaries so later turns can keep reusing them.

---

## 🚀 `agent_host` — Service Host

The production layer that wraps `agent_frame` into a long-running, multi-channel service.

### 🔑 Key Features

| Feature | Description |
|:--------|:------------|
| 💬 **Conversation management** | Channel → Conversation → Session → Agent layered model; per-conversation agent backend, model, sandbox, and compaction settings |
| 💾 **Session persistence** | State survives process restarts; attachment lifecycle managed automatically |
| 📋 **Agent registry** | Background and subagent state persisted across restarts |
| ⏰ **Cron tasks** | Scheduled work with optional checker commands, stored durably; inherits creator conversation's model |
| 📡 **Background sinks** | Direct routing, broadcast topics, multi-target fan-out |
| 🔄 **Agent switching** | `/agent` to choose backend + model mid-conversation with automatic context compression |
| ⚙️ **Conversation-scoped controls** | `/think`, `/sandbox`, `/compact`, `/compact_mode`, `/set_api_timeout` |
| 📸 **Snapshots** | `/snapsave`, `/snapload`, `/snaplist` for global conversation state snapshots |
| 🔒 **Sandbox execution** | Three modes: `disabled`, `subprocess`, `bubblewrap` — per-conversation configurable via `/sandbox` |
| 📝 **Structured logging** | JSONL logs with per-agent / per-session / per-channel views |
| 📊 **Status & billing** | `/status` shows token usage, cache hits, price estimation, compression savings |
| 🔄 **Failure recovery** | Automatic handling of timeouts, upstream errors, restart scenarios, and `\continue` resume state |
| 🧵 **Parallel conversations** | Different conversations run concurrently; messages within the same conversation remain serialized and follow-up messages are coalesced |

---

## 💬 Conversation Lifecycle

```
Channel (e.g. Telegram)
  └─ Conversation (persistent: agent backend, model, sandbox mode, workspace)
       └─ Session (one active agent turn)
            └─ Agent Topology (main agent, subagents, background agents)
```

- `/new` starts a fresh conversation. If no agent is selected, prompts the user first.
- `/agent` first selects an agent backend, then selects the model for that backend with automatic context compression when switching.
- `/sandbox` toggles sandbox mode (`disabled` / `subprocess` / `bubblewrap`).
- `/compact` performs a one-off compaction; `/compact_mode` shows or toggles automatic compaction for the current conversation.
- `/continue` retries the latest interrupted turn from its stored resume context. If the user keeps talking instead, the follow-up message is appended to that stored resume context.
- Cron tasks and background agents inherit the creating conversation's agent backend and model.
- Session destroy paths such as `/new` tear down session-bound `exec`, `file_download`, `image`, and subagent runtime tasks.

---

## 🔒 Sandbox Execution

Three isolation levels, configurable per conversation:

| Mode | Isolation | Use Case |
|:-----|:----------|:---------|
| `disabled` | None — direct execution | Development, trusted environments |
| `subprocess` | Separate process per agent turn | Basic isolation |
| `bubblewrap` | `bwrap` namespaced container | Production — restricted filesystem, network-aware |

### Bubblewrap Details

- Only exposes: current workspace, runtime dir, `.skills/`, `.skill_memory/`
- `.skill_memory` mounted as shared persistent directory across workspaces
- `workspace_mount` becomes a read-only snapshot import
- DNS, `/etc/resolv.conf` properly forwarded
- Read-only mount cleanup on turn completion

---

## 📚 Skill System

Skills are `SKILL.md`-based reusable workflows discovered at runtime.

| Tool | Purpose |
|:-----|:--------|
| `skill_load` | Load a skill's instructions |
| `skill_create` | Persist a new skill from `.skills/<name>/` |
| `skill_update` | Update an existing skill |

### Skill Lifecycle

- Skills live in `.skills/<skill-name>/` with `SKILL.md` + optional `references/`, `scripts/`, `assets/`
- Frontmatter (`name`, `description`) validated on create/update
- Persistent skill-owned data goes to `.skill_memory/<skill-name>/...`
- On each user message, the runtime checks for skill changes:
  - Description changes → inject update notification
  - Content changes on loaded skills → inject latest content

---

## 🔁 Workspace Lifecycle

<div align="center">
  <img src="docs/imgs/workspace_lifecycle.png" alt="Workspace Lifecycle" width="780" />
  <br />
  <em>Workspace creation, usage, archival, and cross-agent reuse</em>
</div>

---

## ⚙️ Configuration Reference

`agent_host` is driven by a single JSON config file:

```jsonc
{
  "version": "0.9",
  "models":     { /* named model profiles */ },
  "agent":      { /* backend -> available model aliases */ },
  "tooling":    { /* helper model routing */ },
  "main_agent": { /* agent behavior settings */ },
  "sandbox":    { /* sandbox defaults */ },
  "channels":   [ /* one or more channel configs */ ]
}
```

<details>
<summary><b>📋 Model Profile (<code>models.&lt;name&gt;</code>)</b></summary>
<br />

Each named entry under `models` describes one model alias and its upstream settings. Foreground agent selection is now split in two parts:

- `agent.agent_frame.available_models` / `agent.zgent.available_models` decide which model aliases each backend may use
- Conversations choose backend + model via `/agent`; `main_agent.model` is only the default/fallback selection when present

| Field | Type | Default | Description |
|:------|:-----|:--------|:------------|
| `type` | `"openrouter"` \| `"openrouter-resp"` \| `"codex-subscription"` | — | Upstream adapter kind |
| `api_endpoint` | string | — | Base URL of the OpenAI-compatible API |
| `model` | string | — | Model identifier passed to the API |
| `capabilities` | string[] | `[]` | Declared abilities such as `chat`, `web_search`, `image_in`, `image_out`, `pdf`, `audio_in` |
| `agent_model_enabled` | bool | `true` | Whether this alias can appear in agent model pickers |
| `supports_vision_input` | bool | `false` | Whether to pass images to the model |
| `api_key` | string | null | Inline API key (prefer `api_key_env`) |
| `api_key_env` | string | `"OPENAI_API_KEY"` | Env var from which to read the API key |
| `chat_completions_path` | string | `"/chat/completions"` | Path appended to `api_endpoint` |
| `codex_home` | string | null | Required for `codex-subscription` models |
| `timeout_seconds` | float | `120.0` | Per-request LLM timeout (adjustable at runtime via `/set_api_timeout`) |
| `context_window_tokens` | int | `128000` | Context window size for compaction budget |
| `cache_ttl` | string | null | Cache TTL hint (e.g. `"5m"`), enables cache control headers |
| `reasoning` | object | null | Reasoning config (budget tokens, effort level) |
| `headers` | object | `{}` | Extra HTTP headers sent with every request |
| `native_web_search` | object | null | Provider-native search (mutually exclusive with `external_web_search`) |
| `description` | string | `""` | Human-readable label; shown to agents in model catalog |

</details>

<details>
<summary><b>🤖 Agent Routing (<code>agent</code> / <code>tooling</code>)</b></summary>
<br />

| Field | Type | Description |
|:------|:-----|:------------|
| `agent.agent_frame.available_models` | string[] | Model aliases selectable by the `agent_frame` backend |
| `agent.zgent.available_models` | string[] | Model aliases selectable by the `zgent` backend |
| `tooling.web_search` | string | Helper model alias for external search |
| `tooling.image` | string | Image input helper alias, optionally with `:self` |
| `tooling.image_gen` | string | Image generation helper alias |
| `tooling.pdf` | string | PDF helper alias |
| `tooling.audio_input` | string | Audio input helper alias |

</details>

<details>
<summary><b>🤖 Main Agent (<code>main_agent</code>)</b></summary>
<br />

| Field | Type | Default | Description |
|:------|:-----|:--------|:------------|
| `model` | string | null | Optional default model alias used before a conversation picks its own backend/model |
| `language` | string | `"zh-CN"` | Reply language injected into the system prompt |
| `timeout_seconds` | float | null | Wall-clock timeout for a full agent turn (`0` = disabled, `null` = unlimited) |
| `global_install_root` | string | `"/opt"` | Shared install root exposed to runtime helpers |
| `enabled_tools` | string[] | all built-ins | Legacy/advanced override for built-in tool availability |
| `max_tool_roundtrips` | int | `12` | Max LLM → tool → LLM cycles per turn |
| `enable_context_compression` | bool | `true` | Enable automatic turn compaction |
| `context_compaction.trigger_ratio` | float | `0.9` | Fraction of context budget that triggers compaction |
| `context_compaction.token_limit_override` | int | null | Hard token budget override for compaction |
| `context_compaction.recent_fidelity_target_ratio` | float | `0.18` | Share of token budget reserved for recent high-fidelity history |
| `idle_compaction.enabled` | bool | `false` | Run compaction between turns when a conversation is idle |
| `idle_compaction.poll_interval_seconds` | int | `15` | How often to check for idle compaction opportunity |
| `idle_compaction.min_ratio` | float | `0.5` | Minimum context pressure before idle compaction runs |
| `timeout_observation_compaction.enabled` | bool | `true` | Compact when the model times out and needs a shorter retry context |

</details>

---

## 📱 Telegram Integration

| Feature | Details |
|:--------|:--------|
| **Long message splitting** | Auto-splits replies that exceed Telegram's length limit |
| **Group chat** | Recognizes `@botname`-suffixed commands; two-person groups treated as direct chat |
| **Retry & queuing** | Send-side retry with FIFO fallback on failure |
| **Interactive commands** | `/new`, `/oldspace`, `/help`, `/status`, `/compact`, `/compact_mode`, `/agent`, `/sandbox`, `/think`, `/set_api_timeout`, `/snapsave`, `/snapload`, `/snaplist`, `/continue` |

---

## 🚀 Quick Start

### 1. Environment Setup

```bash
cp .env.example .env
```

Fill in the required variables:

```dotenv
OPENROUTER_API_KEY=sk-or-...       # For agent_frame / agent_host
TELEGRAM_BOT_TOKEN=...             # For Telegram channel (agent_host)
```

### 2. CLI Mode

```json
{
  "version": "0.9",
  "models": {
    "main": {
      "type": "openrouter",
      "api_endpoint": "https://openrouter.ai/api/v1",
      "model": "openai/gpt-4o-mini",
      "capabilities": ["chat", "image_in"],
      "supports_vision_input": true,
      "api_key_env": "OPENROUTER_API_KEY",
      "cache_ttl": "5m",
      "context_window_tokens": 128000,
      "description": "Fast general-purpose chat model."
    }
  },
  "agent": {
    "agent_frame": { "available_models": ["main"] }
  },
  "main_agent": { "language": "zh-CN" },
  "channels": [{ "kind": "command_line", "id": "local-cli", "prompt": "you> " }]
}
```

```bash
./run_test.sh agent_host/example_config.json
```

### 3. Telegram Bot Mode

```json
{
  "version": "0.9",
  "agent": {
    "agent_frame": { "available_models": ["main"] }
  },
  "models": {
    "main": {
      "type": "openrouter",
      "api_endpoint": "https://openrouter.ai/api/v1",
      "model": "openai/gpt-4o-mini",
      "capabilities": ["chat", "image_in"],
      "supports_vision_input": true,
      "api_key_env": "OPENROUTER_API_KEY",
      "description": "Fast general-purpose chat model."
    }
  },
  "main_agent": { "language": "zh-CN" },
  "channels": [{
    "kind": "telegram",
    "id": "telegram-main",
    "bot_token_env": "TELEGRAM_BOT_TOKEN"
  }]
}
```

```bash
./run_test.sh test_telegram.json
```

### 4. Direct Binary Run

```bash
cargo run --release --manifest-path agent_host/Cargo.toml --bin partyclaw -- \
  --config /path/to/config.json \
  --workdir /path/to/workdir
```

> See [`agent_host/example_config.json`](agent_host/example_config.json) and [`agent_host/example_telegram_config.json`](agent_host/example_telegram_config.json) for full examples.

---

## 🔌 Agent Backend: `agent_frame` vs `zgent`

Backends are now selected per conversation, not per model definition. Models live under `models.<alias>`, each backend exposes its own allowlist under `agent.agent_frame.available_models` or `agent.zgent.available_models`, and `/agent` lets the user choose the backend + model pair at runtime.

> ⚠️ `zgent` is a **soft dependency** — the project compiles normally when the `zgent/` directory is absent; only the `zgent` backend becomes unavailable. It is useful for quick endpoint verification but is **not recommended for production**.

<details>
<summary><b>📊 Full Comparison Table</b></summary>
<br />

| Dimension | `agent_frame` | `zgent` (compat) | Impact |
|:----------|:--------------|:------------------|:-------|
| **Multimodal input** | ✅ Native | ❌ Stripped | User images invisible to LLM |
| **Context compaction** | ✅ Token-aware | ❌ Always `false` | Long sessions overflow |
| **Native web search** | ✅ Injected | ❌ Cleared | Silently disabled |
| **Streaming** | ✅ Supported | ❌ `stream: false` | Higher latency |
| **Checkpoint callback** | ✅ Per-round | ❌ Not wired | No mid-turn persistence |
| **Cache token stats** | ✅ Full | ❌ All zeros | Incomplete billing |
| **`max_tokens`** | ✅ Configurable | ⚠️ Hard-coded `4096` | Long completions truncated |
| **Temperature** | ✅ Configurable | ⚠️ Hard-coded `0.0` | Always deterministic |
| **System prompt update** | ✅ Marker-aware | ⚠️ Unconditional overwrite | Not safe |
| **Cancellation** | ✅ | ✅ | Identical |
| **Skills** | ✅ | ✅ | Identical |
| **Tokio runtime** | Sync on caller | ⚠️ New runtime per call | Extra overhead |

> **Constraint**: if a conversation uses the `zgent` backend, the selected model alias must still point at a `chat_completions`-style upstream (`chat_completions_path` should stay at the default `"/chat/completions"`).

</details>

---

## 🔁 CI / CD

| Trigger | Action |
|:--------|:-------|
| Push / Pull Request | `cargo fmt --check` + `cargo test` for both crates |
| Push to `main` with `VERSION` changed | After CI succeeds, the release workflow tags `vX.Y.Z` automatically and publishes binaries for `partyclaw` and `agent_frame/run_agent` |

> Unified Cargo `target/` directory — avoids duplicate `agent_host/target` and `agent_frame/target` build bloat.

---

## 🙈 Not Tracked in Git

| Path | Reason |
|:-----|:-------|
| `.env` | Secrets |
| `*_workdir/` | Runtime state |
| `logs/`, `sessions/` | Live data |
| `target/` | Build artifacts |

---

## 📊 Status

| Component | Version | State |
|:----------|:--------|:------|
| `agent_frame` | `0.9.2` | ✅ Active — PTY-backed `exec_start`, repo exploration tools, context compaction |
| `agent_host` | `0.9.2` | ✅ Active — backend/model routing, Telegram service host, snapshots, cron |
| Sandbox | — | ✅ Implemented — `subprocess` and `bubblewrap` modes |
| Deployment | — | ✅ systemd on NAT-pl1, bubblewrap verified |

---

<div align="center">

**Built with 🦀 Rust** · **Powered by LLMs** · **Agents as Services**

</div>
