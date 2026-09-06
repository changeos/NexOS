//! os-update 错误类型
//!
//! 设计：每 crate 自定义 `UpdateError`（thiserror），并实现
//! `From<UpdateError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-update 错误
#[derive(Debug, Error)]
pub enum UpdateError {
    /// 无可用更新
    #[error("无可用更新")]
    NoUpdates,

    /// 下载失败（网络/源不可用）
    #[error("下载失败: {0}")]
    DownloadFailed(String),

    /// 校验失败（签名无效/sha256 不匹配）
    #[error("校验失败: {0}")]
    VerificationFailed(String),

    /// 写入非活动槽位失败
    #[error("写入槽位失败: {0}")]
    WriteFailed(String),

    /// 槽位冲突（两槽均活动/无可写槽）
    #[error("槽位冲突: {0}")]
    SlotConflict(String),

    /// 回滚失败
    #[error("回滚失败: {0}")]
    RollbackFailed(String),

    /// CVE 检查失败（源不可用/解析错）
    #[error("CVE 检查失败: {0}")]
    CveCheckFailed(String),

    /// 健康检查失败（启动后探活不通过）
    #[error("健康检查失败: {0}")]
    HealthCheckFailed(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-update Result 别名
pub type UpdateResult<T> = Result<T, UpdateError>;

// —— From 转换：UpdateError → ApiError（统一对外错误码）——
impl From<UpdateError> for os_common::ApiError {
    fn from(e: UpdateError) -> Self {
        use os_common::ApiErrorCode as Code;
        use UpdateError as E;
        let (code, msg) = match e {
            E::NoUpdates => (Code::NotFound, "无可用更新".into()),
            E::DownloadFailed(m) => (Code::UpstreamUnavailable, m),
            E::VerificationFailed(m) => (Code::InvalidInput, m),
            E::WriteFailed(m) => (Code::Internal, m),
            E::SlotConflict(m) => (Code::Conflict, m),
            E::RollbackFailed(m) => (Code::FailoverFailed, m),
            E::CveCheckFailed(m) => (Code::UpstreamUnavailable, m),
            E::HealthCheckFailed(m) => (Code::UpstreamUnavailable, m),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::UpdateError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::NoUpdates).contains("无可用更新"));
        assert!(format!("{}", E::DownloadFailed("d".into())).contains("下载失败"));
        assert!(format!("{}", E::VerificationFailed("v".into())).contains("校验失败"));
        assert!(format!("{}", E::WriteFailed("w".into())).contains("写入槽位失败"));
        assert!(format!("{}", E::SlotConflict("s".into())).contains("槽位冲突"));
        assert!(format!("{}", E::RollbackFailed("r".into())).contains("回滚失败"));
        assert!(format!("{}", E::CveCheckFailed("c".into())).contains("CVE 检查失败"));
        assert!(format!("{}", E::HealthCheckFailed("h".into())).contains("健康检查失败"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
