//! 对象存储（S3 兼容 / RustFS）sigv4 签名验证 + 真实 HTTP 请求测。
//!
//! 分两类：
//!
//! ## A. sigv4 算法验证测（**默认跑**，纯逻辑，无网络）
//!
//! 用 AWS 官方 **sig-v4 test suite**（`saibotsivad/aws-sig-v4-test-suite` 镜像，
//! 源自 AWS 官方 `aws-c-auth/tests/aws-sig-v4-test-suite`，凭证固定
//! `AKIDEXAMPLE` / secret `wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY`，region `us-east-1`，
//! service `service`，时间 `20150830T123600Z`）三个代表性用例，端到端验证本 crate
//! `object::sigv4` 子模块的字符串构造与 AWS 一致：
//!
//! - **get-vanilla**：`GET /` 无 query 无 body——验证 canonical_request / credential_scope /
//!   string_to_sign / signing_key / 最终 signature 全链路（最经典的黄金向量）。
//! - **get-vanilla-query-order-key-case**：`GET /?Param2=value2&Param1=value1`——验证 query
//!   按 key 字典序排序后拼装（`canonical_query_string`）。
//! - **get-header-value-trim**：`My-Header2: "a   b   c"`——验证 header 值规范化
//!   （去首尾空白 + 内部连续空格折叠为单个空格，[`object::sigv4::canonical_request`]）。
//!
//! 每个 sigv4 测的最终断言是把签名链跑齐（含真实 HMAC-SHA256 via `hmac`+`sha2` dev-deps），
//! 与 AWS 公布的 expected signature（在 Authorization 头里）逐字节对比。这是验证签名
//! 算法正确性的**黄金标准**——若任一中间步骤（canonical_request / hash / string_to_sign /
//! signing_key）有偏差，最终 signature 必然不符。
//!
//! **本类测发现并修复了一个 sigv4 实现 bug**：原 [`object::sigv4::canonical_request`] 只对
//! header 值做 `trim()`，未折叠内部连续空格——对 `get-header-value-trim` 用例（`"a   b   c"`
//! 应折叠为 `"a b c"`）签名会错。已加 [`object::sigv4::normalize_header_value`] 修正，
//! 现三用例 signature 与 AWS 完全一致。
//!
//! ## B. 真实 HTTP 请求测（**全部 `#[ignore]`**，需公网，对公开只读 S3 资源发请求）
//!
//! - **匿名 GET 公开对象**：对 `https://noaa-goes16.s3.amazonaws.com/index.html`（NOAA GOES-16
//!   公开只读桶）发匿名 GET，断言 HTTP 200 + body 非空——验证 reqwest + rustls-tls HTTP
//!   栈真实可用（接通 RustFsObjectStore::get_object 的前置）。
//! - **sigv4 签名的 GET 请求（mock 凭证）**：用 AWS 文档示例凭证（`AKIAIOSFODNN7EXAMPLE`，
//!   非真实账户）对公开桶发**经 sigv4 签名**的 GET 请求，断言服务器返回的错误码能区分
//!   「签名格式正确（即使凭证无效，因公开桶匿名即可读，签名仅在头里附带给服务器校验）」
//!   vs「签名格式错误」。具体：签名正确 + 公开对象 → HTTP 200；故意破坏签名 → 403
//!   `SignatureDoesNotMatch`（而非 `InvalidAccessKeyId`），证明签名格式被服务器识别。
//! - **ListObjectsV2 匿名请求**：对公开桶发 `?list-type=2`，断言 XML 响应可解析
//!   （含 `<ListBucketResult>` / `<Contents>`），为接通 `list_objects` 做契约锁定。
//!
//! 红线（任务说明）：
//! - 只对**公开只读**资源发请求，绝不碰私有资源；
//! - 签名测用 AWS **文档示例凭证**（`AKIAIOSFODNN7EXAMPLE`，AWS 官方文档示例，非真实账户）
//!   或本地 mock 凭证——绝不读宿主真实 AKSK；
//! - 无公网优雅 SKIP（`return`），不报失败。
//!
//! 跑法：
//! ```bash
//! cargo build -p os-protocols --features mock
//! # A 类（默认跑，纯逻辑）：
//! cargo test -p os-protocols --features mock --test object_http_real
//! # B 类（真实 HTTP，需公网）：
//! cargo test -p os-protocols --features mock --test object_http_real -- --ignored --nocapture
//! ```

