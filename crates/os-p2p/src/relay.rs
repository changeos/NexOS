//! NAT 中继——设计 §3「relay」：可达性记录 + SEND 路由决策 + store-and-forward。
//!
//! # 可达性记录 `{dst → 经 relay_id}`
//!
//! NAT 节点（无 underlay 可拨地址）连上公网节点后发 `relay_announce` 注册
//! "经你中继可达我"；中继方记录在 [`RelayState::relayed`]，并在 NODES 应答中
//! 为这些节点填写 `relay = 自己`——全网经 DHT walk 学到 `{dst → relay}` 路由
//! （[`RelayState::routes`]）。路由不是中心注册表：每节点只存自己学到的一份，
//! 由 Kademlia 发现语义自然扩散。
//!
//! # SEND 路由（[`next_hop`] 纯函数）
//!
//! ```text
//!  send(dst) ─┬─ dst == 自己            → 本地交付
//!             ├─ 与 dst 直连            → 直发（LAN/公网最优路径）
//!             ├─ dst 经我中继（已注册） → 转发；不在线则入信箱
//!             ├─ 知道 {dst → relay}     → 发往 relay（ttl/hops 递减防环）
//!             └─ 无任何路由             → 触发 lookup(dst) 后重试
//! ```
//!
//! # store-and-forward（离线信箱）
//!
//! 中继方为已注册但当前不在线的被中继节点缓存帧：[`MAILBOX_LIMIT_PER_NODE`]
//! = 100 条/节点，满则丢最旧（保最新）；被中继节点重连（重新握手成功）即冲信箱
//! ——离线消息送达。中继注册有 TTL（[`RelayState::evict_expired`]）：长期不
//! 重连的 NAT 节点连同信箱一起过期清理，防止陈旧幽灵路由。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::identity::NodeId;
use crate::transport::Frame;

/// 每个被中继节点的离线信箱上限（超出丢最旧）。
pub const MAILBOX_LIMIT_PER_NODE: usize = 100;

/// SEND 帧的一跳路由决策（纯函数——全部输入快照化，可穷举测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextHop {
    /// dst 是自己：本地交付上层。
    Deliver,
    /// 与 dst 有已认证连接：直发。
    Direct,
    /// dst 经我中继且已注册：转发；若其当前不在线则入信箱。
    RelayQueue,
    /// 知道 {dst → relay}：发往该 relay（下一跳承担转发或信箱）。
    Forward(NodeId),
    /// 无路由：触发 FINDNODE lookup(dst) 后重试。
    Unknown,
}

/// SEND 路由决策（纯函数）。
///
/// 入参均为节点状态快照：`conns` = 已认证直连集合，`relayed` = 经我中继的
/// 注册集合，`routes` = 我学到的 `{dst → relay}` 可达性记录。
#[must_use]
pub fn next_hop(
    self_id: &NodeId,
    dst: &NodeId,
    conns: &HashSet<NodeId>,
    relayed: &HashSet<NodeId>,
    routes: &HashMap<NodeId, NodeId>,
) -> NextHop {
    if dst == self_id {
        NextHop::Deliver
    } else if conns.contains(dst) {
        NextHop::Direct
    } else if relayed.contains(dst) {
        NextHop::RelayQueue
    } else if let Some(relay) = routes.get(dst) {
        if conns.contains(relay) {
            NextHop::Forward(relay.clone())
        } else {
            NextHop::Unknown // 中继本身失联——交由 lookup 重新发现
        }
    } else {
        NextHop::Unknown
    }
}

/// 中继注册条目（我是它的 relay）。
#[derive(Debug, Clone)]
pub struct RelayRegistration {
    /// 注册时刻。
    pub since: Instant,
    /// 最近一次存活（连接/announce/任意帧）——TTL 判定用。
    pub last_alive: Instant,
}

