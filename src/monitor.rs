use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use log::{info, warn};
use regex::Regex;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::archive;
use crate::backup;
use crate::config::{GlobalConfig, MonitorConfig};
use crate::github;
use crate::status::StatusFile;
use crate::t;

pub async fn check_and_update(
    name: &str,
    monitor: &MonitorConfig,
    global: &GlobalConfig,
    status: &mut StatusFile,
) -> Result<bool> {
    if let Some(mon_status) = status.get(name) {
        let elapsed = Utc::now()
            .signed_duration_since(mon_status.last_check)
            .to_std()
            .unwrap_or_default();
        if elapsed < monitor.interval {
            let remaining = monitor.interval - elapsed;
            info!(
                "[{}] {} ({:.0?})",
                name,
                t!("Interval not reached, next check in", "检查间隔未到, 下次检查在"),
                remaining
            );
            return Ok(false);
        }
    }

    info!("[{}] {}", name, t!("Checking for updates...", "检查更新中..."));

    let api_client = github::build_client(&monitor.proxy, &global.token)?;
    let release = github::get_release(&api_client, &monitor.repo, &monitor.release_type).await?;
    info!(
        "[{}] {}: {} (prerelease: {})",
        name,
        t!("Latest release", "最新发布"),
        release.tag_name,
        release.prerelease
    );

    let asset = github::find_matching_asset(&release, &monitor.regex)?;
    info!(
        "[{}] {}: {} ({} bytes)",
        name,
        t!("Matched asset", "匹配到资源"),
        asset.name,
        asset.size
    );

    if let Some(vc) = &monitor.version_check {
        if let Some(local_version) = detect_local_version(name, vc) {
            let remote_version = normalize_version(&release.tag_name);
            if local_version == remote_version {
                info!(
                    "[{}] {} (local: {}, remote: {})",
                    name,
                    t!("Already up to date", "已是最新版本"),
                    local_version,
                    remote_version
                );
                status.update_check(name);
                status.save(&global.status)?;
                return Ok(false);
            }
            info!(
                "[{}] {}: {} -> {}",
                name,
                t!("Update available", "发现新版本"),
                local_version,
                remote_version
            );
        }
    }

    if let Some(mon_status) = status.get(name) {
        if mon_status.current_tag.as_deref() == Some(&release.tag_name) {
            info!(
                "[{}] {} (tag: {})",
                name,
                t!("Already up to date", "已是最新版本"),
                release.tag_name
            );
            status.update_check(name);
            status.save(&global.status)?;
            return Ok(false);
        }
        info!(
            "[{}] {}: {} -> {}",
            name,
            t!("Update available", "发现新版本"),
            mon_status.current_tag.as_deref().unwrap_or("(none)"),
            release.tag_name
        );
    } else {
        info!(
            "[{}] {}: {}",
            name,
            t!("First run, will install tag", "首次运行, 将安装版本"),
            release.tag_name
        );
    }

    let download_url =
        github::resolve_download_url(&asset.browser_download_url, &monitor.proxy);
    let download_dest = global.working_dir.join(&asset.name);

    let download_client = github::build_download_client(&monitor.proxy, &global.token)?;
    github::download_asset(&download_client, &download_url, &download_dest).await?;

    let extract_dir = global.working_dir.join(format!("{}_extract", name));
    fs::create_dir_all(&extract_dir)?;

    let resolved_path = monitor
        .extract_path
        .as_ref()
        .map(|p| archive::resolve_extract_path(p, &release.tag_name));
    let binary_path =
        archive::extract_if_archive(&download_dest, &extract_dir, &resolved_path)?;

    if let Some(script) = &monitor.pre_update {
        info!("[{}] {}: {}", name, t!("Running pre_update", "执行 pre_update"), script);
        run_script(name, "pre_update", script)?;
    }

    if let Some(backup_config) = &monitor.backup {
        backup::backup_file(&monitor.file, name, backup_config)?;
    }

    if let Some(parent) = monitor.file.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(&binary_path, &monitor.file).context(format!(
        "failed to copy binary to {}",
        monitor.file.display()
    ))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&monitor.file, perms)?;
    }

    info!(
        "[{}] {}: {} (tag: {})",
        name,
        t!("Updated binary", "已更新二进制"),
        monitor.file.display(),
        release.tag_name
    );

    if let Some(script) = &monitor.post_update {
        info!("[{}] {}: {}", name, t!("Running post_update", "执行 post_update"), script);
        if let Err(e) = run_script(name, "post_update", script) {
            warn!("[{}] {}: {}", name, t!("post_update failed (non-fatal)", "post_update 执行失败 (非致命)"), e);
        }
    }

    status.update_tag(name, &release.tag_name);
    status.save(&global.status)?;

    cleanup_temp(name, &download_dest, &extract_dir);

    Ok(true)
}

