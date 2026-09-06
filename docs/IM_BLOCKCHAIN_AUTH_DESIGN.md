# IM 区块链认证设计（身份 = 公钥）

> 决策（用户 2026-08-17）：IM 是重要且认证复杂的模块——**除去其他认证方式，
> 只留区块链认证；用户名只能是公钥**。

## 1. 现状问题

- WS 握手 `ws://host:8080/ws?user=<自报字符串>`：**无任何认证**，任何人可冒充任何用户名
- REST 心跳/消息的 sender 全部自报，无法归因
- 与系统级 admin token（管理用）纠缠不清

## 2. 目标模型

```
身份层（IM 用户）                    信任根
─────────────────                   ─────
用户名 = secp256k1 公钥              密码学（k256，与 os-wallet 同源）
  · 压缩格式 0x + 66 hex             私钥永不出客户端
  · 展示名 = 派生 EVM 地址（后 8 位）  
认证 = 挑战-签名：
  1) POST /api/v1/im/auth/challenge {pubkey}  → {nonce}(60s 单次有效)
  2) 客户端用私钥对 nonce 明文签名（secp256k1, recoverable 或固定 r|s）
  3) POST /api/v1/im/auth/verify {pubkey, nonce, signature} → {token}(24h)
  4) REST: Authorization: Bearer <token>
     WS:   ws://…/ws?user=<pubkey>&token=<token>（握手即验，失败 4401 关闭）
```

- **签名**：对 nonce 十六进制字符串字节做 SHA-256 再 ECDSA 签名？——**简化决策：直接对
  nonce 的 UTF-8 字节签名**（nonce 本身是 256-bit 随机 hex，已不可预测），签名格式
  65 字节 r||s||v（k256 recoverable），服务端用公钥 verify
- **token**：随机 256-bit hex，内存 HashMap<pubkey→(token, 过期)> + 单点登录（新
  verify 顶掉旧 token）
- **去自报化**：所有 sender 字段服务端从 token 反查 pubkey 填充，客户端传的 sender 一律忽略
- **兼容闸门**：无 token 的 `?user=` 裸访问 → 4401/401（一次性破坏性变更，前端与
  Windows 客户端同步升级）

## 3. 边界

- 系统级 NEXOS_ADMIN_TOKEN 保留，仅用于管理端点（与 IM 用户身份正交）
- 私钥管理：前端 localStorage（Web 端）/ Windows 端自有 keystore；服务器不存任何私钥
- 首次使用：客户端本地生成密钥对（或导入）， pubkey 即身份，无需注册——天然防抢注
  （想冒充别人？先拿出别人私钥）

## 4. 分批

| 批次 | 内容 | 验收 |
|---|---|---|
| 1 后端 | im.rs：2 条 auth 端点 + nonce 桶 + k256 验签 + token 桶 + WS 握手强制 + 全端点 sender 反查 + 删自报路径 | 单测≥12（含真密钥对全流程）；workspace 全绿 |
| 2 前端 | Chat.vue 身份卡：生成/导入密钥（@noble/secp256k1）、challenge→sign→token 自动流、展示名 EVM 后缀 | npm build + GUI 验证 |
| 3 协作 | SERVER_NOTES §9 协议变更通知 Windows agent + E2E | 双端实测 |

## 5. 二期（不做）

- 联邦场景下的跨节点身份漫游（pubkey 天然可漫游，换节点重走挑战即可）
- 消息端到端加密（先有身份，后有 E2E——本次铺的是地基）

## 6. 平台身份通用性（chain_auth 抽取后的现状，2026-08-20 核对）

本设计的认证内核已从 `handlers/im.rs` 泛化抽取为共享 crate：
`os-common/src/chain_auth.rs`（`ChainAuth` = nonce 桶 + token 桶 + k256 验签 +
EVM 展示名派生；常量 `NONCE_TTL_SECS=60` / `TOKEN_TTL_SECS=86_400`）。

- **IM 契约零变化**：`im.rs` 里 `ImAuth` 现为 `os_common::chain_auth::ChainAuth`
  的类型别名（`pub type ImAuth = ChainAuth`），§2 的端点、签名格式、token 语义、
  WS 握手校验全部不变——本文档仍是 IM 侧的准确契约。
- **NexHub 复用同密钥对**：os-nexhub 挂**独立** `ChainAuth` 实例（nonce/token 桶
  互不相通，IM 的 token 在 NexHub 不可用，反之亦然），但客户端可用**同一对
  secp256k1 密钥**分别在两侧完成挑战-签名（前端 `useImIdentity` 泛化为
  `useChainIdentity` 的服务端前提）。NexHub 侧端点契约见
  `docs/NEXHUB_LOBBY_DESIGN.md` §12。
- **后续平台能力接入方式**：任何新 handler 需要链上身份时，挂独立
  `ChainAuth` 实例 + 复用 `parse_pubkey` / `verify_nonce_signature` /
  `derive_display_name` / `bearer_token` 四个纯函数即可，不必再复制 IM 的实现；
  身份归因规则保持"服务端从 token 反查 pubkey，body 自报身份一律忽略"。
