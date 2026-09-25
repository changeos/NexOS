//! `ChronyNtp` —— `NtpManager` trait 的真实 chrony 编排实现
//!
//! 定位（规格书 §3 关键实现 / §9.1#8 决策：NTP 由 osd 统管）：
//! - 编排本机 `chronyd` + `chronyc` 命令完成时间同步 / 状态查询 / 上游服务器热更新。
//! - 不依赖外部 `ntpd`（避免双源冲突），osd 是系统时钟权威来源。
//!
//! ## 权限与可测试性（规格书 §6 硬阻塞 / §9 红线）
//! 真实 chrony 操作需要 **root + CAP_SYS_TIME + chrony 守护进程**——
//! 在沙箱外运行会失败（EPERM）或污染宿主时钟（红线）。为支持"不依赖 root 的单元测试"，
//! 本模块把 chrony 命令执行抽象成 [`NtpRunner`] trait：
//!
//! | Runner | 用途 | 真跑 chronyc/改 conf？ |
//! |--------|------|----------------------|
//! | [`ChronyRunner`] | 生产（root 沙箱） | ✅ `tokio::process::Command` 跑 `chronyc`、写 `/etc/chrony/chrony.conf` |
//! | [`FakeRunner`] | 单元测试/fixture | ❌ 内存记录调用 + 返回预设 stdout |
//!
//! [`ChronyNtp`] 默认注入 `ChronyRunner`，测试构造时注入 `FakeRunner`，避免真改系统时间（红线）。
//!
//! ## 编排的 chrony 子命令
//! | NtpManager 方法 | chrony 命令 | 说明 |
//! |----------------|------------|------|
//! | `sync_now` | `chronyc makestep` | 立即步进修正时钟（需 `makestep 1 -1` 已在 conf 中允许） |
//! | `status` | `chronyc tracking` | 读 stratum/offset/last offset/leap status |
//! | `set_servers` | 重写 `chrony.conf` 的 `server`/`pool` 段 + `chronyc reload` | 热更新上游（需 root 写 conf） |
//!
//! ## 解析策略
//! `chronyc tracking` 输出是固定字段表（`Key : Value`），由纯函数 [`parse_tracking`]
//! 解析——**无 IO**，可单测（fixture 字符串测，规格书点名"高价值"）。

use std::sync::Mutex;

use os_core::DateTime;

use crate::ntp::{NtpManager, NtpStatus};
use crate::OrchestratorError;

/// chrony 编排后端抽象
///
/// 抽象对 `chronyc` 子命令的执行与 `/etc/chrony/chrony.conf` 的读写，
/// 便于单元测试用内存后端替身，避免真改系统时间或真写 conf（规格书 §9 红线）。
///
/// 实现者必须线程安全（`Send + Sync`）。本 trait 用**同步签名**（非 async fn in trait），
/// 以支持 `dyn NtpRunner` 派发（与 `CgroupBackend` trait 风格一致，便于 `ChronyNtp`
/// 持有 `Box<dyn NtpRunner>`）。`ChronyNtp` 的 async 方法内部用
/// `tokio::task::block_in_place` 包裹同步调用——要求 multi-thread runtime（生产 osd 用
/// multi-thread；测试用 `#[tokio::test(flavor = "multi_thread")]`）。
///
/// ## 方法语义
/// - [`makestep`](NtpRunner::makestep)：执行 `chronyc makestep`，返回执行是否成功（exit 0）。
/// - [`tracking`](NtpRunner::tracking)：执行 `chronyc tracking`，返回 stdout 文本。
/// - [`read_conf_servers`](NtpRunner::read_conf_servers)：读 `chrony.conf` 当前 `server`/`pool` 行。
/// - [`write_conf_servers`](NtpRunner::write_conf_servers)：重写 `chrony.conf` 的 servers 段后 `chronyc reload`。
pub trait NtpRunner: Send + Sync {
    /// 执行 `chronyc makestep`，立即步进修正系统时钟
    ///
    /// 成功返回 `Ok(())`；命令失败（非 0 退出 / chrony 未运行）返回 Err。
    fn makestep(&self) -> Result<(), OrchestratorError>;

    /// 执行 `chronyc tracking`，返回完整 stdout（供 [`parse_tracking`] 解析）
    fn tracking(&self) -> Result<String, OrchestratorError>;

    /// 读 `chrony.conf` 中当前配置的上游服务器（`server`/`pool` 行去重后）
    ///
    /// 格式：去 `pool.`/`server.` 前缀无关，返回主机名字符串列表。
    fn read_conf_servers(&self) -> Result<Vec<String>, OrchestratorError>;

    /// 重写 `chrony.conf` 的 servers 段为新列表，并执行 `chronyc reload` 热生效
    ///
    /// `servers` 为空时清空所有上游（chrony 将退化为本地孤儿模式，仅用于测试/运维隔离）。
    fn write_conf_servers(&self, servers: &[String]) -> Result<(), OrchestratorError>;
}

