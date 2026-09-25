//! 阶段2 传输编排骨架（规划文档 §3.10 阶段2 / §3.19）
//!
//! 数据集内容走 ZFS send/recv（增量），由 [`os_storage::Replication`] trait 执行；
//! 配置/共享/用户定义走迁移包（见 [`crate::package`]）。本模块是**编排骨架**：
//! 给定 `MigrationPlan` + 节点信息，生成传输计划（哪些数据集怎么传、增量基线、
//! 配置包导出/导入步骤），不真跑 zfs send/recv（红线）。
//!
//! 设计：
//! - [`TransferPlan`]：纯数据结构，描述"该传哪些数据集（按何种基线）+ 配置包步骤"。
//! - [`TransferPlanner`]：纯逻辑生成器，从 `MigrationPlan` + 节点信息推出 [`TransferPlan`]。
//! - [`TransferCommand`]：单个数据集传输的命令行骨架（可下发给真实 ssh 管道或本地管道）。
//!
//! 真执行由 `ZfsMigrationEngine`（在 `MigrationEngine` trait 实现中）注入 `Replication`，
//! 逐数据集调 `send`/`recv` 并推进 checkpoint；本模块只产"做什么"的计划。

use os_core::DatasetId;
use serde::{Deserialize, Serialize};

use crate::checkpoint::TransferredFile;
use crate::error::ProvisionError;
use crate::migration::MigrationPlan;

// ----------------------------------------------------------------------------
// 传输参数
// ----------------------------------------------------------------------------

/// 阶段2 传输编排骨架的输入参数。
///
/// 由 `ZfsMigrationEngine` 在 `execute` 阶段构造：
/// - `plan` 来自 `MigrationEngine::plan` 产出
/// - `source_ssh` / `target_ssh` 是节点的 ssh 端点（zfs send 管道两端）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferParams {
    /// 关联迁移计划
    pub plan: MigrationPlan,
    /// 源节点 ssh 端点（`user@host` 或 `ssh://user@host:port`）
    pub source_ssh: String,
    /// 目标节点 ssh 端点（同上）
    pub target_ssh: String,
    /// 增量基线快照（None = 全量传输；Some = 从该快照增量）
    /// 呼应 [`os_storage::ReplicationConfig::source`] 的快照语义
    pub baseline_snapshot: Option<String>,
    /// 是否启用 mbuffer 中转（大流量建议开）
    pub use_mbuffer: bool,
    /// 是否启用传输压缩（LZ4 流压缩，与数据集 native encryption 独立）
    pub compress_stream: bool,
}

impl TransferParams {
    /// 从 [`MigrationPlan`] + ssh 端点构造（默认全量、压缩、无 mbuffer）。
    pub fn from_plan(
        plan: MigrationPlan,
        source_ssh: impl Into<String>,
        target_ssh: impl Into<String>,
    ) -> Self {
        Self {
            plan,
            source_ssh: source_ssh.into(),
            target_ssh: target_ssh.into(),
            baseline_snapshot: None,
            use_mbuffer: false,
            compress_stream: true,
        }
    }
}

// ----------------------------------------------------------------------------
// 传输计划
// ----------------------------------------------------------------------------

/// 单个数据集的传输命令骨架（喂给 ssh 管道或本地管道执行）。
///
/// 命令字符串是骨架（带 `# TODO` 标注真实管道集成点）；执行前可参数化替换。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferCommand {
    /// 数据集名
    pub dataset: DatasetId,
    /// zfs send 端命令（源端执行）
    pub send_cmd: String,
    /// zfs recv 端命令（目标端执行）
    pub recv_cmd: String,
    /// 是否增量（baseline_snapshot != None）
    pub incremental: bool,
}

/// 阶段2 传输计划——给定节点 + 数据集，推出"传哪些 / 怎么传 / 配置包步骤"。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPlan {
    /// 数据集传输命令清单（每条对应一个 zfs send/recv 管道）
    pub dataset_commands: Vec<TransferCommand>,
    /// 配置包导出步骤的命令骨架（源端：tar/zstd 打包前过滤敏感项）
    pub package_export_cmd: String,
    /// 配置包导入步骤的命令骨架（目标端：解包后导入）
    pub package_import_cmd: String,
    /// 预估总字节数（None = 未知，需运行时探测）
    pub estimated_bytes: Option<u64>,
}

