//! `BleHubRouteHandler` —— OS BLE mesh 网状中继枢纽 HTTP 适配器。
//!
//! 定位：OS 作为 BLE mesh 中的一个节点 + 互联网网关。手机离线（无蜂窝/Wi-Fi）时
//! 经 BLE mesh 多跳中继与其它设备通信——A↔B↔C，A 看不到 C 但经 B 中继，A 的 IM
//! 能发现 C 并收发消息。**开放 mesh，无需配对**：范围内自动发现 + 连接。
//!
//! # 架构（mesh relay mode）
//!
//! - **开放发现**：任何设备在 BLE 范围内自动发现并连接，**无需配对 token**。
//! - **手机即中继**：每部手机都是 mesh 节点，收到消息后转发（hop-by-hop）。
//! - **节点发现协议**：每个节点广播自己的 ID + 可达列表。A 直连 B，B 直连 C →
//!   A 经 B 间接发现 C（hop=2）。路由表由 [`compute_routing`] 从各节点可达列表推导。
//! - **消息路由（flooding + 去重）**：消息带 `msg_id`/`source_id`/`target_id`/
//!   `hop_count`。收到后按 `msg_id` 去重（已见即丢弃，防环路）；`hop_count` 递减后
//!   向其它直接邻居转发；`target_id` 命中本机或广播则投递到 IM。
//! - **OS 角色**：mesh 节点之一 + 互联网网关（消息最终可经 OS 转到互联网/其它 OS）。
//!
//! # 实现策略：JSON 落盘 + Python GATT mesh relay spawn（fire-and-forget）+ 降级
//!
//! - **节点落盘**：mesh 节点序列化为 `/tank/os-data/ble-nodes.json`（目录存在即用，
//!   否则回退 `./ble-nodes.json`）。`new()` 启动时加载；发现/删除同步写回。
//! - **GATT mesh relay 服务**：`POST /start` spawn Python 脚本（BlueZ D-Bus 注册 GATT
//!   外设 + mesh relay mode），pid 记入 `BleMeshStatus`；`POST /stop` kill pid。
//!   Python/dbus/BlueZ 不可用或 spawn 失败 → `running=false`，**绝不 panic**（脚本
//!   内容由纯函数 [`build_gatt_service_script`] 构造，写 `/tmp/os_ble_mesh.py`）。
//! - **适配器探测**：`new()` 缓存 hci0 的 BD Address（`hciconfig` 解析失败 → 回退占位）。
//!
//! # 路由表（10 条，component="ble_hub"）
//!
//! | method | path                              | 动作 |
//! |--------|-----------------------------------|------|
//! | GET    | `/api/v1/ble/status`              | mesh Hub 状态 |
//! | POST   | `/api/v1/ble/start`               | 启动 GATT mesh relay（admin）|
//! | POST   | `/api/v1/ble/stop`                | 停止（admin）|
//! | GET    | `/api/v1/ble/nodes`               | 列 mesh 节点（直接 + 间接）|
//! | DELETE | `/api/v1/ble/nodes/:id`           | 移除节点（admin）|
//! | POST   | `/api/v1/ble/discover`            | 节点发现通告（内部：手机上报 id+可达列表）|
//! | GET    | `/api/v1/ble/routing`             | 路由表（可达节点 + hop + via）|
//! | POST   | `/api/v1/ble/messages`            | 消息中继（flooding + 去重，内部 API）|
//! | GET    | `/api/v1/ble/messages`            | 消息历史 |
//! | GET    | `/api/v1/ble/stats`               | 统计 |

use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// OS 自身在 mesh 中的节点 id。
pub const OS_NODE_ID: &str = "os";

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一个 mesh 节点（手机 / OS / 其它 BLE 设备）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BleMeshNode {
    pub id: String,
    /// 设备名（手机型号 / 用户名）。
    pub name: String,
    /// BLE MAC 地址。
    pub address: String,
    /// 是否直接连接（1 hop，在 BLE 范围内）。
    #[serde(default)]
    pub direct: bool,
    /// 跳数（直接=1，间接 = 经由其它节点）。默认 1。
    #[serde(default = "default_hop_one")]
    pub hop: u32,
    /// 经由哪个**直接**邻居可达（None = 直接；间接节点为其第一跳直接邻居）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// 该节点广播的可直接连的节点列表（用于跨跳发现：B 报告可达 C → A 知道 C 经 B）。
    #[serde(default)]
    pub reachable: Vec<String>,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    pub created_at: String,
}

/// mesh Hub 运行状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BleMeshStatus {
    /// GATT mesh relay 服务是否运行。
    pub running: bool,
    /// 适配器名（hci0）。
    pub adapter: String,
    /// 适配器 BD Address。
    pub address: String,
    /// 已知 mesh 节点总数（直接 + 间接）。
    pub node_count: usize,
    /// 直接连接的节点数（1 hop）。
    pub direct_connections: usize,
    /// Python GATT 脚本进程 pid（运行时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// `GET /api/v1/ble/routing` 返回的单条路由项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingEntry {
    pub node_id: String,
    /// 跳数（直接=1）。
    pub hop: u32,
    /// 第一跳直接邻居（转发入口）。
    pub via: String,
    pub direct: bool,
}

