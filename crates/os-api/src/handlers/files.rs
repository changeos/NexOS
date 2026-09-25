//! `FilesRouteHandler` —— 文件管理器桌面应用的 HTTP→真实文件系统适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/files/*`）翻译为真实文件系统操作，返回 JSON。
//! 这是 OS"系统类三件套"之一（文件管理器）桌面应用的后端 REST 入口。
//!
//! # 当前实现策略：真实文件系统浏览（spawn_blocking）
//!
//! - `GET /list?path=<dir>`：`spawn_blocking` 读目录，返回 `Vec<FileEntry>`。
//!   任意子目录均可列（path 为相对路径时拼到根）。根路径映射：path 为空或 "/"
//!   时映射到 `/tank`（ZFS 池挂载点），不存在则回退 `/var/lib/os/files`。
//! - `GET /stat?path=<f>`：单文件 stat。
//! - `GET /usage?path=<dir>`：目录递归用量（总大小 + 文件/子目录数）。
//!   限制处理条目数 / 递归深度 / 软超时防大目录卡死，超限即停并置
//!   `partial: true`（此时各数值为下界）。
//! - `POST /upload?path=<dir>`（admin）：上传文件。**目标目录不存在时自动创建**
//!   （`create_dir_all`，契约选"自动建"）。body 为 JSON `{filename, content_base64}`；
//!   落盘走 tmp+rename 原子写；重名自动加后缀 `-1` `-2` …；单文件超 2 GiB → 413。
//!   返回 `{name, size_bytes, path}`。
//! - `GET /download?path=<file>`：下载文件，返回 JSON 信封
//!   `{name, path, size_bytes, mime_type, encoding:"base64", content_base64}`，
//!   另附 `Content-Disposition: attachment` 头（filename RFC 5987 百分号编码）。
//!   目录 → 400（目录 zip 打包暂未支持）；超 2 GiB → 413。
//! - `POST /mkdir`（admin）body `{path}`：创建目录。
//! - `POST /delete`（admin）body `{path}`：删除。
//! - `POST /rename`（admin）body `{from, to}`：重命名。
//!
//! # 上传/下载传输形态（重要契约说明）
//!
//! 网关内部 `ApiRequest.body` / `ApiResponse.body` 均为 `serde_json::Value`
//! （见 `gateway.rs` / `http.rs` 的 `decode_body`/`api_to_response`），二进制
//! multipart 体在入站解码时会被丢弃、响应出站恒为 JSON 序列化——因此本组件
//! 无法使用 axum multipart 提取器或裸字节流响应。**通道约定：文件字节经
//! base64 装在 JSON 里传输**，与 `qr_transfer` 的 `media_base64` 上传、
//! `media_gen` 的 `png_base64` 下发完全同款（仓内既有惯例）。代价：载荷膨胀
//! ×4/3，且需全程驻内存——故双向都设 2 GiB 上限（超限 413），大文件请走
//! SMB/NFS 共享通道。前端 `client.ts::filesUpload/filesDownload` 负责编码/解码。
//!
//! # 安全
//!
//! 禁止 path 含 `..`（返回 400），避免路径穿越；上传 filename 禁止含路径
//! 分隔符与 `..`，重名后缀在同一目标目录内生成，不产生新的穿越面。
//!
//! # 路由表
//!
//! | method | path                    | 动作 |
//! |--------|-------------------------|------|
//! | GET    | `/api/v1/files/list`    | 列目录（?path=，任意子目录）|
//! | GET    | `/api/v1/files/stat`    | 单文件 stat（?path=）|
//! | GET    | `/api/v1/files/usage`   | 目录递归用量（?path=）|
//! | GET    | `/api/v1/files/download`| 下载文件（?path=，base64 信封，公开）|
//! | POST   | `/api/v1/files/upload`  | 上传文件（?path= 目标目录，需 admin）|
//! | POST   | `/api/v1/files/mkdir`   | 创建目录（需 admin）|
//! | POST   | `/api/v1/files/delete`  | 删除（需 admin）|
//! | POST   | `/api/v1/files/rename`  | 重命名（需 admin）|

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条文件/目录条目（`GET /api/v1/files/list` 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// 条目名（不含目录前缀）
    pub name: String,
    /// 完整路径（绝对路径）
    pub path: String,
    /// 是否目录
    pub is_dir: bool,
    /// 大小（字节；目录为 0）
    pub size_bytes: u64,
    /// 修改时间（ISO 8601 字符串；不可用时为空）
    pub modified_at: String,
    /// MIME 类型（目录固定 `inode/directory`；文件按扩展名猜测）
    pub mime_type: String,
}

/// 目录递归用量统计（`GET /api/v1/files/usage` 响应体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirUsage {
    /// 统计目标目录（绝对路径）
    pub path: String,
    /// 递归总大小（字节；partial 时为下界）
    pub total_bytes: u64,
    /// 文件数（不含目录；partial 时为下界）
    pub file_count: u64,
    /// 子目录数（partial 时为下界）
    pub dir_count: u64,
    /// 是否因超条目数/深度/时限截断（true 时以上数值均为下界）
    pub partial: bool,
}

/// 上传请求体（`POST /api/v1/files/upload`，JSON）。
///
/// 字节经 base64 装在 JSON 里（通道约束见模块注释）；两个字段均为必填。
#[derive(Debug, Deserialize)]
struct UploadBody {
    /// 保留的原始文件名（不可含路径分隔符 / `..`）
    filename: Option<String>,
    /// 文件内容 base64（标准字母表，无 data: 前缀）
    content_base64: Option<String>,
}

