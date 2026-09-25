//! os-p2p 集成测试——P1 的灵魂（设计 §5 验收：单机多实例组网）。
//!
//! # 测试拓扑（模拟 2 公网 + 3 NAT 的真实互联网环境）
//!
//! 全部节点绑定 `127.0.0.1:0`（随机端口）。**NAT 模拟机制**：非公网节点
//! 不通告 underlay（`Hello.underlay = null`）——全网对它没有可拨地址，唯一
//! 可达路径 = 经公网节点中继（与真实 NAT 后节点的网络可达性同构：loopback
//! 上没有不可达 IP，用"不宣告地址"达成同一语义）。
//!
//! ```text
//!              ┌──────── 公网（可直拨/承担 bootstrap+relay）────────┐
//!              │   P1（bootstrap）                P2（bootstrap）   │
//!              │    ●━━━━━━━━━━━━━━━━━━━━━━━━━━━━━●                │
//!              │   ┃┃（中继注册）                ┃┃                │
//!      ┌───────┼───┸┸───────────────────────────┸┸──────┐          │
//!      │ NAT 域│  ● N0        ● N1        ● N2  │          │
//!      │       │  （只出站连 bootstrap；互不可直拨）│          │
//!      └───────┴────────────────────────────────┴─────────┘
//!  逻辑拓扑：NodeID=pubkey → OverlayAddr=keccak(pubkey) → 160 桶 Kademlia
//!  N0→N2 消息路径：N0 ─▶（其 relay：P1 或 P2）─▶ N2   hops=1 ttl=15
//! ```
//!
//! # 五个验收断言（对应任务书）
//!
//! 1. `five_node_topology_converges`：全网路由表收敛（任一节点 peers 覆盖全网）。
//! 2. `send_direct_and_via_relay`：任两节点互通——直连 hops=0 / 经中继 hops≥1
//!    且 ttl<16（ttl/hops 递减验证）。
//! 3. `kill_public_node_evicts_and_reroutes`：kill 一个节点后其余节点桶剔除、
//!    剩余节点间消息绕行仍通。
//! 4. `late_joiner_walks_into_network`：新节点中途加入经 walk 入网 + 互通。
//! 5. `offline_messages_delivered_after_reconnect`：接收方离线期间的消息在
//!    重连后由中继信箱送达（store-and-forward）。

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use os_p2p::{Handle, NodeIdentity, P2pConfig, P2pMsg, P2pNode, PeerInfo, Timing};

/// 收敛/送达等待的统一超时（宽裕——防 CI 负载抖动）。
const WAIT: Duration = Duration::from_secs(20);
/// 轮询间隔。
const POLL: Duration = Duration::from_millis(50);

// ============================================================================
// 测试基建
// ============================================================================

fn public_config(bootstrap: Vec<SocketAddr>) -> P2pConfig {
    P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap,
        public: true,
        advertise: None,
        identity: None,
        timings: Timing::testing(),
        dial_from_listen_port: false,
        mdns_enabled: false,
        meta_file: None,
        identity_ledger: None,
        exit_offered: false,
    }
}

fn nat_config(bootstrap: Vec<SocketAddr>) -> P2pConfig {
    P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap,
        public: false,
        advertise: None,
        identity: None,
        timings: Timing::testing(),
        dial_from_listen_port: false,
        mdns_enabled: false,
        meta_file: None,
        identity_ledger: None,
        exit_offered: false,
    }
}

fn nat_config_with_identity(bootstrap: Vec<SocketAddr>, id: &NodeIdentity) -> P2pConfig {
    P2pConfig {
        identity: Some(id.clone()),
        ..nat_config(bootstrap)
    }
}

/// 轮询直到谓词为真（超时 false——测试自行断言报错）。
async fn wait_until<F, Fut>(timeout: Duration, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if pred().await {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    false
}

/// 收取指定来源 + tag 的消息（滤掉其它测试流量）。
async fn expect_msg(
    rx: &mut tokio::sync::broadcast::Receiver<P2pMsg>,
    want_from: &os_p2p::NodeId,
    want_tag: &str,
    timeout: Duration,
) -> Option<P2pMsg> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remain, rx.recv()).await {
            Ok(Ok(m)) if &m.from == want_from && m.payload["tag"] == want_tag => {
                return Some(m);
            }
            Ok(_) => continue, // 其它消息（别的 tag / 滞留流量）
            Err(_elapsed) => return None,
        }
    }
    None
}

/// 六节点测试网（P1/P2 公网 + N0/N1/N2 NAT），等待全网收敛。
struct Net {
    pubs: Vec<Handle>,
    nats: Vec<Handle>,
}

