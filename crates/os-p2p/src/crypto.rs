//! P2a 链路加密——ECDH 密钥协商 + AES-256-GCM 会话密码（设计 08-20 补丁）。
//!
//! P1 的握手只做双向身份认证（nonce 挑战-签名），帧明文。P2a 在同一握手上
//! 叠加 **ECDHE**：双方 Hello 时互发 secp256k1 **临时公钥**，挑战签名的覆盖面
//! 从"仅 nonce"扩展到"nonce + 双方临时公钥"（签名转录本，[`ecdh_transcript`]）
//! ——中间人替换任一临时公钥都会使签名验证失败（防 MITM 密钥替换）。随后：
//!
//! ```text
//! ECDH(我的临时私钥, 对端临时公钥) → 32B 共享秘密
//! SHA-256("nexos-p2p-aead-v1" ‖ 共享秘密 ‖ nonce_lo ‖ nonce_hi) → 256-bit 会话密钥
//! （nonce 按字典序 canonical 排序——两侧无需角色约定即得同一密钥）
//! ```
//!
//! [`SessionCipher`] 持有该密钥，负责握手后所有帧的 AES-256-GCM 加解密
//! （96-bit 随机 nonce + 128-bit 认证标签，AAD 绑定协议版本标签）。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use k256::ecdh::EphemeralSecret;
use k256::elliptic_curve::rand_core::{OsRng, RngCore};
use k256::elliptic_curve::sec1::ToEncodedPoint;

use crate::{P2pError, Result};

/// AES-256-GCM 密钥长度。
pub const AEAD_KEY_LEN: usize = 32;
/// GCM nonce 长度（96-bit）。
pub const AEAD_NONCE_LEN: usize = 12;
/// GCM 认证标签长度（128-bit）。
pub const AEAD_TAG_LEN: usize = 16;
/// 密钥派生域分隔标签（KDF 输入首段——绑定协议语境，防跨协议密钥重用）。
const KDF_LABEL: &[u8] = b"nexos-p2p-aead-v1";
/// GCM AAD（附加认证数据）：与 KDF 同源的版本标签——密文绑定到本协议版本。
const AEAD_AAD: &[u8] = b"nexos-p2p-aead-v1";

// ============================================================================
// 临时密钥对（ECDHE）
// ============================================================================

/// 握手用一次性 ECDH 密钥对（每连接新生成，用后即弃——前向保密）。
pub struct EphemeralKey {
    secret: EphemeralSecret,
    public_hex: String,
}

impl EphemeralKey {
    /// CSPRNG 生成。
    #[must_use]
    pub fn generate() -> Self {
        let secret = EphemeralSecret::random(&mut OsRng);
        let compressed = secret.public_key().to_encoded_point(true); // 33 字节
        let public_hex = format!("0x{}", hex::encode(compressed.as_bytes()));
        Self { secret, public_hex }
    }

    /// 本端临时公钥（`0x` + 66 hex 压缩 sec1——Hello.eph 字段）。
    #[must_use]
    pub fn public_hex(&self) -> &str {
        &self.public_hex
    }

    /// ECDH + KDF 派生会话密钥。
    ///
    /// `nonce_lo`/`nonce_hi` 为双方挑战 nonce 的字典序 canonical 排序——两侧
    /// 无需角色约定（谁拨谁收）即得同一密钥；nonce 入 KDF 把会话密钥绑定到
    /// 本次握手（跨会话重放旧 ECDH 公钥也无法复现密钥）。
    pub fn derive_session(
        &self,
        peer_public_hex: &str,
        nonce_lo: &str,
        nonce_hi: &str,
    ) -> Result<SessionCipher> {
        let key = self.shared_key_bytes(peer_public_hex)?;
        Ok(SessionCipher::from_key(&derive_session_key(
            &key, nonce_lo, nonce_hi,
        )))
    }

