<div align="center">

# 🚀 agygram

**Turn your Telegram into a high-performance, autonomous command center for Google Antigravity CLI.**

[![Release: v1.4.0](https://img.shields.io/badge/release-v1.4.0-blue.svg?style=flat-square)](https://github.com/d3intran/agygram/releases)
[![Rust 2021](https://img.shields.io/badge/rust-2021%20edition-orange.svg?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![Antigravity CLI](https://img.shields.io/badge/antigravity%20cli-v2.0+-blue.svg?style=flat-square&logo=google)](https://github.com/d3intran/agygram)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-lightgrey.svg?style=flat-square)](#)

[English](README.md) • [简体中文](README_ZH.md)

<br/>

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Telegram App                    agygram Daemon (v1.4.0)                 │
│                                                                         │
│ 🧑 User: "Refactor API & timer" ──> ⚡ Ingest Event Queue (0µs lag)       │
│                                     │                                   │
│ 🤖 agy: <blockquote expandable> <── ✍️ Multi-Message Rollover Stream     │
│ 🤖 agy: <code>code...</code>        ├── 🧠 Brain Continuity (~/brain)    │
│ ⏰ [Timer Alert Pushed!]        <── ├── 🔔 Proactive Notification Daemon │
│ 🖼️ [Image.png auto-delivered]   <── ├── 📦 Conconfined Asset Delivery   │
│ 🎙️ [Shenhe Voice Voice Note]   <── └── 🔊 Fish Audio TTS (Parallel)     │
└─────────────────────────────────────────────────────────────────────────┘
```

</div>

---

## 💡 Why agygram?

**Google Antigravity CLI (`agy`)** is one of the most capable agentic coding CLIs available. However, developers often work across devices—on mobile while commuting, checking server deployments from a laptop, or wanting quick task triggers without keeping an SSH terminal open.

**agygram** solves this by providing a zero-latency, enterprise-grade Telegram daemon tailored specifically for `agy`:

* **Proactive Async Push (v1.4.0)**: Background timers (`schedule`), asynchronous task completions, and Subagent returns are automatically pushed to Telegram in real time—no need to send another prompt to wake the bot.
* **Continuous Multi-Message Streaming (v1.4.0)**: Exceeds Telegram's 4,096-character limit! Long code generations and architectural plans seamlessly rollover into new messages and continue streaming live.
* **Autonomous Mode (`auto`) (v1.4.0)**: Fully supports `auto` mode alongside `accept-edits` and `plan` for self-directed agent decision-making.
* **Foldable Reasoning & Command Cards (v1.4.0)**: Leverages Telegram Bot API 7.2+ `<blockquote expandable>` to neatly fold lengthy thinking tokens and command logs, keeping mobile views clean and readable.
* **No Telegram Entity Errors**: Incremental streaming HTML balancer auto-completes unclosed tags (`<pre>`, `<code>`, `<blockquote>`) on every frame.
* **Deep Local Brain Continuity**: Inspects `~/.gemini/antigravity-cli/brain/` to resume sessions (`/history`), inspect architecture plans (`/plan`), and read execution reports (`/walkthrough`).
* **Automatic File & Asset Delivery**: Generated mockups, charts (`.png`), PDFs, or JSON data files within the workspace are automatically uploaded as native Telegram attachments.

---

## ✨ Features at a Glance

| Feature | Description |
| :--- | :--- |
| 🔔 **Proactive Notifications** *(New in 1.4.0)* | Daemon actively monitors the Brain message queue. Background tasks and `schedule` timers trigger instant Telegram notifications without manual prompting. |
| 🚀 **Continuous Multi-Message Streaming** *(New in 1.4.0)* | When a response exceeds 4,000 characters, agygram cleanly seals the first message and streams into a new rollover message in real time. |
| 🎯 **Full `auto` Mode Support** *(New in 1.4.0)* | Native integration with `auto` (autonomous execution), `accept-edits` (direct apply), and `plan` (architectural outline only). |
| 🧠 **Expandable Thought & Tool Blocks** *(New in 1.4.0)* | Deep thinking chains and verbose terminal outputs fold inside native `<blockquote expandable>` blocks. |
| 📐 **LaTeX Math Support** *(New in 1.4.0)* | Converts inline `$x$` and block `$$...$$` math notation into clean, render-safe Telegram code blocks. |
| 🛡️ **Workspace Auto-Recovery** *(New in 1.4.0)* | Safely resolves `~` home directory paths and auto-recovers to `$HOME` if a configured path is missing, avoiding OS error 2. |
| ⚡ **Typewriter Streaming** | Real-time `stream-json` event parsing with memory ingestion and adaptive 429 rate-limiting backoff. |
| 🎛️ **In-Place Control Panel** | `/status` provides a unified interactive card for model, effort, mode, and voice controls with back navigation. |
| 📊 **Monospace ASCII Tables** | Converts Markdown tables into mobile-friendly ASCII box `<pre><code>` blocks that never wrap awkwardly on phones. |
| 🖼️ **Artifact Auto-Delivery** | Auto-detects generated images (`.png`/`.jpg`) and documents (`.pdf`/`.json`/`.csv`) inside the bound workspace and sends them as native attachments. |
| 🤖 **Top-Tier Model Matrix** | Seamlessly switch between `Gemini 3.8 Flash` (High/Med/Low), `Claude Sonnet 4.6`, `Claude Opus 4.6`, and `GPT-OSS 120B`. |
| 🔊 **Shenhe TTS Voice Engine** | Asynchronous parallel speech generation powered by Fish Audio with character voice modeling. |
| 🛑 **Graceful Process Interruption** | New messages gracefully cancel in-flight tasks (`SIGINT` first to preserve Brain logs, then `SIGKILL`). |
| 🛡️ **Hardened Security** | Strict numeric User ID whitelist and atomic JSON session persistence with crash resilience. |

---

## 🏗️ Architecture

```mermaid
flowchart TD
    User(["📱 Telegram User"]) <-->|HTTPS / Long-Polling| BotAPI["Telegram Bot Platform"]
    BotAPI <-->|teloxide Engine| Daemon["agygram Daemon"]

    subgraph agygram_Core ["agygram Core Engine (v1.4.0)"]
        Dispatcher["Bot Dispatcher & Router"]
        Pipeline["Multi-Message Streaming Pipeline"]
        SessionMgr["Session Manager & Atomic JSON"]
        Formatter["Expandable HTML Balancer & Tables"]
        Executor["AgyExecutor (tokio::process)"]
        Notifier["Proactive Notification Daemon"]
        TTS["Fish Audio TTS Engine"]
    end

    Daemon <--> Dispatcher
    Dispatcher <--> Pipeline
    Pipeline <--> SessionMgr
    Pipeline <--> Formatter
    Pipeline <--> Executor
    Pipeline -.->|Async Spawn| TTS
    Notifier <--> SessionMgr
    Notifier -.->|Proactive Alert| BotAPI

    Executor <-->|"stream-json (stdin/stdout)"| AgyCLI["Antigravity CLI (agy)"]
    AgyCLI <--> BrainStore[("Local Brain Store (~/.gemini/antigravity-cli/brain)")]
    Notifier <--> BrainStore
```

---

## 🚀 Quick Start in 60 Seconds

### Step 1: Prerequisites
- Ensure **Google Antigravity CLI (`agy`)** is installed and running (`agy --version`).
- Install Rust toolchain (if compiling from source):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

### Step 2: Clone & Build
```bash
git clone https://github.com/d3intran/agygram.git
cd agygram
cargo build --release
```
The optimized release binary is located at `target/release/agygram` (~6.0MB with Fat LTO and symbol stripping).

### Step 3: Interactive Configuration Wizard
Run the interactive onboarding wizard to configure your bot token, allowed user IDs, and workspace in seconds:
```bash
./target/release/agygram init
```
*(Alternatively, copy `config.example.toml` manually to `~/.config/agygram/config.toml`)*

### Step 4: Verify & Launch
```bash
# Verify configuration and CLI path
./target/release/agygram check

# Start daemon in foreground
./target/release/agygram daemon
```

---

## 🛠️ Production Deployment (Systemd Service)

To run `agygram` 24/7 in the background with auto-restart on system reboot:

1. **Create systemd unit file**:
   ```bash
   sudo nano /etc/systemd/system/agygram.service
   ```

2. **Paste configuration** (replace `azureuser` and paths with your setup):
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

3. **Enable and start**:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable agygram
   sudo systemctl start agygram
   sudo systemctl status agygram
   ```

---

## 📋 Command Reference

### 🔄 Session & Control
| Command | Description | Example |
| :--- | :--- | :--- |
| `/status` | Open multi-level in-place interactive control dashboard | `/status` |
| `/new` / `/reset` / `/clear` | Reset conversation context and start fresh | `/new` |
| `/resume [<ID>]` | Resume past session (empty arg continues latest context) | `/resume` or `/resume 8de907d6` |
| `/cancel` | Gracefully terminate active `agy` task and release lock | `/cancel` |
| `/usage` / `/quota` | View Antigravity CLI Token usage and telemetry dashboard | `/quota` |

### 🧠 Antigravity Brain & Ecosystem
| Command | Description | Example |
| :--- | :--- | :--- |
| `/history` | List active VPS history sessions with one-click resume buttons | `/history` |
| `/plan` | View current session's architecture plan (`implementation_plan.md`) | `/plan` |
| `/walkthrough` | View current session's validation report (`walkthrough.md`) | `/walkthrough` |
| `/skills` | Inspect active Antigravity Skills library | `/skills` |
| `/rules` | View active global and workspace rules | `/rules` |
| `/mcp` | View and manage Model Context Protocol (MCP) servers | `/mcp` |
| `/plugin` / `/plugins` | Inspect and manage installed agy plugins | `/plugins` |
| `/changelog` | View agy CLI latest changelog | `/changelog` |

### 📂 Workspace & Files
| Command | Description | Example |
| :--- | :--- | :--- |
| `/cd <path>` | Switch primary workspace directory (supports `~/...`) | `/cd ~/projects/my-app` |
| `/adddir <path>` | Add auxiliary workspace directory (`--add-dir`) | `/adddir ~/libs/shared` |
| `/cleardirs` | Clear all attached auxiliary directories | `/cleardirs` |
| `/dirs` / `/pwd` | View all currently bound workspace directories | `/pwd` |
| `/file <path>` | Transmit any VPS file directly to Telegram | `/file ~/.config/agygram/config.toml` |

### 🤖 Model & Execution Settings
| Command | Description | Example |
| :--- | :--- | :--- |
| `/model [<name>]` | Switch AI model (supports aliases: `3.8`, `sonnet`, `opus`, `oss`) | `/model 3.8` |
| `/models` | List all supported AI models | `/models` |
| `/effort [<level>]` | Set thinking effort level (`low`, `medium`, `high`, `xhigh`, `max`) | `/effort high` |
| `/mode [<mode>]` | Switch execution mode (`auto`, `accept-edits`, `plan`) | `/mode auto` |
| `/agent [<name>]` | Switch active Agent identity | `/agent code-reviewer` |
| `/agents` | List all available system Agents | `/agents` |
| `/system [<prompt>]`| View, set, or clear custom System Prompt | `/system Respond in English` |
| `/voice [on\|off\|toggle\|<text>]` | Control Shenhe voice replies or submit one-shot voice query | `/voice on` or `/voice introduce yourself` |

---

## ❓ FAQ & Troubleshooting

<details>
<summary><b>Q1: "Permission Denied: User ID not allowed"?</b></summary>
Telegram enforces a strict numeric user ID whitelist for server security. Open Telegram and search for <code>@userinfobot</code> to retrieve your ID, then add it to <code>allowed_user_ids = [YOUR_ID]</code> in <code>~/.config/agygram/config.toml</code> and restart the daemon. An empty whitelist now denies every request; serving arbitrary users requires the explicit <code>allow_all_users = true</code> opt-in, which is risky because the daemon runs <code>agy</code> with <code>--dangerously-skip-permissions</code>.
</details>

<details>
<summary><b>Q2: "Failed to find agy CLI binary"?</b></summary>
Ensure <code>agy</code> is installed in a standard PATH directory such as <code>~/.local/bin/agy</code> or <code>/usr/local/bin/agy</code>. Verify with <code>which agy</code> or <code>agy --version</code>. If using systemd, ensure <code>Environment=PATH=...</code> in the unit file contains the agy directory.
</details>

<details>
<summary><b>Q3: How do background timers (`schedule`) work on Telegram?</b></summary>
Starting in v1.4.0, when you instruct the agent to schedule a task or reminder (e.g., <i>"Remind me in 10 minutes to review PR"</i>), the daemon's Proactive Notification Watcher continuously monitors the Brain ledger and pushes the alert to your Telegram chat the moment the timer fires.
</details>

---

## 📜 License

This project is open-sourced under the **MIT License**. Compliant with Google Antigravity CLI native specifications and high-performance Rust best practices.
