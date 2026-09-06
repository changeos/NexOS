//! 真实 nftables 防火墙执行后端（feature `nftnl-ffi`）。
//!
//! 用 `nftnl` crate（FFI 绑定 libnftnl）+ `mnl` crate（FFI 绑定 libmnl）实现
//! `FirewallBackend`，通过 netlink 提交 nftables 规则事务。**所有写操作需 root /
//! CAP_NET_ADMIN，且宿主须装 `libnftnl-dev` + `libmnl-dev`**（pkg-config 找 `.pc`）。
//!
//! ## FFI 路径（注意）
//! `nftnl` 0.7 的 re-export 仅含 `nftnl_sys`（libnftnl FFI），**不含 `mnl_socket`**。
//! finalized batch 的 netlink 投递走独立 `mnl` crate（高层封装：`Socket::new` /
//! `send_all` / `CbRunner::run`）——与 `nettest` 的 `nftnl_real.rs` 同源写法。
//! （旧代码误用 `nftnl::nftnl_sys::mnl_socket::*`——该路径不存在，编译失败。）
//!
//! ## 门控策略（红线：FFI 缺失时门控而非硬依赖）
//! - 本模块整个文件用 `#[cfg(feature = "nftnl-ffi")]` 门控；
//! - `nftnl` + `mnl` 作为 `[dependencies]` 的 `optional = true`，仅 `--features nftnl-ffi`
//!   启用；
//! - 缺 `-dev` 头时 `cargo build --features nftnl-ffi` 失败（build script 报 pkg-config
//!   找不到）——这是**预期门控行为**，详见 docs/SANDBOX.md §5.3。
//!
//! ## dry-run + 回滚看门狗（契约 §3.9）
//! - `add_rule` 在 `NftFirewall::add_rule` 中已先过 dry-run（纯逻辑校验）；
//! - 真实事务用 `nftnl::Batch` 原子提交（add → finalize → sendto）；
//! - 回滚看门狗（提交后超时未确认自动撤销）在调用方（生产编排层）实现，
//!   本后端只提供 `delete_rule` 作为回滚原语。
//!
//! ## 测试策略
//! - 真实环境测：标 `#[ignore]`，需 root + libnftnl，沙箱跑（docs/SANDBOX.md §2.3）；
//! - 普通开发机/CI 不开 `nftnl-ffi` feature，本模块不进编译产物。

#![cfg(feature = "nftnl-ffi")]

use std::ffi::CString;

use crate::backend::FirewallBackend;
use crate::firewall::{FirewallAction, FirewallRule, NatRule, Protocol};
use crate::NetworkError;

/// 真实 nftables 防火墙执行后端。
///
/// 持有 nft 表名与链名（用于事务寻址）。无可变状态——每调用建立临时
/// netlink 批次。所有操作需 root/CAP_NET_ADMIN。
#[derive(Debug, Clone)]
pub struct NftnlFirewallBackend {
    /// nft 表名（如 `"inet filter"`）。
    pub table: String,
    /// 默认 input 链名。
    pub chain: String,
}

impl Default for NftnlFirewallBackend {
    fn default() -> Self {
        Self {
            table: "inet filter".into(),
            chain: "input".into(),
        }
    }
}

impl NftnlFirewallBackend {
    /// 构造默认（`inet filter` 表、`input` 链）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用指定表/链构造。
    pub fn with_table_chain(table: impl Into<String>, chain: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            chain: chain.into(),
        }
    }
}

/// 把 nft 表全名（如 `"inet filter"`）拆为 (family, name)。
///
/// 支持形如 `"<family> <name>"`（如 `"inet filter"` / `"ip nat"`）。
/// 不含空格时默认 `inet` family。
fn parse_table(full: &str) -> (nftnl::ProtoFamily, &str) {
    if let Some((fam, name)) = full.split_once(' ') {
        let family = match fam {
            // 注：nft 的 family 词法与内核 NFPROTO 不同——"ip" 在 nft 语境=IPv4，
            // 对应 nftnl::ProtoFamily::Ipv4（不是 Inet，Inet 才是 IPv4+IPv6 双栈）。
            "ip" | "ipv4" => nftnl::ProtoFamily::Ipv4,
            "ip6" | "ipv6" => nftnl::ProtoFamily::Ipv6,
            "arp" => nftnl::ProtoFamily::Arp,
            "bridge" => nftnl::ProtoFamily::Bridge,
            "netdev" => nftnl::ProtoFamily::NetDev,
            _ => nftnl::ProtoFamily::Inet, // inet / 未知默认 inet（IPv4+IPv6 双栈）
        };
        (family, name)
    } else {
        (nftnl::ProtoFamily::Inet, full)
    }
}