    /// 仅 ECDH 共享秘密（KDF 输入；测试对账用）。
    fn shared_key_bytes(&self, peer_public_hex: &str) -> Result<[u8; 32]> {
        let hex_str = peer_public_hex
            .strip_prefix("0x")
            .unwrap_or(peer_public_hex);
        let bytes =
            hex::decode(hex_str).map_err(|_| P2pError::Crypto("ephemeral key not hex".into()))?;
        let peer_pk = k256::PublicKey::from_sec1_bytes(&bytes)
            .map_err(|_| P2pError::Crypto("ephemeral key not a valid sec1 point".into()))?;
        let shared = self.secret.diffie_hellman(&peer_pk);
        let raw = shared.raw_secret_bytes();
        let mut out = [0u8; 32];
        out.copy_from_slice(raw.as_slice());
        Ok(out)
    }
}

/// ECDH 共享秘密 + 双方 nonce → 256-bit 会话密钥（SHA-256 KDF）。
#[must_use]
pub fn derive_session_key(shared: &[u8], nonce_lo: &str, nonce_hi: &str) -> [u8; AEAD_KEY_LEN] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(KDF_LABEL);
    h.update(shared);
    h.update(nonce_lo.as_bytes());
    h.update(nonce_hi.as_bytes());
    h.finalize().into()
}

/// 挑战签名的转录本：nonce + 签名方临时公钥 + 验证方临时公钥。
///
/// 挑战应答方（签名人）用它签，挑战发起方（验证人）用它验——**任何一方的
/// 临时公钥被中途替换，两侧构造的转录本不再一致 → 验签失败**（ECDH 防 MITM
/// 的根：密钥协商被认证到 NodeID 私钥上）。
#[must_use]
pub fn ecdh_transcript(nonce: &str, signer_eph_hex: &str, verifier_eph_hex: &str) -> String {
    format!("nexos-p2p-ecdh-v1:{nonce}:{signer_eph_hex}:{verifier_eph_hex}")
}

// ============================================================================
// 会话密码（帧级 AES-256-GCM）
// ============================================================================

/// 每连接会话密码：握手 ECDH 派生的 256-bit 密钥 + AES-256-GCM。
///
/// Clone 共享给连接的读写任务（nonce 每帧 CSPRNG 独立随机——两侧无需计数器
/// 协调，也不会跨方向碰撞）。`PartialEq` 按密钥字节（握手测试对账两侧派生一致）。
#[derive(Clone)]
pub struct SessionCipher {
    key: [u8; AEAD_KEY_LEN],
    cipher: Aes256Gcm,
}

impl SessionCipher {
    /// 从原始密钥构造（KDF 输出；测试/诊断路径——生产密钥只经
    /// [`EphemeralKey::derive_session`] 产生）。
    #[must_use]
    pub fn from_key(key: &[u8; AEAD_KEY_LEN]) -> Self {
        Self {
            key: *key,
            cipher: Aes256Gcm::new_from_slice(key).expect("256-bit key 必可构造"),
        }
    }

    /// 会话密钥摘要（诊断输出；不暴露原始密钥）。
    #[must_use]
    pub fn key_fingerprint(&self) -> String {
        crate::short_hex(&hex::encode(sha2_digest(&self.key)))
    }

    /// 加密：输出 `nonce(12B) ‖ ciphertext+tag(≥17B)`（整体即线上帧体）。
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: AEAD_AAD,
                },
            )
            .map_err(|_| P2pError::Crypto("aes-gcm encrypt failed".into()))?;
        let mut out = Vec::with_capacity(AEAD_NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// 解密 [`SessionCipher::seal`] 的输出；标签不符/截断 → Err（调用方断连）。
    pub fn open(&self, wire: &[u8]) -> Result<Vec<u8>> {
        if wire.len() < AEAD_NONCE_LEN + AEAD_TAG_LEN {
            return Err(P2pError::Crypto("encrypted frame too short".into()));
        }
        let (nonce, ct) = wire.split_at(AEAD_NONCE_LEN);
        self.cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: AEAD_AAD,
                },
            )
            .map_err(|_| P2pError::Crypto("aes-gcm tag mismatch (tampered or wrong key)".into()))
    }
}

