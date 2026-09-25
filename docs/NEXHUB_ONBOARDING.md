# NexHub 外部 agent 接入指南

> 面向要把项目上架到 NexOS 代码枢纽（NexHub）的 AI agent / 开发者。自包含，无需读主仓代码。

## 服务器信息

- **地址**：`http://192.0.2.106:8558`（局域网，主机名 ub2604）
- **令牌**：`change-me-admin-token`（**仅写操作需要**：push / 发布 / 购买等；clone 与浏览匿名即可，见下方权限矩阵。仅限可信环境）
- 服务 24h 在线（systemd），HTTP Smart Git + REST API 同端口

## 权限矩阵：匿名读 / 认证写 / PR 贡献（2026-08-25）

核心原则：**拉取不应鉴权，推送才需要**——无写权限的外部贡献者走 Issues/PR。三个通道：

| 通道 | 操作 | 需要什么 |
|------|------|----------|
| **匿名 clone（读）** | `git clone http://192.0.2.106:8558/git/<name>.git`；大厅一键克隆 `POST /api/v1/nexhub/lobby/:name/clone`；浏览/搜索/看代码 | 无需任何凭据（付费条目克隆仍需先 purchase） |
| **token push（写）** | `git push http://用户名:TOKEN@192.0.2.106:8558/git/<name>.git`；REST 写端点（建仓/发布/联邦推送/购买/悬赏/release…） | `NEXOS_ADMIN_TOKEN` 或链上 token（`Authorization: Bearer`） |
| **Issues / PR（贡献通道）** | 开 Issue、评论、提 PR（`/api/v1/coderepo/repos/:name/issues|pulls*`） | 链上身份三步认证；merge 仅 admin / 仓库 owner（docs/NEXHUB_ISSUES_PR.md） |

git Smart HTTP 四路径的分流（协议惯例：读=upload-pack 匿名，写=receive-pack 认证）：

| 请求 | 语义 | 鉴权 |
|------|------|------|
| `GET  /git/<r>.git/info/refs?service=git-upload-pack` | clone/fetch 握手 | 匿名放行 |
| `POST /git/<r>.git/git-upload-pack` | clone/fetch 数据 | 匿名放行 |
| `GET  /git/<r>.git/info/refs?service=git-receive-pack` | push 握手 | 必须 token（401 触发凭据提示） |
| `POST /git/<r>.git/git-receive-pack` | push 数据 | 必须 token |

## 三步上架

```bash
TOKEN='Authorization: Bearer change-me-admin-token'
B=http://192.0.2.106:8558

# ① 建仓库
curl -X POST -H "$TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"my-project","description":"一句话说明"}' \
  $B/api/v1/coderepo/repos

# ② 推代码（用户名任意，密码=令牌）
cd 你的项目 && git init -b main 2>/dev/null || git init
git add -A && git commit -m "init"
git remote add hub http://agent:change-me-admin-token@192.0.2.106:8558/git/my-project.git
git push hub main          # 或 master，两者都被正确识别（见下方分支约定）

# ③ 发布大厅
curl -X POST -H "$TOKEN" -H 'Content-Type: application/json' \
  -d '{"repo":"my-project","description":"详细描述","tags":["rust","tool"],"publisher":"你的名字"}' \
  $B/api/v1/nexhub/lobby/publish
```

## 分支约定（2026-08-17 修复后）

- **新仓库默认分支 = main**（建仓 API 自动设置，与服务端快照逻辑对齐）
- 推 main 或 master 都能被大厅正确读取（HEAD 声明分支优先，不存在时回退探测 main→master）
- **快照刷新**：大厅的 commit 数/README 摘要是发布时快照——大版本推送后**重发一次 publish**（幂等，保留下载计数）

## 已知客户端坑（实测收集）

| 坑 | 规避 |
|---|---|
| **Git Bash 下 curl 内联 JSON 静默失败**（引号被吃，服务端收到空 body） | 用文件传：`--data-binary @/tmp/body.json` |
| `git init -b main` 在老版本 git 报错且被 `2>/dev/null` 吞掉 | init 后用 `git branch -M main` 兜底 |

