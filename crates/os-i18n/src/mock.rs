//! `MockTranslator` —— 供下游（api/client/im/service 等）测试用
//!
//! 仅在 `mock` feature 下编译。纯内存、确定性、无外部状态。
//!
//! ```ignore
//! use os_i18n::{Translator, mock::MockTranslator, Locale};
//! let m = MockTranslator::new()
//!     .with("hello", Locale::ZhCn, "你好")
//!     .with("hello", Locale::En, "Hello");
//! assert_eq!(m.t("hello", Locale::ZhCn, &[]), "你好");
//! ```

use crate::translator::{fill_placeholders, Translator};
use crate::{Locale, TranslationBundle, DEFAULT_LOCALE};
use std::collections::HashMap;
use std::sync::RwLock;

/// Mock 翻译器：内存里持有 `(Locale, key) -> 文案` 表，`t()` 命中即返回（填占位符），
/// 未命中走 fallback：`DEFAULT_LOCALE` -> key 本身（与真实实现语义一致，便于下游测试断言）。
pub struct MockTranslator {
    /// `(Locale, key) -> 模板`
    inner: RwLock<HashMap<(Locale, String), String>>,
}

impl MockTranslator {
    /// 构造空 mock（无任何文案）
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// 链式注入一条 (key, locale, msg)。重复注入后者覆盖前者。
    pub fn with(self, key: impl Into<String>, locale: Locale, msg: impl Into<String>) -> Self {
        if let Ok(mut g) = self.inner.write() {
            g.insert((locale, key.into()), msg.into());
        }
        self
    }
}

impl Default for MockTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator for MockTranslator {
    fn t(&self, key: &str, locale: Locale, args: &[(&str, &str)]) -> String {
        let lookup = |loc: Locale| -> Option<String> {
            let g = self.inner.read().ok()?;
            g.get(&(loc, key.to_string())).cloned()
        };

        let template = lookup(locale).or_else(|| lookup(DEFAULT_LOCALE));
        match template {
            Some(t) => fill_placeholders(&t, args),
            // 未注入：返回 key 本身（与 BundleTranslator 一致，绝不 panic）
            None => key.to_string(),
        }
    }

    fn available_locales(&self) -> Vec<Locale> {
        match self.inner.read() {
            Ok(g) => {
                let mut v: Vec<Locale> = g.keys().map(|(l, _)| *l).collect();
                v.sort_by_key(|l| l.code());
                v.dedup();
                v
            }
            Err(_) => Vec::new(),
        }
    }

    fn reload(&self) -> crate::I18nResult<()> {
        // mock 无外部资源，reload 为 no-op（清空不安全，故保留注入数据）
        Ok(())
    }

    fn bundle(&self, key: &str) -> Option<TranslationBundle> {
        let g = self.inner.read().ok()?;
        let mut messages = HashMap::new();
        for ((locale, k), msg) in g.iter() {
            if k == key {
                messages.insert(*locale, msg.clone());
            }
        }
        if messages.is_empty() {
            return None;
        }
        Some(TranslationBundle {
            key: key.to_string(),
            messages,
        })
    }

    fn all_bundles(&self) -> HashMap<String, TranslationBundle> {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return HashMap::new(),
        };
        let mut out: HashMap<String, TranslationBundle> = HashMap::new();
        for ((locale, k), msg) in g.iter() {
            let entry = out
                .entry(k.clone())
                .or_insert_with(|| TranslationBundle::new(k.clone()));
            entry.messages.insert(*locale, msg.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Locale::{En, ZhCn};

    #[test]
    fn with_then_t_hits() {
        let m = MockTranslator::new()
            .with("hi", ZhCn, "你好 {who}")
            .with("hi", En, "Hello {who}");
        assert_eq!(m.t("hi", ZhCn, &[("who", "张三")]), "你好 张三");
        assert_eq!(m.t("hi", En, &[("who", "Z")]), "Hello Z");
    }

    #[test]
    fn fallback_to_default_then_key() {
        let m = MockTranslator::new().with("hi", En, "Hi");
        // zh_cn 未注入 -> fallback en
        assert_eq!(m.t("hi", ZhCn, &[]), "Hi");
        // 完全未注入 -> key 本身
        assert_eq!(m.t("missing", ZhCn, &[]), "missing");
    }

    #[test]
    fn bundle_and_all_bundles() {
        let m = MockTranslator::new().with("k", En, "v_en");
        let b = m.bundle("k").unwrap();
        assert_eq!(b.get(En).unwrap(), "v_en");
        assert!(m.bundle("none").is_none());
        assert!(m.all_bundles().contains_key("k"));
    }

    #[test]
    fn reload_is_noop_and_safe() {
        let m = MockTranslator::new().with("k", En, "v");
        m.reload().unwrap();
        assert_eq!(m.t("k", En, &[]), "v");
    }
}
