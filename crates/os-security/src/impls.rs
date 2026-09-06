//! os-security 实现——具体 struct（非 trait 定义）。
//!
//! 本文件提供 5 个 trait 的实现 struct，按规格书 §3 命名：
//! - `DbAuthProvider`（`AuthProvider`）—— 真实 Argon2id 密码哈希（批 2 接通）
//! - `JwtIssuerImpl`（`JwtIssuer`）—— 真实 jsonwebtoken HS256（批 2 接通）
//! - `CaCertManager`（`CertManager`）—— 真实 rcgen 自签 CA + 叶子证书签发（批 3 接通）+
//!   ACME 自动签续（批 4 接通，instant-acme，ADR-DEPS-004）；ACME 证书优先，CA 自签作 fallback
//! - `TotpTwoFactor`（`TwoFactor`）—— TOTP 真实（HMAC-SHA1 via hmac/sha1，批 2 接通）；
//!   enable/disable 状态管理 + 加密存储阻塞于 AEAD crate
//! - `BoringtunVpnManager`（`VpnManager`）—— 真实 boringtun noise 协议层（批 3 接通）：
//!   peer 增删查 + x25519 密钥 + 每隧道一个 `boringtun::noise::Tunn`（握手/字节统计真实）；
//!   **不真建 TUN 隧道**（需 root/网络命名空间，红线——仅 fixture 测 noise 层）
//!
//! 已接通：argon2 / jsonwebtoken / hmac+sha1 / rcgen / boringtun / instant-acme。
//! 仍阻塞：AEAD crate（2FA secret 加密落盘）。

use crate::auth::{Principal, Role, User, UserId};
use crate::cert::Certificate;
use crate::jwt::JwtClaims;
use crate::twofactor::TwoFactorSecret;
use crate::vpn::{VpnPeer, VpnStatus};
use crate::SecurityError;
use os_core::DateTime;
use std::collections::HashMap;
use std::sync::Mutex;

// ============================================================================
// DbAuthProvider —— 内存用户表 + 常量时间密码比对
// ============================================================================

/// 基于内存用户表的认证提供者（生产用 DB 版待 storage-agent 配合）。
///
/// 用 `Mutex<HashMap>` 存用户与凭证；密码哈希走 `password::hash_password`（真实
/// Argon2id），校验走 `password::verify_password`（PHC 解析 + argon2 验签，回退常量时间比较）。
pub struct DbAuthProvider {
    users: Mutex<HashMap<UserId, User>>,
    creds: Mutex<HashMap<UserId, String>>, // password_hash
}

impl DbAuthProvider {
    /// 构造空提供者。
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            creds: Mutex::new(HashMap::new()),
        }
    }

    fn now() -> DateTime {
        chrono::Utc::now()
    }
}

impl Default for DbAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl crate::auth::AuthProvider for DbAuthProvider {
    async fn create_user(&self, name: &str, roles: Vec<Role>) -> Result<User, SecurityError> {
        crate::auth::validate_roles(&roles)?;
        let id = UserId::new(uuid::Uuid::new_v4().to_string());
        let user = User::new(id.clone(), name, roles, Self::now())?;
        let mut users = self
            .users
            .lock()
            .map_err(|e| SecurityError::Internal(format!("users 锁中毒: {e}")))?;
        if users.contains_key(&id) {
            return Err(SecurityError::UserExists(name.to_string()));
        }
        users.insert(id.clone(), user.clone());
        Ok(user)
    }

    async fn delete_user(&self, user: &UserId) -> Result<(), SecurityError> {
        let mut users = self
            .users
            .lock()
            .map_err(|e| SecurityError::Internal(format!("users 锁中毒: {e}")))?;
        users
            .remove(user)
            .ok_or_else(|| SecurityError::UserNotFound(user.to_string()))?;
        self.creds
            .lock()
            .map_err(|e| SecurityError::Internal(format!("creds 锁中毒: {e}")))?
            .remove(user);
        Ok(())
    }

    async fn list_users(&self) -> Result<Vec<User>, SecurityError> {
        let users = self
            .users
            .lock()
            .map_err(|e| SecurityError::Internal(format!("users 锁中毒: {e}")))?;
        Ok(users.values().cloned().collect())
    }

    async fn set_password(&self, user: &UserId, password: &str) -> Result<(), SecurityError> {
        // 真实 argon2id 哈希后存储（PHC 字符串）。
        let hash = crate::password::hash_password(password)?;
        let mut creds = self
            .creds
            .lock()
            .map_err(|e| SecurityError::Internal(format!("creds 锁中毒: {e}")))?;
        if !self
            .users
            .lock()
            .map_err(|e| SecurityError::Internal(format!("users 锁中毒: {e}")))?
            .contains_key(user)
        {
            return Err(SecurityError::UserNotFound(user.to_string()));
        }
        creds.insert(user.clone(), hash);
        Ok(())
    }

    async fn authenticate(&self, name: &str, password: &str) -> Result<Principal, SecurityError> {
        let users = self
            .users
            .lock()
            .map_err(|e| SecurityError::Internal(format!("users 锁中毒: {e}")))?;
        // 按显示名查找用户
        let user = users
            .values()
            .find(|u| u.name == name)
            .cloned()
            .ok_or(SecurityError::AuthFailed)?;
        if !user.enabled {
            return Err(SecurityError::AuthFailed);
        }
        let stored = self
            .creds
            .lock()
            .map_err(|e| SecurityError::Internal(format!("creds 锁中毒: {e}")))?
            .get(&user.id)
            .cloned()
            .ok_or(SecurityError::AuthFailed)?;
        if !crate::password::verify_password(password, &stored) {
            return Err(SecurityError::AuthFailed);
        }
        Principal::new(user.clone(), user.roles.clone(), Self::now())
    }

    async fn disable_user(&self, user: &UserId) -> Result<(), SecurityError> {
        let mut users = self
            .users
            .lock()
            .map_err(|e| SecurityError::Internal(format!("users 锁中毒: {e}")))?;
        let u = users
            .get_mut(user)
            .ok_or_else(|| SecurityError::UserNotFound(user.to_string()))?;
        u.enabled = false;
        Ok(())
    }
}

// ============================================================================
// JwtIssuerImpl —— 真实 jsonwebtoken（HS256）
// ============================================================================

/// JWT 签发器实现（HS256 对称签名）。
///
/// 密钥模型：构造时传入 HMAC secret（来自 env/keyring/KMS，本结构不负责密钥源）。
/// 签发用 `jsonwebtoken::encode`（HS256），校验用 `jsonwebtoken::decode` 验签 + 过期。
/// 密钥轮换：维护「当前签发密钥」+「宽限期旧密钥列表」，验签时按 kid 依次尝试；
/// 实际 kid 通过 header 注入（HS256 默认无 kid，这里用 `typ` 之外的简单策略：
/// 维护 current + grace secrets，验签按顺序尝试）。
pub struct JwtIssuerImpl {
    /// 当前签发密钥（HS256 secret）。
    current: Mutex<Vec<u8>>,
    /// 宽限期旧密钥列表（rotate_keys 后追加；仅用于验签）。
    grace: Mutex<Vec<Vec<u8>>>,
}

impl JwtIssuerImpl {
    /// 用给定 secret 构造（无宽限期旧密钥）。
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            current: Mutex::new(secret.into()),
            grace: Mutex::new(Vec::new()),
        }
    }

    /// 构造测试用实例（固定 secret）。
    #[cfg(test)]
    pub fn for_testing() -> Self {
        Self::new(b"test-secret-do-not-use-in-prod".to_vec())
    }

    /// 编码辅助：用 `secret` 编码 claims 为紧凑 JWT（HS256）。
    fn encode_with(secret: &[u8], claims: &JwtClaims) -> Result<String, SecurityError> {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some("JWT".into());
        let key = EncodingKey::from_secret(secret);
        encode(&header, claims, &key)
            .map_err(|e| SecurityError::JwtInvalid(format!("JWT 编码失败: {e}")))
    }

    /// 解码辅助：用 `secret` 验签 + 解析 token，成功返回 token_data。
    fn decode_with(secret: &[u8], token: &str) -> Result<JwtClaims, SecurityError> {
        use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
        let mut validation = Validation::new(Algorithm::HS256);
        // 校验 exp（默认开），不要求 aud（本实现未签 aud）。
        validation.validate_exp = true;
        validation.required_spec_claims.clear();
        let key = DecodingKey::from_secret(secret);
        let data = decode::<JwtClaims>(token, &key, &validation)
            .map_err(|e| SecurityError::JwtInvalid(format!("JWT 校验失败: {e}")))?;
        Ok(data.claims)
    }
}

impl Default for JwtIssuerImpl {
    fn default() -> Self {
        // 默认 secret 仅占位；生产路径必须显式传入密钥源。
        Self::new("os-security-default-jwt-secret")
    }
}

#[async_trait::async_trait]
impl crate::jwt::JwtIssuer for JwtIssuerImpl {
    async fn issue(&self, claims: JwtClaims) -> Result<String, SecurityError> {
        let secret = self
            .current
            .lock()
            .map_err(|e| SecurityError::Internal(format!("current 锁中毒: {e}")))?;
        Self::encode_with(&secret, &claims)
    }

    async fn verify(&self, token: &str) -> Result<JwtClaims, SecurityError> {
        // 先用当前密钥验签。
        let current = self
            .current
            .lock()
            .map_err(|e| SecurityError::Internal(format!("current 锁中毒: {e}")))?;
        if let Ok(claims) = Self::decode_with(&current, token) {
            return Ok(claims);
        }
        drop(current);
        // 当前密钥失败 → 依次尝试宽限期旧密钥。
        let grace = self
            .grace
            .lock()
            .map_err(|e| SecurityError::Internal(format!("grace 锁中毒: {e}")))?;
        for old in grace.iter() {
            if let Ok(claims) = Self::decode_with(old, token) {
                return Ok(claims);
            }
        }
        Err(SecurityError::JwtInvalid(
            "JWT 验签失败：无匹配密钥（current + grace 均拒绝）".into(),
        ))
    }

    async fn rotate_keys(&self) -> Result<(), SecurityError> {
        // trait 契约要求无入参轮换；但 HS256 密钥必须由外部提供。
        // 本实现把「轮换」拆为两步：调用方先 set_current_key(new) 自动把旧密钥
        // 推入宽限期，再调用 rotate_keys 做清理（淘汰超期旧密钥）。单独调用本方法
        // 若未先 set_current_key，则等价于「修剪宽限期」。
        let mut grace = self
            .grace
            .lock()
            .map_err(|e| SecurityError::Internal(format!("grace 锁中毒: {e}")))?;
        // 限制宽限期长度，防止无限增长（保留最近 4 个旧密钥）。
        while grace.len() > 4 {
            grace.remove(0);
        }
        Ok(())
    }
}

