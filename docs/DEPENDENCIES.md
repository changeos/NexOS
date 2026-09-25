# 待注册第三方依赖清单（真实集成阶段输入）

> ⚠️ **状态：历史归档（已完成）**。本清单是骨架期产出的"待注册阻塞清单"，用于驱动后续批量注册 + ADR。
> **P0/P1（ADR-DEPS-001，11 个）+ P2（ADR-DEPS-002，20 个）+ P3（ADR-DEPS-003 aes-gcm / ADR-DEPS-004 instant-acme）第三方依赖已全部注册到 workspace.dependencies（共 73 个含内部 crate path 依赖 + clap/tracing-subscriber/criterion）并被各 crate 按需引用，接通真实实现**（main `4eb29cb`，1935 测全绿）。
> 本文件保留作历史归档与选型理由索引；**当前真实状态以 `docs/adr/ADR-DEPS-00{1,2,3,4}-*.md` + workspace 根 `Cargo.toml [workspace.dependencies]` 为准**。
> 详见 [HANDOVER.md](./HANDOVER.md) §3（已定决策）。

---

> 本清单汇总全 workspace 各 owner agent 在"无依赖骨架策略"下报告的**阻塞项**——
> 即遇到未注册第三方 crate 时只做骨架 + `TODO` 的部分。它是后续"批量注册依赖 + ADR"
> 阶段的直接输入。
>
> 来源：
> - `docs/agents/*/PROGRESS.md`（17 份，覆盖 core/i18n/orchestrator/api/client/im/meta/
>   protocol/provision/update/discover/vm/object/wallet/container/iso/guest 共 17 个 owner agent）
> - 全 workspace 源码 `grep TODO`（`crates/*/src/*.rs`），提取标注了外部依赖的位置。
>
> 编排口径：
> - 已注册于 `[workspace.dependencies]`（serde/serde_json/thiserror/anyhow/bytes/chrono/uuid/
>   tokio/futures/async-trait/tracing）**不计入**本清单（已可用）。
> - 仅"骨架可测 / 真实 I/O 占位"且**明确点名某第三方 crate 未注册**者才计入。
> - 纯集成阶段阻塞（root/systemd/沙箱/嵌套虚拟化等运行时环境）单独标注，不算 crate 注册项。
> - 各 agent trait 签名零改动；本清单不涉及契约变更，仅记录"待引入的库"。
>
> 红线遵守：本文件为只读分析产物，不修改任何源码，仅新建本文档。

---

## 1. 已注册依赖（现状，无需动作）

`Cargo.toml [workspace.dependencies]` 当前已注册：`serde`、`serde_json`、`thiserror`、
`anyhow`、`bytes`、`chrono`、`uuid`、`tokio`（full）、`futures`、`async-trait`、`tracing`，
以及全部内部 `os-*` crate。骨架阶段所有 agent 仅用这一集合即完成纯逻辑实现与测试。

---

## 2. 按 crate 分组的待注册依赖清单

> 每条记录：**第三方 crate** | 用途 | 阻塞的 trait/方法 | PROGRESS / 源码引用。

### 2.1 `os-core` / `os-common`（core-agent）
- **无外部阻塞项**。core-agent 已交付 `TokioBroadcastBus` + `MockEventBus`，无第三方 crate 缺口。
- 参考：`docs/agents/core-agent/PROGRESS.md`（阻塞：无）。

### 2.2 `os-i18n`（i18n-agent）
- **`toml`** | 完整 TOML 解析（当前自实现"扁平 TOML 子集"解析器 `parse_flat_toml`，不支持数组/多行/嵌套表）。 | `BundleTranslator` 内部解析器（trait `Translator` 不受影响）。 | `PROGRESS.md` §决策；`crates/os-i18n/src/impl_translator.rs`。
  - 可选：`rust-i18n`（更高层封装，规格书 §3 推荐，需 ADR 评估是否替代自实现）。

