// github.rs ── GitHub Releases API 交互
//
// 负责：
// - 构建 HTTP 客户端（支持 SOCKS5 代理与 Bearer Token 认证）
// - 查询 GitHub Release（latest / pre-release）
// - 通过正则匹配目标 asset
// - 下载 asset（优先使用系统 curl/wget，回退到 reqwest 流式下载）
// - 校验下载文件的 checksum（GitHub asset digest 或 Release 中的 checksum 文件）
// - 请求重试机制

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use log::{debug, info, warn};
use regex::Regex;
use reqwest::Client;
use ring::digest;
use serde::Deserialize;
use std::io::{Read, Write};
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
    /// GitHub API 返回的可选摘要，例如 `sha256:<hex>`
    #[serde(default)]
    pub digest: Option<String>,
}

/// checksum 文本文件大小上限。
///
/// checksum 文件通常只有几 KB；设置上限可以避免误把大文件当成校验清单下载到内存中。
const MAX_CHECKSUM_FILE_SIZE: u64 = 1024 * 1024;

/// 支持的 checksum 算法。
///
/// SHA1 只用于兼容老项目发布的校验文件；优先级排序里 SHA256 仍高于 SHA1。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChecksumAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl ChecksumAlgorithm {
    /// 从算法名称解析 checksum 算法。
    ///
    /// 兼容 `sha256`、`sha-256`、`sha_256` 等常见写法。
    fn from_name(name: &str) -> Option<Self> {
        let normalized = name
            .trim()
            .to_ascii_lowercase()
            .replace('-', "")
            .replace('_', "");
        match normalized.as_str() {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// 根据十六进制摘要长度推断算法。
    ///
    /// 这用于解析 GNU `sha256sum` 风格的 `hash filename` 行，此格式本身不带算法名。
    fn from_hex_len(len: usize) -> Option<Self> {
        match len {
            40 => Some(Self::Sha1),
            64 => Some(Self::Sha256),
            96 => Some(Self::Sha384),
            128 => Some(Self::Sha512),
            _ => None,
        }
    }

    /// 当前算法对应的十六进制摘要长度。
    fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha384 => 96,
            Self::Sha512 => 128,
        }
    }

    /// 用于日志和错误信息的算法名称。
    fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }

    /// 映射到 ring 的摘要算法实现。
    fn ring_algorithm(self) -> &'static digest::Algorithm {
        match self {
            Self::Sha1 => &digest::SHA1_FOR_LEGACY_USE_ONLY,
            Self::Sha256 => &digest::SHA256,
            Self::Sha384 => &digest::SHA384,
            Self::Sha512 => &digest::SHA512,
        }
    }
}

/// 解析出的期望 checksum。
///
/// `hex` 始终保存为小写十六进制字符串，便于和本地计算结果直接比较。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedChecksum {
    algorithm: ChecksumAlgorithm,
    hex: String,
}

impl ExpectedChecksum {
    /// 从十六进制摘要构造期望值。
    ///
    /// 如果调用方没有提供算法，则根据摘要长度推断；长度不符合算法时返回 `None`。
    fn from_hex(algorithm: Option<ChecksumAlgorithm>, hex: &str) -> Option<Self> {
        let clean = hex.trim();
        let clean = clean
            .strip_prefix("0x")
            .or_else(|| clean.strip_prefix("0X"))
            .unwrap_or(clean)
            .to_ascii_lowercase();

        if clean.is_empty() || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }

        let algorithm = algorithm.or_else(|| ChecksumAlgorithm::from_hex_len(clean.len()))?;
        if clean.len() != algorithm.hex_len() {
            return None;
        }

        Some(Self {
            algorithm,
            hex: clean,
        })
    }
}

