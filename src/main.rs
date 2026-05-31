mod archive;
mod backup;
mod config;
mod github;
mod logger;
mod monitor;
mod status;

use anyhow::Result;
use clap::Parser;
use log::{error, info};
use std::path::PathBuf;

/// OpenWrt Binary Update Manager
///
/// 自动从 GitHub Releases 检测并更新 OpenWrt 上的二进制程序
#[derive(Parser, Debug)]
#[command(name = "openwrt-binary-manager", version, about)]
struct Cli {
    /// 配置文件路径
    #[arg(short, long, default_value = "/etc/updater/config.yaml")]
    config: PathBuf,

    /// 守护进程模式（持续运行）
    #[arg(short, long)]
    daemon: bool,

    /// 守护进程主循环间隔（秒）
    #[arg(short, long, default_value_t = 60)]
    interval: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 加载配置
    let cfg = config::load_config(&cli.config)?;

    // 初始化日志
    logger::init_logger(&cfg.config.log)?;

    info!("=== OpenWrt Binary Manager started ===");
    info!("Config: {}", cli.config.display());
    info!("Mode: {}", if cli.daemon { "daemon" } else { "once" });
    info!("Monitors: {}", cfg.monitors.len());

    // 确保 working_dir 存在
    std::fs::create_dir_all(&cfg.config.working_dir)?;

    if cli.daemon {
        // 守护进程模式：循环执行
        info!("Daemon loop interval: {}s", cli.interval);
        loop {
            run_all_monitors(&cfg).await;
            tokio::time::sleep(std::time::Duration::from_secs(cli.interval)).await;
        }
    } else {
        // 单次运行模式
        run_all_monitors(&cfg).await;
        info!("=== Single run completed ===");
    }

    Ok(())
}

/// 遍历所有 monitors 并执行检查/更新
async fn run_all_monitors(cfg: &config::Config) {
    // 每次运行时都重新加载 status，以支持外部修改
    let mut status = match status::StatusFile::load(&cfg.config.status) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to load status file: {}", e);
            return;
        }
    };

    for (name, monitor) in &cfg.monitors {
        match monitor::check_and_update(name, monitor, &cfg.config, &mut status).await {
            Ok(true) => info!("[{}] ✓ Update completed successfully", name),
            Ok(false) => {} // 跳过（已在 monitor 内部打日志）
            Err(e) => {
                error!("[{}] ✗ Error: {:#}", name, e);
                // 即使出错也更新 last_check，避免对同一个错误不断重试
                status.update_check(name);
                if let Err(save_err) = status.save(&cfg.config.status) {
                    error!("[{}] Failed to save status after error: {}", name, save_err);
                }
            }
        }
    }
}