### 2.3 `osd`（orchestrator-agent）
- **`cgroups-rs`** | cgroup v2 配额写入（CPU/memory/io 限制）。 | `Orchestrator::set_quota` / `get_quota` 真实 cgroup 写入；`CgroupQuota`。 | `PROGRESS.md` §阻塞项；`crates/osd/src/impl_orchestrator.rs`（标 `TODO(集成阶段)`）。
- **chrony 绑定**（无成熟 Rust crate，可能需 FFI/CLI 编排） | NTP 同步管理。 | `ChronyNtp`（`NtpManager` 真实实现）。 | `PROGRESS.md` §阻塞项；`crates/osd/src/ntp.rs`。
- 运行时硬阻塞（非 crate）：真实 systemd unit 生成 + 进程监管 + 退避重启需 root + systemd + CAP_SYS_ADMIN + 沙箱。

### 2.4 `os-api` / `os-cli`（api-agent）
- **`axum`** | 真实 HTTP/WS 监听 bind/serve。 | `InProcessGateway::start`（Gateway trait）；`AxumWsHub`（与 EventBus 对接）。 | `PROGRESS.md` §阻塞；`crates/os-api/src/gateway_impl.rs`（`TODO(Axum 接入)`）。
- **`tower`** | HTTP 中间件/服务组合（与 axum 配套）。 | Gateway 中间件链真实挂载。 | 同上。
- **`hyper`** | 底层 HTTP（axum 依赖，常一并注册）。 | HTTP 监听底层。 | 同上。
- **`serde_yaml`** | YAML 序列化（YamlFormatter 当前为极简自实现）。 | `YamlFormatter`（`OutputFormatter`）。 | `PROGRESS.md` §阻塞；`crates/os-cli/src/format_impl.rs`。

### 2.5 `os-mobile` / `os-desktop`（client-agent）
- **`reqwest`** | 真实 HTTP 传输（OS 网关 REST 调用）。 | `HttpOsClient::{status, discover, pair}`（`OsClient` trait）。 | `PROGRESS.md` §阻塞；`crates/os-mobile/src/client_impl.rs`（`TODO(reqwest)` ×3）。
- 运行时阻塞（非 crate）：`SystemMountManager` 真实 `net use`/`mount -t davfs` 执行（命令已构造，待 `std::process`/桌面运行时）；FCM/APNs 平台 SDK（移动端运行时）。

### 2.6 `os-im`（im-agent）
- **LLM SDK**（`candle` 本地推理 / `ollama` / OpenAI HTTP 客户端） | 真实 LLM 后端。 | `OpenAiBackend` / `LocalCandleBackend` / `CustomBackend`（`LlmBackend` trait）。 | `PROGRESS.md` §阻塞；`crates/os-im/src/llm.rs`。
- 软依赖（非第三方 crate）：`SqliteConversationStore` / `DistributedBlackboard` 待 meta-agent `MetaStore` 接口接入（内部 crate 协作）。

### 2.7 `os-meta`（meta-agent）
- **`openraft`** | 真实 Raft 共识（leader 选举 / 日志复制 / 成员变更）。 | `OpenraftConsensus` / `OpenraftKv`（`Consensus` / `DistributedKv` trait）。 | `PROGRESS.md` §后续阻塞项；`crates/os-meta/src/impls.rs`（`TODO(openraft)`）。
- **`rusqlite`**（或等价 SQLite 绑定） | 真实 SQLite 持久化后端。 | `SqliteMetaStore`（`MetaStore` trait：dump/restore/execute）。 | `PROGRESS.md` §后续阻塞项；`crates/os-meta/src/impls.rs`（`TODO(sqlite)` ×4）。
- **netlink 绑定**（`rtnetlink` / `netlink-packet-route` 等） | VIP 漂移 + ARP 通告。 | `NetlinkVipManager`（`VipManager` trait）。 | `PROGRESS.md` §后续阻塞项；`crates/os-meta/src/impls.rs`（`TODO(netlink)`）。
- 软依赖：`HaFailoverOrchestrator` 真实 VM 迁移执行待 os-compute `VmManager` mock 就绪。