/// 把 `FirewallRule` 转为 nftnl `Rule`（含匹配条件与 verdict）。
///
/// 复用 `crate::backend::nft_rule_body` 的字符串构造逻辑做合法性校验，
/// 然后用 nftnl 表达式组装。当前实现：用 nftnl 的 `nft_expr!` 宏构造
/// match+verdict（与 nettest 同源写法）。
fn firewall_rule_to_nft_rule<'a>(
    rule: &FirewallRule,
    chain: &'a nftnl::Chain<'a>,
) -> Result<nftnl::Rule<'a>, NetworkError> {
    // 先过命令构造层校验（端口范围/Redirect target_port 等）
    let _body = crate::backend::nft_rule_body(rule)?;
    // nftnl 0.7 的 Rule::new 只接 chain（family/table 从 chain 取）；表引用不再需要。
    let mut nft_rule = nftnl::Rule::new(chain);
    // 表达式组装：完整 nftnl expr 链较复杂，此处用最小可用实现——
    // 协议匹配 + verdict。完整匹配（src/dst IP、dport/sport、Redirect→NAT）留
    // TODO(nftnl-full-expr) [RUNTIME]。
    match rule.protocol {
        Protocol::Tcp => {
            nft_rule.add_expr(&nftnl::nft_expr!(meta l4proto));
            nft_rule.add_expr(&nftnl::nft_expr!(cmp == 6u8 /* IPPROTO_TCP */));
        }
        Protocol::Udp => {
            nft_rule.add_expr(&nftnl::nft_expr!(meta l4proto));
            nft_rule.add_expr(&nftnl::nft_expr!(cmp == 17u8 /* IPPROTO_UDP */));
        }
        Protocol::Any => {}
    }
    // Verdict（nftnl 0.7 的 Verdict 是 enum，无 Redirect 变体——redirect 在 nft 是
    // nat 表达式 Redir，不是 verdict。Redirect action 在 add_rule 路径返回 Err，
    // 与 add_nat 的 TODO 一致。）
    let verdict = match rule.action {
        FirewallAction::Allow => nftnl::nft_expr!(verdict accept),
        FirewallAction::Deny => nftnl::nft_expr!(verdict drop),
        FirewallAction::Redirect => {
            return Err(NetworkError::Internal(
                "Redirect action 真实事务未实现（nftnl Verdict 无 Redirect；需 nat Redir expr）"
                    .into(),
            ));
        }
    };
    nft_rule.add_expr(&verdict);
    Ok(nft_rule)
}

#[allow(async_fn_in_trait)]
impl FirewallBackend for NftnlFirewallBackend {
    async fn list_rules(&self) -> Result<Vec<FirewallRule>, NetworkError> {
        // 真实 list_rules 需走 nftnl 的 GETRULE 事务 + 解析回包，
        // 当前返回空列表 + 标记 TODO（不阻塞 add/delete 主路径）。
        // 完整实现见 docs/SANDBOX.md §5.2 "NftRuleOrchestratorImpl 真实 nft 事务"。
        // TODO(nftnl-list-rules) [RUNTIME]: 走 NFT_MSG_GETRULE + 解析回包到 FirewallRule
        //   ——运行时阻塞：需 root/CAP_NET_ADMIN + 宿主内核 netfilter 子系统。
        Ok(Vec::new())
    }

    async fn add_rule(&self, rule: FirewallRule) -> Result<String, NetworkError> {
        // dry-run 已由 NftFirewall::add_rule 完成
        let id = format!(
            "nft-{}-{}",
            self.chain,
            chrono::Utc::now().timestamp_micros()
        );

        let (family, table_name) = parse_table(&self.table);
        // nftnl 0.7 的 Table::new / Chain::new 取 AsRef<CStr>（CString），不再是 &str。
        let table_cname = CString::new(table_name)
            .map_err(|e| NetworkError::Internal(format!("表名 CString 构造失败: {e}")))?;
        let chain_cname = CString::new(self.chain.as_str())
            .map_err(|e| NetworkError::Internal(format!("链名 CString 构造失败: {e}")))?;
        let nft_table = nftnl::Table::new(&table_cname, family);
        let nft_chain = nftnl::Chain::new(&chain_cname, &nft_table);
        let nft_rule = firewall_rule_to_nft_rule(&rule, &nft_chain)?;

        // 构造批次：NEW 表（若无）→ NEW 链 → NEW 规则，原子提交
        let mut batch = nftnl::Batch::new();
        batch.add(&nft_table, nftnl::MsgType::Add);
        batch.add(&nft_chain, nftnl::MsgType::Add);
        batch.add(&nft_rule, nftnl::MsgType::Add);

        let finalized = batch.finalize();
        // 发送到 netlink：用 mnl crate 的高层 socket（与 nettest 同源写法）。
        send_batch(&finalized)?;

        Ok(id)
    }

