use anyhow::{Context, Result};
use chrono::Local;
use log::{info, warn};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::t;

/// Create a timestamped zip backup of a file under `backup_dir/{monitor_name}/`.
pub fn backup_file(
    file_path: &Path,
    monitor_name: &str,
    backup_dir: &Path,
    count: usize,
) -> Result<()> {
    if !file_path.exists() {
        warn!(
            "[{}] {}: {}",
            monitor_name,
            t!("Target file does not exist, skipping backup", "目标文件不存在, 跳过备份"),
            file_path.display()
        );
        return Ok(());
    }

    let mon_dir = backup_dir.join(monitor_name);
    fs::create_dir_all(&mon_dir).context("failed to create backup directory")?;

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{}_{}.zip", file_name, timestamp);
    let backup_path = mon_dir.join(&backup_name);

    create_zip_backup(file_path, file_name, &backup_path)?;

    info!(
        "[{}] {}: {}",
        monitor_name,
        t!("Backup created", "备份已创建"),
        backup_path.display()
    );

    rotate_backups(&mon_dir, file_name, count, monitor_name)?;

    Ok(())
}

/// Save a failsafe copy of the current binary before replacement.
pub fn save_failsafe(file_path: &Path, monitor_name: &str, backup_dir: &Path) -> Result<()> {
    if !file_path.exists() {
        return Ok(());
    }

    let failsafe_dir = backup_dir.join(monitor_name).join("failsafe");
    fs::create_dir_all(&failsafe_dir).context("failed to create failsafe directory")?;

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary");

    let failsafe_path = failsafe_dir.join(file_name);
    fs::copy(file_path, &failsafe_path)
        .context("failed to save failsafe backup")?;

    info!(
        "[{}] {}: {}",
        monitor_name,
        t!("Failsafe saved", "故障保护副本已保存"),
        failsafe_path.display()
    );

    Ok(())
}

/// Restore the binary from the failsafe copy.
pub fn restore_failsafe(file_path: &Path, monitor_name: &str, backup_dir: &Path) -> Result<()> {
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary");

    let failsafe_path = backup_dir
        .join(monitor_name)
        .join("failsafe")
        .join(file_name);

    if !failsafe_path.exists() {
        return Err(anyhow::anyhow!(
            "failsafe backup not found at {}",
            failsafe_path.display()
        ));
    }

    fs::copy(&failsafe_path, file_path).context("failed to restore from failsafe")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        let _ = fs::set_permissions(file_path, perms);
    }

    info!(
        "[{}] {}: {} -> {}",
        monitor_name,
        t!("Restored from failsafe", "已从故障保护恢复"),
        failsafe_path.display(),
        file_path.display()
    );

    Ok(())
}

/// Remove the failsafe copy after a successful update.
pub fn cleanup_failsafe(file_path: &Path, monitor_name: &str, backup_dir: &Path) {
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary");

    let failsafe_path = backup_dir
        .join(monitor_name)
        .join("failsafe")
        .join(file_name);

    if failsafe_path.exists() {
        if let Err(e) = fs::remove_file(&failsafe_path) {
            warn!(
                "[{}] {} {}: {}",
                monitor_name,
                t!("Failed to clean up failsafe", "清理故障保护文件失败"),
                failsafe_path.display(),
                e
            );
        } else {
            let failsafe_dir = failsafe_path.parent().unwrap();
            let _ = fs::remove_dir(failsafe_dir);
        }
    }
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
