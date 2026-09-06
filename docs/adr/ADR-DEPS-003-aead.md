# ADR-DEPS-003：注册 `aes-gcm` 接通 os-services(devtools) 真实 AEAD 加密

- **状态**：已采纳（Accepted）
- **日期**：2026-08-05
- **背景决策来源**：`docs/agents/devtools-agent.md` §9「风险红线」（KVS 加密算法变更须 ADR）+ `crates/os-services/src/impl_devtools.rs` 的 `ENC:` 占位 TODO
- **影响范围**：workspace 根 `Cargo.toml` 的 `[workspace.dependencies]`（新增 `aes-gcm`）+ `crates/os-services/Cargo.toml`（引用 `aes-gcm`/`sha2`/`rand_core`）+ `crates/os-services/src/impl_devtools.rs`（密钥 KVS 真实加密）
- **前置 ADR**：ADR-DEPS-001（`argon2`/`sha2`/`rand_core` 已注册）、ADR-DEPS-002（`gix` 已注册）

---

## 背景

os-services(devtools) 的密钥 KVS（`store_secret`/`get_secret`/`rotate_secret`）当前用
`ENC:` 前缀占位（明文 + 4 字节标记），AEAD 加密是规格书 §9 明确列出的「上线前必须替换」
红线项。本 ADR 推进 KVS 从占位走到**真实 AEAD 加密**——这是 devtools-agent 批 3 的最后一公里：
把 `ENC:` 占位换成 `aes-gcm` 真实加密，让密钥值真正以密文落盘（nonce‖ciphertext‖tag 拼接），
解密时按位拆分校验。

## 决策

在 workspace 根 `[workspace.dependencies]` 注册 `aes-gcm = "0.10"`，并在
`crates/os-services/Cargo.toml` 引用 `aes-gcm`（含配套的 `sha2` 密钥派生 + `rand_core`
OsRng nonce 生成）。

### 注册清单

| crate | workspace 声明 | feature | 用途（归属 crate） |
|-------|---------------|---------|-------------------|
| `aes-gcm` | `"0.10"` | 默认（含 `aes`） | os-services(devtools) 密钥 KVS 真实 AES-256-GCM AEAD |

`sha2`（已注册于 ADR-DEPS-001）与 `rand_core`（已注册于 ADR-DEPS-001）在
`crates/os-services/Cargo.toml` 按 `xxx.workspace = true` 引用：
- `sha2`：从固定的 KVS 主密钥种子（devtools 独立于系统密钥）派生 32 字节 AES-256 密钥。
- `rand_core`：`OsRng` 生成每条密文独立 12 字节 GCM nonce（CSPRNG）。

### 选型理由

1. **NIST 标准**：AES-GCM 是 NIST SP 800-38D 标准的 AEAD（认证加密），同时提供机密性 +
   完整性（GCM tag 校验防篡改）。OS 系统的密钥 KVS 场景需要的就是「加密 + 防篡改」，
   GCM 是行业默认选择（TLS 1.3 / JWT A256GCM 等都用它）。
2. **纯 Rust**：`aes-gcm` 来自 RustCrypto 组织，是纯 Rust 实现（底层 `aes` + `ghash`），
   无 OpenSSL/BoringSSL FFI 系统依赖。与 ADR-DEPS-001 的「`reqwest` 用 rustls-tls 避开
   OpenSSL」「`gix` 选纯 Rust 而非 libgit2」一脉相承——保持构建链干净。
3. **与 ring 后端不冲突**：reqwest/rustls 的 TLS 栈用 `ring` 作为后端，`aes-gcm` 是
   RustCrypto 的独立类型栈（`aead::Aes256Gcm`），与 `ring::aead` 是**两个独立的 AEAD 实现**，
   不共享类型也不互相依赖，二者在同 crate 共存无版本冲突（`ring` 经 reqwest/rustls/boringtun
   已在 lockfile，`aes-gcm` 仅引入 RustCrypto 栈）。这避免了「强制把所有 AEAD 收敛到 ring」
   的耦合（RustCrypto 的可测试性/可审计性更直接）。
4. **0.10 稳定线**：`aes-gcm 0.10`（含 `aead 0.5`）是当前稳定主线，API 成熟
   （`Aes256Gcm::new(&key)` + `encrypt(nonce, payload)` / `decrypt(nonce, ciphertext)`），
   被广泛部署。不采用预发布版本。

### 密文格式与密钥派生

- **密文布局**（落盘字节流）：`nonce(12B) ‖ ciphertext ‖ tag(16B)`。
  aes-gcm 0.10 的 `encrypt` 默认把 tag **后置**追加到 ciphertext，故实际存储格式为
  `nonce ‖ (encrypt 输出 = ciphertext||tag)`。解密时按 `[0..12]` 取 nonce、`[12..]` 取
  `ciphertext||tag` 传给 `decrypt`——长度不足（`< 12 + 16 = 28`）直接判失败。
- **nonce**：每条密文用 `OsRng` 生成独立随机 12 字节 nonce（**绝不复用**，GCM 的安全前提）。
  `OsRng` 是操作系统级熵源（与 os-discover beacon / os-security 一致）。
