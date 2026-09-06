# 模型大厅与权重管理（ModelHub Lobby）设计与实施

> 状态：已实施（2026-08-21；2026-08-31 增 D 面在线仓库源）· 文档用途：架构存档 + PPT 素材 + AI agent 接续开发依据
> 代码：`crates/os-api/src/handlers/model_hub.rs`（单文件组件，19 条路由，61 个单测）
> 测试基线：os-api 全绿（D 面净增 12 测试）；workspace clippy -D warnings 零告警

---

## 0. 一页速览（PPT 提取层）

| 维度 | 数值 / 结论 |
|---|---|
| 组件 | `model_hub`（os-api 网关内 RouteHandler，非独立 crate） |
| 路由 | **19 条**（9 条原有 + C 面 8 条 + D 面 2 条；另 2 条原路由语义扩展） |
| 数据持久化 | SQLite `model_lobby` 表（`model_lobby.db`，WAL）；下载任务内存态 |
| 多源并行度 | n 源 = n 个并发文件流（文件级轮转分配），单文件不分段 |
| 传输分块 | peer share 源 4 MiB/块（base64 信封）；在线仓库源 16 MiB/块（bounded Range 二进制流） |
| 断点续传 | `.part` 临时名 + 完成原子 rename + 按 offset 续传（peer 源 offset/length query；在线源 `Range: bytes=a-b`） |
| 凭据模型 | `?token=` = 系统 admin token（`NEXOS_ADMIN_TOKEN`）；下架支持 admin 或同 sharer；在线源可选 `NEXOS_MODELSCOPE_TOKEN`/`NEXOS_HF_TOKEN` Bearer |
| 新增测试 | A 面 11 + B 面 10 + C 面 9 + D 面 12 = **42 个**（端到端含本地 TcpListener 假源 HTTP 服务） |
| 安全面 | rm -rf 前置矩阵校验 / 符号链接只解链 / share 路径白名单 + canonical 双保险 / token 恒时比较语义 / D 面 repo id 字符集白名单 + 路径 percent-encode |

一句话：**本地权重档案化（A）、模型分享进大厅（B）、从大厅多源并行拉回（C）、从公网
在线仓库直连下载（D：魔搭/HF 镜像）**——四面共用同一个 `/tank/models` 模型库，形成
"个人模型资产的可发现、可共享、可多点加速获取"闭环，契合 NexOS"打破孤岛"理念
（PHILOSOPHY.md：设备/数据孤岛的模型权重形态）。

---

## 1. 需求拆解（四面）

### A. 权重文件详细管理（增强本地模型）
- 模型目录**全文件清单**（递归、name/size/mtime + safetensors 分片序号解析
  `*-0000X-of-0000Y.safetensors`）
- **config.json 解析**：arch（model_type）/ 层数 / hidden / vocab / max_position_embeddings
- 总大小 + **完整性判定**（分片序列 1..=Y 连续 + `model.safetensors.index.json` 在场；
  单文件模型 = 有权重 + 有 config）
- **删除安全**：rm -rf 前置校验矩阵（根目录直系、拒 `..`/嵌套/逃逸；导入的符号链接只解链不删目标）
- **导入**：把模型库外的既有模型目录**符号链接**进库（不复制大文件）

### B. 模型大厅（发布/浏览/多源合并）
- 本地存在的模型才可**发布**；source_url 自动生成本机地址 + token
- 大厅列表**同 name 多发布者合并为一条**，`sources[]` 聚合各发布者的下载基地址
  （多人分享同一模型 = 天然多源）
- **文件共享端点**：供其他 NexOS 拉取权重文件——多源下载的 HTTP 传输面
- 下架权限：admin 或同 sharer

### C. 多源下载任务（lobby_multi）
- 任务体 `{name, sources:[url,...]}`（来自大厅 sources；单源即 sources 长度 1，同一套代码）
- 清单拉取：**首个可达源**的 `/models/:name/detail`（同 token）
- 文件级**轮转分配**：文件 i → 源 `i % n`（n 源 n 文件并发；单文件不做分段——文件天然并行）
- 逐文件下载：`.part` + 原子 rename + Range（offset）续传 + **失败换下一个源重试**
- 全部完成 → 逐文件 size 校验 → completed → 大厅全体分享者 `download_count+1`

### D. 在线仓库源（remote_repo，2026-08-31 新增——魔搭 ModelScope / HF 镜像）
- **动机**：用户为中国大陆网络，魔搭比 HF 官方快得多；原 modelscope CLI 下载依赖
  本机 `pip install modelscope`，未装即降级 failed。D 面=**纯 HTTP 直连**仓库 API，
  无任何 CLI 依赖。
- **源抽象**：`RemoteRepoKind` 枚举（`Modelscope` | `HfMirror`），每源三方法——
  `files_url()`（清单）/ `resolve_url()`（单文件下载，Range）/ `base()`+`token()`
  （env 注入）。与 C 面的 peer share 源并列（share 源是"熟人源"，D 面是"公网仓库源"）。
- **添加模型向导**：用户输入 `org/model` → 探测（存在性+文件清单+大小+默认勾选标记）
  → 展示可勾选清单（权重/config/tokenizer 默认勾，README/LICENSE/图片不勾）→
  下载到本地模型库（`models_root()` 既有约定）。
- **下载引擎**：逐文件 **bounded Range 分块**（16 MiB/块，`bytes=a-b` 闭区间）+ 流式
  落盘 + `.part` 断点续传 + 失败重试 3 次（单源无换源）+ 终态逐文件 size 校验 +
  原子 rename。任务 `type=remote_repo` 与 modelscope/lobby_multi 混排同一任务列表。
- **HF 镜像源**：`hf-mirror.com`（HF 协议兼容镜像，`HF_ENDPOINT` 同源生态），清单走
  `tree/main?recursive=true`，下载走 `resolve/main/<path>`（307→resolve-cache）。

---

## 2. 系统拓扑

### 2.1 组件分层与数据流（全局）

```
┌────────────────────────── 节点 A（ub2604，发布方+下载方双角色）──────────────────────────┐
│                                                                                          │
│  桌面前端（Vue3，路由 /modelhub）                                                          │
│   ├─ 本地模型页（A 面 UI：文件清单/分片完整性/删除确认/导入）                                │
│   └─ 大厅页（B 面 UI：卡片流/搜索/发布/多源下载按钮）                                        │
│                        │ REST (JSON)                                                      │
│  os-api :8080 ──────────┤  InProcessGateway（RateLimit → Auth → Audit 中间件链）           │
│   └─ handler: model_hub（本组件，19 路由）                                                 │
│        ├─ A 面  scan_model_weight_detail / delete_model_blocking / import_model_link      │
│        │      （真实 FS，spawn_blocking；纯函数可单测）                                     │
│        ├─ B 面  SQLite model_lobby 表 ←→ merge_lobby_rows（同 name 多源合并）              │
│        │       handle_share_file（token 校验 + 路径白名单 + offset/length 分段回传）       │
│        └─ C 面  fetch_manifest_from_sources → assign_files_round_robin                    │
│                → run_multi_download（tokio::spawn 后台，n 源 n worker 并行）               │
│                        │                                                                  │
│   /tank/models/<name>/          模型库（A 面扫描对象 + C 面落盘目标 + B 面共享源）           │
│   /tank/os-data/model_lobby.db  大厅发布索引（WAL，照 im.rs 建库惯例）                      │
│   /tank/git-repos/…             （对照：NexHub 仓库，互不影响）                              │
└──────────────────────────────────────────────────────────────────────────────────────────┘
          ▲ source_url（含 token 的 REST 基地址）           │ 拉取 detail + 分块文件
          │ 发布（本机地址自动生成）                          ▼
┌─────────────────────── 节点 B（其他 NexOS，分享者）───────────────────────┐
│  os-api :8080 ─ /api/v1/models/share/<name>/<file>?token=…&offset=&length=│
│  /tank/models/<name>/  同一模型的另一份拷贝（= 另一个下载源）               │
└──────────────────────────────────────────────────────────────────────────┘
```

### 2.2 多源下载调度时序（C 面核心数据流）

