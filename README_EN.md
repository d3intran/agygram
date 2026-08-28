# 🚀 agygram

English | [简体中文文档](README.md)

> **Next-Generation Antigravity CLI Telegram Daemon**
> 
> A high-performance Telegram daemon specifically engineered for the **Google Antigravity CLI (`agy`)**. Built with Rust 2024 and Tokio asynchronous runtime, offering decoupled typewriter streaming output, native Brain session continuity, in-place interactive menus, automated artifact file delivery, and high-fidelity TTS voice replies.

---

## ✨ Key Highlights

- ⚡ **Antigravity CLI Native Structured Stream (`stream-json`)**:
  - Decoupled two-task concurrency model (microsecond-speed memory Ingest + independent Ticker renderer), completely eliminating process pipe backpressure against the CLI engine.
  - Streaming HTML Tag Balancer automatically completes unclosed `<pre><code>`, `<blockquote>`, `<b>`, and `<i>` tags during real-time streaming to prevent Telegram 400 entity parsing errors.
- 🧠 **Deep Integration with Antigravity Brain Storage**:
  - 📜 **`/history` One-Click Session Resume**: Dynamically scans `~/.gemini/antigravity/brain/` for active sessions with planning/walkthrough artifact indicators, allowing instant inline button resumption.
  - 📋 **`/plan` Implementation Plan Viewer**: Directly inspects the current session's `implementation_plan.md` architectural design.
  - 📝 **`/walkthrough` Execution Summary**: Directly inspects the current session's `walkthrough.md` validation report.
  - 🧩 **`/skills` & 🛡️ `/rules` Ecosystem Inspector**: Dynamically inspects mounted Antigravity skills and workspace rules.
- 🎛️ **In-Place Seamless Interactive Control Panel**:
  - `/status` renders a complete configuration card with inline keyboards for model switching, effort levels, execution modes, and voice toggling with `« Back to Dashboard` navigation, preventing chat clutter.
- 📊 **Monospace ASCII Markdown Table Rendering**:
  - Automatically identifies Markdown tables and converts them into mobile-friendly `<pre><code>` ASCII-bordered code blocks, eliminating broken lines on mobile devices.
- 🖼️ **Automated File & Image Delivery (Auto-Delivery)**:
  - When AI generates images (`.png`/`.jpg`/`.webp`) or documents (`.pdf`/`.json`/`.csv`), the daemon detects and sends them directly via `send_photo`/`send_document`.
- 🤖 **Comprehensive Top-Tier Models & Short Aliases**:
  - Out-of-the-box support for `Gemini 3.7 Flash (High/Med/Low)`, `Gemini 3.6 Flash`, `Gemini 3.1 Pro`, `Claude Sonnet 4.6`, `Claude Opus 4.6 Thinking`, and `GPT-OSS 120B`.
  - Supports handy shortcuts like `/model 3.7`, `/model sonnet`, `/model pro`, `/model opus`.
- 🔊 **Shenhe TTS Voice Reply (Fish Audio Integration)**:
  - Supports `/voice on`, `/voice off`, `/voice toggle`, and `/voice <prompt>`, generating high-quality character voice responses in parallel without blocking streaming text.
- 🛑 **Graceful Process Cancellation & Multi-Directory Binding**:
  - Sending a new message automatically and gracefully terminates previous in-flight inference tasks (`SIGINT` first to ensure Brain log integrity, escalating to `SIGKILL`); supports `/cd` primary directory switching and `/adddir` multi-project attachments.

---

## 🏗️ Architecture Overview

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

## 🚀 Quick Start Guide

### 1. Prerequisites
- Ensure **Google Antigravity CLI (`agy`)** is installed and accessible in system PATH (`agy --version`).
- Install Rust toolchain (if building from source):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

### 2. Clone & Build
```bash
git clone https://github.com/d3intran/agygram.git
cd agygram
cargo build --release
```
The compiled binary is located at `target/release/agygram` (optimized to ~5.6MB with Fat LTO and symbol stripping).

### 3. Configure Telegram Bot
Generate the configuration directory and template:
```bash
mkdir -p ~/.config/agygram
cp config.example.toml ~/.config/agygram/config.toml
```

Edit `~/.config/agygram/config.toml`:
```toml
# Required: Telegram Bot Token from @BotFather
bot_token = "1234567890:ABCdefGhIJKlmNoPQRsTUVwxyZ"

# Required: Whitelisted Telegram User IDs (query via @userinfobot)
allowed_user_ids = [123456789]

# Default workspace directory and model parameters
default_workspace = "~"
default_model = ""
default_effort = "high"
default_mode = "accept-edits"

# Streaming typewriter refresh interval in milliseconds (default: 1000)
draft_update_interval_ms = 1000

# Proxy settings (optional, HTTP / SOCKS5 supported)
# proxy_url = "socks5://127.0.0.1:1080"

# Fish Audio TTS Configuration (optional)
# fish_api_key = "YOUR_FISH_AUDIO_API_KEY"
# fish_reference_id = "YOUR_VOICE_REFERENCE_ID"
```

