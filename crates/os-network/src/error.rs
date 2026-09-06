//! os-network 错误类型
//!
//! 设计：每 crate 自定义 `NetworkError`（thiserror），并实现
//! `From<NetworkError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-network 错误
#[derive(Debug, Error)]
pub enum NetworkError {
    /// 接口不存在
    #[error("接口不存在: {0}")]
    InterfaceNotFound(String),

    /// 防火墙/NAT 规则非法（dry-run 未通过）
    #[error("规则非法: {0}")]
    RuleInvalid(String),

    /// 权限不足（缺少 CAP_NET_ADMIN）
    #[error("权限不足，需要 CAP_NET_ADMIN")]
    Permission,

    /// RDMA 能力不可用（无 IB/RoCE 设备）
    #[error("RDMA 不可用: {0}")]
    RdmaUnavailable(String),

    /// DPU 操作失败（厂商后端报错）
    #[error("DPU 错误: {0}")]
    DpuError(String),

    /// 底层命令执行失败（ip / iptables / wg 等）
    #[error("命令执行失败: {0}")]
    CommandFailed(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-network Result 别名
pub type NetworkResult<T> = Result<T, NetworkError>;

// —— From 转换：NetworkError → ApiError（统一对外错误码）——
impl From<NetworkError> for os_common::ApiError {
    fn from(e: NetworkError) -> Self {
        use os_common::ApiErrorCode as Code;
        use NetworkError as E;
        let (code, msg) = match e {
            E::InterfaceNotFound(m) => (Code::NotFound, m),
            E::RuleInvalid(m) => (Code::InvalidInput, m),
            E::Permission => (
                Code::PermissionDenied,
                "权限不足，需要 CAP_NET_ADMIN".into(),
            ),
            E::RdmaUnavailable(m) => (Code::UpstreamUnavailable, m),
            E::DpuError(m) => (Code::UpstreamUnavailable, m),
            E::CommandFailed(m) => (Code::Internal, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::InterfaceNotFound("i".into())).contains("接口不存在"));
        assert!(format!("{}", E::RuleInvalid("r".into())).contains("规则非法"));
        assert!(format!("{}", E::Permission).contains("CAP_NET_ADMIN"));
        assert!(format!("{}", E::RdmaUnavailable("r".into())).contains("RDMA 不可用"));
        assert!(format!("{}", E::DpuError("d".into())).contains("DPU 错误"));
        assert!(format!("{}", E::CommandFailed("c".into())).contains("命令执行失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }

    // error.rs 覆盖率补测：From<NetworkError> for ApiError 全变体映射（原 0/12 → 满覆盖）。
    use super::*;
    use os_common::{ApiError, ApiErrorCode};

    /// 辅助：断言从 `NetworkError` 映射到 `ApiError` 的码符合预期。
    fn assert_code(e: NetworkError, expected: ApiErrorCode) {
        let api: ApiError = e.into();
        assert_eq!(api.code, expected, "错误码映射不符");
    }

    #[test]
    fn interface_not_found_maps_not_found() {
        assert_code(
            NetworkError::InterfaceNotFound("eth0".into()),
            ApiErrorCode::NotFound,
        );
    }

    #[test]
    fn rule_invalid_maps_invalid_input() {
        assert_code(
            NetworkError::RuleInvalid("bad port".into()),
            ApiErrorCode::InvalidInput,
        );
    }

    #[test]
    fn permission_maps_permission_denied() {
        assert_code(NetworkError::Permission, ApiErrorCode::PermissionDenied);
    }

    #[test]
    fn rdma_unavailable_maps_upstream_unavailable() {
        assert_code(
            NetworkError::RdmaUnavailable("无设备".into()),
            ApiErrorCode::UpstreamUnavailable,
        );
    }

    #[test]
    fn dpu_error_maps_upstream_unavailable() {
        assert_code(
            NetworkError::DpuError("redfish 失败".into()),
            ApiErrorCode::UpstreamUnavailable,
        );
    }

    #[test]
    fn command_failed_maps_internal() {
        assert_code(
            NetworkError::CommandFailed("ip link 失败".into()),
            ApiErrorCode::Internal,
        );
    }

    #[test]
    fn io_error_maps_internal() {
        let io = std::io::Error::other("boom");
        assert_code(NetworkError::Io(io), ApiErrorCode::Internal);
    }

    #[test]
    fn internal_maps_internal() {
        assert_code(
            NetworkError::Internal("占位".into()),
            ApiErrorCode::Internal,
        );
    }

    #[test]
    fn result_alias_smoke() {
        // 触达 NetworkResult 别名 + Debug + thiserror Display。
        // 用变量携带非字面量值，规避 clippy::unnecessary_literal_unwrap。
        let val: u32 = 1;
        let ok: NetworkResult<u32> = Ok(val);
        assert!(ok.is_ok());
        let err: NetworkResult<()> = Err(NetworkError::Permission);
        assert!(err.is_err());
        // error 显示格式（thiserror #[error]）触发 Display。
        let s = format!("{}", NetworkError::Permission);
        assert!(s.contains("权限不足"));
    }
}
