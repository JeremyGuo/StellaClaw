# TODO

## Remote Mode visibility 重构

- [x] 移除 Remote Mode 对 `sshfs` 的依赖。Remote Mode 下普通文件、搜索、补丁、下载和 shell 类工具默认作用于远程 workspace；本地 conversation workspace 只承载用户附件、channel 可下载产物、runtime 元数据和少量本地特殊文件。
- [x] 保持本地 workspace 和远程 workspace 使用同一个 workspace-relative 路径 namespace。跨边界同步时，工具参数必须是目录内相对路径；禁止绝对路径、空路径、`..`、home 展开和任何 symlink escape。
- [x] 新增 `shell_make_visible`。仅在 fixed Remote Mode 下暴露，把本地同相对路径文件或目录同步到远程同相对路径，使远程 shell 和远程文件工具可以访问用户附件或本地特殊文件。
- [x] 新增 `attachment_make_visible`。仅在 fixed Remote Mode 下暴露，把远程同相对路径文件或目录同步回本地同相对路径，使 `<attachment>...</attachment>` 和 channel 附件发送可以访问远程产物。
- [x] visibility 工具返回简洁状态即可，例如 `ok`、`kind`、`bytes_copied`、`duration_ms`、`copied|skipped`；不需要返回目标路径，因为本地和远程使用同一个相对路径。
- [x] visibility copy 使用 timeout 作为主要保护，避免预扫大目录导致额外成本。复制时必须使用临时路径，成功后原子 rename；失败、超时或中断后尽量清理临时文件。
- [x] 明确 symlink 策略。visibility copy 直接拒绝 symlink 路径和包含 symlink 的目录树；目标端也拒绝覆盖 symlink 或穿过 symlink parent，不跟随 symlink 指向内容，也不复制 symlink 本身。
- [x] 复制目录或大量小文件时优先探测双方能力。双方都有 `tar`/`gzip` 等能力时可压缩流式传输；缺少解压能力时 fallback 到普通 `scp -r`，不能把压缩包留在目标端后宣称同步完成。
- [x] 根据 `ToolRemoteMode` 动态生成工具 schema 和 system prompt。Local/selectable 模式不暴露 `shell_make_visible` / `attachment_make_visible`，也不注入相关说明；fixed Remote Mode 才暴露工具和跨边界可见性规则。
- [x] 明确 fixed Remote Mode 下默认本地执行的特殊文件/目录。带路径参数的文件类工具在 fixed Remote Mode 下默认远程执行，但 `.stellaclaw/` 及其内部特殊路径保持本地：`.stellaclaw/STELLACLAW.md`、`.stellaclaw/attachments/`、`.stellaclaw/output/`、`.stellaclaw/shared/`、`.stellaclaw/skill_memory/`、`.stellaclaw/skill/`、`.stellaclaw/USER.md`、`.stellaclaw/IDENTITY.md`；指向当前本地 workspace 内的绝对路径或 `file://` 路径也保持本地。其他相对路径默认远程。
- [x] workdir upgrade 只 materialize fixed Remote Mode 下必须本地可见的关键路径，并在下一步 schema 迁移中统一移动到 `.stellaclaw/` 下：`.stellaclaw/`、`.stellaclaw/output/`、`.stellaclaw/attachments/`、`.stellaclaw/shared/`、`.stellaclaw/STELLACLAW.md`、`.stellaclaw/skill_memory/`。普通项目文件和目录不从远程拉回本地，继续保留在远程 workspace。
- [x] fixed Remote Mode 初始化时默认读取远程 workspace 对应目录的 `AGENTS.md`，并把内容注入 session 上下文；读取失败时给出明确 runtime notice，不要求模型自己先用 shell 读取。
- [x] 调整 `<attachment>` 解析边界。Remote Mode 下 `<attachment>` 仍只引用本地 materialized 文件；远程生成的文件必须先调用 `attachment_make_visible`。
- [x] 清理现有 tool result 完整落盘兜底。工具应自行保证 model-visible 输出不会超过运行时限制；统一 cap 仍可截断保护，但不要再把完整超长结果保存到 `.stellaclaw/output/tool_results/`。
- [x] 重新审视 `apply_patch` 的 artifact 输出。Codex-format patch 本地执行时不需要持久化 stdout/stderr；unified patch 可直接返回 `git apply` 的简短 stdout/stderr/returncode，避免默认写入 `.stellaclaw/output/apply_patch/`。

