//! 观测端点地址簿——设计 §2「地址交换所」+ §3「endpoints」。
//!
//! 任何节点连接我时，我看到的是它的 **公网观测地址**（TCP socket 对端
//! ip:port——NAT 后节点经网关映射后的样子）。把它记入本地地址簿并随
//! FINDNODE/NODES **一起八卦**：只要全网任一节点接触过某 NAT 节点，它的
//! 公网端点就全网可知——一个公网 NexOS 足以充当全网地址交换所。
//!
//! ```text
//!  N0(NAT) ──出站──▶ P1(公网/交换所)          N0 在 P1 的观测端点
//!                        │ e = 203.0.113.9:51234（NAT 映射口）
//!                        ▼
//!              P1.endpoints.observe(N0, e)
//!                        │ NODES 应答捎带 endpoints 八卦
//!           ┌────────────┴────────────┐
//!           ▼                         ▼
//!      N1 学到 {N0→e}            N0 学到自己的观测端点
//!      （可对 N0 发起打洞）      （打洞时向对端通告）
//! ```
//!
//! - 每消息八卦上限 [`ENDPOINTS_GOSSIP_LIMIT`] = 32 条（按新鲜度采样，防膨胀）。
//! - 条目 TTL 陈旧清理（[`EndpointBook::evict_expired`]——死映射不滞留）。
//! - 节点也会从八卦里学到**自己的**观测端点（交换所回灌）——TCP 打洞的
//!   PUNCH1/PUNCH2 就携带这份"网络看我是什么 ip:port"。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::identity::NodeId;

/// 单条 NODES 应答携带的观测端点八卦上限（防消息膨胀）。
pub const ENDPOINTS_GOSSIP_LIMIT: usize = 32;

/// 地址簿容量上限（观测 + 学习的总量防护；超出按最旧淘汰）。
const ENDPOINT_BOOK_CAP: usize = 4096;

// ============================================================================
// 八卦载荷
// ============================================================================

/// NODES 应答捎带的端点八卦单元。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointGossip {
    /// 被观测节点。
    pub id: NodeId,
    /// 观测到的 ip:port（NAT 映射口或公网监听口）。
    pub addr: SocketAddr,
}

impl EndpointGossip {
    /// 构造。
    pub fn new(id: NodeId, addr: SocketAddr) -> Self {
        Self { id, addr }
    }
}

/// 观察面条目（`Handle::known_endpoints`）。
#[derive(Debug, Clone)]
pub struct EndpointEntry {
    /// 被观测节点。
    pub id: NodeId,
    /// 观测端点。
    pub addr: SocketAddr,
    /// 最近一次确认时刻（观测或八卦续期）。
    pub last_seen: Instant,
}

// ============================================================================
// 地址簿
// ============================================================================

/// 观测端点地址簿：`{NodeID → 观测 ip:port}`（纯内存结构，无 I/O）。
///
/// 写入路径有二：本机观测（连接建立时的 socket 对端地址——最高可信）与
/// 八卦学习（覆盖旧值需更新鲜度，防陈旧回灌）。读取路径：打洞取端点、
/// NODES 应答采样八卦。
#[derive(Default)]
pub struct EndpointBook {
    entries: HashMap<NodeId, EndpointEntry>,
}

impl EndpointBook {
    /// 空簿。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 本机观测记录（连接成功即调用——NAT 映射口由此入簿）。
    pub fn observe(&mut self, id: NodeId, addr: SocketAddr, now: Instant) {
        self.insert(EndpointEntry {
            id,
            addr,
            last_seen: now,
        });
    }

    /// 批量学习八卦（NODES 应答 / punch 载荷携带的端点）。
    pub fn learn(&mut self, gossip: &[EndpointGossip], now: Instant) {
        for g in gossip {
            self.insert(EndpointEntry {
                id: g.id.clone(),
                addr: g.addr,
                last_seen: now,
            });
        }
    }

    fn insert(&mut self, entry: EndpointEntry) {
        // 容量防护：超出按最旧淘汰（looser 语义——陈旧映射让位）
        if !self.entries.contains_key(&entry.id) && self.entries.len() >= ENDPOINT_BOOK_CAP {
            if let Some(oldest) = self
                .entries
                .values()
                .min_by_key(|e| e.last_seen)
                .map(|e| e.id.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(entry.id.clone(), entry);
    }

    /// 查询某节点的观测端点（打洞 / 诊断）。
    #[must_use]
    pub fn lookup(&self, id: &NodeId) -> Option<SocketAddr> {
        self.entries.get(id).map(|e| e.addr)
    }

    /// 全部条目快照（按新鲜度降序——观察面）。
    #[must_use]
    pub fn entries(&self) -> Vec<EndpointEntry> {
        let mut list: Vec<EndpointEntry> = self.entries.values().cloned().collect();
        list.sort_by_key(|e| std::cmp::Reverse(e.last_seen));
        list
    }

    /// 八卦采样：最新的 ≤limit 条（**排除应答节点自身**——不回灌它已知的自己，
    /// 但保留请求者——请求者正需要学到自己的观测端点）。
    #[must_use]
    pub fn gossip_sample(&self, exclude: Option<&NodeId>, limit: usize) -> Vec<EndpointGossip> {
        let mut list: Vec<&EndpointEntry> = self.entries.values().collect();
        list.sort_by_key(|e| std::cmp::Reverse(e.last_seen));
        list.into_iter()
            .filter(|e| Some(&e.id) != exclude)
            .take(limit)
            .map(|e| EndpointGossip::new(e.id.clone(), e.addr))
            .collect()
    }

    /// TTL 陈旧清理：`ttl` 内未续期的条目移除，返回被清理的节点
    /// （调用方可在日志/观察面对账）。
    pub fn evict_expired(&mut self, now: Instant, ttl: Duration) -> Vec<NodeId> {
        let expired: Vec<NodeId> = self
            .entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.last_seen) > ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.entries.remove(id);
        }
        expired
    }

