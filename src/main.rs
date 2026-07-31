// main.rs ── 程序入口与命令行分发
//
// 本文件定义了 CLI 子命令（upgrade / daemon / check），
// 负责加载配置、初始化国际化与日志，并按并发度调度所有 monitor。

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
use futures_util::StreamExt;
use log::{error, info};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// OpenWrt Binary Update Manager
///
/// Automatically detect and update binaries on OpenWrt from GitHub Releases
#[derive(Parser, Debug)]
#[command(name = "openwrt-binary-manager", version, about)]
struct Cli {
    /// 子命令：决定运行模式
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check for updates and install them (single run)
    /// 单次运行：检查并安装更新，适合配合 cron 使用
    Upgrade {
        /// Path to config file
        /// 配置文件路径
        config: PathBuf,
    },
    /// Run continuously as a daemon, checking for updates periodically
    /// 守护进程模式：周期性轮询检查更新
    Daemon {
        /// Path to config file
        /// 配置文件路径
        config: PathBuf,
        /// Main loop interval in seconds
        /// 主循环间隔（秒），默认 60 秒
        #[arg(short, long, default_value_t = 60)]
        interval: u64,
    },
    /// Dry-run mode: check for available updates only (no changes made)
    /// 检测模式：仅报告可用更新，不修改任何文件
    Check {
        /// Path to config file
        /// 配置文件路径
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 解析命令行参数（clap 自动处理 --help / --version）
    let cli = Cli::parse();

    match cli.command {
        // ── 单次升级模式 ──────────────────────────────────────────
        Command::Upgrade { config } => {
            // 读取并校验 YAML 配置文件
            let cfg = config::load_config(&config)?;
            // 根据配置初始化语言（中文/英文）
            init_i18n(&cfg);
            // 初始化日志系统（同时写入文件与 stderr）
            logger::init_logger(&cfg.config.log)?;
            info!("=== OpenWrt Binary Manager (upgrade) ===");
            info!("{}: {}", t!("Config", "配置文件"), config.display());
            info!("{}: {}", t!("Monitors", "监控数量"), cfg.monitors.len());

            // 确保工作目录存在（用于存放下载和解压的临时文件）
            std::fs::create_dir_all(&cfg.config.working_dir)?;
            // 并发执行所有 monitor 的检查与更新流程
            run_all_monitors(&cfg).await;
            info!("{}", t!("=== Upgrade completed ===", "=== 更新完成 ==="));
        }
        // ── 守护进程模式 ──────────────────────────────────────────
        Command::Daemon { config, interval } => {
            let cfg = config::load_config(&config)?;
            init_i18n(&cfg);
            logger::init_logger(&cfg.config.log)?;
            info!("=== OpenWrt Binary Manager (daemon) ===");
            info!("{}: {}", t!("Config", "配置文件"), config.display());
            info!("{}: {}s", t!("Daemon loop interval", "守护进程循环间隔"), interval);
            info!("{}: {}", t!("Monitors", "监控数量"), cfg.monitors.len());

            std::fs::create_dir_all(&cfg.config.working_dir)?;
            // 无限循环：每轮检查所有 monitor，然后休眠 interval 秒
            loop {
                run_all_monitors(&cfg).await;
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            }
        }
        // ── 检测模式（dry-run，仅报告不修改） ─────────────────────
        Command::Check { config } => {
            let cfg = config::load_config(&config)?;
            init_i18n(&cfg);
            std::fs::create_dir_all(&cfg.config.working_dir)?;

            // 加载状态文件以读取上次检查时间和已知 tag
            let status = match status::StatusFile::load(&cfg.config.status) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}: {}", t!("Failed to load status file", "无法加载状态文件"), e);
                    return Ok(());
                }
            };
            let status = Arc::new(status);
            let cfg = Arc::new(cfg);

            // 并发度至少为 1，避免空配置导致 buffer_unordered 出错
            let concurrency = cfg.config.concurrency.max(1);
            let monitors: Vec<_> = cfg.monitors.iter().collect();

