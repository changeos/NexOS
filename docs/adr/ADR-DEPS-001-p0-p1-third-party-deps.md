# ADR-DEPS-001：注册 P0/P1 高频第三方依赖到 `workspace.dependencies`

- **状态**：已采纳（Accepted）
- **日期**：2026-08-05
- **背景决策来源**：`docs/DEPENDENCIES.md`（17 份 PROGRESS.md + 全 workspace `grep TODO` 汇总）§4「高频共享依赖」与 §6「后续动作建议」首批/二批注册项
- **影响范围**：workspace 根 `Cargo.toml` 的 `[workspace.dependencies]`（仅注册，不改任何 crate 级 `[dependencies]`）

---

## 背景

"无依赖骨架策略"阶段（ADR-COMPAT 系列），全部 17 个 owner agent 仅用已注册基础集
（`serde`/`serde_json`/`thiserror`/`anyhow`/`bytes`/`chrono`/`uuid`/`tokio`/`futures`/
`async-trait`/`tracing`）完成纯逻辑实现与单测。所有需要第三方 crate 的真实 I/O 路径一律
`TODO`/`todo!()` 占位，并在 `docs/DEPENDENCIES.md` 汇总为待注册清单。

清单按"被几个 crate 共享"排序出高频依赖（`reqwest` 6 处最高，`axum`/`nftnl`/`argon2`/
`ed25519-dalek`/`rtnetlink`/`tantivy`/`jsonwebtoken` 各 2 处）。这批依赖注册一次即可解锁多条
实现路径（ROI 最高），是各 agent 切入真实实现的前置。

本 ADR 落实 `docs/DEPENDENCIES.md` §6.1（P0 首批）+ §6.2（P1 二批）的全部条目。

## 决策

在 workspace 根 `[workspace.dependencies]` 注册以下 11 个第三方 crate，**仅注册、不接入**——
任何 crate 级 `[dependencies]` 暂不引用它们；各 owner agent 接通真实实现时按需
`xxx.workspace = true` 引用。

### 注册清单（实际锁定版本）

| crate | 版本（Cargo.lock 锁定） | feature | 用途 | 解锁的 crate |
|-------|----------------------|---------|------|-------------|
| `reqwest` | 0.12.28 | `default-features=false, ["json","rustls-tls"]` | HTTP 客户端（REST/JSON-RPC/CVE 轮询） | os-mobile(client)、os-wallet、os-update、os-guest、os-discover(可选)、os-protocols(object 可选自实现 sigv4) |
| `axum` | 0.8.9 | `["ws"]` | HTTP/WebSocket 服务端 | os-api(Gateway)、os-guest(Captive Portal) |
| `tower` | 0.5.3 | 默认 | HTTP 中间件/服务组合（随 axum） | os-api 中间件链 |
| `hyper` | 1.11.0 | 默认 | HTTP 底层（随 axum，显式注册便于直接用） | os-api、os-guest |
| `jsonwebtoken` | 9.3.1 | 默认 | JWT 签发/校验 | os-security(`JwtIssuer`)、os-guest/api 消费 |
| `argon2` | 0.5.3 | 默认 | 密码 Argon2id 哈希 | os-security(`DbAuthProvider`)、os-services(files 分享密码) |
| `ed25519-dalek` | 2.2.0 | `["rand_core"]` | Ed25519 签名验签 | os-discover(beacon 防伪)、os-update(包验签) |
| `sha2` | 0.10.9 | 默认 | SHA-256 摘要 | os-update(包校验 + 多加密库带入) |
| `nftnl` | 0.7.0 | 默认 | nftables netlink 绑定（FFI） | os-network、os-compute(container)、os-guest |
| `rtnetlink` | 0.16.0 | 默认 | netlink 接口/VLAN/桥配置 | os-network、os-compute(container) |
| `tantivy` | 0.22.1 | 默认 | 全文搜索引擎（BM25/分词） | os-services(files 搜索)、os-services(media 搜索) |

> 完整对应关系与计数见 `docs/DEPENDENCIES.md` §3/§4。本表"解锁的 crate"是潜在消费者，
> 注册不等于接入——接入由各 owner agent 在自己的 ADR/提交中完成。

### 选型理由（事实标准 + 安全考量）

- **`reqwest`**：Rust 生态 HTTP 客户端事实标准（tokio 原生、社区维护最活跃）。
  - **TLS 选 `rustls-tls` 而非默认 `default-tls`（native-tls/openssl）**：避免引入 openssl
    系统依赖（Linux 需 `libssl-dev`、跨平台行为不一致、license 复杂度）。rustls 是纯 Rust
    TLS 实现，无 C 依赖、审计性更好，与 OS"可重现构建 + 最小系统依赖"目标一致。
  - `default-features = false` + 显式 `["json","rustls-tls"]`：关掉 native-tls 默认开关，
    只保留 JSON 编解码 + rustls TLS，构建产物更精简。
- **`axum` + `tower` + `hyper`**：tokio 圈 HTTP 服务端事实标准组合（axum 是 tower/hyper 之上
  的 ergonomic 路由层）。三者通常成套注册（axum 已自动依赖 hyper/tower，显式注册便于 crate
  直接使用 hyper/tower 类型而不必走 axum re-export）。`ws` feature 开启 axum 内建 WebSocket
  支持（`AxumWsHub`、Captive Portal 实时通道所需）。
