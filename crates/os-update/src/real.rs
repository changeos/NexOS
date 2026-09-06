//! 真实 I/O 实现——签名校验 / sha256 摘要 / 更新包下载 / CVE 解析。
//!
//! 本模块把"接通真实库"的逻辑集中放置，便于 impls.rs 复用与单测：
//! - [`verify_package`]：ed25519 验签 + sha256 摘要比对（安全关键，不可绕过）。
//! - [`sha256_hex`]：计算文件 sha256（小写 hex）。
//! - [`download_to_file`]：reqwest 单次下载到指定路径（断点续传留 TODO）。
//! - [`parse_osv_advisories`]：解析 OSV API 响应，过滤受监控组件。
//!
//! 安全红线：[`verify_package`] 一律真实校验签名与摘要，**不提供任何绕过路径**；
//! 任一失败即返回 [`crate::UpdateError::VerificationFailed`]。

use std::path::Path;

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::update::UpdateManifest;
use crate::{CveAdvisory, CveSeverity, UpdateError};

// ============================================================================
// sha256 摘要
// ============================================================================

/// 计算文件的 SHA-256 摘要，返回小写 hex 字符串。
///
/// 失败（文件不存在/读失败）映射 [`UpdateError::VerificationFailed`]——
/// 校验阶段任何 I/O 异常都视为校验未通过（fail-closed）。
pub fn sha256_hex(path: &Path) -> Result<String, UpdateError> {
    let bytes = std::fs::read(path)
        .map_err(|e| UpdateError::VerificationFailed(format!("读取下载文件失败: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

/// 计算内存字节的 SHA-256 摘要（小写 hex）。供测试与下载后立即校验复用。
pub fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ============================================================================
// ed25519 验签 + sha256 比对
// ============================================================================

/// 校验已下载更新包：sha256 摘要比对 + ed25519 签名验签。
///
/// 校验顺序（fail-closed，任一失败立即返回 `VerificationFailed`）：
/// 1. 文件 sha256 == `manifest.sha256`（小写 hex 比对，大小写不敏感）
/// 2. ed25519 公钥验签：签名覆盖**整个 manifest 的 sha256 摘要**（32 字节），
///    即签名针对下载内容指纹而非内容本身——便于离线预签名。
///
/// # 参数
/// - `manifest`：更新清单（含期望 sha256 + Base64 签名）
/// - `downloaded_path`：已下载文件路径
/// - `pubkey`：可信 ed25519 公钥（32 字节）
///
/// # 返回
/// - `Ok(true)`：sha256 匹配且签名有效
/// - `Err(VerificationFailed)`：任一校验失败（含解码/I/O 错误，fail-closed）
///
/// 安全：本函数不可绕过签名校验；公钥由调用方注入（系统构建期烧录可信根公钥）。
pub fn verify_package(
    manifest: &UpdateManifest,
    downloaded_path: &Path,
    pubkey: &[u8; 32],
) -> Result<bool, UpdateError> {
    // 1) sha256 摘要比对
    let actual = sha256_hex(downloaded_path)?;
    if !eq_hex_ci(&actual, &manifest.sha256) {
        return Err(UpdateError::VerificationFailed(format!(
            "sha256 不匹配：期望 {}，实际 {actual}",
            manifest.sha256
        )));
    }
    // 2) ed25519 验签：签名内容 = 文件 sha256 的原始 32 字节摘要
    let digest_bytes = sha256_raw(downloaded_path)?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&manifest.signature)
        .map_err(|e| UpdateError::VerificationFailed(format!("签名 Base64 解码失败: {e}")))?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|e| UpdateError::VerificationFailed(format!("签名反序列化失败: {e}")))?;
    let vk = VerifyingKey::from_bytes(pubkey)
        .map_err(|e| UpdateError::VerificationFailed(format!("公钥非法: {e}")))?;
    vk.verify(&digest_bytes, &sig)
        .map_err(|e| UpdateError::VerificationFailed(format!("ed25519 验签失败: {e}")))?;
    Ok(true)
}

/// 计算文件 sha256 的原始 32 字节摘要（供验签覆盖内容）。
fn sha256_raw(path: &Path) -> Result<[u8; 32], UpdateError> {
    let bytes = std::fs::read(path)
        .map_err(|e| UpdateError::VerificationFailed(format!("读取下载文件失败: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// 大小写不敏感的 hex 串比对（manifest.sha256 可能大写，统一小写比较）。
fn eq_hex_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ============================================================================
// reqwest 下载
// ============================================================================

/// 用 reqwest 下载 `url` 内容并写入 `dest`，返回写入字节数。
///
/// 当前为单次完整下载（GET → bytes → 写盘）。断点续传（Range/分块）留 TODO，
/// 待 ostree/分块依赖注册后扩展；基础下载路径已真实可用。
///
/// 错误映射 [`UpdateError::DownloadFailed`]。
pub async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<u64, UpdateError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpdateError::DownloadFailed(format!("请求失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(UpdateError::DownloadFailed(format!(
            "源返回 HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| UpdateError::DownloadFailed(format!("读取响应体失败: {e}")))?;
    let n = bytes.len() as u64;
    // 写入临时文件再原子改名，避免半写文件被误校验通过
    let tmp = dest.with_extension("partial");
    std::fs::write(&tmp, &bytes)
        .map_err(|e| UpdateError::DownloadFailed(format!("写盘失败: {e}")))?;
    std::fs::rename(&tmp, dest)
        .map_err(|e| UpdateError::DownloadFailed(format!("原子改名失败: {e}")))?;
    // TODO(断点续传)：支持 Range 请求 + 已下载字节续传，提升大包鲁棒性。
    Ok(n)
}

// ============================================================================
// CVE（OSV）解析
// ============================================================================

/// OSV API 单条漏洞条目（仅取本系统关心的字段）。
///
/// OSV schema：<https://docs.google.com/document/d/1sUyYW3sS1m_q9zGlf7QeWS0rMAdDcDQcUvB8sXL5DgM/edit>
/// 这里宽松解析：缺失字段不致命，跳过该条。
#[derive(Debug, serde::Deserialize)]
struct OsvVulnerability {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    affected: Vec<OsvAffected>,
    #[serde(default)]
    published: String,
}

#[derive(Debug, serde::Deserialize)]
struct OsvAffected {
    package: Option<OsvPackage>,
    #[serde(default)]
    ranges: Vec<OsvRange>,
}

#[derive(Debug, serde::Deserialize)]
struct OsvPackage {
    name: String,
}

#[derive(Debug, serde::Deserialize)]
struct OsvRange {
    /// OSV range events 形如 `[{"type":"introduced","value":"0"},{"type":"fixed","value":"1.2"}]`，
    /// 字段异质（部分无 value），统一以 `serde_json::Value` 收集后手动挑出 `fixed`，
    /// 避免严格枚举在边界 schema 上反序列化失败。
    #[serde(default)]
    events: Vec<serde_json::Value>,
}

/// OSV `/query` 批量响应（`vulns` 数组）。
#[derive(Debug, serde::Deserialize)]
pub(crate) struct OsvResponse {
    #[serde(default)]
    vulns: Vec<OsvVulnerability>,
}

/// 把 OSV 响应解析为 [`CveAdvisory`] 列表，过滤 `watched_components` 命中项。
///
/// - CVE id：优先 `aliases` 中以 `CVE-` 开头条目，否则用 OSV `id`
/// - affected_component：取 `affected[].package.name`，须在 watched 列表
/// - fixed_version：取 ranges 内首个 `fixed` 事件值
/// - severity：OSV severity 字段结构异质（CVSS 字符串/向量），本实现统一映射
///   `High`（保守默认；精确 CVSS→级别映射留 TODO，待数据库字段标准化）
/// - published_at：解析 ISO8601，失败用当前时间兜底
///
/// 解析失败（整体 JSON 非法）映射 [`UpdateError::CveCheckFailed`]；单条字段缺失仅跳过。
pub fn parse_osv_advisories(
    body: &str,
    watched_components: &[String],
) -> Result<Vec<CveAdvisory>, UpdateError> {
    let resp: OsvResponse = serde_json::from_str(body)
        .map_err(|e| UpdateError::CveCheckFailed(format!("OSV 响应 JSON 解析失败: {e}")))?;
    let watched: std::collections::HashSet<&str> =
        watched_components.iter().map(String::as_str).collect();

    let mut out = Vec::new();
    for v in resp.vulns {
        // 命中的组件 + fixed 版本
        let mut component: Option<String> = None;
        let mut fixed: Option<String> = None;
        for aff in &v.affected {
            if let Some(pkg) = &aff.package {
                if watched.contains(pkg.name.as_str()) && component.is_none() {
                    component = Some(pkg.name.clone());
                }
            }
            if fixed.is_none() {
                for r in &aff.ranges {
                    for ev in &r.events {
                        if let Some(t) = ev.get("type").and_then(|v| v.as_str()) {
                            if t == "fixed" {
                                if let Some(v) = ev.get("value").and_then(|v| v.as_str()) {
                                    fixed = Some(v.to_string());
                                    break;
                                }
                            }
                        }
                    }
                    if fixed.is_some() {
                        break;
                    }
                }
            }
        }
        let Some(component) = component else { continue };
        // CVE 编号
        let cve_id = v
            .aliases
            .iter()
            .find(|a| a.starts_with("CVE-"))
            .cloned()
            .unwrap_or(v.id.clone());
        let published_at = chrono::DateTime::parse_from_rfc3339(&v.published)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        out.push(CveAdvisory {
            cve_id,
            affected_component: component,
            severity: severity_from_summary(&v.summary),
            fixed_version: fixed.unwrap_or_else(|| "unknown".to_string()),
            published_at,
        });
    }
    Ok(out)
}

/// 保守的严重级别推断。
///
/// OSV `summary` 文本中含 critical/severe → Critical；high → High；
/// moderate → Medium；其余 → Low。无文本或匹配失败统一 Low（不放过任何公告，
/// 但不夸大）。精确 CVSS→级别映射留 TODO。
fn severity_from_summary(summary: &str) -> CveSeverity {
    let s = summary.to_ascii_lowercase();
    if s.contains("critical") || s.contains("severe") {
        CveSeverity::Critical
    } else if s.contains("high") {
        CveSeverity::High
    } else if s.contains("moderate") || s.contains("medium") {
        CveSeverity::Medium
    } else {
        CveSeverity::Low
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{ComponentUpdate, UpdateManifest};
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    fn manifest_with(package_sha: &str, signature_b64: &str) -> UpdateManifest {
        UpdateManifest {
            version: "1.2.0".to_string(),
            release_notes: String::new(),
            size_bytes: 0,
            sha256: package_sha.to_string(),
            signature: signature_b64.to_string(),
            min_current_version: None,
            components: vec![ComponentUpdate {
                name: "osd".to_string(),
                version: "1.2.0".to_string(),
                restart_required: false,
            }],
        }
    }

    // —— sha256 ——

    #[test]
    fn sha256_bytes_known_vector() {
        // SHA-256("abc") 标准向量
        let h = sha256_hex_bytes(b"abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pkg.bin");
        std::fs::write(&p, b"hello world").unwrap();
        let h = sha256_hex(&p).unwrap();
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    // —— ed25519 验签往返 ——

    #[test]
    fn verify_package_valid_signature_and_sha_ok() {
        // 构造测试密钥对（确定性 seed）
        let seed = [7u8; 32];
        let signing = SigningKey::from_bytes(&seed);
        let verifying: VerifyingKey = signing.verifying_key();

        // 写"下载"文件
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg.bin");
        let content = b"update payload bytes";
        std::fs::write(&pkg, content).unwrap();

        // 签名 = ed25519(sha256(content))
        let digest = {
            let mut h = Sha256::new();
            h.update(content);
            h.finalize()
        };
        let sig: Signature = signing.sign(&digest);
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let sha = sha256_hex(&pkg).unwrap();

        let manifest = manifest_with(&sha, &sig_b64);
        let ok = verify_package(&manifest, &pkg, &verifying.to_bytes()).unwrap();
        assert!(ok);
    }

    #[test]
    fn verify_package_tampered_content_fails_sha() {
        let seed = [7u8; 32];
        let signing = SigningKey::from_bytes(&seed);
        let verifying: VerifyingKey = signing.verifying_key();

        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg.bin");
        std::fs::write(&pkg, b"original").unwrap();
        let digest = sha256_raw(&pkg).unwrap();
        let sig = signing.sign(&digest);
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        // 篡改内容（sha256 不再匹配）
        std::fs::write(&pkg, b"tampered!!").unwrap();
        let manifest = manifest_with("deadbeef", &sig_b64);
        let err = verify_package(&manifest, &pkg, &verifying.to_bytes()).unwrap_err();
        assert!(matches!(err, UpdateError::VerificationFailed(_)));
    }

    #[test]
    fn verify_package_bad_signature_fails() {
        // 用密钥 A 签，用密钥 B 验 → 必失败
        let signing_a = SigningKey::from_bytes(&[1u8; 32]);
        let signing_b = SigningKey::from_bytes(&[2u8; 32]);
        let verifying_b: VerifyingKey = signing_b.verifying_key();

        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg.bin");
        std::fs::write(&pkg, b"payload").unwrap();
        let digest = sha256_raw(&pkg).unwrap();
        let sig = signing_a.sign(&digest); // A 签
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let sha = sha256_hex(&pkg).unwrap();

        let manifest = manifest_with(&sha, &sig_b64);
        let err = verify_package(&manifest, &pkg, &verifying_b.to_bytes()).unwrap_err(); // B 验
        assert!(matches!(err, UpdateError::VerificationFailed(_)));
    }

    #[test]
    fn verify_package_missing_file_fails_closed() {
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let manifest = manifest_with("any", "any");
        let err = verify_package(
            &manifest,
            std::path::Path::new("/nonexistent/os-update-pkg-xyz"),
            &signing.verifying_key().to_bytes(),
        )
        .unwrap_err();
        assert!(matches!(err, UpdateError::VerificationFailed(_)));
    }

    #[test]
    fn verify_package_invalid_base64_signature_fails() {
        let signing = SigningKey::from_bytes(&[4u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg.bin");
        std::fs::write(&pkg, b"x").unwrap();
        let sha = sha256_hex(&pkg).unwrap();
        // 非法 Base64
        let manifest = manifest_with(&sha, "@@@not-base64@@@");
        let err = verify_package(&manifest, &pkg, &signing.verifying_key().to_bytes()).unwrap_err();
        assert!(matches!(err, UpdateError::VerificationFailed(_)));
    }

    // —— CVE（OSV）解析 ——

    #[test]
    fn parse_osv_filters_watched_component() {
        let body = r#"{
            "vulns": [
                {
                    "id": "OSV-2024-1",
                    "aliases": ["CVE-2024-1111"],
                    "summary": "Critical RCE in samba",
                    "affected": [
                        {"package": {"name": "samba"}, "ranges": [{"events": [{"type":"introduced","value":"0"},{"type":"fixed","value":"4.20.1"}]}]}
                    ],
                    "published": "2024-01-15T00:00:00Z"
                },
                {
                    "id": "OSV-2024-2",
                    "summary": "low impact bug in nginx",
                    "affected": [{"package": {"name": "nginx"}}],
                    "published": "2024-02-01T00:00:00Z"
                }
            ]
        }"#;
        let watched = vec!["samba".to_string()];
        let list = parse_osv_advisories(body, &watched).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cve_id, "CVE-2024-1111");
        assert_eq!(list[0].affected_component, "samba");
        assert_eq!(list[0].fixed_version, "4.20.1");
        assert_eq!(list[0].severity, CveSeverity::Critical);
    }

    #[test]
    fn parse_osv_no_cve_alias_uses_osv_id() {
        let body = r#"{
            "vulns": [{
                "id": "GHSA-abcd",
                "summary": "high severity issue",
                    "affected": [{"package": {"name": "qemu"}, "ranges": [{"events": [{"type":"fixed","value":"8.2"}]}]}],
                "published": "2024-03-01T00:00:00Z"
            }]
        }"#;
        let list = parse_osv_advisories(body, &["qemu".to_string()]).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cve_id, "GHSA-abcd");
        assert_eq!(list[0].severity, CveSeverity::High);
        assert_eq!(list[0].fixed_version, "8.2");
    }

    #[test]
    fn parse_osv_invalid_json_returns_cve_check_failed() {
        let err = parse_osv_advisories("not json", &[]).unwrap_err();
        assert!(matches!(err, UpdateError::CveCheckFailed(_)));
    }

    #[test]
    fn parse_osv_empty_response() {
        let list = parse_osv_advisories(r#"{"vulns":[]}"#, &["samba".to_string()]).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn parse_osv_missing_fixed_version_uses_unknown() {
        let body = r#"{
            "vulns": [{
                "id": "OSV-X",
                "aliases": ["CVE-2024-9999"],
                "summary": "moderate issue",
                "affected": [{"package": {"name": "rdma-core"}, "ranges": [{"events": [{"type":"introduced","value":"0"}]}]}],
                "published": "2024-04-01T00:00:00Z"
            }]
        }"#;
        let list = parse_osv_advisories(body, &["rdma-core".to_string()]).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].fixed_version, "unknown");
        assert_eq!(list[0].severity, CveSeverity::Medium);
    }

    #[test]
    fn parse_osv_invalid_published_falls_back_to_now() {
        let body = r#"{
            "vulns": [{
                "id": "OSV-Y",
                "aliases": ["CVE-2024-1"],
                "summary": "low",
                "affected": [{"package": {"name": "samba"}, "ranges": [{"events": [{"type":"fixed","value":"1"}]}]}],
                "published": "not-a-date"
            }]
        }"#;
        let list = parse_osv_advisories(body, &["samba".to_string()]).unwrap();
        assert_eq!(list.len(), 1);
        // 仅断言不 panic 且时间合法（近 now）
        assert!(list[0].published_at <= chrono::Utc::now());
    }
}
