//! TranslationBundle —— 单条翻译资源（key → 各语言文案）
//!
//! 一条 bundle 描述一个翻译 key 在所有语言下的文案；
//! 翻译资源库即 `Vec<TranslationBundle>` 或 `HashMap<String, TranslationBundle>`。

use crate::Locale;
use os_core::{Deserialize, Serialize};
use std::collections::HashMap;

/// 单条翻译资源
///
/// 文案支持 ICU `{name}` 风格的命名占位符（由 Translator 在 `t()` 时用 `args` 填充）。
/// 示例：
/// ```ignore
/// TranslationBundle {
///     key: "pool.created".into(),
///     messages: HashMap::from([
///         (Locale::En,   "Pool {name} created".into()),
///         (Locale::ZhCn, "存储池 {name} 已创建".into()),
///         (Locale::ZhTw, "儲存池 {name} 已建立".into()),
///     ]),
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationBundle {
    /// 翻译键（点分层级，如 `pool.created` / `error.disk_failed`）
    pub key: String,

    /// 各语言文案（key 为 Locale，value 为带占位符的模板字符串）
    ///
    /// 缺失某语言时，Translator 应 fallback 到 `DEFAULT_LOCALE`（见 §3.1）
    #[serde(default)]
    pub messages: HashMap<Locale, String>,
}

impl TranslationBundle {
    /// 构造一条空 bundle（仅 key，无文案）
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            messages: HashMap::new(),
        }
    }

    /// 链式追加某语言文案
    pub fn with_message(mut self, locale: Locale, message: impl Into<String>) -> Self {
        self.messages.insert(locale, message.into());
        self
    }

    /// 取指定语言的文案；若缺失返回 None（由调用方决定 fallback 策略）
    pub fn get(&self, locale: Locale) -> Option<&str> {
        self.messages.get(&locale).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_empty_bundle() {
        let b = TranslationBundle::new("pool.created");
        assert_eq!(b.key, "pool.created");
        assert!(b.messages.is_empty());
        // 任意 locale 查询都返回 None。
        assert!(b.get(Locale::En).is_none());
        assert!(b.get(Locale::ZhCn).is_none());
    }

    #[test]
    fn with_message_chains_and_inserts() {
        let b = TranslationBundle::new("pool.created")
            .with_message(Locale::En, "Pool {name} created")
            .with_message(Locale::ZhCn, "存储池 {name} 已创建");
        assert_eq!(b.messages.len(), 2);
        assert_eq!(b.get(Locale::En).unwrap(), "Pool {name} created");
        assert_eq!(b.get(Locale::ZhCn).unwrap(), "存储池 {name} 已创建");
        // 未配置语言返回 None。
        assert!(b.get(Locale::ZhTw).is_none());
    }

    #[test]
    fn with_message_overwrites_same_locale() {
        let b = TranslationBundle::new("k")
            .with_message(Locale::En, "first")
            .with_message(Locale::En, "second");
        assert_eq!(b.messages.len(), 1);
        assert_eq!(b.get(Locale::En).unwrap(), "second");
    }

    #[test]
    fn serde_roundtrip_preserves_bundle() {
        let b = TranslationBundle::new("error.internal")
            .with_message(Locale::En, "Internal error")
            .with_message(Locale::ZhCn, "内部错误")
            .with_message(Locale::ZhTw, "內部錯誤");
        let s = serde_json::to_string(&b).unwrap();
        let back: TranslationBundle = serde_json::from_str(&s).unwrap();
        assert_eq!(back.key, "error.internal");
        assert_eq!(back.messages.len(), 3);
        assert_eq!(back.get(Locale::En).unwrap(), "Internal error");
        assert_eq!(back.get(Locale::ZhCn).unwrap(), "内部错误");
        assert_eq!(back.get(Locale::ZhTw).unwrap(), "內部錯誤");
    }

    #[test]
    fn serde_deserializes_with_default_messages() {
        // 缺 messages 字段时按 #[serde(default)] 解析为空 map。
        let v: TranslationBundle = serde_json::from_str(r#"{"key":"x"}"#).unwrap();
        assert_eq!(v.key, "x");
        assert!(v.messages.is_empty());
    }

    #[test]
    fn get_returns_str_borrowed_from_message() {
        let b = TranslationBundle::new("k").with_message(Locale::En, "hello");
        // 验证 get 返回 &str（不是拥有 String）。
        let s: &str = b.get(Locale::En).unwrap();
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn new_accepts_string_and_str() {
        // impl Into<String> 接受多种字符串类型。
        let from_string = TranslationBundle::new(String::from("k1"));
        let from_str = TranslationBundle::new("k2");
        let from_box = translation_box_new_helper();
        assert_eq!(from_string.key, "k1");
        assert_eq!(from_str.key, "k2");
        assert_eq!(from_box.key, "k3");
    }

    fn translation_box_new_helper() -> TranslationBundle {
        TranslationBundle::new(Box::<str>::from("k3").to_string())
    }
}