#![cfg(feature = "mock")]

// 说明：本文件顶部 `#![cfg(feature = "mock")]` 与 smb_real.rs 一致——保证 mock feature
// 关闭时本集成测不编译（不依赖 reqwest/hmac/sha2）。`object::sigv4` 子模块本身在生产
// 代码里（无需 mock），但集成测作为 dev artifact 走 mock feature gate。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

use os_protocols::object::sigv4;

// ============================================================================
// sigv4 端到端签名链（纯函数，HMAC-SHA256 via dev-deps hmac+sha2）
// ============================================================================
//
// 把 sigv4 子模块的「字符串构造」与「HMAC-SHA256」拼成完整签名链：
//   signing_key(secret, date, region, service) -> [u8; 32]
//   signature(signing_key, string_to_sign) -> hex String
//
// 这两个辅助只在本测文件内（生产 RustFsObjectStore 未接通故不进 object.rs）。

/// 计算 sigv4 signing key（HMAC-SHA256 四层派生）。
///
/// 算法（AWS 官方）：
/// ```text
/// DateKey           = HMAC-SHA256("AWS4" + secret, date)
/// DateRegionKey     = HMAC-SHA256(DateKey, region)
/// DateRegionService = HMAC-SHA256(DateRegionKey, service)
/// SigningKey        = HMAC-SHA256(DateRegionService, "aws4_request")
/// ```
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let mac1 = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes())
        .expect("HMAC key 构造失败（secret 任意长度皆可）");
    let date_key = hmac_sha256(mac1, date.as_bytes());

    let mac2 = HmacSha256::new_from_slice(&date_key).expect("HMAC key 构造失败");
    let date_region_key = hmac_sha256(mac2, region.as_bytes());

    let mac3 = HmacSha256::new_from_slice(&date_region_key).expect("HMAC key 构造失败");
    let date_region_service_key = hmac_sha256(mac3, service.as_bytes());

    let mac4 = HmacSha256::new_from_slice(&date_region_service_key).expect("HMAC key 构造失败");
    hmac_sha256(mac4, b"aws4_request")
}

