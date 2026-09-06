//! MountManager 实现——挂载命令构造（Windows net use / Linux davfs2）+ SystemMountManager 骨架。
//!
//! 设计（规划文档 §3.15 桌面独有）：
//! - **命令构造为纯函数**（[`build_net_use_command`] / [`build_davfs2_command`]），
//!   把「挂载目标 → 平台命令字符串/参数」做成可确定性单测的纯逻辑，不实际执行。
//! - `SystemMountManager`：维护内存挂载表（mount_id → MountInfo），`mount` 构造命令后
//!   记录挂载（真实 `net use`/`mount` 执行依赖系统命令，留 TODO；命令构造已可测）。
//! - `make_persistent`：构造持久化配置（Windows 注册表行 / Linux fstab 行），纯函数可测。
//! - `list_available_shares`：复用 os-mobile 的 HTTP 传输（[`os_mobile::HttpTransport`]）
//!   经网关 `GET /shares` 查询远端可挂载共享；未注入传输时回退到本地注入的 shares（向后兼容）。
//!
//! 为什么命令构造独立成函数：net use 与 davfs2 的参数差异是关键路径（红线 §9 谨慎），
//! 抽出后可覆盖各协议×平台组合的单测，避免依赖真实系统命令。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use os_mobile::HttpTransport;

use crate::mount::{MountInfo, MountManager, MountProtocol, MountTarget, RemoteShare};
use crate::DesktopError;

// ----------------------------------------------------------------------------
// 命令构造（纯函数）
// ----------------------------------------------------------------------------

/// Windows `net use` 命令构造结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetUseCommand {
    /// 程序名（`net`）
    pub program: String,
    /// 参数列表（含 `use` 子命令与全部参数）
    pub args: Vec<String>,
}

/// 构造 Windows `net use <drive>: \\<endpoint>\<share> /USER:<user> <password>` 命令。
///
/// - `drive_letter`：如 `"Z:"`；None 时用 `*`（自动分配）。
/// - `user`/`password`：None 时省略（匿名/当前用户）。
///
/// 返回 `NetUseCommand`（含 program + args），便于测试断言或交给 std::process::Command 执行。
/// 注：真实执行 net use 依赖 Windows 系统；本函数仅构造命令字符串。
pub fn build_net_use_command(
    target: &MountTarget,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<NetUseCommand, DesktopError> {
    if target.protocol != MountProtocol::Smb {
        return Err(DesktopError::UnsupportedProtocol(format!(
            "net use 仅支持 SMB，收到 {:?}",
            target.protocol
        )));
    }
    let drive = target.drive_letter.as_deref().unwrap_or("*");
    // UNC 路径：\\<endpoint>\<share>（endpoint 去掉协议前缀与端口）
    let host = strip_endpoint_host(&target.endpoint);
    let unc = format!("\\{}\\{}", host, target.share_path);

    let mut args: Vec<String> = vec!["use".to_string(), drive.to_string(), unc];
    if let Some(u) = user {
        args.push(format!("/USER:{}", u));
    }
    if let Some(p) = password {
        args.push(p.to_string());
    }
    Ok(NetUseCommand {
        program: "net".to_string(),
        args,
    })
}

/// Linux davfs2 挂载命令构造结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Davfs2Command {
    pub program: String,
    pub args: Vec<String>,
}

/// 构造 Linux `mount -t davfs <url> <mount_point>` 命令（WebDAV）。
///
/// - `mount_point`：本地挂载点路径；None 时返回 `UnsupportedProtocol`（davfs2 必须有挂载点）。
/// - URL 由 endpoint + share_path 拼成（如 `https://os:8443/share`）。
pub fn build_davfs2_command(target: &MountTarget) -> Result<Davfs2Command, DesktopError> {
    if target.protocol != MountProtocol::Webdav {
        return Err(DesktopError::UnsupportedProtocol(format!(
            "davfs2 仅支持 WebDAV，收到 {:?}",
            target.protocol
        )));
    }
    let mount_point = target
        .mount_point
        .as_ref()
        .ok_or_else(|| DesktopError::MountFailed("davfs2 需指定 mount_point".into()))?;
    let url = format!(
        "{}/{}",
        target.endpoint.trim_end_matches('/'),
        target.share_path
    );
    Ok(Davfs2Command {
        program: "mount".to_string(),
        args: vec![
            "-t".to_string(),
            "davfs".to_string(),
            url,
            mount_point.to_string_lossy().to_string(),
        ],
    })
}

