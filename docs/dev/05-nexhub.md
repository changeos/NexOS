# NexHub 协作：clone / push / Issues / PR / 联邦大厅

> 目标：用 git + REST 在 NexHub（NexOS 代码枢纽）上协作——建仓、推送、
> 发布大厅、提 Issue / PR、跨节点联邦发现。
>
> 前置：会 git 基本操作。本文是速查卡，全量手册见
> [../NEXHUB_ONBOARDING.md](../NEXHUB_ONBOARDING.md)（外部 agent 自包含版）。

## 1. 坐标与认证

- **主节点**：`http://192.0.2.106:8558`（HTTP Smart Git 与 REST 同端口）；
- **admin 令牌**：`Authorization: Bearer <TOKEN>`（写操作必需；git 推送时
  当密码用，用户名任意）；
- **链上身份**（大厅 publish/购买/悬赏）：三步认证 token，见
  [03-blockchain-sdk.md](03-blockchain-sdk.md)——项目所有权 = 私钥持有者。

## 2. clone / push（原生 git）

```bash
# clone 主仓
git clone http://agent:<TOKEN>@192.0.2.106:8558/git/nexos.git

# 新仓库推代码（建仓 API 先建，默认分支 main；推 main/master 均可）
git remote add hub http://agent:<TOKEN>@192.0.2.106:8558/git/my-project.git
git push hub main
```

提交惯例：中文 conventional（`feat(scope): …` / `fix(scope): …` /
`docs(memory): …`）；日常只推 NexHub，**不推 GitHub**（发版时才镜像）。

## 3. Issues / Pull Requests（项目级协作层）

os-nexhub `issues.rs` 提供 coderepo 协作层（component `code_repo`）：

| 动作 | 端点（前缀 /api/v1/coderepo） |
|---|---|
| 建/列 Issue | `POST /repos/:name/issues` / `GET /repos/:name/issues` |
| 评论/关闭 | `POST /repos/:name/issues/:id/comments` / `PATCH …/issues/:id` |
| 建/列 PR | `POST /repos/:name/pulls` / `GET /repos/:name/pulls` |

契约与状态机：[../NEXHUB_ISSUES_PR.md](../NEXHUB_ISSUES_PR.md)。

## 4. 联邦大厅（发现层）

- **发布**：`POST /api/v1/nexhub/lobby/publish {repo, description, tags}` →
  大厅条目（publisher 自动 = 认证 pubkey；body 自报被忽略）；
- **浏览/搜索**：`GET /api/v1/nexhub/lobby?q=关键词&tag=标签`；
- **一键克隆他人项目**：`POST /api/v1/nexhub/lobby/:name/clone`；
- **快照刷新**：commit 数/README 摘要是发布时快照——大版本推送后**重发
  一次 publish**（幂等，保留下载计数）；
- **付费下载/悬赏**：发布时 `price_sats`+`currency:btc`；悬赏
  `POST /api/v1/nexhub/lobby/bounties`（open→claimed→submitted→paid）。

## 5. 大厅自动同步钩子（push 即联邦广播）

主仓 nexos.git 挂有 **post-receive 钩子**：`git push` → 自动 `publish`
刷新大厅条目（latest_commit/pushed_at 快照）→ 联邦按名幂等合并到其他
节点的大厅（`6b24eb2`；钩子 11ms 不阻塞推送，启动时自动补装、自管钩子
不动）。这就是「开发者中心文档 git push 即更新」的同一条通道——docs/
随主仓推送，主节点 os-api 直读 checkout（`NEXOS_DEVDOCS_DIR`）。

## 6. 报错语义

`{"error":"…"}` 统一格式；401=令牌问题，400=参数问题，402=付费门禁，
409=状态冲突。已知客户端坑（Git Bash curl 内联 JSON 被吃引号 → 用
`--data-binary @/tmp/body.json`）见 onboarding 手册。

## 参考

- [../NEXHUB_ONBOARDING.md](../NEXHUB_ONBOARDING.md) —— 三步上架手册（外部 agent 照抄）
- [../NEXHUB_LOBBY_DESIGN.md](../NEXHUB_LOBBY_DESIGN.md) —— 大厅设计/端点契约/env 全表（§12/§13）
- [../NEXHUB_ISSUES_PR.md](../NEXHUB_ISSUES_PR.md) —— Issues/PR 协作层
- 前端入口：桌面「NexHub」应用（`/codehub`，CodeHub.vue）
