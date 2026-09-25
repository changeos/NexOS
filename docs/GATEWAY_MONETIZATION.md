# API 网关 —— AI 变现通道 × 区块链清算（架构定位与分 phase 路线）

> 定位（用户 2026-08-16）：**API 网关是 AI 变现的关键通道**，必须与区块链相通。
> NexOS 里每一份 AI 产能（推理 token / 生成图 / agent 能力）都经由网关定价、计量、结算。

## 1. 变现全景：一张图看懂钱的流向

```
        AI 产能                         网关（定价/计量/闸门）                区块链（清算/信任）
┌──────────────────┐   sk-os-key   ┌──────────────────────┐   付款    ┌──────────────────┐
│ 本地 vLLM(按需)   │ ────────────▶ │ 计费模式:            │ ◀──────── │ USDT / BTC / EVM │
│ 免费 API 渠道      │               │  free / per_token /  │  充值订单  │ 链上转账          │
│ 付费 API 渠道      │               │  per_image / credits │           │ (自动核验→入账)   │
│ 生图(sd-turbo)    │               │ 按量扣积分 + 402 闸门 │           │                  │
└──────────────────┘               └──────────┬───────────┘           └────────┬─────────┘
                                              │ 消费明细/账单                   │
                                              ▼                              ▼
                                   CallLog / stats（审计）        NexHub 悬赏 sats 定价
                                   （二期: 账单哈希上链锚定）      （同一条价值闭环）
```

三件已有资产各就各位：
- **网关**：One API 式渠道聚合 + sk-os- 令牌 + ModelRatio 计费 + 兑换码（→ 积分制升级中）
- **区块链模块**：7 条链预设 / RPC 节点启停 / 浏览器 / os-wallet（k256 EVM 密钥学）
- **NexHub 货币化**：sats 定价 + 付费门禁 + 悬赏——与网关共用"价值单位"语义

## 2. 分 phase 路线

### Phase 1（进行中，2026-08-16）：计费模式 + 充值订单
- token 四种计费模式（free/per_token/per_image/credits），配额统一为积分
- PaymentOrder：USDT/BTC/EVM 三币种，env 配收款地址，**admin 手动确认到账** → 积分入账
- 完成标志：创建 key 选模式、充值订单闭环（人工确认）

### Phase 1 实现参考（2026-08-20 核对源码，供协作 agent 直查）

**billing_mode 四模式语义**（源码 `crates/os-api/src/handlers/api_gateway.rs` 模块头
+ `is_valid_billing_mode`；创建令牌时缺省 `per_token`，非法值 400；旧 JSON/老库
缺列回落 `per_token`）：

| billing_mode | 语义 |
|--------------|------|
| `free` | 免费模式：转发不检查配额、不扣费（quota 字段忽略） |
| `per_token` | 按 token 计费（默认/现状）：`ModelRatio × GroupRatio` 扣 `quota_used` |
| `per_image` | 按生成图计费：每次生图成功按固定价 `IMAGE_PRICE_CREDITS` 扣 `quota_used`（`charge_image_call` 挂钩）；其文本转发仍按 per_token 计量叠加 |
| `credits` | 预付积分：创建时 `initial_credits` 写入 `quota_limit`，一切消费扣积分，`quota_used >= quota_limit` 拒绝（429） |

**价目常量**（全部 `pub const`，集中可调，源码位置
`crates/os-api/src/handlers/api_gateway.rs` §"计费模式与加密货币充值：常量价目表"）：

| 常量 | 值 | 语义 |
|------|----|------|
| `IMAGE_PRICE_CREDITS` | 100 | per_image 模式每次生图扣的积分 |
| `PRICE_USDT_PER_CREDIT` | 0.01 | 1 积分 = 0.01 USDT（两位小数） |
| `PRICE_SATS_PER_CREDIT` | 1,500 | 1 积分 = 1500 聪（BTC 以 sat 面额整数计） |
| `PRICE_WEI_PER_CREDIT` | 20e15 wei（0.02 ETH） | 1 积分的 EVM 价（wei 整数计，u128 乘法防溢出） |

