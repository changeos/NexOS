# TIPS —— 统一打赏原语（链上身份账本）

> 组件 `tips`（`crates/os-api/src/handlers/tips.rs`），2026-08-31。
> 前端组件 `crates/os-api/web/src/components/TipButton.vue`。

## 1. 概念

打赏 = **一条真实账本记录**：`from`（打赏者链上身份 pubkey）→ `to`（目标所有者
pubkey），挂到具体目标（IM 消息 / 大厅条目 / 节点）。四个大厅面（IM 聊天、
NexHub 代码大厅、模型大厅、API 大厅）共用同一原语——「所有大厅的打赏功能」。

- **服务端不虚构链上转账**：`amount` 为站内积分记账；可选 `txid` 字段登记
  用户自报的真实链上凭证（**不验真**，见 §7 安全声明）。
- **to_pubkey 服务端解析**（防自报伪造）：请求体不含收款方，服务端按
  target 反查目标所有者（§4 映射表）。
- **from 链上身份优先**：Bearer 链上 token（IM / NexHub 两桶依次验）→
  pubkey；无 token 回落网关 Principal（测试期默认注入 admin，保留字
  `"admin"`）。

## 2. 端点契约（3 条，component=`tips`）

链上 token 在 handler 内自验（网关系统中间件不认识链上 token，挂
`requires_auth=true` 会全拦 401——与 api-market 同理），故三条路由全部
`requires_auth=false`；写操作身份由 handler 解析。

| method | path                             | 动作 | 鉴权 |
|--------|----------------------------------|------|------|
| POST   | `/api/v1/tips`                   | 打赏入账 → 202 | 链上 token 优先 / Principal 回落 |
| GET    | `/api/v1/tips/target/:kind/:ref` | 目标聚合（公开读） | 无 |
| GET    | `/api/v1/tips/me`                | 我的收到/给出聚合 | 同 POST（按身份） |

### POST /api/v1/tips

请求：

```json
{
  "target_kind": "lobby_entry",
  "target_ref": "nexhub:nexos",
  "amount": 100,
  "message": "好项目",
  "txid": "0xabc123…"
}
```

- `target_kind` ∈ `im_message | lobby_entry | node`（非法 → 400）。
- `target_ref` 非空、≤512 字符（格式见 §4）。
- `amount` 正整数（≤0 → 400；DB 层 CHECK(amount>0) 兜底）。
- `message` 可选 ≤500 字符；`txid` 可选 ≤128 字符。

响应 `202`（from/to 脱敏前缀）：

```json
{
  "ok": true,
  "id": 7,
  "from": "0x0123456789…",
  "to": "0xabcdef0123…",
  "target_kind": "lobby_entry",
  "target_ref": "nexhub:nexos",
  "amount": 100,
  "created_at": 1756627200
}
```

失败：`400`（目标不存在 / 所有者非链上身份 / 参数非法，带原因）、
`401`（无 token 且网关 Principal 亦 None——如 `NEXOS_AUTH_DEFAULT_ADMIN=0`
且未带有效 JWT）。

### GET /api/v1/tips/target/:kind/:ref

响应（公开读；recent 脱敏：from/to/txid 前 10 字符 + `…`，短保留字原样）：

```json
{
  "target_kind": "im_message",
  "target_ref": "f81d4fae-7dec-11d0-a765-00a0c91e6bf6",
  "total": 350,
  "count": 3,
  "recent": [
    {
      "from": "0x0123456789…", "to": "0xabcdef0123…",
      "target_kind": "im_message", "target_ref": "f81d4fae-…",
      "amount": 100, "message": "说得好", "txid": null,
      "created_at": 1756627200
    }
  ]
}
```

`recent` 新在前、上限 20 条。目标尚无打赏 → `total=0, count=0, recent=[]`
（不校验目标存在性——前端并行拉取零成本）。

### GET /api/v1/tips/me

响应（按解析出的身份聚合；recent 两列各上限 10 条）：

```json
{
  "identity": "0xabcdef0123…",
  "received": { "total": 300, "count": 2 },
  "given":    { "total": 100, "count": 1 },
  "recent_received": [ …TipRecentEntry… ],
  "recent_given":    [ …TipRecentEntry… ]
}
```

## 3. 持久化与 env

账本 SQLite `tips` 表（WAL + 幂等建表 + `CHECK(amount>0)` +
`(target_kind,target_ref)` / `from_pubkey` / `to_pubkey` 三索引）：

```sql
CREATE TABLE IF NOT EXISTS tips (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    from_pubkey TEXT NOT NULL,
    to_pubkey   TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_ref  TEXT NOT NULL,
    amount      INTEGER NOT NULL,
    message     TEXT,
    txid        TEXT,
    created_at  INTEGER NOT NULL,
    CHECK(amount > 0)
);
```

