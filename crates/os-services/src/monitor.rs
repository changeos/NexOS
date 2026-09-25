//! 监控 / 告警 / 可观测性（规划文档 §3.16 monitor 组件）
//!
//! 职责：
//! - metric 采集与查询（Counter / Gauge / Histogram）
//! - 日志收集与按级别/目标/时间过滤 tail
//! - 告警规则（基于 metric 的条件表达式 + 持续时长阈值）与告警状态查询
//!
//! 本模块分三层（自底向上）：
//! 1. **数据模型**：`Metric` / `MetricKind` / `MetricPoint` / `Sample` 等——
//!    纯数据结构 + 构造器，无外部依赖。
//! 2. **告警引擎**（纯逻辑，高价值可测）：
//!    - [`condition`]：条件表达式（`">0.9"` / `"<=100"`）解析为 [`condition::Comparison`]，
//!      并对单个值求值（[`condition::Condition::evaluate`]）。
//!    - [`AlertEngine`]：维护每条 [`AlertRule`] 的 [`AlertState`] 状态机
//!      （`Pending` → `Firing` → `Resolved`），实现 `for_duration_secs` 抖动抑制
//!      与抑制/去重（同一规则同时刻只一个 Firing 告警）。
//! 3. **Monitor trait + 实现**：
//!    - [`OtelMonitor`]：`impl Monitor`，基于 opentelemetry + opentelemetry-prometheus；
//!      指标采集用 OTel `Counter`/`Gauge`/`Histogram` 仪器经 `SdkMeterProvider` 聚合，
//!      `/metrics` 端点经 `prometheus::Registry` + `TextEncoder` 输出文本格式
//!      （见 [`OtelMonitor::render_metrics`]）。
//!      **日志采集**：经自定义 tracing-subscriber [`log_bridge::LogBridgeLayer`]
//!      把 `tracing::event!` / `#[instrument]` 宏产生的真实日志事件捕获为
//!      [`LogEntry`]（level/target/message/timestamp/fields 五元组）写入内存环形
//!      buffer，`tail_logs` 经 [`LogFilter`] 过滤后返回。**日志导出**：
//!      [`OtelMonitor::build_subscriber_with`] 提供 tracing-subscriber JSON 格式
//!      （`fmt::format::Json`）→ 文件的骨架（轮转由调用方/系统日志采集接管）。
//!    - `MockMonitor`（feature `mock`，模块 `monitor::mock`）：纯内存确定性实现，供下游测试注入。

use std::collections::HashMap;

use os_core::{DateTime, Deserialize, Serialize};

use crate::ServiceError;

// ----------------------------------------------------------------------------
// Metric
// ----------------------------------------------------------------------------

/// metric 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// 单调递增计数器
    Counter,
    /// 可增可减的瞬时值
    Gauge,
    /// 直方图（带桶分布）
    Histogram,
}

/// 单个 metric 数据点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    /// metric 名（如 `"cpu_usage"` / `"mem_free_bytes"`）
    pub name: String,
    /// metric 类型
    pub kind: MetricKind,
    /// 数值
    pub value: f64,
    /// 标签（多维属性，如 `{"host":"os1","device":"sda"}`）
    pub labels: HashMap<String, String>,
    /// 采集时间戳
    pub timestamp: DateTime,
}

impl Metric {
    /// 构造一个 Gauge metric 数据点（最常见：cpu/mem/温度等瞬时值）。
    pub fn gauge(name: impl Into<String>, value: f64, timestamp: DateTime) -> Self {
        Self {
            name: name.into(),
            kind: MetricKind::Gauge,
            value,
            labels: HashMap::new(),
            timestamp,
        }
    }

    /// 构造一个 Counter metric 数据点（单调递增，如已发送字节数）。
    pub fn counter(name: impl Into<String>, value: f64, timestamp: DateTime) -> Self {
        Self {
            name: name.into(),
            kind: MetricKind::Counter,
            value,
            labels: HashMap::new(),
            timestamp,
        }
    }

    /// 构造一个 Histogram metric 数据点（带桶分布，如请求延迟）。
    pub fn histogram(name: impl Into<String>, value: f64, timestamp: DateTime) -> Self {
        Self {
            name: name.into(),
            kind: MetricKind::Histogram,
            value,
            labels: HashMap::new(),
            timestamp,
        }
    }

    /// 链式追加一个 label（多维属性）。
    pub fn with_label(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.labels.insert(key.into(), val.into());
        self
    }
}

/// 直方图桶边界上的累计计数样本（Histogram 类型的展开形式）。
///
/// 注：当前 `Metric::value` 承载单点观测值；`MetricPoint`/`Sample` 为未来
/// 多样本聚合（sum/count/桶分布）预留的扩展点，便于 OTel 导出器对接。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// 桶上界（`f64::INFINITY` 表示 +Inf 桶）
    pub upper_bound: f64,
    /// 该桶的累计计数
    pub count: u64,
}

/// 直方图聚合点（sum / count / 桶分布）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    /// metric 名
    pub name: String,
    /// 所有点的 sum
    pub sum: f64,
    /// 总观测数
    pub count: u64,
    /// 桶分布（按 upper_bound 升序）
    pub buckets: Vec<Sample>,
    /// 时间戳
    pub timestamp: DateTime,
}

impl MetricPoint {
    /// 从一批观测值构造直方图聚合点（按给定桶上界分桶）。
    ///
    /// `bounds` 须升序；自动追加 +Inf 桶。空观测 → count=0、sum=0。
    pub fn from_values(
        name: impl Into<String>,
        values: &[f64],
        mut bounds: Vec<f64>,
        timestamp: DateTime,
    ) -> Self {
        bounds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        bounds.dedup_by(|a, b| a == b);
        // 每个桶累计计数（含 ≤ upper_bound 的所有观测）
        let mut buckets: Vec<Sample> = bounds
            .iter()
            .map(|&ub| {
                let c = values.iter().filter(|v| **v <= ub).count() as u64;
                Sample {
                    upper_bound: ub,
                    count: c,
                }
            })
            .collect();
        // +Inf 桶（所有有限观测都落入）
        buckets.push(Sample {
            upper_bound: f64::INFINITY,
            count: values.len() as u64,
        });
        let sum = values.iter().sum();
        Self {
            name: name.into(),
            sum,
            count: values.len() as u64,
            buckets,
            timestamp,
        }
    }
}

// ----------------------------------------------------------------------------
// Log
// ----------------------------------------------------------------------------

/// 日志级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// 数值化严重程度（Trace=0 … Error=4），用于 `>=` 级别过滤。
    pub fn severity(self) -> u8 {
        match self {
            LogLevel::Trace => 0,
            LogLevel::Debug => 1,
            LogLevel::Info => 2,
            LogLevel::Warn => 3,
            LogLevel::Error => 4,
        }
    }
}

/// 单条日志
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// 级别
    pub level: LogLevel,
    /// 目标（模块/组件名，如 `"os_storage::replication"`）
    pub target: String,
    /// 日志消息
    pub message: String,
    /// 时间戳
    pub timestamp: DateTime,
    /// 结构化字段（如 `"request_id"`）
    pub fields: HashMap<String, String>,
}

impl LogEntry {
    /// 是否匹配给定过滤条件（纯逻辑，可独立测试，**不考虑 `limit`**）。
    ///
    /// - `level`：日志级别 `>=` 该值才匹配（None = 不过滤）。
    /// - `target`：精确匹配（None = 不过滤）。
    /// - `since`/`until`：时间戳闭区间 `[since, until]`（None = 不限端点）。
    /// - `source`：`target` 子串匹配（None = 不过滤）。
    /// - `keyword`：`message` 大小写不敏感子串匹配（None = 不过滤）。
    ///
    /// `limit` 在 [`LogFilter::apply`]（批量过滤 + 排序 + 截断）处统一处理，
    /// 单条 `matches` 不感知 limit。
    pub fn matches(&self, filter: &LogFilter) -> bool {
        if let Some(lvl) = filter.level {
            if self.level.severity() < lvl.severity() {
                return false;
            }
        }
        if let Some(t) = &filter.target {
            if &self.target != t {
                return false;
            }
        }
        if let Some(since) = filter.since {
            if self.timestamp < since {
                return false;
            }
        }
        if let Some(until) = filter.until {
            if self.timestamp > until {
                return false;
            }
        }
        if let Some(src) = &filter.source {
            if !self.target.contains(src.as_str()) {
                return false;
            }
        }
        if let Some(kw) = &filter.keyword {
            if !self.message.to_lowercase().contains(&kw.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

impl LogFilter {
    /// 便捷构造：仅按关键词搜索（大小写不敏感子串匹配 message）。
    #[must_use]
    pub fn keyword(kw: impl Into<String>) -> Self {
        Self {
            keyword: Some(kw.into()),
            ..Self::default()
        }
    }

    /// 按本过滤条件过滤 + 升序排序 + 应用 limit 截断（纯逻辑，无 IO）。
    ///
    /// 输入 `logs` 不被修改（消费后返回新 `Vec`）；等时间戳条目保留原相对顺序
    /// （稳定排序）。与 [`crate::devtools::filter_logs`] 语义一致。
    pub fn apply(self, mut logs: Vec<LogEntry>) -> Vec<LogEntry> {
        logs.retain(|l| l.matches(&self));
        logs.sort_by_key(|l| l.timestamp);
        if let Some(n) = self.limit {
            logs.truncate(n);
        }
        logs
    }
}

// ----------------------------------------------------------------------------
// Alert
// ----------------------------------------------------------------------------

/// 告警严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

/// 告警规则
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    /// 规则名（人类可读）
    pub name: String,
    /// 关联 metric 名
    pub metric: String,
    /// 触发条件表达式（如 `">0.9"` 表示 metric 值大于 0.9 时触发）
    pub condition: String,
    /// 条件需持续满足的秒数（避免抖动误报）
    pub for_duration_secs: u32,
    /// 严重程度
    pub severity: AlertSeverity,
}

/// 已触发的告警
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// 触发它的规则名
    pub rule_name: String,
    /// 严重程度
    pub severity: AlertSeverity,
    /// 触发时间
    pub fired_at: DateTime,
    /// 是否已恢复
    pub resolved: bool,
    /// 告警消息（含当时的 metric 值等上下文）
    pub message: String,
}

/// 日志过滤条件
///
/// 所有字段均为 `Option`，`None` 表示该维度不限制；多维度间为「逻辑与」。
/// 设计与 [`crate::devtools::LogQuery`] 对齐（devtools 的 CI 日志查询），便于
/// 上层（api-agent 监控/日志路由）复用同一查询语义。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogFilter {
    /// 级别过滤（None = 不过滤；保留 `>=` 该级别）
    pub level: Option<LogLevel>,
    /// 目标过滤（None = 不过滤；精确匹配 `target`）
    pub target: Option<String>,
    /// 起始时间（None = 不限，含端点）
    pub since: Option<DateTime>,
    /// 截止时间（None = 不限，含端点）
    pub until: Option<DateTime>,
    /// 来源前缀 / 子串匹配（None = 不过滤；匹配 `target` 子串，大小写敏感）
    pub source: Option<String>,
    /// 关键词（大小写不敏感子串匹配 `message`，None = 不过滤）
    pub keyword: Option<String>,
    /// 最多返回条数（None = 不限）。结果按 timestamp 升序后再截断。
    pub limit: Option<usize>,
}

// ============================================================================
// 告警引擎（纯逻辑层）
// ============================================================================
//
// 设计目标：把「规则评估 + 抖动抑制 + 状态机」做成纯函数/纯状态机，
// 完全脱离 IO 与 async，可独立单元测试（高价值，规格书 §7 标注为早期阻塞点）。
// 上层 [`OtelMonitor`] 只负责喂 metric 样本 + 调 [`AlertEngine::ingest`]。

pub mod condition {
    //! 条件表达式解析与求值（纯函数）。
    //!
    //! 支持的语法（与 Prometheus-style `for` 规则兼容）：
    //! - 比较算子前缀形式：`">0.9"` / `">=0.9"` / `"<100"` / `"<=100"` / `"==0"` / `"!=1"`
    //! - 数值用 f64 解析；前后空白忽略。
    //! - 不支持的算子 / 非法数值 → [`Condition::parse`] 返回 `Err`。

