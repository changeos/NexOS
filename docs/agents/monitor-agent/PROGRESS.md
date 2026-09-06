# monitor-agent 进度日志

## 当前状态
- 阶段：OTel 真实指标采集 + prometheus 导出已接通（批 3，`p2/monitor-agent` 分支）；tracing 日志桥接仍留 TODO
- 最后更新：2026-08-05

## 已完成
- [x] 告警引擎纯逻辑层（`condition` 模块 + `AlertEngine` 状态机 + `AlertState`/`EvalOutcome`）：
  - 条件表达式解析（`>0.9`/`>=0.9`/`<100`/`<=100`/`==0`/`!=1`，含空白容忍）
  - 抖动抑制状态机（Inactive → Pending → Firing → Resolved，`for_duration_secs` 窗口判断）
  - 去重（同一规则同时刻最多一个 Firing）
- [x] 数据模型（`Metric`/`MetricKind`/`MetricPoint`/`Sample`/`LogEntry`/`LogLevel`/`AlertRule`/`Alert`/`AlertSeverity`/`LogFilter`）
- [x] **OtelMonitor 真实 OTel 接通**（`monitor.rs`）：
  - 指标采集用 `opentelemetry::metrics::{Counter<u64>, Gauge<f64>, Histogram<f64>}` 经 `SdkMeterProvider` 聚合
  - 按 `MetricKind` 分派：Counter 单调累加（f64→u64 饱和）/ Gauge last-write-wins（覆盖）/ Histogram 单点观测（SDK 内桶聚合）
  - 仪器句柄按 `name+labels` lazy 创建并缓存（OTel 仪器 Clone+Send+Sync，避免重复注册同名仪器）
  - `render_metrics()`：`registry.gather() + TextEncoder::encode()` 生成 Prometheus exposition v0.0.4 文本，供 axum `/metrics` 端点
  - `metrics_content_type()`：返回 `text/plain; version=0.0.4; charset=utf-8`（供 axum 设响应头）
  - exporter 配置：`without_target_info()` + `without_scope_info()`（关闭噪声 metric，OS 单租户无需 resource 区分）+ `service.name=os` resource
  - 保留内存时序（`metrics: HashMap<name, Vec<Metric>>`）——OTel 仅保留聚合态不保留原始样本，`query_metrics` 与告警引擎抖动窗口依赖原始时序，故二者并存
  - 告警引擎联动真实采集：`record_metric` 内 ingest 样本 → Fired/Resolved 推进 alerts 列表（端到端测覆盖）
- [x] MockMonitor（feature `mock`，`monitor::mock` 子模块）：纯内存确定性实现，复用 AlertEngine，含错误注入

## 依赖接入（ADR-DEPS-002 续）
- workspace 根 `[workspace.dependencies]` 新增：
  - `opentelemetry_sdk = "0.32"`（SdkMeterProvider/Resource；opentelemetry-prometheus 0.32 内部传递依赖 0.32.x，显式注册便于直接 use）
  - `prometheus = { version = "0.14", default-features = false }`（Registry/TextEncoder；opentelemetry-prometheus 0.32 内部传递依赖，显式注册便于 /metrics 端点直接编码）
  - （`opentelemetry`/`opentelemetry-prometheus` 已由 ADR-DEPS-002 注册）
- crate 级 `os-services/Cargo.toml`：`opentelemetry`/`opentelemetry_sdk`/`opentelemetry-prometheus`/`prometheus` 四个 `.workspace = true`。
- **未引入** tracing-subscriber（日志桥接留后续，未在本次任务范围）。

## 测试与质量门
- `cargo check -p os-services --features mock` → 0 error
- `cargo test -p os-services --features mock` → **263 passed**, 0 failed
  - monitor 模块 **28 个测试**（原 20 + 新增 8 OTel 导出测）：
    gauge 出现/counter 累加/gauge last-write-wins/histogram bucket/labels 区分/空输出/content-type/告警+导出端到端
- `cargo test -p os-services`（默认无 mock）→ 全绿
- `cargo clippy -p os-services --features mock -- -D warnings` → 0 warning
- `cargo clippy -p os-services -- -D warnings` → 0 warning
- `cargo doc -p os-services --features mock --no-deps` → 0 warning
- `cargo check --workspace` → 0 error（workspace 注册未影响其他 crate）

## 阻塞
- 无（OTel 真实指标采集 + prometheus 导出已接通）

## 下一步
1. tracing-subscriber 桥接（`tail_logs` 真实日志源；当前 OtelMonitor.logs 为空，需注册 tracing 后接 layer 收集）
2. axum `/metrics` 路由（api-agent 接 `OtelMonitor::render_metrics` + `metrics_content_type`，本 agent 仅提供方法）
3. OTLP exporter（如需推模式而非拉模式，注册 opentelemetry-otlp 后接 OTLPMeterExporter）

## 协作备注
- 共享 crate `os-services`：本次仅改 `monitor.rs`（OtelMonitor + 模块文档 + 新增 8 测）+ `Cargo.toml`（加 4 个 dep）；**未碰** files/media/backup/devtools/power 实现、`error.rs`、`lib.rs`、`mock.rs`。
- workspace 根 `Cargo.toml`：仅扩 ADR-DEPS-002 的可观测性分区（加 `opentelemetry_sdk`/`prometheus` 两行 + 注释），未改其他分区。