**充值订单端点契约**（handler 同上文件；订单生命周期
`pending → confirmed(admin confirm，token quota_limit += credits，幂等) / rejected(记原因)`）：

| method | path | 鉴权 | 请求 | 响应 |
|--------|------|------|------|------|
| POST | `/api/v1/gateway/payments` | admin | `{token_id, currency(usdt/btc/evm), credits>0}`（非法币种 400） | 201 `PaymentOrder`（金额经 `crypto_amount_for` 换算；env 地址未配时 `address=""` 且响应附 `warning`） |
| GET | `/api/v1/gateway/payments` | — | `?status=pending/confirmed/rejected`（可空） | `PaymentOrder[]`（最新在前） |
| POST | `/api/v1/gateway/payments/:id/confirm` | admin | `{txid?, chain_id?, rpc_url?, erc20_contract?, erc20_decimals?}`（body 可空；2026-08-31 起 evm 订单带 txid 先过链上核验，2026-09-02 起 usdt 订单可走 ERC-20 核验 + 金额规则 AtLeast，见下「支付验真」节） | 200 确认后订单（+`chain_verify` 标注，ERC-20 时含 `token`）；核验不过 409/400；重复 confirm 拒绝 |
| POST | `/api/v1/gateway/payments/:id/reject` | admin | `{reason?}` | 200 拒绝后订单 |

