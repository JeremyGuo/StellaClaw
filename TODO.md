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

## Memory 三层模型与跨 Conversation 查询

### 目标问题

- [ ] 当前 `STELLACLAW.md` 同时承担长期事实、conversation handoff、用户偏好和临时计划，职责过宽，几轮后容易把稳定事实和当前状态混在一起。
- [ ] memory 不应暴露真实路径或要求模型维护目录结构。模型只表达“这是用户偏好 / 有意义 conversation 事实 / 当前 conversation 状态”，系统内部决定落盘位置、索引和注入预算。
- [ ] 跨 conversation 不应复制完整 transcript。已有 `.stellaclaw/log/<session_id>/all_messages.jsonl` 是完整历史权威来源；需要细节时通过只读查询或 background agent 回答。
- [ ] 通用 Agent 不能把长期任务空间假设成 repo。常驻共享层只保存用户偏好；跨任务事实进入 `Meaningful-Conversation-Memory`，默认查询式读取。

### 三层语义

1. `Shared-User-Memory`

- [ ] 常驻共享层。只记录这个用户长期如何合作，例如语言偏好、工程偏好、常用约束、稳定工作习惯。
- [ ] 每轮自动注入，但必须有很小预算。
- [ ] 用于回答“用户偏好是什么”以及约束当前 Agent 行为。
- [ ] 不确定、只对当前 turn 有用、当前任务进度、一次性命令输出不要写入。

2. `Meaningful-Conversation-Memory`

- [ ] 可查询层。保存跨 conversation 有意义的 conversation 事实和 conversation 短描述。
- [ ] `facts` 记录稳定事实，subject 可以是 Project A、某个客户、某个文档、某个数据集、某个 repo、某个业务流程。
- [ ] `descriptions` 记录每个 conversation 做过什么、涉及哪些 subject/entity、现在状态是什么、是否值得以后回查。
- [ ] 默认不全量注入 prompt；需要跨 conversation 找线索时，用工具 list/search。
- [ ] 它先回答“Project A 是什么”这类简单问题；facts 不够时，再回答“有哪些 conversation 可能和 Project A 有关”。

3. `In-Conversation-Memory`

- [ ] 内部层。它分为 conversation-level memory 和 session-level working memory，由系统维护，不作为共享长期记忆。
- [ ] conversation-level 保存同一个 conversation 下跨多次 session 仍有效的目标、约束、open threads、handoff；session-level 保存某一次 session/run 的 plan、临时步骤、工具批次状态和 rolling summary。
- [ ] `plan.md` 不能直接放在 `in_conversation/` 根下；它属于具体 `<session_id>`，因为同一个 conversation 下不同 session 的计划和工作模式可能不同。
- [ ] 不要求模型直接改路径；压缩、turn 结束、工具结果完成时由 SessionActor 更新。
- [ ] 它只服务当前 conversation 的连续性，另一个 conversation 默认不直接读取它；跨 conversation 细节交给独立的 Cross Conversation Ask 系统处理。

### 当前目录对比

当前已有：

```text
<workdir>/
  rundir/
    .stellaclaw/
      USER.md                         # 已有：全局用户元数据；可作为 Shared-User-Memory 兼容输入
      IDENTITY.md                     # 已有：全局身份信息
      skill/                          # 已有：runtime skill store
      skill_memory/                   # 已有：skill 相关全局历史迁移/存储，不复用为 agent memory
  conversations/
    <conversation_id>/
      .stellaclaw/
        STELLACLAW.md                 # 已有：历史 project memory；后续作为兼容输入，不再新增为独立层
        USER.md                       # 已有/迁移保留：workspace 级用户元数据
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
        profile.md                    # Shared-User-Memory，模型可维护的长期用户偏好
        index.json                    # user memory hash、大小、常驻注入摘要 hash
      meaningful_conversations/
        facts.jsonl                   # Meaningful-Conversation-Memory facts，每行一个稳定事实
        subjects.json                 # subject/entity alias、最近访问、fact_ids、短摘要
        descriptions.jsonl            # Meaningful-Conversation-Memory conversation 描述索引
        index.json                    # facts/descriptions manifest、hash、大小、last_compacted_at
  conversations/
    <conversation_id>/
      .stellaclaw/
        memory_v1/
          meaningful_conversation_memory.json # 单条 conversation profile；默认注入本 conversation 的所有 session
          in_conversation/
            conversation.md           # conversation-level summary / handoff
            constraints.md            # 跨本 conversation 多次 session 仍有效的用户约束和纠正
            open_threads.md           # 跨 session 未完成事项、阻塞点、待确认问题
            index.json                # conversation-level hash、mtime、last_injected_turn、大小
            sessions/
              <session_id>/
                plan.md               # 当前 session/run 的计划、下一步、工具批次状态
                summary.md            # 当前 session/run 的 rolling summary
                scratch.md            # 当前 session/run 临时 notes；压缩时可丢弃或合并
                index.json            # session-level hash、mtime、last_injected_turn、大小
```