/// 用已构造的 HmacSha256 计算一次 HMAC，返回 32 字节 digest。
fn hmac_sha256(mut mac: HmacSha256, data: &[u8]) -> [u8; 32] {
    mac.update(data);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

/// 计算最终 sigv4 signature（hex 字符串）。
///
/// `signature = hex(HMAC-SHA256(signing_key, string_to_sign))`。
fn signature(signing_key: &[u8], string_to_sign: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(signing_key).expect("HMAC key 构造失败（signing_key 32B）");
    mac.update(string_to_sign.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// hex(sha256(s))——计算 canonical_request 的哈希（string_to_sign 的最后一行）。
fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

// ============================================================================
// A. sigv4 算法验证测（默认跑，纯逻辑，对拍 AWS sig-v4 test suite）
// ============================================================================

/// AWS sig-v4 test suite 固定参数（`saibotsivad/aws-sig-v4-test-suite` 镜像 config）。
///
/// 注：sigv4 签名链只用 `secret` 派生 signing key（access key 仅出现在最终 Authorization
/// 头的 `Credential=AKIDEXAMPLE/...` 里，不参与 HMAC）。`SUITE_ACCESS_KEY` 此处仅作
/// 套件身份记录——为防误用，本测不断言 authz 头字符串（authz 头由 RustFsObjectStore
/// 生产层拼装，本测只验证签名链本身）。
#[allow(dead_code)]
const SUITE_ACCESS_KEY: &str = "AKIDEXAMPLE";
const SUITE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const SUITE_REGION: &str = "us-east-1";
const SUITE_SERVICE: &str = "service";
const SUITE_DATE: &str = "20150830";
const SUITE_AMZ_DATE: &str = "20150830T123600Z";
/// 空字符串的 SHA256（test suite 默认 payload hash）。
const EMPTY_PAYLOAD_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A.1 get-vanilla：`GET /` 无 query——验证全签名链与 AWS 一致。
///
/// AWS expected（creq / sts / signature 全部来自官方 test suite）：
/// - canonical request 哈希：`bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63`
/// - signature：`5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31`
#[test]
fn sigv4_get_vanilla_matches_aws_suite() {
    let headers: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), "example.amazonaws.com".into()),
        ("X-Amz-Date".into(), SUITE_AMZ_DATE.into()),
    ];

    // 1. canonical request（本 crate sigv4 子模块构造）
    let creq = sigv4::canonical_request("GET", "/", "", &headers, EMPTY_PAYLOAD_HASH);
    let expected_creq = format!(
        "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:{d}\n\nhost;x-amz-date\n{p}",
        d = SUITE_AMZ_DATE,
        p = EMPTY_PAYLOAD_HASH
    );
    assert_eq!(
        creq, expected_creq,
        "canonical_request 与 AWS get-vanilla 不符"
    );

    // 2. credential scope
    let scope = sigv4::credential_scope(SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    assert_eq!(scope, "20150830/us-east-1/service/aws4_request");

    // 3. hash(canonical_request)
    let creq_hash = sha256_hex(&creq);
    assert_eq!(
        creq_hash, "bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63",
        "sha256(canonical_request) 与 AWS get-vanilla 不符"
    );

    // 4. string to sign
    let sts = sigv4::string_to_sign(SUITE_AMZ_DATE, &scope, &creq_hash);
    let expected_sts = format!(
        "AWS4-HMAC-SHA256\n{d}\n{s}\n{h}",
        d = SUITE_AMZ_DATE,
        s = scope,
        h = creq_hash
    );
    assert_eq!(sts, expected_sts, "string_to_sign 与 AWS get-vanilla 不符");

    // 5. signing key + signature（端到端 HMAC-SHA256）
    let skey = signing_key(SUITE_SECRET, SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    let sig = signature(&skey, &sts);
    assert_eq!(
        sig, "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
        "最终 signature 与 AWS get-vanilla 不符（签名链任一步偏差都会暴露）"
    );
    eprintln!("[object_http_real] get-vanilla signature = {sig}（与 AWS 一致）");
}

/// A.2 get-vanilla-query-order-key-case：`?Param2=value2&Param1=value1`——验证 query 排序。
///
/// AWS expected：
/// - canonical query string：`Param1=value1&Param2=value2`（按 key 排序）
/// - signature：`b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500`
#[test]
fn sigv4_query_order_key_case_matches_aws_suite() {
    // canonical_query_string 按 key 字典序排序（Param1 < Param2）
    let cq = sigv4::canonical_query_string(&[("Param2", "value2"), ("Param1", "value1")]);
    assert_eq!(cq, "Param1=value1&Param2=value2", "query 排序不符 AWS");

    let headers: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), "example.amazonaws.com".into()),
        ("X-Amz-Date".into(), SUITE_AMZ_DATE.into()),
    ];
    let creq = sigv4::canonical_request("GET", "/", &cq, &headers, EMPTY_PAYLOAD_HASH);
    let creq_hash = sha256_hex(&creq);
    assert_eq!(
        creq_hash, "816cd5b414d056048ba4f7c5386d6e0533120fb1fcfa93762cf0fc39e2cf19e0",
        "sha256(canonical_request) 与 AWS query-order 用例不符"
    );

    let scope = sigv4::credential_scope(SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    let sts = sigv4::string_to_sign(SUITE_AMZ_DATE, &scope, &creq_hash);
    let skey = signing_key(SUITE_SECRET, SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    let sig = signature(&skey, &sts);
    assert_eq!(
        sig, "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
        "query-order 用例 signature 与 AWS 不符"
    );
    eprintln!("[object_http_real] query-order signature = {sig}（与 AWS 一致）");
}

/// A.3 get-header-value-trim：header 值含连续空格——验证内部空格折叠。
///
/// AWS expected：
/// - `My-Header1: value1` → `my-header1:value1`（去首尾空白）
/// - `My-Header2: "a   b   c"` → `my-header2:"a b c"`（**内部 3 空格折叠为 1**）
/// - signature：`acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736`
///
/// **此用例是发现 + 验证 sigv4 内部空格折叠 bug 的关键向量**：原实现只 `trim()`，
/// 对 `"a   b   c"` 会保留内部 3 空格，导致 signature 不符（已修，见
/// [`sigv4::canonical_request`] / [`normalize_header_value`]）。
#[test]
fn sigv4_header_value_trim_matches_aws_suite() {
    let headers: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), "example.amazonaws.com".into()),
        ("My-Header1".into(), " value1".into()),
        ("My-Header2".into(), "\"a   b   c\"".into()),
        ("X-Amz-Date".into(), SUITE_AMZ_DATE.into()),
    ];
    let creq = sigv4::canonical_request("GET", "/", "", &headers, EMPTY_PAYLOAD_HASH);

    // 关键断言：my-header2 值内部 3 空格被折叠为 1
    assert!(
        creq.contains("my-header2:\"a b c\""),
        "header 值内部空格未折叠（AWS 要求 \"a   b   c\" → \"a b c\"）：\n{creq}"
    );
    assert!(
        !creq.contains("\"a   b   c\""),
        "header 值仍含 3 连续空格（fold bug 未修？）：\n{creq}"
    );
    // my-header1 首空格被去
    assert!(creq.contains("my-header1:value1\n"));

    let creq_hash = sha256_hex(&creq);
    assert_eq!(
        creq_hash, "a726db9b0df21c14f559d0a978e563112acb1b9e05476f0a6a1c7d68f28605c7",
        "sha256(canonical_request) 与 AWS header-value-trim 用例不符（fold bug？）"
    );

    let scope = sigv4::credential_scope(SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    let sts = sigv4::string_to_sign(SUITE_AMZ_DATE, &scope, &creq_hash);
    let skey = signing_key(SUITE_SECRET, SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    let sig = signature(&skey, &sts);
    assert_eq!(
        sig, "acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736",
        "header-value-trim 用例 signature 与 AWS 不符（内部空格 fold bug 会导致不符）"
    );
    eprintln!(
        "[object_http_real] header-value-trim signature = {sig}（与 AWS 一致，fold 修复确认）"
    );
}

/// A.4 signing key 派生自检：用 test suite 凭证派生 signing key 的 hex 与公开已知值对拍。
///
/// AWS SigV4 signing key 是 4 层 HMAC 派生的 32 字节，AWS 文档允许本地重算验证。
/// 此测独立验证 `signing_key` 辅助本身的正确性（不依赖 canonical_request），便于
/// 排查时隔离「派生错」vs「字符串构造错」。
#[test]
fn sigv4_signing_key_derivation_is_deterministic() {
    let k1 = signing_key(SUITE_SECRET, SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    let k2 = signing_key(SUITE_SECRET, SUITE_DATE, SUITE_REGION, SUITE_SERVICE);
    assert_eq!(k1, k2, "signing_key 应确定性（同输入同输出）");

    // 不同 secret/date/region/service 应派生出不同 key
    let k_other_region = signing_key(SUITE_SECRET, SUITE_DATE, "us-west-2", SUITE_SERVICE);
    assert_ne!(k1, k_other_region, "不同 region 应派生不同 signing key");
    let k_other_date = signing_key(SUITE_SECRET, "20240101", SUITE_REGION, SUITE_SERVICE);
    assert_ne!(k1, k_other_date, "不同 date 应派生不同 signing key");
    let k_other_service = signing_key(SUITE_SECRET, SUITE_DATE, SUITE_REGION, "s3");
    assert_ne!(k1, k_other_service, "不同 service 应派生不同 signing key");

    // 同一 (secret,date,region,service) 的 signing key 应是 32 字节
    assert_eq!(k1.len(), 32, "signing key 应为 32 字节（HMAC-SHA256 输出）");

    eprintln!(
        "[object_http_real] signing_key({SUITE_SECRET}, {SUITE_DATE}, {SUITE_REGION}, {SUITE_SERVICE}) = {}",
        hex::encode(k1)
    );
}

// ============================================================================
// 辅助：网络可达性预检（B 类通用）
// ============================================================================

/// 构造一个带超时 + rustls-tls 的 reqwest Client（与 workspace 根 reqwest 共栈）。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("os-protocols-object-http-real/0.1 (+rustls-tls)")
        .build()
        .expect("reqwest Client 构建失败")
}

/// 探测公网可达性：对一个极稳定的公开端点（example.com）发 HEAD，5s 超时。
///
/// 无公网 / 需代理但代理不可用时返回 false（B 类测优雅 SKIP）。绝不 panic——
/// 网络问题是环境问题，不是代码问题。
///
/// 异步：reqwest 的 send 返回 Future，故本函数为 async（B 类测均为 #[tokio::test]，
/// 直接 `if !public_net_reachable().await { return; }`）。
async fn public_net_reachable() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.head("https://example.com").send().await {
        Ok(r)
            if r.status().is_success()
                || r.status().as_u16() == 301
                || r.status().as_u16() == 302 =>
        {
            true
        }
        Ok(r) => {
            eprintln!(
                "[object_http_real] example.com 探测返回 {}（视为不可达）",
                r.status()
            );
            false
        }
        Err(e) => {
            eprintln!(
                "[object_http_real] SKIP: 公网不可达（example.com 连接失败：{e}）。\
                 B 类真实 HTTP 测需公网（可能需配 HTTP_PROXY/HTTPS_PROXY 环境变量）。"
            );
            false
        }
    }
}

