# NexHub CLI（nexhub）—— 分发端点与命令面

> 状态：已实现（NexHub 网页优先重排 P2 批次，docs/research/NEXHUB_WEB_CLI_DESIGN.md §B / §4.1）
> 日期：2026-09-05
> 实现：`crates/os-api/src/handlers/nexhub_cli.rs`（端点 + 渲染）、
> `crates/os-api/src/assets/nexhub-cli.sh`（脚本资产，`include_str!` 随二进制分发）
> 关联文档：[NEXHUB_ONBOARDING.md](NEXHUB_ONBOARDING.md)（agent 三步上架）、
> [NEXHUB_LOBBY_DESIGN.md](NEXHUB_LOBBY_DESIGN.md)（大厅/链上身份）、
> [NEXHUB_ISSUES_PR.md](NEXHUB_ISSUES_PR.md)（协作层）

## 1. 一句话

**一台没有任何预装的机器，一条 curl 命令获得 NexHub 代码托管 / 应用分发的 CLI**：
`nexhub` 是单文件自包含 POSIX sh 脚本（无二进制下载），由节点按请求动态生成——
缺省节点地址烘焙自 HTTP Host 头，装完即连回提供它的节点；升级 = `nexhub self-update`
重新拉同一端点。

## 2. 安装（一条命令）

```bash
curl -fsSL http://<节点地址>:8558/api/v1/coderepo/cli.sh | sh
```

- 安装目标：`~/.local/bin/nexhub`（在 PATH 内时）否则 `/usr/local/bin/nexhub`
  （不可写时打印 sudo 手动命令）。
- 安装即最新版；`nexhub self-update` 随时可刷新（下载后先 `bash -n` 语法校验再原子覆盖）。

## 3. 分发端点：`GET /api/v1/coderepo/cli.sh`

| 属性 | 值 |
|------|----|
| 鉴权 | **公开读**（`requires_auth=false`——未登录机器可达） |
| 生成方式 | 按请求动态渲染（照 `provisioning.rs` install.sh 先例） |
| 缺省节点地址 | `X-Forwarded-Host` → `Host` → env `NEXOS_GIT_ADVERTISE_HOST` → `127.0.0.1:8558` 兜底；无端口 Host 自动补缺省端口 **8558** |
| 响应 | `content-type: text/x-shellscript; charset=utf-8` + `X-Content-Type-Options: nosniff`，经网关 `text/*` 直传通道返回脚本原文（非 JSON 信封），`curl \| sh` 即用 |
| 组件 | `nexhub_cli`（`NexhubCliRouteHandler`，os-api） |

烘焙进脚本的三项（占位符单引号净化，防注入同 install.sh）：

| 占位符 | 内容 |
|--------|------|
| `@@NEXHUB_NODE@@` | 缺省节点 base URL（凭据缺省值；`NEXHUB_NODE` env / `login <node-url>` 可覆盖） |
| `@@NEXHUB_CLI_URL@@` | cli.sh 自身 URL（安装模式重新下载 / `self-update` 用） |
| `@@NEXHUB_VERSION@@` | 脚本版本 = 运行二进制 `CARGO_PKG_VERSION`（脚本随节点二进制天然同版本） |

## 4. 命令面