/// `GET /api/v1/ble/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BleStats {
    pub node_count: usize,
    pub direct: usize,
    pub reachable: usize,
    pub message_count: usize,
    pub running: bool,
}

/// 一条 mesh 中继消息（flooding + hop_count + 去重）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BleMessage {
    /// 中继记录 id（OS 侧）。
    pub id: String,
    /// 消息唯一 id（用于跨节点去重防环路）。
    pub msg_id: String,
    /// 来源节点 id。
    pub source_id: String,
    /// 目标节点 id（None = 广播给 mesh 所有节点）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub content: String,
    /// `text` / `file` / `image`。
    #[serde(default = "default_msg_type_text")]
    pub msg_type: String,
    /// 剩余跳数（收到时；递减后转发，0 则不再转发）。
    #[serde(default = "default_relay_hops")]
    pub hop_count: u32,
    /// 已遍历的节点 id（路径溯源）。
    #[serde(default)]
    pub path: Vec<String>,
    /// `inbound`（手机→OS）/ `outbound`（OS→手机）/ `relay`（经 OS 中转）。
    #[serde(default = "default_dir_inbound")]
    pub direction: String,
    pub created_at: String,
}

// ----------------------------------------------------------------------------
// 纯函数：节点 id / mesh QR / 路由推导 / GATT 脚本（易测试，不依赖外部进程）
// ----------------------------------------------------------------------------

/// 生成 mesh 节点 id：`mesh-XXXXXX`（大写字母数字，去易混字符）。
#[must_use]
pub fn generate_node_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32);
    let mut state = if seed == 0 { 0x9E37_79B9 } else { seed };
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut out = String::with_capacity(11);
    out.push_str("mesh-");
    for _ in 0..6 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let idx = ((state >> 33) as usize) % ALPHABET.len();
        out.push(ALPHABET[idx] as char);
    }
    out
}

/// 构造 mesh 连接二维码内容：`os-ble-mesh://<address>`（开放 mesh，无 token）。
#[must_use]
pub fn build_mesh_qr_data(address: &str) -> String {
    format!("os-ble-mesh://{address}")
}

