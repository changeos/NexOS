//! NFS 协议（编排）
//!
//! 实现说明（规划文档 §3.3）：
//! - NFSv3：编排 nfsserve（Rust 实现）——导出表用经典 `exports(5)` 格式
//! - NFSv4：编排 nfs-ganesha——生成 `ganesha.conf` 的 `EXPORT { ... }` 块
//!
//! 本文件补充配置生成（纯函数）：
//! - `NfsExportsEntry::render`：单条 `/etc/exports` 行（NFSv3）
//! - `render_exports`：完整 `/etc/exports` 文本
//! - `GaneshaExport::render`：单个 ganesha `EXPORT {}` 块
//! - `GaneshaConfig::render`：完整 `ganesha.conf` 文本

use std::path::PathBuf;

use os_core::{Deserialize, Serialize, ShareId};

use crate::common::{FileProtocol, Share};
use crate::ProtocolResult;

/// NFS export 选项（对应 exports 文件 / ganesha EXPORT 块的字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfsExportOptions {
    /// 读写 / 只读
    pub read_write: bool,
    /// 同步 / 异步写（sync 更安全，async 更快）
    pub sync: bool,
    /// 是否禁用 root_squash（允许客户端 root 以服务端 root 身份操作；高危）
    pub no_root_squash: bool,
    /// 安全级别（sec=sys / sec=krb5 / sec=krb5i / sec=krb5p）
    #[serde(default = "default_sec")]
    pub sec: String,
}

fn default_sec() -> String {
    "sys".to_string()
}

impl Default for NfsExportOptions {
    fn default() -> Self {
        Self {
            read_write: true,
            sync: true,
            no_root_squash: false,
            sec: default_sec(),
        }
    }
}

// ----------------------------------------------------------------------------
// NFSv3：/etc/exports 渲染（编排 nfsserve / 经典 nfsd）
// ----------------------------------------------------------------------------

/// 一条 `/etc/exports` 导出项（一条共享对一个客户端组的导出）。
///
/// 格式（exports(5)）：`<目录> <客户端>(<选项>) [<客户端>(<选项>) ...]`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfsExportsEntry {
    /// 导出目录（数据集绝对路径，如 `/tank/media`）
    pub path: PathBuf,
    /// 客户端导出项列表（客户端 + 该客户端的选项）
    pub clients: Vec<NfsClientExport>,
}

/// 单个客户端的导出选项项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfsClientExport {
    /// 客户端说明（主机名 / IP / CIDR / `*`）
    pub client: String,
    /// 导出选项
    pub options: NfsExportOptions,
}

impl NfsClientExport {
    /// 渲染该客户端的选项括号段（不含客户端名），如 `rw,sync,root_squash`。
    #[must_use]
    pub fn render_options(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        parts.push(if self.options.read_write { "rw" } else { "ro" });
        parts.push(if self.options.sync { "sync" } else { "async" });
        parts.push(if self.options.no_root_squash {
            "no_root_squash"
        } else {
            "root_squash"
        });
        // sec 选项仅在非默认 sys 时输出（sys 是隐含默认，写出来虽合法但冗余）
        if self.options.sec != "sys" {
            match self.options.sec.as_str() {
                "krb5" | "krb5i" | "krb5p" => parts.push(match self.options.sec.as_str() {
                    "krb5" => "sec=krb5",
                    "krb5i" => "sec=krb5i",
                    _ => "sec=krb5p",
                }),
                _ => {}
            }
        }
        parts.join(",")
    }
}

impl NfsExportsEntry {
    /// 渲染成一条 `/etc/exports` 行（含尾部换行）。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{} ", self.path.display());
        let rendered: Vec<String> = self
            .clients
            .iter()
            .map(|c| format!("{}({})", c.client, c.render_options()))
            .collect();
        out.push_str(&rendered.join(" "));
        out.push('\n');
        out
    }
}

/// 把多条导出项渲染成完整 `/etc/exports` 文本（按入参顺序拼接）。
#[must_use]
pub fn render_exports(entries: &[NfsExportsEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&e.render());
    }
    out
}

