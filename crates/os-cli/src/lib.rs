//! os-cli —— 管理命令行工具（接口契约）
//!
//! 定位（规划文档 §3.0/#19）：面向运维/管理员的 CLI，可：
//! - 连接远端 `os-api`（通过 HTTP/WS 调用网关）
//! - 或本地直调（与 osd 同进程，零网络）
//!
//! 命令模型：树形（Command 暴露 subcommands），每条命令在 `CommandContext` 下 `execute`，
//! 产出 `CommandOutput`，再由 `OutputFormatter`（Text/Json/Yaml）格式化输出。
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//! 管理类 trait 以同步为主（命令执行通常瞬时完成；耗时操作应返回任务 ID 异步轮询）。

pub mod cli;
pub mod command;
pub mod command_tree;
pub mod error;
pub mod format;
pub mod format_impl;
pub mod parse;

/// Mock 实现（feature gate `mock`，供下游 client 测试）。
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockCommand, MockOutputFormatter};

pub use command::{ArgSpec, Command, CommandContext, CommandOutput, CommandSpec, OutputFormat};
pub use command_tree::{format_output, CommandTree, ResolveResult};
pub use error::{CliError, CliResult};
pub use format::{JsonFormatter, OutputFormatter, TextFormatter, YamlFormatter};
pub use parse::{parse_args, ParsedArgs};

// —— clap 接通：顶层命令 + 子命令执行器（规划文档 §3.0/#19）——
pub use cli::{
    Cli, CliCommand, CommandRunner, OutputFormatArg, PoolAction, ShareAction, UserAction, VmAction,
};