## Dirac Token Usage 优化借鉴项

- [ ] 设计 provider-neutral 的稳定行锚点协议。读文件、搜索结果、函数抽取结果都应返回稳定 anchor，后续编辑只引用 anchor 和新内容，减少重复输出旧代码。
- [ ] 实现精准编辑工具。支持单文件/多文件批量 `replace`、`insert_before`、`insert_after`，按 anchor 校验当前文件内容，避免 line number 漂移导致反复重试。
- [ ] 收紧读文件与搜索结果的上下文预算。全文件读取要有大小上限，支持行范围读取；重复读取同一文件时用文件 hash 判断未变化则只返回简短提示；搜索结果要限制匹配数、上下文行数、总字节数和超长行。
- [ ] 增加 AST / symbol 级上下文工具。优先提供 file skeleton、指定函数抽取、符号引用/定义定位、符号级替换能力，用结构化局部上下文替代整文件上下文。
- [ ] 支持多文件/多编辑批处理。允许模型在一次 tool call 内提交多个文件的非重叠编辑，并在工具端统一校验、预览、应用和诊断，减少 roundtrip 和重复历史输入。

## Memory v1 简化抽象与跨 Conversation 查询

### 目标问题

- [ ] 当前 `STELLACLAW.md` 同时承担长期事实、conversation handoff、用户偏好和临时计划，职责过宽，几轮后容易把稳定事实和当前状态混在一起。
- [ ] memory 不应暴露真实路径或要求模型维护目录结构。模型只表达“更新用户常驻记忆 / 搜索长期记忆 / 写入 public 或 shared 长期记忆”，系统内部决定落盘位置、索引和注入预算。
- [ ] 跨 conversation 不应复制完整 transcript。已有 `.stellaclaw/log/<session_id>/all_messages.jsonl` 是完整历史权威来源；需要细节时通过只读查询或 background agent 回答。
- [ ] 通用 Agent 不能把长期任务空间假设成 repo。`public` / `shared` 的 subject 可以是任意项目、客户、文档、数据集、流程、偏好或现实任务，不要求和代码仓库绑定。
- [ ] v1 先做一个简单但带抽象的 memory 系统；后续 BM25、embedding、graph、reranker 等算法都挂在同一个接口后面，不改变 prompt 和工具协议。

### 核心抽象

- [ ] 新增 provider-neutral `MemorySystem` / `MemoryBackend` 抽象。v1 必须支持的唯一核心接口是“获取要插入 system prompt / runtime context 的 memory block”。

```rust
trait MemorySystem {
    fn system_prompt_context(&self, request: MemoryContextRequest) -> Result<MemoryPromptContext>;
}
```

- [ ] `MemoryPromptContext` 只返回预算内的文本块和 manifest/hash，不返回真实文件路径。prompt 构造方只负责把它拼进 system prompt 或 runtime context。
- [ ] v1 backend 可以只是文件 + 可重建索引；后续更好的算法实现同一个 trait，例如 `HybridSearchMemoryBackend`、`GraphMemoryBackend`。
- [ ] Memory 系统只负责长期 memory 和当前 conversation/session 的压缩上下文；完整 transcript 继续由已有 `all_messages.jsonl` 负责。

### Scope 语义

- [ ] `user` memory 是特殊常驻块，不走 `memory_write(scope=...)`。它不是 `USER.md`：`USER.md` 存用户个人信息/profile metadata；user memory 存模型可维护的长期合作偏好、行为约束、通用纠正和工作方式。
- [ ] `public` scope：当前 conversation 的公开长期记忆。默认可被同一个 conversation 下的所有 session / foreground / background / subagent 使用；适合当前 conversation 的目标、长期约束、关键事实和 handoff。
- [ ] `shared` scope：跨 conversation 的长期记忆。默认不全量常驻注入；通过 `memory_search(scope="shared")` 查询，适合“Project A 是什么”、客户约定、数据口径、长期任务事实等。
- [ ] 除 `user`、`public`、`shared` 外，v1 不暴露其他 scope 给模型。session 内短记忆是系统内部状态，不作为 `memory_write` scope。
- [ ] 每条长期 memory 只保存密集自然语言事实或逻辑，不保存实际代码和大段工具输出；单条硬限制约 1KB，超过由工具端拒绝或截断。

