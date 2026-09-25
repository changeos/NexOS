//! TCP 打洞——设计 §2「连接阶梯第 3 级」+ §3「punch」（08-20 补丁）。
//!
//! # 场景
//!
//! A、B 都在 NAT 后（无可拨 underlay），但都连着共同可达节点 C（公网交换所）。
//! C 对 A/B 各自观测到了 NAT 映射口——经端点八卦，A 知道"B 在公网看来是
//! ip:Pb"且知道自己的观测端点 Pa。打洞流程：
//!
//! ```text
//!   A（发起方）                C（共同中介/交换所）              B（目标）
//!     │ ── punch1{dst=B, ──────▶ │ 收到 dst≠自己 → 直连转交 ──▶ │
//!     │     token, Pa}          │                              │ 学习 Pa
//!     │                         │ ◀── punch2{dst=A, ────────── │
//!     │ ◀── 转交 ────────────── │        token, Pb}            │ 发送后稍候
//!     │ 收到即向 Pb 发起 connect │                              │ 向 Pa 发起 connect
//!     │         （双方同时打开 simultaneous open）              │
//!     │ ══════════════ TCP 直连建立 ══════════════════════════ │
//!     │   走标准握手（ECDH+签名）→ register_conn → ConnectPath::Punched
//! ```
//!
//! - **同时打开**：双方各自向对方观测端点出站 connect；打洞连接是出站+入站
//!   竞速，**先完成握手注册者胜**（重复连接由 register_conn 去重拒绝——对端
//!   拨来的入站连接落在同一 listener 上，复用现有 accept 路径）。
//! - **映射复用**：打洞出站 socket 绑定与中介连接相同的本地端口
//!   （`Conn::local`——`dial_from_listen_port` 配置让出站连接复用监听口，
//!   loopback 上即模拟 full-cone NAT 的稳定映射）。
//! - **重试**：每轮按端点轮转拨打 [`PUNCH_ATTEMPTS`] = 3 次，间隔
//!   `Timing::punch_retry_interval`（默认 800ms）；全部失败 →
//!   [`ConnectError::PunchFailed`]，连接阶梯继续落中继兜底。
//!
//! # 连接阶梯（`Handle::connect` 的实现）
//!
//! ```text
//!  已直连 ──▶ Direct（短路）
//!  桶内 underlay 可拨 ──▶ 直接拨号 ──▶ Direct
//!  端点簿有观测端点 ──▶ TCP 打洞 ──成功──▶ Punched
//!                                └─失败──▶ 落中继路由 ──▶ Relayed
//!  什么都没有 ──▶ Err(NoRoute / PunchFailed)
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::api::{self, Conn, Shared};
use crate::endpoints::EndpointGossip;
use crate::identity::NodeId;
use crate::transport::Frame;

/// 每端点同时打开尝试轮数。
pub const PUNCH_ATTEMPTS: u32 = 3;
/// 响应方发送 PUNCH2 后的约定等待上限（双方入网时刻对齐窗口）。
pub const PUNCH_RDV_DELAY: Duration = Duration::from_millis(150);

// ============================================================================
// 连接阶梯观察面
// ============================================================================

/// `Handle::connect` 的阶梯结果（本连接落在哪一级建立）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectPath {
    /// 已直连 / underlay 直接拨号成功（阶梯 1-2 级）。
    Direct,
    /// TCP 打洞建立的同时打开直连（阶梯 3 级）。
    Punched,
    /// 经中继路由可达（阶梯 4 级兜底——虚拟路径，SEND 走 relay 转发）。
    Relayed,
}

/// 连接建立失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    /// 打洞失败且无中继路由可落（NAT 类型不支持 / 端点全失效）。
    /// NodeID 装箱（压缩公钥 144B——装箱保持 Err 精简）。
    #[error("tcp punch failed for {0} and no relay route available")]
    PunchFailed(Box<NodeId>),
    /// 对目标一无所知（无直连地址 / 无观测端点 / 无中继路由）。
    #[error("no route to {0}")]
    NoRoute(Box<NodeId>),
}

/// 连接阶梯统计（`Handle::ladder_stats` / CLI status）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LadderStats {
    /// 直连建立次数（underlay 直拨）。
    pub direct: u64,
    /// 打洞成功次数。
    pub punched: u64,
    /// 落中继次数。
    pub relayed: u64,
    /// 打洞失败次数（随后落中继或报错）。
    pub punch_failed: u64,
}

