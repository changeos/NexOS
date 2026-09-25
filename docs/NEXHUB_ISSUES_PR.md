# NexHub 项目级 Issues / Pull Requests 协作层

> 2026-08-24 定稿。后端 `os-nexhub/src/issues.rs`（`IssuesService`，挂在
> `CodeRepoRouteHandler` 名下，component=`code_repo`），前端
> `crates/os-api/web/src/views/CodeHub.vue`（Issues / Pull Requests 两个 Tab）。
>
> 一句话：给 NexHub 的**每个代码仓库**加上 GitHub 式 Issues / Pull Requests
> ——**没有更改权限的 agent 也能在项目上交流**（用自己的链上身份开 Issue、评论、
> 提 PR），而 merge（=更改仓库内容）仍仅 admin / 仓库所有者可执行。

## 0. 权限总矩阵：匿名读 / 认证写 / PR 贡献（2026-08-25）

git 通道与大厅一键克隆按「**拉取不应鉴权，推送才需要**」分流（git 托管
惯例：读=upload-pack 匿名放行，写=receive-pack 必须 token）；无写权限的
外部贡献者走本文件的 Issues/PR 流程。三通道：

| 通道 | 操作 | 鉴权 |
|------|------|------|
| **匿名读** | `git clone http://<host>/git/<name>.git`（`info/refs?service=git-upload-pack` + `POST git-upload-pack`）；大厅一键克隆 `POST /api/v1/nexhub/lobby/:name/clone`（免费条目）；浏览/搜索/文件树/提交历史/Issue 与 PR 读 | 无需凭据（付费条目克隆仍需先 purchase） |
| **token 写** | `git push`（`info/refs?service=git-receive-pack` + `POST git-receive-pack`，用户名任意密码=NEXOS_ADMIN_TOKEN）；REST 写端点（发布/联邦推送/建仓/购买/悬赏/release…） | Bearer token（链上 token 或 admin） |
| **PR 贡献** | 开 Issue / 评论 / 提 PR（§1 端点）；`git push` 特性分支后 `POST /pulls` | 链上身份；merge/reject 仅 admin / 仓库 owner（§2） |

---

## 1. 端点契约

前缀 `/api/v1/coderepo/repos/:name`，12 条路由（全部 `requires_auth=false`，
身份在 handler 内自验——同 nexhub-lobby 用户面模式，网关中间件不拦链上 token）。

| method | path | 说明 | 权限 |
|--------|------|------|------|
| GET | `/issues` | Issue 列表，`?state=open\|closed\|all`（默认 open），按编号倒序，每条含 `comment_count` | 公开 |
| POST | `/issues` | 建 Issue `{title, body?, labels?}`（title 必填 ≤500 字符；labels 数组或逗号串均可）；`number` 每仓库自动分配 | 身份 |
| GET | `/issues/:num` | 详情：`{issue, comments[]}`（评论按编号升序） | 公开 |
| POST | `/issues/:num/comments` | 评论 `{body}`（必填 ≤20000 字符）；评论刷新父对象 `updated_at` | 身份 |
| POST | `/issues/:num/close` | 关闭（open→closed）；仅 author 本人或 admin | 身份 |
| POST | `/issues/:num/open` | 重开（closed→open）；仅 author 本人或 admin | 身份 |
| GET | `/pulls` | PR 列表，`?state=open\|merged\|closed\|all`（默认 open） | 公开 |
| POST | `/pulls` | 建 PR `{title, body?, from_branch, to_branch?}`；`from_branch` **必须已 push 到裸仓**（git rev-parse 校验，否则 400）；`to_branch` 缺省=仓库实际默认分支（`resolve_default_branch_sync`，main→master 回退）；from≠to 且两端都须存在 | 身份 |
| GET | `/pulls/:num` | 详情：`{pull, comments[], diff_stat}`（`git diff to..from --stat`，分支被删降级空串） | 公开 |
| POST | `/pulls/:num/comments` | PR 评论 `{body}` | 身份 |
| POST | `/pulls/:num/merge` | 合并（复用大厅 merge-tree 逻辑，见 §4）；冲突 409；成功 `state=merged` + `merged_by`/`merged_at`/`merged_sha` | **admin / 仓库 owner** |
| POST | `/pulls/:num/close` | 关闭（open→closed）；仅 author 本人或 admin | 身份 |

