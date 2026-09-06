# NexHub 优化过程记录（调研 + 设计 + 实现）

> 文档用途：**架构决策存档 + PPT 素材 + 后续 AI agent 接续开发依据**
> 日期：2026-08-15 · 负责人：主代理（调研/设计/实现/文档）
> 关联文档：`docs/NEXHUB_LOBBY_DESIGN.md`（大厅原始设计）、`crates/os-nexhub/README.md`
> 代码基线：`nexos` @ `c689a97`（main）

---

## 0. TL;DR（PPT 一页版）

- **NexHub 大厅已有**：发布/搜索/一键克隆到本地、SQLite 索引、nexos 自动 seed。
- **本次新增**：**货币化**——每个大厅条目可标「免费」或「虚拟货币价格」（btc/nex/usdc/eth）；
  付费条目克隆前必须 `POST /:name/purchase` 取得授权（落库 `hub_entitlement`），否则 `402`。
- **反向代理结论收紧**：大厅一旦「公开 + 付费」，反代（至少 TLS 终止）从「可选」变「必需」——
  付款凭据不能明文走公网。给出 Caddy 模板（`deploy/nexhub/Caddyfile`），零代码改动。
- **交付物**：`crates/os-nexhub/src/nexhub_lobby.rs`（+价格字段/+授权表/+购买路由/+克隆门禁）、
  设计文档 §10、Caddyfile、本过程文档。
- **一期未做（钩子预留）**：链上验真（接 `os-wallet`）、作者收款地址绑定、NEX 账本清算。

---

## 1. 背景与目标

用户原话（设计哲学）：

> NexHub 是非常重要的组件，需要一个大厅；个人创建的项目可以分享到大厅；可以**免费也可以设置
> 虚拟货币（如 btc）**；可以从大厅下到本地 NexHub；**网络是不是要有反代**；写代码的同时把过程
> 形成文件，供后续写 PPT 和其他 AI agent 使用。

目标拆成三块：**(A) 调研现状**、**(B) 优化设计（货币化 + 反代论证）**、**(C) 落地代码 + 文档**。

---

## 2. 调研结论

### 2.1 现状（已具备）

| 能力 | 位置 | 说明 |
|------|------|------|
| 大厅发布 | `nexhub_lobby.rs` `POST /publish` | 快照本地裸仓库元数据入库 `hub_lobby` |
| 大厅列表/搜索/详情/统计 | `GET /` `/stats` `/:name` | `?q` `?tag` `?sort` 过滤 |
| 一键克隆到本地 | `POST /:name/clone` | 服务端 `git clone --bare`（10s 超时兜底） |
| Seed | `seed_if_empty` | nexos 主仓库自动成为大厅第一条 |
| 反向代理分析 | `NEXHUB_LOBBY_DESIGN.md §6` | 结论：一期不反代，二期公网再上 Caddy |

### 2.2 缺口（vs 用户哲学）

1. **无货币化**：`hub_lobby` 没有价格/货币字段；发布只有「免费」语义；克隆无付费门禁。
2. **无购买/授权**：付费条目该如何「先付后下」完全没有链路。
3. **反代结论偏松**：原 §6 把反代当「二期可选」，但用户明确要「虚拟货币付费」→ 付费即涉
   敏感凭据，反代（TLS）应前置为「必需」。
4. **钱包能力未接**：`os-wallet` crate 已定义 `BTC/EVM` 适配器 + `VerificationFactor`
   （签名挑战/余额阈值/凭证），是付费验真的现成地基，但大厅未引用。

### 2.3 关键资产（可复用，避免重造）

- `os-wallet`：BTC/EVM `ChainAdapter`、`SignatureAlgorithm`（BIP-322/Schnorr/ECDSA/EIP-191/712）、
  `VerificationFactor`（余额阈值/凭证）——二期链上验真直接复用。
- `os-api::handlers::api_gateway` 的 `reqwest` 共享 Client + `proxy_forward`——「内置 git 反代」
  （远端不可达时 os-api 流式透传 `/git/*`）的先例，无需新组件。
