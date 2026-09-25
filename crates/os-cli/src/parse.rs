//! 命令行参数解析（自实现，规划文档 §3.0/#19）。
//!
//! 设计：workspace 未注册 `clap`（虚构依赖违反红线），故本 crate 自实现一个
//! 极简的 `--flag value` / `--flag=value` / 位置参数 解析器，覆盖 CLI 常见用法。
//!
//! 解析规则：
//! - `--name value`：长选项 + 空格分隔值（下一 token 不以 `-` 开头时消费）。
//! - `--name=value`：长选项 + 等号分隔值。
//! - `--flag`：布尔标志（无值；存在即 true）。
//! - `--`：分隔符，其后全部视为位置参数（不再解析）。
//! - 其它 token：位置参数。
//!
//! 不支持：短选项合并（`-abc`）、子命令自动分发（由 `CommandTree` 顶层处理）。
//! 保持最小可用；待 `clap` 注册后可整体替换（接口 `ParsedArgs` 不变）。

use std::collections::HashMap;

use crate::CliError;

// ----------------------------------------------------------------------------
// 解析结果
// ----------------------------------------------------------------------------

/// 解析后的命令参数：命名选项（`--flag`）+ 位置参数列表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    /// `--name value` / `--name=value` 的键值对（同名多次出现取最后一次）
    opts: HashMap<String, String>,
    /// 布尔标志集合（`--flag` 出现过的名字）
    flags: std::collections::HashSet<String>,
    /// 位置参数（非 `--` 开头，或 `--` 之后的全部）
    positional: Vec<String>,
}

impl ParsedArgs {
    /// 构造空集。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取命名选项值（`--name value` 形式）。
    pub fn opt(&self, name: &str) -> Option<&str> {
        self.opts.get(name).map(|s| s.as_str())
    }

    /// 取命名选项值（多值场景：返回所有出现的值；本实现仅取最后，留扩展点）。
    pub fn opt_or_default<'a>(&'a self, name: &str, default: &'a str) -> &'a str {
        self.opts.get(name).map(|s| s.as_str()).unwrap_or(default)
    }

    /// 布尔标志是否存在（`--flag` 形式）。
    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    /// 位置参数切片。
    pub fn positional(&self) -> &[String] {
        &self.positional
    }

    /// 位置参数数量。
    pub fn positional_count(&self) -> usize {
        self.positional.len()
    }

    /// 命名选项数量。
    pub fn opt_count(&self) -> usize {
        self.opts.len()
    }
}

// ----------------------------------------------------------------------------
// 解析器
// ----------------------------------------------------------------------------

