# LLM 推理环境（vLLM Python venv 管理）

> 2026-08-31 新增。模型管理桌面应用（`/llm` 页）新增「推理环境」能力：把
> 「Python venv + 指定版本 vLLM」做成可管理的资源——多环境并存、一个默认
> 环境供实例拉起、页面上一键创建/更新。
>
> 2026-09-02 新增**渠道（channel）**：创建/更新请求体可选 `channel` =
> `stable`（默认，行为与历史完全一致）或 `nightly`（预置示例，见 §2.1）。
>
> 2026-09-03 uv 定位链**多用户扩展**（Spark 实测缺陷）：uv 官方安装器装进
> 交互用户 `~/.local/bin`（如 `/home/nvidia/.local/bin/uv`），服务进程却常跑
> root（home=/root）——原链 PATH → 运行用户 `~/.local/bin` 之后直接走自动安装，
> 已装的 uv 探测不到。现链在运行用户落点之后插入 `/home/*/.local/bin` glob
> 全用户扫描（找到即可用；envs 仍装运行用户 home，语义不变），env 见 §3。
>
> 动机：os-api 的 llm handler 原来硬编码 `/home/oem/vllm-env/bin/vllm` 拉起
> vLLM；机器重装系统后该 venv 丢失，重建/升级 vLLM 只能手动敲 uv 命令。本功能
> 将该流程产品化。

## 1. 功能定位

- **推理环境** = `<root>/<name>/` 一个 uv 管理的 Python venv，内装指定版本
  vLLM（`uv venv` + `uv pip install vllm==<ver>`）。
- 多环境并存：可以同时维护 `vllm-026`（vLLM 0.26.0）、`vllm-latest` 等多套
  环境，互不影响；实例创建时可指定用哪套。
- **默认环境**：`llm_environments.is_default=1` 且 `status=ready` 的行。
  实例拉起（`POST /llm/instances/:id/start` 与 autostart）不再用硬编码路径，
  改为解析默认环境的 `<path>/bin/vllm`；注册表无可用默认行时**回退旧硬编码
  `/home/oem/vllm-env/bin/vllm`**（向后兼容存量部署，别破坏未迁移机器）。
- 创建/更新是分钟级长任务（下载 CPython + 数 GB 的 vLLM/CUDA 轮子）：接口
  立即返回 `202 {task_id}`，后台线程执行，前端轮询任务详情看日志尾。

## 2. 端点契约（7 条，挂进 llm handler 的 `routes()`）

代码：`crates/os-api/src/handlers/llm_envs.rs`（子模块，`llm.rs` 委托）。
写接口（POST/DELETE）需系统 admin；GET 公开读。响应错误统一 `{"error": "..."}`。

| method | path | 权限 | 请求 | 响应（例） |
|--------|------|------|------|------------|
| GET | `/api/v1/llm/environments` | 公开 | — | `{"environments":[{...}],"default_name":"main"}` |
| POST | `/api/v1/llm/environments` | admin | `{"name":"main","python_version":"3.12","vllm_version":"latest","channel":"stable"}`（后三字段可省） | `202 {"task_id":"envtask-1","env_name":"main","status":"creating","channel":"stable"}` |
| POST | `/api/v1/llm/environments/:name/update` | admin | `{"vllm_version":"0.27.0","channel":"nightly"}`（均可省：版本=latest、渠道=沿用当前；body 可为 null） | `202 {"task_id":"envtask-2","env_name":"main","status":"updating","channel":"nightly"}` |
| DELETE | `/api/v1/llm/environments/:name` | admin | — | `{"ok":true,"name":"spare","removed_path":"~/llm-envs/spare","rm_error":null}` |
| POST | `/api/v1/llm/environments/:name/default` | admin | — | `{"ok":true,"default_name":"second"}` |
| GET | `/api/v1/llm/environments/tasks` | 公开 | — | `{"tasks":[{id,kind,env_name,status,started_at,finished_at}]}`（不带日志） |
| GET | `/api/v1/llm/environments/tasks/:id` | 公开 | — | `{"id":"envtask-1","kind":"create","env_name":"main","status":"done","started_at":1756627200,"finished_at":1756628900,"log":["$ uv venv --python 3.12 ~/llm-envs/main",...]}` |

### 环境行字段（`environments[]` 元素）

