# NexHub 功能矩阵调研：对标 GitHub / GitLab / Gitea（2026-09-24）

> 目的：用户反馈「NexHub 功能太少」。本调研梳理三家 forge 的功能全景，
> 对照 NexHub 现状，给出可集成功能 top 10（价值 × 成本）、不做清单与分期建议。
> 语境约束：**个人 OS / 单用户 + 小团队 + 联邦节点**；栈 = axum + SQLite + git 子进程 + Vue。
> 资料来源：docs.github.com（pull-requests / issues 索引页，经 jumpbox 拉取）、
> gitea.com 首页、about.gitlab.com/features（经 jumpbox 拉取）+ 模型知识。

## 0. NexHub 现状盘点（避免重复推荐）

| 域 | 已有 | 实现位置 |
|----|------|----------|
| 仓库 | 裸仓 CRUD / 文件树 / README+manifest 渲染 / 提交历史 / HTTP+SSH clone URL / 目录导入 | `crates/os-nexhub/src/code_repo.rs`（12 条路由） |
| Issues/PR（**仓级**） | Issue（title/body/labels/open-closed/评论）、PR（open/merged/closed、diff stat、merge-tree 3-way 合并、owner/admin 才可 merge）、链上身份（secp256k1 challenge\|verify） | `crates/os-nexhub/src/issues.rs`（SQLite `hub_repo_*`） |
| Issues/PR（**film 项目级，第二套**） | labels/assignee/评论/PR、管线失败一键提 Issue | `crates/os-api/src/handlers/film_hub/collab.rs` |
| CI | push 自动触发、Cargo/npm 流水线探测、环形日志、同仓 FIFO + 全局并发 2、徽章（CiBadge.vue）、monorepo 骨架注入 | `crates/os-api/src/handlers/nexhub_ci.rs`（ci.db） |
| 大厅/联邦 | 发布索引、快照自动同步（post-receive 钩子）、两步联邦广播、悬赏（含链上验真放款）、发版 `hub_releases`（**tag + 标题 + 说明，无二进制附件**） | `crates/os-nexhub/src/nexhub_lobby.rs` + `lobby_sync_hook.rs` |
| 周边 | 应用仓协议（nexos-app-* 一键部署）、nexhub CLI、浏览页 Vue 套件 | `crates/os-api/web/src/views/nexhub/` |

关键架构事实（影响成本评估）：
- merge 执行集中在 `merge_pr_blocking`（git merge-tree + commit-tree 双 parent）——加策略只需在此分叉；
- `hub_releases` 已有 tag 化发版，**只缺 assets 附件**；
- `lobby_sync_hook.rs` 证明 post-receive 钩子链路已通——webhook 可复用同一路径；
- `os-im` crate 已有会话/联邦推送基建——通知可搭车；
- `os-mcp` crate 已存在——MCP 工具面只需包装现有 HTTP API；
- os-nexhub 源码中 **webhook 出现次数 = 0**，无 star/watch/milestone/模板/行级评论。

## 1. 三家功能矩阵全景（按域）

### 1.1 代码协作