impl Net {
    async fn spawn() -> Net {
        let p1 = P2pNode::spawn(public_config(vec![])).expect("P1 起动");
        let p2 = P2pNode::spawn(public_config(vec![p1.listen_addr()])).expect("P2 起动");
        let boot = vec![p1.listen_addr(), p2.listen_addr()];
        let nats = (0..3)
            .map(|_| P2pNode::spawn(nat_config(boot.clone())).expect("NAT 起动"))
            .collect();
        let net = Net {
            pubs: vec![p1, p2],
            nats,
        };
        net.wait_converged().await;
        net
    }

    fn all(&self) -> Vec<&Handle> {
        self.pubs.iter().chain(self.nats.iter()).collect()
    }

    /// 任一节点的路由表覆盖全网（k=16 > 5，小网 = 全网入表）。
    async fn wait_converged(&self) {
        for (i, node) in self.all().iter().enumerate() {
            let others = self.all().len() - 1;
            let ok = wait_until(WAIT, || async { node.peers().await.len() >= others }).await;
            assert!(
                ok,
                "节点 #{i} 路由表未覆盖全网（期望 ≥{others} 个已知节点）"
            );
        }
    }

    async fn shutdown_all(&self) {
        for h in self.all() {
            h.clone().shutdown().await;
        }
    }
}

/// 节点视角中某邻居的 PeerInfo。
async fn peer_of(node: &Handle, id: &os_p2p::NodeId) -> Option<PeerInfo> {
    node.peers().await.into_iter().find(|p| &p.id == id)
}

// ============================================================================
// ① 全网路由表收敛
// ============================================================================

#[tokio::test]
async fn five_node_topology_converges() {
    let net = Net::spawn().await;

    // 公网节点与全网直连（4 条活跃连接——两公网互连 + 三个 NAT 入站）
    for (i, p) in net.pubs.iter().enumerate() {
        let ok = wait_until(WAIT, || async {
            p.peers().await.iter().filter(|x| x.connected).count() >= 4
        })
        .await;
        assert!(ok, "公网节点 #{i} 应与全网直连");
    }

    // NAT 节点在公网节点视角：无 underlay（不可直拨）、有中继路由、非公网
    for nat in &net.nats {
        for pubn in &net.pubs {
            let info = peer_of(pubn, nat.self_id()).await.expect("NAT 应已入表");
            assert!(
                info.underlay.is_none(),
                "NAT 不通告 underlay（模拟不可直拨）"
            );
            assert!(!info.public);
            assert!(
                info.relayed_by_me || info.relay.is_some(),
                "公网节点应知道自己（或对端）是该 NAT 的中继"
            );
        }
    }

    // 桶摘要非空且 PO 合法（0..=159）
    let summary = net.pubs[0].buckets_summary().await;
    assert!(!summary.is_empty());
    for s in &summary {
        assert!(s.po <= 159 && s.count <= 16, "桶 {} 非法", s.po);
    }

    // NAT 节点知道彼此的中继路由（DHT walk 学到的可达性记录）
    for nat in &net.nats {
        for other in &net.nats {
            if nat.self_id() != other.self_id() {
                let route = nat.route(other.self_id()).await;
                assert!(
                    route.is_some_and(|r| net.pubs.iter().any(|p| p.self_id() == &r)),
                    "NAT 应学到其它 NAT 经某公网节点中继的路由"
                );
            }
        }
    }

    net.shutdown_all().await;
}

// ============================================================================
// ② 任两节点 send 互通（直连 + 经中继，ttl/hops 递减）
// ============================================================================