```json
{
  "name": "main",
  "path": "/home/oem/llm-envs/main",
  "python_version": "3.12",
  "vllm_version_requested": "latest",
  "vllm_version_installed": "0.26.0",
  "channel": "stable",
  "is_default": true,
  "status": "ready",
  "size_bytes": 6442450944,
  "created_at": 1756627200,
  "updated_at": 1756628900,
  "last_error": null
}
```

- `status` ∈ `creating` | `updating` | `ready` | `error`（error 时 `last_error`
  带原因，任务日志里有完整命令输出）。
- `channel` ∈ `stable` | `nightly`（2026-09-02 起；**存量行/缺省一律 stable**，
  旧库经幂等 `ALTER TABLE ... ADD COLUMN channel TEXT` 迁移，NULL 读取兜底
  stable）。
- `created_at`/`updated_at` 为 Unix epoch 秒；`vllm_version_installed` 来自
  `<env>/bin/python -c "import importlib.metadata;..."` 的真实输出，`size_bytes`
  为目录递归 stat 求和——**无估算、无占位**。

### 2.1 渠道（channel）契约与 nightly 预置示例

- **契约**：`channel` 可选值仅 `stable` | `nightly`（大小写敏感，非法 400，
  create 与 update 双端点同校验）。create 缺省/空白 = `stable`；update 缺省 =
  **沿用该行当前渠道**（版本字段缺省则 = latest）。update 可 nightly↔stable
  互切（切换即按新渠道重装）。
- **stable（默认，零变化）**：`uv pip install [-U] --python <env>/bin/python
  vllm[==<ver>]`，`vllm_version` 语义不变（latest/具体版本钉版本）。
- **nightly（预置示例，恒最新）**：基于用户点名命令
  `uv pip install -U vllm --torch-backend=auto --extra-index-url https://wheels.vllm.ai/nightly`
  ——实际执行按环境加 `--python`：

  ```
  uv pip install --python <env>/bin/python -U vllm \
      --torch-backend=auto --extra-index-url https://wheels.vllm.ai/nightly
  ```

  - **不钉版本**：nightly 恒装最新（`vllm_version` 参数被忽略，注册表
    `vllm_version_requested` 规范化为 `latest`）；`--torch-backend=auto`
    让 uv 按本机 CUDA 自动选 torch 轮子后端。
  - **源顺序语义**：`https://wheels.vllm.ai/nightly` 是主源——vLLM 每日构建
    只发在该源，PyPI/镜像源没有。`NEXOS_LLM_PIP_INDEX_URL` 设置时**不**走
    `UV_PIP_INDEX_URL` env（那会把镜像顶成主源、抢走 vllm 的解析优先级），
    而是追加为**第二个** `--extra-index-url` 排在 nightly 源之后——uv 默认
    first-index 策略下顺序即优先级：vllm nightly 轮子命中 nightly 源，其余
    PyPI 依赖轮子（torch 等）落到镜像/官方源下载（镜像做 PyPI 兜底）。
  - **风险提示**：nightly 无 SLA——每日构建可能引入破坏性变更/回归，也可能
    与已装 CUDA 驱动不匹配；**生产环境建议 stable**，nightly 仅用于尝鲜/
    复现上游 nightly 问题。出问题可随时 update 切回 stable（钉回具体版本）。
- **前端**：创建表单与更新对话框均有渠道下拉（stable 默认 / nightly 预置
  示例）；选 nightly 时版本输入禁用（灰掉 + 「恒装最新」说明），下方展示与
  后端 argv 同构的完整命令预览（等宽代码块）；环境卡 nightly 蓝色渠道徽章
  （stable 无徽章）。

### 关键校验与错误码

- name 合法性：`^[a-z0-9][a-z0-9-]{0,31}$`（400；防路径穿越与怪目录名）。
- channel 合法性：仅 `stable` | `nightly`（400，大小写敏感）。
- 重名：409（`llm_environments.name` UNIQUE 兜底并发）。
- **首个创建的环境自动设为默认**（插入时表空即 `is_default=1`）。
- 删除：默认环境 409（先切默认）；环境上有 running 任务 409（防 rm -rf 打断
  安装）；环境不存在 404。删除 = 删注册表行 + `rm -rf` venv 目录（尽力而为，
  目录删除失败不阻塞删行，`rm_error` 带回）。
- 切默认：事务内先全清 `is_default` 再单设（互斥，至多一行默认）。
- `POST /llm/instances` 请求体新增可选 `env_name`：指定环境须存在于注册表
  （否则 400）；spawn 时该环境必须 `ready`（否则实例进 error 带原因——显式
  指定不静默回退）。实例表 `llm_instances` 新增可空列 `env_name`（存量库
  ALTER TABLE 幂等迁移）。