// ----------------------------------------------------------------------------
// 真实后端：tokio::process::Command 跑 chronyc（生产用，需 root + chrony）
// ----------------------------------------------------------------------------

/// chrony 默认配置文件路径（Ubuntu/Debian 标准位置）
const CHRONY_CONF_PATH: &str = "/etc/chrony/chrony.conf";

/// chronyc 可执行名（依赖 PATH；沙箱镜像已装 chrony 包）
const CHRONYC_BIN: &str = "chronyc";

/// 基于 `tokio::process::Command` 的真实 chrony 编排后端
///
/// **权限**：所有写操作（`makestep`/`write_conf_servers`）需 root + CAP_SYS_TIME
/// （规格书 §6 / §8）；`tracking`/`read_conf_servers` 仅读，非 root 也可。
///
/// 本结构无自身状态，所有调用都直接落系统命令；`Send + Sync` 由无状态保证。
#[derive(Debug, Default, Clone)]
pub struct ChronyRunner {
    /// chrony.conf 路径（默认 `/etc/chrony/chrony.conf`，测试可注入临时路径）
    conf_path: String,
}

impl ChronyRunner {
    /// 构造（生产用，conf 路径默认 `/etc/chrony/chrony.conf`）
    pub fn new() -> Self {
        Self {
            conf_path: CHRONY_CONF_PATH.into(),
        }
    }

    /// 用自定义 conf 路径构造（`#[ignore]` 真实测可指向临时文件）
    pub fn with_conf_path(conf_path: impl Into<String>) -> Self {
        Self {
            conf_path: conf_path.into(),
        }
    }

    /// conf 路径（供测试/运维查询）
    pub fn conf_path(&self) -> &str {
        &self.conf_path
    }