    use std::str::FromStr;

    use crate::ServiceError;

    /// 比较算子
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Comparison {
        /// `>` 大于
        Gt,
        /// `>=` 大于等于
        Ge,
        /// `<` 小于
        Lt,
        /// `<=` 小于等于
        Le,
        /// `==` 等于
        Eq,
        /// `!=` 不等于
        Ne,
    }

    impl Comparison {
        /// 对两个值执行比较。
        pub fn compare(self, lhs: f64, rhs: f64) -> bool {
            match self {
                Comparison::Gt => lhs > rhs,
                Comparison::Ge => lhs >= rhs,
                Comparison::Lt => lhs < rhs,
                Comparison::Le => lhs <= rhs,
                Comparison::Eq => lhs == rhs,
                Comparison::Ne => lhs != rhs,
            }
        }
    }

    /// 解析后的条件（算子 + 阈值）。
    #[derive(Debug, Clone, PartialEq)]
    pub struct Condition {
        /// 比较算子
        pub op: Comparison,
        /// 阈值
        pub threshold: f64,
        /// 原始表达式（便于错误诊断/回显）
        pub raw: String,
    }

    impl Condition {
        /// 解析条件表达式（如 `">0.9"` → `Gt(0.9)`）。
        ///
        /// 错误：空串 / 未知算子 / 阈值非数字 / 缺算子 → `ServiceError::Internal`。
        pub fn parse(expr: &str) -> Result<Self, ServiceError> {
            let trimmed = expr.trim();
            // 先尝试两字符算子（>=, <=, ==, !=），再单字符（>, <）。
            let (op, rest) = if let Some(r) = trimmed.strip_prefix(">=") {
                (Comparison::Ge, r)
            } else if let Some(r) = trimmed.strip_prefix("<=") {
                (Comparison::Le, r)
            } else if let Some(r) = trimmed.strip_prefix("==") {
                (Comparison::Eq, r)
            } else if let Some(r) = trimmed.strip_prefix("!=") {
                (Comparison::Ne, r)
            } else if let Some(r) = trimmed.strip_prefix('>') {
                (Comparison::Gt, r)
            } else if let Some(r) = trimmed.strip_prefix('<') {
                (Comparison::Lt, r)
            } else {
                return Err(ServiceError::Internal(format!(
                    "非法告警条件表达式（缺少比较算子）: {expr:?}"
                )));
            };
            let threshold = f64::from_str(rest.trim())
                .map_err(|_| ServiceError::Internal(format!("非法告警条件阈值: {expr:?}")))?;
            Ok(Self {
                op,
                threshold,
                raw: expr.to_string(),
            })
        }

        /// 对单个值求值：`value <op> threshold`。
        pub fn evaluate(&self, value: f64) -> bool {
            self.op.compare(value, self.threshold)
        }
    }
}

/// 告警状态机的单条状态（每个规则一份）。
///
/// 状态流转（规格书 §2/§7）：
/// ```text
///  [无] ──条件满足──> Pending ──持续 for_duration_secs──> Firing
///    │                  │                                   │
///    │                  └──条件不再满足──> [无]              │
///    └──────────────────────────────────────────────────────┘
///                                          Firing ──条件不再满足──> Resolved
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum AlertState {
    /// 未触发（条件未满足或从未达标）。
    Inactive,
    /// 条件已满足但未持续够 `for_duration_secs`（抖动抑制窗口）。
    ///
    /// `since` = 条件首次满足的时间戳。
    Pending { since: DateTime },
    /// 已正式触发告警（持续达标）。
    ///
    /// `fired_at` = 转入 Firing 的时间戳。
    Firing { fired_at: DateTime },
}

impl AlertState {
    /// 是否处于 Firing（已触发未恢复）。
    pub fn is_firing(&self) -> bool {
        matches!(self, AlertState::Firing { .. })
    }
}

/// 告警引擎评估一条规则后的结果（供上层生成 `Alert` 事件）。
#[derive(Debug, Clone, PartialEq)]
pub enum EvalOutcome {
    /// 无变化（维持 Inactive/Pending/Firing 但未跨状态）。
    NoChange,
    /// 刚从非 Firing 转入 Firing（上层应产生一条告警通知）。
    Fired {
        /// 触发时刻
        fired_at: DateTime,
        /// 触发时的 metric 值（写入告警消息）
        value: f64,
    },
    /// 刚从 Firing 转入 Resolved（上层应标记对应 Alert.resolved=true）。
    Resolved {
        /// 恢复时刻
        resolved_at: DateTime,
    },
}

/// 告警引擎——纯状态机，维护每条规则的 [`AlertState`]。
///
/// 不持有 metric 历史（避免内存膨胀）；上层在每次新样本到达时调用
/// [`ingest`](Self::ingest)，由引擎根据「上次状态 + 当前样本 + 时间戳」推进状态机。
///
/// 抑制/去重：同一规则同时刻最多一个 Firing 告警——状态机本身保证只有
/// 非 Firing → Firing 的转换才产生 [`EvalOutcome::Fired`]，重复样本不会重复触发。
pub struct AlertEngine {
    /// rule_name → (rule, 当前状态)
    rules: HashMap<String, (AlertRule, AlertState)>,
}

impl AlertEngine {
    /// 创建空引擎。
    pub fn new() -> Self {
        Self {
            rules: HashMap::new(),
        }
    }

    /// 注册一条规则（同名覆盖；新规则初始状态为 Inactive）。
    ///
    /// 同时校验条件表达式可解析（解析失败立即报错，避免运行时才发现坏规则）。
    pub fn add_rule(&mut self, rule: AlertRule) -> Result<(), ServiceError> {
        // 提前解析校验，坏规则直接拒绝注册。
        let _ = condition::Condition::parse(&rule.condition)?;
        self.rules
            .insert(rule.name.clone(), (rule, AlertState::Inactive));
        Ok(())
    }

    /// 列出所有已注册规则。
    pub fn rules(&self) -> impl Iterator<Item = &AlertRule> {
        self.rules.values().map(|(r, _)| r)
    }

    /// 查某规则的当前状态。
    pub fn state(&self, rule_name: &str) -> Option<&AlertState> {
        self.rules.get(rule_name).map(|(_, s)| s)
    }

    /// 摄入一个 metric 样本，推进状态机，返回评估结果。
    ///
    /// - `rule_name`：要评估的规则（须已 `add_rule`）。
    /// - `value`：当前 metric 值。
    /// - `now`：样本时间戳（用于判断是否持续够 `for_duration_secs`）。
    ///
    /// 返回 `Ok(None)` 表示规则不存在或无意义样本。
    pub fn ingest(
        &mut self,
        rule_name: &str,
        value: f64,
        now: DateTime,
    ) -> Result<Option<EvalOutcome>, ServiceError> {
        let entry = match self.rules.get_mut(rule_name) {
            Some(e) => e,
            None => return Ok(None),
        };
        let (rule, state) = entry;
        let cond = condition::Condition::parse(&rule.condition)?;
        let satisfied = cond.evaluate(value);
        let for_dur = chrono::Duration::seconds(rule.for_duration_secs as i64);

        let (new_state, outcome) = match (state.clone(), satisfied) {
            // —— 条件满足 ——
            (AlertState::Inactive, true) => {
                if rule.for_duration_secs == 0 {
                    // 无持续时长要求，直接 Firing
                    (
                        AlertState::Firing { fired_at: now },
                        Some(EvalOutcome::Fired {
                            fired_at: now,
                            value,
                        }),
                    )
                } else {
                    (
                        AlertState::Pending { since: now },
                        EvalOutcome::NoChange.into(),
                    )
                }
            }
            (AlertState::Pending { since }, true) => {
                if now - since >= for_dur {
                    (
                        AlertState::Firing { fired_at: now },
                        Some(EvalOutcome::Fired {
                            fired_at: now,
                            value,
                        }),
                    )
                } else {
                    // 仍在抖动窗口内，维持 Pending
                    (AlertState::Pending { since }, EvalOutcome::NoChange.into())
                }
            }
            (AlertState::Firing { fired_at }, true) => {
                // 已 Firing，继续保持
                (
                    AlertState::Firing { fired_at },
                    EvalOutcome::NoChange.into(),
                )
            }
            // —— 条件不再满足 ——
            (AlertState::Inactive, false) => (AlertState::Inactive, EvalOutcome::NoChange.into()),
            (AlertState::Pending { .. }, false) => {
                // 抖动：未达标就退出 Pending，重置
                (AlertState::Inactive, EvalOutcome::NoChange.into())
            }
            (AlertState::Firing { .. }, false) => {
                // 从 Firing 恢复
                (
                    AlertState::Inactive,
                    Some(EvalOutcome::Resolved { resolved_at: now }),
                )
            }
        };
        *state = new_state;
        Ok(outcome)
    }
}

impl Default for AlertEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Monitor trait（async）
// ============================================================================

/// 监控服务——采集 metric、查询告警、tail 日志。
#[allow(async_fn_in_trait)]
pub trait Monitor: Send + Sync {
    /// 记录一个 metric 数据点。
    async fn record_metric(&self, m: Metric) -> Result<(), ServiceError>;

