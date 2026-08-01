// monitor.rs ── 核心：检查更新与执行更新逻辑
//
// 本模块是整个程序的核心，包含两个主要入口：
// - check_and_update : 完整更新流程（下载、备份、替换、校验）
// - check_only       : 仅检测模式（dry-run，不修改任何文件）
//
// 更新流程（check_and_update）：
//   1. 检查间隔是否已到（基于状态文件的 last_check）
//   2. 查询 GitHub Release 并正则匹配目标 asset
//   3. 版本比对（若配置了 version_check，则执行命令获取本地版本）
//   4. 与状态文件中的 current_tag 比对（无 version_check 时的备用方案）
//   5. 下载 asset 并解压
//   6. 执行 pre_update 脚本
//   7. 保存故障保护副本 + 历史备份
//   8. 替换目标二进制文件并设置权限
//   9. 校验新二进制（若开启 failsafe 且配置了 version_check）
//  10. 校验失败则恢复故障保护副本；成功则清理故障保护副本
//  11. 执行 post_update 脚本
//  12. 更新状态文件并清理临时文件

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use log::{error, info, warn};
use regex::Regex;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::archive;
use crate::backup;
use crate::config::{FailsafeMode, GlobalConfig, MonitorConfig};
use crate::github;
use crate::status::StatusFile;
use crate::t;

