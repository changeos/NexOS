//! 后端实现骨架：命令编排（不执行）
//!
//! 本模块为 `NetworkManager` / `Firewall` 的默认后端（`NetlinkManager` / `NftFirewall`）
//! 提供**纯函数式命令构造层**——只生成 `ip`/`nft` 命令的参数向量，**不**调用 `tokio::process`
//! 真正执行，避免误配断网（红线：不在本机真改网络配置）。
//!
//! ## 阻塞项（外部依赖未在 workspace 注册）
//! - `rtnetlink`：未注册，`NetlinkManager` 的真实 netlink 通信待引入后填充；
//! - `nftnl`：未注册，`NftFirewall` 的真实 nftables 事务待引入后填充。
//!
//! 当前阶段：构造命令参数 + 合法性校验（dry-run 的纯逻辑部分），并配单元测试。
//! 真实执行（spawn 进程 / netlink socket）留 `TODO(netlink-exec)` / `TODO(nftnl-exec)`
//! \[RUNTIME\]——需 root/CAP_NET_ADMIN + 内核 netfilter/rtnetlink 子系统。
//!
//! ## 权限说明
//! 所有 `ip link set` / `nft add rule` 等操作均需 **root 或 CAP_NET_ADMIN**。
//! 沙箱/CI 应使用 `mock` feature 的 `Mock*` 类型，避免特权依赖。

use crate::firewall::{FirewallAction, FirewallRule, Protocol};
use crate::interface::{validate_vlan_id, InterfaceId, InterfaceType, IpCidr};

/// `ip` 命令的程序名（便于测试断言与统一替换）。
pub const IP_BIN: &str = "ip";
/// `nft` 命令的程序名。
pub const NFT_BIN: &str = "nft";

// ============================================================================
// ip 命令参数构造（NetlinkManager 的命令编排层）
// ============================================================================

/// 构造 `ip link set <id> up` 参数向量。
///
/// 返回 `["link", "set", "dev", "<id>", "up"]`（实际执行需 root/CAP_NET_ADMIN）。
pub fn ip_link_up_args(id: &InterfaceId) -> Vec<String> {
    vec![
        "link".into(),
        "set".into(),
        "dev".into(),
        id.as_str().into(),
        "up".into(),
    ]
}

/// 构造 `ip link set <id> down` 参数向量。
pub fn ip_link_down_args(id: &InterfaceId) -> Vec<String> {
    vec![
        "link".into(),
        "set".into(),
        "dev".into(),
        id.as_str().into(),
        "down".into(),
    ]
}

/// 构造 `ip link add link <parent> name <name> type vlan id <vid>` 参数向量。
///
/// 先做 VLAN ID 前置校验（1..=4094），失败返回 `NetworkError`。
pub fn ip_link_add_vlan_args(
    parent: &InterfaceId,
    vid: u16,
    name: &InterfaceId,
) -> Result<Vec<String>, crate::NetworkError> {
    validate_vlan_id(vid)?;
    Ok(vec![
        "link".into(),
        "add".into(),
        "link".into(),
        parent.as_str().into(),
        "name".into(),
        name.as_str().into(),
        "type".into(),
        "vlan".into(),
        "id".into(),
        vid.to_string(),
    ])
}

/// 构造 `ip link add <name> type bridge` 参数向量。
pub fn ip_link_add_bridge_args(name: &InterfaceId) -> Vec<String> {
    vec![
        "link".into(),
        "add".into(),
        name.as_str().into(),
        "type".into(),
        "bridge".into(),
    ]
}

/// 构造 `ip link add <name> type bond` 参数向量。
///
/// bond mode 通过 `ip link set <name> type bond mode <mode>` 单独设置（见
/// `ip_link_set_bond_mode_args`），因为 `ip link add` 阶段不直接带 mode。
pub fn ip_link_add_bond_args(name: &InterfaceId) -> Vec<String> {
    vec![
        "link".into(),
        "add".into(),
        name.as_str().into(),
        "type".into(),
        "bond".into(),
    ]
}

/// 构造 `ip link set <name> type bond mode <mode>` 参数向量。
pub fn ip_link_set_bond_mode_args(
    name: &InterfaceId,
    mode: crate::interface::BondMode,
) -> Vec<String> {
    let mode_str = match mode {
        crate::interface::BondMode::ActiveBackup => "active-backup",
        crate::interface::BondMode::BalanceRr => "balance-rr",
        crate::interface::BondMode::Lacp => "802.3ad",
        crate::interface::BondMode::Broadcast => "broadcast",
    };
    vec![
        "link".into(),
        "set".into(),
        "dev".into(),
        name.as_str().into(),
        "type".into(),
        "bond".into(),
        "mode".into(),
        mode_str.into(),
    ]
}

/// 构造 `ip link set dev <id> master <bond>` 参数向量（把 slave 加入 bond）。
pub fn ip_link_enslave_args(slave: &InterfaceId, bond: &InterfaceId) -> Vec<String> {
    vec![
        "link".into(),
        "set".into(),
        "dev".into(),
        slave.as_str().into(),
        "master".into(),
        bond.as_str().into(),
    ]
}

/// 构造 `ip link delete <id>` 参数向量。
///
/// 调用方需先拒绝删除物理接口（契约语义：仅可删 VLAN/桥/绑定）。
pub fn ip_link_delete_args(id: &InterfaceId) -> Vec<String> {
    vec![
        "link".into(),
        "delete".into(),
        "dev".into(),
        id.as_str().into(),
    ]
}

/// 构造 `ip addr flush dev <id>` + `ip addr add <cidr> dev <id>`（覆盖地址列表）的参数序列。
///
/// 先 flush 再逐条 add；返回多组参数（每组即一条命令的 argv）。
pub fn ip_addr_set_args(
    id: &InterfaceId,
    addrs: &[IpCidr],
) -> Result<Vec<Vec<String>>, crate::NetworkError> {
    let mut cmds = Vec::with_capacity(1 + addrs.len());
    cmds.push(vec![
        "addr".into(),
        "flush".into(),
        "dev".into(),
        id.as_str().into(),
    ]);
    for a in addrs {
        a.validate()?;
        cmds.push(vec![
            "addr".into(),
            "add".into(),
            a.to_string(),
            "dev".into(),
            id.as_str().into(),
        ]);
    }
    Ok(cmds)
}