pub async fn check_only(
    name: &str,
    monitor: &MonitorConfig,
    global: &GlobalConfig,
    status: &StatusFile,
) -> Result<Vec<String>> {
    let mut report: Vec<String> = Vec::new();

    report.push(format!("{}: {}", t!("Target", "目标文件"), monitor.file.display()));
    report.push(format!("{}: {}", t!("Repo", "仓库"), monitor.repo));
    report.push(format!(
        "{}: {:?}",
        t!("Release type", "发布类型"),
        monitor.release_type
    ));

    if let Some(mon_status) = status.get(name) {
        let elapsed = Utc::now()
            .signed_duration_since(mon_status.last_check)
            .to_std()
            .unwrap_or_default();
        if elapsed < monitor.interval {
            let remaining = monitor.interval - elapsed;
            report.push(format!(
                "{} ({:.0?})",
                t!("Interval not reached, next check in", "检查间隔未到, 下次检查在"),
                remaining
            ));
        }
    }

    if let Some(mon_status) = status.get(name) {
        if let Some(ref tag) = mon_status.current_tag {
            report.push(format!("{}: {}", t!("Current known tag", "当前已知版本"), tag));
        }
    }

    let api_client = github::build_client(&monitor.proxy, &global.token)?;
    let release = github::get_release(&api_client, &monitor.repo, &monitor.release_type).await?;

    report.push(format!(
        "{}: {} (prerelease: {})",
        t!("Latest release", "最新发布"),
        release.tag_name,
        release.prerelease
    ));

    let _asset = github::find_matching_asset(&release, &monitor.regex)?;

    if let Some(vc) = &monitor.version_check {
        if let Some(local_version) = detect_local_version(name, vc) {
            let remote_version = normalize_version(&release.tag_name);
            report.push(format!("{}: {}", t!("Local version", "本地版本"), local_version));
            report.push(format!("{}: {}", t!("Remote version", "远程版本"), remote_version));
            if local_version == remote_version {
                report.push(format!("{}: {}", t!("Status", "状态"), t!("up to date", "已是最新")));
                return Ok(report);
            }
            report.push(format!(
                "{}: {} ({} -> {})",
                t!("Status", "状态"),
                t!("update available", "有新版本"),
                local_version,
                remote_version
            ));
            return Ok(report);
        }
    }

    if let Some(mon_status) = status.get(name) {
        if mon_status.current_tag.as_deref() == Some(&release.tag_name) {
            report.push(format!(
                "{}: {} (tag: {})",
                t!("Status", "状态"),
                t!("up to date", "已是最新"),
                release.tag_name
            ));
        } else {
            report.push(format!(
                "{}: {} ({} -> {})",
                t!("Status", "状态"),
                t!("update available", "有新版本"),
                mon_status.current_tag.as_deref().unwrap_or("(none)"),
                release.tag_name
            ));
        }
    } else {
        report.push(format!(
            "{}: {} (tag: {})",
            t!("Status", "状态"),
            t!("first install", "首次安装"),
            release.tag_name
        ));
    }

    Ok(report)
}

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

fn cleanup_temp(name: &str, download_path: &Path, extract_dir: &Path) {
    if download_path.exists() {
        if let Err(e) = fs::remove_file(download_path) {
            warn!(
                "[{}] {} {}: {}",
                name,
                t!("Failed to clean up download file", "清理下载文件失败"),
                download_path.display(),
                e
            );
        }
    }
    if extract_dir.exists() {
        if let Err(e) = fs::remove_dir_all(extract_dir) {
            warn!(
                "[{}] {} {}: {}",
                name,
                t!("Failed to clean up extract dir", "清理解压目录失败"),
                extract_dir.display(),
                e
            );
        }
    }
}

fn detect_local_version(name: &str, vc: &crate::config::VersionCheckConfig) -> Option<String> {
    let output = match Command::new("sh")
        .arg("-c")
        .arg(&vc.command)
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            warn!(
                "[{}] {}: {}",
                name,
                t!("version_check command failed", "version_check 命令执行失败"),
                e
            );
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        warn!(
            "[{}] {} {:?}, stderr: {}",
            name,
            t!("version_check exited with", "version_check 退出码"),
            output.status.code(),
            stderr.trim()
        );
    }

    let re = match Regex::new(&vc.regex) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "[{}] {}: {}",
                name,
                t!("version_check regex compile error", "version_check 正则编译错误"),
                e
            );
            return None;
        }
    };

    match re.captures(&stdout) {
        Some(caps) => match caps.get(1) {
            Some(m) => {
                let version = m.as_str().trim().to_string();
                info!(
                    "[{}] {}: {}",
                    name,
                    t!("version_check detected local version", "version_check 检测到本地版本"),
                    version
                );
                Some(version)
            }
            None => {
                warn!(
                    "[{}] {}: {}",
                    name,
                    t!("version_check regex matched but no capture group found in output", "version_check 正则已匹配但未找到捕获组"),
                    stdout.trim()
                );
                None
            }
        },
        None => {
            warn!(
                "[{}] {} '{}' {}: {}",
                name,
                t!("version_check regex", "version_check 正则"),
                vc.regex,
                t!("did not match output", "未匹配输出"),
                stdout.trim()
            );
            None
        }
    }
}

fn normalize_version(version: &str) -> String {
    version
        .trim()
        .strip_prefix('v')
        .or_else(|| version.trim().strip_prefix('V'))
        .unwrap_or(version.trim())
        .to_string()
}
