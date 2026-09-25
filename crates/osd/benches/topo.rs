//! 组件依赖拓扑排序性能基准（criterion micro-benchmark）。
//!
//! 覆盖算法（见 `src/topo.rs`）：
//! - `topological_sort`：Kahn 算法（入度表 + BFS）做拓扑排序，O(V+E)。
//! - 环检测：未出队的节点视为环中节点。
//!
//! 真实场景：osd 启动时对全部组件（含 disabled）做拓扑排序决定拉起顺序；
//! 组件数典型几十~几百。本基准构造不同形态的大依赖图：
//! - 线性链（每节点依赖前一个）→ 退化为 O(V) 入队
//! - 菱形/分层（多对多）→ 测 dependents HashMap 查询密集路径
//! - 稀疏（每节点依赖固定 1~2 个）→ 接近真实
//!
//! 运行：`cargo bench -p osd`。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use os_core::ResourceQuota;
use osd::component::HealthProbeConfig;
use osd::{topological_sort, ComponentDescriptor, ComponentId};

// ----------------------------------------------------------------------------
// fixture 构造
// ----------------------------------------------------------------------------

fn desc(id: &str, deps: &[&str]) -> ComponentDescriptor {
    ComponentDescriptor {
        id: ComponentId::new(id),
        dependencies: deps.iter().map(|&s| ComponentId::new(s)).collect(),
        quota: ResourceQuota {
            cpu_cores: None,
            memory_bytes: None,
            io_bps_limit: None,
        },
        health_probe: HealthProbeConfig {
            kind: "exec".into(),
            target: "/bin/true".into(),
            interval_secs: 10,
            timeout_secs: 1,
            failure_threshold: 3,
        },
        command: Some("/bin/true".into()),
        enabled: true,
    }
}

/// 线性链：c0 ← c1 ← c2 ← ... ← c(n-1)（每个依赖前一个）。
fn linear_chain(n: usize) -> Vec<ComponentDescriptor> {
    (0..n)
        .map(|i| {
            let deps = if i == 0 {
                vec![]
            } else {
                vec![format!("c{}", i - 1)]
            };
            let deps_ref: Vec<&str> = deps.iter().map(|s| s.as_str()).collect();
            desc(&format!("c{i}"), &deps_ref)
        })
        .collect()
}

/// 稀疏 DAG：每节点依赖前 1~2 个（模拟真实组件图，~2x 边密度）。
fn sparse_dag(n: usize) -> Vec<ComponentDescriptor> {
    (0..n)
        .map(|i| {
            let deps: Vec<String> = match i {
                0 | 1 => vec![],
                _ => vec![format!("c{}", i - 1), format!("c{}", i - 2)],
            };
            let deps_ref: Vec<&str> = deps.iter().map(|s| s.as_str()).collect();
            desc(&format!("c{i}"), &deps_ref)
        })
        .collect()
}

/// 分层 DAG：k 层，每层 w 节点，每节点依赖上一层全部（菱形扩散）。
/// 总节点 n = k * w，边数 ~ k * w^2。
fn layered_dag(layers: usize, width: usize) -> Vec<ComponentDescriptor> {
    let mut out = Vec::with_capacity(layers * width);
    for layer in 0..layers {
        for w in 0..width {
            let id = format!("l{layer}_w{w}");
            let deps: Vec<String> = if layer == 0 {
                vec![]
            } else {
                (0..width).map(|p| format!("l{}_w{p}", layer - 1)).collect()
            };
            let deps_ref: Vec<&str> = deps.iter().map(|s| s.as_str()).collect();
            out.push(desc(&id, &deps_ref));
        }
    }
    out
}

// ----------------------------------------------------------------------------
// topological_sort（不同图形态）
// ----------------------------------------------------------------------------

fn bench_topo_sort(c: &mut Criterion) {
    let mut group = c.benchmark_group("topological_sort");

    // 线性链：100 / 1000 / 5000 节点
    for n in [100usize, 1000, 5000] {
        let descs = linear_chain(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(BenchmarkId::new("linear_chain", n), |b| {
            b.iter(|| {
                let order = topological_sort(black_box(&descs)).unwrap();
                black_box(order);
            });
        });
    }

    // 稀疏 DAG（每节点 ~2 依赖）
    for n in [100usize, 1000, 5000] {
        let descs = sparse_dag(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(BenchmarkId::new("sparse_dag", n), |b| {
            b.iter(|| {
                let order = topological_sort(black_box(&descs)).unwrap();
                black_box(order);
            });
        });
    }

    // 分层 DAG（菱形扩散，边密集）：3 层 × 50 宽 = 150 节点，~7500 边
    for &(layers, width) in &[(3usize, 50usize), (4, 40), (5, 30)] {
        let descs = layered_dag(layers, width);
        let n = descs.len();
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(
            BenchmarkId::new("layered_dag", format!("l{layers}_w{width}")),
            |b| {
                b.iter(|| {
                    let order = topological_sort(black_box(&descs)).unwrap();
                    black_box(order);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_topo_sort);
criterion_main!(benches);