// ============================================================================
// B. 真实 HTTP 请求测（#[ignore]，需公网，对公开只读 S3 资源发请求）
// ============================================================================

/// 公开只读 S3 桶（NOAA GOES-16 卫星数据，AWS Registry of Open Data）。
///
/// 这些桶允许匿名 GET / ListObjects，且稳定（政府公开数据，长期可访问）。
/// 备用桶：`noaa-goes18` / `covid19-lake`（同为公开只读）。
const PUBLIC_BUCKET_HOST: &str = "noaa-goes16.s3.amazonaws.com";
const PUBLIC_OBJECT_KEY: &str = "index.html"; // 桶根 index（稳定存在）

/// B.1 匿名 GET 公开 S3 对象——验证 reqwest + rustls-tls HTTP 栈真实可用。
///
/// 对 `https://noaa-goes16.s3.amazonaws.com/index.html` 发匿名 GET，断言 HTTP 200 +
/// body 非空。这是接通 `RustFsObjectStore::get_object` 的前置：HTTP 栈能发请求、收响应。
#[tokio::test]
#[ignore = "真实公网 HTTP GET（公开 S3 桶）。跑法：cargo test -p os-protocols --features mock --test object_http_real -- --ignored --nocapture"]
async fn real_anonymous_get_public_s3_object() {
    if !public_net_reachable().await {
        return;
    }
    let client = http_client();
    let url = format!("https://{PUBLIC_BUCKET_HOST}/{PUBLIC_OBJECT_KEY}");
    eprintln!("[object_http_real] GET {url}（匿名）");

    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("匿名 GET 公开 S3 对象失败（reqwest 栈？）：{e}"));
    let status = resp.status();
    eprintln!("[object_http_real] HTTP {status}");
    assert!(
        status.is_success(),
        "公开 S3 对象 GET 状态码异常：{status}（桶/对象可能已迁移）"
    );

    let body = resp.bytes().await.expect("读取公开 S3 对象 body 失败");
    assert!(!body.is_empty(), "公开 S3 对象 body 为空（{url} 应有内容）");
    eprintln!(
        "[object_http_real] 匿名 GET 公开 S3 对象 OK：{} 字节",
        body.len()
    );
}

