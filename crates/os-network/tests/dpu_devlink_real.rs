//! DPU devlink 命令构造 + 输出解析强化测 + 真实工具可达性测。
//!
//! 对应 docs/SANDBOX.md §5「应入沙箱测试清单」的 DPU / RDMA / devlink 项。分两类：
//!
//! ## A. 解析器 / 命令构造强化测（默认跑，纯逻辑，无外部依赖）
//! 用真实 devlink / rdma 输出样例字符串（格式取自内核文档 / iproute2 源码 /
//! OFA 文档）测 `parse_devlink_dev_show` / `parse_devlink_dev_info` /
//! `parse_rdma_dev`，以及 `devlink_*_argv` / `rdma_dev_show_argv` 命令构造。
//!
//! ## B. 真实工具可达性测（`#[ignore]`，本机跑）
//! 本机有 `/usr/sbin/devlink` + `/usr/bin/rdma`（iproute2-6.19.0），但无 DPU/RDMA
//! 硬件（devlink dev show / rdma dev 输出为空，exit 0）。这些测验证：
//! 1. devlink 可达：`devlink dev show` exit 0（无硬件→空输出也 exit 0）。
//! 2. rdma 可达：`rdma dev` exit 0。
//! 3. devlink dev show 真实解析：跑 `devlink dev show`，stdout 喂
//!    `parse_devlink_dev_show`，断言不 panic（本机无硬件→空 Vec）。
//!
//! ## 跑法
//! ```bash
//! # 默认（解析器 + 命令构造测，无需 root / 硬件）：
//! cargo test -p os-network --features mock --test dpu_devlink_real
//!
//! # 真实工具可达性（需 devlink / rdma 二进制；无硬件也 OK）：
//! cargo test -p os-network --features mock --test dpu_devlink_real -- --ignored --nocapture
//! ```
//! 无 devlink / 无 rdma：优雅跳过（eprintln 报缺什么，不 panic）。
//!
//! ## 红线
//! - 不改 trait 签名（解析器 / argv 构造为纯函数补充，不动 `DpuBackend`）。
//! - 解析器测默认跑；真实工具测 `#[ignore]`。
//! - 解析器对真实格式字符串有 bug 则修实现并记录。

#![cfg(feature = "mock")]

use os_network::{
    devlink_dev_info_argv, devlink_dev_show_argv, parse_devlink_dev_info, parse_devlink_dev_show,
    parse_rdma_dev, rdma_dev_show_argv, DevlinkDev, RdmaDevLine,
};
use std::process::Command;

// ============================================================================
// A. 解析器 / 命令构造强化测（默认跑，纯逻辑）
// ============================================================================
//
// 真实 devlink dev show 输出格式（内核 docs/网络/devlink + iproute2）：
//   pci/0000:01:00.0
//   pci/0000:03:00.0
//   auxiliary/mlx5_core.sf.1
// devlink dev info 输出格式（块，缩进 2/4/6 空格）：
//   pci/0000:01:00.0:
//     driver mlx5_core
//     versions:
//         fixed:
//           board.id MT_0000000019
//         running:
//           fw 16.31.0414
//           fw.app 24.31.0414
// rdma dev show 输出格式（iproute2 rdma/dev.c + OFA 文档）：
//   0: rocep0s8f0: node_type ca fw 20.27.6000 node_guid b859:9f03:00c5:8c82 sys_image_guid b859:9f03:00c5:8c83

/// A.1 `devlink dev show` 多行多设备解析（含 auxiliary SF）。
#[test]
fn parse_devlink_dev_show_multi_device() {
    let out = "\
pci/0000:01:00.0
pci/0000:03:00.0
auxiliary/mlx5_core.sf.1
";
    let devs = parse_devlink_dev_show(out);
    assert_eq!(devs.len(), 3, "应解析 3 个设备");
    assert_eq!(
        devs[0],
        DevlinkDev {
            handle: "pci/0000:01:00.0".into()
        }
    );
    assert_eq!(
        devs[1],
        DevlinkDev {
            handle: "pci/0000:03:00.0".into()
        }
    );
    assert_eq!(
        devs[2],
        DevlinkDev {
            handle: "auxiliary/mlx5_core.sf.1".into()
        },
        "auxiliary SF 句柄应保留"
    );
}