**环境变量**（源码 `pay_address_for`，全部 grep 核实）：

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_PAY_USDT_ADDR` | 未设置（订单 address 为空 + warning） | USDT（TRON）充值收款地址 |
| `NEXOS_PAY_BTC_ADDR` | 同上 | BTC 充值收款地址 |
| `NEXOS_PAY_EVM_ADDR` | 同上 | EVM 链充值收款地址 |

> **当前占位值声明（演示用）**：实机经 `/etc/default/os-api`（os-api systemd 的
> EnvironmentFile）注入的是三个**占位地址**——
> `TPLACEHOLDER9USDT9DO9NOT9SENDxxxxxxxxx`（USDT）、
> `bc1qplaceholder9do9not9send9real9btcxxxx`（BTC）、
> `0x000000000000000000000000000000000000dEaD`（EVM）。
> 前端 `web/src/views/ApiGateway.vue` 的 `PLACEHOLDER_PAY_ADDRESSES` 会识别这三个
> 值并显示醒目红色警示"占位收款地址（未配置真实钱包）——请勿真实转账，支付通道
> 上线前仅供流程演示"，同时禁用复制按钮。**换真实收款前先改 env，前端警示随之消失。**

### 支付验真（dApp 一期 2026-08-31 落地 ✅ / 二期增强 2026-09-02 ✅）——confirm 接线链上核验

> Phase 2「自动确认」的第一根线（docs/DAPP_RESEARCH.md §3 方向 1）：admin confirm
> 不再纯人工——**evm 订单带 txid 时复用 NexHub 同一核验编排**（核验本体
> `os-nexhub::chain_verify`（`eth_getTransactionByHash`/`eth_getTransactionReceipt`），
> 业务接线 `os-nexhub::nexhub_lobby`「链上支付验真」段，`api_gateway.rs` confirm
> 分支直接 import）。核对四件事：收款地址（=订单 `address`，env `NEXOS_PAY_EVM_ADDR`）、
> 金额（=订单 `amount_crypto`）、链（`chain_id`）、执行成功（status==1）。
>
> **二期增强（2026-09-02）两项**：
> ① **ERC-20（USDT@EVM）Transfer 日志核验**——usdt 订单定位到 EVM 链（body/env
> `chain_id`）且有代币合约（body `erc20_contract` → env `NEXOS_USDT_EVM_CONTRACT`）
> 时，核验切换为 receipt `logs[]` 对账：`address==合约 && topics[0]==Transfer 签名
> 哈希 && topics[2]==订单收款地址`，`data`（uint256）按**最小单位**比对（订单
> `amount_crypto` 如 `"10.00"` 按小数位 6 换算成 `10000000` 微 USDT）；无匹配日志 →
> 409 Mismatch("erc20_log")。TRON 形态（定位不到 EVM 链 ID）或缺合约 → 仍 admin
> 人工（unverified 标注，**不猜合约地址**）。
> ② **金额规则 AtLeast**（网关 confirm 定稿）——链上金额 **≥** 订单应付额即过
> （充值多打不亏待用户：超额照常入账订单积分）；不足 → 409 Mismatch("value")。
> `Verified.value_wei` 恒为链上**实付**（落库 `chain_value_wei` 可审计）。

**结果语义表**（与 NexHub 购买/悬赏完全同款，单一实现两处复用）：

| 核验结局 | confirm 动作 | HTTP |
|----------|--------------|------|
| Verified | 确认 + 积分入账；`block_number`/`value_wei`（**链上实付**，AtLeast 下可能 > 订单额）落库 `payment_orders.chain_block/chain_value_wei`（老库 `ALTER TABLE` 幂等补列），ERC-20 时响应另附 `chain_verify.token`（合约地址） | 200 |
| Pending（未上块） | **不确认**——可重试语义非欺诈，稍后重试同一单 | 409 |
| Mismatch（地址/金额/链/状态/erc20_log 不符） | 不确认（订单留 pending、积分不加）；错误带字段名与链上实际值 | 409 |
| NotFound（链上无此交易） | 不确认 | 400 |
| RpcError（RPC 不可达/超时） | **降级放行**：确认 + `chain_verify:{status:"degraded"}` + 日志警告（网络故障不阻断入账；admin 仍在环内兜底） | 200 |
| 未带 txid / usdt(TRON 形态或缺合约)/btc 订单 | admin 链下手动确认（`chain_verify:{status:"unverified"}` 标注） | 200 |
| `NEXOS_CHAIN_VERIFY_ENABLED=0` | 整体回旧行为（txid 非空即过，无任何标注） | 200 |

**金额规则矩阵**（dApp 二期定稿，三处接线各自设置）：

| 业务线 | 规则 | 链上金额 vs 应付额 | 理由 |
|--------|------|--------------------|------|
| 网关 confirm（充值） | **AtLeast** | ≥ 即过（不足 Mismatch("value")） | 充值多打不亏待用户——超额照常入账订单积分，实付落库可审计 |
| NexHub purchase（购买） | **Exact** | 必须相等（多/少都 Mismatch） | 商品定价等值对账——须按应付额整额打款（docs/NEXHUB_LOBBY_DESIGN.md §10.6） |
| bounty approve（放款） | **AtLeast** | ≥ 即过 | 与自证面「金额足额」（≥ 奖励）语义对齐，多打不亏待 hunter |

**env 清单**（核验七件套；前三个与 `chain_verify.rs` 模块头一致，后四个为接线层）：

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_CHAIN_VERIFY_ENABLED` | `1` | 总开关；`0`/`false`/`off` = 回旧行为 |
| `NEXOS_CHAIN_RPC_URLS` | （空） | 节点级 RPC 预设 `{"<chain_id>": "<url>" 或 ["<url>",...]}`；坏值警告并忽略 |
| `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` | `10` | 单次核验 RPC 超时（下限 1s） |
| `NEXOS_EVM_CHAIN_ID` | （无） | evm/usdt 订单缺省链 ID（confirm body `chain_id` 优先） |
| `NEXOS_HUB_PAY_TO` | （无） | NexHub 购买流缺省收款地址（网关不用——订单自带 `NEXOS_PAY_EVM_ADDR` 地址） |
| `NEXOS_USDT_EVM_CONTRACT` | （无） | 二期 ERC-20：USDT@EVM 代币合约地址（0x+40hex；confirm body `erc20_contract` 优先；**都无则不核——不猜合约地址**） |
| `NEXOS_USDT_EVM_DECIMALS` | `6` | 二期 ERC-20：USDT 小数位（主流链 6；body `erc20_decimals` 优先；非法值警告回默认） |