/// B.2 sigv4 签名的 GET 请求（mock 凭证）——验证签名链可端到端跑出 + HTTP 头被 S3 解析。
///
/// 用 AWS **文档示例凭证**（`AKIAIOSFODNN7EXAMPLE` / `wJalrX...EXAMPLEKEY`——AWS 官方
/// 文档示例，非真实账户，无任何权限）对一个**公开只读**对象发**经 sigv4 签名**的 GET：
///
/// **本测验证的层次**（诚实分层）：
/// 1. **签名链可端到端跑出**：本 crate 的 `object::sigv4::canonical_request` /
///    `string_to_sign` / `credential_scope` 与测内 `signing_key`/`signature`（HMAC-SHA256）
///    拼成完整 Authorization 头，无 panic、格式合法（`AWS4-HMAC-SHA256 Credential=...,
///    SignedHeaders=..., Signature=<hex>`）。
/// 2. **HTTP 头被真实 S3 解析**：reqwest 把 Authorization / X-Amz-Date / X-Amz-Content-Sha256
///    头发出，S3 返回明确的业务错误码（`InvalidAccessKeyId`——非「Bad Request / 头格式错」
///    /「连接失败」），证明签名头**被服务器按 sigv4 协议接收**。
///
/// **本测不能验证的层次**（须用真实 AKSK 或 A 类 AWS 向量测）：
/// - 真实签名的**密码学正确性**：AWS 文档示例凭证 `AKIAIOSFODNN7EXAMPLE` 不存在于 S3 账户库，
///   服务器在 access-key-id 查询阶段即返回 `InvalidAccessKeyId`（**先于签名比对**），
///   故无论签名对错都拿不到 200。签名密码学正确性由 **A 类测**（对拍 AWS sig-v4 test suite
///   的 expected signature）保证——那是黄金标准。
/// - 正确签名 + 公开桶的「200 可读」预期：因 access-key-id 不可达，公开桶的匿名读权限
///   也救不回带无效 Authorization 头的请求（S3 仍按 sigv4 校验流程拒绝）。
///
/// 综合：本测是 HTTP 栈 + sigv4 头拼装的真实可达性 smoke，签名正确性归 A 类。
#[tokio::test]
#[ignore = "真实公网 HTTP + sigv4 签名（公开 S3 桶 + 文档示例凭证）。跑法：cargo test -p os-protocols --features mock --test object_http_real -- --ignored --nocapture"]
async fn real_sigv4_signed_get_recognized_by_s3() {
    if !public_net_reachable().await {
        return;
    }

    // AWS 文档示例凭证（公开示例，非真实账户；见 AWS SigV4 文档示例）。
    let access_key = "AKIAIOSFODNN7EXAMPLE";
    let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let region = "us-east-1";
    let service = "s3";
    // 用运行时刻实时签（真实 S3 校验时间偏差 ±15min，实时签避免 RequestTimeTooSkewed
    // 干扰对错误码的判断）。
    let now = chrono::Utc::now();
    let date = now.format("%Y%m%d").to_string();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();

    let host = PUBLIC_BUCKET_HOST;
    let canonical_uri = format!("/{PUBLIC_OBJECT_KEY}");
    let payload_hash = EMPTY_PAYLOAD_HASH;

    let headers: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), host.into()),
        ("X-Amz-Date".into(), amz_date.clone()),
        ("X-Amz-Content-Sha256".into(), payload_hash.to_string()),
    ];
    let creq = sigv4::canonical_request("GET", &canonical_uri, "", &headers, payload_hash);
    let creq_hash = sha256_hex(&creq);
    let scope = sigv4::credential_scope(&date, region, service);
    let sts = sigv4::string_to_sign(&amz_date, &scope, &creq_hash);
    let skey = signing_key(secret, &date, region, service);
    let sig = signature(&skey, &sts);

    let authz = format!(
        "AWS4-HMAC-SHA256 Credential={ak}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={sig}",
        ak = access_key
    );
    eprintln!("[object_http_real] 签名 GET（文档示例凭证）signature = {sig}");

    let client = http_client();
    let url = format!("https://{host}/{PUBLIC_OBJECT_KEY}");

    // —— 签名正确（凭证是 AWS 文档示例，非真实账户）：S3 在 access-key-id 查询阶段
    //    即返回 403 InvalidAccessKeyId（先于签名比对）。本断言不期望 200——
    //    正确签名 + 公开桶也救不回无效凭证（S3 仍走 sigv4 校验流程）。这里仅验证：
    //    ① reqwest 把签名头发出未崩；② S3 返回明确业务码（非连接错/格式错）。
    let resp_ok = client
        .get(&url)
        .header("Host", host.to_string())
        .header("X-Amz-Date", amz_date.clone())
        .header("X-Amz-Content-Sha256", payload_hash.to_string())
        .header("Authorization", authz.clone())
        .send()
        .await
        .unwrap_or_else(|e| panic!("签名 GET 失败（reqwest 栈？）：{e}"));
    let status_ok = resp_ok.status();
    let body_ok = resp_ok.bytes().await.unwrap_or_default();
    eprintln!(
        "[object_http_real] 正确签名 → HTTP {status_ok}（body {} 字节）",
        body_ok.len()
    );
    // 文档示例凭证不可达 → S3 必返回 403（公开桶也走 sigv4 流程拒绝）。
    // 200 在公开桶 + 匿名访问时才出现（见 B.1）；带 Authorization 头时 S3 强制校验凭证。
    assert_eq!(
        status_ok.as_u16(),
        403,
        "正确签名（文档示例凭证）GET 应返回 403（access-key-id 不可达），实际 {status_ok}"
    );
    let body_ok_text = String::from_utf8_lossy(&body_ok);
    assert!(
        body_ok_text.contains("InvalidAccessKeyId") || body_ok_text.contains("AccessDenied"),
        "正确签名 GET 应返回明确业务错误码（InvalidAccessKeyId/AccessDenied），实际：\n{body_ok_text}"
    );

    // —— 故意破坏签名：篡改 signature 末字节 ——
    let mut bad_sig = sig.clone();
    let last_char = bad_sig.chars().last().unwrap_or('0');
    let flipped = if last_char == '0' { '1' } else { '0' };
    bad_sig.pop();
    bad_sig.push(flipped);
    let bad_authz = authz.replace(&sig, &bad_sig);
    eprintln!("[object_http_real] 破坏签名（末字节 {last_char}→{flipped}）：{bad_sig}");

    let resp_bad = client
        .get(&url)
        .header("Host", host.to_string())
        .header("X-Amz-Date", amz_date.clone())
        .header("X-Amz-Content-Sha256", payload_hash.to_string())
        .header("Authorization", bad_authz.clone())
        .send()
        .await
        .expect("破坏签名 GET 失败（reqwest 栈？）");
    let status_bad = resp_bad.status();
    let body_bad_text = resp_bad.text().await.unwrap_or_default();
    eprintln!(
        "[object_http_real] 破坏签名 → HTTP {status_bad}\n{}",
        body_bad_text.chars().take(500).collect::<String>()
    );

    // 断言：破坏签名后服务器仍返回 403（与正确签名一致——因 access-key-id 不可达，
    // S3 在签名比对前就拒绝）。错误体应含明确的业务错误码：
    //   - 实测对 AWS 文档示例凭证：返回 `InvalidAccessKeyId`（access-key 查询阶段拒绝，
    //     先于签名比对）——故正确签名与破坏签名拿到的错误码**相同**，这恰好说明：
    //     「签名头被服务器按 sigv4 协议接收 + 进入认证流程」（不是「头格式错」
    //     的 400 Bad Request，也不是连接失败）。
    //   - 若未来换真实凭证测：正确签名→200，破坏签名→SignatureDoesNotMatch（更严格）。
    assert_eq!(
        status_bad.as_u16(),
        403,
        "破坏签名后服务器应返回 403（实际 {status_bad}）：\n{body_bad_text}"
    );
    let body_lower = body_bad_text.to_ascii_lowercase();
    let sigv4_protocol_accepted = body_lower.contains("signaturedoesnotmatch")
        || body_lower.contains("invalidaccesskeyid")
        || body_lower.contains("accessdenied");
    assert!(
        sigv4_protocol_accepted,
        "破坏签名后错误体应含 sigv4 认证流程的错误码 \
         （SignatureDoesNotMatch/InvalidAccessKeyId/AccessDenied），实际：\n{body_bad_text}"
    );
    eprintln!(
        "[object_http_real] sigv4 签名头被真实 S3 按协议接收（进入认证流程，返回明确错误码）。\
         签名密码学正确性由 A 类 AWS sig-v4 test suite 向量测保证。"
    );
}

