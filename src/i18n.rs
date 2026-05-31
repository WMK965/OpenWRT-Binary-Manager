use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lang {
    EnUs,
    ZhCn,
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn init(lang: Lang) {
    _ = LANG.set(lang);
}

pub fn lang() -> Lang {
    LANG.get().copied().unwrap_or(Lang::EnUs)
}

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

#[macro_export]
macro_rules! t {
    ($en:literal, $zh:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::EnUs => $en,
            $crate::i18n::Lang::ZhCn => $zh,
        }
    };
    ($en:literal, $zh:literal, $($fmt:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::EnUs => format!($en, $($fmt)+),
            $crate::i18n::Lang::ZhCn => format!($zh, $($fmt)+),
        }
    };
}