- 既有 lobby 的 SQLite 模式（WAL + 三级路径降级 + `Mutex` 短锁）——新表照抄。

---

## 3. 设计决策

### 3.1 货币化模型（§10）

- **字段**：`hub_lobby` 增 `price_sats INTEGER DEFAULT 0` + `currency TEXT DEFAULT 'free'`。
  `price_sats==0` ⇒ 免费；`>0` ⇒ 付费，`currency∈{btc,nex,usdc,eth}`。
- **授权表** `hub_entitlement(repo_name, buyer, chain, txid, amount_sats, currency, paid_at)`，
  主键 `(repo_name, buyer)`。
- **门禁**：`POST /:name/clone` 对付费条目校验 `find_entitlement(buyer)`；命中或 `buyer==publisher`
  才放行，否则 `402 Payment Required`。
- **一期验真 `verify_payment()`**：校验「金额足额 + 货币一致 + txid 非空」即落库。这是
  **自证收据**阶段（不查链），但把「链上验真」留为显式钩子，二期替换为 `os-wallet` 调用，
  **一期不引入 os-wallet 依赖**（保持 os-nexhub 仅依赖 os-common，审计原则不变）。
- **发布者豁免**：作者克隆自己的付费条目免购，避免「自己买自己」。

### 3.2 反向代理（结论收紧）

| 场景 | 反代 | 理由 |
|------|------|------|
| 纯内网/单机、仅免费分享 | **不需要** | 单体 os-api 8080 即是入口，开箱即用 |
| 公开 + 付费 | **必需（至少 TLS 终止）** | 付款凭据/Token 不能明文走公网 |
| 公网多节点生产 | Caddy（自动 HTTPS + 限流 + 缓存） | 弹性优先 |

**为什么付费必须反代**：付费流程传 `buyer`、支付收据 `txid`、`NEXOS_ADMIN_TOKEN`。若 os-api
直接 `0.0.0.0:8080` 暴露公网且明文 HTTP，这些凭据可被嗅探/重放 → 收款地址与令牌裸奔。
Caddy `reverse_proxy localhost:8080` + 自动 ACME，os-api 收敛到 `127.0.0.1:8080`，零代码改动。

---

## 4. 实现清单（本次改动）

| 文件 | 改动 |
|------|------|
| `crates/os-nexhub/src/nexhub_lobby.rs` | `LobbyEntry` +2 字段；`hub_lobby` schema +2 列；新增 `hub_entitlement` 表+函数；新增 `Entitlement` 结构；`resolve_price()`/`verify_payment()` 纯函数；`POST /:name/purchase` 路由；`clone` 货币化门禁；路由数 6→7；+4 单测（货币化） |
| `crates/os-nexhub/README.md` | 路由表 6→7，功能补货币化说明 |
| `docs/NEXHUB_LOBBY_DESIGN.md` | §6.1 反代升级结论；新增 §10 货币化（模型/API/流程/一期二期） |
| `deploy/nexhub/Caddyfile` | 公开付费大厅反代模板（TLS 终止 + 限流 + 缓存 + 健康检查） |
| `docs/NEXHUB_OPTIMIZATION_2026-08-15.md` | 本过程文档 |

**未改动**（刻意）：`code_repo.rs` 一行未动（复用其 `repos_dir`/`build_clone_url*`）；前端
`CodeHub.vue` 仅需在已有 `nexhubLobby*` client 方法上扩展 `purchase`（调用方演示见 §7）。

---

## 5. 关键契约（给前端 / 其他 agent）

### 5.1 发布（付费）

```http
POST /api/v1/nexhub/lobby/publish
Authorization: Bearer <ADMIN_TOKEN>
{ "repo": "my-app", "price_sats": 1000, "currency": "btc",
  "tags": ["demo"], "publisher": "alice" }
# → 201 { ..., "price_sats": 1000, "currency": "btc" }
# 免费：省略 price_sats，或 "price_sats": 0 → currency 强制 "free"
```

