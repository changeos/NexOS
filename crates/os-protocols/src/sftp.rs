//! SFTP 协议（SSH 文件传输）
//!
//! 实现说明：Rust 侧内置 SSH 服务（如 russh）或编排 OpenSSH 的 sftp-subsystem。
//!
//! 配置生成：`SftpConfig::render` 渲染 INI 风格摘要；`render_authorized_keys`
//! 把 `(用户, 公钥)` 列表渲染成 OpenSSH 兼容的 `authorized_keys` 文本，
//! 供 `RusshSftpBackend` 写入 `~/.ssh/authorized_keys`（真实 russh 集成
//! TODO \[DOC\]：russh 已注册且 `RusshSftpBackend` 已接通真实 SSH/SFTP 协议栈；
//! 端口监听由上层挂载，本 trait 仅承载配置——此 TODO 为文档说明性，非运行时阻塞）。

use std::collections::HashMap;

use os_core::{Deserialize, Serialize};

use crate::common::FileProtocol;
use crate::ProtocolResult;

/// SFTP 服务配置（监听 + 认证策略）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SftpConfig {
    /// 监听地址（如 `0.0.0.0:22`）
    pub listen: String,
    /// 是否启用密码认证
    pub password_auth: bool,
    /// 是否启用公钥认证
    pub pubkey_auth: bool,
    /// SFTP 子系统的 chroot 根（如 `/srv/sftp`；None = 不 chroot）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chroot: Option<String>,
}

impl SftpConfig {
    /// 一个开箱即用的开发态默认配置（`0.0.0.0:22` / 仅公钥认证 / 不 chroot）。
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            listen: "0.0.0.0:22".into(),
            password_auth: false,
            pubkey_auth: true,
            chroot: None,
        }
    }

    /// 渲染成 INI 风格配置文本（`[sftp]` 段）。
    #[must_use]
    pub fn render(&self) -> String {
        let yn = |b: bool| if b { "true" } else { "false" };
        let mut out = format!(
            "[sftp]\nlisten = {}\npassword_auth = {}\npubkey_auth = {}\n",
            self.listen,
            yn(self.password_auth),
            yn(self.pubkey_auth)
        );
        if let Some(c) = &self.chroot {
            out.push_str(&format!("chroot = {c}\n"));
        }
        out
    }
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// 把一组用户→公钥列表映射渲染成 OpenSSH 兼容的 `authorized_keys` 文本。
///
/// 注意：authorized_keys 是单文件（每用户一份），本函数返回的是**汇总文本**，
/// 编排器（`RusshSftpBackend`）按用户拆分写入各自 `~/.ssh/authorized_keys`。
/// 每条公钥独占一行；空用户/空公钥跳过。
#[must_use]
pub fn render_authorized_keys(keys: &HashMap<String, Vec<String>>) -> String {
    // 按用户名稳定排序，输出确定性
    let mut users: Vec<&String> = keys.keys().collect();
    users.sort();
    let mut out = String::new();
    for u in users {
        // 用户段头（仅汇总视图；真实 per-user 文件不含此行）
        out.push_str(&format!("# user: {u}\n"));
        for k in &keys[u] {
            let trimmed = k.trim();
            if !trimmed.is_empty() {
                out.push_str(trimmed);
                out.push('\n');
            }
        }
    }
    out
}

/// SFTP 管理器。
///
/// 继承 `FileProtocol`；额外提供 authorized_keys 管理（公钥授权）。
#[allow(async_fn_in_trait)]
pub trait SftpManager: FileProtocol {
    /// 为用户添加 authorized_keys 条目（公钥授权）。
    async fn authorize_key(&self, user: &str, pubkey: &str) -> ProtocolResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sftp_render_basic() {
        let c = SftpConfig::defaults();
        let txt = c.render();
        assert!(txt.starts_with("[sftp]\n"));
        assert!(txt.contains("listen = 0.0.0.0:22"));
        assert!(txt.contains("password_auth = false"));
        assert!(txt.contains("pubkey_auth = true"));
        // 无 chroot 时不输出 chroot 行
        assert!(!txt.contains("chroot"));
    }

    #[test]
    fn sftp_render_with_chroot_and_password() {
        let c = SftpConfig {
            listen: "0.0.0.0:2222".into(),
            password_auth: true,
            pubkey_auth: true,
            chroot: Some("/srv/sftp".into()),
        };
        let txt = c.render();
        assert!(txt.contains("password_auth = true"));
        assert!(txt.contains("chroot = /srv/sftp"));
    }

    #[test]
    fn authorized_keys_render_sorted_and_filtered() {
        let mut keys = HashMap::new();
        keys.insert(
            "bob".into(),
            vec!["ssh-ed25519 AAAA bob@host".into(), "  ".into()],
        );
        keys.insert("alice".into(), vec!["ssh-rsa BBBB alice@host".into()]);
        let txt = render_authorized_keys(&keys);
        // alice 段在 bob 前（按用户名排序）
        let a = txt.find("# user: alice").unwrap();
        let b = txt.find("# user: bob").unwrap();
        assert!(a < b);
        assert!(txt.contains("ssh-ed25519 AAAA bob@host"));
        assert!(txt.contains("ssh-rsa BBBB alice@host"));
        // 空公钥被过滤
        assert_eq!(txt.matches("  \n").count(), 0);
    }
}