```
 POST /api/v1/models/downloads {name, sources:[A,B]}
   │
   ├─(1) 同步清单拉取：逐源 GET <A|B>/api/v1/models/<name>/detail?token=…
   │       首个 200+files 数组者胜出（全源失败 → 502，任务不入列）
   │
   ├─(2) 纯函数分配：assign_files_round_robin(files.len, sources.len)
   │       文件[i] → 源[i % n]     （f1→A f2→B f3→A f4→B …）
   │
   ├─(3) tokio::spawn(run_multi_download)
   │       每源 1 个 worker（= n 路并发文件流），worker 内顺序处理本源文件：
   │
   │   worker(A) ──► f1 ──► f3 ──► …        worker(B) ──► f2 ──► f4 ──► …
   │      │ 失败(404/超时/长度不符)               │
   │      └──► 换下一个源重试该文件（轮转整圈：A→B→A…，圈尽即任务失败）
   │
   │   每文件： .part 续传判定 → 分块循环（4 MiB/块，offset 递增）
   │           → sync_all → 尺寸校验 → 原子 rename 落位
   │
   ├─(4) 全 worker join → 逐文件终态 size 校验（fs metadata == 清单 size）
   │
   └─(5) completed + SQLite: UPDATE model_lobby SET download_count+1 WHERE name=…
              （归因全体分享者，不区分实际供数源）
```

### 2.3 单文件下载状态机（download_file_with_source）

```
            ┌─────────────────────────────────────────────────┐
            │ part 已有字节 p；清单期望 e                        │
            └───────┬─────────────────────────┬───────────────┘
                    │ resume_offset_for(p,e)  │
        p < e ──────┘                         └───── p >= e（损坏/远端变更）
        offset = p（续传）                          offset = 0（删 .part 重下）
                    │                                   │
                    ▼                                   ▼
        ┌─── 循环：offset < e ────────────────────────────────┐
        │ GET share/<name>/<rel>?offset=&length=min(4MiB,e-o) │
        │   │ 非 200 / JSON 坏 / base64 坏 / 长度≠want → Err   │──► 换源（上层 worker）
        │   ▼                                                │
        │ append 写 .part；offset+=want；bytes_done+=want     │
        │ （任务被 DELETE 移除 → Err"已取消"）                  │
        └──────────────┬──────────────────────────────────────┘
                       ▼
        sync_all（tokio File 有内部写缓冲，必须显式刷盘）
                       ▼
        metadata(.part) == e ? ──否──► 删 .part + Err（换源从 0 重下）
                       │是
                       ▼
        rename(.part → 最终路径)  （原子落位，下载中永不出现半截正式文件）
```

---

## 3. 端点契约（20 条路由全表）

所有响应为 JSON。写操作（POST/DELETE）经网关 `AuthMiddleware` 强制鉴权
（`Authorization: Bearer <NEXOS_ADMIN_TOKEN>` 或 JWT admin）；例外：`DELETE /lobby/:id`
路由层仅要求"已认证"，handler 内细判 admin-or-sharer；`GET /share/:name/*` 用 `?token=`
自鉴权（远端拉取方不走网关 Bearer）。

| # | method | path | 鉴权 | 语义 | 失败码 |
|---|--------|------|------|------|--------|
| 1 | GET | `/api/v1/models/local` | 公开 | 列本地模型（**自家库 + HF hub 缓存合并**，见 §3.0；符号链接导入项与真实目录同权重可见） | — |
| 2 | GET | `/api/v1/models/local/:id` | 公开 | 单模型概要（顶层文件 + config；HF 条目按 `org--name` 反查 snapshot） | 404 |
| 3 | DELETE | `/api/v1/models/local/:id` | admin | 删模型（与 #5 同一安全校验；HF 缓存条目 **400 拒绝**） | 400/404 |
| 4 | GET | `/api/v1/models/:name/detail` | 公开 | **A 面**：权重档案（清单/分片/架构/完整性；HF 条目走 snapshot 目录，同一套解析） | 400/404 |
| 5 | DELETE | `/api/v1/models/:name` | admin | **A 面**：安全删除（矩阵校验；链接只解链；HF 缓存条目 400 拒绝） | 400/404 |
| 6 | POST | `/api/v1/models/import` | admin | **A 面**：库外目录符号链接导入 | 400/404/409 |
| 7 | GET | `/api/v1/models/downloads` | 公开 | 任务列表（modelscope + lobby_multi + remote_repo 三类混排，带 `type`） | — |
| 8 | POST | `/api/v1/models/downloads` | admin | 建任务：`sources`→多源；否则 `model_id`→modelscope | 400/**502** |
| 9 | DELETE | `/api/v1/models/downloads/:id` | admin | 取消（multi/remote：移除即 runner 收摊信号） | 404 |
| 10 | GET | `/api/v1/models/downloads/:id` | 公开 | 任务详情（multi/remote 实时快照） | 404 |
| 11 | GET | `/api/v1/models/recommended` | 公开 | 推荐模型（标注 downloaded） | — |
| 12 | GET | `/api/v1/models/stats` | 公开 | 聚合统计（active/completed 计三类任务） | — |
| 13 | POST | `/api/v1/models/lobby/publish` | admin | **B 面**：发布（本地须存在） | 400/404 |
| 14 | GET | `/api/v1/models/lobby?name=&q=` | 公开 | **B 面**：列表（同 name 合并多源） | — |
| 15 | GET | `/api/v1/models/lobby/:name` | 公开 | **B 面**：单模型（聚合 sources） | 404 |
| 16 | DELETE | `/api/v1/models/lobby/:id` | 认证 | **B 面**：下架（admin 或同 sharer） | 403/404 |
| 17 | GET | `/api/v1/models/share/:name/*?token=&offset=&length=` | token | **C 面传输面**：分块回传 base64 | 400/401/404 |
| 18 | GET | `/api/v1/models/remote/:kind/:org/:model` | 公开 | **D 面**：在线仓库探测（kind=modelscope\|hf） | 400/404/502 |
| 19 | POST | `/api/v1/models/remote/downloads` | admin | **D 面**：创建在线仓库下载任务（文件级勾选） | 400/**502** |
| 20 | GET | `/api/v1/models/spark-zone?probe=` | 公开 | **E 面**：Spark 专区（SM120/NVFP4 策展 + 逐条两源实时可用性；`probe=0` 跳过探测） | — |

### 3.0 HF hub 缓存自动扫描（2026-09-03：服务用户 ≠ 安装用户）

**动机（Spark 实测缺陷）**：`huggingface-cli`/`hf download` 把模型装进交互用户的
`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<hash>/`（hub 布局：blobs
存内容 + snapshots 存指向 blobs 的符号链接）。服务进程常跑 root（home=/root），
模型却装在 `/home/nvidia`——只扫自家模型库目录（`/tank/models`）天生看不见，
用户原话："在 hf 的缓存目录应该自动检查到，不在，应可以手动添加"。

**扫描根候选链**（全部扫描、canonical 去重；任一候选不存在静默跳过）：

```
NEXOS_MODELHUB_HF_CACHE          ← 显式指定（设置即替换全链：测试隔离/特殊布局）
HF_HUB_CACHE                     ← HF 官方 env，指向 hub 目录本身
HF_HOME/hub                      ← HF 官方约定的另一形态
/root/.cache/huggingface/hub     ← 服务常跑 root
/home/*/.cache/huggingface/hub   ← glob 全用户（安装用户 ≠ 服务用户是常态，诚实解）
```

**条目识别**：`models--<org>--<name>/snapshots/` 下取当前 snapshot——优先
`refs/main` 指向的 commit hash（HF 官方语义，确定性最高），缺 refs 时兜底 mtime
最新；snapshot 顶层须有 `config.json` 或 `*.safetensors`（复用 `is_valid_model_dir`）
才算模型（空 snapshot / 纯 README 剔除）。

**清单条目**（`GET /models/local` 元素新增两字段，旧字段语义不变）：

```jsonc
{
  "id": "nvidia--Qwen3.6-27B-NVFP4",   // org--name（URL 路径段安全，无 /）
  "display_name": "nvidia/Qwen3.6-27B-NVFP4",
  "source": "hf_cache",                 // 'local'（库目录/导入链接）| 'hf_cache'
  "path": "/home/nvidia/.cache/huggingface/hub/models--nvidia--Qwen3.6-27B-NVFP4/snapshots/<hash>",
  // path = snapshot 真实目录：symlink 布局对 vLLM 透明，--model <path> 直接可建实例
  "size_bytes": ..., "file_count": ..., "modified_at": ..., "has_config": true
}
```

**合并语义**：
- 自家库条目 `source=local`（`display_name`=目录名，与 id 相同）；
- 与自家目录**同名共存不去重**（id 撞车时两条都在列表，来源徽章区分）；
- `GET /stats` 的 `local_total`/`total_size_bytes` 同口径合并；
- `GET /models/:name/detail` 与 `GET /models/local/:id` 对 HF 条目同样成立
  （目录解析顺序：`<root>/<name>` 优先 → 不在库内时按 `org--name` 反查缓存）。

**删除被拒（400）**：HF 缓存是 huggingface 工具链的私有布局（blobs 硬链接复用 +
refs 记账），只 rm snapshot 会留孤儿 blobs——`DELETE /models/:name`、
`DELETE /models/local/:id` 对"库内无此名、缓存命中"的条目返回 400 并指引
`hf` CLI 或整删 `models--<org>--<name>`。前端对 `source=hf_cache` 的卡片不渲染
删除按钮。

**手动添加契约**（自动扫描不可覆盖时的逃生口）：既有 `POST /api/v1/models/import`
即手动添加入口——body `{path}` 指向任意本地模型目录（含 HF snapshot 目录），
校验（存在 + 顶层有 config.json 或 *.safetensors + 在模型库外 + 库内不重名）通过
后符号链接入库（不复制大文件）；路径不存在 404 / 非模型目录 400 / 库内重名 409。
UI 入口：本地模型 Tab「＋ 导入模型」。

### 3.1 GET /api/v1/models/:name/detail（A 面主端点）

```jsonc
{
  "name": "Qwen3-VL-8B-Instruct",
  "path": "/tank/models/Qwen3-VL-8B-Instruct",
  "total_size_bytes": 17526623982,
  "file_count": 8,
  "complete": true,                       // 分片模型=序列连续+index.json；单文件=权重+config
  "shards": {
    "sharded": true,
    "shard_total": 5,                     // 命名声明的 Y
    "shard_files": ["model-00001-of-00005.safetensors", "…"],
    "sequence_complete": true,            // 1..=Y 无缺号
    "missing_shards": [],                 // 缺号列表（如缺 3 号 → [3]）
    "index_file_present": true            // model.safetensors.index.json
  },
  "config": {                             // 不存在为 null
    "arch": "qwen3vl",                    // model_type
    "num_hidden_layers": 36,
    "hidden_size": 2048,
    "vocab_size": 151936,                 // 字符串数字也可解析
    "max_position_embeddings": 262144,
    "raw": { /* 原始 config.json 全文 */ }
  },
  "files": [                              // 递归、按路径排序
    { "name": "config.json", "size_bytes": 119, "modified_at": "2026-08-20T10:00:00+08:00",
      "shard_index": null, "shard_total": null },
    { "name": "model-00001-of-00005.safetensors", "size_bytes": 5100000000,
      "modified_at": "…", "shard_index": 1, "shard_total": 5 }
  ]
}
```

分片命名解析规则（`parse_shard_filename`，纯函数）：`<前缀>-NNNNN-of-NNNNY.safetensors`
——序号与总数各**恰 5 位数字**、序号 ≥1；`model-1-of-5.safetensors`（位数不符）与
`model.safetensors`（不分片）都返回 None（不算分片文件）。

### 3.2 DELETE /api/v1/models/:name —— 删除安全校验矩阵

| 场景 | 结果 |
|---|---|
| `<root>/<name>` 是真实目录，canonical 父 == canonical root | 200 `{"ok":true,"action":"delete"}`（rm -rf） |
| `<root>/<name>` 是**符号链接**（导入产物，目标在库外） | 200 `{"action":"unlink"}`——**只解除链接，目标目录原样保留** |
| name 含 `..` / `/` / `\` / NUL / 以 `-` 开头 / 空白 | 400（名字校验先行） |
| 目标不存在 | 404 |
| 目标是根的**嵌套**子目录（`<root>/a/b`） | 400（canonical 父 ≠ root，拒删） |
| 目标是普通文件 | 400 |
| 符号链接解析后逃逸到根外 | 天然安全：符号链接走 unlink 分支，根本不 canonical 目标 |

旧端点 `DELETE /local/:id` 已改为同一实现（原先只有简单的 `..`/`/` 检查）。

### 3.3 POST /api/v1/models/import

请求 `{ "path": "/home/oem/hf_models/Qwen3-VL-8B-Instruct" }`，校验链：

1. 源路径存在且为目录（否则 404）；
2. 顶层含 `config.json` 或任一 `*.safetensors` 才认是模型（否则 400）；
3. 源必须**在模型根之外**（根内目录无需导入，400）；
4. 库内重名（链接或目录已占位）→ 409；
5. 通过 → `symlink(src_canonical, <root>/<basename>)`，响应 201：

```json
{ "name": "Qwen3-VL-8B-Instruct", "link_path": "/tank/models/Qwen3-VL-8B-Instruct",
  "target_path": "/home/oem/hf_models/Qwen3-VL-8B-Instruct" }
