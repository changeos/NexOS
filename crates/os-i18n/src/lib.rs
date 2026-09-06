//! os-i18n —— 国际化（i18n）层
//!
//! 定位：SSOT 翻译资源管理（规划文档 §3.1）。初始支持简中 / 繁中 / 英文三种语言。
//!
//! 本 crate 定义契约（trait + 数据结构 + Error），并内置默认实现 [`BundleTranslator`]
//! （从嵌入式 TOML 资源加载，零运行时文件 IO）与测试用 `MockTranslator`（feature `mock`）。
//!
//! 设计要点：
//! - 所有 trait 同步（翻译查询属轻量内存操作，非数据路径，见全局规范 §1）
//! - 翻译资源以 key 为索引，文案按 Locale 分桶（TranslationBundle）
//! - 组件通过实现 `Localizable` 暴露自己用到的可翻译键（便于校验缺失/导出清单）

pub mod bundle;
pub mod error;
pub mod impl_translator;
pub mod locale;
pub mod translator;

#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::MockTranslator;

pub use bundle::TranslationBundle;
pub use error::{I18nError, I18nResult};
pub use impl_translator::BundleTranslator;
pub use locale::{Locale, DEFAULT_LOCALE};
pub use translator::{fill_placeholders, Localizable, Translator};
