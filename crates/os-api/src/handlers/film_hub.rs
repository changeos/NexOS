//! FilmHub 流程引擎（2026-09-06，v0.1.35 批次）——`handlers/film.rs` 的姊妹模块。
//!
//! film.rs 超 5000 行后按任务书拆分：本模块承载 FilmHub 新链的全部增量，
//! 与 film.rs **共享** `FilmCtx` / `FilmRouteHandler` / 任务框架与阶段执行器
//! （film.rs 侧把所需项提升为 `pub(crate)`，零复制）。产品主线：
//!
//! ```text
//! hub 建项目 → 剧情页（①txt/小说导入 ②AI 写）→ hub 存剧情
//!   → 分镜页（AI 读剧情分析生成分镜脚本）→ hub 存分镜
//!   → 定妆（AI 提取六类对象 + 每对象多视图）→ BGM（高频/场景触发式）
//!   → 生成（cache 半成品与 dist 成品分离，compose 版本化）
//! ```
//!
//! # hub 文件树（冻结契约，docs/FILM_STUDIO.md「FilmHub」章）
//!
//! ```text
//! <dir>/hub/
//! ├── project.md        # front-matter: title/ratio/style_hint/export_dir；正文=idea
//! ├── README.md         # 当前阶段（story|storyboard|casting|audio|compose）+ 一句话进度
//! ├── story/source-*.txt    # 方式一：导入原文（多份支持）
//! ├── story/story.md        # 剧情正稿（front-matter: source/words/summary；正文=分幕剧本）
//! ├── storyboard/storyboard.json  # 分镜（ScriptShot 全字段 + casting 引用扩展）
//! ├── casting/extraction.json     # AI 提取报告（六类，key=weapons 对应 props/ 目录）
//! ├── casting/characters/<name>/card.md  # front-matter: name/voice/portrait；正文=外形描述
//! ├── casting/characters/<name>/views/<view>.png   # 多视图（front/side/back/action-N/custom-*）
//! ├── casting/{props,pets,formations,actions,scenes}/<name>/…  # 同构
//! ├── audio/bgm/<track>/info.md   # front-matter: trigger(global|scene:<场景名>)/mood/duration
//! │                        + track.mp3
//! ├── budget.json       # 成本账本（film_cost_events 投影，DB 为真值）
//! ├── assets.json       # 资产统一清单 [{path,sha256,bytes,source,ref}]
//! ├── ownership.json    # 多人分工：members/sections/casting_objects（对象级认领）
//! ├── activity.json     # 操作流水环形 200 条 [{ts,author,action,target}]
//! ├── cache/            # 试生成/半成品（shot 试生成图、临时音频；不进 dist）
//! └── dist/             # 成品 final-vYYYYMMDD-HHMM.mp4 + compose-report.json
//! ```
//!
//! - **multipart 说明**：网关契约 body 恒为 JSON（files.rs / downloads.rs 同款
//!   先例——multipart 体在入站解码时被丢弃），story/import 等导入面一律
//!   b64 JSON 信封 `{filename, content_b64}`。
//! - **惰性初始化**：旧项目（无 hub/）首次调新端点（或 image/video/tts 试生成
//!   落 cache）自动 export 初始化树——film_characters 旧角色库同步迁移为
//!   casting/characters/<slug>/（card.md + portrait 定妆视图）。
//! - **ownership/activity**：分工与留名文件 export 时**原样保留**（真值在文件）；
//!   activity 由各写端点在完成点追加（author 缺省 "anonymous"）。
//!
//! # 路由（21 条新增，component=film；读公开/写 admin；未装应用全 404）
//!
//! 见 [`hub_routes`] 与 FILM_STUDIO.md 端点表（含 POST script 兼容别名）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::film::{
    error_response, load_characters, now_iso, ok_json, parse_script_shots, read_script,
    sniff_image_ext, spec_admin, spec_public, task_accepted, task_finish, task_log,
    validate_model_ref, FilmCtx, FilmProject, FilmRouteHandler, FilmTask, ModelRef, ScriptShot,
    IMAGE_MAX_BYTES,
};
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteSpec};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// 定妆对象六类（casting/<type>/ 目录名；extraction.json 的 weapons 对应 props/）。
pub const CASTING_TYPES: [&str; 6] = [
    "characters",
    "props",
    "pets",
    "formations",
    "actions",
    "scenes",
];

/// README 阶段枚举（story → storyboard → casting → audio → compose）。
pub const HUB_STAGES: [&str; 5] = ["story", "storyboard", "casting", "audio", "compose"];

/// activity.json 环形上限（条）。
const ACTIVITY_MAX: usize = 200;

/// story 导入原文上限（解码后字节数）。
const STORY_IMPORT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 剧情提示词内嵌原文的字符上限（2MB 原文全量进 prompt 不现实，截断如实标注）。
const STORY_PROMPT_MAX_CHARS: usize = 80_000;

/// 分镜提示词内嵌剧情正稿的字符上限。
const STORYBOARD_PROMPT_MAX_CHARS: usize = 60_000;

/// 提取提示词内嵌剧情/分镜的字符上限。
const EXTRACT_PROMPT_MAX_CHARS: usize = 40_000;

/// BGM 音频导入上限（解码后字节数）。
const BGM_IMPORT_MAX_BYTES: usize = 20 * 1024 * 1024;

/// 视图名/音轨名/缓存文件名的字符集（字母数字开头——含中日韩，允许 - 和 _）。
fn is_slug_like(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphanumeric() => {}
        _ => return false,
    }
    s.chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        && s.len() <= 64
}

/// hub 元文件（activity/ownership）读改写的进程内串行锁（防并发丢更新）。
static HUB_META_LOCK: Lazy<std::sync::Mutex<()>> = Lazy::new(|| std::sync::Mutex::new(()));

/// 成本事件序号（进程级；id=fce-<n>）。
static COST_SEQ: Lazy<std::sync::atomic::AtomicU64> =
    Lazy::new(|| std::sync::atomic::AtomicU64::new(0));

// ----------------------------------------------------------------------------
// 纯工具：slug / 百分号解码 / front-matter / 截断
// ----------------------------------------------------------------------------

/// 名字 slug 化：字母数字（含中日韩）保留，其余折叠为 '-'；去首尾 '-'，≤64 字符。
/// 例："小明 & 小红 v2" → "小明-小红-v2"。
#[must_use]
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    trimmed.chars().take(64).collect()
}

/// URL 段百分号解码（%XX；非法序列原样保留；'+' 不当空格——路径语义）。
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 长文本截断（字符级；超限附截断标注）。
#[must_use]
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}\n…（超长已截断至 {max} 字符）")
}

/// front-matter 解析：`---\nkey: value\n---\n正文` → (键值表, 正文)。
/// 无 front-matter → 空表 + 原文。值支持双引号包裹（含 ':#' 等字符时写入侧会加引号）。
#[must_use]
pub fn split_front_matter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut map = BTreeMap::new();
    let trimmed = text.trim_start_matches('\u{feff}');
    let rest = trimmed.strip_prefix("---");
    let Some(after) = rest else {
        return (map, text.to_string());
    };
    // 首个换行后到下一个独立 --- 行之间为键值区
    let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
    let fm_area = &after[body_start..];
    let Some(end) = fm_area.find("\n---") else {
        return (map, text.to_string());
    };
    let fm_text = &fm_area[..end];
    let body = &fm_area[end + "\n---".len()..];
    for line in fm_text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key.is_empty() {
                continue;
            }
            let mut val = v.trim();
            if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
                val = &val[1..val.len() - 1];
            }
            map.insert(key.to_string(), val.replace("\\\"", "\""));
        }
    }
    (map, body.trim_start_matches('\n').to_string())
}

/// front-matter 值转义：含冒号/井号/引号/首尾空白的值加双引号。
fn fm_escape(v: &str) -> String {
    if v.is_empty() {
        return "\"\"".to_string();
    }
    let needs = v.contains(':')
        || v.contains('#')
        || v.contains('"')
        || v.starts_with(' ')
        || v.ends_with(' ');
    if needs {
        format!("\"{}\"", v.replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

/// 渲染 front-matter 文档（键序由 BTreeMap 决定，稳定）。
#[must_use]
pub fn render_doc(fm: &BTreeMap<String, String>, body: &str) -> String {
    if fm.is_empty() {
        return body.to_string();
    }
    let mut out = String::from("---\n");
    for (k, v) in fm {
        out.push_str(&format!("{k}: {}\n", fm_escape(v)));
    }
    out.push_str("---\n");
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
    }
    out
}

/// 当前时刻紧凑标签（dist 版本名 final-vYYYYMMDD-HHMM 用）。
#[must_use]
fn now_compact() -> String {
    use chrono::Local;
    Local::now().format("%Y%m%d-%H%M").to_string()
}

// ----------------------------------------------------------------------------
// hub 路径与白名单
// ----------------------------------------------------------------------------

/// 项目 hub 树根（`<dir>/hub`）。
#[must_use]
pub fn hub_root(project: &FilmProject) -> String {
    format!("{}/hub", project.dir.trim_end_matches('/'))
}

/// dist 落点根：export_dir 设置时即 dist 根（语义保留），否则 `<dir>/hub/dist`。
#[must_use]
fn dist_root(project: &FilmProject) -> String {
    match project
        .export_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(ed) => ed.trim_end_matches('/').to_string(),
        None => format!("{}/dist", hub_root(project)),
    }
}

/// hub 相对路径 → 绝对路径（防穿越：拒绝绝对段、`.`/`..` 段与空段）。
fn hub_abs(root: &str, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim().trim_start_matches('/');
    if rel.is_empty() {
        return Err("路径不可为空".to_string());
    }
    let mut out = PathBuf::from(root);
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." || comp.contains('\\') {
            return Err(format!("非法路径段「{comp}」（禁止穿越）"));
        }
        out.push(comp);
    }
    Ok(out)
}

/// 文本扩展名（files 面 text 读/写判定）。
fn is_text_ext(name: &str) -> bool {
    matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "md" | "txt" | "json" | "srt"
    )
}

/// 二进制资产扩展名。
fn is_binary_ext(name: &str) -> bool {
    matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "mp3" | "mp4"
    )
}

/// files GET 白名单：树内全部文本 + 资产二进制（casting 视图 / BGM 音轨 /
/// dist 成片 / cache 半成品）。
fn check_get_path(rel: &str) -> Result<bool, String> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let segs: Vec<&str> = rel.split('/').collect();
    // 二进制资产面
    if segs.len() >= 5 && segs[0] == "casting" && segs[3] == "views" && is_binary_ext(name) {
        return Ok(false); // binary
    }
    if segs.len() == 4 && segs[0] == "audio" && segs[1] == "bgm" && name == "track.mp3" {
        return Ok(false);
    }
    if segs.first() == Some(&"dist") && is_binary_ext(name) {
        return Ok(false);
    }
    if segs.first() == Some(&"cache") && is_binary_ext(name) {
        return Ok(false);
    }
    if is_text_ext(name) {
        return Ok(true); // text
    }
    Err(format!("路径不在 files 读白名单内: {rel}（文本=md/txt/json/srt；二进制=casting 视图/BGM 音轨/dist 成品/cache 半成品）"))
}

/// files PUT 白名单：树内可编辑文本（服务端真值 budget/events 与二进制除外；
/// budget.json 仅允许改 budget_limit，见写入口径）。
fn check_put_path(rel: &str) -> Result<(), String> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let segs: Vec<&str> = rel.split('/').collect();
    if !is_text_ext(name) {
        return Err(format!("files PUT 仅支持文本文件（md/txt/json）: {rel}"));
    }
    if rel == "budget.json" {
        return Ok(()); // 特例：仅 budget_limit 可改（写入口径重建 events）
    }
    if matches!(rel, "assets.json" | "activity.json") {
        return Err(format!("{rel} 为服务端真值清单，不可经 files PUT 改写"));
    }
    if segs.first() == Some(&"cache") || segs.first() == Some(&"dist") {
        return Err(format!("cache/ 与 dist/ 不在 PUT 白名单: {rel}"));
    }
    if segs.len() >= 5 && segs[0] == "casting" && segs[3] == "views" {
        return Err(format!("定妆视图为二进制资产，请走 views/import: {rel}"));
    }
    if segs.len() == 4 && segs[0] == "audio" && segs[1] == "bgm" && name != "info.md" {
        return Err(format!("音轨二进制请走 audio/bgm 导入: {rel}"));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// assets.json / activity.json / ownership.json
// ----------------------------------------------------------------------------

/// assets.json 条目（`ref` 为 Rust 关键字，serde 改名）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssetItem {
    /// 相对 hub 根路径（如 casting/characters/小明/views/front.png）。
    path: String,
    sha256: String,
    bytes: u64,
    /// `ai` / `import`。
    source: String,
    /// 归属对象（如 characters/小明）。
    #[serde(rename = "ref")]
    ref_obj: String,
}

/// 资产登记（按 path 去重更新；失败仅日志——清单是派生面不拦业务）。
async fn register_asset(root: &str, rel: &str, bytes: &[u8], source: &str, ref_obj: &str) {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let sha = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let item = AssetItem {
        path: rel.to_string(),
        sha256: sha,
        bytes: bytes.len() as u64,
        source: source.to_string(),
        ref_obj: ref_obj.to_string(),
    };
    let _guard = HUB_META_LOCK.lock();
    let path = format!("{root}/assets.json");
    let mut items: Vec<AssetItem> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    items.retain(|i| i.path != rel);
    items.push(item);
    items.sort_by(|a, b| a.path.cmp(&b.path));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&items).unwrap_or_default(),
    ) {
        eprintln!("[filmhub] 资产登记失败（{rel}）: {e}");
    }
}

/// activity 追加一条（环形 200；失败仅日志）。
pub(crate) async fn append_activity(root: &str, author: &str, action: &str, target: &str) {
    let author = if author.trim().is_empty() {
        "anonymous"
    } else {
        author.trim()
    };
    let entry = serde_json::json!({
        "ts": now_iso(),
        "author": author,
        "action": action,
        "target": target,
    });
    let _guard = HUB_META_LOCK.lock();
    let path = format!("{root}/activity.json");
    let mut list: Vec<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    list.push(entry);
    if list.len() > ACTIVITY_MAX {
        let cut = list.len() - ACTIVITY_MAX;
        list.drain(0..cut);
    }
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&list).unwrap_or_default(),
    ) {
        eprintln!("[filmhub] activity 追加失败（{action}）: {e}");
    }
}

/// 请求体 author 字段归一（缺省/空白 → "anonymous"）。
#[must_use]
pub fn author_of(raw: &Option<String>) -> String {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("anonymous")
        .to_string()
}

