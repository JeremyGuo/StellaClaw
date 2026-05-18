<div align="center">

# Stellaclaw

**面向长期运行、多入口协作的 Rust Agent Host。**

持久对话。隔离执行。可恢复 Agent。

[English README](README.md) · [Roadmap](ROAD_MAP.md) · [版本记录](VERSION) · [配置示例](example_config.json)

</div>

---

## Stellaclaw 是什么？

Stellaclaw 是 ClawParty 的下一代运行时：一个自托管 Agent 系统。它常驻在线，从不同 channel 接收消息，并以持久状态、工具、技能、workspace 和崩溃恢复来驱动 LLM session。

Stellaclaw 是多入口设计。StellaCodeX 是桌面入口；Telegram group 可以作为轻量 conversation surface；Web API 可以支撑其他客户端。它们共享同一个 Stellaclaw 后端、持久 conversation、session runtime、provider 配置和 workspace 模型。

StellaCodeX 让你可以从任何电脑打开同一个 Agent 工作界面：UI 在本机运行，但文件、终端、工具调用和代码修改都发生在当前 conversation 连接的 Stellaclaw server workspace 里。

![StellaCodeX desktop connected to a server workspace](docs/assets/stellacode.png)

它有点像 VS Code Server，但核心不是远程手写代码，而是远程 Agent 工作。代码仓库放在服务器能访问的位置；Agent 在那里读写文件、运行命令、维护上下文并推进任务；用户可以从任意机器监督、打断、查看文件、打开终端，必要时接管。

Telegram 也是一等入口。通过创建不同 Telegram group，可以快速创建多个不同任务或不同功能的 conversation；它们仍然共享同一个后端服务。

![Telegram groups backed by the same Stellaclaw server](docs/assets/telegram.png)

每个 conversation 都可以独立选择模型、沙盒策略和 remote execution binding。Remote mode 可以让某个 conversation 表现得像是直接运行在 Stellaclaw 能通过 SSH 访问的另一台服务器上，这就是 Stellaclaw 支持 SSH Remote 工作流的方式：远程执行属于后端 conversation/runtime，而不是客户端自己承担。

StellaCodeX 和 Remote mode 让 Stellaclaw 支持一条实用的三跳工作路径：

```text
StellaCodeX / Channel Host -> Stellaclaw Server -> Remote Workspace Server
```

StellaCodeX、Telegram group 和 Web client 是用户侧 host surface。Stellaclaw server 拥有 conversation、routing、session、delivery 和 recovery。Remote mode 可以再把某个 conversation 绑定到另一台服务器的 workspace、terminal 和 filesystem，让 Agent 在项目真正所在的位置工作。

这带来的部署优势是：用户保留本地桌面或 channel 体验，中心 Agent 服务保持持久运行，重型项目工作可以发生在另一台远程机器上，而 conversation state 不需要迁移出 host。

当前 Rust 实现分成这些运行层：

| 层 | Binary / Crate | 负责 |
|---|---|---|
| Client / channel surface | `apps/stellacodeX/electron`、Telegram、Web API | 用户交互、桌面 workspace 浏览、terminal UI、附件、delivery |
| Host server | `stellaclaw` | Channels、conversations、workdirs、Telegram/Web surfaces、config、routing、delivery、runtime skill persistence |
| Agent server | `agent_server` + `stellaclaw_core` | SessionActor 状态机、provider/tool loop、compaction、session history、runtime metadata |
| Remote workspace server | SSH/sshfs target | 可选 fixed/selectable remote workspace、remote shell、项目文件、执行根目录 |

一句话边界：

```text
Conversation = 持久边界 + 路由器
SessionActor = 执行边界 + 状态机
```

这个拆分让平台/channel 逻辑不进入 model loop，让 remote workspace 逻辑不污染 client，并让每个 conversation 都有可恢复的执行边界。

---

## Highlights