### USER.md 优化

- [ ] `USER.md` 只作为用户个人信息/profile metadata，例如姓名、身份、长期身份信息、联系方式类元信息；不要把临时任务、项目事实、当前计划或模型行为偏好塞进去。
- [ ] user memory 存“模型应该如何和这个用户长期合作”的偏好和纠正，例如语言、工程风格、回答习惯、长期协作约束。它可以从 `USER.md` 派生少量提示，但不是 `USER.md` 的全文镜像。
- [ ] 当前 `USER.md` 不应再每轮全量加载进 prompt。默认只注入 meta：路径/存在性、mtime/hash、大小、短摘要、最近更新时间、是否有未读更新。
- [ ] 如果模型需要 `USER.md` 全文，必须自己用 `file_read` 读取；system prompt 要说明 `USER.md` 的全文不是常驻上下文。
- [ ] `USER.md` 更新后，runtime 只发“meta changed” notification，例如 hash、mtime、size、changed sections、简短摘要；不把完整 diff 或全文塞进 prompt。
- [ ] 如果模型需要具体更新内容，先根据 notification 判断是否相关，再自己 `file_read` 读取对应文件或片段。
- [ ] `USER.md` 和 `rundir/memory_v1/user/profile.md` 需要分别维护 hash/mtime/summary，避免 profile metadata 更新导致 user memory 常驻块失效或反过来污染。

### 当前目录对比

当前已有：

```text
<workdir>/
  rundir/
    .stellaclaw/
      USER.md                         # 已有：全局用户个人信息/profile metadata；不作为 user memory 全文注入
      IDENTITY.md                     # 已有：全局身份信息
      skill/                          # 已有：runtime skill store
      skill_memory/                   # 已有：skill 相关全局历史迁移/存储，不复用为 agent memory
  conversations/
    <conversation_id>/
      .stellaclaw/
        STELLACLAW.md                 # 已有：历史 project memory；后续作为兼容输入，不再新增为独立层
        USER.md                       # 已有/迁移保留：workspace 级用户个人信息/profile metadata
        IDENTITY.md                   # 已有/迁移保留：workspace 级身份信息
        attachments/                  # 已有：用户上传和 channel incoming 附件
        output/                       # 已有：assistant / provider / tool 可下载产物
        shared/                       # 已有：跨 conversation 共享目录入口
        skill_memory/                 # 已有：skill 相关历史迁移/存储，不复用为 agent memory
        log/
          <session_id>/
            session.json              # 已有：SessionActorPersistedState
            all_messages.jsonl        # 已有：完整 conversation history，继续复用
            current_messages.jsonl    # 已有：压缩后 provider context history
```

需要新增的内部结构：

```text
<workdir>/
  rundir/
    memory_v1/
      user/
        profile.md                    # user memory 常驻块；存合作偏好，不存个人信息 profile
        index.json                    # user memory hash、大小、last_updated_at、注入摘要 hash
      shared/
        entries.jsonl                 # shared scope 长期记忆，每行一条
        subjects.json                 # subject/entity alias、最近访问、entry ids、短摘要
        index.json                    # shared manifest、hash、大小、last_indexed_at
  conversations/
    <conversation_id>/
      .stellaclaw/
        memory_v1/
          public/
            profile.md                # 当前 conversation 的单条描述；默认注入本 conversation 所有 session
            entries.jsonl             # public scope 长期记忆，每行一条
            index.json                # public manifest、hash、大小、last_indexed_at
          sessions/
            <session_id>/
              compacted.md            # 当前 session 的自然语言压缩上下文
              index.json              # session-level hash、mtime、last_injected_turn、大小
```

迁移关系：