/// 构造 Linux fstab 持久化行（开机自动挂载）。
///
/// 格式：`<url> <mount_point> davfs defaults,_netdev 0 0`
pub fn build_fstab_line(target: &MountTarget) -> Result<String, DesktopError> {
    if target.protocol != MountProtocol::Webdav {
        return Err(DesktopError::UnsupportedProtocol(
            "fstab 行仅支持 WebDAV/davfs2".into(),
        ));
    }
    let mount_point = target
        .mount_point
        .as_ref()
        .ok_or_else(|| DesktopError::MountFailed("需指定 mount_point".into()))?;
    let url = format!(
        "{}/{}",
        target.endpoint.trim_end_matches('/'),
        target.share_path
    );
    Ok(format!(
        "{} {} davfs defaults,_netdev 0 0",
        url,
        mount_point.display()
    ))
}

/// 从 endpoint 抽取主机名（去 scheme 与端口），用于 UNC 路径。
fn strip_endpoint_host(endpoint: &str) -> String {
    let no_scheme = endpoint.split("://").nth(1).unwrap_or(endpoint);
    // 去端口
    no_scheme.split(':').next().unwrap_or(no_scheme).to_string()
}

// ----------------------------------------------------------------------------
// SystemMountManager（内存挂载表骨架）
// ----------------------------------------------------------------------------

/// 系统挂载管理器——维护内存挂载表，命令构造由纯函数提供（真实执行留 TODO）。
///
/// 真实 `net use`/`mount` 执行依赖系统命令与权限；本骨架聚焦挂载表状态机与命令构造，
/// 便于单测。集成时把构造出的命令交给 `std::process::Command` 执行（待桌面运行时接入）。
///
/// `list_available_shares`：若注入了 HTTP 传输（[`Self::with_transport`]），经网关
/// `GET /shares` 真实查询；否则回退到本地注入的 shares（向后兼容，便于无 HTTP 的测试）。
pub struct SystemMountManager {
    /// mount_id → 挂载信息
    mounts: Mutex<HashMap<String, MountInfo>>,
    next_id: Mutex<u64>,
    /// 可用共享缓存（list_available_shares 回退返回；可注入测试）
    shares: Mutex<Vec<RemoteShare>>,
    /// 可选 HTTP 传输（注入后 list_available_shares 经网关真实查询）
    transport: Mutex<Option<Arc<dyn HttpTransport>>>,
}

impl SystemMountManager {
    /// 创建空管理器。
    pub fn new() -> Self {
        Self {
            mounts: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
            shares: Mutex::new(Vec::new()),
            transport: Mutex::new(None),
        }
    }

    /// 注入可用共享列表（测试/初始化用）。
    pub fn with_shares(self, shares: Vec<RemoteShare>) -> Self {
        *self.shares.lock().unwrap() = shares;
        self
    }

    /// 注入 HTTP 传输——`list_available_shares` 将经网关 `GET /shares` 真实查询。
    ///
    /// 传入后 `with_shares` 的本地缓存仅作为传输失败时的回退。生产环境用
    /// [`os_mobile::ReqwestTransport`]（reqwest + rustls，ADR-DEPS-001）。
    #[must_use]
    pub fn with_transport(self, transport: Arc<dyn HttpTransport>) -> Self {
        *self.transport.lock().unwrap() = Some(transport);
        self
    }

    /// 取下一个 mount_id（自增字符串）。
    fn next_mount_id(&self) -> String {
        let mut id = self.next_id.lock().unwrap();
        let s = format!("mnt-{}", *id);
        *id += 1;
        s
    }

    /// 已挂载数量。
    pub fn mount_count(&self) -> usize {
        self.mounts.lock().unwrap().len()
    }
}

impl Default for SystemMountManager {
    fn default() -> Self {
        Self::new()
    }
}

// MountManager trait 为原生 async（非 #[async_trait]），故 impl 用原生 async fn。
impl MountManager for SystemMountManager {
    async fn list_available_shares(
        &self,
        endpoint: &str,
    ) -> Result<Vec<RemoteShare>, DesktopError> {
        // 真实路径：若注入了 HTTP 传输，经网关 GET /shares 查询（reqwest + rustls）。
        let transport_opt = self.transport.lock().unwrap().clone();
        if let Some(transport) = transport_opt {
            // 构造 GET /shares 请求（相对网关根）。
            let req = os_mobile::http::RequestSpec::get("/shares");
            let resp = transport.send(endpoint, &req).await.map_err(|e| {
                DesktopError::Internal(format!("list_available_shares HTTP 失败: {}", e.message))
            })?;
            let shares: Vec<RemoteShare> = os_mobile::http::parse_json_response(&resp)
                .map_err(|e| DesktopError::Internal(format!("shares 响应解析失败: {e}")))?;
            return Ok(shares);
        }
        // 回退：返回本地注入的 shares（便于无 HTTP 的单测，向后兼容）。
        Ok(self.shares.lock().unwrap().clone())
    }

