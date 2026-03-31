# ClawParty 2.0

**A Rust-based multi-agent host and next-generation agentic framework.**

---

## What's in This Repo

```
ClawParty2.0/
├── agent_frame/    # Standalone agent runtime (tools, skills, compaction)
└── agent_host/     # Long-running service host (channels, sessions, cron, recovery)
```

They are separate Rust crates but share design philosophy: **agents as services, not scripts.**

---

## `agent_frame` — Agent Runtime

A self-contained Rust library and CLI binary for running a single LLM agent session.

### Capabilities

| Feature | Details |
|---------|---------|
| Built-in tools | File I/O, patch apply, shell execution, web fetch, web search, image inspection |
| Skill system | `SKILL.md`-based skill discovery with `load_skill` tool |
| Context compaction | Automatic compression when context approaches model limits |
| Token accounting | Tracks `cache_read` / `cache_write` / `cache_hit` / `cache_miss` per request |
| Tool timeouts | Every tool call has an explicit timeout budget |
| Cancellation | `SessionExecutionControl` carries an `AtomicBool` cancel flag checked before every LLM call and tool execution |
| Checkpoint callback | Optional callback fired after each tool round for mid-session state persistence |
| Modes | CLI binary (`run_agent`) or embedded library |

### Build & Test

```bash
cargo test --manifest-path agent_frame/Cargo.toml
```

### Configuration

See `agent_frame/example_config.json` and `agent_frame/example_openrouter_config.json`.

Web search: set either `native_web_search.enabled = true` (suppresses standalone `web_search` tool)
or configure an external search provider under `external_web_search`. Only one should be active per model.

---

## `agent_host` — Service Host

The production layer that wraps `agent_frame` into a long-running, multi-channel service.

### Architecture

```
Telegram / CLI / (future channels)
          │
    ┌─────▼──────┐
    │   Channel   │   Telegram bot, CLI, extensible
    └─────┬───────┘
          │
    ┌─────▼──────┐
    │   Session   │   Persistent storage, attachment lifecycle, workdir
    └─────┬───────┘
          │
    ┌─────▼────────────────────────────┐
    │          Agent Topology           │
    │  ┌────────────────────────────┐  │
    │  │  Main Foreground Agent     │  │  One per session, user-facing
    │  │  Main Background Agent     │  │  Long-running delegated work
    │  │  Sub-Agent                 │  │  Short-lived delegated tasks
    │  └────────────────────────────┘  │
    └──────────────────────────────────┘
          │
    ┌─────▼──────┐
    │  Cron / Sink│   Scheduled tasks, broadcast routing, direct routing
    └────────────┘
```

### Key Features

- **Session persistence** — state survives process restarts; attachment lifecycle managed automatically
- **Agent registry** — background and subagent state persisted across restarts
- **Cron tasks** — scheduled work with optional checker commands, stored durably
- **Background sinks** — direct routing, broadcast topics, multi-target fan-out
- **Structured logging** — JSONL logs with per-agent / per-session / per-channel views
- **Failure recovery** — automatic handling of timeouts, upstream errors, and restart scenarios

### Configuration

`agent_host` is driven by a single JSON config file. Top-level structure:

```jsonc
{
  "models": { /* named model profiles, see below */ },
  "main_agent": { /* agent behavior settings */ },
  "channels": [ /* one or more channel configs */ ]
}
```

#### Model Profile (`models.<name>`)

Each named entry under `models` describes one LLM endpoint. `main_agent.model` selects which one the foreground agent uses.

| Field | Type | Default | Description |
|---|---|---|---|
| `api_endpoint` | string | — | Base URL of the OpenAI-compatible API |
| `model` | string | — | Model identifier passed to the API |
| `backend` | `"agent_frame"` \| `"zgent"` | `"agent_frame"` | Agent execution backend (see below) |
| `supports_vision_input` | bool | `false` | Whether to pass images to the model |
| `image_tool_model` | string \| `"self"` | null | A separate model name to use for the `image` tool; `"self"` means use this model |
| `api_key` | string | null | Inline API key (prefer `api_key_env`) |
| `api_key_env` | string | `"OPENAI_API_KEY"` | Env var from which to read the API key |
| `chat_completions_path` | string | `"/chat/completions"` | Path appended to `api_endpoint` |
| `timeout_seconds` | float | `120.0` | Per-request LLM timeout |
| `context_window_tokens` | int | `128000` | Context window size used for compaction budget |
| `cache_ttl` | string | null | Cache TTL hint (e.g. `"5m"`), enables cache control headers |
| `reasoning` | object | null | Reasoning config (budget tokens, effort level) |
| `headers` | object | `{}` | Extra HTTP headers sent with every request |
| `native_web_search` | object | null | Provider-native search (mutually exclusive with `external_web_search`) |
| `external_web_search` | object | null | External search via a separate model/endpoint |
| `description` | string | `""` | Human-readable label; also shown to agents in the model catalog |

#### Main Agent (`main_agent`)

