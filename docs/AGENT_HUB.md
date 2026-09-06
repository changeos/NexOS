# Agent 集合（AgentHub）

> 源码：`crates/os-api/src/handlers/agenthub.rs`（`AgentHubRouteHandler`，组件名 `agenthub`）
> + 子模块 `crates/os-api/src/handlers/agenthub_toolchain.rs`（工具链手动安装）·
> 前端：`crates/os-api/web/src/views/AgentHub.vue`（路由 `/agenthub`，appRegistry id=`agenthub`）
> 登记：2026-09-02 · 路由表/env 均从源码核实 · Web 界面 agent 节 2026-09-02 增补（实测记录）

## 1. 功能说明

「Agent 集合」桌面应用的后端 REST 入口：常用 **AI coding agent**（OpenCode /
OpenClaw / Claude Code / Codex / Gemini CLI / Qwen Code / Aider / Goose / Crush）
的目录浏览、**一键安装/卸载后台任务**、已安装探测与工具链可用性探测，外加
自定义 agent 发布（持久化），以及**工具链手动安装**（node/uv/cargo 用户态
安装器，2026-09-02 增补——此前 npm 渠道提示「缺少 node/npm 工具链」按钮禁用
却无处可装）。

与应用中心（app_store，NexOS 内置模块目录）和 agent 协调组件（agent-coord，
IM @ 定向投递）均不重叠：本组件管**把哪些 AI agent CLI 装到本机**。

- **预置目录**：9 条常用 AI agent（`preset_agents()`，代码常量），npm 渠道 6 条
  （opencode-ai / openclaw / @anthropic-ai/claude-code / @openai/codex /
  @google/gemini-cli / @qwen-code/qwen-code）、uv 1 条（aider-chat）、script
  2 条（goose / crush 官方安装脚本）；每条含 `check_binary`（探测用可执行名，
  全目录唯一）。
- **一键安装/卸载**：命令由纯函数构造（`build_install_cmd` / `build_uninstall_cmd`，
  可单测不真跑）→ `tokio::process` 真实 spawn。**fire-and-forget**：请求立即
  返回 201 + 任务对象，进程退出码在 `tokio::spawn` 后台任务里回写（status /
  pid / error / log_tail 尾 10 行）。spawn 失败（工具链缺失）或退出非 0 →
  `failed` + 日志，绝不 panic。
- **npm sudo 决策**：npm 渠道安装前探测 `npm config get prefix` 的 lib/node_modules
  写权限，不可写则命令前置 `sudo`（stdin 已 null，sudo 需密码时立即失败不挂起）；
  env `NEXOS_AGENTHUB_NPM_SUDO`=always/never 覆盖自动探测。
- **已安装探测**：`sh -c "command -v <check_binary>"`（spawn_blocking）之外
  显式探测用户级安装目录 `~/.local/bin`、`~/.cargo/bin`、
  `~/.nvm/versions/node/<ver>/bin`（systemd 服务 PATH 可能不含；探测口径详见
  §1.2）；二进制名先过白名单（字母数字与 `.` `_` `-`，≤64 字符）杜绝
  shell 注入。
- **工具链探测**：node / npm / uv / cargo / curl 逐个 `--version`（3s 超时），
  程序名先经 `resolve_bin` 解析用户级 bin（uv/cargo 可能仅装在
  `~/.local/bin` / `~/.cargo/bin`，nvm 装的 node/npm 仅在
  `~/.nvm/versions/node/<ver>/bin`，服务 PATH 不含）；前端据此禁用缺工具链的
  安装按钮。安装/卸载 spawn 同样先解析程序真实路径（sudo 包裹时解析其后的
  工具名），避免服务 PATH 缺用户级 bin 时 ENOENT。
- **自定义发布**：`POST /publish` 追加自定义 agent（渠道校验 npm/script/uv/cargo；
  script 目标须 http(s) URL；check_binary 过白名单），JSON 原子持久化；
  `DELETE /published/:id` 仅可删 `source=user` 条目。
- **工具链手动安装**（子模块 `agenthub_toolchain.rs`）：`POST
  /api/v1/agenthub/toolchain/install`（admin，body `{name}`，name ∈
  `node|uv|cargo`——node 一次覆盖 node+npm 两者；curl/bash 为系统基础件不装）
  → `202 {task_id, toolchain, status}` **异步任务**（安装下载数十 MB）。
  一律**用户态安装，无 sudo/apt**（os-api 进程无 root）；幂等：探测已命中 →
  任务直接 done 提示「已安装」；同工具链有 running 任务时重复 POST 409。
