use anyhow::{anyhow, Result};
use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// 顶层配置
#[derive(Debug, Deserialize)]
pub struct Config {
    pub config: GlobalConfig,
    pub monitors: HashMap<String, MonitorConfig>,
}

/// 全局配置
#[derive(Debug, Deserialize)]
pub struct GlobalConfig {
    pub log: PathBuf,
    pub status: PathBuf,
    #[serde(rename = "working-dir")]
    pub working_dir: PathBuf,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

/// 单个 monitor 的配置
#[derive(Debug, Deserialize)]
pub struct MonitorConfig {
    pub file: PathBuf,
    #[serde(deserialize_with = "deserialize_duration")]
    pub interval: Duration,
    #[serde(default)]
    pub proxy: Option<String>,
    pub regex: String,
    pub repo: String,
    #[serde(rename = "type")]
    pub release_type: ReleaseType,
    #[serde(default)]
    pub extract_path: Option<String>,
    #[serde(default)]
    pub pre_update: Option<String>,
    #[serde(default)]
    pub post_update: Option<String>,
    #[serde(default)]
    pub backup: Option<BackupConfig>,
    #[serde(default)]
    pub version_check: Option<VersionCheckConfig>,
}

/// 备份配置
#[derive(Debug, Deserialize)]
pub struct BackupConfig {
    #[serde(default)]
    pub enabled: bool,
    pub dir: PathBuf,
    #[serde(default = "default_backup_count")]
    pub count: usize,
}

fn default_backup_count() -> usize {
    3
}

/// 版本检测配置
///
/// 通过执行命令并正则提取版本号，与远程 release tag 比对，
/// 若版本一致则跳过更新。正则必须包含一个捕获组用于提取版本号。
#[derive(Debug, Deserialize)]
pub struct VersionCheckConfig {
    pub command: String,
    pub regex: String,
}

/// Release 类型
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub enum ReleaseType {
    #[serde(rename = "latest")]
    Latest,
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

/// 解析 duration 字符串
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration string"));
    }

    // 分离数字部分和单位部分
    let (num_part, unit_part) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|pos| s.split_at(pos))
        .ok_or_else(|| anyhow!("missing unit in duration '{}'", s))?;

    let value: u64 = num_part
        .parse()
        .map_err(|_| anyhow!("invalid number in duration '{}'", s))?;

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
pub fn load_config(path: &std::path::Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("failed to read config file '{}': {}", path.display(), e))?;
    let config: Config = serde_yaml::from_str(&content)
        .map_err(|e| anyhow!("failed to parse config file '{}': {}", path.display(), e))?;

    // 验证配置
    for (name, monitor) in &config.monitors {
        if monitor.repo.split('/').count() != 2 {
            return Err(anyhow!(
                "monitor '{}': repo '{}' must be in 'owner/repo' format",
                name,
                monitor.repo
            ));
        }
        // 验证正则是否合法
        regex::Regex::new(&monitor.regex).map_err(|e| {
            anyhow!("monitor '{}': invalid regex '{}': {}", name, monitor.regex, e)
        })?;
        // 验证 version_check 正则
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

    #[test]
    fn test_parse_duration_errors() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("123").is_err());
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn test_deserialize_config() {
        let yaml = r#"
config:
  log: /tmp/updater/updater.log
  status: /tmp/updater/updater.status
  working-dir: /tmp/updater
  token: "ghp_test123"

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
    version_check:
      command: "/usr/bin/qBittorrent-nox --version"
      regex: "qBittorrent v([0-9.]+)"
    backup:
      enabled: true
      dir: /tmp/updater/backups
      count: 3
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
        assert!(m.backup.as_ref().unwrap().enabled);
        assert_eq!(m.backup.as_ref().unwrap().count, 3);
        assert_eq!(
            config.config.token.as_deref(),
            Some("ghp_test123")
        );
        let vc = m.version_check.as_ref().unwrap();
        assert_eq!(vc.command, "/usr/bin/qBittorrent-nox --version");
        assert_eq!(vc.regex, "qBittorrent v([0-9.]+)");
    }
}