### 4. Check Environment & Launch
```bash
# Verify configuration and agy CLI availability
./target/release/agygram check

# Run daemon in foreground
./target/release/agygram daemon
```

---

## 🛠️ Production Deployment (Systemd Service)

Register `agygram` as a systemd background service with auto-restart on boot:

1. **Create systemd unit file**:
   ```bash
   sudo nano /etc/systemd/system/agygram.service
   ```

2. **Add the following configuration** (replace username and paths accordingly):
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

3. **Start and enable the service**:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable agygram
   sudo systemctl start agygram
   sudo systemctl status agygram
   ```

---

## 📋 Command Reference Table

| Command | Description | Example |
| :--- | :--- | :--- |
| `/start` / `/help` | Show full help menu and command list | `/help` |
| `/new` / `/reset` / `/clear` | Reset conversation context and start fresh | `/new` or `/reset` |
| `/resume [<ID>]` | Resume past session (empty arg continues latest context) | `/resume` or `/resume 8de907d6` |
| `/history` | List active VPS history sessions with one-click resume buttons | `/history` |
| `/plan` | View current session's architecture plan (`implementation_plan.md`) | `/plan` |
| `/walkthrough` | View current session's validation walkthrough (`walkthrough.md`) | `/walkthrough` |
| `/skills` | Inspect mounted Antigravity Skills | `/skills` |
| `/rules` | View active global and workspace rules | `/rules` |
| `/status` | Open multi-level in-place interactive control dashboard | `/status` |
| `/usage` / `/quota` | View Antigravity CLI Token usage and telemetry dashboard | `/quota` |
| `/cd <path>` | Switch primary workspace directory (supports `~/...`) | `/cd ~/projects/my-app` |
| `/adddir <path>` | Add auxiliary workspace directory (`--add-dir`) | `/adddir ~/libs/shared` |
| `/cleardirs` | Clear all attached auxiliary directories | `/cleardirs` |
| `/dirs` / `/pwd` | View all currently bound workspace directories | `/pwd` |
| `/model [<name>]` | Switch AI model (supports aliases like `3.7`, `sonnet`, `pro`) | `/model 3.7` |
| `/models` | List all supported AI models | `/models` |
| `/mcp` | View configured Model Context Protocol (MCP) servers | `/mcp` |
| `/plugins` | View installed agy plugins | `/plugins` |
| `/changelog` | View agy CLI changelog | `/changelog` |
| `/effort [<level>]` | Set thinking effort level (`low`, `medium`, `high`) | `/effort high` |
| `/mode [<mode>]` | Switch execution mode (`accept-edits`, `plan`) | `/mode accept-edits` |
| `/agent [<name>]` | Switch active Agent identity | `/agent code-reviewer` |
| `/agents` | List all available system Agents | `/agents` |
| `/system [<prompt>]`| View, set, or clear custom System Prompt | `/system Respond in English` |
| `/file <path>` | Transmit any VPS file directly to Telegram | `/file ~/.config/agygram/config.toml` |
| `/voice [on\|off\|toggle\|<text>]` | Control Shenhe voice replies or submit one-shot voice query | `/voice on` or `/voice introduce yourself` |
| `/cancel` | Gracefully terminate active `agy` task and release lock | `/cancel` |

---

## ❓ FAQ & Troubleshooting

<details>
<summary><b>Q1: "Permission Denied: User ID not allowed" on start?</b></summary>
Telegram uses a strict numeric user ID whitelist for VPS security. Search for <code>@userinfobot</code> in Telegram to obtain your numeric ID, add it to <code>allowed_user_ids = [YOUR_ID]</code> in <code>~/.config/agygram/config.toml</code>, and restart the service.
</details>

<details>
<summary><b>Q2: "Failed to find agy CLI binary"?</b></summary>
Ensure the <code>agy</code> binary is installed under a standard PATH directory such as <code>~/.local/bin/agy</code> or <code>/usr/local/bin/agy</code>. Verify with <code>which agy</code> or <code>agy --version</code>. If using systemd, ensure <code>Environment=PATH=...</code> in the unit file includes the directory containing agy.
</details>

<details>
<summary><b>Q3: How to configure a proxy for Telegram connectivity?</b></summary>
Uncomment <code>proxy_url</code> in <code>~/.config/agygram/config.toml</code> and specify your proxy endpoint, e.g., <code>proxy_url = "socks5://127.0.0.1:1080"</code> or <code>proxy_url = "http://127.0.0.1:7890"</code>.
</details>

---

## 📜 License

This project is licensed under the **MIT License**. Compliant with Google Antigravity CLI native specifications and high-performance Rust best practices.
