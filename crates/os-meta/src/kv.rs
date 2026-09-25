//! 分布式 KV——集群状态存储
//!
//! 决策依据：规划文档 §3.5 —— 所有需要集群一致性的状态（VIP owner / 节点角色 /
//! 配置项 / 锁）经 openraft log 复制写入分布式 KV，强一致。
//!
//! 设计要点：
//! - 写操作经 leader 复制到 quorum 后才提交（线性一致）
//! - `cas`（compare-and-swap）提供乐观锁：配置切换/选主竞选的无锁协调

use async_trait::async_trait;
use os_core::{DateTime, Deserialize, Serialize};

// ----------------------------------------------------------------------------
// KvEntry
// ----------------------------------------------------------------------------

/// KV 条目（集群状态存储单元）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KvEntry {
    /// 键
    pub key: String,
    /// 值（开放结构，消费方按 key 命名空间解析）
    pub value: serde_json::Value,
    /// 单调递增版本号（每次 put 自增；CAS 乐观锁依据）
    pub version: u64,
    /// 创建时间
    pub created_at: DateTime,
    /// 最近修改时间
    pub modified_at: DateTime,
}

// ----------------------------------------------------------------------------
// DistributedKv trait（async，经 openraft log 复制）
// ----------------------------------------------------------------------------

/// 分布式 KV——经 openraft log 复制的强一致 KV 存储。
///
/// 实现者：`OpenraftKv`（默认，状态机内嵌于 MetaStore）；其他实现可替换。
/// 并发约束：写操作经 leader 串行复制；`cas` 保证原子性。
///
/// 注：按 ADR-COMPAT-001，本 trait 经 `Box<dyn DistributedKv>` 运行期多态
/// （见 mock.rs `_assert_dyn_compatible`），故用 `#[async_trait]`；方法签名未变。
#[async_trait]
pub trait DistributedKv: Send + Sync {
    /// 写入（创建或覆盖）。
    /// 返回写入后的条目（含自增后的新 version）。
    async fn put(&self, key: &str, value: serde_json::Value) -> Result<KvEntry, crate::MetaError>;

    /// 读取；不存在返回 None。
    async fn get(&self, key: &str) -> Option<KvEntry>;

    /// 删除；不存在视为成功。
    async fn delete(&self, key: &str) -> Result<(), crate::MetaError>;

    /// 按前缀枚举（只读，可由任意节点本地查询快照）。
    async fn list(&self, prefix: &str) -> Vec<KvEntry>;

    /// 乐观锁 CAS（compare-and-swap）。
    ///
    /// - `expected_version = None` 表示"键必须不存在"（仅创建）
    /// - `expected_version = Some(v)` 表示"键当前版本必须等于 v"（仅更新）
    ///
    /// 版本不符返回 `CasConflict`。
    async fn cas(
        &self,
        key: &str,
        expected_version: Option<u64>,
        new_value: serde_json::Value,
    ) -> Result<KvEntry, crate::MetaError>;
}

// ----------------------------------------------------------------------------
// 单元测试：KvEntry serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::Utc;

    fn sample_entry(version: u64) -> KvEntry {
        let now = Utc::now();
        KvEntry {
            key: "vip/owner".into(),
            value: serde_json::json!({"node": "n1"}),
            version,
            created_at: now,
            modified_at: now,
        }
    }

    #[test]
    fn kv_entry_serde_roundtrip_object_value() {
        let e = sample_entry(3);
        let json = serde_json::to_string(&e).unwrap();
        let back: KvEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.key, e.key);
        assert_eq!(back.value, e.value);
        assert_eq!(back.version, e.version);
        assert_eq!(back.created_at, e.created_at);
        assert_eq!(back.modified_at, e.modified_at);
    }

    #[test]
    fn kv_entry_serde_roundtrip_scalar_value() {
        let e = KvEntry {
            key: "counter".into(),
            value: serde_json::json!(42),
            version: 1,
            created_at: Utc::now(),
            modified_at: Utc::now(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: KvEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value, serde_json::json!(42));
    }

    #[test]
    fn kv_entry_serde_roundtrip_array_value() {
        let e = KvEntry {
            key: "list".into(),
            value: serde_json::json!([1, 2, 3]),
            version: 1,
            created_at: Utc::now(),
            modified_at: Utc::now(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: KvEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn kv_entry_serde_roundtrip_null_value() {
        let e = KvEntry {
            key: "null-key".into(),
            value: serde_json::Value::Null,
            version: 1,
            created_at: Utc::now(),
            modified_at: Utc::now(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: KvEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value, serde_json::Value::Null);
    }

    #[test]
    fn kv_entry_clone_preserves_all_fields() {
        let e = sample_entry(5);
        let c = e.clone();
        assert_eq!(e.key, c.key);
        assert_eq!(e.version, c.version);
        assert_eq!(e.created_at, c.created_at);
    }

    #[test]
    fn kv_entry_version_monotonic_in_test_helper() {
        let e1 = sample_entry(1);
        let e2 = sample_entry(2);
        assert!(e2.version > e1.version);
    }
}
