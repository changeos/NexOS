//! tantivy 全文索引（files-agent 接通真实实现）。
//!
//! 设计（见 `docs/agents/files-agent.md` §3、ADR-DEPS-001）：
//! - **schema**：三字段
//!   - `path`（TextOptions `"raw"` 分词 + STORED 字符串）：精确索引（便于
//!     [`SearchIndex::delete_by_path`] 经 [`Term`] 删除）+ 命中后原样回读写回 [`SearchHit::path`]。
//!     不参与查询解析（不入 BM25）。
//!   - `name`（TEXT）：文件名，参与分词与 BM25 评分（命中文件名比命中内容权重高）。
//!   - `content`（TEXT | STORED）：文件正文，参与分词 + BM25 + 高亮 snippet；STORED 让
//!     [`SnippetGenerator::snippet_from_doc`] 可直接从命中文档提取片段，无需回读磁盘。
//! - **查询**：用 [`QueryParser`]（lenient 模式）解析用户输入，对 `name`/`content` 默认字段
//!   做 OR 检索；用 [`TopDocs`]（带 offset + limit）做分页 + BM25 排序；用
//!   [`SnippetGenerator`] 对 `content` 字段生成高亮片段。
//! - **目录**：`Index::open_or_create`（既支持持久化磁盘目录，也支持 RAM/临时目录；测试用
//!   `tempfile::TempDir` 风格的临时目录——本 crate 不引入 `tempfile`，测试用
//!   `std::env::temp_dir()` + UUID 子目录自管）。
//!
//! **状态**：本模块是 files-agent 的「真实实现」替代原 TF 占位（[`crate::files_model::text_search`]
//! 仍保留为纯函数参考与回退）。`DefaultFileManager` 通过 `with_search_index` 注入一个
//! `Arc<SearchIndex>`；未注入时 `fulltext_search` 返回空结果（保留旧行为，便于未配置索引的
//! 调用方平滑降级）。

use std::path::Path as StdPath;
use std::sync::Mutex;

use os_core::{PageRequest, PageResponse};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, TextFieldIndexing, TextOptions, Value, STORED, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, SnippetGenerator, TantivyDocument, Term};

use crate::files::SearchHit;
use crate::ServiceError;

/// 索引目录创建错误统一映射到 [`ServiceError::Internal`]（保留底层消息）。
fn map_err(e: tantivy::TantivyError) -> ServiceError {
    ServiceError::Internal(format!("tantivy: {e}"))
}

/// 单条待索引的文件（路径 + 文件名 + 正文）。
///
/// `content` 为空时仍建索引（仅靠 `name` 可被检索到——便于"按文件名搜索"场景）。
#[derive(Debug, Clone)]
pub struct IndexedFile {
    /// 文件路径（绝对或相对，原样回读，写入 [`SearchHit::path`]）
    pub path: String,
    /// 文件名（参与分词 + BM25）
    pub name: String,
    /// 文件正文（参与分词 + BM25 + 高亮；可为空）
    pub content: String,
}

/// 全文索引句柄——封装 tantivy `Index` + `reader` + schema 字段句柄。
///
/// **线程安全**：内部用 `Mutex<IndexWriter>`（tantivy 写入器非 `Sync`；多线程串行写）。
/// `IndexReader` 与字段句柄本身可多线程共享读。
///
/// **生命周期**：
/// - 构造：[`SearchIndex::create_in_dir`]（持久化目录）或 [`SearchIndex::create_in_ram`]（测试）。
/// - 增量：[`SearchIndex::add_file`] / [`SearchIndex::commit`]；批量索引见
///   [`SearchIndex::index_dir`]（递归扫描目录、读文本文件）。
/// - 查询：[`SearchIndex::search`]（返回分页 [`SearchHit`]）。
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter<TantivyDocument>>,
    /// `path` 字段句柄（STRING | STORED：精确索引 + 回读）
    f_path: Field,
    /// `name` 字段句柄（TEXT，索引 + BM25）
    f_name: Field,
    /// `content` 字段句柄（TEXT + STORED，索引 + BM25 + 高亮回读）
    f_content: Field,
}