// ----------------------------------------------------------------------------
// NFSv4：nfs-ganesha 配置渲染（ganesha.conf 的 EXPORT 块）
// ----------------------------------------------------------------------------

/// nfs-ganesha 全局配置（`NFS_Core_Param` / `NFSv4` 段，极简）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaneshaConfig {
    /// NFSv4 域名（用于 idmap，如 `os.local`）
    pub domain: String,
    /// ganesha.conf 路径（如 `/etc/ganesha/ganesha.conf`）
    pub config_path: PathBuf,
    /// 监听地址（如 `0.0.0.0`；可留空用默认）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bind_addr: String,
}

impl GaneshaConfig {
    /// 一个开箱即用的开发态默认配置。
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            domain: "os.local".into(),
            config_path: PathBuf::from("/etc/ganesha/ganesha.conf"),
            bind_addr: String::new(),
        }
    }

    /// 渲染 ganesha 全局核心参数段（`NFS_Core_Param` / `NFSv4`）。
    #[must_use]
    pub fn render_global(&self) -> String {
        let mut out = String::new();
        out.push_str("NFS_Core_Param {\n");
        if !self.bind_addr.is_empty() {
            out.push_str(&format!("    Bind_Addr = \"{}\";\n", self.bind_addr));
        }
        out.push_str("}\n");
        out.push_str("NFSv4 {\n");
        out.push_str(&format!("    Domain_Name = \"{}\";\n", self.domain));
        out.push_str("    Delegations = true;\n");
        out.push_str("}\n");
        out
    }

    /// 渲染完整 ganesha.conf（全局段 + 各 EXPORT 块）。
    #[must_use]
    pub fn render(&self, exports: &[GaneshaExport]) -> String {
        let mut out = self.render_global();
        for e in exports {
            out.push('\n');
            out.push_str(&e.render());
        }
        out
    }
}

impl Default for GaneshaConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// 单个 ganesha `EXPORT { ... }` 块的渲染规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaneshaExport {
    /// 导出 ID（ganesha `Export_Id`，节点内唯一整数）
    pub export_id: u16,
    /// 导出路径（数据集绝对路径）
    pub path: PathBuf,
    /// NFS 协议版本（3 / 4 / 4.1）——`Protocols` 列表
    pub protocols: Vec<u8>,
    /// 客户端访问控制列表（ganesha `CLIENT` 块）
    pub clients: Vec<GaneshaClient>,
    /// 传输协议（TCP / UDP，ganesha `Transports`）
    #[serde(default = "default_transports")]
    pub transports: Vec<String>,
    /// 是否只读（ganesha `Access_Type`）
    pub read_only: bool,
    /// 是否启用 root_squash（ganesha `Squash`，`Root` / `Rootid` / `No_root_squash`）
    pub squash_root: bool,
    /// 安全级别（sec=sys / krb5 / krb5i / krb5p）
    pub sec: String,
}

fn default_transports() -> Vec<String> {
    vec!["TCP".into()]
}

/// ganesha `CLIENT` 块规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaneshaClient {
    /// 客户端范围（IP / CIDR / `*`）
    pub clients: String,
    /// 该客户端的访问类型（`RW` / `RO` / `MDONLY` / `NONE`）
    pub access_type: GaneshaAccess,
    /// 该客户端的 squash 策略（None = 继承 EXPORT 级）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squash: Option<GaneshaSquash>,
}

/// ganesha `Access_Type` 枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GaneshaAccess {
    /// 读写
    #[serde(rename = "RW")]
    Rw,
    /// 只读
    #[serde(rename = "RO")]
    Ro,
    /// 仅元数据
    #[serde(rename = "MDONLY")]
    Mdonly,
    /// 无访问
    #[serde(rename = "NONE")]
    None,
}

impl GaneshaAccess {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GaneshaAccess::Rw => "RW",
            GaneshaAccess::Ro => "RO",
            GaneshaAccess::Mdonly => "MDONLY",
            GaneshaAccess::None => "NONE",
        }
    }
}

