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
| 🛠️ **Built-in tools** | File I/O, patch apply, shell execution (`exec_start` / `exec_observe` / `exec_wait` / `exec_kill`), web fetch, web search, image inspection |
| 📚 **Skill system** | `SKILL.md`-based skill discovery with `skill_load` / `skill_create` / `skill_update` tools |
| 🗜️ **Context compaction** | Automatic compression when context approaches model limits; safe handling of tool_use/tool_result boundaries |
| 📊 **Token accounting** | Tracks `cache_read` / `cache_write` / `cache_hit` / `cache_miss` per request |
| ⏱️ **Tool timeouts** | Every tool call has an explicit timeout budget; tools categorized as `immediate` or `timed` |
| ⚡ **Parallel tool calls** | Multiple independent tool calls in the same round execute concurrently |
| 🛑 **Cancellation** | `SessionExecutionControl` with `AtomicBool` cancel flag — checked before every LLM call and tool execution |
| 💾 **Checkpoint callback** | Optional callback fired after each tool round for mid-session state persistence |
| 🤖 **Subagents** | `run_subagent` with model selection and `timeout_seconds=0` for unbounded wait |
| 🎛️ **Modes** | CLI binary (`run_agent`) or embedded library |

### 🔨 Build & Test

```bash
cargo test --manifest-path agent_frame/Cargo.toml
```

### ⚙️ Configuration

See [`agent_frame/example_config.json`](agent_frame/example_config.json) and [`agent_frame/example_openrouter_config.json`](agent_frame/example_openrouter_config.json).

> **Web search**: set either `native_web_search.enabled = true` (suppresses standalone `web_search` tool) or configure an external search provider under `external_web_search`. Only one should be active per model.

---

## 🚀 `agent_host` — Service Host

The production layer that wraps `agent_frame` into a long-running, multi-channel service.

### 🔑 Key Features

| Feature | Description |
|:--------|:------------|
| 💬 **Conversation management** | Channel → Conversation → Session → Agent layered model; per-conversation model and sandbox settings |
| 💾 **Session persistence** | State survives process restarts; attachment lifecycle managed automatically |
| 📋 **Agent registry** | Background and subagent state persisted across restarts |
| ⏰ **Cron tasks** | Scheduled work with optional checker commands, stored durably; inherits creator conversation's model |
| 📡 **Background sinks** | Direct routing, broadcast topics, multi-target fan-out |
| 🔄 **Model switching** | `/model` to switch mid-conversation with automatic context compression |
| 📸 **Snapshots** | `/snapsave`, `/snapload`, `/snaplist` for global conversation state snapshots |
| 🔒 **Sandbox execution** | Three modes: `disabled`, `subprocess`, `bubblewrap` — per-conversation configurable via `/sandbox` |
| 📝 **Structured logging** | JSONL logs with per-agent / per-session / per-channel views |
| 📊 **Status & billing** | `/status` shows token usage, cache hits, price estimation, compression savings |
| 🔄 **Failure recovery** | Automatic handling of timeouts, upstream errors, and restart scenarios |

---

## 💬 Conversation Lifecycle

```
Channel (e.g. Telegram)
  └─ Conversation (persistent: model, sandbox mode, workspace)
       └─ Session (one active agent turn)
            └─ Agent Topology (main agent, subagents, background agents)
```

