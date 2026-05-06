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
- [x] 明确 fixed Remote Mode 下默认本地执行的特殊文件/目录。带路径参数的文件类工具在 fixed Remote Mode 下默认远程执行，但以下 workspace-relative 路径保持本地：`.stellaclaw/`、`.output/`、`attachments/`、`shared/`、`STELLACLAW.md`；指向当前本地 workspace 内的绝对路径或 `file://` 路径也保持本地。其他相对路径默认远程。
- [x] workdir upgrade 只 materialize fixed Remote Mode 下必须本地可见的关键路径：`.stellaclaw/`、`.output/`、`attachments/`、`shared/`、`STELLACLAW.md`。普通项目文件和目录不从远程拉回本地，继续保留在远程 workspace。
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

## Memory 分层与压缩工作流

### 目标问题

- [ ] 解决当前 `STELLACLAW.md` 同时承担 project memory、session handoff、用户偏好和压缩后续航线索的问题。单文件职责过宽会导致几轮后上下文变糊：临时计划不该进 project memory，稳定事实又不该只留在可被压缩丢失的聊天历史里。
- [ ] 给模型一个专用 memory 语义工具，而不是继续要求它用普通 `file_read` / `file_write` / `apply_patch` 自己维护记忆文件。普通文件工具缺少 scope、去重、唯一替换、行号回显和即时刷新语义。
- [ ] memory 变更要能影响下一轮上下文。当前 `STELLACLAW.md` 通过 `observe_component_without_notice` 更新 notified snapshot，但不生成 runtime notice，通常要等 compaction promote 后才进入 canonical system snapshot；这对“下一轮不要变蠢”不够。

### 建议 Workdir 格式

实现该设计会改变 workdir 持久化结构，落地时必须提升 `LATEST_WORKDIR_VERSION`，新增 upgrade step，并更新根 `VERSION`。

```text
<workdir>/
  rundir/
    .stellaclaw/
      USER.md                         # 已有：全局用户元数据，继续作为身份/偏好高优先级输入
      IDENTITY.md                     # 已有：全局身份信息
    memory/
      user/
        *.md                          # 跨 workspace / conversation 的用户偏好、常用命令、稳定工作习惯
        index.json                    # 可选：文件 hash、mtime、最近加载行数、最后一次注入 turn
      repo/
        <repo_key>/
          facts.jsonl                 # 跨 conversation 的 repo 事实，每行一个结构化 fact
          notes/
            *.md                      # 可选：按主题组织的 repo 记忆，适合人工编辑
          index.json                  # repo identity、remote URL、最近 fact hash、last_seen
  conversations/
    <conversation_id>/
      STELLACLAW.md                   # 已有：当前 workspace/project 的稳定长期项目记录
      .stellaclaw/
        memory/
          session/
            plan.md                   # 当前任务计划、下一步、阻塞点；可以随 turn 更新
            summary.md                # 压缩后的滚动 handoff summary；压缩后优先读它
            notes/
              *.md                    # 当前会话临时 notes；不跨 conversation
            index.json                # session memory manifest：hash、mtime、last_injected_turn
          transcript/
            full.jsonl                # 可选：压缩前完整 transcript，供 read_file / transcript lookup 回查
```

文件职责：

- [ ] `rundir/memory/user/*.md`：只存用户级稳定偏好，不写项目事实，不写当前任务进度。每轮可自动注入有限预算，例如前 200 行或 4k chars。
- [ ] `rundir/memory/repo/<repo_key>/facts.jsonl`：只存可验证、相对稳定、跨会话有价值的 repo facts。每行字段建议为 `id`、`subject`、`fact`、`citations`、`reason`、`category`、`created_at`、`updated_at`、`confidence`、`source_conversation_id`。
- [ ] `conversations/<id>/STELLACLAW.md`：保留为 project memory，但只写长期项目事实、架构决策、稳定约定和 handoff-critical decisions；不写每轮状态和临时计划。
- [ ] `conversations/<id>/.stellaclaw/memory/session/plan.md`：当前任务工作台。适合保存“已完成/正在做/下一步/失败路径/用户刚刚纠正的约束”。
- [ ] `conversations/<id>/.stellaclaw/memory/session/summary.md`：压缩产物的稳定落点。每次 compaction 后更新，下一轮 prompt 自动带入或优先摘要注入。
- [ ] `conversations/<id>/.stellaclaw/memory/transcript/full.jsonl`：不进常规 prompt，只在 summary 提示里给出回查路径，避免为了保真持续携带完整历史。

### 需要新增的工具

- [ ] 新增 provider-neutral `memory` 工具，由 `SessionActor` 本地执行，不走 provider-specific translator。工具使用虚拟路径，不暴露真实绝对路径给模型。
- [ ] 虚拟路径固定为 `/memory/user/...`、`/memory/repo/...`、`/memory/project/STELLACLAW.md`、`/memory/session/...`。禁止 `..`、空 segment、绝对路径、symlink escape；禁止跨 scope rename。
- [ ] `memory` 工具必须支持目录 `view`，并在创建前鼓励模型先 view 对应目录，避免重复记忆。
- [ ] `str_replace` 必须要求 `old_str` 在目标文件中恰好出现一次；失败时返回匹配次数和行号，不做模糊替换。
- [ ] `create` 默认不覆盖；需要修改已有记忆时用 `str_replace` / `insert`。
- [ ] `/memory/repo/facts.jsonl` 的 `create` / `insert` 应接受 JSON object 或 JSONL line，并做字段校验；普通 markdown repo notes 可放 `/memory/repo/notes/*.md`。

MVP tool schema：

