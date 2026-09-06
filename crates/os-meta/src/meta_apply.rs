//! MetaStore apply 命令分发模型——把业务 JSON 命令作用到内存表（apply_log 的纯算法）。
//!
//! 规格（启动 prompt §2）：`apply_log` 把业务 JSON 命令作用到 SQLite 的**命令分发模型**
//! （不真连 SQLite，用内存表模拟或 fixture）。
//!
//! 本模块提供：
//! - [`MetaCommand`]：从 JSON 反序列化的业务命令（put/delete/用户类操作）
//! - [`MetaTable`]：内存表（行=JSON 对象，主键索引）
//! - [`InMemoryMetaState`]：所有表的状态集合 + apply 命令
//!
//! `SqliteMetaStore`（实现待 openraft/sqlite 注册后填充）将复用 `MetaCommand` 的命令模型，
//! 仅替换后端为真实 SQLite。本模块的纯函数 apply 逻辑可独立测试。

use std::collections::BTreeMap;

use os_core::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// MetaCommand（apply_log 的载荷模型）
// ----------------------------------------------------------------------------

/// apply_log 的业务命令模型。
///
/// 设计：以 KV 语义为最小公共子集（与 `DistributedKv` 对齐），
/// 表名 + 主键值定位行，value 为行内容。
/// 后续扩展（用户/共享/权限等表）可在 `kind` 增加 variant，apply 时分发。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MetaCommand {
    /// 插入或覆盖一行（UPSERT 语义）
    Put {
        /// 表名（如 "users" / "shares" / "kv"）
        table: String,
        /// 主键值（JSON，支持字符串/数字）
        key: serde_json::Value,
        /// 行内容（JSON 对象）
        value: serde_json::Value,
    },
    /// 删除一行
    Delete {
        /// 表名
        table: String,
        /// 主键值
        key: serde_json::Value,
    },
}

impl MetaCommand {
    /// 反序列化一条 openraft log 命令。
    pub fn from_json(entry: &serde_json::Value) -> Result<Self, crate::MetaError> {
        serde_json::from_value::<MetaCommand>(entry.clone())
            .map_err(|e| crate::MetaError::ApplyFailed(format!("非法 apply 命令: {e}")))
    }
}

// ----------------------------------------------------------------------------
// MetaTable / InMemoryMetaState
// ----------------------------------------------------------------------------

/// 内存表：主键 → 行（按 JSON 字符串规范化主键，便于比较）。
///
/// 派生 `Serialize`/`Deserialize` 以支持 `SqliteMetaStore::snapshot`/`restore`
/// 把整个状态序列化为字节流（模拟 SQLite dump）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetaTable {
    rows: BTreeMap<String, serde_json::Value>,
}

impl MetaTable {
    /// 规范化主键（用 JSON 字符串表示，保证 {"k":"a"} 与 {"k":"a"} 相等）。
    fn norm_key(key: &serde_json::Value) -> String {
        // 用 compact JSON 序列化保证一致性
        serde_json::to_string(key).unwrap_or_else(|_| String::new())
    }

    /// UPSERT 一行。
    pub fn put(&mut self, key: serde_json::Value, value: serde_json::Value) {
        self.rows.insert(Self::norm_key(&key), value);
    }

    /// 删除一行（不存在视为成功）。
    pub fn delete(&mut self, key: &serde_json::Value) {
        self.rows.remove(&Self::norm_key(key));
    }

    /// 查询一行。
    pub fn get(&self, key: &serde_json::Value) -> Option<&serde_json::Value> {
        self.rows.get(&Self::norm_key(key))
    }

    /// 行数。
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 枚举所有行（任意顺序，按规范化键排序）。
    pub fn rows(&self) -> impl Iterator<Item = (&String, &serde_json::Value)> {
        self.rows.iter()
    }
}

/// 内存状态集合（所有表的 apply 结果）。
///
/// `SqliteMetaStore.apply_log` 的纯算法等价物：把每条命令作用到对应表，
/// 维护一份可查询的内存副本。SQLite 实现就绪前用于测试与 Mock。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InMemoryMetaState {
    tables: BTreeMap<String, MetaTable>,
    /// 已应用的命令计数（便于断言 apply 顺序）
    pub applied_count: u64,
}

impl InMemoryMetaState {
    /// 创建空状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用一条命令（put/delete 分发到对应表）。返回是否改变了状态。
    pub fn apply(&mut self, cmd: &MetaCommand) -> Result<bool, crate::MetaError> {
        match cmd {
            MetaCommand::Put { table, key, value } => {
                let t = self.tables.entry(table.clone()).or_default();
                let norm = MetaTable::norm_key(key);
                let changed = t.rows.get(&norm) != Some(value);
                t.put(key.clone(), value.clone());
                self.applied_count += 1;
                Ok(changed)
            }
            MetaCommand::Delete { table, key } => {
                if let Some(t) = self.tables.get_mut(table) {
                    let existed = t.rows.remove(&MetaTable::norm_key(key)).is_some();
                    if existed {
                        self.applied_count += 1;
                    }
                    Ok(existed)
                } else {
                    // 表不存在视为 no-op 成功
                    Ok(false)
                }
            }
        }
    }

    /// 应用一条原始 JSON 命令（便利方法，先反序列化再 apply）。
    pub fn apply_json(&mut self, entry: &serde_json::Value) -> Result<bool, crate::MetaError> {
        let cmd = MetaCommand::from_json(entry)?;
        self.apply(&cmd)
    }