- **密钥派生**：devtools KVS 的主密钥独立于系统密钥（规格书 §9 红线）。本 ADR 用
  `SHA-256(固定种子)` 派生 32 字节 AES-256 密钥（`derive_kvs_key`，纯函数 + 构造器注入种子，
  默认种子硬编码于 `DefaultDevTools`）。**这是初始接通的简化方案**——后续可平滑升级为
  argon2 派生（os-security 已接通）或外部 KMS 注入（构造器注入），接口（`new_with_seed`）
  已预留。密钥**绝不存明文**：种子仅在内存，落盘的永远是密文。

## 备选方案与否定理由

1. **`ring::aead`**：ring 已在 lockfile（reqwest/rustls/boringtun 传递）。否定理由：ring 的 API
   更面向「TLS 会话密钥」场景（`LessSafeKey`/`UnboundKey`/`SealingKey`），devtools KVS 用
   RustCrypto 的 `aead` trait 更直接、可测试性更好；且与「不强制收敛到 ring」的解耦原则一致。
2. **`chacha20poly1305`**（同样 RustCrypto）：同样纯 Rust AEAD，否定理由：AES-GCM 在 OS
   设备的 CPU（常带 AES-NI 指令集）上硬件加速更普遍，性能更优；且 GCM 是行业默认。
   后续若需纯软件场景（无 AES-NI 的低端 SoC）可由独立 ADR 切到 chacha20poly1305。
3. **argon2 直接派生密钥**：argon2 已在 workspace，可作 KDF。否定理由（本轮）：argon2 是
   **慢哈希**（OWASP 推荐档 ~50ms/次），KVS 每次加解密都跑 argon2 不经济；argon2 适合
   「从用户密码派生主密钥」（一次性），而 KVS 主密钥派生是一次性初始化、加解密用对称密钥。
   本 ADR 用 SHA-256 派生（初始接通），argon2 派生留作后续平滑升级（构造器注入接口已预留）。
4. **保持 `ENC:` 占位不动**：规格书 §9 明确列为红线（「上线前必须替换为 AEAD」），否定。

## 代价

- **依赖树新增**：`aes-gcm 0.10` 引入 `aes 0.8` + `ghash 0.5` + `aead 0.5` + `cipher 0.4` +
  `polyval 0.6` + `ctr 0.9`（全 RustCrypto 栈，纯 Rust，无 FFI）。这些 crate 此前未被任何
  workspace crate 直接引用（ring 内部有自己的 AES 实现），故 lockfile 会新增这一组包。
  编译开销小（纯 Rust，无 C 编译）。
- **密文不可读**：落盘密文是二进制（`nonce‖ct‖tag`），调试时不可直接 `cat`——但这是
  安全要求（密文落盘），可接受（运维通过 `get_secret` 解密查看）。
- **密钥派生简化**：本轮用 SHA-256（固定种子）派生，安全性依赖种子的保密性（生产部署
  须把种子从配置/KMS 注入，而非硬编码默认值）。后续升级 argon2 派生 / KMS 注入由独立 ADR。

## 验证

1. **版本解析 + 编译**：`cargo check -p os-services --features mock` 通过（exit 0），
   `aes-gcm 0.10.x` 及其 RustCrypto 传递依赖完整编译。
2. **真实 AEAD 往返测试**：`kvs_store_get_roundtrip`（store → get 明文一致） +
   新增 `kvs_aead_wrong_key_rejected`（错误主密钥 → 解密失败，密文不泄漏） +
   新增 `kvs_aead_tamper_rejected`（篡改密文一字节 → 解密失败，GCM tag 校验生效）。
3. **密文非明文**：新增断言密文**不含**明文子串（`!cipher.windows(plain.len()).any(...)`），
   证伪旧的 `ENC:` 占位（旧的密文是 `ENC:plaintext`，含明文）。
4. **clippy 无警告**：`cargo clippy -p os-services --features mock -- -D warnings` 通过。

## 对既有约定的影响

- workspace 根 `Cargo.toml` `[workspace.dependencies]` 新增「AEAD 加密（os-services devtools 密钥 KVS）」
  分区（归属标注），紧随 ADR-DEPS-002 的 P2「安全」分区。
- `crates/os-services/Cargo.toml` 新增 `aes-gcm`/`sha2`/`rand_core` 三条 `workspace = true` 引用。
- `crates/os-services/src/impl_devtools.rs`：`DefaultDevTools` 内部 `encrypt`/`decrypt` 从
  `ENC:` 占位改为真实 AES-256-GCM；构造器新增 `new_with_seed`（注入主密钥种子），
  `new()` 保持默认种子（向后兼容）。**trait 签名零改动**，`ServiceError` variant 零改动
  （加密失败收敛为 `ServiceError::Internal`）。
- 不影响 os-services 的其他五个组件（backup/monitor/media/files/power）——`aes-gcm` 仅在
  `impl_devtools.rs` 引用，编译时链接但其他组件不暴露其类型。

## 后续

- **密钥派生升级**：从 SHA-256（固定种子）平滑升级到 argon2（用户密码派生主密钥）或
  外部 KMS 注入——`new_with_seed` 构造器接口已预留，独立 ADR 评估迁移。
- **2FA secret 加密落盘**（os-security 批 4 剩余项）：可复用本 ADR 的 `aes-gcm` 注册，
  os-security 按 `aes-gcm.workspace = true` 引用即可。
- **多版本共存监控**：若后续其他 crate 引入 ring 的 AEAD（`ring::aead`），
  `aes-gcm`（RustCrypto）与 ring 是两套独立实现，互不冲突，可接受。
