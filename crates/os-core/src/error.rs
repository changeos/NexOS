//! os-core 错误类型

use thiserror::Error;

/// os-core 自身错误（极简——core 几乎不产生业务错误，主要为 EventBus/序列化）
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("序列化错误: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("事件总线错误: {0}")]
    EventBus(String),

    #[error("内部错误: {0}")]
    Internal(String),
}

pub type CoreResult<T> = Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::CoreError as E;

    #[test]
    fn error_display_covers_all_variants() {
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(format!("{}", E::Serde(serde_err)).contains("序列化错误"));
        assert!(format!("{}", E::EventBus("e".into())).contains("事件总线错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