| 功能 | 三家概况（GH/GL/Gitea） | NexHub 现状 | 缺口 |
|------|------------------------|-------------|------|
| 行级 code review（评论挂 diff 行、review 状态机 approve/request-changes、required reviews） | 全有，GH 以 required reviews + stacked PRs 为 2026 主推 | PR 仅整体评论流 | **缺**（核心缺口） |
| 分支保护（禁直推默认分支 / 必须过 CI / 必须 review） | 全有，规则可组合 | 无 | **缺** |
| Merge 策略（merge-commit / squash / rebase、auto-merge） | 全有 | 仅 merge-commit 一种 | **缺 squash/rebase** |
| Fork + 跨仓 PR | 全有 | 无（联邦靠大厅快照 + 仓级 PR，外部贡献走 clone+push 权限） | 缺，但有联邦替代语境 |
| Releases（tag、说明、**二进制附件**、changelog） | 全有 | tag+notes 已有（`hub_releases`，联邦化） | **缺 assets 附件** |
| Web 编辑器（在线改文件+commit、web IDE） | 全有 | 无 | 缺 |
| Blame / 历史/分支对比 | 全有 | 仅提交历史列表 | 缺 |
| Submodule | 全有（浏览+clone 处理） | git 原生可用，浏览不渲染 | 小缺口 |
| Wiki | 全有 | 无 | 缺 |
| 静态页（Pages） | GH Pages / GL Pages / Gitea Pages（插件） | 无 | 缺 |
| 镜像同步（mirror/push mirror，Gitea 特色） | Gitea 核心卖点，GL 有 repo mirroring | 联邦大厅≈pull-semantic 镜像，无 git 级定时镜像 | 部分缺 |

### 1.2 Issues 域

| 功能 | 三家概况 | NexHub 现状 | 缺口 |
|------|----------|-------------|------|
| Labels | 全有；GL 有 scoped labels（互斥组） | 仓级/film 均有 | 已有 |
| Milestone | 全有 | 无 | **缺** |
| Assignee（多人） | 全有 | film 有单人 assignee，仓级无 | 部分缺 |
| 看板/表格视图（Projects：iteration/date/父子 issue 字段、insights） | GH Projects / GL boards / Gitea projects | 无 | **缺** |
| Issue/PR 模板（.github/ISSUE_TEMPLATE 等） | 全有 | 无 | **缺** |
| 交叉引用（fixes #n 自动关、时间线 cross-link） | 全有 | 无（merge 不联动 issue） | **缺** |
| 子任务/父子 issue、epic（GL） | GH 子 issue / GL epic | 无 | 缺（过重的先不做） |
| @提醒 + 通知（watch/subscribe/inbox） | 全有 | 无 | **缺** |

### 1.3 自动化

| 功能 | 三家概况 | NexHub 现状 | 缺口 |
|------|----------|-------------|------|
| CI/CD | GH Actions（生态最大）/ GL CI（最完整语义）/ Gitea Actions（**兼容 GH Actions 语法，可复用其 marketplace**） | 内置探测式 CI（cargo/npm），badge 已有 | 已有简版；缺自定义流水线配置 |
| Webhooks（push/PR/issue → HTTP 回调 + HMAC） | 全有，生态标配 | **无** | **缺（集成解锁器）** |
| 依赖机器人（dependabot/renovate） | GH 原生 | 无 | 缺 |
| Badge | 全有 | CI badge 已有 | 基本满足 |
| **MCP server**（AI 助手直接操作仓库） | GL 2026 已把自己暴露为 MCP server | 无（但 os-mcp crate 在） | **缺（NexOS 独有契合点）** |

### 1.4 协作周边

| 功能 | 三家概况 | NexHub 现状 | 缺口 |
|------|----------|-------------|------|
| Star/watch | 全有 | 无；联邦大厅有热度/悬赏替代信号 | 部分缺 |
| 贡献图 | GH 招牌 | 无 | 缺（虚荣指标） |
| Org/团队权限 | 全有 | 单用户 + owner/admin + 链上身份，够用 | 语义已覆盖 |
| Discussion 论坛 | GH Discussions / GL forum | 无（IM 大厅承担部分社交） | 缺 |
| Packages / 容器 registry | GL 强项 / Gitea packages 多格式 | 无 | 缺（基建重） |
| OAuth 三方应用 | 全有 | 无 | 缺（无生态） |

## 2. 推荐集成 Top 10（价值 × 成本排序）

> 价值以「个人 OS + 小团队 + 联邦」语境打分；成本按我们栈（axum+SQLite+git 子进程+Vue）估算，单位人日。

