# LOGGING.md

这份文档定义 ClawParty 里的日志边界、日志级别和去重原则。

目标只有三个：

1. 让日志能稳定支持排障。
2. 让统计/计费只依赖少数权威事件。
3. 避免同一件事在多个模块重复记录。

## 总规则

| 规则 | 说明 |
| --- | --- |
| 一件事只选一个权威日志源 | 不要在 `server`、`session`、`agent`、`api` 四层都打一遍同义日志。 |
| `info` 只留稳定边界事件 | 启动、关闭、完成、失败、关键状态切换、权威统计事件。 |
| 高频过程日志默认降到 `debug` | round started、tool started、mailbox drained、typing、sink fan-out 这类过程不进 `info`。 |
| 原始内容默认不记 | 不记录完整用户文本、完整模型回复、完整工具输出，除非该日志本身就是专门的 transcript 或 API body debug。 |
| 敏感信息必须脱敏 | token、api key、Authorization、cookie、可执行密钥、私密 headers/body 一律脱敏。 |
| 统计只能依赖权威事件 | `/status`、usage、spend 只依赖明确定义的日志事件，不能靠“近似同义日志”拼出来。 |
| 同一事件允许分 level 记不同粒度 | 不是只有“记/不记”两种状态。`info` 记摘要，`debug` 记展开后的详细上下文。 |

## Log Level 标准

| Level | 什么时候用 | 什么时候不要用 |
| --- | --- | --- |
| `ERROR` | 进程级失败、关键任务失败且当前路径无法正常完成 | 可恢复的重试、预期内拒绝 |
| `WARN` | 降级运行、数据异常、恢复失败、外部依赖异常但服务还能继续 | 高频失败重试中的每一步细节 |
| `INFO` | 生命周期边界、权威统计事件、关键业务状态变化 | 高频过程事件、纯调试 breadcrumb |
| `DEBUG` | 高频内部过程、诊断细节、request started、内部队列流转 | 长期依赖的统计事件 |
| `TRACE` | 当前项目默认不依赖 | 不建议新增，除非后续明确打开更细颗粒度方案 |

## 记录粒度规则

| 规则 | `INFO/WARN/ERROR` 应记什么 | `DEBUG` 应记什么 |
| --- | --- | --- |
| 完成类事件 | 结果摘要、关键标识、统计字段、能帮助定位的问题摘要 | 该次完成事件的详细上下文、展开后的参数摘要、必要的预览字段 |
| 启动类事件 | 谁开始了什么、在哪个会话/agent/channel 下开始 | 更详细的配置和前置条件 |
| 失败类事件 | 错误摘要、关键入参摘要、影响范围 | 更多上下文、脱敏后的详细 payload / output preview |
| 高频过程事件 | 通常不需要 | 可以记详细 breadcrumb |

一句话讲：

- `info` 不是“不详细”，而是“摘要且稳定”。
- `debug` 不是“另一份重复日志”，而是“同一事件的展开视图”。

## 事件矩阵

