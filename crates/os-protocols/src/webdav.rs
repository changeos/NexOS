//! WebDAV 协议
//!
//! 实现说明：WebDAV 通常由 Rust 侧内置 HTTP 服务（如 dav-server）实现，共享管理多为
//! `FileProtocol` 父 trait 的默认行为，本 trait 仅承载 WebDAV 专属配置。
//!
//! 配置生成：`WebDavConfig::render` 把监听/TLS/认证选项渲染成 INI 风格配置摘要，
//! 供 `DavServerBackend` 启动内置 HTTP 服务时消费（真实 dav-server 集成在编排器侧
//! TODO \[DOC\]：dav-server 已注册且 `DavServerBackend` 已接通真实协议栈；端口监听由
//! 上层挂载，本 trait 仅承载配置——此 TODO 为文档说明性，非运行时阻塞）。

use os_core::{Deserialize, Serialize};

use crate::common::FileProtocol;

/// WebDAV 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebDavConfig {
    /// 监听地址（如 `0.0.0.0:5005`）
    pub listen: String,
    /// 是否启用 HTTPS（反代或内置 TLS）
    pub tls: bool,
    /// 是否启用基本认证
    pub basic_auth: bool,
}

impl WebDavConfig {
    /// 一个开箱即用的开发态默认配置（`0.0.0.0:5005` / 无 TLS / 启用基本认证）。
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            listen: "0.0.0.0:5005".into(),
            tls: false,
            basic_auth: true,
        }
    }

    /// 渲染成 INI 风格配置文本（`[webdav]` 段）。
    #[must_use]
    pub fn render(&self) -> String {
        let yn = |b: bool| if b { "true" } else { "false" };
        format!(
            "[webdav]\nlisten = {}\ntls = {}\nbasic_auth = {}\n",
            self.listen,
            yn(self.tls),
            yn(self.basic_auth)
        )
    }
}

impl Default for WebDavConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// WebDAV 管理器。
///
/// 继承 `FileProtocol`；共享生命周期/会话管理复用父 trait。
#[allow(async_fn_in_trait)]
pub trait WebDavManager: FileProtocol {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webdav_render_basic() {
        let c = WebDavConfig {
            listen: "0.0.0.0:5005".into(),
            tls: false,
            basic_auth: true,
        };
        let txt = c.render();
        assert!(txt.starts_with("[webdav]\n"));
        assert!(txt.contains("listen = 0.0.0.0:5005"));
        assert!(txt.contains("tls = false"));
        assert!(txt.contains("basic_auth = true"));
    }

    #[test]
    fn webdav_render_tls() {
        let c = WebDavConfig {
            listen: "0.0.0.0:5443".into(),
            tls: true,
            basic_auth: true,
        };
        let txt = c.render();
        assert!(txt.contains("tls = true"));
    }
}
