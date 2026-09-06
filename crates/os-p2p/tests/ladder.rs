//! os-p2p 集成测试——P2a 的灵魂：观测端点八卦 + TCP 打洞 + 连接阶梯 + mDNS 降级。
//!
//! # 测试拓扑
//!
//! ## loopback 打洞模拟（full-cone NAT 同构）
//!
//! `dial_from_listen_port = true`：节点所有出站连接绑定**监听端口**（SO_REUSEADDR）
//! ——交换所观测到的"公网端点"即真实可拨入的监听口（模拟 NAT 端口稳定映射；
//! 真实 NAT 语义 = full-cone / endpoint-independent mapping）：
//!
//! ```text
//!        P1（公网 = 交换所；观测 A→Pa、B→Pb 并八卦回灌）
//!      ／  ＼
//!   A(Pa)   (Pb)B        A.connect(B)：
//!    └── 同时打开 ──┘     ① PUNCH1{token,Pa} 经 P1 转交 → B
//!                        ② B 回 PUNCH2{token,Pb} → A
//!                        ③ A 拨 Pb ∧ B 拨 Pa（双方绑各自监听口）
//!                        ④ 先建立者胜（入站走现有 listener accept 路径）
//!                        ⑤ 标准握手（ECDH+签名）→ ConnectPath::Punched
//! ```
//!
//! ## 打洞失败落中继
//!
//! `dial_from_listen_port = false`（默认）：出站用临时端口 → 交换所观测到的
//! 是"死映射"（无监听）→ 双方拨打必然被拒 ×3 → PunchFailed → 阶梯落中继。
//!
//! ## 阶梯短路
//!
//! 有直连 underlay（公网对端）时 `connect()` 直拨返回 Direct——punched/punch_failed
//! 计数为 0（结构性短路：不进入打洞代码路径）。

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use os_p2p::{ConnectPath, Handle, P2pConfig, P2pMsg, P2pNode, Timing};

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

/// NAT 节点配置（`reuse_listen_port`：出站绑定监听口——打洞"稳定映射"模拟）。
fn nat_config(bootstrap: Vec<SocketAddr>, reuse_listen_port: bool) -> P2pConfig {
    P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap,
        public: false,
        advertise: None,
        identity: None,
        timings: Timing::testing(),
        dial_from_listen_port: reuse_listen_port,
        mdns_enabled: false,
        meta_file: None,
        identity_ledger: None,
        exit_offered: false,
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
            Ok(_) => continue,
            Err(_elapsed) => return None,
        }
    }
    None
}

/// 节点视角中某邻居的 PeerInfo。
async fn peer_of(node: &Handle, id: &os_p2p::NodeId) -> Option<os_p2p::PeerInfo> {
    node.peers().await.into_iter().find(|p| &p.id == id)
}

// ============================================================================
// ① 观测端点：记录 + 经交换所八卦传播（多实例）+ 自观测回灌
// ============================================================================