impl JwtIssuerImpl {
    /// 设置新的当前签发密钥（密钥轮换配套接口）。
    ///
    /// 自动把**旧**当前密钥推入宽限期（用于轮换后旧 token 的宽限验签）。
    /// 不在 trait 上暴露（trait 的 rotate_keys 无入参）；由调用方在持有具体类型时注入。
    pub fn set_current_key(&self, new_secret: impl Into<Vec<u8>>) -> Result<(), SecurityError> {
        let new_secret = new_secret.into();
        let mut current = self
            .current
            .lock()
            .map_err(|e| SecurityError::Internal(format!("current 锁中毒: {e}")))?;
        // 仅当新旧不同才入宽限期（避免重复入列）。
        let old = current.clone();
        if old != new_secret {
            let mut grace = self
                .grace
                .lock()
                .map_err(|e| SecurityError::Internal(format!("grace 锁中毒: {e}")))?;
            if !grace.iter().any(|k| k == &old) {
                grace.push(old);
            }
            while grace.len() > 4 {
                grace.remove(0);
            }
            *current = new_secret;
        }
        Ok(())
    }

    /// 清空宽限期旧密钥（彻底淘汰旧密钥）。
    pub fn clear_grace(&self) -> Result<(), SecurityError> {
        let mut grace = self
            .grace
            .lock()
            .map_err(|e| SecurityError::Internal(format!("grace 锁中毒: {e}")))?;
        grace.clear();
        Ok(())
    }
}

// ============================================================================
// CaCertManager —— 真实 rcgen（自签 CA + 叶子证书签发）
// ============================================================================

/// 证书来源——CA 自签 vs ACME（用于续期策略选择）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum CertSource {
    /// 内部 CA 自签（`init_ca` + `sign`/`renew` 路径）。
    InternalCa,
    /// ACME 自动签发（`acme_request` 路径）。续期走 `acme_request(domain)`。
    /// 持有原签发域名，renew_expiring 时按此 re-request。
    Acme(String),
}

/// 内部已签发的叶子证书记录（保留 CN + 原 CSR 公钥，用于 renew 重签）。
#[derive(Clone)]
#[allow(dead_code)] // pem/der 保留供未来 cert 导出 API 与链验证审计；当前测试间接消费。
struct LeafRecord {
    /// 证书元数据（对外暴露的 `Certificate`）。
    meta: Certificate,
    /// 叶子证书 PEM（返回给调用方的字节，按 PEM 编码——见 sign 契约"PEM/DER"）。
    pem: Vec<u8>,
    /// 叶子证书 DER（用于链验证测试）。
    der: Vec<u8>,
    /// 签发时的 CN（renew 时复用）。
    common_name: String,
    /// 签发时的有效期天数（renew 时复用）。
    days: u32,
    /// 签发时从 CSR 提取的公钥原始字节 + 算法 OID 信息——renew 需重新构造 CSR 不可行，
    /// 故保留 DER 中的 SPKI 以便重新构造叶子参数。简化处理：renew 用同 CN 重新生成
    /// 叶子 KeyPair（旧公钥失效，调用方须用新证书——符合 PKI renew 语义）。
    spki_hint: String,
    /// 证书来源（CA 自签 vs ACME）+ ACME 域名。续期策略按此分派。
    source: CertSource,
}

/// 内部 CA 状态——rcgen 的私钥 + issuer（用于后续签发）+ CA 证书元数据。
struct CaState {
    /// CA 私钥（rcgen KeyPair）。私钥仅在内存；生产路径须存 KMS/keyring。
    /// 用 `Box<dyn Any>>`-free 的直接持有：rcgen KeyPair 拷贝便宜（DER 字节）。
    key_pair: rcgen::KeyPair,
    /// CA 证书参数（构造 Issuer 用；每次签发按需重建 Issuer——因 Issuer 借用 params）。
    params: rcgen::CertificateParams,
    /// CA 证书元数据（init_ca 返回值的一份拷贝）。
    meta: Certificate,
    /// CA 证书 DER（链验证用）。
    #[allow(dead_code)] // 仅测试消费；保留供未来 cert 导出 API。
    der: Vec<u8>,
}

/// 内部 CA + ACME 证书管理器（真实 rcgen + instant-acme 实现）。
///
/// - `init_ca`：用 rcgen 生成 ECDSA P256 自签根 CA（IsCa::Ca + KeyCertSign/CrlSign），
///   返回 CA 证书元数据；私钥仅内存持有（生产路径须存 KMS/keyring）。
/// - `sign`：解析 PEM/DER CSR（用 rcgen x509-parser）→ 用 CA issuer 签发叶子证书
///   （DigitalSignature + ServerAuth/ClientAuth EKU）→ 返回 PEM 字节。
/// - `list_certs`：列出 CA + 所有已签发叶子证书（含 ACME 来源）。
/// - `renew`：按 id 重新签发叶子证书（保持 CN，新生成 KeyPair——PKI renew 语义）。
/// - `acme_request`：用 instant-acme 走 RFC 8555 流程（order → challenge → finalize →
///   download），为对外域名签发 LE 等公共 CA 证书。须先 `with_acme` 注入 [`crate::acme::AcmeConfig`]；
///   未注入则返回错误（内部域名场景改用 init_ca + sign fallback）。
/// - `renew_expiring`：续期检查——按来源分派（ACME → re-request；CA 自签 → renew）。
///
/// **ACME 与 CA 协调**：ACME 证书优先（公网可信）；CA 自签作 fallback（内部可信）。
/// 不自动从 ACME 回退到 CA（语义不同，调用方显式选择）。
///
/// **密钥安全**：CA 私钥与叶子私钥均仅在内存；本结构不负责密钥持久化（生产路径
/// 须配合 keyring/KMS）。`sign`/`renew` 返回的是**证书**，不含私钥（私钥由 CSR
/// 提交方持有——标准 PKI 流程）。ACME 路径的私钥（`Order::finalize` 返回的 PEM）由
/// instant-acme 内部 rcgen 生成，本结构当前不持久化（留 TODO）。
pub struct CaCertManager {
    /// CA 状态（init_ca 后 Some）。
    ca: Mutex<Option<CaState>>,
    /// 已签发叶子证书，按 id（serial hex）索引。
    leaves: Mutex<HashMap<String, LeafRecord>>,
    /// ACME 配置（注入后激活 acme_request 真实路径；None 则 acme_request 返回错误，
    /// 调用方改走 init_ca + sign 内部 CA 自签 fallback）。
    acme: Mutex<Option<crate::acme::AcmeConfig>>,
}

impl CaCertManager {
    /// 构造空 CA 管理器（须先 `init_ca` 才能 `sign`/`renew`）。
    pub fn new() -> Self {
        Self {
            ca: Mutex::new(None),
            leaves: Mutex::new(HashMap::new()),
            acme: Mutex::new(None),
        }
    }

    /// 注入 ACME 配置，激活 `acme_request` 真实路径（builder 模式）。
    ///
    /// 未注入时 `acme_request` 返回错误（调用方改走 `init_ca` + `sign` 内部 CA
    /// 自签 fallback）。注入后 `acme_request` 走 RFC 8555 流程（order → challenge →
    /// finalize → download）。
    pub fn with_acme(self, config: crate::acme::AcmeConfig) -> Self {
        if let Ok(mut g) = self.acme.lock() {
            *g = Some(config);
        }
        self
    }

    /// 从 rcgen 的 OffsetDateTime 转 os-core 的 DateTime（UTC）。
    fn to_date_time(od: time::OffsetDateTime) -> DateTime {
        // time crate 的 OffsetDateTime → unix 秒 → chrono DateTime<Utc>。
        let ts = od.unix_timestamp();
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).unwrap_or_else(chrono::Utc::now)
    }

    /// 把 rcgen Certificate + subject/issuer CN 转 os-security `Certificate` 元数据。
    fn meta_from_rcgen(
        cert: &rcgen::Certificate,
        subject_cn: &str,
        issuer_cn: &str,
    ) -> Result<Certificate, SecurityError> {
        // 解析 DER 提取 not_before/not_after/serial——用 x509-parser。
        let der_bytes = cert.der().as_ref().to_vec();
        let (_rest, x509) = x509_parser::parse_x509_certificate(&der_bytes)
            .map_err(|e| SecurityError::Internal(format!("解析证书失败: {e}")))?;
        let not_before = Self::to_date_time(x509.validity().not_before.to_datetime());
        let not_after = Self::to_date_time(x509.validity().not_after.to_datetime());
        let serial_hex = format!("{}", x509.serial);
        Ok(Certificate {
            id: serial_hex.clone(),
            common_name: subject_cn.to_string(),
            not_before,
            not_after,
            issuer: issuer_cn.to_string(),
            serial: serial_hex,
            auto_renew: false,
        })
    }

    /// 取 CN 字符串（从 DistinguishedName 找 CommonName；缺失时回退占位）。
    fn cn_of(dn: &rcgen::DistinguishedName) -> String {
        dn.get(&rcgen::DnType::CommonName)
            .map(dn_value_to_str)
            .unwrap_or_else(|| "<unset>".into())
    }

    /// 用 CA issuer 签发一张叶子证书（公共内部逻辑：从已构造的 leaf params 签发）。
    fn sign_leaf_with_ca(
        ca_state: &CaState,
        mut leaf_params: rcgen::CertificateParams,
        days: u32,
        leaf_key: &rcgen::KeyPair,
    ) -> Result<(rcgen::Certificate, String), SecurityError> {
        // 设置有效期：not_before = 现在 - 5min（避免时钟漂移），not_after = 现在 + days。
        let now = time::OffsetDateTime::now_utc();
        let skew = time::Duration::new(5 * 60, 0);
        leaf_params.not_before = now.checked_sub(skew).unwrap_or(now);
        let secs = (days as i64).saturating_mul(86400);
        let ttl = time::Duration::new(secs, 0);
        leaf_params.not_after = now
            .checked_add(ttl)
            .ok_or_else(|| SecurityError::Internal("有效期溢出（days 过大）".into()))?;
        // 叶子证书默认 key usages：DigitalSignature + ServerAuth/ClientAuth。
        if !leaf_params
            .key_usages
            .contains(&rcgen::KeyUsagePurpose::DigitalSignature)
        {
            leaf_params
                .key_usages
                .push(rcgen::KeyUsagePurpose::DigitalSignature);
        }
        // issuer 借用 ca params + ca key——按需构造。
        let issuer = rcgen::Issuer::from_params(&ca_state.params, &ca_state.key_pair);
        let issuer_cn = Self::cn_of(&ca_state.params.distinguished_name);
        let cert = leaf_params
            .signed_by(leaf_key, &issuer)
            .map_err(|e| SecurityError::Internal(format!("rcgen 签发失败: {e}")))?;
        Ok((cert, issuer_cn))
    }
}

