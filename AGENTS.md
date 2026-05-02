# AGENTS.md

本仓库后续实现以 [ROAD_MAP.md](ROAD_MAP.md) 为主要指导文件。

## 目标

- 围绕 `ROAD_MAP.md` 的方向，逐步实现一个支持多端的 Agent Server。
- 整个项目使用 Rust 开发。
- 优先把核心链路做对，再补外围能力。
- 保持实现可落地、可维护、可演进。

## 协作原则

- 默认先参考 `ROAD_MAP.md`，再决定实现方案、目录结构和重构方向。
- 不要主动往 `ROAD_MAP.md` 增加过多细节。
- 如果认为某个设计、约束、流程、里程碑应该写入 `ROAD_MAP.md`，必须先询问用户，再决定是否加入。
- 如果任务没有明确要求，不要把临时想法、备选方案、局部实现细节直接写进 `ROAD_MAP.md`。

## 工程原则

- 默认使用 Rust 完成服务端、核心模块和配套工程代码。
- 避免过度设计。
- 避免为了“未来可能需要”提前引入过多抽象、层次或通用框架。
- 优先选择直接、清晰、容易验证的实现。
- 新增抽象前，先说明它正在解决的当前问题，而不是假设未来问题。
- 尽量让代码结构贴近真实运行边界和真实职责，减少额外心智负担。
- 像强工程团队一样工作：先把边界、职责、数据流和失败路径想清楚，再做实现。

## Provider pricing 约定

- 新增或修改 provider 时，必须同步检查根目录 [pricing](pricing) 下对应 `<provider_type>.json` 的模型价格表。
- pricing 文件是按 provider type 拆分的模型价格字典，编译时嵌入 `stellaclaw_core`；模型未配置价格时，系统应视为不计算美元成本，而不是解析 provider 返回值中的成本字段。
- 价格表应尽量覆盖该 provider 实际会使用的模型；订阅制、固定包月、外部计费或无法按 token 可靠归因的 provider 可以保持空字典。
- core 内部的成本计算应集中在独立的价格管理模块中，例如 `PriceManager`，provider 只传入 `ModelConfig` 和 `TokenUsage` 获取成本，避免把 pricing JSON、字段名或计算公式散落到各个 provider 实现里。
- pricing 单位使用 USD / 1M tokens，字段为 `cache_read`、`cache_write`、`input`、`output`。

## ChatMessage / FileItem 约定

- 修改 conversation history、provider 消息翻译、工具结果、用户附件、assistant 生成文件相关逻辑时，必须遵守 [core/src/session_actor/chat_message.md](core/src/session_actor/chat_message.md) 和 [core/src/session_actor/file_item.md](core/src/session_actor/file_item.md)。
- `ChatMessage` 是 provider-neutral 的持久化历史；不要把 provider-specific 的 `input_image`、`input_file`、multipart 字段、Responses item 等直接存进 `ChatMessage`。
- `ChatMessageItem::File` / `ToolResultContent.file` 必须使用标准 `FileItem` payload。用户上传、工具产物、assistant 生成文件优先 materialize 成稳定 `file://` URI；`data:` URL 主要作为 request-time normalization / transport 形态，避免把大 base64 长期写入 JSONL 历史。
- 新增写入 `FileItem` 的代码时，应尽量补齐 `name`、精确 `media_type`，图片应尽量补齐 `width` / `height`；文件不可用时保留 `FileItem` 并设置 `FileState::Crashed { reason }`，不要静默丢弃。
- provider 需要 base64、URL、file id 或 multipart 时，应在发送请求前由 media normalizer / provider translator 从 `FileItem` 派生，不改变持久化的 `ChatMessage` 形态。

## 文档约定

- `ROAD_MAP.md` 负责方向、边界和阶段性目标。
- 具体实现细节优先放在代码、提交说明或独立设计文档中，而不是持续膨胀 `ROAD_MAP.md`。
- 当文档与代码不一致时，修正时应优先澄清差异来源，并与用户确认是否需要更新 roadmap。

## 版本维护约定

本仓库从 `1.0.0` 开始使用 SemVer 维护项目发布版本。根目录 [VERSION](VERSION) 是项目发布版本和 changelog 的唯一入口；每次做非平凡变更时，都应先阅读它，避免回退已经记录过的行为。

