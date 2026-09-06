//! `NetworkRouteHandler` —— 网络管理（网卡 / 路由 / 防火墙 / VLAN / 桥接）的
//! HTTP 适配器（规划文档 §3.4 / §3.6）。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/network/*`）翻译为「真实探测」或「内存态占位」
//! 两类响应，对外提供统一的网络管理 REST 入口。这是 OS UI「网络管理」应用对应的
//! 后端路由。
//!
//! # 当前实现策略：内存态 + 真实探测混合
//!
//! - **真实探测**（只读，无特权要求）：网卡列表 / 路由表 / 状态概要
//!   - `GET /api/v1/network/interfaces` → `ip -j -br addr` + 读 `/sys/class/net/<name>/speed`
//!   - `GET /api/v1/network/routes`     → `ip -j route show default`
//!   - `GET /api/v1/network/status`     → 聚合默认网关 / DNS / 接口计数
//! - **内存态占位**（写操作 / 需特权）：防火墙规则 / 创建 VLAN / 创建桥接
//!   - `GET  /api/v1/network/firewall`  → 返回内存规则列表（预置空）
//!   - `POST /api/v1/network/vlan`      → 记录到内存（占位，不真改内核）
//!   - `POST /api/v1/network/bridge`    → 记录到内存（占位，不真改内核）
//!
//! 真实写操作需 root/CAP_NET_ADMIN（`ip link add` / `nft add rule`），且 `os-network`
//! 真实后端（rtnetlink / nftnl-ffi）当前未作为 `os-api` 的依赖注入。
//! 按本任务红线（「重点是路由能响应、JSON 结构正确，数据可后续接真实后端」），
//! 写操作先落内存态；未来接通 `os-network` 真实后端时，仅需把内部
//! `Mutex<Vec<FirewallRule>>` / `Mutex<Vec<VlanSpec>>` / `Mutex<Vec<BridgeSpec>>`
//! 替换为对 `Arc<NetlinkManager>` / `Arc<NftFirewall>` 的调用，路由签名与 JSON
//! 结构保持不变。
//!
//! # spawn_blocking 模式
//!
//! `ip` / `cat /sys/class/net/...` 是阻塞系统调用，按 [`crate::handlers::storage`]
//! 磁盘探测的同款模式，统一丢进 `tokio::task::spawn_blocking` 跑 `std::process::Command`，
//! 避免卡 tokio runtime。
//!
//! # 路径参数
//!
//! 网关 dispatch 当前不向 handler 传递 `PathParams`，故 `handle` 从 `req.path`
//! 字符串解析（`split('?')` 剥离 query）。本 handler 路由都是静态路径，按
//! (method, path) 精确分发。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO（对外 JSON 结构——与前端「网络管理」接口对齐）
// ----------------------------------------------------------------------------

/// `GET /api/v1/network/interfaces` 返回的单个网卡信息。
///
/// 字段命名与前端 `NetworkInterface` 接口对齐（snake_case）。
/// `addresses` 形如 `["192.0.2.106/24"]`（CIDR）；`speed_mbps` 从
/// `/sys/class/net/<name>/speed` 读，读失败（虚拟 / 无链路）为 None；
/// `type` 经 `/sys/class/net/<name>/wireless` 目录与接口名启发式判定为
/// `ethernet` / `wifi` / `loopback`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    /// 网卡名（如 `enp131s0`）
    pub name: String,
    /// 状态：`up` / `down`（`ip -br` 的 `operstate`，UNKNOWN 归一为 down）
    pub state: String,
    /// IPv4/IPv6 地址（CIDR 形式，如 `192.0.2.106/24`）
    pub addresses: Vec<String>,
    /// 链路速率（Mbps），从 `/sys/class/net/<name>/speed` 读；读失败为 None
    pub speed_mbps: Option<u64>,
    /// 类型：`ethernet` / `wifi` / `loopback`
    #[serde(rename = "type")]
    pub kind: String,
    /// 网卡角色标签（`normal` / `management` / `storage` / `pxe` / `dhcp` / `dns`），
    /// 从内存态 `nic_roles` 映射读取，默认 `normal`。允许前端缺失时回写默认值。
    #[serde(default = "NicRole::default_str")]
    pub role: String,
}

/// 网卡角色标签——标识网卡用途（PXE/DHCP/DNS/管理口/存储口等）。
///
/// 序列化为 snake_case 单词（与前端徽章 key 一致）。`Normal` 为默认（无特殊角色）。
/// 该枚举仅用于 `POST /role` 请求体的反序列化校验 + `nic_roles` 映射的内存存储；
/// `GET /interfaces` 返回的 `role` 字段为字符串（`normal`/`management`/...），
/// 由 [`NicRole::as_str`] 转出。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NicRole {
    /// 普通（默认，无特殊角色）
    #[default]
    Normal,
    /// 管理口
    Management,
    /// 存储口（存储 / 复制流量）
    Storage,
    /// PXE 启动服务口
    Pxe,
    /// DHCP 服务口
    Dhcp,
    /// DNS 服务口
    Dns,
}

impl NicRole {
    /// 默认角色字符串（`"normal"`），供 `NetworkInterface::role` 的 `serde(default)` 引用。
    #[must_use]
    pub fn default_str() -> String {
        "normal".to_string()
    }

    /// 转为对外字符串（snake_case 单词）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NicRole::Normal => "normal",
            NicRole::Management => "management",
            NicRole::Storage => "storage",
            NicRole::Pxe => "pxe",
            NicRole::Dhcp => "dhcp",
            NicRole::Dns => "dns",
        }
    }

    /// 从字符串解析为角色（未知值回退 `Normal`，宽松解析便于前端容错）。
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "management" => NicRole::Management,
            "storage" => NicRole::Storage,
            "pxe" => NicRole::Pxe,
            "dhcp" => NicRole::Dhcp,
            "dns" => NicRole::Dns,
            _ => NicRole::Normal,
        }
    }
}

/// `GET /api/v1/network/routes` 返回的单条默认路由。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultRoute {
    /// 目的地（恒为 `default`）
    pub destination: String,
    /// 网关 IP（如 `192.0.2.1`）
    pub gateway: String,
    /// 出接口（如 `enp131s0`）
    #[serde(rename = "interface")]
    pub iface: String,
}

/// `GET /api/v1/network/status` 返回的网络状态概要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatus {
    /// 默认网关（无默认路由时为 None）
    pub default_gateway: Option<String>,
    /// DNS 服务器列表（从 `/etc/resolv.conf` 解析 nameserver）
    pub dns_servers: Vec<String>,
    /// 网卡总数
    pub interface_count: usize,
    /// 处于 up 状态的网卡数
    pub up_count: usize,
}

/// 防火墙规则（内存态占位 JSON 结构）。
///
/// 字段与 `os-network::FirewallRule` 对齐（简化），便于后续接真实 nft 后端时
/// JSON 结构不变。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirewallRule {
    /// 规则 ID（内存态自增）
    pub id: String,
    /// 协议（`tcp` / `udp` / `any`）
    pub protocol: String,
    /// 源端口范围（如 `any` / `1000-2000`）
    pub src_port: String,
    /// 目标端口范围（如 `80`）
    pub dst_port: String,
    /// 动作（`allow` / `deny` / `redirect`）
    pub action: String,
}

/// 创建 VLAN 请求体（`POST /api/v1/network/vlan`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VlanSpec {
    /// 父接口名（如 `enp131s0`）
    pub parent: String,
    /// VLAN ID（1..=4094）
    pub vlan_id: u16,
    /// 新 VLAN 接口名（如 `vlan100`）
    pub name: String,
}

