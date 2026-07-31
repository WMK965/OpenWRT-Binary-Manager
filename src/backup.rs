// backup.rs ── 备份与故障保护模块
//
// 提供两类功能：
// 1. 历史备份：每次更新前创建带时间戳的 zip 备份，并按保留份数轮转清理旧备份
// 2. 故障保护（failsafe）：替换前保存原二进制副本，更新失败时自动恢复
//
// 备份目录结构：
//   {backup_dir}/
//   ├── {monitor_name}/
//   │   ├── failsafe/
//   │   │   └── {binary}              # 故障保护副本
//   │   ├── {binary}_20260531_0800.zip # 历史备份
//   │   └── {binary}_20260530_1200.zip
//
// 说明：历史备份打包为 zip 是为了防止备份文件被意外当作可执行文件运行。

use anyhow::{Context, Result};
use chrono::Local;
use log::{info, warn};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::t;

/// 创建带时间戳的 zip 历史备份
///
/// 将 `file_path` 打包为 `{backup_dir}/{monitor_name}/{file_name}_{timestamp}.zip`，
/// 并按 `count` 保留最新份数，超出的旧备份会被删除。
///
/// 若目标文件不存在（首次安装），则跳过备份并记录警告。
pub fn backup_file(
    file_path: &Path,
    monitor_name: &str,
    backup_dir: &Path,
    count: usize,
) -> Result<()> {
    // 目标文件不存在时跳过（如首次安装场景）
    if !file_path.exists() {
        warn!(
            "[{}] {}: {}",
            monitor_name,
            t!("Target file does not exist, skipping backup", "目标文件不存在, 跳过备份"),
            file_path.display()
        );
        return Ok(());
    }

    // 创建 monitor 专属备份子目录
    let mon_dir = backup_dir.join(monitor_name);
    fs::create_dir_all(&mon_dir).context("failed to create backup directory")?;

    // 提取文件名（不含路径），用于生成备份文件名
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // 生成带时间戳的备份文件名：{file_name}_{YYYYMMDD_HHMMSS}.zip
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{}_{}.zip", file_name, timestamp);
    let backup_path = mon_dir.join(&backup_name);

    // 执行 zip 打包
    create_zip_backup(file_path, file_name, &backup_path)?;

    info!(
        "[{}] {}: {}",
        monitor_name,
        t!("Backup created", "备份已创建"),
        backup_path.display()
    );

    // 按保留份数轮转清理旧备份
    rotate_backups(&mon_dir, file_name, count, monitor_name)?;

    Ok(())
}

/// 保存故障保护副本（替换前的原二进制）
///
/// 将当前二进制直接复制到 `{backup_dir}/{monitor_name}/failsafe/{file_name}`。
/// 与历史备份不同，故障保护副本不压缩，以便恢复时快速操作。
///
/// 若目标文件不存在（首次安装），则跳过。
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

    // 直接复制原文件作为故障保护副本
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

/// 从故障保护副本恢复二进制
///
/// 将 `{backup_dir}/{monitor_name}/failsafe/{file_name}` 复制回原路径，
/// 并设置权限为 0o755（Unix 下）。
///
/// 若故障保护副本不存在则返回错误。
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

    // 将故障保护副本复制回目标路径
    fs::copy(&failsafe_path, file_path).context("failed to restore from failsafe")?;

    // 恢复后设置可执行权限（Unix 专属）
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

/// 更新成功后清理故障保护副本
///
/// 删除 `{backup_dir}/{monitor_name}/failsafe/{file_name}`，
/// 并在副本目录为空时尝试删除该目录。
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
            // 副本删除成功后，尝试删除空的 failsafe 目录
            let failsafe_dir = failsafe_path.parent().unwrap();
            let _ = fs::remove_dir(failsafe_dir);
        }
    }
}

/// 将单个文件打包为 zip
///
/// `entry_name` 为 zip 内的条目名（通常为原文件名），
/// 使用 Deflated 压缩方法。
fn create_zip_backup(source: &Path, entry_name: &str, zip_path: &Path) -> Result<()> {
    let zip_file = fs::File::create(zip_path).context("failed to create backup zip file")?;
    let mut zip_writer = zip::ZipWriter::new(zip_file);

    // 压缩选项：使用 Deflated 压缩
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // 在 zip 中创建一个新条目
    zip_writer
        .start_file(entry_name, options)
        .context("failed to start zip entry")?;

    // 读取源文件内容并写入 zip
    let mut source_file = fs::File::open(source).context("failed to open source file for backup")?;
    let mut buffer = Vec::new();
    source_file
        .read_to_end(&mut buffer)
        .context("failed to read source file")?;
    zip_writer
        .write_all(&buffer)
        .context("failed to write to zip")?;

    // 完成 zip 写入
    zip_writer.finish().context("failed to finalize zip")?;
    Ok(())
}

/// 备份轮转：保留最新 `count` 份备份，删除多余的旧备份
///
/// 通过文件名前缀 `{file_name}_` 和 `.zip` 后缀筛选历史备份，
/// 按文件名排序后删除最早的若干份。
fn rotate_backups(
    backup_dir: &Path,
    file_name: &str,
    count: usize,
    monitor_name: &str,
) -> Result<()> {
    // 筛选条件：以 "{file_name}_" 开头且以 ".zip" 结尾
    let prefix = format!("{}_", file_name);
    let mut backups: Vec<PathBuf> = fs::read_dir(backup_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(&prefix) && name.ends_with(".zip")
        })
        .map(|e| e.path())
        .collect();

    // 按文件名排序（时间戳在文件名中，字典序即时间序）
    backups.sort();

    // 超出保留份数时，删除最早的若干份
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