impl PartialEq for SessionCipher {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl std::fmt::Debug for SessionCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionCipher({})", self.key_fingerprint())
    }
}

fn sha2_digest(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(data).into()
}

// ============================================================================
// 单元测——ECDH 对称性 / KDF canonical 排序 / GCM 往返与防篡改
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // 1. ECDH 对称性 + 转录本：双方各自生成临时密钥，交叉派生 → 同一密钥；
    //    转录本对 (signer, verifier) 顺序敏感（方向反了就不同——两侧各按
    //    自己的视角构造，语义互逆）
    #[test]
    fn ecdh_symmetric_key_agreement() {
        let a = EphemeralKey::generate();
        let b = EphemeralKey::generate();
        assert_ne!(a.public_hex(), b.public_hex(), "CSPRNG 不重复");
        // nonce canonical 排序与两侧无关
        let (lo, hi) = if "nonce-a" <= "nonce-b" {
            ("nonce-a", "nonce-b")
        } else {
            ("nonce-b", "nonce-a")
        };
        let ka = a.derive_session(b.public_hex(), lo, hi).unwrap();
        let kb = b.derive_session(a.public_hex(), lo, hi).unwrap();
        assert_eq!(ka, kb, "两侧必须派生同一会话密钥");
        assert_ne!(ka, SessionCipher::from_key(&[0u8; 32]));
        // 转录本：不同 eph / 不同 nonce / 方向反转都改变消息
        let t = ecdh_transcript("n", "0xAA", "0xBB");
        assert_eq!(t, "nexos-p2p-ecdh-v1:n:0xAA:0xBB");
        assert_ne!(ecdh_transcript("n", "0xBB", "0xAA"), t);
        assert_ne!(ecdh_transcript("m", "0xAA", "0xBB"), t);
    }

    // 2. 非法临时公钥拒绝（非 hex / 非 sec1 点）
    #[test]
    fn ecdh_rejects_malformed_peer_key() {
        let a = EphemeralKey::generate();
        for bad in ["", "0x1234", "zz", "0x00"] {
            assert!(
                a.derive_session(bad, "lo", "hi").is_err(),
                "应拒绝非法临时公钥: {bad}"
            );
        }
    }

    // 3. GCM 往返：seal→open 还原；篡改密文/nonce → 标签失败；截断 → 长度拒绝；
    //    不同密钥互解失败
    #[test]
    fn gcm_roundtrip_and_tamper_detection() {
        let cipher = SessionCipher::from_key(&derive_session_key(b"s", "lo", "hi"));
        let plaintext = r#"{"kind":"send","text":"hello NexOS"}"#.as_bytes();
        let sealed = cipher.seal(plaintext).unwrap();
        assert_eq!(
            sealed.len(),
            plaintext.len() + AEAD_NONCE_LEN + AEAD_TAG_LEN
        );
        assert_eq!(cipher.open(&sealed).unwrap(), plaintext.to_vec());
        // 两次 seal 的 nonce 不同（随机 nonce）——密文不同
        assert_ne!(cipher.seal(plaintext).unwrap(), sealed);
        // 篡改密文中部一字节 → GCM 标签失败
        let mut tampered = sealed.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xFF;
        assert!(cipher.open(&tampered).is_err(), "篡改必须被 GCM 标签检出");
        // 篡改 nonce 首字节同样失败
        let mut bad_nonce = sealed.clone();
        bad_nonce[0] ^= 0x01;
        assert!(cipher.open(&bad_nonce).is_err());
        // 截断 → 长度拒绝
        assert!(cipher.open(&sealed[..20]).is_err());
        assert!(cipher.open(&[]).is_err());
        // 错误密钥互解失败（KDF 输入不同 → 密钥不同）
        let other = SessionCipher::from_key(&derive_session_key(b"t", "lo", "hi"));
        assert!(other.open(&sealed).is_err());
        // 指纹可用且不泄密钥
        assert!(!cipher.key_fingerprint().is_empty());
    }
}
