//! os-p2p CLI 冒烟测试——`p2p-node` 二进制的 status/peers 输出与 stdin 驱动。
//!
//! std 测试内 spawn 二进制（`CARGO_BIN_EXE_p2p-node` 由 cargo 提供给集成测试）
//! + 管道驱动 stdin + 后台线程收 stdout，断言：
//!   启动横幅（NodeID/OverlayAddr/昵称）→ status（路由表/端点簿/连接阶梯）→
//!   peers → quit 干净退出（exit 0）。
//!
//! P2b 追加：**重启身份稳定**——同一 `NEXOS_P2P_KEY_FILE` 跑两轮，NodeID 一致
//! （密钥持久化修漂移的端到端验收）。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// 从累积输出中等待包含指定标记的行（带超时）。
fn wait_for_marker(rx: &mpsc::Receiver<String>, out: &mut String, marker: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if out.contains(marker) {
            return true;
        }
        // 非阻塞收割新行
        while let Ok(line) = rx.try_recv() {
            out.push_str(&line);
            out.push('\n');
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 从横幅输出提取 `NodeID      = 0x…` 的完整 hex。
fn extract_node_id(out: &str) -> String {
    let line = out
        .lines()
        .find(|l| l.contains("NodeID      = 0x"))
        .expect("横幅应有 NodeID 行");
    line.split('=').nth(1).expect("= 后有值").trim().to_string()
}

#[test]
fn cli_status_smoke() {
    let exe = env!("CARGO_BIN_EXE_p2p-node");
    // KEY_FILE 指到临时文件——既隔离系统位置，又是重启身份断言的载体
    let key_file = std::env::temp_dir().join(format!(
        "p2p-cli-key-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&key_file);
    let key_str = key_file.to_string_lossy().to_string();

    let mut child = Command::new(exe)
        .env("NEXOS_P2P_LISTEN", "127.0.0.1:0")
        .env("NEXOS_P2P_NAME", "smoke-node")
        .env("NEXOS_P2P_MDNS", "0") // CI 无组播——静默降级路径本身也是被测语义
        .env("NEXOS_P2P_KEY_FILE", &key_str) // P2b：密钥持久化（测试内隔离）
        .env_remove("NEXOS_P2P_BOOTSTRAP")
        .env_remove("NEXOS_P2P_PUBLIC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("p2p-node 启动");

    // 后台线程持续收 stdout
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().expect("stdout 管道");
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut out = String::new();
    // ① 启动横幅：昵称 / NodeID / OverlayAddr / 监听地址
    for marker in [
        "name        = smoke-node",
        "NodeID      = 0x",
        "OverlayAddr = 0x",
        "listen      = 127.0.0.1:",
    ] {
        assert!(
            wait_for_marker(&rx, &mut out, marker),
            "横幅应含 {marker:?}，实际输出:\n{out}"
        );
    }
    let first_node_id = extract_node_id(&out);

    // ② status：路由表 / 端点簿 / 连接阶梯
    child
        .stdin
        .as_mut()
        .expect("stdin 管道")
        .write_all(b"status\n")
        .expect("写 status");
    for marker in ["== status ==", "路由表", "端点簿", "连接阶梯: direct=0"] {
        assert!(
            wait_for_marker(&rx, &mut out, marker),
            "status 应含 {marker:?}，实际输出:\n{out}"
        );
    }

    // ③ peers（空表提示）+ 未知命令回显
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"peers\nbogus\n")
        .unwrap();
    assert!(
        wait_for_marker(&rx, &mut out, "== peers =="),
        "peers 分节，实际输出:\n{out}"
    );
    assert!(
        wait_for_marker(&rx, &mut out, "未知命令: bogus"),
        "未知命令回显，实际输出:\n{out}"
    );

    // ④ send 参数错误提示（不崩溃）
    child.stdin.as_mut().unwrap().write_all(b"send\n").unwrap();
    assert!(
        wait_for_marker(&rx, &mut out, "用法: send"),
        "send 用法提示，实际输出:\n{out}"
    );

    // ⑤ quit：优雅退出（exit 0）
    child.stdin.as_mut().unwrap().write_all(b"quit\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "quit 后应退出，实际输出:\n{out}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("wait 失败: {e}"),
        }
    };
    assert!(status.success(), "quit 应干净退出（exit 0），实际 {status}");
    // 进程已退——收割管道余量（reader 线程可能尚未读完最后几行）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !out.contains("bye") && std::time::Instant::now() < deadline {
        while let Ok(line) = rx.try_recv() {
            out.push_str(&line);
            out.push('\n');
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(out.contains("bye"), "退出告别语，实际输出:\n{out}");

    // ⑥ P2b 重启身份稳定：同一 KEY_FILE 再跑一轮 → 同一 NodeID
    let second = Command::new(exe)
        .env("NEXOS_P2P_LISTEN", "127.0.0.1:0")
        .env("NEXOS_P2P_NAME", "smoke-node")
        .env("NEXOS_P2P_MDNS", "0")
        .env("NEXOS_P2P_KEY_FILE", &key_str)
        .env_remove("NEXOS_P2P_BOOTSTRAP")
        .env_remove("NEXOS_P2P_PUBLIC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("p2p-node 二次启动");
    let mut second = second;
    let mut second_out = String::new();
    {
        let stdout = second.stdout.take().expect("stdout 管道");
        let reader = BufReader::new(stdout);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        for line in reader.lines() {
            let Ok(l) = line else { break };
            second_out.push_str(&l);
            second_out.push('\n');
            if second_out.contains("listen      = 127.0.0.1:")
                || std::time::Instant::now() > deadline
            {
                // 横幅四行到齐即停（阻塞读留给 quit 触发退出）
                if second_out.contains("OverlayAddr = 0x") {
                    break;
                }
            }
        }
    }
    assert!(
        second_out.contains("NodeID      = 0x"),
        "二次启动横幅应含 NodeID，实际:\n{second_out}"
    );
    assert_eq!(
        extract_node_id(&second_out),
        first_node_id,
        "同一 KEY_FILE 重启 NodeID 稳定（密钥持久化）"
    );
    let _ = second.kill();
    let _ = second.wait();
    let _ = std::fs::remove_file(&key_file);
}
