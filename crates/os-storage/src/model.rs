//! 存储领域模型 —— Pool / Dataset / Snapshot / Vdev / Quota / 加密配置
//!
//! 这些结构体描述 ZFS 拓扑的「读模型」（来自 `zpool list` / `zfs list` 的解析结果），
//! 以及创建时的「规格」（VdevSpec）。ID 复用 os-core 的 newtype。

use crate::error::StorageError;
use os_core::{Capacity, DatasetId, Deserialize, Health, PoolId, Serialize, SnapshotId};

// ----------------------------------------------------------------------------
// Vdev 规格 / 实例
// ----------------------------------------------------------------------------

/// vdev 类型（创建池时声明的磁盘冗余级别）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VdevKind {
    /// 单盘（无冗余）
    Disk,
    /// 镜像（2 盘镜像，等价 RAID1）
    Mirror,
    /// RAID-Z1（单校验，等价 RAID5）
    Raidz1,
    /// RAID-Z2（双校验，等价 RAID6）
    Raidz2,
    /// RAID-Z3（三校验）
    Raidz3,
}

impl VdevKind {
    /// 对应 `zpool create` 命令行里的 vdev 关键字（如 `mirror`/`raidz1`）。
    /// `Disk` 返回空字符串——单盘在 zpool 命令里不带前缀关键字。
    pub fn as_zpool_keyword(&self) -> &'static str {
        match self {
            VdevKind::Disk => "",
            VdevKind::Mirror => "mirror",
            VdevKind::Raidz1 => "raidz1",
            VdevKind::Raidz2 => "raidz2",
            VdevKind::Raidz3 => "raidz3",
        }
    }

    /// 从 `zpool status` 输出里的 vdev 类型字符串反解析。
    /// `""`/`disk`/单盘 → Disk；`mirror` → Mirror；`raidz1`/`raidz-1` → Raidz1，依此类推。
    /// 未知类型返回 None（调用方决定如何处理）。
    pub fn from_status_str(s: &str) -> Option<Self> {
        match s.trim() {
            "" | "disk" => Some(VdevKind::Disk),
            "mirror" => Some(VdevKind::Mirror),
            "raidz1" | "raidz-1" => Some(VdevKind::Raidz1),
            "raidz2" | "raidz-2" => Some(VdevKind::Raidz2),
            "raidz3" | "raidz-3" => Some(VdevKind::Raidz3),
            _ => None,
        }
    }
}

/// 创建池时声明的 vdev 规格（一组磁盘 + 冗余级别）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VdevSpec {
    /// 冗余级别
    pub kind: VdevKind,
    /// 成员盘设备路径（如 `/dev/sdb` / `/dev/disk/by-id/...`）
    pub disks: Vec<String>,
}

/// vdev 运行时实例（来自 `zpool status` 的解析）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vdev {
    /// 冗余级别
    pub kind: VdevKind,
    /// 成员盘设备路径
    pub disks: Vec<String>,
    /// 该 vdev 的健康状态（单盘/镜像/raidz 整体状态）
    pub health: Health,
    /// 读错误计数（来自 `zpool status` 的 READ 列；0 表示无错）
    pub read_errors: u64,
    /// 写错误计数（来自 `zpool status` 的 WRITE 列；0 表示无错）
    pub write_errors: u64,
    /// 校验和错误计数（来自 `zpool status` 的 CKSUM 列；0 表示无错）
    pub cksum_errors: u64,
}

// ----------------------------------------------------------------------------
// Pool（存储池）
// ----------------------------------------------------------------------------

/// ZFS 存储池（顶层容器）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    /// 池 ID（如 `tank`）
    pub id: PoolId,
    /// 池名（与 ID 同，保留双字段便于未来分离显示名）
    pub name: String,
    /// 组成该池的 vdev 列表
    pub vdevs: Vec<Vdev>,
    /// 容量（已用/总量）
    pub capacity: Capacity,
    /// 池整体健康状态
    pub health: Health,
}

