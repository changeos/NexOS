//! `NetworkExitRouteHandler` —— WAN 出口共享（component=network-exit）+
//! 防火墙基础的 REST 入口。
//!
//! 定位（2026-08-30 新增，用户定调「在网络管理里增加 wan 的出口设置，增加
//! 类似的功能允许其他节点从本节点做为网络出口，同时完善防火墙的基本功能」）：
//! **overlay 级出口节点**——v2ray 客户端模式（本地 SOCKS5 入口）与 Tailscale
//! exit node（出口声明/授权/订阅）的混合形态，流量经 os-p2p 加密叠加层
//! （直连/打洞/中继）送到出口节点转发。调研对比与架构图见
//! docs/NETWORK_EXIT_RELAY.md。
//!
//! # 两段能力
//!
//! 1. **WAN 出口**（依赖 os-p2p，未启用 503）：出口声明（digest 自广播
//!    `exit_offered` 位，os-p2p meta 扩展字段，其他节点经 gossip 学到「谁是
//!    出口」；本 handler 持久化 offered 状态为权威源，启动时推送给 p2p）+
//!    出口授权（`{NodeID → 过期时刻}` 表，默认 deny、逐节点 TTL——NodeID 经
//!    握手签名验证不可伪造，等价 v2ray 的 UUID 用户鉴权但无需分发秘密）+
//!    数据面（入口节点本地 SOCKS5 `127.0.0.1:11081`，用户浏览器/应用指向它
//!    → overlay 消息 `net_exit`（open/opened/data/ack/close，64KiB 分块 + 每向
//!    8 块窗口背压）→ 出口节点查授权后本机拨本地 SOCKS5 `127.0.0.1:11080`
//!    代拨目标，双向分块回传）。
//! 2. **防火墙**（不依赖 p2p）：规则模型（方向/协议/端口/来源/动作/启用）
//!    持久化 JSON + 翻译成 iptables 自定义链 `NEXOS-FW`（INPUT）/`NEXOS-FW-OUT`
//!    （OUTPUT）落地——flush 先行再注入，不污染用户规则；真实 iptables 经
//!    sudo 子进程（复用 storage 的 sudo 模式），无特权时降级 applied=false。
//!
//! # 路由表
//!
//! | method | path | 鉴权 | 动作 |
//! |--------|------|------|------|
//! | GET    | `/api/v1/net-exit/status` | 公开 | 出口状态全景（offer/授权/已知出口/默认出口/本地 SOCKS 地址） |
//! | POST   | `/api/v1/net-exit/offer` | admin | `{enabled}` 切换本节点出口声明 |
//! | POST   | `/api/v1/net-exit/authorize` | admin | `{node_id, ttl_min}` 授权节点经本节点出网（默认 deny） |
//! | DELETE | `/api/v1/net-exit/authorize/:node_id` | admin | 撤销授权 |
//! | POST   | `/api/v1/net-exit/use` | admin | `{exit_node_id}` 设默认出口 |
//! | POST   | `/api/v1/net-exit/proxy` | admin | `{host, port, exit_node_id?}` 经出口探活一次 |
//! | GET    | `/api/v1/firewall/rules` | 公开 | 规则列表（**空表起步，无 seed 演示数据**） |
//! | POST   | `/api/v1/firewall/rules` | admin | 添加规则（deny in 22 any 需 `force:true`） |
//! | POST   | `/api/v1/firewall/rules/:id/toggle` | admin | 启/停规则 |
//! | DELETE | `/api/v1/firewall/rules/:id` | admin | 删除规则 |
//! | POST   | `/api/v1/firewall/apply` | admin | 规则 → iptables NEXOS-FW[-OUT] 链 |
//! | GET    | `/api/v1/firewall/status` | 公开 | iptables 链实况回读 |
//!
//! # 红线
//!
//! - SOCKS5 双端**只绑 127.0.0.1**（不对外暴露——远端流量走 overlay 消息进
//!   ingress，授权在 overlay 层凭 NodeID 检查，SOCKS5 层无认证）；
//! - 授权默认 deny；
//! - 不动 transfer/provisioning/llm。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Semaphore};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use os_p2p::{Handle as P2pHandle, NodeId};

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "network-exit";
/// 未启用统一文案（503 body 的 error 字段——前端凭此展示开启指引）。
const DISABLED_MSG: &str = "WAN 出口未启用（需 NEXOS_P2P_ENABLE=1 启动组网节点）";

/// overlay 消息命名空间键。
const NS: &str = "net_exit";
/// 消息种类：请求代拨（入口→出口）。
const KIND_OPEN: &str = "open";
/// 代拨成功（出口→入口）。
const KIND_OPENED: &str = "opened";
/// 代拨失败/拒绝（出口→入口）。
const KIND_OPEN_FAILED: &str = "open_failed";
/// 数据分块（双向）。
const KIND_DATA: &str = "data";
/// 分块确认（双向——发送方据此放行窗口，背压）。
const KIND_ACK: &str = "ack";
/// 本地流结束（双向）。
const KIND_CLOSE: &str = "close";

/// 单块数据上限（64KiB——overlay 帧上限 4MiB，base64 后 ~87KB 余量充足）。
const CHUNK: usize = 64 * 1024;
/// 每连接每方向在途分块窗口（背压：对端未 ack 时暂停读本地 socket）。
const WINDOW: usize = 8;
/// 单块窗口等待超时（对端死亡判定）。
const WINDOW_TIMEOUT: Duration = Duration::from_secs(30);
/// open → opened 等待超时（授权拒绝/出口不可达也要在此内应答）。
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// 出口侧 SOCKS5 端口默认值（`NEXOS_EXIT_SOCKS_PORT`）。
pub const EXIT_SOCKS_PORT_DEFAULT: u16 = 11_080;
/// 入口侧 SOCKS5 端口默认值（`NEXOS_EXIT_ENTRY_SOCKS_PORT`）。
pub const ENTRY_SOCKS_PORT_DEFAULT: u16 = 11_081;
/// 防火墙 INPUT 挂接链名（apply flush 先行再注入）。
pub const FW_CHAIN_IN: &str = "NEXOS-FW";
/// 防火墙 OUTPUT 挂接链名（in/out 分链——同链混挂会让 INPUT 语境的 --dport
/// 规则误伤 OUTPUT 流量）。
pub const FW_CHAIN_OUT: &str = "NEXOS-FW-OUT";

/// 当前 unix 秒（观测时间戳；SystemTime 倒拨钳制为 0）。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ============================================================================
// 配置
// ============================================================================

/// network-exit / 防火墙组件配置（env 便捷解析见 [`ExitConfig::from_env`]）。
#[derive(Debug, Clone)]
pub struct ExitConfig {
    /// 出口状态持久化文件（offered / 授权表 / 默认出口）。
    pub state_file: PathBuf,
    /// 防火墙规则持久化文件。
    pub fw_file: PathBuf,
    /// 出口侧本地 SOCKS5 端口（本机 ingress 代拨入口；测试可用 0 取随机端口）。
    pub exit_socks_port: u16,
    /// 入口侧本地 SOCKS5 端口（用户应用指向这里；测试可用 0）。
    pub entry_socks_port: u16,
    /// iptables 前缀程序（生产 `sudo`；测试注入假脚本断言 argv）。
    pub ipt_sudo_bin: String,
}

impl Default for ExitConfig {
    fn default() -> Self {
        Self {
            state_file: PathBuf::from("/tank/os-data/net-exit.json"),
            fw_file: PathBuf::from("/tank/os-data/firewall.json"),
            exit_socks_port: EXIT_SOCKS_PORT_DEFAULT,
            entry_socks_port: ENTRY_SOCKS_PORT_DEFAULT,
            ipt_sudo_bin: "sudo".to_string(),
        }
    }
}

impl ExitConfig {
    /// env 解析：`NEXOS_EXIT_STATE` / `NEXOS_FIREWALL_FILE` /
    /// `NEXOS_EXIT_SOCKS_PORT` / `NEXOS_EXIT_ENTRY_SOCKS_PORT` / `NEXOS_IPT_SUDO`。
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("NEXOS_EXIT_STATE") {
            if !v.trim().is_empty() {
                cfg.state_file = PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("NEXOS_FIREWALL_FILE") {
            if !v.trim().is_empty() {
                cfg.fw_file = PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("NEXOS_EXIT_SOCKS_PORT") {
            if let Ok(p) = v.trim().parse() {
                cfg.exit_socks_port = p;
            }
        }
        if let Ok(v) = std::env::var("NEXOS_EXIT_ENTRY_SOCKS_PORT") {
            if let Ok(p) = v.trim().parse() {
                cfg.entry_socks_port = p;
            }
        }
        if let Ok(v) = std::env::var("NEXOS_IPT_SUDO") {
            if !v.trim().is_empty() {
                cfg.ipt_sudo_bin = v;
            }
        }
        cfg
    }
}

// ============================================================================
// 出口状态（offered / 授权表 / 默认出口）——持久化 JSON
// ============================================================================

/// 单条出口授权（默认 deny——不在表内或已过期即拒绝）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthEntry {
    /// 被授权节点（NodeID hex——握手签名验证的公钥，不可伪造）。
    pub node_id: String,
    /// 授权时刻（unix 秒）。
    pub granted_at: u64,
    /// 过期时刻（unix 秒；到点自动失效）。
    pub expires_at: u64,
}

/// 出口状态持久化单元（`NEXOS_EXIT_STATE`，原子写）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ExitState {
    /// 本节点是否声明可作网络出口（权威源——启动时推送给 os-p2p digest 位）。
    #[serde(default)]
    pub offered: bool,
    /// 授权表（默认空 = 全 deny）。
    #[serde(default)]
    pub authorizations: Vec<AuthEntry>,
    /// 默认出口节点（NodeID hex；None = 未设置）。
    #[serde(default)]
    pub default_exit: Option<String>,
}

impl ExitState {
    /// 授权判定：在表内且未过期（默认 deny）。
    fn is_authorized(&self, node_id: &str, now: u64) -> bool {
        self.authorizations
            .iter()
            .any(|a| a.node_id == node_id && a.expires_at > now)
    }

    /// 未过期的授权节点列表（status 的 `exit_for`）。
    fn active_grantees(&self, now: u64) -> Vec<String> {
        let mut list: Vec<String> = self
            .authorizations
            .iter()
            .filter(|a| a.expires_at > now)
            .map(|a| a.node_id.clone())
            .collect();
        list.sort();
        list.dedup();
        list
    }

    /// 授权/续期（同节点覆盖）。
    fn authorize(&mut self, node_id: &str, ttl_min: u64, now: u64) -> AuthEntry {
        let entry = AuthEntry {
            node_id: node_id.to_string(),
            granted_at: now,
            expires_at: now.saturating_add(ttl_min.saturating_mul(60)),
        };
        self.authorizations.retain(|a| a.node_id != node_id);
        self.authorizations.push(entry.clone());
        entry
    }

    /// 撤销授权（返回是否存在过）。
    fn revoke(&mut self, node_id: &str) -> bool {
        let before = self.authorizations.len();
        self.authorizations.retain(|a| a.node_id != node_id);
        before != self.authorizations.len()
    }
}

/// 原子写 JSON（同目录临时文件 + rename——参考 os-p2p meta 的写法）。
fn write_json_atomic(path: &std::path::Path, v: &impl Serialize) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(v)
        .map_err(|e| std::io::Error::other(format!("序列化失败: {e}")))?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 加载 JSON（缺失/损坏 → 默认值；T: Default）。
fn load_json_or_default<T: for<'de> Deserialize<'de> + Default>(path: &std::path::Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            eprintln!(
                "[network-exit] 状态文件损坏（{}），重建默认值: {e}",
                path.display()
            );
            T::default()
        }),
        Err(_) => T::default(),
    }
}

