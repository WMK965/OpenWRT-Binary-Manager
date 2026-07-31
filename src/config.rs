// config.rs ── 配置文件结构与反序列化
//
// 定义 YAML 配置文件的数据结构，并实现：
// - 自定义 Duration 反序列化（支持 "30s"/"5m"/"1h"/"1d" 等人类可读格式）
// - 自定义 backup 字段反序列化（支持 false/true/数字 三种写法）
// - 自定义 failsafe 字段反序列化（支持 true/false/"allow_post"）
// - 配置加载与校验（repo 格式、正则合法性）

use anyhow::{anyhow, Result};
use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// 顶层配置（对应整个 YAML 文件）
#[derive(Debug, Deserialize)]
pub struct Config {
    /// 全局配置段
    pub config: GlobalConfig,
    /// 监控项映射表，key 为 monitor 名称
    pub monitors: HashMap<String, MonitorConfig>,
}

/// 全局配置段
#[derive(Debug, Deserialize)]
pub struct GlobalConfig {
    /// 日志文件路径
    pub log: PathBuf,
    /// 状态文件路径（JSON 格式，记录各 monitor 运行状态）
    pub status: PathBuf,
    /// 工作目录（下载和解压的临时文件存放于此）
    #[serde(rename = "working-dir")]
    pub working_dir: PathBuf,
    /// 可选：GitHub Personal Access Token，用于提升 API 速率限制
    #[serde(default)]
    pub token: Option<String>,
    /// 可选：界面语言 (en_us / zh_cn)，留空则自动检测系统环境
    #[serde(default)]
    pub language: Option<String>,
    /// 可选：并发检查数（默认 4）
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// 可选：API 请求超时秒数（默认 30）
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// 可选：请求失败重试次数（默认 2）
    #[serde(default = "default_retry")]
    pub retry: u32,
    /// 可选：下载超时秒数（默认 600）
    #[serde(default = "default_download_timeout")]
    pub download_timeout: u64,
    /// 可选：全局备份目录配置（启用备份/故障保护的 monitor 会在此下创建子目录）
    #[serde(default)]
    pub backup: Option<GlobalBackupConfig>,
}

// ── 默认值函数 ──────────────────────────────────────────────

fn default_concurrency() -> usize {
    4
}

fn default_timeout() -> u64 {
    30
}

fn default_retry() -> u32 {
    2
}

fn default_download_timeout() -> u64 {
    600
}

/// 全局备份配置
#[derive(Debug, Deserialize)]
pub struct GlobalBackupConfig {
    /// 备份根目录，各 monitor 的备份将存放于 `{dir}/{monitor_name}/` 子目录下
    pub dir: PathBuf,
}

/// Failsafe 故障保护模式
///
/// - `On`        ：启用故障保护（默认），更新失败时自动恢复
/// - `Off`       ：完全关闭故障保护
/// - `AllowPost` ：更新失败恢复后，仍执行 post_update 脚本以重启服务
#[derive(Debug, Clone, PartialEq)]
pub enum FailsafeMode {
    On,
    Off,
    AllowPost,
}

impl Default for FailsafeMode {
    fn default() -> Self {
        FailsafeMode::On
    }
}

fn default_failsafe() -> FailsafeMode {
    FailsafeMode::On
}

/// 单个 monitor 的配置
#[derive(Debug, Deserialize)]
pub struct MonitorConfig {
    /// 目标二进制文件的绝对路径
    pub file: PathBuf,
    /// 检查间隔（支持 "30s"/"5m"/"1h"/"1d" 等格式）
    #[serde(deserialize_with = "deserialize_duration")]
    pub interval: Duration,
    /// 可选：HTTP 镜像前缀（如 https://gh-proxy.com/）或 socks5 代理地址
    #[serde(default)]
    pub proxy: Option<String>,
    /// 匹配 release asset 文件名的正则表达式
    pub regex: String,
    /// GitHub 仓库，格式为 `owner/repo`
    pub repo: String,
    /// Release 类型：latest（正式版）或 pre-release（预发布）
    #[serde(rename = "type")]
    pub release_type: ReleaseType,
    /// 可选：存档内要提取的文件路径，支持 {tag} / {version} 变量
    #[serde(default)]
    pub extract_path: Option<String>,
    /// 可选：替换前执行的 shell 脚本（支持多行）
    #[serde(default)]
    pub pre_update: Option<String>,
    /// 可选：替换后执行的 shell 脚本（支持多行）
    #[serde(default)]
    pub post_update: Option<String>,
    /// 可选：保留历史备份份数（需全局 backup 已配置）
    /// 反序列化支持：false -> None, true -> Some(3), N -> Some(N)
    #[serde(default, deserialize_with = "deserialize_backup")]
    pub backup: Option<usize>,
    /// 可选：故障保护模式 (true/false/allow_post, 默认 true)
    #[serde(default = "default_failsafe", deserialize_with = "deserialize_failsafe")]
    pub failsafe: FailsafeMode,
    /// 可选：本地版本检测配置
    /// 配置后会通过执行命令获取本地版本号，与远程 tag 比对决定是否需要更新
    #[serde(default)]
    pub version_check: Option<VersionCheckConfig>,
}