- **Web 界面 agent**（「打开界面」能力，见 §1.3）：目录条目可选 `web`
  描述符（仅实测确认有 Web UI 的 agent 标注，首期 OpenCode）→ 三个
  `/web/:agentId/*` 端点管理其后台服务进程（spawn start_cmd → 端口就绪 →
  返回浏览器可直达的 URL；进程表 + 环形日志 100 行 + 端口探测兜底恢复）。

### 1.1 工具链安装源矩阵（中国镜像优先，全部可 env 覆盖）

| 工具链 | 安装方式 | 主源（优先） | 回退源（失败重试一次） | 落点 |
|--------|----------|--------------|------------------------|------|
| `node`（+npm）| nvm 安装脚本 `curl -o- <url> \| bash` → `. nvm.sh && nvm install --lts` | `https://ghfast.top/https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh`，env `METHOD=script`（避开 github.com git clone）+ `NVM_SOURCE=<ghfast 镜像 nvm.sh>`；node 二进制 `NVM_NODEJS_ORG_MIRROR=https://npmmirror.com/mirrors/node` | 官方 `https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh`（无镜像 env） | `~/.nvm`（bin 在 `~/.nvm/versions/node/<ver>/bin`）；完成后幂等追加 nvm 初始化到 `~/.bashrc`（已含 nvm 字样则不动） |
| `uv` | 官方脚本 `curl -LsSf https://astral.sh/uv/install.sh \| sh` | astral.sh 直连 | 同一脚本 + env `INSTALLER_DOWNLOAD_URL=https://ghfast.top/https://github.com/astral-sh/uv/releases/latest/download`（uv 官方支持的下载源覆盖变量）| `~/.local/bin/uv` |
| `cargo` | rustup `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh -s -- -y --profile minimal --default-toolchain stable` | 清华 TUNA 镜像：env `RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup` + `RUSTUP_UPDATE_ROOT=https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup` | 官方（去镜像 env 重试一次） | `~/.cargo/bin/cargo` |

任务模型照推理环境（llm_envs）：进程内任务表 + 环形日志（上限 200 行）+ 轮询
`GET /api/v1/agenthub/toolchain/install/tasks/:id`；全部外部命令经
`ToolchainExecutor` 抽象（生产 std::process 真实执行 + 30min 超时 kill，测试
注入 mock 绝不联网）；探测函数可注入（测试隔离宿主 PATH）。状态机
`running → done | error`；安装后校验产物文件存在（node/npm 经用户目录探测、
uv/cargo 查 `~/.local/bin/uv`、`~/.cargo/bin/cargo` 可执行位）。

### 1.2 探测口径（用户级安装位置兜底）

`command -v` 之外显式探测用户级 bin 目录——systemd 服务 PATH 常不含它们，
探测/spawn 统一经 `resolve_bin_in(home, name)` 解析（`agenthub.rs`）：

1. `~/.local/bin`（uv / npm 用户级前缀）
2. `~/.cargo/bin`（cargo install / rustup）
3. `~/.nvm/versions/node/<ver>/bin`（nvm 装的 node/npm 与 npm -g 前缀；
   多版本取**数值最高**——`nvm_bin_dirs` 数值降序，避免字典序 `v10 < v9` 错排）

**PATH 注入口径（选定方案）**：不做进程 PATH 注入——探测命中即返回完整路径，
agent 安装/卸载 spawn 前对程序名做同样解析（既有 `resolve_bin` 机制），按钮
解禁后即以完整路径调用 npm/uv/cargo；用户登录 shell 由 `~/.bashrc` 的 nvm
初始化行与 rustup 的 profile 修改自行覆盖。

### 1.3 Web 界面 agent（「打开界面」能力，2026-09-02 实测增补）

装好的 agent 若有 Web UI，页面上「打开界面」一键启动服务并新标签直达网页。
**诚实不猜**：只有目录条目带 `web` 描述符的 agent（预置目录里目前仅
OpenCode）才有按钮，其余 agent 不标不猜——描述符由代码常量标注
（`preset_agents()`），不经 publish API 注入（避免任意命令行混进目录数据）。

