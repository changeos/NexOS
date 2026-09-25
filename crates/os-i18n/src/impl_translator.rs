//! `BundleTranslator` —— `Translator` 的内置默认实现
//!
//! 设计目标（见规格书 §3 / §9 红线）：
//! - **真实 TOML 解析**：使用 `toml` crate（workspace 已注册，见 ADR-DEPS-002）做完整
//!   TOML 解析，再递归扁平化为 `full_key -> 文案模板` 的查找表（见 `parse_toml`）。
//!   支持 TOML 全语法：行注释 / 节头 / 嵌套表 / 内联表 / 数组 / 多行字符串 / 转义等。
//!   相比早期自写「扁平子集」解析器，业务资源文件可自由使用任意 TOML 语法。
//! - **编译期嵌入资源**：用 `include_str!` 把三语资源打进二进制，部署零额外文件。
//! - **并发友好**：内部用 `RwLock<HashMap>` 持有内存索引，`t()` 走读锁，多线程并发读安全。
//! - **永不 panic**：`t()` 按 locale → `DEFAULT_LOCALE` → key 本身 fallback。

use crate::translator::{fill_placeholders, Translator};
use crate::{I18nError, I18nResult, Locale, TranslationBundle, DEFAULT_LOCALE};
use std::collections::HashMap;
use std::sync::RwLock;

/// 内嵌的三语资源（编译期 `include_str!`，部署零额外文件）
const EN_TOML: &str = include_str!("../locales/en.toml");
const ZH_CN_TOML: &str = include_str!("../locales/zh_cn.toml");
const ZH_TW_TOML: &str = include_str!("../locales/zh_tw.toml");

/// 单语言翻译表：`full_key -> 文案模板`（如 `"pool.created" -> "存储池 {name} 已创建"`）
type LocaleTable = HashMap<String, String>;

/// 内置默认实现：从嵌入式 TOML 资源加载翻译表，提供并发只读查询。
///
/// 持有 `Locale -> LocaleTable` 的内存索引，`t()` 为 O(1) HashMap 查找。
/// `reload()` 重新解析嵌入资源（当前嵌入资源恒定，主要用于契约完整性与未来
/// 从外部目录加载的实现兼容）。
pub struct BundleTranslator {
    /// 内存索引；`RwLock` 支持多线程并发读 + reload 时独占写
    inner: RwLock<HashMap<Locale, LocaleTable>>,
}

impl BundleTranslator {
    /// 从嵌入式默认资源构造（最常用入口）
    ///
    /// 解析失败返回 `I18nError::ParseFailed`（嵌入资源由本 crate 维护，正常不应失败）。
    pub fn new() -> I18nResult<Self> {
        Self::from_toml(EN_TOML, ZH_CN_TOML, ZH_TW_TOML)
    }

    /// 从三段 TOML 文本构造（便于测试注入自定义资源）
    ///
    /// 三个参数依次为 en / zh_cn / zh_tw 的 TOML 文本。某语言传入空串表示该语言不提供。
    pub fn from_toml(en: &str, zh_cn: &str, zh_tw: &str) -> I18nResult<Self> {
        let mut map: HashMap<Locale, LocaleTable> = HashMap::new();
        if !en.is_empty() {
            map.insert(Locale::En, parse_toml(en)?);
        }
        if !zh_cn.is_empty() {
            map.insert(Locale::ZhCn, parse_toml(zh_cn)?);
        }
        if !zh_tw.is_empty() {
            map.insert(Locale::ZhTw, parse_toml(zh_tw)?);
        }
        Ok(Self {
            inner: RwLock::new(map),
        })
    }

    /// 查询某 (locale, key) 的原始模板（未填占位符）；不走 fallback。
    fn lookup(&self, locale: Locale, key: &str) -> Option<String> {
        let guard = self.inner.read().ok()?;
        guard.get(&locale).and_then(|tbl| tbl.get(key).cloned())
    }
}

