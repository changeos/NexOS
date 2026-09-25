//! Captive Portal——未认证访客拦截与重定向
//!
//! 决策依据：规划文档 §3.18 —— 访客接入 VLAN 后，未认证前由 Captive Portal 拦截
//! 各操作系统的"联网检测"探测请求（iOS captive-login / Android generate_204 /
//! Win ncsi / macOS hotspot-detect），返回 302 重定向到落地页完成认证。

use os_core::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// PortalConfig / ProbeRequest / PortalResponse
// ----------------------------------------------------------------------------

/// Portal 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalConfig {
    /// HTTP 监听端口（通常 80，用于拦截明文探测）
    pub listen_http: u16,
    /// HTTPS 监听端口（通常 443，用于拦截 TLS 探测与落地页）
    pub listen_https: u16,
    /// 关联的访客 VLAN ID（None = 不绑定特定 VLAN）
    pub vlan_id: Option<u16>,
    /// 自定义落地页 HTML（None = 用默认内置落地页）
    pub landing_html: Option<String>,
    /// 是否桥接到 AP（true = Portal 同时充当当 AP 桥接）
    pub ap_bridge: bool,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            listen_http: 80,
            listen_https: 443,
            vlan_id: None,
            landing_html: None,
            ap_bridge: false,
        }
    }
}

/// 探测请求（操作系统联网检测）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRequest {
    /// User-Agent（用于识别探测来源 OS）
    pub user_agent: String,
    /// 请求 Host（如 "captive.apple.com" / "msftconnecttest.com"）
    pub host: String,
    /// 请求路径（如 "/generate_204" / "/hotspot-detect.html"）
    pub path: String,
}

/// Portal 响应（按探测来源决定重定向/落地页/放行）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortalResponse {
    /// 302 重定向到指定 URL（通常是落地页或认证入口）
    Redirect {
        /// 重定向目标 URL
        url: String,
    },
    /// 直接返回落地页 HTML（200）
    Landing {
        /// 落地页 HTML 内容
        html: String,
    },
    /// 放行（访客已认证，返回正常响应让其通过）
    Pass,
}

// ----------------------------------------------------------------------------
// OS 探测识别 + Portal 流程状态机（纯逻辑，可单测）
// ----------------------------------------------------------------------------

/// 探测来源 OS（识别后用于定制响应）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOs {
    /// iOS / iPadOS（captive.apple.com / hotspot.html）
    Ios,
    /// Android（connectivitycheck.gstatic.com / generate_204）
    Android,
    /// Windows（msftconnecttest.com / ncsi.txt）
    Windows,
    /// macOS（captive.apple.com）
    Macos,
    /// Linux（connectivity-check.ubuntu.com 等）
    Linux,
    /// 未知来源
    Unknown,
}

/// 默认落地页 URL（重定向目标）。
pub const DEFAULT_LANDING_URL: &str = "/portal/landing";

/// 默认落地页 HTML（最小可工作版本，含认证入口）。
pub const DEFAULT_LANDING_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>OS 访客认证</title></head><body><h1>欢迎使用 OS 访客网络</h1><p>请完成认证以接入。</p><a href=\"/portal/auth\">前往认证</a></body></html>";

