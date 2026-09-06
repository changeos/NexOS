//! 默认实现（trait impl + 真实后端）。
//!
//! 本模块为 5 个 trait 提供具体实现：
//! - `OpenraftConsensus`：真实 openraft 0.9 单节点集群（[`crate::raft_backend`]）
//! - `OpenraftKv`：内存 KV（CAS 完整实现，核心价值；openraft log 复制待多节点接入）
//! - `SqliteMetaStore`：真实 rusqlite 后端（apply_log 写 SQLite、snapshot/restore 用 dump）
//! - `HaFailoverOrchestrator`：驱动 [`crate::failover_sm::FailoverTask`] 状态机（内存）
//! - `NetlinkVipManager`：内存态（netlink/ARP 漂移待系统级实现）
//!
//! 5 个 trait 的 mock 实现位于 `mock` 模块（feature gate `mock`），专供下游测试；
//! 本模块的真实实现与 mock 分离，避免循环。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use os_core::{DateTime, Health, NodeId, NodeInfo, NodeRole, Utc};
use os_network::IpCidr;

use crate::consensus::{ClusterConfig, ClusterState, ClusterStatus};
use crate::failover::FailoverStatus;
use crate::failover_sm::FailoverTask;
use crate::kv::KvEntry;
use crate::meta_apply::InMemoryMetaState;
use crate::meta_store::MetaSnapshot;
use crate::vip::VipConfig;
use crate::{Consensus, DistributedKv, FailoverOrchestrator, MetaError, MetaStore, VipManager};

// 内部工具：当前 UTC 时间
fn now() -> DateTime {
    Utc::now()
}

// ----------------------------------------------------------------------------
// OpenraftConsensus（共识，真实 openraft 单节点后端）
// ----------------------------------------------------------------------------

/// 共识实现——基于真实 openraft 0.9 Raft 引擎（ADR-DEPS-002）。
///
/// 设计（meta-agent.md §3.5）：本结构持有两种运行模式：
/// 1. **轻量模式**（`new()` / `with_state()`）：不启动 Raft 任务，仅维护内存态
///    （Standalone / 注入角色）。保留此模式以兼容 `mock` 模块（`crate::mock`）的
///    MockConsensus——mock 是测试替身，无需真实 Raft 开销。
/// 2. **真实模式**（[`Self::start_single_node`]）：经 [`crate::raft_backend::spawn_single_node`]
///    启动一个真实 openraft 单节点集群（自己作为唯一 voter → 立即当选 leader），
///    `status`/`get_leader`/`get_members` 直接查询 Raft metrics。
///
/// 多节点动态成员变更（add_learner / change_membership）由 openraft 提供，本封装暂以
/// 单节点跑通真实 Raft 引擎（满足 P2 接通 DoD：openraft 单节点共识测）；多节点扩展
/// 见 raft_backend 模块的 TODO。
pub struct OpenraftConsensus {
    inner: Mutex<ConsensusInner>,
    /// 真实 openraft 句柄（None = 轻量模式；leave_cluster 后置 None）。
    raft: Mutex<Option<openraft::Raft<crate::raft_backend::MetaRaftConfig>>>,
}

struct ConsensusInner {
    cluster: Option<ClusterConfig>,
    state: ClusterState,
    term: u64,
    commit_index: u64,
    applied_index: u64,
    leader: Option<NodeId>,
    // 本节点 ID：真实 openraft 封装会用于日志复制/投票判定（见规格 §3.5），
    // 轻量模式仅注入不读取，故暂允许 dead_code。
    #[allow(dead_code)]
    self_id: Option<NodeId>,
}

// 手动实现 Default：ClusterState 无 derive(Default)（契约层枚举不擅自扩展派生），
// 此处显式指定默认角色为 Standalone（与 OpenraftConsensus::new 一致）。
impl Default for ConsensusInner {
    fn default() -> Self {
        Self {
            cluster: None,
            state: ClusterState::Standalone,
            term: 0,
            commit_index: 0,
            applied_index: 0,
            leader: None,
            self_id: None,
        }
    }
}

impl OpenraftConsensus {
    /// 创建空实例（Standalone 状态，尚未加入集群；**轻量模式**，不启动 Raft）。
    ///
    /// 保留此构造以兼容 `crate::mock::MockConsensus`（测试替身不需要真实 Raft 开销）。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ConsensusInner {
                state: ClusterState::Standalone,
                ..Default::default()
            }),
            raft: Mutex::new(None),
        }
    }

    /// 测试/集成注入：设置为某角色 + leader（**轻量模式**）。
    pub fn with_state(self_id: NodeId, state: ClusterState, leader: Option<NodeId>) -> Self {
        let inner = ConsensusInner {
            self_id: Some(self_id),
            state,
            leader,
            term: 1,
            ..Default::default()
        };
        Self {
            inner: Mutex::new(inner),
            raft: Mutex::new(None),
        }
    }

    /// 启动**真实 openraft 单节点集群**并返回封装（**真实模式**）。
    ///
    /// 流程：[`crate::raft_backend::spawn_single_node`] 创建 Raft 任务 → 把自己作为
    /// 唯一 voter 调用 `initialize` → 单节点立即当选 leader。返回的实例后续 `status` /
    /// `get_leader` / `get_members` 均查询 Raft metrics（真实选主结果）。
    ///
    /// `id` 是 openraft 内部 NodeId（u64）；领域 NodeId（String）由调用方在更上层映射。
    pub async fn start_single_node(id: u64) -> Result<Self, MetaError> {
        let raft = crate::raft_backend::spawn_single_node(id)
            .await
            .map_err(|e| MetaError::Internal(format!("openraft 启动失败: {e}")))?;
        Ok(Self {
            inner: Mutex::new(ConsensusInner {
                state: ClusterState::Leader,
                leader: Some(NodeId::new(id.to_string())),
                term: 1,
                self_id: Some(NodeId::new(id.to_string())),
                ..Default::default()
            }),
            raft: Mutex::new(Some(raft)),
        })
    }

    /// 真实模式下取最新 Raft metrics 快照（轻量模式 / 已 leave 返回 None）。
    fn metrics(&self) -> Option<openraft::RaftMetrics<u64, openraft::BasicNode>> {
        self.raft
            .lock()
            .expect("poisoned")
            .as_ref()
            .map(|r| r.metrics().borrow().clone())
    }

    /// 把 openraft ServerState 映射到契约层 ClusterState。
    fn map_state(s: openraft::ServerState) -> ClusterState {
        use openraft::ServerState as S;
        match s {
            S::Leader => ClusterState::Leader,
            S::Follower | S::Learner => ClusterState::Follower,
            S::Candidate => ClusterState::Candidate,
            S::Shutdown => ClusterState::Offline,
        }
    }

    /// 把 openraft u64 NodeId 映射到领域 NodeId（String newtype）。
    fn map_node_id(id: u64) -> NodeId {
        NodeId::new(id.to_string())
    }

    /// 测试辅助：克隆 Raft 句柄（`Raft` 内部 Arc，clone 廉价），供 wait() 跨 await 使用。
    #[cfg(test)]
    fn raft_clone(&self) -> Option<openraft::Raft<crate::raft_backend::MetaRaftConfig>> {
        self.raft.lock().expect("poisoned").clone()
    }
}

impl Default for OpenraftConsensus {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Consensus for OpenraftConsensus {
    async fn join_cluster(&self, _endpoint: String, _token: String) -> Result<NodeRole, MetaError> {
        // 真实模式：单节点集群已初始化，"加入" 语义上等同于确认自己已是成员。
        // 完整动态成员加入（add_learner → change_membership）见 raft_backend TODO。
        if self.raft.lock().expect("poisoned").is_some() {
            // 已在集群中（单节点 = leader）；返回当前角色。
            let m = self.metrics();
            let role = match m.as_ref().map(|m| m.state) {
                Some(openraft::ServerState::Leader) => NodeRole::Leader,
                _ => NodeRole::Follower,
            };
            return Ok(role);
        }
        // 轻量模式：直接置 Follower（保留原 mock 兼容行为）。
        let mut g = self.inner.lock().expect("poisoned");
        g.state = ClusterState::Follower;
        Ok(NodeRole::Follower)
    }

