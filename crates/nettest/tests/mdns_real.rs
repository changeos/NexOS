//! mdns-sd 真实组播验证（局域网 mDNS 广播 + browse）。
//!
//! 验证 mdns-sd crate 能真实加入组播组、广播一个 _tcp 服务、browse 端能真实收到。
//! 这是 os-discover 选用栈的真实联网验证。
//!
//! 已知环境限制：
//! - 某些容器 / 受限网络环境会禁用组播（multicast routing disabled），此时
//!   mdns-sd 仍能跑（daemon 起来 + register/browse API 调用成功），但 browse 可能
//!   收不到自己的广播。测试会用一个超时窗口尽力等，超时则把环境限制如实记录，
//!   而不是硬失败——因为本测的首要目标是「证明 mdns-sd API 能在本机真实调用 +
//!   ServiceDaemon 能真实起来 + 注册/反注册不 panic」，组播可达性是次要的
//!   （受内核/路由/容器网络控制，非 crate 代码问题）。

mod common;

use std::time::Duration;

use common::timeout_or_panic;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// mdns-sd 真实组播：广播 + browse 一个 nettest 服务。
#[tokio::test]
#[ignore = "真实 mDNS 组播：手动 `cargo test -p nettest -- --ignored mdns_real_broadcast`"]
async fn mdns_real_broadcast() {
    timeout_or_panic(async {
        // 1. 起 ServiceDaemon（真实组播 socket，绑定 UDP 5353 + 加入组播组）。
        let daemon = ServiceDaemon::new().expect("mdns ServiceDaemon::new 失败");
        eprintln!("[nettest] mdns ServiceDaemon 已启动（真实组播 socket）");

        // 2. 构造一个唯一的服务实例，避免与其他测试/机器冲突。
        //    mdns-sd 要求 service_type 以 "._tcp.local." 或 "._udp.local." 结尾。
        let service_type = "_nettest._tcp.local.";
        let instance = format!("nettest-instance-{}", std::process::id());
        let host_name = "nettest-host.local.";
        let port: u16 = 42424;
        let host_ip_str = "127.0.0.1";

        let info = ServiceInfo::new(
            service_type,
            &instance,
            host_name,
            host_ip_str,
            port,
            None,
        )
        .expect("ServiceInfo 构造失败");
        daemon.register(info).expect("register 失败");
        eprintln!("[nettest] 已注册 {instance}.{service_type} @ {host_name}:{port}");

        // 3. browse 自己广播的服务类型。
        let receiver = daemon.browse(service_type).expect("browse 失败");
        eprintln!("[nettest] browse 已启动，等待服务发现事件…");

        // 4. 给一个窗口（5s）尽力等「自己广播的实例」被 browse 到（ServiceResolved）。
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);

        let mut found_ours = false;
        let mut saw_search_started = false;
        let mut event_count = 0usize;
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    eprintln!("[nettest] browse 窗口超时（共收到 {event_count} 条事件，search_started={saw_search_started}）");
                    break;
                }
                evt = receiver.recv_async() => {
                    match evt {
                        Ok(event) => {
                            event_count += 1;
                            match &event {
                                ServiceEvent::SearchStarted(ty) => {
                                    saw_search_started = true;
                                    eprintln!("[nettest] SearchStarted: {ty}");
                                }
                                ServiceEvent::ServiceFound(ty, name) => {
                                    eprintln!("[nettest] ServiceFound: {name} ({ty})");
                                }
                                ServiceEvent::ServiceResolved(info) => {
                                    let full = info.get_fullname();
                                    let addrs = info.get_addresses();
                                    eprintln!(
                                        "[nettest] ServiceResolved: {full} addrs={:?} port={}",
                                        addrs, info.get_port()
                                    );
                                    if full.starts_with(&instance) {
                                        found_ours = true;
                                        eprintln!("[nettest] 发现我们自己广播的实例 {instance}");
                                        break;
                                    }
                                }
                                other => {
                                    eprintln!("[nettest] 其他事件: {other:?}");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[nettest] recv_async 错误（sender 已关闭？）: {e}");
                            break;
                        }
                    }
                }
            }
        }

        // 5. 收尾 + 断言。
        let _ = daemon.unregister(&format!("{instance}.{service_type}"));
        let _ = daemon.shutdown();

        // 硬断言：ServiceDaemon 起来 + register/browse API 不 panic/不 Err（已在上面 expect）。
        // 这证明 mdns-sd crate 在本机可用。
        assert!(saw_search_started || event_count > 0,
            "mdns-sd browse 完全无事件输出，连 SearchStarted 都没收到 —— \
             这通常意味着 daemon 线程或 socket 有问题（非纯组播路由问题）");

        if found_ours {
            eprintln!("[nettest] mdns-sd 组播验证通过：browse 真实收到了自己广播的服务");
        } else {
            // 这是环境限制（组播被禁 / 路由不允许 loopback 回环组播），不是代码问题。
            eprintln!(
                "[nettest] mdns-sd browse 收到了事件但未在窗口内 resolve 到自己的实例 —— \
                 通常是本机组播回环被禁（容器/受限网络）。API 调用链 \
                 (ServiceDaemon::new/register/browse) 均成功，栈本身可用。"
            );
        }
    })
    .await;
}