| 区域 | 事件/内容 | 要不要记 | Level | 权威日志源 | 备注 |
| --- | --- | --- | --- | --- | --- |
| Server | 进程启动 | 要 | `info` | `server` | 包含 `workdir`、`config`、启动模式 |
| Server | 进程正常关闭 | 要 | `info` | `server` | 只保留一次结束事件 |
| Server | 致命退出 | 要 | `error` | `server` | 这是主故障入口 |
| Server | `.env` 加载成功 | 要 | `info` | `server` | 低频且有运维价值 |
| Server | config 自动升级 | 要 | `info` | `server` | 用于定位兼容路径 |
| Server | `/help` 命中 | 记，但不进主观察面 | `debug` | `server` | 不应污染业务统计 |
| Server | 普通命令路由细节 | 不建议单独记 | - | - | 除非发生错误或降级 |
| Channel | channel 启动/监听成功 | 要 | `info` | `channel` | 这是接入层生命周期 |
| Channel | channel 缺配置被禁用 | 要 | `warn` | `channel` | 如 web 没 token |
| Channel | 外部平台请求失败但可恢复 | 要 | `warn` | `channel` | 如 telegram 重试、queue blocked |
| Channel | 外部平台永久失败 | 要 | `warn` 或 `error` | `channel` | 是否 `error` 取决于是否影响主流程终止 |
| Channel | typing / sendChatAction | 记，但只作调试 | `debug` | `channel` | 高频且无统计价值 |
| Channel | CLI send / sink fan-out | 记，但只作调试 | `debug` | `channel` | 不作为业务完成证据 |
| Conversation | conversation 创建/删除 | 可以记 | `info` | `server` 或 `conversation` | 若后续要补，保留单点即可 |
| Session | session 创建/恢复/销毁 | 要 | `info` | `session` | 生命周期事件 |
| Session | user message enqueue | 要 | `info` | `session` | 这是会话入口的稳定边界 |
| Session | actor message enqueue | 要 | `info` | `session` | 背景消息进入前台上下文的稳定边界 |
| Session | mailbox drained / staged | 记，但只作调试 | `debug` | `session` | 和 enqueue 语义重叠，不能再做统计源 |
| Session | visible message append | 记，但只作调试 | `debug` | `session` | 只是内部状态推进 |
| Session | transcript 写入失败 | 要 | `warn` | `session` | 影响可观测性，但不一定阻断主流程 |
| Session | idle compaction 完成 | 要 | `info` | `session` | 低频、状态边界明确 |
| Session | idle compaction 失败 | 要 | `warn` | `session` | 需要运维可见 |
| Agent | turn 总 usage | 要 | `info` | `agent` | 权威事件：`turn_token_usage` |
| Agent | model call completed | 要，且要分层记录 | `info` + `debug` | `agent` | `info` 记摘要，`debug` 记展开上下文；权威事件仍是 `agent_frame_model_call_completed` |
| Agent | session started / yielded / completed | 要 | `info` | `agent` | 生命周期边界 |
| Agent | compaction completed | 要 | `info` | `agent` | 有诊断价值，频率可接受 |
| Agent | round started | 记，但只作调试 | `debug` | `agent` | 高频过程 |
| Agent | model call started | 记，但只作调试 | `debug` | `agent` | 开始事件不作统计依据 |
| Agent | tool call started | 记，但只作调试 | `debug` | `agent` | 高频过程 |
| Agent | tool call completed（成功） | 要，且要分层记录 | `info` + `debug` | `agent` | `info` 记工具执行摘要，`debug` 记脱敏后的详细结果 |
| Agent | tool call completed（失败） | 要，且要分层记录 | `warn`/`info` + `debug` | `agent` | 失败需要主观察面可见，`debug` 再展开上下文 |
| Agent | tool-wait compaction scheduled/started/completed | 记，但默认调试 | `debug` | `agent` | 更偏内部控制流 |
| Agent | background agent enqueued/started/replied | 要 | `info` | `agent` | 背景代理生命周期 |
| API | upstream request started | 要，主要用于展开请求侧细节 | `debug` | `api` | 这里适合记录 request payload、cache-control、headers/body preview |
| API | upstream request completed | 要，且要承接 cache 返回值 | `info` + `debug` | `api` | `info` 记 request outcome + usage/cache 摘要，`debug` 可保留更展开的 response 细节 |
| API | upstream request failed | 要 | `warn` | `api` | API 层权威失败事件 |
| API | request/response headers | 要，但必须脱敏 | 跟随 request 事件 | `api` | 仅保留脱敏版本 |
| API | request/response body preview | 可记 | 跟随 request 事件 | `api` | 默认 preview，必要时 `full` |
| API | 原始密钥/完整敏感 body | 不允许 | - | - | 必须脱敏或关闭 |
| Progress | 用户可见 progress 更新 | 默认不单独记 | - | - | 用户已经在 channel 看到了，不再重复写主日志 |
| Progress | progress 更新失败 | 要 | `warn` | `channel` | 因为会影响用户体验 |
| Transcript | malformed transcript line | 要 | `warn` | `transcript`/`session` | 数据损坏信号 |
| Security | 鉴权失败、非法请求、签名错误 | 要 | `warn` | `server` 或 `channel` | 这是安全信号 |
| Security | 明显内部一致性破坏或不可恢复安全故障 | 要 | `error` | `server` | 例如关键鉴权组件失效 |

## 明确“不用 log”的内容