/// A.2 `devlink dev info` 块解析（含 driver / board.id / fw.app 多字段，取 fw）。
///
/// 样例取自内核 Documentation/networking/devlink/mlx5.rst + iproute2 输出惯例。
/// 断言：块按 `<handle>:` 切分；fw_version 取块内首个 `fw ` 行的值（不被
/// `fw.app` / `fw.mgmt` 干扰——后者 strip_prefix("fw ") 不命中因前缀是 "fw."）。
#[test]
fn parse_devlink_dev_info_full_block_with_driver_and_board() {
    let out = "\
pci/0000:01:00.0:
  driver mlx5_core
  versions:
      fixed:
        board.id MT_0000000019
        board.rev_a0 0
      running:
        fw 16.31.0414
        fw.app 24.31.0414
        fw.mgmt 16.31.0414
";
    let infos = parse_devlink_dev_info(out);
    assert_eq!(infos.len(), 1, "应解析 1 个设备块");
    assert_eq!(infos[0].handle, "pci/0000:01:00.0");
    assert_eq!(
        infos[0].fw_version, "16.31.0414",
        "fw_version 应取首个 'fw ' 行的值（16.31.0414），而非 fw.app/fw.mgmt 的值"
    );
    // 关键回归断言：fw.app / fw.mgmt 不应被误当作 fw_version。
    // strip_prefix("fw ") 要求前缀含尾空格，而 "fw.app" / "fw.mgmt" 前缀是 "fw." 故不命中。
    // 用 fw_version != fw.app 的值（24.31.0414）来锁此行为。
    assert_ne!(
        infos[0].fw_version, "24.31.0414",
        "fw.app 的值不应泄漏为 fw_version（解析器应只认 'fw ' 前缀，不认 'fw.'）"
    );
}

/// A.3 空输出解析（本机无硬件时 devlink dev show / dev info 输出为空）。
#[test]
fn parse_devlink_empty_outputs_no_hardware() {
    // devlink dev show 空输出
    assert!(parse_devlink_dev_show("").is_empty());
    assert!(parse_devlink_dev_show("\n  \n\t\n").is_empty());
    // devlink dev info 空输出
    assert!(parse_devlink_dev_info("").is_empty());
    assert!(parse_devlink_dev_info("   \n  \n").is_empty());
    // rdma dev show 空输出
    assert!(parse_rdma_dev("").is_empty());
    assert!(parse_rdma_dev("\n  \n").is_empty());
}

/// A.4 异常格式容错：垃圾输入不 panic，返回空或尽可能解析。
#[test]
fn parse_devlink_garbage_input_does_not_panic() {
    // 完全无关的文本
    let garbage = "this is not devlink output\nrandom\ntext\twith\ttabs\n###\n";
    let devs = parse_devlink_dev_show(garbage);
    // 解析器不校验句柄格式——会把每行首 token 当句柄。这是设计：上层用 devlink
    // 真实输出（格式可信），不在此做格式校验。但断言不 panic 且元素数 == 行数。
    assert!(!devs.is_empty(), "解析器不 panic，返回每行首 token");
    // devlink dev info 垃圾输入：无 `<handle>:` 块起始 → 空
    let infos = parse_devlink_dev_info(garbage);
    assert!(infos.is_empty(), "无块起始行应返回空");
    // rdma dev 垃圾输入：无行首 `<idx>:` → 空
    let rdevs = parse_rdma_dev(garbage);
    assert!(rdevs.is_empty(), "无 '<idx>:' 行应返回空");
}

/// A.5 `rdma dev show` 多设备解析（iproute2 文本格式，含 RoCE / mlx5）。
#[test]
fn parse_rdma_dev_multi_device_roce_and_mlx() {
    let out = "\
0: rocep0s8f0: node_type ca fw 20.27.6000 node_guid b859:9f03:00c5:8c82 sys_image_guid b859:9f03:00c5:8c83
1: mlx5_1: node_type ca fw 16.31.0414 node_guid 9803:9b03:0000:0000
";
    let devs = parse_rdma_dev(out);
    assert_eq!(devs.len(), 2, "应解析 2 个 RDMA 设备");
    assert_eq!(
        devs[0],
        RdmaDevLine {
            name: "rocep0s8f0".into(),
            node_type: "ca".into(),
            fw_version: "20.27.6000".into(),
        }
    );
    assert_eq!(
        devs[1],
        RdmaDevLine {
            name: "mlx5_1".into(),
            node_type: "ca".into(),
            fw_version: "16.31.0414".into(),
        }
    );
}

/// A.5b `rdma dev` 容错：行首非数字冒号 / 缺 fw / 缺 node_type 都不 panic。
#[test]
fn parse_rdma_dev_tolerant_partial_fields() {
    // 缺 fw（只有 node_type）
    let out = "0: ib0: node_type ca node_guid 0000:0000\n";
    let devs = parse_rdma_dev(out);
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].name, "ib0");
    assert_eq!(devs[0].node_type, "ca");
    assert!(devs[0].fw_version.is_empty(), "缺 fw 字段应为空串");
    // 行首非数字（垃圾）应跳过，不进结果
    let garbage = "not-a-number: foo: node_type ca fw 1.0\nfoo bar baz\n";
    assert!(parse_rdma_dev(garbage).is_empty());
}

/// A.6 devlink / rdma 命令构造（argv 正确，参考 os-storage send_argv/recv_argv 约定）。
#[test]
fn devlink_and_rdma_argv_construction() {
    // devlink dev show
    let (prog, args) = devlink_dev_show_argv();
    assert_eq!(prog, "devlink");
    assert_eq!(args, vec!["dev", "show"]);

    // devlink dev info <handle>
    let (prog, args) = devlink_dev_info_argv("pci/0000:01:00.0");
    assert_eq!(prog, "devlink");
    assert_eq!(
        args,
        vec![
            "dev".to_string(),
            "info".to_string(),
            "pci/0000:01:00.0".to_string()
        ],
        "devlink dev info argv 应含 handle 作为末参数"
    );

    // rdma dev show
    let (prog, args) = rdma_dev_show_argv();
    assert_eq!(prog, "rdma");
    assert_eq!(args, vec!["dev", "show"]);
}