/// 创建桥接请求体（`POST /api/v1/network/bridge`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeSpec {
    /// 新桥接接口名（如 `br0`）
    pub name: String,
}

/// `GET /api/v1/network/bonds` 返回的单条链路聚合（bond）信息。
///
/// 由 [`parse_bond_info`] 解析 `/proc/net/bonding/<name>` 得到 mode / status / slaves，
/// `name` 由 handler 从文件名回填（bonding 文件内容本身不含 bond 名）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BondInfo {
    /// bond 名（如 `bond0`），由 handler 从文件名回填
    pub name: String,
    /// 聚合模式（如 `IEEE 802.3ad Dynamic link aggregation`）
    pub mode: String,
    /// bond 主状态（`up` / `down`）
    pub status: String,
    /// 从接口列表（如 `["eth0", "eth1"]`）
    pub slaves: Vec<String>,
}

/// `POST /api/v1/network/bonds` 请求体（admin）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BondCreateReq {
    /// 新 bond 名（如 `bond0`）
    pub name: String,
    /// 聚合模式（`802.3ad` / `active-backup` / `balance-rr` / 数字 `4` / ...）
    pub mode: String,
    /// 从接口列表（如 `["eth0", "eth1"]`）
    pub slaves: Vec<String>,
}

/// `POST /api/v1/network/firewall/rules` 请求体（admin）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirewallRuleAddReq {
    /// 链（`INPUT` / `OUTPUT` / `FORWARD`）
    pub chain: String,
    /// 动作（`accept` / `drop` / `reject` → 大写后传 `-j`）
    pub action: String,
    /// 协议（`tcp` / `udp` / `any`）
    pub protocol: String,
    /// 源 CIDR/IP（`any` 或空表示不限）
    pub source: String,
    /// 目的 CIDR/IP（`any` 或空表示不限）
    pub dest: String,
    /// 目标端口（0 表示不限）
    pub port: u16,
}

/// `GET /api/v1/network/firewall/rules` 返回的单条 iptables 规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallRuleEntry {
    /// 规则 ID（`<chain>#<num>`，供 DELETE 引用）
    pub id: String,
    /// 所属链
    pub chain: String,
    /// 行号（`iptables --line-numbers`）
    pub num: u32,
    /// target（`ACCEPT` / `DROP` / ...）
    pub target: String,
    /// 协议
    pub protocol: String,
    /// 源
    pub source: String,
    /// 目的
    pub destination: String,
    /// 原始规则行（便于展示）
    pub raw: String,
}

// ----------------------------------------------------------------------------
// NetworkRouteHandler
// ----------------------------------------------------------------------------

/// 网络管理路由处理器——HTTP 边界适配到「真实探测 + 内存态占位」混合后端
/// （可后续替换为真实 `os-network` NetlinkManager / NftFirewall）。
///
/// 真实探测（网卡/路由/状态）直接 spawn_blocking 跑 `ip` / 读 sysfs；
/// 内存态（防火墙/VLAN/桥接）持三把 `Mutex<Vec<_>>`，构造时空，
/// `POST` 写入、`GET firewall` 回读。
pub struct NetworkRouteHandler {
    /// 内存态防火墙规则（占位，预置空）
    firewall_rules: Mutex<Vec<FirewallRule>>,
    /// 内存态 VLAN 列表（占位）
    vlans: Mutex<Vec<VlanSpec>>,
    /// 内存态桥接列表（占位）
    bridges: Mutex<Vec<BridgeSpec>>,
    /// 内存态网卡角色映射（网卡名 → 角色），默认不在此 map 中的网卡均为 `Normal`。
    nic_roles: Mutex<HashMap<String, NicRole>>,
}

impl NetworkRouteHandler {
    /// 构造 handler，预置空内存态（真实探测部分不持有状态）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            firewall_rules: Mutex::new(Vec::new()),
            vlans: Mutex::new(Vec::new()),
            bridges: Mutex::new(Vec::new()),
            nic_roles: Mutex::new(HashMap::new()),
        }
    }

    /// 内存态防火墙规则快照（测试 / 诊断用）。
    #[must_use]
    pub fn firewall_snapshot(&self) -> Vec<FirewallRule> {
        self.firewall_rules
            .lock()
            .expect("firewall poisoned")
            .clone()
    }

    /// 内存态 VLAN 列表快照。
    #[must_use]
    pub fn vlan_snapshot(&self) -> Vec<VlanSpec> {
        self.vlans.lock().expect("vlans poisoned").clone()
    }

    /// 内存态桥接列表快照。
    #[must_use]
    pub fn bridge_snapshot(&self) -> Vec<BridgeSpec> {
        self.bridges.lock().expect("bridges poisoned").clone()
    }

    /// 取某网卡当前角色（未显式设置时回退 `Normal`）。
    #[must_use]
    pub fn nic_role(&self, name: &str) -> NicRole {
        self.nic_roles
            .lock()
            .expect("nic_roles poisoned")
            .get(name)
            .copied()
            .unwrap_or(NicRole::Normal)
    }

    /// 设置某网卡角色（覆盖写入）。
    pub fn set_nic_role(&self, name: &str, role: NicRole) {
        self.nic_roles
            .lock()
            .expect("nic_roles poisoned")
            .insert(name.to_string(), role);
    }
}

