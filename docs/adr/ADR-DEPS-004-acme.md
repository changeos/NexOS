# ADR-DEPS-004：注册 instant-acme 并接通 os-security 的 ACME 自动证书签续

- **状态**：已采纳（Accepted）
- **日期**：2026-08-05
- **背景决策来源**：`docs/PROGRESS.md`「P2 真实集成第二波 / os-security」末「未接入：`acme_request`（instant-acme 未注册，P3）」+ 主规划 §3.16「证书管理（内部 CA + ACME）」+ `docs/agents/security-agent.md` §3（`CertManager` 拥有契约）
- **影响范围**：workspace 根 `Cargo.toml`（注册 `instant-acme`）+ `crates/os-security`（crate 级引用 + `CaCertManager::acme_request` 真实实现）
- **前置 ADR**：ADR-DEPS-002（P2 领域依赖；本 ADR 落地其中「P3 后续 ADR：ACME」暂缓项）

---

## 背景

os-security 的 `CertManager` trait 含 `acme_request(domain) -> Certificate`——对外域名（如 `os.example.com`）
走 ACME（Let's Encrypt 等）自动签发与续期。批 3（P2 真实集成第二波）已接通 `rcgen`（内部 CA 自签），
但 `acme_request` 仍返回 `Err(SecurityError::Internal(...))`，理由：**ACME 客户端 crate 未注册**。
ADR-DEPS-002 §后续明确「ACME 暂缓至 P3 由独立 ADR 评估」。

本 ADR 收口该暂缓项：注册 `instant-acme`（纯 Rust、async、RFC 8555 ACMEv2 客户端），并接通
`CaCertManager::acme_request` 的真实 ACME 流程（order → challenge → finalize → download）。

## 决策

### 1. 选型：`instant-acme` 而非 `acme-micro` / 自实现

| 维度 | `instant-acme` ✅ | `acme-micro` | 自实现 |
|------|-------------------|--------------|--------|
| 维护 | djc 维护（活跃，2025 仍在发版） | 维护停滞（最后发版 2021） | 自负重 |
| 栈 | 纯 Rust（hyper + rustls + ring），无 openssl/FFI | reqwest + openssl | — |
| 异步 | 原生 async（tokio） | 同步阻塞 | — |
| RFC 8555 | 完整（account/order/authz/challenge/finalize/revoke + ARI + external account binding + device-attest） | 部分 | 需自写 JWS/签名 |
| CryptoProvider 抽象 | `HttpClient` trait 可注入测试桩（fixture） | 无 | — |

`instant-acme` 与 workspace 现有 rustls/ring 栈共栈（与 os-discover mTLS、os-protocols libunftp/russh
的 ring 后端一致），不引入 openssl（与 ADR-DEPS-001 reqwest rustls-tls 决策同源）。**选用 `instant-acme`。**

### 2. 注册：workspace 根 + feature 策略

```toml
# workspace 根 Cargo.toml [workspace.dependencies]
instant-acme = { version = "0.8", default-features = false, features = ["hyper-rustls", "rcgen", "ring"] }
```

- **版本**：`"0.8"`（任务方建议 `0.7` 或最新；`0.8` 是 docs.rs 当前主线，与 `0.7` 公共 API 兼容，
  `0.8` 修正 Account/Order 句柄生命周期、补 ARI / device-attest。lockfile 锁定 `0.8.5`）。
- **`default-features = false` + 显式 feature**：
  - `hyper-rustls`：默认 HTTP 客户端（`DefaultClient`，基于 hyper + rustls）。
  - `rcgen`：`Order::finalize(&mut self)` 内部用 rcgen 生成 CSR（os-security 已依赖 rcgen，共栈）。
  - `ring`：crypto 后端选 ring（与 workspace rustls/discover/protocols 一致）。
  - **不开 `aws-lc-rs`**（instant-acme 默认 feature）：aws-lc-rs 需 C 编译器 + nasm
    （Windows 麻烦、CI 沙箱缺），与 ADR-DEPS-002 §选型理由否定默认后端同源。
- **不开 `fips`**：FIPS 模式非 OS 场景需求。

### 3. 接入：`crates/os-security/Cargo.toml`

```toml
instant-acme = { workspace = true }
```

