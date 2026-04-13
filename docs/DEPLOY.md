# ClawParty 部署说明

ClawParty 是一个 Rust 写的常驻 agent host。主要二进制是 `partyclaw`，配置文件通常从 `deploy_telegram.json` 复制出来，运行时状态写入一个独立的 `workdir`。

最重要的三个入口是：

```bash
cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
./target/release/partyclaw config ./deploy_prod.json
./target/release/partyclaw --config ./deploy_prod.json --workdir ./deploy_workdir
```

Linux 还可以用内置的 `setup` 命令生成 `systemd --user` 服务：

```bash
./target/release/partyclaw setup ./deploy_prod.json ./deploy_workdir mybot
```

`setup` 目前只支持 Linux。macOS 用 `launchd`，Windows 可以原生运行 `.exe`，服务化建议用任务计划程序或 NSSM / WinSW 这类服务包装器。

## 共同准备

部署前需要：

- Git
- Rust stable toolchain
- 一个模型 API key，例如 `OPENROUTER_API_KEY`
- 如果跑 Telegram channel，需要 `TELEGRAM_BOT_TOKEN`

推荐文件职责：

- `deploy_telegram.json`: 仓库里的 Telegram 配置模板
- `deploy_prod.json`: 你自己的部署配置
- `.env`: 本机密钥，不要提交
- `deploy_workdir/`: 运行时持久化目录，保存会话、workspace、日志、快照等数据

`.env` 示例：

```dotenv
OPENROUTER_API_KEY=sk-or-...
TELEGRAM_BOT_TOKEN=...
OPENAI_API_KEY=...
```

启动时，`partyclaw` 会自动读取：

- 当前工作目录下的 `.env`
- 配置文件所在目录下的 `.env`

所以最简单的方式是把 `.env` 放在仓库根目录，并从仓库根目录启动服务。

## Linux

下面以 Ubuntu / Debian 为例。其他发行版把包管理命令换成对应命令即可。

### 1. 安装依赖

```bash
sudo apt-get update
sudo apt-get install -y git curl build-essential pkg-config

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

如果要使用生产推荐的 `bubblewrap` 沙盒，再装：

```bash
sudo apt-get install -y bubblewrap
```

不想用 `bubblewrap` 时，在 `partyclaw config` 的 `Sandbox` 页面选择 `subprocess`。

### 2. 拉代码并编译

```bash
git clone <your-repo-url> ClawParty2.0
cd ClawParty2.0

cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
```

编译完成后的二进制路径是：

```bash
./target/release/partyclaw
```

### 3. 准备配置和密钥

```bash
cp deploy_telegram.json deploy_prod.json
mkdir -p deploy_workdir
cp .env.example .env

${EDITOR:-nano} .env
```

至少填好：

```dotenv
OPENROUTER_API_KEY=...
TELEGRAM_BOT_TOKEN=...
```

然后打开配置 TUI：

```bash
./target/release/partyclaw config ./deploy_prod.json
```

在 TUI 中重点检查：

- `Models`: 模型 alias、上游模型名、API key 环境变量
- `Tooling`: `web_search`、`image`、`image_gen` 指向哪个模型 alias
- `Main Agent`: 默认语言、memory、compaction
- `Sandbox`: Linux 可选 `bubblewrap` 或 `subprocess`；需要让 bubblewrap 沙盒内命令访问宿主 Docker 时，显式打开 `map_docker_socket`
- `Channels`: Telegram 的 `bot_token_env` 是否是 `TELEGRAM_BOT_TOKEN`

`sandbox.map_docker_socket` 默认是 `false`。只有在 Linux + `sandbox.mode = "bubblewrap"` + 宿主存在 `/run/docker.sock` 时才会把 Docker socket 映射进沙盒；macOS、Windows 和 `subprocess` 沙盒会直接跳过这个功能。

如果开启 Docker socket 映射，请确保运行服务的 Linux 用户本身已经属于 `docker` 组：

```bash
groups
sudo usermod -aG docker "$USER"
```

加入组后需要重新登录，或重启 user manager/session，才能让 systemd user service 拿到新的组成员身份。

### 4. 前台试运行

```bash
./target/release/partyclaw --config ./deploy_prod.json --workdir ./deploy_workdir --sandbox-auto
```

确认 Telegram 或 CLI channel 能正常响应后，再做服务化部署。

### 5. systemd 用户服务部署

```bash
./target/release/partyclaw setup ./deploy_prod.json ./deploy_workdir mybot

