//! `ZfsCliBackend` —— 通过 `zpool`/`zfs` CLI 实现 [`crate::StorageBackend`]。
//!
//! 设计：
//! - **命令执行抽象**：不直接 `tokio::process::Command::output()`，而是经 [`CommandRunner`]
//!   trait。生产用 [`TokioCommandRunner`]（spawn 真实子进程），测试注入返回 fixture 的 mock
//!   runner——这样 `cargo test` 无需真实 ZFS 环境（开发机通常无 ZFS，规格书 §6 要求沙箱）。
//! - **命令构造**：全部在 [`crate::cli`]（纯函数，有独立单测）。
//! - **输出解析**：在 [`crate::model`]（`Pool/Dataset/Snapshot::from_list_line`，纯函数单测）。
//! - **并发锁**：同一数据集的写操作（create/destroy/snapshot/set_quota）互斥。用
//!   `DashMap<String, Mutex<()>>` 按 dataset 名分锁，避免全局串行。本骨架用 `std::sync::Mutex`
//!   守护内部 `HashMap`（无新依赖，符合横切规则「不引外部 crate」）。
//!
//! **权限**（2026-08-23 修复）：所有 `zpool`/`zfs` 命令经 **sudo** 执行——os-api 以
//! 普通用户（oem）运行，`zpool create` 等写操作需 root，直接 spawn 会以
//! "permission denied" 失败（即使带 `-f`）。包装逻辑见 [`wrap_with_sudo`]（纯函数），
//! 部署侧需 sudoers 免密配置（NOPASSWD）。测试用 fixture，不触发真实权限校验。

use crate::backend::StorageBackend;
use crate::cli;
use crate::error::StorageError;
use crate::model::{Dataset, Pool, Quota, Snapshot, Vdev, VdevSpec};
use crate::options::DatasetOptions;
use async_trait::async_trait;
use os_core::{CommandOutput, DatasetId, PoolId, SnapshotId};
use std::process::Stdio;
use tokio::process::Command;

/// 命令执行器抽象——隔离子进程 spawn，使 `ZfsCliBackend` 可测。
///
/// 生产实现 [`TokioCommandRunner`] 调真实 `zpool`/`zfs`；测试用闭包/结构体注入 fixture。
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// 执行 `<program> <args...>`，返回 stdout/stderr/退出码。
    async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError>;
}

/// 生产用执行器——`tokio::process::Command` spawn 真实子进程。
///
/// `zpool`/`zfs` 经 **sudo** 执行（见 [`wrap_with_sudo`]）：os-api 以普通用户运行，
/// ZFS 写操作（`zpool create -f` / `zfs create` 等）需 root。依赖部署侧 sudoers 配置：
///
/// ```text
/// # /etc/sudoers.d/nexos-zfs（oem 用户免密执行 ZFS 工具链，stdin 关闭时也成立）
/// oem ALL=(root) NOPASSWD: /usr/sbin/zpool, /usr/sbin/zfs
/// ```
///
/// 未配置免密 sudo 的环境，写命令会失败（sudo 要求交互密码而 stdin 为 null）——
/// 错误信息（"a password is required" / "not in the sudoers file"）保留在
/// `CommandOutput.stderr` 供上层诊断。
pub struct TokioCommandRunner;

/// 需要经 sudo 提权执行的程序白名单（ZFS 工具链）。
///
/// 其余程序（如测试探针 `/bin/echo`、`/bin/sh`）原样 spawn——sudo 包装只针对
/// 需 root 的 zpool/zfs，避免普通探测命令被 sudoers 拦截。
const SUDO_PROGRAMS: [&str; 2] = ["zpool", "zfs"];

/// 把 `<program> <args...>` 包装为 `sudo <program> <args...>`（仅 zpool/zfs）。
///
/// 纯函数（可单测）：返回 `(sudo, [program, args...])`；program 不在
/// [`SUDO_PROGRAMS`] 白名单时原样返回 `(program, args)`。
pub(crate) fn wrap_with_sudo(program: &str, args: &[String]) -> (String, Vec<String>) {
    if !SUDO_PROGRAMS.contains(&program) {
        return (program.to_string(), args.to_vec());
    }
    let mut sudo_args = Vec::with_capacity(args.len() + 1);
    sudo_args.push(program.to_string());
    sudo_args.extend_from_slice(args);
    ("sudo".to_string(), sudo_args)
}

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
        // zpool/zfs → sudo zpool/zfs（权限说明见 TokioCommandRunner 文档注释）；
        // 其余程序原样（wrap_with_sudo 白名单外直通）。
        let (program, args) = wrap_with_sudo(program, args);
        let output = Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}

/// ZFS CLI 后端——默认 `StorageBackend` 实现。
///
/// 构造：生产用 [`ZfsCliBackend::new`]（TokioCommandRunner）；测试用
/// [`ZfsCliBackend::with_runner`] 注入 fixture runner。
///
/// 并发：同一资源的写操作（create/destroy/snapshot/set_quota）经全局 `write_lock`
/// 串行化（用 `tokio::sync::Mutex`，其 guard 为 `Send`，可安全跨 `.await`）。
/// 读操作不加锁。TODO(集成阶段)：改用 per-dataset 细粒度锁（如 `DashMap<String, Mutex>`，
/// 需引入 `dashmap` crate，走 ADR）；当前全局锁在写并发不高时性能足够。
pub struct ZfsCliBackend {
    runner: Box<dyn CommandRunner>,
    /// 全局写串行锁。所有写方法在执行 CLI 前 `lock().await`，方法返回前 drop。
    write_lock: tokio::sync::Mutex<()>,
}

impl ZfsCliBackend {
    /// 生产构造（用真实 `zpool`/`zfs` 子进程）。
    pub fn new() -> Self {
        Self::with_runner(Box::new(TokioCommandRunner))
    }