```json
{
  "type": "function",
  "function": {
    "name": "memory",
    "description": "Manage scoped StellaClaw memory files. Use it for user preferences, current-session plans/summaries, stable project memory, and verified repo facts. Do not use ordinary file tools for these memory paths.",
    "parameters": {
      "type": "object",
      "properties": {
        "command": {
          "type": "string",
          "enum": ["view", "create", "str_replace", "insert", "delete", "rename"],
          "description": "Memory operation to perform."
        },
        "path": {
          "type": "string",
          "description": "Virtual memory path. Must start with /memory/user/, /memory/session/, /memory/project/, or /memory/repo/. Used by all commands except rename may use old_path."
        },
        "view_range": {
          "type": "array",
          "items": { "type": "number" },
          "minItems": 2,
          "maxItems": 2,
          "description": "Optional for view. 1-based inclusive [start_line, end_line]."
        },
        "file_text": {
          "type": "string",
          "description": "Required for create. Full file content to create. Fails if the file already exists."
        },
        "old_str": {
          "type": "string",
          "description": "Required for str_replace. Exact string to replace; must occur exactly once."
        },
        "new_str": {
          "type": "string",
          "description": "Replacement text for str_replace, or fallback insert text if insert_text is absent."
        },
        "insert_line": {
          "type": "number",
          "description": "Required for insert. 0-based line number. 0 inserts before the first line."
        },
        "insert_text": {
          "type": "string",
          "description": "Required for insert unless new_str is provided."
        },
        "old_path": {
          "type": "string",
          "description": "Required for rename unless path is used as the old path."
        },
        "new_path": {
          "type": "string",
          "description": "Required for rename. Must stay within the same memory scope."
        }
      },
      "required": ["command"]
    }
  }
}
```

可选第二阶段工具：

- [ ] `memory_record_fact`：结构化追加 repo/project/user fact，工具端生成 id、校验 citation、做简单去重，比让模型手写 JSONL 更稳。

```json
{
  "type": "function",
  "function": {
    "name": "memory_record_fact",
    "description": "Record a verified, durable fact into scoped memory with citations and reason.",
    "parameters": {
      "type": "object",
      "properties": {
        "scope": { "type": "string", "enum": ["user", "repo", "project"] },
        "subject": { "type": "string" },
        "fact": { "type": "string" },
        "citations": { "type": "array", "items": { "type": "string" } },
        "reason": { "type": "string" },
        "category": { "type": "string" },
        "confidence": { "type": "string", "enum": ["low", "medium", "high"] }
      },
      "required": ["scope", "subject", "fact", "reason"]
    }
  }
}
```

### Prompt 需要调整

- [ ] system prompt 中新增明确的 `MemoryInstructions` 段落：什么时候写 user memory、session memory、project memory、repo facts；什么时候禁止写；如何处理过时或错误记忆。
- [ ] 每轮 user-side runtime context 中新增 `MemoryContext` 段落，而不是只把 `STELLACLAW.md` 作为 system snapshot。建议自动注入：
  - user memory 的有限预算摘要；
  - session `plan.md` 和 `summary.md` 的有限预算内容；
  - project `STELLACLAW.md` snapshot；
  - repo facts 的最近/相关条目，先简单按 repo_key + 最近 N 条，后续再做检索。
- [ ] prompt 明确要求：多轮任务必须维护 `/memory/session/plan.md` 或 `/memory/session/summary.md`，不要把临时计划写入 `STELLACLAW.md`。
- [ ] prompt 明确要求：写 project/repo memory 前必须是稳定事实，尽量带 citation/reason；不确定、未验证、只对当前 turn 有用的信息写 session memory。

### 对压缩 Workflow 的影响

- [ ] 压缩前先 flush 当前完整 transcript 到 `conversations/<id>/.stellaclaw/memory/transcript/full.jsonl`，并记录 line count / hash。压缩 summary 里只放回查路径，不把完整 transcript 继续带入 prompt。
- [ ] 压缩产物不要只写回 `current_messages`。还要更新 `/memory/session/summary.md`，保留当前目标、已完成、关键文件、失败路径、下一步和用户约束。
- [ ] 压缩后 `current_messages` 应包含一个短的 `conversation-summary` user message，引用 `/memory/session/summary.md` 和 transcript 路径；后续 turn 由 `MemoryContext` 自动带入 session summary。
- [ ] 如果本轮 memory 工具修改了 `/memory/project/STELLACLAW.md`，SessionActor 必须立即更新 runtime metadata snapshot，或在下一轮 user message 前插入 synthetic memory update notice；不能继续只等 compaction promote。
- [ ] 如果本轮 memory 工具修改了 `/memory/session/*`，下一次 prompt 构造必须重新读取 session memory；session memory 不应该依赖 compaction 才生效。
- [ ] compression prompt 应明确区分“conversation summary”和“memory update suggestions”。模型可以建议写入 memory，但实际写入必须通过 `memory` 工具完成，避免 summarizer 顺手把临时内容污染 project memory。
- [ ] `RuntimeMetadataState` 需要新增 memory component hashes：`user_memory`、`session_memory_manifest`、`project_memory`、`repo_memory_manifest`。其中 session memory 变化生成 user-side context；project memory 变化可以更新 system snapshot 或生成 runtime notice；repo/user memory 按预算注入。
- [ ] crash 恢复时，`session.json` 仍保存 `runtime_metadata_state`，但 memory 文件本身是权威存储。恢复后应重新 hash memory manifest，避免 session.json 里的旧 snapshot 覆盖磁盘上更新后的 memory。
- [ ] 压缩触发阈值可以更激进：有 session summary 和 transcript lookup 后，旧 tool rounds 不必长期保留；保留最近若干 closed rounds + session summary 即可。