/// ownership.json 读（文件缺失/损坏 → 空骨架）。
fn load_ownership(root: &str) -> Value {
    std::fs::read_to_string(format!("{root}/ownership.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(
            || serde_json::json!({"members": [], "sections": {}, "casting_objects": {}}),
        )
}

/// ownership.json 写回。
fn save_ownership(root: &str, v: &Value) {
    let _ = std::fs::write(
        format!("{root}/ownership.json"),
        serde_json::to_string_pretty(v).unwrap_or_default(),
    );
}

/// 对象级认领写入（key = `<type>/<slug>`；不存在则建骨架）。
fn set_object_claim(root: &str, key: &str, owner: &str) {
    let _guard = HUB_META_LOCK.lock();
    let mut own = load_ownership(root);
    let objects = own
        .as_object_mut()
        .expect("load_ownership 保证对象")
        .entry("casting_objects")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(map) = objects.as_object_mut() {
        map.insert(
            key.to_string(),
            serde_json::json!({"owner": owner, "claimed_at": now_iso()}),
        );
    }
    save_ownership(root, &own);
}

/// 对象级认领迁移（对象改名时把认领键一并迁移）。
fn move_object_claim(root: &str, old_key: &str, new_key: &str) {
    if old_key == new_key {
        return;
    }
    let _guard = HUB_META_LOCK.lock();
    let mut own = load_ownership(root);
    if let Some(map) = own
        .get_mut("casting_objects")
        .and_then(Value::as_object_mut)
    {
        if let Some(v) = map.remove(old_key) {
            map.insert(new_key.to_string(), v);
        }
    }
    save_ownership(root, &own);
}

/// 对象级认领删除。
fn remove_object_claim(root: &str, key: &str) {
    let _guard = HUB_META_LOCK.lock();
    let mut own = load_ownership(root);
    if let Some(map) = own
        .get_mut("casting_objects")
        .and_then(Value::as_object_mut)
    {
        map.remove(key);
    }
    save_ownership(root, &own);
}

/// 查对象的当前认领人（None=未认领）。
fn object_claimer(root: &str, key: &str) -> Option<String> {
    load_ownership(root)
        .get("casting_objects")
        .and_then(|o| o.get(key))
        .and_then(|c| c.get("owner"))
        .and_then(Value::as_str)
        .map(String::from)
}

/// ownership.json 校验（files PUT 入口）：members/sections 枚举与
/// casting_objects 键格式（type 枚举 + slug 名；对象存在性宽容——允许认领
/// extraction 报告中尚未落地的对象）。
fn validate_ownership(v: &Value) -> Result<(), String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "ownership.json 须为 JSON 对象".to_string())?;
    if let Some(members) = obj.get("members") {
        let arr = members
            .as_array()
            .ok_or_else(|| "members 须为数组".to_string())?;
        for m in arr {
            let name = m
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "members[].name 须为非空字符串".to_string())?;
            if name.contains('/') {
                return Err("members[].name 不可含路径分隔符".to_string());
            }
        }
    }
    if let Some(sections) = obj.get("sections") {
        let map = sections
            .as_object()
            .ok_or_else(|| "sections 须为对象".to_string())?;
        for (k, val) in map {
            if !HUB_STAGES.contains(&k.as_str()) {
                return Err(format!("sections 键「{k}」不在枚举 {HUB_STAGES:?} 之内"));
            }
            let owner = val
                .get("owner")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("sections.{k}.owner 须为非空字符串"))?;
            if owner.contains('/') {
                return Err(format!("sections.{k}.owner 不可含路径分隔符"));
            }
        }
    }
    if let Some(objects) = obj.get("casting_objects") {
        let map = objects
            .as_object()
            .ok_or_else(|| "casting_objects 须为对象".to_string())?;
        for (k, val) in map {
            let (ty, name) = k
                .split_once('/')
                .ok_or_else(|| format!("casting_objects 键「{k}」须为 <type>/<name> 路径形态"))?;
            if !CASTING_TYPES.contains(&ty) {
                return Err(format!(
                    "casting_objects 键「{k}」的 type 不在枚举 {CASTING_TYPES:?} 之内"
                ));
            }
            if !is_slug_like(name) {
                return Err(format!(
                    "casting_objects 键「{k}」的 name 须为 slug 形态（字母数字/-/_，≤64 字符）"
                ));
            }
            let owner = val
                .get("owner")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("casting_objects[\"{k}\"].owner 须为非空字符串"))?;
            if owner.contains('/') {
                return Err(format!("casting_objects[\"{k}\"].owner 不可含路径分隔符"));
            }
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 成本记账（film_cost_events；study 方案 §G）
// ----------------------------------------------------------------------------

/// 一次阶段任务的记账规格（完成点由 [`finish_stage`] 统一落事件 + 账本投影）。
pub(crate) struct CostSpec<'a> {
    pub stage: &'a str,
    pub shot: Option<u32>,
    pub model_ref: Option<&'a ModelRef>,
    pub started: Instant,
    pub bytes: u64,
    /// (prompt, completion, total)；local 只有 total（记入 completion，prompt=0）。
    pub tokens: Option<(u32, u32, u32)>,
}

/// 建表幂等（film.rs create_schema 调用）。
pub(crate) fn create_cost_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS film_cost_events (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            task_id TEXT NOT NULL DEFAULT '',
            stage TEXT NOT NULL,
            shot INTEGER,
            source TEXT NOT NULL DEFAULT '',
            channel_id TEXT,
            model TEXT NOT NULL DEFAULT '',
            ok INTEGER NOT NULL DEFAULT 0,
            wall_secs REAL NOT NULL DEFAULT 0,
            bytes_out INTEGER NOT NULL DEFAULT 0,
            prompt_tokens INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            est_cost REAL NOT NULL DEFAULT 0,
            currency TEXT NOT NULL DEFAULT 'CNY',
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_film_cost_events_project
            ON film_cost_events(project_id);",
    )
}

/// est_cost 计价纯函数：per_call + per_sec×wall + per_token×(tokens/1000)
/// （单价缺省 0——未配置只计量不计价，诚实不假装）。
#[must_use]
pub fn est_cost(prices: (f64, f64, f64), wall_secs: f64, tokens: u64) -> f64 {
    let (per_call, per_sec, per_token) = prices;
    let est = per_call + per_sec * wall_secs + per_token * (tokens as f64 / 1000.0);
    (est * 1e6).round() / 1e6
}

/// 落一条成本事件 + budget.json 投影重建，然后任务收尾（done/error 同口径，
/// error 也记——ok=false，est_cost 按 0）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_stage(
    ctx: &FilmCtx,
    tasks: &Arc<std::sync::Mutex<std::collections::HashMap<String, FilmTask>>>,
    task_id: &str,
    project: &FilmProject,
    status: &str,
    line: &str,
    output: Option<String>,
    cost: CostSpec<'_>,
) {
    let ok = status == "done";
    let wall = cost.started.elapsed().as_secs_f64();
    let (source, channel_id, model) = match cost.model_ref {
        Some(mr) => (
            mr.source.clone(),
            mr.channel_id.clone().filter(|c| !c.trim().is_empty()),
            mr.model
                .clone()
                .filter(|m| !m.trim().is_empty())
                .or_else(|| {
                    // channel 缺省模型名（渠道 models[0]）在转发时已解析；此处尽力补
                    mr.channel_id
                        .as_deref()
                        .and_then(|cid| ctx.channel_model_of(cid))
                }),
        ),
        None => (String::new(), None, None), // compose：本地 ffmpeg 无外部模型
    };
    let tokens = cost.tokens.unwrap_or((0, 0, 0));
    let prices = channel_id
        .as_deref()
        .and_then(|cid| ctx.channel_prices(cid))
        .unwrap_or((0.0, 0.0, 0.0));
    let total_tokens = if tokens.2 > 0 {
        u64::from(tokens.2)
    } else {
        u64::from(tokens.0) + u64::from(tokens.1)
    };
    let est = if ok {
        est_cost(prices, wall, total_tokens)
    } else {
        0.0
    };
    let id = format!(
        "fce-{}",
        COST_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    );
    let created_at = now_iso();
    if let Ok(conn) = ctx.db.lock() {
        if let Err(e) = conn.execute(
            "INSERT INTO film_cost_events
             (id,project_id,task_id,stage,shot,source,channel_id,model,ok,
              wall_secs,bytes_out,prompt_tokens,completion_tokens,est_cost,currency,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                id,
                project.id,
                task_id,
                cost.stage,
                cost.shot.map(u64::from),
                source,
                channel_id,
                model.unwrap_or_default(),
                if ok { 1 } else { 0 },
                wall,
                cost.bytes as i64,
                tokens.0,
                tokens.1,
                est,
                "CNY",
                created_at
            ],
        ) {
            eprintln!(
                "[filmhub] 成本事件落库失败（{} {}/{}）: {e}",
                project.id, cost.stage, task_id
            );
        } else {
            rewrite_budget(&conn, project);
        }
    }
    task_finish(tasks, task_id, status, line, output);
}

/// budget.json 重建（DB 为真值 → 树内投影；budget_limit 从现有文件保留）。
fn rewrite_budget(conn: &Connection, project: &FilmProject) {
    let root = hub_root(project);
    let path = format!("{root}/budget.json");
    let limit: Option<f64> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("budget_limit").and_then(Value::as_f64));
    let events: Vec<Value> = conn
        .prepare(
            "SELECT id,task_id,stage,shot,source,channel_id,model,ok,wall_secs,
                    bytes_out,prompt_tokens,completion_tokens,est_cost,currency,created_at
             FROM film_cost_events WHERE project_id=?1 ORDER BY created_at,id",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map(params![project.id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "task_id": r.get::<_, String>(1)?,
                    "stage": r.get::<_, String>(2)?,
                    "shot": r.get::<_, Option<u64>>(3)?,
                    "source": r.get::<_, String>(4)?,
                    "channel_id": r.get::<_, Option<String>>(5)?,
                    "model": r.get::<_, String>(6)?,
                    "ok": r.get::<_, i64>(7)? != 0,
                    "wall_secs": r.get::<_, f64>(8)?,
                    "bytes_out": r.get::<_, i64>(9)?,
                    "prompt_tokens": r.get::<_, i64>(10)?,
                    "completion_tokens": r.get::<_, i64>(11)?,
                    "est_cost": r.get::<_, f64>(12)?,
                    "currency": r.get::<_, String>(13)?,
                    "created_at": r.get::<_, String>(14)?,
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();
    let doc = serde_json::json!({
        "version": 1,
        "currency": "CNY",
        "budget_limit": limit,
        "events": events,
    });
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&doc).unwrap_or_default(),
    ) {
        eprintln!("[filmhub] budget.json 重建失败（{}）: {e}", project.id);
    }
}

/// 成本事件行（聚合端点用）。
struct CostRow {
    stage: String,
    channel_id: Option<String>,
    est_cost: f64,
    wall_secs: f64,
    bytes: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    created_at: String,
}