/// 下载信封（`GET /api/v1/files/download` 响应体）。
#[derive(Debug, Clone, Serialize)]
struct FileDownload {
    /// 文件名（不含目录前缀）
    name: String,
    /// 完整路径（绝对路径）
    path: String,
    /// 字节数
    size_bytes: u64,
    /// 按扩展名推断的 MIME（未知 application/octet-stream）
    mime_type: String,
    /// 恒为 "base64"（信封编码方式，前端按此解码）
    encoding: String,
    /// 文件内容 base64
    content_base64: String,
}

// ----------------------------------------------------------------------------
// FilesRouteHandler
// ----------------------------------------------------------------------------

/// 文件管理器路由处理器——HTTP 边界适配到真实文件系统。
///
/// 无需持有可变状态（所有操作直接作用于真实 FS）。`new()` 是默认入口。
pub struct FilesRouteHandler;

impl FilesRouteHandler {
    /// 构造 handler。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for FilesRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for FilesRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/files/list", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/files/stat", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/files/usage", false, vec![]),
            // 下载：GET 免认证（与 list/stat/usage 只读惯例一致；SMB 内网同款暴露面）
            spec(HttpMethod::Get, "/api/v1/files/download", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/files/upload",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/files/mkdir",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/files/delete",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/files/rename",
                true,
                vec!["admin".into()],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/files/list —— 列目录（?path=）
            (HttpMethod::Get, ["api", "v1", "files", "list"]) => {
                let raw = query_param(&req.path, "path").unwrap_or_default();
                match resolve_root(&raw) {
                    Err(msg) => Ok(error_response(400, &msg)),
                    Ok(dir) => {
                        let joined = tokio::task::spawn_blocking(move || list_dir(&dir))
                            .await
                            .map_err(|e| {
                                ApiGatewayError::Internal(format!("列目录任务 join 失败: {e}"))
                            })?;
                        let entries = joined
                            .map_err(|e| ApiGatewayError::Internal(format!("列目录失败: {e}")))?;
                        Ok(ok_json(to_value(&entries)?))
                    }
                }
            }

            // —— GET /api/v1/files/stat —— 单文件 stat（?path=）
            (HttpMethod::Get, ["api", "v1", "files", "stat"]) => {
                let raw = query_param(&req.path, "path").unwrap_or_default();
                match resolve_root(&raw) {
                    Err(msg) => Ok(error_response(400, &msg)),
                    Ok(p) => {
                        let joined = tokio::task::spawn_blocking(move || stat_path(&p))
                            .await
                            .map_err(|e| {
                                ApiGatewayError::Internal(format!("stat 任务 join 失败: {e}"))
                            })?;
                        let entry = joined
                            .map_err(|e| ApiGatewayError::Internal(format!("stat 失败: {e}")))?;
                        Ok(ok_json(to_value(&entry)?))
                    }
                }
            }

            // —— GET /api/v1/files/usage —— 目录递归用量（?path=，超限降级 partial）
            (HttpMethod::Get, ["api", "v1", "files", "usage"]) => {
                let raw = query_param(&req.path, "path").unwrap_or_default();
                match resolve_root(&raw) {
                    Err(msg) => Ok(error_response(400, &msg)),
                    Ok(dir) => {
                        let joined = tokio::task::spawn_blocking(move || dir_usage(&dir))
                            .await
                            .map_err(|e| {
                                ApiGatewayError::Internal(format!("usage 任务 join 失败: {e}"))
                            })?;
                        let usage = joined.map_err(|e| {
                            ApiGatewayError::Internal(format!("统计目录用量失败: {e}"))
                        })?;
                        Ok(ok_json(to_value(&usage)?))
                    }
                }
            }

            // —— GET /api/v1/files/download —— 下载文件（?path=<文件>，base64 信封）
            //
            // 通道约束（见模块注释）：网关响应恒为 JSON 序列化，无法回裸字节流，
            // 故文件内容装在 content_base64 字段里，Content-Disposition 头照常
            // 下发（filename RFC 5987 编码）。目录 → 400；超 2 GiB → 413。
            (HttpMethod::Get, ["api", "v1", "files", "download"]) => {
                let Some(raw) = query_param(&req.path, "path") else {
                    return Ok(error_response(400, "download 需要 ?path=<文件路径>"));
                };
                let p = match resolve_root(&raw) {
                    Err(msg) => return Ok(error_response(400, &msg)),
                    Ok(p) => p,
                };
                let joined =
                    tokio::task::spawn_blocking(move || read_download(&p, DOWNLOAD_MAX_BYTES))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("download 任务 join 失败: {e}"))
                        })?;
                match joined {
                    Ok(dl) => Ok(ApiResponse {
                        status: 200,
                        body: to_value(&dl)?,
                        headers: serde_json::json!({
                            "content-disposition": content_disposition_value(&dl.name),
                        }),
                    }),
                    Err((status, msg)) => Ok(error_response(status, &msg)),
                }
            }

            // —— POST /api/v1/files/upload —— 上传文件（?path=<目标目录>，admin）
            //
            // 契约：目标目录不存在时自动创建（create_dir_all）；body JSON
            // {filename, content_base64}（multipart 无法穿过网关 JSON 通道，见
            // 模块注释）；超 2 GiB → 413；落盘 tmp+rename 原子写；重名加
            // `-1`/`-2` 后缀。成功返回 {name, size_bytes, path}。
            (HttpMethod::Post, ["api", "v1", "files", "upload"]) => {
                let raw = query_param(&req.path, "path").unwrap_or_default();
                let dir = match resolve_root(&raw) {
                    Err(msg) => return Ok(error_response(400, &msg)),
                    Ok(d) => d,
                };
                // 体解析失败（非 JSON 对象，如空体/字符串）也按 400 处理：
                // 与"multipart 缺字段 → 400"语义对齐（缺字段的传输层等价物）
                let body: UploadBody = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(
                            400,
                            "上传请求体须为 JSON 对象 {filename, content_base64}",
                        ))
                    }
                };
                let filename = body.filename.unwrap_or_default().trim().to_string();
                let b64 = body.content_base64.unwrap_or_default().trim().to_string();
                if filename.is_empty() || b64.is_empty() {
                    return Ok(error_response(
                        400,
                        "缺少必填字段 filename / content_base64（JSON 通道，见模块注释）",
                    ));
                }
                if let Err(msg) = validate_filename(&filename) {
                    return Ok(error_response(400, &msg));
                }
                // 超限前置检查：按 base64 长度估算解码后大小（len*3/4），避免
                // 先把 >2 GiB 的字符串解码进内存再拒绝。
                if b64.len() / 4 * 3 > UPLOAD_MAX_BYTES {
                    return Ok(error_response(413, "文件超限：单文件最大 2 GiB"));
                }
                let bytes = match base64::engine::general_purpose::STANDARD.decode(&b64) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("content_base64 非法: {e}"))),
                };
                let size = bytes.len();
                let joined = tokio::task::spawn_blocking(move || {
                    store_upload(
                        std::path::Path::new(&dir),
                        &filename,
                        &bytes,
                        UPLOAD_MAX_BYTES,
                    )
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("upload 任务 join 失败: {e}")))?;
                match joined {
                    Ok(final_path) => {
                        let name = final_path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        Ok(ok_json(serde_json::json!({
                            "name": name,
                            "size_bytes": size,
                            "path": final_path.to_string_lossy(),
                        })))
                    }
                    Err((status, msg)) => Ok(error_response(status, &msg)),
                }
            }

            // —— POST /api/v1/files/mkdir —— 创建目录（admin）
            (HttpMethod::Post, ["api", "v1", "files", "mkdir"]) => {
                let body: PathBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 mkdir 请求体失败: {e}"))
                })?;
                if let Err(msg) = validate_path(&body.path) {
                    return Ok(error_response(400, &msg));
                }
                let p = body.path.clone();
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&p))
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("mkdir 任务 join 失败: {e}")))?
                    .map_err(|e| ApiGatewayError::Internal(format!("创建目录失败: {e}")))?;
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "path": body.path,
                    "action": "mkdir"
                })))
            }

            // —— POST /api/v1/files/delete —— 删除（admin）
            (HttpMethod::Post, ["api", "v1", "files", "delete"]) => {
                let body: PathBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 delete 请求体失败: {e}"))
                })?;
                if let Err(msg) = validate_path(&body.path) {
                    return Ok(error_response(400, &msg));
                }
                let p = body.path.clone();
                let joined = tokio::task::spawn_blocking(move || {
                    let meta = std::fs::metadata(&p);
                    if meta.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    }
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("delete 任务 join 失败: {e}")))?;
                joined.map_err(|e| ApiGatewayError::Internal(format!("删除失败: {e}")))?;
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "path": body.path,
                    "action": "delete"
                })))
            }

            // —— POST /api/v1/files/rename —— 重命名（admin）
            (HttpMethod::Post, ["api", "v1", "files", "rename"]) => {
                let body: RenameBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 rename 请求体失败: {e}"))
                })?;
                if let Err(msg) = validate_path(&body.from) {
                    return Ok(error_response(400, &msg));
                }
                if let Err(msg) = validate_path(&body.to) {
                    return Ok(error_response(400, &msg));
                }
                let (from, to) = (body.from.clone(), body.to.clone());
                tokio::task::spawn_blocking(move || std::fs::rename(&from, &to))
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("rename 任务 join 失败: {e}")))?
                    .map_err(|e| ApiGatewayError::Internal(format!("重命名失败: {e}")))?;
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "from": body.from,
                    "to": body.to,
                    "action": "rename"
                })))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "files: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// mkdir/delete 请求体。
