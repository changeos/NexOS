//! 访客域类型——身份体系 / 角色 / 文件共享（呼应 §3.18 身份体系表）
//!
//! 决策依据：规划文档 §3.18 —— 访客身份分四类（RandomId/ExtendedId/PublicKey/ChainCredential），
//! 每类身份有不同的生命周期与验证强度；角色绑定 IM 群组、文件共享、带宽/时长配额。

use os_core::{DateTime, Deserialize, GuestId, Serialize, ShareId};

// ----------------------------------------------------------------------------
// GuestId 生成（纯算法，无 rand crate 依赖）
// ----------------------------------------------------------------------------

/// GuestId 安全字符集（排除易混淆字符 O / 0 / I / 1 / L，避免人工抄录歧义）。
///
/// 共 31 个字符（A-H J-N P-Z 共 25 个大写字母 + 2-9 共 8 个数字 = 33... 实际取
/// A-Z 去掉 O/I/L = 23 个字母，加 2-9 去掉 0/1 已在数字段剔除 = 8 个数字，合计 31）。
/// 6 位组合数 = 31^6 ≈ 8.87 亿（~10 亿量级），满足"约 10 亿组合"要求。
pub const GUEST_ID_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// GuestId 前缀（格式 `GUEST-XXXXXX`）。
pub const GUEST_ID_PREFIX: &str = "GUEST-";

/// GuestId 随机部分长度（6 位）。
pub const GUEST_ID_RANDOM_LEN: usize = 6;

/// 熵源 trait——抽象随机字节来源，便于测试注入确定性实现，生产用系统熵。
///
/// 设计：`fill` 用调用方提供的字节缓冲；实现负责填充 `[0, alphabet_len)` 范围内
/// 的偏移（或任意 u8，由 `generate_guest_id_with` 取模映射到字符集）。
pub trait EntropySource: Send + Sync {
    /// 向 `buf` 填充任意 u8 字节（实现可用系统时间/线程地址/进程 pid 等混合熵）。
    fn fill(&self, buf: &mut [u8]);
}

/// 用给定熵源生成一个 `GUEST-XXXXXX` 格式的 GuestId。
///
/// 算法：取 6 字节熵，每字节对字符集长度取模得到字符，拼成 6 位随机部分，
/// 再加 `GUEST-` 前缀。取模带来的模偏差在 256 mod 31 = 8 的偏差范围内可接受
/// （访客 ID 非密码学密钥，碰撞由调用方去重重试兜底）。
pub fn generate_guest_id_with(entropy: &dyn EntropySource) -> GuestId {
    let mut buf = [0u8; GUEST_ID_RANDOM_LEN];
    entropy.fill(&mut buf);
    let alphabet_len = GUEST_ID_ALPHABET.len() as u8;
    let mut s = String::with_capacity(GUEST_ID_PREFIX.len() + GUEST_ID_RANDOM_LEN);
    s.push_str(GUEST_ID_PREFIX);
    for b in buf {
        let idx = (b % alphabet_len) as usize;
        s.push(GUEST_ID_ALPHABET[idx] as char);
    }
    GuestId::new(s)
}

/// 校验 GuestId 格式（前缀 + 6 位且全部在安全字符集内）。
///
/// 供 IdentityEngine.create_guest 去重时前置校验，也可供 api 层入参校验。
pub fn validate_guest_id(id: &GuestId) -> Result<(), crate::GuestError> {
    let s = id.as_str();
    let rest = s.strip_prefix(GUEST_ID_PREFIX).ok_or_else(|| {
        crate::GuestError::Internal(format!("GuestId 缺少前缀 `{GUEST_ID_PREFIX}`: {s}"))
    })?;
    if rest.len() != GUEST_ID_RANDOM_LEN {
        return Err(crate::GuestError::Internal(format!(
            "GuestId 随机部分长度错误：期望 {GUEST_ID_RANDOM_LEN}，实际 {}",
            rest.len()
        )));
    }
    for c in rest.bytes() {
        if !GUEST_ID_ALPHABET.contains(&c) {
            return Err(crate::GuestError::Internal(format!(
                "GuestId 含非法字符 `{}`（不在安全字符集内）",
                c as char
            )));
        }
    }
    Ok(())
}

