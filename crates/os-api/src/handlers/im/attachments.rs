//! IM 附件域（2026-09-25 大文件拆分批，纯搬运零行为变化）：附件消息体
//! （Attachment/ImFileRecord/ImFileDownload）+ 落盘/读取（64MB 上限、文件名
//! 消毒、MIME 推断）+ `?token=` 直链下载校验 `verify_attachment`。

use super::*;

/// IM 消息附件（服务端核对后的真值——`size_bytes`/`filename` 以
/// [`crate::handlers::im`] 上传落盘记录为准，客户端自报值被覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub file_id: String,
    pub filename: String,
    pub size_bytes: u64,
    /// MIME（上传时按扩展名猜测，发消息可覆盖）。
    #[serde(default)]
    pub mime: Option<String>,
}

/// 发消息 body 里的自报附件（仅 file_id 必填——filename/size 以服务端为准）。
#[derive(Debug, Clone, Deserialize)]
pub(super) struct AttachmentReq {
    file_id: String,
    #[serde(default)]
    #[allow(dead_code)] // 自报值一律被服务端真值覆盖（防伪造），字段仅作兼容解析
    filename: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // 同上：伪造 size 无效
    size_bytes: Option<u64>,
    #[serde(default)]
    mime: Option<String>,
}

/// IM 附件落盘记录（im_files 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImFileRecord {
    pub file_id: String,
    /// 净化后的原始文件名（展示/下载 Content-Disposition 用）。
    pub filename: String,
    pub size_bytes: u64,
    pub mime: Option<String>,
    /// 上传者 pubkey（链上身份）。
    pub uploader: Option<String>,
    /// 落盘绝对路径。
    pub path: String,
    pub created_at: String,
}

/// 附件下载信封（`GET /api/v1/im/files/:file_id` 响应体，与 files.rs
/// download 的 base64 JSON 信封同款先例——网关响应恒 JSON，无法回裸流）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImFileDownload {
    pub file_id: String,
    pub filename: String,
    pub size_bytes: u64,
    pub mime_type: String,
    /// 恒为 "base64"。
    pub encoding: String,
    pub content_base64: String,
}

/// 附件单文件上限：64 MiB。
pub(super) const IM_FILE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// 净化后文件名长度上限（字符；uuid 前缀另计）。
pub(super) const IM_FILENAME_MAX_CHARS: usize = 120;

/// 附件根目录（env `NEXOS_IM_FILES_ROOT` 覆盖）：`/tank/im-files`（可建）→
/// `/var/lib/os/im-files` → `./im-files`（与 default_db_path 同款回退链）。
pub(super) fn im_files_root_default() -> String {
    for p in ["/tank/im-files", "/var/lib/os/im-files"] {
        let path = std::path::Path::new(p);
        if path
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return p.to_string();
        }
    }
    "./im-files".to_string()
}

/// 净化上传文件名（纯函数）：先按白名单逐字符映射——ASCII 字母数字、CJK
/// （一-龥）、`.`、`-`、`_`、`(`、`)`、空格保留，**其余（含 `/` `\` 与控制
/// 字符——路径穿越/不可文名）一律 `_`**；再截到 [`IM_FILENAME_MAX_CHARS`]
/// 字符；全空回退 `file`。净化后是安全的单段名（uuid 前缀 + 该名落盘）。
#[must_use]
pub fn sanitize_im_filename(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            let keep = ('\u{4E00}'..='\u{9FA5}').contains(&c)
                || c.is_ascii_alphanumeric()
                || matches!(c, '.' | '-' | '_' | '(' | ')' | ' ');
            if keep {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return "file".to_string();
    }
    truncate_chars(&cleaned, IM_FILENAME_MAX_CHARS)
}

/// 附件直链 url（纯函数）：`/api/v1/im/files/<file_id>?token=<token>`
/// （相对路径，客户端自行拼 scheme://host:port）。
#[must_use]
pub fn im_file_url(file_id: &str, token: &str) -> String {
    if token.is_empty() {
        format!("/api/v1/im/files/{file_id}")
    } else {
        format!("/api/v1/im/files/{file_id}?token={token}")
    }
}