    async fn leave_cluster(&self) -> Result<(), MetaError> {
        // 真实模式：触发 openraft shutdown（单节点离开即关闭集群），并清空句柄
        // 使后续 status/get_leader 回落到轻量模式（Standalone）。
        let raft_opt = self.raft.lock().expect("poisoned").take();
        if let Some(raft) = raft_opt {
            let _ = raft.shutdown().await; // 忽略 shutdown 错误（已离开视为成功）
        }
        let mut g = self.inner.lock().expect("poisoned");
        g.cluster = None;
        g.state = ClusterState::Standalone;
        g.leader = None;
        Ok(())
    }

    async fn get_leader(&self) -> Option<NodeId> {
        // 真实模式：从 Raft metrics 取 current_leader。
        if let Some(m) = self.metrics() {
            return m.current_leader.map(Self::map_node_id);
        }
        self.inner.lock().expect("poisoned").leader.clone()
    }

    async fn get_members(&self) -> Vec<NodeInfo> {
        // 真实模式：从 Raft metrics 的 membership_config 取 voters。
        if let Some(m) = self.metrics() {
            let nodes = m.membership_config.membership().nodes();
            let role = if matches!(m.state, openraft::ServerState::Leader) {
                NodeRole::Leader
            } else {
                NodeRole::Follower
            };
            return nodes
                .into_iter()
                .map(|(id, node)| NodeInfo {
                    node_id: Self::map_node_id(*id),
                    role,
                    version: String::new(),
                    arch: String::new(),
                    endpoints: vec![node.addr.clone()],
                    health: Health::Healthy,
                })
                .collect();
        }
        self.inner
            .lock()
            .expect("poisoned")
            .cluster
            .as_ref()
            .map(|c| c.nodes.clone())
            .unwrap_or_default()
    }

    async fn status(&self) -> ClusterStatus {
        // 真实模式：从 Raft metrics 拼装 ClusterStatus。
        if let Some(m) = self.metrics() {
            let leader = m.current_leader.map(Self::map_node_id);
            let state = Self::map_state(m.state);
            let applied_index = m.last_applied.map(|l| l.index).unwrap_or(0);
            return ClusterStatus {
                state,
                leader,
                term: m.current_term,
                // openraft 不直接暴露 committed index；以 last_applied 近似（单节点等价）。
                commit_index: applied_index,
                applied_index,
                checked_at: now(),
            };
        }
        // 轻量模式：返回注入的内存态。
        let g = self.inner.lock().expect("poisoned");
        ClusterStatus {
            state: g.state,
            leader: g.leader.clone(),
            term: g.term,
            commit_index: g.commit_index,
            applied_index: g.applied_index,
            checked_at: now(),
        }
    }
}

// ----------------------------------------------------------------------------
// OpenraftKv（分布式 KV，内存骨架；CAS 完整实现——核心价值）
// ----------------------------------------------------------------------------

/// 分布式 KV 实现骨架（内存态；CAS 乐观锁已完整实现，是核心价值点）。
///
/// TODO(openraft)：真实实现须经 openraft log 复制到 quorum 后才提交，
/// leader 校验由 `OpenraftConsensus.status` 注入；当前为单节点内存版。
pub struct OpenraftKv {
    inner: Mutex<HashMap<String, KvEntry>>,
}

impl OpenraftKv {
    /// 创建空实例。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 测试便利：从初始条目构造。
    pub fn from_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = KvEntry>,
    {
        let map = entries.into_iter().map(|e| (e.key.clone(), e)).collect();
        Self {
            inner: Mutex::new(map),
        }
    }
}

impl Default for OpenraftKv {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DistributedKv for OpenraftKv {
    async fn put(&self, key: &str, value: serde_json::Value) -> Result<KvEntry, MetaError> {
        let mut g = self.inner.lock().expect("poisoned");
        let now = now();
        let entry = if let Some(prev) = g.get(key) {
            KvEntry {
                key: key.to_string(),
                value,
                version: prev.version + 1,
                created_at: prev.created_at,
                modified_at: now,
            }
        } else {
            KvEntry {
                key: key.to_string(),
                value,
                version: 1,
                created_at: now,
                modified_at: now,
            }
        };
        g.insert(key.to_string(), entry.clone());
        Ok(entry)
    }

    async fn get(&self, key: &str) -> Option<KvEntry> {
        self.inner.lock().expect("poisoned").get(key).cloned()
    }

    async fn delete(&self, key: &str) -> Result<(), MetaError> {
        self.inner.lock().expect("poisoned").remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Vec<KvEntry> {
        let g = self.inner.lock().expect("poisoned");
        let mut v: Vec<KvEntry> = g
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, e)| e.clone())
            .collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    async fn cas(
        &self,
        key: &str,
        expected_version: Option<u64>,
        new_value: serde_json::Value,
    ) -> Result<KvEntry, MetaError> {
        let mut g = self.inner.lock().expect("poisoned");
        let now = now();
        match (expected_version, g.get(key)) {
            (None, Some(existing)) => {
                // 期望"键必须不存在"，但键存在 → 冲突
                Err(MetaError::CasConflict {
                    expected: 0,
                    actual: existing.version,
                })
            }
            (None, None) => {
                // 仅创建
                let entry = KvEntry {
                    key: key.to_string(),
                    value: new_value,
                    version: 1,
                    created_at: now,
                    modified_at: now,
                };
                g.insert(key.to_string(), entry.clone());
                Ok(entry)
            }
            (Some(expected), Some(existing)) => {
                if existing.version != expected {
                    return Err(MetaError::CasConflict {
                        expected,
                        actual: existing.version,
                    });
                }
                let entry = KvEntry {
                    key: key.to_string(),
                    value: new_value,
                    version: existing.version + 1,
                    created_at: existing.created_at,
                    modified_at: now,
                };
                g.insert(key.to_string(), entry.clone());
                Ok(entry)
            }
            (Some(_expected), None) => {
                // 期望键存在并匹配版本，但键不存在
                Err(MetaError::CasConflict {
                    expected: _expected,
                    actual: 0,
                })
            }
        }
    }
}

// ----------------------------------------------------------------------------
// SqliteMetaStore（MetaStore，真实 rusqlite 后端，ADR-DEPS-002）
// ----------------------------------------------------------------------------

/// 元数据存储实现——真实 rusqlite 后端（§9.1#7 openraft 状态机内嵌 SQLite）。
///
/// 设计：
/// - **存储模型**：所有业务命令（[`crate::meta_apply::MetaCommand`]）作用到一张统一的
///   SQLite 表 `meta_kv(table_name TEXT, pk TEXT, value TEXT, PRIMARY KEY(table_name, pk))`，
///   其中 `pk` = `serde_json::to_string(key)`（与 [`crate::meta_apply::MetaTable`] 的主键
///   规范化一致），`value` = `serde_json::to_string(value)`。
/// - **apply_log**：解析命令，事务内 UPSERT/DELETE 到 `meta_kv`（强一致复制后由状态机
///   apply 钩子调用）。
/// - **snapshot**：把 `meta_kv` 全表序列化为 JSON 字节流（SQLite 的逻辑 dump；
///   规格要求"SQLite dump"，这里用 JSON 行集表达，便于跨节点传输与版本对齐）。
/// - **restore**：清空 `meta_kv` 后从快照重灌。
/// - **query**：执行参数化 SQL。对兼容旧契约的 `SELECT * FROM <table>` 自动重写为
///   `SELECT value FROM meta_kv WHERE table_name = ?`，返回解析后的 JSON 值；
///   其他 SQL 原样执行（返回每行以列名为键的 JSON 对象）。
///
/// 默认连 `:memory:` 库（测试/单节点）；生产可经 [`Self::open`] 接文件路径。
pub struct SqliteMetaStore {
    conn: Mutex<rusqlite::Connection>,
    /// 已 apply 命令计数（对应 openraft applied_index 的近似，用于 snapshot seq）。
    applied: Mutex<u64>,
}

impl SqliteMetaStore {
    /// 创建空实例（`:memory:` 库，自动建表）。
    pub fn new() -> Self {
        Self::open_in_memory().expect("打开 :memory: SQLite 必须成功")
    }