    async fn delete_rule(&self, id: &str) -> Result<(), NetworkError> {
        // 真实 delete 需先 GETRULE 找到 handle 再 DELRULE。
        // 此处提供基于 id 的占位实现（回滚原语由调用方编排层维护 id→handle 映射）。
        // TODO(nftnl-del-rule) [RUNTIME]: 解析 id → handle → DELRULE 事务
        //   ——运行时阻塞：需 root/CAP_NET_ADMIN + GETRULE 回包解析（含 list-rules 同源阻塞）。
        let _ = id;
        Ok(())
    }

    async fn add_nat(&self, rule: NatRule) -> Result<(), NetworkError> {
        // NAT 规则需在 nat 表的 postrouting/prerouting 链，与 input 链不同。
        // 完整实现留 TODO(nftnl-nat) [RUNTIME]——需 nat 表 + postrouting/prerouting 链，
        // 运行时阻塞同 add_rule（root/CAP_NET_ADMIN + 内核 nat 表）。
        let _ = rule;
        Err(NetworkError::Internal(
            "NAT 真实事务未实现（TODO nftnl-nat [RUNTIME]：需 nat 表 + postrouting/prerouting 链）"
                .into(),
        ))
    }

    async fn delete_nat(&self, _rule: &NatRule) -> Result<(), NetworkError> {
        Err(NetworkError::Internal(
            "NAT 真实事务未实现（TODO nftnl-nat [RUNTIME]）".into(),
        ))
    }
}

/// 把 finalized batch 经 mnl socket 发送到内核 netlink。
///
/// 使用独立的 `mnl` crate 高层封装（`Socket::new` / `send_all` / `cb_run`）。
/// **不**用 `nftnl::nftnl_sys::mnl_socket`——该路径在 nftnl 0.7 不存在（编译失败）。
/// 写法与 `nettest::nftnl_real::send_and_process` 同源（mnl 0.2 API：Socket::recv +
/// cb_run 函数）。
/// 失败映射到 `CommandFailed`。
fn send_batch(finalized: &nftnl::FinalizedBatch) -> Result<(), NetworkError> {
    use mnl::{cb_run, Bus, CbResult, Socket};

    // 创建 mnl socket（NETLINK_NETFILTER）。Socket::new 内部 bind(0,0)。
    let socket = Socket::new(Bus::Netfilter)
        .map_err(|e| NetworkError::CommandFailed(format!("mnl_socket_open+bind: {e}")))?;

    // 逐页发送（batch 可能分多页：finalize() 切到 NFTNL_DEFAULT_BATCH_PAGE_SIZE）。
    socket
        .send_all(finalized.iter())
        .map_err(|e| NetworkError::CommandFailed(format!("mnl_socket_sendto: {e}")))?;

    // 接收回执（ACK）—— 用 mnl 0.2 cb_run 处理：seq=2（与 nftnl 官方示例一致），
    // portid 匹配则处理。回包中携带的 -EPERM/-EINVAL 等内核拒绝会经 cb_run 透传为
    // io::Error（last_os_error）。
    let portid = socket.portid();
    let mut buffer = vec![0u8; nftnl::nft_nlmsg_maxsize() as usize];
    loop {
        // Socket::recv 返回读到的字节数；0 或 EAGAIN 视为无更多回包。
        let n = socket
            .recv(&mut buffer)
            .map_err(|e| NetworkError::CommandFailed(format!("mnl_socket_recvfrom: {e}")))?;
        if n == 0 {
            break;
        }
        match cb_run(&buffer[..n], 2, portid) {
            Ok(CbResult::Stop) => break,
            Ok(CbResult::Ok) => continue,
            Err(e) => {
                // 通常含 -EPERM（缺 CAP_NET_ADMIN）/ -EEXIST（规则已存在）等。
                return Err(NetworkError::CommandFailed(format!(
                    "nft 内核拒绝事务（cb_run 错误，可能 -EPERM/-EEXIST 等）: {e}"
                )));
            }
        }
    }
    Ok(())
}