fn load_cost_rows(conn: &Connection, project_id: &str) -> Vec<CostRow> {
    conn.prepare(
        "SELECT stage,channel_id,est_cost,wall_secs,bytes_out,prompt_tokens,completion_tokens,created_at
         FROM film_cost_events WHERE project_id=?1 ORDER BY created_at,id",
    )
    .and_then(|mut stmt| {
        let rows = stmt.query_map(params![project_id], |r| {
            Ok(CostRow {
                stage: r.get(0)?,
                channel_id: r.get(1)?,
                est_cost: r.get(2)?,
                wall_secs: r.get(3)?,
                bytes: r.get(4)?,
                prompt_tokens: r.get(5)?,
                completion_tokens: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// GET /film/projects/:id/cost?by=stage|channel|day 聚合。
fn cost_summary(conn: &Connection, project: &FilmProject, by: &str) -> Result<Value, String> {
    if !matches!(by, "stage" | "channel" | "day") {
        return Err(format!("by 须为 stage|channel|day（当前 {by}）"));
    }
    let rows = load_cost_rows(conn, &project.id);
    let limit: Option<f64> = std::fs::read_to_string(format!("{}/budget.json", hub_root(project)))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("budget_limit").and_then(Value::as_f64));
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, (f64, u64, f64, i64, i64)> = BTreeMap::new();
    let (mut total, mut t_wall, mut t_bytes, mut t_tokens) = (0.0, 0.0, 0i64, 0i64);
    for r in &rows {
        let key = match by {
            "channel" => r.channel_id.clone().unwrap_or_else(|| "local".to_string()),
            "day" => r.created_at[..10.min(r.created_at.len())].to_string(),
            _ => r.stage.clone(),
        };
        let e = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            (0.0, 0, 0.0, 0, 0)
        });
        e.0 += r.est_cost;
        e.1 += 1;
        e.2 += r.wall_secs;
        e.3 += r.bytes;
        e.4 += r.prompt_tokens + r.completion_tokens;
        total += r.est_cost;
        t_wall += r.wall_secs;
        t_bytes += r.bytes;
        t_tokens += r.prompt_tokens + r.completion_tokens;
    }
    let groups: Vec<Value> = order
        .into_iter()
        .map(|k| {
            let g = &groups[&k];
            serde_json::json!({
                "key": k,
                "cost": (g.0 * 1e6).round() / 1e6,
                "events": g.1,
                "wall_secs": (g.2 * 100.0).round() / 100.0,
                "bytes": g.3,
                "tokens": g.4,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "total": (total * 1e6).round() / 1e6,
        "currency": "CNY",
        "events": rows.len(),
        "limit": limit,
        "totals": {
            "wall_secs": (t_wall * 100.0).round() / 100.0,
            "bytes": t_bytes,
            "tokens": t_tokens,
        },
        "groups": groups,
    }))
}

// ----------------------------------------------------------------------------
// 提示词三份（剧情 / 分镜 / 提取——冻结契约原文见 docs/FILM_STUDIO.md）
// ----------------------------------------------------------------------------

/// 剧情（story generate）user 提示词。题材硬约束同款（首尾夹逼）；
/// source 给定时切换「基于原文改编浓缩」分支。
#[must_use]
pub fn build_story_prompt(
    idea: &str,
    ratio: &str,
    style_hint: Option<&str>,
    source_text: Option<&str>,
) -> String {
    let style = style_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("电影感、自然光影");
    let (source_block, req1) = match source_text {
        Some(src) => (
            format!(
                "【改编原文】请基于以下导入原文改编浓缩为分幕剧本（保留主线与关键情节，收敛为 3 到 6 幕）：\n{}\n",
                truncate_chars(src, STORY_PROMPT_MAX_CHARS)
            ),
            "1. 在改编浓缩的前提下输出分幕剧本：以「【第一幕】」「【第二幕】」……为幕标题，每幕写明场景、人物动作与关键对白，共 3 到 6 幕。".to_string(),
        ),
        None => (
            String::new(),
            "1. 输出分幕剧本：以「【第一幕】」「【第二幕】」……为幕标题，每幕写明场景、人物动作与关键对白，共 3 到 6 幕。".to_string(),
        ),
    };
    format!(
        "请为下面的影片创意创作剧情正稿（分幕剧本文本）。\n\
         【创意】{idea}\n\
         【画幅】{ratio}\n\
         【风格提示】{style}\n\
         {source_block}\
         要求：\n\
         {req1}\n\
         2. 必须严格围绕【创意】的故事创作：禁止更换题材、禁止另编与【创意】无关的故事；每一幕都必须直接服务于该创意的叙事。\n\
         3. 只输出分幕剧本正文（不要 markdown 代码块标记、不要任何解释文字）；正文中反复出现的人物、武器、宠物、场景与动作请使用稳定统一的名字（后续定妆阶段将按这些名字建定妆对象）。\n\
         最后再强调一次：所有幕必须讲【创意】本身的故事——【创意】是：{idea}"
    )
}

/// 剧情 system 提示词（题材硬约束同款）。
#[must_use]
pub fn story_system_prompt() -> &'static str {
    "你是专业的短片编剧。必须严格围绕用户给出的【创意】创作，禁止更换题材、\
禁止另编与【创意】无关的故事。只输出剧本正文，不要任何解释文字。"
}

/// 分镜（storyboard generate）user 提示词：从剧情逐幕分析；casting 空槽字段
/// 按名引用定妆对象；无剧情时回落【创意】（旧 script 兼容别名同款锚定）。
/// roster（旧角色库 film_characters）非空时注入【角色表】段（characters 须从表选）。
#[must_use]
pub fn build_storyboard_prompt(
    story: Option<&str>,
    idea: &str,
    ratio: &str,
    style_hint: Option<&str>,
    roster_entries: &[String],
) -> String {
    let style = style_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("电影感、自然光影");
    let (label, content, anchor) = match story {
        Some(s) => {
            let body = truncate_chars(s, STORYBOARD_PROMPT_MAX_CHARS);
            let head: String = s.trim().chars().take(80).collect();
            ("剧情", body, head)
        }
        None => ("创意", idea.to_string(), idea.to_string()),
    };
    let roster = if roster_entries.is_empty() {
        String::new()
    } else {
        let list = roster_entries.join("\n");
        format!("\n【角色表】（分镜 characters 字段只能从下列角色名中选取）：\n{list}\n")
    };
    format!(
        "请为下面的影片{label}生成分镜脚本。\n\
         【{label}】{content}\n\
         【画幅】{ratio}\n\
         【风格提示】{style}\n\
         要求：\n\
         1. 从{label}逐幕分析，输出 5 到 12 个镜头，按叙事顺序（幕结构优先对齐：一幕可拆多个镜头，不要跳幕）。\n\
         2. 必须严格围绕【{label}】的故事创作分镜：禁止更换题材、禁止另编与【{label}】无关的故事；每个镜头的画面都必须直接服务于该创意的叙事。\n\
         3. 只输出一个 JSON 数组，不要任何解释文字或 markdown 代码块标记。每个元素形如：\n\
         {{\"shot\":1,\"desc\":\"画面描述\",\"image_prompt\":\"关键帧生图提示词（含风格与 {ratio} 构图信息）\",\"video_prompt\":\"图生视频运动与镜头语言提示词\",\"line\":\"角色台词，无台词则为空字符串\",\"duration_secs\":5,\"characters\":[\"出场人物名\"],\"props\":[\"武器或重要道具名\"],\"pets\":[\"宠物名\"],\"scenes\":[\"场景名\"],\"actions\":[\"高频动作名\"]}}\n\
         4. duration_secs 取 2-10 的整数。\n\
         5. casting 引用字段（characters/props/pets/scenes/actions）按名字引用{label}中反复出现的人物、武器、宠物、场景与动作——这些名字将在定妆阶段建定妆对象；{label}中没有的类别输出空数组，不要编造{label}里不存在的对象。\n\
         {roster}最后再强调一次：所有镜头必须讲【{label}】本身的故事——【{label}】是：{anchor}"
    )
}

/// 分镜 system 提示词（与旧 script 阶段同款题材硬约束）。
#[must_use]
pub fn storyboard_system_prompt() -> &'static str {
    "你是专业的短片分镜师。必须严格围绕用户给出的【剧情】创作，\
禁止更换题材、禁止另编与【剧情】无关的故事。只输出 JSON，不要任何解释文字。"
}

/// 分镜解析失败后的一次重试提示词（casting 字段同契约）。
#[must_use]
pub fn build_storyboard_retry_prompt(story: Option<&str>, idea: &str) -> String {
    let (label, anchor) = match story {
        Some(s) => ("剧情", s.trim().chars().take(80).collect::<String>()),
        None => ("创意", idea.to_string()),
    };
    format!(
        "你上一次的输出无法解析为 JSON 数组。请重新输出分镜 JSON 数组本体：\
只输出一个 JSON 数组（以 [ 开头、以 ] 结尾），元素字段为 \
shot/desc/image_prompt/video_prompt/line/duration_secs 与 casting 引用字段 \
characters/props/pets/scenes/actions（按名引用{label}中反复出现的对象，没有的类别空数组），\
不要 markdown 标记、不要任何解释文字。\
必须严格围绕【{label}】的故事创作分镜：禁止更换题材、禁止另编与【{label}】\
无关的故事；每个镜头的画面都必须直接服务于该创意的叙事。\
【{label}】是：{anchor}"
    )
}

/// 提取（casting extract）user 提示词：六类定义 + frequency 统计要求 + 严格 JSON。
#[must_use]
pub fn build_extract_prompt(story: &str, shots: &[ScriptShot]) -> String {
    let shots_compact: Vec<Value> = shots
        .iter()
        .map(|s| {
            serde_json::json!({
                "shot": s.shot,
                "desc": s.desc,
                "characters": s.characters,
                "props": s.props,
                "pets": s.pets,
                "scenes": s.scenes,
                "actions": s.actions,
            })
        })
        .collect();
    let shots_text = truncate_chars(
        &serde_json::to_string(&shots_compact).unwrap_or_default(),
        EXTRACT_PROMPT_MAX_CHARS,
    );
    format!(
        "请从下面的影片剧情与分镜中提取「定妆对象」清单（六类）。\n\
         【剧情】{}\n\
         【分镜】{}\n\
         六类定义（key 固定）：\n\
         - characters：出场人物（主角与重要配角，按名字）\n\
         - weapons：人物使用的武器 / 关键道具（按名字）\n\
         - pets：出场的宠物 / 动物（按名字）\n\
         - formations：多人同框的站位排列（如「三人前二后一」，按排列名）\n\
         - actions：跨镜头重复出现的高频动作（如「拔剑」「雨中奔跑」）\n\
         - scenes：跨镜头重复出现的高频场景（按场景名，如「灯塔顶」）\n\
         要求：\n\
         1. 只输出一个 JSON 对象（以 {{ 开头、以 }} 结尾），不要 markdown 代码块标记或解释文字，形如：\n\
         {{\"characters\":[{{\"name\":\"名字\",\"desc\":\"外形与设定描述（供定妆图与一致性注入用）\",\"frequency\":3,\"reason\":\"为何需要定妆\"}}],\"weapons\":[…],\"pets\":[…],\"formations\":[…],\"actions\":[…],\"scenes\":[…]}}\n\
         2. frequency 必须是整数 = 该对象出场的镜头数（对照【分镜】逐镜头统计；仅在剧情出现而分镜未覆盖的按 0）。\n\
         3. 每类按 frequency 降序；剧情中不存在的类别给空数组；名字须与剧情/分镜中使用的名字一致。",
        truncate_chars(story, EXTRACT_PROMPT_MAX_CHARS),
        shots_text
    )
}

/// 提取 system 提示词。
#[must_use]
pub fn extract_system_prompt() -> &'static str {
    "你是专业的影片定妆统筹。只输出 JSON 对象，不要任何解释文字。"
}

// ----------------------------------------------------------------------------
// 提取结果解析（容错）
// ----------------------------------------------------------------------------

/// 从 LLM 输出提取候选 JSON **对象**片段（原文 / ``` 围栏块 / 首尾大括号）。
fn json_object_candidates(text: &str) -> Vec<String> {
    let mut out = vec![];
    let trimmed = text.trim();
    out.push(trimmed.to_string());
    let mut rest = trimmed;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(body_from) = after.find('\n').map(|i| i + 1) else {
            break;
        };
        let Some(end) = after[body_from..].find("```") else {
            break;
        };
        out.push(after[body_from..body_from + end].trim().to_string());
        rest = &after[body_from + end + 3..];
    }
    if let (Some(a), Some(b)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if a < b {
            out.push(trimmed[a..=b].to_string());
        }
    }
    out
}

/// 解析提取报告为规范化六类 JSON（characters|weapons|pets|formations|actions|
/// scenes → [{name,desc,frequency,reason}]；字段缺省容错；名空丢弃）。
pub fn parse_extraction(text: &str) -> Result<Value, String> {
    let stripped = super::film::strip_think_blocks(text);
    for cand in json_object_candidates(&stripped) {
        let Ok(v) = serde_json::from_str::<Value>(&cand) else {
            continue;
        };
        let Some(obj) = v.as_object() else {
            continue;
        };
        let mut out = serde_json::Map::new();
        for key in [
            "characters",
            "weapons",
            "pets",
            "formations",
            "actions",
            "scenes",
        ] {
            let mut items: Vec<Value> = Vec::new();
            if let Some(arr) = obj.get(key).and_then(Value::as_array) {
                for it in arr {
                    let Some(name) = it
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let frequency = it.get("frequency").and_then(|f| match f {
                        Value::Number(n) => n.as_u64(),
                        Value::String(s) => s.trim().trim_end_matches('次').parse::<u64>().ok(),
                        _ => None,
                    });
                    items.push(serde_json::json!({
                        "name": name,
                        "desc": it.get("desc").and_then(Value::as_str).unwrap_or("").trim(),
                        "frequency": frequency.unwrap_or(0),
                        "reason": it.get("reason").and_then(Value::as_str).unwrap_or("").trim(),
                    }));
                }
            }
            out.insert(key.to_string(), Value::Array(items));
        }
        if out
            .values()
            .all(|v| v.as_array().is_some_and(Vec::is_empty))
        {
            continue; // 全空视为解析失败（换下一候选）
        }
        return Ok(Value::Object(out));
    }
    Err("无法从 LLM 输出解析出六类定妆对象 JSON 对象".to_string())
}

// ----------------------------------------------------------------------------
// hub 树初始化 / 导出（惰性 export）
// ----------------------------------------------------------------------------

/// hub 树目录集（初始化/惰性建）。
async fn create_hub_dirs(root: &str) -> Result<(), String> {
    let mut dirs: Vec<String> = vec![
        format!("{root}/story"),
        format!("{root}/storyboard"),
        format!("{root}/cache"),
        format!("{root}/dist"),
        format!("{root}/audio/bgm"),
    ];
    for t in CASTING_TYPES {
        dirs.push(format!("{root}/casting/{t}"));
    }
    for d in dirs {
        tokio::fs::create_dir_all(&d)
            .await
            .map_err(|e| format!("建 hub 目录失败 {d}: {e}"))?;
    }
    Ok(())
}

/// project.md 内容（front-matter: title/ratio/style_hint/export_dir；正文=idea）。
fn project_md_content(project: &FilmProject) -> String {
    let mut fm = BTreeMap::new();
    fm.insert("title".to_string(), project.title.clone());
    fm.insert("ratio".to_string(), project.ratio.clone());
    if let Some(s) = project
        .style_hint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        fm.insert("style_hint".to_string(), s.to_string());
    }
    if let Some(e) = project
        .export_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        fm.insert("export_dir".to_string(), e.to_string());
    }
    render_doc(&fm, &project.idea)
}

/// README 阶段中文名。
fn stage_label(stage: &str) -> &'static str {
    match stage {
        "story" => "剧情",
        "storyboard" => "分镜",
        "casting" => "定妆",
        "audio" => "音频",
        "compose" => "合成",
        _ => "未知",
    }
}

/// README.md 内容（front-matter: stage/progress；AI agent 入口）。
fn readme_content(stage: &str, progress: &str) -> String {
    let mut fm = BTreeMap::new();
    fm.insert("stage".to_string(), stage.to_string());
    fm.insert("progress".to_string(), progress.to_string());
    render_doc(
        &fm,
        &format!(
            "# FilmHub 项目\n\n- 当前阶段：{stage}（{}）\n- 进度：{progress}\n\n阶段序：story → storyboard → casting → audio → compose。\n详细契约见 docs/FILM_STUDIO.md。\n",
            stage_label(stage)
        ),
    )
}

/// 更新 README 阶段（保留既有 progress 缺省文案）。
async fn set_readme_stage(root: &str, stage: &str, progress: &str) {
    let _ = tokio::fs::write(format!("{root}/README.md"), readme_content(stage, progress)).await;
}

/// 项目状态 → 初始 README 阶段（老项目惰性初始化的起步推断）。
fn status_to_stage(status: &str) -> &'static str {
    match status {
        "scripted" => "storyboard",
        "producing" => "audio",
        "done" => "compose",
        _ => "story",
    }
}

/// storyboard.json 内容（version + ScriptShot 全字段 + 生成元信息）。
fn storyboard_json(shots: &[ScriptShot], generated_by: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "shots": shots,
        "generated_by": generated_by,
        "created_at": now_iso(),
    }))
    .unwrap_or_default()
}

/// script.json 镜像内容（画布/下游阶段读的既有形态，零翻译）。
fn script_json(shots: &[ScriptShot], generated_by: &str) -> String {
    serde_json::to_string_pretty(&super::film::ScriptFile {
        shots: shots.to_vec(),
        generated_by: generated_by.to_string(),
        created_at: now_iso(),
    })
    .unwrap_or_default()
}

/// 导出内核：项目 DB 状态 → hub 树文本（story/casting/audio/ownership/activity/
/// assets **原样保留**——文件是它们的真值；storyboard 按 mtime 新者覆盖）。
/// 返回写入文件列表（hub 相对路径）。
async fn export_inner(ctx: &FilmCtx, project: &FilmProject) -> Result<Vec<String>, String> {
    let root = hub_root(project);
    let mut written: Vec<String> = Vec::new();
    // project.md
    let pm = format!("{root}/project.md");
    tokio::fs::write(&pm, project_md_content(project))
        .await
        .map_err(|e| format!("写 project.md 失败: {e}"))?;
    written.push("project.md".to_string());
    // README：已存在则保留（stage 真值在文件），缺失按项目状态起步
    if !Path::new(&format!("{root}/README.md")).is_file() {
        let stage = status_to_stage(&project.status);
        set_readme_stage(&root, stage, "项目已初始化").await;
        written.push("README.md".to_string());
    }
    // storyboard：script.json（画布状态）比树内新才覆盖（PUT 手改不被冲掉）
    let script_path = format!("{}/script.json", project.dir);
    let sb_path = format!("{root}/storyboard/storyboard.json");
    let script_mtime = std::fs::metadata(&script_path)
        .and_then(|m| m.modified())
        .ok();
    let sb_mtime = std::fs::metadata(&sb_path).and_then(|m| m.modified()).ok();
    let sb_stale = match (script_mtime, sb_mtime) {
        (Some(s), Some(b)) => s > b,
        (Some(_), None) => true,
        _ => false,
    };
    if sb_stale {
        if let Ok(shots) = read_script(project).await {
            let pretty = storyboard_json(&shots, "export（项目状态）");
            tokio::fs::write(&sb_path, pretty)
                .await
                .map_err(|e| format!("写 storyboard.json 失败: {e}"))?;
            written.push("storyboard/storyboard.json".to_string());
        }
    }
    // budget.json：DB 真值投影（保留现有 budget_limit）
    if let Ok(conn) = ctx.db.lock() {
        rewrite_budget(&conn, project);
        written.push("budget.json".to_string());
    }
    Ok(written)
}