### 5.2 购买授权（付费条目）

```http
POST /api/v1/nexhub/lobby/my-app/purchase
Authorization: Bearer <TOKEN>          # 任意已认证用户即可（非必须 admin）
{ "buyer": "bc1q...", "txid": "tx_abc", "amount_sats": 1000, "currency": "btc" }
# → 200 { "ok": true, "repo_name": "my-app", "buyer": "bc1q...", ... }
# 金额不足/货币不符 → 402；免费条目 → 400
```

### 5.3 克隆（货币化门禁）

```http
POST /api/v1/nexhub/lobby/my-app/clone
Authorization: Bearer <ADMIN_TOKEN>
{ "buyer": "bc1q..." }                 # 付费条目必带 buyer 且已购；发布者(buyer==publisher)豁免
# → 200 { "ok": true, "cloned": true, "download_count": 1, "clone_url_http": "..." }
# 未购 → 402 Payment Required
```

### 5.4 列表/详情响应新增字段

```json
{ "repo_name": "my-app", "price_sats": 1000, "currency": "btc",
  "clone_url_ssh": "ssh://oem@.../my-app.git", "clone_url_http": "http://.../git/my-app.git" }
```

---

## 6. 如何构建与验证

> 本机（Windows）无 cargo，无法在本地 `cargo build/test`。**验证在 ub2604（有 cargo）执行**。

```bash
# 在 ub2604 的 nexos 克隆（路径已迁 /home/oem/NexOS）
cd /home/oem/NexOS
git pull origin main            # 拿到本次 c689a97 之后的货币化提交
cargo test -p os-nexhub         # 期望全绿（含新增 4 个货币化单测）
cargo build -p os-api           # 0 警告
```

**新增单测覆盖**：`publish_free_and_paid_persists_price_and_currency`、
`paid_clone_requires_purchase_then_succeeds`（402→购买→200→发布者豁免→不足402）、
`verify_payment_rejects_*`、`resolve_price_free_and_paid_rules`。

**手动 curl 验收**（参考 `NEXHUB_LOBBY_DESIGN.md §8` 手册，扩展 purchase）：

```bash
TOKEN='Authorization: Bearer <TOKEN>'; B=http://127.0.0.1:8080/api/v1/nexhub/lobby
curl -s -X POST -H "$TOKEN" -H 'Content-Type: application/json' \
  -d '{"repo":"my-app","price_sats":1000,"currency":"btc"}' "$B/publish"
curl -s -X POST -H "$TOKEN" -H 'Content-Type: application/json' \
  -d '{"buyer":"bc1q...","txid":"tx1","amount_sats":1000,"currency":"btc"}' "$B/my-app/purchase"
curl -s -X POST -H "$TOKEN" -d '{"buyer":"bc1q..."}' "$B/my-app/clone"
```

---

## 7. 部署（公开 + 付费大厅的反代）

1. os-api 改为监听 `127.0.0.1:8080`（不暴露公网）。
2. 安装 Caddy，把 `deploy/nexhub/Caddyfile` 放到 `/etc/caddy/Caddyfile`，域名改为真实域名。
3. `caddy reload` → 自动签发 HTTPS 证书；大厅全量走 `https://<域名>`。
4. 前端 `CodeHub.vue` 大厅 Tab：读 `price_sats/currency` 渲染「免费 / 价格」徽标；付费条目
   显示「购买」按钮 → 调 `nexhubLobbyPurchase()`（在 `client.ts` 现有 `nexhubLobby*` 方法旁新增）；
   购买成功后再点亮「克隆到本地」。
5. **生产务必**把 `NEXOS_ADMIN_TOKEN` 换成强随机 token（明文 token 仅本地调试）。

---

## 8. 一期 vs 二期路线