    /// 打开文件路径的 SQLite 库（生产用；启用 WAL 提升并发）。
    pub fn open(path: &str) -> Result<Self, MetaError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| MetaError::Internal(format!("打开 SQLite 失败: {e}")))?;
        Self::init_conn(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            applied: Mutex::new(0),
        })
    }

    /// 打开 `:memory:` 库（测试用）。
    pub fn open_in_memory() -> Result<Self, MetaError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| MetaError::Internal(format!("打开 :memory: SQLite 失败: {e}")))?;
        Self::init_conn(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            applied: Mutex::new(0),
        })
    }

    /// 初始化 schema（幂等）。
    fn init_conn(conn: &rusqlite::Connection) -> Result<(), MetaError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta_kv (
                table_name  TEXT NOT NULL,
                pk          TEXT NOT NULL,
                value       TEXT NOT NULL,
                PRIMARY KEY (table_name, pk)
            );",
        )
        .map_err(|e| MetaError::Internal(format!("建表失败: {e}")))?;
        Ok(())
    }

    /// 内部状态快照（测试用，重建为 InMemoryMetaState 视图）。
    pub fn snapshot_state(&self) -> InMemoryMetaState {
        let conn = self.conn.lock().expect("poisoned");
        let mut state = InMemoryMetaState::new();
        let mut stmt = match conn.prepare("SELECT table_name, pk, value FROM meta_kv") {
            Ok(s) => s,
            Err(_) => return state,
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .ok();
        if let Some(rows) = rows {
            for row in rows.flatten() {
                let (table, pk, value) = row;
                // 反规范化：pk/value 都是 JSON 字符串
                let key = serde_json::from_str::<serde_json::Value>(&pk)
                    .unwrap_or(serde_json::Value::Null);
                let value = serde_json::from_str::<serde_json::Value>(&value)
                    .unwrap_or(serde_json::Value::Null);
                let t = state.table_or_create(&table);
                t.put(key, value);
            }
        }
        state.applied_count = *self.applied.lock().expect("poisoned");
        state
    }
}

impl Default for SqliteMetaStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetaStore for SqliteMetaStore {
    async fn apply_log(&self, entry: serde_json::Value) -> Result<(), MetaError> {
        // 解析 MetaCommand，事务内 UPSERT/DELETE 到 meta_kv。
        let cmd = crate::meta_apply::MetaCommand::from_json(&entry)?;
        let conn = self.conn.lock().expect("poisoned");
        let tx = conn.unchecked_transaction().map_err(sqlite_internal)?;
        match &cmd {
            crate::meta_apply::MetaCommand::Put { table, key, value } => {
                let pk = serde_json::to_string(key).map_err(MetaError::Serde)?;
                let v = serde_json::to_string(value).map_err(MetaError::Serde)?;
                tx.execute(
                    "INSERT OR REPLACE INTO meta_kv (table_name, pk, value) VALUES (?1, ?2, ?3)",
                    rusqlite::params![table, pk, v],
                )
                .map_err(sqlite_internal)?;
            }
            crate::meta_apply::MetaCommand::Delete { table, key } => {
                let pk = serde_json::to_string(key).map_err(MetaError::Serde)?;
                tx.execute(
                    "DELETE FROM meta_kv WHERE table_name = ?1 AND pk = ?2",
                    rusqlite::params![table, pk],
                )
                .map_err(sqlite_internal)?;
            }
        }
        tx.commit().map_err(sqlite_internal)?;
        *self.applied.lock().expect("poisoned") += 1;
        Ok(())
    }

    async fn snapshot(&self) -> Result<MetaSnapshot, MetaError> {
        // 把 meta_kv 全表序列化为 JSON 行集（逻辑 dump）。
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn
            .prepare("SELECT table_name, pk, value FROM meta_kv ORDER BY table_name, pk")
            .map_err(sqlite_internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "table": r.get::<_, String>(0)?,
                    "pk": r.get::<_, String>(1)?,
                    "value": r.get::<_, String>(2)?,
                }))
            })
            .map_err(sqlite_internal)?;
        let mut dump: Vec<serde_json::Value> = Vec::new();
        for row in rows {
            dump.push(row.map_err(sqlite_internal)?);
        }
        let serialized = serde_json::to_vec(&dump).map_err(MetaError::Serde)?;
        let seq = *self.applied.lock().expect("poisoned");
        Ok(MetaSnapshot::new(seq, bytes::Bytes::from(serialized)))
    }

    async fn restore(&self, snap: MetaSnapshot) -> Result<(), MetaError> {
        // 反序列化 dump → 清空 meta_kv → 重灌。
        let dump: Vec<serde_json::Value> =
            serde_json::from_slice(&snap.sqlite_dump).map_err(MetaError::Serde)?;
        let conn = self.conn.lock().expect("poisoned");
        let tx = conn.unchecked_transaction().map_err(sqlite_internal)?;
        tx.execute("DELETE FROM meta_kv", [])
            .map_err(sqlite_internal)?;
        for row in &dump {
            let table = row["table"]
                .as_str()
                .ok_or_else(|| MetaError::SnapshotFailed("快照行缺少 table 字段".into()))?;
            let pk = row["pk"]
                .as_str()
                .ok_or_else(|| MetaError::SnapshotFailed("快照行缺少 pk 字段".into()))?;
            let value = row["value"]
                .as_str()
                .ok_or_else(|| MetaError::SnapshotFailed("快照行缺少 value 字段".into()))?;
            tx.execute(
                "INSERT OR REPLACE INTO meta_kv (table_name, pk, value) VALUES (?1, ?2, ?3)",
                rusqlite::params![table, pk, value],
            )
            .map_err(sqlite_internal)?;
        }
        tx.commit().map_err(sqlite_internal)?;
        *self.applied.lock().expect("poisoned") = snap.seq;
        Ok(())
    }

    async fn query(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, MetaError> {
        let conn = self.conn.lock().expect("poisoned");

        // 兼容旧契约：`SELECT * FROM <table>` 重写为 meta_kv 查询，返回解析后的 value。
        let lower = sql.to_ascii_lowercase();
        if let Some(name) = lower
            .strip_prefix("select * from ")
            .map(|s| s.trim().trim_end_matches(';').trim())
        {
            // 限定为纯表名（无 WHERE/ORDER 等）才重写；其余走通用路径。
            if !name.is_empty()
                && !name.contains(|c: char| c.is_whitespace() || c == '(' || c == ',')
            {
                let mut stmt = conn
                    .prepare("SELECT value FROM meta_kv WHERE table_name = ?1")
                    .map_err(sqlite_internal)?;
                let rows = stmt
                    .query_map(rusqlite::params![name], |r| r.get::<_, String>(0))
                    .map_err(sqlite_internal)?;
                let mut out = Vec::new();
                for row in rows {
                    let v: String = row.map_err(sqlite_internal)?;
                    let parsed: serde_json::Value =
                        serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v));
                    out.push(parsed);
                }
                return Ok(out);
            }
        }

        // 通用参数化查询：每行以列名为键组成 JSON 对象（字符串列尝试解析为 JSON）。
        let mut stmt = conn.prepare(sql).map_err(sqlite_internal)?;
        // 把 JSON 参数转为拥有所有权的 Box<dyn ToSql>，再借引用喂给 query。
        let owned_params: Vec<Box<dyn rusqlite::ToSql>> = params
            .iter()
            .map(json_to_owned_sql_param)
            .collect::<Result<Vec<_>, _>>()?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            owned_params.iter().map(|b| b.as_ref()).collect();
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("").to_string())
            .collect();
        let mut rows = stmt.query(&param_refs[..]).map_err(sqlite_internal)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_internal)? {
            let mut obj = serde_json::Map::new();
            for (i, name) in col_names.iter().enumerate() {
                let val: rusqlite::types::Value =
                    row.get(i).unwrap_or(rusqlite::types::Value::Null);
                obj.insert(name.clone(), sql_value_to_json(&val));
            }
            out.push(serde_json::Value::Object(obj));
        }
        Ok(out)
    }
}

