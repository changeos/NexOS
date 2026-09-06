//! LLM 后端（规划文档 §3.7）
//!
//! 抽象 LLM 调用，支持云（OpenAI 等）/本地（candle/Phi-3-mini）/自定义后端。
//! IM 把对话历史 + 可用 tools 一并送入，LLM 返回文本 + 工具调用。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::conversation::Message;
use crate::tool::{ToolCall, ToolDescriptor};

// ----------------------------------------------------------------------------
// 后端类型
// ----------------------------------------------------------------------------

/// LLM 后端类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmBackendType {
    /// 云端（OpenAI / Anthropic / Azure 等）
    Cloud,
    /// 本地（candle / Phi-3-mini / Ollama 等）
    Local,
    /// 自定义（自建推理服务）
    Custom,
}

// ----------------------------------------------------------------------------
// 请求与响应
// ----------------------------------------------------------------------------

/// token 用量
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenUsage {
    /// 提示 token 数
    pub prompt_tokens: u32,
    /// 生成 token 数
    pub completion_tokens: u32,
    /// 合计
    pub total_tokens: u32,
}

/// LLM 请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// 对话消息（含历史）
    pub messages: Vec<Message>,
    /// 指定模型（None = 用后端默认）
    pub model: Option<String>,
    /// 采样温度（None = 默认）
    pub temperature: Option<f32>,
    /// 最大生成 token（None = 默认）
    pub max_tokens: Option<u32>,
    /// 可用工具描述符（function calling 注册表）
    pub tools: Vec<ToolDescriptor>,
}

/// LLM 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// 文本内容
    pub content: String,
    /// LLM 发起的工具调用（可能多个）
    pub tool_calls: Vec<ToolCall>,
    /// token 用量
    pub usage: TokenUsage,
    /// 结束原因（如 `stop` / `tool_calls` / `length`）
    pub finish_reason: String,
}

// ----------------------------------------------------------------------------
// LlmBackend trait（async）
// ----------------------------------------------------------------------------

/// LLM 后端——对话补全 + 工具调用 + 模型列举。
///
/// 实现者：`OpenAiBackend` / `LocalCandleBackend` / `CustomBackend`。
/// 预期以 `Box<dyn LlmBackend>` 注入 IM，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// 对话补全（含 function calling）。
    async fn chat(&self, req: LlmRequest) -> Result<LlmResponse, crate::ImError>;

    /// 列出后端可用模型。
    async fn list_models(&self) -> Result<Vec<String>, crate::ImError>;

    /// 后端类型。
    async fn backend_type(&self) -> LlmBackendType;
}