#[tokio::test]
async fn observed_endpoints_recorded_and_gossiped() {
    let p1 = P2pNode::spawn(public_config(vec![])).expect("P1 起动（交换所）");
    let boot = vec![p1.listen_addr()];
    let n0 = P2pNode::spawn(nat_config(boot.clone(), false)).expect("N0 起动");
    let n1 = P2pNode::spawn(nat_config(boot, false)).expect("N1 起动");

    // 交换所直接观测：P1 的地址簿有 N0/N1 的条目（socket 对端地址——
    // 即 NAT 映射口；loopback 上 = 出站临时端口）
    let ok = wait_until(WAIT, || async {
        p1.lookup_endpoint(n0.self_id()).await.is_some()
            && p1.lookup_endpoint(n1.self_id()).await.is_some()
    })
    .await;
    assert!(ok, "交换所应观测到两个 NAT 节点的端点");
    let n0_at_p1 = p1.lookup_endpoint(n0.self_id()).await.unwrap();
    assert_eq!(
        n0_at_p1.ip(),
        n0.listen_addr().ip(),
        "观测地址 = socket 对端（loopback）"
    );
    assert_ne!(
        n0_at_p1.port(),
        n0.listen_addr().port(),
        "未开 reuse 时观测口 = 出站临时端口（≠ 监听口）"
    );

    // 八卦传播：N0 经 P1 的 NODES 应答学到 N1 的观测端点（自己没有的视角）；
    // 且学到自己的观测端点（交换所回灌——打洞通告的依据）
    let ok = wait_until(WAIT, || async {
        n0.lookup_endpoint(n1.self_id()).await.is_some()
            && n0.lookup_endpoint(n0.self_id()).await.is_some()
            && n1.lookup_endpoint(n0.self_id()).await.is_some()
    })
    .await;
    assert!(ok, "观测端点应随 NODES 八卦传播到 N0/N1（含自观测回灌）");
    // N0 学到的 N1 端点 == P1 观测的 N1 端点（转述一致）
    assert_eq!(
        n0.lookup_endpoint(n1.self_id()).await,
        p1.lookup_endpoint(n1.self_id()).await
    );
    // 观察面快照非空且含双方
    let entries = n0.known_endpoints().await;
    assert!(entries.len() >= 2, "known_endpoints 观察面");

    n0.shutdown().await;
    n1.shutdown().await;
    p1.shutdown().await;
}

// ============================================================================
// ② loopback 模拟打洞：同时打开成功 → ConnectPath::Punched + 直连 0 跳
// ============================================================================

#[tokio::test]
async fn punch_loopback_simultaneous_open_succeeds() {
    // P1 = 公网交换所；A/B = NAT 节点（reuse_listen_port=true：出站绑监听口，
    // 观测端点即真实可拨入——full-cone 映射模拟）
    let p1 = P2pNode::spawn(public_config(vec![])).expect("P1 起动");
    let boot = vec![p1.listen_addr()];
    let a = P2pNode::spawn(nat_config(boot.clone(), true)).expect("A 起动");
    let b = P2pNode::spawn(nat_config(boot, true)).expect("B 起动");

    // 打洞前置知识：A 知道 B 的观测端点 + 自己的观测端点（交换所八卦回灌）；
    // B 知道自己的观测端点（PUNCH2 通告用）
    let ok = wait_until(WAIT, || async {
        a.lookup_endpoint(b.self_id()).await.is_some()
            && a.lookup_endpoint(a.self_id()).await.is_some()
            && b.lookup_endpoint(b.self_id()).await.is_some()
    })
    .await;
    assert!(ok, "打洞前置：观测端点应经交换所八卦到位");
    // 观测端点 == 各自监听口（reuse 映射模拟生效）
    assert_eq!(
        a.lookup_endpoint(a.self_id()).await.unwrap().port(),
        a.listen_addr().port(),
        "reuse 模式下观测口 = 监听口"
    );

    // 阶梯执行：A.connect(B)——两端点经 P1 交换后同时打开
    let path = tokio::time::timeout(WAIT, a.connect(b.self_id()))
        .await
        .expect("connect 应在超时内返回")
        .expect("打洞应成功");
    assert_eq!(path, ConnectPath::Punched, "必须走打洞路径");

    // 连接建立（A/B 互相可见，入路由表）
    let ok = wait_until(WAIT, || async {
        peer_of(&a, b.self_id()).await.is_some_and(|p| p.connected)
            && peer_of(&b, a.self_id()).await.is_some_and(|p| p.connected)
    })
    .await;
    assert!(ok, "打洞连接应双向入路由表");

    // 直连语义验证：A→B 消息 hops=0（不穿中继——打洞产物是真实直连）
    let mut rx_b = b.on_msg();
    a.send(
        b.self_id(),
        serde_json::json!({"tag": "punch-direct", "text": "洞已打通"}),
    );
    let m = expect_msg(&mut rx_b, a.self_id(), "punch-direct", WAIT)
        .await
        .expect("打洞直连消息必达");
    assert_eq!(m.hops, 0, "打洞产物 = 直连（0 跳）");
    assert_eq!(m.ttl, 16);

    // 阶梯统计：punched ≥ 1
    let ladder = a.ladder_stats().await;
    assert!(ladder.punched >= 1, "punch 计数入阶梯统计");

    a.shutdown().await;
    b.shutdown().await;
    p1.shutdown().await;
}