    /// 测试构造——注入自定义 [`CommandRunner`]（返回 fixture 输出）。
    pub fn with_runner(runner: Box<dyn CommandRunner>) -> Self {
        Self {
            runner,
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 获取写锁 guard（全局串行），guard 可跨 `.await` 持有（`tokio::sync::MutexGuard` 为 Send）。
    async fn write_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().await
    }

    /// 列出所有池并补全每个池的 vdev 明细（含错误计数）。
    ///
    /// [`StorageBackend::list_pools`] 只跑 `zpool list`（容量+健康，无 vdev）。
    /// 本方法额外跑 `zpool status` 解析 vdev 树，把 vdevs 合并进每个 `Pool`。
    /// 容量/健康以 `zpool list` 为准（更精确），vdevs 来自 `zpool status`。
    ///
    /// 多余的 zfs 子进程调用有成本，故不进 trait（默认 `list_pools` 保持轻量）；
    /// 需 vdev 明细的调用方显式调本方法。
    pub async fn list_pools_with_vdevs(&self) -> Result<Vec<Pool>, StorageError> {
        // 先取池级容量/健康（zpool list -p -H）
        let mut pools = self.list_pools().await?;
        if pools.is_empty() {
            return Ok(pools);
        }
        // 再跑 zpool status 解析 vdev 树
        let out = self.exec("zpool", &cli::zpool_status_args(None)).await?;
        let statuses = parse_zpool_status(&out.stdout);
        // 按 name 合并 vdevs（zpool list 和 status 的池名应一一对应）
        for pool in &mut pools {
            if let Some(st) = statuses.iter().find(|s| s.name == pool.name) {
                pool.vdevs = st.vdevs.clone();
            }
        }
        Ok(pools)
    }

    /// 执行命令，非零退出映射为 `CommandFailed`（保留 stderr）。
    async fn exec(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
        let out = self.runner.run(program, args).await?;
        if !out.is_success() {
            return Err(StorageError::CommandFailed(format!(
                "{program} {:?} 退出码 {}：{}",
                args.join(" "),
                out.exit_code,
                out.stderr.trim()
            )));
        }
        Ok(out)
    }

    /// 把非零退出 stderr 分类映射成更具体的错误（PoolExists/DatasetExists/NotFound 等）。
    /// 规则：OpenZFS 的 stderr 含可识别关键词。
    fn classify_err(cmd_err: StorageError, ctx: &str) -> StorageError {
        let StorageError::CommandFailed(msg) = &cmd_err else {
            return cmd_err;
        };
        let lower = msg.to_lowercase();
        // 池/数据集已存在
        if lower.contains("already exists") {
            if ctx.contains("pool") {
                return StorageError::PoolExists(ctx.to_string());
            }
            return StorageError::DatasetExists(ctx.to_string());
        }
        // 不存在
        if lower.contains("does not exist")
            || lower.contains("no such")
            || lower.contains("not found")
        {
            if ctx.starts_with("snapshot:") {
                return StorageError::SnapshotNotFound(ctx.to_string());
            }
            if ctx.starts_with("pool:") {
                return StorageError::PoolNotFound(ctx.to_string());
            }
            return StorageError::DatasetNotFound(ctx.to_string());
        }
        // vdev 非法
        if lower.contains("invalid vdev") || lower.contains("no valid devices") {
            return StorageError::InvalidVdev(ctx.to_string());
        }
        cmd_err
    }
}

impl Default for ZfsCliBackend {
    fn default() -> Self {
        Self::new()
    }
}

// 注：StorageBackend 是原生 async fn in trait（无 #[async_trait]）。
// ZfsCliBackend 不需要 `Box<dyn StorageBackend>`（单实现），保持原生 async 零开销。
impl StorageBackend for ZfsCliBackend {
    async fn create_pool(&self, id: &PoolId, vdevs: Vec<VdevSpec>) -> Result<Pool, StorageError> {
        let _g = self.write_guard().await;
        let args = cli::zpool_create_args(id.as_str(), &vdevs);
        self.exec("zpool", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("pool:{}", id)))?;
        // 创建后回读池状态（list 单行解析）。
        let out = self.exec("zpool", &cli::zpool_list_args()).await?;
        for line in out.stdout.lines() {
            let pool = Pool::from_list_line(line)?;
            if pool.id == *id {
                return Ok(pool);
            }
        }
        Err(StorageError::CommandFailed(format!(
            "create_pool 后 list 未找到新池 {}（输出：{:?}）",
            id, out.stdout
        )))
    }

    async fn destroy_pool(&self, id: &PoolId) -> Result<(), StorageError> {
        let _g = self.write_guard().await;
        let args = cli::zpool_destroy_args(id.as_str());
        self.exec("zpool", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("pool:{}", id)))?;
        Ok(())
    }

    async fn list_pools(&self) -> Result<Vec<Pool>, StorageError> {
        let out = self.exec("zpool", &cli::zpool_list_args()).await?;
        let mut pools = Vec::new();
        for line in out.stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            pools.push(Pool::from_list_line(line)?);
        }
        Ok(pools)
    }

    async fn create_dataset(
        &self,
        name: &DatasetId,
        options: DatasetOptions,
    ) -> Result<Dataset, StorageError> {
        let _g = self.write_guard().await;
        let args = cli::zfs_create_args(name.as_str(), &options);
        self.exec("zfs", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("dataset:{}", name)))?;
        // 回读
        let out = self.exec("zfs", &cli::zfs_list_datasets_args(None)).await?;
        for line in out.stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let ds = Dataset::from_list_line(line)?;
            if ds.id == *name {
                return Ok(ds);
            }
        }
        Err(StorageError::CommandFailed(format!(
            "create_dataset 后 list 未找到新数据集 {}（输出：{:?}）",
            name, out.stdout
        )))
    }

    async fn destroy_dataset(&self, name: &DatasetId) -> Result<(), StorageError> {
        let _g = self.write_guard().await;
        let args = cli::zfs_destroy_args(name.as_str());
        self.exec("zfs", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("dataset:{}", name)))?;
        Ok(())
    }

    async fn list_datasets(&self, pool: Option<&PoolId>) -> Result<Vec<Dataset>, StorageError> {
        let args = cli::zfs_list_datasets_args(pool.map(|p| p.as_str()));
        let out = self.exec("zfs", &args).await?;
        let mut datasets = Vec::new();
        for line in out.stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            datasets.push(Dataset::from_list_line(line)?);
        }
        Ok(datasets)
    }

    async fn snapshot(&self, dataset: &DatasetId, name: &str) -> Result<Snapshot, StorageError> {
        let _g = self.write_guard().await;
        let args = cli::zfs_snapshot_args(dataset.as_str(), name);
        self.exec("zfs", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("dataset:{}", dataset)))?;
        // 回读快照
        let snap_full = format!("{}@{}", dataset, name);
        let out = self
            .exec("zfs", &cli::zfs_list_snapshots_args(Some(dataset.as_str())))
            .await?;
        for line in out.stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let snap = Snapshot::from_list_line(line)?;
            if snap.id.as_str() == snap_full {
                return Ok(snap);
            }
        }
        Err(StorageError::CommandFailed(format!(
            "snapshot 后 list 未找到新快照 {snap_full}（输出：{:?}）",
            out.stdout
        )))
    }

    async fn destroy_snapshot(&self, snapshot: &SnapshotId) -> Result<(), StorageError> {
        let args = cli::zfs_destroy_snapshot_args(snapshot.as_str());
        self.exec("zfs", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("snapshot:{}", snapshot)))?;
        Ok(())
    }

    async fn list_snapshots(
        &self,
        dataset: Option<&DatasetId>,
    ) -> Result<Vec<Snapshot>, StorageError> {
        let args = cli::zfs_list_snapshots_args(dataset.map(|d| d.as_str()));
        let out = self.exec("zfs", &args).await?;
        let mut snaps = Vec::new();
        for line in out.stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            snaps.push(Snapshot::from_list_line(line)?);
        }
        Ok(snaps)
    }

    async fn set_quota(&self, dataset: &DatasetId, quota: Quota) -> Result<(), StorageError> {
        let _g = self.write_guard().await;
        let args = cli::zfs_set_quota_args(dataset.as_str(), quota.refquota, quota.refreservation);
        self.exec("zfs", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("dataset:{}", dataset)))?;
        Ok(())
    }

    async fn get_quota(&self, dataset: &DatasetId) -> Result<Quota, StorageError> {
        let args = cli::zfs_get_quota_args(dataset.as_str());
        let out = self
            .exec("zfs", &args)
            .await
            .map_err(|e| Self::classify_err(e, &format!("dataset:{}", dataset)))?;
        // zfs get -p -H -o value refquota,refreservation <ds>
        // 输出 2 行（每个属性一行），值可能是数字或 `-`
        let lines: Vec<&str> = out
            .stdout
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        let parse_val = |s: &str| -> Option<u64> {
            if s == "-" || s.is_empty() {
                None
            } else {
                s.parse::<u64>().ok()
            }
        };
        let refquota = lines.first().and_then(|l| parse_val(l));
        let refreservation = lines.get(1).and_then(|l| parse_val(l));
        Ok(Quota {
            refquota,
            refreservation,
        })
    }
}