/// 惰性初始化：hub/project.md 缺失时建全树（含旧 film_characters 迁移为
/// casting 定妆对象）。已初始化则仅补缺失目录（幂等）。
pub(crate) async fn ensure_hub(ctx: &FilmCtx, project: &FilmProject) -> Result<String, String> {
    let root = hub_root(project);
    if Path::new(&format!("{root}/project.md")).is_file() {
        create_hub_dirs(&root).await?;
        return Ok(root);
    }
    eprintln!("[filmhub] 惰性初始化 hub 树：{}", project.id);
    create_hub_dirs(&root).await?;
    // 骨架元文件（ownership/activity/assets）
    let _guard = HUB_META_LOCK.lock();
    if !Path::new(&format!("{root}/ownership.json")).is_file() {
        save_ownership(
            &root,
            &serde_json::json!({"members": [], "sections": {}, "casting_objects": {}}),
        );
    }
    if !Path::new(&format!("{root}/activity.json")).is_file() {
        let _ = std::fs::write(format!("{root}/activity.json"), "[]");
    }
    if !Path::new(&format!("{root}/assets.json")).is_file() {
        let _ = std::fs::write(format!("{root}/assets.json"), "[]");
    }
    drop(_guard);
    // 旧角色库迁移（一次性：card.md + 定妆图 → views/front）
    let roster = {
        let conn = ctx.db.lock().expect("film db poisoned");
        load_characters(&conn, &project.id)
    };
    for c in &roster {
        let slug = slugify(&c.name);
        if slug.is_empty() {
            continue;
        }
        let cdir = format!("{root}/casting/characters/{slug}");
        let _ = tokio::fs::create_dir_all(format!("{cdir}/views")).await;
        let mut fm = BTreeMap::new();
        fm.insert("name".to_string(), c.name.clone());
        if let Some(v) = c.voice.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            fm.insert("voice".to_string(), v.to_string());
        }
        if let Some(pref) = c
            .portrait_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let src = format!("{}/{}", project.dir.trim_end_matches('/'), pref);
            if let Ok(bytes) = tokio::fs::read(&src).await {
                let ext = sniff_image_ext(&bytes).unwrap_or("png");
                let vrel = format!("casting/characters/{slug}/views/front.{ext}");
                let _ = tokio::fs::write(format!("{root}/{vrel}"), &bytes).await;
                register_asset(
                    &root,
                    &vrel,
                    &bytes,
                    "import",
                    &format!("characters/{slug}"),
                )
                .await;
                fm.insert("portrait".to_string(), format!("views/front.{ext}"));
            }
        }
        let _ = tokio::fs::write(format!("{cdir}/card.md"), render_doc(&fm, &c.description)).await;
    }
    export_inner(ctx, project).await?;
    Ok(root)
}

/// 新建项目即建 hub 树（POST /film/projects 调用；失败仅日志不拦建项）。
pub(crate) async fn init_hub_for_new(project: &FilmProject) {
    let root = hub_root(project);
    if let Err(e) = create_hub_dirs(&root).await {
        eprintln!("[filmhub] 新项目 hub 目录初始化失败（{}）: {e}", project.id);
        return;
    }
    let _ = tokio::fs::write(format!("{root}/project.md"), project_md_content(project)).await;
    set_readme_stage(&root, "story", "项目已创建，待剧情导入或 AI 撰写").await;
    let _guard = HUB_META_LOCK.lock();
    save_ownership(
        &root,
        &serde_json::json!({"members": [], "sections": {}, "casting_objects": {}}),
    );
    let _ = std::fs::write(format!("{root}/activity.json"), "[]");
    let _ = std::fs::write(format!("{root}/assets.json"), "[]");
    let _ = std::fs::write(
        format!("{root}/budget.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1, "currency": "CNY", "budget_limit": null, "events": []
        }))
        .unwrap_or_default(),
    );
}

// ----------------------------------------------------------------------------
// 请求体 DTO（写端点统一可带 author）
// ----------------------------------------------------------------------------

/// story 导入（b64 JSON 信封——网关无 multipart 通道）。
#[derive(Debug, Deserialize)]
struct StoryImportBody {
    filename: String,
    content_b64: String,
    #[serde(default)]
    author: Option<String>,
}

/// story / storyboard / extract / 视图 / BGM 生成的公共面。
#[derive(Debug, Deserialize)]
struct StoryGenBody {
    model_ref: ModelRef,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    source_file: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StoryboardGenBody {
    model_ref: ModelRef,
    #[serde(default)]
    author: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtractBody {
    model_ref: ModelRef,
    #[serde(default)]
    author: Option<String>,
}

/// 定妆对象建（name+desc 必填；author 触发对象级自动认领）。
#[derive(Debug, Deserialize)]
struct CastingCreateBody {
    name: String,
    desc: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// 定妆对象改（部分更新；voice 空串=清空）。
#[derive(Debug, Deserialize)]
struct CastingUpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    portrait: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// 定妆视图生成。
#[derive(Debug, Deserialize)]
struct ViewGenBody {
    model_ref: ModelRef,
    view: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// 定妆视图导入。
#[derive(Debug, Deserialize)]
struct ViewImportBody {
    image_b64: String,
    view: String,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// BGM 建条目/导入（track_b64 省略=先建条目）。
#[derive(Debug, Deserialize)]
struct BgmCreateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    info: Option<BgmInfo>,
    #[serde(default)]
    track_b64: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// BGM info 面（info.md front-matter）。
#[derive(Debug, Default, Deserialize)]
struct BgmInfo {
    /// `global` / `scene:<场景名>`。
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default)]
    mood: Option<String>,
    /// 时长（秒）。
    #[serde(default)]
    duration: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BgmGenBody {
    model_ref: ModelRef,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// files PUT。
#[derive(Debug, Deserialize)]
struct FilesPutBody {
    content: String,
    #[serde(default)]
    author: Option<String>,
}

/// export（无必填字段）。
#[derive(Debug, Default, Deserialize)]
struct ExportBody {
    #[serde(default)]
    author: Option<String>,
}

// ----------------------------------------------------------------------------
// 定妆对象（card.md / views）读写
// ----------------------------------------------------------------------------

/// 定妆对象卡（card.md 解析态）。
#[derive(Debug, Clone, Default)]
struct CastingCard {
    name: String,
    voice: Option<String>,
    portrait: Option<String>,
    desc: String,
}

/// 读定妆对象卡（目录缺失 → None）。
fn read_card(root: &str, ctype: &str, slug: &str) -> Option<CastingCard> {
    let path = format!("{root}/casting/{ctype}/{slug}/card.md");
    let text = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_front_matter(&text);
    Some(CastingCard {
        name: fm.get("name").cloned().unwrap_or_else(|| slug.to_string()),
        voice: fm.get("voice").filter(|v| !v.is_empty()).cloned(),
        portrait: fm.get("portrait").filter(|v| !v.is_empty()).cloned(),
        desc: body.trim().to_string(),
    })
}

/// 写定妆对象卡。
fn write_card(root: &str, ctype: &str, slug: &str, card: &CastingCard) -> Result<(), String> {
    let mut fm = BTreeMap::new();
    fm.insert("name".to_string(), card.name.clone());
    if let Some(v) = card
        .voice
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        fm.insert("voice".to_string(), v.to_string());
    }
    if let Some(p) = card
        .portrait
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        fm.insert("portrait".to_string(), p.to_string());
    }
    let dir = format!("{root}/casting/{ctype}/{slug}");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建定妆目录失败 {dir}: {e}"))?;
    std::fs::write(format!("{dir}/card.md"), render_doc(&fm, card.desc.trim()))
        .map_err(|e| format!("写 card.md 失败: {e}"))
}

/// 列某类定妆对象目录（slug 排序）。
fn list_casting_slugs(root: &str, ctype: &str) -> Vec<String> {
    let dir = format!("{root}/casting/{ctype}");
    let mut out: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 定妆对象视图清单（views/ 下文件）。
fn list_views(root: &str, ctype: &str, slug: &str) -> Vec<(String, String, u64)> {
    let dir = format!("{root}/casting/{ctype}/{slug}/views");
    let mut out: Vec<(String, String, u64)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    if !meta.is_file() {
                        return None;
                    }
                    let file = e.file_name().to_string_lossy().into_owned();
                    let stem = file.rsplit_once('.').map(|(s, _)| s.to_string())?;
                    Some((stem, file, meta.len()))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 收集对象既有视图 b64（channel 参考注入：跨视图严格一致外形）。
async fn collect_view_refs(
    root: &str,
    ctype: &str,
    slug: &str,
    log: &(dyn Fn(String) + Sync),
) -> Vec<String> {
    use base64::Engine;
    let mut out = Vec::new();
    for (stem, file, _) in list_views(root, ctype, slug) {
        if out.len() >= 4 {
            break;
        }
        match tokio::fs::read(format!("{root}/casting/{ctype}/{slug}/views/{file}")).await {
            Ok(b) => {
                out.push((stem, base64::engine::general_purpose::STANDARD.encode(b)));
            }
            Err(e) => log(format!("视图 {stem} 读取失败: {e}（跳过参考注入）")),
        }
    }
    out.into_iter().map(|(_, b)| b).collect()
}

/// 定妆视图生成缺省提示词（人物类冻结模板；其余类别同构措辞）。
#[must_use]
pub fn default_view_prompt(ctype: &str, desc: &str, view: &str) -> String {
    if ctype == "characters" {
        format!("同一定妆对象的多视图：{desc}，{view} 视图，严格一致外形")
    } else {
        format!("定妆对象（{ctype}）的 {view} 视图定妆图：{desc}，严格一致外形")
    }
}

// ----------------------------------------------------------------------------
// BGM（audio/bgm/<track>）读写
// ----------------------------------------------------------------------------

/// BGM 条目视图（GET 列表元素）。
fn read_bgm_entry(root: &str, track: &str) -> Option<Value> {
    let dir = format!("{root}/audio/bgm/{track}");
    if !Path::new(&dir).is_dir() {
        return None;
    }
    let (fm, body) = std::fs::read_to_string(format!("{dir}/info.md"))
        .ok()
        .map(|t| split_front_matter(&t))
        .unwrap_or_default();
    let trigger = fm
        .get("trigger")
        .cloned()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "global".to_string());
    let has_track = Path::new(&format!("{dir}/track.mp3")).is_file();
    let bytes = std::fs::metadata(format!("{dir}/track.mp3"))
        .map(|m| m.len())
        .unwrap_or(0);
    Some(serde_json::json!({
        "track": track,
        "trigger": trigger,
        "mood": fm.get("mood").cloned().unwrap_or_default(),
        "duration": fm.get("duration").and_then(|d| d.parse::<u64>().ok()),
        "note": body.trim(),
        "has_track": has_track,
        "bytes": bytes,
    }))
}

/// BGM 音轨清单（track 名排序）。
fn list_bgm_tracks(root: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(format!("{root}/audio/bgm"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 写 BGM info.md。
fn write_bgm_info(dir: &str, trigger: &str, mood: Option<&str>, duration: Option<u64>) {
    let mut fm = BTreeMap::new();
    fm.insert("trigger".to_string(), trigger.to_string());
    if let Some(m) = mood.map(str::trim).filter(|s| !s.is_empty()) {
        fm.insert("mood".to_string(), m.to_string());
    }
    if let Some(d) = duration {
        fm.insert("duration".to_string(), d.to_string());
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(format!("{dir}/info.md"), render_doc(&fm, ""));
}

/// trigger 校验（global | scene:<场景名>）。
fn validate_trigger(trigger: &str) -> Result<String, String> {
    let t = trigger.trim();
    if t == "global" {
        return Ok("global".to_string());
    }
    if let Some(scene) = t.strip_prefix("scene:") {
        let s = scene.trim();
        if !s.is_empty() && !s.contains('/') {
            return Ok(format!("scene:{s}"));
        }
    }
    Err(format!("trigger 须为 global 或 scene:<场景名>（当前 {t}）"))
}

/// compose 的 BGM 输入解析：body 指定 track → 该音轨 mp3；缺省 trigger=global
/// 优先 → 旧 bgm.mp3 兜底。返回（ffmpeg 输入参数相对路径, 音轨名）。
#[must_use]
pub fn select_bgm_input(
    project: &FilmProject,
    pick: Option<&str>,
) -> (Option<String>, Option<String>) {
    let root = hub_root(project);
    if let Some(t) = pick.map(str::trim).filter(|s| !s.is_empty()) {
        if read_bgm_entry(&root, t).is_some_and(|e| e["has_track"].as_bool().unwrap_or(false)) {
            return (
                Some(format!("hub/audio/bgm/{t}/track.mp3")),
                Some(t.to_string()),
            );
        }
        return (None, None);
    }
    for t in list_bgm_tracks(&root) {
        if let Some(e) = read_bgm_entry(&root, &t) {
            if e["trigger"] == "global" && e["has_track"].as_bool().unwrap_or(false) {
                return (
                    Some(format!("hub/audio/bgm/{t}/track.mp3")),
                    Some(t.clone()),
                );
            }
        }
    }
    if Path::new(&format!("{}/bgm.mp3", project.dir)).is_file() {
        return (Some("bgm.mp3".to_string()), None);
    }
    (None, None)
}

// ----------------------------------------------------------------------------
// 新阶段执行器（后台任务体）
// ----------------------------------------------------------------------------

/// 读剧情正稿正文（story.md；缺失 → None）。
async fn read_story_body(project: &FilmProject) -> Option<String> {
    let text = tokio::fs::read_to_string(format!("{}/story/story.md", hub_root(project)))
        .await
        .ok()?;
    let (_, body) = split_front_matter(&text);
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 从剧情正稿提取一句话梗概（首个非幕标题行，截 60 字）。
fn story_summary(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("【") || t.starts_with('#') {
            continue;
        }
        return t.chars().take(60).collect();
    }
    "（无梗概）".to_string()
}

/// 剧情正稿幕数统计。
fn count_acts(body: &str) -> usize {
    body.lines()
        .filter(|l| l.trim().starts_with("【") && l.trim().contains("幕"))
        .count()
}

/// story 阶段：AI 写剧情（source_file 给定则基于原文改编浓缩）→ story.md +
/// README 阶段推进。
pub(crate) async fn run_story_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    mr: ModelRef,
    prompt_override: Option<String>,
    source_file: Option<String>,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    log(format!("剧情生成：模型 {}", mr.label()));
    let root = match ensure_hub(ctx, &project).await {
        Ok(r) => r,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "story",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    // source_file 解析（存的 source-<slug>.txt 名或原始文件名均可）
    let source_text = match source_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(want) => {
            let stem = want.rsplit_once('.').map(|(s, _)| s).unwrap_or(want);
            let direct = hub_abs(&root, &format!("story/{want}")).ok();
            let by_slug = hub_abs(&root, &format!("story/source-{}.txt", slugify(stem))).ok();
            let path = direct
                .filter(|p| p.is_file())
                .or(by_slug.filter(|p| p.is_file()));
            match path {
                Some(p) => match tokio::fs::read_to_string(&p).await {
                    Ok(t) => {
                        log(format!(
                            "基于导入原文改编浓缩：{}（{} 字符）",
                            p.file_name()
                                .map(|f| f.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            t.chars().count()
                        ));
                        Some(t)
                    }
                    Err(e) => {
                        let msg = format!("读取导入原文失败 {}: {e}", p.display());
                        return finish_stage(
                            ctx,
                            &tasks,
                            task_id,
                            &project,
                            "error",
                            &msg,
                            None,
                            CostSpec {
                                stage: "story",
                                shot: None,
                                model_ref: Some(&mr),
                                started,
                                bytes: 0,
                                tokens: None,
                            },
                        );
                    }
                },
                None => {
                    let msg = format!("导入原文不存在: {want}（先 POST story/import）");
                    return finish_stage(
                        ctx,
                        &tasks,
                        task_id,
                        &project,
                        "error",
                        &msg,
                        None,
                        CostSpec {
                            stage: "story",
                            shot: None,
                            model_ref: Some(&mr),
                            started,
                            bytes: 0,
                            tokens: None,
                        },
                    );
                }
            }
        }
        None => None,
    };
    let user = prompt_override
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            build_story_prompt(
                &project.idea,
                &project.ratio,
                project.style_hint.as_deref(),
                source_text.as_deref(),
            )
        });
    log(format!(
        "剧情提示：{}",
        user.chars().take(60).collect::<String>()
    ));
    let (text, usage) = match ctx
        .chat_text_with_usage(&mr, story_system_prompt(), &user)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "story",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    let body = text.trim().to_string();
    if body.is_empty() {
        let msg = "LLM 返回空剧情正文".to_string();
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "story",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: usage,
            },
        );
    }
    // story.md（front-matter: source/words/summary）
    let words = body.chars().count();
    let acts = count_acts(&body);
    let mut fm = BTreeMap::new();
    if let Some(sf) = source_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let stem = sf.rsplit_once('.').map(|(s, _)| s).unwrap_or(sf);
        fm.insert(
            "source".to_string(),
            format!("source-{}.txt", slugify(stem)),
        );
    }
    fm.insert("words".to_string(), words.to_string());
    fm.insert("summary".to_string(), story_summary(&body));
    let path = format!("{root}/story/story.md");
    if let Err(e) = tokio::fs::write(&path, render_doc(&fm, &body)).await {
        let msg = format!("写剧情失败 {path}: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "story",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: body.len() as u64,
                tokens: usage,
            },
        );
    }
    set_readme_stage(
        &root,
        "story",
        &format!("剧情正稿已生成（{words} 字 / {acts} 幕）"),
    )
    .await;
    append_activity(&root, &author, "story.generate", "story/story.md").await;
    log(format!("剧情正稿 {words} 字 / {acts} 幕 → {path}"));
    finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("剧情正稿已存 {path}"),
        Some(path),
        CostSpec {
            stage: "story",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes: body.len() as u64,
            tokens: usage,
        },
    );
}

/// storyboard 阶段：读 story.md 分幕内容生成分镜（无剧情回落【创意】——旧
/// script 兼容别名同款锚定）→ storyboard/storyboard.json + script.json 镜像
/// （generate 产出即应用）。`output_storyboard`：true=新端点（output=
/// storyboard.json），false=旧 POST script 别名（output=script.json）。
pub(crate) async fn run_storyboard_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    mr: ModelRef,
    output_storyboard: bool,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    log(format!("分镜生成：模型 {}", mr.label()));
    let root = match ensure_hub(ctx, &project).await {
        Ok(r) => r,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "storyboard",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    let story = read_story_body(&project).await;
    if let Some(s) = &story {
        log(format!(
            "读剧情正稿：{} 字符 / {} 幕",
            s.chars().count(),
            count_acts(s)
        ));
    } else {
        log(
            "剧情正稿缺失（hub/story/story.md），回落【创意】直生分镜（可先 story 阶段）"
                .to_string(),
        );
    }
    // 旧角色库注入（characters 须从表选；与旧 script 阶段同口径——名+描述清单）
    let (roster_names, roster_entries): (Vec<String>, Vec<String>) = {
        let conn = ctx.db.lock().expect("film db poisoned");
        let roster = load_characters(&conn, &project.id);
        let mut names = Vec::new();
        let mut entries = Vec::new();
        for (i, c) in roster.iter().enumerate() {
            names.push(c.name.clone());
            entries.push(format!("{}. {}：{}", i + 1, c.name, c.description));
        }
        (names, entries)
    };
    if !roster_names.is_empty() {
        log(format!("角色表注入：{} 个角色", roster_names.len()));
    }
    let user = build_storyboard_prompt(
        story.as_deref(),
        &project.idea,
        &project.ratio,
        project.style_hint.as_deref(),
        &roster_entries,
    );
    let system = storyboard_system_prompt();
    let (text, mut usage) = match ctx.chat_text_with_usage(&mr, system, &user).await {
        Ok(t) => t,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "storyboard",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    log(format!("LLM 原始输出 {} 字符", text.chars().count()));
    let shots = match parse_script_shots(&text) {
        Ok(s) => (s, None),
        Err(first_err) => {
            log(format!(
                "首解析失败（{first_err}），重试一次（更收紧提示词）"
            ));
            let retry_user = build_storyboard_retry_prompt(story.as_deref(), &project.idea);
            match ctx.chat_text_with_usage(&mr, system, &retry_user).await {
                Ok((retry, u2)) => match parse_script_shots(&retry) {
                    Ok(s) => (s, u2),
                    Err(second_err) => {
                        let msg = format!("LLM 输出两次均无法解析为分镜 JSON：{second_err}");
                        return finish_stage(
                            ctx,
                            &tasks,
                            task_id,
                            &project,
                            "error",
                            &msg,
                            None,
                            CostSpec {
                                stage: "storyboard",
                                shot: None,
                                model_ref: Some(&mr),
                                started,
                                bytes: 0,
                                tokens: usage.or(u2),
                            },
                        );
                    }
                },
                Err(e) => {
                    let msg = format!("重试调用失败: {e}");
                    return finish_stage(
                        ctx,
                        &tasks,
                        task_id,
                        &project,
                        "error",
                        &msg,
                        None,
                        CostSpec {
                            stage: "storyboard",
                            shot: None,
                            model_ref: Some(&mr),
                            started,
                            bytes: 0,
                            tokens: usage,
                        },
                    );
                }
            }
        }
    };
    let (shots, retry_usage) = shots;
    if usage.is_none() {
        usage = retry_usage;
    }
    // 绑定容错：未知名保留原样 + 日志（不静默丢弃——用户可改名后复用绑定；
    // 旧 script 阶段同款口径）
    if !roster_names.is_empty() {
        for s in &shots {
            for name in &s.characters {
                if !roster_names.contains(name) {
                    log(format!(
                        "注意：镜头 {} 角色名「{name}」不在角色表，保留原样",
                        s.shot
                    ));
                }
            }
        }
    }
    // 双写：storyboard.json（树真值）+ script.json（画布/下游镜像，产出即应用）
    let sb_path = format!("{root}/storyboard/storyboard.json");
    let sc_path = format!("{}/script.json", project.dir);
    let label = mr.label();
    if let Err(e) = tokio::fs::write(&sb_path, storyboard_json(&shots, &label)).await {
        let msg = format!("写分镜失败 {sb_path}: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "storyboard",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: usage,
            },
        );
    }
    if let Err(e) = tokio::fs::write(&sc_path, script_json(&shots, &label)).await {
        let msg = format!("写分镜镜像失败 {sc_path}: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "storyboard",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: usage,
            },
        );
    }
    super::film::set_project_status(&ctx.db, &project.id, "scripted");
    set_readme_stage(
        &root,
        "storyboard",
        &format!("分镜已生成（{} 个镜头）", shots.len()),
    )
    .await;
    append_activity(
        &root,
        &author,
        "storyboard.generate",
        "storyboard/storyboard.json",
    )
    .await;
    let bytes = std::fs::metadata(&sb_path).map(|m| m.len()).unwrap_or(0);
    let out = if output_storyboard {
        sb_path.clone()
    } else {
        sc_path.clone()
    };
    log(format!(
        "分镜 {} 个镜头 → {sb_path}（+ script.json 镜像）",
        shots.len()
    ));
    finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("分镜 {} 个镜头已存 {out}", shots.len()),
        Some(out),
        CostSpec {
            stage: "storyboard",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes,
            tokens: usage,
        },
    );
}

