# Codex Tool Diff

本文记录本项目工具系统和 OpenAI Codex 工具实现之间仍然有意义的差异。已经落地并不再构成差异的旧方案说明已删除。

参考代码：

- 本项目 shell：`core/src/session_actor/tool_catalog/process_tools.rs`
- 本项目 tool batch：`core/src/session_actor/tool_batch.rs`
- 本项目 file tools：`core/src/session_actor/tool_catalog/file_tools.rs`
- 本项目 apply_patch：`core/src/session_actor/tool_catalog/file_tools/patch.rs`
- Codex unified exec：`learnable_materials/codex/codex-rs/core/src/tools/handlers/unified_exec.rs`
- Codex process manager：`learnable_materials/codex/codex-rs/core/src/unified_exec/process_manager.rs`
- Codex apply_patch：`learnable_materials/codex/codex-rs/tools/src/apply_patch_tool.rs`、`learnable_materials/codex/codex-rs/core/src/tools/handlers/apply_patch.rs`
- Codex tool registry plan：`learnable_materials/codex/codex-rs/tools/src/tool_registry_plan.rs`

## 当前已实现

### Tool Batch

- 工具定义已经带 `ToolConcurrency`。
- 工具 description 会自动标注 parallel / serial。
- batch executor 会并发运行连续 parallel 工具，遇到 serial 工具时按原顺序独占执行。
- serial 是全局 barrier，不是“同一个工具名自己串行”。
- 单个工具失败或 panic 只生成自己的 error result，不会跳过同一 batch 内其它工具。
- 文件写入、`edit`、`apply_patch`、全部 shell 工具、下载启动/取消、media stop、Host bridge 状态变更和 skill 持久化类工具为 serial。

### Shell Tools

旧 `shell` 工具已经移除，不再保留兼容入口。模型可见工具收敛为：

| 工具 | 并发 | 作用 |
|---|---|---|
| `shell_exec` | serial | 启动一个 fresh process 执行命令；默认 `tty=false`，stdin 关闭，不复用隐藏 shell。 |
| `shell_write_stdin` | serial | 向 `tty=true` 的运行中进程写 raw stdin；`chars=""` 时只观察已有输出。 |
| `shell_stop` | serial | 停止指定 `process_id` 的运行中进程。 |

当前语义：

- `shell_exec` 每次都启动一个新进程，完成信号来自 child process exit，不再注入 sentinel。
- `tty=false` 是默认路径：命令通过 shell `-c` / `-lc` 执行，stdin 关闭，stdout/stderr 合并进工具输出。
- `tty=true` 会分配 PTY 并保留 stdin writer，适合显式交互 shell、TUI 或需要终端语义的程序。
- `login=true` 使用 shell login exec flag，例如 `zsh -lc <command>`，用于加载用户 shell profile 环境；它不复用上一条命令的 `export` / `cd` / `source` 状态。
- `shell_exec` 如果在 `yield_time_ms` 内退出会返回 `exit_code`；如果仍在运行会返回 `process_id`，后续用 `shell_write_stdin(chars="")` 观察或 `shell_stop` 停止。
- 非空 `shell_write_stdin` 只允许写入 `tty=true` 进程；对 `tty=false` 进程写入会返回 `stdin_closed`。
- `remote` 在 `shell_exec` 创建进程时决定执行位置；后续 `process_id` 已绑定该 remote。冲突 remote 会返回 invalid arguments。
- `yield_time_ms` 取值范围：
  - `shell_exec`: 默认 `10000`，范围 `250..30000`。
  - `shell_write_stdin`: 默认 `250`，范围 `250..30000`。
- `timeout_ms` 可设置运行硬超时；超时后进程会被 kill。
- `max_output_chars` 默认 `20000`，最大 `200000`。
- 模型可见输出按字符做 middle truncation，返回 `output_truncated`、`original_chars`、`total_output_lines`。
- pending output buffer 是 1 MiB head/tail buffer，不提供随机读取中间内容。
- PTY 默认尺寸 `100x30`，`tty=true` 时可用 `cols=40..200`、`rows=10..80` 固定尺寸。

### Terminal Snapshot

Shell runtime 现在维护 terminal screen state，不再只是用控制码数量做粗略启发式。

当前解析：

- SGR 颜色码。
- 光标移动。
- 清屏 / 清行。
- carriage return 覆盖。
- alternate screen。

返回策略：

- 普通 plain text 和带颜色日志返回 stripped `output`。
- 颜色控制码本身不会触发 `terminal_snapshot`。
- 屏幕型输出返回 `terminal_snapshot.visible_text`。
- 每个 `shell_exec` fresh process 都有独立 screen state。
- 同一个运行中的 `tty=true` 任务，后续 `shell_write_stdin` 会继续累计同一份 screen state。
- alternate screen 已退出时不返回空 snapshot。

### File Editing

- `file_write`、`edit`、`apply_patch` 都是 serial。
- `apply_patch` 支持 `format=auto|codex|unified`。
- 本地 workspace 支持 Codex-style `*** Begin Patch` / `*** End Patch` envelope。
- Codex-style patch 支持 add / delete / update / move。
- 本地 Codex-style patch 写入前会解析和校验路径、上下文、文件操作。
- unified diff 继续走 `git apply`。
- SSH remote 仍使用 unified diff。

