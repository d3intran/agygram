pub mod bot;
pub mod config;
pub mod executor;
pub mod models;
pub mod qq;
pub mod quota;
pub mod session;
pub mod text;
pub mod tts;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use serde::Serialize;
use session::SessionManager;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "agygram")]
#[command(
    about = "Multi-Platform Bot Daemon (Telegram & QQ) for Antigravity CLI (agy)",
    long_about = None
)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Bot 守护进程 (支持 Telegram & QQ 并行运行)
    Daemon,
    /// 交互式快速初始化与配置文件向导 (Init Wizard)
    Init,
    /// 显示当前配置与配置文件位置
    Config,
    /// 检查 agy CLI 环境与配置有效性
    Check,
    /// 检查并打印实时 Google Cloud Models & Quota 响应
    Quota,
}

#[derive(Serialize)]
struct InitConfig<'a> {
    bot_token: &'a str,
    allowed_user_ids: Vec<u64>,
    allow_all_users: bool,
    default_workspace: &'a str,
    default_model: &'a str,
    default_effort: &'a str,
    default_mode: &'a str,
    draft_update_interval_ms: u64,
    interrupt_on_new_message: bool,
    idle_timeout_secs: u64,
    max_execution_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    qq_app_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qq_app_secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    qq_allowed_openids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    qq_allowed_groups: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qq_group_session_mode: Option<&'a str>,
}

struct InitWizardParams<'a> {
    bot_token: &'a str,
    allowed_user_ids: Vec<u64>,
    qq_app_id: Option<&'a str>,
    qq_app_secret: Option<&'a str>,
    qq_allowed_openids: Vec<String>,
    qq_allowed_groups: Vec<String>,
    workspace: &'a str,
}

fn build_init_toml(params: &InitWizardParams<'_>) -> Result<String> {
    let has_qq = params.qq_app_id.is_some() && params.qq_app_secret.is_some();
    let cfg = InitConfig {
        bot_token: params.bot_token,
        allowed_user_ids: params.allowed_user_ids.clone(),
        allow_all_users: false,
        default_workspace: params.workspace,
        default_model: "",
        default_effort: models::Effort::High.as_str(),
        default_mode: models::Mode::AcceptEdits.as_str(),
        draft_update_interval_ms: 1000,
        interrupt_on_new_message: true,
        idle_timeout_secs: 180,
        max_execution_secs: 3600,
        qq_app_id: params.qq_app_id,
        qq_app_secret: params.qq_app_secret,
        qq_allowed_openids: params.qq_allowed_openids.clone(),
        qq_allowed_groups: params.qq_allowed_groups.clone(),
        qq_group_session_mode: has_qq.then_some(models::QqGroupSessionMode::Group.as_str()),
    };

    let body = toml::to_string_pretty(&cfg).context("Failed to serialize generated config TOML")?;
    Ok(format!("# agygram Configuration File\n{body}"))
}

