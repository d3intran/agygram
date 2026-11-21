# Changelog

All notable changes to `agygram` are documented in this file.

## [1.7.0] - 2026-11-21

### Security
- **Telegram `sendWidget` Token Redaction (`bot/widget.rs`)**: Replaced raw `reqwest::Error` wrapping with `.without_url()` + token scrubbing across `sendWidget` and `editMessageWidgetData` so production `bot_token` can never leak into `journalctl` or chat error replies.
- **QQ Shared-Group RBAC & `/file` Path Confinement (`config.rs`, `artifacts.rs`, `qq/commands.rs`, `qq/delivery.rs`)**:
  - Added `qq_admin_openids` (`AGYGRAM_QQ_ADMIN_OPENIDS`, with backward-compatible fallback to non-group entries in `qq_allowed_openids`) restricting sensitive commands (`/file`, `/history`, `/resume`, `/rules`, `/cd`, `/adddir`, `/cleardirs`, `/system`, `/sandbox`, `/project`, `/remote`) in shared groups to admins.
  - Enforced shared `artifacts::validate_explicit_file_path` security checks on `/file` (workspace/scratch root confinement, hidden dot-component blocking, `looks_sensitive` filtering, binary/ELF rejection), plus shared-group credential-leak warnings (`auth_token=`, `sk-`, `ghp_`, private keys).
  - Injected default `--settings` `permissions.deny` rules protecting `~/.config/agygram/*`, `~/.config/userbot/*`, and `~/.ssh/*`, and redacted secret keys by default in `agygram config`.

### Fixed
- **Antigravity CLI 1.5.14 Model Catalog & 2026-11-30 Compatibility (`models.rs`, `bot/callbacks.rs`)**:
  - Added **Gemini 4 Neon** (`xhigh`/`high`/`medium`/`low`) and **Orchestrator (Neon)** at top flagship rank, updated default model labels to `Gemini 4 Neon`, and stopped clamping default-model `xhigh` down to `high`.
  - Removed backward remapping of `claude-opus-5-5-*` / `claude-sonnet-5-5-*` onto retiring Claude 4.6 IDs ahead of the 2026-11-30 deprecation.
- **Native Leading `/` Slash Commands & True `/compact` (`executor/mod.rs`, `bot/commands.rs`, `qq/commands.rs`)**:
  - Stopped prepending `[System Instructions: ...]` ahead of leading `/` commands (bare passthrough for `/compact`, trailing suffix for `/swarm`/`/goal`/skills), restoring native `agy` command/skill expansion and true `CHECKPOINT` context compaction on both Telegram and QQ (with `>300K` token nudge on QQ).
- **Fixed `sessions.json` `O(N²)` Token & Duration Inflation (`session.rs`)**: Overwrote cumulative per-conversation `usage` and `duration_seconds` from `agy`'s `ResultPayload` instead of summing them every turn, automatically self-healing legacy inflated counters.
- **QQ 5-Slot Passive Reply Budget & `40034128` Double-Hit Fix (`qq/client.rs`, `qq/delivery.rs`)**:
  - Tracked passive reply slot budgets per `msg_id` (accounting for placeholders and media), auto-expanded chunk budgets (`2200` chars / `6500` B) for long answers, and gracefully archived overflow text to `scratch/reply-<id>.md` instead of silently dropping tails.
  - Stopped re-probing exhausted `msg_id`s on `40034128` -> `40034105` and restricted Markdown 1-hour blacklisting strictly to Markdown permission rejections (never on `429` or passive limits).
- **Lifecycle, Concurrency & Watchdog Hardening (`lib.rs`, `config.rs`, `session.rs`, `bot/pipeline.rs`, `bot/notifier.rs`, `qq/gateway.rs`, `qq/mod.rs`)**:
  - Added Unix `SIGTERM` + `SIGINT` graceful shutdown (`session_mgr.shutdown_all_processes()`) and resilient dual-bot supervision.
  - Raised default `idle_timeout_secs` from `180s` to `420s` to align with `agy 1.5.10+` long thinking (600s) and rate-limit backoff (240s) windows.
  - Separated `detached_pids` from foreground `active_pid` so follow-up messages no longer `SIGINT`-kill active background subagents/timers; killed stale-generation PIDs immediately on `set_pid`; added configurable global `max_concurrent_agy` semaphore (default `2` for 892 MB VPS hosts).
  - Fixed Telegram `initial_status_msg` cancellation race (`select!` + post-send cancel re-check), wired `BotRuntime` into `cp:cancel`/`/new`, and added per-chat `CancellationToken` (`QqRuntime`) + fault-tolerant detached follow-up spawning on QQ.
  - Fixed `artifacts.rs` to allow `brain/<id>/scratch/` media and hide unclosed `[[send_file:...` draft markers; fixed `notifier.rs` half-written fresh JSON swallowing and permanent TG error retry loops; fixed `text.rs` blockquote fenced-code reopening; fixed `stream-json` malformed `{...}` leakage and tool-step detach deadlocks.