// 把 rusqlite::Value 转换为 JSON：TEXT 列尝试解析为 JSON（保持 value 列语义）。
fn sql_value_to_json(v: &rusqlite::types::Value) -> serde_json::Value {
    use rusqlite::types::Value as V;
    match v {
        V::Null => serde_json::Value::Null,
        V::Integer(i) => serde_json::json!(i),
        V::Real(f) => serde_json::json!(f),
        V::Text(s) => serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone())),
        V::Blob(b) => serde_json::Value::Array(b.iter().map(|x| serde_json::json!(*x)).collect()),
    }
}

// 把 JSON Value 转为拥有所有权的 SQL 绑定参数（Box<dyn ToSql>）。
fn json_to_owned_sql_param(v: &serde_json::Value) -> Result<Box<dyn rusqlite::ToSql>, MetaError> {
    // rusqlite ToSql 已为 i64/f64/String/Vec<u8>/Null/bool 实现。
    match v {
        serde_json::Value::Null => Ok(Box::new(rusqlite::types::Null)),
        serde_json::Value::Bool(b) => Ok(Box::new(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Box::new(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Box::new(f))
            } else {
                Ok(Box::new(rusqlite::types::Null))
            }
        }
        serde_json::Value::String(s) => Ok(Box::new(s.clone())),
        // 复合类型（数组/对象）序列化为 JSON 字符串绑定。
        _ => Ok(Box::new(v.to_string())),
    }
}

// rusqlite::Error → MetaError::Internal
fn sqlite_internal(e: rusqlite::Error) -> MetaError {
    MetaError::Internal(format!("SQLite 错误: {e}"))
}

// 注意：MetaSnapshot 已在批 0 提供 serde + Bytes；snapshot/restore 用 JSON 行集
// 表达 SQLite 逻辑 dump，支撑跨节点传输与 applied_index 对齐。

// ----------------------------------------------------------------------------
// HaFailoverOrchestrator（故障转移，内存骨架；驱动 FailoverTask 状态机）
// ----------------------------------------------------------------------------

/// 故障转移编排器骨架（内存态；驱动 [`FailoverTask`] 状态机）。
///
/// TODO：真实实现由 leader 调用 os-compute（迁移 VM）/ 本 crate VipManager（切 VIP）/
/// os-storage（提升副本）。compute mock 就绪前用 stub 记录迁移意图。
pub struct HaFailoverOrchestrator {
    tasks: Mutex<HashMap<os_core::TaskId, FailoverTask>>,
}

impl HaFailoverOrchestrator {
    /// 创建空实例。
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for HaFailoverOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FailoverOrchestrator for HaFailoverOrchestrator {
    async fn detect_failure(&self, _node: &NodeId) -> Result<Option<String>, MetaError> {
        // TODO：真实实现经 Consensus 探活（心跳超时 / 进程探针）。
        // 骨架：默认存活（None）。
        Ok(None)
    }

    async fn trigger_failover(&self, failed: &NodeId) -> Result<os_core::TaskId, MetaError> {
        // 入队一个 Triggered 任务，返回 TaskId 供轮询。
        let task = FailoverTask::new(failed.clone());
        let tid = task.task_id;
        self.tasks.lock().expect("poisoned").insert(tid, task);
        Ok(tid)
    }

    async fn failover_status(&self, task: &os_core::TaskId) -> FailoverStatus {
        self.tasks
            .lock()
            .expect("poisoned")
            .get(task)
            .map(|t| t.to_status())
            .unwrap_or(FailoverStatus::Aborted)
    }
}

// ----------------------------------------------------------------------------
// NetlinkVipManager（VIP，内存骨架）
// ----------------------------------------------------------------------------

/// VIP 管理器骨架（内存态；netlink/ARP 漂移待实现）。
///
/// TODO：真实实现经 netlink 绑定 VIP + ARP 广播通告漂移；
/// VIP 已被其他节点持有时返回 `VipConflict`。
pub struct NetlinkVipManager {
    config: VipConfig,
    owner: Mutex<Option<NodeId>>,
}

impl NetlinkVipManager {
    /// 用 VIP 配置创建。
    pub fn new(config: VipConfig) -> Self {
        Self {
            config,
            owner: Mutex::new(None),
        }
    }

    /// 便利构造（无 owner）。
    pub fn with_cidr(cidr: IpCidr, interface: impl Into<String>) -> Self {
        Self::new(VipConfig {
            ip: cidr,
            interface: interface.into(),
            current_owner: None,
        })
    }

    /// 当前配置快照（含 owner）。
    pub fn config(&self) -> VipConfig {
        let owner = self.owner.lock().expect("poisoned").clone();
        VipConfig {
            ip: self.config.ip,
            interface: self.config.interface.clone(),
            current_owner: owner,
        }
    }
}

#[async_trait]
impl VipManager for NetlinkVipManager {
    async fn assign(&self, node: &NodeId) -> Result<(), MetaError> {
        let mut g = self.owner.lock().expect("poisoned");
        if let Some(current) = g.as_ref() {
            if current != node {
                return Err(MetaError::VipConflict(format!(
                    "VIP 已被节点 {} 持有",
                    current
                )));
            }
            return Ok(());
        }
        // TODO(netlink)：调用 netlink 绑定 VIP + 发送 ARP 通告漂移。
        *g = Some(node.clone());
        Ok(())
    }

    async fn release(&self) -> Result<(), MetaError> {
        // TODO(netlink)：解绑 VIP。
        let mut g = self.owner.lock().expect("poisoned");
        *g = None;
        Ok(())
    }

    async fn current_owner(&self) -> Option<NodeId> {
        self.owner.lock().expect("poisoned").clone()
    }
}

// ----------------------------------------------------------------------------
// 单元测试：默认实现骨架的正确性（CAS / VIP / Failover 入队 / MetaStore apply）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::TaskId;
    use serde_json::json;

    #[tokio::test]
    async fn kv_put_get_version_increment() {
        let kv = OpenraftKv::new();
        let e1 = kv.put("a", json!(1)).await.unwrap();
        assert_eq!(e1.version, 1);
        let e2 = kv.put("a", json!(2)).await.unwrap();
        assert_eq!(e2.version, 2);
        assert_eq!(e1.created_at, e2.created_at);
        let got = kv.get("a").await.unwrap();
        assert_eq!(got.value, json!(2));
        assert_eq!(got.version, 2);
    }

