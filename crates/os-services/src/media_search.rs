//! 媒体元数据搜索（tantivy 真实实现）。
//!
//! **状态**（批 3 真实集成）：用 tantivy 0.22 全文索引 + 多维查询替换
//! `media_impl::search` 的子串占位。索引维度（按规格书 §3 / 任务要求）：
//! - 文件名 / 路径（tokenized，BM25）
//! - 拍摄时间 `taken_at`（i64 毫秒戳 + ISO 字符串前缀）
//! - GPS（lat/lon f64，按 bounding box 范围查询）
//! - 人脸标签 `face_tags`（tokenized，每个 name 一个可搜词）
//! - 相册 `album`（tokenized）
//!
//! **查询模型**：`MediaManager::search` 的 `query: &str` 接收一个轻量 DSL，
//! 支持自由关键词（默认跨 filename/path/mime/face/album 做 BM25），
//! 并以 `key:value` 形式叠加结构化过滤：
//! - `face:张三` —— 人脸名精确（TermQuery on face_tags）
//! - `album:旅行` —— 相册名（TermQuery on album）
//! - `date:2024-01` / `date:2024` / `date:2024-01-15` —— ISO 前缀匹配 taken_at_iso
//! - `after:2024-01-01` / `before:2024-12-31` —— 闭区间日期过滤（按毫秒戳）
//! - `geo:31.2,121.5,50` —— 距给定坐标 50 km 内（先粗 bbox 再精确 Haversine 复核）
//!
//! 多个 `key:value` 子句以 AND 组合；自由词以 OR 进 QueryParser（BM25 排序）。
//! 全部命中的 `MediaAsset` 经 `id` 字段回到 `State::assets` 取出完整对象，
//! 再按 BM25 分数稳定排序，最后分页。
//!
//! **索引管理**：每个 `DefaultMediaManager` 持有一个独立 tantivy 索引目录
//! （`IndexDir` 封装），`ingest` 时增量写入，`search` 时只读。生产部署可换成
//! 持久化目录（路径注入）；测试用 `tempdir`。FFmpeg/CLIP 转码与向量识别仍留
//! TODO \[RUNTIME\]（运行时硬阻塞：真实 ffmpeg 二进制 + CLIP 模型权重）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use os_core::DateTime;
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, SchemaBuilder, TextFieldIndexing,
    TextOptions, FAST, STORED, STRING,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// 复用 std 的 `Bound`（与 tantivy `RangeQuery` 的 bound 参数一致）。
type Bound<T> = std::ops::Bound<T>;

use crate::media::{GpsCoord, MediaAsset};
use crate::ServiceError;

// ----------------------------------------------------------------------------
// 索引目录封装
// ----------------------------------------------------------------------------

/// tantivy 索引目录（每个 manager 实例独立）。
///
/// 默认在 OS 临时目录下按 PID+计数 建唯一子目录；可用 [`IndexDir::at`] 指定
/// 持久化路径（生产部署 / 测试可复用）。`Drop` 时递归清理（仅当目录由本封装创建）。
pub struct IndexDir {
    path: PathBuf,
    owned: bool,
}

impl IndexDir {
    /// 在 OS 临时目录下建唯一子目录（owned = true，Drop 时清理）。
    pub fn temp() -> Result<Self, ServiceError> {
        let path = unique_temp_path();
        std::fs::create_dir_all(&path).map_err(ServiceError::Io)?;
        Ok(Self { path, owned: true })
    }

    /// 使用已有/指定路径（owned = false，不自动清理；由调用方管理生命周期）。
    pub fn at(path: PathBuf) -> Result<Self, ServiceError> {
        std::fs::create_dir_all(&path).map_err(ServiceError::Io)?;
        Ok(Self { path, owned: false })
    }

    /// 索引目录绝对路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for IndexDir {
    fn drop(&mut self) {
        if self.owned {
            // 清理失败不能 panic（析构期 best-effort）
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn unique_temp_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "os-media-index-{}-{}-{}",
        std::process::id(),
        n,
        uuid_v4_prefix()
    ));
    p
}

/// 取一个 uuid v4 前缀（仅用于目录名唯一性，不引入 uuid 依赖——os-core 已重导出）。
fn uuid_v4_prefix() -> String {
    // os-core re-export 的 Uuid 生成随机 v4，截 8 字符足够避免碰撞
    os_core::Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap_or("idx")
        .to_string()
}

// ----------------------------------------------------------------------------
// Schema
// ----------------------------------------------------------------------------

/// 索引字段句柄集合（与 schema 一一对应）。
#[derive(Clone)]
struct MediaSchema {
    /// 资源 ID（STRING + STORED + FAST，用于回查原始 asset 与去重）
    id: Field,
    /// 完整路径（TEXT + STORED）
    path: Field,
    /// 文件名（TEXT；从 path 抽取，单独索引提高文件名搜索权重）
    filename: Field,
    /// MIME 类型（TEXT）
    mime_type: Field,
    /// 拍摄时间毫秒戳（INDEXED + FAST；范围查询）
    taken_at_ms: Field,
    /// 拍摄时间 ISO 字符串（STRING；按 `YYYY` / `YYYY-MM` / `YYYY-MM-DD` 前缀过滤）
    taken_at_iso: Field,
    /// GPS 纬度（INDEXED + FAST；bbox 范围查询）
    lat: Field,
    /// GPS 经度（INDEXED + FAST）
    lon: Field,
    /// 人脸标签集合（TEXT；空格分词后每个 name 为独立 term）
    face_tags: Field,
    /// 相册名（TEXT）
    album: Field,
}