// ============================================================================
// 防火墙——规则模型 + iptables 链落地
// ============================================================================

/// 防火墙规则（真实数据原则：**空表起步，无 seed 演示数据**；测试填充仅在
/// cfg(test)）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallRule {
    /// 规则 ID（`fw-<n>` 自增）。
    pub id: String,
    /// 方向：`in`（INPUT 链）/ `out`（OUTPUT 链）。
    pub direction: String,
    /// 协议：`tcp` / `udp` / `icmp` / `any`。
    pub proto: String,
    /// 目标端口（None = 不限；icmp/any 无端口）。
    pub port: Option<u16>,
    /// 源 CIDR/IP（`any` = 不限；out 方向语义为「源=本机」时仍可填目标网段校验）。
    pub source: String,
    /// 动作：`allow`（ACCEPT）/ `deny`（DROP）。
    pub action: String,
    /// 是否启用（apply 只翻译 enabled 规则）。
    pub enabled: bool,
    /// 备注。
    pub note: String,
}

/// iptables 单条命令（apply 计划的元素；`fail_ok` = 失败可忽略——如链已存在/
/// jump 已挂接的探测命令）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IptCommand {
    /// 完整 argv（含 `iptables` 程序名，不含 sudo 前缀）。
    pub argv: Vec<String>,
    /// 失败是否可忽略（`-N` 已存在 / `-C` 探测未挂接）。
    pub fail_ok: bool,
}

/// 防火墙规则存储 + iptables 落地器（独立于 p2p——未启用组网也可管理规则）。
pub struct FirewallManager {
    cfg: ExitConfig,
    rules: Mutex<Vec<FirewallRule>>,
    seq: AtomicU64,
}

impl FirewallManager {
    /// 构造（加载持久化文件；缺失/损坏 → 空表）。
    #[must_use]
    pub fn new(cfg: ExitConfig) -> Self {
        let rules = load_json_or_default::<Vec<FirewallRule>>(&cfg.fw_file);
        let seq = rules
            .iter()
            .filter_map(|r| r.id.strip_prefix("fw-").and_then(|n| n.parse::<u64>().ok()))
            .max()
            .unwrap_or(0);
        Self {
            cfg,
            rules: Mutex::new(rules),
            seq: AtomicU64::new(seq),
        }
    }

    /// from_env 构造便捷形态。
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(ExitConfig::from_env())
    }

    fn persist(&self, rules: &[FirewallRule]) {
        if let Err(e) = write_json_atomic(&self.cfg.fw_file, &rules) {
            eprintln!(
                "[network-exit] 防火墙规则落盘失败（{}）: {e}",
                self.cfg.fw_file.display()
            );
        }
    }

    /// 规则快照。
    #[must_use]
    pub fn rules_snapshot(&self) -> Vec<FirewallRule> {
        self.rules.lock().expect("fw rules poisoned").clone()
    }

    /// 添加规则（生成自增 id；**不含危险端口防呆**——调用方 handler 负责）。
    #[allow(clippy::too_many_arguments)]
    pub fn add_rule(
        &self,
        direction: &str,
        proto: &str,
        port: Option<u16>,
        source: &str,
        action: &str,
        enabled: bool,
        note: &str,
    ) -> FirewallRule {
        let id = format!("fw-{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let rule = FirewallRule {
            id,
            direction: direction.to_string(),
            proto: proto.to_string(),
            port,
            source: source.to_string(),
            action: action.to_string(),
            enabled,
            note: note.to_string(),
        };
        let mut rules = self.rules.lock().expect("fw rules poisoned");
        rules.push(rule.clone());
        self.persist(&rules);
        rule
    }

    /// 删除规则（按 id；返回被删的规则）。
    pub fn remove_rule(&self, id: &str) -> Option<FirewallRule> {
        let mut rules = self.rules.lock().expect("fw rules poisoned");
        let idx = rules.iter().position(|r| r.id == id)?;
        let removed = rules.remove(idx);
        self.persist(&rules);
        Some(removed)
    }

    /// 启/停切换（返回更新后的规则）。
    pub fn toggle_rule(&self, id: &str, enabled: bool) -> Option<FirewallRule> {
        let mut rules = self.rules.lock().expect("fw rules poisoned");
        let rule = rules.iter_mut().find(|r| r.id == id)?;
        rule.enabled = enabled;
        let updated = rule.clone();
        self.persist(&rules);
        Some(updated)
    }

    /// 规则 → iptables 命令计划（**纯函数**，不执行）：
    ///
    /// 1. 建链（`-N`，已存在失败可忽略）+ **flush 先行**（`-F`——旧规则全清，
    ///    链内容与 JSON 状态一致）；
    /// 2. 逐条 enabled 规则 `-A`（in → NEXOS-FW / out → NEXOS-FW-OUT；
    ///    proto=any 省略 `-p`，port=None 省略 `--dport`，source=any 省略 `-s`；
    ///    allow → ACCEPT / deny → DROP）；
    /// 3. 挂接守卫（`-C` 探测失败即 `-I <链> 1 -j`——只在未挂接时插入，
    ///    重复 apply 不产生重复 jump 行）。
    #[must_use]
    pub fn plan_apply(rules: &[FirewallRule]) -> Vec<IptCommand> {
        let ipt = |argv: &[&str], fail_ok: bool| IptCommand {
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            fail_ok,
        };
        let mut plan = vec![
            ipt(&["iptables", "-N", FW_CHAIN_IN], true),
            ipt(&["iptables", "-F", FW_CHAIN_IN], false),
            ipt(&["iptables", "-N", FW_CHAIN_OUT], true),
            ipt(&["iptables", "-F", FW_CHAIN_OUT], false),
        ];
        for r in rules.iter().filter(|r| r.enabled) {
            let chain = if r.direction == "out" {
                FW_CHAIN_OUT
            } else {
                FW_CHAIN_IN
            };
            let mut argv: Vec<&str> = vec!["iptables", "-A", chain];
            let mut proto = "";
            let mut port_str = String::new();
            match r.proto.as_str() {
                "tcp" | "udp" => {
                    proto = &r.proto;
                    if let Some(p) = r.port {
                        port_str = p.to_string();
                    }
                }
                "icmp" => proto = "icmp",
                _ => {}
            }
            if !proto.is_empty() {
                argv.push("-p");
                argv.push(proto);
                if !port_str.is_empty() {
                    argv.push("--dport");
                    argv.push(&port_str);
                }
            }
            let source = r.source.trim();
            if !source.is_empty() && source != "any" {
                argv.push("-s");
                argv.push(source);
            }
            let target = if r.action == "deny" { "DROP" } else { "ACCEPT" };
            argv.push("-j");
            argv.push(target);
            plan.push(ipt(&argv, false));
        }
        // 挂接守卫：INPUT/OUTPUT → 自定义链（-C 失败 = 未挂接 → -I 插入）
        plan.push(ipt(&["iptables", "-C", "INPUT", "-j", FW_CHAIN_IN], true));
        plan.push(ipt(
            &["iptables", "-I", "INPUT", "1", "-j", FW_CHAIN_IN],
            false,
        ));
        plan.push(ipt(&["iptables", "-C", "OUTPUT", "-j", FW_CHAIN_OUT], true));
        plan.push(ipt(
            &["iptables", "-I", "OUTPUT", "1", "-j", FW_CHAIN_OUT],
            false,
        ));
        plan
    }

    /// 执行 apply 计划（spawn_blocking 逐条跑 `<sudo_bin> iptables …`）。
    /// 返回 `(applied, 执行记录)`——任一非 fail_ok 命令失败即 applied=false
    /// （sudo/iptables 不可用**降级**，不 Err 不 panic）。
    pub async fn apply(&self, rules: &[FirewallRule]) -> (bool, Vec<serde_json::Value>) {
        let plan = Self::plan_apply(rules);
        let sudo = self.cfg.ipt_sudo_bin.clone();
        let mut log = Vec::new();
        let mut all_ok = true;
        for cmd in plan {
            let fail_ok = cmd.fail_ok;
            let argv = cmd.argv;
            let argv_run = argv.clone();
            let sudo_run = sudo.clone();
            let out = tokio::task::spawn_blocking(move || {
                std::process::Command::new(&sudo_run)
                    .args(&argv_run)
                    .output()
                    .map(|o| {
                        (
                            o.status.success(),
                            String::from_utf8_lossy(&o.stderr).trim().to_string(),
                        )
                    })
                    .map_err(|e| e.to_string())
            })
            .await;
            let entry = match out {
                Ok(Ok((true, _))) => serde_json::json!({
                    "cmd": argv.join(" "), "ok": true,
                }),
                Ok(Ok((false, stderr))) => {
                    let ok = fail_ok;
                    if !ok {
                        all_ok = false;
                    }
                    serde_json::json!({
                        "cmd": argv.join(" "), "ok": ok, "fail_ok": fail_ok,
                        "stderr": stderr,
                    })
                }
                Ok(Err(e)) => {
                    // 命令本身不可执行（sudo 缺失等）——fail_ok 的探测命令
                    // （-C）在此场景也视为"未挂接"，由后续 -I 上；但 -I 同样
                    // 不可执行 → all_ok=false。统一记录。
                    if !fail_ok {
                        all_ok = false;
                    }
                    serde_json::json!({
                        "cmd": argv.join(" "), "ok": false, "fail_ok": fail_ok,
                        "error": e,
                    })
                }
                Err(e) => {
                    all_ok = false;
                    serde_json::json!({
                        "cmd": argv.join(" "), "ok": false, "error": format!("join 失败: {e}"),
                    })
                }
            };
            log.push(entry);
        }
        (all_ok, log)
    }

    /// iptables 链实况回读（`-L <链> -n --line-numbers`；失败降级 ok=false）。
    pub async fn chain_status(&self, chain: &str) -> serde_json::Value {
        let sudo = self.cfg.ipt_sudo_bin.clone();
        let chain = chain.to_string();
        let (sudo_run, chain_run) = (sudo.clone(), chain.clone());
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(&sudo_run)
                .args(["iptables", "-L", &chain_run, "-n", "--line-numbers"])
                .output()
                .map(|o| {
                    (
                        o.status.success(),
                        String::from_utf8_lossy(&o.stdout).trim().to_string(),
                        String::from_utf8_lossy(&o.stderr).trim().to_string(),
                    )
                })
                .map_err(|e| e.to_string())
        })
        .await;
        match out {
            Ok(Ok((true, stdout, _))) => serde_json::json!({
                "chain": chain, "ok": true, "raw": stdout,
                "lines": stdout.lines().map(str::to_string).collect::<Vec<_>>(),
            }),
            Ok(Ok((false, _, stderr))) => serde_json::json!({
                "chain": chain, "ok": false, "error": stderr,
            }),
            Ok(Err(e)) => serde_json::json!({ "chain": chain, "ok": false, "error": e }),
            Err(e) => serde_json::json!({
                "chain": chain, "ok": false, "error": format!("join 失败: {e}"),
            }),
        }
    }
}

/// 危险规则判定：deny + in + tcp/any + 端口 22 + 源不限（把管理口对全网关死）
/// ——后端拒绝除非 force=true；前端 apply 前对含此形态的规则集弹 confirm。
#[must_use]
pub fn is_dangerous_ssh_drop(rule: &FirewallRule) -> bool {
    rule.action == "deny"
        && rule.direction == "in"
        && matches!(rule.proto.as_str(), "tcp" | "any")
        && rule.port == Some(22)
        && (rule.source.trim().is_empty() || rule.source.trim() == "any")
}