systemctl --user restart mybot.service
systemctl --user enable mybot.service
systemctl --user status mybot.service --no-pager
```

`partyclaw setup` 生成的是 systemd user service，不会写入 `SupplementaryGroups=docker`。不要手工给 user service 加类似下面的 drop-in：

```ini
[Service]
SupplementaryGroups=docker
```

用户级 systemd 在不少环境里不能这样切换组，可能导致服务启动前失败并报 `status=216/GROUP` / `Changing group credentials failed: Operation not permitted`。正确做法是让当前用户加入 `docker` 组，然后按上面的 `sandbox.map_docker_socket` 配置决定是否映射 `/run/docker.sock`。

如果希望机器重启后，即使该用户没有登录也自动拉起服务：

```bash
sudo loginctl enable-linger "$USER"
loginctl show-user "$USER" -p Linger
```

查看日志：

```bash
journalctl --user -u mybot.service -n 100 --no-pager
journalctl --user -u mybot.service -f
```

## macOS

macOS 可以原生编译运行，但没有 `bubblewrap`。请在配置里使用 `subprocess` 沙盒。

### 1. 安装依赖

如果还没有 Xcode Command Line Tools：

```bash
xcode-select --install
```

安装 Rust：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

如果没有 Git，可以用 Homebrew 安装：

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
brew install git
```

### 2. 拉代码并编译

```bash
git clone <your-repo-url> ClawParty2.0
cd ClawParty2.0

cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
```

编译完成后的二进制路径是：

```bash
./target/release/partyclaw
```

### 3. 准备配置和密钥

```bash
cp deploy_telegram.json deploy_prod.json
mkdir -p deploy_workdir
cp .env.example .env

nano .env
```

打开配置 TUI：

```bash
./target/release/partyclaw config ./deploy_prod.json
```

在 `Sandbox` 页面选择：

```text
subprocess
```

### 4. 前台试运行

```bash
./target/release/partyclaw --config ./deploy_prod.json --workdir ./deploy_workdir --sandbox-auto
```

### 5. launchd 用户服务部署

先在仓库根目录生成 LaunchAgent：

```bash
REPO="$(pwd)"
LABEL="com.clawparty.mybot"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
SERVICE_PATH="gui/$(id -u)/$LABEL"

mkdir -p "$HOME/Library/LaunchAgents"
mkdir -p "$REPO/deploy_workdir/launchd"

cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$REPO/target/release/partyclaw</string>
    <string>--config</string>
    <string>$REPO/deploy_prod.json</string>
    <string>--workdir</string>
    <string>$REPO/deploy_workdir</string>
    <string>--sandbox-auto</string>
  </array>
  <key>WorkingDirectory</key>
  <string>$REPO</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$REPO/deploy_workdir/launchd/stdout.log</string>
  <key>StandardErrorPath</key>
  <string>$REPO/deploy_workdir/launchd/stderr.log</string>
</dict>
</plist>
PLIST

plutil -lint "$PLIST"
```

启动服务：

```bash
launchctl bootout "gui/$(id -u)" "$PLIST" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl kickstart -k "$SERVICE_PATH"
launchctl print "$SERVICE_PATH"
```

查看日志：

```bash
tail -n 100 "$REPO/deploy_workdir/launchd/stdout.log"
tail -n 100 "$REPO/deploy_workdir/launchd/stderr.log"
tail -f "$REPO/deploy_workdir/launchd/stderr.log"
```

停止服务：

```bash
launchctl bootout "gui/$(id -u)" "$PLIST"
```

## Windows

Windows 可以原生编译运行。请在配置里使用 `subprocess` 沙盒；`bubblewrap` 只适用于 Linux。

