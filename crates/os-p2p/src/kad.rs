//! Kademlia 核心——设计 §3「kad」：邻域 k-buckets + FINDNODE 迭代查询原语。
//!
//! # 桶语义（Swarm 同款 proximity order bins）
//!
//! 每节点维护 [`BUCKET_COUNT`](crate::identity::BUCKET_COUNT) = 160 个桶（OverlayAddr
//! 160-bit → 邻域阶 0..=159）：与自身 Overlay 的共同前导比特数为 p 的节点进第
//! `p` 桶（[`OverlayAddr::bucket_for`]）——PO 越大越近，159 = 最近邻域。每桶容量
//! [`K`] = 16；桶满时按 Kademlia 惯例**倾向保留旧节点**（LRU 末端只有陈旧
//! （`stale_after` 无活动）才被替换，否则丢弃候选——老节点存活更久是经验规律）。
//!
//! # 迭代查询（引擎侧 api::lookup 驱动，本模块提供数据面原语）
//!
//! ```text
//!  FINDNODE(target) ──▶ 对端返回 NODES = 离 target 最近的 ≤K 个已知节点
//!  向每轮 ALPHA=3 个更近的未查询节点继续 FINDNODE
//!  一轮无更近节点且无新条目 ⇒ 收敛（无更近即收敛）
//! ```
//!
//! 桶刷新（对随机 target 定期 walk）与节点失效剔除（连续 ping 超时 →
//! [`KBuckets::record_failure`] 达 [`Timing::max_failures`](crate::Timing) 次即除名）
//! 在 api.rs 维护循环中驱动。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::identity::{NodeId, OverlayAddr};

/// 每桶容量 k（Swarm 同款默认）。
pub const K: usize = 16;
/// 每轮并行查询数 α（迭代 FINDNODE 的扇出）。
pub const ALPHA: usize = 3;
/// 单次迭代查询的轮数上限（防病态网络下的活锁）。
pub const MAX_LOOKUP_ROUNDS: usize = 10;

/// 节点描述（NODES 帧载荷单元；也用于路由表条目的可序列化形态）。
///
/// - `underlay`：可拨 TCP 地址——公网节点通告监听地址；NAT 节点为 `None`
///   （不可直拨，只能经中继）。**这就是 P1 的 NAT 模拟机制**：非公网节点
///   不通告 underlay，全网对它只有中继路径。
/// - `relay`：该节点的中继服务者（`Some(r)` = "经 r 可达"）——仅由实际承担
///   中继的节点在 NODES 应答中填写，其余节点原样转述学习。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// 节点身份（secp256k1 压缩公钥）。
    pub id: NodeId,
    /// 可拨 underlay 地址（None = NAT/不可直拨）。
    #[serde(default)]
    pub underlay: Option<SocketAddr>,
    /// 是否公网服务节点（bootstrap/relay 职责）。
    #[serde(default)]
    pub public: bool,
    /// 经哪 个节点中继可达（可达性记录 {dst → 经 relay_id}）。
    #[serde(default)]
    pub relay: Option<NodeId>,
}

impl NodeInfo {
    /// Overlay 地址（由公钥派生——地址不可伪造，身份即坐标）。
    #[must_use]
    pub fn overlay(&self) -> OverlayAddr {
        self.id.overlay()
    }

    /// 是否可直拨（有 underlay 地址）。
    #[must_use]
    pub fn dialable(&self) -> bool {
        self.underlay.is_some()
    }
}

/// upsert 结果（测试断言与可观测日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// 新节点入桶。
    New,
    /// 已在表中——刷新信息与 last_seen。
    Refreshed,
    /// 桶满且 LRS（最久未见）条目已陈旧——被新节点替换。
    ReplacedStale,
    /// 桶满且现有条目都活跃——候选被丢弃（Kademlia 保守保留旧节点）。
    DroppedBucketFull,
    /// 自身/无效条目——忽略。
    IgnoredSelf,
}