crate 级直接引用 workspace 声明（与 argon2/jsonwebtoken/rcgen/boringtun 同模式）。
额外补 `rustls-pki-types`（已间接引入，用于 `PrivateKeyDer` 等类型）——若 instant-acme 已 re-export 则不重复。

### 4. 实现：`CaCertManager::acme_request` 真实 ACME 流程

新增 `AcmeConfig`（构造配置）+ `AcmeChallenge`（challenge 解析结果，供协调器完成）+
`AcmeChallengeSolver`（trait，抽象 challenge 完成——HTTP-01 / DNS-01 由调用方注入）。
`CaCertManager` 增持有 `Option<AcmeConfig>`（默认 None → ACME 不可用，回退到内部 CA 自签）。

**流程**（`acme_request`）：
1. 取 `AcmeConfig`（directory_url + account credentials + challenge solver）；None → `Err`。
2. 构造/恢复 `Account`（`Account::builder_with_http` 或 `from_credentials`）。
3. `account.new_order(&NewOrder::new(&[Identifier::Dns(domain)]))` → `Order`。
4. 遍历 `order.authorizations()`：取 challenge（优先 HTTP-01，回退 DNS-01），调用
   `solver.solve(challenge)`（调用方完成 HTTP/DNS 摆放），`challenge.set_ready()`。
5. `order.poll_ready(&RetryPolicy)` → `OrderStatus::Ready`。
6. `order.finalize()`（rcgen 生成 CSR + 提交）→ `order.poll_certificate(&RetryPolicy)` → PEM 证书链。
7. 解析 PEM → `Certificate` 元数据（CN/not_before/not_after/issuer/serial），入库（`auto_renew=true`）。

**与 rcgen CA 协调**：ACME 证书优先（`acme_request` 成功则用 LE 证书）；CA 自签作 fallback：
- ACME 不可用（无 `AcmeConfig` / 网络失败 / challenge 失败）→ `acme_request` 返回 `Err`，
  调用方可改走 `init_ca` + `sign` 覆盖内部域名场景（既有行为不变）。
- 不自动从 ACME 回退到 CA 自签（语义不同：ACME 是公网可信，CA 自签是内部可信——混用会误导）。

### 5. 续期逻辑

新增 `renew_expiring(&self, threshold_days: u32) -> Result<Vec<String>>`：遍历 `leaves` 中
`auto_renew=true` 且 `not_after - now < threshold_days` 的证书，逐个 re-request：
- ACME 来源（`issuer` 非 CA CN）→ 调 `acme_request(domain)` 重新签发。
- CA 自签来源 → 调既有 `renew(id)`。

trait 不改签名；续期入口由调用方（上层守护进程）按调度触发。

### 6. 测试策略（红线：不真发 Let's Encrypt）

**不依赖 pebble**（pebble 是 Go 编译的二进制 ACME 测试服务器，需额外系统依赖，与「最小系统依赖」目标不符；
且 sandbox 通常无 pebble）。改用 **in-memory fixture**：

- 实现 `instant_acme::HttpClient` trait 的 `FixtureAcmeServer`：内存中模拟一个最小 ACMEv2 服务器，
  按 RFC 8555 响应 `newNonce` / `newAccount` / `newOrder` / `authz` / `challenge` / `finalize` / `cert`
  序列。instant-acme 的 JWS 签名/Nonce/重试逻辑**真实跑**（验证客户端集成正确），仅网络层替换为内存。
  返回的证书用 rcgen 自签的 fixture CA 签发（与 `examples/certgen.rs` 同思路）。
- `AcmeChallengeSolver` 提供 `AutoSolveSolver`（测试用：`set_ready` 前自动标记 challenge 已通过——
  与 fixture server 配合，fixture 在收到 `set_ready` 后直接将 challenge 置 Valid）。
- 测试覆盖：account 创建、order 创建、challenge 解析 + set_ready、finalize、poll_certificate、
  续期（`renew_expiring` 对近过期证书 re-request）、ACME 证书入 list、无 `AcmeConfig` 时 `Err`、
  `Identifier::Dns` 解析、HTTP-01/DNS-01 key_authorization 格式。

**生产路径测（不跑）**：真实 LE Staging 测试需联网，留 TODO；CI 沙箱缺出口带宽时跳过。

## 备选方案与否定理由

