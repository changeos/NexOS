//! os-services 错误类型
//!
//! 设计：每 crate 自定义 `ServiceError`（thiserror），并实现
//! `From<ServiceError> for os_common::ApiError`，由 os-api 网关统一序列化返回。
//!
//! 七组件的错误统一收敛为 `ServiceError`（按组件分 variant），避免错误类型爆炸；
//! 错误消息保留足够诊断信息（含 stderr / 任务 ID 等）。

use thiserror::Error;

/// os-services 错误（覆盖 backup / monitor / media / files / devtools / power 七组件）
#[derive(Debug, Error)]
pub enum ServiceError {
    /// 备份任务不存在（unschedule / trigger_now / restore 指定的 job_id 无效）
    #[error("备份任务不存在: {0}")]
    JobNotFound(String),

    /// CI 流水线执行失败（步骤报错 / 超时 / 被取消）
    #[error("流水线执行失败: {0}")]
    PipelineFailed(String),

    /// 密钥不存在（get_secret / rotate_secret 指定的 key 未存储）
    #[error("密钥不存在: {0}")]
    SecretNotFound(String),

    /// 媒体资源不存在（transcode / stream 指定的 asset_id 无效）
    #[error("媒体资源不存在: {0}")]
    AssetNotFound(String),

    /// 分享链接不存在（revoke / 查询指定的链接 id 无效）
    #[error("分享链接不存在: {0}")]
    LinkNotFound(String),

    /// 分享链接已过期（expires_at 已过 / 密码错误次数耗尽）
    #[error("分享链接已过期: {0}")]
    ShareExpired(String),

    /// 硬件错误（SMART 失败 / UPS 通信失败 / 风扇停转 / 温度越限）
    #[error("硬件错误: {0}")]
    HardwareError(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-services Result 别名
pub type ServiceResult<T> = Result<T, ServiceError>;

// —— From 转换：ServiceError → ApiError（统一对外错误码）——
impl From<ServiceError> for os_common::ApiError {
    fn from(e: ServiceError) -> Self {
        use os_common::ApiErrorCode as Code;
        use ServiceError as E;
        let (code, msg) = match e {
            E::JobNotFound(m) => (Code::NotFound, m),
            E::PipelineFailed(m) => (Code::UpstreamUnavailable, m),
            E::SecretNotFound(m) => (Code::NotFound, m),
            E::AssetNotFound(m) => (Code::NotFound, m),
            E::LinkNotFound(m) => (Code::NotFound, m),
            // ERROR_GUIDE §3.3 P3 保留：分享过期=访问被拒绝，
            // 与 SessionExpired→PermissionDenied 一致，可接受。
            E::ShareExpired(m) => (Code::PermissionDenied, m),
            // 按 ERROR_GUIDE §3.3：本机硬件（SMART/UPS/fan）非外部上游，
            // 属内部故障，归 Internal（原 UpstreamUnavailable 已修正）。
            E::HardwareError(m) => (Code::Internal, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::JobNotFound("j".into())).contains("备份任务不存在"));
        assert!(format!("{}", E::PipelineFailed("p".into())).contains("流水线执行失败"));
        assert!(format!("{}", E::SecretNotFound("s".into())).contains("密钥不存在"));
        assert!(format!("{}", E::AssetNotFound("a".into())).contains("媒体资源不存在"));
        assert!(format!("{}", E::LinkNotFound("l".into())).contains("分享链接不存在"));
        assert!(format!("{}", E::ShareExpired("s".into())).contains("分享链接已过期"));
        assert!(format!("{}", E::HardwareError("h".into())).contains("硬件错误"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