impl SearchIndex {
    /// 在指定目录创建（或打开已有的）tantivy 索引。
    ///
    /// 目录不存在会自动创建。已存在且 schema 兼容则复用，否则 tantivy 报错（应换新目录）。
    pub fn create_in_dir<P: AsRef<StdPath>>(dir: P) -> Result<Self, ServiceError> {
        let dir_ref = dir.as_ref();
        std::fs::create_dir_all(dir_ref).map_err(ServiceError::Io)?;
        let mmap_dir = tantivy::directory::MmapDirectory::open(dir_ref)
            .map_err(|e| ServiceError::Internal(format!("tantivy dir: {e}")))?;
        let schema = build_schema();
        let index = Index::open_or_create(mmap_dir, schema).map_err(map_err)?;
        Self::from_index(index)
    }

    /// 在内存中创建索引（无持久化；测试用）。
    pub fn create_in_ram() -> Self {
        let schema = build_schema();
        // RAM 目录无 IO，不会失败。
        let index = Index::create_in_ram(schema);
        Self::from_index(index).expect("RAM 索引构造不会失败")
    }

    fn from_index(index: Index) -> Result<Self, ServiceError> {
        // reader 必须开启 reload policy，写入后能感知新段。
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(map_err)?;
        // 写入器：单线程 + 15MB 内存预算（OS 场景增量索引而非大规模批量，足够；
        // 真实生产可按文件规模调大并由后台 merge 策略合并段）。
        let writer = index
            .writer_with_num_threads(1, 15_000_000)
            .map_err(map_err)?;
        let schema = index.schema();
        let f_path = schema.get_field("path").expect("schema 含 path");
        let f_name = schema.get_field("name").expect("schema 含 name");
        let f_content = schema.get_field("content").expect("schema 含 content");
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            f_path,
            f_name,
            f_content,
        })
    }

    /// 增量添加一个文件到索引（未提交）。提交见 [`Self::commit`]。
    ///
    /// 简单封装 [`doc!`]——path/name/content 三字段。
    pub fn add_file(&self, file: &IndexedFile) -> Result<(), ServiceError> {
        let w = self.writer.lock().expect("writer poisoned");
        let d = doc!(
            self.f_path => file.path.as_str(),
            self.f_name => file.name.as_str(),
            self.f_content => file.content.as_str(),
        );
        w.add_document(d).map_err(map_err)?;
        Ok(())
    }

    /// 提交当前未提交的写入并阻塞等待 reader reload（保证后续 [`Self::search`] 可见）。
    pub fn commit(&self) -> Result<(), ServiceError> {
        let mut w = self.writer.lock().expect("writer poisoned");
        w.commit().map_err(map_err)?;
        // OnCommitWithDelay 策略下，显式 reload 立即生效（测试需要确定时序）。
        self.reader.reload().map_err(map_err)?;
        Ok(())
    }

    /// 删除 path 等于给定值的文档（文件被删/改名时调用）。提交后生效。
    ///
    /// 实现：`path` 字段已用 `STRING | STORED` 索引（精确匹配），故可直接构造
    /// [`Term::from_field_text`] 经 `IndexWriter::delete_term` 删除。删除需后续
    /// [`Self::commit`] 才对 [`Self::search`] 可见（与 tantivy 增量语义一致）。
    ///
    /// 返回值不区分「命中的文档数」（tantivy `delete_term` 不暴露命中计数）——
    /// 调用方若需确认删除效果，可在 commit 后用相同 path 反查。
    pub fn delete_by_path(&self, path: &str) -> Result<(), ServiceError> {
        let w = self.writer.lock().expect("writer poisoned");
        let term = Term::from_field_text(self.f_path, path);
        // tantivy 0.22: delete_term 直接返回 Opstamp（u64），不报错；删除语义在
        // 后续 commit 后对 reader 可见。对「不存在的 path」是 no-op（不写 tombstone）。
        let _opstamp = w.delete_term(term);
        Ok(())
    }

    /// 递归索引一个目录下所有可读文本文件（按扩展名白名单过滤）。
    ///
    /// `extensions` 空 = 不限（尝试读所有非二进制文件；读失败的单个文件跳过）。
    /// 已存在的索引不会被清空（增量语义）；如需重建请用新目录。
    ///
    /// 返回成功索引的文件数。
    pub fn index_dir(&self, root: &str, extensions: &[String]) -> Result<usize, ServiceError> {
        let root_path = StdPath::new(root);
        if !root_path.is_dir() {
            return Err(ServiceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("不是目录或不存在: {root}"),
            )));
        }
        let mut count = 0usize;
        let mut stack = vec![root_path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)?.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                // 扩展名白名单（空 = 全收）
                if !extensions.is_empty() {
                    let ext_ok = p
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| extensions.iter().any(|x| x == e))
                        .unwrap_or(false);
                    if !ext_ok {
                        continue;
                    }
                }
                // 读文件（读失败跳过——保护二进制/权限不足等情况）
                let content = match std::fs::read_to_string(&p) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let path_str = p.to_string_lossy().to_string();
                self.add_file(&IndexedFile {
                    path: path_str,
                    name,
                    content,
                })?;
                count += 1;
            }
        }
        if count > 0 {
            self.commit()?;
        }
        Ok(count)
    }

    /// 执行查询：BM25 排序 + 分页 + 高亮 snippet。
    ///
    /// - `query`：用户自由文本；空串返回空结果（与原占位语义一致）。
    /// - `page`：分页参数（offset/limit）；total 为命中总数（分页前）。
    ///
    /// 失败映射到 [`ServiceError::Internal`]（保留 tantivy 错误消息）。
    pub fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<SearchHit>, ServiceError> {
        if query.trim().is_empty() {
            return Ok(PageResponse {
                items: vec![],
                total: 0,
                offset: page.offset,
                limit: page.limit,
            });
        }
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.f_name, self.f_content]);
        // 长查询不提升字数权重默认值过高，保留默认 BM25；lenient 容忍非法语法。
        let (query_ast, _errs) = parser.parse_query_lenient(query);
        let query: Box<dyn tantivy::query::Query> = query_ast;

        // 用 TopDocs 取分页窗口，Count 取总数；两者经 MultiCollector 一次 search 完成。
        // TopDocs::with_limit(L).and_offset(O)：内部跟踪前 L+O 个文档，返回跳过 O 个后的 L 个。
        // 为保证分页正确（offset+limit 范围内全部返回），limit 用 page.limit、offset 用 page.offset。
        let offset = page.offset as usize;
        let limit = page.limit.max(1) as usize;
        let top_collector = TopDocs::with_limit(limit).and_offset(offset);
        let count_collector = tantivy::collector::Count;

        let mut multi = tantivy::collector::MultiCollector::new();
        let top_handle = multi.add_collector(top_collector);
        let count_handle = multi.add_collector(count_collector);

        let mut multi_fruit = searcher.search(&query, &multi).map_err(map_err)?;
        let top = top_handle.extract(&mut multi_fruit);
        let total_count: usize = count_handle.extract(&mut multi_fruit);
        let total = u32::try_from(total_count).unwrap_or(u32::MAX);

        // snippet 生成器：对 content 字段高亮（若该文档无 content，返回空片段）。
        let snip_gen = SnippetGenerator::create(&searcher, &query, self.f_content).ok();

        let mut items = Vec::with_capacity(top.len());
        for (score, doc_addr) in top {
            let doc: TantivyDocument = searcher.doc(doc_addr).map_err(map_err)?;
            let path = doc
                .get_first(self.f_path)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let snippet = match &snip_gen {
                Some(g) => {
                    let s = g.snippet_from_doc(&doc);
                    s.fragment().to_string()
                }
                None => String::new(),
            };
            items.push(SearchHit {
                path,
                snippet,
                score,
            });
        }
        Ok(PageResponse {
            items,
            total,
            offset: page.offset,
            limit: page.limit,
        })
    }

    /// 当前索引中的文档总数（从 searcher 读取，反映已 commit 的段）。
    pub fn num_docs(&self) -> Result<u64, ServiceError> {
        Ok(self.reader.searcher().num_docs())
    }
}

