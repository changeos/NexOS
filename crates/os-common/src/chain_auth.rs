//! 链上身份认证内核——挑战-签名（challenge→verify→token）的共享实现。
//!
//! 从 os-api `handlers/im.rs` 的 `ImAuth` 泛化而来（设计
//! `docs/MEDIA_GEN_AND_CHAIN_AUTH.md` §C / `docs/IM_BLOCKCHAIN_AUTH_DESIGN.md`）：
//! 身份 = secp256k1 公钥（压缩格式 `0x` + 66 hex），权限 = 私钥持有者；
//! IM 与 NexHub 各挂**独立实例**（token 桶互不相通），契约完全同款：
//!
//! 1. `POST <prefix>/auth/challenge {pubkey}` → `{nonce}`（60s 单次有效）
//! 2. 客户端用私钥对 nonce 的 UTF-8 字节做 ECDSA 签名（65 字节 `r||s||v` hex）
//! 3. `POST <prefix>/auth/verify {pubkey, nonce, signature}` → `{token}`（24h，
//!    单点登录——同 pubkey 新 verify 顶掉旧 token）
//! 4. 业务端点 `Authorization: Bearer <token>`，服务端反查 pubkey 归因
//!    （body/query 自报身份一律忽略）。
//!
//! # 组件
//!
//! - [`ChainAuth`]：nonce 桶（pubkey→nonce，60s TTL 单次）+ token 桶
//!   （token→(pubkey, 过期)，pubkey→token 反查索引），内存态 + Mutex
//!   （重启失效可接受：客户端 401 后自动重走挑战-签名）。
//! - [`parse_pubkey`]：压缩公钥格式校验（`k256::VerifyingKey::from_sec1`）。
//! - [`derive_display_name`]：EVM 地址展示名派生
//!   （`keccak256(未压缩公钥[1..])[12..]`）。
//! - [`verify_nonce_signature`]：65 字节 `r||s||v` ECDSA 验签。
//! - [`bearer_token`]：从请求头解析 `Authorization: Bearer <token>`。
//!
//! # 依赖说明
//!
//! k256 / tiny-keccak / hex 已是 workspace 根注册项（os-api blockchain 钱包
//! 同栈）；随机数经 `k256::elliptic_curve::rand_core::OsRng`（无需独立 rand crate，
//! 与原 im.rs 实现一致）。

use std::collections::HashMap;
use std::sync::Mutex;

/// nonce 有效期（秒）：challenge 签发后 60s 内须完成 verify。
pub const NONCE_TTL_SECS: i64 = 60;
/// token 有效期（秒）：24h（单点登录——同 pubkey 新 verify 顶掉旧 token）。
pub const TOKEN_TTL_SECS: i64 = 86_400;

/// 链上身份认证存储——nonce 桶 + token 桶（内存态）。
///
/// 每个 handler（IM / NexHub）挂独立实例：nonce/token 桶互不相通，IM 的 token
/// 在 NexHub 不可用（反之亦然），但客户端可用**同一密钥对**分别在两侧完成
/// 挑战-签名（设计 §C「useImIdentity 泛化为 useChainIdentity」的服务端前提）。
/// 与 handler 共享（`Arc`）；IM 侧还共享给 WS 握手层（os-api http.rs）。
#[derive(Default)]
pub struct ChainAuth {
    inner: Mutex<AuthInner>,
}

#[derive(Default)]
struct AuthInner {
    /// 待验证 nonce：pubkey → (nonce, 签发时间 unix 秒)。新 challenge 覆盖旧值。
    nonces: HashMap<String, PendingNonce>,
    /// 有效 token：token → (pubkey, 过期时间 unix 秒)。
    tokens: HashMap<String, TokenRecord>,
    /// pubkey → 当前 token（单点登录：issue 时顶掉旧 token）。
    token_by_pubkey: HashMap<String, String>,
}

struct PendingNonce {
    nonce: String,
    issued_at: i64,
}

struct TokenRecord {
    pubkey: String,
    expires_at: i64,
}