// ============================================================================
// SOCKS5——最小服务端（仅 CONNECT / 无认证）+ 最小客户端
// ============================================================================

/// 解析 SOCKS5 greeting（客户端能力协商）：`[VER=5, NMETHODS, METHODS..]`。
/// 返回完整 greeting 的字节数（缓冲不足 → None——继续读）。
/// 仅支持无认证（METHODS 含 0x00 即可；VER != 5 → Some(0) 表示协议错误）。
#[must_use]
pub fn parse_socks5_greeting(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    if buf[0] != 0x05 {
        return Some(0); // 协议错误标记（调用方回失败应答）
    }
    let nmethods = buf[1] as usize;
    if buf.len() < 2 + nmethods {
        return None;
    }
    Some(2 + nmethods)
}

/// 解析 SOCKS5 CONNECT 请求：`[VER=5, CMD=1, RSV=0, ATYP, ADDR..., PORT(2)]`。
/// ATYP：1 = IPv4（4B）/ 3 = 域名（1B 长度前缀）/ 4 = IPv6（16B）。
/// 返回 `(host, port, 消耗字节数)`；CMD != 1 或缓冲不足 → None。
#[must_use]
pub fn parse_socks5_connect(buf: &[u8]) -> Option<(String, u16, usize)> {
    if buf.len() < 5 {
        return None;
    }
    if buf[0] != 0x05 || buf[1] != 0x01 || buf[2] != 0x00 {
        return None;
    }
    match buf[3] {
        0x01 => {
            if buf.len() < 4 + 4 + 2 {
                return None;
            }
            let host = format!("{}.{}.{}.{}", buf[4], buf[5], buf[6], buf[7]);
            let port = u16::from_be_bytes([buf[8], buf[9]]);
            Some((host, port, 10))
        }
        0x03 => {
            let len = buf[4] as usize;
            if buf.len() < 5 + len + 2 {
                return None;
            }
            let host = std::str::from_utf8(&buf[5..5 + len]).ok()?.to_string();
            let port = u16::from_be_bytes([buf[5 + len], buf[6 + len]]);
            Some((host, port, 5 + len + 2))
        }
        0x04 => {
            if buf.len() < 4 + 16 + 2 {
                return None;
            }
            let seg: Vec<String> = (0..8)
                .map(|i| u16::from_be_bytes([buf[4 + i * 2], buf[5 + i * 2]]).to_string())
                .collect();
            let host = seg.join(":");
            let port = u16::from_be_bytes([buf[20], buf[21]]);
            Some((host, port, 22))
        }
        _ => None,
    }
}

/// SOCKS5 应答（成功/失败，绑定地址回零——客户端不消费）。
fn socks5_reply(success: bool) -> [u8; 10] {
    [
        0x05,
        if success { 0x00 } else { 0x01 },
        0x00,
        0x01,
        0,
        0,
        0,
        0,
        0,
        0,
    ]
}

/// 读到「再也无法推进」为止的缓冲读取（greeting/CONNECT 分节读）。
async fn read_socks_frame(
    stream: &mut TcpStream,
    parse: fn(&[u8]) -> Option<usize>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        match parse(&buf) {
            Some(0) => return Ok(Some(buf)), // 协议错误标记
            Some(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            None => {}
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None); // 对端关闭
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 最小 SOCKS5 客户端：greeting + CONNECT + 等应答（出口侧 ingress 收到远端
/// open 后「本机拨 SOCKS5」用——11080 代拨入口）。
async fn socks5_dial(proxy: SocketAddr, host: &str, port: u16) -> std::io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy).await?;
    stream.write_all(&[0x05, 0x01, 0x00]).await?; // greeting：无认证
    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;
    if resp[0] != 0x05 || resp[1] != 0x00 {
        return Err(std::io::Error::other("SOCKS5 服务端拒绝无认证"));
    }
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await?;
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await?;
    if reply[1] != 0x00 {
        return Err(std::io::Error::other(format!(
            "SOCKS5 CONNECT 失败（code={}）",
            reply[1]
        )));
    }
    Ok(stream)
}

// ============================================================================
// 出口服务——overlay 连接引擎 + 双端 SOCKS5 监听
// ============================================================================

