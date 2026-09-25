//! MCP tools 注册表——表驱动，把每个 MCP tool 映射到一条 os-api 路由。
//!
//! 设计（呼应任务表）：
//! - 每个 [`OsTool`] = `(name, description, method, api_path, params)` 描述体；
//!   `name` 是 MCP 客户端调 `tools/call` 时传的参数（如 `os_pool_list`），
//!   `api_path` 是 os-api 网关的相对路径（如 `/api/v1/pools`，可含 `{param}`
//!   路径模板），由 [`OsApiClient`](crate::api::OsApiClient) 拼成完整 URL。
//! - v0.1.50 前的 10 个 tools 全是无参只读 GET；v0.1.50 NexHub 工具面
//!   （docs/research/NEXHUB_FEATURES.md §2 top2）新增 8 个 `nexhub_*` tools——
//!   含**带参读**（repo/path/q）与**写操作**（POST：写文件 / 开 Issue / 建 PR /
//!   merge PR）。写操作经 os-mcp 的 admin token 落 os-api 网关鉴权，来源标记见
//!   [`OsTool::mcp_mark`]。
//! - 表驱动的好处：新增一个 tool 只需在 [`ALL_TOOLS`] 里加一行；URL/请求构造
//!   与 tool 匹配逻辑共用一份代码（[`crate::api::OsApiClient::call_tool`]），
//!   单测覆盖一次即覆盖全部。
//!
//! 注册的 tools（18 个 = 10 OS 管理 + 8 NexHub 仓库操作）：
//!
//! | tool name | 描述 | os-api 路由 |
//! |-----------|------|--------------|
//! | `os_status` | 查询 OS 系统状态 | `GET /status` |
//! | `os_pool_list` | 列出存储池 | `GET /api/v1/pools` |
//! | `os_dataset_list` | 列出数据集 | `GET /api/v1/datasets` |
//! | `os_snapshot_list` | 列出快照 | `GET /api/v1/snapshots` |
//! | `os_vm_list` | 列出虚拟机 | `GET /api/v1/vms` |
//! | `os_share_list` | 列出共享 | `GET /shares` |
//! | `os_user_list` | 列出用户 | `GET /api/v1/users` |
//! | `os_node_list` | 列出集群节点 | `GET /discover/nodes` |
//! | `os_virt_check` | CPU 虚拟化检测 | `GET /api/v1/system/virt-check` |
//! | `os_health` | 健康检查 | `GET /healthz` |
//! | `nexhub_list_repos` | 列 NexHub 仓库 | `GET /api/v1/coderepo/repos` |
//! | `nexhub_read_file` | 读仓库文件 | `GET /api/v1/coderepo/repos/{repo}/file` |
//! | `nexhub_write_file` | 写文件（git commit，author=mcp） | `POST /api/v1/coderepo/repos/{repo}/file` |
//! | `nexhub_search_code` | 代码搜索（grep） | `GET /api/v1/coderepo/repos/{repo}/search` |
//! | `nexhub_list_issues` | Issue 列表 | `GET /api/v1/coderepo/repos/{repo}/issues` |
//! | `nexhub_create_issue` | 建 Issue（label `mcp` 来源标记） | `POST /api/v1/coderepo/repos/{repo}/issues` |
//! | `nexhub_create_pr` | 建 PR（body 尾部 via 标记） | `POST /api/v1/coderepo/repos/{repo}/pulls` |
//! | `nexhub_merge_pr` | 合并 PR | `POST /api/v1/coderepo/repos/{repo}/pulls/{num}/merge` |

use serde::Serialize;

/// 一个 MCP tool 参数的静态描述（渲染进 `tools/list` 的 inputSchema）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolParam {
    /// 参数名（`tools/call` 的 `arguments.<name>`）。
    pub name: &'static str,
    /// 参数描述（给 AI 助手看）。
    pub description: &'static str,
    /// 是否必填（渲染进 inputSchema.required + 调用时校验）。
    pub required: bool,
    /// JSON Schema 类型（`string` / `number`）。
    pub kind: &'static str,
}