/// 校验链上身份（=用户名）格式并解析公钥：`0x` + 66 hex（33 字节压缩 secp256k1，
/// `k256::VerifyingKey::from_sec1` 必须解析成功）。
pub fn parse_pubkey(s: &str) -> Option<k256::ecdsa::VerifyingKey> {
    let hex_part = s.strip_prefix("0x")?;
    if hex_part.len() != 66 {
        return None;
    }
    let bytes = hex::decode(hex_part).ok()?;
    k256::ecdsa::VerifyingKey::from_sec1_bytes(&bytes).ok()
}

/// 展示名派生（纯函数）：EVM 地址 `0x` + 40 hex =
/// keccak256(未压缩公钥[1..])[12..]（与 os-api blockchain 钱包同规则；
/// os-wallet 的派生走 alloy 栈，跨 crate 复用会引入重依赖，故本地实现）。
#[must_use]
pub fn derive_display_name(vk: &k256::ecdsa::VerifyingKey) -> String {
    use tiny_keccak::{Hasher, Keccak};
    let uncompressed = vk.to_encoded_point(false); // 0x04 || X || Y（65 字节）
    let mut hasher = Keccak::v256();
    hasher.update(&uncompressed.as_bytes()[1..]);
    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash);
    format!("0x{}", hex::encode(&hash[12..]))
}

/// ECDSA 验签（纯函数）：签名 = 65 字节 `r||s||v`（v 为恢复位，校验时忽略），
/// 对 nonce 的 UTF-8 字节签（ecdsa crate 的 `verify` 内部做 SHA-256 摘要，
/// 与前端 @noble/secp256k1 `sign(sha256(nonce))` 逐字节兼容）。
#[must_use]
pub fn verify_nonce_signature(vk: &k256::ecdsa::VerifyingKey, nonce: &str, sig65: &[u8]) -> bool {
    use k256::ecdsa::signature::Verifier;
    if sig65.len() != 65 {
        return false;
    }
    let Ok(sig) = k256::ecdsa::Signature::from_slice(&sig65[..64]) else {
        return false;
    };
    vk.verify(nonce.as_bytes(), &sig).is_ok()
}

/// 从请求头解析 `Authorization: Bearer <token>`（大小写宽容；`headers` 是网关
/// 契约的开放结构 JSON 对象）。缺失/空值 → None。
pub fn bearer_token(headers: &serde_json::Value) -> Option<&str> {
    let header = headers
        .get("authorization")
        .and_then(|v| v.as_str())
        .or_else(|| headers.get("Authorization").and_then(|v| v.as_str()))?;
    header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .filter(|t| !t.is_empty())
}