| 阶段 | 内容 | 状态 |
|------|------|------|
| 一期 | 价格/货币字段、purchase 授权落库、clone 门禁 402、发布者豁免、自证收据 `verify_payment` | ✅ 已落地 |
| 二期·钩子 | `verify_payment_onchain()` 经 `os-wallet::ChainAdapter` 做 BTC/EVM 链上验真（余额阈值/BIP-322/凭证） | 预留 `verify_payment` 替换点 |
| 二期 | 作者 `publisher` 绑定钱包收款地址，付款直打作者；NEX 虚拟币内部账本清算；退款/纠纷 | 待设计 |
| 二期 | 大厅 Star/评分、作者主页、组织；联邦大厅（节点间索引同步，homepage_node 字段已预留） | 待设计 |
| 二期 | os-network「反代管理」UI（可视化编辑 Caddyfile / 启停） | 待设计 |

---

## 8.1 悬赏（bounty）增补（2026-08-15 第二轮）

- **需求**（用户原话）：大厅除有偿/无偿分享项目外，新增「悬赏」——出资求别人做某件事，
  例如悬赏更新一些 GitHub 上停更的项目，或其他想要做的东西。
- **设计要点**：与货币化（§3.1 / §10）共用同一套虚拟货币与支付校验，但语义相反——
  「卖成果」vs「求活」。新增 `hub_bounty` 表 + 8 条 `/api/v1/nexhub/bounty/*` 路由，
  状态机 `open → claimed → submitted → paid`（外加 `reject` 重开、`cancel` 取消）。
  **奖励必须 >0**（无偿请求不算悬赏）。验收时 poster 自证支付（复用 `verify_payment`），
  不足额 → 402；二期经 `os-wallet` 链上托管释放（避免「验收后不付」）。
- **改动文件**：
  - `crates/os-nexhub/src/nexhub_lobby.rs`：`Bounty` 结构 + `hub_bounty` 建表/持久化 +
    8 条路由与处理器 + 8 个单测（含完整生命周期、支付不足 402、状态机约束）。
  - `crates/os-nexhub/README.md`：路由表 7→15，功能与测试数同步。
  - `docs/NEXHUB_LOBBY_DESIGN.md`：新增 §11 悬赏（动机/原则/模型/API/流程/一期二期）。
- **验证**：`cargo test -p os-nexhub`（ub2604，cargo 1.97.1）→ **51 passed / 0 failed**
  （原 45 + 货币化 0 增量 + 本次 bounty 6 新测）。分支 `nexhub-monetization` 已推送裸仓。
- **关键契约（给前端 / 其他 agent）**：
  - 发布：`POST /api/v1/nexhub/bounty` `{title, reward_sats, currency, target_url?, description?, tags?, poster?}` → 201
  - 认领：`POST /api/v1/nexhub/bounty/:id/claim` `{hunter}` → claimed
  - 提交：`POST /api/v1/nexhub/bounty/:id/submit` `{hunter, solution_url}` → submitted（claimed 须本人）
  - 验收：`POST /api/v1/nexhub/bounty/:id/approve` `{txid, amount_sats?, currency?}` → paid（不足额 402）
  - 驳回：`POST /api/v1/nexhub/bounty/:id/reject` → open 重开（清认领/交付）
  - 取消：`POST /api/v1/nexhub/bounty/:id/cancel` → cancelled（仅 open）
  - 列表：`GET /api/v1/nexhub/bounty` `?status=open|claimed|submitted|paid|cancelled` `?q=关键词`
- **接续指引**：链上托管释放接 `os-wallet`（同 §9 第 2 点 `verify_payment_onchain` 思路）；
  从网关 auth token 取 caller 强制 `approve/cancel/reject` 限 poster、`claim/submit` 限 hunter；
  `deadline` 字段已预留「到期自动取消」；`target_url` 仅作参考文本，服务端不主动抓取 GitHub。

---

## 9. 给后续 AI agent 的接续指引

1. **代码入口**：所有大厅逻辑在 `crates/os-nexhub/src/nexhub_lobby.rs`，单文件 ~1800 行，
   遵循「SQLite `Mutex<Connection>` 短锁 + 阻塞 git 走 `spawn_blocking`」模式。