```

不复制任何大文件（几 GB 权重秒级入库）；`GET /models/local` 立即可见（扫描用 stat
语义跟随符号链接）。

### 3.4 POST /api/v1/models/lobby/publish（B 面）

```jsonc
// 请求（name 必填；其余可选）
{ "name": "Qwen3-VL-8B-Instruct", "display_name": "千问3-VL-8B",
  "description": "最强视觉语言模型", "tags": ["vl","8B"], "sharer": "alice" }
// sharer 缺省：认证身份用户名（admin token → "admin"）；净化规则 [A-Za-z0-9._-]，其余→'-'
// 响应 201
{ "ok": true, "id": "Qwen3-VL-8B-Instruct@alice", "name": "…", "display_name": "千问3-VL-8B",
  "description": "…", "tags": ["vl","8B"], "arch": "qwen3vl",        // 扫本地 detail 得出
  "size_bytes": 17526623982, "file_count": 8, "sharer": "alice",
  "source_url": "http://ub2604:8080/api/v1/models/share/Qwen3-VL-8B-Instruct?token=<admin token>",
  "share_token": "<admin token>", "created_at": "2026-08-21T12:00:00+08:00" }
```

- **本地存在才可发布**：`<root>/<name>` 须为目录且含 config.json 或 `*.safetensors`（404）
- `source_url` 自动生成：host = `NEXOS_MODELHUB_SHARE_HOST` → `hostname` 命令 → `localhost`；
  port = `NEXOS_HTTP_PORT`/`OS_HTTP_PORT`（默认 8080）
- **凭据风险（明示）**：`?token=` 携带系统 admin token——任何拿到 source_url 的节点都能
  以 admin 身份调本机全部写接口之外的 share 读取面（share 端点本身只读）。source_url 会
  存进 SQLite、出现在大厅 API 响应中。生产建议：发布专用 token（待办，见 §9）、或
  NEXOS_MODELHUB_SHARE_HOST 指向反代层做 token 替换。未配置 admin token 时 source_url
  不带 `?token=`（share 端点将 401，发布会成功但无法被拉取）。
- 同 `(name, sharer)` 重复发布 = **刷新快照**（`INSERT OR REPLACE`，保留 download_count）

### 3.5 GET /api/v1/models/lobby（合并列表）

```jsonc
[
  { "name": "Qwen3-VL-8B-Instruct",        // ← 多源合并键
    "display_name": "千问3-VL-8B", "description": "…", "tags": ["vl","8B"],
    "arch": "qwen3vl", "size_bytes": 17526623982, "file_count": 8,
    "download_count": 12,                  // 各来源计数之和
    "sources": [                           // ← 多人分享即多源（按发布时间升序）
      { "sharer": "alice", "source_url": "http://10.0.0.2:8080/api/v1/models/share/…?token=…",
        "size_bytes": 17526623982, "file_count": 8, "created_at": "…" },
      { "sharer": "bob",   "source_url": "http://10.0.0.5:8080/api/v1/models/share/…?token=…",
        "size_bytes": 17526623982, "file_count": 8, "created_at": "…" } ],
    "created_at": "…" }                    // 最早发布时间
]
```

- 排序：download_count 降序 → name 升序
- `?name=X` 精确过滤；`?q=子串` 对 name/display_name/description/arch/tags 大小写不敏感匹配
- `GET /lobby/:name` 返回同结构的单条（404 无此名）

### 3.6 DELETE /api/v1/models/lobby/:id —— 下架权限

id 形如 `<name>@<sharer>`。判定顺序：admin 角色（放行）→ JWT 用户名 == sharer（放行）→
403。匿名（无认证）在路由层即被拦（requires_auth=true）。响应
`{"ok":true,"id":…,"name":…,"sharer":…,"action":"unpublish"}`。

### 3.7 GET /api/v1/models/share/:name/*（文件共享端点，C 面传输面）

```
GET /api/v1/models/share/Qwen3-VL/model-00001-of-00005.safetensors
    ?token=<admin token>&offset=8589934592&length=4194304
