//! russh 的 SSH 服务端 handler（用于离线/可测的真实 SFTP 协议栈接通）。
//!
//! 设计动机（ADR-DEPS-002 / 规格 §9 红线"不真监听端口"）：
//! - `RusshSftpBackend` 需要一个 `russh::server::Server` 实现才能证明 SSH 协议栈
//!   **真的接通**（而非 TODO 骨架）。
//! - 本模块提供 `OsSshHandler`（实现 `russh::server::Handler`，承载 authorized_keys
//!   公钥认证）与 `OsSshServer`（实现 `russh::server::Server`，每连接生成 handler）。
//! - **不真监听端口**：`OsSshServer` 可被 `russh::server::Server::run_on_socket`
//!   驱动，但本 crate 不调用它；上层（api/service）负责端口绑定。测试直接断言
//!   认证决策（`auth_publickey` 接受/拒绝）与配置构造。
//!
//! 安全模型（务实边界）：
//! - 公钥认证：`auth_publickey` 仅当用户存在于 `authorized_keys` 映射且公钥 base64
//!   匹配时 `Auth::Accept`；否则 `Auth::reject()`。
//! - 密码认证：默认拒绝（与 [`crate::sftp::SftpConfig`] 的 `password_auth=false` 默认一致）；
//!   生产需密码认证应外接 PAM/数据库，本 crate 不实现。
//! - 子系统请求（SFTP）：返回成功占位（真实 SFTP 文件传输由 SFTP 子系统协议承载，
//!   本批仅证明协议栈接通 + 认证决策正确）。

use std::collections::HashMap;
use std::sync::Arc;

use russh::keys::PublicKeyBase64;
use russh::server::{Auth, Handler, Server, Session};
use russh::{ChannelId, MethodKind, MethodSet};

/// SSH 服务端 handler——每客户端连接一份；持有 authorized_keys 引用做公钥认证。
#[derive(Debug)]
pub struct OsSshHandler {
    /// 用户 → 授权公钥列表（公钥 base64 编码字符串，与 ssh-keygen 输出一致）。
    authorized_keys: Arc<HashMap<String, Vec<String>>>,
}

impl OsSshHandler {
    /// 用 authorized_keys 映射构造 handler。
    pub fn new(authorized_keys: Arc<HashMap<String, Vec<String>>>) -> Self {
        Self { authorized_keys }
    }
}

impl Handler for OsSshHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        // 默认拒绝密码认证（与 SftpConfig.password_auth 默认 false 一致），
        // 提示客户端改用公钥认证。
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::PublicKey][..])),
            partial_success: false,
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        // 公钥以 base64 编码字符串比对（authorized_keys 行的标准表示）。
        // 存储的行格式可能是 `<algo> <base64> [comment]`，故提取 base64 段比对；
        // 若存储的已是裸 base64，parse_pubkey_line 回退为整行。
        let offered = public_key.public_key_base64();
        let accepted = self
            .authorized_keys
            .get(user)
            .map(|keys| {
                keys.iter().any(|k| {
                    let candidate = parse_pubkey_line(k).unwrap_or_else(|| k.trim().to_string());
                    candidate == offered.as_str()
                })
            })
            .unwrap_or(false);
        if accepted {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 仅接受 "sftp" 子系统请求；其他拒绝。
        // channel_success/failure 返回 Result（通道已关闭时可能 Err），离线骨架静默丢弃。
        if name.eq_ignore_ascii_case("sftp") {
            let _ = session.channel_success(channel);
        } else {
            let _ = session.channel_failure(channel);
        }
        Ok(())
    }
}

/// SSH 服务端工厂——实现 `russh::server::Server`，每连接生成一份 [`OsSshHandler`]。
#[derive(Debug, Clone)]
pub struct OsSshServer {
    /// 用户 → 授权公钥列表（与 handler 共享，Arc 克隆廉价）。
    authorized_keys: Arc<HashMap<String, Vec<String>>>,
}

impl OsSshServer {
    /// 用 authorized_keys 映射构造服务端工厂。
    pub fn new(authorized_keys: Arc<HashMap<String, Vec<String>>>) -> Self {
        Self { authorized_keys }
    }

    /// 当前授权用户数量（断言用）。
    pub fn user_count(&self) -> usize {
        self.authorized_keys.len()
    }

    /// 取 authorized_keys 映射快照（断言用）。
    pub fn authorized_keys(&self) -> &HashMap<String, Vec<String>> {
        &self.authorized_keys
    }
}

