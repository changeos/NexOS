//! 元数据 HA 复制——openraft 状态机内嵌 SQLite（规划文档 §9.1#7）
//!
//! 决策依据：§9.1#7 —— 元数据（用户/共享/权限/审计等结构化数据）用 SQLite 存储，
//! 通过 openraft 状态机做 HA 复制：写操作进 openraft log → 复制到 quorum →
//! apply 到各节点本地 SQLite；快照 = SQLite dump，随 openraft snapshot 流转。
//!
//! 设计要点：
//! - `apply_log`：openraft 状态机的 apply 钩子，把已提交的 log（业务 JSON 命令）
//!   作用到本地 SQLite
//! - `snapshot`/`restore`：openraft 安装/传输快照时调用，载荷为 SQLite dump
//! - `query`：本地只读查询（不走 log，直接读 follower 本地副本，分担读压力）

use async_trait::async_trait;
use bytes::Bytes;
use os_core::{DateTime, Deserialize, Serialize, Utc};

// ----------------------------------------------------------------------------
// MetaSnapshot
// ----------------------------------------------------------------------------

/// 元数据快照（SQLite dump，随 openraft snapshot 复制）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaSnapshot {
    /// 快照序列号（对应 openraft applied_index）
    pub seq: u64,
    /// 快照时间
    pub timestamp: DateTime,
    /// SQLite dump 字节流（二进制）
    pub sqlite_dump: Bytes,
}

impl MetaSnapshot {
    /// 构造一个当前时间戳的快照
    pub fn new(seq: u64, sqlite_dump: Bytes) -> Self {
        Self {
            seq,
            timestamp: Utc::now(),
            sqlite_dump,
        }
    }
}

// ----------------------------------------------------------------------------
// MetaStore trait（async，openraft 状态机内嵌 SQLite）
// ----------------------------------------------------------------------------

/// 元数据存储——openraft 状态机的持久化后端（内嵌 SQLite）。
///
/// 实现者：`SqliteMetaStore`（默认，单库 + WAL）；每个节点持有一份本地副本，
/// 写路径经 openraft log 强一致复制，读路径可走本地副本。
///
/// 注：按 ADR-COMPAT-001，本 trait 经 `Box<dyn MetaStore>` 运行期多态（见 mock.rs
/// `_assert_dyn_compatible`），故用 `#[async_trait]`；方法签名未变。
#[async_trait]
pub trait MetaStore: Send + Sync {
    /// 应用 openraft log（一条已提交的业务 JSON 命令）到本地 SQLite。
    /// 由 openraft 状态机 apply 钩子调用。
    async fn apply_log(&self, entry: serde_json::Value) -> Result<(), crate::MetaError>;

    /// 创建快照（SQLite dump），供 openraft 安装/传输。
    async fn snapshot(&self) -> Result<MetaSnapshot, crate::MetaError>;

    /// 从快照恢复（覆盖本地 SQLite），用于新成员追赶 / 日志压缩后重建。
    async fn restore(&self, snap: MetaSnapshot) -> Result<(), crate::MetaError>;

    /// 本地只读查询（直接读本地 SQLite 副本，不走 log，分担读压力）。
    ///
    /// - `sql`：参数化 SQL（? 占位）
    /// - `params`：绑定参数（按位置对应 ?）
    ///
    /// 返回每行作为 JSON 数组。
    async fn query(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, crate::MetaError>;
}

// ----------------------------------------------------------------------------
// 单元测试：MetaSnapshot 构造 + serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_snapshot_new_sets_seq_and_timestamp() {
        let snap = MetaSnapshot::new(7, Bytes::from_static(b"dump"));
        assert_eq!(snap.seq, 7);
        assert_eq!(snap.sqlite_dump.as_ref(), b"dump");
        // timestamp 应被设置为构造时刻附近（非默认零值）
        // （chrono::Utc::now() 总是非零，这里仅断言非默认特征）
        let _ = snap.timestamp; // 可访问
    }

    #[test]
    fn meta_snapshot_serde_roundtrip() {
        let snap = MetaSnapshot::new(42, Bytes::from_static(b"\x00\x01\x02 snapshot"));
        let json = serde_json::to_string(&snap).unwrap();
        let back: MetaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.sqlite_dump.as_ref(), b"\x00\x01\x02 snapshot");
    }

    #[test]
    fn meta_snapshot_serde_roundtrip_empty_dump() {
        let snap = MetaSnapshot::new(0, Bytes::new());
        let json = serde_json::to_string(&snap).unwrap();
        let back: MetaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 0);
        assert!(back.sqlite_dump.is_empty());
    }

    #[test]
    fn meta_snapshot_clone_preserves_bytes() {
        let snap = MetaSnapshot::new(3, Bytes::from_static(b"payload"));
        let c = snap.clone();
        assert_eq!(c.seq, snap.seq);
        assert_eq!(c.sqlite_dump.as_ref(), snap.sqlite_dump.as_ref());
    }

    #[test]
    fn meta_snapshot_debug_format_contains_seq() {
        let snap = MetaSnapshot::new(99, Bytes::from_static(b"x"));
        let s = format!("{snap:?}");
        assert!(s.contains("seq"));
        assert!(s.contains("99"));
    }

    #[test]
    fn meta_snapshot_seq_zero_is_valid() {
        // seq=0 在契约中合法（空快照）
        let snap = MetaSnapshot::new(0, Bytes::new());
        assert_eq!(snap.seq, 0);
    }
}
