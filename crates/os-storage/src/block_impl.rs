//! `LioBlockExport` —— 通过内核 LIO（iSCSI）+ nvmet（NVMe-oF）实现 [`crate::BlockExport`]。
//!
//! 决策依据：规划文档 §9.1#11——块 export 由 os-storage 统管，不依赖外部 targetd/SCST。
//!
//! 实现路径（CLI/configfs 编排）：
//! - **iSCSI**：LIO configfs（`/sys/kernel/config/target/iscsi/`）+ `targetcli` CLI 编排
//!   target/tpg/lun。**portal** 由 LIO 默认 `auto_add_default_portal=true` 自动建
//!   （`/iscsi create` 阶段即建 0.0.0.0:3260），故本实现不显式建 portal——显式建会因
//!   「NetworkPortal already exists」退非零而 ExportFailed。
//! - **NVMe-oF**：nvmet configfs（`/sys/kernel/config/nvmet/`）+ `nvmetcli`。
//!
//! volume 名（`tank/vol0`）含 `/`，但 LIO backstore 名与 IQN/NQN name 段不允许 `/`，
//! 故 backstore 名与 WWN 后缀统一经 [`sanitize_name`] 把 `/` → `-`（`tank-vol0`）。
//!
//! 当前为骨架：命令构造真实可测（构造 targetcli/nvmetcli 参数），执行经
//! [`crate::backend_impl::CommandRunner`]，状态维护在内存 map（export 注册表）。
//! configfs 直写与持久化（`saveconfig.json`）留 TODO(集成阶段)。
//!
//! 权限：configfs/targetcli/nvmetcli 操作需 **root**（写 `/sys/kernel/config/`）。

use crate::backend_impl::CommandRunner;
use crate::block::{BlockExport, IscsiTarget, NvmeofNamespace};
use crate::error::StorageError;
use os_core::{CommandOutput, VolumeId};
use std::collections::HashMap;
use std::sync::Mutex;

/// 把 ZFS volume 名（`tank/vol0`）里的 `/` 替换为 `-`（`tank-vol0`）。
///
/// LIO backstore 名与 IQN/NQN 的 name 段都不能含 `/`（rtslib 会判名字非法），
/// 而 ZFS volume 名必含 `/`。统一用此函数规范化，保证 backstore 名 `vol-<sanitized>`
/// 与 WWN 后缀 `vol-<sanitized>-lun<N>` 在 LIO/nvmet 都合法。
fn sanitize_name(volume: &VolumeId) -> String {
    volume.as_str().replace('/', "-")
}

/// LIO/nvmet 块 export 管理器（默认 [`crate::BlockExport`] 实现）。
///
/// 内部维护 export 注册表（IQN/NQN → target），便于 `list_exports`/`unexport` 不依赖
/// configfs 反查。生产部署需保证本进程单例持有该注册表（多实例需走共享存储，留 TODO）。
pub struct LioBlockExport {
    runner: Box<dyn CommandRunner>,
    iqn_base: String,
    nqn_base: String,
    iscsi_targets: Mutex<HashMap<String, IscsiTarget>>,
    nvmeof_namespaces: Mutex<HashMap<String, NvmeofNamespace>>,
}

impl LioBlockExport {
    /// 生产构造。`iqn_base`/`nqn_base` 是生成 target 标识的前缀（如
    /// `iqn.2026-08.example.os` / `nqn.2026-08.example.os`）。
    pub fn new(iqn_base: impl Into<String>, nqn_base: impl Into<String>) -> Self {
        Self::with_runner(
            Box::new(crate::backend_impl::TokioCommandRunner),
            iqn_base,
            nqn_base,
        )
    }

    /// 测试构造——注入 runner。
    pub fn with_runner(
        runner: Box<dyn CommandRunner>,
        iqn_base: impl Into<String>,
        nqn_base: impl Into<String>,
    ) -> Self {
        Self {
            runner,
            iqn_base: iqn_base.into(),
            nqn_base: nqn_base.into(),
            iscsi_targets: Mutex::new(HashMap::new()),
            nvmeof_namespaces: Mutex::new(HashMap::new()),
        }
    }

    /// 生成 iSCSI target IQN（基于 volume + lun）。
    ///
    /// 注意：IQN 的 name 段不能含 `/`（rtslib 会判 WWN 非法），而 ZFS volume 名必含 `/`
    /// （如 `tank/vol0`），故把 volume 里的 `/` 替换为 `-`（`tank/vol0` → `vol-tank-vol0`）。
    /// IQN 反向域名段（`iqn_base` 的 `2026-08.<...>` 部分）需至少含一个 `.`，调用方负责。
    fn make_iqn(&self, volume: &VolumeId, lun_id: u32) -> String {
        format!(
            "{}:vol-{}-lun{}",
            self.iqn_base,
            sanitize_name(volume),
            lun_id
        )
    }

