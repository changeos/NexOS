//! 命令树解析与分发（规划文档 §3.0/#19）。
//!
//! 设计：
//! - `CommandTree` 持有顶层 `Command` 列表（如 storage/network/vm）。
//! - `resolve` 把原始 `args`（如 `["storage", "dataset", "list", "--pool=tank"]`）
//!   按命令名逐级匹配子命令，返回命中的叶子 Command 与剩余参数。
//! - `execute_path` 在给定上下文下执行解析后的命令链。
//!
//! 子命令匹配策略：CommandSpec.name 精确匹配下一个 token；无匹配则把当前 Command
//! 当作叶子（剩余 token 全部作为 args 传给 execute）。这样支持
//! `os storage list`（storage 为叶子）与 `os storage dataset list`（多级）两种形态。
//!
//! 本模块为纯逻辑（不含网络），便于单测命令解析正确性。

use std::collections::HashMap;

use crate::command::{Command, CommandContext, CommandOutput, CommandSpec};
use crate::CliError;

// ----------------------------------------------------------------------------
// CommandTree
// ----------------------------------------------------------------------------

/// 命令树——持有顶层命令，支持按名解析与执行。
pub struct CommandTree {
    /// 顶层命令名 → Command 句柄
    roots: HashMap<String, Box<dyn Command>>,
}

impl std::fmt::Debug for CommandTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.roots.keys().map(|s| s.as_str()).collect();
        f.debug_struct("CommandTree")
            .field("roots", &names)
            .finish()
    }
}

impl CommandTree {
    /// 创建空树。
    pub fn new() -> Self {
        Self {
            roots: HashMap::new(),
        }
    }

    /// 注册一个顶层命令（key 取 `command.name()`）。
    pub fn register(&mut self, command: Box<dyn Command>) {
        let name = command.name().to_string();
        self.roots.insert(name, command);
    }

    /// 已注册顶层命令数。
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// 列出全部顶层命令规格（按名排序，确定性）。
    pub fn top_level_specs(&self) -> Vec<CommandSpec> {
        let mut names: Vec<&String> = self.roots.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|n| command_to_spec(self.roots[n].as_ref()))
            .collect()
    }

    /// 取顶层命令引用（按名）。
    pub fn get(&self, name: &str) -> Option<&dyn Command> {
        self.roots.get(name).map(|b| b.as_ref())
    }

    /// 解析 args：按 token 逐级匹配子命令，返回命中的命令名路径 + 剩余参数。
    ///
    /// 例：args=["storage","dataset","list","--pool=tank"]，
    /// 若 storage 有子命令 dataset、dataset 有子命令 list，则返回
    /// (["storage","dataset","list"], ["--pool=tank"])；
    /// 若 storage 无 dataset 子命令，则把 storage 当叶子，返回
    /// (["storage"], ["dataset","list","--pool=tank"])。
    ///
    /// 注：本骨架的 `Command` trait 仅暴露 `subcommands()` 规格（不暴露子命令对象），
    /// 故实际叶子执行由 `execute` 在命令内部完成；此处解析仅产出"理论上命中的命令名路径"
    /// 与"传给最终叶子的剩余参数"，并把第一个 token 命中的顶层 Command 返回。
    pub fn resolve<'a>(&'a self, args: &[String]) -> Result<ResolveResult<'a>, CliError> {
        if args.is_empty() {
            return Err(CliError::InvalidArgs("空命令".to_string()));
        }
        let root_name = &args[0];
        let root = self
            .roots
            .get(root_name)
            .ok_or_else(|| CliError::CommandNotFound(root_name.clone()))?;

        // 沿 subcommands 规格逐级匹配名（仅校验名是否存在于声明中，作 path 提示）。
        // 注：Command trait 仅暴露 subcommands() 规格（不暴露子命令对象），故实际仅做
        // "名命中"判定并把命中的子命令名记入 path（便于日志/帮助）；
        // 真正的叶子执行由顶层 Command.execute 在其内部完成，故 **不** 从 remaining 中
        // 消费子命令名——remaining 始终为根命令名之后的全部 token。
        let mut path = vec![root_name.clone()];
        let current_specs: Vec<CommandSpec> = root.subcommands();
        if args.len() > 1 {
            if let Some(spec) = current_specs.iter().find(|s| s.name == args[1]) {
                path.push(spec.name.clone());
            }
        }
        // remaining = 根命令名之后的全部参数（含子命令名本身，交由 execute 处理）
        let remaining = args[1..].to_vec();
        Ok(ResolveResult {
            command: root.as_ref(),
            path,
            remaining,
        })
    }

    /// 在给定上下文下执行解析后的命令（调用命中的顶层 Command.execute）。
    ///
    /// `args` 为去掉程序名后的原始参数（如 `["storage","list","--pool=tank"]`）。
    pub fn execute(
        &self,
        args: &[String],
        ctx: &mut CommandContext,
    ) -> Result<CommandOutput, CliError> {
        let resolved = self.resolve(args)?;
        // 把命令名路径之后的部分作为 execute 的 args
        resolved.command.execute(&resolved.remaining, ctx)
    }
}

