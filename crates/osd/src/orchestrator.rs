//! Orchestrator trait —— 组件生命周期编排契约（规划文档 §3.13）
//!
//! osd 的核心 trait：管理业务组件进程的启动/停止/重启/状态查询/配额调整。
//! 依赖排序由实现负责（拓扑排序；检测到循环抛 `OrchestratorError::DependencyCycle`）。

use crate::component::{ComponentDescriptor, ComponentId, ComponentStatus};
use os_core::ResourceQuota;

/// 编排器 trait（异步，数据路径）
///
/// 实现者：osd 主进程。并发性：多个生命周期操作可并发触发（如重启 A 同时停止 B），
/// 实现需保证对同一组件的操作串行化。
pub trait Orchestrator: Send + Sync {
    /// 启动指定组件
    ///
    /// 失败原因：组件不存在 / 依赖未就绪 / 启动超时 / 已在运行（见 [`crate::OrchestratorError`]）
    async fn start(&self, id: &ComponentId) -> crate::OrchestratorResult<()>;

    /// 停止指定组件（优雅停止：先发 SIGTERM，超时后 SIGKILL）
    async fn stop(&self, id: &ComponentId) -> crate::OrchestratorResult<()>;

    /// 重启组件（= stop + start；依赖不被影响）
    async fn restart(&self, id: &ComponentId) -> crate::OrchestratorResult<()>;

    /// 查询组件当前状态
    async fn status(&self, id: &ComponentId) -> crate::OrchestratorResult<ComponentStatus>;

    /// 列出所有已注册组件描述符
    async fn list_components(&self) -> crate::OrchestratorResult<Vec<ComponentDescriptor>>;

    /// 调整组件资源配额（cgroup v2 在线调整，无需重启）
    async fn set_quota(
        &self,
        id: &ComponentId,
        quota: ResourceQuota,
    ) -> crate::OrchestratorResult<()>;

    /// 取组件当前配额（默认实现可选；便于运维查询）
    async fn get_quota(&self, _id: &ComponentId) -> crate::OrchestratorResult<ResourceQuota> {
        Err(crate::OrchestratorError::ComponentNotFound(_id.clone()))
    }
}