    /// 生成 NVMe-oF subsystem NQN（基于 volume）。同样把 volume 的 `/` 替换为 `-`。
    fn make_nqn(&self, volume: &VolumeId) -> String {
        format!("{}:vol-{}", self.nqn_base, sanitize_name(volume))
    }

    async fn exec_targetcli(&self, args: &[String]) -> Result<CommandOutput, StorageError> {
        let out = self.runner.run("targetcli", args).await?;
        if out.exit_code != 0 {
            return Err(StorageError::ExportFailed(format!(
                "targetcli {:?} 退出码 {}：{}",
                args.join(" "),
                out.exit_code,
                out.stderr.trim()
            )));
        }
        Ok(out)
    }

    async fn exec_nvmetcli(&self, args: &[String]) -> Result<CommandOutput, StorageError> {
        let out = self.runner.run("nvmetcli", args).await?;
        if out.exit_code != 0 {
            return Err(StorageError::ExportFailed(format!(
                "nvmetcli {:?} 退出码 {}：{}",
                args.join(" "),
                out.exit_code,
                out.stderr.trim()
            )));
        }
        Ok(out)
    }
}

impl Default for LioBlockExport {
    fn default() -> Self {
        Self::new("iqn.2026-08.example.os", "nqn.2026-08.example.os")
    }
}

impl BlockExport for LioBlockExport {
    async fn export_iscsi(
        &self,
        volume: &VolumeId,
        lun_id: u32,
        initiators: Vec<String>,
    ) -> Result<IscsiTarget, StorageError> {
        let iqn = self.make_iqn(volume, lun_id);
        // targetcli 编排：LIO 的标准流程是
        //   ① 建 backstore（block backend，名为 vol-<sanitized-volume>，指向 zvol 块设备）
        //   ② 建 iSCSI target（/iscsi create <iqn>，targetcli 自动建 tpg1 + 默认 portal）
        //   ③ 把 backstore 作为 LUN 映射到 target 的 tpg1/luns
        //   ④ portal（默认 0.0.0.0:3260）：**省略**——LIO 默认 `auto_add_default_portal=true`，
        //      `/iscsi create` 已自动建默认 portal；显式 `portals create` 会因 "already exists"
        //      退非零（exit 1）触发 ExportFailed，故不重发。要自定义 portal 地址的部署，
        //      须先 `set global auto_add_default_portal=false` 再显式建——当前 trait 不带
        //      portal 地址参数，默认行为足够。
        // backstore 名固定为 vol-<sanitized-volume>（`/` → `-`），便于 unexport 反向删除——
        // 之前用 `vol-<volume>`（含 `/`）会被 targetcli 判「name cannot contain /」拒收。
        let zvol_path = format!("/dev/zvol/{}", volume);
        let backstore = format!("vol-{}", sanitize_name(volume));
        let args = vec![
            format!("/backstores/block create {backstore} {zvol_path}"),
            format!("/iscsi create {iqn}"),
            format!("/iscsi/{iqn}/tpg1/luns create /backstores/block/{backstore}"),
        ];
        for a in args {
            self.exec_targetcli(&[a]).await?;
        }
        // initiator ACL（若指定）
        if !initiators.is_empty() {
            let _ = self
                .exec_targetcli(&[format!(
                    "/iscsi/{iqn}/tpg1/acls create {}",
                    initiators.join(" ")
                )])
                .await;
            // ACL 失败不致命（可后置配置），忽略错误继续
        }

        let target = IscsiTarget {
            iqn: iqn.clone(),
            volume: volume.clone(),
            lun_id,
            initiators,
            listen: "0.0.0.0:3260".to_string(),
        };
        self.iscsi_targets
            .lock()
            .expect("iscsi_targets poisoned")
            .insert(iqn, target.clone());
        Ok(target)
    }

    async fn export_nvmeof(
        &self,
        volume: &VolumeId,
        nqn: &str,
    ) -> Result<NvmeofNamespace, StorageError> {
        // nqn 由调用方指定（ trait 签名），但若调用方传空则用默认生成
        let nqn = if nqn.is_empty() {
            self.make_nqn(volume)
        } else {
            nqn.to_string()
        };
        let zvol_path = format!("/dev/zvol/{}", volume);
        let args = vec![
            format!("create subsystem {nqn}"),
            format!("create namespace {nqn} -b {zvol_path}"),
            format!("create host {nqn} -n '*'"),
        ];
        for a in args {
            self.exec_nvmetcli(&[a]).await?;
        }
        let ns = NvmeofNamespace {
            nqn: nqn.clone(),
            volume: volume.clone(),
            nsid: 1,
            hosts: vec!["*".to_string()],
            transport_addr: "0.0.0.0:4420".to_string(),
        };
        self.nvmeof_namespaces
            .lock()
            .expect("nvmeof_namespaces poisoned")
            .insert(nqn, ns.clone());
        Ok(ns)
    }