| 内容 | 原因 | 替代方案 |
| --- | --- | --- |
| 同一条消息在 `received`、`drained`、`staged` 三层各打一条 `info` | 重复、语义混乱、容易误做统计 | 只保留 `enqueue` 为 `info`，其余降到 `debug` |
| 每次 typing 心跳都进 `info` | 高频噪音 | 仅 `debug` |
| 每次 sink fan-out 都进 `info` | 只是内部转发，不是用户真正完成事件 | 仅 `debug` |
| 每个 round/tool step 都进 `info` | 容易把真正异常淹没 | 高频过程统一 `debug` |
| 完整用户文本/完整 assistant 文本进普通结构化日志 | 隐私风险大，也会放大日志量 | 用户内容走 transcript；普通日志只保留摘要字段 |
| 完整工具输出进普通结构化日志 | 噪音大、可能包含敏感内容 | 普通日志只留长度、是否失败、tool id |
| 同一个 API 完成事件同时在 `api` 和 `agent` 层重复写“完成语义” | 统计 join 容易双算 | `api` 层记 request outcome，`agent` 层只记 model-call/accounting |

## 当前推荐的权威事件

| 目的 | 权威事件 | 所在流 |
| --- | --- | --- |
| 每 turn 总 token/usage 汇总 | `turn_token_usage` | `agent` |
| 每次模型调用的分模型计费 | `agent_frame_model_call_completed` | `agent` |
| API 请求完成 | `upstream_api_request_completed` | `api` |
| API 请求失败 | `upstream_api_request_failed` | `api` |
| foreground/background session 生命周期 | `session_created` / `session_restored` / `session_destroyed` 等 | `session` |
| 用户消息真正进入 durable mailbox | `user_message_enqueued` | `session` |
| actor 消息真正进入 durable mailbox | `actor_message_enqueued` | `session` |

## Completed 事件的推荐分层

### Tool Call Completed

| Level | 应记录内容 |
| --- | --- |
| `INFO` | `tool_name`、`tool_call_id`、`session_id`、`agent_id`、`round_index`、成功/失败、`output_len`、必要的参数摘要 |
| `WARN` | 在失败时记录上面这些字段，再加 `error` 摘要、失败分类、必要的 stderr/output 摘要 |
| `DEBUG` | 在 `INFO/WARN` 基础上，再记录脱敏后的完整参数摘要、执行目标、remote/workdir、退出状态、截断后的 stdout/stderr 或 tool output preview |

这里最重要的一点是：

- `INFO` 层不应该只写“tool finished”。
- 对 `exec` / `shell` / `dsl` 这类工具，`INFO` 至少要能看出“执行了什么类型的命令”和“结果大概如何”。
- `DEBUG` 才展开完整细节，但仍然要脱敏和截断。

### Model Call Completed

| Level | 应记录内容 |
| --- | --- |
| `INFO` | `model`、`model_key`、`session_id`、`agent_id`、`api_request_id`、`round_index`、`tool_call_count`、`input_total_tokens`、`output_total_tokens`、`context_total_tokens`、`cache_read_input_tokens`、`cache_write_input_tokens`、`cache_uncached_input_tokens`、`normal_billed_input_tokens` |
| `DEBUG` | 在 `INFO` 基础上，再补这次调用对应的请求侧上下文摘要，例如 request payload 里采用了什么 `cache_control` 策略、是否显式打了 cache breakpoint、必要的 request/response preview |

你提到的这两类 cache 信息，我建议都要有：

| 方向 | 应记录内容 | 推荐位置 |
| --- | --- | --- |
| 请求侧 | 这次 message payload 最终使用的 `cache_control` 状态，例如 `type`、`ttl`、是否存在显式 cache marker | `api` request started / `debug`，必要时在 `model call completed` 摘要补规范化字段 |
| 响应侧 | provider 返回的 cache 使用结果，例如 `cache_read_input_tokens`、`cache_write_input_tokens`、`cache_uncached_input_tokens` | `model call completed` 的 `info` 和 `api request completed` 的 `info` |

## 当前实现 vs 建议目标

### 当前实现已经有的