/// casting extract 阶段：AI 读 story+storyboard 提取六类对象 → extraction.json。
pub(crate) async fn run_extract_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    mr: ModelRef,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    log(format!("定妆提取：模型 {}", mr.label()));
    let root = match ensure_hub(ctx, &project).await {
        Ok(r) => r,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "casting",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    let Some(story) = read_story_body(&project).await else {
        let msg = "剧情正稿缺失（hub/story/story.md）：先运行 story 阶段".to_string();
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "casting",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: None,
            },
        );
    };
    let shots = match read_script(&project).await {
        Ok(s) => s,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "casting",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    let user = build_extract_prompt(&story, &shots);
    let (text, usage) = match ctx
        .chat_text_with_usage(&mr, extract_system_prompt(), &user)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "casting",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    let report = match parse_extraction(&text) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("LLM 输出无法解析为六类定妆对象 JSON：{e}");
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &msg,
                None,
                CostSpec {
                    stage: "casting",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: text.len() as u64,
                    tokens: usage,
                },
            );
        }
    };
    let total: usize = report
        .as_object()
        .map(|m| {
            m.values()
                .filter_map(|v| v.as_array().map(|a| a.len()))
                .sum()
        })
        .unwrap_or(0);
    let path = format!("{root}/casting/extraction.json");
    if let Err(e) = tokio::fs::write(
        &path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .await
    {
        let msg = format!("写提取报告失败 {path}: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "casting",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: text.len() as u64,
                tokens: usage,
            },
        );
    }
    set_readme_stage(
        &root,
        "casting",
        &format!("定妆提取完成（六类共 {total} 个对象）"),
    )
    .await;
    append_activity(&root, &author, "casting.extract", "casting/extraction.json").await;
    log(format!("提取六类共 {total} 个对象 → {path}"));
    finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("定妆提取已存 {path}"),
        Some(path),
        CostSpec {
            stage: "casting",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes: text.len() as u64,
            tokens: usage,
        },
    );
}

/// 定妆视图生成阶段（AI 生图复用 media 内核 / channel images API）→
/// views/<view>.png + assets.json 登记 source=ai；主视图缺省回填 card portrait。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_view_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    ctype: String,
    slug: String,
    mr: ModelRef,
    view: String,
    prompt_override: Option<String>,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let root = hub_root(&project);
    let Some(card) = read_card(&root, &ctype, &slug) else {
        let msg = format!("定妆对象不存在: casting/{ctype}/{slug}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "view",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: None,
            },
        );
    };
    let desc = if card.desc.is_empty() {
        format!("（{}）", card.name)
    } else {
        card.desc.clone()
    };
    let prompt = prompt_override
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| default_view_prompt(&ctype, &desc, &view));
    if let Err(e) = tokio::fs::create_dir_all(format!("{root}/casting/{ctype}/{slug}/views")).await
    {
        let msg = format!("建视图目录失败: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "view",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    let out_path = format!("{root}/casting/{ctype}/{slug}/views/{view}.png");
    log(format!(
        "定妆视图生成「{}」{view}：模型 {}（720x720）",
        card.name,
        mr.label()
    ));
    log(format!(
        "视图提示：{}",
        prompt.chars().take(60).collect::<String>()
    ));
    // 跨视图一致性：channel 档注入既有视图为参考（local 无参考入口，纯 prompt 档）
    let result = match mr.source.as_str() {
        "local" => {
            ctx.gen_image_local(&prompt, 720, 720, &out_path, &log)
                .await
        }
        "channel" => {
            let refs = collect_view_refs(&root, &ctype, &slug, &log).await;
            if !refs.is_empty() {
                log(format!(
                    "跨视图参考注入：{} 张既有视图，strength {}",
                    refs.len(),
                    super::film::ref_strength_of(ctx)
                ));
            }
            match ctx
                .gen_image_channel(
                    &mr,
                    &prompt,
                    720,
                    720,
                    &refs,
                    super::film::ref_strength_of(ctx),
                )
                .await
            {
                Ok(bytes) => {
                    let ext = sniff_image_ext(&bytes).unwrap_or("png");
                    let path = format!("{root}/casting/{ctype}/{slug}/views/{view}.{ext}");
                    tokio::fs::write(&path, &bytes)
                        .await
                        .map_err(|e| format!("写视图失败 {path}: {e}"))
                }
                Err(e) => Err(e),
            }
        }
        other => Err(format!("未知 source: {other}")),
    };
    if let Err(e) = result {
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &e,
            None,
            CostSpec {
                stage: "view",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    // 产物路径（channel 可能非 png 扩展）
    let actual = ["png", "jpg", "webp"]
        .iter()
        .map(|e| format!("{root}/casting/{ctype}/{slug}/views/{view}.{e}"))
        .find(|p| Path::new(p).is_file())
        .unwrap_or(out_path.clone());
    let rel = actual
        .strip_prefix(&format!("{root}/"))
        .map(String::from)
        .unwrap_or_default();
    let bytes = std::fs::read(&actual).unwrap_or_default();
    register_asset(&root, &rel, &bytes, "ai", &format!("{ctype}/{slug}")).await;
    // 主视图缺省回填（front 页）
    let mut card2 = card.clone();
    if card2.portrait.is_none() || view == "front" {
        card2.portrait = Some(format!(
            "views/{}",
            rel.rsplit('/').next().unwrap_or_default()
        ));
        let _ = write_card(&root, &ctype, &slug, &card2);
    }
    set_readme_stage(
        &root,
        "casting",
        &format!("定妆视图新增：{ctype}/{slug}/{view}"),
    )
    .await;
    append_activity(&root, &author, "casting.view.generate", &rel).await;
    finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("定妆视图已存 {actual}"),
        Some(actual.clone()),
        CostSpec {
            stage: "view",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes: bytes.len() as u64,
            tokens: None,
        },
    );
}

/// BGM 生成阶段（复用渠道 music 能力；trigger 从 info.md 读）→
/// audio/bgm/<track>/track.mp3 + assets.json 登记 source=ai。
pub(crate) async fn run_bgm_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    track: String,
    mr: ModelRef,
    prompt_override: Option<String>,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let root = hub_root(&project);
    let dir = format!("{root}/audio/bgm/{track}");
    let entry = read_bgm_entry(&root, &track);
    let Some(entry) = entry else {
        let msg = format!("BGM 音轨不存在: {track}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "bgm",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: None,
            },
        );
    };
    let trigger = entry["trigger"].as_str().unwrap_or("global").to_string();
    let mood = entry["mood"].as_str().unwrap_or_default().to_string();
    log(format!(
        "BGM 生成「{track}」：模型 {}（trigger={trigger}）",
        mr.label()
    ));
    let prompt = prompt_override
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            let style = project
                .style_hint
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("电影感");
            let scene = if trigger == "global" {
                String::new()
            } else {
                format!("，触发场景：{}", trigger.trim_start_matches("scene:"))
            };
            format!(
                "为影片生成背景音乐，风格：{}{}，适配创意：{}",
                if mood.is_empty() {
                    style.to_string()
                } else {
                    mood.clone()
                },
                scene,
                project.idea
            )
        });
    log(format!(
        "BGM 提示：{}",
        prompt.chars().take(60).collect::<String>()
    ));
    let bytes = match mr.source.as_str() {
        "local" => {
            let msg = "本地音乐生成能力未接入（请用 source=channel）".to_string();
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &msg,
                None,
                CostSpec {
                    stage: "bgm",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            );
        }
        "channel" => match ctx.music_channel(&mr, &prompt).await {
            Ok(b) => b,
            Err(e) => {
                return finish_stage(
                    ctx,
                    &tasks,
                    task_id,
                    &project,
                    "error",
                    &e,
                    None,
                    CostSpec {
                        stage: "bgm",
                        shot: None,
                        model_ref: Some(&mr),
                        started,
                        bytes: 0,
                        tokens: None,
                    },
                )
            }
        },
        other => {
            let msg = format!("未知 source: {other}");
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &msg,
                None,
                CostSpec {
                    stage: "bgm",
                    shot: None,
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            );
        }
    };
    let out_path = format!("{dir}/track.mp3");
    if let Err(e) = tokio::fs::write(&out_path, &bytes).await {
        let msg = format!("写 BGM 失败 {out_path}: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "bgm",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: bytes.len() as u64,
                tokens: None,
            },
        );
    }
    let rel = format!("audio/bgm/{track}/track.mp3");
    register_asset(&root, &rel, &bytes, "ai", &rel).await;
    set_readme_stage(&root, "audio", &format!("BGM 就绪：{track}")).await;
    append_activity(&root, &author, "bgm.generate", &rel).await;
    finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("BGM 已存 {out_path}"),
        Some(out_path),
        CostSpec {
            stage: "bgm",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes: bytes.len() as u64,
            tokens: None,
        },
    );
}