confirm body 增量可选字段：`chain_id`（对准指定链核验）、`rpc_url`（admin 自配 RPC，
候选链第一段；候选链 = body rpc_url → `NEXOS_CHAIN_RPC_URLS` → 链预设公共 RPC 兜底）、
`erc20_contract` / `erc20_decimals`（二期：usdt 订单 ERC-20 核验的代币合约定位，
缺省回落上两行 env）。

**限制清单 / 降级策略（明示；二期更新 2026-09-02）**：

- **18 位小数假设（native evm 订单）**：native 金额对账以 wei 计，
  `PRICE_WEI_PER_CREDIT=20e15` 已按 18 位小数链定价；**非 18 位小数链不适用**
  （若配置此类链，价目须按其面额重算，当前未支持）。ERC-20（usdt）不受此限——
  金额按 `NEXOS_USDT_EVM_DECIMALS`（默认 6）换算成最小单位对账。
- **金额规则（二期改定）**：网关 confirm 已改 **AtLeast**（≥ 应付额即过，多打
  照常入账——旧版「等值比对」的不足额/超额都拒已废除）；NexHub 购买保持
  **Exact** 等值（见上矩阵）。
- **ERC-20 覆盖范围**：USDT@EVM 已核（Transfer 日志路径）；**USDT-TRON 与 BTC
  仍纯人工确认**（响应标 `unverified`）——TRON 核验与 BTC UTXO 扫描是后续。
  ERC-20 路径限制：`from`（topics[1]）不校验（凭证不含付款方期望）；同一交易
  内多笔同收款方 Transfer **不合计**（单笔日志须独立满足金额规则）；日志 data
  非 32 字节定宽/超 u128 → fail-closed 拒绝。
- **降级放行的风险权衡**：RPC 故障时确认照常（可用性优先，admin 兜底），台账口径
  「RPC 可用时核验，不白嫖」；要求强一致可在故障窗口内暂停 confirm。
- **Pending 重试建议**：409 +「请稍后重试」文案；订单保持 pending 可重复 confirm，
  前端建议对 Pending 做指数退避轮询（如 5s/15s/60s），上块后重试即过。

### Phase 2：链上自动核验（gateway ↔ blockchain 打通的第一根线）
- **EVM 存款地址派生**：每笔订单从 os-wallet 派生唯一收款地址（HD 派生或 nonce 地址），
  弃用静态 env 地址 → 一单一址，对账天然精确
- **自动确认**：区块链模块的 RPC client 轮询/订阅订单地址的入账交易
  （金额匹配 + 确认数阈值）→ 自动 confirm → 积分入账，admin 只处理异常
- 范围：先 EVM 系（含 ERC20-USDT）；BTC 上链核验依赖 UTXO 扫描，靠后
- 完成标志：EVM 充值全自动到账，无人值守

### Phase 3：结算与信任升级
- **账单上链锚定**：日账单哈希锚定到链上（防篡改审计），挂 explorer 可查
- **sats 统一价值单位**：网关积分 ↔ NexHub 悬赏 sats 汇率打通，一个钱包两侧消费
- **钱包即身份**：契约 ApiRequest 加 principal（登录体系上线后），
  sk-os- key 可绑定钱包地址，按地址归账
- 安全前置项：钱包私钥明文落盘问题必须先解决（调研报告安全隐患项）——
  变现通道的密钥不能裸奔

## 3. 为什么网关是变现通道的正确位置

1. **唯一计量的地方**：所有 AI 调用（无论本地/免费/付费上游）都过网关，token/图/次
   的计量只有这里有完整视角
2. **闸门与故障转移天然是商业逻辑**：402（余额不足切付费渠道）、优先免费上游、
   本地兜底——这就是成本最优的自动采购
3. **sk-os- 是"发票"**：CallLog 按令牌归账，兑换码/充值订单是"入金"，
   一套账本自洽
4. **与 NexHub 悬赏对称**：悬赏是"出资求活"（钱→任务），网关是"出资求算力"
   （钱→AI 用量）——同一套虚拟货币语义的两侧

## 4. 风险与红线