- **三跳工作路径**：StellaCodeX / Telegram / Web client 连接 Stellaclaw Host；同一个 conversation 可以通过 Remote mode 再绑定到第二台 SSH workspace server。
- **多入口共享同一后端**：桌面端、Telegram group 和 Web API client 共享同一套持久 conversation runtime、模型配置、skill store、memory store 和 workspace 模型。
- **Conversation 级独立控制**：每个 conversation 都有自己的模型快照、sandbox override、reasoning effort、remote binding 和 foreground/background/subagent session binding。
- **可恢复执行**：未完成 turn、cooperative interrupt、runtime 崩溃、provider 失败、上下文压缩和长 tool batch 都作为生命周期事件处理，而不是静默丢状态。
- **Server-backed 桌面工作区**：StellaCodeX 可以在服务端 workspace 上浏览、预览、上传/下载、打开 terminal、检查 tool 详情、渲染 message attachment。
- **Remote-aware tools**：文件、shell、download、patch 和 visibility tools 都会随 Remote mode 改变 schema；fixed SSH Remote 下不暴露 `remote` 选择，绑定的远程 cwd 是隐式执行根。
- **Runtime skills + Memory**：`SKILL.md` 目录可运行时加载/持久化；Memory v1 把 user、conversation、public 三类长期事实和临时 chat history 分开。

## 高级特性

对外介绍 Stellaclaw 时，尤其应该说清楚这些已经在当前代码里落地的高级能力：

| 能力 | 当前代码中的含义 |
|---|---|
| 持久 Host / 隔离执行拆分 | `Conversation` 负责路由、workspace materialization、附件、状态和 delivery；`SessionActor` 在 `agent_server` 中负责 provider/tool loop。 |
| 动态 ToolCatalog | Tool schema 根据 runtime state、模型能力、remote mode、session kind、host tool scope 和 provider visibility 重建；过滤后的 catalog 同时也是本地执行白名单。 |
| Prompt Protocol | 工具使用偏好和约束放在工具定义旁边，只有 required tools 对当前 provider 可见时才注入 system prompt。 |
| Memory v1 | Host 侧 memory 有 `user`、`conversation`、`public` scope。User memory 进入 system prompt snapshot；conversation/public memory 通过显式搜索或压缩 recall 进入上下文。 |
| FileItem / 附件链路 | 用户上传、tool 产物和 assistant `<attachment>` 标签会被解析成结构化文件、稳定 workspace path、预览/下载和 provider-time 多模态规范化。 |
| Codex subscription 支持 | Codex websocket provider 保留 auth 状态，支持 priority service tier、encrypted reasoning continuation 和 streamed reasoning summary。 |
| Rich StellaCodeX UI | Electron 已支持 conversation list、前台 WebSocket、workspace tree、文件/message attachment 预览、sandboxed HTML preview、split Git diff tool detail、xterm terminal、plan panel、usage panel 和主题配色。 |

## 典型场景

Stellaclaw 适合那些需要把用户、Agent、文件、terminal 和远程机器放在同一个可恢复上下文里的长期科研 / 工程任务：

- **Paper wiki 和科研知识库**：整理大规模论文库，生成 paper summary、venue/year 趋势分析和研究报告，并把结果保留在 conversation workspace 中。
- **远程服务器实验**：一个 conversation 绑定一个项目或一台机器，让 Agent 在数据、GPU 和构建环境真正所在的位置运行脚本、trace、benchmark 和实验，再从 StellaCodeX 或 Telegram 查看结果。
- **Benchmark 驱动的代码优化**：让 Agent 反复修改底层代码、跑 benchmark、查看 diff 和产物，把完整优化轨迹保存在同一个 conversation。
- **长时间后台任务**：从桌面端启动任务后可以关闭客户端；后端 runtime 继续执行，之后从手机、Telegram 或其他客户端查看进度并继续接管。
- **论文和 artifact 开发**：把论文修改、实验脚本、图表、生成文件和远程 terminal 绑定在同一个项目 conversation 里。

---

## 当前状态

Stellaclaw 现在已经可以作为 Telegram-backed 和 StellaCodeX/Web-backed Agent host 使用。当前实现以 Rust 为主，并且明确保持 Host/runtime 边界。