#[derive(Debug, Deserialize)]
struct PathBody {
    path: String,
}

/// rename 请求体。
#[derive(Debug, Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

/// 构造一条 [`RouteSpec`]（component 固定 `files`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "files".to_string(),
        requires_auth,
        required_roles,
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 把可序列化结果转成 `serde_json::Value`。
fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 从请求路径中剥离 `?query` 后的纯 path 段。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 从请求路径的 query string 中提取指定参数。
fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split('?').nth(1)?;
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next()? == key {
            let v = it.next().unwrap_or("");
            let decoded = url_decode(v);
            if decoded.is_empty() {
                return None;
            }
            return Some(decoded);
        }
    }
    None
}

/// 极简 URL 解码（仅处理 `+` → 空格 与 `%XX`）。
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) =
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 校验 path 不含 `..`（防穿越），空路径仅用于根映射场景由调用方处理。
/// 返回 `Err(msg)` 表示非法。
fn validate_path(path: &str) -> Result<(), String> {
    let p = path.trim();
    if p.is_empty() {
        return Err("path 不可为空".into());
    }
    // 任一段为 ".." 即拒绝
    for seg in p.split('/') {
        if seg == ".." {
            return Err("路径不可包含 '..'".into());
        }
    }
    Ok(())
}

/// 把用户输入的 path 解析为真实文件系统绝对路径。
///
/// - 空 / "/" → 根映射：优先 `/tank`，不存在则 `/var/lib/os/files`（自动创建）。
/// - 否则：校验无 `..`，若以 `/` 开头视为绝对路径，否则拼接根目录。
///
/// 返回 `Err(msg)` 表示非法输入（400）。
fn resolve_root(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    // 根映射
    if trimmed.is_empty() || trimmed == "/" {
        if std::path::Path::new("/tank").is_dir() {
            return Ok("/tank".to_string());
        }
        let fallback = "/var/lib/os/files";
        let _ = std::fs::create_dir_all(fallback);
        return Ok(fallback.to_string());
    }
    validate_path(trimmed)?;
    if trimmed.starts_with('/') {
        Ok(trimmed.to_string())
    } else {
        // 相对路径：拼到根目录
        let base = if std::path::Path::new("/tank").is_dir() {
            "/tank"
        } else {
            "/var/lib/os/files"
        };
        Ok(format!("{base}/{trimmed}"))
    }
}