/// 本地连接事件（ingress → writer 泵：远端数据 / 远端关闭）。
enum ConnEvent {
    /// 远端发来一块数据（writer 写本地流后回 ack）。
    Data(Vec<u8>),
    /// 远端关闭（writer 收尾，不再回发 close——对方已知道；理由仅日志语义）。
    Closed(#[allow(dead_code)] String),
}

/// 一条 overlay 中继连接的本地账目（双端同构：entry 端本地流 = 应用 socket，
/// exit 端本地流 = 目标 socket）。
struct ExitConn {
    /// 连接 ID（发起侧生成，全网唯一性由 NodeID 前缀保证；账目键已含——
    /// 字段留作调试观测）。
    #[allow(dead_code)]
    id: String,
    /// 对端节点（entry 侧 = 出口节点；exit 侧 = 请求者）。
    peer: NodeId,
    /// 远端数据 → writer 泵。
    tx: mpsc::Sender<ConnEvent>,
    /// 发送窗口（reader 泵每块占 1 permit，收到 ack 归还——背压）。
    sem: Arc<Semaphore>,
}

/// 出口服务共享根。
pub struct ExitService {
    p2p: P2pHandle,
    cfg: ExitConfig,
    state: Mutex<ExitState>,
    firewall: Arc<FirewallManager>,
    conns: Mutex<HashMap<String, Arc<ExitConn>>>,
    /// open → 等待 opened/open_failed 的本地挂起（入口侧）。
    pending: Mutex<HashMap<String, oneshot::Sender<Result<(), String>>>>,
    seq: AtomicU64,
    /// 入口侧 SOCKS5 实际监听地址（端口 0 → 随机；status 展示用）。
    entry_addr: Mutex<Option<SocketAddr>>,
    /// 出口侧 SOCKS5 实际监听地址。
    exit_addr: Mutex<Option<SocketAddr>>,
    /// 观测计数（status 展示）。
    stats_conns_opened: AtomicU64,
    stats_conns_refused: AtomicU64,
    stats_bytes_relayed: AtomicU64,
}

impl ExitService {
    /// 起服务（**必须在 tokio runtime 内**）：加载持久化状态 → 推送 offered
    /// 到 p2p digest 位 → spawn ingress（on_msg）+ 双端 SOCKS5 监听（均
    /// 127.0.0.1——红线：不对外暴露，远端流量走 overlay）。
    #[must_use]
    pub fn spawn(p2p: P2pHandle, cfg: ExitConfig, firewall: Arc<FirewallManager>) -> Arc<Self> {
        let state = load_json_or_default::<ExitState>(&cfg.state_file);
        let offered = state.offered;
        let service = Arc::new(Self {
            p2p: p2p.clone(),
            cfg,
            state: Mutex::new(state),
            firewall,
            conns: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            entry_addr: Mutex::new(None),
            exit_addr: Mutex::new(None),
            stats_conns_opened: AtomicU64::new(0),
            stats_conns_refused: AtomicU64::new(0),
            stats_bytes_relayed: AtomicU64::new(0),
        });
        // 权威源是本组件状态文件——启动即推送到 p2p（下一轮 gossip 生效）。
        let h = p2p.clone();
        tokio::spawn(async move {
            h.set_exit_offered(offered).await;
        });
        // ingress：overlay 消息分发（net_exit 命名空间；其余让路）。
        let svc = service.clone();
        let mut rx = p2p.on_msg();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => ExitService::handle_inbound(&svc, &msg.from, msg.payload).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[network-exit] 观测落后 {n} 条（跳过）");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        // 入口侧 SOCKS5（用户应用指向这里 → 经 overlay 到远端出口）。
        let svc = service.clone();
        let port = service.cfg.entry_socks_port;
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    *svc.entry_addr.lock().expect("entry addr poisoned") = Some(
                        listener
                            .local_addr()
                            .ok()
                            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0))),
                    );
                    eprintln!(
                        "[network-exit] 入口 SOCKS5 已监听 127.0.0.1:{port}（浏览器/系统代理指向这里）"
                    );
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let svc = svc.clone();
                                tokio::spawn(async move {
                                    svc.entry_socks_conn(stream).await;
                                });
                            }
                            Err(e) => {
                                eprintln!("[network-exit] 入口 SOCKS5 accept 失败: {e}");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[network-exit] 入口 SOCKS5 监听失败（127.0.0.1:{port}）: {e}");
                }
            }
        });
        // 出口侧 SOCKS5（本机 ingress 代拨入口；本机进程也可直接使用）。
        let svc = service.clone();
        let port = service.cfg.exit_socks_port;
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    *svc.exit_addr.lock().expect("exit addr poisoned") = Some(
                        listener
                            .local_addr()
                            .ok()
                            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0))),
                    );
                    eprintln!("[network-exit] 出口 SOCKS5 已监听 127.0.0.1:{port}（本机代拨入口）");
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                tokio::spawn(async move {
                                    exit_local_socks_conn(stream).await;
                                });
                            }
                            Err(e) => {
                                eprintln!("[network-exit] 出口 SOCKS5 accept 失败: {e}");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[network-exit] 出口 SOCKS5 监听失败（127.0.0.1:{port}）: {e}");
                }
            }
        });
        service
    }

    // ------------------------------------------------------------------
    // 观察面
    // ------------------------------------------------------------------

    /// 状态全景（GET /net-exit/status）。
    pub async fn status(&self) -> serde_json::Value {
        let now = unix_now();
        let state = self.state.lock().expect("state poisoned").clone();
        let known: Vec<serde_json::Value> = self
            .p2p
            .node_meta()
            .await
            .into_iter()
            .filter(|e| e.exit_offered && e.id != *self.p2p.self_id())
            .map(|e| {
                serde_json::json!({
                    "node_id": e.id.to_hex(),
                    "last_seen": e.last_seen,
                    "alive": matches!(e.state, os_p2p::MetaState::Active { .. }),
                })
            })
            .collect();
        let entry = self
            .entry_addr
            .lock()
            .expect("entry addr poisoned")
            .map(|a| a.to_string())
            .unwrap_or_else(|| format!("127.0.0.1:{}", self.cfg.entry_socks_port));
        let exit = self
            .exit_addr
            .lock()
            .expect("exit addr poisoned")
            .map(|a| a.to_string())
            .unwrap_or_else(|| format!("127.0.0.1:{}", self.cfg.exit_socks_port));
        let auths: Vec<serde_json::Value> = state
            .authorizations
            .iter()
            .map(|a| {
                serde_json::json!({
                    "node_id": a.node_id,
                    "granted_at": a.granted_at,
                    "expires_at": a.expires_at,
                    "expired": a.expires_at <= now,
                })
            })
            .collect();
        serde_json::json!({
            "enabled": true,
            "node_id": self.p2p.self_id().to_hex(),
            "offered": state.offered,
            "exit_for": state.active_grantees(now),
            "authorizations": auths,
            "known_exits": known,
            "default_exit": state.default_exit,
            "local_socks": entry,
            "exit_socks": exit,
            "active_conns": self.conns.lock().expect("conns poisoned").len(),
            "stats": {
                "conns_opened": self.stats_conns_opened.load(Ordering::Relaxed),
                "conns_refused": self.stats_conns_refused.load(Ordering::Relaxed),
                "bytes_relayed": self.stats_bytes_relayed.load(Ordering::Relaxed),
            },
        })
    }

    /// 切换本节点出口声明（POST /net-exit/offer）——持久化为权威源 + 推送
    /// p2p digest 位。
    pub async fn set_offer(&self, enabled: bool) -> bool {
        {
            let mut st = self.state.lock().expect("state poisoned");
            st.offered = enabled;
            if let Err(e) = write_json_atomic(&self.cfg.state_file, &*st) {
                eprintln!(
                    "[network-exit] 出口状态落盘失败（{}）: {e}",
                    self.cfg.state_file.display()
                );
            }
        }
        self.p2p.set_exit_offered(enabled).await
    }

    /// 授权（POST /net-exit/authorize）。
    pub fn authorize(&self, node_id: &str, ttl_min: u64) -> AuthEntry {
        let mut st = self.state.lock().expect("state poisoned");
        let entry = st.authorize(node_id, ttl_min, unix_now());
        let _ = write_json_atomic(&self.cfg.state_file, &*st);
        entry
    }

    /// 撤销授权（DELETE /net-exit/authorize/:node_id）。
    pub fn revoke(&self, node_id: &str) -> bool {
        let mut st = self.state.lock().expect("state poisoned");
        let removed = st.revoke(node_id);
        let _ = write_json_atomic(&self.cfg.state_file, &*st);
        removed
    }

    /// 设默认出口（POST /net-exit/use；None 清除）。
    pub fn set_default_exit(&self, node_id: Option<&str>) -> Option<String> {
        let mut st = self.state.lock().expect("state poisoned");
        st.default_exit = node_id.map(str::to_string);
        let v = st.default_exit.clone();
        let _ = write_json_atomic(&self.cfg.state_file, &*st);
        v
    }

    /// 经默认/指定出口探活（POST /net-exit/proxy）：open → opened → close，
    /// 返回 (ok, exit_node_id, error)。
    pub async fn probe(
        &self,
        host: &str,
        port: u16,
        exit_node: Option<&str>,
    ) -> (bool, String, Option<String>) {
        let exit_id = match exit_node.map(str::to_string).or_else(|| {
            self.state
                .lock()
                .expect("state poisoned")
                .default_exit
                .clone()
        }) {
            Some(id) => id,
            None => {
                return (
                    false,
                    String::new(),
                    Some("未设置默认出口（先 POST /net-exit/use）".into()),
                )
            }
        };
        let Ok(node) = exit_id.parse::<NodeId>() else {
            return (false, exit_id, Some("出口 NodeID 解析失败".into()));
        };
        let conn_id = format!("nx{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending poisoned")
            .insert(conn_id.clone(), tx);
        self.p2p.send(
            &node,
            serde_json::json!({
                NS: KIND_OPEN, "conn_id": conn_id, "host": host, "port": port,
            }),
        );
        let result = match tokio::time::timeout(OPEN_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("出口节点无应答".into()),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("pending poisoned")
                    .remove(&conn_id);
                Err("open 超时".into())
            }
        };
        if result.is_ok() {
            // 探活即关（不传输数据）
            self.p2p.send(
                &node,
                serde_json::json!({ NS: KIND_CLOSE, "conn_id": conn_id, "reason": "probe" }),
            );
            self.stats_conns_opened.fetch_add(1, Ordering::Relaxed);
            (true, exit_id, None)
        } else {
            self.stats_conns_refused.fetch_add(1, Ordering::Relaxed);
            let err = result.err().unwrap_or_default();
            (false, exit_id, Some(err))
        }
    }

    /// 防火墙管理器（handler 复用同一实例——写读一致）。
    #[must_use]
    pub fn firewall(&self) -> Arc<FirewallManager> {
        self.firewall.clone()
    }

    // ------------------------------------------------------------------
    // 入站 overlay 消息分发
    // ------------------------------------------------------------------

    /// 处理一条入站 overlay 消息：net_exit 帧五种去向（open 走出口路径，
    /// opened/open_failed 唤醒等待者，data 喂 writer 泵，ack 归还窗口，
    /// close 收尾）。`self: &Arc<Self>`——open 路径需要 clone 服务实例进
    /// spawn 任务（泵共享同一连接账目）。
    async fn handle_inbound(self: &Arc<Self>, from: &NodeId, payload: serde_json::Value) {
        let Some(kind) = payload.get(NS).and_then(|v| v.as_str()) else {
            return; // 非本组件帧（transfer/联邦桥），让路
        };
        match kind {
            KIND_OPEN => {
                // 代拨可耗时（SOCKS5 + 目标 connect）——独立任务，不阻塞 ingress。
                let id = payload
                    .get("conn_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let host = payload
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let port = payload.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                let svc = self.clone();
                let from = from.clone();
                tokio::spawn(async move {
                    svc.handle_open(&from, &id, &host, port).await;
                });
            }
            KIND_OPENED | KIND_OPEN_FAILED => {
                let conn_id = payload
                    .get("conn_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(tx) = self
                    .pending
                    .lock()
                    .expect("pending poisoned")
                    .remove(conn_id)
                {
                    let result = if kind == KIND_OPENED {
                        Ok(())
                    } else {
                        Err(payload
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("拒绝")
                            .to_string())
                    };
                    let _ = tx.send(result);
                }
            }
            KIND_DATA => {
                let Some(conn_id) = payload.get("conn_id").and_then(|v| v.as_str()) else {
                    return;
                };
                let conn = {
                    let table = self.conns.lock().expect("conns poisoned");
                    table
                        .get(&format!("x:{conn_id}"))
                        .or_else(|| table.get(&format!("e:{conn_id}")))
                        .cloned()
                };
                let Some(conn) = conn else {
                    return; // 未知/已关闭连接——丢弃（对端会收到我们的 close）
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD
                    .decode(payload.get("bytes").and_then(|v| v.as_str()).unwrap_or(""))
                else {
                    return;
                };
                self.stats_bytes_relayed
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                // 喂 writer 泵（try_send：通道满 = 本地写不动——对端窗口因未收到
                // ack 而暂停，天然背压；错误 = writer 已收尾，丢弃）。ack 由
                // writer 实际写入本地流后回发（窗口与真实写透绑定）。
                let _ = conn.tx.try_send(ConnEvent::Data(bytes));
            }
            KIND_ACK => {
                let raw = payload
                    .get("conn_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let conn = {
                    let table = self.conns.lock().expect("conns poisoned");
                    table
                        .get(&format!("x:{raw}"))
                        .or_else(|| table.get(&format!("e:{raw}")))
                        .cloned()
                };
                let Some(conn) = conn else {
                    return;
                };
                conn.sem.add_permits(1);
            }
            KIND_CLOSE => {
                let raw = payload
                    .get("conn_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let conn = {
                    let mut table = self.conns.lock().expect("conns poisoned");
                    table
                        .remove(&format!("x:{raw}"))
                        .or_else(|| table.remove(&format!("e:{raw}")))
                };
                if let Some(conn) = conn {
                    // 对方已关闭——通知 writer 收尾（不再回发 close）
                    let _ = conn.tx.try_send(ConnEvent::Closed("remote closed".into()));
                }
            }
            _ => {}
        }
    }

    /// 出口路径：查授权（默认 deny）→ 本机拨 SOCKS5 → 注册连接 + 双向泵 →
    /// 回 opened/open_failed。
    async fn handle_open(self: &Arc<Self>, from: &NodeId, conn_id: &str, host: &str, port: u16) {
        let reply = |reason: Option<&str>| {
            let peer = from.clone();
            let id = conn_id.to_string();
            let reason = reason.map(str::to_string);
            let p2p = self.p2p.clone();
            tokio::spawn(async move {
                let msg = match reason {
                    None => serde_json::json!({ NS: KIND_OPENED, "conn_id": id }),
                    Some(r) => {
                        serde_json::json!({ NS: KIND_OPEN_FAILED, "conn_id": id, "reason": r })
                    }
                };
                p2p.send(&peer, msg);
            });
        };
        if conn_id.is_empty() || host.is_empty() || port == 0 {
            self.stats_conns_refused.fetch_add(1, Ordering::Relaxed);
            reply(Some("bad request"));
            return;
        }
        // 授权三查：本节点 offered + 授权表命中 + 未过期（默认 deny）。
        let now = unix_now();
        let unauthorized = {
            let state = self.state.lock().expect("state poisoned");
            !state.offered || !state.is_authorized(&from.to_hex(), now)
        };
        if unauthorized {
            self.stats_conns_refused.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[network-exit] 拒绝 {} 的代拨请求（未 offer 或未授权——默认 deny）",
                short_node(&from.to_hex())
            );
            reply(Some("unauthorized"));
            return;
        }
        // 本机拨 SOCKS5（出口侧代拨入口；授权在 overlay 层完成，SOCKS5 层无认证）。
        let proxy = self
            .exit_addr
            .lock()
            .expect("exit addr poisoned")
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], self.cfg.exit_socks_port)));
        let stream = match socks5_dial(proxy, host, port).await {
            Ok(s) => s,
            Err(e) => {
                self.stats_conns_refused.fetch_add(1, Ordering::Relaxed);
                reply(Some(&format!("dial failed: {e}")));
                return;
            }
        };
        // 账目 key 加出口侧前缀：自环场景（入口=出口=本节点）两侧共用一张
        // conns 表，裸 conn_id 会互相覆盖导致泵饿死（08-30 实测数据面无响应）
        let key = format!("x:{conn_id}");
        let (rx, sem) = self.register_conn(key.clone(), from.clone());
        let _ = sem;
        let (r_half, w_half) = stream.into_split();
        let svc = self.clone();
        tokio::spawn(pump_writer(svc.clone(), w_half, rx, key.clone()));
        tokio::spawn(pump_reader(svc, r_half, key));
        self.stats_conns_opened.fetch_add(1, Ordering::Relaxed);
        reply(None);
    }

    /// 注册本地连接账目（返回 writer 泵的接收端 + reader 泵的窗口）。
    /// **先注册后发 open**——出口侧 opened 与后续 data 都能即时命中账目
    /// （入口侧注册竞态规避）。
    fn register_conn(
        &self,
        id: String,
        peer: NodeId,
    ) -> (mpsc::Receiver<ConnEvent>, Arc<Semaphore>) {
        let (tx, rx) = mpsc::channel(WINDOW * 8);
        let sem = Arc::new(Semaphore::new(WINDOW));
        self.conns.lock().expect("conns poisoned").insert(
            id.clone(),
            Arc::new(ExitConn {
                id,
                peer,
                tx,
                sem: sem.clone(),
            }),
        );
        (rx, sem)
    }

    // ------------------------------------------------------------------
    // 入口侧 SOCKS5（用户应用 → overlay → 远端出口）
    // ------------------------------------------------------------------

    /// 入口侧一条 SOCKS5 连接：handshake → CONNECT → 注册账目 → overlay open →
    /// 成功即双向泵（app read_half → data 消息；远端 data → app write_half）。
    async fn entry_socks_conn(self: &Arc<Self>, mut stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        // greeting：仅无认证（METHODS 含 0x00）
        match read_socks_frame(&mut stream, parse_socks5_greeting).await {
            Ok(Some(buf)) if !buf.is_empty() && buf[0] == 0x05 && buf.contains(&0x00) => {}
            _ => return,
        }
        if stream.write_all(&[0x05, 0x00]).await.is_err() {
            return;
        }
        // CONNECT 请求（仅 CMD=1）
        let (host, port, _) =
            match read_socks_frame(&mut stream, |b| parse_socks5_connect(b).map(|(_, _, n)| n))
                .await
            {
                Ok(Some(buf)) => match parse_socks5_connect(&buf) {
                    Some(t) => t,
                    None => {
                        let _ = stream.write_all(&socks5_reply(false)).await;
                        return;
                    }
                },
                _ => return,
            };
        // 选出口：状态里的默认出口（锁取值独立成句——guard 不得跨 await）
        let default = self
            .state
            .lock()
            .expect("state poisoned")
            .default_exit
            .clone();
        let exit_id = match default {
            Some(id) => id,
            None => {
                let _ = stream.write_all(&socks5_reply(false)).await;
                return;
            }
        };
        let Ok(node) = exit_id.parse::<NodeId>() else {
            let _ = stream.write_all(&socks5_reply(false)).await;
            return;
        };
        // 先注册账目（opened 前置数据也能命中），再发 open 等结果。
        let conn_id = format!("nx{}", self.seq.fetch_add(1, Ordering::Relaxed));
        // 账目 key 加入口侧前缀（同上：防自环双注册覆盖）
        let key = format!("e:{conn_id}");
        let (rx, _sem) = self.register_conn(key.clone(), node.clone());
        let (tx, rx_wait) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending poisoned")
            .insert(conn_id.clone(), tx);
        self.p2p.send(
            &node,
            serde_json::json!({
                NS: KIND_OPEN, "conn_id": conn_id, "host": host, "port": port,
            }),
        );
        let result = match tokio::time::timeout(OPEN_TIMEOUT, rx_wait).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("出口节点无应答".into()),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("pending poisoned")
                    .remove(&conn_id);
                self.close_conn(&key, "open timeout", false);
                Err("open 超时".into())
            }
        };
        if let Err(e) = result {
            self.stats_conns_refused.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[network-exit] open 失败（{host}:{port} via {}）: {e}",
                short_node(&exit_id)
            );
            let _ = stream.write_all(&socks5_reply(false)).await;
            return;
        }
        if stream.write_all(&socks5_reply(true)).await.is_err() {
            self.close_conn(&key, "app closed", true);
            return;
        }
        self.stats_conns_opened.fetch_add(1, Ordering::Relaxed);
        let (r_half, w_half) = stream.into_split();
        let svc = self.clone();
        tokio::spawn(pump_writer(svc.clone(), w_half, rx, key.clone()));
        tokio::spawn(pump_reader(svc, r_half, key));
    }

    /// 关闭本地连接账目（notify_remote = 同时给对端发 close）。
    fn close_conn(&self, conn_id: &str, reason: &str, notify_remote: bool) {
        let conn = self.conns.lock().expect("conns poisoned").remove(conn_id);
        if let Some(conn) = conn {
            if notify_remote {
                let peer = conn.peer.clone();
                // 线上始终裸 conn_id（本地账目才带 x:/e: 方向前缀——剥掉再发）
                let id = conn_id
                    .trim_start_matches("x:")
                    .trim_start_matches("e:")
                    .to_string();
                let reason = reason.to_string();
                let p2p = self.p2p.clone();
                tokio::spawn(async move {
                    p2p.send(
                        &peer,
                        serde_json::json!({
                            NS: KIND_CLOSE, "conn_id": id, "reason": reason,
                        }),
                    );
                });
            }
        }
    }
}

