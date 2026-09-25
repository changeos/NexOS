//! `.desktop` 文件解析 + **真实文件读取**（freedesktop.org Desktop Entry Spec）。
//!
//! 定位：第三方 `.deb` 安装的图形应用会在 `/usr/share/applications/` 落
//! `.desktop` 文件（freedesktop.org 标准）。[`crate::pkg::PackageManager`] 在
//! install 后扫描该目录，把带图标的桌面应用归类为
//! [`crate::pkg::PackageSource::ThirdParty`]（"未知来源"），区别于官方 apt 源应用。
//!
//! 本模块实现 [Desktop Entry Spec][des] 的最小解析子集：
//! - 识别 `[Desktop Entry]` 主段；
//! - 解析 `Key=Value` 行（忽略注释 `#` / 空行）；
//! - 支持本地化键 `Key[lang]`（取裸 Key，丢弃 locale 后缀）；
//! - 不处理布尔/列表类型的语义（保留字符串原值，由调用方按需转换）。
//!
//! [des]: https://specifications.freedesktop.org/desktop-entry-spec/latest/
//!
//! 两层 API：
//! - 纯函数 [`parse_desktop_entry`]（`&str -> DesktopEntry`）——单测友好，无 IO；
//! - 文件 IO [`parse_desktop_file`] / [`scan_dir`] / [`scan_default_dirs`]——读真实
//!   `.desktop` 文件并解析，扫描结果过滤为图形应用（[`is_graphical_app`]）。
//!
//! 设计：
//! - 纯函数与文件 IO 分离——便于单测验证解析正确性，IO 测试用 tempdir；
//! - 错误：缺 `[Desktop Entry]` 段 / 非法 `Key=` 行 / 文件读失败。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{ComputeError, ComputeResult};

/// `.desktop` 文件解析结果（仅主段字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopEntry {
    /// 应用类型（Application / Link / Directory）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_type: Option<String>,
    /// 应用显示名（Name=）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 通用名（GenericName=）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generic_name: Option<String>,
    /// 注释/tooltip（Comment=）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// 启动命令（Exec=）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// 图标名或路径（Icon=）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// 分类（Categories=，分号分隔）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// 是否不显示在菜单（NoDisplay=true）
    #[serde(default)]
    pub no_display: bool,
    /// 是否终端应用（Terminal=true）
    #[serde(default)]
    pub terminal: bool,
    /// MIME 类型（MimeType=，分号分隔）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mime_types: Vec<String>,
    /// 关键字（Keywords=，分号分隔）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// 原始键值对（兜底，含未识别字段）
    #[serde(default, skip_serializing)]
    pub raw: HashMap<String, String>,
}

/// 主段标签。
const DESKTOP_ENTRY_HEADER: &str = "[Desktop Entry]";