/// 路由表条目（桶内；桶按 last_seen 升序——头部 = LRS 最久未见）。
#[derive(Debug, Clone)]
pub struct BucketEntry {
    /// 节点描述。
    pub info: NodeInfo,
    /// 缓存的 Overlay 地址（upsert 时算一次，排序热路径不重算 keccak）。
    pub overlay: OverlayAddr,
    /// 最近一次确认存活（pong/新连接/新消息）。
    pub last_seen: Instant,
    /// 连续失败计数（ping 超时/拨号失败）。
    pub failures: u32,
}

/// 桶摘要行（`Handle::buckets_summary` / README 拓扑展示）。
#[derive(Debug, Clone, Serialize)]
pub struct BucketStat {
    /// 邻域阶（共同前导比特数；越大越近）。
    pub po: u8,
    /// 桶内节点数。
    pub count: usize,
    /// 短 NodeID 列表（`0x1234…cdef`）。
    pub entries: Vec<String>,
}

/// 160 邻域 k-buckets 路由表（纯内存结构，无 I/O——引擎持锁短临界区使用）。
pub struct KBuckets {
    self_id: NodeId,
    self_overlay: OverlayAddr,
    stale_after: Duration,
    /// `buckets[p]` = 与自身 PO 恰为 p 的节点（桶内按 last_seen 升序）。
    buckets: Vec<Vec<BucketEntry>>,
}

impl KBuckets {
    /// 构造（`stale_after`：桶满替换策略中 LRS 的陈旧判定线）。
    pub fn new(self_id: NodeId, stale_after: Duration) -> Self {
        let buckets = vec![Vec::new(); crate::identity::BUCKET_COUNT];
        Self {
            self_overlay: self_id.overlay(),
            self_id,
            stale_after,
            buckets,
        }
    }

    /// 自身 NodeID。
    #[must_use]
    pub fn self_id(&self) -> &NodeId {
        &self.self_id
    }

    /// 插入/刷新节点（Hello/NODES/重连都会走到）。
    pub fn upsert(&mut self, info: NodeInfo) -> InsertOutcome {
        self.upsert_at(info, Instant::now())
    }

    /// 带自定义时钟的 upsert（测试注入陈旧条目）。
    fn upsert_at(&mut self, info: NodeInfo, now: Instant) -> InsertOutcome {
        if info.id == self.self_id {
            return InsertOutcome::IgnoredSelf;
        }
        let overlay = info.overlay();
        let po = self.self_overlay.bucket_for(&overlay);
        let bucket = &mut self.buckets[po];
        if let Some(pos) = bucket.iter().position(|e| e.info.id == info.id) {
            // 已在表中：尾部 = 最新鲜，刷新信息并清零失败计数
            let entry = &mut bucket[pos];
            entry.info = info;
            entry.last_seen = now;
            entry.failures = 0;
            let e = bucket.remove(pos);
            bucket.push(e);
            return InsertOutcome::Refreshed;
        }
        if bucket.len() < K {
            bucket.push(BucketEntry {
                info,
                overlay,
                last_seen: now,
                failures: 0,
            });
            return InsertOutcome::New;
        }
        // 桶满：Kademlia 保守策略——只有 LRS（头部）已陈旧才替换
        if now.duration_since(bucket[0].last_seen) > self.stale_after {
            let evicted = bucket.remove(0);
            tracing::debug!(
                self = %crate::short_hex(&self.self_id.to_hex()),
                evicted = %crate::short_hex(&evicted.info.id.to_hex()),
                po,
                "桶满替换陈旧 LRS"
            );
            bucket.push(BucketEntry {
                info,
                overlay,
                last_seen: now,
                failures: 0,
            });
            InsertOutcome::ReplacedStale
        } else {
            tracing::debug!(
                self = %crate::short_hex(&self.self_id.to_hex()),
                candidate = %crate::short_hex(&info.id.to_hex()),
                po,
                "桶满丢弃候选（现有条目活跃）"
            );
            InsertOutcome::DroppedBucketFull
        }
    }

