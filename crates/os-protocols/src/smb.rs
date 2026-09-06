//! SMB 协议（编排 Samba）
//!
//! 实现说明（规划文档 §3.3）：
//! - `write_smb_conf` 生成 smb.conf（基于共享列表渲染 `[share]` 段）
//! - `reload_smbd` 通过 `smbcontrol all reload-config` 热重载
//! - `enable_time_machine` 写入 `vfs objects = fruit streams_xattr` + `fruit:time machine = yes`
//!   （macOS Time Machine 备份目标）

use std::path::PathBuf;

use os_core::{Deserialize, Serialize, ShareId};

use crate::common::{FileProtocol, Share};
use crate::ProtocolResult;

/// Samba 全局配置（`[global]` 段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SambaConfig {
    /// NetBIOS 工作组（如 `WORKGROUP`）
    pub workgroup: String,
    /// SMB 服务监听地址（空 = 全部）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<String>,
    /// 是否允许 guest（全局默认，可被 share 级覆盖）
    pub guest_ok: bool,
    /// 日志级别（0-10）
    pub log_level: u8,
    /// smb.conf 路径（如 `/etc/samba/smb.conf`）
    pub config_path: PathBuf,
}

impl SambaConfig {
    /// 一个开箱即用的开发态默认配置（`WORKGROUP` / 全网卡 / 不允许 guest /
    /// 日志级别 1 / 配置写 `/etc/samba/smb.conf`）。
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            workgroup: "WORKGROUP".into(),
            interfaces: Vec::new(),
            guest_ok: false,
            log_level: 1,
            config_path: PathBuf::from("/etc/samba/smb.conf"),
        }
    }

    /// 渲染 `[global]` 段文本（不含各 `[share]` 段）。
    ///
    /// 对应 Samba `smb.conf` 的全局配置块。`interfaces` 非空时输出
    /// `interfaces = ...` 与 `bind interfaces only = yes`。
    #[must_use]
    pub fn render_global(&self) -> String {
        let mut out = String::new();
        out.push_str("[global]\n");
        out.push_str(&format!("    workgroup = {}\n", self.workgroup));
        if !self.interfaces.is_empty() {
            out.push_str(&format!("    interfaces = {}\n", self.interfaces.join(" ")));
            out.push_str("    bind interfaces only = yes\n");
        }
        out.push_str(&format!("    log level = {}\n", self.log_level));
        // map to guest / guest ok 的全局默认
        let guest = if self.guest_ok { "yes" } else { "no" };
        // 全局 guest 走 Bad User 映射（Samba 推荐写法）
        out.push_str(&format!(
            "    map to guest = {}\n",
            if self.guest_ok { "Bad User" } else { "Never" }
        ));
        out.push_str(&format!("    guest ok = {guest}\n"));
        out
    }
}

impl Default for SambaConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// 单个 SMB 共享的渲染规格（对应 smb.conf 里一个 `[name]` 段）。
///
/// 由 `SambaOrchestrator` 在创建/更新共享时由 `Share` + `ShareOptions` 派生而来，
/// 交由 `render()` 产出段文本，最终拼进完整 smb.conf。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SambaShareSpec {
    /// 共享名（smb.conf 段名，如 `media`，对应 `[media]`）
    pub name: String,
    /// 共享路径（数据集绝对路径，如 `/tank/media`）
    pub path: PathBuf,
    /// 备注/描述（`comment` 行；None 省略）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// 是否在网络邻居可见（`browseable`；默认 yes）
    pub browseable: bool,
    /// 是否只读（`read only`）
    pub read_only: bool,
    /// 是否允许 guest（`guest ok`）
    pub guest_ok: bool,
    /// 允许访问的用户列表（`valid users`；空 = 不限）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub valid_users: Vec<String>,
    /// 允许访问的主机列表（`hosts allow`；空 = 不限）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts_allow: Vec<String>,
    /// 是否启用 macOS Time Machine（写入 `vfs objects = fruit streams_xattr` +
    /// `fruit:time machine = yes`；由 `SmbManager::enable_time_machine` 触发）
    #[serde(default)]
    pub time_machine: bool,
    /// Time Machine 容量上限（GB）；仅 `time_machine=true` 时输出
    /// `fruit:time machine max size = <N>G`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_machine_max_size_gb: Option<u64>,
}

