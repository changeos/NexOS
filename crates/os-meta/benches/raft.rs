//! Raft 纯算法性能基准（criterion micro-benchmark）。
//!
//! 覆盖算法（见 `src/raft.rs` / `src/meta_apply.rs`）：
//! - `advance_commit_index`：从高到低扫描日志找首个 quorum + 本 term 索引（§5.4.2/§5.4.3）
//! - `check_election` / `has_quorum_set`：选举投票去重 + 法定数判定（§5.4.1）
//! - `log_is_up_to_date`：RequestVote 的 (term, index) 字典序比较
//! - `InMemoryMetaState::apply`：命令分发到内存表（CAS/UPSERT 走的同一规范化+比较路径）
//!
//! 这些是纯函数 / 锁内同步逻辑，无 IO，适合 criterion `iter` micro-benchmark。
//! 数字用于跟踪回归：算法语义改动（如 has_quorum_set 改 HashSet→BTreeSet）应可被察觉。
//!
//! 运行：`cargo bench -p os-meta`（默认跑全部）；只跑某组 `-- advance_commit`。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use os_core::NodeId;
use os_meta::{
    meta_apply::{InMemoryMetaState, MetaCommand},
    raft::{
        advance_commit_index, advance_commit_index_from_log, check_election, log_is_up_to_date,
        ElectionOutcome, LogEntry, QuorumConfig, Vote,
    },
};
use serde_json::json;

// ----------------------------------------------------------------------------
// fixture 构造
// ----------------------------------------------------------------------------

fn node(s: impl Into<String>) -> NodeId {
    NodeId::new(s)
}

/// N 节点集群配置。
fn cfg_of(n: usize) -> QuorumConfig {
    let members = (0..n).map(|i| node(format!("n{i}"))).collect();
    QuorumConfig::new(members)
}

/// 构造 `len` 条 term=1 的日志（index 1..=len）。
fn log_of(len: u64) -> Vec<LogEntry> {
    (1..=len)
        .map(|i| LogEntry {
            term: 1,
            index: i,
            command: json!({"i": i}),
        })
        .collect()
}

// ----------------------------------------------------------------------------
// advance_commit_index（核心：commitIndex 推进扫描）
// ----------------------------------------------------------------------------

fn bench_advance_commit_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("advance_commit_index");
    // 不同集群规模 × 日志规模：模拟真实 leader 周期性推进 commitIndex。
    // peer_matches 设计为"全员都已复制到 last_log_index"——最坏情况 N 必须扫到顶。
    for n_nodes in [3u32, 5, 7, 9] {
        for log_len in [64u64, 256, 1024, 4096] {
            let cfg = cfg_of(n_nodes as usize);
            let peer_count = n_nodes as usize - 1; // 减去 leader 自身
            let peers: Vec<u64> = vec![log_len; peer_count];
            // term_of 直接返回 1（与本任期一致 → 命中提交）→ 测最坏扫描路径
            group.throughput(Throughput::Elements(log_len));
            group.bench_with_input(
                BenchmarkId::new(format!("nodes={n_nodes}"), log_len),
                &(n_nodes, log_len),
                |b, &(_n, _l)| {
                    b.iter(|| {
                        let new_ci = advance_commit_index(
                            black_box(&cfg),
                            black_box(0),       // 当前 commitIndex=0
                            black_box(log_len), // last_log_index
                            black_box(1),       // current_term=1
                            |_| 1u64,           // term_of: 全 1（本任期）
                            black_box(log_len), // self_match
                            black_box(&peers),
                        );
                        black_box(new_ci);
                    });
                },
            );
        }
    }
    group.finish();
}