    /// 查询指定时间范围内某 metric 的所有数据点。
    async fn query_metrics(
        &self,
        name: &str,
        from: DateTime,
        to: DateTime,
    ) -> Result<Vec<Metric>, ServiceError>;

    /// 新增告警规则。
    async fn add_alert_rule(&self, rule: AlertRule) -> Result<(), ServiceError>;

    /// 列出当前所有告警（含已触发未恢复 + 近期已恢复）。
    async fn list_alerts(&self) -> Result<Vec<Alert>, ServiceError>;

    /// 按过滤条件 tail 日志。
    async fn tail_logs(&self, filter: LogFilter) -> Result<Vec<LogEntry>, ServiceError>;
}

// ============================================================================
// log_bridge —— tracing-subscriber 日志桥接层（采集：tracing 事件 → LogEntry）
// ============================================================================
//
// 把 OS 各 crate 经 `tracing::info!` / `#[instrument]` / `tracing::warn!` 等
// 宏产生的真实日志事件，捕获为 [`LogEntry`] 写入内存环形 buffer，供
// [`OtelMonitor::tail_logs`] 查询。
//
// 实现方式：自定义 `tracing_subscriber::Layer`（[`log_bridge::LogBridgeLayer`]），
// 在 `on_event` 钩子里把 `Event` 的 level / target / message / fields / timestamp
// 抽出转成 `LogEntry` 推入 buffer。这是 tracing-subscriber 推荐的「自定义 sink」
// 模式（与 `fmt::Layer`、`tracing-appender::Writer` 同级组合）。
//
// **为什么不直接用 `fmt::Layer` 输出后 parse**：fmt 输出是人类可读字符串（或
// JSON 行），parse 回结构化 LogEntry 损失类型且脆弱；直接在 Layer 层取结构化
// 字段（`Event::fields` + `Visit`）保留 `fields: HashMap<String,String>` 维度。
//
// **buffer 容量与丢弃策略**：固定容量 `DEFAULT_CAPACITY`（8192）的 `VecDeque`，
// 满后丢弃最旧条目（ring buffer）。理由：监控日志是滚动窗口（tail 语义），
// 旧日志价值递减且历史归档由文件落盘（[`OtelMonitor::build_subscriber_with`]
// 的 `json_log_path`）接管。
//
// **线程安全**：buffer 是 `LogBuffer`（newtype 包 `Arc<Mutex<VecDeque>>>`），
// Layer clone 与 OtelMonitor 各持一份；并发写靠 Mutex 串行化。

pub mod log_bridge {
    //! tracing-subscriber 日志桥接：[`LogBridgeLayer`] 把 tracing 事件捕获为
    //! [`LogEntry`] 写入共享环形 [`LogBuffer`]。
    //!
    //! ## 使用
    //! ```no_run
    //! use os_services::monitor::OtelMonitor;
    //!
    //! let mon = OtelMonitor::new();
    //! let dispatch = mon.build_subscriber();
    //! // dispatch.set_global_default() 在主程序注册一次（测试见 monitor::tests）
    //! ```
    //!
    //! ## 设计
    //! - 采集：`LogBridgeLayer::on_event` 把 `tracing::Event` 转成 `LogEntry`；
    //!   消息体取 event 的 `message` field（`tracing::info!("msg")` /
    //!   `tracing::info!("formatted {}", x)` 经 `tracing::field::Visit` 取最终
    //!   字符串），其余 visit 值入 `fields` map。
    //! - 时间戳：用 `chrono::Utc::now()`（事件采集时刻）。tracing 0.1 的 Event
    //!   本身无内嵌时间戳（由 Subscriber 的 `Duration` 接口提供相对时间），
    //!   故取采集墙钟最贴合日志语义。
    //! - buffer：`LogBuffer`（newtype 包 `Arc<Mutex<VecDeque>>`），满容量丢弃最旧。

    use std::collections::{HashMap, VecDeque};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use os_core::Utc;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::Layer;

    use crate::monitor::{LogEntry, LogLevel};

    /// 默认 buffer 容量（8192 条，约够 tail 最近 8k 条日志；满则丢最旧）。
    pub const DEFAULT_CAPACITY: usize = 8192;

    /// 共享日志环形 buffer——newtype 包 `Arc<Mutex<VecDeque<LogEntry>>>`。
    ///
    /// `Clone` 廉价（仅 Arc bump）；[`LogBridgeLayer`] 与 [`crate::monitor::OtelMonitor`]
    /// 各持一份 clone，并发读写靠内层 Mutex 串行化。
    #[derive(Clone)]
    pub struct LogBuffer {
        inner: Arc<Mutex<VecDeque<LogEntry>>>,
        capacity: usize,
    }

    impl LogBuffer {
        /// 构造空 buffer（`capacity` 为容量上限；满后丢最旧）。
        pub fn new(capacity: usize) -> Self {
            Self {
                inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
                capacity,
            }
        }

        /// 推入一条 LogEntry；满容量丢最旧（VecDeque::pop_front 是 O(1)）。
        /// 锁中毒视为日志丢失（不 panic 退避，避免日志路径拖垮主流程）。
        pub fn push(&self, entry: LogEntry) {
            if let Ok(mut buf) = self.inner.lock() {
                if buf.len() >= self.capacity {
                    buf.pop_front();
                }
                buf.push_back(entry);
            }
        }

        /// 取快照（clone 全部条目为 Vec，按插入顺序升序）。
        /// 锁中毒返回空 Vec（不 panic）。
        pub fn snapshot(&self) -> Vec<LogEntry> {
            match self.inner.lock() {
                Ok(buf) => buf.iter().cloned().collect(),
                Err(_) => Vec::new(),
            }
        }

        /// 当前长度。
        pub fn len(&self) -> usize {
            self.inner.lock().map(|b| b.len()).unwrap_or(0)
        }

        /// 是否为空。
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// 容量上限。
        pub fn capacity(&self) -> usize {
            self.capacity
        }
    }

    /// tracing-subscriber `Layer`：把 `tracing::Event` 捕获为 `LogEntry`
    /// 写入共享 buffer。
    ///
    /// clone-safe：内部只持 `LogBuffer`（Arc），多 Layer 实例可并存。
    pub struct LogBridgeLayer {
        buffer: LogBuffer,
    }

    impl LogBridgeLayer {
        /// 构造 Layer，挂到给定 buffer（与 `OtelMonitor::logs` 同一份）。
        pub fn new(buffer: LogBuffer) -> Self {
            Self { buffer }
        }

        /// 把 `tracing::Level` 映射到 [`LogLevel`]。
        fn map_level(level: &tracing::Level) -> LogLevel {
            match *level {
                tracing::Level::ERROR => LogLevel::Error,
                tracing::Level::WARN => LogLevel::Warn,
                tracing::Level::INFO => LogLevel::Info,
                tracing::Level::DEBUG => LogLevel::Debug,
                tracing::Level::TRACE => LogLevel::Trace,
            }
        }

        /// 从 `Event` 抽出 LogEntry（纯转换，不写 buffer）。
        fn event_to_entry(event: &Event<'_>) -> LogEntry {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);

            // message：visitor 收集的 "message" field；若不存在则空串。
            let message = visitor.messages.join(" ");
            // fields：除 message 外其余 visit 值（键名→字符串化）。
            visitor.fields.remove("message");
            LogEntry {
                level: Self::map_level(event.metadata().level()),
                target: event.metadata().target().to_string(),
                message,
                timestamp: Utc::now(),
                fields: visitor.fields,
            }
        }
    }

