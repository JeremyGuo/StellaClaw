# NEW_MEMORY_SYSTEM

本文档用于记录新的 memory system 设计、当前已确认决策、数据结构、触发规则、检索路径，以及后续实现 TODO。  
后续相关改动统一以本文档维护进度，不再把 memory system 的实现细节散落到其他临时说明里。

## 目标

- 用 `conversation` 替代 `/new` 作为主要上下文边界。
- `/snapsave` / `/snapload` 需要完整保存并恢复 conversation memory，不只保存 workspace 和短上下文。
- 共享状态只保留：
  - `skills`
  - `skill_memory`
  - `USER.md`
  - `IDENTITY.md`
- `conversation` 内的 short memory 使用类似 Codex 的思路：
  - 自动 compaction
  - recent high-fidelity 保留区
  - 分层 retrieval
- 每次 compaction 时，同步整理三层 memory：
  - `memory_summary`
  - `MEMORY`
  - `rollout_summary`
- 原始细节不靠 summary 保存，而靠 `rollout_transcript.jsonl` 做最底层证据。

## 已确认设计

### 1. 边界

- 不再依赖 `/new` 作为用户主流程。
- `/new` 退出主流程设计；后续可以删除，或仅保留为管理/兼容命令，而不是主要上下文切换方式。
- 一个 Telegram `conversation` 对应一条长期存在的 conversation memory 链。
- 一次 `rollout` 的定义：
  - 从上一次 compaction 结束后开始
  - 到下一次 compaction 触发前结束
  - 即“两次 compaction 之间的完整对话与工具历史”

### 2. 三层 memory + 一层 archive

- `memory_summary`
  - 最上层路由层
  - 每轮自动注入
  - 负责告诉 agent 最近有哪些主题、先去哪里查
- `MEMORY`
  - 中层索引层
  - 不自动全量注入
  - 按需用工具读取
  - 负责按主题归组多个 `rollout_summary`
- `rollout_summary`
  - 单个 rollout 的中等细节摘要
  - 不自动注入
  - 按需读取
- `rollout_transcript.jsonl`
  - 单个 rollout 的原始完整事件流
  - 只做 archive 和定点取证
  - 永远不允许默认全量打开

这三层的 canonical 存储优先使用 `json`，不再以 `markdown` 作为唯一真源。  
如果后续需要给人类调试或浏览，可以再派生渲染为 `.md` 视图，但不是主存储格式。

### 3. 上下文组装路径

每轮模型请求的 memory 路径固定为：

```text
old_summary + recent context + rendered memory_summary
-> 如果需要历史，search MEMORY
-> 打开相关 rollout_summary
-> 还不够，再 search rollout_transcript
-> 只 read 命中的小片段
```

### 4. 共享状态更新

共享状态更新不合并进 `[User Message]`，统一使用 synthetic `system message`。

- `USER.md` 更新：
  - `[System Message: USER Updated, read the file if you need to know the changes]`
- `IDENTITY.md` 更新：
  - `[System Message: IDENTITY Updated, read the file if you need to know the changes]`
- `skill` 更新：
  - `Skill <name> updated to version <version>.`
  - 附上最新 `description`

### 4.1 system prompt 重组

- 每次 compaction 完成后，system prompt 必须自动重新组装。
- 重组后的 system prompt 应反映：
  - 最新共享状态版本
  - 最新 `old_summary`
  - 最新 `memory_summary` 路由信息
- 共享状态更新不写死进旧 short memory，而是在下一轮组装时重新注入。

### 4.2 compaction 调用方式

- compaction 调用 LLM 时，已改为基于当前原始 `messages` 直接在末尾追加一条 synthetic `user` compaction request。
- 不再另起一个只有 `system + user` 的独立压缩上下文。
- 这样做的目标是：
  - 更贴近原 conversation 上下文
  - 更有机会复用前缀缓存
- compaction request 会明确要求：
  - 不要把 system prompt、skills、`USER.md`、`IDENTITY.md` 的共享内容重新写进 summary
  - recent high-fidelity 区间不要被重新总结