// ============================================================================
// 纯函数：会话 token / 约定时刻 / 拨号计划
// ============================================================================

/// 随机打洞会话 token（128-bit hex——关联 PUNCH1/PUNCH2 与并发去重）。
#[must_use]
pub fn fresh_punch_token() -> String {
    use k256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// 响应方约定延迟：由随机字节线性映射到 `[0, PUNCH_RDV_DELAY]`
/// （纯函数便于测试；随机源仅一字节）。
#[must_use]
pub fn delay_from_byte(b: u8) -> Duration {
    let max = PUNCH_RDV_DELAY.as_millis() as u64;
    Duration::from_millis(u64::from(b) * max / 255)
}

/// 随机取一个响应方约定延迟。
fn random_responder_delay() -> Duration {
    use k256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut b = [0u8; 1];
    OsRng.fill_bytes(&mut b);
    delay_from_byte(b[0])
}

/// 打洞拨号计划：对端观测端点 + 约定开始时刻 + 重试节奏（纯数据，可穷举测试）。
#[derive(Debug, Clone)]
pub struct PunchPlan {
    /// 会话 token（PUNCH1/PUNCH2 回显关联）。
    pub token: String,
    /// 对端观测端点（同时打开的拨打目标）。
    pub peer_endpoints: Vec<SocketAddr>,
    /// 约定的开始拨打时刻。
    pub start_at: Instant,
    /// 尝试轮数。
    pub attempts: u32,
    /// 轮间隔。
    pub retry_interval: Duration,
}

impl PunchPlan {
    /// 发起方计划：收到 PUNCH2 即刻开始（对端已在路上）。
    #[must_use]
    pub fn initiator(
        token: String,
        peer_endpoints: Vec<SocketAddr>,
        now: Instant,
        retry_interval: Duration,
    ) -> Self {
        Self {
            token,
            peer_endpoints,
            start_at: now,
            attempts: PUNCH_ATTEMPTS,
            retry_interval,
        }
    }

    /// 响应方计划：发送 PUNCH2 后延迟 `delay`（给转发留传输窗口）。
    #[must_use]
    pub fn responder(
        token: String,
        peer_endpoints: Vec<SocketAddr>,
        now: Instant,
        delay: Duration,
        retry_interval: Duration,
    ) -> Self {
        Self {
            start_at: now + delay,
            ..Self::initiator(token, peer_endpoints, now, retry_interval)
        }
    }

    /// 距约定时刻还需等待多久（相对 `now` 的纯函数）。
    #[must_use]
    pub fn wait_from(&self, now: Instant) -> Duration {
        self.start_at.saturating_duration_since(now)
    }

    /// 第 `attempt` 轮的拨打目标：按轮数轮转起点（多端点均摊探测），去重保序。
    #[must_use]
    pub fn attempt_targets(&self, attempt: u32) -> Vec<SocketAddr> {
        if self.peer_endpoints.is_empty() {
            return Vec::new();
        }
        let n = self.peer_endpoints.len();
        let offset = (attempt as usize) % n;
        self.peer_endpoints
            .iter()
            .cycle()
            .skip(offset)
            .take(n)
            .copied()
            .collect()
    }
}

// ============================================================================
// 连接阶梯（Handle::connect 实现）
// ============================================================================

fn bump(shared: &Shared, field: fn(&mut LadderStats)) {
    field(&mut shared.state.lock().expect("state poisoned").ladder);
}

/// 连接阶梯：已直连 → underlay 直拨 → 观测端点打洞 → 中继兜底。
///
/// 高优先级成功即短路（有直连不打洞、能打洞不中继）。
pub(crate) async fn connect_ladder(
    shared: &Arc<Shared>,
    target: &NodeId,
) -> Result<ConnectPath, ConnectError> {
    if target == &shared.self_id {
        return Err(ConnectError::NoRoute(Box::new(target.clone())));
    }
    // 阶梯 0：已有已认证直连——短路
    if api::is_connected(shared, target) {
        return Ok(ConnectPath::Direct);
    }
    // 阶梯 1/2：桶内 underlay 可拨（公网节点 / LAN 邻居）——直接拨号
    let info = {
        let st = shared.state.lock().expect("state poisoned");
        st.buckets.get(target).map(|e| e.info.clone())
    };
    if let Some(info) = info.filter(|i| i.dialable()) {
        if api::ensure_conn(shared, &info).await.is_some() {
            bump(shared, |s| s.direct += 1);
            tracing::info!(
                peer = %crate::short_hex(&target.to_hex()),
                "连接阶梯：underlay 直连"
            );
            return Ok(ConnectPath::Direct);
        }
    }
    // 阶梯 3：端点簿有观测端点——TCP 打洞
    let punched = {
        let st = shared.state.lock().expect("state poisoned");
        st.endpoints.lookup(target).is_some()
    };
    let mut punch_attempted = false;
    if punched {
        punch_attempted = true;
        if try_punch(shared, target).await {
            bump(shared, |s| s.punched += 1);
            tracing::info!(
                peer = %crate::short_hex(&target.to_hex()),
                "连接阶梯：TCP 打洞成功（同时打开直连）"
            );
            return Ok(ConnectPath::Punched);
        }
        bump(shared, |s| s.punch_failed += 1);
        tracing::info!(
            peer = %crate::short_hex(&target.to_hex()),
            "连接阶梯：打洞失败，尝试中继兜底"
        );
    }
    // 阶梯 4：中继兜底（虚拟路径：SEND 经 relay 转发）。路由知识可能缺失或
    // 陈旧（本节点的上次 walk 早于对端注册中继）——先做一次定向 walk 再查。
    let mut relay = {
        let st = shared.state.lock().expect("state poisoned");
        st.relay.route_for(target)
    };
    if relay.is_none() {
        crate::api::lookup(shared.clone(), target.overlay()).await;
        relay = {
            let st = shared.state.lock().expect("state poisoned");
            st.relay.route_for(target)
        };
    }
    if let Some(relay) = relay.filter(|r| api::is_connected(shared, r)) {
        bump(shared, |s| s.relayed += 1);
        tracing::info!(
            peer = %crate::short_hex(&target.to_hex()),
            relay = %crate::short_hex(&relay.to_hex()),
            "连接阶梯：落中继路径"
        );
        return Ok(ConnectPath::Relayed);
    }
    // 全阶梯失败：打过洞报 PunchFailed（更精确），否则 NoRoute
    Err(if punch_attempted {
        ConnectError::PunchFailed(Box::new(target.clone()))
    } else {
        ConnectError::NoRoute(Box::new(target.clone()))
    })
}

// ============================================================================
// 打洞执行
// ============================================================================

/// 打洞总入口（`punching` 并发防抖内环）：经每个已连中介逐一尝试。
async fn try_punch(shared: &Arc<Shared>, target: &NodeId) -> bool {
    {
        let mut st = shared.state.lock().expect("state poisoned");
        if st.punching.contains(target) {
            return false;
        }
        st.punching.insert(target.clone());
    }
    let outcome = punch_all_intermediaries(shared, target).await;
    shared
        .state
        .lock()
        .expect("state poisoned")
        .punching
        .remove(target);
    outcome
}

async fn punch_all_intermediaries(shared: &Arc<Shared>, target: &NodeId) -> bool {
    // 我的观测端点（交换所八卦回灌的"网络看我是什么"）——PUNCH1 通告给对端
    let my_endpoints: Vec<SocketAddr> = {
        let st = shared.state.lock().expect("state poisoned");
        st.endpoints.lookup(&shared.self_id).into_iter().collect()
    };
    if my_endpoints.is_empty() {
        tracing::debug!(
            peer = %crate::short_hex(&target.to_hex()),
            "无自身观测端点（未被交换所观测过），无法发起打洞"
        );
        return false;
    }
    // 中介候选：已直连节点（公网优先——更可能同时连着目标），排除目标自身
    let candidates: Vec<Arc<Conn>> = {
        let st = shared.state.lock().expect("state poisoned");
        let mut v: Vec<Arc<Conn>> = st
            .conns
            .values()
            .filter(|c| c.peer != *target && !c.is_closed())
            .cloned()
            .collect();
        v.sort_by_key(|c| !c.public); // 公网（交换所角色）排前
        v
    };
    if candidates.is_empty() {
        tracing::debug!("无任何已连节点可充当中介，打洞无从发起");
        return false;
    }
    for via in candidates {
        // 对端可能先建立（入站竞速胜出）——每轮前检查
        if api::is_connected(shared, target) {
            return true;
        }
        if punch_via(shared, target, &via, &my_endpoints).await {
            return true;
        }
    }
    api::is_connected(shared, target)
}

/// 经单个中介的打洞：PUNCH1 转交 → 等 PUNCH2 回带对端端点 → 同时打开拨号。
async fn punch_via(
    shared: &Arc<Shared>,
    target: &NodeId,
    via: &Arc<Conn>,
    my_endpoints: &[SocketAddr],
) -> bool {
    let token = fresh_punch_token();
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut st = shared.state.lock().expect("state poisoned");
        st.pending_punch.insert(token.clone(), tx);
    }
    let cleanup = |shared: &Arc<Shared>, token: &str| {
        shared
            .state
            .lock()
            .expect("state poisoned")
            .pending_punch
            .remove(token);
    };
    // dst=最终目标：中介节点按直连转交（api::handle_frame 的 punch 分支）
    if !via.try_send(Frame::punch1(&shared.self_id, target, &token, my_endpoints)) {
        cleanup(shared, &token);
        return false;
    }
    let peer_endpoints = match tokio::time::timeout(shared.timing.punch_setup_timeout, rx).await {
        Ok(Ok(eps)) if !eps.is_empty() => eps,
        Ok(Ok(_)) => {
            tracing::debug!(peer = %crate::short_hex(&target.to_hex()), "对端无可通告观测端点");
            cleanup(shared, &token);
            return false;
        }
        Ok(Err(_)) => {
            cleanup(shared, &token);
            return false; // 发起方等待通道关闭（节点停机边缘）
        }
        Err(_) => {
            tracing::debug!(
                peer = %crate::short_hex(&target.to_hex()),
                "打洞端点交换超时（中介不知目标或目标拒绝）"
            );
            cleanup(shared, &token);
            return false;
        }
    };
    cleanup(shared, &token);
    let plan = PunchPlan::initiator(
        token,
        peer_endpoints,
        Instant::now(),
        shared.timing.punch_retry_interval,
    );
    run_punch_dials(shared, target, plan, via.local).await
}

