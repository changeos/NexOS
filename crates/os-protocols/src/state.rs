//! 共享生命周期状态机 + 内存共享/会话存储
//!
//! 定位（规划文档 §3.3 / 规格 §3）：本模块提供协议无关的共享生命周期模型与
//! 纯内存状态存储，供各协议编排器（`SambaOrchestrator` / `DavServerBackend` 等）
//! 复用。真实协议栈调用（写盘 / reload / 服务起停）在各编排器里补，
//! 这里只管"共享/会话的逻辑视图"。
//!
//! 设计要点：
//! - `ShareState` 是共享生命周期状态机：`Creating → Active → Stopping → Stopped`，
//!   非法迁移返回 `Err`（便于编排器在并发/重入场景下做正确处理）。
//! - `ShareStore` 是 `Send + Sync` 的内存存储（`Mutex<HashMap>`），承载共享与活跃会话。
//!   各编排器持有一份，把 `FileProtocol` 父 trait 的 7 个生命周期/会话方法落到此存储。
//! - 无外部 IO、无 panic（锁中毒除外），便于在 mock 与真实编排器中确定性复用。

use std::collections::HashMap;
use std::sync::Mutex;

use os_core::ShareId;

use crate::common::{Session, Share, ShareOptions};
use crate::error::{ProtocolError, ProtocolResult};

// ----------------------------------------------------------------------------
// 共享状态机
// ----------------------------------------------------------------------------

/// 共享生命周期状态。
///
/// 状态迁移（合法路径）：
/// ```text
///   Creating ──▶ Active ──▶ Stopping ──▶ Stopped
///      │           ▲           │
///      └───────────┘           └──(终态，删除后移除)
/// ```
/// `Creating` 是瞬时态（编排器落盘配置的窗口期）；正常稳态为 `Active`；
/// 卸载流程进入 `Stopping`（停止协议服务/踢会话），完成后 `Stopped`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareState {
    /// 创建中（落盘配置未完成，尚未 reload）
    Creating,
    /// 活跃（已对外暴露，可接受客户端连接）
    Active,
    /// 停止中（正在 reload 移除 / 踢会话）
    Stopping,
    /// 已停止（配置已移除，等待清理）
    Stopped,
}

impl ShareState {
    /// 判定从 `self` 迁移到 `next` 是否合法。
    ///
    /// 合法迁移：
    /// - Creating → Active / Stopping / Stopped
    /// - Active → Stopping / Stopped
    /// - Stopping → Stopped
    /// - 同态自迁移（幂等）一律合法
    #[must_use]
    pub fn can_transition(self, next: ShareState) -> bool {
        if self == next {
            return true;
        }
        matches!(
            (self, next),
            (ShareState::Creating, ShareState::Active)
                | (ShareState::Creating, ShareState::Stopping)
                | (ShareState::Creating, ShareState::Stopped)
                | (ShareState::Active, ShareState::Stopping)
                | (ShareState::Active, ShareState::Stopped)
                | (ShareState::Stopping, ShareState::Stopped)
        )
    }
}

impl std::fmt::Display for ShareState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShareState::Creating => f.write_str("creating"),
            ShareState::Active => f.write_str("active"),
            ShareState::Stopping => f.write_str("stopping"),
            ShareState::Stopped => f.write_str("stopped"),
        }
    }
}

// ----------------------------------------------------------------------------
// 内存共享/会话存储
// ----------------------------------------------------------------------------

/// 协议无关的内存共享/会话存储。
///
/// 各协议编排器持有一份 `ShareStore`，在其中维护"本协议对外暴露的共享"
/// 与"当前活跃会话"。`FileProtocol` 父 trait 的 7 个方法（create/update/
/// delete/list/get/list_sessions/close_session）可直接落到这里；
/// 协议特有副作用（写 smb.conf / reload smbd 等）由编排器在调用前后补。
///
/// 线程安全：内部 `Mutex`；锁中毒视作内部错误。
pub struct ShareStore {
    inner: Mutex<StoreState>,
}

#[derive(Default)]
struct StoreState {
    /// 共享按 ID 索引（ID 字符串形式作 key）
    shares: HashMap<String, Share>,
    /// 共享对应的运行态（默认 `Active`，创建后立即对外）
    states: HashMap<String, ShareState>,
    /// 活跃会话按 ID 索引
    sessions: HashMap<String, Session>,
}

impl std::fmt::Debug for ShareStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.inner.lock().expect("store poisoned");
        f.debug_struct("ShareStore")
            .field("shares", &st.shares.len())
            .field("sessions", &st.sessions.len())
            .finish()
    }
}

