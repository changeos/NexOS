//! 可插拔网络服务：DHCP / DNS / PXE
//!
//! 决策依据：规划文档 §3.9 —— OS 内置轻量 DHCP/DNS/PXE，支撑 PXE 装机与局域网名解析。
//! 三个 trait 均 async，可独立替换实现（如 dnsmasq 后端 / 自研后端）。

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

// ============================================================================
// DHCP
// ============================================================================

/// DHCP 租约（动态或静态）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhcpLease {
    /// 客户端 MAC
    pub mac: String,
    /// 分配的 IP
    pub ip: IpAddr,
    /// 主机名（客户端上报）
    pub hostname: Option<String>,
    /// 租约到期时间（UTC）
    pub expiry: chrono::DateTime<chrono::Utc>,
}

impl DhcpLease {
    /// 校验 MAC 地址格式（IEEE 802：6 组两位十六进制，`:` 或 `-` 分隔，大小写不敏感）。
    ///
    /// 例：`"aa:bb:cc:dd:ee:ff"`、`"AA-BB-CC-DD-EE-FF"` 合法；`"aabb.ccdd.eeff"` 三点形式非法。
    pub fn validate_mac(mac: &str) -> Result<(), crate::NetworkError> {
        let parts: Vec<&str> = mac.split([':', '-']).collect();
        if parts.len() != 6 {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "MAC `{mac}` 非法（须 6 组两位十六进制，: 或 - 分隔）"
            )));
        }
        for p in parts {
            if p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(crate::NetworkError::RuleInvalid(format!(
                    "MAC 段 `{p}` 非法（须两位十六进制）"
                )));
            }
        }
        Ok(())
    }
}

/// DHCP 服务器——地址分配与静态租约管理。
#[allow(async_fn_in_trait)]
pub trait DhcpServer: Send + Sync {
    /// 列出当前所有租约（含静态与动态）。
    async fn list_leases(&self) -> Result<Vec<DhcpLease>, crate::NetworkError>;

    /// 添加静态租约（MAC 绑定固定 IP）。
    async fn add_static_lease(&self, mac: String, ip: IpAddr) -> Result<(), crate::NetworkError>;

    /// 移除静态租约。
    async fn remove_static_lease(&self, mac: &str) -> Result<(), crate::NetworkError>;

    /// 设置动态分配范围。
    ///
    /// - `start` / `end`：可分配地址区间
    /// - `gateway`：下发给客户端的网关
    /// - `dns`：下发给客户端的 DNS 列表
    async fn set_range(
        &self,
        start: IpAddr,
        end: IpAddr,
        gateway: IpAddr,
        dns: Vec<IpAddr>,
    ) -> Result<(), crate::NetworkError>;
}

// ============================================================================
// DNS
// ============================================================================

/// DNS 记录（A / AAAA / CNAME / MX / TXT）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DnsRecord {
    /// A 记录（IPv4）
    A(IpAddr),
    /// AAAA 记录（IPv6）
    Aaaa(IpAddr),
    /// CNAME 记录（别名）
    Cname { host: String },
    /// MX 记录（邮件交换）
    Mx { host: String, priority: u16 },
    /// TXT 记录
    Txt { text: String },
}

impl DnsRecord {
    /// 构造 A 记录（IPv4）。
    pub fn a(ip: std::net::Ipv4Addr) -> Self {
        Self::A(IpAddr::V4(ip))
    }
    /// 构造 AAAA 记录（IPv6）。
    pub fn aaaa(ip: std::net::Ipv6Addr) -> Self {
        Self::Aaaa(IpAddr::V6(ip))
    }
    /// 构造 CNAME 记录。
    pub fn cname(host: impl Into<String>) -> Self {
        Self::Cname { host: host.into() }
    }
    /// 构造 MX 记录。
    pub fn mx(host: impl Into<String>, priority: u16) -> Self {
        Self::Mx {
            host: host.into(),
            priority,
        }
    }
    /// 构造 TXT 记录。
    pub fn txt(text: impl Into<String>) -> Self {
        Self::Txt { text: text.into() }
    }
}

