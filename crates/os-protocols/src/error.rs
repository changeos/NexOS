//! os-protocols 错误类型
//!
//! 设计：每 crate 自定义 `ProtocolError`（thiserror），并实现
//! `From<ProtocolError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-protocols 错误
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// 共享不存在
    #[error("共享不存在: {0}")]
    ShareNotFound(String),

    /// 共享已存在（同名/同路径冲突）
    #[error("共享已存在: {0}")]
    ShareExists(String),

    /// 协议被禁用（如编译时未启用 SMB / 当前节点角色不支持）
    #[error("协议被禁用: {0}")]
    ProtocolDisabled(String),

    /// 会话不存在
    #[error("会话不存在: {0}")]
    SessionNotFound(String),

    /// 配置生成失败（写 smb.conf / ganesha.conf 失败等）
    #[error("配置生成失败: {0}")]
    ConfigFailed(String),

    /// 服务重载失败（reload smbd / nfs-ganesha 失败）
    #[error("服务重载失败: {0}")]
    ReloadFailed(String),

    /// 对象存储 bucket 不存在
    #[error("bucket 不存在: {0}")]
    BucketNotFound(String),

    /// 对象不存在
    #[error("对象不存在: {0}")]
    ObjectNotFound(String),

    /// 访问被拒绝（access key 无权限 / 用户不在 valid_users）
    #[error("访问被拒绝: {0}")]
    AccessDenied(String),

    /// 底层命令执行失败
    #[error("命令执行失败: {0}")]
    CommandFailed(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-protocols Result 别名
pub type ProtocolResult<T> = Result<T, ProtocolError>;

// —— From 转换：ProtocolError → ApiError（统一对外错误码）——
impl From<ProtocolError> for os_common::ApiError {
    fn from(e: ProtocolError) -> Self {
        use os_common::ApiErrorCode as Code;
        use ProtocolError as E;
        let (code, msg) = match e {
            E::ShareNotFound(m) => (Code::NotFound, m),
            E::BucketNotFound(m) => (Code::NotFound, m),
            E::ObjectNotFound(m) => (Code::NotFound, m),
            E::SessionNotFound(m) => (Code::NotFound, m),
            E::ShareExists(m) => (Code::Conflict, m),
            // 按 ERROR_GUIDE §3.3：协议被配置禁用是配置/能力问题（状态不允许），
            // 非"上游服务不可用"，故归 Conflict（原 UpstreamUnavailable 已修正）。
            E::ProtocolDisabled(m) => (Code::Conflict, m),
            E::ConfigFailed(m) => (Code::Internal, m),
            E::ReloadFailed(m) => (Code::Internal, m),
            E::AccessDenied(m) => (Code::PermissionDenied, m),
            E::CommandFailed(m) => (Code::Internal, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::ProtocolError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::ShareNotFound("s".into())).contains("共享不存在"));
        assert!(format!("{}", E::ShareExists("s".into())).contains("共享已存在"));
        assert!(format!("{}", E::ProtocolDisabled("p".into())).contains("协议被禁用"));
        assert!(format!("{}", E::SessionNotFound("s".into())).contains("会话不存在"));
        assert!(format!("{}", E::ConfigFailed("c".into())).contains("配置生成失败"));
        assert!(format!("{}", E::ReloadFailed("r".into())).contains("服务重载失败"));
        assert!(format!("{}", E::BucketNotFound("b".into())).contains("bucket 不存在"));
        assert!(format!("{}", E::ObjectNotFound("o".into())).contains("对象不存在"));
        assert!(format!("{}", E::AccessDenied("a".into())).contains("访问被拒绝"));
        assert!(format!("{}", E::CommandFailed("c".into())).contains("命令执行失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