/// 出口侧本地 SOCKS5 连接（127.0.0.1:11080——本机进程或 ingress 代拨）：
/// handshake → CONNECT → 直接 `TcpStream::connect` 目标 → 双向裸转发。
/// 授权不在这一层（远端授权在 overlay ingress 凭 NodeID 检查；本机进程为
/// 本地信任域）。
async fn exit_local_socks_conn(mut stream: TcpStream) {
    stream.set_nodelay(true).ok();
    match read_socks_frame(&mut stream, parse_socks5_greeting).await {
        Ok(Some(buf)) if !buf.is_empty() && buf[0] == 0x05 && buf.contains(&0x00) => {}
        _ => return,
    }
    if stream.write_all(&[0x05, 0x00]).await.is_err() {
        return;
    }
    let (host, port, _) =
        match read_socks_frame(&mut stream, |b| parse_socks5_connect(b).map(|(_, _, n)| n)).await {
            Ok(Some(buf)) => match parse_socks5_connect(&buf) {
                Some(t) => t,
                None => {
                    let _ = stream.write_all(&socks5_reply(false)).await;
                    return;
                }
            },
            _ => return,
        };
    let target = match TcpStream::connect((host.as_str(), port)).await {
        Ok(t) => t,
        Err(_) => {
            let _ = stream.write_all(&socks5_reply(false)).await;
            return;
        }
    };
    if stream.write_all(&socks5_reply(true)).await.is_err() {
        return;
    }
    let mut target = target;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
}

/// reader 泵（本地流 → overlay data 消息；双端同构）：每块占一个窗口 permit
/// （背压——对端未 ack 则暂停读本地流），EOF/错误 → close 对端 + 清账。
async fn pump_reader<R: AsyncRead + Unpin>(svc: Arc<ExitService>, mut r: R, conn_id: String) {
    let (peer, sem) = {
        let Some(conn) = svc
            .conns
            .lock()
            .expect("conns poisoned")
            .get(&conn_id)
            .cloned()
        else {
            return;
        };
        (conn.peer.clone(), conn.sem.clone())
    };
    let mut buf = vec![0u8; CHUNK];
    loop {
        let permit = match tokio::time::timeout(WINDOW_TIMEOUT, sem.acquire()).await {
            Ok(p) => p.expect("semaphore 未 close"),
            Err(_) => {
                eprintln!("[network-exit] 连接 {conn_id} 发送窗口超时（对端未 ack）——关闭");
                break;
            }
        };
        permit.forget(); // 对端 ack 时 add_permits 归还（ingress KIND_ACK 路径）
        match r.read(&mut buf).await {
            Ok(0) => break, // 本地流 EOF
            Ok(n) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                svc.p2p.send(
                    &peer,
                    serde_json::json!({
                        NS: KIND_DATA, "conn_id": conn_id.trim_start_matches("x:").trim_start_matches("e:"), "bytes": b64,
                    }),
                );
            }
            Err(_) => break,
        }
    }
    svc.close_conn(&conn_id, "local eof", true);
}

/// writer 泵（overlay data 消息 → 本地流；双端同构）：Data → 写本地流 → 回
/// ack（对端窗口归还——与真实写透绑定）；Closed → 收尾清账（不回发 close——
/// 对方已关闭）。
async fn pump_writer<W: AsyncWrite + Unpin>(
    svc: Arc<ExitService>,
    mut w: W,
    mut rx: mpsc::Receiver<ConnEvent>,
    conn_id: String,
) {
    let peer = {
        let Some(conn) = svc
            .conns
            .lock()
            .expect("conns poisoned")
            .get(&conn_id)
            .cloned()
        else {
            return;
        };
        conn.peer.clone()
    };
    while let Some(ev) = rx.recv().await {
        match ev {
            ConnEvent::Data(bytes) => {
                if w.write_all(&bytes).await.is_err() {
                    break;
                }
                let id = conn_id.clone();
                let peer = peer.clone();
                let p2p = svc.p2p.clone();
                tokio::spawn(async move {
                    let wire_id = id
                        .trim_start_matches("x:")
                        .trim_start_matches("e:")
                        .to_string();
                    p2p.send(
                        &peer,
                        serde_json::json!({ NS: KIND_ACK, "conn_id": wire_id }),
                    );
                });
            }
            ConnEvent::Closed(_) => break,
        }
    }
    let _ = w.shutdown().await;
    svc.close_conn(&conn_id, "writer done", false);
}

/// NodeID hex 短式（日志用）。
fn short_node(hex: &str) -> String {
    let n = hex.len();
    if n <= 12 {
        hex.to_string()
    } else {
        format!("{}…{}", &hex[..8], &hex[n - 4..])
    }
}

// ============================================================================
// RouteHandler——HTTP 边界
// ============================================================================

/// WAN 出口 + 防火墙路由处理器。
///
/// - `exit: Some(service)`：main.rs 在 os-p2p spawn 成功后装配；
/// - `exit: None`：P2P 未启用——net-exit 端点 503 + 引导文案（**防火墙端点
///   照常工作**：规则管理与 iptables 落地是本机能力，不依赖组网）。
pub struct NetworkExitRouteHandler {
    exit: Option<Arc<ExitService>>,
    firewall: Arc<FirewallManager>,
}

impl NetworkExitRouteHandler {
    /// 未启用构造（默认部署：`NEXOS_P2P_ENABLE` 未设/为 0）——防火墙可用。
    #[must_use]
    pub fn new_disabled() -> Self {
        Self {
            exit: None,
            firewall: Arc::new(FirewallManager::from_env()),
        }
    }

    /// 已启用构造（main.rs 装配：p2p spawn 成功后传入共享服务实例）。
    #[must_use]
    pub fn new(exit: Arc<ExitService>) -> Self {
        Self {
            firewall: exit.firewall(),
            exit: Some(exit),
        }
    }

    /// 未启用统一 503 语义。
    fn disabled_response() -> ApiResponse {
        ApiResponse {
            status: 503,
            body: serde_json::json!({"error": DISABLED_MSG}),
            headers: serde_json::json!({}),
        }
    }
}

impl Default for NetworkExitRouteHandler {
    fn default() -> Self {
        Self::new_disabled()
    }
}