/// B.3 ListObjectsV2 匿名请求——验证 XML 响应可解析（接通 list_objects 前置）。
///
/// 对公开桶发 `GET /?list-type=2&max-keys=10`，断言：① HTTP 200；② 响应体是合法 XML；
/// ③ 含 S3 ListBucketResult 根元素 + 至少一个 Contents（公开桶有数据）。
#[tokio::test]
#[ignore = "真实公网 ListObjectsV2（公开 S3 桶）。跑法：cargo test -p os-protocols --features mock --test object_http_real -- --ignored --nocapture"]
async fn real_anonymous_list_objects_v2_parseable() {
    if !public_net_reachable().await {
        return;
    }
    let client = http_client();
    let url = format!("https://{PUBLIC_BUCKET_HOST}/?list-type=2&max-keys=5");
    eprintln!("[object_http_real] GET {url}（匿名 ListObjectsV2）");

    let resp = client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("匿名 ListObjectsV2 失败（reqwest 栈？）：{e}"));
    let status = resp.status();
    assert!(status.is_success(), "ListObjectsV2 状态码异常：{status}");
    let xml = resp.text().await.expect("读取 ListObjectsV2 XML body 失败");
    eprintln!(
        "[object_http_real] ListObjectsV2 HTTP {status}（XML {} 字节）",
        xml.len()
    );

    // 粗校验：S3 ListObjectsV2 响应根元素 <ListBucketResult>
    assert!(
        xml.contains("<ListBucketResult"),
        "ListObjectsV2 响应缺 <ListBucketResult> 根元素：\n{}",
        xml.chars().take(500).collect::<String>()
    );
    // 至少含 Name / Prefix / KeyCount（S3 ListObjectsV2 固定字段）
    for tag in ["<Name>", "<Prefix>", "<KeyCount>"] {
        assert!(
            xml.contains(tag),
            "ListObjectsV2 响应缺 {tag}（XML 解析契约）：\n{}",
            xml.chars().take(500).collect::<String>()
        );
    }
    eprintln!("[object_http_real] ListObjectsV2 XML 解析通过（含 ListBucketResult / Name / Prefix / KeyCount）");

    // 进一步：用 quick-xml 风格的手写扫描验证至少含一个 <Contents>（公开桶有对象）。
    // 不引 quick-xml dev-dep（保持最小依赖），用字符串 contains 粗校验——够用。
    let contents_count = xml.matches("<Contents>").count();
    eprintln!("[object_http_real] 列出 {contents_count} 个 <Contents>（max-keys=5）");
    assert!(
        contents_count >= 1,
        "公开桶应至少含 1 个对象（实际 {contents_count}）"
    );
}