- [ ] 不新增 transcript 副本；继续复用 `conversations/<id>/.stellaclaw/log/<session_id>/all_messages.jsonl`。
- [ ] `STELLACLAW.md` 保留为兼容输入。后续可由迁移/整理工具把稳定内容拆成 `public` / `shared` entries，把当前 conversation 描述拆到 `public/profile.md`。
- [ ] 新增目录统一使用 `memory_v1/`，明确这是第一版 memory 持久化格式；后续更好的 memory 系统可以新增 `memory_v2/` 或写迁移，不和 v1 混用。
- [ ] `rundir/.stellaclaw/USER.md` / conversation `.stellaclaw/USER.md` 保留为用户个人信息/profile metadata，只注入 meta，不全量注入；新增 `rundir/memory_v1/user/profile.md` 只存模型可维护的用户偏好。
- [ ] `.stellaclaw/skill/` 是 skill store，`.stellaclaw/skill_memory/` 不作为 agent memory 使用；旧 `.skill/` / `.skill_memory/` 只作为迁移输入。

### 文件内容和维护

- [ ] `rundir/memory_v1/user/profile.md`：用户长期偏好。系统每轮常驻注入小预算版本；更新必须通过 `memory_user_save()`，用 diff/patch 方式改写，避免无限追加同义句。
- [ ] `rundir/memory_v1/shared/entries.jsonl`：`shared` scope 的长期记忆。每行由系统自动填 `id`、`source_conversation_id`、`source_session_id`、`created_at`、`updated_at`；模型只提供 `subject`、`text`、`tags`。
- [ ] `rundir/memory_v1/shared/subjects.json`：shared 查询索引，记录 subject、aliases、summary、entry ids、last_seen_at。它是可重建索引，不常驻注入。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/public/profile.md`：当前 conversation 的单条描述/profile。默认注入该 conversation 下所有 session，帮助新 session 知道“这个 conversation 是什么”；不要在这里堆一条条 entries。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/public/entries.jsonl`：`public` scope 的长期记忆。所有同 conversation 的 session/agent 可以搜索和维护。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/sessions/<session_id>/compacted.md`：当前 session 的自然语言短记忆。它由压缩流程和 SessionActor 维护，不暴露为 `memory_write` scope。
- [ ] `index.json` 文件只保存 hash、mtime、size、last_injected_turn、last_indexed_at、active_session_id 等系统字段；不要让模型维护。

### 工具

- [ ] 新增 `memory_user_save`，专门维护 user 常驻记忆。它不接收 scope，不写事实库；工具返回简短 notification 和 diff，下一轮 prompt 自动看到更新后的 user memory。

```json
{
  "type": "function",
  "function": {
    "name": "memory_user_save",
    "description": "Update stable user memory. Use only for durable user preferences, collaboration style, and general corrections.",
    "parameters": {
      "type": "object",
      "properties": {
        "diff": {
          "type": "string",
          "description": "Unified diff or compact patch against the current user memory block."
        },
        "reason": {
          "type": "string",
          "description": "Why this belongs in durable user memory."
        }
      },
      "required": ["diff"]
    }
  }
}
```

- [ ] 新增 `memory_search`，查询 `public` / `shared` 长期记忆。它返回 top 5-10 条，包含 `id`、`scope`、`subject`、`text`、`updated_at` 和排序提示；如果结果重复或冲突，模型自己消融后再调用 `memory_update` / `memory_delete` / `memory_write`。

```json
{
  "type": "function",
  "function": {
    "name": "memory_search",
    "description": "Search long memory entries. Results may include duplicates or conflicts; resolve them explicitly when needed.",
    "parameters": {
      "type": "object",
      "properties": {
        "query": {
          "type": "string",
          "description": "Natural language search query."
        },
        "scope": {
          "type": "string",
          "enum": ["public", "shared", "all"],
          "description": "Where to search. Defaults to all."
        },
        "limit": {
          "type": "number",
          "description": "Maximum results. Tool enforces a conservative upper bound."
        }
      },
      "required": ["query"]
    }
  }
}
```

- [ ] 新增 `memory_write`，只写 `public` / `shared`。写入前不强制 search，减少一轮开销；读到重复或冲突时再由模型调用 `memory_update` / `memory_delete` 修正。

```json
{
  "type": "function",
  "function": {
    "name": "memory_write",
    "description": "Write a compact long-memory entry. Do not store code, raw transcripts, or large tool outputs.",
    "parameters": {
      "type": "object",
      "properties": {
        "scope": {
          "type": "string",
          "enum": ["public", "shared"],
          "description": "public is current-conversation memory; shared is cross-conversation memory."
        },
        "subject": {
          "type": "string",
          "description": "Short subject/entity name."
        },
        "text": {
          "type": "string",
          "description": "Compact durable memory text. Tool enforces about 1KB per entry."
        },
        "tags": {
          "type": "array",
          "items": { "type": "string" },
          "description": "Optional compact tags."
        },
        "reason": {
          "type": "string",
          "description": "Why this should be durable long memory."
        }
      },
      "required": ["scope", "text"]
    }
  }
}
```

- [ ] 新增 `memory_update` / `memory_delete`，让模型在读到重复、过期或冲突 memory 时主动整理；工具端按 id 操作并写审计记录。

```json
{
  "type": "function",
  "function": {
    "name": "memory_update",
    "description": "Replace an existing public/shared memory entry after resolving duplication, conflict, or stale content.",
    "parameters": {
      "type": "object",
      "properties": {
        "memory_id": { "type": "string" },
        "text": { "type": "string" },
        "reason": { "type": "string" }
      },
      "required": ["memory_id", "text"]
    }
  }
}
```

```json
{
  "type": "function",
  "function": {
    "name": "memory_delete",
    "description": "Delete or tombstone an obsolete, duplicate, or wrong public/shared memory entry.",
    "parameters": {
      "type": "object",
      "properties": {
        "memory_id": { "type": "string" },
        "reason": { "type": "string" }
      },
      "required": ["memory_id"]
    }
  }
}
```

- [ ] 保留/新增只读 `session_history_lookup` 作为当前 conversation 的精确回查工具；跨 conversation 细节不要让当前 Agent 直接扫其他 conversation 的完整 JSONL，应交给独立的 Cross Conversation Ask 工具。

### 调用时机

- [ ] 用户表达稳定偏好、长期合作方式或纠正通用行为时，调用 `memory_user_save(diff=...)`。
- [ ] 当前 conversation 下所有 session 都应该知道的事实、目标、约束、handoff，调用 `memory_write(scope="public", ...)`。
- [ ] 其他 conversation 也应该能查到的稳定事实，调用 `memory_write(scope="shared", ...)`；例如“Project A 是什么”、“某客户约定”、“某数据集口径”、“长期任务事实”。
- [ ] 用户问简单跨 conversation 问题时，先 `memory_search(scope="shared", query=...)`；如果结果足够，直接答。
- [ ] `memory_search` 读到重复、冲突或过期条目时，模型自己判断并调用 `memory_update` / `memory_delete` / `memory_write` 消融；写入新条目时不预先强制 search。
- [ ] 当前 session/run 的短期计划、下一步和工具批次状态由系统写入 `sessions/<session_id>/compacted.md` 或内存态 plan，不进入 `public` / `shared`，除非它已经变成跨 session 仍有效的事实。

### Prompt 和压缩影响

- [ ] system prompt 中新增 `MemoryInstructions`：清楚定义 `memory_user_save`、`public`、`shared` 的边界，让模型知道不同 scope 应该用哪个工具保存。
- [ ] 每轮 runtime context 只常驻注入：user memory 小预算块、当前 conversation 的 `public/profile.md`、当前 session 的 `compacted.md`。`shared/entries.jsonl` 和其他 conversation 的内容不默认全量注入，只通过 `memory_search` 查询。
- [ ] 压缩前不复制 transcript；先确保 `SessionStateStore::save` 已 flush `all_messages.jsonl`。
- [ ] 压缩输出保持简单：只要求自然语言 `summary` 和自然语言 `plan`，不要定义过细结构。它们可以用 JSON 包起来便于解析，但字段内容是自由文本。
- [ ] 生成下一轮短上下文时，按顺序拼接：system prompt + `MemorySystem::system_prompt_context()` + compacted summary + compacted plan + 当前 `update_plan` 的内存态 plan。`update_plan` 的结果要转成自然语言附加到 plan 后面，避免压缩时丢掉当前工作计划。
- [ ] 只有跨 session 仍有效的目标、约束、事实或 handoff 才提升到 `public` 或 `shared`；临时尝试、一次性工具结果和当前执行步骤留在 session compacted context。
- [ ] `RuntimeMetadataState` 需要新增 memory component hashes：`user_memory_snapshot`、`public_memory_manifest`、`shared_memory_manifest`、`active_session_compacted_manifest`。User/public/session 变化下一轮可立即注入；shared 变化只刷新查询索引，除非搜索命中。

### Session Compact Workflow

- [ ] 新 session 启动时创建 `memory_v1/sessions/<session_id>/`，并读取 user memory、当前 conversation public profile、当前 session compacted context 作为起始上下文；不要复制旧 session 的 plan。
- [ ] 如果是从 crash / release 恢复同一个 `<session_id>`，可以恢复该 session 的 `compacted.md` 和内存态 plan；如果是新的 `<session_id>`，旧 session-level compact 只作为审计/回查材料，不默认注入。
- [ ] turn 结束或触发压缩时，系统根据本 turn 的 user message、assistant answer、工具结果和当前 `update_plan` 更新 active session 的 `compacted.md`。这些更新由 SessionActor 或 MemorySystem 内部完成，不要求模型调用文件工具。
- [ ] 工具批次完成时，只把仍影响后续执行的结果写入 session compact。例如“测试 X 失败，原因 Y，下一步 Z”；不要把完整 stdout/stderr 复制进去。
- [ ] `compacted.md` 是给当前 session 继续工作的短上下文，不是日志。它可以包含自然语言 summary、当前方向、重要约束、下一步和阻塞点，但不要求固定章节。
- [ ] prompt 注入顺序应固定：user memory -> public conversation profile -> active session compact -> 当前 update_plan plan -> 当前 user message。越靠后的内容越具体。

### Search 算法

- [ ] v1 使用本地 hybrid retrieval，不只做 grep。算法为：metadata/alias filter + BM25Okapi lexical search + dense embedding cosine search + Reciprocal Rank Fusion + lightweight rule rerank。
- [ ] 索引对象包括 `shared/entries.jsonl` 和当前 conversation 的 `public/entries.jsonl`；当前 conversation `public/profile.md` 可以作为一条特殊 document 进入 public search。
- [ ] 每个 searchable document 至少包含 `id`、`scope`、`subject`、`aliases`、`text`、`tags`、`conversation_id`、`updated_at`。字段缺失时用空值，不阻塞索引。
- [ ] query normalization 要做大小写归一、标点清理、基础中英文 tokenization、subject alias 展开。`subjects.json` 是 alias / subject catalog，不进入 prompt 全量注入。
- [ ] metadata/alias filter 先用 `scope`、`subject`、`aliases`、`tags`、`conversation_id`、`updated_at` 缩小候选或加召回 boost。它不是唯一过滤条件，避免 alias 漏配导致召回失败。
- [ ] BM25 使用 BM25Okapi，默认参数 `k1 = 1.2`、`b = 0.75`。它负责精确关键词、专有名词、文件名、项目名、客户名、接口名等 lexical match。
- [ ] embedding search 使用 dense embedding + cosine similarity。v1 数据量小可以先本地 brute-force topK，不必先引入向量数据库；后续数据量变大再考虑 HNSW。
- [ ] BM25 topK 和 vector topK 不直接相加分数，使用 Reciprocal Rank Fusion 合并，默认 `k = 60`：

```text
rrf_score(doc) = sum(1 / (60 + rank_i(doc)))
```

- [ ] RRF 后做轻量规则 rerank：subject exact match、alias match、scope match、recent update 加小分；obsolete/archived、同 conversation 重复结果扣小分。规则 boost 不能大到盖过 RRF 主排序。
- [ ] 默认检索流程：

```text
query
  -> normalize/tokenize
  -> alias expansion
  -> metadata/alias candidate boost
  -> BM25 top 50
  -> embedding cosine top 50
  -> RRF merge top 30
  -> lightweight rerank
  -> return top N under budget
