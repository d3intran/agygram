# 🚀 agygram

[English Documentation](README_EN.md) | 简体中文文档

> **Next-Generation Antigravity CLI Telegram Daemon**
> 
> 专为 **Google Antigravity CLI (`agy`)** 打造的高性能 Telegram 守护进程。基于 Rust 2024 与 Tokio 异步运行时，提供微秒级解耦的流式打字机输出、本地 Brain 会话接续、多级原地交互控制板、产物直传与高保真语音回复能力。

---

## ✨ 核心亮点 (Highlights)

- ⚡ **Antigravity CLI 原生结构化流 (`stream-json`)**：
  - 双协程解耦架构（内存级 Ingest + 独立 Ticker 渲染），彻底消除对 CLI 子进程管道的反压。
  - 增量 HTML 标签平衡器（Streaming Tag Balancer），自动闭合未完成的 `<pre><code>`、`<blockquote>`、`<b>`、`<i>` 标签，杜绝 Telegram 400 格式解析错误。
- 🧠 **Antigravity 本地 Brain 会话深度联动**：
  - 📜 **`/history` 历史会话一键接续**：动态扫描 `~/.gemini/antigravity/brain/`，列出活跃会话及 Planning/Walkthrough 产物标记，轻触按钮即可接续上下文。
  - 📋 **`/plan` 方案查看**：直接调阅当前会话的 `implementation_plan.md` 架构设计。
  - 📝 **`/walkthrough` 报告调阅**：直接调阅当前会话的任务总结与验证报告 `walkthrough.md`。
  - 🧩 **`/skills` 与 🛡️ `/rules` 巡检**：动态查看生效的 Antigravity 技能库与工作区规则。
- 🎛️ **原地无缝多级交互控制板 (In-Place Interactive Control Panel)**：
  - `/status` 呈现完整会话状态卡片，支持模型选择、思考力度 (Effort)、模式 (Mode)、语音开关原地切换，附带 `« 返回控制板` 导航，杜绝刷屏。
- 📊 **Markdown 表格等宽 ASCII 优雅排版**：
  - 自动识别 Markdown 表格并转换为移动端等宽 `<pre><code>` 框线排版，告别手机端折行错位。
- 🖼️ **生成产物文件自动直传 (Auto-Delivery)**：
  - AI 生成图片 (`.png`/`.jpg`/`.webp`) 或文档 (`.pdf`/`.json`/`.csv`) 时，守护进程自动捕获并推送给 Telegram 用户。
- 🤖 **全系顶级模型支持与极简别名**：
  - 支持 `Gemini 3.7 Flash (High/Med/Low)`、`Gemini 3.6 Flash`、`Gemini 3.1 Pro`、`Claude Sonnet 4.6`、`Claude Opus 4.6 Thinking`、`GPT-OSS 120B`。
  - 支持极简指令切换，如 `/model 3.7`、`/model sonnet`、`/model pro`。
- 🔊 **申鹤语音回复与极速 TTS (Fish Audio)**：
  - 支持 `/voice on`、`/voice off`、`/voice toggle` 与 `/voice <提问>`，并行异步合成语音，不阻塞流式文本。
- 🛑 **无损优雅中断与多工作目录绑定**：
  - 发送新消息自动平滑中断上一次任务（`SIGINT` 先行保障 Brain 会话日志完整，超时升级 `SIGKILL`）；支持 `/cd` 切换主目录及 `/adddir` 多工程附加绑定。

---

## 🏗️ 架构概览 (Architecture)

```mermaid
flowchart TD
    User([Telegram User]) <-->|HTTPS / MTProto / Proxy| TelegramServer[Telegram Bot Platform]
    TelegramServer <-->|teloxide Long-Polling| Daemon[agygram Daemon]

    subgraph agygram Core
        BotDispatcher[Bot Dispatcher & Commands]
        Pipeline[Decoupled Streaming Pipeline]
        SessionMgr[Session Manager & Atomic JSON Store]
        Formatter[HTML Tag Balancer & Table Formatter]
        Executor[AgyExecutor Subprocess Engine]
        TTS[Fish Audio TTS Engine]
    end

    Daemon <--> BotDispatcher
    BotDispatcher <--> Pipeline
    Pipeline <--> SessionMgr
    Pipeline <--> Formatter
    Pipeline <--> Executor
    Pipeline -.->|Async Parallel| TTS

    Executor <-->|tokio::process stream-json| AgyCLI[Antigravity CLI (agy)]
    AgyCLI <--> BrainStore[(~/.gemini/antigravity/brain)]
```