### 5. compaction 触发

最终保留两种正常触发方式：

- `threshold compaction`
  - 当估算 token 达到模型上下文窗口的一定比例时触发
- `idle compaction`
  - 当 conversation 空闲超过阈值时触发
  - 但必须同时满足最小上下文长度阈值，避免只聊一两句就压缩

### 6. token 参数方向

当前只保留比例参数，不写死常量阈值。

- `compact_trigger_ratio`
  - 用于计算 `threshold compaction` 触发点
- `idle_compact_min_ratio`
  - 用于限制 `idle compaction` 最小长度
- `recent_fidelity_target_ratio`
  - 用于决定 recent high-fidelity 保留区目标大小

推荐初始方向：

- `compact_trigger_ratio = 0.9`
- `idle_compact_min_ratio = 0.5`
- `recent_fidelity_target_ratio = 0.18`

最终是否调整，后续以真实 telemetry 为准。

### 6.1 recent high-fidelity 保留规则

- recent high-fidelity 保留区已改为按 token 预算决定，而不是按固定消息条数。
- 当前实现会：
  - 根据 `recent_fidelity_target_ratio` 计算 tail token budget
  - 从消息尾部向前回溯，尽量保留最近消息
  - 如果命中 `tool_result`，会自动把对应的 `tool_call` 一起纳入，避免从工具事务中间截断

### 6.2 特殊运行态工具保证边界

- 对 `exec`、`download_file`、`subagent` 这三类跨 turn 持续存在的运行态：
  - 压缩不会销毁底层运行态文件或任务本身
  - 压缩后的 messages 会直接额外拼上一条当前 active runtime state 摘要消息
  - 其中会包含关键标识，例如：
    - `exec_id`
    - `download_id`
    - `subagent` id
    - 关键 path / cwd / url
- 因此当前实现属于：
  - continuation-safe
  - 但不是“把完整 stdout/stderr 或完整历史都无损复制进 summary”
- 如果后续需要更强的无损保证，应继续把 active runtime tasks 提升为结构化 compaction 输出字段，而不只是一段附加文本。

## 目录结构

建议 conversation 级目录结构如下：

```text
conversations/<conversation_id>/
  memory_summary.json
  MEMORY.json
  rollouts/
    <rollout_id>/
      rollout_summary.json
      rollout_transcript.jsonl
```

说明：

- `memory_summary.json`
  - conversation 级最上层导航的 canonical 结构化存储
- `MEMORY.json`
  - conversation 级中层索引的 canonical 结构化存储
- `rollouts/<rollout_id>/rollout_summary.json`
  - 单个 rollout 的中等细节总结
- `rollouts/<rollout_id>/rollout_transcript.jsonl`
  - 单个 rollout 的完整事件流

不单独引入 `current_rollout/` 文件目录。  
open rollout 的积累过程优先复用现有 session 持久化与当前运行态，直到 compaction 成功后再一次性封存出 `rollout_transcript.jsonl`。

## active context 设计

你们这里不强制新增单独的 `active_context.json` 文件。  
更合理的做法是直接复用并扩展当前已经存在的 session 持久化结构。

也就是：

- `session.agent_messages`
  - 当前真正送给模型的 short memory
- `pending_continue.resume_messages`
  - 失败恢复时优先使用的恢复态

需要区分两件事：

1. 存储层字段
   - `old_summary`
   - `last_rollout_id`
   - `shared_versions_seen`
2. 送给模型时的 message 组装结果
   - 这些字段本身不是直接裸放给模型
   - host 会把它们重新打包成 synthetic `system/user` message 片段，再加入真正送模的 `messages`

所以这里的 `active context` 是一个逻辑层，不要求单独文件落盘。

这里还要区分“正常 short memory 来源”和“异常恢复来源”：

- 正常 short memory 主来源：
  - `session.agent_messages`