迁移关系：

- [ ] 不新增 transcript 副本；继续复用 `conversations/<id>/.stellaclaw/log/<session_id>/all_messages.jsonl`。
- [ ] `STELLACLAW.md` 保留为兼容输入。后续可由迁移/整理工具把稳定内容拆成 `Meaningful-Conversation-Memory` 的 facts，把当前状态拆成 conversation description 或 `In-Conversation-Memory`。
- [ ] 新增目录统一使用 `memory_v1/`，明确这是第一版 memory 持久化格式；后续更好的 memory 系统可以新增 `memory_v2/` 或写迁移，不和 v1 混用。
- [ ] `rundir/.stellaclaw/USER.md` / conversation `.stellaclaw/USER.md` 保留为 profile metadata；新增 `rundir/memory_v1/user/profile.md` 只存模型可维护的用户偏好。
- [ ] `.stellaclaw/skill/` 是 skill store，`.stellaclaw/skill_memory/` 不作为 agent memory 使用；旧 `.skill/` / `.skill_memory/` 只作为迁移输入。

### 文件内容和维护

- [ ] `rundir/memory_v1/user/profile.md`：用户长期偏好。系统每轮常驻注入它的压缩摘要；工具端限制大小，重复偏好用替换更新，不追加同义句。
- [ ] `rundir/memory_v1/meaningful_conversations/facts.jsonl`：Meaningful-Conversation-Memory 的稳定事实库，所有 conversation / session / agent 共同维护的一条一条 records。每行包含 `id`、`subject`、`text`、`source_conversation_id`、`citations`、`reason`、`category`、`confidence`、`created_at`、`updated_at`。同一 subject 的同义事实应更新旧 id，不重复追加。
- [ ] `rundir/memory_v1/meaningful_conversations/subjects.json`：fact 查询索引，记录 name、aliases、summary、fact_ids、last_seen_at。用户问“Project A 是什么”时先查这里，再取相关 facts；不要把所有 facts 常驻注入。
- [ ] `rundir/memory_v1/meaningful_conversations/descriptions.jsonl`：所有有意义 conversation 的短描述索引，是由每个 conversation 的 `meaningful_conversation_memory.json` 同步出的全局查询 records。每行包含 `conversation_id`、`title`、`description`、`subjects`、`tags`、`status`、`updated_at`、`last_session_id`。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/meaningful_conversation_memory.json`：该 conversation 的唯一权威 conversation-level 描述/profile，包含 `conversation_id`、`title`、`description`、`subjects`、`tags`、`status`、`updated_at`、`last_session_id`、`related_fact_ids`。它默认注入该 conversation 下所有 session，帮助新 session 知道“这个 conversation 是什么”；不要在这里堆一条条 facts。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/in_conversation/conversation.md`：当前 conversation 的长期连续性摘要和 handoff，不保存某一次 session 的详细执行计划。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/in_conversation/constraints.md`：当前 conversation 内跨 session 仍有效的用户约束、纠正和边界。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/in_conversation/open_threads.md`：当前 conversation 内跨 session 仍未完成、阻塞或待确认的事项。
- [ ] `conversations/<id>/.stellaclaw/memory_v1/in_conversation/sessions/<session_id>/*.md`：某一次 session/run 内部连续性。系统在压缩、工具批次结束、用户纠正约束后更新；不共享，不作为跨 conversation fact；新 session 不应把旧 session 的 plan 当作当前计划。

### 工具

- [ ] 新增 provider-neutral `memory` 工具，只操作 `Shared-User-Memory`。工具使用逻辑字段，不暴露真实路径。

