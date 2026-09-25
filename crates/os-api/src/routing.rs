//! 路由注册表与匹配算法（纯逻辑，无网络依赖）。
//!
//! 设计：
//! - `RouteRegistry` 持有全部已注册路由（保留注册顺序的 `Vec` + 三张索引）。
//! - 路径模式支持 Axum 风格参数段 `:id`（单段捕获），`*` 通配剩余段（catch-all，可选）。
//! - 匹配优先级：静态段优先于参数段；最具体匹配胜出。
//! - 注册时检测 `method + path` 冲突（同 method 同模式才算冲突，参数与静态视为不同模式）。
//! - 性能（见 benches/routing.rs）：
//!   - `register` 用 `pattern_set`（HashSet）做 O(1) 冲突检测，批量注册为 O(n)
//!     （旧实现线性扫描 → O(n²)）。
//!   - `match_request` 先对静态路径走 `static_routes` HashMap O(1) 短路；
//!     未命中再按 method 分桶扫描（桶内通常远小于全量，方法维度常数级提速）。
//!   - 语义不变：返回的索引指向 `routes`，与 `all()` 一致；specificity 优先级、
//!     参数提取、wildcard 行为与旧实现逐字相同（现有测试全过）。
//!
//! 该模块为纯算法，便于单测覆盖各种匹配场景；真实 Axum 路由挂载由 Gateway 骨架
//! 在 `start` 中按此注册表构建（见 `gateway_impl.rs`）。

use std::collections::{HashMap, HashSet};

use crate::gateway::{ApiRequest, HttpMethod, RouteSpec};

// ----------------------------------------------------------------------------
// 路径段
// ----------------------------------------------------------------------------

/// 路径模式中的单一段类型。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// 静态字面段（精确匹配，如 `pools`）。
    Literal(String),
    /// 参数段（`:name`，捕获一段；name 为参数名）。
    Param(String),
    /// 通配剩余段（`*`，捕获多段至末尾）。
    Wildcard,
}

/// 解析路径模式（按 `/` 切分，自动去除空前/后段）。
fn parse_pattern(pattern: &str) -> Vec<Segment> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s == "*" {
                Segment::Wildcard
            } else if let Some(name) = s.strip_prefix(':') {
                Segment::Param(name.to_string())
            } else {
                Segment::Literal(s.to_string())
            }
        })
        .collect()
}

/// 解析真实请求路径（按 `/` 切分，去除空前/后段）。
fn parse_path(path: &str) -> Vec<String> {
    // 去除 query 串
    let path = path.split('?').next().unwrap_or(path);
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// ----------------------------------------------------------------------------
// 匹配结果
// ----------------------------------------------------------------------------

/// 路径匹配结果：捕获的参数（参数名 -> 值）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathParams {
    inner: HashMap<String, String>,
}

impl PathParams {
    /// 构造空参数集。
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入一对参数。
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.inner.insert(name.into(), value.into());
    }

    /// 按参数名取值。
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner.get(name).map(|s| s.as_str())
    }

    /// 是否无参数。
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 参数数量。
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 返回内部 HashMap 的引用。
    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.inner
    }
}

/// 尝试用模式匹配真实路径；成功返回捕获参数，失败返回 None。
///
/// 规则：
/// - 逐段比较；Literal 须精确相等；Param 捕获任意非空段；Wildcard 捕获剩余所有段。
/// - 模式段与路径段数量须一致（Wildcard 例外：它在末尾时消耗剩余段）。
pub fn match_path(pattern: &str, path: &str) -> Option<PathParams> {
    let segs = parse_pattern(pattern);
    let path_segs = parse_path(path);
    let mut params = PathParams::new();

    let mut i = 0usize;
    let mut j = 0usize;
    while i < segs.len() && j < path_segs.len() {
        match &segs[i] {
            Segment::Literal(lit) => {
                if &path_segs[j] != lit {
                    return None;
                }
            }
            Segment::Param(name) => {
                params.insert(name.clone(), path_segs[j].clone());
            }
            Segment::Wildcard => {
                // 消耗剩余所有段（可为空）
                for v in &path_segs[j..] {
                    params.insert("wildcard".to_string(), v.clone());
                }
                return Some(params);
            }
        }
        i += 1;
        j += 1;
    }

    // 末尾单独的 Wildcard 且路径已耗尽：匹配空
    if i < segs.len() && matches!(segs[i], Segment::Wildcard) && j == path_segs.len() {
        return Some(params);
    }

    if i == segs.len() && j == path_segs.len() {
        Some(params)
    } else {
        None
    }
}

