//! osd 错误类型

use crate::ComponentId;
use thiserror::Error;

/// 编排器错误
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// 组件不存在（未在注册表中找到该 ID）
    #[error("组件不存在: {0}")]
    ComponentNotFound(ComponentId),

    /// 组件启动失败（依赖未就绪 / 启动超时 / 二进制缺失）
    #[error("组件 {component} 启动失败: {reason}")]
    StartFailed {
        component: ComponentId,
        reason: String,
    },

    /// 组件停止失败（进程不响应 SIGTERM / SIGKILL 失败）
    #[error("组件 {component} 停止失败: {reason}")]
    StopFailed {
        component: ComponentId,
        reason: String,
    },

    /// 依赖图中存在循环（拓扑排序无法完成）
    #[error("组件依赖存在循环: {cycle}")]
    DependencyCycle { cycle: String },

    /// NTP 同步失败（上游不可达 / 偏移过大）
    #[error("NTP 同步失败: {0}")]
    NtpSyncFailed(String),

    /// 配额设置失败（cgroup v2 写入失败 / 配额非法）
    #[error("配额设置失败: {0}")]
    QuotaFailed(String),

    /// 底层 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// osd Result 别名
pub type OrchestratorResult<T> = Result<T, OrchestratorError>;

// —— From 转换：OrchestratorError → os-common::ApiError ——
//
// os-api 网关把任意 crate Error 统一转 ApiError 序列化返回前端（呼应 §12.3）。
// 错误码归类：
// - 组件不存在 → NotFound
// - 依赖循环 / 配额非法 → InvalidInput（配置/参数类）
// - 启动/停止失败 / NTP 失败 → UpstreamUnavailable（运行期依赖问题）
// - IO → Internal
impl From<OrchestratorError> for os_common::ApiError {
    fn from(e: OrchestratorError) -> Self {
        use os_common::ApiErrorCode as Code;
        use OrchestratorError as E;
        let (code, msg) = match e {
            E::ComponentNotFound(c) => (Code::NotFound, c.to_string()),
            E::DependencyCycle { cycle } => (Code::InvalidInput, cycle),
            E::QuotaFailed(m) => (Code::InvalidInput, m),
            E::StartFailed { component, reason } => (
                Code::UpstreamUnavailable,
                format!("组件 {component} 启动失败: {reason}"),
            ),
            E::StopFailed { component, reason } => (
                Code::UpstreamUnavailable,
                format!("组件 {component} 停止失败: {reason}"),
            ),
            E::NtpSyncFailed(m) => (Code::UpstreamUnavailable, m),
            E::Io(io) => (Code::Internal, io.to_string()),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::OrchestratorError as E;
    use crate::ComponentId;
    use os_common::{ApiError, ApiErrorCode};

    #[test]
    fn error_display_covers_all_variants() {
        let cid = ComponentId("osd-storage".into());
        assert!(format!("{}", E::ComponentNotFound(cid.clone())).contains("组件不存在"));
        assert!(format!(
            "{}",
            E::StartFailed {
                component: cid.clone(),
                reason: "dep".into()
            }
        )
        .contains("启动失败"));
        assert!(format!(
            "{}",
            E::StopFailed {
                component: cid.clone(),
                reason: "sigkill".into()
            }
        )
        .contains("停止失败"));
        assert!(format!(
            "{}",
            E::DependencyCycle {
                cycle: "a->b->a".into()
            }
        )
        .contains("循环"));
        assert!(format!("{}", E::NtpSyncFailed("n".into())).contains("NTP 同步失败"));
        assert!(format!("{}", E::QuotaFailed("q".into())).contains("配额设置失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
    }

    // ---- From<OrchestratorError> for ApiError 覆盖所有变体的错误码映射 ----

    fn cid() -> ComponentId {
        ComponentId("osd-x".into())
    }

    #[test]
    fn component_not_found_maps_to_not_found() {
        let api: ApiError = E::ComponentNotFound(cid()).into();
        assert_eq!(api.code, ApiErrorCode::NotFound);
        assert!(api.message.contains("osd-x"));
    }

    #[test]
    fn dependency_cycle_maps_to_invalid_input() {
        let api: ApiError = E::DependencyCycle {
            cycle: "a -> b -> a".into(),
        }
        .into();
        assert_eq!(api.code, ApiErrorCode::InvalidInput);
        assert!(api.message.contains("a -> b -> a"));
    }

    #[test]
    fn quota_failed_maps_to_invalid_input() {
        let api: ApiError = E::QuotaFailed("bad quota".into()).into();
        assert_eq!(api.code, ApiErrorCode::InvalidInput);
        assert!(api.message.contains("bad quota"));
    }

    #[test]
    fn start_failed_maps_to_upstream_unavailable() {
        let api: ApiError = E::StartFailed {
            component: cid(),
            reason: "timeout".into(),
        }
        .into();
        assert_eq!(api.code, ApiErrorCode::UpstreamUnavailable);
        assert!(api.message.contains("osd-x"));
        assert!(api.message.contains("timeout"));
    }

    #[test]
    fn stop_failed_maps_to_upstream_unavailable() {
        let api: ApiError = E::StopFailed {
            component: cid(),
            reason: "sigterm ignored".into(),
        }
        .into();
        assert_eq!(api.code, ApiErrorCode::UpstreamUnavailable);
        assert!(api.message.contains("停止失败"));
    }

    #[test]
    fn ntp_sync_failed_maps_to_upstream_unavailable() {
        let api: ApiError = E::NtpSyncFailed("upstream unreachable".into()).into();
        assert_eq!(api.code, ApiErrorCode::UpstreamUnavailable);
        assert!(api.message.contains("upstream unreachable"));
    }

    #[test]
    fn io_error_maps_to_internal() {
        let api: ApiError = E::Io(std::io::Error::other("disk full")).into();
        assert_eq!(api.code, ApiErrorCode::Internal);
        assert!(api.message.contains("disk full"));
    }

    #[test]
    fn all_variants_have_distinct_meaningful_codes() {
        // 确保错误码归类完整：NotFound / InvalidInput / UpstreamUnavailable / Internal 均被覆盖
        let codes = [
            <E as Into<ApiError>>::into(E::ComponentNotFound(cid())).code,
            <E as Into<ApiError>>::into(E::DependencyCycle { cycle: "x".into() }).code,
            <E as Into<ApiError>>::into(E::StartFailed {
                component: cid(),
                reason: "x".into(),
            })
            .code,
            <E as Into<ApiError>>::into(E::Io(std::io::Error::other("x"))).code,
        ];
        assert!(codes.contains(&ApiErrorCode::NotFound));
        assert!(codes.contains(&ApiErrorCode::InvalidInput));
        assert!(codes.contains(&ApiErrorCode::UpstreamUnavailable));
        assert!(codes.contains(&ApiErrorCode::Internal));
    }
}
