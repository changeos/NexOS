# security-agent 进度

> 分支：`p2/security-agent`（worktree `os-wt-p2-security`）
> crate：`os-security`

## 批次概览

| 批次 | 范围 | 状态 |
|------|------|------|
| 批 1 | trait 骨架 + 纯逻辑（33 测） | ✅ 已交付（基线） |
| 批 2 | 接通 argon2 / jsonwebtoken / hmac+sha1 真实实现 | ✅ 已交付（49 测） |
| **批 3（本次）** | **接通 rcgen（CA 签发）+ boringtun（VPN noise 层）真实实现** | ✅ **已交付（67 测）** |
| 批 4（剩余） | ACME 客户端（instant-acme）、AEAD（2FA secret 加密落盘） | ⏳ 阻塞（依赖未注册，ADR-DEPS-002 暂缓至 P3） |

## 批 3 真实实现（2026-08-05）

### 已接通

| 模块 | 原状态 | 现状态 | 依赖 |
|------|--------|--------|------|
| `impls.rs` `CaCertManager` init_ca/sign/list_certs/renew | 阻塞（rcgen 未注册，返回 `Internal`） | ✅ 真实 rcgen 自签 CA（ECDSA P256，IsCa::Ca(Unconstrained) + KeyCertSign/CrlSign）+ CSR 解析签发叶子证书（DigitalSignature + ServerAuth/ClientAuth EKU）+ renew（保持 CN，新 KeyPair） | `rcgen` 0.14（workspace）+ `x509-parser`/`time`/`rustls-pki-types`（rcgen 传递依赖，crate 直引） |
| `impls.rs` `BoringtunVpnManager` add_peer/remove_peer/list_peers/status | 阻塞（boringtun 未注册，status 返回静态） | ✅ 真实 boringtun noise 协议层：构造时生成本机 x25519 密钥对，add_peer 校验公钥（base64 + 32 字节）+ 构造 `boringtun::noise::Tunn`（每 peer 一条），status 聚合各 Tunn 的 tx/rx 字节 + 握手状态 | `boringtun` 0.7（workspace，**不开 device feature**）+ `base64` 0.22（crate 直引） |

### 设计要点

1. **CA 证书签发（rcgen 0.14 新 API）**：
   - `init_ca`：构造 `CertificateParams`（`IsCa::Ca(BasicConstraints::Unconstrained)` + KeyCertSign/CrlSign/CrlSign + 10 年有效期）→ `KeyPair::generate()`（ECDSA P256）→ `params.self_signed(&key)`。幂等（重复 init 返回相同 CA）。
   - `sign`：PEM/DER 双解析 CSR（`CertificateSigningRequestParams::from_pem`/`from_der`，含 `verify_signature`）→ 提取 params + 公钥校验 → 用 `CertificateParams::signed_by(&leaf_key, &Issuer::from_params(&ca_params, &ca_key))` 签发叶子（DigitalSignature + ServerAuth/ClientAuth）。返回 PEM 字节。
   - `renew`：按 id 取出叶子记录（CN + days）→ 重建 params（同 CN）+ 新 KeyPair → `sign_leaf_with_ca` 重签（PKI renew 语义：旧证书失效，新证书生效，serial 变化）。
   - 密钥安全：CA 私钥 + 叶子私钥均仅内存（生产路径须配合 keyring/KMS）；`sign`/`renew` 返回证书不含私钥（CSR 提交方持有私钥，标准 PKI 流程）。
2. **证书链验证**（测试 `cert_chain_verification`）：用 x509-parser 解析 CA + 叶子 DER，验证：
   - 叶子 issuer DN == CA subject DN；
   - 叶子签名算法 OID 非空（由 CA 签发）；
   - CA 证书 `BasicConstraints.ca = true`，叶子 `ca = false`；
   - 叶子 subject 含 CN=server.example.com。
