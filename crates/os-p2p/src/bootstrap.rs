//! 冷启动 bootstrap——设计 §3「bootstrap」：mDNS LAN 种子 + env 引导节点 +
//! walk 入网 + 保活（P2a）。
//!
//! # 环境变量（设计 §3 契约 + P2a 扩展 + P2b 密钥持久化）
//!
//! | 变量 | 语义 | 默认 |
//! |---|---|---|
//! | [`ENV_BOOTSTRAP`] `NEXOS_P2P_BOOTSTRAP` | 引导节点列表 `host:port,...` | 空（孤网节点，等入站） |
//! | [`ENV_LISTEN`] `NEXOS_P2P_LISTEN` | 监听地址（支持 `:7070` 省 IP 形式） | `:7070` |
//! | [`ENV_PUBLIC`] `NEXOS_P2P_PUBLIC` | `1/true/yes` = 公网服务节点（bootstrap + relay） | 未设置 |
//! | [`ENV_ADVERTISE`] `NEXOS_P2P_ADVERTISE` | 显式通告地址 `ip:port`（NAT 后云主机；隐含 public=1） | 未设置 |
//! | [`ENV_MDNS`] `NEXOS_P2P_MDNS` | `0/false` = 关闭 mDNS LAN 种子 | 开（standalone 节点行为） |
//! | [`ENV_MDNS_TYPE`] `NEXOS_P2P_MDNS_TYPE` | mDNS 服务类型覆盖（**测试隔离**用） | [`MDNS_SERVICE_TYPE`] |
//! | [`ENV_NAME`] `NEXOS_P2P_NAME` | 节点昵称（p2p-node CLI 展示用） | 空 |
//! | [`ENV_KEY_FILE`] `NEXOS_P2P_KEY_FILE` | secp256k1 私钥文件（hex）——**重启身份稳定**（P2b） | [`default_key_file`] 降级链 |
//! | [`ENV_META_FILE`] `NEXOS_P2P_META_FILE` | 节点元数据注册表文件（JSON）——**重启不丢**（meta 组件） | key_file 同目录 `node-meta.json` |
//!
//! # 密钥持久化（P2b：修 NodeID 漂移）
//!
//! [`load_or_create_identity`]：文件存在 → 读 hex 私钥还原身份；不存在 → CSPRNG
//! 生成并**原子写**（同目录临时文件 + 0600 权限 + rename，中途崩溃不留半截私钥）；
//! 文件损坏（非法 hex/长度）→ 告警日志 + 重新生成覆盖（身份漂移一次，好过永续
//! 崩溃）；目录不可写 → 告警 + 退回内存身份（进程内仍可用，重启换身份）。
//! `config_from_env` 默认走此逻辑——**CLI（p2p-node）与库（os-api 内嵌）同一
//! 份私钥文件，锚点/节点重启身份稳定**。
//!
//! # mDNS LAN 种子（P2a，连接阶梯第 1 级的发现面）
//!
//! LAN 内经 mdns-sd 广播/发现 `_nexos-p2p._tcp`（**与 avahi 的 `_nexos._tcp`
//! 通告区分**——服务类型与端口都不同，两套发现互不串扰）。发现的邻居作为
//! **首选种子**（优先于 env 引导拨号——同网段直连延迟最优）：
//!
//! ```text
//!   ① mdns 首轮窗口（Timing::mdns_first_pass）收集 LAN 邻居并拨号
//!   ② env 引导节点（NEXOS_P2P_BOOTSTRAP）补位
//!   ③ FINDNODE walk 入网（DHT 全局发现）
//!   ④ 保活循环：env 重拨 + 持续吃 mDNS 事件（新邻居上线即拨）
//! ```
//!
//! **mDNS 不可用（无组播环境/容器）静默降级**：`ServiceDaemon` 起不来或 browse
//! 无结果都不报错——直接走 env 引导，网络照常组网。
//!
//! # 冷启动 walk（Swarm Bee 的 ping/findnode/pong 同款流程）
//!
//! ```text
//!  新节点 N                       引导节点 B（公网 NexOS）
//!    │ ── TCP + Hello + 挑战签名 + ECDH ──▶ │        ① 连上已知节点
//!    │ ◀──────────── 认证+加密完成 ──────── │
//!    │ ── FINDNODE(target=N) ──────────────▶ │        ② 问"谁离我最近"
//!    │ ◀── NODES(k 个最近 + 观测端点八卦) ─── │        ③ 地址交换所顺带回灌
//!    │ ── FINDNODE ────────────────────────▶ 更近节点  ④ 沿更近者迭代查询
//!    │ ……直到一轮无更近（收敛）……                       ⑤ 路由表成网
//! ```
//!
//! [`bootstrap_task`] 还承担**引导连接保活**：NAT 节点的生存线是它与引导节点
//! 的出站连接（全网对它的可达性都经此中继），断线即按 `reconnect_backoff`
//! 退避重拨，永不放弃。

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::api::{dial_addr, lookup, Shared};