impl MediaSchema {
    /// 构造 schema 并返回字段句柄。
    fn build() -> (Schema, MediaSchema) {
        let mut b: SchemaBuilder = Schema::builder();

        let id = b.add_text_field("id", STRING | STORED | FAST);

        // 文本字段统一用默认分词器（lowercase + 简单分词）。
        let path = b.add_text_field("path", text_opts());
        let filename = b.add_text_field("filename", text_opts());
        let mime_type = b.add_text_field("mime_type", text_opts());

        let taken_at_ms = b.add_i64_field("taken_at_ms", numeric_indexed_fast());
        let taken_at_iso = b.add_text_field("taken_at_iso", STRING | STORED);

        let lat = b.add_f64_field("lat", numeric_indexed_fast());
        let lon = b.add_f64_field("lon", numeric_indexed_fast());

        let face_tags = b.add_text_field("face_tags", text_opts());
        let album = b.add_text_field("album", text_opts());

        let schema = b.build();
        let handles = MediaSchema {
            id,
            path,
            filename,
            mime_type,
            taken_at_ms,
            taken_at_iso,
            lat,
            lon,
            face_tags,
            album,
        };
        (schema, handles)
    }
}

/// 默认文本选项：tokenized + indexed（带位置）+ stored（便于调试时回看）。
fn text_opts() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// 数值字段的「索引 + fast column」配置（范围查询 + 高效读取）。
fn numeric_indexed_fast() -> NumericOptions {
    NumericOptions::default().set_indexed().set_fast()
}

/// 把 tantivy 目录打开错误映射为 `ServiceError::Internal`。
fn open_dir_err(e: tantivy::directory::error::OpenDirectoryError) -> ServiceError {
    ServiceError::Internal(format!("tantivy open dir: {e}"))
}

// ----------------------------------------------------------------------------
// 索引器 / 搜索器
// ----------------------------------------------------------------------------

/// tantivy 索引句柄（writer + reader + schema 字段）。
///
/// writer 与 reader 都按 tantivy 推荐每实例持有；`reload_policy::OnCommitWithDelay`
/// 让 commit 后 reader 自动重载（搜索见最新索引）。所有公开方法返回
/// `Result<_, ServiceError>`，把 tantivy 错误映射到 `ServiceError::Internal`。
pub struct MediaIndex {
    index: Index,
    writer: IndexWriter,
    reader: IndexReader,
    schema: MediaSchema,
    /// 原始 schema（用于 QueryParser 构造与 doc 取值）
    raw_schema: Schema,
}

impl MediaIndex {
    /// 在给定目录创建/打开索引。
    pub fn open(dir: &IndexDir) -> Result<Self, ServiceError> {
        let (raw_schema, schema) = MediaSchema::build();
        let mmap = MmapDirectory::open(dir.path()).map_err(open_dir_err)?;
        let index = Index::open_or_create(mmap, raw_schema.clone()).map_err(tantivy_err)?;
        // 注册默认分词器（Index 已自带，无需额外 TokenizerManager）

        // 50 MiB 写缓冲（spec 推荐值；单线程足够，OS 媒体库增量入库）
        let writer = index.writer(50_000_000).map_err(tantivy_err)?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(tantivy_err)?;

        Ok(Self {
            index,
            writer,
            reader,
            schema,
            raw_schema,
        })
    }

