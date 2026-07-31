// github.rs ── GitHub Releases API 交互
//
// 负责：
// - 构建 HTTP 客户端（支持 SOCKS5 代理与 Bearer Token 认证）
// - 查询 GitHub Release（latest / pre-release）
// - 通过正则匹配目标 asset
// - 下载 asset（优先使用系统 curl/wget，回退到 reqwest 流式下载）
// - 请求重试机制

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use log::{debug, info, warn};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::io::Write;
use std::path::Path;

use crate::config::ReleaseType;
use crate::t;

/// GitHub Release 的元数据（从 API 响应反序列化）
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Release {
    /// Release 的 tag 名称（如 v1.2.3）
    pub tag_name: String,
    /// Release 的标题（可选）
    pub name: Option<String>,
    /// 是否为预发布
    pub prerelease: bool,
    /// 该 Release 包含的所有附件资源
    pub assets: Vec<Asset>,
}

/// Release 中的单个附件资源
#[derive(Debug, Deserialize)]
pub struct Asset {
    /// 资源文件名（用于正则匹配）
    pub name: String,
    /// 浏览器下载地址（GitHub 的直链）
    pub browser_download_url: String,
    /// 文件大小（字节）
    pub size: u64,
}

/// 构建 API 请求用的 HTTP 客户端
///
/// - `proxy`：仅识别 `socks5://` 开头的代理；HTTP 镜像前缀不在此处理（在 resolve_download_url 中拼接）
/// - `token`：非空时添加 `Authorization: Bearer <token>` 头部，提升 API 速率限制
/// - `timeout_secs`：请求超时时间
pub fn build_client(proxy: &Option<String>, token: &Option<String>, timeout_secs: u64) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent("openwrt-binary-manager/0.1.0")
        .timeout(std::time::Duration::from_secs(timeout_secs));

    // 仅 socks5 代理走 reqwest 的代理配置
    if let Some(proxy_url) = proxy {
        if proxy_url.starts_with("socks5://") {
            let proxy = reqwest::Proxy::all(proxy_url)
                .context("failed to parse socks5 proxy URL")?;
            builder = builder.proxy(proxy);
        }
    }

    // 配置 GitHub Token 认证（提升速率限制：60 -> 5000 次/小时）
    if let Some(token) = token {
        if !token.is_empty() {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))?,
            );
            builder = builder.default_headers(headers);
        }
    }

    builder.build().context("failed to build HTTP client")
}

/// 构建下载用的 HTTP 客户端
///
/// 与 `build_client` 逻辑相同，区别在于使用更长的超时时间
/// （下载大文件需要更长的时间窗口）。
pub fn build_download_client(proxy: &Option<String>, token: &Option<String>, timeout_secs: u64) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent("openwrt-binary-manager/0.1.0")
        .timeout(std::time::Duration::from_secs(timeout_secs));

    if let Some(proxy_url) = proxy {
        if proxy_url.starts_with("socks5://") {
            let proxy = reqwest::Proxy::all(proxy_url)
                .context("failed to parse socks5 proxy URL")?;
            builder = builder.proxy(proxy);
        }
    }

    if let Some(token) = token {
        if !token.is_empty() {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))?,
            );
            builder = builder.default_headers(headers);
        }
    }

    builder.build().context("failed to build download client")
}

