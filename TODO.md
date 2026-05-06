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

## Memory v1：长期记忆与短期记忆

### 设计目标

Memory v1 只保留两条清晰路径：

长期记忆保存跨 turn / session / conversation 仍有用的用户偏好、事实、约束和 handoff。它由 `memory_write` / `memory_search` / `memory_update` / `memory_delete` 维护，持久化为 `user` / `public` / `conversation` 三个 scope 的 entries。

短期记忆保存当前 session/run 的计划、最近工具结果、压缩后的 provider history 和下一步状态。它继续使用现有 `update_plan`、provider current context compression、`all_messages.jsonl` 和 `current_messages.jsonl`。

完整历史权威来源是 `conversations/<id>/.stellaclaw/log/<session_id>/all_messages.jsonl`。provider 下一轮实际看到的短上下文是 `current_messages.jsonl`：较早历史由结构化 compaction result 渲染成简短上下文，最近几轮消息和工具结果保留原文。

### 核心抽象

- [x] Host service registry 骨架已接入 Conversation，现有 Core / ManagedSession / Skill / Cron host action 已通过 service 边界路由。
- [ ] Memory v1 按 host service 架构接入。Conversation 只负责 host service 路由、session 生命周期和 channel 投影；memory 文件、索引和算法由 MemoryService 层处理。
- [ ] `MemoryService` 实现为 host service，挂到 `HostServiceRegistry`，处理 `memory_write`、`memory_search`、`memory_update`、`memory_delete`。
- [ ] `MemoryClient` 是 Conversation 持有的轻量 clone，用于把 user/public 请求非阻塞投递给 workdir 级 `WorkdirMemoryManager`。
- [ ] `WorkdirMemoryManager` 是 workdir 级共享管理线程，统一维护 `user` / `public` memory、索引、user memory 后台压缩、重试和 notification snapshot。
- [ ] `conversation` scope 由 `MemoryService` 使用 conversation-local backend 维护，数据在当前 conversation workdir 内。
- [ ] `MemoryBackend` 是具体存储和检索算法接口。v1 backend 可以是文件 + 可重建索引；后续算法实现同一个 trait。

```rust
trait MemoryBackend {
    fn write(&self, request: MemoryWriteRequest) -> Result<MemoryWriteResult>;
    fn update(&self, request: MemoryUpdateRequest) -> Result<MemoryEntry>;
    fn delete(&self, request: MemoryDeleteRequest) -> Result<MemoryDeleteResult>;
    fn search(&self, request: MemorySearchRequest) -> Result<Vec<MemorySearchHit>>;
    fn prompt_context(&self, request: MemoryContextRequest) -> Result<MemoryPromptBlock>;
}
```

### Scope 语义

`user` scope 是全局用户长期合作记忆。它存模型可维护的长期合作偏好、行为约束、通用纠正和工作方式。它作为条目管理，渲染进 system prompt snapshot；两次 compact 之间，当前 provider context 使用同一个 `scope="user"` snapshot。

`public` scope 是跨 conversation 的长期记忆，适合“Project A 是什么”、客户约定、数据口径、长期任务事实等。

`conversation` scope 是当前 conversation 的长期记忆，可被同一个 conversation 下的所有 session / foreground / background / subagent 使用，适合当前 conversation 的目标、长期约束、关键事实和 handoff。

`memory_search` 默认搜索当前 `conversation` scope 和全局 `public` scope。`user` 作为 system prompt snapshot 的一部分直接提供。

`memory_write` 写入后返回新 entry 和同 scope / 同 subject 附近的 5 条相关记忆。模型在同一个回合里检查这些结果；发现重复、冲突或过期内容时，继续调用 `memory_update` / `memory_delete` / `memory_write` 整理。

长期 memory 保存密集自然语言事实或逻辑，单条硬限制约 1KB。工具端负责限制体积、裁剪返回和提示整理方式。

### Workdir 格式

当前已有：

```text
<workdir>/
  rundir/
    .stellaclaw/
      USER.md                         # 用户个人信息/profile metadata
  conversations/
    <conversation_id>/
      .stellaclaw/
        USER.md                       # workspace 级用户个人信息/profile metadata
        log/
          <session_id>/
            session.json              # SessionActorPersistedState
            all_messages.jsonl        # 完整历史
            current_messages.jsonl    # provider 当前上下文
```