- 异常恢复专用来源：
  - `pending_continue.resume_messages`

`pending_continue.resume_messages` 不是正常 memory 层的一部分。  
它只在上一轮失败后、需要恢复时覆盖正常输入基线。

建议字段：

```json
{
  "conversation_id": "string",
  "old_summary": "string",
  "recent_messages": [],
  "last_rollout_id": "string | null",
  "shared_versions_seen": {
    "user_version": 0,
    "identity_version": 0,
    "skills_registry_version": 0
  }
}
```

说明：

- `old_summary`
  - 当前 conversation 的远端压缩记忆
- `recent_messages`
  - recent high-fidelity 保留区
- `last_rollout_id`
  - 最近一次封存的 rollout
- `shared_versions_seen`
  - 当前 conversation 已见过的共享状态版本

示例：

```json
{
  "conversation_id": "telegram:1717801091",
  "old_summary": "## Goals\n- 继续设计新的 memory system。\n\n## Decisions\n- 每次 compaction 关闭一个 rollout。\n- 采用三层 memory：memory_summary / MEMORY / rollout_summary。\n\n## Important Facts\n- rollout_id 对应两次 compaction 之间的一整段对话。\n\n## Next Step\n- 明确 compaction 输出如何映射到各层文件。",
  "recent_messages": [
    {
      "role": "user",
      "content": "rollout_search和rollout_read参数是什么？"
    },
    {
      "role": "assistant",
      "content": "我建议直接定得很实用，不搞抽象。"
    }
  ],
  "last_rollout_id": "2026-04-06T10-20-11-abcd",
  "shared_versions_seen": {
    "user_version": 3,
    "identity_version": 1,
    "skills_registry_version": 7
  }
}
```

## `rollout_id` 设计

- `rollout_id` 不是单条 `user` / `assistant` / `tool` event 的 id。
- `rollout_id` 对应的是：
  - 一整个 rollout
  - 即两次 compaction 之间的完整对话片段
- `event_id` 才对应单条事件。

对应关系：

- 一个 `rollout_id`
  - 对应多个 `event_id`
- 一个 `rollout_summary.md`
  - 对应一个 `rollout_id`
- 一个 `MEMORY.md` block
  - 可对应多个 `rollout_id`

`rollout_id` 由 host/runtime 生成，不由 LLM 生成。

## compaction 输出格式

每次 compaction 不再只返回纯文本，而是返回结构化 JSON。

建议 schema：

```json
{
  "old_summary": "string",
  "new_summary": "string",
  "keywords": ["string"],
  "important_refs": {
    "paths": ["string"],
    "commands": ["string"],
    "errors": ["string"],
    "urls": ["string"],
    "ids": ["string"]
  },
  "memory_hints": [
    {
      "group": "string",
      "conclusions": ["string"]
    }
  ],
  "next_step": "string"
}
```

说明：

- `old_summary`
  - 对更早历史的进一步压缩
- `new_summary`
  - 对当前 rollout 的压缩总结
  - 直接作为该 rollout 的 `rollout_summary.md` 主体内容
- `keywords`
  - 用于更新 `MEMORY.md` 与搜索入口
- `important_refs`
  - 用于保存重要路径、命令、错误、URL、ID
- `memory_hints`
  - 用于辅助把本次 rollout 归入某个 memory group
- `next_step`
  - 用于继续工作和恢复

建议这份 JSON 只作为 host 的中间产物，不直接暴露给用户。

这里有一个关键边界：

- `old_summary`
  - 是新的“远端压缩结果”
  - 后续不会以裸字段形式给模型，而是会被 host 打包回一段 summary message
- `new_summary`
  - 是本次 rollout 的 summary 正文
  - 会进入 `rollout_summary.json`

## compaction 输入来源

不同触发方式，compaction 的输入来源不同：

### 1. `idle compaction`

来源：

- 优先 `pending_continue.resume_messages`
- 否则 `session.agent_messages`

说明：

