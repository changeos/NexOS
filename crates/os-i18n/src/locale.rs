//! Locale 枚举 —— 支持的语言（规划文档 §3.1 初始三语）

use os_core::Serialize;
use serde::Deserialize;

/// 系统支持的语言
///
/// serde 序列化为 snake_case 字符串（如 `zh_cn` / `zh_tw` / `en`），
/// 便于与前端/i18n 资源文件命名对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    /// 简体中文（zh-CN）
    ZhCn,
    /// 繁体中文（zh-TW）
    ZhTw,
    /// 英文（en）
    En,
}

/// 默认语言（fallback / 未提供时使用）
pub const DEFAULT_LOCALE: Locale = Locale::En;

impl Default for Locale {
    fn default() -> Self {
        DEFAULT_LOCALE
    }
}

impl Locale {
    /// 返回该语言的 BCP-47 标签（如 `zh-CN`），用于 HTTP `Accept-Language` / Intl API
    pub fn bcp47(&self) -> &'static str {
        match self {
            Locale::ZhCn => "zh-CN",
            Locale::ZhTw => "zh-TW",
            Locale::En => "en",
        }
    }

    /// 返回该语言的 snake_case 代码（与 serde 序列化一致，便于资源文件名拼接）
    pub fn code(&self) -> &'static str {
        match self {
            Locale::ZhCn => "zh_cn",
            Locale::ZhTw => "zh_tw",
            Locale::En => "en",
        }
    }

    /// 所有支持的语言（用于 `Translator::available_locales` 默认实现参考）
    pub fn all() -> &'static [Locale] {
        &[Locale::ZhCn, Locale::ZhTw, Locale::En]
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcp47_tags_match_spec() {
        assert_eq!(Locale::ZhCn.bcp47(), "zh-CN");
        assert_eq!(Locale::ZhTw.bcp47(), "zh-TW");
        assert_eq!(Locale::En.bcp47(), "en");
    }

    #[test]
    fn code_returns_snake_case() {
        assert_eq!(Locale::ZhCn.code(), "zh_cn");
        assert_eq!(Locale::ZhTw.code(), "zh_tw");
        assert_eq!(Locale::En.code(), "en");
    }

    #[test]
    fn all_lists_three_supported_locales() {
        let all = Locale::all();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&Locale::ZhCn));
        assert!(all.contains(&Locale::ZhTw));
        assert!(all.contains(&Locale::En));
    }

    #[test]
    fn default_is_english() {
        assert_eq!(Locale::default(), DEFAULT_LOCALE);
        assert_eq!(Locale::default(), Locale::En);
    }

    #[test]
    fn display_matches_code() {
        assert_eq!(format!("{}", Locale::ZhCn), "zh_cn");
        assert_eq!(format!("{}", Locale::ZhTw), "zh_tw");
        assert_eq!(format!("{}", Locale::En), "en");
    }

    #[test]
    fn serde_roundtrip_preserves_locale() {
        for &loc in Locale::all() {
            let s = serde_json::to_string(&loc).unwrap();
            let back: Locale = serde_json::from_str(&s).unwrap();
            assert_eq!(back, loc, "{loc:?} 往返失配");
        }
    }

    #[test]
    fn serde_serializes_to_snake_case_strings() {
        assert_eq!(serde_json::to_string(&Locale::ZhCn).unwrap(), "\"zh_cn\"");
        assert_eq!(serde_json::to_string(&Locale::ZhTw).unwrap(), "\"zh_tw\"");
        assert_eq!(serde_json::to_string(&Locale::En).unwrap(), "\"en\"");
    }

    #[test]
    fn serde_deserializes_from_snake_case_strings() {
        let zh_cn: Locale = serde_json::from_str("\"zh_cn\"").unwrap();
        let zh_tw: Locale = serde_json::from_str("\"zh_tw\"").unwrap();
        let en: Locale = serde_json::from_str("\"en\"").unwrap();
        assert_eq!(zh_cn, Locale::ZhCn);
        assert_eq!(zh_tw, Locale::ZhTw);
        assert_eq!(en, Locale::En);
    }

    #[test]
    fn serde_rejects_unknown_variant() {
        assert!(serde_json::from_str::<Locale>("\"fr\"").is_err());
        assert!(serde_json::from_str::<Locale>("\"zh-CN\"").is_err());
        assert!(serde_json::from_str::<Locale>("\"ZHCN\"").is_err());
    }

    #[test]
    fn locale_is_copy_clone_and_eq() {
        let a = Locale::ZhCn;
        let b = a; // Copy
        assert_eq!(a, b);
        // 验证 Copy：赋值后两边相等（无需显式 clone）。
        assert_eq!(a, Locale::ZhCn);
        // PartialEq / Hash 派生验证（放入 HashSet 不应 panic）
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&a));
    }

    #[test]
    fn default_locale_constant_is_english() {
        const D: Locale = DEFAULT_LOCALE;
        assert_eq!(D, Locale::En);
        assert_eq!(D.code(), "en");
    }
}