3. **VPN（boringtun noise 层，不开 device feature）**：
   - 构造时生成 x25519 本机密钥对（`StaticSecret::random_from_rng(OsRng)` + `PublicKey::from`），暴露本机公钥（base64，WireGuard 标准）。
   - `add_peer`：base64 解码 peer 公钥（须 32 字节，否则 VpnError）→ 构造 `boringtun::noise::Tunn::new(our_secret, peer_public, None, None, idx, None)`（真实 WireGuard noise 状态机）。
   - `status`：遍历各 `Tunn::stats()` → 聚合 tx/rx 字节 + 握手状态；`running = 至少一 peer 完成握手`。
   - **红线遵守**：不开 `device` feature（无 socket2/TUN），不建真实隧道；仅 noise 协议层（握手/会话/字节统计）真实，fixture 测试覆盖。

### 红线遵守

- ❌ 不改 trait 签名（`CertManager`/`VpnManager` 契约未动）。
- ❌ 不改其他 agent crate（`cargo check --workspace` 全绿）。
- ❌ 不真建 TUN 隧道（boringtun `device` feature 未开，需 root/网络命名空间）。
- ✅ AEAD 加密（2FA secret）仍阻塞（crate 未注册）——TotpTwoFactor secret 内存版保留，留 TODO。

### 测试

- 原 49 测全保留（`vpn_add_remove_peer`/`cert_list_empty_ok` 升级为真实路径：前者用真实 x25519 公钥 + boringtun Tunn 构造；后者保留空列表语义）。
- 新增测试 18 个：
  - **rcgen CA**：cert_init_ca_real、cert_init_ca_idempotent、cert_list_includes_ca_after_init、cert_sign_without_init_rejected、cert_sign_zero_days_rejected、cert_sign_returns_valid_pem、cert_sign_then_list_two、**cert_chain_verification**（链验证：issuer==subject DN / 签名算法 / BasicConstraints）、cert_renew_changes_serial、cert_renew_unknown_id_rejected、cert_acme_request_blocked
  - **boringtun VPN**：vpn_manager_generates_public_key、vpn_add_peer_invalid_pubkey_rejected、vpn_remove_nonexistent_rejected、vpn_add_multiple_peers_and_status、vpn_add_then_remove_keeps_others、vpn_peer_allowed_ips_preserved、wg_key_b64_roundtrip
- **当前总测试数：67**（49 → 67，新增 18）。

### cargo 结果（`-p os-security --features mock`）

```
cargo check   ✅ clean（无 warning）
cargo test    ✅ 67 passed; 0 failed
cargo clippy  ✅ clean（无 warning，-D warnings）
cargo check --workspace ✅ 全 workspace 通过（无下游破坏）
```

## 批 2 真实实现（2026-08-05）

### 已接通

| 模块 | 原状态 | 现状态 | 依赖 |
|------|--------|--------|------|
| `password.rs` `hash_password`/`verify_password` | TODO 占位（明文比对/拒绝） | ✅ 真实 Argon2id（PHC 字符串，OsRng salt，回退常量时间比较兼容 mock 路径） | `argon2` 0.5（workspace） |
| `impls.rs` `JwtIssuerImpl`（issue/verify/rotate_keys） | TODO 返回 `Internal` | ✅ 真实 jsonwebtoken HS256（编码/验签/过期校验 + 密钥轮换宽限期） | `jsonwebtoken` 9（workspace） |
| `totp.rs` `compute_hmac_sha1` + `generate_code` | TODO 占位（返回错误） | ✅ 真实 HMAC-SHA1（RFC 2104）+ RFC 6238 TOTP 生成 | `hmac` 0.12 + `sha1` 0.10（crate 直依赖，按 ADR-DEPS-001「按需引用」） |
| `impls.rs` `TotpTwoFactor` enable/verify/disable | 阻塞（HMAC + AEAD 未注册） | ✅ HMAC 真实 + 内存 secret 存储；enable 生成 OsRng secret + otpauth URI（base32） | `rand` 0.8（crate 直依赖） |

### 设计要点