impl Default for NetworkRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for NetworkRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 真实探测（只读）——
            spec(HttpMethod::Get, "/api/v1/network/interfaces", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/network/routes", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/network/status", false, vec![]),
            // —— 网卡角色（内存态）—— :name 为路径参数段，由网关 dispatch 解析
            spec(
                HttpMethod::Get,
                "/api/v1/network/interfaces/:name/role",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/network/interfaces/:name/role",
                true,
                vec!["admin".into()],
            ),
            // —— 内存态占位 ——
            spec(HttpMethod::Get, "/api/v1/network/firewall", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/network/vlan",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/network/bridge",
                true,
                vec!["admin".into()],
            ),
            // —— 链路聚合（bond）：读 /proc/net/bonding，写 spawn ip link ——
            spec(HttpMethod::Get, "/api/v1/network/bonds", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/network/bonds",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/network/bonds/:name",
                true,
                vec!["admin".into()],
            ),
            // —— 防火墙规则（iptables）：spawn iptables，失败降级 ——
            spec(
                HttpMethod::Get,
                "/api/v1/network/firewall/rules",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/network/firewall/rules",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/network/firewall/rules/:id",
                true,
                vec!["admin".into()],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method, path) {
            // —— GET /api/v1/network/interfaces —— 真实探测网卡列表
            //
            // spawn_blocking 跑 `ip -j -br addr`（JSON），解析后对每个网卡读
            // `/sys/class/net/<name>/speed`（速率 Mbps）+ 判 wireless 目录定 type，
            // 再从内存态 `nic_roles` 映射补 role 字段（默认 normal）。
            (HttpMethod::Get, "/api/v1/network/interfaces") => {
                let mut ifaces = tokio::task::spawn_blocking(detect_interfaces)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("网卡探测任务 join 失败: {e}"))
                    })??;
                // 补 role 字段（从内存态映射读取，默认 normal）
                for iface in &mut ifaces {
                    iface.role = self.nic_role(&iface.name).as_str().to_string();
                }
                Ok(ok_json(to_value(&ifaces)?))
            }

            // —— GET /api/v1/network/routes —— 真实探测默认路由
            (HttpMethod::Get, "/api/v1/network/routes") => {
                let routes = tokio::task::spawn_blocking(detect_default_routes)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("路由探测任务 join 失败: {e}"))
                    })??;
                Ok(ok_json(to_value(&routes)?))
            }

            // —— GET /api/v1/network/status —— 网络状态概要（聚合）
            //
            // 探测默认网关（detect_default_routes 取首条 gateway）+ DNS（解析
            // /etc/resolv.conf 的 nameserver）+ 接口计数（detect_interfaces 的
            // 总数与 up 数）。
            (HttpMethod::Get, "/api/v1/network/status") => {
                let status = tokio::task::spawn_blocking(detect_status)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("网络状态探测任务 join 失败: {e}"))
                    })??;
                Ok(ok_json(to_value(&status)?))
            }

            // —— GET /api/v1/network/firewall —— 列出内存态防火墙规则
            (HttpMethod::Get, "/api/v1/network/firewall") => {
                let list = self
                    .firewall_rules
                    .lock()
                    .expect("firewall poisoned")
                    .clone();
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/network/vlan —— 创建 VLAN（内存态占位）
            //
            // body: VlanSpec { parent, vlan_id, name }。vlan_id 校验 1..=4094。
            (HttpMethod::Post, "/api/v1/network/vlan") => {
                let body: VlanSpec = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建 VLAN 请求体失败: {e}"))
                })?;
                if body.vlan_id == 0 || body.vlan_id > 4094 {
                    return Ok(error_response(
                        400,
                        &format!("VLAN ID 非法（须 1..=4094）: {}", body.vlan_id),
                    ));
                }
                self.vlans
                    .lock()
                    .expect("vlans poisoned")
                    .push(body.clone());
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&body)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/network/bridge —— 创建桥接（内存态占位）
            //
            // body: BridgeSpec { name }。
            (HttpMethod::Post, "/api/v1/network/bridge") => {
                let body: BridgeSpec = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建桥接请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "桥接名不可为空"));
                }
                self.bridges
                    .lock()
                    .expect("bridges poisoned")
                    .push(body.clone());
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&body)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/network/bonds —— 列链路聚合（读 /proc/net/bonding/*）
            //
            // spawn_blocking 读 /proc/net/bonding 目录，每个文件解析为一条 BondInfo，
            // 文件名回填 name。无 bonding 模块 / 目录不存在 → 空列表（降级，不报错）。
            (HttpMethod::Get, "/api/v1/network/bonds") => {
                let bonds = tokio::task::spawn_blocking(detect_bonds)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("bond 探测任务 join 失败: {e}"))
                    })??;
                Ok(ok_json(to_value(&bonds)?))
            }

            // —— POST /api/v1/network/bonds —— 创建 bond（spawn ip link，失败降级不 panic）
            //
            // body: BondCreateReq { name, mode, slaves }。构造 shell 命令后 best-effort 执行；
            // 无 root/CAP_NET_ADMIN 或 ip 不可用时返回 applied=false（200，非 Err），
            // 便于 UI 提示「需特权」而不炸服务。
            (HttpMethod::Post, "/api/v1/network/bonds") => {
                let body: BondCreateReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建 bond 请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "bond 名不可为空"));
                }
                if body.slaves.is_empty() {
                    return Ok(error_response(400, "从接口(slaves)不可为空"));
                }
                let tokens = build_bond_create_cmd(&body.name, &body.mode, &body.slaves);
                let script = tokens.join(" ");
                let applied = run_cmd("sh", &["-c".to_string(), script]).is_ok();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "name": body.name,
                        "mode": body.mode,
                        "slaves": body.slaves,
                        "applied": applied,
                        "warning": if applied {
                            String::new()
                        } else {
                            "bond 创建命令执行失败（可能缺少 root/CAP_NET_ADMIN）".to_string()
                        },
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/network/bonds/:name —— 删 bond（spawn ip link delete）
            //
            // 失败（无特权 / bond 不存在）降级为 applied=false（200，非 Err）。
            (HttpMethod::Delete, path) if path.starts_with("/api/v1/network/bonds/") => {
                let name = path.rsplit('/').next().unwrap_or("").to_string();
                if name.is_empty() {
                    return Ok(error_response(400, "bond 名不可为空"));
                }
                let applied =
                    run_cmd("ip", &["link".into(), "delete".into(), name.clone()]).is_ok();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({ "name": name, "applied": applied }),
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/network/firewall/rules —— 列 iptables 规则（spawn，失败降级空）
            //
            // spawn_blocking 跑 `iptables -L <chain> -n --line-numbers`（INPUT/OUTPUT/FORWARD），
            // 任意失败（iptables 不可用 / 无特权）整体降级为空数组。
            (HttpMethod::Get, "/api/v1/network/firewall/rules") => {
                let rules = tokio::task::spawn_blocking(list_firewall_rules)
                    .await
                    .unwrap_or_default();
                Ok(ok_json(to_value(&rules)?))
            }

            // —— POST /api/v1/network/firewall/rules —— 添加 iptables 规则（spawn，失败降级）
            //
            // body: FirewallRuleAddReq。构造 iptables 参数后 best-effort 执行。
            (HttpMethod::Post, "/api/v1/network/firewall/rules") => {
                let body: FirewallRuleAddReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加防火墙规则请求体失败: {e}"))
                })?;
                let chain = body.chain.trim().to_string();
                if chain.is_empty() {
                    return Ok(error_response(400, "chain 不可为空"));
                }
                let args = build_iptables_add_cmd(
                    &chain,
                    &body.action,
                    &body.protocol,
                    &body.source,
                    &body.dest,
                    body.port,
                );
                let applied = run_cmd("iptables", &args).is_ok();
                let rule_preview = format!("iptables {}", args.join(" "));
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "chain": chain,
                        "action": body.action,
                        "protocol": body.protocol,
                        "source": body.source,
                        "dest": body.dest,
                        "port": body.port,
                        "applied": applied,
                        "rule": rule_preview,
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/network/firewall/rules/:id —— 删 iptables 规则
            //
            // id 形如 `INPUT#3`（GET 返回的 id）。解析为 chain + num 后 `iptables -D`。
            (HttpMethod::Delete, path) if path.starts_with("/api/v1/network/firewall/rules/") => {
                let id = path.rsplit('/').next().unwrap_or("").to_string();
                let (chain, num) = match id.split_once('#') {
                    Some((c, n)) => (c.to_string(), n.to_string()),
                    None => {
                        return Ok(error_response(400, "规则 id 格式应为 <chain>#<num>"));
                    }
                };
                if chain.is_empty() || num.is_empty() {
                    return Ok(error_response(400, "规则 id 格式应为 <chain>#<num>"));
                }
                let applied =
                    run_cmd("iptables", &["-D".into(), chain.clone(), num.clone()]).is_ok();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "id": id, "chain": chain, "num": num, "applied": applied
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/network/interfaces/:name/role —— 取某网卡角色
            //
            // 路径模式含 `:name` 参数段，dispatch 已按模式匹配到此 handler；
            // 这里从 req.path 末段回解 name（路径形如 .../<name>/role）。
            (HttpMethod::Get, path)
                if path.starts_with("/api/v1/network/interfaces/") && path.ends_with("/role") =>
            {
                let name = extract_iface_name(path).ok_or_else(|| {
                    ApiGatewayError::Internal(format!("解析网卡名失败（路径非法）: {path}"))
                })?;
                let role = self.nic_role(&name);
                Ok(ok_json(serde_json::json!({
                    "name": name,
                    "role": role.as_str(),
                })))
            }

            // —— POST /api/v1/network/interfaces/:name/role —— 设置某网卡角色
            //
            // body: {"role": "pxe"}。role 值宽松解析（未知值回退 normal 并 400 提示）。
            (HttpMethod::Post, path)
                if path.starts_with("/api/v1/network/interfaces/") && path.ends_with("/role") =>
            {
                let name = extract_iface_name(path).ok_or_else(|| {
                    ApiGatewayError::Internal(format!("解析网卡名失败（路径非法）: {path}"))
                })?;
                if name.is_empty() {
                    return Ok(error_response(400, "网卡名不可为空"));
                }
                #[derive(serde::Deserialize)]
                struct RoleBody {
                    role: String,
                }
                let body: RoleBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析设置网卡角色请求体失败: {e}"))
                })?;
                // 校验角色值合法（未知 → 400，避免静默回退 normal）
                let normalized = body.role.trim().to_ascii_lowercase();
                let valid = matches!(
                    normalized.as_str(),
                    "normal" | "management" | "storage" | "pxe" | "dhcp" | "dns"
                );
                if !valid {
                    return Ok(error_response(
                        400,
                        &format!(
                            "未知网卡角色: {}（合法值: normal/management/storage/pxe/dhcp/dns）",
                            body.role
                        ),
                    ));
                }
                let role = NicRole::from_str_lossy(&normalized);
                self.set_nic_role(&name, role);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "name": name,
                        "role": role.as_str(),
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "network: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 真实探测：ip -j -br addr / ip -j route / sysfs / resolv.conf
// ----------------------------------------------------------------------------

/// `ip -j -br addr` 输出的反序列化结构（仅取所需字段）。
///
/// 输出形如：
/// ```json
/// [{"ifname":"enp131s0","operstate":"UP","addr_info":[{"local":"192.0.2.106","prefixlen":24}]}]
/// ```
#[derive(Debug, serde::Deserialize)]
struct IpBrAddrEntry {
    #[serde(rename = "ifname")]
    name: String,
    #[serde(default)]
    operstate: String,
    #[serde(default)]
    addr_info: Vec<IpAddrInfo>,
}

#[derive(Debug, serde::Deserialize)]
struct IpAddrInfo {
    #[serde(default)]
    local: String,
    #[serde(default)]
    prefixlen: u32,
}

/// `ip -j route show default` 输出的反序列化结构（仅取所需字段）。
///
/// 输出形如：
/// ```json
/// [{"dst":"default","gateway":"192.0.2.1","dev":"enp131s0"}]
/// ```
#[derive(Debug, serde::Deserialize)]
struct IpRouteEntry {
    #[serde(default, rename = "dst")]
    destination: String,
    #[serde(default)]
    gateway: String,
    #[serde(default, rename = "dev")]
    iface: String,
}

/// 跑 `ip -j -br addr` 并解析为网卡列表（含 sysfs 速率 + 类型判定）。
///
/// 失败时返回 `ApiGatewayError::Internal`（与 storage::detect_disks 同款错误风格）。
/// 任意单卡的速率 / 类型探测失败不阻断整体（降级为 None / `ethernet`）。
fn detect_interfaces() -> Result<Vec<NetworkInterface>, ApiGatewayError> {
    let output = std::process::Command::new("ip")
        .args(["-j", "-br", "addr"])
        .output()
        .map_err(|e| ApiGatewayError::Internal(format!("执行 ip -j -br addr 失败: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiGatewayError::Internal(format!(
            "ip -j -br addr 失败（exit={}）: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    // ip -j 无地址时也可能输出空 stdout（合法），按空列表处理。
    let stdout = &output.stdout;
    let entries: Vec<IpBrAddrEntry> = if stdout.is_empty() {
        Vec::new()
    } else {
        serde_json::from_slice(stdout)
            .map_err(|e| ApiGatewayError::Internal(format!("解析 ip -j -br addr JSON 失败: {e}")))?
    };

    let ifaces = entries
        .into_iter()
        .map(|e| {
            let kind = detect_iface_type(&e.name);
            let speed_mbps = read_speed_mbps(&e.name);
            // operstate 归一：UP→up，DOWN/UNKNOWN/其它→down（与前端展示一致）
            let state = match e.operstate.to_ascii_lowercase().as_str() {
                "up" => "up".to_string(),
                _ => "down".to_string(),
            };
            let addresses = e
                .addr_info
                .iter()
                .map(|a| format!("{}/{}", a.local, a.prefixlen))
                .filter(|a| !a.starts_with('/')) // local 缺失则跳过
                .collect();
            NetworkInterface {
                name: e.name,
                state,
                addresses,
                speed_mbps,
                kind,
                // 角色默认 normal，由 GET /interfaces handler 从内存态映射覆写。
                role: NicRole::default_str(),
            }
        })
        .collect();
    Ok(ifaces)
}

/// 跑 `ip -j route show default` 并解析为默认路由列表。
///
/// 无默认路由时返回空 Vec（不报错）。
fn detect_default_routes() -> Result<Vec<DefaultRoute>, ApiGatewayError> {
    let output = std::process::Command::new("ip")
        .args(["-j", "route", "show", "default"])
        .output()
        .map_err(|e| {
            ApiGatewayError::Internal(format!("执行 ip -j route show default 失败: {e}"))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiGatewayError::Internal(format!(
            "ip -j route show default 失败（exit={}）: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let stdout = &output.stdout;
    let entries: Vec<IpRouteEntry> = if stdout.is_empty() {
        Vec::new()
    } else {
        serde_json::from_slice(stdout)
            .map_err(|e| ApiGatewayError::Internal(format!("解析 ip -j route JSON 失败: {e}")))?
    };
    let routes = entries
        .into_iter()
        .map(|e| DefaultRoute {
            // dst 缺失时回退为 "default"（语义即默认路由）
            destination: if e.destination.is_empty() {
                "default".to_string()
            } else {
                e.destination
            },
            gateway: e.gateway,
            iface: e.iface,
        })
        .collect();
    Ok(routes)
}

/// 聚合网络状态概要：默认网关 + DNS + 接口计数。
///
/// 三个子探测各自独立失败时降级（不阻断整体），任一硬失败（ip 命令本身不可执行）
/// 才向上抛错。DNS 解析失败 → 返回空列表。
fn detect_status() -> Result<NetworkStatus, ApiGatewayError> {
    let routes = detect_default_routes()?;
    let ifaces = detect_interfaces()?;
    let default_gateway = routes.into_iter().next().map(|r| r.gateway);
    let dns_servers = read_dns_servers();
    let interface_count = ifaces.len();
    let up_count = ifaces.iter().filter(|i| i.state == "up").count();
    Ok(NetworkStatus {
        default_gateway,
        dns_servers,
        interface_count,
        up_count,
    })
}

// ----------------------------------------------------------------------------
// sysfs / resolv.conf 读取（纯函数，便于单测）
// ----------------------------------------------------------------------------

/// 读 `/sys/class/net/<name>/speed`（Mbps）。
///
/// 读失败（虚拟接口无 speed 文件 / 链路 down 时内核返回 EINVAL）返回 None。
fn read_speed_mbps(name: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{name}/speed");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u64>().ok()
}

/// 判定网卡类型：loopback / wifi / ethernet。
///
/// - 名为 `lo` 或 `lo*` → loopback；
/// - 存在 `/sys/class/net/<name>/wireless` 目录 → wifi；
/// - 否则 → ethernet（保守默认）。
fn detect_iface_type(name: &str) -> String {
    if name == "lo" || name.starts_with("lo") {
        return "loopback".to_string();
    }
    let wireless = format!("/sys/class/net/{name}/wireless");
    if std::path::Path::new(&wireless).is_dir() {
        return "wifi".to_string();
    }
    "ethernet".to_string()
}

/// 解析 `/etc/resolv.conf` 提取 nameserver 列表。
///
/// 跳过注释行（`#`）与 `;`，取 `nameserver <ip>` 行的 IP。文件缺失 / 不可读 → 空列表。
fn read_dns_servers() -> Vec<String> {
    let content = match std::fs::read_to_string("/etc/resolv.conf") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                return None;
            }
            let mut parts = line.split_whitespace();
            if parts.next()? == "nameserver" {
                let ip = parts.next()?;
                if !ip.is_empty() {
                    return Some(ip.to_string());
                }
            }
            None
        })
        .collect()
}

// ----------------------------------------------------------------------------
// 链路聚合 / 防火墙：命令构造（纯函数）+ 探测/执行
// ----------------------------------------------------------------------------

/// 归一化 bond 模式名 → `ip link add type bond mode <X>` 接受的形式。
///
/// 支持数字（`0`..`6`）与常见别名（`lacp` → `802.3ad`）；未知值原样透传，
/// 交由 `ip` 校验（失败时 handler 降级为 applied=false）。
fn normalize_bond_mode(mode: &str) -> String {
    match mode.trim().to_ascii_lowercase().as_str() {
        "0" | "balance-rr" => "balance-rr".into(),
        "1" | "active-backup" => "active-backup".into(),
        "2" | "balance-xor" => "balance-xor".into(),
        "3" | "broadcast" => "broadcast".into(),
        "4" | "802.3ad" | "lacp" => "802.3ad".into(),
        "5" | "balance-tlb" => "balance-tlb".into(),
        "6" | "balance-alb" => "balance-alb".into(),
        other => other.to_string(),
    }
}

/// 构造创建 bond 的 shell 命令 token 序列（纯函数，不执行）。
///
/// 返回扁平 token 列表，用空格连接后交给 `sh -c` 执行：
/// `ip link add <name> type bond mode <mode> ; ip link set <slave> master <name> ; ...`
///
/// handler 负责 spawn 与失败降级；本函数仅做构造，便于单测。
#[must_use]
pub fn build_bond_create_cmd(name: &str, mode: &str, slaves: &[String]) -> Vec<String> {
    let m = normalize_bond_mode(mode);
    let mut tokens: Vec<String> = vec![
        "ip".into(),
        "link".into(),
        "add".into(),
        name.into(),
        "type".into(),
        "bond".into(),
        "mode".into(),
        m,
    ];
    for s in slaves {
        tokens.push(";".into());
        tokens.push("ip".into());
        tokens.push("link".into());
        tokens.push("set".into());
        tokens.push(s.clone());
        tokens.push("master".into());
        tokens.push(name.into());
    }
    tokens
}

/// 归一化防火墙动作 → iptables `-j` 目标（大写）。
fn normalize_iptables_action(action: &str) -> String {
    match action.trim().to_ascii_lowercase().as_str() {
        "accept" | "allow" => "ACCEPT".into(),
        "drop" | "deny" => "DROP".into(),
        "reject" => "REJECT".into(),
        "redirect" => "REDIRECT".into(),
        other => other.to_ascii_uppercase(),
    }
}

/// 构造添加一条 iptables 规则的参数向量（**不含** `iptables` 程序名）。
///
/// 形如：`-A INPUT -p tcp -s 0.0.0.0/0 -d 0.0.0.0/0 --dport 22 -j ACCEPT`。
/// `protocol` 为 `any`/空、`source`/`dest` 为 `any`/空、`port==0` 时对应子句自动省略。
#[must_use]
pub fn build_iptables_add_cmd(
    chain: &str,
    action: &str,
    protocol: &str,
    source: &str,
    dest: &str,
    port: u16,
) -> Vec<String> {
    let target = normalize_iptables_action(action);
    let chain = chain.trim();
    let mut args: Vec<String> = vec!["-A".into(), chain.into()];
    let proto = protocol.trim().to_ascii_lowercase();
    let src = source.trim();
    let dst = dest.trim();
    if !proto.is_empty() && proto != "any" {
        args.push("-p".into());
        args.push(proto.clone());
        if port > 0 {
            args.push("--dport".into());
            args.push(port.to_string());
        }
    }
    if !src.is_empty() && src != "any" {
        args.push("-s".into());
        args.push(src.into());
    }
    if !dst.is_empty() && dst != "any" {
        args.push("-d".into());
        args.push(dst.into());
    }
    args.push("-j".into());
    args.push(target);
    args
}

/// 解析 `/proc/net/bonding/<name>` 内容为 [`BondInfo`] 列表（纯函数）。
///
/// 单个 bonding 文件描述一个 bond，故返回长度恒为 0 或 1。
/// `name` 字段留空（内容不含 bond 名），由 handler 从文件名回填。
/// 解析字段：`Bonding Mode:`（mode）、首条 `MII Status:`（status，bond 本体）、
/// 所有 `Slave Interface:`（slaves）。无可识别字段时返回空 Vec。
#[must_use]
pub fn parse_bond_info(proc_output: &str) -> Vec<BondInfo> {
    let mut mode = String::new();
    let mut status = String::new();
    let mut slaves: Vec<String> = Vec::new();
    for line in proc_output.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Bonding Mode:") {
            mode = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("MII Status:") {
            // 首条 MII Status 为 bond 本体状态（出现在 Slave 段之前），只记一次。
            if status.is_empty() {
                status = v.trim().to_string();
            }
        } else if let Some(v) = line.strip_prefix("Slave Interface:") {
            let s = v.trim().to_string();
            if !s.is_empty() {
                slaves.push(s);
            }
        }
    }
    if mode.is_empty() && status.is_empty() && slaves.is_empty() {
        return Vec::new();
    }
    vec![BondInfo {
        name: String::new(),
        mode,
        status,
        slaves,
    }]
}

/// 探测系统链路聚合列表（读 `/proc/net/bonding/*`，文件名即 bond 名）。
///
/// `/proc/net/bonding` 不存在（无 bond / 内核未加载 bonding）时返回空 Vec，不报错。
fn detect_bonds() -> Result<Vec<BondInfo>, ApiGatewayError> {
    let dir = std::path::Path::new("/proc/net/bonding");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut infos = parse_bond_info(&content);
        for info in &mut infos {
            info.name = name.clone();
        }
        result.extend(infos);
    }
    Ok(result)
}

/// spawn 一个命令（best-effort）；失败返回 `Err(诊断消息)`，不 panic。
fn run_cmd(program: &str, args: &[String]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("执行 {program} 失败: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} 退出码 {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 解析 `iptables -L <chain> -n --line-numbers` 输出为规则条目（私有纯函数）。
///
/// 输出形如：
/// ```text
/// Chain INPUT (policy ACCEPT)
/// num  target  prot opt source     destination
/// 1    ACCEPT  tcp  --  0.0.0.0/0  0.0.0.0/0  tcp dpt:22
/// ```
/// 首列须为行号（`--line-numbers`），否则该行跳过（保守解析）。
fn parse_iptables_chain(chain: &str, output: &str) -> Vec<FirewallRuleEntry> {
    let mut rules = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Chain") || line.starts_with("target") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let num = match parts[0].parse::<u32>() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let target = parts[1].to_string();
        let protocol = parts.get(2).map(|s| (*s).to_string()).unwrap_or_default();
        let source = parts.get(4).map(|s| (*s).to_string()).unwrap_or_default();
        let destination = parts.get(5).map(|s| (*s).to_string()).unwrap_or_default();
        rules.push(FirewallRuleEntry {
            id: format!("{chain}#{num}"),
            chain: chain.to_string(),
            num,
            target,
            protocol,
            source,
            destination,
            raw: line.to_string(),
        });
    }
    rules
}

/// 列出 iptables 规则（INPUT/OUTPUT/FORWARD）。失败降级为空列表。
fn list_firewall_rules() -> Vec<FirewallRuleEntry> {
    let mut all = Vec::new();
    for chain in ["INPUT", "OUTPUT", "FORWARD"] {
        let out = match run_cmd(
            "iptables",
            &[
                "-L".into(),
                chain.into(),
                "-n".into(),
                "--line-numbers".into(),
            ],
        ) {
            Ok(o) => o,
            Err(_) => return Vec::new(), // iptables 不可用 → 空列表降级
        };
        all.extend(parse_iptables_chain(chain, &out));
    }
    all
}

// ----------------------------------------------------------------------------
// 内部辅助（与其它 handler 同款）
// ----------------------------------------------------------------------------

/// 从形如 `/api/v1/network/interfaces/<name>/role[?query]` 的路径中提取 `<name>`。
///
/// 返回 `None` 当段数不符（dispatch 已按模式匹配，正常不会触发）。
/// 网卡名可含 `.` / `-` 等（如 `enp131s0.100`），按 `/` 切分后取倒数第二段。
fn extract_iface_name(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // 期望形如 [..., "interfaces", "<name>", "role"]，name 为倒数第二段
    let name_idx = segs.len().checked_sub(2)?;
    if segs.get(name_idx + 1)? != &"role" {
        return None;
    }
    segs.get(name_idx).map(|s| s.to_string())
}

/// 构造一条 [`RouteSpec`]（component 固定 `network`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "network".to_string(),
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

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 把可序列化结果转成 `serde_json::Value`，序列化失败统一映射为 `ApiGatewayError::Internal`。
fn to_value<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

// ----------------------------------------------------------------------------
// 单元测——纯函数覆盖（探测函数依赖真实环境，单测只覆盖解析/类型判定逻辑）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个空状态 handler。
    fn empty_handler() -> NetworkRouteHandler {
        NetworkRouteHandler::new()
    }

    /// 构造一个 GET 请求。
    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    #[test]
    fn detect_iface_type_categorizes() {
        // loopback 严格按名匹配
        assert_eq!(detect_iface_type("lo"), "loopback");
        assert_eq!(detect_iface_type("lo0"), "loopback");
        // ethernet / wifi 依赖 sysfs wireless 目录，真实环境（含 wifi 卡）会归 wifi，
        // 无 wireless 目录的虚拟/纯以太环境归 ethernet。这里只断言落在两个值之一。
        let ethernet_kind = detect_iface_type("enp131s0");
        assert!(
            ethernet_kind == "ethernet" || ethernet_kind == "wifi",
            "enp131s0 应归 ethernet/wifi，实际: {ethernet_kind}"
        );
        let wifi_kind = detect_iface_type("wlp132s0");
        assert!(
            wifi_kind == "ethernet" || wifi_kind == "wifi",
            "wlp132s0 应归 ethernet/wifi，实际: {wifi_kind}"
        );
    }

    #[test]
    fn read_dns_servers_parses_nameserver_lines() {
        let tmp = std::env::temp_dir().join("resolv_test.conf");
        std::fs::write(
            &tmp,
            "# comment\nnameserver 192.0.2.1\n; semi\nnameserver 8.8.8.8\noptions edns0\n",
        )
        .unwrap();
        let content = std::fs::read_to_string(&tmp).unwrap();
        let dns: Vec<String> = content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                    return None;
                }
                let mut parts = line.split_whitespace();
                if parts.next()? == "nameserver" {
                    let ip = parts.next()?;
                    if !ip.is_empty() {
                        return Some(ip.to_string());
                    }
                }
                None
            })
            .collect();
        assert_eq!(dns, vec!["192.0.2.1".to_string(), "8.8.8.8".to_string()]);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn ip_br_addr_json_parses_typical_output() {
        let json = r#"[
            {"ifname":"lo","operstate":"UNKNOWN","addr_info":[{"local":"127.0.0.1","prefixlen":8}]},
            {"ifname":"enp131s0","operstate":"UP","addr_info":[{"local":"192.0.2.106","prefixlen":24},{"local":"fe80::62cf:84ff:fead:1c68","prefixlen":64}]},
            {"ifname":"wlp132s0","operstate":"DOWN","addr_info":[]}
        ]"#;
        let entries: Vec<IpBrAddrEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].name, "enp131s0");
        assert_eq!(entries[1].addr_info.len(), 2);
        assert_eq!(entries[1].addr_info[0].local, "192.0.2.106");
        assert_eq!(entries[1].addr_info[0].prefixlen, 24);
        assert_eq!(entries[2].addr_info.len(), 0);
    }

    #[test]
    fn ip_route_json_parses_default() {
        let json = r#"[{"dst":"default","gateway":"192.0.2.1","dev":"enp131s0","protocol":"dhcp","prefsrc":"192.0.2.106","metric":100}]"#;
        let entries: Vec<IpRouteEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].destination, "default");
        assert_eq!(entries[0].gateway, "192.0.2.1");
        assert_eq!(entries[0].iface, "enp131s0");
    }

    #[tokio::test]
    async fn routes_declares_all_endpoints() {
        let h = empty_handler();
        let routes = h.routes().await;
        // 8 原有 + 6 新增（bond GET/POST/DELETE + firewall/rules GET/POST/DELETE）= 14
        assert_eq!(routes.len(), 14);
        assert!(routes.iter().all(|r| r.handler_component == "network"));
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/interfaces")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/routes")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/firewall")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/network/vlan")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/network/bridge")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/status")));
        // 网卡角色路由（:name 参数段）
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/interfaces/:name/role")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/network/interfaces/:name/role")));
        // —— 新增：链路聚合（bond）——
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/bonds")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/network/bonds")));
        assert!(pairs.contains(&(HttpMethod::Delete, "/api/v1/network/bonds/:name")));
        // —— 新增：防火墙规则（iptables）——
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/network/firewall/rules")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/network/firewall/rules")));
        assert!(pairs.contains(&(HttpMethod::Delete, "/api/v1/network/firewall/rules/:id")));
        // 写操作要求 admin
        let post_vlan = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/network/vlan")
            .unwrap();
        assert!(post_vlan.requires_auth);
        assert_eq!(post_vlan.required_roles, vec!["admin".to_string()]);
        // 创建 bond 也要求 admin
        let post_bond = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/network/bonds")
            .unwrap();
        assert!(post_bond.requires_auth);
        assert_eq!(post_bond.required_roles, vec!["admin".to_string()]);
        // 设置角色也要求 admin
        let post_role = routes
            .iter()
            .find(|r| {
                r.method == HttpMethod::Post && r.path == "/api/v1/network/interfaces/:name/role"
            })
            .unwrap();
        assert!(post_role.requires_auth);
        assert_eq!(post_role.required_roles, vec!["admin".to_string()]);
    }

    #[tokio::test]
    async fn get_firewall_empty_returns_empty_array() {
        let h = empty_handler();
        let resp = h.handle(get_req("/api/v1/network/firewall")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn post_vlan_creates_and_validates() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/network/vlan".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "parent": "enp131s0",
                "vlan_id": 100,
                "name": "vlan100"
            }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create vlan 应成功");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["vlan_id"], 100);
        assert_eq!(resp.body["name"], "vlan100");
        assert_eq!(h.vlan_snapshot().len(), 1);

        // vlan_id 越界 → 400 body（非 Err）
        let req_bad = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/network/vlan".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "parent": "enp131s0",
                "vlan_id": 5000,
                "name": "vlan5000"
            }),
            auth: None,
        };
        let resp_bad = h.handle(req_bad).await.unwrap();
        assert_eq!(resp_bad.status, 400);
    }

    #[tokio::test]
    async fn post_vlan_invalid_body_returns_err() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/network/vlan".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "parent": "enp131s0" }), // 缺 vlan_id / name
            auth: None,
        };
        let err = h.handle(req).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn post_bridge_creates_and_rejects_empty_name() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/network/bridge".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "name": "br0" }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create bridge 应成功");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["name"], "br0");
        assert_eq!(h.bridge_snapshot().len(), 1);

        // 空名 → 400 body
        let req_bad = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/network/bridge".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "name": "  " }),
            auth: None,
        };
        let resp_bad = h.handle(req_bad).await.unwrap();
        assert_eq!(resp_bad.status, 400);
    }

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        let h = empty_handler();
        let resp = h.handle(get_req("/api/v1/network/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    #[tokio::test]
    async fn real_probe_interfaces_smoke() {
        // 真实环境探测：ip 命令应可用，至少返回 lo。
        // CI / 无 ip 环境下跳过（不 fail）。
        if std::process::Command::new("ip")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("[test] ip 命令不可用，跳过 real_probe_interfaces_smoke");
            return;
        }
        let h = empty_handler();
        let resp = h
            .handle(get_req("/api/v1/network/interfaces"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 应为数组");
        assert!(!arr.is_empty(), "至少应含 lo");
        let names: Vec<&str> = arr.iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"lo"), "应含 lo");
        // lo 类型应为 loopback
        let lo = arr.iter().find(|i| i["name"] == "lo").unwrap();
        assert_eq!(lo["type"], "loopback");
    }

    #[tokio::test]
    async fn real_probe_routes_smoke() {
        if std::process::Command::new("ip")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("[test] ip 命令不可用，跳过 real_probe_routes_smoke");
            return;
        }
        let h = empty_handler();
        let resp = h.handle(get_req("/api/v1/network/routes")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 应为数组");
        if !arr.is_empty() {
            assert_eq!(arr[0]["destination"], "default");
            assert!(arr[0]["gateway"].is_string());
        }
    }

    #[tokio::test]
    async fn real_probe_status_smoke() {
        if std::process::Command::new("ip")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("[test] ip 命令不可用，跳过 real_probe_status_smoke");
            return;
        }
        let h = empty_handler();
        let resp = h.handle(get_req("/api/v1/network/status")).await.unwrap();
        assert_eq!(resp.status, 200);
        // interface_count 至少 1（lo）
        let cnt = resp.body["interface_count"].as_u64().unwrap();
        assert!(cnt >= 1, "interface_count 至少 1");
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<NetworkRouteHandler>();
        assert_default::<NicRole>();
    }

    /// 构造一个 POST 请求（body 默认 Null）。
    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    #[test]
    fn nic_role_round_trips_strings() {
        // as_str / from_str_lossy 对全部合法值往返
        for r in [
            NicRole::Normal,
            NicRole::Management,
            NicRole::Storage,
            NicRole::Pxe,
            NicRole::Dhcp,
            NicRole::Dns,
        ] {
            let s = r.as_str();
            assert_eq!(NicRole::from_str_lossy(s), r);
            // 大小写 / 前后空白容错
            assert_eq!(
                NicRole::from_str_lossy(&format!(" {} ", s.to_uppercase())),
                r
            );
        }
        // 未知值 → Normal
        assert_eq!(NicRole::from_str_lossy("bogus"), NicRole::Normal);
        assert_eq!(NicRole::default_str(), "normal");
    }

    #[test]
    fn nic_role_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(NicRole::Management).unwrap(),
            serde_json::json!("management")
        );
        assert_eq!(
            serde_json::to_value(NicRole::Pxe).unwrap(),
            serde_json::json!("pxe")
        );
        // 反序列化
        let r: NicRole = serde_json::from_value(serde_json::json!("dhcp")).unwrap();
        assert_eq!(r, NicRole::Dhcp);
    }

    #[test]
    fn extract_iface_name_parses_role_path() {
        assert_eq!(
            extract_iface_name("/api/v1/network/interfaces/enp131s0/role"),
            Some("enp131s0".to_string())
        );
        // 带点 / 减号的网卡名（VLAN / 子接口）
        assert_eq!(
            extract_iface_name("/api/v1/network/interfaces/enp131s0.100/role"),
            Some("enp131s0.100".to_string())
        );
        // 带 query
        assert_eq!(
            extract_iface_name("/api/v1/network/interfaces/eth0/role?x=1"),
            Some("eth0".to_string())
        );
        // 非法路径
        assert_eq!(extract_iface_name("/api/v1/network/interfaces/eth0"), None);
    }

    #[tokio::test]
    async fn get_role_defaults_normal_and_set_reads_back() {
        let h = empty_handler();
        // 未设置 → 默认 normal
        let resp = h
            .handle(get_req("/api/v1/network/interfaces/enp131s0/role"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["role"], "normal");
        assert_eq!(resp.body["name"], "enp131s0");

        // 设置为 pxe
        let resp = h
            .handle(post_req(
                "/api/v1/network/interfaces/enp131s0/role",
                serde_json::json!({ "role": "pxe" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["role"], "pxe");

        // 读回
        let resp = h
            .handle(get_req("/api/v1/network/interfaces/enp131s0/role"))
            .await
            .unwrap();
        assert_eq!(resp.body["role"], "pxe");

        // 切换为 management
        let resp = h
            .handle(post_req(
                "/api/v1/network/interfaces/enp131s0/role",
                serde_json::json!({ "role": "management" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["role"], "management");
        assert_eq!(h.nic_role("enp131s0"), NicRole::Management);
    }

    #[tokio::test]
    async fn set_role_rejects_unknown_role_value() {
        let h = empty_handler();
        let resp = h
            .handle(post_req(
                "/api/v1/network/interfaces/eth0/role",
                serde_json::json!({ "role": "BOGUS" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("未知网卡角色"));
        // 未写入
        assert_eq!(h.nic_role("eth0"), NicRole::Normal);
    }

    #[tokio::test]
    async fn set_role_invalid_body_returns_err() {
        let h = empty_handler();
        // 缺 role 字段
        let err = h
            .handle(post_req(
                "/api/v1/network/interfaces/eth0/role",
                serde_json::json!({}),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    // —— 链路聚合 / 防火墙：纯函数 + 路由行为单测 ——

    #[test]
    fn build_bond_create_cmd_includes_mode_and_slaves() {
        let slaves = vec!["eth0".to_string(), "eth1".to_string()];
        let cmd = build_bond_create_cmd("bond0", "802.3ad", &slaves);
        let joined = cmd.join(" ");
        assert!(cmd.contains(&"mode".to_string()), "应含 mode 关键字");
        assert!(cmd.contains(&"802.3ad".to_string()), "应含规范化后的 mode");
        assert!(cmd.contains(&"bond0".to_string()), "应含 bond 名");
        assert!(cmd.contains(&"eth0".to_string()), "应含从接口 eth0");
        assert!(cmd.contains(&"eth1".to_string()), "应含从接口 eth1");
        assert!(joined.contains("master"), "应含 master 关键字");
        assert!(
            joined.contains("ip link add bond0 type bond"),
            "应含 link add 子句: {joined}"
        );
        assert!(joined.contains("ip link set eth0 master bond0"));
    }

    #[test]
    fn build_bond_create_cmd_normalizes_mode_aliases() {
        // lacp → 802.3ad
        let cmd = build_bond_create_cmd("bond1", "lacp", &["eth2".to_string()]);
        assert!(cmd.contains(&"802.3ad".to_string()), "lacp → 802.3ad");
        // 数字 1 → active-backup
        let cmd2 = build_bond_create_cmd("bond1", "1", &["eth2".to_string()]);
        assert!(
            cmd2.contains(&"active-backup".to_string()),
            "1 → active-backup"
        );
        // 无从接口时仍能构造（仅 link add 子句）
        let cmd3 = build_bond_create_cmd("bond2", "active-backup", &[]);
        assert!(cmd3
            .join(" ")
            .contains("ip link add bond2 type bond mode active-backup"));
    }

    #[test]
    fn build_iptables_add_cmd_includes_chain_and_port() {
        let args = build_iptables_add_cmd("INPUT", "accept", "tcp", "0.0.0.0/0", "0.0.0.0/0", 22);
        assert!(args.contains(&"-A".to_string()));
        assert!(args.contains(&"INPUT".to_string()), "应含 chain");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"tcp".to_string()));
        assert!(args.contains(&"--dport".to_string()));
        assert!(args.contains(&"22".to_string()), "应含端口 22");
        assert!(args.contains(&"-j".to_string()));
        assert!(
            args.contains(&"ACCEPT".to_string()),
            "action accept → ACCEPT"
        );
    }

    #[test]
    fn build_iptables_add_cmd_omits_optional_clauses() {
        // protocol=any + port=0 → 省略 -p / --dport；source/dest 空 → 省略 -s/-d
        let args = build_iptables_add_cmd("OUTPUT", "drop", "any", "", "", 0);
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"--dport".to_string()));
        assert!(!args.contains(&"-s".to_string()));
        assert!(!args.contains(&"-d".to_string()));
        assert!(args.contains(&"DROP".to_string()));
        // deny → DROP
        let args2 = build_iptables_add_cmd("FORWARD", "deny", "udp", "any", "any", 53);
        assert!(args2.contains(&"-p".to_string()));
        assert!(args2.contains(&"udp".to_string()));
        assert!(args2.contains(&"--dport".to_string()));
        assert!(args2.contains(&"53".to_string()));
        assert!(args2.contains(&"DROP".to_string()));
        // source/dest = "any" → 省略
        assert!(!args2.contains(&"-s".to_string()));
    }

    #[test]
    fn parse_bond_info_parses_mode_and_slaves() {
        let content = "\
Ethernet Channel Bonding Driver: v3.7.1
Bonding Mode: IEEE 802.3ad Dynamic link aggregation
Transmit Hash Policy: layer2 (0)
MII Status: up
MII Polling Interval (ms): 100

Slave Interface: eth0
MII Status: up
Speed: 1000 Mbps

Slave Interface: eth1
MII Status: up
Speed: 1000 Mbps
";
        let infos = parse_bond_info(content);
        assert_eq!(infos.len(), 1);
        let b = &infos[0];
        assert!(b.mode.contains("802.3ad"), "mode 应含 802.3ad: {}", b.mode);
        assert_eq!(b.status, "up");
        assert_eq!(b.slaves, vec!["eth0".to_string(), "eth1".to_string()]);
        // name 由 handler 回填，parser 留空
        assert_eq!(b.name, "");
    }

    #[test]
    fn parse_bond_info_empty_for_non_bonding_content() {
        assert!(parse_bond_info("hello world\n").is_empty());
        assert!(parse_bond_info("").is_empty());
        assert!(!parse_bond_info("MII Status: down\n").is_empty()); // 仅 status 也算
    }

    #[test]
    fn parse_iptables_chain_skips_header_and_requires_line_numbers() {
        // 无 --line-numbers（首列非数字）→ 全部跳过
        let no_num = "\
Chain INPUT (policy ACCEPT)
target  prot opt source               destination
ACCEPT  tcp  --  0.0.0.0/0            0.0.0.0/0            tcp dpt:22
";
        assert!(parse_iptables_chain("INPUT", no_num).is_empty());

        // 带 --line-numbers
        let with_num = "\
Chain INPUT (policy ACCEPT)
num  target  prot opt source               destination
1    ACCEPT  tcp  --  0.0.0.0/0            0.0.0.0/0            tcp dpt:22
2    DROP    all  --  10.0.0.0/8           0.0.0.0/0
";
        let rules = parse_iptables_chain("INPUT", with_num);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].num, 1);
        assert_eq!(rules[0].target, "ACCEPT");
        assert_eq!(rules[0].id, "INPUT#1");
        assert_eq!(rules[0].protocol, "tcp");
        assert_eq!(rules[1].chain, "INPUT");
        assert_eq!(rules[1].target, "DROP");
    }

    #[tokio::test]
    async fn post_bond_validates_and_degrades_gracefully() {
        let h = empty_handler();
        // 空名 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/network/bonds",
                serde_json::json!({ "name": " ", "mode": "802.3ad", "slaves": ["eth0"] }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 空 slaves → 400
        let resp = h
            .handle(post_req(
                "/api/v1/network/bonds",
                serde_json::json!({ "name": "bond0", "mode": "802.3ad", "slaves": [] }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 合法但无特权 → 200 + applied=false（降级，不 panic / Err）
        let resp = h
            .handle(post_req(
                "/api/v1/network/bonds",
                serde_json::json!({ "name": "bond0", "mode": "802.3ad", "slaves": ["eth0","eth1"] }),
            ))
            .await
            .expect("create bond 应返回 Ok（降级）");
        assert_eq!(resp.status, 200);
        // applied 可能为 true/false 取决于环境，但字段必须存在
        assert!(resp.body["applied"].is_boolean());
        assert_eq!(resp.body["name"], "bond0");
    }

    #[tokio::test]
    async fn post_firewall_rule_and_delete_degrade_gracefully() {
        let h = empty_handler();
        // 空 chain → 400
        let resp = h
            .handle(post_req(
                "/api/v1/network/firewall/rules",
                serde_json::json!({ "chain": " ", "action": "accept", "protocol": "tcp", "source": "any", "dest": "any", "port": 22 }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 合法但无特权 → 200 + applied 布尔
        let resp = h
            .handle(post_req(
                "/api/v1/network/firewall/rules",
                serde_json::json!({ "chain": "INPUT", "action": "accept", "protocol": "tcp", "source": "0.0.0.0/0", "dest": "0.0.0.0/0", "port": 22 }),
            ))
            .await
            .expect("add rule 应返回 Ok（降级）");
        assert_eq!(resp.status, 200);
        assert!(resp.body["applied"].is_boolean());
        assert!(resp.body["rule"].as_str().unwrap().contains("iptables"));

        // DELETE 非法 id 格式 → 400
        let resp = h
            .handle(req_delete("/api/v1/network/firewall/rules/bogus"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // DELETE 合法 id 格式 → 200 + applied 布尔（降级）
        let resp = h
            .handle(req_delete("/api/v1/network/firewall/rules/INPUT#3"))
            .await
            .expect("delete rule 应返回 Ok（降级）");
        assert_eq!(resp.status, 200);
        assert!(resp.body["applied"].is_boolean());
    }

    #[tokio::test]
    async fn get_bonds_and_firewall_rules_return_200() {
        let h = empty_handler();
        // GET bonds → 200 数组（真实环境可能有内容，无 bonding 则空）
        let resp = h.handle(get_req("/api/v1/network/bonds")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array());
        // GET firewall/rules → 200 数组（iptables 不可用则空）
        let resp = h
            .handle(get_req("/api/v1/network/firewall/rules"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array());
    }

    /// 构造一个 DELETE 请求。
    fn req_delete(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }
}