- 这是 turn 外部、基于持久化 session 状态做的 compaction
- 如果存在失败恢复态，优先使用恢复态，因为它更接近下次真正会继续的上下文

### 2. `threshold compaction`

来源：

- 当前 turn 内部的 working `messages`

说明：

- 这是对话执行中因为上下文超长触发的 compaction
- 压的是当前正在运行的那份 message 数组

### 3. `tool-wait compaction`

来源：

- 当前 turn 内部的 working `messages`

说明：

- 这是因为 tool 等待时间过长、接近 timeout observation 边界时触发的 compaction
- 它不是基于 session 持久化层做的，而是基于当前 turn 的 working context

### 统一切分约束

不管 compaction 来源是哪一种，都必须满足：

- recent high-fidelity 保留区按 token budget 选取
- 不能从 `tool_call` 和对应 `tool_result` 中间截断
- 不能把最近的关键 user steering 随意压掉

示例：

```json
{
  "old_summary": "## Goals\n- 设计新的 conversation-level memory system。\n\n## Decisions\n- 不再使用 topic 机制。\n- 使用三层 memory + rollout archive。\n\n## Important Facts\n- agent 不需要预先知道 rollout_id。\n\n## Next Step\n- 定义 compaction 输出与文件更新规则。",
  "new_summary": "## Summary\n- 本轮明确了 rollout_id 对应两次 compaction 之间的整段对话，而不是单条消息。\n- 确认 retrieval 路径为 memory_summary -> MEMORY -> rollout_summary -> rollout_transcript snippet。\n- 确认 transcript 不能全量打开，只能 search + snippet read。\n\n## Important Refs\n- Paths: NEW_MEMORY_SYSTEM.md\n- Terms: rollout_id, event_id, rollout_search, rollout_read\n\n## Next Step\n- 设计 compaction 输出如何映射到 active context、MEMORY 和 rollout_summary。",
  "keywords": [
    "rollout_id",
    "rollout_summary",
    "rollout_transcript",
    "memory_search",
    "snippet read"
  ],
  "important_refs": {
    "paths": [
      "NEW_MEMORY_SYSTEM.md"
    ],
    "commands": [],
    "errors": [],
    "urls": [],
    "ids": [
      "rollout_id",
      "event_id"
    ]
  },
  "memory_hints": [
    {
      "group": "Conversation Retrieval",
      "conclusions": [
        "Agent 先查 summary 层，再查 rollout 层。",
        "rollout_transcript 只能做定点取证，不能全量打开。"
      ]
    }
  ],
  "next_step": "实现 rollout_search 和 rollout_read 的索引与局部读取。"
}
```

## compaction 输出如何组装并更新文件

每次 compaction 成功后，host 必须明确把 JSON 结果拆分写入不同层，而不是把同一段文本复制到所有文件。

### 1. 更新 active context

来源：

- `old_summary`
- 当前 recent high-fidelity 保留区
- 当前 `shared_versions_seen`
- 新生成的 `rollout_id`

写法：

- 更新 `session.agent_messages`
  - 它仍然是当前真正送给模型的 short memory
- host 将以下状态重新打包成新的前缀 messages：
  - `old_summary`
  - 共享状态更新 system messages
  - 最近高保真区原文
- 更新逻辑字段：
  - `old_summary = compaction_result.old_summary`
  - `last_rollout_id = 当前关闭的 rollout_id`
  - `shared_versions_seen = 当前共享状态版本`
- `pending_continue`
  - 继续作为失败恢复入口
  - 不另造并行机制

也就是说：

- `old_summary` 进入 short memory 的远端压缩层
- `new_summary` 不进入 active context 主体

### 2. 写入 `rollout_summary.json`

来源：

- `new_summary`
- `keywords`
- `important_refs`
- `next_step`
- host 自己生成的 metadata

写法：

- `new_summary` 直接作为 `rollout_summary.json.summary`
- `rollout_id`、`conversation_id`、`created_at`、`source_event_range` 由 host 补进 metadata

建议结构：

