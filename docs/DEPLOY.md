# 部署说明

本文档说明如何在一台 Linux 主机上部署 ClawParty 的 `agent_host` 服务。示例中的路径、服务名、环境变量名都可以按你的机器调整；不要把真实 token、API key 或私有路径直接提交到仓库。

## 1. 前置条件

- Linux 主机
- Rust stable toolchain
- `systemd --user`
- 如果使用 `bubblewrap` 沙盒：安装 `bwrap`
- Git

## 2. 准备目录

建议准备两个目录：

- 仓库目录：例如 `/srv/clawparty/ClawParty2.0`
- 运行目录：例如 `/srv/clawparty/workdir`

运行目录会保存：

- session 持久化
- workspaces
- snapshots
- conversation memory artifacts
- 共享的 `agent/USER.md`、`agent/IDENTITY.md`

## 3. 环境变量

复制 `.env.example` 为 `.env`，然后只在本机填写敏感值。常见变量例如：

```dotenv
OPENROUTER_API_KEY=...
OPENAI_API_KEY=...
TELEGRAM_BOT_TOKEN=...
```

不要把真实密钥写进 JSON 配置，也不要把填过值的 `.env` 提交到仓库。

## 4. 配置文件

服务端主配置一般是一个 JSON 文件，例如 `deploy_telegram.json`。当前推荐使用 `0.4` 配置结构：

```json
{
  "version": "0.4",
  "main_agent": {
    "model": "gpt54",
    "enable_context_compression": true,
    "context_compaction": {
      "trigger_ratio": 0.9,
      "token_limit_override": null,
      "recent_fidelity_target_ratio": 0.18
    },
    "idle_compaction": {
      "enabled": false,
      "poll_interval_seconds": 15,
      "min_ratio": 0.5
    },
    "timeout_observation_compaction": {
      "enabled": true
    }
  }
}
```

说明：

- `context_compaction.trigger_ratio`
  表示达到模型上下文窗口的多少比例后触发长度压缩
- `idle_compaction`
  表示空闲时的后台压缩
- `timeout_observation_compaction`
  表示 tool 执行过长时，在 turn 内做 timeout-observation compaction

## 5. 编译

在仓库根目录执行：

```bash
cargo build --release --manifest-path agent_host/Cargo.toml
```

如果你也需要单独运行 `agent_frame` 的 CLI：

```bash
cargo build --release --manifest-path agent_frame/Cargo.toml --bin run_agent
```

## 6. 启动命令

典型启动方式：

```bash
./target/release/agent_host \
  --config /path/to/deploy_telegram.json \
  --workdir /path/to/workdir
```

## 7. systemd 用户服务

一个脱敏后的 `systemd --user` service 例子：

```ini
[Unit]
Description=ClawParty Agent Host
After=network.target

[Service]
Type=simple
WorkingDirectory=/srv/clawparty/ClawParty2.0
ExecStart=/srv/clawparty/ClawParty2.0/target/release/agent_host --config /srv/clawparty/ClawParty2.0/deploy_telegram.json --workdir /srv/clawparty/workdir
Restart=always
RestartSec=3
EnvironmentFile=/srv/clawparty/ClawParty2.0/.env

[Install]
WantedBy=default.target
```

安装与启动：

```bash
systemctl --user daemon-reload
systemctl --user enable clawparty2.service
systemctl --user restart clawparty2.service
systemctl --user status clawparty2.service --no-pager
```

## 8. 升级流程

推荐升级步骤：

1. 拉取最新代码
2. 检查 `VERSION`
3. 重新编译 release
4. 重启 `clawparty2.service`
5. 检查日志和 `/status`

示例：

```bash
git pull --ff-only
cargo build --release --manifest-path agent_host/Cargo.toml
systemctl --user restart clawparty2.service
systemctl --user status clawparty2.service --no-pager
```

## 9. Snapshot 与恢复

当前 `/snapsave` 和 `/snapload` 会完整保存和恢复：

- workspace 内容
- session 状态
- conversation memory artifacts

所以升级前，如果要做保险备份，可以先在聊天里执行一次 `/snapsave <name>`。

## 10. 发布产物

CI/CD 在以下条件同时满足时会自动创建 GitHub Release：

- CI 通过
- `VERSION` 文件在当前主分支提交中发生变化
- 该版本对应的 tag 尚未存在

Release 会自动：

- 读取 `VERSION` frontmatter 的版本号
- 提取对应 `# <version>` 下的更新说明
- 构建常见 Linux 架构的 release 二进制
- 上传 GitHub Release assets
