//! os-i18n 错误类型

use thiserror::Error;

/// i18n 层错误
#[derive(Debug, Error)]
pub enum I18nError {
    /// 翻译键缺失（fallback 已用尽，查询返回 key 本身时仅记录告警，非错误；
    /// 此变体用于「严格模式」校验场景——实现可选择抛错而非静默 fallback）
    #[error("翻译键缺失: {key}")]
    MissingKey { key: String },

    /// 翻译资源加载失败（文件不存在 / IO 错误 / 网络拉取失败）
    #[error("翻译资源加载失败: {0}")]
    LoadFailed(String),

    /// 翻译资源解析失败（JSON 非法 / schema 不符）
    #[error("翻译资源解析失败: {0}")]
    ParseFailed(String),
}

/// i18n Result 别名
pub type I18nResult<T> = Result<T, I18nError>;

// —— From 转换：I18nError → os-common::ApiError ——
//
// os-api 网关把任意 crate Error 统一转 ApiError 序列化返回前端（呼应 §12.3）。
// i18n 错误（翻译键缺失 / 资源加载失败 / 解析失败）目前无对应细分错误码，
// 统一映射到 Internal，原始信息放 message。
impl From<I18nError> for os_common::ApiError {
    fn from(e: I18nError) -> Self {
        os_common::ApiError::internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::I18nError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::MissingKey { key: "k".into() }).contains("翻译键缺失"));
        assert!(format!("{}", E::LoadFailed("l".into())).contains("翻译资源加载失败"));
        assert!(format!("{}", E::ParseFailed("p".into())).contains("翻译资源解析失败"));
    }
}