```json
{
  "rollout_id": "string",
  "conversation_id": "string",
  "created_at": "string",
  "source_event_range": {
    "start_event_id": 0,
    "end_event_id": 0
  },
  "summary": "string",
  "keywords": ["string"],
  "important_refs": {
    "paths": [],
    "commands": [],
    "errors": [],
    "urls": [],
    "ids": []
  },
  "next_step": "string"
}
```

### 3. 封存 `rollout_transcript.jsonl`

来源：

- 当前 open rollout 对应的完整对话/工具事件

写法：

- 不依赖 `current_rollout/` 文件目录
- compaction 成功后，由 host 基于“上次 compaction 之后到现在”的完整对话/工具事件，直接生成：
  - `rollouts/<rollout_id>/rollout_transcript.jsonl`
- 这个文件是一次性封存产物，不要求预先单独维护 open-rollout 文件

### 4. 增量更新 `MEMORY.json`

来源：

- `memory_hints`
- `keywords`
- `important_refs`
- 新写入的 `rollout_summary.json` 路径

写法：

- 根据 `memory_hints[].group` 找到或创建对应 memory group
- 把本次 rollout 挂到该 group 的 rollout 列表里
- 将 `memory_hints[].conclusions` 合并进该 group 的稳定结论字段
- 将 `keywords` 合并进该 group 的关键词字段
- `important_refs` 只在它们有稳定复用意义时进入 `MEMORY.json`
- 过细的 refs 只保留在 `rollout_summary.json`

建议结构：

```json
{
  "groups": [
    {
      "group": "Conversation Retrieval",
      "scope": "conversation 级历史检索与 transcript 定点取证",
      "conclusions": [
        "Agent 先查 summary 层，再查 rollout 层。"
      ],
      "keywords": [
        "rollout_id",
        "rollout_summary",
        "snippet read"
      ],
      "rollouts": [
        "rollouts/2026-04-06T10-20-11-abcd/rollout_summary.json"
      ]
    }
  ]
}
```

### 5. 增量更新 `memory_summary.json`

来源：

- `memory_hints`
- `keywords`
- `next_step`
- 最新更新过的 memory group 列表

写法：

- 更新最近活跃的 memory groups
- 更新对应检索路径
- 不复制 `new_summary` 全文
- 不复制 `rollout_summary` 全文

建议结构：

```json
{
  "recent_active_areas": [
    "Conversation Retrieval",
    "Context Compaction"
  ],
  "quick_routes": [
    "如果需要历史检索，先查 MEMORY 中的 Conversation Retrieval",
    "如果需要精确证据，先打开 rollout_summary，再做 transcript snippet read"
  ]
}
```

### 6. 重组 system prompt

来源：

- 最新共享状态版本
- 最新 active context
- 最新 `memory_summary`

写法：

- compaction 成功后，不沿用旧的 prompt 前缀
- 在下一轮正式处理用户消息前，host 必须基于最新状态重新组装 system prompt
- 这样共享状态更新和 summary 更新才能立即生效

## compaction 消息来源与切分规则
当前代码里，`tool-wait compaction` 的调度会参考稳定前缀，但真正压缩的仍然是当前 turn 内部的 working `messages`。  
因此最终文档统一以 “idle 使用持久化上下文，threshold/tool-wait 使用内部 working messages” 作为准则。

## compaction 进行中的即时用户反馈

如果用户发来一句话时，当前 foreground 正在 compaction：

- host 必须立即回复一条短消息：
  - `正在压缩上下文，可能要等待压缩完毕后才能回复。`
- 这条反馈不能等 agent 自己处理
- 因为 compaction 调用过程中不能保证立刻中断并插入新消息

## compaction 失败处理

按最终设计，分三类：

### 1. idle compaction 失败

- 不立即打断用户当前流程
- 下次用户再次说话时自动重试

### 2. tool-wait compaction 失败

- 发起中断
- 立即告诉用户：
  - `自动上下文压缩失败，输入 /continue 继续。`