/// 引导节点列表 env（`host:port,...`）。
pub const ENV_BOOTSTRAP: &str = "NEXOS_P2P_BOOTSTRAP";
/// 监听地址 env（支持 `:7070` 省 IP 形式）。
pub const ENV_LISTEN: &str = "NEXOS_P2P_LISTEN";
/// 公网服务节点声明 env（`1`/`true`/`yes`）。
pub const ENV_PUBLIC: &str = "NEXOS_P2P_PUBLIC";
/// 显式通告地址 env（`ip:port`）：NAT 后的服务节点（如云主机监听 `0.0.0.0`
/// 而对外另有公网 IP）用真实可达地址覆盖"通告监听地址"的默认；设置即隐含
/// public=1。
pub const ENV_ADVERTISE: &str = "NEXOS_P2P_ADVERTISE";
/// mDNS LAN 种子开关 env（默认开；`0`/`false` 关闭——standalone 节点默认在
/// LAN 内可发现，嵌入式场景显式关闭）。
pub const ENV_MDNS: &str = "NEXOS_P2P_MDNS";
/// mDNS 服务类型覆盖 env（**测试隔离**用）：完整服务类型名（形如
/// `_nexos-p2p-test._tcp.local.`——须满足 mdns-sd browse 的域后缀契约），
/// 广播与发现共用。默认不设置 = 生产真实域 [`MDNS_SERVICE_TYPE`]；空/非法值
/// 回落默认（见 [`resolve_mdns_service_type`]）。
///
/// 动机：集成测试（如 ladder 的 mDNS 降级测试）把测试节点切到隔离服务域——
/// 开发机上常有生产 os-api 在真实域 `_nexos-p2p._tcp.local.` 广播，测试节点
/// 若在真实域 browse 会发现并拨入生产 P2P 端口，生产 register_conn 把测试
/// 回环地址（127.0.0.1:xxxx）记入节点元数据注册表（测试污染生产）。
pub const ENV_MDNS_TYPE: &str = "NEXOS_P2P_MDNS_TYPE";
/// 节点昵称 env（p2p-node CLI 展示；非协议字段）。
pub const ENV_NAME: &str = "NEXOS_P2P_NAME";
/// 节点私钥文件 env（hex 私钥；P2b 身份持久化——锚点/节点重启 NodeID 稳定）。
pub const ENV_KEY_FILE: &str = "NEXOS_P2P_KEY_FILE";
/// 节点元数据注册表文件 env（JSON；meta 组件——注册表持久化，重启不丢）。
/// 缺省落 key_file 同目录 `node-meta.json`（与身份同处一地）。
pub const ENV_META_FILE: &str = "NEXOS_P2P_META_FILE";
/// 身份账本文件 env（JSON；os-identity 组件——指纹证据/对比/冲突持久化，
/// 重启不丢）。缺省落 key_file 同目录 `identity-ledger.json`（与身份同处一地）。
/// 注：os-api 装配层用 `/tank/os-data/identity-ledger.json` + `NEXOS_IDENTITY_FILE`
/// 注入共享实例（见 os-api main.rs）；本 env 服务 standalone CLI（p2p-node）。
pub const ENV_IDENTITY_FILE: &str = "NEXOS_IDENTITY_FILE";
/// 本节点网络出口声明（network-exit，2026-08-30）：truthy 时 digest 自广播带
/// `exit_offered:true`（启动默认值；运行期以 os-api network-exit 组件的持久化
/// 状态为准——启动时推送到 p2p）。
pub const ENV_EXIT_OFFER: &str = "NEXOS_P2P_EXIT_OFFER";
/// 默认监听端口。
pub const P2P_PORT_DEFAULT: u16 = 7070;
/// mDNS 服务类型：`_nexos-p2p._tcp.local.`。
///
/// **与 avahi 的 `_nexos._tcp` 通告刻意区分**（服务类型与端口都不同）——
/// os-discover 的 LAN 发现层与本 P2P 组网层各自独立通告，互不串扰。
pub const MDNS_SERVICE_TYPE: &str = "_nexos-p2p._tcp.local.";

/// 解析监听地址：`":7070"` → `0.0.0.0:7070`；`"1.2.3.4:70"` / `"[::1]:70"` 原样。
#[must_use]
pub fn parse_listen(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let host_port = if let Some(rest) = s.strip_prefix(':') {
        format!("0.0.0.0:{rest}")
    } else {
        s.to_string()
    };
    host_port.to_socket_addrs().ok()?.next()
}

/// 解析引导列表（逗号分隔，忽略空白与非法项——部分失败不阻塞冷启动）。
#[must_use]
pub fn parse_bootstrap_list(s: &str) -> Vec<SocketAddr> {
    s.split(',')
        .filter_map(|item| parse_addr(item.trim()))
        .collect()
}

fn parse_addr(s: &str) -> Option<SocketAddr> {
    if s.is_empty() || !s.contains(':') {
        return None;
    }
    s.to_socket_addrs().ok()?.next()
}