**`web` 描述符契约**（`CatalogAgent.web: Option<AgentWebDesc>`，`#[serde(default)]`
——旧持久化文件缺此键读回 None；None 序列化跳过不落键）：

| 字段 | 类型 | 说明 |
|------|------|------|
| `start_cmd` | `Vec<String>` | 启动命令 argv；argv[0] spawn 前经 `resolve_bin` 解析用户级安装位置绝对路径 |
| `port` | `u16` | 固定服务端口（就绪探测 / 停止 / 恢复都按它） |
| `url_path` | `String` | Web UI 路径（含前导 `/`） |
| `note` | `String` | 备注（鉴权形态等实测记录，前端按钮 tooltip） |

**端点**（3 条，进程表 agentId→{pid, port, started_at, 环形日志 100 行}）：

| method | path | 鉴权 | 行为 |
|--------|------|------|------|
| POST | `/api/v1/agenthub/web/:agentId/start` | admin | spawn `start_cmd` → ≤15s 端口就绪轮询（500ms 间隔；进程早夭立即失败）→ `200 {agent_id, url, pid, port, state}`，state ∈ `started` / `idempotent`（已在跑，幂等返回）/ `recovered`（端口被占且表丢失，按端口重建表）；失败 `500 {error}`（错误串内嵌日志尾）。未知 agent 404；未标注 web 的 agent 400 |
| POST | `/api/v1/agenthub/web/:agentId/stop` | admin | SIGTERM→≤3s→SIGKILL；端口仍开 → `fuser -k <port>/tcp` 兜底（实证：opencode npm 包 bin 为 shim，真实服务进程是 spawn 出的子进程、接管端口——只杀记账 pid 关不掉端口）→ `200 {ok:true}`；未在运行 404；兜底后端口仍占 500 |
| GET | `/api/v1/agenthub/web/:agentId/status` | 公开 | `{running, url, pid, port, started_at, log_tail}`（尾 20 行）；表空但端口在监听 → 重建表返回 running（os-api 重启恢复）；表内条目端口已关 → 清死条目返回 running=false |

**URL 推导**：`http://<host>:<port><url_path>`，host 三分支（provisioning
`source_base_url` 同款先例）：请求 Host 头（**去掉 API 端口**——如
`192.168.1.5:8558` → `192.168.1.5`，跨机访问即节点 IP/域名；IPv6
`[::1]:8558` → `[::1]`）→ env `NEXOS_GIT_ADVERTISE_HOST` → `127.0.0.1`。

**实测 OpenCode 行为记录**（2026-09-02，`opencode-ai` 1.18.26，npm 渠道装于
`~/.local`（npm 用户级前缀），`opencode serve --help` + 实跑验证）：

- `opencode serve` 起 headless HTTP 服务，**Web UI 直接服务在根路径 `/`**
  （HTTP 200 HTML 应用，无重定向、无独立 /app 前缀）；
- `--port` **缺省 0 = 随机端口**——必须显式固定（本组件用 4096，opencode
  官方文档 serve 示例常用端口）；
- `--hostname` 缺省 `127.0.0.1`——跨机（节点 IP）访问必须传 `0.0.0.0`；
- **无 token**：启动 stderr 明确警告 `OPENCODE_SERVER_PASSWORD is not set;
  server is unsecured.`——OpenCode 自身经 `OPENCODE_SERVER_PASSWORD` env 做
  鉴权，本组件**不注入该 env**（无鉴权透传，如实记录）；需要鉴权的运维请
  自行给 os-api 服务配该 env；
- 日志形态：`opencode server listening on http://0.0.0.0:45671` 一行就绪
  标记（`--print-logs` 时结构化日志走 stderr）；端口被占时启动即退
  （`Error: Unexpected error / ServeError`），本组件的就绪轮询会立即失败
  并带日志尾；
- 启动就绪实测 ~1s 内（本地）；SIGTERM 干净退出；
- **bin 为 shim**：npm 包装的 `opencode` 入口会 spawn 平台二进制接管端口
  （实测 spawn 得到 pid 312617，持端口的是其子进程 pid 312619）——stop 只杀
  记账 pid 关不掉端口，本组件在端口探测不关时用 `fuser -k <port>/tcp` 兜底。

