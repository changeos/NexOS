//! MCP tools 注册表——表驱动，把每个 MCP tool 映射到一条 os-api GET 路由。
//!
//! 设计（呼应任务表）：
//! - 每个 [`OsTool`] = `(name, description, api_path)` 三元组；`name` 是 MCP 客户端
//!   调用时传的 `tools/call` 参数（如 `os_pool_list`），`api_path` 是 os-api 网关
//!   的相对路径（如 `/api/v1/pools`），由 [`OsApiClient`](crate::api::OsApiClient)
//!   拼成完整 URL 后 GET。
//! - 10 个 tools 全部是无参只读 GET（查池 / 数据集 / 快照 / VM / 共享 / 用户 /
//!   节点 / 系统状态 / CPU 虚拟化检测 / 健康检查），覆盖 os-api 的核心可观测面。
//! - 表驱动的好处：新增一个 tool 只需在 [`all_tools`] 里加一行；URL 构造与
//!   tool 匹配逻辑共用一份代码，单测覆盖一次即覆盖全部。
//!
//! 注册的 tools（与任务要求一致）：
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

use serde::{Deserialize, Serialize};

/// 一个 MCP tool 的静态描述（name + description + os-api 相对路径）。
///
/// 序列化为 MCP `tools/list` 响应的 `tools[]` 元素子集（`name` + `description`，
/// MCP 还要求 `inputSchema`——由 `jsonrpc` 层在渲染响应时补充为空对象 schema）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OsTool {
    /// MCP tool 名（`tools/call` 参数 `name`，如 `os_pool_list`）。
    /// 全小写 + 下划线，符合 MCP tool 命名约定。
    pub name: &'static str,
    /// 工具的人类可读描述（给 AI 助手看，决定 AI 何时选该 tool）。
    pub description: &'static str,
    /// 对应的 os-api 相对路径（如 `/api/v1/pools`），由 OsApiClient 拼成完整 URL。
    pub api_path: &'static str,
}

/// 全部 MCP tools（表驱动，顺序即 `tools/list` 展示顺序）。
///
/// 用 `&'static` 切片常量：所有字段都是字符串字面量，零分配；`all_tools()` 直接返回引用。
pub const ALL_TOOLS: &[OsTool] = &[
    OsTool {
        name: "os_status",
        description: "查询 OS 系统状态（CPU 虚拟化能力 + 版本 + 进程 uptime）",
        api_path: "/status",
    },
    OsTool {
        name: "os_pool_list",
        description: "列出所有 ZFS 存储池（池名 + 状态 + 容量）",
        api_path: "/api/v1/pools",
    },
    OsTool {
        name: "os_dataset_list",
        description: "列出所有 ZFS 数据集（路径 + 容量 + 快照计数）",
        api_path: "/api/v1/datasets",
    },
    OsTool {
        name: "os_snapshot_list",
        description: "列出所有 ZFS 快照（路径 + 创建时间）",
        api_path: "/api/v1/snapshots",
    },
    OsTool {
        name: "os_vm_list",
        description: "列出所有虚拟机（名称 + 状态 + CPU/内存）",
        api_path: "/api/v1/vms",
    },
    OsTool {
        name: "os_share_list",
        description: "列出所有文件共享（SMB / NFS / WebDAV）",
        api_path: "/shares",
    },
    OsTool {
        name: "os_user_list",
        description: "列出所有用户（用户名 + 角色 + 是否启用）",
        api_path: "/api/v1/users",
    },
    OsTool {
        name: "os_node_list",
        description: "列出集群节点（hostname + 端点 + 能力）",
        api_path: "/discover/nodes",
    },
    OsTool {
        name: "os_virt_check",
        description: "CPU 虚拟化能力详查（VMX/SVM + KVM 可用性 + 综合判定 + 诊断）",
        api_path: "/api/v1/system/virt-check",
    },
    OsTool {
        name: "os_health",
        description: "os-api 健康检查（liveness 探针，返回 {status:ok}）",
        api_path: "/healthz",
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

    /// 至少 10 个 tools（任务要求）；本实现恰好 10 个。
    #[test]
    fn has_at_least_ten_tools() {
        assert!(
            ALL_TOOLS.len() >= 10,
            "至少 10 个 tools，实际 {}",
            ALL_TOOLS.len()
        );
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

    /// 任务要求的核心 tool 全部存在（10 个一一校验 name）。
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

    /// OsTool 可序列化为 JSON（含 name + description + api_path 三字段）。
    #[test]
    fn tool_serializes_to_json() {
        let t = find_tool("os_health").unwrap();
        let v = serde_json::to_value(t).unwrap();
        assert_eq!(v["name"], "os_health");
        assert_eq!(v["api_path"], "/healthz");
        assert!(v["description"].as_str().unwrap().contains("健康检查"));
    }
}