/// 根据 User-Agent / Host / Path 识别探测来源 OS（纯匹配）。
///
/// 识别规则（各 OS 联网检测探测特征）：
/// - iOS/macOS：host 含 `captive.apple.com` 或 path 含 `hotspot-detect.html`；
///   UA 含 `iPhone`/`iPad` → iOS，含 `Macintosh` → macOS。
/// - Android：host 含 `clients3.google.com` / `connectivitycheck.gstatic.com`，
///   或 path 含 `generate_204`；UA 含 `Android` 或 `Curl`（部分 ROM）。
/// - Windows：host 含 `msftconnecttest.com` 或 `www.msftncsi.com`；
///   path 含 `ncsi.txt` / `connecttest.txt`。
/// - Linux：host 含 `connectivity-check.ubuntu.com` / `firefox` 等。
/// - 其余 → Unknown。
pub fn detect_probe_os(user_agent: &str, host: &str, path: &str) -> ProbeOs {
    let ua = user_agent.to_lowercase();
    let h = host.to_lowercase();
    let p = path.to_lowercase();

    // Apple 系（iOS / macOS 共用 captive.apple.com）。
    if h.contains("captive.apple.com")
        || p.contains("hotspot-detect.html")
        || p.contains("/hotspot.html")
    {
        if ua.contains("ipad") || ua.contains("iphone") || ua.contains("ios") {
            return ProbeOs::Ios;
        }
        return ProbeOs::Macos;
    }
    // Android。
    if h.contains("connectivitycheck.gstatic.com")
        || h.contains("clients3.google.com")
        || h.contains("connectivitycheck.android.com")
        || p.contains("generate_204")
        || p.contains("/gen_204")
        || ua.contains("android")
        || ua.contains("pixel")
    {
        return ProbeOs::Android;
    }
    // Windows。
    if h.contains("msftconnecttest.com")
        || h.contains("msftncsi.com")
        || p.contains("ncsi.txt")
        || p.contains("connecttest.txt")
    {
        return ProbeOs::Windows;
    }
    // Linux。
    if h.contains("connectivity-check.ubuntu.com")
        || h.contains("detectportal.firefox.com")
        || ua.contains("linux")
        || ua.contains("ubuntu")
    {
        return ProbeOs::Linux;
    }
    ProbeOs::Unknown
}

/// Portal 流程状态机（Landing → Register → Success）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalState {
    /// 落地页（访客被拦截后看到的第一页）
    Landing,
    /// 注册/认证中（访客提交了认证表单）
    Register,
    /// 认证成功（应放行）
    Success,
}

impl PortalState {
    /// 初始状态。
    pub fn initial() -> Self {
        PortalState::Landing
    }

    /// 状态转移（给定输入事件，返回下一状态；非法转移返回 None）。
    ///
    /// - Landing --Submit--> Register
    /// - Register --Approve--> Success
    /// - Register --Reject--> Landing
    /// - Success --Reset--> Landing
    /// - 其他 → None（非法）
    pub fn transition(self, event: PortalEvent) -> Option<PortalState> {
        match (self, event) {
            (PortalState::Landing, PortalEvent::Submit) => Some(PortalState::Register),
            (PortalState::Register, PortalEvent::Approve) => Some(PortalState::Success),
            (PortalState::Register, PortalEvent::Reject) => Some(PortalState::Landing),
            (PortalState::Success, PortalEvent::Reset) => Some(PortalState::Landing),
            _ => None,
        }
    }
}

/// Portal 流程输入事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalEvent {
    /// 访客提交认证表单
    Submit,
    /// 后端批准认证
    Approve,
    /// 后端拒绝认证
    Reject,
    /// 重置回落地页
    Reset,
}

/// 决定对一次探测请求返回何种响应（纯逻辑）。
///
/// - `authed=true`：放行（Pass）。
/// - `authed=false`：未认证时返回 Landing（用 config.landing_html 或默认）。
pub fn decide_response(authed: bool, config: &PortalConfig, _os: ProbeOs) -> PortalResponse {
    if authed {
        return PortalResponse::Pass;
    }
    // 未认证：优先返回落地页 HTML（确保客户端能展示）；
    // 部分 OS 探测在收到 200 + 落地页时停止探测并弹窗。
    let html = config
        .landing_html
        .clone()
        .unwrap_or_else(|| DEFAULT_LANDING_HTML.to_string());
    PortalResponse::Landing { html }
}

// ----------------------------------------------------------------------------
// CaptivePortal trait（async）
// ----------------------------------------------------------------------------

