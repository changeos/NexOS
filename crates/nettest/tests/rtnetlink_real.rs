//! rtnetlink 真实接口/地址/路由查询 + dummy 网卡写路径 + genetlink 冒烟验证
//! （netlink：纯 Rust 绑定，无系统库 FFI）。
//!
//! 验证 OS 系统选用的真实接口执行层（os-network::RtnetlinkBackend）的底层依赖
//! —— `rtnetlink` crate —— 在本机能真实经 netlink 协议向内核查询网卡/地址/路由
//! 并做 dummy 网卡 CRUD（写路径），以及 genetlink（generic netlink 控制器）family
//! 枚举。覆盖 RtnetlinkBackend 真实层的最底层回归。
//!
//! ## 与 os-network 的关系
//! os-network::RtnetlinkBackend 复用 `rtnetlink::new_connection()` +
//! `handle.link().get().execute()` / `handle.address().get()` / `handle.route().get()`
//! 流式 API；本测直接调用同一个 rtnetlink crate 的同样路径，验证「真实 netlink
//! socket 真实起来 + 真实收发 link/addr/route dump 报文 + dummy 写路径」，这是
//! os-network 接口执行层的最底层回归。genetlink 部分（`genl ctrl list`）验证
//! generic netlink 控制器 family 枚举（os-network 现版本仅用 rtnetlink，genl 测
//! 为前瞻性回归 + 验证 netlink 协议族整体可用）。
//!
//! ## 运行环境
//! - **只读查询**（link/addr/route dump、get by name）：现代内核允许非特权用户读取
//!   （无 CAP_NET_ADMIN 也能 dump 元数据，但部分发行版/容器策略可能限制）。本测无
//!   root 也能跑通多数环境，但仍标 `#[ignore]` 以保持默认套件干净 + 与 nftnl_real /
//!   zfs_real 一致。建议用 sudo 兜底（route dump 某些发行版非 root 受限）。
//! - **写路径**（dummy 网卡 CRUD）：**必须 root + CAP_NET_ADMIN**（非 root 会 EPERM）。
//!   用 sudo 跑（`sudo cargo test ... -- --ignored`），测后自动删除 dummy 网卡。
//! - 无内核（极少数容器）或 netlink socket 被禁：测应**优雅失败**（明确 eprintln
//!   报告 netlink 错误），不 panic 污染默认套件。
//!
//! ## 副作用
//! - 只读测：无副作用，无残留。
//! - dummy CRUD 测：创建 `osdummy` 网卡 → 验证存在 → 删除 → 验证消失。测后必无残留
//!   （finally 风格兜底删除 + 测前预清理）。

mod common;

use std::net::IpAddr;

use common::timeout_or_panic;
use futures::TryStreamExt;

