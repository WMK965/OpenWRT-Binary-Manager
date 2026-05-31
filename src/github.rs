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

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Release {
    pub tag_name: String,
    pub name: Option<String>,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

pub fn build_client(proxy: &Option<String>, token: &Option<String>, timeout_secs: u64) -> Result<Client> {
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

    builder.build().context("failed to build HTTP client")
}

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

/// Retry an async operation up to `retries` times with 1s delay between attempts.
pub async fn with_retry<F, Fut, T>(retries: u32, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..=retries {
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
    Err(last_err.unwrap())
}

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

async fn get_latest_release(client: &Client, repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    debug!("GET {}", url);

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("failed to fetch latest release")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub API returned {}: {}", status, body));
    }

    resp.json::<Release>()
        .await
        .context("failed to parse release JSON")
}

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

    releases
        .into_iter()
        .find(|r| r.prerelease)
        .ok_or_else(|| anyhow!("no pre-release found for {}", repo))
}

pub fn find_matching_asset<'a>(release: &'a Release, pattern: &str) -> Result<&'a Asset> {
    let re = Regex::new(pattern).context("invalid asset regex")?;

    release
        .assets
        .iter()
        .find(|a| re.is_match(&a.name))
        .ok_or_else(|| {
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

pub fn resolve_download_url(original_url: &str, proxy: &Option<String>) -> String {
    match proxy {
        Some(proxy_url) if !proxy_url.is_empty() && !proxy_url.starts_with("socks5://") => {
            let prefix = proxy_url.trim_end_matches('/');
            format!("{}/{}", prefix, original_url)
        }
        _ => original_url.to_string(),
    }
}

pub async fn download_asset(
    client: &Client,
    url: &str,
    dest: &Path,
    retries: u32,
) -> Result<()> {
    // First try native tools (curl / wget), fall back to reqwest
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

    with_retry(retries, || download_with_reqwest(client, url, dest)).await
}

fn try_native_download(url: &str, dest: &Path) -> Result<()> {
    let tools = ["curl", "wget"];
    for tool in &tools {
        let (cmd, args): (&str, &[&str]) = match *tool {
            "curl" => ("curl", &["-fSL", "-o", &dest.to_string_lossy(), "--connect-timeout", "30", "--max-time", "600", url]),
            "wget" => ("wget", &["-q", "-O", &dest.to_string_lossy(), "--timeout=30", "--tries=3", url]),
            _ => continue,
        };

        let status = std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => {
                info!("{}: {} ({})", t!("Downloading via", "通过下载"), tool, url);
                return Ok(());
            }
            _ => continue,
        }
    }
    Err(anyhow!("no native download tool available"))
}

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

    let total_size = resp.content_length();
    let mut file = std::fs::File::create(dest)
        .context("failed to create download destination file")?;

    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading download stream")?;
        file.write_all(&chunk)
            .context("failed to write to destination file")?;
        downloaded += chunk.len() as u64;

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

    #[test]
    fn test_resolve_download_url_no_proxy() {
        let url = "https://github.com/user/repo/releases/download/v1/file.zip";
        assert_eq!(resolve_download_url(url, &None), url);
        assert_eq!(resolve_download_url(url, &Some(String::new())), url);
    }

    #[test]
    fn test_resolve_download_url_mirror() {
        let url = "https://github.com/user/repo/releases/download/v1/file.zip";
        let proxy = Some("https://gh-proxy.com/".to_string());
        assert_eq!(
            resolve_download_url(url, &proxy),
            "https://gh-proxy.com/https://github.com/user/repo/releases/download/v1/file.zip"
        );
    }

    #[test]
    fn test_resolve_download_url_socks5_passthrough() {
        let url = "https://github.com/user/repo/releases/download/v1/file.zip";
        let proxy = Some("socks5://127.0.0.1:1080".to_string());
        assert_eq!(resolve_download_url(url, &proxy), url);
    }

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

        let pattern = r"^qbittorrent-enhanced-nox_x86_64-linux-musl_static\.zip$";
        let asset = find_matching_asset(&release, pattern).unwrap();
        assert_eq!(
            asset.name,
            "qbittorrent-enhanced-nox_x86_64-linux-musl_static.zip"
        );

        let pattern2 = r"^nonexistent\.zip$";
        assert!(find_matching_asset(&release, pattern2).is_err());
    }
}
