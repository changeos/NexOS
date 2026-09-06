//! 真实链验收集成测试（dApp 一期 C 阶段，2026-08-31 立项）——只读核验公共 RPC
//! + anvil dev 链全闭环，证明 `chain_verify::verify_evm_tx` 在**真实链上**工作。
//!
//! # 定位与门禁
//!
//! 全部用例 `#[ignore]` 标注：**依赖外网 RPC / 本机 anvil，不进常规门禁**
//! （`cargo test -p os-nexhub` 不带 `--ignored` 时一个都不会跑、不触网）。
//! 手动验收：
//!
//! ```text
//! cargo test -p os-nexhub --test chain_verify_live -- --ignored --nocapture
//! # 或只跑单条：
//! cargo test -p os-nexhub --test chain_verify_live mainnet_real_tx -- --ignored --nocapture
//! ```
//!
//! # env 覆盖（全部可选；默认值 = 2026-08-31 验收时钉死的真实交易）
//!
//! | env | 格式/默认 | 作用 |
//! |---|---|---|
//! | `NEXOS_LIVE_TX_MAINNET` | `"<tx_hash>,<to>,<value_wei>[,<block>]"` | 覆盖主网核验交易（默认：Byzantium 后第一块的真实转账，见下） |
//! | `NEXOS_LIVE_TX_SEPOLIA` | 同上 | 覆盖 Sepolia 核验交易 |
//! | `NEXOS_LIVE_RPC` | URL | 覆盖主网端点（默认 `fallback_rpc_for(1)` 全表） |
//! | `NEXOS_LIVE_RPC_SEPOLIA` | URL | 覆盖 Sepolia 端点（默认 `fallback_rpc_for(11155111)` 全表） |
//! | `NEXOS_LIVE_ANVIL` | `1` | anvil 闭环用例的开关（不设则该用例静默跳过） |
//! | `NEXOS_LIVE_ANVIL_RPC` | URL（默认 `http://127.0.0.1:8545`） | anvil 端点 |
//! | `NEXOS_LIVE_CAST` | 可执行路径 | cast 位置（默认 `~/.foundry/bin/cast` → PATH） |
//!
//! # 验收记录（2026-08-31，中国大陆住宅网络，本机直连无代理）
//!
//! ## 端点可达性（curl POST eth_blockNumber，8s 超时）
//!
//! 主网：ethereum-rpc.publicnode.com **通**（0.7-0.9s）；cloudflare-eth.com
//! HTTP 200 但 JSON-RPC error -32046 "Cannot fulfill request"（拒答）；
//! rpc.ankr.com/eth 要求 API key（401 类）；eth.llamarpc.com HTTP 521（停服）。
//! Sepolia：ethereum-sepolia-rpc.publicnode.com **通**（0.6s）；
//! sepolia.gateway.tenderly.co **通**（0.6s）；rpc.sepolia.org HTTP 404；
//! rpc2.sepolia.org 连接超时。
//!
//! ## 真实历史交易三例断言（任务 1）
//!
//! 主网 fixture：**区块 4,370,001 = Byzantium 硬分叉（EIP-658）后第一个区块**
//! （2017-10-16）里的真实转账 5.735 ETH——
//! hash `0x655a05552f4e176e2eda6c503d3d971b03b688d8a81823488fbdecf6c8caa3fc`，
//! to `0xc5eb713bbde1e192d024da6cb4da968a0c0868cb`，value `5735000000000000000`。
//! **为什么不选更早的**：pre-Byzantium（<4,370,000）的 receipt 无 `status` 字段
//! （以 `root` 代替，实测区块 46,169 的 2015 年转账即此形状），现代全节点的
//! `verify_evm_tx` 会判「receipt 缺 status」→ RpcError——已如实记录为核验器
//! 的历史区块局限，非本测试缺陷。
//! Sepolia fixture：区块 11,605,120 的真实普通转账 2.5 ETH——
//! hash `0x85d158445f2781e0253ed8374a8a4f4f4cc2fe3c8dbf5b430e0f21a28d3ca51c`，
//! to `0xa73700df142d8d3a0212bac7c19ef075c4462ced`，value `2500000000000000000`。
//!
//! 结果（publicnode 端点，三例 × 两链全过）：
//! 主网 Verified{block:4370001} / Mismatch-to / Mismatch-value；
//! Sepolia Verified{block:11605120} / Mismatch-to / Mismatch-value。
//!
//! ## anvil dev 链全闭环（任务 2）
//!
//! foundryup 直连 GitHub 失败（连接超时），改经 ghfast.top 镜像下载
//! foundry_stable_linux_amd64.tar.gz（1.5.1-stable）解压至 `~/.foundry/bin`
//! （未动系统路径）。`anvil --chain-id 1337` 起链后 `cast send` 用 anvil 内置
//! 确定性账户 #0 → #1 真实发 0.01 ETH（第一笔 `0xbe53c82c…c1fb1`，块 1，
//! status 1），随后 verify_evm_tx：Verified / Mismatch-to / Mismatch-value 全过。
//! 测试用例每次运行现场发新交易（不依赖固定 hash），断言同三例。
//!
//! # 已知边界
//!
//! - 公共端点无 SLA：限流/区域性不可达可能让 RpcError 抖动，重试或用
//!   `NEXOS_LIVE_RPC*` 指定自建节点即可；用例失败信息会带 chain_verify 汇总的
//!   各端点原因。
//! - Sepolia 端点表里 rpc2.sepolia.org 会挂满单请求超时——failover 语义使然，
//!   用例最坏多等一个超时窗，属预期行为。

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use os_nexhub::chain_verify::{
    fallback_rpc_for, verify_evm_tx, AmountRule, TxProof, VerifyOutcome,
};