```

- [ ] Rust v1 实现建议：BM25 用 `tantivy`；中文分词先用简单 tokenizer，后续可接 `jieba-rs` 或 Tantivy tokenizer；embedding vectors 先本地文件/SQLite 存储并 brute-force cosine；索引可以从 `memory_v1/shared/*.jsonl` 和当前 conversation `memory_v1/public/*.jsonl` 重建。
- [ ] 所有 search 结果必须有严格预算：默认 top 5，最大 top 20；单条结果裁剪；总返回字符数受工具端硬上限控制。需要更多信息时，让 Agent 发更具体的 search 或使用 Cross Conversation Ask。

### update_plan 状态

- [x] `update_plan` 工具 schema 仍定义在 `core/src/session_actor/tool_catalog/host_tools.rs`，但 plan 数据由 `SessionActor` 持有，不再存放在 `ConversationRuntime`。
- [x] `SessionActor` 解析 `update_plan` payload 后保存当前 session 的内存态 plan，并通过 `SessionEvent::PlanUpdated` 和带 plan snapshot 的 `SessionEvent::Progress` 推送给 Conversation。
- [x] Conversation 只负责把 SessionActor 发出的 plan 投影到 `TurnProgress.plan`，不再维护 `session_plans` 作为数据源。
- [x] plan 不持久化；正常 Turn 完成时清空；用户中断或可继续错误不主动清空。
- [x] active threshold compression 触发时，如果当前 session 还有 plan，会把当前 plan 作为 assistant-side compact context 拼到压缩后的 provider history，供同一 turn 继续使用；正常 Turn 完成清空 plan 时会移除这段 plan context。idle compaction 发生在 turn 结束后，plan 已为空。

### 大小控制

- [ ] 工具端限制单条 entry、单次返回、单文件和总 records JSONL 大小；超过上限时拒绝写入、要求合并旧 entry，或自动截断并在 tool result 中说明。
- [ ] user memory 常驻注入预算保持很小，例如 2KB-4KB；超出后先压缩/合并偏好，不把全文塞进 prompt。
- [ ] shared entries 默认零常驻注入；只允许 `memory_search(scope="shared")` 返回 top N 相关结果，并受单次返回预算限制。
- [ ] public profile 控制在约 2KB；public entries 不全量注入，只通过当前 conversation search 或必要的小预算摘要进入 context。
- [ ] 单条 public/shared entry 控制在约 1KB；同 subject 的重复、冲突、过期内容由模型在读到时通过 `memory_update` / `memory_delete` / `memory_write` 消融，不在写入时强制预查。
- [ ] `sessions/<session_id>/compacted.md` 控制在约 8KB；压缩时滚动改写，而不是无限追加。旧 session compact 可以保留供恢复/审计，但新 session 不默认注入旧 session compact。

## Cross Conversation Ask 工具

- [ ] `ask_conversation` 是独立系统工具，不属于 memory 工具组。Memory 负责建立 shared/public 索引；ask 负责按需启动 background agent 到目标 conversation 查细节。
- [ ] 当前 Agent 必须先用 `memory_search(scope="shared")`、conversation index，或用户直接给出的 conversation id 确定目标 conversation，再调用 `ask_conversation`。
- [ ] background agent 只读目标 conversation 的 `.stellaclaw/memory_v1/public/profile.md`、`.stellaclaw/memory_v1/public/entries.jsonl`、必要的 `.stellaclaw/memory_v1/sessions/<session_id>/compacted.md` 和已有 `.stellaclaw/log/<session_id>/all_messages.jsonl`；不要让当前 Agent 直接扫其他 conversation 的完整 JSONL。
- [ ] `ask_conversation` 的返回作为普通 tool result 回到当前 Agent，可以直接用于回答用户，也可以提示当前 Agent 是否需要把新确认的稳定事实写回 `memory_write(scope="shared")`。

```json
{
  "type": "function",
  "function": {
    "name": "ask_conversation",
    "description": "Ask a specific prior conversation for details. The system starts a background agent over that conversation and returns its answer to the current agent.",
    "parameters": {
      "type": "object",
      "properties": {
        "conversation_id": {
          "type": "string",
          "description": "Target conversation id, usually obtained from memory search or a conversation index."
        },
        "question": {
          "type": "string",
          "description": "Specific question to answer from the target conversation."
        },
        "max_answer_chars": {
          "type": "number",
          "description": "Maximum answer size. Tool must enforce an upper bound."
        },
        "include_citations": {
          "type": "boolean",
          "description": "Whether to include message/file anchors supporting the answer."
        }
      },
      "required": ["conversation_id", "question"]
    }
  }
}
```

- [ ] `ask_conversation` background agent 的答案必须有 `max_answer_chars` 和工具端硬上限；需要更多细节时再次 ask 更具体的问题。