/// 构建 API 请求用的 HTTP 客户端
///
/// - `proxy`：仅识别 `socks5://` 开头的代理；HTTP 镜像前缀不在此处理（在 resolve_download_url 中拼接）
/// - `token`：非空时添加 `Authorization: Bearer <token>` 头部，提升 API 速率限制
/// - `timeout_secs`：请求超时时间
pub fn build_client(
    proxy: &Option<String>,
    token: &Option<String>,
    timeout_secs: u64,
) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent("openwrt-binary-manager/0.1.0")
        .timeout(std::time::Duration::from_secs(timeout_secs));

    // 仅 socks5 代理走 reqwest 的代理配置
    if let Some(proxy_url) = proxy {
        if proxy_url.starts_with("socks5://") {
            let proxy =
                reqwest::Proxy::all(proxy_url).context("failed to parse socks5 proxy URL")?;
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
pub fn build_download_client(
    proxy: &Option<String>,
    token: &Option<String>,
    timeout_secs: u64,
) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent("openwrt-binary-manager/0.1.0")
        .timeout(std::time::Duration::from_secs(timeout_secs));

    if let Some(proxy_url) = proxy {
        if proxy_url.starts_with("socks5://") {
            let proxy =
                reqwest::Proxy::all(proxy_url).context("failed to parse socks5 proxy URL")?;
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
pub async fn download_asset(client: &Client, url: &str, dest: &Path, retries: u32) -> Result<()> {
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

/// 校验已下载 asset 的 checksum。
///
/// 优先使用 GitHub API 在 asset 上返回的 digest 字段；若不存在，
/// 则在同一个 Release 中查找常见 checksum 文件并解析目标 asset 的摘要。
pub async fn verify_asset_checksum(
    client: &Client,
    release: &Release,
    asset: &Asset,
    downloaded_path: &Path,
    proxy: &Option<String>,
    retries: u32,
) -> Result<()> {
    let expected = find_expected_checksum(client, release, asset, proxy, retries).await?;
    verify_file_checksum(downloaded_path, &expected)?;
    info!(
        "{}: {} ({})",
        t!("Checksum verified", "checksum 校验通过"),
        downloaded_path.display(),
        expected.algorithm.name()
    );
    Ok(())
}

async fn find_expected_checksum(
    client: &Client,
    release: &Release,
    asset: &Asset,
    proxy: &Option<String>,
    retries: u32,
) -> Result<ExpectedChecksum> {
    // GitHub 新版 Release asset 可能直接返回 digest 字段，优先信任这个结构化值。
    if let Some(digest_value) = &asset.digest {
        if let Some(checksum) = parse_checksum_value(digest_value) {
            debug!(
                "{}: {} ({})",
                t!("Using GitHub asset digest", "使用 GitHub asset digest"),
                asset.name,
                checksum.algorithm.name()
            );
            return Ok(checksum);
        }

        warn!(
            "{}: {} ({})",
            t!(
                "Ignoring unsupported GitHub asset digest",
                "忽略不支持的 GitHub asset digest"
            ),
            asset.name,
            digest_value
        );
    }

    // 兼容未提供 digest 的项目：从同一 Release 中寻找校验清单或 asset 专属摘要文件。
    let (checksum_asset, allow_standalone) = find_checksum_asset(release, &asset.name)
        .ok_or_else(|| anyhow!("checksum asset not found for '{}'", asset.name))?;

    let checksum_url = resolve_download_url(&checksum_asset.browser_download_url, proxy);
    info!(
        "{}: {}",
        t!("Downloading checksum", "下载 checksum"),
        checksum_asset.name
    );
    let checksum_text =
        with_retry(retries, || download_checksum_text(client, &checksum_url)).await?;

    parse_checksum_text(&checksum_text, &asset.name, allow_standalone).ok_or_else(|| {
        anyhow!(
            "checksum file '{}' did not contain a checksum for '{}'",
            checksum_asset.name,
            asset.name
        )
    })
}

/// 在 Release assets 中查找最可能对应目标 asset 的 checksum 文件。
///
/// 返回值里的 bool 表示该文件是否可接受“只有一行纯 hash”的格式：
/// asset 专属文件如 `<asset>.sha256` 可以接受，通用清单如 `SHA256SUMS` 必须匹配文件名。
fn find_checksum_asset<'a>(release: &'a Release, asset_name: &str) -> Option<(&'a Asset, bool)> {
    release
        .assets
        .iter()
        .filter(|candidate| candidate.name != asset_name)
        .filter_map(|candidate| {
            checksum_asset_score(&candidate.name, asset_name)
                .map(|(score, allow_standalone)| (score, candidate, allow_standalone))
        })
        .min_by_key(|(score, _, _)| *score)
        .map(|(_, asset, allow_standalone)| (asset, allow_standalone))
}

/// 给候选 checksum asset 打分。
///
/// 分数越小优先级越高。目标 asset 专属文件优先，其次是常见聚合清单，
/// 最后才接受文件名里泛泛包含 `checksum` / `sha256` 等关键词的候选项。
fn checksum_asset_score(candidate_name: &str, asset_name: &str) -> Option<(u8, bool)> {
    let candidate = normalize_checksum_filename(candidate_name);
    let target = normalize_checksum_filename(asset_name);
    let target_base = path_basename(&target);
    let candidate_base = path_basename(&candidate);

    if candidate == target || candidate_base == target_base {
        return None;
    }

    // `<asset>.sha256` 这类文件通常只描述一个目标文件，因此允许纯 hash 内容。
    for (suffix, score) in [
        (".sha256", 0),
        (".sha256sum", 1),
        (".sha384", 2),
        (".sha384sum", 3),
        (".sha512", 4),
        (".sha512sum", 5),
        (".sha1", 6),
        (".sha1sum", 7),
    ] {
        if candidate == format!("{}{}", target, suffix)
            || candidate_base == format!("{}{}", target_base, suffix)
        {
            return Some((score, true));
        }
    }

    // 通用 checksum 清单必须在内容中显式出现目标 asset 文件名，避免误用其他文件的摘要。
    let common_score = match candidate_base {
        "sha256sums" | "sha256sums.txt" | "sha256sum.txt" | "sha256.txt" => Some(20),
        "sha384sums" | "sha384sums.txt" | "sha384sum.txt" | "sha384.txt" => Some(21),
        "sha512sums" | "sha512sums.txt" | "sha512sum.txt" | "sha512.txt" => Some(22),
        "checksums" | "checksums.txt" | "checksum" | "checksum.txt" => Some(30),
        _ => None,
    };
    if let Some(score) = common_score {
        return Some((score, false));
    }

    // 有些项目会使用自定义命名，例如 `release-checksums.txt`，保守地放在较低优先级。
    if candidate_base.contains("sha256") {
        Some((40, false))
    } else if candidate_base.contains("sha384") {
        Some((41, false))
    } else if candidate_base.contains("sha512") {
        Some((42, false))
    } else if candidate_base.contains("checksum") || candidate_base.contains("sha1") {
        Some((50, false))
    } else {
        None
    }
}

/// 下载 checksum 文本文件。
///
/// 这里始终使用 reqwest 客户端，复用代理和 token 配置，并限制大小为 1 MiB。
async fn download_checksum_text(client: &Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .context("failed to start checksum download")?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(anyhow!("checksum download failed with status {}", status));
    }

    if let Some(len) = resp.content_length() {
        if len > MAX_CHECKSUM_FILE_SIZE {
            return Err(anyhow!(
                "checksum file is too large: {} bytes (limit: {} bytes)",
                len,
                MAX_CHECKSUM_FILE_SIZE
            ));
        }
    }

    let bytes = resp
        .bytes()
        .await
        .context("failed to read checksum download")?;
    if bytes.len() as u64 > MAX_CHECKSUM_FILE_SIZE {
        return Err(anyhow!(
            "checksum file is too large: {} bytes (limit: {} bytes)",
            bytes.len(),
            MAX_CHECKSUM_FILE_SIZE
        ));
    }

    String::from_utf8(bytes.to_vec()).context("checksum file is not valid UTF-8")
}

/// 从 checksum 文本中解析目标 asset 的摘要。
///
/// 支持 GNU、BSD、`filename: hash`、`filename hash` 以及 asset 专属文件中的纯 hash。
fn parse_checksum_text(
    checksum_text: &str,
    asset_name: &str,
    allow_standalone: bool,
) -> Option<ExpectedChecksum> {
    checksum_text.lines().find_map(|line| {
        // 按更明确的格式优先解析，最后才在允许时接受单行纯 hash。
        parse_bsd_checksum_line(line, asset_name)
            .or_else(|| parse_gnu_checksum_line(line, asset_name))
            .or_else(|| parse_filename_first_checksum_line(line, asset_name))
            .or_else(|| {
                if allow_standalone {
                    parse_checksum_value(line)
                } else {
                    None
                }
            })
    })
}

/// 解析 BSD 风格 checksum 行。
///
/// 示例：`SHA256 (app.tar.gz) = <hex>`。
fn parse_bsd_checksum_line(line: &str, asset_name: &str) -> Option<ExpectedChecksum> {
    let line = normalize_checksum_line(line)?;
    let open = line.find('(')?;
    let close = line[open + 1..].find(')')? + open + 1;
    let algorithm = ChecksumAlgorithm::from_name(&line[..open])?;
    let filename = &line[open + 1..close];
    if !filename_matches_asset(filename, asset_name) {
        return None;
    }

    let rest = line[close + 1..].trim();
    let hex = rest.strip_prefix('=')?.trim();
    ExpectedChecksum::from_hex(Some(algorithm), first_checksum_token(hex)?)
}

/// 解析 GNU coreutils 风格 checksum 行。
///
/// 示例：`<hex>  app.tar.gz` 或 `<hex> *app.tar.gz`。
fn parse_gnu_checksum_line(line: &str, asset_name: &str) -> Option<ExpectedChecksum> {
    let line = normalize_checksum_line(line)?;
    let line = line.strip_prefix('\\').unwrap_or(line);
    let mut parts = line.splitn(2, char::is_whitespace);
    let hex = parts.next()?;
    let filename = parts.next()?.trim().trim_start_matches('*').trim();
    if !filename_matches_asset(filename, asset_name) {
        return None;
    }

    ExpectedChecksum::from_hex(None, hex)
}

/// 解析文件名在前的 checksum 行。
///
/// 示例：`app.tar.gz: sha256:<hex>` 或 `app.tar.gz <hex>`。
fn parse_filename_first_checksum_line(line: &str, asset_name: &str) -> Option<ExpectedChecksum> {
    let line = normalize_checksum_line(line)?;

    if let Some((filename, checksum)) = line.rsplit_once(':') {
        if filename_matches_asset(filename, asset_name) {
            return parse_checksum_value(checksum);
        }
    }

    let mut parts = line.rsplitn(2, char::is_whitespace);
    let checksum = parts.next()?;
    let filename = parts.next()?.trim();
    if filename_matches_asset(filename, asset_name) {
        return parse_checksum_value(checksum);
    }

    None
}

/// 解析单个 checksum 值。
///
/// 支持 `sha256:<hex>`、`sha256=<hex>`、`sha256 <hex>` 和纯十六进制摘要。
fn parse_checksum_value(value: &str) -> Option<ExpectedChecksum> {
    let value = normalize_checksum_line(value)?;
    let value = value
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches(',')
        .trim();

    for separator in [':', '='] {
        if let Some((algorithm, hex)) = value.split_once(separator) {
            if let Some(algorithm) = ChecksumAlgorithm::from_name(algorithm) {
                return ExpectedChecksum::from_hex(Some(algorithm), first_checksum_token(hex)?);
            }
        }
    }

    let mut parts = value.split_whitespace();
    if let (Some(algorithm), Some(hex)) = (parts.next(), parts.next()) {
        if let Some(algorithm) = ChecksumAlgorithm::from_name(algorithm) {
            return ExpectedChecksum::from_hex(Some(algorithm), first_checksum_token(hex)?);
        }
    }

    ExpectedChecksum::from_hex(None, first_checksum_token(value)?)
}

/// 比较本地文件 checksum 与期望值。
fn verify_file_checksum(path: &Path, expected: &ExpectedChecksum) -> Result<()> {
    let actual = calculate_file_checksum(path, expected.algorithm)?;
    if actual != expected.hex {
        return Err(anyhow!(
            "checksum mismatch for '{}': expected {}:{}, got {}:{}",
            path.display(),
            expected.algorithm.name(),
            expected.hex,
            expected.algorithm.name(),
            actual
        ));
    }
    Ok(())
}

/// 流式计算文件 checksum，避免把大 release asset 一次性读入内存。
fn calculate_file_checksum(path: &Path, algorithm: ChecksumAlgorithm) -> Result<String> {
    let mut file = std::fs::File::open(path).context("failed to open file for checksum")?;
    let mut context = digest::Context::new(algorithm.ring_algorithm());
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .context("failed to read file for checksum")?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }

    Ok(to_hex(context.finish().as_ref()))
}

/// 将摘要字节转成小写十六进制字符串。
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 规范化 checksum 文本行，跳过空行和注释行。
fn normalize_checksum_line(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        None
    } else {
        Some(line)
    }
}

/// 取 checksum 表达式中的第一个有效 token。
///
/// 用于剥离行尾逗号、分号或后续注释式内容。
fn first_checksum_token(value: &str) -> Option<&str> {
    value
        .trim()
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .find(|part| !part.is_empty())
}

/// 判断 checksum 文件中的文件名是否指向目标 asset。
///
/// 同时接受完整路径、basename 和带前置目录的路径，兼容不同发布工具生成的清单。
fn filename_matches_asset(filename: &str, asset_name: &str) -> bool {
    let filename = normalize_checksum_filename(filename);
    let asset_name = normalize_checksum_filename(asset_name);
    let asset_base = path_basename(&asset_name);

    filename == asset_name
        || filename == asset_base
        || filename.ends_with(&format!("/{}", asset_name))
        || filename.ends_with(&format!("/{}", asset_base))
}

/// 规范化 checksum 文件名或清单中的文件名。
///
/// 处理引号、GNU 二进制模式前缀 `*`、Windows 路径分隔符和 `./` 前缀。
fn normalize_checksum_filename(filename: &str) -> String {
    let mut clean = filename
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_start_matches('*')
        .trim()
        .replace('\\', "/");

    while let Some(stripped) = clean.strip_prefix("./") {
        clean = stripped.to_string();
    }

    clean.to_ascii_lowercase()
}

/// 取 `/` 分隔路径的 basename。
fn path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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
            "curl" => (
                "curl",
                &[
                    "-fSL",
                    "-o",
                    &dest.to_string_lossy(),
                    "--connect-timeout",
                    "30",
                    "--max-time",
                    "600",
                    url,
                ],
            ),
            "wget" => (
                "wget",
                &[
                    "-q",
                    "-O",
                    &dest.to_string_lossy(),
                    "--timeout=30",
                    "--tries=3",
                    url,
                ],
            ),
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
    let mut file =
        std::fs::File::create(dest).context("failed to create download destination file")?;

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
                if pct.is_multiple_of(10) && downloaded > 0 {
                    debug!(
                        "{}: {}% ({}/{})",
                        t!("Download progress", "下载进度"),
                        pct,
                        downloaded,
                        total
                    );
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
                    digest: None,
                },
                Asset {
                    name: "qbittorrent-enhanced-nox_aarch64-linux-musl_static.zip".to_string(),
                    browser_download_url: "https://example.com/file2.zip".to_string(),
                    size: 1024,
                    digest: None,
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

    #[test]
    fn test_parse_github_asset_digest() {
        let checksum = parse_checksum_value(
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        )
        .unwrap();

        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(
            checksum.hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_parse_checksum_text_common_formats() {
        let gnu = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  app-linux-amd64.tar.gz";
        let checksum = parse_checksum_text(gnu, "app-linux-amd64.tar.gz", false).unwrap();
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);

        let bsd = "SHA256 (dist/app-linux-amd64.tar.gz) = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let checksum = parse_checksum_text(bsd, "app-linux-amd64.tar.gz", false).unwrap();
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);

        let standalone = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(parse_checksum_text(standalone, "app-linux-amd64.tar.gz", false).is_none());
        assert!(parse_checksum_text(standalone, "app-linux-amd64.tar.gz", true).is_some());
    }

    #[test]
    fn test_find_checksum_asset_prefers_asset_specific_file() {
        let release = Release {
            tag_name: "v1.0.0".to_string(),
            name: None,
            prerelease: false,
            assets: vec![
                Asset {
                    name: "checksums.txt".to_string(),
                    browser_download_url: "https://example.com/checksums.txt".to_string(),
                    size: 100,
                    digest: None,
                },
                Asset {
                    name: "app.tar.gz".to_string(),
                    browser_download_url: "https://example.com/app.tar.gz".to_string(),
                    size: 1024,
                    digest: None,
                },
                Asset {
                    name: "app.tar.gz.sha256".to_string(),
                    browser_download_url: "https://example.com/app.tar.gz.sha256".to_string(),
                    size: 64,
                    digest: None,
                },
            ],
        };

        let (asset, allow_standalone) = find_checksum_asset(&release, "app.tar.gz").unwrap();
        assert_eq!(asset.name, "app.tar.gz.sha256");
        assert!(allow_standalone);
    }

    #[test]
    fn test_verify_file_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, b"hello").unwrap();

        let expected = ExpectedChecksum {
            algorithm: ChecksumAlgorithm::Sha256,
            hex: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
        };
        verify_file_checksum(&path, &expected).unwrap();

        let wrong = ExpectedChecksum {
            algorithm: ChecksumAlgorithm::Sha256,
            hex: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        };
        assert!(verify_file_checksum(&path, &wrong).is_err());
    }
}