- 因为此时正在接近 tool-timeout 边界，不能继续无声拖延

### 3. threshold compaction 失败

- 处理方式与 tool-wait compaction 失败一致
- 发起中断
- 立即告诉用户：
  - `自动上下文压缩失败，输入 /continue 继续。`

## 文件格式建议

如果文件不是“必须原样喂给 Agent”，优先使用结构化格式保存。  
建议：

- 自动注入层：
  - active context（由 `session.agent_messages` 等现有持久化结构承载）
- 中间运行态和索引：
  - `json` / `jsonl`
- 给 agent 阅读的中层和总结层：
  - 优先 `json`
  - 由工具返回筛选后的结构化片段给 agent

原因：

- `json/jsonl` 适合 host 做确定性更新和增量维护
- 你们这里 retrieval 主要通过专门工具而不是裸文件直接塞 prompt，所以 canonical JSON 更稳
- 如果后续需要人类调试视图，可以再派生渲染 markdown，不作为真源

最终建议：

- `rollout_transcript.jsonl`
  - `jsonl`
- `rollout_summary.json`
  - `json`
- `MEMORY.json`
  - `json`
- `memory_summary.json`
  - `json`

## retrieval 工具设计

### `memory_search`

作用：

- 搜索 `MEMORY.md`
- 定位相关 `rollout_summary`
- 先返回中层线索，不碰 transcript archive

### `rollout_search`

参数：

```json
{
  "query": "string",
  "rollout_id": "string | null",
  "kinds": ["user_message", "assistant_message", "tool_call", "tool_result", "system_message", "compaction"],
  "limit": 10
}
```

返回：

```json
{
  "matches": [
    {
      "rollout_id": "string",
      "event_id": 0,
      "timestamp": "string",
      "kind": "string",
      "preview": "string",
      "paths": ["string"],
      "commands": ["string"],
      "errors": ["string"],
      "score_hint": "string"
    }
  ],
  "truncated": false
}
```

说明：

- `kinds` 用来限制搜索哪类 event
- 如果不知道 `rollout_id`，允许全局搜索 conversation 索引
- 不返回全文，只返回命中点和 preview

### `rollout_read`

参数：

```json
{
  "rollout_id": "string",
  "anchor_event_id": 0,
  "mode": "window | turn_segment",
  "before": 3,
  "after": 3
}
```

返回：

```json
{
  "rollout_id": "string",
  "anchor_event_id": 0,
  "mode": "string",
  "events": [],
  "has_more_before": true,
  "has_more_after": false
}
```

说明：

- 默认推荐 `mode = "turn_segment"`
- 只读局部窗口
- 绝不默认全量打开 `rollout_transcript.jsonl`

## 三层 memory 的职责边界

### `memory_summary.md`

只做：

- 最近主题导航
- 快速路由
- 告诉 agent 先去哪个 `MEMORY.md` block

不做：

- 具体证据
- 长篇结论
- 原始对话复盘

示例：

```json
{
  "recent_active_areas": [
    "Conversation Retrieval",
    "Context Compaction",
    "Shared State Update Injection"
  ],
  "quick_routes": [
    "如果问题与历史检索有关，先看 MEMORY 中的 Conversation Retrieval",
    "如果需要具体压缩规则，先看 MEMORY 中的 Context Compaction",
    "如果需要精确历史证据，先看相关 rollout_summary，再做 transcript snippet read"
  ]
}
```

### `MEMORY.md`

只做：

- 中层索引
- 稳定结论
- 按主题归组多个 rollout

不做：

- 全量时间线
- 完整命令输出
- 长篇原始错误
- 逐条对话记录

示例：