### Added & Optimized
- **Full `agy 1.5.14` Integration & Optional ACP Backend (`executor/protocol.rs`, `executor/mod.rs`, `executor/acp.rs`, `quota.rs`)**:
  - Extended `stream-json` parsing for `init` metadata, `StepType::Checkpoint`, `subagent_info`, `tool_info.error`, `ResultPayload.{finished_reason, structured_output}`, and `WAITING` status.
  - Added `--settings` (`/thinking`), `--remote-agent` (`/remote`), `--json-schema` structured `/card` generation, `agy -p /credits --output-format json` display, CLI-first quota fetching (eliminating OAuth refresh write races), and `cache/conversation_metadata.json`-backed `/history` (filtering internal subagent/swarm sessions and showing AI titles).
  - Added optional `agy acp --stdio` JSON-RPC backend pilot (`executor_backend = "acp"`, idle auto-reaper `acp_idle_timeout_secs = 300`, automatic fallback to `print` mode) and removed ineffective `stdbuf` wrapping on the Go `agy` binary.
- **Telegram & QQ UX Overhaul (`bot/*`, `qq/*`)**:
  - **Telegram**: Removed constant 1.46 KB Widget prompt bloat on normal turns (now on-demand + 24h `sendWidget` capability cache), upgraded `split_html_chunks` to visible UTF-16 length (packing ~3,900 CJK chars/message), added `reply_to_message` quote context, non-blocking typing heartbeats, `reply_markup` preservation on plain fallback, `/changelog` truncation, `/update` version cache invalidation, and cross-channel TG+QQ status on `/panel`.
  - **QQ**: Modularized `qq/handler.rs` (4,372 lines) into `handler.rs`, `commands.rs`, `delivery.rs`, and `media.rs`; added recursive LaTeX-to-Unicode formatting, `<faceType>` tag sanitization, `.wav` voice extension fix + 2x retry + accurate network-timeout quote notices, URL/ref-hash attachment deduplication + 7d/100MB `scratch/m*` LRU cleanup, unknown `/xxx` command guard, structured command/queue/delivery logging, hourly Op 7 reconnect `INFO` downgrade, and lower peak upload RAM via immediate `drop(bytes)`.

## [1.6.5] - 2026-11-20