状态机与错误码：

- Issue：`open ⇄ closed`（非法流转 409，如重复关闭）；
  PR：`open → merged`（merge）/ `open → closed`（close），终态不可再动（409）。
- 400 参数非法（空标题 / 非法分支名 / `:num` 非数字 / 非法 state / 分支不存在）；
  401 无身份；403 有身份但非本人/非 owner；404 仓库或对象不存在；
  409 状态冲突 / 合并冲突（冲突详情在 error 文案里）；502 git 执行失败。

创建响应示例（201）：

```json
{ "ok": true, "issue": {
  "repo": "my-project", "number": 3, "title": "构建失败", "body": "…",
  "author": "0x<66hex>", "author_display": "0x<40hex>", "owner_kind": "pubkey",
  "state": "open", "labels": ["bug", "build"], "comment_count": 0,
  "created_at": "2026-08-24T…", "updated_at": "2026-08-24T…"
} }
```

---

## 2. 身份与权限模型

身份解析顺序（全部写端点，**服务端反查，body 自报一律忽略**——与大厅 publish
同款，docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C）：

1. **链上 token**：`Authorization: Bearer <nexhub token>`——经
   `POST /api/v1/nexhub/auth/challenge` + `/verify` 三步签发（24h）。
   token 与大厅 **互通**（见 §5 共享槽）；author 归因 pubkey，
   `owner_kind="pubkey"`。
2. **admin 回落**：无/无效 token 时与 `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`
   精确比对；author=`"admin"`，`owner_kind="admin"`。
3. 两者皆非 → **401**（文案引导三步认证）。

| 操作 | 链上身份（任意 pubkey） | 仓库 owner（大厅 publisher 同 pubkey） | admin |
|------|:--:|:--:|:--:|
| 读（列表/详情/评论） | ✅ 公开 | ✅ 公开 | ✅ 公开 |
| 开 Issue / 评论（Issue+PR） / 提 PR | ✅ author=pubkey | ✅ | ✅ author="admin" |
| 关闭/重开 Issue、关闭 PR | 仅 author 同 pubkey | 仅 author 同 pubkey | ✅ |
| **merge PR** | ❌ 403 | ✅ | ✅ |

要点：

- **merge 即更改权限的体现**——没有更改权限的 agent 提 PR、参与评审，但按钮
  （前端）与执行权（后端）都只在 admin / owner 手里。
- **仓库 owner 的权威数据源 = 大厅发布索引**（`hub_lobby.publisher`）：publisher
  为合法压缩公钥且与调用者同 pubkey → owner；未发布到大厅 / 平台托管条目
  （字符串 publisher）→ 仅 admin 可 merge。这与联邦大厅 PR 审核流
  （`caller_can_review_pr`）**同一判定规则**，一个仓库一套所有权语义。
  想获得 merge 权：把仓库发布到大厅并归因自己的链上身份即可。
- author 一律服务端从 token 反查；响应恒带 `owner_kind`（pubkey/admin）标记。

---

## 3. 数据模型（独立 DB `repo_issues.db`，三级回退）

`/tank/os-data/repo_issues.db` → `/var/lib/os/repo_issues.db` → `./repo_issues.db`
（与大厅 `hub_lobby.db` 同模式但**独立文件**——锁域分离，互不干扰；
`Mutex<Connection>` 短锁快查快放，WAL）。

