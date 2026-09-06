//! os-cli 错误类型
//!
//! 设计：每 crate 自定义 `CliError`（thiserror），并实现
//! `From<CliError> for os_common::ApiError`，便于与网关/库内错误统一。

use thiserror::Error;

/// os-cli 错误
#[derive(Debug, Error)]
pub enum CliError {
    /// 命令参数非法（必填缺失 / 类型不符 / 取值越界）
    #[error("非法参数: {0}")]
    InvalidArgs(String),

    /// 命令不存在（未注册的子命令名）
    #[error("命令不存在: {0}")]
    CommandNotFound(String),

    /// 连接 os-api 失败（端点不可达 / 网络错误）
    #[error("API 连接失败: {0}")]
    ApiConnectionFailed(String),

    /// 认证失败（token 无效 / 过期 / 权限不足）
    #[error("认证失败: {0}")]
    AuthFailed(String),

    /// 输出格式化失败（序列化 / 模板渲染错误）
    #[error("输出失败: {0}")]
    OutputFailed(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-cli Result 别名
pub type CliResult<T> = Result<T, CliError>;

// —— From 转换：CliError → ApiError（统一对外错误码）——
impl From<CliError> for os_common::ApiError {
    fn from(e: CliError) -> Self {
        use os_common::ApiErrorCode as Code;
        use CliError as E;
        let (code, msg) = match e {
            E::InvalidArgs(m) => (Code::InvalidInput, m),
            E::CommandNotFound(m) => (Code::NotFound, m),
            E::ApiConnectionFailed(m) => (Code::UpstreamUnavailable, m),
            E::AuthFailed(m) => (Code::PermissionDenied, m),
            E::OutputFailed(m) => (Code::Internal, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::CliError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::InvalidArgs("a".into())).contains("非法参数"));
        assert!(format!("{}", E::CommandNotFound("c".into())).contains("命令不存在"));
        assert!(format!("{}", E::ApiConnectionFailed("a".into())).contains("API 连接失败"));
        assert!(format!("{}", E::AuthFailed("a".into())).contains("认证失败"));
        assert!(format!("{}", E::OutputFailed("o".into())).contains("输出失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
