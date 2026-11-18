# Changelog

All notable changes to `agygram` are documented in this file.

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