impl Default for ShareStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ShareStore {
    /// 构造空存储。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StoreState::default()),
        }
    }

    /// 当前共享数量（断言/可观测用）。
    pub fn share_count(&self) -> usize {
        self.inner.lock().expect("store poisoned").shares.len()
    }

    /// 当前活跃会话数量。
    pub fn session_count(&self) -> usize {
        self.inner.lock().expect("store poisoned").sessions.len()
    }

    // —— 共享生命周期 ——

    /// 插入一个新共享（创建时调用）。若 ID 已存在返回 `ShareExists`。
    /// 新共享默认状态为 `Active`（编排器在落盘配置前后可显式置 `Creating`）。
    pub fn put_share(&self, share: Share) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("store poisoned");
        let key = share.id.as_str().to_string();
        if st.shares.contains_key(&key) {
            return Err(ProtocolError::ShareExists(key));
        }
        st.states.insert(key.clone(), ShareState::Active);
        st.shares.insert(key, share);
        Ok(())
    }

    /// 读取单个共享。
    pub fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        let st = self.inner.lock().expect("store poisoned");
        st.shares
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| ProtocolError::ShareNotFound(id.as_str().to_string()))
    }

    /// 列出所有共享（快照）。
    pub fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        let st = self.inner.lock().expect("store poisoned");
        let mut all: Vec<Share> = st.shares.values().cloned().collect();
        // 稳定排序，便于测试断言（不依赖 HashMap 迭代序）
        all.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(all)
    }

    /// 应用更新（合并 `ShareOptions` 中协议无关字段到共享的语义由编排器决定；
    /// 此处仅提供对共享本身的就地替换入口）。
    pub fn replace_share(&self, share: Share) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("store poisoned");
        let key = share.id.as_str().to_string();
        if !st.shares.contains_key(&key) {
            return Err(ProtocolError::ShareNotFound(key));
        }
        st.shares.insert(key, share);
        Ok(())
    }

    /// 移除共享及其状态与会话（关联会话一并清理）。
    pub fn remove_share(&self, id: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("store poisoned");
        let key = id.as_str().to_string();
        if st.shares.remove(&key).is_none() {
            return Err(ProtocolError::ShareNotFound(key));
        }
        st.states.remove(&key);
        // 级联清理该共享下的会话
        st.sessions.retain(|_, s| &s.share_id != id);
        Ok(())
    }

    /// 设置共享的运行态（带状态机校验；非法迁移返回 `Internal`）。
    pub fn set_state(&self, id: &ShareId, next: ShareState) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("store poisoned");
        let key = id.as_str().to_string();
        let cur = st
            .states
            .get(&key)
            .copied()
            .ok_or_else(|| ProtocolError::ShareNotFound(key.clone()))?;
        if !cur.can_transition(next) {
            return Err(ProtocolError::Internal(format!(
                "非法状态迁移：{cur} → {next}（share={key}）"
            )));
        }
        st.states.insert(key, next);
        Ok(())
    }

    /// 查询共享运行态。
    pub fn state_of(&self, id: &ShareId) -> ProtocolResult<ShareState> {
        let st = self.inner.lock().expect("store poisoned");
        st.states
            .get(id.as_str())
            .copied()
            .ok_or_else(|| ProtocolError::ShareNotFound(id.as_str().to_string()))
    }

    // —— 会话管理 ——

    /// 记录一个活跃会话（同 ID 视作更新）。
    pub fn put_session(&self, session: Session) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("store poisoned");
        st.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    /// 列出所有活跃会话。
    pub fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        let st = self.inner.lock().expect("store poisoned");
        let mut all: Vec<Session> = st.sessions.values().cloned().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(all)
    }

    /// 关闭并移除一个会话。不存在返回 `SessionNotFound`。
    pub fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("store poisoned");
        if st.sessions.remove(session_id).is_none() {
            return Err(ProtocolError::SessionNotFound(session_id.to_string()));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// ShareOptions 应用辅助：把协议无关选项语义化
// ----------------------------------------------------------------------------

/// 把 `ShareOptions` 的语义应用到共享上（返回新 Share，不改原值）。
///
/// 语义（协议无关）：
/// - `comment`：写共享的备注（暂存到 `ShareOptions`，不进 `Share` 字段；
///   `Share` 当前无 comment 字段，故此处为 no-op 占位，便于编排器未来扩展）。
/// - `guest_ok` / `browseable` / `valid_users`：协议特有，由编排器在渲染配置时消费。
///
/// 当前 `Share` 没有 comment/选项字段，本函数仅保证 `options` 被消费（避免 unused 警告）
/// 并返回共享原值的克隆。编排器可基于返回值继续做协议特有处理。
#[must_use]
pub fn apply_options(share: &Share, _options: &ShareOptions) -> Share {
    // 占位：`Share` 当前无选项承载字段；保留入口以便未来扩展（如加 comment 列）。
    // 真正的协议特有渲染（valid users → smb.conf / hosts allow → exports）在配置生成器里做。
    share.clone()
}

// ----------------------------------------------------------------------------
// 单元测试（状态机 + 存储）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Protocol, Share, ShareOptions};
    use chrono::Utc;
    use std::path::PathBuf;

    fn sample_share(id: &str, name: &str) -> Share {
        Share {
            id: ShareId::new(id),
            name: name.into(),
            protocol: Protocol::Smb,
            path: PathBuf::from("/tank/media"),
            read_only: false,
            hosts_allow: vec![],
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn state_machine_legal_transitions() {
        // 合法迁移
        assert!(ShareState::Creating.can_transition(ShareState::Active));
        assert!(ShareState::Creating.can_transition(ShareState::Stopping));
        assert!(ShareState::Active.can_transition(ShareState::Stopping));
        assert!(ShareState::Active.can_transition(ShareState::Stopped));
        assert!(ShareState::Stopping.can_transition(ShareState::Stopped));
        // 幂等自迁移
        assert!(ShareState::Active.can_transition(ShareState::Active));
    }

    #[test]
    fn state_machine_illegal_transitions() {
        // 非法：不可回退到 Creating；不可从 Stopped 再迁移
        assert!(!ShareState::Active.can_transition(ShareState::Creating));
        assert!(!ShareState::Stopped.can_transition(ShareState::Active));
        assert!(!ShareState::Stopped.can_transition(ShareState::Stopping));
        assert!(!ShareState::Stopping.can_transition(ShareState::Active));
    }

    #[test]
    fn state_display() {
        assert_eq!(ShareState::Creating.to_string(), "creating");
        assert_eq!(ShareState::Active.to_string(), "active");
        assert_eq!(ShareState::Stopping.to_string(), "stopping");
        assert_eq!(ShareState::Stopped.to_string(), "stopped");
    }

    #[test]
    fn store_share_lifecycle() {
        let store = ShareStore::new();
        // 初始空
        assert_eq!(store.share_count(), 0);
        assert!(store.list_shares().unwrap().is_empty());

        // 插入
        store.put_share(sample_share("s1", "media")).unwrap();
        assert_eq!(store.share_count(), 1);
        // 重复插入报 ShareExists
        assert!(matches!(
            store.put_share(sample_share("s1", "media")).unwrap_err(),
            ProtocolError::ShareExists(_)
        ));

        // 读取
        let got = store.get_share(&ShareId::new("s1")).unwrap();
        assert_eq!(got.name, "media");

        // 不存在
        assert!(matches!(
            store.get_share(&ShareId::new("nope")).unwrap_err(),
            ProtocolError::ShareNotFound(_)
        ));

        // 替换
        let mut updated = sample_share("s1", "media2");
        updated.read_only = true;
        store.replace_share(updated).unwrap();
        assert_eq!(store.get_share(&ShareId::new("s1")).unwrap().name, "media2");

        // 移除
        store.remove_share(&ShareId::new("s1")).unwrap();
        assert_eq!(store.share_count(), 0);
        // 二次移除报 NotFound
        assert!(matches!(
            store.remove_share(&ShareId::new("s1")).unwrap_err(),
            ProtocolError::ShareNotFound(_)
        ));
    }

    #[test]
    fn store_state_machine_transitions() {
        let store = ShareStore::new();
        store.put_share(sample_share("s1", "media")).unwrap();
        // 默认 Active
        assert_eq!(
            store.state_of(&ShareId::new("s1")).unwrap(),
            ShareState::Active
        );
        // Active → Stopping → Stopped
        store
            .set_state(&ShareId::new("s1"), ShareState::Stopping)
            .unwrap();
        store
            .set_state(&ShareId::new("s1"), ShareState::Stopped)
            .unwrap();
        // 非法：Stopped → Active
        let err = store
            .set_state(&ShareId::new("s1"), ShareState::Active)
            .unwrap_err();
        assert!(matches!(err, ProtocolError::Internal(_)));
    }

    #[test]
    fn store_sessions_and_cascade() {
        let store = ShareStore::new();
        store.put_share(sample_share("s1", "media")).unwrap();
        // 会话
        let sess = Session {
            id: "S-1".into(),
            protocol: Protocol::Smb,
            user: "alice".into(),
            client_ip: "10.0.0.2".into(),
            connected_at: Utc::now(),
            share_id: ShareId::new("s1"),
        };
        store.put_session(sess).unwrap();
        assert_eq!(store.session_count(), 1);
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        // 关闭
        store.close_session("S-1").unwrap();
        assert_eq!(store.session_count(), 0);
        assert!(matches!(
            store.close_session("S-1").unwrap_err(),
            ProtocolError::SessionNotFound(_)
        ));
    }

    #[test]
    fn store_remove_share_cascades_sessions() {
        let store = ShareStore::new();
        store.put_share(sample_share("s1", "media")).unwrap();
        let sess = Session {
            id: "S-1".into(),
            protocol: Protocol::Smb,
            user: "alice".into(),
            client_ip: "10.0.0.2".into(),
            connected_at: Utc::now(),
            share_id: ShareId::new("s1"),
        };
        store.put_session(sess).unwrap();
        // 删共享应级联删会话
        store.remove_share(&ShareId::new("s1")).unwrap();
        assert_eq!(store.session_count(), 0);
    }

    #[test]
    fn apply_options_is_identity_for_now() {
        let s = sample_share("s1", "media");
        let out = apply_options(&s, &ShareOptions::default());
        assert_eq!(out.id, s.id);
        assert_eq!(out.name, s.name);
    }
}
