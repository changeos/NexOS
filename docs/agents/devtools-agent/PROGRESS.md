# devtools-agent 进度日志

> 分支：`feature/aead`（worktree `os-wt-aead`）
> crate：`os-services`（仅 `impl_devtools.rs` + 密钥 KVS AEAD 接通）

## 当前状态
- 阶段：密钥 KVS 真实 AES-256-GCM AEAD 已接通（替换 `ENC:` 占位）；CI Git 服务（gix）基线已就绪
- 最后更新：2026-08-05

## 批次概览

| 批次 | 范围 | 状态 |
|------|------|------|
| 基线 | Git 服务（gix 真实 init/commit/log/branch）+ DevTools trait 骨架 | ✅ 已交付（本轮接手前） |
| **批 3（本次）** | **密钥 KVS 真实 AEAD 加密（aes-gcm，替代 `ENC:` 占位）** | ✅ **已交付** |

## 批 3 真实实现（2026-08-05）

### 已接通

| 模块 | 原状态 | 现状态 | 依赖 |
|------|--------|--------|------|
| `impl_devtools.rs` 密钥 KVS（store_secret/get_secret/rotate_secret） | `ENC:` 占位（明文 + 4 字节标记，**非真实加密**） | ✅ 真实 AES-256-GCM AEAD：nonce(12B)‖ciphertext‖tag(16B) 拼接存储；nonce 用 OsRng 现场生成；密钥用 SHA-256 从种子派生（独立于系统密钥） | `aes-gcm 0.10`（workspace 新注册，ADR-DEPS-003）+ `sha2` + `rand_core`（ADR-DEPS-001 已注册） |

### 设计要点

1. **真实 AEAD（AES-256-GCM）**：
   - 密钥派生：`SHA-256(域分隔串 ‖ 种子)` → 32 字节 AES-256 密钥（`derive_kvs_key`）。
     域分隔串 `"os-devtools-kvs/aes-256-gcm/v1\x00"` 防止跨用途密钥复用。
   - 加密：`Aes256Gcm::generate_nonce(&mut OsRng)` 生成 12 字节 nonce（每条独立，**绝不复用**）→
     `cipher.encrypt(nonce, plain)` 输出 `ciphertext ‖ tag`（后置）→ 拼接存储 `nonce ‖ ct ‖ tag`。
   - 解密：长度校验（`< nonce(12)+tag(16)=28` 直接失败）→ 拆分 nonce / `ct‖tag` → `cipher.decrypt`，
     tag 校验失败返回错误（**密文不泄漏明文**）。
   - **trait 签名零改动**，加密/解密失败统一收敛为 `ServiceError::Internal`（不新增 variant）。
2. **构造器注入种子**：新增 `new_with_seed(seed: &[u8])`（生产部署注入配置/KMS 种子）；
   `new()` 用硬编码默认种子 `os-devtools-kvs-seed-v1`（向后兼容 + 测试方便）。后续可平滑升级
   argon2 派生 / KMS 注入（ADR-DEPS-003「后续」）。
3. **rotate_secret 真实化**：原占位 `:rotated` 标记废弃——改用 OsRng 生成 32 字节随机新值
   （AES-256 密钥材料长度，通用作密钥/令牌），用同主密钥重新加密落盘。
4. **临界区优化**：加密/解密在 mutex 外执行（只锁内存态读写），缩短临界区。
5. **解密失败审计**：get_secret 解密失败（密文损坏/tag 失败/密钥不匹配）→ 记 `success=false` 审计 + 错误消息。

### 安全性测试覆盖（新增）

- `kvs_store_get_roundtrip`：store → get 明文一致（保留，验证真实 AEAD 往返）。
- `kvs_aead_ciphertext_is_not_plaintext`：密文不含明文子串（证伪旧的 `ENC:` 占位）；不以 `ENC:` 起头。
- `kvs_aead_nonce_unique_per_encryption`：同明文两次加密 → 密文不同（nonce 随机 + AES-CTR 流异或不同）。
- `kvs_aead_wrong_key_rejected`：种子 A 加密、种子 B 解密 → GCM tag 校验失败（**错误密钥拒绝**）。
- `kvs_aead_tamper_rejected` / `kvs_aead_tamper_nonce_rejected`：篡改密文/nonce 一字节 → tag 校验失败（**篡改拒绝**）。
- `kvs_aead_short_ciphertext_rejected`：长度不足直接判失败（不进 aead decrypt）。
- `kvs_aead_roundtrip_with_various_plaintext_lengths`：空/单字节/16 字节/32 字节/100 字节多种长度往返一致。
- `kvs_derive_key_is_deterministic_and_seed_dependent`：同种子确定派生、不同种子互异、长度 32。
- `kvs_rotate_changes_value_and_audits`：轮换产出 32 字节随机新值，二次轮换不重复（替换原 `:rotated` 断言）。

### 测试统计

- `cargo test -p os-services --features mock`：**292 passed**（含本轮新增 8 个 AEAD 专项测 + 调整 1 个 rotate 测）。
  impl_devtools 模块测试 29 个（Git 服务 9 + DevTools trait 5 + KVS/AEAD 12 + 辅助 3）。
- `cargo clippy -p os-services --features mock --all-targets -- -D warnings`：0 warning。
- `cargo check --workspace --features mock`：全绿（新依赖不影响其他 crate）。

## 依赖接入（ADR-DEPS-003 续）

- workspace 根 `[workspace.dependencies]` 新增：
  - `aes-gcm = "0.10"`（纯 Rust AEAD，RustCrypto 栈；含默认 `aes` feature）
- crate 级 `os-services/Cargo.toml`：
  - `aes-gcm.workspace = true`（AEAD 加密）
  - `sha2.workspace = true`（密钥派生）
  - `rand_core.workspace = true`（OsRng nonce + 轮换新值生成）
- 新增传递依赖：`aes 0.8` / `ghash 0.5` / `aead 0.5` / `cipher 0.4` / `polyval 0.6` / `ctr 0.9`（全 RustCrypto，纯 Rust 无 FFI）。

## ADR

- 新增 [ADR-DEPS-003](../../adr/ADR-DEPS-003-aead.md)：记录 `aes-gcm` 选型（NIST 标准 / 纯 Rust / 与 ring 后端不冲突）、密文格式、密钥派生方案与备选否定理由。

## 红线遵守

- ✅ trait 签名零改动（`DevTools` 的 6 个方法签名不变）。
- ✅ 未改动 `ServiceError` variant（加密失败收敛为既有 `Internal`）。
- ✅ 未修改其他 service-agent 的 impl 文件（backup/monitor/media/files/power）。
- ✅ KVS 主密钥独立于系统密钥（独立种子 + 域分隔串，不共享 os-security 的 argon2/JWT 栈）。
- ✅ 密钥值绝不存明文（落盘永远是 `nonce‖ct‖tag`）。
- ✅ nonce 随机（OsRng），每条密文独立不复用。

## 待办（后续批次，超出本次范围）

- [ ] `devtools.rs` 契约文件的 `MemKvs` 测试辅助仍用 `ENC:` 占位（这是契约数据模型测试桩，
      测试 `SecretEntry`/`SecretAuditLog` CRUD/审计，非生产路径；本次按红线「不改 trait 契约文件」保留）。
- [ ] 密钥派生升级：SHA-256（固定种子）→ argon2（用户密码派生）或 KMS 注入（`new_with_seed` 接口已预留）。
- [ ] CI 远端 `git clone`（需 gix `blocking-network-client` feature）。
- [ ] CI 步骤沙箱执行器（容器/namespace 隔离）。
