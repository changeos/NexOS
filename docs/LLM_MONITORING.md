# vLLM 实例轻量监控（LLM Monitoring）

> 源码：`crates/os-api/src/handlers/llm.rs`（handler 组件名 `llm`）。
> 本文只覆盖**实例级轻量监控**；os-api 进程级 `/metrics`（OTel/Prometheus）是另一
> 套，见 docs/DEPLOYMENT.md §8。

## 1. 端点契约

| method | path | 鉴权 | 说明 |
|--------|------|------|------|
| GET | `/api/v1/llm/instances/:id/metrics` | 公开读 | 抓取该实例 vLLM 的 `/metrics`，解析为指标快照 |

响应体（`InstanceMetricsResponse`）：

| 字段 | 类型 | 说明 |
|------|------|------|
| instance_id | String | 实例 id |
| reachable | bool | 真实 vLLM /metrics 是否抓取成功 |
| simulated | bool | metrics 是否为合成模拟数据（仅模拟模式且真实端口不通时 true） |
| collected_at | String | 采集时刻（ISO 8601 本地时间） |
| base_url | String | 抓取目标 `http://127.0.0.1:<port>` |
| metrics | Snapshot \| null | 指标快照；不可达时 null |

快照字段（`InstanceMetricsSnapshot`，缺失为 null——vLLM 版本差异 / Counter 无历史）：

| 字段 | 说明 |
|------|------|
| num_requests_running | 运行中请求数（Gauge） |
| num_requests_waiting | 排队请求数（Gauge） |
| gpu_cache_usage | KV cache 占用率 0-1（Gauge） |
| prefix_cache_hit_rate | prefix cache 命中率 0-1（Gauge） |
| generation_tokens_per_sec | 生成 token 速率（Counter 差值/秒；首次采样无历史为 null） |
| prompt_tokens_per_sec | prompt token 速率（同上） |
| requests_success_per_sec | 完成请求速率（同上） |
| e2e_latency_ms | 端到端请求时延均值（`e2e_request_latency_seconds` sum/count，毫秒） |

## 2. 行为要点

- **按需采集，零后台开销**：无轮询任务，API 调用时才抓 vLLM
  `GET http://127.0.0.1:<port>/metrics`（Prometheus 文本轻量逐行解析，不引入
  prometheus crate）；同实例 **5s 内存缓存**去抖（`METRICS_CACHE_TTL`），抓取
  超时 **3s**（`METRICS_FETCH_TIMEOUT`）。
- **Counter 速率**：token / request_success 是 Counter，需两次采样差值算速率；
  采样历史按实例存内存，重启即清零（重新预热一次后恢复）。
- **降级语义**：实例不存在 404；不可达时 **200** + `reachable:false` +
  `metrics:null`（监控探测不是错误），绝不伪造。

