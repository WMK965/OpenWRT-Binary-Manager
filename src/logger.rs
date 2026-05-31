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

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let log_file = Mutex::new(log_file);

    Builder::new()
        .filter_level(LevelFilter::Info)
        .format(move |buf, record| {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let line = format!(
                "[{}] [{}] {}\n",
                timestamp,
                record.level(),
                record.args()
            );

            // 写入 stderr
            let _ = buf.write_all(line.as_bytes());

            // 写入日志文件
            if let Ok(mut file) = log_file.lock() {
                let _ = file.write_all(line.as_bytes());
            }

            Ok(())
        })
        .init();

    Ok(())
}