当前已实现：

- Telegram channel：入站消息、group-based conversations、附件、typing indicator、可编辑 progress panel、最终成功/失败 delivery，以及 `/model` / `/remote` / `/sandbox` / `/status` / `/continue` / `/cancel` 控制命令。
- Web channel API：models、conversations、messages、status、workspace list/read/upload/download/move/delete、terminals、foreground WebSocket、conversation stream 和 seen-state tracking。
- StellaCodeX Electron 桌面端：server profiles、conversation list、chat、粘贴/拖拽附件、workspace browser、文件预览、HTML sandbox rendering、terminal dock、plan/overview panels、usage breakdown、可配置主题、Git diff tool detail rendering。
- Per-conversation model switching、sandbox switching、reasoning effort、remote workspace switching、status query、cancel、continue、foreground/background/subagent bindings 和 managed-agent status。
- `agent_server` 子进程边界：stdin/stdout line-delimited JSON-RPC。
- `SessionActor` control/data mailbox、turn loop、tool batch executor、provider worker isolation、idle compaction、崩溃恢复、未完成 turn 继续执行和未闭合 tool-call history repair。
- Codex subscription provider：使用官方 websocket shape，支持 access token refresh、priority service tier、encrypted reasoning continuation 和 streamed reasoning summary persistence。
- OpenRouter chat-completions / responses providers、Claude provider、Brave Search provider、OpenAI-compatible image generation/editing 和 provider-backed media helpers。
- Model-aware multimodal input normalization：模型不支持某种文件模态时降级成文本/file context。
- 内置 tools：文件、搜索、patch、fresh-process shell、downloads、web fetch/search、media、cron、subagents、background agents、memory 和 host coordination。
- Runtime `SKILL.md` 系统：`skill_load`、`skill_create`、`skill_update`、`skill_delete`，通过 `.stellaclaw/skill/` 持久化。
- 从 legacy PartyClaw workdir 布局迁移到当前 Stellaclaw 布局。

下一步计划：

- 稳定面向外部系统和 admin UI 的 REST/admin API 形态。
- 更完整的 host 管理和 observability。

完整架构方向见 [ROAD_MAP.md](ROAD_MAP.md)。

---

## 架构

![Stellaclaw 架构](docs/assets/stellaclaw-architecture-zh.svg)

核心边界仍然是：

```text
Conversation = 持久边界 + 路由器
SessionActor = 执行边界 + 状态机
```

Stellaclaw 有意把本地 client/channel surface、持久 Host server、隔离 Agent runtime 和可选 remote workspace 拆成不同层。UI 可以重启，agent runtime 可以崩溃后恢复，项目仓库也可以留在另一台 SSH 服务器上，而 conversation history 不需要离开 Host workdir。

这个结构的关键点：

- Channel 代码保持平台相关和用户可见。
- Conversation 代码拥有持久路由决策和 workspace materialization。
- SessionActor 拥有 model/tool loop 和 session history。
- Remote workspace 行为可以演进，而不需要把 StellaCodeX、Telegram 或 Web 代码变成 session internals。
- 服务崩溃或重启后，可以从持久 conversation/session state 恢复。

### Remote Mode：SSHFS Workspace Illusion

Remote mode 的目标是让 Agent 像在远程项目目录里工作一样，同时 Stellaclaw 仍把持久 conversation 和 session state 保存在 host server 上。

![SSHFS remote workspace illusion](docs/assets/remote-sshfs-illusion.svg)

在 fixed Remote mode 下，file tools 操作 sshfs mount 后的 workspace path，所以 read、write、search、patch、upload、download 对 Agent 来说都像本地文件操作。交互式 terminal 不通过 FUSE mount 执行命令，而是通过 SSH 连接并进入配置的 remote cwd。Conversation binding 记录当前 remote workspace；session history 和 recovery metadata 仍留在 Stellaclaw workdir。