- [x] `SessionStateStore` 已保存 `session.json`、`all_messages.jsonl` 和 `current_messages.jsonl`。
- [x] `all_messages.jsonl` 已作为完整历史来源，`current_messages.jsonl` 已作为 provider 当前短上下文来源。
- [x] `USER.md` profile 文件已由 runtime metadata 读取并进入 system prompt snapshot。
- [ ] `USER.md` prompt 注入改成 meta snapshot / notification；全文内容由模型按需读取。

Memory v1 新增：

```text
<workdir>/
  rundir/
    memory_v1/
      user/
        entries.jsonl                 # user scope active 条目
        manifest.json                 # hash、大小、last_updated_at、压缩状态、阈值快照
        compaction.json               # 后台压缩任务状态、失败次数、next_retry_at
      public/
        entries.jsonl                 # public scope 长期记忆
        subjects.json                 # subject/entity aliases、最近访问、entry ids、短摘要
        index.json                    # public manifest、hash、大小、last_indexed_at
  conversations/
    <conversation_id>/
      .stellaclaw/
        memory_v1/
          conversation/
            entries.jsonl             # conversation scope 长期记忆
            index.json                # conversation manifest、hash、大小、last_indexed_at
```

- [ ] 新增目录统一使用 `memory_v1/`，明确这是第一版 memory 持久化格式。
- [ ] `USER.md` 保留为用户个人信息/profile metadata；模型可维护的合作偏好进入 `rundir/memory_v1/user/entries.jsonl`。

### 文件内容和维护

- [ ] `rundir/memory_v1/user/entries.jsonl`：用户长期合作记忆 active 条目。系统自动填 `id`、`created_at`、`updated_at`；模型通过 `memory_write(scope="user")` 新增，通过 `memory_update` / `memory_delete` 按 id 整理。
- [ ] `rundir/memory_v1/user/manifest.json`：记录 entries hash、rendered hash、rendered bytes、last_compacted_at、soft/hard threshold、dirty 状态和最后一次注入版本。
- [ ] `rundir/memory_v1/user/compaction.json`：记录 `state`、`attempts`、`last_error`、`next_retry_at`、`last_input_hash`、`last_output_hash`，用于失败重试和相同输入去重。
- [ ] `rundir/memory_v1/public/entries.jsonl`：`public` scope 的长期记忆。系统自动填 `id`、`source_conversation_id`、`source_session_id`、`created_at`、`updated_at`；模型提供 `subject`、`text`、`tags`。
- [ ] `rundir/memory_v1/public/subjects.json`：public 查询索引，记录 subject、aliases、summary、entry ids、last_seen_at。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/conversation/entries.jsonl`：`conversation` scope 的长期记忆。同 conversation 的所有 session/agent 可以搜索和维护。
- [ ] `index.json` 保存 hash、mtime、size、last_injected_turn、last_indexed_at、active_session_id 等系统字段，由 runtime 维护。

### 工具

- [ ] 新增 `memory_search`，查询当前 conversation scope 和全局 public scope 的长期记忆。参数保持 `query` + `limit`；返回 top 5-10 条，包含 `id`、`scope`、`subject`、`text`、`updated_at` 和排序提示。

```json
{
  "type": "function",
  "function": {
    "name": "memory_search",
    "description": "Search long memory entries. Results may include duplicates or conflicts; resolve them explicitly when needed.",
    "parameters": {
      "type": "object",
      "properties": {
        "query": { "type": "string", "description": "Natural language search query." },
        "limit": { "type": "number", "description": "Maximum results. Tool enforces a conservative upper bound." }
      },
      "required": ["query"]
    }
  }
}
```

- [ ] 新增 `memory_write`，统一写 `user` / `public` / `conversation`。工具端把它实现成幂等写入：先自动检索同 scope / 同 subject 附近候选，再用轻量判重模型或本地小模型判断是否重复、是否应该写入。

```json
{
  "type": "function",
  "function": {
    "name": "memory_write",
    "description": "Write a compact long-memory entry for durable facts, preferences, constraints, or handoff. The tool performs internal dedupe before creating an entry.",
    "parameters": {
      "type": "object",
      "properties": {
        "scope": {
          "type": "string",
          "enum": ["user", "public", "conversation"],
          "description": "user is system-prompt user memory; public is cross-conversation memory; conversation is current-conversation memory."
        },
        "subject": { "type": "string", "description": "Optional short subject/entity name." },
        "text": { "type": "string", "description": "Compact durable memory text. Tool enforces about 1KB per entry." },
        "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional compact tags." },
        "reason": { "type": "string", "description": "Why this should be durable long memory." }
      },
      "required": ["scope", "text"]
    }
  }
}
```

- [ ] `memory_write` 内部流程：

```text
request
  -> normalize(scope, subject, text, tags)
  -> hard size check, about 1KB per entry
  -> exact hash dedupe in target scope
  -> internal memory_search(target scope, subject + text, candidate_limit <= 10)
  -> dedupe judge model/local model
  -> duplicate: return status="duplicate" and the matched entry id
  -> create: append entry and return status="created" plus the new entry
  -> uncertain/related: create only when judge says the new text adds durable information; return at most return_related_limit related ids