impl Default for BundleTranslator {
    fn default() -> Self {
        // Default 委托 new()；嵌入资源解析失败时回退到空表（永不 panic）
        Self::new().unwrap_or_else(|_| Self {
            inner: RwLock::new(HashMap::new()),
        })
    }
}

impl Translator for BundleTranslator {
    fn t(&self, key: &str, locale: Locale, args: &[(&str, &str)]) -> String {
        // fallback 链：指定 locale -> DEFAULT_LOCALE -> key 本身（绝不 panic / 空串）
        let template = self
            .lookup(locale, key)
            .or_else(|| self.lookup(DEFAULT_LOCALE, key));

        match template {
            Some(t) => fill_placeholders(&t, args),
            // 全部缺失：返回 key 本身，便于前端发现遗漏
            None => key.to_string(),
        }
    }

    fn available_locales(&self) -> Vec<Locale> {
        match self.inner.read() {
            Ok(g) => {
                let mut v: Vec<Locale> = g.keys().copied().collect();
                v.sort_by_key(|l| l.code());
                v
            }
            // 锁中毒不应发生在正常使用；退化为空列表
            Err(_) => Vec::new(),
        }
    }

    fn reload(&self) -> I18nResult<()> {
        // 重新解析嵌入资源并替换索引（写锁）
        let fresh = Self::from_toml(EN_TOML, ZH_CN_TOML, ZH_TW_TOML)?;
        let mut guard = self
            .inner
            .write()
            .map_err(|_| I18nError::LoadFailed("i18n 索引锁中毒".into()))?;
        *guard = fresh.inner.into_inner().unwrap_or_default();
        Ok(())
    }