// ----------------------------------------------------------------------------
// 编排器（纯逻辑）
// ----------------------------------------------------------------------------

/// 传输编排骨架——从 [`TransferParams`] 推 [`TransferPlan`]。
///
/// 不真跑 zfs send/recv（红线）；只生成命令字符串与计划。
/// 增量/全量分支由 `baseline_snapshot` 决定；压缩/mbuffer 由 flag 切换。
#[derive(Debug, Clone, Default)]
pub struct TransferPlanner;

impl TransferPlanner {
    /// 生成传输计划。
    pub fn plan(params: &TransferParams) -> TransferPlan {
        let mut commands = Vec::with_capacity(params.plan.datasets.len());
        for ds in &params.plan.datasets {
            commands.push(Self::build_dataset_command(params, ds));
        }

        let package_export_cmd = Self::build_package_export(params);
        let package_import_cmd = Self::build_package_import(params);

        TransferPlan {
            dataset_commands: commands,
            package_export_cmd,
            package_import_cmd,
            estimated_bytes: None, // 运行时探测，规划阶段未知
        }
    }

    /// 生成单个数据集的传输命令（send 端 + recv 端）。
    pub fn build_dataset_command(params: &TransferParams, dataset: &DatasetId) -> TransferCommand {
        let incremental = params.baseline_snapshot.is_some();
        let send_cmd = Self::build_send_cmd(params, dataset);
        let recv_cmd = Self::build_recv_cmd(params, dataset);
        TransferCommand {
            dataset: dataset.clone(),
            send_cmd,
            recv_cmd,
            incremental,
        }
    }

    /// 生成 `zfs send` 端命令（源节点执行）。
    pub fn build_send_cmd(params: &TransferParams, dataset: &DatasetId) -> String {
        let snap_full = format!("{}@migrate", dataset.as_str());
        let snap_base = params.baseline_snapshot.as_deref().unwrap_or("");
        let incremental = params.baseline_snapshot.is_some();
        let mut s = String::new();
        s.push_str("# 由 os-provision::transfer 生成——源端 zfs send\n");
        if params.compress_stream {
            s.push_str("# 注：LZ4 流压缩（中间链路），与数据集 native encryption 独立\n");
        }
        if incremental {
            s.push_str(&format!("zfs send -I {} {}", snap_base, snap_full));
        } else {
            s.push_str(&format!("zfs send {}", snap_full));
        }
        if params.compress_stream {
            s.push_str(" | lz4c -1");
        }
        if params.use_mbuffer {
            s.push_str(" | mbuffer -m 1G");
        }
        s.push('\n');
        s
    }

    /// 生成 `zfs recv` 端命令（目标节点执行）。
    pub fn build_recv_cmd(params: &TransferParams, dataset: &DatasetId) -> String {
        let mut s = String::new();
        s.push_str("# 由 os-provision::transfer 生成——目标端 zfs recv\n");
        if params.use_mbuffer {
            s.push_str("mbuffer -m 1G | ");
        }
        if params.compress_stream {
            s.push_str("lz4c -d | ");
        }
        // -F 允许覆盖（迁移场景目标池是空的，但容忍已有快照）
        s.push_str(&format!("zfs recv -F {}\n", dataset.as_str()));
        s
    }