/// DNS 服务器——本地域名解析记录管理。
#[allow(async_fn_in_trait)]
pub trait DnsServer: Send + Sync {
    /// 列出所有记录。
    async fn list_records(&self) -> Result<Vec<(String, DnsRecord)>, crate::NetworkError>;

    /// 新增记录。
    async fn add_record(&self, name: String, record: DnsRecord) -> Result<(), crate::NetworkError>;

    /// 删除记录。
    async fn delete_record(
        &self,
        name: &str,
        record: &DnsRecord,
    ) -> Result<(), crate::NetworkError>;
}

// ============================================================================
// PXE
// ============================================================================

/// PXE 服务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PxeState {
    /// 已启用
    Enabled,
    /// 已禁用
    Disabled,
}

/// PXE 服务信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PxeStatus {
    /// 当前状态
    pub state: PxeState,
    /// 启动文件名（如 "pxelinux.0"）
    pub boot_file: Option<String>,
    /// next-server（TFTP 服务器地址）
    pub next_server: Option<IpAddr>,
}

/// PXE 服务器——网络装机引导配置。
#[allow(async_fn_in_trait)]
pub trait PxeServer: Send + Sync {
    /// 设置引导文件与 next-server。
    async fn set_boot_file(
        &self,
        filename: String,
        next_server: IpAddr,
    ) -> Result<(), crate::NetworkError>;

    /// 查询 PXE 服务状态。
    async fn status(&self) -> Result<PxeStatus, crate::NetworkError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn dhcp_lease_mac_valid() {
        assert!(DhcpLease::validate_mac("aa:bb:cc:dd:ee:ff").is_ok());
        assert!(DhcpLease::validate_mac("AA-BB-CC-DD-EE-FF").is_ok());
        assert!(DhcpLease::validate_mac("00:11:22:33:44:55").is_ok());
    }

    #[test]
    fn dhcp_lease_mac_invalid() {
        assert!(DhcpLease::validate_mac("").is_err());
        assert!(DhcpLease::validate_mac("aa:bb:cc").is_err()); // 组数不足
        assert!(DhcpLease::validate_mac("aa:bb:cc:dd:ee:gg").is_err()); // 非十六进制
        assert!(DhcpLease::validate_mac("aaa:bb:cc:dd:ee:ff").is_err()); // 段超长
        assert!(DhcpLease::validate_mac("aabb.ccdd.eeff").is_err()); // 三点形式不支持
    }

    #[test]
    fn dns_record_constructors() {
        let a = DnsRecord::a(Ipv4Addr::new(192, 168, 1, 1));
        let aaaa = DnsRecord::aaaa(Ipv6Addr::LOCALHOST);
        let cname = DnsRecord::cname("os.example.com");
        let mx = DnsRecord::mx("mail.example.com", 10);
        let txt = DnsRecord::txt("v=spf1 -all");
        assert!(matches!(a, DnsRecord::A(_)));
        assert!(matches!(aaaa, DnsRecord::Aaaa(_)));
        assert!(matches!(cname, DnsRecord::Cname { .. }));
        assert!(matches!(mx, DnsRecord::Mx { priority: 10, .. }));
        assert!(matches!(txt, DnsRecord::Txt { .. }));
    }

    #[test]
    fn dns_record_serde_tagged() {
        // 契约 DnsRecord 用 `#[serde(tag = "kind")]` 内部标记表示。
        // 注：serde 内部标记（internally-tagged）不支持 newtype 变体含非结构体
        // （如 A(IpAddr)），序列化 A/AAAA 会报错——这是既有契约的已知限制，
        // 已记入 PROGRESS.md「契约问题」。此处仅测结构体变体（MX），其可正常序列化。
        let mx = DnsRecord::mx("mail.example.com", 10);
        let j = serde_json::to_string(&mx).unwrap();
        assert!(j.contains("\"kind\":\"mx\""));
    }

    #[test]
    fn pxe_status_serde() {
        let s = PxeStatus {
            state: PxeState::Enabled,
            boot_file: Some("pxelinux.0".into()),
            next_server: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: PxeStatus = serde_json::from_str(&j).unwrap();
        assert_eq!(s.state, back.state);
        assert_eq!(s.boot_file, back.boot_file);
    }
}