```

- **token**：必须精确等于系统 admin token（query 携带——远端拉取方不做网关 Bearer 鉴权）；
  错/缺/系统未配置 → 401
- **路径白名单**：percent-decode 后逐段校验（空/`.`/`..`/含 `\` 或 NUL → 400），
  再 canonicalize 双保险（目标必须仍在 `<root>/<name>/` 内，防符号链接中途逃逸）
- 目标是目录 → 400（"请指定具体文件路径"）；不存在 → 404
- **offset/length**（可选，默认整文件）：服务端单次上限 64 MiB（`length` 超限 400——
  内存防护，防止无参请求把 17 GB 权重整个读进内存）
- 回传 JSON 信封（二进制走 base64，+33% 带宽开销——网关 ApiResponse 契约是 JSON，
  权衡见 §8）：

```jsonc
{ "ok": true, "name": "Qwen3-VL", "path": "model-00001-of-00005.safetensors",
  "offset": 8589934592, "length": 4194304, "total_size": 5100000000,
  "eof": false, "content_base64": "AAAA…" }
```

### 3.8 POST /api/v1/models/downloads（双任务合一）

```jsonc
// modelscope 任务（原语义不变）
{ "model_id": "Qwen/Qwen3-VL-8B-Instruct" }
// lobby_multi 多源任务（新）
{ "name": "Qwen3-VL-8B-Instruct",
  "sources": ["http://10.0.0.2:8080/api/v1/models/share/Qwen3-VL-8B-Instruct?token=…",
               "http://10.0.0.5:8080/api/v1/models/share/Qwen3-VL-8B-Instruct?token=…"] }
```

- 多源任务 name 必填且是合法目录名；sources 每项须 `http(s)://host[:port]…`（400）
- **清单同步拉取**（15s 超时/源）：首个可达源的 `detail`；全部不可达 → **502**
  `{"error":"全部 2 个源的模型清单均不可用（最后错误: …）"}`，任务不入列
- 成功 → 201 + 任务快照（见 3.9），后台 `tokio::spawn` 开始下载

### 3.9 lobby_multi 任务状态 JSON（GET /downloads/:id）

```jsonc
{ "id": "mdlm-101", "type": "lobby_multi", "name": "Qwen3-VL-8B-Instruct",
  "local_dir": "/tank/models/Qwen3-VL-8B-Instruct",
  "sources": ["http://…?token=…", "http://…?token=…"],
  "status": "downloading",                // downloading | completed | failed
  "files_total": 8, "files_done": 3,
  "bytes_done": 5232398336,               // 含续传现场的既有 .part 字节
  "total_bytes": 17526623982,
  "active_sources": ["http://10.0.0.2:8080/…"],   // 仍在供数的源
  "recent_files": [                        // 最近 5 条文件级简报（含换源失败记录）
    { "file": "model-00003-of-00005.safetensors", "source": "http://10.0.0.5:8080/…",
      "bytes": 5100000000, "status": "done", "error": null },
    { "file": "model-00003-of-00005.safetensors", "source": "http://10.0.0.2:8080/…",
      "bytes": 0, "status": "failed", "error": "分块请求返回 404 @0" } ],
  "cancel_requested": false, "error": null, "created_at": "…" }
```

`GET /downloads` 列表 = modelscope 任务（补 `"type":"modelscope"`）+ lobby_multi 任务
+ remote_repo 任务三类拼接。`DELETE /downloads/:id`：multi/remote 任务从列表移除——
runner 分块间探测到消失即收摊（状态最终 failed"用户取消"，因已移除不再可见）；
modelscope 任务保持原 kill-pid 语义。

### 3.10 GET /api/v1/models/remote/:kind/:org/:model（D 面探测，公开读）

`kind`：`modelscope`（魔搭）| `hf`（HF 镜像）。`org/model` 路径段即仓库 id。
一次探测 = 存在性（上游 404/Code≠200/非数组 → 404 "仓库不存在（或无权访问）"）
+ 文件清单（含大小与向导默认勾选标记）：

```jsonc
{ "ok": true, "kind": "modelscope", "repo_id": "Qwen/Qwen2.5-0.5B-Instruct",
  "name": "Qwen2.5-0.5B-Instruct",        // 本地目录名建议（repo 末段）
  "file_count": 11, "total_size_bytes": 999604128,
  "files": [
    { "name": "model.safetensors", "size_bytes": 988097824, "default_selected": true },
    { "name": "config.json",       "size_bytes": 659,       "default_selected": true },
    { "name": "README.md",         "size_bytes": 4917,      "default_selected": false } ] }
```

失败码：kind 非法 400；仓库不存在 404；上游网络/5xx 502。
`default_selected` 规则（`is_default_selected` 纯函数）：`.safetensors/.bin/.pt/.pth/
.gguf/.onnx/.json/.txt` + `spiece.model/tokenizer.model/sentencepiece.bpe.model` 勾；
README/LICENSE/.gitattributes/图片/pdf 不勾。

### 3.11 POST /api/v1/models/remote/downloads（D 面创建任务，admin）

```jsonc
// body
{ "kind": "modelscope",                    // modelscope | hf
  "repo_id": "Qwen/Qwen3-VL-8B-Instruct",  // org/model（单字段，非路径参数）
  "name": "Qwen3-VL-8B",                   // 可选：本地目录名（缺省 repo 末段，须过模型名校验）
  "files": ["model-00001-of-00004.safetensors", "config.json"] }  // 可选：勾选子集（缺省=全部）
// 201 → remote_repo 任务（GET /downloads/:id 同口径）
{ "id": "mdlrm-103", "type": "remote_repo", "kind": "modelscope",
  "repo_id": "Qwen/Qwen3-VL-8B-Instruct", "name": "Qwen3-VL-8B",
  "local_dir": "/tank/models/Qwen3-VL-8B",
  "status": "downloading", "files_total": 2, "files_done": 0,
  "bytes_done": 5100000000,                // 含续传现场的既有 .part 字节
  "total_bytes": 9900000000,
  "recent_files": [ /* 同 lobby_multi 的 FileProgress，source=源 label */ ],
  "cancel_requested": false, "error": null, "created_at": "…" }
```

失败码：kind/repo_id/files 非法 400（`files` 含清单外路径 400 并列明）；
上游探测失败 502（任务不入列）。取消/详情/统计与既有 `/downloads/:id` 三类任务
同端点同语义。

---

## 4. 数据模型（SQLite `model_lobby`）

DB 路径：`/tank/os-data/model_lobby.db` → `/var/lib/os/model_lobby.db` → `./model_lobby.db`
（照 im.rs / nexhub_lobby.rs 的 default_db_path 三级回退；WAL 模式；打开失败降级内存库）。

| 列 | 类型 | 说明 |
|---|---|---|
| id | TEXT PK | `<name>@<sanitized sharer>`——同发布者重复发布幂等刷新 |
| name | TEXT NOT NULL | 模型名（**多源合并键**），索引 `idx_model_lobby_name` |
| display_name / description / tags(JSON 数组) | TEXT | 展示与搜索 |
| arch | TEXT | 发布时扫本地 detail 得出（model_type） |
| size_bytes / file_count | INTEGER | 发布时快照（合并展示取各源最大值） |
| sharer | TEXT | 发布者（默认 admin / 认证身份） |
| source_url | TEXT | **含 token 的完整 REST 下载基地址**（多源下载直接可用） |
| share_token | TEXT | 发布时的 admin token 快照（审计/排障） |
| created_at | TEXT | ISO 8601 本地时区 |
| download_count | INTEGER | 多源任务 completed 时同 name 全体 +1 |

合并逻辑（`merge_lobby_rows` 纯函数）：按 name 分桶 → sources 按 created_at 升序聚合 →
size/file_count 取桶内最大、download_count 求和、展示字段取首行 → 全列表按
download_count 降序 + name 升序。**持久化口径**：大厅=SQLite（重启保留）；下载任务
（modelscope 与 lobby_multi）=内存态（与原有下载任务一致，重启即失——进度可由
`.part` 文件自然恢复，见 §8 待办）。