## 3. 环境变量

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_LLM_METRICS_SIMULATE` | 未设置（纯真实模式） | 设 `1` 或 `true` 开启模拟模式：先 200ms 探测真实端口，通则仍用真实数据；不通才返回时间种子 sin 波合成的平滑模拟数据（`simulated:true` + `reachable:false`），供 GPU 被占用（如 sd-turbo 生图互斥）时前端联调 |
| `NEXOS_LLM_DB` | 未设置 | 实例定义持久化 DB 路径覆盖（见 §4）。未设置时按 `/tank/os-data/llm.db` → `/var/lib/os/llm.db` → `./llm.db` 顺序探测（forwarding.db 同款惯例） |

## 4. 实例定义持久化（2026-08-22）

> 痛点：实例定义原先只在内存（`Vec<ModelInstance>`），服务重启即丢——
> 每次重启都要手动重建。本批起定义落 SQLite，重启后恢复；**不自动拉起**
> （用户裁决：手动启动即可）。

- **存储**：`llm.db` 的 `llm_instances` 表（WAL + `CREATE TABLE IF NOT EXISTS`
  幂等建表）。列：`id` PK / `name` / `model` / `source_type` / `port` /
  `config`（`VllmConfig` 的 JSON 序列化）/ `status` / `pid` / `error` /
  `created_at`。
- **双写点**：创建（POST /instances）、删除（DELETE /:id）、启动（/:start，
  含 spawn 失败的 error 态）、停止（/:stop）、健康探测 status 翻转
  （/:health starting→running）——内存态是运行时真值，落表仅供重启恢复；
  写表失败只影响恢复，不影响当次请求。
- **重启恢复**：服务启动（`LlmRouteHandler::new()`）从表加载全部定义，
  **status 一律重置 `stopped`、`pid`/`error` 清空、health 置 None**——旧
  pid 跨重启不可信（可能被复用），绝不自动拉起。`config`/`source_type`/
  `port`/`name`/`model`/`created_at` 原样还原；id 计数器越过已恢复的最大
  数字后缀（新建实例不撞 id）。
- **首次开库**：表空（全新部署）seed 2 个 demo 实例并落表（与旧内存态
  行为对齐）；之后重启以表内定义为准，不再重复 seed。
- **测试面（P1-P5）**：创建→重启→定义在（stopped/pid/error 重置 + config
  全字段 roundtrip）；删除→重启→不在；运行态落表→stop 同步 stopped/pid
  NULL；表里有 running/pid→重启强制重置；多实例恢复 + 新建 id 不冲突。

## 5. GPU 探测与统一内存（GB10/Jetson，2026-09-03）

> 背景：DGX Spark（NVIDIA GB10 / Grace Blackwell 超芯片，aarch64）页面显示
> 「未检测到 GPU」——实测 nvidia-smi 本身工作正常，csv 输出为
> `0, NVIDIA GB10, [N/A], [N/A], [N/A], 0`（显存三列 `[N/A]`：GB10 统一内存
> 架构无独立显存），旧解析器 `[N/A].parse::<u64>()` 失败丢行 → devices 空 →
> 判 `available:false`。

### 语义（修复后）

- **有输出即算有 GPU**：csv 行 index+name 可解析即成卡，显存字段
  `[N/A]`/解析失败 → `memory_total_mib`/`memory_used_mib`/`memory_free_mib`
  = `null`（Option 化，**不再当无卡**），`utilization_pct` 同样容 N/A。
- **统一内存回退**：显存总量报不出（`[N/A]`）即置 `unified_memory=true`，
  并从 `/proc/meminfo` 填 `unified_memory_total_mib`（MemTotal）、
  `unified_memory_free_mib`（MemAvailable）、`unified_memory_used_mib`
  （差值）——CPU/GPU 共享同一 LPDDR5x 池（Spark 实测 127600528 kB ≈
  121.7 GiB），与 `/monitor/metrics` 同源同口径（monitor.rs `read_meminfo`）。
- **识别方式**：device-tree 在 Spark 上**不存在**（Ubuntu/ACPI 形态，`/proc/
  device-tree/` 为空），故以 nvidia-smi 形态为准——独立显存卡（RTX 3090 等
  数值形态）恒报数字，`[N/A]` 即统一内存架构；识别不了（meminfo 也读不到）
  就 `null` 字段与统一内存标记并存，前端如实展示。
- **端点影响**：`GET /api/v1/llm/gpu`（devices 带 unified 字段）、
  `GET /api/v1/llm/stats`（GB10 算 1 卡）、`GET /api/v1/llm/gateway/health`
  （`gpu_memory_total_mib` 只累加已知独立显存——GB10 诚实为 0，新增
  `gpu_unified_memory:true` 告知消费方容量语义在设备级 unified 字段）。
- **前端**：GPU 徽章 `GB10 · 统一内存 121.7 GB`（memory_total=null +
  unified=true）；GPU 监控 Tab「独立显存 N/A（CPU/GPU 共享统一内存）」+
  共享池总量/已用/可用三行 + 内存占用条（unified 口径）；独立显存卡展示
  零变化。
- **rocm 路径零回归**：rocm-smi 解析恒为独立显存（Some 数值 +
  unified=false）。