/// 构造 `ip -j -o link show` 参数向量（JSON 输出，供 `list_interfaces` 解析）。
pub fn ip_link_show_json_args() -> Vec<String> {
    vec!["-j".into(), "-o".into(), "link".into(), "show".into()]
}

// ============================================================================
// nft 命令参数构造（NftFirewall 的命令编排层）
// ============================================================================

/// 把 `Protocol` 映射为 nft 关键字（`tcp` / `udp` / 省略表示 any）。
pub fn nft_protocol_keyword(p: Protocol) -> Option<&'static str> {
    match p {
        Protocol::Tcp => Some("tcp"),
        Protocol::Udp => Some("udp"),
        Protocol::Any => None,
    }
}

/// 把 `FirewallAction` 映射为 nft verdict 关键字（`accept` / `drop` / `redirect`）。
pub fn nft_action_keyword(a: FirewallAction) -> &'static str {
    match a {
        FirewallAction::Allow => "accept",
        FirewallAction::Deny => "drop",
        FirewallAction::Redirect => "redirect",
    }
}

/// 构造单条 nft 规则的规则体字符串（不含 `add rule ...` 前缀）。
///
/// 例：`tcp dport 80 accept`、`udp sport 1000-2000 ip daddr 10.0.0.1 drop`。
/// `redirect` 动作附加 `to :<target_port>`。
///
/// 规则体合法性由 `FirewallRule::validate()` 保证（调用方应先 dry-run）。
pub fn nft_rule_body(rule: &FirewallRule) -> Result<String, crate::NetworkError> {
    rule.validate()?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(proto) = nft_protocol_keyword(rule.protocol) {
        parts.push(proto.into());
    }
    if let Some(src) = rule.src_addr {
        parts.push(format!("ip saddr {src}"));
    }
    if let Some(sp) = &rule.src_port {
        parts.push(format!("sport {sp}"));
    }
    if let Some(dst) = rule.dst_addr {
        parts.push(format!("ip daddr {dst}"));
    }
    if let Some(dp) = &rule.dst_port {
        parts.push(format!("dport {dp}"));
    }
    let action = nft_action_keyword(rule.action);
    match rule.action {
        FirewallAction::Redirect => {
            // target_port 已由 validate 保证存在且合法
            let tp = rule.target_port.unwrap();
            parts.push(format!("{action} to :{tp}"));
        }
        _ => parts.push(action.into()),
    }
    Ok(parts.join(" "))
}

/// 构造 `add rule <table> <chain> <body>` 参数向量。
///
/// `table` / `chain` 由调用方决定（如 `"inet filter"` / `"input"`）。
pub fn nft_add_rule_args(
    table: &str,
    chain: &str,
    rule: &FirewallRule,
) -> Result<Vec<String>, crate::NetworkError> {
    let body = nft_rule_body(rule)?;
    Ok(vec![
        "add".into(),
        "rule".into(),
        table.into(),
        chain.into(),
        body,
    ])
}

// ============================================================================
// 后端 trait：把"命令构造/合法性校验"与"真实执行"解耦
// ============================================================================
//
// 设计动机（呼应 docs/SANDBOX.md §1、network-agent.md §9 红线）：
// rtnetlink / nftnl 真实执行需 root + CAP_NET_ADMIN（且 nftnl 需 libnftnl-dev
// FFI）。把执行层抽象成 trait，使：
//   - 单元测/CI 注入内存后端（`InMemoryNetlinkBackend` / `InMemoryFirewallBackend`），
//     跑通"构造 → 校验 → 提交 → 回读"完整真实路径逻辑，不依赖特权；
//   - 生产/沙箱注入真实后端（`RtnetlinkBackend` / `NftnlFirewallBackend`），
//     真改内核网络配置；
//   - 真实后端的"硬依赖"（FFI）按 feature 门控，缺依赖时不进编译产物。
//
// 注：trait 用原生 async fn in trait（与 `NetworkManager` / `Firewall` 一致，
// 非 dyn 兼容，见 ADR-COMPAT-001）。后端作为具体类型注入 `NetlinkManager<B>` /
// `NftFirewall<B>` 的泛型参数。

/// 接口管理执行后端——把 `NetworkManager` 的语义操作落到具体的 netlink/socket/内存。
///
/// 实现者：
/// - `InMemoryNetlinkBackend`：纯内存（测试/CI 默认）；
/// - `RtnetlinkBackend`：真实 rtnetlink 通信（生产，需 root/CAP_NET_ADMIN）。
///
/// 方法语义与 `NetworkManager` 同名 trait 方法一一对应；此处只关注"执行"，
/// 合法性校验（VLAN ID 范围、接口名长度、CIDR 前缀等）由 `NetlinkManager`
/// 在调用前完成（复用 `validate_*` 纯函数）。
#[allow(async_fn_in_trait)]
pub trait NetlinkBackend: Send + Sync {
    /// 列出所有接口（含物理/虚拟）。
    async fn list_interfaces(
        &self,
    ) -> Result<Vec<crate::interface::Interface>, crate::NetworkError>;