impl Pool {
    /// 从 `zpool list -p -H` 单行输出解析 Pool（不含 vdev 明细——vdev 需 `zpool status`）。
    ///
    /// `zpool list -p -H` 字段顺序（tab 分隔）：
    /// `NAME  SIZE  ALLOC  FREE  CKPOINT  EXPANDSZ  FRAG  CAP  DEDUP  HEALTH  ALTROOT`
    /// （CAP 是百分比，如 `12`；HEALTH 是 `ONLINE`/`DEGRADED`/`FAULTED`/`UNAVAIL`/`REMOVED`）
    ///
    /// vdev 列表由调用方（`ZfsCliBackend::list_pools`）后续用 `zpool status` 补全，
    /// 此处返回的 Pool 的 `vdevs` 为空 Vec（list 命令不含 vdev 信息）。
    pub fn from_list_line(line: &str) -> Result<Self, StorageError> {
        let cols: Vec<&str> = line.split('\t').map(|c| c.trim()).collect();
        if cols.len() < 10 {
            return Err(StorageError::CommandFailed(format!(
                "zpool list 行字段不足（{} < 10）：{line:?}",
                cols.len()
            )));
        }
        let name = cols[0].to_string();
        let total_bytes = parse_bytes(cols[1])
            .ok_or_else(|| StorageError::CommandFailed(format!("SIZE 非法: {:?}", cols[1])))?;
        let alloc_bytes = parse_bytes(cols[2])
            .ok_or_else(|| StorageError::CommandFailed(format!("ALLOC 非法: {:?}", cols[2])))?;
        // 字段顺序（0 基下标，tab 分隔）：
        //   0 NAME  1 SIZE  2 ALLOC  3 FREE  4 CKPOINT  5 EXPANDSZ
        //   6 FRAG  7 CAP   8 DEDUP  9 HEALTH  10 ALTROOT
        // （原代码误取 cols[8] = DEDUP 列，导致 health 恒为 Unknown；修正为 cols[9]）
        let health = parse_health(cols[9]);
        Ok(Pool {
            id: PoolId::new(name.clone()),
            name,
            vdevs: Vec::new(),
            capacity: Capacity {
                used_bytes: alloc_bytes,
                total_bytes,
            },
            health,
        })
    }
}

// ----------------------------------------------------------------------------
// Dataset（数据集）
// ----------------------------------------------------------------------------

/// 数据集加密状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionState {
    /// 未加密
    Off,
    /// 已加密且密钥已加载（可读写）
    Unlocked,
    /// 已加密但密钥未加载（不可访问）
    Locked,
}

/// 数据集（文件系统或 zvol 的统一抽象）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    /// 数据集 ID（如 `tank/media`）
    pub id: DatasetId,
    /// 所属池 ID
    pub pool: PoolId,
    /// 数据集名（与 ID 同，便于显示）
    pub name: String,
    /// 已用空间（字节）
    pub used_bytes: u64,
    /// 可用空间（字节）
    pub avail_bytes: u64,
    /// 是否已挂载
    pub mounted: bool,
    /// 加密状态
    pub encryption: EncryptionState,
}