```json
{
  "type": "function",
  "function": {
    "name": "memory",
    "description": "Search or update shared user preferences. Do not use it for facts, current-conversation plans, or raw transcript lookup.",
    "parameters": {
      "type": "object",
      "properties": {
        "command": {
          "type": "string",
          "enum": ["search", "list", "record", "update", "delete"],
          "description": "Memory operation."
        },
        "query": {
          "type": "string",
          "description": "Search query for user preferences."
        },
        "text": {
          "type": "string",
          "description": "Durable user preference text for record/update."
        },
        "memory_id": {
          "type": "string",
          "description": "Existing user preference id for update/delete."
        },
        "reason": {
          "type": "string",
          "description": "Why this should be durable user preference memory."
        },
        "limit": {
          "type": "number",
          "description": "Maximum results for search/list. Tool enforces a conservative upper bound."
        }
      },
      "required": ["command"]
    }
  }
}
```

- [ ] 新增 `meaningful_conversation_memory` 工具，用来查询/维护第二层。它负责 facts 的查询与写入，也负责 list/search 有意义 conversation；返回受预算限制的 facts 或短描述，不返回完整 transcript。

```json
{
  "type": "function",
  "function": {
    "name": "meaningful_conversation_memory",
    "description": "Search or update meaningful conversation memory: durable facts and meaningful conversation descriptions. Facts are queried on demand and are not injected by default.",
    "parameters": {
      "type": "object",
      "properties": {
        "command": {
          "type": "string",
          "enum": ["search_facts", "record_fact", "update_fact", "delete_fact", "list_sessions", "search_sessions"],
          "description": "Meaningful conversation memory operation."
        },
        "query": {
          "type": "string",
          "description": "Search text, subject, alias, or task name."
        },
        "subject": {
          "type": "string",
          "description": "Fact subject/entity, such as Project A, a customer, a document, a dataset, a repo, or a business process."
        },
        "text": {
          "type": "string",
          "description": "Durable fact text for record_fact/update_fact."
        },
        "fact_id": {
          "type": "string",
          "description": "Existing fact id for update_fact/delete_fact."
        },
        "citations": {
          "type": "array",
          "items": { "type": "string" },
          "description": "Source references, conversation ids, file paths, or message anchors supporting the fact."
        },
        "reason": {
          "type": "string",
          "description": "Why this fact should be durable meaningful conversation memory."
        },
        "confidence": {
          "type": "string",
          "enum": ["low", "medium", "high"]
        },
        "status": {
          "type": "string",
          "description": "Optional status filter for list_sessions/search_sessions, such as active, completed, blocked, archived."
        },
        "limit": {
          "type": "number",
          "description": "Maximum facts or descriptions to return. Tool enforces a conservative upper bound."
        }
      },
      "required": ["command"]
    }
  }
}
```

- [ ] 保留/新增只读 `session_history_lookup` 作为当前 conversation 的精确回查工具；跨 conversation 细节不要让当前 Agent 直接扫其他 conversation 的完整 JSONL，应交给独立的 Cross Conversation Ask 工具。

### 调用时机

- [ ] 用户表达稳定偏好、长期合作方式或纠正通用行为时，调用 `memory(record)`。
- [ ] 出现跨 conversation 仍有价值的稳定事实时，调用 `meaningful_conversation_memory(record_fact)`；例如“Project A 是什么”、“某客户约定”、“某数据集口径”、“某 repo 的长期约束”。
- [ ] 用户问简单跨 conversation 问题时，先 `meaningful_conversation_memory(search_facts, query=...)`；如果 facts 足够，直接答。
- [ ] fact 查询不够但可能在旧对话里时，调用 `meaningful_conversation_memory(search_sessions/list_sessions)` 找候选 conversation；具体细节交给独立 Cross Conversation Ask 工具。
- [ ] 当前 session/run 的计划、下一步、工具批次状态写入 `In-Conversation-Memory/sessions/<session_id>/`；跨本 conversation 多次 session 仍有效的约束、open threads、handoff 写入 `In-Conversation-Memory` conversation-level 文件；都不要写入 Shared-User-Memory 或 Meaningful-Conversation-Memory facts。

### Prompt 和压缩影响