| 命令 | 端点映射 | 说明 |
|------|---------|------|
| `nexhub login [token]` \| `[node-url]` \| `<node-url> <token>` | —（本地） | 双参形式显式指定节点；单参形似 URL（含 `://`）则视为节点地址、token 交互粘贴；无参全交互（TTY 上关闭回显）。**成功即写凭据文件（0600）** |
| `nexhub whoami` | `GET /api/v1/version` + admin 探测 | 打印节点地址/版本/凭据路径 + token 有效性（401 → 红字"token 无效"退出 1） |
| `nexhub ping` | 同上 | 连通 + token 校验，退出码 0/1（供脚本）；无凭据时仅验连通 |
| `nexhub repo list [--json]` | `GET /api/v1/coderepo/repos` | 表格 NAME/DESCRIPTION/BRANCH/COMMITS |
| `nexhub repo create <name> [desc]` | `POST /api/v1/coderepo/repos` | 201 成功 / 409 已存在 / 400 名非法 |
| `nexhub repo delete <name> [--yes]` | `DELETE /api/v1/coderepo/repos/:name` | 确认提示（`--yes` 跳过）；404 仓库不存在 |
| `nexhub repo info <name> [--json]` | `GET …/repos` + `GET …/repos/:name/contents` | 元数据 + **Clone (SSH) / Clone (HTTP) 两行** |
| `nexhub clone <repo>` | `GET /api/v1/coderepo/repos` → `exec git clone <clone_url_http>` | **token 不进 URL**；提示 push 凭据（用户名任意、密码=token）或用 SSH 地址；无 git 时打印手动命令 |
| `nexhub apps list [--json]` | `GET /api/v1/apps/catalog` | 表格 REPO/VERSION/INSTALLED/STATUS（已装(最新)/可升级/未装/错误）/DETAIL |
| `nexhub apps deploy <repo> [--json]` | `POST /api/v1/apps/install {repo}` | 输出 action（install/upgrade/noop）+ 版本；4xx/5xx 红字 stderr + 退出 1 |
| `nexhub apps remove <id> [--yes]` | `DELETE /api/v1/apps/:id` | 确认提示（`--yes` 跳过） |
| `nexhub self-update` | `GET /api/v1/coderepo/cli.sh` | 重新下载（`bash -n` 校验）原子覆盖自身（`command -v nexhub` 定位） |
| `nexhub version` / `help` / `--help` | — | 版本 / 帮助（顶注含安装一行命令） |

退出码：**0** 成功 / **1** 远端或运行时错误（stderr 红字 `[nexhub]`）/ **2** 参数错误。

## 5. 凭据与安全

- 凭据文件：`~/.config/nexhub/credentials`，权限 **0600**（临时文件 0600 + 原子
  rename；重复 login 收紧已有宽松权限），内容：

  ```
  NODE_URL=http://<节点>:8558
  TOKEN=<token>
  ```

- 环境变量（CI 场景，**覆盖凭据文件**）：

  | 变量 | 缺省 | 作用 |
  |------|------|------|
  | `NEXHUB_NODE` | 凭据 `NODE_URL` → 安装时烘焙的缺省节点 | 覆盖节点地址 |
  | `NEXHUB_TOKEN` | 凭据 `TOKEN` | 覆盖 token |

- **token 不进 argv**：`Authorization: Bearer` 头写入 0600 临时文件，经
  `curl -H @file` 注入（防 `ps` 泄漏），进程退出即清理。
- token 有效性探测复用既有 admin 读端点（`GET /api/v1/provisioning/ssh/deploys`，
  401 = 无效）；admin token 读写全量；链上 token 由 Web/IM 侧取得后 login，
  CLI 不做签名（写操作归因链上公钥走大厅端点自身校验）。

## 6. 依赖与兼容

- **curl 必须**；**jq 首选**，缺失降级 `python3 -c json`（两者皆无时仅 `--json`
  原样输出并提示）。
- POSIX sh 语法（bash/dash 均可解释，`curl | sh` 在 dash 下同样可安装）；
  避免 bash4 特性（macOS 自带 bash 3.2 兼容）。
- Windows/Git Bash 不在本期支持范围（同 install.sh 的引号坑规避，见设计稿 §6.4#5）。

## 7. 测试与实现索引

| 项 | 位置 |
|----|------|
| 端点/渲染单测（9 个：路由公开性、200+CT+nosniff、Host 三档推导、单引号净化、`bash -n` 语法、credentials 0600——root 跳过） | `crates/os-api/src/handlers/nexhub_cli.rs` |
| 网关接线集成测（2 个：无身份 200 直传、Host 推导穿越 dispatch） | `crates/os-api/tests/nexhub_cli_wiring.rs` |
| 脚本资产 | `crates/os-api/src/assets/nexhub-cli.sh`（仓库内可独立 `bash -n` 校验） |

本功能**零新增后端环境变量**（缺省节点地址推导复用 `NEXOS_GIT_ADVERTISE_HOST`；
CLI 侧 `NEXHUB_NODE`/`NEXHUB_TOKEN` 为客户端 env，不涉及节点配置）。
