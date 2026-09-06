//! os-provision 错误类型
//!
//! 设计：每 crate 自定义 `ProvisionError`（thiserror），并实现
//! `From<ProvisionError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-provision 错误
#[derive(Debug, Error)]
pub enum ProvisionError {
    /// PXE 启动失败（目标节点未响应/网络不可达/DHCP 冲突）
    #[error("PXE 启动失败: {0}")]
    PxeBootFailed(String),

    /// 系统初始化失败（分区/装基础系统/建池/拉起 osd 空壳 阶段出错）
    #[error("系统初始化失败: {0}")]
    InitFailed(String),

    /// 迁移失败（ZFS send/recv 出错/配置同步失败/排除清单处理失败）
    #[error("迁移失败: {0}")]
    MigrationFailed(String),

    /// 目标节点不可达（网络层探测失败/节点未上线）
    #[error("目标节点不可达: {0}")]
    TargetUnreachable(String),

    /// 配置非法（base_image 路径错/disk 列表空/arch 不支持等）
    #[error("配置非法: {0}")]
    InvalidConfig(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-provision Result 别名
pub type ProvisionResult<T> = Result<T, ProvisionError>;

// —— From 转换：ProvisionError → ApiError（统一对外错误码）——
impl From<ProvisionError> for os_common::ApiError {
    fn from(e: ProvisionError) -> Self {
        use os_common::ApiErrorCode as Code;
        use ProvisionError as E;
        let (code, msg) = match e {
            E::PxeBootFailed(m) => (Code::UpstreamUnavailable, m),
            E::InitFailed(m) => (Code::Internal, m),
            E::MigrationFailed(m) => (Code::Internal, m),
            E::TargetUnreachable(m) => (Code::UpstreamUnavailable, m),
            E::InvalidConfig(m) => (Code::InvalidInput, m),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::ProvisionError as E;
    use os_common::{ApiError, ApiErrorCode};

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::PxeBootFailed("p".into())).contains("PXE 启动失败"));
        assert!(format!("{}", E::InitFailed("i".into())).contains("系统初始化失败"));
        assert!(format!("{}", E::MigrationFailed("m".into())).contains("迁移失败"));
        assert!(format!("{}", E::TargetUnreachable("t".into())).contains("目标节点不可达"));
        assert!(format!("{}", E::InvalidConfig("c".into())).contains("配置非法"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }

    /// 覆盖 `From<ProvisionError> for ApiError` 的全部变体 → 错误码身份映射。
    #[test]
    fn error_to_api_error_maps_all_variants() {
        let cases: [(E, ApiErrorCode); 6] = [
            (
                E::PxeBootFailed("p".into()),
                ApiErrorCode::UpstreamUnavailable,
            ),
            (E::InitFailed("i".into()), ApiErrorCode::Internal),
            (E::MigrationFailed("m".into()), ApiErrorCode::Internal),
            (
                E::TargetUnreachable("t".into()),
                ApiErrorCode::UpstreamUnavailable,
            ),
            (E::InvalidConfig("c".into()), ApiErrorCode::InvalidInput),
            (E::Internal("x".into()), ApiErrorCode::Internal),
        ];
        for (err, expected_code) in cases {
            let api: ApiError = err.into();
            assert_eq!(api.code, expected_code);
        }
    }

    /// 错误消息透传不丢失。
    #[test]
    fn error_to_api_error_preserves_message() {
        let api: ApiError = E::MigrationFailed("数据集校验失败-细节".into()).into();
        assert_eq!(api.message, "数据集校验失败-细节");
    }
}