| # | 功能 | 一句话价值 | 工作量 |
|---|------|-----------|--------|
| 1 | **Webhooks 事件回调** | 解锁一切外部集成（IM 提醒、自动部署、联邦桥接），三家生态的真正标配 | 2-3 |
| 2 | **NexHub MCP 工具面** | 学 GitLab 2026：把列仓/读文件/开 issue/建 PR 包成 MCP 工具，NexOS 全 AI 语境下 agent 直接操作仓库——差异化最强的一件 | 2-3 |
| 3 | **Release 二进制附件** | `hub_releases` 已有 tag+notes，补 assets 即闭环「发版=下载」；联邦同步元数据即可 | 1-2 |
| 4 | **Merge 策略 squash/rebase** | 单仓线性历史是小团队刚需，`merge_pr_blocking` 单点分叉即可 | 1 |
| 5 | **交叉引用 + 自动关闭** | `fixes #n` 在 merge 时关 issue 并写时间线——协作闭环感的大头，半天级 SQLite 逻辑 | 1 |
| 6 | **行级 review + 状态机 + merge 门禁** | 补齐 PR 质量闭环：diff 行锚评论（file+line+commit）、pending/approved/changes_requested、门禁联动 | 4-6 |
| 7 | **Issue/PR 模板** | 仓内 `nexhub/ISSUE_TEMPLATE.md` 创建表单预填，约定协作规范 | 1 |
| 8 | **通知中心（@mention + watch）** | 评论扫 @、issue/PR 变更进收件箱；os-im 已有会话与联邦推送基建可搭车 | 3-4 |
| 9 | **看板视图 + milestone** | issues 已有 state，加列字段 + Vue 拖拽即得规划感；milestone 顺带（表+due date） | 2-3 |
| 10 | **分支保护** | pre-receive 钩子拦默认分支直推 + 「CI passed 才可 merge」，与 #6 门禁共用规则表 | 2-3 |

### 实现草案要点

1. **Webhooks**：SQLite `hub_webhooks(repo, url, secret, events, active)`；事件源三处——`lobby_sync_hook` post-receive（push）、`issues.rs` 写路径（issue/PR/comment）、`merge_pull`；投递 = 后台 tokio 任务 POST JSON + HMAC-SHA256 签名头，环形重试。管理走 `/api/v1/coderepo/repos/:name/hooks`。
2. **MCP 工具面**：os-mcp 注册 `nexhub_list_repos / nexhub_read_file / nexhub_create_issue / nexhub_comment / nexhub_open_pr`（身份沿用链上 token），全部薄包装现有 handler；不新起服务。
3. **Release assets**：`hub_release_assets(release_id, name, size, sha256, path)`；文件落 `<repos_root>/.assets/<repo>/<release_id>/`；上传（owner/admin）+ 下载（公开流式）两路由；联邦 payload 增 assets 元数据清单。
4. **Merge 策略**：`POST /pulls/:num/merge?strategy=squash|rebase|merge`；squash = merge-tree 后单 commit-tree（合注 PR 标题）；rebase = fast-forward 校验后 update-ref；冲突仍 409。
5. **交叉引用**：merge 成功路径正则扫 PR body+comments 的 `(fixes|closes|resolves) #(\d+)` → 批量置 issue closed + 双向时间线记录（`hub_issue_events` 表，issue/PR 通用，为后续通知/活动流打底）。
6. **行级 review**：PR 详情已产 `git diff`，解析 unified diff 生成 (file, new_line, commit) 锚点存 `hub_pull_review_comments`；`hub_pull_reviews(reviewer, state, summary)`；门禁规则「≥1 approved 且无 changes_requested」在 merge 前校验（可配置关闭）。
7. **模板**：建 issue/PR 表单页先 GET 文件树探测 `.nexhub/ISSUE_TEMPLATE.md` / `PULL_REQUEST_TEMPLATE.md`，存在则 markdown 分段（`## 标题` 转 label）预填。
8. **通知**：`hub_notifications(user, kind, subject, read, created_ms)` + 顶栏未读铃铛；@mention 从评论正文正则提取 handle（映射 pubkey/agent id）；watch = 仓库维度订阅开关。os-im 打通（通知同步进默认会话）放 P1 末。
9. **看板**：`hub_repo_issues` 加 `kanban_col`（backlog/doing/done），看板页四列拖拽（复用现有 issues 列表 API + PATCH）；milestone = `hub_milestones` 表 + issue.milestone_id。
10. **分支保护**：`hub_branch_rules(repo, pattern, block_direct_push, require_ci)`；pre-receive 钩子（复用 post-receive 安装机制）查表拦截；merge 端点查 require_ci → 最近 run 必须 passed。