项目同时有两条 schema 版本线，它们和根目录发布版本相互独立：

- `config` schema version：由 [stellaclaw/src/config/mod.rs](stellaclaw/src/config/mod.rs) 中的 `LATEST_CONFIG_VERSION` 管理，对应用户配置 JSON 顶层 `version` 字段。
- `workdir` schema version：由 [stellaclaw/src/upgrade/mod.rs](stellaclaw/src/upgrade/mod.rs) 中的 `LATEST_WORKDIR_VERSION` 管理，对应运行目录里的 `STELLA_VERSION` 文件。

不要假设 `config`、`workdir` 和根目录发布版本必须相同。`config` / `workdir` 是数据结构兼容版本；根 `VERSION` 是项目发布版本。

### 如果改了 workdir 结构

凡是改变运行目录内持久化布局或 schema 的变更，包括 `conversations/`、`rundir/`、`.log/stellaclaw/`、session state、cron、skill store、workspace seed、runtime binding 等，都必须：

- 更新 [stellaclaw/src/upgrade/mod.rs](stellaclaw/src/upgrade/mod.rs) 的 `LATEST_WORKDIR_VERSION`。
- 在 [stellaclaw/src/upgrade/](stellaclaw/src/upgrade) 下新增或更新顺序 upgrade step，并在 `upgrade_workdir` 的 upgrader 链里注册。
- 确保旧 workdir 可以按版本顺序升级到最新版本；不要跳过中间迁移。
- 更新根 [VERSION](VERSION) changelog，明确说明这是 `workdir` schema 变更。

运行目录里的版本文件规则：

- 新 Stellaclaw workdir 使用 `STELLA_VERSION`。
- 旧 PartyClaw workdir 可能只有 `VERSION`；迁移后应写入 `STELLA_VERSION`，旧 `VERSION` 只作为兼容输入。
- 不要把某个本地部署目录里的 `STELLA_VERSION` 当作项目发布版本；它只描述该 workdir 的 schema。

### 如果改了 config 结构

凡是新增、删除、重命名或改变用户配置 JSON 字段含义的变更，都必须：

- 更新 [stellaclaw/src/config/mod.rs](stellaclaw/src/config/mod.rs) 的 `LATEST_CONFIG_VERSION`。
- 在 [stellaclaw/src/config/loaders/](stellaclaw/src/config/loaders) 下新增或更新 loader，保留旧 config 的升级路径。
- 更新 [example_config.json](example_config.json) 以及相关部署样例中的配置形态。
- 更新根 [VERSION](VERSION) changelog，明确说明这是 `config` schema 变更。

本地部署配置文件里的顶层 `version` 字段只描述 config schema，不是项目发布版本。

### 根 VERSION bump 策略

根 [VERSION](VERSION) 使用 SemVer：

- 不兼容的运行行为或 schema 破坏性变化：提升 `MAJOR`，`MINOR=0`，`PATCH=0`。
- config schema 变化、新功能、重要用户可见行为变化：提升 `MINOR`，`PATCH=0`。
- workdir-only schema 迁移、bug fix、文档/CI 改进：提升 `PATCH`。

每次 bump 都要在 [VERSION](VERSION) 顶部新增对应 changelog 小节，并说明是否影响 `config`、`workdir` 或两者都不影响。

### 发布前检查

在新增根 [VERSION](VERSION) 小节前，必须检查 GitHub 上一个版本是否已经成功发布：

- 查询上一个版本对应的 tag 和 GitHub Release，例如 `v1.0.0`。
- 如果上一个版本已经有非 draft release，新版本 changelog 只记录本次增量。
- 如果上一个版本没有成功发布，不要让 changelog 断档；把未发布版本的小节内容合并进当前新版本小节，再发布当前版本。
- 推送后继续检查当前版本的 CI / Release workflow，确认 tag 和 GitHub Release 最终创建成功。

## 服务重启约定

本仓库的 `stellaclaw` 服务可能正在运行当前 conversation 自身。重启时建议使用 `nohup bash -c 'sleep 10 && kill <old_pid> && sleep 2 && /path/to/stellaclaw --config ... --workdir ...' &` 的延迟方式，然后**立即结束当前对话**，避免重启过程中自身进程被 kill 导致会话中断或状态不一致。