/// 中继状态：双向账本 + 离线信箱。
///
/// - `relayed`：**我作为中继**服务的节点（对方 announce 注册）。
/// - `routes`：**我作为发送方**学到的可达性记录 `{dst → 经 relay_id}`。
/// - `mailbox`：store-and-forward 队列（仅对我中继的离线节点有意义）。
#[derive(Default)]
pub struct RelayState {
    relayed: HashMap<NodeId, RelayRegistration>,
    routes: HashMap<NodeId, NodeId>,
    mailbox: HashMap<NodeId, VecDeque<Frame>>,
}

impl RelayState {
    /// 空状态。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册/续期"经我中继可达 dst"（announce 与重连都走这里）。
    pub fn register_relayed(&mut self, dst: NodeId, now: Instant) {
        self.relayed
            .entry(dst)
            .and_modify(|r| r.last_alive = now)
            .or_insert(RelayRegistration {
                since: now,
                last_alive: now,
            });
    }

    /// 存活续期（收到被中继节点的任意帧）。
    pub fn mark_alive(&mut self, dst: &NodeId, now: Instant) {
        if let Some(r) = self.relayed.get_mut(dst) {
            r.last_alive = now;
        }
    }

    /// dst 是否经我中继。
    #[must_use]
    pub fn is_relayed(&self, dst: &NodeId) -> bool {
        self.relayed.contains_key(dst)
    }

    /// 经我中继的节点集合。
    #[must_use]
    pub fn relayed_ids(&self) -> HashSet<NodeId> {
        self.relayed.keys().cloned().collect()
    }

    /// 注销（显式下线清理；信箱一并丢弃——注册已不在，路由语义失效）。
    pub fn unregister_relayed(&mut self, dst: &NodeId) {
        self.relayed.remove(dst);
        self.mailbox.remove(dst);
    }

    /// 学习可达性记录 `{dst → 经 relay_id}`（NODES 应答来源；禁止自环/指向自身）。
    /// 返回是否接受（非法关系拒绝时 false）。
    pub fn set_route(&mut self, dst: NodeId, relay: NodeId, self_id: &NodeId) -> bool {
        if &dst == self_id || &relay == self_id || dst == relay {
            return false;
        }
        self.routes.insert(dst, relay);
        true
    }

    /// 查询路由。
    #[must_use]
    pub fn route_for(&self, dst: &NodeId) -> Option<NodeId> {
        self.routes.get(dst).cloned()
    }

    /// 路由表只读引用（引擎快照决策用）。
    #[must_use]
    pub fn routes_ref(&self) -> &HashMap<NodeId, NodeId> {
        &self.routes
    }

    /// 经某 relay 的全部路由目标（该 relay 除名时的级联清理清单）。
    #[must_use]
    pub fn routes_via(&self, relay: &NodeId) -> Vec<NodeId> {
        self.routes
            .iter()
            .filter(|(_, r)| *r == relay)
            .map(|(dst, _)| dst.clone())
            .collect()
    }

    /// 级联删除路由。
    pub fn remove_routes_via(&mut self, relay: &NodeId) {
        self.routes.retain(|_, r| r != relay);
    }

    /// 入信箱（store-and-forward）：满 100 条丢最旧（保最新）。
    /// 返回是否入箱成功（注册不存在 → false，不应发生）。
    pub fn enqueue_offline(&mut self, dst: &NodeId, frame: Frame) -> bool {
        if !self.relayed.contains_key(dst) {
            return false;
        }
        let q = self.mailbox.entry(dst.clone()).or_default();
        if q.len() >= MAILBOX_LIMIT_PER_NODE {
            let dropped = q.pop_front();
            tracing::warn!(
                dst = %crate::short_hex(&dst.to_hex()),
                dropped_hops = dropped.map(|f| f.hops).unwrap_or(0),
                "离线信箱满 {} 条，丢最旧",
                MAILBOX_LIMIT_PER_NODE
            );
        }
        q.push_back(frame);
        true
    }