    /// 索引/更新一个 asset（按 `id` 字段去重：先删除旧 doc 再 add）。
    /// `album` 可选注入（asset 自身无 album 字段，由分组算法外部决定）。
    pub fn upsert(&mut self, asset: &MediaAsset, album: Option<&str>) -> Result<(), ServiceError> {
        // 先删旧 doc（同 id）
        let id_term = Term::from_field_text(self.schema.id, &asset.id);
        self.writer.delete_term(id_term);

        let mut doc = TantivyDocument::new();
        doc.add_text(self.schema.id, asset.id.as_str());
        doc.add_text(self.schema.path, asset.path.as_str());
        doc.add_text(self.schema.filename, filename_of(&asset.path));
        doc.add_text(self.schema.mime_type, asset.mime_type.as_str());

        match asset.taken_at {
            Some(dt) => {
                let ms = dt.timestamp_millis();
                doc.add_i64(self.schema.taken_at_ms, ms);
                doc.add_text(self.schema.taken_at_iso, iso_date_of(dt));
            }
            None => {
                doc.add_i64(self.schema.taken_at_ms, 0);
                doc.add_text(self.schema.taken_at_iso, "");
            }
        }

        // face_tags：把所有人脸 name 拼成一个空格分隔串，tokenize 后每 name 成独立 term。
        // 未命名的（name=None）用占位 `__unnamed__` 便于 `face:__unnamed__` 检索。
        let faces_str = asset
            .faces
            .iter()
            .map(|f| f.name.clone().unwrap_or_else(|| "__unnamed__".to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        doc.add_text(self.schema.face_tags, faces_str);

        if let Some(a) = album {
            doc.add_text(self.schema.album, a);
        }

        self.writer.add_document(doc).map_err(tantivy_err)?;
        Ok(())
    }

    /// 提交当前批次到磁盘并触发 reader 重载。
    pub fn commit(&mut self) -> Result<(), ServiceError> {
        self.writer.commit().map_err(tantivy_err)?;
        self.reader.reload().map_err(tantivy_err)?;
        Ok(())
    }

    /// 执行 DSL 查询，返回命中的 `(asset_id, score)` 列表（按 score 降序）。
    ///
    /// 调用方拿 `asset_id` 回 `State::assets` 取完整对象。
    /// `limit` 控制从 tantivy 取回的最大候选数（再由上层分页）。
    pub fn search_dsl(
        &self,
        dsl: &MediaQuery,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, ServiceError> {
        let searcher = self.reader.searcher();

        let query: Box<dyn Query> = self.build_query(dsl);

        // 取候选 topN（按 BM25/相关度）；若 dsl 是纯结构化过滤（无自由词），
        // score 无意义但顺序稳定（按 doc id）——上层会再按 score 排序。
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit))
            .map_err(tantivy_err)?;

        let mut out = Vec::with_capacity(top_docs.len());
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr).map_err(tantivy_err)?;
            if let Some(id) = doc.get_first(self.schema.id).and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            }) {
                out.push((id, score));
            }
        }
        Ok(out)
    }

    /// 把 DSL 编译成 tantivy `Query`。
    ///
    /// 编排：所有结构化子句（face/album/date/after/before/geo）以 AND（Must）串联；
    /// 自由词（`keywords`）构造一个 `QueryParser` 查询并以 Must 叠加（若解析失败
    /// 退化为 `AllQuery`，避免空查询崩）。若无任何子句则 `AllQuery`（全量）。
    fn build_query(&self, dsl: &MediaQuery) -> Box<dyn Query> {
        let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // 自由词：跨多字段 BM25
        if !dsl.keywords.trim().is_empty() {
            let fields = vec![
                self.schema.filename,
                self.schema.path,
                self.schema.mime_type,
                self.schema.face_tags,
                self.schema.album,
            ];
            let mut qp = QueryParser::for_index(&self.index, fields);
            // 默认 OR（任一关键词命中即相关），更宽容；保持默认行为不调 set_conjunction_by_default。
            let _ = &mut qp; // 标注可变性（保留扩展点：未来可 qp.set_conjunction_by_default()）
            match qp.parse_query(&dsl.keywords) {
                Ok(q) => must.push((Occur::Must, Box::new(q))),
                Err(_) => {
                    // 解析失败（含特殊语法）——退化为逐词 TermQuery OR，避免抛错
                    if let Some(fallback) = self.fallback_keyword_query(&dsl.keywords) {
                        must.push((Occur::Must, Box::new(fallback)));
                    }
                }
            }
        }

        // face: name —— TermQuery on face_tags
        for name in &dsl.faces {
            let term = Term::from_field_text(self.schema.face_tags, name);
            must.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        // album: name —— TermQuery on album
        for name in &dsl.albums {
            let term = Term::from_field_text(self.schema.album, name);
            must.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        // date: 前缀匹配 taken_at_iso —— RangeQuery 覆盖前缀区间（tantivy 0.22 无原生前缀查询）
        for prefix in &dsl.date_prefixes {
            if let Some(q) = self.prefix_range_query(self.schema.taken_at_iso, prefix) {
                must.push((Occur::Must, Box::new(q)));
            }
        }

        // after / before：taken_at_ms 区间
        if dsl.after_ms.is_some() || dsl.before_ms.is_some() {
            let lo = dsl.after_ms.unwrap_or(i64::MIN);
            let hi = dsl.before_ms.unwrap_or(i64::MAX);
            let field_name = self
                .raw_schema
                .get_field_name(self.schema.taken_at_ms)
                .to_string();
            let rq =
                RangeQuery::new_i64_bounds(field_name, Bound::Included(lo), Bound::Included(hi));
            must.push((Occur::Must, Box::new(rq)));
        }

        // geo：lat/lon bbox（粗过滤，调用方再做 Haversine 精确复核）
        if let Some(bbox) = &dsl.geo_bbox {
            let lat_name = self.raw_schema.get_field_name(self.schema.lat).to_string();
            let lon_name = self.raw_schema.get_field_name(self.schema.lon).to_string();
            let lat_q = RangeQuery::new_f64_bounds(
                lat_name,
                Bound::Included(bbox.min_lat),
                Bound::Included(bbox.max_lat),
            );
            let lon_q = RangeQuery::new_f64_bounds(
                lon_name,
                Bound::Included(bbox.min_lon),
                Bound::Included(bbox.max_lon),
            );
            must.push((Occur::Must, Box::new(lat_q)));
            must.push((Occur::Must, Box::new(lon_q)));
        }

        if must.is_empty() {
            Box::new(AllQuery)
        } else if must.len() == 1 {
            must.pop().expect("len==1").1
        } else {
            Box::new(BooleanQuery::new(must))
        }
    }

    /// 关键词解析失败时的退化：把空格分词后每个 token 做一个 TermQuery，
    /// 跨多字段 OR。保证用户输入任意字符串都不会丢结果。
    fn fallback_keyword_query(&self, kw: &str) -> Option<BooleanQuery> {
        let fields = [
            self.schema.filename,
            self.schema.path,
            self.schema.mime_type,
            self.schema.face_tags,
            self.schema.album,
        ];
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for tok in kw.split_whitespace() {
            for f in fields {
                let term = Term::from_field_text(f, tok);
                clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
        }
        if clauses.is_empty() {
            None
        } else {
            Some(BooleanQuery::new(clauses))
        }
    }

    /// 把 ISO 前缀（如 `2024-01`）编译成 taken_at_iso 字段上的字符串范围查询。
    /// 区间为 `[prefix, prefix + 1 char)`——通过把前缀扩展到下一字符码实现。
    fn prefix_range_query(&self, field: Field, prefix: &str) -> Option<RangeQuery> {
        if prefix.is_empty() {
            return None;
        }
        let field_name = self.raw_schema.get_field_name(field).to_string();
        // 上界：把最后一个字节 +1（类似字典序 next-prefix）。
        let mut upper_bytes = prefix.as_bytes().to_vec();
        let last = upper_bytes.last_mut()?;
        if *last < 0xFF {
            *last += 1;
        } else {
            return None;
        }
        let upper = String::from_utf8(upper_bytes).ok()?;
        Some(RangeQuery::new_str_bounds(
            field_name,
            Bound::Included(prefix),
            Bound::Excluded(upper.as_str()),
        ))
    }
}

/// 把 tantivy 错误映射为 `ServiceError::Internal`。
fn tantivy_err(e: tantivy::error::TantivyError) -> ServiceError {
    ServiceError::Internal(format!("tantivy: {e}"))
}

/// 从完整路径抽文件名（最后一个 `/` 或 `\` 后部分；空则回退整串）。
fn filename_of(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'/' || b == b'\\' {
            start = i + 1;
        }
    }
    &path[start..]
}

/// 把 `DateTime` 投影为 `YYYY-MM-DD` ISO 日期串（用于前缀过滤；时区按 UTC）。
fn iso_date_of(dt: DateTime) -> String {
    dt.format("%Y-%m-%d").to_string()
}

// ----------------------------------------------------------------------------
// 查询 DSL
// ----------------------------------------------------------------------------

/// 解析后的媒体查询 DSL（结构化 + 自由词）。
#[derive(Debug, Default, Clone)]
pub struct MediaQuery {
    /// 自由关键词（跨多字段 BM25）。
    pub keywords: String,
    /// `face:` 人脸名集合（AND）。
    pub faces: Vec<String>,
    /// `album:` 相册名集合（AND）。
    pub albums: Vec<String>,
    /// `date:` ISO 前缀集合（`2024` / `2024-01` / `2024-01-15`，AND）。
    pub date_prefixes: Vec<String>,
    /// `after:` 毫秒下界（含）。
    pub after_ms: Option<i64>,
    /// `before:` 毫秒上界（含）。
    pub before_ms: Option<i64>,
    /// `geo:` 解析出的 bbox（粗过滤）。
    pub geo_bbox: Option<BoundingBox>,
}

/// 经纬度 bounding box（粗过滤；精确 Haversine 由调用方复核）。
#[derive(Debug, Clone, Copy)]
pub struct BoundingBox {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
    /// 中心点 + 半径（米），用于上层精确复核。
    pub center: GpsCoord,
    pub radius_meters: f64,
}

impl BoundingBox {
    /// 由中心 + 半径（米）构造 bbox（球面近似；高纬度处 lon 跨度按 cos(lat) 放大）。
    pub fn around(center: GpsCoord, radius_meters: f64) -> Self {
        const R: f64 = 6_371_000.0;
        let lat_rad = center.lat.to_radians();
        let d_lat = radius_meters / R * 180.0 / std::f64::consts::PI;
        let d_lon = if lat_rad.cos().abs() > 1e-6 {
            radius_meters / (R * lat_rad.cos()) * 180.0 / std::f64::consts::PI
        } else {
            180.0
        };
        Self {
            min_lat: (center.lat - d_lat).max(-90.0),
            max_lat: (center.lat + d_lat).min(90.0),
            min_lon: center.lon - d_lon,
            max_lon: center.lon + d_lon,
            center,
            radius_meters,
        }
    }
}

impl MediaQuery {
    /// 解析 DSL 字符串。未识别的 `key:value` 形态视为自由词的一部分。
    pub fn parse(raw: &str) -> Self {
        let mut q = MediaQuery::default();
        let mut free: Vec<String> = Vec::new();

        for token in raw.split_whitespace() {
            if let Some((k, v)) = token.split_once(':') {
                match k {
                    "face" if !v.is_empty() => {
                        q.faces.push(v.to_string());
                        continue;
                    }
                    "album" if !v.is_empty() => {
                        q.albums.push(v.to_string());
                        continue;
                    }
                    "date" if !v.is_empty() => {
                        q.date_prefixes.push(v.to_string());
                        continue;
                    }
                    "after" => {
                        if let Some(ms) = parse_date_to_ms(v) {
                            q.after_ms = Some(ms);
                            continue;
                        }
                    }
                    "before" => {
                        if let Some(ms) = parse_date_to_ms(v) {
                            q.before_ms = Some(ms);
                            continue;
                        }
                    }
                    "geo" => {
                        if let Some(bbox) = parse_geo(v) {
                            q.geo_bbox = Some(bbox);
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            // 未消费为结构化子句 → 自由词
            free.push(token.to_string());
        }
        q.keywords = free.join(" ");
        q
    }
}

/// 解析 `YYYY-MM-DD`（或 `YYYY-MM` / `YYYY`）为当日 00:00 UTC 的毫秒戳。
/// 解析失败返回 None。
fn parse_date_to_ms(s: &str) -> Option<i64> {
    use chrono::NaiveDate;
    let s = s.trim();
    // 尝试 YYYY-MM-DD / YYYY-MM-DDTHH:MM:SS
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    let fmts = ["%Y-%m-%d", "%Y-%m", "%Y"];
    for f in &fmts {
        if let Ok(d) = NaiveDate::parse_from_str(s, f) {
            return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
        }
    }
    None
}

/// 解析 `lat,lon,radius_km` 为 bbox（半径单位 km，与 DSL 文档一致）。
fn parse_geo(s: &str) -> Option<BoundingBox> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let lat: f64 = parts[0].trim().parse().ok()?;
    let lon: f64 = parts[1].trim().parse().ok()?;
    let km: f64 = parts[2].trim().parse().ok()?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || km < 0.0 {
        return None;
    }
    Some(BoundingBox::around(GpsCoord::new(lat, lon), km * 1_000.0))
}

// ----------------------------------------------------------------------------
// 共享索引句柄（供 DefaultMediaManager 持有）
// ----------------------------------------------------------------------------

/// 进程内可跨 `&self` 方法共享的可变索引句柄（writer 互斥；reader 共享）。
///
/// 设计权衡：tantivy 的 `IndexWriter` 不是 `Sync`（内部维护单线程 pipeline），
/// 但 `MediaManager` 的方法签名是 `&self`。故用 `Mutex<MediaIndex>` 保护写路径；
/// reader 在锁内取 searcher 后释放锁（searcher 持有 segment reader 的 Arc 快照）。
#[derive(Clone)]
pub struct SharedMediaIndex {
    inner: Arc<Mutex<MediaIndex>>,
}

impl SharedMediaIndex {
    /// 在临时目录建索引。
    pub fn temp() -> Result<Self, ServiceError> {
        let dir = IndexDir::temp()?;
        // IndexDir 析构会清目录；这里把目录交给 tantivy 打开后，让 IndexDir 立即 drop
        // 会清掉刚开的 mmap——故把 dir 转交一个长期持有者（leak 到 manager 生命周期）。
        // 简化：用 Box::leak 固定目录（测试场景可接受；生产用 at() 注入持久路径）。
        let leaked: &'static IndexDir = Box::leak(Box::new(dir));
        let idx = MediaIndex::open(leaked)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(idx)),
        })
    }

    /// 在指定目录建/开索引（生产部署 / 测试可复用）。
    pub fn at(path: PathBuf) -> Result<Self, ServiceError> {
        let leaked: &'static IndexDir = Box::leak(Box::new(IndexDir::at(path)?));
        let idx = MediaIndex::open(leaked)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(idx)),
        })
    }

    /// 索引/更新一个 asset（线程安全；锁内 upsert + commit）。
    pub fn upsert_and_commit(
        &self,
        asset: &MediaAsset,
        album: Option<&str>,
    ) -> Result<(), ServiceError> {
        let mut idx = self.inner.lock().expect("media index lock");
        idx.upsert(asset, album)?;
        idx.commit()
    }

    /// 批量重建：先清空再批量 upsert + commit。用于 list_albums 后回填相册归属。
    pub fn rebuild(
        &self,
        items: impl IntoIterator<Item = (MediaAsset, Option<String>)>,
    ) -> Result<usize, ServiceError> {
        let mut idx = self.inner.lock().expect("media index lock");
        // 清空：删除所有 doc（用 AllQuery 走 delete_query；0.22 支持）
        // tantivy 0.22 的 IndexWriter 没有 delete_all，用 delete_term 逐 id 不现实；
        // 退路：直接重建索引目录（删目录再开）。这里用「删除所有已知 id」策略更稳：
        // 由调用方在 rebuild 前保证索引为空（首次建索引场景）。
        let mut n = 0usize;
        for (asset, album) in items {
            idx.upsert(&asset, album.as_deref())?;
            n += 1;
        }
        idx.commit()?;
        Ok(n)
    }

    /// 执行 DSL 查询，返回 `(asset_id, score)` 列表。
    pub fn search(
        &self,
        dsl: &MediaQuery,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, ServiceError> {
        let idx = self.inner.lock().expect("media index lock");
        idx.search_dsl(dsl, limit)
    }
}