/// 生成 256-bit 随机 hex（nonce / token 共用；CSPRNG）。
fn random_hex32() -> String {
    use k256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

impl ChainAuth {
    /// 空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 签发挑战 nonce（覆盖该 pubkey 的旧 nonce；调用方须已校验 pubkey 格式）。
    pub fn create_nonce(&self, pubkey: &str) -> String {
        let nonce = random_hex32();
        self.inner
            .lock()
            .expect("chain auth poisoned")
            .nonces
            .insert(
                pubkey.to_string(),
                PendingNonce {
                    nonce: nonce.clone(),
                    issued_at: now_secs(),
                },
            );
        nonce
    }

    /// 消费 nonce（匹配即焚：验证签名失败同样烧掉——防暴力尝试；
    /// 不匹配/过期则不动桶——错误猜测不应烧掉真 nonce，否则可恶意
    /// DoS 他人挑战流）。匹配且未过 60s TTL → true。
    pub fn take_nonce(&self, pubkey: &str, nonce: &str) -> bool {
        let mut inner = self.inner.lock().expect("chain auth poisoned");
        let Some(pending) = inner.nonces.get(pubkey) else {
            return false;
        };
        if pending.nonce != nonce || now_secs() - pending.issued_at > NONCE_TTL_SECS {
            return false;
        }
        inner.nonces.remove(pubkey);
        true
    }

    /// 签发 token（256-bit hex，24h）——单点登录：同 pubkey 旧 token 立即失效。
    /// 返回 `(token, expires_in 秒)`。
    pub fn issue_token(&self, pubkey: &str) -> (String, i64) {
        let token = random_hex32();
        let mut inner = self.inner.lock().expect("chain auth poisoned");
        if let Some(old) = inner
            .token_by_pubkey
            .insert(pubkey.to_string(), token.clone())
        {
            inner.tokens.remove(&old);
        }
        inner.tokens.insert(
            token.clone(),
            TokenRecord {
                pubkey: pubkey.to_string(),
                expires_at: now_secs() + TOKEN_TTL_SECS,
            },
        );
        (token, TOKEN_TTL_SECS)
    }

    /// 校验 Bearer token：有效且未过期 → 所属 pubkey（过期惰性清除）。
    pub fn verify_token(&self, token: &str) -> Option<String> {
        let mut inner = self.inner.lock().expect("chain auth poisoned");
        if let Some(rec) = inner.tokens.get(token) {
            if rec.expires_at > now_secs() {
                return Some(rec.pubkey.clone());
            }
            inner.tokens.remove(token);
        }
        None
    }

    /// WS 握手校验（IM 侧使用）：`?user=<pubkey>` 必须与 token 反查的 pubkey
    /// 一致且未过期。
    pub fn verify_ws(&self, user: &str, token: &str) -> bool {
        self.verify_token(token).is_some_and(|pk| pk == user)
    }

    /// 测试钩子：把 token 置为立即过期（验证 401 过期路径）。
    ///
    /// 消费方（os-api im.rs 等）的集成测试跨 crate 调用，故 pub +
    /// `#[doc(hidden)]`——不属于对外稳定契约。
    #[doc(hidden)]
    pub fn expire_token_for_test(&self, token: &str) {
        if let Some(rec) = self
            .inner
            .lock()
            .expect("chain auth poisoned")
            .tokens
            .get_mut(token)
        {
            rec.expires_at = now_secs() - 1;
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测——桶语义 + 密钥学纯函数（随实现从 os-api im.rs 迁移/泛化）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成真 secp256k1 密钥对（CSPRNG，与消费方测试同栈）。
    fn new_key() -> k256::ecdsa::SigningKey {
        use k256::elliptic_curve::rand_core::OsRng;
        k256::ecdsa::SigningKey::random(&mut OsRng)
    }

    /// 私钥 → 用户名（0x + 66 hex 压缩公钥）。
    fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    /// 客户端签名：SHA-256(nonce UTF-8) → RFC6979 ECDSA（65 字节 r||s||v）。
    fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> [u8; 65] {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        out
    }

    // 1. 桶语义：nonce 覆盖 / 单次使用 / token 反查 / 单点登录 / WS 匹配
    #[test]
    fn bucket_semantics() {
        let auth = ChainAuth::default();
        let pk = pubkey_hex(&new_key());
        // nonce 新 challenge 覆盖旧值
        let n1 = auth.create_nonce(&pk);
        let n2 = auth.create_nonce(&pk);
        assert_ne!(n1, n2);
        assert!(!auth.take_nonce(&pk, &n1), "旧 nonce 已被覆盖");
        assert!(auth.take_nonce(&pk, &n2), "最新 nonce 可用");
        assert!(!auth.take_nonce(&pk, &n2), "nonce 单次使用");
        // 错误猜测不烧真 nonce
        let n3 = auth.create_nonce(&pk);
        assert!(!auth.take_nonce(&pk, "deadbeef"), "不匹配应 false");
        assert!(auth.take_nonce(&pk, &n3), "不匹配的猜测不应烧掉真 nonce");
        // token 反查 + WS 匹配
        let (token, expires_in) = auth.issue_token(&pk);
        assert_eq!(expires_in, TOKEN_TTL_SECS);
        assert_eq!(auth.verify_token(&token), Some(pk.clone()));
        assert_eq!(auth.verify_token("bogus"), None);
        assert!(auth.verify_ws(&pk, &token), "user 与 token 匹配");
        assert!(
            !auth.verify_ws(&pubkey_hex(&new_key()), &token),
            "user 不匹配应拒绝"
        );
        // 单点登录：新 token 顶掉旧 token
        let (token2, _) = auth.issue_token(&pk);
        assert_ne!(token, token2);
        assert_eq!(auth.verify_token(&token), None, "旧 token 应被顶掉");
        assert_eq!(auth.verify_token(&token2), Some(pk.clone()));
    }

    // 2. 过期 token → None（expire_token_for_test 钩子）
    #[test]
    fn expired_token_rejected() {
        let auth = ChainAuth::new();
        let pk = pubkey_hex(&new_key());
        let (token, _) = auth.issue_token(&pk);
        assert!(auth.verify_token(&token).is_some());
        auth.expire_token_for_test(&token);
        assert_eq!(auth.verify_token(&token), None, "过期 token 应被拒");
    }

    // 3. 展示名派生：公开测试向量（私钥 1 = 生成元 ↔ 著名 EVM 地址常量）
    #[test]
    fn display_name_known_vector() {
        let vk =
            parse_pubkey("0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .expect("生成元公钥应可解析");
        assert_eq!(
            derive_display_name(&vk),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
        // 随机密钥往返：parse(derive 用的公钥) 恒成功、两次派生一致
        let sk = new_key();
        let vk2 = parse_pubkey(&pubkey_hex(&sk)).unwrap();
        assert_eq!(derive_display_name(&vk2), derive_display_name(&vk2));
    }

    // 4. pubkey 格式校验：缺 0x / 长度错 / 非 hex / 非法 sec1 点
    #[test]
    fn parse_pubkey_rejects_invalid() {
        let valid = pubkey_hex(&new_key());
        assert!(parse_pubkey(&valid).is_some());
        for bad in [
            valid[2..].to_string(),             // 缺 0x 前缀
            format!("0x{}", &valid[2..66]),     // 长度不足（64 hex）
            format!("0x{}zz", &valid[2..66]),   // 非 hex 字符
            format!("0x04{}", "ab".repeat(32)), // 0x04 未压缩标签 + 33 字节 → sec1 失败
            "0x".to_string(),
            String::new(),
        ] {
            assert!(parse_pubkey(&bad).is_none(), "应拒绝: {bad}");
        }
    }

    // 5. 验签：真签名通过 / 伪造签名拒绝 / 格式非法拒绝
    #[test]
    fn verify_nonce_signature_all_paths() {
        let sk = new_key();
        let attacker = new_key();
        let vk = sk.verifying_key();
        let nonce = "a".repeat(64);
        assert!(verify_nonce_signature(vk, &nonce, &sign_nonce(&sk, &nonce)));
        assert!(!verify_nonce_signature(
            vk,
            &nonce,
            &sign_nonce(&attacker, &nonce)
        ));
        assert!(
            !verify_nonce_signature(vk, &nonce, &[0u8; 64]),
            "非 65 字节"
        );
        assert!(
            !verify_nonce_signature(vk, &nonce, &[0u8; 65]),
            "全零 r||s 非法"
        );
    }

    // 6. bearer_token 头解析：标准 / 小写 / 缺失 / 空值
    #[test]
    fn bearer_token_parses_headers() {
        assert_eq!(
            bearer_token(&serde_json::json!({"authorization": "Bearer abc"})),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&serde_json::json!({"Authorization": "Bearer abc"})),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&serde_json::json!({"authorization": "bearer abc"})),
            Some("abc")
        );
        assert_eq!(bearer_token(&serde_json::json!({})), None);
        assert_eq!(
            bearer_token(&serde_json::json!({"authorization": "Bearer "})),
            None,
            "空 token 应 None"
        );
        assert_eq!(
            bearer_token(&serde_json::json!({"authorization": "Basic abc"})),
            None
        );
    }

    // 7. 端到端（challenge → sign → take_nonce → verify → issue → verify_token）
    #[test]
    fn full_challenge_sign_verify_flow() {
        let auth = ChainAuth::new();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let nonce = auth.create_nonce(&pubkey);
        let sig = sign_nonce(&sk, &nonce);
        assert!(auth.take_nonce(&pubkey, &nonce), "nonce 匹配且未过期");
        assert!(verify_nonce_signature(sk.verifying_key(), &nonce, &sig));
        let (token, _) = auth.issue_token(&pubkey);
        assert_eq!(auth.verify_token(&token), Some(pubkey));
        assert_eq!(NONCE_TTL_SECS, 60, "nonce 60s TTL（契约）");
        assert_eq!(TOKEN_TTL_SECS, 86_400, "token 24h（契约）");
    }
}