    /// 查询指定接口；不存在返回 `InterfaceNotFound`。
    async fn get_interface(
        &self,
        id: &InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError>;

    /// 创建 VLAN 子接口（前置 VLAN ID 校验已由调用方完成）。
    async fn create_vlan(
        &self,
        parent: &InterfaceId,
        vid: u16,
        name: InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError>;

    /// 创建软件桥。
    async fn create_bridge(
        &self,
        name: InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError>;

    /// 创建链路聚合（bond）。
    async fn create_bond(
        &self,
        name: InterfaceId,
        mode: crate::interface::BondMode,
        slaves: Vec<InterfaceId>,
    ) -> Result<crate::interface::Interface, crate::NetworkError>;

    /// 设置接口地址（覆盖原地址列表）。
    async fn set_address(
        &self,
        id: &InterfaceId,
        addrs: Vec<IpCidr>,
    ) -> Result<(), crate::NetworkError>;

    /// 启用接口（up）。
    async fn up(&self, id: &InterfaceId) -> Result<(), crate::NetworkError>;

    /// 禁用接口（down）。
    async fn down(&self, id: &InterfaceId) -> Result<(), crate::NetworkError>;

    /// 删除接口（仅可删虚拟接口；物理接口拒绝——由实现判断类型后决定）。
    async fn delete_interface(&self, id: &InterfaceId) -> Result<(), crate::NetworkError>;
}

/// 防火墙执行后端——把 `Firewall` 的语义操作落到具体的 nftables 事务/内存。
///
/// 实现者：
/// - `InMemoryFirewallBackend`：纯内存（测试/CI 默认）；
/// - `NftnlFirewallBackend`：真实 nftnl netlink 事务（feature `nftnl-ffi`，需
///   libnftnl-dev + libmnl-dev + root/CAP_NET_ADMIN）。
#[allow(async_fn_in_trait)]
pub trait FirewallBackend: Send + Sync {
    /// 列出当前生效的规则。
    async fn list_rules(&self) -> Result<Vec<FirewallRule>, crate::NetworkError>;

    /// 提交一条规则（dry-run 已由调用方完成），返回规则 ID。
    async fn add_rule(&self, rule: FirewallRule) -> Result<String, crate::NetworkError>;

    /// 删除指定规则。
    async fn delete_rule(&self, id: &str) -> Result<(), crate::NetworkError>;

    /// 新增 NAT 规则。
    async fn add_nat(&self, rule: crate::firewall::NatRule) -> Result<(), crate::NetworkError>;

    /// 删除 NAT 规则。
    async fn delete_nat(&self, rule: &crate::firewall::NatRule) -> Result<(), crate::NetworkError>;
}

// ============================================================================
// 内存后端（无特权依赖，单测/CI 默认）
// ============================================================================

/// 内存版 `NetlinkBackend`：维护接口列表，记录操作序列。
///
/// 默认预置一个 `lo`（回环）接口，模拟最小内核拓扑。所有写操作即时反映到
/// `list_interfaces` / `get_interface` 的回读，便于测试断言"提交后可回读"。
#[derive(Debug, Default)]
pub struct InMemoryNetlinkBackend {
    inner: std::sync::Mutex<InMemoryNetState>,
}

#[derive(Debug, Default)]
struct InMemoryNetState {
    interfaces: Vec<crate::interface::Interface>,
    /// 记录 up/down/delete 等调用（按顺序），供断言。
    ops: Vec<String>,
    /// 自增 ifindex（模拟内核分配）。
    next_index: u32,
}

impl InMemoryNetlinkBackend {
    /// 构造一个空内存后端。
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造并预置回环接口（`lo`，Up，127.0.0.1/8）。
    pub fn with_loopback() -> Self {
        let mut st = InMemoryNetState {
            next_index: 1,
            ..Default::default()
        };
        st.interfaces.push(
            crate::interface::Interface::new(InterfaceId::new("lo"), InterfaceType::Loopback)
                .with_state(crate::interface::IfState::Up)
                .with_addr(IpCidr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    8,
                )),
        );
        Self {
            inner: std::sync::Mutex::new(st),
        }
    }

    /// 预置一个接口（不触发校验，直接插入；测试 fixture 用）。
    pub fn with_interface(self, iface: crate::interface::Interface) -> Self {
        self.inner
            .lock()
            .expect("in-memory net poisoned")
            .interfaces
            .push(iface);
        self
    }

    /// 取已记录的操作序列（供测试断言）。
    pub fn recorded_ops(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("in-memory net poisoned")
            .ops
            .clone()
    }

    /// 分配下一个 ifindex（内部辅助）。
    fn alloc_index(&self, st: &mut InMemoryNetState) -> u32 {
        st.next_index += 1;
        st.next_index
    }
}

#[allow(async_fn_in_trait)]
impl NetlinkBackend for InMemoryNetlinkBackend {
    async fn list_interfaces(
        &self,
    ) -> Result<Vec<crate::interface::Interface>, crate::NetworkError> {
        Ok(self
            .inner
            .lock()
            .expect("in-memory net poisoned")
            .interfaces
            .clone())
    }

    async fn get_interface(
        &self,
        id: &InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        self.inner
            .lock()
            .expect("in-memory net poisoned")
            .interfaces
            .iter()
            .find(|i| i.id == *id)
            .cloned()
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))
    }

    async fn create_vlan(
        &self,
        parent: &InterfaceId,
        vid: u16,
        name: InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        // 复用命令构造层的校验（保持单点真理）
        let _ = ip_link_add_vlan_args(parent, vid, &name)?;
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let _idx = self.alloc_index(&mut st);
        let iface =
            crate::interface::Interface::new(name, InterfaceType::Vlan).with_parent(parent.clone());
        st.interfaces.push(iface.clone());
        st.ops
            .push(format!("create_vlan:{}:{}:{}", parent, vid, iface.id));
        Ok(iface)
    }

    async fn create_bridge(
        &self,
        name: InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        crate::interface::Interface::validate_name(name.as_str())?;
        let _ = ip_link_add_bridge_args(&name);
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let _idx = self.alloc_index(&mut st);
        let iface = crate::interface::Interface::new(name, InterfaceType::Bridge);
        st.interfaces.push(iface.clone());
        st.ops.push(format!("create_bridge:{}", iface.id));
        Ok(iface)
    }

    async fn create_bond(
        &self,
        name: InterfaceId,
        mode: crate::interface::BondMode,
        slaves: Vec<InterfaceId>,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        crate::interface::Interface::validate_name(name.as_str())?;
        let _add = ip_link_add_bond_args(&name);
        let _mode = ip_link_set_bond_mode_args(&name, mode);
        for s in &slaves {
            let _ = ip_link_enslave_args(s, &name);
        }
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let _idx = self.alloc_index(&mut st);
        let mut iface = crate::interface::Interface::new(name, InterfaceType::Bond);
        if let Some(first) = slaves.into_iter().next() {
            iface = iface.with_parent(first);
        }
        st.interfaces.push(iface.clone());
        st.ops.push(format!("create_bond:{}:{:?}", iface.id, mode));
        Ok(iface)
    }