/// 一个 MCP tool 的静态描述（name + method + os-api 相对路径 + 参数表）。
///
/// 序列化为 MCP `tools/list` 响应的 `tools[]` 元素子集（`name` + `description`；
/// MCP 还要求 `inputSchema`——由 `jsonrpc` 层按 `params` 渲染补充）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OsTool {
    /// MCP tool 名（`tools/call` 参数 `name`，如 `os_pool_list`）。
    /// 全小写 + 下划线，符合 MCP tool 命名约定。
    pub name: &'static str,
    /// 工具的人类可读描述（给 AI 助手看，决定 AI 何时选该 tool；含返回形态说明）。
    pub description: &'static str,
    /// 对应的 os-api 相对路径（如 `/api/v1/pools`；可含 `{param}` 路径模板，
    /// 由 OsApiClient 替换 + 百分号编码后拼成完整 URL）。
    pub api_path: &'static str,
    /// HTTP 方法（`GET` / `POST`；GET 参数走 query，POST 参数走 JSON body）。
    pub method: &'static str,
    /// 参数表（空 = 无参 tool）。
    pub params: &'static [ToolParam],
    /// 写操作来源标记：true 时 POST body 自动附加 `mcp` 归因——有 `labels`
    /// 通道的工具（建 Issue）追加 `mcp` 标签；有 `body` 文本通道的工具（建 PR）
    /// 尾部追加 `(via os-mcp)`。写文件的 git author 由服务端固定为 mcp，无需此标。
    pub mcp_mark: bool,
}

/// 空参数表（无参 tool 共用）。
const NO_PARAMS: &[ToolParam] = &[];

