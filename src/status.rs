// status.rs ── 状态文件管理
//
// 状态文件以 JSON 格式持久化记录每个 monitor 的运行状态：
// - last_check   ：上次检查远程 release 的时间
// - current_tag  ：当前已安装的 release tag
// - last_update  ：上次成功更新的时间
//
// 通过这些状态实现“检查间隔控制”和“是否需要更新”的判断。

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// 顶层 status 结构（对应整个 JSON 文件）
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StatusFile {
    /// 各 monitor 的状态映射表，key 为 monitor 名称
    #[serde(default)]
    pub monitors: HashMap<String, MonitorStatus>,
}

/// 单个 monitor 的状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorStatus {
    /// 上次检查时间
    pub last_check: DateTime<Utc>,
    /// 当前安装的 release tag（首次运行时为 None）
    #[serde(default)]
    pub current_tag: Option<String>,
    /// 上次成功更新时间
    #[serde(default)]
    pub last_update: Option<DateTime<Utc>>,
}

impl StatusFile {
    /// 从文件加载 status，文件不存在则返回空 status
    pub fn load(path: &Path) -> Result<Self> {
        // 文件不存在视为首次运行，返回空状态
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        // 文件为空同样视为首次运行
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let status: StatusFile = serde_json::from_str(&content)?;
        Ok(status)
    }

    /// 原子写入 status 文件（先写临时文件，再 rename）
    ///
    /// 采用“写入临时文件 + rename”模式，避免写入过程中
    /// 程序崩溃导致状态文件损坏（rename 在同一文件系统上是原子的）。
    pub fn save(&self, path: &Path) -> Result<()> {
        // 确保目录存在
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        // 临时文件扩展名设为 .tmp，与目标文件同目录以保证 rename 的原子性
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, &content)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// 获取指定 monitor 的状态
    pub fn get(&self, name: &str) -> Option<&MonitorStatus> {
        self.monitors.get(name)
    }

    /// 更新指定 monitor 的检查时间（不修改 tag）
    ///
    /// 用于“无需更新”或“检查失败”场景，避免下一轮立即重试。
    pub fn update_check(&mut self, name: &str) {
        let entry = self
            .monitors
            .entry(name.to_string())
            .or_insert_with(|| MonitorStatus {
                last_check: Utc::now(),
                current_tag: None,
                last_update: None,
            });
        entry.last_check = Utc::now();
    }

    /// 更新指定 monitor 的 tag 和更新时间
    ///
    /// 用于“更新成功”场景，记录新安装的 release tag。
    pub fn update_tag(&mut self, name: &str, tag: &str) {
        let entry = self
            .monitors
            .entry(name.to_string())
            .or_insert_with(|| MonitorStatus {
                last_check: Utc::now(),
                current_tag: None,
                last_update: None,
            });
        entry.current_tag = Some(tag.to_string());
        entry.last_update = Some(Utc::now());
        entry.last_check = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// 测试：加载不存在的状态文件应返回空状态
    #[test]
    fn test_load_nonexistent() {
        let status = StatusFile::load(Path::new("/nonexistent/path")).unwrap();
        assert!(status.monitors.is_empty());
    }

    /// 测试：保存后重新加载，数据应保持一致
    #[test]
    fn test_save_and_load() {
        let mut tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let mut status = StatusFile::default();
        status.update_tag("test-monitor", "v1.0.0");
        status.save(&path).unwrap();

        let loaded = StatusFile::load(&path).unwrap();
        let mon = loaded.get("test-monitor").unwrap();
        assert_eq!(mon.current_tag.as_deref(), Some("v1.0.0"));
    }
}