    /// 生成配置包导出命令（源端：tar + 排除清单 + zstd）。
    ///
    /// 呼应 §3.19：导出前用 [`crate::exclude::ExcludeRules`] 过滤敏感项，
    /// 打包脚本里硬编码 `--exclude` 列表（从 plan.exclude_keys 派生）。
    pub fn build_package_export(params: &TransferParams) -> String {
        let mut s = String::new();
        s.push_str("# 由 os-provision::transfer 生成——配置包导出（源端）\n");
        s.push_str("# §3.19：导出前过滤敏感项（JWT/TLS/SSH/钱包/集群密钥等）\n");
        s.push_str("set -euo pipefail\n");
        s.push_str("PKG_DIR=\"$(mktemp -d)\"\n");
        s.push_str("mkdir -p \"$PKG_DIR/etc\" \"$PKG_DIR/share\" \"$PKG_DIR/users\"\n");
        s.push_str("# 拷贝配置/共享/用户定义，按 §3.19 排除清单过滤\n");
        for ek in &params.plan.exclude_keys {
            s.push_str(&format!("EXCLUDE_PATTERNS+=( \"{}\" )\n", ek));
        }
        s.push_str("tar --exclude-from=<(printf '%s\\n' \"${EXCLUDE_PATTERNS[@]}\") \\\n");
        s.push_str("    -cf - /etc/os /etc/samba /var/lib/os/users 2>/dev/null \\\n");
        s.push_str("    | zstd -19 -T0 -o migrate-pkg.tar.zst\n");
        s.push_str("echo [os-provision] 配置包导出完成：migrate-pkg.tar.zst\n");
        s.push_str(
            "# TODO: 调 os-provision::package::MigrationPackage::pack_to_bytes 做 JSON 校验\n",
        );
        s
    }

    /// 生成配置包导入命令（目标端：zstd 解压 + tar 解包）。
    pub fn build_package_import(_params: &TransferParams) -> String {
        let mut s = String::new();
        s.push_str("# 由 os-provision::transfer 生成——配置包导入（目标端）\n");
        s.push_str("set -euo pipefail\n");
        s.push_str("# §3.19：导入前再过一遍排除清单（防御性，防止包内残留敏感项）\n");
        s.push_str("zstd -d -c migrate-pkg.tar.zst \\\n");
        s.push_str("    | tar -xf - -C / --keep-newer-files\n");
        s.push_str("echo [os-provision] 配置包导入完成\n");
        s.push_str("# TODO: 调 os-provision::package::MigrationPackage::audit 做安全自检\n");
        s
    }

    /// 估算总字节数（基于已传文件记录的 size 累加）。
    ///
    /// 若迁移 checkpoint 有已传记录，可基于此估算进度；本骨架返回 None（运行时探测）。
    pub fn estimate_bytes(
        _params: &TransferParams,
        _transferred: &[TransferredFile],
    ) -> Option<u64> {
        None
    }

    /// 计算传输进度（已传字节数 / 总字节数）。
    ///
    /// 纯逻辑：基于 checkpoint 已传记录 + 估算总数。返回 0.0–1.0。
    pub fn progress(transferred: &[TransferredFile], total: Option<u64>) -> f32 {
        let done: u64 = transferred.iter().map(|f| f.size).sum();
        match total {
            Some(t) if t > 0 => (done as f32 / t as f32).min(1.0),
            _ => 0.0,
        }
    }

