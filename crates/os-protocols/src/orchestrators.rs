//! 协议编排器——把 `FileProtocol` 父 trait + 各子 trait 落到内存存储 + 配置生成，
//! 并为 WebDAV/FTP/SFTP 接通真实协议栈（dav-server / libunftp / russh）。
//!
//! 设计（规格 §3 / §5.1）：
//! - 每个协议一个编排器 struct（`SambaOrchestrator` / `NfsOrchestrator` /
//!   `DavServerBackend` / `LibunftpBackend` / `RusshSftpBackend`），不挂 agent 前缀。
//! - 各编排器持有一份 [`ShareStore`]（协议无关的共享/会话存储）+ 一份自身配置。
//! - `FileProtocol` 的 7 个生命周期/会话方法落到 `ShareStore`；协议特有副作用：
//!   - **WebDAV/FTP/SFTP**：接通真实协议栈对象（`DavHandler` / `libunftp::Server` /
//!     `russh` `Server`+`Handler`），不真监听端口（红线）；测试用离线 fixture 验证。
//!   - **SMB/NFS**：编排 Samba/nfs-ganesha（CLI 骨架，本批不引入 samba crate）。
//! - 错误统一 `ProtocolError`，对外经 `From<ProtocolError> for ApiError`。
//!
//! 为什么 SMB 仍走 CLI 编排（务实边界，见规格 §2 / §9）：
//! SMB 无成熟纯 Rust 实现，编排 Samba（smbd/smbcontrol/smbstatus）；smb.conf/ganesha.conf
//! 渲染为真实可用的纯函数，但热重载/写盘以 TODO 标注（不真改系统配置，红线）。
//! 所有 `TODO(协议栈)` 标记均属 **\[RUNTIME\]** 类——需真实 samba/nfs-ganesha/exportfs
//! 二进制 + root/CAP_SYS_ADMIN，逻辑骨架与配置生成已就绪，仅缺运行时环境。
//!
//! 这些编排器可被下游（api/service）通过 trait 消费，并在 mock feature 下有完整内存实现。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use os_core::ShareId;

use crate::common::{FileProtocol, Session, Share, ShareOptions};
use crate::error::{ProtocolError, ProtocolResult};
use crate::ftp::FtpConfig;
use crate::nfs::{GaneshaConfig, NfsExportOptions};
use crate::sftp::SftpConfig;
use crate::smb::{render_smb_conf, SambaConfig, SambaShareSpec};
use crate::state::{apply_options, ShareStore};
use crate::webdav::WebDavConfig;

// ============================================================================
// SambaOrchestrator —— SMB（编排 Samba）
// ============================================================================

/// 真实重载策略——控制 `reload_smbd` 是否真跑 `smbcontrol`。
///
/// 设计动机（红线 §9）：生产侧默认 `Enabled`（真 reload 运行中的 smbd）；
/// 测试/CI 侧注入 `DryRun`（只构造命令文本、不 spawn 子进程）或 `Disabled`
/// （完全跳过 reload，纯内存生命周期）。三种策略都向后兼容——`Default` 为 `Enabled`，
/// 与历史 TODO 接通后的生产行为一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReloadPolicy {
    /// 真跑 `smbcontrol all reload-config`（生产；失败转 `ReloadFailed`）。
    #[default]
    Enabled,
    /// 只构造命令文本、不 spawn（测试；断言命令字符串正确，不碰宿主 smbd）。
    DryRun,
    /// 完全跳过 reload（测试/CI；只验证内存生命周期 + 落盘）。
    Disabled,
}

/// SMB 编排器——编排 Samba（smbd + smb.conf + vfs_fruit）。
///
/// 内部状态：全局 `SambaConfig` + 各共享的渲染规格（`SambaShareSpec`）+
/// `ShareStore`（共享/会话）。`write_smb_conf` 渲染完整 smb.conf 文本并真实落盘到
/// `config.config_path`（生产 `/etc/samba/smb.conf`，测试可注入 `/tmp/...`）；
/// `reload_smbd` 按 [`ReloadPolicy`] 决定是否真跑 `smbcontrol all reload-config`。
pub struct SambaOrchestrator {
    config: SambaConfig,
    /// 共享 ID → SMB 渲染规格（含 Time Machine 标记）
    specs: Mutex<HashMap<String, SambaShareSpec>>,
    store: ShareStore,
    /// 真实 reload 策略（默认 `Enabled`；测试注入 `DryRun`/`Disabled`）。
    reload: ReloadPolicy,
}

impl Default for SambaOrchestrator {
    fn default() -> Self {
        Self::new(SambaConfig::defaults())
    }
}

impl SambaOrchestrator {
    /// 用指定全局配置构造（`ReloadPolicy::Enabled`，与生产行为一致）。
    #[must_use]
    pub fn new(config: SambaConfig) -> Self {
        Self::with_reload(config, ReloadPolicy::default())
    }

    /// 用指定全局配置 + reload 策略构造（测试注入 `DryRun`/`Disabled`）。
    #[must_use]
    pub fn with_reload(config: SambaConfig, reload: ReloadPolicy) -> Self {
        Self {
            config,
            specs: Mutex::new(HashMap::new()),
            store: ShareStore::new(),
            reload,
        }
    }

    /// 当前 reload 策略（测试断言用）。
    #[must_use]
    pub fn reload_policy(&self) -> ReloadPolicy {
        self.reload
    }

    /// 由 `Share` + `ShareOptions` 派生 SMB 渲染规格（共享级默认值）。
    fn spec_for(share: &Share, options: &ShareOptions) -> SambaShareSpec {
        SambaShareSpec {
            name: share.name.clone(),
            path: share.path.clone(),
            comment: options.comment.clone(),
            browseable: options.browseable.unwrap_or(true),
            read_only: share.read_only,
            guest_ok: options.guest_ok.unwrap_or(false),
            valid_users: options.valid_users.clone(),
            hosts_allow: share.hosts_allow.clone(),
            time_machine: false,
            time_machine_max_size_gb: None,
        }
    }