/// Captive Portal——拦截未认证访客并引导认证。
///
/// 实现者：`HttpCaptivePortal`（默认，基于 hyper/warp + nftables 重定向）；
/// 与 nft 模块协同（认证成功后由 NftRuleOrchestrator 放行该访客 IP）。
#[allow(async_fn_in_trait)]
pub trait CaptivePortal: Send + Sync {
    /// 启动 Portal（按 `config` 监听并拦截探测）。
    async fn start(&self, config: PortalConfig) -> Result<(), crate::GuestError>;

    /// 停止 Portal。
    async fn stop(&self) -> Result<(), crate::GuestError>;

    /// 处理一次探测请求——兼容 iOS/Android/Win/macOS，返回 302 重定向/落地页/放行。
    async fn handle_detection(
        &self,
        request: ProbeRequest,
    ) -> Result<PortalResponse, crate::GuestError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PortalConfig {
        PortalConfig {
            listen_http: 80,
            listen_https: 443,
            vlan_id: Some(100),
            landing_html: None,
            ap_bridge: false,
        }
    }

    #[test]
    fn detect_ios() {
        assert_eq!(
            detect_probe_os(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0)",
                "captive.apple.com",
                "/hotspot-detect.html"
            ),
            ProbeOs::Ios
        );
    }

    #[test]
    fn detect_macos() {
        assert_eq!(
            detect_probe_os(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X)",
                "captive.apple.com",
                "/hotspot-detect.html"
            ),
            ProbeOs::Macos
        );
    }

    #[test]
    fn detect_android() {
        assert_eq!(
            detect_probe_os(
                "Mozilla/5.0 (Linux; Android 14; Pixel)",
                "connectivitycheck.gstatic.com",
                "/generate_204"
            ),
            ProbeOs::Android
        );
    }

    #[test]
    fn detect_windows() {
        assert_eq!(
            detect_probe_os("Microsoft", "msftconnecttest.com", "/connecttest.txt"),
            ProbeOs::Windows
        );
    }

    #[test]
    fn detect_linux() {
        assert_eq!(
            detect_probe_os(
                "Mozilla/5.0 (X11; Linux x86_64; Ubuntu)",
                "connectivity-check.ubuntu.com",
                "/"
            ),
            ProbeOs::Linux
        );
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(
            detect_probe_os("curl/8", "example.com", "/"),
            ProbeOs::Unknown
        );
    }

    #[test]
    fn state_machine_transitions() {
        let s = PortalState::initial();
        assert_eq!(s, PortalState::Landing);

        let s = s.transition(PortalEvent::Submit).unwrap();
        assert_eq!(s, PortalState::Register);

        let s = s.transition(PortalEvent::Approve).unwrap();
        assert_eq!(s, PortalState::Success);

        // Reset 回 Landing。
        let s = s.transition(PortalEvent::Reset).unwrap();
        assert_eq!(s, PortalState::Landing);

        // Reject 回 Landing。
        let r = PortalState::Register
            .transition(PortalEvent::Reject)
            .unwrap();
        assert_eq!(r, PortalState::Landing);

        // 非法转移返回 None。
        assert!(PortalState::Landing
            .transition(PortalEvent::Approve)
            .is_none());
        assert!(PortalState::Success
            .transition(PortalEvent::Submit)
            .is_none());
    }

    #[test]
    fn decide_response_authed_pass() {
        let c = cfg();
        assert!(matches!(
            decide_response(true, &c, ProbeOs::Ios),
            PortalResponse::Pass
        ));
    }

    #[test]
    fn decide_response_unauthed_returns_landing() {
        let c = cfg();
        let resp = decide_response(false, &c, ProbeOs::Android);
        match resp {
            PortalResponse::Landing { html } => assert!(html.contains("OS")),
            other => panic!("期望 Landing，实际 {other:?}"),
        }
    }

    #[test]
    fn decide_response_uses_custom_landing_html() {
        let mut c = cfg();
        c.landing_html = Some("<h1>custom</h1>".into());
        let resp = decide_response(false, &c, ProbeOs::Windows);
        match resp {
            PortalResponse::Landing { html } => assert_eq!(html, "<h1>custom</h1>"),
            other => panic!("期望 Landing，实际 {other:?}"),
        }
    }
}
