//! 输出格式化器（规划文档 §3.0/#19）
//!
//! 三种内置格式：Text（人读）、Json（脚本友好）、Yaml（配置友好）。
//! 此处仅声明 struct，不实现渲染逻辑。

use crate::command::CommandOutput;
use crate::CliError;

/// 输出格式化器——把 `CommandOutput` 渲染为字符串。
pub trait OutputFormatter: Send + Sync {
    /// 格式化输出。
    fn format(&self, output: &CommandOutput) -> Result<String, CliError>;
}

/// 纯文本格式化器（表格/对齐，便于人读）
pub struct TextFormatter;

/// JSON 格式化器
pub struct JsonFormatter;

/// YAML 格式化器
pub struct YamlFormatter;