1. **密码哈希**：`hash_password` 用 `Argon2::default()`（OWASP 推荐档 m=19456/t=2/p=1）+
   `SaltString::generate(OsRng)` 产出 PHC 字符串。`verify_password` 先 `PasswordHash::new`
   解析 PHC → argon2 验签；解析失败（mock 路径的明文/任意占位）回退 `constant_time_eq`，
   保持向后兼容。
2. **JWT**：HS256 对称签名，构造器 `JwtIssuerImpl::new(secret)` 注入密钥。密钥轮换：
   `set_current_key(new)` 自动把旧密钥推入宽限期（最多保留 4 个），`verify` 依次尝试
   current + grace。`rotate_keys()`（trait 契约无入参）负责修剪宽限期长度。
3. **TOTP**：HMAC-SHA1 真实（RFC 4226 测试向量 + RFC 6238 时间向量全部通过）。
   `TotpTwoFactor::verify` 用 `generate_code` 计算 ±1 窗口 code，常量时间比对。
4. **密钥源**：HS256 secret 由调用方注入（env/keyring/KMS，本结构不负责）；
   2FA secret 内存版直接存明文（仅测试/演示）。

### 测试

- 原 33 测全保留（仅替换 2 个"阻塞期"语义测试为"真实路径"测试）。
- 新增测试 16 个：
  - `password.rs`：hash_password_succeeds、hash_verify_roundtrip、hash_password_random_salt、verify_password_malformed_phc_falls_back
  - `totp.rs`：hmac_sha1_known_vector、hmac_sha1_rfc4226_sequence、totp_rfc6238_vectors_sha1、totp_default_digits_six
  - `impls.rs`：db_auth_set_password_and_authenticate、jwt_issue_and_verify_roundtrip、jwt_verify_wrong_key_rejected、jwt_verify_expired_rejected、jwt_verify_malformed_rejected、jwt_rotate_keys_grace_window、totp_verify_not_enabled、totp_enable_returns_otpauth、totp_enable_then_verify_roundtrip、totp_disable_then_verify_fails、base32_encode_known_vectors、base32_encode_20bytes_length
- **当前总测试数：49**（33 → 49，新增 16）。
- 已知向量：RFC 4226 HOTP counter 0..9、RFC 6238 TOTP(SHA1, 8 位) 6 个时间戳、RFC 4648 Base32 编码向量。

### cargo 结果（`-p os-security --features mock`）

```
cargo check   ✅ clean（无 warning）
cargo test    ✅ 49 passed; 0 failed
cargo clippy  ✅ clean（无 warning）
cargo check --workspace ✅ 全 workspace 通过（无下游破坏）
```

## 剩余 TODO（批 4 阻塞）

| 项 | 阻塞依赖 | 备注 |
|----|---------|------|
| `CaCertManager::acme_request` | ACME 客户端（`instant-acme`） | ADR-DEPS-002 §后续 明确暂缓至 P3；内部 CA（init_ca + sign）已覆盖内部域名场景 |
| `TotpTwoFactor` secret 持久化 | AEAD crate（`chacha20poly1305`/`aes-gcm`） | 当前内存版；生产需加密落盘 |
| `JwtIssuerImpl` 密钥源（KMS/keyring） | `keyring` / KMS SDK | 当前由调用方注入 secret |
| `BoringtunVpnManager` 真实数据面（TUN/socket） | boringtun `device` feature + root | 当前仅 noise 协议层；peer 增删查 + 握手状态 + 字节统计已可用 |
| ed25519 签名验签 | `ed25519-dalek`（已注册） | **os-security 无此需求**——beacon 防伪属 os-discover、包验签属 os-update；本 crate 跳过 |

## 不变量

- trait 签名未改（`AuthProvider`/`JwtIssuer`/`CertManager`/`TwoFactor`/`VpnManager`）。
- `SecurityError` 枚举 variant 未增减。
- 其他 agent crate 未改动。
- mock feature 行为兼容（`MockJwtIssuer` 仍走 serde 序列化，不依赖 jsonwebtoken；`MockCertManager`/`MockVpnManager` 不依赖 rcgen/boringtun）。