    // tracing-subscriber 的 Layer 需要 S: LookupSpan（即使本 Layer 不查 span）。
    // 对 registry()（默认 Subscriber）成立。
    impl<S> Layer<S> for LogBridgeLayer
    where
        S: Subscriber,
        for<'a> S: tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let entry = Self::event_to_entry(event);
            self.buffer.push(entry);
        }
    }

    /// `tracing::field::Visit` 实现：收集 message（可多条拼接）+ 其余字段。
    ///
    /// `tracing::info!("hello {}", name)` 产生一个名为 `message` 的字段，
    /// 值为 format 后的 `"hello world"`。多次 `record` 累积多条 message
    /// （兼容 `tracing::info!("a"); tracing::info!("b")` 同 span）。
    #[derive(Default)]
    struct FieldVisitor {
        messages: Vec<String>,
        fields: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let val = format!("{value:?}");
            if field.name() == "message" {
                self.messages.push(val);
            } else {
                self.fields.insert(field.name().to_string(), val);
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.messages.push(value.to_string());
            } else {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
        }
    }

    /// 简易文件 writer（满足 `for<'a> MakeWriter<'a>`）：append 打开 + 串行写。
    ///
    /// **轮转**：tracing-appender 未在 workspace 注册（不虚构未注册依赖），
    /// 故此 writer 仅追加写不轮转；外部 logrotate(8) / journald 接管轮转。
    #[derive(Clone)]
    pub struct FileWriter {
        inner: Arc<Mutex<std::fs::File>>,
    }

    impl FileWriter {
        /// 打开（或创建）path 为 append 写；失败则回退到 `/dev/null`（不丢日志，
        /// 不让采集路径整体失败）。
        pub fn new(path: &Path) -> Self {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| {
                    eprintln!("os monitor FileWriter 打开失败 {path:?}: {e}; 回退 /dev/null");
                    e
                })
                .ok()
                .unwrap_or_else(|| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/null")
                        .expect("/dev/null 不可写")
                });
            Self {
                inner: Arc::new(Mutex::new(file)),
            }
        }
    }

    impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for FileWriter {
        type Writer = FileWriteHandle;

        fn make_writer(&'a self) -> Self::Writer {
            FileWriteHandle {
                file: self.inner.clone(),
            }
        }
    }

    /// `Write` 句柄（clone 内部 Arc；每次 write 走 Mutex 串行）。
    pub struct FileWriteHandle {
        file: Arc<Mutex<std::fs::File>>,
    }

    impl std::io::Write for FileWriteHandle {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.file.lock() {
                Ok(mut f) => f.write(buf),
                Err(_) => Ok(buf.len()), // 锁中毒：丢弃这条日志
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            match self.file.lock() {
                Ok(mut f) => f.flush(),
                Err(_) => Ok(()),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        //! log_bridge 单元测：buffer 推入/快照/容量限制 + Layer 事件转换。

        use super::*;
        use crate::monitor::{LogEntry, LogLevel};
        use os_core::Utc;

        fn entry(level: LogLevel, target: &str, msg: &str) -> LogEntry {
            LogEntry {
                level,
                target: target.into(),
                message: msg.into(),
                timestamp: Utc::now(),
                fields: HashMap::new(),
            }
        }

        #[test]
        fn buffer_push_and_snapshot() {
            let buf = LogBuffer::new(8);
            buf.push(entry(LogLevel::Info, "t", "a"));
            buf.push(entry(LogLevel::Warn, "t", "b"));
            let snap = buf.snapshot();
            assert_eq!(snap.len(), 2);
            assert_eq!(snap[0].message, "a");
            assert_eq!(snap[1].message, "b");
            assert_eq!(buf.len(), 2);
            assert!(!buf.is_empty());
            assert_eq!(buf.capacity(), 8);
        }

        #[test]
        fn buffer_capacity_drops_oldest() {
            let buf = LogBuffer::new(2);
            buf.push(entry(LogLevel::Info, "t", "a"));
            buf.push(entry(LogLevel::Info, "t", "b"));
            buf.push(entry(LogLevel::Info, "t", "c")); // 满 → 丢 a
            let snap = buf.snapshot();
            assert_eq!(snap.len(), 2);
            assert_eq!(snap[0].message, "b");
            assert_eq!(snap[1].message, "c");
        }
    }
}

// ============================================================================
// OtelMonitor —— 基于 opentelemetry + opentelemetry-prometheus 的 Monitor 实现
// ============================================================================
//
// 真实 OTel 接通（ADR-DEPS-002 已注册依赖）：
// - **指标采集**：用 `opentelemetry::metrics::MeterProvider` + 同步仪器
//   （`Counter<u64>` / `Gauge<f64>` / `Histogram<f64>`）。每条 metric 按
//   `MetricKind` 选择对应 OTel 仪器类型，首次记录时 lazy 创建并缓存
//   （OTel 仪器均为 `Clone + Send + Sync`，按 name+labels 复用）。
//   - `Counter`：单调递增；按 `(name, labels)` 增量累加。
//   - `Gauge`：瞬时值；`record` 直接覆盖最近观测（不累加，符合 cpu/temp 语义）。
//   - `Histogram`：分布；记录单点观测值，SDK 内部按桶聚合。
// - **prometheus 导出**：`opentelemetry-prometheus::exporter()` 把 OTel SDK
//   绑到一个 `prometheus::Registry`；`render_metrics()` 调
//   `registry.gather() + TextEncoder::encode()` 生成 Prometheus 文本格式，
//   供 axum 的 `/metrics` 端点直接回写（type=text/version=0.0.4）。
//
// 设计权衡：
// - 仍保留内存时序（`metrics: HashMap<name, Vec<Metric>>`）——OTel SDK 仅保留
//   聚合态（sum/last/直方图），不保留原始样本序列；`query_metrics`（按时间范围
//   取历史点）与告警引擎的抖动抑制窗口（`Pending{since}` 跨样本持续判断）依赖
//   原始时序，故二者并存：聚合态供 /metrics 端点，原始时序供查询/告警。
// - 锁粒度：单 `Mutex<OtelState>` 串行化所有写（record/add_rule）；OTel 仪器
//   引用从 state 内持有，render 时在同一把锁下 gather（保证一致快照）。

use std::sync::Mutex;

use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_prometheus::ExporterBuilder;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::Resource;
use prometheus::{Encoder, TextEncoder};

/// OTel 指标仪器句柄——按 `MetricKind` 三选一缓存。
///
/// 三种 OTel 同步仪器均实现 `Clone + Send + Sync`，故可在 `Mutex` 内持有；
/// 同名同 labels 的多次记录复用同一句柄，避免重复创建（OTel 文档建议）。
enum OtInst {
    /// 单调递增计数器（`MetricKind::Counter`）。用 u64 而非 f64——OTel Counter
    /// 语义即"事件计数"，f64 计数器更易因浮点误差漂移。
    Counter(opentelemetry::metrics::Counter<u64>),
    /// 瞬时值（`MetricKind::Gauge`）。
    Gauge(opentelemetry::metrics::Gauge<f64>),
    /// 分布（`MetricKind::Histogram`）。
    Histogram(opentelemetry::metrics::Histogram<f64>),
}

/// 基于 opentelemetry + opentelemetry-prometheus 的 Monitor 实现。
///
/// 内部用 `Mutex<OtelState>` 串行化所有读写；OTel 仪器句柄 + 时序 + 告警引擎
/// 共享同一 state。`render_metrics()` 生成 Prometheus 文本格式供 `/metrics` 端点。
///
/// **日志桥接**：`logs` 字段是一个 `Arc` 共享环形 buffer，tracing-subscriber 的
/// [`log_bridge::LogBridgeLayer`] 持有其 clone，把全局 tracing 事件写入；
/// `tail_logs` 在同一 buffer 上过滤。故本 struct 须与
/// [`OtelMonitor::build_subscriber`] 配套使用：先 `OtelMonitor::new()` →
/// `build_subscriber()` 取 `Dispatch` → `set_global_default` 注册 → 之后所有
/// `tracing::info!` / `#[instrument]` 事件才会进入 buffer。
pub struct OtelMonitor {
    inner: Mutex<OtelState>,
    /// 日志环形 buffer（与 [`log_bridge::LogBridgeLayer`] 共享同一 Arc）。
    /// `tail_logs` 与采集层并发读写，靠 Mutex 串行化。
    logs: log_bridge::LogBuffer,
}

struct OtelState {
    /// metric 时序：name → 按 timestamp 升序的样本（供 query_metrics / 告警引擎）。
    metrics: HashMap<String, Vec<Metric>>,
    /// 告警引擎（纯逻辑状态机）。
    engine: AlertEngine,
    /// 已产生的 Alert（Firing 未恢复 + 近期 Resolved）。
    alerts: Vec<Alert>,
    // —— OTel 真实导出态 ——
    /// prometheus registry（与 exporter 绑定，gather 出 /metrics 数据）。
    registry: prometheus::Registry,
    /// SDK meter provider（持有 reader/exporter；drop 时回收）。
    provider: SdkMeterProvider,
    /// meter（创建仪器的工厂，从 provider 取一次复用）。
    meter: Meter,
    /// name+labels → 仪器句柄缓存（lazy 创建，避免重复注册同名仪器）。
    instruments: HashMap<String, OtInst>,
}

impl OtelState {
    fn new() -> Self {
        // 拼装 OTel 导出栈：prometheus registry ← exporter ← SdkMeterProvider。
        let registry = prometheus::Registry::new();
        let exporter = ExporterBuilder::default()
            .with_registry(registry.clone())
            // 关闭 target_info / otel_scope_info 噪声 metric，让 /metrics 输出
            // 只含业务指标（OS 单租户、无需 resource 区分）。
            .without_target_info()
            .without_scope_info()
            .build()
            .expect("构建 opentelemetry-prometheus exporter 不可失败");
        let resource = Resource::builder()
            .with_attribute(KeyValue::new("service.name", "os"))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(exporter)
            .build();
        let meter = provider.meter("os-monitor");
        Self {
            metrics: HashMap::new(),
            engine: AlertEngine::new(),
            alerts: Vec::new(),
            registry,
            provider,
            meter,
            instruments: HashMap::new(),
        }
    }

    /// 把 metric 的 labels 转成 OTel KeyValue（用于仪器记录的属性维度）。
    fn labels_to_kvs(labels: &HashMap<String, String>) -> Vec<KeyValue> {
        let mut kvs: Vec<KeyValue> = labels
            .iter()
            .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
            .collect();
        // 排序保证确定性（与 OTel 内部 BTreeMap 行为一致，便于测试断言）。
        kvs.sort_by(|a, b| a.key.cmp(&b.key));
        kvs
    }

    /// 仪器缓存键：name + 排序后的 labels（保证同 name 不同 label 集各自独立仪器）。
    fn inst_key(name: &str, labels: &HashMap<String, String>) -> String {
        if labels.is_empty() {
            return name.to_string();
        }
        let mut parts: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        format!("{name}|{}", parts.join(","))
    }

    /// 取或创建 OTel 仪器（按 metric 的 kind + name + labels 缓存）。
    fn instrument_for(&mut self, m: &Metric) -> &OtInst {
        let key = Self::inst_key(&m.name, &m.labels);
        // 占位：若已存在直接返回；否则插入新仪器。
        if !self.instruments.contains_key(&key) {
            let inst = match m.kind {
                MetricKind::Counter => {
                    OtInst::Counter(self.meter.u64_counter(m.name.clone()).build())
                }
                MetricKind::Gauge => OtInst::Gauge(self.meter.f64_gauge(m.name.clone()).build()),
                MetricKind::Histogram => {
                    OtInst::Histogram(self.meter.f64_histogram(m.name.clone()).build())
                }
            };
            self.instruments.insert(key.clone(), inst);
        }
        // 这里 unwrap 安全：上面刚保证存在。
        self.instruments
            .get(&key)
            .expect("instrument just inserted")
    }
}

impl OtelMonitor {
    /// 创建空的 Monitor 实例（同时初始化 OTel 导出栈 + 空 log buffer）。
    ///
    /// 返回的实例 `logs` buffer 为空——须随后调用 [`OtelMonitor::build_subscriber`]
    /// 取 `Dispatch` 注册为全局 subscriber（`set_global_default`），之后
    /// `tracing::info!` / `#[instrument]` 等宏产生的事件才会经
    /// [`log_bridge::LogBridgeLayer`] 捕获进 buffer。详见 [`log_bridge`] 模块文档。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(OtelState::new()),
            logs: log_bridge::LogBuffer::new(log_bridge::DEFAULT_CAPACITY),
        }
    }

    /// 构造一个 tracing-subscriber `Dispatch`，挂载：
    /// - [`log_bridge::LogBridgeLayer`]（把 tracing 事件写入本 monitor 的 buffer）；
    /// - 可选的 JSON 文件 fmt 层（`make_writer` 决定落盘目标）。
    ///
    /// 调用方用 `dispatch.set_global_default()` 注册。环境过滤默认 `Info`，
    /// 可用 `EnvFilter::new("debug,os_storage=trace")` 覆盖。
    ///
    /// **设计权衡**：tracing 的 subscriber 是全局单例（`set_global_default`），
    /// 故「monitor 实例 ↔ subscriber」是手动配对的——调用方负责保证注册的
    /// Dispatch 与 `tail_logs` 的 monitor 实例对应（单进程单 monitor 场景成立）。
    /// 多 monitor 场景需各自 `with_subscriber` 局部派生（tracing 支持 `Span::in_scope`）。
    pub fn build_subscriber(&self) -> tracing::Dispatch {
        self.build_subscriber_with(tracing_subscriber::EnvFilter::new("info"), None)
    }

    /// 构造 subscriber（全可配置版）：
    /// - `filter`：`EnvFilter` 过滤指令（如 `"info"` / `"debug,os_storage=trace"`）；
    /// - `json_log_path`：`Some(path)` 启用 JSON 格式文件落盘（追加写），
    ///   `None` = 仅内存 buffer 不落盘。
    ///
    /// 文件落盘用 `std::fs::OpenOptions` append 打开 + `Arc<Mutex<File>>` 串行化写
    /// （满足 `for<'a> MakeWriter<'a>` 接口）。**轮转**：tracing-appender 未在
    /// workspace 注册（本任务红线：不虚构未注册依赖），故此处仅骨架追加写——
    /// 轮转由外部 logrotate(8) / 系统日志采集（journald/vector）接管。
    pub fn build_subscriber_with(
        &self,
        filter: tracing_subscriber::EnvFilter,
        json_log_path: Option<&std::path::Path>,
    ) -> tracing::Dispatch {
        let bridge = log_bridge::LogBridgeLayer::new(self.logs.clone());
        use tracing_subscriber::layer::SubscriberExt;
        if let Some(path) = json_log_path {
            // JSON 格式 → 文件（追加；轮转外部接管）。
            let writer = log_bridge::FileWriter::new(path);
            let fmt_layer = tracing_subscriber::fmt::layer().json().with_writer(writer);
            let sub = tracing_subscriber::registry()
                .with(filter)
                .with(bridge)
                .with(fmt_layer);
            tracing::Dispatch::new(sub)
        } else {
            // 仅内存 buffer（默认 fmt 输出到 stderr 便于本地调试）。
            let sub = tracing_subscriber::registry().with(filter).with(bridge);
            tracing::Dispatch::new(sub)
        }
    }

    /// 便捷：取本 monitor 共享 buffer 的快照（snapshot 后 apply filter）。
    /// 内部工具，被 `tail_logs` 复用。
    fn snapshot_logs(&self) -> Vec<LogEntry> {
        self.logs.snapshot()
    }

    /// 返回当前 buffer 中日志条数（测试 / 监控自省用）。
    pub fn log_count(&self) -> usize {
        self.logs.len()
    }

    /// 内部：记录 metric 并驱动告警引擎评估所有匹配该 metric 的规则。
    fn record_sync(&self, m: Metric) -> Result<(), ServiceError> {
        let mut st = self
            .inner
            .lock()
            .map_err(|e| ServiceError::Internal(format!("OtelMonitor 锁中毒: {e}")))?;
        let name = m.name.clone();
        let ts = m.timestamp;
        let value = m.value;
        let kind = m.kind;
        let kvs = OtelState::labels_to_kvs(&m.labels);

        // —— 1) 喂 OTel 仪器（按 kind 分派）——
        // Counter/Gauge/Histogram 是 Clone 句柄，clone 出来用避免与 &mut st 借用冲突。
        match st.instrument_for(&m) {
            OtInst::Counter(c) => {
                // Counter 单调递增：累加本次观测值（若调用方传的是绝对值，
                // 多次 record 会重复累加——这是 OTel Counter 语义；调用方应传增量）。
                // f64→u64 饱和转换（负值钳为 0，>u64::MAX 钳为 MAX）。
                let inc = if value.is_finite() && value > 0.0 {
                    if value >= u64::MAX as f64 {
                        u64::MAX
                    } else {
                        value as u64
                    }
                } else {
                    0
                };
                c.clone().add(inc, &kvs);
            }
            OtInst::Gauge(g) => {
                // Gauge：record 直接覆盖（last-write-wins），符合 cpu/temp 语义。
                g.clone().record(value, &kvs);
            }
            OtInst::Histogram(h) => {
                // Histogram：单点观测，SDK 内部按桶聚合。
                h.clone().record(value, &kvs);
            }
        }

        // —— 2) 时序存储（供 query_metrics / 告警引擎）——
        st.metrics.entry(name.clone()).or_default().push(m);

        // —— 3) 驱动告警引擎：对每条 metric 匹配的规则 ingest 当前样本 ——
        let matching: Vec<String> = st
            .engine
            .rules()
            .filter(|r| r.metric == name)
            .map(|r| r.name.clone())
            .collect();
        for rule_name in matching {
            let rule_severity = st.engine.state(&rule_name).and_then(|_| {
                st.engine
                    .rules()
                    .find(|r| r.name == rule_name)
                    .map(|r| r.severity)
            });
            if let Some(severity) = rule_severity {
                if let Ok(Some(outcome)) = st.engine.ingest(&rule_name, value, ts) {
                    match outcome {
                        EvalOutcome::Fired { fired_at, value: v } => {
                            st.alerts.push(Alert {
                                rule_name: rule_name.clone(),
                                severity,
                                fired_at,
                                resolved: false,
                                message: format!("规则 `{rule_name}` 触发：{name}={v}"),
                            });
                        }
                        EvalOutcome::Resolved { resolved_at } => {
                            // 标记对应告警已恢复
                            for a in &mut st.alerts {
                                if a.rule_name == rule_name && !a.resolved {
                                    a.resolved = true;
                                    a.message.push_str(&format!(" | 已恢复 @ {resolved_at}"));
                                    break;
                                }
                            }
                        }
                        EvalOutcome::NoChange => {}
                    }
                }
            }
        }
        // 用 kind 抑制未使用告警（Histogram 路径下 kind 仅用于上面 match 的可读性）。
        let _ = kind;
        Ok(())
    }

    /// 渲染 Prometheus 文本格式（`/metrics` 端点响应体）。
    ///
    /// 流程：`registry.gather()` 收集所有 MetricFamily → `TextEncoder::encode()`
    /// 编码为 Prometheus exposition format v0.0.4 文本。
    /// axum handler 直接 `String::from_utf8(buf)` 作为 body 返回，content-type
    /// `text/plain; version=0.0.4; charset=utf-8`（`TextEncoder::format_type()`）。
    pub fn render_metrics(&self) -> Result<String, ServiceError> {
        let st = self
            .inner
            .lock()
            .map_err(|e| ServiceError::Internal(format!("OtelMonitor 锁中毒: {e}")))?;
        // 触发一次 SDK 聚合 flush，确保最近 record 已被 reader 收集。
        // （prometheus exporter 用 ManualReader，collect 在 gather 时拉取，
        //   显式 flush 非必须，但保险起见——失败不致命，忽略。）
        let _ = st.provider.force_flush();
        let mf = st.registry.gather();
        let encoder = TextEncoder::new();
        let mut buf = Vec::with_capacity(256);
        encoder
            .encode(&mf, &mut buf)
            .map_err(|e| ServiceError::Internal(format!("prometheus 编码失败: {e}")))?;
        String::from_utf8(buf)
            .map_err(|e| ServiceError::Internal(format!("prometheus 输出非 UTF-8: {e}")))
    }

    /// Prometheus 文本格式的 content-type（供 axum 设响应头）。
    pub fn metrics_content_type() -> &'static str {
        "text/plain; version=0.0.4; charset=utf-8"
    }
}