/// 系统熵源默认实现（无 rand 依赖）。
///
/// 混合 `SystemTime` 纳秒 + 线程/对象地址 + 进程内自增计数器作为熵源。
/// **非密码学安全**，仅用于访客 ID 生成（非密钥）；碰撞由调用方去重兜底。
pub struct SystemEntropy {
    counter: std::sync::atomic::AtomicU64,
}

impl SystemEntropy {
    /// 构造（种子取系统当前纳秒时间）。
    pub fn new() -> Self {
        let seed = Self::time_nanos();
        Self {
            counter: std::sync::atomic::AtomicU64::new(seed),
        }
    }

    fn time_nanos() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xdead_beef)
    }

    fn next_state(&self) -> u64 {
        // 简单线性同余 + 抖动（混合时间与计数器）。
        let prev = self
            .counter
            .fetch_add(0x9E3779B97F4A7C15, std::sync::atomic::Ordering::Relaxed);
        let t = Self::time_nanos();
        // xorshift64 一步混合，避免连续取模产生明显序列。
        let mut x = prev.wrapping_add(t).wrapping_mul(0x9E3779B97F4A7C15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58476D1CE4E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        x
    }
}

impl Default for SystemEntropy {
    fn default() -> Self {
        Self::new()
    }
}

impl EntropySource for SystemEntropy {
    fn fill(&self, buf: &mut [u8]) {
        // 每 8 字节消费一个 state；不足 8 字节的尾部单独处理。
        let mut i = 0;
        while i + 8 <= buf.len() {
            let s = self.next_state();
            buf[i..i + 8].copy_from_slice(&s.to_le_bytes());
            i += 8;
        }
        if i < buf.len() {
            let s = self.next_state().to_le_bytes();
            let rem = buf.len() - i;
            buf[i..].copy_from_slice(&s[..rem]);
        }
    }
}

/// 用系统默认熵源生成一个 GuestId（便捷封装）。
pub fn generate_guest_id() -> GuestId {
    let entropy = SystemEntropy::new();
    generate_guest_id_with(&entropy)
}

// ----------------------------------------------------------------------------
// 身份类型 / 状态
// ----------------------------------------------------------------------------

/// 访客身份类型（§3.18 身份体系表）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestIdentityType {
    /// 随机 ID（一次性，GUEST-XXXXXX，最低信任）
    RandomId,
    /// 扩展 ID（带续期，长期访客）
    ExtendedId,
    /// 公钥（持有密钥对的访客，签名认证）
    PublicKey,
    /// 链上凭证（经链上验证的访客，最高信任，见 chain 模块）
    ChainCredential,
}

/// 访客状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestStatus {
    /// 待认证（已创建但未完成首次认证）
    Pending,
    /// 已认证（在有效期内）
    Authed,
    /// 已过期（JWT/NFT 超时）
    Expired,
    /// 已撤销（管理员/策略主动吊销）
    Revoked,
}

// ----------------------------------------------------------------------------
// GuestIdentity
// ----------------------------------------------------------------------------

/// 访客身份（一个访客的完整身份记录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestIdentity {
    /// 访客 ID（格式 GUEST-XXXXXX）
    pub id: GuestId,
    /// 身份类型
    pub identity_type: GuestIdentityType,
    /// 创建时间
    pub created_at: DateTime,
    /// 过期时间
    pub expires_at: DateTime,
    /// JWT 过期时间（Access token 失效时刻）
    pub jwt_expiry: DateTime,
    /// NFT 超时（秒；用于 nftables 规则自动过期，0 = 不启用 nft 超时）
    pub nft_timeout_secs: u64,
    /// 当前状态
    pub status: GuestStatus,
    /// 扩展元数据（公钥/链地址/标签等开放结构）
    pub metadata: serde_json::Value,
}