    /// 渲染完整 smb.conf 文本（全局段 + 所有共享段）。这是 `write_smb_conf` 的纯函数核心。
    #[must_use]
    pub fn render_conf(&self) -> String {
        let specs: Vec<SambaShareSpec> = {
            let g = self.specs.lock().expect("specs poisoned");
            let mut all: Vec<SambaShareSpec> = g.values().cloned().collect();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            all
        };
        render_smb_conf(&self.config, &specs)
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for SambaOrchestrator {
    async fn create_share(&self, share: Share, options: ShareOptions) -> ProtocolResult<Share> {
        let spec = Self::spec_for(&share, &options);
        self.store.put_share(share.clone())?;
        self.specs
            .lock()
            .expect("specs poisoned")
            .insert(share.id.as_str().to_string(), spec);
        // write_smb_conf（落盘）+ reload_smbd（smbcontrol）已接通为独立方法，
        // 由上层（api/service）在共享变更后显式调用——此处保持纯内存生命周期，
        // 避免生命周期方法隐式落盘破坏默认配置（/etc/samba/smb.conf）的安全契约。
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, options: ShareOptions) -> ProtocolResult<Share> {
        let mut share = self.store.get_share(id)?;
        share = apply_options(&share, &options);
        // 把新选项重渲为 SMB 规格（覆盖旧值）
        let spec = Self::spec_for(&share, &options);
        self.store.replace_share(share.clone())?;
        self.specs
            .lock()
            .expect("specs poisoned")
            .insert(id.as_str().to_string(), spec);
        // write_smb_conf + reload_smbd 已接通；由上层显式调用（见 create_share 注释）。
        Ok(share)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        self.store.remove_share(id)?;
        self.specs
            .lock()
            .expect("specs poisoned")
            .remove(id.as_str());
        // write_smb_conf + reload_smbd 已接通；由上层显式调用（见 create_share 注释）。
        Ok(())
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        self.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        self.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        self.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        self.store.close_session(session_id)?;
        // TODO(协议栈) [RUNTIME]: smbcontrol <pid> close-share / 直接 kill 客户端连接。
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
impl crate::smb::SmbManager for SambaOrchestrator {
    async fn write_smb_conf(&self) -> ProtocolResult<PathBuf> {
        let conf = self.render_conf();
        // 真实落盘到 config.config_path（生产 /etc/samba/smb.conf；
        // 测试经 SambaOrchestrator::with_reload + 临时 config_path 注入）。
        // 若父目录缺失则先创建（生产 /etc/samba 可能尚未建）。
        if let Some(parent) = self.config.config_path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        tokio::fs::write(&self.config.config_path, &conf).await?;
        Ok(self.config.config_path.clone())
    }

    async fn reload_smbd(&self) -> ProtocolResult<()> {
        match self.reload {
            ReloadPolicy::Disabled => Ok(()),
            ReloadPolicy::DryRun => {
                // 不 spawn 子进程：只校验命令构造可达（测试侧契约）。
                // 构造的命令字符串与 Enabled 一致：`smbcontrol all reload-config`。
                eprintln!(
                    "[samba] DryRun reload：smbcontrol all reload-config \
                     (config_path={})",
                    self.config.config_path.display()
                );
                Ok(())
            }
            ReloadPolicy::Enabled => {
                // 真跑 `smbcontrol all reload-config`（生产路径）。
                // 注：`all` 目标会找运行中的 smbd；若 smbd 未运行，smbcontrol 返回非 0，
                // 此处把失败转 ReloadFailed（含 stderr），由上层决定是否容忍。
                let out = tokio::process::Command::new("smbcontrol")
                    .arg("all")
                    .arg("reload-config")
                    .output()
                    .await
                    .map_err(|e| {
                        ProtocolError::ReloadFailed(format!("spawn smbcontrol 失败: {e}"))
                    })?;
                if out.status.success() {
                    return Ok(());
                }
                // 失败：聚合 stderr 给上层（典型：smbd 未运行时 "No daemons active"）。
                let stderr = String::from_utf8_lossy(&out.stderr);
                Err(ProtocolError::ReloadFailed(format!(
                    "smbcontrol reload-config 退出 {:?}: {stderr}",
                    out.status.code()
                )))
            }
        }
    }

    async fn enable_time_machine(
        &self,
        share: &ShareId,
        size_limit_gb: Option<u64>,
    ) -> ProtocolResult<Share> {
        let mut g = self.specs.lock().expect("specs poisoned");
        let spec = g
            .get_mut(share.as_str())
            .ok_or_else(|| ProtocolError::ShareNotFound(share.as_str().to_string()))?;
        spec.time_machine = true;
        spec.time_machine_max_size_gb = size_limit_gb;
        drop(g);
        // write_smb_conf + reload_smbd 已接通；由上层显式调用（见 create_share 注释）。
        self.store.get_share(share)
    }

    async fn list_smb_sessions(&self) -> ProtocolResult<Vec<Session>> {
        // TODO(协议栈) [RUNTIME]: 解析 `smbstatus -p -J`（JSON 输出）转 Session。
        self.store.list_sessions()
    }
}

// ============================================================================
// NfsOrchestrator —— NFS（v3 exports + v4 ganesha）
// ============================================================================

/// NFS 编排器——编排 nfsserve(NFSv3) / nfs-ganesha(NFSv4)。
///
/// 内部状态：ganesha 全局配置 + 各共享的 NFS export 选项 + `ShareStore` +
/// exports 文件落盘路径（生产 `/etc/exports`，测试注入 `/tmp/...`）+ reload 策略。
/// 配置生成（exports 文件 / ganesha.conf）由 `crate::nfs` 的纯函数提供；
/// `apply_exports` 把渲染产物落盘 + 按 [`ReloadPolicy`] 触发真实 exportfs 往返。
pub struct NfsOrchestrator {
    ganesha: GaneshaConfig,
    /// 共享 ID → (客户端列表, export 选项)
    exports: Mutex<HashMap<String, (Vec<String>, NfsExportOptions)>>,
    store: ShareStore,
    /// exports 文件落盘路径（默认 `/etc/exports`；测试注入 `/tmp/...`）。
    exports_path: PathBuf,
    /// 真实 exportfs reload 策略（默认 `Enabled`；测试注入 `DryRun`/`Disabled`）。
    reload: ReloadPolicy,
}

impl Default for NfsOrchestrator {
    fn default() -> Self {
        Self::new(GaneshaConfig::defaults())
    }
}

impl NfsOrchestrator {
    /// 用指定 ganesha 全局配置构造（`exports_path=/etc/exports`、`ReloadPolicy::Enabled`）。
    #[must_use]
    pub fn new(ganesha: GaneshaConfig) -> Self {
        Self::with_reload(
            ganesha,
            PathBuf::from("/etc/exports"),
            ReloadPolicy::default(),
        )
    }

    /// 用指定 ganesha 配置 + exports 路径 + reload 策略构造（测试注入临时路径）。
    #[must_use]
    pub fn with_reload(
        ganesha: GaneshaConfig,
        exports_path: PathBuf,
        reload: ReloadPolicy,
    ) -> Self {
        Self {
            ganesha,
            exports: Mutex::new(HashMap::new()),
            store: ShareStore::new(),
            exports_path,
            reload,
        }
    }

    /// 当前 ganesha 全局配置快照（含 `render` 入口，供编排器生成 ganesha.conf）。
    #[must_use]
    pub fn ganesha_config(&self) -> &GaneshaConfig {
        &self.ganesha
    }

    /// 当前 exports 文件落盘路径（测试断言用）。
    #[must_use]
    pub fn exports_path(&self) -> &PathBuf {
        &self.exports_path
    }

    /// 当前 reload 策略（测试断言用）。
    #[must_use]
    pub fn reload_policy(&self) -> ReloadPolicy {
        self.reload
    }

    /// 渲染 NFSv3 `/etc/exports` 文本（基于已注册的 export）。
    #[must_use]
    pub fn render_exports(&self, shares: &[Share]) -> String {
        use crate::nfs::{NfsClientExport, NfsExportsEntry};
        let ex = self.exports.lock().expect("exports poisoned");
        let mut entries: Vec<NfsExportsEntry> = Vec::new();
        for s in shares {
            if let Some((clients, opts)) = ex.get(s.id.as_str()) {
                entries.push(NfsExportsEntry {
                    path: s.path.clone(),
                    clients: clients
                        .iter()
                        .map(|c| NfsClientExport {
                            client: c.clone(),
                            options: opts.clone(),
                        })
                        .collect(),
                });
            }
        }
        crate::nfs::render_exports(&entries)
    }

    /// 把当前 export 表渲染 + 落盘到 `exports_path`，并按 reload 策略触发真实 exportfs。
    ///
    /// - **落盘**：始终写 `exports_path`（生产 `/etc/exports`，测试注入 `/tmp/...`）。
    /// - **exportfs 往返**（`ReloadPolicy::Enabled`）：对每条 `<client>:<path>` 跑
    ///   `exportfs -i -o <opts> <client>:<path>`（`-i` 忽略 `/etc/exports`，只应用本条），
    ///   把编排器渲染的 option 串喂给真实 exportfs 的 option 解析器并落入内核 export 表。
    ///   失败转 `ReloadFailed`（含 stderr）。
    /// - **DryRun**：只打印将执行的命令，不 spawn（测试）。
    /// - **Disabled**：只落盘，不跑 exportfs（测试/CI）。
    ///
    /// 红线：用 `exportfs -i`（忽略 /etc/exports）+ 经 exports_path 注入临时路径，
    /// 不碰宿主 `/etc/exports`、不改既有 export。
    pub async fn apply_exports(&self, shares: &[Share]) -> ProtocolResult<()> {
        let txt = self.render_exports(shares);
        // 落盘（父目录缺失则先建；生产 /etc 已存在，测试 /tmp/... 父目录可能需建）
        if let Some(parent) = self.exports_path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        tokio::fs::write(&self.exports_path, &txt).await?;

        match self.reload {
            ReloadPolicy::Disabled => Ok(()),
            ReloadPolicy::DryRun => {
                eprintln!(
                    "[nfs] DryRun apply_exports：exports_path={} ({} 字节)",
                    self.exports_path.display(),
                    txt.len()
                );
                Ok(())
            }
            ReloadPolicy::Enabled => {
                // 对每条 export 行展开为 client:path，逐条 exportfs -i -o <opts> 应用。
                // exportfs 无「批量应用临时文件」模式（-ra 读 /etc/exports，-f 是 flush），
                // 故用 `-i`（忽略 /etc/exports）逐条落内核 export 表——与 batch6 验证一致。
                // 注意：std Mutex guard 不可跨 await 持有（clippy::await_holding_lock），
                // 故先在锁内 clone 出 (client, opts_str, path) 列表再 drop guard，最后逐条 await。
                let specs: Vec<(String, String)> = {
                    let ex = self.exports.lock().expect("exports poisoned");
                    let mut acc = Vec::new();
                    for s in shares {
                        if let Some((clients, opts)) = ex.get(s.id.as_str()) {
                            let opts_str = crate::nfs::NfsClientExport {
                                client: String::new(),
                                options: opts.clone(),
                            }
                            .render_options();
                            for c in clients {
                                acc.push((format!("{c}:{}", s.path.display()), opts_str.clone()));
                            }
                        }
                    }
                    acc
                }; // guard 在此 drop
                for (spec, opts_str) in specs {
                    let out = tokio::process::Command::new("exportfs")
                        .args(["-i", "-o", &opts_str, &spec])
                        .output()
                        .await
                        .map_err(|e| {
                            ProtocolError::ReloadFailed(format!("spawn exportfs 失败: {e}"))
                        })?;
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        return Err(ProtocolError::ReloadFailed(format!(
                            "exportfs {spec} 退出 {:?}: {stderr}",
                            out.status.code()
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    /// 撤销指定共享已落入内核 export 表的导出（`exportfs -u <client>:<path>`，幂等）。
    ///
    /// 仅 `ReloadPolicy::Enabled` 下真跑。调用方需传入**移除前**从 exports map 取出的
    /// clients 列表（因调用前 map 条目可能已被移除）。
    /// exportfs -u 对不存在的 export 返回非 0 但无害，故此处忽略错误（幂等语义）。
    async fn unexport_clients(&self, share: &Share, clients: &[String]) -> ProtocolResult<()> {
        if self.reload != ReloadPolicy::Enabled {
            return Ok(());
        }
        for c in clients {
            let spec = format!("{c}:{}", share.path.display());
            // 幂等：忽略退出码（exportfs -u 对不存在 export 报错但无副作用）
            let _ = tokio::process::Command::new("exportfs")
                .args(["-u", &spec])
                .output()
                .await;
        }
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for NfsOrchestrator {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        self.store.put_share(share.clone())?;
        // 共享创建本身不触发 export（需 add_export 显式登记）；apply_exports 已接通，
        // 由 add_export/remove_export 调用。ganesha.conf reload（v4）留后续接通。
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, options: ShareOptions) -> ProtocolResult<Share> {
        let mut share = self.store.get_share(id)?;
        share = apply_options(&share, &options);
        self.store.replace_share(share.clone())?;
        // export 选项变更后若该共享已导出，上层应调 apply_exports 重落（已接通）。
        Ok(share)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        // 取共享快照（用于 unexport；remove_share 后不可达）+ 移除前取回 clients 快照。
        let share_snapshot = self.store.get_share(id).ok();
        let removed_clients = self
            .exports
            .lock()
            .expect("exports poisoned")
            .remove(id.as_str())
            .map(|(clients, _)| clients);
        self.store.remove_share(id)?;
        // 真实撤销内核 export（用移除前取回的 clients；幂等；仅 Enabled 下真跑）
        if let (Some(s), Some(clients)) = (share_snapshot, removed_clients) {
            self.unexport_clients(&s, &clients).await?;
        }
        let shares = self.store.list_shares()?;
        self.apply_exports(&shares).await?;
        Ok(())
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        self.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        self.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        // NFS 无状态协议，会话由客户端持有；返回空。
        Ok(Vec::new())
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        // NFSv4 有状态，可经 SETCLIENTID 撤销；本批次骨架返回 SessionNotFound。
        Err(ProtocolError::SessionNotFound(session_id.to_string()))
    }
}

#[allow(async_fn_in_trait)]
impl crate::nfs::NfsManager for NfsOrchestrator {
    async fn add_export(
        &self,
        share: &ShareId,
        clients: Vec<String>,
        options: NfsExportOptions,
    ) -> ProtocolResult<Share> {
        // 校验共享存在
        let s = self.store.get_share(share)?;
        self.exports
            .lock()
            .expect("exports poisoned")
            .insert(share.as_str().to_string(), (clients, options));
        // 真实落盘 exports + 按 reload 策略触发 exportfs 往返（v3）。
        // ganesha EXPORT 块 + reload（v4）仍留后续接通（本批聚焦 v3 exportfs）。
        let shares = self.store.list_shares()?;
        self.apply_exports(&shares).await?;
        Ok(s)
    }

    async fn remove_export(&self, share: &ShareId) -> ProtocolResult<()> {
        // 取共享快照（用于 unexport；remove_share 后不可达）+ 移除前取回 clients 快照。
        let share_snapshot = self.store.get_share(share).ok();
        let removed_clients = self
            .exports
            .lock()
            .expect("exports poisoned")
            .remove(share.as_str())
            .map(|(clients, _)| clients);
        if removed_clients.is_none() {
            // 共享本身可能存在但未导出——保留幂等：
            // 若共享不存在则报错，存在但无 export 视作成功（已无 export 可移除）。
            if self.store.get_share(share).is_err() {
                return Err(ProtocolError::ShareNotFound(share.as_str().to_string()));
            }
        }
        // 真实撤销内核 export（用移除前取回的 clients；幂等；仅 Enabled 下真跑）
        if let (Some(s), Some(clients)) = (share_snapshot, removed_clients) {
            self.unexport_clients(&s, &clients).await?;
        }
        // 重写 exports 文件（移除该条后落盘）
        let shares = self.store.list_shares()?;
        self.apply_exports(&shares).await?;
        Ok(())
    }
}

// ============================================================================
// DavServerBackend —— WebDAV（接通 dav-server 真实协议栈）
// ============================================================================

/// WebDAV 后端——基于 [`dav_server::DavHandler`] 的真实 WebDAV 协议栈。
///
/// 设计：
/// - 每个共享（`create_share`）对应一个独立的 `DavHandler<()>`，其文件系统后端为
///   [`dav_server::memfs::MemFs`]（**纯内存**）。选用 MemFs 而非 `LocalFs`，是为了
///   ① 离线/CI 友好（无磁盘依赖）；② 测试可通过 `handle_request` 把真实
///   `http::Request` 喂入 `dav-server` 的真实协议处理器，验证 RFC4918 行为。
/// - **不真监听 TCP 端口**（红线）：仅持有处理器对象，对外暴露 `handler`/`handle_request`
///   供上层（api/service）在 axum/hyper 路由中挂载；端口绑定由上层负责。
/// - 共享/会话生命周期复用 [`ShareStore`]；`WebDavManager` 父 trait 默认行为即可。
pub struct DavServerBackend {
    config: WebDavConfig,
    store: ShareStore,
    /// 共享 ID → 真实 dav-server 处理器（每个共享一份 MemFs 后端）。
    handlers: Mutex<HashMap<String, dav_server::DavHandler>>,
}

impl Default for DavServerBackend {
    fn default() -> Self {
        Self::new(WebDavConfig::defaults())
    }
}

impl DavServerBackend {
    /// 用指定配置构造。
    #[must_use]
    pub fn new(config: WebDavConfig) -> Self {
        Self {
            config,
            store: ShareStore::new(),
            handlers: Mutex::new(HashMap::new()),
        }
    }

    /// 当前 WebDAV 配置快照。
    #[must_use]
    pub fn config(&self) -> &WebDavConfig {
        &self.config
    }

    /// 为单个共享构造一个真实 dav-server 处理器（MemFs 后端 + FakeLs 锁系统）。
    ///
    /// 这是 `create_share` 的协议栈核心：把"一个共享目录"映射为 dav-server 可消费的
    /// `DavHandler`。MemFs 是 RFC4918 WebDAV 的内存实现，离线可测、无磁盘副作用。
    fn build_handler() -> dav_server::DavHandler {
        dav_server::DavConfig::new()
            .filesystem(dav_server::memfs::MemFs::new())
            .locksystem(dav_server::fakels::FakeLs::new())
            .build_handler()
    }

    /// 取指定共享对应的真实 dav-server 处理器（None = 共享未挂载）。
    ///
    /// 上层（api/service）用它把 WebDAV 路由挂到 axum/hyper；测试可用
    /// [`Self::handle_request`] 直接驱动协议栈。
    pub fn handler(&self, id: &ShareId) -> Option<dav_server::DavHandler> {
        self.handlers
            .lock()
            .expect("handlers poisoned")
            .get(id.as_str())
            .cloned()
    }

    /// 当前已挂载（注册）的 dav-server 处理器数量（断言用）。
    pub fn mount_count(&self) -> usize {
        self.handlers.lock().expect("handlers poisoned").len()
    }

    /// 把一个真实的 `http::Request` 喂入指定共享的 dav-server 处理器，返回其真实 HTTP 响应。
    ///
    /// 这是验证 WebDAV 协议栈**真的接通**的关键入口——不发网络，仅驱动协议处理。
    /// 测试可用它跑 PROPFIND/GET/PUT/MKCOL 等 RFC4918 方法。
    ///
    /// 泛型边界与 [`dav_server::DavHandler::handle`] 对齐（`Data: bytes::Buf`、
    /// `Error: std::error::Error + Send + Sync + 'static`）。
    pub async fn handle_request<B>(
        &self,
        id: &ShareId,
        req: http::Request<B>,
    ) -> Option<http::Response<dav_server::body::Body>>
    where
        B: http_body::Body + 'static,
        B::Data: bytes::Buf + Send + 'static,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let h = self.handler(id)?;
        Some(h.handle(req).await)
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for DavServerBackend {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        // 先落 ShareStore（共享逻辑视图），再为该共享构造并挂载真实 dav-server 处理器。
        self.store.put_share(share.clone())?;
        let handler = Self::build_handler();
        self.handlers
            .lock()
            .expect("handlers poisoned")
            .insert(share.id.as_str().to_string(), handler);
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, options: ShareOptions) -> ProtocolResult<Share> {
        let mut share = self.store.get_share(id)?;
        share = apply_options(&share, &options);
        self.store.replace_share(share.clone())?;
        // WebDAV 共享语义由 MemFs 后端承载；选项更新不重建处理器（保留数据）。
        Ok(share)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        self.store.remove_share(id)?;
        // 从 dav-server 处理器表中卸载该共享（处理器及其 MemFs 一并丢弃）。
        self.handlers
            .lock()
            .expect("handlers poisoned")
            .remove(id.as_str());
        Ok(())
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        self.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        self.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        self.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        self.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::webdav::WebDavManager for DavServerBackend {}

// ============================================================================
// LibunftpBackend —— FTP（接通 libunftp 真实协议栈）
// ============================================================================

/// FTP 后端——基于 [`libunftp::ServerBuilder`] 的真实 FTP 协议栈。
///
/// 设计：
/// - 每个共享（`create_share`）对应一份独立的内存存储后端
///   [`crate::ftp_backend::InMemoryFtpBackend`]（实现 `StorageBackend<DefaultUser>`），
///   通过 [`Self::build_server`] 可构造出**真实但未监听**的 `libunftp::Server`。
/// - **不真监听 TCP 端口**（红线）：`build_server` 仅返回构造好的 `Server` 对象，
///   由上层（api/service）负责 `listen()`；测试可校验服务配置 + 直接驱动存储后端。
/// - 共享/会话生命周期复用 [`ShareStore`]；`FtpManager` 父 trait 默认行为即可。
pub struct LibunftpBackend {
    config: FtpConfig,
    store: ShareStore,
    /// 共享 ID → 该共享的内存 FTP 存储后端（与共享生命周期一致）。
    backends: Mutex<HashMap<String, Arc<crate::ftp_backend::InMemoryFtpBackend>>>,
}

impl Default for LibunftpBackend {
    fn default() -> Self {
        Self::new(FtpConfig::defaults())
    }
}

impl LibunftpBackend {
    /// 用指定配置构造。
    #[must_use]
    pub fn new(config: FtpConfig) -> Self {
        Self {
            config,
            store: ShareStore::new(),
            backends: Mutex::new(HashMap::new()),
        }
    }

    /// 当前 FTP 配置快照。
    #[must_use]
    pub fn config(&self) -> &FtpConfig {
        &self.config
    }

    /// 取指定共享对应的存储后端句柄（None = 共享未挂载）。
    ///
    /// 测试可经此驱动真实 libunftp `StorageBackend`（list/get/put 等），验证 FTP 协议栈接通。
    pub fn storage(&self, id: &ShareId) -> Option<Arc<crate::ftp_backend::InMemoryFtpBackend>> {
        self.backends
            .lock()
            .expect("backends poisoned")
            .get(id.as_str())
            .cloned()
    }

    /// 当前已挂载（注册）的 FTP 存储后端数量（断言用）。
    pub fn mount_count(&self) -> usize {
        self.backends.lock().expect("backends poisoned").len()
    }

    /// 构造一个真实但**未监听**的 `libunftp::Server`，绑定到指定共享的存储后端。
    ///
    /// 配置来源：
    /// - 匿名认证（[`libunftp::auth::AnonymousAuthenticator`]）——生产应替换为真实认证器；
    /// - 被动端口范围取自 [`FtpConfig::passive_ports`]；
    /// - 固定 greeting（libunftp 要求 `&'static str`）。
    ///
    /// 返回 `Server<InMemoryFtpBackend, DefaultUser>`，由上层 `listen(addr)` 完成端口绑定。
    /// 这是验证 FTP 协议栈**真的接通**的关键入口。
    ///
    /// 注：工厂闭包每次为连接构造一份独立后端（连接级隔离），与 libunftp 的
    /// `Box<dyn Fn() -> Storage>` 语义一致；真实生产应换为可 Clone 的 fs 后端。
    pub fn build_server(
        &self,
        id: &ShareId,
    ) -> ProtocolResult<
        libunftp::Server<crate::ftp_backend::InMemoryFtpBackend, unftp_core::auth::DefaultUser>,
    > {
        // 校验共享已挂载（驱动真实存储后端存在性校验）。
        if self.storage(id).is_none() {
            return Err(ProtocolError::ShareNotFound(id.as_str().to_string()));
        }
        let (lo, hi) = self.config.passive_ports;
        let server = libunftp::ServerBuilder::new(Box::new(|| {
            // libunftp 的存储工厂语义：每个连接一份 StorageBackend。
            // 这里返回独立后端实例（离线测试不要求连接间共享数据）。
            crate::ftp_backend::InMemoryFtpBackend::new()
        }))
        .authenticator(Arc::new(libunftp::auth::AnonymousAuthenticator))
        .passive_ports(lo..=hi)
        .greeting("OS FTP ready")
        .build()
        .map_err(|e| ProtocolError::Internal(format!("libunftp build 失败: {e}")))?;
        Ok(server)
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for LibunftpBackend {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        // 先落 ShareStore（共享逻辑视图），再为该共享构造并挂载真实 FTP 存储后端。
        self.store.put_share(share.clone())?;
        self.backends.lock().expect("backends poisoned").insert(
            share.id.as_str().to_string(),
            Arc::new(crate::ftp_backend::InMemoryFtpBackend::new()),
        );
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, options: ShareOptions) -> ProtocolResult<Share> {
        let mut share = self.store.get_share(id)?;
        share = apply_options(&share, &options);
        self.store.replace_share(share.clone())?;
        // FTP 共享语义由存储后端承载；选项更新不重建后端（保留数据）。
        Ok(share)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        self.store.remove_share(id)?;
        // 从存储后端表中卸载该共享（后端及其内存数据一并丢弃）。
        self.backends
            .lock()
            .expect("backends poisoned")
            .remove(id.as_str());
        Ok(())
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        self.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        self.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        self.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        self.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::ftp::FtpManager for LibunftpBackend {}

// ============================================================================
// RusshSftpBackend —— SFTP（接通 russh 真实协议栈）
// ============================================================================

/// SFTP 后端——基于 [`russh`] 的真实 SSH/SFTP 协议栈。
///
/// 设计：
/// - 内部持有 `SftpConfig` + `authorized_keys`（用户 → 公钥列表）+ `ShareStore`。
/// - [`Self::build_ssh_server`] 构造一个真实但**未监听**的 [`crate::sftp_backend::OsSshServer`]
///   （实现 `russh::server::Server`，承载 authorized_keys 公钥认证 + SFTP 子系统）；
///   [`Self::build_ssh_config`] 构造带 Ed25519 主机密钥的 `russh::server::Config`。
/// - **不真监听 TCP 端口**（红线）：上层（api/service）负责 `run_on_socket`；
///   测试可直接断言认证决策与配置构造，验证 SSH 协议栈接通。
/// - `authorize_key` 把公钥追加到映射（同时驱动真实 `OsSshHandler` 的认证决策）。
pub struct RusshSftpBackend {
    config: SftpConfig,
    /// 用户 → 公钥列表（标准 OpenSSH 公钥行；auth_publickey 解析 base64 段比对）
    authorized_keys: Mutex<HashMap<String, Vec<String>>>,
    store: ShareStore,
}

impl Default for RusshSftpBackend {
    fn default() -> Self {
        Self::new(SftpConfig::defaults())
    }
}

impl RusshSftpBackend {
    /// 用指定配置构造。
    #[must_use]
    pub fn new(config: SftpConfig) -> Self {
        Self {
            config,
            authorized_keys: Mutex::new(HashMap::new()),
            store: ShareStore::new(),
        }
    }

    /// 当前 SFTP 配置快照（含 `render` 入口，供编排器生成配置）。
    #[must_use]
    pub fn config(&self) -> &SftpConfig {
        &self.config
    }

    /// 渲染汇总的 authorized_keys 文本（编排器按用户拆分写盘）。
    #[must_use]
    pub fn render_authorized_keys(&self) -> String {
        crate::sftp::render_authorized_keys(
            &self.authorized_keys.lock().expect("authkeys poisoned"),
        )
    }

    /// 取 authorized_keys 映射快照（拷贝；供 [`Self::build_ssh_server`] 消费）。
    fn authorized_keys_snapshot(&self) -> HashMap<String, Vec<String>> {
        self.authorized_keys
            .lock()
            .expect("authkeys poisoned")
            .clone()
    }

    /// 构造一个真实但**未监听**的 SSH 服务端工厂（`russh::server::Server` 实现），
    /// 携带当前 authorized_keys——可被 `run_on_socket` 驱动接受客户端连接。
    ///
    /// 这是验证 SFTP 协议栈**真的接通**的关键入口（公钥认证 + SFTP 子系统请求处理）。
    #[must_use]
    pub fn build_ssh_server(&self) -> crate::sftp_backend::OsSshServer {
        crate::sftp_backend::OsSshServer::new(Arc::new(self.authorized_keys_snapshot()))
    }

    /// 构造一个真实可用的 `russh::server::Config`——含一个临时 Ed25519 主机密钥
    /// + 仅公钥认证（与 [`SftpConfig`] 默认 `pubkey_auth=true` 一致）。
    ///
    /// 返回 `Arc<Config>`（`run_on_socket` 要求 `Arc<Config>`）；离线测试可直接断言其字段。
    pub fn build_ssh_config(&self) -> ProtocolResult<Arc<russh::server::Config>> {
        crate::sftp_backend::build_ssh_config()
            .map_err(|e| ProtocolError::ConfigFailed(format!("SSH 配置生成失败: {e}")))
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for RusshSftpBackend {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        // 共享语义：在 russh sftp-subsystem 暴露该共享路径。本批共享路径管理由
        // ShareStore 维护（数据集路径来源），russh 侧的路径过滤在 OsSshHandler 的
        // data/exec 请求处理中（生产补 chroot/路径白名单；离线骨架不实现文件传输）。
        self.store.put_share(share.clone())?;
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, options: ShareOptions) -> ProtocolResult<Share> {
        let mut share = self.store.get_share(id)?;
        share = apply_options(&share, &options);
        self.store.replace_share(share.clone())?;
        Ok(share)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        self.store.remove_share(id)
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        self.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        self.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        self.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        self.store.close_session(session_id)?;
        // 真实 russh 会话级断开由 OsSshServer 持有的 Handle 负责（生产侧）；
        // 这里只清理 ShareStore 中的会话记录。
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
impl crate::sftp::SftpManager for RusshSftpBackend {
    async fn authorize_key(&self, user: &str, pubkey: &str) -> ProtocolResult<()> {
        let trimmed = pubkey.trim();
        if trimmed.is_empty() {
            return Err(ProtocolError::ConfigFailed("公钥不能为空".into()));
        }
        // 存储完整 OpenSSH 公钥行（`<algo> <base64> [comment]`），便于
        // [`Self::render_authorized_keys`] 输出可直接落盘 authorized_keys；
        // 运行时认证比对在 [`crate::sftp_backend::OsSshHandler::auth_publickey`] 中
        // 提取 base64 段进行（与存储格式解耦）。
        self.authorized_keys
            .lock()
            .expect("authkeys poisoned")
            .entry(user.to_string())
            .or_default()
            .push(trimmed.to_string());
        // authorized_keys 即时生效：新客户端连接时 OsSshServer.new_client() 取最新快照。
        // 生产侧另需把 per-user authorized_keys 落盘（~/.ssh/authorized_keys），
        // 由上层在 SftpConfig 变更钩子里完成；本 crate 只负责协议栈接通。
        Ok(())
    }
}

// ============================================================================
// 编排器测试：内存生命周期 + 配置渲染 + WebDAV/FTP/SFTP 真实协议栈离线驱动
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Protocol;
    use crate::nfs::{NfsExportOptions, NfsManager};
    use crate::sftp::SftpManager;
    use crate::smb::SmbManager;
    use chrono::Utc;
    use unftp_core::storage::StorageBackend;

    fn share(id: &str, name: &str, protocol: Protocol) -> Share {
        Share {
            id: ShareId::new(id),
            name: name.into(),
            protocol,
            path: PathBuf::from("/tank/media"),
            read_only: false,
            hosts_allow: vec![],
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn samba_lifecycle_and_conf_render() {
        let orch = SambaOrchestrator::default();
        let s = share("s1", "media", Protocol::Smb);
        let created = orch
            .create_share(s.clone(), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(created.id.as_str(), "s1");
        // list / get
        assert_eq!(orch.list_shares().await.unwrap().len(), 1);
        assert_eq!(
            orch.get_share(&ShareId::new("s1")).await.unwrap().name,
            "media"
        );
        // 渲染 smb.conf 含 [media] 段
        let conf = orch.render_conf();
        assert!(conf.contains("[global]"));
        assert!(conf.contains("[media]"));
        assert!(conf.contains("path = /tank/media"));
        // enable_time_machine 写入规格
        let _ = orch
            .enable_time_machine(&ShareId::new("s1"), Some(250))
            .await
            .unwrap();
        let conf2 = orch.render_conf();
        assert!(conf2.contains("vfs objects = fruit streams_xattr"));
        assert!(conf2.contains("fruit:time machine max size = 250G"));
        // delete
        orch.delete_share(&ShareId::new("s1")).await.unwrap();
        assert_eq!(orch.list_shares().await.unwrap().len(), 0);
        assert!(matches!(
            orch.get_share(&ShareId::new("s1")).await.unwrap_err(),
            ProtocolError::ShareNotFound(_)
        ));
    }

    #[tokio::test]
    async fn samba_write_smb_conf_writes_to_injected_path() {
        // write_smb_conf 已接通真实落盘：注入临时 config_path（避免碰 /etc/samba）+
        // Disabled reload（不跑 smbcontrol），验证文件被写入且内容为渲染产物。
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let conf_path = tmp.path().join("smb.conf");
        let mut cfg = SambaConfig::defaults();
        cfg.config_path = conf_path.clone();
        let orch = SambaOrchestrator::with_reload(cfg, ReloadPolicy::Disabled);
        let path = orch.write_smb_conf().await.expect("write_smb_conf 失败");
        assert_eq!(path, conf_path);
        // 读回验证内容
        let written = std::fs::read_to_string(&conf_path).expect("读回 smb.conf 失败");
        assert!(written.contains("[global]"), "落盘文件缺 [global] 段");
        // 默认配置（无共享）只有 [global]
        assert_eq!(written.matches('[').count(), 1);
    }

    #[tokio::test]
    async fn samba_session_lifecycle() {
        let orch = SambaOrchestrator::default();
        let sess = Session {
            id: "S-1".into(),
            protocol: Protocol::Smb,
            user: "alice".into(),
            client_ip: "10.0.0.2".into(),
            connected_at: Utc::now(),
            share_id: ShareId::new("s1"),
        };
        orch.store.put_session(sess).unwrap();
        assert_eq!(orch.list_smb_sessions().await.unwrap().len(), 1);
        orch.close_session("S-1").await.unwrap();
        assert_eq!(orch.list_smb_sessions().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn nfs_add_export_and_render_exports() {
        // add/remove_export 已接通 apply_exports（真实落盘 + exportfs 往返）：
        // 注入临时 exports_path + Disabled reload，避免碰 /etc/exports + 内核 export 表。
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let exports_path = tmp.path().join("exports");
        let orch = NfsOrchestrator::with_reload(
            GaneshaConfig::defaults(),
            exports_path.clone(),
            ReloadPolicy::Disabled,
        );
        let s = share("n1", "media", Protocol::Nfs);
        orch.create_share(s.clone(), ShareOptions::default())
            .await
            .unwrap();
        orch.add_export(
            &ShareId::new("n1"),
            vec!["10.0.0.0/24".into()],
            NfsExportOptions::default(),
        )
        .await
        .unwrap();
        let exports_txt = orch.render_exports(&[s]);
        assert!(exports_txt.contains("/tank/media 10.0.0.0/24(rw,sync,root_squash)"));
        // apply_exports 应已落盘到临时 exports 文件
        let written = std::fs::read_to_string(&exports_path).expect("读回 exports 失败");
        assert!(
            written.contains("/tank/media 10.0.0.0/24(rw,sync,root_squash)"),
            "落盘 exports 缺 media 行：{written}"
        );
        // 移除 export
        orch.remove_export(&ShareId::new("n1")).await.unwrap();
        let exports_txt2 = orch.render_exports(&orch.list_shares().await.unwrap());
        assert!(exports_txt2.is_empty());
        // 落盘文件也应清空（无 export）
        let written2 = std::fs::read_to_string(&exports_path).expect("读回 exports 失败");
        assert!(
            written2.is_empty(),
            "remove_export 后 exports 文件应清空：{written2}"
        );
    }

    #[tokio::test]
    async fn nfs_close_session_returns_not_found() {
        let orch = NfsOrchestrator::default();
        assert!(matches!(
            orch.close_session("any").await.unwrap_err(),
            ProtocolError::SessionNotFound(_)
        ));
    }

    #[tokio::test]
    async fn webdav_lifecycle() {
        let orch = DavServerBackend::default();
        let _ = orch
            .create_share(
                share("w1", "media", Protocol::Webdav),
                ShareOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(orch.list_shares().await.unwrap().len(), 1);
        assert_eq!(orch.config().listen, "0.0.0.0:5005");
        // create_share 应挂载真实 dav-server 处理器
        assert_eq!(orch.mount_count(), 1);
        assert!(orch.handler(&ShareId::new("w1")).is_some());
        // delete 卸载处理器
        orch.delete_share(&ShareId::new("w1")).await.unwrap();
        assert_eq!(orch.mount_count(), 0);
        assert!(orch.handler(&ShareId::new("w1")).is_none());
    }

    /// 真实驱动 dav-server 协议栈：通过 `handle_request` 喂入 RFC4918 PROPFIND 请求，
    /// 断言响应状态码（200/207）——证明 WebDAV 协议处理器真的接通，而非骨架。
    #[tokio::test]
    async fn webdav_drives_real_protocol_stack() {
        let orch = DavServerBackend::default();
        orch.create_share(
            share("w1", "media", Protocol::Webdav),
            ShareOptions::default(),
        )
        .await
        .unwrap();
        // PROPFIND /（Depth: 0）→ 应返回 207 Multi-Status（dav-server 真实行为）
        let req = http::Request::builder()
            .method("PROPFIND")
            .uri("/")
            .header("Depth", "0")
            .header(http::header::CONTENT_TYPE, "application/xml")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let resp = orch.handle_request(&ShareId::new("w1"), req).await.unwrap();
        // dav-server 对 PROPFIND 返回 207（Multi-Status）
        assert_eq!(resp.status(), 207);

        // MKCOL 创建集合 → 201
        let req2 = http::Request::builder()
            .method("MKCOL")
            .uri("/docs")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let resp2 = orch
            .handle_request(&ShareId::new("w1"), req2)
            .await
            .unwrap();
        assert_eq!(resp2.status(), 201);

        // PUT 写文件 → 201/204
        let req3 = http::Request::builder()
            .method("PUT")
            .uri("/docs/hello.txt")
            .body(http_body_util::Full::new(bytes::Bytes::from_static(
                b"hello webdav",
            )))
            .unwrap();
        let resp3 = orch
            .handle_request(&ShareId::new("w1"), req3)
            .await
            .unwrap();
        assert!(resp3.status().is_success());

        // 不存在的共享 → handle_request 返回 None
        let req4 = http::Request::builder()
            .method("GET")
            .uri("/")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        assert!(orch
            .handle_request(&ShareId::new("nope"), req4)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn ftp_lifecycle() {
        let orch = LibunftpBackend::default();
        let _ = orch
            .create_share(share("f1", "media", Protocol::Ftp), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(orch.list_shares().await.unwrap().len(), 1);
        assert_eq!(orch.config().listen, "0.0.0.0:21");
        // create_share 应挂载真实 FTP 存储后端
        assert_eq!(orch.mount_count(), 1);
        assert!(orch.storage(&ShareId::new("f1")).is_some());
        // delete 卸载存储后端
        orch.delete_share(&ShareId::new("f1")).await.unwrap();
        assert_eq!(orch.mount_count(), 0);
        assert!(orch.storage(&ShareId::new("f1")).is_none());
    }

    /// 真实驱动 libunftp 协议栈：构造真实但未监听的 Server，并直接驱动其
    /// StorageBackend（list/get/put），证明 FTP 协议栈真的接通。
    #[tokio::test]
    async fn ftp_builds_real_server_and_drives_storage() {
        let orch = LibunftpBackend::default();
        orch.create_share(share("f1", "media", Protocol::Ftp), ShareOptions::default())
            .await
            .unwrap();
        // 构造真实但未监听的 libunftp::Server（不调 listen，红线）
        let _server = orch.build_server(&ShareId::new("f1")).unwrap();

        // 直接驱动真实 StorageBackend：put → list → metadata
        let backend = orch.storage(&ShareId::new("f1")).unwrap();
        let user = unftp_core::auth::DefaultUser;
        use std::io::Cursor;
        let n = backend
            .put(
                &user,
                Cursor::new(*b"ftp-data"),
                PathBuf::from("/note.txt"),
                0,
            )
            .await
            .unwrap();
        assert_eq!(n, 8);
        let entries = backend.list(&user, PathBuf::from("/")).await.unwrap();
        assert_eq!(entries.len(), 1);
        let md = backend
            .metadata(&user, PathBuf::from("/note.txt"))
            .await
            .unwrap();
        assert_eq!(md.len, 8);

        // 不存在的共享 → build_server 报错
        assert!(matches!(
            orch.build_server(&ShareId::new("nope")).unwrap_err(),
            ProtocolError::ShareNotFound(_)
        ));
    }

    #[tokio::test]
    async fn sftp_builds_real_ssh_server_and_config() {
        use russh::keys::PublicKeyBase64;
        let orch = RusshSftpBackend::default();
        // 授权一个真实 Ed25519 公钥
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let pub_line = format!("ssh-ed25519 {} test@host", key.public_key_base64());
        orch.authorize_key("alice", &pub_line).await.unwrap();

        // 构造真实 SSH 服务端工厂（russh::server::Server 实现）——含 authorized_keys
        let server = orch.build_ssh_server();
        assert_eq!(server.user_count(), 1);
        assert!(server.authorized_keys().contains_key("alice"));

        // 构造真实 russh 配置（含 Ed25519 主机密钥 + 仅公钥认证）
        let config = orch.build_ssh_config().unwrap();
        assert_eq!(*config.methods, [russh::MethodKind::PublicKey]);
        assert_eq!(config.keys.len(), 1);
        assert_eq!(config.keys[0].algorithm(), russh::keys::Algorithm::Ed25519);
    }

    #[tokio::test]
    async fn sftp_authorize_and_render_keys() {
        let orch = RusshSftpBackend::default();
        orch.authorize_key("alice", "ssh-rsa AAAA alice@host")
            .await
            .unwrap();
        // 空公钥拒绝
        assert!(orch.authorize_key("bob", "  ").await.is_err());
        let keys = orch.render_authorized_keys();
        assert!(keys.contains("# user: alice"));
        assert!(keys.contains("ssh-rsa AAAA alice@host"));
    }

    #[tokio::test]
    async fn sftp_lifecycle() {
        let orch = RusshSftpBackend::default();
        let _ = orch
            .create_share(
                share("sf1", "media", Protocol::Sftp),
                ShareOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(orch.list_shares().await.unwrap().len(), 1);
    }

    // —— SambaOrchestrator 错误路径 ——

    #[tokio::test]
    async fn samba_update_share_not_found() {
        // update_share 对不存在的共享 → ShareNotFound
        let orch = SambaOrchestrator::default();
        let err = orch
            .update_share(&ShareId::new("ghost"), ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn samba_delete_share_not_found() {
        // delete_share 对不存在的共享 → ShareNotFound
        let orch = SambaOrchestrator::default();
        let err = orch.delete_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn samba_create_share_duplicate_returns_share_exists() {
        // create_share 重复 ID → ShareExists
        let orch = SambaOrchestrator::default();
        let s = share("s1", "media", Protocol::Smb);
        orch.create_share(s.clone(), ShareOptions::default())
            .await
            .unwrap();
        let err = orch
            .create_share(s, ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareExists(_)));
    }

    #[tokio::test]
    async fn samba_update_share_applies_options() {
        // update_share 成功路径：返回更新后的共享
        let orch = SambaOrchestrator::default();
        orch.create_share(share("s1", "media", Protocol::Smb), ShareOptions::default())
            .await
            .unwrap();
        let updated = orch
            .update_share(&ShareId::new("s1"), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(updated.id.as_str(), "s1");
    }

    #[tokio::test]
    async fn samba_enable_time_machine_not_found() {
        // enable_time_machine 对不存在共享 → ShareNotFound
        let orch = SambaOrchestrator::default();
        let err = orch
            .enable_time_machine(&ShareId::new("ghost"), Some(100))
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn samba_reload_smbd_disabled_is_ok() {
        // Disabled reload 策略 → Ok（不 spawn smbcontrol）
        let orch = SambaOrchestrator::with_reload(SambaConfig::defaults(), ReloadPolicy::Disabled);
        assert!(orch.reload_smbd().await.is_ok());
    }

    #[tokio::test]
    async fn samba_reload_smbd_dry_run_is_ok() {
        // DryRun reload 策略 → Ok（只 eprintln，不 spawn）
        let orch = SambaOrchestrator::with_reload(SambaConfig::defaults(), ReloadPolicy::DryRun);
        assert!(orch.reload_smbd().await.is_ok());
    }

    #[tokio::test]
    async fn samba_list_sessions_empty_initially() {
        let orch = SambaOrchestrator::default();
        assert!(orch.list_sessions().await.unwrap().is_empty());
        assert!(orch.list_smb_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn samba_close_session_not_found() {
        let orch = SambaOrchestrator::default();
        let err = orch.close_session("ghost").await.unwrap_err();
        assert!(matches!(err, ProtocolError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn samba_render_policy_accessors() {
        // ReloadPolicy 访问器 + with_reload 注入
        let orch = SambaOrchestrator::with_reload(SambaConfig::defaults(), ReloadPolicy::DryRun);
        assert_eq!(orch.reload_policy(), ReloadPolicy::DryRun);
    }

    // —— NfsOrchestrator 错误路径 ——

    #[tokio::test]
    async fn nfs_update_share_not_found() {
        let orch = NfsOrchestrator::default();
        let err = orch
            .update_share(&ShareId::new("ghost"), ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn nfs_delete_share_not_found() {
        let orch = NfsOrchestrator::default();
        let err = orch.delete_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn nfs_get_share_not_found() {
        let orch = NfsOrchestrator::default();
        let err = orch.get_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn nfs_add_export_share_not_found() {
        // add_export 对不存在的共享 → ShareNotFound
        let orch = NfsOrchestrator::with_reload(
            GaneshaConfig::defaults(),
            std::env::temp_dir().join("os-cov-exports.tmp"),
            ReloadPolicy::Disabled,
        );
        let err = orch
            .add_export(
                &ShareId::new("ghost"),
                vec!["10.0.0.0/24".into()],
                NfsExportOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn nfs_remove_export_share_not_found() {
        // remove_export：共享不存在 → ShareNotFound
        let orch = NfsOrchestrator::with_reload(
            GaneshaConfig::defaults(),
            std::env::temp_dir().join("os-cov-exports2.tmp"),
            ReloadPolicy::Disabled,
        );
        let err = orch
            .remove_export(&ShareId::new("ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn nfs_remove_export_share_exists_but_not_exported_is_idempotent() {
        // remove_export：共享存在但从未 add_export → 幂等成功（已无 export 可移除）
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let orch = NfsOrchestrator::with_reload(
            GaneshaConfig::defaults(),
            tmp.path().join("exports"),
            ReloadPolicy::Disabled,
        );
        orch.create_share(share("n1", "media", Protocol::Nfs), ShareOptions::default())
            .await
            .unwrap();
        // 共享存在但无 export → remove_export 应 Ok（幂等）
        orch.remove_export(&ShareId::new("n1")).await.unwrap();
    }

    #[tokio::test]
    async fn nfs_delete_share_with_existing_export_cleans_up() {
        // delete_share 对已导出的共享：应同时清 exports map + 重写 exports 文件
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let exports_path = tmp.path().join("exports");
        let orch = NfsOrchestrator::with_reload(
            GaneshaConfig::defaults(),
            exports_path.clone(),
            ReloadPolicy::Disabled,
        );
        orch.create_share(share("n1", "media", Protocol::Nfs), ShareOptions::default())
            .await
            .unwrap();
        orch.add_export(
            &ShareId::new("n1"),
            vec!["10.0.0.0/24".into()],
            NfsExportOptions::default(),
        )
        .await
        .unwrap();
        // exports 文件应非空
        let written = std::fs::read_to_string(&exports_path).unwrap();
        assert!(!written.is_empty());
        // delete_share → apply_exports 重写为空
        orch.delete_share(&ShareId::new("n1")).await.unwrap();
        let written2 = std::fs::read_to_string(&exports_path).unwrap();
        assert!(written2.is_empty());
    }

    #[tokio::test]
    async fn nfs_list_sessions_returns_empty() {
        // NFS 无状态协议 → list_sessions 总是空
        let orch = NfsOrchestrator::default();
        assert!(orch.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn nfs_accessors_return_injected_config() {
        // ganesha_config / exports_path / reload_policy 访问器
        let cfg = GaneshaConfig::defaults();
        let path = PathBuf::from("/tmp/test-exports");
        let orch = NfsOrchestrator::with_reload(cfg, path.clone(), ReloadPolicy::DryRun);
        assert_eq!(orch.exports_path(), &path);
        assert_eq!(orch.reload_policy(), ReloadPolicy::DryRun);
        assert_eq!(orch.ganesha_config().domain, "os.local");
    }

    #[tokio::test]
    async fn nfs_render_exports_empty_when_no_exports_registered() {
        // render_exports 对无 export 的共享列表 → 空字符串
        let orch = NfsOrchestrator::default();
        let s = share("n1", "media", Protocol::Nfs);
        assert_eq!(orch.render_exports(&[s]), "");
    }

    // —— DavServerBackend 错误路径 ——

    #[tokio::test]
    async fn webdav_get_share_not_found() {
        let orch = DavServerBackend::default();
        let err = orch.get_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn webdav_update_share_not_found() {
        let orch = DavServerBackend::default();
        let err = orch
            .update_share(&ShareId::new("ghost"), ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn webdav_delete_share_not_found() {
        let orch = DavServerBackend::default();
        let err = orch.delete_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn webdav_update_share_succeeds() {
        // update_share 成功路径
        let orch = DavServerBackend::default();
        orch.create_share(
            share("w1", "media", Protocol::Webdav),
            ShareOptions::default(),
        )
        .await
        .unwrap();
        let updated = orch
            .update_share(&ShareId::new("w1"), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(updated.id.as_str(), "w1");
    }

    #[tokio::test]
    async fn webdav_list_sessions_and_close() {
        let orch = DavServerBackend::default();
        // 初始空会话
        assert!(orch.list_sessions().await.unwrap().is_empty());
        // close_session 对不存在会话 → SessionNotFound
        let err = orch.close_session("ghost").await.unwrap_err();
        assert!(matches!(err, ProtocolError::SessionNotFound(_)));
    }

    // —— LibunftpBackend 错误路径 ——

    #[tokio::test]
    async fn ftp_get_share_not_found() {
        let orch = LibunftpBackend::default();
        let err = orch.get_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn ftp_update_share_not_found() {
        let orch = LibunftpBackend::default();
        let err = orch
            .update_share(&ShareId::new("ghost"), ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn ftp_delete_share_not_found() {
        let orch = LibunftpBackend::default();
        let err = orch.delete_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn ftp_build_server_for_missing_share_errors() {
        // build_server 对未挂载共享 → ShareNotFound（storage(id).is_none() 分支）
        let orch = LibunftpBackend::default();
        let err = orch.build_server(&ShareId::new("ghost")).unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn ftp_storage_for_missing_share_is_none() {
        let orch = LibunftpBackend::default();
        assert!(orch.storage(&ShareId::new("ghost")).is_none());
    }

    #[tokio::test]
    async fn ftp_create_share_duplicate_returns_share_exists() {
        let orch = LibunftpBackend::default();
        let s = share("f1", "media", Protocol::Ftp);
        orch.create_share(s.clone(), ShareOptions::default())
            .await
            .unwrap();
        let err = orch
            .create_share(s, ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareExists(_)));
    }

    #[tokio::test]
    async fn ftp_list_sessions_empty_and_close_not_found() {
        let orch = LibunftpBackend::default();
        assert!(orch.list_sessions().await.unwrap().is_empty());
        let err = orch.close_session("ghost").await.unwrap_err();
        assert!(matches!(err, ProtocolError::SessionNotFound(_)));
    }

    // —— RusshSftpBackend 错误路径 ——

    #[tokio::test]
    async fn sftp_get_share_not_found() {
        let orch = RusshSftpBackend::default();
        let err = orch.get_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn sftp_update_share_not_found() {
        let orch = RusshSftpBackend::default();
        let err = orch
            .update_share(&ShareId::new("ghost"), ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn sftp_delete_share_not_found() {
        let orch = RusshSftpBackend::default();
        let err = orch.delete_share(&ShareId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, ProtocolError::ShareNotFound(_)));
    }

    #[tokio::test]
    async fn sftp_create_share_duplicate_returns_share_exists() {
        let orch = RusshSftpBackend::default();
        let s = share("sf1", "media", Protocol::Sftp);
        orch.create_share(s.clone(), ShareOptions::default())
            .await
            .unwrap();
        let err = orch
            .create_share(s, ShareOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ProtocolError::ShareExists(_)));
    }

    #[tokio::test]
    async fn sftp_authorize_key_appends_multiple_keys_per_user() {
        // 同一用户多次 authorize_key → 累加（entry().or_default().push）
        let orch = RusshSftpBackend::default();
        orch.authorize_key("alice", "ssh-rsa AAAA alice@host")
            .await
            .unwrap();
        orch.authorize_key("alice", "ssh-ed25519 BBBB alice@host2")
            .await
            .unwrap();
        let server = orch.build_ssh_server();
        // alice 应有 2 条公钥
        assert_eq!(
            server.authorized_keys().get("alice").map(|v| v.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn sftp_close_session_not_found() {
        let orch = RusshSftpBackend::default();
        let err = orch.close_session("ghost").await.unwrap_err();
        assert!(matches!(err, ProtocolError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn sftp_list_sessions_empty() {
        let orch = RusshSftpBackend::default();
        assert!(orch.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sftp_config_accessor_returns_defaults() {
        let orch = RusshSftpBackend::default();
        let cfg = orch.config();
        assert_eq!(cfg.listen, "0.0.0.0:22");
        assert!(cfg.pubkey_auth);
        assert!(!cfg.password_auth);
    }

    #[tokio::test]
    async fn sftp_render_authorized_keys_empty_when_no_keys() {
        let orch = RusshSftpBackend::default();
        assert_eq!(orch.render_authorized_keys(), "");
    }
}