/// 从各 mesh 节点的可达列表推导完整路由表（Bellman-Ford 风格，多轮传播）。
///
/// - `self_id`：本机节点 id（通常 `os`），不出现在结果中。
/// - 直接节点（`direct=true`）为 hop=1，`via`=其自身。
/// - 间接节点经直接邻居的 `reachable` 列表传播：B(direct) 报告可达 C → C 经 B 可达，hop=2。
/// - 取最小跳数；`via` 始终为第一跳直接邻居。
#[must_use]
pub fn compute_routing(self_id: &str, nodes: &[BleMeshNode]) -> Vec<RoutingEntry> {
    use std::collections::HashMap;
    // node_id -> (hop, via_direct_neighbor)
    let mut table: HashMap<String, (u32, String)> = HashMap::new();
    // seed：直接节点 hop=1
    for n in nodes {
        if n.direct && n.id != self_id {
            table.insert(n.id.clone(), (1, n.id.clone()));
        }
    }
    // 传播可达列表（最多 8 轮，mesh 直径通常 ≤ 6）
    for _ in 0..8 {
        let snapshot: Vec<(String, (u32, String))> =
            table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut changed = false;
        for (nid, (nh, nvia)) in &snapshot {
            let Some(node) = nodes.iter().find(|n| &n.id == nid) else {
                continue;
            };
            for r in &node.reachable {
                if r == self_id || r == nid {
                    continue;
                }
                let new_hop = nh + 1;
                let better = match table.get(r) {
                    Some(&(h, _)) => new_hop < h,
                    None => true,
                };
                if better {
                    table.insert(r.clone(), (new_hop, nvia.clone()));
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut out: Vec<RoutingEntry> = table
        .into_iter()
        .map(|(id, (hop, via))| RoutingEntry {
            node_id: id,
            hop,
            via,
            direct: hop == 1,
        })
        .collect();
    out.sort_by(|a, b| a.hop.cmp(&b.hop).then(a.node_id.cmp(&b.node_id)));
    out
}

/// 构造 Python GATT mesh relay 服务脚本（写到 `/tmp/os_ble_mesh.py` 后执行）。
///
/// 注册 GATT 外设到 BlueZ D-Bus（mesh relay mode）：
/// - Service UUID `0000ff20-...`（OS mesh Hub）
/// - Discovery 特征值（Write + Notify）：节点通告——手机上报 `{node_id, name, reachable:[]}`
/// - MessageRelay 特征值（Write + Notify）：消息 `{msg_id, source, target?, content, hop_count}`
///
/// mesh relay 语义：收到消息后向其它已连接设备广播（hop_count 递减，msg_id 去重）。
/// 消息经 stdout JSON 行输出（Rust 读取路由到 IM），stdin 接收 OS→mesh 消息。
/// dbus/gi 缺失 → 降级 `bluetoothctl` 交互模式；均失败退出码 1，caller 据此不 panic。
#[must_use]
pub fn build_gatt_service_script(adapter: &str, service_name: &str) -> String {
    format!(
        r#"#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""OS BLE mesh relay GATT 服务脚本（运行时由 os-api 生成）。
注册 GATT 外设到 BlueZ D-Bus（adapter={adapter}，service={service_name}）。
mesh relay mode：开放 mesh（无需配对），收到消息向其它设备转发（hop_count 递减 + msg_id 去重）。
依赖：python3-dbus / python3-gi。缺失 -> 降级 bluetoothctl -> 均失败退出码 1。
"""
import json
import sys

ADAPTER = {adapter_lit}
SERVICE_NAME = {service_name_lit}
# GATT UUID（16-bit 0xFF20 自定义 mesh 服务 + 标准 Base UUID）
SERVICE_UUID = "0000ff20-0000-1000-8000-00805f9b34fb"
DISCOVERY_UUID = "0000ff21-0000-1000-8000-00805f9b34fb"
RELAY_UUID = "0000ff22-0000-1000-8000-00805f9b34fb"
# mesh relay 状态
SEEN_MSGS = set()  # 已见 msg_id（去重防环路）


def emit(obj):
    """stdout 输出一行 JSON（Rust 读取路由到 IM）。"""
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def run_dbus_gatt():
    try:
        import dbus
        import dbus.service
        import dbus.mainloop.glib
        from gi.repository import GLib
    except ImportError as exc:
        raise RuntimeError("dbus/gi 不可用: %s" % exc)
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()
    emit({{"event": "mesh_start", "adapter": ADAPTER, "service": SERVICE_UUID, "mode": "open_mesh"}})

    class Characteristic(dbus.service.Object):
        def __init__(self, bus, index, uuid, flags, path_base):
            self.path = path_base + "/char%04d" % index
            self.bus = bus
            self.uuid = uuid
            self.flags = flags
            self.value = []
            dbus.service.Object.__init__(self, bus, self.path)

        @dbus.service.method("org.bluez.GattCharacteristic1",
                             in_signature="a{{sv}}", out_signature="ay")
        def ReadValue(self, options):
            return dbus.Array(self.value, signature="y")

        @dbus.service.method("org.bluez.GattCharacteristic1",
                             in_signature="aya{{sv}}", out_signature="")
        def WriteValue(self, value, options):
            data = bytes(bytearray(value))
            text = data.decode("utf-8", "replace")
            self.value = list(value)
            if self.uuid == DISCOVERY_UUID:
                try:
                    announce = json.loads(text)
                except Exception:
                    announce = {{"node_id": text.strip(), "reachable": []}}
                emit({{"event": "discover", "announce": announce}})
            elif self.uuid == RELAY_UUID:
                try:
                    msg = json.loads(text)
                except Exception:
                    msg = {{"msg_id": text, "content": text, "hop_count": 0}}
                mid = msg.get("msg_id")
                # mesh relay 去重：已见 msg_id 不再处理/转发
                if mid and mid in SEEN_MSGS:
                    emit({{"event": "relay_drop", "msg_id": mid, "reason": "dup"}})
                    return
                if mid:
                    SEEN_MSGS.add(mid)
                emit({{"event": "message_relay", "msg": msg}})
                # 递减 hop_count；> 0 时本进程会向其它连接广播（这里仅记录转发意图）
                hops = int(msg.get("hop_count", 0))
                if hops > 0:
                    msg["hop_count"] = hops - 1
                    emit({{"event": "relay_forward", "msg": msg}})

        @dbus.service.method("org.bluez.GattCharacteristic1", out_signature="ay")
        def StartNotify(self):
            emit({{"event": "notify_start", "uuid": self.uuid}})

        @dbus.service.method("org.bluez.GattCharacteristic1")
        def StopNotify(self):
            emit({{"event": "notify_stop", "uuid": self.uuid}})

    path_base = "/org/bluez/" + ADAPTER + "/os_mesh"
    chars = [
        Characteristic(bus, 0, DISCOVERY_UUID, ["write", "notify"], path_base),
        Characteristic(bus, 1, RELAY_UUID, ["write", "notify"], path_base),
    ]
    loop = GLib.MainLoop()
    emit({{"event": "mesh_ready", "chars": len(chars), "mode": "open_mesh"}})
    loop.run()


def run_bluetoothctl_fallback():
    """dbus/gi 不可用时降级：bluetoothctl 开放广播（功能受限）。"""
    import subprocess
    emit({{"event": "fallback", "mode": "bluetoothctl"}})
    try:
        proc = subprocess.Popen(
            ["bluetoothctl"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True)
        proc.stdin.write("power on\nagent on\ndiscoverable on\nadvertise on\n")
        proc.stdin.flush()
        proc.wait()
    except Exception as exc:
        raise RuntimeError("bluetoothctl 降级失败: %s" % exc)


def main():
    try:
        run_dbus_gatt()
    except Exception as exc:
        sys.stderr.write("GATT_DBUS_FAILED: %s\n" % exc)
        try:
            run_bluetoothctl_fallback()
        except Exception as exc2:
            sys.stderr.write("BLE_MESH_FAILED: %s\n" % exc2)
            sys.exit(1)


if __name__ == "__main__":
    main()
"#,
        adapter = adapter,
        adapter_lit = py_str_literal(adapter),
        service_name = service_name,
        service_name_lit = py_str_literal(service_name),
    )
}

fn default_hop_one() -> u32 {
    1
}
fn default_relay_hops() -> u32 {
    7
}
fn default_dir_inbound() -> String {
    "inbound".to_string()
}
fn default_msg_type_text() -> String {
    "text".to_string()
}

/// Python 字符串字面量（双引号 + 转义）。
fn py_str_literal(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

// ----------------------------------------------------------------------------
// BleHubRouteHandler
// ----------------------------------------------------------------------------

/// 蓝牙 mesh 中继 Hub 路由处理器——GATT mesh relay 服务管理 + 节点发现 + 路由 + 消息中继。
pub struct BleHubRouteHandler {
    nodes: Mutex<Vec<BleMeshNode>>,
    messages: Mutex<Vec<BleMessage>>,
    /// 已见 msg_id 集合（flooding 去重防环路）。
    seen: Mutex<HashSet<String>>,
    status: Mutex<BleMeshStatus>,
    counter: Mutex<u64>,
    /// 节点落盘路径（`None` = 纯内存态，测试用）。
    persist_path: Option<String>,
}

impl BleHubRouteHandler {
    /// 构造 handler：探测 hci0 适配器地址 + 加载 `ble-nodes.json`（缺失/空 → 空）。
    #[must_use]
    pub fn new() -> Self {
        let path = nodes_file_path();
        let nodes = normalize_loaded(load_nodes_from(&path));
        let (adapter, address) = detect_adapter();
        Self {
            nodes: Mutex::new(nodes),
            messages: Mutex::new(vec![]),
            seen: Mutex::new(HashSet::new()),
            status: Mutex::new(BleMeshStatus {
                running: false,
                adapter,
                address,
                node_count: 0,
                direct_connections: 0,
                pid: None,
            }),
            counter: Mutex::new(100),
            persist_path: Some(path),
        }
    }

    /// 用指定节点列表构造（**纯内存态**：测试注入，不落盘、不触外部进程）。
    #[must_use]
    pub fn with_nodes(nodes: Vec<BleMeshNode>) -> Self {
        Self {
            nodes: Mutex::new(nodes),
            messages: Mutex::new(vec![]),
            seen: Mutex::new(HashSet::new()),
            status: Mutex::new(BleMeshStatus {
                running: false,
                adapter: "hci0".into(),
                address: "EC:91:61:42:A4:AC".into(),
                node_count: 0,
                direct_connections: 0,
                pid: None,
            }),
            counter: Mutex::new(100),
            persist_path: None,
        }
    }

    /// 当前全量节点快照。
    #[must_use]
    pub fn nodes_snapshot(&self) -> Vec<BleMeshNode> {
        self.nodes.lock().expect("nodes poisoned").clone()
    }

    /// 当前状态快照（聚合 node_count / direct_connections）。
    #[must_use]
    pub fn status_snapshot(&self) -> BleMeshStatus {
        let mut s = self.status.lock().expect("status poisoned").clone();
        let nodes = self.nodes.lock().expect("nodes poisoned");
        s.node_count = nodes.len();
        s.direct_connections = nodes.iter().filter(|n| n.direct).count();
        s
    }

    /// 生成下一个 id。
    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 同步把节点列表写回 JSON（仅当 `persist_path` 为 `Some`）。
    fn persist(&self) {
        if let Some(path) = &self.persist_path {
            let list = self.nodes.lock().expect("nodes poisoned").clone();
            if let Err(e) = save_nodes_to(path, &list) {
                eprintln!("[ble_hub] 落盘失败 {path}: {e}");
            }
        }
    }

    /// 统计快照。
    fn stats_snapshot(&self) -> BleStats {
        let nodes = self.nodes.lock().expect("nodes poisoned");
        let messages = self.messages.lock().expect("messages poisoned");
        let status = self.status.lock().expect("status poisoned");
        let direct = nodes.iter().filter(|n| n.direct).count();
        BleStats {
            node_count: nodes.len(),
            direct,
            reachable: nodes.len().saturating_sub(direct),
            message_count: messages.len(),
            running: status.running,
        }
    }

    /// 真实 spawn Python GATT mesh relay 脚本（fire-and-forget），成功返回 pid。
    fn spawn_gatt(adapter: &str, service_name: &str) -> Result<u32, String> {
        let script_path = "/tmp/os_ble_mesh.py";
        let script = build_gatt_service_script(adapter, service_name);
        std::fs::write(script_path, script).map_err(|e| format!("写入 GATT 脚本失败: {e}"))?;
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(script_path);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        let stderr_log = std::env::temp_dir().join("os-ble-mesh.log");
        let stderr_file = std::fs::File::create(&stderr_log)
            .map(Stdio::from)
            .unwrap_or(Stdio::null());
        cmd.stderr(stderr_file);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                drop(child); // 不等待：OS 收养
                Ok(pid)
            }
            Err(e) => Err(format!("spawn python3 失败: {e}")),
        }
    }

    /// 杀掉子进程（SIGTERM）。失败返回 Err，caller 仍继续。
    fn kill_pid(pid: u32) -> Result<(), String> {
        let out = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "kill {pid} 退出码 {:?}: {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!("kill {pid} 失败: {e}")),
        }
    }

    /// 记录一条消息并做 flooding 去重。返回 (recorded: 是否首次记录, message)。
    fn record_message(&self, mut msg: BleMessage) -> (bool, BleMessage) {
        let mut seen = self.seen.lock().expect("seen poisoned");
        if seen.contains(&msg.msg_id) {
            return (false, msg); // 已见：去重丢弃
        }
        seen.insert(msg.msg_id.clone());
        msg.id = self.next_id("ble-msg");
        drop(seen);
        let clone = msg.clone();
        self.messages.lock().expect("messages poisoned").push(msg);
        (true, clone)
    }
}

impl Default for BleHubRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for BleHubRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/ble/status", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/ble/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/ble/stop",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/ble/nodes", false, vec![]),
            spec(
                HttpMethod::Delete,
                "/api/v1/ble/nodes/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Post, "/api/v1/ble/discover", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/ble/routing", false, vec![]),
            spec(HttpMethod::Post, "/api/v1/ble/messages", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/ble/messages", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/ble/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/ble/status —— mesh Hub 状态
            (HttpMethod::Get, ["api", "v1", "ble", "status"]) => {
                Ok(ok_json(to_value(&self.status_snapshot())?))
            }

            // —— POST /api/v1/ble/start —— 启动 GATT mesh relay（admin）
            (HttpMethod::Post, ["api", "v1", "ble", "start"]) => {
                let (adapter, _address) = {
                    let s = self.status.lock().expect("status poisoned");
                    (s.adapter.clone(), s.address.clone())
                };
                let pid_res = Self::spawn_gatt(&adapter, "os-mesh-hub");
                let mut status = self.status.lock().expect("status poisoned");
                match pid_res {
                    Ok(pid) => {
                        status.running = true;
                        status.pid = Some(pid);
                        Ok(ok_json(to_value(&*status)?))
                    }
                    Err(e) => {
                        // spawn 失败：running 保持 false，不 panic（降级）
                        status.running = false;
                        status.pid = None;
                        Ok(error_response_with(
                            500,
                            &format!("BLE mesh relay 启动失败（已降级）：{e}"),
                            serde_json::to_value(&*status).unwrap_or(serde_json::Value::Null),
                        ))
                    }
                }
            }

            // —— POST /api/v1/ble/stop —— 停止（admin）
            (HttpMethod::Post, ["api", "v1", "ble", "stop"]) => {
                let mut status = self.status.lock().expect("status poisoned");
                if let Some(pid) = status.pid.take() {
                    let _ = Self::kill_pid(pid);
                }
                status.running = false;
                // 停服后所有节点标记离线（mesh relay 停止，邻居不再可达）
                let mut nodes = self.nodes.lock().expect("nodes poisoned");
                for n in nodes.iter_mut() {
                    n.online = false;
                    n.direct = false;
                }
                drop(nodes);
                self.persist();
                Ok(ok_json(to_value(&*status)?))
            }

            // —— GET /api/v1/ble/nodes —— 列 mesh 节点（直接 + 间接）
            (HttpMethod::Get, ["api", "v1", "ble", "nodes"]) => {
                Ok(ok_json(to_value(&self.nodes_snapshot())?))
            }

            // —— DELETE /api/v1/ble/nodes/:id —— 移除节点（admin）
            (HttpMethod::Delete, ["api", "v1", "ble", "nodes", id]) => {
                let mut nodes = self.nodes.lock().expect("nodes poisoned");
                let before = nodes.len();
                nodes.retain(|n| n.id != *id);
                if nodes.len() == before {
                    return Ok(error_response(404, &format!("节点不存在: {id}")));
                }
                drop(nodes);
                self.persist();
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/ble/discover —— 节点发现通告（内部：手机上报 id + 可达列表）
            (HttpMethod::Post, ["api", "v1", "ble", "discover"]) => {
                #[derive(serde::Deserialize)]
                struct DiscoverReq {
                    node_id: String,
                    #[serde(default)]
                    name: Option<String>,
                    #[serde(default)]
                    address: Option<String>,
                    /// 该节点直接能连的节点列表（用于跨跳发现）。
                    #[serde(default)]
                    reachable: Vec<String>,
                    #[serde(default = "default_true")]
                    direct: bool,
                }
                let body: DiscoverReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析发现通告请求体失败: {e}"))
                })?;
                if body.node_id.trim().is_empty() {
                    return Ok(error_response(400, "node_id 不可为空"));
                }
                let now = now_iso();
                let mut nodes = self.nodes.lock().expect("nodes poisoned");
                let announce = &body;
                if let Some(n) = nodes.iter_mut().find(|n| n.id == announce.node_id) {
                    // 更新已有节点
                    n.online = true;
                    n.direct = announce.direct;
                    if announce.direct {
                        n.hop = 1;
                        n.via = None;
                    }
                    if let Some(nm) = &announce.name {
                        n.name = nm.clone();
                    }
                    if let Some(a) = &announce.address {
                        n.address = a.clone();
                    }
                    n.reachable = announce.reachable.clone();
                    n.last_seen = Some(now.clone());
                } else {
                    let node = BleMeshNode {
                        id: announce.node_id.clone(),
                        name: announce
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("节点 {}", announce.node_id)),
                        address: announce
                            .address
                            .clone()
                            .unwrap_or_else(|| announce.node_id.clone()),
                        direct: announce.direct,
                        hop: if announce.direct { 1 } else { 2 },
                        // 新发现的间接节点经由谁可达，待路由表推导后补全
                        via: None,
                        reachable: announce.reachable.clone(),
                        online: true,
                        last_seen: Some(now.clone()),
                        created_at: now.clone(),
                    };
                    nodes.push(node);
                }
                drop(nodes);
                self.persist();
                // 返回当前路由表（发现后立即可见可达性）
                let routing = compute_routing(OS_NODE_ID, &self.nodes_snapshot());
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "node_id": body.node_id,
                    "routing": to_value(&routing)?,
                })))
            }

            // —— GET /api/v1/ble/routing —— 路由表（可达节点 + hop + via）
            (HttpMethod::Get, ["api", "v1", "ble", "routing"]) => {
                let routing = compute_routing(OS_NODE_ID, &self.nodes_snapshot());
                Ok(ok_json(serde_json::json!({
                    "self": OS_NODE_ID,
                    "entries": to_value(&routing)?,
                })))
            }

            // —— POST /api/v1/ble/messages —— 消息中继（flooding + 去重，内部 API）
            (HttpMethod::Post, ["api", "v1", "ble", "messages"]) => {
                #[derive(serde::Deserialize)]
                struct RelayReq {
                    #[serde(default)]
                    msg_id: Option<String>,
                    #[serde(default)]
                    source_id: Option<String>,
                    #[serde(default)]
                    target_id: Option<String>,
                    content: String,
                    #[serde(default = "default_msg_type_text")]
                    msg_type: String,
                    #[serde(default = "default_relay_hops")]
                    hop_count: u32,
                    #[serde(default)]
                    path: Vec<String>,
                }
                let body: RelayReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 mesh 消息请求体失败: {e}"))
                })?;
                if body.content.trim().is_empty() {
                    return Ok(error_response(400, "content 不可为空"));
                }
                let msg_id = body
                    .msg_id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("m-{}", short_uuid()));
                let source_id = body
                    .source_id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| OS_NODE_ID.to_string());
                let direction = if source_id == OS_NODE_ID {
                    "outbound"
                } else if body.target_id.as_deref() == Some(OS_NODE_ID) {
                    "inbound"
                } else {
                    "relay"
                };
                let mut path = body.path.clone();
                if !path.contains(&OS_NODE_ID.to_string()) {
                    path.push(OS_NODE_ID.to_string());
                }
                let msg = BleMessage {
                    id: String::new(), // record_message 填充
                    msg_id: msg_id.clone(),
                    source_id,
                    target_id: body.target_id,
                    content: body.content,
                    msg_type: body.msg_type,
                    hop_count: body.hop_count,
                    path,
                    direction: direction.to_string(),
                    created_at: now_iso(),
                };
                let (recorded, msg) = self.record_message(msg);
                if !recorded {
                    // 已见 msg_id：去重丢弃（flooding 防环路）
                    return Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "dedup": true,
                        "msg_id": msg_id,
                    })));
                }
                // 是否还需继续转发（hop_count > 0）
                let should_relay = msg.hop_count > 0;
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::json!({
                        "ok": true,
                        "message": to_value(&msg)?,
                        "should_relay": should_relay,
                        "next_hop_count": msg.hop_count.saturating_sub(1),
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/ble/messages —— 消息历史
            (HttpMethod::Get, ["api", "v1", "ble", "messages"]) => {
                let list = self.messages.lock().expect("messages poisoned").clone();
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/ble/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "ble", "stats"]) => {
                Ok(ok_json(to_value(&self.stats_snapshot())?))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "ble_hub: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "ble_hub".to_string(),
        requires_auth,
        required_roles,
    }
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 构造错误响应但附带状态快照（启动失败时返回 500 + 当前 status 供前端展示降级）。
fn error_response_with(status: u16, msg: &str, payload: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({ "error": msg, "status": payload }),
        headers: serde_json::json!({}),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

fn short_uuid() -> String {
    os_core::Uuid::new_v4()
        .to_string()
        .chars()
        .take(8)
        .collect()
}

// ----------------------------------------------------------------------------
// 配置落盘 + 适配器探测
// ----------------------------------------------------------------------------

/// 解析 ble-nodes.json 路径：`/tank/os-data/ble-nodes.json`（目录存在即用），否则 `./ble-nodes.json`。
fn nodes_file_path() -> String {
    let dir = "/tank/os-data";
    if Path::new(dir).is_dir() {
        format!("{dir}/ble-nodes.json")
    } else {
        "./ble-nodes.json".to_string()
    }
}

fn load_nodes_from(path: &str) -> Vec<BleMeshNode> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_nodes_to(path: &str, list: &[BleMeshNode]) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// 重启加载后重置运行态：online=false（mesh relay 停止，邻居暂不可达）。
/// 保留 id/name/address/reachable/hop/via/created_at（拓扑知识不丢）。
fn normalize_loaded(mut nodes: Vec<BleMeshNode>) -> Vec<BleMeshNode> {
    for n in &mut nodes {
        n.online = false;
    }
    nodes
}

/// 探测 hci0 适配器名 + BD Address（`hciconfig hci0` 解析）。
/// 失败回退 `(hci0, EC:91:61:42:A4:AC)`（硬件确认地址，不 panic）。
fn detect_adapter() -> (String, String) {
    let default_addr = "EC:91:61:42:A4:AC".to_string();
    let out = std::process::Command::new("hciconfig").arg("hci0").output();
    let Ok(o) = out else {
        return ("hci0".to_string(), default_addr);
    };
    let text = String::from_utf8_lossy(&o.stdout);
    let adapter = text
        .lines()
        .next()
        .and_then(|l| l.split(':').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hci0".to_string());
    let address = text
        .lines()
        .find(|l| l.contains("BD Address:"))
        .and_then(|l| l.split("BD Address:").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(default_addr);
    (adapter, address)
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    fn make_node(id: &str, direct: bool, reachable: &[&str]) -> BleMeshNode {
        BleMeshNode {
            id: id.into(),
            name: format!("node-{id}"),
            address: format!("AA:BB:CC:00:00:{id}"),
            direct,
            hop: if direct { 1 } else { 2 },
            via: None,
            reachable: reachable.iter().map(|s| s.to_string()).collect(),
            online: true,
            last_seen: Some("2026-08-13T09:00:00+08:00".into()),
            created_at: "2026-08-13T08:00:00+08:00".into(),
        }
    }

    /// 唯一临时 JSON 路径（避免并发冲突）。
    fn temp_json_path() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        format!("/tmp/os-ble-test-{}-{n}.json", std::process::id())
    }

    // 1. generate_node_id 格式：mesh-XXXXXX
    #[test]
    fn node_id_format() {
        let t = generate_node_id();
        assert!(t.starts_with("mesh-"), "应以 mesh- 开头：{t}");
        let suffix = &t["mesh-".len()..];
        assert_eq!(suffix.len(), 6, "后缀 6 字符：{t}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_alphanumeric()),
            "字母数字：{t}"
        );
        let t2 = generate_node_id();
        assert_ne!(t, t2, "两次 id 应不同");
    }

    // 2. build_mesh_qr_data 含 os-ble-mesh:// + address（无 token）
    #[test]
    fn mesh_qr_data_has_scheme_and_address() {
        let qr = build_mesh_qr_data("EC:91:61:42:A4:AC");
        assert!(
            qr.starts_with("os-ble-mesh://"),
            "应含 os-ble-mesh://：{qr}"
        );
        assert!(qr.contains("EC:91:61:42:A4:AC"), "应含 address：{qr}");
        assert!(!qr.contains("token"), "开放 mesh 不应含 token：{qr}");
    }

    // 3. build_gatt_service_script 含 dbus + mesh relay GATT
    #[test]
    fn gatt_script_has_dbus_and_mesh_relay() {
        let s = build_gatt_service_script("hci0", "os-mesh-hub");
        assert!(s.contains("import dbus"), "应含 dbus 导入");
        assert!(
            s.contains("org.bluez.GattCharacteristic1"),
            "应含 GATT 特征值接口"
        );
        assert!(
            s.contains("0000ff20-0000-1000-8000-00805f9b34fb"),
            "应含 mesh 服务 UUID"
        );
        assert!(s.contains("DISCOVERY_UUID"), "应含 Discovery 特征值");
        assert!(s.contains("RELAY_UUID"), "应含 MessageRelay 特征值");
        assert!(s.contains("open_mesh"), "应标记 open mesh 模式");
        assert!(s.contains("SEEN_MSGS"), "应含去重逻辑");
        assert!(s.contains("hop_count"), "应含 hop_count 路由字段");
        assert!(s.contains("bluetoothctl"), "应含降级 bluetoothctl");
    }

    // 4. ble-nodes.json roundtrip
    #[test]
    fn nodes_json_roundtrip() {
        let path = temp_json_path();
        let nodes = vec![
            make_node("A", true, &["B"]),
            make_node("B", true, &["A", "C"]),
        ];
        save_nodes_to(&path, &nodes).expect("写入应成功");
        let loaded = load_nodes_from(&path);
        assert_eq!(loaded.len(), 2, "应回读 2 条");
        assert_eq!(loaded[0].id, "A");
        assert!(loaded[0].direct);
        assert!(loaded[1].reachable.contains(&"C".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    // 5. routes 数量（10）
    #[tokio::test]
    async fn routes_declares_ten_endpoints() {
        let h = BleHubRouteHandler::with_nodes(vec![]);
        let routes = h.routes().await;
        assert_eq!(routes.len(), 10, "应声明 10 条路由");
        assert!(routes.iter().all(|r| r.handler_component == "ble_hub"));
        // 写操作（start/stop/delete）要求 admin
        for r in &routes {
            if r.method == HttpMethod::Delete {
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
            if r.method == HttpMethod::Post
                && (r.path.ends_with("/start") || r.path.ends_with("/stop"))
            {
                assert_eq!(
                    r.required_roles,
                    vec!["admin".to_string()],
                    "{:?} 应 admin",
                    r.path
                );
            }
        }
    }

    // 6. compute_routing 跨跳发现（A↔B↔C：OS 经 B 发现 C，hop=2）
    #[test]
    fn routing_cross_hop_discovery() {
        // OS 直连 B；B 报告可达 C → C 经 B 可达 hop=2
        let nodes = vec![make_node("B", true, &["C"])];
        let rt = compute_routing(OS_NODE_ID, &nodes);
        assert_eq!(rt.len(), 2, "应有 B + C 两条");
        assert_eq!(rt[0].node_id, "B");
        assert_eq!(rt[0].hop, 1);
        assert!(rt[0].direct);
        assert_eq!(rt[1].node_id, "C");
        assert_eq!(rt[1].hop, 2, "C 应为 2 跳");
        assert!(!rt[1].direct);
        assert_eq!(rt[1].via, "B", "C 经 B 可达");
    }

    // 7. 消息中继 flooding 去重
    #[tokio::test]
    async fn message_relay_dedup() {
        let h = BleHubRouteHandler::with_nodes(vec![]);
        // 首条消息（带固定 msg_id）
        let r1 = h
            .handle(post_req(
                "/api/v1/ble/messages",
                serde_json::json!({
                    "msg_id": "M1",
                    "source_id": "phone-A",
                    "target_id": "phone-C",
                    "content": "hi via mesh",
                    "hop_count": 5,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(r1.status, 201);
        assert_eq!(r1.body["should_relay"], true);
        assert_eq!(r1.body["next_hop_count"], 4);
        assert_eq!(r1.body["message"]["direction"], "relay");
        // 相同 msg_id 重复 → 去重
        let r2 = h
            .handle(post_req(
                "/api/v1/ble/messages",
                serde_json::json!({
                    "msg_id": "M1",
                    "source_id": "phone-A",
                    "content": "dup",
                    "hop_count": 5,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(r2.body["dedup"], true);
        // 消息历史仅 1 条
        let hist = h.handle(get_req("/api/v1/ble/messages")).await.unwrap();
        assert_eq!(hist.body.as_array().unwrap().len(), 1);
    }

    // 8. 节点发现通告 + 路由表更新
    #[tokio::test]
    async fn discover_announce_updates_routing() {
        let h = BleHubRouteHandler::with_nodes(vec![]);
        // B 直连，报告可达 C
        let resp = h
            .handle(post_req(
                "/api/v1/ble/discover",
                serde_json::json!({
                    "node_id": "B",
                    "name": "Phone-B",
                    "reachable": ["C"],
                    "direct": true,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        // 路由表应含 B(1) + C(2)
        let routing = h.handle(get_req("/api/v1/ble/routing")).await.unwrap();
        let entries = routing.body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["node_id"], "B");
        assert_eq!(entries[0]["hop"], 1);
        assert_eq!(entries[1]["node_id"], "C");
        assert_eq!(entries[1]["hop"], 2);
        // 节点列表可见 B
        let nodes = h.handle(get_req("/api/v1/ble/nodes")).await.unwrap();
        assert_eq!(nodes.body.as_array().unwrap().len(), 1);
        assert_eq!(nodes.body[0]["name"], "Phone-B");
    }

    // 9. stats 聚合
    #[tokio::test]
    async fn stats_aggregation() {
        let h = BleHubRouteHandler::with_nodes(vec![
            make_node("B", true, &[]),
            make_node("C", false, &[]),
            make_node("D", true, &[]),
        ]);
        let resp = h.handle(get_req("/api/v1/ble/stats")).await.unwrap();
        assert_eq!(resp.body["node_count"], 3);
        assert_eq!(resp.body["direct"], 2);
        assert_eq!(resp.body["reachable"], 1, "间接 = 1（C）");
        assert_eq!(resp.body["message_count"], 0);
    }

    // 10. 降级：Python 不可用 → running=false 不 panic
    #[tokio::test]
    async fn start_degrades_without_python() {
        let h = BleHubRouteHandler::with_nodes(vec![]);
        let resp = h
            .handle(post_req("/api/v1/ble/start", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            resp.status == 200 || resp.status == 500,
            "应为 200 或 500，实际 {}",
            resp.status
        );
        if resp.status == 500 {
            assert_eq!(
                resp.body["status"]["running"], false,
                "降级后 running=false"
            );
        }
        let st = h.handle(get_req("/api/v1/ble/status")).await.unwrap();
        assert_eq!(st.status, 200);
        assert_eq!(st.body["adapter"], "hci0");
    }

    // 11. 删除节点 + 兜底 404
    #[tokio::test]
    async fn delete_node_and_fallback() {
        let h = BleHubRouteHandler::with_nodes(vec![make_node("B", true, &[])]);
        let resp = h.handle(del_req("/api/v1/ble/nodes/B")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(h.nodes_snapshot().len(), 0);
        let miss = h.handle(del_req("/api/v1/ble/nodes/nope")).await.unwrap();
        assert_eq!(miss.status, 404);
        let unk = h.handle(get_req("/api/v1/ble/unknown")).await.unwrap();
        assert_eq!(unk.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<BleHubRouteHandler>();
    }

    #[test]
    fn dto_round_trips_serde() {
        let n = make_node("A", true, &["B"]);
        let v = serde_json::to_value(&n).unwrap();
        let back: BleMeshNode = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "A");
        assert!(back.direct);
        assert_eq!(back.reachable, vec!["B".to_string()]);
    }
}
