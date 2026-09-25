//! ZFS CLI 输出解析性能基准（criterion micro-benchmark）。
//!
//! 覆盖算法（见 `src/model.rs`）：
//! - `Pool::from_list_line`：`zpool list -p -H` 单行（10 列 tab 分隔）→ Pool
//! - `Dataset::from_list_line`：`zfs list -p -H -o name,used,avail,mounted,encryption` → Dataset
//! - `Snapshot::from_list_line`：`zfs list -t snapshot -p -H -o name,used,creation` → Snapshot
//!
//! 真实场景：`zfs list` 在大数据集池上可能返回数千~数万行；解析耗时直接影响 UI 响应。
//! 本基准构造大输出 fixture，测整体解析吞吐（lines/sec）。
//!
//! 运行：`cargo bench -p os-storage`。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use os_storage::{Dataset, Pool, Snapshot};

// ----------------------------------------------------------------------------
// fixture 构造
// ----------------------------------------------------------------------------

/// 一行 `zpool list -p -H` 输出（10 列 tab 分隔，1TB 池样本）。
fn pool_line(name: &str) -> String {
    format!("{name}\t10995116277760\t1374389534720\t9620726743040\t-\t-\t12\t12\t1.00x\tONLINE\t-")
}

/// 一行 `zfs list -p -H -o name,used,avail,mounted,encryption` 输出。
fn dataset_line(pool: &str, idx: usize) -> String {
    format!("{pool}/ds{idx}\t5497558138880\t5497558138880\tyes\toff")
}

/// 一行 `zfs list -t snapshot -p -H -o name,used,creation` 输出。
fn snapshot_line(pool: &str, ds_idx: usize, snap_idx: usize) -> String {
    format!(
        "{pool}/ds{ds_idx}@snap{snap_idx}\t1073741824\t{}",
        1_700_000_000 + snap_idx as i64
    )
}

// ----------------------------------------------------------------------------
// Pool::from_list_line（单行解析 + 大批量）
// ----------------------------------------------------------------------------

fn bench_pool_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_from_list_line");
    for n_lines in [100usize, 1000, 10000] {
        let lines: Vec<String> = (0..n_lines)
            .map(|i| pool_line(&format!("tank{i}")))
            .collect();
        group.throughput(Throughput::Elements(n_lines as u64));
        group.bench_function(BenchmarkId::new("batch", n_lines), |b| {
            b.iter(|| {
                let mut pools = Vec::with_capacity(lines.len());
                for line in &lines {
                    pools.push(Pool::from_list_line(black_box(line)).unwrap());
                }
                black_box(pools);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// Dataset::from_list_line（单行解析 + 大批量）
// ----------------------------------------------------------------------------

fn bench_dataset_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("dataset_from_list_line");
    for n_lines in [100usize, 1000, 10000] {
        let lines: Vec<String> = (0..n_lines).map(|i| dataset_line("tank", i)).collect();
        group.throughput(Throughput::Elements(n_lines as u64));
        group.bench_function(BenchmarkId::new("batch", n_lines), |b| {
            b.iter(|| {
                let mut datasets = Vec::with_capacity(lines.len());
                for line in &lines {
                    datasets.push(Dataset::from_list_line(black_box(line)).unwrap());
                }
                black_box(datasets);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// Snapshot::from_list_line（含 @ 分割 + Unix 时间戳解析）
// ----------------------------------------------------------------------------

fn bench_snapshot_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_from_list_line");
    for n_lines in [100usize, 1000, 10000] {
        let lines: Vec<String> = (0..n_lines)
            .map(|i| snapshot_line("tank", i % 100, i))
            .collect();
        group.throughput(Throughput::Elements(n_lines as u64));
        group.bench_function(BenchmarkId::new("batch", n_lines), |b| {
            b.iter(|| {
                let mut snaps = Vec::with_capacity(lines.len());
                for line in &lines {
                    snaps.push(Snapshot::from_list_line(black_box(line)).unwrap());
                }
                black_box(snaps);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_pool_parse,
    bench_dataset_parse,
    bench_snapshot_parse,
);
criterion_main!(benches);