// ============================================================================
// ③ 打洞失败（死映射）→ PunchFailed 内部短路 → 落中继 ConnectPath::Relayed
// ============================================================================

#[tokio::test]
async fn punch_failure_falls_back_to_relay() {
    // reuse=false：观测端点是出站临时口（无监听）→ 双方拨打必被拒
    let p1 = P2pNode::spawn(public_config(vec![])).expect("P1 起动");
    let boot = vec![p1.listen_addr()];
    let a = P2pNode::spawn(nat_config(boot.clone(), false)).expect("A 起动");
    let b = P2pNode::spawn(nat_config(boot, false)).expect("B 起动");

    let ok = wait_until(WAIT, || async {
        a.lookup_endpoint(b.self_id()).await.is_some()
            && a.lookup_endpoint(a.self_id()).await.is_some()
            && b.lookup_endpoint(b.self_id()).await.is_some()
    })
    .await;
    assert!(ok, "端点八卦到位（打洞会真实执行而非跳过）");

    let path = tokio::time::timeout(WAIT, a.connect(b.self_id()))
        .await
        .expect("connect 应在超时内返回")
        .expect("打洞失败应落中继");
    assert_eq!(path, ConnectPath::Relayed, "打洞失败必须落中继兜底");

    // 阶梯统计对账：打洞失败被记账
    let ladder = a.ladder_stats().await;
    assert!(ladder.punch_failed >= 1, "punch_failed 计数");
    assert!(ladder.punched == 0);

    // 中继路径可用：A→B 经 P1 中继送达（hops≥1）
    let mut rx_b = b.on_msg();
    a.send(b.self_id(), serde_json::json!({"tag": "relay-fallback"}));
    let m = expect_msg(&mut rx_b, a.self_id(), "relay-fallback", WAIT)
        .await
        .expect("落中继后消息必达");
    assert!(m.hops >= 1, "中继路径 ≥1 跳");

    a.shutdown().await;
    b.shutdown().await;
    p1.shutdown().await;
}

// ============================================================================
// ④ 阶梯短路顺序：有直连 underlay 不打洞
// ============================================================================

#[tokio::test]
async fn ladder_short_circuits_direct() {
    // N0 只引导到 P1；P2 的 underlay 经 walk 入 N0 的桶
    let p1 = P2pNode::spawn(public_config(vec![])).expect("P1 起动");
    let p2 = P2pNode::spawn(public_config(vec![p1.listen_addr()])).expect("P2 起动");
    let n0 = P2pNode::spawn(nat_config(vec![p1.listen_addr()], false)).expect("N0 起动");

    // N0 认识 P2（桶内有 underlay）
    let ok = wait_until(WAIT, || async {
        peer_of(&n0, p2.self_id())
            .await
            .is_some_and(|p| p.underlay.is_some())
    })
    .await;
    assert!(ok, "N0 应经 walk 学到 P2 的 underlay");
    // 且 N0 也有 P2 的观测端点（打洞本可尝试——短路才是被测对象）
    let ok = wait_until(WAIT, || async {
        n0.lookup_endpoint(p2.self_id()).await.is_some()
    })
    .await;
    assert!(ok, "P2 的观测端点也在簿（证明直连优先于打洞的短路顺序）");

    let path = tokio::time::timeout(WAIT, n0.connect(p2.self_id()))
        .await
        .expect("connect 应在超时内返回")
        .expect("直连应成功");
    assert_eq!(path, ConnectPath::Direct, "有 underlay 必须直连（不打洞）");

    // 短路对账：无打洞动作（punched/punch_failed 全零——连接或由 redial 探测
    // 先建立（阶梯 0 短路）或由 connect 直拨建立，两种都不进打洞路径）
    let ladder = n0.ladder_stats().await;
    assert_eq!(ladder.punched, 0, "有直连不打洞");
    assert_eq!(ladder.punch_failed, 0);
    // 连接真实存在
    assert!(peer_of(&n0, p2.self_id())
        .await
        .is_some_and(|p| p.connected));

    n0.shutdown().await;
    p1.shutdown().await;
    p2.shutdown().await;
}

