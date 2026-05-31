mod archive;
mod backup;
mod config;
mod github;
mod i18n;
mod logger;
mod monitor;
mod status;

use anyhow::Result;
use clap::{Parser, Subcommand};
use log::{error, info};
use std::path::PathBuf;

/// OpenWrt Binary Update Manager
///
/// Automatically detect and update binaries on OpenWrt from GitHub Releases
#[derive(Parser, Debug)]
#[command(name = "openwrt-binary-manager", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check for updates and install them (single run)
    Upgrade {
        /// Path to config file
        config: PathBuf,
    },
    /// Run continuously as a daemon, checking for updates periodically
    Daemon {
        /// Path to config file
        config: PathBuf,
        /// Main loop interval in seconds
        #[arg(short, long, default_value_t = 60)]
        interval: u64,
    },
    /// Dry-run mode: check for available updates only (no changes made)
    Check {
        /// Path to config file
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Upgrade { config } => {
            let cfg = config::load_config(&config)?;
            init_i18n(&cfg);
            logger::init_logger(&cfg.config.log)?;
            info!("=== OpenWrt Binary Manager (upgrade) ===");
            info!("{}: {}", t!("Config", "配置文件"), config.display());
            info!("{}: {}", t!("Monitors", "监控数量"), cfg.monitors.len());

            std::fs::create_dir_all(&cfg.config.working_dir)?;
            run_all_monitors(&cfg).await;
            info!("{}", t!("=== Upgrade completed ===", "=== 更新完成 ==="));
        }
        Command::Daemon { config, interval } => {
            let cfg = config::load_config(&config)?;
            init_i18n(&cfg);
            logger::init_logger(&cfg.config.log)?;
            info!("=== OpenWrt Binary Manager (daemon) ===");
            info!("{}: {}", t!("Config", "配置文件"), config.display());
            info!("{}: {}s", t!("Daemon loop interval", "守护进程循环间隔"), interval);
            info!("{}: {}", t!("Monitors", "监控数量"), cfg.monitors.len());

            std::fs::create_dir_all(&cfg.config.working_dir)?;
            loop {
                run_all_monitors(&cfg).await;
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            }
        }
        Command::Check { config } => {
            let cfg = config::load_config(&config)?;
            init_i18n(&cfg);
            std::fs::create_dir_all(&cfg.config.working_dir)?;

            let status = match status::StatusFile::load(&cfg.config.status) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}: {}", t!("Failed to load status file", "无法加载状态文件"), e);
                    return Ok(());
                }
            };

            for (name, monitor) in &cfg.monitors {
                match monitor::check_only(name, monitor, &cfg.config, &status).await {
                    Ok(report) => {
                        if !report.is_empty() {
                            println!("[{}]", name);
                            for line in &report {
                                println!("  {}", line);
                            }
                            println!();
                        }
                    }
                    Err(e) => eprintln!("[{}] {}: {:#}", name, t!("Error", "错误"), e),
                }
            }
        }
    }

    Ok(())
}

fn init_i18n(cfg: &config::Config) {
    let lang = cfg
        .config
        .language
        .as_deref()
        .and_then(i18n::Lang::from_str)
        .unwrap_or_else(i18n::detect_locale);
    i18n::init(lang);
}

async fn run_all_monitors(cfg: &config::Config) {
    let mut status = match status::StatusFile::load(&cfg.config.status) {
        Ok(s) => s,
        Err(e) => {
            error!("{}: {}", t!("Failed to load status file", "无法加载状态文件"), e);
            return;
        }
    };

    for (name, monitor) in &cfg.monitors {
        match monitor::check_and_update(name, monitor, &cfg.config, &mut status).await {
            Ok(true) => info!("[{}] {}", name, t!("Update completed successfully", "更新成功完成")),
            Ok(false) => {}
            Err(e) => {
                error!("[{}] {}: {:#}", name, t!("Error", "错误"), e);
                status.update_check(name);
                if let Err(save_err) = status.save(&cfg.config.status) {
                    error!("[{}] {}: {}", name, t!("Failed to save status after error", "错误后保存状态失败"), save_err);
                }
            }
        }
    }
}
