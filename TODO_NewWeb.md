# TODO New Web

## 流式消息设计

### 核心边界

- WebSocket 单连接内有序可靠；断线后不做补包，客户端按重新打开 foreground session 处理。
- REST API 负责加载 durable history。
- WebSocket handshake 负责返回当前 live projection。
- 后续 realtime 事件只做增量更新。
- 所有可持久化 timeline 变化只通过 `message_appended`。
- 所有 stream 事件只修改当前 live projection。
- 用户消息在 durable `message_appended(role=user)` 前不能占据确定 timeline 位置，只能作为 local pending / queued outbound 显示。
- 工具执行失败不是 `stream_error`，而是正常 `ToolResult(error)`。
- `stream_error` 只撤销当前 live provisional assistant message，不撤销 durable history。

### Live State Key

WebChannel 侧 live projection 按 foreground session 维护：

```text
(conversation_id, foreground_session_id)
```

当前 turn / provisional message 使用：

```text
turn_id + message_id
```

### WebSocket Handshake Snapshot

客户端连接 foreground session websocket 时，server 返回：

```text
last_committed_message_id / message_index
current_turn_state?
current_provisional_assistant_message?
running_tool_results?
queued_outbound_messages?
```

`last_committed_message_id / message_index` 用作 durable history 的恢复锚点：

- 客户端可用它向前/向后分页获取旧消息。
- 首次打开 foreground session 时，用 REST API 基于该锚点加载历史消息并写入本地显示列表。
- 重连时，用该锚点确认本地 durable history 是否缺消息；缺失部分通过 REST API 补齐。
- 它不表示 live projection 的内容，只表示 server 当前确认的最后一条 durable message。

客户端重连时：

- 丢弃本地 live projection。
- REST 重新拉 durable history。
- 用 handshake snapshot 恢复当前正在运行的 live turn。

### WebSocket 拆分与命名

第一版明确拆成三个 websocket，不做一个大杂烩 event bus：

```text
GET /api/ws/home
GET /api/conversations/{conversation_id}/foreground_sessions/{foreground_session_id}/ws
GET /api/conversations/{conversation_id}/terminals/{terminal_id}/ws
```

所有 websocket 都必须支持统一 heartbeat：

```text
*.heartbeat {
  server_time
}
```

- server 定期发送 heartbeat。
- heartbeat 周期为 30s。
- heartbeat 不是业务事件，不参与 `seq` / timeline / stream state。
- client 如果超过约定时间没有收到任何 frame，包括 heartbeat 或业务事件，就主动断开并重连；建议超时阈值至少大于 2 个 heartbeat 周期。
- 任意业务事件也可视为连接活跃信号；heartbeat 只用于空闲连接保活和半开连接检测。

#### `GET /api/ws/home`

首页 / conversation catalog websocket。

负责：

- conversation 列表 snapshot。
- conversation create / rename / delete 的事件。
- foreground session list / rename / delete 的事件。
- foreground session summary state，例如 idle / queued / running / failed。
- conversation metadata 动态更新，例如 conversation name。
- foreground session metadata 动态更新，例如 session nickname。
- seen state / unread summary。
- conversation / foreground session metadata update request/response。
- conversation settings / runtime config snapshot and update request/response。

不负责：

- 聊天 durable message history。
- assistant / tool / reasoning stream。
- terminal output。
- workspace 文件内容。

连接后第一帧必须是：

```text
home.snapshot
```

后续事件使用 `home.` 前缀：

```text
home.conversation_upserted
home.conversation_updated
home.conversation_deleted
home.foreground_session_upserted
home.foreground_session_updated
home.foreground_session_deleted
home.foreground_session_state_updated
home.foreground_session_seen_state_updated
home.heartbeat
home.error
```

conversation / foreground session mutation 走 REST。mutation 成功后的最终 UI 状态仍以 home websocket 广播事件为准：

```text
home.conversation_updated
home.foreground_session_updated
```

conversation / foreground session query 默认由 `home.snapshot` 覆盖；第一版不单独设计 query request。

#### `GET /api/conversations/{conversation_id}/foreground_sessions/{foreground_session_id}/ws`

Foreground session 聊天 websocket。

负责：