/// 解析 `.desktop` 文件内容。
///
/// 规则：
/// - 第一非空非注释行须为 `[Desktop Entry]`，否则 `InvalidSpec`；
/// - `[Other Section]` 段（如 `[Desktop Action X]`）忽略——只取主段；
/// - `#` 开头行 / 空行忽略；
/// - `Key=Value`：key 转 ASCII 小写规范化（spec 要求大小写敏感，但实践中文件混杂，
///   统一小写以容忍），`Key[locale]` 折叠成 `Key`（取首个出现的 locale 值）；
/// - value 不 strip 引号（spec 不要求引号），保留原值。
///
/// 失败返回 `ComputeError::InvalidSpec`。
pub fn parse_desktop_entry(content: &str) -> ComputeResult<DesktopEntry> {
    let mut entry = DesktopEntry::default();
    let mut in_main = false; // 当前是否处于 [Desktop Entry] 段
    let mut saw_main_section = false; // 文件是否曾出现 [Desktop Entry] 段（用于最终校验）
    let mut in_any_section = false;

    for raw_line in content.lines() {
        let line = raw_line.trim();

        // 空行 / 注释
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // 段头
        if line.starts_with('[') {
            in_any_section = true;
            in_main = line == DESKTOP_ENTRY_HEADER;
            if in_main {
                saw_main_section = true;
            }
            continue;
        }

        // 段头之前的键值对（spec 禁止，但容忍：归到 raw，不当主段）
        if !in_any_section {
            // 缺段头——直接报错更严格；这里选择宽容：继续直到遇到 [Desktop Entry]
            continue;
        }
        if !in_main {
            // 其他段的键值对忽略
            continue;
        }

        // Key=Value
        let Some((key, value)) = line.split_once('=') else {
            return Err(ComputeError::InvalidSpec(format!(
                "非法 .desktop 行（缺 `=`）: {raw_line}"
            )));
        };

        // 折叠 locale 后缀：Name[zh_CN] -> Name
        let bare_key = key
            .split('[')
            .next()
            .unwrap_or(key)
            .trim()
            .to_ascii_lowercase();

        let v = value.trim().to_string();

        // 已有该 bare key（来自更早的 locale 或裸值）则跳过——保留首个
        if entry.raw.contains_key(&bare_key) {
            continue;
        }
        entry.raw.insert(bare_key.clone(), v.clone());
        assign_known_field(&mut entry, &bare_key, &v);
    }

    if !saw_main_section {
        return Err(ComputeError::InvalidSpec(format!(
            "缺 `{DESKTOP_ENTRY_HEADER}` 主段"
        )));
    }

    Ok(entry)
}

/// 把已知键赋值到结构化字段（其余留在 raw）。
fn assign_known_field(entry: &mut DesktopEntry, key: &str, value: &str) {
    match key {
        "type" => entry.entry_type = Some(value.to_string()),
        "name" => entry.name = Some(value.to_string()),
        "genericname" => entry.generic_name = Some(value.to_string()),
        "comment" => entry.comment = Some(value.to_string()),
        "exec" => entry.exec = Some(value.to_string()),
        "icon" => entry.icon = Some(value.to_string()),
        "categories" => entry.categories = split_semi(value),
        "mimetype" => entry.mime_types = split_semi(value),
        "keywords" => entry.keywords = split_semi(value),
        "nodisplay" => entry.no_display = parse_bool(value),
        "terminal" => entry.terminal = parse_bool(value),
        _ => {}
    }
}