/// 重试执行一个异步操作，最多重试 `retries` 次，每次间隔 1 秒。
///
/// 总尝试次数 = retries + 1（首次 + 重试）。仅在返回 Err 时重试。
pub async fn with_retry<F, Fut, T>(retries: u32, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..=retries {
        // 非首次尝试时，先休眠 1 秒再重试，并打印警告日志
        if attempt > 0 {
            warn!(
                "{} {}/{}: {}",
                t!("Retrying", "重试中"),
                attempt,
                retries,
                last_err.as_ref().unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    // 所有尝试均失败，返回最后一次的错误
    Err(last_err.unwrap())
}

/// 根据 release_type 获取对应的 Release
///
/// - Latest    ：调用 /releases/latest 端点
/// - PreRelease：从 /releases 列表中筛选第一个 prerelease=true 的
pub async fn get_release(
    client: &Client,
    repo: &str,
    release_type: &ReleaseType,
) -> Result<Release> {
    match release_type {
        ReleaseType::Latest => get_latest_release(client, repo).await,
        ReleaseType::PreRelease => get_latest_prerelease(client, repo).await,
    }
}

/// 获取仓库的最新正式版 Release
///
/// 调用 GitHub API：GET /repos/{owner}/{repo}/releases/latest
async fn get_latest_release(client: &Client, repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    debug!("GET {}", url);

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("failed to fetch latest release")?;

    // 非 2xx 状态码视为失败，返回包含状态码和响应体的错误
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub API returned {}: {}", status, body));
    }

    resp.json::<Release>()
        .await
        .context("failed to parse release JSON")
}

/// 获取仓库最新的预发布版 Release
///
/// 调用 GitHub API：GET /repos/{owner}/{repo}/releases?per_page=20
/// 然后从返回列表中找到第一个 prerelease=true 的 Release。
async fn get_latest_prerelease(client: &Client, repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=20", repo);
    debug!("GET {}", url);

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("failed to fetch releases list")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub API returned {}: {}", status, body));
    }

    let releases: Vec<Release> = resp
        .json()
        .await
        .context("failed to parse releases list JSON")?;

    // GitHub API 返回的 release 列表按创建时间倒序排列，
    // 因此第一个 prerelease=true 的即为最新的预发布版本
    releases
        .into_iter()
        .find(|r| r.prerelease)
        .ok_or_else(|| anyhow!("no pre-release found for {}", repo))
}

/// 在 Release 的 assets 中查找匹配正则的第一个资源
///
/// `pattern` 为正则表达式字符串，匹配 asset 的文件名。
/// 若无匹配项则返回错误，并列出所有可用资源名以便调试。
pub fn find_matching_asset<'a>(release: &'a Release, pattern: &str) -> Result<&'a Asset> {
    let re = Regex::new(pattern).context("invalid asset regex")?;

    release
        .assets
        .iter()
        .find(|a| re.is_match(&a.name))
        .ok_or_else(|| {
            // 未匹配时收集所有 asset 名称，方便用户排查正则问题
            let names: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
            anyhow!(
                "{} '{}'. {}: {:?}",
                t!("No asset matched regex", "无资源匹配正则"),
                pattern,
                t!("Available assets", "可用资源"),
                names
            )
        })
}

/// 解析最终下载 URL
///
/// - 若 proxy 是 HTTP 镜像前缀（非 socks5）：将前缀拼接到原始 URL 之前
///   例如 `https://gh-proxy.com/` + `https://github.com/...` -> `https://gh-proxy.com/https://github.com/...`
/// - 若 proxy 是 socks5 或为空：直接返回原始 URL（socks5 代理在客户端层面处理）
pub fn resolve_download_url(original_url: &str, proxy: &Option<String>) -> String {
    match proxy {
        Some(proxy_url) if !proxy_url.is_empty() && !proxy_url.starts_with("socks5://") => {
            // 去除前缀末尾多余的 '/'，避免出现双斜杠
            let prefix = proxy_url.trim_end_matches('/');
            format!("{}/{}", prefix, original_url)
        }
        // socks5 代理或无代理：直接使用原始 URL
        _ => original_url.to_string(),
    }
}

/// 下载 asset 到指定路径
///
/// 下载策略：
/// 1. 优先尝试系统原生的 curl / wget（OpenWrt 上通常已安装，效率更高）
/// 2. 若原生工具不可用或失败，回退到 reqwest 流式下载（带重试）
pub async fn download_asset(
    client: &Client,
    url: &str,
    dest: &Path,
    retries: u32,
) -> Result<()> {
    // 首先尝试原生下载工具（curl / wget）
    if try_native_download(url, dest).is_ok() {
        let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        info!(
            "{}: {} ({} bytes)",
            t!("Download complete", "下载完成"),
            dest.display(),
            size
        );
        return Ok(());
    }

    // 原生工具失败则使用 reqwest 重试下载
    with_retry(retries, || download_with_reqwest(client, url, dest)).await
}

/// 尝试使用系统原生的 curl 或 wget 下载文件
///
/// 依次尝试 curl 和 wget，任一成功即返回。两者均不可用或失败则返回 Err。
/// 此函数适用于 OpenWrt 等资源受限环境，避免 reqwest 下载大文件时的内存开销。
fn try_native_download(url: &str, dest: &Path) -> Result<()> {
    let tools = ["curl", "wget"];
    for tool in &tools {
        // 根据工具类型构造对应的命令行参数
        let (cmd, args): (&str, &[&str]) = match *tool {
            "curl" => ("curl", &["-fSL", "-o", &dest.to_string_lossy(), "--connect-timeout", "30", "--max-time", "600", url]),
            "wget" => ("wget", &["-q", "-O", &dest.to_string_lossy(), "--timeout=30", "--tries=3", url]),
            _ => continue,
        };

        // 执行命令，丢弃 stdout/stderr
        let status = std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            // 命令执行成功且退出码为 0：下载完成
            Ok(s) if s.success() => {
                info!("{}: {} ({})", t!("Downloading via", "通过下载"), tool, url);
                return Ok(());
            }
            // 命令不存在或失败：尝试下一个工具
            _ => continue,
        }
    }
    Err(anyhow!("no native download tool available"))
}

