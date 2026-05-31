use anyhow::{Context, Result};
use chrono::Local;
use log::{info, warn};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::BackupConfig;

/// 备份指定文件
///
/// 将文件打包为 zip（防止被意外执行），文件名格式：{原文件名}_{YYYYMMDD_HHmmss}.zip
/// 备份完成后自动轮转，删除超出 count 的最旧备份
pub fn backup_file(file_path: &Path, monitor_name: &str, config: &BackupConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if !file_path.exists() {
        warn!(
            "[{}] Target file does not exist, skipping backup: {}",
            monitor_name,
            file_path.display()
        );
        return Ok(());
    }

    // 确保备份目录存在
    fs::create_dir_all(&config.dir)
        .context("failed to create backup directory")?;

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{}_{}.zip", file_name, timestamp);
    let backup_path = config.dir.join(&backup_name);

    // 将文件打包为 zip
    create_zip_backup(file_path, file_name, &backup_path)?;

    info!(
        "[{}] Backup created: {}",
        monitor_name,
        backup_path.display()
    );

    // 轮转：删除超出 count 的旧备份
    rotate_backups(&config.dir, file_name, config.count, monitor_name)?;

    Ok(())
}

/// 将单个文件打包为 zip
fn create_zip_backup(source: &Path, entry_name: &str, zip_path: &Path) -> Result<()> {
    let zip_file = fs::File::create(zip_path).context("failed to create backup zip file")?;
    let mut zip_writer = zip::ZipWriter::new(zip_file);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip_writer
        .start_file(entry_name, options)
        .context("failed to start zip entry")?;

    let mut source_file = fs::File::open(source).context("failed to open source file for backup")?;
    let mut buffer = Vec::new();
    source_file
        .read_to_end(&mut buffer)
        .context("failed to read source file")?;
    zip_writer
        .write_all(&buffer)
        .context("failed to write to zip")?;

    zip_writer.finish().context("failed to finalize zip")?;
    Ok(())
}

/// 轮转备份：保留最新的 count 份，删除多余的
fn rotate_backups(
    backup_dir: &Path,
    file_name: &str,
    count: usize,
    monitor_name: &str,
) -> Result<()> {
    // 列出匹配的备份文件
    let prefix = format!("{}_", file_name);
    let mut backups: Vec<PathBuf> = fs::read_dir(backup_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(&prefix) && name.ends_with(".zip")
        })
        .map(|e| e.path())
        .collect();

    // 按文件名排序（因为包含时间戳，字典序即时间序）
    backups.sort();

    // 删除多余的（保留最新的 count 份）
    if backups.len() > count {
        let to_remove = backups.len() - count;
        for path in backups.iter().take(to_remove) {
            info!(
                "[{}] Removing old backup: {}",
                monitor_name,
                path.display()
            );
            if let Err(e) = fs::remove_file(path) {
                warn!(
                    "[{}] Failed to remove old backup {}: {}",
                    monitor_name,
                    path.display(),
                    e
                );
            }
        }
    }

    Ok(())
}