/// truthy 判定（`1`/`true`/`yes`，大小写不敏感）。
#[must_use]
pub fn truthy(v: Option<&str>) -> bool {
    v.map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// falsy 判定（显式关闭：`0`/`false`/`no`）。
fn falsy(v: Option<&str>) -> bool {
    v.map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(false)
}

/// 种子合并：**mDNS 发现的 LAN 邻居优先**于 env 引导（去重保序）。
#[must_use]
pub fn merge_seeds(mdns: Vec<SocketAddr>, env: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    mdns.into_iter()
        .chain(env)
        .filter(|a| seen.insert(*a))
        .collect()
}

/// 从环境变量构造节点配置（BOOTSTRAP/LISTEN/PUBLIC/MDNS + KEY_FILE 身份持久化）。
#[must_use]
pub fn config_from_env() -> crate::P2pConfig {
    let listen = std::env::var(ENV_LISTEN)
        .ok()
        .as_deref()
        .and_then(parse_listen)
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], P2P_PORT_DEFAULT)));
    let bootstrap = std::env::var(ENV_BOOTSTRAP)
        .ok()
        .as_deref()
        .map(parse_bootstrap_list)
        .unwrap_or_default();
    let public = truthy(std::env::var(ENV_PUBLIC).ok().as_deref());
    // NAT 后服务节点：显式通告地址（如云主机公网 IP:port）——覆盖
    // "public → 通告监听地址（0.0.0.0 无法被拨到）"的默认；隐含 public=1
    let advertise = std::env::var(ENV_ADVERTISE)
        .ok()
        .as_deref()
        .and_then(parse_addr);
    let public = public || advertise.is_some();
    // mDNS 默认开（standalone 节点在 LAN 内可发现）；显式 0/false 关闭
    let mdns_enabled = !falsy(std::env::var(ENV_MDNS).ok().as_deref());
    // 身份持久化（P2b）：KEY_FILE 显式路径或默认降级链——重启同身份
    let key_file = std::env::var(ENV_KEY_FILE)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_key_file);
    let identity = load_or_create_identity(&key_file);
    // 元数据注册表持久化：显式 env 或 key 同目录 node-meta.json（None 仅在调用方
    // 显式构造 P2pConfig 时出现——config_from_env 路径总有落点）
    let meta_file = std::env::var(ENV_META_FILE)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            Some(
                key_file
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.join("node-meta.json"))
                    .unwrap_or_else(|| std::path::PathBuf::from("node-meta.json")),
            )
        });
    // 身份账本持久化（os-identity 组件）：显式 env 或 key 同目录
    // identity-ledger.json——standalone CLI（p2p-node）重启不丢；os-api 装配
    // 层注入共享实例时覆盖本字段（cfg.identity_ledger）。
    let identity_file = std::env::var(ENV_IDENTITY_FILE)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            Some(
                key_file
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.join("identity-ledger.json"))
                    .unwrap_or_else(|| std::path::PathBuf::from("identity-ledger.json")),
            )
        });
    crate::P2pConfig {
        listen,
        bootstrap,
        public,
        advertise,
        mdns_enabled,
        identity: Some(identity),
        meta_file,
        identity_ledger: Some(std::sync::Arc::new(std::sync::Mutex::new(
            os_identity::IdentityLedger::new(identity_file),
        ))),
        exit_offered: truthy(std::env::var(ENV_EXIT_OFFER).ok().as_deref()),
        ..crate::P2pConfig::default()
    }
}

// ============================================================================
// 密钥持久化（P2b：修 NodeID 漂移）
// ============================================================================

/// 默认私钥文件降级链（与 monitor.rs 的 default_db_path 同款三段式）：
/// `/tank/os-data/p2p-node-key` → `/var/lib/os/p2p-node-key` → `./p2p-node-key`。
///
/// 取**首个父目录存在或可创建**的位置；都不行落在当前目录（保底——写失败时
/// [`load_or_create_identity`] 再退回内存身份并告警）。
#[must_use]
pub fn default_key_file() -> std::path::PathBuf {
    for p in ["/tank/os-data/p2p-node-key", "/var/lib/os/p2p-node-key"] {
        let path = std::path::Path::new(p);
        if path
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return path.to_path_buf();
        }
    }
    std::path::PathBuf::from("./p2p-node-key")
}