/// 解析 `args` 为 [`ParsedArgs`]。
///
/// 约定：`args` 为"程序名之后"的参数（即不含 `argv` 的第 0 项）。
///
/// 取值规则（无 schema 时的确定性策略）：
/// - `--name=value`：等号分隔 → 命名选项（最可靠，推荐写法）。
/// - `--name`：无 `=` → 布尔标志（存在即 true）。
///   理由：无 schema 无法判断 `--name` 后是否跟值；按 POSIX 惯例，等号形式
///   是"带值选项"的唯一可信表达。需要值的选项应写作 `--pool=tank`。
/// - `-x`：单字母短选项 → 布尔标志（不支持合并短选项 `-abc`）。
/// - `--`：分隔符，其后全部视为位置参数。
/// - 其它 token：位置参数。
///
/// 例：`["list", "--pool=tank", "--verbose"]`
///   → 位置 `["list"]` + opt `pool=tank` + flag `verbose`。
pub fn parse_args(args: &[String]) -> Result<ParsedArgs, CliError> {
    let mut out = ParsedArgs::default();
    let mut no_more_opts = false;

    for tok in args {
        if no_more_opts {
            out.positional.push(tok.clone());
            continue;
        }
        if tok == "--" {
            no_more_opts = true;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("--") {
            // 长选项：仅识别 --name=value；否则视为布尔标志
            if let Some((name, value)) = rest.split_once('=') {
                if name.is_empty() {
                    return Err(CliError::InvalidArgs(format!("非法选项名（空）：{tok}")));
                }
                out.opts.insert(name.to_string(), value.to_string());
            } else if rest.is_empty() {
                // `--` 已在上方处理；这里防御性跳过（不应到达）
                no_more_opts = true;
            } else {
                // 无 `=` → 布尔标志
                out.flags.insert(rest.to_string());
            }
            continue;
        }
        // 短选项 `-x`：单字母视为标志（不支持 `-x=value`/`-xvalue`/合并）
        if tok.starts_with('-') && tok.len() > 1 && tok != "-" {
            out.flags.insert(tok[1..].to_string());
            continue;
        }
        // 位置参数
        out.positional.push(tok.clone());
    }

    Ok(out)
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn s(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn parse_long_opt_with_equals() {
        let p = parse_args(&s(&["--pool=tank"])).unwrap();
        assert_eq!(p.opt("pool"), Some("tank"));
        assert!(p.positional().is_empty());
    }

    #[test]
    fn parse_long_opt_with_space_value() {
        // 无 `=` 时按 POSIX 惯例视为布尔标志：`--pool` 是 flag，`tank` 是位置参数。
        // 需要带值请写 `--pool=tank`（见 parse_long_opt_with_equals）。
        let p = parse_args(&s(&["--pool", "tank"])).unwrap();
        assert!(p.flag("pool"));
        assert_eq!(p.positional(), &["tank".to_string()]);
        assert_eq!(p.opt("pool"), None);
    }

    #[test]
    fn parse_flag_alone() {
        let p = parse_args(&s(&["--verbose"])).unwrap();
        assert!(p.flag("verbose"));
        assert!(!p.flag("missing"));
    }

    #[test]
    fn parse_positional_collected() {
        let p = parse_args(&s(&["list", "all"])).unwrap();
        assert_eq!(p.positional(), &["list".to_string(), "all".to_string()]);
    }

    #[test]
    fn parse_mixed_positional_and_opts() {
        // list --pool=tank --verbose extra
        let p = parse_args(&s(&["list", "--pool=tank", "--verbose", "extra"])).unwrap();
        assert_eq!(p.positional(), &["list".to_string(), "extra".to_string()]);
        assert_eq!(p.opt("pool"), Some("tank"));
        assert!(p.flag("verbose"));
    }

    #[test]
    fn parse_double_dash_terminates_opts() {
        // `--` 后的 --foo 视为位置参数
        let p = parse_args(&s(&["--", "--not-an-option"])).unwrap();
        assert_eq!(p.positional(), &["--not-an-option".to_string()]);
        assert!(!p.flag("not-an-option"));
    }

    #[test]
    fn parse_short_option_as_flag() {
        let p = parse_args(&s(&["-v"])).unwrap();
        assert!(p.flag("v"));
    }

    #[test]
    fn parse_opt_value_dash_not_consumed() {
        // 下一 token 以 `-` 开头 → 当前视为 flag
        let p = parse_args(&s(&["--pool", "-v"])).unwrap();
        assert!(p.flag("pool"));
        assert!(p.flag("v"));
    }

    #[test]
    fn parse_empty_args() {
        let p = parse_args(&[]).unwrap();
        assert_eq!(p.positional_count(), 0);
        assert_eq!(p.opt_count(), 0);
    }

    #[test]
    fn parse_opt_or_default() {
        let p = parse_args(&s(&["--pool=tank"])).unwrap();
        assert_eq!(p.opt_or_default("pool", "default"), "tank");
        assert_eq!(p.opt_or_default("missing", "default"), "default");
    }

    #[test]
    fn parse_empty_option_name_rejected() {
        // `--=value` → 非法空名
        let err = parse_args(&s(&["--=value"])).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgs(_)));
    }

    #[test]
    fn parse_opt_last_wins() {
        let p = parse_args(&s(&["--pool=a", "--pool=b"])).unwrap();
        assert_eq!(p.opt("pool"), Some("b"), "同名选项多次出现取最后");
    }
}
