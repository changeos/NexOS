//! `PowerRouteHandler` —— 系统自举「电源控制层」（Power Control）REST 入口。
//!
//! 定位：PXE 装机流水线的**第一环**——先唤醒/上电，再 PXE 引导，最后 SSH 部署
//! 收尾（`provisioning.rs` 的 PXE / ISO / SSH 三件套负责后两环）。四个功能域：
//!
//! - **本机 BMC（`bmc/*`，in-band）**：真实 spawn `ipmitool chassis status` /
//!   `sel info` / `mc info` / `sensor list` / `chassis power <action>` 子进程
//!   （argv 直传不经 shell）。构造时一次性探测 PATH 上的 ipmitool
//!   （[`which_binary`]）；缺失或本机无 `/dev/ipmi0`（驱动未加载）时端点返回
//!   **明确降级说明**（200 + `available:false` + 安装/加载指引），绝不 500。
//! - **远程 IPMI 2.0 设备（`ipmi/devices*`，RMCP+ over lanplus）**：设备注册表
//!   （JSON 持久化，密码落盘明文——注释与文档均标注生产须 vault 化），
//!   `ipmitool -I lanplus -H -p -U -P [-C <cipher>] ...` 远程 test / status /
//!   power（同样 argv 直传：主机/用户名/密码即使含 shell 元字符也无注入面）。
//!   列表/详情响应**密码脱敏**（`password:null` + `has_password` 布尔）。
//! - **网段扫描（`ipmi/scan`）**：**纯 Rust 实现 RMCP Presence Ping**
//!   （IPMI 2.0 规范 §13.5 免凭据探测，UDP/623）：tokio UDP 并发探测 CIDR
//!   全地址（仅允许 /24~ /32，≤256 地址，防误扫大网段），应答解析出
//!   IP / RMCP+ 支持 / IPMI 实体位 / ASF 版本 / 企业号。长任务后台化
//!   （`tokio::spawn`），前端轮询 `GET /power/ipmi/scan/:id` 取进度与结果。
//! - **LAN 魔术唤醒（`wol/*`）**：WoL 目标注册表 + 魔术包构造
//!   （6×0xFF + 16×MAC [+ 6B SecureOn 密码]）+ UDP 广播发送
//!   （默认 3 次、间隔 100ms，提高不同网卡固件的唤醒成功率）。
//!   不依赖 ipmitool，ipmitool 缺失时照常可用。`GET /power/wol/arp`
//!   读 `ip neigh` 列出局域网邻居 MAC，辅助选目标。
//!
//! # 状态持久化
//!
//! 设备与 WoL 目标落 JSON（env `NEXOS_POWER_STATE`，缺省
//! `/tank/os-data/power-state.json`；**原子写**：先写 `.tmp` 再 rename，
//! 与 update.rs 同款手法）。扫描任务是短生命周期观测态，仅存内存
//! （上限 50 条，防膨胀）。
//!
//! # 安全
//!
//! - 子进程一律 argv 直传（无 shell 拼接），参数含元字符也不构成注入；
//! - 设备密码仅用于 spawn `-P` 参数，API 响应一律脱敏；
//! - 远程电源控制 / 设备注册 / 扫描发起等写操作要求 admin；
//!   `POST /power/wol/wake` 开发期公开（广播包无凭据，风险为「误唤醒」），
//!   生产硬化清单见 docs/PROVISIONING.md §电源控制层。
//! - 子进程输出截 16KB（`POWER_OUTPUT_CAP`），传感器表截 200 行。
//!
//! # 路由表（16 条，前缀 `/api/v1/provisioning/power`）
//!
//! | method | path                              | 动作 |
//! |--------|-----------------------------------|------|
//! | GET    | `/power/bmc`                      | 本机 BMC 聚合状态（chassis/SEL/MC）|
//! | POST   | `/power/bmc/power`                | 本机电源控制 on/off/cycle/soft（admin）|
//! | GET    | `/power/bmc/sensors`              | 本机传感器表（截 200 行）|
//! | GET    | `/power/ipmi/devices`             | 远程设备列表（密码脱敏）|
//! | POST   | `/power/ipmi/devices`             | 注册设备（admin）|
//! | DELETE | `/power/ipmi/devices/:id`         | 删设备（admin）|
//! | POST   | `/power/ipmi/devices/:id/test`    | 连通性测试（admin，真实 lanplus）|
//! | POST   | `/power/ipmi/devices/:id/power`   | 远程电源控制（admin）|
//! | GET    | `/power/ipmi/devices/:id/status`  | 远程 chassis status（实时）|
//! | POST   | `/power/ipmi/scan`                | 发起网段扫描（admin，后台任务）|
//! | GET    | `/power/ipmi/scan/:id`            | 扫描任务状态+结果 |
//! | GET    | `/power/wol/targets`              | WoL 目标列表 |
//! | POST   | `/power/wol/targets`              | 注册 WoL 目标（admin）|
//! | DELETE | `/power/wol/targets/:id`          | 删 WoL 目标（admin）|
//! | POST   | `/power/wol/wake`                 | 发送魔术包（开发期公开）|
//! | GET    | `/power/wol/arp`                  | 局域网邻居（ip neigh 解析）|

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// 本机/远程 ipmitool 子进程整体超时（BMC 偶发慢，10s 兜住不挂死）。
const IPMI_TIMEOUT: Duration = Duration::from_secs(10);

/// `ip neigh` 子进程超时。
const IP_NEIGH_TIMEOUT: Duration = Duration::from_secs(5);

/// 子进程输出捕获上限（stdout/stderr 各 16KB，防内存放大）。
const POWER_OUTPUT_CAP: usize = 16 * 1024;

/// 传感器表行数上限（超出截断并标记 truncated）。
const SENSOR_ROWS_MAX: usize = 200;

/// 扫描地址数上限（仅允许 /24 ~ /32，≤256 地址，防误扫大网段）。
const SCAN_MAX_ADDRESSES: usize = 256;

/// 扫描并发缺省值。
const SCAN_CONCURRENCY_DEFAULT: usize = 64;

/// RMCP（ASF）UDP 端口缺省值。
const RMCP_PORT_DEFAULT: u16 = 623;

/// 扫描任务记录上限（内存态防膨胀，超出丢最旧）。
const SCAN_TASKS_MAX: usize = 50;

/// WoL 魔术包发送次数（不同网卡固件对首次广播包丢弃率不同，多发提高成功率）。
const WOL_SEND_ATTEMPTS: usize = 3;

/// 相邻两次魔术包的间隔。
const WOL_SEND_INTERVAL: Duration = Duration::from_millis(100);

/// WoL 缺省广播地址。
const WOL_BROADCAST_DEFAULT: &str = "255.255.255.255";

/// WoL 缺省 UDP 端口（discard 协议 9，多数网卡固件默认监听 9 或 7）。
const WOL_PORT_DEFAULT: u16 = 9;

/// ASF 规范的 IANA 企业号（4542 = 0x011BE，RMCP Presence Ping/Pong 帧标识）。
const ASF_IANA_ENTERPRISE: u32 = 4542;

/// RMCP 版本 1.0（帧首字节 0x06）。
const RMCP_VERSION_1: u8 = 0x06;

/// RMCP 消息类：ASF Presence Ping（IPMI 2.0 规范 §13.5 RMCP+ 发现用）。
const RMCP_CLASS_ASF_PING: u8 = 0x06;

/// RMCP 消息类：ASF Presence Pong（IPMI 2.0 规范定义的应答类；多数 BMC 回显 0x06）。
const RMCP_CLASS_ASF_PONG: u8 = 0x07;

/// ASF 消息类型：Presence Ping（ipmitool `asf.h` `ASF_TYPE_PING`）。
const ASF_TYPE_PING: u8 = 0x80;

/// ASF 消息类型：Presence Pong（ipmitool `asf.h` `ASF_TYPE_PONG`；个别实现用 0x81）。
const ASF_TYPE_PONG: u8 = 0x40;

// ----------------------------------------------------------------------------
// DTO —— 本机 BMC
// ----------------------------------------------------------------------------

/// 一行 `key : value` 解析结果（chassis status / sel info / mc info 通用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvLine {
    pub key: String,
    pub value: String,
}

/// `GET /power/bmc` 响应：本机 BMC 聚合状态。
#[derive(Debug, Clone, Serialize)]
pub struct BmcInfo {
    /// 本机 BMC 是否可用（ipmitool 在且 chassis status 命令成功）。
    pub available: bool,
    /// PATH 上是否探测到 ipmitool 二进制。
    pub ipmitool_found: bool,
    /// chassis status 键值（System Power / Power Overload / ...）。
    pub chassis: Vec<KvLine>,
    /// SEL（系统事件日志）摘要键值（Entries / Percent Used / ...）。
    pub sel: Vec<KvLine>,
    /// MC（BMC 自身）信息键值（Firmware Revision / Manufacturer ID / ...）。
    pub mc: Vec<KvLine>,
    /// 便捷字段：System Power（on/off；不可用为 null）。
    pub system_power: Option<String>,
    /// 降级说明（不可用时的安装/加载指引）。
    pub hint: Option<String>,
    /// 最近一次错误摘要（stderr，截断）。
    pub error: Option<String>,
}

/// `GET /power/bmc/sensors` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct SensorsInfo {
    pub available: bool,
    pub count: usize,
    /// 是否因超出 200 行被截断。
    pub truncated: bool,
    pub rows: Vec<SensorRow>,
    pub hint: Option<String>,
}

/// 一行 `ipmitool sensor list` 表格（竖线分隔）解析结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SensorRow {
    pub name: String,
    /// 传感器类型（Temperature / Fan / Voltage ...）。
    #[serde(rename = "type")]
    pub sensor_type: String,
    /// 读数（含单位，如 "26 (+/- 0) degrees C"）。
    pub reading: String,
    /// ok / nr / cr / nc / ...（空表示无该列）。
    pub status: String,
    /// 原始行（列数异常的 BMC 输出兜底展示）。
    pub raw: String,
}

// ----------------------------------------------------------------------------
// DTO —— 远程 IPMI 设备
// ----------------------------------------------------------------------------

/// 远程 IPMI 2.0 设备（RMCP+ / lanplus）。
///
/// `password` 随状态文件持久化（**明文**——开发期取舍，生产须 vault 化，
/// 见 docs/PROVISIONING.md 生产硬化清单）；API 响应经 [`device_masked`] 脱敏，
/// 密码永不出现在 HTTP 输出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpmiDevice {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// BMC 密码（持久化用；响应脱敏）。
    pub password: Option<String>,
    /// 可选 cipher suite（ipmitool `-C`，如 "3"/"17"）。
    pub cipher: Option<String>,
    /// unknown / reachable / unreachable
    pub status: String,
    pub last_checked: Option<String>,
    pub created_at: String,
}

/// `POST /power/ipmi/devices/:id/test` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceTestResult {
    pub reachable: bool,
    /// 连通时附 chassis status 键值。
    pub chassis: Vec<KvLine>,
    pub system_power: Option<String>,
    /// ipmitool 原始输出摘要（stderr 错误信息在此）。
    pub output: String,
    pub duration_ms: u64,
}

/// `POST .../power`（本机与远程同形）响应。
#[derive(Debug, Clone, Serialize)]
pub struct PowerActionResult {
    pub ok: bool,
    pub action: String,
    /// 本机/远程目标描述（host 或 "bmc"）。
    pub target: String,
    pub output: String,
    pub error: Option<String>,
}

