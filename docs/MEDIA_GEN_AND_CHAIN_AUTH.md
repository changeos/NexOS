# 媒体生成 API + NexHub 链上身份（设计定稿 2026-08-18）

> 用户需求：模型管理增加图片生成与 API、视频生成与 API；NexHub 大厅的项目权限与
> 更改都要走区块链认证。

## A. 图片生成（真实能力：本地 sd-turbo）

- 新 handler `media_gen.rs`（组件 media-gen），端点：
  - `POST /api/v1/media/image` {prompt, width?, height?, steps?} → PNG base64
    （默认 768×432/4 步，与壁纸管线同款；产出经 /tmp/media-gen 落盘再编码返回）
  - `GET /api/v1/media/image/recent` 近期生成记录（内存环形，最近 50 条，不含 base64）
- **计费挂钩**：调用时经 api_gateway 的 `charge_image_call` 语义——按调用方 sk-os-/
  admin 场景先简化为：admin token 直接放行（内网期），网关接入列 TODO（与
  per_image 计费模式对接的接口已在网关侧备好）
- **GPU 互斥处理**：llm-101（Qwen3，22G）运行时 sd-turbo 放不下——探测显存
  （nvidia-smi 查询），不足时返回 503 + 明确提示"先停推理实例"；生图进程短生命周期
  （生成即退，释放显存）
- 实现：spawn python（diffusers 环境 /home/oem/.local，复用壁纸脚本路径）——
  Rust 原生 diffusers 不存在，spawn 是当前合理选择；超时 60s

## B. 视频生成（框架先行，后端可插）

- 同 handler：
  - `POST /api/v1/media/video` {prompt, duration?, backend?} → 任务 {id, status:queued}
  - `GET /api/v1/media/video/tasks` / `GET /api/v1/media/video/tasks/:id`
  - 任务生命周期：queued→processing→completed(url)|failed(error)
  - **后端接口抽象** `VideoBackend` trait：`local`（未来本地模型）/`external`（外部
    API，读 env NEXOS_VIDEO_API_URL/KEY 配置）；当前无已配后端 → 任务创建即
    failed，error 明确"未配置视频后端"（诚实，不假装排队）
- UI 在模型管理页新增「生成」区：图片生成表单+预览、视频任务列表

## B+. 端点契约表（实现定稿，2026-08-20 核对源码）

| method | path | 鉴权 | 请求 | 成功响应 | 错误码 |
|--------|------|------|------|----------|--------|
| POST | `/api/v1/media/image` | admin | `{prompt, width?, height?, steps?}`（宽高默认 768/432、steps 默认 4） | `{id, png_base64, width, height, elapsed_ms, file_path}`（PNG 先落 `/tmp/media-gen/<token>.png` 再 base64） | 400 参数非法 / 503 显存不足或探测不可用 / 502 生成失败或超时 |
| GET | `/api/v1/media/image/recent` | 公开 | — | `[{id, prompt_summary(前120字), width, height, steps, elapsed_ms, created_at}]`（内存环形 ≤50 条） | — |
| POST | `/api/v1/media/video` | admin | `{prompt, duration_secs?, backend?}`（duration 默认 5、backend 默认 external） | 202 `{id, prompt, duration_secs, backend, status, video_url, error, created_at}`；创建即尝试 submit：成功 processing / 失败 failed（error 附用户指引） | 400 参数非法 |
| GET | `/api/v1/media/video/tasks` | 公开 | — | `VideoTask[]`（内存态，重启即空） | — |
| GET | `/api/v1/media/video/tasks/:id` | 公开 | — | 单个 `VideoTask` | 404 |

**参数校验规则**（源码 `validate_image_params` / `validate_video_params`）：

- prompt 两类共用：非空、≤2000 字；
- **宽高：仅显式传入的值受"64 的倍数 + 256..=1024"约束**；默认 768×432（16:9
  壁纸管线同款）不受该约束——432 本就不是 64 的倍数（diffusers 只要求 8 的倍数），
  64 倍数规则只为拦调用方笔误（如 433/100），默认值放行；
- steps 1..=8（默认 4）；duration_secs 1..=30（默认 5）；backend ∈ {external, local}；
- 显存门槛：空闲 < **6000 MiB** → 503"先停 LLM 实例再生成"（多卡取最大空闲值），
  nvidia-smi 探测本身不可用也 503（无法确认安全，默认拒绝）。
  **统一内存回退**（GB10/Jetson，2026-09-03）：`memory.free` 报 `[N/A]`
  （DGX Spark 实测——无独立显存，CPU/GPU 共享 LPDDR5x 池）→ 回退
  `/proc/meminfo` MemAvailable（vLLM 占的就是这个池，闸门语义不变）；
  其余不可解析输出（无 `[N/A]` 标记）仍默认拒绝。

