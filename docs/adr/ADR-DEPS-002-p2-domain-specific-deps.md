# ADR-DEPS-002：注册 P2 领域专用依赖到 `workspace.dependencies`

- **状态**：已采纳（Accepted）
- **日期**：2026-08-05
- **背景决策来源**：`docs/DEPENDENCIES.md` §6.3「P2 领域专用」+ 主规划 §15 接口契约索引（各 owner agent 真实实现路径）
- **影响范围**：workspace 根 `Cargo.toml` 的 `[workspace.dependencies]`（仅注册，不改任何 crate 级 `[dependencies]`）
- **前置 ADR**：ADR-DEPS-001（P0/P1 高频共享依赖）

---

## 背景

ADR-DEPS-001 完成最高频共享依赖（`reqwest` 6 处、`axum`/`nftnl`/`argon2`/`ed25519-dalek` 等）的注册，
解锁了"被多个 crate 共享"的实现路径。本 ADR 推进 **P2 领域专用依赖**——每个 crate 归属明确、
复用面窄（多为一对一：某 crate → 某 crate）的领域库，覆盖共识/存储/虚拟化/区块链/网络协议/资源
管控/可观测/安全/i18n/开发工具 10 个领域。

这批依赖是各 owner agent 把对应领域从 `todo!()`/mock 推进到真实实现的最后一公里前置：例如
`os-meta` 要从 `MockConsensusEngine` 走到真实 Raft 必须有 `openraft`，`os-wallet` 要从占位
钱包走到真实 BTC/EVM 签名必须有 `bitcoin`/`alloy`/`secp256k1`。

## 决策

在 workspace 根 `[workspace.dependencies]` 注册以下 18 个第三方 crate，**仅注册、不接入**——
任何 crate 级 `[dependencies]` 暂不引用它们；各 owner agent 接通真实实现时按需
`xxx.workspace = true` 引用。

### 注册清单（实际锁定版本）

> 版本列已通过临时 crate 全量引用 + `cargo generate-lockfile` + `cargo check` 验证可解析、
> 可编译（见「验证」节）。`锁定版本` 为 lockfile 实际记录值。

| crate | workspace 声明 | 锁定版本 | feature | 用途（归属 crate） |
|-------|---------------|---------|---------|-------------------|
| `openraft` | `"0.9"` | 0.9.25 | 默认 | os-meta 真实 Raft 共识（替代 MockConsensusEngine） |
| `rusqlite` | `"0.32"` | 0.32.1 | `["bundled"]` | os-meta MetaStore SQLite（bundled 内嵌编译，规避系统 sqlite） |
| `virt` | `"0.4"` | 0.4.3 | 默认 | os-compute(vm) 真实 KVM/QEMU（libvirt FFI 绑定） |
| `bitcoin` | `"0.32"` | 0.32.102 | 默认 | os-wallet BTC（rust-bitcoin；0.33 仍 beta，不采用） |
| `alloy` | `"2"` | 2.3.0 | 默认 | os-wallet EVM（纯 Rust，选 alloy 而非 ethers） |
| `secp256k1` | `"0.31"` | 0.31.1 | 默认 | os-wallet 验签（BTC/EVM 共享，与 bitcoin 同生态） |
| `dav-server` | `"0.11"` | 0.11.0 | 默认 | os-protocols WebDAV（基于 axum/hyper） |
| `libunftp` | `"0.23"` | 0.23.0 | 默认 | os-protocols FTP 服务端 |
| `russh` | `"0.62"` | 0.62.5 | 默认 | os-protocols SFTP（纯 Rust SSH） |
| `mdns-sd` | `"0.20"` | 0.20.3 | 默认 | os-discover mDNS（纯 Rust，无 avahi 系统依赖） |
| `rustls` | `"0.23"` | 0.23.43 | `default-features=false, ["ring"]` | os-discover mTLS 终结（与 reqwest rustls-tls 共栈） |
| `cgroups-rs` | `"0.5"` | 0.5.1 | 默认 | osd 守护进程 cgroup v2 资源限制 |
| `toml` | `"1"` | 1.1.4+spec-1.1.0 | 默认 | os-i18n TOML 解析（自实现 translator，选 toml 而非 rust-i18n） |
| `opentelemetry` | `"0.32"` | 0.32.0 | 默认 | os-services(monitor) 指标采集 |
| `opentelemetry-prometheus` | `"0.32"` | 0.32.0 | 默认 | os-services(monitor) Prometheus exporter（与 opentelemetry 版本同步） |
| `rcgen` | `"0.14"` | 0.14.8 | 默认 | os-security CA 自签 X.509 证书 |
| `boringtun` | `"0.7"` | 0.7.1 | 默认 | os-security WireGuard VPN（userspace，Cloudflare 维护） |
| `gix` | `"0.86"` | 0.86.0 | 默认 | os-services(devtools) 纯 Rust Git（gitoxide 旗舰 crate） |