impl Dataset {
    /// 从 `zfs list -p -H -o name,used,avail,mounted,encryption <pool>` 单行解析 Dataset。
    ///
    /// 字段说明（均 tab 分隔，`-p` 保证数值为精确整数字节）：
    /// - `name`：如 `tank/media`（含池名前缀，以首个 `/` 分割出 pool）
    /// - `used`：已用字节
    /// - `avail`：可用字节
    /// - `mounted`：`yes`/`no`（`-p` 下为布尔字符串）
    /// - `encryption`：`off`/`on`/`-`（`-` 表示未设置）
    ///
    /// 注意：ZFS 的 `encryption` 属性 `on` 仅表示已配置加密，不区分密钥是否已加载。
    /// 真正区分 Locked/Unlocked 需查 `keystatus`（`available`/`unavailable`/`none`）。
    /// 此处保守地：`on` → Unlocked（调用方若需精确，用 [`Self::with_keystatus`] 修正）。
    pub fn from_list_line(line: &str) -> Result<Self, StorageError> {
        let cols: Vec<&str> = line.split('\t').map(|c| c.trim()).collect();
        if cols.len() < 5 {
            return Err(StorageError::CommandFailed(format!(
                "zfs list 行字段不足（{} < 5）：{line:?}",
                cols.len()
            )));
        }
        let full_name = cols[0];
        let (pool_name, _ds_name) = full_name.split_once('/').unwrap_or((full_name, ""));
        let used_bytes = parse_bytes(cols[1])
            .ok_or_else(|| StorageError::CommandFailed(format!("used 非法: {:?}", cols[1])))?;
        let avail_bytes = parse_bytes(cols[2])
            .ok_or_else(|| StorageError::CommandFailed(format!("avail 非法: {:?}", cols[2])))?;
        let mounted = matches!(cols[3].to_ascii_lowercase().as_str(), "yes" | "true");
        let encryption = parse_encryption_state(cols[4]);
        Ok(Dataset {
            id: DatasetId::new(full_name),
            pool: PoolId::new(pool_name.to_string()),
            name: full_name.to_string(),
            used_bytes,
            avail_bytes,
            mounted,
            encryption,
        })
    }

    /// 用 `keystatus` 列修正加密状态（区分 Locked/Unlocked）。
    /// `keystatus` 取值：`available`（已加载）、`unavailable`（未加载）、`none`（未加密）。
    #[must_use]
    pub fn with_keystatus(mut self, keystatus: &str) -> Self {
        self.encryption = match keystatus.trim() {
            "available" => EncryptionState::Unlocked,
            "unavailable" => EncryptionState::Locked,
            _ => EncryptionState::Off,
        };
        self
    }
}

// ----------------------------------------------------------------------------
// Snapshot（快照）
// ----------------------------------------------------------------------------

/// ZFS 快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// 快照 ID（如 `tank/media@snap1`）
    pub id: SnapshotId,
    /// 所属数据集 ID
    pub dataset: DatasetId,
    /// 创建时间
    pub created: chrono::DateTime<chrono::Utc>,
    /// 该快照独占占用空间（字节）
    pub used_bytes: u64,
}

impl Snapshot {
    /// 从 `zfs list -t snapshot -p -H -o name,used,creation <dataset>` 单行解析 Snapshot。
    ///
    /// 字段（tab 分隔，`-p` 精确值）：
    /// - `name`：完整快照名 `tank/media@snap1`（以 `@` 分割出 dataset 与快照名）
    /// - `used`：该快照独占占用字节
    /// - `creation`：Unix 时间戳（秒，`-p` 输出）
    pub fn from_list_line(line: &str) -> Result<Self, StorageError> {
        let cols: Vec<&str> = line.split('\t').map(|c| c.trim()).collect();
        if cols.len() < 3 {
            return Err(StorageError::CommandFailed(format!(
                "zfs snapshot list 行字段不足（{} < 3）：{line:?}",
                cols.len()
            )));
        }
        let full = cols[0];
        let (dataset_name, _snap_name) = full
            .split_once('@')
            .ok_or_else(|| StorageError::CommandFailed(format!("快照名缺 @：{full:?}")))?;
        let used_bytes = parse_bytes(cols[1])
            .ok_or_else(|| StorageError::CommandFailed(format!("used 非法: {:?}", cols[1])))?;
        let creation_secs = cols[2].parse::<i64>().map_err(|e| {
            StorageError::CommandFailed(format!("creation 非法 {:?}: {e}", cols[2]))
        })?;
        let created = chrono::DateTime::<chrono::Utc>::from_timestamp(creation_secs, 0)
            .ok_or_else(|| {
                StorageError::CommandFailed(format!("creation 时间戳越界: {creation_secs}"))
            })?;
        Ok(Snapshot {
            id: SnapshotId::new(full),
            dataset: DatasetId::new(dataset_name.to_string()),
            created,
            used_bytes,
        })
    }
}

// ----------------------------------------------------------------------------
// Quota（配额）
// ----------------------------------------------------------------------------