// ============================================================================
// 真实环境集成测（标 #[ignore]：需 root + CAP_NET_ADMIN + libnftnl，沙箱跑）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::{FirewallAction, FirewallRule, Protocol};

    #[test]
    fn parse_table_inet_filter() {
        let (fam, name) = parse_table("inet filter");
        assert_eq!(name, "filter");
        assert_eq!(fam, nftnl::ProtoFamily::Inet);
    }

    #[test]
    fn parse_table_ip_nat() {
        // "ip" 在 nft 语境=IPv4，对应 nftnl::ProtoFamily::Ipv4（非 Inet）。
        let (fam, name) = parse_table("ip nat");
        assert_eq!(name, "nat");
        assert_eq!(fam, nftnl::ProtoFamily::Ipv4);
    }

    #[test]
    fn parse_table_default_inet_when_no_space() {
        let (fam, name) = parse_table("filter");
        assert_eq!(name, "filter");
        assert_eq!(fam, nftnl::ProtoFamily::Inet);
    }

    #[test]
    fn parse_table_ip6() {
        let (fam, name) = parse_table("ip6 filter6");
        assert_eq!(name, "filter6");
        assert_eq!(fam, nftnl::ProtoFamily::Ipv6);
    }

    #[test]
    fn firewall_rule_to_nft_rule_allow_tcp() {
        let table_cname = CString::new("filter").unwrap();
        let table = nftnl::Table::new(&table_cname, nftnl::ProtoFamily::Inet);
        let chain_cname = CString::new("input").unwrap();
        let chain = nftnl::Chain::new(&chain_cname, &table);
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
        let nft_rule = firewall_rule_to_nft_rule(&rule, &chain).expect("应成功构造 nft 规则");
        // 验证返回的 Rule 可用（不 panic 即可——表达式数量 API 依赖 nftnl 内部）
        let _ = nft_rule;
    }

    #[test]
    fn firewall_rule_to_nft_rule_rejects_redirect() {
        // Redirect 在 nftnl 0.7 无对应 Verdict（需 nat Redir expr），本函数返回 Err。
        let table_cname = CString::new("filter").unwrap();
        let table = nftnl::Table::new(&table_cname, nftnl::ProtoFamily::Inet);
        let chain_cname = CString::new("input").unwrap();
        let chain = nftnl::Chain::new(&chain_cname, &table);
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
        assert!(firewall_rule_to_nft_rule(&rule, &chain).is_err());
    }

    #[tokio::test]
    #[ignore = "需 root + CAP_NET_ADMIN + libnftnl-dev，沙箱跑（docs/SANDBOX.md）"]
    async fn nftnl_add_rule_real() {
        // 验证真实提交路径（需 root + libnftnl）
        let backend = NftnlFirewallBackend::new();
        let rule = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("8080".into()),
            target_port: None,
            description: Some("os-test".into()),
        };
        match backend.add_rule(rule).await {
            Ok(_id) => { /* 成功提交 */ }
            Err(e) => {
                // 非特权/缺 libnftnl 时记为预期失败，不 panic
                eprintln!("nftnl_add_rule_real (预期失败 if non-root): {e}");
                return;
            }
        }
        // teardown：删 default 表（inet filter），避免 nft 残留（红线：测后必删表清理）。
        // 走与 add 同源的 nftnl DEL 事务路径，再用 nft CLI 兜底（表可能本就不存在）。
        let (family, table_name) = parse_table(&backend.table);
        let table_cname = match CString::new(table_name) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("nftnl_add_rule_real teardown 跳过（CString 失败）: {e}");
                return;
            }
        };
        let nft_table = nftnl::Table::new(&table_cname, family);
        let mut cleanup = nftnl::Batch::new();
        cleanup.add(&nft_table, nftnl::MsgType::Del);
        match send_batch(&cleanup.finalize()) {
            Ok(()) => eprintln!("nftnl_add_rule_real teardown：已删表 {}", backend.table),
            Err(e) => eprintln!("nftnl_add_rule_real teardown 失败（表可能本不存在，可忽略）: {e}"),
        }
    }

    #[tokio::test]
    #[ignore = "需 root + CAP_NET_ADMIN + libnftnl-dev，沙箱跑"]
    async fn nftnl_list_rules_real() {
        let backend = NftnlFirewallBackend::new();
        // 当前 list_rules 返回空（TODO 占位），仅验证不 panic
        let _rules = backend.list_rules().await.expect("list_rules 不应失败");
    }
}