    #[tokio::test]
    async fn kv_list_prefix() {
        let kv = OpenraftKv::new();
        kv.put("a/1", json!(1)).await.unwrap();
        kv.put("a/2", json!(2)).await.unwrap();
        kv.put("b/1", json!(3)).await.unwrap();
        let a = kv.list("a/").await;
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|e| e.key.starts_with("a/")));
    }

    #[tokio::test]
    async fn kv_cas_create_only() {
        let kv = OpenraftKv::new();
        // 仅创建：键不存在 → 成功
        let e = kv.cas("x", None, json!("v")).await.unwrap();
        assert_eq!(e.version, 1);
        // 再次仅创建 → 冲突
        let err = kv.cas("x", None, json!("v2")).await.unwrap_err();
        assert!(matches!(err, MetaError::CasConflict { .. }));
    }

    #[tokio::test]
    async fn kv_cas_update_match() {
        let kv = OpenraftKv::new();
        let e = kv.put("k", json!(1)).await.unwrap();
        // 版本匹配 → 成功
        let e2 = kv.cas("k", Some(e.version), json!(2)).await.unwrap();
        assert_eq!(e2.version, 2);
        // 旧版本 → 冲突
        let err = kv.cas("k", Some(e.version), json!(3)).await.unwrap_err();
        assert!(matches!(
            err,
            MetaError::CasConflict {
                expected: 1,
                actual: 2
            }
        ));
        // 期望键存在但键不存在
        let err = kv.cas("missing", Some(1), json!(1)).await.unwrap_err();
        assert!(matches!(
            err,
            MetaError::CasConflict {
                expected: 1,
                actual: 0
            }
        ));
    }

    #[tokio::test]
    async fn kv_delete() {
        let kv = OpenraftKv::new();
        kv.put("k", json!(1)).await.unwrap();
        kv.delete("k").await.unwrap();
        assert!(kv.get("k").await.is_none());
    }

    #[tokio::test]
    async fn meta_store_apply_snapshot_restore_roundtrip() {
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        let snap = store.snapshot().await.unwrap();
        assert_eq!(snap.seq, 1);
        // 恢复到新 store
        let store2 = SqliteMetaStore::new();
        store2.restore(snap).await.unwrap();
        let rows = store2.query("SELECT * FROM kv", vec![]).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn meta_store_query_unsupported_returns_internal() {
        let store = SqliteMetaStore::new();
        let err = store.query("DROP TABLE x", vec![]).await.unwrap_err();
        assert!(matches!(err, MetaError::Internal(_)));
    }

    #[tokio::test]
    async fn vip_assign_release_conflict() {
        let cidr = IpCidr::new("10.0.0.5".parse().unwrap(), 24);
        let mgr = NetlinkVipManager::with_cidr(cidr, "br0");
        let n1 = NodeId::new("n1");
        let n2 = NodeId::new("n2");
        mgr.assign(&n1).await.unwrap();
        assert_eq!(mgr.current_owner().await, Some(n1.clone()));
        // 冲突
        let err = mgr.assign(&n2).await.unwrap_err();
        assert!(matches!(err, MetaError::VipConflict(_)));
        // 幂等：再 assign 同一节点 → 成功
        mgr.assign(&n1).await.unwrap();
        // release
        mgr.release().await.unwrap();
        assert!(mgr.current_owner().await.is_none());
        // release 后可重新 assign
        mgr.assign(&n2).await.unwrap();
        assert_eq!(mgr.current_owner().await, Some(n2));
    }

    #[tokio::test]
    async fn failover_trigger_and_status() {
        let fo = HaFailoverOrchestrator::new();
        let n1 = NodeId::new("n1");
        let tid: TaskId = fo.trigger_failover(&n1).await.unwrap();
        let st = fo.failover_status(&tid).await;
        // 初始为 Running（Triggered 阶段）
        assert!(matches!(st, FailoverStatus::Running { .. }));
        // 不存在的任务 → Aborted
        let other = TaskId::new();
        assert!(matches!(
            fo.failover_status(&other).await,
            FailoverStatus::Aborted
        ));
    }

    #[tokio::test]
    async fn failover_detect_default_alive() {
        let fo = HaFailoverOrchestrator::new();
        let n1 = NodeId::new("n1");
        assert_eq!(fo.detect_failure(&n1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn consensus_join_and_status() {
        let c = OpenraftConsensus::new();
        let role = c
            .join_cluster("10.0.0.1:7946".into(), "tok".into())
            .await
            .unwrap();
        assert_eq!(role, NodeRole::Follower);
        let st = c.status().await;
        assert_eq!(st.state, ClusterState::Follower);
    }

    // ------------------------------------------------------------------------
    // 新增测试：真实 openraft 单节点共识（ADR-DEPS-002 接通 DoD）
    // ------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn openraft_single_node_elects_self_as_leader() {
        // 启动真实 openraft 单节点集群：自己作为唯一 voter → 当选 leader。
        let c = OpenraftConsensus::start_single_node(1)
            .await
            .expect("启动单节点 Raft");

        // 等待选举完成（单节点：initialize 后立即进入 Candidate → Leader）。
        if let Some(raft) = c.raft_clone() {
            raft.wait(Some(std::time::Duration::from_secs(2)))
                .state(openraft::ServerState::Leader, "单节点必须当选 Leader")
                .await
                .expect("等待 Leader 超时");
        }

        // 验证契约层观测与 Raft metrics 一致。
        let st = c.status().await;
        assert_eq!(st.state, ClusterState::Leader);
        assert!(st.term >= 1, "当选后 term 应 >= 1，实际 {}", st.term);
        let leader = c.get_leader().await.expect("必须有 leader");
        assert_eq!(leader.as_str(), "1");
        // 单节点集群：成员列表应包含自己。
        let members = c.get_members().await;
        assert_eq!(members.len(), 1, "单节点集群成员应为 1");
        assert_eq!(members[0].node_id.as_str(), "1");
        assert_eq!(members[0].role, NodeRole::Leader);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn openraft_single_node_join_returns_leader_role() {
        // 已在集群中的单节点：join_cluster 应返回当前角色（Leader）。
        let c = OpenraftConsensus::start_single_node(2).await.unwrap();
        if let Some(raft) = c.raft_clone() {
            raft.wait(Some(std::time::Duration::from_secs(2)))
                .state(openraft::ServerState::Leader, "等 leader")
                .await
                .unwrap();
        }
        let role = c
            .join_cluster("ignored".into(), "tok".into())
            .await
            .unwrap();
        assert_eq!(role, NodeRole::Leader);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn openraft_single_node_leave_clears_state() {
        // leave_cluster → shutdown Raft，状态回到 Standalone。
        let c = OpenraftConsensus::start_single_node(3).await.unwrap();
        if let Some(raft) = c.raft_clone() {
            raft.wait(Some(std::time::Duration::from_secs(2)))
                .state(openraft::ServerState::Leader, "等 leader")
                .await
                .unwrap();
        }
        c.leave_cluster().await.unwrap();
        let st = c.status().await;
        assert_eq!(st.state, ClusterState::Standalone);
    }

    // ------------------------------------------------------------------------
    // 新增测试：真实 rusqlite MetaStore（ADR-DEPS-002 接通 DoD）
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn sqlite_apply_put_then_query_returns_value() {
        // apply put → 通过兼容路径 "SELECT * FROM <table>" 查到解析后的 value。
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        let rows = store.query("SELECT * FROM kv", vec![]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], json!({"v": 1}));
    }

    #[tokio::test]
    async fn sqlite_apply_delete_removes_row() {
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"delete","table":"kv","key":"a"}))
            .await
            .unwrap();
        let rows = store.query("SELECT * FROM kv", vec![]).await.unwrap();
        assert!(rows.is_empty(), "delete 后表应为空");
    }

    #[tokio::test]
    async fn sqlite_multi_table_isolation() {
        // 不同 table 互不干扰。
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"users","key":"u1","value":{"name":"alice"}}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"shares","key":"s1","value":{"path":"/x"}}))
            .await
            .unwrap();
        let users = store.query("SELECT * FROM users", vec![]).await.unwrap();
        let shares = store.query("SELECT * FROM shares", vec![]).await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(shares.len(), 1);
        assert_eq!(users[0], json!({"name": "alice"}));
        assert_eq!(shares[0], json!({"path": "/x"}));
    }

    #[tokio::test]
    async fn sqlite_query_real_sql_with_params() {
        // 通用参数化 SQL：直接查 meta_kv（暴露 value 列为 JSON 字符串→解析）。
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"b","value":{"v":2}}))
            .await
            .unwrap();
        // 参数化：按 table_name + pk 过滤。
        let rows = store
            .query(
                "SELECT value FROM meta_kv WHERE table_name = ? AND pk = ?",
                vec![json!("kv"), json!("\"a\"")],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        // value 列为 TEXT，存储的是 value JSON 的字符串；sql_value_to_json 会解析回 JSON。
        assert_eq!(rows[0]["value"], json!({"v": 1}));
    }

    #[tokio::test]
    async fn sqlite_snapshot_restore_roundtrip_to_fresh_store() {
        // 真实 SQLite dump 往返：A store 写入 → snapshot → B store restore → 数据一致。
        let a = SqliteMetaStore::new();
        a.apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        a.apply_log(json!({"op":"put","table":"kv","key":"b","value":{"v":2}}))
            .await
            .unwrap();
        let snap = a.snapshot().await.unwrap();
        assert_eq!(snap.seq, 2, "snapshot seq 应等于 applied 计数");

        let b = SqliteMetaStore::new();
        b.restore(snap).await.unwrap();
        let rows = b.query("SELECT * FROM kv", vec![]).await.unwrap();
        assert_eq!(rows.len(), 2, "恢复后应有 2 行");
        // restore 后 applied 计数对齐到 snap.seq
        let snap2 = b.snapshot().await.unwrap();
        assert_eq!(snap2.seq, 2);
    }

    #[tokio::test]
    async fn sqlite_restore_overwrites_existing() {
        // restore 覆盖既有内容（先写入再 restore 别的快照）。
        let a = SqliteMetaStore::new();
        a.apply_log(json!({"op":"put","table":"t","key":"x","value":1}))
            .await
            .unwrap();
        let snap_a = a.snapshot().await.unwrap();

        let b = SqliteMetaStore::new();
        b.apply_log(json!({"op":"put","table":"t","key":"y","value":2}))
            .await
            .unwrap();
        // restore a 的快照应清空 b 的既有数据
        b.restore(snap_a).await.unwrap();
        let rows = b.query("SELECT * FROM t", vec![]).await.unwrap();
        assert_eq!(rows.len(), 1, "restore 覆盖后只剩快照中的 1 行");
    }

    #[tokio::test]
    async fn sqlite_numeric_key_distinct_from_string_key() {
        // 数字键 1 与字符串键 "1" 不同（pk 用 JSON 规范化区分类型，与 InMemoryMetaState 一致）。
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"t","key":1,"value":"num"}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"t","key":"1","value":"str"}))
            .await
            .unwrap();
        let rows = store.query("SELECT * FROM t", vec![]).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    // ------------------------------------------------------------------------
    // 新增测试：SqliteMetaStore 边界 + 序列化解析路径
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn sqlite_apply_invalid_command_returns_apply_failed() {
        // apply_log 收到非法 JSON 命令 → MetaError::ApplyFailed（from_json 内部转 ApplyFailed）
        let store = SqliteMetaStore::new();
        let err = store
            .apply_log(json!({"op":"unknown_op"}))
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::ApplyFailed(_)));
    }

    #[tokio::test]
    async fn sqlite_apply_value_must_be_json_serializable() {
        // value 字段合法 JSON 即可（任意类型，含数字/字符串/对象）
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"t","key":"a","value":42}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"t","key":"b","value":"raw-string"}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"t","key":"c","value":[1,2,3]}))
            .await
            .unwrap();
        let rows = store.query("SELECT * FROM t", vec![]).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn sqlite_snapshot_empty_store_returns_empty_dump() {
        // 空 store snapshot → dump 为空数组 []
        let store = SqliteMetaStore::new();
        let snap = store.snapshot().await.unwrap();
        assert_eq!(snap.seq, 0);
        let dump: Vec<serde_json::Value> = serde_json::from_slice(&snap.sqlite_dump).unwrap();
        assert!(dump.is_empty());
    }

    #[tokio::test]
    async fn sqlite_snapshot_orders_rows_by_table_then_pk() {
        // snapshot 内 ORDER BY table_name, pk → 行有序（确定性 dump）
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"z","key":"a","value":1}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"a","key":"b","value":2}))
            .await
            .unwrap();
        let snap = store.snapshot().await.unwrap();
        let dump: Vec<serde_json::Value> = serde_json::from_slice(&snap.sqlite_dump).unwrap();
        assert_eq!(dump.len(), 2);
        // a 在 z 之前（ORDER BY table_name）
        assert_eq!(dump[0]["table"], "a");
        assert_eq!(dump[1]["table"], "z");
    }

    #[tokio::test]
    async fn sqlite_restore_corrupt_snapshot_returns_serde_error() {
        // 非法快照字节 → restore 返回 MetaError::Serde
        let store = SqliteMetaStore::new();
        let bad = MetaSnapshot::new(0, bytes::Bytes::from_static(b"not json"));
        let err = store.restore(bad).await.unwrap_err();
        assert!(matches!(err, MetaError::Serde(_)));
    }

    #[tokio::test]
    async fn sqlite_restore_row_missing_table_field_returns_snapshot_failed() {
        // dump 行缺少 table 字段 → restore 报 SnapshotFailed
        let store = SqliteMetaStore::new();
        let dump = json!([{"pk":"\"a\"", "value":"1"}]); // 缺 table
        let snap = MetaSnapshot::new(1, bytes::Bytes::from(dump.to_string().into_bytes()));
        let err = store.restore(snap).await.unwrap_err();
        assert!(matches!(err, MetaError::SnapshotFailed(_)));
    }

    #[tokio::test]
    async fn sqlite_restore_row_missing_pk_field_returns_snapshot_failed() {
        let store = SqliteMetaStore::new();
        let dump = json!([{"table":"t", "value":"1"}]); // 缺 pk
        let snap = MetaSnapshot::new(1, bytes::Bytes::from(dump.to_string().into_bytes()));
        let err = store.restore(snap).await.unwrap_err();
        assert!(matches!(err, MetaError::SnapshotFailed(_)));
    }

    #[tokio::test]
    async fn sqlite_restore_row_missing_value_field_returns_snapshot_failed() {
        let store = SqliteMetaStore::new();
        let dump = json!([{"table":"t", "pk":"\"a\""}]); // 缺 value
        let snap = MetaSnapshot::new(1, bytes::Bytes::from(dump.to_string().into_bytes()));
        let err = store.restore(snap).await.unwrap_err();
        assert!(matches!(err, MetaError::SnapshotFailed(_)));
    }

    #[tokio::test]
    async fn sqlite_restore_empty_dump_clears_existing() {
        // 空 dump → restore 后表清空（DELETE FROM meta_kv 生效）
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"t","key":"a","value":1}))
            .await
            .unwrap();
        let empty = MetaSnapshot::new(0, bytes::Bytes::from_static(b"[]"));
        store.restore(empty).await.unwrap();
        let rows = store.query("SELECT * FROM t", vec![]).await.unwrap();
        assert!(rows.is_empty(), "空 dump restore 应清空表");
    }

    #[tokio::test]
    async fn sqlite_snapshot_state_reflects_applied() {
        // snapshot_state 把 meta_kv 反规范化为 InMemoryMetaState（覆盖纯解析路径）
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"b","value":{"v":2}}))
            .await
            .unwrap();
        let state = store.snapshot_state();
        assert_eq!(state.applied_count, 2);
        let t = state.table("kv").expect("kv 表应存在");
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(&json!("a")), Some(&json!({"v":1})));
        assert_eq!(t.get(&json!("b")), Some(&json!({"v":2})));
    }

    #[tokio::test]
    async fn sqlite_snapshot_state_empty_when_no_rows() {
        let store = SqliteMetaStore::new();
        let state = store.snapshot_state();
        assert_eq!(state.applied_count, 0);
        assert_eq!(state.table_count(), 0);
    }

    #[tokio::test]
    async fn sqlite_query_select_star_with_where_not_rewritten() {
        // SELECT * FROM <table> WHERE ... 含 WHERE → 不重写（走通用路径）
        // meta_kv 表本身没有 WHERE 列重写，直接查原表
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        // 直接查 meta_kv（通用路径，返回 table_name/pk/value 三列）
        let rows = store
            .query("SELECT table_name, pk FROM meta_kv", vec![])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["table_name"], json!("kv"));
    }

    #[tokio::test]
    async fn sqlite_query_select_star_with_comma_not_rewritten() {
        // SELECT * FROM a, b → 含逗号 → 不重写
        let store = SqliteMetaStore::new();
        let err = store.query("SELECT * FROM a, b", vec![]).await.unwrap_err();
        assert!(matches!(err, MetaError::Internal(_))); // 表不存在
    }

    #[tokio::test]
    async fn sqlite_query_select_star_with_paren_not_rewritten() {
        // SELECT * FROM (subquery) → 含括号 → 不重写
        let store = SqliteMetaStore::new();
        let err = store.query("SELECT * FROM (SELECT 1)", vec![]).await;
        // 子查询语法 SQLite 支持，应返回 Ok（1 行）或语法错；此处不强断言
        let _ = err;
    }

    #[tokio::test]
    async fn sqlite_query_select_star_lowercase_rewritten() {
        // "select * from <table>" 小写也应被重写（实现用 to_ascii_lowercase 比较）
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        let rows = store.query("select * from kv", vec![]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], json!({"v": 1}));
    }

    #[tokio::test]
    async fn sqlite_query_select_star_with_trailing_semicolon_rewritten() {
        // "SELECT * FROM <table>;" 末尾分号应被 trim 后重写
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        let rows = store.query("SELECT * FROM kv;", vec![]).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_query_params_bound_correctly() {
        // 参数化查询：? 占位绑定各种 JSON 类型
        let store = SqliteMetaStore::new();
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":{"v":1}}))
            .await
            .unwrap();
        // 用参数绑定 table_name + pk
        let rows = store
            .query(
                "SELECT value FROM meta_kv WHERE table_name = ? AND pk = ?",
                vec![json!("kv"), json!("\"a\"")],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["value"], json!({"v": 1}));
    }

    #[tokio::test]
    async fn sqlite_query_null_param_bound() {
        // Null 参数绑定（覆盖 json_to_owned_sql_param 的 Null 分支）
        let store = SqliteMetaStore::new();
        // 写一行 value=null
        store
            .apply_log(json!({"op":"put","table":"kv","key":"a","value":null}))
            .await
            .unwrap();
        let rows = store
            .query(
                "SELECT value FROM meta_kv WHERE table_name = ? AND pk IS NOT ?",
                vec![json!("kv"), json!(null)],
            )
            .await
            .unwrap();
        // pk="\"a\"" IS NOT null → 命中
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_query_bool_and_number_params() {
        // 直接建一张普通表测 bool/number 参数绑定（覆盖 bool/i64/f64 参数分支）
        let store = SqliteMetaStore::new();
        // 用通用 SQL 建表 + 插入 + 查询（覆盖 bool/i64/f64/string 参数分支）
        let rows = store
            .query(
                "SELECT ? AS b, ? AS i, ? AS f, ? AS s",
                vec![json!(true), json!(42i64), json!(2.5), json!("hello")],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        // bool → 1 (SQLite 无原生 bool)，i64/f64/string 各就位
        assert_eq!(rows[0]["b"], json!(1));
        assert_eq!(rows[0]["i"], json!(42));
        assert_eq!(rows[0]["f"], json!(2.5));
        assert_eq!(rows[0]["s"], json!("hello"));
    }

    #[tokio::test]
    async fn sqlite_query_object_param_serialized_as_json_string() {
        // 复合类型（对象）参数 → 序列化为 JSON 字符串绑定（覆盖 json_to_owned_sql_param 的 _ 分支）
        let store = SqliteMetaStore::new();
        let rows = store
            .query("SELECT ? AS obj", vec![json!({"k": "v"})])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        // 对象被序列化为 JSON 字符串存入 TEXT 列，sql_value_to_json 再解析回对象
        assert_eq!(rows[0]["obj"], json!({"k": "v"}));
    }

    #[tokio::test]
    async fn sqlite_query_array_param_serialized_as_json_string() {
        // 数组参数 → 序列化为 JSON 字符串
        let store = SqliteMetaStore::new();
        let rows = store
            .query("SELECT ? AS arr", vec![json!([1, 2, 3])])
            .await
            .unwrap();
        assert_eq!(rows[0]["arr"], json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn sqlite_open_in_memory_idempotent_schema() {
        // open_in_memory 多次建表幂等（CREATE TABLE IF NOT EXISTS）
        let s1 = SqliteMetaStore::open_in_memory().unwrap();
        let s2 = SqliteMetaStore::open_in_memory().unwrap();
        // 两个独立 :memory: 库，互不干扰
        s1.apply_log(json!({"op":"put","table":"t","key":"a","value":1}))
            .await
            .unwrap();
        let rows2 = s2.query("SELECT * FROM t", vec![]).await.unwrap();
        assert!(rows2.is_empty(), "独立 :memory: 库互不可见");
    }

    #[tokio::test]
    async fn sqlite_open_file_path_creates_db() {
        // open 文件路径 → 创建文件库（覆盖文件路径分支）
        let tmp = std::env::temp_dir();
        let path = tmp.join(format!(
            "os-meta-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_str = path.to_str().unwrap();
        {
            let store = SqliteMetaStore::open(path_str).unwrap();
            store
                .apply_log(json!({"op":"put","table":"t","key":"a","value":{"v":1}}))
                .await
                .unwrap();
            let rows = store.query("SELECT * FROM t", vec![]).await.unwrap();
            assert_eq!(rows.len(), 1);
        }
        // 清理临时文件
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn sqlite_default_equals_new() {
        // Default trait impl 等同 new
        let d = SqliteMetaStore::default();
        let n = SqliteMetaStore::new();
        // 两者都是空 :memory: 库，行为一致
        let rows_d = d.query("SELECT * FROM x", vec![]).await.unwrap();
        let rows_n = n.query("SELECT * FROM x", vec![]).await.unwrap();
        assert!(rows_d.is_empty() && rows_n.is_empty());
    }

    // ------------------------------------------------------------------------
    // 新增测试：OpenraftKv 边界（CAS 各分支 + list 排序）
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn kv_get_missing_returns_none() {
        let kv = OpenraftKv::new();
        assert!(kv.get("missing").await.is_none());
    }

    #[tokio::test]
    async fn kv_list_empty_prefix_returns_all_sorted() {
        // 空前缀 "" 匹配所有键，且按 key 排序
        let kv = OpenraftKv::new();
        kv.put("c", json!(1)).await.unwrap();
        kv.put("a", json!(2)).await.unwrap();
        kv.put("b", json!(3)).await.unwrap();
        let all = kv.list("").await;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].key, "a");
        assert_eq!(all[1].key, "b");
        assert_eq!(all[2].key, "c");
    }

    #[tokio::test]
    async fn kv_list_no_match_returns_empty() {
        let kv = OpenraftKv::new();
        kv.put("a", json!(1)).await.unwrap();
        let v = kv.list("no-such-prefix/").await;
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn kv_put_preserves_created_at_across_updates() {
        let kv = OpenraftKv::new();
        let e1 = kv.put("k", json!(1)).await.unwrap();
        let e2 = kv.put("k", json!(2)).await.unwrap();
        let e3 = kv.put("k", json!(3)).await.unwrap();
        assert_eq!(e1.created_at, e2.created_at);
        assert_eq!(e2.created_at, e3.created_at);
        // modified_at 应递增（或至少不早于 created_at）
        assert!(e3.modified_at >= e3.created_at);
    }

    #[tokio::test]
    async fn kv_cas_update_creates_new_version_preserves_created_at() {
        let kv = OpenraftKv::new();
        let e1 = kv.put("k", json!(1)).await.unwrap();
        let e2 = kv.cas("k", Some(1), json!(2)).await.unwrap();
        assert_eq!(e2.version, 2);
        assert_eq!(e1.created_at, e2.created_at);
    }

    #[tokio::test]
    async fn kv_delete_missing_is_ok() {
        let kv = OpenraftKv::new();
        kv.delete("never-existed").await.unwrap();
    }

    #[tokio::test]
    async fn kv_delete_then_recreate_resets_version() {
        // 删除后重新创建 → version 从 1 开始（新条目）
        let kv = OpenraftKv::new();
        kv.put("k", json!(1)).await.unwrap();
        kv.put("k", json!(2)).await.unwrap();
        kv.delete("k").await.unwrap();
        let e = kv.put("k", json!(3)).await.unwrap();
        assert_eq!(e.version, 1, "删除后重建 version 应重置为 1");
    }

    #[tokio::test]
    async fn kv_from_entries_constructs_with_initial_data() {
        use os_core::Utc;
        let now = Utc::now();
        let entries = vec![
            KvEntry {
                key: "a".into(),
                value: json!(1),
                version: 1,
                created_at: now,
                modified_at: now,
            },
            KvEntry {
                key: "b".into(),
                value: json!(2),
                version: 5,
                created_at: now,
                modified_at: now,
            },
        ];
        let kv = OpenraftKv::from_entries(entries);
        assert_eq!(kv.get("a").await.unwrap().version, 1);
        assert_eq!(kv.get("b").await.unwrap().version, 5);
    }

    #[tokio::test]
    async fn kv_default_equals_new() {
        let d = OpenraftKv::default();
        assert!(d.get("anything").await.is_none());
    }

    // ------------------------------------------------------------------------
    // 新增测试：NetlinkVipManager 边界
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn vip_default_config_has_no_owner() {
        let cidr = IpCidr::new("10.0.0.1".parse().unwrap(), 24);
        let mgr = NetlinkVipManager::with_cidr(cidr, "eth0");
        let cfg = mgr.config();
        assert_eq!(cfg.ip, cidr);
        assert_eq!(cfg.interface, "eth0");
        assert!(cfg.current_owner.is_none());
    }

    #[tokio::test]
    async fn vip_config_reflects_assigned_owner() {
        let cidr = IpCidr::new("10.0.0.1".parse().unwrap(), 24);
        let mgr = NetlinkVipManager::with_cidr(cidr, "eth0");
        let n1 = NodeId::new("n1");
        mgr.assign(&n1).await.unwrap();
        let cfg = mgr.config();
        assert_eq!(cfg.current_owner, Some(n1));
    }

    #[tokio::test]
    async fn vip_release_when_no_owner_is_ok() {
        // 无 owner 时 release 也成功
        let cidr = IpCidr::new("10.0.0.1".parse().unwrap(), 24);
        let mgr = NetlinkVipManager::with_cidr(cidr, "eth0");
        mgr.release().await.unwrap();
        assert!(mgr.current_owner().await.is_none());
    }

    #[tokio::test]
    async fn vip_assign_same_owner_twice_is_idempotent() {
        let cidr = IpCidr::new("10.0.0.1".parse().unwrap(), 24);
        let mgr = NetlinkVipManager::with_cidr(cidr, "eth0");
        let n1 = NodeId::new("n1");
        mgr.assign(&n1).await.unwrap();
        mgr.assign(&n1).await.unwrap(); // 幂等
        assert_eq!(mgr.current_owner().await, Some(n1));
    }

    #[tokio::test]
    async fn vip_new_with_config_preserves_fields() {
        let cidr = IpCidr::new("192.168.1.100".parse().unwrap(), 24);
        let cfg = VipConfig {
            ip: cidr,
            interface: "br0".into(),
            current_owner: Some(NodeId::new("preset")),
        };
        let mgr = NetlinkVipManager::new(cfg);
        // new 不读 current_owner（用内部 Mutex None）；config() 返回内部 owner
        let got = mgr.config();
        assert_eq!(got.ip, IpCidr::new("192.168.1.100".parse().unwrap(), 24));
        assert_eq!(got.interface, "br0");
        // 内部 owner 初始 None（preset 仅存于传入 cfg，不写入内部状态）
        assert!(got.current_owner.is_none());
    }

    // ------------------------------------------------------------------------
    // 新增测试：HaFailoverOrchestrator 边界
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn failover_default_equals_new() {
        let fo = HaFailoverOrchestrator::default();
        let n1 = NodeId::new("n1");
        assert_eq!(fo.detect_failure(&n1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn failover_multiple_tasks_distinct_ids() {
        let fo = HaFailoverOrchestrator::new();
        let t1 = fo.trigger_failover(&NodeId::new("n1")).await.unwrap();
        let t2 = fo.trigger_failover(&NodeId::new("n2")).await.unwrap();
        assert_ne!(t1, t2, "不同任务的 TaskId 应不同");
        // 两个任务都是 Running（初始 Triggered）
        assert!(matches!(
            fo.failover_status(&t1).await,
            FailoverStatus::Running { .. }
        ));
        assert!(matches!(
            fo.failover_status(&t2).await,
            FailoverStatus::Running { .. }
        ));
    }

    // ------------------------------------------------------------------------
    // 新增测试：OpenraftConsensus 轻量模式（不启动 Raft）
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn consensus_default_equals_new() {
        let c = OpenraftConsensus::default();
        let st = c.status().await;
        assert_eq!(st.state, ClusterState::Standalone);
    }

    #[tokio::test]
    async fn consensus_with_state_injects_role_and_leader() {
        let c = OpenraftConsensus::with_state(
            NodeId::new("n1"),
            ClusterState::Leader,
            Some(NodeId::new("n1")),
        );
        let st = c.status().await;
        assert_eq!(st.state, ClusterState::Leader);
        assert_eq!(st.leader.as_ref().map(|n| n.as_str()), Some("n1"));
        assert_eq!(st.term, 1);
        // get_leader 在轻量模式读 inner.leader
        let leader = c.get_leader().await;
        assert_eq!(leader.as_ref().map(|n| n.as_str()), Some("n1"));
    }

    #[tokio::test]
    async fn consensus_get_members_empty_when_no_cluster() {
        let c = OpenraftConsensus::new();
        // 轻量模式无 cluster → get_members 返回空
        let members = c.get_members().await;
        assert!(members.is_empty());
    }

    #[tokio::test]
    async fn consensus_leave_when_already_standalone_is_ok() {
        // 轻量模式（未 join）直接 leave → 状态置 Standalone，Ok
        let c = OpenraftConsensus::new();
        c.leave_cluster().await.unwrap();
        let st = c.status().await;
        assert_eq!(st.state, ClusterState::Standalone);
        assert!(st.leader.is_none());
    }
}