```

- [ ] 判重模型输入只包含新 entry 草稿和裁剪后的候选条目；候选超过上限直接丢弃，不把长列表交给模型。判重输出只允许 `duplicate` / `create` / `create_with_related` / `reject_too_vague`。
- [ ] `memory_write` tool result 有硬预算：默认最多返回 1 条 duplicate entry 或 1 条 created entry，加上最多 3-5 条 related ids/short snippets；超过预算的候选不返回。
- [ ] 重复时不创建新 entry，只返回类似：

```json
{
  "status": "duplicate",
  "duplicate_id": "m_123",
  "message": "A highly similar memory already exists; no new entry was created."
}
```

- [ ] 新增 `memory_update` / `memory_delete`，让模型在读到重复、过期或冲突 memory 时主动整理；工具端按 id 操作并写审计记录。

```json
{
  "type": "function",
  "function": {
    "name": "memory_update",
    "description": "Replace an existing user, public, or conversation memory entry after resolving duplication, conflict, or stale content.",
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
    "description": "Delete or tombstone an obsolete, duplicate, or wrong user, public, or conversation memory entry.",
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

- [ ] 保留/新增只读 `session_history_lookup` 作为当前 conversation 的精确回查工具；跨 conversation 细节通过独立的 Cross Conversation Ask 工具查询。

### 调用时机

- [ ] 用户表达稳定偏好、长期合作方式或纠正通用行为时，调用 `memory_write(scope="user", text=...)`。
- [ ] 当前 conversation 下所有 session 都应该知道的事实、目标、约束、handoff，调用 `memory_write(scope="conversation", ...)`。
- [ ] 其他 conversation 也应该能查到的稳定事实，调用 `memory_write(scope="public", ...)`。
- [ ] 用户问简单跨 conversation 或当前 conversation 长期事实问题时，先 `memory_search(query=...)`；工具会搜索当前 `conversation` 和全局 `public`。
- [ ] `memory_write` 会自动去重；模型看到 `status="duplicate"` 时停止重复写入。模型读到 `memory_search` 返回的冲突或过期条目时，调用 `memory_update` / `memory_delete` / `memory_write` 整理。
- [x] 当前 run 的短期执行状态已由现有 `update_plan` 内存态维护。
- [ ] 跨 session 仍有用的 conversation 状态提升为 `memory_write(scope="conversation", ...)`。

### User Memory 压缩与容错

- [ ] MemorySystem 独立维护 user memory 压缩。超过 hard threshold 后由 WorkdirMemoryManager 立即排队压缩；每日定时任务负责 soft threshold 整理。
- [ ] soft threshold 默认 4KB：超过后标记 `dirty`，每天固定时间尝试压缩一次。soft 压缩失败不影响会话启动和工具调用，第二天再重试。
- [ ] hard threshold 默认 8KB：超过后立即触发后台压缩。压缩成功后按 notification 逻辑通知已有 conversation；新 conversation 直接加载压缩后的条目。
- [ ] hard 压缩失败时 conversation 继续运行，user memory 保持原样注入。系统记录错误和输入 hash，设置 `next_retry_at = now + 6h`；到期后重试。
- [ ] 如果压缩输出没有比输入更短，立刻再重试一次。第二次仍没有变短时，降低压缩目标或提高本次阈值/预算，记录 `threshold_override`，让同一输入进入有界重试流程。
- [ ] 压缩目标是删除重复、过时、一次性、偏离 user scope 的信息，并把同类偏好合并成更密集条目。若压缩会丢失仍有用的用户偏好或合作约束，应保留信息并允许暂时超过 soft threshold。
- [ ] 压缩复用现有 Provider 调用链。配置中指定用于 memory compaction 的 provider alias / model alias；缺省可以复用当前默认 low-cost text model。
- [ ] 压缩输入全量包含当前 active user memory 条目，输出仍是条目列表，保留稳定 id 或给出 id 映射，便于后续按 id 更新和删除。
- [ ] user memory 的 system prompt 渲染使用紧凑列表：

```text
User Memory:
* [u_001] 用户主要使用中文沟通，技术名词可保留英文。
* [u_014] 用户偏好直接、可落地、少废话的工程方案。
```

- [ ] user memory 更新、压缩完成或当前 system prompt 中的 user memory snapshot 落后时，在下一个用户消息送入模型前插入 host notification。notification 使用类似 git diff 的紧凑格式，带 id。

### Search 算法

- [ ] v1 使用本地 hybrid retrieval：metadata/alias filter + BM25Okapi lexical search + dense embedding cosine search + Reciprocal Rank Fusion + lightweight rule rerank。
- [ ] 索引对象包括 `public/entries.jsonl` 和当前 conversation 的 `conversation/entries.jsonl`。
- [ ] 每个 searchable document 至少包含 `id`、`scope`、`subject`、`aliases`、`text`、`tags`、`conversation_id`、`updated_at`。字段缺失时用空值，索引继续构建。
- [ ] query normalization 做大小写归一、标点清理、基础中英文 tokenization、subject alias 展开。`subjects.json` 是 alias / subject catalog。
- [ ] metadata/alias filter 先用 `scope`、`subject`、`aliases`、`tags`、`conversation_id`、`updated_at` 缩小候选或加召回 boost。最终召回同时保留 lexical/vector 路径。
- [ ] BM25 使用 BM25Okapi，默认参数 `k1 = 1.2`、`b = 0.75`。
- [ ] embedding search 使用 dense embedding + cosine similarity。v1 数据量小先本地 brute-force topK；后续数据量变大再考虑 HNSW。
- [ ] BM25 topK 和 vector topK 使用 Reciprocal Rank Fusion 合并，默认 `k = 60`。
- [ ] RRF 后做轻量规则 rerank：subject exact match、alias match、scope match、recent update 加小分；obsolete/archived、同 conversation 重复结果扣小分。
- [ ] Rust v1 实现建议：BM25 用 `tantivy`；中文分词直接用 `jieba-rs`；embedding vectors 先本地文件/SQLite 存储并 brute-force cosine。

默认检索流程：

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

### Context 组装与压缩

- [x] `update_plan` 继续作为当前 session 的内存态执行计划，由 SessionActor 持有。
- [x] provider current context compression 继续负责解决 turn 数多、工具结果多导致的上下文膨胀；完整历史仍以 `all_messages.jsonl` 为权威来源，压缩后的 provider context 存在 `current_messages.jsonl`。
- [ ] system prompt 中新增 `MemoryInstructions`：清楚定义 `memory_write(scope="user"|"public"|"conversation")`、`memory_search`、`memory_update`、`memory_delete` 的边界。
- [ ] 每轮 runtime context 使用当前 system prompt snapshot，并追加按预算检索出的 conversation/public memory、现有 provider current context compression 和当前 `update_plan`。其他 conversation 的内容通过 `memory_search` 或 Cross Conversation Ask 查询。
- [ ] 达到压缩阈值时，先确保 `SessionStateStore::save` 已 flush `all_messages.jsonl`，再基于当前 provider context 构造一次 compaction provider request。
- [x] 当前 compaction provider request 已使用原本要压缩的上下文作为前缀，在末尾追加一条 `role="user"` 的压缩请求消息；这个请求只用于生成压缩结果，不写入 `all_messages.jsonl`。
- [ ] 压缩请求消息示例：

```json
{
  "role": "user",
  "content": "请压缩以上上下文，返回严格 JSON，不要输出 Markdown。保留用户目标、关键事实、已完成事项、当前状态、下一步计划和仍需注意的约束。不要复制大段日志、完整代码或完整工具输出。preserved_tool_call_ids 只填写后续很大可能还会继续用到的工具调用；如果你已经从工具结果中提取出后续需要的结论，就不要再保留该工具结果。请谨慎保留工具结果，避免压缩后上下文仍然过长。"
}
```

- [ ] 压缩输出使用结构化 JSON，字段内容仍是自然语言，便于后续渲染成 provider context：

```json
{
  "summary": "本轮到目前为止的压缩摘要，覆盖目标、关键事实、重要决策、涉及文件/模块、已完成工作和主要错误处理。",
  "current_state": "当前正在做什么，最近一次有意义的工具结果是什么，当前阻塞点是什么。",
  "plan": "下一步计划。可以是自然语言段落，也可以是短列表。",
  "preserved_tool_call_ids": [
    "call_abc123"
  ]
}
```

- [ ] `preserved_tool_call_ids` 只引用当前 session history 中真实存在的 tool call id；系统校验 id，忽略不存在的 id，并按预算从 `all_messages.jsonl` / 当前 history 精确保留对应 tool call 参数和 tool result。
- [ ] `preserved_tool_call_ids` 不渲染成 compacted block 里的 “Preserved Tool Results” 文本列表；它只作为系统回填对应 tool call / tool result pair 的控制字段。
- [ ] 系统把压缩 JSON 的 `summary` / `current_state` / `plan` 渲染成一条 compacted provider history block，写入 `current_messages.jsonl`；最近几轮消息和被 `preserved_tool_call_ids` 命中的工具调用参数与结果按预算保留，完整 transcript 继续由 `all_messages.jsonl` 回查。
- [ ] 默认高保真 recent context 保留预算调整为压缩阈值的 10%；显式配置的 `compression_retain_recent_tokens` 仍按配置值生效。
- [ ] 压缩后的 provider context 拼接顺序固定：current system prompt snapshot -> budgeted conversation/public memory -> compacted provider history block -> 当前 `update_plan` plan -> 当前 user message。
- [ ] 长期 memory 注入预算按相关性分配：至少 80% 给 `scope="conversation"` 的搜索结果，至少 20% 给 `scope="public"` 的搜索结果；某一侧结果不足时，剩余预算可被另一侧使用。
- [ ] `RuntimeMetadataState` 需要新增 memory component hashes：`user_memory_snapshot`、`conversation_memory_manifest`、`public_memory_manifest`。

### Memory Config

- [ ] config 增加 `memory` 小节，后续实现时必须走 config schema 迁移和 `VERSION` changelog：

```json
{
  "memory": {
    "enabled": true,
    "user_compaction_model_alias": "default",
    "dedupe_model_alias": "default",
    "user_soft_threshold_bytes": 4096,
    "user_hard_threshold_bytes": 8192,
    "user_retry_after_failed_hard_compaction_secs": 21600,
    "user_soft_compaction_schedule": "daily",
    "write_candidate_limit": 10,
    "write_return_related_limit": 5,
    "tool_result_max_bytes": 4096
  }
}
```

- [ ] threshold 按渲染后注入文本预算计算。系统同时记录 raw bytes 和 rendered bytes，触发压缩以 rendered bytes 为准。
- [ ] 工具端限制单条 entry、`memory_write` 候选数、单次返回、单文件和总 records JSONL 大小；超过上限时拒绝写入、丢弃多余候选、要求合并旧 entry，或自动截断并在 tool result 中说明。

## Cross Conversation Ask 工具

- [ ] `ask_conversation` 是独立系统工具。Memory 负责建立 public/conversation 索引；ask 负责按需启动 background agent 到目标 conversation 查细节。
- [ ] 当前 Agent 必须先用 `memory_search(query=...)`、conversation index，或用户直接给出的 conversation id 确定目标 conversation，再调用 `ask_conversation`。
- [ ] background agent 读取目标 conversation 的 `.stellaclaw/memory_v1/conversation/entries.jsonl`、压缩后的 provider current context 和已有 `.stellaclaw/log/<session_id>/all_messages.jsonl`，并把整理后的答案返回当前 Agent。
- [ ] `ask_conversation` 的返回作为普通 tool result 回到当前 Agent，可以直接用于回答用户，也可以提示当前 Agent 是否需要把新确认的稳定事实写回 `memory_write(scope="public")`。

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
