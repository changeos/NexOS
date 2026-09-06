//! 路由注册表匹配性能基准（criterion micro-benchmark）。
//!
//! 覆盖算法（见 `src/routing.rs`）：
//! - `RouteRegistry::register`：O(n) 批量注册（`pattern_set` HashSet 做 O(1) 冲突检测，
//!   旧线性扫描实现为 O(n²)）。
//! - `RouteRegistry::match_request`：
//!   - **静态路径短路**：命中已注册静态路由时 O(1)（`static_routes` HashMap 查找）。
//!   - **按 method 分桶扫描**：未短路时仅扫对应 method 的桶内路由，由 specificity
//!     选最具体（参数/wildcard 路由仍线性，桶大小主导延迟）。
//!
//! 本基准构造大量 RouteSpec，测：
//! - 批量注册（`register` 重复调用 n 次）
//! - 命中静态路由（走 O(1) 短路）
//! - 命中参数路由（走分桶线性扫描 + 参数捕获）
//! - 未命中（method 对但路径无匹配，扫满桶后返回 None）
//!
//! 运行：`cargo bench -p os-api`。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use os_api::{routing::match_path, HttpMethod, RouteRegistry, RouteSpec};

// ----------------------------------------------------------------------------
// fixture 构造
// ----------------------------------------------------------------------------

fn spec(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "test".to_string(),
        requires_auth: false,
        required_roles: vec![],
    }
}

/// 构造一个含 `n` 条 GET 路由的注册表：
/// - 60% 参数路由（`/api/v1/r{i}/:id`）
/// - 40% 静态路由（`/api/v1/r{i}/list`）
///
/// 静态路由 path 末段不同以避免冲突。
fn registry_of(n: usize) -> RouteRegistry {
    let mut r = RouteRegistry::new();
    for i in 0..n {
        if i % 5 == 0 {
            // 静态路由（不同末段避免冲突）
            r.register(spec(HttpMethod::Get, &format!("/api/v1/r{i}/list")))
                .unwrap();
        } else if i % 5 == 1 {
            r.register(spec(HttpMethod::Get, &format!("/api/v1/r{i}/detail")))
                .unwrap();
        } else {
            // 参数路由（不同前缀段避免 same_pattern 冲突）
            r.register(spec(HttpMethod::Get, &format!("/api/v1/r{i}/:id")))
                .unwrap();
        }
    }
    r
}

// ----------------------------------------------------------------------------
// register（批量注册 + 冲突检测）
// ----------------------------------------------------------------------------

/// `i % 5 == 0` 注册 `/list`（静态），`== 1` 注册 `/detail`（静态），否则 `/:id`（参数）。
/// 故最大的静态 `/list` 索引为 `n-1` 向下取到 5 的倍数；最大的参数 `/:id` 索引取
/// `n-2`（只要它不是 5 的倍数也不是 +1）。
fn largest_static_list_idx(n: usize) -> usize {
    // 找 <= n-1 的最大 i 使 i % 5 == 0
    ((n - 1) / 5) * 5
}
fn largest_param_idx(n: usize) -> usize {
    // 找 <= n-1 的最大 i 使 i % 5 >= 2（即注册的是 :id）
    let mut i = n - 1;
    while i % 5 < 2 {
        i -= 1;
    }
    i
}

fn bench_register(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_register");
    for n in [100usize, 1000, 5000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(BenchmarkId::new("batch", n), |b| {
            b.iter_with_large_drop(|| {
                let mut r = RouteRegistry::new();
                for i in 0..n {
                    if i % 5 == 0 {
                        r.register(spec(HttpMethod::Get, &format!("/api/v1/r{i}/list")))
                            .unwrap();
                    } else if i % 5 == 1 {
                        r.register(spec(HttpMethod::Get, &format!("/api/v1/r{i}/detail")))
                            .unwrap();
                    } else {
                        r.register(spec(HttpMethod::Get, &format!("/api/v1/r{i}/:id")))
                            .unwrap();
                    }
                }
                black_box(r);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// match_request（每请求匹配延迟）
// ----------------------------------------------------------------------------

fn bench_match_request(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_match_request");
    for n in [100usize, 1000, 5000] {
        let r = registry_of(n);
        // 命中真正的静态路由（走 O(1) static_routes 短路）。
        // largest_static_list_idx 给出注册为 `/list` 的最大索引。
        let hit_static = format!("/api/v1/r{}/list", largest_static_list_idx(n));
        // 命中参数路由（走分桶线性扫描 + 参数捕获）。
        let hit_param = format!("/api/v1/r{}/42", largest_param_idx(n));
        // 未命中（method 对但路径无匹配，扫满桶后返回 None）。
        let miss = "/api/v1/nonexistent/zzz";

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("hit_static", n), |b| {
            b.iter(|| {
                let m = r.match_request(black_box(HttpMethod::Get), black_box(&hit_static));
                black_box(m);
            });
        });
        group.bench_function(BenchmarkId::new("hit_param", n), |b| {
            b.iter(|| {
                let m = r.match_request(black_box(HttpMethod::Get), black_box(&hit_param));
                black_box(m);
            });
        });
        group.bench_function(BenchmarkId::new("miss", n), |b| {
            b.iter(|| {
                let m = r.match_request(black_box(HttpMethod::Get), black_box(miss));
                black_box(m);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// match_path（单次路径模式匹配，无注册表扫描）
// ----------------------------------------------------------------------------

fn bench_match_path(c: &mut Criterion) {
    c.bench_function("match_path_single", |b| {
        let pattern = "/api/v1/:ns/:kind/:id";
        let path = "/api/v1/storage/pools/tank";
        b.iter(|| {
            let p = match_path(black_box(pattern), black_box(path));
            black_box(p);
        });
    });
}

criterion_group!(
    benches,
    bench_register,
    bench_match_request,
    bench_match_path,
);
criterion_main!(benches);