/// `GET /power/ipmi/devices/:id/status` 响应（实时探测）。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatusInfo {
    pub reachable: bool,
    pub chassis: Vec<KvLine>,
    /// 便捷字段：电源开/关/未知。
    pub system_power: Option<String>,
    /// 识别灯（chassis identify 灯态，BMC 有则解析）。
    pub identify: Option<String>,
    pub checked_at: String,
    pub error: Option<String>,
}

// ----------------------------------------------------------------------------
// DTO —— 网段扫描（RMCP+ 发现）
// ----------------------------------------------------------------------------

/// 一条 Presence Pong 应答解析结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PongInfo {
    /// RMCP 消息类字节（0x06=回显 ping 类 / 0x07=IPMI 2.0 规范 pong 类）。
    pub message_class: u8,
    /// 支持的 ASF 版本（实体位 bit0 置位 → "1.0"，否则 "unknown"）。
    pub asf_version: String,
    /// IPMI 实体位（data[8] bit7）——BMC 是 IPMI 管理板。
    pub ipmi_supported: bool,
    /// RMCP+（IPMI 2.0）支持：IPMI 2.0 规范要求 RMCP+ BMC 必答 Presence Pong，
    /// 此处据「pong 类字节为 0x07 或 IPMI 实体位置位」判定；
    /// 严格会话能力（RAKP/cipher）在设备 test（lanplus）时确认。
    pub rmcp_plus_supported: bool,
    /// ASF 规范企业号（4542 = ASF；不同 BMC 字节序不一，命中 4542 时归一）。
    pub enterprise_iana: u32,
    /// OEM 自定义区（data[4..8]）。
    pub oem_iana: u32,
    /// 支持的交互（data[9]，ASF 侧保留位）。
    pub supported_interactions: u8,
}

/// 一个扫描命中的设备。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanHit {
    pub ip: String,
    #[serde(flatten)]
    pub pong: PongInfo,
}

/// 扫描任务（running → completed / failed）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanTask {
    pub id: String,
    pub cidr: String,
    pub port: u16,
    /// running / completed / failed
    pub status: String,
    /// 已发出探测包的地址数。
    pub scanned: usize,
    pub total: usize,
    /// 命中设备（去重，按 IP）。
    pub found: Vec<ScanHit>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

// ----------------------------------------------------------------------------
// DTO —— WoL
// ----------------------------------------------------------------------------

/// WoL 唤醒目标。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WolTarget {
    pub id: String,
    pub name: String,
    /// 规范化后的 MAC（小写冒号分隔）。
    pub mac: String,
    /// 广播地址（缺省 255.255.255.255）。
    pub broadcast: String,
    /// UDP 端口（缺省 9）。
    pub port: u16,
    /// SecureOn 密码（6 字节，MAC 同格式；持久化用，响应脱敏）。
    pub secureon_password: Option<String>,
    pub created_at: String,
}

/// `POST /power/wol/wake` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct WakeResult {
    pub ok: bool,
    /// 目标名或临时 MAC。
    pub target: String,
    pub mac: String,
    pub broadcast: String,
    pub port: u16,
    /// 尝试次数 / 实际成功发出的 UDP 包数。
    pub attempts: usize,
    pub sent: usize,
    /// 单包字节数（102；含 SecureOn 为 108）。
    pub bytes_per_packet: usize,
    /// 是否追加了 SecureOn 密码。
    pub secureon: bool,
    pub error: Option<String>,
}

/// 一条 `ip neigh` 邻居。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArpEntry {
    pub ip: String,
    pub mac: String,
    pub dev: String,
    /// REACHABLE / STALE / DELAY / PERMANENT ...
    pub state: String,
}

/// `GET /power/wol/arp` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct ArpInfo {
    /// `ip` 命令是否可用。
    pub available: bool,
    pub neighbors: Vec<ArpEntry>,
    pub hint: Option<String>,
}

// ----------------------------------------------------------------------------
// 纯函数：ipmitool 输出解析
// ----------------------------------------------------------------------------

/// 解析 ipmitool 键值型输出（chassis status / sel info / mc info）。
///
/// 行格式：`System Power         : on`（任意空白 + `:` 分隔）。无分隔符的行跳过。
#[must_use]
pub fn parse_kv_lines(text: &str) -> Vec<KvLine> {
    text.lines()
        .filter_map(|line| {
            let pos = line.find(':')?;
            let key = line[..pos].trim();
            let value = line[pos + 1..].trim();
            if key.is_empty() {
                return None;
            }
            Some(KvLine {
                key: key.to_string(),
                value: value.to_string(),
            })
        })
        .collect()
}

/// 从键值对里取指定键（忽略大小写）。
#[must_use]
pub fn kv_get<'a>(kvs: &'a [KvLine], key: &str) -> Option<&'a str> {
    kvs.iter()
        .find(|kv| kv.key.eq_ignore_ascii_case(key))
        .map(|kv| kv.value.as_str())
}

/// 解析 `ipmitool sensor list` 表格（竖线分隔）为行数组。
///
/// 列序（ipmitool 手册）：`Sensor | Type | Reading | Status | ...`；
/// 列数不足的行仍保留（raw 兜底），超 [`SENSOR_ROWS_MAX`] 行截断。
#[must_use]
pub fn parse_sensor_list(text: &str, cap: usize) -> (Vec<SensorRow>, bool) {
    let mut rows = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if rows.len() >= cap {
            truncated = true;
            break;
        }
        let cols: Vec<&str> = line.split('|').map(|c| c.trim()).collect();
        if cols.len() < 2 || cols.iter().all(|c| c.is_empty()) {
            continue; // 表头分隔线等噪声
        }
        let col =
            |i: usize| -> String { cols.get(i).map_or_else(String::new, |c| (*c).to_string()) };
        rows.push(SensorRow {
            name: col(0),
            sensor_type: col(1),
            reading: col(2),
            status: col(3),
            raw: line.to_string(),
        });
    }
    (rows, truncated)
}

// ----------------------------------------------------------------------------
// 纯函数：ipmitool 命令组装（argv 直传，无 shell）
// ----------------------------------------------------------------------------

/// 组装本机（in-band）ipmitool 参数：`chassis status` / `chassis power on` 等。
#[must_use]
pub fn ipmi_local_argv(kind: IpmiCmd) -> Vec<String> {
    match kind {
        IpmiCmd::ChassisStatus => vec!["chassis".into(), "status".into()],
        IpmiCmd::ChassisPower(action) => vec!["chassis".into(), "power".into(), action],
        IpmiCmd::SelInfo => vec!["sel".into(), "info".into()],
        IpmiCmd::McInfo => vec!["mc".into(), "info".into()],
        IpmiCmd::SensorList => vec!["sensor".into(), "list".into()],
    }
}

/// 组装远程（RMCP+ / lanplus）ipmitool 参数。
///
/// 形如 `-I lanplus -H host -p port -U user -P pass [-C cipher] chassis ...`。
/// 全部经 argv 直传（tokio Command 不经 shell），host/用户/密码含元字符亦无注入面。
#[must_use]
pub fn ipmi_remote_argv(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    cipher: Option<&str>,
    kind: IpmiCmd,
) -> Vec<String> {
    let mut argv = vec![
        "-I".to_string(),
        "lanplus".to_string(),
        "-H".to_string(),
        host.to_string(),
        "-p".to_string(),
        port.to_string(),
        "-U".to_string(),
        username.to_string(),
        "-P".to_string(),
        password.to_string(),
    ];
    if let Some(c) = cipher.filter(|c| !c.trim().is_empty()) {
        argv.push("-C".into());
        argv.push(c.trim().to_string());
    }
    argv.extend(ipmi_local_argv(kind));
    argv
}

/// ipmitool 子命令族（本机/远程共用组装尾部）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpmiCmd {
    ChassisStatus,
    /// `chassis power <action>`（on/off/cycle/soft）。
    ChassisPower(String),
    SelInfo,
    McInfo,
    SensorList,
}

/// 合法电源动作（ipmitool chassis power 子命令）。
pub const POWER_ACTIONS: [&str; 4] = ["on", "off", "cycle", "soft"];

// ----------------------------------------------------------------------------
// 纯函数：CIDR 展开
// ----------------------------------------------------------------------------

/// 展开 IPv4 CIDR 为地址列表（**仅允许 /24 ~ /32**，≤256 地址）。
///
/// - `192.0.2.0/24` → 256 个地址（含网络/广播地址——BMC 可被配置在任意位）；
/// - `10.0.0.5/32` → `[10.0.0.5]`；
/// - `/23` 及更宽 / 非法输入 → Err（防误扫大网段）。
pub fn expand_cidr(cidr: &str) -> Result<Vec<Ipv4Addr>, String> {
    let (base, prefix) = cidr
        .trim()
        .split_once('/')
        .ok_or_else(|| format!("CIDR 需形如 a.b.c.d/prefix：{cidr}"))?;
    let base: Ipv4Addr = base
        .trim()
        .parse()
        .map_err(|e| format!("网段地址非法（{base}）: {e}"))?;
    let prefix: u32 = prefix
        .trim()
        .parse()
        .map_err(|e| format!("前缀长度非法（{prefix}）: {e}"))?;
    if !(24..=32).contains(&prefix) {
        return Err(format!(
            "仅允许 /24 ~ /32（≤{SCAN_MAX_ADDRESSES} 地址），{cidr} 超出防误扫上限"
        ));
    }
    let count = 1u64 << (32 - prefix);
    let base_u = u32::from(base);
    let masked = if prefix == 0 {
        base_u
    } else {
        base_u & (u32::MAX << (32 - prefix))
    };
    Ok((0..count)
        .map(|i| Ipv4Addr::from(masked + i as u32))
        .collect())
}

// ----------------------------------------------------------------------------
// 纯函数：RMCP Presence Ping / Pong
// ----------------------------------------------------------------------------

/// 构造 RMCP Presence Ping 帧（12 字节，免凭据发现）。
///
/// 字节布局（与 ipmitool `lan.c` `ipmi_lan_ping` 一致）：
///
/// ```text
/// 06 00 FF 06   RMCP 头：ver=06(RMCP 1.0) res=00 seq=FF(无ACK) class=06(ASF)
/// 00 00 11 BE   ASF IANA 企业号 4542（网络序）
/// 80            ASF 类型 = Presence Ping
/// 00            tag（回显用）
/// 00 00         reserved / data length=0
/// ```
///
/// IPMI 2.0 规范 §13.5：RMCP+（IPMI 2.0）BMC **必须**应答此帧（Presence Pong），
/// 故可免凭据探测网段内的 BMC。
#[must_use]
pub fn rmcp_presence_ping(tag: u8) -> Vec<u8> {
    let mut f = Vec::with_capacity(12);
    f.push(RMCP_VERSION_1);
    f.push(0x00); // reserved
    f.push(0xFF); // sequence：无需 RMCP ACK
    f.push(RMCP_CLASS_ASF_PING);
    f.extend_from_slice(&ASF_IANA_ENTERPRISE.to_be_bytes()); // htonl(4542)
    f.push(ASF_TYPE_PING);
    f.push(tag);
    f.push(0x00); // reserved
    f.push(0x00); // data length
    f
}