## 3. env 清单

| 变量 | 默认值 | 作用 | 注入点 |
|------|--------|------|--------|
| `NEXOS_LLM_ENVS_ROOT` | `~/llm-envs`（home_dir 解析，目录不存在自动创建） | venv 根目录，每个环境在 `<root>/<name>/` | `llm_envs.rs` `default_envs_root()`（handler 构造时读取） |
| `NEXOS_LLM_UV_BIN` | 空（解析链：PATH 里的 `uv` → 运行用户 `~/.local/bin/uv` → `/home/*` 多用户 glob → `/root/.local/bin/uv` → 自动安装；见 §4 拓扑） | uv 绝对路径覆盖 | `llm_envs.rs` `locate_uv()`（任务线程内） |
| `NEXOS_LLM_UV_SCAN_ROOT` | `/home` | uv 多用户 glob 基座（`<root>/*/.local/bin/uv`）。生产缺省扫全部桌面用户家目录；测试注入 tempdir 造多用户布局（不碰真机 /home），特殊家目录布局的机器也可指到自己的位置 | `llm_envs.rs` `locate_uv()` |
| `NEXOS_LLM_UV_INSTALL_URL` | `https://astral.sh/uv/install.sh` | 找不到 uv 时自动安装脚本的 URL（`curl -LsSf <url> \| sh`，安装过程也写任务日志） | `llm_envs.rs` `locate_uv()` |
| `NEXOS_LLM_PIP_INDEX_URL` | 空（=官方 PyPI） | pip 镜像源。**stable**：以 `UV_PIP_INDEX_URL` + `PIP_INDEX_URL` 两组 env 透传给 `uv pip install`（镜像即主源）；**nightly**：改追加第二个 `--extra-index-url` 排在 nightly 源之后（nightly 主、镜像兜底 PyPI，见 §2.1） | `llm_envs.rs` `pip_install_argv()` / `nightly_extra_args()` |

> 106 机器部署建议：`NEXOS_LLM_PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple`
> （清华源），可把 vLLM 轮子下载从小时级缩到分钟级。

## 4. 拓扑（ASCII）

```
 前端 LlmModels.vue「推理环境」Tab
   │  创建/更新/切默认/删除        轮询 2s（任务日志尾）
   ▼                                   ▼
 REST /api/v1/llm/environments*  ──►  /environments/tasks[/:id]
   │  (llm.rs handle() 前缀匹配整体委托)
   ▼
 llm_envs.rs  LlmEnvState（与 llm.rs 共享同一条 llm.db 连接）
   │  POST create/update：注册表插行(creating/updating) → 202 {task_id}
   │                        └─► 后台 std 线程（进程内 EnvTask 注册表 + 环形日志 200 行）
   │                             │
   │                             ▼
   │                        EnvExecutor 抽象（生产 UvExecutor：std::process + 分类限时
   │                        30min venv/pip、2min 探测、5min uv 自安装，超时 kill）
   │                             │
   │                             ▼
   │                        uv 定位（每环命中路径写任务日志，可追溯）
   │                        NEXOS_LLM_UV_BIN → PATH → 运行用户 ~/.local/bin
   │                          → /home/*/.local/bin 多用户 glob（2026-09-03：Spark 实测
   │                            uv 装在 /home/nvidia/.local/bin 而服务跑 root——找到即可用，
   │                            uv 二进制跨用户调用没问题；envs 仍装运行用户 home）
   │                          → /root/.local/bin 兜底 → curl|sh 自动安装（复查 ~/.local/bin）
   │                        1) uv venv --python <pv>  ~/llm-envs/<name>/
   │                        2) uv pip install [--python <env>/bin/python] [-U] vllm[==<ver>]
   │                           stable：镜像源经 UV_PIP_INDEX_URL/PIP_INDEX_URL env 透传（主源）
   │                           nightly：-U vllm --torch-backend=auto
   │                             --extra-index-url https://wheels.vllm.ai/nightly
   │                             [+ 第二个 --extra-index-url <镜像>（PyPI 兜底，nightly 主源在前）]
   │                        3) <env>/bin/python -c "importlib.metadata.version('vllm')"
   │                        4) 目录递归求 size → 注册表置 ready / error(last_error)
   ▼
 llm.db（SQLite WAL）
   ├── llm_environments（注册表：name/path/版本/渠道/默认/状态/大小/错误）
   └── llm_instances（实例表，新增可空列 env_name）

 实例拉起旁路（default_env_bin）：
   POST /llm/instances/:id/start ──► env_bin_for(env_name?)
        ├─ env_name=None → 注册表 is_default=1 且 ready 的行 → <path>/bin/vllm
        │                  （无可用默认行 → 回退旧硬编码 /home/oem/vllm-env/bin/vllm）
        └─ env_name=Some  → 该环境行（须存在且 ready，否则实例 error）
        spawn 时 PATH 注入 <env>/bin（vLLM 编译 CUDA kernel 需要 ninja）
```