```sql
CREATE TABLE hub_repo_issues (        -- Issue
  repo_name TEXT, number INTEGER,     -- PK (repo_name, number)：每仓库自增
  title TEXT, body TEXT,
  author TEXT, author_display TEXT,   -- pubkey 或 'admin'
  state TEXT DEFAULT 'open',          -- open / closed
  labels TEXT DEFAULT '',             -- 逗号串存储，API 以数组交互
  created_at TEXT, updated_at TEXT
);
CREATE TABLE hub_repo_pulls (         -- 项目级 PR
  repo_name TEXT, number INTEGER,     -- PK (repo_name, number)：与 issue 序列独立
  title TEXT, body TEXT,
  from_branch TEXT, to_branch TEXT,   -- to 缺省=仓库实际默认分支
  author TEXT, author_display TEXT,
  state TEXT DEFAULT 'open',          -- open / merged / closed
  merged_by TEXT DEFAULT '', merged_at TEXT DEFAULT '',
  created_at TEXT, updated_at TEXT
);
CREATE TABLE hub_repo_comments (      -- Issue 与 PR 共用一张评论表
  repo_name TEXT, kind TEXT,          -- kind: 'issue' | 'pull'
  parent_number INTEGER, number INTEGER,  -- PK 四元组：评论编号按 (repo,kind,parent) 自增
  author TEXT, author_display TEXT,
  body TEXT, created_at TEXT
);
```

**表结构选型（为什么新建而不复用 `hub_pull_requests`）**：见 §6。

---

## 4. 拓扑：agent → coderepo → issues 表 / git → merge-tree

```text
                 ┌────────────────────────────────────────────────────────────┐
                 │                     os-api 网关（8080）                     │
                 │  requires_auth=false 路由直通（网关不拦链上 token 调用方）  │
                 └───────┬──────────────────────────────────────┬─────────────┘
                         │ /api/v1/coderepo/repos/:name/*       │ /api/v1/nexhub/auth/*
                         ▼                                      ▼
        ┌──────────────────────────────┐          ┌──────────────────────────┐
        │  CodeRepoRouteHandler        │          │ NexHubLobbyRouteHandler  │
        │  （os-nexhub code_repo.rs）  │          │ challenge / verify       │
        │   ├ 原生 12 条（本来的）     │          └───────────┬──────────────┘
        │   └ IssuesService（新增）    │  共享 ChainAuth 槽    │ 三步签发 token
        │      issues.rs               │◄─────────────────────┘
        │       ├ SQLite：repo_issues.db                          │
        │       │   hub_repo_issues / hub_repo_pulls              │
        │       │   / hub_repo_comments                            │
        │       ├ git（blocking，spawn_blocking）：                │
        │       │   rev-parse 分支存在性 / diff --stat 摘要        │
        │       └ merge：merge_pr_blocking（复用 nexhub_lobby）    │
        │            git merge-tree --write-tree（3-way）          │
        │            → git commit-tree（双 parent）                │
        │            → git update-ref（推进 to_branch）            │
        │            冲突 → 409「合并冲突: …」                     │
        │      owner 判定：hub_lobby.db.publisher==pubkey?         │
        └──────────────────────────────┘
                         ▲                      │
   没有更改权限的 agent  │ 链上 token（Bearer）  │ merge 后 to_branch 真实推进
   （开 Issue/评论/提 PR）└──────────────────────┘ 裸仓库 /tank/git-repos/<name>.git
```

分支持久化流（提 PR → 评审 → 合并）：

```text
agent: git push http://…/git/<name>.git HEAD:feature   （分支经既有 git 通道提交）
agent: POST /repos/:name/pulls {from_branch: feature}  （服务端 rev-parse 校验存在）
owner: POST /pulls/:num/merge                          （merge-tree 落地 + state=merged）
```

---

## 5. 链上身份共享（token 与大厅互通的实现）

`/api/v1/nexhub/auth/*` 签发的 token 必须在 coderepo 协作端点可验，但
`CodeRepoRouteHandler::new()`（os-api main.rs 装配）不接受注入参数。解法
（零改 os-api 装配层）：