/// 数据集配额（refquota/refreservation，字节）
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Quota {
    /// refquota：该数据集自身（不含子数据集）可用上限
    pub refquota: Option<u64>,
    /// refreservation：保证预留的最低空间（不被子数据集抢占）
    pub refreservation: Option<u64>,
}

// ----------------------------------------------------------------------------
// 加密配置
// ----------------------------------------------------------------------------

/// 加密配置（创建/加载加密数据集时声明）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    /// 是否启用加密
    pub enabled: bool,
    /// 加密算法（如 `aes-256-gcm`；off 时忽略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    /// 密钥位置（如 `prompt` / `file:///etc/keys/tank.key` / `pkcs11:...`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keylocation: Option<String>,
    /// 密钥格式（`raw` / `hex` / `passphrase`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyformat: Option<String>,
}

// ----------------------------------------------------------------------------
// CLI 输出解析辅助（`-p -H` 机器可读格式的字段解析）
// ----------------------------------------------------------------------------

/// 解析 `-p` 输出的字节数值。`-p` 模式下数值已是精确整数（无单位后缀）。
/// 兼容 `-`（无值/不适用）与负数（极少见）。
pub(crate) fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s == "-" {
        return Some(0);
    }
    s.parse::<u64>().ok()
}

/// 解析 ZFS/zpool 健康字符串到 [`Health`]。
///
/// `zpool list -H` 的 HEALTH 列：`ONLINE`/`DEGRADED`/`FAULTED`/`UNAVAIL`/`REMOVED`。
/// `zpool status` 里单盘还可能出现 `DEGRADED` 等。
pub(crate) fn parse_health(s: &str) -> Health {
    match s.trim().to_ascii_uppercase().as_str() {
        "ONLINE" => Health::Healthy,
        "DEGRADED" => Health::Degraded,
        "FAULTED" | "UNAVAIL" | "REMOVED" | "SUSPENDED" | "OFFLINE" => Health::Unhealthy,
        _ => Health::Unknown,
    }
}

/// 健康解析的 crate 内公共入口（[`parse_health`] 的 thin wrapper）。
///
/// 之所以单独命名而非把 `parse_health` 改 pub：`parse_health` 已被本模块多处内部
/// 调用且是 `pub(crate)`，跨模块引用用独立名字避免 visibility 改动波及面。
pub(crate) fn parse_health_public(s: &str) -> Health {
    parse_health(s)
}