### 版本选择策略

- **保守大版本优先**（与 ADR-DEPS-001 同策略）：`openraft`/`bitcoin`/`secp256k1`/`rusqlite` 锁定
  当前**稳定主线**而非最新预发布（如 `openraft 0.9` 而非 `0.10-alpha`、`bitcoin 0.32` 而非
  `0.33-beta`、`secp256k1 0.31` 而非 `0.32-beta`、`rusqlite 0.32` 而非最新 `0.40`）。理由：
  这些 crate 的真实接入尚未发生，先用文档充分、部署广泛的稳定线降低未来返工风险。
- **追新但只锁大版本**：`virt`/`alloy`/`dav-server`/`libunftp`/`russh`/`mdns-sd`/`cgroups-rs`/
  `rcgen`/`boringtun`/`gix`/`toml`/`opentelemetry` 采用最新稳定大版本（声明里只写主版本号如
  `"0.11"`/`"2"`/`"1"`，让 Cargo 在大版本内取最新补丁），因为这些库无明显 breaking 风险且最新线
  功能更全。
- **`rustls`**：`default-features = false` + `["ring"]`，与 ADR-DEPS-001 的 `reqwest rustls-tls`
  共用同一 TLS 栈（ring 后端），避免引入 aws-lc-rs（需 C 编译器 + nasm）。`mdns-sd` 选用纯 Rust
  实现而非绑定系统 avahi/ZeroConf，与"最小系统依赖"目标一致。

### 选型理由（含已定争议项）

- **EVM：alloy 而非 ethers**（已定）。alloy 是 foundry/paradigm 维护的纯 Rust 以太坊栈
  （`alloy-provider`/`alloy-rpc-client`/`alloy-contract` 全套），API 现代、异步原生、无 ethers-rs
  的历史包袱。本次验证 `alloy 2.3.0` 全套（含 `alloy-transport-http`/`alloy-provider`/
  `alloy-contract`）完整编译通过。
- **i18n：toml 而非 rust-i18n**（已定）。os-i18n 已自实现 translator（trait + 加载逻辑），
  只缺 TOML 文件解析能力，选基础 `toml` crate 而非完整 i18n 框架，避免引入不必要的宏/运行时假设。
- **SQLite：rusqlite + `bundled`**（已定）。`bundled` feature 触发 `libsqlite3-sys` 内嵌编译
  SQLite C 源码，**避免** OS 部署机预装 `libsqlite3-dev`，与"最小系统依赖 + 可重现构建"一致。
  代价：首次编译多耗 ~30s（C 源码编译），可接受。
- **Git：gix 而非 git2/libgit2**。gix 是 gitoxide 纯 Rust 实现，无 libgit2 FFI 系统依赖，
  与 nftnl/libvirt 那类 FFI 解耦，构建更干净。
- **SFTP：russh 而非 libssh2**。russh 纯 Rust SSH 实现，无 libssh2 FFI。
- **VPN：boringtun 而非 wireguard-go**。boringtun 是 Cloudflare 维护的 userspace WireGuard
  Rust 实现，无需内核 WG 模块，跨场景部署（容器/用户态隧道）更灵活。

## 备选方案与否定理由

1. **按各 owner agent 分散注册**——与 ADR-DEPS-001 同否定理由：版本漂移、不一致。统一 workspace
   注册锁版本，一次升级全局生效。否定。
2. **本次一并注册 P3（object-agent S3 / ACME）**——S3 自实现 sigv4 已在 P0 接通 `reqwest`，
   无需额外 crate；ACME 暂缓（无明确 owner agent 优先级）。本批聚焦"已确认归属且无争议"的 18 项，
   P3 待后续 ADR。否定本次纳入。
3. **`secp256k1` 升到 0.32（beta）**——beta 不用于安全关键路径（钱包验签），降级回 0.31 稳定线。
4. **`rustls` 用默认 aws-lc-rs 后端**——aws-lc-rs 需 C 编译器 + nasm（Windows 麻烦），改用 `ring`
   后端，与 reqwest 共栈、构建更顺。否定默认后端。

## 代价

