# discover-agent 进度日志

## 当前状态
- 阶段：**mDNS 真实组播 + mTLS 真实双向认证已接通**（批 4，p2/discover-agent 分支）；beacon ed25519 验签（批 3 续）
- 最后更新：2026-08-05

## 已完成
- [x] 节点能力模型 + HA 资格检测纯算法（`capabilities.rs`：NodeCapabilities.minimal/full、qualify_peer、aggregate_qualifications、version_satisfies 含逗号分隔组合约束）
- [x] beacon 防伪签名 challenge/nonce 生成与比对 + **真实 ed25519 验签**（`beacon.rs`）：
  - `BeaconPayload.canonical_bytes`、`is_nonce_fresh`、`is_expired`
  - `generate_challenge_nonce` 改用 `rand_core::OsRng`（CSPRNG，替代时间戳占位）
  - `verify_beacon_signature(..., pubkey: Option<&VerifyingKey>)`：`Some(pk)` 走 `ed25519-dalek` `verify_strict` 真实密码学校验；`None` 回退结构校验
  - 签名侧助手：`generate_keypair`（OsRng）、`sign_beacon`、`pubkey_fingerprint`（SHA-256，与 mTLS 证书指纹算法对齐）
  - 内联 `hex_encode`/`hex_decode`（避免引入未注册的 `hex` crate）
  - 新增 `BeaconVerifyOutcome::BadSignature` 变体
- [x] 联邦决策状态机（`federation_sm.rs`：FederationState/Event/StateMachine + decide_action 决策矩阵；含降级路径：mTLS 失败→单机 / 无 peer→单机 / HA 加入失败→peer 或单机）
- [x] DefaultFederationPolicy（`impls.rs`：FederationPolicy 实现，复用 capabilities 算法 + federation_sm 决策）
- [x] **MdnsDiscovery 真实 mDNS 组播**（`impls.rs`，mdns-sd 0.20 接通）：
  - `start_advertising`：启动 mdns-sd `ServiceDaemon`，把 `PeerNode` 编码为 mDNS TXT 记录（`_os._tcp.local.`），含 beacon 签名 + 公钥 hex，发布到 LAN
  - `discover_peers`：browse `_os._tcp.local.`，在 timeout 内解析 `ServiceResolved` → 解码 TXT 回 `PeerNode` + beacon 校验（预置公钥表 → 真实 ed25519 验签；无则结构校验回退）；与内存 fixture 表合并返回
  - `register_beacon_pubkey(node_id, pubkey)`：预置/凭证公钥注入入口，启用真实验签（生产路径）
  - TXT 编解码（`encode_txt`/`decode_from_txt`）：node_id/endpoints/version/arch/caps(JSON)/bsig/bpub，单值 < 255B
  - 端点解析（`parse_socket_addr_host_port`）支持 IPv4/IPv6/hostname
  - 内存 fixture 路径保留（`inject_peer`/`with_peers`），便于确定性测试（不真组播）
  - 真实组播 loopback 测（`mdns_real_advertise_and_discover_loopback`）：publisher 自身 browse 解析到自身 + TXT 往返字段还原
- [x] **MtlsPeerAuthenticator 真实 mTLS 双向认证**（`mtls.rs`，rustls 0.23 + ring 接通）：
  - `new(cert_chain, key, trusted_roots)`：注入本机证书链 + 私钥 + 受信根证书库
  - `pair(peer_endpoint, token)`：用 rustls `ClientConfig`（mTLS：本机身份 + 对端根证书验证）连接 `peer_endpoint`，`complete_io` 驱动真实 mTLS 握手；成功后取对端证书链，计算首张证书 SHA-256 指纹写入 `PeerSession.mtls_cert_fingerprint`
  - `unpair` / `list_trusted_peers`：内存会话表与受信列表
  - `cert_fingerprint(cert_der)`：证书 SHA-256 指纹（与 beacon 公钥指纹算法一致，便于上层关联校验）
  - 真实 mTLS 握手测（自签证书 fixture + loopback TCP）：成功握手 / 过期凭证拒绝 / 不受信根拒绝 / TCP 拒绝 / unpair / 指纹 SHA-256
- [x] Mock 三件套（`mock.rs`，feature `mock`）：MockDiscovery / MockPeerAuthenticator / MockFederationPolicy，含失败注入与 override（行为不变）
- [x] Cargo.toml：`ed25519-dalek` / `sha2` / `rand_core` / `mdns-sd` / `rustls`(std) 接入（workspace dep）；`rcgen` 作 dev-dep；lib.rs 统一导出（含 SigningKey/VerifyingKey/MtlsPeerAuthenticator/cert_fingerprint re-export）
- [x] FederationAction 派生 PartialEq/Eq（非破坏性，便于状态机/测试断言）

## 依赖接入（ADR-DEPS-001 + ADR-DEPS-002）
- ADR-DEPS-001 续：workspace `rand_core = { version = "0.6", features = ["std"] }`（ed25519 配套）。
- **ADR-DEPS-002 P2 接入**（本批）：
  - crate 级 `os-discover/Cargo.toml`：`mdns-sd.workspace = true` / `rustls = { workspace = true, features = ["std"] }`。
    - 注：workspace 声明 `rustls = { default-features=false, ["ring"] }`；crate 级补启 `"std"` feature（`ClientConnection`/`ServerConnection`/`StreamOwned` 由 std 门控，rustls 默认开 std 但 workspace 禁了默认 features，无新传递依赖）。
  - dev-dep：`rcgen.workspace = true`（仅测试用——构造 mTLS 握手 fixture 自签证书）。
  - **未引入** rustls-pki-types（经 `rustls::pki_types` re-export 访问，不单独依赖）/ hex（未注册，内联实现）。

## 测试与质量门
- `cargo check -p os-discover --features mock` → 0 error
- `cargo test -p os-discover --features mock` → **63 passed**, 0 failed（批 3 的 51 + 本批新增 12：
  - mDNS 真实组播 5：loopback 广播→扫描→TXT 往返、TXT 编码校验、预置公钥真实 ed25519 验签、真实签名 inject 通过、端点解析变体
  - mTLS 真实握手 7：成功握手+指纹、过期凭证拒绝、不受信根拒绝、TCP 拒绝、unpair+list_trusted、证书指纹 SHA-256、端点解析变体
  ）
- `cargo test -p os-discover`（默认无 mock）→ 53 passed
- `cargo clippy -p os-discover --all-targets --features mock -- -D warnings` → 0 warning
- `cargo clippy -p os-discover --all-targets -- -D warnings` → 0 warning
- `cargo doc -p os-discover --features mock --no-deps` → 0 warning
- `cargo check --workspace` → 0 error（mdns-sd/rustls/rcgen 接入未影响其他 crate）

## 阻塞
- 无（mDNS 组播 + mTLS 握手均已真实接通）

## 下一步
1. 持续扫描模式（`on_peer_discovered` 回调的真实 mDNS 事件循环驱动——当前为同步占位，完整事件循环由上层集成实现独立 tokio task 驱动）
2. beacon 公钥指纹与 mTLS 对端证书指纹的关联校验（同一节点两路信任源对齐）——已具备基础（两路均 SHA-256），上层 os-security CertManager 协同落地
3. mdns-sd 服务类型 label 长度敏感（实测环境 label > 16 字符时 browse resolve 异常）——生产用 `_os._tcp.local.`（短 label）规避，未来若需唯一类型注意 label 长度