- [ ] system prompt 中新增 `MemoryInstructions`：清楚说明三层边界，尤其是 Shared-User-Memory、Meaningful-Conversation-Memory 与内部 `In-Conversation-Memory` 的区别。
- [ ] 每轮 user-side runtime context 中只常驻注入：Shared-User-Memory 的小预算摘要、当前 conversation 的 `meaningful_conversation_memory.json` 单条描述、当前 In-Conversation-Memory conversation-level 摘要、当前 active session 的 session-level 摘要。Meaningful-Conversation-Memory facts 和其他 conversation descriptions 不默认全量注入，只通过工具 search/list。
- [ ] 压缩前不复制 transcript；先确保 `SessionStateStore::save` 已 flush `all_messages.jsonl`。
- [ ] 压缩产物先写入 `In-Conversation-Memory/sessions/<session_id>/summary.md`；只有跨 session 仍有效的目标、约束、open threads、handoff 才提升到 conversation-level `conversation.md` / `constraints.md` / `open_threads.md`。同时更新本 conversation 的单条 `meaningful_conversation_memory.json`，并把它同步到全局 meaningful_conversations descriptions index。
- [ ] 压缩 prompt 应区分三类输出：继续当前 conversation 的 summary、建议写入 Meaningful-Conversation-Memory facts 的候选、更新本 conversation 的单条 meaningful conversation description。真正写 facts 必须通过 `meaningful_conversation_memory` 工具或系统校验流程。
- [ ] `RuntimeMetadataState` 需要新增 memory component hashes：`user_preference_snapshot`、`meaningful_conversation_memory_manifest`、`in_conversation_memory_manifest`、`active_session_memory_manifest`。User preference 变化按预算注入；Meaningful-Conversation-Memory 变化只刷新查询索引；In-Conversation conversation-level 和 active session-level 变化下一轮立即生效。

### In-Conversation-Memory Workflow

- [ ] 新 session 启动时创建 `in_conversation/sessions/<session_id>/`，并读取 conversation-level `conversation.md`、`constraints.md`、`open_threads.md` 作为起始上下文；不要复制旧 session 的 `plan.md`。
- [ ] 如果是从 crash / release 恢复同一个 `<session_id>`，可以恢复该 session 的 `plan.md` / `summary.md`；如果是新的 `<session_id>`，旧 session-level memory 只作为审计/回查材料，不默认注入。
- [ ] turn 结束时，系统根据本 turn 的 user message、assistant answer、tool batch result 更新 active session 的 `plan.md` / `summary.md` / `scratch.md`。这些更新由 SessionActor 内部完成，不要求模型调用文件工具。
- [ ] 工具批次完成时，只把仍影响后续执行的结果写入 active session memory。例如“测试 X 失败，原因 Y，下一步 Z”；不要把完整 stdout/stderr 复制进去。
- [ ] 用户纠正当前 conversation 约束时，先写入 active session memory；如果约束跨 session 仍有效，再提升到 `constraints.md`。
- [ ] session-level 到 conversation-level 的提升规则必须保守：只有明确跨 session 仍有效的目标、约束、open thread、handoff 才提升；临时计划、尝试路径、一次性工具结果留在 session-level。
- [ ] `open_threads.md` 使用短条目维护，每条包含 `id`、`status`、`subject`、`next_action`、`last_session_id`、`updated_at`。完成后标记 done 或移到短 completed section，避免无限增长。
- [ ] `conversation.md` 是给新 session 的 handoff，不是日志。它只保留当前 conversation 的目标、当前已确认状态、关键上下文、仍需注意的边界。
- [ ] active session 的 `plan.md` 是工作计划，可以频繁改写；每轮应尽量保持“当前目标 / 已完成 / 下一步 / 阻塞点”四块，不保留历史流水账。
- [ ] `scratch.md` 是临时缓冲区，不进入跨 session 默认上下文；压缩时可以删除、合并到 `summary.md`，或提升少量内容到 conversation-level。
- [ ] `index.json` 记录 conversation-level 和 session-level 文件的 hash、mtime、size、last_injected_turn、active_session_id。prompt 构造时根据 hash 判断是否需要重新读取。
- [ ] 注入顺序应固定：Shared-User-Memory -> `meaningful_conversation_memory.json` -> conversation-level In-Conversation-Memory -> active session-level memory -> 当前 user message。越靠后的内容越具体。

### Meaningful-Conversation-Memory 检索算法