/// 列出目录下的条目（目录优先，再按名称排序）。失败返回 `Err`（IO 错误）。
fn list_dir(dir: &str) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata()?;
        let is_dir = meta.is_dir();
        let size_bytes = if is_dir { 0 } else { meta.len() };
        let modified_at = format_modified(meta.modified().ok().as_ref());
        let mime_type = if is_dir {
            "inode/directory".into()
        } else {
            guess_mime(&name)
        };
        entries.push(FileEntry {
            name,
            path: path.to_string_lossy().into_owned(),
            is_dir,
            size_bytes,
            modified_at,
            mime_type,
        });
    }
    // 目录优先，再按名称（不区分大小写）排序
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

/// 对单一路径做 stat，返回一条 `FileEntry`。失败（不存在）返回 `Err`。
fn stat_path(p: &str) -> std::io::Result<FileEntry> {
    let meta = std::fs::metadata(p)?;
    let path = std::path::Path::new(p);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string());
    let is_dir = meta.is_dir();
    let size_bytes = if is_dir { 0 } else { meta.len() };
    Ok(FileEntry {
        name,
        path: p.to_string(),
        is_dir,
        size_bytes,
        modified_at: format_modified(meta.modified().ok().as_ref()),
        mime_type: if is_dir {
            "inode/directory".into()
        } else {
            guess_mime(
                &path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        },
    })
}

// ----------------------------------------------------------------------------
// 上传 / 下载内部实现（阻塞 FS 操作，均由 handler 经 spawn_blocking 调用）
// ----------------------------------------------------------------------------

/// 校验上传文件名：非空、不含路径分隔符（`/` `\`）、不为 `.`/`..`。
///
/// 文件名只作为目标目录内的单段（`dir.join(filename)`），拒绝分隔符后
/// 不存在再穿越的可能；`..` 单段名同理拒绝。
fn validate_filename(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("filename 不可为空".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("filename 不可包含路径分隔符".into());
    }
    if name == "." || name == ".." {
        return Err("filename 不可为 . 或 ..".into());
    }
    Ok(())
}

/// 拆 `stem.ext`：无扩展名或点文件（`.bashrc`）整体作 stem。
fn split_stem_ext(name: &str) -> (String, Option<String>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            (stem.to_string(), Some(ext.to_string()))
        }
        _ => (name.to_string(), None),
    }
}

/// 重名避让：目标已存在时按 `stem-N.ext`（`stem` 无扩展名则 `stem-N`）
/// 顺次找空闲名。全部占用（超 [`DEDUPE_MAX_TRIES`]）返回 `None`。
fn dedupe_filename(dir: &std::path::Path, filename: &str) -> Option<String> {
    if !dir.join(filename).exists() {
        return Some(filename.to_string());
    }
    let (stem, ext) = split_stem_ext(filename);
    for n in 1..=DEDUPE_MAX_TRIES {
        let cand = match &ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        if !dir.join(&cand).exists() {
            return Some(cand);
        }
    }
    None
}

/// 上传落盘（阻塞调用）：目录自动创建 → 大小闸门 → 重名避让 →
/// **tmp + rename 原子写**（临时文件与终名同目录=同文件系统，rename 原子；
/// 写失败清理临时文件，不留半截文件在目标目录）。
///
/// 返回最终落盘路径；`Err((http_status, msg))` 由 handler 转 error_response。
fn store_upload(
    dir: &std::path::Path,
    filename: &str,
    bytes: &[u8],
    max_bytes: usize,
) -> Result<std::path::PathBuf, (u16, String)> {
    if bytes.len() > max_bytes {
        return Err((
            413,
            format!("文件超限：{} 字节 > 上限 {} 字节", bytes.len(), max_bytes),
        ));
    }
    // 目标目录不存在时自动创建（契约选"自动建"；已存在为 no-op）
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err((400, format!("目标目录自动创建失败: {e}")));
    }
    let final_name = match dedupe_filename(dir, filename) {
        Some(n) => n,
        None => return Err((409, "重名后缀尝试次数超限，请改名重试".to_string())),
    };
    let final_path = dir.join(&final_name);
    // 临时文件放同目录（保证与终名同文件系统，rename 为原子替换）
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".upload-{}-{nanos}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err((500, format!("写入临时文件失败: {e}")));
    }
    match std::fs::rename(&tmp, &final_path) {
        Ok(()) => Ok(final_path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err((500, format!("上传落盘失败: {e}")))
        }
    }
}