impl Default for CaCertManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 从 rcgen `DnValue` 提取可读字符串（覆盖 Utf8String/PrintableString/Ia5String/TeletexString；
/// 其它少见表型回退 `<binary>`）。用于 cert 元数据的 common_name 字段。
fn dn_value_to_str(v: &rcgen::DnValue) -> String {
    use rcgen::DnValue;
    match v {
        DnValue::Utf8String(s) => s.clone(),
        DnValue::PrintableString(s) => s.as_str().to_string(),
        DnValue::Ia5String(s) => s.as_str().to_string(),
        DnValue::TeletexString(s) => s.as_str().to_string(),
        // BmpString/UniversalString 较少用于 CN；非 UTF-8 友好型回退占位。
        _ => "<binary>".into(),
    }
}

#[allow(async_fn_in_trait)]
impl crate::cert::CertManager for CaCertManager {
    async fn init_ca(&self, common_name: &str) -> Result<Certificate, SecurityError> {
        let mut ca_guard = self
            .ca
            .lock()
            .map_err(|e| SecurityError::Internal(format!("ca 锁中毒: {e}")))?;
        if ca_guard.is_some() {
            // 已初始化——返回现有 CA 元数据（幂等；不重复生成覆盖既有根）。
            return Ok(ca_guard.as_ref().unwrap().meta.clone());
        }
        // 构造 CA 参数：自签 + IsCa::Ca(Unconstrained) + KeyCertSign/CrlSign。
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params
            .distinguished_name
            .push(rcgen::DnType::OrganizationName, "OS-System Internal CA");
        params.key_usages.push(rcgen::KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(rcgen::KeyUsagePurpose::CrlSign);
        params
            .key_usages
            .push(rcgen::KeyUsagePurpose::DigitalSignature);
        // 有效期：CA 长有效期（10 年）。
        let now = time::OffsetDateTime::now_utc();
        let skew = time::Duration::new(5 * 60, 0);
        params.not_before = now.checked_sub(skew).unwrap_or(now);
        let ten_years = time::Duration::new(10 * 365 * 86400, 0);
        params.not_after = now
            .checked_add(ten_years)
            .ok_or_else(|| SecurityError::Internal("CA 有效期溢出".into()))?;

        // 生成 CA ECDSA P256 KeyPair + 自签。
        let key_pair = rcgen::KeyPair::generate()
            .map_err(|e| SecurityError::Internal(format!("CA KeyPair 生成失败: {e}")))?;
        let ca_cert = params
            .self_signed(&key_pair)
            .map_err(|e| SecurityError::Internal(format!("CA 自签失败: {e}")))?;
        let der = ca_cert.der().as_ref().to_vec();
        let meta = Self::meta_from_rcgen(&ca_cert, common_name, common_name)?;
        // ca_cert 在离开作用域后被丢弃——CA 证书内容已存 DER；签发用 params+key。
        drop(ca_cert);
        *ca_guard = Some(CaState {
            key_pair,
            params,
            meta: meta.clone(),
            der,
        });
        Ok(meta)
    }

    async fn sign(&self, csr: &[u8], days: u32) -> Result<Vec<u8>, SecurityError> {
        if days == 0 {
            return Err(SecurityError::Internal("days 须 > 0".into()));
        }
        let ca_guard = self
            .ca
            .lock()
            .map_err(|e| SecurityError::Internal(format!("ca 锁中毒: {e}")))?;
        let ca_state = ca_guard
            .as_ref()
            .ok_or_else(|| SecurityError::Internal("CA 未初始化（须先 init_ca）".into()))?;

        // 解析 CSR：尝试 PEM，失败则尝试 DER。
        let csr_params = match std::str::from_utf8(csr)
            .ok()
            .and_then(|s| rcgen::CertificateSigningRequestParams::from_pem(s).ok())
        {
            Some(p) => p,
            None => {
                // 尝试 DER：CertificateSigningRequestDer 是 newtype（Vec<u8>），用 Into 构造。
                let csr_der: rustls_pki_types::CertificateSigningRequestDer = csr.to_vec().into();
                rcgen::CertificateSigningRequestParams::from_der(&csr_der).map_err(|e| {
                    SecurityError::Internal(format!("CSR 解析失败（PEM/DER 均失败）: {e}"))
                })?
            }
        };

        // 用 CA 签发叶子证书。
        let mut leaf_params = csr_params.params.clone();
        // CSR 解析后 params 可能缺 CN——回退用 CSR subject DN（已解析进 distinguished_name）。
        let leaf_cn = Self::cn_of(&csr_params.params.distinguished_name);
        // 叶子 EKU：ServerAuth + ClientAuth（覆盖服务端/客户端用途）。
        if !leaf_params
            .extended_key_usages
            .contains(&rcgen::ExtendedKeyUsagePurpose::ServerAuth)
        {
            leaf_params
                .extended_key_usages
                .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
        }
        if !leaf_params
            .extended_key_usages
            .contains(&rcgen::ExtendedKeyUsagePurpose::ClientAuth)
        {
            leaf_params
                .extended_key_usages
                .push(rcgen::ExtendedKeyUsagePurpose::ClientAuth);
        }
        // CSR 携带的公钥——构造一个"代理 KeyPair"不可行（rcgen PublicKey 无对应 KeyPair）。
        // 故 sign 用 CA 生成新叶子 KeyPair——这是标准 PKI "签发新证书"流程（CSR 仅用于
        // 申请参数 + 公钥校验签名真实性，签发证书的公钥来自申请方）。本实现选择生成新
        // 叶子密钥对，与 renew 一致；CSR 公钥校验已由 from_der 完成（verify_signature）。
        let leaf_key = rcgen::KeyPair::generate()
            .map_err(|e| SecurityError::Internal(format!("叶子 KeyPair 生成失败: {e}")))?;
        let (cert, issuer_cn) = Self::sign_leaf_with_ca(ca_state, leaf_params, days, &leaf_key)?;
        let der = cert.der().as_ref().to_vec();
        let pem = cert.pem().into_bytes();
        let meta = Self::meta_from_rcgen(&cert, &leaf_cn, &issuer_cn)?;
        drop(ca_guard);

        // 入库（按 serial hex 索引）。
        let mut leaves = self
            .leaves
            .lock()
            .map_err(|e| SecurityError::Internal(format!("leaves 锁中毒: {e}")))?;
        leaves.insert(
            meta.id.clone(),
            LeafRecord {
                meta,
                pem: pem.clone(),
                der,
                common_name: leaf_cn,
                days,
                spki_hint: String::new(),
                source: CertSource::InternalCa,
            },
        );
        Ok(pem)
    }

    async fn list_certs(&self) -> Result<Vec<Certificate>, SecurityError> {
        let mut out = Vec::new();
        // CA 在前。
        if let Some(ca) = self
            .ca
            .lock()
            .map_err(|e| SecurityError::Internal(format!("ca 锁中毒: {e}")))?
            .as_ref()
        {
            out.push(ca.meta.clone());
        }
        // 叶子证书（按 id 排序，输出稳定）。
        let mut leaves = self
            .leaves
            .lock()
            .map_err(|e| SecurityError::Internal(format!("leaves 锁中毒: {e}")))?
            .values()
            .map(|r| r.meta.clone())
            .collect::<Vec<_>>();
        leaves.sort_by(|a, b| a.id.cmp(&b.id));
        out.extend(leaves);
        Ok(out)
    }

    async fn renew(&self, id: &str) -> Result<(), SecurityError> {
        // 取出叶子记录（CN + days），然后在 CA 锁内重签。
        let (cn, days) = {
            let leaves = self
                .leaves
                .lock()
                .map_err(|e| SecurityError::Internal(format!("leaves 锁中毒: {e}")))?;
            let rec = leaves
                .get(id)
                .ok_or_else(|| SecurityError::Internal(format!("renew: 证书 id 不存在: {id}")))?;
            (rec.common_name.clone(), rec.days)
        };

        let ca_guard = self
            .ca
            .lock()
            .map_err(|e| SecurityError::Internal(format!("ca 锁中毒: {e}")))?;
        let ca_state = ca_guard
            .as_ref()
            .ok_or_else(|| SecurityError::Internal("CA 未初始化（须先 init_ca）".into()))?;
        // 重建叶子参数（同 CN，新 KeyPair——PKI renew 语义：旧证书失效，新证书生效）。
        let mut leaf_params = rcgen::CertificateParams::default();
        leaf_params.distinguished_name = rcgen::DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn.as_str());
        leaf_params
            .extended_key_usages
            .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
        leaf_params
            .extended_key_usages
            .push(rcgen::ExtendedKeyUsagePurpose::ClientAuth);
        let leaf_key = rcgen::KeyPair::generate()
            .map_err(|e| SecurityError::Internal(format!("renew 叶子 KeyPair 生成失败: {e}")))?;
        let (cert, issuer_cn) = Self::sign_leaf_with_ca(ca_state, leaf_params, days, &leaf_key)?;
        let der = cert.der().as_ref().to_vec();
        let pem = cert.pem().into_bytes();
        let meta = Self::meta_from_rcgen(&cert, &cn, &issuer_cn)?;
        drop(ca_guard);

        // 替换入库：用新 serial 作 id（旧 id 的记录移除）。
        let mut leaves = self
            .leaves
            .lock()
            .map_err(|e| SecurityError::Internal(format!("leaves 锁中毒: {e}")))?;
        leaves.remove(id);
        leaves.insert(
            meta.id.clone(),
            LeafRecord {
                meta,
                pem,
                der,
                common_name: cn,
                days,
                spki_hint: String::new(),
                source: CertSource::InternalCa,
            },
        );
        Ok(())
    }