    async fn set_address(
        &self,
        id: &InterfaceId,
        addrs: Vec<IpCidr>,
    ) -> Result<(), crate::NetworkError> {
        // 复用命令构造层的 CIDR 校验
        let _cmds = ip_addr_set_args(id, &addrs)?;
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let iface = st
            .interfaces
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        iface.addrs = addrs;
        st.ops.push(format!("set_address:{}", id));
        Ok(())
    }

    async fn up(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        let _args = ip_link_up_args(id);
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let iface = st
            .interfaces
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        iface.state = crate::interface::IfState::Up;
        st.ops.push(format!("up:{}", id));
        Ok(())
    }

    async fn down(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        let _args = ip_link_down_args(id);
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let iface = st
            .interfaces
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        iface.state = crate::interface::IfState::Down;
        st.ops.push(format!("down:{}", id));
        Ok(())
    }

    async fn delete_interface(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        let _args = ip_link_delete_args(id);
        let mut st = self.inner.lock().expect("in-memory net poisoned");
        let pos = st
            .interfaces
            .iter()
            .position(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        // 契约：物理/回环接口拒绝删除
        if !is_deletable(st.interfaces[pos].ty) {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "物理/回环接口不可删除: {id}"
            )));
        }
        st.interfaces.remove(pos);
        st.ops.push(format!("delete:{}", id));
        Ok(())
    }
}

/// 内存版 `FirewallBackend`：维护规则与 NAT 列表。
#[derive(Debug, Default)]
pub struct InMemoryFirewallBackend {
    inner: std::sync::Mutex<InMemoryFwState>,
}

#[derive(Debug, Default)]
struct InMemoryFwState {
    rules: Vec<(String, FirewallRule)>,
    nats: Vec<crate::firewall::NatRule>,
    next_id: u64,
}

impl InMemoryFirewallBackend {
    /// 构造空内存后端。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前规则数（测试断言用）。
    pub fn rule_count(&self) -> usize {
        self.inner
            .lock()
            .expect("in-memory fw poisoned")
            .rules
            .len()
    }

    /// 当前 NAT 规则快照。
    pub fn nat_snapshot(&self) -> Vec<crate::firewall::NatRule> {
        self.inner
            .lock()
            .expect("in-memory fw poisoned")
            .nats
            .clone()
    }
}

#[allow(async_fn_in_trait)]
impl FirewallBackend for InMemoryFirewallBackend {
    async fn list_rules(&self) -> Result<Vec<FirewallRule>, crate::NetworkError> {
        Ok(self
            .inner
            .lock()
            .expect("in-memory fw poisoned")
            .rules
            .iter()
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn add_rule(&self, rule: FirewallRule) -> Result<String, crate::NetworkError> {
        // 构造命令以触发规则体合法性校验（dry_run 的纯逻辑部分）
        let _args = nft_rule_body(&rule)?;
        let mut st = self.inner.lock().expect("in-memory fw poisoned");
        st.next_id += 1;
        let id = format!("rule-{}", st.next_id);
        st.rules.push((id.clone(), rule));
        Ok(id)
    }

    async fn delete_rule(&self, id: &str) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().expect("in-memory fw poisoned");
        let before = st.rules.len();
        st.rules.retain(|(rid, _)| rid != id);
        if st.rules.len() == before {
            return Err(crate::NetworkError::InterfaceNotFound(format!(
                "规则不存在: {id}"
            )));
        }
        Ok(())
    }

    async fn add_nat(&self, rule: crate::firewall::NatRule) -> Result<(), crate::NetworkError> {
        self.inner
            .lock()
            .expect("in-memory fw poisoned")
            .nats
            .push(rule);
        Ok(())
    }

    async fn delete_nat(&self, rule: &crate::firewall::NatRule) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().expect("in-memory fw poisoned");
        let before = st.nats.len();
        st.nats.retain(|n| n != rule);
        if st.nats.len() == before {
            return Err(crate::NetworkError::InterfaceNotFound(
                "NAT 规则不存在".into(),
            ));
        }
        Ok(())
    }
}

// ============================================================================
// 真实后端：rtnetlink（接口执行层，纯 Rust 无 FFI）
// ============================================================================

// 真实后端模块在 lib.rs 顶层声明（非 backend 子目录），此处仅 re-export 类型。
#[cfg(feature = "nftnl-ffi")]
pub use crate::nftnl_real::NftnlFirewallBackend;
pub use crate::rtnetlink_real::RtnetlinkBackend;

// ============================================================================
// NetlinkManager / NftFirewall（trait 实现，泛型委托后端）
// ============================================================================

/// 默认 `NetworkManager` 实现：基于 netlink（rtnetlink）。
///
/// 泛型参数 `B` 为执行后端，默认 `InMemoryNetlinkBackend`（无特权，测试/CI 安全）；
/// 生产注入 `RtnetlinkBackend`（真实 rtnetlink 通信，需 root/CAP_NET_ADMIN）。
///
/// 设计：本 struct 只负责"前置校验 + 委托执行 + 构造返回值"，不直接持有 socket。
/// 这使真实/内存两条路径走同一套逻辑（构造命令参数 + 校验），仅"执行"环节不同。
#[derive(Debug, Clone, Default)]
pub struct NetlinkManager<B: NetlinkBackend = InMemoryNetlinkBackend> {
    /// 注入的执行后端。
    pub backend: B,
}

impl NetlinkManager<InMemoryNetlinkBackend> {
    /// 构造一个使用内存后端的管理器（默认安全：不执行真实 netlink）。
    pub fn new() -> Self {
        Self::default()
    }
}

impl<B: NetlinkBackend> NetlinkManager<B> {
    /// 用指定后端构造（依赖注入入口）。
    pub fn with_backend(backend: B) -> Self {
        Self { backend }
    }
}

// —— NetworkManager trait 实现：委托后端 ——
// 注：trait 用原生 async fn in trait（非 dyn 兼容，见 ADR-COMPAT-001）。
#[allow(async_fn_in_trait)]
impl<B: NetlinkBackend> crate::interface::NetworkManager for NetlinkManager<B> {
    async fn list_interfaces(
        &self,
    ) -> Result<Vec<crate::interface::Interface>, crate::NetworkError> {
        self.backend.list_interfaces().await
    }