    /// 确认存活（pong/收到帧）：刷新 last_seen、清零失败计数。
    pub fn touch(&mut self, id: &NodeId) {
        let now = Instant::now();
        for bucket in &mut self.buckets {
            if let Some(e) = bucket.iter_mut().find(|e| &e.info.id == id) {
                e.last_seen = now;
                e.failures = 0;
            }
        }
    }

    /// 记一次失败（ping 超时/拨号失败）。连续失败达 `max_failures` → 除名并
    /// 返回被除名节点（调用方级联清理可达性记录）。
    pub fn record_failure(&mut self, id: &NodeId, max_failures: u32) -> Option<NodeInfo> {
        for bucket in &mut self.buckets {
            if let Some(pos) = bucket.iter().position(|e| &e.info.id == id) {
                let e = &mut bucket[pos];
                e.failures += 1;
                if e.failures >= max_failures {
                    let removed = bucket.remove(pos);
                    tracing::info!(
                        self = %crate::short_hex(&self.self_id.to_hex()),
                        evicted = %crate::short_hex(&removed.info.id.to_hex()),
                        failures = removed.failures,
                        "连续失败达上限，节点除名"
                    );
                    return Some(removed.info);
                }
                return None;
            }
        }
        None
    }

    /// 显式移除（连接关闭且不可重拨时）。
    pub fn remove(&mut self, id: &NodeId) -> Option<NodeInfo> {
        for bucket in &mut self.buckets {
            if let Some(pos) = bucket.iter().position(|e| &e.info.id == id) {
                return Some(bucket.remove(pos).info);
            }
        }
        None
    }

    /// 查询条目。
    #[must_use]
    pub fn get(&self, id: &NodeId) -> Option<&BucketEntry> {
        self.buckets.iter().flatten().find(|e| &e.info.id == id)
    }

    /// 全部条目快照。
    #[must_use]
    pub fn entries(&self) -> Vec<NodeInfo> {
        self.buckets
            .iter()
            .flatten()
            .map(|e| e.info.clone())
            .collect()
    }

    /// 已知节点 ID 集合。
    #[must_use]
    pub fn known_ids(&self) -> HashSet<NodeId> {
        self.buckets
            .iter()
            .flatten()
            .map(|e| e.info.id.clone())
            .collect()
    }

    /// 表大小。
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    /// 是否空表。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 离 target 最近的 ≤k 个已知节点（全表按 XOR 距离排序）。
    #[must_use]
    pub fn closest(&self, target: &OverlayAddr, k: usize) -> Vec<NodeInfo> {
        let mut all: Vec<(OverlayAddr, NodeInfo)> = self
            .buckets
            .iter()
            .flatten()
            .map(|e| (e.overlay, e.info.clone()))
            .collect();
        all.sort_by_key(|(ov, _)| ov.xor(target));
        all.into_iter().take(k).map(|(_, info)| info).collect()
    }

    /// 离 target 最近且**可直拨**的 ≤k 个节点（迭代 FINDNODE 的候选——NAT 节点
    /// 无法承接查询，只能作为查询目标的对端结果存在）。
    #[must_use]
    pub fn closest_dialable(&self, target: &OverlayAddr, k: usize) -> Vec<NodeInfo> {
        self.closest(target, k * 4)
            .into_iter()
            .filter(|i| i.dialable())
            .take(k)
            .collect()
    }

    /// 非空桶摘要（邻域阶 / 数量 / 短 ID）。
    #[must_use]
    pub fn summary(&self) -> Vec<BucketStat> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.is_empty())
            .map(|(po, b)| BucketStat {
                po: po as u8,
                count: b.len(),
                entries: b
                    .iter()
                    .map(|e| crate::short_hex(&e.info.id.to_hex()))
                    .collect(),
            })
            .collect()
    }

    /// 测试钩子：把条目 last_seen 回拨 `back`（构造陈旧 LRS 场景）。
    #[doc(hidden)]
    pub fn backdate_for_test(&mut self, id: &NodeId, back: Duration) {
        let now = Instant::now();
        if let Some(then) = now.checked_sub(back) {
            for bucket in &mut self.buckets {
                if let Some(e) = bucket.iter_mut().find(|e| &e.info.id == id) {
                    e.last_seen = then;
                }
            }
        }
    }

    /// 测试钩子：条目所在的桶下标（对账 PO 语义）。
    #[doc(hidden)]
    pub fn bucket_index_for_test(&self, id: &NodeId) -> Option<usize> {
        self.buckets
            .iter()
            .position(|b| b.iter().any(|e| &e.info.id == id))
    }
}