/// 主网真实历史转账（Byzantium 后第一块，2017-10-16，5.735 ETH）。
const MAINNET_TX: RealTx = RealTx {
    chain_id: 1,
    hash: "0x655a05552f4e176e2eda6c503d3d971b03b688d8a81823488fbdecf6c8caa3fc",
    to: "0xc5eb713bbde1e192d024da6cb4da968a0c0868cb",
    value: "5735000000000000000",
    block: Some(4_370_001),
};

/// Sepolia 真实普通转账（块 11,605,120，2.5 ETH）。
const SEPOLIA_TX: RealTx = RealTx {
    chain_id: 11_155_111,
    hash: "0x85d158445f2781e0253ed8374a8a4f4f4cc2fe3c8dbf5b430e0f21a28d3ca51c",
    to: "0xa73700df142d8d3a0212bac7c19ef075c4462ced",
    value: "2500000000000000000",
    block: Some(11_605_120),
};

/// anvil 内置确定性账户（dev 链公开无价值，密钥写死无风险）。
const ANVIL_PK0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const ANVIL_TO1: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
const ANVIL_TO2: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
const ANVIL_VALUE_WEI: &str = "10000000000000000"; // 0.01 ETH

/// 公链单请求超时（国内直连公共端点实测 <1s，15s 足够容纳 failover 中的一家挂起）。
const PUBLIC_TIMEOUT: Duration = Duration::from_secs(15);
/// 端点探测超时（探测要遍历全表，收窄避免挂起端点拖太久）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 一笔真实交易凭证（block 仅在已知时校验，env 覆盖的未知交易跳过块高断言）。
struct RealTx {
    chain_id: u64,
    hash: &'static str,
    to: &'static str,
    value: &'static str,
    block: Option<u64>,
}