/// 同时打开拨号循环：轮转端点 × [`PUNCH_ATTEMPTS`] 轮，每轮间检查对端入站
/// 连接是否已先建立（竞速胜出即成功）；本地端口复用中介连接的映射口。
pub(crate) async fn run_punch_dials(
    shared: &Arc<Shared>,
    target: &NodeId,
    plan: PunchPlan,
    local_bind: Option<SocketAddr>,
) -> bool {
    let wait = plan.wait_from(Instant::now());
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    for attempt in 0..plan.attempts {
        for addr in plan.attempt_targets(attempt) {
            if api::is_connected(shared, target) {
                return true; // 对端先建立（入站竞速胜出）
            }
            // 端点拨号失败（NAT 拒绝/无映射）→ None：直接进入下一轮重试
            if let Some(stream) = api::dial_socket(shared, addr, local_bind).await {
                if let Some(accepted) = api::handshake_stream(shared, stream).await {
                    if api::register_conn(shared, accepted).await.is_ok() {
                        tracing::info!(
                            peer = %crate::short_hex(&target.to_hex()),
                            addr = %addr,
                            attempt,
                            "TCP 打洞成功：同时打开连接已建立并走标准握手升级"
                        );
                        return true;
                    }
                }
            }
        }
        if api::is_connected(shared, target) {
            return true;
        }
        if attempt + 1 < plan.attempts {
            tokio::time::sleep(plan.retry_interval).await;
        }
    }
    api::is_connected(shared, target)
}