2. **加链上验真**：实现 `verify_payment_onchain(receipt, entry) -> Result<(),String>`，
   内部调用 `os-wallet` 的 `ChainAdapter::query_balance` / 签名验证；替换 `verify_payment`
   调用点（保留一期自证作为 fallback）。注意 `os-nexhub` 当前不依赖 `os-wallet`，若要调用需
   在 `Cargo.toml` 加 `os-wallet`（workspace dep）并桥接 `HandlerError`。
3. **加收款地址**：在 `hub_lobby` 增 `payee_address` 列，purchase 时把 `txid` 关联到作者地址；
   验真后把 `amount_sats` 记入作者账本（新表 `hub_ledger`）。
4. **前端**：`crates/os-api/web/src/api/client.ts` 已有 `nexhubLobby*` 方法，照葫芦画瓢加
   `nexhubLobbyPurchase(name, buyer, txid, amount, currency)`；`CodeHub.vue` 大厅 Tab 已有
   卡片/发布/克隆 UI，扩一个价格徽标 + 购买按钮即可。
5. **测试**：新逻辑请沿用本文件 §6 的单测风格（真实 git fixture + 内存 SQLite），`cargo test -p os-nexhub`。
6. **不要动**：`code_repo.rs`（大厅只复用其 pub 函数）、`/git/*` CGI 装配（留在 os-api）。

---

## 10. PPT 素材

### 10.1 三句核心叙事

1. 「大厅解决流动，本地解决留存」——NexHub 让个人项目从孤岛变成可被发现、被复用的资产。
2. 「免费是默认，付费是选项」——每个条目一行价格，克隆前先授权，作者对自己的成果有定价权。
3. 「免费大厅可裸奔内网，付费大厅必须 TLS 收口」——反代不是锦上添花，是收款安全底线。

### 10.2 架构图（建议画法）

```
        ┌──────────── 公网 ────────────┐
        │   Caddy（TLS 终止 / 限流）    │   ← 公开+付费时必需
        └──────────────┬───────────────┘
                       │ https
        ┌──────────────┴───────────────┐
        │  os-api :8080（127.0.0.1）    │
        │  ├ /api/v1/nexhub/lobby/*     │
        │  │   ├ list / detail / stats  │
        │  │   ├ publish (admin)        │
        │  │   ├ purchase (auth) ──▶ hub_entitlement │
        │  │   └ clone (门禁: 已购?) ──▶ git clone --bare │
        │  ├ /api/v1/coderepo/*         │
        │  └ /git/* (Smart HTTP)        │
        │  SQLite: hub_lobby + hub_entitlement + hub_bounty │
        └──────────────┬───────────────┘
                       │ 克隆落地
              /tank/git-repos/*.git（本地留存）
```

### 10.3 数据模型速览

- `hub_lobby`：+`price_sats`(INT) +`currency`(TEXT) —— 条目的「标价」。
- `hub_entitlement`：`(repo_name, buyer)` 主键 +`chain`+`txid`+`amount_sats`+`currency`+`paid_at`
  —— 谁为哪个条目付过款（授权索引）。
- `hub_bounty`：`id` 主键 +`title`+`description`+`tags`+`poster`+`reward_sats`+`currency`+`target_url`
  +`status`+`claimed_by`+`solution_url`+`deadline`+`created_at`+`updated_at`+`paid_at`+`payout_txid`
  —— 悬赏「出资求活」子资源（状态机见 §8.1 / 设计文档 §11）。
- `os-wallet`（二期）：`ChainKind`/`ChainAdapter`/`VerificationFactor` —— 链上验真地基。

---

_过程记录完毕。两轮优化（货币化 + 悬赏）代码均已提交并推送至分支 `nexhub-monetization`
（裸仓 `refs/heads/nexhub-monetization`），`cargo test -p os-nexhub` 51 passed。合并前注意
远程 `master` 已分叉（见会话记录），建议 rebase 到最新 `master` 后落地。_