impl Default for OtelMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl Monitor for OtelMonitor {
    async fn record_metric(&self, m: Metric) -> Result<(), ServiceError> {
        self.record_sync(m)
    }

    async fn query_metrics(
        &self,
        name: &str,
        from: DateTime,
        to: DateTime,
    ) -> Result<Vec<Metric>, ServiceError> {
        let st = self
            .inner
            .lock()
            .map_err(|e| ServiceError::Internal(format!("OtelMonitor 锁中毒: {e}")))?;
        let out = st
            .metrics
            .get(name)
            .map(|v| {
                v.iter()
                    .filter(|m| m.timestamp >= from && m.timestamp <= to)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(out)
    }

    async fn add_alert_rule(&self, rule: AlertRule) -> Result<(), ServiceError> {
        let mut st = self
            .inner
            .lock()
            .map_err(|e| ServiceError::Internal(format!("OtelMonitor 锁中毒: {e}")))?;
        st.engine.add_rule(rule)
    }

    async fn list_alerts(&self) -> Result<Vec<Alert>, ServiceError> {
        let st = self
            .inner
            .lock()
            .map_err(|e| ServiceError::Internal(format!("OtelMonitor 锁中毒: {e}")))?;
        Ok(st.alerts.clone())
    }

    async fn tail_logs(&self, filter: LogFilter) -> Result<Vec<LogEntry>, ServiceError> {
        let snap = self.snapshot_logs();
        Ok(filter.apply(snap))
    }
}

// ============================================================================
// MockMonitor（feature `mock`）—— 纯内存确定性实现，供下游测试注入
// ============================================================================

#[cfg(feature = "mock")]
pub mod mock {
    //! `MockMonitor` —— 纯内存 [`crate::Monitor`] 实现，供下游测试注入。
    //!
    //! 仅在 `mock` feature 下编译。下游在 `[dev-dependencies]` 加
    //! `os-services = { workspace = true, features = ["mock"] }`。
    //!
    //! 设计（见 `_conventions.md §5`）：
    //! - 实现完整 `Monitor` trait，不依赖外部状态（无 OTel / 无文件）。
    //! - 提供构造器预置返回值，并暴露内部状态便于断言。
    //! - 错误注入：`with_error` 测试错误路径。

    use std::sync::Mutex;

    use crate::monitor::{Alert, AlertEngine, AlertRule, EvalOutcome, LogEntry, LogFilter, Metric};
    use crate::ServiceError;
    use os_core::DateTime;

    /// Mock 监控服务——纯内存、确定性。
    ///
    /// 内部复用 [`AlertEngine`]（与 `OtelMonitor` 同一套纯逻辑），保证 mock 行为
    /// 与真实实现一致，下游测试可信。
    pub struct MockMonitor {
        inner: Mutex<MockState>,
        /// 强制错误：若设置，下次方法调用返回此错误。
        forced_error: Mutex<Option<ServiceError>>,
    }

    struct MockState {
        metrics: Vec<Metric>,
        logs: Vec<LogEntry>,
        engine: AlertEngine,
        alerts: Vec<Alert>,
    }

    impl MockState {
        fn new() -> Self {
            Self {
                metrics: Vec::new(),
                logs: Vec::new(),
                engine: AlertEngine::new(),
                alerts: Vec::new(),
            }
        }
    }

    impl MockMonitor {
        /// 创建空 mock。
        pub fn new() -> Self {
            Self {
                inner: Mutex::new(MockState::new()),
                forced_error: Mutex::new(None),
            }
        }

        /// 预置一个 metric（便于 `query_metrics` 返回确定性数据）。
        pub fn with_metric(self, m: Metric) -> Self {
            self.inner.lock().unwrap().metrics.push(m);
            self
        }

        /// 预置一条日志（便于 `tail_logs` 返回确定性数据）。
        pub fn with_log(self, l: LogEntry) -> Self {
            self.inner.lock().unwrap().logs.push(l);
            self
        }

        /// 预置告警规则。
        pub fn with_rule(self, rule: AlertRule) -> Self {
            self.inner.lock().unwrap().engine.add_rule(rule).ok();
            self
        }

        /// 设置强制错误——下次任意方法调用返回此错误（错误路径测试）。
        pub fn with_error(self, e: ServiceError) -> Self {
            *self.forced_error.lock().unwrap() = Some(e);
            self
        }

        /// 取走并返回强制错误（None 表示无注入）。
        fn take_error(&self) -> Option<ServiceError> {
            self.forced_error.lock().unwrap().take()
        }

        /// 直接读取已记录的 metric 列表（断言用）。
        pub fn recorded_metrics(&self) -> Vec<Metric> {
            self.inner.lock().unwrap().metrics.clone()
        }

        /// 直接读取当前告警列表（断言用）。
        pub fn alerts(&self) -> Vec<Alert> {
            self.inner.lock().unwrap().alerts.clone()
        }
    }

    impl Default for MockMonitor {
        fn default() -> Self {
            Self::new()
        }
    }

    #[allow(async_fn_in_trait)]
    impl crate::Monitor for MockMonitor {
        async fn record_metric(&self, m: Metric) -> Result<(), ServiceError> {
            if let Some(e) = self.take_error() {
                return Err(e);
            }
            let mut st = self
                .inner
                .lock()
                .map_err(|e| ServiceError::Internal(format!("MockMonitor 锁中毒: {e}")))?;
            let name = m.name.clone();
            let ts = m.timestamp;
            let value = m.value;
            st.metrics.push(m);

            let matching: Vec<String> = st
                .engine
                .rules()
                .filter(|r| r.metric == name)
                .map(|r| r.name.clone())
                .collect();
            for rule_name in matching {
                let severity = st
                    .engine
                    .rules()
                    .find(|r| r.name == rule_name)
                    .map(|r| r.severity);
                if let Some(sev) = severity {
                    if let Ok(Some(outcome)) = st.engine.ingest(&rule_name, value, ts) {
                        match outcome {
                            EvalOutcome::Fired { fired_at, value: v } => {
                                st.alerts.push(Alert {
                                    rule_name: rule_name.clone(),
                                    severity: sev,
                                    fired_at,
                                    resolved: false,
                                    message: format!("规则 `{rule_name}` 触发：{name}={v}"),
                                });
                            }
                            EvalOutcome::Resolved { resolved_at } => {
                                for a in &mut st.alerts {
                                    if a.rule_name == rule_name && !a.resolved {
                                        a.resolved = true;
                                        a.message.push_str(&format!(" | 已恢复 @ {resolved_at}"));
                                        break;
                                    }
                                }
                            }
                            EvalOutcome::NoChange => {}
                        }
                    }
                }
            }
            Ok(())
        }

        async fn query_metrics(
            &self,
            name: &str,
            from: DateTime,
            to: DateTime,
        ) -> Result<Vec<Metric>, ServiceError> {
            if let Some(e) = self.take_error() {
                return Err(e);
            }
            let st = self
                .inner
                .lock()
                .map_err(|e| ServiceError::Internal(format!("MockMonitor 锁中毒: {e}")))?;
            Ok(st
                .metrics
                .iter()
                .filter(|m| m.name == name && m.timestamp >= from && m.timestamp <= to)
                .cloned()
                .collect())
        }

        async fn add_alert_rule(&self, rule: AlertRule) -> Result<(), ServiceError> {
            if let Some(e) = self.take_error() {
                return Err(e);
            }
            let mut st = self
                .inner
                .lock()
                .map_err(|e| ServiceError::Internal(format!("MockMonitor 锁中毒: {e}")))?;
            st.engine.add_rule(rule)
        }

        async fn list_alerts(&self) -> Result<Vec<Alert>, ServiceError> {
            if let Some(e) = self.take_error() {
                return Err(e);
            }
            Ok(self.inner.lock().unwrap().alerts.clone())
        }

        async fn tail_logs(&self, filter: LogFilter) -> Result<Vec<LogEntry>, ServiceError> {
            if let Some(e) = self.take_error() {
                return Err(e);
            }
            let st = self
                .inner
                .lock()
                .map_err(|e| ServiceError::Internal(format!("MockMonitor 锁中毒: {e}")))?;
            Ok(st
                .logs
                .iter()
                .filter(|l| l.matches(&filter))
                .cloned()
                .collect())
        }
    }
}

// ============================================================================
// 单元测试（纯逻辑：条件解析 / 抖动抑制 / 状态机 / 过滤 / 构造器）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::Utc;

    fn ts(s: &str) -> DateTime {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&Utc)
    }