---

## 🚀 快速上手 (Quick Start)

### 1. 环境准备
- 确保系统已安装并配置好 **Google Antigravity CLI (`agy`)**（运行 `agy --version` 确认可用）。
- 安装 Rust 工具链（若自行编译）：
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

### 2. 获取并编译 agygram
```bash
git clone https://github.com/d3intran/agygram.git
cd agygram
cargo build --release
```
编译产物位于 `target/release/agygram`（体积仅 ~5.6MB，经过 LTO 与符号剥离极致优化）。

### 3. 配置 Telegram Bot
一键生成配置模板目录与文件：
```bash
mkdir -p ~/.config/agygram
cp config.example.toml ~/.config/agygram/config.toml
```

编辑 `~/.config/agygram/config.toml`：
```toml
# 必填：来自 @BotFather 的 Telegram Bot Token
bot_token = "1234567890:ABCdefGhIJKlmNoPQRsTUVwxyZ"

# 必填：授权操作的 Telegram User ID（可从 @userinfobot 获取，支持多用户）
allowed_user_ids = [123456789]

# 默认工作主目录与模型
default_workspace = "~"
default_model = ""
default_effort = "high"
default_mode = "accept-edits"

# 打字机流式刷新最小间隔（毫秒，建议 1000）
draft_update_interval_ms = 1000

# 代理设置（国内 VPS 或网络受限时配置，支持 HTTP / SOCKS5）
# proxy_url = "socks5://127.0.0.1:1080"

# Fish Audio 语音回复配置（可选，开启申鹤语音）
# fish_api_key = "YOUR_FISH_AUDIO_API_KEY"
# fish_reference_id = "YOUR_VOICE_REFERENCE_ID"
```

### 4. 验证环境与启动运行
```bash
# 检查配置文件与 agy CLI 路径有效性
./target/release/agygram check

# 前台启动守护进程
./target/release/agygram daemon
```

---

## 🛠️ 生产环境部署 (Systemd Service)

将 `agygram` 注册为 Linux 系统守护服务，实现开机自启与崩溃自动拉起：

1. **创建服务单元文件**：
   ```bash
   sudo nano /etc/systemd/system/agygram.service
   ```

2. **写入以下配置**（根据实际用户与路径替换）：
   ```ini
   [Unit]
   Description=agygram Telegram Bot Daemon Service
   After=network.target network-online.target
   Wants=network-online.target

   [Service]
   Type=simple
   User=azureuser
   WorkingDirectory=/home/azureuser
   ExecStart=/home/azureuser/agygram daemon
   Restart=always
   RestartSec=5s
   Environment=PATH=/usr/local/bin:/usr/bin:/bin:/home/azureuser/.local/bin:/home/azureuser/.cargo/bin
   Environment=HOME=/home/azureuser

   [Install]
   WantedBy=multi-user.target
   ```