---

## 5. 多源调度算法（C 面详细）

1. **清单拉取**（同步，POST 内）：按 sources 顺序逐个 GET
     `<scheme>://<authority>/api/v1/models/<name>/detail?token=<source_url 的 token>`
   （`derive_detail_url` 从 source_url 拆 scheme/authority/token）。HTTP 失败/非 200/
   JSON 坏/无 files/空清单 → 换下一个源；全败 → 502。
2. **轮转分配**（`assign_files_round_robin` 纯函数）：`文件 i → 源 i % n`。
   n 源即 n 路文件并发；文件数 < 源数时多余源空闲（不浪费——无文件可分）。
3. **worker**：每源一个 tokio task，顺序处理分到的文件；单文件内部分块串行
   （不做单文件分段并行——文件天然是并行单位，实现简单且失败域清晰）。
4. **失败换源**：文件级错误（404/超时/分块长度不符/size 校验不符）→ 从**分配源起**
   轮转尝试整圈（`(assigned + attempt) % n`）；每源一次机会，圈尽 → 任务 failed。
   已下载的 `.part` 保留，换源后从当前偏移续传（不重复已下载字节）。
5. **续传**（`resume_offset_for` 纯函数）：`.part` 字节 < 期望 → offset=现有字节续传；
   ≥ 期望（损坏/远端模型变更）→ 删 `.part` 从 0 重下。任务创建时把各文件既有 `.part`
   字节计入 `bytes_done`（进度不从 0 假起）。
6. **原子落位**：全部分块完成 → `sync_all`（**tokio::fs::File 有内部写缓冲，drop 是后台
   异步刷盘——不显式 sync 会读到滞后 metadata，实测出现过 0/半截字节，已修复并注释**）
   → 尺寸校验 → `rename(.part → 最终路径)`。下载中目录里永不出现半截正式文件。
7. **终态**：全部 worker join → 逐文件 `fs metadata == 清单 size` 校验 → completed /
   failed（首个致命错误进 `error` 字段）；completed 时 `UPDATE model_lobby SET
   download_count = download_count + 1 WHERE name = ?`。
8. **取消**：DELETE 移除任务 → runner 分块间/文件间探测列表中任务消失 → 停止。

---

## 5D. 在线仓库源（D 面详细：拓扑 / 实测 API / 避坑）

### 5D.1 拓扑与数据流

```
┌──────────────────────────── 节点 A（下载方）────────────────────────────┐
│  桌面前端 ModelHub.vue「下载」Tab                                          │
│   └─ 新建下载对话框：源选择（魔搭 ModelScope | HF 镜像 | ModelScope CLI）    │
│        ├─ HTTP 源向导：org/model 输入 → 探测 → 文件清单勾选（权重默认勾）      │
│        └─ CLI 源：原 model_id 表单（spawn modelscope download，旧语义）      │
│                     │ REST (JSON)                                         │
│  os-api :8080 ──────┤  GET /models/remote/:kind/:org/:model   （探测，公开） │
│                      └─ POST /models/remote/downloads        （建任务，admin）│
│   handler: model_hub D 面                                                  │
│    ├─ RemoteRepoKind（枚举源：files_url/resolve_url/base/token）            │
│    ├─ probe_remote_repo → parse_modelscope_files / parse_hf_tree（纯函数）  │
│    └─ run_remote_download（tokio::spawn 后台：逐文件 × 重试 3 次）           │
│         └─ download_remote_file：bounded Range 16 MiB/块 + 流式落盘          │
│              + .part 续传 + 终态 size 校验 + 原子 rename                    │
│                     │ HTTPS                                                │
│  ┌──────────────────┴───────────────────┐                                 │
│  │ ModelScope（默认 www.modelscope.cn）  │  HF 镜像（默认 hf-mirror.com）   │
│  │  files: /api/v1/models/<id>/repo/files│   tree: /api/models/<id>/tree/  │
│  │  dl:    /<id>/resolve/master/<path>   │   dl:   /<id>/resolve/main/<p>  │
│  │  （LFS 大文件 302 → cdn-lfs-cn-1）    │  （307 → resolve-cache）        │
│  └──────────────────────────────────────┘                                 │
│  /tank/models/<name>/           落盘目标（models_root() 既有约定）           │
│  任务列表（内存态）               GET /models/downloads 三类混排 + 前端 3s 轮询 │
└──────────────────────────────────────────────────────────────────────────┘
```

### 5D.2 实测 API 样例（2026-08-31 curl，实现依据——以实测为准）

**ModelScope 存在性/清单**（files 接口即存在性探测，无需单独调模型信息接口）：

```bash
# 清单（Recursive=true 平铺；Type=tree 是目录条目 Size=0，Type=blob 是文件）
curl -sS "https://www.modelscope.cn/api/v1/models/Qwen/Qwen2.5-0.5B-Instruct/repo/files?Recursive=true"
# → {"Code":200,"Data":{"Files":[
#     {"Name":"sub","Path":"sub","Size":0,"Type":"tree"},          ← 滤除
#     {"Name":"model.safetensors","Path":"model.safetensors",
#      "Size":988097824,"Type":"blob","Sha256":"fdf756…"},          ← 取 Path+Size
#     {"Name":"config.json","Path":"config.json","Size":659,"Type":"blob"}, …]}}

# 仓库不存在：HTTP 404 + {"Code":10010205001,
#   "Message":"获取模型信息失败，信息：record not found","Success":false}
```

**ModelScope 单文件下载**（resolve 端点，bounded Range）：

```bash
# 小文件 Range：回 200（非 206！）+ Content-Range，正文恰为所求 100 字节
curl -sS -D - -H "Range: bytes=0-99" \
  "https://www.modelscope.cn/Qwen/Qwen2.5-0.5B-Instruct/resolve/master/config.json"
# → HTTP/1.1 200 OK / Accept-Ranges: bytes / Content-Range: bytes 0-99/659 / Content-Length: 100

# LFS 大文件：302 重定向到 CDN（reqwest 自动跟随，Range 头保留，206）
curl -sS -D - -o /dev/null -H "Range: bytes=0-1048575" \
  "https://www.modelscope.cn/Qwen/Qwen2.5-0.5B-Instruct/resolve/master/model.safetensors"
# → HTTP/1.1 302 Found / Location: https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/…
#   （跟随后）HTTP/2 206 / content-range: bytes 0-1048575/988097824
```

**HF 镜像**（协议同型，revision 段为 `main`）：

```bash
curl -sS "https://hf-mirror.com/api/models/Qwen/Qwen2.5-0.5B-Instruct/tree/main?recursive=true"
# → [{"type":"directory","path":"sub","size":0},
#    {"type":"file","path":"model.safetensors","size":988097824,"lfs":{"size":988097824}}, …]

curl -sS -D - -H "Range: bytes=0-99" \
  "https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct/resolve/main/config.json"
# → HTTP/2 307 → resolve-cache → 200 + content-range: bytes 0-99/659
```

**真实端到端**：开发期以临时 ignored 测试对生产 API 跑通 探测（11 文件/999,604,128 字节，
与页面一致）→ config.json 全量下载（659 字节，内容为合法 JSON）→ 预置 .part 前 300 字节
重发 → 服务端仅补 359 字节（`bytes=300-658`）且最终文件逐字节等于全量结果。验证后该
临时测试已删除（真实 API 不入测试树）。

### 5D.3 避坑（与简报口径的差异更正 + 实测结论）

1. **`/api/v1/models/<id>/download?FilePath=<path>` 端点不存在**——实测回
   `404 page not found`（任务简报里写的该端点不成立）。真实下载端点是
   **`/<org>/<model>/resolve/master/<path>`**（git web 同款路径），本文档为准。
2. **ModelScope 忽略 open-ended Range**：`Range: bytes=600-` 实测返回**整个文件**
   （Content-Length: 659），静默丢续传。必须 **bounded `bytes=a-b`** 闭区间分块
   （`REMOTE_CHUNK_BYTES` 16 MiB）并强校验收到的字节数（多收=Range 被忽略 → 截断
   检测；少收=连接截断 → 报错留 .part 续传）。
3. **小文件 Range 命中回 200 不是 206**（LFS 走 CDN 才回 206）——判据用
   Content-Range/字节数，不要断言 206。
