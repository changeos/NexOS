# NexOS dApp（去中心化应用）集成方向调研

> 调研日期：2026-08-31 · 方法：只读代码扫描（全部结论附文件路径）+ 外部公开资料检索（附来源链接）
> 目的：在 NexOS 现有区块链底座上确定 dApp 的**方向、技术选型、路线图**——本文是设计调研，零代码改动
> 文档铁律遵守：面向零上下文读者、结构化小节、量化数字、ASCII 拓扑，可直接提取为 PPT 素材

---

## 0. 一页结论（TL;DR）

- **NexOS 已经是一个"差一步上链"的 dApp 平台**：链上身份（secp256k1 挑战-签名）、多链钱包契约
  （alloy/bitcoin 双栈）、链节点编排（7 条链预设）、两条变现线（网关充值 + NexHub 悬赏/购买）、
  P2P 加密 overlay 全部就位，唯一缺的是**把"自证支付"变成"链上验真"**这一根线。
- **推荐一期（小而真）**：`verify_payment` 接真实 EVM RPC——用一条 JSON-RPC 调用
  （`eth_getTransactionByHash`）把 NexHub 购买/悬赏验收从"任意字符串收据即可白嫖"
  （安全隐患 S1）升级为真实链上核验，在 dev 链（anvil）+ Sepolia 测试网实测。
  工作量约 2-3 天，直接封掉高危隐患，且不引入任何新身份体系/新链依赖。
- **链选型**：EVM 兼容链单一栈（一期只锁测试网），二期扩展 Base（L2，单笔约 $0.002-0.05）。
  不做多链、不上账户抽象（EIP-7702/4337）、不承诺 BTC 链上核验——理由见 §5。
- **红线继承**：私钥明文（服务端 wallets.json + 浏览器 localStorage）与法律敏感性
  （中国大陆对虚拟货币业务的监管）决定**默认测试网/私有链，主网开关显式配置**。

---

## 1. 定位：dApp 对 NexOS 意味着什么

### 1.1 从"打破五类孤岛"到"信任孤岛"

PHILOSOPHY.md（`/home/oem/NexOS/PHILOSOPHY.md`）定义 NexOS 为"连接 OS"——AI 时代每个
超级个体的操作系统，要打破五类孤岛：

| 孤岛 | 现有解法 | dApp 补上什么 |
|------|----------|---------------|
| 设备孤岛 | 蓝牙 mesh 中继 | —（与本调研无关） |
| 数据孤岛 | ZFS 统一存储 + 文件共享 | **内容指纹上链确权**（谁在何时拥有什么数据） |
| AI 孤岛 | 模型管理 + API 网关 | **算力产能定价与链上结算**（api_market 已挂牌未闭环） |
| 身份孤岛 | 用户管理 + IM Federation | **跨节点可验证身份**（chain_auth 已是准 dApp 身份） |
| 代码孤岛 | NexHub 代码枢纽 | **成果确权/交易/悬赏**（已实现自证版，缺验真） |

前四类孤岛的解法都停在"本机/本集群可信"——**信任本身是第六座孤岛**：节点 A 凭什么信
节点 B 的大厅条目、悬赏验收、API 计费账单？当前的答案是"自证"（收据任意字符串、admin
手动确认）。dApp 的意义 = **把信任外包给一条中立的链**：身份由公钥证明、支付由交易哈希
证明、账单由链上锚定证明。这正好是 GATEWAY_MONETIZATION.md 已写好的 Phase 2/3 方向。

### 1.2 超级个体叙事下的 dApp 角色

AI 时代个体产出（代码、模型推理、生成图、数据集）天然是**数字商品**，需要三类 Web3 原语：

1. **身份**（谁做的）——secp256k1 公钥即用户名，已有；
2. **交易**（卖给了谁、多少钱）——NexHub 购买/悬赏 + 网关充值订单的骨架已有，缺链上结算；
3. **凭证**（做过什么、信誉如何）——os-wallet 的 `CredentialSpec`（Ordinal/ERC-721/ERC-1155
   持有性查询，`crates/os-wallet/src/chain.rs:32-60`）已定义契约，零消费方。

> **PPT 表述**：NexOS 的 dApp 不是"做个 DeFi 前端"，而是把超级个体的**产能 → 交易 → 信誉**
> 三本账从"自证"升级为"链证"。现有 21 万行代码里 80% 的零件已经造好，缺的是最后一根线：
> 让服务端真的去问一条链。

---

## 2. 现有底座盘点（全部经源码核实）

### 2.1 底座能力清单总表

