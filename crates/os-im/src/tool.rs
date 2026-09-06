//! Tool 契约——Function Calling（规划文档 §3.7.1 SDK 核心）
//!
//! 每个 Tool 是一项可被 LLM 调用的能力（如 "创建快照"/"启动 VM"/"发起转账"）。
//! 高危工具（requires_confirmation）执行前需经 IM 内确认（见 confirmation.rs）。
//! 条件激活工具（conditionally_activated）仅在运行期条件满足时注册。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 工具分类与描述
// ----------------------------------------------------------------------------

/// 工具分类（对应各执行域）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// 存储域（池/数据集/快照/复制）
    Storage,
    /// 计算域（VM/容器）
    Compute,
    /// 网络域（接口/防火墙/VLAN）
    Network,
    /// 访客域（认证/会话）
    Guest,
    /// 钱包域（链上签名/转账）
    Wallet,
    /// 安全域（用户/证书/2FA）
    Security,
    /// 集群元数据域（共识/KV/故障转移）
    Meta,
    /// 服务域（SMB/NFS/WebDAV/iSCSI）
    Service,
    /// 系统分发/迁移/ISO/更新
    Provision,
    /// 只读查询（聚合状态/报表）
    Query,
}

/// 工具描述符（注册到 LLM function calling 的 schema）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// 工具名（唯一，如 `storage.snapshot.create`）
    pub name: String,
    /// 描述（喂给 LLM，说明用途/约束）
    pub description: String,
    /// 参数 JSON Schema（喂给 LLM function calling）
    pub parameters_schema: serde_json::Value,
    /// 分类
    pub category: ToolCategory,
    /// 是否高危——true 则执行前需经确认门（见 confirmation.rs）
    pub requires_confirmation: bool,
    /// 条件激活标识（None = 常驻；Some("rpc_available:bitcoin") = 仅当条件满足时注册）
    pub conditionally_activated: Option<String>,
}

// ----------------------------------------------------------------------------
// 工具调用与结果
// ----------------------------------------------------------------------------

/// 一次工具调用（LLM 发起）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// 目标工具名
    pub tool_name: String,
    /// 调用参数（JSON，符合该工具 schema）
    pub arguments: serde_json::Value,
    /// 调用 ID（用于关联结果）
    pub call_id: String,
}

/// 工具调用结果（回填给 LLM / 用户）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// 关联的调用 ID
    pub call_id: String,
    /// 是否成功
    pub success: bool,
    /// 输出（JSON，开放结构）
    pub output: serde_json::Value,
    /// 失败原因（success = false 时填）
    pub error: Option<String>,
}

// ----------------------------------------------------------------------------
// Tool trait（async，Function Calling 契约）
// ----------------------------------------------------------------------------

/// 工具——可被 LLM 调用的能力单元。
///
/// 实现者：各执行组件通过 `Box<dyn Tool>` 注入到 agent / orchestrator。
/// trait 层不硬依赖具体 crate，保持开放。
///
/// 经 `Box<dyn Tool>` 注入，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具描述符（注册给 LLM）。
    async fn descriptor(&self) -> ToolDescriptor;

    /// 执行调用。
    async fn invoke(&self, call: &ToolCall) -> Result<ToolResult, crate::ImError>;

    /// 是否当前可用（条件激活型：返回 false 则不注册到 LLM）。
    async fn is_available(&self) -> bool;
}