- [ ] v1 使用本地 hybrid retrieval，不只做 grep。算法为：metadata/alias filter + BM25Okapi lexical search + dense embedding cosine search + Reciprocal Rank Fusion + lightweight rule rerank。
- [ ] 索引对象包括 `facts.jsonl` 中的 facts 和 `descriptions.jsonl` 中的 conversation descriptions。两类 document 进入同一检索管线，但按 command 区分返回：`search_facts` 返回 facts，`search_sessions` / `list_sessions` 返回 conversation descriptions。
- [ ] 每个 searchable document 至少包含 `id`、`kind`、`subject`、`aliases`、`title`、`text`、`tags`、`status`、`conversation_id`、`updated_at`、`confidence`、`citations`。字段缺失时用空值，不阻塞索引。
- [ ] query normalization 要做大小写归一、标点清理、基础中英文 tokenization、subject alias 展开。`subjects.json` 是 alias / subject catalog，不进入 prompt 全量注入。
- [ ] metadata/alias filter 先用 `subject`、`aliases`、`tags`、`conversation_id`、`status`、`updated_at`、`confidence` 缩小候选或加召回 boost。它不是唯一过滤条件，避免 alias 漏配导致召回失败。
- [ ] BM25 使用 BM25Okapi，默认参数 `k1 = 1.2`、`b = 0.75`。它负责精确关键词、专有名词、文件名、项目名、客户名、接口名等 lexical match。
- [ ] embedding search 使用 dense embedding + cosine similarity。v1 数据量小可以先本地 brute-force topK，不必先引入向量数据库；后续数据量变大再考虑 HNSW。
- [ ] BM25 topK 和 vector topK 不直接相加分数，使用 Reciprocal Rank Fusion 合并，默认 `k = 60`：

```text
rrf_score(doc) = sum(1 / (60 + rank_i(doc)))
```

- [ ] RRF 后做轻量规则 rerank：subject exact match、alias match、high confidence、recent update 加小分；low confidence、obsolete/archived、同 conversation 重复结果扣小分。规则 boost 不能大到盖过 RRF 主排序。
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

- [ ] Rust v1 实现建议：BM25 用 `tantivy`；中文分词先用简单 tokenizer，后续可接 `jieba-rs` 或 Tantivy tokenizer；embedding vectors 先本地文件/SQLite 存储并 brute-force cosine；索引可以从 `memory_v1/meaningful_conversations/*.jsonl` 重建。
- [ ] 所有 search 结果必须有严格预算：默认 top 5，最大 top 20；单条结果裁剪；总返回字符数受工具端硬上限控制。需要更多信息时，让 Agent 发更具体的 search 或使用 Cross Conversation Ask。

### 大小控制

- [ ] `memory` 工具端限制单条 record、单次返回、单文件和总 records JSONL 大小；超过上限时拒绝写入或要求合并旧 record。
- [ ] Shared-User-Memory 的常驻注入预算保持很小，例如 2KB-4KB；超出后先压缩/合并偏好，不把全文塞进 prompt。
- [ ] Meaningful-Conversation-Memory facts 默认零常驻注入；只允许 `meaningful_conversation_memory(search_facts)` 返回 top N 相关结果，并受单次返回预算限制。
- [ ] 单条 fact 控制在约 1KB；同 subject 的重复 fact 走 update，不走追加。
- [ ] `meaningful_conversation_memory.json` 控制在约 2KB；全局 descriptions list 只返回 title/description/subjects/status，不返回 transcript。
- [ ] `In-Conversation-Memory` conversation-level 文件各自控制在约 4KB-8KB；`sessions/<session_id>/plan.md` 控制在约 4KB，`summary.md` 控制在约 8KB；压缩时滚动改写，而不是无限追加。旧 session 的 session-level 文件可以保留供恢复/审计，但不常驻注入新 session。

## Cross Conversation Ask 工具

- [ ] `ask_conversation` 是独立系统工具，不属于 memory 工具组。Memory 负责建立 facts 和 conversation 索引；ask 负责按需启动 background agent 到目标 conversation 查细节。
- [ ] 当前 Agent 必须先用 `meaningful_conversation_memory(search_sessions/list_sessions)` 或用户直接给出的 conversation id 确定目标 conversation，再调用 `ask_conversation`。
- [ ] background agent 只读目标 conversation 的 `.stellaclaw/memory_v1/meaningful_conversation_memory.json`、`.stellaclaw/memory_v1/in_conversation/conversation.md`、必要的 `.stellaclaw/memory_v1/in_conversation/sessions/<session_id>/summary.md` 和已有 `.stellaclaw/log/<session_id>/all_messages.jsonl`；不要让当前 Agent 直接扫其他 conversation 的完整 JSONL。
- [ ] `ask_conversation` 的返回作为普通 tool result 回到当前 Agent，可以直接用于回答用户，也可以提示当前 Agent 是否需要把新确认的稳定事实写回 `meaningful_conversation_memory(record_fact)`。

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
          "description": "Target conversation id, usually obtained from meaningful_conversation_memory search_sessions/list_sessions."
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