## 5. 操作手册

1. **创建**：模型管理 → 「推理环境」Tab → 填名称（如 `main`）、Python 版本
   （3.10/3.11/3.12/3.13，uv 自动下载对应 CPython）、渠道（稳定版=默认 /
   Nightly=预置示例）、vLLM 版本（留空 = latest；nightly 时禁用——恒装最新）
   → 「创建环境」。表单下方实时显示将执行的完整安装命令。返回 202，任务面板
   开始 2s 轮询，日志尾自动滚动。
2. **等 ready**：环境卡状态徽标由蓝（创建中）变绿（就绪）；首个创建的环境
   自动成为默认（紫色徽标）；nightly 环境带蓝色 Nightly 渠道徽章。失败变红，
   卡上显示错误摘要，任务日志有完整 uv/pip 输出。
3. **（可选）更新**：环境卡「更新」→ 选渠道（可 nightly↔stable 切换重装）
   + 填目标版本（stable 默认 latest；nightly 禁用）→ 后台 `uv pip install
   -U`（命令预览联动）；stable 下已装版本与目标版本不一致时卡片有 ⚠ 提示。
4. **（可选）切换默认**：另一环境卡「设为默认」（互斥切换）。
5. **拉实例**：「实例管理」Tab 创建实例时「推理环境」下拉可选指定环境
   （默认空 = 默认环境）；启动实例即用对应 venv 的 `bin/vllm` 拉起。
6. **删除**：非默认环境可删（confirm 后删行 + rm -rf venv）；默认环境需先
   把别的环境设为默认。

排错：
- 环境卡 error → 看卡上 `last_error` 摘要与任务面板日志（uv/pip 完整输出）；
  网络类失败多为镜像源未配（配 `NEXOS_LLM_PIP_INDEX_URL` 后重试更新）。
- 服务重启后任务面板清空是预期（任务态在进程内）；DB 里停在
  creating/updating 的环境对同一环境再发一次「更新」即可修复。
- uv 未安装：首次创建任务会自动 `curl -LsSf https://astral.sh/uv/install.sh | sh`
  （离线机器请预装并用 `NEXOS_LLM_UV_BIN` 指定路径）。

## 6. 真实数据声明与避坑

- **生产零演示数据**：环境列表/任务面板只展示 `llm_environments` 注册表与
  进程内任务态的真实内容；`llm_envs.rs` 不 seed 任何演示环境行。mock 执行器
  （AlwaysOk/Echo/FailOnArg/ProbeVersion）只存在于 `#[cfg(test)]`，测试绝不
  真跑 uv / 网络 / rm 生产目录（测试注入临时根目录 + 固定 uv 路径）。
- **版本/大小不虚构**：`vllm_version_installed` = importlib.metadata 真实
  输出；`size_bytes` = 目录递归 stat 求和；任务日志 = 命令真实 stdout/stderr
  截尾（每条命令输出截尾 4000 字符防爆内存，日志环形 200 行）。
- **与实例监控隔离**：本功能不读取 `NEXOS_LLM_METRICS_SIMULATE`，模拟链路
  不影响环境/实例状态；InstanceMonitor 组件零改动（仅卡片 CSS 风格参照）。
- **uv 安装避坑**：uv 自安装走 `sh -c "curl ... | sh"`，需机器可出网；
  无外网时预装 uv 并设 `NEXOS_LLM_UV_BIN`。
- **镜像源避坑**：vLLM + CUDA 轮子数 GB，官方源慢/易断；国内机器务必配
  `NEXOS_LLM_PIP_INDEX_URL`。断流失败会落 error + last_error，直接重发更新。
- **超时防挂死**：`uv venv`/`uv pip install` 每条命令限时 30 分钟、版本探测
  2 分钟、uv 自安装 5 分钟，超时 kill 子进程并落 error（不会卡死任务线程）。