/// 版本检测配置
///
/// 通过执行命令并正则提取版本号，与远程 release tag 比对，
/// 若版本一致则跳过更新。正则必须包含一个捕获组用于提取版本号。
#[derive(Debug, Deserialize)]
pub struct VersionCheckConfig {
    /// 获取本地版本号的命令（如 `/usr/bin/qBittorrent-nox --version`）
    pub command: String,
    /// 提取版本号的正则表达式，必须包含一个捕获组
    pub regex: String,
    /// 可选：比较前去除远程 tag 的前缀（如 `release-`）
    #[serde(default)]
    pub strip_prefix: Option<String>,
}

/// Release 类型
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub enum ReleaseType {
    /// 正式版（GitHub API 的 /releases/latest 端点）
    #[serde(rename = "latest")]
    Latest,
    /// 预发布版（从 /releases 列表中筛选 prerelease=true 的最新一个）
    #[serde(rename = "pre-release")]
    PreRelease,
}

/// 自定义 Duration 反序列化器，支持 "30s" / "5m" / "1h" / "6h" / "1d" 格式
fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    struct DurationVisitor;

    impl<'de> Visitor<'de> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a duration string like '30s', '5m', '1h', '1d'")
        }

        fn visit_str<E>(self, value: &str) -> Result<Duration, E>
        where
            E: de::Error,
        {
            parse_duration(value).map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_str(DurationVisitor)
}

/// 自定义 backup 反序列化器：false -> None, true -> Some(3), N -> Some(N)
///
/// 这样配置文件中可以灵活写：
/// - `backup: false`     -> 不备份
/// - `backup: true`      -> 备份并保留默认 3 份
/// - `backup: 5`         -> 备份并保留 5 份
fn deserialize_backup<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BackupVisitor;

    impl<'de> Visitor<'de> for BackupVisitor {
        type Value = Option<usize>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("false, true, or a number")
        }

        /// 布尔值处理：true -> 默认保留 3 份，false -> 不备份
        fn visit_bool<E>(self, v: bool) -> Result<Option<usize>, E>
        where
            E: de::Error,
        {
            if v {
                Ok(Some(3))
            } else {
                Ok(None)
            }
        }

        /// 有符号整数处理：<=0 -> 不备份，>0 -> 保留对应份数
        fn visit_i64<E>(self, v: i64) -> Result<Option<usize>, E>
        where
            E: de::Error,
        {
            if v <= 0 {
                Ok(None)
            } else {
                Ok(Some(v as usize))
            }
        }

        /// 无符号整数处理：0 -> 不备份，>0 -> 保留对应份数
        fn visit_u64<E>(self, v: u64) -> Result<Option<usize>, E>
        where
            E: de::Error,
        {
            if v == 0 {
                Ok(None)
            } else {
                Ok(Some(v as usize))
            }
        }
    }

    // 使用 deserialize_any 以支持布尔、数字等多种 YAML 类型
    deserializer.deserialize_any(BackupVisitor)
}

/// 自定义 failsafe 反序列化器：true/未指定 -> On, false -> Off, "allow_post" -> AllowPost
fn deserialize_failsafe<'de, D>(deserializer: D) -> Result<FailsafeMode, D::Error>
where
    D: Deserializer<'de>,
{
    struct FailsafeVisitor;

    impl<'de> Visitor<'de> for FailsafeVisitor {
        type Value = FailsafeMode;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("true, false, or 'allow_post'")
        }

        /// 布尔值处理：true -> On, false -> Off
        fn visit_bool<E>(self, v: bool) -> Result<FailsafeMode, E>
        where
            E: de::Error,
        {
            if v {
                Ok(FailsafeMode::On)
            } else {
                Ok(FailsafeMode::Off)
            }
        }

        /// 字符串处理：仅支持 "allow_post"，其他值报错
        fn visit_str<E>(self, v: &str) -> Result<FailsafeMode, E>
        where
            E: de::Error,
        {
            match v {
                "allow_post" => Ok(FailsafeMode::AllowPost),
                _ => Err(de::Error::custom(format!(
                    "unknown failsafe value '{}', expected true, false, or 'allow_post'",
                    v
                ))),
            }
        }
    }

    deserializer.deserialize_any(FailsafeVisitor)
}

