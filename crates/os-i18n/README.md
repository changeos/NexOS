# os-i18n

> 国际化层 · SSOT 翻译资源管理 · 初始支持简中 / 繁中 / 英文（规划文档 §3.1）

OS 系统的 i18n 层：翻译资源单一事实源（SSOT）——`Translator` / `Localizable`
trait + `Locale` / `TranslationBundle` 数据结构，内置零运行时文件 IO 的默认实现。

## 核心能力

- **翻译契约**（`translator`）：`Translator` trait（同步——翻译查询属轻量内存操作，
  非数据路径）+ `fill_placeholders` 占位符填充。
- **组件自描述**（`Localizable`）：组件实现它暴露自己用到的可翻译键，
  便于校验缺失 / 导出文案清单。
- **语言与资源模型**（`locale` / `bundle`）：`Locale`（`en` / `zh_cn` / `zh_tw`，
  常量 `DEFAULT_LOCALE`）+ `TranslationBundle`（文案按 Locale 分桶，key 索引）。
- **默认实现**（`impl_translator`）：`BundleTranslator`——从嵌入式 TOML 资源加载
  （`locales/{en,zh_cn,zh_tw}.toml` 经 `include_str!` 编译进二进制），零运行时
  文件 IO、零外部服务依赖。
- **测试桩**：`mock` feature 开启 `MockTranslator`，供下游
  api / client / im / service 测试注入。

## 架构位置

**依赖**（上游）：`os-core`、`os-common`；第三方 serde / serde_json / thiserror /
toml（选 toml 而非 rust-i18n，见 ADR-DEPS-002）。

**被用**（下游）：`os-integration`（dev）；定位为全系统 UI 文案 SSOT，
供各领域 crate / 客户端经 `Translator` 查询文案。

## 独立使用

- **仓库外引用**：`os-i18n = { git = "http://ub2604:8080/git/nexos.git" }`。
- **关键接口**：
  - `Translator`：`fn t(&self, key, locale, args) -> String` 同步查询（带占位符
    参数），配套 `available_locales()` / `reload()`；实现方 `BundleTranslator`，
    测试 `MockTranslator`。
  - `Localizable`：`fn i18n_keys()` 暴露组件用到的可翻译键集合。
  - `TranslationBundle`：`new(key)` + `with_message(locale, msg)` 链式构造自定义
    资源分桶，`get(locale)` 查询。
- **feature**：`mock`（默认关）——`MockTranslator` 测试桩。

## 测试

```bash
cargo test -p os-i18n
```

覆盖 Locale 解析 / TOML 解析（嵌套表、多行串）/ bundle 分桶 / 占位符填充 /
三语言嵌入式资源加载与 fallback 链（locale → `DEFAULT_LOCALE` → key 本身，
`t()` 永不 panic）。