fn write_secure_config(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory {}", parent.display()))?;
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts
        .open(path)
        .with_context(|| format!("Failed to open {} for writing", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.flush()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set 0600 permissions on {}", path.display()))?;
    }

    Ok(())
}

fn parse_csv_ids(raw: &str) -> Vec<u64> {
    raw.split(',')
        .filter_map(|part| part.trim().parse::<u64>().ok())
        .collect()
}

fn parse_csv_strings(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn run_init() -> Result<()> {
    println!("🚀 欢迎使用 agygram 初始化向导 (Interactive Setup Wizard)！\n");
    let config_path = Config::get_config_path()?;
    if config_path.exists() {
        let confirm = prompt_line(&format!(
            "⚠️ 检测到已有配置文件: {}\n是否覆盖已有配置？(y/N): ",
            config_path.display()
        ))?;
        if !confirm.eq_ignore_ascii_case("y") {
            println!("已取消初始化。当前配置未受影响。");
            return Ok(());
        }
    }

    println!("------------------------------------------------------------");
    let bot_token = prompt_line(
        "1. 请输入 Telegram Bot Token (从 @BotFather 获取；若仅使用 QQ 机器人可直接回车跳过):\n> ",
    )?;

    let allowed_user_ids = if bot_token.is_empty() {
        Vec::new()
    } else {
        let user_ids_str = prompt_line(
            "\n   请输入授权操作的 Telegram User ID (纯数字，多个用逗号分隔；留空则拒绝所有请求):\n> ",
        )?;
        let ids = parse_csv_ids(&user_ids_str);
        if ids.is_empty() {
            println!("   ⚠️ Telegram 白名单为空：守护进程将拒绝所有 Telegram 请求。需要公开可用时，请在配置中显式设置 allow_all_users = true。");
        }
        ids
    };

    let qq_app_id_raw = prompt_line(
        "\n2. 请输入 QQ 开放平台 AppID (https://q.qq.com；若不使用 QQ 机器人请直接回车跳过):\n> ",
    )?;
    let (qq_app_id, qq_app_secret, qq_allowed_openids, qq_allowed_groups) =
        if qq_app_id_raw.is_empty() {
            (None, None, Vec::new(), Vec::new())
        } else {
            let secret = prompt_line("   请输入 QQ AppSecret:\n> ")?;
            if secret.is_empty() {
                anyhow::bail!("QQ AppSecret 不能为空");
            }
            let openids_raw = prompt_line(
                "   请输入授权私聊的 QQ 用户 OpenID (多个用逗号分隔；留空则拒绝私聊请求):\n> ",
            )?;
            let groups_raw = prompt_line(
                "   请输入授权使用的 QQ 群 OpenID (多个用逗号分隔；留空则拒绝群聊请求):\n> ",
            )?;
            (
                Some(qq_app_id_raw),
                Some(secret),
                parse_csv_strings(&openids_raw),
                parse_csv_strings(&groups_raw),
            )
        };

    if bot_token.is_empty() && (qq_app_id.is_none() || qq_app_secret.is_none()) {
        println!("❌ 至少需要配置 Telegram Bot Token 或 QQ AppID/AppSecret！初始化终止。");
        return Ok(());
    }

    let default_home = dirs::home_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let ws = prompt_line(&format!(
        "\n3. 请输入默认工作主目录 (默认: {default_home}):\n> "
    ))?;
    let workspace = if ws.is_empty() { "~" } else { ws.as_str() };

    let content = build_init_toml(&InitWizardParams {
        bot_token: &bot_token,
        allowed_user_ids,
        qq_app_id: qq_app_id.as_deref(),
        qq_app_secret: qq_app_secret.as_deref(),
        qq_allowed_openids,
        qq_allowed_groups,
        workspace,
    })?;

    write_secure_config(&config_path, &content)?;

    println!(
        "\n✅ 配置文件生成成功 (权限 0600): {}",
        config_path.display()
    );
    println!("------------------------------------------------------------");
    println!("🎉 初始化完成！你可以运行以下命令启动服务：\n");
    println!("   agygram check   # 验证配置与环境");
    println!("   agygram daemon  # 启动 Bot 守护进程\n");
    Ok(())
}

fn run_show_config() -> Result<()> {
    let config_path = Config::get_config_path()?;
    println!("配置文件路径: {}", config_path.display());
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        println!("\n--- 配置内容 ---\n{content}");
    } else {
        println!("配置文件尚未创建。可运行 `agygram init` 进行快速创建。");
    }
    Ok(())
}

fn run_check() -> Result<()> {
    let config_path = Config::get_config_path()?;
    println!("1. 检查配置文件: {}", config_path.display());
    match Config::load() {
        Ok(cfg) => {
            if cfg.has_telegram() {
                println!(
                    "   ✅ Telegram Bot: 已配置 (授权用户: {:?})",
                    cfg.allowed_user_ids
                );
                if cfg.denies_everyone() {
                    println!("   ⚠️ Telegram 白名单为空且 allow_all_users = false：守护进程会拒绝所有 Telegram 请求。");
                }
            } else {
                println!("   ⚪ Telegram Bot: 未配置");
            }

            if cfg.has_qq() {
                println!(
                    "   ✅ QQ Bot (开放平台): 已配置 (AppID: {}, 会话模式: {}, 授权 OpenID: {:?}, 授权群: {:?})",
                    cfg.qq_app_id.as_deref().unwrap_or("-"),
                    cfg.qq_group_session_mode,
                    cfg.qq_allowed_openids,
                    cfg.qq_allowed_groups
                );
                if cfg.denies_qq_everyone() {
                    println!("   ⚠️ QQ 白名单为空且 allow_all_users = false：守护进程会拒绝所有 QQ 请求。");
                }
            } else {
                println!("   ⚪ QQ Bot (开放平台): 未配置 (可在 config.toml 中配置 qq_app_id 与 qq_app_secret)");
            }
        }
        Err(e) => println!("   ❌ 配置加载失败: {e}"),
    }

    println!("2. 检查 agy CLI 命令...");
    if let Ok(p) = which::which("agy") {
        println!("   ✅ 找到 agy CLI 路径: {}", p.display());
    } else {
        let fallback = dirs::home_dir().unwrap_or_default().join(".local/bin/agy");
        if fallback.exists() {
            println!("   ✅ 在 fallback 路径找到 agy CLI: {}", fallback.display());
        } else {
            println!("   ⚠️ 未在 PATH 中找到 agy CLI，请确认 agy 已正确安装并添加到 PATH。");
        }
    }
    Ok(())
}

fn run_quota() -> Result<()> {
    println!("🔍 正在查询 Google Cloud Antigravity Models & Quota 实时额度...\n");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to initialize Tokio runtime for quota check")?;

    rt.block_on(async {
        if let Ok(cfg) = Config::load() {
            quota::configure_proxy(cfg.proxy_url.as_deref());
        }
        let panel = quota::QuotaFetcher::format_models_and_quota_plain()
            .await
            .context("获取实时 Quota 失败")?;
        println!("{panel}");
        Ok(())
    })
}

fn run_daemon() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to initialize Tokio multi-thread runtime")?;

    rt.block_on(async {
        let (config, config_path) = Config::load_with_path()?;
        info!("配置文件加载成功: {}", config_path.display());

        quota::configure_proxy(config.proxy_url.as_deref());
        tts::configure_proxy(config.proxy_url.as_deref());

        let config = Arc::new(config);
        let session_mgr = Arc::new(SessionManager::from_config(&config));

        match (config.has_telegram(), config.has_qq()) {
            (true, true) => {
                info!("🚀 并行启动多平台守护进程: Telegram Bot + QQ Bot...");
                let tg_config = config.clone();
                let tg_mgr = session_mgr.clone();
                let qq_config = config.clone();
                let qq_mgr = session_mgr.clone();

                let cancel = CancellationToken::new();
                let tg_cancel = cancel.clone();
                let qq_cancel = cancel.clone();

                let mut tg_task = tokio::spawn(async move {
                    let res = tokio::select! {
                        res = bot::run_bot_with_session_mgr(tg_config, tg_mgr) => res,
                        _ = tg_cancel.cancelled() => Ok(()),
                    };
                    tg_cancel.cancel();
                    res
                });

                let mut qq_task = tokio::spawn(async move {
                    let res = tokio::select! {
                        res = qq::run_qq_bot(qq_config, qq_mgr) => res,
                        _ = qq_cancel.cancelled() => Ok(()),
                    };
                    qq_cancel.cancel();
                    res
                });

                let (first_name, first_res, sibling_name, mut sibling_task) = tokio::select! {
                    join_res = &mut tg_task => (
                        "Telegram Bot",
                        join_res
                            .map_err(|e| anyhow::anyhow!("Telegram Bot task panicked: {e}"))
                            .and_then(|r| r),
                        "QQ Bot",
                        qq_task,
                    ),
                    join_res = &mut qq_task => (
                        "QQ Bot",
                        join_res
                            .map_err(|e| anyhow::anyhow!("QQ Bot task panicked: {e}"))
                            .and_then(|r| r),
                        "Telegram Bot",
                        tg_task,
                    ),
                };

                cancel.cancel();

                let sibling_res = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    &mut sibling_task,
                )
                .await
                {
                    Ok(join_res) => join_res
                        .map_err(|e| anyhow::anyhow!("{sibling_name} task panicked: {e}"))
                        .and_then(|r| r),
                    Err(_) => {
                        sibling_task.abort();
                        Ok(())
                    }
                };

                if let Err(err) = first_res {
                    error!("{first_name} daemon terminated with error: {err:#}");
                    return Err(err);
                }
                if let Err(err) = sibling_res {
                    error!("{sibling_name} daemon terminated with error: {err:#}");
                    return Err(err);
                }
                info!("{first_name} daemon exited; dual-bot shutdown complete.");
                Ok(())
            }
            (true, false) => {
                info!("🚀 启动 Telegram Bot 守护进程...");
                Box::pin(bot::run_bot_with_session_mgr(config, session_mgr)).await
            }
            (false, true) => {
                info!("🚀 启动 QQ Bot 守护进程...");
                Box::pin(qq::run_qq_bot(config, session_mgr)).await
            }
            (false, false) => {
                anyhow::bail!(
                    "Neither Telegram bot_token nor QQ credentials configured in config.toml."
                );
            }
        }
    })
}