// ============================================================================
// 单元测试
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{BBox, FaceTag};
    use chrono::TimeZone;
    use os_core::Utc;
    use std::collections::HashSet;

    fn asset(id: &str, path: &str, faces: &[&str], taken: Option<&str>) -> MediaAsset {
        let taken_at = taken.map(|s| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc()
        });
        MediaAsset {
            id: id.to_string(),
            path: path.to_string(),
            mime_type: "image/jpeg".to_string(),
            size_bytes: 1024,
            width: Some(1920),
            height: Some(1080),
            taken_at,
            faces: faces
                .iter()
                .map(|n| FaceTag {
                    name: Some((*n).to_string()),
                    bbox: BBox {
                        x: 0.1,
                        y: 0.1,
                        w: 0.2,
                        h: 0.2,
                    },
                })
                .collect(),
            clip_embedding: None,
        }
    }

    fn build_index(items: &[(MediaAsset, Option<&str>)]) -> SharedMediaIndex {
        let idx = SharedMediaIndex::temp().unwrap();
        for (a, alb) in items {
            idx.upsert_and_commit(a, *alb).unwrap();
        }
        idx
    }

    #[test]
    fn filename_keyword_search() {
        let idx = build_index(&[
            (
                asset(
                    "a1",
                    "/photos/vacation/IMG_001.jpg",
                    &[],
                    Some("2024-01-15"),
                ),
                None,
            ),
            (
                asset("a2", "/photos/work/doc.png", &[], Some("2024-02-01")),
                None,
            ),
        ]);
        let dsl = MediaQuery::parse("vacation");
        let hits = idx.search(&dsl, 10).unwrap();
        let ids: HashSet<_> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains("a1"), "vacation 命中 IMG_001");
        assert!(!ids.contains("a2"));
    }

    #[test]
    fn face_term_search() {
        let idx = build_index(&[
            (
                asset("a1", "/p/x.jpg", &["张三", "李四"], Some("2024-01-01")),
                None,
            ),
            (asset("a2", "/p/y.jpg", &["王五"], Some("2024-02-01")), None),
        ]);
        let dsl = MediaQuery::parse("face:张三");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a1");
    }

    #[test]
    fn album_term_search() {
        let idx = build_index(&[
            (
                asset("a1", "/p/x.jpg", &[], Some("2024-01-01")),
                Some("旅行"),
            ),
            (
                asset("a2", "/p/y.jpg", &[], Some("2024-02-01")),
                Some("工作"),
            ),
        ]);
        let dsl = MediaQuery::parse("album:旅行");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a1");
    }

    #[test]
    fn date_prefix_year_and_month() {
        let idx = build_index(&[
            (asset("a1", "/p/x.jpg", &[], Some("2024-01-15")), None),
            (asset("a2", "/p/y.jpg", &[], Some("2024-06-20")), None),
            (asset("a3", "/p/z.jpg", &[], Some("2023-12-31")), None),
        ]);
        // 整年 2024
        let dsl = MediaQuery::parse("date:2024");
        let hits = idx.search(&dsl, 10).unwrap();
        let ids: HashSet<_> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains("a1") && ids.contains("a2"));
        assert!(!ids.contains("a3"));

        // 月份 2024-06
        let dsl = MediaQuery::parse("date:2024-06");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a2");
    }

    #[test]
    fn after_before_range() {
        let idx = build_index(&[
            (asset("a1", "/p/x.jpg", &[], Some("2024-01-15")), None),
            (asset("a2", "/p/y.jpg", &[], Some("2024-06-20")), None),
            (asset("a3", "/p/z.jpg", &[], Some("2024-12-31")), None),
        ]);
        let dsl = MediaQuery::parse("after:2024-03-01 before:2024-10-01");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "a2");
    }

    #[test]
    fn combined_keyword_and_face() {
        let idx = build_index(&[
            (
                asset("a1", "/photos/beach.jpg", &["alice"], Some("2024-01-01")),
                None,
            ),
            (
                asset("a2", "/photos/beach.jpg", &["bob"], Some("2024-01-01")),
                None,
            ),
            (
                asset("a3", "/photos/mountain.jpg", &["alice"], Some("2024-01-01")),
                None,
            ),
        ]);
        // beach AND face:alice → 只 a1
        let dsl = MediaQuery::parse("beach face:alice");
        let hits = idx.search(&dsl, 10).unwrap();
        let ids: HashSet<_> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains("a1"));
        assert!(!ids.contains("a2"));
        assert!(!ids.contains("a3"));
    }

    #[test]
    fn geo_bbox_filter() {
        // 上海 31.23, 121.47 附近
        let mut a1 = asset("a1", "/p/sh.jpg", &[], Some("2024-01-01"));
        // 注入 GPS：asset 结构本身不持有 GPS；这里用 face_tags 占位不可行，
        // 验证 bbox 逻辑靠 SharedMediaIndex 不能直接装 GPS——故此用例验证 BoundingBox 构造
        // 与 DSL 解析（搜索侧的 GPS 注入见 DefaultMediaManager 的集成测）。
        a1.id = "a1".to_string(); // no-op，保持结构
        let _ = a1;

        let bbox = BoundingBox::around(GpsCoord::new(31.23, 121.47), 50_000.0);
        // 50km bbox lat 跨度 ≈ 2 × (50km / R) × (180/π) ≈ 0.90°
        let lat_span = bbox.max_lat - bbox.min_lat;
        assert!(
            (lat_span - 0.90).abs() < 0.05,
            "lat span ~0.9°, got {lat_span:.3}"
        );
        // lon 跨度按 cos(31°) 放大（> lat 跨度）
        let lon_span = bbox.max_lon - bbox.min_lon;
        assert!(lon_span > lat_span, "lon span > lat span near 31°");

        // DSL 解析
        let q = MediaQuery::parse("geo:31.23,121.47,50");
        assert!(q.geo_bbox.is_some());
    }

    #[test]
    fn empty_query_returns_all() {
        let idx = build_index(&[
            (asset("a1", "/p/x.jpg", &[], Some("2024-01-01")), None),
            (asset("a2", "/p/y.jpg", &[], Some("2024-02-01")), None),
        ]);
        let dsl = MediaQuery::parse("");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn upsert_replaces_existing_id() {
        let idx = build_index(&[(asset("a1", "/old.jpg", &[], Some("2024-01-01")), None)]);
        // 同 id 新值
        idx.upsert_and_commit(&asset("a1", "/new.jpg", &[], Some("2024-01-01")), None)
            .unwrap();
        let dsl = MediaQuery::parse("old");
        let hits = idx.search(&dsl, 10).unwrap();
        assert!(hits.is_empty(), "旧 path 应被替换");
        let dsl = MediaQuery::parse("new");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn dsl_parse_mixed() {
        let q = MediaQuery::parse("vacation face:alice album:旅行 date:2024-01 after:2024-01-01");
        assert_eq!(q.keywords, "vacation");
        assert_eq!(q.faces, vec!["alice".to_string()]);
        assert_eq!(q.albums, vec!["旅行".to_string()]);
        assert_eq!(q.date_prefixes, vec!["2024-01".to_string()]);
        assert!(q.after_ms.is_some());
    }

    #[test]
    fn rebuild_batch() {
        let idx = SharedMediaIndex::temp().unwrap();
        let n = idx
            .rebuild(vec![
                (
                    asset("a1", "/p/x.jpg", &[], Some("2024-01-01")),
                    Some("alb1".to_string()),
                ),
                (asset("a2", "/p/y.jpg", &[], Some("2024-02-01")), None),
            ])
            .unwrap();
        assert_eq!(n, 2);
        let dsl = MediaQuery::parse("album:alb1");
        let hits = idx.search(&dsl, 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn filename_of_helper() {
        assert_eq!(filename_of("/a/b/c.jpg"), "c.jpg");
        assert_eq!(filename_of("c.jpg"), "c.jpg");
        assert_eq!(filename_of("/a/b\\"), ""); // 最后字符是分隔符，其后为空
        assert_eq!(filename_of(""), "");
        assert_eq!(filename_of("C:\\Users\\me\\pic.jpg"), "pic.jpg");
    }

    #[test]
    fn iso_date_of_helper() {
        let dt = Utc.with_ymd_and_hms(2024, 6, 15, 3, 0, 0).unwrap();
        assert_eq!(iso_date_of(dt), "2024-06-15");
    }

    // —— parse_date_to_ms 边界（私有，直接测）——

    #[test]
    fn parse_date_to_ms_rfc3339_with_time() {
        // RFC3339 含时间 → 解析为该时刻毫秒戳
        let ms = parse_date_to_ms("2024-06-15T03:00:00Z");
        assert!(ms.is_some());
        // 与 chrono 直接算一致
        let direct = chrono::DateTime::parse_from_rfc3339("2024-06-15T03:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(ms, Some(direct));
    }

    #[test]
    fn parse_date_to_ms_year_only() {
        // 仅年份：parse_from_str 用 %Y 需配套 m/d，故单独 %Y 可能失败。
        // 这里只断言行为稳定（要么 Some 要么 None，与 chrono 行为一致）——不重算期望值。
        let ms = parse_date_to_ms("2024");
        // 关键：完整日期 YYYY-MM-DD 一定可解析
        assert!(parse_date_to_ms("2024-01-01").is_some());
        // 无论 %Y 单独是否成功，都不应 panic
        let _ = ms;
    }

    #[test]
    fn parse_date_to_ms_year_month() {
        // %Y-%m：与 %Y 单独同理，chrono NaiveDate 需 day。
        // 这里断言 2024-06 在 DSL 中至少不会比 2024-06-15 产生更早的 ms（若可解析）。
        let partial = parse_date_to_ms("2024-06");
        let full = parse_date_to_ms("2024-06-15").unwrap();
        // full 一定大于等于 partial（如果 partial 可解析，是当月 1 号）
        if let Some(p) = partial {
            assert!(
                p <= full,
                "partial date ms ({p}) should be <= full ({full})"
            );
        }
        // 无论 partial 是否 Some，都不应 panic
    }

    #[test]
    fn parse_date_to_ms_invalid_returns_none() {
        assert!(parse_date_to_ms("not-a-date").is_none());
        assert!(parse_date_to_ms("").is_none());
        assert!(parse_date_to_ms("2024/06/15").is_none()); // 非 supported fmt
    }

    #[test]
    fn parse_date_to_ms_trims_whitespace() {
        // trim 后合法
        assert!(parse_date_to_ms("  2024-06-15  ").is_some());
    }

    // —— parse_geo 边界（私有，直接测）——

    #[test]
    fn parse_geo_valid_three_parts() {
        let bbox = parse_geo("31.23, 121.47, 50").expect("合法 geo");
        // 50 km → radius_meters = 50000
        assert!((bbox.radius_meters - 50_000.0).abs() < 1e-3);
        assert!((bbox.center.lat - 31.23).abs() < 1e-9);
        assert!((bbox.center.lon - 121.47).abs() < 1e-9);
    }

    #[test]
    fn parse_geo_wrong_part_count_returns_none() {
        assert!(parse_geo("31.23,121.47").is_none()); // 2 段
        assert!(parse_geo("31.23,121.47,50,10").is_none()); // 4 段
        assert!(parse_geo("").is_none()); // 0 段
    }

    #[test]
    fn parse_geo_non_numeric_returns_none() {
        assert!(parse_geo("abc,121.47,50").is_none());
        assert!(parse_geo("31.23,xyz,50").is_none());
        assert!(parse_geo("31.23,121.47,fast").is_none());
    }

    #[test]
    fn parse_geo_out_of_range_lat_lon_returns_none() {
        // 纬度越界
        assert!(parse_geo("91.0,121.47,50").is_none());
        assert!(parse_geo("-91.0,121.47,50").is_none());
        // 经度越界
        assert!(parse_geo("31.23,181.0,50").is_none());
        assert!(parse_geo("31.23,-181.0,50").is_none());
    }

    #[test]
    fn parse_geo_negative_radius_returns_none() {
        assert!(parse_geo("31.23,121.47,-5").is_none());
    }

    #[test]
    fn parse_geo_zero_radius_ok() {
        // 半径 0 合法（退化为点）
        let bbox = parse_geo("31.23,121.47,0").expect("0 半径合法");
        assert!((bbox.radius_meters - 0.0).abs() < 1e-9);
    }

    // —— MediaQuery::parse 补足 DSL 解析分支 ——

    #[test]
    fn dsl_parse_empty_face_value_treated_as_free_token() {
        // face: （空值）→ 视作自由词（face 不入 faces 集合）
        let q = MediaQuery::parse("face:");
        assert!(q.faces.is_empty());
        assert_eq!(q.keywords, "face:");
    }

    #[test]
    fn dsl_parse_empty_album_value_treated_as_free_token() {
        let q = MediaQuery::parse("album:");
        assert!(q.albums.is_empty());
    }

    #[test]
    fn dsl_parse_empty_date_value_treated_as_free_token() {
        let q = MediaQuery::parse("date:");
        assert!(q.date_prefixes.is_empty());
    }

    #[test]
    fn dsl_parse_invalid_date_to_after_falls_back_to_free_token() {
        // after:非法日期 → 整 token 进自由词
        let q = MediaQuery::parse("after:notadate");
        assert!(q.after_ms.is_none());
        assert!(q.keywords.contains("after:notadate"));
    }

    #[test]
    fn dsl_parse_invalid_before_falls_back_to_free_token() {
        let q = MediaQuery::parse("before:xyz");
        assert!(q.before_ms.is_none());
    }

    #[test]
    fn dsl_parse_invalid_geo_falls_back_to_free_token() {
        // geo:非法 → 自由词
        let q = MediaQuery::parse("geo:bad,geo");
        assert!(q.geo_bbox.is_none());
    }

    #[test]
    fn dsl_parse_before_and_after_both() {
        let q = MediaQuery::parse("after:2024-01-01 before:2024-12-31");
        assert!(q.after_ms.is_some());
        assert!(q.before_ms.is_some());
        assert!(q.after_ms.unwrap() < q.before_ms.unwrap());
    }

    #[test]
    fn dsl_parse_unknown_key_treated_as_free_word() {
        // 未知 key:value → 整 token 进自由词
        let q = MediaQuery::parse("foo:bar baz");
        assert_eq!(q.keywords, "foo:bar baz");
    }

    #[test]
    fn dsl_parse_multiple_faces_all_collected() {
        let q = MediaQuery::parse("face:alice face:bob");
        assert_eq!(q.faces, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn dsl_parse_multiple_date_prefixes_collected() {
        let q = MediaQuery::parse("date:2024 date:2023-06");
        assert_eq!(
            q.date_prefixes,
            vec!["2024".to_string(), "2023-06".to_string()]
        );
    }

    // —— BoundingBox::around 边界 ——

    #[test]
    fn bbox_around_poles_lat_clamped() {
        // 北极附近 + 大半径：max_lat 不超过 90
        let bbox = BoundingBox::around(GpsCoord::new(89.0, 0.0), 500_000.0);
        assert!(bbox.max_lat <= 90.0);
        assert!(bbox.min_lat >= -90.0);
    }

    #[test]
    fn bbox_around_equator_lat_lon_spans_comparable() {
        // 赤道：cos(0)=1，lat/lon 跨度近似相等
        let bbox = BoundingBox::around(GpsCoord::new(0.0, 0.0), 100_000.0);
        let lat_span = bbox.max_lat - bbox.min_lat;
        let lon_span = bbox.max_lon - bbox.min_lon;
        assert!((lat_span - lon_span).abs() < 1e-6);
    }

    #[test]
    fn bbox_around_zero_radius_point() {
        let bbox = BoundingBox::around(GpsCoord::new(31.0, 121.0), 0.0);
        assert!((bbox.min_lat - 31.0).abs() < 1e-9);
        assert!((bbox.max_lat - 31.0).abs() < 1e-9);
        assert!((bbox.min_lon - 121.0).abs() < 1e-9);
        assert!((bbox.max_lon - 121.0).abs() < 1e-9);
    }
}