/// 解析 RMCP Presence Pong 应答（不匹配返回 None）。
///
/// 帧布局（ipmitool `ipmi_handle_pong` 注释 / openbmc 实机抓包互证）：
///
/// ```text
/// [0]=06 ver  [1]=00  [2]=seq  [3]=class(06 回显或 07 规范 pong)
/// [4..8]  BMC 侧 IANA
/// [8]=0x40 ASF 类型 Presence Pong（个别实现 0x81，一并接受）
/// [9]=tag  [10]=00  [11]=data len（0x10=16）
/// data[0..4]  ASF 规范企业号（4542）
/// data[4..8]  OEM 自定义区
/// data[8]     支持的实体：bit7=IPMI、bit0=ASF 1.0
/// data[9]     支持的交互
/// ```
///
/// 注：IANA 字段在不同 BMC 上字节序不一致（ipmitool 发网络序、openbmc 回
/// 小端），此处两种字节序任一命中 4542 即归一，其余按网络序取值。
#[must_use]
pub fn parse_rmcp_pong(frame: &[u8]) -> Option<PongInfo> {
    if frame.len() < 12 {
        return None;
    }
    if frame[0] != RMCP_VERSION_1 {
        return None;
    }
    let class = frame[3];
    if class != RMCP_CLASS_ASF_PING && class != RMCP_CLASS_ASF_PONG && class != 0x00 {
        return None;
    }
    let msg_type = frame[8];
    if msg_type != ASF_TYPE_PONG && msg_type != 0x81 {
        return None; // ping 回显 / ACK / 无关 ASF 消息
    }
    let data_len = frame[11] as usize;
    if data_len < 10 {
        return None; // 不足以携带实体位
    }
    let data = frame.get(12..12 + data_len)?;
    let sup_entities = data[8];
    let ipmi_supported = sup_entities & 0x80 != 0;
    Some(PongInfo {
        message_class: class,
        asf_version: if sup_entities & 0x01 != 0 {
            "1.0".into()
        } else {
            "unknown".into()
        },
        ipmi_supported,
        rmcp_plus_supported: class == RMCP_CLASS_ASF_PONG || ipmi_supported,
        enterprise_iana: iana_guess(&data[0..4]),
        oem_iana: iana_guess(&data[4..8]),
        supported_interactions: data[9],
    })
}

/// IANA 字段解析（字节序兼容归一，见 [`parse_rmcp_pong`] 注释）。
fn iana_guess(b: &[u8]) -> u32 {
    if b.len() != 4 {
        return 0;
    }
    let be = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let le = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if be == ASF_IANA_ENTERPRISE || le == ASF_IANA_ENTERPRISE {
        ASF_IANA_ENTERPRISE
    } else {
        be
    }
}

// ----------------------------------------------------------------------------
// 纯函数：WoL 魔术包
// ----------------------------------------------------------------------------

/// 解析 MAC / SecureOn 密码字符串为 6 字节。
///
/// 接受 `aa:bb:cc:dd:ee:ff` / `AA-BB-CC-DD-EE-FF` / `aabbccddeeff`；
/// 非法（长度/十六进制）返回 None。
#[must_use]
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// MAC 规范化为小写冒号格式（输入必须已可解析）。
#[must_use]
pub fn normalize_mac(s: &str) -> Option<String> {
    parse_mac(s).map(|m| {
        m.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// 构造 WoL 魔术包：`FF×6 + MAC×16 [+ SecureOn 密码×1]`。
///
/// - 基础长度 102 字节（6 + 96）；追加 SecureOn 后 108 字节
///   （AMD/Realtek 的 MagicPacket + SecureON 扩展格式）。
#[must_use]
pub fn build_magic_packet(mac: &[u8; 6], secureon: Option<&[u8; 6]>) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(102 + secureon.map_or(0, |_| 6));
    pkt.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        pkt.extend_from_slice(mac);
    }
    if let Some(pw) = secureon {
        pkt.extend_from_slice(pw);
    }
    pkt
}

// ----------------------------------------------------------------------------
// 纯函数：ip neigh 解析
// ----------------------------------------------------------------------------

/// 解析 `ip neigh` 输出为邻居列表（无 lladdr 的行跳过）。
///
/// 行示例：`192.168.1.14 dev br0 lladdr b4:2e:99:aa:bb:cc REACHABLE`
#[must_use]
pub fn parse_ip_neigh(text: &str) -> Vec<ArpEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() || (toks[0].contains(':') && !toks[0].contains('.')) {
            continue; // IPv6 行（IP 含 ':' 且无 '.'）跳过
        }
        let ip = toks[0];
        if ip.parse::<std::net::IpAddr>().is_err() {
            continue;
        }
        let mut dev = String::new();
        let mut mac = String::new();
        let mut state = String::new();
        for (i, t) in toks.iter().enumerate() {
            match *t {
                "dev" => dev = toks.get(i + 1).unwrap_or(&"").to_string(),
                "lladdr" => mac = toks.get(i + 1).unwrap_or(&"").to_string(),
                _ => {}
            }
        }
        // 状态词在行尾（REACHABLE/STALE/...；含 "/" 的高阶态取整段）
        if let Some(last) = toks.last() {
            if last
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '/' || c == '_')
                && !last.is_empty()
            {
                state = last.to_string();
            }
        }
        if mac.is_empty() {
            continue; // FAILED 等无 MAC 行对选 MAC 无意义
        }
        out.push(ArpEntry {
            ip: ip.to_string(),
            mac: mac.to_ascii_lowercase(),
            dev,
            state,
        });
    }
    out
}

// ----------------------------------------------------------------------------
// 纯函数：PATH 探测（构造时一次性解析，测试可注入绝对路径）
// ----------------------------------------------------------------------------

/// 在 PATH 字符串中查找可执行文件（unix 额外校验可执行位）。
#[must_use]
pub fn find_in_path(bin: &str, path_value: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_value) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(bin);
        let meta = std::fs::metadata(&cand).ok()?;
        if meta.is_file() && is_executable(&meta) {
            return Some(cand);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

// ----------------------------------------------------------------------------
// 子进程执行辅助
// ----------------------------------------------------------------------------

/// 一次子进程执行的结果。
#[derive(Debug, Clone)]
struct ProcOutcome {
    exit_code: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl ProcOutcome {
    fn is_success(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }
}

/// 截断输出到 `cap` 字节（UTF-8 边界安全）。
fn truncate_output(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[截断，原 {} 字节]", &s[..end], s.len())
}

/// 带超时执行子进程（kill_on_drop 强杀；argv 直传不经 shell）。
async fn run_timed(mut cmd: tokio::process::Command, timeout: Duration) -> ProcOutcome {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => ProcOutcome {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: truncate_output(&String::from_utf8_lossy(&out.stdout), POWER_OUTPUT_CAP),
            stderr: truncate_output(&String::from_utf8_lossy(&out.stderr), POWER_OUTPUT_CAP),
            timed_out: false,
        },
        Ok(Err(e)) => ProcOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("spawn 失败: {e}"),
            timed_out: false,
        },
        Err(_) => ProcOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("超时（{}s）被终止", timeout.as_secs()),
            timed_out: true,
        },
    }
}

// ----------------------------------------------------------------------------
// 状态持久化（设备 + WoL 目标；原子写）
// ----------------------------------------------------------------------------

/// 持久化状态（JSON，`NEXOS_POWER_STATE`）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistState {
    #[serde(default)]
    devices: Vec<IpmiDevice>,
    #[serde(default)]
    wol_targets: Vec<WolTarget>,
}

/// 原子写 JSON（先写 `<path>.tmp` 再 rename；父目录自动创建）。
fn persist_state_to(path: &Path, st: &PersistState) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建状态目录失败 {dir:?}: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(st).map_err(|e| format!("状态序列化失败: {e}"))?;
    std::fs::write(&tmp, body).map_err(|e| format!("写临时状态失败 {tmp:?}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("原子替换状态失败 {path:?}: {e}"))
}

/// 读回 JSON 状态（缺失/解析失败 → 缺省，首次运行/文件损坏降级不报错）。
fn load_state_from(path: &Path) -> PersistState {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => PersistState::default(),
    }
}

// ----------------------------------------------------------------------------
// PowerRouteHandler
// ----------------------------------------------------------------------------

/// 电源控制层路由处理器——IPMI（本机/远程）+ RMCP+ 扫描 + WoL 魔术唤醒。
pub struct PowerRouteHandler {
    /// ipmitool 绝对路径（构造时 PATH 探测；None = 不可用，IPMI 域降级）。
    ipmitool: Option<PathBuf>,
    /// `ip` 命令绝对路径（`ip neigh`；None = ARP 端点降级）。
    ip_bin: Option<PathBuf>,
    /// 状态文件路径（设备 + WoL 目标）。
    state_path: PathBuf,
    devices: Mutex<Vec<IpmiDevice>>,
    wol_targets: Mutex<Vec<WolTarget>>,
    /// 扫描任务注册表（内存态；Arc 供后台任务回写）。
    scan_tasks: Arc<Mutex<HashMap<String, ScanTask>>>,
    counter: Mutex<u64>,
}