## B++. 环境变量（media_gen.rs 全部注入点，grep 核实）

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_IMGGEN_BIN` | `python3` | 生图可执行路径覆写（测试/运维注入假脚本） |
| `NEXOS_IMGGEN_SCRIPT` | `/tmp/nexos-imggen.py` | 生图管线脚本路径；缺失或内容有变化时自动落盘（内嵌 diffusers 文生图脚本） |
| `NEXOS_IMGGEN_TIMEOUT_SECS` | `60`（钳制 1..=300） | 生图子进程超时秒数，超时 kill（`kill_on_drop`） |
| `NEXOS_SMI_BIN` | `nvidia-smi` | 显存探测二进制覆写（测试注入假脚本模拟输出） |
| `NEXOS_SD_MODEL` | `/tank/models/sd-turbo` | sd-turbo 模型路径（python 脚本内读取，fp16 + cuda） |
| `NEXOS_VIDEO_API_URL` | 未设置 | 外部视频后端 API 地址；未设置时 external 后端 submit 直接报"未配置"（任务即 failed）。注意：URL 已设置时当前也诚实失败（HTTP 客户端尚未接入） |
| `NEXOS_VIDEO_API_KEY` | 未设置 | 外部视频后端 API 密钥；**当前 Rust 侧未读取**，仅出现在失败指引文案中，预留给 external 后端客户端接入时使用 |

子进程内部传递变量（非配置项，spawn 时由 Rust 注入）：`NEXOS_IMGGEN_PROMPT` /
`_WIDTH` / `_HEIGHT` / `_STEPS` / `_OUT`、`NEXOS_SD_MODEL`（python 侧读取）。

## C. NexHub 链上身份与权限（复用 IM 的挑战-签名模式）

**核心：身份=secp256k1 公钥，权限=私钥持有者**。与 IM 同款三步认证。

> 状态（2026-08-18）：批次 2 后端已落地——共享内核 `os_common::chain_auth::ChainAuth`
> （IM 的 `ImAuth` 改为其类型别名，端点/契约零变化）+ os-nexhub `auth/challenge|verify`
> 端点 + 全部写端点的 token 反查权限执行；已知限制①②已修复（详见
> `docs/NEXHUB_LOBBY_DESIGN.md` §11.7 更新），仅剩 `/git/*` 通道付费校验二期。

- **抽取共享**：IM 的 ImAuth（nonce/token 桶 + k256 验签）抽到 `os-common::
  chain_auth`（os-nexhub 已依赖 os-common；IM 改为消费共享实现，端点/契约不变）
- **NexHub 认证端点**：`POST /api/v1/nexhub/auth/challenge|verify`（同款契约，
  token 24h 单点登录）
- **权限执行**（全部服务端从 token 反查 pubkey，body 自报字段忽略）：
  - publish：publisher=调用者 pubkey（展示名派生 EVM 地址）
  - 重发布/下架：仅 publisher 本人或系统 admin token
  - bounty：poster=调用者；claim 的 hunter、approve/reject 的操作者=token 身份
    ——**修掉审计"已知限制②"（身份自报）**
  - purchase/entitlements：buyer=token 身份——**部分修复"已知限制①"的冒名豁免**
    （/git/* 通道的付费校验仍列二期）
- **存量兼容**：现有条目 publisher 为字符串（NexOS/zcode）→ 视为"平台托管"，
  仅系统 admin token 可改；新发布一律 pubkey
- 前端：useImIdentity 泛化为 useChainIdentity（同一密钥对，IM/NexHub 共用身份），
  大厅发布/下架/悬赏操作走链上 token

## 分批

| 批次 | 内容 | 文件域 |
|---|---|---|
| 1 | A+B 后端（media_gen.rs + spawn 管线 + 显存探测 + 视频任务框架） | os-api |
| 2 | C 后端（os-common chain_auth 抽取 + IM 消费改造 + os-nexhub 认证与权限执行 + 存量兼容） | os-common/os-api/im.rs/os-nexhub |
| 3 | 前端（模型管理生成区 + 大厅身份集成） | web/ |

批次 1/2 文件不相交，**并行派发**；批次 3 串行收尾。