DB 路径默认链（llm.rs 同款写法）：

```
env NEXOS_TIPS_DB  →  /tank/os-data/tips.db  →  /var/lib/os/tips.db  →  ./tips.db
```

## 4. target_kind × target_ref 映射表（四大厅）

`lobby_entry` 的 ref 以 `<来源>:` 前缀分流三大家大厅；to_pubkey 解析路径
（来源库只读打开，与各家 handler 同库同路径）：

| 大厅面 | target_kind | target_ref 格式 | 例 | to_pubkey 解析 |
|--------|-------------|-----------------|----|----------------|
| IM 聊天 | `im_message` | IM 消息 id | `f81d4fae-7dec-…` | im.db `im_messages.sender_id`（`fed:<node>:<pubkey>` 剥前缀取末段；`system` 等非链上身份 → 400） |
| NexHub 代码大厅 | `lobby_entry` | `nexhub:<repo_name>` | `nexhub:nexos` | hub_lobby.db `hub_lobby.publisher`（合法 pubkey → owner；平台托管串 `NexOS`/`local` 等 → 400） |
| 模型大厅 | `lobby_entry` | `model:<name>@<sharer>` | `model:qwen3.5-9b@0xabc…` | model_lobby.db `model_lobby.sharer`（行 id 即 `<name>@<sharer>`，前端按 name+sharer 精确重建；sharer 非 pubkey（如 `admin`）→ 400） |
| API 大厅 | `lobby_entry` | `apimarket:<id>` | `apimarket:u1-…` | api_market.db `api_market.publisher_pubkey`（链上身份唯一通道，恒 pubkey） |
| 节点 | `node` | NodeID（`0x`+66hex） | `0x04ab…`（66hex） | NodeID **本身即**节点身份公钥（os-p2p identity：NodeId = 压缩 secp256k1 验签公钥，与 chain_auth 身份串同构；node-meta/peers 的 NodeID 同源） |

注意：

- 模型大厅前端合并条目（同 name 多源）按**源**各挂一枚 TipButton
  （ref 含各自 sharer），每枚累计数独立。
- 解析失败一律 `400` 带原因（目标不存在 / 所有者非链上身份 / 来源库
  不可读 / ref 前缀未知）——平台托管条目（publisher=`NexOS`）无链上身份，
  不可打赏，这是设计语义而非错误。

## 5. 拓扑（ASCII）

```
前端四大厅面（crates/os-api/web/src/）
  Chat.vue（IM 消息操作区）      target=im_message:<msg id>
  CodeHub.vue（大厅条目操作区）   target=lobby_entry:nexhub:<repo>
  ModelHub.vue（大厅条目卡片）    target=lobby_entry:model:<name>@<sharer>
  ApiGateway.vue（API 大厅卡片）  target=lobby_entry:apimarket:<id>
        │ 全部经
        ▼
  TipButton.vue（通用打赏按钮+弹窗：金额/留言/可选 txid；
  挂载即并行 GET /tips/target/:kind/:ref 显示真实累计数）
        │  endpoints.tipCreate / tipsTarget / tipsMe（client.ts tips 段）
        ▼
  REST /api/v1/tips*（axum 网关，requires_auth=false）
        ▼
  tips.rs（TipsRouteHandler）
   ├─ from：Bearer 链上 token → ChainAuth(IM 桶) / ChainAuth(nexhub+api-market 桶)
   │        依次验 → pubkey；无 token → 网关 Principal（extract_principal
   │        测试期默认 admin → 保留字 "admin"）
   ├─ to：按 §4 映射表反查来源库（im.db / hub_lobby.db / model_lobby.db /
   │        api_market.db，只读）——防自报伪造
   └─ INSERT tips.db（tips 表，CHECK(amount>0)）
        ▲
        └─ main.rs 装配：TipsRouteHandler::with_shared_auth(im_auth, nexhub_chain_auth)
           （与 IM handler / nexhub-lobby / api-market 共享同一批 token 桶）
```

### 大厅接入方式权衡（前端并行拉取，后端零侵入）

选择：**前端 TipButton 并行拉取聚合端点**，各大厅 handler 不改数据结构。
- 代价：每个条目卡片多一跳 GET（本地账本轻查询，可忽略；无打赏时返回
  零值）。
- 收益：NexHub/模型/API 三家大厅 handler（含联邦同步、快照刷新等复杂
  路径）零改动、零回归面；打赏语义完全集中在 tips.rs 单点演进。
- 备选（未采）：大厅列表端点内联 `tips_total/tips_count`——需跨组件读
  账本或反向依赖，四家 × 两处（列表/详情）改动面大。