/// 从 `zpool status` 输出解析某池的 vdev 列表（list 命令不含 vdev 明细）。
///
/// 便捷封装：取 [`parse_zpool_status`] 结果中匹配池名的第一个池的 vdevs。
/// 找不到时返回空 Vec（容错——调用方可先确认池存在）。
#[allow(dead_code)]
pub(crate) fn parse_vdevs_from_status(status_output: &str) -> Vec<Vdev> {
    parse_zpool_status(status_output)
        .into_iter()
        .next()
        .map(|p| p.vdevs)
        .unwrap_or_default()
}

/// `zpool status` 解析出的单池结果（池元数据 + vdev 树）。
///
/// 池级容量/已用空间不在 `zpool status` 输出里（需 `zpool list`），
/// 此处只给 name/state/scan/vdevs。调用方（如 [`ZfsCliBackend::list_pools_with_vdevs`]）
/// 负责把 vdevs 合并进 `Pool`（容量来自 `zpool list`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    /// 池名（如 `tank`）
    pub name: String,
    /// 池整体健康（`state:` 行映射到 [`os_core::Health`]）
    pub health: os_core::Health,
    /// scan 行原文（如 `scrub repaired 0B in ... with 0 errors on ...`）；
    /// 无 scan 行时为 None（新池未跑过 scrub）
    pub scan: Option<String>,
    /// 顶层 vdev 列表（已折叠嵌套 mirror/raidz 成员到 disks）
    pub vdevs: Vec<Vdev>,
}

/// 把单条 config 数据行解析成 (name, state, read, write, cksum)。
///
/// config 数据行形如（NAME 列后跟 STATE READ WRITE CKSUM，空白分隔）：
/// ```text
/// \ttank                        ONLINE       0     0     0
/// \t  mirror-0                  ONLINE       0     0     0
/// \t    /dev/sdb                ONLINE       0     0     0
/// ```
/// NAME 可能含 `/`、`-`、`mirror-0`/`raidz1-0` 等复合名。STATE 取值
/// ONLINE/DEGRADED/FAULTED/UNAVAIL/REMOVED/OFFLINE/SUSPENDED。错误计数可能是
/// `0` 或数字（也可能 `N`，但 OpenZFS 输出整数）。
///
/// 返回 None 表示该行不是有效的数据行（字段不足或计数列非数字）。
fn parse_status_data_row(line: &str) -> Option<(&str, os_core::Health, u64, u64, u64)> {
    // 去掉前导缩进后按空白分列。zpool status 的 NAME/STATE/READ/WRITE/CKSUM
    // 是空格对齐的（非 tab 分隔），所以用 split_whitespace 最稳。
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 5 {
        return None;
    }
    let name = cols[0];
    let health = crate::model::parse_health_public(cols[1]);
    let read = cols[2].parse::<u64>().ok()?;
    let write = cols[3].parse::<u64>().ok()?;
    let cksum = cols[4].parse::<u64>().ok()?;
    Some((name, health, read, write, cksum))
}