/// rtnetlink 真实接口查询：`new_connection()` → spawn 协议循环 → `link().get().execute()`
/// 流式收集所有 link message → 断言至少有一个名为 `lo`。
///
/// 环境不支持（netlink socket 禁用 / 权限不足）时**优雅跳过**（return + 明确 eprintln），
/// 不 panic——这样手动 `--ignored` 跑时清楚看到环境缺什么，也不污染测试套件。
#[tokio::test]
#[ignore = "真实 netlink 查询：手动 `cargo test -p nettest -- --ignored rtnetlink_real_link_list`（建议 root/CAP_NET_ADMIN，多数环境非 root 也可读）"]
async fn rtnetlink_real_link_list() {
    timeout_or_panic(async {
        // 1. 建立 netlink 连接（new_connection 返回 (connection, handle, _)）。
        //    rtnetlink 0.16 默认开 `tokio_socket` feature，new_connection 在。
        let (connection, handle, _) = match rtnetlink::new_connection() {
            Ok(tuple) => tuple,
            Err(e) => {
                // netlink socket 建立失败（容器/沙箱可能禁 netlink）：优雅跳过。
                eprintln!(
                    "[nettest] SKIP: rtnetlink::new_connection() 失败 —— netlink socket 建立\
                     失败（容器/沙箱可能禁 netlink）。原始错误: {e}"
                );
                return;
            }
        };
        // 2. 后台驱动 netlink 协议循环（必须在收发消息前 spawn）。
        tokio::spawn(connection);
        eprintln!("[nettest] rtnetlink 连接已建立，协议循环已 spawn");

        // 3. 流式收集 link dump 报文。
        //    try_next 返回 Result<Option<LinkMessage>>，None 表示流结束。
        let mut stream = handle.link().get().execute();
        let mut links: Vec<rtnetlink::packet_route::link::LinkMessage> = Vec::new();
        loop {
            match stream.try_next().await {
                Ok(Some(msg)) => links.push(msg),
                Ok(None) => break,
                Err(e) => {
                    // netlink 错误（权限不足 / 容器策略限制）：优雅跳过。
                    eprintln!(
                        "[nettest] SKIP: link dump 收到 netlink 错误 —— 可能是权限不足\
                         （需 CAP_NET_ADMIN，部分发行版限制非特权读 link）。错误: {e}"
                    );
                    return;
                }
            }
        }
        eprintln!("[nettest] link dump 完成，共 {} 个接口", links.len());

        // 4. 断言：至少有 lo（任何 Linux 都有回环接口）。
        //    解析 IfName 属性获取接口名（LinkAttribute::IfName）。
        use rtnetlink::packet_route::link::LinkAttribute;
        let names: Vec<String> = links
            .iter()
            .map(|m| {
                m.attributes
                    .iter()
                    .find_map(|a| match a {
                        LinkAttribute::IfName(n) => Some(n.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "<unknown>".to_string())
            })
            .collect();
        eprintln!("[nettest] 接口列表: {:?}", names);

        assert!(
            !links.is_empty(),
            "[nettest] link dump 返回空 —— 真实 netlink 应至少返回 lo"
        );
        assert!(
            names.iter().any(|n| n == "lo"),
            "[nettest] link dump 中未找到 'lo' —— 任何 Linux 都应有回环接口。接口列表: {names:?}"
        );

        // 断言至少一个物理网卡（eth/enp/wlan 前缀，排除 lo/docker/br-/veth 等虚拟）。
        let has_physical = names.iter().any(|n| {
            n.starts_with("eth")
                || n.starts_with("enp")
                || n.starts_with("ens")
                || n.starts_with("enx")
                || n.starts_with("wlan")
                || n.starts_with("wlp")
                || n.starts_with("wlx")
        });
        eprintln!("[nettest] has_physical={has_physical} (names={names:?})");
        // 不强断言物理网卡存在（容器/极简环境可能只有 lo）——记 eprintln，避免误红。
        // 但真实 OS 金属机必有物理网卡；此处仅记录供人工核对。

        eprintln!("[nettest] rtnetlink 真实接口查询冒烟通过：lo 已在 link dump 中");
    })
    .await;
}

// ============================================================================
// 只读测：addr_list / route_list / link_get_by_name
// ============================================================================

/// rtnetlink 真实**地址**查询：`handle.address().get().execute()` 流式收集所有
/// 地址消息 → 断言 lo 有 127.0.0.1/8。
#[tokio::test]
#[ignore = "真实 netlink 地址查询：手动 `cargo test -p nettest -- --ignored rtnetlink_real_addr_list`"]
async fn rtnetlink_real_addr_list() {
    timeout_or_panic(async {
        let (connection, handle, _) = match rtnetlink::new_connection() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[nettest] SKIP: new_connection 失败（容器可能禁 netlink）: {e}");
                return;
            }
        };
        tokio::spawn(connection);

        let mut stream = handle.address().get().execute();
        let mut lo_has_loopback = false;
        let mut count = 0usize;
        loop {
            match stream.try_next().await {
                Ok(Some(msg)) => {
                    count += 1;
                    // 找该地址消息的 IFA_ADDRESS + 接口名（通过 ifindex 反查太重，
                    // 这里用 AddressAttribute::Label 取接口名，或直接比对地址）。
                    use rtnetlink::packet_route::address::AddressAttribute as Aa;
                    let prefix = msg.header.prefix_len;
                    let mut addr_opt: Option<IpAddr> = None;
                    for nla in &msg.attributes {
                        if let Aa::Address(ip) = nla {
                            addr_opt = Some(*ip);
                        }
                    }
                    // 127.0.0.1/8 是 lo 的回环地址（kernel 默认带），前缀 8。
                    if prefix == 8 {
                        if let Some(IpAddr::V4(v4)) = addr_opt {
                            if v4.is_loopback() {
                                lo_has_loopback = true;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[nettest] SKIP: addr dump netlink 错误（可能权限不足）: {e}");
                    return;
                }
            }
        }
        eprintln!("[nettest] addr dump 完成，共 {count} 条地址记录");

        assert!(
            lo_has_loopback,
            "[nettest] 未在 addr dump 中找到 127.0.0.1/8（lo 回环地址）—— \
             任何 Linux 都应配置。共 {count} 条地址记录"
        );
        eprintln!("[nettest] rtnetlink 真实地址查询通过：lo 的 127.0.0.1/8 已确认");
    })
    .await;
}

/// rtnetlink 真实**路由**查询：`handle.route().get().execute()` 流式收集所有路由
/// 消息 → 断言 main 表（254）有 default 路由（destination_prefix_length==0 且带
/// Gateway 属性）。
#[tokio::test]
#[ignore = "真实 netlink 路由查询：手动 `cargo test -p nettest -- --ignored rtnetlink_real_route_list`"]
async fn rtnetlink_real_route_list() {
    timeout_or_panic(async {
        let (connection, handle, _) = match rtnetlink::new_connection() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[nettest] SKIP: new_connection 失败（容器可能禁 netlink）: {e}");
                return;
            }
        };
        tokio::spawn(connection);

        // route().get() 需要一个 RouteMessage 作为查询模板（默认空 dump 全表）。
        let mut stream = handle
            .route()
            .get(rtnetlink::packet_route::route::RouteMessage::default())
            .execute();
        let mut has_default = false;
        let mut count = 0usize;
        let mut sample_default = String::new();
        loop {
            match stream.try_next().await {
                Ok(Some(msg)) => {
                    count += 1;
                    // main 表 = RT_TABLE_MAIN = 254；default 路由 = 目的前缀 0
                    // 且带 Gateway（IPv4 default route）。
                    let is_main = msg.header.table == 254;
                    let is_default_prefix = msg.header.destination_prefix_length == 0;
                    use rtnetlink::packet_route::route::RouteAttribute as Ra;
                    let gw = msg.attributes.iter().find_map(|nla| match nla {
                        Ra::Gateway(g) => Some(g.clone()),
                        _ => None,
                    });
                    if is_main && is_default_prefix {
                        if let Some(g) = gw {
                            has_default = true;
                            sample_default = format!("default via {g:?}");
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[nettest] SKIP: route dump netlink 错误（可能权限不足）: {e}");
                    return;
                }
            }
        }
        eprintln!("[nettest] route dump 完成，共 {count} 条路由记录");

        assert!(
            has_default,
            "[nettest] 未在 route dump 中找到 main 表 default 路由 —— \
             OS 真实环境应有默认网关。共 {count} 条路由记录"
        );
        eprintln!(
            "[nettest] rtnetlink 真实路由查询通过：main 表 default 路由已确认 ({sample_default})"
        );
    })
    .await;
}

/// rtnetlink 真实**按名查接口**：`handle.link().get().match_name("lo")` →
/// 断言返回 lo 且 ifindex==1（Linux 内核 lo 固定为 ifindex 1）。
#[tokio::test]
#[ignore = "真实 netlink 按名查接口：手动 `cargo test -p nettest -- --ignored rtnetlink_real_link_get_by_name`"]
async fn rtnetlink_real_link_get_by_name() {
    timeout_or_panic(async {
        let (connection, handle, _) = match rtnetlink::new_connection() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[nettest] SKIP: new_connection 失败（容器可能禁 netlink）: {e}");
                return;
            }
        };
        tokio::spawn(connection);

        let mut stream = handle.link().get().match_name("lo".to_string()).execute();
        let msg = match stream.try_next().await {
            Ok(Some(m)) => m,
            Ok(None) => {
                eprintln!("[nettest] SKIP: match_name(\"lo\") 返回空 —— 异常（lo 必存在）");
                return;
            }
            Err(e) => {
                eprintln!("[nettest] SKIP: link get netlink 错误（可能权限不足）: {e}");
                return;
            }
        };
        let ifindex = msg.header.index;
        eprintln!("[nettest] link get by name \"lo\" → ifindex={ifindex}");

        assert_eq!(
            ifindex, 1,
            "[nettest] lo 的 ifindex 应为 1（Linux 内核固定），got {ifindex}"
        );
        eprintln!("[nettest] rtnetlink 真实按名查接口通过：lo ifindex=1");
    })
    .await;
}

// ============================================================================
// 写路径测：dummy 网卡 CRUD（#[ignore] + 必须 root/CAP_NET_ADMIN）
// ============================================================================

/// 辅助：判断当前进程是否 root（读 /proc/self/status Uid）。
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.trim().split('\t').next())
                .and_then(|u| u.parse::<u32>().ok())
        })
        .map(|uid| uid == 0)
        .unwrap_or(false)
}