/// 全部 MCP tools（表驱动，顺序即 `tools/list` 展示顺序）。
///
/// 用 `&'static` 切片常量：所有字段都是字符串字面量，零分配；`all_tools()` 直接返回引用。
pub const ALL_TOOLS: &[OsTool] = &[
    // ============ OS 管理面（v0.1.50 前的 10 个无参只读 GET）============
    OsTool {
        name: "os_status",
        description: "查询 OS 系统状态（CPU 虚拟化能力 + 版本 + 进程 uptime）",
        api_path: "/status",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_pool_list",
        description: "列出所有 ZFS 存储池（池名 + 状态 + 容量）",
        api_path: "/api/v1/pools",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_dataset_list",
        description: "列出所有 ZFS 数据集（路径 + 容量 + 快照计数）",
        api_path: "/api/v1/datasets",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_snapshot_list",
        description: "列出所有 ZFS 快照（路径 + 创建时间）",
        api_path: "/api/v1/snapshots",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_vm_list",
        description: "列出所有虚拟机（名称 + 状态 + CPU/内存）",
        api_path: "/api/v1/vms",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_share_list",
        description: "列出所有文件共享（SMB / NFS / WebDAV）",
        api_path: "/shares",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_user_list",
        description: "列出所有用户（用户名 + 角色 + 是否启用）",
        api_path: "/api/v1/users",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_node_list",
        description: "列出集群节点（hostname + 端点 + 能力）",
        api_path: "/discover/nodes",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_virt_check",
        description: "CPU 虚拟化能力详查（VMX/SVM + KVM 可用性 + 综合判定 + 诊断）",
        api_path: "/api/v1/system/virt-check",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "os_health",
        description: "os-api 健康检查（liveness 探针，返回 {status:ok}）",
        api_path: "/healthz",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    // ============ NexHub 仓库操作面（v0.1.50 top2，8 个带参/写工具）============
    OsTool {
        name: "nexhub_list_repos",
        description: "列出 NexHub 全部代码仓库。返回 {repos:[{name, description, \
                      size_bytes, last_commit, branch_count, commit_count, clone_url_ssh, \
                      clone_url_http}]}——仓库浏览/选仓的第一步。",
        api_path: "/api/v1/coderepo/repos",
        method: "GET",
        params: NO_PARAMS,
        mcp_mark: false,
    },
    OsTool {
        name: "nexhub_read_file",
        description: "读仓库文件内容（默认分支 HEAD 版本）。返回 {name, path, ok, \
                      exists, content}（content 为 UTF-8 全文；exists=false 表示\
                      路径不存在）。",
        api_path: "/api/v1/coderepo/repos/{repo}/file",
        method: "GET",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名（nexhub_list_repos 的 name，不含 .git）",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "path",
                description: "仓库相对路径（如 src/main.rs / README.md）",
                required: true,
                kind: "string",
            },
        ],
        mcp_mark: false,
    },
    OsTool {
        name: "nexhub_write_file",
        description: "写文件到仓库（全文覆盖，git plumbing 落一个 commit，提交\
                      author 固定为 mcp——AI 写入可追溯）。需 admin token。\
                      返回 {ok, name, path, branch, commit, author}。",
        api_path: "/api/v1/coderepo/repos/{repo}/file",
        method: "POST",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "path",
                description: "仓库相对路径（不可含 ..）",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "content",
                description: "文件全文（UTF-8，≤2 MiB）",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "message",
                description: "提交信息（缺省 mcp write: <path>）",
                required: false,
                kind: "string",
            },
            ToolParam {
                name: "branch",
                description: "目标分支（缺省 = 仓库默认分支）",
                required: false,
                kind: "string",
            },
        ],
        mcp_mark: false,
    },
    OsTool {
        name: "nexhub_search_code",
        description: "代码搜索（默认分支全树固定串子串匹配，简化 grep）。返回 \
                      {name, q, matches:[{path, line, text}]}（≤50 条命中）。",
        api_path: "/api/v1/coderepo/repos/{repo}/search",
        method: "GET",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "q",
                description: "搜索串（固定串，非正则）",
                required: true,
                kind: "string",
            },
        ],
        mcp_mark: false,
    },
    OsTool {
        name: "nexhub_list_issues",
        description: "列仓库 Issue。返回 {repo, state, issues:[{number, title, \
                      body, author, state, labels, comment_count, created_at, \
                      updated_at}]}。",
        api_path: "/api/v1/coderepo/repos/{repo}/issues",
        method: "GET",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "state",
                description: "状态过滤（open/closed/all，缺省 open）",
                required: false,
                kind: "string",
            },
        ],
        mcp_mark: false,
    },
    OsTool {
        name: "nexhub_create_issue",
        description: "开 Issue（写操作，需 admin token；自动附加 label `mcp` \
                      标记 AI 来源）。返回 {ok, issue:{number, ...}}。",
        api_path: "/api/v1/coderepo/repos/{repo}/issues",
        method: "POST",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "title",
                description: "标题（≤500 字符）",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "body",
                description: "正文（可空，≤20000 字符）",
                required: false,
                kind: "string",
            },
            ToolParam {
                name: "labels",
                description: "标签数组（如 [\"bug\",\"ui\"]；自动附加 mcp 标记）",
                required: false,
                kind: "string",
            },
        ],
        mcp_mark: true,
    },
    OsTool {
        name: "nexhub_create_pr",
        description: "提 Pull Request（写操作，需 admin token；body 尾部自动附\
                      (via os-mcp) 来源标记）。from_branch 须已 push 到裸仓。\
                      返回 {ok, pull:{number, from_branch, to_branch, ...}}。",
        api_path: "/api/v1/coderepo/repos/{repo}/pulls",
        method: "POST",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "title",
                description: "PR 标题",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "from_branch",
                description: "来源分支（须已 push 到仓库）",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "to_branch",
                description: "目标分支（缺省 = 仓库默认分支）",
                required: false,
                kind: "string",
            },
            ToolParam {
                name: "body",
                description: "PR 描述（可空）",
                required: false,
                kind: "string",
            },
        ],
        mcp_mark: true,
    },
    OsTool {
        name: "nexhub_merge_pr",
        description: "合并 Pull Request（写操作，需 admin token；merge-tree 3-way\
                      落地，冲突 409）。返回 {ok, number, state, merged_by, \
                      merged_sha}。",
        api_path: "/api/v1/coderepo/repos/{repo}/pulls/{num}/merge",
        method: "POST",
        params: &[
            ToolParam {
                name: "repo",
                description: "仓库名",
                required: true,
                kind: "string",
            },
            ToolParam {
                name: "num",
                description: "PR 编号（正整数，nexhub_create_pr 返回的 number）",
                required: true,
                kind: "number",
            },
        ],
        mcp_mark: false,
    },
];