## 3. 建议不做清单

| 功能 | 不做的理由 |
|------|-----------|
| Packages / 容器 registry | 存储索引 + 认证 + 拉取协议全套基建，且个人 OS 无消费生态 |
| Gitea Actions 兼容层 | 兼容 GH Actions 语法面巨大（runner 协议/marketplace），我们的 CI 定位是「仓库健康」不是通用执行平台 |
| Org / 团队权限体系 | 单用户 + owner/admin + 链上身份已覆盖全部真实场景，引入角色矩阵纯增复杂度 |
| Discussions 论坛 / follow 社交 / 贡献图 | 虚荣或社区运营向，单用户小团队场景不符；IM 大厅已承担发现/社交 |
| 依赖机器人（dependabot 类） | 需持续可达公网 registry + 依赖库谱数据，与「本机自治」语境冲突 |
| 安全扫描（GL 式 SAST/依赖扫） | 规则库与侧数据库重，价值靠规模 |
| OAuth 三方应用 | 没有三方开发者生态可接，MCP 工具面（top 2）已覆盖「外部程序操作 NexHub」的真需求 |
| Stacked PR / epic / value stream analytics | 企业级工作流编排，复杂度远超收益 |
| Fork（暂缓） | 联邦大厅 + 仓级 PR 已提供外部协作路径；本地 fork 语义等真有多贡献者再做 |
| Wiki（暂缓，低成本可选） | 本质是「另一个仓 + 已有的 MarkdownView」，但价值低——不如先把 Releases/模板做实 |

## 4. 分期建议

- **P0 快赢（合计约 1 周）**：#3 Release assets → #4 merge 策略 → #5 交叉引用 → #7 模板 → #1 webhooks。全部单点插入现有代码路径，无新基建。
- **P1（2-3 周）**：#2 MCP 工具面 → #6 行级 review+门禁 → #8 通知 → #9 看板+milestone → #10 分支保护。#2 放 P1 首位（差异化叙事强且不依赖别家）。
- **P2（机会型）**：Web 编辑器、Pages 静态站（CI 已能 build，只差服务路由）、Gitea 式 mirror（「备份我的 GitHub 仓到 NexOS」场景真实）、star 联邦热度信号、blame 渲染。

## 5. 开放问题

1. **两套 Issues/PR 归一**：新功能（模板/看板/通知/交叉引用）落在仓级 `issues.rs` 还是先抽公共内核供 film `collab.rs` 复用？建议：事件表与模板放仓级内核，film 侧逐步迁移。
2. **通知通道**：独立 `hub_notifications` 表 + UI 铃铛起步，还是直接进 os-im 会话（天然带联邦推送）？后者起点高但耦合 IM 协议演进。
3. **Webhook 可达性**：联邦对端/内网目标的 TLS、重试与死信策略；联邦节点间 webhook 是否应改走既有 fed 传输而非裸 HTTP。
4. **Review 身份权重**：链上身份（pubkey）世界里「≥1 approved」的 reviewer 池如何界定——owner 之外的哪些 pubkey 算有效 review？
5. **CI 门禁口径**：现有 CI 只跑默认分支 HEAD（「仓库健康」口径）；require_ci 门禁是否需要升级为「PR 目标 commit 精确 run」？
