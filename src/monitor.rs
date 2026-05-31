use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use log::{info, warn};
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::archive;
use crate::backup;
use crate::config::{GlobalConfig, MonitorConfig};
use crate::github;
use crate::status::StatusFile;

/// 检查并更新单个 monitor
///
/// 返回 Ok(true) 表示执行了更新，Ok(false) 表示跳过（间隔未到/无更新）
pub async fn check_and_update(
    name: &str,
    monitor: &MonitorConfig,
    global: &GlobalConfig,
    status: &mut StatusFile,
) -> Result<bool> {
    // 1. 检查 interval 是否已到
    if let Some(mon_status) = status.get(name) {
        let elapsed = Utc::now()
            .signed_duration_since(mon_status.last_check)
            .to_std()
            .unwrap_or_default();
        if elapsed < monitor.interval {
            let remaining = monitor.interval - elapsed;
            info!(
                "[{}] Interval not reached, next check in {:.0?}",
                name, remaining
            );
            return Ok(false);
        }
    }

    info!("[{}] Checking for updates...", name);

    // 2. 构建 API client
    let api_client = github::build_client(&monitor.proxy, &global.token)?;

    // 3. 获取 release
    let release = github::get_release(&api_client, &monitor.repo, &monitor.release_type).await?;
    info!(
        "[{}] Latest release: {} (prerelease: {})",
        name, release.tag_name, release.prerelease
    );

    // 4. 匹配 asset
    let asset = github::find_matching_asset(&release, &monitor.regex)?;
    info!(
        "[{}] Matched asset: {} ({} bytes)",
        name, asset.name, asset.size
    );

    // 5. 比较 tag
    if let Some(mon_status) = status.get(name) {
        if mon_status.current_tag.as_deref() == Some(&release.tag_name) {
            info!("[{}] Already up to date (tag: {})", name, release.tag_name);
            status.update_check(name);
            status.save(&global.status)?;
            return Ok(false);
        }
        info!(
            "[{}] Update available: {} -> {}",
            name,
            mon_status.current_tag.as_deref().unwrap_or("(none)"),
            release.tag_name
        );
    } else {
        info!(
            "[{}] First run, will install tag: {}",
            name, release.tag_name
        );
    }

    // 6. 下载 asset 到 working_dir
    let download_url =
        github::resolve_download_url(&asset.browser_download_url, &monitor.proxy);
    let download_dest = global.working_dir.join(&asset.name);

    let download_client = github::build_download_client(&monitor.proxy, &global.token)?;
    github::download_asset(&download_client, &download_url, &download_dest).await?;

    // 7. 解压（如果是存档）
    let extract_dir = global.working_dir.join(format!("{}_extract", name));
    fs::create_dir_all(&extract_dir)?;

    let binary_path =
        archive::extract_if_archive(&download_dest, &extract_dir, &monitor.extract_path)?;

    // 8. 执行 pre_update 脚本
    if let Some(script) = &monitor.pre_update {
        info!("[{}] Running pre_update: {}", name, script);
        run_script(name, "pre_update", script)?;
    }

    // 9. 备份旧文件
    if let Some(backup_config) = &monitor.backup {
        backup::backup_file(&monitor.file, name, backup_config)?;
    }

    // 10. 替换目标文件
    // 确保目标目录存在
    if let Some(parent) = monitor.file.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(&binary_path, &monitor.file).context(format!(
        "failed to copy binary to {}",
        monitor.file.display()
    ))?;

    // 设置可执行权限 (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&monitor.file, perms)?;
    }

    info!(
        "[{}] Updated binary: {} (tag: {})",
        name,
        monitor.file.display(),
        release.tag_name
    );

    // 11. 执行 post_update 脚本
    if let Some(script) = &monitor.post_update {
        info!("[{}] Running post_update: {}", name, script);
        if let Err(e) = run_script(name, "post_update", script) {
            warn!("[{}] post_update failed (non-fatal): {}", name, e);
        }
    }

    // 12. 更新 status
    status.update_tag(name, &release.tag_name);
    status.save(&global.status)?;

    // 13. 清理临时文件
    cleanup_temp(name, &download_dest, &extract_dir);

    Ok(true)
}

/// 执行 shell 脚本，捕获输出
fn run_script(monitor_name: &str, stage: &str, script: &str) -> Result<()> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .context(format!("failed to execute {} script", stage))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.is_empty() {
        info!("[{}] {} stdout: {}", monitor_name, stage, stdout.trim());
    }
    if !stderr.is_empty() {
        warn!("[{}] {} stderr: {}", monitor_name, stage, stderr.trim());
    }

    if !output.status.success() {
        return Err(anyhow!(
            "{} script exited with code {:?}",
            stage,
            output.status.code()
        ));
    }

    Ok(())
}

/// 清理临时文件
fn cleanup_temp(name: &str, download_path: &Path, extract_dir: &Path) {
    if download_path.exists() {
        if let Err(e) = fs::remove_file(download_path) {
            warn!(
                "[{}] Failed to clean up download file {}: {}",
                name,
                download_path.display(),
                e
            );
        }
    }
    if extract_dir.exists() {
        if let Err(e) = fs::remove_dir_all(extract_dir) {
            warn!(
                "[{}] Failed to clean up extract dir {}: {}",
                name,
                extract_dir.display(),
                e
            );
        }
    }
}