- 当前 foreground session 的 live turn projection。
- durable `message_appended` 增量。
- user message queued ack。
- assistant / reasoning / tool call stream。
- single tool result stream。
- turn lifecycle。
- plan live state。

不负责：

- 首页 conversation list 变更。
- workspace response。
- terminal output。
- conversation / foreground session metadata request response。

连接后第一帧必须是：

```text
chat.snapshot
```

后续事件使用 `chat.` 前缀：

```text
chat.user_message_queued
chat.message_appended
chat.stream_turn_start
chat.stream_assistant_message_delta
chat.stream_tool_call_delta
chat.stream_reasoning_summary_part_added
chat.stream_reasoning_summary_delta
chat.stream_tool_result_done
chat.stream_error
chat.stream_turn_done
chat.plan_updated
chat.heartbeat
```

#### `GET /api/conversations/{conversation_id}/terminals/{terminal_id}/ws`

Terminal websocket。

负责：

- terminal attach/replay snapshot。
- terminal output。
- terminal input ack / resize ack。
- terminal closed/error。

不负责聊天消息和 conversation catalog。

连接后第一帧必须是：

```text
terminal.snapshot
```

后续事件使用 `terminal.` 前缀：

```text
terminal.output
terminal.input_ack
terminal.resize_ack
terminal.closed
terminal.heartbeat
terminal.error
```

#### 不放进 websocket 的接口

以下第一版继续走 REST request/response，用于 snapshot、分页、显式读写或大对象传输；不承担 realtime 增量职责。

- workspace list/read/write/upload/download/delete/move
  - 作用：文件树、文件内容、上传下载和文件变更。
  - 原因：这是显式 request/response，可能包含大 payload 或二进制数据，不适合混入消息 websocket。
  - 后续：如果 workspace 变更需要影响 UI，可另行设计 workspace 专用事件，不进入聊天 timeline。

- model list
  - 作用：加载可选模型/provider/capability 列表。
  - 原因：低频配置数据，打开设置或初始化时读取即可。

- message history paging
  - 作用：按 `last_committed_message_id / message_index` 锚点加载 durable history。
  - 用途：首次打开 foreground session、向上滚动加载旧消息、重连后补 durable history 缺口。
  - 原因：历史消息是可分页数据，不是 realtime stream。

- message detail
  - 作用：按需读取单条 durable message 的完整详情。
  - 用途：展开长工具结果、附件详情、完整 structured payload、协议校验排查。
  - 原因：列表渲染不一定需要完整大消息，详情按需加载。

- seen mark mutation
  - 作用：标记 foreground session 已读位置。
  - 原因：这是明确 mutation；成功后由 `home.foreground_session_seen_state_updated` 广播 summary。

- conversation / foreground session create/rename/delete mutation
  - 作用：创建、重命名、删除 conversation 或 foreground session。
  - 原因：这是明确 mutation；成功后由 `home.conversation_*` / `home.foreground_session_*` 广播最终状态。

这些 REST mutation 的 response 只表示 accepted / failed；最终 UI 状态以对应 websocket typed event 为准。

### 事件

#### `user_message_queued`

非 durable。

表示 WebChannel / ChannelService 已收到用户消息并排队。

必须携带：

```text
client_message_id
conversation_id
foreground_session_id
```

前端用它把本地 pending bubble 从“发送中”改成“已排队”。真正进入 timeline 仍然等待 `message_appended(role=user)`。

#### `message_appended`

durable。

任意进入 SessionActor history 的消息都走这里：

- user message
- assistant message
- tool result
- tool error / repair result
- runtime synthetic/context message
- 非流式 provider assistant message

payload 至少包含：

```text
conversation_id
foreground_session_id
message_index
message_id
message
turn_id?
```

如果存在同一 `message_id` 的 provisional assistant message：

- 完全一致：标记 committed，不重建 UI。
- 不一致且 provisional 非空：替换为 durable message，并报协议错误。
- provisional 为空：直接 append，不报错。

#### `stream_turn_start`

live lifecycle。

表示当前 turn 开始。

在第一段 assistant / reasoning / tool call 可渲染内容出现前，前端显示“思考中”。

#### `stream_assistant_message_delta`

live。

追加当前 provisional assistant 文本。

#### `stream_tool_call_delta`