### 2.8 `os-protocols` — 文件协议（protocol-agent）
- **`dav-server`** | 内置 WebDAV HTTP 服务路由挂载。 | `DavServerBackend::{mount,unmount}`（`WebDavManager`）。 | `PROGRESS.md` §下一步；`crates/os-protocols/src/orchestrators.rs`（`TODO(协议栈)` ×多）。
- **`libunftp`** | 内置 FTP 服务实例挂载。 | `LibunftpBackend::mount`（`FtpManager`）。 | 同上。
- **`russh`** | 内置 SSH/sftp-subsystem + 写 per-user `authorized_keys`。 | `RusshSftpBackend::{mount,unmount,authorize_key}`（`SftpManager`）。 | 同上。
- 运行时阻塞（非 crate）：SMB 编排 Samba（CLI `smbcontrol`/`smbstatus`，无纯 Rust 实现，走 `tokio::process`）；NFS `exportfs`/ganesha reload（CLI 编排）。

### 2.9 `os-protocols` — 对象存储（object-agent）
- **S3 客户端库**（选型待裁决：`aws-sdk-s3` / `rusoto` / 自实现 `reqwest`+sigv4） | 真实 S3 REST 调用（PUT/GET/DELETE/LIST/multipart）。 | `RustFsObjectStore` 全方法（`ObjectStore` trait 9 方法）。 | `PROGRESS.md` §阻塞；`crates/os-protocols/src/object.rs`（`TODO` ×多，SigV4 字符串构造已完成）。
- 注：若选 `reqwest`+sigv4 自实现，则与 client/wallet/guest 共享 `reqwest` 注册。
- 软依赖：access key secret 端到端转交流程待 wallet-agent 凭证消费契约。

### 2.10 `os-compute` — VM（vm-agent）
- **`virt`（libvirt Rust 绑定）** | 真实 KVM/libvirt 域操作。 | `LibvirtVmManager::{create,destroy,start,stop,pause,resume,migrate,list,status}`（`VmManager` trait）。 | `PROGRESS.md` §阻塞；`crates/os-compute/src/impl_vm.rs`（`TODO(libvirt)` ×9）。
- 运行时阻塞（非 crate）：root/libvirt 组权限、KVM 提权环境。

### 2.11 `os-compute` — 容器/网络/包（container-agent）
- **`youki`**（或 `libcontainer`） | OCI 容器运行时。 | `YoukiRuntime`（`ContainerRuntime`）。 | `PROGRESS.md` §阻塞；`crates/os-compute/src/`。
- **`oci-distribution`** | OCI 镜像拉取。 | `YoukiRuntime` 镜像拉取路径。 | 同上。
- **`libcni`**（或 CNI Rust 绑定） | CNI 网络插件链应用。 | `CniContainerNetwork`（`ContainerNetwork`）。 | `PROGRESS.md` §阻塞。
- **`rtnetlink`** | 容器网络 netlink 配置。 | `CniContainerNetwork` 底层。 | 同上。
- **`nftnl`** | 容器网络 nftables 规则。 | `CniContainerNetwork` 防火墙集成。 | 同上。
- 注：`DpkgPackageManager`（apt CLI 编排）**可立即推进**（apt 是 CLI，tokio::process 已在）。

### 2.12 `os-wallet`（wallet-agent）
- **`rust-bitcoin`** | BTC BIP-322 / Schnorr / ECDSA 验签 + UTXO 查询。 | `BitcoinAdapter::{verify_signature, query_credential}`（`ChainAdapter`）。 | `PROGRESS.md` §阻塞；`crates/os-wallet/src/chain.rs`（`TODO(wallet-agent)`）。
- **`alloy`**（或 `ethers`） | EVM EIP-191 / EIP-712 验签 + RPC 调用。 | `EvmAdapter::{verify_signature, query_credential}`。 | 同上。
- **`secp256k1`** | secp256k1 椭圆曲线（BTC/EVM 共用）。 | 上述两 adapter 验签底层。 | 同上。
- **`keccak256`**（或经 alloy 带入） | EVM 地址/类型哈希。 | `EvmAdapter` 验签。 | 同上。
- **`reqwest`** | RPC 探活（BTC `getblockchaininfo` / EVM `eth_blockNumber` JSON-RPC）。 | `RpcRegistryImpl::probe`（`RpcRegistry`）。 | `PROGRESS.md` §阻塞；`crates/os-wallet/src/registry.rs`（`TODO(wallet-agent)`）。
- **`walletconnect-relay`**（WalletConnect v2 SDK） | relay 配对 + 签名转发。 | `WalletConnectV2Connector` / `QrCodeConnector`（`WalletConnector`）。 | `PROGRESS.md` §阻塞；`crates/os-wallet/src/connector.rs`（`TODO(wallet-agent)` ×多）。