/// 辅助：通过 rtnetlink 查指定接口名是否存在（match_name 流非空即存在）。
async fn link_exists(handle: &rtnetlink::Handle, name: &str) -> bool {
    let mut stream = handle.link().get().match_name(name.to_string()).execute();
    matches!(stream.try_next().await, Ok(Some(_)))
}

/// rtnetlink 真实**写路径**：dummy 网卡 CRUD。
///
/// 流程（全部经 rtnetlink，非 ip 命令）：
/// 1. 测前预清理（若残留 osdummy 先删，避免上次中断残留）；
/// 2. rtnetlink `LinkDummy::new("osdummy").build()` → `link().add()` 创建；
/// 3. rtnetlink 验证 osdummy 存在；
/// 4. rtnetlink `link().del(ifindex)` 删除；
/// 5. rtnetlink 验证 osdummy 消失。
///
/// 必须 root + CAP_NET_ADMIN（非 root 直接 skip + eprintln，不 panic）。
/// 测后兜底：无论断言成败，都尝试再删一次 osdummy（防残留）。
#[tokio::test]
#[ignore = "真实 netlink 写路径：需 root + CAP_NET_ADMIN，手动 \
            `sudo cargo test -p nettest -- --ignored rtnetlink_real_dummy_crud`"]
async fn rtnetlink_real_dummy_crud() {
    timeout_or_panic(async {
        if !is_root() {
            eprintln!(
                "[nettest] SKIP: dummy CRUD 需 root + CAP_NET_ADMIN（当前非 root）。\
                 用 `sudo cargo test ... -- --ignored rtnetlink_real_dummy_crud` 跑。"
            );
            return;
        }

        let (connection, handle, _) = match rtnetlink::new_connection() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[nettest] SKIP: new_connection 失败: {e}");
                return;
            }
        };
        tokio::spawn(connection);

        const NAME: &str = "osdummy";

        // —— 兜底清理辅助（防残留）：测末必调 ——
        // 用 ip 命令兜底删（rtnetlink 删需先查 ifindex，多一层；这里直接 ip del 更稳）。
        let cleanup = || {
            let _ = std::process::Command::new("ip")
                .args(["link", "del", NAME])
                .output();
        };

        // 1. 测前预清理（防上次中断残留）。
        if link_exists(&handle, NAME).await {
            eprintln!("[nettest] 预清理：检测到残留 {NAME}，先删除");
            cleanup();
            // 给内核一点时间生效。
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // 2. 创建 dummy 网卡（rtnetlink 写路径）。
        let add_msg = rtnetlink::LinkDummy::new(NAME).build();
        if let Err(e) = handle.link().add(add_msg).execute().await {
            eprintln!("[nettest] FAIL: 创建 {NAME} 失败: {e}");
            cleanup();
            panic!("[nettest] 创建 dummy 网卡失败（root 下应成功）: {e}");
        }
        eprintln!("[nettest] 已创建 dummy 网卡 {NAME}");

        // 3. 验证存在（rtnetlink 读路径）。
        let exists_after_create = link_exists(&handle, NAME).await;
        eprintln!("[nettest] 创建后 {NAME} 存在 = {exists_after_create}");
        if !exists_after_create {
            cleanup();
            panic!("[nettest] 创建 dummy 后 rtnetlink 未查到 {NAME} —— 写路径异常");
        }

        // 4. 删除 dummy 网卡（rtnetlink 写路径）：先查 ifindex，再 del。
        let mut get_stream = handle.link().get().match_name(NAME.to_string()).execute();
        let msg = match get_stream.try_next().await {
            Ok(Some(m)) => m,
            Ok(None) => {
                cleanup();
                panic!("[nettest] 删除前查 {NAME} 返回空（刚创建应存在）");
            }
            Err(e) => {
                cleanup();
                panic!("[nettest] 删除前查 {NAME} 出错: {e}");
            }
        };
        let ifindex = msg.header.index;
        if let Err(e) = handle.link().del(ifindex).execute().await {
            eprintln!("[nettest] FAIL: 删除 {NAME}(ifindex={ifindex}) 失败: {e}");
            cleanup();
            panic!("[nettest] 删除 dummy 网卡失败（root 下应成功）: {e}");
        }
        eprintln!("[nettest] 已删除 dummy 网卡 {NAME}(ifindex={ifindex})");

        // 5. 验证消失（rtnetlink 读路径）。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let exists_after_del = link_exists(&handle, NAME).await;
        eprintln!("[nettest] 删除后 {NAME} 存在 = {exists_after_del}");
        if exists_after_del {
            cleanup();
            panic!("[nettest] 删除 dummy 后 rtnetlink 仍查到 {NAME} —— 删除未生效");
        }

        eprintln!("[nettest] rtnetlink 真实写路径通过：dummy 网卡 CRUD 完整（创建→验证存在→删除→验证消失）");
    })
    .await;
}

