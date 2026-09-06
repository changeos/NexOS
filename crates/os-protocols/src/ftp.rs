//! FTP 协议
//!
//! 实现说明：Rust 侧内置 FTP 服务（如 libunftp）或编排外部 ftpd。
//!
//! 配置生成：`FtpConfig::render` 把监听/被动端口范围/TLS/匿名选项渲染成
//! INI 风格配置摘要，供 `LibunftpBackend` 启动服务时消费（真实 libunftp 集成
//! TODO \[DOC\]：libunftp 已注册且 `LibunftpBackend` 已接通真实协议栈；端口监听由
//! 上层挂载，本 trait 仅承载配置——此 TODO 为文档说明性，非运行时阻塞）。

use os_core::{Deserialize, Serialize};

use crate::common::FileProtocol;

/// FTP 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtpConfig {
    /// 监听地址（控制连接，如 `0.0.0.0:21`）
    pub listen: String,
    /// 被动模式端口范围 (start, end)
    pub passive_ports: (u16, u16),
    /// 是否启用 TLS（FTPS）
    pub tls: bool,
    /// 是否允许匿名
    pub anonymous: bool,
}

impl FtpConfig {
    /// 一个开箱即用的开发态默认配置（`0.0.0.0:21` / 被动 30000-40000 / 无 TLS / 禁匿名）。
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            listen: "0.0.0.0:21".into(),
            passive_ports: (30000, 40000),
            tls: false,
            anonymous: false,
        }
    }

    /// 渲染成 INI 风格配置文本（`[ftp]` 段）。
    #[must_use]
    pub fn render(&self) -> String {
        let yn = |b: bool| if b { "true" } else { "false" };
        format!(
            "[ftp]\nlisten = {}\npassive_range = {}-{}\ntls = {}\nanonymous = {}\n",
            self.listen,
            self.passive_ports.0,
            self.passive_ports.1,
            yn(self.tls),
            yn(self.anonymous)
        )
    }
}

impl Default for FtpConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// FTP 管理器。
///
/// 继承 `FileProtocol`；共享生命周期/会话管理复用父 trait。
#[allow(async_fn_in_trait)]
pub trait FtpManager: FileProtocol {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftp_render_basic() {
        let c = FtpConfig::defaults();
        let txt = c.render();
        assert!(txt.starts_with("[ftp]\n"));
        assert!(txt.contains("listen = 0.0.0.0:21"));
        assert!(txt.contains("passive_range = 30000-40000"));
        assert!(txt.contains("tls = false"));
        assert!(txt.contains("anonymous = false"));
    }

    #[test]
    fn ftp_render_ftps_anon() {
        let c = FtpConfig {
            listen: "0.0.0.0:990".into(),
            passive_ports: (50000, 50100),
            tls: true,
            anonymous: true,
        };
        let txt = c.render();
        assert!(txt.contains("tls = true"));
        assert!(txt.contains("anonymous = true"));
        assert!(txt.contains("passive_range = 50000-50100"));
    }
}