**边界**：子进程**不随 os-api 退出而亡**（reparent 到 init）——重启后进程表
丢失，start/status 按端口探测兜底重建表（`state=recovered` / `pid=null`，
pid 与启动时间如实置空；stop 走 fuser 按端口杀）。若端口被**其它进程**占用，
恢复机制同样会认领该端口（描述符端口固定，无 per-agent 区分指纹）——admin
stop 或自行排查。外部命令经 `WebLauncher` 抽象（生产 `ProcessWebLauncher`
真实 spawn + 读者线程 + 收尸线程防僵尸 + `kill`/`fuser`/TcpStream 探测；
测试注入 mock 绝不真跑 opencode）。

## 2. 组件拓扑与数据流

```
浏览器 AgentHub.vue ──POST /api/v1/agenthub/install──▶ os-api 网关（Auth: admin）
        │                                                    │
        │ GET /agents /installed /toolchains                 ▼
        │ GET /tasks[/:id]                            AgentHubRouteHandler
        │                                            ┌─────────┼──────────────┐
        │                                            ▼         ▼              ▼
        │                                      preset_agents() published   tasks
        │                                      （代码常量9条） Arc<Mutex>  Arc<Mutex>
        │                                            │         （JSON 持久化）（内存，≤100）
        │ GET /agents 时合并 installed               ▼
        ▼                                    spawn_blocking:
浏览器 ◀──────── JSON ───────────── command -v <check_binary>（已装探测）
                                                     │
                                     POST /install → build_install_cmd() 纯函数
                                                     ▼
                        tokio::process::spawn + tokio::spawn 后台等待（fire-and-forget）
                        [sudo] npm install -g <pkg>   （npm 渠道）
                        bash -c "curl -fsSL <url> | bash"（script 渠道）
                        uv tool install <pkg>          （uv 渠道）
                        cargo install <crate>          （cargo 渠道）
                                                     │ 退出码回写 tasks（pid/log_tail）
                                                     ▼
                                          宿主工具链（npm/curl/uv/cargo）
```

## 3. 路由表（16 条，component="agenthub"；2 条来自 agenthub_toolchain 子模块，3 条 Web 界面管理）

| method | path | 鉴权 | 动作 |
|--------|------|------|------|
| GET | `/api/v1/agenthub/agents` | 公开 | 目录（预置+自定义，含 installed 探测；`?category=` / `?installed=1` / `?source=` 过滤）|
| GET | `/api/v1/agenthub/agents/:id` | 公开 | 单 agent 详情 |
| GET | `/api/v1/agenthub/installed` | 公开 | 已安装列表（command -v 探测）|
| GET | `/api/v1/agenthub/toolchains` | 公开 | 工具链可用性（node/npm/uv/cargo/curl，含版本首行）|
| POST | `/api/v1/agenthub/install` | admin | 一键安装（后台任务，201 返回任务对象）|
| POST | `/api/v1/agenthub/uninstall` | admin | 卸载（script 渠道 400 明确拒绝）|
| GET | `/api/v1/agenthub/tasks` | 公开 | 任务列表（内存态，≤100 条，重启即清）|
| GET | `/api/v1/agenthub/tasks/:id` | 公开 | 任务详情（含 log_tail）|
| POST | `/api/v1/agenthub/publish` | admin | 发布自定义 agent（持久化）|
| DELETE | `/api/v1/agenthub/published/:id` | admin | 删自定义 agent（预置条目 404）|
| GET | `/api/v1/agenthub/stats` | 公开 | 聚合统计（total/installed/toolchains_ready/tasks）|
| POST | `/api/v1/agenthub/toolchain/install` | admin | 工具链手动安装 → `202 {task_id, toolchain, status}`（body `{name: "node"\|"uv"\|"cargo"}`；非法 name 400 / 重复 running 任务 409 / 已装探测命中返回 status=done）|
| GET | `/api/v1/agenthub/toolchain/install/tasks/:id` | 公开 | 工具链安装任务详情（`{id, toolchain, status, log[], started_at, finished_at}`，环形日志 200 行）|
| POST | `/api/v1/agenthub/web/:agentId/start` | admin | 启动 agent Web 服务（仅 web 描述符标注的 agent；契约见 §1.3）|
| POST | `/api/v1/agenthub/web/:agentId/stop` | admin | 停止 agent Web 服务 |
| GET | `/api/v1/agenthub/web/:agentId/status` | 公开 | agent Web 服务状态（含 URL 与日志尾）|