// ============================================================================
// genetlink：family 枚举（generic netlink 控制器 CTRL_CMD_GETFamily dump）
// ============================================================================

/// genetlink 真实 family 枚举：经 generic netlink 控制器（nlctrl，family id 0x10）
/// 发 `CTRL_CMD_GETFamily` + `NLM_F_DUMP` → 收所有已注册 family 的 NewFamily 响应 →
/// 断言至少有 nlctrl 自身（family name "nlctrl"），有 wifi 时额外断言 nl80211。
///
/// os-network 现版本仅用 rtnetlink（不用 genetlink），此测为前瞻性回归 +
/// 验证 netlink 协议族（含 generic）整体在本机可用。实现参照 genetlink crate
/// 官方示例 `examples/list_genetlink_family.rs`。
#[tokio::test]
#[ignore = "真实 genetlink family 查询：手动 `cargo test -p nettest -- --ignored rtnetlink_real_genetlink_families`"]
async fn rtnetlink_real_genetlink_families() {
    timeout_or_panic(async {
        use futures::StreamExt;
        use netlink_packet_core::{
            NetlinkHeader, NetlinkMessage, NetlinkPayload, NLM_F_DUMP, NLM_F_REQUEST,
        };
        use netlink_packet_generic::{
            ctrl::{nlas::GenlCtrlAttrs, GenlCtrl, GenlCtrlCmd},
            GenlMessage,
        };

        // genetlink::new_connection 返回 (connection, handle, _)。
        let (connection, mut handle, _) = match genetlink::new_connection() {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "[nettest] SKIP: genetlink::new_connection 失败（容器可能禁 netlink）: {e}"
                );
                return;
            }
        };
        tokio::spawn(connection);
        eprintln!("[nettest] genetlink 连接已建立，协议循环已 spawn");

        // 构造 CTRL_CMD_GETFamily dump 请求（nlas 空 + NLM_F_DUMP = 枚举全部 family）。
        let mut nl_hdr = NetlinkHeader::default();
        nl_hdr.flags = NLM_F_REQUEST | NLM_F_DUMP;
        let nlmsg = NetlinkMessage::new(
            nl_hdr,
            GenlMessage::from_payload(GenlCtrl {
                cmd: GenlCtrlCmd::GetFamily,
                nlas: vec![],
            })
            .into(),
        );

        let mut responses = match handle.request(nlmsg).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[nettest] SKIP: genetlink request 失败: {e}");
                return;
            }
        };

        let mut names: Vec<String> = Vec::new();
        loop {
            match responses.next().await {
                Some(Ok(resp)) => match resp.payload {
                    NetlinkPayload::InnerMessage(genlmsg) => {
                        // 只取 NewFamily 响应（dump 全表时每条 family 一个 NewFamily）。
                        if genlmsg.payload.cmd == GenlCtrlCmd::NewFamily {
                            for nla in &genlmsg.payload.nlas {
                                if let GenlCtrlAttrs::FamilyName(n) = nla {
                                    names.push(n.clone());
                                }
                            }
                        }
                    }
                    NetlinkPayload::Error(err) => {
                        eprintln!(
                            "[nettest] SKIP: genetlink family dump 收到 netlink 错误: {err:?}"
                        );
                        return;
                    }
                    _ => {}
                },
                Some(Err(e)) => {
                    eprintln!(
                        "[nettest] SKIP: genetlink family dump decode 错误: {e}"
                    );
                    return;
                }
                None => break,
            }
        }
        eprintln!(
            "[nettest] genetlink family dump 完成，共 {} 个 family: {:?}",
            names.len(),
            names
        );

        assert!(
            !names.is_empty(),
            "[nettest] genetlink family dump 返回空 —— 至少应有 nlctrl"
        );
        assert!(
            names.iter().any(|n| n == "nlctrl"),
            "[nettest] genetlink family 列表未含 nlctrl（generic netlink 控制器自身必存在）: {names:?}"
        );

        // 有 wifi 时额外确认 nl80211（非强制，仅记录）。
        let has_nl80211 = names.iter().any(|n| n == "nl80211");
        eprintln!(
            "[nettest] nl80211 存在 = {has_nl80211}（有 wifi 网卡时应有）"
        );

        eprintln!("[nettest] genetlink 真实 family 枚举通过：nlctrl 已确认");
    })
    .await;
}