/// ganesha `Squash` 枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GaneshaSquash {
    /// root 映射为匿名（默认最安全）
    #[serde(rename = "root")]
    Root,
    /// root + 所有用户映射为匿名 id
    #[serde(rename = "rootid")]
    Rootid,
    /// 不 squash（root 即 root；高危）
    #[serde(rename = "No_Root_Squash")]
    NoRootSquash,
}

impl GaneshaSquash {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GaneshaSquash::Root => "root",
            GaneshaSquash::Rootid => "rootid",
            GaneshaSquash::NoRootSquash => "No_Root_Squash",
        }
    }
}

impl GaneshaExport {
    /// 渲染单个 `EXPORT { ... }` 块（含尾部换行）。
    ///
    /// 输出顺序遵循 ganesha 文档惯例：`Export_Id / Path / Pseudo / Access_Type /
    /// Squash / SecType / Protocols / Transports / CLIENT { ... }`。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("EXPORT {\n");
        out.push_str(&format!("    Export_Id = {};\n", self.export_id));
        out.push_str(&format!("    Path = \"{}\";\n", self.path.display()));
        // Pseudo 路径（NFSv4 用）：简单复用 path 做伪根，便于客户端挂载
        out.push_str(&format!("    Pseudo = \"{}\";\n", pseudo_of(&self.path)));
        out.push_str(&format!(
            "    Access_Type = {};\n",
            if self.read_only { "RO" } else { "RW" }
        ));
        out.push_str(&format!(
            "    Squash = {};\n",
            if self.squash_root {
                GaneshaSquash::NoRootSquash.as_str()
            } else {
                GaneshaSquash::Root.as_str()
            }
        ));
        out.push_str(&format!("    SecType = {};\n", self.sec));
        // Protocols：3 / 4 / 4.1，去重保序
        let protos: Vec<String> = self.protocols.iter().map(u8::to_string).collect();
        out.push_str(&format!("    Protocols = {};\n", protos.join(",")));
        out.push_str(&format!(
            "    Transports = {};\n",
            self.transports.join(",")
        ));
        // 客户端块
        for c in &self.clients {
            out.push_str("    CLIENT {\n");
            out.push_str(&format!("    Clients = {};\n", c.clients));
            out.push_str(&format!("    Access_Type = {};\n", c.access_type.as_str()));
            if let Some(s) = c.squash {
                out.push_str(&format!("    Squash = {};\n", s.as_str()));
            }
            out.push_str("    }\n");
        }
        out.push_str("}\n");
        out
    }
}

/// 由真实路径推导 ganesha Pseudo 路径：把 `/tank/media` → `/tank/media`
/// （简单复用；若需重命名伪根可在编排器侧覆盖）。
fn pseudo_of(path: &std::path::Path) -> String {
    path.display().to_string()
}

/// NFS 管理器——编排 nfsserve(NFSv3) / nfs-ganesha(NFSv4)。
///
/// 继承 `FileProtocol` 提供共享生命周期/会话管理；本 trait 补充 NFS 特有能力。
#[allow(async_fn_in_trait)]
pub trait NfsManager: FileProtocol {
    /// 为共享添加 NFS export（指定客户端列表与选项）。
    async fn add_export(
        &self,
        share: &ShareId,
        clients: Vec<String>,
        options: NfsExportOptions,
    ) -> ProtocolResult<Share>;

    /// 移除共享的 NFS export。
    async fn remove_export(&self, share: &ShareId) -> ProtocolResult<()>;
}

