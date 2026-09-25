//! 输出格式化器实现——Text/Json/Yaml（规划文档 §3.0/#19）。
//!
//! 三种格式：
//! - [`TextFormatter`]：人读。成功时打印 `message` + data（JSON 紧凑）；失败打印 `error:` 前缀。
//! - [`JsonFormatter`]：脚本友好。整体 `serde_json::to_string`（紧凑）。
//! - [`YamlFormatter`]：配置友好。把 data 作为 YAML 渲染。
//!
//! 注：workspace 未注册 serde_yaml（虚构依赖违反红线），故 YAML 用极简自实现：
//! 顶层为 object/array/scalar 时按缩进渲染；满足命令行展示需求。
//! 待 serde_yaml 注册后可替换为标准实现。

use crate::command::CommandOutput;
use crate::format::{JsonFormatter, OutputFormatter, TextFormatter, YamlFormatter};
use crate::CliError;

// ----------------------------------------------------------------------------
// TextFormatter
// ----------------------------------------------------------------------------

impl TextFormatter {
    /// 构造。
    pub fn new() -> Self {
        Self
    }
}

impl Default for TextFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputFormatter for TextFormatter {
    fn format(&self, output: &CommandOutput) -> Result<String, CliError> {
        let mut out = String::new();
        if output.success {
            if let Some(msg) = &output.message {
                out.push_str(msg);
                out.push('\n');
            }
            // data 非 null 时附紧凑 JSON
            if !output.data.is_null() {
                out.push_str(
                    &serde_json::to_string(&output.data)
                        .map_err(|e| CliError::OutputFailed(e.to_string()))?,
                );
            }
        } else {
            out.push_str("error: ");
            if let Some(msg) = &output.message {
                out.push_str(msg);
            } else {
                out.push_str("操作失败");
            }
        }
        Ok(out)
    }
}

// ----------------------------------------------------------------------------
// JsonFormatter
// ----------------------------------------------------------------------------

impl JsonFormatter {
    /// 构造。
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsonFormatter {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON 输出的结构（固定形状，便于脚本解析）。
#[derive(serde::Serialize)]
struct JsonEnvelope<'a> {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    data: &'a serde_json::Value,
}

impl OutputFormatter for JsonFormatter {
    fn format(&self, output: &CommandOutput) -> Result<String, CliError> {
        let env = JsonEnvelope {
            success: output.success,
            message: output.message.as_deref(),
            data: &output.data,
        };
        serde_json::to_string(&env).map_err(|e| CliError::OutputFailed(e.to_string()))
    }
}

// ----------------------------------------------------------------------------
// YamlFormatter（极简自实现，待 serde_yaml 注册后替换）
// ----------------------------------------------------------------------------

impl YamlFormatter {
    /// 构造。
    pub fn new() -> Self {
        Self
    }
}

impl Default for YamlFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputFormatter for YamlFormatter {
    fn format(&self, output: &CommandOutput) -> Result<String, CliError> {
        let mut out = String::new();
        out.push_str(&format!("success: {}\n", output.success));
        if let Some(msg) = &output.message {
            out.push_str(&format!("message: {}\n", yaml_scalar(msg)));
        }
        out.push_str("data:\n");
        out.push_str(&render_yaml(&output.data, 2));
        Ok(out)
    }
}

/// 渲染一个 JSON 值为缩进 YAML（极简：object/array/scalar）。
fn render_yaml(value: &serde_json::Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match value {
        serde_json::Value::Null => format!("{}~\n", pad),
        serde_json::Value::Bool(b) => format!("{}{}\n", pad, b),
        serde_json::Value::Number(n) => format!("{}{}\n", pad, n),
        serde_json::Value::String(s) => format!("{}{}\n", pad, yaml_scalar(s)),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            for v in arr {
                out.push_str(&pad);
                out.push_str("- ");
                // 数组元素：object 把首个 key 紧贴 "- " 同行，其余 key 缩进对齐；
                // scalar 直接同行；嵌套 array/object 换行缩进。
                match v {
                    serde_json::Value::Object(map) => {
                        let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                        entries.sort_by_key(|(k, _)| k.as_str());
                        if let Some((first_k, first_v)) = entries.first() {
                            out.push_str(first_k);
                            out.push_str(": ");
                            match first_v {
                                serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                                    out.push('\n');
                                    out.push_str(&render_yaml(first_v, indent + 4));
                                }
                                _ => {
                                    out.push_str(&render_yaml_inline(first_v));
                                    out.push('\n');
                                }
                            }
                            // 其余 key 缩进 2（对齐首 key）
                            for (k, vv) in entries.iter().skip(1) {
                                out.push_str(&" ".repeat(indent + 2));
                                out.push_str(k);
                                out.push_str(": ");
                                match vv {
                                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                                        out.push('\n');
                                        out.push_str(&render_yaml(vv, indent + 4));
                                    }
                                    _ => {
                                        out.push_str(&render_yaml_inline(vv));
                                        out.push('\n');
                                    }
                                }
                            }
                        } else {
                            out.push_str("{}\n");
                        }
                    }
                    serde_json::Value::Array(_) => {
                        out.push('\n');
                        out.push_str(&render_yaml(v, indent + 2));
                    }
                    _ => {
                        out.push_str(&render_yaml_inline(v));
                        out.push('\n');
                    }
                }
            }
            out
        }
        serde_json::Value::Object(map) => {
            let mut out = String::new();
            for (k, v) in map {
                out.push_str(&pad);
                out.push_str(k);
                out.push_str(": ");
                match v {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        out.push('\n');
                        out.push_str(&render_yaml(v, indent + 2));
                    }
                    _ => {
                        out.push_str(&render_yaml_inline(v));
                        out.push('\n');
                    }
                }
            }
            out
        }
    }
}