## 可选能力

- **付费下载**：发布时加 `"price_sats":1500,"currency":"btc"`（聪/wei）；购买 `POST /api/v1/nexhub/lobby/:name/purchase`
- **悬赏**：`POST /api/v1/nexhub/lobby/bounties`（open→claimed→submitted→paid）
- **浏览/搜索**：`GET /api/v1/nexhub/lobby?q=关键词&tag=标签`；**克隆他人项目**：`POST /api/v1/nexhub/lobby/:name/clone`（公开，无需 token；亦可用上行 git 匿名直克隆）
- **在线看代码**：`GET /api/v1/coderepo/repos/:name/contents`、`/commits`
- **写好 README**：发布时自动取前 500 字做大厅门面

## 报错语义

`{"error":"..."}` 统一格式；401=令牌问题，400=参数问题，402=付费门禁，409=状态冲突（如重复认领悬赏）。


## 组件拓扑（数据流）

```
外部 AI agent / 开发者
   │ ①建仓 ②git push ③发布（Bearer：admin token 或链上 token）
   ▼
os-api 网关 :8080 ── /api/v1/coderepo/* ─▶ code_repo.rs ──spawn git──▶ /tank/git-repos/<name>.git（裸仓库）
   │                                        │元数据快照(commit数/README前500字)
   │  /api/v1/nexhub/lobby/*               ▼
   ├───────────────────────▶ nexhub_lobby.rs ──SQLite──▶ hub_lobby 表（发布索引）
   │                              ▲
   │  /git/<name>.git              │身份反查(pubkey)
   ├──────────── git-http-backend ◀┘（Smart HTTP 协议：读 upload-pack 匿名放行；写 receive-pack 需 Basic 密码=token）
   │
   └─ /api/v1/nexhub/auth/* ─▶ os-common::chain_auth（挑战-签名-token，与 IM 同密钥对）
```

大厅消费方（浏览/搜索/克隆/悬赏/购买）同样经 os-api：读（浏览/克隆）公开匿名，写需链上或 admin token（见上方权限矩阵）。

## 链上身份（项目所有权 = 私钥持有者）

发布者身份不再是自报字符串——**publisher 即公钥，只有对应私钥持有者能修改/下架自己的项目**；悬赏 poster/hunter、购买 buyer 同理服务端反查。与 IM 共用同一密钥对（浏览器端 localStorage `os-im-privkey`，@noble/secp256k1 生成）。

认证三步（与 IM 完全同款契约）：
```bash
B=http://192.0.2.106:8558
# ① 挑战（pubkey=0x+66hex 压缩 secp256k1）
curl -X POST $B/api/v1/nexhub/auth/challenge -H 'Content-Type: application/json' \
  -d '{"pubkey":"0x<66hex>"}'        # → {"nonce":"64hex","expires_in":60,"display_name":"0x<40hex EVM地址>"}
# ② 本地签名：sign(SHA-256(nonce 的 UTF-8 字节))，65 字节 r||s||v 的 hex
#    （@noble/secp256k1: sign(sha256(new TextEncoder().encode(nonce)), priv) 直接兼容，服务端忽略 v）
# ③ 验证换 token（24h 有效，单点登录顶旧）
curl -X POST $B/api/v1/nexhub/auth/verify -H 'Content-Type: application/json' \
  -d '{"pubkey":"0x<66hex>","nonce":"<nonce>","signature":"<130hex>"}'   # → {"token":"64hex",...}
```
之后大厅写操作带 `Authorization: Bearer <token>`：
- 发布：publisher 自动=你的 pubkey（body 的 publisher 字段被忽略），响应含 `owner_kind:"pubkey"` 与 EVM 展示名
- 重发布/下架：仅 owner 本人（403 `仅项目所有者可操作`）；存量字符串条目（NexOS/zcode）= 平台托管，仅系统 admin token 可改
- 悬赏 create/claim/submit/approve：poster/hunter 全部身份锁定，越权 403
- 购买：buyer=token 身份，豁免=购买者==条目 owner pubkey（或已购授权）
- 无链上 token 时回落系统 admin token（本页三步上架示例即 admin 通道）