#[tokio::test]
async fn send_direct_and_via_relay() {
    let net = Net::spawn().await;
    let (p1, p2) = (&net.pubs[0], &net.pubs[1]);
    let (n0, n1, n2) = (&net.nats[0], &net.nats[1], &net.nats[2]);

    // 直连路径：P1 → P2（公网互连，不穿中继）
    let mut rx_p2 = p2.on_msg();
    p1.send(
        p2.self_id(),
        serde_json::json!({"tag": "t2-direct", "text": "pub→pub"}),
    );
    let m = expect_msg(&mut rx_p2, p1.self_id(), "t2-direct", WAIT)
        .await
        .expect("直连消息必达");
    assert_eq!(m.hops, 0, "直连 0 跳");
    assert_eq!(m.ttl, 16, "直连不减 ttl");
    assert_eq!(m.payload["text"], "pub→pub");

    // 公网 → NAT（NAT 入站连着公网 = 直连路径）
    let mut rx_n0 = n0.on_msg();
    p2.send(n0.self_id(), serde_json::json!({"tag": "t2-pub2nat"}));
    let m = expect_msg(&mut rx_n0, p2.self_id(), "t2-pub2nat", WAIT)
        .await
        .expect("公网→NAT 必达");
    assert_eq!(m.hops, 0);

    // NAT → 公网（NAT 出站连着 bootstrap = 直连路径）
    let mut rx_p1 = p1.on_msg();
    n0.send(p1.self_id(), serde_json::json!({"tag": "t2-nat2pub"}));
    let m = expect_msg(&mut rx_p1, n0.self_id(), "t2-nat2pub", WAIT)
        .await
        .expect("NAT→公网必达");
    assert_eq!(m.hops, 0);

    // 经中继路径：N0 → N1（互不可直拨，经 bootstrap 中继；ttl/hops 递减验证）
    let mut rx_n1 = n1.on_msg();
    n0.send(
        n1.self_id(),
        serde_json::json!({"tag": "t2-relay", "text": "nat→nat"}),
    );
    let m = expect_msg(&mut rx_n1, n0.self_id(), "t2-relay", WAIT)
        .await
        .expect("经中继消息必达");
    assert!(m.hops >= 1, "经中继至少 1 跳（实测 {}）", m.hops);
    assert!(m.ttl < 16, "每经一跳 ttl-1（实测 {}）", m.ttl);
    assert_eq!(16 - m.ttl, m.hops, "ttl 减量 == hops 增量");

    // 邻居对全排列抽查：N2 → N0 反向也通
    let mut rx_n0b = n0.on_msg();
    n2.send(n0.self_id(), serde_json::json!({"tag": "t2-relay-back"}));
    let m = expect_msg(&mut rx_n0b, n2.self_id(), "t2-relay-back", WAIT)
        .await
        .expect("反向中继必达");
    assert!(m.hops >= 1);

    net.shutdown_all().await;
}

// ============================================================================
// ③ kill 一个节点后桶剔除 + 消息绕行
// ============================================================================

#[tokio::test]
async fn kill_public_node_evicts_and_reroutes() {
    let net = Net::spawn().await;
    let (p1, p2) = (&net.pubs[0], &net.pubs[1]);
    let p2_id = p2.self_id().clone();

    // kill 公网节点 P2（优雅停机 = 断连；端口随之关闭 → 重拨必失败）
    p2.clone().shutdown().await;

    // 其余全部节点把 P2 从路由表剔除（重拨连续失败 → 桶除名）
    for (i, node) in net.all().iter().enumerate() {
        if node.self_id() == &p2_id {
            continue;
        }
        let ok = wait_until(WAIT, || async { peer_of(node, &p2_id).await.is_none() }).await;
        assert!(ok, "节点 #{i} 应在 P2 失效后将其从桶剔除");
    }

    // 消息绕行：P2 已死，N0 → N1 经 P1 中继仍通
    let (n0, n1) = (&net.nats[0], &net.nats[1]);
    let mut rx_n1 = n1.on_msg();
    n0.send(n1.self_id(), serde_json::json!({"tag": "t3-reroute"}));
    let m = expect_msg(&mut rx_n1, n0.self_id(), "t3-reroute", WAIT)
        .await
        .expect("P2 死后消息应绕行（经 P1 中继）");
    assert!(m.hops >= 1);

    // 绕行 = 重新学到经幸存公网节点 P1 的路由（旧 P2 路由已级联清除）
    let ok = wait_until(WAIT, || async {
        n0.route(n1.self_id()).await == Some(p1.self_id().clone())
    })
    .await;
    assert!(ok, "N0→N1 的可达性路由应收敛到幸存中继 P1");

    // 死节点不再出现在 P1 的路由信息里
    assert!(peer_of(p1, &p2_id).await.is_none());

    // 清理（P2 已 shutdown；其余收尾）
    for h in net.all() {
        if h.self_id() != &p2_id {
            h.clone().shutdown().await;
        }
    }
}

// ============================================================================
// ④ 新节点中途加入经 walk 入网
// ============================================================================