impl PowerRouteHandler {
    /// 生产构造：工具路径按 PATH 探测，状态文件取 env `NEXOS_POWER_STATE`
    /// （缺省 `/tank/os-data/power-state.json`）。
    #[must_use]
    pub fn new() -> Self {
        let state_path = std::env::var_os("NEXOS_POWER_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tank/os-data/power-state.json"));
        let path_var = std::env::var("PATH").unwrap_or_default();
        let ipmitool = find_in_path("ipmitool", &path_var);
        let ip_bin = find_in_path("ip", &path_var);
        Self::build(state_path, ipmitool, ip_bin)
    }

    /// 全参构造（测试注入工具绝对路径与状态文件；传 None 模拟工具缺失降级）。
    #[must_use]
    pub fn with_paths(
        state_path: PathBuf,
        ipmitool: Option<PathBuf>,
        ip_bin: Option<PathBuf>,
    ) -> Self {
        Self::build(state_path, ipmitool, ip_bin)
    }

    fn build(state_path: PathBuf, ipmitool: Option<PathBuf>, ip_bin: Option<PathBuf>) -> Self {
        let st = load_state_from(&state_path);
        let wol_targets = if st.wol_targets.is_empty() {
            demo_wol_targets()
        } else {
            st.wol_targets
        };
        Self {
            ipmitool,
            ip_bin,
            state_path,
            devices: Mutex::new(st.devices),
            wol_targets: Mutex::new(wol_targets),
            scan_tasks: Arc::new(Mutex::new(HashMap::new())),
            counter: Mutex::new(200),
        }
    }

    /// 当前设备列表快照（含密码，仅内部/测试用）。
    #[must_use]
    pub fn devices_snapshot(&self) -> Vec<IpmiDevice> {
        self.devices.lock().expect("devices poisoned").clone()
    }

    /// 当前 WoL 目标列表快照。
    #[must_use]
    pub fn wol_targets_snapshot(&self) -> Vec<WolTarget> {
        self.wol_targets
            .lock()
            .expect("wol_targets poisoned")
            .clone()
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 状态落盘（每次变更后调用；失败仅记 stderr，不阻塞业务）。
    fn persist(&self) {
        let st = PersistState {
            devices: self.devices.lock().expect("devices poisoned").clone(),
            wol_targets: self
                .wol_targets
                .lock()
                .expect("wol_targets poisoned")
                .clone(),
        };
        if let Err(e) = persist_state_to(&self.state_path, &st) {
            eprintln!("[power] 状态持久化失败: {e}");
        }
    }

    /// 设备响应脱敏：密码永不出 HTTP。
    fn device_masked(d: &IpmiDevice) -> serde_json::Value {
        let mut v = serde_json::to_value(d).unwrap_or_default();
        let has = d.password.as_deref().is_some_and(|p| !p.trim().is_empty());
        if let Some(o) = v.as_object_mut() {
            o.insert("password".into(), serde_json::Value::Null);
            o.insert("has_password".into(), serde_json::json!(has));
        }
        v
    }

    /// WoL 目标响应脱敏：SecureOn 密码不出 HTTP。
    fn wol_target_masked(t: &WolTarget) -> serde_json::Value {
        let mut v = serde_json::to_value(t).unwrap_or_default();
        let has = t
            .secureon_password
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());
        if let Some(o) = v.as_object_mut() {
            o.insert("secureon_password".into(), serde_json::Value::Null);
            o.insert("has_secureon".into(), serde_json::json!(has));
        }
        v
    }

    // ---- 本机 BMC（in-band）----

    /// 执行本机 ipmitool 子命令（工具缺失 → Err 附降级指引）。
    async fn run_local(&self, kind: IpmiCmd) -> Result<ProcOutcome, String> {
        let bin = self.ipmitool.as_ref().ok_or_else(|| {
            "未找到 ipmitool（PATH 探测失败）。安装：sudo apt install ipmitool\
             \n（LAN 魔术唤醒 WoL 不依赖 ipmitool，仍可使用）"
                .to_string()
        })?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(ipmi_local_argv(kind));
        Ok(run_timed(cmd, IPMI_TIMEOUT).await)
    }

    /// 聚合本机 BMC 状态（chassis + SEL + MC；命令失败 → 明确降级非 500）。
    async fn bmc_info(&self) -> BmcInfo {
        let ipmitool_found = self.ipmitool.is_some();
        let chassis_out = self.run_local(IpmiCmd::ChassisStatus).await;
        match chassis_out {
            Ok(out) if out.is_success() => {
                let chassis = parse_kv_lines(&out.stdout);
                let sel = self
                    .run_local(IpmiCmd::SelInfo)
                    .await
                    .ok()
                    .filter(|o| o.is_success())
                    .map(|o| parse_kv_lines(&o.stdout))
                    .unwrap_or_default();
                let mc = self
                    .run_local(IpmiCmd::McInfo)
                    .await
                    .ok()
                    .filter(|o| o.is_success())
                    .map(|o| parse_kv_lines(&o.stdout))
                    .unwrap_or_default();
                BmcInfo {
                    available: true,
                    ipmitool_found,
                    system_power: kv_get(&chassis, "System Power").map(str::to_string),
                    chassis,
                    sel,
                    mc,
                    hint: None,
                    error: None,
                }
            }
            Ok(out) => {
                // 命令失败：常见为本机无 /dev/ipmi0（ipmi_devintf 未加载）
                BmcInfo {
                    available: false,
                    ipmitool_found,
                    chassis: Vec::new(),
                    sel: Vec::new(),
                    mc: Vec::new(),
                    system_power: None,
                    hint: Some(
                        "ipmitool 已安装但本机 BMC 不可用（常见原因：未加载 IPMI 设备接口）。\
                         尝试：sudo modprobe ipmi_devintf ipmi_si"
                            .to_string(),
                    ),
                    error: Some(out.err_summary()),
                }
            }
            Err(hint) => BmcInfo {
                available: false,
                ipmitool_found,
                chassis: Vec::new(),
                sel: Vec::new(),
                mc: Vec::new(),
                system_power: None,
                hint: Some(hint),
                error: None,
            },
        }
    }

    /// 本机电源控制（on/off/cycle/soft）。
    async fn bmc_power(&self, action: &str) -> Result<PowerActionResult, (u16, String)> {
        let out = match self
            .run_local(IpmiCmd::ChassisPower(action.to_string()))
            .await
        {
            Ok(o) => o,
            Err(hint) => return Err((503, hint)),
        };
        Ok(PowerActionResult {
            ok: out.is_success(),
            action: action.to_string(),
            target: "bmc".into(),
            output: out.stdout.trim().to_string(),
            error: if out.is_success() {
                None
            } else {
                Some(out.err_summary())
            },
        })
    }

    // ---- 远程 IPMI 设备（RMCP+ / lanplus）----

    /// 执行远程 ipmitool 子命令（argv 直传防注入）。
    async fn run_remote(&self, dev: &IpmiDevice, kind: IpmiCmd) -> ProcOutcome {
        let bin = self
            .ipmitool
            .clone()
            .unwrap_or_else(|| PathBuf::from("ipmitool"));
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(ipmi_remote_argv(
            &dev.host,
            dev.port,
            &dev.username,
            dev.password.as_deref().unwrap_or(""),
            dev.cipher.as_deref(),
            kind,
        ));
        run_timed(cmd, IPMI_TIMEOUT).await
    }

    /// 远程设备连通性测试（chassis status 应答 → reachable + 电源态）。
    async fn device_test(&self, dev: &IpmiDevice) -> DeviceTestResult {
        let started = std::time::Instant::now();
        let out = self.run_remote(dev, IpmiCmd::ChassisStatus).await;
        let reachable = out.is_success();
        let chassis = if reachable {
            parse_kv_lines(&out.stdout)
        } else {
            Vec::new()
        };
        DeviceTestResult {
            reachable,
            system_power: kv_get(&chassis, "System Power").map(str::to_string),
            chassis,
            output: if reachable {
                out.stdout.trim().to_string()
            } else {
                out.err_summary()
            },
            duration_ms: started.elapsed().as_millis() as u64,
        }
    }

    // ---- 网段扫描（RMCP Presence Ping，纯 Rust UDP）----

    /// 发起一次扫描（后台任务体）：分批发 ping，按 deadline 收 pong。
    async fn scan_run(
        tasks: Arc<Mutex<HashMap<String, ScanTask>>>,
        id: String,
        addrs: Vec<Ipv4Addr>,
        port: u16,
        timeout_ms: u64,
        concurrency: usize,
    ) {
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                Self::patch_scan(&tasks, &id, |t| {
                    t.status = "failed".into();
                    t.error = Some(format!("UDP 绑定失败: {e}"));
                    t.finished_at = Some(now_iso());
                });
                return;
            }
        };
        let ping = rmcp_presence_ping(0);
        let per_host = Duration::from_millis(timeout_ms.max(10));
        let mut scanned = 0usize;
        for batch in addrs.chunks(concurrency.max(1)) {
            for addr in batch {
                let peer = SocketAddr::new((*addr).into(), port);
                let _ = sock.send_to(&ping, peer).await;
            }
            scanned += batch.len();
            let n = scanned;
            Self::patch_scan(&tasks, &id, |t| t.scanned = n);
            let deadline = std::time::Instant::now() + per_host;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; 256];
                match tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await {
                    Ok(Ok((n, peer))) => {
                        let Some(pong) = parse_rmcp_pong(&buf[..n]) else {
                            continue;
                        };
                        let ip = peer.ip().to_string();
                        Self::patch_scan(&tasks, &id, |t| {
                            // 同 IP 去重（BMC 可能重复应答/多网卡回环）
                            if !t.found.iter().any(|h| h.ip == ip) {
                                t.found.push(ScanHit {
                                    ip: ip.clone(),
                                    pong,
                                });
                            }
                        });
                    }
                    Ok(Err(_)) | Err(_) => break,
                }
            }
        }
        Self::patch_scan(&tasks, &id, |t| {
            t.status = "completed".into();
            t.finished_at = Some(now_iso());
        });
    }

    /// 就地更新一个扫描任务。
    fn patch_scan(
        tasks: &Arc<Mutex<HashMap<String, ScanTask>>>,
        id: &str,
        f: impl FnOnce(&mut ScanTask),
    ) {
        let mut guard = tasks.lock().expect("scan_tasks poisoned");
        if let Some(t) = guard.get_mut(id) {
            f(t);
        }
    }

    // ---- WoL ----

    /// 发送魔术包（`attempts` 次、间隔 [`WOL_SEND_INTERVAL`]）。
    /// 返回实际成功发出的 UDP 包数。
    async fn send_wol(broadcast: &str, port: u16, packet: &[u8]) -> Result<usize, String> {
        let sock = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("UDP 绑定失败: {e}"))?;
        sock.set_broadcast(true)
            .map_err(|e| format!("开启广播失败: {e}"))?;
        let peer: SocketAddr = format!("{broadcast}:{port}")
            .parse()
            .map_err(|e| format!("广播地址非法（{broadcast}:{port}）: {e}"))?;
        let mut sent = 0usize;
        for i in 0..WOL_SEND_ATTEMPTS {
            if i > 0 {
                tokio::time::sleep(WOL_SEND_INTERVAL).await;
            }
            match sock.send_to(packet, peer).await {
                Ok(_) => sent += 1,
                Err(e) if i == WOL_SEND_ATTEMPTS - 1 && sent == 0 => {
                    return Err(format!("发送失败（{broadcast}:{port}）: {e}"));
                }
                Err(_) => {}
            }
        }
        Ok(sent)
    }
}

impl Default for PowerRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcOutcome {
    /// 失败摘要（超时/退出码/stderr）。
    fn err_summary(&self) -> String {
        if self.timed_out {
            return format!("超时被终止（{}）", self.stderr);
        }
        if self.stderr.trim().is_empty() {
            format!("exit_code={}", self.exit_code)
        } else {
            format!("exit_code={}: {}", self.exit_code, self.stderr.trim())
        }
    }
}

// ----------------------------------------------------------------------------
// 请求体
// ----------------------------------------------------------------------------

/// 电源控制请求体（本机与远程共用）。
#[derive(Debug, Deserialize)]
struct PowerActionBody {
    action: String,
}

/// 注册远程 IPMI 设备请求体。
#[derive(Debug, Deserialize)]
struct CreateDeviceBody {
    name: String,
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    cipher: Option<String>,
}

/// 发起扫描请求体。
#[derive(Debug, Deserialize)]
struct CreateScanBody {
    cidr: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    concurrency: Option<usize>,
}

/// 注册 WoL 目标请求体。
#[derive(Debug, Deserialize)]
struct CreateWolTargetBody {
    name: String,
    mac: String,
    #[serde(default)]
    broadcast: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    secureon_password: Option<String>,
}

/// 唤醒请求体（name 或 mac 二选一；broadcast/port 可临时覆盖）。
#[derive(Debug, Deserialize)]
struct WakeBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mac: Option<String>,
    #[serde(default)]
    broadcast: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

// ----------------------------------------------------------------------------
// RouteHandler 实现
// ----------------------------------------------------------------------------