// —— 单元测试：NFS 配置渲染 ——

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(rw: bool, no_root: bool) -> NfsExportOptions {
        NfsExportOptions {
            read_write: rw,
            sync: true,
            no_root_squash: no_root,
            sec: "sys".into(),
        }
    }

    #[test]
    fn exports_v3_render_single_client_rw() {
        let e = NfsExportsEntry {
            path: PathBuf::from("/tank/media"),
            clients: vec![NfsClientExport {
                client: "10.0.0.0/24".into(),
                options: opts(true, false),
            }],
        };
        let line = e.render();
        assert_eq!(line, "/tank/media 10.0.0.0/24(rw,sync,root_squash)\n");
    }

    #[test]
    fn exports_v3_render_ro_and_no_root_squash() {
        let e = NfsExportsEntry {
            path: PathBuf::from("/tank/ro"),
            clients: vec![NfsClientExport {
                client: "*".into(),
                options: opts(false, true),
            }],
        };
        assert_eq!(e.render(), "/tank/ro *(ro,sync,no_root_squash)\n");
    }

    #[test]
    fn exports_v3_render_multi_clients() {
        let e = NfsExportsEntry {
            path: PathBuf::from("/tank/media"),
            clients: vec![
                NfsClientExport {
                    client: "10.0.0.5".into(),
                    options: opts(true, true),
                },
                NfsClientExport {
                    client: "10.0.0.0/24".into(),
                    options: opts(false, false),
                },
            ],
        };
        let line = e.render();
        assert!(line.contains("10.0.0.5(rw,sync,no_root_squash)"));
        assert!(line.contains("10.0.0.0/24(ro,sync,root_squash)"));
    }

    #[test]
    fn exports_v3_render_sec_krb5() {
        let e = NfsExportsEntry {
            path: PathBuf::from("/tank/secure"),
            clients: vec![NfsClientExport {
                client: "10.0.0.0/24".into(),
                options: NfsExportOptions {
                    read_write: true,
                    sync: true,
                    no_root_squash: false,
                    sec: "krb5p".into(),
                },
            }],
        };
        let line = e.render();
        assert!(line.contains("sec=krb5p"));
    }

    #[test]
    fn render_exports_multi_lines() {
        let entries = vec![
            NfsExportsEntry {
                path: PathBuf::from("/tank/a"),
                clients: vec![NfsClientExport {
                    client: "*".into(),
                    options: opts(true, false),
                }],
            },
            NfsExportsEntry {
                path: PathBuf::from("/tank/b"),
                clients: vec![NfsClientExport {
                    client: "*".into(),
                    options: opts(false, false),
                }],
            },
        ];
        let txt = render_exports(&entries);
        assert_eq!(txt.lines().count(), 2);
        assert!(txt.contains("/tank/a"));
        assert!(txt.contains("/tank/b"));
    }

    #[test]
    fn ganesha_global_render() {
        let g = GaneshaConfig {
            domain: "os.local".into(),
            config_path: PathBuf::from("/etc/ganesha/ganesha.conf"),
            bind_addr: "0.0.0.0".into(),
        };
        let txt = g.render_global();
        assert!(txt.contains("NFS_Core_Param {"));
        assert!(txt.contains("Bind_Addr = \"0.0.0.0\";"));
        assert!(txt.contains("NFSv4 {"));
        assert!(txt.contains("Domain_Name = \"os.local\";"));
    }

    #[test]
    fn ganesha_export_block_render() {
        let e = GaneshaExport {
            export_id: 1,
            path: PathBuf::from("/tank/media"),
            protocols: vec![4],
            clients: vec![GaneshaClient {
                clients: "10.0.0.0/24".into(),
                access_type: GaneshaAccess::Rw,
                squash: Some(GaneshaSquash::Rootid),
            }],
            transports: vec!["TCP".into()],
            read_only: false,
            squash_root: false,
            sec: "sys".into(),
        };
        let block = e.render();
        assert!(block.starts_with("EXPORT {\n"));
        assert!(block.contains("Export_Id = 1;"));
        assert!(block.contains("Path = \"/tank/media\";"));
        assert!(block.contains("Pseudo = \"/tank/media\";"));
        assert!(block.contains("Access_Type = RW;"));
        assert!(block.contains("Squash = root;"));
        assert!(block.contains("Protocols = 4;"));
        assert!(block.contains("Transports = TCP;"));
        assert!(block.contains("CLIENT {"));
        assert!(block.contains("Clients = 10.0.0.0/24;"));
        assert!(block.contains("Access_Type = RW;"));
        assert!(block.contains("Squash = rootid;"));
    }

    #[test]
    fn ganesha_export_read_only_no_squash() {
        let e = GaneshaExport {
            export_id: 2,
            path: PathBuf::from("/tank/ro"),
            protocols: vec![4],
            clients: vec![GaneshaClient {
                clients: "*".into(),
                access_type: GaneshaAccess::Ro,
                squash: None,
            }],
            transports: vec!["TCP".into()],
            read_only: true,
            squash_root: true,
            sec: "sys".into(),
        };
        let block = e.render();
        assert!(block.contains("Access_Type = RO;"));
        assert!(block.contains("Squash = No_Root_Squash;"));
    }

    #[test]
    fn ganesha_full_conf() {
        let g = GaneshaConfig::defaults();
        let exports = vec![GaneshaExport {
            export_id: 1,
            path: PathBuf::from("/tank/media"),
            protocols: vec![4],
            clients: vec![GaneshaClient {
                clients: "*".into(),
                access_type: GaneshaAccess::Rw,
                squash: None,
            }],
            transports: vec!["TCP".into()],
            read_only: false,
            squash_root: false,
            sec: "sys".into(),
        }];
        let conf = g.render(&exports);
        assert!(conf.contains("NFSv4 {"));
        assert!(conf.contains("EXPORT {"));
        // global 在前
        assert!(conf.find("NFSv4").unwrap() < conf.find("EXPORT").unwrap());
    }

    // —— render_options 边界分支补测 ——

    #[test]
    fn render_options_async_writes_async_keyword() {
        // sync=false → "async"（注意 "async," 包含子串 "sync,"，故用精确逗号分隔断言）
        let c = NfsClientExport {
            client: "*".into(),
            options: NfsExportOptions {
                read_write: true,
                sync: false,
                no_root_squash: false,
                sec: "sys".into(),
            },
        };
        let s = c.render_options();
        // 期望 "rw,async,root_squash"：精确断言 async 在中间、不含独立 sync
        assert_eq!(s, "rw,async,root_squash");
    }

    #[test]
    fn render_options_sec_krb5_and_krb5i() {
        // sec=krb5 / sec=krb5i 分支
        for (sec_in, expected) in [("krb5", "sec=krb5"), ("krb5i", "sec=krb5i")] {
            let c = NfsClientExport {
                client: "*".into(),
                options: NfsExportOptions {
                    read_write: true,
                    sync: true,
                    no_root_squash: false,
                    sec: sec_in.into(),
                },
            };
            assert!(c.render_options().contains(expected), "sec={sec_in}");
        }
    }

    #[test]
    fn render_options_invalid_sec_falls_through_silently() {
        // 非 krb5/krb5i/krb5p 的 sec 值 → 不输出 sec= 行（_ => {} 分支）
        let c = NfsClientExport {
            client: "*".into(),
            options: NfsExportOptions {
                read_write: true,
                sync: true,
                no_root_squash: false,
                sec: "bogus".into(),
            },
        };
        let s = c.render_options();
        assert!(!s.contains("sec="));
    }

    #[test]
    fn render_options_sys_sec_omits_sec_line() {
        // sec=sys（默认）→ 不输出 sec= 行（冗余）
        let c = NfsClientExport {
            client: "*".into(),
            options: opts(true, false),
        };
        assert!(!c.render_options().contains("sec="));
    }

    // —— GaneshaAccess / GaneshaSquash 全枚举覆盖 ——

    #[test]
    fn ganesha_access_as_str_all_variants() {
        assert_eq!(GaneshaAccess::Rw.as_str(), "RW");
        assert_eq!(GaneshaAccess::Ro.as_str(), "RO");
        assert_eq!(GaneshaAccess::Mdonly.as_str(), "MDONLY");
        assert_eq!(GaneshaAccess::None.as_str(), "NONE");
    }

    #[test]
    fn ganesha_squash_as_str_all_variants() {
        assert_eq!(GaneshaSquash::Root.as_str(), "root");
        assert_eq!(GaneshaSquash::Rootid.as_str(), "rootid");
        assert_eq!(GaneshaSquash::NoRootSquash.as_str(), "No_Root_Squash");
    }

    #[test]
    fn ganesha_export_with_mdonly_none_access_and_no_clients() {
        // 覆盖：Access_Type = MDONLY/NONE + 客户端块 Squash=None（不输出 Squash 行）+ 空 clients
        let e = GaneshaExport {
            export_id: 3,
            path: PathBuf::from("/tank/m"),
            protocols: vec![3, 4],
            clients: vec![
                GaneshaClient {
                    clients: "10.0.0.1".into(),
                    access_type: GaneshaAccess::Mdonly,
                    squash: None,
                },
                GaneshaClient {
                    clients: "10.0.0.2".into(),
                    access_type: GaneshaAccess::None,
                    squash: Some(GaneshaSquash::NoRootSquash),
                },
            ],
            transports: vec!["TCP".into(), "UDP".into()],
            read_only: false,
            squash_root: false,
            sec: "sys".into(),
        };
        let block = e.render();
        // 多协议
        assert!(block.contains("Protocols = 3,4;"));
        assert!(block.contains("Transports = TCP,UDP;"));
        // 客户端 MDONLY + 无 Squash 行
        assert!(block.contains("Access_Type = MDONLY;"));
        // 客户端 NONE + No_Root_Squash
        assert!(block.contains("Access_Type = NONE;"));
        assert!(block.contains("Squash = No_Root_Squash;"));
    }

    #[test]
    fn ganesha_export_no_clients_emits_empty_client_section() {
        // 空 clients → 不输出 CLIENT 块
        let e = GaneshaExport {
            export_id: 4,
            path: PathBuf::from("/tank/x"),
            protocols: vec![4],
            clients: vec![],
            transports: vec!["TCP".into()],
            read_only: true,
            squash_root: false,
            sec: "sys".into(),
        };
        let block = e.render();
        assert!(!block.contains("CLIENT {"));
        assert!(block.contains("Access_Type = RO;"));
    }

    // —— render_global bind_addr 边界 ——

    #[test]
    fn ganesha_global_render_empty_bind_addr_omits_line() {
        // bind_addr 为空 → 不输出 Bind_Addr 行（默认配置路径）
        let g = GaneshaConfig::defaults();
        assert!(g.bind_addr.is_empty());
        let txt = g.render_global();
        assert!(!txt.contains("Bind_Addr"));
        assert!(txt.contains("Delegations = true;"));
    }

    #[test]
    fn ganesha_full_conf_empty_exports_only_global() {
        let g = GaneshaConfig::defaults();
        let conf = g.render(&[]);
        assert!(conf.contains("NFS_Core_Param"));
        assert!(!conf.contains("EXPORT"));
    }

    // —— render_exports 边界 ——

    #[test]
    fn render_exports_empty_entries_returns_empty_string() {
        assert_eq!(render_exports(&[]), "");
    }

    #[test]
    fn nfs_exports_entry_render_single_client() {
        // 直接测 NfsExportsEntry::render（含尾部换行）
        let e = NfsExportsEntry {
            path: PathBuf::from("/d"),
            clients: vec![NfsClientExport {
                client: "host".into(),
                options: opts(true, false),
            }],
        };
        assert_eq!(e.render(), "/d host(rw,sync,root_squash)\n");
    }

    #[test]
    fn nfs_default_sec_is_sys() {
        // 默认 sec=sys
        assert_eq!(default_sec(), "sys");
        assert_eq!(NfsExportOptions::default().sec, "sys");
        assert!(NfsExportOptions::default().read_write);
        assert!(NfsExportOptions::default().sync);
        assert!(!NfsExportOptions::default().no_root_squash);
    }

    #[test]
    fn default_transports_is_tcp() {
        assert_eq!(default_transports(), vec!["TCP".to_string()]);
    }

    #[test]
    fn pseudo_of_passthrough() {
        // pseudo_of 直接复用路径字符串
        assert_eq!(
            pseudo_of(std::path::Path::new("/tank/media")),
            "/tank/media"
        );
    }
}