- **`jsonwebtoken`**：纯 Rust JWT 库，HMAC/RSA/ECDSA/EdDSA 全算法覆盖，社区最常用。版本 9
  是稳定主线（最新 11.0.0 API 变化较大，本批先用广泛部署的 9）。
- **`argon2`**：RustCrypto 家族 Argon2id 实现（PHC 字符串格式、`password-hash` trait）。
  版本 0.5 是最新**稳定**线（0.6 仍是 -rc，不用于安全关键路径）。
- **`ed25519-dalek`**：dalek cryptography 维护，纯 Rust Ed25519 事实标准。`rand_core` feature
  暴露密钥生成入口（需注入 CSPRNG）。用于 beacon 防伪（discover）与**安全关键**的更新包验签
  （update `AbUpdateEngine::verify`，不可绕过）。
- **`sha2`**：RustCrypto SHA-2 家族，与 `argon2`/`ed25519-dalek` 同生态（共享 `digest` trait）。
- **`nftnl`**：mullvad 维护的 libnftnl 安全绑定（nftables netlink 用户态接口）。
  - **注意 FFI**：build script 经 `nftnl-sys`→`mnl-sys` 链接系统库 `libnftnl` + `libmnl`，
    集成环境须 `apt install libnftnl-dev libmnl-dev`（或等价）。本 ADR 注册阶段不涉及链接。
- **`rtnetlink`**：rust-netlink 组织维护的 rtnetlink 绑定（接口/VLAN/桥/地址配置）。
- **`tantivy`**：Rust 全文搜索引擎事实标准（BM25、分词器、列式存储、并发索引）。

## 备选方案与否定理由

1. **按各 owner agent 单独注册（分散）**——同一 crate（如 `reqwest` 6 处、`nftnl` 3 处）
   会在多个 crate 级 `Cargo.toml` 各自写版本，易漂移、版本不一致。统一 workspace 注册锁版本，
   一次升级全局生效。否定分散方案。
2. **用最新大版本（reqwest 0.13 / tantivy 0.26 / ed25519-dalek 3.0 / sha2 0.11）**——这些是
   最新但部分有 breaking change（如 ed25519-dalek 3.0、jsonwebtoken 11.0），且社区迁移尚在进行。
   本批优先**保守稳定线**（文档充分、部署广泛、依赖树已验证），降低真实接入阶段的返工风险。
   升级时机由后续独立 ADR 评估。否定盲目追新。
3. **reqwest 默认 TLS（openssl）**——见上"安全考量"，rustls 优先。

## 代价

- **`Cargo.lock` 体积**：注册 11 个 crate 引入其完整依赖树（reqwest/tantivy 各带数十传递依赖），
  锁文件增大、首次 `cargo fetch` 下载量增加。可接受（CI 缓存复用）。
- **版本锁定**：所有消费者共享同一版本，单 crate 想用不同版本需 `path`/`git` override
  （目前无此需求）。
- **`nftnl` FFI 系统依赖**：链接需 `libnftnl`/`libmnl` 开发包；非 FFI crate 无此开销。
- **未接入即注册**：注册但不引用不增加任何编译产物体积（仅 lockfile 记录）；owner agent
  接入后才真正编译进对应 crate。

## 验证

1. **版本解析验证**：通过临时 crate 引用全部 11 个 workspace dep，`cargo generate-lockfile`
   成功锁定所有版本（无"version not found"）；`cargo check -p <临时>` 对除 `nftnl` 外的 10 个
   crate 完整编译通过（含 `reqwest` 确实走 rustls 而非 openssl：观察到 `rustls`/`tokio-rustls`/
   `hyper-rustls` 编译、无 `native-tls`/`openssl-sys` 编译）。`nftnl 0.7.0` 在 lockfile 中成功
   锁定，仅 build script 因本环境缺 `libmnl` 系统库未链接——**版本/feature 解析维度已验证通过，
   FFI 链接为 owner-agent 集成环境职责**。
2. **注册不破坏现有编译**：移除临时 crate 后，11 个依赖无任何 crate 引用，workspace 编译产物
   不变；现有 22 crate 行为不受影响。

## 对既有约定的影响

- workspace 根 `Cargo.toml` `[workspace.dependencies]` 新增"P0/P1 第三方依赖"分区（带注释与
  ADR 引用），与原基础/异步/日志/内部 crate 分区并列。
- 不改任何 crate 级 `Cargo.toml`（红线遵守）。
- `docs/DEPENDENCIES.md` §3/§4 的"待注册"状态随本 ADR 转为"已注册（待接入）"；该文档为只读
  分析产物，不回改原文，仅以本 ADR 增补覆盖。

## 后续

- 各 owner agent 接入真实实现时：crate 级 `Cargo.toml` 加 `xxx.workspace = true`，源码 `use`
  对应类型，移除对应 `TODO`/`todo!()`。
- **P2 领域专用依赖**（openraft/rusqlite/virt/youki/rust-bitcoin/dav-server/...）由各 agent
  按业务优先级单独提 ADR（见 `docs/DEPENDENCIES.md` §6.3）。
- **版本升级**：本批锁定保守稳定线；后续若需追新（如 reqwest 0.13、tantivy 0.26），由独立 ADR
  评估 breaking change 与迁移成本。
