use agygram::bot;
use agygram::config::Config;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "agygram")]
#[command(about = "Telegram Bot Daemon for Antigravity CLI (agy)", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Telegram Bot 守护进程 (默认)
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Daemon) {
        Commands::Daemon => {
            let config = Config::load()?;
            info!("配置文件加载成功: {}", Config::get_config_path()?.display());
            bot::run_bot(config).await?;
        }
        Commands::Init => {
            println!("🚀 欢迎使用 agygram 初始化向导 (Interactive Setup Wizard)！\n");
            let config_path = Config::get_config_path()?;
            if config_path.exists() {
                print!("⚠️ 检测到已有配置文件: {}\n是否覆盖已有配置？(y/N): ", config_path.display());
                use std::io::Write;
                std::io::stdout().flush().unwrap();
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("已取消初始化。当前配置未受影响。");
                    return Ok(());
                }
            }

            use std::io::Write;
            println!("------------------------------------------------------------");
            print!("1. 请输入 Telegram Bot Token (从 @BotFather 获取):\n> ");
            std::io::stdout().flush().unwrap();
            let mut bot_token = String::new();
            std::io::stdin().read_line(&mut bot_token)?;
            let bot_token = bot_token.trim().to_string();

            if bot_token.is_empty() {
                println!("❌ Bot Token 不能为空！初始化终止。");
                return Ok(());
            }

            print!("\n2. 请输入授权操作的 Telegram User ID (纯数字，多个用逗号分隔；留空则拒绝所有请求):\n> ");
            std::io::stdout().flush().unwrap();
            let mut user_ids_str = String::new();
            std::io::stdin().read_line(&mut user_ids_str)?;
            let mut allowed_user_ids = Vec::new();
            for part in user_ids_str.split(',') {
                if let Ok(id) = part.trim().parse::<u64>() {
                    allowed_user_ids.push(id);
                }
            }
            if allowed_user_ids.is_empty() {
                println!("   ⚠️ 白名单为空：守护进程将拒绝所有消息。需要公开可用时，请在配置中显式设置 allow_all_users = true。");
            }

            let default_home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
            print!("\n3. 请输入默认工作主目录 (默认: {}):\n> ", default_home);
            std::io::stdout().flush().unwrap();
            let mut ws = String::new();
            std::io::stdin().read_line(&mut ws)?;
            let workspace = if ws.trim().is_empty() {
                "~".to_string()
            } else {
                ws.trim().to_string()
            };

            let config_content = format!(
                "# agygram Configuration File\n\
                bot_token = \"{}\"\n\
                allowed_user_ids = {:?}\n\
                allow_all_users = false\n\
                default_workspace = \"{}\"\n\
                default_model = \"\"\n\
                default_effort = \"high\"\n\
                default_mode = \"accept-edits\"\n\
                draft_update_interval_ms = 1000\n\
                interrupt_on_new_message = true\n\
                idle_timeout_secs = 180\n\
                max_execution_secs = 3600\n\
                executor_timeout_secs = 300\n",
                bot_token, allowed_user_ids, workspace
            );

            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&config_path, config_content)?;

            println!("\n✅ 配置文件生成成功: {}", config_path.display());
            println!("------------------------------------------------------------");
            println!("🎉 初始化完成！你可以运行以下命令启动服务：\n");
            println!("   agygram check   # 验证配置与环境");
            println!("   agygram daemon  # 启动 Telegram 守护进程\n");
        }
        Commands::Config => {
            let config_path = Config::get_config_path()?;
            println!("配置文件路径: {}", config_path.display());
            if config_path.exists() {
                let content = std::fs::read_to_string(&config_path)?;
                println!("\n--- 配置内容 ---\n{}", content);
            } else {
                println!("配置文件尚未创建。可运行 `agygram init` 进行快速创建。");
            }
        }
        Commands::Check => {
            let config_path = Config::get_config_path()?;
            println!("1. 检查配置文件: {}", config_path.display());
            match Config::load() {
                Ok(cfg) => {
                    println!("   ✅ 配置有效。Bot Token 已设置，授权用户: {:?}", cfg.allowed_user_ids);
                    if cfg.denies_everyone() {
                        println!("   ⚠️ 白名单为空且 allow_all_users = false：守护进程会拒绝所有 Telegram 请求。");
                    }
                }
                Err(e) => println!("   ❌ 配置加载失败: {}", e),
            }

            println!("2. 检查 agy CLI 命令...");
            match which::which("agy") {
                Ok(p) => println!("   ✅ 找到 agy CLI 路径: {}", p.display()),
                Err(_) => {
                    // Check if ~/.local/bin/agy exists
                    let fallback = dirs::home_dir().unwrap_or_default().join(".local/bin/agy");
                    if fallback.exists() {
                        println!("   ✅ 在 fallback 路径找到 agy CLI: {}", fallback.display());
                    } else {
                        println!("   ⚠️ 未在 PATH 中找到 agy CLI，请确认 agy 已正确安装并添加到 PATH。");
                    }
                }
            }
        }
        Commands::Quota => {
            println!("🔍 正在查询 Google Cloud Antigravity Models & Quota 实时额度...\n");
            match agygram::quota::QuotaFetcher::format_models_and_quota().await {
                Ok(panel) => {
                    println!("{}", panel);
                }
                Err(e) => {
                    eprintln!("❌ 获取实时 Quota 失败: {:#}", e);
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}