### 2.13 `os-discover`（discover-agent）
- **`mdns-sd`** | mDNS 组播广播/扫描。 | `MdnsDiscovery`（`Discovery` trait 真实组播 IO）。 | `PROGRESS.md` §阻塞；`crates/os-discover/src/impls.rs`（`TODO`）。
- **`rustls`** | mTLS 双向认证。 | `MtlsPeerAuthenticator`（`PeerAuthenticator`）。 | `PROGRESS.md` §下一步；`crates/os-discover/src/auth.rs`。
- **`ed25519-dalek`** | beacon 防伪签名验签。 | `verify_beacon_signature` 真实验签。 | `PROGRESS.md` §阻塞；`crates/os-discover/src/beacon.rs`（`TODO`，结构校验已完成）。

### 2.14 `os-update`（update-agent）
- **签名库**（`ed25519`/`ed25519-dalek` + `sha2`） | 更新包验签 + sha256 比对（**安全关键，不可绕过**）。 | `AbUpdateEngine::verify`（当前 `todo!()`，强制不可跳过校验）。 | `PROGRESS.md` §下一步；`crates/os-update/src/impls.rs`（`todo!()`）。
- **bootloader/ostree 绑定**（grub-bls / systemd-boot / ostree） | 槽位激活 / 写入。 | `activate_slot` / `write_to_inactive_slot`。 | `PROGRESS.md` §下一步；`crates/os-update/src/impls.rs`。
- **`reqwest`**（或 HTTP 客户端） | NVD/OSV CVE 公告轮询 + 更新源下载。 | `NvdCveMonitor::check_advisories` / `AbUpdateEngine::{check_updates,download}`。 | `PROGRESS.md` §下一步；`crates/os-update/src/cve.rs` / `impls.rs`。
- 软依赖：滚动升级 `execute` 待 os-meta leader 选举（`Consensus::get_members/get_leader`）。

### 2.15 `os-guest`（guest-agent）
- **`axum`** / **`hyper`** | Captive Portal HTTP/HTTPS 真实监听。 | `HttpCaptivePortal::start`（当前内存态，`handle_detection` 完整）。 | `PROGRESS.md` §阻塞；`crates/os-guest/src/impls.rs`（`TODO(guest-agent)`）。
- **`nftnl`** | nftables 真实 apply/revoke/rollback。 | `NftRuleOrchestratorImpl::{apply,revoke,rollback}`（字符串构造 + dry-run/checkpoint 已完整）。 | `PROGRESS.md` §阻塞；`crates/os-guest/src/impls.rs`（`TODO(guest-agent)`）。
- **`jsonwebtoken`**（经 os-security） | JWT 签发。 | `DefaultChainOrchestrator` 中调 `os-security::JwtIssuer`。 | `PROGRESS.md` §阻塞。
- 软依赖：IdentityEngine/PolicyEngine 切 MetaStore（待 os-meta）。

### 2.16 `os-iso`（iso-agent）
- **无第三方 crate 注册阻塞**。xorriso/mksquashfs/sha256sum 经 `tokio::process::Command` 编排（workspace 已有 tokio）。
- 运行时硬阻塞（非 crate）：沙箱 + 工具链 + 嵌套虚拟化（真实 spawn 子进程、裸机写盘/建池、`/proc/cpuinfo`/`lsblk` 探测）。
- 参考：`docs/agents/iso-agent/PROGRESS.md`（阻塞：无，骨架无外部阻塞）。

### 2.17 `os-provision`（provision-agent）
- **无第三方 crate 注册阻塞**。敏感项排除/断点续传/迁移包均为纯逻辑；指纹用 `std::hash::DefaultHasher`（未引入 sha2）。
- 软依赖（内部 crate）：`PxeProvisioner` / `ZfsMigrationEngine` 待 os-network `PxeServer` / os-storage `Replication` / os-meta `MetaStore`。
- 参考：`docs/agents/provision-agent/PROGRESS.md`（阻塞：无）。