当前 Windows 原生路径的行为：

- agent 执行 shell 命令时使用 `%COMSPEC% /C`，通常是 `cmd.exe /C`
- PTY 只在 Unix 平台启用；Windows 上请求 TTY 时会退化为普通管道执行
- 后台任务进程用 Windows 的 `tasklist` / `taskkill` 检查和终止
- `partyclaw setup` 仍然只支持 Linux，不会生成 Windows Service
- `sandbox.map_docker_socket` 是 Linux bubblewrap 专用能力，Windows 会跳过；Windows 上请直接使用 Docker Desktop / 原生命令，不需要映射 `/run/docker.sock`

### 1. 安装依赖

在 PowerShell 中执行：

```powershell
winget install --id Git.Git -e
winget install --id Rustlang.Rustup -e
```

重新打开 PowerShell，让 `cargo` 进入 `PATH`。确认版本：

```powershell
git --version
cargo --version
```

### 2. 拉代码并编译

```powershell
git clone <your-repo-url> ClawParty2.0
cd ClawParty2.0

cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
```

编译完成后的二进制路径是：

```powershell
.\target\release\partyclaw.exe
```

### 3. 准备配置和密钥

```powershell
Copy-Item .\deploy_telegram.json .\deploy_prod.json
New-Item -ItemType Directory -Force .\deploy_workdir
Copy-Item .\.env.example .\.env

notepad .\.env
```

至少填好：

```dotenv
OPENROUTER_API_KEY=...
TELEGRAM_BOT_TOKEN=...
```

打开配置 TUI：

```powershell
.\target\release\partyclaw.exe config .\deploy_prod.json
```

在 `Sandbox` 页面选择：

```text
subprocess
```

### 4. 前台试运行

```powershell
.\target\release\partyclaw.exe --config .\deploy_prod.json --workdir .\deploy_workdir --sandbox-auto
```

确认 Telegram 或 CLI channel 能正常响应后，再做服务化部署。

### 5. 任务计划程序部署

Windows 原生没有使用 `partyclaw setup`。下面用系统自带的任务计划程序，在当前用户登录时启动。

先在仓库根目录创建启动脚本：

```powershell
@'
Set-Location $PSScriptRoot
New-Item -ItemType Directory -Force .\deploy_workdir | Out-Null
.\target\release\partyclaw.exe --config .\deploy_prod.json --workdir .\deploy_workdir --sandbox-auto *> .\deploy_workdir\partyclaw.log
'@ | Set-Content -Encoding UTF8 .\run_partyclaw.ps1
```

注册并启动任务：

```powershell
$TaskName = "ClawParty\mybot"
$Script = (Resolve-Path .\run_partyclaw.ps1).Path
$Action = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$Script`""