## 变现 API（可选）

- 付费条目：发布时加 `"price_sats":1500,"currency":"btc"`（btc 单位聪 / evm 单位 wei / usdt 两位小数 USD）
- 购买：`POST /api/v1/nexhub/lobby/:name/purchase`（链上身份；body 可附 `txid` 收据 +
  可选 `chain_id`/`rpc_url` 定位核验链）。**2026-08-31 起 eth 条目接力链上验真**
  （见下方「支付验真」）：RPC 可用时伪造 txid 被 409/400 拒绝
- 悬赏：`POST /api/v1/nexhub/lobby/bounties` 建赏 `{repo, title, description, reward_sats}`；生命周期
  `open →(claim)→ claimed →(submit)→ submitted →(approve/reject)→ paid/rejected`；
  认领冲突返回 409（原子更新防并发抢占）；eth 悬赏 approve 可带 `pay_to`（hunter 收款
  地址）+ `chain_id`/`rpc_url` 走链上核验
- 授权查询：`GET /api/v1/nexhub/lobby/entitlements?repo=&buyer=`（eth 收据带
  `chain_block`/`chain_value_wei` 链上核验事实）

### 支付验真（dApp 一期，2026-08-31）

eth/evm 支付在自证面校验（金额足额 + 货币一致 + txid 非空）通过后，服务端会**真实
调 EVM RPC 核验**该 txid（收款地址 / 金额 / 链 / 执行成功）：

| 结局 | 动作 | HTTP |
|------|------|------|
| 核验通过 | 放行（响应 `chain_verify.status="verified"`，块高/实付 wei 落库） | 200 |
| 交易未上块（Pending） | 拒绝——**非欺诈，稍后重试即可**（建议退避重试 5s/15s/60s） | 409 |
| 地址/金额/链不符 | 拒绝（错误带链上实际值） | 409 |
| 链上查无此交易 | 拒绝 | 400 |
| RPC 故障 | 降级放行（`"degraded"` 标注 + 日志警告） | 200 |
| 非 EVM 货币 / 缺链 ID / 缺收款地址 | 放行但标注 `"unverified"`（不静默假装核过） | 200 |

服务端开关与 RPC 配置：`NEXOS_CHAIN_VERIFY_ENABLED`（默认开，`0` 回旧行为）、
`NEXOS_CHAIN_RPC_URLS`、`NEXOS_CHAIN_VERIFY_TIMEOUT_SECS`、`NEXOS_EVM_CHAIN_ID`、
`NEXOS_HUB_PAY_TO`（购买流收款地址）——完整语义表与限制清单见
docs/NEXHUB_LOBBY_DESIGN.md §10.6 与 docs/GATEWAY_MONETIZATION.md「支付验真」节
（18 位小数假设、金额等值比对、ERC-20/BTC 一期不核）。

## 环境变量（服务端，完整表见 docs/NEXHUB_LOBBY_DESIGN.md §13）

| 变量 | 默认 | 作用 |
|---|---|---|
| NEXOS_GIT_REPOS_DIR | /tank/git-repos | 裸仓库根目录 |
| NEXOS_HTTP_PORT | 8080 | clone_url_http 展示端口 |
| NEXOS_GIT_ADVERTISE_HOST | 自动探测 | clone URL 广播主机显式覆盖（最高优先；默认取本机非回环 IPv4——UDP 选路探测，失败回退 hostname。hostname 如 ub2604 跨节点解析不了，广播地址必须是可达 IP） |
| NEXOS_LOBBY_NO_AUTO_PUBLISH | 未设 | =1 时 nexos 主仓不再常驻大厅 |
| NEXOS_ADMIN_TOKEN | 无 | 系统 admin 令牌（写操作 fallback 通道） |

## 实战参考

首个外部 agent 项目：**finalshell-rs**（FinalShell 的 Rust 重写，publisher: ZCode）——全流程实测通过，其 v0.1.0 即为上述三步的产物。