### 2.18 `os-storage`（storage-agent，无独立 PROGRESS.md，源码 + README 批次摘要）
- **无第三方 crate 注册阻塞**。`ZfsCliBackend` / `LioBlockExport` 全部走 CLI 编排（`zpool`/`zfs`/`targetcli`/`nvmetcli`/`cryptsetup`），用 workspace 已注册的 `tokio::process::Command`。
- 运行时阻塞（非 crate）：configfs 直写/持久化、passphrase stdin 注入需扩展 `CommandRunner` trait（集成阶段）；root 权限。
- 参考：`crates/os-storage/src/lib.rs` 头注；`crates/os-storage/src/{backend_impl,block_impl,crypto_impl,replication_impl}.rs`（`TODO(集成阶段)`）。

### 2.19 `os-network` — 基础 + RDMA + DPU（network-agent / rdma-agent，无独立 PROGRESS.md）
- **`rtnetlink`** | netlink socket 真实通信（接口/VLAN/桥/绑定）。 | `NetlinkManager`（接口管理 trait 执行层）。 | `crates/os-network/src/backend.rs`（`TODO(netlink-exec)`）。
- **`nftnl`** | nftables 真实事务（含 dry-run + 回滚看门狗）。 | `NftFirewall`（防火墙 trait 执行层）。 | `crates/os-network/src/backend.rs`（`TODO(nftnl-exec)`）。
- **RDMA/DPU**：当前均构造命令文本 + 走进程执行库（`tokio::process`）。可选高级绑定：`ibv_*`/verbs Rust 绑定、Redfish 客户端（`crates/os-network/src/{rdma,dpu}.rs`，`TODO(rdma-agent)`）。优先级低（CLI 编排已可用）。

### 2.20 `os-security`（security-agent，无独立 PROGRESS.md）
- **`argon2`** | 密码 Argon2id 哈希/校验。 | `hash_password` / `verify_password`（`DbAuthProvider` 底层）。 | `crates/os-security/src/password.rs`（`TODO(security-agent)`）。
- **`sha1`** + **`hmac`**（或 `ring`） | TOTP HMAC-SHA1 计算。 | `compute_hmac_sha1`（`TotpTwoFactor` 底层，dynamic truncation 已纯实现）。 | `crates/os-security/src/totp.rs`（`TODO(security-agent)`）。
- **`jsonwebtoken`** | JWT 签发/校验（签名/过期/类型校验 + 密钥热轮换）。 | `JwtIssuerImpl::{sign,verify,...}`（`JwtIssuer`）。 | `crates/os-security/src/impls.rs`（`TODO(security-agent)` ×多）。
- **`rcgen`** | 根 CA 证书 + 私钥生成。 | `CaCertManager::{init_ca,sign,renew}`（`CertManager`）。 | `crates/os-security/src/impls.rs`（`TODO(security-agent)`）。
- **`instant-acme`**（或等价 ACME 客户端） | ACME 证书申请（Let's Encrypt）。 | `CaCertManager::acme_request`。 | `crates/os-security/src/impls.rs`。
- **`boringtun`** | WireGuard VPN 真实运行状态。 | `VpnManager` 真实实现（运行状态当前静态返回）。 | `crates/os-security/src/impls.rs`（`TODO(security-agent)`）。