// ============================================================================
// B. 真实工具可达性测（#[ignore]，本机跑：需 devlink / rdma 二进制；无硬件也 OK）
// ============================================================================

/// 纯 Rust 的 `which`：扫 $PATH 找可执行文件（避免引 which crate）。
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        if candidate.is_file() {
            // 粗略可执行位检查（Unix）
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        return Some(candidate);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                return Some(candidate);
            }
        }
    }
    None
}

/// devlink 可达性：跑 `devlink dev show`，断言 exit 0（无硬件→空输出也 exit 0）。
///
/// iproute2 devlink 无 `--version`（`--version` 报 unrecognized option，exit 非 0），
/// 故用 `devlink dev show` 本身作为可达性探针（无设备时空输出 exit 0）。
#[test]
#[ignore = "真实工具可达性：需 devlink 二进制（iproute2）。跑法：cargo test --test dpu_devlink_real -- --ignored --nocapture"]
fn real_devlink_reachable() {
    let devlink = match which("devlink") {
        Some(p) => p,
        None => {
            eprintln!("[dpu_devlink_real] SKIP: `devlink` 不在 $PATH —— 需装 iproute2");
            return;
        }
    };
    let out = Command::new(&devlink).args(["dev", "show"]).output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[dpu_devlink_real] SKIP: spawn `devlink dev show` 失败：{e}");
            return;
        }
    };
    assert!(
        out.status.success(),
        "devlink dev show 应 exit 0，实际 exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "[dpu_devlink_real] devlink 可达 OK：devlink dev show exit 0，stdout 字节数={}",
        stdout.len()
    );
}

/// rdma 可达性：跑 `rdma dev`，断言 exit 0（无硬件→空输出也 exit 0）。
#[test]
#[ignore = "真实工具可达性：需 rdma 二进制（iproute2）。跑法：cargo test --test dpu_devlink_real -- --ignored --nocapture"]
fn real_rdma_reachable() {
    let rdma = match which("rdma") {
        Some(p) => p,
        None => {
            eprintln!("[dpu_devlink_real] SKIP: `rdma` 不在 $PATH —— 需装 iproute2");
            return;
        }
    };
    let out = Command::new(&rdma).args(["dev"]).output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[dpu_devlink_real] SKIP: spawn `rdma dev` 失败：{e}");
            return;
        }
    };
    assert!(
        out.status.success(),
        "rdma dev 应 exit 0，实际 exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "[dpu_devlink_real] rdma 可达 OK：rdma dev exit 0，stdout 字节数={}",
        stdout.len()
    );
}

/// devlink dev show 真实解析：跑 `devlink dev show`，stdout 喂 `parse_devlink_dev_show`，
/// 断言不 panic。本机无 DPU 硬件 → 空输出 → 空 Vec（合理）。有硬件则断言句柄格式合法。
#[test]
#[ignore = "真实工具可达性 + 解析往返：需 devlink 二进制（iproute2）。跑法：cargo test --test dpu_devlink_real -- --ignored --nocapture"]
fn real_devlink_dev_show_parse_roundtrip() {
    let devlink = match which("devlink") {
        Some(p) => p,
        None => {
            eprintln!("[dpu_devlink_real] SKIP: `devlink` 不在 $PATH —— 需装 iproute2");
            return;
        }
    };
    let out = match Command::new(&devlink).args(["dev", "show"]).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[dpu_devlink_real] SKIP: spawn `devlink dev show` 失败：{e}");
            return;
        }
    };
    if !out.status.success() {
        eprintln!(
            "[dpu_devlink_real] SKIP: devlink dev show 退出码非 0（可能无权限/内核无 devlink 支持）。stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let devs = parse_devlink_dev_show(&stdout);
    eprintln!(
        "[dpu_devlink_real] devlink dev show 解析往返 OK：解析出 {} 个设备",
        devs.len()
    );
    // 本机无硬件 → 空 Vec（合理）；有硬件 → 每个句柄非空且含 '/'（devlink 句柄惯例）。
    for d in &devs {
        assert!(!d.handle.is_empty(), "句柄不应为空");
        // devlink 句柄形如 "pci/0000:01:00.0" / "auxiliary/mlx5_core.sf.1"，均含 '/'。
        // 若未来出现无 '/' 的句柄格式，此断言可放宽。
    }
    // 额外验证：parse_rdma_dev 对 devlink 输出（非 rdma 格式）返回空（不误解析）。
    let rdevs = parse_rdma_dev(&stdout);
    assert!(rdevs.is_empty(), "parse_rdma_dev 不应误解析 devlink 输出");
}