/// compose 阶段（film.rs 改造后的执行体）：dist 版本化产物 + BGM 选择 + 报告。
pub(crate) async fn run_compose_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    bgm_pick: Option<String>,
    author: String,
) {
    use super::film::{
        build_concat_args, build_concat_list, build_mix_args, build_srt, compose_timeout,
        detect_ffmpeg, is_executable, ratio_dims, run_ffmpeg_pass, FFMPEG_INSTALL_HINT,
    };
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    // 0. ffmpeg 检测（缺失即失败附安装指引——不自动安装）
    let Some(ffmpeg) = ctx
        .ffmpeg_bin
        .clone()
        .or_else(detect_ffmpeg)
        .filter(|p| is_executable(p))
    else {
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            FFMPEG_INSTALL_HINT,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    };
    log(format!("ffmpeg：{ffmpeg}"));
    // 1. 分镜与镜头视频齐备性（committed 正式产物；cache 有未 commit 试生成则附提示）
    let shots = match read_script(&project).await {
        Ok(s) => s,
        Err(e) => {
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                CostSpec {
                    stage: "compose",
                    shot: None,
                    model_ref: None,
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    let cache_dir = format!("{}/cache", hub_root(&project));
    let missing: Vec<String> = shots
        .iter()
        .filter(|s| !Path::new(&format!("{}/shot-{}.mp4", project.dir, s.shot)).is_file())
        .map(|s| format!("shot-{}.mp4", s.shot))
        .collect();
    if !missing.is_empty() {
        let pending: Vec<String> = missing
            .iter()
            .filter(|m| Path::new(&format!("{cache_dir}/{m}")).is_file())
            .cloned()
            .collect();
        let hint = if pending.is_empty() {
            String::new()
        } else {
            format!(
                "（hub/cache 有试生成未 commit：{} —— 先 POST cache/<file>/commit 转正）",
                pending.join(", ")
            )
        };
        let msg = format!(
            "缺少镜头视频 {}（先完成各镜头 video 阶段并 commit）{hint}",
            missing.join(", ")
        );
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    let (w, h) = ratio_dims(&project.ratio).unwrap_or((1272, 720));
    // 2. concat 清单 + 第一遍（统一尺寸/fps 重编码；cwd=项目目录，文件名相对）
    if let Err(e) = tokio::fs::write(
        format!("{}/compose-concat.txt", project.dir),
        build_concat_list(shots.len()),
    )
    .await
    {
        let msg = format!("写 concat 清单失败: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    let pass1 = build_concat_args(w, h, "compose-video.mp4");
    if let Err(e) = run_ffmpeg_pass(&ffmpeg, &project.dir, &pass1, compose_timeout(), &log).await {
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &e,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    // 3. 字幕
    let srt = build_srt(&shots);
    let has_srt = !srt.is_empty();
    if has_srt {
        if let Err(e) = tokio::fs::write(format!("{}/subs.srt", project.dir), srt).await {
            let msg = format!("写字幕失败: {e}");
            return finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &msg,
                None,
                CostSpec {
                    stage: "compose",
                    shot: None,
                    model_ref: None,
                    started,
                    bytes: 0,
                    tokens: None,
                },
            );
        }
    }
    // 4. 人声轨（committed line-N.mp3 按时间轴）+ BGM 选择
    let mut voices: Vec<(String, u64)> = Vec::new();
    let mut t = 0u64;
    for s in &shots {
        if Path::new(&format!("{}/line-{}.mp3", project.dir, s.shot)).is_file() {
            voices.push((format!("line-{}.mp3", s.shot), t));
        }
        t += u64::from(s.duration_secs) * 1000;
    }
    let (bgm_input, bgm_track) = select_bgm_input(&project, bgm_pick.as_deref());
    log(format!(
        "混音输入：人声 {} 路，BGM {}，字幕 {}",
        voices.len(),
        bgm_input.as_deref().unwrap_or("无"),
        if has_srt { "有" } else { "无" }
    ));
    // 5. 第二遍 → dist/final-v<ts>.mp4（export_dir 语义保留：设置时 dist 落那里）
    let droot = dist_root(&project);
    if let Err(e) = tokio::fs::create_dir_all(&droot).await {
        let msg = format!("创建 dist 目录失败 {droot}: {e}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    let ts = now_compact();
    let mut final_name = format!("final-v{ts}.mp4");
    for n in 2.. {
        if !Path::new(&format!("{droot}/{final_name}")).exists() {
            break;
        }
        final_name = format!("final-v{ts}-{n}.mp4");
    }
    let final_path = format!("{droot}/{final_name}");
    let out_arg = if project
        .export_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        final_path.clone() // export 分支：绝对路径（cwd=项目目录）
    } else {
        format!("hub/dist/{final_name}") // 缺省分支：相对名落 hub/dist
    };
    let pass2 = build_mix_args(&voices, bgm_input.as_deref(), has_srt, &out_arg);
    if let Err(e) = run_ffmpeg_pass(&ffmpeg, &project.dir, &pass2, compose_timeout(), &log).await {
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &e,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    if !Path::new(&final_path).is_file() {
        let msg = format!("ffmpeg 退出 0 但产物缺失 {final_path}");
        return finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &msg,
            None,
            CostSpec {
                stage: "compose",
                shot: None,
                model_ref: None,
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    // 6. compose-report.json（版本化产物随行报告）
    let final_bytes = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    let total_secs: u64 = shots.iter().map(|s| u64::from(s.duration_secs)).sum();
    let report = serde_json::json!({
        "version": 1,
        "final": final_name,
        "created_at": now_iso(),
        "shots": shots.len(),
        "duration_secs": total_secs,
        "bgm": {
            "track": bgm_track,
            "input": bgm_input,
        },
        "voices": voices.len(),
        "subtitles": has_srt,
        "bytes": final_bytes,
        "ffmpeg": ffmpeg,
        "export_dir": project.export_dir,
    });
    let report_path = format!("{droot}/compose-report.json");
    let _ = tokio::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .await;
    super::film::set_project_status(&ctx.db, &project.id, "done");
    let root = hub_root(&project);
    set_readme_stage(&root, "compose", &format!("成片已合成：{final_name}")).await;
    append_activity(&root, &author, "compose", &format!("dist/{final_name}")).await;
    finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("成片已合成 {final_path}"),
        Some(final_path),
        CostSpec {
            stage: "compose",
            shot: None,
            model_ref: None,
            started,
            bytes: final_bytes,
            tokens: None,
        },
    );
}

// ----------------------------------------------------------------------------
// files 面（树清单 / 读 / 写）
// ----------------------------------------------------------------------------

/// 递归列 hub 树（depth≤6、条目≤2000；{path,bytes,kind}）。
fn walk_tree(root: &str, prefix: &str, out: &mut Vec<Value>, depth: usize) {
    if depth > 6 || out.len() >= 2000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(if prefix.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{prefix}")
    }) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if e.path().is_dir() {
            walk_tree(root, &rel, out, depth + 1);
        } else if let Ok(meta) = e.metadata() {
            let kind = if is_text_ext(&name) {
                "text"
            } else if is_binary_ext(&name) {
                "binary"
            } else {
                "other"
            };
            out.push(serde_json::json!({
                "path": rel,
                "bytes": meta.len(),
                "kind": kind,
            }));
        }
    }
}

/// GET 单文件（文本 → {kind,content}；二进制资产 → b64 信封）。
async fn files_get(root: &str, rel: &str) -> Result<ApiResponse, String> {
    let is_text = check_get_path(rel)?;
    let abs = hub_abs(root, rel)?;
    if !abs.is_file() {
        return Err(format!("文件不存在: {rel}"));
    }
    if is_text {
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| format!("读取失败 {rel}: {e}"))?;
        Ok(ok_json(serde_json::json!({
            "path": rel, "kind": "text", "content": content,
            "bytes": content.len(),
        })))
    } else {
        use base64::Engine;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| format!("读取失败 {rel}: {e}"))?;
        let mime = match abs
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            Some("mp3") => "audio/mpeg",
            Some("mp4") => "video/mp4",
            _ => "application/octet-stream",
        };
        Ok(ok_json(serde_json::json!({
            "path": rel, "kind": "binary", "mime": mime,
            "content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "bytes": bytes.len(),
        })))
    }
}

/// PUT 单文件（文本白名单；ownership 校验；storyboard 手改不自动应用——
/// 走 POST import 应用；budget.json 仅 budget_limit 生效）。
async fn files_put(
    ctx: &FilmCtx,
    project: &FilmProject,
    root: &str,
    rel: &str,
    body: FilesPutBody,
) -> Result<ApiResponse, String> {
    check_put_path(rel)?;
    let abs = hub_abs(root, rel)?;
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("建父目录失败: {e}"))?;
    }
    let content = body.content;
    if rel == "ownership.json" {
        let v: Value = serde_json::from_str(&content)
            .map_err(|e| format!("ownership.json 非合法 JSON: {e}"))?;
        validate_ownership(&v)?;
        {
            let _guard = HUB_META_LOCK.lock();
            save_ownership(root, &v);
        }
        append_activity(root, &author_of(&body.author), "files.put", rel).await;
        return Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({"written": rel, "bytes": content.len()}),
            headers: serde_json::json!({}),
        });
    }
    if rel == "budget.json" {
        // 仅 budget_limit 生效：events 恒以 DB 真值重建（防手改账本）
        let v: Value =
            serde_json::from_str(&content).map_err(|e| format!("budget.json 非合法 JSON: {e}"))?;
        let limit = v.get("budget_limit").and_then(Value::as_f64);
        let path = format!("{root}/budget.json");
        let doc = serde_json::json!({
            "version": 1, "currency": "CNY", "budget_limit": limit,
            "note": "events 为 film_cost_events 投影（DB 真值），手写无效",
        });
        tokio::fs::write(
            &path,
            serde_json::to_string_pretty(&doc).unwrap_or_default(),
        )
        .await
        .map_err(|e| format!("写 budget.json 失败: {e}"))?;
        if let Ok(conn) = ctx.db.lock() {
            rewrite_budget(&conn, project);
        }
        append_activity(root, &author_of(&body.author), "files.put", rel).await;
        return Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({"written": rel, "budget_limit": limit}),
            headers: serde_json::json!({}),
        });
    }
    // JSON 文件须合法 JSON（storyboard/extraction——防手滑写坏树）
    if rel.ends_with(".json") {
        serde_json::from_str::<Value>(&content).map_err(|e| format!("{rel} 非合法 JSON: {e}"))?;
    }
    tokio::fs::write(&abs, &content)
        .await
        .map_err(|e| format!("写失败 {rel}: {e}"))?;
    append_activity(root, &author_of(&body.author), "files.put", rel).await;
    Ok(ApiResponse {
        status: 200,
        body: serde_json::json!({"written": rel, "bytes": content.len()}),
        headers: serde_json::json!({}),
    })
}

// ----------------------------------------------------------------------------
// 路由声明
// ----------------------------------------------------------------------------

/// FilmHub 新增 21 条路由（component=film；读公开/写 admin）。
pub fn hub_routes() -> Vec<RouteSpec> {
    vec![
        spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/story/import"),
        spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/story/generate"),
        spec_admin(
            HttpMethod::Post,
            "/api/v1/film/projects/:id/storyboard/generate",
        ),
        spec_admin(
            HttpMethod::Post,
            "/api/v1/film/projects/:id/casting/extract",
        ),
        spec_public(HttpMethod::Get, "/api/v1/film/projects/:id/casting/:type"),
        spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/casting/:type"),
        spec_admin(
            HttpMethod::Put,
            "/api/v1/film/projects/:id/casting/:type/:name",
        ),
        spec_admin(
            HttpMethod::Delete,
            "/api/v1/film/projects/:id/casting/:type/:name",
        ),
        spec_admin(
            HttpMethod::Post,
            "/api/v1/film/projects/:id/casting/:type/:name/views/generate",
        ),
        spec_admin(
            HttpMethod::Post,
            "/api/v1/film/projects/:id/casting/:type/:name/views/import",
        ),
        spec_public(HttpMethod::Get, "/api/v1/film/projects/:id/audio/bgm"),
        spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/audio/bgm"),
        spec_admin(
            HttpMethod::Delete,
            "/api/v1/film/projects/:id/audio/bgm/:track",
        ),
        spec_admin(
            HttpMethod::Post,
            "/api/v1/film/projects/:id/audio/bgm/:track/generate",
        ),
        spec_admin(
            HttpMethod::Post,
            "/api/v1/film/projects/:id/cache/:file/commit",
        ),
        spec_public(HttpMethod::Get, "/api/v1/film/projects/:id/files"),
        spec_public(HttpMethod::Get, "/api/v1/film/projects/:id/files/*"),
        spec_admin(HttpMethod::Put, "/api/v1/film/projects/:id/files/*"),
        spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/export"),
        spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/import"),
        spec_public(HttpMethod::Get, "/api/v1/film/projects/:id/cost"),
    ]
}

// ----------------------------------------------------------------------------
// 分发（film.rs 兜底委托；未匹配返回 None 落回 404）
// ----------------------------------------------------------------------------