### 2.21 `os-services`（backup/monitor/media/files/devtools/power，批 3 多 agent，无独立 PROGRESS.md）
- **`tantivy`** | 全文搜索索引（BM25 / 分词 / 高亮 / 向量混合）。 | `FileManager::search`（当前子串匹配占位）；`MediaManager::search`。 | `crates/os-services/src/{impl_files,files_model,media_impl}.rs`（`TODO(tantivy)`）。
- **`opentelemetry`** + **`prometheus`**（+ `tracing-subscriber` 桥接） | metric/log/alert 导出（MeterProvider / OTLP exporter / prometheus registry）。 | `OtelMonitor`（`Monitor` trait 真实导出，当前内存态）。 | `crates/os-services/src/monitor.rs`（`TODO(otel)`）。
- **FFmpeg**（CLI 子进程，经 `tokio::process`） | HLS ABR 转码调度。 | `MediaManager::{transcode,stream_playlist}`。 | `crates/os-services/src/media_impl.rs`（`TODO`，运行时硬阻塞）。
- **CLIP 模型推理**（candle 或 ONNX runtime） | 图片向量嵌入。 | `MediaManager::ingest`（clip_embedding）。 | `crates/os-services/src/media_impl.rs`（`TODO`，运行时硬阻塞）。
- **人脸检测**（模型库，待选型） | 人脸分组（隐私相关，须安全评审）。 | `MediaManager::ingest`（faces）。 | `crates/os-services/src/media_impl.rs`。
- **`gix`** | Git 服务（仓库/分支/提交元数据 + 拉取执行）。 | `DevTools::trigger`（当前返回 `Internal`）。 | `crates/os-services/src/{devtools,impl_devtools}.rs`（`TODO(devtools)`）。
- **AEAD 加密库**（如 `aes-gcm` / `chacha20poly1305`） | 加密 KVS 密钥存储。 | `DevTools::{store_secret,retrieve_secret}`。 | `crates/os-services/src/impl_devtools.rs`（`TODO(devtools)`）。
- **`argon2`**（重复，经 os-security 或直接） | 文件分享链接密码哈希。 | `files_model.rs` 分享密码。 | `crates/os-services/src/files_model.rs`（`TODO(security)`）。
- backup-agent：无第三方 crate 阻塞（GFS 保留/cron 调度/快照策略纯逻辑）；远程 `zfs send|ssh recv` 待 storage `Replication` mock（内部 crate 软依赖）。
- power-agent：无第三方 crate 阻塞（SMART/风扇/温度走 `tokio::process` CLI / sysfs）。

---

## 3. 按 crate 名聚合的统计表