/// 加载或生成节点身份（**CLI 与库共用的身份持久化入口**）。
///
/// - 文件存在且为合法 32 字节私钥 hex（可选 `0x` 前缀）→ 还原同一 NodeID；
/// - 文件不存在 → CSPRNG 生成 + 原子写（0600）；
/// - 文件损坏（读不出/非法 hex/长度不符）→ **告警日志 + 重新生成覆盖**
///   （漂移一次好过永续崩溃——观测端点簿/中继注册按 NodeID 记账，旧身份条目
///   靠 TTL 自然过期）；
/// - 写入失败（目录不可写/权限）→ 告警 + 返回内存身份（本进程内照常组网）。
#[must_use]
pub fn load_or_create_identity(path: &std::path::Path) -> crate::NodeIdentity {
    match read_key_hex(path) {
        // ① 合法私钥 → 还原身份（重启同 NodeID）
        Some(hex_seed) => {
            tracing::info!(
                key_file = %path.display(),
                "加载持久化节点身份（NodeID 重启稳定）"
            );
            crate::NodeIdentity::from_seed(&hex_seed)
        }
        // ② 无文件 → 生成 + 原子写
        None => {
            let identity = crate::NodeIdentity::generate();
            match write_key_atomic(path, &identity.to_seed()) {
                Ok(()) => tracing::info!(
                    key_file = %path.display(),
                    node_id = %identity.node_id(),
                    "生成新节点身份并持久化（0600 原子写）"
                ),
                Err(e) => tracing::warn!(
                    key_file = %path.display(),
                    error = %e,
                    "节点私钥写入失败（身份仅存内存，重启将更换 NodeID）"
                ),
            }
            identity
        }
    }
}

/// 读私钥文件并解析为 32 字节种子。
///
/// 返回 None 的两种语义由调用方区分不了"无文件"与"损坏"——损坏时这里先告警
/// （带原因），返回 None 让调用方走"生成 + 覆盖"路径。
fn read_key_hex(path: &std::path::Path) -> Option<[u8; 32]> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                key_file = %path.display(),
                error = %e,
                "节点私钥文件不可读——降级重新生成（身份将漂移一次）"
            );
            return None;
        }
    };
    let hex_str = content.trim().strip_prefix("0x").unwrap_or(content.trim());
    let seed = hex::decode(hex_str).ok()?;
    if seed.len() != 32 {
        tracing::warn!(
            key_file = %path.display(),
            len = seed.len(),
            "节点私钥文件长度非法（应 32 字节 hex）——降级重新生成并覆盖"
        );
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&seed);
    Some(out)
}

/// 原子写私钥：同目录临时文件（`<名>.tmp.<pid>`，0600）→ fsync → rename。
///
/// 0600 私钥文件权限（Unix PermissionsExt）；rename 保证观察者只会看到完整
/// 私钥或旧文件，中途崩溃不留半截内容。父目录不存在时先创建。
fn write_key_atomic(path: &std::path::Path, seed: &[u8; 32]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(format!("0x{}", hex::encode(seed)).as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    // rename 保留临时文件的 0600 权限（Unix）；非 Unix 平台无 PermissionsExt，
    // 权限语义交给 OS 默认（本 crate 部署面为 Linux）
    Ok(())
}

// ============================================================================
// mDNS 种子器
// ============================================================================

/// 解析实际生效的 mDNS 服务类型：[`ENV_MDNS_TYPE`] 覆盖，否则回落默认
/// [`MDNS_SERVICE_TYPE`]。
///
/// 合法覆盖 = trim 后非空、以 `_` 开头、且以 `._tcp.local.` / `._udp.local.`
/// 结尾——与 mdns-sd `browse` 的域后缀契约（`check_domain_suffix`）对齐：
/// 非法值若放行到 browse 会启动失败 → mDNS 整体静默关闭（隔离失效而非
/// 换域），故在入口拦下并告警回落默认。
///
/// 仅在 mDNS 启动路径（[`MdnsSeeder::start`]）解析——生产默认行为不变；
/// 测试经进程内 env 把广播/发现整体切到隔离域，不再 browse 真实域里的
/// 生产服务（见 [`ENV_MDNS_TYPE`] 的动机说明）。非法值告警后回落默认
/// （与 mDNS 整体"不可用即降级"的静默哲学一致——不构成启动错误）。
#[must_use]
pub fn resolve_mdns_service_type() -> String {
    match std::env::var(ENV_MDNS_TYPE) {
        Ok(v) => {
            let v = v.trim();
            let valid =
                v.starts_with('_') && (v.ends_with("._tcp.local.") || v.ends_with("._udp.local."));
            if valid {
                v.to_string()
            } else {
                tracing::warn!(
                    env = ENV_MDNS_TYPE,
                    value = %v,
                    "mDNS 服务类型覆盖非法（应形如 _name._tcp.local.）——回落默认 {MDNS_SERVICE_TYPE}"
                );
                MDNS_SERVICE_TYPE.to_string()
            }
        }
        Err(_) => MDNS_SERVICE_TYPE.to_string(),
    }
}

/// 运行中的 mDNS 种子器（广播自身 + 持续发现邻居）。
pub(crate) struct MdnsSeeder {
    daemon: ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
    /// 自身实例名（过滤自己广播的回声）。
    instance: String,
}

impl MdnsSeeder {
    /// 启动广播 + browse。任何一步失败返回 None——**调用方静默降级** env 引导
    /// （无组播环境/容器/权限不足都是常态，不构成错误）。
    fn start(self_id: &crate::identity::NodeId, listen: SocketAddr) -> Option<Self> {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::info!("mDNS 不可用（{e}），静默降级 env 引导");
                return None;
            }
        };
        // 通告 IP：监听 unspecified 时用 loopback 保底（同机发现仍可用；
        // 多网卡真实 IP 枚举留后续）
        let ip = if listen.ip().is_unspecified() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            listen.ip()
        };
        // 实例名 = 节点公钥尾部 16 hex（DNS 标签安全 + 全网唯一）
        let instance = format!("n-{}", &self_id.to_hex()[50..66]);
        // 服务类型：默认真实域；ENV_MDNS_TYPE 可覆盖（测试隔离——不 browse
        // 真实域里的生产服务）。广播与 browse 必须同类型（自己看不见自己
        // 广播的域就没有 LAN 种子语义了）
        let service_type = resolve_mdns_service_type();
        let info = ServiceInfo::new(
            &service_type,
            &instance,
            &format!("{instance}.local."),
            ip,
            listen.port(),
            None,
        )
        .ok()?;
        if let Err(e) = daemon.register(info) {
            tracing::info!("mDNS 注册失败（{e}），静默降级 env 引导");
            return None;
        }
        let receiver = match daemon.browse(&service_type) {
            Ok(r) => r,
            Err(e) => {
                tracing::info!("mDNS browse 启动失败（{e}），静默降级 env 引导");
                return None;
            }
        };
        tracing::info!(
            service = %service_type,
            %listen,
            "mDNS LAN 种子器上线（广播 + 发现）"
        );
        Some(Self {
            daemon,
            receiver,
            instance,
        })
    }
}