#[async_trait]
impl RouteHandler for NetworkExitRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— WAN 出口 ——
            spec_read(HttpMethod::Get, "/api/v1/net-exit/status"),
            spec_admin(HttpMethod::Post, "/api/v1/net-exit/offer"),
            spec_admin(HttpMethod::Post, "/api/v1/net-exit/authorize"),
            spec_admin(HttpMethod::Delete, "/api/v1/net-exit/authorize/:node_id"),
            spec_admin(HttpMethod::Post, "/api/v1/net-exit/use"),
            spec_admin(HttpMethod::Post, "/api/v1/net-exit/proxy"),
            // —— 防火墙 ——
            spec_read(HttpMethod::Get, "/api/v1/firewall/rules"),
            spec_admin(HttpMethod::Post, "/api/v1/firewall/rules"),
            spec_admin(HttpMethod::Post, "/api/v1/firewall/rules/:id/toggle"),
            spec_admin(HttpMethod::Delete, "/api/v1/firewall/rules/:id"),
            spec_admin(HttpMethod::Post, "/api/v1/firewall/apply"),
            spec_read(HttpMethod::Get, "/api/v1/firewall/status"),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/net-exit/status ——
            (HttpMethod::Get, ["api", "v1", "net-exit", "status"]) => match &self.exit {
                Some(s) => Ok(ok_json(s.status().await)),
                None => Ok(Self::disabled_response()),
            },

            // —— POST /api/v1/net-exit/offer —— {enabled}
            (HttpMethod::Post, ["api", "v1", "net-exit", "offer"]) => match &self.exit {
                Some(s) => {
                    let Some(enabled) = req.body.get("enabled").and_then(|v| v.as_bool()) else {
                        return Ok(error_response(400, "body 需要 {enabled: bool}"));
                    };
                    let applied = s.set_offer(enabled).await;
                    Ok(ok_json(serde_json::json!({
                        "offered": enabled, "applied": applied,
                        "note": "下一轮元数据交互（≤6 tick）自广播携带，全网 1-2 轮感知",
                    })))
                }
                None => Ok(Self::disabled_response()),
            },

            // —— POST /api/v1/net-exit/authorize —— {node_id, ttl_min}
            (HttpMethod::Post, ["api", "v1", "net-exit", "authorize"]) => match &self.exit {
                Some(s) => {
                    let Some(node_id) = req
                        .body
                        .get("node_id")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                    else {
                        return Ok(error_response(400, "body 需要 {node_id, ttl_min}"));
                    };
                    if node_id.parse::<NodeId>().is_err() {
                        return Ok(error_response(400, "node_id 不是合法 NodeID（0x+66 hex）"));
                    }
                    let ttl = req
                        .body
                        .get("ttl_min")
                        .and_then(|v| v.as_u64())
                        .filter(|v| *v > 0 && *v <= 60 * 24 * 30)
                        .unwrap_or(60);
                    let entry = s.authorize(node_id, ttl);
                    Ok(ApiResponse {
                        status: 201,
                        body: to_value(&entry)?,
                        headers: serde_json::json!({}),
                    })
                }
                None => Ok(Self::disabled_response()),
            },

            // —— DELETE /api/v1/net-exit/authorize/:node_id ——
            (HttpMethod::Delete, ["api", "v1", "net-exit", "authorize", node_id]) => {
                match &self.exit {
                    Some(s) => {
                        if s.revoke(node_id) {
                            Ok(ok_json(serde_json::json!({"ok": true, "node_id": node_id})))
                        } else {
                            Ok(error_response(404, &format!("无此授权: {node_id}")))
                        }
                    }
                    None => Ok(Self::disabled_response()),
                }
            }

            // —— POST /api/v1/net-exit/use —— {exit_node_id}
            (HttpMethod::Post, ["api", "v1", "net-exit", "use"]) => match &self.exit {
                Some(s) => {
                    let id = req
                        .body
                        .get("exit_node_id")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty());
                    if let Some(id) = &id {
                        if id.parse::<NodeId>().is_err() {
                            return Ok(error_response(400, "exit_node_id 不是合法 NodeID"));
                        }
                    }
                    let set = s.set_default_exit(id);
                    Ok(ok_json(serde_json::json!({
                        "default_exit": set,
                        "note": "本地 SOCKS5（浏览器/系统代理指向它）见 status.local_socks",
                    })))
                }
                None => Ok(Self::disabled_response()),
            },

            // —— POST /api/v1/net-exit/proxy —— {host, port, exit_node_id?}
            (HttpMethod::Post, ["api", "v1", "net-exit", "proxy"]) => match &self.exit {
                Some(s) => {
                    let Some(host) = req
                        .body
                        .get("host")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                    else {
                        return Ok(error_response(400, "body 需要 {host, port, exit_node_id?}"));
                    };
                    let Some(port) = req
                        .body
                        .get("port")
                        .and_then(|v| v.as_u64())
                        .filter(|p| (1..=65535).contains(p))
                    else {
                        return Ok(error_response(400, "port 须为 1..=65535"));
                    };
                    let exit_node = req
                        .body
                        .get("exit_node_id")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty());
                    let (ok, node, err) = s.probe(host, port as u16, exit_node).await;
                    Ok(ok_json(serde_json::json!({
                        "ok": ok, "exit_node": node, "error": err,
                    })))
                }
                None => Ok(Self::disabled_response()),
            },

            // —— GET /api/v1/firewall/rules ——（公开；真实数据，空表起步）
            (HttpMethod::Get, ["api", "v1", "firewall", "rules"]) => {
                Ok(ok_json(to_value(&self.firewall.rules_snapshot())?))
            }

            // —— POST /api/v1/firewall/rules ——（admin）
            (HttpMethod::Post, ["api", "v1", "firewall", "rules"]) => {
                let direction = req
                    .body
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("in")
                    .to_ascii_lowercase();
                if !matches!(direction.as_str(), "in" | "out") {
                    return Ok(error_response(400, "direction 合法值: in / out"));
                }
                let proto = req
                    .body
                    .get("proto")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("any")
                    .to_ascii_lowercase();
                if !matches!(proto.as_str(), "tcp" | "udp" | "icmp" | "any") {
                    return Ok(error_response(400, "proto 合法值: tcp / udp / icmp / any"));
                }
                let port: Option<u16> = match req.body.get("port") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => match v.as_u64().filter(|p| (1..=65535).contains(p)) {
                        Some(p) => Some(p as u16),
                        None => return Ok(error_response(400, "port 须为 1..=65535 或 null")),
                    },
                };
                if port.is_some() && !matches!(proto.as_str(), "tcp" | "udp") {
                    return Ok(error_response(
                        400,
                        "port 仅 tcp/udp 可填（icmp/any 无端口）",
                    ));
                }
                let source = req
                    .body
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("any")
                    .to_string();
                let action = req
                    .body
                    .get("action")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("allow")
                    .to_ascii_lowercase();
                if !matches!(action.as_str(), "allow" | "deny") {
                    return Ok(error_response(400, "action 合法值: allow / deny"));
                }
                let enabled = req
                    .body
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let note = req
                    .body
                    .get("note")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("")
                    .to_string();
                let force = req
                    .body
                    .get("force")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // 先构造后防呆（is_dangerous_ssh_drop 需要完整规则）
                let rule = self
                    .firewall
                    .add_rule(&direction, &proto, port, &source, &action, enabled, &note);
                if is_dangerous_ssh_drop(&rule) && !force {
                    // 回滚（防呆拒绝时不留半条规则）
                    self.firewall.remove_rule(&rule.id);
                    return Ok(error_response(
                        400,
                        "危险规则：deny + in + 22 + 源不限 会把 SSH 管理口对全网关死。\
                         确认要这样做请在 body 加 \"force\": true",
                    ));
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&rule)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/firewall/rules/:id/toggle —— {enabled}
            (HttpMethod::Post, ["api", "v1", "firewall", "rules", id, "toggle"]) => {
                let Some(enabled) = req.body.get("enabled").and_then(|v| v.as_bool()) else {
                    return Ok(error_response(400, "body 需要 {enabled: bool}"));
                };
                match self.firewall.toggle_rule(id, enabled) {
                    Some(r) => Ok(ok_json(to_value(&r)?)),
                    None => Ok(error_response(404, &format!("规则不存在: {id}"))),
                }
            }

            // —— DELETE /api/v1/firewall/rules/:id ——
            (HttpMethod::Delete, ["api", "v1", "firewall", "rules", id]) => {
                match self.firewall.remove_rule(id) {
                    Some(_) => Ok(ok_json(serde_json::json!({"ok": true, "id": id}))),
                    None => Ok(error_response(404, &format!("规则不存在: {id}"))),
                }
            }

            // —— POST /api/v1/firewall/apply ——（admin）
            (HttpMethod::Post, ["api", "v1", "firewall", "apply"]) => {
                let rules = self.firewall.rules_snapshot();
                let dangerous: Vec<&str> = rules
                    .iter()
                    .filter(|r| r.enabled && is_dangerous_ssh_drop(r))
                    .map(|r| r.id.as_str())
                    .collect();
                if !dangerous.is_empty() {
                    let force = req
                        .body
                        .get("force")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !force {
                        return Ok(error_response(
                            400,
                            &format!(
                                "规则集含危险规则（deny SSH 管理口）：{}。确认请加 \"force\": true",
                                dangerous.join(", ")
                            ),
                        ));
                    }
                }
                let (applied, log) = self.firewall.apply(&rules).await;
                Ok(ok_json(serde_json::json!({
                    "applied": applied,
                    "rules_total": rules.len(),
                    "rules_enabled": rules.iter().filter(|r| r.enabled).count(),
                    "chains": [FW_CHAIN_IN, FW_CHAIN_OUT],
                    "commands": log,
                    "warning": if applied {
                        String::new()
                    } else {
                        "部分命令执行失败（可能缺少 sudo 免密 / iptables）——详见 commands".into()
                    },
                })))
            }

            // —— GET /api/v1/firewall/status ——（公开）
            (HttpMethod::Get, ["api", "v1", "firewall", "status"]) => {
                let in_chain = self.firewall.chain_status(FW_CHAIN_IN).await;
                let out_chain = self.firewall.chain_status(FW_CHAIN_OUT).await;
                Ok(ok_json(serde_json::json!({
                    "chains": { FW_CHAIN_IN: in_chain, FW_CHAIN_OUT: out_chain },
                    "note": "iptables 实况（sudo -L 回读；ok=false = 无特权或 iptables 缺失）",
                })))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "network-exit: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助（与其它 handler 同款）
// ----------------------------------------------------------------------------

/// 构造一条只读路由规格。
fn spec_read(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: false,
        required_roles: Vec::new(),
    }
}

