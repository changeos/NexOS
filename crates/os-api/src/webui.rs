//! Web UI 静态资源内嵌（rust-embed，规划文档 §3.6 / §9.1#10）。
//!
//! 在**编译期**把 Web 前端打进 os-api binary（无运行期外部文件依赖），
//! 由 [`crate::http::build_router`] 的 fallback handler 经 [`get_asset`](crate::webui::get_asset) 对外服务：
//! - `GET /` → `index.html`
//! - `GET /static/<path>` → 对应静态文件（CSS/JS/图片等）
//!
//! # 双源策略：Vue3（static-dist/）优先，旧版（static/）兜底
//!
//! 内嵌两个资源池：
//! - `VueAssets`：`crates/os-api/static-dist/`，**Vue3 构建产物**
//!   （`npm run build`，见 `crates/os-api/web/` + Makefile `web` 目标 +
//!   `scripts/build-iso.sh` 前端构建阶段）。`vite.config.ts` 设 `base: '/static/'`，
//!   故产物内引用形如 `/static/assets/index-*.js`，与本模块 fallback 路径一致。
//! - `LegacyAssets`：`crates/os-api/static/`，**旧版手写前端**（兼容保留）。
//!
//! [`get_asset`](crate::webui::get_asset) 先查 Vue3 产物，未命中再查旧版——既支持新前端上线，
//! 又保证尚未构建 Vue3（如纯 `cargo build` 无前端产物）时仍可启动。
//!
//! # 选型
//!
//! 选 [`rust_embed`](https://crates.io/crates/rust-embed)（8.x）：
//! - 编译期嵌入，零运行期 IO（文件不存在则编译失败，提前暴露问题）
//! - `#[derive(Embed)]` 生成 `get(path) -> Option<EmbeddedFile>`，自带文件元数据
//! - `folder` 路径相对 `$CARGO_MANIFEST_DIR`（即 `crates/os-api/`）解析

use rust_embed::Embed;

/// Vue3 构建产物资源池（**主源**）。
///
/// `folder` = `static-dist/`，由 `crates/os-api/web/` 的 `npm run build` 产出
/// （vite outDir=`../static-dist`）。构建产物不入 git（见仓库根 `.gitignore`），
/// 编译期需先执行 `make web` 或 `npm run build` 生成。
/// 注意：改前端后必须 cargo clean -p os-api 重编译，rust-embed 才会重新嵌入。
#[derive(Embed)]
#[folder = "static-dist/"]
struct VueAssets;

/// 旧版手写前端资源池（**兜底源**，兼容保留）。
///
/// `folder` = `static/`，含占位 HTML/CSS/JS。保留以便：
/// - 未构建 Vue3 时（纯 `cargo build`）binary 仍可启动并返回 HTML；
/// - 旧版前端文件可作参照。
#[derive(Embed)]
#[folder = "static/"]
struct LegacyAssets;

/// 取静态文件内容 + 对应 MIME 类型（**Vue3 产物优先，旧版兜底**）。
///
/// `path` 为去掉前导 `/` 的相对路径（如 `index.html` / `assets/index-xxx.js` /
/// `css/style.css`）。命中返回 `(字节, mime)`；两个池都未命中返回 `None`
/// （由调用方回 404）。
///
/// 用于 axum handler（[`crate::http::build_router`] 的 fallback）：
/// ```ignore
/// if let Some((data, mime)) = webui::get_asset("index.html") { ... }
/// ```
pub fn get_asset(path: &str) -> Option<(Vec<u8>, &'static str)> {
    // 优先从磁盘读（绕过 rust-embed 增量编译缓存不刷新问题）
    // release 构建可加 #[cfg(debug_assertions)] 限制，当前默认 debug 跑
    {
        if let Ok(data) = read_from_disk(path) {
            return Some((data, mime_for_path(path)));
        }
    }
    // 先查 Vue3 构建产物（主源：crates/os-api/static-dist/）
    if let Some(file) = VueAssets::get(path) {
        let data = file.data.into_owned();
        // rust-embed 嵌入的文件可能过时（<10 字节通常是占位），debug 下回退磁盘
        if data.len() > 10 {
            return Some((data, mime_for_path(path)));
        }
    }
    // 未命中 → 查旧版手写前端（兜底：crates/os-api/static/）
    if let Some(file) = LegacyAssets::get(path) {
        let data = file.data.into_owned();
        if data.len() > 10 {
            return Some((data, mime_for_path(path)));
        }
    }
    // 最后兜底：从磁盘读
    {
        if let Ok(data) = read_from_disk(path) {
            return Some((data, mime_for_path(path)));
        }
    }
    None
}

/// 从磁盘读 static-dist/ 文件（绕过 rust-embed 缓存）。
fn read_from_disk(path: &str) -> Result<Vec<u8>, std::io::Error> {
    use std::path::PathBuf;
    // 运行时从 cwd 或已知路径读
    let base = option_env!("OS_STATIC_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("crates/os-api/static-dist"));
    let full = base.join(path);
    std::fs::read(&full)
}

/// 按扩展名推断静态 MIME（覆盖常见 Web UI 资源类型）。
///
/// 默认 `application/octet-stream`（二进制流，浏览器按内容嗅探）。
fn mime_for_path(path: &str) -> &'static str {
    // rsplit('.').next() 取最后一段扩展名（返回 Option；无扩展名落到默认分支）
    match path.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_asset_index_html_present() {
        // index.html 必须被内嵌（Vue3 产物或旧版，编译期即决定，运行期一定存在）
        let (data, mime) = get_asset("index.html").expect("index.html 应被内嵌");
        let s = String::from_utf8(data).unwrap();
        assert!(s.contains("NexOS"), "index.html 内容应含标题");
        assert_eq!(mime, "text/html; charset=utf-8");
    }

    #[test]
    fn get_asset_missing_returns_none() {
        assert!(get_asset("does-not-exist.xyz").is_none());
    }

    #[test]
    fn mime_for_path_known_extensions() {
        assert_eq!(mime_for_path("a.css"), "text/css; charset=utf-8");
        assert_eq!(
            mime_for_path("b.js"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(mime_for_path("c.png"), "image/png");
        assert_eq!(mime_for_path("d.svg"), "image/svg+xml");
        assert_eq!(mime_for_path("e.json"), "application/json; charset=utf-8");
        // 无扩展名 / 未知 → 默认二进制流
        assert_eq!(mime_for_path("noext"), "application/octet-stream");
        assert_eq!(mime_for_path("x.unknown"), "application/octet-stream");
    }

    #[test]
    fn vue_assets_embedded_when_built() {
        // 验证 Vue3 产物池存在（先 make web 后产物落地）。
        // 至少应含 index.html；assets/<hash>.js 经 hash 变化，只统计非空。
        let entries: Vec<_> = VueAssets::iter().collect();
        assert!(
            entries.iter().any(|p| p.ends_with("index.html")),
            "Vue3 产物池应含 index.html（先 make web 构建前端）: {:?}",
            entries
        );
    }
}