## 4. env

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_AGENTHUB_FILE` | `/tank/os-data/agenthub.json` | 自定义 agent 持久化文件（原子写：先 `.tmp` 再 rename；读取缺失/损坏降级空态）|
| `NEXOS_AGENTHUB_NPM_SUDO` | （auto）| npm 渠道是否前置 sudo：`always` / `never` / 未设=自动探测 npm 前缀写权限 |
| `NEXOS_AGENTHUB_HOME` | `$HOME` → `/home/$USER` → `/root` | 工具链安装/探测根目录（运维诊断与测试注入用）|
| `NEXOS_AGENTHUB_NVM_INSTALL_URL` | ghfast 镜像 → 官方 install.sh | nvm 安装脚本 URL 覆盖（设置后单次尝试，不再叠加镜像/回退链）|
| `NEXOS_AGENTHUB_NVM_NODE_MIRROR` | `https://npmmirror.com/mirrors/node` | node 二进制下载镜像（透传 `NVM_NODEJS_ORG_MIRROR`）|
| `NEXOS_AGENTHUB_UV_INSTALL_URL` | `https://astral.sh/uv/install.sh` | uv 安装脚本 URL 覆盖（设置后失败不回退 ghfast）|
| `NEXOS_AGENTHUB_RUSTUP_INSTALL_URL` | `https://sh.rustup.rs` | rustup 安装脚本 URL 覆盖（设置后不走清华镜像 env）|

## 5. 任务状态机与降级语义

- agent 安装/卸载任务：`pending` → `running` → `completed` / `failed`（未知渠道
  直接 `failed` 不 spawn；spawn 失败 → `failed` + "命令启动失败（xx 可能未
  安装）"）。
- 工具链安装任务（子模块）：`running` → `done` / `error`（安装命令非 0 退出/
  超时 30min kill/产物校验失败 → `error`，环形日志留全过程）；幂等探测命中 →
  建任务即 `done`（log 提示「已安装」）；同工具链 running 任务重复 POST 409。
- Web 服务进程（§1.3）：无任务态机，进程表 + 端口探测对账——start
  `started → idempotent | recovered`（或 500 失败清表）；status 对死条目自动
  清理（端口关即亡）；stop 未在运行 404、端口仍占 500。进程退出码不回写
  （服务常驻），退出由端口探测发现。
- script 渠道**无卸载命令**：uninstall 在 HTTP 层 400（不建任务）；前端对已装
  script 条目隐藏卸载按钮并提示。
- 已安装探测/工具链探测失败一律降级（空列表 / available=false），不 panic
  不 500——与 app_store 的降级语义一致。
- 任务列表内存态上限 100 条（agent 任务）/ 进程内 HashMap（工具链任务，重启
  即清）；`/agents` 的 `installed` 每次实时探测，不缓存（`command -v` 本地毫秒级）。

## 6. 安全注记

- `check_binary` 白名单校验（`is_safe_binary_name`）：字母数字与 `. _ -`，
  ≤64 字符——`command -v` 拼壳前强制通过，自定义发布同样校验。
- `install_target` 校验（`is_valid_target`）：script 渠道必须 https?:// 开头；
  其余渠道禁止空白与控制字符，长度 ≤512。
- 安装命令白名单构造（`build_install_cmd`）：程序名固定为 npm/bash/uv/cargo
  四选一，目标仅作为参数传入，无任意 shell 拼接。
- 工具链安装（`agenthub_toolchain`）：name 白名单 `node|uv|cargo` 三选一；
  安装脚本 URL 全部为代码常量 / `NEXOS_AGENTHUB_*` env 派生（运维显式控制），
  无用户可控 URL 拼接面；落点固定在用户 HOME 下，不触系统目录。
- 写操作全部要求 admin Bearer（网关 `NEXOS_ADMIN_TOKEN`）。

## 7. 前端

`AgentHub.vue` 三 Tab：**集合**（统计卡 + 工具链徽章行 + 工具链安装入口 +
分类 chips + 搜索 + agent 卡片网格，安装/卸载按钮按渠道工具链可用性禁用）/
**任务**（DataTable + 可展开日志，活跃任务 3s 轮询）/ **自定义**（发布表单 +
我发布的列表）。