    /// 条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否空簿。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// 单元测——观测/学习/查询 / 八卦采样上限与排序 / TTL 清理
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;

    fn nid(seed: u8) -> NodeId {
        NodeIdentity::from_seed(&[seed; 32]).node_id()
    }

    fn addr(port: u16) -> SocketAddr {
        format!("203.0.113.{port}:41000").parse().unwrap()
    }

    // 1. 观测与学习：observe/learn 写入，lookup 命中；重复观测刷新地址与时刻；
    //    entries 按新鲜度降序
    #[test]
    fn observe_learn_and_lookup() {
        let mut book = EndpointBook::new();
        let a = nid(1);
        let b = nid(2);
        let t0 = Instant::now();
        book.observe(a.clone(), addr(1), t0);
        assert_eq!(book.lookup(&a), Some(addr(1)));
        assert_eq!(book.len(), 1);
        // 学习八卦（另一节点转述）
        book.learn(&[EndpointGossip::new(b.clone(), addr(2))], t0);
        assert_eq!(book.lookup(&b), Some(addr(2)));
        // 重新观测刷新地址（NAT 重映射）与新鲜度
        let t1 = t0 + Duration::from_secs(10);
        book.observe(a.clone(), addr(3), t1);
        assert_eq!(book.lookup(&a), Some(addr(3)), "新观测覆盖旧映射");
        // 快照按新鲜度降序：a(最新) 在前
        let list = book.entries();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, a);
        assert!(list[0].last_seen > list[1].last_seen);
        assert!(book.lookup(&nid(9)).is_none());
    }

    // 2. 八卦采样：上限 32 / 排除应答者自身 / 保留请求者 / 新鲜度优先
    #[test]
    fn gossip_sample_cap_exclusion_and_freshness() {
        let mut book = EndpointBook::new();
        let t0 = Instant::now();
        let responder = nid(100); // 应答节点（采样要排除它自己）
        let requester = nid(101); // 请求者（必须保留——它要学自己的观测端点）
        book.observe(responder.clone(), addr(1), t0);
        // 依次观测 40 个（i 越大越新鲜）；请求者最后观测（比全部循环条目新鲜，
        // 避免与最旧条目并列导致采样排序不稳定）
        for i in 0..40u16 {
            book.observe(
                nid(i as u8 + 3),
                addr(i + 10),
                t0 + Duration::from_millis(u64::from(i)),
            );
        }
        book.observe(requester.clone(), addr(2), t0 + Duration::from_millis(200));
        assert_eq!(book.len(), 42);
        let sample = book.gossip_sample(Some(&responder), ENDPOINTS_GOSSIP_LIMIT);
        assert_eq!(sample.len(), ENDPOINTS_GOSSIP_LIMIT, "上限 32 条");
        assert!(
            sample.iter().all(|g| g.id != responder),
            "应答者自身不入采样"
        );
        assert!(
            sample.iter().any(|g| g.id == requester),
            "请求者保留在采样中（学自己的观测端点）"
        );
        // 新鲜度优先：最新的观测（i=39）必在，最老的（i=0..=7 之外的先入者）被挤出
        assert!(sample.iter().any(|g| g.addr == addr(49)));
        assert!(!sample.iter().any(|g| g.addr == addr(10)));
        // 上限参数生效
        assert_eq!(book.gossip_sample(None, 5).len(), 5);
        assert!(EndpointBook::new().gossip_sample(None, 32).is_empty());
    }

    // 3. TTL 清理：超时未续期的条目移除（返回清理清单）；续期条目保留
    #[test]
    fn ttl_evicts_stale_entries() {
        let mut book = EndpointBook::new();
        let a = nid(20);
        let b = nid(21);
        let t0 = Instant::now();
        book.observe(a.clone(), addr(1), t0);
        book.observe(b.clone(), addr(2), t0);
        // TTL 内：不清
        assert!(book
            .evict_expired(t0 + Duration::from_secs(10), Duration::from_secs(60))
            .is_empty());
        assert_eq!(book.len(), 2);
        // a 续期（八卦再学到）→ 只清 b
        book.learn(
            &[EndpointGossip::new(a.clone(), addr(1))],
            t0 + Duration::from_secs(50),
        );
        let evicted = book.evict_expired(t0 + Duration::from_secs(100), Duration::from_secs(60));
        assert_eq!(evicted, vec![b.clone()]);
        assert!(book.lookup(&a).is_some(), "续期条目保留");
        assert!(book.lookup(&b).is_none(), "陈旧条目清理");
        assert_eq!(book.len(), 1);
        assert!(!book.is_empty());
    }
}