// ============================================================================
// 单元测——桶选择 / closest 排序 / 桶满策略 / 失效剔除
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;

    fn ids(n: usize, tag: u16) -> Vec<NodeId> {
        (0..n)
            .map(|i| {
                let mut seed = [0u8; 32];
                seed[0] = (tag & 0xFF) as u8;
                seed[1] = (tag >> 8) as u8;
                seed[2] = i as u8;
                seed[3] = 1; // 恒非零私钥（tag=i=0 时也合法）
                NodeIdentity::from_seed(&seed).node_id()
            })
            .collect()
    }

    fn info(id: &NodeId) -> NodeInfo {
        NodeInfo {
            id: id.clone(),
            underlay: Some("127.0.0.1:7000".parse().unwrap()),
            public: false,
            relay: None,
        }
    }

    // 1. 插入与桶位置自洽：每个条目落在 PO(self, entry) 对应桶；自身被忽略
    #[test]
    fn insert_places_entries_by_proximity_order() {
        let self_id = ids(1, 0).into_iter().next().unwrap();
        let mut table = KBuckets::new(self_id.clone(), Duration::from_secs(60));
        assert!(table.is_empty());
        for peer in ids(30, 1) {
            assert_eq!(table.upsert(info(&peer)), InsertOutcome::New);
        }
        assert_eq!(table.len(), 30);
        let self_overlay = self_id.overlay();
        // 逐条目对账：所在桶下标 == PO(自身, 条目)
        for entry_id in table.known_ids() {
            let po = self_overlay.proximity_order(&entry_id.overlay());
            assert_eq!(
                table.bucket_index_for_test(&entry_id),
                Some(usize::from(po)),
                "PO={po} 的条目必须在同号桶"
            );
        }
        // 桶容量不超 k
        for stat in table.summary() {
            assert!(stat.count <= K, "桶 {} 超 k=16", stat.po);
        }
        // 自身 upsert 被忽略
        assert_eq!(table.upsert(info(&self_id)), InsertOutcome::IgnoredSelf);
        assert_eq!(table.len(), 30);
        // 重复 upsert → Refreshed
        let again = ids(30, 1).into_iter().next().unwrap();
        assert_eq!(table.upsert(info(&again)), InsertOutcome::Refreshed);
        assert_eq!(table.len(), 30);
    }

    // 2. closest：按 XOR 距离返回最近 k 个；closest_dialable 过滤无 underlay
    #[test]
    fn closest_returns_nearest_by_xor_distance() {
        let self_id = ids(1, 100).into_iter().next().unwrap();
        let mut table = KBuckets::new(self_id, Duration::from_secs(60));
        let peers = ids(25, 101);
        for (i, p) in peers.iter().enumerate() {
            let mut inf = info(p);
            if i % 2 == 0 {
                inf.underlay = None; // 一半 NAT（不可直拨）
            }
            table.upsert(inf);
        }
        let target = crate::identity::OverlayAddr::random();
        let top = table.closest(&target, 5);
        assert_eq!(top.len(), 5);
        // 与全量排序一致（前 5）
        let mut all = peers.clone();
        all.sort_by_key(|p| p.overlay().xor(&target));
        assert_eq!(
            top.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            all[..5].to_vec(),
            "closest 必须是 XOR 最近的 k 个"
        );
        // dialable 过滤：NAT 条目被排除
        let dialable = table.closest_dialable(&target, 5);
        assert!(dialable.iter().all(|i| i.dialable()));
        assert!(!dialable.is_empty());
    }

    // 3. 桶满策略：k=16 塞满同 PO 桶后新候选被丢；LRS 陈旧后被替换
    #[test]
    fn bucket_full_policy_keeps_fresh_drops_candidate_replaces_stale() {
        let self_id = ids(1, 200).into_iter().next().unwrap();
        let stale_after = Duration::from_secs(1);
        let mut table = KBuckets::new(self_id.clone(), stale_after);
        // 生成 17 个与自身 PO=0（首比特不同）的节点（随机种子一半概率 PO=0）
        let mut po0: Vec<NodeId> = Vec::new();
        let mut i = 0u8;
        while po0.len() < 17 && i < 255 {
            let mut seed = [0u8; 32];
            seed[0] = 201 + (i % 40);
            seed[1] = i;
            seed[31] = 250;
            let cand = NodeIdentity::from_seed(&seed).node_id();
            if self_id.overlay().proximity_order(&cand.overlay()) == 0 {
                po0.push(cand);
            }
            i += 1;
        }
        assert_eq!(po0.len(), 17, "255 个种子里必有 17 个 PO=0（期望 ~127）");
        for p in &po0[..16] {
            assert_eq!(table.upsert(info(p)), InsertOutcome::New);
        }
        // 第 17 个：现有条目都新鲜 → 丢弃
        assert_eq!(
            table.upsert(info(&po0[16])),
            InsertOutcome::DroppedBucketFull
        );
        assert_eq!(table.len(), 16);
        assert!(table.get(&po0[16]).is_none());
        // LRS 回拨 2s（> stale_after）→ 替换
        table.backdate_for_test(&po0[0], Duration::from_secs(2));
        assert_eq!(table.upsert(info(&po0[16])), InsertOutcome::ReplacedStale);
        assert_eq!(table.len(), 16);
        assert!(table.get(&po0[0]).is_none(), "陈旧 LRS 被替换");
        assert!(table.get(&po0[16]).is_some());
    }

    // 4. 失效剔除：连续 ping 失败达 max_failures → 除名；touch 清零计数
    #[test]
    fn record_failure_evicts_after_consecutive_failures() {
        let self_id = ids(1, 300).into_iter().next().unwrap();
        let mut table = KBuckets::new(self_id, Duration::from_secs(60));
        let peers = ids(3, 301);
        for p in &peers {
            table.upsert(info(p));
        }
        // 2 次失败内不除名
        assert!(table.record_failure(&peers[0], 3).is_none());
        assert!(table.record_failure(&peers[0], 3).is_none());
        assert!(table.get(&peers[0]).is_some());
        // touch 清零
        table.touch(&peers[0]);
        assert!(table.record_failure(&peers[0], 3).is_none());
        assert!(table.record_failure(&peers[0], 3).is_none());
        // 第 3 次（未 touch）→ 除名
        let evicted = table.record_failure(&peers[0], 3);
        assert_eq!(evicted.map(|i| i.id), Some(peers[0].clone()));
        assert!(table.get(&peers[0]).is_none());
        assert_eq!(table.len(), 2);
        // 不存在的节点 → None（幂等）
        assert!(table.record_failure(&peers[0], 3).is_none());
    }

    // 5. NodeInfo 序列化：NODES 载荷 JSON 往返 + 默认字段宽容（旧版本帧）
    #[test]
    fn node_info_serde_roundtrip_and_defaults() {
        let id = NodeIdentity::from_seed(&[7; 32]).node_id();
        let n = NodeInfo {
            id: id.clone(),
            underlay: Some("198.51.100.9:7070".parse().unwrap()),
            public: true,
            relay: Some(id.clone()),
        };
        let json = serde_json::to_string(&n).unwrap();
        let back: NodeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.underlay.unwrap().port(), 7070);
        assert!(back.public && back.relay == Some(id.clone()));
        // 缺省字段（兼容旧帧）：underlay/public/relay 可缺
        let min: NodeInfo =
            serde_json::from_str(&format!("{{\"id\":\"{}\"}}", id.to_hex())).unwrap();
        assert!(min.underlay.is_none() && !min.public && min.relay.is_none());
        assert!(!min.dialable());
    }
}