/// 附件落盘（阻塞调用，handler 经 spawn_blocking 调）：目录自动建 →
/// tmp+rename 原子写（files.rs store_upload 同款）。返回最终路径。
pub(super) fn store_im_file(
    dir: &std::path::Path,
    stored_name: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, (u16, String)> {
    if bytes.len() > IM_FILE_MAX_BYTES {
        return Err((413, "附件超限：单文件最大 64 MiB".to_string()));
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err((500, format!("附件目录自动创建失败: {e}")));
    }
    let final_path = dir.join(stored_name);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".imfile-{}-{nanos}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err((500, format!("写入临时文件失败: {e}")));
    }
    match std::fs::rename(&tmp, &final_path) {
        Ok(()) => Ok(final_path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err((500, format!("附件落盘失败: {e}")))
        }
    }
}

/// 读取附件并构造 base64 信封（阻塞调用；files.rs read_download 同款）：
/// 不存在 → 404；超 [`IM_FILE_MAX_BYTES`] → 413；IO 错误 → 500。
pub(super) fn read_im_file(
    rec: &ImFileRecord,
    max_bytes: u64,
) -> Result<ImFileDownload, (u16, String)> {
    let meta = std::fs::metadata(&rec.path).map_err(|e| (404, format!("附件文件不存在: {e}")))?;
    if meta.len() > max_bytes {
        return Err((413, "附件超限：单文件最大 64 MiB".to_string()));
    }
    let bytes = std::fs::read(&rec.path).map_err(|e| (500, format!("读取附件失败: {e}")))?;
    Ok(ImFileDownload {
        file_id: rec.file_id.clone(),
        filename: rec.filename.clone(),
        size_bytes: bytes.len() as u64,
        mime_type: rec
            .mime
            .clone()
            .unwrap_or_else(|| guess_mime_im(&rec.filename)),
        content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        encoding: "base64".to_string(),
    })
}

/// 按扩展名猜 MIME（极简映射，files.rs guess_mime 的 IM 精简版——文档传输
/// 场景优先覆盖 Office/PDF/图片）。
pub(super) fn guess_mime_im(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// `Content-Disposition: attachment` 头值（RFC 5987；files.rs 同款双 filename）。
pub(super) fn content_disposition_im(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ascii = if ascii.is_empty() { "download" } else { &ascii };
    let mut pct = String::with_capacity(name.len());
    for b in name.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if keep {
            pct.push(b as char);
        } else {
            pct.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{pct}")
}

// ----------------------------------------------------------------------------
// 消息推送通知 webhook（2026-08-22，docs/IM_AGENTS_AND_FILES.md §7）
// —— 注册/管理端点见 handle()，派发在 ImShared::dispatch_webhooks
// ----------------------------------------------------------------------------

pub(super) fn insert_file_record(conn: &Connection, r: &ImFileRecord) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_files (file_id,filename,size_bytes,mime,uploader,path,created_at) VALUES (?,?,?,?,?,?,?)",
        params![
            r.file_id,
            r.filename,
            r.size_bytes as i64,
            r.mime.as_deref(),
            r.uploader.as_deref(),
            r.path,
            r.created_at
        ],
    )?;
    Ok(())
}

pub(super) fn find_file_record(
    conn: &Connection,
    file_id: &str,
) -> rusqlite::Result<Option<ImFileRecord>> {
    conn.query_row(
        "SELECT file_id,filename,size_bytes,mime,uploader,path,created_at FROM im_files WHERE file_id=?",
        params![file_id],
        |row| {
            Ok(ImFileRecord {
                file_id: row.get(0)?,
                filename: row.get(1)?,
                size_bytes: row.get::<_, i64>(2)? as u64,
                mime: row.get(3)?,
                uploader: row.get(4)?,
                path: row.get(5)?,
                created_at: row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_default(),
            })
        },
    )
    .optional()
}

// ---- im_webhooks CRUD（消息推送通知，2026-08-22）----

impl ImRouteHandler {
    /// （伪造自报无效），mime 取自报（可精化）回落存储值。
    pub(super) async fn verify_attachment(
        &self,
        req: Option<&AttachmentReq>,
    ) -> Result<Option<Attachment>, ApiResponse> {
        let Some(a) = req else {
            return Ok(None);
        };
        let fid = a.file_id.clone();
        let record = self
            .db_call(move |conn| find_file_record(conn, &fid).unwrap_or(None))
            .await;
        let Some(rec) = record else {
            return Err(error_response(
                400,
                &format!(
                    "attachment.file_id 不存在: {}（先 POST /api/v1/im/files）",
                    a.file_id
                ),
            ));
        };
        Ok(Some(Attachment {
            file_id: rec.file_id,
            filename: rec.filename,
            size_bytes: rec.size_bytes,
            mime: a.mime.clone().filter(|m| !m.trim().is_empty()).or(rec.mime),
        }))
    }
}