    async fn mount(&self, target: MountTarget) -> Result<MountInfo, DesktopError> {
        // 构造命令（校验协议 + 参数完整性）；真实执行留 TODO。
        match target.protocol {
            MountProtocol::Smb => {
                let _cmd = build_net_use_command(&target, None, None)?;
            }
            MountProtocol::Webdav => {
                let _cmd = build_davfs2_command(&target)?;
            }
        }
        // TODO(std::process): 执行构造出的命令；当前仅记录挂载。
        let mount_id = self.next_mount_id();
        let mount_path = target.drive_letter.clone().or_else(|| {
            target
                .mount_point
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
        });
        let info = MountInfo {
            target,
            mounted: true,
            mount_path,
            persistent: false,
        };
        self.mounts.lock().unwrap().insert(mount_id, info.clone());
        Ok(info)
    }

    async fn unmount(&self, mount_id: &str) -> Result<(), DesktopError> {
        let mut mounts = self.mounts.lock().unwrap();
        match mounts.get_mut(mount_id) {
            Some(info) => {
                // TODO(std::process): 真实 net use <drive> /delete 或 umount
                info.mounted = false;
                info.mount_path = None;
                Ok(())
            }
            None => Err(DesktopError::UnmountFailed(format!(
                "挂载不存在: {}",
                mount_id
            ))),
        }
    }

    async fn list_mounts(&self) -> Result<Vec<MountInfo>, DesktopError> {
        let mounts = self.mounts.lock().unwrap();
        let mut v: Vec<MountInfo> = mounts.values().cloned().collect();
        // 按 mount_path 排序保证确定性（测试断言稳定）
        v.sort_by(|a, b| {
            a.mount_path
                .cmp(&b.mount_path)
                .then_with(|| a.target.share_path.cmp(&b.target.share_path))
        });
        Ok(v)
    }

