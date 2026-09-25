# NexHub 大厅（Hub Lobby）设计与实施

> 状态：实施中 · 文档用途：架构决策存档 + PPT 素材 + AI agent 接续开发依据
> 日期：2026-08-14 · 负责人：主代理（设计与验收）/ 子代理（分批实施）

## 1. 需求（用户原话拆解）

- NexHub 是非常重要的组件，需要有一个**大厅**
- 个人创建的项目可以**分享到大厅**
- 也可以**从大厅下（克隆）到本地** NexHub
- 网络层评估：**是否需要反向代理**
- 实施过程同步形成文档（供 PPT 与其他 AI agent 复用）

## 2. 产品定位

大厅 = NexHub 的**发现层**。对标关系：

| GitHub 概念 | NexHub 对应 |
|---|---|
| Explore / Trending | 大厅列表（卡片流） |
| Public 仓库 | 已发布到大厅的本地仓库 |
    | git clone | 一键「克隆到本地」（服务端 spawn git clone） |
| Federation（NexOS 特有） | 跨节点大厅索引同步（二期） |

一句话：**本地 NexHub 解决"留存"，大厅解决"流动"** —— 契合"打破代码孤岛"理念（PHILOSOPHY.md 第五条孤岛）。

## 3. 架构设计

```
┌────────────────────────── 节点 A（ub2604）──────────────────────────┐
│  前端 CodeHub.vue                                                   │
│   ├─ Tab1 我的仓库（现有 scan_repos）                                │
│   └─ Tab2 大厅（新）：卡片流 / 搜索 / 标签 / 发布 / 一键克隆           │
│                        │ REST                                       │
│  os-api 8080 ──────────┤                                            │
│   ├─ /api/v1/nexhub/lobby/*   大厅 API（新 handler: nexhub_lobby）   │
│   ├─ /api/v1/coderepo/*       本地仓库管理（现有 code_repo）          │
│   ├─ /git/*                   HTTP Smart Git（现有，克隆传输通道）     │
│   └─ SQLite hub_lobby 表（发布索引，复用 IM 的 SQLite 模式）          │
│                        │                                            │
│   /tank/git-repos/*.git  裸仓库（本地留存 + 克隆落地目标）             │
└─────────────────────────────────────────────────────────────────────┘
                         │ 二期：联邦（节点间大厅索引同步）
                ┌────────┴────────┐
                │  节点 B（手机/    │  ← BLE mesh / 局域网 / 公网中继
                │  其他 NexOS）    │
                └─────────────────┘
```

**复用已有资产**：HTTP Smart Git（克隆传输）、code_repo 的 scan_repos/build_clone_url（仓库元数据）、IM 大厅模式（SQLite 表 + 列表 API 的先例）、reqwest 共享 Client（二期联邦探测）。

## 4. 数据模型（SQLite `hub_lobby`）

| 字段 | 说明 |
|---|---|
| repo_name | 唯一键 |
| description / tags(JSON数组) | 展示与搜索 |
| publisher | 发布者（用户/agent 名） |
| source_url | 克隆源（本机路径 / http / ssh） |
| homepage_node | 来源节点 id（**联邦预留**） |
| source_node | **联邦来源节点（P3，2026-08-22）**：本地发布恒 `'local'`；经 os-p2p 同步的远程条目 = 发布节点名（幂等 ALTER 补列，存量行回填 `'local'`） |
| clone_url_http | **发布节点的 HTTP 克隆地址（2026-08-25 跨节点拉取修复）**：发布/常驻刷新时经 `build_clone_url_http` 定格（advertise_host 地址优先链 → 可达 IP），联邦载荷原样携带；消费节点一键克隆联邦条目经此 URL 从源节点拉取。旧库/旧 payload 无此字段 → 空串（历史条目需源节点重 publish 刷新地址） |
| commit_count / size_bytes / default_branch / last_commit(_date) | 快照元数据 |
| readme_excerpt | README.md 前 500 字（卡片摘要） |
| download_count | 克隆计数（活跃度） |
| published_at | 发布时间 |

**决策**：大厅存"发布快照"而非实时扫描——发布时快照一次，浏览零开销；仓库删除时大厅条目级联清理。

## 5. API 设计（新 handler `nexhub_lobby`，路由前缀 /api/v1/nexhub/lobby）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | 大厅列表，`?q=`关键词 `?tag=`标签过滤，按 download/published_at 排序 |
| GET | `/:name` | 详情（含 readme_excerpt 与双通道 clone 地址） |
| POST | `/publish` | 发布本地仓库：body `{repo, description?, tags?, publisher?}`；校验仓库存在→快照元数据→入库；重复发布=更新快照 |
| DELETE | `/:name` | 下架 |
| POST | `/:name/clone` | 克隆到本地（克隆源选择）：本机条目（source_node/homepage_node=local 或 source_url 本机存在）→ `git clone --bare <source_url>`（10s）；联邦条目 → 条目自带 clone_url_http 经 HTTP 从源节点拉取（120s）；两者皆无 → 502（错误区分「本机路径不存在 / 源节点不可达」）→ download_count+1；目标已在本机则直接注册 |
| GET | `/stats` | 发布数/总下载/标签云聚合 |

鉴权：全部走现有 admin token 中间件（读接口可豁免，与 coderepo 惯例一致）。

**Seed（nexos 常驻）**：`nexos` 主仓库**默认常驻大厅**——每次启动无条件确保已发布：条目不存在 → 自动发布为大厅第一条（publisher: NexOS）；已存在 → 刷新快照并保留 download_count（推送新代码后 commit 数 / last_commit / README 摘要不过期；下架后重启会回来）。逃生口：env `NEXOS_LOBBY_NO_AUTO_PUBLISH=1` 跳过（用户显式下架 nexos 后不想被启动拉回）。

## 6. 网络层：要不要反向代理？（结论）

**一期（局域网/单机）：不需要外置反代。**
- os-api 8080 已是统一入口：API + `/git/*` Smart HTTP + 静态资源，token 鉴权全覆盖
- 大厅克隆走服务端 `git clone <source_url>`，客户端只与本机 os-api 通信
- 外置 nginx/caddy 只会增加部署负担，违背"单体二进制开箱即用"