fn parse_body<T: DeserializeOwned>(body: &Value, what: &str) -> Result<T, String> {
    serde_json::from_value(body.clone()).map_err(|e| format!("解析{what}请求体失败: {e}"))
}

use serde::de::DeserializeOwned;

/// query 参数提取（path 带 ?a=b&c=d）。
fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    for kv in q.split('&') {
        let (k, v) = kv.split_once('=')?;
        if percent_decode(k) == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// FilmHub 新链分发器（film.rs handle 的兜底委托入口）。
pub async fn try_handle(
    h: &FilmRouteHandler,
    req: &ApiRequest,
    segs: &[&str],
) -> Result<Option<ApiResponse>, crate::error::ApiGatewayError> {
    use crate::error::ApiGatewayError;
    let bad = |msg: String| Ok(Some(error_response(400, &msg)));
    // segs 形如 ["api","v1","film","projects",<id>,...]
    if segs.len() < 5 || segs[0..4] != ["api", "v1", "film", "projects"] {
        return Ok(None);
    }
    let id = segs[4];
    let rest = &segs[5..];
    let ctx = h.ctx();
    let project = match h.project_or_404(id) {
        Ok(p) => p,
        Err(resp) => return Ok(Some(resp)),
    };
    match (req.method, rest) {
        // —— 1. story 导入（b64 JSON 信封）——
        (HttpMethod::Post, ["story", "import"]) => {
            let body: StoryImportBody =
                parse_body(&req.body, "story 导入").map_err(ApiGatewayError::Internal)?;
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let filename = body.filename.trim();
            if filename.is_empty() || filename.contains('/') || filename.contains('\\') {
                return bad("filename 不可为空且不可含路径分隔符".to_string());
            }
            let raw = body.content_b64.trim();
            if raw.is_empty() {
                return bad("content_b64 不可为空".to_string());
            }
            use base64::Engine;
            let bytes = match base64::engine::general_purpose::STANDARD.decode(raw) {
                Ok(b) => b,
                Err(e) => return bad(format!("content_b64 解码失败: {e}")),
            };
            if bytes.is_empty() {
                return bad("content_b64 解码后为空".to_string());
            }
            if bytes.len() > STORY_IMPORT_MAX_BYTES {
                return bad(format!(
                    "导入原文超过上限 {}MB（当前 {:.1}MB）",
                    STORY_IMPORT_MAX_BYTES / 1024 / 1024,
                    bytes.len() as f64 / 1024.0 / 1024.0
                ));
            }
            let text = match String::from_utf8(bytes) {
                Ok(t) => t,
                Err(_) => return bad("导入原文须为 UTF-8 文本".to_string()),
            };
            let stem = filename
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(filename);
            let slug = slugify(stem);
            let slug = if slug.is_empty() {
                "source".to_string()
            } else {
                slug
            };
            let rel = format!("story/source-{slug}.txt");
            let path = format!("{root}/{rel}");
            if let Err(e) = tokio::fs::write(&path, &text).await {
                return Ok(Some(error_response(
                    500,
                    &format!("写导入原文失败 {path}: {e}"),
                )));
            }
            let author = author_of(&body.author);
            append_activity(&root, &author, "story.import", &rel).await;
            eprintln!(
                "[filmhub] 剧情原文导入：{}（{rel}，{} 字符）",
                project.id,
                text.chars().count()
            );
            Ok(Some(ApiResponse {
                status: 201,
                body: serde_json::json!({
                    "path": rel, "filename": filename,
                    "bytes": text.len(), "chars": text.chars().count(),
                }),
                headers: serde_json::json!({}),
            }))
        }

        // —— 2. story 生成（202 任务，stage=story）——
        (HttpMethod::Post, ["story", "generate"]) => {
            let body: StoryGenBody =
                parse_body(&req.body, "story 生成").map_err(ApiGatewayError::Internal)?;
            if let Err(msg) = validate_model_ref(&body.model_ref, "chat") {
                return bad(msg);
            }
            let author = author_of(&body.author);
            let task_id = h.spawn_stage_task(&project, "story", |tid| {
                let ctx = ctx.clone();
                let project = project.clone();
                let mr = body.model_ref;
                let (prompt, source, author) = (body.prompt, body.source_file, author.clone());
                async move {
                    run_story_stage(&ctx, &tid, project, mr, prompt, source, author).await;
                }
            });
            Ok(Some(task_accepted(&h.tasks, &task_id)?))
        }

        // —— 3. storyboard 生成（读 story.md 分幕；旧 POST script 兼容别名在 film.rs）——
        (HttpMethod::Post, ["storyboard", "generate"]) => {
            let body: StoryboardGenBody =
                parse_body(&req.body, "storyboard 生成").map_err(ApiGatewayError::Internal)?;
            if let Err(msg) = validate_model_ref(&body.model_ref, "chat") {
                return bad(msg);
            }
            let author = author_of(&body.author);
            let task_id = h.spawn_stage_task(&project, "storyboard", |tid| {
                let ctx = ctx.clone();
                let project = project.clone();
                let mr = body.model_ref;
                let author = author.clone();
                async move {
                    run_storyboard_stage(&ctx, &tid, project, mr, true, author).await;
                }
            });
            Ok(Some(task_accepted(&h.tasks, &task_id)?))
        }

        // —— 4. casting 提取（先于 :type 匹配）——
        (HttpMethod::Post, ["casting", "extract"]) => {
            let body: ExtractBody =
                parse_body(&req.body, "定妆提取").map_err(ApiGatewayError::Internal)?;
            if let Err(msg) = validate_model_ref(&body.model_ref, "chat") {
                return bad(msg);
            }
            let author = author_of(&body.author);
            let task_id = h.spawn_stage_task(&project, "casting", |tid| {
                let ctx = ctx.clone();
                let project = project.clone();
                let mr = body.model_ref;
                let author = author.clone();
                async move {
                    run_extract_stage(&ctx, &tid, project, mr, author).await;
                }
            });
            Ok(Some(task_accepted(&h.tasks, &task_id)?))
        }

        // —— 5. 定妆对象 CRUD ——
        (HttpMethod::Get, ["casting", ctype]) => {
            if !CASTING_TYPES.contains(ctype) {
                return Ok(Some(error_response(
                    404,
                    &format!("未知定妆类别: {ctype}（{CASTING_TYPES:?}）"),
                )));
            }
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let mut out: Vec<Value> = Vec::new();
            for slug in list_casting_slugs(&root, ctype) {
                let card = read_card(&root, ctype, &slug).unwrap_or_default();
                let views: Vec<Value> = list_views(&root, ctype, &slug)
                    .into_iter()
                    .map(|(view, file, bytes)| {
                        serde_json::json!({
                            "view": view, "file": file, "bytes": bytes,
                            "url": super::film::files_download_url(&format!(
                                "{root}/casting/{ctype}/{slug}/views/{file}")),
                        })
                    })
                    .collect();
                out.push(serde_json::json!({
                    "type": ctype, "name": card.name, "slug": slug,
                    "desc": card.desc, "voice": card.voice, "portrait": card.portrait,
                    "views": views,
                    "claimed_by": object_claimer(&root, &format!("{ctype}/{slug}")),
                }));
            }
            Ok(Some(ok_json(Value::Array(out))))
        }

        (HttpMethod::Post, ["casting", ctype]) => {
            if !CASTING_TYPES.contains(ctype) {
                return Ok(Some(error_response(
                    404,
                    &format!("未知定妆类别: {ctype}（{CASTING_TYPES:?}）"),
                )));
            }
            let body: CastingCreateBody =
                parse_body(&req.body, "建定妆对象").map_err(ApiGatewayError::Internal)?;
            let name = body.name.trim();
            let desc = body.desc.trim();
            if name.is_empty() {
                return bad("name 不可为空".to_string());
            }
            if desc.is_empty() {
                return bad("desc 不可为空".to_string());
            }
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let slug = slugify(name);
            if slug.is_empty() {
                return bad(format!("name 无法 slug 化: {name}"));
            }
            // 重名 409：slug 目录与既有卡名双重判定
            if read_card(&root, ctype, &slug).is_some() {
                return Ok(Some(error_response(
                    409,
                    &format!("定妆对象已存在: casting/{ctype}/{slug}"),
                )));
            }
            for s in list_casting_slugs(&root, ctype) {
                if let Some(c) = read_card(&root, ctype, &s) {
                    if c.name == name {
                        return Ok(Some(error_response(
                            409,
                            &format!("定妆对象名「{name}」已存在（casting/{ctype}/{s}）"),
                        )));
                    }
                }
            }
            let card = CastingCard {
                name: name.to_string(),
                voice: body
                    .voice
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(String::from),
                portrait: None,
                desc: desc.to_string(),
            };
            if let Err(e) = write_card(&root, ctype, &slug, &card) {
                return Ok(Some(error_response(500, &e)));
            }
            let rel = format!("casting/{ctype}/{slug}/card.md");
            // 对象级自动认领（body 带 author 即认领，owner=author）
            let author = author_of(&body.author);
            let mut actions = vec!["casting.create"];
            if body.author.is_some() && author != "anonymous" {
                set_object_claim(&root, &format!("{ctype}/{slug}"), &author);
                actions.push("casting.claim");
            }
            for a in actions {
                append_activity(&root, &author, a, &format!("casting/{ctype}/{slug}")).await;
            }
            eprintln!(
                "[filmhub] 定妆对象创建：{}/{}（{}）",
                project.id, ctype, card.name
            );
            Ok(Some(ApiResponse {
                status: 201,
                body: serde_json::json!({
                    "type": ctype, "name": card.name, "slug": slug,
                    "voice": card.voice, "desc": card.desc, "path": rel,
                    "claimed_by": object_claimer(&root, &format!("{ctype}/{slug}")),
                }),
                headers: serde_json::json!({}),
            }))
        }

        (HttpMethod::Put, ["casting", ctype, name]) => {
            if !CASTING_TYPES.contains(ctype) {
                return Ok(Some(error_response(
                    404,
                    &format!("未知定妆类别: {ctype}（{CASTING_TYPES:?}）"),
                )));
            }
            let body: CastingUpdateBody =
                parse_body(&req.body, "改定妆对象").map_err(ApiGatewayError::Internal)?;
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let slug = percent_decode(name);
            let Some(mut card) = read_card(&root, ctype, &slug) else {
                return Ok(Some(error_response(
                    404,
                    &format!("定妆对象不存在: casting/{ctype}/{slug}"),
                )));
            };
            let new_slug = match body
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(n) => {
                    let ns = slugify(n);
                    if ns.is_empty() {
                        return bad(format!("name 无法 slug 化: {n}"));
                    }
                    if ns != slug && read_card(&root, ctype, &ns).is_some() {
                        return Ok(Some(error_response(
                            409,
                            &format!("定妆对象已存在: casting/{ctype}/{ns}"),
                        )));
                    }
                    card.name = n.to_string();
                    ns
                }
                None => slug.clone(),
            };
            if let Some(d) = body
                .desc
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                card.desc = d.to_string();
            }
            if let Some(v) = body.voice.as_deref() {
                let t = v.trim();
                card.voice = if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                };
            }
            if let Some(p) = body.portrait.as_deref() {
                if p.trim().is_empty() {
                    card.portrait = None;
                } else if !p.trim().starts_with("views/") || p.contains("..") {
                    return bad(
                        "portrait 须为 views/ 下的相对路径（如 views/front.png）".to_string()
                    );
                } else {
                    card.portrait = Some(p.trim().to_string());
                }
            }
            // 改名迁移目录 + 认领键
            if new_slug != slug {
                let from = format!("{root}/casting/{ctype}/{slug}");
                let to = format!("{root}/casting/{ctype}/{new_slug}");
                tokio::fs::rename(&from, &to)
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("改名迁移失败: {e}")))?;
                move_object_claim(
                    &root,
                    &format!("{ctype}/{slug}"),
                    &format!("{ctype}/{new_slug}"),
                );
            }
            if let Err(e) = write_card(&root, ctype, &new_slug, &card) {
                return Ok(Some(error_response(500, &e)));
            }
            let author = author_of(&body.author);
            append_activity(
                &root,
                &author,
                "casting.update",
                &format!("casting/{ctype}/{new_slug}"),
            )
            .await;
            Ok(Some(ok_json(serde_json::json!({
                "type": ctype, "name": card.name, "slug": new_slug,
                "voice": card.voice, "portrait": card.portrait, "desc": card.desc,
            }))))
        }

        (HttpMethod::Delete, ["casting", ctype, name]) => {
            if !CASTING_TYPES.contains(ctype) {
                return Ok(Some(error_response(
                    404,
                    &format!("未知定妆类别: {ctype}（{CASTING_TYPES:?}）"),
                )));
            }
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let slug = percent_decode(name);
            let dir = format!("{root}/casting/{ctype}/{slug}");
            if !Path::new(&dir).is_dir() {
                return Ok(Some(error_response(
                    404,
                    &format!("定妆对象不存在: casting/{ctype}/{slug}"),
                )));
            }
            let removed = match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
            remove_object_claim(&root, &format!("{ctype}/{slug}"));
            eprintln!(
                "[filmhub] 定妆对象删除：{}/{}（{}）",
                project.id, ctype, slug
            );
            Ok(Some(ok_json(serde_json::json!({
                "deleted": format!("casting/{ctype}/{slug}"), "dir_removed": removed,
            }))))
        }

        // —— 6. 定妆视图（生成 / 导入）——
        (HttpMethod::Post, ["casting", ctype, name, "views", "generate"]) => {
            if !CASTING_TYPES.contains(ctype) {
                return Ok(Some(error_response(404, &format!("未知定妆类别: {ctype}"))));
            }
            let body: ViewGenBody =
                parse_body(&req.body, "视图生成").map_err(ApiGatewayError::Internal)?;
            if let Err(msg) = validate_model_ref(&body.model_ref, "image") {
                return bad(msg);
            }
            let view = body.view.trim().to_string();
            if !is_slug_like(&view) {
                return bad(format!(
                    "view 须为 slug 形态（字母数字开头，允许 -/_，≤64 字符；front/side/back/action-N/custom-*）：{}",
                    body.view
                ));
            }
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let slug = percent_decode(name);
            if read_card(&root, ctype, &slug).is_none() {
                return Ok(Some(error_response(
                    404,
                    &format!("定妆对象不存在: casting/{ctype}/{slug}"),
                )));
            }
            let author = author_of(&body.author);
            let task_id = h.spawn_stage_task(&project, "view", |tid| {
                let ctx = ctx.clone();
                let project = project.clone();
                let mr = body.model_ref;
                let (ctype_s, slug_s, view_s, prompt, author) = (
                    (*ctype).to_string(),
                    slug.clone(),
                    view.clone(),
                    body.prompt,
                    author.clone(),
                );
                async move {
                    run_view_stage(
                        &ctx, &tid, project, ctype_s, slug_s, mr, view_s, prompt, author,
                    )
                    .await;
                }
            });
            Ok(Some(task_accepted(&h.tasks, &task_id)?))
        }

        (HttpMethod::Post, ["casting", ctype, name, "views", "import"]) => {
            if !CASTING_TYPES.contains(ctype) {
                return Ok(Some(error_response(404, &format!("未知定妆类别: {ctype}"))));
            }
            let body: ViewImportBody =
                parse_body(&req.body, "视图导入").map_err(ApiGatewayError::Internal)?;
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let slug = percent_decode(name);
            let Some(mut card) = read_card(&root, ctype, &slug) else {
                return Ok(Some(error_response(
                    404,
                    &format!("定妆对象不存在: casting/{ctype}/{slug}"),
                )));
            };
            let view = body.view.trim().to_string();
            if !is_slug_like(&view) {
                return bad(format!("view 须为 slug 形态: {}", body.view));
            }
            // mime/大小校验（定妆图上传同款）
            let raw = body.image_b64.trim();
            if raw.is_empty() {
                return bad("image_b64 不可为空".to_string());
            }
            use base64::Engine;
            let bytes = match base64::engine::general_purpose::STANDARD.decode(raw) {
                Ok(b) => b,
                Err(e) => return bad(format!("image_b64 解码失败: {e}")),
            };
            if bytes.is_empty() {
                return bad("image_b64 解码后为空".to_string());
            }
            if bytes.len() > IMAGE_MAX_BYTES {
                return bad(format!(
                    "图片超过上限 {}MB（当前 {:.1}MB）",
                    IMAGE_MAX_BYTES / 1024 / 1024,
                    bytes.len() as f64 / 1024.0 / 1024.0
                ));
            }
            let ext = match body.mime.as_deref() {
                Some(m) => match super::film::ext_for_mime(m) {
                    Some(e) => e,
                    None => return bad(format!("mime 仅支持 png/jpeg/webp（当前 {m}）")),
                },
                None => match sniff_image_ext(&bytes) {
                    Some(e) => e,
                    None => {
                        return bad("mime 缺省时按魔数嗅探仅支持 png/jpeg/webp（请显式传 mime）"
                            .to_string())
                    }
                },
            };
            if sniff_image_ext(&bytes) != Some(ext) {
                return bad("图片内容与 mime 不符（魔数校验失败）".to_string());
            }
            let rel = format!("casting/{ctype}/{slug}/views/{view}.{ext}");
            let path = format!("{root}/{rel}");
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(e) = tokio::fs::write(&path, &bytes).await {
                return Ok(Some(error_response(
                    500,
                    &format!("写视图失败 {path}: {e}"),
                )));
            }
            register_asset(&root, &rel, &bytes, "import", &format!("{ctype}/{slug}")).await;
            if card.portrait.is_none() || view == "front" {
                card.portrait = Some(format!("views/{view}.{ext}"));
                let _ = write_card(&root, ctype, &slug, &card);
            }
            let author = author_of(&body.author);
            append_activity(&root, &author, "casting.view.import", &rel).await;
            Ok(Some(ApiResponse {
                status: 201,
                body: serde_json::json!({
                    "path": rel, "bytes": bytes.len(), "source": "import",
                    "portrait": card.portrait,
                }),
                headers: serde_json::json!({}),
            }))
        }

        // —— 7. BGM ——
        (HttpMethod::Get, ["audio", "bgm"]) => {
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let list: Vec<Value> = list_bgm_tracks(&root)
                .into_iter()
                .filter_map(|t| read_bgm_entry(&root, &t))
                .collect();
            Ok(Some(ok_json(serde_json::json!({"tracks": list}))))
        }

        (HttpMethod::Post, ["audio", "bgm"]) => {
            let body: BgmCreateBody =
                parse_body(&req.body, "建 BGM").map_err(ApiGatewayError::Internal)?;
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let info = body.info.unwrap_or_default();
            let trigger = match info.trigger.as_deref() {
                None | Some("") => "global".to_string(),
                Some(t) => match validate_trigger(t) {
                    Ok(v) => v,
                    Err(e) => return bad(e),
                },
            };
            // 音轨名：body.name 优先，缺省 bgm-<n>
            let track = match body
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(n) => {
                    let s = slugify(n);
                    if !is_slug_like(&s) {
                        return bad(format!("name 无法 slug 化: {n}"));
                    }
                    s
                }
                None => format!("bgm-{}", list_bgm_tracks(&root).len() + 1),
            };
            if read_bgm_entry(&root, &track).is_some() {
                return Ok(Some(error_response(
                    409,
                    &format!("BGM 音轨已存在: {track}"),
                )));
            }
            let dir = format!("{root}/audio/bgm/{track}");
            if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                return Ok(Some(error_response(
                    500,
                    &format!("建音轨目录失败 {dir}: {e}"),
                )));
            }
            write_bgm_info(&dir, &trigger, info.mood.as_deref(), info.duration);
            let mut imported_bytes: Option<u64> = None;
            let action;
            if let Some(b64) = body
                .track_b64
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                use base64::Engine;
                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) => b,
                    Err(e) => return bad(format!("track_b64 解码失败: {e}")),
                };
                if bytes.is_empty() {
                    return bad("track_b64 解码后为空".to_string());
                }
                if bytes.len() > BGM_IMPORT_MAX_BYTES {
                    return bad(format!(
                        "音频超过上限 {}MB（当前 {:.1}MB）",
                        BGM_IMPORT_MAX_BYTES / 1024 / 1024,
                        bytes.len() as f64 / 1024.0 / 1024.0
                    ));
                }
                let path = format!("{dir}/track.mp3");
                if let Err(e) = tokio::fs::write(&path, &bytes).await {
                    return Ok(Some(error_response(
                        500,
                        &format!("写音轨失败 {path}: {e}"),
                    )));
                }
                let rel = format!("audio/bgm/{track}/track.mp3");
                register_asset(&root, &rel, &bytes, "import", &rel).await;
                imported_bytes = Some(bytes.len() as u64);
                action = "bgm.import";
            } else {
                action = "bgm.create";
            }
            let author = author_of(&body.author);
            append_activity(&root, &author, action, &format!("audio/bgm/{track}")).await;
            let entry = read_bgm_entry(&root, &track).unwrap_or_default();
            Ok(Some(ApiResponse {
                status: 201,
                body: serde_json::json!({
                    "track": track, "trigger": trigger,
                    "mood": info.mood, "duration": info.duration,
                    "bytes": imported_bytes, "entry": entry,
                }),
                headers: serde_json::json!({}),
            }))
        }

        (HttpMethod::Delete, ["audio", "bgm", track]) => {
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let t = percent_decode(track);
            let dir = format!("{root}/audio/bgm/{t}");
            if !Path::new(&dir).is_dir() {
                return Ok(Some(error_response(404, &format!("BGM 音轨不存在: {t}"))));
            }
            let removed = match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
            eprintln!("[filmhub] BGM 音轨删除：{}/{}", project.id, t);
            Ok(Some(ok_json(serde_json::json!({
                "deleted": t, "dir_removed": removed,
            }))))
        }

        (HttpMethod::Post, ["audio", "bgm", track, "generate"]) => {
            let body: BgmGenBody =
                parse_body(&req.body, "BGM 生成").map_err(ApiGatewayError::Internal)?;
            if let Err(msg) = validate_model_ref(&body.model_ref, "music") {
                return bad(msg);
            }
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let t = percent_decode(track);
            if read_bgm_entry(&root, &t).is_none() {
                return Ok(Some(error_response(404, &format!("BGM 音轨不存在: {t}"))));
            }
            let author = author_of(&body.author);
            let task_id = h.spawn_stage_task(&project, "bgm", |tid| {
                let ctx = ctx.clone();
                let project = project.clone();
                let mr = body.model_ref;
                let (t_s, prompt, author) = (t.clone(), body.prompt, author.clone());
                async move {
                    run_bgm_stage(&ctx, &tid, project, t_s, mr, prompt, author).await;
                }
            });
            Ok(Some(task_accepted(&h.tasks, &task_id)?))
        }

        // —— 8. cache commit（半成品转正）——
        (HttpMethod::Post, ["cache", file, "commit"]) => {
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let file = percent_decode(file);
            // 目标名白名单：shot-<n>.png / shot-<n>.mp4 / line-<n>.mp3
            let valid = (|| {
                let (stem, ext) = file.rsplit_once('.')?;
                let (prefix, num) = stem.split_once('-')?;
                if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                match prefix {
                    "shot" if ext == "png" || ext == "mp4" => Some(()),
                    "line" if ext == "mp3" => Some(()),
                    _ => None,
                }
            })()
            .is_some();
            if !valid {
                return bad(format!(
                    "cache 产物名须为 shot-<n>.png / shot-<n>.mp4 / line-<n>.mp3（当前 {file}）"
                ));
            }
            let from = format!("{root}/cache/{file}");
            if !Path::new(&from).is_file() {
                return Ok(Some(error_response(
                    404,
                    &format!("cache 半成品不存在: {file}"),
                )));
            }
            let to = format!("{}/{}", project.dir.trim_end_matches('/'), file);
            let bytes = std::fs::metadata(&from).map(|m| m.len()).unwrap_or(0);
            if let Err(e) = tokio::fs::rename(&from, &to).await {
                return Ok(Some(error_response(
                    500,
                    &format!("转正失败 {from} → {to}: {e}"),
                )));
            }
            #[derive(Default, Deserialize)]
            struct CommitBody {
                #[serde(default)]
                author: Option<String>,
            }
            let body: CommitBody = serde_json::from_value(req.body.clone()).unwrap_or_default();
            let author = author_of(&body.author);
            append_activity(&root, &author, "cache.commit", &file).await;
            eprintln!("[filmhub] cache 转正：{}/{} → {}", project.id, file, to);
            Ok(Some(ok_json(serde_json::json!({
                "committed": file, "from": from, "to": to, "bytes": bytes,
            }))))
        }

        // —— 9. files 面 ——
        (HttpMethod::Get, ["files"]) => {
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let mut files: Vec<Value> = Vec::new();
            walk_tree(&root, "", &mut files, 0);
            Ok(Some(ok_json(serde_json::json!({
                "root": root, "files": files,
            }))))
        }

        (HttpMethod::Get, ["files", rest @ ..]) if !rest.is_empty() => {
            let root = hub_root(&project);
            let rel = percent_decode(&rest.join("/"));
            match files_get(&root, &rel).await {
                Ok(resp) => Ok(Some(resp)),
                Err(e) => {
                    let status = if e.contains("不存在") { 404 } else { 400 };
                    Ok(Some(error_response(status, &e)))
                }
            }
        }

        (HttpMethod::Put, ["files", rest @ ..]) if !rest.is_empty() => {
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let rel = percent_decode(&rest.join("/"));
            let body: FilesPutBody =
                parse_body(&req.body, "files PUT").map_err(ApiGatewayError::Internal)?;
            match files_put(&ctx, &project, &root, &rel, body).await {
                Ok(resp) => Ok(Some(resp)),
                Err(e) => Ok(Some(error_response(400, &e))),
            }
        }

        // —— 9b. export / import ——
        (HttpMethod::Post, ["export"]) => {
            let body: ExportBody = serde_json::from_value(req.body.clone()).unwrap_or_default();
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            // ensure_hub 已含 export_inner；显式 export 再刷新一次（README 已在则不动）
            let written = match export_inner(&ctx, &project).await {
                Ok(w) => w,
                Err(e) => return Ok(Some(error_response(500, &e))),
            };
            let author = author_of(&body.author);
            append_activity(&root, &author, "export", "hub").await;
            eprintln!(
                "[filmhub] 项目导出：{}（{} 个文件）",
                project.id,
                written.len()
            );
            Ok(Some(ok_json(serde_json::json!({
                "root": root, "written": written,
            }))))
        }

        (HttpMethod::Post, ["import"]) => {
            #[derive(Default, Deserialize)]
            struct ImportBody {
                #[serde(default)]
                author: Option<String>,
            }
            let body: ImportBody = serde_json::from_value(req.body.clone()).unwrap_or_default();
            let root = match ensure_hub(&ctx, &project).await {
                Ok(r) => r,
                Err(e) => return bad(e),
            };
            let sb_path = format!("{root}/storyboard/storyboard.json");
            let raw = match tokio::fs::read_to_string(&sb_path).await {
                Ok(r) => r,
                Err(e) => {
                    return bad(format!(
                        "读取分镜失败 {sb_path}: {e}（先 storyboard/generate 或 files PUT）"
                    ))
                }
            };
            let parsed: Result<Vec<ScriptShot>, String> = serde_json::from_str::<Value>(&raw)
                .map_err(|e| format!("storyboard.json 非合法 JSON: {e}"))
                .and_then(|v| {
                    let shots = v.get("shots").cloned().unwrap_or(Value::Null);
                    serde_json::from_value::<Vec<ScriptShot>>(shots)
                        .map_err(|e| format!("shots 数组解析失败: {e}"))
                });
            let shots = match parsed {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => return bad("storyboard.json 的 shots 为空（无可应用分镜）".to_string()),
                Err(e) => return bad(e),
            };
            // casting 引用校验（未知引用不硬拦——报告给 agent 修名或补定妆）
            let mut known: Vec<String> = Vec::new();
            for t in CASTING_TYPES {
                for s in list_casting_slugs(&root, t) {
                    if let Some(c) = read_card(&root, t, &s) {
                        known.push(c.name);
                    }
                }
            }
            let mut unknown: Vec<String> = Vec::new();
            for s in &shots {
                for name in s
                    .characters
                    .iter()
                    .chain(&s.props)
                    .chain(&s.pets)
                    .chain(&s.scenes)
                    .chain(&s.actions)
                {
                    if !known.contains(name) && !unknown.contains(name) {
                        unknown.push(name.clone());
                    }
                }
            }
            // 应用：script.json 镜像（画布状态）
            let sc_path = format!("{}/script.json", project.dir);
            let pretty = script_json(&shots, "import（hub 树应用）");
            if let Err(e) = tokio::fs::write(&sc_path, pretty).await {
                return Ok(Some(error_response(
                    500,
                    &format!("写分镜镜像失败 {sc_path}: {e}"),
                )));
            }
            if project.status == "draft" {
                super::film::set_project_status(&ctx.db, &project.id, "scripted");
            }
            let author = author_of(&body.author);
            append_activity(&root, &author, "hub.import", "storyboard/storyboard.json").await;
            eprintln!(
                "[filmhub] 项目导入应用：{}（{} 镜头，未知定妆引用 {} 个）",
                project.id,
                shots.len(),
                unknown.len()
            );
            Ok(Some(ok_json(serde_json::json!({
                "applied": {"shots": shots.len(), "path": "script.json"},
                "known_objects": known.len(),
                "unknown_casting_refs": unknown,
            }))))
        }

        // —— 10. 成本汇总 ——
        (HttpMethod::Get, ["cost"]) => {
            let by = query_param(&req.path, "by").unwrap_or_else(|| "stage".to_string());
            let conn = h.db.lock().expect("film db poisoned");
            match cost_summary(&conn, &project, &by) {
                Ok(v) => Ok(Some(ok_json(v))),
                Err(e) => bad(e),
            }
        }

        _ => Ok(None),
    }
}

// 单元测试见文件尾（cfg(test)）。
#[cfg(test)]
mod tests;