| 事件 | 当前已记录字段 |
| --- | --- |
| `agent_frame_model_call_completed` | `session_id`、`channel_id`、`agent_kind`、`model_key`、`model`、`round_index`、`tool_call_count`、`api_request_id`、`input_total_tokens`、`output_total_tokens`、`context_total_tokens`、`cache_read_input_tokens`、`cache_write_input_tokens`、`cache_uncached_input_tokens`、`normal_billed_input_tokens` |
| `upstream_api_request_started` | `api_request_id`、provider、`api_kind`、`auth_kind`、`model`、`method`、`url`、`timeout_seconds`、脱敏后的 request headers、body preview/full json |
| `upstream_api_request_completed` | `api_request_id`、provider、`status_code`、`elapsed_ms`、脱敏后的 response headers、`response_id`、usage/cache 返回值、body preview/full json |
| `agent_frame_tool_call_completed` | `session_id`、`channel_id`、`agent_kind`、`model_key`、`model`、`round_index`、`tool_name`、`tool_call_id`、`output_len`、`errored` |

### 当前实现还不够的

| 缺口 | 说明 |
| --- | --- |
| `tool call completed` 的 `INFO` 摘要还不够强 | 现在成功的 tool completed 更偏 `debug`，而且没有统一输出“执行了什么命令/参数摘要” |
| 请求侧 `cache_control` 还没拆成统一顶层字段 | 现在主要是通过 `upstream_api_request_started` 的 request body preview/json 间接看到，而不是显式的 `request_cache_control_type` / `request_cache_control_ttl` |
| `model call completed` 还没把请求侧 cache 策略做成规范化摘要 | 响应侧 cache token 已经有了，但请求侧 cache 策略还没有作为统一字段固定下来 |

## 字段建议

| 字段 | 是否推荐 | 说明 |
| --- | --- | --- |
| `log_stream` | 必须 | `server` / `channel` / `session` / `agent` / `api` |
| `log_key` | 强烈推荐 | 用于路由到分流 jsonl |
| `kind` | 必须 | 稳定事件名，供统计或排障使用 |
| `channel_id` | 推荐 | 跟 conversation 关联 |
| `conversation_id` | 推荐 | 便于跨 session 聚合 |
| `session_id` | 推荐 | session 级追踪 |
| `agent_id` | 推荐 | agent 级追踪 |
| `api_request_id` | API/模型调用强烈推荐 | 串联 `agent` 和 `api` |
| `model` / `model_key` | 推荐 | 计费、问题定位 |
| `elapsed_ms` | 推荐 | 性能诊断 |
| `request_cache_control_type` | 对模型请求强烈推荐 | 规范化记录请求侧 cache-control 类型 |
| `request_cache_control_ttl` | 对模型请求推荐 | 规范化记录请求侧 ttl |
| `request_has_cache_breakpoint` | 对模型请求推荐 | 标识这次 payload 是否显式打了 cache marker |
| `error` | 失败时必须 | 用统一字符串摘要 |
| `argument_summary` | 对工具调用强烈推荐 | `info` 层也要能看懂大概执行了什么 |
| `output_preview` / `stderr_preview` | 仅 `debug` 或失败时推荐 | 必须截断和脱敏 |
| `has_text` / `attachment_count` / `output_len` | 推荐 | 用摘要替代大内容 |
| 原始密钥、完整正文 | 禁止 | 必须脱敏或省略 |

## 实施顺序建议

| 优先级 | 事项 | 说明 |
| --- | --- | --- |
| P0 | 先固定权威事件 | 先保证 `turn_token_usage`、`agent_frame_model_call_completed`、API outcome 不再漂移 |
| P0 | 清理重复 `info` | 优先清掉 mailbox、typing、sink、tool step 这类高频噪音 |
| P1 | 统一事件命名 | 用 `*_enqueued`、`*_completed`、`*_failed` 这类稳定语义 |
| P1 | 统一关键字段 | 先把 `channel_id`、`conversation_id`、`session_id`、`api_request_id` 补齐 |
| P2 | 再补遗漏的 lifecycle log | 比如 conversation create/delete、关键配置生效点 |

## 一句话决策法

如果一个事件满足下面任一条件，它通常值得记 `info`：

- 它是稳定生命周期边界。
- 它是统计/计费/审计的权威来源。
- 它表示用户侧或运维侧需要知道的异常或降级。

如果它只是“系统正在一步一步做事”，默认放到 `debug`，不要进 `info`。