**二期场景与对应方案（按需启用）：**
1. **远端不可达（NAT 穿透）** → 内置中继：os-api 用 reqwest 对 `/git/*` 做流式透传（"内置 git 反代"），复用 api_gateway 已有代理转发模式，不引入新组件
2. **公网多节点生产** → 外置 Caddy（自动 HTTPS + 域名 + 限流），给出 Caddyfile 模板纳入文档；后续可在 os-network 做"反代管理"UI
3. **离线场景** → BLE mesh 中继（与 IM 同通道，远期）

> PPT 表述：**"反代的本质是流量入口问题。NexOS 一期把入口收敛到单体 os-api（内聚优先）；跨网规模化了再把反代外置为可选件（弹性优先）。"**

### 6.1 关键升级：大厅一旦「公开 + 付费」，反代从可选变必需

原 §6 把反代定位为「二期按需启用」。但本次优化加入**货币化**（§10：免费 / 虚拟货币
BTC 付费），结论需收紧：

- **大厅变公开 + 付费的那一刻，反代（至少做 TLS 终止）从「可选」升级为「必需」**。
  理由：付费流程要传 `buyer` 身份、支付收据 `txid`、`NEXOS_ADMIN_TOKEN` 等敏感凭据；
  若仍走明文 `http://:8080`，这些凭据在公网被嗅探/重放，等于把收款地址和令牌裸奔。
- **最小反代 = Caddy（自动 HTTPS）**：一行 `reverse_proxy localhost:8080` + 自动签发
  证书，零代码改动；os-api 继续只监听内网 `127.0.0.1:8080`，公网流量全收口到 Caddy。
  模板见 `deploy/nexhub/Caddyfile`（本仓库新增）。
- **反代额外收益**：静态资源缓存（大厅卡片/README 摘要）、`/git/*` 大文件流式限速、
  全局限流防刷（付费克隆是成本动作，需防恶意刷量）、HTTP/2 多路复用。
- **仍不引入反代的场景**：纯局域网/单机、仅免费分享、无公网暴露——保持单体开箱即用。

> 一句话给 PPT：**"免费大厅可以裸奔在内网；付费大厅必须 TLS 收口——反代不是锦上添花，
> 是收款安全的底线。"**

## 7. 实施批次（串行子代理，防共享文件冲突）

| 批次 | 内容 | 产物 |
|---|---|---|
| 1 后端 | handlers/nexhub_lobby.rs + SQLite 表 + 6 API + ≥10 单测 | cargo test 全绿 |
| 2 前端 | CodeHub.vue 加大厅 Tab（卡片流/发布框/克隆按钮）+ i18n 若需 | npm build + cargo clean -p os-api && build 重嵌入 |
| 3 验收 | 真实端到端：发布→搜索→克隆→计数；虚拟桌面 GUI 验证 | 本文档"实施记录"章节 + 推送 nexos-local |

## 8. 实施记录（每批次完成后追加）

### 批次 1（后端）✅ 2026-08-14 完成

- **产物**：`crates/os-api/src/handlers/nexhub_lobby.rs`（新建约 1050 行）+ mod.rs/main.rs 注册（组件名 `nexhub-lobby`，28 组件 / 295 路由无冲突）
- **实现要点**：
  - SQLite `hub_lobby` 表 14 字段（含联邦预留 `homepage_node`），建库照抄 im.rs 惯例（WAL + /tank/os-data → /var/lib/os → cwd 三级降级 + Mutex 短锁）
  - 复用 code_repo 的 `repos_dir / build_clone_url / build_clone_url_http`（pub 导入，未改 code_repo.rs 一行）
  - 发布=快照：spawn git 统计元数据 + `git show HEAD:README.md` 取前 500 字摘要；重复发布 INSERT OR REPLACE 刷新快照且**保留 download_count**
  - 克隆：本机源直接注册；远端 `git clone --bare`（10s 超时 + kill_on_drop + GIT_TERMINAL_PROMPT=0），失败 502 不计数
  - Seed 幂等：表空且 nexos 仓库存在时自动发布（真机验证：533 commits、main、真实 README 摘要）→ **2026-08-17 起升级为默认常驻**：每次启动发布/刷新快照，见 §5 Seed 一节
- **测试**：19 个新增（真实 git fixture：2 commits+README），`cargo test -p os-api` **780 passed + 1 ignored / 0 failed**（连续 10 轮），`cargo build` 0 警告
- **已知事项**：code_repo 两个既有测试存在 `NEXOS_GIT_REPOS_DIR` env 竞态（原始代码可复现，非本批引入，未处理——遵守不改 code_repo.rs 约束）；`--check` 在 cwd 生成 hub_lobby.db（与 im.db 同降级行为）
- **curl 手册**：见本节末（验收用）
```bash
TOKEN='Authorization: Bearer <TOKEN>'; BASE=http://127.0.0.1:8080/api/v1/nexhub/lobby
curl -s "$BASE"                        # 列表（seed 含 nexos）
curl -s "$BASE?q=nexos&sort=downloads" # 搜索 + 排序
curl -s "$BASE/nexos"                  # 详情（readme + 双通道地址）
curl -s -X POST -H "$TOKEN" -H 'Content-Type: application/json' \
  -d '{"repo":"token-test2","description":"大厅演示","tags":["demo"],"publisher":"zcode"}' \
  "$BASE/publish"                      # 发布
curl -s -X POST -H "$TOKEN" "$BASE/token-test2/clone"   # 克隆到本地
curl -s -X DELETE -H "$TOKEN" "$BASE/token-test2"       # 下架
```

### 批次 2（前端）✅ 2026-08-14 完成

- **产物**：`CodeHub.vue` 新增第 5 Tab「🏛 大厅」（插在仓库列表后）+ `client.ts` 6 个 nexhubLobby* 方法
- **功能**：搜索/排序/标签云过滤工具条 · 统计条（发布数/总克隆）· 卡片网格（标签 chips/发布者/⬇下载/commit/大小/相对时间）· 懒加载详情（README 摘要 + SSH/HTTP 双通道地址一键复制）· 克隆到本地（spinner+禁用，区分"新克隆/已在本地注册"文案）· 下架 · 发布对话框（本地仓库下拉排除已发布、标签中文逗号兼容）
- **构建**：npm 0 TS 错误；rust-embed 重嵌入验证（二进制内 chunk 与 static-dist 一致）；780 测试保持全绿