/// 构造一条写路由规格（admin）。
fn spec_admin(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: true,
        required_roles: vec!["admin".to_string()],
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

fn to_value<T: serde::Serialize + ?Sized>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_p2p::{P2pConfig, P2pNode, Timing};

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

    fn delete_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 临时目录 + 测试配置（端口 0 随机、假 sudo 可注入）。
    fn test_cfg(tag: &str) -> ExitConfig {
        let dir = std::env::temp_dir().join(format!("net-exit-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ExitConfig {
            state_file: dir.join("state.json"),
            fw_file: dir.join("firewall.json"),
            exit_socks_port: 0,
            entry_socks_port: 0,
            ipt_sudo_bin: "sudo".to_string(),
        }
    }

    fn spawn_test_node() -> P2pHandle {
        P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("随机端口绑定必成功")
    }

    // 1. SOCKS5 greeting 解析（固定字节样本）：完整/分片/协议错误
    #[test]
    fn socks5_greeting_parse_samples() {
        // [5,1,0]：1 个方法（无认证）
        assert_eq!(parse_socks5_greeting(&[0x05, 0x01, 0x00]), Some(3));
        // [5,2,0,2]：无认证+用户名密码（含 0 → 可协商，消耗 4）
        assert_eq!(parse_socks5_greeting(&[0x05, 0x02, 0x00, 0x02]), Some(4));
        // 分片：只到 NMETHODS → 不足
        assert_eq!(parse_socks5_greeting(&[0x05, 0x02]), None);
        assert_eq!(parse_socks5_greeting(&[0x05, 0x02, 0x00]), None);
        // 空
        assert_eq!(parse_socks5_greeting(&[]), None);
        // 协议错误：VER != 5 → Some(0) 标记
        assert_eq!(parse_socks5_greeting(&[0x04, 0x01, 0x00]), Some(0));
    }

    // 2. SOCKS5 CONNECT 解析（固定字节样本）：域名 / IPv4 / IPv6 / 非法
    #[test]
    fn socks5_connect_parse_samples() {
        // 域名 example.com:443 → [5,1,0,3,11,'e'..'m',0x01,0xBB]
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(
            parse_socks5_connect(&req),
            Some(("example.com".to_string(), 443, req.len()))
        );
        // IPv4 10.0.0.1:8080
        let req = vec![0x05, 0x01, 0x00, 0x01, 10, 0, 0, 1, 0x1F, 0x90];
        assert_eq!(
            parse_socks5_connect(&req),
            Some(("10.0.0.1".to_string(), 8080, 10))
        );
        // IPv6 ::1:22
        let mut req = vec![0x05, 0x01, 0x00, 0x04];
        req.extend_from_slice(&[0u8; 15]);
        req.push(1);
        req.extend_from_slice(&22u16.to_be_bytes());
        let (host, port, n) = parse_socks5_connect(&req).unwrap();
        assert_eq!(host, "0:0:0:0:0:0:0:1");
        assert_eq!((port, n), (22, 22));
        // CMD != CONNECT（0x02 BIND）
        assert_eq!(
            parse_socks5_connect(&[0x05, 0x02, 0x00, 0x01, 10, 0, 0, 1, 0, 80]),
            None
        );
        // 缓冲不足（域名缺端口）
        let mut partial = vec![0x05, 0x01, 0x00, 0x03, 11];
        partial.extend_from_slice(b"example.com");
        assert_eq!(parse_socks5_connect(&partial), None);
        // 未知 ATYP
        assert_eq!(parse_socks5_connect(&[0x05, 0x01, 0x00, 0x05, 0, 0]), None);
    }

    // 3. SOCKS5 服务端 ↔ 客户端回环（出口侧本地 SOCKS：CONNECT + 双向数据）
    #[tokio::test]
    async fn exit_local_socks_roundtrip_with_mock_target() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // mock 目标：echo 服务器
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        // 出口侧本地 SOCKS5（accept 循环任务）
        let socks_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_addr = socks_listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = socks_listener.accept().await {
                tokio::spawn(exit_local_socks_conn(s));
            }
        });
        // 客户端：socks5_dial → 发送 → 收 echo
        let mut stream = socks5_dial(socks_addr, "127.0.0.1", target_addr.port())
            .await
            .expect("SOCKS5 代拨应成功");
        stream.write_all(b"hello-nexos-exit").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("echo 应在超时前到达")
            .expect("读不应出错");
        assert_eq!(&buf[..n], b"hello-nexos-exit", "数据经 SOCKS5 双向回传");
        // 拨不存在的端口 → 失败
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        assert!(
            socks5_dial(socks_addr, "127.0.0.1", dead_port)
                .await
                .is_err(),
            "目标不可达应回 SOCKS5 失败应答"
        );
    }

    // 4. 授权表：默认 deny / TTL 过期 / 撤销
    #[test]
    fn authorization_expiry_and_default_deny() {
        let mut st = ExitState::default();
        let now = 1000u64;
        assert!(!st.is_authorized("0xabc", now), "默认 deny");
        st.authorize("0xabc", 10, now);
        assert!(st.is_authorized("0xabc", now + 599), "TTL 内允许");
        assert!(
            !st.is_authorized("0xabc", now + 600),
            "到期即拒（>expires）"
        );
        assert!(!st.is_authorized("0xdef", now), "其他节点不受授权影响");
        assert_eq!(st.active_grantees(now), vec!["0xabc".to_string()]);
        // 续期覆盖（同节点单条）
        st.authorize("0xabc", 60, now + 10);
        assert_eq!(
            st.authorizations
                .iter()
                .filter(|a| a.node_id == "0xabc")
                .count(),
            1
        );
        assert!(st.is_authorized("0xabc", now + 10 + 3599));
        // 撤销
        assert!(st.revoke("0xabc"));
        assert!(!st.is_authorized("0xabc", now + 11), "撤销后 deny");
        assert!(!st.revoke("0xabc"), "再撤销返回 false");
    }

    // 5. 防火墙 CRUD + 持久化往返（空表起步——无 seed）
    #[tokio::test]
    async fn firewall_crud_and_persistence() {
        let cfg = test_cfg("fw-crud");
        let fw = FirewallManager::new(cfg.clone());
        assert!(
            fw.rules_snapshot().is_empty(),
            "空表起步（无 seed 演示数据）"
        );
        // 添加
        let r1 = fw.add_rule("in", "tcp", Some(443), "10.0.0.0/8", "allow", true, "https");
        assert_eq!(r1.id, "fw-1");
        let r2 = fw.add_rule("out", "any", None, "any", "deny", false, "占位");
        assert_eq!(r2.id, "fw-2");
        assert_eq!(fw.rules_snapshot().len(), 2);
        // toggle
        let t = fw.toggle_rule("fw-2", true).unwrap();
        assert!(t.enabled);
        assert!(fw.toggle_rule("fw-nope", true).is_none(), "未知 id → None");
        // 删除
        assert!(fw.remove_rule("fw-1").is_some());
        assert!(fw.remove_rule("fw-1").is_none(), "重复删 → None");
        assert_eq!(fw.rules_snapshot().len(), 1);
        // "重启"：新实例同文件 → 规则保真
        let fw2 = FirewallManager::new(cfg.clone());
        let reloaded = fw2.rules_snapshot();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].id, "fw-2");
        assert!(reloaded[0].enabled, "toggle 后状态持久化");
        assert_eq!(reloaded[0].action, "deny");
        let _ = std::fs::remove_file(&cfg.fw_file);
    }

    // 6. iptables 命令组装：链建立/flush 先行/规则注入顺序/jump 守卫收尾
    #[test]
    fn iptables_plan_order_flush_first() {
        let rules = vec![
            FirewallRule {
                id: "fw-1".into(),
                direction: "in".into(),
                proto: "tcp".into(),
                port: Some(443),
                source: "10.0.0.0/8".into(),
                action: "allow".into(),
                enabled: true,
                note: String::new(),
            },
            FirewallRule {
                id: "fw-2".into(),
                direction: "in".into(),
                proto: "any".into(),
                port: None,
                source: "any".into(),
                action: "deny".into(),
                enabled: true,
                note: String::new(),
            },
            FirewallRule {
                id: "fw-3".into(),
                direction: "out".into(),
                proto: "udp".into(),
                port: Some(53),
                source: "any".into(),
                action: "deny".into(),
                enabled: false,
                note: "停用不入链".into(),
            },
        ];
        let plan = FirewallManager::plan_apply(&rules);
        let argv: Vec<Vec<String>> = plan.iter().map(|c| c.argv.clone()).collect();
        // 头四条：建链 ×2 + flush ×2（flush 先行——在一切 -A 之前）
        assert_eq!(argv[0], str_vec(&["iptables", "-N", FW_CHAIN_IN]));
        assert_eq!(
            argv[1],
            str_vec(&["iptables", "-F", FW_CHAIN_IN]),
            "flush 先行"
        );
        assert_eq!(argv[2], str_vec(&["iptables", "-N", FW_CHAIN_OUT]));
        assert_eq!(argv[3], str_vec(&["iptables", "-F", FW_CHAIN_OUT]));
        let first_append = argv
            .iter()
            .position(|a| a.iter().any(|s| s == "-A"))
            .unwrap();
        assert!(first_append > 3, "-A 必须在 flush 之后");
        // 规则 1：in/tcp/443/源限 → NEXOS-FW
        assert!(argv.contains(&str_vec(&[
            "iptables",
            "-A",
            FW_CHAIN_IN,
            "-p",
            "tcp",
            "--dport",
            "443",
            "-s",
            "10.0.0.0/8",
            "-j",
            "ACCEPT"
        ])));
        // 规则 2：any 无 -p/--dport，源不限无 -s → DROP
        assert!(argv.contains(&str_vec(&["iptables", "-A", FW_CHAIN_IN, "-j", "DROP"])));
        // 规则 3 停用 → 不出现 53
        assert!(
            !argv.iter().any(|a| a.contains(&"53".to_string())),
            "停用规则不入链"
        );
        // 尾部 jump 守卫：-C 探测（fail_ok）+ -I 插入
        let tail: Vec<&Vec<String>> = argv.iter().rev().take(4).collect();
        assert!(tail.contains(&&str_vec(&["iptables", "-C", "INPUT", "-j", FW_CHAIN_IN])));
        assert!(tail.contains(&&str_vec(&[
            "iptables",
            "-I",
            "INPUT",
            "1",
            "-j",
            FW_CHAIN_IN
        ])));
        assert!(tail.contains(&&str_vec(&["iptables", "-C", "OUTPUT", "-j", FW_CHAIN_OUT])));
        assert!(tail.contains(&&str_vec(&[
            "iptables",
            "-I",
            "OUTPUT",
            "1",
            "-j",
            FW_CHAIN_OUT
        ])));
        // fail_ok 标记：-N/-C 可失败，-F/-A/-I 不可
        assert!(plan[0].fail_ok && !plan[1].fail_ok);
        let append_cmd = plan
            .iter()
            .find(|c| c.argv.contains(&"-A".to_string()))
            .unwrap();
        assert!(!append_cmd.fail_ok);
        // out 方向规则入 NEXOS-FW-OUT（启用后）
        let mut rules2 = rules.clone();
        rules2[2].enabled = true;
        let plan2 = FirewallManager::plan_apply(&rules2);
        assert!(plan2.iter().any(|c| c.argv
            == str_vec(&[
                "iptables",
                "-A",
                FW_CHAIN_OUT,
                "-p",
                "udp",
                "--dport",
                "53",
                "-j",
                "DROP"
            ])));
    }

    fn str_vec(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    // 7. 假 sudo 注入：apply 执行的 argv 落盘断言（链序/flush 先行/规则顺序）
    #[tokio::test]
    async fn firewall_apply_executes_via_injected_sudo() {
        let mut cfg = test_cfg("fw-apply");
        let dir = cfg.fw_file.parent().unwrap().to_path_buf();
        let record = dir.join("sudo-argv.log");
        let fake = dir.join("fake-sudo");
        // 假 sudo：把 argv 按行追加进 record 文件（firewall OK / -C 探测返回失败
        // 以触发 -I 插入——用 exit 1 模拟"未挂接"）
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 1\n",
                record.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        cfg.ipt_sudo_bin = fake.to_string_lossy().to_string();
        let fw = FirewallManager::new(cfg.clone());
        fw.add_rule(
            "in",
            "tcp",
            Some(22),
            "192.168.1.0/24",
            "allow",
            true,
            "mgmt ssh",
        );
        let (applied, log) = fw.apply(&fw.rules_snapshot()).await;
        // 假 sudo 全部 exit 1 → 非 fail_ok 命令失败 → applied=false（降级语义）
        assert!(!applied, "命令失败时 applied=false（降级不 panic）");
        assert!(!log.is_empty());
        let ran: Vec<String> = std::fs::read_to_string(&record)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert!(!ran.is_empty(), "假 sudo 应记录 argv");
        assert!(
            ran[0].starts_with("iptables -N"),
            "首条命令为建链: {}",
            ran[0]
        );
        assert!(ran[1].starts_with("iptables -F"), "flush 紧随其后（先行）");
        let rule_cmd = ran
            .iter()
            .find(|l| l.contains("--dport 22"))
            .expect("应注入 22 端口规则");
        assert!(rule_cmd.contains(&format!("-A {FW_CHAIN_IN}")));
        assert!(rule_cmd.contains("-s 192.168.1.0/24") && rule_cmd.contains("-j ACCEPT"));
        // jump 守卫在规则之后
        let rule_pos = ran.iter().position(|l| l.contains("--dport 22")).unwrap();
        let jump_pos = ran.iter().position(|l| l.contains("-I INPUT 1")).unwrap();
        assert!(jump_pos > rule_pos, "jump 插入在规则注入之后");
        let _ = std::fs::remove_file(&record);
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&cfg.fw_file);
    }

    // 8. 危险端口防呆：deny in 22 any → 400 + 回滚；force → 放行
    #[tokio::test]
    async fn firewall_rejects_dangerous_ssh_drop_unless_forced() {
        let cfg = test_cfg("fw-danger");
        let h = NetworkExitRouteHandler {
            exit: None,
            firewall: Arc::new(FirewallManager::new(cfg.clone())),
        };
        // 无 force → 400 + 不入库
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules",
                serde_json::json!({
                    "direction": "in", "proto": "tcp", "port": 22,
                    "source": "any", "action": "deny"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("危险"));
        assert!(
            h.firewall.rules_snapshot().is_empty(),
            "防呆拒绝时回滚（不留半条规则）"
        );
        // force → 201
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules",
                serde_json::json!({
                    "direction": "in", "proto": "tcp", "port": 22,
                    "source": "any", "action": "deny", "force": true
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "force=true 放行");
        assert_eq!(h.firewall.rules_snapshot().len(), 1);
        // 限定源的 deny 22（如仅封锁某网段）不属危险形态
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules",
                serde_json::json!({
                    "direction": "in", "proto": "tcp", "port": 22,
                    "source": "203.0.113.0/24", "action": "deny"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "限定源的 deny 22 不拦");
        // out 方向 22 不拦（ OUTPUT 语义不关死管理口）
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules",
                serde_json::json!({
                    "direction": "out", "proto": "tcp", "port": 22,
                    "source": "any", "action": "deny"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        // 非法 direction / proto → 400
        for bad in [
            serde_json::json!({"direction": "sideways", "proto": "tcp", "action": "allow"}),
            serde_json::json!({"direction": "in", "proto": "gre", "action": "allow"}),
            serde_json::json!({"direction": "in", "proto": "icmp", "port": 22, "action": "allow"}),
        ] {
            let resp = h
                .handle(post_req("/api/v1/firewall/rules", bad))
                .await
                .unwrap();
            assert_eq!(resp.status, 400);
        }
        let _ = std::fs::remove_file(&cfg.fw_file);
    }

    // 9. handler 路由声明与鉴权（net-exit 写 admin / 读公开；firewall 同）
    #[tokio::test]
    async fn routes_declared_with_auth() {
        let h = NetworkExitRouteHandler::new_disabled();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 12);
        assert!(routes.iter().all(|r| r.handler_component == COMPONENT));
        let find = |m: HttpMethod, p: &str| {
            routes
                .iter()
                .find(|r| r.method == m && r.path == p)
                .unwrap_or_else(|| panic!("缺路由 {p}"))
                .clone()
        };
        assert!(!find(HttpMethod::Get, "/api/v1/net-exit/status").requires_auth);
        assert!(find(HttpMethod::Post, "/api/v1/net-exit/offer").requires_auth);
        assert_eq!(
            find(HttpMethod::Post, "/api/v1/net-exit/authorize").required_roles,
            vec!["admin".to_string()]
        );
        assert!(find(HttpMethod::Delete, "/api/v1/net-exit/authorize/:node_id").requires_auth);
        assert!(find(HttpMethod::Post, "/api/v1/net-exit/use").requires_auth);
        assert!(find(HttpMethod::Post, "/api/v1/net-exit/proxy").requires_auth);
        assert!(!find(HttpMethod::Get, "/api/v1/firewall/rules").requires_auth);
        assert!(find(HttpMethod::Post, "/api/v1/firewall/rules").requires_auth);
        assert!(find(HttpMethod::Post, "/api/v1/firewall/rules/:id/toggle").requires_auth);
        assert!(find(HttpMethod::Delete, "/api/v1/firewall/rules/:id").requires_auth);
        assert!(find(HttpMethod::Post, "/api/v1/firewall/apply").requires_auth);
        assert!(!find(HttpMethod::Get, "/api/v1/firewall/status").requires_auth);
    }

    // 10. 未启用（p2p 关）语义：net-exit 503；防火墙照常
    #[tokio::test]
    async fn disabled_p2p_net_exit_503_firewall_ok() {
        let cfg = test_cfg("disabled");
        let h = NetworkExitRouteHandler {
            exit: None,
            firewall: Arc::new(FirewallManager::new(cfg.clone())),
        };
        let resp = h.handle(get_req("/api/v1/net-exit/status")).await.unwrap();
        assert_eq!(resp.status, 503);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("NEXOS_P2P_ENABLE"));
        let resp = h
            .handle(post_req(
                "/api/v1/net-exit/offer",
                serde_json::json!({"enabled": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 503);
        // 防火墙不依赖 p2p
        let resp = h.handle(get_req("/api/v1/firewall/rules")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, serde_json::json!([]));
        let _ = std::fs::remove_file(&cfg.fw_file);
    }

    // 11. handler 层 CRUD/toggle/delete 路由行为（走 handle 而非直调 manager）
    #[tokio::test]
    async fn firewall_handler_routes_crud() {
        let cfg = test_cfg("fw-routes");
        let h = NetworkExitRouteHandler {
            exit: None,
            firewall: Arc::new(FirewallManager::new(cfg.clone())),
        };
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules",
                serde_json::json!({
                    "direction": "in", "proto": "tcp", "port": 80,
                    "source": "any", "action": "allow", "note": "http"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["id"], "fw-1");
        assert_eq!(resp.body["port"], 80);
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules/fw-1/toggle",
                serde_json::json!({"enabled": false}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["enabled"], false);
        let resp = h.handle(get_req("/api/v1/firewall/rules")).await.unwrap();
        assert_eq!(resp.body[0]["enabled"], false, "toggle 后列表可见");
        let resp = h
            .handle(delete_req("/api/v1/firewall/rules/fw-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let resp = h
            .handle(delete_req("/api/v1/firewall/rules/fw-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/rules/fw-9/toggle",
                serde_json::json!({"enabled": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        // 兜底 404
        let resp = h.handle(get_req("/api/v1/firewall/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_file(&cfg.fw_file);
    }

    // 12. apply 危险规则集防呆（enabled deny-22 → 未 force 400 / force 放行执行）
    #[tokio::test]
    async fn firewall_apply_danger_guard() {
        let cfg = test_cfg("fw-apply-guard");
        let h = NetworkExitRouteHandler {
            exit: None,
            firewall: Arc::new(FirewallManager::new(cfg.clone())),
        };
        h.firewall
            .add_rule("in", "tcp", Some(22), "any", "deny", true, "test");
        let resp = h
            .handle(post_req("/api/v1/firewall/apply", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("危险"));
        let resp = h
            .handle(post_req(
                "/api/v1/firewall/apply",
                serde_json::json!({"force": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["applied"].is_boolean());
        assert!(resp.body["commands"].as_array().unwrap().len() >= 6);
        let _ = std::fs::remove_file(&cfg.fw_file);
    }

    // 13. 端到端：双 handler spawn——B offer + authorize A；A use B + proxy 探活
    //     拨通 mock TcpListener；未授权的 C 被拒。
    #[tokio::test]
    async fn end_to_end_two_nodes_offer_authorize_proxy() {
        let cfg_a = test_cfg("e2e-a");
        let cfg_b = test_cfg("e2e-b");
        let a = spawn_test_node();
        let b = spawn_test_node();
        let c = spawn_test_node();
        // 组网：B 拨 A（A↔B 直连，A 的出口请求可达 B）；C 直拨 B（C 的未授权
        // 探活也要可达 B——否则测的是"无路由超时"而非"授权拒绝"）
        let _a_id = b.dial(a.listen_addr()).await.expect("B→A 拨号");
        let _ = c.dial(b.listen_addr()).await.expect("C→B 拨号");
        let fw_a = Arc::new(FirewallManager::new(cfg_a.clone()));
        let fw_b = Arc::new(FirewallManager::new(cfg_b.clone()));
        let svc_a = ExitService::spawn(a.clone(), cfg_a, fw_a);
        let svc_b = ExitService::spawn(b.clone(), cfg_b, fw_b);
        // 等 SOCKS5 双端就绪
        let wait_addr = |svc: Arc<ExitService>, is_entry: bool| async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let got = if is_entry {
                    *svc.entry_addr.lock().expect("entry addr poisoned")
                } else {
                    *svc.exit_addr.lock().expect("exit addr poisoned")
                };
                if let Some(addr) = got {
                    return addr;
                }
                assert!(std::time::Instant::now() < deadline, "SOCKS5 监听应就绪");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let _entry_a = wait_addr(svc_a.clone(), true).await;
        let _exit_b = wait_addr(svc_b.clone(), false).await;
        // mock 目标（循环 accept——探活多次触发代拨）
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = target.accept().await {
                let mut buf = [0u8; 256];
                if let Ok(n) = s.read(&mut buf).await {
                    let _ = s.write_all(&buf[..n]).await;
                }
            }
        });
        // B：offer + 授权 A
        assert!(svc_b.set_offer(true).await);
        svc_b.authorize(&a.self_id().to_hex(), 30);
        // A：默认出口 = B
        svc_a.set_default_exit(Some(&b.self_id().to_hex()));
        // A 经 B 探活 mock 目标
        let (ok, node, err) = svc_a.probe("127.0.0.1", target_port, None).await;
        assert!(ok, "探活应成功（via {}）err={:?}", short_node(&node), err);
        assert_eq!(node, b.self_id().to_hex());
        // handler 层：POST /net-exit/proxy 同路径
        let h = NetworkExitRouteHandler::new(svc_a.clone());
        let resp = h
            .handle(post_req(
                "/api/v1/net-exit/proxy",
                serde_json::json!({"host": "127.0.0.1", "port": target_port}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true, "handler 探活同路径成功");
        // B 撤销 offer → A 探活失败（unauthorized）
        svc_b.set_offer(false).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (ok, _, err) = svc_a.probe("127.0.0.1", target_port, None).await;
        assert!(!ok, "撤销 offer 后应拒绝");
        assert!(err.unwrap_or_default().contains("unauthorized"));
        svc_b.set_offer(true).await;
        // C 未被授权 → 经 B 探活失败（默认 deny）
        let cfg_c = test_cfg("e2e-c");
        let svc_c = ExitService::spawn(
            c.clone(),
            cfg_c,
            Arc::new(FirewallManager::new(test_cfg("e2e-c-fw"))),
        );
        svc_c.set_default_exit(Some(&b.self_id().to_hex()));
        let (ok, _, err) = svc_c.probe("127.0.0.1", target_port, None).await;
        assert!(!ok, "未授权节点默认 deny");
        assert!(err.unwrap_or_default().contains("unauthorized"));
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    // 14. 端到端数据面：A 的入口 SOCKS5 → overlay → B 代拨 mock echo 目标，
    //     全双工字节往返（SOCKS5 客户端 = socks5_dial 复用）。
    #[tokio::test]
    async fn end_to_end_entry_socks_data_path() {
        let cfg_a = test_cfg("e2e2-a");
        let cfg_b = test_cfg("e2e2-b");
        let a = spawn_test_node();
        let b = spawn_test_node();
        let a_id = b.dial(a.listen_addr()).await.expect("组网");
        assert_eq!(&a_id, a.self_id());
        let svc_a = ExitService::spawn(
            a.clone(),
            cfg_a,
            Arc::new(FirewallManager::new(test_cfg("e2e2-a-fw"))),
        );
        let svc_b = ExitService::spawn(
            b.clone(),
            cfg_b,
            Arc::new(FirewallManager::new(test_cfg("e2e2-b-fw"))),
        );
        // 就绪等待（双端 SOCKS5）
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (entry_a, _exit_b) = loop {
            let e = *svc_a.entry_addr.lock().unwrap();
            let x = *svc_b.exit_addr.lock().unwrap();
            if let (Some(e), Some(x)) = (e, x) {
                break (e, x);
            }
            assert!(std::time::Instant::now() < deadline, "SOCKS5 就绪超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        // mock echo 目标（多次读写——验证窗口背压下的连续分块）
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = target.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        assert!(svc_b.set_offer(true).await);
        svc_b.authorize(&a.self_id().to_hex(), 30);
        svc_a.set_default_exit(Some(&b.self_id().to_hex()));
        // 用户应用视角：拨 A 的入口 SOCKS5（v2ray 客户端模式）
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut app = socks5_dial(entry_a, "127.0.0.1", target_port)
            .await
            .expect("入口 SOCKS5 应代拨成功");
        app.write_all(b"ping-1").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(10), app.read(&mut buf))
            .await
            .expect("echo 应及时返回（open→data→ack 链路）")
            .expect("读不应出错");
        assert_eq!(&buf[..n], b"ping-1");
        // 大块（> 单块 64KiB，覆盖分块 + 窗口）
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        app.write_all(&big).await.unwrap();
        let mut got = Vec::with_capacity(big.len());
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while got.len() < big.len() {
            assert!(
                std::time::Instant::now() < deadline,
                "大块回传不完整（{}/{})",
                got.len(),
                big.len()
            );
            let n = tokio::time::timeout(Duration::from_secs(10), app.read(&mut buf))
                .await
                .expect("分块应持续到达")
                .expect("读不应出错");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, big, "200KiB 数据经 overlay 分块往返保真");
        drop(app);
        a.shutdown().await;
        b.shutdown().await;
    }
}