/// 解析 duration 字符串
///
/// 支持的单位：
/// - `s` / `sec` / `secs` / `second` / `seconds` -> 秒
/// - `m` / `min` / `mins` / `minute` / `minutes` -> 分钟（×60）
/// - `h` / `hr` / `hrs` / `hour` / `hours`       -> 小时（×3600）
/// - `d` / `day` / `days`                         -> 天（×86400）
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration string"));
    }

    // 分离数字部分和单位部分：找到第一个非数字字符的位置
    let (num_part, unit_part) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|pos| s.split_at(pos))
        .ok_or_else(|| anyhow!("missing unit in duration '{}'", s))?;

    let value: u64 = num_part
        .parse()
        .map_err(|_| anyhow!("invalid number in duration '{}'", s))?;

    // 按单位换算为秒数
    let secs = match unit_part {
        "s" | "sec" | "secs" | "second" | "seconds" => value,
        "m" | "min" | "mins" | "minute" | "minutes" => value * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * 3600,
        "d" | "day" | "days" => value * 86400,
        _ => return Err(anyhow!("unknown duration unit '{}' in '{}'", unit_part, s)),
    };

    Ok(Duration::from_secs(secs))
}

/// 从文件路径加载配置
///
/// 读取 YAML 文件并反序列化为 `Config` 结构，同时执行以下校验：
/// 1. repo 必须为 `owner/repo` 格式（恰好一个斜杠）
/// 2. asset 匹配正则必须合法
/// 3. version_check 正则（若配置）必须合法
pub fn load_config(path: &std::path::Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("failed to read config file '{}': {}", path.display(), e))?;
    let config: Config = serde_yaml::from_str(&content)
        .map_err(|e| anyhow!("failed to parse config file '{}': {}", path.display(), e))?;

    // 验证每个 monitor 的配置合法性
    for (name, monitor) in &config.monitors {
        // 校验 repo 格式：必须为 owner/repo，即按 '/' 分割后恰好 2 部分
        if monitor.repo.split('/').count() != 2 {
            return Err(anyhow!(
                "monitor '{}': repo '{}' must be in 'owner/repo' format",
                name,
                monitor.repo
            ));
        }
        // 验证 asset 匹配正则是否合法
        regex::Regex::new(&monitor.regex).map_err(|e| {
            anyhow!("monitor '{}': invalid regex '{}': {}", name, monitor.regex, e)
        })?;
        // 验证 version_check 正则是否合法
        if let Some(vc) = &monitor.version_check {
            regex::Regex::new(&vc.regex).map_err(|e| {
                anyhow!(
                    "monitor '{}': invalid version_check regex '{}': {}",
                    name,
                    vc.regex,
                    e
                )
            })?;
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试：duration 字符串解析的各种合法格式
    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(
            parse_duration("2days").unwrap(),
            Duration::from_secs(172800)
        );
    }

    /// 测试：duration 字符串解析的非法情况
    #[test]
    fn test_parse_duration_errors() {
        assert!(parse_duration("").is_err()); // 空字符串
        assert!(parse_duration("123").is_err()); // 缺少单位
        assert!(parse_duration("5x").is_err()); // 未知单位
    }

    /// 测试：完整配置文件的反序列化
    #[test]
    fn test_deserialize_config() {
        let yaml = r#"
config:
  log: /tmp/updater/updater.log
  status: /tmp/updater/updater.status
  working-dir: /tmp/updater
  token: "ghp_test123"
  backup:
    dir: /tmp/updater/backups

monitors:
  qBittorrent-ee:
    file: /usr/bin/qBittorrent-nox
    interval: 6h
    proxy: "https://gh-proxy.com/"
    regex: "^qbittorrent-enhanced-nox_x86_64-linux-musl_static\\.zip$"
    repo: c0re100/qBittorrent-Enhanced-Edition
    type: latest
    extract_path: qbittorrent-nox
    pre_update: "/etc/init.d/qbittorrent stop"
    post_update: "/etc/init.d/qbittorrent restart"
    backup: 3
    failsafe: allow_post
    version_check:
      command: "/usr/bin/qBittorrent-nox --version"
      regex: "qBittorrent v([0-9.]+)"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.monitors.len(), 1);

        let m = config.monitors.get("qBittorrent-ee").unwrap();
        assert_eq!(m.interval, Duration::from_secs(6 * 3600));
        assert_eq!(m.release_type, ReleaseType::Latest);
        assert_eq!(m.extract_path.as_deref(), Some("qbittorrent-nox"));
        assert_eq!(
            m.pre_update.as_deref(),
            Some("/etc/init.d/qbittorrent stop")
        );
        assert_eq!(m.backup, Some(3));
        assert_eq!(m.failsafe, FailsafeMode::AllowPost);
        assert!(config.config.backup.is_some());
        assert_eq!(
            config.config.token.as_deref(),
            Some("ghp_test123")
        );
        let vc = m.version_check.as_ref().unwrap();
        assert_eq!(vc.command, "/usr/bin/qBittorrent-nox --version");
        assert_eq!(vc.regex, "qBittorrent v([0-9.]+)");
    }
}