### 批次 3（验收 + 计划外修复）✅ 2026-08-14 完成

**API 端到端（8/8）**：seed 列表（nexos, 533 commits）→ 关键词搜索 → 发布 token-test2（tags）→ 标签过滤 → 详情双通道地址 + README 摘要 → 克隆到本地 → download_count 计数与排序 → stats 聚合。

**GUI 验证（虚拟桌面 + CDP）**：大厅 Tab 渲染两张卡片信息完整、统计条与标签云正常、布局无错乱（VL 审查通过）；克隆按钮真实点击成功、发布对话框字段齐全。

**计划外发现并修复：全应用 401 缺口**
- 现象：GUI 克隆返回 401。根因：client.ts 无任何 Authorization 注入——服务端启用 NEXOS_ADMIN_TOKEN 后**所有 UI 写操作**（不限于大厅）都会失败，属全应用级遗留缺口
- 修复（子代理）：client.ts 集中 request() 注入 `Bearer`（localStorage key `os-api-token`，隐私模式降级内存）+ 401 统一友好提示；Settings.vue 新增「API 令牌」卡片（保存/清除/已配置 badge，token 仅存本浏览器）
- 复测：CDP 注入 token 后 GUI 克隆成功（"已在本地，直接注册"），计数正确累加

**踩坑记录**：① 虚拟桌面 workspace 守护进程会掉，`workspace start --ack-hidden-workspace` 重启；② Chrome 进程名是 `chrome` 不是 `google-chrome`（pkill -x 匹配不上会留僵尸实例，多实例导致 CDP 报 multiple apps，须按 PID 清）；③ Chrome 需 `--user-data-dir + --remote-debugging-port=0 + --disable-dev-shm-usage` 才稳定暴露 CDP，否则标签页 Aw Snap 崩溃。

## 10. 货币化（免费 / 虚拟货币 BTC）—— 本次优化

> 需求（用户原话）：「个人创建的项目可以分享到大厅，可以免费也可以设置虚拟货币，如 btc，
> 也可以从大厅下到本地的 nexhub。」

### 10.1 设计原则

- **免费是默认，付费是选项**：每个大厅条目带 `price_sats`（最小货币单位，0=免费）+ `currency`
  （`free`/`btc`/`nex`/`usdc`/`eth`；2026-09-02 二期增 `usdt`——EVM 链上经 ERC-20
  Transfer 日志核验，最小单位=微 USDT）。免费条目克隆零门禁，付费条目克隆前需授权。
- **价格与克隆解耦**：价格只决定「能否拿授权」，克隆动作本身不变（仍是服务端 `git clone --bare`）。
- **链上支付一期用「自证收据」，二期接 os-wallet 验真**：一期不引入 os-wallet 依赖
  （保持 os-nexhub 仅依赖 os-common），`verify_payment()` 校验「金额足额 + 货币一致 +
  txid 非空」即落库授权；二期把钩子换成 `os-wallet::ChainAdapter` 的真实链上验真
  （余额阈值 / BIP-322 签名挑战 / Ordinal 凭证）。
- **发布者豁免**：作者本人克隆自己的付费条目无需购买（避免「自己买自己」）。

### 10.2 数据模型增量

`hub_lobby` 表新增两列：`price_sats INTEGER DEFAULT 0`、`currency TEXT DEFAULT 'free'`。
存量旧库（14 列）升级：`create_schema()` 建表后以 `PRAGMA table_info(hub_lobby)`
探测缺列并 `ALTER TABLE ADD COLUMN` 幂等补齐（对照 16 列清单逐列核对，旧条目回填
`0`/`free`），避免「CREATE IF NOT EXISTS 对旧表 no-op → 列表静默清空 / 发布 500」。
新增 `hub_entitlement` 表（授权索引）：

```sql
CREATE TABLE hub_entitlement (
    repo_name   TEXT NOT NULL,
    buyer       TEXT NOT NULL,          -- 钱包地址 / 用户 id（与 os-wallet AddressId 对齐）
    chain       TEXT NOT NULL,          -- btc/nex/usdc/eth
    txid        TEXT NOT NULL,          -- 链上 txid / 收据指纹（一期自证）
    amount_sats INTEGER NOT NULL,       -- 实付（应 ≥ 条目 price_sats）
    currency    TEXT NOT NULL,
    paid_at     TEXT NOT NULL,
    PRIMARY KEY (repo_name, buyer)
);
```

### 10.3 API 增量（前缀 /api/v1/nexhub/lobby）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/:name/purchase` | 购买授权：body `{buyer, txid, chain?, amount_sats?, currency?}`；免费条目→400；金额不足/货币不符→402；成功落库 `hub_entitlement` |
| GET | `/entitlements` | 授权记录查询（任意已认证）：`?repo=<name>` 审计某条目全部买家、`?buyer=<b>` 自查购买记录，可组合、都不带则全量；按支付时间降序 |
| POST | `/:name/clone` | 货币化门禁：付费条目需携带 `buyer` 且已购（或 buyer==publisher），否则 `402 Payment Required` |

列表/详情响应现含 `price_sats` / `currency`，前端可直接渲染「免费 / 价格」徽标与购买按钮。

### 10.4 流程

```
作者 publish(paid, btc, 1000) ──▶ hub_lobby(price_sats=1000, currency=btc)
买家 列表看到价格徽标 ──▶ 点击购买
买家 POST /:name/purchase {buyer, txid, amount_sats:1000, currency:btc}
        └─ verify_payment() 足额+货币+txid ──▶ INSERT hub_entitlement(buyer)
买家 POST /:name/clone {buyer} ──▶ 门禁: find_entitlement(buyer) 命中 ──▶ git clone --bare + download_count+1
```

### 10.5 一期 vs 二期清单

- **一期（已落地）**：价格/货币字段、purchase 授权落库、clone 门禁 402、发布者豁免、
  `verify_payment` 自证收据。前端大厅卡片价格徽标 + 购买/克隆按钮（client.ts 已有
  `nexhubLobby*` 方法，扩展 purchase 即可）。