- **依赖树膨胀（潜在）**：18 个 crate 全量引用后传递依赖显著（alloy 带入 ethers-v2 栈、gix 带入
  gitoxide 全套、boringtun 带入 ring/curve25519 等）。但**注册不引用不进 lockfile/不编译**（见
  验证），仅当 owner agent crate 级引用后才真实计入。
- **FFI 系统依赖（virt）**：`virt` 经 build script 链接系统 `libvirt`，集成环境须
  `apt install libvirt-dev`。**本次验证环境无 libvirt-dev，`virt 0.4.3` 仍成功 check 通过**
  （crate 级 build script 在无 libvirt 时生成空绑定/不链接——版本/feature 解析维度已验证；真正
  链接为 owner-agent（compute-agent）集成环境职责）。注：`rusqlite` 因用 `bundled` 已规避此问题。
- **多版本共存**：`secp256k1` 在 lockfile 中出现 0.29（bitcoin 传递）/0.30（其他传递）/0.31（直接）
  三个次版本；`reqwest` 出现 0.12（直接）/0.13（alloy 传递）两个主版本。这是 Cargo 在不同 major
  间的正常行为，各自独立编译、互不冲突，可接受。
- **版本锁定**：所有消费者共享同一版本，单 crate 想用不同版本需 `path`/`git` override（目前无此需求）。

## 验证

1. **版本解析 + 编译验证（临时 crate 法，复用 ADR-DEPS-001 验证模式）**：
   临时新建 `crates/.verify-p2-tmp/`（引用全部 18 个 workspace dep），加入 workspace `members`，
   `cargo generate-lockfile` 成功锁定 **832 个包**（含全部 18 个 P2 crate 及其传递依赖），
   `cargo check -p verify-p2` **完整编译通过**（exit 0，5m41s），18 个 crate 全部 `Checking ... Finished`：
   `openraft 0.9.25`/`rusqlite 0.32.1`/`virt 0.4.3`/`bitcoin 0.32.102`/`alloy 2.3.0`/
   `secp256k1 0.31.1`/`dav-server 0.11.0`/`libunftp 0.23.0`/`russh 0.62.5`/`mdns-sd 0.20.3`/
   `rustls 0.23.43`/`cgroups-rs 0.5.1`/`toml 1.1.4`/`opentelemetry 0.32.0`/
   `opentelemetry-prometheus 0.32.0`/`rcgen 0.14.8`/`boringtun 0.7.1`/`gix 0.86.0`。
   验证后删除临时 crate、回退 `members`。
2. **注册不破坏现有编译 + 不改 lockfile**：删除临时 crate 后 `cargo generate-lockfile` 产生的
   `Cargo.lock` 与 HEAD **零 diff**——证实 P2 依赖"注册不引用"状态下不进 lockfile、不改变任何编译
   产物（仅当 crate 级引用后才计入）。`cargo check --workspace` 通过（1.6s，无 P2 重编译）。
3. **mock feature 全量通过**：对全部 21 个定义 `mock` feature 的 crate 逐一 `cargo check -p <crate>
   --features mock` 全部 `Finished`（注册与 mock 路径正交，互不影响）。

## 对既有约定的影响

- workspace 根 `Cargo.toml` `[workspace.dependencies]` 新增"P2 领域专用依赖（ADR-DEPS-002）"分区
  （按归属 crate 分组带注释：共识存储/虚拟化/区块链/网络协议/设备发现/资源管控/i18n/可观测/安全/
  开发工具），紧随 ADR-DEPS-001 的 P0/P1 分区。
- 不改任何 crate 级 `Cargo.toml`（红线遵守）。
- `docs/DEPENDENCIES.md` §6.3 的"待注册"状态随本 ADR 转为"已注册（待接入）"；该文档为只读分析产物，
  不回改原文，仅以本 ADR 增补覆盖。

## 后续

- 各 owner agent 接入真实实现时：crate 级 `Cargo.toml` 加 `xxx.workspace = true`，源码 `use`
  对应类型，移除对应 `TODO`/`todo!()`/mock 路径（mock 保留为测试桩，由 `mock` feature 控制）。
- **FFI 集成环境前置**：`virt` 接入前须确保构建机有 `libvirt-dev`（`rusqlite` 已用 bundled 规避）。
- **P3 后续 ADR**：object-agent S3（如需额外 crate）、ACME（证书自动签发）等暂缓项由独立 ADR 评估。
- **版本升级**：本批锁定稳定线/大版本；后续若需追新（如 openraft 0.10 stable、bitcoin 0.33 stable
  落地后），由独立 ADR 评估 breaking change 与迁移成本。