1. **pebble 集成测试**——pebble 是事实标准 ACME 测试服务器，但需 Go 工具链编译 + 额外 binary 部署，
   与 workspace「最小系统依赖 + 纯 Rust 栈」目标不符；CI 沙箱通常无 pebble。改用 in-memory fixture
   （覆盖客户端集成逻辑；服务端语义由 instant-acme 自身的 RFC 一致性保证）。否定本次纳入。
2. **`acme-micro`**——维护停滞（最后发版 2021），reqwest + openssl 栈，同步阻塞 API。否定。
3. **自实现 ACME**——需自写 JWS 签名 / nonce 管理 / 重试，工作量大且易错（ACMEv2 协议细节多）。
   否定，复用 instant-acme。
4. **`aws-lc-rs` 后端**（instant-acme 默认）——aws-lc-rs 需 C 编译器 + nasm，与 workspace 既定 ring
   栈不一致（ADR-DEPS-002 否定默认后端同源）。否定，显式选 ring。
5. **ACME 失败自动回退 CA 自签**——语义混淆（公网可信 vs 内部可信），调用方不知证书来源。
   否定；`acme_request` 失败显式 `Err`，调用方决定 fallback。

## 代价

- **依赖树膨胀**：`instant-acme` 带入 hyper-util / hyper-rustls / rustls-webpki / rustls-platform-verifier
  等（部分与 os-discover/os-protocols 共栈，复用既有编译产物）。注册 + 引用后 lockfile 增量约 12 包。
- **ring 0.17 共存**：workspace 已有 ring 0.17（boringtun / rustls 传递），instant-acme 也用 ring 0.17，
  无多版本问题。
- **`rcgen` feature 重叠**：os-security 已直接依赖 rcgen（`features = ["pem", "x509-parser"]`），
  instant-acme 的 `rcgen` feature 再引 rcgen（无 feature）——Cargo 自动合并 feature，无冲突。
- **测试 fixture 复杂度**：`FixtureAcmeServer` 需正确模拟 RFC 8555 响应（含 Location header / nonce /
  Retry-After），实现量约 200 行——但一次性投入，后续 ACME 相关测试均复用。

## 验证

1. **版本解析 + 编译**：临时 crate 引用 `instant-acme = { version="0.8", default-features=false,
   features=["hyper-rustls","rcgen","ring"] }`，`cargo check` 通过（7s，无 aws-lc-rs 编译）。
2. **`cargo check/test/clippy -p os-security --features mock` 全绿**：67 → 67+N 测（N 为新增 ACME 路径测）。
3. **不真发 LE**：所有 ACME 测试走 `FixtureAcmeServer`（in-memory），零网络出口。

## 对既有约定的影响

- workspace 根 `Cargo.toml` `[workspace.dependencies]` 新增「安全（os-security）」分区下 `instant-acme`。
- `crates/os-security/Cargo.toml` 加 `instant-acme.workspace = true`。
- `crates/os-security/src/impls.rs`：`CaCertManager` 增 `acme: Mutex<Option<AcmeConfig>>` 字段 +
  `with_acme`/`renew_expiring` 方法 + `acme_request` 真实实现（替换原 `Err` 占位）。
- **trait 签名零改动**（`CertManager` 5 方法签名全保留）。
- **`SecurityError` 不新增 variant**（ACME 错误映射到既有 `Internal` / `CertExpired`）。
- 其他 agent crate 零改动。

## 后续

- **生产 LE Staging 测**：CI 环境有出口带宽时，加 `#[ignore]` 标的真实 LE Staging 集成测（需 challenge
  solver 真摆 HTTP/DNS——通常用 `--allow-net` + 临时 HTTP server）。
- **Account credentials 持久化**：当前 `AcmeConfig` 持有 `AccountCredentials`（serde 序列化）；生产路径
  须配合 keyring/KMS 落盘（与 CA 私钥 / JWT secret 同存储策略，留 TODO）。
- **ARI（ACME Renewal Information）**：instant-acme 0.8 支持 ARI（`RenewalInfo`）；续期调度可从「固定
  threshold_days」升级为「按 ARI 建议窗口」——留 TODO，需 LE 生产环境支持 ARI。
- **External Account Binding (EAB)**：部分 ACME 服务器（ZeroSSL / Google Trust Services）要求 EAD；
  `AccountBuilder::create` 已支持 `external_account: Option<&ExternalAccountKey>`——`AcmeConfig` 预留字段。