4. **reqwest/hyper 发小写请求头名**（`range:`）——测试断言与假源解析都要大小写不敏感。
5. **重定向携带的 CDN auth_key 有时效**（Location 里 `auth_key=<ts>-…`）——不要缓存
   重定向后的 URL，让 reqwest 每次跟随（跨主机时 reqwest 自动剥 Authorization，恰好
   是安全方向：token 不泄露给 CDN）。
6. **HF tree API 单页 1000 条**：超多文件仓库走 Link header 分页（模型场景罕见）——
   当前实现取第一页，巨多文件仓库清单不全列入待办（魔搭 files API 未见分页，72B 模型
   48 文件实测一次全量）。
7. **仓库 id 字符集限 `[A-Za-z0-9._-]`**（`validate_repo_id`）——两源主流模型 id 均在
   集内；同时天然免疫 URL 注入。文件路径另行逐段 percent-encode（`encode_path`，
   支持中文/空格文件名）。
8. **测试 env 覆盖与 ENV_MUTEX**：`ScopedModelsRoot` 与新 `ScopedEnvs` 共用一把互斥锁
   且 std Mutex 不可重入——**必须一次锁覆盖全部键**（models 根 + 两源 base），
   逐键各拿一把会自死锁（开发期实测踩坑）。

---

## 5E. Spark 专区（E 面详细：SM120/NVFP4 策展 + 实时可用性）

### 5E.0 语义定稿（用户需求原文 → 行为）

> "模型仓库增加 Spark 专区选项，如 nv-community/Qwen3.6-27B-NVFP4、
> unsloth/Qwen3.8-27B-NVFP4 这两个的下载，但别的 SM120 也能用，可以公用这些模型，
> 只是这些可以 Spark 上更好的选择"

- **专区 = 精选 NVFP4 模型清单 + 一键下载入口**，不是新下载器：下载完全复用 D 面
  在线源机制（魔搭/HF 镜像、清单勾选、断点续传、文件级并行），前端进既有下载向导
  （预填 repo 与首选源）。
- **非 Spark 专属**：NVFP4 对 SM120 计算架构（DGX Spark 的 GB10 / RTX 50 系等）有
  硬件级优化，但模型文件通用。前端专区头部常驻醒目说明条（四语 i18n）：
  "NVFP4 为 SM120 架构优化的量化格式：DGX Spark 与其它 SM120 GPU（如 RTX 50 系）
  通用，非 Spark 专属。专区只是「在 Spark 上更好的选择」的策展入口。"

### 5E.1 端点契约

`GET /api/v1/models/spark-zone`（公开读，路由表 #20）：

- 缺省（或 `?probe=1`）：返回策展清单 + **逐条两源实时可用性**——条目 × 2 源全并行
  （`futures::join_all`），单源 3s 超时（`SPARK_ZONE_PROBE_TIMEOUT_SECS`，专区页要快，
  宁可标不可用不卡 30s）。探测复用 D 面 `files_url` + 解析器（存在性 + 件数 + 全量
  大小），失败标 `available=false` + error 原因，**条目不剔除**（诚实降级）。
- `?probe=0`：跳过探测（`probed=false`，sources 恒"未探测"占位），省网络适合弱网。
- `downloaded`：本地库 `models_root()/<repo 末段>` 目录存在即 true（已下载不再拉）。

```jsonc
{
  "ok": true,
  "probed": true,
  "origin": "builtin",                  // builtin | env（NEXOS_SPARK_ZONE_FILE 生效）
  "entries": [
    {
      "repo": "nv-community/Qwen3.6-27B-NVFP4",
      "org": "nv-community",
      "quant": "NVFP4",
      "params": "27B",
      "note": "Qwen3.6 27B NVFP4（ModelOpt 量化）——…",
      "downloaded": false,
      "sources": [                      // 恒 [modelscope, hf] 两元素
        { "kind": "modelscope", "available": true, "file_count": 17,
          "total_size_bytes": 21900000000, "error": null },
        { "kind": "hf", "available": false, "file_count": null,
          "total_size_bytes": null, "error": "探测返回 401 Unauthorized" }
      ]
    }
  ]
}
```

下载动作不经本端点：前端「下载」按钮 → 既有新建下载向导（预填 repo + 首个可用源，
向导内再探测勾选）→ `POST /models/remote/downloads`（D 面，admin）。

### 5E.2 内置策展清单与实测记录（2026-09-02，收录依据）

探测方法：curl ModelScope `GET /api/v1/models/<repo>/repo/files?Recursive=true` 与
HF 镜像 `GET /api/models/<repo>/tree/main?recursive=true`（即 D 面生产同款探测路径）。
收录口径：真实存在 + NVFP4 量化 + 体积适合 Spark 128GB 统一内存（≤25GiB 级，
留 KV cache 余量）。探测过的候选共 10 仓，收录 7：

| # | repo | 参数量 | 魔搭 | HF 镜像 | 全量大小 | 备注 |
|---|------|--------|------|---------|----------|------|
| 1 | `nv-community/Qwen3.6-27B-NVFP4` | 27B | ✓ 17 文件 | ✗ 401 | 20.4 GiB | **用户点名**；config：modelopt NVFP4 |
| 2 | `unsloth/Qwen3.8-27B-NVFP4` | 27B | ✓ 14 文件 | ✓ 13 文件 | 21.8 GiB | **用户点名**；compressed-tensors mixed-precision |
| 3 | `unsloth/Qwen3.6-27B-NVFP4` | 27B | ✓ 20 | ✓ 20 | 21.8 GiB | 同源模型另一发布渠道 |
| 4 | `nv-community/Qwen3.6-35B-A3B-NVFP4` | 35B-A3B | ✓ 16 | ✗ 401 | 21.8 GiB | MoE 激活 3B，低激活高吞吐 |
| 5 | `unsloth/Qwen3.6-35B-A3B-NVFP4` | 35B-A3B | ✓ 19 | ✓ 19 | 24.7 GiB | MoE |
| 6 | `RedHatAI/Qwen3.6-35B-A3B-NVFP4` | 35B-A3B | ✓ 88 | ✓ 87 | 23.4 GiB | RedHatAI 发行 |
| 7 | `nv-community/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-NVFP4` | 30B-A3B | ✓ 72 | ✗ 401 | 20.1 GiB | NVIDIA 官方系 MoE |

实探后**未收录**（同 org 或高热但不适配收录口径）：

| repo | 魔搭 | HF | 大小 | 不收录原因 |
|------|------|-----|------|-----------|
| `nv-community/GLM-5.2-NVFP4` | ✓ 57 | ✗ 401 | 432.9 GiB | 超 Spark 128GB 统一内存（策展口径 ≤25GiB 级） |
| `RadixArk/Qwen3.8-Flash-Next-NVFP4` | ✓ 420 | ✓ 419 | 126.0 GiB | 同上（超体量） |
| `unsloth/Qwen3.6-35B-A3B-NVFP4-Fast` | ✓ 18 | ✓ 18 | 22.1 GiB | 与 #5 近重复变体，控制清单长度 |

附注：魔搭搜索 API（`PUT /api/v1/dolphin/models`，`Name=NVFP4`）共命中 853 仓，
上表候选即从两用户点名 org（nv-community / unsloth）+ 同名热门发行（RedHatAI）中
实探选出。**nv-community 系为魔搭独占**（HF 镜像对不存在/无权限仓库回 401；
`?author=nv-community` 列表为空）——专区卡片对此如实展示两源徽章一绿一红。

### 5E.3 env 覆盖（`NEXOS_SPARK_ZONE_FILE`）

可选 JSON 文件覆盖策展表（运维增删条目），两种形态：

- **数组** `[ {repo, org, quant, params, note}, … ]` → **合并**语义：同 repo 整条覆盖
  内置（保位），新 repo 追加殿后（`merge_spark_zone_entries` 纯函数）。
- **对象** `{"replace": [ … ]}` → **整体替换**语义：内置条目全部让位（可删条目）。

诚实降级：未设置 / 读失败 / 解析失败 → 回退内置表（`eprintln!` 带 `[modelhub]` 前缀
记原因）；repo 形态不合法的条目剔除并记日志（不整表报废）。响应 `origin` 字段
如实标注 `builtin` / `env`。

### 5E.4 边界（明确不做）

- **策展表静态**：不做全网 NVFP4 搜索/自动发现（魔搭搜索 API 有但结果噪音大——
  853 命中含 GGUF 混排/个人转存；策展质量靠人工维护 + env 覆盖通道）。