/// 分号分隔列表（spec 用 `;`，容忍尾随空）。
fn split_semi(v: &str) -> Vec<String> {
    v.split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// spec 布尔值：true/false（大小写敏感，宽容小写）。
fn parse_bool(v: &str) -> bool {
    v.trim().eq_ignore_ascii_case("true")
}

/// 判断 `.desktop` 是否表示"第三方图形应用"——有 Icon + Type=Application + 非系统类。
///
/// 供 [`crate::pkg::PackageManager`] 决定归类用。返回 true 即应归
/// [`crate::pkg::PackageSource::ThirdParty`]。
pub fn is_graphical_app(entry: &DesktopEntry) -> bool {
    entry.entry_type.as_deref() == Some("Application") && entry.icon.is_some() && !entry.no_display
}

// ----------------------------------------------------------------------------
// 文件 IO（读真实 .desktop 文件 + 扫描目录）
// ----------------------------------------------------------------------------

/// 从文件系统读取并解析单个 `.desktop` 文件。
///
/// 读取整个文件为 UTF-8 字符串后调 [`parse_desktop_entry`]。文件读失败（不存在/
/// 权限不足）映射 `ComputeError::Io`；解析失败（缺主段/非法行）映射 `InvalidSpec`。
pub fn parse_desktop_file(path: &std::path::Path) -> ComputeResult<DesktopEntry> {
    let content = std::fs::read_to_string(path)?;
    parse_desktop_entry(&content)
}

/// 扫描目录下所有 `*.desktop` 文件，解析为 [`DesktopEntry`]。
///
/// 容错策略：单个文件解析失败（如非本规范的 `.desktop`）**跳过不报错**——扫描结果
/// 只含成功解析的条目（实现层 `list_installed` 不应因个别损坏文件整体失败）。
/// 子目录不递归（仅顶层，匹配 `/usr/share/applications/` 扁平布局）。
/// 返回 `(entry, file_path)` 列表（按文件名排序，确定性）。
pub fn scan_dir(dir: &std::path::Path) -> ComputeResult<Vec<(DesktopEntry, std::path::PathBuf)>> {
    let mut entries: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        // key 用文件名（去后缀）做排序，便于确定性
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        entries.push((std::path::PathBuf::from(stem), path));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut result = Vec::new();
    for (_, path) in entries {
        // 解析失败的文件跳过（容忍损坏/非标准文件）
        if let Ok(entry) = parse_desktop_file(&path) {
            result.push((entry, path));
        }
    }
    Ok(result)
}

/// 扫描 [`crate::apt::DESKTOP_FILE_DIRS`] 默认目录，返回所有**图形应用**
/// （[`is_graphical_app`] 过滤）。
///
/// 任一目录不存在不报错（容忍未安装 flatpak 等）——`read_dir` 失败的目录直接跳过。
/// 返回 `(entry, file_path)` 列表（按文件名排序，确定性）。
pub fn scan_default_dirs() -> Vec<(DesktopEntry, std::path::PathBuf)> {
    let mut result = Vec::new();
    for dir in crate::apt::DESKTOP_FILE_DIRS {
        let dir_path = std::path::Path::new(dir);
        // 目录不存在（如未装 flatpak）静默跳过——read_dir 失败也跳过
        if let Ok(entries) = scan_dir(dir_path) {
            for (entry, path) in entries {
                if is_graphical_app(&entry) {
                    result.push((entry, path));
                }
            }
        }
    }
    result
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# 注释行
[Desktop Entry]
Type=Application
Name=Visual Studio Code
Name[zh_CN]=代码编辑器
Comment=Code Editing. Redefined.
Exec=/usr/share/code/code %U
Icon=vscode
Categories=Development;IDE;
MimeType=text/plain;
Keywords=vscode;code;
Terminal=false
"#;

    #[test]
    fn parses_known_fields() {
        let e = parse_desktop_entry(SAMPLE).unwrap();
        assert_eq!(e.entry_type.as_deref(), Some("Application"));
        assert_eq!(e.name.as_deref(), Some("Visual Studio Code"));
        assert_eq!(e.exec.as_deref(), Some("/usr/share/code/code %U"));
        assert_eq!(e.icon.as_deref(), Some("vscode"));
        assert_eq!(e.categories, vec!["Development", "IDE"]);
        assert_eq!(e.mime_types, vec!["text/plain"]);
        assert!(!e.terminal);
    }

    #[test]
    fn locale_suffix_collapses_to_bare_key_first_wins() {
        // Name 在 Name[zh_CN] 之前，应保留首个
        let e = parse_desktop_entry(SAMPLE).unwrap();
        assert_eq!(e.name.as_deref(), Some("Visual Studio Code"));
        // raw 中 bare key 只一份
        assert!(e.raw.contains_key("name"));
        assert!(!e.raw.contains_key("name[zh_cn]"));
    }

    #[test]
    fn missing_header_errors() {
        let err = parse_desktop_entry("Type=Application\nName=x").unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn malformed_kv_line_errors() {
        let err = parse_desktop_entry("[Desktop Entry]\nBadLineNoEquals\n").unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn ignores_other_sections() {
        let content = r#"[Desktop Entry]
Name=Main
[Desktop Action Open]
Name=Open File
Exec=open %f
"#;
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.name.as_deref(), Some("Main"));
        // Open File / open 来自 action 段，应被忽略
        assert_eq!(e.exec, None);
    }

    #[test]
    fn empty_and_comment_lines_skipped() {
        let content = "[Desktop Entry]\n\n# hi\nName=Foo\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.name.as_deref(), Some("Foo"));
    }

    #[test]
    fn nodisplay_and_terminal_parsed_as_bool() {
        let content = "[Desktop Entry]\nNoDisplay=true\nTerminal=TRUE\n";
        let e = parse_desktop_entry(content).unwrap();
        assert!(e.no_display);
        assert!(e.terminal);
    }

    #[test]
    fn semicolon_lists_handle_trailing_separator() {
        let content = "[Desktop Entry]\nCategories=A;B;\nMimeType=x;\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.categories, vec!["A", "B"]);
        assert_eq!(e.mime_types, vec!["x"]);
    }

    #[test]
    fn unknown_keys_kept_in_raw() {
        let content = "[Desktop Entry]\nX-Custom=hello\nStartupNotify=true\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.raw.get("x-custom").unwrap(), "hello");
        assert_eq!(e.raw.get("startupnotify").unwrap(), "true");
    }

    #[test]
    fn is_graphical_app_requires_icon_and_application_type() {
        let mut e = parse_desktop_entry(SAMPLE).unwrap();
        assert!(is_graphical_app(&e));
        e.icon = None;
        assert!(!is_graphical_app(&e));
        e.icon = Some("x".into());
        e.entry_type = Some("Link".into());
        assert!(!is_graphical_app(&e));
    }

    #[test]
    fn nodisplay_excludes_from_graphical() {
        let mut e = parse_desktop_entry(SAMPLE).unwrap();
        e.no_display = true;
        assert!(!is_graphical_app(&e));
    }

    // --------------------------------------------------------------------
    // 文件 IO 测（tempdir 真实文件系统）
    // --------------------------------------------------------------------

    #[test]
    fn parse_desktop_file_reads_and_parses_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("code.desktop");
        std::fs::write(&path, SAMPLE).unwrap();

        let e = parse_desktop_file(&path).unwrap();
        assert_eq!(e.entry_type.as_deref(), Some("Application"));
        assert_eq!(e.name.as_deref(), Some("Visual Studio Code"));
        assert_eq!(e.icon.as_deref(), Some("vscode"));
    }

    #[test]
    fn parse_desktop_file_missing_returns_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = parse_desktop_file(&tmp.path().join("nope.desktop")).unwrap_err();
        assert!(matches!(err, ComputeError::Io(_)));
    }

    #[test]
    fn parse_desktop_file_malformed_returns_invalid_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.desktop");
        std::fs::write(&path, "Type=Application\nName=x\n").unwrap(); // 缺 [Desktop Entry]
        let err = parse_desktop_file(&path).unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn scan_dir_parses_all_desktop_files_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        // 写三个 .desktop + 一个非 .desktop（应被忽略）
        std::fs::write(tmp.path().join("zcode.desktop"), SAMPLE).unwrap();
        std::fs::write(
            tmp.path().join("anano.desktop"),
            "[Desktop Entry]\nType=Application\nName=Nano\nIcon=nano\nExec=nano\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("README"), "not desktop").unwrap();
        // 损坏的 .desktop 应被跳过（容忍）
        std::fs::write(tmp.path().join("mbroken.desktop"), "no header here").unwrap();

        let entries = scan_dir(tmp.path()).unwrap();
        // 损坏的被跳过，剩 anano + zcode（按文件名排序）
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0.name.as_deref(), Some("Nano"));
        assert_eq!(entries[1].0.name.as_deref(), Some("Visual Studio Code"));
        // 第二项路径应以 zcode.desktop 结尾
        assert_eq!(entries[1].1.file_name().unwrap(), "zcode.desktop");
    }

    #[test]
    fn scan_dir_empty_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = scan_dir(tmp.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn scan_dir_missing_returns_io_error() {
        let err = scan_dir(std::path::Path::new("/nonexistent/deskd-12345")).unwrap_err();
        assert!(matches!(err, ComputeError::Io(_)));
    }

    #[test]
    fn scan_dir_skips_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        // 子目录（名为 foo.desktop）应被 is_file 跳过
        std::fs::create_dir(tmp.path().join("foo.desktop")).unwrap();
        let entries = scan_dir(tmp.path()).unwrap();
        assert!(entries.is_empty(), "子目录不应被当作 .desktop 解析");
    }

    #[test]
    fn scan_default_dirs_filters_graphical_apps() {
        // scan_default_dirs 扫真实系统目录——可能为空（无 GUI）或非空。
        // 此处不强断言内容（环境相关），只验证：不 panic + 返回 Vec + 全部满足
        // is_graphical_app（过滤器生效）。
        let apps = scan_default_dirs();
        for (entry, path) in &apps {
            assert!(
                is_graphical_app(entry),
                "非图形应用漏过过滤器: {}",
                path.display()
            );
            assert!(path.extension().and_then(|e| e.to_str()) == Some("desktop"));
        }
    }

    // --------------------------------------------------------------------
    // 补充测：serde 往返 / 边界 / Default / parse 边界
    // --------------------------------------------------------------------

    #[test]
    fn desktop_entry_default_all_none_empty() {
        let e = DesktopEntry::default();
        assert!(e.entry_type.is_none());
        assert!(e.name.is_none());
        assert!(e.generic_name.is_none());
        assert!(e.comment.is_none());
        assert!(e.exec.is_none());
        assert!(e.icon.is_none());
        assert!(e.categories.is_empty());
        assert!(e.mime_types.is_empty());
        assert!(e.keywords.is_empty());
        assert!(!e.no_display);
        assert!(!e.terminal);
        assert!(e.raw.is_empty());
    }

    #[test]
    fn desktop_entry_serde_roundtrip() {
        let mut e = parse_desktop_entry(SAMPLE).unwrap();
        // 注入一些 raw 字段测试 raw HashMap 序列化（注意：raw 用 skip_serializing，
        // roundtrip 后会丢失 raw，故只比较结构化字段）
        e.raw.insert("custom-key".to_string(), "value".to_string());
        let json = serde_json::to_string(&e).unwrap();
        let back: DesktopEntry = serde_json::from_str(&json).unwrap();
        // 结构化字段应一致
        assert_eq!(back.entry_type, e.entry_type);
        assert_eq!(back.name, e.name);
        assert_eq!(back.icon, e.icon);
        assert_eq!(back.categories, e.categories);
        // raw 因 skip_serializing 反序列化为空（仅 #[serde(default)] 兜底）
        assert!(
            back.raw.is_empty(),
            "raw 用 skip_serializing 故 roundtrip 后为空"
        );
    }

    #[test]
    fn desktop_entry_serde_skip_none_and_empty() {
        // Default entry：None 字段被 skip_serializing_if 跳过，Vec 空被跳过，
        // raw 被 skip_serializing 跳过；仅 bool 字段（no_display/terminal）始终序列化
        let e = DesktopEntry::default();
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, "{\"no_display\":false,\"terminal\":false}");
    }

    #[test]
    fn parse_desktop_entry_minimal_header_only() {
        // 仅有 header 段，无任何键值对 → Ok + 空 entry
        let e = parse_desktop_entry("[Desktop Entry]\n").unwrap();
        assert!(e.entry_type.is_none());
        assert!(e.name.is_none());
    }

    #[test]
    fn parse_desktop_entry_key_with_spaces() {
        // Key = Value（带空格）：trim 处理空格
        let content = "[Desktop Entry]\nName = Spaced Name \n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.name.as_deref(), Some("Spaced Name"));
    }

    #[test]
    fn parse_desktop_entry_value_with_special_chars() {
        // value 含 = 字符：split_once 只切第一个 =
        let content = "[Desktop Entry]\nExec=sh -c \"a=b && c\"\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.exec.as_deref(), Some("sh -c \"a=b && c\""));
    }

    #[test]
    fn parse_desktop_entry_locale_only_no_bare() {
        // 只有 Name[zh_CN]，无 Name：应被折叠为 name
        let content = "[Desktop Entry]\nName[zh_CN]=代码编辑器\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.name.as_deref(), Some("代码编辑器"));
        assert!(e.raw.contains_key("name"));
    }

    #[test]
    fn parse_desktop_entry_keys_are_lowercased() {
        // 大写键名 → 折叠小写
        let content = "[Desktop Entry]\nTYPE=Application\nNAME=Foo\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.entry_type.as_deref(), Some("Application"));
        assert_eq!(e.name.as_deref(), Some("Foo"));
        assert!(e.raw.contains_key("type"));
        assert!(e.raw.contains_key("name"));
    }

    #[test]
    fn parse_desktop_entry_empty_value_kept() {
        // 空 value：仍写入 raw
        let content = "[Desktop Entry]\nComment=\n";
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.comment.as_deref(), Some(""));
        assert_eq!(e.raw.get("comment").unwrap(), "");
    }

    #[test]
    fn parse_desktop_entry_unknown_section_only_no_error() {
        // 仅 [Desktop Action X] 无 [Desktop Entry] → 缺主段错误
        let err = parse_desktop_entry("[Desktop Action Open]\nName=x\n").unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn parse_desktop_entry_multiple_sections_picks_only_main() {
        let content =
            "[Desktop Entry]\nName=Main\n\n[Other]\nFoo=bar\n\n[Desktop Entry]\nType=Link\n";
        // 多次出现 [Desktop Entry] —— 当前实现遇段头即 in_main = (line == header)
        // 第二段又切回主段，但其字段会覆盖（受 raw contains_key 保护，首条保留）
        let e = parse_desktop_entry(content).unwrap();
        assert_eq!(e.name.as_deref(), Some("Main"));
        // 不直接断 Type 是否被覆盖（依赖实现细节），仅验证不 panic
    }

    #[test]
    fn parse_bool_various_forms() {
        // 直接测 parse_bool 容错
        assert!(parse_bool("true"));
        assert!(parse_bool("True"));
        assert!(parse_bool("TRUE"));
        assert!(parse_bool(" true "));
        assert!(!parse_bool("false"));
        assert!(!parse_bool("1"));
        assert!(!parse_bool(""));
        assert!(!parse_bool("yes"));
    }

    #[test]
    fn split_semi_handles_various_inputs() {
        assert_eq!(split_semi("a;b;c"), vec!["a", "b", "c"]);
        assert_eq!(split_semi("a;b;"), vec!["a", "b"]); // 尾随空
        assert_eq!(split_semi(";"), Vec::<String>::new());
        assert_eq!(split_semi(""), Vec::<String>::new());
        assert_eq!(split_semi("only"), vec!["only"]);
        assert_eq!(split_semi(" a ; b "), vec!["a", "b"]); // 空格被 trim
    }

    #[test]
    fn parse_desktop_file_reads_utf8_with_crlf() {
        // CRLF 行尾
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crlf.desktop");
        std::fs::write(&path, "[Desktop Entry]\r\nType=Application\r\nName=Win\r\n").unwrap();
        let e = parse_desktop_file(&path).unwrap();
        assert_eq!(e.entry_type.as_deref(), Some("Application"));
        assert_eq!(e.name.as_deref(), Some("Win"));
    }

    #[test]
    fn scan_dir_handles_hidden_desktop_extension_files() {
        // .txt / 无扩展名 / 大写后缀均应跳过（仅 *.desktop）
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.desktop"),
            "[Desktop Entry]\nName=A\nType=Application\nIcon=a\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("b.txt"), "[Desktop Entry]\nName=B\n").unwrap();
        std::fs::write(tmp.path().join("c"), "[Desktop Entry]\nName=C\n").unwrap();
        let entries = scan_dir(tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.name.as_deref(), Some("A"));
    }
}