/// 解析 env 覆盖格式 `"<hash>,<to>,<value_wei>[,<block>]"`。
fn parse_tx_env(chain_id: u64, spec: &str, env_name: &str) -> RealTx {
    let parts: Vec<&str> = spec.split(',').map(str::trim).collect();
    if parts.len() < 3 || parts.len() > 4 || parts.iter().any(|p| p.is_empty()) {
        panic!(
            "{env_name} 格式应为 \"<tx_hash>,<to>,<value_wei>[,<block>]\"，实测：{spec}"
        );
    }
    RealTx {
        chain_id,
        hash: Box::leak(parts[0].to_string().into_boxed_str()),
        to: Box::leak(parts[1].to_ascii_lowercase().into_boxed_str()),
        value: Box::leak(parts[2].to_string().into_boxed_str()),
        block: parts.get(3).and_then(|b| b.parse().ok()),
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// 拿一条真实交易的凭证（env 覆盖 > 内置 fixture）。
fn resolve_tx(env_name: &str, builtin: RealTx) -> RealTx {
    env_nonempty(env_name)
        .map(|spec| parse_tx_env(builtin.chain_id, &spec, env_name))
        .unwrap_or(builtin)
}

/// 拿端点表（env 覆盖单端点 > fallback 全表）。
fn resolve_rpcs(env_name: &str, chain_id: u64) -> Vec<String> {
    match env_nonempty(env_name) {
        Some(url) => vec![url],
        None => fallback_rpc_for(chain_id),
    }
}

/// 把地址最后一位 hex 换成另一个 hex（保证合法且不同，用于 Mismatch-to 构造）。
fn flip_last_hex_char(addr: &str) -> String {
    let mut s = addr.to_ascii_lowercase();
    let last = s.pop().expect("空地址");
    s.push(if last == '0' { '1' } else { '0' });
    s
}

/// 三例断言：正确凭证 → Verified；改 to → Mismatch-to；改 value → Mismatch-value。
///
/// 这是对「真链核验闭环」的最小完整证明：同一笔链上事实，凭证对就过、
/// 凭证错就拦，且 Mismatch 的 expect/actual 双侧都对得上链上/凭证。
async fn assert_three_way(rpcs: &[String], tx: &RealTx, timeout: Duration) {
    // —— 例 1：真实 to + 真实 value → Verified ——
    let out = verify_evm_tx(
        rpcs,
        &TxProof {
            chain_id: tx.chain_id,
            tx_hash: tx.hash.to_string(),
            expected_to: tx.to.to_string(),
            expected_value: tx.value.to_string(),
            amount_rule: AmountRule::Exact,
            erc20: None,
        },
        timeout,
    )
    .await;
    match out {
        VerifyOutcome::Verified {
            block_number,
            to,
            value_wei,
            ..
        } => {
            assert_eq!(to, tx.to.to_ascii_lowercase(), "Verified.to 应为链上收款地址");
            assert_eq!(value_wei, tx.value, "Verified.value_wei 应为链上金额");
            if let Some(expect_block) = tx.block {
                assert_eq!(block_number, expect_block, "Verified.block_number");
            } else {
                assert!(block_number > 0, "已上块的交易块高应 > 0");
            }
            eprintln!(
                "[live] 例1 Verified ✓ chain={} block={block_number} to={to} value={value_wei}",
                tx.chain_id
            );
        }
        other => panic!("真实凭证应 Verified，实测 {other:?}（端点 {:?}）", rpcs),
    }

    // —— 例 2：改错 to → Mismatch("to")，expect=凭证地址 actual=链上地址 ——
    let bad_to = flip_last_hex_char(tx.to);
    let out = verify_evm_tx(
        rpcs,
        &TxProof {
            chain_id: tx.chain_id,
            tx_hash: tx.hash.to_string(),
            expected_to: bad_to.clone(),
            expected_value: tx.value.to_string(),
            amount_rule: AmountRule::Exact,
            erc20: None,
        },
        timeout,
    )
    .await;
    assert_eq!(
        out,
        VerifyOutcome::Mismatch {
            field: "to".into(),
            expect: bad_to,
            actual: tx.to.to_ascii_lowercase(),
        },
        "改错 to 应 Mismatch-to（链上事实作 actual）"
    );
    eprintln!("[live] 例2 Mismatch-to ✓ chain={}", tx.chain_id);

    // —— 例 3：改错 value（×10 仍是合法十进制）→ Mismatch("value") ——
    let bad_value = format!("{}0", tx.value);
    let out = verify_evm_tx(
        rpcs,
        &TxProof {
            chain_id: tx.chain_id,
            tx_hash: tx.hash.to_string(),
            expected_to: tx.to.to_string(),
            expected_value: bad_value.clone(),
            amount_rule: AmountRule::Exact,
            erc20: None,
        },
        timeout,
    )
    .await;
    assert_eq!(
        out,
        VerifyOutcome::Mismatch {
            field: "value".into(),
            expect: bad_value,
            actual: tx.value.to_string(),
        },
        "改错 value 应 Mismatch-value（两侧均为 wei 十进制）"
    );
    eprintln!("[live] 例3 Mismatch-value ✓ chain={}", tx.chain_id);
}

// ----------------------------------------------------------------------------
// 用例 0：端点可达性探测（对 fallback_rpc_for 全表打 eth_blockNumber）
// ----------------------------------------------------------------------------

/// 探一个端点：Ok(块高 hex) = 健康可用；Err = 传输层/服务层不可用。
async fn probe(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_blockNumber", "params": [],
        }))
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e.to_string().chars().take(60).collect::<String>()))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("(无 message)");
        return Err(format!("JSON-RPC error: {}", &msg[..msg.len().min(50)]));
    }
    v.get("result")
        .and_then(|r| r.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "缺 result".into())
}