    async fn get_interface(
        &self,
        id: &InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        self.backend.get_interface(id).await
    }

    async fn create_vlan(
        &self,
        parent: &InterfaceId,
        vid: u16,
        name: InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        // 前置校验（VLAN ID 范围）由后端复用 ip_link_add_vlan_args 完成；
        // 此处显式再校一次，确保任何后端都先过这道门。
        crate::interface::validate_vlan_id(vid)?;
        self.backend.create_vlan(parent, vid, name).await
    }

    async fn create_bridge(
        &self,
        name: InterfaceId,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        crate::interface::Interface::validate_name(name.as_str())?;
        self.backend.create_bridge(name).await
    }

    async fn create_bond(
        &self,
        name: InterfaceId,
        mode: crate::interface::BondMode,
        slaves: Vec<InterfaceId>,
    ) -> Result<crate::interface::Interface, crate::NetworkError> {
        crate::interface::Interface::validate_name(name.as_str())?;
        self.backend.create_bond(name, mode, slaves).await
    }

    async fn set_address(
        &self,
        id: &InterfaceId,
        addrs: Vec<IpCidr>,
    ) -> Result<(), crate::NetworkError> {
        // CIDR 前缀校验在落 netlink 前完成
        for a in &addrs {
            a.validate()?;
        }
        self.backend.set_address(id, addrs).await
    }

    async fn up(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        self.backend.up(id).await
    }

    async fn down(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        self.backend.down(id).await
    }

    async fn delete_interface(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        self.backend.delete_interface(id).await
    }
}

/// 默认 `Firewall` 实现：基于 nftables（nftnl / `nft` CLI）。
///
/// 泛型参数 `B` 为执行后端，默认 `InMemoryFirewallBackend`（测试/CI 安全）；
/// 生产注入 `NftnlFirewallBackend`（feature `nftnl-ffi`，需 libnftnl-dev +
/// root/CAP_NET_ADMIN）。
#[derive(Debug, Clone, Default)]
pub struct NftFirewall<B: FirewallBackend = InMemoryFirewallBackend> {
    /// nft 表名（如 `"inet filter"`）。
    pub table: String,
    /// 默认 input 链名。
    pub input_chain: String,
    /// 注入的执行后端。
    pub backend: B,
}

impl NftFirewall<InMemoryFirewallBackend> {
    /// 构造默认（`inet filter` 表、`input` 链、内存后端）。
    pub fn new() -> Self {
        Self {
            table: "inet filter".into(),
            input_chain: "input".into(),
            backend: InMemoryFirewallBackend::default(),
        }
    }
}

impl<B: FirewallBackend> NftFirewall<B> {
    /// 用指定后端与表/链构造（依赖注入入口）。
    pub fn with_backend(table: impl Into<String>, chain: impl Into<String>, backend: B) -> Self {
        Self {
            table: table.into(),
            input_chain: chain.into(),
            backend,
        }
    }
}

// —— Firewall trait 实现：委托后端，dry-run 在本层完成 ——
#[allow(async_fn_in_trait)]
impl<B: FirewallBackend> crate::firewall::Firewall for NftFirewall<B> {
    async fn list_rules(&self) -> Result<Vec<FirewallRule>, crate::NetworkError> {
        self.backend.list_rules().await
    }

    async fn add_rule(&self, rule: FirewallRule) -> Result<String, crate::NetworkError> {
        // 契约 §3.9 安全约束：先 dry-run 校验合法性 + 是否会断管理网
        self.dry_run(&rule).await?;
        // dry-run 通过后委托后端提交
        self.backend.add_rule(rule).await
    }

    async fn delete_rule(&self, id: &str) -> Result<(), crate::NetworkError> {
        self.backend.delete_rule(id).await
    }

    async fn dry_run(&self, rule: &FirewallRule) -> Result<(), crate::NetworkError> {
        // 纯逻辑校验：规则体合法性（端口范围/Redirect target_port 等）。
        // "是否会断管理网"的连通性判断需要知道当前 SSH/管理源地址，
        // 接入拓扑后在此扩展（TODO(topology-aware-dry-run) [DOC]：当前为纯逻辑校验，
        // 拓扑感知需注入管理源地址；非运行时阻塞，属增强型待办）。
        rule.validate()?;
        // 复用命令构造层再做一次规则体构造校验（双保险）
        nft_rule_body(rule)?;
        Ok(())
    }

    async fn add_nat(&self, rule: crate::firewall::NatRule) -> Result<(), crate::NetworkError> {
        self.backend.add_nat(rule).await
    }

    async fn delete_nat(&self, rule: &crate::firewall::NatRule) -> Result<(), crate::NetworkError> {
        self.backend.delete_nat(rule).await
    }
}

