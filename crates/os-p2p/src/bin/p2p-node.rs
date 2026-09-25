//! p2p-node —— os-p2p 独立节点载体（P2a）。
//!
//! **这是部署到非 NexOS 公网机（cloud）跑锚点/交换所/中继角色的可执行体**：
//! cloud 上 `NEXOS_P2P_PUBLIC=1 ./p2p-node` 即成全网 bootstrap 锚点 + 观测端点
//! 交换所 + 打洞失败时的中继志愿者；普通机器不带 PUBLIC 跑则是一个 NAT 后
//! 组网节点。全部 env 配置（复用 NEXOS_P2P_* + NEXOS_P2P_NAME 昵称）。
//!
//! **身份持久化（P2b）**：`config_from_env` 默认从 `NEXOS_P2P_KEY_FILE`
//! （缺省降级链 `/tank/os-data/p2p-node-key` → `/var/lib/os/p2p-node-key` →
//! `./p2p-node-key`）加载/生成 secp256k1 私钥——**锚点/节点重启 NodeID 稳定**
//! （原子写 0600；损坏文件降级重生成并告警）。
//!
//! # 启动输出（stdout）
//!
//! ```text
//! [p2p-node] name        = anchor-1
//! [p2p-node] NodeID      = 0x02…（66 hex）
//! [p2p-node] OverlayAddr = 0x7e5f…bdf（EVM 同源 20 字节）
//! [p2p-node] listen      = 0.0.0.0:7070
//! [p2p-node] 命令: status | peers | send <node_id> <text> | quit
//! ```
//!
//! # stdin 命令（交互模式）
//!
//! - `status`：路由表（k-buckets 摘要）/ 端点簿（地址交换所）/ 连接阶梯统计
//! - `peers`：已知节点清单（连接状态 / underlay / 公网角色 / 中继路由）
//! - `send <node_id> <text>`：向某节点发应用消息
//! - `quit` / `exit`：优雅停机
//!
//! 消息接收事件实时打印 `[recv] from=… hops=… payload=…`（上层服务未接入时的
//! 人工观测面）。
//!
//! # 服务化运行：stdin EOF → 纯服务模式
//!
//! systemd 等服务化部署无 TTY，stdin 一打开即 EOF：此时**不退出**（会被
//! systemd 拉起-退出循环），而是转入**纯服务模式**——不再 select stdin，
//! 只 await 消息通道继续打印 `[recv]`，交互命令停用；`on_msg` 广播通道
//! 关闭（节点停机）时才优雅退出。**绝不能**把已关闭的 stdin 通道继续留在
//! select 里：对已关闭 channel `recv()` 立即返回 `None` → 空命令跳过 →
//! select 立即再入，busy-loop 100% CPU 空转。

use os_p2p::{config_from_env, NodeId, P2pMsg, P2pNode, ENV_NAME};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    let cfg = config_from_env();
    let name = std::env::var(ENV_NAME).unwrap_or_default();
    let handle = match P2pNode::spawn(cfg) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[p2p-node] 启动失败: {e}");
            std::process::exit(1);
        }
    };
    println!("[p2p-node] name        = {name}");
    println!("[p2p-node] NodeID      = {}", handle.self_id());
    println!("[p2p-node] OverlayAddr = {}", handle.self_id().overlay());
    println!("[p2p-node] listen      = {}", handle.listen_addr());
    println!("[p2p-node] 命令: status | peers | send <node_id> <text> | quit");

    let mut rx = handle.on_msg();
    // stdin 阻塞读取隔离到专用线程，经 tokio 通道汇入 select（线程退出即 EOF，
    // 服务化部署下这会立即发生——随后转入纯服务模式，见下方 lines_open）
    let (lines_tx, mut lines_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if lines_tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // stdin 是否仍存活：服务化部署（systemd，无 TTY）stdin 立即 EOF，EOF 后
    // 置 false 转入**纯服务模式**——只服务消息通道。若继续把已关闭的
    // lines_rx 留在 select 里，其 recv() 每次立即返回 None → 空命令 →
    // select 立即再入，busy-loop 100% CPU 空转（锚点节点空转两天的根因）。
    let mut lines_open = true;

    loop {
        // 纯服务模式：stdin 已 EOF，不再 select stdin，只 await 消息通道；
        // on_msg 广播通道关闭（节点停机，发送端全部 drop）时优雅退出。
        if !lines_open {
            match rx.recv().await {
                Ok(m) => print_recv(&m),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[p2p-node] 消息积压，跳过 {n} 条");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    println!("[p2p-node] 节点已停机（消息通道关闭）");
                    break;
                }
            }
            continue;
        }
        let cmd = tokio::select! {
            // stdin EOF（读线程退出，服务化部署常态）：转纯服务模式，见
            // lines_open 注释——不退出（systemd 会拉起-退出循环），也不空转。
            line = lines_rx.recv() => match line {
                Some(l) => l,
                None => {
                    lines_open = false;
                    println!("[p2p-node] stdin EOF → 纯服务模式（继续收消息打印 [recv]，kill/SIGTERM 退出）");
                    continue;
                }
            },
            msg = rx.recv() => {
                match msg {
                    Ok(m) => print_recv(&m),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[p2p-node] 消息积压，跳过 {n} 条");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // 广播通道关闭 = 节点停机：交互模式同样优雅退出
                        println!("[p2p-node] 节点已停机（消息通道关闭）");
                        break;
                    }
                }
                continue;
            }
        };
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            continue;
        }
        let (op, rest) = match cmd.split_once(' ') {
            Some((op, rest)) => (op, rest.trim()),
            None => (cmd.as_str(), ""),
        };
        match op {
            "status" => print_status(&handle).await,
            "peers" => print_peers(&handle).await,
            "send" => match parse_send(rest) {
                Some((to, text)) => {
                    handle.send(&to, serde_json::json!({ "text": text }));
                    println!("[p2p-node] 已发送 → {} text={text}", short(&to.to_hex()));
                }
                None => println!("[p2p-node] 用法: send <node_id> <text>"),
            },
            "connect" => match NodeId::parse(rest) {
                Some(target) => match handle.connect(&target).await {
                    Ok(path) => {
                        println!("[p2p-node] connect {} → {path:?}", short(&target.to_hex()))
                    }
                    Err(e) => println!("[p2p-node] connect 失败: {e}"),
                },
                None => println!("[p2p-node] 用法: connect <node_id>"),
            },
            "quit" | "exit" => break,
            other => println!("[p2p-node] 未知命令: {other}（status|peers|send|connect|quit）"),
        }
    }
    println!("[p2p-node] bye");
    handle.shutdown().await;
}