impl Default for CommandTree {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析结果：命中的顶层命令引用 + 命令名路径 + 剩余参数。
pub struct ResolveResult<'a> {
    /// 命中的顶层 Command（实际执行者）。
    pub command: &'a dyn Command,
    /// 命令名路径（如 `["storage","dataset"]`）。
    pub path: Vec<String>,
    /// 传给最终叶子的剩余参数。
    pub remaining: Vec<String>,
}

/// 由 Command 实例构造其 CommandSpec（顶层规格）。
fn command_to_spec(c: &dyn Command) -> CommandSpec {
    CommandSpec {
        name: c.name().to_string(),
        description: c.description().to_string(),
        args: c.subcommands().into_iter().flat_map(|s| s.args).collect(),
    }
}

// ----------------------------------------------------------------------------
// 便捷：按输出格式选择 formatter
// ----------------------------------------------------------------------------

/// 按 `OutputFormat` 选择对应格式化器并渲染。
pub fn format_output(
    output: &CommandOutput,
    format: crate::command::OutputFormat,
) -> Result<String, CliError> {
    use crate::command::OutputFormat;
    // struct 在 format.rs 声明（pub），impl OutputFormatter 在 format_impl.rs；
    // 调用 .format() 须把 trait 引入作用域。
    use crate::format::{JsonFormatter, OutputFormatter, TextFormatter, YamlFormatter};
    match format {
        OutputFormat::Text => TextFormatter::new().format(output),
        OutputFormat::Json => JsonFormatter::new().format(output),
        OutputFormat::Yaml => YamlFormatter::new().format(output),
    }
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandContext, CommandOutput, CommandSpec, OutputFormat};

    /// 测试用叶子命令：execute 回固定输出，subcommands 暴露声明。
    struct LeafCommand {
        name: String,
        subs: Vec<CommandSpec>,
    }

    impl Command for LeafCommand {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test leaf"
        }
        fn subcommands(&self) -> Vec<CommandSpec> {
            self.subs.clone()
        }
        fn execute(
            &self,
            args: &[String],
            _ctx: &mut CommandContext,
        ) -> Result<CommandOutput, CliError> {
            Ok(CommandOutput {
                success: true,
                data: serde_json::json!({"name": self.name, "args": args}),
                message: None,
            })
        }
    }

    fn ctx() -> CommandContext {
        CommandContext {
            api_endpoint: None,
            token: None,
            format: OutputFormat::Text,
        }
    }

    #[test]
    fn resolve_root_only_passes_remaining() {
        let mut tree = CommandTree::new();
        tree.register(Box::new(LeafCommand {
            name: "storage".to_string(),
            subs: vec![],
        }));
        let r = tree
            .resolve(&[
                "storage".to_string(),
                "list".to_string(),
                "--pool=tank".to_string(),
            ])
            .unwrap();
        assert_eq!(r.path, vec!["storage".to_string()]);
        assert_eq!(
            r.remaining,
            vec!["list".to_string(), "--pool=tank".to_string()]
        );
    }

    #[test]
    fn resolve_unknown_root_errors() {
        let tree = CommandTree::new();
        // 不用 unwrap_err（ResolveResult 非 Debug，因含 trait object 引用）
        let res = tree.resolve(&["nope".to_string()]);
        assert!(matches!(res, Err(CliError::CommandNotFound(_))));
    }

    #[test]
    fn resolve_empty_args_errors() {
        let tree = CommandTree::new();
        let res = tree.resolve(&[]);
        assert!(matches!(res, Err(CliError::InvalidArgs(_))));
    }

    #[test]
    fn execute_dispatches_to_root_command() {
        let mut tree = CommandTree::new();
        tree.register(Box::new(LeafCommand {
            name: "vm".to_string(),
            subs: vec![CommandSpec {
                name: "list".to_string(),
                description: "list vms".to_string(),
                args: vec![],
            }],
        }));
        let mut c = ctx();
        let out = tree
            .execute(&["vm".to_string(), "list".to_string()], &mut c)
            .unwrap();
        assert!(out.success);
        assert_eq!(out.data["name"], "vm");
        assert_eq!(out.data["args"][0], "list");
    }

    #[test]
    fn top_level_specs_sorted() {
        let mut tree = CommandTree::new();
        tree.register(Box::new(LeafCommand {
            name: "zeta".to_string(),
            subs: vec![],
        }));
        tree.register(Box::new(LeafCommand {
            name: "alpha".to_string(),
            subs: vec![],
        }));
        let specs = tree.top_level_specs();
        assert_eq!(specs[0].name, "alpha");
        assert_eq!(specs[1].name, "zeta");
    }

    #[test]
    fn format_output_all_three_formats() {
        use crate::command::OutputFormat;
        let out = CommandOutput {
            success: true,
            data: serde_json::json!({"k": "v"}),
            message: Some("ok".to_string()),
        };
        let text = format_output(&out, OutputFormat::Text).unwrap();
        assert!(text.contains("ok"));
        let json = format_output(&out, OutputFormat::Json).unwrap();
        assert!(json.contains("\"success\":true"));
        let yaml = format_output(&out, OutputFormat::Yaml).unwrap();
        assert!(yaml.contains("k: v"));
    }

    /// 端到端：命令树解析 + 自实现参数解析器（parse_args）协同。
    /// 验证 `os storage list --pool=tank --verbose` 这样的典型 CLI 输入
    /// 被正确分发到顶层命令，且 `--pool=tank` / `--verbose` 被 parse_args 结构化。
    #[test]
    fn execute_with_parsed_args_end_to_end() {
        use crate::parse::parse_args;
        let mut tree = CommandTree::new();
        tree.register(Box::new(LeafCommand {
            name: "storage".to_string(),
            subs: vec![
                CommandSpec {
                    name: "list".to_string(),
                    description: "list pools".to_string(),
                    args: vec![],
                },
                CommandSpec {
                    name: "create".to_string(),
                    description: "create pool".to_string(),
                    args: vec![],
                },
            ],
        }));
        let raw = [
            "storage".to_string(),
            "list".to_string(),
            "--pool=tank".to_string(),
            "--verbose".to_string(),
        ];
        let mut c = ctx();
        let out = tree.execute(&raw, &mut c).unwrap();
        // execute 收到根名之后的全部 token（含子命令名 list）
        let args_from_execute = out.data["args"].as_array().unwrap();
        let arg_strings: Vec<String> = args_from_execute
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        // 用 parse_args 把 execute 收到的 args 解析为结构化
        let parsed = parse_args(&arg_strings).unwrap();
        assert_eq!(parsed.positional(), &["list".to_string()]);
        assert_eq!(parsed.opt("pool"), Some("tank"));
        assert!(parsed.flag("verbose"));
    }
}