impl SambaShareSpec {
    /// 渲染该共享对应的 smb.conf 段文本（含尾部换行）。
    ///
    /// 字段输出顺序遵循 Samba 官方推荐：`comment / path / browseable / read only /
    /// guest ok / valid users / hosts allow`，Time Machine 段在末尾。
    /// 空集合/None 字段省略，保持生成的配置精简可读。
    #[must_use]
    pub fn render(&self) -> String {
        let yn = |b: bool| if b { "yes" } else { "no" };
        let mut out = String::new();
        out.push_str(&format!("[{}]\n", self.name));
        if let Some(c) = &self.comment {
            out.push_str(&format!("    comment = {c}\n"));
        }
        out.push_str(&format!("    path = {}\n", self.path.display()));
        out.push_str(&format!("    browseable = {}\n", yn(self.browseable)));
        out.push_str(&format!("    read only = {}\n", yn(self.read_only)));
        out.push_str(&format!("    guest ok = {}\n", yn(self.guest_ok)));
        if !self.valid_users.is_empty() {
            out.push_str(&format!(
                "    valid users = {}\n",
                self.valid_users.join(" ")
            ));
        }
        if !self.hosts_allow.is_empty() {
            out.push_str(&format!(
                "    hosts allow = {}\n",
                self.hosts_allow.join(" ")
            ));
        }
        if self.time_machine {
            // macOS Time Machine：启用 fruit/streams_xattr VFS 模块
            // （Samba ≥ 4.8 推荐写法，参考 Samba wiki: Fruit and Streams）
            out.push_str("    vfs objects = fruit streams_xattr\n");
            out.push_str("    fruit:time machine = yes\n");
            if let Some(gb) = self.time_machine_max_size_gb {
                out.push_str(&format!("    fruit:time machine max size = {gb}G\n"));
            }
        }
        out
    }
}

/// 把一组全局配置 + 共享规格渲染成完整 smb.conf 文本。
///
/// `[global]` 段在前，各 `[share]` 段按入参顺序在后。这是 `SmbManager::write_smb_conf`
/// 的核心纯函数部分（写盘由编排器完成）。
#[must_use]
pub fn render_smb_conf(global: &SambaConfig, shares: &[SambaShareSpec]) -> String {
    let mut out = global.render_global();
    for s in shares {
        out.push('\n');
        out.push_str(&s.render());
    }
    out
}

/// SMB 管理器——编排 Samba。
///
/// 继承 `FileProtocol` 提供共享生命周期/会话管理；本 trait 补充 SMB 特有能力。
#[allow(async_fn_in_trait)]
pub trait SmbManager: FileProtocol {
    /// 生成 smb.conf（写盘并返回路径）。
    async fn write_smb_conf(&self) -> ProtocolResult<PathBuf>;

    /// 热重载 smbd 配置（smbcontrol all reload-config）。
    async fn reload_smbd(&self) -> ProtocolResult<()>;

    /// 为指定共享启用 macOS Time Machine 支持（vfs_fruit）。
    ///
    /// `size_limit_gb` 对应 `fruit:time machine max size`（None = 不限）。
    async fn enable_time_machine(
        &self,
        share: &ShareId,
        size_limit_gb: Option<u64>,
    ) -> ProtocolResult<Share>;

    /// 列出当前 SMB 会话（smbstatus 解析）。
    async fn list_smb_sessions(&self) -> ProtocolResult<Vec<crate::common::Session>>;
}

