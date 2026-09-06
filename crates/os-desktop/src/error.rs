//! os-desktop 错误类型
//!
//! 设计：每 crate 自定义 `DesktopError`（thiserror），并实现
//! `From<DesktopError> for os_common::ApiError`，由 Tauri 桥统一对外。

use thiserror::Error;

/// os-desktop 错误（聚焦桌面特有的挂载相关错误；客户端通用错误由 os-mobile 承载）
#[derive(Debug, Error)]
pub enum DesktopError {
    /// 挂载失败（Windows `net use` 失败 / davfs2 挂载失败 / 权限不足）
    #[error("挂载失败: {0}")]
    MountFailed(String),

    /// 卸载失败（设备忙 / `net use /delete` 失败）
    #[error("卸载失败: {0}")]
    UnmountFailed(String),

    /// 共享不存在（远端 OS 上没有指定的 share）
    #[error("共享不存在: {0}")]
    ShareNotFound(String),

    /// 不支持的协议（如远端只提供 FTP，但本地只支持 SMB/WebDAV）
    #[error("不支持的协议: {0}")]
    UnsupportedProtocol(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-desktop Result 别名
pub type DesktopResult<T> = Result<T, DesktopError>;

// —— From 转换：DesktopError → ApiError（统一对外错误码）——
impl From<DesktopError> for os_common::ApiError {
    fn from(e: DesktopError) -> Self {
        use os_common::ApiErrorCode as Code;
        use DesktopError as E;
        let (code, msg) = match e {
            // ERROR_GUIDE §3.3 P3 保留：挂载/卸载的远端 OS 视为"上游"，UpstreamUnavailable 可接受。
            E::MountFailed(m) => (Code::UpstreamUnavailable, m),
            E::UnmountFailed(m) => (Code::UpstreamUnavailable, m),
            E::ShareNotFound(m) => (Code::NotFound, m),
            E::UnsupportedProtocol(m) => (Code::InvalidInput, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::DesktopError as E;
    use os_common::{ApiError, ApiErrorCode as Code};

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::MountFailed("m".into())).contains("挂载失败"));
        assert!(format!("{}", E::UnmountFailed("u".into())).contains("卸载失败"));
        assert!(format!("{}", E::ShareNotFound("s".into())).contains("共享不存在"));
        assert!(format!("{}", E::UnsupportedProtocol("u".into())).contains("不支持的协议"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }

    // —— 扩展边界（覆盖率补测：From<DesktopError> for ApiError 全变体映射）——

    #[test]
    fn mount_failed_maps_to_upstream_unavailable() {
        let api: ApiError = E::MountFailed("net use fail".into()).into();
        assert_eq!(api.code, Code::UpstreamUnavailable);
        assert_eq!(api.message, "net use fail");
    }

    #[test]
    fn unmount_failed_maps_to_upstream_unavailable() {
        let api: ApiError = E::UnmountFailed("device busy".into()).into();
        assert_eq!(api.code, Code::UpstreamUnavailable);
        assert_eq!(api.message, "device busy");
    }

    #[test]
    fn share_not_found_maps_to_not_found() {
        let api: ApiError = E::ShareNotFound("photos".into()).into();
        assert_eq!(api.code, Code::NotFound);
        assert_eq!(api.message, "photos");
    }

    #[test]
    fn unsupported_protocol_maps_to_invalid_input() {
        let api: ApiError = E::UnsupportedProtocol("ftp".into()).into();
        assert_eq!(api.code, Code::InvalidInput);
        assert_eq!(api.message, "ftp");
    }

    #[test]
    fn io_error_maps_to_internal() {
        let api: ApiError = E::Io(std::io::Error::other("perm")).into();
        assert_eq!(api.code, Code::Internal);
        assert!(api.message.contains("perm"));
    }

    #[test]
    fn internal_maps_to_internal() {
        let api: ApiError = E::Internal("boom".into()).into();
        assert_eq!(api.code, Code::Internal);
        assert_eq!(api.message, "boom");
    }

    #[test]
    fn error_from_io_various_kinds() {
        // From<std::io::Error> 路径覆盖多种 ErrorKind
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::AlreadyExists,
        ] {
            let e = E::from(std::io::Error::new(kind, "x"));
            assert!(format!("{}", e).contains("IO 错误"));
        }
    }

    #[test]
    fn error_source_chain_for_io() {
        // Io variant 的 source 应是内部 io::Error（thiserror #[from]）
        use std::error::Error as _;
        let e = E::Io(std::io::Error::other("inner"));
        assert!(e.source().is_some());
    }

    #[test]
    fn error_debug_format_all_variants() {
        // Debug 派生
        let _d1 = format!("{:?}", E::MountFailed("m".into()));
        let _d2 = format!("{:?}", E::Internal("i".into()));
        let _d3 = format!("{:?}", E::ShareNotFound("s".into()));
    }
}