```json
{
  "groups": [
    {
      "group": "Conversation Retrieval",
      "scope": "conversation 级历史检索、summary 路由、transcript 定点取证",
      "conclusions": [
        "Agent 应先查 memory_summary，再查 MEMORY。",
        "如果需要中等细节，先打开相关 rollout_summary。",
        "如果需要原始证据，只允许 search + snippet read rollout_transcript.jsonl。"
      ],
      "rollouts": [
        "rollouts/2026-04-06T10-20-11-abcd/rollout_summary.json"
      ],
      "keywords": [
        "rollout_id",
        "rollout_summary",
        "rollout_transcript",
        "snippet read",
        "memory_search"
      ]
    }
  ]
}
```

### `rollout_summary.md`

只做：

- 单次 rollout 的中等细节总结
- 供后续 agent 回看该段历史

示例：

```json
{
  "rollout_id": "2026-04-06T10-20-11-abcd",
  "conversation_id": "telegram:1717801091",
  "created_at": "2026-04-06T10:20:11+08:00",
  "source_event_range": {
    "start_event_id": 120,
    "end_event_id": 188
  },
  "summary": "本轮明确了 rollout_id 对应两次 compaction 之间的整段对话；确认 transcript 只能 search + snippet read，不能全量打开。",
  "keywords": [
    "rollout_id",
    "rollout_search",
    "rollout_read"
  ],
  "important_refs": {
    "paths": ["NEW_MEMORY_SYSTEM.md"],
    "commands": [],
    "errors": [],
    "urls": [],
    "ids": ["rollout_id", "event_id"]
  },
  "next_step": "设计 retrieval 工具的返回结构。"
}
```

### `rollout_transcript.jsonl`

只做：

- 原始证据 archive
- 最后兜底

示例：

```json
{"event_id":120,"timestamp":"2026-04-06T09:58:10+08:00","kind":"user_message","role":"user","text":"rollout_id是一次 user/assistant/tool 还是两次压缩之间的所有对话的那个文档的ID"}
{"event_id":121,"timestamp":"2026-04-06T09:58:26+08:00","kind":"assistant_message","role":"assistant","text":"是后者。rollout_id 应该对应两次压缩之间的整段对话档案。"}
{"event_id":122,"timestamp":"2026-04-06T10:01:11+08:00","kind":"user_message","role":"user","text":"那么search工具怎么去读取内容，才能不全部载入"}
{"event_id":123,"timestamp":"2026-04-06T10:01:30+08:00","kind":"assistant_message","role":"assistant","text":"靠两步，不靠全量打开：search + read snippet。"}
```

## 每次 compaction 后的处理步骤

1. 关闭当前 open rollout
2. 调用 LLM 生成结构化 compaction 结果
3. 生成新的 `rollout_id`
4. 写入：
   - `rollouts/<rollout_id>/rollout_transcript.jsonl`
   - `rollouts/<rollout_id>/rollout_summary.json`
5. 增量更新 `MEMORY.json`
6. 增量更新 `memory_summary.json`
7. 重建 active context（直接更新现有 session 持久化结构）

## 当前已落地

- 已将 `compact_trigger_ratio` 默认值统一为 `0.9`
- 已为 `idle compaction` 增加最小上下文长度限制：
  - `idle` 只有在空闲时间满足且 `estimated_tokens >= context_window * idle_compact_min_ratio` 时才会触发
- 已在 host 侧接入前台运行 phase 跟踪：
  - 当前 turn 进入 `compaction` 时会标记运行态
  - 如果用户在 compaction 过程中发送新消息，host 会立即回复：
    - `正在压缩上下文，可能要等待压缩完毕后才能回复。`
  - 同时仍会对前台会话发起 `yield request`
- `agent_frame::compaction` 已升级为结构化输出：
  - `ContextCompactionReport` 现在会携带：
    - `structured_output`
    - `compacted_messages`
  - `structured_output` 当前包含：
    - `old_summary`
    - `new_summary`
    - `keywords`
    - `important_refs`
    - `memory_hints`
    - `next_step`
- 结构化 compaction 结果已接入 host 事件流：
  - `SessionEvent::CompactionCompleted`
  - `SessionEvent::ToolWaitCompactionCompleted`
  - 这为后续按 compaction 事件实时封存 rollout 打通了通道