live。

追加当前 provisional assistant message 内的 tool call 参数。

#### `stream_reasoning_summary_part_added`

live。

创建 reasoning summary part。

#### `stream_reasoning_summary_delta`

live。

追加 reasoning summary 文本。

#### `stream_tool_result_done`

live。

单个工具完成时发送完整 `ToolResultItem`。

工具内部错误也通过这个事件返回 `ToolResult(error)`。

后续仍需要 durable `message_appended` 确认 tool result 进入 history。

#### `stream_error`

live error。

撤销当前 provisional assistant message。

不用于工具执行错误。

如果错误发生在还没有 `message_id` 的阶段，payload 允许 `message_id: null`，但仍要携带 `turn_id` 和错误 scope。

#### `stream_turn_done`

live lifecycle。

表示当前 turn 彻底结束，final assistant 已 durable append，后续不会再有该 turn 的 provider/tool loop。

#### `plan_updated`

live state。

用于计划面板 / round summary 状态。

`update_plan` 的 tool call / result 仍可作为 durable messages 出现在 timeline；前端不要从 tool result 解析 plan。

### 渲染规则

前端渲染分三层：

```text
durable history
current live turn projection
local pending outbound messages
```

用户敲回车后：

- 先创建 local pending outbound bubble。
- 收到 `user_message_queued` 后改成 queued。
- 收到 `message_appended(role=user)` 后，从 pending outbound 移除，按 durable `message_index` 放入正式 timeline。

当前 assistant stream：

- `stream_turn_start` 后可以先显示“思考中”。
- 收到 assistant / reasoning / tool call delta 后创建或更新 provisional assistant message。
- 收到 `message_appended` 后校验 provisional 与 durable message。
- 收到 `stream_error` 后删除当前 provisional assistant message。

工具执行：

- tool call 参数属于 assistant provisional message。
- tool result 属于 live turn projection。
- 单个工具完成先发 `stream_tool_result_done`。
- durable history 仍以后续 `message_appended(tool_result)` 为准。

Turn 结束：

- `stream_turn_done` 只表示当前 turn 完全结束。
- 不能用某个 assistant message done 代替 turn done。
- 带 tool call 的 turn 会经历多次 assistant/tool loop，最后 final assistant durable append 后才 turn done。

## 首页 Snapshot / Event 设计

### 核心原则

- 首页不打开具体 Conversation 时，也通过 websocket snapshot + sequenced events 获取状态。
- WebSocket 单连接内有序可靠；服务端要避免 snapshot/event race。
- 首页 snapshot 只包含 catalog / summary 状态，不包含完整消息历史、foreground live projection 和 layout 状态。
- 服务端只持久化跨设备 / 重启后仍应保留的业务状态。
- Conversation / foreground session 的 `order` 和 `folder_open` 由客户端本地持久化，不进服务端协议。
- 临时 UI 状态不进入服务端协议，例如滚动位置、hover、搜索框内容、临时筛选输入。
- 不设置泛泛的顶层 `ui_state`。
- 不在 server snapshot 中提供 layout state；客户端按本地 layout state 对 server snapshot 排序和折叠。

### Home Snapshot

客户端连接首页 websocket 时，server 返回：

```text
home_snapshot {
  seq
  server_time
  conversations[]
}
```

`conversations[]` 每项包含：

```text
conversation_id
conversation_name
updated_at
last_committed_message_id
last_committed_message_index
last_message_preview
foreground_sessions[]
```

`foreground_sessions[]` 每项包含：

```text
foreground_session_id
session_name
state: idle | queued | running | failed
active_turn_id?
last_committed_message_id
last_committed_message_index
last_activity_at
seen_state / unread_count
```

### Home Events

所有首页事件带全局单调 `seq`。

```text
conversation_upserted
conversation_updated
conversation_deleted
foreground_session_upserted
foreground_session_updated
foreground_session_deleted
foreground_session_state_updated
foreground_session_seen_state_updated
home_error
```

如果用户在首页执行新建、重命名、删除等服务端业务操作，可以走 REST 或 websocket request。

最终 UI 状态以 websocket event 为准；request response 只表示请求 accepted / failed。

排序和文件夹展开/收起是本地客户端 layout 操作：