- **互斥防呆**：同一环境存在 running 任务时，创建（重名）/更新/删除均 409；
  默认环境删除 409；`default` 切换在 SQLite 事务内互斥。
- **日志纪律**：进程内日志一律 `eprintln!`（`[llm-env]` 前缀），不用 tracing
  （os-api 无 subscriber，tracing 无声）。

## 7. 存储

`llm.db`（`NEXOS_LLM_DB` 覆盖，默认 `/tank/os-data/llm.db` → `/var/lib/os/llm.db`
→ `./llm.db`）新表（建表幂等，llm.rs `create_schema` 一并执行）：

```sql
CREATE TABLE IF NOT EXISTS llm_environments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL,
    path TEXT NOT NULL,
    python_version TEXT,
    vllm_version_requested TEXT,
    vllm_version_installed TEXT,
    channel TEXT,
    is_default INTEGER DEFAULT 0,
    status TEXT DEFAULT 'creating',
    created_at INTEGER,
    updated_at INTEGER,
    last_error TEXT,
    size_bytes INTEGER DEFAULT 0
);
```

迁移（幂等，`create_env_schema` 内执行，llm.rs `create_schema` 一并调用）：
`ALTER TABLE llm_environments ADD COLUMN channel TEXT`——列已存在时 ALTER 报
duplicate column 忽略（`llm_instances` 的 env_name/launch_command 同款惯例）；
存量行 NULL 由 `env_list` 读取时兜底 `stable`。

`llm_instances` 同批新增可空列：`ALTER TABLE llm_instances ADD COLUMN env_name TEXT`
（列已存在时忽略，幂等；forwarding.rs 同款迁移惯例）。

## 8. 测试

`llm_envs.rs` / `llm.rs` 内联 `#[cfg(test)]`（2026-09-03 uv 多用户定位落地后
llm_envs 33 用例全绿；此前 2026-09-02 渠道功能落地时 os-api lib 全量 1575
通过零回归；llm_envs 27 用例 + llm 集成 104 用例。2026-08-31 首版新增 25 例，
渠道功能再增 8 例，uv 多用户再增 6 例）：

- **uv 定位链（2026-09-03 新增 6 例）**：多用户 glob 过滤（带执行位普通文件
  才命中 / 无执行位与目录跳过 / 按用户名排序）；`NEXOS_LLM_UV_BIN` 覆盖优先
  且命中路径写任务日志；多用户 glob 命中（运行用户 miss 后在
  `/home/*/.local/bin` 找到——Spark 形态）；运行用户 `~/.local/bin` 优先于
  glob；全链未命中 → curl|sh 自动安装（mock 执行器在 HOME 落点造 uv）→
  复查通过返回；安装命令失败 → Err 传播。定位链内核 `locate_uv_in` 参数化
  （path_dirs/home/user_homes/root 兜底全注入 tempdir 合成值、不读进程 env）——
  不碰真机 `/home`、`/root`，也**不改写 PATH/HOME**（那是进程全局变量，同
  二进制并行的 provisioning/http 测试 spawn git/ssh 会读，改写会交叉污染）。

- 纯函数：环境名/版本号/渠道校验（含路径穿越拒绝、channel 大小写敏感与
  空白裁剪）、`vllm_spec`、安装命令构造（stable 逐字零回归 / nightly 与
  预置示例逐 argv 相等 / 镜像叠加顺序 nightly 主-镜像兜底 / stable 镜像走
  env_kv）、日志环形截断、`dir_size` 递归求和、`UvExecutor::timeout_for`
  分类限时。
- mock 执行器驱动全流转：创建 creating→ready（首个自动默认 + 版本探测值落
  库）、创建失败→error+last_error、更新带 `-U` 且记录新版本、任务列表/详情
  含日志与 Echo 断言命令构造；nightly 创建任务日志含完整预置命令
  （`-U vllm --torch-backend=auto --extra-index-url <nightly>`、无 `==` 钉
  版本）；update 渠道切换（stable→nightly 命令换形态、缺省沿用当前渠道、
  nightly→stable 恢复钉版本）；存量表无 channel 列迁移后旧行读作 stable。
- 契约：409 重名 / 默认环境拒删 / 默认互斥切换 / 404 缺失 / 400 非法名与
  版本与渠道。
- 集成（llm.rs）：`routes()` 全量声明（写接口 admin、GET 公开）、
  environments 路由委托、`default_env_bin` 注册表解析与旧路径回退、
  `env_name` 实例创建校验/落表/重启恢复。