也就是说：Agent 拿到的是普通 workspace path，但文件字节和 shell 都由远程服务器支撑。

![Two-hop remote access through StellaCodeX and Web Channel](docs/assets/remote-web-two-hop.svg)

通过 Web Channel，StellaCodeX 可以连接到一台远离用户笔记本的 Stellaclaw server。Remote mode 还可以从这台 Stellaclaw server 再跳到另一台项目服务器。因为 conversation 和 session state 由 Stellaclaw 持久化，当前工作可以恢复；真实仓库则留在适合构建、测试、terminal 和文件编辑的远程机器上。

---

## Telegram 体验

Telegram channel 是当前主要可用产品 surface。

支持的控制命令：

| 命令 | 用途 |
|---|---|
| `/model` | 查看或切换 conversation 的模型 |
| `/remote` | 选择或清除 remote workspace execution mode |
| `/sandbox` | 查看或切换 sandbox mode |
| `/status` | 查看 conversation 状态、模型、remote、sandbox、usage |
| `/continue` | 继续最近被中断的 turn |
| `/cancel` | 请求取消当前 turn |
| `/compact` | 主动压缩当前上下文 |

---

## Tooling

Stellaclaw 通过动态 catalog 暴露 tools。Catalog 会根据 runtime state、模型能力、session type、host tool scope、provider visibility 和 remote mode 重新构建。同一份过滤后的 catalog 同时决定 provider request schema、Prompt Protocol 注入和本地执行白名单。

工具类型包括：

| 类别 | 示例 |
|---|---|
| 文件、搜索、可见性 | `file_read`、`file_write`、`grep`、`apply_patch`、`shell_make_visible`、`attachment_make_visible` |
| Fresh-process shell | `shell_exec`、`shell_write_stdin`、`shell_stop` |
| Web / downloads | `web_fetch`、`web_search`、`file_download_start`、`file_download_progress`、`file_download_wait`、`file_download_cancel` |
| Media | `image_view`、`pdf_view`、`audio_view`、provider-backed analysis/generation tools |
| Host coordination | `update_plan`、subagents、background agents、cron、managed-agent status |
| Memory | `memory_search`、`memory_write`、`memory_update`、`memory_delete` |
| Skills | `skill_load`、`skill_create`、`skill_update`、`skill_delete` |

Remote mode 会影响 tool schema：

- selectable mode 暴露可选 `remote` 字段；
- fixed remote mode 隐藏 `remote` 字段，把绑定的 execution root 当成隐式目标；
- `apply_patch` 会把 active workspace / remote cwd 内的绝对路径归一化成相对路径，并拒绝执行根之外的路径。

Shell surface 是明确的 process 语义：新命令使用 `shell_exec.command`；长运行命令返回 `process_id`；后续通过 `shell_write_stdin` 观察/交互，或通过 `shell_stop` 停止。没有隐藏可复用 shell session，也没有 `cmd` alias。

---

## Skills

Skills 是同步到 conversation workspace 的 `SKILL.md` 目录，canonical workspace 路径是 `.stellaclaw/skill/`：

```text
.stellaclaw/skill/
  web-report-deploy/
    SKILL.md
    references/
    scripts/
    assets/
```

- `skill_load` 把 skill 内容加载进当前 session。
- `skill_create` 把 staged workspace skill 持久化到 runtime skill store 的 `.stellaclaw/skill/<name>`。
- `skill_update` 更新 runtime skill store 并同步已有 conversation workspace。
- `skill_delete` 删除 runtime store 和已有 workspace 中的 skill。

Skills 用来沉淀长期可复用工作流；稳定的用户/项目事实应该进 Memory v1 或普通仓库文档，而不是全部塞进 system prompt。

---

## Multimodal Input

内部消息模型支持结构化 `ChatMessageItem::File`。Web API 也支持在发送消息时传 `files[]`，例如：

```json
{
  "user_name": "StellaCodeX",
  "text": "请看这张图",
  "files": [
    {
      "uri": "file:///path/to/image.png",
      "media_type": "image/png",
      "name": "image.png"
    }
  ]
}
```