schtasks /Create /TN $TaskName /TR $Action /SC ONLOGON /F
schtasks /Run /TN $TaskName
schtasks /Query /TN $TaskName /V /FO LIST
```

查看日志：

```powershell
Get-Content .\deploy_workdir\partyclaw.log -Tail 100
Get-Content .\deploy_workdir\partyclaw.log -Wait
```

停止任务：

```powershell
schtasks /End /TN "ClawParty\mybot"
```

删除任务：

```powershell
schtasks /Delete /TN "ClawParty\mybot" /F
```

如果需要“开机后未登录也运行”的真正 Windows Service，建议用 NSSM 或 WinSW 包装同一条前台启动命令。

## 更新部署

代码更新后，一般只需要重新编译并重启服务。

Linux：

```bash
cd ClawParty2.0
git pull --ff-only
cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
systemctl --user restart mybot.service
systemctl --user status mybot.service --no-pager
```

macOS：

```bash
cd ClawParty2.0
git pull --ff-only
cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
launchctl kickstart -k "gui/$(id -u)/com.clawparty.mybot"
```

Windows：

```powershell
cd ClawParty2.0
git pull --ff-only
cargo build --release --manifest-path agent_host/Cargo.toml --bin partyclaw
schtasks /End /TN "ClawParty\mybot"
schtasks /Run /TN "ClawParty\mybot"
schtasks /Query /TN "ClawParty\mybot" /V /FO LIST
```

如果这些路径没有变化，通常不需要重新执行 `setup` 或重写 plist：

- `partyclaw` 二进制路径
- 配置文件路径
- `deploy_workdir` 路径
- `.env` 路径

如果移动了仓库、改了配置文件名、改了 workdir 路径，Linux 重新执行：

```bash
./target/release/partyclaw setup ./deploy_prod.json ./deploy_workdir mybot
systemctl --user restart mybot.service
```

macOS 重新生成并加载 plist。

Windows 重新生成 `run_partyclaw.ps1` 并重新创建任务计划程序任务。

## 修改配置或密钥后生效

修改配置：

```bash
./target/release/partyclaw config ./deploy_prod.json
```

Windows：

```powershell
.\target\release\partyclaw.exe config .\deploy_prod.json
```

Linux 重启：

```bash
systemctl --user restart mybot.service
```

macOS 重启：

```bash
launchctl kickstart -k "gui/$(id -u)/com.clawparty.mybot"
```

Windows 重启：

```powershell
schtasks /End /TN "ClawParty\mybot"
schtasks /Run /TN "ClawParty\mybot"
```

修改 `.env` 后也需要重启服务。`.env` 的路径变了，则需要重新执行 Linux `setup`、重新生成 macOS plist，或重新生成 Windows 启动脚本。

## 可选：构建 zgent-server

只有启用 zgent 原生 kernel 路径时才需要额外构建 `./zgent` 里的 `zgent-server`。

Linux：

```bash
./scripts/build_zgent_server.sh
```

如果依赖已经装好，只编译：

```bash
./scripts/build_zgent_server.sh --skip-install
```

macOS：

```bash
cargo build --manifest-path zgent/Cargo.toml --bin zgent-server
```

Windows：

```powershell
cargo build --manifest-path zgent\Cargo.toml --bin zgent-server
```

## 故障排查

检查配置能否打开：

```bash
./target/release/partyclaw config ./deploy_prod.json
```

前台启动看直接错误：

```bash
./target/release/partyclaw --config ./deploy_prod.json --workdir ./deploy_workdir --sandbox-auto
```

Windows：

```powershell
.\target\release\partyclaw.exe --config .\deploy_prod.json --workdir .\deploy_workdir --sandbox-auto
```

Linux 服务状态：

```bash
systemctl --user status mybot.service --no-pager
journalctl --user -u mybot.service -n 200 --no-pager
```

如果 Linux user service 反复 auto-restart，并且日志出现：

```text
Changing group credentials failed: Operation not permitted
status=216/GROUP
```

检查是否有旧的 docker group drop-in：

```bash
systemctl --user cat mybot.service
ls ~/.config/systemd/user/mybot.service.d
```

如果看到 `SupplementaryGroups=docker`，请移走该 drop-in 并重载：

```bash
mv ~/.config/systemd/user/mybot.service.d/docker.conf \
  ~/.config/systemd/user/mybot.service.d/docker.conf.disabled
systemctl --user daemon-reload
systemctl --user reset-failed mybot.service
systemctl --user restart mybot.service
```

如果该 drop-in 目录是 root 创建的，`mv` 可能需要加 `sudo`。

macOS 服务状态：

```bash
launchctl print "gui/$(id -u)/com.clawparty.mybot"
tail -n 200 deploy_workdir/launchd/stderr.log
```

Windows 服务状态：

```powershell
schtasks /Query /TN "ClawParty\mybot" /V /FO LIST
Get-Content .\deploy_workdir\partyclaw.log -Tail 200
```

Telegram 没响应时优先检查：

- `.env` 是否包含 `TELEGRAM_BOT_TOKEN=...`
- `deploy_prod.json` 里的 `bot_token_env` 是否是 `TELEGRAM_BOT_TOKEN`
- bot 是否被加入目标群聊
- 修改 `.env` 或配置后是否已经重启服务
- 当前沙盒是否适合系统：Linux 可用 `bubblewrap`，macOS / Windows 使用 `subprocess`