// ----------------------------------------------------------------------------
// 路由注册表
// ----------------------------------------------------------------------------

/// 路由注册表——聚合各组件声明路由，支持注册/查询/匹配。
///
/// 数据结构（性能优化，见 benches/routing.rs）：
/// - `routes`：保留注册顺序的全部路由；`all()` 与 `match_request` 返回的索引均指此 Vec。
/// - `by_method`：按 HTTP method 分桶，每桶存该 method 的路由索引——匹配时只扫
///   对应桶，使 method 维度常数级（5 个 method 共存时提速约 5×）。
/// - `static_routes`：`(method, 完整 path) → 路由索引`。对静态路径路由（无参数/wildcard）
///   用 HashMap O(1) 查找；命中即短路（静态路由 specificity 最高，必为该 path 的最具体匹配）。
/// - `pattern_set`：`(method, pattern_key) → 已注册`。register 时 O(1) 检测模式冲突，
///   替代旧的线性扫描（批量注册从 O(n²) 降到 O(n)）。
#[derive(Debug, Default)]
pub struct RouteRegistry {
    /// 已注册路由（保留注册顺序）。
    routes: Vec<RouteSpec>,
    /// method → 该 method 下全部路由索引（匹配时按 method 分桶扫描）。
    by_method: HashMap<HttpMethod, Vec<usize>>,
    /// (method, 静态完整 path) → 路由索引。仅收录无参数段/wildcard 的静态路由。
    static_routes: HashMap<(HttpMethod, String), usize>,
    /// (method, pattern_key) → 已注册。O(1) 检测模式冲突（替代旧线性扫描）。
    pattern_set: HashSet<(HttpMethod, String)>,
}

impl RouteRegistry {
    /// 构造空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一条路由；返回 `Err(RouteConflict)` 当 method + path 模式完全相同时。
    ///
    /// 注：静态与参数模式视为不同（如 `/pools/:id` 与 `/pools/list` 不冲突），
    /// 与多数 web 框架行为一致。
    ///
    /// 实现：用 `pattern_set`（HashSet）做 O(1) 冲突检测，替代旧的线性扫描——
    /// 批量注册 N 条从 O(n²) 降到 O(n)。
    pub fn register(&mut self, spec: RouteSpec) -> Result<(), crate::ApiGatewayError> {
        // O(1) 冲突检测：pattern_key 把同结构模式（参数名不同）归并为同一键。
        let pkey = pattern_key(&spec.path);
        let key = (spec.method, pkey);
        if self.pattern_set.contains(&key) {
            return Err(crate::ApiGatewayError::RouteConflict(format!(
                "{} {:?}",
                spec.path, spec.method
            )));
        }
        self.pattern_set.insert(key);

        let idx = self.routes.len();
        // 静态路由（无参数段、无 wildcard）索引到 static_routes，O(1) 精确查找。
        if is_static(&spec.path) {
            // 同一 method 下静态 path 字符串唯一（pattern_set 已保证模式不冲突；
            // 静态模式的 pattern_key 就是 path 本身，故 path 也不重复）。
            self.static_routes
                .insert((spec.method, spec.path.clone()), idx);
        }
        // 按 method 分桶（线性扫描替换为按桶扫描）。
        self.by_method.entry(spec.method).or_default().push(idx);
        self.routes.push(spec);
        Ok(())
    }