#[tokio::test]
async fn late_joiner_walks_into_network() {
    let net = Net::spawn().await;
    let p1 = &net.pubs[0];

    // 新 NAT 节点只连一个引导节点（最小知识冷启动）
    let late = P2pNode::spawn(nat_config(vec![p1.listen_addr()])).expect("迟到者起动");

    // 迟到者经 walk 学到全网
    let others = net.all().len(); // 5 个既有节点
    let ok = wait_until(WAIT, || async { late.peers().await.len() >= others }).await;
    assert!(ok, "迟到者应经 FINDNODE walk 覆盖全网");

    // 既有节点也认识到迟到者（refresh walk 学到）
    for (i, node) in net.all().iter().enumerate() {
        let ok = wait_until(WAIT, || async {
            peer_of(node, late.self_id()).await.is_some()
        })
        .await;
        assert!(ok, "既有节点 #{i} 应认识到迟到者");
    }

    // 迟到者与任意 NAT 互通（含按需 lookup 路径）
    let n1 = &net.nats[1];
    let mut rx_late = late.on_msg();
    n1.send(late.self_id(), serde_json::json!({"tag": "t4-welcome"}));
    let m = expect_msg(&mut rx_late, n1.self_id(), "t4-welcome", WAIT)
        .await
        .expect("既有 NAT → 迟到者必达");
    assert!(m.hops >= 1, "NAT→NAT 经中继（≥1 跳）");

    let mut rx_n2 = net.nats[2].on_msg();
    late.send(
        net.nats[2].self_id(),
        serde_json::json!({"tag": "t4-hello"}),
    );
    let m = expect_msg(&mut rx_n2, late.self_id(), "t4-hello", WAIT)
        .await
        .expect("迟到者 → 既有 NAT 必达");
    assert!(m.hops >= 1);

    late.shutdown().await;
    net.shutdown_all().await;
}

// ============================================================================
// ⑤ 离线消息：接收方重连后经中继信箱送达（store-and-forward）
// ============================================================================

#[tokio::test]
async fn offline_messages_delivered_after_reconnect() {
    // N1 用固定身份（离线重连后保持同一 NodeID——中继信箱按 NodeID 归档）
    let n1_identity = NodeIdentity::generate();
    let p1 = P2pNode::spawn(public_config(vec![])).expect("P1 起动");
    let p2 = P2pNode::spawn(public_config(vec![p1.listen_addr()])).expect("P2 起动");
    let boot = vec![p1.listen_addr(), p2.listen_addr()];
    let n0 = P2pNode::spawn(nat_config(boot.clone())).expect("N0 起动");
    let n1 = P2pNode::spawn(nat_config_with_identity(boot.clone(), &n1_identity)).expect("N1 起动");
    let n1_id = n1.self_id().clone();

    // 小网收敛（4 节点）
    for (i, node) in [&p1, &p2, &n0, &n1].iter().enumerate() {
        let ok = wait_until(WAIT, || async { node.peers().await.len() >= 3 }).await;
        assert!(ok, "节点 #{i} 未收敛");
    }
    // 先验证在线互通
    let mut rx_n1 = n1.on_msg();
    n0.send(&n1_id, serde_json::json!({"tag": "t5-online"}));
    assert!(
        expect_msg(&mut rx_n1, n0.self_id(), "t5-online", WAIT)
            .await
            .is_some(),
        "在线基线必达"
    );

    // N1 离线（保持身份，稍后重连）
    n1.shutdown().await;
    // 等 P1/P2 视角里 N1 断连（中继保留注册与信箱）
    for (i, relay) in [&p1, &p2].iter().enumerate() {
        let ok = wait_until(WAIT, || async {
            peer_of(relay, &n1_id).await.is_some_and(|p| !p.connected)
        })
        .await;
        assert!(ok, "中继 #{i} 应看到 N1 已断连（但保留条目）");
    }

    // N0 → 离线的 N1：消息进入中继信箱（store-and-forward）
    n0.send(&n1_id, serde_json::json!({"tag": "t5-offline-1", "seq": 1}));
    n0.send(&n1_id, serde_json::json!({"tag": "t5-offline-2", "seq": 2}));
    tokio::time::sleep(Duration::from_millis(300)).await; // 入箱

    // N1 以同一身份重连 → 信箱冲刷送达
    let n1b = P2pNode::spawn(nat_config_with_identity(boot, &n1_identity)).expect("N1 重连起动");
    assert_eq!(n1b.self_id(), &n1_id, "同私钥 → 同 NodeID");
    let mut rx_n1b = n1b.on_msg();
    let m1 = expect_msg(&mut rx_n1b, n0.self_id(), "t5-offline-1", WAIT)
        .await
        .expect("离线消息 1 应在重连后送达");
    let m2 = expect_msg(&mut rx_n1b, n0.self_id(), "t5-offline-2", WAIT)
        .await
        .expect("离线消息 2 应在重连后送达");
    assert_eq!(m1.payload["seq"], 1);
    assert_eq!(m2.payload["seq"], 2);
    assert!(m1.hops >= 1 && m2.hops >= 1, "经中继信箱投递（≥1 跳）");

    // 重连后在线互通恢复
    n0.send(&n1_id, serde_json::json!({"tag": "t5-after"}));
    assert!(
        expect_msg(&mut rx_n1b, n0.self_id(), "t5-after", WAIT)
            .await
            .is_some(),
        "重连后在线互通恢复"
    );

    n1b.shutdown().await;
    n0.shutdown().await;
    p1.shutdown().await;
    p2.shutdown().await;
}
