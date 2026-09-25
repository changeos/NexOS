//! 命令模型——树形命令与执行上下文（规划文档 §3.0/#19）
//!
//! 设计：
//! - `Command` trait 暴露 `subcommands`，构成命令树（如 `os storage dataset list`）
//! - `execute` 在 `CommandContext`（含远端端点/token/输出格式）下执行
//! - 返回 `CommandOutput`，由调用方交给 `OutputFormatter` 渲染

use os_core::{Deserialize, Serialize};

use crate::CliError;

// ----------------------------------------------------------------------------
// 输出格式
// ----------------------------------------------------------------------------

/// 输出格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// 纯文本（表格/对齐，便于人读）
    Text,
    /// JSON
    Json,
    /// YAML
    Yaml,
}

// ----------------------------------------------------------------------------
// 命令规格 / 参数规格
// ----------------------------------------------------------------------------

/// 子命令规格（用于声明命令树）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    /// 子命令名（如 `"list"` / `"create"`）
    pub name: String,
    /// 简短描述
    pub description: String,
    /// 参数规格
    pub args: Vec<ArgSpec>,
}

/// 参数规格
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgSpec {
    /// 参数名（如 `"--path"`）
    pub name: String,
    /// 是否必填
    pub required: bool,
    /// 默认值（None = 无默认）
    pub default: Option<String>,
}

// ----------------------------------------------------------------------------
// 执行上下文 / 输出
// ----------------------------------------------------------------------------

/// 命令执行上下文（贯穿整条命令的生命周期）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandContext {
    /// os-api 远端端点（None = 本地直调模式，与 osd 同进程）
    pub api_endpoint: Option<String>,
    /// 认证 token（None = 未认证/匿名）
    pub token: Option<String>,
    /// 期望的输出格式
    pub format: OutputFormat,
}

/// 命令执行输出
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    /// 是否成功
    pub success: bool,
    /// 结构化数据（供格式化器渲染）
    pub data: serde_json::Value,
    /// 人类可读附加消息（None = 无）
    pub message: Option<String>,
}

// ----------------------------------------------------------------------------
// Command trait（同步）
// ----------------------------------------------------------------------------

/// 命令——构成树形 CLI。
///
/// 实现者：各业务命令（如 `StorageCommand` / `NetworkCommand` / `VmCommand`）。
pub trait Command: Send + Sync {
    /// 命令名（如 `"storage"`）。
    fn name(&self) -> &str;

    /// 简短描述。
    fn description(&self) -> &str;

    /// 子命令规格列表（叶子命令返回空 Vec）。
    fn subcommands(&self) -> Vec<CommandSpec>;

    /// 执行命令；`args` 为去掉本命令名后的剩余参数。
    fn execute(&self, args: &[String], ctx: &mut CommandContext)
        -> Result<CommandOutput, CliError>;
}
