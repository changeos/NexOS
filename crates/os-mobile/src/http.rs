//! HTTP 请求模型与纯逻辑工具（URL 构造 / 查询参数编码 / 响应解析）。
//!
//! 设计：reqwest 未在 workspace 注册（红线：不虚构依赖），故真实 HTTP 调用由
//! [`crate::HttpOsClient`] 以骨架 + TODO 形式承载。本模块抽出**与 HTTP
//! 客户端实现无关的纯逻辑**，使其可独立、确定性地单测：
//!
//! - [`RequestSpec`]：描述一次 HTTP 请求（method / path / 查询参数 / headers / body）。
//! - [`build_url`] / [`RequestSpec::build_url`]：base + path + 查询编码 → 完整 URL。
//! - [`encode_query`]：`&[(K, V)]` → `application/x-www-form-urlencoded` 风格的查询串
//!   （RFC 3986 percent-encode，空格编码为 `%20`，与 reqwest/url 行为一致）。
//! - [`parse_json_response`]：JSON bytes → typed `T`（封装 `serde_json::from_slice`，
//!   统一错误映射到 [`MobileError::EndpointUnreachable`] 语义的「响应体无效」）。
//!
//! 为什么单独成模块（而非埋进 HttpOsClient）：
//! - URL 构造 / 查询编码 / JSON 解析是无副作用的纯函数，提取后可被 mock 与真实实现
//!   共用，且单测无需起 HTTP mock server，确定性高。
//! - 桌面端（os-desktop）复用 os-mobile 的客户端契约时，同样复用本模块。

use std::borrow::Cow;
use std::collections::BTreeMap;

use os_core::Deserialize;

use crate::MobileError;

// ----------------------------------------------------------------------------
// HTTP 方法（轻量枚举，避免引入 http crate）
// ----------------------------------------------------------------------------

/// HTTP 方法（覆盖 OsClient 经网关调用所需的方法子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
}

impl HttpMethod {
    /// 转为标准 HTTP 方法大写字符串。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        }
    }
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ----------------------------------------------------------------------------
// RequestSpec
// ----------------------------------------------------------------------------

/// 一次 HTTP 请求的规格（method / path / 查询参数 / headers / body）。
///
/// 不含 base URL——base 由 [`crate::HttpOsClient`] 持有，
/// `RequestSpec` 只描述「相对网关根的请求形状」，便于复用与测试。
///
/// 查询参数与 headers 用 `BTreeMap` 以保证序列化/拼 URL 时的**确定性顺序**
/// （单测可稳定断言输出，避免 HashMap 乱序）。
#[derive(Debug, Clone)]
pub struct RequestSpec {
    /// HTTP 方法
    pub method: HttpMethod,
    /// 相对路径（如 `"/status"` / `"/discover/nodes"`）。须以 `/` 起始。
    pub path: String,
    /// 查询参数（key → value；按 key 字典序拼接到 URL）
    pub query: BTreeMap<String, String>,
    /// 请求头（key → value；按 key 字典序，便于断言）
    pub headers: BTreeMap<String, String>,
    /// 请求体（None = 无 body；JSON 序列化由调用方完成）
    pub body: Option<Vec<u8>>,
}

impl RequestSpec {
    /// 构造一个 GET 请求（无查询参数 / 无 body）。
    #[must_use]
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            path: path.into(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: None,
        }
    }

    /// 构造一个 POST 请求，body 为 JSON 序列化后的字节。
    ///
    /// 失败（serde_json 序列化失败）返回 [`MobileError::Internal`]——理论上对已实现
    /// `Serialize` 的入参不应发生，按防御性返回。
    pub fn post_json<T: serde::Serialize>(
        path: impl Into<String>,
        body: &T,
    ) -> Result<Self, MobileError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| MobileError::Internal(format!("请求体序列化失败: {e}")))?;
        Ok(Self {
            method: HttpMethod::Post,
            path: path.into(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Some(bytes),
        })
    }

    /// 追加一个查询参数（builder 风格）。
    #[must_use]
    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.insert(key.into(), value.into());
        self
    }

    /// 追加一个请求头（builder 风格）。
    #[must_use]
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// 用给定 base URL 构造完整请求 URL（base + path + ?query）。
    ///
    /// 详见 [`build_url`]。base 尾部多余的 `/` 与 path 首部多余的 `/` 会被归一
    /// （恰好一个 `/` 分隔），避免拼出 `//`。
    #[must_use]
    pub fn build_url(&self, base_url: &str) -> String {
        build_url(base_url, &self.path, &self.query)
    }
}