| Field | Type | Default | Description |
|---|---|---|---|
| `model` | string | — | Must match a key in `models` |
| `language` | string | `"zh-CN"` | Reply language injected into the system prompt |
| `timeout_seconds` | float | null | Wall-clock timeout for a full agent turn (null = unlimited) |
| `enabled_tools` | string[] | all built-ins | Tools made available to the agent |
| `max_tool_roundtrips` | int | `12` | Max LLM → tool → LLM cycles per turn |
| `enable_context_compression` | bool | `true` | Enable automatic mid-turn compaction |
| `effective_context_window_percent` | float | `0.9` | Fraction of `context_window_tokens` before compaction triggers |
| `auto_compact_token_limit` | int | null | Hard token budget that triggers compaction |
| `retain_recent_messages` | int | `8` | Minimum recent messages preserved during compaction |
| `enable_idle_context_compaction` | bool | `false` | Run compaction in the background between turns |
| `idle_context_compaction_poll_interval_seconds` | int | `15` | How often to check for idle compaction opportunity |

#### Example — CLI

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

#### Example — Telegram

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

See `agent_host/example_config.json` and `agent_host/example_telegram_config.json` for full examples.

### Build & Test

```bash
cargo test --manifest-path agent_host/Cargo.toml
```

### Run Locally

```bash
cp .env.example .env    # fill in OPENROUTER_API_KEY and/or TELEGRAM_BOT_TOKEN

# CLI mode
./run_test.sh agent_host/example_config.json

# Telegram bot mode
./run_test.sh test_telegram.json
```

---

## Agent Backend: `agent_frame` vs `zgent`

The `backend` field in each model profile selects the agent execution backend.
The default is `"agent_frame"`. Setting it to `"zgent"` routes the session through
a compatibility layer (`backend.rs`) that wraps zgent-core's OpenAI client while
reusing the same tool registry, skills, and message history format as `agent_frame`.

`zgent` is useful for quickly verifying that a new LLM endpoint is reachable and
produces sensible tool calls. It is **not recommended for production** use because
of the limitations listed below.

| Dimension | `agent_frame` | `zgent` (compat layer) | Impact on agent_host |
|---|---|---|---|
| **Multimodal input** | ✅ Images passed natively as `input_image` / `image_url` | ❌ Images stripped; replaced with `[N image item(s) omitted…]` | User-sent images are invisible to the LLM |
| **Context compaction** | ✅ Token-aware summarisation, written back to session | ❌ Always returns `compacted=false`; no compression | Long sessions grow unbounded until context window overflow |
| **Native web search** | ✅ `native_web_search` config injected into system prompt | ❌ Field forcibly cleared (`None`) | Provider-native search is silently disabled |
| **Streaming** | ✅ Supported | ❌ Forced `stream: false` | Higher response latency, no incremental output |
| **Checkpoint callback** | ✅ Fired after each tool round | ❌ Not wired | Mid-turn state persistence and recovery do not apply |
| **Cache token stats** | ✅ `cache_read` / `cache_write` / `cache_hit` / `cache_miss` | ❌ All cache fields zero | Billing analytics are incomplete |
| **`max_tokens`** | ✅ Driven by config | ⚠️ Hard-coded `4096` | Long completions are truncated regardless of model capability |
| **Temperature** | ✅ Driven by config | ⚠️ Hard-coded `0.0` | Output is always deterministic; creative tasks are clamped |
| **System prompt update** | ✅ Detects `[AgentFrame Runtime]` / compaction markers before replacing | ⚠️ Unconditionally overwrites first system message | System prompt replacement is not marker-aware |
| **Cancellation** | ✅ Checked before every LLM call and tool execution | ✅ Same | Identical behaviour |
| **Skills** | ✅ Native integration | ✅ `discover_skills` called in compat layer | Identical behaviour |
| **Tokio runtime** | Synchronous block on calling thread | ⚠️ New single-threaded `tokio` runtime built per call | Extra runtime construction overhead per turn |

> **Constraint**: when `backend` is `"zgent"`, `chat_completions_path` must remain
> the default (`"/chat/completions"`). The config loader rejects any other value.

---

## Sandbox Design

[`SANDBOX_DESIGN.md`](./SANDBOX_DESIGN.md) contains a detailed design for multi-agent
workspace isolation — going beyond the current shared `rundir` model.

**Core idea**: per-agent scratch workspace + durable `projects/` store + enforced mount manager.

Key mechanisms:
- Read/write leases with epoch validation (stale writers can't commit)
- Overlay / copy-on-write writable mounts
- Tombstone-based safe project deletion
- Startup recovery for stale leases and crashed agents

> This design is **specified but not yet implemented**.
> It is only worth building with hard enforcement — soft prompt conventions are not enough.

---

## Environment Setup

```bash
cp .env.example .env
```

Required variables:

```dotenv
OPENROUTER_API_KEY=sk-or-...       # For agent_frame / agent_host
TELEGRAM_BOT_TOKEN=...             # For Telegram channel (agent_host)
```

---

## CI / CD

| Trigger | Action |
|---------|--------|
| Push / Pull Request | `cargo fmt --check` + `cargo test` for `agent_frame` and `agent_host` |
| Version tag `v*.*.*` | Release binaries: `agent_host`, `agent_frame/run_agent` |

---

## Not Tracked in Git

- `.env` — secrets
- `*_workdir/` — runtime state
- `logs/`, `sessions/` — live data
- `target/` — build artifacts

---

## Status

| Component | Version | State |
|-----------|---------|-------|
| `agent_frame` | 0.1.0 | ✅ Stable |
| `agent_host` | 0.1.0 | ✅ Active — cancellation hardening in progress |
| Sandbox design | — | 📐 Designed, not yet implemented |