- 私钥安全：Phase 3 前必须完成钱包加密存储（当前明文 /tank/os-data/wallets.json）
- 价目表初版是拍脑袋值（代码内 pub const，集中可调）
- 链上自动核验先只做 EVM；BTC/跨链桥不承诺
- 法律合规：真实加密货币收款属敏感能力，默认全部面向**测试网/私有链**
  （区块链模块的本地节点预设），主网开关需显式配置

## 5. 对外接入（OpenAI 兼容，2026-08-31）✅

任何 OpenAI SDK / 兼容客户端**零改造**接入：换 Base URL + 换 key 即可。

**核心场景——把接入信息交接给 AI 助手**（2026-08-31 用户澄清后按此设计）：
前端「API 网关 → 接入说明」Tab 以「一键复制完整接入块」为中心（i18n 四语言）：

- **一键接入块**：一段可直接粘贴给任何 AI/agent/工具的自包含 markdown——
  Base URL（按当前访问主机动态拼）+ 完整 sk-os- key + 实时模型清单
  （`GET …/v1/models` 按令牌过滤，绝不猜模型名）+ 非流式/流式 curl +
  OpenAI SDK 片段。未生成令牌/未拉到模型时块内**显式占位并注明**，不编造。
- **生成接入令牌**：面板一键 `POST /api/v1/gateway/tokens` 建 free 令牌
  （`agent-access-<日期>`，不计量不限额——AI 试调不被配额卡住），创建响应的
  一次性完整 key 当场并入接入块。admin 语义如实标注：测试期默认 admin 放行，
  设了管理 token 的部署需先在 设置 → API 令牌 填好。
- **按模型复制**：模型清单每项带「复制接入」——只含该模型（如 qwen3-9b）的
  精简块（URL + key + model 名 + 单例 curl）。
- Base URL 拼法 / 计费 429 语义 / 鉴权口径作为面板辅助说明区。

### 5.1 接入要素

| 要素 | 值 |
|------|-----|
| **Base URL** | `http://<节点IP>:8558/api/v1/gateway/v1`（跨机用节点 IP；os-api 监听 `0.0.0.0`） |
| **鉴权** | `Authorization: Bearer <sk-os-令牌>`（网关令牌，与管理端 admin token 是两回事） |
| **chat 端点** | `POST …/v1/chat/completions`（`completions` 同理） |
| **模型列表** | `GET …/v1/models`（OpenAI list 形状，同一令牌鉴权） |
| **流式** | 请求体 `"stream": true` → `text/event-stream` 逐块透传（真流式，见 5.3） |

### 5.2 models 端点契约

```json
{
  "object": "list",
  "data": [
    { "id": "gpt-4o", "object": "model", "created": 1767225600, "owned_by": "nexos-gateway" }
  ]
}
```

- 聚合口径：启用渠道（再按令牌 `allowed_channels` 过滤）的 `models` 字段
  ∪ 映射 `public_name`（映射目标渠道须仍启用且在 allowed_channels 内，不虚列），
  去重 → 按令牌 `allowed_models` 过滤（空=不过滤）→ 按 id 排序。
- `created` 是**协议占位常量**（`MODELS_LIST_CREATED_TS`，2026-01-01T00:00:00Z），
  非业务数据；`owned_by` 固定 `nexos-gateway`。
- 列模型**不查配额**（无消费不 429）；401 语义与转发端点一致（缺/错 key、
  禁用/过期令牌）。

### 5.3 流式语义（实现要点，http.rs `gateway_openai_handler`）

- **真流式**：SSE 请求经 http.rs 特挂的 axum 路由（同路径特挂优先，非流式
  回落原 dispatch 整包路径，零回归），复用 `resolve_forward_plan` 鉴权/选路
  （与非流式完全同口径），reqwest `bytes_stream()` 逐块透传。上游内容
  content-type 原样透传（缺省 `text/event-stream`）+ `cache-control: no-cache`。
