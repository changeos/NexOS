//! WebSocket Hub——内存订阅/推送模型（对接 os-core EventBus，规划文档 §3.6 / §9.1#9）。
//!
//! `WsHub` 是纯内存实现：维护 user -> 订阅集合 的映射；每条订阅有一个 `SubscriptionId`
//! 与一个广播通道。`broadcast` 遍历所有订阅推送；`send_to` 仅推送给指定用户的订阅。
//!
//! 真实 Axum WS 连接由网关在握手时调用 `subscribe(user)` 拿到订阅 ID，
//! 然后从该订阅的接收端循环读消息写给客户端；连接断开时 `unsubscribe`。
//! 本骨架用 `tokio::sync::broadcast` 通道，可与真实 Axum 集成。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use os_core::SubscriptionId;
use tokio::sync::broadcast;

use crate::websocket::WsMessage;

/// 单条订阅：用户 + 接收端句柄 + 最近客户端活动时间。
struct Subscription {
    user: String,
    tx: broadcast::Sender<WsMessage>,
    /// 最近一次**客户端→服务端**方向的连接活动时刻（握手创建时置 now，
    /// [`WsHub::touch_raw`] 在真实连接读循环收到任意客户端帧时刷新）。
    ///
    /// - 用 `SystemTime` 而非 `Instant`：可任意回拨（测试构造"半开僵死"
    ///   订阅无需依赖进程已运行时长）；单调性损失不敏感——120s 量级的
    ///   新鲜度窗口 + 判定侧 `unwrap_or_default` 兜底时钟回拨。
    /// - **多租户影响评估**：该字段挂在通用 WS hub 上，IM 网页端等既有
    ///   订阅者只多记一个时间戳——touch 是既有 `subs` RwLock 写锁内的
    ///   一次纳秒级字段赋值（每客户端约每 25s 一次协议层 ping 触发），
    ///   广播/定向推送路径（读锁）不受语义影响；过期**不删订阅**，仅
    ///   [`WsHub::fresh_subscriber_count_for`] 的新鲜度判定读取它。
    last_active: SystemTime,
}

/// WebSocket Hub（线程安全）。
///
/// `Clone`：内部状态用 `Arc` 共享，clone 廉价（同一 hub 的多个 clone 共享订阅表），
/// 便于 axum WS handler 各自持有一份句柄。
#[derive(Clone)]
pub struct WsHub {
    inner: std::sync::Arc<WsHubInner>,
}

#[derive(Default)]
struct WsHubInner {
    next_id: AtomicU64,
    subs: RwLock<HashMap<SubscriptionId, Subscription>>,
    /// user -> 订阅 ID 列表（便于定向推送）
    by_user: RwLock<HashMap<String, Vec<SubscriptionId>>>,
    /// 广播通道容量（每条订阅的 broadcast 通道大小）
    channel_cap: usize,
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new(256)
    }
}

impl WsHub {
    /// 构造；`channel_cap` 为每条订阅 broadcast 通道容量。
    pub fn new(channel_cap: usize) -> Self {
        Self {
            inner: std::sync::Arc::new(WsHubInner {
                next_id: AtomicU64::new(1),
                subs: RwLock::new(HashMap::new()),
                by_user: RwLock::new(HashMap::new()),
                channel_cap,
            }),
        }
    }

    /// 订阅；返回订阅 ID 与接收端。
    ///
    /// 命名为 `subscribe_raw` 以避免与 `WebSocketHub::subscribe`（async）签名遮蔽，
    /// 并返回接收端供真实 Axum WS 连接循环读取。
    pub fn subscribe_raw(&self, user: &str) -> (SubscriptionId, broadcast::Receiver<WsMessage>) {
        let id = SubscriptionId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = broadcast::channel(self.inner.channel_cap);
        {
            let mut subs = self.inner.subs.write().expect("ws subs lock poisoned");
            subs.insert(
                id,
                Subscription {
                    user: user.to_string(),
                    tx,
                    last_active: SystemTime::now(),
                },
            );
        }
        {
            let mut by_user = self
                .inner
                .by_user
                .write()
                .expect("ws by_user lock poisoned");
            by_user.entry(user.to_string()).or_default().push(id);
        }
        (id, rx)
    }

    /// 取消订阅。
    ///
    /// 命名为 `unsubscribe_raw` 以避免与 `WebSocketHub::unsubscribe`（async）签名遮蔽。
    pub fn unsubscribe_raw(&self, id: SubscriptionId) {
        if let Some(sub) = {
            let mut subs = self.inner.subs.write().expect("ws subs lock poisoned");
            subs.remove(&id)
        } {
            let mut by_user = self
                .inner
                .by_user
                .write()
                .expect("ws by_user lock poisoned");
            if let Some(ids) = by_user.get_mut(&sub.user) {
                ids.retain(|x| *x != id);
                if ids.is_empty() {
                    by_user.remove(&sub.user);
                }
            }
        }
    }