- **二期（钩子预留）**：`verify_payment_onchain()` 经 os-wallet 做 BTC/EVM 链上验真；
  收款地址配置（作者 `publisher` 绑定钱包地址，付款直打作者）；NEX 虚拟币内部账本清算；
  退款/纠纷；Star/评分与付费联动。

### 10.6 支付链上验真（dApp 一期 2026-08-31 落地 ✅ / 二期增强 2026-09-02 ✅）

> 原文 §10.1 的「一期自证收据」升级：`verify_payment`（自证面：金额足额 + 货币一致 +
> txid 非空）通过后，接力**真实 EVM RPC 核验**（`eth_getTransactionByHash` +
> `eth_getTransactionReceipt`，核验本体 `crates/os-nexhub/src/chain_verify.rs`，
> 业务接线 `nexhub_lobby.rs`「链上支付验真」段——`ChainPayGate`（env 定格 + 可注入
> 执行器接缝）+ `check_chain_payment`（编排 + 语义映射），os-api 网关 PaymentOrder
> confirm 复用同一套）。安全隐患台账 S1（付费门禁白嫖）就此缓解：**RPC 可用时不白嫖**。
>
> **二期增强（2026-09-02）两项**：① **ERC-20（USDT@EVM）Transfer 日志核验**（见下
> 触发条件 2'）；② **金额规则 `AmountRule`**——NexHub 购买保持 Exact 等值，悬赏
> approve 升级 AtLeast（见下金额规则表）。

**触发条件**（全部满足才核验；任一不满足 → 放行 + 响应标注 `chain_verify.status="unverified"`，不静默）：

1. `NEXOS_CHAIN_VERIFY_ENABLED≠0`（默认开；`0`=整体回旧行为——非空即过、无任何标注）；
2. 货币为 EVM native（`eth`；网关侧 `evm`）**或** usdt 且满足 2'（`btc`/`nex`/`usdc`
   仍自证——usdc 的 ERC-20 接入是后续项，当前只配了 USDT 合约 env）；
   2'. **usdt@EVM**（二期）：链 ID 可定位（= EVM 链；TRON 上的 USDT 定位不到 →
   人工通道）**且** ERC-20 合约可定位：body `erc20_contract` → env
   `NEXOS_USDT_EVM_CONTRACT`，都无则 Unverified（**不猜合约地址**）；小数位
   body `erc20_decimals` → env `NEXOS_USDT_EVM_DECIMALS`（默认 6）。核验路径：
   receipt `logs[]` 找 `address==合约 && topics[0]==Transfer 签名哈希 &&
   topics[2]==收款地址` 的日志，`data`（uint256）按**最小单位**对账（微 USDT）；
   无匹配日志 → Mismatch("erc20_log")。`from` 不校验；多笔同收款方 Transfer
   不合计（单笔独立满足金额规则）；
3. 链 ID 可定位：body `chain_id` → 数值 `chain` 串 → env `NEXOS_EVM_CHAIN_ID`；
4. 收款地址可定位：购买流 = env `NEXOS_HUB_PAY_TO`（节点运营者/条目 owner 配置；
   **不收 body 自报地址**——买家自指地址再自付是白嫖通道）；悬赏 approve = body
   `pay_to`（poster 提供 hunter 收款地址；**不回落 env 缺省**——节点地址会错杀发给
   hunter 的真支付）。

**结果语义表**（`VerifyOutcome` → 业务动作；native 单位 wei / ERC-20 单位 token
最小单位；`Verified` 恒携带链上**实付**，ERC-20 时另带 `token`=合约地址）：

| VerifyOutcome | 业务动作 | HTTP |
|---------------|----------|------|
| Verified | 放行；`block_number`/`value_wei` 落库 `hub_entitlement.chain_block/chain_value_wei`（审计口径：chain_block 有值 ⇒ 经真实 RPC 核验） | 200 |
| Pending（未上块） | 拒绝——**可重试语义，非欺诈**；稍后重试同一笔即可 | 409 |
| Mismatch（to/value/status/erc20_log/erc20_contract 不符） | 拒绝；错误信息带字段名与链上实际值，不落授权 | 409 |
| NotFound（链上无此交易） | 拒绝（txid 有误或被节点裁剪） | 400 |
| RpcError（RPC 不可达/超时/候选为空） | **降级放行** + `[chain-verify]` 日志警告（网络故障不阻断交易；S1 缓解 = RPC 可用时核验） | 200 |

**金额规则表**（dApp 二期定稿，2026-09-02；三处接线各自设置）：

| 业务线 | 规则 | 链上金额 vs 应付额 | 理由 |
|--------|------|--------------------|------|
| NexHub purchase（购买） | **Exact** | 必须相等（多/少都 Mismatch("value")） | 商品定价等值对账——买家须按应付额整额打款；`amount_sats` 自报与链上逐位比对 |
| bounty approve（悬赏放款） | **AtLeast** | ≥ 即过（不足 Mismatch("value")） | 与自证面「金额足额」（`verify_payment` 要求 ≥ 奖励）对齐——放款多打不亏待 hunter |
| 网关 confirm（充值） | **AtLeast** | ≥ 即过 | 充值多打不亏待用户（docs/GATEWAY_MONETIZATION.md 支付验真节同表） |

**RPC 来源链**（三段拼接为候选列表，按序 failover）：body 显式 `rpc_url`（admin/owner
自配）→ env `NEXOS_CHAIN_RPC_URLS`（JSON `{"<chain_id>": "<url>" 或 ["<url>",...]}`，
解析失败警告并忽略，不 panic）→ `fallback_rpc_for(chain_id)`（链预设公共 RPC 兜底）。

**金额单位与换算**：`eth` 条目 `price_sats`/`amount_sats` 沿用「最小货币单位」
语义 = **wei**（整数直传）；核验层 `to_wei_str` 兼容十进制小数串（如 `"0.02"` →
`"20000000000000000"`），按 **18 位小数**换算——EVM 主流链 native 币均为 18 位，
**非 18 位链不适用**（换算口径见 docs/GATEWAY_MONETIZATION.md「支付验真」限制清单）。
`usdt` 条目（二期）`= 微 USDT（10^-6）`：整数直传，带小数点按
`NEXOS_USDT_EVM_DECIMALS` 位换算（`to_min_unit_str` 通用函数，网关 usdt 订单的
`"10.00"` 即经它换成 `"10000000"`）。