/// `advance_commit_index_from_log` 用内存日志切片版（更常用路径：闭包内做 log[idx-1]）。
fn bench_advance_commit_index_from_log(c: &mut Criterion) {
    let mut group = c.benchmark_group("advance_commit_index_from_log");
    for log_len in [64u64, 256, 1024, 4096] {
        let cfg = cfg_of(5);
        let log = log_of(log_len);
        let peers: Vec<u64> = vec![log_len; 4]; // 5 节点 4 follower 全复制
        group.throughput(Throughput::Elements(log_len));
        group.bench_function(BenchmarkId::new("nodes=5", log_len), |b| {
            b.iter(|| {
                let new_ci = advance_commit_index_from_log(
                    black_box(&cfg),
                    black_box(&log),
                    black_box(0),
                    black_box(1),
                    black_box(log_len),
                    black_box(&peers),
                );
                black_box(new_ci);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// check_election（选举投票去重 + quorum 判定）
// ----------------------------------------------------------------------------

fn bench_check_election(c: &mut Criterion) {
    let mut group = c.benchmark_group("check_election");
    // 投票规模：从 10 到 1000 张（含重复/跨 term/不同候选者噪声）。
    for n_votes in [10usize, 100, 1000] {
        let cfg = cfg_of(7);
        let cand = node("cand");
        // 一半票有效（同 term 同候选者，去重后约 n_votes/2 不同 voter）
        let votes: Vec<Vote> = (0..n_votes)
            .map(|i| {
                if i % 2 == 0 {
                    Vote {
                        voter: node(format!("v{}", i / 2)), // 去重后唯一
                        candidate: cand.clone(),
                        term: 5,
                    }
                } else {
                    // 噪声：不同候选者 / 不同 term
                    Vote {
                        voter: node(format!("noise{i}")),
                        candidate: node("other"),
                        term: 4,
                    }
                }
            })
            .collect();
        group.throughput(Throughput::Elements(n_votes as u64));
        group.bench_function(BenchmarkId::new("nodes=7", n_votes), |b| {
            b.iter(|| {
                let outcome = check_election(
                    black_box(&cfg),
                    black_box(&cand),
                    black_box(5),
                    black_box(&votes),
                );
                black_box(outcome);
                let _ = matches!(outcome, ElectionOutcome::Won);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// log_is_up_to_date（RequestVote 投票判定的核心比较）
// ----------------------------------------------------------------------------

fn bench_log_is_up_to_date(c: &mut Criterion) {
    c.bench_function("log_is_up_to_date", |b| {
        b.iter(|| {
            // 模拟大量 RequestVote 比较：term 相同 → index 比较（最常见路径）
            for _ in 0..1000 {
                let r =
                    log_is_up_to_date(black_box(5), black_box(100), black_box(5), black_box(99));
                black_box(r);
            }
        });
    });
}

// ----------------------------------------------------------------------------
// InMemoryMetaState::apply（命令分发 + 主键规范化 + UPSERT）
// 与 CAS 共享同一比较/规范化路径，是 apply_log 的纯算法等价物。
// ----------------------------------------------------------------------------

fn bench_meta_apply(c: &mut Criterion) {
    let mut group = c.benchmark_group("meta_apply");
    for n_entries in [100usize, 1000, 10000] {
        // 预构造 n 条 Put 命令（不同表/键）
        let cmds: Vec<MetaCommand> = (0..n_entries)
            .map(|i| MetaCommand::Put {
                table: format!("t{}", i % 8),
                key: json!(format!("k{i}")),
                value: json!({"v": i}),
            })
            .collect();
        group.throughput(Throughput::Elements(n_entries as u64));
        group.bench_function(BenchmarkId::new("put_distinct", n_entries), |b| {
            b.iter(|| {
                let mut state = InMemoryMetaState::new();
                for cmd in &cmds {
                    let _ = black_box(state.apply(black_box(cmd)));
                }
                black_box(state);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_advance_commit_index,
    bench_advance_commit_index_from_log,
    bench_check_election,
    bench_log_is_up_to_date,
    bench_meta_apply,
);
criterion_main!(benches);