- **故障转移边界（首字节语义）**：
  - **首字节前**（连接失败 / 非 2xx / 首个数据块读取失败或为空）→ 记 failed
    日志后切换下一候选渠道（客户端还没收到任何字节，切换安全）；
  - **首字节后**（首个数据块已透传）→ **不再切换**（切了会把两个渠道的流
    拼在一起串流）。上游中途断流则断开连接，末尾补一条 `: gateway: …`
    SSE 注释帧（SSE 规范忽略注释行，不污染数据帧）并记 failed 日志。
- **usage 与计费诚实**：透传时保留 SSE 尾部 64 KiB 窗口，流结束解析**最后一个**
  含 `usage` 的 `data:` 块（OpenAI 在 `stream_options.include_usage=true` 时于
  末块下发）。解析到 → 按真实 pt/ct/tt 记账扣费；未上报 → 记 0 并在调用日志
  `error` 字段注明「上游未上报 usage（流式计费记 0）」——**禁止估算编造**。
  建议客户端带 `stream_options: {"include_usage": true}`（网关不代注入：部分
  上游不认 stream_options 会 400，注入属改变上游行为）。
- **超时**：流式用独立 reqwest client（`HTTP_STREAM`，无总超时——总超时会掐断
  长流；仅 10s 连接建立超时快速失败给故障转移），非流式总超时**300s**（2026-09-03
  由 60s 提升——devdocs AI 翻译 6K 字块 + 思考段生成实测超 60s 被掐出 502；
  env `NEXOS_GATEWAY_UPSTREAM_TIMEOUT_SECS` 可配，见 5.6）。连接阶段另有共享
  client 的 10s connect_timeout 独立先掐——死/静默上游照样快速故障转移，
  不因总超时放大而变慢。

### 5.4 curl 三例

```bash
# ① 非流式
curl http://127.0.0.1:8558/api/v1/gateway/v1/chat/completions \
  -H 'Authorization: Bearer sk-os-你的令牌' \
  -H 'Content-Type: application/json' \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "你好"}]}'

# ② 流式（SSE 逐块透传；include_usage 让上游末块上报用量供计费）
curl -N http://127.0.0.1:8558/api/v1/gateway/v1/chat/completions \
  -H 'Authorization: Bearer sk-os-你的令牌' \
  -H 'Content-Type: application/json' \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "你好"}],
       "stream": true, "stream_options": {"include_usage": true}}'

# ③ 模型列表（OpenAI SDK client.models.list() 等价）
curl http://127.0.0.1:8558/api/v1/gateway/v1/models \
  -H 'Authorization: Bearer sk-os-你的令牌'
```

### 5.5 令牌获取

- **AI 交接路径（推荐）**：API 网关 → 接入说明 Tab →「生成接入令牌」——
  一键建 free 令牌（`agent-access-<日期>`），完整 key 自动并入一键接入块，
  无需手工复制中间步骤。
- 页面：API 网关 → 令牌 Tab →「创建令牌」（选计费模式/配额/允许模型）；
  **完整 key 仅创建响应显示一次**（列表只显示打码 key，创建时立即复制）。
- 脚本：`POST /api/v1/gateway/tokens`（admin 鉴权），body 如
  `{"name":"我的应用","billing_mode":"per_token","quota_limit":0,"allowed_models":[]}`，
  响应 201 含一次性明文 `key`。

### 5.6 env

| env | 缺省 | 语义 |
|-----|------|------|
| `NEXOS_GATEWAY_UPSTREAM_TIMEOUT_SECS` | `300` | 非流式转发总超时（直连 `forward_upstream` 与中继 `relay_roundtrip` 整包同源同值；合法域 1..=3600，越界/非数字回落 300）。2026-09-03 由写死 60s 改可配——devdocs AI 翻译（6K 字块 + 思考段）超 60s 被掐 502。连接失败不受影响：连接阶段由共享 client 的 10s connect_timeout 独立先掐。 |
| `NEXOS_AUTH_DEFAULT_ADMIN` | 测试期 `1` | 只影响**管理端**默认 admin 注入；对外端点（`/gateway/v1/*`）一律凭 sk-os- 令牌鉴权，不受该开关影响。其余复用网关既有 env（`NEXOS_PAY_*` 收款地址、`NEXOS_EVM_*` 链上核验等）。 |