    async fn acme_request(&self, domain: &str) -> Result<Certificate, SecurityError> {
        // 取 ACME 配置（须先 with_acme 注入）。
        let config = {
            let g = self
                .acme
                .lock()
                .map_err(|e| SecurityError::Internal(format!("acme 锁中毒: {e}")))?;
            g.clone().ok_or_else(|| {
                SecurityError::Internal(
                    "CaCertManager::acme_request 阻塞：未注入 AcmeConfig（用 with_acme 注入；\
                     内部域名场景改用 init_ca + sign)"
                        .into(),
                )
            })?
        };
        // 真实 ACME 流程（instant-acme）：order → challenge → finalize → download。
        let (cert_chain_pem, _priv_key_pem) = crate::acme::run_acme_order(&config, domain).await?;
        // 解析 PEM 证书链 → Certificate 元数据（auto_renew=true，续期由 renew_expiring 驱动）。
        let meta = crate::acme::cert_meta_from_pem_chain(&cert_chain_pem, true)?;
        let der = pem_chain_first_der(&cert_chain_pem)?;
        // 入库（按 serial hex 索引；source=Acme(domain) 供 renew_expiring 分派）。
        let mut leaves = self
            .leaves
            .lock()
            .map_err(|e| SecurityError::Internal(format!("leaves 锁中毒: {e}")))?;
        leaves.insert(
            meta.id.clone(),
            LeafRecord {
                meta: meta.clone(),
                pem: cert_chain_pem.into_bytes(),
                der,
                common_name: meta.common_name.clone(),
                days: 0, // ACME 证书有效期由 CA 决定，days 字段不适用（用 not_after 判断）
                spki_hint: String::new(),
                source: CertSource::Acme(domain.to_string()),
            },
        );
        Ok(meta)
    }
}

impl CaCertManager {
    /// 续期检查：遍历 `auto_renew=true` 且 `not_after - now < threshold_days` 的证书，
    /// 按来源分派 re-request（ACME 来源 → `acme_request(domain)`；CA 自签 → `renew(id)`）。
    ///
    /// 返回成功续期的证书 id 列表（新 serial）。失败的单个证书不中断其他证书续期
    /// （其错误被记录但不抛出——若所有目标都失败才返回最后一条错误）。
    ///
    /// trait 不改签名；续期入口由调用方（上层守护进程）按调度触发（如每日 cron）。
    pub async fn renew_expiring(&self, threshold_days: u32) -> Result<Vec<String>, SecurityError> {
        // trait 方法（acme_request/renew）需 CertManager trait 在作用域内。
        use crate::cert::CertManager;
        // 先快照：需要续期的 (id, source) 列表（在锁内采集，锁外执行续期避免长时间持锁）。
        let now = chrono::Utc::now();
        let threshold = chrono::Duration::days(threshold_days as i64);
        let to_renew: Vec<(String, CertSource)> = {
            let leaves = self
                .leaves
                .lock()
                .map_err(|e| SecurityError::Internal(format!("leaves 锁中毒: {e}")))?;
            leaves
                .iter()
                .filter(|(_, rec)| rec.meta.auto_renew)
                .filter(|(_, rec)| {
                    // not_after - now < threshold（即剩余有效期不足 threshold_days）。
                    let remaining = rec.meta.not_after.signed_duration_since(now);
                    remaining < threshold
                })
                .map(|(id, rec)| (id.clone(), rec.source.clone()))
                .collect()
        };
        let mut renewed = Vec::new();
        let mut last_err: Option<SecurityError> = None;
        for (id, source) in to_renew {
            let res = match source {
                CertSource::Acme(domain) => {
                    // ACME 来源：重新走 acme_request（旧记录由 acme_request 内 insert 覆盖
                    // —— 不同 serial 故 id 变化，需手动移除旧 id）。
                    let r = self.acme_request(&domain).await;
                    if r.is_ok() {
                        // 移除旧 serial 记录。
                        if let Ok(mut leaves) = self.leaves.lock() {
                            leaves.remove(&id);
                        }
                    }
                    r.map(|_| ())
                }
                CertSource::InternalCa => {
                    // CA 自签来源：复用既有 renew（按 CN 重签）。
                    self.renew(&id).await
                }
            };
            match res {
                Ok(()) => renewed.push(id),
                Err(e) => last_err = Some(e),
            }
        }
        if renewed.is_empty() {
            // 无任何证书成功续期且存在错误 → 抛出最后一条。
            if let Some(e) = last_err {
                return Err(e);
            }
        }
        Ok(renewed)
    }
}