    /// 已注册路由数。
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// 返回全部路由（克隆）。
    pub fn all(&self) -> &[RouteSpec] {
        &self.routes
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn static_routes_len(&self) -> usize {
        self.static_routes.len()
    }

    /// 匹配请求：在所有路由中找出 method 相同且 path 命中的最具体路由。
    ///
    /// 具体性：静态段数多的优先；若并列取注册顺序首个。
    /// 返回 `(匹配到的 RouteSpec 索引, 捕获的路径参数)`。
    ///
    /// 实现分两步（性能优化）：
    /// 1. **静态路径短路**：若请求路径命中 `static_routes`（即存在与请求路径完全相同
    ///    的静态路由），其 specificity 为该路径段的 2× 段数，是该 path 所能匹配的
    ///    最具体路由（任何参数/wildcard 路由若也匹配该 path，其 specificity 必更低），
    ///    故直接返回——O(1)。
    /// 2. **按 method 分桶扫描**：仅遍历请求 method 对应桶内的路由（参数/wildcard
    ///    及长度不匹配的静态路由仍走 match_path 逐条比较），通过 specificity 选最具体。
    ///    桶内通常远小于全量；即便方法维度无收益（单 method），语义与旧实现一致。
    pub fn match_request(&self, method: HttpMethod, path: &str) -> Option<(usize, PathParams)> {
        // 1) 静态路由 O(1) 精确查找短路。
        //    去 query 后查；静态 path 字面值不含参数，故键即规范化后的请求路径。
        let norm_path = path.split('?').next().unwrap_or(path);
        if let Some(&idx) = self.static_routes.get(&(method, norm_path.to_string())) {
            return Some((idx, PathParams::new()));
        }

        // 2) 按 method 分桶扫描（无桶说明该 method 无路由，直接 None）。
        let bucket = self.by_method.get(&method)?;
        let mut best: Option<(usize, PathParams, usize)> = None;
        for &idx in bucket {
            let r = &self.routes[idx];
            // 注：桶已按 method 过滤，无需再比 method。
            if let Some(params) = match_path(&r.path, path) {
                let specificity = specificity_of(&r.path);
                // PathParams 非 Copy，比较时用 as_ref 避免移动 best。
                let take = match best.as_ref() {
                    None => true,
                    Some((_, _, cur)) => specificity > *cur,
                };
                if take {
                    best = Some((idx, params, specificity));
                }
            }
        }
        best.map(|(idx, params, _)| (idx, params))
    }
}

/// 两个路径模式是否结构等价（按段类型序列比较）。
///
/// 注：register 的快速路径已用 `pattern_set`（基于 `pattern_key`）替代旧的线性扫描，
/// 此函数保留为语义参考与单测入口（`pattern_key` 是其可哈希的规范化形式，二者等价）。
#[cfg(test)]
fn same_pattern(a: &str, b: &str) -> bool {
    pattern_key(a) == pattern_key(b)
}

/// 把路径模式归一化为结构等价键：Literal 原样、`:name` 统一为 `:param`、`*` 保留。
///
/// 同结构的两个模式（如 `/a/:id` 与 `/a/:name`）产生相同键——用于 O(1) 冲突检测。
/// 与 `same_pattern` 等价但 O(段数) 单次计算、可哈希。
fn pattern_key(pattern: &str) -> String {
    // 统计是否含参数/wildcard 以快速判断静态路由（避免二次 parse）；这里直接生成键。
    let mut out = String::with_capacity(pattern.len());
    for seg in pattern.split('/') {
        if seg.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        if seg == "*" {
            out.push('*');
        } else if seg.starts_with(':') {
            // 参数名不影响结构等价，归一化为占位。
            out.push_str(":param");
        } else {
            out.push_str(seg);
        }
    }
    out
}

/// 路径模式是否为纯静态（无参数段、无 wildcard）。
///
/// 静态路由可被 `static_routes` HashMap O(1) 精确查找。
fn is_static(pattern: &str) -> bool {
    for seg in pattern.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "*" || seg.starts_with(':') {
            return false;
        }
    }
    true
}

/// 计算路径模式的具体性（静态段计 2 分，参数段计 1 分，通配 0 分；末尾越靠前权重不变）。
fn specificity_of(pattern: &str) -> usize {
    parse_pattern(pattern)
        .iter()
        .map(|s| match s {
            Segment::Literal(_) => 2,
            Segment::Param(_) => 1,
            Segment::Wildcard => 0,
        })
        .sum()
}

