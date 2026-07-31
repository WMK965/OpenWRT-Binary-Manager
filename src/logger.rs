// logger.rs ── 日志系统初始化
//
// 基于 env_logger，自定义格式同时输出到：
// 1. 日志文件（append 模式，持久化保存）
// 2. stderr（便于守护进程查看实时输出）

use anyhow::Result;
use env_logger::Builder;
use log::LevelFilter;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// 初始化日志系统
///
/// - 输出到日志文件（append 模式）
/// - 同时输出到 stderr
/// - 格式: [2024-01-01 12:00:00] [INFO] message
pub fn init_logger(log_path: &Path) -> Result<()> {
    // 确保日志目录存在
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // 以追加模式打开日志文件（不存在则创建）
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    // 用 Mutex 包裹文件句柄，保证多线程并发写入时不交错
    let log_file = Mutex::new(log_file);

    Builder::new()
        .filter_level(LevelFilter::Info) // 日志级别：INFO 及以上
        .format(move |buf, record| {
            // 时间戳使用本地时区，格式 YYYY-MM-DD HH:MM:SS
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let line = format!(
                "[{}] [{}] {}\n",
                timestamp,
                record.level(),
                record.args()
            );

            // 写入 stderr（守护进程下通常被重定向或丢弃）
            let _ = buf.write_all(line.as_bytes());

            // 写入日志文件（加锁保证线程安全）
            if let Ok(mut file) = log_file.lock() {
                let _ = file.write_all(line.as_bytes());
            }

            Ok(())
        })
        .init();

    Ok(())
}