/// 检查并执行更新（完整流程）
///
/// 返回值：
/// - `Ok(true)`  ：已执行更新
/// - `Ok(false)` ：未更新（间隔未到 / 已是最新版本）
/// - `Err(e)`    ：更新过程中出错
pub async fn check_and_update(
    name: &str,
    monitor: &MonitorConfig,
    global: &GlobalConfig,
    status: &mut StatusFile,
) -> Result<bool> {
    // ── 步骤 1：检查间隔是否已到 ──────────────────────────────
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

    // ── 步骤 2：查询 GitHub Release ───────────────────────────
    // 构建 API 客户端（含代理/Token 配置），带重试机制获取 Release 信息
    let api_client = github::build_client(&monitor.proxy, &global.token, global.timeout)?;
    let release = github::with_retry(global.retry, || {
        github::get_release(&api_client, &monitor.repo, &monitor.release_type)
    })
    .await?;
    info!(
        "[{}] {}: {} (prerelease: {})",
        name,
        t!("Latest release", "最新发布"),
        release.tag_name,
        release.prerelease
    );

    // 通过正则匹配目标 asset
    let asset = github::find_matching_asset(&release, &monitor.regex)?;
    info!(
        "[{}] {}: {} ({} bytes)",
        name,
        t!("Matched asset", "匹配到资源"),
        asset.name,
        asset.size
    );

    // ── 步骤 3：版本比对（若配置了 version_check） ────────────
    // 通过执行本地命令获取当前二进制版本号，与远程 tag 比对
    if let Some(vc) = &monitor.version_check {
        if let Some(local_version) = detect_local_version(name, vc) {
            // 对远程 tag 进行归一化（去除 v 前缀、strip_prefix 等）
            let remote_version = normalize_version(&release.tag_name, &vc.strip_prefix);
            if local_version == remote_version {
                // 本地版本与远程一致：无需更新
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
            // 发现新版本
            info!(
                "[{}] {}: {} -> {}",
                name,
                t!("Update available", "发现新版本"),
                local_version,
                remote_version
            );
        }
        // 若 detect_local_version 返回 None（命令执行失败等），继续后续流程
    }

    // ── 步骤 4：与状态文件中的 current_tag 比对 ──────────────
    // 这是无 version_check 时的备用判断方案：若上次记录的 tag 与当前远程 tag 一致则跳过
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
        // 状态文件中记录的 tag 与远程不同：需要更新
        info!(
            "[{}] {}: {} -> {}",
            name,
            t!("Update available", "发现新版本"),
            mon_status.current_tag.as_deref().unwrap_or("(none)"),
            release.tag_name
        );
    } else {
        // 首次运行：状态文件中无该 monitor 的记录
        info!(
            "[{}] {}: {}",
            name,
            t!("First run, will install tag", "首次运行, 将安装版本"),
            release.tag_name
        );
    }

    // ── 步骤 5：下载 asset ────────────────────────────────────
    // 解析最终下载 URL（处理 HTTP 镜像前缀）
    let download_url =
        github::resolve_download_url(&asset.browser_download_url, &monitor.proxy);
    let download_dest = global.working_dir.join(&asset.name);

    // 构建下载客户端（使用更长的超时时间），执行下载
    let download_client = github::build_download_client(&monitor.proxy, &global.token, global.download_timeout)?;
    github::download_asset(&download_client, &download_url, &download_dest, global.retry).await?;

    // ── 步骤 6：解压归档 ──────────────────────────────────────
    // 创建解压临时目录
    let extract_dir = global.working_dir.join(format!("{}_extract", name));
    fs::create_dir_all(&extract_dir)?;

    // 解析 extract_path 中的 {tag} / {version} 占位符
    let resolved_path = monitor
        .extract_path
        .as_ref()
        .map(|p| archive::resolve_extract_path(p, &release.tag_name));
    // 执行解压，获取最终二进制文件路径
    let binary_path =
        archive::extract_if_archive(&download_dest, &extract_dir, &resolved_path)?;

    // ── 步骤 7：执行 pre_update 脚本 ──────────────────────────
    // 通常用于停止正在运行的服务，避免文件替换时被占用
    if let Some(script) = &monitor.pre_update {
        info!("[{}] {}: {}", name, t!("Running pre_update", "执行 pre_update"), script);
        // pre_update 失败会中止整个更新流程（返回 Err）
        run_script(name, "pre_update", script)?;
    }

    // ── 步骤 8：保存故障保护副本 ──────────────────────────────
    // 在替换前保存当前二进制副本，以便更新失败时恢复
    // 只要 failsafe 未关闭就会执行（备份目录由全局 backup_dir 提供）
    if monitor.failsafe != FailsafeMode::Off {
        backup::save_failsafe(&monitor.file, name, &global.backup_dir)?;
    }

    // ── 步骤 9：创建历史备份（可选） ──────────────────────────
    // 按 monitor.backup_count 配置的保留份数创建带时间戳的 zip 备份
    if let Some(count) = monitor.backup_count {
        backup::backup_file(&monitor.file, name, &global.backup_dir, count)?;
    }

    // ── 步骤 10：替换目标二进制文件 ───────────────────────────
    // 确保目标目录存在
    if let Some(parent) = monitor.file.parent() {
        fs::create_dir_all(parent)?;
    }

    // 将解压出的二进制复制到目标路径
    fs::copy(&binary_path, &monitor.file).context(format!(
        "failed to copy binary to {}",
        monitor.file.display()
    ))?;

    // 设置可执行权限（Unix 专属，0o755 = rwxr-xr-x）
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

    // ── 步骤 11：校验新二进制（故障保护核心） ────────────────
    // 仅当 failsafe 未关闭且配置了 version_check 时执行校验：
    // 通过执行 version_check.command 检测新二进制是否能正常返回版本号
    let verified = if monitor.failsafe != FailsafeMode::Off {
        if let Some(vc) = &monitor.version_check {
            // 执行版本检测命令，能提取到版本号即视为校验通过
            let ok = detect_local_version(name, vc).is_some();
            if !ok {
                // ── 校验失败：恢复故障保护副本 ──
                error!(
                    "[{}] {}: {}",
                    name,
                    t!("New binary verification failed, restoring backup", "新二进制校验失败, 正在恢复备份"),
                    monitor.file.display()
                );
                if let Err(e) = backup::restore_failsafe(&monitor.file, name, &global.backup_dir) {
                    error!("[{}] {}: {}", name, t!("Failed to restore failsafe", "故障保护恢复失败"), e);
                }
                // allow_post 模式：恢复后仍执行 post_update 脚本以重启服务
                if monitor.failsafe == FailsafeMode::AllowPost {
                    if let Some(script) = &monitor.post_update {
                        info!("[{}] {}: {} (allow_post)", name, t!("Running post_update", "执行 post_update"), script);
                        if let Err(e) = run_script(name, "post_update", script) {
                            warn!("[{}] {}: {}", name, t!("post_update failed (non-fatal)", "post_update 执行失败 (非致命)"), e);
                        }
                    }
                }
                // 清理临时文件并返回错误
                cleanup_temp(name, &download_dest, &extract_dir);
                return Err(anyhow!(
                    "{}",
                    t!("binary verification failed after update", "更新后二进制校验失败")
                ));
            }
            // ── 校验通过 ──
            info!(
                "[{}] {}",
                name,
                t!("New binary verified successfully", "新二进制校验通过")
            );
            true
        } else {
            // 未配置 version_check：无法校验，返回 false（不清理故障保护副本）
            false
        }
    } else {
        // failsafe 关闭：不校验
        false
    };

    // ── 步骤 12：清理故障保护副本（仅校验通过时） ────────────
    if verified {
        backup::cleanup_failsafe(&monitor.file, name, &global.backup_dir);
    }

    // ── 步骤 13：执行 post_update 脚本 ────────────────────────
    // 通常用于重启服务。post_update 失败仅记录警告，不影响更新结果
    if let Some(script) = &monitor.post_update {
        info!("[{}] {}: {}", name, t!("Running post_update", "执行 post_update"), script);
        if let Err(e) = run_script(name, "post_update", script) {
            warn!("[{}] {}: {}", name, t!("post_update failed (non-fatal)", "post_update 执行失败 (非致命)"), e);
        }
    }

    // ── 步骤 14：更新状态文件并清理临时文件 ──────────────────
    status.update_tag(name, &release.tag_name);
    status.save(&global.status)?;

    cleanup_temp(name, &download_dest, &extract_dir);

    Ok(true)
}