/// 工具：根据接口类型判断是否可删除（物理接口拒绝）。
pub fn is_deletable(ty: InterfaceType) -> bool {
    !matches!(ty, InterfaceType::Physical | InterfaceType::Loopback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::{Firewall, FirewallAction, Protocol};
    use crate::interface::{BondMode, Interface, InterfaceId, InterfaceType, NetworkManager};
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn ip_link_up_args_correct() {
        let id = InterfaceId::new("eth0");
        assert_eq!(
            ip_link_up_args(&id),
            vec!["link", "set", "dev", "eth0", "up"]
        );
    }

    #[test]
    fn ip_link_down_args_correct() {
        let id = InterfaceId::new("eth0");
        assert_eq!(
            ip_link_down_args(&id),
            vec!["link", "set", "dev", "eth0", "down"]
        );
    }

    #[test]
    fn ip_link_add_vlan_valid() {
        let parent = InterfaceId::new("eth0");
        let name = InterfaceId::new("vlan100");
        let args = ip_link_add_vlan_args(&parent, 100, &name).unwrap();
        assert_eq!(
            args,
            vec!["link", "add", "link", "eth0", "name", "vlan100", "type", "vlan", "id", "100"]
        );
    }

    #[test]
    fn ip_link_add_vlan_rejects_zero() {
        let parent = InterfaceId::new("eth0");
        let name = InterfaceId::new("vlan0");
        let err = ip_link_add_vlan_args(&parent, 0, &name).unwrap_err();
        assert!(matches!(err, crate::NetworkError::RuleInvalid(_)));
    }

    #[test]
    fn ip_link_add_vlan_rejects_4095() {
        let parent = InterfaceId::new("eth0");
        let name = InterfaceId::new("vlan4095");
        assert!(ip_link_add_vlan_args(&parent, 4095, &name).is_err());
    }

    #[test]
    fn ip_link_add_bridge_correct() {
        let args = ip_link_add_bridge_args(&InterfaceId::new("br0"));
        assert_eq!(args, vec!["link", "add", "br0", "type", "bridge"]);
    }

    #[test]
    fn ip_link_add_bond_and_mode_correct() {
        let name = InterfaceId::new("bond0");
        let add = ip_link_add_bond_args(&name);
        assert_eq!(add, vec!["link", "add", "bond0", "type", "bond"]);
        let mode = ip_link_set_bond_mode_args(&name, BondMode::Lacp);
        assert_eq!(
            mode,
            vec!["link", "set", "dev", "bond0", "type", "bond", "mode", "802.3ad"]
        );
    }

    #[test]
    fn ip_link_enslave_correct() {
        let args = ip_link_enslave_args(&InterfaceId::new("eth1"), &InterfaceId::new("bond0"));
        assert_eq!(args, vec!["link", "set", "dev", "eth1", "master", "bond0"]);
    }

    #[test]
    fn ip_link_delete_correct() {
        let args = ip_link_delete_args(&InterfaceId::new("vlan100"));
        assert_eq!(args, vec!["link", "delete", "dev", "vlan100"]);
    }

    #[test]
    fn ip_addr_set_args_flush_then_add() {
        let id = InterfaceId::new("eth0");
        let cmds = ip_addr_set_args(
            &id,
            &[IpCidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 24)],
        )
        .unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], vec!["addr", "flush", "dev", "eth0"]);
        assert_eq!(
            cmds[1],
            vec!["addr", "add", "192.168.1.10/24", "dev", "eth0"]
        );
    }

    #[test]
    fn ip_addr_set_args_rejects_bad_prefix() {
        let id = InterfaceId::new("eth0");
        let err =
            ip_addr_set_args(&id, &[IpCidr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40)]).unwrap_err();
        assert!(matches!(err, crate::NetworkError::RuleInvalid(_)));
    }

    #[test]
    fn ip_link_show_json_args_correct() {
        assert_eq!(ip_link_show_json_args(), vec!["-j", "-o", "link", "show"]);
    }

    #[test]
    fn nft_protocol_keyword_mapping() {
        assert_eq!(nft_protocol_keyword(Protocol::Tcp), Some("tcp"));
        assert_eq!(nft_protocol_keyword(Protocol::Udp), Some("udp"));
        assert_eq!(nft_protocol_keyword(Protocol::Any), None);
    }

    #[test]
    fn nft_action_keyword_mapping() {
        assert_eq!(nft_action_keyword(FirewallAction::Allow), "accept");
        assert_eq!(nft_action_keyword(FirewallAction::Deny), "drop");
        assert_eq!(nft_action_keyword(FirewallAction::Redirect), "redirect");
    }

    #[test]
    fn nft_rule_body_tcp_allow() {
        use crate::firewall::{FirewallAction, FirewallRule};
        let rule = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        assert_eq!(nft_rule_body(&rule).unwrap(), "tcp dport 80 accept");
    }

    #[test]
    fn nft_rule_body_redirect_with_target() {
        use crate::firewall::{FirewallAction, FirewallRule};
        let rule = FirewallRule {
            action: FirewallAction::Redirect,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            dst_port: Some("443".into()),
            target_port: Some(8443),
            description: None,
        };
        assert_eq!(
            nft_rule_body(&rule).unwrap(),
            "tcp ip daddr 10.0.0.1 dport 443 redirect to :8443"
        );
    }

    #[test]
    fn nft_rule_body_rejects_redirect_without_target() {
        use crate::firewall::{FirewallAction, FirewallRule};
        let rule = FirewallRule {
            action: FirewallAction::Redirect,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        assert!(nft_rule_body(&rule).is_err());
    }

    #[test]
    fn nft_add_rule_args_correct() {
        use crate::firewall::{FirewallAction, FirewallRule};
        let rule = FirewallRule {
            action: FirewallAction::Deny,
            protocol: Protocol::Udp,
            src_addr: None,
            src_port: Some("1000-2000".into()),
            dst_addr: None,
            dst_port: None,
            target_port: None,
            description: None,
        };
        let args = nft_add_rule_args("inet filter", "input", &rule).unwrap();
        assert_eq!(
            args,
            vec![
                "add",
                "rule",
                "inet filter",
                "input",
                "udp sport 1000-2000 drop"
            ]
        );
    }

    #[test]
    fn is_deletable_logic() {
        use crate::interface::InterfaceType;
        assert!(!is_deletable(InterfaceType::Physical));
        assert!(!is_deletable(InterfaceType::Loopback));
        assert!(is_deletable(InterfaceType::Vlan));
        assert!(is_deletable(InterfaceType::Bridge));
        assert!(is_deletable(InterfaceType::Bond));
    }

    #[tokio::test]
    async fn netlink_manager_create_vlan_validates_first() {
        let m = NetlinkManager::new();
        // vid 越界 → 立即返回 RuleInvalid（不会进 unimplemented 分支的错误类型不同）
        let err = m
            .create_vlan(
                &InterfaceId::new("eth0"),
                5000,
                InterfaceId::new("vlan5000"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::NetworkError::RuleInvalid(_)));
    }

    #[tokio::test]
    async fn netlink_manager_up_unknown_returns_not_found() {
        // 内存后端：未知接口 up → InterfaceNotFound（不再返回 Internal 占位）
        let m = NetlinkManager::new();
        let err = m.up(&InterfaceId::new("eth0")).await.unwrap_err();
        assert!(matches!(err, crate::NetworkError::InterfaceNotFound(_)));
    }

    // —— 内存后端真实路径逻辑测试（验证 NetlinkManager 委托正确）——

    #[tokio::test]
    async fn in_memory_netlink_full_lifecycle() {
        let backend = InMemoryNetlinkBackend::with_loopback().with_interface(
            crate::interface::Interface::new(
                InterfaceId::new("eth0"),
                crate::interface::InterfaceType::Physical,
            )
            .with_state(crate::interface::IfState::Up),
        );
        let m = NetlinkManager::with_backend(backend);

        // list 含 lo + eth0
        let ifaces = m.list_interfaces().await.unwrap();
        assert!(ifaces.iter().any(|i| i.id.as_str() == "lo"));
        assert!(ifaces.iter().any(|i| i.id.as_str() == "eth0"));

        // get_interface
        let eth0 = m.get_interface(&InterfaceId::new("eth0")).await.unwrap();
        assert_eq!(eth0.ty, crate::interface::InterfaceType::Physical);

        // create_vlan
        let vlan = m
            .create_vlan(&InterfaceId::new("eth0"), 100, InterfaceId::new("vlan100"))
            .await
            .unwrap();
        assert_eq!(vlan.ty, crate::interface::InterfaceType::Vlan);
        assert_eq!(vlan.parent.as_ref().map(|p| p.as_str()), Some("eth0"));

        // set_address + up
        m.set_address(
            &InterfaceId::new("vlan100"),
            vec![IpCidr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 5)),
                24,
            )],
        )
        .await
        .unwrap();
        m.up(&InterfaceId::new("vlan100")).await.unwrap();
        let after = m.get_interface(&InterfaceId::new("vlan100")).await.unwrap();
        assert_eq!(after.state, crate::interface::IfState::Up);
        assert_eq!(after.addrs.len(), 1);

        // delete（虚拟接口可删）
        m.delete_interface(&InterfaceId::new("vlan100"))
            .await
            .unwrap();
        assert!(m.get_interface(&InterfaceId::new("vlan100")).await.is_err());
    }

    #[tokio::test]
    async fn in_memory_netlink_delete_physical_rejected() {
        let backend = InMemoryNetlinkBackend::with_loopback();
        let m = NetlinkManager::with_backend(backend);
        // 删 lo（回环）拒绝
        let err = m
            .delete_interface(&InterfaceId::new("lo"))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::NetworkError::RuleInvalid(_)));
    }

    #[tokio::test]
    async fn in_memory_netlink_create_bond_and_bridge() {
        let m = NetlinkManager::new();
        let _bond = m
            .create_bond(
                InterfaceId::new("bond0"),
                crate::interface::BondMode::Lacp,
                vec![InterfaceId::new("eth0")],
            )
            .await
            .unwrap();
        let _br = m.create_bridge(InterfaceId::new("br0")).await.unwrap();
        let ifaces = m.list_interfaces().await.unwrap();
        assert!(ifaces
            .iter()
            .any(|i| i.id.as_str() == "bond0" && i.ty == crate::interface::InterfaceType::Bond));
        assert!(ifaces
            .iter()
            .any(|i| i.id.as_str() == "br0" && i.ty == crate::interface::InterfaceType::Bridge));
    }

    #[tokio::test]
    async fn in_memory_firewall_add_and_delete_rule() {
        use crate::firewall::{Firewall, FirewallAction, FirewallRule};
        let fw = NftFirewall::new();
        let rule = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        let id = fw.add_rule(rule).await.unwrap();
        // list_rules 回读
        assert_eq!(fw.list_rules().await.unwrap().len(), 1);
        // delete
        fw.delete_rule(&id).await.unwrap();
        assert_eq!(fw.list_rules().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn in_memory_firewall_add_rule_blocked_by_dry_run() {
        use crate::firewall::{Firewall, FirewallAction, FirewallRule};
        let fw = NftFirewall::new();
        // Redirect 无 target_port → dry_run 失败 → add_rule 失败
        let bad = FirewallRule {
            action: FirewallAction::Redirect,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        assert!(fw.add_rule(bad).await.is_err());
        assert_eq!(fw.list_rules().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn in_memory_firewall_nat_crud() {
        use crate::firewall::{Firewall, NatRule};
        use std::net::Ipv4Addr;
        let fw = NftFirewall::new();
        let nat = NatRule {
            protocol: Protocol::Tcp,
            src: std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            translated_addr: std::net::IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
            translated_port: Some(12345),
        };
        fw.add_nat(nat.clone()).await.unwrap();
        // delete（NAT 规则须存在）
        fw.delete_nat(&nat).await.unwrap();
        // 再删一次 → 失败
        assert!(fw.delete_nat(&nat).await.is_err());
    }

    #[tokio::test]
    async fn in_memory_firewall_delete_unknown_rule() {
        use crate::firewall::Firewall;
        let fw = NftFirewall::new();
        let err = fw.delete_rule("nope").await.unwrap_err();
        assert!(matches!(err, crate::NetworkError::InterfaceNotFound(_)));
    }

    #[tokio::test]
    async fn nft_firewall_dry_run_validates() {
        use crate::firewall::{FirewallAction, FirewallRule};
        let fw = NftFirewall::new();
        let bad = FirewallRule {
            action: FirewallAction::Redirect,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None, // Redirect 须有 target_port
            description: None,
        };
        assert!(fw.dry_run(&bad).await.is_err());

        let good = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        assert!(fw.dry_run(&good).await.is_ok());
    }

    // —— 覆盖率补测：内存后端构造器 / down 成功路径 / NftFirewall::with_backend /
    // InMemoryFirewallBackend 访问器（rule_count / nat_snapshot）——

    #[test]
    fn in_memory_netlink_new_empty_and_recorded_ops() {
        // InMemoryNetlinkBackend::new()（默认空）+ recorded_ops() 初始空。
        let be = InMemoryNetlinkBackend::new();
        assert!(be.recorded_ops().is_empty());
        // alloc_index 经 with_loopback + 后续 create_* 间接覆盖，这里只触发访问器。
    }

    #[tokio::test]
    async fn in_memory_netlink_down_success_records_op() {
        // down 成功路径（既有测只覆盖了 up 与"未知接口 up 失败"）。
        let backend = InMemoryNetlinkBackend::with_loopback();
        let m = NetlinkManager::with_backend(backend);
        // lo 存在 → down 成功 → 状态变 Down + ops 记录
        m.down(&InterfaceId::new("lo")).await.unwrap();
        let lo = m.get_interface(&InterfaceId::new("lo")).await.unwrap();
        assert_eq!(lo.state, crate::interface::IfState::Down);
        assert!(m.backend.recorded_ops().iter().any(|o| o == "down:lo"));
    }

    #[tokio::test]
    async fn in_memory_netlink_down_unknown_not_found() {
        // down 未知接口 → InterfaceNotFound。
        let m = NetlinkManager::new();
        assert!(matches!(
            m.down(&InterfaceId::new("eth9")).await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
    }

    #[tokio::test]
    async fn in_memory_netlink_set_address_unknown_not_found() {
        // set_address 未知接口 → InterfaceNotFound（内存后端 find 失败分支）。
        let m = NetlinkManager::new();
        let err = m
            .set_address(
                &InterfaceId::new("eth9"),
                vec![IpCidr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 24)],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::NetworkError::InterfaceNotFound(_)));
    }

    #[tokio::test]
    async fn in_memory_netlink_delete_bridge_ok_and_recorded() {
        // delete 虚拟接口（Bridge）成功路径 + ops 记录。
        let m = NetlinkManager::new();
        m.create_bridge(InterfaceId::new("br0")).await.unwrap();
        m.delete_interface(&InterfaceId::new("br0")).await.unwrap();
        assert!(m.backend.recorded_ops().iter().any(|o| o == "delete:br0"));
    }

    #[tokio::test]
    async fn in_memory_netlink_get_unknown_not_found() {
        // get_interface 未知 → InterfaceNotFound。
        let m = NetlinkManager::new();
        assert!(matches!(
            m.get_interface(&InterfaceId::new("nope"))
                .await
                .unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
    }

    #[tokio::test]
    async fn in_memory_netlink_create_bond_empty_slaves() {
        // create_bond 空 slaves（不进 with_parent 分支）。
        let m = NetlinkManager::new();
        let bond = m
            .create_bond(InterfaceId::new("bond0"), BondMode::Broadcast, vec![])
            .await
            .unwrap();
        assert!(bond.parent.is_none());
        // recorded_ops 含 create_bond
        assert!(m
            .backend
            .recorded_ops()
            .iter()
            .any(|o| o.starts_with("create_bond:")));
    }

    #[tokio::test]
    async fn in_memory_netlink_create_bridge_records_op() {
        // create_bridge recorded_ops 分支。
        let m = NetlinkManager::new();
        m.create_bridge(InterfaceId::new("br0")).await.unwrap();
        assert!(m
            .backend
            .recorded_ops()
            .iter()
            .any(|o| o == "create_bridge:br0"));
    }

    #[tokio::test]
    async fn in_memory_netlink_set_address_records_op() {
        // set_address recorded_op 分支。
        let m = NetlinkManager::new();
        m.create_vlan(&InterfaceId::new("eth0"), 100, InterfaceId::new("vlan100"))
            .await
            .unwrap();
        m.set_address(
            &InterfaceId::new("vlan100"),
            vec![IpCidr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 24)],
        )
        .await
        .unwrap();
        assert!(m
            .backend
            .recorded_ops()
            .iter()
            .any(|o| o == "set_address:vlan100"));
    }

    #[tokio::test]
    async fn in_memory_firewall_backend_direct_accessors() {
        // 直接经 InMemoryFirewallBackend 测 new() / rule_count() / nat_snapshot()
        // + delete_rule 失败 + delete_nat 失败路径（不经 NftFirewall trait 层）。
        let be = InMemoryFirewallBackend::new();
        assert_eq!(be.rule_count(), 0);
        assert!(be.nat_snapshot().is_empty());

        // add_rule → rule_count
        let rule = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        let id = be.add_rule(rule).await.unwrap();
        assert_eq!(be.rule_count(), 1);
        // delete_rule 不存在
        assert!(matches!(
            be.delete_rule("nope").await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
        // delete_rule 存在
        be.delete_rule(&id).await.unwrap();
        assert_eq!(be.rule_count(), 0);

        // NAT add / delete
        let nat = crate::firewall::NatRule {
            protocol: Protocol::Udp,
            src: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            translated_addr: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
            translated_port: Some(53),
        };
        be.add_nat(nat.clone()).await.unwrap();
        assert_eq!(be.nat_snapshot().len(), 1);
        // delete_nat 不存在
        assert!(matches!(
            be.delete_nat(&crate::firewall::NatRule {
                protocol: Protocol::Tcp,
                src: IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
                translated_addr: IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)),
                translated_port: None,
            })
            .await
            .unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
        be.delete_nat(&nat).await.unwrap();
        assert!(be.nat_snapshot().is_empty());
    }

    #[tokio::test]
    async fn nft_firewall_with_backend_custom_table_chain() {
        // NftFirewall::with_backend 注入自定义表/链（覆盖 with_backend 构造器）。
        let fw = NftFirewall::with_backend("ip nat", "prerouting", InMemoryFirewallBackend::new());
        assert_eq!(fw.table, "ip nat");
        assert_eq!(fw.input_chain, "prerouting");
        // 注入的后端可用（add + list 闭环）
        let rule = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("22".into()),
            target_port: None,
            description: None,
        };
        fw.add_rule(rule).await.unwrap();
        assert_eq!(fw.list_rules().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn netlink_manager_with_backend_custom() {
        // NetlinkManager::with_backend（默认泛型参数路径）+ 默认无预置接口。
        let m: NetlinkManager<InMemoryNetlinkBackend> = NetlinkManager::with_backend(
            InMemoryNetlinkBackend::with_loopback().with_interface(
                Interface::new(InterfaceId::new("eth0"), InterfaceType::Physical)
                    .with_state(crate::interface::IfState::Up),
            ),
        );
        let ifaces = m.list_interfaces().await.unwrap();
        assert!(ifaces.iter().any(|i| i.id.as_str() == "lo"));
        assert!(ifaces.iter().any(|i| i.id.as_str() == "eth0"));
    }
}