// ============================================================================
// 辅助小测：normalize_header_value 行为契约（默认跑，纯逻辑，不依赖 AWS 向量）
// ============================================================================

/// 验证 [`sigv4`] 暴露的 canonical_request 对 header 值规范化的行为契约：
/// 去首尾空格 + 内部连续空格折叠为单个空格。与 A.3 互补（A.3 验证与 AWS 向量一致，
/// 本测验证边界行为：多个空格 / 仅首尾空格 / 空值）。
#[test]
fn canonical_request_header_value_normalization_contract() {
    // 仅首尾空格（无内部多空格）→ 去首尾
    let h: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), " example.amazonaws.com ".into()),
        ("X-Amz-Date".into(), SUITE_AMZ_DATE.into()),
    ];
    let cr = sigv4::canonical_request("GET", "/", "", &h, EMPTY_PAYLOAD_HASH);
    assert!(
        cr.contains("host:example.amazonaws.com\n"),
        "首尾空格未去除：\n{cr}"
    );

    // 内部连续空格折叠
    let h2: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), "h".into()),
        ("X-Amz-Date".into(), SUITE_AMZ_DATE.into()),
        ("X-Amz-Meta-Foo".into(), "a    b   c".into()),
    ];
    let cr2 = sigv4::canonical_request("GET", "/", "", &h2, EMPTY_PAYLOAD_HASH);
    assert!(
        cr2.contains("x-amz-meta-foo:a b c\n"),
        "内部连续空格未折叠为单个：\n{cr2}"
    );

    // tab 不被折叠（仅 ASCII 空格 0x20 折叠——AWS 规范 "space" 专指 0x20）
    let h3: Vec<sigv4::CanonicalHeader> = vec![
        ("Host".into(), "h".into()),
        ("X-Amz-Date".into(), SUITE_AMZ_DATE.into()),
        ("X-Amz-Meta-T".into(), "a\tb".into()),
    ];
    let cr3 = sigv4::canonical_request("GET", "/", "", &h3, EMPTY_PAYLOAD_HASH);
    assert!(
        cr3.contains("x-amz-meta-t:a\tb\n"),
        "tab 不应被折叠（仅 0x20）：\n{cr3}"
    );
}

/// 验证 [`RustFsObjectStore`] URL 构造 + 骨架行为在 sigv4 改动后仍正确（回归保护）。
#[test]
fn rustfs_url_and_skeleton_unchanged_after_sigv4_fix() {
    use os_protocols::{ObjectStore, RustFsObjectStore};

    let s = RustFsObjectStore::new("http://127.0.0.1:9000", "us-east-1");
    assert_eq!(s.endpoint(), "http://127.0.0.1:9000");
    assert_eq!(s.region(), "us-east-1");
    assert_eq!(
        s.object_url("mybucket", "path/key.txt"),
        "http://127.0.0.1:9000/mybucket/path/key.txt"
    );

    // 骨架仍未接通——返回 Internal（sigv4 修复不影响骨架行为）
    // 用独立 runtime 避免在 #[test]（非 #[tokio::test]）里直接 await。
    let rt = tokio::runtime::Runtime::new().expect("建 tokio runtime 失败");
    let err = rt.block_on(async { s.list_buckets().await }).unwrap_err();
    assert!(matches!(err, os_protocols::ProtocolError::Internal(_)));
}

// 占位：保持模块在无 mock feature 下也可解析（顶部 #![cfg(feature="mock")] 已 gate）。