**已知限制 / 信任模型**（明示以免误判覆盖范围；二期更新 2026-09-02）：

- body `rpc_url` / `erc20_contract` 由请求方提供（购买流=买家）：恶意买家可指向
  自建假 RPC 或自选代币合约伪造 Verified。缓解：链上事实（block/value/token 合约）
  落库可审计、admin 可复核 tx；生产建议仅经 env `NEXOS_CHAIN_RPC_URLS` /
  `NEXOS_USDT_EVM_CONTRACT` 固定可信值（body 字段仅 admin 通道使用）。
- 悬赏 approve 的 `pay_to` 由 poster 自报：可自付自证（平台核的是「真有一笔该金额
  的链上转账」，hunter 是否收到由其本人核对 tx）。
- ERC-20 路径技术限制：`from`（topics[1]）不校验（凭证不含付款方期望）；同一交易
  多笔同收款方 Transfer 不合计；日志 data 非 32 字节定宽/超 u128 → fail-closed。
- RPC 故障降级放行 = 有意权衡（可用性优先）；台账 S1 等级由高降为中（RPC 可用时
  不可白嫖，故障窗口恢复自证语义）。


## 11. 悬赏（bounty）—— 出资求活子资源

### 11.1 需求动机（用户原话拆解）

> 大厅除了可以有偿无偿分享项目，还可以**悬赏**，比如悬赏更新一些 GitHub 上停更的项目，
> 或其他想要做的东西。

货币化（§10）是「卖我的成果」；悬赏是「出钱求别人做某事」。二者共用同一套虚拟货币与
（一期自证 / 二期链上）支付机制，但语义相反：悬赏**必有奖励**（`reward_sats>0` 且
`currency` 为真实链），且存在生命周期状态机。典型场景：

- 悬赏「更新某停更 GitHub 仓库」（target_url 指向该 repo，hunter 提交 PR 链接）
- 悬赏「做一个小工具 / 修某个 bug / 写某文档」等任何可交付的成果

### 11.2 设计原则

- **奖励必须 >0**：无偿请求不算悬赏（`resolve_price` 解析后 `reward_sats==0` → 400）。
- **复用货币化支付校验**：验收时构造一条 `Entitlement` 式收据，复用 `verify_payment`
  （金额 ≥ 奖励、货币匹配、txid 非空）→ 不足额 402。一期自证，二期接 os-wallet。
- **状态机驱动**：`open → claimed → submitted → paid`，外加 `rejected→open`（重开）、
  `cancelled`（仅 open）。每次状态迁移写 `updated_at`。
- **无外置抓取**：`target_url` 仅作参考文本（如 GitHub 链接），服务端不主动拉取/校验。
- **不新增网关组件**：复用 `nexhub-lobby` 组件，新增 `/api/v1/nexhub/bounty/*` 路由，
  与大厅共享同一 SQLite 库（新表 `hub_bounty`）。
- **一期鉴权简化**：悬赏写操作（发布/认领/提交/验收/驳回/取消）仅要求「任意已认证用户」，
  不强制校验 caller 是否为 poster/hunter（身份来自网关 token，一期 handler 未读）；
  二期从 auth 上下文取 caller，强制 approve/cancel/reject 限 poster、claim/submit 限 hunter。

### 11.3 数据模型增量（新增 `hub_bounty` 表）

```sql
CREATE TABLE IF NOT EXISTS hub_bounty (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    description  TEXT DEFAULT '',
    tags         TEXT DEFAULT '[]',
    poster       TEXT DEFAULT '',
    reward_sats  INTEGER DEFAULT 0,
    currency     TEXT DEFAULT 'btc',
    target_url   TEXT DEFAULT '',
    status       TEXT DEFAULT 'open',
    claimed_by   TEXT DEFAULT '',
    solution_url TEXT DEFAULT '',
    deadline     TEXT DEFAULT '',
    created_at   TEXT,
    updated_at   TEXT,
    paid_at      TEXT DEFAULT '',
    payout_txid  TEXT DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_bounty_status ON hub_bounty(status);
```

`status` 取值：`open` / `claimed` / `submitted` / `paid` / `cancelled`。

### 11.4 API 增量（前缀 /api/v1/nexhub/bounty，共 8 条）

| method | path | 动作 | 状态迁移 |
|--------|------|------|----------|
| GET | `/api/v1/nexhub/bounty` | 列表（`?status=` `?q=`） | — |
| GET | `/api/v1/nexhub/bounty/:id` | 详情 | — |
| POST | `/api/v1/nexhub/bounty` | 发布悬赏（奖励必须 >0） | → open |
| POST | `/api/v1/nexhub/bounty/:id/claim` | hunter 认领 | open → claimed |
| POST | `/api/v1/nexhub/bounty/:id/submit` | hunter 提交交付物（solution_url） | open/claimed → submitted |
| POST | `/api/v1/nexhub/bounty/:id/approve` | poster 验收 + 自证支付（txid） | submitted → paid |
| POST | `/api/v1/nexhub/bounty/:id/reject` | poster 驳回 | submitted → open（清认领/交付） |
| POST | `/api/v1/nexhub/bounty/:id/cancel` | poster 取消 | open → cancelled |

读公开；写需任意已认证用户（一期）。关键约束：

- `submit`：`claimed` 状态须本人（`claimed_by==hunter`），否则 403；非 open/claimed → 409。
- `approve`：非 `submitted` → 409；支付不足/货币不符/空 txid → 402（复用 `verify_payment`）。
- `cancel`：非 `open` → 409（claim 后不可取消，需走 reject 或等 poster 处理）。
- `reject`：非 `submitted` → 409。

### 11.5 流程

```
poster ──POST /bounty─────────────────► hub_bounty(status=open, reward>0)
hunter ──POST /bounty/:id/claim────────► status=claimed (claimed_by=hunter)
hunter ──POST /bounty/:id/submit───────► status=submitted (solution_url=PR/repo)
poster ──POST /bounty/:id/approve──────► verify_payment(奖励,货币,txid)
                                        │  不足额 → 402
                                        └─► status=paid, paid_at, payout_txid（自证收据）
poster ──POST /bounty/:id/reject───────► status=open（清 claimed_by/solution_url，他人可再认领）
poster ──POST /bounty/:id/cancel───────► status=cancelled（仅 open）
```