// ============================================================================
// ⑤ mDNS 种子不可用/无邻居 → 静默降级 env 引导（组网不受影响）
// ============================================================================

#[tokio::test]
async fn mdns_unavailable_degrades_to_env_bootstrap() {
    // 测试隔离：把 mDNS 服务类型切到测试专属域 `_nexos-p2p-test._tcp.local.`
    // ——开发机（如 106）上常有生产 os-api 在真实域 `_nexos-p2p._tcp.local.`
    // 广播（:7070），测试节点若在真实域 browse 会发现并拨入生产 P2P 端口，
    // 生产 register_conn 把测试节点的回环地址（127.0.0.1:xxxx）记入节点
    // 元数据注册表（测试污染生产）。隔离域内广播/发现只见本测试节点。
    // 类型名须满足 mdns-sd browse 域后缀契约（`._tcp.local.` 结尾）——缺
    // 后缀会被 browse 拒绝，mDNS 整体静默关闭，隔离就名存实亡了。
    // env 操作为 Rust 2024 unsafe——与 bootstrap.rs 单测同款模式（串行独占）。
    let saved_type = std::env::var(os_p2p::bootstrap::ENV_MDNS_TYPE).ok();
    unsafe {
        std::env::set_var(
            os_p2p::bootstrap::ENV_MDNS_TYPE,
            "_nexos-p2p-test._tcp.local.",
        );
    }
    // 两节点都开 mDNS（真实 daemon：隔离域内 P1/N0 同类型互见即拨——同类型
    // 互测逻辑不变，且互见组网正是断言所求；起不来（容器无组播）则静默降级
    // ——两种情况都必须完成组网，关键是不再碰真实域里的生产服务）
    let p1 = P2pNode::spawn(P2pConfig {
        mdns_enabled: true,
        ..public_config(vec![])
    })
    .expect("P1 起动（mDNS on）");
    let n0 = P2pNode::spawn(P2pConfig {
        mdns_enabled: true,
        ..nat_config(vec![p1.listen_addr()], false)
    })
    .expect("N0 起动（mDNS on）");

    // env 引导路径照常工作：N0 连上 P1 且入表（mDNS 同域互见先连上也不影响
    // 该断言——两种来源殊途同归，组网不受 mDNS 可用性影响正是被测语义）
    let ok = wait_until(WAIT, || async {
        peer_of(&n0, p1.self_id())
            .await
            .is_some_and(|p| p.connected)
    })
    .await;
    assert!(
        ok,
        "mDNS 不可用/无邻居时必须静默降级 env 引导（组网不受影响）"
    );

    // 互通冒烟
    let mut rx_p1 = p1.on_msg();
    n0.send(p1.self_id(), serde_json::json!({"tag": "mdns-degrade"}));
    assert!(
        expect_msg(&mut rx_p1, n0.self_id(), "mdns-degrade", WAIT)
            .await
            .is_some(),
        "降级后互通正常"
    );

    n0.shutdown().await;
    p1.shutdown().await;
    // 恢复现场：摘掉隔离服务类型（此前未设置则删除，不留进程级残余）
    unsafe {
        match saved_type {
            Some(v) => std::env::set_var(os_p2p::bootstrap::ENV_MDNS_TYPE, v),
            None => std::env::remove_var(os_p2p::bootstrap::ENV_MDNS_TYPE),
        }
    }
}