// ----------------------------------------------------------------------------
// URL 构造 / 查询编码
// ----------------------------------------------------------------------------

/// 把 `base_url` + `path` + `query` 拼成完整 URL。
///
/// 规则：
/// - base 尾部 `/` 与 path 首部 `/` 归一为恰好一个 `/`。
/// - path 为空时 URL 即 base（去掉尾部 `/` 后）。
/// - query 为空时不追加 `?`；非空时按 key 字典序（BTreeMap 天然有序）拼接，
///   key/value 经 [`percent_encode`] 编码，用 `&` 连接。
///
/// # 示例
/// ```
/// use os_mobile::http::build_url;
/// use std::collections::BTreeMap;
/// let mut q = BTreeMap::new();
/// q.insert("k".to_string(), "v".to_string());
/// assert_eq!(build_url("https://os/", "/status", &q), "https://os/status?k=v");
/// ```
#[must_use]
pub fn build_url(base_url: &str, path: &str, query: &BTreeMap<String, String>) -> String {
    let trimmed_base = base_url.trim_end_matches('/');
    let url = if path.is_empty() {
        trimmed_base.to_string()
    } else {
        // path 至少 1 个前导 '/'；去掉 base 末尾的 '/' 后直接拼 path（path 自带前导 /）
        let trimmed_path = path.trim_start_matches('/');
        format!("{trimmed_base}/{trimmed_path}")
    };

    if query.is_empty() {
        return url;
    }

    let qs = encode_query(query);
    format!("{url}?{qs}")
}

/// 把 `&[(key, value)]` 形式的查询参数编码为查询串（不含前导 `?`）。
///
/// - 参数顺序：迭代器顺序（调用方控制）；若需确定性顺序，用 [`encode_query`]（BTreeMap，按 key 字典序）。
/// - key 与 value 均经 [`percent_encode`]；键值对间用 `&` 连接，键值间用 `=`.
/// - key 为空的项被跳过（避免产生 `=v` 这种畸形对）。
#[must_use]
pub fn encode_query_pairs<'a, I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str> + 'a,
    V: AsRef<str> + 'a,
{
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in pairs {
        let k = k.as_ref();
        if k.is_empty() {
            continue;
        }
        parts.push(format!(
            "{}={}",
            percent_encode(k),
            percent_encode(v.as_ref())
        ));
    }
    parts.join("&")
}