    /// 全员广播。返回成功推送的订阅数（接收端已断开的不计）。
    ///
    /// 命名为 `broadcast_n` 以避免与 `WebSocketHub::broadcast`（async）签名遮蔽。
    pub fn broadcast_n(&self, msg: WsMessage) -> usize {
        let subs = self.inner.subs.read().expect("ws subs lock poisoned");
        let mut sent = 0usize;
        for sub in subs.values() {
            if sub.tx.send(msg.clone()).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// 定向推送给某用户（其全部订阅）。返回成功推送数。
    ///
    /// 命名为 `send_to_n` 以避免与 `WebSocketHub::send_to`（async）签名遮蔽。
    pub fn send_to_n(&self, user: &str, msg: WsMessage) -> usize {
        let ids: Vec<SubscriptionId> = {
            let by_user = self.inner.by_user.read().expect("ws by_user lock poisoned");
            by_user.get(user).cloned().unwrap_or_default()
        };
        let subs = self.inner.subs.read().expect("ws subs lock poisoned");
        let mut sent = 0usize;
        for id in ids {
            if let Some(sub) = subs.get(&id) {
                if sub.tx.send(msg.clone()).is_ok() {
                    sent += 1;
                }
            }
        }
        sent
    }

    /// 当前活跃订阅数。
    pub fn subscriber_count(&self) -> usize {
        self.inner.subs.read().expect("ws subs lock poisoned").len()
    }

    /// 某用户的订阅数。
    pub fn subscriber_count_for(&self, user: &str) -> usize {
        self.inner
            .by_user
            .read()
            .expect("ws by_user lock poisoned")
            .get(user)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// 刷新订阅活跃时间戳（真实连接读循环——[`crate::http::run_ws`]——收到
    /// 任意客户端帧（含协议层 Ping/Pong）时调用；未知 id 静默忽略）。
    ///
    /// 只计客户端→服务端方向：半开 TCP 下服务端 send 进本地发送缓冲仍
    /// "成功"，出向不构成活性证据，故广播/定向推送路径**不** touch。
    pub fn touch_raw(&self, id: SubscriptionId) {
        let mut subs = self.inner.subs.write().expect("ws subs lock poisoned");
        if let Some(sub) = subs.get_mut(&id) {
            sub.last_active = SystemTime::now();
        }
    }

    /// 某用户在新鲜度窗口 `stale` 内有客户端活动的订阅数（半开/僵死连接
    /// 兜底判定用：订阅存在但 `last_active` 距今 > `stale` 的不计入）。
    ///
    /// 纯判定**不删订阅**——订阅清理仍由连接断开路径 `unsubscribe_raw`
    /// 负责（重连同 user 会复用/清理），过期只影响派生的在线状态。
    pub fn fresh_subscriber_count_for(&self, user: &str, stale: Duration) -> usize {
        let ids: Vec<SubscriptionId> = {
            let by_user = self.inner.by_user.read().expect("ws by_user lock poisoned");
            by_user.get(user).cloned().unwrap_or_default()
        };
        let now = SystemTime::now();
        let subs = self.inner.subs.read().expect("ws subs lock poisoned");
        ids.iter()
            .filter(|id| {
                subs.get(*id).is_some_and(|s| {
                    // 时钟回拨（last_active > now）→ Err → 0 elapsed → 视为新鲜
                    now.duration_since(s.last_active).unwrap_or_default() <= stale
                })
            })
            .count()
    }

    /// 测试专用：把订阅 `last_active` 回拨 `age`（构造"半开僵死订阅"场景，
    /// 配合 [`WsHub::fresh_subscriber_count_for`] 验证新鲜度判定）。
    /// 仅 `#[cfg(test)]` 编译，生产二进制无此 API。
    #[cfg(test)]
    pub(crate) fn backdate_last_active_for_test(&self, id: SubscriptionId, age: Duration) {
        let mut subs = self.inner.subs.write().expect("ws subs lock poisoned");
        if let Some(sub) = subs.get_mut(&id) {
            // 回拨越过 UNIX_EPOCH 不可能（age 是秒级），checked 兜底防呆
            sub.last_active = SystemTime::now()
                .checked_sub(age)
                .unwrap_or(std::time::UNIX_EPOCH);
        }
    }
}

#[async_trait::async_trait]
impl crate::websocket::WebSocketHub for WsHub {
    async fn broadcast(&self, msg: WsMessage) -> Result<(), crate::ApiGatewayError> {
        // 忽略无订阅时的 send 错误（视为正常空投递）
        let _ = self.broadcast_n(msg);
        Ok(())
    }

    async fn send_to(&self, user: &str, msg: WsMessage) -> Result<(), crate::ApiGatewayError> {
        let _ = self.send_to_n(user, msg);
        Ok(())
    }

    async fn subscribe(&self, user: &str) -> Result<SubscriptionId, crate::ApiGatewayError> {
        Ok(self.subscribe_raw(user).0)
    }

    async fn unsubscribe(&self, id: SubscriptionId) {
        self.unsubscribe_raw(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{Event, Topic};

    fn msg() -> WsMessage {
        WsMessage::Event {
            event: Event::new("test", Topic::System, "unit.test"),
        }
    }

    #[tokio::test]
    async fn subscribe_and_broadcast() {
        let hub = WsHub::default();
        let (id, mut rx) = hub.subscribe_raw("alice");
        let _ = id;
        let sent = hub.broadcast_n(msg());
        assert_eq!(sent, 1);
        let received = rx.recv().await.unwrap();
        assert!(matches!(received, WsMessage::Event { .. }));
    }

    #[tokio::test]
    async fn send_to_targeted() {
        let hub = WsHub::default();
        let (_id_a, mut rx_a) = hub.subscribe_raw("alice");
        let (_id_b, mut rx_b) = hub.subscribe_raw("bob");
        // 给 alice 推
        assert_eq!(hub.send_to_n("alice", msg()), 1);
        assert!(rx_a.try_recv().is_ok());
        // bob 不应收到
        assert!(rx_b.try_recv().is_err());
    }

    #[tokio::test]
    async fn unsubscribe_removes() {
        let hub = WsHub::default();
        let (id, _rx) = hub.subscribe_raw("alice");
        assert_eq!(hub.subscriber_count(), 1);
        hub.unsubscribe_raw(id);
        assert_eq!(hub.subscriber_count(), 0);
        assert_eq!(hub.subscriber_count_for("alice"), 0);
    }

    #[tokio::test]
    async fn multi_sub_per_user() {
        let hub = WsHub::default();
        let (_id1, mut rx1) = hub.subscribe_raw("alice");
        let (_id2, mut rx2) = hub.subscribe_raw("alice");
        assert_eq!(hub.subscriber_count_for("alice"), 2);
        assert_eq!(hub.send_to_n("alice", msg()), 2);
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    // —— 订阅新鲜度（touch_raw / fresh_subscriber_count_for）——

    #[tokio::test]
    async fn freshness_window_touch_and_backdate() {
        let hub = WsHub::default();
        let (id, _rx) = hub.subscribe_raw("alice");
        let stale = Duration::from_secs(120);
        // 新订阅 + touch → 新鲜
        assert_eq!(hub.fresh_subscriber_count_for("alice", stale), 1);
        hub.touch_raw(id);
        assert_eq!(hub.fresh_subscriber_count_for("alice", stale), 1);
        // 回拨超过窗口 → 新鲜度判定 0，但订阅本身不删（subscriber_count_for 仍 1）
        hub.backdate_last_active_for_test(id, Duration::from_secs(121));
        assert_eq!(hub.fresh_subscriber_count_for("alice", stale), 0);
        assert_eq!(hub.subscriber_count_for("alice"), 1, "过期不删订阅");
        // touch 复活 → 重新新鲜
        hub.touch_raw(id);
        assert_eq!(hub.fresh_subscriber_count_for("alice", stale), 1);
        // 多订阅只看每条各自的活跃：一条过期一条新鲜 → 计 1
        let (id2, _rx2) = hub.subscribe_raw("alice");
        hub.backdate_last_active_for_test(id2, Duration::from_secs(9999));
        assert_eq!(hub.fresh_subscriber_count_for("alice", stale), 1);
    }

    #[tokio::test]
    async fn broadcast_no_subs_zero() {
        let hub = WsHub::default();
        assert_eq!(hub.broadcast_n(msg()), 0);
    }

    #[tokio::test]
    async fn trait_dispatch_works() {
        let hub = WsHub::default();
        let _ = <WsHub as crate::websocket::WebSocketHub>::subscribe(&hub, "alice")
            .await
            .unwrap();
        assert_eq!(hub.subscriber_count(), 1);
        <WsHub as crate::websocket::WebSocketHub>::broadcast(&hub, msg())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cloned_hub_shares_state() {
        // Clone 后的 WsHub 共享订阅表：A 订阅 → B 广播 → A 收到
        let hub_a = WsHub::default();
        let hub_b = hub_a.clone();
        let (_id, mut rx) = hub_a.subscribe_raw("alice");
        assert_eq!(hub_b.subscriber_count(), 1, "clone 应共享订阅表");
        assert_eq!(hub_b.broadcast_n(msg()), 1);
        assert!(rx.try_recv().is_ok());
    }

    // —— 覆盖率补测：WebSocketHub trait 的 send_to/unsubscribe 路径 ——

    #[tokio::test]
    async fn trait_send_to_delivers_to_user() {
        // 覆盖 WebSocketHub::send_to（async trait 实现返回 Ok）
        let hub = WsHub::default();
        let (_id, mut rx) = hub.subscribe_raw("alice");
        <WsHub as crate::websocket::WebSocketHub>::send_to(&hub, "alice", msg())
            .await
            .unwrap();
        assert!(rx.try_recv().is_ok(), "send_to 应推给 alice");
    }

    #[tokio::test]
    async fn trait_send_to_unknown_user_is_ok() {
        // 给未订阅用户推送 → trait 仍返回 Ok（无订阅视为空投递）
        let hub = WsHub::default();
        let res = <WsHub as crate::websocket::WebSocketHub>::send_to(&hub, "nobody", msg()).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn trait_unsubscribe_via_async_iface() {
        // 覆盖 WebSocketHub::unsubscribe（async trait 实现）
        let hub = WsHub::default();
        let id = <WsHub as crate::websocket::WebSocketHub>::subscribe(&hub, "alice")
            .await
            .unwrap();
        assert_eq!(hub.subscriber_count(), 1);
        <WsHub as crate::websocket::WebSocketHub>::unsubscribe(&hub, id).await;
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn broadcast_to_dropped_receiver_still_ok() {
        // 接收端 drop 后 broadcast_n 计 0，trait broadcast 仍返回 Ok
        let hub = WsHub::default();
        let (id, rx) = hub.subscribe_raw("alice");
        drop(rx);
        // broadcast_n 对已 drop 接收端的 send 失败 → 不计；但订阅表仍有条目
        // 这里测 trait broadcast 不 panic
        let res = <WsHub as crate::websocket::WebSocketHub>::broadcast(&hub, msg()).await;
        assert!(res.is_ok());
        hub.unsubscribe_raw(id);
    }

    /// 真实 axum WebSocket 端到端：启动网关 → 客户端 WS 握手 → 服务端 broadcast →
    /// 客户端收到序列化的 WsMessage。
    ///
    /// 覆盖 [`crate::http::ws_handler`] 真实握手 + [`run_ws`] 转发循环。
    /// WS 握手强制 IM 认证（设计 §2）：`?user=<pubkey>&token=<IM token>`，
    /// 这里注册 im handler + 共享 ImAuth 并直接 issue 一个合法 token。
    #[tokio::test]
    async fn real_ws_endpoint_pushes_messages() {
        use crate::gateway::Gateway;
        use crate::gateway_impl::InProcessGateway;
        use crate::handlers::im::{ImAuth, ImRouteHandler};
        use futures::StreamExt;

        let gw = InProcessGateway::new();
        let auth = std::sync::Arc::new(ImAuth::default());
        gw.register_component(
            "im",
            Box::new(ImRouteHandler::with_empty_ws(gw.ws_hub(), auth.clone())),
        )
        .await
        .expect("注册 im handler");
        gw.set_im_auth(Some(auth.clone()));
        // 取 OS 临时端口
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .expect("start");

        // 客户端 WS 握手：?user=<pubkey>&token=<IM token>（无 token 会被 401 拒绝）
        let pubkey = {
            use k256::elliptic_curve::rand_core::OsRng;
            let sk = k256::ecdsa::SigningKey::random(&mut OsRng);
            format!(
                "0x{}",
                hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
            )
        };
        let (token, _) = auth.issue_token(&pubkey);
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let req = format!(
            "ws://127.0.0.1:{}/ws?user={pubkey}&token={token}",
            addr.port()
        )
        .into_client_request()
        .unwrap();
        let (mut ws_stream, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("WS 握手成功");

        // 服务端广播一条消息
        let pushed = msg();
        // 给握手 + subscribe_raw 一点时间就绪
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(gw.ws_hub().broadcast_n(pushed.clone()), 1, "应有 1 个订阅");

        // 客户端应收到序列化的 WsMessage（axum::extract::ws 把 Text 帧透传）
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next())
            .await
            .expect("WS 收到不应超时")
            .expect("stream 不应结束")
            .expect("帧应无错");
        let text = frame.into_text().expect("应为文本帧");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("应为合法 JSON");
        assert_eq!(parsed["type"], "event", "WsMessage::Event 序列化标签");

        // 关闭连接 → 服务端应取消订阅
        drop(ws_stream);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // 客户端断开后订阅数应回落（run_ws 在 select 退出后 unsubscribe_raw）
        let _ = gw;
        // gw drop 不触发 stop；显式 stop 网关
        // 注：InProcessGateway 内部 Arc 共享，drop 局部不影响 serve task；
        // 这里测试目的已达成（真实 WS 推送），serve 由 process 退出回收。
    }
}