/// 打洞响应方（收到 PUNCH1 的目标节点）：学习发起方端点 → 回 PUNCH2（自己的
/// 观测端点）→ 按约定时刻向发起方端点同时打开。
pub(crate) fn spawn_punch_responder(
    shared: Arc<Shared>,
    via: Arc<Conn>,
    initiator: NodeId,
    token: String,
    initiator_endpoints: Vec<SocketAddr>,
) {
    {
        let mut st = shared.state.lock().expect("state poisoned");
        let gossip: Vec<EndpointGossip> = initiator_endpoints
            .iter()
            .map(|&a| EndpointGossip::new(initiator.clone(), a))
            .collect();
        st.endpoints.learn(&gossip, Instant::now());
        if st.punching.contains(&initiator) {
            tracing::debug!(
                peer = %crate::short_hex(&initiator.to_hex()),
                "并发打洞请求（已在打洞），忽略"
            );
            return;
        }
        st.punching.insert(initiator.clone());
    }
    let own: Vec<SocketAddr> = {
        let st = shared.state.lock().expect("state poisoned");
        st.endpoints.lookup(&shared.self_id).into_iter().collect()
    };
    if own.is_empty() {
        shared
            .state
            .lock()
            .expect("state poisoned")
            .punching
            .remove(&initiator);
        tracing::debug!(
            peer = %crate::short_hex(&initiator.to_hex()),
            "无自身观测端点可通告，忽略打洞请求"
        );
        return;
    }
    let worker = shared.clone();
    api::spawn_tracked(&shared, async move {
        let shared = worker;
        // 回 PUNCH2（dst=发起方；经收到 PUNCH1 的连接发给中介转交）
        if !via.try_send(Frame::punch2(&shared.self_id, &initiator, &token, &own)) {
            shared
                .state
                .lock()
                .expect("state poisoned")
                .punching
                .remove(&initiator);
            return;
        }
        let plan = PunchPlan::responder(
            token,
            initiator_endpoints,
            Instant::now(),
            random_responder_delay(),
            shared.timing.punch_retry_interval,
        );
        let ok = run_punch_dials(&shared, &initiator, plan, via.local).await;
        shared
            .state
            .lock()
            .expect("state poisoned")
            .punching
            .remove(&initiator);
        tracing::debug!(
            peer = %crate::short_hex(&initiator.to_hex()),
            ok,
            "打洞响应方完成"
        );
    });
}