/// 把 BTreeMap 形式的查询参数编码为查询串（按 key 字典序，不含前导 `?`）。
#[must_use]
pub fn encode_query(query: &BTreeMap<String, String>) -> String {
    encode_query_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

/// RFC 3986 百分号编码（unreserved = `A-Za-z0-9-._~`，其余全部 `%HH` 大写）。
///
/// 与 `url`/`reqwest` crate 的 `application/x-www-form-urlencoded` 行为对齐：
/// **空格编码为 `%20`**（非 `+`），保留字符一律编码，确保服务端按 query 解析无误。
#[must_use]
pub fn percent_encode(input: &str) -> Cow<'_, str> {
    /// unreserved 集合（RFC 3986）：ALPHA / DIGIT / "-" / "." / "_" / "~"
    fn is_unreserved(b: u8) -> bool {
        matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~')
    }

    // 快速路径：若全部字节都是 unreserved，直接返回借用，零分配。
    if input.bytes().all(is_unreserved) {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Cow::Owned(out)
}

// ----------------------------------------------------------------------------
// 响应解析
// ----------------------------------------------------------------------------

/// JSON 响应体（反序列化后的通用形态）——给 [`parse_json_response`] 的输入。
///
/// 真实 HTTP 客户端拿到的是字节；这里抽出一层「JSON → typed」的纯解析，
/// 便于在不用起 HTTP server 的情况下单测「响应体 → SystemStatus」映射。
#[derive(Debug, Clone)]
pub struct JsonResponse {
    /// HTTP 状态码（用于错误映射决策；本纯函数不强制校验，由调用方决定）
    pub status: u16,
    /// 响应体字节（JSON 文本）
    pub body: Vec<u8>,
}

impl JsonResponse {
    /// 用状态码 + 字节构造。
    #[must_use]
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self { status, body }
    }

    /// 是否为 2xx 成功状态。
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 把 JSON 响应体解析为 `T: Deserialize`。
///
/// 纯函数：不校验状态码（由调用方 [`crate::HttpOsClient`]
/// 先判 `is_success` 再解析），仅做 `serde_json::from_slice`，失败映射到
/// [`MobileError::EndpointUnreachable`]（语义：收到了响应但解析失败，等价于不可用）。
pub fn parse_json_response<T: for<'de> Deserialize<'de>>(
    resp: &JsonResponse,
) -> Result<T, MobileError> {
    serde_json::from_slice(&resp.body)
        .map_err(|e| MobileError::EndpointUnreachable(format!("响应解析失败: {e}")))
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // —— HttpMethod ——

    #[test]
    fn http_method_as_str() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
        assert_eq!(HttpMethod::Post.as_str(), "POST");
        assert_eq!(HttpMethod::Put.as_str(), "PUT");
        assert_eq!(HttpMethod::Delete.as_str(), "DELETE");
        assert_eq!(format!("{}", HttpMethod::Post), "POST");
    }

    // —— percent_encode ——

    #[test]
    fn percent_encode_unreserved_untouched() {
        assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
        // 全 unreserved → 借用，零分配
        assert!(matches!(percent_encode("abc"), Cow::Borrowed(_)));
    }

    #[test]
    fn percent_encode_space_is_percent_20() {
        // 空格编码为 %20（非 +），与 url/reqwest 一致
        assert_eq!(percent_encode("a b"), "a%20b");
    }

    #[test]
    fn percent_encode_special_chars() {
        assert_eq!(percent_encode("k=v&x"), "k%3Dv%26x");
        assert_eq!(percent_encode("/path"), "%2Fpath");
        assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(percent_encode(""), "");
    }

    #[test]
    fn percent_encode_uppercase_hex() {
        // hex 必须大写（:0x3A → %3A，非 %3a）
        assert_eq!(percent_encode(":"), "%3A");
    }

    // —— encode_query ——

    #[test]
    fn encode_query_empty() {
        let q = BTreeMap::new();
        assert_eq!(encode_query(&q), "");
    }

    #[test]
    fn encode_query_sorted_and_encoded() {
        let mut q = BTreeMap::new();
        q.insert("z".into(), "1".into());
        q.insert("a".into(), "x y".into());
        q.insert("m".into(), "k=v".into());
        // BTreeMap 按 key 字典序：a, m, z
        assert_eq!(encode_query(&q), "a=x%20y&m=k%3Dv&z=1");
    }

    #[test]
    fn encode_query_skips_empty_key() {
        let mut q = BTreeMap::new();
        q.insert(String::new(), "v".into());
        q.insert("k".into(), "v".into());
        assert_eq!(encode_query(&q), "k=v");
    }

    #[test]
    fn encode_query_pairs_preserves_order() {
        let pairs = vec![("z", "1"), ("a", "2"), ("m", "3")];
        // 非排序版本：保持入参顺序
        assert_eq!(encode_query_pairs(pairs), "z=1&a=2&m=3");
    }

    // —— build_url ——

    #[test]
    fn build_url_trims_trailing_slash_on_base() {
        let q = BTreeMap::new();
        assert_eq!(build_url("https://os/", "/status", &q), "https://os/status");
        assert_eq!(build_url("https://os", "/status", &q), "https://os/status");
    }

    #[test]
    fn build_url_trims_leading_slash_on_path() {
        let q = BTreeMap::new();
        // base 末尾 / + path 开头 // → 恰好一个 /
        assert_eq!(
            build_url("https://os/", "//status", &q),
            "https://os/status"
        );
    }

    #[test]
    fn build_url_empty_path_is_base() {
        let q = BTreeMap::new();
        assert_eq!(build_url("https://os/", "", &q), "https://os");
        assert_eq!(build_url("https://os", "", &q), "https://os");
    }

    #[test]
    fn build_url_appends_query_sorted() {
        let mut q = BTreeMap::new();
        q.insert("b".into(), "2".into());
        q.insert("a".into(), "1 2".into());
        assert_eq!(
            build_url("https://os", "/status", &q),
            "https://os/status?a=1%202&b=2"
        );
    }

    #[test]
    fn build_url_no_query_no_question_mark() {
        let q = BTreeMap::new();
        let url = build_url("https://os", "/status", &q);
        assert!(!url.contains('?'));
    }

    // —— RequestSpec ——

    #[test]
    fn request_spec_get_builder() {
        let r = RequestSpec::get("/status");
        assert_eq!(r.method, HttpMethod::Get);
        assert_eq!(r.path, "/status");
        assert!(r.query.is_empty());
        assert!(r.headers.is_empty());
        assert!(r.body.is_none());
    }

    #[test]
    fn request_spec_post_json_serializes_body() {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            code: &'a str,
        }
        let r = RequestSpec::post_json("/pair", &Body { code: "abc" }).unwrap();
        assert_eq!(r.method, HttpMethod::Post);
        let body = r.body.unwrap();
        assert_eq!(body, br#"{"code":"abc"}"#);
    }

    #[test]
    fn request_spec_with_query_and_header() {
        let r = RequestSpec::get("/status")
            .with_query("node", "1")
            .with_query("verbose", "true")
            .with_header("Authorization", "Bearer xyz");
        assert_eq!(r.query.get("node").unwrap(), "1");
        assert_eq!(r.query.get("verbose").unwrap(), "true");
        assert_eq!(r.headers.get("Authorization").unwrap(), "Bearer xyz");
    }

    #[test]
    fn request_spec_build_url_combines_all() {
        let r = RequestSpec::get("/discover/nodes").with_query("lan", "10.0.0.0/24");
        let url = r.build_url("https://os.example:8443/");
        assert_eq!(
            url,
            "https://os.example:8443/discover/nodes?lan=10.0.0.0%2F24"
        );
    }

    // —— JsonResponse / parse_json_response ——

    #[test]
    fn json_response_is_success() {
        assert!(JsonResponse::new(200, vec![]).is_success());
        assert!(JsonResponse::new(204, vec![]).is_success());
        assert!(JsonResponse::new(299, vec![]).is_success());
        assert!(!JsonResponse::new(301, vec![]).is_success());
        assert!(!JsonResponse::new(404, vec![]).is_success());
        assert!(!JsonResponse::new(500, vec![]).is_success());
    }

    #[test]
    fn parse_json_response_ok() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct S {
            hostname: String,
            node_count: u32,
        }
        let resp = JsonResponse::new(200, br#"{"hostname":"os","node_count":2}"#.to_vec());
        let s: S = parse_json_response(&resp).unwrap();
        assert_eq!(s.hostname, "os");
        assert_eq!(s.node_count, 2);
    }

    #[test]
    fn parse_json_response_invalid_maps_to_endpoint_unreachable() {
        let resp = JsonResponse::new(200, b"not json".to_vec());
        let r: Result<serde_json::Value, _> = parse_json_response(&resp);
        assert!(matches!(
            r.unwrap_err(),
            MobileError::EndpointUnreachable(_)
        ));
    }

    // —— 扩展边界（覆盖率补测）——

    #[test]
    fn http_method_display_covers_all_variants() {
        // 覆盖所有变体的 Display + as_str（Copy/Eq/Debug 派生也间接被覆盖）
        assert_eq!(format!("{}", HttpMethod::Get), "GET");
        assert_eq!(format!("{}", HttpMethod::Put), "PUT");
        assert_eq!(format!("{}", HttpMethod::Delete), "DELETE");
        // PartialEq/Eq
        assert_eq!(HttpMethod::Get, HttpMethod::Get);
        assert_ne!(HttpMethod::Get, HttpMethod::Post);
    }

    #[test]
    fn percent_encode_borrowed_for_all_unreserved() {
        // 全 unreserved 集合 → 借用路径
        let s = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
        assert!(matches!(percent_encode(s), Cow::Borrowed(_)));
        assert_eq!(percent_encode(s).as_ref(), s);
    }

    #[test]
    fn percent_encode_non_ascii_byte_uppercase() {
        // 验证多字节 UTF-8 → %HH（每字节大写十六进制）
        assert_eq!(percent_encode("ø"), "%C3%B8"); // U+00F8 → 0xC3 0xB8
        assert_eq!(percent_encode("\u{1F600}"), "%F0%9F%98%80"); // 😀
    }

    #[test]
    fn percent_encode_plus_and_tilde() {
        // '+' 与 '~' 区别：'+' 不是 unreserved → 编码；'~' 是 unreserved → 不编码
        assert_eq!(percent_encode("a+b"), "a%2Bb");
        assert_eq!(percent_encode("a~b"), "a~b");
    }

    #[test]
    fn percent_encode_regress_reserved_set() {
        // gen-delims / sub-delims（RFC 3986）均非 unreserved → 编码
        for c in [
            ':', '/', '?', '#', '[', ']', '@', '!', '$', '&', '\'', '(', ')', '*', ',', ';', '=',
        ] {
            let s = format!("a{c}b");
            let e = percent_encode(&s);
            assert_ne!(e.as_ref(), s, "char {c:?} 应被编码");
            assert!(e.contains(&format!("%{:02X}", c as u8)));
        }
    }

    #[test]
    fn encode_query_pairs_skips_empty_keys_and_values() {
        // 空键被跳过；空值保留为 k=
        let pairs = vec![("a", ""), ("b", "v"), ("", "x")];
        assert_eq!(encode_query_pairs(pairs), "a=&b=v");
    }

    #[test]
    fn encode_query_pairs_empty_iter() {
        let pairs: Vec<(&str, &str)> = vec![];
        assert_eq!(encode_query_pairs(pairs), "");
    }

    #[test]
    fn encode_query_pairs_all_empty_keys() {
        let pairs = vec![("", "1"), ("", "2")];
        assert_eq!(encode_query_pairs(pairs), "");
    }

    #[test]
    fn build_url_with_unicode_query_value() {
        let mut q = BTreeMap::new();
        q.insert("name".into(), "相册".into());
        // 中文 → %E5%85%A8...
        let url = build_url("https://os", "/shares", &q);
        assert!(url.starts_with("https://os/shares?name="));
        assert!(url.contains("%E7%9B%B8%E5%86%8C")); // 相册
    }

    #[test]
    fn build_url_with_empty_base_and_path() {
        let q = BTreeMap::new();
        // 空字符串 base + 空字符串 path → 空字符串
        assert_eq!(build_url("", "", &q), "");
    }

    #[test]
    fn build_url_with_multi_segment_path() {
        let q = BTreeMap::new();
        assert_eq!(build_url("https://os", "/a/b/c", &q), "https://os/a/b/c");
    }

    #[test]
    fn build_url_with_many_query_pairs_sorted() {
        let mut q = BTreeMap::new();
        q.insert("c".into(), "3".into());
        q.insert("a".into(), "1".into());
        q.insert("b".into(), "2".into());
        // BTreeMap 天然字典序：a, b, c
        assert_eq!(
            build_url("https://os", "/x", &q),
            "https://os/x?a=1&b=2&c=3"
        );
    }

    #[test]
    fn request_spec_debug_clone_and_modify_after_build() {
        let r1 = RequestSpec::get("/status").with_query("k", "v");
        let r2 = r1.clone();
        // Clone：副本与原件相等（字段层面）
        assert_eq!(r1.path, r2.path);
        assert_eq!(r1.method, r2.method);
        assert_eq!(r1.query, r2.query);
        // Debug：能格式化
        let _dbg = format!("{:?}", r1);
    }

    #[test]
    fn request_spec_post_json_multiple_fields() {
        #[derive(serde::Serialize)]
        struct Body {
            a: u32,
            b: String,
            c: Vec<i32>,
        }
        let r = RequestSpec::post_json(
            "/submit",
            &Body {
                a: 1,
                b: "x".into(),
                c: vec![1, 2, 3],
            },
        )
        .unwrap();
        let body = String::from_utf8(r.body.unwrap()).unwrap();
        assert!(body.contains("\"a\":1"));
        assert!(body.contains("\"b\":\"x\""));
        assert!(body.contains("\"c\":[1,2,3]"));
        assert_eq!(r.method, HttpMethod::Post);
        assert_eq!(r.path, "/submit");
    }

    #[test]
    fn request_spec_with_multiple_headers_and_queries() {
        let r = RequestSpec::get("/api")
            .with_query("q1", "1")
            .with_header("X-A", "a")
            .with_query("q2", "2")
            .with_header("X-B", "b");
        assert_eq!(r.query.len(), 2);
        assert_eq!(r.headers.len(), 2);
        // BTreeMap 字典序
        let keys: Vec<_> = r.headers.keys().collect();
        assert_eq!(keys, vec!["X-A", "X-B"]);
    }

    #[test]
    fn request_spec_build_url_with_headers_ignored_in_url() {
        // headers 不进 URL（仅 body 进 URL 的 query）
        let r = RequestSpec::get("/status").with_header("Authorization", "Bearer xyz");
        let url = r.build_url("https://os");
        assert_eq!(url, "https://os/status");
    }

    #[test]
    fn json_response_status_boundary() {
        // 边界：199 与 300 不算 success；200 与 299 算
        assert!(!JsonResponse::new(199, vec![]).is_success());
        assert!(JsonResponse::new(200, vec![]).is_success());
        assert!(JsonResponse::new(299, vec![]).is_success());
        assert!(!JsonResponse::new(300, vec![]).is_success());
    }

    #[test]
    fn json_response_body_roundtrip() {
        // 构造后 body 可读
        let r = JsonResponse::new(201, b"{\"ok\":true}".to_vec());
        assert_eq!(r.status, 201);
        assert!(r.is_success());
        assert_eq!(&r.body, b"{\"ok\":true}");
    }

    #[test]
    fn parse_json_response_complex_struct() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Resp {
            items: Vec<String>,
            total: u32,
            nested: Nested,
        }
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Nested {
            flag: bool,
            score: f64,
        }
        let resp = JsonResponse::new(
            200,
            br#"{"items":["a","b"],"total":2,"nested":{"flag":true,"score":1.5}}"#.to_vec(),
        );
        let r: Resp = parse_json_response(&resp).unwrap();
        assert_eq!(r.items, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(r.total, 2);
        assert!(r.nested.flag);
        assert_eq!(r.nested.score, 1.5);
    }

    #[test]
    fn parse_json_response_empty_body_errors() {
        let resp = JsonResponse::new(200, vec![]);
        let r: Result<serde_json::Value, _> = parse_json_response(&resp);
        assert!(r.is_err());
        assert!(matches!(
            r.unwrap_err(),
            MobileError::EndpointUnreachable(_)
        ));
    }

    #[test]
    fn parse_json_response_wrong_type_errors() {
        // 类型不匹配（期望 number，收到 string）
        let resp = JsonResponse::new(200, br#""not a number""#.to_vec());
        let r: Result<u32, _> = parse_json_response(&resp);
        assert!(r.is_err());
    }

    #[test]
    fn post_json_with_unit_struct_serializes_empty_object() {
        // unit struct 序列化为 null，不是 object —— 测实际行为
        #[derive(serde::Serialize)]
        struct Unit;
        let r = RequestSpec::post_json("/x", &Unit).unwrap();
        let body = String::from_utf8(r.body.unwrap()).unwrap();
        assert_eq!(body, "null");
    }
}