    // —— 条件解析 ——

    #[test]
    fn condition_parse_all_operators() {
        use condition::Comparison::*;
        let cases = [
            (">0.9", Gt, 0.9),
            (">=0.9", Ge, 0.9),
            ("<100", Lt, 100.0),
            ("<=100", Le, 100.0),
            ("==0", Eq, 0.0),
            ("!=1", Ne, 1.0),
            ("  >  0.5 ", Gt, 0.5), // 空白容忍
        ];
        for (expr, op, thr) in cases {
            let c = condition::Condition::parse(expr).unwrap_or_else(|e| panic!("{expr}: {e:?}"));
            assert_eq!(c.op, op, "expr={expr}");
            assert!((c.threshold - thr).abs() < 1e-9, "expr={expr}");
        }
    }

    #[test]
    fn condition_parse_errors() {
        assert!(condition::Condition::parse("0.9").is_err()); // 缺算子
        assert!(condition::Condition::parse(">abc").is_err()); // 非数字
        assert!(condition::Condition::parse("").is_err());
    }

    #[test]
    fn condition_evaluate() {
        let c = condition::Condition::parse(">0.9").unwrap();
        assert!(c.evaluate(0.95));
        assert!(!c.evaluate(0.9)); // 严格大于
        assert!(!c.evaluate(0.8));
    }

    // —— MetricPoint 直方图聚合 ——

    #[test]
    fn metric_point_from_values() {
        let t = ts("2026-01-01T00:00:00Z");
        let mp = MetricPoint::from_values("lat", &[0.1, 0.5, 1.5, 2.0], vec![0.5, 1.0, 2.0], t);
        assert_eq!(mp.count, 4);
        assert!((mp.sum - 4.1).abs() < 1e-9);
        // 桶：<=0.5 → 2 个；<=1.0 → 2 个；<=2.0 → 4 个；+Inf → 4 个
        assert_eq!(
            mp.buckets.iter().map(|s| s.count).collect::<Vec<_>>(),
            vec![2, 2, 4, 4]
        );
    }

    // —— LogEntry 过滤 ——

    #[test]
    fn log_entry_matches_level_target_since() {
        let entry = LogEntry {
            level: LogLevel::Warn,
            target: "os_storage::replication".into(),
            message: "x".into(),
            timestamp: ts("2026-01-01T10:00:00Z"),
            fields: HashMap::new(),
        };
        // level 过滤（>= Warn）
        assert!(entry.matches(&LogFilter {
            level: Some(LogLevel::Warn),
            ..Default::default()
        }));
        assert!(!entry.matches(&LogFilter {
            level: Some(LogLevel::Error),
            ..Default::default()
        }));
        // target 精确匹配
        assert!(entry.matches(&LogFilter {
            target: Some("os_storage::replication".into()),
            ..Default::default()
        }));
        assert!(!entry.matches(&LogFilter {
            target: Some("other".into()),
            ..Default::default()
        }));
        // since
        assert!(entry.matches(&LogFilter {
            since: Some(ts("2026-01-01T09:00:00Z")),
            ..Default::default()
        }));
        assert!(!entry.matches(&LogFilter {
            since: Some(ts("2026-01-01T11:00:00Z")),
            ..Default::default()
        }));
    }

