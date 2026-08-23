use agygram::bot;
use agygram::config::Config;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "agygram")]
#[command(about = "Telegram Bot Daemon for Antigravity CLI (agy)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Telegram Bot 守护进程 (默认)
    Daemon,
    /// 显示当前配置与配置文件位置
    Config,
    /// 检查 agy CLI 环境与配置有效性
    Check,
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
        Commands::Config => {
            let config_path = Config::get_config_path()?;
            println!("配置文件路径: {}", config_path.display());
            if config_path.exists() {
                let content = std::fs::read_to_string(&config_path)?;
                println!("\n--- 配置内容 ---\n{}", content);
            } else {
                println!("配置文件尚未创建。");
            }
        }
        Commands::Check => {
            let config_path = Config::get_config_path()?;
            println!("1. 检查配置文件: {}", config_path.display());
            match Config::load() {
                Ok(cfg) => println!("   ✅ 配置有效。Bot Token 已设置，授权用户: {:?}", cfg.allowed_user_ids),
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
    }

    Ok(())
}