    fn bundle(&self, key: &str) -> Option<TranslationBundle> {
        let guard = self.inner.read().ok()?;
        let mut messages = HashMap::new();
        for (locale, tbl) in guard.iter() {
            if let Some(msg) = tbl.get(key) {
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
        let guard = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return HashMap::new(),
        };
        // 收集所有 key（跨语言并集）
        let mut all_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for tbl in guard.values() {
            all_keys.extend(tbl.keys().cloned());
        }
        all_keys
            .into_iter()
            .map(|k| {
                let mut messages = HashMap::new();
                for (locale, tbl) in guard.iter() {
                    if let Some(msg) = tbl.get(&k) {
                        messages.insert(*locale, msg.clone());
                    }
                }
                (k.clone(), TranslationBundle { key: k, messages })
            })
            .collect()
    }
}

/// 用 `toml` crate 解析 TOML 文本，递归扁平化为 `full_key -> 文案模板` 表
///
/// 支持完整 TOML 语法：行注释 / 节头 / 嵌套表 / 内联表 / 数组 / 多行字符串 / 基本字符串转义等。
/// 仅叶子值为字符串的键会被收集（翻译文案恒为字符串）；嵌套表（`[a.b]` 或
/// `a.b = {...}`）的 key 路径用 `.` 拼接成全名（如 `pool.created`）。非字符串
/// 叶子值（整数 / 布尔 / 数组 / 日期）会被跳过——i18n 文案模板只关心字符串。
///
/// 解析失败（语法非法）返回 `I18nError::ParseFailed`，附带 toml crate 原始错误信息。
fn parse_toml(input: &str) -> I18nResult<LocaleTable> {
    let root: toml::Table =
        toml::from_str(input).map_err(|e| I18nError::ParseFailed(e.to_string()))?;
    let mut table: LocaleTable = HashMap::new();
    flatten_table(&root, "", &mut table);
    Ok(table)
}

/// 递归遍历 `toml::Table`，把字符串叶子以 `prefix.key` 为全名写入 `out`
///
/// - `prefix` 为到达当前表之前的路径前缀（顶层为空串，下钻一层后为该节名）；
/// - 遇到 `Value::String(s)` 写入 `out[prefix.key] = s`；
/// - 遇到 `Value::Table(t)` 以 `key` 作为新前缀递归；
/// - 其他值类型（整数 / 布尔 / 数组 / 日期）跳过（非翻译文案）。
fn flatten_table(table: &toml::Table, prefix: &str, out: &mut LocaleTable) {
    for (key, value) in table {
        let full_key = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::String(s) => {
                out.insert(full_key, s.clone());
            }
            toml::Value::Table(t) => {
                flatten_table(t, &full_key, out);
            }
            // 非字符串标量与数组：翻译文案不关心，直接跳过（保留对应键不产生条目）
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Locale::{En, ZhCn, ZhTw};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn parses_sections_and_keys_with_comments() {
        // 完整 TOML：顶部注释 + 节头 + 尾注释 + 基本/字面字符串
        let toml_text = r#"
# 顶部注释
[error]
not_found = "未找到：{name}"   # 尾注释
internal = "内部错误"
"#;
        let table = parse_toml(toml_text).unwrap();
        assert_eq!(table.get("error.not_found").unwrap(), "未找到：{name}");
        assert_eq!(table.get("error.internal").unwrap(), "内部错误");
    }

    #[test]
    fn parse_toml_supports_nested_tables() {
        // 自写解析器不支持的嵌套表（[a.b.c] 节头 + dotted key），toml crate 支持
        let toml_text = r#"
[a.b.c]
deep = "深嵌套值"

[pool]
"created.alt" = "带引号的点路径键"
"#;
        let table = parse_toml(toml_text).unwrap();
        assert_eq!(table.get("a.b.c.deep").unwrap(), "深嵌套值");
        // 带引号的键 "created.alt" 是单个键名（不被拆成路径），故全名为 pool.created.alt
        assert_eq!(table.get("pool.created.alt").unwrap(), "带引号的点路径键");
    }

    #[test]
    fn parse_toml_supports_multiline_strings() {
        // 多行基本字符串（"""..."""）——自写解析器不支持，toml crate 支持
        let toml_text = r#"
[multi]
banner = """
第一行
第二行"""
"#;
        let table = parse_toml(toml_text).unwrap();
        let banner = table.get("multi.banner").unwrap();
        assert!(banner.contains("第一行"));
        assert!(banner.contains("第二行"));
    }

    #[test]
    fn parse_toml_supports_inline_tables_and_arrays() {
        // 内联表 + 数组：字符串元素仍是文案候选；非字符串叶子（数组本身）被跳过
        let toml_text = r#"
[ui]
title = { en = "Storage", zh = "存储" }
tags = ["a", "b", "c"]
"#;
        let table = parse_toml(toml_text).unwrap();
        // 内联表扁平化
        assert_eq!(table.get("ui.title.en").unwrap(), "Storage");
        assert_eq!(table.get("ui.title.zh").unwrap(), "存储");
        // 数组作为整体不是字符串叶子，不应作为翻译文案出现
        assert!(!table.contains_key("ui.tags"));
    }

    #[test]
    fn parse_toml_supports_escape_sequences() {
        // 基本字符串转义：toml crate 处理 \n / \" / \\（自写解析器也曾支持）
        let toml_text = r#"
[fmt]
escapes = "a\nb\"c\\d"
"#;
        let table = parse_toml(toml_text).unwrap();
        assert_eq!(table.get("fmt.escapes").unwrap(), "a\nb\"c\\d");
    }

    #[test]
    fn new_loads_embedded_resources() {
        let bt = BundleTranslator::new().unwrap();
        // 英文
        assert_eq!(bt.t("common.ok", En, &[]), "OK");
        // 简中
        assert_eq!(bt.t("common.ok", ZhCn, &[]), "确定");
        // 繁中
        assert_eq!(bt.t("common.ok", ZhTw, &[]), "確定");
    }

    #[test]
    fn fallback_chain_locale_missing_then_default_then_key() {
        // 构造一个仅含英文（默认语言）的 translator
        let bt = BundleTranslator::from_toml(r#"common.ok = "OK""#, "", "").unwrap();
        // zh_cn 缺失 -> fallback 到 en
        assert_eq!(bt.t("common.ok", ZhCn, &[]), "OK");
        assert_eq!(bt.t("common.ok", ZhTw, &[]), "OK");
        // 完全缺失的 key -> 返回 key 本身
        assert_eq!(bt.t("no.such.key", ZhCn, &[]), "no.such.key");
        assert_eq!(bt.t("no.such.key", En, &[]), "no.such.key");
    }

    #[test]
    fn placeholder_substitution() {
        let bt = BundleTranslator::new().unwrap();
        let msg = bt.t("pool.created", ZhCn, &[("name", "tank")]);
        assert_eq!(msg, "存储池 tank 已创建");
        // 未匹配占位符原样保留
        let msg2 = bt.t("pool.created", ZhCn, &[]);
        assert_eq!(msg2, "存储池 {name} 已创建");
    }

    #[test]
    fn available_locales_sorted() {
        let bt = BundleTranslator::new().unwrap();
        let locales = bt.available_locales();
        assert!(locales.contains(&En));
        assert!(locales.contains(&ZhCn));
        assert!(locales.contains(&ZhTw));
        // 已排序（按 code）
        let codes: Vec<&str> = locales.iter().map(|l| l.code()).collect();
        assert_eq!(codes, vec!["en", "zh_cn", "zh_tw"]);
    }

    #[test]
    fn reload_replaces_index() {
        let bt = BundleTranslator::new().unwrap();
        // reload 重新解析嵌入资源
        bt.reload().unwrap();
        assert_eq!(bt.t("common.cancel", ZhCn, &[]), "取消");
    }

    #[test]
    fn bundle_returns_aggregated_messages_or_none() {
        let bt = BundleTranslator::new().unwrap();
        let b = bt.bundle("common.ok").expect("common.ok 应存在");
        assert_eq!(b.key, "common.ok");
        assert_eq!(b.get(En).unwrap(), "OK");
        assert_eq!(b.get(ZhCn).unwrap(), "确定");
        assert!(bt.bundle("no.such.key").is_none());
    }

    #[test]
    fn all_bundles_union_of_keys() {
        let bt = BundleTranslator::new().unwrap();
        let all = bt.all_bundles();
        assert!(all.contains_key("common.ok"));
        assert!(all.contains_key("pool.created"));
        assert!(all.contains_key("error.internal"));
    }

    #[test]
    fn concurrent_reads_are_safe() {
        let bt = Arc::new(BundleTranslator::new().unwrap());
        let mut handles = vec![];
        for i in 0..8 {
            let bt = Arc::clone(&bt);
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    let m = bt.t("pool.created", ZhCn, &[("name", "t")]);
                    assert!(m.contains("存储池"));
                    let _ = bt.t("no.such.key", En, &[]);
                    let _ = bt.available_locales();
                    // 每隔几次触发 reload，验证读写锁不互相踩
                    if i == 0 {
                        let _ = bt.reload();
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("线程不应 panic");
        }
    }

    #[test]
    fn parse_failure_reports_error() {
        // 真正的 TOML 语法错误：未闭合的字符串字面量
        let bad = "x = \"未闭合";
        let err = parse_toml(bad).unwrap_err();
        match err {
            I18nError::ParseFailed(_) => {}
            other => panic!("期望 ParseFailed，得到 {other:?}"),
        }
    }

    #[test]
    fn translator_handles_advanced_toml_end_to_end() {
        // 端到端：自写解析器不支持的语法（嵌套表 + 内联表 + 多行）经 toml crate 解析后，
        // 经 BundleTranslator 查询仍能命中并填占位符；fallback 链不受影响。
        let en = r#"
[network]
label = "Network"

[network.detail]
multiline = """
Line 1: {host}
Line 2"""

[ui]
button = { text = "Connect" }
"#;
        let zh_cn = r#"
[network]
label = "网络"

[network.detail]
multiline = """
第一行：{host}
第二行"""
"#;
        let bt = BundleTranslator::from_toml(en, zh_cn, "").unwrap();
        // 嵌套表扁平化
        assert_eq!(bt.t("network.label", ZhCn, &[]), "网络");
        assert_eq!(bt.t("network.label", En, &[]), "Network");
        // 内联表
        assert_eq!(bt.t("ui.button.text", En, &[]), "Connect");
        // zh_cn 缺失 ui.button.text -> fallback 到 en
        assert_eq!(bt.t("ui.button.text", ZhCn, &[]), "Connect");
        // 多行字符串 + 占位符
        let m = bt.t("network.detail.multiline", ZhCn, &[("host", "1.2.3.4")]);
        assert!(m.contains("第一行：1.2.3.4"));
        assert!(m.contains("第二行"));
    }

    // —— 覆盖率补充：from_toml / Default / available_locales / bundle / all_bundles 边界 ——

    #[test]
    fn from_toml_empty_strings_yield_no_locales() {
        // 三段全空 -> 无 locale 加载，但仍构造成功（空 map）。
        let bt = BundleTranslator::from_toml("", "", "").unwrap();
        assert!(bt.available_locales().is_empty());
        // 任意 key 查询都走 fallback：DEFAULT_LOCALE 也缺失 -> 返回 key 本身。
        assert_eq!(bt.t("any.key", En, &[]), "any.key");
        assert_eq!(bt.t("any.key", ZhCn, &[]), "any.key");
        assert_eq!(bt.t("any.key", ZhTw, &[]), "any.key");
    }

    #[test]
    fn from_toml_partial_locales_loaded() {
        // 仅加载 zh_tw；查询 en（默认）缺失 -> 走 fallback 链到 key 本身。
        let bt = BundleTranslator::from_toml("", "", "common.ok = \"確定\"").unwrap();
        let locales = bt.available_locales();
        assert_eq!(locales, vec![ZhTw]);
        assert_eq!(bt.t("common.ok", ZhTw, &[]), "確定");
        // DEFAULT_LOCALE (En) 缺失 -> 直接 fallback 到 key 本身（不再走中间 zh_tw）。
        assert_eq!(bt.t("common.ok", En, &[]), "common.ok");
    }

    #[test]
    fn from_toml_invalid_returns_parse_failed() {
        // 语法非法的 en -> ParseFailed。
        let r = BundleTranslator::from_toml("x = \"未闭合", "", "");
        assert!(matches!(r, Err(I18nError::ParseFailed(_))));
        // zh_cn 语法非法也同理。
        let r = BundleTranslator::from_toml("", "= nokey", "");
        assert!(matches!(r, Err(I18nError::ParseFailed(_))));
        // zh_tw 语法非法也同理。
        let r = BundleTranslator::from_toml("", "", "[unclosed");
        assert!(matches!(r, Err(I18nError::ParseFailed(_))));
    }

    #[test]
    fn default_returns_working_translator() {
        // Default::default() 不应 panic（嵌入资源恒定有效）。
        let bt = BundleTranslator::default();
        assert!(!bt.available_locales().is_empty());
        assert_eq!(bt.t("common.ok", En, &[]), "OK");
    }

    #[test]
    fn available_locales_sorted_by_code_asc() {
        let bt = BundleTranslator::new().unwrap();
        let locales = bt.available_locales();
        let codes: Vec<&str> = locales.iter().map(|l| l.code()).collect();
        // 排序后按字典序：en < zh_cn < zh_tw。
        assert_eq!(codes, vec!["en", "zh_cn", "zh_tw"]);
        // 含三语。
        assert_eq!(locales.len(), 3);
    }

    #[test]
    fn bundle_partial_locales_returns_only_loaded() {
        // 仅 en 加载了某 key -> bundle.messages 只含 en。
        let bt = BundleTranslator::from_toml(r#"k.v = "en-only""#, "", "").unwrap();
        let b = bt.bundle("k.v").unwrap();
        assert_eq!(b.key, "k.v");
        assert_eq!(b.messages.len(), 1);
        assert_eq!(b.get(En).unwrap(), "en-only");
        assert!(b.get(ZhCn).is_none());
    }

    #[test]
    fn all_bundles_empty_when_no_resources() {
        let bt = BundleTranslator::from_toml("", "", "").unwrap();
        assert!(bt.all_bundles().is_empty());
    }

    #[test]
    fn all_bundles_union_across_locales() {
        // 不同 locale 各持有不同的 key 子集，all_bundles 应取并集。
        let bt = BundleTranslator::from_toml(
            "en_only = \"EN\"\ncommon = \"shared\"\n",
            "zh_only = \"ZH\"\ncommon = \"共享\"\n",
            "tw_only = \"TW\"\n",
        )
        .unwrap();
        let all = bt.all_bundles();
        // 并集应含全部 4 个 key。
        assert!(all.contains_key("en_only"));
        assert!(all.contains_key("zh_only"));
        assert!(all.contains_key("tw_only"));
        assert!(all.contains_key("common"));
        // common 在两语言中都有 -> bundle.messages.len()==2。
        assert_eq!(all.get("common").unwrap().messages.len(), 2);
        // tw_only 仅在 zh_tw 中 -> bundle.messages.len()==1。
        assert_eq!(all.get("tw_only").unwrap().messages.len(), 1);
    }

    #[test]
    fn t_with_multiple_placeholders_substitutes_all() {
        let bt = BundleTranslator::from_toml(r#"greet = "Hi {name}, welcome to {place}!""#, "", "")
            .unwrap();
        let out = bt.t("greet", En, &[("name", "Alice"), ("place", "OS")]);
        assert_eq!(out, "Hi Alice, welcome to OS!");
    }

    #[test]
    fn t_falls_back_from_zh_tw_via_default_when_zh_tw_missing() {
        // zh_tw 缺失但 en 存在 -> 走 fallback 到 DEFAULT_LOCALE。
        let bt = BundleTranslator::from_toml(r#"k = "EN""#, "", "").unwrap();
        assert_eq!(bt.t("k", ZhTw, &[]), "EN");
        assert_eq!(bt.t("k", ZhCn, &[]), "EN");
    }

    #[test]
    fn reload_after_partial_load_restores_all_three() {
        // 先构造一个只有 en 的 translator，reload 后从嵌入资源恢复三语。
        let bt = BundleTranslator::from_toml(r#"k = "tmp""#, "", "").unwrap();
        assert_eq!(bt.available_locales(), vec![En]);
        bt.reload().unwrap();
        // reload 后应有三语。
        let locales = bt.available_locales();
        assert_eq!(locales.len(), 3);
        // 嵌入资源中的 common.ok 应可查询。
        assert_eq!(bt.t("common.ok", ZhCn, &[]), "确定");
    }

    #[test]
    fn flatten_table_skips_non_string_leaves() {
        // 整数 / 布尔 / 数组 / 浮点 等非字符串叶子应被跳过。
        let toml_text = r#"
[a]
str = "保留"
int = 42
bool = true
float = 3.14
arr = [1, 2, 3]

[a.sub]
deep = "深嵌套字符串"
"#;
        let table = parse_toml(toml_text).unwrap();
        assert_eq!(table.get("a.str").unwrap(), "保留");
        assert_eq!(table.get("a.sub.deep").unwrap(), "深嵌套字符串");
        // 非字符串叶子不应作为翻译条目。
        assert!(!table.contains_key("a.int"));
        assert!(!table.contains_key("a.bool"));
        assert!(!table.contains_key("a.float"));
        assert!(!table.contains_key("a.arr"));
    }

    #[test]
    fn flatten_table_deeply_nested_paths() {
        // 多层嵌套：路径用 `.` 拼接。
        let toml_text = r#"
[a.b.c.d.e]
leaf = "deep-value"
"#;
        let table = parse_toml(toml_text).unwrap();
        assert_eq!(table.get("a.b.c.d.e.leaf").unwrap(), "deep-value");
    }

    #[test]
    fn parse_toml_empty_input_yields_empty_table() {
        let table = parse_toml("").unwrap();
        assert!(table.is_empty());
    }

    #[test]
    fn parse_toml_whitespace_only_input_yields_empty_table() {
        let table = parse_toml("   \n\t  \n").unwrap();
        assert!(table.is_empty());
    }
}