### Provider Tool Result Translation

当前和 Codex 保持一致：

- tool call / function call 带 arguments。
- tool result / function_call_output 不重复带 arguments，只通过 `tool_call_id` / `call_id` 配对。
- UI 可以把 tool call arguments 和 tool result 合并展示，但这不是 provider 发送给模型的 tool result 结构。

## 仍然未实现或仍有差异

### Shell Process Manager 所有权

当前 shell process manager 仍是 `process_tools.rs` 内的静态全局 `OnceLock<Mutex<ShellManager>>`。

Codex 更接近 session-owned process manager：process store、output buffer、lifecycle 和 turn/session 边界绑定更清楚。

后续建议：

- 把 shell process manager 移到 `LocalToolBatchExecutor` 或 session service 所有。
- shell 生命周期跟随 session，而不是进程级静态全局。
- 为后续 session restore / cleanup / per-session resource limit 留出边界。

### Output Notify

当前 reader 线程把 PTY output 写入内存 head/tail buffer，但等待侧仍用短 sleep 循环 drain。

Codex 使用 output `Notify`、exit token、deadline、pause-state change 的事件驱动等待。

后续建议：

- 给 shell process session 增加 output notify / condvar。
- reader 收到新 chunk 后唤醒 `shell_exec` / `shell_write_stdin`。
- exit watcher 唤醒等待者。
- 保留 deadline 作为模型阻塞上限。

### Post-Exit Close Wait

当前 shell exit 后会短暂 sleep 并 drain，语义接近但没有独立的 post-exit close wait 状态。

Codex 在收到 exit signal 后最多等约 50ms 收尾最后输出。

后续建议：

- 把 post-exit wait 显式建模，避免魔法 sleep 分散在 collect 逻辑里。

### Pause-State Deadline

当前 shell 等待没有接入 out-of-band elicitation pause。

Codex 在 pause-state change 时会重算 deadline，暂停期间不消耗 `yield_time_ms`。

后续建议：

- 如果本项目后续有 tool-level pause/approval/elicitation 状态，把 shell wait deadline 接入该状态。

### Output Truncation 单位

当前 shell 使用 `max_output_chars`，按字符 middle truncation。

Codex 使用 `max_output_tokens`，并与当前 turn truncation budget 取较小值。

保留当前差异的理由：

- 本项目多数工具已使用 char budget。
- 这样可以避免 shell runtime 强依赖 token estimator。

后续可选：

- 在 tool result 中增加估算 token 统计。
- 对支持 token estimator 的 provider，在发送前做第二层 token-aware cap。

### Terminal Emulator 完整度

当前 terminal screen state 已覆盖常见日志、颜色、光标移动、清屏、回车覆盖和 alternate screen。

仍未覆盖：

- 完整 ANSI/VT 状态机。
- scroll region。
- insert/delete char/line。
- 宽字符和组合字符的准确列宽。
- 样式/颜色保留。
- foreground child 进程级识别。

当前 process 层级：

- 每次 `shell_exec` 的前台命令窗口是 screen state reset 边界。
- 同一运行中任务的 observe/write_stdin 会继续累计 screen state。
- 还没有识别长期 shell 内部的真实 foreground child PID。

### Shell Apply Patch 拦截

Codex 会识别 shell/exec 中的 `apply_patch` 调用并转入 apply_patch handler。

当前本项目还未实现该拦截。

后续建议：

- 先识别明确的 `apply_patch <<EOF ...` / `apply_patch <<'PATCH' ...`。
- 能解析时直接走 `apply_patch` 工具逻辑。
- 不能解析时返回提示，要求模型改用 `apply_patch` 工具。
- 暂不拦截所有 `git apply`，避免误伤构建脚本或第三方工具。

### Apply Patch Freeform Grammar

Codex 对 `apply_patch` 有 freeform grammar 形态，减少 JSON string escaping 和 malformed patch。

当前本项目仍是 JSON function tool，patch body 放在 `patch` 字符串中。

后续建议：

- 如果 provider 支持 freeform/custom tool，再为 `apply_patch` 增加 grammar 版本。
- 保留 JSON schema 版本作为兼容 fallback。

### Apply Patch Remote Codex-Style

当前 Codex-style patch 只支持本地 workspace。

SSH remote 仍使用 unified diff。

后续建议：

- 在 remote 侧部署同一套 Codex-style parser/runner。
- 或把 patch 操作解析成本地结构化 operations，再通过 remote file API 逐项写入。

### Structured File Change Events

当前 apply_patch 返回基础结果，但还没有 Codex 那种完整 ToolEmitter / file change event 流。

后续建议：

- 为 add/delete/update/move 生成结构化 file change event。
- 在前端展示文件变更摘要，而不是只展示 JSON result。
- 将 shell apply_patch 拦截后的变更也统一进入同一事件流。

## 当前优先级

建议下一步优先级：

1. 把 shell manager 从静态全局迁到 session-owned manager。
2. 给 shell 输出等待接入 Notify/condvar，替代 sleep-loop。
3. 实现 shell apply_patch 拦截。
4. 给 apply_patch 增加结构化 file change events。
5. 评估 `max_output_tokens` 是否需要作为 shell 的可选参数，而不是替代 `max_output_chars`。