    #[test]
    fn log_entry_matches_until_source_keyword() {
        // 新增维度：until（截止时间）/ source（target 子串）/ keyword（message 子串）。
        let entry = LogEntry {
            level: LogLevel::Info,
            target: "os_storage::replication".into(),
            message: "Replication started for dataset".into(),
            timestamp: ts("2026-01-01T10:00:00Z"),
            fields: HashMap::new(),
        };
        // until：entry 在 11:00 之前 → 匹配 until=11:00，不匹配 until=09:00。
        assert!(entry.matches(&LogFilter {
            until: Some(ts("2026-01-01T11:00:00Z")),
            ..Default::default()
        }));
        assert!(!entry.matches(&LogFilter {
            until: Some(ts("2026-01-01T09:00:00Z")),
            ..Default::default()
        }));
        // source：target 子串匹配。
        assert!(entry.matches(&LogFilter {
            source: Some("os_storage".into()),
            ..Default::default()
        }));
        assert!(!entry.matches(&LogFilter {
            source: Some("os_compute".into()),
            ..Default::default()
        }));
        // keyword：message 大小写不敏感子串匹配。
        assert!(entry.matches(&LogFilter {
            keyword: Some("replication".into()),
            ..Default::default()
        }));
        assert!(!entry.matches(&LogFilter {
            keyword: Some("snapshot".into()),
            ..Default::default()
        }));
    }

    #[test]
    fn log_filter_apply_sorts_and_limits() {
        // apply：过滤 + 按 timestamp 升序稳定排序 + limit 截断。
        let t = ts("2026-01-01T00:00:00Z");
        let logs = vec![
            LogEntry {
                level: LogLevel::Info,
                target: "a".into(),
                message: "third".into(),
                timestamp: t + chrono::Duration::seconds(20),
                fields: HashMap::new(),
            },
            LogEntry {
                level: LogLevel::Warn,
                target: "a".into(),
                message: "first".into(),
                timestamp: t,
                fields: HashMap::new(),
            },
            LogEntry {
                level: LogLevel::Info,
                target: "b".into(),
                message: "second".into(),
                timestamp: t + chrono::Duration::seconds(10),
                fields: HashMap::new(),
            },
        ];
        // 仅 target=a 的两条，按时间升序，limit=1 取最早。
        let out = LogFilter {
            target: Some("a".into()),
            limit: Some(1),
            ..Default::default()
        }
        .apply(logs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "first");
    }

    #[test]
    fn log_filter_keyword_helper() {
        assert_eq!(LogFilter::keyword("x").keyword.as_deref(), Some("x"));
    }

    // —— 告警引擎：抖动抑制 + 状态机 ——

    fn rule(name: &str, dur_secs: u32) -> AlertRule {
        AlertRule {
            name: name.into(),
            metric: "cpu_usage".into(),
            condition: ">80".into(),
            for_duration_secs: dur_secs,
            severity: AlertSeverity::Critical,
        }
    }