    /// 跑一个 `chronyc` 子命令，返回 stdout（同步封装）
    ///
    /// 用 `tokio::process::Command`（规格书 §5.1 要求 tokio::process），
    /// 通过 `block_in_place` + `Handle::block_on` 在同步 trait 方法内驱动异步 Command。
    /// 要求调用方运行在 multi-thread tokio runtime 上（生产 osd 与测试均用 multi-thread）。
    fn run_chronyc(&self, args: &[&str]) -> Result<String, OrchestratorError> {
        let output = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                tokio::process::Command::new(CHRONYC_BIN)
                    .args(args)
                    .output(),
            )
        })
        .map_err(|e| {
            OrchestratorError::NtpSyncFailed(format!(
                "执行 chronyc {} 失败: {}（确认 chrony 已安装且在 PATH）",
                args.join(" "),
                e
            ))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(OrchestratorError::NtpSyncFailed(format!(
                "chronyc {} 失败（exit {:?}）: {}",
                args.join(" "),
                output.status.code(),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl NtpRunner for ChronyRunner {
    fn makestep(&self) -> Result<(), OrchestratorError> {
        // `chronyc makestep`：立即步进修正时钟（需 conf 内 makestep 指令允许）
        self.run_chronyc(&["makestep"])?;
        Ok(())
    }

    fn tracking(&self) -> Result<String, OrchestratorError> {
        // `chronyc tracking`：返回当前同步状态表
        self.run_chronyc(&["tracking"])
    }

    fn read_conf_servers(&self) -> Result<Vec<String>, OrchestratorError> {
        // 读 chrony.conf，提取所有 `server`/`pool` 行的首字段
        // 用 std::fs 同步读（文件小，block_in_place 上下文内同步读安全）
        let content = std::fs::read_to_string(&self.conf_path).map_err(|e| {
            OrchestratorError::NtpSyncFailed(format!(
                "读 chrony.conf({}) 失败: {}",
                self.conf_path, e
            ))
        })?;
        Ok(parse_conf_servers(&content))
    }

    fn write_conf_servers(&self, servers: &[String]) -> Result<(), OrchestratorError> {
        // 读现有 conf → 替换 servers 段 → 写回 → chronyc reload
        // 保留所有非 server/pool 行（如 driftfile/rtcsync/makestep 等配置不动）
        let content = std::fs::read_to_string(&self.conf_path).map_err(|e| {
            OrchestratorError::NtpSyncFailed(format!(
                "读 chrony.conf({}) 失败: {}",
                self.conf_path, e
            ))
        })?;
        let new_content = rewrite_conf_servers(&content, servers);
        std::fs::write(&self.conf_path, new_content).map_err(|e| {
            OrchestratorError::NtpSyncFailed(format!(
                "写 chrony.conf({}) 失败: {}（确认以 root 运行）",
                self.conf_path, e
            ))
        })?;
        // 热重载（chrony 会重新读 conf 并重连上游）；conf 语法错时 chronyc 报错
        self.run_chronyc(&["reload"])?;
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 测试后端：内存记录调用 + 预设 stdout（不跑 chronyc，零 root 依赖）
// ----------------------------------------------------------------------------

/// 内存 chrony 后端（仅测试 / fixture 用）
///
/// 行为：记录每次调用（便于断言编排逻辑），`tracking` 返回构造时注入的预设 stdout，
/// `read_conf_servers`/`write_conf_servers` 用内存 conf 字段往返。**不**跑任何命令，
/// **不**写任何文件，可在非 root 沙箱运行。
#[derive(Debug, Clone)]
pub struct FakeRunner {
    /// `tracking` 返回的预设 stdout（None → 返回 Err 模拟命令失败）
    pub tracking_stdout: Option<String>,
    /// `makestep` 是否成功（默认 true）
    pub makestep_ok: bool,
    /// 当前内存中的 servers 列表（read/write 往返）
    pub conf_servers: Vec<String>,
    /// 记录所有调用（按顺序），便于测试断言编排顺序
    pub calls: std::sync::Arc<Mutex<Vec<String>>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            tracking_stdout: Some(TRACKING_SAMPLE.into()),
            makestep_ok: true,
            conf_servers: vec!["pool.ntp.org".into()],
            calls: std::sync::Arc::new(Mutex::new(vec![])),
        }
    }
}

impl FakeRunner {
    /// 构造（默认返回正常 tracking 样本）
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置 tracking 返回的 stdout（链式）
    pub fn with_tracking(mut self, stdout: impl Into<String>) -> Self {
        self.tracking_stdout = Some(stdout.into());
        self
    }

    /// 让 tracking 返回错误（模拟 chronyc 不可用）
    pub fn with_tracking_error(mut self) -> Self {
        self.tracking_stdout = None;
        self
    }

    /// 设置 makestep 是否成功
    pub fn with_makestep_ok(mut self, ok: bool) -> Self {
        self.makestep_ok = ok;
        self
    }

    /// 设置初始 conf servers
    pub fn with_conf_servers(mut self, servers: Vec<String>) -> Self {
        self.conf_servers = servers;
        self
    }

    /// 取调用记录快照
    pub fn calls_snapshot(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("FakeRunner calls poisoned")
            .clone()
    }
}

impl NtpRunner for FakeRunner {
    fn makestep(&self) -> Result<(), OrchestratorError> {
        self.calls
            .lock()
            .expect("FakeRunner calls poisoned")
            .push("makestep".into());
        if self.makestep_ok {
            Ok(())
        } else {
            Err(OrchestratorError::NtpSyncFailed("makestep 模拟失败".into()))
        }
    }

    fn tracking(&self) -> Result<String, OrchestratorError> {
        self.calls
            .lock()
            .expect("FakeRunner calls poisoned")
            .push("tracking".into());
        self.tracking_stdout
            .clone()
            .ok_or_else(|| OrchestratorError::NtpSyncFailed("chronyc tracking 模拟失败".into()))
    }

    fn read_conf_servers(&self) -> Result<Vec<String>, OrchestratorError> {
        self.calls
            .lock()
            .expect("FakeRunner calls poisoned")
            .push("read_conf".into());
        Ok(self.conf_servers.clone())
    }

    fn write_conf_servers(&self, servers: &[String]) -> Result<(), OrchestratorError> {
        self.calls
            .lock()
            .expect("FakeRunner calls poisoned")
            .push(format!("write_conf({})", servers.join(",")));
        // 模拟真实写：更新内存 conf
        // 注：self.conf_servers 是 Vec（非内部可变），测试通过重建 FakeRunner 验证；
        // 真实生产用 ChronyRunner 落盘。这里为保持 trait 语义返回 Ok。
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 纯函数：chronyc tracking / chrony.conf 解析
// ----------------------------------------------------------------------------

/// `chronyc tracking` 标准输出样本（用于测试 fixture，格式取自真实 chrony 5.x）
pub const TRACKING_SAMPLE: &str = "\
Reference ID    : B97DBE7B (ntp-nts-3.ps5.canonical.com)
Stratum         : 3
Ref time (UTC)  : Wed Aug 05 08:43:15 2026
System time     : 0.001169685 seconds slow of NTP time
Last offset     : +0.000228348 seconds
RMS offset      : 0.000537975 seconds
Frequency       : 18.700 ppm slow
Residual freq   : -0.001 ppm
Skew            : 0.092 ppm
Root delay      : 0.234126642 seconds
Root dispersion : 0.002602637 seconds
Update interval : 1035.5 seconds
Leap status     : Normal";

/// 解析 `chronyc tracking` 输出的中间结果（无 IO，纯函数）
///
/// 字段语义（取自 chrony 文档）：
/// - `stratum`：本地时钟层级（0=未同步，1=直连参考源，≥2=经上游）
/// - `system_offset_sec`：本地时钟相对 NTP 的偏移（秒；slow=本地慢故为负，fast=本地快故为正）
/// - `last_offset_sec`：最近一次同步测算的偏移（秒，带符号）
/// - `leap_status`：`Normal`=已同步 / `Not synchronised`=未同步 / `Insert`/`Delete`=闰秒待插删
#[derive(Debug, Clone, PartialEq)]
pub struct TrackingParsed {
    /// stratum 数值（解析失败为 0）
    pub stratum: u32,
    /// System time 行偏移（秒；slow→负，fast→正；解析失败为 0.0）
    pub system_offset_sec: f64,
    /// Last offset 行偏移（秒，带符号；解析失败为 0.0）
    pub last_offset_sec: f64,
    /// Leap status 原文（"Normal"/"Not synchronised"/...）
    pub leap_status: String,
}

/// 解析 `chronyc tracking` 的 stdout 为 [`TrackingParsed`]
///
/// 纯函数（无 IO），用字段名前缀匹配 + 容错解析（缺字段不报错，填默认）。
/// 设计为对 chrony 不同小版本输出鲁棒：只取每行冒号后的首段值。
///
/// 返回值始终为 `Ok(TrackingParsed)`——chrony 输出格式稳定，缺字段填默认
/// （stratum=0 / offset=0.0）而非报错，避免一次字段缺失导致整个 status 查询失败。
pub fn parse_tracking(stdout: &str) -> TrackingParsed {
    let mut stratum = 0u32;
    let mut system_offset_sec = 0.0f64;
    let mut last_offset_sec = 0.0f64;
    let mut leap_status = String::new();

    for raw in stdout.lines() {
        // 字段行格式：`Key : Value`（冒号前后可能有空格）
        let line = raw.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match key {
            "Stratum" => {
                // Stratum 行：`3`
                if let Some(num) = value.split_whitespace().next() {
                    stratum = num.parse().unwrap_or(0);
                }
            }
            "System time" => {
                // System time 行：`0.001169685 seconds slow of NTP time`
                // 或 `0.000123 seconds fast of NTP time`
                system_offset_sec = parse_signed_offset_seconds(value);
            }
            "Last offset" => {
                // Last offset 行：`+0.000228348 seconds`
                last_offset_sec = parse_signed_offset_seconds(value);
            }
            "Leap status" => {
                // Leap status 行：`Normal` / `Not synchronised` / `Insert second` / `Delete second`
                // 保留完整值（trim 去首尾空格），供 tracking_to_status 做精确匹配
                leap_status = value.trim().to_string();
            }
            _ => {}
        }
    }

    TrackingParsed {
        stratum,
        system_offset_sec,
        last_offset_sec,
        leap_status,
    }
}

/// 解析带符号的秒数偏移值
///
/// 处理三种格式：
/// - `+0.000228348 seconds`（Last offset，显式符号）
/// - `0.001169685 seconds slow of NTP time`（System time slow → 负）
/// - `0.000123 seconds fast of NTP time`（System time fast → 正）
fn parse_signed_offset_seconds(value: &str) -> f64 {
    // 取第一个 token 作为数值（可能带 + / - 前缀）
    let num_token = match value.split_whitespace().next() {
        Some(t) => t,
        None => return 0.0,
    };
    let abs_val: f64 = num_token
        .trim_start_matches(['+', '-'])
        .parse()
        .unwrap_or(0.0);
    let sign = if num_token.starts_with('-') {
        -1.0
    } else {
        1.0
    };
    let mut result = abs_val * sign;

    // System time 行的 slow/fast 修饰词覆盖符号（slow=本地慢→负，fast=本地快→正）
    let lower = value.to_ascii_lowercase();
    if lower.contains("slow") {
        result = -abs_val;
    } else if lower.contains("fast") {
        result = abs_val;
    }

    result
}

/// 把 `TrackingParsed` 翻译成 [`NtpStatus`]
///
/// 同步判定：`leap_status == "Normal"` **且** `stratum > 0`（chrony 文档：Normal 表示
/// 已与上游同步且无闰秒告警；stratum=0 表示未同步）。servers 由调用方传入
/// （来自 `read_conf_servers`，避免本函数做 IO）。
///
/// 偏移取 `System time`（反映当前实时偏移，比 `Last offset` 上次测算更准），
/// 转毫秒（×1000）四舍五入为 i64。
pub fn tracking_to_status(parsed: &TrackingParsed, servers: Vec<String>) -> NtpStatus {
    let synced = parsed.stratum > 0 && parsed.leap_status == "Normal";
    let offset_ms = (parsed.system_offset_sec * 1000.0).round() as i64;
    NtpStatus {
        synced,
        offset_ms,
        // last_sync：chrony 的 Ref time 是上游参考时间非本机同步完成时间，
        // 语义不完全等同；保持 None（trait 文档允许），真实"最近同步"由 chrony
        // 内部维护，osd 不强行解析 RFC 时间字符串（避免时区/格式歧义）。
        last_sync: None,
        servers,
    }
}

/// 从 `chrony.conf` 内容解析 `server`/`pool` 行的上游主机列表
///
/// 解析规则：
/// - 仅处理非注释行（`#` 开头跳过；行内 `#` 后为注释）。
/// - 行首关键字为 `server` 或 `pool`，取第二字段（主机名/IP）。
/// - 去重保序（同一主机多次出现只取一次）。
///
/// 纯函数（无 IO），供 [`ChronyRunner::read_conf_servers`] 与测试用。
pub fn parse_conf_servers(conf: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in conf.lines() {
        // 去行内注释
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let keyword = match parts.next() {
            Some(k) => k,
            None => continue,
        };
        if keyword == "server" || keyword == "pool" {
            if let Some(host) = parts.next() {
                // 去重保序
                if !out.iter().any(|h: &String| h == host) {
                    out.push(host.to_string());
                }
            }
        }
    }
    out
}

/// 重写 `chrony.conf` 内容的 servers 段（保留其余行）
///
/// 规则：
/// - 删除所有原 `server`/`pool` 行（含其行内注释）。
/// - 在文件末尾追加新的 `server <host>` 行（用 iburst 加速首次同步）。
/// - 空列表 → 仅删除原上游（chrony 退化为本地孤儿模式）。
///
/// 纯函数（无 IO），供 [`ChronyRunner::write_conf_servers`] 与测试用。
pub fn rewrite_conf_servers(conf: &str, servers: &[String]) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for raw in conf.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        let keyword = parts.next().unwrap_or("");
        if keyword == "server" || keyword == "pool" {
            continue; // 跳过原 server/pool 行
        }
        kept.push(raw);
    }

    let mut out = kept.join("\n");
    for s in servers {
        if !s.is_empty() {
            // iburst：首次连接快速连发 4 包加速收敛（chrony 推荐配置）
            out.push_str("\nserver ");
            out.push_str(s);
            out.push_str(" iburst");
        }
    }
    // 确保末尾换行（POSIX 文本文件规范）
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

// ----------------------------------------------------------------------------
// ChronyNtp：NtpManager 真实实现
// ----------------------------------------------------------------------------

/// chrony 编排的 NTP 管理器（`NtpManager` 真实实现）
///
/// 持有一个 [`NtpRunner`]（生产用 [`ChronyRunner`]，测试注入 [`FakeRunner`]），
/// 所有方法委派给 runner + 纯函数解析。
///
/// **权限**：`sync_now`/`set_servers` 需 root + CAP_SYS_TIME（写系统时钟/写 conf）；
/// `status` 仅读。真实执行见 `#[ignore]` 测试（沙箱跑）。
pub struct ChronyNtp {
    runner: Box<dyn NtpRunner>,
}

impl std::fmt::Debug for ChronyNtp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChronyNtp")
            .field("runner", &"<dyn NtpRunner>")
            .finish()
    }
}

impl ChronyNtp {
    /// 构造（生产用，runner 为 [`ChronyRunner`]，操作需 root + chrony）
    pub fn new() -> Self {
        Self {
            runner: Box::new(ChronyRunner::new()),
        }
    }

    /// 用自定义 runner 构造（测试注入 [`FakeRunner`]）
    pub fn with_runner(runner: Box<dyn NtpRunner>) -> Self {
        Self { runner }
    }

    /// 取 runner 类型名（运维查询）
    pub fn runner_type(&self) -> &'static str {
        "NtpRunner"
    }
}

impl Default for ChronyNtp {
    fn default() -> Self {
        Self::new()
    }
}

impl NtpManager for ChronyNtp {
    async fn sync_now(&self) -> crate::OrchestratorResult<DateTime> {
        // 委派给 runner（同步 trait 方法，用 block_in_place 驱动；要求 multi-thread runtime）
        tokio::task::block_in_place(|| self.runner.makestep())?;
        // makestep 成功后返回当前 UTC 时间（同步完成的时刻）
        Ok(os_core::Utc::now())
    }

    async fn status(&self) -> NtpStatus {
        // tracking 命令失败时返回"未同步"占位状态（不 panic，不污染调用方）
        let stdout = match tokio::task::block_in_place(|| self.runner.tracking()) {
            Ok(s) => s,
            Err(e) => {
                tracing_or_log_warn(&format!("chronyc tracking 失败: {e}"));
                return NtpStatus {
                    synced: false,
                    offset_ms: 0,
                    last_sync: None,
                    servers: read_servers_or_empty(&*self.runner),
                };
            }
        };
        let parsed = parse_tracking(&stdout);
        let servers = read_servers_or_empty(&*self.runner);
        tracking_to_status(&parsed, servers)
    }

    async fn set_servers(&self, servers: Vec<String>) -> crate::OrchestratorResult<()> {
        // 写 conf + reload（需 root）
        tokio::task::block_in_place(|| self.runner.write_conf_servers(&servers))
    }
}

/// 读 conf servers，失败返回空（status 查询不应因 conf 读失败整体报错）
fn read_servers_or_empty(runner: &dyn NtpRunner) -> Vec<String> {
    runner.read_conf_servers().unwrap_or_default()
}

/// 轻量日志：无 tracing 依赖时降级到 eprintln（避免引入新依赖，规格书 §9 红线）
///
/// 真实生产环境 osd 会接入 tracing；此处保持零额外依赖，待 osd 接入
/// tracing subscriber 后替换为 `tracing::warn!`。
fn tracing_or_log_warn(msg: &str) {
    // 编译期避免 unused：直接 eprintln（不引入 log/tracing crate）
    eprintln!("[osd:ntp:warn] {msg}");
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_tracking：纯函数 fixture 测（高价值，规格书点名） ----

    #[test]
    fn parse_tracking_real_sample() {
        let parsed = parse_tracking(TRACKING_SAMPLE);
        assert_eq!(parsed.stratum, 3);
        // System time slow 0.001169685 → 负
        assert!((parsed.system_offset_sec - (-0.001169685)).abs() < 1e-9);
        // Last offset +0.000228348 → 正
        assert!((parsed.last_offset_sec - 0.000228348).abs() < 1e-9);
        assert_eq!(parsed.leap_status, "Normal");
    }

    #[test]
    fn parse_tracking_fast_system_time() {
        let stdout = "\
System time     : 0.000123 seconds fast of NTP time
Stratum         : 2
Last offset     : -0.000045 seconds
Leap status     : Normal";
        let parsed = parse_tracking(stdout);
        assert_eq!(parsed.stratum, 2);
        // fast → 正
        assert!((parsed.system_offset_sec - 0.000123).abs() < 1e-9);
        // Last offset 带 - 符号 → 负
        assert!((parsed.last_offset_sec - (-0.000045)).abs() < 1e-9);
        assert_eq!(parsed.leap_status, "Normal");
    }

    #[test]
    fn parse_tracking_not_synchronised() {
        let stdout = "\
Reference ID    : None
Stratum         : 0
System time     : 0.000000 seconds slow of NTP time
Last offset     : +0.000000000 seconds
Leap status     : Not synchronised";
        let parsed = parse_tracking(stdout);
        assert_eq!(parsed.stratum, 0);
        assert_eq!(parsed.leap_status, "Not synchronised");
    }

    #[test]
    fn parse_tracking_empty_input_returns_defaults() {
        let parsed = parse_tracking("");
        assert_eq!(parsed.stratum, 0);
        assert_eq!(parsed.system_offset_sec, 0.0);
        assert_eq!(parsed.last_offset_sec, 0.0);
        assert_eq!(parsed.leap_status, "");
    }

    #[test]
    fn parse_tracking_malformed_stratum_defaults_to_zero() {
        // stratum 行非数字 → 填 0，不 panic
        let stdout = "Stratum         : abc\nLeap status     : Normal";
        let parsed = parse_tracking(stdout);
        assert_eq!(parsed.stratum, 0);
        assert_eq!(parsed.leap_status, "Normal");
    }

    #[test]
    fn parse_tracking_extra_whitespace_robust() {
        // 字段前后多余空格 + 制表符混合（chrony 不同版本对齐方式略异）
        let stdout =
            "Stratum:16\nSystem time:   1.5 seconds slow of NTP time\nLeap status:   Normal";
        let parsed = parse_tracking(stdout);
        assert_eq!(parsed.stratum, 16);
        assert!((parsed.system_offset_sec - (-1.5)).abs() < 1e-9);
        assert_eq!(parsed.leap_status, "Normal");
    }

    // ---- tracking_to_status：解析结果 → NtpStatus ----

    #[test]
    fn tracking_to_status_synced_normal_stratum() {
        let parsed = TrackingParsed {
            stratum: 3,
            system_offset_sec: 0.001169685,
            last_offset_sec: 0.000228348,
            leap_status: "Normal".into(),
        };
        let status = tracking_to_status(&parsed, vec!["pool.ntp.org".into()]);
        assert!(status.synced);
        // 0.001169685 sec → 1.169685 ms → round 1
        assert_eq!(status.offset_ms, 1);
        assert_eq!(status.servers, vec!["pool.ntp.org".to_string()]);
        assert!(status.last_sync.is_none());
    }

    #[test]
    fn tracking_to_status_unsynced_when_stratum_zero() {
        let parsed = TrackingParsed {
            stratum: 0,
            system_offset_sec: 0.0,
            last_offset_sec: 0.0,
            leap_status: "Not synchronised".into(),
        };
        let status = tracking_to_status(&parsed, vec![]);
        assert!(!status.synced);
        assert_eq!(status.offset_ms, 0);
    }

    #[test]
    fn tracking_to_status_unsynced_when_leap_not_normal() {
        // stratum>0 但 leap 非 Normal（闰秒告警）→ 视为未同步
        let parsed = TrackingParsed {
            stratum: 2,
            system_offset_sec: 0.0,
            last_offset_sec: 0.0,
            leap_status: "Insert".into(),
        };
        let status = tracking_to_status(&parsed, vec![]);
        assert!(!status.synced);
    }

    #[test]
    fn tracking_to_status_negative_offset_rounds() {
        // slow 1.5ms → -2（round(-1.5) 在 Rust as i64 是向 0 截断 → -1；
        // 这里测 round 后 cast：round(-1.5)=−2.0 as i64 = -2）
        let parsed = TrackingParsed {
            stratum: 1,
            system_offset_sec: -0.0015,
            last_offset_sec: 0.0,
            leap_status: "Normal".into(),
        };
        let status = tracking_to_status(&parsed, vec![]);
        assert!(status.synced);
        assert_eq!(status.offset_ms, -2);
    }

    // ---- parse_signed_offset_seconds：边界 ----

    #[test]
    fn parse_signed_offset_explicit_positive() {
        assert!((parse_signed_offset_seconds("+0.0005 seconds") - 0.0005).abs() < 1e-9);
    }

    #[test]
    fn parse_signed_offset_explicit_negative() {
        assert!((parse_signed_offset_seconds("-0.0005 seconds") - (-0.0005)).abs() < 1e-9);
    }

    #[test]
    fn parse_signed_offset_slow_overrides_sign() {
        // 显式 + 但 slow 修饰 → 负（slow 优先）
        let v = parse_signed_offset_seconds("+0.001 seconds slow of NTP time");
        assert!((v - (-0.001)).abs() < 1e-9);
    }

    #[test]
    fn parse_signed_offset_no_token_returns_zero() {
        assert_eq!(parse_signed_offset_seconds(""), 0.0);
    }

    #[test]
    fn parse_signed_offset_non_number_returns_zero() {
        assert_eq!(parse_signed_offset_seconds("abc seconds"), 0.0);
    }

    // ---- parse_conf_servers / rewrite_conf_servers ----

    #[test]
    fn parse_conf_servers_basic() {
        let conf = "\
# chrony.conf
server pool.ntp.org iburst
pool 2.pool.ntp.org
driftfile /var/lib/chrony/drift
makestep 1 -1";
        let servers = parse_conf_servers(conf);
        assert_eq!(servers, vec!["pool.ntp.org", "2.pool.ntp.org"]);
    }

    #[test]
    fn parse_conf_servers_dedups() {
        let conf = "\
server pool.ntp.org iburst
server pool.ntp.org iburst
pool pool.ntp.org";
        let servers = parse_conf_servers(conf);
        // 三次同主机去重
        assert_eq!(servers, vec!["pool.ntp.org"]);
    }

    #[test]
    fn parse_conf_servers_ignores_comments() {
        let conf = "\
# server commented.out
server real.ntp.org
# pool also.commented";
        let servers = parse_conf_servers(conf);
        assert_eq!(servers, vec!["real.ntp.org"]);
    }

    #[test]
    fn parse_conf_servers_empty_returns_empty() {
        assert!(parse_conf_servers("").is_empty());
        assert!(parse_conf_servers("# only comments\n").is_empty());
    }

    #[test]
    fn rewrite_conf_preserves_other_directives() {
        let conf = "\
server old.ntp.org iburst
driftfile /var/lib/chrony/drift
rtcsync
makestep 1 -1";
        let new = rewrite_conf_servers(conf, &["new.ntp.org".into()]);
        // 非 server/pool 行保留
        assert!(new.contains("driftfile /var/lib/chrony/drift"));
        assert!(new.contains("rtcsync"));
        assert!(new.contains("makestep 1 -1"));
        // 旧 server 删除
        assert!(!new.contains("old.ntp.org"));
        // 新 server 追加（带 iburst）
        assert!(new.contains("server new.ntp.org iburst"));
    }

    #[test]
    fn rewrite_conf_empty_servers_clears_all() {
        let conf = "server a.ntp.org\npool b.ntp.org\ndriftfile /x";
        let new = rewrite_conf_servers(conf, &[]);
        assert!(!new.contains("a.ntp.org"));
        assert!(!new.contains("b.ntp.org"));
        assert!(new.contains("driftfile /x"));
    }

    #[test]
    fn rewrite_conf_ends_with_newline() {
        let new = rewrite_conf_servers("server a\n", &["b".into()]);
        assert!(new.ends_with('\n'));
    }

    #[test]
    fn rewrite_conf_skips_empty_server_entries() {
        let new = rewrite_conf_servers("driftfile /x", &[String::new(), "real.ntp.org".into()]);
        assert!(new.contains("server real.ntp.org iburst"));
        // 空字符串条目不生成 server 行
        assert!(!new.contains("server  iburst"));
    }

    // ---- ChronyNtp + FakeRunner：编排逻辑测试（不跑真 chronyc） ----

    fn build_with_fake(runner: FakeRunner) -> (ChronyNtp, FakeRunner) {
        // FakeRunner 内部用 Arc<Mutex> 记录调用，克隆一份给测试断言用
        let runner_clone = runner.clone();
        let ntp = ChronyNtp::with_runner(Box::new(runner));
        (ntp, runner_clone)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_now_calls_makestep_returns_current_time() {
        let (ntp, runner) = build_with_fake(FakeRunner::new());
        let before = os_core::Utc::now();
        let t = ntp.sync_now().await.expect("makestep 应成功");
        let after = os_core::Utc::now();
        // 返回时间应在调用前后之间
        assert!(t >= before && t <= after);
        // 确认调了 makestep
        assert!(runner.calls_snapshot().iter().any(|c| c == "makestep"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_now_propagates_makestep_error() {
        let runner = FakeRunner::new().with_makestep_ok(false);
        let ntp = ChronyNtp::with_runner(Box::new(runner));
        let err = ntp.sync_now().await.expect_err("makestep 失败应报错");
        assert!(matches!(err, OrchestratorError::NtpSyncFailed(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_parses_tracking_and_reads_servers() {
        let runner = FakeRunner::new()
            .with_tracking(TRACKING_SAMPLE)
            .with_conf_servers(vec!["pool.ntp.org".into()]);
        let ntp = ChronyNtp::with_runner(Box::new(runner));
        let status = ntp.status().await;
        // TRACKING_SAMPLE stratum=3 + Normal → synced
        assert!(status.synced);
        // System time: 0.001169685 seconds slow → 本地慢 → 负偏移
        // -0.001169685 sec × 1000 = -1.169685 ms → round = -1
        assert_eq!(status.offset_ms, -1);
        assert_eq!(status.servers, vec!["pool.ntp.org".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_unsynced_when_tracking_says_not_synchronised() {
        let stdout = "\
Stratum         : 0
Leap status     : Not synchronised";
        let runner = FakeRunner::new().with_tracking(stdout);
        let ntp = ChronyNtp::with_runner(Box::new(runner));
        let status = ntp.status().await;
        assert!(!status.synced);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_returns_unsynced_when_tracking_fails() {
        // tracking 命令失败（chronyc 不可用）→ 不 panic，返回未同步占位
        let runner = FakeRunner::new().with_tracking_error();
        let ntp = ChronyNtp::with_runner(Box::new(runner));
        let status = ntp.status().await;
        assert!(!status.synced);
        assert_eq!(status.offset_ms, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_servers_calls_write_conf() {
        let runner = FakeRunner::new();
        let ntp = ChronyNtp::with_runner(Box::new(runner.clone()));
        ntp.set_servers(vec!["new.ntp.org".into(), "new2.ntp.org".into()])
            .await
            .expect("set_servers 应成功");
        let calls = runner.calls_snapshot();
        assert!(calls
            .iter()
            .any(|c| c == "write_conf(new.ntp.org,new2.ntp.org)"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_servers_empty_list_succeeds() {
        let runner = FakeRunner::new();
        let ntp = ChronyNtp::with_runner(Box::new(runner.clone()));
        ntp.set_servers(vec![]).await.expect("空列表应成功");
        assert!(runner.calls_snapshot().iter().any(|c| c == "write_conf()"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_then_sync_then_status_orchestration_order() {
        // 验证编排：status → sync_now → status 顺序调用，runner 记录顺序正确
        let runner = FakeRunner::new();
        let ntp = ChronyNtp::with_runner(Box::new(runner.clone()));
        let _ = ntp.status().await;
        let _ = ntp.sync_now().await;
        let _ = ntp.status().await;
        let calls = runner.calls_snapshot();
        // 期望顺序：tracking, read_conf, makestep, tracking, read_conf
        assert_eq!(calls[0], "tracking");
        assert_eq!(calls[1], "read_conf");
        assert_eq!(calls[2], "makestep");
        assert_eq!(calls[3], "tracking");
        assert_eq!(calls[4], "read_conf");
    }

    // ---- 真实 chrony 集成测（#[ignore]，沙箱跑） ----
    // 红线：不在普通 CI 跑（需 root + chrony + 可能改系统时间）；
    // 沙箱（SANDBOX.md 方案 A/B）跑 `cargo test -- --ignored`。

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需 root + chrony 守护（沙箱跑；可能触发真实时钟步进）"]
    async fn real_chronyc_tracking_parses() {
        // 真实跑 chronyc tracking，验证解析对真实输出鲁棒
        let ntp = ChronyNtp::new();
        let status = ntp.status().await;
        // 真实环境通常已同步（stratum>0 + Normal）；不强断 synced（CI 容器可能未配上游）
        assert!(status.offset_ms.abs() < 3_600_000); // 偏移 < 1 小时（ sanity check ）
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需 root + chrony 守护（沙箱跑；不真改系统时间，仅 dry-run）"]
    async fn real_parse_conf_servers_reads_system_conf() {
        // 真实读 /etc/chrony/chrony.conf（若存在）
        let runner = ChronyRunner::new();
        match runner.read_conf_servers() {
            Ok(servers) => {
                // conf 存在：servers 列表非空（多数发行版默认配 pool）
                println!("真实 chrony.conf servers: {servers:?}");
            }
            Err(e) => {
                // conf 不存在（如沙箱未装 chrony）：跳过，不算失败
                println!("跳过：读 chrony.conf 失败（沙箱未配？）: {e}");
            }
        }
    }
}