/// 解析 `encryption` 属性字符串（`off`/`-` → Off；其余（`on`/具体算法）→ Unlocked 保守值）。
pub(crate) fn parse_encryption_state(s: &str) -> EncryptionState {
    match s.trim() {
        "" | "-" | "off" | "none" => EncryptionState::Off,
        _ => EncryptionState::Unlocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_list_line() -> &'static str {
        // 真实 `zpool list -p -H` 输出样本（tank 池，10TB 总量）
        "tank\t10995116277760\t1374389534720\t9620726743040\t-\t-\t12\t12\t1.00x\tONLINE\t-"
    }

    #[test]
    fn parses_pool_list_line() {
        let pool = Pool::from_list_line(pool_list_line()).unwrap();
        assert_eq!(pool.id.as_str(), "tank");
        assert_eq!(pool.name, "tank");
        assert_eq!(pool.capacity.total_bytes, 10_995_116_277_760);
        assert_eq!(pool.capacity.used_bytes, 1_374_389_534_720);
        assert_eq!(pool.health, Health::Healthy);
        assert!(pool.vdevs.is_empty(), "list 行不含 vdev 明细");
    }

    #[test]
    fn pool_list_line_degraded() {
        let line =
            "backup\t2000000000000\t1500000000000\t500000000000\t-\t-\t5\t75\t1.00x\tDEGRADED\t-";
        let pool = Pool::from_list_line(line).unwrap();
        assert_eq!(pool.health, Health::Degraded);
        assert_eq!(pool.capacity.used_ratio(), 0.75);
    }

    #[test]
    fn pool_list_line_too_few_cols() {
        let line = "tank\t100";
        let err = Pool::from_list_line(line).unwrap_err();
        assert!(matches!(err, StorageError::CommandFailed(_)));
    }

    #[test]
    fn parses_dataset_list_line() {
        // zfs list -p -H -o name,used,avail,mounted,encryption
        let line = "tank/media\t5497558138880\t5497558138880\tyes\toff";
        let ds = Dataset::from_list_line(line).unwrap();
        assert_eq!(ds.id.as_str(), "tank/media");
        assert_eq!(ds.pool.as_str(), "tank");
        assert_eq!(ds.used_bytes, 5_497_558_138_880);
        assert_eq!(ds.avail_bytes, 5_497_558_138_880);
        assert!(ds.mounted);
        assert_eq!(ds.encryption, EncryptionState::Off);
    }

    #[test]
    fn parses_encrypted_dataset_list_line() {
        let line = "vault/secret\t1048576\t10485760\tno\taes-256-gcm";
        let ds = Dataset::from_list_line(line).unwrap();
        assert_eq!(ds.pool.as_str(), "vault");
        assert!(!ds.mounted);
        assert_eq!(ds.encryption, EncryptionState::Unlocked); // 保守值
                                                              // 用 keystatus 精确修正
        let ds = ds.with_keystatus("unavailable");
        assert_eq!(ds.encryption, EncryptionState::Locked);
    }

    #[test]
    fn dataset_root_pool_no_slash() {
        // 顶层 dataset（pool 本身作为 dataset）
        let line = "tank\t100\t200\tyes\toff";
        let ds = Dataset::from_list_line(line).unwrap();
        assert_eq!(ds.pool.as_str(), "tank");
    }

    #[test]
    fn parses_snapshot_list_line() {
        // zfs list -t snapshot -p -H -o name,used,creation
        // creation = 1700000000（2023-11-14）
        let line = "tank/media@snap1\t1073741824\t1700000000";
        let snap = Snapshot::from_list_line(line).unwrap();
        assert_eq!(snap.id.as_str(), "tank/media@snap1");
        assert_eq!(snap.dataset.as_str(), "tank/media");
        assert_eq!(snap.used_bytes, 1_073_741_824);
        assert_eq!(snap.created.timestamp(), 1_700_000_000);
    }

    #[test]
    fn snapshot_missing_at_errors() {
        let line = "tank-media-snap1\t0\t1700000000";
        let err = Snapshot::from_list_line(line).unwrap_err();
        assert!(matches!(err, StorageError::CommandFailed(_)));
    }

    #[test]
    fn vdev_kind_keyword_round_trip() {
        for kind in [
            VdevKind::Mirror,
            VdevKind::Raidz1,
            VdevKind::Raidz2,
            VdevKind::Raidz3,
        ] {
            let kw = kind.as_zpool_keyword();
            assert_eq!(VdevKind::from_status_str(kw), Some(kind));
        }
        assert_eq!(VdevKind::Disk.as_zpool_keyword(), "");
        assert_eq!(VdevKind::from_status_str("disk"), Some(VdevKind::Disk));
        assert_eq!(VdevKind::from_status_str("raidz-2"), Some(VdevKind::Raidz2));
        assert_eq!(VdevKind::from_status_str("unknown"), None);
    }

    #[test]
    fn parse_bytes_handles_dash_and_empty() {
        assert_eq!(parse_bytes("-"), Some(0));
        assert_eq!(parse_bytes(""), Some(0));
        assert_eq!(parse_bytes("12345"), Some(12_345));
        assert_eq!(parse_bytes("12.5"), None); // -p 不应有小数
    }

    #[test]
    fn parse_health_variants() {
        assert_eq!(parse_health("ONLINE"), Health::Healthy);
        assert_eq!(parse_health("degraded"), Health::Degraded); // 大小写不敏感
        assert_eq!(parse_health("FAULTED"), Health::Unhealthy);
        assert_eq!(parse_health("weird"), Health::Unknown);
    }
}