- `idle compaction` 和手动 `/compact` 已开始落第一版 artifact：
- 前台运行中的自动 compaction 也已接入 runtime-event 封存：
  - `SessionEvent::CompactionCompleted`
  - `SessionEvent::ToolWaitCompactionCompleted`
  - 现在 threshold / tool-wait compaction 也会自动生成 rollout artifact
- 当前第一版 artifact 落盘路径为：
  - `conversation_memory/rollouts/<rollout_id>/rollout_summary.json`
  - `conversation_memory/rollouts/<rollout_id>/rollout_transcript.jsonl`
  - `conversation_memory/MEMORY.json`
  - `conversation_memory/memory_summary.json`
- 当前 rollout artifact 落盘仍然属于第一阶段：
  - schema 与目录已经落地
  - 但还没有把 retrieval 工具、shared version 路由、以及完整的 conversation-level archive search 串完

## TODO List

- [x] 重构 compaction 机制：改为结构化输出，并引入 `old_summary` / `new_summary`
- [x] 落地 rollout 文件结构：实现 `rollout_summary.json`、`rollout_transcript.jsonl`
- [x] 落地三层 memory：实现 `memory_summary.json`、`MEMORY.json` 的增量更新（第一版）
- [x] 落地 retrieval：实现 `memory_search`、`rollout_search`、`rollout_read`
- [x] 重构共享状态注入：改为 synthetic `system message` + version tracking
- [x] 调整 conversation 生命周期：移除 `/new` 的主流程地位，并在 compaction 后自动重组 system prompt
- [x] 补齐中断、idle compaction、失败恢复与 telemetry

## 当前进度

- 已接通结构化 compaction 输出：
  - `old_summary`
  - `new_summary`
  - `keywords`
  - `important_refs`
  - `memory_hints`
  - `next_step`
- 已在 compaction 成功后封存第一版 conversation memory artifacts：
  - `rollout_summary.json`
  - `rollout_transcript.jsonl`
  - `MEMORY.json`
  - `memory_summary.json`
- 已接通第一版 retrieval 工具：
  - `memory_search`
  - `rollout_search`
  - `rollout_read`
- 已开始重构共享状态注入通道：
  - runtime skill update 不再拼进 user message
  - 改为独立的 synthetic `system message`
- 已接通 `USER.md` / `IDENTITY.md` 的第一版 version tracking：
  - 首次观察只记录版本，不提示
  - 后续内容变化时注入：
    - `[System Message: USER Updated, read the file if you need to know the changes]`
    - `[System Message: IDENTITY Updated, read the file if you need to know the changes]`
- system prompt 现在每轮优先从磁盘重读：
  - `USER.md`
  - `IDENTITY.md`
  - `AGENTS.md`
- 已补第一版 compaction 失败恢复分流：
  - `threshold compaction` / `tool-wait compaction` 失败时，错误链路会带上明确来源
  - host 会优先给出 `/continue` 恢复文案，而不是普通失败文案
- 已补 idle compaction 失败重试状态：
  - idle compaction 失败时会在 session 内记录 retry pending
  - 下次用户发话前会自动尝试一次 idle retry
- `/new` 已从默认 bot command 列表移除
- `/new` 目前会返回兼容提示，不再执行真正的 session reset
- 已补 idle compaction retry 的状态可见性：
  - `/status` 里会显示 retry pending 与最近错误摘要

## 当前优先顺序

1. 完成 compaction 输出结构化改造
2. 落地 rollout 文件结构
3. 落地 `memory_summary.md` / `MEMORY.md` 增量更新
4. 落地 `rollout_search` / `rollout_read`
5. 替换共享状态更新注入方式

## 维护规则

- 后续每次 memory system 相关改动后，优先更新本文档的：
  - `已确认设计`
  - `TODO List`
  - `当前优先顺序`
- 如果某个设计被推翻，不在原处硬改成混乱状态，直接在对应章节明确更新结论。
