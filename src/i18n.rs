// i18n.rs ── 国际化（中英文切换）
//
// 通过全局 OnceLock 保存当前语言，配合 `t!` 宏在代码中
// 同时给出英文和中文字面量，运行时根据语言选择其一。

use std::sync::OnceLock;

/// 支持的语言种类
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lang {
    EnUs, // 英文（默认）
    ZhCn, // 简体中文
}

/// 全局语言设置，进程生命周期内只初始化一次
static LANG: OnceLock<Lang> = OnceLock::new();

/// 初始化全局语言（仅第一次调用生效，后续调用会被忽略）
pub fn init(lang: Lang) {
    _ = LANG.set(lang);
}

/// 获取当前语言；若未初始化则返回英文（EnUs）
pub fn lang() -> Lang {
    LANG.get().copied().unwrap_or(Lang::EnUs)
}

/// 自动检测系统语言环境。
///
/// 依次检查 `LANG` / `LC_ALL` / `LC_MESSAGES` 环境变量，
/// 若任一以 `zh` 开头（不区分大小写）则判定为中文，否则为英文。
pub fn detect_locale() -> Lang {
    for var in ["LANG", "LC_ALL", "LC_MESSAGES"] {
        if let Ok(val) = std::env::var(var) {
            let lower = val.to_lowercase();
            if lower.starts_with("zh") {
                return Lang::ZhCn;
            }
        }
    }
    Lang::EnUs
}

impl Lang {
    /// 从字符串解析语言，支持 `en_us` / `en` / `zh_cn` / `zh`（不区分大小写）
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "en_us" | "en" => Some(Lang::EnUs),
            "zh_cn" | "zh" => Some(Lang::ZhCn),
            _ => None,
        }
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lang::EnUs => write!(f, "en_us"),
            Lang::ZhCn => write!(f, "zh_cn"),
        }
    }
}

/// 翻译宏：根据当前语言返回对应字面量。
///
/// 用法：
/// - `t!("English", "中文")` → 返回 `&str`
/// - `t!("Hello {}", "你好 {}", name)` → 返回 `String`（带格式化参数）
#[macro_export]
macro_rules! t {
    // 无格式化参数版本：返回 &str
    ($en:literal, $zh:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::EnUs => $en,
            $crate::i18n::Lang::ZhCn => $zh,
        }
    };
    // 带格式化参数版本：调用 format! 返回 String
    ($en:literal, $zh:literal, $($fmt:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::EnUs => format!($en, $($fmt)+),
            $crate::i18n::Lang::ZhCn => format!($zh, $($fmt)+),
        }
    };
}