/// 仅检测模式（dry-run）：查询远程 Release 并生成报告，不执行任何修改
///
/// 返回报告行列表，每行是一条状态信息（目标文件、仓库、版本比对结果等）。
pub async fn check_only(
    name: &str,
    monitor: &MonitorConfig,
    global: &GlobalConfig,
    status: &StatusFile,
) -> Result<Vec<String>> {
    let mut report: Vec<String> = Vec::new();

    // 输出基本信息
    report.push(format!("{}: {}", t!("Target", "目标文件"), monitor.file.display()));
    report.push(format!("{}: {}", t!("Repo", "仓库"), monitor.repo));
    report.push(format!(
        "{}: {:?}",
        t!("Release type", "发布类型"),
        monitor.release_type
    ));

    // 检查间隔是否已到
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

    // 输出当前已知版本（来自状态文件）
    if let Some(mon_status) = status.get(name) {
        if let Some(ref tag) = mon_status.current_tag {
            report.push(format!("{}: {}", t!("Current known tag", "当前已知版本"), tag));
        }
    }

    // 查询远程 Release
    let api_client = github::build_client(&monitor.proxy, &global.token, global.timeout)?;
    let release = github::with_retry(global.retry, || {
        github::get_release(&api_client, &monitor.repo, &monitor.release_type)
    })
    .await?;

    report.push(format!(
        "{}: {} (prerelease: {})",
        t!("Latest release", "最新发布"),
        release.tag_name,
        release.prerelease
    ));

    // 验证 asset 是否能匹配（仅查找，不下载）
    let _asset = github::find_matching_asset(&release, &monitor.regex)?;

    // 版本比对（若配置了 version_check）
    if let Some(vc) = &monitor.version_check {
        if let Some(local_version) = detect_local_version(name, vc) {
            let remote_version = normalize_version(&release.tag_name, &vc.strip_prefix);
            report.push(format!("{}: {}", t!("Local version", "本地版本"), local_version));
            report.push(format!("{}: {}", t!("Remote version", "远程版本"), remote_version));
            if local_version == remote_version {
                // 版本一致：已是最新
                report.push(format!("{}: {}", t!("Status", "状态"), t!("up to date", "已是最新")));
                return Ok(report);
            }
            // 发现新版本
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

    // 无 version_check 时，通过状态文件中的 tag 判断
    if let Some(mon_status) = status.get(name) {
        if mon_status.current_tag.as_deref() == Some(&release.tag_name) {
            // tag 一致：已是最新
            report.push(format!(
                "{}: {} (tag: {})",
                t!("Status", "状态"),
                t!("up to date", "已是最新"),
                release.tag_name
            ));
        } else {
            // tag 不一致：有新版本
            report.push(format!(
                "{}: {} ({} -> {})",
                t!("Status", "状态"),
                t!("update available", "有新版本"),
                mon_status.current_tag.as_deref().unwrap_or("(none)"),
                release.tag_name
            ));
        }
    } else {
        // 状态文件中无记录：首次安装
        report.push(format!(
            "{}: {} (tag: {})",
            t!("Status", "状态"),
            t!("first install", "首次安装"),
            release.tag_name
        ));
    }

    Ok(report)
}

/// 执行 shell 脚本（通过 `sh -c`）
///
/// - `monitor_name`：monitor 名称，用于日志标识
/// - `stage`：阶段标识（"pre_update" / "post_update"）
/// - `script`：要执行的脚本内容
///
/// stdout 输出以 info 级别记录，stderr 输出以 warn 级别记录。
/// 退出码非 0 时返回错误（pre_update 会因此中止更新流程）。
fn run_script(monitor_name: &str, stage: &str, script: &str) -> Result<()> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .context(format!("failed to execute {} script", stage))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // 记录 stdout（非空时）
    if !stdout.is_empty() {
        info!("[{}] {} stdout: {}", monitor_name, stage, stdout.trim());
    }
    // 记录 stderr（非空时，以 warn 级别）
    if !stderr.is_empty() {
        warn!("[{}] {} stderr: {}", monitor_name, stage, stderr.trim());
    }

    // 退出码非 0 视为失败
    if !output.status.success() {
        return Err(anyhow!(
            "{} script exited with code {:?}",
            stage,
            output.status.code()
        ));
    }

    Ok(())
}

/// 清理临时下载文件和解压目录
///
/// 在更新完成或失败后调用，删除工作目录下的临时产物。
/// 清理失败仅记录警告，不影响主流程。
fn cleanup_temp(name: &str, download_path: &Path, extract_dir: &Path) {
    // 删除下载的原始文件
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
    // 删除解压目录（递归）
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

/// 检测本地二进制版本号
///
/// 通过执行 `vc.command` 获取输出，再用 `vc.regex` 正则提取版本号。
/// 正则必须包含一个捕获组，提取到的内容即为版本号。
///
/// 返回值：
/// - `Some(version)`：成功提取到版本号
/// - `None`         ：命令执行失败 / 正则未匹配 / 无捕获组
fn detect_local_version(name: &str, vc: &crate::config::VersionCheckConfig) -> Option<String> {
    // 执行版本检测命令
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

    // 命令执行失败（退出码非 0）时记录警告，但仍尝试从 stdout 提取版本号
    if !output.status.success() {
        warn!(
            "[{}] {} {:?}, stderr: {}",
            name,
            t!("version_check exited with", "version_check 退出码"),
            output.status.code(),
            stderr.trim()
        );
    }

    // 编译正则表达式
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

    // 用正则匹配 stdout，提取第一个捕获组作为版本号
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
            // 正则匹配成功但无捕获组
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
        // 正则未匹配
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

/// 归一化远程版本号
///
/// 处理步骤：
/// 1. 去除首尾空白
/// 2. 去除前导 `v` 或 `V`（如 `v1.2.3` -> `1.2.3`）
/// 3. 若配置了 `strip_prefix`，则去除指定前缀（如 `release-1.2.3` -> `1.2.3`）
///
/// 这样可以将远程 tag 转换为与本地版本号可比的格式。
fn normalize_version(version: &str, strip_prefix: &Option<String>) -> String {
    let version = version.trim();
    // 去除前导 v/V
    let version = version.strip_prefix('v').or_else(|| version.strip_prefix('V')).unwrap_or(version);
    // 去除自定义前缀
    if let Some(prefix) = strip_prefix {
        version.strip_prefix(prefix.as_str()).unwrap_or(version).to_string()
    } else {
        version.to_string()
    }
}