#[async_trait]
impl RouteHandler for PowerRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 本机 BMC ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/bmc",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/bmc/power",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/bmc/sensors",
                false,
                vec![],
            ),
            // —— 远程 IPMI 设备 ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/ipmi/devices",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/ipmi/devices",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/provisioning/power/ipmi/devices/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/ipmi/devices/:id/test",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/ipmi/devices/:id/power",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/ipmi/devices/:id/status",
                false,
                vec![],
            ),
            // —— 网段扫描 ——
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/ipmi/scan",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/ipmi/scan/:id",
                false,
                vec![],
            ),
            // —— WoL ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/wol/targets",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/wol/targets",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/provisioning/power/wol/targets/:id",
                true,
                vec!["admin".into()],
            ),
            // 魔术包本身无凭据；开发期公开（生产收紧策略见文档硬化清单）
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/power/wol/wake",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/power/wol/arp",
                false,
                vec![],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // ==================== 本机 BMC ====================
            // —— GET /power/bmc —— chassis + SEL + MC 聚合（不可用明确降级）——
            (HttpMethod::Get, ["api", "v1", "provisioning", "power", "bmc"]) => {
                let info = self.bmc_info().await;
                Ok(ok_json(to_value(&info)?))
            }

            // —— POST /power/bmc/power {action} ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "power", "bmc", "power"]) => {
                let body: PowerActionBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析电源控制请求体失败: {e}"))
                })?;
                let action = body.action.trim().to_lowercase();
                if !POWER_ACTIONS.contains(&action.as_str()) {
                    return Ok(error_response(
                        400,
                        &format!("action 必须是 {} 之一", POWER_ACTIONS.join("/")),
                    ));
                }
                match self.bmc_power(&action).await {
                    Ok(r) => Ok(ok_json(to_value(&r)?)),
                    Err((status, msg)) => Ok(error_response(status, &msg)),
                }
            }

            // —— GET /power/bmc/sensors —— 传感器表（截 200 行）——
            (HttpMethod::Get, ["api", "v1", "provisioning", "power", "bmc", "sensors"]) => {
                match self.run_local(IpmiCmd::SensorList).await {
                    Ok(out) if out.is_success() => {
                        let (rows, truncated) = parse_sensor_list(&out.stdout, SENSOR_ROWS_MAX);
                        let info = SensorsInfo {
                            available: true,
                            count: rows.len(),
                            truncated,
                            rows,
                            hint: None,
                        };
                        Ok(ok_json(to_value(&info)?))
                    }
                    Ok(out) => {
                        let info = SensorsInfo {
                            available: false,
                            count: 0,
                            truncated: false,
                            rows: Vec::new(),
                            hint: Some(format!(
                                "传感器读取失败: {}（本机 BMC 不可用时尝试 sudo modprobe ipmi_devintf ipmi_si）",
                                out.err_summary()
                            )),
                        };
                        Ok(ok_json(to_value(&info)?))
                    }
                    Err(hint) => {
                        let info = SensorsInfo {
                            available: false,
                            count: 0,
                            truncated: false,
                            rows: Vec::new(),
                            hint: Some(hint),
                        };
                        Ok(ok_json(to_value(&info)?))
                    }
                }
            }

            // ==================== 远程 IPMI 设备 ====================
            // —— GET /power/ipmi/devices ——（密码脱敏）——
            (HttpMethod::Get, ["api", "v1", "provisioning", "power", "ipmi", "devices"]) => {
                let list: Vec<serde_json::Value> = self
                    .devices
                    .lock()
                    .expect("devices poisoned")
                    .iter()
                    .map(Self::device_masked)
                    .collect();
                Ok(ok_json(serde_json::Value::Array(list)))
            }

            // —— POST /power/ipmi/devices —— 注册 ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "power", "ipmi", "devices"]) => {
                let body: CreateDeviceBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析设备请求体失败: {e}")))?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                let host = body.host.trim().to_string();
                if host.is_empty() || host.split_whitespace().count() > 1 {
                    return Ok(error_response(400, "host 不可为空且不能含空白"));
                }
                let port = body.port.unwrap_or(RMCP_PORT_DEFAULT);
                if port == 0 {
                    return Ok(error_response(400, "port 必须在 1-65535"));
                }
                let dev = IpmiDevice {
                    id: self.next_id("ipmi"),
                    name: body.name.trim().to_string(),
                    host,
                    port,
                    username: body
                        .username
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "admin".into()),
                    password: body
                        .password
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    cipher: body
                        .cipher
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    status: "unknown".into(),
                    last_checked: None,
                    created_at: now_iso(),
                };
                let resp = Self::device_masked(&dev);
                self.devices.lock().expect("devices poisoned").push(dev);
                self.persist();
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /power/ipmi/devices/:id ——
            (HttpMethod::Delete, ["api", "v1", "provisioning", "power", "ipmi", "devices", id]) => {
                let mut devs = self.devices.lock().expect("devices poisoned");
                let before = devs.len();
                devs.retain(|d| d.id != *id);
                if devs.len() == before {
                    return Ok(error_response(404, &format!("设备不存在: {id}")));
                }
                drop(devs);
                self.persist();
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /power/ipmi/devices/:id/test —— 真实 lanplus 连通性 ——
            (
                HttpMethod::Post,
                ["api", "v1", "provisioning", "power", "ipmi", "devices", id, "test"],
            ) => {
                let dev = {
                    let devs = self.devices.lock().expect("devices poisoned");
                    match devs.iter().find(|d| d.id == *id) {
                        Some(d) => d.clone(),
                        None => return Ok(error_response(404, &format!("设备不存在: {id}"))),
                    }
                };
                let result = self.device_test(&dev).await;
                let reachable = if result.reachable {
                    "reachable"
                } else {
                    "unreachable"
                };
                {
                    let mut devs = self.devices.lock().expect("devices poisoned");
                    if let Some(d) = devs.iter_mut().find(|d| d.id == dev.id) {
                        d.status = reachable.into();
                        d.last_checked = Some(now_iso());
                    }
                }
                self.persist();
                Ok(ok_json(to_value(&result)?))
            }

            // —— POST /power/ipmi/devices/:id/power {action} —— 远程电源控制 ——
            (
                HttpMethod::Post,
                ["api", "v1", "provisioning", "power", "ipmi", "devices", id, "power"],
            ) => {
                let dev = {
                    let devs = self.devices.lock().expect("devices poisoned");
                    match devs.iter().find(|d| d.id == *id) {
                        Some(d) => d.clone(),
                        None => return Ok(error_response(404, &format!("设备不存在: {id}"))),
                    }
                };
                let body: PowerActionBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析电源控制请求体失败: {e}"))
                })?;
                let action = body.action.trim().to_lowercase();
                if !POWER_ACTIONS.contains(&action.as_str()) {
                    return Ok(error_response(
                        400,
                        &format!("action 必须是 {} 之一", POWER_ACTIONS.join("/")),
                    ));
                }
                if self.ipmitool.is_none() {
                    return Ok(error_response(
                        503,
                        "未找到 ipmitool（PATH 探测失败）。安装：sudo apt install ipmitool",
                    ));
                }
                let out = self
                    .run_remote(&dev, IpmiCmd::ChassisPower(action.clone()))
                    .await;
                Ok(ok_json(to_value(&PowerActionResult {
                    ok: out.is_success(),
                    action: action.clone(),
                    target: format!("{}:{}", dev.host, dev.port),
                    output: out.stdout.trim().to_string(),
                    error: if out.is_success() {
                        None
                    } else {
                        Some(out.err_summary())
                    },
                })?))
            }

            // —— GET /power/ipmi/devices/:id/status —— 实时 chassis status ——
            (
                HttpMethod::Get,
                ["api", "v1", "provisioning", "power", "ipmi", "devices", id, "status"],
            ) => {
                let dev = {
                    let devs = self.devices.lock().expect("devices poisoned");
                    match devs.iter().find(|d| d.id == *id) {
                        Some(d) => d.clone(),
                        None => return Ok(error_response(404, &format!("设备不存在: {id}"))),
                    }
                };
                if self.ipmitool.is_none() {
                    return Ok(error_response(
                        503,
                        "未找到 ipmitool（PATH 探测失败）。安装：sudo apt install ipmitool",
                    ));
                }
                let out = self.run_remote(&dev, IpmiCmd::ChassisStatus).await;
                let reachable = out.is_success();
                let chassis = if reachable {
                    parse_kv_lines(&out.stdout)
                } else {
                    Vec::new()
                };
                {
                    let mut devs = self.devices.lock().expect("devices poisoned");
                    if let Some(d) = devs.iter_mut().find(|d| d.id == dev.id) {
                        d.status = if reachable {
                            "reachable"
                        } else {
                            "unreachable"
                        }
                        .into();
                        d.last_checked = Some(now_iso());
                    }
                }
                self.persist();
                Ok(ok_json(to_value(&DeviceStatusInfo {
                    reachable,
                    system_power: kv_get(&chassis, "System Power").map(str::to_string),
                    identify: kv_get(&chassis, "Identify Supported").map(str::to_string),
                    chassis,
                    checked_at: now_iso(),
                    error: if reachable {
                        None
                    } else {
                        Some(out.err_summary())
                    },
                })?))
            }

            // ==================== 网段扫描 ====================
            // —— POST /power/ipmi/scan ——（admin，后台任务）——
            (HttpMethod::Post, ["api", "v1", "provisioning", "power", "ipmi", "scan"]) => {
                let body: CreateScanBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析扫描请求体失败: {e}")))?;
                let addrs = match expand_cidr(&body.cidr) {
                    Ok(a) => a,
                    Err(e) => return Ok(error_response(400, &e)),
                };
                let port = body.port.unwrap_or(RMCP_PORT_DEFAULT);
                let timeout_ms = body.timeout_ms.unwrap_or(500).clamp(50, 5_000);
                let concurrency = body
                    .concurrency
                    .unwrap_or(SCAN_CONCURRENCY_DEFAULT)
                    .clamp(1, SCAN_MAX_ADDRESSES);
                let task = ScanTask {
                    id: self.next_id("scan"),
                    cidr: body.cidr.trim().to_string(),
                    port,
                    status: "running".into(),
                    scanned: 0,
                    total: addrs.len(),
                    found: Vec::new(),
                    started_at: now_iso(),
                    finished_at: None,
                    error: None,
                };
                let id = task.id.clone();
                {
                    let mut tasks = self.scan_tasks.lock().expect("scan_tasks poisoned");
                    // 上限淘汰：丢最旧（按 started_at 字符串序即可近似）
                    if tasks.len() >= SCAN_TASKS_MAX {
                        if let Some(oldest) = tasks
                            .values()
                            .min_by_key(|t| t.started_at.clone())
                            .map(|t| t.id.clone())
                        {
                            tasks.remove(&oldest);
                        }
                    }
                    tasks.insert(id.clone(), task);
                }
                let registry = self.scan_tasks.clone();
                let run_id = id.clone();
                tokio::spawn(async move {
                    Self::scan_run(registry, run_id, addrs, port, timeout_ms, concurrency).await;
                });
                let snapshot = self
                    .scan_tasks
                    .lock()
                    .expect("scan_tasks poisoned")
                    .get(&id)
                    .cloned();
                match snapshot {
                    Some(t) => Ok(ApiResponse {
                        status: 202,
                        body: to_value(&t)?,
                        headers: serde_json::json!({}),
                    }),
                    None => Ok(error_response(500, "扫描任务创建异常")),
                }
            }

            // —— GET /power/ipmi/scan/:id —— 任务状态+结果 ——
            (HttpMethod::Get, ["api", "v1", "provisioning", "power", "ipmi", "scan", id]) => {
                let t = self
                    .scan_tasks
                    .lock()
                    .expect("scan_tasks poisoned")
                    .get(*id)
                    .cloned();
                match t {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("扫描任务不存在: {id}"))),
                }
            }

            // ==================== WoL ====================
            // —— GET /power/wol/targets ——（SecureOn 脱敏）——
            (HttpMethod::Get, ["api", "v1", "provisioning", "power", "wol", "targets"]) => {
                let list: Vec<serde_json::Value> = self
                    .wol_targets
                    .lock()
                    .expect("wol_targets poisoned")
                    .iter()
                    .map(Self::wol_target_masked)
                    .collect();
                Ok(ok_json(serde_json::Value::Array(list)))
            }

            // —— POST /power/wol/targets —— 注册目标 ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "power", "wol", "targets"]) => {
                let body: CreateWolTargetBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 WoL 目标请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                let Some(mac) = normalize_mac(&body.mac) else {
                    return Ok(error_response(
                        400,
                        "MAC 非法：需形如 aa:bb:cc:dd:ee:ff（或 - 分隔 / 12 位十六进制）",
                    ));
                };
                let secureon = match body.secureon_password.as_deref() {
                    None | Some("") => None,
                    Some(pw) => match normalize_mac(pw) {
                        Some(s) => Some(s),
                        None => {
                            return Ok(error_response(
                                400,
                                "SecureOn 密码非法：需与 MAC 同格式（6 字节十六进制）",
                            ));
                        }
                    },
                };
                let broadcast = body
                    .broadcast
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| WOL_BROADCAST_DEFAULT.to_string());
                if broadcast.parse::<std::net::IpAddr>().is_err() {
                    return Ok(error_response(400, "broadcast 必须是合法 IP 地址"));
                }
                let port = body.port.unwrap_or(WOL_PORT_DEFAULT);
                let t = WolTarget {
                    id: self.next_id("wol"),
                    name: body.name.trim().to_string(),
                    mac,
                    broadcast,
                    port,
                    secureon_password: secureon,
                    created_at: now_iso(),
                };
                let resp = Self::wol_target_masked(&t);
                self.wol_targets
                    .lock()
                    .expect("wol_targets poisoned")
                    .push(t);
                self.persist();
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /power/wol/targets/:id ——
            (HttpMethod::Delete, ["api", "v1", "provisioning", "power", "wol", "targets", id]) => {
                let mut list = self.wol_targets.lock().expect("wol_targets poisoned");
                let before = list.len();
                list.retain(|t| t.id != *id);
                if list.len() == before {
                    return Ok(error_response(404, &format!("WoL 目标不存在: {id}")));
                }
                drop(list);
                self.persist();
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /power/wol/wake {name|mac} —— 发送魔术包 ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "power", "wol", "wake"]) => {
                let body: WakeBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析唤醒请求体失败: {e}")))?;
                // 目标解析：注册名 → 注册表项（带 SecureOn）；裸 MAC → 临时目标
                let (mac, broadcast, port, secureon, label) = if let Some(name) = body
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let found = {
                        let list = self.wol_targets.lock().expect("wol_targets poisoned");
                        list.iter().find(|t| t.name == name).cloned()
                    };
                    let Some(t) = found else {
                        return Ok(error_response(404, &format!("WoL 目标不存在: {name}")));
                    };
                    (
                        t.mac,
                        body.broadcast.unwrap_or(t.broadcast),
                        body.port.unwrap_or(t.port),
                        t.secureon_password,
                        t.name,
                    )
                } else if let Some(mac) =
                    body.mac.as_deref().map(str::trim).filter(|s| !s.is_empty())
                {
                    let Some(m) = normalize_mac(mac) else {
                        return Ok(error_response(400, "MAC 非法：需形如 aa:bb:cc:dd:ee:ff"));
                    };
                    (
                        m,
                        body.broadcast
                            .unwrap_or_else(|| WOL_BROADCAST_DEFAULT.to_string()),
                        body.port.unwrap_or(WOL_PORT_DEFAULT),
                        None,
                        format!("mac:{mac}"),
                    )
                } else {
                    return Ok(error_response(400, "需提供 name（已注册目标）或 mac"));
                };
                let Some(mac_bytes) = parse_mac(&mac) else {
                    return Ok(error_response(400, "MAC 解析失败"));
                };
                let secureon_bytes = secureon.as_deref().and_then(parse_mac);
                let packet = build_magic_packet(&mac_bytes, secureon_bytes.as_ref());
                let bytes = packet.len();
                let result = Self::send_wol(&broadcast, port, &packet).await;
                Ok(ok_json(to_value(&WakeResult {
                    ok: result.is_ok(),
                    target: label,
                    mac,
                    broadcast: broadcast.clone(),
                    port,
                    attempts: WOL_SEND_ATTEMPTS,
                    sent: result.as_ref().copied().unwrap_or(0),
                    bytes_per_packet: bytes,
                    secureon: secureon_bytes.is_some(),
                    error: result.err(),
                })?))
            }

            // —— GET /power/wol/arp —— 局域网邻居（ip neigh）——
            (HttpMethod::Get, ["api", "v1", "provisioning", "power", "wol", "arp"]) => {
                let Some(ip) = self.ip_bin.clone() else {
                    return Ok(ok_json(to_value(&ArpInfo {
                        available: false,
                        neighbors: Vec::new(),
                        hint: Some("未找到 ip 命令（iproute2）。可用 MAC 请手工填写".into()),
                    })?));
                };
                let mut cmd = tokio::process::Command::new(ip);
                cmd.args(["neigh"]);
                let out = run_timed(cmd, IP_NEIGH_TIMEOUT).await;
                if out.is_success() {
                    Ok(ok_json(to_value(&ArpInfo {
                        available: true,
                        neighbors: parse_ip_neigh(&out.stdout),
                        hint: Some("局域网邻居（ip neigh）——选中自动填 MAC".into()),
                    })?))
                } else {
                    Ok(ok_json(to_value(&ArpInfo {
                        available: false,
                        neighbors: Vec::new(),
                        hint: Some(format!("ip neigh 执行失败: {}", out.err_summary())),
                    })?))
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "power: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 构造一条 RouteSpec（component 固定 `power`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "power".to_string(),
        requires_auth,
        required_roles,
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 把可序列化结果转成 Value，序列化失败统一映射为 Internal。
fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 从请求路径剥离 `?query` 后的纯 path 段。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 当前 ISO 时间戳（本地时区）。
fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// 预置示例 WoL 目标（首次 GET 非空；无状态文件时注入，不预置 IPMI 设备——
/// 设备带密码不该有演示假数据）。
fn demo_wol_targets() -> Vec<WolTarget> {
    vec![WolTarget {
        id: "wol-1".into(),
        name: "示例目标（改我）".into(),
        mac: "aa:bb:cc:dd:ee:ff".into(),
        broadcast: WOL_BROADCAST_DEFAULT.into(),
        port: WOL_PORT_DEFAULT,
        secureon_password: None,
        created_at: "2026-08-25T08:00:00+08:00".into(),
    }]
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("os-api-power-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一个可执行假脚本（#!/bin/sh + body）。
    fn fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// 测试 handler：注入状态文件 + 工具路径（None 模拟缺失降级）。
    fn handler_with(dir: &Path, ipmitool: Option<&Path>, ip: Option<&Path>) -> PowerRouteHandler {
        PowerRouteHandler::with_paths(
            dir.join("power-state.json"),
            ipmitool.map(PathBuf::from),
            ip.map(PathBuf::from),
        )
    }

    /// 无工具 handler（IPMI 域降级、WoL 域可用）。
    fn degraded_handler(dir: &Path) -> PowerRouteHandler {
        handler_with(dir, None, None)
    }

    /// 假 ipmitool：记录 argv 到 $ARGV_LOG，按子命令回放真实格式输出。
    fn fake_ipmitool(dir: &Path) -> PathBuf {
        let log = dir.join("argv.log");
        let log_str = log.display().to_string().replace('"', "\\\"");
        let body = format!(
            r#"
LOG="{log_str}"
echo "$@" >> "$LOG"
case "$*" in
  *"chassis status"*)
    printf 'System Power         : on\n'
    printf 'Power Overload       : false\n'
    printf 'Power Fault          : false\n'
    printf 'Identify Supported   : yes\n'
    exit 0;;
  *"chassis power"*)
    echo "Chassis Power Control: Up/On"
    exit 0;;
  *"sel info"*)
    printf 'Version       : 1.5 (v1.5, v2 compliant)\n'
    printf 'Entries       : 12\n'
    printf 'Percent Used  : 3\n'
    exit 0;;
  *"mc info"*)
    printf 'Device ID         : 32\n'
    printf 'Firmware Revision : 1.73\n'
    printf 'Manufacturer ID   : 4771\n'
    exit 0;;
  *"sensor list"*)
    printf 'Ambient Temp | Temperature | 26 (+/- 0) degrees C | ok    | 7.1\n'
    printf 'CPU Fan      | Fan        | 2400 RPM             | ok    | 29.1\n'
    exit 0;;
esac
exit 1
"#
        );
        fake_bin(dir, "ipmitool", &body)
    }

    // ============ WoL 魔术包（字节级）============

    #[test]
    fn magic_packet_layout() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let pkt = build_magic_packet(&mac, None);
        assert_eq!(pkt.len(), 102, "6×FF + 16×6 = 102 字节");
        assert_eq!(&pkt[..6], &[0xFF; 6], "前导 6 字节全 FF");
        for i in 0..16 {
            let seg = &pkt[6 + i * 6..6 + (i + 1) * 6];
            assert_eq!(seg, &mac, "第 {} 段 MAC 重复", i + 1);
        }
    }

    #[test]
    fn magic_packet_with_secureon_appends_password() {
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let pw = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        let pkt = build_magic_packet(&mac, Some(&pw));
        assert_eq!(pkt.len(), 108, "102 + SecureOn 6 字节");
        assert_eq!(&pkt[102..], &pw, "尾部 6 字节为 SecureOn 密码");
    }

    #[test]
    fn parse_mac_accepts_common_formats() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );
        assert_eq!(
            parse_mac("AA-BB-CC-DD-EE-FF"),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );
        assert_eq!(
            parse_mac("  aabbccddeeff "),
            Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
        );
        assert_eq!(
            normalize_mac("AA:BB:CC:DD:EE:FF").as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );
        // 非法：长度不足 / 非十六进制
        assert_eq!(parse_mac("aa:bb:cc:dd:ee"), None);
        assert_eq!(parse_mac("zz:bb:cc:dd:ee:ff"), None);
    }

    // ============ RMCP Presence Ping / Pong ============

    #[test]
    fn rmcp_ping_frame_exact_bytes() {
        // ipmitool `lan.c` ipmi_lan_ping 同款 12 字节帧（openbmc 实机互证）
        let f = rmcp_presence_ping(0);
        assert_eq!(
            f,
            vec![0x06, 0x00, 0xFF, 0x06, 0x00, 0x00, 0x11, 0xBE, 0x80, 0x00, 0x00, 0x00]
        );
        let tagged = rmcp_presence_ping(0x5A);
        assert_eq!(tagged[9], 0x5A, "tag 回显位");
    }

    /// openbmc phosphor-net-ipmid 实机应答（28 字节）。
    const OPENBMC_PONG: [u8; 28] = [
        0x06, 0x00, 0xFF, 0x06, 0xBE, 0x11, 0x00, 0x00, 0x40, 0x00, 0x00, 0x10, 0xBE, 0x11, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x81, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn parse_rmcp_pong_openbmc_sample() {
        let p = parse_rmcp_pong(&OPENBMC_PONG).expect("openbmc pong 应可解析");
        assert!(p.ipmi_supported, "data[8]=0x81 bit7 → IPMI 实体");
        assert_eq!(p.asf_version, "1.0", "data[8] bit0 → ASF 1.0");
        assert!(p.rmcp_plus_supported, "IPMI 实体位 → 判 RMCP+（IPMI 2.0）");
        assert_eq!(p.enterprise_iana, 4542, "小端 4542 归一为 ASF 企业号");
        assert_eq!(p.oem_iana, 0);
        assert_eq!(p.message_class, 0x06, "openbmc 回显 ping 类 06");
    }

    #[test]
    fn parse_rmcp_pong_class07_and_network_order() {
        // IPMI 2.0 规范 pong 类（07）+ 网络序 IANA 的合成帧
        let mut f = vec![0x06, 0x00, 0xFF, 0x07];
        f.extend_from_slice(&4542u32.to_be_bytes()); // BMC 侧 IANA
        f.extend_from_slice(&[0x40, 0x00, 0x00, 0x10]); // PONG / tag / res / len
        f.extend_from_slice(&4542u32.to_be_bytes()); // data[0..4]: ASF IANA（网络序）
        f.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // data[4..8]: OEM 区
        f.push(0x80); // data[8]: 实体位（仅 IPMI，无 ASF 1.0 位）
        f.push(0x00); // data[9]: 交互
        f.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // data[10..16] 保留
        assert_eq!(f.len(), 28, "pong 全帧 4+4+4+16=28 字节");
        let p = parse_rmcp_pong(&f).expect("规范 class-07 pong 应可解析");
        assert_eq!(p.message_class, 0x07);
        assert!(p.rmcp_plus_supported);
        assert!(p.ipmi_supported);
        assert_eq!(p.asf_version, "unknown");
        assert_eq!(p.enterprise_iana, 4542);
        assert_eq!(p.supported_interactions, 0);
    }

    #[test]
    fn parse_rmcp_pong_rejects_garbage() {
        assert!(parse_rmcp_pong(&[]).is_none(), "空帧");
        assert!(parse_rmcp_pong(&[0x06; 5]).is_none(), "过短");
        // ping 回显（type=0x80）不是 pong
        let echo = rmcp_presence_ping(0);
        assert!(parse_rmcp_pong(&echo).is_none());
        // 版本错
        let mut bad = OPENBMC_PONG;
        bad[0] = 0x00;
        assert!(parse_rmcp_pong(&bad).is_none());
        // 类型错（0x00 非 pong）
        let mut bad_type = OPENBMC_PONG;
        bad_type[8] = 0x00;
        assert!(parse_rmcp_pong(&bad_type).is_none());
    }

    // ============ CIDR 展开（/24 上限）============

    #[test]
    fn expand_cidr_bounds() {
        let a = expand_cidr("192.0.2.0/24").unwrap();
        assert_eq!(a.len(), 256, "/24 = 256 地址");
        assert_eq!(a[0].to_string(), "192.0.2.0");
        assert_eq!(a[255].to_string(), "192.0.2.255");

        let a = expand_cidr("192.168.1.77/32").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].to_string(), "192.168.1.77");

        // 超上限：/23（512）、/16（65536）拒绝
        assert!(expand_cidr("10.0.0.0/23").is_err());
        assert!(expand_cidr("10.0.0.0/16").is_err());
        // 非法输入
        assert!(expand_cidr("10.0.0.0").is_err());
        assert!(expand_cidr("abc/24").is_err());
        assert!(expand_cidr("10.0.0.0/three").is_err());
        // 前缀低于 24 的错误文案带防误扫说明
        let err = expand_cidr("10.0.0.0/8").unwrap_err();
        assert!(err.contains("防误扫"), "错误说明: {err}");
    }

    // ============ ipmitool 输出解析 ============

    #[test]
    fn parse_kv_lines_and_system_power() {
        let text = "\
System Power         : on
Power Overload       : false
Power Fault          : false

no separator line
Identify Supported   : yes";
        let kvs = parse_kv_lines(text);
        assert_eq!(kvs.len(), 4, "无分隔符行跳过");
        assert_eq!(kv_get(&kvs, "System Power"), Some("on"));
        assert_eq!(kv_get(&kvs, "system power"), Some("on"), "忽略大小写");
        assert_eq!(kv_get(&kvs, "Power Fault"), Some("false"));
        assert_eq!(kv_get(&kvs, "Not There"), None);
    }

    #[test]
    fn parse_sensor_list_truncates_at_cap() {
        let sample = "\
Ambient Temp | Temperature | 26 (+/- 0) degrees C | ok    | 7.1
CPU Fan      | Fan        | 2400 RPM             | ok    | 29.1
PSU Voltage  | Voltage    | 12.10 Volts          | nc    | 10.1";
        let (rows, truncated) = parse_sensor_list(sample, 200);
        assert!(!truncated);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "Ambient Temp");
        assert_eq!(rows[0].sensor_type, "Temperature");
        assert_eq!(rows[0].reading, "26 (+/- 0) degrees C");
        assert_eq!(rows[0].status, "ok");
        assert_eq!(rows[2].status, "nc");

        // 超上限截断（250 行 → 200 + truncated）
        let mut big = String::new();
        for i in 0..250 {
            big.push_str(&format!("S{i} | T | {i} | ok | 1.1\n"));
        }
        let (rows2, truncated2) = parse_sensor_list(&big, SENSOR_ROWS_MAX);
        assert_eq!(rows2.len(), 200, "截到 SENSOR_ROWS_MAX");
        assert!(truncated2);
        assert_eq!(rows2.last().unwrap().name, "S199");
    }

    #[test]
    fn parse_ip_neigh_entries() {
        let sample = "\
192.168.1.14 dev br0 lladdr B4:2E:99:AA:BB:CC REACHABLE
192.168.1.20 dev br0 lladdr aa:bb:cc:dd:ee:ff STALE
192.168.1.31 dev br0  FAILED
fe80::1 dev br0 lladdr aa:bb:cc:dd:ee:ff REACHABLE";
        let ns = parse_ip_neigh(sample);
        assert_eq!(ns.len(), 2, "无 lladdr 的 FAILED 行与 IPv6 行跳过");
        assert_eq!(ns[0].ip, "192.168.1.14");
        assert_eq!(ns[0].mac, "b4:2e:99:aa:bb:cc", "MAC 归一小写");
        assert_eq!(ns[0].dev, "br0");
        assert_eq!(ns[0].state, "REACHABLE");
        assert_eq!(ns[1].state, "STALE");
    }

    // ============ PATH 探测 ============

    #[test]
    fn find_in_path_requires_executable() {
        let dir = temp_dir("findpath");
        let exe = fake_bin(&dir, "ipmitool", "exit 0");
        // 可执行位被去掉 → 探测不到
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&exe, perms).unwrap();
        assert!(find_in_path("ipmitool", &dir.display().to_string()).is_none());
        // 恢复可执行 → 命中（绝对路径返回）
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).unwrap();
        let found = find_in_path("ipmitool", &dir.display().to_string()).unwrap();
        assert_eq!(found, exe);
        // 空目录串与不存在名
        assert!(find_in_path("no-such-bin", &dir.display().to_string()).is_none());
        assert!(find_in_path("ipmitool", "").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ ipmitool 命令组装（argv 防注入基线）============

    #[test]
    fn ipmi_argv_shapes() {
        assert_eq!(
            ipmi_local_argv(IpmiCmd::ChassisPower("cycle".into())),
            vec!["chassis", "power", "cycle"]
        );
        assert_eq!(ipmi_local_argv(IpmiCmd::SelInfo), vec!["sel", "info"]);
        let argv = ipmi_remote_argv(
            "192.0.2.77",
            623,
            "admin",
            "p@ss 'quoted'; rm -rf /",
            Some("3"),
            IpmiCmd::ChassisPower("on".into()),
        );
        assert_eq!(
            argv,
            vec![
                "-I",
                "lanplus",
                "-H",
                "192.0.2.77",
                "-p",
                "623",
                "-U",
                "admin",
                "-P",
                "p@ss 'quoted'; rm -rf /",
                "-C",
                "3",
                "chassis",
                "power",
                "on",
            ],
            "密码/主机作为独立 argv 直传，原样保留不经 shell 解释"
        );
        // 无 cipher：不含 -C
        let argv2 = ipmi_remote_argv("h", 623, "u", "p", None, IpmiCmd::ChassisStatus);
        assert!(!argv2.contains(&"-C".to_string()));
        // 空白 cipher 同样不注入
        let argv3 = ipmi_remote_argv("h", 623, "u", "p", Some("  "), IpmiCmd::ChassisStatus);
        assert!(!argv3.contains(&"-C".to_string()));
    }

    // ============ 本机 BMC：降级（非 500）============

    #[tokio::test]
    async fn bmc_degrades_without_ipmitool() {
        let dir = temp_dir("bmc-degrade");
        let h = degraded_handler(&dir);
        // GET /power/bmc：200 + available:false + 安装指引（明确降级非 500）
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/bmc"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "降级非 500: {resp:?}");
        assert_eq!(resp.body["available"], false);
        assert_eq!(resp.body["ipmitool_found"], false);
        assert!(resp.body["system_power"].is_null());
        let hint = resp.body["hint"].as_str().unwrap();
        assert!(hint.contains("ipmitool"), "hint 提示安装: {hint}");

        // POST /power/bmc/power：503 降级（非 500）
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/bmc/power",
                serde_json::json!({"action": "on"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 503, "降级非 500: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("ipmitool"));

        // GET /power/bmc/sensors：200 + available:false（WoL 域不受影响）
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/bmc/sensors"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["available"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ 本机 BMC：假 ipmitool（argv 断言 + 输出解析）============

    #[tokio::test]
    async fn bmc_power_and_status_with_fake_ipmitool() {
        let dir = temp_dir("bmc-fake");
        let tool = fake_ipmitool(&dir);
        let h = handler_with(&dir, Some(&tool), None);

        // GET /power/bmc：chassis/SEL/MC 三段解析
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/bmc"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["available"], true);
        assert_eq!(resp.body["system_power"], "on");
        let chassis = resp.body["chassis"].as_array().unwrap();
        assert!(chassis.iter().any(|kv| kv["key"] == "System Power"));
        assert_eq!(resp.body["sel"][1]["key"], "Entries");
        assert_eq!(resp.body["sel"][1]["value"], "12");
        assert_eq!(resp.body["mc"][1]["value"], "1.73", "BMC 固件版本");

        // POST /power/bmc/power：argv 落盘可查（不经 shell 的直接证据）
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/bmc/power",
                serde_json::json!({"action": "cycle"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["action"], "cycle");
        assert_eq!(resp.body["output"], "Chassis Power Control: Up/On");
        let logged = std::fs::read_to_string(dir.join("argv.log")).unwrap();
        assert!(
            logged.trim_end().ends_with("chassis power cycle"),
            "最后一组 argv 应为 chassis power cycle: {logged}"
        );

        // 非法 action → 400
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/bmc/power",
                serde_json::json!({"action": "nuke"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);

        // 传感器表解析
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/bmc/sensors"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["available"], true);
        assert_eq!(resp.body["count"], 2);
        assert_eq!(resp.body["rows"][0]["name"], "Ambient Temp");
        assert_eq!(resp.body["rows"][1]["status"], "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ 远程设备：CRUD + 密码脱敏 + 持久化 ============

    #[tokio::test]
    async fn device_crud_masks_password_and_persists() {
        let dir = temp_dir("dev-crud");
        let h = handler_with(&dir, None, None);

        // 注册（密码入 state，响应脱敏）
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/ipmi/devices",
                serde_json::json!({
                    "name": "节点 BMC",
                    "host": "192.0.2.77",
                    "username": "admin",
                    "password": "s3cret!",
                    "cipher": "3"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        assert!(resp.body["password"].is_null(), "密码不出 HTTP");
        assert_eq!(resp.body["has_password"], true);
        assert_eq!(resp.body["port"], 623, "缺省 RMCP 端口");
        let id = resp.body["id"].as_str().unwrap().to_string();

        // 列表同样脱敏
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/ipmi/devices"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["password"].is_null());

        // 状态文件里明文落盘（开发期取舍；生产须 vault 化）
        let raw = std::fs::read_to_string(dir.join("power-state.json")).unwrap();
        assert!(raw.contains("s3cret!"), "密码持久化在 state 文件");

        // 重启（重新构造）后仍在
        let h2 = handler_with(&dir, None, None);
        let resp = h2
            .handle(get_req("/api/v1/provisioning/power/ipmi/devices"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1);

        // 校验：host 含空白拒绝 / 空名拒绝
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/ipmi/devices",
                serde_json::json!({"name": "x", "host": "1.2.3.4 5.6"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);

        // 删除 → 204；再删 → 404
        let resp = h
            .handle(del_req(&format!(
                "/api/v1/provisioning/power/ipmi/devices/{id}"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 204);
        let resp = h
            .handle(del_req(&format!(
                "/api/v1/provisioning/power/ipmi/devices/{id}"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ 远程设备：test / power / status（假 ipmitool argv）============

    #[tokio::test]
    async fn device_test_power_status_with_fake_ipmitool() {
        let dir = temp_dir("dev-remote");
        let tool = fake_ipmitool(&dir);
        let h = handler_with(&dir, Some(&tool), None);
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/ipmi/devices",
                serde_json::json!({"name": "bmc-a", "host": "192.0.2.77", "password": "pw"}),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();

        // test：reachable + chassis 解析
        let resp = h
            .handle(post_req(
                &format!("/api/v1/provisioning/power/ipmi/devices/{id}/test"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["reachable"], true);
        assert_eq!(resp.body["system_power"], "on");

        // power：argv 断言（lanplus/-H/-p/-U/-P + chassis power on）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/provisioning/power/ipmi/devices/{id}/power"),
                serde_json::json!({"action": "on"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["target"], "192.0.2.77:623");
        let logged = std::fs::read_to_string(dir.join("argv.log")).unwrap();
        let last = logged.trim_end().lines().last().unwrap();
        assert_eq!(
            last, "-I lanplus -H 192.0.2.77 -p 623 -U admin -P pw chassis power on",
            "远程 argv 组装（无 shell，参数原样）"
        );

        // status：实时探测回写 reachable
        let resp = h
            .handle(get_req(&format!(
                "/api/v1/provisioning/power/ipmi/devices/{id}/status"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["reachable"], true);
        assert_eq!(resp.body["system_power"], "on");

        // 不存在设备 → 404
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/ipmi/devices/nope/power",
                serde_json::json!({"action": "on"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ 网段扫描：端点状态机（真 UDP + 假 BMC）============

    /// 起一个应答 Presence Pong 的假 BMC（绑定 127.0.0.1 随机端口）。
    async fn fake_bmc_responder() -> (u16, tokio::task::JoinHandle<()>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let Ok((n, peer)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                if parse_rmcp_pong(&buf[..n]).is_none() && buf.first() == Some(&0x06) {
                    // 收到的是 presence ping → 回 openbmc 样本 pong
                    let _ = sock.send_to(&OPENBMC_PONG, peer).await;
                }
            }
        });
        (port, task)
    }

    #[tokio::test]
    async fn scan_endpoint_lifecycle_with_fake_bmc() {
        let dir = temp_dir("scan");
        let h = handler_with(&dir, None, None);
        let (bmc_port, responder) = fake_bmc_responder().await;

        // 非法 CIDR → 400
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/ipmi/scan",
                serde_json::json!({"cidr": "10.0.0.0/16"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);

        // /32 发起 → 202 running
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/ipmi/scan",
                serde_json::json!({
                    "cidr": "127.0.0.1/32",
                    "port": bmc_port,
                    "timeout_ms": 200,
                    "concurrency": 4
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{resp:?}");
        assert_eq!(resp.body["status"], "running");
        assert_eq!(resp.body["total"], 1);
        let scan_id = resp.body["id"].as_str().unwrap().to_string();

        // 轮询到 completed
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut task_json = serde_json::Value::Null;
        while std::time::Instant::now() < deadline {
            let resp = h
                .handle(get_req(&format!(
                    "/api/v1/provisioning/power/ipmi/scan/{scan_id}"
                )))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            if resp.body["status"] == "completed" {
                task_json = resp.body;
                break;
            }
            assert_eq!(resp.body["status"], "running");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(task_json["status"], "completed", "扫描应在期限内完成");
        assert_eq!(task_json["scanned"], 1);
        let found = task_json["found"].as_array().unwrap();
        assert_eq!(found.len(), 1, "假 BMC 命中（去重）");
        assert_eq!(found[0]["ip"], "127.0.0.1");
        assert_eq!(found[0]["ipmi_supported"], true);
        assert_eq!(found[0]["rmcp_plus_supported"], true);
        assert_eq!(found[0]["enterprise_iana"], 4542);
        assert_eq!(found[0]["asf_version"], "1.0");

        // 未知任务 → 404
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/ipmi/scan/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        responder.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ WoL：端到端本地 UDP 收包验证字节 ============

    #[tokio::test]
    async fn wol_wake_delivers_magic_bytes_locally() {
        let dir = temp_dir("wol-wake");
        let h = degraded_handler(&dir); // 无 ipmitool：WoL 域必须照常工作
        let recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = recv.local_addr().unwrap().port();

        // 按 name 唤醒（先注册，广播地址覆盖为本机收包口）
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/targets",
                serde_json::json!({
                    "name": "节点A",
                    "mac": "AA:BB:CC:DD:EE:FF",
                    "secureon_password": "112233445566"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        assert!(resp.body["secureon_password"].is_null(), "SecureOn 脱敏");
        assert_eq!(resp.body["has_secureon"], true);
        assert_eq!(resp.body["mac"], "aa:bb:cc:dd:ee:ff", "MAC 规范化");

        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/wake",
                serde_json::json!({
                    "name": "节点A",
                    "broadcast": "127.0.0.1",
                    "port": port
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["sent"], 3, "3 次广播");
        assert_eq!(resp.body["attempts"], 3);
        assert_eq!(resp.body["bytes_per_packet"], 108, "含 SecureOn");
        assert_eq!(resp.body["secureon"], true);

        // 本地收包逐字节验证（至少读到一包）
        let mut buf = [0u8; 128];
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), recv.recv_from(&mut buf))
            .await
            .expect("应收到魔术包")
            .unwrap();
        let expect = build_magic_packet(
            &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
            Some(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]),
        );
        assert_eq!(&buf[..n], expect.as_slice(), "魔术包字节级一致");

        // 裸 MAC 临时唤醒（无注册目标）
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/wake",
                serde_json::json!({"mac": "aa:bb:cc:dd:ee:ff", "broadcast": "127.0.0.1", "port": port}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["bytes_per_packet"], 102, "无 SecureOn");
        // 两者都不给 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/wake",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wol_target_crud_and_mac_validation() {
        let dir = temp_dir("wol-crud");
        let h = handler_with(&dir, None, None);
        // MAC 非法 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/targets",
                serde_json::json!({"name": "x", "mac": "zz:bb:cc:dd:ee:ff"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // SecureOn 非 6 字节 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/targets",
                serde_json::json!({"name": "x", "mac": "aa:bb:cc:dd:ee:ff", "secureon_password": "1234"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 广播非法 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/targets",
                serde_json::json!({"name": "x", "mac": "aa:bb:cc:dd:ee:ff", "broadcast": "not-an-ip"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);

        // 列表含预置示例 + 新增；删除后 404
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/wol/targets"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1, "预置示例目标");
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/power/wol/targets",
                serde_json::json!({"name": "节点B", "mac": "01-02-03-04-05-06"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["mac"], "01:02:03:04:05:06");
        assert_eq!(resp.body["port"], 9, "缺省端口 9");
        assert_eq!(resp.body["broadcast"], "255.255.255.255");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(del_req(&format!(
                "/api/v1/provisioning/power/wol/targets/{id}"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 204);
        let resp = h
            .handle(del_req(&format!(
                "/api/v1/provisioning/power/wol/targets/{id}"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ ARP 邻居（假 ip 命令）============

    #[tokio::test]
    async fn arp_endpoint_with_fake_ip() {
        let dir = temp_dir("arp");
        let ip = fake_bin(
            &dir,
            "ip",
            "printf '192.168.1.14 dev br0 lladdr B4:2E:99:AA:BB:CC REACHABLE\\n192.168.1.20 dev br0 lladdr aa:bb:cc:dd:ee:ff STALE\\n'",
        );
        let h = handler_with(&dir, None, Some(&ip));
        let resp = h
            .handle(get_req("/api/v1/provisioning/power/wol/arp"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["available"], true);
        let ns = resp.body["neighbors"].as_array().unwrap();
        assert_eq!(ns.len(), 2);
        assert_eq!(ns[0]["mac"], "b4:2e:99:aa:bb:cc");
        // ip 缺失 → 降级可用性说明（非 500）
        let h2 = handler_with(&dir, None, None);
        let resp = h2
            .handle(get_req("/api/v1/provisioning/power/wol/arp"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["available"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ 路由声明 + 鉴权矩阵 ============

    #[tokio::test]
    async fn routes_declares_sixteen_endpoints() {
        let h = PowerRouteHandler::with_paths(
            PathBuf::from("/tmp/os-api-power-route-check.json"),
            None,
            None,
        );
        let routes = h.routes().await;
        assert_eq!(routes.len(), 16, "应有 16 条路由: {routes:?}");
        for p in [
            "/api/v1/provisioning/power/bmc",
            "/api/v1/provisioning/power/bmc/power",
            "/api/v1/provisioning/power/ipmi/devices/:id/test",
            "/api/v1/provisioning/power/ipmi/scan/:id",
            "/api/v1/provisioning/power/wol/wake",
            "/api/v1/provisioning/power/wol/arp",
        ] {
            assert!(routes.iter().any(|r| r.path == p), "缺少端点 {p}");
        }
    }

    #[tokio::test]
    async fn routes_auth_matrix() {
        let h = PowerRouteHandler::with_paths(
            PathBuf::from("/tmp/os-api-power-route-auth.json"),
            None,
            None,
        );
        let routes = h.routes().await;
        assert!(
            routes.iter().all(|r| r.handler_component == "power"),
            "全部归属 power 组件"
        );
        // 写操作（POST/DELETE）除 wol/wake（开发期公开）外一律 admin
        for r in &routes {
            let is_wake =
                r.method == HttpMethod::Post && r.path == "/api/v1/provisioning/power/wol/wake";
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                if is_wake {
                    assert!(!r.requires_auth, "wake 开发期公开: {r:?}");
                } else {
                    assert!(r.requires_auth, "写操作需 auth: {r:?}");
                    assert_eq!(r.required_roles, vec!["admin".to_string()]);
                }
            }
        }
        // 读操作全部公开（观测面）
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "GET 观测面公开: {r:?}");
            }
        }
    }

    // ============ 默认 trait ============

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<PowerRouteHandler>();
    }
}
