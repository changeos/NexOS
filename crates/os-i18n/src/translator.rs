//! Translator / Localizable trait —— 翻译查询契约
//!
//! - `Translator`：翻译资源后端（查询/列举/热重载），由 i18n 服务实现
//! - `Localizable`：组件实现它暴露自己用到的可翻译键（便于缺失校验/导出清单）

use crate::{Locale, TranslationBundle};
use std::collections::HashMap;

/// 翻译器 trait（同步）
///
/// 实现者持有 SSOT 翻译资源（内存索引），提供 O(1) 查询。
/// 默认实现可从嵌入式 JSON / 文件目录加载（实现由 owner agent 提供）。
///
/// 并发性：实现应内部加锁或用并发只读结构，支持多读线程并发调用 `t()`。
pub trait Translator: Send + Sync {
    /// 翻译指定 key 到目标语言
    ///
    /// - `args`：命名占位符（如 `[("name", "tank")]`），替换模板中的 `{name}`
    /// - key 缺失或某语言缺失时：实现应 fallback 到 `DEFAULT_LOCALE`，仍缺失则返回
    ///   key 本身（绝不返回空串或 panic），便于前端发现遗漏
    fn t(&self, key: &str, locale: Locale, args: &[(&str, &str)]) -> String;

    /// 当前可用语言列表（资源已加载的语言）
    fn available_locales(&self) -> Vec<Locale>;

    /// 热重载翻译资源（运行时切换语言包无需重启）
    ///
    /// 失败原因：资源文件不存在/解析失败，详见 [`crate::I18nError`]
    fn reload(&self) -> crate::I18nResult<()>;

    /// 取指定 key 的完整 bundle（用于校验/导出；默认实现可选）
    ///
    /// 返回 None 表示该 key 不存在或实现未提供原始 bundle 查询。
    fn bundle(&self, _key: &str) -> Option<TranslationBundle> {
        None
    }

    /// 返回所有 bundle 的快照（用于导出/全量校验；默认空）
    ///
    /// 实现可返回 `HashMap<key, TranslationBundle>`。大型资源库可实现为按需加载。
    fn all_bundles(&self) -> HashMap<String, TranslationBundle> {
        HashMap::new()
    }
}

/// 可本地化 trait（同步）
///
/// 组件（如存储/网络/钱包模块）实现它，暴露自身用到的所有翻译键。
/// 用途：
/// - CI 脚本扫描全量键，校验资源文件无缺失
/// - 运维导出「待翻译键清单」交给翻译人员
/// - 帮助前端预加载对应文案
pub trait Localizable {
    /// 该组件用到的翻译键列表（点分层级，如 `["pool.created", "pool.destroyed"]`）
    fn i18n_keys(&self) -> Vec<String>;
}

/// 默认占位符替换工具函数（供实现复用）
///
/// 将模板中所有 `{name}` 替换为 args 中对应的 value；未匹配的占位符原样保留。
/// 这是最小实现——实现者可替换为更完整的 ICU MessageFormat 处理器。
pub fn fill_placeholders(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        let placeholder = format!("{{{k}}}");
        out = out.replace(&placeholder, v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_placeholders_no_args_returns_template() {
        assert_eq!(fill_placeholders("plain text", &[]), "plain text");
        // 含占位符但无 args：原样保留。
        assert_eq!(fill_placeholders("hi {name}", &[]), "hi {name}");
    }

    #[test]
    fn fill_placeholders_replaces_single() {
        assert_eq!(
            fill_placeholders("hello {name}", &[("name", "world")]),
            "hello world"
        );
    }

    #[test]
    fn fill_placeholders_replaces_multiple_occurrences() {
        // 同一占位符在模板中出现多次，应全部替换。
        assert_eq!(
            fill_placeholders("{x} and {x} again", &[("x", "v")]),
            "v and v again"
        );
    }

    #[test]
    fn fill_placeholders_replaces_multiple_distinct_keys() {
        let out = fill_placeholders("{a}/{b}/{c}", &[("a", "1"), ("b", "2"), ("c", "3")]);
        assert_eq!(out, "1/2/3");
    }

    #[test]
    fn fill_placeholders_unmatched_placeholder_preserved() {
        // 模板里的占位符若不在 args，原样保留。
        assert_eq!(
            fill_placeholders("{matched} {unmatched}", &[("matched", "ok")]),
            "ok {unmatched}"
        );
    }

    #[test]
    fn fill_placeholders_empty_value_replaces() {
        assert_eq!(fill_placeholders("a{x}b", &[("x", "")]), "ab");
    }

    #[test]
    fn fill_placeholders_treats_braces_without_name_as_plain_text() {
        // 普通花括号（无标识符）不被视为占位符。
        assert_eq!(
            fill_placeholders("{ not a placeholder }", &[]),
            "{ not a placeholder }"
        );
    }

    #[test]
    fn fill_placeholders_handles_unicode_values() {
        assert_eq!(
            fill_placeholders("你好 {name}", &[("name", "世界")]),
            "你好 世界"
        );
    }

    #[test]
    fn localizable_default_i18n_keys_returns_provided() {
        struct Demo;
        impl Localizable for Demo {
            fn i18n_keys(&self) -> Vec<String> {
                vec!["a.b".to_string(), "c.d".to_string()]
            }
        }
        let d = Demo;
        assert_eq!(d.i18n_keys(), vec!["a.b".to_string(), "c.d".to_string()]);
    }

    #[test]
    fn translator_bundle_default_returns_none() {
        struct NoBundle;
        impl Translator for NoBundle {
            fn t(&self, _k: &str, _l: Locale, _a: &[(&str, &str)]) -> String {
                String::new()
            }
            fn available_locales(&self) -> Vec<Locale> {
                vec![]
            }
            fn reload(&self) -> crate::I18nResult<()> {
                Ok(())
            }
        }
        let t = NoBundle;
        // bundle 默认实现应返回 None。
        assert!(t.bundle("any").is_none());
        // all_bundles 默认实现应返回空 map。
        assert!(t.all_bundles().is_empty());
    }
}
