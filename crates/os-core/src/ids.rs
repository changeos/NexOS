//! 领域 newtype ID（全局规范 §1.3：所有领域 ID 用 newtype，避免裸 String 混淆）
//!
//! 设计原则：
//! - 每个 ID 是独立类型，编译期防止互赋（PoolId 不会误传给 VmId）
//! - 内部多为 String（人类可读名）或 Uuid（全局唯一）；均实现 Serialize/Clone
//! - 不在此处校验格式（校验属业务层，在各 crate 的 create_* 方法里做）

use serde::{Deserialize, Serialize};

/// 通用 ID 宏：生成一个 newtype，自动派生常用 trait
macro_rules! string_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(PoolId, "ZFS 存储池 ID（如 `tank`）");
string_id!(DatasetId, "ZFS 数据集 ID（如 `tank/media`）");
string_id!(SnapshotId, "ZFS 快照 ID（如 `tank/media@snap1`）");
string_id!(VmId, "虚拟机 ID");
string_id!(ContainerId, "容器 ID");
string_id!(GuestId, "访客 ID（格式 GUEST-XXXXXX，见 §3.18）");
string_id!(NodeId, "集群节点 ID（openraft 成员标识）");
string_id!(ShareId, "文件共享 ID（SMB/NFS/WebDAV 共享）");
string_id!(
    VolumeId,
    "块存储卷 ID（zvol / iSCSI LUN / NVMe-oF namespace）"
);
string_id!(WalletSessionId, "钱包连接会话 ID（WalletConnect session）");
string_id!(ChainId, "区块链标识（如 `bitcoin` / `ethereum` / `base`）");
string_id!(AddressId, "链上地址（BTC 地址 / EVM 地址）");

/// 任务 ID——全局唯一，用于异步任务追踪（备份/迁移/复制/agent 委派任务等）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

use crate::Uuid;