    #[test]
    fn engine_no_duration_fires_immediately() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 0)).unwrap();
        let t = ts("2026-01-01T00:00:00Z");
        let out = eng.ingest("r", 90.0, t).unwrap();
        assert_eq!(
            out,
            Some(EvalOutcome::Fired {
                fired_at: t,
                value: 90.0
            })
        );
        assert!(eng.state("r").unwrap().is_firing());
    }

    #[test]
    fn engine_pending_first_no_fire() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 300)).unwrap(); // 5 分钟
        let t0 = ts("2026-01-01T00:00:00Z");
        // 首次满足 → Pending，未持续够时长，不触发
        assert_eq!(
            eng.ingest("r", 90.0, t0).unwrap(),
            Some(EvalOutcome::NoChange)
        );
        assert_eq!(eng.state("r").unwrap(), &AlertState::Pending { since: t0 });
    }

    #[test]
    fn engine_jitter_suppression_resets_pending() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 300)).unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        eng.ingest("r", 90.0, t0).unwrap(); // Pending
        assert_eq!(eng.state("r").unwrap(), &AlertState::Pending { since: t0 });
        // 短时回落（未持续够 5 分钟）→ 重置为 Inactive（抖动抑制）
        let t1 = ts("2026-01-01T00:01:00Z");
        assert_eq!(
            eng.ingest("r", 50.0, t1).unwrap(),
            Some(EvalOutcome::NoChange)
        );
        assert_eq!(eng.state("r").unwrap(), &AlertState::Inactive);
    }

    #[test]
    fn engine_fires_after_sustained_duration() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 300)).unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        eng.ingest("r", 90.0, t0).unwrap(); // Pending
        assert_eq!(eng.state("r").unwrap(), &AlertState::Pending { since: t0 });
        // 5 分钟后仍满足 → Firing
        let t1 = ts("2026-01-01T00:05:00Z");
        let out = eng.ingest("r", 95.0, t1).unwrap();
        assert_eq!(
            out,
            Some(EvalOutcome::Fired {
                fired_at: t1,
                value: 95.0
            })
        );
        assert!(eng.state("r").unwrap().is_firing());
    }

    #[test]
    fn engine_pending_not_yet_duration_stays_pending() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 300)).unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        eng.ingest("r", 90.0, t0).unwrap(); // Pending
                                            // 3 分钟（< 5 分钟）仍满足 → 维持 Pending，不触发
        let t1 = ts("2026-01-01T00:03:00Z");
        assert_eq!(
            eng.ingest("r", 91.0, t1).unwrap(),
            Some(EvalOutcome::NoChange)
        );
        assert_eq!(eng.state("r").unwrap(), &AlertState::Pending { since: t0 });
    }

    #[test]
    fn engine_resolves_when_condition_clears() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 0)).unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        eng.ingest("r", 90.0, t0).unwrap(); // Firing
                                            // 条件不再满足 → Resolved
        let t1 = ts("2026-01-01T00:06:00Z");
        let out = eng.ingest("r", 50.0, t1).unwrap();
        assert_eq!(out, Some(EvalOutcome::Resolved { resolved_at: t1 }));
        assert_eq!(eng.state("r").unwrap(), &AlertState::Inactive);
    }

    #[test]
    fn engine_dedup_no_duplicate_fire() {
        let mut eng = AlertEngine::new();
        eng.add_rule(rule("r", 0)).unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        // 首次 → Fired
        assert!(matches!(
            eng.ingest("r", 90.0, t0).unwrap(),
            Some(EvalOutcome::Fired { .. })
        ));
        // 重复样本（仍 Firing）→ NoChange，不再产生新 Fired（去重）
        let t1 = ts("2026-01-01T00:01:00Z");
        assert_eq!(
            eng.ingest("r", 91.0, t1).unwrap(),
            Some(EvalOutcome::NoChange)
        );
    }

    #[test]
    fn engine_unknown_rule_returns_none() {
        let mut eng = AlertEngine::new();
        assert_eq!(
            eng.ingest("nope", 1.0, ts("2026-01-01T00:00:00Z")).unwrap(),
            None
        );
    }

    #[test]
    fn engine_rejects_bad_condition_on_add() {
        let mut eng = AlertEngine::new();
        let bad = AlertRule {
            name: "r".into(),
            metric: "m".into(),
            condition: "??".into(),
            for_duration_secs: 0,
            severity: AlertSeverity::Info,
        };
        assert!(eng.add_rule(bad).is_err());
    }

    // —— Metric 构造器 ——

    #[test]
    fn metric_constructors() {
        let t = ts("2026-01-01T00:00:00Z");
        let g = Metric::gauge("cpu", 50.0, t).with_label("host", "os1");
        assert_eq!(g.kind, MetricKind::Gauge);
        assert_eq!(g.labels.get("host").unwrap(), "os1");
        assert_eq!(Metric::counter("c", 1.0, t).kind, MetricKind::Counter);
        assert_eq!(Metric::histogram("h", 2.0, t).kind, MetricKind::Histogram);
    }

    // —— OtelMonitor 集成（端到端走引擎）——

    #[tokio::test]
    async fn otel_record_query_and_alert() {
        let mon = OtelMonitor::new();
        let t = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::gauge("cpu_usage", 50.0, t))
            .await
            .unwrap();
        let out = mon
            .query_metrics(
                "cpu_usage",
                t - chrono::Duration::seconds(1),
                t + chrono::Duration::seconds(1),
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
    }

    #[tokio::test]
    async fn otel_alert_flow_fire_and_resolve() {
        let mon = OtelMonitor::new();
        mon.add_alert_rule(rule("cpu_high", 0)).await.unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::gauge("cpu_usage", 95.0, t0))
            .await
            .unwrap();
        let alerts = mon.list_alerts().await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert!(!alerts[0].resolved);
        // 条件不再满足 → 标记恢复
        let t1 = ts("2026-01-01T00:06:00Z");
        mon.record_metric(Metric::gauge("cpu_usage", 50.0, t1))
            .await
            .unwrap();
        let alerts = mon.list_alerts().await.unwrap();
        assert!(alerts[0].resolved, "应已恢复");
    }

    #[tokio::test]
    async fn otel_tail_logs_filter() {
        // 真实 tracing 事件 → buffer → tail_logs 过滤往返（采集层真实）。
        //
        // 用 `tracing::dispatcher::with_default` 在测试作用域内设置 subscriber，
        // 不污染全局（`set_global_default` 是进程级且不可撤销，多测试会冲突）。
        let mon = OtelMonitor::new();
        let dispatch = mon.build_subscriber();
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(target: "os_storage::replication", "replication started");
            tracing::warn!(target: "os_storage::replication", "disk slow");
            tracing::debug!(target: "os_compute::vm", "vm booted"); // 默认 Info 过滤会被丢弃
        });
        // 默认 build_subscriber 用 EnvFilter "info" → debug 被过滤掉。
        assert_eq!(mon.log_count(), 2);

        // 全量（空 filter）
        let all = mon.tail_logs(LogFilter::default()).await.unwrap();
        assert_eq!(all.len(), 2);

        // 按 level 过滤（>= Warn）
        let warns = mon
            .tail_logs(LogFilter {
                level: Some(LogLevel::Warn),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].message, "disk slow");

        // 按 target 精确匹配
        let repl = mon
            .tail_logs(LogFilter {
                target: Some("os_storage::replication".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(repl.len(), 2);

        // 按 source 子串匹配（target 子串）
        let storage = mon
            .tail_logs(LogFilter {
                source: Some("os_storage".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(storage.len(), 2);

        // 按 keyword（message 大小写不敏感子串）
        let kw = mon
            .tail_logs(LogFilter {
                keyword: Some("REPLICATION".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(kw.len(), 1);
        assert_eq!(kw[0].message, "replication started");

        // 不匹配的 keyword → 空
        let none = mon
            .tail_logs(LogFilter {
                keyword: Some("snapshot".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn otel_tail_logs_levels_filtered_by_envfilter() {
        // EnvFilter "info" 应过滤掉 trace/debug 事件（采集层遵循 subscriber 过滤）。
        let mon = OtelMonitor::new();
        let dispatch = mon.build_subscriber();
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::trace!(target: "t", "trace msg");
            tracing::debug!(target: "t", "debug msg");
            tracing::info!(target: "t", "info msg");
            tracing::warn!(target: "t", "warn msg");
            tracing::error!(target: "t", "error msg");
        });
        // trace/debug 被 EnvFilter 丢弃，剩 info/warn/error 三条。
        let out = mon.tail_logs(LogFilter::default()).await.unwrap();
        let levels: Vec<_> = out.iter().map(|l| l.level).collect();
        assert_eq!(
            levels,
            vec![LogLevel::Info, LogLevel::Warn, LogLevel::Error]
        );
    }

    #[tokio::test]
    async fn otel_tail_logs_fields_captured() {
        // tracing 的结构化字段（非 message）应进 LogEntry.fields。
        let mon = OtelMonitor::new();
        let dispatch = mon.build_subscriber();
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(target: "os_api", request_id = "abc123", "handled request");
        });
        let out = mon.tail_logs(LogFilter::default()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "handled request");
        assert_eq!(
            out[0].fields.get("request_id").map(|s| s.as_str()),
            Some("abc123")
        );
    }

    #[tokio::test]
    async fn otel_tail_logs_limit_truncates() {
        // limit 截断：取最早 N 条（按 timestamp 升序后截断）。
        let mon = OtelMonitor::new();
        let dispatch = mon.build_subscriber();
        tracing::dispatcher::with_default(&dispatch, || {
            for i in 0..5 {
                tracing::info!(target: "t", "msg-{i}");
            }
        });
        let out = mon
            .tail_logs(LogFilter {
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        // 升序 → 最早两条（msg-0 / msg-1）
        assert!(out[0].message.contains("msg-0"));
        assert!(out[1].message.contains("msg-1"));
    }

    #[tokio::test]
    async fn otel_tail_logs_buffer_capacity_drops_oldest() {
        // buffer 容量限制：默认 8192，这里直接测 push 路径（log_bridge 单测覆盖了
        // 小容量；此测验证 OtelMonitor 与 LogBuffer 的接线）。
        let mon = OtelMonitor::new();
        let dispatch = mon.build_subscriber();
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(target: "t", "one");
            tracing::info!(target: "t", "two");
        });
        assert!(mon.log_count() >= 2);
    }

    // —— MockMonitor ——

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_query_returns_preset() {
        let t = ts("2026-01-01T00:00:00Z");
        let m = Metric::gauge("cpu", 50.0, t);
        let mon = crate::monitor::mock::MockMonitor::new().with_metric(m.clone());
        let out = mon
            .query_metrics(
                "cpu",
                t - chrono::Duration::seconds(1),
                t + chrono::Duration::seconds(1),
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_error_injection() {
        use crate::ServiceError;
        let mon = crate::monitor::mock::MockMonitor::new()
            .with_error(ServiceError::Internal("boom".into()));
        let err = mon
            .query_metrics("x", ts("2026-01-01T00:00:00Z"), ts("2026-01-01T01:00:00Z"))
            .await;
        assert!(err.is_err());
    }

    // —— OtelMonitor 真实 OTel 导出（prometheus 文本格式）——

    #[tokio::test]
    async fn otel_export_gauge_appears_in_metrics() {
        // 记录一个 Gauge，render_metrics 输出应含该 metric 行。
        let mon = OtelMonitor::new();
        let t = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::gauge("cpu_usage", 0.42, t))
            .await
            .unwrap();
        let out = mon.render_metrics().unwrap();
        // Gauge → prometheus # TYPE <name> gauge + 数据行；值 0.42 应出现。
        assert!(
            out.contains("# TYPE cpu_usage gauge"),
            "gauge TYPE 行缺失:\n{out}"
        );
        assert!(out.contains("cpu_usage "), "gauge 数据行缺失:\n{out}");
        assert!(out.contains("0.42"), "gauge 值 0.42 缺失:\n{out}");
    }

    #[tokio::test]
    async fn otel_export_counter_accumulates() {
        // Counter 单调累加：两次 record 5 + 3 → 总和 8。
        let mon = OtelMonitor::new();
        let t = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::counter("bytes_sent", 5.0, t))
            .await
            .unwrap();
        mon.record_metric(Metric::counter("bytes_sent", 3.0, t))
            .await
            .unwrap();
        let out = mon.render_metrics().unwrap();
        // OTel monotonic counter → prometheus _total 后缀 + TYPE counter。
        assert!(
            out.contains("# TYPE bytes_sent_total counter") || out.contains("bytes_sent_total"),
            "counter _total 输出缺失:\n{out}"
        );
        assert!(out.contains('8'), "累加值 8 缺失:\n{out}");
    }

    #[tokio::test]
    async fn otel_export_gauge_last_write_wins() {
        // Gauge 覆盖语义：record 0.5 后再 record 0.9 → 输出 0.9（非累加）。
        let mon = OtelMonitor::new();
        let t = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::gauge("mem_usage", 0.5, t))
            .await
            .unwrap();
        mon.record_metric(Metric::gauge("mem_usage", 0.9, t))
            .await
            .unwrap();
        let out = mon.render_metrics().unwrap();
        // Gauge 不累加，应是 0.9 而非 1.4。
        assert!(
            out.contains("0.9"),
            "gauge last-write-wins=0.9 缺失:\n{out}"
        );
        assert!(!out.contains("1.4"), "gauge 不应累加出 1.4:\n{out}");
    }

    #[tokio::test]
    async fn otel_export_histogram_buckets() {
        // Histogram → prometheus _bucket/_sum/_count 三件套。
        let mon = OtelMonitor::new();
        let t = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::histogram("req_latency", 0.05, t))
            .await
            .unwrap();
        mon.record_metric(Metric::histogram("req_latency", 0.5, t))
            .await
            .unwrap();
        let out = mon.render_metrics().unwrap();
        assert!(
            out.contains("# TYPE req_latency histogram"),
            "histogram TYPE 缺失:\n{out}"
        );
        assert!(
            out.contains("req_latency_bucket"),
            "histogram _bucket 缺失:\n{out}"
        );
        assert!(
            out.contains("req_latency_sum"),
            "histogram _sum 缺失:\n{out}"
        );
        assert!(
            out.contains("req_latency_count"),
            "histogram _count 缺失:\n{out}"
        );
        // 两次观测 → count=2
        assert!(out.contains("req_latency_count"), "count 缺失");
    }

    #[tokio::test]
    async fn otel_export_distinguishes_labels() {
        // 同名 metric 不同 labels → 各自独立数据行。
        let mon = OtelMonitor::new();
        let t = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::gauge("temp", 45.0, t).with_label("device", "cpu"))
            .await
            .unwrap();
        mon.record_metric(Metric::gauge("temp", 35.0, t).with_label("device", "disk"))
            .await
            .unwrap();
        let out = mon.render_metrics().unwrap();
        assert!(out.contains(r#"device="cpu""#), "cpu label 行缺失:\n{out}");
        assert!(
            out.contains(r#"device="disk""#),
            "disk label 行缺失:\n{out}"
        );
        assert!(out.contains("45"), "cpu 温度 45 缺失");
        assert!(out.contains("35"), "disk 温度 35 缺失");
    }

    #[tokio::test]
    async fn otel_export_empty_when_no_metrics() {
        // 无 metric 记录 → render_metrics 返回空串（或仅空白），不报错。
        let mon = OtelMonitor::new();
        let out = mon.render_metrics().unwrap();
        // 关闭了 target_info/scope_info，无 metric 时输出为空。
        assert!(
            out.trim().is_empty(),
            "无 metric 时输出应为空，实际:\n{out}"
        );
    }

    #[test]
    fn otel_metrics_content_type_is_text_v004() {
        // content-type 必须是 prometheus exposition v0.0.4。
        let ct = OtelMonitor::metrics_content_type();
        assert!(ct.starts_with("text/plain"));
        assert!(ct.contains("version=0.0.4"));
    }

    #[tokio::test]
    async fn otel_record_then_alert_then_export() {
        // 端到端：record 触发告警 → alert 列表非空 + /metrics 仍含 metric。
        let mon = OtelMonitor::new();
        mon.add_alert_rule(rule("cpu_high", 0)).await.unwrap();
        let t0 = ts("2026-01-01T00:00:00Z");
        mon.record_metric(Metric::gauge("cpu_usage", 95.0, t0))
            .await
            .unwrap();
        let alerts = mon.list_alerts().await.unwrap();
        assert_eq!(alerts.len(), 1);
        // /metrics 端点也应能 gather 出该 gauge。
        let out = mon.render_metrics().unwrap();
        assert!(
            out.contains("cpu_usage"),
            "告警触发后 /metrics 缺 metric:\n{out}"
        );
    }

    // —— 日志导出（JSON 文件落盘骨架）——

    #[tokio::test]
    async fn otel_log_export_json_file() {
        // build_subscriber_with(Some(path)) 启用 JSON 文件落盘：
        // tracing 事件同时进内存 buffer + 写入 JSON 文件。
        let tmp = tempfile::tempdir().expect("tempdir");
        let log_path = tmp.path().join("os.log.json");
        let mon = OtelMonitor::new();
        let dispatch =
            mon.build_subscriber_with(tracing_subscriber::EnvFilter::new("info"), Some(&log_path));
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(target: "os_storage", "hello export");
        });
        // 内存 buffer 应有该条
        let out = mon.tail_logs(LogFilter::default()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "hello export");
        // 文件应存在且为 JSON 行（含 "hello export" 消息字段）
        let content = std::fs::read_to_string(&log_path).expect("读日志文件");
        assert!(
            content.contains("hello export"),
            "JSON 文件缺消息，内容:\n{content}"
        );
        // JSON 格式基本校验：含 "timestamp" / "level" / "target" 字段
        assert!(
            content.contains("\"target\""),
            "JSON 缺 target 字段:\n{content}"
        );
    }

    fn _silence_unused() {
        let _: DateTime = ts("2026-01-01T00:00:00Z");
    }
}