### 11.6 一期 vs 二期清单

- **一期（已落地）**：`hub_bounty` 表、8 条路由、完整状态机、验收自证支付（复用
  `verify_payment`）、列表 `?status=`/`?q=` 过滤、51 个单测全绿。
- **二期（钩子预留）**：验收经 os-wallet 做链上**托管释放**（poster 预先锁定奖励到
  多签/HTLC，approve 触发释放，避免「验收后不付」）；从 auth token 取 caller 强制
  poster/hunter 身份；悬赏到期自动取消（deadline 字段已预留）；赏金榜/被采纳率等声誉。

### 11.7 已知限制（二期）

> 2026-08-15 审查追加：以下为一期**有意接受**的安全缺口，随二期身份体系收敛，
> 在此明示以免误判门禁覆盖范围。
>
> **2026-08-18 更新（链上身份上线，docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C）**：
> ①②的 handler 侧缺口**已修复（链上身份）**——身份 = secp256k1 公钥
> （`POST /api/v1/nexhub/auth/challenge|verify` 挑战-签名，token 24h 单点登录），
> 全部写端点的 owner/buyer/hunter/poster 由服务端从 token 反查 pubkey 填充，
> body 自报身份一律忽略；豁免判定改为 `buyer == 条目 owner pubkey` 的身份比对；
> `approve/reject/cancel` 锁 poster、`submit` 锁 claim 的 hunter、重发布/下架锁
> owner（越权 403）。仍开放项仅剩 `/git/*` 通道付费校验（见 1.b，二期）。

1. **付费门禁旁路（货币化 §10 与悬赏共用）**——部分修复（链上身份）
   - **publisher 豁免可冒用：已修复（链上身份）**。旧实现（2026-08-15 记录）：
     克隆门禁的发布者豁免是纯字符串比对（`buyer == entry.publisher`），publish
     不传 publisher 时缺省 `'local'`，clone 携带 `{"buyer": "local"}` 即可免购
     任意默认发布者的付费条目（publisher 名在大厅列表 API 公开可见，可逐条
     冒用）。现行实现：clone 的 buyer 一律取调用方 token 反查的 pubkey，豁免
     条件为 `buyer == 条目 owner pubkey`（发布时的 token 身份），body 自报
     buyer 不再参与判定；admin token 恒可（平台管理通道）。
   - **`/git/*` Smart HTTP 通道不受门禁约束：仍开放（二期）**。详情响应直接返回
     `clone_url_http=/git/<name>.git`；该通道（os-api `http.rs`）走
     `git_authenticate`，**只需 Bearer admin token，不校验购买状态/链上身份**。
     即付费门禁只保护大厅 clone 端点，底层 git 通道对持 admin token 者全开
     （单管理员 NAS 场景影响有限）。二期收敛方向：`/git/*` 按授权表
     （`hub_entitlement`，buyer=pubkey）+ 链上 token 校验。
2. **bounty poster/hunter 身份 body 自报：已修复（链上身份）**。旧实现（2026-08-15
   记录）：出资方/承接方身份全部来自请求体自报，任意已认证用户可 approve/
   reject/cancel 他人悬赏或伪造 txid 完成验收。现行实现：create 的 poster、
   claim/submit 的 hunter、approve/reject/cancel 的操作者全部从 token 反查
   （`approve/reject/cancel` 仅 poster；`submit` 仅 claim 的 hunter；越权 403）；
   admin token 回落通道保留——存量字符串 poster 的平台托管悬赏仍仅 admin 可
   操作。根由（契约 `ApiRequest` 无 auth 字段）不变，解法是 handler 内自带
   挑战-签名认证（共享内核 `os_common::chain_auth::ChainAuth`，与 IM 同款），
   与网关层 `requires_auth` 正交（写路由改为 false，身份闸门在 handler 内）。

## 9. 后续演进（不在本期）

- 联邦大厅：节点发现（os-discover）+ 定期拉取对等节点大厅索引 + 去重合并（homepage_node 字段已预留）
- 大厅评分/Star、作者主页、组织
- 推送通知（大厅新作品 → IM 系统广播，复用 WsMessage 广播通道）

## 12. 链上身份端点契约（/api/v1/nexhub/auth/*，2026-08-20 补记）

与 IM 同款三步挑战-签名（共享内核 `os_common::chain_auth::ChainAuth`，nexhub 与
IM 可共享同一密钥对——同一 pubkey 两处登录互不影响，见
docs/IM_BLOCKCHAIN_AUTH_DESIGN.md §2）：

| 步骤 | 端点 | 请求 | 响应 |
|------|------|------|------|
| 1 | `POST /api/v1/nexhub/auth/challenge` | `{pubkey}`（secp256k1 压缩格式 0x+66hex） | `{nonce}`（60s 单次有效） |
| 2 | 客户端签名 | 私钥对 nonce 的 UTF-8 字节做 ECDSA（65 字节 `r\|\|s\|\|v` hex） | — |
| 3 | `POST /api/v1/nexhub/auth/verify` | `{pubkey, nonce, signature}` | `{token}`（24h，单点登录：新 verify 顶掉旧 token） |
| 4 | 写端点调用 | `Authorization: Bearer <nexhub token>` | 服务端反查 pubkey 归因；无/无效 token 回落系统 admin token（`NEXOS_ADMIN_TOKEN`），再无 → 拒写 |

身份解析与权限矩阵（源码 `crates/os-nexhub/src/nexhub_lobby.rs` 模块头）：链上身份
publisher/poster/hunter/buyer 全部 = token 反查 pubkey（展示名派生 EVM 地址后 8 位）；
admin 回落仅保留平台托管通道（存量字符串 publisher 条目）。越权 403。

## 13. 环境变量（os-nexhub 全量，2026-08-20 从源码 grep 核实）

