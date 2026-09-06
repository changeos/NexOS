---
category: 开发者指南
---

# 开发者指南索引（docs/dev/）

> NexOS 开发者功能与文档的常驻门户入口。桌面「开发者中心」应用（`/devdocs`）
> 直接渲染仓库 `docs/`——**文档唯一事实源 = 本仓库**，git push 即更新。

## 指南清单

| # | 文档 | 一句话 |
|---|------|--------|
| 01 | [应用开发指南](01-app-development.md) | 新增桌面应用全流程（Vue→appRegistry→router→图标→client→构建），含「图标上桌面」两步 |
| 02 | [安装自己的应用](02-app-install.md) | 应用中心 install_type=nexos 现状、外部拒绝策略、正确姿势=进主仓开发 |
| 03 | [区块链 SDK](03-blockchain-sdk.md) | secp256k1 三步认证、EVM 地址派生、一钥多组件、python 两个必踩坑（低 S + 压缩前缀）附正确代码 |
| 04 | [IM Agent 接入](04-im-agent.md) | 三步认证→WS→进群→@ 唤醒→断线补拉；协调组件注册与 webhook |
| 05 | [NexHub 协作](05-nexhub.md) | clone/push、Issues/PR、联邦大厅、push 即广播的 post-receive 钩子 |
| 06 | [os-api Handler 开发](06-os-api-handler.md) | 新增 RouteHandler 装配六步（mod.rs/main.rs/测试惯例，agent_coord 范例） |
| 07 | [多节点部署](07-deploy.md) | 三节点布局（106/113/aliyun）、构建分发、更新 artifact 闭环、cron 应急通道 |

## 如何贡献文档（自动出现在开发者中心）

**把 `.md` 放进 `docs/`（或一级子目录）即可**——开发者中心索引按以下规则
自动收录（无需任何注册）：

1. 扫描范围：`docs/` 根目录 + 一级子目录（`dev/`、`adr/`、`agents/`…）
   的全部 `*.md`；
2. 标题取正文首个 `# ` 行（没有则用文件名）；
3. 分类：frontmatter `category:` 优先，否则一级子目录名，根目录文件归
   `docs` 组。本文件即示例——frontmatter 声明了「开发者指南」分组：

   ```markdown
   ---
   category: 开发者指南
   ---
   # 标题
   ```

4. 推送即生效：主节点直读 checkout（后端缓存 30s，新增/删除文件即时可见）。

写作要求（仓库铁律，`docs/README.md`）：

- 每个功能的新增能力和**全部环境变量**必须写进对应 MD（`| 变量 | 默认 | 作用 |` 表）；
- 文档要详细且带**组件拓扑**（ASCII/mermaid：分层/数据流/外部依赖），
  面向「零上下文读者 + 可提取为演示素材」；
- 内容必须来自真实代码勘察——引用真实文件路径/端点/提交惯例，不编造。

## 后端契约速查（DevDocsRouteHandler，component=`devdocs`）

| method | path | 动作 |
|--------|------|------|
| GET | `/api/v1/devdocs/index` | 文档索引（分类/标题/路径/大小/mtime；缓存 30s） |
| GET | `/api/v1/devdocs/doc/*path` | 单篇 Markdown 原文（仅 .md；canonicalize 防穿越） |

env：`NEXOS_DEVDOCS_DIR`（文档根，缺省 `/home/oem/NexOS/docs`，回退二进制旁
`./docs`）；目录不存在 → 降级空清单 + 「文档服务在本仓库节点」提示
（113/aliyun 无 checkout 形态；联邦分发 docs 属下期）。

## 全局文档地图

功能文档速查表（40+ 篇）：[../README.md](../README.md) · 架构全景：
[../ARCHITECTURE.md](../ARCHITECTURE.md) · 交接账：仓库根 MEMORY.md