Provider 层会根据模型能力做多模态输入规范化。如果模型不支持某种文件模态，系统会尽量降级成文本 context 或文件引用，而不是直接破坏整轮请求。

---

## Providers

当前实现包含：

- Codex subscription provider：使用官方 websocket shape，支持 access token refresh、priority service tier、encrypted reasoning continuation 和 streamed reasoning summary persistence。
- OpenRouter chat-completions / responses providers。
- OpenAI-compatible image generation / image edit provider。
- Claude provider。
- Brave Search provider，包括 image / video / news verticals。
- provider-backed media helpers。

Provider 层会在 provider-neutral `ChatMessage` / `FileItem` 历史基础上做请求期 translation。Provider pricing 配置位于根目录 `pricing/`，按 provider type 拆分；模型未配置价格时，系统只记录 token usage，不计算美元成本。

---

## Quick Start

### 1. Build

```bash
cargo build --workspace --release
```

Agent server binary：

```bash
target/release/agent_server
```

Host server binary：

```bash
target/release/stellaclaw
```

### 2. Configure

复制并编辑配置：

```bash
cp example_config.json config.json
```

关键字段：

- `version`：当前 config schema version，当前是 `0.12`。
- `agent_server.path`：`agent_server` binary 路径。
- provider 配置：Codex / Claude / OpenRouter 等。
- Web channel token：供 StellaCodeX 或外部 API 调用。
- workdir：conversation、session、workspace 和 runtime state 的持久化目录。

### 3. Run

```bash
target/release/stellaclaw --config config.json --workdir ./rundir
```

启动后，StellaCodeX 或 Web API 可以连接配置中的 Web channel 地址。

### 4. systemd

生产环境建议用 systemd 或其他 supervisor 管理 `stellaclaw`，确保崩溃后自动重启。

---

## Version Files

仓库从 `1.0.0` 开始使用 SemVer 管理根发布版本。根目录 [VERSION](VERSION) 是项目发布版本和 changelog 的唯一入口。

项目同时有两条独立 schema 版本线：

- `config` schema version：由 `stellaclaw/src/config/mod.rs` 的 `LATEST_CONFIG_VERSION` 管理，当前是 `0.12`。
- `workdir` schema version：由 `stellaclaw/src/upgrade/mod.rs` 的 `LATEST_WORKDIR_VERSION` 管理，当前是 `0.17`。

不要假设 `config`、`workdir` 和根发布版本必须相同。前两者是数据结构兼容版本；根 `VERSION` 是项目发布版本。

---

## CI/CD

| Workflow | Trigger | 做什么 |
|---|---|---|
| CI | push 到 `main`、pull request | `cargo fmt --all --check`、`cargo test --workspace --locked` |
| Release | `main` 上 CI 成功 | 如果根 `VERSION` 变化且 tag 不存在，构建 release binaries，创建 tag 和 GitHub Release |

Release 产物包括：

- `stellaclaw`
- `agent_server`
- StellaCodeX 2 desktop 包
- Electron updater metadata，例如 `latest*.yml` 和 blockmap

---

## Repository Layout

```text
agent_host/       # Host-facing abstractions
agent_server/     # Session process wrapper around stellaclaw_core
apps/stellacodeX/electron/ # React/Electron desktop client
core/             # Core SessionActor and provider/tool loop
docs/             # Documentation assets
pricing/          # Provider model pricing tables
stellaclaw/       # Host binary, channels, conversation runtime
```

---

## Development

常用检查：

```bash
cargo fmt --all --check
cargo test --workspace --locked
```

StellaCodeX 2 前端检查：

```bash
cd apps/stellacodeX/electron
npm run check
```

本地开发启动 StellaCodeX 2：

```bash
cd apps/stellacodeX/electron
npm start
```

开发态 Electron 不会自动检查更新；只有打包后的 app 才会通过 GitHub Release metadata 检查并下载更新。

---

Built with Rust. Designed for durable agents, real conversations, and long-running work.
