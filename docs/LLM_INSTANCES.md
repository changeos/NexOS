# LLM 实例管理：端口选取 / 拉起日志 / 换口重试（LLM Instances）

> 源码：`crates/os-api/src/handlers/llm.rs`（handler 组件名 `llm`）。
> 本文覆盖**实例生命周期**的四块新能力（2026-08-31）：自动选口的真实占用
> 检测、创建时手动选口、按实例拉起日志、spawn 失败换口重试；并记录三个
> 生产缺陷的修复（端口双真相源 / starting 卡死 / reasoning 键名）。
> 实例定义持久化与监控指标是另外两篇：docs/LLM_MONITORING.md §4、§1。

## 1. 端点契约（本批新增/变更）

| method | path | 鉴权 | 说明 |
|--------|------|------|------|
| GET | `/api/v1/llm/instances/:id/log?tail=200&follow=0` | 公开读（对齐 metrics） | 实例拉起日志尾 N 行 |

`POST /api/v1/llm/instances` 请求体新增**可选顶层字段 `port`**（与既有
`config` 平级；`config.port` 仍为占位，后端以顶层 `port`/自动选口为准并回写
两处）：

| 字段 | 类型 | 语义 |
|------|------|------|
| `port` | `Option<u16>` | 缺省 = 自动选口（实例表去重 + 真实试绑，8123 起）。给定则校验：1024..=65535（违反 400）；不与实例表冲突、不在保留段 8558/7070/11080/11081、真实试绑通过（违反 409，均带可读原因） |

保留段是 OS 自身服务：8558 = os-api HTTP（provisioning 缺省）、7070 =
os-p2p overlay、11080/11081 = 网络出口双端 SOCKS5（入口/出口代拨）。

`GET /instances/:id/log` 响应（`InstanceLogResponse`）：

| 字段 | 类型 | 说明 |
|------|------|------|
| `instance_id` | String | 实例 id |
| `lines` | String[] | 日志尾 N 行（按文件顺序；默认 200、上限 1000，单次读取 ≤256KB，非法/0 值回默认） |
| `file` | String | 日志文件绝对路径（排查用） |
| `status` | String | 实例当前 status（starting 时看启动进度最常用） |

404 语义：实例不存在，或日志文件尚未生成（实例从未拉起过）。`follow`
参数当前为拉取式实现（响应同构），持续跟随由前端 2s 轮询完成。

`POST /instances/:id/chat` 响应新增字段（reasoning 双键兼容，见 §6）：
`reasoning`（思考段，vLLM 0.28 `reasoning` / 0.27 `reasoning_content`
归一）、`finish_reason`、`total_tokens`；`content` 可为空串（思考段吃满
时不是错误）。

创建/启动响应（`ModelInstance`）**透出最终端口** `port`（顶层）——注意
换口重试是后台异步的，重试发生后实例行与 DB 的 port 才更新为最终值，前端
以列表轮询看到的为准。

### `launch_command`（2026-08-31，接入说明面板「启动参数」块）

`GET /instances`、`GET /instances/:id`、`POST /instances`、`/:id/start`、
`/:id/stop` 的实例 JSON **恒带** `launch_command`（响应时注入，非 DB 行原生）：

- 曾拉起 → 最近一次**真实 argv**（`<venv>/bin/vllm serve …`，含推理环境二进制
  路径；spawn 时落库列 `launch_command`，换口重试同步替换 `--port`，重启恢复
  保留）；
- 从未拉起 → 服务端按当前 config 用 `build_vllm_serve_cmd`（与 spawn 同函数
  同参）构造，二进制为 `vllm` 占位名。

详见 docs/LLM_EXTERNAL_APIS.md §6。

