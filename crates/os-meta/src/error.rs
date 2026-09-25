//! os-meta 错误类型
//!
//! 设计：每 crate 自定义 `MetaError`（thiserror），并实现
//! `From<MetaError> for os_common::ApiError`，由 os-api 网关统一序列化返回。
//! 故障转移相关错误映射到 `FailoverFailed` 错误码。

use thiserror::Error;

/// os-meta 错误
#[derive(Debug, Error)]
pub enum MetaError {
    /// 当前节点非 leader（写操作需转发到 leader）
    #[error("当前节点非 leader: {0}")]
    NotLeader(String),

    /// 当前节点非集群成员（未加入任何 HA 集群）
    #[error("当前节点非集群成员: {0}")]
    NotMember(String),

    /// 法定人数丢失（可用成员不足，集群无法提交日志）
    #[error("法定人数丢失: {0}")]
    QuorumLost(String),

    /// 故障转移失败（迁移 VM / 切 VIP / 提升副本任一步骤失败）
    #[error("故障转移失败: {0}")]
    FailoverFailed(String),

    /// VIP 冲突（目标 VIP 已被其他节点占用，或接口绑定失败）
    #[error("VIP 冲突: {0}")]
    VipConflict(String),

    /// 快照创建/恢复失败
    #[error("快照失败: {0}")]
    SnapshotFailed(String),

    /// openraft log 应用失败（写本地 SQLite 状态机失败）
    #[error("log 应用失败: {0}")]
    ApplyFailed(String),

    /// CAS 乐观锁冲突（expected_version 与当前版本不符）
    #[error("CAS 版本冲突: 期望 {expected}, 实际 {actual}")]
    CasConflict {
        /// 期望版本号
        expected: u64,
        /// 实际版本号
        actual: u64,
    },