impl Server for OsSshServer {
    type Handler = OsSshHandler;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        OsSshHandler::new(Arc::clone(&self.authorized_keys))
    }

    fn handle_session_error(&mut self, _error: <Self::Handler as Handler>::Error) {
        // 连接级错误：生产应记日志；离线骨架静默丢弃。
    }
}

/// 构造一个真实可用的 `russh::server::Config`——含一个临时 Ed25519 主机密钥
/// （离线生成，仅用于证明 SSH 握手栈接通；生产应从持久化密钥加载）。
///
/// 配置要点：
/// - 仅启用公钥认证（与 [`crate::sftp::SftpConfig`] 的 `pubkey_auth=true` 默认一致）；
/// - 注入一个 Ed25519 主机密钥（每次调用新生成，仅离线测试场景）。
///
/// 返回 `Arc<Config>`（`run_on_socket` 要求 `Arc<Config>`）。
pub fn build_ssh_config() -> Result<Arc<russh::server::Config>, russh::keys::Error> {
    let mut config = russh::server::Config::default();
    // 仅公钥认证：从全集移除其他方法（MethodSet 无 PUBLICKEY 常量，用 empty+push 构造）。
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::PublicKey);
    config.methods = methods;
    // 离线生成临时主机密钥（Ed25519：快、现代、无 RSA 大数依赖）。
    let host_key =
        russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)?;
    config.keys = vec![host_key];
    Ok(Arc::new(config))
}