- 专区不引入新下载器/新任务类型：下载走 D 面 `remote_repo` 任务。
- 探测只看清单存在性（件数/大小），不校验量化格式真伪（config.json 的
  `quantization_config` 在下载后由本地权重档案 A 面呈现）。


---

## 6. 环境变量清单（功能文档铁律）

| env | 默认 | 作用 / 注入点 |
|---|---|---|
| `NEXOS_MODELS_DIR` / `OS_MODELS_DIR` | `/tank/models`（存在性回退 `/var/lib/os/models`） | **模型库根目录覆盖**（`models_root()`）——测试隔离/自定义库位 |
| `NEXOS_MODELHUB_HF_CACHE` | 空（候选链：`HF_HUB_CACHE`/`HF_HOME/hub` → `/root/.cache` → `/home/*` glob 全用户） | **HF hub 缓存扫描根覆盖**（`hf_cache_candidate_roots()`）——设置即替换全链（测试隔离/特殊缓存位；识别 `models--org--name/snapshots/<hash>`，见 §3.0） |
| `HF_HUB_CACHE` / `HF_HOME` | 空（继承服务环境） | HF 官方缓存位置 env（未设 `NEXOS_MODELHUB_HF_CACHE` 时并入候选链） |
| `NEXOS_MODELHUB_SHARE_HOST` | `hostname` 命令 → `localhost`（OnceLock 缓存） | 发布 source_url 的 host（`share_host()`）——多网卡/公网地址场景 |
| `NEXOS_HTTP_PORT` / `OS_HTTP_PORT` | `8080` | 发布 source_url 的端口（`share_port()`，与 code_repo http_port 同款） |
| `NEXOS_ADMIN_TOKEN` / `OS_ADMIN_TOKEN` | 无 | share 端点 token 校验 + 发布 source_url 的 token + handler admin 判定（构造时定格；测试经 `with_admin_token` 注入） |
| `NEXOS_MODELSCOPE_BASE` | `https://www.modelscope.cn` | **D 面**魔搭 API 基地址（`RemoteRepoKind::base()`，trim + 去尾 `/`；测试注入假源 TcpListener 地址） |
| `NEXOS_MODELSCOPE_TOKEN` | 无（可选） | **D 面**魔搭私有模型访问令牌（`RemoteRepoKind::token()`）——探测与下载请求注入 `Authorization: Bearer <t>`（跨主机重定向由 reqwest 自动剥头，不泄给 CDN） |
| `NEXOS_HF_BASE` | `https://hf-mirror.com` | **D 面** HF 镜像基地址（可指自建镜像或官方 `https://huggingface.co`） |
| `NEXOS_HF_TOKEN` | 无（可选） | **D 面** HF 私有模型令牌（同 Bearer 注入口径） |
| `NEXOS_SPARK_ZONE_FILE` | 无（用内置表） | **E 面** Spark 专区策展清单覆盖文件（JSON 数组=合并 / `{"replace":[…]}`=整体替换，见 §5E.3；读/解析失败回退内置表并 eprintln `[modelhub]`） |

---

## 7. 安全设计汇总

| 面 | 威胁 | 对策 |
|---|---|---|
| 删除 | path 穿越（`..`）、误删嵌套目录、删到导入目标 | 名字白名单（纯函数）+ canonicalize 父目录 == 模型根 + 符号链接只 unlink；测试矩阵 7 场景 |
| 导入 | 把库内目录重复导入 / 导入任意目录 | 源必须在根外 + 模型性校验（config/safetensors）+ 库内重名 409 |
| share | 路径穿越（明文与 `%2e%2e` 编码）、目录列举、内存放大 | 逐段白名单（decode 后）+ canonical 前缀双保险 + 目录 400 + 64 MiB 单次上限 + token 必须 |
| 大厅 | 匿名下架他人条目 | 路由 requires_auth + handler admin-or-sharer 细判 |
| 凭据 | admin token 随 source_url 分发扩散 | 文档明示风险（§3.4）；发布专用 token 列入待办 |
| 多源下载 | 恶意源给假清单/假文件 | 分块长度强校验 + 终态逐文件 size 校验不符即 failed（不静默接受坏权重） |
| D 面在线仓库 | 仓库 id / 文件路径注入 URL；token 泄露给 CDN | `validate_repo_id` 限 `[A-Za-z0-9._-]`（单 org 段+单 model 段）天然免注入；文件路径逐段 percent-encode；落盘前过 `validate_model_name` + `.part` 隔离 + 终态 size 校验；Authorization 跨主机重定向自动剥离（reqwest 默认） |

---

## 8. 已知权衡与待办