/// 读取待下载文件并构造 base64 信封（阻塞调用）。
///
/// 不存在 → 404；目录 → 400（zip 打包暂未支持）；超 [`DOWNLOAD_MAX_BYTES`]
/// → 413；读取 IO 错误 → 500。
fn read_download(p: &str, max_bytes: u64) -> Result<FileDownload, (u16, String)> {
    let meta = std::fs::metadata(p).map_err(|e| (404, format!("文件不存在或不可读: {e}")))?;
    if meta.is_dir() {
        return Err((
            400,
            "目标是目录，仅支持单文件下载（目录 zip 打包暂未支持）".into(),
        ));
    }
    if meta.len() > max_bytes {
        return Err((
            413,
            format!(
                "文件超限：{} 字节 > 下载通道上限 {max_bytes} 字节",
                meta.len()
            ),
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| (500, format!("读取文件失败: {e}")))?;
    let name = std::path::Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string());
    Ok(FileDownload {
        mime_type: guess_mime(&name),
        size_bytes: bytes.len() as u64,
        content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        name,
        path: p.to_string(),
        encoding: "base64".to_string(),
    })
}

/// 构造 `Content-Disposition: attachment` 头值：ASCII 回退名 +
/// RFC 5987 `filename*=UTF-8''…`（非 ASCII 文件名百分号编码，浏览器优先取后者）。
fn content_disposition_value(name: &str) -> String {
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_filename_fallback(name),
        pct_encode_rfc5987(name)
    )
}

/// ASCII 回退文件名：非 [A-Za-z0-9._-] 一律替换 `_`（老浏览器/老工具用）。
fn ascii_filename_fallback(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "download".into()
    } else {
        s
    }
}

/// RFC 5987 attr-char 百分号编码（UTF-8 字节级；attr-char 外全部 `%XX`）。
fn pct_encode_rfc5987(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 上传单文件大小上限（2 GiB）。base64 JSON 通道载荷 ×4/3 且全程驻内存，
/// 与下载通道同限；大文件请走 SMB/NFS。超限 413。
const UPLOAD_MAX_BYTES: usize = 2 * 1024 * 1024 * 1024;
/// 下载单文件大小上限（2 GiB，理由同上）。超限 413。
const DOWNLOAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 重名后缀最大尝试次数（`-1` … `-10000`），防病态目录下死循环。
const DEDUPE_MAX_TRIES: u32 = 10_000;

/// usage 统计上限：最多处理条目数（防大目录卡死）。
const USAGE_MAX_ENTRIES: u64 = 50_000;
/// usage 统计上限：最大递归深度（防超深目录树）。
const USAGE_MAX_DEPTH: usize = 32;
/// usage 统计软超时（防慢速文件系统长时间占用请求）。
const USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 统计目录递归用量（总大小 + 文件/子目录数），带默认上限。
/// 目标不是目录时返回 `Err`（InvalidInput）。
fn dir_usage(root: &str) -> std::io::Result<DirUsage> {
    dir_usage_capped(root, USAGE_MAX_ENTRIES, USAGE_MAX_DEPTH, USAGE_TIMEOUT)
}

/// [`dir_usage`] 的可参数化版本（测试注入小上限验证 partial 降级）。
///
/// 迭代 DFS（显式栈，防递归溢出）；`DirEntry::metadata` 不跟随符号链接（防环），
/// symlink/socket 等特殊条目不计入。触达任一上限或遍历中途出错即停并置
/// `partial: true`——此时返回的数值是下界，调用方应按 "≥" 展示。
fn dir_usage_capped(
    root: &str,
    max_entries: u64,
    max_depth: usize,
    timeout: std::time::Duration,
) -> std::io::Result<DirUsage> {
    let meta = std::fs::metadata(root)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage 目标不是目录",
        ));
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut total_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    let mut dir_count: u64 = 0;
    let mut entries_seen: u64 = 0;
    let mut partial = false;
    // (目录, 深度)：根为 0
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.into(), 0)];

    'walk: while let Some((dir, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                // 单个目录读失败：降级继续统计其余部分
                partial = true;
                continue;
            }
        };
        for ent in rd {
            if entries_seen >= max_entries || std::time::Instant::now() > deadline {
                partial = true;
                stack.clear();
                break 'walk;
            }
            let ent = match ent {
                Ok(ent) => ent,
                Err(_) => {
                    partial = true;
                    continue;
                }
            };
            entries_seen += 1;
            // DirEntry::metadata 不跟随符号链接（Unix 上等价 lstat），无环风险
            let m = match ent.metadata() {
                Ok(m) => m,
                Err(_) => {
                    partial = true;
                    continue;
                }
            };
            let ft = m.file_type();
            if ft.is_dir() {
                dir_count += 1;
                if depth + 1 < max_depth {
                    stack.push((ent.path(), depth + 1));
                } else {
                    // 深度超限：该子目录不再展开（仍计入 dir_count）
                    partial = true;
                }
            } else if ft.is_file() {
                file_count += 1;
                total_bytes += m.len();
            }
            // 其它类型（symlink / socket / fifo 等）不计
        }
    }

    Ok(DirUsage {
        path: root.to_string(),
        total_bytes,
        file_count,
        dir_count,
        partial,
    })
}