/// 返回全部 MCP tools（`tools/list` 响应数据源）。
#[must_use]
pub fn all_tools() -> &'static [OsTool] {
    ALL_TOOLS
}

/// 按 tool 名查找（`tools/call` 时用：从参数 name 定位到 OsTool）。
#[must_use]
pub fn find_tool(name: &str) -> Option<&'static OsTool> {
    ALL_TOOLS.iter().find(|t| t.name == name)
}

/// 给定 os-api base URL（如 `http://127.0.0.1:8080`）与 tool，构造完整请求 URL。
///
/// 规范化：去掉 base URL 末尾的 `/`，再拼上 `api_path`（api_path 以 `/` 开头）。
/// 注意：带 `{param}` 模板的 tool 须先经 [`crate::api::OsApiClient::call_tool`]
/// 替换路径参数——本函数只做字面拼接（模板渲染后的 path 用）。
/// 例：base `http://127.0.0.1:8080/` + path `/api/v1/pools` → `http://127.0.0.1:8080/api/v1/pools`。
#[must_use]
pub fn build_url(base: &str, tool: &OsTool) -> String {
    let trimmed = base.trim_end_matches('/');
    format!("{trimmed}{api_path}", api_path = tool.api_path)
}

// ----------------------------------------------------------------------------
// 单元测试——tools 表完整性 + URL 构造 + tool 查找
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 至少 10 个 tools（任务要求）；本实现 18 个（10 OS + 8 NexHub）。
    #[test]
    fn has_at_least_ten_tools() {
        assert!(
            ALL_TOOLS.len() >= 10,
            "至少 10 个 tools，实际 {}",
            ALL_TOOLS.len()
        );
    }

    /// NexHub 工具面 8 个全部注册（任务 top2）。
    #[test]
    fn nexhub_tools_present() {
        let required = [
            "nexhub_list_repos",
            "nexhub_read_file",
            "nexhub_write_file",
            "nexhub_search_code",
            "nexhub_list_issues",
            "nexhub_create_issue",
            "nexhub_create_pr",
            "nexhub_merge_pr",
        ];
        assert_eq!(required.len(), 8);
        for name in required {
            assert!(find_tool(name).is_some(), "缺少 NexHub tool: {name}");
        }
        assert_eq!(ALL_TOOLS.len(), 18, "10 OS + 8 NexHub = 18: 实际 {}", ALL_TOOLS.len());
    }

    /// tool name 全部唯一（避免 tools/call 歧义）。
    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = ALL_TOOLS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "存在重复的 tool name");
    }

    /// 全部 tool name 符合 MCP 命名约定（小写字母 + 数字 + 下划线，字母开头）。
    #[test]
    fn tool_names_match_mcp_convention() {
        for t in ALL_TOOLS {
            assert!(
                t.name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase()),
                "tool name 须字母开头: {}",
                t.name
            );
            assert!(
                t.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "tool name 仅允许小写字母/数字/下划线: {}",
                t.name
            );
        }
    }

    /// 全部 api_path 以 `/` 开头（与 build_url 拼接逻辑一致）。
    #[test]
    fn api_paths_start_with_slash() {
        for t in ALL_TOOLS {
            assert!(
                t.api_path.starts_with('/'),
                "api_path 须以 / 开头: {}",
                t.api_path
            );
        }
    }

    /// method 只有 GET/POST 两种；参数名在 tool 内唯一；kind 合法。
    #[test]
    fn methods_and_params_are_wellformed() {
        for t in ALL_TOOLS {
            assert!(
                matches!(t.method, "GET" | "POST"),
                "method 须 GET/POST: {} {}",
                t.name,
                t.method
            );
            let mut names: Vec<&str> = t.params.iter().map(|p| p.name).collect();
            names.sort_unstable();
            let before = names.len();
            names.dedup();
            assert_eq!(names.len(), before, "{} 参数名重复", t.name);
            for p in t.params {
                assert!(
                    matches!(p.kind, "string" | "number"),
                    "{} 参数 {} 非法 kind {}",
                    t.name,
                    p.name,
                    p.kind
                );
            }
        }
    }

    /// 路径模板占位符都有对应参数定义（GET 与 POST 共用校验）。
    #[test]
    fn path_placeholders_have_params() {
        for t in ALL_TOOLS {
            let mut rest = t.api_path;
            while let Some(start) = rest.find('{') {
                let end = start + rest[start..].find('}').expect("模板未闭合");
                let ph = &rest[start + 1..end];
                assert!(
                    t.params.iter().any(|p| p.name == ph),
                    "{} 路径模板 {{{ph}}} 缺参数定义",
                    t.name
                );
                rest = &rest[end + 1..];
            }
        }
    }

    /// 任务要求的核心 tool 全部存在（10 个 OS 一一校验 name）。
    #[test]
    fn required_tools_present() {
        let required = [
            "os_status",
            "os_pool_list",
            "os_dataset_list",
            "os_snapshot_list",
            "os_vm_list",
            "os_share_list",
            "os_user_list",
            "os_node_list",
            "os_virt_check",
            "os_health",
        ];
        for name in required {
            assert!(find_tool(name).is_some(), "缺少必需 tool: {name}");
        }
    }

    /// find_tool 对未知 name 返回 None。
    #[test]
    fn find_tool_returns_none_for_unknown() {
        assert!(find_tool("nonexistent_tool").is_none());
    }

    /// build_url 拼接正确（去末尾 / + 加 api_path）。
    #[test]
    fn build_url_trims_trailing_slash() {
        let tool = find_tool("os_pool_list").unwrap();
        assert_eq!(
            build_url("http://127.0.0.1:8080", tool),
            "http://127.0.0.1:8080/api/v1/pools"
        );
        assert_eq!(
            build_url("http://127.0.0.1:8080/", tool),
            "http://127.0.0.1:8080/api/v1/pools"
        );
        assert_eq!(
            build_url("http://127.0.0.1:8080///", tool),
            "http://127.0.0.1:8080/api/v1/pools"
        );
    }

    /// build_url 对每个 tool 都能生成含 api_path 的 URL（全覆盖冒烟）。
    #[test]
    fn build_url_covers_all_tools() {
        let base = "http://127.0.0.1:8080";
        for t in ALL_TOOLS {
            let url = build_url(base, t);
            assert!(
                url.ends_with(t.api_path),
                "URL {url} 应以 {} 结尾",
                t.api_path
            );
            assert!(url.starts_with(base), "URL {url} 应以 {base} 开头");
        }
    }

    /// OsTool 可序列化为 JSON（含 name + description + api_path 等字段）。
    #[test]
    fn tool_serializes_to_json() {
        let t = find_tool("os_health").unwrap();
        let v = serde_json::to_value(t).unwrap();
        assert_eq!(v["name"], "os_health");
        assert_eq!(v["api_path"], "/healthz");
        assert_eq!(v["method"], "GET");
        assert!(v["description"].as_str().unwrap().contains("健康检查"));
        // NexHub 写工具带 method=POST + 参数
        let t = find_tool("nexhub_read_file").unwrap();
        let v = serde_json::to_value(t).unwrap();
        assert_eq!(v["method"], "GET");
        let params = v["params"].as_array().unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0]["name"], "repo");
        assert_eq!(params[1]["name"], "path");
    }

    /// 写工具（POST + mcp_mark）与读工具（GET）的分野符合任务要求：
    /// 只读多用公开面，写操作带 mcp 来源标记。
    #[test]
    fn write_tools_are_post_and_marked() {
        let write_tools: Vec<&OsTool> = ALL_TOOLS
            .iter()
            .filter(|t| t.method == "POST")
            .collect();
        assert_eq!(write_tools.len(), 4, "4 个写工具（write_file/issue/pr/merge）");
        for t in &write_tools {
            match t.name {
                "nexhub_write_file" => assert!(!t.mcp_mark, "git author 服务端标记"),
                "nexhub_merge_pr" => assert!(!t.mcp_mark, "merge 无内容通道"),
                _ => assert!(t.mcp_mark, "{} 应带 mcp 来源标记", t.name),
            }
        }
        // OS 管理面 10 个全部无参 GET
        assert!(
            ALL_TOOLS
                .iter()
                .take(10)
                .all(|t| t.method == "GET" && t.params.is_empty())
        );
    }
}