- 不调用服务端。
- 不产生 home websocket event。
- 不影响其它设备。

### Snapshot Race 避免

`WebChannelMain` 处理首页订阅时必须在同一串行边界完成：

```text
1. 注册 subscriber
2. 读取当前 home projection
3. 发送 home_snapshot(seq = current_seq)
4. 后续只发送 seq > snapshot.seq 的 events
```

客户端重连时：

```text
1. 丢弃本地 home live projection
2. 重新连接 home websocket
3. 收 home_snapshot
4. 用 snapshot 重建首页
5. 继续 apply seq 更大的 events
```

第一版可以不做 delta replay；重连直接发全量 snapshot。

### 进入 Conversation

首页只维护 catalog / summary。

点击某个 foreground session 后：

```text
1. 用 last_committed_message_id / message_index 通过 REST 加载 durable history
2. 连接 foreground session websocket
3. foreground websocket handshake 返回该 session 的 live projection
```

首页的 `last_committed_message_id` 是进入 Conversation 的历史加载锚点，不是聊天 stream 的替代品。

## 后端架构更新要求

### WebChannel

- 重写 WebChannel 时引入明确的 `WebChannelMain` 串行状态中心。
- Client connection thread 只负责 socket read/write 和 request decode/encode，不直接拼接业务状态。
- `WebChannelMain` 维护 home projection：
  - conversation summaries
  - foreground session summaries
  - last committed message anchors
  - running / queued / failed summary state
  - seen summary
- `WebChannelMain` 维护 foreground session live projection：
  - current turn state
  - current provisional assistant message
  - running / completed tool result live state
  - queued outbound user messages
- `WebChannelMain` 负责 websocket handshake snapshot，避免 snapshot/event race。
- `WebChannelMain` 负责给每个 websocket 定时发送 30s heartbeat。
- ChannelService / AgentSessionService / SessionActor 输出的事件先进入 `WebChannelMain`，再由它投影成 `home.*`、`chat.*`、`terminal.*`。
- REST mutation 的 response 只表示 accepted / failed；最终 UI 状态由 `WebChannelMain` 通过 websocket typed event 广播。
- 不再让前端聊天 UI 消费泛用 `status.label`。

### ChannelService / AgentSessionService

- ChannelService 继续作为 conversation 内的串行 service 边界。
- ChannelService 需要向 WebChannelMain 输出 typed channel events，而不是把所有东西压成 `Status { label, detail }`。
- AgentSessionService 需要保留 foreground session id，并把所有 SessionActor event 投影时带上 conversation / foreground session 归属。
- AgentSessionService 需要增加用户消息 queued 边界：
  - ChannelService 接收用户消息并成功转交 foreground session 后，发 `chat.user_message_queued` 所需事件。
  - 真正 timeline 位置仍等待 SessionActor `MessageAppended(role=user)`。

### SessionActor / Tool Stream

- SessionActor 仍然是 durable history 的权威来源。
- `MessageAppended` 是唯一 durable timeline commit 事件。
- Provider stream delta 不写 history，只形成 provisional assistant message。
- Provider stream event 需要统一携带：
  - `turn_id`
  - `message_id`
  - message 内 stream index / item identity
- `stream_error` 只表示当前 provisional assistant message 无效，WebChannelMain 和 client 都删除该 provisional message。
- Tool executor 需要支持单工具完成事件：
  - 每个 tool operation 完成后立即向 SessionActor / AgentSessionService 输出 `stream_tool_result_done`。
  - payload 带完整 `ToolResultItem`，包括 `tool_call_id`、`tool_name`、structured result / error / files。
  - 工具实现内部返回/抛出的错误由 tool runtime 统一包装成普通 `ToolResult(error)`，不走 `stream_error`；这里不是要求工具调用方或模型额外修改。
- Tool result 最终仍由 SessionActor 写入 durable history，并通过 `message_appended(tool_result)` 确认。
- 如果 tool result live state 和 durable `message_appended(tool_result)` 不一致，前端以 durable message 为准，并上报协议错误。
- 带 tool call 的 assistant message 落盘后，如果 tool 批次继续执行，turn 仍未结束。
- `stream_turn_done` 只能在 final assistant durable append 且本轮 provider/tool loop 完全结束后发送。