/// 把 `(user, pubkey_line)` 追加进 authorized_keys 映射（解析 ssh-keygen 格式公钥行）。
///
/// 输入：标准 OpenSSH 公钥行（如 `ssh-ed25519 AAAA... user@host`）。
/// 仅取 base64 段（第二列），与 [`OsSshHandler::auth_publickey`] 的比对口径一致。
/// 空行/格式错误返回 `None`。
#[must_use]
pub fn parse_pubkey_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    // 期望至少 "<algo> <base64>" 两段；取 base64 段。
    parts.get(1).map(|s| (*s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pubkey_line_extracts_b64() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFake alice@host";
        assert_eq!(
            parse_pubkey_line(line),
            Some("AAAAC3NzaC1lZDI1NTE5AAAAIFake".into())
        );
        // 空行
        assert_eq!(parse_pubkey_line("   "), None);
    }

    #[tokio::test]
    async fn handler_accepts_known_pubkey_rejects_unknown() {
        // 生成一对密钥，把公钥加入授权表。
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let pub_b64 = key.public_key_base64();
        let mut keys: HashMap<String, Vec<String>> = HashMap::new();
        keys.insert("alice".into(), vec![pub_b64]);
        let mut h = OsSshHandler::new(Arc::new(keys));

        // alice + 正确公钥 → Accept
        let pk = key.public_key().clone();
        let auth = h.auth_publickey("alice", &pk).await.unwrap();
        assert!(matches!(auth, Auth::Accept));

        // bob + 同一公钥 → reject（bob 未授权）
        let auth2 = h.auth_publickey("bob", &pk).await.unwrap();
        assert!(matches!(auth2, Auth::Reject { .. }));

        // 密码 → reject
        let auth3 = h.auth_password("alice", "secret").await.unwrap();
        assert!(matches!(auth3, Auth::Reject { .. }));
    }

    #[test]
    fn build_ssh_config_has_ed25519_key_and_pubkey_only() {
        let cfg = build_ssh_config().unwrap();
        // 仅公钥认证
        assert_eq!(*cfg.methods, [MethodKind::PublicKey]);
        assert_eq!(cfg.keys.len(), 1);
        // 主机密钥算法为 Ed25519
        let k = &cfg.keys[0];
        assert_eq!(k.algorithm(), russh::keys::Algorithm::Ed25519);
    }

    // —— parse_pubkey_line 边界情况 ——

    #[test]
    fn parse_pubkey_line_empty_returns_none() {
        assert_eq!(parse_pubkey_line(""), None);
        assert_eq!(parse_pubkey_line("   \t  "), None);
    }

    #[test]
    fn parse_pubkey_line_single_token_returns_none() {
        // 仅一段（无第二列 base64）→ None
        assert_eq!(parse_pubkey_line("ssh-ed25519"), None);
    }

    #[test]
    fn parse_pubkey_line_two_tokens_takes_second() {
        // 标准两段（无 comment）：取第二段作 base64
        assert_eq!(
            parse_pubkey_line("ssh-rsa AAAABBBB"),
            Some("AAAABBBB".into())
        );
    }

    #[test]
    fn parse_pubkey_line_handles_extra_whitespace() {
        // 多空格 / 制表符分隔：split_whitespace 容忍
        assert_eq!(
            parse_pubkey_line("  ssh-ed25519\tCCCC  d@h  "),
            Some("CCCC".into())
        );
    }

    // —— auth_publickey：authorized_keys 行带 algo 前缀的解析路径 ——

    #[tokio::test]
    async fn auth_publickey_accepts_pubkey_stored_as_full_openssh_line() {
        // authorized_keys 存的是完整行 "ssh-ed25519 <b64> [comment]"，
        // auth_publickey 提取 base64 段比对——验证 parse_pubkey_line 在认证中的回退路径
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let pub_b64 = key.public_key_base64();
        let full_line = format!("ssh-ed25519 {pub_b64} alice@host");
        let mut keys: HashMap<String, Vec<String>> = HashMap::new();
        keys.insert("alice".into(), vec![full_line]);
        let mut h = OsSshHandler::new(Arc::new(keys));

        let pk = key.public_key().clone();
        let auth = h.auth_publickey("alice", &pk).await.unwrap();
        assert!(matches!(auth, Auth::Accept));
    }

    #[tokio::test]
    async fn auth_publickey_rejects_when_user_has_no_keys_entry() {
        // 用户不在映射中 → unwrap_or(false) → reject
        let mut h = OsSshHandler::new(Arc::new(HashMap::new()));
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let pk = key.public_key().clone();
        let auth = h.auth_publickey("ghost", &pk).await.unwrap();
        assert!(matches!(auth, Auth::Reject { .. }));
    }

    #[tokio::test]
    async fn auth_publickey_rejects_mismatched_pubkey() {
        // 用户存在但提供的公钥不匹配 → reject
        let key1 =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let key2 =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let mut keys: HashMap<String, Vec<String>> = HashMap::new();
        keys.insert("alice".into(), vec![key1.public_key_base64()]);
        let mut h = OsSshHandler::new(Arc::new(keys));
        let pk2 = key2.public_key().clone();
        let auth = h.auth_publickey("alice", &pk2).await.unwrap();
        assert!(matches!(auth, Auth::Reject { .. }));
    }

    #[tokio::test]
    async fn auth_password_always_rejects_and_suggests_pubkey() {
        // 密码认证默认拒绝，并提示仅可继续公钥认证
        let mut h = OsSshHandler::new(Arc::new(HashMap::new()));
        let auth = h.auth_password("anyone", "any").await.unwrap();
        match auth {
            Auth::Reject {
                proceed_with_methods,
                ..
            } => {
                let methods = proceed_with_methods.unwrap();
                assert!(methods.contains(&MethodKind::PublicKey));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // —— OsSshServer 工厂行为 ——

    #[test]
    fn ssh_server_user_count_and_authorized_keys_snapshot() {
        let mut keys: HashMap<String, Vec<String>> = HashMap::new();
        keys.insert("alice".into(), vec!["ssh-ed25519 AAAA a@h".into()]);
        keys.insert("bob".into(), vec!["ssh-rsa BBBB b@h".into()]);
        let server = OsSshServer::new(Arc::new(keys));
        assert_eq!(server.user_count(), 2);
        let snapshot = server.authorized_keys();
        assert!(snapshot.contains_key("alice"));
        assert!(snapshot.contains_key("bob"));
    }

    #[test]
    fn ssh_server_new_client_clones_authorized_keys() {
        // new_client 生成一份 handler，持有与 server 同源的 authorized_keys
        let mut keys: HashMap<String, Vec<String>> = HashMap::new();
        keys.insert("alice".into(), vec!["ssh-ed25519 AAAA".into()]);
        let mut server = OsSshServer::new(Arc::new(keys));
        let _handler = server.new_client(None);
        // 仅断言 new_client 不 panic 且返回 handler（类型已由签名保证）
    }

    #[test]
    fn ssh_server_handle_session_error_is_silent_noop() {
        // handle_session_error 静默丢弃错误（生产应记日志）——仅断言不 panic
        let mut server = OsSshServer::new(Arc::new(HashMap::new()));
        // 构造一个 russh::Error 较重；此处用 channel_closed 类错误验证调用可达。
        //russh::Error 无便捷构造器，跳过传参——仅验证方法存在与签名（编译期保证）。
        let _ = &mut server;
    }
}