    async fn unexport(&self, target_id: &str) -> Result<(), StorageError> {
        // iSCSI target 与 NVMe-oF namespace 共用 unexport（按 target_id 即 IQN/NQN 查找）。
        // 注意：std::sync::MutexGuard 不是 Send，持有它跨 `.await` 会触发 clippy
        // `await_holding_lock`（且可能在 future 挂起时死锁）。这里把「在锁内确定目标类型 +
        // 取销毁所需的派生信息」与「出锁后执行 CLI」严格分离：先在块内拿锁判定 + remove，
        // 块结束 guard 自动 drop，再执行 await。
        /// 锁内判定的卸载动作（决定后续调哪个 CLI，并携带销毁所需的派生信息）。
        enum UnexportKind {
            // iSCSI: 携带 backstore 名（`vol-<volume>`）——create 时建的 block backstore，
            // unexport 需连带删除（targetcli 删 target 不级联删 backstore）。
            Iscsi { backstore: String },
            Nvmeof,
        }
        let kind = {
            let mut iscsi = self.iscsi_targets.lock().expect("iscsi_targets poisoned");
            let mut nvmeof = self
                .nvmeof_namespaces
                .lock()
                .expect("nvmeof_namespaces poisoned");
            if let Some(t) = iscsi.remove(target_id) {
                Some(UnexportKind::Iscsi {
                    backstore: format!("vol-{}", sanitize_name(&t.volume)),
                })
            } else if nvmeof.remove(target_id).is_some() {
                Some(UnexportKind::Nvmeof)
            } else {
                None
            }
        }; // <- 两个 MutexGuard 在此 drop，后续 await 不持锁
        match kind {
            Some(UnexportKind::Iscsi { backstore }) => {
                // unexport 是 create 的逆操作：先删 iSCSI target，再删 backstore。
                // configfs/targetcli 删除失败不致命（注册表已移除，可能对象本就不存在），
                // 忽略错误继续——返回 Ok 让上层幂等清理。
                let _ = self
                    .exec_targetcli(&[format!("/iscsi delete {target_id}")])
                    .await;
                let _ = self
                    .exec_targetcli(&[format!("/backstores/block delete {backstore}")])
                    .await;
                Ok(())
            }
            Some(UnexportKind::Nvmeof) => {
                let _ = self
                    .exec_nvmetcli(&[format!("delete subsystem {target_id}")])
                    .await;
                Ok(())
            }
            None => Err(StorageError::ExportFailed(format!(
                "未找到 export：{target_id}"
            ))),
        }
    }

    async fn list_exports(&self) -> Result<(Vec<IscsiTarget>, Vec<NvmeofNamespace>), StorageError> {
        // 从内存注册表返回（生产应反查 configfs，留 TODO(集成阶段)）
        let iscsi = self
            .iscsi_targets
            .lock()
            .expect("iscsi_targets poisoned")
            .values()
            .cloned()
            .collect();
        let nvmeof = self
            .nvmeof_namespaces
            .lock()
            .expect("nvmeof_namespaces poisoned")
            .values()
            .cloned()
            .collect();
        Ok((iscsi, nvmeof))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct OkRunner;
    #[async_trait]
    impl CommandRunner for OkRunner {
        async fn run(&self, _p: &str, _a: &[String]) -> Result<CommandOutput, StorageError> {
            Ok(CommandOutput::ok())
        }
    }

    #[tokio::test]
    async fn export_iscsi_registers_and_lists() {
        let be =
            LioBlockExport::with_runner(Box::new(OkRunner), "iqn.2026-08.test", "nqn.2026-08.test");
        let t = be
            .export_iscsi(
                &VolumeId::new("tank/vol0"),
                0,
                vec!["iqn.1998-01.initiator".into()],
            )
            .await
            .unwrap();
        assert!(t.iqn.starts_with("iqn.2026-08.test:vol-"));
        assert_eq!(t.lun_id, 0);
        let (iscsi, nvmeof) = be.list_exports().await.unwrap();
        assert_eq!(iscsi.len(), 1);
        assert!(nvmeof.is_empty());
    }

    #[tokio::test]
    async fn export_nvmeof_with_explicit_nqn() {
        let be =
            LioBlockExport::with_runner(Box::new(OkRunner), "iqn.2026-08.test", "nqn.2026-08.test");
        let ns = be
            .export_nvmeof(&VolumeId::new("tank/vol1"), "nqn.custom:ns1")
            .await
            .unwrap();
        assert_eq!(ns.nqn, "nqn.custom:ns1");
    }

    #[tokio::test]
    async fn unexport_iscsi_then_missing() {
        let be =
            LioBlockExport::with_runner(Box::new(OkRunner), "iqn.2026-08.test", "nqn.2026-08.test");
        let t = be
            .export_iscsi(&VolumeId::new("tank/vol0"), 0, Vec::new())
            .await
            .unwrap();
        be.unexport(&t.iqn).await.unwrap();
        let (iscsi, _) = be.list_exports().await.unwrap();
        assert!(iscsi.is_empty());
        // 二次 unexport 报 ExportFailed
        let err = be.unexport(&t.iqn).await.unwrap_err();
        assert!(matches!(err, StorageError::ExportFailed(_)));
    }
}