### Fixed & Optimized
- **Fixed Dropped QQ Detached Subagent Replies & AI-Generated Images (`qq/handler.rs`)**:
  - Added `stream_qq_detached_followups` (parity with Telegram's `stream_detached_followups`) so asynchronous subagents (e.g. `image-generator`) that finish after Turn 1's 1.5s detachment grace window automatically deliver their Turn-2+ text and media back to QQ instead of being silently dropped when `stream_rx` closed. Guards against stale replies via `session_mgr.is_generation_current` and clears detached PIDs on completion.
- **Zero-Copy Direct Brain Media Delivery & Eliminated Extra Image-Gen LLM Rounds (`artifacts.rs`, `bot/pipeline.rs`, `qq/handler.rs`)**:
  - Whitelisted direct-child recognized media files (`.jpg/.jpeg/.png/.webp/.gif/.mp4/.mov/.webm`) inside the active conversation's `~/.gemini/antigravity-cli/brain/<conversation_id>/` directory while strictly rejecting subdirectories (`.system_generated/`), dotfiles, sensitive filenames, non-media files, and other conversations' directories.
  - Updated `FILE_DELIVERY_PROMPT_GUIDE` and `QQ_DEFAULT_SYSTEM_PROMPT` so generated images can be sent directly via `[[send_file:...]]` without wasting extra LLM turns and safety-monitor evaluations on `cp ... scratch/` or redundant `read_file` checks / subagent polling loops.
- **QQ & Telegram End-to-End Latency Overhaul (`qq/client.rs`, `qq/handler.rs`, `bot/pipeline.rs`, `bot/media.rs`, `executor/mod.rs`, `tts.rs`)**:
  - **Adaptive QQ Markdown Capability Cache & Direct Plain-Text Placeholders**: Cached `msg_type=2` unsupported rejections for 1 hour once plain-text fallback succeeds (eliminating 2x serial HTTPS RTTs on every QQ message/chunk for bots without Markdown whitelisting) and routed ephemeral placeholders (`"我想想..."`, queue notices) directly through `send_plain_message`.
  - **Concurrent QQ Turn Pipeline**: Spawned `"我想想..."` placeholders concurrently with parallel attachment/quote downloads (zero RTT added before `agy` spawn), overlapped rich-media `/files` uploads (bounded concurrency = 3) with text reply delivery in `deliver_qq_reply`, and made post-delivery placeholder recall non-blocking.
  - **Eliminated Duplicate `agy` Workspace Mount & Blocking Telegram Chat Actions**: Stopped passing redundant `--add-dir <workspace>` alongside `current_dir(workspace)` (avoiding double `workspaceDirs` file-watcher indexing in `agy`), moved Telegram `send_chat_action` (`Typing` / `UploadPhoto` / `UploadDocument`) off the critical path, and enabled `tcp_nodelay` + `pool_idle_timeout` on the Fish Audio TTS client.

## [1.6.4] - 2026-11-18

### Fixed
- **Eliminated Post-Turn Telegram Notification Log Dumping (`notifier.rs`)**:
  - **Live Reload of `agy`'s `read.json` + Dedicated `agygram_notified.json`**: Fixed a stale-cache bug where `notifier` loaded `.system_generated/messages/read.json` only once at daemon boot (`cache.ledger_loaded`), causing background commands (`task-XXX`) already consumed by `agy` during a turn to be dumped onto Telegram right after the turn finished—and overwriting `agy`'s `read.json`. `notifier` now reloads `read.json` (read-only) on every directory change and persists its own state in `agygram_notified.json`.
  - **Internal Tool Plumbing Suppression & Single-Card Cap**: Filtered out internal LLM wakeup messages (`run_command`, `invoke_subagent`, `swarm`, `run_workflow`) so raw compiler/shell logs are never pushed to user chats, added a 90s first-boot historical flood guard, capped legitimate timer/reminder notifications to a single expandable card, and quieted benign QQ 2-minute placeholder recall TTL (`40064004`) logs.

## [1.6.3] - 2026-11-18

### Added & Fixed
- **QQ Group Multi-Speaker Attribution & Persistent Member Nicknames**:
  - **Automatic Speaker Tagging in Shared Group Sessions**: When `qq_group_session_mode` is `"group"` (default shared context), every group message now injects a deterministic speaker header (`[群聊发言人: 群成员 #XXXX]` or `[群聊发言人: 群成员「称呼」(#XXXX)]`) plus multi-user system prompt instructions so `agy` cleanly distinguishes different group members instead of mistaking everyone for the same user.
  - **Persistent `/nick` / `/whoami` Identity Binding**: Group and C2C users can run `/nick <称呼>` (or `/nick clear`, `/whoami`) to bind a human-readable nickname stored in `~/.config/agygram/qq_nicknames.json` (`0600` atomic write), surviving `/new` resets and daemon restarts.
- **QQ Quoted/Reply Message Resolution & Context Ring Cache**:
  - **Full Wire Parsing for Quotes & Outbound `ref_idx`**: Added `message_scene` (`msg_idx` / `ref_msg_idx` with automatic token redaction), `message_reference`, composite `msg_elements` (`message_type = 103`), and outbound `MessageResponse.ext_info.ref_idx` parsing.
  - **Bounded Per-Chat Message & Media Ring Buffer**: Records inbound `@Bot` messages, passive `GROUP_MESSAGE_CREATE` messages (when full-group mode is enabled on QQ Open Platform), and all outbound Bot text/image/file/voice replies (`id` + `ref_idx` + local `scratch/` media paths). Quoting a Bot-sent photo/reply or prior group message now automatically injects `[引用消息上下文]` and `[Quoted File: ...]` (downloading cached remote attachments on demand), and emits an explicit anti-hallucination guard note if QQ withholds an uncached historical message.

## [1.6.2] - 2026-11-18

### Performance & Latency Overhaul
- **Zero-Blocking Stale-While-Revalidate (SWR) Control Panel & Commands (Telegram & QQ)**:
  - **Fixed Self-Cancelling Quota Timeout Loop**: Removed the 1.8s `tokio::time::timeout` wrapper around `agy -p /quota` (which took ~5.6s on 1-vCPU VPS nodes, killing the subprocess at 1.80s before `QUOTA_CACHE` could ever populate and causing every panel open/button click to stall for 1.8s with perpetual cache misses). Replaced with `QuotaFetcher::peek_overview_and_revalidate()` (`0 ms` snapshot read + detached single-flight background refresh, 180s TTL).
  - **Non-Blocking Model, Agent & CLI Version Snapshots**: Added `trigger_background_model_refresh_if_stale()` (30-min TTL), `AgyExecutor::list_agents_parsed_cached()` (10-min SWR TTL, eliminating 2.0s `agy agent` stalls on Workspace card clicks and deduplicating double `agy agent` spawns in `/agent`), `peek_cached_agy_version()` (`0 ms` snapshot), and sequential low-memory startup cache warmup in `run_daemon()`.
  - **Concurrent Callback Acknowledgement & Parallel Manual Refresh**: `answerCallbackQuery` now dispatches concurrently with state mutation and `editMessageText` (saving a full Telegram HTTPS RTT ~250–450ms on every button tap), while explicit `🔄 刷新` buttons run `agy models` and live quota fetches in parallel via `tokio::join!` with an immediate toast notification.
- **HTTP Transport, Filesystem & Streaming Hot-Path Optimizations**:
  - **Tuned Keep-Alive & `TCP_NODELAY` Clients**: Enabled `tcp_nodelay(true)`, `tcp_keepalive(60s)`, `pool_idle_timeout(90s)`, and `pool_max_idle_per_host` on Telegram Bot (`build_telegram_bot` with or without proxy), Widget transport, and QQ OpenAPI client.
  - **Memoized Transcript Titles & Brain Roots**: Cached `brain_dir_candidates()` in `OnceLock`, bounded `extract_transcript_title` to 40 lines / 128 KB, and memoized conversation titles in `CONV_TITLE_CACHE` so `/session`, `/history`, and `/resume` avoid repeated multi-megabyte `.jsonl` reads.
  - **Zero-Alloc Streaming & Fast-Path Widget/Notifier Guards**: Added zero-allocation `contains_ascii_case_insensitive_str` for `[[send_file:...]]` and `tg-widget`/`tg-card` draft/reply scans, skipped redundant `canonicalize` syscalls when replies contain no `.widget.html`, and added a 6s missing-directory probe backoff in the background notification poller (~75% fewer idle `stat` syscalls).

## [1.6.1] - 2026-11-18

### Fixed & Changed
- **Explicit-Only Artifact Delivery & Full `.md`/Document Support (Telegram & QQ)**:
  - **No Unsolicited File Spam**: Stopped blindly auto-sending workspace/scratch files merely mentioned in AI prose unless the user explicitly asks to send a file or the AI emits `[[send_file:<path>]]`.
  - **Structured `[[send_file:<path>]]` Protocol + Prompt Intent Guard**: Added `FILE_DELIVERY_PROMPT_GUIDE` to both Telegram and QQ system prompts, `extract_explicit_send_files` (strips `[[send_file:...]]` outside code fences and during live TG streaming drafts), and `prompt_explicitly_requests_file_delivery` fallback (handles explicit send requests in resumed sessions while ignoring troubleshooting questions like *"why didn't a file send?"*).
  - **Fixed `.md` / `.txt` / `.html` Scratch Delivery**: Explicitly requested documents (`.md`, `.markdown`, `.txt`, `.html`, `.json`, `.csv`, `.pdf`, `.zip`, media, etc.) are now delivered cleanly via QQ `file_type=4` Base64 group/C2C upload and Telegram `sendDocument`, instead of being silently dropped by `is_blocked_artifact_extension`.
  - **Hardened Path Confinement & Sensitivity Filtering**: Added hidden dot-component traversal blocking inside workspace/scratch roots (e.g. `.gemini/`, `.ssh/`, `.config/`, `.env`, `.git/`) and expanded `looks_sensitive` to reject `.session`, `oauth-token`, `agygram/config.toml`, and `userbot/config.json`.

## [1.6.0] - 2026-11-18

### Added
- **Unified Markdown AST & Responsive Smart Table Engine** (`pulldown-cmark` + `unicode-width`):
  - Replaced regex/line-by-line formatting with a full AST pipeline shared by Telegram (`TelegramFormatter`) and QQ (`QqFormatter`).
  - **CJK-Aware Responsive Tables**: Compact tables (`<= 38` display columns, single-line cells) render as East-Asian-width-aligned Unicode box-drawing grids (`┌─┬─┐` / `│` / `└─┴─┘`); wider or multi-line tables automatically switch to mobile-friendly vertical card/record layouts on both Telegram and QQ.
  - **Bulletproof Chunking & Entity Safety**: Tag-aware and fence-aware message splitting automatically closes and reopens `<pre>`, `<code>`, `<blockquote>`, or ```` ``` ```` fences across boundaries, preserves HTML entities (`&amp;`, `&lt;`, etc.), avoids nested `<blockquote>` tags, and falls back gracefully to plain text on Telegram entity parse errors.
  - Aligned support for headings, GitHub Alerts (`[!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`), expandable blockquotes, task lists (`☑`/`☐`), spoilers, and `Title (URL)` link rendering on QQ.
- **Rewritten Multi-Page Telegram Control Panel & Dynamic Model Registry**:
  - Added `/panel`, `/settings` (unified with `/status`), and `/session` multi-page InlineKeyboard dashboards (`Home`, `Model Family -> Variant/Effort`, `Live Quota`, `Effort`, `Mode`, `Session Management`, `Workspace & Agent`, `Output/TTS`, and `System & Ecosystem`).
  - Aligned built-in model catalog with live Antigravity CLI (`agy`) models: **Gemini 4 Hydrogen** (`xhigh`/`high`/`medium`/`low`), **Gemini 4 Carbon** (`high`/`medium`/`low`), **Gemini 4 Argon** (`high`/`medium`/`low`), **Gemini 3.8 Flash** (`high`/`medium`/`low`), **Orchestrator** (`Carbon`/`Argon`), **Claude 4.6** (`claude-opus-4-6-thinking`, `claude-sonnet-4-6`, with legacy `5-5` migration aliases), and **GPT-OSS 120B**.
  - Added live runtime merging from `agy models` (with 120s TTL and `/quota` unmapped-model refresh trigger) so newly deployed upstream models appear automatically.
  - Enforced strictly `<= 64`-byte deterministic `callback_data` tokens (short slugs + FNV-1a hashes `mh:`, `rh:`, `sh:`) to eliminate button truncation and session/CLI misalignment.
  - Added smart `--effort` compatibility (`effective_cli_effort`) that omits unsupported `--effort` flags on fixed-budget models (`Claude`, `Orchestrator`, `GPT-OSS`, effort-suffixed IDs) and clamps `xhigh`/`max` on default `gemini-4-carbon`.
- **Telegram Bot API 10.4+ QuickJS In-Message Widgets & Offline Mini-Games**:
  - Overhauled `/card` and `/widget` into an interactive hub featuring **4 built-in offline mini-games** (`2048`, `minesweeper`, `memory`, `reaction`), all strictly validated under Telegram's `<= 32 KB` raw and `<= 4,096 B` zlib-9 compressed limits (`flate2`).
  - Added multi-template JSON cards (`CustomCardSpec`: `metric`, `quiz`, `table`, `status`, `card`), `/card html <target>` standalone WebView `.html` exports, and `/card <prompt>` AI generation.
  - Integrated `WIDGET_PROMPT_GUIDE` and `extract_ai_widgets_with_roots` into the Telegram turn pipeline so `agy` can emit ```` ```tg-widget ```` (offline HTML/QuickJS mini-games/apps) or ```` ```tg-card ```` (JSON cards) or `.widget.html` artifacts in normal chat, with automatic `.html` document or HTML text fallback when `sendWidget` is unavailable (groups, `> 4 KB` compressed, or unsupported clients).
- **QQ Official Bot OpenAPI v2 Media/File Upgrade & Command Alignment**:
  - Upgraded `QqMediaFileType` to support all four OpenAPI v2 media types (`1`=Image, `2`=Video MP4, `3`=Voice, `4`=File) via direct Base64 `file_data` upload (`<= 5 MB` decoded, no external file server required).
  - Added `/file <path>`, `/panel`, `/compact [<focus>]`, `/workspace`, `/cd`, `/pwd`, `/dirs`, `/adddir`, `/cleardirs`, `/skills`, `/rules`, `/agents`, `/sandbox`, `/project`, and numbered `/model <index>` selection matching the dynamic registry.
  - Added non-silent graceful fallback for QQ files (`> 5 MB` or API-rejected `file_type=4` / `40034128` passive reply limits): inline formatted text/code previews for text-like artifacts (`<= 64 KB`) or explicit diagnostic + VPS path notices, plus proactive push fallback on passive reply exhaustion.
- **Repository & CI/CD Polish**:
  - Added GitHub issue templates (`.github/ISSUE_TEMPLATE/`), PR template, `AGYGRAM_VERSION` pinning in `update.sh`, binary fallback lookup in `run.sh`, and CI release smoke test (`./agygram --version` + `sha256sum -c`).

### Changed
- `Config::normalize` now normalizes `default_model` aliases (e.g. legacy `claude-opus-5-5-high` -> `claude-opus-4-6-thinking`) at load time.
- `tts::clean_text_for_speech` now strips Markdown table dividers, Unicode box-drawing borders, horizontal rules, and `[title](url)` URLs before Fish Audio synthesis.
- Hardened `executor` detachment (`1.5s` grace window after `state: DONE, step_type: agent_response`) and `artifacts` Markdown link/image path extraction.

## [1.5.14] - 2026-11-11
- Introduced initial Telegram Bot API 10.4 QuickJS in-message HTML widgets (`/card`, `/widget`) and fixed workspace/scratch artifact auto-delivery edge cases.
- Completed Rust Edition 2024 migration, cross-layer decoupling, workspace Clippy lints, and CI toolchain pinning (`1.99.1`).

## [1.5.10] - [1.5.13] - 2026-11-08 ~ 2026-11-11
- **v1.5.13**: Skipped unnecessary `agy` shutdown wait and fixed `/update` privilege elevation detection.
- **v1.5.12**: Added per-session Fish Audio voice switching (`/voice id <ID>`) and `fish_reference_id` default config.
- **v1.5.11**: Switched QQ voice mode to voice-only delivery with Fish Audio emotion cues (`[calm]`, `[happy]`, etc.).
- **v1.5.10**: Maintenance and stability release.

## [1.5.0] - [1.5.8] - 2026-11-08
- **v1.5.8**: Added QQ rich media voice replies and `/voice` toggle.
- **v1.5.7**: Added QQ group whitelist (`qq_allowed_groups`), hardened draft HTML, and redacted QQ tokens from logs.
- **v1.5.6**: Delivered generated images on QQ and improved prose artifact path detection.
- **v1.5.5**: Accepted inbound QQ image/file/voice attachments by downloading them into workspace `scratch/`.
- **v1.5.4**: Made `/new` and `/cancel` generation-authoritative and excluded QQ sessions from Telegram notifier.
- **v1.5.3**: Added instant `"我想想..."` placeholder, message recall, and QQ turn queuing concurrency.
- **v1.5.2**: Added `QqFormatter` for Markdown table conversion and `qq_allow_all_groups` config option.
- **v1.5.1**: Supported string-encoded `expires_in` from QQ `getAppAccessToken`.
- **v1.5.0**: Added native QQ Bot WebSocket gateway running concurrently with Telegram in a single daemon.

## [1.4.0] - [1.4.10] - 2026-10-22 ~ 2026-11-07
- Added proactive Brain queue notification daemon, multi-message rollover streaming (>4,000 chars), `auto` mode, expandable `<blockquote expandable>` thinking cards, autonomous OAuth token refresh & dual-endpoint `/quota` telemetry, `agygram quota` CLI command, background detachment follow-up streaming, and dynamic model validation.

## [1.3.0] - [1.3.1] - 2026-10-18 ~ 2026-10-22
- Added GitHub Actions CI/CD release pipeline, fixed background task deadlock, and hardened size/type-aware media egress and safe attachment naming.

## [1.2.0] - [1.2.2] - 2026-10-16
- Hardened fail-closed user whitelist (`allow_all_users = false`), callback authorization, sandbox-confined artifact delivery, CJK table alignment, and syntax highlighting across chunk boundaries.

## [1.1.0] - [1.1.2] - 2026-10-14
- Added Gemini 3.8 Flash support, `xhigh`/`max` effort levels, Brain directory resolution, and interactive `/model` inline keyboard selection.

## [1.0.0] - 2026-08-28
- Initial open-source release of `agygram` — ultra-lightweight Rust Telegram daemon for Google Antigravity CLI (`agy`).