- `/new` starts a fresh conversation. If no model is selected, prompts the user first.
- `/model` switches the main model with automatic context compression.
- `/sandbox` toggles sandbox mode (`disabled` / `subprocess` / `bubblewrap`).
- Cron tasks and background agents inherit the creating conversation's model.

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
| `skill_load` | Load a skill's instructions (renamed from `load_skill`, alias preserved) |
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
  "models":     { /* named model profiles */ },
  "main_agent": { /* agent behavior settings */ },
  "channels":   [ /* one or more channel configs */ ]
}
```

<details>
<summary><b>📋 Model Profile (<code>models.&lt;name&gt;</code>)</b></summary>
<br />

Each named entry under `models` describes one LLM endpoint. `main_agent.model` selects which one the foreground agent uses.

| Field | Type | Default | Description |
|:------|:-----|:--------|:------------|
| `api_endpoint` | string | — | Base URL of the OpenAI-compatible API |
| `model` | string | — | Model identifier passed to the API |
| `backend` | `"agent_frame"` \| `"zgent"` | `"agent_frame"` | Agent execution backend |
| `supports_vision_input` | bool | `false` | Whether to pass images to the model |
| `image_tool_model` | string \| `"self"` | null | A separate model name for the `image` tool; `"self"` = this model |
| `api_key` | string | null | Inline API key (prefer `api_key_env`) |
| `api_key_env` | string | `"OPENAI_API_KEY"` | Env var from which to read the API key |
| `chat_completions_path` | string | `"/chat/completions"` | Path appended to `api_endpoint` |
| `timeout_seconds` | float | `120.0` | Per-request LLM timeout (adjustable at runtime via `/set_api_timeout`) |
| `context_window_tokens` | int | `128000` | Context window size for compaction budget |
| `cache_ttl` | string | null | Cache TTL hint (e.g. `"5m"`), enables cache control headers |
| `reasoning` | object | null | Reasoning config (budget tokens, effort level) |
| `headers` | object | `{}` | Extra HTTP headers sent with every request |
| `native_web_search` | object | null | Provider-native search (mutually exclusive with `external_web_search`) |
| `external_web_search` | object | null | External search via a separate model/endpoint |
| `description` | string | `""` | Human-readable label; shown to agents in model catalog |

</details>

<details>
<summary><b>🤖 Main Agent (<code>main_agent</code>)</b></summary>
<br />

| Field | Type | Default | Description |
|:------|:-----|:--------|:------------|
| `model` | string | — | Must match a key in `models` |
| `language` | string | `"zh-CN"` | Reply language injected into the system prompt |
| `timeout_seconds` | float | null | Wall-clock timeout for a full agent turn (`0` = disabled, `null` = unlimited) |
| `enabled_tools` | string[] | all built-ins | Tools made available to the agent |
| `max_tool_roundtrips` | int | `12` | Max LLM → tool → LLM cycles per turn |
| `enable_context_compression` | bool | `true` | Enable automatic mid-turn compaction |
| `effective_context_window_percent` | float | `0.9` | Fraction of `context_window_tokens` before compaction triggers |
| `auto_compact_token_limit` | int | null | Hard token budget that triggers compaction |
| `retain_recent_messages` | int | `8` | Minimum recent messages preserved during compaction |
| `enable_idle_context_compaction` | bool | `false` | Run compaction in the background between turns |
| `idle_context_compaction_poll_interval_seconds` | int | `15` | How often to check for idle compaction opportunity |

</details>

---

## 📱 Telegram Integration

| Feature | Details |
|:--------|:--------|
| **Long message splitting** | Auto-splits replies that exceed Telegram's length limit |
| **Group chat** | Recognizes `@botname`-suffixed commands; two-person groups treated as direct chat |
| **Retry & queuing** | Send-side retry with FIFO fallback on failure |
| **Interactive commands** | `/model`, `/sandbox`, `/status`, `/new`, `/snapsave`, `/snapload`, `/snaplist`, `/set_api_timeout` |

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
  "models": {
    "main": {
      "backend": "agent_frame",
      "api_endpoint": "https://openrouter.ai/api/v1",
      "model": "openai/gpt-4o-mini",
      "supports_vision_input": true,
      "api_key_env": "OPENROUTER_API_KEY",
      "cache_ttl": "5m",
      "context_window_tokens": 128000,
      "description": "Fast general-purpose chat model."
    }
  },
  "main_agent": { "model": "main", "language": "zh-CN" },
  "channels": [{ "kind": "command_line", "id": "local-cli", "prompt": "you> " }]
}
```

```bash
./run_test.sh agent_host/example_config.json
```

### 3. Telegram Bot Mode

```json
{
  "models": {
    "main": {
      "backend": "agent_frame",
      "api_endpoint": "https://openrouter.ai/api/v1",
      "model": "openai/gpt-4o-mini",
      "supports_vision_input": true,
      "api_key_env": "OPENROUTER_API_KEY",
      "description": "Fast general-purpose chat model."
    }
  },
  "main_agent": { "model": "main", "language": "zh-CN" },
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

> See [`agent_host/example_config.json`](agent_host/example_config.json) and [`agent_host/example_telegram_config.json`](agent_host/example_telegram_config.json) for full examples.

---

## 🔌 Agent Backend: `agent_frame` vs `zgent`

The `backend` field in each model profile selects the agent execution backend. Default is `"agent_frame"`. Setting it to `"zgent"` routes through a compatibility layer.

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

> **Constraint**: when `backend` is `"zgent"`, `chat_completions_path` must remain the default (`"/chat/completions"`).

</details>

---

## 🔁 CI / CD

| Trigger | Action |
|:--------|:-------|
| Push / Pull Request | `cargo fmt --check` + `cargo test` for both crates |
| Version tag `v*.*.*` | Release binaries: `agent_host`, `agent_frame/run_agent` |

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
| `agent_frame` | `0.2.0` | ✅ Stable — parallel tools, skill CRUD, exec refactor |
| `agent_host` | `0.2.0` | ✅ Active — conversation model, sandbox, snapshots |
| Sandbox | — | ✅ Implemented — `subprocess` and `bubblewrap` modes |
| Deployment | — | ✅ systemd on NAT-pl1, bubblewrap verified |

---

<div align="center">

**Built with 🦀 Rust** · **Powered by LLMs** · **Agents as Services**

</div>
