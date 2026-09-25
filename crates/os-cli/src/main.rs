//! `os` 管理命令行——可执行入口（规划文档 §3.0/#19）。
//!
//! 本 crate 原为纯库（24 crate 之一，无 binary 入口），加入此 `main.rs` 后
//! 支持 `cargo run -p os-cli -- <args>`，产物名为 `os`（见 Cargo.toml `[[bin]]`）。
//!
//! ## 入口职责
//!
//! 1. **命令解析**：复用 [`os_cli::cli::Cli`]（clap derive，`--server` / `--output`
//!    / `--token` 全局选项 + `status` / `pool` / `vm` / `share` / `user` / `discover`
//!    子命令树）。clap 自动处理 `--help`（命令树帮助）与 `--version`。
//!    - 注：clap 路径与既有自实现 [`os_cli::parse_args`] / [`os_cli::CommandTree`]
//!      并行不冲突（见 cli.rs 模块注释）；此处选用 clap 为主路径以获得原生
//!      `--help`/`--version`/错误退出码（exit 2）体验，CommandTree 仅用于
//!      `--help` 后的人读「命令树概览」展示。
//! 2. **命令执行**：经 [`os_cli::cli::CommandRunner`]（注入 `ReqwestTransport`）
//!    路由子命令到网关 REST 调用（status/discover 复用 OsClient；pool/vm/share/user
//!    走通用 transport.send）。
//! 3. **输出格式化**：`--output text|json|yaml`（默认 text），由 [`os_cli::format_output`]
//!    在 runner 内部选择 [`os_cli::OutputFormatter`] 渲染。
//! 4. **错误处理**：clap 解析错（含 `--help`/`--version` 触发的 DisplayHelp/DisplayVersion）
//!    走 clap 既有退出码（0 for help/version；2 for usage error）；运行期错误
//!    按类别映射退出码（参数/认证/连接/内部）。
//!
//! ## 退出码约定
//!
//! | 退出码 | 含义 |
//! |--------|------|
//! | 0      | 成功（含 `--help` / `--version`） |
//! | 1      | 运行期失败（远端不可达 / 内部错误 / 输出失败） |
//! | 2      | 参数非法 / 未知命令（clap 既有） |
//!
//! ## 异步
//!
//! 子命令执行为异步（`CommandRunner::run` 调 `OsClient` / `transport.send`），
//! 故入口用 tokio multi-thread runtime 包裹（命令执行通常瞬时完成，但 HTTP 调用
//! 必须异步）。本地纯解析路径（如 `--help`）不进 runtime。

use std::process::ExitCode;

use clap::Parser;
use os_cli::cli::{Cli, CommandRunner};
use os_cli::command_tree::CommandTree;
use os_cli::error::CliError;
use os_cli::{Command, CommandSpec};

fn main() -> ExitCode {
    // clap::parse 已内置 `--help`（exit 0）/ `--version`（exit 0）/ 用法错（exit 2）
    // 的退出处理；失败时 clap 自行打印诊断后 exit，不会走到此处之后。
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // e.kind() == DisplayHelp / DisplayVersion → exit_code() 返回 0；
            // 其余（参数错、未知子命令）返回 2。clap 已自行打印到 stdout/stderr。
            let code = e.exit_code();
            let _ = e.print();
            return ExitCode::from(code as u8);
        }
    };

    // 运行期执行：进入 tokio runtime（HTTP 调用为异步）。
    // rt-multi-thread feature 来自 workspace tokio（features=["full"]）。
    let runner = match CommandRunner::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: 初始化失败: {e}");
            return ExitCode::from(1);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: 启动 runtime 失败: {e}");
            return ExitCode::from(1);
        }
    };

    match rt.block_on(runner.run(&cli)) {
        Ok(rendered) => {
            // runner.run 已按 --output 渲染为最终字符串；打印到 stdout。
            println!("{rendered}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            // 按错误类别打印诊断 + 退出码
            report_error(&e);
            ExitCode::from(exit_code_for(&e))
        }
    }
}

// ----------------------------------------------------------------------------
// 错误报告 + 退出码映射
// ----------------------------------------------------------------------------

/// 把 [`CliError`] 打印为「error: <类别>: <消息>」到 stderr。
///
/// 分类前缀便于人读（运维一眼判断是参数错还是网关不可达）。
fn report_error(e: &CliError) {
    let (kind, msg) = describe(e);
    eprintln!("error: {kind}: {msg}");
}