/// 把 `SystemTime` 格式化为 ISO 8601 字符串（失败返回空串）。
fn format_modified(t: Option<&std::time::SystemTime>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    use chrono::{DateTime, Local};
    DateTime::<Local>::from(*t)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 按扩展名猜测 MIME 类型（极简映射，未知返回 `application/octet-stream`）。
fn guess_mime(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "ts" => "text/typescript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/x-rar-compressed",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "iso" => "application/x-iso9660-image",
        "img" | "qcow2" | "vdi" | "vmdk" => "application/x-diskimage",
        _ => "application/octet-stream",
    }
    .into()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    /// 模拟前端 `encodeURIComponent`：`/` → `%2F`（其余 ASCII 原样）。
    fn url_encode_path(s: &str) -> String {
        s.replace('/', "%2F")
    }

    // —— routes() 声明 ——

    #[tokio::test]
    async fn routes_declares_eight_endpoints() {
        let h = FilesRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 8);
        assert!(routes.iter().all(|r| r.handler_component == "files"));
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/files/list")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/files/stat")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/files/usage")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/files/download")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/files/upload")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/files/mkdir")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/files/delete")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/files/rename")));
        // 写操作要求 admin；读端点（含 download）免认证
        for r in &routes {
            if r.method == HttpMethod::Post {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            } else {
                assert!(!r.requires_auth, "GET {} 应免认证", r.path);
            }
        }
    }

    // —— GET /api/v1/files/list —— 列根目录（真实 FS，至少能列回退目录）——

    #[tokio::test]
    async fn list_root_returns_entries_or_empty() {
        // CI 等受限环境：既无 /tank、又无权限创建 /var/lib/os/files 回退目录时，
        // 该用例的前提不成立（handler 会走 500 分支），跳过而非误报。
        if !std::path::Path::new("/tank").is_dir()
            && std::fs::create_dir_all("/var/lib/os/files").is_err()
        {
            return eprintln!("[files] 跳过 list_root 测试：无 /tank 且 /var/lib/os 不可写");
        }
        let h = FilesRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/files/list")).await.unwrap();
        // 回退目录 /var/lib/os/files 一定存在（resolve_root 自动创建），故 200
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array());
    }

    // —— 安全：path 含 ".." 被拒绝（400）——

    #[tokio::test]
    async fn list_rejects_dotdot_path() {
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/files/list?path=/foo/../bar"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains(".."));
    }

    #[tokio::test]
    async fn stat_rejects_dotdot_path() {
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/files/stat?path=../etc/passwd"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // —— 任意子目录可列（目录导航的契约：?path= 指向哪就列哪）——

    #[tokio::test]
    async fn list_subdirectory_returns_entries() {
        let base = std::env::temp_dir().join(format!(
            "os-files-list-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sub = base.join("outer").join("inner");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), b"hello").unwrap();
        let h = FilesRouteHandler::new();
        // URL 编码的绝对子目录路径（前端 encodeURIComponent 产物）
        let encoded = format!(
            "/api/v1/files/list?path={}",
            url_encode_path(&sub.to_string_lossy())
        );
        let resp = h.handle(get_req(&encoded)).await.unwrap();
        assert_eq!(resp.status, 200, "list body: {resp:?}");
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "a.txt");
        assert_eq!(arr[0]["is_dir"], false);
        assert_eq!(arr[0]["size_bytes"], 5);
        assert!(!arr[0]["modified_at"].as_str().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    // —— GET /api/v1/files/usage —— 目录递归用量 ——

    /// 构造临时目录树：root/{f1(10B), f2(20B), d/{f3(30B)}}。
    fn usage_fixture() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "os-files-usage-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let d = base.join("d");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(base.join("f1"), [0u8; 10]).unwrap();
        std::fs::write(base.join("f2"), [0u8; 20]).unwrap();
        std::fs::write(d.join("f3"), [0u8; 30]).unwrap();
        base
    }

    #[tokio::test]
    async fn usage_counts_recursive_files() {
        let base = usage_fixture();
        let h = FilesRouteHandler::new();
        let encoded = format!(
            "/api/v1/files/usage?path={}",
            url_encode_path(&base.to_string_lossy())
        );
        let resp = h.handle(get_req(&encoded)).await.unwrap();
        assert_eq!(resp.status, 200, "usage body: {resp:?}");
        assert_eq!(resp.body["file_count"], 3);
        assert_eq!(resp.body["dir_count"], 1);
        assert_eq!(resp.body["total_bytes"], 60);
        assert_eq!(resp.body["partial"], false);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn usage_rejects_dotdot_path() {
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/files/usage?path=/tank/../etc"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn usage_rejects_file_target() {
        let base = usage_fixture();
        let err = dir_usage(base.join("f1").to_str().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn usage_partial_when_entries_capped() {
        let base = usage_fixture();
        // 上限 1 条：必然截断，数值为下界
        let u = dir_usage_capped(
            base.to_str().unwrap(),
            1,
            USAGE_MAX_DEPTH,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        assert!(u.partial);
        assert!(u.total_bytes < 60 || u.file_count < 3);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn usage_partial_when_depth_capped() {
        let base = usage_fixture();
        // 深度上限 1：子目录 d 不再展开（f3 统计不到）
        let u = dir_usage_capped(
            base.to_str().unwrap(),
            USAGE_MAX_ENTRIES,
            1,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        assert!(u.partial);
        assert_eq!(u.file_count, 2); // 只统计到根层 f1/f2
        assert_eq!(u.dir_count, 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    // —— mkdir/delete/rename 路由声明（用临时目录真实跑一遍）——

    #[tokio::test]
    async fn mkdir_delete_rename_roundtrip() {
        let tmp = std::env::temp_dir();
        let base = tmp.join(format!(
            "os-files-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let h = FilesRouteHandler::new();

        // mkdir
        let dir = base.join("subdir");
        let resp = h
            .handle(post_req(
                "/api/v1/files/mkdir",
                serde_json::json!({ "path": dir.to_string_lossy() }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "mkdir body: {resp:?}");
        assert!(dir.is_dir());

        // rename
        let dir2 = base.join("subdir2");
        let resp = h
            .handle(post_req(
                "/api/v1/files/rename",
                serde_json::json!({
                    "from": dir.to_string_lossy(),
                    "to": dir2.to_string_lossy()
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "rename body: {resp:?}");
        assert!(!dir.exists());
        assert!(dir2.is_dir());

        // delete
        let resp = h
            .handle(post_req(
                "/api/v1/files/delete",
                serde_json::json!({ "path": dir2.to_string_lossy() }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "delete body: {resp:?}");
        assert!(!dir2.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    // —— 上传 / 下载（数据面最后一公里；传输形态见模块注释：base64 JSON）——

    /// 唯一临时目录（每次调用新目录，测试间互不干扰）。
    fn temp_dir_unique(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "os-files-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// 构造上传请求（POST JSON {filename, content_base64}）。
    fn upload_req(dir: &str, filename: &str, content: &[u8]) -> ApiRequest {
        post_req(
            &format!("/api/v1/files/upload?path={}", url_encode_path(dir)),
            serde_json::json!({
                "filename": filename,
                "content_base64": base64::engine::general_purpose::STANDARD.encode(content),
            }),
        )
    }

    #[tokio::test]
    async fn upload_persists_file_and_returns_meta() {
        let base = temp_dir_unique("up");
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(upload_req(
                base.to_str().unwrap(),
                "hello.txt",
                b"hello world",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "upload body: {resp:?}");
        assert_eq!(resp.body["name"], "hello.txt");
        assert_eq!(resp.body["size_bytes"], 11);
        assert_eq!(
            resp.body["path"].as_str().unwrap(),
            base.join("hello.txt").to_str().unwrap()
        );
        // 落盘内容逐字节一致
        assert_eq!(
            std::fs::read(base.join("hello.txt")).unwrap(),
            b"hello world".to_vec()
        );
        // 原子写不留临时文件残留
        let leftovers: Vec<_> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".upload-"))
            .collect();
        assert!(leftovers.is_empty(), "tmp 残留: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn upload_duplicate_appends_suffixes() {
        let base = temp_dir_unique("dup");
        let h = FilesRouteHandler::new();
        for (i, expect) in ["a.bin", "a-1.bin", "a-2.bin"].iter().enumerate() {
            let resp = h
                .handle(upload_req(base.to_str().unwrap(), "a.bin", &[i as u8; 3]))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "第 {} 次: {resp:?}", i + 1);
            assert_eq!(resp.body["name"], *expect, "第 {} 次重名避让", i + 1);
        }
        // 三份都在盘上，原文件未被覆盖
        assert_eq!(std::fs::read(base.join("a.bin")).unwrap(), vec![0u8, 0, 0]);
        assert_eq!(
            std::fs::read(base.join("a-1.bin")).unwrap(),
            vec![1u8, 1, 1]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn upload_auto_creates_missing_target_dir() {
        // 契约：目标目录不存在 → 自动创建（create_dir_all，多层也建）
        let base = temp_dir_unique("mk");
        let target = base.join("newdir").join("deep");
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(upload_req(target.to_str().unwrap(), "x.md", b"# hi"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "upload body: {resp:?}");
        assert!(target.join("x.md").is_file(), "应自动建目录并落盘");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn upload_missing_fields_rejected() {
        let base = temp_dir_unique("fields");
        let h = FilesRouteHandler::new();
        let path = format!(
            "/api/v1/files/upload?path={}",
            url_encode_path(base.to_str().unwrap())
        );
        // multipart 缺字段语义等价物：空对象 / 只有 filename / 只有 content_base64 / null body
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "filename": "a.txt" }),
            serde_json::json!({ "content_base64": "aGVsbG8=" }),
            serde_json::Value::Null,
        ] {
            let resp = h.handle(post_req(&path, body)).await.unwrap();
            assert_eq!(resp.status, 400, "缺字段应 400: {resp:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn upload_rejects_traversal_and_bad_filenames() {
        let base = temp_dir_unique("trav");
        let h = FilesRouteHandler::new();
        // 目标目录路径穿越（?path= 含 ..）
        let resp = h
            .handle(upload_req("/tmp/../etc", "a.txt", b"x"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "path 穿越应 400: {resp:?}");
        // 文件名带路径分隔符（可借 dir.join 逃出目标目录）
        for bad in ["a/b.txt", "a\\b.txt", "..", "."] {
            let resp = h
                .handle(upload_req(base.to_str().unwrap(), bad, b"x"))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "filename={bad:?} 应 400");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn upload_rejects_invalid_base64() {
        let base = temp_dir_unique("b64");
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(post_req(
                &format!(
                    "/api/v1/files/upload?path={}",
                    url_encode_path(base.to_str().unwrap())
                ),
                serde_json::json!({ "filename": "a.txt", "content_base64": "!!!not-base64!!!" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非法 base64 应 400: {resp:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn store_upload_enforces_size_gate() {
        // 413 闸门走小上限注入验证（真实 2 GiB 上限不宜在单测里分配）
        let base = temp_dir_unique("limit");
        let err = store_upload(&base, "big.bin", &[0u8; 100], 10).unwrap_err();
        assert_eq!(err.0, 413);
        // 未落任何文件
        assert!(std::fs::read_dir(&base).unwrap().next().is_none());
        // 界内尺寸正常通过
        let p = store_upload(&base, "ok.bin", &[0u8; 10], 10).unwrap();
        assert_eq!(p, base.join("ok.bin"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn download_returns_envelope_and_disposition_header() {
        let base = temp_dir_unique("dl");
        std::fs::write(base.join("数据 报告.txt"), "报告内容").unwrap();
        let h = FilesRouteHandler::new();
        let encoded = format!(
            "/api/v1/files/download?path={}",
            url_encode_path(base.join("数据 报告.txt").to_str().unwrap())
        );
        let resp = h.handle(get_req(&encoded)).await.unwrap();
        assert_eq!(resp.status, 200, "download body: {resp:?}");
        // 信封字段
        assert_eq!(resp.body["name"], "数据 报告.txt");
        assert_eq!(resp.body["size_bytes"], 12); // "报告内容" 4 汉字 × 3 字节
        assert_eq!(resp.body["mime_type"], "text/plain");
        assert_eq!(resp.body["encoding"], "base64");
        // 内容逐字节还原
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(resp.body["content_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, "报告内容".as_bytes().to_vec());
        // Content-Disposition：attachment + ASCII 回退名 + RFC 5987 编码名
        let cd = resp.headers["content-disposition"].as_str().unwrap();
        assert!(cd.starts_with("attachment; filename=\""), "cd={cd}");
        assert!(cd.contains("filename*=UTF-8''"), "cd={cd}");
        assert!(cd.contains("%20"), "空格应百分号编码: {cd}");
        assert!(cd.contains("%E6%95%B0%E6%8D%AE"), "中文应 UTF-8 编码: {cd}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn download_directory_returns_400_even_with_zip() {
        let base = temp_dir_unique("dldir");
        let h = FilesRouteHandler::new();
        for q in ["", "&zip=1"] {
            let encoded = format!(
                "/api/v1/files/download?path={}{}",
                url_encode_path(base.to_str().unwrap()),
                q
            );
            let resp = h.handle(get_req(&encoded)).await.unwrap();
            assert_eq!(resp.status, 400, "zip={q:?} 目录下载应 400: {resp:?}");
            assert!(
                resp.body["error"].as_str().unwrap().contains("目录"),
                "error 应说明目录不支持: {resp:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn download_missing_file_returns_404() {
        let base = temp_dir_unique("dl404");
        let h = FilesRouteHandler::new();
        let encoded = format!(
            "/api/v1/files/download?path={}",
            url_encode_path(base.join("nope.txt").to_str().unwrap())
        );
        let resp = h.handle(get_req(&encoded)).await.unwrap();
        assert_eq!(resp.status, 404, "missing body: {resp:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn download_missing_path_param_returns_400() {
        let h = FilesRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/files/download")).await.unwrap();
        assert_eq!(resp.status, 400);
        let resp = h
            .handle(get_req("/api/v1/files/download?path="))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "空 path 应视同缺失");
    }

    #[tokio::test]
    async fn download_rejects_dotdot_path() {
        let h = FilesRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/files/download?path=/tank/../etc/passwd"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn split_stem_ext_variants() {
        assert_eq!(
            split_stem_ext("a.txt"),
            ("a".to_string(), Some("txt".to_string()))
        );
        assert_eq!(split_stem_ext("archive.tar.gz").1, Some("gz".to_string()));
        assert_eq!(split_stem_ext("noext").1, None);
        // 点文件：整体作 stem（生成 .bashrc-1 而非 -1.bashrc）
        assert_eq!(split_stem_ext(".bashrc").1, None);
        assert_eq!(split_stem_ext(".bashrc").0, ".bashrc");
    }

    #[test]
    fn pct_encode_rfc5987_covers_ascii_and_cjk() {
        // attr-char 原样保留
        assert_eq!(pct_encode_rfc5987("aB9-._~"), "aB9-._~");
        // 空格/引号/分号 → %XX；中文按 UTF-8 字节编码
        assert_eq!(pct_encode_rfc5987("a b"), "a%20b");
        assert_eq!(pct_encode_rfc5987("数"), "%E6%95%B0");
        assert!(pct_encode_rfc5987("引号\"名").contains("%22"));
    }

    #[test]
    fn ascii_fallback_replaces_unsafe_chars() {
        assert_eq!(ascii_filename_fallback("数据.txt"), "__.txt");
        assert_eq!(ascii_filename_fallback("q\"uote.rdp"), "q_uote.rdp");
        assert_eq!(ascii_filename_fallback("???"), "___");
    }

    // —— 兜底 ——

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        let h = FilesRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/files/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    // —— 辅助函数 ——

    #[test]
    fn guess_mime_known_extensions() {
        assert_eq!(guess_mime("a.mp4"), "video/mp4");
        assert_eq!(guess_mime("photo.JPG"), "image/jpeg");
        assert_eq!(guess_mime("readme.md"), "text/markdown");
        assert_eq!(guess_mime("noext"), "application/octet-stream");
    }

    #[test]
    fn validate_path_rejects_dotdot() {
        assert!(validate_path("../x").is_err());
        assert!(validate_path("a/../b").is_err());
        assert!(validate_path("/ok/path").is_ok());
        assert!(validate_path("").is_err());
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<FilesRouteHandler>();
    }
}