/// 计算 config 数据行的「深度」（树形缩进层级）。
///
/// `zpool status` 用制表符+空格表达树深度：
/// - 池根数据行：1 个 tab（如 `\ttank ONLINE ...`）→ 深度 0
/// - 顶层 vdev：1 tab + 2 空格（如 `\t  /dev/sdb ONLINE`）→ 深度 1
/// - mirror/raidz 成员：1 tab + 4 空格（如 `\t    /dev/sdb ONLINE`）→ 深度 2
///
/// 保守算法：按行首的（tab + 空格）总缩进字符数 / 2 取整作为相对深度，
/// 再减去池根行（最深 1 tab）的基准。实际只需「能否区分池根 vs vdev vs 子设备」
/// 三级，故用缩进字符数的相对比较即可，不依赖精确层级公式。
///
/// 返回 (leading_whitespace_chars, trimmed_rest)。调用方据此判断归属。
fn status_indent(line: &str) -> usize {
    // 数前导空白（tab 算 1 字符，按 OpenZFS 实际输出每级 2 空格或 1 tab）。
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// 把一个顶层 vdev 的 kind 从其名字推断（`mirror-0` → Mirror，`raidz1-0` → Raidz1）。
///
/// OpenZFS 在 `zpool status` 里给冗余 vdev 起形如 `mirror-0`/`raidz1-0`/`raidz2-1`
/// 的合成名。单盘 vdev 则直接是设备路径（`/dev/sdb`/`/tmp/foo.img`），推断为 Disk。
fn infer_vdev_kind(name: &str) -> crate::model::VdevKind {
    // 取 `-` 前的部分作为类型关键字（如 `mirror`/`raidz1`/`raidz2`/`raidz3`）。
    let kw = name.split('-').next().unwrap_or(name);
    crate::model::VdevKind::from_status_str(kw).unwrap_or(crate::model::VdevKind::Disk)
}

/// 把已折叠的子设备错误计数汇总进顶层 vdev（取最大值，反映该 vdev 最严重的盘况）。
///
/// `zpool status` 里顶层 mirror/raidz 行的 READ/WRITE/CKSUM 通常是成员盘之和或 0
/// （取决于版本），单独看不可靠。本解析器用「成员盘最大错误计数」填充顶层 vdev
/// 的错误字段——更贴近「该 vdev 是否有盘报错」的语义。
fn aggregate_errors(disks: &[(String, os_core::Health, u64, u64, u64)]) -> (u64, u64, u64) {
    let (mut r, mut w, mut c) = (0u64, 0u64, 0u64);
    for (_, _, dr, dw, dc) in disks {
        r = r.max(*dr);
        w = w.max(*dw);
        c = c.max(*dc);
    }
    (r, w, c)
}

/// 从 `zpool status` 输出解析所有池（含 vdev 树）。
///
/// 输入是 `zpool status [pool]` 的完整 stdout（人类可读树形格式，**非** -p -H）。
/// 典型输出：
/// ```text
///   pool: osprobepersist
///  state: ONLINE
///   scan: scrub repaired 0B in 00:00:00 with 0 errors on Thu Aug  6 18:02:30 2026
/// config:
///
/// \tNAME                         STATE     READ WRITE CKSUM
/// \tosprobepersist              ONLINE       0     0     0
/// \t  /tmp/osprobe-persist.img  ONLINE       0     0     0
///
/// errors: No known data errors
/// ```
///
/// 解析规则：
/// - `  pool: <name>` → 池名（前导 2 空格）。
/// - ` state: <STATE>` → 池整体健康。
/// - ` scan: <text>` → scan 行原文（可选）。
/// - `config:` 到下个空行/`errors:` 之间是 config 段；跳过表头（含 `NAME` 列名）。
/// - config 数据行按缩进分组：池根行（最深 1 tab）定界每个池的 vdev 段；
///   比 pool 根行更深的就是该池的 vdev/子设备。
///
/// 多池输出（`zpool status` 全量）会按 `pool:` 行切分，每段独立解析。
/// 容错：异常行（字段不足、计数非数字）跳过不报错；空输出返回空 Vec。
pub fn parse_zpool_status(output: &str) -> Vec<PoolStatus> {
    // 第一步：按 `  pool:` 行把整个输出切成多段（每段对应一个池）。
    // 段内含该池的 state/scan/config/errors 全部行。
    // trim_start 后比前缀，容错不同缩进（真实 `  pool:`，但手工构造可能不同）。
    let mut segments: Vec<Vec<&str>> = Vec::new();
    for line in output.lines() {
        if line.trim_start().starts_with("pool:") {
            segments.push(Vec::new());
        }
        if let Some(last) = segments.last_mut() {
            last.push(line);
        }
    }

    let mut result = Vec::with_capacity(segments.len());
    for seg in segments {
        let mut name: Option<String> = None;
        let mut health = os_core::Health::Unknown;
        let mut scan: Option<String> = None;
        let mut in_config = false;
        // 当前池的 config 数据行（去掉表头），按 (indent, raw_line) 保留。
        let mut data_rows: Vec<(usize, &str)> = Vec::new();

        for line in seg {
            // 先 trim 整行前导空白再比前缀——容错不同缩进（真实输出 `  pool:`/` state:`，
            // 但手工构造/老版本 zfs 可能缩进不同）。`config:` 段内的数据行**不**在此处理
            // （它们的缩进承载树形语义，不能 trim 掉）。
            let trimmed_line = line.trim_start();
            // `  pool: tank` → 取冒号后 trim
            if let Some(rest) = trimmed_line.strip_prefix("pool:") {
                name = Some(rest.trim().to_string());
                continue;
            }
            // ` state: ONLINE`（trim 后比前缀，容错缩进）
            if let Some(rest) = trimmed_line.strip_prefix("state:") {
                health = crate::model::parse_health_public(rest.trim());
                continue;
            }
            // `  scan: ...`
            if let Some(rest) = trimmed_line.strip_prefix("scan:") {
                scan = Some(rest.trim().to_string());
                continue;
            }
            // config 段开始（注意：仅在非 config 段时识别，避免误吞数据行）
            if !in_config && trimmed_line.starts_with("config:") {
                in_config = true;
                continue;
            }
            // `errors:` 行标志 config 段结束（也标志整个池段结束）
            if trimmed_line.starts_with("errors:") {
                in_config = false;
                continue;
            }
            if in_config {
                // 空行 → config 段内分隔，跳过
                if line.trim().is_empty() {
                    continue;
                }
                // 跳过表头行（含 `NAME` 列名，且 STATE 列不是健康关键字）
                // OpenZFS 表头：`\tNAME   STATE   READ   WRITE   CKSUM`
                if trimmed_line.starts_with("NAME")
                    && trimmed_line.contains("STATE")
                    && trimmed_line.contains("CKSUM")
                {
                    continue;
                }
                // 注意：用原始 line（带前导缩进）算 indent，不能 trim。
                data_rows.push((status_indent(line), line));
            }
        }

        let name = match name {
            Some(n) if !n.is_empty() => n,
            // 无 pool: 行（异常输出）→ 跳过该段
            _ => continue,
        };

        // 第二步：把 config 数据行按缩进分成「池根行」和「vdev/子设备行」。
        // 池根行（最浅缩进，NAME == 池名）定界；其后的更深行就是该池的 vdev 树。
        // 多个池根行（多池 zpool status 全量）已被段切分处理，段内通常只有 1 个池根行。
        let pool_root_indent = data_rows
            .iter()
            .filter(|(_, l)| {
                parse_status_data_row(l)
                    .map(|(n, _, _, _, _)| n == name.as_str())
                    .unwrap_or(false)
            })
            .map(|(ind, _)| *ind)
            .min();

        // 收集比池根行更深的行 → vdev 树节点。
        let mut vdev_nodes: Vec<(usize, &str)> = Vec::new();
        if let Some(root_ind) = pool_root_indent {
            for (ind, line) in &data_rows {
                if *ind > root_ind {
                    vdev_nodes.push((*ind, *line));
                }
            }
        } else {
            // 无池根行（解析兜底）：所有数据行都当 vdev（除 NAME 行已过滤）
            vdev_nodes = data_rows.clone();
        }

        // 第三步：把 vdev 节点折叠成顶层 Vdev。
        // 拓扑：顶层 vdev 行（缩进 == root+1 级）+ 其成员行（缩进更深）。
        // 单盘池：只有 1 个顶层 vdev 行，无成员 → 直接成 Disk vdev。
        // mirror/raidz 池：1 个顶层 `mirror-0` 行 + N 个成员行 → 成 Mirror/RaidzN vdev，
        //   disks 收集成员路径，错误取成员最大值。
        let top_level_indent = vdev_nodes.iter().map(|(i, _)| *i).min();
        let vdevs = match top_level_indent {
            Some(top_ind) => {
                // 按「顶层行 + 紧随其后且更深的行」分组
                let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
                for (_, line) in vdev_nodes.iter().filter(|(i, _)| *i == top_ind) {
                    groups.push((line, Vec::new()));
                }
                let mut current_top: Option<usize> = None;
                for (ind, line) in &vdev_nodes {
                    if *ind == top_ind {
                        current_top = Some(groups.len().saturating_sub(1));
                    } else if let Some(idx) = current_top {
                        // 把更深的行挂到最近的顶层组
                        if idx < groups.len() {
                            groups[idx].1.push(*line);
                        }
                    }
                }

                let mut vdevs = Vec::with_capacity(groups.len());
                for (top_line, member_lines) in groups {
                    let (top_name, top_health, tr, tw, tc) = match parse_status_data_row(top_line) {
                        Some(d) => d,
                        None => continue, // 异常行跳过
                    };
                    let kind = infer_vdev_kind(top_name);
                    if member_lines.is_empty() {
                        // 单盘 vdev（顶层即叶子）
                        vdevs.push(Vdev {
                            kind,
                            disks: vec![top_name.to_string()],
                            health: top_health,
                            read_errors: tr,
                            write_errors: tw,
                            cksum_errors: tc,
                        });
                    } else {
                        // 冗余 vdev：成员是 disks；错误取成员最大值
                        let mut disks: Vec<(String, os_core::Health, u64, u64, u64)> =
                            Vec::with_capacity(member_lines.len());
                        for ml in member_lines {
                            if let Some((mn, mh, mr, mw, mc)) = parse_status_data_row(ml) {
                                disks.push((mn.to_string(), mh, mr, mw, mc));
                            }
                        }
                        let (r, w, c) = aggregate_errors(&disks);
                        vdevs.push(Vdev {
                            kind,
                            disks: disks.into_iter().map(|(p, _, _, _, _)| p).collect(),
                            // 顶层 vdev 的健康用其自身的 state（OpenZFS 给 mirror/raidz 汇总态）
                            health: top_health,
                            read_errors: r,
                            write_errors: w,
                            cksum_errors: c,
                        });
                    }
                }
                vdevs
            }
            None => Vec::new(),
        };

        result.push(PoolStatus {
            name,
            health,
            scan,
            vdevs,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use os_core::{DatasetId, PoolId, SnapshotId};

    /// 测试用 CommandRunner——按 (program, args 首元素) 分发预设 fixture。
    struct FixtureRunner {
        fixtures: std::sync::Mutex<Vec<FixtureEntry>>,
    }

    struct FixtureEntry {
        /// 匹配 program（zpool/zfs）
        program: &'static str,
        /// 匹配 args 的前缀子串（如 "create" / "list -p"）
        args_contains: &'static str,
        output: CommandOutput,
    }

    impl FixtureRunner {
        fn new() -> Self {
            Self {
                fixtures: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn on(
            mut self,
            program: &'static str,
            args_contains: &'static str,
            output: CommandOutput,
        ) -> Self {
            self.fixtures.get_mut().unwrap().push(FixtureEntry {
                program,
                args_contains,
                output,
            });
            self
        }
    }

    #[async_trait]
    impl CommandRunner for FixtureRunner {
        async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
            let joined = args.join(" ");
            let fixtures = self.fixtures.lock().unwrap();
            // 匹配规则：args_contains 与子命令（args[0]）精确相等。
            // 用「精确 args[0]」而非「全串 contains」——后者会把 `zfs list -t snapshot`
            // 误命中 args_contains="snapshot" 的 fixture（"snapshot" 是其中的 -t 取值）。
            let subcmd = args.first().map(String::as_str);
            for f in fixtures.iter() {
                if f.program == program && subcmd == Some(f.args_contains) {
                    return Ok(f.output.clone());
                }
            }
            // 无匹配：返回失败，便于发现未预设的命令
            Err(StorageError::CommandFailed(format!(
                "FixtureRunner 无匹配 fixture: {program} {joined}"
            )))
        }
    }

    #[tokio::test]
    async fn list_pools_parses_multiple() {
        let stdout = "tank\t10995116277760\t1374389534720\t9620726743040\t-\t-\t12\t12\t1.00x\tONLINE\t-\nbackup\t2000000000000\t500000000000\t1500000000000\t-\t-\t5\t25\t1.00x\tDEGRADED\t-\n";
        let runner = FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let pools = backend.list_pools().await.unwrap();
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].id.as_str(), "tank");
        assert_eq!(pools[0].health, os_core::Health::Healthy);
        assert_eq!(pools[1].id.as_str(), "backup");
        assert_eq!(pools[1].health, os_core::Health::Degraded);
    }

    #[tokio::test]
    async fn create_pool_round_trips() {
        // create 成功空输出，随后 list 返回新池
        let runner = FixtureRunner::new()
            .on("zpool", "create", CommandOutput::ok())
            .on(
                "zpool",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: "tank\t10995116277760\t1374389534720\t9620726743040\t-\t-\t12\t12\t1.00x\tONLINE\t-".to_string(),
                    stderr: String::new(),
                },
            );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let pool = backend
            .create_pool(
                &PoolId::new("tank"),
                vec![crate::model::VdevSpec {
                    kind: crate::model::VdevKind::Mirror,
                    disks: vec!["/dev/sdb".into(), "/dev/sdc".into()],
                }],
            )
            .await
            .unwrap();
        assert_eq!(pool.id.as_str(), "tank");
        assert_eq!(pool.capacity.total_bytes, 10_995_116_277_760);
    }

    #[tokio::test]
    async fn create_pool_already_exists_maps_to_pool_exists() {
        let runner = FixtureRunner::new().on(
            "zpool",
            "create",
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "cannot create 'tank': pool already exists".to_string(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .create_pool(&PoolId::new("tank"), Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::PoolExists(_)));
    }

    #[tokio::test]
    async fn destroy_pool_not_found_maps() {
        let runner = FixtureRunner::new().on(
            "zpool",
            "destroy",
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "cannot destroy 'ghost': no such pool".to_string(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .destroy_pool(&PoolId::new("ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::PoolNotFound(_)));
    }

    #[tokio::test]
    async fn create_dataset_round_trips() {
        let runner = FixtureRunner::new()
            .on("zfs", "create", CommandOutput::ok())
            .on(
                "zfs",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: "tank/media\t5497558138880\t5497558138880\tyes\toff".to_string(),
                    stderr: String::new(),
                },
            );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let ds = backend
            .create_dataset(&DatasetId::new("tank/media"), DatasetOptions::default())
            .await
            .unwrap();
        assert_eq!(ds.id.as_str(), "tank/media");
        assert_eq!(ds.pool.as_str(), "tank");
    }

    #[tokio::test]
    async fn create_dataset_exists_maps() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "create",
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "cannot create 'tank/media': dataset already exists".to_string(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .create_dataset(&DatasetId::new("tank/media"), DatasetOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::DatasetExists(_)));
    }

    #[tokio::test]
    async fn snapshot_round_trips() {
        let runner = FixtureRunner::new()
            .on("zfs", "snapshot", CommandOutput::ok())
            .on(
                "zfs",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: "tank/media@snap1\t1073741824\t1700000000".to_string(),
                    stderr: String::new(),
                },
            );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let snap = backend
            .snapshot(&DatasetId::new("tank/media"), "snap1")
            .await
            .unwrap();
        assert_eq!(snap.id.as_str(), "tank/media@snap1");
        assert_eq!(snap.created.timestamp(), 1_700_000_000);
    }

    #[tokio::test]
    async fn get_quota_parses_values() {
        // refquota=1000, refreservation=`-`
        let runner = FixtureRunner::new().on(
            "zfs",
            "get",
            CommandOutput {
                exit_code: 0,
                stdout: "1000\n-".to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let q = backend
            .get_quota(&DatasetId::new("tank/media"))
            .await
            .unwrap();
        assert_eq!(q.refquota, Some(1000));
        assert_eq!(q.refreservation, None);
    }

    #[tokio::test]
    async fn set_quota_success() {
        let runner = FixtureRunner::new().on("zfs", "set", CommandOutput::ok());
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        backend
            .set_quota(
                &DatasetId::new("tank/media"),
                Quota {
                    refquota: Some(1000),
                    refreservation: Some(500),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_datasets_scoped_to_pool() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: "tank/media\t100\t200\tyes\toff\ntank/docs\t50\t150\tyes\toff".to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let ds = backend
            .list_datasets(Some(&PoolId::new("tank")))
            .await
            .unwrap();
        assert_eq!(ds.len(), 2);
    }

    #[tokio::test]
    async fn list_snapshots_parses() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: "tank/media@s1\t1024\t1700000000\ntank/media@s2\t2048\t1700000100"
                    .to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let snaps = backend
            .list_snapshots(Some(&DatasetId::new("tank/media")))
            .await
            .unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].id.as_str(), "tank/media@s1");
    }

    #[tokio::test]
    async fn destroy_snapshot_success() {
        let runner = FixtureRunner::new().on("zfs", "destroy", CommandOutput::ok());
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        backend
            .destroy_snapshot(&SnapshotId::new("tank/media@s1"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_not_found_maps() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "destroy",
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "cannot destroy 'tank/media@ghost': no such snapshot".to_string(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .destroy_snapshot(&SnapshotId::new("tank/media@ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::SnapshotNotFound(_)));
    }

    #[test]
    fn classify_err_invalid_vdev() {
        let err = StorageError::CommandFailed("invalid vdev specification".into());
        let mapped = ZfsCliBackend::classify_err(err, "pool:tank");
        assert!(matches!(mapped, StorageError::InvalidVdev(_)));
    }

    // —— 端到端 fixture 测补充（命令构造 + 执行 + 解析全链路，不真跑 zfs）——

    #[tokio::test]
    async fn destroy_dataset_round_trips() {
        // destroy 成功空输出（list 不回读）。
        let runner = FixtureRunner::new().on("zfs", "destroy", CommandOutput::ok());
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        backend
            .destroy_dataset(&DatasetId::new("tank/media"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn destroy_dataset_not_found_maps() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "destroy",
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "cannot destroy 'tank/ghost': dataset does not exist".to_string(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .destroy_dataset(&DatasetId::new("tank/ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::DatasetNotFound(_)));
    }

    #[tokio::test]
    async fn get_quota_both_values() {
        // refquota=2048, refreservation=1024（双值都解析出来）
        let runner = FixtureRunner::new().on(
            "zfs",
            "get",
            CommandOutput {
                exit_code: 0,
                stdout: "2048\n1024".to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let q = backend
            .get_quota(&DatasetId::new("tank/media"))
            .await
            .unwrap();
        assert_eq!(q.refquota, Some(2048));
        assert_eq!(q.refreservation, Some(1024));
    }

    #[tokio::test]
    async fn list_pools_empty_output() {
        // 空 stdout（无池）应返回空 Vec，不报错。
        let runner = FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let pools = backend.list_pools().await.unwrap();
        assert!(pools.is_empty());
    }

    #[tokio::test]
    async fn list_datasets_empty_output() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let ds = backend.list_datasets(None).await.unwrap();
        assert!(ds.is_empty());
    }

    #[tokio::test]
    async fn list_snapshots_empty_output() {
        let runner = FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let snaps = backend.list_snapshots(None).await.unwrap();
        assert!(snaps.is_empty());
    }

    #[tokio::test]
    async fn create_pool_list_missing_returns_command_failed() {
        // create 成功，但后续 list 找不到新池——应返回 CommandFailed（非静默成功）。
        let runner = FixtureRunner::new()
            .on("zpool", "create", CommandOutput::ok())
            .on(
                "zpool",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout:
                        "other\t10995116277760\t0\t10995116277760\t-\t-\t0\t0\t1.00x\tONLINE\t-"
                            .to_string(),
                    stderr: String::new(),
                },
            );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .create_pool(&PoolId::new("tank"), Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn create_dataset_list_missing_returns_command_failed() {
        let runner = FixtureRunner::new()
            .on("zfs", "create", CommandOutput::ok())
            .on(
                "zfs",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: "tank/other\t10\t90\tyes\toff".to_string(),
                    stderr: String::new(),
                },
            );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend
            .create_dataset(&DatasetId::new("tank/media"), DatasetOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn set_quota_only_refreservation() {
        // refquota=None, refreservation=Some —— 验证只设 reservation 也走通执行路径。
        let runner = FixtureRunner::new().on("zfs", "set", CommandOutput::ok());
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        backend
            .set_quota(
                &DatasetId::new("tank/media"),
                Quota {
                    refquota: None,
                    refreservation: Some(500),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_pools_skips_blank_lines() {
        // 含空行/尾随换行应被跳过（避免空行触发 from_list_line 报错）。
        let stdout = "\ntank\t10995116277760\t1374389534720\t9620726743040\t-\t-\t12\t12\t1.00x\tONLINE\t-\n\n";
        let runner = FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let pools = backend.list_pools().await.unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id.as_str(), "tank");
    }

    #[test]
    fn classify_err_dataset_not_found() {
        // "no such dataset" → DatasetNotFound（ctx 非 pool:/snapshot:）
        let err = StorageError::CommandFailed("cannot open 'tank/ghost': no such dataset".into());
        let mapped = ZfsCliBackend::classify_err(err, "dataset:tank/ghost");
        assert!(matches!(mapped, StorageError::DatasetNotFound(_)));
    }

    #[test]
    fn classify_err_pool_not_found() {
        let err = StorageError::CommandFailed("cannot open 'ghost': no such pool".into());
        let mapped = ZfsCliBackend::classify_err(err, "pool:ghost");
        assert!(matches!(mapped, StorageError::PoolNotFound(_)));
    }

    #[test]
    fn classify_err_passes_through_unknown() {
        // 无可识别关键词 → 原样返回 CommandFailed。
        let err = StorageError::CommandFailed("permission denied".into());
        let mapped = ZfsCliBackend::classify_err(err, "pool:tank");
        assert!(matches!(mapped, StorageError::CommandFailed(_)));
    }

    // —— ZfsCliBackend::exec 直接测（exec 是命令构造→错误映射的枢纽）——

    #[tokio::test]
    async fn exec_maps_nonzero_to_command_failed() {
        // 验证 exec 把非零退出码包成 CommandFailed（保留 stderr 供诊断）。
        let runner = FixtureRunner::new().on(
            "zfs",
            "get",
            CommandOutput {
                exit_code: 2,
                stdout: String::new(),
                stderr: "bad option".to_string(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let err = backend.exec("zfs", &["get".to_string()]).await.unwrap_err();
        match err {
            StorageError::CommandFailed(msg) => {
                assert!(msg.contains("退出码 2"), "msg 应含退出码: {msg}");
                assert!(msg.contains("bad option"), "msg 应含 stderr: {msg}");
            }
            other => panic!("应为 CommandFailed，实际: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_passes_through_success() {
        // status==0 的 CommandOutput 原样返回（exec 不改 stdout/stderr）。
        let runner = FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: "tank\t1\t0\t1\t-\t-\t0\t0\t1.00x\tONLINE\t-".to_string(),
                stderr: String::new(),
            },
        );
        let backend = ZfsCliBackend::with_runner(Box::new(runner));
        let out = backend.exec("zpool", &["list".to_string()]).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("tank"));
    }

    // —— sudo 包装（创建池权限修复，2026-08-23）——

    #[test]
    fn wrap_with_sudo_prefixes_zfs_tools_only() {
        // zpool：sudo zpool <args...>（args 原序保留在程序名后）
        let (p, a) = wrap_with_sudo("zpool", &["create".to_string(), "-f".to_string()]);
        assert_eq!(p, "sudo");
        assert_eq!(
            a,
            vec!["zpool".to_string(), "create".to_string(), "-f".to_string()]
        );
        // zfs：同款包装
        let (p, a) = wrap_with_sudo("zfs", &["list".to_string(), "-t".to_string()]);
        assert_eq!(p, "sudo");
        assert_eq!(
            a,
            vec!["zfs".to_string(), "list".to_string(), "-t".to_string()]
        );
        // 白名单外（测试探针 / 绝对路径）：原样直通，不受 sudo 影响
        let (p, a) = wrap_with_sudo("/bin/echo", &["hello".to_string()]);
        assert_eq!(p, "/bin/echo");
        assert_eq!(a, vec!["hello".to_string()]);
        let (p, _) = wrap_with_sudo("wipefs", &["-a".to_string()]);
        assert_eq!(p, "wipefs");
        // 空参数也安全（程序名仍前移）
        let (p, a) = wrap_with_sudo("zpool", &[]);
        assert_eq!(p, "sudo");
        assert_eq!(a, vec!["zpool".to_string()]);
    }
}

// ----------------------------------------------------------------------------
// 生产执行器 `TokioCommandRunner` 真实 spawn 测
// ----------------------------------------------------------------------------
//
// 以下测验证**生产执行路径**真实 spawn 子进程（非 fixture）。用系统自带命令
// （`/bin/true` / `/bin/false` / `/bin/echo`）验证：
// - 成功路径：stdout 捕获、退出码 0。
// - 失败路径：非零退出码被 exec 映射为 CommandFailed。
// - 错误路径：程序不存在映射为 StorageError::Io。
//
// 这些测**不跑 zfs**（避免依赖 root/ZFS 环境），只验证 runner 本身的 spawn+等待+解析
// 行为正确——这是 `ZfsCliBackend::new()`（生产构造）依赖的执行层。
//
// 注意：硬编码 `/bin/true` 等路径（Linux 标准路径，本 crate 只面向 Linux + ZFS）。
// 跨平台兼容不在本 crate 范围（ZFS 仅 Linux/FreeBSD）。
#[cfg(test)]
mod tokio_command_runner_tests {
    use super::*;

    #[tokio::test]
    async fn runner_captures_exit_zero_and_stdout() {
        // /bin/echo 输出参数到 stdout，退出码 0。
        let runner = TokioCommandRunner;
        let out = runner
            .run("/bin/echo", &["hello-zfs".to_string()])
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.trim(), "hello-zfs");
        assert!(out.stderr.is_empty());
    }

    #[tokio::test]
    async fn runner_captures_nonzero_exit() {
        // /bin/false 立即以退出码 1 退出。
        let runner = TokioCommandRunner;
        let out = runner.run("/bin/false", &[]).await.unwrap();
        assert_ne!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn runner_captures_stderr() {
        // sh -c 'echo err >&2' 向 stderr 写入（echo 二进制无直接 stderr 重定向能力，
        // 用 sh 包装保证 stderr 真实分离捕获）。
        let runner = TokioCommandRunner;
        let out = runner
            .run(
                "/bin/sh",
                &[
                    "-c".to_string(),
                    "printf boom >&1; printf err >&2".to_string(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "boom");
        assert_eq!(out.stderr, "err");
    }

    #[tokio::test]
    async fn runner_missing_binary_maps_io_error() {
        // 不存在的程序 → tokio spawn 返回 io::Error → StorageError::Io（经 #[from]）。
        let runner = TokioCommandRunner;
        let err = runner
            .run("/usr/local/nonexistent-zfs-probe-xyz", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, StorageError::Io(_)),
            "应映射为 Io 错误，实际: {err:?}"
        );
    }
}

// ----------------------------------------------------------------------------
// 真实 ZFS 集成测（#[ignore]，需沙箱）
// ----------------------------------------------------------------------------
//
// 对应 docs/SANDBOX.md §5.2「ZfsCliBackend 真实 zpool/zfs」——在沙箱（方案 A：
// Docker privileged + ZFS-on-loop）跑 `cargo test --features mock -- --ignored`
// 才会执行。CI 三道门（非沙箱）跳过。
//
// 前提：环境装 zfsutils-linux、加载 zfs 模块、有可支配 loop 设备、以 root 跑。
// 用 `ZfsCliBackend::new()`（生产构造，TokioCommandRunner 真实 spawn zpool/zfs）。
//
// 命名约定：`real_*` + `#[ignore]` 标真实环境测。每个测自建自毁临时池，不污染宿主。
#[cfg(test)]
mod real_zfs_sandbox_tests {
    use super::*;
    use os_core::DatasetId;

    /// 临时池名前缀——避免与真实池冲突，测试后必须 destroy。
    const POOL_PREFIX: &str = "osprobe";

    /// 跳过条件：非沙箱环境（无 zfs / 非 root）时 panic 给出清晰提示。
    /// 沙箱内（Docker privileged + zfs 模块）应满足。
    fn require_real_zfs() {
        let probe = std::process::Command::new("zfs").arg("version").output();
        match probe {
            Ok(o) if o.status.success() => {}
            Ok(o) => panic!(
                "`zfs version` 退出码非 0（可能非 root 或模块缺失）：{}",
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => panic!("`zfs` 不在 PATH（非沙箱环境？）：{e}"),
        }
    }

    /// 生成唯一临时池名（带 PID + 时间戳，防并发测冲突）。
    fn unique_pool(tag: &str) -> String {
        format!(
            "{POOL_PREFIX}_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    /// 安全销毁池（即使前序断言失败也应调用——用 RAII guard）。
    struct PoolGuard {
        name: String,
    }
    impl Drop for PoolGuard {
        fn drop(&mut self) {
            // Drop 不能 async。尽力清理：开一个一次性 runtime 阻塞执行 destroy，
            // 忽略错误（池可能已被测主体 destroy 或环境不可用）。
            // 不用 Handle::current().block_on——panic 展开时可能已无 runtime。
            let name = self.name.clone();
            let _ = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?
                    .block_on(ZfsCliBackend::new().destroy_pool(&PoolId::new(name)))
                    .ok()
            })
            .join()
            .ok()
            .flatten();
            // 忽略返回值：失败（池已不存在 / 非沙箱）属预期。
        }
    }

    #[tokio::test]
    #[ignore = "需真实 ZFS 沙箱（Docker privileged + zfs 模块 + root）。跑法：cargo test -p os-storage --features mock -- --ignored real_create_and_list_pool"]
    async fn real_create_and_list_pool() {
        require_real_zfs();
        // 沙箱需提供空文件作为 vdev（zpool create 接受 file vdev；真实生产用块设备）。
        // 此测假设环境已备好一个可用 file vdev 路径（通过 OS_TEST_VDEV 环境变量传入）。
        let vdev = std::env::var("OS_TEST_VDEV").unwrap_or_else(|_| {
            panic!("请在沙箱设 OS_TEST_VDEV=<可写空文件路径>（如 truncate -s 64M 制作的稀疏文件）");
        });
        let backend = ZfsCliBackend::new();
        let pool_name = unique_pool("tank");
        let _guard = PoolGuard {
            name: pool_name.clone(),
        };

        // create → list 能读到
        let created = backend
            .create_pool(
                &PoolId::new(pool_name.clone()),
                vec![crate::model::VdevSpec {
                    kind: crate::model::VdevKind::Disk,
                    disks: vec![vdev],
                }],
            )
            .await
            .expect("zpool create 应成功");
        assert_eq!(created.id.as_str(), pool_name);

        let pools = backend.list_pools().await.expect("list_pools 应成功");
        assert!(
            pools.iter().any(|p| p.id.as_str() == pool_name),
            "新池应在 list_pools 结果中"
        );

        // destroy → list 读不到
        backend
            .destroy_pool(&PoolId::new(pool_name.clone()))
            .await
            .expect("destroy_pool 应成功");
        let pools = backend.list_pools().await.expect("list_pools 应成功");
        assert!(
            !pools.iter().any(|p| p.id.as_str() == pool_name),
            "destroy 后池不应再出现"
        );
    }

    #[tokio::test]
    #[ignore = "需真实 ZFS 沙箱（含已有 OS_TEST_POOL 环境变量指向可写池）。跑法：cargo test -p os-storage --features mock -- --ignored real_dataset_and_snapshot_lifecycle"]
    async fn real_dataset_and_snapshot_lifecycle() {
        require_real_zfs();
        let pool = std::env::var("OS_TEST_POOL").unwrap_or_else(|_| {
            panic!("请在沙箱设 OS_TEST_POOL=<可写 zfs 池名>（数据集创建在其下）");
        });
        let backend = ZfsCliBackend::new();
        let ds_name = format!("{}/osprobe_{}", pool, std::process::id());

        // create dataset
        let ds = backend
            .create_dataset(&DatasetId::new(ds_name.clone()), DatasetOptions::default())
            .await
            .expect("create_dataset 应成功");
        assert_eq!(ds.id.as_str(), ds_name);

        // snapshot
        let snap = backend
            .snapshot(&DatasetId::new(ds_name.clone()), "s1")
            .await
            .expect("snapshot 应成功");
        assert_eq!(snap.id.as_str(), format!("{ds_name}@s1"));

        // list snapshots 能读到
        let snaps = backend
            .list_snapshots(Some(&DatasetId::new(ds_name.clone())))
            .await
            .expect("list_snapshots 应成功");
        assert!(snaps
            .iter()
            .any(|s| s.id.as_str() == format!("{ds_name}@s1")));

        // set/get quota
        backend
            .set_quota(
                &DatasetId::new(ds_name.clone()),
                Quota {
                    refquota: Some(1_000_000),
                    refreservation: None,
                },
            )
            .await
            .expect("set_quota 应成功");
        let q = backend
            .get_quota(&DatasetId::new(ds_name.clone()))
            .await
            .expect("get_quota 应成功");
        assert_eq!(q.refquota, Some(1_000_000));

        // destroy dataset（-r 连同快照一起销毁）
        backend
            .destroy_dataset(&DatasetId::new(ds_name.clone()))
            .await
            .expect("destroy_dataset 应成功");
        let ds_list = backend
            .list_datasets(Some(&PoolId::new(pool)))
            .await
            .expect("list_datasets 应成功");
        assert!(!ds_list.iter().any(|d| d.id.as_str() == ds_name));
    }
}