    /// 序列化/反序列化错误
    #[error("序列化错误: {0}")]
    Serde(#[from] serde_json::Error),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-meta Result 别名
pub type MetaResult<T> = Result<T, MetaError>;

// —— From 转换：MetaError → ApiError（统一对外错误码）——
impl From<MetaError> for os_common::ApiError {
    fn from(e: MetaError) -> Self {
        use os_common::ApiErrorCode as Code;
        use MetaError as E;
        let (code, msg) = match e {
            // ERROR_GUIDE §3.3 P3 保留：非 leader/非成员即"当前节点无权处理写"，
            // PermissionDenied 合理（语义接近 RBAC 拒绝）。
            E::NotLeader(m) => (Code::PermissionDenied, m),
            E::NotMember(m) => (Code::PermissionDenied, m),
            E::QuorumLost(m) => (Code::UpstreamUnavailable, m),
            E::FailoverFailed(m) => (Code::FailoverFailed, m),
            E::VipConflict(m) => (Code::Conflict, m),
            E::SnapshotFailed(m) => (Code::Internal, m),
            E::ApplyFailed(m) => (Code::Internal, m),
            E::CasConflict { expected, actual } => (
                Code::Conflict,
                format!("CAS 版本冲突: 期望 {expected}, 实际 {actual}"),
            ),
            E::Serde(m) => (Code::Internal, m.to_string()),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::MetaError as E;
    use os_common::{ApiError, ApiErrorCode};

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::NotLeader("n".into())).contains("当前节点非 leader"));
        assert!(format!("{}", E::NotMember("n".into())).contains("当前节点非集群成员"));
        assert!(format!("{}", E::QuorumLost("q".into())).contains("法定人数丢失"));
        assert!(format!("{}", E::FailoverFailed("f".into())).contains("故障转移失败"));
        assert!(format!("{}", E::VipConflict("v".into())).contains("VIP 冲突"));
        assert!(format!("{}", E::SnapshotFailed("s".into())).contains("快照失败"));
        assert!(format!("{}", E::ApplyFailed("a".into())).contains("log 应用失败"));
        let cas = format!(
            "{}",
            E::CasConflict {
                expected: 3,
                actual: 7
            }
        );
        assert!(
            cas.contains("CAS 版本冲突") && cas.contains("期望 3") && cas.contains("实际 7"),
            "got: {cas}"
        );
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(format!("{}", E::Serde(serde_err)).contains("序列化错误"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }

    // ---- From<MetaError> for ApiError 覆盖所有变体的错误码映射 ----

    #[test]
    fn not_leader_maps_to_permission_denied() {
        let api: ApiError = E::NotLeader("n1".into()).into();
        assert_eq!(api.code, ApiErrorCode::PermissionDenied);
    }

    #[test]
    fn not_member_maps_to_permission_denied() {
        let api: ApiError = E::NotMember("n2".into()).into();
        assert_eq!(api.code, ApiErrorCode::PermissionDenied);
    }

    #[test]
    fn quorum_lost_maps_to_upstream_unavailable() {
        let api: ApiError = E::QuorumLost("minority".into()).into();
        assert_eq!(api.code, ApiErrorCode::UpstreamUnavailable);
    }

    #[test]
    fn failover_failed_maps_to_failover_failed_code() {
        let api: ApiError = E::FailoverFailed("vm migrate".into()).into();
        assert_eq!(api.code, ApiErrorCode::FailoverFailed);
    }

    #[test]
    fn vip_conflict_maps_to_conflict() {
        let api: ApiError = E::VipConflict("10.0.0.5 taken".into()).into();
        assert_eq!(api.code, ApiErrorCode::Conflict);
    }

    #[test]
    fn snapshot_failed_maps_to_internal() {
        let api: ApiError = E::SnapshotFailed("restore".into()).into();
        assert_eq!(api.code, ApiErrorCode::Internal);
    }

    #[test]
    fn apply_failed_maps_to_internal() {
        let api: ApiError = E::ApplyFailed("bad cmd".into()).into();
        assert_eq!(api.code, ApiErrorCode::Internal);
    }

    #[test]
    fn cas_conflict_maps_to_conflict() {
        let api: ApiError = E::CasConflict {
            expected: 2,
            actual: 5,
        }
        .into();
        assert_eq!(api.code, ApiErrorCode::Conflict);
        assert!(api.message.contains("期望 2"));
        assert!(api.message.contains("实际 5"));
    }

    #[test]
    fn serde_error_maps_to_internal() {
        let serde_err = serde_json::from_str::<serde_json::Value>("bad").unwrap_err();
        let api: ApiError = E::Serde(serde_err).into();
        assert_eq!(api.code, ApiErrorCode::Internal);
    }

    #[test]
    fn io_error_maps_to_internal() {
        let api: ApiError = E::Io(std::io::Error::other("disk")).into();
        assert_eq!(api.code, ApiErrorCode::Internal);
    }

    #[test]
    fn internal_error_maps_to_internal() {
        let api: ApiError = E::Internal("boom".into()).into();
        assert_eq!(api.code, ApiErrorCode::Internal);
        assert!(api.message.contains("boom"));
    }

    #[test]
    fn all_codes_covered() {
        // 确保所有错误码归类被覆盖
        let codes = [
            <E as Into<ApiError>>::into(E::NotLeader("x".into())).code,
            <E as Into<ApiError>>::into(E::QuorumLost("x".into())).code,
            <E as Into<ApiError>>::into(E::FailoverFailed("x".into())).code,
            <E as Into<ApiError>>::into(E::VipConflict("x".into())).code,
            <E as Into<ApiError>>::into(E::SnapshotFailed("x".into())).code,
            <E as Into<ApiError>>::into(E::Internal("x".into())).code,
            <E as Into<ApiError>>::into(E::CasConflict {
                expected: 1,
                actual: 2,
            })
            .code,
        ];
        assert!(codes.contains(&ApiErrorCode::PermissionDenied));
        assert!(codes.contains(&ApiErrorCode::UpstreamUnavailable));
        assert!(codes.contains(&ApiErrorCode::FailoverFailed));
        assert!(codes.contains(&ApiErrorCode::Conflict));
        assert!(codes.contains(&ApiErrorCode::Internal));
    }
}