// ----------------------------------------------------------------------------
// GuestRole / GuestFileShare / FileAccess
// ----------------------------------------------------------------------------

/// 文件访问权限
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    /// 只读
    ReadOnly,
    /// 读写
    ReadWrite,
}

/// 授权给访客的文件共享
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestFileShare {
    /// 共享 ID（复用 os-core::ShareId）
    pub share_id: ShareId,
    /// 访问权限
    pub access: FileAccess,
}

/// 访客角色（绑定 IM 群组 / 共享 / 配额 / 允许服务）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestRole {
    /// 角色名（如 "guest-default" / "chain-verified"）
    pub name: String,
    /// 自动加入的 IM 群组列表
    pub im_groups: Vec<String>,
    /// 授权的文件共享列表
    pub file_shares: Vec<GuestFileShare>,
    /// 带宽上限（kbps；0 = 不限）
    pub bandwidth_limit_kbps: u64,
    /// 每日在线时长上限（分钟；0 = 不限）
    pub daily_time_limit_mins: u32,
    /// 允许访问的服务列表（如 ["smb", "webdav"]）
    pub allowed_services: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定熵源（确定性，用于测试）。
    struct FixedEntropy(&'static [u8]);
    impl EntropySource for FixedEntropy {
        fn fill(&self, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.0.get(i).copied().unwrap_or(0);
            }
        }
    }

    #[test]
    fn guest_id_format_and_alphabet() {
        // 字符集恰好 31 个字符，排除 O/0/I/1/L。
        assert_eq!(GUEST_ID_ALPHABET.len(), 31);
        for &c in GUEST_ID_ALPHABET {
            assert!(!matches!(c, b'O' | b'0' | b'I' | b'1' | b'L'));
        }
    }

    #[test]
    fn generate_guest_id_deterministic_with_fixed_entropy() {
        // 字节 0..5 → 取模 31 映射到字符集。
        let e = FixedEntropy(&[0, 1, 2, 30, 31, 32]);
        let id = generate_guest_id_with(&e);
        let s = id.as_str();
        assert!(s.starts_with("GUEST-"));
        assert_eq!(s.len(), "GUEST-".len() + 6);
        // 字节 31 % 31 == 0 → 第一个字符；字节 32 % 31 == 1 → 第二个字符。
        let chars: Vec<char> = s["GUEST-".len()..].chars().collect();
        assert_eq!(chars[0] as u8, GUEST_ID_ALPHABET[0]);
        assert_eq!(chars[1] as u8, GUEST_ID_ALPHABET[1]);
        // 字节 30 → 索引 30（alphabet 末位字符）。
        assert_eq!(chars[3] as u8, GUEST_ID_ALPHABET[30]);
    }

    #[test]
    fn validate_guest_id_ok_and_fail() {
        let e = FixedEntropy(&[0, 1, 2, 3, 4, 5]);
        let id = generate_guest_id_with(&e);
        assert!(validate_guest_id(&id).is_ok());

        assert!(validate_guest_id(&GuestId::new("GUEST-ABC")).is_err()); // 长度不足
        assert!(validate_guest_id(&GuestId::new("ABCDEF123456")).is_err()); // 无前缀
        assert!(validate_guest_id(&GuestId::new("GUEST-ABCDEF")).is_ok()); // ABCDEF 均在安全字符集
                                                                           // 显式含非法字符 0。
        assert!(validate_guest_id(&GuestId::new("GUEST-ABC0EF")).is_err());
    }

    #[test]
    fn system_entropy_generates_valid_unique_ids() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = generate_guest_id();
            validate_guest_id(&id).unwrap();
            ids.insert(id.as_str().to_string());
        }
        // 1000 个 ID 应几乎全唯一（系统熵足够；允许极小概率碰撞但 1000 量级不期望）。
        assert!(ids.len() > 990, "重复率过高: {}/1000", 1000 - ids.len());
    }
}