工具链手动安装交互：工具链徽章行对缺失项（node 覆盖 node+npm / uv / cargo）
显示「安装」按钮 → `window.confirm` 确认（说明装到用户目录 `~/.nvm` 等、预计
下载量、无需 sudo）→ `POST /toolchain/install` → 集合页内任务面板（照推理
环境任务面板样式）2s 轮询环形日志、自动滚底 → 终态停轮询并刷新
`GET /toolchains` + `GET /stats`（渠道按钮解禁）。409（重复任务）等错误经
全局消息条展示。新增文案 i18n 四语言（`agentHub.*` 键：zh-CN / zh-TW 繁化
「工具鏈/安裝」/ en-US / ja-JP）。

Web 界面交互（§1.3）：已装且带 `web` 描述符的卡片显示**状态点**（绿=运行中/
灰=未运行）+「打开界面」按钮（tooltip 显示描述符 note，即鉴权形态记录）——
未跑点击 → `POST /web/:id/start`（按钮转圈「启动中…」）→ 成功
`window.open(url)` 新标签直达；已跑点击直接 open；旁附「停止」小按钮（运行
中才显示，confirm 后 POST stop）。状态 5s 轮询（仅集合页可见且有 web agent
时；切 Tab/卸载页面即歇）；启动失败的错误串内嵌后端日志尾，经全局消息条
展示。文案 i18n 四语言（`agentHub.openUi` 等 9 键：zh-TW 繁化「開啟介面/
啟動/停止/執行中」）。

注册链：`appRegistry.ts`（registry + desktopApps + APP_CATEGORY=devtools）→
`router/index.ts`（/agenthub）→ `DashboardView.vue`（allApps + 内联 SVG 机器人
头像+下载箭头）→ `AppIcon.vue`（ICONS 同款 SVG）→ `client.ts`
（agentHub* 16 个 endpoint 封装）。

## 8. 边界与已知限制

- **用户态无 sudo**：os-api 进程无 root，工具链一律装用户目录；需要系统级
  node 的场景请手工 apt/_nodesource 安装。
- **中国镜像优先**：GitHub 直连不可达的节点（Spark aarch 实测）依赖 ghfast.top
  （nvm 脚本 / uv Releases）与清华 TUNA（rustup dist）；两者都失败时回退官方
  源重试一次，仍失败任务 `error` 并留完整日志。
- **nvm 残留边界**：nvm v0.40.1 安装脚本的 `nvm-exec` / `bash_completion` 两个
  小文件在 `METHOD=script` 时始终走官方 `raw.githubusercontent.com`（脚本自身
  逻辑，`NVM_SOURCE` 不覆盖）——GitHub 完全断连的机器上镜像尝试可能卡在这两
  个 ~2KB 文件上，此时用 `NEXOS_AGENTHUB_NVM_INSTALL_URL` 整链覆盖自建镜像。
- **rustup 镜像细节**：脚本本体 `sh.rustup.rs` 始终直连（TUNA 未镜像该入口，
  仅 `RUSTUP_DIST_SERVER`/`RUSTUP_UPDATE_ROOT` 覆盖 dist 与 rustup-init 下载）。
- 工具链安装任务态进程内（重启即清）；安装物在磁盘，重启后页面探测自会命中。
- **Web 服务进程生命周期**（§1.3）：子进程随 spawn 而生但**不随 os-api 退出
  而亡**（reparent 到 init）——重启后进程表丢失，靠端口探测兜底重建（pid/
  启动时间如实置空，stop 走 fuser 按端口杀，fuser 缺失时报错）；描述符端口
  固定，被**其它进程**占用时恢复机制会认领该端口（无 per-agent 指纹）。
- **无鉴权透传**：OpenCode serve 默认无 token（`OPENCODE_SERVER_PASSWORD`
  未设即开放，OpenCode 自身行为）——本组件不注入该 env、不在 URL 里拼 token，
  需要鉴权的运维请给 os-api 服务自行配置；`/web/:id/status` 为公开端点，只
  暴露运行态/URL/日志尾，不含敏感信息。
- 真实 OpenCode 安装/serve 不进单元测试（外部依赖 + 网络）；开发实测记录在
  §1.3，测试一律走 `WebLauncher` mock 注入。