    /// 取表（不存在返回空表的快照语义：返回 None 表示表未创建）。
    pub fn table(&self, name: &str) -> Option<&MetaTable> {
        self.tables.get(name)
    }

    /// 取表（不存在则创建空表）。
    pub fn table_or_create(&mut self, name: &str) -> &mut MetaTable {
        self.tables.entry(name.to_string()).or_default()
    }

    /// 表数量。
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_from_json_put() {
        let v = json!({"op":"put","table":"users","key":"u1","value":{"name":"alice"}});
        let cmd = MetaCommand::from_json(&v).expect("parse put");
        match cmd {
            MetaCommand::Put { table, key, value } => {
                assert_eq!(table, "users");
                assert_eq!(key, json!("u1"));
                assert_eq!(value, json!({"name":"alice"}));
            }
            _ => panic!("expected Put"),
        }
    }

    #[test]
    fn command_from_json_delete() {
        let v = json!({"op":"delete","table":"users","key":"u1"});
        let cmd = MetaCommand::from_json(&v).expect("parse delete");
        assert!(matches!(cmd, MetaCommand::Delete { .. }));
    }

    #[test]
    fn command_invalid_returns_apply_failed() {
        let v = json!({"op":"unknown"});
        let err = MetaCommand::from_json(&v).unwrap_err();
        assert!(matches!(err, crate::MetaError::ApplyFailed(_)));
    }

    #[test]
    fn apply_put_then_get() {
        let mut s = InMemoryMetaState::new();
        let cmd = MetaCommand::Put {
            table: "kv".into(),
            key: json!("k1"),
            value: json!({"v": 1}),
        };
        assert!(s.apply(&cmd).unwrap());
        let t = s.table("kv").unwrap();
        assert_eq!(t.get(&json!("k1")), Some(&json!({"v": 1})));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn apply_put_idempotent_no_change() {
        let mut s = InMemoryMetaState::new();
        let cmd = MetaCommand::Put {
            table: "kv".into(),
            key: json!("k1"),
            value: json!({"v": 1}),
        };
        assert!(s.apply(&cmd).unwrap()); // changed
        assert!(!s.apply(&cmd).unwrap()); // no change（值相同）
    }

    #[test]
    fn apply_delete_existing() {
        let mut s = InMemoryMetaState::new();
        s.apply(&MetaCommand::Put {
            table: "kv".into(),
            key: json!("k1"),
            value: json!(1),
        })
        .unwrap();
        assert!(s
            .apply(&MetaCommand::Delete {
                table: "kv".into(),
                key: json!("k1")
            })
            .unwrap());
        assert_eq!(s.table("kv").unwrap().get(&json!("k1")), None);
    }

    #[test]
    fn apply_delete_missing_is_noop() {
        let mut s = InMemoryMetaState::new();
        // 表不存在
        assert!(!s
            .apply(&MetaCommand::Delete {
                table: "x".into(),
                key: json!("k")
            })
            .unwrap());
        // 表存在但键不存在
        s.apply(&MetaCommand::Put {
            table: "kv".into(),
            key: json!("a"),
            value: json!(1),
        })
        .unwrap();
        assert!(!s
            .apply(&MetaCommand::Delete {
                table: "kv".into(),
                key: json!("missing")
            })
            .unwrap());
    }

    #[test]
    fn apply_json_pipeline() {
        let mut s = InMemoryMetaState::new();
        s.apply_json(&json!({"op":"put","table":"kv","key":"a","value":1}))
            .unwrap();
        s.apply_json(&json!({"op":"put","table":"kv","key":"b","value":2}))
            .unwrap();
        s.apply_json(&json!({"op":"delete","table":"kv","key":"a"}))
            .unwrap();
        let t = s.table("kv").unwrap();
        assert_eq!(t.get(&json!("a")), None);
        assert_eq!(t.get(&json!("b")), Some(&json!(2)));
        assert_eq!(s.applied_count, 3);
    }

    #[test]
    fn multiple_tables_isolated() {
        let mut s = InMemoryMetaState::new();
        s.apply(&MetaCommand::Put {
            table: "users".into(),
            key: json!("u1"),
            value: json!({"n":"a"}),
        })
        .unwrap();
        s.apply(&MetaCommand::Put {
            table: "shares".into(),
            key: json!("s1"),
            value: json!({"p":"/x"}),
        })
        .unwrap();
        assert_eq!(s.table_count(), 2);
        assert_eq!(s.table("users").unwrap().len(), 1);
        assert_eq!(s.table("shares").unwrap().len(), 1);
    }

    #[test]
    fn numeric_key_normalization() {
        let mut s = InMemoryMetaState::new();
        // 数字键 1 与 字符串键 "1" 不同（JSON 规范化区分类型）
        s.apply(&MetaCommand::Put {
            table: "t".into(),
            key: json!(1),
            value: json!("num"),
        })
        .unwrap();
        s.apply(&MetaCommand::Put {
            table: "t".into(),
            key: json!("1"),
            value: json!("str"),
        })
        .unwrap();
        assert_eq!(s.table("t").unwrap().get(&json!(1)), Some(&json!("num")));
        assert_eq!(s.table("t").unwrap().get(&json!("1")), Some(&json!("str")));
        assert_eq!(s.table("t").unwrap().len(), 2);
    }
}