/// 构建 schema：path(raw 索引|STORED) / name(TEXT) / content(TEXT|STORED)。
///
/// `path` 字段用 `TextOptions` + `"raw"` 分词器（不做分词，整体作单个 token）+ STORED：
/// 既支持搜索后回读 [`SearchHit::path`]，又让 [`SearchIndex::delete_by_path`] 可经
/// [`Term`] 精确删除（等价于 tantivy 旧版的 `STRING` 字段语义）。
fn build_schema() -> Schema {
    let mut sb = Schema::builder();
    // path：raw 分词器（精确匹配，不分词）+ STORED（回读）。不入 BM25 评分。
    let path_indexing = TextFieldIndexing::default().set_tokenizer("raw");
    let path_opts = TextOptions::default()
        .set_indexing_options(path_indexing)
        .set_stored();
    sb.add_text_field("path", path_opts);
    // name：参与分词 + BM25（TEXT 已含默认 record 选项）
    sb.add_text_field("name", TEXT);
    // content：参与分词 + 高亮回读
    sb.add_text_field("content", TEXT | STORED);
    sb.build()
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::PageRequest;

    fn idx() -> SearchIndex {
        SearchIndex::create_in_ram()
    }

    #[test]
    fn add_and_search_hits_content() {
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/a/rust.md".into(),
            name: "rust.md".into(),
            content: "Tantivy is a full text search engine written in rust".into(),
        })
        .unwrap();
        s.add_file(&IndexedFile {
            path: "/b/python.md".into(),
            name: "python.md".into(),
            content: "Python is another language".into(),
        })
        .unwrap();
        s.commit().unwrap();

        let r = s
            .search(
                "tantivy",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].path, "/a/rust.md");
        assert!(r.items[0].snippet.to_lowercase().contains("tantivy"));
        assert!(r.items[0].score > 0.0);
    }

    #[test]
    fn search_matches_name_field() {
        // 仅靠文件名命中的文档（content 不含关键词）也应可被检索到。
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/x/README_tantivy.md".into(),
            name: "README_tantivy.md".into(),
            content: "unrelated content here".into(),
        })
        .unwrap();
        s.commit().unwrap();
        let r = s
            .search(
                "tantivy",
                PageRequest {
                    offset: 0,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].path, "/x/README_tantivy.md");
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/a".into(),
            name: "a".into(),
            content: "hello".into(),
        })
        .unwrap();
        s.commit().unwrap();
        let r = s
            .search(
                "",
                PageRequest {
                    offset: 0,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(r.total, 0);
        assert!(r.items.is_empty());
        // 仅空白也算空查询
        let r2 = s
            .search(
                "   ",
                PageRequest {
                    offset: 0,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(r2.total, 0);
    }

    #[test]
    fn search_no_match_returns_empty() {
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/a".into(),
            name: "a".into(),
            content: "hello world".into(),
        })
        .unwrap();
        s.commit().unwrap();
        let r = s
            .search(
                "nonexistentterm12345",
                PageRequest {
                    offset: 0,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(r.total, 0);
        assert!(r.items.is_empty());
    }

    #[test]
    fn search_pagination_offset_limit() {
        let s = idx();
        // 5 个文档都含 "rust"
        for i in 0..5 {
            s.add_file(&IndexedFile {
                path: format!("/d/{i}.md"),
                name: format!("{i}.md"),
                content: format!("rust content {i}"),
            })
            .unwrap();
        }
        s.commit().unwrap();

        // 第一页
        let p1 = s
            .search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 2,
                },
            )
            .unwrap();
        assert_eq!(p1.total, 5);
        assert_eq!(p1.items.len(), 2);
        // 第二页
        let p2 = s
            .search(
                "rust",
                PageRequest {
                    offset: 2,
                    limit: 2,
                },
            )
            .unwrap();
        assert_eq!(p2.total, 5);
        assert_eq!(p2.items.len(), 2);
        // 两页路径不重叠
        let p1_paths: Vec<_> = p1.items.iter().map(|h| h.path.clone()).collect();
        assert!(p2.items.iter().all(|h| !p1_paths.contains(&h.path)));
        // 越界 offset：total 仍是真实命中数 5，items 为空
        let p3 = s
            .search(
                "rust",
                PageRequest {
                    offset: 100,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(p3.total, 5); // 真实命中数（分页前）
        assert!(p3.items.is_empty()); // 越界 → 空 items
    }

    #[test]
    fn search_scores_sorted_desc() {
        let s = idx();
        // 文档 A：短内容（关键词密度高 → BM25 高）
        s.add_file(&IndexedFile {
            path: "/short".into(),
            name: "short".into(),
            content: "rust".into(),
        })
        .unwrap();
        // 文档 B：长内容（关键词密度低 → BM25 低）
        s.add_file(&IndexedFile {
            path: "/long".into(),
            name: "long".into(),
            content: format!("rust {}", "noise ".repeat(200)),
        })
        .unwrap();
        s.commit().unwrap();
        let r = s
            .search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(r.total, 2);
        // TopDocs 已按 score 降序
        assert!(r.items[0].score >= r.items[1].score);
        assert_eq!(r.items[0].path, "/short");
    }

    #[test]
    fn snippet_highlights_fragment() {
        let s = idx();
        let body = "Tantivy makes it easy to build a search engine. \
                    The tantivy documentation is comprehensive.";
        s.add_file(&IndexedFile {
            path: "/doc".into(),
            name: "doc.md".into(),
            content: body.into(),
        })
        .unwrap();
        s.commit().unwrap();
        let r = s
            .search(
                "tantivy",
                PageRequest {
                    offset: 0,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(r.total, 1);
        // snippet 应包含命中词（大小写不敏感检查）
        assert!(r.items[0].snippet.to_lowercase().contains("tantivy"));
        // snippet 应比原文短（仅片段）
        assert!(r.items[0].snippet.len() < body.len() + 50);
    }

    #[test]
    fn num_docs_reflects_commit() {
        let s = idx();
        assert_eq!(s.num_docs().unwrap(), 0);
        s.add_file(&IndexedFile {
            path: "/a".into(),
            name: "a".into(),
            content: "x".into(),
        })
        .unwrap();
        // 未 commit：reader 看不到
        assert_eq!(s.num_docs().unwrap(), 0);
        s.commit().unwrap();
        assert_eq!(s.num_docs().unwrap(), 1);
    }

    #[test]
    fn create_in_dir_persistent() {
        let dir = std::env::temp_dir().join(format!("tantivy-files-{}", uuid::Uuid::new_v4()));
        // 首次创建 + 写入
        {
            let s = SearchIndex::create_in_dir(&dir).unwrap();
            s.add_file(&IndexedFile {
                path: "/p".into(),
                name: "p.md".into(),
                content: "persisted content rust".into(),
            })
            .unwrap();
            s.commit().unwrap();
        }
        // 重新打开：索引应可被检索
        let s2 = SearchIndex::create_in_dir(&dir).unwrap();
        let r = s2
            .search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].path, "/p");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn index_dir_recurses_and_filters_extensions() {
        let root = std::env::temp_dir().join(format!("tantivy-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.md"), "hello rust world").unwrap();
        std::fs::write(root.join("sub/b.md"), "more rust here").unwrap();
        std::fs::write(root.join("c.txt"), "rust in txt").unwrap();
        std::fs::write(root.join("d.bin"), b"\x00\x01rust").unwrap(); // 二进制
        std::fs::write(root.join("e.md"), "no keyword here").unwrap();

        let s = idx();
        let n = s
            .index_dir(root.to_str().unwrap(), &["md".to_string()])
            .unwrap();
        // 3 个 .md（a / sub/b / e）；d.bin 因扩展名白名单被跳过，c.txt 同理
        assert_eq!(n, 3);
        std::fs::remove_dir_all(&root).ok();

        // 搜索 rust：应命中 a / sub/b（不命中 e，因其无关键词）
        let r = s
            .search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(r.total, 2);
        let paths: Vec<_> = r.items.iter().map(|h| h.path.clone()).collect();
        assert!(paths.iter().any(|p| p.ends_with("a.md")));
        assert!(paths.iter().any(|p| p.ends_with("b.md")));
    }

    #[test]
    fn index_dir_missing_path_errors() {
        let s = idx();
        let err = s.index_dir("/no/such/path/xyz", &[]).unwrap_err();
        assert!(matches!(err, ServiceError::Io(_)));
    }

    #[test]
    fn delete_by_path_removes_single_doc_after_commit() {
        // 删除语义：delete_by_path 后需 commit 才对 search/num_docs 可见。
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/keep.md".into(),
            name: "keep.md".into(),
            content: "rust content keep".into(),
        })
        .unwrap();
        s.add_file(&IndexedFile {
            path: "/gone.md".into(),
            name: "gone.md".into(),
            content: "rust content gone".into(),
        })
        .unwrap();
        s.commit().unwrap();
        assert_eq!(s.num_docs().unwrap(), 2);

        // 删除 /gone.md 并提交
        s.delete_by_path("/gone.md").unwrap();
        // 未 commit：reader 仍看到旧段
        assert_eq!(s.num_docs().unwrap(), 2);
        s.commit().unwrap();
        // commit 后：仅 1 条（tantivy 删除是 tombstone，num_docs 反映可见文档数）
        assert_eq!(s.num_docs().unwrap(), 1);

        // 搜索 rust：只剩 /keep.md
        let r = s
            .search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].path, "/keep.md");
    }

    #[test]
    fn delete_by_path_nonexistent_is_noop() {
        // 删除不存在的 path 不报错（tantivy delete_term 找不到匹配即无 tombstone 写入）。
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/a".into(),
            name: "a".into(),
            content: "rust".into(),
        })
        .unwrap();
        s.commit().unwrap();
        s.delete_by_path("/does/not/exist").unwrap();
        s.commit().unwrap();
        assert_eq!(s.num_docs().unwrap(), 1);
    }

    #[test]
    fn delete_by_path_exact_match_only() {
        // STRING 字段精确匹配——不应误删前缀/子串同名的文档。
        let s = idx();
        s.add_file(&IndexedFile {
            path: "/docs/a.md".into(),
            name: "a.md".into(),
            content: "rust".into(),
        })
        .unwrap();
        s.add_file(&IndexedFile {
            path: "/docs/a.md.bak".into(),
            name: "a.md.bak".into(),
            content: "rust".into(),
        })
        .unwrap();
        s.commit().unwrap();
        s.delete_by_path("/docs/a.md").unwrap();
        s.commit().unwrap();
        // 仅删精确匹配 /docs/a.md，保留 /docs/a.md.bak
        assert_eq!(s.num_docs().unwrap(), 1);
        let r = s
            .search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(r.items[0].path, "/docs/a.md.bak");
    }
}