// —— 单元测试：smb.conf 渲染 ——
#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> SambaShareSpec {
        SambaShareSpec {
            name: name.into(),
            path: PathBuf::from("/tank/media"),
            comment: Some("媒体库".into()),
            browseable: true,
            read_only: false,
            guest_ok: false,
            valid_users: vec!["alice".into(), "bob".into()],
            hosts_allow: vec!["10.0.0.0/24".into()],
            time_machine: false,
            time_machine_max_size_gb: None,
        }
    }

    #[test]
    fn global_render_basic() {
        let g = SambaConfig {
            workgroup: "WG".into(),
            interfaces: vec![],
            guest_ok: false,
            log_level: 2,
            config_path: PathBuf::from("/etc/samba/smb.conf"),
        };
        let txt = g.render_global();
        assert!(txt.starts_with("[global]\n"));
        assert!(txt.contains("workgroup = WG"));
        assert!(txt.contains("log level = 2"));
        assert!(txt.contains("map to guest = Never"));
        assert!(txt.contains("guest ok = no"));
        // 无 interfaces 时不输出 bind 指令
        assert!(!txt.contains("bind interfaces only"));
    }

    #[test]
    fn global_render_with_interfaces_and_guest() {
        let g = SambaConfig {
            workgroup: "WG".into(),
            interfaces: vec!["lo".into(), "eth0".into()],
            guest_ok: true,
            log_level: 1,
            config_path: PathBuf::from("/etc/samba/smb.conf"),
        };
        let txt = g.render_global();
        assert!(txt.contains("interfaces = lo eth0"));
        assert!(txt.contains("bind interfaces only = yes"));
        assert!(txt.contains("map to guest = Bad User"));
        assert!(txt.contains("guest ok = yes"));
    }

    #[test]
    fn share_render_full() {
        let s = spec("media");
        let txt = s.render();
        assert!(txt.starts_with("[media]\n"));
        assert!(txt.contains("comment = 媒体库"));
        assert!(txt.contains("path = /tank/media"));
        assert!(txt.contains("browseable = yes"));
        assert!(txt.contains("read only = no"));
        assert!(txt.contains("valid users = alice bob"));
        assert!(txt.contains("hosts allow = 10.0.0.0/24"));
        // 未启用 Time Machine 不输出 vfs objects
        assert!(!txt.contains("vfs objects"));
    }

    #[test]
    fn share_render_minimal_omits_optionals() {
        let s = SambaShareSpec {
            name: "tmp".into(),
            path: PathBuf::from("/tank/tmp"),
            comment: None,
            browseable: false,
            read_only: true,
            guest_ok: true,
            valid_users: vec![],
            hosts_allow: vec![],
            time_machine: false,
            time_machine_max_size_gb: None,
        };
        let txt = s.render();
        assert!(txt.contains("browseable = no"));
        assert!(txt.contains("read only = yes"));
        assert!(txt.contains("guest ok = yes"));
        // 无 comment/valid users/hosts allow 行
        assert!(!txt.contains("comment ="));
        assert!(!txt.contains("valid users"));
        assert!(!txt.contains("hosts allow"));
    }

    #[test]
    fn share_render_time_machine_with_limit() {
        let mut s = spec("backup");
        s.time_machine = true;
        s.time_machine_max_size_gb = Some(500);
        let txt = s.render();
        assert!(txt.contains("vfs objects = fruit streams_xattr"));
        assert!(txt.contains("fruit:time machine = yes"));
        assert!(txt.contains("fruit:time machine max size = 500G"));
    }

    #[test]
    fn share_render_time_machine_without_limit() {
        let mut s = spec("backup");
        s.time_machine = true;
        s.time_machine_max_size_gb = None;
        let txt = s.render();
        assert!(txt.contains("fruit:time machine = yes"));
        assert!(!txt.contains("fruit:time machine max size"));
    }

    #[test]
    fn full_conf_renders_global_then_shares() {
        let g = SambaConfig::defaults();
        let shares = vec![spec("media"), spec("docs")];
        let conf = render_smb_conf(&g, &shares);
        // global 段在前
        let global_pos = conf.find("[global]").unwrap();
        let media_pos = conf.find("[media]").unwrap();
        let docs_pos = conf.find("[docs]").unwrap();
        assert!(global_pos < media_pos);
        assert!(media_pos < docs_pos);
    }

    #[test]
    fn full_conf_empty_shares_only_global() {
        let g = SambaConfig::defaults();
        let conf = render_smb_conf(&g, &[]);
        assert!(conf.contains("[global]"));
        // 无 share 段
        assert_eq!(conf.matches('[').count(), 1);
    }
}