    async fn make_persistent(&self, mount_id: &str) -> Result<(), DesktopError> {
        let mut mounts = self.mounts.lock().unwrap();
        let info = mounts
            .get_mut(mount_id)
            .ok_or_else(|| DesktopError::MountFailed(format!("挂载不存在: {}", mount_id)))?;
        // 构造持久化配置（验证可行性）；真实写 fstab/注册表留 TODO。
        match info.target.protocol {
            MountProtocol::Webdav => {
                let _line = build_fstab_line(&info.target)?;
            }
            MountProtocol::Smb => {
                // Windows 注册表持久化：检查 drive_letter 存在
                if info.target.drive_letter.is_none() {
                    return Err(DesktopError::MountFailed(
                        "SMB 持久化需 drive_letter".into(),
                    ));
                }
            }
        }
        info.persistent = true;
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 单元测——命令构造（各协议×平台组合）+ 挂载表状态机
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::{MountProtocol, MountTarget};
    use std::path::PathBuf;

    fn smb_target() -> MountTarget {
        MountTarget {
            endpoint: "https://os:8443".to_string(),
            share_path: "photos".to_string(),
            protocol: MountProtocol::Smb,
            drive_letter: Some("Z:".to_string()),
            mount_point: None,
        }
    }

    fn webdav_target() -> MountTarget {
        MountTarget {
            endpoint: "https://os:8443".to_string(),
            share_path: "backup".to_string(),
            protocol: MountProtocol::Webdav,
            drive_letter: None,
            mount_point: Some(PathBuf::from("/mnt/os")),
        }
    }

    // —— 命令构造 ——

    #[test]
    fn net_use_basic_no_credentials() {
        let cmd = build_net_use_command(&smb_target(), None, None).unwrap();
        assert_eq!(cmd.program, "net");
        assert_eq!(cmd.args, vec!["use", "Z:", "\\os\\photos"]);
    }

    #[test]
    fn net_use_with_credentials() {
        let cmd = build_net_use_command(&smb_target(), Some("admin"), Some("secret")).unwrap();
        assert_eq!(
            cmd.args,
            vec!["use", "Z:", "\\os\\photos", "/USER:admin", "secret"]
        );
    }

    #[test]
    fn net_use_auto_drive_letter() {
        let mut t = smb_target();
        t.drive_letter = None;
        let cmd = build_net_use_command(&t, None, None).unwrap();
        assert_eq!(cmd.args[1], "*");
    }

    #[test]
    fn net_use_strips_scheme_and_port_from_endpoint() {
        let mut t = smb_target();
        t.endpoint = "https://os.example.com:8443".to_string();
        let cmd = build_net_use_command(&t, None, None).unwrap();
        assert_eq!(cmd.args[2], "\\os.example.com\\photos");
    }

    #[test]
    fn net_use_rejects_webdav() {
        let err = build_net_use_command(&webdav_target(), None, None).unwrap_err();
        assert!(matches!(err, DesktopError::UnsupportedProtocol(_)));
    }

    #[test]
    fn davfs2_basic() {
        let cmd = build_davfs2_command(&webdav_target()).unwrap();
        assert_eq!(cmd.program, "mount");
        assert_eq!(
            cmd.args,
            vec![
                "-t".to_string(),
                "davfs".to_string(),
                "https://os:8443/backup".to_string(),
                "/mnt/os".to_string()
            ]
        );
    }

    #[test]
    fn davfs2_requires_mount_point() {
        let mut t = webdav_target();
        t.mount_point = None;
        let err = build_davfs2_command(&t).unwrap_err();
        assert!(matches!(err, DesktopError::MountFailed(_)));
    }

    #[test]
    fn davfs2_rejects_smb() {
        let err = build_davfs2_command(&smb_target()).unwrap_err();
        assert!(matches!(err, DesktopError::UnsupportedProtocol(_)));
    }

    // —— fstab 持久化行 ——

    #[test]
    fn fstab_line_webdav() {
        let line = build_fstab_line(&webdav_target()).unwrap();
        assert!(line.contains("https://os:8443/backup"));
        assert!(line.contains("/mnt/os"));
        assert!(line.contains("davfs"));
        assert!(line.contains("_netdev"));
    }

    #[test]
    fn fstab_line_rejects_smb() {
        let err = build_fstab_line(&smb_target()).unwrap_err();
        assert!(matches!(err, DesktopError::UnsupportedProtocol(_)));
    }

    // —— SystemMountManager 状态机 ——

    #[tokio::test]
    async fn list_available_shares_returns_injected() {
        let mgr = SystemMountManager::new().with_shares(vec![
            RemoteShare {
                name: "photos".into(),
                protocol: MountProtocol::Smb,
                description: None,
            },
            RemoteShare {
                name: "backup".into(),
                protocol: MountProtocol::Webdav,
                description: Some("备份".into()),
            },
        ]);
        let shares = mgr.list_available_shares("os").await.unwrap();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].name, "photos");
    }

    #[tokio::test]
    async fn mount_smb_records_in_table() {
        let mgr = SystemMountManager::new();
        let info = mgr.mount(smb_target()).await.unwrap();
        assert!(info.mounted);
        assert_eq!(info.mount_path.as_deref(), Some("Z:"));
        assert_eq!(mgr.mount_count(), 1);
    }

    #[tokio::test]
    async fn mount_webdav_records_mount_point_path() {
        let mgr = SystemMountManager::new();
        let info = mgr.mount(webdav_target()).await.unwrap();
        assert_eq!(info.mount_path.as_deref(), Some("/mnt/os"));
    }

    #[tokio::test]
    async fn unmount_marks_unmounted() {
        let mgr = SystemMountManager::new();
        mgr.mount(smb_target()).await.unwrap();
        let id_guess = {
            let map = mgr.mounts.lock().unwrap();
            map.keys().next().cloned().unwrap()
        };
        mgr.unmount(&id_guess).await.unwrap();
        let mounts = mgr.list_mounts().await.unwrap();
        assert!(!mounts[0].mounted);
        assert!(mounts[0].mount_path.is_none());
    }

    #[tokio::test]
    async fn unmount_unknown_id_errors() {
        let mgr = SystemMountManager::new();
        assert!(mgr.unmount("nope").await.is_err());
    }

    #[tokio::test]
    async fn make_persistent_webdav_sets_flag() {
        let mgr = SystemMountManager::new();
        mgr.mount(webdav_target()).await.unwrap();
        let id = mgr.mounts.lock().unwrap().keys().next().cloned().unwrap();
        mgr.make_persistent(&id).await.unwrap();
        let mounts = mgr.list_mounts().await.unwrap();
        assert!(mounts[0].persistent);
    }

    #[tokio::test]
    async fn make_persistent_smb_needs_drive_letter() {
        let mgr = SystemMountManager::new();
        let mut t = smb_target();
        t.drive_letter = None;
        // drive_letter=None 时 net use 用 *，仍可挂载记录
        mgr.mount(t.clone()).await.unwrap();
        let id = mgr.mounts.lock().unwrap().keys().next().cloned().unwrap();
        // SMB 无 drive_letter → 持久化失败
        let err = mgr.make_persistent(&id).await.unwrap_err();
        assert!(matches!(err, DesktopError::MountFailed(_)));
    }

    // —— 扩展边界（覆盖率补测）——

    #[test]
    fn net_use_command_struct_debug_eq_clone() {
        let cmd = NetUseCommand {
            program: "net".into(),
            args: vec!["use".into(), "Z:".into()],
        };
        let cmd2 = cmd.clone();
        assert_eq!(cmd, cmd2);
        let _dbg = format!("{:?}", cmd);
    }

    #[test]
    fn davfs2_command_struct_debug_eq_clone() {
        let cmd = Davfs2Command {
            program: "mount".into(),
            args: vec!["-t".into(), "davfs".into()],
        };
        let cmd2 = cmd.clone();
        assert_eq!(cmd, cmd2);
        let _dbg = format!("{:?}", cmd);
    }

    #[test]
    fn strip_endpoint_host_variants() {
        // 直接测内部 host 提取：scheme://host:port → host
        // 通过 net use 间接覆盖各分支
        let cases = [
            ("https://os:8443", "os"),
            ("https://os.example.com:8443", "os.example.com"),
            ("os", "os"),                    // 无 scheme
            ("os:8443", "os"),               // 无 scheme 但有端口
            ("http://10.0.0.5", "10.0.0.5"), // 无端口
            ("", ""),                        // 空
        ];
        for (endpoint, expected_host) in cases {
            let mut t = smb_target();
            t.endpoint = endpoint.into();
            let cmd = build_net_use_command(&t, None, None).unwrap();
            // args[2] 形如 \<host>\<share>
            assert!(
                cmd.args[2].contains(&format!("\\{expected_host}\\")),
                "endpoint {endpoint} → host 应为 {expected_host}，实际 {}",
                cmd.args[2]
            );
        }
    }

    #[test]
    fn net_use_with_user_only_no_password() {
        // 只给 user，不给 password
        let cmd = build_net_use_command(&smb_target(), Some("admin"), None).unwrap();
        assert_eq!(cmd.args, vec!["use", "Z:", "\\os\\photos", "/USER:admin"]);
    }

    #[test]
    fn net_use_with_password_only_no_user() {
        // 只给 password，不给 user（参数仍会 append password）
        let cmd = build_net_use_command(&smb_target(), None, Some("secret")).unwrap();
        assert_eq!(cmd.args, vec!["use", "Z:", "\\os\\photos", "secret"]);
    }

    #[test]
    fn net_use_user_with_special_chars() {
        // 用户名含特殊字符（域名\用户）
        let cmd = build_net_use_command(&smb_target(), Some("DOMAIN\\admin"), None).unwrap();
        assert!(cmd.args.contains(&"/USER:DOMAIN\\admin".to_string()));
    }

    #[test]
    fn davfs2_url_trims_trailing_slash_in_endpoint() {
        let mut t = webdav_target();
        t.endpoint = "https://os:8443/".into(); // 尾部斜杠
        let cmd = build_davfs2_command(&t).unwrap();
        // URL 不应有双斜杠
        assert_eq!(cmd.args[2], "https://os:8443/backup");
    }

    #[test]
    fn fstab_line_requires_mount_point() {
        // fstab 行需要 mount_point；缺则 MountFailed
        let mut t = webdav_target();
        t.mount_point = None;
        let err = build_fstab_line(&t).unwrap_err();
        assert!(matches!(err, DesktopError::MountFailed(_)));
    }

    #[test]
    fn fstab_line_format_full() {
        let line = build_fstab_line(&webdav_target()).unwrap();
        // 完整格式：URL mount_point davfs defaults,_netdev 0 0
        assert_eq!(
            line,
            "https://os:8443/backup /mnt/os davfs defaults,_netdev 0 0"
        );
    }

    #[tokio::test]
    async fn system_mount_manager_default_eq_new() {
        let m1 = SystemMountManager::default();
        let m2 = SystemMountManager::new();
        assert_eq!(m1.mount_count(), m2.mount_count());
        assert_eq!(m1.mount_count(), 0);
    }

    #[tokio::test]
    async fn mount_assigns_incrementing_ids() {
        // 连续挂载：mount_id 自增（mnt-1, mnt-2, ...）
        let mgr = SystemMountManager::new();
        mgr.mount(smb_target()).await.unwrap();
        mgr.mount(webdav_target()).await.unwrap();
        let mounts = mgr.list_mounts().await.unwrap();
        assert_eq!(mounts.len(), 2);
        let ids: Vec<_> = mgr.mounts.lock().unwrap().keys().cloned().collect();
        assert!(ids.iter().any(|id| id == "mnt-1"));
        assert!(ids.iter().any(|id| id == "mnt-2"));
    }

    #[tokio::test]
    async fn list_mounts_returns_empty_initially() {
        let mgr = SystemMountManager::new();
        let mounts = mgr.list_mounts().await.unwrap();
        assert!(mounts.is_empty());
    }

    #[tokio::test]
    async fn list_mounts_sorted_by_mount_path() {
        // 多挂载：list_mounts 按 mount_path 排序保证确定性
        let mgr = SystemMountManager::new();
        // 用不同的 drive_letter 制造不同 mount_path
        let mut t_a = smb_target();
        t_a.drive_letter = Some("Z:".into());
        let mut t_b = smb_target();
        t_b.drive_letter = Some("Y:".into());
        let mut t_c = smb_target();
        t_c.drive_letter = Some("X:".into());
        mgr.mount(t_a).await.unwrap();
        mgr.mount(t_b).await.unwrap();
        mgr.mount(t_c).await.unwrap();
        let mounts = mgr.list_mounts().await.unwrap();
        // 字典序：X: < Y: < Z:
        assert_eq!(mounts[0].mount_path.as_deref(), Some("X:"));
        assert_eq!(mounts[1].mount_path.as_deref(), Some("Y:"));
        assert_eq!(mounts[2].mount_path.as_deref(), Some("Z:"));
    }

    #[tokio::test]
    async fn mount_smb_auto_drive_letter_uses_star_in_path() {
        // drive_letter=None → mount_path 为 None（SMB 无 letter 也无 mount_point）
        let mut t = smb_target();
        t.drive_letter = None;
        let mgr = SystemMountManager::new();
        let info = mgr.mount(t).await.unwrap();
        assert!(info.mount_path.is_none());
        assert!(info.mounted);
    }

    #[tokio::test]
    async fn mount_id_format_is_mnt_prefix() {
        let mgr = SystemMountManager::new();
        mgr.mount(smb_target()).await.unwrap();
        let ids: Vec<_> = mgr.mounts.lock().unwrap().keys().cloned().collect();
        assert_eq!(ids.len(), 1);
        assert!(ids[0].starts_with("mnt-"));
    }

    #[tokio::test]
    async fn unmount_then_remount_new_id() {
        // 卸载后重新挂载 → 新 mount_id（自增不回收）
        // 注：unmount 只改状态（mounted=false），不删表项；重新 mount 是新增表项。
        let mgr = SystemMountManager::new();
        mgr.mount(smb_target()).await.unwrap();
        let id1: String = mgr.mounts.lock().unwrap().keys().next().cloned().unwrap();
        assert_eq!(id1, "mnt-1");
        mgr.unmount(&id1).await.unwrap();
        // 再挂载 → 新 id（mnt-2）
        mgr.mount(smb_target()).await.unwrap();
        let ids: Vec<_> = mgr.mounts.lock().unwrap().keys().cloned().collect();
        // unmount 不删表项 + 重新 mount 新增 → 共 2 项
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"mnt-1".to_string()));
        assert!(ids.contains(&"mnt-2".to_string()));
        // id1 状态变为 unmounted
        let m1 = mgr.mounts.lock().unwrap();
        let info1 = m1.get("mnt-1").unwrap();
        assert!(!info1.mounted);
        let info2 = m1.get("mnt-2").unwrap();
        assert!(info2.mounted);
    }

    #[tokio::test]
    async fn make_persistent_unknown_id_errors() {
        let mgr = SystemMountManager::new();
        let err = mgr.make_persistent("nonexistent").await.unwrap_err();
        assert!(matches!(err, DesktopError::MountFailed(_)));
    }

    #[tokio::test]
    async fn make_persistent_webdav_twice_idempotent() {
        // 重复 make_persistent 不出错（标志位幂等）
        let mgr = SystemMountManager::new();
        mgr.mount(webdav_target()).await.unwrap();
        let id = mgr.mounts.lock().unwrap().keys().next().cloned().unwrap();
        mgr.make_persistent(&id).await.unwrap();
        mgr.make_persistent(&id).await.unwrap();
        let mounts = mgr.list_mounts().await.unwrap();
        assert!(mounts[0].persistent);
    }
}