    /// 取走全部信箱消息（重连冲箱）。
    #[must_use]
    pub fn take_mailbox(&mut self, dst: &NodeId) -> Vec<Frame> {
        self.mailbox
            .remove(dst)
            .map(|q| q.into_iter().collect())
            .unwrap_or_default()
    }

    /// 信箱深度（可观测）。
    #[must_use]
    pub fn mailbox_len(&self, dst: &NodeId) -> usize {
        self.mailbox.get(dst).map_or(0, VecDeque::len)
    }

    /// 过期清理：`relay_ttl` 内无存活的被中继节点 → 连信箱一起移除，
    /// 返回被清理的节点（调用方同步从路由表除名——幽灵路由清除）。
    pub fn evict_expired(&mut self, now: Instant, relay_ttl: Duration) -> Vec<NodeId> {
        let expired: Vec<NodeId> = self
            .relayed
            .iter()
            .filter(|(_, r)| now.duration_since(r.last_alive) > relay_ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.relayed.remove(id);
            self.mailbox.remove(id);
            self.routes.remove(id);
        }
        expired
    }
}

// ============================================================================
// 单元测——路由决策矩阵 / 信箱上限与冲箱 / 注册 TTL
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;
    use crate::transport::FrameKind;

    fn nid(seed: u8) -> NodeId {
        NodeIdentity::from_seed(&[seed; 32]).node_id()
    }

    fn send_frame(src: &NodeId, dst: &NodeId, tag: u64) -> Frame {
        Frame::send(src, dst, serde_json::json!({ "tag": tag }))
    }

    fn snapshot(
        state: &RelayState,
        conns: &[NodeId],
    ) -> (HashSet<NodeId>, HashSet<NodeId>, HashMap<NodeId, NodeId>) {
        (
            conns.iter().cloned().collect(),
            state.relayed_ids(),
            state.routes.clone(),
        )
    }

    // 1. 路由决策矩阵：五种 NextHop 全覆盖（含 relay 失联 → Unknown）
    #[test]
    fn next_hop_decision_matrix() {
        let me = nid(1);
        let peer = nid(2); // 直连对象
        let nat = nid(3); // 经我中继的 NAT 节点
        let far = nid(4); // 经 peer 中继的远端 NAT
        let unknown = nid(5);

        let mut state = RelayState::new();
        state.register_relayed(nat.clone(), Instant::now());
        assert!(state.set_route(far.clone(), peer.clone(), &me));
        // 自环/指向自身拒绝
        assert!(
            !state.set_route(me.clone(), peer.clone(), &me),
            "dst=自身拒绝"
        );
        assert!(
            !state.set_route(far.clone(), me.clone(), &me),
            "relay=自身拒绝"
        );
        assert!(!state.set_route(far.clone(), far.clone(), &me), "自环拒绝");

        let conns = vec![peer.clone()];
        let (c, r, rt) = snapshot(&state, &conns);
        assert_eq!(next_hop(&me, &me, &c, &r, &rt), NextHop::Deliver);
        assert_eq!(next_hop(&me, &peer, &c, &r, &rt), NextHop::Direct);
        assert_eq!(next_hop(&me, &nat, &c, &r, &rt), NextHop::RelayQueue);
        assert_eq!(
            next_hop(&me, &far, &c, &r, &rt),
            NextHop::Forward(peer.clone()),
            "远端 NAT 经其 relay 转发"
        );
        assert_eq!(next_hop(&me, &unknown, &c, &r, &rt), NextHop::Unknown);

        // relay 失联 → 路由不可用 → Unknown（交 lookup 重发现）
        let (c2, r2, rt2) = snapshot(&state, &[]);
        assert_eq!(next_hop(&me, &far, &c2, &r2, &rt2), NextHop::Unknown);
        // 直连优先于中继：nat 直连时走 Direct
        let (c3, r3, rt3) = snapshot(&state, std::slice::from_ref(&nat));
        assert_eq!(next_hop(&me, &nat, &c3, &r3, &rt3), NextHop::Direct);
    }

    // 2. 信箱：入箱/上限 100 丢最旧/冲箱取走/未注册拒绝
    #[test]
    fn mailbox_cap_drop_oldest_and_drain() {
        let mut state = RelayState::new();
        let nat = nid(6);
        let sender = nid(7);
        state.register_relayed(nat.clone(), Instant::now());
        assert!(state.enqueue_offline(&nat, send_frame(&sender, &nat, 0)));
        assert_eq!(state.mailbox_len(&nat), 1);
        // 超限丢最旧
        for tag in 1..=MAILBOX_LIMIT_PER_NODE + 5 {
            assert!(state.enqueue_offline(&nat, send_frame(&sender, &nat, tag as u64)));
        }
        assert_eq!(
            state.mailbox_len(&nat),
            MAILBOX_LIMIT_PER_NODE,
            "上限 {} 条",
            MAILBOX_LIMIT_PER_NODE
        );
        let mail = state.take_mailbox(&nat);
        assert_eq!(mail.len(), MAILBOX_LIMIT_PER_NODE);
        // 最旧 5 条被丢：首条 tag = 6
        assert_eq!(mail[0].app_payload().unwrap()["tag"], 6, "丢最旧保最新");
        assert_eq!(
            mail.last().unwrap().app_payload().unwrap()["tag"],
            MAILBOX_LIMIT_PER_NODE as u64 + 5
        );
        assert!(mail.iter().all(|f| f.kind == FrameKind::Send));
        assert_eq!(state.mailbox_len(&nat), 0, "冲箱后清空");
        assert!(state.take_mailbox(&nat).is_empty());
        // 未注册的节点不能入箱
        let outsider = nid(8);
        assert!(!state.enqueue_offline(&outsider, send_frame(&sender, &outsider, 1)));
    }

    // 3. 注册 TTL：超时清理（连同信箱与路由）；存活续期可保活
    #[test]
    fn registration_ttl_evicts_with_mailbox_unless_alive() {
        let mut state = RelayState::new();
        let nat = nid(9);
        let me = nid(10);
        let now = Instant::now();
        state.register_relayed(nat.clone(), now);
        state.set_route(nat.clone(), me.clone(), &nid(11)); // 他人视角路由（dst=nat）
        state.enqueue_offline(&nat, send_frame(&me, &nat, 1));
        // TTL 内存活 → 不清理
        assert!(state
            .evict_expired(now + Duration::from_secs(10), Duration::from_secs(60))
            .is_empty());
        // 续期后再超时才清
        state.mark_alive(&nat, now + Duration::from_secs(50));
        assert!(state
            .evict_expired(now + Duration::from_secs(100), Duration::from_secs(60))
            .is_empty());
        // 超过 TTL 未续期 → 连信箱带路由清理
        let evicted = state.evict_expired(now + Duration::from_secs(200), Duration::from_secs(60));
        assert_eq!(evicted, vec![nat.clone()]);
        assert!(!state.is_relayed(&nat));
        assert_eq!(state.mailbox_len(&nat), 0);
        assert_eq!(state.route_for(&nat), None);
    }

    // 4. 路由级联清理：relay 除名时 routes_via / remove_routes_via
    #[test]
    fn routes_cleanup_when_relay_evicted() {
        let mut state = RelayState::new();
        let me = nid(20);
        let relay = nid(21);
        let d1 = nid(22);
        let d2 = nid(23);
        state.set_route(d1.clone(), relay.clone(), &me);
        state.set_route(d2.clone(), relay.clone(), &me);
        state.set_route(nid(24), nid(25), &me);
        let via = state.routes_via(&relay);
        assert_eq!(via.len(), 2, "HashMap 序不定，按集合比较");
        assert!(via.contains(&d1) && via.contains(&d2));
        state.remove_routes_via(&relay);
        assert_eq!(state.route_for(&d1), None);
        assert_eq!(state.route_for(&d2), None);
        assert!(state.route_for(&nid(24)).is_some(), "无关路由保留");
    }
}