源码：`crates/os-nexhub/src/code_repo.rs`、`nexhub_lobby.rs`、`lib.rs` 模块头。
OS_ 前缀为旧名兼容（仅 NEXOS_ 未设置时生效）：

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_GIT_REPOS_DIR` / `OS_GIT_REPOS_DIR` | `/tank/git-repos` | 裸仓库根目录（`<name>.git` 的父目录；clone URL 与 `/git/*` CGI 共用） |
| `NEXOS_GIT_USER` / `OS_GIT_USER` | `oem` | SSH clone URL 中的用户名 |
| `NEXOS_GIT_HOST` / `OS_GIT_HOST` | `hostname` 命令输出（回退 `localhost`） | clone URL 主机名（进程内 OnceLock 缓存） |
| `NEXOS_HTTP_PORT` / `OS_HTTP_PORT` | `8080` | HTTP Smart Git clone URL 端口（与 os-api main.rs `--addr` 默认值一致） |
| `NEXOS_LOBBY_NO_AUTO_PUBLISH` | 未设置 | 设 `1` 跳过 nexos 主仓库的启动自动常驻发布（用户显式下架后不想被重启拉回，见 §5 Seed） |
| `NEXOS_LOBBY_SYNC_API` | `http://127.0.0.1:8558` | post-receive 自动同步钩子 curl 的 os-api 基址（§15；容器/自定义端口部署覆盖） |
| `NEXOS_ADMIN_TOKEN` / `OS_ADMIN_TOKEN` | 未设置 | 链上 token 之外的 admin 回落通道（与 os-api 网关同一变量；未设置则仅链上身份可写；钩子生成时回落默认 `change-me-admin-token`，§15） |
| `NEXOS_CHAIN_VERIFY_ENABLED` | `1` | 链上支付验真总开关（§10.6）：`0`/`false`/`off` = 整体回旧行为（txid 非空即过，响应无标注） |
| `NEXOS_CHAIN_RPC_URLS` | （空） | 节点级 RPC 预设（§10.6）：JSON `{"<chain_id>": "<url>" 或 ["<url>",...]}`；坏值警告并忽略，不 panic |
| `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` | `10` | 单次核验 RPC 超时（§10.6；下限 1s，非法值回默认并警告） |
| `NEXOS_EVM_CHAIN_ID` | （无） | EVM 支付缺省链 ID（§10.6；NexHub 购买/悬赏与网关 PaymentOrder confirm 共用；body `chain_id`/数值 `chain` 优先） |
| `NEXOS_HUB_PAY_TO` | （无） | NexHub 购买流缺省 EVM 收款地址（§10.6；节点运营者/条目 owner 配置；悬赏 approve 不回落此值） |
| `NEXOS_USDT_EVM_CONTRACT` | （无） | USDT@EVM 的 ERC-20 合约地址（§10.6 二期；body `erc20_contract` 优先；都无则不核不猜） |
| `NEXOS_USDT_EVM_DECIMALS` | `6` | USDT 小数位（§10.6 二期；body `erc20_decimals` 优先；非法值警告回默认，上限 36） |

DB 路径（非 env，三级回退）：`/tank/os-data/hub_lobby.db` → `/var/lib/os/hub_lobby.db`
→ `./hub_lobby.db`；外部依赖：PATH 上需有系统 `git`。

---

## 14. 联邦大厅（P3，2026-08-22：跨 NexOS 节点项目发现）

设计来源 `docs/NEXOS_P2P_NETWORK_DESIGN.md` §8。开启条件：两侧节点
`NEXOS_P2P_ENABLE=1` 且组网连通；未启用时发布/接收静默停用，单机语义零变化。

### 14.1 协议与流转

```json
// 出站（POST /publish 成功且 owner_kind=pubkey 才广播，admin 字符串条目不联邦）
{"fed": "nexhub_lobby", "node": "node-106", "entry": { ...完整 LobbyEntry JSON... }}
// 入站（对端 FederationBridge 分发 → LobbyFedEndpoint.ingest）
//   去重（repo+node 内存缓存 + DB 权威判定）→ 写本地 hub_lobby
//   （source_node=发布节点；本地 download_count 不带入，从 0 起步）
```

### 14.2 去重与防护（ingest 语义）

| 场景 | 处置 |
|---|---|
| 库中无同名条目 | `Written`——写入 + `source_node` 标记来源 |
| 同 `repo_name+source_node` 重发（对端刷新快照） | `Refreshed`——覆盖刷新但**保留本地 download_count** |
| 同名但来源不同（本地/他节点先到） | `Skipped`——**本地/首到条目受保护**，不覆盖 |
| 内存缓存命中（同 repo+node 短时重收） | `Duplicate`——不触碰 DB |
| 载荷非法（非 nexhub_lobby / 缺 node/entry / repo_name 非法） | `Invalid`——零写入（repo_name 走与本地发布同款校验，防路径穿越） |

### 14.3 远程条目的克隆（2026-08-25 修复：跨节点拉取走 HTTP）

`source_node != 'local'` 的条目，`POST /:name/clone` 的克隆源选择
（`select_clone_source` 纯函数）：

- 条目属本机（source_node/homepage_node=local，或 source_url 恰为本机存在路径）
  → 现行 `source_url` 路径 spawn `git clone --bare`（10s 超时），行为不变；
- **联邦条目 → 条目自带的 `clone_url_http`（发布节点定格的 `/git/*` Smart HTTP
  地址，匿名读）从源节点 HTTP 拉取（120s 超时）**——修复前误用 source_url，
  而那是源节点本机路径（如 113 克隆 `/tank/git-repos/nexos.git` 报
  "repository does not exist"——该路径只在 106 存在）；
- 两者皆不可用才 502，错误信息区分「本机路径不存在 / 源节点不可达」；历史
  条目（无 clone_url_http 或 URL 仍是旧主机名格式）失败时提示**源节点需重
  publish 刷新地址**（publish/常驻刷新即定格新地址并随联邦重广播）。

响应额外带 `source_node` + 提示 note；前端 CodeHub.vue 大厅行显示 `🌐 来自 <node>`
徽章，克隆按钮 title 提示"将从远程节点拉取"。

### 14.4 依赖方向（独立性红线）

os-nexhub **不依赖 os-p2p**（审计 §6）——发送通道抽象为 trait
`LobbyFedTransport { fn broadcast(&self, payload) }`，os-api 装配层注入实现
`P2pLobbyTransport(Handle)`（`crates/os-api/src/handlers/p2p.rs`）；接收端
`LobbyFedEndpoint`（与 handler 共享同一 `Arc<Mutex<Connection>>`）由 main.rs
在 handler Box 进网关前经 `fed_endpoint()` 取出交给 FederationBridge。

### 14.5 测试（10 新增，os-nexhub 85 全绿）

pubkey 发布广播载荷 / admin 条目不广播 / 无通道静默 201 / ingest 写入 +
source_node / 同名同节点去重 / 本地条目保护 / 同源刷新保留计数 / 非法载荷
拒绝 / source_node 列迁移（旧 16 列库）/ 纯函数（载荷构造 + 节点名净化）。
另有 os-api p2p.rs 的双节点端到端（B 广播条目 → A 落地 source_node 标记）。

---

## 15. 大厅条目自动同步最新提交（2026-08-25：nexos 条目不再停留在发布时快照）

### 15.1 问题与目标

nexos 是联邦大厅的第一个条目（启动 `ensure_nexos_published` 自动发布 + 联邦
广播），但条目的 commit 数/最新提交/README 摘要是**发布时的快照**——106 节点
推了新提交后，本地与联邦条目都不会自动更新，需手动重发 publish。系统自举
（新节点从联邦大厅拿系统代码）依赖条目反映真实最新状态，故补全自动同步链。

### 15.2 自动同步链路

```text
git push（如 106 推 nexos.git 新提交）
  │
  ▼
<tanks>/git-repos/nexos.git/hooks/post-receive        ← os-nexhub lobby_sync_hook 生成/补装
  │  （后台执行，绝不阻塞 git push；恒 exit 0）
  ▼
curl POST http://127.0.0.1:8558/api/v1/nexhub/lobby/publish      （admin token）
  │   重取仓库快照：commit_count / latest_commit（短 hash+subject+作者+时间，
  │   git log -1 经 code_repo::parse_git_log 解析）/ README 摘要 /
  │   pushed_at=本次刷新时间；INSERT OR REPLACE 保留 download_count
  ▼
curl POST http://127.0.0.1:8558/api/v1/nexhub/lobby/nexos/federate（admin token）
  │   置 federated=true + broadcast_entry 重广播最新快照
  ▼
联邦各节点 LobbyFedEndpoint.ingest
  │   按 name 幂等合并：同 repo_name+source_node → Refreshed
  │   （字段更新为最新快照，条目不重复，本地克隆计数保留）
  ▼
各节点联邦大厅条目 = 仓库真实最新状态（新节点自举即看到）
```

publish 先于 federate（federate 广播的是本地条目最新快照，不先 publish 广播的
还是旧快照）；既有两步联邦语义不变（publish 仍只写本地、不自动广播）。

### 15.3 post-receive 钩子安装

- **自动补装**：os-api 启动的 ensure 流程（`ensure_nexos_published`，每次启动
  执行）顺手补装 `<NEXOS_GIT_REPOS_DIR>/nexos.git/hooks/post-receive`——任何
  部署形态（systemd/docker/手动）启动即获得自动同步能力，无需人工。
- **幂等规则**（`lobby_sync_hook::ensure_post_receive_hook`）：
  - 钩子缺失 → 写入生成脚本 + chmod 755；
  - 已装且与生成器当前产物逐字节一致 → no-op（重复安装内容恒一致）；
  - 是生成器旧产物（含 `# nexhub-lobby-auto-sync v1` 标记）但内容漂移
    （地址/token 变更）→ 覆盖为新产物；
  - 不含标记（用户自管钩子）→ 一律不动；
  - 失败仅记日志不阻塞启动（降级 = 自动同步退化为启动时刷新）。
- **手工为其他仓库安装**（任何裸仓库都可用同一机制）：

  ```sh
  # 生成脚本（repo 换成目标仓库名）后放 <repos>/<repo>.git/hooks/post-receive + chmod 755
  # 或直接调用库函数：os_nexhub::ensure_post_receive_hook(repos, repo, api, token)
  ```

- **环境变量**：

  | 变量 | 默认 | 作用 |
  |------|------|------|
  | `NEXOS_LOBBY_SYNC_API` | `http://127.0.0.1:8558` | 钩子 curl 的 os-api 基址（容器/自定义端口部署覆盖） |
  | `NEXOS_ADMIN_TOKEN` / `OS_ADMIN_TOKEN` | `change-me-admin-token` | 钩子携带的 admin token（**生产必须设置真实值**；脚本内注释同标注） |

- **性能红线**：钩子绝不阻塞 git push——curl 带 `-m 5` 硬超时且整体
  `( … ) &` 后台子 shell 执行（实测 push 往返 ~10ms 量级）。

### 15.4 快照字段增量（hub_lobby 20 列）

| 新列 | 形状 | 语义 |
|------|------|------|
| `latest_commit` | JSON：`{short_hash(7位), subject, author, date}` | 结构化最新提交（比既有 `last_commit` 拼接串多作者维度；发布/刷新即重取） |
| `pushed_at` | RFC3339 | 最近一次快照刷新时间（publish/常驻刷新/钩子触发重发布均更新；前端可按「最近有活力」排序） |

旧库自动补列（`migrate_hub_lobby_columns`，NULL→`latest_commit=None`/
`pushed_at=""`）；联邦 payload 缺字段经 serde default 兼容旧节点。

### 15.5 与系统自举的依赖关系

新节点加入联邦后从大厅拿系统代码（`POST /:name/clone` 走 nexos 条目）——自举
**依赖条目反映真实最新状态**：commit 数/最新提交摘要是新节点判断「拿到的系统
代码是否新鲜」的依据。自动同步链保证：主仓库每次 push，联邦大厅各节点的 nexos
条目即时（下一次 push 起）刷新为最新快照，自举不再看到过期状态。

### 15.6 测试（6 新增，os-nexhub 117 全绿）

publish 快照字段（latest_commit 形状/pushed_at RFC3339 递增）/ 启动 ensure 补装
钩子（env 推导地址+token、幂等、逃生口一并跳过）/ 钩子脚本生成纯函数（契约要素：
双端点/认证头/超时/后台/恒成功/引号转义）/ 补装幂等（装两次内容一致、用户自管
不动、漂移覆盖）/ 联邦消费端按 name 合并（旧快照→新快照条目仍 1 条、字段最新）/
真实 git push 端到端（临时裸仓 + 本地 sink HTTP 服务：钩子触发 publish+federate
两请求、push 不被阻塞）。