## 6. 身份链路（chain_auth）

- 私钥永不出客户端（IM / NexHub 共用同一密钥对，各自挑战-签名取独立
  token——两 `ChainAuth` token 桶互不相通）。
- tips 接收 Bearer token 后**依次**验 IM 桶、nexhub 桶——用户在任一
  侧认证过即可打赏，from 恒为 token 反查 pubkey（与 im.rs caller、
  api-market caller 同款语义）。
- 验不过 / 无 token → 回落网关 Principal（`extract_principal`：JWT sub
  或固定 admin token；测试期 `NEXOS_AUTH_DEFAULT_ADMIN≠0` 时无凭据请求
  注入 admin Principal）→ from = 用户名（admin 即保留字 `"admin"`）。
  `"admin"` 无链上身份，**不能作为打赏收款方**（to_pubkey 恒经
  `parse_pubkey` 校验）——admin 只能给出、不能收到，账本诚实记录。

## 7. txid 不验真声明与安全隐患台账

`txid` 是**用户自报**的链上转账凭证：服务端只做长度（≤128）与非空
净化，**不连接任何链、不核对任何交易**。与 NexHub purchase 的自证收据
（`docs/FEATURE_SURVEY_2026-08-20.md` §5.3 S1）同已知限制——按项目惯例
「只记录不处理」，已追加台账条目 S7（见下），不在本期修复。

台账新增条目（追加于 `docs/FEATURE_SURVEY_2026-08-20.md` §5.3 表末）：

| # | 隐患 | 位置 | 等级 |
|---|---|---|---|
| S7 | tips txid 自报不验真：任意非空字符串即可在账本登记"链上凭证"（仅展示层凭证，amount 为站内积分，不产生授权/提现，危害有限；与 S1 同类） | handlers/tips.rs（handle_create，txid 仅长度校验） | 低 |

## 8. v1 边界与二期

v1（本期）：

- 账本是**本地账本**：不联邦同步（tips 不经 os-p2p 广播，跨节点各自记账）。
- amount 是站内积分记账，**无提现、无余额、无对账**。
- to_pubkey 解析只读本地来源库；联邦远程条目（如 NexHub 联邦同步来的
  `source_node≠local` 条目）的 owner 若为链上身份照常可打赏（publisher
  字段随联邦载荷同步），但打赏记录只在本地。
- 平台托管条目（无链上身份 owner）不可打赏（400 带原因，设计语义）。

二期（预留，不在本期）：

- **联邦同步**：tips 账本经 os-p2p 联邦广播/合并（同 im 大厅、NexHub
  大厅的 fed 模式），跨节点聚合 total/count。
- **链上真实转账**：txid 验真（连接对应链核对交易存在性/金额/收付方），
  或直接由服务端/钱包发起链上转账。
- **提现**：积分余额 → 链上资产兑换（引入 os-wallet connector 做真实
  签名广播；钱包私钥明文存储为既有旧患 S-旧，届时一并处理）。

## 9. 测试（handlers/tips.rs `#[cfg(test)]`，11 例全绿）

- `routes_declares_all_tips_endpoints`：3 条路由、component=tips、
  全部 requires_auth=false。
- `ledger_rejects_non_positive_amount_via_check`：直插合法行 + 0/负数
  被 `CHECK(amount>0)` 拒。
- `post_tip_rejects_bad_kind_ref_amount`：未知 kind / 空 ref / amount≤0 /
  留言超长均 400。
- `post_tip_resolves_im_message_author`：普通消息作者、联邦消息
  （`fed:node-a:<pubkey>`）、系统消息 400、消息不存在 400。
- `post_tip_resolves_three_lobby_sources`：nexhub（链上/平台托管 400）、
  model（链上/admin 400）、apimarket、未知前缀 400。
- `post_tip_node_target_must_be_valid_pubkey`：合法 NodeID 202（to=ref）、
  昵称式 400。
- `from_identity_prefers_chain_token_then_principal`：IM token → from=
  pubkey；无 token → Principal admin；两者皆无 → 401。
- `target_aggregation_and_recent_masking`：25 条 → total=325/count=25、
  recent 截 20 新在前、脱敏与时间戳、未知 kind 400。
- `me_aggregation_by_identity`：admin given 聚合；链上身份 received 聚合
  （token 视角）。
- `ledger_survives_reopen_same_db`：同库重开账本仍在（WAL 持久化）。
- `unmatched_path_returns_404`：未知路径 / target 缺 ref 段 404。

测试用临时 DB（`std::env::temp_dir()` + 进程号 + 原子计数，llm.rs 手法）；
来源库以最小 schema 临时文件注入（`TipsRouteHandler::with_parts`）。