3. **启动并启用自启**：
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable agygram
   sudo systemctl start agygram
   sudo systemctl status agygram
   ```

---

## 📋 指令速查表 (Command Reference)

| 指令 | 作用说明 | 使用示例 |
| :--- | :--- | :--- |
| `/start` / `/help` | 显示完整帮助菜单与指令列表 | `/help` |
| `/new` / `/reset` / `/clear` | 重置会话上下文与统计，开启全新会话 | `/new` 或 `/reset` |
| `/resume [<ID>]` | 恢复/接续历史对话（缺省参数自动接续最近上下文） | `/resume` 或 `/resume 8de907d6` |
| `/history` | 查看 VPS 活跃历史会话列表，支持一键内联点击接续 | `/history` |
| `/plan` | 查看当前会话的架构设计方案 (`implementation_plan.md`) | `/plan` |
| `/walkthrough` | 查看当前会话的任务验证与执行总结 (`walkthrough.md`) | `/walkthrough` |
| `/skills` | 巡检并列出已挂载的 Antigravity 技能库 (Skills) | `/skills` |
| `/rules` | 查看当前生效的全局与项目规则 (Rules) | `/rules` |
| `/status` | 打开多级原地交互控制卡片（模型、Effort、Mode 等） | `/status` |
| `/usage` / `/quota` | 查看 Antigravity CLI Token 详细额度与遥测面板 | `/quota` |
| `/cd <path>` | 切换主工作目录（支持 `~/...` 简写） | `/cd ~/projects/my-app` |
| `/adddir <path>` | 添加附加项目工作目录（对应 `--add-dir`） | `/adddir ~/libs/shared` |
| `/cleardirs` | 清空所有附加的项目工作目录 | `/cleardirs` |
| `/dirs` / `/pwd` | 查看当前绑定的所有工作目录路径 | `/pwd` |
| `/model [<name>]` | 切换 AI 模型（支持别名 `3.7`, `sonnet`, `pro` 或内联选择） | `/model 3.7` |
| `/models` | 列出系统支持的全部 AI 模型列表 | `/models` |
| `/mcp` | 查看已配置的 MCP 服务器列表 | `/mcp` |
| `/plugins` | 查看已安装的 agy 插件列表 | `/plugins` |
| `/changelog` | 查看 agy CLI 最新更新日志 | `/changelog` |
| `/effort [<level>]` | 设置思考力度（`low`, `medium`, `high`） | `/effort high` |
| `/mode [<mode>]` | 切换执行模式（`accept-edits`, `plan`） | `/mode accept-edits` |
| `/agent [<name>]` | 切换指定的 Agent 智能体 | `/agent code-reviewer` |
| `/agents` | 列出系统可用的全部 Agent 列表 | `/agents` |
| `/system [<prompt>]`| 查看、设置或清空自定义 System Prompt | `/system 请用中文回复` |
| `/file <path>` | 将 VPS 上的任何文件发送给 Telegram 用户 | `/file ~/.config/agygram/config.toml` |
| `/voice [on\|off\|toggle\|<text>]` | 申鹤语音回复控制或单次语音提问 | `/voice on` 或 `/voice 介绍一下你自己` |
| `/cancel` | 紧急平滑终止当前运行中的 `agy` 任务并释放锁 | `/cancel` |

---

## ❓ 常见问题 (FAQ & Troubleshooting)

<details>
<summary><b>Q1: 启动时提示 "未在授权名单中"？</b></summary>
Telegram 采用用户 ID 白名单保护 VPS 安全。请通过 Telegram 搜索 <code>@userinfobot</code> 获取自己的纯数字 User ID，填入 <code>~/.config/agygram/config.toml</code> 的 <code>allowed_user_ids = [你的ID]</code> 中并重启服务。
</details>

<details>
<summary><b>Q2: 提示 "找不到 agy CLI"？</b></summary>
请确认 <code>agy</code> 二进制文件已安装在 <code>~/.local/bin/agy</code> 或 <code>/usr/local/bin/agy</code> 等系统 PATH 路径下。运行 <code>which agy</code> 或 <code>agy --version</code> 进行确认。若使用 systemd，请确保 Unit 文件中的 <code>Environment=PATH=...</code> 包含 agy 所在的目录。
</details>

<details>
<summary><b>Q3: 国内服务器如何配置代理连接 Telegram？</b></summary>
在 <code>~/.config/agygram/config.toml</code> 中取消注释 <code>proxy_url</code> 并填写代理地址，例如：<code>proxy_url = "socks5://127.0.0.1:1080"</code> 或 <code>proxy_url = "http://127.0.0.1:7890"</code>。
</details>

---

## 📜 许可证 (License)

本项目基于 **MIT 许可证** 开源。遵循 Google Antigravity CLI 原生规范与高性能 Rust 代码实践。