/// 探测主网/Sepolia 全部 fallback 端点，输出可达性表；断言两网各至少一家健康
/// （这是下面两条真链用例的前置条件——全挂时给出清晰的表而非逐条 RpcError）。
#[tokio::test]
#[ignore = "依赖外网公共 RPC（可达性探测，见文件头验收记录）"]
async fn rpc_endpoint_reachability_probe() {
    for (name, chain) in [("主网", 1u64), ("Sepolia", 11_155_111u64)] {
        let mut healthy = 0usize;
        for url in fallback_rpc_for(chain) {
            match probe(&url).await {
                Ok(block_hex) => {
                    healthy += 1;
                    let block = u64::from_str_radix(block_hex.trim_start_matches("0x"), 16)
                        .unwrap_or(0);
                    eprintln!("[live] {name:8} {url:50} 通  (block {block})");
                }
                Err(reason) => eprintln!("[live] {name:8} {url:50} 不通 ({reason})"),
            }
        }
        assert!(
            healthy >= 1,
            "{name} fallback 端点应至少一家健康（全表见上方输出；可用 NEXOS_LIVE_RPC* 指定自建节点）"
        );
    }
}

// ----------------------------------------------------------------------------
// 用例 1/2：公共 RPC 只读核验——真实历史交易三例断言
// ----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "依赖外网公共 RPC（真实历史交易核验，见文件头验收记录）"]
async fn mainnet_real_tx_verify() {
    let tx = resolve_tx("NEXOS_LIVE_TX_MAINNET", MAINNET_TX);
    let rpcs = resolve_rpcs("NEXOS_LIVE_RPC", 1);
    assert_three_way(&rpcs, &tx, PUBLIC_TIMEOUT).await;
}

#[tokio::test]
#[ignore = "依赖外网公共 RPC（真实历史交易核验，见文件头验收记录）"]
async fn sepolia_real_tx_verify() {
    let tx = resolve_tx("NEXOS_LIVE_TX_SEPOLIA", SEPOLIA_TX);
    let rpcs = resolve_rpcs("NEXOS_LIVE_RPC_SEPOLIA", 11_155_111);
    assert_three_way(&rpcs, &tx, PUBLIC_TIMEOUT).await;
}