/// 从 browse 事件流收集邻居种子（阻塞式——经 spawn_blocking 调用；自己的
/// 回声按实例名过滤）。`window` 到期即返回当前收集结果。
fn collect_mdns_seeds(
    receiver: &mdns_sd::Receiver<ServiceEvent>,
    own_instance: &str,
    window: Duration,
) -> Vec<SocketAddr> {
    let mut seeds = Vec::new();
    let deadline = Instant::now() + window;
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        match receiver.recv_timeout(remain) {
            Ok(ServiceEvent::ServiceResolved(svc)) => {
                // fullname = "<instance>.<service type>"；自己的回声跳过
                if svc.get_fullname().starts_with(own_instance) {
                    continue;
                }
                for ip in svc.get_addresses_v4() {
                    seeds.push(SocketAddr::new(IpAddr::V4(ip), svc.get_port()));
                }
            }
            Ok(_) => {}      // SearchStarted / ServiceFound（未解析完）等
            Err(_) => break, // 超时/通道关
        }
    }
    seeds
}

// ============================================================================
// 冷启动任务
// ============================================================================

/// 冷启动任务：mDNS 首轮（LAN 邻居优先）→ env 引导 → walk 入网 → 保活循环
/// （env 重拨 + mDNS 持续发现）。
pub(crate) async fn bootstrap_task(shared: Arc<Shared>) {
    let addrs = shared.bootstrap_addrs.clone();
    let mut mdns = if shared.mdns_enabled {
        MdnsSeeder::start(&shared.self_id, shared.listen_addr)
    } else {
        None
    };
    // ① mDNS 首轮窗口（LAN 邻居 = 首选种子）；不可用/无邻居 → 空表静默降级。
    //    flume recv 阻塞语义——经 spawn_blocking 隔离，不占 async worker。
    let mdns_seeds = match &mdns {
        Some(m) => {
            let (rx, own, window) = (
                m.receiver.clone(),
                m.instance.clone(),
                shared.timing.mdns_first_pass,
            );
            tokio::task::spawn_blocking(move || collect_mdns_seeds(&rx, &own, window))
                .await
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    if !mdns_seeds.is_empty() {
        tracing::info!(
            seeds = ?mdns_seeds,
            "mDNS 发现 LAN 邻居（首选种子，优先于 env 引导）"
        );
    }
    // ② 种子拨号（mDNS 优先）+ ③ walk 入网
    for addr in merge_seeds(mdns_seeds, addrs.clone()) {
        let _ = dial_addr(&shared, addr).await;
    }
    lookup(shared.clone(), shared.self_overlay()).await;
    // ④ 保活：断线重拨（NAT 节点的生存线）+ mDNS 新邻居即拨
    let mut shutdown_rx = shared.shutdown_watch();
    loop {
        let backoff = shared.timing.reconnect_backoff;
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            _ = tokio::time::sleep(backoff) => {
                for addr in &addrs {
                    if !shared.connected_to_addr(addr) {
                        let _ = dial_addr(&shared, *addr).await;
                    }
                }
                if let Some(m) = &mdns {
                    let (rx, own) = (m.receiver.clone(), m.instance.clone());
                    // recv_timeout(ZERO) 非阻塞收割积压事件
                    for seed in collect_mdns_seeds(&rx, &own, Duration::ZERO) {
                        if !shared.connected_to_addr(&seed) {
                            let _ = dial_addr(&shared, seed).await;
                        }
                    }
                }
            }
        }
    }
    if let Some(m) = mdns.take() {
        let _ = m.daemon.shutdown();
    }
}

// ============================================================================
// 单元测——env 解析 / 种子合并
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // 1. 监听地址解析：省 IP / 全格式 / 非法
    #[test]
    fn parse_listen_variants() {
        assert_eq!(
            parse_listen(":7070"),
            Some(SocketAddr::from(([0, 0, 0, 0], 7070))),
            "省 IP 形式默认全网卡"
        );
        assert_eq!(
            parse_listen("127.0.0.1:7080"),
            Some(SocketAddr::from(([127, 0, 0, 1], 7080)))
        );
        assert!(parse_listen("[::1]:7090").is_some(), "IPv6 字面量");
        assert_eq!(parse_listen(""), None);
        assert_eq!(parse_listen("nonsense"), None);
        assert_eq!(parse_listen("99999"), None);
    }

    // 2. 引导列表解析：多地址 / 空白 / 非法项跳过
    #[test]
    fn parse_bootstrap_list_variants() {
        let list = parse_bootstrap_list("203.0.113.10:7070, 198.51.100.7:7001,,bogus");
        assert_eq!(
            list,
            vec![
                SocketAddr::from(([203, 0, 113, 10], 7070)),
                SocketAddr::from(([198, 51, 100, 7], 7001)),
            ]
        );
        assert!(parse_bootstrap_list("").is_empty());
        assert!(parse_bootstrap_list("a,b,c").is_empty());
    }

    // 3. truthy 判定
    #[test]
    fn truthy_values() {
        for yes in ["1", "true", "TRUE", "Yes", " yes "] {
            assert!(truthy(Some(yes)), "{yes} 应为真");
        }
        for no in ["0", "false", "", "off", "2"] {
            assert!(!truthy(Some(no)), "{no} 应为假");
        }
        assert!(!truthy(None));
    }

    // 4. 种子合并：mDNS 邻居优先于 env 引导；去重保序；空侧直通
    #[test]
    fn merge_seeds_prefers_mdns() {
        let lan: Vec<SocketAddr> = vec!["192.168.1.20:7070".parse().unwrap()];
        let env: Vec<SocketAddr> = vec![
            "203.0.113.10:7070".parse().unwrap(),
            "198.51.100.7:7001".parse().unwrap(),
        ];
        let merged = merge_seeds(lan.clone(), env.clone());
        assert_eq!(
            merged,
            vec![
                "192.168.1.20:7070".parse().unwrap(),
                "203.0.113.10:7070".parse().unwrap(),
                "198.51.100.7:7001".parse().unwrap(),
            ],
            "LAN 邻居在最前（优先拨号）"
        );
        // 重复去重
        let dup = merge_seeds(env.clone(), env.clone());
        assert_eq!(dup, env, "重复种子去重");
        assert!(merge_seeds(vec![], vec![]).is_empty());
        assert_eq!(merge_seeds(vec![], env.clone()), env);
        assert_eq!(merge_seeds(lan, vec![]).len(), 1);
    }

    // 5. 服务类型区分：_nexos-p2p._tcp ≠ avahi 的 _nexos._tcp（两套发现不串扰）
    #[test]
    fn mdns_service_type_distinct_from_avahi() {
        assert_eq!(MDNS_SERVICE_TYPE, "_nexos-p2p._tcp.local.");
        assert!(
            !MDNS_SERVICE_TYPE.starts_with("_nexos._tcp"),
            "与 avahi 通告区分"
        );
        assert_ne!(MDNS_SERVICE_TYPE, "_nexos._tcp.local.");
    }

    // 5b. 服务类型 env 覆盖（测试隔离）：未设置 → 默认；合法覆盖生效；
    //     空/缺 `._tcp.local.` 后缀等非法值回落默认（mdns-sd browse 契约，
    //     非法值放行会致 browse 失败 → mDNS 整体静默关闭——隔离失效）
    #[test]
    fn mdns_service_type_env_override() {
        let saved = std::env::var(ENV_MDNS_TYPE).ok();
        unsafe {
            std::env::remove_var(ENV_MDNS_TYPE);
        }
        assert_eq!(
            resolve_mdns_service_type(),
            MDNS_SERVICE_TYPE,
            "未设置时走默认真实域"
        );
        unsafe {
            std::env::set_var(ENV_MDNS_TYPE, "_nexos-p2p-test._tcp.local.");
        }
        assert_eq!(
            resolve_mdns_service_type(),
            "_nexos-p2p-test._tcp.local.",
            "合法覆盖生效（测试隔离域）"
        );
        unsafe {
            std::env::set_var(ENV_MDNS_TYPE, "  ");
        }
        assert_eq!(
            resolve_mdns_service_type(),
            MDNS_SERVICE_TYPE,
            "空值回落默认"
        );
        unsafe {
            std::env::set_var(ENV_MDNS_TYPE, "nexos-p2p-test._tcp.local.");
        }
        assert_eq!(
            resolve_mdns_service_type(),
            MDNS_SERVICE_TYPE,
            "非法值（非 _ 前缀）回落默认"
        );
        unsafe {
            std::env::set_var(ENV_MDNS_TYPE, "_nexos-p2p-test.local.");
        }
        assert_eq!(
            resolve_mdns_service_type(),
            MDNS_SERVICE_TYPE,
            "非法值（缺 ._tcp/._udp 后缀——mdns-sd browse 会拒）回落默认"
        );
        unsafe {
            std::env::set_var(ENV_MDNS_TYPE, "_nexos-p2p-test._udp.local.");
        }
        assert_eq!(
            resolve_mdns_service_type(),
            "_nexos-p2p-test._udp.local.",
            "._udp 后缀同为合法"
        );
        match saved {
            Some(v) => unsafe {
                std::env::set_var(ENV_MDNS_TYPE, v);
            },
            None => unsafe {
                std::env::remove_var(ENV_MDNS_TYPE);
            },
        }
    }

    // 6. env → config（单测内串行改 env——cargo 多测试同进程并行，此处独占）
    #[test]
    fn config_from_env_reads_all_vars() {
        let saved: Vec<(String, String)> = [
            ENV_BOOTSTRAP,
            ENV_LISTEN,
            ENV_PUBLIC,
            ENV_ADVERTISE,
            ENV_MDNS,
            ENV_KEY_FILE,
            ENV_META_FILE,
        ]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
        // KEY_FILE 指到临时文件——测试不污染系统降级链位置
        let key_tmp = std::env::temp_dir().join(format!("p2p-key-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&key_tmp);
        unsafe {
            // 测试进程内环境变量操作（Rust 2024 起为 unsafe；此处单线程独占）
            std::env::set_var(ENV_BOOTSTRAP, "203.0.113.10:7070,198.51.100.7:7001");
            std::env::set_var(ENV_LISTEN, ":7171");
            std::env::set_var(ENV_PUBLIC, "1");
            std::env::set_var(ENV_KEY_FILE, &key_tmp);
            std::env::remove_var(ENV_MDNS);
            std::env::remove_var(ENV_META_FILE);
        }
        let cfg = config_from_env();
        assert_eq!(cfg.listen.port(), 7171);
        assert_eq!(cfg.bootstrap.len(), 2);
        assert_eq!(cfg.bootstrap[0].port(), 7070);
        assert!(cfg.public);
        assert!(cfg.mdns_enabled, "mDNS 默认开（standalone 节点行为）");
        // META_FILE 缺省：key 同目录 node-meta.json（与身份同处一地）
        assert_eq!(
            cfg.meta_file,
            Some(key_tmp.parent().unwrap().join("node-meta.json")),
            "元数据注册表缺省落 key 文件同目录"
        );
        // ADVERTISE：显式通告覆盖监听地址 + 隐含 public（NAT 后云主机语义）
        unsafe {
            std::env::remove_var(ENV_PUBLIC);
            std::env::set_var(ENV_ADVERTISE, "203.0.113.2:7070");
        }
        let cfg2 = config_from_env();
        assert_eq!(
            cfg2.advertise,
            Some("203.0.113.2:7070".parse().unwrap()),
            "通告地址应取 env 而非监听地址"
        );
        assert!(cfg2.public, "设置 ADVERTISE 隐含 public=1");
        unsafe {
            std::env::set_var(ENV_PUBLIC, "1");
            std::env::remove_var(ENV_ADVERTISE);
        }
        // 身份持久化（P2b）：config_from_env 注入 identity 且文件已落盘
        let id1 = cfg
            .identity
            .as_ref()
            .expect("config_from_env 应注入持久化身份");
        assert!(
            key_tmp.exists(),
            "私钥文件应已生成（{}）",
            key_tmp.display()
        );
        // 再读一次 → 同一 NodeID（重启语义）
        let id2 = config_from_env().identity.expect("二次读取同样有身份");
        assert_eq!(id1.node_id(), id2.node_id(), "重启身份稳定");
        unsafe {
            std::env::set_var(ENV_MDNS, "0");
        }
        assert!(!config_from_env().mdns_enabled, "显式 0 关闭 mDNS");
        // META_FILE 显式覆盖（集群运维把注册表挪到独立数据盘）
        let meta_tmp = std::env::temp_dir().join(format!("p2p-meta-test-{}", std::process::id()));
        unsafe {
            std::env::set_var(ENV_META_FILE, &meta_tmp);
        }
        assert_eq!(
            config_from_env().meta_file,
            Some(meta_tmp.clone()),
            "显式 META_FILE 覆盖默认落点"
        );
        unsafe {
            std::env::remove_var(ENV_META_FILE);
            std::env::remove_var(ENV_PUBLIC);
            std::env::remove_var(ENV_LISTEN);
            std::env::remove_var(ENV_BOOTSTRAP);
            std::env::remove_var(ENV_MDNS);
            // KEY_FILE 先留着：下面这次 config_from_env 仍走临时文件，
            // 不在系统降级链位置产生真实私钥副作用
        }
        let cfg = config_from_env();
        assert_eq!(cfg.listen.port(), P2P_PORT_DEFAULT);
        assert!(cfg.bootstrap.is_empty() && !cfg.public);
        assert!(cfg.advertise.is_none());
        assert!(cfg.mdns_enabled);
        unsafe {
            // 全部断言完成后才摘 KEY_FILE（此刻起不再调用 config_from_env）
            std::env::remove_var(ENV_KEY_FILE);
        }
        for (k, v) in saved {
            unsafe {
                std::env::set_var(k, v);
            }
        }
        let _ = std::fs::remove_file(&key_tmp);
    }

    // 7. 密钥持久化（P2b）：生成 → 重启（再次加载）→ 同一 NodeID；文件 hex 往返
    #[test]
    fn identity_persists_across_restart() {
        let dir = std::env::temp_dir().join(format!("p2p-keydir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("node-key");
        let _ = std::fs::remove_file(&key);
        let first = load_or_create_identity(&key);
        // 文件已写入且是 0x + 64 hex
        let content = std::fs::read_to_string(&key).expect("私钥文件应存在");
        assert!(content.trim().starts_with("0x"), "带 0x 前缀: {content}");
        assert_eq!(content.trim().len(), 66, "0x + 64 hex");
        // "重启"：再次加载 → 同身份（NodeID 不漂移）
        let second = load_or_create_identity(&key);
        assert_eq!(first.node_id(), second.node_id(), "重启同 NodeID");
        assert_eq!(first.to_seed(), second.to_seed());
        // 与种子还原一致（hex 往返）
        let from_file = read_key_hex(&key).expect("文件可读回");
        assert_eq!(
            crate::NodeIdentity::from_seed(&from_file).node_id(),
            first.node_id()
        );
        // Unix 下权限 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "私钥文件应 0600");
        }
        let _ = std::fs::remove_file(&key);
        let _ = std::fs::remove_dir(&dir);
    }

    // 8. 损坏私钥文件：降级重新生成（覆盖为新合法 hex）+ 新身份自洽
    #[test]
    fn corrupt_key_file_regenerates_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("p2p-keydir-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("node-key");
        // 损坏形态一：非 hex 内容
        std::fs::write(&key, "this is not a private key!!!").unwrap();
        let regenerated = load_or_create_identity(&key);
        let healed = std::fs::read_to_string(&key).expect("损坏文件应被覆盖重写");
        assert_eq!(healed.trim().len(), 66, "覆盖为合法 0x+64 hex");
        // 覆盖后的文件能还原同一身份（自愈闭环）
        assert_eq!(
            load_or_create_identity(&key).node_id(),
            regenerated.node_id(),
            "覆盖后再加载身份稳定"
        );
        // 损坏形态二：长度不对（半截私钥）
        std::fs::write(&key, "0xdeadbeef").unwrap();
        let regenerated2 = load_or_create_identity(&key);
        assert_eq!(
            regenerated2.node_id(),
            load_or_create_identity(&key).node_id()
        );
        assert_ne!(
            regenerated.node_id(),
            regenerated2.node_id(),
            "两次重生成不同身份"
        );
        // 损坏形态三：非法 hex 字符（长度凑够 64）
        std::fs::write(&key, format!("0x{}", "z".repeat(64))).unwrap();
        let _ = load_or_create_identity(&key);
        assert_eq!(
            std::fs::read_to_string(&key).unwrap().trim().len(),
            66,
            "非法 hex 同样触发覆盖"
        );
        let _ = std::fs::remove_file(&key);
        let _ = std::fs::remove_dir(&dir);
    }

    // 9. 不可写目录：不 panic，退回内存身份（本进程仍可用）
    #[test]
    fn unwritable_key_file_degrades_to_ephemeral() {
        let path = std::path::Path::new("/proc/nexos-p2p-definitely-unwritable/key");
        let id = load_or_create_identity(path);
        // 未 panic 且拿到可用身份（可派生 NodeID / 可签名）
        let _ = id.node_id().to_hex();
        let sig = id.sign("nonce");
        assert_eq!(sig.len(), 65);
        assert!(id.node_id().verify_signature("nonce", &sig));
    }

    // 10. 默认私钥文件降级链：末端文件名固定；取链上首个父目录可用位置
    #[test]
    fn default_key_file_chain_shape() {
        let p = default_key_file();
        assert!(
            p.ends_with("p2p-node-key"),
            "链上任意位置文件名固定: {}",
            p.display()
        );
        // 结果必须来自降级链（顺序语义：/tank/os-data → /var/lib/os → ./），
        // 即等于链上首个"父目录存在或可创建"的候选——不写死机器状态
        let chain = [
            "/tank/os-data/p2p-node-key",
            "/var/lib/os/p2p-node-key",
            "./p2p-node-key",
        ];
        let expected = chain
            .iter()
            .find(|c| {
                std::path::Path::new(c)
                    .parent()
                    .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
            })
            .unwrap_or(&chain[2]);
        assert_eq!(p, std::path::Path::new(expected), "降级链顺序语义");
    }
}