/// 使用 reqwest 流式下载文件
///
/// 以字节流（bytes_stream）方式逐步写入文件，避免大文件占用过多内存。
/// 当服务器提供 Content-Length 时，按 10% 的粒度打印下载进度。
async fn download_with_reqwest(client: &Client, url: &str, dest: &Path) -> Result<()> {
    info!("{}: {}", t!("Downloading", "下载中"), url);

    let resp = client
        .get(url)
        .send()
        .await
        .context("failed to start download")?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(anyhow!("download failed with status {}", status));
    }

    // 获取文件总大小（若服务器提供），用于进度显示
    let total_size = resp.content_length();
    let mut file = std::fs::File::create(dest)
        .context("failed to create download destination file")?;

    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;

    // 逐块读取并写入文件
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading download stream")?;
        file.write_all(&chunk)
            .context("failed to write to destination file")?;
        downloaded += chunk.len() as u64;

        // 每 10% 打印一次进度（仅在已知总大小时）
        if let Some(total) = total_size {
            if total > 0 {
                let pct = (downloaded as f64 / total as f64 * 100.0) as u32;
                if pct % 10 == 0 && downloaded > 0 {
                    debug!("{}: {}% ({}/{})", t!("Download progress", "下载进度"), pct, downloaded, total);
                }
            }
        }
    }

    info!(
        "{}: {} ({} bytes)",
        t!("Download complete", "下载完成"),
        dest.display(),
        downloaded
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：无代理或空代理时，下载 URL 保持原样
    #[test]
    fn test_resolve_download_url_no_proxy() {
        let url = "https://github.com/user/repo/releases/download/v1/file.zip";
        assert_eq!(resolve_download_url(url, &None), url);
        assert_eq!(resolve_download_url(url, &Some(String::new())), url);
    }

    /// 测试：HTTP 镜像前缀会被拼接到原始 URL 之前
    #[test]
    fn test_resolve_download_url_mirror() {
        let url = "https://github.com/user/repo/releases/download/v1/file.zip";
        let proxy = Some("https://gh-proxy.com/".to_string());
        assert_eq!(
            resolve_download_url(url, &proxy),
            "https://gh-proxy.com/https://github.com/user/repo/releases/download/v1/file.zip"
        );
    }

    /// 测试：socks5 代理不修改 URL（代理在客户端层面处理）
    #[test]
    fn test_resolve_download_url_socks5_passthrough() {
        let url = "https://github.com/user/repo/releases/download/v1/file.zip";
        let proxy = Some("socks5://127.0.0.1:1080".to_string());
        assert_eq!(resolve_download_url(url, &proxy), url);
    }

    /// 测试：正则匹配 asset
    #[test]
    fn test_find_matching_asset() {
        let release = Release {
            tag_name: "v1.0.0".to_string(),
            name: Some("Release v1.0.0".to_string()),
            prerelease: false,
            assets: vec![
                Asset {
                    name: "qbittorrent-enhanced-nox_x86_64-linux-musl_static.zip".to_string(),
                    browser_download_url: "https://example.com/file.zip".to_string(),
                    size: 1024,
                },
                Asset {
                    name: "qbittorrent-enhanced-nox_aarch64-linux-musl_static.zip".to_string(),
                    browser_download_url: "https://example.com/file2.zip".to_string(),
                    size: 1024,
                },
            ],
        };

        // 匹配 x86_64 版本
        let pattern = r"^qbittorrent-enhanced-nox_x86_64-linux-musl_static\.zip$";
        let asset = find_matching_asset(&release, pattern).unwrap();
        assert_eq!(
            asset.name,
            "qbittorrent-enhanced-nox_x86_64-linux-musl_static.zip"
        );

        // 不存在的文件应返回错误
        let pattern2 = r"^nonexistent\.zip$";
        assert!(find_matching_asset(&release, pattern2).is_err());
    }
}