## 2. 环境变量

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_LLM_SPAWN_DIR` | `/tmp` | 实例拉起日志目录；文件名 `llm-vllm-<instance_id>.log`（stdout+stderr 同文件，append 模式跨次拉起累积） |
| `NEXOS_LLM_SPAWN_LOG` | 未设置 | **旧单文件模式（向后兼容）**：设置后所有实例共写这一个文件（等同旧行为）。限制：按实例日志端点 `/instances/:id/log` 此时返回的是共享文件内容，多实例混杂失真——生产建议改用 `NEXOS_LLM_SPAWN_DIR` |

优先级：`NEXOS_LLM_SPAWN_LOG` > `NEXOS_LLM_SPAWN_DIR`。两个 env 在 handler
构造时读一次（运行期不再碰 env）；测试用 `with_spawn_log_dir(dir)` 注入。

## 3. 拓扑：创建 → 选口（试绑）→ spawn → 日志文件 → 重试环

```
POST /instances {name, model, port?}
        │
        ├─ port 给定 ──► validate_manual_port
        │                 ├─ 1024..=65535 ?          否 → 400
        │                 ├─ 保留段 8558/7070/11080/11081 ? → 409
        │                 ├─ 实例表已占 ?             是 → 409（带实例 id）
        │                 └─ TcpListener::bind 0.0.0.0:p 试绑 失败 → 409（带原因）
        │
        └─ port 缺省 ──► pick_free_port（8123 起递增）
                          └─ 候选口：实例表未占 && bind 0.0.0.0:p 试绑通过
                             （TOCTOU：试绑 drop 与 vLLM 真绑之间仍可能被抢，
                               由下方 30s 监测环兜底）
        │
        ▼
   行 port 唯一真相源：row.port = p；config.port = p（随写镜像）
        │
        ▼ （autostart / POST :id/start）
   spawn vllm serve --host .. --port <row.port>（参数强制取行 port）
        │   stdout+stderr ──► <SPAWN_DIR>/llm-vllm-<id>.log
        │
        ├─ 命令不存在/启动失败 → status=error（原错误路径）
        │
        └─ Ok(pid) → status=starting
                │
                ▼
        后台监测任务（30s 窗口，500ms 轮询 child.try_wait）
                │
                ├─ 仍在跑 ──────────────► 继续等（加载模型可分钟级）
                │                            └─ 窗口结束 → 正常退出监测
                ├─ 退出 + 日志尾含
                │  "Address already in use"/Errno 98
                │        │
                │        ├─ 未重试过 ──► pick_free_port 选下一个真实空闲口
                │        │               日志追加 "=== 端口 X 被占用，自动换口 Y 重试 ==="
                │        │               spawn_fn(config{port:Y}) ──► 成功：
                │        │                   行 port 与 config.port 同步 = Y、pid 更新、
                │        │                   status 保持 starting、落库；继续监测新子进程
                │        │                失败：status=error（原错误路径）
                │        └─ 已重试过 ──► status=error（"换口重试后仍失败"）
                └─ 退出 + 无端口占用字样 ──► 不猜原因，保持现状态
                                           （交给列表健康修正 / 用户裁决）