## 6. 渠道中继（via_node）与局域网共享联邦模型（2026-09-03）✅

**用户原话定稿**：nexos 的 api 对于收到的 p2p 的 api 也可以转发出去，为 nexos
所在局域网的 ai 提供 api 支持。

### 6.1 中继渠道语义

`gateway_channels` 新增 `via_node TEXT DEFAULT ''`（幂等 ALTER，存量行 `''` =
直连语义不变）：

- **`via_node` 非空 = 中继渠道**：转发**不直连**上游——经 os-p2p overlay 定向
  该源节点（NodeID，0x+66hex）代发，复用 api_market 的 `api_relay_req` /
  `api_relay_resp` 分块协议执行层（llm_external 的 via_node 中继同一套）。
  请求面：URL = `channel.base_url + path`、鉴权头 = `channel.api_key`
  （Bearer）、`via_node` = 定向目标。
- **非流式**：`proxy_forward` 的故障转移循环里，中继渠道走 `relay_roundtrip`
  整包（源端读完上游再回）；失败记 failed 日志（文案带「经 <节点短式> 中继
  失败」可区分）后照常转移下一渠道。
- **流式**：http.rs 特挂路径对中继渠道用 `relay stream:true` 的分块回传——
  首帧 Head 即上游响应头/状态，后续 chunk 与直连 `bytes_stream()` 汇成同一
  `Result<Bytes, String>` 流，`sse_forward_stream` 的逐块透传/尾部 usage 记账
  语义**零变化**；首字节故障转移边界照旧（首块前可切渠道，开吐后不切）。
- **本地计费/鉴权照常**（节点自治）：本局域网调用者仍凭本节点的 sk-os- 令牌
  （401/403/429 语义与直连渠道完全一致），usage 从上游响应/SSE 尾块解析后按
  本节点 ModelRatio×GroupRatio 扣配额，调用日志/统计不受中继影响。

### 6.2 一键导入（外部 API → 网关渠道）

`POST /api/v1/gateway/channels` body 增可选 `from_external_api: <登记 id>`：

- 复制 `llm_external_apis` 行的 `name` / `base_url` / `api_key` / `models` /
  `via_node`（provider 缺省 `openai`；priority/weight 可覆盖，缺省 0/1）；
- `models` 为空 → 导入时先探 `<base_url>/models` 回填（via_node 非空经中继
  探测，否则直连——产物与连通测试同构，真实清单零编造）；探测失败**不阻塞
  导入**（字段全是登记表里的真实数据），响应带 `warning` 说明；
- 登记不存在 404；llm 组件未装配 503。
- 前端双向入口：模型管理 → 外部 API 卡片「发布到网关」；API 网关 → 添加渠道
  对话框「从外部 API 导入」。渠道列表对 via_node 非空的渠道显示 🌐 中继徽章。

### 6.3 局域网共享场景（用法闭环）

1. 消费节点在联邦大厅/API 市场导入外部 API（via_node 自动写入），或在模型
   管理手填后编辑补 via_node；
2. 点「发布到网关」→ 生成中继渠道（可再调优先级/权重/计费令牌）；
3. **本局域网任何 AI 工具**指向 `http://<本节点>:8558/api/v1/gateway/v1` +
   本节点令牌（sk-os-）→ 网关路由到中继渠道 → overlay → 源节点代发——对接入
   方完全透明（与非中继渠道同形）。「接入说明」Tab 的辅助说明区有
   「局域网共享联邦模型」一段（i18n 四语言）。

### 6.4 信任链与安全边界

- 信任链：**本节点令牌持有者 → 经本节点 → 源节点白名单内的 URL**。本节点
  网关令牌体系不变（sk-os- 签发/禁用/配额/计费全在本地）；relay 白名单在
  **源节点侧**不变（封闭集合：仅其已发布条目的 base + `/models` +
  `/chat/completions`，其余 403——源节点不是开放代理）。
- 消费节点侧的渠道 `base_url`/`api_key` 是管理员显式登记/导入的数据；中继
  渠道失败自动按优先级故障转移到本节点其他（含直连）渠道。
