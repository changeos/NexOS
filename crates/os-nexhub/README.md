# os-nexhub

NexHub —— **代码仓库中心 + 大厅发现层**。从 os-api 抽离的独立 RouteHandler crate
（审计 `docs/COMPONENT_INDEPENDENCE_AUDIT.md` §6「NexHub 独立化专项」，方案甲）。

## 功能

- **代码仓库中心**（`code_repo`）：原生系统 git 裸仓库管理（不依赖 Gitea/Docker）——
  仓库 CRUD、文件树/文件内容/提交历史浏览、目录导入（`git init + add + commit + push`）、
  AI 会话归档（哪个 agent 会话创建了什么仓库）、聚合统计。
- **大厅发现层**（`nexhub_lobby`，设计文档 `docs/NEXHUB_LOBBY_DESIGN.md`）：个人项目
  发布到大厅分享（SQLite `hub_lobby` 发布索引 + 元数据快照）、搜索/标签/排序浏览、
  一键克隆到本地（服务端 `git clone --bare`，10s 超时）、统计聚合、nexos 主仓库
  默认常驻大厅（每次启动发布/刷新快照，`NEXOS_LOBBY_NO_AUTO_PUBLISH=1` 可关）。
  **货币化（§10）**：条目可标价为免费或虚拟货币（btc/nex/usdc/eth），付费条目克隆前
  需先 `POST /:name/purchase` 取得授权（落库 `hub_entitlement`），未购 → `402`。
  **悬赏（§11）**：大厅内「出资求活」子资源（`hub_bounty` 表 + `/api/v1/nexhub/bounty/*`）：
  发布悬赏（奖励必须 >0）、认领/提交/验收/驳回/取消的完整生命周期；验收时 poster 自证
  支付（复用货币化 `verify_payment`），二期经 `os-wallet` 链上释放。

## 路由（注册进 os-api 网关，抽离前后零变化）

| 组件 | 前缀 | 条数 | 说明 |
|------|------|------|------|
| `code_repo` | `/api/v1/coderepo/*` | 12 | repos CRUD / contents / file / commits / clone-url / import / sessions / stats（写需 admin） |
| `nexhub-lobby` | `/api/v1/nexhub/lobby/*` + `/api/v1/nexhub/bounty/*` | 16 | 大厅 8 条（列表?q/?tag/?sort / stats / entitlements?repo/?buyer 查询，任意已认证 / :name 详情 / publish / DELETE :name / :name/purchase / :name/clone，写需 admin，purchase 需任意已认证）+ 悬赏 8 条（列表?status/?q / :id 详情 / 发布 / :id/claim / :id/submit / :id/approve / :id/reject / :id/cancel，写需任意已认证） |

`/git/*` Smart HTTP（git-http-backend CGI）**留在 os-api 网关装配层**（审计 §6.3
方案甲）：本 crate 的 `build_clone_url_http` 生成指向 `http://<host>:<port>/git/<name>.git`
的地址（URL 字符串契约），os-api 的 `http.rs` 在 `GIT_PROJECT_ROOT` 回退时调用本 crate
的 `repos_dir()`。

## 契约对接

实现 `os_common::gateway::RouteHandler`（轻量契约：无 auth 字段的契约 `ApiRequest` +
`HandlerError`）。os-api 作为组合根在装配层桥接为网关版 `RouteHandler`
（`HandlerError → ApiGatewayError` 身份映射），注册方式与抽离前完全同构
（`main.rs` 的 `gw.register_component("code_repo", ...)` 等）。

## 运行期约定（环境变量 / 路径）

- 仓库根：`NEXOS_GIT_REPOS_DIR` → `OS_GIT_REPOS_DIR` → 默认 `/tank/git-repos`
- clone URL：`NEXOS_GIT_USER`/`OS_GIT_USER`（默认 `oem`）、`NEXOS_GIT_HOST`/`OS_GIT_HOST`
  （默认 hostname）、`NEXOS_HTTP_PORT`/`OS_HTTP_PORT`（默认 8080）
- 大厅 DB：`/tank/os-data/hub_lobby.db` → `/var/lib/os/hub_lobby.db` → `./hub_lobby.db`
  （`NexHubLobbyRouteHandler::with_db_path` 可注入覆盖）
- nexos 常驻开关：`NEXOS_LOBBY_NO_AUTO_PUBLISH=1` 时启动跳过 nexos 大厅常驻
  （发布与快照刷新；用户显式下架 nexos 后不想被启动拉回的场景）
- 外部依赖：PATH 上需有系统 `git` 二进制（失败降级不 panic）

## 独立维护说明

- **依赖面**：仅 `os-common`（网关契约）+ tokio / rusqlite / serde / chrono；
  **不依赖 os-api**（避免审计指出的倒置边）。
- **测试自足**：`cargo test -p os-nexhub`（57 个单测：纯函数解析/构造器 +
  tempdir 真实 git fixture + 内存 SQLite 隔离）。
- **演进方向**：`/git/*` CGI 彻底随迁（审计 §6.3 方案乙，暴露
  `git_http_router(repos_root, auth)` 供 os-api merge）为二期项。