```

列表健康修正（`GET /instances` 每次返回前）同时承担三件事：

- running → /v1/models 探活，死了回落 stopped；
- stopped / **starting** → 端口活且 `/v1/models` 就绪（served_model_name
  匹配）→ 修正 running——**starting 纳入修正是 2026-08-31 缺陷修复**：
  模型加载（实测 19G 权重 ~80s+）远超拉起时的一次性探测窗口，此前会永久
  卡 starting；探测仍不通保持 starting（不猜成 error）；
- **端口收敛**：任何 `config.port != row.port` 的历史双写残留统一收敛到
  行 port 并落库。

## 4. 避坑复盘

### 4.1 8123 被外部进程占用（本批动因）

- 现象：生产 8123 被外部进程占用，旧 `pick_free_port` 只查内存实例表端口，
  照样返回 8123；vLLM 拉起即 `OSError: [Errno 98] Address already in use`
  失败，实例卡 error；且所有实例日志共写 `/tmp/llm-vllm.log`，无法按实例
  排查。
- 修复：选口真实试绑（§3）+ spawn 后 30s 监测换口重试（最多一次）+ 按实例
  日志文件（§2）。

### 4.2 TOCTOU 窗口（已知、有意接受）

- 试绑（bind 后立即 drop）与 vLLM 子进程真绑之间，第三方进程仍可能抢占
  同一端口——这是无法在选口时刻消除的固有竞态。
- 兜底：spawn 后 30s 监测环（§3）。窗口外（>30s 才被抢）极罕见，且列表
  健康修正会把「声称 running 但端口死」的实例回落 stopped，不会静默错乱。
- 试绑口径与 vLLM 一致绑 `0.0.0.0`；仅绑定具体地址（如 127.0.0.1:p）的
  占用方在双方都开 SO_REUSEADDR 时可能探测不到（wildcard/specific 重叠
  规则），该形态罕见，同样由监测环兜底。

### 4.3 端口双真相源（2026-08-31 缺陷）

- 现象：实例端口双存于行字段 `port` 与 `config` JSON 的 `port`——spawn 用
  config.port、健康探测用行 port；手动改库只改其一（行 8124、config 留
  8123）后 vLLM 绑 8123、探测打 8124，实例**永久卡 starting**。
- 修复：**行 port 定为唯一真相源**。spawn 参数、/health、/chat、/metrics、
  网关探测全部取行 port；config.port 只作随写镜像，在创建、落库、重启恢复
  （load_persisted_instances）、列表修正四个写入点同步收敛。换口重试同样
  两处同步写（`apply_spawn_retry_to_row`）。

### 4.4 starting 无再探测（2026-08-31 缺陷）

- 现象：starting→running 只依赖拉起时的健康探测；模型加载超窗（19G 权重
  ~80s+）后即使 vLLM 已就绪（手动 /health 返回 200）也不再有探测翻转，
  checked_at 不再前进，UI 永久「启动中」。
- 修复：列表修正把 starting 纳入探测（探活 + /v1/models 就绪 → running，
  落库）；前端日志抽屉在 starting 时高亮「日志」按钮看启动进度。

### 4.5 reasoning 键名（2026-08-31 缺陷）

- 现象：vLLM 0.28 思考模型推理输出键从 `reasoning_content`（0.27）改名
  `reasoning`；小 `max_tokens`（如 200）下输出全被思考段吃掉、content 为
  null、finish_reason=length——后端报「缺少 content」、前端显示像故障。
- 修复：`POST /:id/chat` 双键归一解析（`reasoning` 或 `reasoning_content`
  → 响应 `reasoning` 字段），content 与 reasoning 双空才报错（错误带
  finish_reason 与 max_tokens 提示）；前端推理测试折叠展示思考段，content
  空且 reasoning 非空时提示「输出全被思考段占用（已用 N token）——可调大
  max_tokens 重试」。

### 4.6 测试与生产端口的相互干扰

- 测试里起的假 vLLM 服务**只绑临时端口**（127.0.0.1:0），绝不固定绑 8123
  ——挂死的测试二进制会把 8123 连同假模型列表泄漏给生产（真 vLLM 绑不上 +
  健康探测误报 + 网关发现假条目，实测踩过；见 llm.rs tests 的
  `spawn_fake_vllm_prefer_8123` 2026-08-31 修正注释）。
- `pick_free_port` 的真实试绑是 bind 后立即 drop，不持有端口，对生产无干扰。

## 5. 前端（LlmModels.vue）

- **实例行「日志」按钮**：右侧抽屉显示日志尾（默认拉 300 行），2s 轮询 +
  自动滚底；「暂停跟随」停轮询翻历史、「清空」只清屏不动文件；实例
  starting 时按钮高亮（看模型加载进度）。
- **创建对话框「端口」输入**：可空 = 自动（placeholder「自动（默认 8123
  起）」）；填了本地先校验 1024-65535，冲突/被占由后端 409 带原因。
- 文案走 i18n 顶层 `llmLog` 命名空间（zh-CN/zh-TW/en-US/ja-JP 四语全量）。

## 6. 测试（llm.rs tests，mock 仅 cfg(test)）

- 真实试绑：先占一个 0.0.0.0 端口再断言选口跳过；表内去重保留；任意环境
  下选出口必须真实可绑。
- 手动端口：合法（201 且两处 port 同源）/ 越界 400 / 表内冲突 409（带实例
  id）/ 保留段 409 / 真实被占 409 / 缺省自动。
- 日志端点：写临时日志文件断言 tail 行数与内容（默认 200 / 指定 10 /
  超限 clamp 1000 / follow 参数容忍）；文件不存在与实例不存在均 404。
- 重试路径：`monitor_addr_in_use` 注入 fake 子进程（真实 `sh -c 'exit 0'` /
  `sleep 5` + 伪造日志含 Errno 98）——换口成功更新两处端口并落库、二度
  占用落 error、非占用退出不动状态；`log_says_addr_in_use` 双方言形态单测。
- 端口收敛：config.port ≠ 行 port 的残留经列表修正 / 重启恢复两路收敛。
- starting 修正：假 /v1/models 服务就绪 → starting 翻 running（落库）；
  端口死保持 starting。
- reasoning：0.28 `reasoning`（content null + finish_reason=length + usage）
  与 0.27 `reasoning_content` 双形态 200 透出；双空才 502（带提示）。