/// 打印一条接收事件 `[recv]`（交互 / 纯服务模式共用的人工观测面）。
fn print_recv(m: &P2pMsg) {
    println!(
        "[recv] from={} hops={} ttl={} payload={}",
        short(&m.from.to_hex()),
        m.hops,
        m.ttl,
        m.payload
    );
}

/// status：路由表 / 端点簿 / 连接阶梯统计（组网观测三件套）。
async fn print_status(handle: &os_p2p::Handle) {
    println!("[p2p-node] == status ==");
    // 路由表（k-buckets 摘要）
    let buckets = handle.buckets_summary().await;
    let known: usize = buckets.iter().map(|b| b.count).sum();
    println!(
        "[p2p-node] 路由表: {known} 节点 / {} 个非空桶",
        buckets.len()
    );
    for b in buckets.iter().take(8) {
        println!(
            "[p2p-node]   po={:>3} count={:>2} {}",
            b.po,
            b.count,
            b.entries.join(" ")
        );
    }
    // 端点簿（地址交换所）
    let endpoints = handle.known_endpoints().await;
    println!("[p2p-node] 端点簿: {} 条观测端点", endpoints.len());
    for e in endpoints.iter().take(8) {
        println!("[p2p-node]   {} → {}", short(&e.id.to_hex()), e.addr);
    }
    // 连接阶梯统计
    let ladder = handle.ladder_stats().await;
    println!(
        "[p2p-node] 连接阶梯: direct={} punched={} relayed={} punch_failed={}",
        ladder.direct, ladder.punched, ladder.relayed, ladder.punch_failed
    );
}

/// peers：已知节点清单。
async fn print_peers(handle: &os_p2p::Handle) {
    println!("[p2p-node] == peers ==");
    let peers = handle.peers().await;
    if peers.is_empty() {
        println!("[p2p-node] （尚无已知节点——配置 NEXOS_P2P_BOOTSTRAP 或等 LAN mDNS 发现）");
        return;
    }
    for p in peers {
        println!(
            "[p2p-node]   {} connected={} underlay={:?} public={} relay={}",
            short(&p.id.to_hex()),
            p.connected,
            p.underlay
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".into()),
            p.public,
            p.route_via
                .map(|r| short(&r.to_hex()))
                .unwrap_or_else(|| "-".into()),
        );
    }
}

/// `send <node_id> <text>` 解析。
fn parse_send(rest: &str) -> Option<(NodeId, String)> {
    let (id, text) = rest.split_once(' ')?;
    let to = NodeId::parse(id.trim())?;
    Some((to, text.trim().to_string()))
}

fn short(hex: &str) -> String {
    let n = hex.len();
    if n <= 12 {
        hex.to_string()
    } else {
        format!("{}…{}", &hex[..8], &hex[n - 4..])
    }
}