    /// 验证传输计划参数合法性（诊断用，不 panic）。
    pub fn validate(params: &TransferParams) -> Result<(), ProvisionError> {
        if params.source_ssh.trim().is_empty() {
            return Err(ProvisionError::InvalidConfig("source_ssh 不能为空".into()));
        }
        if params.target_ssh.trim().is_empty() {
            return Err(ProvisionError::InvalidConfig("target_ssh 不能为空".into()));
        }
        if params.plan.source_node == params.plan.target_node {
            return Err(ProvisionError::InvalidConfig(
                "source_node 与 target_node 不能相同".into(),
            ));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::NodeId;

    fn sample_plan() -> MigrationPlan {
        MigrationPlan {
            source_node: NodeId::new("node-a"),
            target_node: NodeId::new("node-b"),
            datasets: vec![DatasetId::new("tank/media"), DatasetId::new("tank/photos")],
            exclude_keys: vec!["/etc/shadow".into(), "/etc/os/jwt-signing.key".into()],
            resume_point: None,
        }
    }

    fn sample_params() -> TransferParams {
        TransferParams::from_plan(sample_plan(), "root@10.0.0.2", "root@10.0.0.3")
    }

    #[test]
    fn from_plan_defaults() {
        let p = sample_params();
        assert_eq!(p.source_ssh, "root@10.0.0.2");
        assert_eq!(p.target_ssh, "root@10.0.0.3");
        assert!(p.baseline_snapshot.is_none());
        assert!(!p.use_mbuffer);
        assert!(p.compress_stream);
    }

    #[test]
    fn plan_has_command_per_dataset() {
        let p = sample_params();
        let plan = TransferPlanner::plan(&p);
        assert_eq!(plan.dataset_commands.len(), 2);
        assert_eq!(plan.dataset_commands[0].dataset.as_str(), "tank/media");
        assert_eq!(plan.dataset_commands[1].dataset.as_str(), "tank/photos");
    }

    #[test]
    fn send_cmd_full_no_baseline() {
        let p = sample_params();
        let ds = DatasetId::new("tank/media");
        let cmd = TransferPlanner::build_send_cmd(&p, &ds);
        assert!(cmd.contains("zfs send tank/media@migrate"));
        assert!(cmd.contains("lz4c -1"));
        assert!(!cmd.contains("-I"));
        // 不含 mbuffer
        assert!(!cmd.contains("mbuffer"));
    }

    #[test]
    fn send_cmd_incremental() {
        let mut p = sample_params();
        p.baseline_snapshot = Some("tank/media@base".into());
        let ds = DatasetId::new("tank/media");
        let cmd = TransferPlanner::build_send_cmd(&p, &ds);
        assert!(cmd.contains("zfs send -I tank/media@base tank/media@migrate"));
    }

    #[test]
    fn send_cmd_with_mbuffer() {
        let mut p = sample_params();
        p.use_mbuffer = true;
        let ds = DatasetId::new("tank/media");
        let cmd = TransferPlanner::build_send_cmd(&p, &ds);
        assert!(cmd.contains("mbuffer -m 1G"));
    }

    #[test]
    fn send_cmd_no_compress() {
        let mut p = sample_params();
        p.compress_stream = false;
        let ds = DatasetId::new("tank/media");
        let cmd = TransferPlanner::build_send_cmd(&p, &ds);
        assert!(!cmd.contains("lz4c"));
    }

    #[test]
    fn recv_cmd_marks_force() {
        let p = sample_params();
        let ds = DatasetId::new("tank/photos");
        let cmd = TransferPlanner::build_recv_cmd(&p, &ds);
        assert!(cmd.contains("zfs recv -F tank/photos"));
    }

    #[test]
    fn package_export_includes_excludes() {
        let p = sample_params();
        let cmd = TransferPlanner::build_package_export(&p);
        assert!(cmd.contains("EXCLUDE_PATTERNS"));
        assert!(cmd.contains("/etc/shadow"));
        assert!(cmd.contains("/etc/os/jwt-signing.key"));
        assert!(cmd.contains("zstd -19"));
        assert!(cmd.contains("配置包导出完成"));
    }

    #[test]
    fn package_import_runs_defensive_filter() {
        let p = sample_params();
        let cmd = TransferPlanner::build_package_import(&p);
        assert!(cmd.contains("防御性"));
        assert!(cmd.contains("zstd -d"));
        assert!(cmd.contains("配置包导入完成"));
    }

    #[test]
    fn incremental_flag_propagates() {
        let mut p = sample_params();
        p.baseline_snapshot = Some("snap@base".into());
        let ds = DatasetId::new("tank/media");
        let c = TransferPlanner::build_dataset_command(&p, &ds);
        assert!(c.incremental);
    }

    #[test]
    fn validate_rejects_empty_ssh() {
        let mut p = sample_params();
        p.source_ssh = String::new();
        let err = TransferPlanner::validate(&p).unwrap_err();
        assert!(matches!(err, ProvisionError::InvalidConfig(_)));
    }

    #[test]
    fn validate_rejects_same_node() {
        let mut p = sample_params();
        p.plan.target_node = p.plan.source_node.clone();
        let err = TransferPlanner::validate(&p).unwrap_err();
        match err {
            ProvisionError::InvalidConfig(m) => assert!(m.contains("不能相同")),
            _ => panic!("应为 InvalidConfig"),
        }
    }

    #[test]
    fn progress_zero_when_no_total() {
        let t = vec![TransferredFile::new("a", 100)];
        let p = TransferPlanner::progress(&t, None);
        assert_eq!(p, 0.0);
    }

    #[test]
    fn progress_ratio() {
        let t = vec![TransferredFile::new("a", 500)];
        let p = TransferPlanner::progress(&t, Some(1000));
        assert!((p - 0.5).abs() < 1e-6);
    }

    #[test]
    fn progress_clamped_to_one() {
        let t = vec![TransferredFile::new("a", 2000)];
        let p = TransferPlanner::progress(&t, Some(1000));
        assert_eq!(p, 1.0);
    }

    #[test]
    fn estimate_bytes_none_in_skeleton() {
        let p = sample_params();
        assert_eq!(TransferPlanner::estimate_bytes(&p, &[]), None);
    }

    // —— 覆盖率补测：recv_cmd mbuffer 分支 + target_ssh 空校验 ——

    #[test]
    fn recv_cmd_with_mbuffer_prefix() {
        // 覆盖 build_recv_cmd 的 use_mbuffer 分支（mbuffer -m 1G | 前缀）
        let mut p = sample_params();
        p.use_mbuffer = true;
        let ds = DatasetId::new("tank/media");
        let cmd = TransferPlanner::build_recv_cmd(&p, &ds);
        assert!(cmd.contains("mbuffer -m 1G | "));
        assert!(cmd.contains("lz4c -d | "));
        assert!(cmd.contains("zfs recv -F tank/media"));
    }

    #[test]
    fn recv_cmd_no_mbuffer_no_compress() {
        // 全关：recv_cmd 仅 zfs recv -F
        let mut p = sample_params();
        p.use_mbuffer = false;
        p.compress_stream = false;
        let ds = DatasetId::new("tank/photos");
        let cmd = TransferPlanner::build_recv_cmd(&p, &ds);
        assert!(!cmd.contains("mbuffer"));
        assert!(!cmd.contains("lz4c"));
        assert!(cmd.contains("zfs recv -F tank/photos"));
    }

    #[test]
    fn validate_rejects_empty_target_ssh() {
        // 覆盖 validate 的 target_ssh 空校验分支
        let mut p = sample_params();
        p.target_ssh = "   ".into();
        let err = TransferPlanner::validate(&p).unwrap_err();
        match err {
            ProvisionError::InvalidConfig(m) => assert!(m.contains("target_ssh")),
            _ => panic!("应为 InvalidConfig(target_ssh)"),
        }
    }

    #[test]
    fn validate_passes_with_baseline_snapshot() {
        // baseline_snapshot 设了 + 参数合法 → ok（覆盖 validate 正常路径带增量）
        let mut p = sample_params();
        p.baseline_snapshot = Some("tank/media@base".into());
        assert!(TransferPlanner::validate(&p).is_ok());
    }

    #[test]
    fn plan_with_empty_datasets_produces_empty_commands() {
        // 空数据集列表 → dataset_commands 空
        let mut p = sample_params();
        p.plan.datasets = vec![];
        let plan = TransferPlanner::plan(&p);
        assert!(plan.dataset_commands.is_empty());
        // 但 export/import 命令仍生成
        assert!(!plan.package_export_cmd.is_empty());
        assert!(!plan.package_import_cmd.is_empty());
    }

    #[test]
    fn progress_zero_when_total_zero() {
        // total=0 → 0.0（避免除零）
        let t = vec![TransferredFile::new("a", 100)];
        let p = TransferPlanner::progress(&t, Some(0));
        assert_eq!(p, 0.0);
    }

    #[test]
    fn plan_idempotent() {
        let p = sample_params();
        let plan1 = TransferPlanner::plan(&p);
        let plan2 = TransferPlanner::plan(&p);
        assert_eq!(plan1.dataset_commands.len(), plan2.dataset_commands.len());
        assert_eq!(
            plan1.dataset_commands[0].send_cmd,
            plan2.dataset_commands[0].send_cmd
        );
    }
}