/// 便捷：从 `ApiRequest` 抽取 method+path 后调用 `match_request`。
impl RouteRegistry {
    /// 按完整请求匹配路由。
    pub fn match_api_request(&self, req: &ApiRequest) -> Option<(usize, PathParams)> {
        self.match_request(req.method, &req.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(method: HttpMethod, path: &str) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: "test".to_string(),
            requires_auth: false,
            required_roles: vec![],
        }
    }

    #[test]
    fn literal_exact_match() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/api/v1/pools")).unwrap();
        let (idx, params) = r.match_request(HttpMethod::Get, "/api/v1/pools").unwrap();
        assert_eq!(idx, 0);
        assert!(params.is_empty());
    }

    #[test]
    fn param_capture() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/api/v1/pools/:id"))
            .unwrap();
        let (idx, params) = r
            .match_request(HttpMethod::Get, "/api/v1/pools/tank")
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(params.get("id"), Some("tank"));
    }

    #[test]
    fn method_mismatch_no_match() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/api/v1/pools")).unwrap();
        assert!(r.match_request(HttpMethod::Post, "/api/v1/pools").is_none());
    }

    #[test]
    fn static_beats_param() {
        let mut r = RouteRegistry::new();
        // 参数路由先注册，静态后注册；匹配仍应选静态（更具体）
        r.register(spec(HttpMethod::Get, "/api/v1/pools/:id"))
            .unwrap();
        r.register(spec(HttpMethod::Get, "/api/v1/pools/list"))
            .unwrap();
        let (idx, params) = r
            .match_request(HttpMethod::Get, "/api/v1/pools/list")
            .unwrap();
        assert_eq!(idx, 1);
        assert!(params.is_empty());
    }

    #[test]
    fn conflict_rejected() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/api/v1/pools")).unwrap();
        let err = r
            .register(spec(HttpMethod::Get, "/api/v1/pools"))
            .unwrap_err();
        assert!(matches!(err, crate::ApiGatewayError::RouteConflict(_)));
    }

    #[test]
    fn param_and_static_not_conflict() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/api/v1/pools/:id"))
            .unwrap();
        r.register(spec(HttpMethod::Get, "/api/v1/pools/list"))
            .unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn same_pattern_structural_eq() {
        assert!(same_pattern("/a/:id", "/a/:name"));
        assert!(!same_pattern("/a/:id", "/a/b"));
        assert!(!same_pattern("/a/:id", "/a/:id/x"));
    }

    #[test]
    fn wildcard_match() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/static/*")).unwrap();
        assert!(r
            .match_request(HttpMethod::Get, "/static/css/app.css")
            .is_some());
        assert!(r.match_request(HttpMethod::Get, "/static").is_some());
    }

    #[test]
    fn query_stripped() {
        assert!(match_path("/api/v1/pools", "/api/v1/pools?x=1").is_some());
    }

    #[test]
    fn empty_paths() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/")).unwrap();
        assert!(r.match_request(HttpMethod::Get, "/").is_some());
    }

    #[test]
    fn multiple_params() {
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/:ns/:kind/:id")).unwrap();
        let (_, params) = r
            .match_request(HttpMethod::Get, "/storage/pools/tank")
            .unwrap();
        assert_eq!(params.get("ns"), Some("storage"));
        assert_eq!(params.get("kind"), Some("pools"));
        assert_eq!(params.get("id"), Some("tank"));
    }

    // —— PathParams API + match_path 边界（覆盖率补测）——

    #[test]
    fn path_params_len_is_empty_as_map() {
        let mut p = PathParams::new();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        p.insert("id", "tank");
        assert!(!p.is_empty());
        assert_eq!(p.len(), 1);
        assert_eq!(p.get("id"), Some("tank"));
        assert_eq!(p.get("missing"), None);
        // as_map 返回内部 HashMap 引用
        assert_eq!(p.as_map().get("id").map(|s| s.as_str()), Some("tank"));
    }

    #[test]
    fn match_path_wildcard_at_tail_with_empty_path_consumed() {
        // 末尾单独的 Wildcard 且路径已耗尽 → 匹配空（分支 142-144）
        assert!(match_path("/static/*", "/static").is_some());
        // Wildcard 在尾部消耗剩余多段
        assert!(match_path("/static/*", "/static/a/b/c").is_some());
    }

    #[test]
    fn match_path_returns_none_on_length_mismatch() {
        // 模式比路径长（无 wildcard）→ None
        assert!(match_path("/a/b/c", "/a/b").is_none());
        // 静态段不匹配
        assert!(match_path("/a/b", "/a/x").is_none());
    }

    #[test]
    fn registry_is_empty_and_static_routes_indexed() {
        let mut r = RouteRegistry::new();
        assert!(r.is_empty());
        r.register(spec(HttpMethod::Get, "/api/v1/pools")).unwrap();
        assert!(!r.is_empty());
        assert_eq!(r.len(), 1);
        // 静态路由应被收录进 static_routes 索引（O(1) 短路）
        assert_eq!(r.static_routes_len(), 1);
    }

    #[test]
    fn match_request_returns_none_for_method_without_routes() {
        // 该 method 无任何注册路由 → by_method 桶为 None
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/api/v1/pools")).unwrap();
        assert!(r
            .match_request(HttpMethod::Delete, "/api/v1/pools")
            .is_none());
    }

    #[test]
    fn match_request_param_best_among_bucket() {
        // 桶内多条参数路由，应按 specificity 选最具体
        let mut r = RouteRegistry::new();
        r.register(spec(HttpMethod::Get, "/:a/:b/:c")).unwrap(); // specificity=3
        r.register(spec(HttpMethod::Get, "/storage/:b/:c")).unwrap(); // specificity=5
        let (idx, _) = r
            .match_request(HttpMethod::Get, "/storage/pools/tank")
            .unwrap();
        assert_eq!(idx, 1, "更具体的静态段应胜出");
    }

    /// 性能冒烟测（默认不跑，`--ignored`）：验证 5000 路由下静态命中走 O(1) 短路
    /// （~ns 级）、参数命中仍线性扫描（~µs 级）。运行：
    /// `cargo test -p os-api --features mock perf_smoke --release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_smoke_5000() {
        let mut r = RouteRegistry::new();
        for i in 0..5000usize {
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
        // static_routes 收录 2/5 = 2000 条静态路由（list/detail）。
        assert_eq!(r.static_routes_len(), 2000);

        // 真正的静态命中：r4996 注册的是 /detail（i=4996 时 i%5==1）。
        let iters = 100_000;
        let t = std::time::Instant::now();
        let mut acc = 0u64;
        for _ in 0..iters {
            if let Some((idx, _)) = r.match_request(HttpMethod::Get, "/api/v1/r4996/detail") {
                acc += idx as u64;
            }
        }
        let static_ns = t.elapsed().as_nanos() as f64 / iters as f64;

        // 参数命中：r4998 注册的是 /:id（i=4998 时 i%5==3）。
        let t = std::time::Instant::now();
        let mut acc2 = 0u64;
        let iters2 = 1_000;
        for _ in 0..iters2 {
            if let Some((idx, _)) = r.match_request(HttpMethod::Get, "/api/v1/r4998/42") {
                acc2 += idx as u64;
            }
        }
        let param_ns = t.elapsed().as_nanos() as f64 / iters2 as f64;

        println!(
            "perf_smoke_5000: static_hit={static_ns:.0} ns/call (acc={acc}); \
             param_hit={param_ns:.0} ns/call (acc={acc2})"
        );
        // 静态命中应远快于参数命中（O(1) vs O(n)）。上限留足防 CI 抖动。
        assert!(static_ns < 500.0, "静态命中未走短路？static_ns={static_ns}");
        // 参数命中仍线性——仅作记录，不强断言具体值（依赖机器）。
        let _ = param_ns;
        let _ = acc;
        let _ = acc2;
    }
}