| # | 模块 | 位置（绝对路径） | 规模 | 核心能力 | dApp 复用方式 |
|---|------|------------------|------|----------|---------------|
| 1 | **链上身份内核 chain_auth** | `crates/os-common/src/chain_auth.rs` | 407 行 / 7 单测 | secp256k1 挑战-签名-token 三步认证；`parse_pubkey`（压缩公钥 0x+66hex）；`derive_display_name`（keccak256 → EVM 地址）；`verify_nonce_signature`（65B r\|\|s\|\|v） | **直接就是 dApp 账户体系**——任何链上操作的身份归因 |
| 2 | **多链钱包契约 os-wallet** | `crates/os-wallet/src/`（chain/connector/registry/signing/model/mock/error） | 6,098 行 | ① `ChainAdapter` trait（验签/余额/凭证）+ Bitcoin/EVM 双真实实现（rust-bitcoin 0.32 + alloy 2，workspace `Cargo.toml:196-202`）② `WalletConnector`（WalletConnect v2/注入式/二维码三连接器）③ `RpcRegistry` 条件激活（RPC 探活：BTC `getblockchaininfo` / EVM `eth_blockNumber`，reqwest 真实 HTTP）④ `signing.rs`（1,741 行）EIP-191/EIP-712/BIP-340 Schnorr/ECDSA **真实验签** | **钱包连接与链上查询的现成引擎**——当前消费方仅 os-guest（访客三因子）与测试，os-api 尚未接线 |
| 3 | **区块链应用（节点+浏览器+钱包）** | `crates/os-api/src/handlers/blockchain.rs` | ≈2,350 行 / 20 路由 | docker compose 真实启停 RPC 节点（geth/reth/erigon/ganache/anvil/op-geth/arbitrum-node，RPC 8545/WS 8546）+ Blockscout 浏览器；k256+tiny-keccak 纯 Rust 生成/导入钱包并派生 EVM 地址；`eth_getBalance` 余额查询 | **自建节点/私有链的编排面**——dev 链（anvil，chain_id 1337）一键拉起，即 dApp 沙箱 |
| 4 | **链预设** | 同上 `chain_presets()`（blockchain.rs:663-721） | 7 条 | ethereum mainnet(1)/sepolia(11155111)、dev(1337)、l2：optimism(10)/arbitrum(42161)/**base(8453)**、custom(0) | **链选型的现成菜单**——Base 已在预设，一期测试网零新增 |
| 5 | **API 网关变现** | `crates/os-api/src/handlers/api_gateway.rs` | ≈5,000 行 | billing_mode 四模式（free/per_token/per_image/credits）；价目常量 `PRICE_USDT_PER_CREDIT=0.01` / `PRICE_SATS_PER_CREDIT=1500` / `PRICE_WEI_PER_CREDIT=20e15`；PaymentOrder 三币种（usdt/btc/evm）4 条 payments 端点（`POST/GET /api/v1/gateway/payments`、`/:id/confirm`、`/:id/reject`），admin 手动确认 | **"钱换积分"的入金通道**——dApp 只需替换人工确认为链上核验（GATEWAY_MONETIZATION Phase 2 原文设计） |
| 6 | **NexHub 货币化+悬赏** | `crates/os-nexhub/src/nexhub_lobby.rs`（crate 共 14,103 行） | 9,293 行 / 18 路由 | 大厅条目 `price_sats+currency`（free/btc/nex/usdc/eth）→ `POST /:name/purchase` 授权（`hub_entitlement` 落库，未购 402）；悬赏完整生命周期 `open→claimed→submitted→paid/rejected`（`hub_bounty` 表，原子认领 409）；发布者/悬赏人/买家身份全部 token 反查（`publisher=pubkey`，body 自报被忽略） | **去中心化市场的主业务层**——dApp 要做的只是把 `verify_payment`（nexhub_lobby.rs:3773）从"自证"换成"链证" |
| 7 | **推理服务市场 api_market** | `crates/os-api/src/handlers/api_market.rs` + `docs/API_MARKET.md` | 独立 handler | 推理端点挂牌：发布者身份=区块链公钥（**唯一通道，无 admin 回落**）、硬件探测（nvidia-smi//proc/cpuinfo//proc/meminfo）、心跳负载、价格排序；与 nexhub-lobby **共享同一 ChainAuth 实例**（token 互通） | **"算力即商品"的货架已摆好**——消费计费/结算闭环是天然 dApp 场景 |
| 8 | **P2P 组网 os-p2p** | `crates/os-p2p/src/`（api/kad/transport/crypto/punch/relay/transfer/meta/bootstrap…） | 14,116 行 | 全分布式 Kademlia（k=16，160 个 proximity bin）；**OverlayAddr = keccak256(公钥)[12..]——与 EVM 地址同源**；ECDH+AES-256-GCM 全帧加密+nonce 挑战签名握手；连接阶梯（LAN mDNS→公网直连→TCP 打洞→中继兜底+离线信箱）；`Handle::send/on_msg` 跨节点消息通道（api.rs:355/364）；transfer 组件分块传输带 sha256 清单 | **跨节点 dApp 的传输层**——链下消息/文件/清单哈希全部可走 overlay，不依赖公网 IP |
| 9 | **指纹账本 os-identity** | `crates/os-identity/src/ledger.rs` | 1,036 行 | `IdentityLedger`：NodeID↔地址证据登记（Handshake/ProbeVerified/ProbeMismatch/Gossip 四类）、冲突/失配观测、`owns_addr` 判定、JSON 原子落盘 | **P2P 侧的信誉原始数据**——上链锚定（Merkle root）即可变成"节点信誉凭证" |
| 10 | **访客三因子 os-guest** | `crates/os-guest/src/`（chain.rs 编排 os-wallet） | 契约 crate | GuestIdentity 四类（RandomId/ExtendedId/PublicKey/**ChainCredential**）；chain-orchestrator 委派 os-wallet 完成"持有 Ordinal/NFT 即可入网"的验证编排 | **链上凭证的消费场景样板**——持有 NexOS NFT = 访客高级权限 |

### 2.2 三条"价值闭环"现状（钱在哪里转）

```
闭环 A：NexHub 悬赏/购买（docs/GATEWAY_MONETIZATION.md + docs/NEXHUB_ONBOARDING.md）
  出资人 --(reward_sats)--> 悬赏 --(认领/交付)--> hunter --(验收)--> poster 自证支付
  ⚠️ 断点：verify_payment 只查"货币一致+金额足额+txid 非空"（nexhub_lobby.rs:3773-3793）
          ——任意非空字符串即通过（S1 高危）

闭环 B：API 网关充值（docs/GATEWAY_MONETIZATION.md Phase 1 已实现）
  用户 --(USDT/BTC/EVM 链上转账)--> 占位收款地址（env 注入）--> admin 手动 confirm --> 积分入账
  ⚠️ 断点：无自动核验（Phase 2 设计已写：一单一址派生 + RPC 轮询，未实现）
           当前实机三个收款地址均为占位符（TPLACEHOLDER9…/bc1qplaceholder…/0x…dEaD）

闭环 C：api_market 算力市场（docs/API_MARKET.md）
  发布者（链上身份挂牌）--> 消费者按价格排序 --> 拿 endpoint_url 直连
  ⚠️ 断点："消费计费走 api_gateway 的 sk-os- 令牌，本批不做调用闭环"（API_MARKET.md 原文）
```

**结论：三条闭环的断点全部是同一件事——没有服务端主动去链上核对。** 这就是 dApp 一期的靶心。

### 2.3 集群与部署现状（dApp 的运行环境）

- 4 节点集群（MEMORY.md §七）：106 主开发机（debug，os-api :8558）、113（release）、
  aliyun 公网节点（203.0.113.2，NAT 后 advertise :7070）、云锚点（198.51.100.114 独立
  `p2p-node` bin）——**aliyun 可作为公共 RPC 不可达时的自建节点宿主**。
- 规模数字（PHILOSOPHY.md）：28 桌面应用、470+ API 路由、3,900+ 单元测试、21 万行代码
  （实测 `crates/` 下 .rs 共 264,142 行，含测试）；workspace 26 个 crate（`Cargo.toml` members）。

---

## 3. 候选 dApp 方向（5 个，按推荐度排序）

### 方向 1：支付验真最小闭环（把两条变现线的"自证"换成"链证"）★ 一期推荐

- **场景**：NexHub 购买付费条目 / 悬赏验收 / 网关充值订单时，服务端拿用户提交的 txid
  真实调用 EVM RPC：`eth_getTransactionByHash` 校验 `to == 收款地址 && value >= 应付额`
  （native 币），ERC-20 再补 `eth_getTransactionReceipt` 解 Transfer 日志。
- **依托底座**：`verify_payment`（nexhub_lobby.rs:3773，现成的校验骨架，只差最后一段）；
  blockchain.rs 已有 reqwest JSON-RPC 调用范式（eth_getBalance，30s 超时降级）；
  7 条链预设；价目常量已含 wei 单位（`PRICE_WEI_PER_CREDIT`）。
- **新增工作量**：**小（约 2-3 天）**——一个 `chain_verify.rs` 小模块（reqwest POST 两个
  RPC method）+ `verify_payment` 换调用 + env 表（`NEXOS_EVM_RPC_URL` 等）+ 测试网实测。
- **价值**：★★★★★ 直接封掉台账 S1 高危隐患（付费门禁白嫖）；GATEWAY_MONETIZATION
  Phase 2 的第一根线；是后续一切结算类 dApp 的地基。**小而真：不虚构链上交互——
  在 anvil dev 链真实造一笔交易验证，在 Sepolia 用水龙头币验证。**

### 方向 2：链上身份门户（第三凭证 + 身份面板）

- **场景**：一个"身份中心"桌面应用/页面：显示公钥、EVM 展示名（keccak 派生）、NodeID、
  积分余额、NexHub 授权记录（`GET /api/v1/nexhub/lobby/entitlements`）、链上凭证
  （NFT/Ordinal 持有）；并推动网关 dispatch 把 `Authorization: Bearer <chain token>`
  识别为与 admin token/JWT 并列的第三种 Principal。
- **依托底座**：chain_auth（407 行内核 + IM/NexHub/api_market 三处挂载）；前端
  `useChainIdentity.ts`/`useImIdentity.ts`（同一 localStorage 密钥，@noble/secp256k1）；
  FEATURE_SURVEY_2026-08-20.md §1.1② 已把"ChainAuth 升级为系统第三凭证"列为三大
  最值得投入方向之一（当前覆盖率仅 2-3/30 应用，media_gen 生图仍要求 admin token）。
- **新增工作量**：**中（约 1-2 周）**——网关 dispatch 改造（契约 `ApiRequest` 无 auth
  字段，handler 自读 Bearer 的现状需收口）+ 一个聚合只读端点 + 前端面板。
- **价值**：★★★★☆ 身份是所有 dApp 的入口；一旦成为系统级凭证，媒体生成/区块链写操作
  获得真实调用方归因，"大厅身份用户用不了 AI 产能"的矛盾（FEATURE_SURVEY 原文）解除。

### 方向 3：去中心化悬赏市场深化（NexHub bounty × api_market 交叉）

- **场景**：悬赏不只是"人做任务"——把 api_market 的算力挂牌引入悬赏交付（AI agent
  产能接单），验收后链上放款；悬赏/挂牌/信誉全部公开可审计。
- **依托底座**：bounty 8 路由完整生命周期 + 409 原子认领；api_market 挂牌+心跳+硬件画像；
  两者已共享同一 ChainAuth 实例（token 互通）。
- **新增工作量**：**中大（2-4 周）**——依赖方向 1 的验真先行；agent 自动认领协议
  （P2P overlay 通知 or 大厅轮询）；escrow（链上托管合约 or 服务端积分托管，后者无需合约）。
- **价值**：★★★★☆ 这是 NexOS 最有叙事差异化的场景（"AI 时代的 bounties + 算力市场"），
  但**必须建立在验真之上，否则是空中楼阁**。

### 方向 4：NexHub 条目 NFT 化 / 数据确权锚定

- **场景**：发布者把仓库快照（commit 哈希/README/文件清单 sha256——os-p2p transfer
  组件已产出分块 sha256 清单）的 Merkle 根锚定到链（一笔 OP-Stack 存储交易），发布即确权；
  可选 ERC-721 铸造"作品凭证"（os-guest 的 ChainCredential 恰好消费 ERC-721 持有性）。
- **依托底座**：os-wallet `CredentialSpec::Erc721` 契约 + EvmAdapter 真实 `eth_call
  balanceOf/ownerOf` 能力（chain.rs 头注释）；blockchain.rs 节点编排（可自建链降低成本）；
  os-p2p transfer sha256 清单。
- **新增工作量**：**中（2-3 周）**——需要一个简单合约（铸造/锚定）+ 部署 + os-wallet
  签名路径接线（注意 blockchain.rs 现签名 spawn python3 eth-account，缺库降级——
  应改走 alloy 原生签名，signing.rs 已有验签侧）。
- **价值**：★★★☆☆ 叙事好看（确权），但**对现有用户的即时价值低于验真**；且涉及
  合约部署与审计成本。放二期末/三期。

### 方向 5：跨节点服务支付结算（P2P 微支付账本）

- **场景**：节点 A 消费节点 B 的推理/存储/转发服务，按时计量形成账单，定期链上批量
  结算（或积分互认 + 日账单哈希上链锚定防篡改）——GATEWAY_MONETIZATION Phase 3 原文
  （"账单上链锚定 + sats 统一价值单位 + 钱包即身份"）。
- **依托底座**：网关 CallLog 计量 + sk-os- 令牌；P2P overlay 加密消息通道（`Handle::send`）；
  os-identity 账本（跨节点信誉原始数据）。
- **新增工作量**：**大（4 周+）**——跨节点账本对账协议、争议处理、汇率/清结算规则。
- **价值**：★★★☆☆ 长期价值高（这是"连接 OS"的终局商业模式），但一期做是过度设计。

---

## 4. 技术选型

### 4.1 链选择

| 选项 | 说明 | 判定 |
|------|------|------|
| **EVM 单栈（推荐）** | 身份（chain_auth 公钥→EVM 地址同源）、钱包（os-wallet EvmAdapter/alloy）、节点预设（7 条里 6 条 EVM 系）全部 EVM 对齐；只维护一套 ABI/RPC 语义 | ✅ 一期锁 **dev 链(1337, anvil) + Sepolia(11155111)**，二期加 **Base(8453)** |
| EVM L2（Base） | 单笔 $0.002-0.05（[Base 官方文档](https://docs.base.org/base-chain/network-information/network-fees)：0.005 gwei 底价，200k gas ≈ $0.002；[Spark 链费对比](https://www.spark.money/tools/chain-fee-comparison)：$0.01-0.05），且已在 `chain_presets()` 预设中（blockchain.rs:663-721） | ✅ 二期生产链首选；L2 中位费 2026 年较早期下降 95%+（[arXiv 研究](https://arxiv.org/html/2606.22206v1)） |
| 多链（BTC+EVM 并行） | os-wallet 有 BitcoinAdapter，但 BTC 链上核验依赖 UTXO 扫描（`scantxoutset`），GATEWAY_MONETIZATION.md §4 明确"BTC 上链核验靠后、跨链桥不承诺" | ❌ 一期不做；BTC 只保留 sats 计价单位语义 |
| 自建私有链 | blockchain.rs docker compose 已能拉 geth/anvil（真实 spawn `docker compose up -d`），aliyun 公网节点可承载 | ✅ 作为 dev/演示环境与"大陆公共 RPC 不可达"的兜底 |
| 非 EVM 公链（Solana 等） | 身份/钱包/预设三线全部重写 | ❌ 否决 |

**中国网络环境考量**：公共 RPC（PublicNode、Alchemy、QuickNode 等免费层，
[Chainlist](https://chainlist.org) 聚合，Base Sepolia 见 chain 84532 页）多为海外托管，
大陆直连延迟高或不稳定，且无官方直连清单——**对策：`RpcRegistry` 的探活/降级机制
（crates/os-wallet/src/registry.rs：EVM `eth_blockNumber` 探测 + `RpcSource::{Local,
Remote}` 双源）正是为此设计，一期给验真模块配"RPC URL 列表 + 探活回退 + 本地 anvil
兜底"三档即可，零新增基建。**

### 4.2 签名方案

| 阶段 | 方案 | 依据 |
|------|------|------|
| 一期 | **本地 secp256k1 直签（现状复用）**：前端 @noble/secp256k1（localStorage `os-im-privkey`），后端 k256 验签 | chain_auth 已全线在用（IM/NexHub/api_market），零新增；签名 nonce 的方式与链上交易签名是**同一把私钥**——账户即身份 |
| 二期 | **钱包连接**：os-wallet `WalletConnector` 三连接器（WalletConnect v2 / 注入式 / 二维码，connector.rs）接线 os-api + Web；外部钱包（MetaMask 等）用 **EIP-712 结构化签名**（signing.rs 1,741 行已实现 EIP-191/EIP-712 真实验签与类型哈希构造） | 让用户不必把私钥交给 NexOS 前端——直接缓解台账 S3（localStorage 明文私钥）；EIP-712 让"购买授权"变成钱包里可读的确认弹窗 |
| 不采用 | 账户抽象（EIP-4337 bundler/paymaster、EIP-7702 智能 EOA）——2026 年已是主流（[EIP-7702 解读](https://www.openfort.io/blog/eip-7702)、[Turnkey: 4337→7702](https://www.turnkey.com/blog/account-abstraction-erc-4337-eip-7702)） | 引入 bundler/paymaster 基建与第三方依赖，超出 NexOS 自持节点哲学；EOA 直签足够支撑结算类场景。三期可重估（社交恢复值得要） |

### 4.3 RPC 接入

分层配置（全部 env，符合功能文档铁律）：

| 档 | 来源 | 用法 |
|----|------|------|
| 默认 | 本地 dev 链 anvil（blockchain.rs 节点编排拉起，`http://127.0.0.1:8545`） | 开发/演示/CI，零外网依赖 |
| 测试网 | 公共 RPC（如 `https://ethereum-sepolia-rpc.publicnode.com`、`https://base-sepolia-rpc.publicnode.com`，[Chainlist](https://chainlist.org/chain/84532) 可换）——env 可配列表，逐个探活回退 | 一期验真实测 |
| 生产（可选） | 私有服务商免费层（Alchemy ~10 万请求/天、QuickNode ~10M credits/月）或 aliyun 节点自建 op-geth | 二期起，主网开关显式配置 |

### 4.4 Gas 与成本（2026-08 行情）

| 链 | 单笔成本 | 来源 |
|----|----------|------|
| Base（L2） | 底价 0.005 gwei，200k gas ≈ **$0.002**；常见区间 **$0.01-0.05** | [docs.base.org](https://docs.base.org/base-chain/network-information/network-fees)、[Spark](https://www.spark.money/tools/chain-fee-comparison) |
| Ethereum 主网 | 低 gas 期 ~0.326 gwei，简单转账 ≈ **$0.01** | [L2Beat 成本追踪](https://l2beat.com/layer2s/costs) |
| Sepolia / dev 链 | **$0**（水龙头币 / anvil 无价值币） | — |
| NexOS 语义 | 1 积分 = 0.01 USDT = 1500 sats = 0.02 ETH（`api_gateway.rs` 价目常量，代码内 pub const 可调） | 源码已核实 |

结论：**验真类（只读 RPC）零 gas；锚定类（写 32 字节）在 Base 上一笔约 $0.002-0.05，
月锚定 30 笔 < $2**——成本不构成障碍，合规才是（见 §7）。

---

## 5. 推荐路线图

### 一期（1-2 周）：链上验真最小闭环 ★小而真

```
范围：方向 1 全量 + 方向 2 的只读聚合端点（不碰网关 dispatch）
1. crates/os-nexhub 新增 chain_verify 模块：
   - evm_verify_tx(txid, expect_to, min_value, rpc_urls) —— eth_getTransactionByHash
     （native 币）+ eth_getTransactionReceipt（ERC-20 Transfer 日志）
   - RPC 列表 env：NEXOS_EVM_RPC_URL（分号分隔多地址，逐个探活回退）
   - RPC 不可达 → 拒绝确认并提示（不静默放行，也不假装成功——真实数据铁律）
2. verify_payment（nexhub_lobby.rs:3773）接线：测试链(chain_id 白名单 env)的 txid
   必须过链上核验；白名单外的币种维持现状并在响应中标注 unverified:true
3. 网关 PaymentOrder confirm（api_gateway.rs）：admin confirm 时同一函数复验
4. 钱包文件安全前置（台账"旧·高"项止血）：wallets.json 权限 0600 + 前端警示
   （ApiGateway.vue 的 PLACEHOLDER_PAY_ADDRESSES 红色警示范式现成可抄）
5. 实测：anvil dev 链真实造一笔交易走通全链路；Sepolia 水龙头币复测
验收标准：伪造 txid 无法购买付费条目（400）；真实测试网交易 3s 内核验通过
```

### 二期（3-6 周）：身份第三凭证 + 钱包连接

```
1. 方向 2：网关 dispatch 识别 chain token 为第三 Principal；media_gen/blockchain
   写操作获得 pubkey 归因；"身份中心"页面（公钥/EVM 名/积分/授权/凭证）
2. os-wallet WalletConnector 接线 os-api + Web（注入式优先，WalletConnect 次之）；
   EIP-712 签名替换"前端自填收据"的购买流
3. 网关充值一单一址：EVM 订单从 os-wallet 派生唯一收款地址（GATEWAY_MONETIZATION
   Phase 2 原设计：HD 派生/nonce 地址）+ RPC 轮询自动 confirm
4. Base(8453) 接入（预设已有）：验真合约地址/chain_id 配置化
```

#### 二期支付验真增强（2026-09-02 完成 ✅，本调研之外立项的增量批次）

> 在一期「支付验真最小闭环」之上补两项，均已完成（代码
> `crates/os-nexhub/src/chain_verify.rs` + `nexhub_lobby.rs`「链上支付验真」段 +
> `crates/os-api/src/handlers/api_gateway.rs` confirm 接线）：

- [x] **任务 1：ERC-20（USDT）Transfer 事件核验**——`TxProof` 增可选
  `erc20: Option<Erc20Spec{contract, decimals}>`；有 erc20 时核验路径改为
  `eth_getTransactionReceipt` 的 `logs[]`：匹配 `address==contract &&
  topics[0]==Transfer 签名哈希 && topics[2]==收款地址` 的日志，`data`（uint256）
  按**最小单位**对账（口径与 expected_value 一致并文档写死）；无匹配日志 →
  `Mismatch("erc20_log")`。契约兼容：`erc20: None` 时 native 路径零变化；
  `Verified` 增 `token: Option<String>`（ERC-20 时=合约地址）。接线：网关
  confirm 的 usdt 订单定位到 EVM 链时构造 erc20 凭证（合约来源 body
  `erc20_contract`/`erc20_decimals` → env `NEXOS_USDT_EVM_CONTRACT`/
  `NEXOS_USDT_EVM_DECIMALS`（默认 6）→ 都无则 Unverified 放行，**不猜合约地址**）；
  TRON 形态（定位不到 EVM 链 ID）与 BTC 仍人工。NexHub 侧 currency 域增 `usdt`
  （购买/悬赏 body 同样支持 erc20 字段）。
- [x] **任务 2：「≥应付额」增强**——`TxProof` 增 `amount_rule: AmountRule`
  （`Exact`（默认，向后兼容）| `AtLeast`）；AtLeast 下链上金额 ≥ 应付即过
  （Verified 携带链上实付 `value_wei`），不足 → `Mismatch("value")`。接线定稿：
  **网关 confirm = AtLeast**（充值多打不亏待用户）、**NexHub purchase = Exact**
  （商品定价等值）、**bounty approve = AtLeast**（与自证面「足额」对齐）——
  理由矩阵见 docs/GATEWAY_MONETIZATION.md「支付验真」节 / NEXHUB_LOBBY_DESIGN.md §10.6。
- 测试：`chain_verify.rs` mock RPC 12 个新用例（ERC-20 正确/金额不符/收款方不符/
  无日志/topic0 不匹配/假合约/多日志扫描/data 畸形 fail-closed/AtLeast 三分支×
  ERC-20 与 native/工具直测）+ 接线层 CV17-CV22（to_min_unit_str 换算、usdt 三形态
  分流、purchase Exact/approve AtLeast 凭证捕获）+ 网关 GW-CV11-14（usdt@EVM 核验
  body/env 合约、TRON 人工、缺合约不猜、AtLeast 规则与实付落库）。
- 未尽（后续批次）：USDT-TRON 核验、BTC UTXO 扫描、usdc 等 ERC-20 的 env 泛化、
  一单一址派生 + RPC 轮询自动 confirm（GATEWAY_MONETIZATION Phase 2 主体）、
  ERC-20 真实链 live 验收（`chain_verify_live.rs` 现仅 native 用例）。

### 三期（季度级）：结算、确权、信誉

```
1. 方向 3：悬赏 × api_market 交叉（AI 产能接单 + escrow 放款）
2. 方向 4：发布快照 Merkle 根锚定 + 可选 ERC-721 作品凭证（os-guest ChainCredential 消费）
3. 方向 5：跨节点账单哈希上链锚定 + sats 统一价值单位（GATEWAY_MONETIZATION Phase 3）
4. 私钥加密存储（passphrase/age）——Phase 3 前必须完成（GATEWAY_MONETIZATION §2 红线原文）
5. 重估账户抽象（社交恢复/gas 代付）
```

**排序理由**：验真（一期）是唯一"不做则其余全部是空中楼阁"的项——付费门禁、悬赏放款、
充值核验三条业务线共用它；身份凭证（二期）解锁 30 个应用的身份覆盖缺口；结算/确权（三期）
在信任地基稳固后才有意义。

---

## 6. 风险与合规

### 6.1 私钥与安全现状（引用台账 docs/FEATURE_SURVEY_2026-08-20.md §5.3）

| # | 隐患 | 位置 | 等级 | 对 dApp 的影响 |
|---|------|------|------|----------------|
| S1 | NexHub 付费门禁可白嫖：自证收据 txid 任意非空字符串即发授权 | `crates/os-nexhub/src/nexhub_lobby.rs:3773-3793`（verify_payment） | 高 | **一期直接修复** |
| 旧 | 钱包私钥明文 `/tank/os-data/wallets.json`（blockchain.rs:514 `WALLETS_FILE`） | `crates/os-api/src/handlers/blockchain.rs:514` | 高 | 变现红线自认（GATEWAY_MONETIZATION §2：Phase 3 前必须加密）；一期做 0600+警示止血，二期钱包连接后引导用户改用外部钱包 |
| S3 | 链上身份私钥明文存浏览器 localStorage（XSS 即失窃） | `useImIdentity.ts:40`（`os-im-privkey`） | 中 | 二期钱包连接（EIP-712）可让私钥不落地 |
| S4 | ChainAuth nonce 桶无过期清扫无限速（公网狂刷 challenge 内存只增不减） | `crates/os-common/src/chain_auth.rs:144-172` | 低-中 | dApp 若公网暴露需先修（清扫 + 限速） |
| S5 | GET /gateway/payments 公开暴露订单（收款地址+金额+txid） | `api_gateway.rs:1256` | 低 | 一期顺手收紧 |
| S6 | media_gen recent 公开读生成记录 | media_gen.rs 路由表 | 低 | 与身份凭证（二期）一并归因 |

另：blockchain.rs 交易签名依赖宿主机 `python3 -c eth-account`，缺库时静默降级返回
未签名交易（`signed:false`，blockchain.rs:569-591）——**dApp 任何真实转账必须改走
alloy 原生签名**（os-wallet signing.rs 已有完整 Rust 实现），不得走 python 降级路径。

### 6.2 法律敏感点（中国大陆环境）

- 2021 年"9·24 通知"（人民银行等十部门）：虚拟货币相关业务活动属**非法金融活动**；
  境外交易所向境内居民提供服务同属非法（[官方原文](http://m.safe.gov.cn/safe/2021/0924/19911.html)）。
- 个人持有/交易：无法律明文规定为犯罪，但相关民事行为可能被认定无效、损失自担
  （[锦天城律所解读](https://www.allbrightlaw.com/SH/CN/10475/1fea7d58626fdc70.aspx)）。
- 2025-2026 监管升级：八部门新通知——境内主体及其控制的境外主体未经同意不得在境外
  发行虚拟货币；禁止企业/个体工商户在名称与经营范围使用"虚拟货币""稳定币""RWA"等字样
  （[证券时报](https://www.stcn.com/article/detail/3633747.html)、[新华网](https://www.news.cn/fortune/20260206/b569f51297ce4d6c980c4954922cd7c2/c.html)）。
- **NexOS 的对应姿态（继承 GATEWAY_MONETIZATION.md §4 既有红线）**：
  1. 默认全部面向**测试网/私有链**（dev 链 anvil / Sepolia），主网/真实收款开关必须显式 env 配置；
  2. 产品文案不出现"发行代币/交易所/理财"语义；`currency:"nex"` 虚拟计价单位只做积分语义；
  3. 开源项目本身（代码能力）与运营行为（实币收款）分离——换真实收款地址前必须完成合规评估；
  4. 不向境内用户提供实币出入金的托管/兑换服务（那是非法金融活动红线）。

### 6.3 技术风险

- 公共 RPC 依赖：限流/停服/审查 → RpcRegistry 探活多源回退 + 本地 anvil 兜底（已设计）。
- 合约风险（三期才涉及）：锚定/NFT 合约需测试网长期运行 + 审计后再上主网。
- 测试网水龙头依赖：Sepolia 币获取不稳定 → dev 链为主、测试网为辅。

---

## 7. 拓扑图：现有底座 + 一期 dApp 组件

```
                              NexOS dApp 一期拓扑（链上验真闭环）
 ═════════════════════════════════════════════════════════════════════════════════════

   用户/外部 AI agent（浏览器 + 本地 secp256k1 私钥）
        │  ① POST /api/v1/nexhub/auth/challenge {pubkey 0x+66hex}
        │  ② sign(SHA-256(nonce)) 65B r||s||v      ← @noble/secp256k1（localStorage）
        │  ③ POST /api/v1/nexhub/auth/verify ──▶ Bearer token（24h）
        ▼
 ┌────────────────────────── os-api 网关 :8558（Rust axum）──────────────────────────┐
 │                                                                                    │
 │  ┌── 现有底座（不动） ──────────────────────────────────────────────────────────┐  │
 │  │  chain_auth（os-common 407 行）     IM / NexHub / api_market 三处共享实例     │  │
 │  │  nexhub_lobby（os-nexhub 9,293 行） 购买/悬赏 18 路由 · hub_entitlement      │  │
 │  │  api_gateway（≈5,000 行）           PaymentOrder 三币种 · admin confirm      │  │
 │  │  blockchain（≈2,350 行 20 路由）    7 链预设 · docker compose 节点/浏览器    │  │
 │  │  os-wallet（6,098 行契约+真实实现） ChainAdapter/WalletConnector/RpcRegistry │  │
 │  │                                    signing.rs EIP-191/712/Schnorr 验签       │  │
 │  └──────────────────────────────────────────────────────────────────────────────┘  │
 │                                                                                    │
 │  ┌── 一期新增（约 +600 行）───────────────────────────────────────────────────┐   │
 │  │  chain_verify（os-nexhub 新模块）                                          │   │
 │  │    evm_verify_tx(txid, to, value):                                        │   │
 │  │      eth_getTransactionByHash ─▶ to/amount/确认数                          │   │
 │  │      eth_getTransactionReceipt ─▶ ERC-20 Transfer 日志（二期）              │   │
 │  │    verify_payment（nexhub_lobby.rs:3773）末段换调本函数                     │   │
 │  │    PaymentOrder /confirm 复用同一核验                                      │   │
 │  └───────────────────────────┬──────────────────────────────────────────────┘   │
 └──────────────────────────────┼────────────────────────────────────────────────────┘
                                │ HTTPS JSON-RPC（env：NEXOS_EVM_RPC_URL 多地址探活回退）
                                ▼
        ┌───────────────────────────────────────────────────────────────┐
        │  链层（三档，全部现有能力）                                       │
        │  ① anvil dev 链 127.0.0.1:8545（blockchain.rs docker compose）   │
        │  ② Sepolia 测试网公共 RPC（PublicNode / Chainlist 可换）          │
        │  ③ 二期 Base(8453) / 自建 op-geth（aliyun 公网节点）              │
        └───────────────────────────────────────────────────────────────┘

   P2P overlay（os-p2p 14,116 行，Kademlia + AES-256-GCM）——与链层并行：
   106 主机 ●───● 113 ──● aliyun(203.0.113.2) ──● 云锚点(198.51.100.114)
   跨节点消息 Handle::send/on_msg · 分块传输 sha256 · os-identity 指纹账本
   （三期：信誉/账单 Merkle 根经此汇聚后上链锚定）

 一期数据流（一笔真实购买）：
   buyer --purchase{txid}--> nexhub --chain_verify--> Sepolia RPC
        <--ok-- hub_entitlement 落库（伪造 txid 在此被 400 拒绝）
```

---

## 8. 附录

### 8.1 关键源码索引（本文引用事实的文件清单）

| 文件 | 相关事实 |
|------|----------|
| `/home/oem/NexOS/crates/os-common/src/chain_auth.rs` | 三步认证、NONCE_TTL 60s、TOKEN_TTL 24h、EVM 展示名派生 |
| `/home/oem/NexOS/crates/os-wallet/src/{chain,connector,registry,signing}.rs` | ChainAdapter/WalletConnector/RpcRegistry/EIP-191-712 验签 |
| `/home/oem/NexOS/crates/os-api/src/handlers/blockchain.rs` | 20 路由、7 链预设（:663-721）、wallets.json 明文（:514）、python3 签名降级（:569-591） |
| `/home/oem/NexOS/crates/os-api/src/handlers/api_gateway.rs` | billing_mode 四模式、价目常量、payments 4 端点 |
| `/home/oem/NexOS/crates/os-nexhub/src/nexhub_lobby.rs` | 18 路由、price_sats/currency、bounty 生命周期、verify_payment（:3773） |
| `/home/oem/NexOS/crates/os-api/src/handlers/api_market.rs` | 发布者=公钥唯一通道、硬件探测、心跳负载 |
| `/home/oem/NexOS/crates/os-p2p/src/api.rs` | Handle::send（:355）/on_msg（:364）/connect |
| `/home/oem/NexOS/crates/os-identity/src/ledger.rs` | 四类证据、冲突观测、原子落盘 |
| `/home/oem/NexOS/docs/GATEWAY_MONETIZATION.md` | 三 phase 路线、价目表、安全红线 |
| `/home/oem/NexOS/docs/NEXHUB_ONBOARDING.md` | 三步上架、权限矩阵、认证契约 |
| `/home/oem/NexOS/docs/FEATURE_SURVEY_2026-08-20.md` | §1.1 三大方向、§5.3 安全隐患台账 |
| `/home/oem/NexOS/PHILOSOPHY.md` | 五类孤岛、规模数字 |

### 8.2 外部资料来源

- Base 网络费官方文档：<https://docs.base.org/base-chain/network-information/network-fees>
- L2Beat 成本追踪：<https://l2beat.com/layer2s/costs>
- Spark 链费对比：<https://www.spark.money/tools/chain-fee-comparison>
- L2 费用研究（arXiv，中位费降 95%+）：<https://arxiv.org/html/2606.22206v1>
- Chainlist（公共 RPC 聚合，Base Sepolia #84532）：<https://chainlist.org/chain/84532>
- CompareNodes Base 公共端点监控：<https://www.comparenodes.com/library/public-endpoints/base/>
- EIP-7702 解读（Openfort）：<https://www.openfort.io/blog/eip-7702>
- 账户抽象演进 4337→7702（Turnkey）：<https://www.turnkey.com/blog/account-abstraction-erc-4337-eip-7702>
- 9·24 通知官方原文（外汇局）：<http://m.safe.gov.cn/safe/2021/0924/19911.html>
- 锦天城律所解读（个人持有/交易风险）：<https://www.allbrightlaw.com/SH/CN/10475/1fea7d58626fdc70.aspx>
- 八部门新规报道（证券时报/新华网）：<https://www.stcn.com/article/detail/3633747.html> ·
  <https://www.news.cn/fortune/20260206/b569f51297ce4d6c980c4954922cd7c2/c.html>

---

*本文档为只读调研产物，未改动任何代码。一期实施时的端点契约/env 清单变更须同步回
`docs/NEXHUB_LOBBY_DESIGN.md` 与 `docs/GATEWAY_MONETIZATION.md`（功能文档同步铁律）。*