// ============================================================================
// 单元测——会话 token / 约定时刻 / 端点轮转（纯函数）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        format!("203.0.113.9:{port}").parse().unwrap()
    }

    // 1. 会话 token：格式（32 hex）、CSPRNG 唯一
    #[test]
    fn punch_token_fresh_and_unique() {
        let t1 = fresh_punch_token();
        let t2 = fresh_punch_token();
        assert_eq!(t1.len(), 32, "128-bit hex");
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t1, t2, "CSPRNG 不重复");
    }

    // 2. 约定时刻：发起方即刻（wait=0）；响应方延迟 `delay` 后开始；
    //    delay_from_byte 线性映射端点（0 → 0，255 → PUNCH_RDV_DELAY）
    #[test]
    fn punch_plan_rendezvous_timing() {
        let now = Instant::now();
        let eps = vec![addr(1), addr(2)];
        let init = PunchPlan::initiator("t1".into(), eps.clone(), now, Duration::from_millis(50));
        assert_eq!(init.wait_from(now), Duration::ZERO, "发起方收到即拨");
        assert_eq!(init.attempts, PUNCH_ATTEMPTS);
        assert_eq!(init.retry_interval, Duration::from_millis(50));
        let resp = PunchPlan::responder(
            "t2".into(),
            eps,
            now,
            Duration::from_millis(120),
            Duration::from_millis(50),
        );
        assert_eq!(
            resp.wait_from(now),
            Duration::from_millis(120),
            "响应方等约定时刻"
        );
        assert_eq!(resp.attempts, PUNCH_ATTEMPTS, "响应方同预算");
        // 时间走过一半：剩余等待相应减半（saturating 不负）
        let later = now + Duration::from_millis(60);
        assert_eq!(resp.wait_from(later), Duration::from_millis(60));
        assert_eq!(init.wait_from(later), Duration::ZERO);
        // 随机延迟映射：0/255 两端
        assert_eq!(delay_from_byte(0), Duration::ZERO);
        assert_eq!(delay_from_byte(255), PUNCH_RDV_DELAY);
        assert!(delay_from_byte(128) > Duration::ZERO && delay_from_byte(128) < PUNCH_RDV_DELAY);
    }

    // 3. 端点轮转：attempt 轮转起点、多轮均摊；空端点空目标
    #[test]
    fn punch_plan_endpoint_rotation() {
        let eps = vec![addr(1), addr(2), addr(3)];
        let plan = PunchPlan::initiator("t".into(), eps, Instant::now(), Duration::ZERO);
        assert_eq!(plan.attempt_targets(0), vec![addr(1), addr(2), addr(3)]);
        assert_eq!(plan.attempt_targets(1), vec![addr(2), addr(3), addr(1)]);
        assert_eq!(plan.attempt_targets(2), vec![addr(3), addr(1), addr(2)]);
        // 超出端点数的轮数取模回绕
        assert_eq!(plan.attempt_targets(3), plan.attempt_targets(0));
        // 单端点每轮同一目标
        let single =
            PunchPlan::initiator("t".into(), vec![addr(9)], Instant::now(), Duration::ZERO);
        assert_eq!(single.attempt_targets(7), vec![addr(9)]);
        // 空端点 → 无目标（调用方自然失败）
        let empty = PunchPlan::initiator("t".into(), vec![], Instant::now(), Duration::ZERO);
        assert!(empty.attempt_targets(0).is_empty());
    }
}