            // 通过 buffer_unordered 实现“乱序并发”：最多同时跑 concurrency 个任务
            // 每个 monitor 调用 check_only，仅查询远程 release 并生成报告，不下载、不替换
            let results: Vec<_> = futures_util::stream::iter(monitors)
                .map(|(name, monitor)| {
                    let name = name.clone();
                    let cfg = Arc::clone(&cfg);
                    let status = Arc::clone(&status);
                    async move {
                        let result = monitor::check_only(&name, monitor, &cfg.config, &status).await;
                        (name, result)
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;

            // 输出每个 monitor 的检测报告到 stdout
            for (name, result) in results {
                match result {
                    Ok(report) => {
                        // 报告为空则跳过
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

/// 根据配置初始化国际化语言。
///
/// - 若配置中显式指定了 `language`，则按其解析；
/// - 否则通过环境变量（LANG/LC_ALL/LC_MESSAGES）自动检测。
fn init_i18n(cfg: &config::Config) {
    let lang = cfg
        .config
        .language
        .as_deref()
        .and_then(i18n::Lang::from_str)
        .unwrap_or_else(i18n::detect_locale);
    i18n::init(lang);
}

/// 并发运行所有 monitor 的检查与更新流程（upgrade / daemon 共用）。
///
/// 流程：
/// 1. 加载状态文件（记录每个 monitor 的上次检查时间和当前 tag）
/// 2. 用 Mutex 包裹状态文件以支持多任务并发安全写入
/// 3. 按 `concurrency` 并发执行 `check_and_update`
/// 4. 汇总结果，将最新状态写回磁盘
async fn run_all_monitors(cfg: &config::Config) {
    // 加载状态文件；失败则直接返回，不执行任何更新
    let status = match status::StatusFile::load(&cfg.config.status) {
        Ok(s) => s,
        Err(e) => {
            error!("{}: {}", t!("Failed to load status file", "无法加载状态文件"), e);
            return;
        }
    };

    let status = Arc::new(Mutex::new(status));
    let cfg = Arc::new(cfg);
    let concurrency = cfg.config.concurrency.max(1);
    let monitors: Vec<_> = cfg.monitors.iter().collect();

    // 并发执行：每个 monitor 在执行期间持有 status 锁，确保串行写入状态
    // 注意：锁的范围被限制在 check_and_update 调用期间，
    //       实际上会导致并发退化为串行（因状态锁竞争）。这是为保证状态一致性。
    let results: Vec<_> = futures_util::stream::iter(monitors)
        .map(|(name, monitor)| {
            let name = name.clone();
            let cfg = Arc::clone(&cfg);
            let status = Arc::clone(&status);
            async move {
                let result = {
                    let mut s = status.lock().await;
                    monitor::check_and_update(&name, monitor, &cfg.config, &mut s).await
                };
                (name, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    // 尝试从 Arc 中取出 Mutex：若唯一引用则直接取出，否则阻塞等待锁后克隆
    let mut status = match Arc::try_unwrap(status) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => {
            let guard = arc.blocking_lock();
            guard.clone()
        }
    };

    // 汇总每个 monitor 的结果：
    // - Ok(true)  ：更新成功
    // - Ok(false) ：无需更新（间隔未到 / 已是最新版本）
    // - Err(e)    ：出错，仅更新检查时间并保存
    for (name, result) in results {
        match result {
            Ok(true) => info!("[{}] {}", name, t!("Update completed successfully", "更新成功完成")),
            Ok(false) => {}
            Err(e) => {
                error!("[{}] {}: {:#}", name, t!("Error", "错误"), e);
                // 出错时也记录本次检查时间，避免下一轮立即重试
                status.update_check(&name);
                if let Err(save_err) = status.save(&cfg.config.status) {
                    error!("[{}] {}: {}", name, t!("Failed to save status after error", "错误后保存状态失败"), save_err);
                }
            }
        }
    }

    // 最终将状态文件写回磁盘（原子写入：先写 .tmp 再 rename）
    if let Err(save_err) = status.save(&cfg.config.status) {
        error!("{}: {}", t!("Failed to save status", "保存状态失败"), save_err);
    }
}