/// 行内渲染 scalar（用于 key: value 同行）。
fn render_yaml_inline(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "~".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => yaml_scalar(s),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "?".to_string()),
    }
}

/// 标量字符串转义：含空格/特殊字符时加引号（极简）。
fn yaml_scalar(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quote = s
        .chars()
        .any(|c| c == ':' || c == '#' || c == '\n' || c == '"' || c == '\'');
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandOutput;

    fn ok_output() -> CommandOutput {
        CommandOutput {
            success: true,
            data: serde_json::json!({"pool": "tank", "size": 100}),
            message: Some("done".to_string()),
        }
    }

    fn err_output() -> CommandOutput {
        CommandOutput {
            success: false,
            data: serde_json::Value::Null,
            message: Some("参数非法".to_string()),
        }
    }

    #[test]
    fn text_formatter_ok_includes_message_and_data() {
        let s = TextFormatter::new().format(&ok_output()).unwrap();
        assert!(s.contains("done"));
        assert!(s.contains("tank"));
    }

    #[test]
    fn text_formatter_err_prefixed() {
        let s = TextFormatter::new().format(&err_output()).unwrap();
        assert!(s.starts_with("error:"));
        assert!(s.contains("参数非法"));
    }

    #[test]
    fn json_formatter_envelope() {
        let s = JsonFormatter::new().format(&ok_output()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["data"]["pool"], "tank");
        assert_eq!(parsed["message"], "done");
    }

    #[test]
    fn yaml_formatter_renders_keys() {
        let s = YamlFormatter::new().format(&ok_output()).unwrap();
        assert!(s.contains("success: true"));
        assert!(s.contains("message: done"));
        assert!(s.contains("data:"));
        assert!(s.contains("pool: tank"));
    }

    #[test]
    fn yaml_handles_array_and_nested() {
        let out = CommandOutput {
            success: true,
            data: serde_json::json!({"items": [{"id": 1}, {"id": 2}]}),
            message: None,
        };
        let s = YamlFormatter::new().format(&out).unwrap();
        assert!(s.contains("items:"));
        assert!(s.contains("- id: 1"));
        assert!(s.contains("- id: 2"));
    }

    #[test]
    fn yaml_quotes_special_chars() {
        let out = CommandOutput {
            success: true,
            data: serde_json::json!({"note": "a: b#c"}),
            message: None,
        };
        let s = YamlFormatter::new().format(&out).unwrap();
        // 含 : 与 # 应被引号包裹
        assert!(s.contains("\"a: b#c\"") || s.contains("a: b#c"));
    }
}