/// Entrypoint for the `agygram` CLI and daemon.
///
/// # Errors
/// Returns an error if CLI execution, configuration loading, or the bot daemon fails.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Commands::Daemon);

    let default_level = if matches!(command, Commands::Daemon) {
        "info"
    } else {
        "warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();

    match command {
        Commands::Daemon => run_daemon(),
        Commands::Init => run_init(),
        Commands::Config => run_show_config(),
        Commands::Check => run_check(),
        Commands::Quota => run_quota(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_toml_escapes_special_chars_and_supports_qq_only() {
        let toml_text = build_init_toml(&InitWizardParams {
            bot_token: "",
            allowed_user_ids: Vec::new(),
            qq_app_id: Some("102857208"),
            qq_app_secret: Some(r#"sec"with\special"#),
            qq_allowed_openids: vec!["OPENID_A".into()],
            qq_allowed_groups: vec!["GROUP_B".into()],
            workspace: r#"C:\work "dir"\test"#,
        })
        .expect("TOML serialization must succeed");

        let parsed: Config = toml::from_str(&toml_text).expect("generated TOML must parse back");
        assert!(!parsed.has_telegram());
        assert!(parsed.has_qq());
        assert_eq!(parsed.qq_app_secret.as_deref(), Some(r#"sec"with\special"#));
        assert_eq!(parsed.default_workspace, r#"C:\work "dir"\test"#);
        assert_eq!(parsed.qq_group_session_mode, "group");
        assert_eq!(parsed.default_mode, "accept-edits");
    }

    #[cfg(unix)]
    #[test]
    fn write_secure_config_enforces_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agygram_init_perm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg_path = dir.join("nested/config.toml");

        write_secure_config(&cfg_path, "bot_token = \"secret\"\n").unwrap();
        let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_example_toml_parses_cleanly() {
        let example = include_str!("../config.example.toml");
        let parsed: Config =
            toml::from_str(example).expect("config.example.toml must deserialize cleanly");
        assert_eq!(parsed.default_mode, "accept-edits");
        assert_eq!(parsed.default_effort, "high");
        assert_eq!(parsed.qq_group_session_mode, "group");
    }
}