// ----------------------------------------------------------------------------
// HTTP 路径测——list_available_shares 经 os-mobile HttpTransport（reqwest）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod http_tests {
    use super::*;
    use async_trait::async_trait;
    use os_mobile::http::{JsonResponse, RequestSpec};
    use os_mobile::transport::{TransportError, TransportResult};
    use os_mobile::{HttpTransport, RetryableError};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 离线 FakeTransport：回放预设 JSON 响应，记录观察到的 (base, path, method)。
    struct FakeTransport {
        resp: JsonResponse,
        observed: Mutex<Vec<(String, String, &'static str)>>,
    }

    #[async_trait]
    impl HttpTransport for FakeTransport {
        async fn send(&self, base_url: &str, req: &RequestSpec) -> TransportResult {
            self.observed.lock().unwrap().push((
                base_url.to_string(),
                req.path.clone(),
                req.method.as_str(),
            ));
            Ok(self.resp.clone())
        }
    }

    /// 失败型 FakeTransport：总是返回 404，验证错误映射。
    struct FailTransport;

    #[async_trait]
    impl HttpTransport for FailTransport {
        async fn send(&self, _base: &str, _req: &RequestSpec) -> TransportResult {
            Err(TransportError::new(
                "not found",
                RetryableError::ClientStatus(404),
            ))
        }
    }

    #[tokio::test]
    async fn list_available_shares_via_http_parses_response() {
        let body = r#"[{"name":"photos","protocol":"smb","description":null},{"name":"backup","protocol":"webdav","description":"备份"}]"#;
        let transport: Arc<dyn HttpTransport> = Arc::new(FakeTransport {
            resp: JsonResponse::new(200, body.as_bytes().to_vec()),
            observed: Mutex::new(Vec::new()),
        });
        let mgr = SystemMountManager::new().with_transport(transport);
        let shares = mgr.list_available_shares("https://os:8443").await.unwrap();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].name, "photos");
        assert_eq!(shares[0].protocol, MountProtocol::Smb);
        assert_eq!(shares[1].name, "backup");
        assert_eq!(shares[1].protocol, MountProtocol::Webdav);
        assert_eq!(shares[1].description.as_deref(), Some("备份"));
    }

    #[tokio::test]
    async fn list_available_shares_http_error_maps_to_internal() {
        let transport: Arc<dyn HttpTransport> = Arc::new(FailTransport);
        let mgr = SystemMountManager::new().with_transport(transport);
        let err = mgr.list_available_shares("https://os").await.unwrap_err();
        assert!(matches!(err, DesktopError::Internal(_)));
    }

    #[tokio::test]
    async fn list_available_shares_falls_back_when_no_transport() {
        // 未注入 transport → 回退到本地注入的 shares（向后兼容）
        let mgr = SystemMountManager::new().with_shares(vec![RemoteShare {
            name: "photos".into(),
            protocol: MountProtocol::Smb,
            description: None,
        }]);
        let shares = mgr.list_available_shares("https://os").await.unwrap();
        assert_eq!(shares.len(), 1);
    }

    /// 真实 reqwest 经 loopback HTTP：验证 ReqwestTransport 真实发 GET /shares。
    #[tokio::test]
    async fn list_available_shares_real_reqwest_loopback() {
        let body = r#"[{"name":"photos","protocol":"smb","description":null}]"#;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.ok();
        });
        let transport: Arc<dyn HttpTransport> =
            Arc::new(os_mobile::ReqwestTransport::new().unwrap());
        let mgr = SystemMountManager::new().with_transport(transport);
        let shares = mgr.list_available_shares(&base_url).await.unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].name, "photos");
    }

    // —— 扩展边界（覆盖率补测：HTTP 路径细节）——

    #[tokio::test]
    async fn list_available_shares_via_http_invalid_json_maps_to_internal() {
        // HTTP 返回 200 但 body 非 JSON 数组 → 解析失败 → DesktopError::Internal
        let transport: Arc<dyn HttpTransport> = Arc::new(FakeTransport {
            resp: JsonResponse::new(200, b"not json".to_vec()),
            observed: Mutex::new(Vec::new()),
        });
        let mgr = SystemMountManager::new().with_transport(transport);
        let err = mgr.list_available_shares("https://os").await.unwrap_err();
        assert!(matches!(err, DesktopError::Internal(_)));
    }

    #[tokio::test]
    async fn list_available_shares_via_http_empty_array() {
        // HTTP 返回空数组
        let transport: Arc<dyn HttpTransport> = Arc::new(FakeTransport {
            resp: JsonResponse::new(200, b"[]".to_vec()),
            observed: Mutex::new(Vec::new()),
        });
        let mgr = SystemMountManager::new().with_transport(transport);
        let shares = mgr.list_available_shares("https://os").await.unwrap();
        assert!(shares.is_empty());
    }

    #[tokio::test]
    async fn list_available_shares_via_http_wrong_shape_array() {
        // 返回 JSON 数组但元素缺字段 → 解析失败
        let body = r#"[{"name":"photos"}]"#; // 缺 protocol
        let transport: Arc<dyn HttpTransport> = Arc::new(FakeTransport {
            resp: JsonResponse::new(200, body.as_bytes().to_vec()),
            observed: Mutex::new(Vec::new()),
        });
        let mgr = SystemMountManager::new().with_transport(transport);
        let err = mgr.list_available_shares("https://os").await.unwrap_err();
        assert!(matches!(err, DesktopError::Internal(_)));
    }

    #[tokio::test]
    async fn list_available_shares_via_http_returns_object_not_array() {
        // 返回 JSON 对象而非数组 → 反序列化为 Vec 失败
        let body = r#"{"error":"oops"}"#;
        let transport: Arc<dyn HttpTransport> = Arc::new(FakeTransport {
            resp: JsonResponse::new(200, body.as_bytes().to_vec()),
            observed: Mutex::new(Vec::new()),
        });
        let mgr = SystemMountManager::new().with_transport(transport);
        let err = mgr.list_available_shares("https://os").await.unwrap_err();
        assert!(matches!(err, DesktopError::Internal(_)));
    }

    #[tokio::test]
    async fn list_available_shares_server_error_then_no_fallback() {
        // 注入了 transport 但 HTTP 失败 → 不回退到本地 shares，直接报错
        let mgr = SystemMountManager::new()
            .with_shares(vec![RemoteShare {
                name: "local".into(),
                protocol: MountProtocol::Smb,
                description: None,
            }])
            .with_transport(Arc::new(FailTransport));
        let err = mgr.list_available_shares("https://os").await.unwrap_err();
        // 注入了 transport → 走 HTTP 路径 → 失败 → 不回退
        assert!(matches!(err, DesktopError::Internal(_)));
    }

    #[tokio::test]
    async fn list_available_shares_observed_records_get_shares() {
        // 用一个能记录观察的 transport，验证 list_available_shares 发的是 GET /shares
        // 复用模块内已有的 FakeTransport（带 observed 字段）。
        let body = r#"[]"#;
        let transport: Arc<FakeTransport> = Arc::new(FakeTransport {
            resp: JsonResponse::new(200, body.as_bytes().to_vec()),
            observed: Mutex::new(Vec::new()),
        });
        let observed_ref: Arc<dyn HttpTransport> = transport.clone();
        let mgr = SystemMountManager::new().with_transport(observed_ref);
        mgr.list_available_shares("https://os:8443").await.unwrap();
        let got = transport.observed.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "https://os:8443");
        assert_eq!(got[0].1, "/shares");
        assert_eq!(got[0].2, "GET");
    }

    #[tokio::test]
    async fn with_shares_returns_injected_multiple_protocols() {
        // 注入混合协议的 shares
        let mgr = SystemMountManager::new().with_shares(vec![
            RemoteShare {
                name: "s1".into(),
                protocol: MountProtocol::Smb,
                description: None,
            },
            RemoteShare {
                name: "s2".into(),
                protocol: MountProtocol::Webdav,
                description: Some("d2".into()),
            },
            RemoteShare {
                name: "s3".into(),
                protocol: MountProtocol::Smb,
                description: None,
            },
        ]);
        let shares = mgr.list_available_shares("os").await.unwrap();
        assert_eq!(shares.len(), 3);
        let smb_count = shares
            .iter()
            .filter(|s| s.protocol == MountProtocol::Smb)
            .count();
        assert_eq!(smb_count, 2);
    }
}