// ----------------------------------------------------------------------------
// 用例 3：anvil dev 链全闭环——真实发交易 → 验真
// ----------------------------------------------------------------------------

/// 找 cast 可执行文件：env 覆盖 → `~/.foundry/bin/cast` → PATH。
fn find_cast() -> PathBuf {
    if let Some(p) = env_nonempty("NEXOS_LIVE_CAST") {
        return PathBuf::from(p);
    }
    let user_cast = home_dir().join(".foundry/bin/cast");
    if user_cast.is_file() {
        return user_cast;
    }
    PathBuf::from("cast") // 交给 PATH
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/home/oem"))
}

/// dev 链闭环：cast 现场发一笔真实转账 → verify_evm_tx 三例断言。
/// 前置：`anvil --chain-id 1337` 已在 127.0.0.1:8545 跑着 + cast 可用
/// （安装路径见文件头验收记录），并设 `NEXOS_LIVE_ANVIL=1` 才实际执行。
#[tokio::test]
#[ignore = "依赖本机 anvil + cast（dev 链全闭环，见文件头验收记录）"]
async fn anvil_dev_chain_closed_loop() {
    if env_nonempty("NEXOS_LIVE_ANVIL").as_deref() != Some("1") {
        eprintln!("[live] 跳过 anvil 闭环（未设 NEXOS_LIVE_ANVIL=1）");
        return;
    }
    let rpc = env_nonempty("NEXOS_LIVE_ANVIL_RPC").unwrap_or_else(|| "http://127.0.0.1:8545".into());
    let cast = find_cast();

    // —— 真实发交易：anvil#0 → anvil#1 转 0.01 ETH（每次现发，hash 不写死）——
    let out = Command::new(&cast)
        .args([
            "send",
            "--rpc-url",
            &rpc,
            "--private-key",
            ANVIL_PK0,
            "--json",
            "--value",
            "0.01ether",
            ANVIL_TO1,
        ])
        .output()
        .unwrap_or_else(|e| panic!("无法执行 cast（{}）：{e}。用 NEXOS_LIVE_CAST 指定路径", cast.display()));
    assert!(
        out.status.success(),
        "cast send 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("cast --json 输出非 JSON：{e}"));
    let tx_hash = receipt["transactionHash"]
        .as_str()
        .expect("cast 回执缺 transactionHash")
        .to_string();
    eprintln!("[live] anvil 已发交易 {tx_hash} → {ANVIL_TO1} (0.01 ETH)");

    let rpcs = vec![rpc];
    let tx = RealTx {
        chain_id: 1337,
        hash: Box::leak(tx_hash.into_boxed_str()),
        to: ANVIL_TO1,
        value: ANVIL_VALUE_WEI,
        block: None, // 块高随 anvil 生命周期漂移，只断言 > 0
    };

    // 三例断言与公共链同构：Verified / Mismatch-to / Mismatch-value。
    assert_three_way(&rpcs, &tx, Duration::from_secs(5)).await;

    // dev 链加验一例「期望地址=另一账户」——模拟真实场景里买家贴错收款地址：
    // 链上实际打给了 #1，凭证却声称打给 #2，必须拦下且 actual 回报链上事实。
    let out = verify_evm_tx(
        &rpcs,
        &TxProof {
            chain_id: 1337,
            tx_hash: tx.hash.to_string(),
            expected_to: ANVIL_TO2.to_string(),
            expected_value: ANVIL_VALUE_WEI.to_string(),
            amount_rule: AmountRule::Exact,
            erc20: None,
        },
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        out,
        VerifyOutcome::Mismatch {
            field: "to".into(),
            expect: ANVIL_TO2.into(),
            actual: ANVIL_TO1.into(),
        },
        "贴错收款地址必须 Mismatch-to"
    );
    eprintln!("[live] dev 链闭环全过 ✓ (chain 1337)");
}
