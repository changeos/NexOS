//! Mock 实现（feature gate `mock`）——供下游 agent（client）测试用。
//!
//! 提供：`MockCommand` / `MockOutputFormatter`。

#![cfg(feature = "mock")]

use crate::command::{Command, CommandContext, CommandOutput, CommandSpec};
use crate::format::OutputFormatter;
use crate::CliError;
use std::sync::Mutex;

// ============================================================
// MockCommand
// ============================================================

/// Mock `Command`——可配置声明的子命令与固定输出。
pub struct MockCommand {
    name: String,
    description: String,
    subs: Vec<CommandSpec>,
    output: Mutex<CommandOutput>,
    execute_count: Mutex<u32>,
}

impl MockCommand {
    /// 构造：名字 + 固定成功输出。
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: "mock command".to_string(),
            subs: vec![],
            output: Mutex::new(CommandOutput {
                success: true,
                data: serde_json::json!({"mock": true}),
                message: Some("ok".to_string()),
            }),
            execute_count: Mutex::new(0),
        }
    }

    /// 设置描述。
    pub fn with_description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    /// 设置子命令规格。
    pub fn with_subcommands(mut self, subs: Vec<CommandSpec>) -> Self {
        self.subs = subs;
        self
    }

    /// 设置 execute 返回的输出。
    pub fn with_output(self, output: CommandOutput) -> Self {
        *self.output.lock().unwrap() = output;
        self
    }

    /// execute 被调用次数。
    pub fn execute_count(&self) -> u32 {
        *self.execute_count.lock().unwrap()
    }
}

impl Command for MockCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn subcommands(&self) -> Vec<CommandSpec> {
        self.subs.clone()
    }
    fn execute(
        &self,
        _args: &[String],
        _ctx: &mut CommandContext,
    ) -> Result<CommandOutput, CliError> {
        *self.execute_count.lock().unwrap() += 1;
        Ok(self.output.lock().unwrap().clone())
    }
}

// ============================================================
// MockOutputFormatter
// ============================================================

/// Mock `OutputFormatter`——返回固定字符串，便于断言注入。
pub struct MockOutputFormatter {
    fixed: String,
}

impl MockOutputFormatter {
    /// 构造：固定返回内容。
    pub fn new(fixed: impl Into<String>) -> Self {
        Self {
            fixed: fixed.into(),
        }
    }
}

impl OutputFormatter for MockOutputFormatter {
    fn format(&self, _output: &CommandOutput) -> Result<String, CliError> {
        Ok(self.fixed.clone())
    }
}

// ============================================================
// 单元测
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::OutputFormat;

    #[test]
    fn mock_command_returns_configured_output() {
        let cmd = MockCommand::new("storage")
            .with_description("storage ops")
            .with_output(CommandOutput {
                success: true,
                data: serde_json::json!({"pools": 3}),
                message: None,
            });
        assert_eq!(cmd.name(), "storage");
        assert_eq!(cmd.description(), "storage ops");
        let mut ctx = CommandContext {
            api_endpoint: None,
            token: None,
            format: OutputFormat::Text,
        };
        let out = cmd.execute(&[], &mut ctx).unwrap();
        assert_eq!(out.data["pools"], 3);
        assert_eq!(cmd.execute_count(), 1);
    }

    #[test]
    fn mock_command_subcommands_exposed() {
        let cmd = MockCommand::new("vm").with_subcommands(vec![CommandSpec {
            name: "list".to_string(),
            description: "list vms".to_string(),
            args: vec![],
        }]);
        assert_eq!(cmd.subcommands().len(), 1);
        assert_eq!(cmd.subcommands()[0].name, "list");
    }

    #[test]
    fn mock_output_formatter_returns_fixed() {
        let f = MockOutputFormatter::new("[mock]");
        let out = CommandOutput {
            success: true,
            data: serde_json::Value::Null,
            message: None,
        };
        assert_eq!(f.format(&out).unwrap(), "[mock]");
    }

    #[test]
    fn mock_command_in_tree() {
        let mut tree = crate::command_tree::CommandTree::new();
        tree.register(Box::new(MockCommand::new("net")));
        assert_eq!(tree.len(), 1);
        let mut ctx = CommandContext {
            api_endpoint: None,
            token: None,
            format: OutputFormat::Text,
        };
        let out = tree.execute(&["net".to_string()], &mut ctx).unwrap();
        assert!(out.success);
    }
}