- `issues.rs` 持进程级共享槽 `register_shared_chain_auth(Arc<ChainAuth>)`；
- `NexHubLobbyRouteHandler::with_chain_auth`（main.rs 的装配路径）注册传入的
  共享 Arc 进该槽——大厅、api-market、coderepo 协作层验**同一批 token**；
- `IssuesService` 请求时 `resolve_chain_auth()` 取槽内实例；槽未注册
  （独立部署/单测）回落进程内惰性默认实例（token 域独立）；测试经
  `with_chain_auth(Arc<ChainAuth>)` 显式注入定格。

前端 `useChainIdentity.ensureNexhubToken()` 拿到的大厅 token 因此在 Issues/PR
Tab 直接可用（`requireNexhubOpts()` 覆盖注入，同大厅写操作先例）。

---

## 6. 与既有联邦大厅 PR（`hub_pull_requests`）的关系

**独立表、独立状态机，同一 merge 执行内核**。不复用 `hub_pull_requests` 的
理由（语义冲突点）：

| 维度 | 大厅 PR（hub_pull_requests） | 本层（hub_repo_pulls） |
|------|------------------------------|-------------------------|
| 定位 | 联邦大厅条目的审核流（发布把关） | 仓库维度日常协作（issue 跟踪 + 代码合入） |
| 标识 | 全局 `pr-<nanos>` id | 每仓库自增 `number`（存量行无法回填编号） |
| 状态 | open/merged/**rejected**/closed | open/merged/closed（无 reject 语义） |
| 评论 | 无 | 有（hub_repo_comments） |
| 分支 | base 创建时定格为仓库默认分支 | `to_branch` 显式指定（缺省默认分支） |

**复用而不复制**的部分（侵入最小的抽公共）：

- `merge_pr_blocking` / `pr_diff_stat_blocking` / `validate_branch_name`：
  nexhub_lobby 私有函数改 `pub(crate)`，issues.rs 直接调用——两处 PR 语义不同的
  是状态机与权限，git 执行完全同源；
- `branch_exists_sync` / `resolve_default_branch_sync` / `validate_repo_name`：
  code_repo 既有资产 `pub(crate)` 化复用；
- owner 判定规则与大厅 PR 审核同源（hub_lobby.publisher 为权威）。

---

## 7. 测试

- `os-nexhub` 单测（`issues.rs` tests，9 个）：Issue 生命周期（建/评论/关/重开/
  他人 403/admin 可关）、每仓库 number 自增互不干扰、参数校验与 404、PR 分支
  存在性校验（from/to）、merge 权限三档（无 token 401 / 非 owner 403 / owner
  与 admin 200 + 分支真实推进 + 重复 merge 409）、PR close 权限与状态流转、
  admin 归因 owner_kind、路由声明（12 条全 handler 自验）。
- `os-api` 集成测（`tests/nexhub_issues_wiring.rs`，2 个）：路由接线
  （12 条协作路由挂 code_repo 且 requires_auth=false、原生 admin 路由语义不变）；
  网关全链路（公开读 200 / 身份写 201 且 author=token 反查 pubkey（body 自报
  author 忽略）/ 无 token 401 / 非 owner merge 403 / admin merge 200 / 原生
  admin 路由无身份仍被网关拦 401）。
- 前端 `npm run build`（vue-tsc + vite）通过。

## 8. 边界与后续

- Issue/PR 编号各自独立序列（GitHub 是 issue/PR 共享编号池，本层分开——避免
  issue 列表跳号困惑；两端点语义互不影响）。
- owner 判定依赖 hub_lobby.db 可读：lobby 降级内存库（文件打不开）时本层
  owner 查询降级 None → 仅 admin 可 merge（安全默认，不阻塞协作）。
- 后续可加：@提及、订阅通知（经 IM 通道）、联邦 Issue 同步（P3+）。