/// 错误类别（人读名）+ 消息提取。
fn describe(e: &CliError) -> (&'static str, String) {
    match e {
        CliError::InvalidArgs(m) => ("非法参数", m.clone()),
        CliError::CommandNotFound(m) => ("命令不存在", m.clone()),
        CliError::ApiConnectionFailed(m) => ("API 连接失败", m.clone()),
        CliError::AuthFailed(m) => ("认证失败", m.clone()),
        CliError::OutputFailed(m) => ("输出失败", m.clone()),
        CliError::Io(m) => ("IO 错误", m.to_string()),
        CliError::Internal(m) => ("内部错误", m.clone()),
    }
}

/// 运行期错误 → 退出码。
///
/// 约定：
/// - 参数/命令 → 2（与 clap 用法错对齐）。
/// - 认证 → 3（与连接错区分，便于脚本判别是否需重登）。
/// - 连接 → 4（网关不可达，运维侧可重试）。
/// - 其余（输出/IO/内部）→ 1。
fn exit_code_for(e: &CliError) -> u8 {
    match e {
        CliError::InvalidArgs(_) | CliError::CommandNotFound(_) => 2,
        CliError::AuthFailed(_) => 3,
        CliError::ApiConnectionFailed(_) => 4,
        CliError::OutputFailed(_) | CliError::Io(_) | CliError::Internal(_) => 1,
    }
}

// ----------------------------------------------------------------------------
// 命令树概览（--help 后可附加；当前 clap 自带 help 已足够，此处保留为
// 未来扩展点 + 防止 CommandTree/Command/CommandSpec 导入未用警告）
// ----------------------------------------------------------------------------

/// 构造一个空 CommandTree（占位：当前命令树由 clap 子命令枚举承载，
/// CommandTree 用于库内 execute 派发测试；此处仅取类型以确保编译期链接）。
#[allow(dead_code)]
fn _command_tree_overview() -> Vec<CommandSpec> {
    let tree = CommandTree::new();
    tree.top_level_specs()
}

/// 防止 `Command` trait 导入被判定未用（库内 trait，main 通过 runner 间接消费）。
#[allow(dead_code)]
fn _assert_command_trait(_: &dyn Command) {}

// ============================================================================
// 单元测——main 内部纯函数（错误映射）
// ============================================================================
// 注意：本 binary crate 的 main 不直接单测（需 tokio runtime + 网络）；
// 命令解析/执行测见 cli.rs（FakeTransport 离线）。此处仅覆盖 exit_code_for
// 与 describe 两个纯函数的分支。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_invalid_args_and_command_not_found_are_2() {
        assert_eq!(exit_code_for(&CliError::InvalidArgs("x".into())), 2);
        assert_eq!(exit_code_for(&CliError::CommandNotFound("y".into())), 2);
    }

    #[test]
    fn exit_code_auth_failed_is_3() {
        assert_eq!(exit_code_for(&CliError::AuthFailed("no".into())), 3);
    }

    #[test]
    fn exit_code_api_connection_failed_is_4() {
        assert_eq!(
            exit_code_for(&CliError::ApiConnectionFailed("down".into())),
            4
        );
    }

    #[test]
    fn exit_code_internal_output_io_are_1() {
        assert_eq!(exit_code_for(&CliError::Internal("i".into())), 1);
        assert_eq!(exit_code_for(&CliError::OutputFailed("o".into())), 1);
        assert_eq!(exit_code_for(&CliError::Io(std::io::Error::other("x"))), 1);
    }

    #[test]
    fn describe_covers_all_variants() {
        assert_eq!(describe(&CliError::InvalidArgs("a".into())).0, "非法参数");
        assert_eq!(
            describe(&CliError::CommandNotFound("c".into())).0,
            "命令不存在"
        );
        assert_eq!(
            describe(&CliError::ApiConnectionFailed("d".into())).0,
            "API 连接失败"
        );
        assert_eq!(describe(&CliError::AuthFailed("e".into())).0, "认证失败");
        assert_eq!(describe(&CliError::OutputFailed("f".into())).0, "输出失败");
        assert_eq!(
            describe(&CliError::Io(std::io::Error::other("g"))).0,
            "IO 错误"
        );
        assert_eq!(describe(&CliError::Internal("h".into())).0, "内部错误");
    }

    #[test]
    fn command_tree_overview_is_empty_when_unpopulated() {
        // 当前未注册顶层命令（命令树承载由 clap 枚举负责）；返回空列表。
        assert!(_command_tree_overview().is_empty());
    }
}
