//! tantivy 全文搜索性能基准（criterion micro-benchmark）。
//!
//! 覆盖算法（见 `src/search_index.rs`）：
//! - `SearchIndex::add_file` + `commit`：建立索引（分词 + 倒排写入）
//! - `SearchIndex::search`：QueryParser lenient 解析 + BM25 排序 + Count + snippet 高亮
//!
//! 真实场景：files 组件初次扫描目录建索引；用户输入关键词查询。
//! 本基准测：
//! - 建索引吞吐（docs/sec）—— 含 commit 段合并开销
//! - 查询延迟（单次 query）—— 含 BM25 排序 + 高亮 snippet
//!
//! 运行：`cargo bench -p os-services`。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use os_core::PageRequest;
use os_services::{IndexedFile, SearchIndex};

// ----------------------------------------------------------------------------
// fixture 构造
// ----------------------------------------------------------------------------

/// 一段固定语料（~60 词），含目标关键词 "rust"。
const SAMPLE_BODY: &str = "\
Rust is a systems programming language that runs blazingly fast, prevents segfaults, \
and guarantees thread safety. It has a rich type system, ownership model, and borrow checker \
that enable memory safety without a garbage collector. Rust is used for web servers, \
operating systems, file systems, and embedded devices. The rust compiler enforces strict rules \
at compile time, eliminating entire classes of bugs before they reach production.";

/// 构造 `n` 个 IndexedFile：路径/名各异但内容都含 "rust"（保证查询有命中）。
fn files_of(n: usize) -> Vec<IndexedFile> {
    (0..n)
        .map(|i| IndexedFile {
            path: format!("/docs/file{i}.md"),
            name: format!("file{i}.md"),
            content: format!("doc {i}: {SAMPLE_BODY}"),
        })
        .collect()
}

/// 预建好索引：n 个文档已 commit。
fn index_with(n: usize) -> SearchIndex {
    let s = SearchIndex::create_in_ram();
    for f in files_of(n) {
        s.add_file(&f).unwrap();
    }
    s.commit().unwrap();
    s
}

// ----------------------------------------------------------------------------
// 建索引（add_file + commit）
// ----------------------------------------------------------------------------

fn bench_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_index_build");
    for n in [100usize, 500, 2000] {
        let files = files_of(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(BenchmarkId::new("add_and_commit", n), |b| {
            b.iter_with_large_drop(|| {
                let s = SearchIndex::create_in_ram();
                for f in &files {
                    s.add_file(black_box(f)).unwrap();
                }
                s.commit().unwrap();
                black_box(s);
            });
        });
    }
    group.finish();
}

// ----------------------------------------------------------------------------
// 查询（search：BM25 + Count + snippet）
// ----------------------------------------------------------------------------

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_query");
    for n in [100usize, 500, 2000] {
        let s = index_with(n);
        // 单词查询（最常见）
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("term_rust", n), |b| {
            b.iter(|| {
                let r = s
                    .search(
                        black_box("rust"),
                        PageRequest {
                            offset: 0,
                            limit: 10,
                        },
                    )
                    .unwrap();
                black_box(r);
            });
        });
        // 多词查询（QueryParser OR 拼接）
        group.bench_function(BenchmarkId::new("multi_word", n), |b| {
            b.iter(|| {
                let r = s
                    .search(
                        black_box("rust safety thread"),
                        PageRequest {
                            offset: 0,
                            limit: 10,
                        },
                    )
                    .unwrap();
                black_box(r);
            });
        });
        // 未命中查询（最坏：全扫描后空结果，但仍跑 Count）
        group.bench_function(BenchmarkId::new("miss_rare", n), |b| {
            b.iter(|| {
                let r = s
                    .search(
                        black_box("zzznonexistentxyz12345"),
                        PageRequest {
                            offset: 0,
                            limit: 10,
                        },
                    )
                    .unwrap();
                black_box(r);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_index_build, bench_search);
criterion_main!(benches);
