use anyhow::{Context, Result};
use chrono::Local;
use log::{info, warn};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::BackupConfig;
use crate::t;

pub fn backup_file(file_path: &Path, monitor_name: &str, config: &BackupConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if !file_path.exists() {
        warn!(
            "[{}] {}: {}",
            monitor_name,
            t!("Target file does not exist, skipping backup", "目标文件不存在, 跳过备份"),
            file_path.display()
        );
        return Ok(());
    }

    fs::create_dir_all(&config.dir).context("failed to create backup directory")?;

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{}_{}.zip", file_name, timestamp);
    let backup_path = config.dir.join(&backup_name);

    create_zip_backup(file_path, file_name, &backup_path)?;

    info!(
        "[{}] {}: {}",
        monitor_name,
        t!("Backup created", "备份已创建"),
        backup_path.display()
    );

    rotate_backups(&config.dir, file_name, config.count, monitor_name)?;

    Ok(())
}

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

fn rotate_backups(
    backup_dir: &Path,
    file_name: &str,
    count: usize,
    monitor_name: &str,
) -> Result<()> {
    let prefix = format!("{}_", file_name);
    let mut backups: Vec<PathBuf> = fs::read_dir(backup_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(&prefix) && name.ends_with(".zip")
        })
        .map(|e| e.path())
        .collect();

    backups.sort();

    if backups.len() > count {
        let to_remove = backups.len() - count;
        for path in backups.iter().take(to_remove) {
            info!(
                "[{}] {}: {}",
                monitor_name,
                t!("Removing old backup", "删除旧备份"),
                path.display()
            );
            if let Err(e) = fs::remove_file(path) {
                warn!(
                    "[{}] {} {}: {}",
                    monitor_name,
                    t!("Failed to remove old backup", "删除旧备份失败"),
                    path.display(),
                    e
                );
            }
        }
    }

    Ok(())
}