| 第三方 crate | 需要的 crate（agent） | 计数 | 用途一致性 | 高频共享 |
|---|---|---|---|---|
| **`reqwest`** | os-mobile(client)、os-wallet、os-discover(可选)、os-update、os-guest、os-protocols(object 可选) | **6** | HTTP 客户端（REST/JSON-RPC/CVE 轮询/S3） | ★★★ 最高频 |
| **`axum`** | os-api、os-guest | 2 | HTTP/WS 监听（Gateway / Captive Portal） | ★★ |
| **`hyper`** | os-api、os-guest | 2 | HTTP 底层（随 axum） | ★★ |
| **`nftnl`** | os-network、os-compute(container)、os-guest | 3 | nftables 真实事务 | ★★ |
| **`rtnetlink`** | os-network、os-compute(container) | 2 | netlink 接口/网络配置 | ★★ |
| **`ed25519-dalek`** / `ed25519` | os-discover、os-update、(os-wallet 经 rust-bitcoin) | 2+ | 签名验签（beacon / 更新包） | ★★ |
| **`sha2`** | os-update、(经多个加密库带入) | 1+ | sha256 比对 | — |
| **`argon2`** | os-security、os-services(files) | 2 | 密码 Argon2id 哈希 | ★★ |
| **`tower`** | os-api | 1 | HTTP 中间件（随 axum） | — |
| **`serde_yaml`** | os-cli | 1 | YAML 序列化 | — |
| **`toml`** | os-i18n | 1 | TOML 解析 | — |
| **`cgroups-rs`** | osd | 1 | cgroup v2 配额 | — |
| chrony 绑定 | osd | 1 | NTP 同步 | — |
| **`openraft`** | os-meta | 1 | Raft 共识 | — |
| **`rusqlite`** | os-meta | 1 | SQLite 持久化 | — |
| netlink 绑定 | os-meta、(os-network 已计 rtnetlink) | 1 | VIP 漂移 | — |
| **`dav-server`** | os-protocols | 1 | WebDAV 服务 | — |
| **`libunftp`** | os-protocols | 1 | FTP 服务 | — |
| **`russh`** | os-protocols | 1 | SSH/sftp 服务 | — |
| S3 客户端（`aws-sdk-s3`/`rusoto`/reqwest+sigv4） | os-protocols | 1 | 对象存储 REST | — |
| **`virt`**（libvirt） | os-compute(vm) | 1 | KVM/libvirt 域操作 | — |
| **`youki`**/`libcontainer` | os-compute(container) | 1 | OCI 容器运行时 | — |
| **`oci-distribution`** | os-compute(container) | 1 | OCI 镜像拉取 | — |
| **`libcni`** | os-compute(container) | 1 | CNI 网络插件 | — |
| **`rust-bitcoin`** | os-wallet | 1 | BTC 验签/UTXO | — |
| **`alloy`**（或 ethers） | os-wallet | 1 | EVM 验签/RPC | — |
| **`secp256k1`** | os-wallet | 1 | secp256k1 曲线 | — |
| keccak256（经 alloy） | os-wallet | 1 | EVM 哈希 | — |
| **`walletconnect-relay`** | os-wallet | 1 | WC v2 配对 | — |
| **`mdns-sd`** | os-discover | 1 | mDNS 组播 | — |
| **`rustls`** | os-discover | 1 | mTLS 双向认证 | — |
| bootloader/ostree 绑定 | os-update | 1 | A/B 槽位激活 | — |
| **`sha1`** + **`hmac`**（或 `ring`） | os-security | 1 | TOTP HMAC-SHA1 | — |
| **`jsonwebtoken`** | os-security（+ os-guest 经其消费） | 1+ | JWT 签发/校验 | ★★（跨 crate 消费） |
| **`rcgen`** | os-security | 1 | CA 证书生成 | — |
| **`instant-acme`** | os-security | 1 | ACME 证书 | — |
| **`boringtun`** | os-security | 1 | WireGuard VPN | — |
| **`tantivy`** | os-services(files)、os-services(media) | 2 | 全文搜索索引 | ★★ |
| **`opentelemetry`** + **`prometheus`** | os-services(monitor) | 1 | 可观测性导出 | — |
| FFmpeg（CLI，非 crate） | os-services(media) | 1 | HLS 转码 | — |
| CLIP 推理（candle/ONNX） | os-services(media) | 1 | 图片向量 | — |
| 人脸检测库 | os-services(media) | 1 | 人脸分组 | — |
| **`gix`** | os-services(devtools) | 1 | Git 服务 | — |
| AEAD（`aes-gcm`/`chacha20poly1305`） | os-services(devtools) | 1 | 加密 KVS | — |

---

## 4. 高频共享依赖（多 crate 共用，应优先注册）

> 以下依赖被**多个 crate** 同时需要，注册一次即可解锁多条实现路径，应作为**首批注册**对象。
> 单 crate 独占的领域专用库（如 `openraft`/`virt`/`youki`/`rust-bitcoin`）可按 agent 优先级分批。

| 优先级 | 第三方 crate | 解锁的 crate / agent | 备注 |
|---|---|---|---|
| **P0** | **`reqwest`** | os-mobile(client-agent)、os-wallet、os-update、os-guest、os-discover(可选)、os-protocols(object 可选自实现) | **6 处**。HTTP 传输是跨 agent 最高频缺口；注册后 client/wallet RPC 探活/update CVE 轮询/guest Portal 立即可推进。 |
| **P0** | **`axum` + `tower` + `hyper`** | os-api(api-agent)、os-guest(guest-agent) | HTTP/WS **监听**栈。api-agent Gateway 与 guest Captive Portal 都等它。三者通常成套注册（axum 依赖 hyper/tower）。 |
| **P0** | **`jsonwebtoken`** | os-security(security-agent)、os-guest(guest-agent 经 os-security 消费) | JWT 签发是鉴权链路核心，guest/api 都间接依赖。 |
| **P1** | **`nftnl`** | os-network、os-compute(container)、os-guest | nftables 真实事务执行层（3 处）。 |
| **P1** | **`rtnetlink`** | os-network、os-compute(container) | netlink 接口配置（2 处）。 |
| **P1** | **`argon2`** | os-security、os-services(files) | 密码哈希（2 处）。 |
| **P1** | **`ed25519-dalek`** + **`sha2`** | os-discover、os-update、（os-wallet 经 rust-bitcoin） | 签名验签（beacon 防伪 + 更新包校验，安全关键）。 |
| **P1** | **`tantivy`** | os-services(files)、os-services(media) | 全文搜索（2 处）。 |
| **P2** | 领域专用（单 crate 独占） | openraft(meta)、rusqlite(meta)、virt(vm)、youki+oci-distribution+libcni(container)、rust-bitcoin+alloy+secp256k1(wallet)、dav-server/libunftp/russh(protocol)、mdns-sd/rustls(discover)、cgroups-rs(osd)、toml(i18n)、serde_yaml(cli)、gix+AEAD(devtools)、opentelemetry+prometheus(monitor)、rcgen/instant-acme/boringtun+sha1/hmac(security) | 各 owner agent 按业务优先级单独提 ADR。 |