- **base64 传输开销**：网关 `ApiResponse` 契约是 JSON（`api_to_response` 恒
  `Content-Type: application/json`），二进制文件只能 base64（+33% 带宽）。缓解：客户端
  4 MiB 分块 + 服务端 offset/length（内存 O(块) 而非 O(文件)）。后续可给 share 加
  raw octet-stream 旁路（需 http.rs 挂原生 axum 路由，同 /git/* CGI 先例）。
- **清单以首源为准**：多源清单不一致（如某分享者只分享了部分文件）时以首个可达源为
  准；缺失文件经"失败换源"兜底。跨源清单并集/校验和（sha256）列入待办。
- **下载任务不持久化**：与既有 modelscope 任务同口径（内存态）；重启后大厅仍可查、
  `.part` 文件仍在（重发任务自动续传），但任务列表清空。SQLite 化列入待办。
- **HTTP Range 未用标准头**：续传走自定义 `offset/length` query（两端都是本组件，契约
  自洽）；对接第三方源（HF 等）时需适配层。（D 面已按标准 `Range: bytes=a-b` 对接
  魔搭/HF 镜像，此条仅剩 C 面 peer share 源。）
- **D 面顺序下载（无文件级并行）**：remote_repo 任务逐文件串行（单源无轮转分配），
  大模型多分片场景吞吐不及多源；单文件内 16 MiB 块连续 Range 对 CDN 已够跑满带宽。
  并行度调优与"魔搭 + HF 镜像双源混跑"列入待办。
- **D 面任务内存态**：与 C 面同口径，重启即失（`.part` 保留可续传）。
- **HF tree 分页**：>1000 文件的仓库只取第一页（Link header 分页未实现，模型场景罕见）。
- 发布专用 share token、大厅联邦索引同步（多节点 lobby 表互推，参考 NexHub 二期）。

---

## 9. 测试清单（30 + 12 个新增 + 1 个改写 + 6 个 E 面新增 + 8 个 HF 缓存新增）

| 面 | 测试 | 覆盖点 |
|---|---|---|
| A | parse_shard_filename_variants | 分片命名 6 变体（含位数不符/0 号） |
| A | analyze_shards_full_sequence_complete | 连续序列 + index → 完整 |
| A | analyze_shards_missing_middle_reports_gap | 缺中号 → missing=[3] → 不完整 |
| A | judge_complete_unsharded_requires_config_and_weight | 单文件模型三态判定 |
| A | parse_config_info_extracts_arch_fields | 架构五字段（含字符串数字） |
| A | scan_model_weight_detail_three_models_real_assertions | **三模型真实 FS 断言**（大小精确到字节/分片序号挂载/缺号/404） |
| A | get_model_detail_endpoint_e2e | HTTP 端到端（200/404/400） |
| A | delete_safety_matrix_validate | **删除安全校验矩阵**（7 场景纯函数） |
| A | delete_symlink_unlinks_without_touching_target | 链接删除不动目标（真实 rm） |
| A | import_model_link_validates_and_becomes_visible_in_list | 导入校验 + **list 可见** + 409 |
| A | import_endpoint_e2e_creates_link | POST /import 端到端 |
| B | build_source_url_and_lobby_id_pure | source_url/id/sharer 净化 |
| B | merge_lobby_rows_merges_same_name_and_sorts | 同 name 合并 + 排序 + 求和 |
| B | filter_lobby_entries_name_and_query | ?name=/?q= 过滤 |
| B | publish_requires_local_model | 本地不存在 404 / 非法名 400 |
| B | publish_creates_entry_with_source_url | 201 字段全断言 + 幂等刷新 |
| B | lobby_list_merges_dual_publishers_and_detail | **双发布者合并 sources=2** + 详情 + 搜索 |
| B | lobby_delete_permission_matrix | 匿名/他人 403、同 sharer 200、admin 200、404 |
| B | share_rejects_bad_token | 错/缺 token 401；未配置 admin token 401 |
| B | share_rejects_path_traversal | 明文 `..` 与 `%2e%2e` 均 400 |
| B | share_serves_file_chunks_and_rejects_directory | base64 往返/offset 切片/eof/嵌套文件/目录 400/越界 400/超长 400 |
| B | validate_share_rel_path_pure | 路径白名单纯函数 8 断言 |
| C | assign_files_round_robin_pure | **双源轮转分配纯函数**（4 组） |
| C | source_url_derivation_pure | split/derive_detail/build_share_file_url |
| C | resume_offset_pure | **续传判定**（不足续传/越界重下） |
| C | fetch_manifest_picks_first_reachable | 死源回落到可达源 + 全死 Err |
| C | multi_post_all_sources_dead_returns_502 | **清单拉取失败 502** + 参数校验 400 |
| C | multi_download_e2e_dual_source_failover | **假双源端到端**：换源/逐字节校验/bytes/简报/download_count 归因 |
| C | multi_download_resumes_existing_part | 预置 .part 续传（服务端实际收到 offset=400） |
| C | multi_download_size_mismatch_fails | 清单虚报大小 → **完成校验失败** → failed |
| C | multi_task_lifecycle_get_delete | **任务状态机**（建/查/删/404） |
| — | routes_declares_twenty_endpoints_all_model_hub | 20 条路由 + 鉴权声明（E 面再改写） |
| D | validate_repo_id_pure | org/model 形态校验（8 拒绝场景：单段/三段/空段/../空格/URL 元字符） |
| D | encode_path_pure | 路径逐段 percent-encode（空格/中文/`+`/`#`；unreserved 保留） |
| D | is_default_selected_pure | **向导默认勾选规则**（权重/config/tokenizer 勾；README/LICENSE/图片/pdf 不勾） |
| D | parse_modelscope_files_pure | **实测响应形态解析**（tree 滤除/排序/子目录路径；Code≠200 带上游 Message；缺数组/空清单 Err） |
| D | parse_hf_tree_pure | **实测 tree 响应解析**（directory 滤除；LFS 逻辑大小取顶层 size；非数组/空 Err） |
| D | remote_repo_kind_parse_and_slug | 源枚举解析（大小写/hf 别名）与 slug |
| D | remote_probe_modelscope_and_hf_mock | **双源探测端到端**（假源 TcpListener）：清单/大小/默认勾选/404 归一 |
| D | remote_probe_get_endpoint_routing | GET 探测端点路由（200/400 kind/404 不存在；三段 id 路由不匹配 404） |
| D | remote_download_e2e_selected_files_range_resume | **下载端到端**：勾选子集落盘、未勾不落盘、预置 .part 续传（服务端实收 `bytes=500-1199`）、逐字节校验、.part 无残留、三类任务混排可见 |
| D | remote_download_file_retry_then_fail | 单文件 404 → 重试 3 次 → 任务 failed 且错误指明文件与次数 |
| D | remote_download_validation_rejections | kind/repo_id/空 files/清单外路径 400；上游不可达 502 任务不入列 |
| D | remote_task_cancel_lifecycle | 建后即取消 → DELETE 200（type=remote_repo）→ 详情 404 |
| E | builtin_spark_zone_entries_valid_and_serializable | **策展表质检**：repo 形态/quant=NVFP4/org=首段/无重复/含两个用户点名仓 + JSON roundtrip |
| E | merge_spark_zone_entries_override_and_append | env 合并纯函数：同 repo 整条覆盖保位、新条目殿后、空 env 原表 |
| E | parse_spark_zone_env_shapes | env 文件两种形态解析（数组=合并 / replace 对象=替换）+ 5 种非法形态 Err |
| E | spark_zone_env_file_merge_replace_fallback | **env 覆盖端到端**（真实临时文件）：合并/替换/读失败回退/解析失败回退/非法 repo 条目剔除 |
| E | spark_zone_endpoint_probe_semantics_mock | **探测语义端到端**（FakeRemoteRepo 真 TCP）：双源可用（件数/大小精确断言）+ downloaded 标记 + `?probe=0` 零上游请求 + 单侧源死条目不剔除 |
| E | routes_declare_spark_zone_public_read | 路由注册契约（GET + 公开 + model_hub） |

测试基础设施：`FakeSource`（std TcpListener 单线程 HTTP 服务，GET only，`Connection:
close`，可注入文件表/失败文件/清单虚报大小/请求记录）+ `FakeRemoteRepo`（D 面：同款
TcpListener，实现魔搭 files + HF tree 双协议 JSON 端点 + resolve 二进制端点（**认真解析
`range: bytes=a-b` 并回 Content-Range**）+ 原始请求头记录）+ `ScopedModelsRoot` /
`ScopedEnvs`（env 覆盖互斥守卫，**共用一把 ENV_MUTEX、一次锁覆盖全部键**——std Mutex
不可重入，drop 自动清理，panic 不泄漏）。真实 API 不入测试（网络依赖）；开发期以
临时 ignored 测试对生产 API 验证后删除（§5D.2）。

---

## 10. 演进时间线

| 日期 | 事项 |
|---|---|
| 2026-08-13 | model_hub 初版：本地扫描 + modelscope 下载 + 推荐（9 路由） |
| 2026-08-21 | A 面权重档案/安全删除/导入；B 面大厅（SQLite model_lobby + 多源合并 + share 传输面）；C 面 lobby_multi 多源并行下载（17 路由，30 新测试） |
| 2026-08-31 | **D 面在线仓库源**：魔搭 ModelScope + HF 镜像 HTTP 直连（探测/文件级勾选向导/bounded Range 续传；19 路由，12 新测试；前端源选择 + i18n 四语） |
| 2026-09-02 | **E 面 Spark 专区**：SM120/NVFP4 精选策展（7 仓实测收录）+ 逐条两源实时可用性（3s 超时，?probe=0 可跳过）+ env `NEXOS_SPARK_ZONE_FILE` 覆盖；前端专区 Tab 复用下载向导 + 头部"非 Spark 专属"说明条 i18n 四语（20 路由，6 新测试） |
| 2026-09-03 | **HF hub 缓存自动扫描**（Spark 实测缺陷：模型装 `/home/nvidia/.cache/huggingface/hub`、服务跑 root 扫不到）：本地清单并入 HF 缓存（候选链 env 覆盖 + 全用户 glob；refs/main 定位 snapshot；id=org--name / source=hf_cache / path=snapshot 真实目录可直接建实例）；detail/stats 同口径、HF 条目删除 400 拒；手动添加强化（导入入口文案 + HF snapshot 路径示例）；前端 HF 徽章 + LlmModels 实例表单模型路径 datalist + i18n 四语（8 新测试，见 §3.0） |
| 待办 | 发布专用 token / share raw 流式旁路 / 任务 SQLite 持久化 / 清单并集+sha256 / 大厅联邦同步 / D 面文件级并行与双源混跑 / HF tree Link 分页 |

## 11. 关联文档与代码

- 代码：`crates/os-api/src/handlers/model_hub.rs`（后端唯一改动文件；handler 注册已在
  main.rs 既有 `model_hub` 组件，无需新组件注册）
- 前端（D 面本批已接）：`crates/os-api/web/src/views/ModelHub.vue`（下载 Tab 源选择
  向导 + remote_repo 任务卡片）+ `crates/os-api/web/src/api/client.ts`
  （`probeModelRepo` / `createModelRepoDownload`）+ i18n `modelhub` 命名空间
  （`web/src/i18n/locales/*.json` 四语全量；zh-TW 口径：模型/倉庫/下載/檔案/設定檔）
- 前端（E 面 Spark 专区）：`ModelHub.vue` 第 5 Tab「Spark 专区」（头部说明条 + 策展
  卡片 + 源可用性徽章 + 一键进既有下载向导 `openCreate(repo, 首选源)`）+
  `client.ts` `sparkZone({probe})` + i18n `modelhub.spark*` 18 键四语
  （zh-TW 繁化口径：專區/量化/通用/下載）
- 同类先例：`docs/NEXHUB_LOBBY_DESIGN.md`（大厅模式）、`crates/os-nexhub/src/nexhub_lobby.rs`
  （SQLite 建库）、`crates/os-api/src/handlers/im.rs`（Mutex<Connection> 惯例）、
  `crates/os-nexhub/src/code_repo.rs`（cached_hostname/HTTP 端口惯例）