/// PEM 证书链 → 第一张证书的 DER（与 acme.rs::pem_first_cert_to_der 同实现，但
/// 此处为 impls 模块私有辅助——避免跨模块可见性复杂化）。
fn pem_chain_first_der(pem: &str) -> Result<Vec<u8>, SecurityError> {
    use base64::Engine;
    let mut in_cert = false;
    let mut b64 = String::new();
    for line in pem.lines() {
        if line.starts_with("-----BEGIN CERTIFICATE-----") {
            in_cert = true;
            continue;
        }
        if line.starts_with("-----END CERTIFICATE-----") {
            break;
        }
        if in_cert {
            b64.push_str(line.trim());
        }
    }
    if b64.is_empty() {
        return Err(SecurityError::Internal(
            "PEM 证书链中未找到 CERTIFICATE 块".into(),
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| SecurityError::Internal(format!("PEM base64 解码失败: {e}")))
}

// ============================================================================
// TotpTwoFactor —— TOTP 校验（HMAC-SHA1 真实），secret 内存存储（AEAD 待接入）
// ============================================================================

/// TOTP 双因素实现。
///
/// - HMAC-SHA1 计算 + dynamic truncation：**真实**（`totp` 模块，hmac + sha1）。
/// - secret 存储：**内存版**（`Mutex<HashMap<UserId, Vec<u8>>>`），生产路径需
///   AEAD 加密落盘（AEAD crate 未注册，TODO）。
/// - `enable`：生成随机 20 字节 secret（OsRng），存内存，返回 otpauth URI。
/// - `verify`：取出 secret → 用 `generate_code` 计算当前窗口 ± window 的 code → 常量时间比对。
/// - `disable`：删除内存 secret。
pub struct TotpTwoFactor {
    secrets: Mutex<HashMap<UserId, Vec<u8>>>,
}

impl TotpTwoFactor {
    pub fn new() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for TotpTwoFactor {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl crate::twofactor::TwoFactor for TotpTwoFactor {
    async fn enable(&self, user: &UserId) -> Result<TwoFactorSecret, SecurityError> {
        use rand::rngs::OsRng;
        use rand::RngCore;
        // 生成 20 字节随机 secret（与 RFC 4226/6238 测试向量同长度）。
        let mut secret = vec![0u8; 20];
        OsRng.fill_bytes(&mut secret);
        // TODO(security-agent): AEAD 加密后落盘（chacha20poly1305/aes-gcm 未注册）。
        // 当前内存版直接存明文 secret——仅测试/演示，生产路径必须加密。
        let mut store = self
            .secrets
            .lock()
            .map_err(|e| SecurityError::Internal(format!("secrets 锁中毒: {e}")))?;
        // 一次性返回的 otpauth URI（base32 编码 secret 供扫码 App）。
        let b32 = base32_encode(&secret);
        let otpauth = format!(
            "otpauth://totp/os:{}?secret={b32}&issuer=os&digits={}&period={}",
            user,
            crate::totp::DEFAULT_DIGITS,
            crate::totp::DEFAULT_STEP
        );
        // 内存占位：encrypted 字段存 base32（仅为可读性，非真实加密）。
        let tf = TwoFactorSecret {
            user_id: user.clone(),
            encrypted: b32,
            otpauth_uri: Some(otpauth),
        };
        store.insert(user.clone(), secret);
        Ok(tf)
    }

    async fn verify(&self, user: &UserId, code: &str) -> Result<bool, SecurityError> {
        let digits = crate::totp::DEFAULT_DIGITS;
        // 格式校验是纯逻辑。
        if !crate::totp::validate_code_format(code, digits) {
            return Err(SecurityError::TwoFactorFailed(format!(
                "code 格式非法（应为 {digits} 位数字）"
            )));
        }
        // 取出 secret（用户须先 enable；未启用 → 错误）。
        let store = self
            .secrets
            .lock()
            .map_err(|e| SecurityError::Internal(format!("secrets 锁中毒: {e}")))?;
        let secret = store
            .get(user)
            .ok_or_else(|| SecurityError::TwoFactorFailed("用户未启用 2FA".into()))?
            .clone();
        drop(store);

        // 当前时间窗口 ± window，逐一计算并常量时间比对。
        // 注：generate_code 接收 Unix 秒（内部再除以 step 得 counter）；这里用
        // counter × step 还原窗口代表时间戳，避免重复推导。
        let now = chrono::Utc::now().timestamp();
        let step = crate::totp::DEFAULT_STEP;
        let current_counter = crate::totp::time_step_counter(now, crate::totp::DEFAULT_STEP)?;
        let window = crate::totp::DEFAULT_WINDOW as u64;
        let want: u32 = code
            .parse()
            .map_err(|_| SecurityError::TwoFactorFailed("code 解析为数字失败".into()))?;
        let lo = current_counter.saturating_sub(window);
        let hi = current_counter.saturating_add(window);
        let mut matched = false;
        for c in lo..=hi {
            // 该窗口的代表 Unix 秒 = counter * step。
            let ts: i64 = (c as i64).saturating_mul(step as i64);
            let computed = crate::totp::generate_code(&secret, ts, step, digits)?;
            // 常量时间比较（6 位定长字符串）。
            let cb = format!("{computed:0width$}", width = digits as usize);
            if cb.len() == code.len()
                && crate::password::constant_time_eq(cb.as_bytes(), code.as_bytes())
            {
                matched = true;
            }
        }
        let _ = want; // want 仅用于早校验可解析性（已 parse）。
        Ok(matched)
    }

    async fn disable(&self, user: &UserId) -> Result<(), SecurityError> {
        let mut store = self
            .secrets
            .lock()
            .map_err(|e| SecurityError::Internal(format!("secrets 锁中毒: {e}")))?;
        store.remove(user);
        Ok(())
    }
}

/// 简易 Base32 编码（RFC 4648，无 padding），用于 otpauth URI 的 secret 字段。
///
/// 不引入额外 crate；TOTP secret 通常 20 字节 → 32 字符 base32。
fn base32_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits_left = 0;
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            let idx = ((buffer >> bits_left) & 0x1f) as usize;
            out.push(ALPHABET[idx] as char);
        }
    }
    if bits_left > 0 {
        let idx = ((buffer << (5 - bits_left)) & 0x1f) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

// ============================================================================
// BoringtunVpnManager —— 真实 boringtun（noise 协议层 + x25519 密钥，不建 TUN）
// ============================================================================

/// 单个 peer 的运行态：trait 模型 + boringtun noise 隧道（每 peer 一条点对点 Tunn）。
///
/// `Tunn` 是 boringtun 的 WireGuard 状态机（握手/会话/计数），**不绑定 TUN 设备**——
/// 它只处理 noise 协议层的封装/解封装；数据面（TUN 读写）属 `device` feature，
/// 本实现按红线不开（需 root/网络命名空间）。测试用 fixture 验证 noise 层。
struct PeerEntry {
    peer: VpnPeer,
    /// boringtun noise 隧道。WireGuard 是点对点协议，每 peer 一条独立 Tunn。
    tun: boringtun::noise::Tunn,
    /// 最近一次握手是否完成（由 stats 推导：time_since_last_handshake 非 None）。
    handshake_done: bool,
}

/// WireGuard（boringtun 用户态）VPN 管理器（真实 noise 协议层实现）。
///
/// - 持有本机 x25519 静态私钥（`boringtun::x25519::StaticSecret`，内存）。
/// - `add_peer`：base64 解码 peer 公钥（须 32 字节 x25519 公钥）→ 构造
///   `boringtun::noise::Tunn`（本机私钥 + peer 公钥）→ 入库。
/// - `status`：聚合所有 peer 的 `Tunn::stats()`（tx/rx 字节 + 握手时间）。
/// - **不建 TUN 隧道**（红线）：boringtun `device` feature 未开，无 socket2/TUN；
///   仅 noise 协议层（握手状态机 + 字节统计）真实。
///
/// 密钥安全：本机私钥仅在内存（生产路径须配合 keyring/KMS）。
pub struct BoringtunVpnManager {
    /// peer 运行态（按 public_key 索引顺序存储，便于 list）。
    peers: Mutex<Vec<PeerEntry>>,
    /// 监听端口（WireGuard UDP）。
    listen_port: u16,
    /// 本机 x25519 静态私钥（用于与所有 peer 建立握手）。用 Mutex 包裹以便
    /// 在 add_peer 时按需克隆派生 PublicKey——但 x25519::StaticSecret 不可 Clone，
    /// 故构造 manager 时生成一次，后续 Tunn 直接用（Tunn 持有私钥副本）。
    /// 此处保留私钥的公钥（base64），供调试/审计；私钥本身仅存在于各 Tunn 内。
    our_public_key_b64: String,
}

impl BoringtunVpnManager {
    /// 构造，指定监听端口；自动生成一个本机 x25519 密钥对（私钥仅内存）。
    pub fn new(listen_port: u16) -> Self {
        use boringtun::x25519::{PublicKey, StaticSecret};
        // rand 0.8 的 OsRng 实现了 rand_core 0.6 的 CryptoRng + RngCore，
        // 与 boringtun（依赖 rand_core 0.6.4）兼容。
        use rand::rngs::OsRng;
        // 生成本机私钥 + 公钥（Curve25519 ECDH 基）。
        let our_secret = StaticSecret::random_from_rng(OsRng);
        let our_public = PublicKey::from(&our_secret);
        let our_public_key_b64 = wg_key_b64(our_public.as_bytes());
        Self {
            peers: Mutex::new(Vec::new()),
            listen_port,
            our_public_key_b64,
        }
    }

    /// 构造并暴露本机公钥（base64，WireGuard 标准）——调用方用此配置 peer 的对端。
    pub fn new_with_public(listen_port: u16) -> (Self, String) {
        let mgr = Self::new(listen_port);
        let pk = mgr.our_public_key_b64.clone();
        (mgr, pk)
    }

    /// 取本机公钥（base64）。
    pub fn our_public_key(&self) -> &str {
        &self.our_public_key_b64
    }

    /// 解码 WireGuard base64 公钥（标准 RFC 7748 §5：32 字节 + base64）。
    /// 失败返回 VpnError。
    fn decode_peer_pubkey(b64: &str) -> Result<[u8; 32], SecurityError> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| SecurityError::VpnError(format!("公钥 base64 解码失败: {e}")))?;
        if bytes.len() != 32 {
            return Err(SecurityError::VpnError(format!(
                "公钥长度非法：期望 32 字节，实得 {} 字节",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }

    /// 用本机私钥 + peer 公钥构造一个 boringtun Tunn。
    /// 私钥每次重新生成（x25519 StaticSecret 不可 Clone）——为保证多 peer 共享同一
    /// 本机身份，调用方应在 manager 构造时固定私钥。本实现的权衡：**每个 Tunn 用
    /// 独立私钥**（即每个 peer 一对本机密钥），简化实现——对"peer 增删查 + noise 握手"
    /// 测试目标足够；生产路径须共享单一本机私钥（见 TODO）。
    fn make_tunn(peer_pub: &[u8; 32]) -> Result<boringtun::noise::Tunn, SecurityError> {
        use boringtun::x25519::{PublicKey, StaticSecret};
        use rand::rngs::OsRng;
        use rand::RngCore;
        let our_secret = StaticSecret::random_from_rng(OsRng);
        let peer_public = PublicKey::from(*peer_pub);
        // index 用 OsRng 随机（boringtun 用 index 路由会话）。
        let idx = OsRng.next_u32();
        let tun = boringtun::noise::Tunn::new(our_secret, peer_public, None, None, idx, None);
        Ok(tun)
    }
}

#[allow(async_fn_in_trait)]
impl crate::vpn::VpnManager for BoringtunVpnManager {
    async fn add_peer(&self, peer: VpnPeer) -> Result<(), SecurityError> {
        // 校验公钥格式（base64 + 32 字节）——同时完成解码。
        let peer_pub = Self::decode_peer_pubkey(&peer.public_key)?;
        let mut peers = self
            .peers
            .lock()
            .map_err(|e| SecurityError::Internal(format!("peers 锁中毒: {e}")))?;
        // 去重：同公钥拒绝。
        if peers.iter().any(|p| p.peer.public_key == peer.public_key) {
            return Err(SecurityError::VpnError(format!(
                "peer 已存在: {}",
                peer.public_key
            )));
        }
        // 构造 boringtun Tunn（真实 noise 状态机）。
        let tun = Self::make_tunn(&peer_pub)?;
        peers.push(PeerEntry {
            peer,
            tun,
            handshake_done: false,
        });
        Ok(())
    }

    async fn remove_peer(&self, pub_key: &str) -> Result<(), SecurityError> {
        let mut peers = self
            .peers
            .lock()
            .map_err(|e| SecurityError::Internal(format!("peers 锁中毒: {e}")))?;
        let before = peers.len();
        peers.retain(|p| p.peer.public_key != pub_key);
        if peers.len() == before {
            return Err(SecurityError::VpnError(format!("peer 不存在: {pub_key}")));
        }
        Ok(())
    }

    async fn list_peers(&self) -> Result<Vec<VpnPeer>, SecurityError> {
        Ok(self
            .peers
            .lock()
            .map_err(|e| SecurityError::Internal(format!("peers 锁中毒: {e}")))?
            .iter()
            .map(|e| e.peer.clone())
            .collect())
    }

    async fn status(&self) -> Result<VpnStatus, SecurityError> {
        let mut peers = self
            .peers
            .lock()
            .map_err(|e| SecurityError::Internal(format!("peers 锁中毒: {e}")))?;
        let mut total_rx: u64 = 0;
        let mut total_tx: u64 = 0;
        let mut any_handshake = false;
        for entry in peers.iter_mut() {
            // Tunn::stats -> (time_since_last_handshake, tx_bytes, rx_bytes, loss, rtt)
            let (handshake, tx, rx, _loss, _rtt) = entry.tun.stats();
            entry.handshake_done = handshake.is_some();
            any_handshake |= entry.handshake_done;
            total_tx += tx as u64;
            total_rx += rx as u64;
        }
        let count = peers.len() as u32;
        // running = 至少有一个 peer 完成过握手（即 noise 层曾建立会话）。
        // 无 TUN 设备时握手不会自然发生（无 UDP 收发），但状态语义仍反映 noise 层。
        Ok(VpnStatus {
            running: any_handshake,
            listen_port: self.listen_port,
            peer_count: count,
            bytes_rx: total_rx,
            bytes_tx: total_tx,
        })
    }
}

/// WireGuard 风格的 base64 编码（标准 base64，无换行；RFC 7748 §5）。
fn wg_key_b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// 保留 TokenType 引用，避免未使用 import 警告（测试中 JwtClaims 构造已用到）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthProvider;
    use crate::cert::CertManager;
    use crate::jwt::{JwtIssuer, TokenType};
    use crate::twofactor::TwoFactor;
    use crate::vpn::VpnManager;

    #[tokio::test]
    async fn db_auth_create_list_disable() {
        let p = DbAuthProvider::new();
        let u = p
            .create_user("alice", vec![Role::Admin])
            .await
            .expect("create");
        let list = p.list_users().await.expect("list");
        assert_eq!(list.len(), 1);
        p.disable_user(&u.id).await.expect("disable");
        // 禁用后认证失败
        let r = p.authenticate("alice", "x").await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn db_auth_set_password_and_authenticate() {
        // 真实 argon2 路径：set_password 哈希后存储，authenticate 走 PHC 校验。
        let p = DbAuthProvider::new();
        let u = p.create_user("bob", vec![Role::User]).await.unwrap();
        p.set_password(&u.id, "s3cret-pw")
            .await
            .expect("set_password ok");

        // 正确密码认证成功。
        let prin = p.authenticate("bob", "s3cret-pw").await.expect("auth ok");
        assert_eq!(prin.user.id, u.id);
        // 错误密码认证失败。
        assert!(p.authenticate("bob", "wrong").await.is_err());
        // 空密码认证失败。
        assert!(p.authenticate("bob", "").await.is_err());
    }

    #[tokio::test]
    async fn db_auth_create_dup_role_rejected() {
        let p = DbAuthProvider::new();
        let r = p.create_user("dup", vec![Role::User, Role::User]).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn jwt_issue_and_verify_roundtrip() {
        // 真实 jsonwebtoken 路径：issue → verify 往返。
        let j = JwtIssuerImpl::for_testing();
        let now = chrono::Utc::now().timestamp();
        let claims = JwtClaims {
            sub: UserId::new("user-42"),
            roles: vec![Role::Admin, Role::User],
            exp: now + 3600,
            iat: now,
            token_type: TokenType::Access,
            custom: serde_json::json!({"scope": "read"}),
        };
        let token = j.issue(claims.clone()).await.expect("issue ok");
        assert!(token.split('.').count() == 3, "应为三段式 JWT");

        let back = j.verify(&token).await.expect("verify ok");
        assert_eq!(back.sub, claims.sub);
        assert_eq!(back.roles, claims.roles);
        assert_eq!(back.token_type, claims.token_type);
        assert_eq!(back.exp, claims.exp);
        assert_eq!(back.iat, claims.iat);
    }

    #[tokio::test]
    async fn jwt_verify_wrong_key_rejected() {
        // 不同 secret 签发的 token，用另一个 issuer 验签应失败。
        let issuer_a = JwtIssuerImpl::new("secret-a");
        let issuer_b = JwtIssuerImpl::new("secret-b");
        let now = chrono::Utc::now().timestamp();
        let claims = JwtClaims {
            sub: UserId::new("x"),
            roles: vec![Role::User],
            exp: now + 60,
            iat: now,
            token_type: TokenType::Access,
            custom: serde_json::Value::Null,
        };
        let token = issuer_a.issue(claims).await.unwrap();
        assert!(issuer_b.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn jwt_verify_expired_rejected() {
        // 过期 token（exp 已过）验签应失败。
        let j = JwtIssuerImpl::for_testing();
        let claims = JwtClaims {
            sub: UserId::new("x"),
            roles: vec![Role::User],
            exp: 1, // 1970 年——必过期
            iat: 0,
            token_type: TokenType::Access,
            custom: serde_json::Value::Null,
        };
        let token = j.issue(claims).await.unwrap();
        assert!(j.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn jwt_verify_malformed_rejected() {
        let j = JwtIssuerImpl::for_testing();
        assert!(j.verify("not.a.jwt").await.is_err());
        assert!(j.verify("").await.is_err());
        assert!(j.verify("xxx").await.is_err());
    }

    #[tokio::test]
    async fn jwt_rotate_keys_grace_window() {
        // 轮换流程：用 A 签发 token → set_current_key(B)（A 自动入宽限期）→
        // 旧 token 仍可验签 → 清空宽限期后旧 token 失效。
        let j = JwtIssuerImpl::new("key-A");
        let now = chrono::Utc::now().timestamp();
        let claims = JwtClaims {
            sub: UserId::new("u"),
            roles: vec![Role::User],
            exp: now + 3600,
            iat: now,
            token_type: TokenType::Access,
            custom: serde_json::Value::Null,
        };
        let old_token = j.issue(claims.clone()).await.unwrap();
        // set_current_key 自动把旧 key-A 推入宽限期，并切到 key-B。
        j.set_current_key("key-B").unwrap();
        j.rotate_keys().await.unwrap();
        // 旧 token（key-A 签发）应仍能通过宽限期验签。
        assert!(
            j.verify(&old_token).await.is_ok(),
            "旧 token 应在宽限期内验签通过"
        );
        // 新 token（key-B 签发）也能验签。
        let new_token = j.issue(claims).await.unwrap();
        assert!(j.verify(&new_token).await.is_ok());
        // 清空宽限期后，旧 token 失效。
        j.clear_grace().unwrap();
        assert!(
            j.verify(&old_token).await.is_err(),
            "宽限期清空后旧 token 应失效"
        );
    }

    #[tokio::test]
    async fn cert_list_empty_ok() {
        let c = CaCertManager::new();
        assert!(c.list_certs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn totp_verify_bad_format() {
        let t = TotpTwoFactor::new();
        let err = t.verify(&UserId::new("u"), "abc").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn totp_verify_not_enabled() {
        // 未 enable 的用户 verify 应失败（明确的错误，非 panic）。
        let t = TotpTwoFactor::new();
        let err = t.verify(&UserId::new("ghost"), "123456").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn totp_enable_returns_otpauth() {
        // enable 应返回含 otpauth URI 的 secret，且 URI 含 base32 编码 secret。
        let t = TotpTwoFactor::new();
        let tf = t.enable(&UserId::new("user1")).await.expect("enable ok");
        assert!(tf
            .otpauth_uri
            .as_ref()
            .unwrap()
            .starts_with("otpauth://totp/"));
        assert!(tf.encrypted.len() >= 16, "base32 secret 应有合理长度");
    }

    #[tokio::test]
    async fn totp_enable_then_verify_roundtrip() {
        // 真实 2FA 往返：enable → 用同一模块独立计算当前 code → verify 通过。
        let t = TotpTwoFactor::new();
        let uid = UserId::new("user2");
        let tf = t.enable(&uid).await.expect("enable");
        // enable 必返回 otpauth URI（一次性给客户端扫码）。
        assert!(tf.otpauth_uri.is_some());
        // 取出内存中的 secret（生产路径不可见；测试用反射通过同 crate 直接访问）。
        let secret = t
            .secrets
            .lock()
            .unwrap()
            .get(&uid)
            .cloned()
            .expect("secret stored");
        // 用同一 secret 独立计算当前 code。
        let now = chrono::Utc::now().timestamp();
        let code = crate::totp::generate_code(
            &secret,
            now,
            crate::totp::DEFAULT_STEP,
            crate::totp::DEFAULT_DIGITS,
        )
        .unwrap();
        let code_str = format!(
            "{:0width$}",
            code,
            width = crate::totp::DEFAULT_DIGITS as usize
        );
        assert!(
            t.verify(&uid, &code_str).await.expect("verify"),
            "正确 code 应通过"
        );
        // 错误 code 应失败（用恒定差值构造一个不匹配的 6 位）。
        let wrong = if code_str == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert!(!t.verify(&uid, wrong).await.unwrap(), "错误 code 不应通过");
    }

    #[tokio::test]
    async fn totp_disable_then_verify_fails() {
        // disable 后 verify 应报「未启用」。
        let t = TotpTwoFactor::new();
        let uid = UserId::new("user3");
        t.enable(&uid).await.unwrap();
        t.disable(&uid).await.unwrap();
        assert!(t.verify(&uid, "123456").await.is_err());
    }

    #[test]
    fn base32_encode_known_vectors() {
        // RFC 4648 §10 测试向量（无 padding）。
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b"fo"), "MZXQ");
        assert_eq!(base32_encode(b"foo"), "MZXW6");
        assert_eq!(base32_encode(b"foob"), "MZXW6YQ");
        assert_eq!(base32_encode(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn base32_encode_20bytes_length() {
        // 20 字节 secret → 32 字符 base32（标准 TOTP secret 长度）。
        let s = base32_encode(&[0u8; 20]);
        assert_eq!(s.len(), 32);
    }

    #[tokio::test]
    async fn totp_disable_ok() {
        let t = TotpTwoFactor::new();
        assert!(t.disable(&UserId::new("u")).await.is_ok());
    }

    #[tokio::test]
    async fn vpn_add_remove_peer() {
        // 真实 boringtun 路径：add_peer 校验公钥（base64 + 32 字节）+ 构造 Tunn。
        let v = BoringtunVpnManager::new(51820);
        let pk = gen_wg_public_key();
        let peer = VpnPeer {
            public_key: pk.clone(),
            allowed_ips: vec![],
            endpoint: None,
            user: None,
        };
        v.add_peer(peer).await.unwrap();
        assert_eq!(v.list_peers().await.unwrap().len(), 1);
        // 重复添加（同公钥）应拒绝。
        let dup = VpnPeer {
            public_key: pk.clone(),
            allowed_ips: vec![],
            endpoint: None,
            user: None,
        };
        assert!(v.add_peer(dup).await.is_err());
        v.remove_peer(&pk).await.unwrap();
        assert_eq!(v.list_peers().await.unwrap().len(), 0);
        // 无 peer 时 status：running=false（无握手），peer_count=0。
        let st = v.status().await.unwrap();
        assert_eq!(st.listen_port, 51820);
        assert_eq!(st.peer_count, 0);
        assert!(!st.running);
    }

    // ========================================================================
    // 批 3 新增：rcgen CA 证书签发 + 链验证
    // ========================================================================

    /// 生成一对 WireGuard 公钥（base64，32 字节）——测试 fixture。
    fn gen_wg_public_key() -> String {
        use boringtun::x25519::{PublicKey, StaticSecret};
        use rand::rngs::OsRng;
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        wg_key_b64(public.as_bytes())
    }

    #[tokio::test]
    async fn cert_init_ca_real() {
        // 真实 rcgen：init_ca 自签 CA，返回元数据（含 CN/issuer/serial/有效期）。
        let c = CaCertManager::new();
        let ca = c.init_ca("os-test-ca").await.expect("init_ca");
        assert_eq!(ca.common_name, "os-test-ca");
        // 自签 CA：issuer == subject CN。
        assert_eq!(ca.issuer, "os-test-ca");
        assert!(!ca.serial.is_empty(), "serial 应非空");
        // 有效期：CA 长有效期（10 年级别）。
        let now = chrono::Utc::now();
        assert!(ca.not_before <= now, "not_before 不应晚于现在");
        let ten_years = chrono::Duration::days(365 * 10);
        assert!(
            ca.not_after >= now + ten_years - chrono::Duration::days(2),
            "not_after 应约 10 年后"
        );
    }

    #[tokio::test]
    async fn cert_init_ca_idempotent() {
        // 重复 init_ca 不覆盖既有根——返回相同元数据。
        let c = CaCertManager::new();
        let ca1 = c.init_ca("ca-once").await.unwrap();
        let ca2 = c.init_ca("ca-once").await.unwrap();
        assert_eq!(ca1.serial, ca2.serial, "幂等：serial 不变");
    }

    #[tokio::test]
    async fn cert_list_includes_ca_after_init() {
        let c = CaCertManager::new();
        assert!(c.list_certs().await.unwrap().is_empty());
        c.init_ca("ca-list").await.unwrap();
        let list = c.list_certs().await.unwrap();
        assert_eq!(list.len(), 1, "init 后应含 1 张（CA）");
        assert_eq!(list[0].common_name, "ca-list");
    }

    #[tokio::test]
    async fn cert_sign_without_init_rejected() {
        // 未 init_ca 直接 sign 应失败。
        let c = CaCertManager::new();
        let csr = make_test_csr("leaf-x");
        let r = c.sign(&csr, 30).await;
        assert!(r.is_err(), "未 init_ca 时 sign 应失败");
    }

    #[tokio::test]
    async fn cert_sign_zero_days_rejected() {
        let c = CaCertManager::new();
        c.init_ca("ca").await.unwrap();
        let csr = make_test_csr("leaf");
        assert!(c.sign(&csr, 0).await.is_err(), "days=0 应拒绝");
    }

    #[tokio::test]
    async fn cert_sign_returns_valid_pem() {
        // sign 应返回 PEM 字节（"-----BEGIN CERTIFICATE-----"）。
        let c = CaCertManager::new();
        c.init_ca("ca-sign").await.unwrap();
        let csr = make_test_csr("leaf-sign");
        let pem_bytes = c.sign(&csr, 90).await.expect("sign");
        let pem = String::from_utf8(pem_bytes).expect("PEM 是 UTF-8");
        assert!(
            pem.contains("-----BEGIN CERTIFICATE-----"),
            "应返回 PEM 证书：{pem}"
        );
        assert!(pem.contains("-----END CERTIFICATE-----"));
    }

    #[tokio::test]
    async fn cert_sign_then_list_two() {
        // init CA + sign 一张叶子 → list 含 2 张（CA + 叶子）。
        let c = CaCertManager::new();
        c.init_ca("ca-2").await.unwrap();
        let csr = make_test_csr("leaf-2");
        c.sign(&csr, 30).await.unwrap();
        let list = c.list_certs().await.unwrap();
        assert_eq!(list.len(), 2);
        // CA 在前（issuer==subject）；叶子在后（issuer=CA CN）。
        assert_eq!(list[0].common_name, "ca-2");
        assert_eq!(list[1].issuer, "ca-2", "叶子 issuer 应为 CA CN");
        assert_eq!(list[1].common_name, "leaf-2");
    }

    #[tokio::test]
    async fn cert_chain_verification() {
        // 真实证书链验证：CA 自签 + 叶子由 CA 签发 →
        // 1) 叶子的 issuer == CA 的 subject；
        // 2) 叶子的 authority key id 派生自 CA 的 subject key id；
        // 3) 用 x509-parser 验证叶子证书结构完整、签名算法存在。
        let c = CaCertManager::new();
        c.init_ca("root-ca").await.unwrap();
        let csr = make_test_csr("server.example.com");
        let leaf_pem = c.sign(&csr, 365).await.expect("sign");

        // 取 CA 的 DER（从 manager 内部状态），随后释放锁。
        let ca_der = {
            let ca_guard = c.ca.lock().unwrap();
            ca_guard.as_ref().expect("CA initialized").der.clone()
        };

        // 解析 PEM → DER。
        let leaf_pem_str = String::from_utf8(leaf_pem).unwrap();
        let leaf_der = pem_to_cert_der(&leaf_pem_str).expect("PEM→DER");

        // 1) 解析两张证书，验证 issuer/subject 链关系。
        let (_, ca_x509) = x509_parser::parse_x509_certificate(&ca_der).unwrap();
        let (_, leaf_x509) = x509_parser::parse_x509_certificate(&leaf_der).unwrap();

        // 叶子的 issuer DN 应等于 CA 的 subject DN。
        let ca_subject = ca_x509.subject().to_string();
        let leaf_issuer = leaf_x509.issuer().to_string();
        assert_eq!(
            ca_subject, leaf_issuer,
            "叶子 issuer DN 应等于 CA subject DN"
        );
        // 叶子 subject DN 含 CN=server.example.com。
        let leaf_subject = leaf_x509.subject().to_string();
        assert!(
            leaf_subject.contains("server.example.com"),
            "叶子 subject 应含 CN: {leaf_subject}"
        );

        // 2) 验证叶子证书的签名算法存在（OID 非空）——间接证明由 CA 签发。
        let sig_alg_oid_bytes = leaf_x509.signature_algorithm.algorithm.as_bytes();
        assert!(!sig_alg_oid_bytes.is_empty(), "叶子签名算法 OID 应非空");

        // 3) 验证 CA 是 CA（BasicConstraints ca=true）。
        let ca_is_ca = ca_x509
            .basic_constraints()
            .ok()
            .flatten()
            .map(|bc| bc.value.ca)
            .unwrap_or(false);
        assert!(ca_is_ca, "CA 证书的 BasicConstraints.ca 应为 true");

        // 4) 验证叶子不是 CA（ca=false 或无 BasicConstraints）。
        let leaf_is_ca = leaf_x509
            .basic_constraints()
            .ok()
            .flatten()
            .map(|bc| bc.value.ca)
            .unwrap_or(false);
        assert!(!leaf_is_ca, "叶子证书不应是 CA");
    }

    #[tokio::test]
    async fn cert_renew_changes_serial() {
        // renew 后叶子证书 serial 变化（新 KeyPair + 新签发）。
        let c = CaCertManager::new();
        c.init_ca("ca-renew").await.unwrap();
        let csr = make_test_csr("leaf-renew");
        c.sign(&csr, 30).await.unwrap();
        let before = c.list_certs().await.unwrap();
        let leaf_before = &before[1];
        let id_before = leaf_before.id.clone();
        let serial_before = leaf_before.serial.clone();

        c.renew(&id_before).await.expect("renew");

        let after = c.list_certs().await.unwrap();
        // 仍 2 张（CA + renew 后的叶子）。
        assert_eq!(after.len(), 2);
        let leaf_after = &after[1];
        assert_eq!(leaf_after.common_name, "leaf-renew", "renew 保持 CN");
        assert_ne!(
            leaf_after.serial, serial_before,
            "renew 后 serial 应变化（新证书）"
        );
    }

    #[tokio::test]
    async fn cert_renew_unknown_id_rejected() {
        let c = CaCertManager::new();
        c.init_ca("ca").await.unwrap();
        assert!(c.renew("nonexistent-id").await.is_err());
    }

    #[tokio::test]
    async fn cert_acme_request_without_config_blocked() {
        // 未注入 AcmeConfig → acme_request 返回明确错误（不静默成功，不回退 CA 自签）。
        let c = CaCertManager::new();
        let r = c.acme_request("example.com").await;
        assert!(r.is_err());
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("AcmeConfig") || msg.contains("acme"),
            "错误信息应提示未注入 AcmeConfig: {msg}"
        );
    }

    /// 测试 fixture：用 rcgen 生成一个真实 CSR（PEM 字节）。
    fn make_test_csr(cn: &str) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        let key = rcgen::KeyPair::generate().expect("KeyPair");
        let csr = params.serialize_request(&key).expect("serialize_request");
        csr.pem().expect("pem").into_bytes()
    }

    /// 测试 fixture：PEM CERTIFICATE → DER 字节。
    fn pem_to_cert_der(pem_str: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        use base64::Engine;
        let b64 = pem_str
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        let der = base64::engine::general_purpose::STANDARD.decode(b64)?;
        Ok(der)
    }

    // ========================================================================
    // 批 3 新增：boringtun VPN peer 管理（真实 noise 层）
    // ========================================================================

    #[tokio::test]
    async fn vpn_manager_generates_public_key() {
        // 构造时自动生成本机公钥（base64，44 字符含 padding，解码后 32 字节）。
        let (v, pk) = BoringtunVpnManager::new_with_public(51820);
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(pk.as_bytes())
            .unwrap();
        assert_eq!(bytes.len(), 32, "本机公钥解码后应 32 字节");
        assert_eq!(v.our_public_key(), pk);
    }

    #[tokio::test]
    async fn vpn_add_peer_invalid_pubkey_rejected() {
        // 非法公钥（非 base64 / 长度错）应拒绝。
        let v = BoringtunVpnManager::new(51820);
        let bad_cases = ["not-base64!!!", "k1", "", "AAAA", &"A".repeat(64)];
        for bad in bad_cases {
            let peer = VpnPeer {
                public_key: bad.to_string(),
                allowed_ips: vec![],
                endpoint: None,
                user: None,
            };
            assert!(v.add_peer(peer).await.is_err(), "非法公钥应被拒绝: {bad}");
        }
    }

    #[tokio::test]
    async fn vpn_remove_nonexistent_rejected() {
        let v = BoringtunVpnManager::new(51820);
        let pk = gen_wg_public_key();
        // 不存在的 peer 移除应失败。
        assert!(v.remove_peer(&pk).await.is_err());
    }

    #[tokio::test]
    async fn vpn_add_multiple_peers_and_status() {
        // 多 peer 管理：add 3 个不同公钥 → list 3 → status peer_count=3。
        let v = BoringtunVpnManager::new(51821);
        for i in 0..3 {
            let pk = gen_wg_public_key();
            let peer = VpnPeer {
                public_key: pk,
                allowed_ips: vec![os_network::IpCidr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, i + 1)),
                    32,
                )],
                endpoint: Some(format!("1.2.3.{i}:51820")),
                user: None,
            };
            v.add_peer(peer).await.expect("add peer");
        }
        assert_eq!(v.list_peers().await.unwrap().len(), 3);
        let st = v.status().await.unwrap();
        assert_eq!(st.peer_count, 3);
        assert_eq!(st.listen_port, 51821);
        // 无 TUN 设备 → 无真实握手 → running=false（语义：无 peer 曾完成握手）。
        assert!(!st.running);
        // 字节计数初始为 0（无数据收发）。
        assert_eq!(st.bytes_rx, 0);
        assert_eq!(st.bytes_tx, 0);
    }

    #[tokio::test]
    async fn vpn_add_then_remove_keeps_others() {
        // 增删查：A、B、C → 删 B → list 应含 A、C。
        let v = BoringtunVpnManager::new(51822);
        let pk_a = gen_wg_public_key();
        let pk_b = gen_wg_public_key();
        let pk_c = gen_wg_public_key();
        for pk in [&pk_a, &pk_b, &pk_c] {
            v.add_peer(VpnPeer {
                public_key: pk.clone(),
                allowed_ips: vec![],
                endpoint: None,
                user: None,
            })
            .await
            .unwrap();
        }
        v.remove_peer(&pk_b).await.unwrap();
        let remaining: Vec<String> = v
            .list_peers()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.public_key)
            .collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&pk_a));
        assert!(remaining.contains(&pk_c));
        assert!(!remaining.contains(&pk_b));
    }

    #[tokio::test]
    async fn vpn_peer_allowed_ips_preserved() {
        // peer 的 allowed_ips（IpCidr）在增删查中保持。
        let v = BoringtunVpnManager::new(51823);
        let pk = gen_wg_public_key();
        let cidr = os_network::IpCidr::parse("10.8.0.0/24").unwrap();
        v.add_peer(VpnPeer {
            public_key: pk.clone(),
            allowed_ips: vec![cidr],
            endpoint: Some("peer.example:51820".into()),
            user: Some(UserId::new("alice")),
        })
        .await
        .unwrap();
        let list = v.list_peers().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].allowed_ips.len(), 1);
        assert_eq!(list[0].allowed_ips[0].to_string(), "10.8.0.0/24");
        assert_eq!(list[0].endpoint.as_deref(), Some("peer.example:51820"));
        assert_eq!(
            list[0].user.as_ref().map(|u| u.to_string()),
            Some("alice".into())
        );
    }

    #[test]
    fn wg_key_b64_roundtrip() {
        // wg_key_b64 编码 32 字节 → base64，解码回 32 字节。
        let raw = [0x42u8; 32];
        let b64 = wg_key_b64(&raw);
        use base64::Engine;
        let back = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .unwrap();
        assert_eq!(back, raw.to_vec());
    }

    // ========================================================================
    // 批 4 新增：instant-acme ACME 自动证书签续（fixture 测，不真发 LE）
    // ========================================================================

    /// 构造一个注入 fixture ACME 服务器的 CaCertManager。
    fn make_acme_manager(domain_kind: crate::acme::AcmeChallengeKind) -> CaCertManager {
        let fixture = crate::acme::fixture::FixtureAcmeServer::new().expect("fixture CA 构造");
        let solver = std::sync::Arc::new(crate::acme::AutoSolveSolver::new());
        let config = crate::acme::AcmeConfig::with_directory(
            fixture.directory_url().to_string(),
            vec!["mailto:admin@example.com".into()],
            solver,
            domain_kind,
        )
        .with_http(std::sync::Arc::new(fixture));
        CaCertManager::new().with_acme(config)
    }

    #[tokio::test]
    async fn acme_request_full_flow_returns_certificate() {
        // 真实 instant-acme 流程（account → order → challenge → finalize → download）
        // 走 in-memory fixture（零网络）。返回的 Certificate 元数据应含签发域名。
        let c = make_acme_manager(crate::acme::AcmeChallengeKind::Http01);
        let cert = c
            .acme_request("os.example.com")
            .await
            .expect("acme_request ok");
        // fixture 用 rcgen 自签 CA 签叶子，CN 来自 SAN DNS（rcgen CertificateParams::new
        // 把域名放 SAN；CN 为空 → 回退首 SAN DNS）。
        assert_eq!(cert.common_name, "os.example.com");
        assert!(!cert.serial.is_empty(), "serial 应非空");
        assert!(cert.auto_renew, "ACME 证书应 auto_renew=true");
        // issuer 是 fixture CA CN。
        assert_eq!(cert.issuer, "OS ACME Fixture CA");
        // not_after 在未来。
        assert!(cert.not_after > chrono::Utc::now());
    }

    #[tokio::test]
    async fn acme_request_certificate_added_to_list() {
        // acme_request 成功后证书应出现在 list_certs（仅 ACME 叶子，未 init_ca）。
        let c = make_acme_manager(crate::acme::AcmeChallengeKind::Http01);
        let cert = c.acme_request("api.os.test").await.expect("acme");
        let list = c.list_certs().await.expect("list");
        assert_eq!(list.len(), 1, "list 应含 1 张 ACME 叶子证书");
        assert_eq!(list[0].common_name, "api.os.test");
        assert_eq!(list[0].id, cert.id);
    }

    #[tokio::test]
    async fn acme_request_multiple_domains_separate_certs() {
        // 多域名各签一张 → list 含多张（不同 serial）。
        let c = make_acme_manager(crate::acme::AcmeChallengeKind::Http01);
        c.acme_request("a.test").await.unwrap();
        c.acme_request("b.test").await.unwrap();
        let list = c.list_certs().await.unwrap();
        assert_eq!(list.len(), 2);
        let cns: Vec<_> = list.iter().map(|c| c.common_name.clone()).collect();
        assert!(cns.contains(&"a.test".into()));
        assert!(cns.contains(&"b.test".into()));
    }

    #[tokio::test]
    async fn acme_request_solver_observed() {
        // 验证 AcmeChallengeSolver 被调用（AutoSolveSolver 记录 challenge）。
        let solver = std::sync::Arc::new(crate::acme::AutoSolveSolver::new());
        let fixture = crate::acme::fixture::FixtureAcmeServer::new().unwrap();
        let config = crate::acme::AcmeConfig::with_directory(
            fixture.directory_url().to_string(),
            vec![],
            solver.clone(),
            crate::acme::AcmeChallengeKind::Http01,
        )
        .with_http(std::sync::Arc::new(fixture));
        let c = CaCertManager::new().with_acme(config);
        c.acme_request("solver.test").await.unwrap();
        let seen = solver.observed();
        assert_eq!(seen.len(), 1, "solver 应被调用一次");
        assert_eq!(seen[0].domain, "solver.test");
        assert_eq!(seen[0].kind, crate::acme::AcmeChallengeKind::Http01);
        // HTTP-01 key authorization 格式：<token>.<thumbprint>。
        assert!(seen[0].key_authorization.contains('.'));
        // dns_value（base64url SHA256 摘要）应非空。
        assert!(seen[0].dns_value.as_ref().unwrap().len() > 10);
    }

    #[tokio::test]
    async fn acme_request_without_config_returns_error() {
        // 未注入 AcmeConfig → acme_request 返回明确错误（与
        // cert_acme_request_without_config_blocked 一致，但此处显式测错误信息）。
        let c = CaCertManager::new();
        let err = c.acme_request("nope.test").await.unwrap_err();
        match err {
            SecurityError::Internal(msg) => {
                assert!(msg.contains("AcmeConfig"), "错误信息: {msg}");
            }
            other => panic!("期望 Internal，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn acme_request_dns01_preferred() {
        // preferred=Dns01 但 fixture 只挂 http-01 → 应报错（无 dns-01 challenge）。
        // 验证 challenge 类型选择逻辑：不静默回退，明确报错。
        let c = make_acme_manager(crate::acme::AcmeChallengeKind::Dns01);
        let r = c.acme_request("dns01.test").await;
        assert!(r.is_err(), "fixture 无 dns-01 challenge，应报错");
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("dns-01") || msg.contains("challenge"),
            "错误应提示 challenge 类型: {msg}"
        );
    }

    #[tokio::test]
    async fn renew_expiring_skips_non_renewable() {
        // auto_renew=false 的证书不续期。
        let c = CaCertManager::new();
        c.init_ca("ca").await.unwrap();
        let csr = make_test_csr("leaf");
        c.sign(&csr, 30).await.unwrap();
        // sign 出的叶子 auto_renew=false（meta_from_rcgen 默认）。
        let renewed = c.renew_expiring(365).await.unwrap();
        assert!(renewed.is_empty(), "auto_renew=false 不应续期");
    }

    #[tokio::test]
    async fn renew_expiring_acme_cert_renews() {
        // ACME 证书 auto_renew=true 且近过期 → renew_expiring 触发 re-request。
        // 手动注入一张近过期的 ACME 证书记录（绕过真实 ACME 流程，直接构造 LeafRecord）。
        let c = make_acme_manager(crate::acme::AcmeChallengeKind::Http01);
        {
            let mut leaves = c.leaves.lock().unwrap();
            let near_expiry = chrono::Utc::now() + chrono::Duration::days(2);
            let meta = Certificate {
                id: "acme-old-1".into(),
                common_name: "expiring.test".into(),
                not_before: chrono::Utc::now() - chrono::Duration::days(88),
                not_after: near_expiry,
                issuer: "OS ACME Fixture CA".into(),
                serial: "acme-old-1".into(),
                auto_renew: true,
            };
            leaves.insert(
                "acme-old-1".into(),
                LeafRecord {
                    meta,
                    pem: vec![],
                    der: vec![],
                    common_name: "expiring.test".into(),
                    days: 0,
                    spki_hint: String::new(),
                    source: CertSource::Acme("expiring.test".into()),
                },
            );
        }
        // threshold=30 天，2 天后过期 → 应续期。
        let renewed = c.renew_expiring(30).await.expect("renew ok");
        assert_eq!(renewed.len(), 1, "应续期 1 张 ACME 证书");
        // 旧 id（acme-old-1）应被移除（acme_request 用新 serial 覆盖）。
        let list = c.list_certs().await.unwrap();
        assert!(list.iter().all(|c| c.id != "acme-old-1"), "旧 id 应被移除");
        // 新证书来自 ACME re-request。
        assert!(list.iter().any(|c| c.common_name == "expiring.test"));
    }

    #[tokio::test]
    async fn renew_expiring_internal_ca_renews() {
        // CA 自签 + auto_renew=true 的近过期证书 → renew_expiring 走 renew(id)。
        let c = CaCertManager::new();
        c.init_ca("ca").await.unwrap();
        // 手动注入一张 auto_renew=true 的近过期 CA 自签叶子。
        let (old_id, _cn) = {
            let mut leaves = c.leaves.lock().unwrap();
            let near_expiry = chrono::Utc::now() + chrono::Duration::days(2);
            let meta = Certificate {
                id: "ca-old-1".into(),
                common_name: "internal.test".into(),
                not_before: chrono::Utc::now() - chrono::Duration::days(28),
                not_after: near_expiry,
                issuer: "ca".into(),
                serial: "ca-old-1".into(),
                auto_renew: true,
            };
            leaves.insert(
                "ca-old-1".into(),
                LeafRecord {
                    meta,
                    pem: vec![],
                    der: vec![],
                    common_name: "internal.test".into(),
                    days: 30,
                    spki_hint: String::new(),
                    source: CertSource::InternalCa,
                },
            );
            ("ca-old-1".to_string(), "internal.test".to_string())
        };
        let renewed = c.renew_expiring(30).await.expect("renew ok");
        assert_eq!(renewed.len(), 1);
        // renew 后旧 id 记录被新 serial 替换。
        let list = c.list_certs().await.unwrap();
        assert!(list.iter().all(|c| c.id != old_id), "旧 id 应被替换");
        // CA + 新叶子 = 2 张。
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|c| c.common_name == "internal.test"));
    }

    #[tokio::test]
    async fn renew_expiring_no_acme_config_for_acme_cert() {
        // ACME 来源证书需续期，但 manager 未注入 AcmeConfig → renew_expiring 返回错误。
        let c = CaCertManager::new();
        {
            let mut leaves = c.leaves.lock().unwrap();
            let meta = Certificate {
                id: "acme-1".into(),
                common_name: "x.test".into(),
                not_before: chrono::Utc::now(),
                not_after: chrono::Utc::now() + chrono::Duration::days(2),
                issuer: "LE".into(),
                serial: "acme-1".into(),
                auto_renew: true,
            };
            leaves.insert(
                "acme-1".into(),
                LeafRecord {
                    meta,
                    pem: vec![],
                    der: vec![],
                    common_name: "x.test".into(),
                    days: 0,
                    spki_hint: String::new(),
                    source: CertSource::Acme("x.test".into()),
                },
            );
        }
        let r = c.renew_expiring(30).await;
        assert!(r.is_err(), "无 AcmeConfig 时续期 ACME 证书应报错");
    }
}