**Top 5 高频依赖**（按计数降序）：
1. `reqwest` — 6 crate / agent
2. `nftnl` — 3 crate
3. `axum` / `hyper` / `argon2` / `ed25519(-dalek)` / `rtnetlink` / `tantivy` / `jsonwebtoken`（跨 crate 消费） — 各 2 crate

---

## 5. 非 crate 注册的运行时阻塞（仅供集成阶段参考，不计入本清单注册项）

以下为"骨架已可测，真实执行需运行时环境"，**不属于第三方 crate 注册范围**，但需集成阶段准备：

- **root / CAP_SYS_ADMIN / CAP_SYS_TIME**：systemd unit（osd）、cgroup 写入（osd）、NTP（osd）、libvirt（vm）、configfs/targetcli/nvmetcli（storage block）、netlink（network/meta VIP）。
- **沙箱 / 嵌套虚拟化**：ISO 真实 spawn xorriso/mksquashfs（iso）、裸机写盘建池（iso installer）、FFmpeg 转码（media）。
- **CLI 工具链编排**（经 workspace 已有 `tokio::process::Command`，无需新 crate）：Samba/NFS exportfs/ganesha（protocol）、zfs/zpool/cryptsetup（storage）、apt/dpkg（container，可立即推进）、smartctl/sensors（power）。
- **平台运行时**：FCM/APNs 移动推送（mobile）、Windows `net use`/Linux `mount -t davfs`（desktop）、Redfish/OVS offload（network dpu，低优先）。
- **内部 crate 软依赖**（非第三方）：provision 待 network/storage/meta mock；backup 待 storage Replication；im 待 meta MetaStore；meta HaFailover 待 compute VmManager；update 滚动待 meta leader 选举。

---

## 6. 后续动作建议（"批量注册依赖 + ADR"阶段输入）

1. **首批（P0）注册 ADR**：`reqwest`、`axum`+`tower`+`hyper`、`jsonwebtoken`。
   这一批解锁最多 agent（client/wallet/update/guest/api/security），ROI 最高。
2. **第二批（P1）注册 ADR**：`nftnl`、`rtnetlink`、`argon2`、`ed25519-dalek`+`sha2`、`tantivy`。
   覆盖安全链路 + 网络执行层 + 搜索。
3. **第三批（P2）按领域分提**：各 owner agent 就独占依赖单独提 ADR（openraft/virt/youki/rust-bitcoin/dav-server 等），由 ReviewAgent 评估版本与 license（workspace `license = "MIT OR Apache-2.0"`）。
4. **选型待裁决项**（需 OrchestratorAgent 决策）：
   - S3 客户端：`aws-sdk-s3` vs `rusoto` vs 自实现 `reqwest`+sigv4（object-agent）。
   - EVM 库：`alloy` vs `ethers`（wallet-agent）。
   - ACME：`instant-acme` vs 其他（security-agent）。
   - i18n：自实现 vs `toml` vs `rust-i18n`（i18n-agent）。
5. **版本与 license 兼容性**：批量注册时统一在 `[workspace.dependencies]` 固定版本，避免子 crate 各自漂移；所有候选 crate 须 MIT/Apache-2.0 兼容。

---

*本清单由依赖分析子代理于 2026-08-05 汇总，基于 17 份 PROGRESS.md + 全 workspace `grep TODO` 产出。*
