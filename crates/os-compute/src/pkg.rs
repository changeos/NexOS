//! 第三方包管理（os-pkg，编排 apt/dpkg）
//!
//! 实现说明（规划文档 §3.4）：
//! - `install` 编排 `dpkg -i` / `apt-get install` 安装 .deb 包
//! - 第三方带图标的应用归"未知来源"（`PackageSource::ThirdParty`），区别于官方源

use std::path::PathBuf;

use os_core::{Deserialize, Serialize};

use crate::ComputeResult;

/// 包 ID（deb 包名，newtype String）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageId(pub String);

impl PackageId {
    /// 构造包 ID
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 借包名
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 包来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSource {
    /// 第三方（手动 .deb，带图标应用归"未知来源"）
    ThirdParty,
    /// 官方源（apt 仓库）
    Official,
}

/// 已安装包信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    /// 包 ID
    pub id: PackageId,
    /// 版本
    pub version: String,
    /// 描述
    pub description: String,
    /// 安装时间
    pub installed_at: chrono::DateTime<chrono::Utc>,
    /// 图标路径（None = 无图标）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<PathBuf>,
    /// .desktop 文件路径（None = 非桌面应用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_file: Option<PathBuf>,
    /// 来源
    pub source: PackageSource,
}

/// 包管理器——编排 apt/dpkg。
#[allow(async_fn_in_trait)]
pub trait PackageManager: Send + Sync {
    /// 安装 .deb 包（dpkg/apt 编排），返回安装后的包信息。
    async fn install(&self, deb_path: &std::path::Path) -> ComputeResult<PackageInfo>;

    /// 卸载包。
    async fn uninstall(&self, id: &PackageId) -> ComputeResult<()>;

    /// 升级包（到最新可用版本）。
    async fn upgrade(&self, id: &PackageId) -> ComputeResult<PackageInfo>;

    /// 列出已安装包。
    async fn list_installed(&self) -> ComputeResult<Vec<PackageInfo>>;

    /// 搜索包（按关键词）。
    async fn search(&self, query: &str) -> ComputeResult<Vec<PackageInfo>>;
}

// ----------------------------------------------------------------------------
// 包生命周期状态机（Installed / Removed）
// ----------------------------------------------------------------------------

/// 包生命周期状态。
///
/// 与容器状态机不同——包状态由 install/uninstall 显式驱动，无 Paused 等中间态。
/// 由实现层（`DpkgPackageManager`/`MockPackageManager`）维护。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageState {
    /// 已安装
    Installed,
    /// 已卸载
    Removed,
}

/// 包状态机合法迁移：仅 Installed → Removed / Removed → Installed。
pub fn can_transition_package(from: PackageState, to: PackageState) -> bool {
    use PackageState::*;
    matches!((from, to), (Installed, Removed) | (Removed, Installed))
}

// ----------------------------------------------------------------------------
// PackageInfo / PackageId 辅助构造器
// ----------------------------------------------------------------------------

impl PackageInfo {
    /// 构造官方源包信息。
    pub fn official(id: PackageId, version: impl Into<String>) -> Self {
        Self {
            id,
            version: version.into(),
            description: String::new(),
            installed_at: chrono::Utc::now(),
            icon_path: None,
            desktop_file: None,
            source: PackageSource::Official,
        }
    }

    /// 构造第三方包信息（带图标应用归"未知来源"）。
    pub fn third_party(id: PackageId, version: impl Into<String>) -> Self {
        Self {
            id,
            version: version.into(),
            description: String::new(),
            installed_at: chrono::Utc::now(),
            icon_path: None,
            desktop_file: None,
            source: PackageSource::ThirdParty,
        }
    }

    /// 设置描述。
    pub fn with_description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    /// 设置图标路径（同时把来源标记为 ThirdParty——有图标的桌面应用一律归"未知来源"）。
    pub fn with_icon(mut self, path: PathBuf) -> Self {
        self.icon_path = Some(path);
        if self.source == PackageSource::Official {
            self.source = PackageSource::ThirdParty;
        }
        self
    }

    /// 设置 .desktop 文件路径。
    pub fn with_desktop(mut self, path: PathBuf) -> Self {
        self.desktop_file = Some(path);
        self
    }

    /// 是否带图标的图形应用（即"未知来源"语义）。
    pub fn is_third_party_app(&self) -> bool {
        self.icon_path.is_some() && self.source == PackageSource::ThirdParty
    }
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- PackageId ----------------

    #[test]
    fn package_id_new_and_as_str() {
        let id = PackageId::new("nginx");
        assert_eq!(id.as_str(), "nginx");
        assert_eq!(id.0, "nginx");
    }

    #[test]
    fn package_id_new_accepts_string() {
        let id = PackageId::new(String::from("redis"));
        assert_eq!(id.as_str(), "redis");
    }

    #[test]
    fn package_id_display() {
        let id = PackageId::new("postgres");
        assert_eq!(format!("{id}"), "postgres");
    }

    #[test]
    fn package_id_eq_and_hash() {
        let a = PackageId::new("nginx");
        let b = PackageId::new("nginx");
        let c = PackageId::new("redis");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut set = std::collections::HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn package_id_serde_roundtrip() {
        let id = PackageId::new("code");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""code""#);
        let back: PackageId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    // ---------------- PackageSource ----------------

    #[test]
    fn package_source_serde_snake_case() {
        let off_json = serde_json::to_string(&PackageSource::Official).unwrap();
        assert_eq!(off_json, r#""official""#);
        let tp_json = serde_json::to_string(&PackageSource::ThirdParty).unwrap();
        assert_eq!(tp_json, r#""third_party""#);
        // 反序列化
        let back: PackageSource = serde_json::from_str(&off_json).unwrap();
        assert_eq!(back, PackageSource::Official);
        let back: PackageSource = serde_json::from_str(&tp_json).unwrap();
        assert_eq!(back, PackageSource::ThirdParty);
    }

    // ---------------- PackageState ----------------

    #[test]
    fn can_transition_package_legal() {
        assert!(can_transition_package(
            PackageState::Installed,
            PackageState::Removed
        ));
        assert!(can_transition_package(
            PackageState::Removed,
            PackageState::Installed
        ));
    }

    #[test]
    fn can_transition_package_illegal_self() {
        // 自身 → 自身 不允许
        assert!(!can_transition_package(
            PackageState::Installed,
            PackageState::Installed
        ));
        assert!(!can_transition_package(
            PackageState::Removed,
            PackageState::Removed
        ));
    }

    #[test]
    fn package_state_serde_snake_case() {
        let ins = serde_json::to_string(&PackageState::Installed).unwrap();
        assert_eq!(ins, r#""installed""#);
        let rem = serde_json::to_string(&PackageState::Removed).unwrap();
        assert_eq!(rem, r#""removed""#);
        let back: PackageState = serde_json::from_str(&rem).unwrap();
        assert_eq!(back, PackageState::Removed);
    }

    // ---------------- PackageInfo builders ----------------

    #[test]
    fn package_info_official_defaults() {
        let pi = PackageInfo::official(PackageId::new("nginx"), "1.25");
        assert_eq!(pi.id.as_str(), "nginx");
        assert_eq!(pi.version, "1.25");
        assert!(pi.description.is_empty());
        assert_eq!(pi.source, PackageSource::Official);
        assert!(pi.icon_path.is_none());
        assert!(pi.desktop_file.is_none());
        assert!(!pi.is_third_party_app());
    }

    #[test]
    fn package_info_third_party_defaults() {
        let pi = PackageInfo::third_party(PackageId::new("code"), "1.85.0");
        assert_eq!(pi.id.as_str(), "code");
        assert_eq!(pi.version, "1.85.0");
        assert_eq!(pi.source, PackageSource::ThirdParty);
        // 无 icon 仍不算 third-party app
        assert!(!pi.is_third_party_app());
    }

    #[test]
    fn package_info_with_description_builder() {
        let pi = PackageInfo::official(PackageId::new("x"), "1").with_description("hello world");
        assert_eq!(pi.description, "hello world");
    }

    #[test]
    fn package_info_with_desktop_builder() {
        let pi = PackageInfo::official(PackageId::new("x"), "1")
            .with_desktop(PathBuf::from("/usr/share/applications/x.desktop"));
        assert_eq!(
            pi.desktop_file.as_deref(),
            Some(std::path::Path::new("/usr/share/applications/x.desktop"))
        );
    }

    #[test]
    fn with_icon_flips_official_to_third_party() {
        // 官方源包加图标 → 自动归 ThirdParty（"未知来源"语义）
        let pi = PackageInfo::official(PackageId::new("x"), "1")
            .with_icon(PathBuf::from("/usr/share/icons/x.png"));
        assert_eq!(pi.source, PackageSource::ThirdParty);
        assert!(pi.icon_path.is_some());
        // 有图标 + ThirdParty → is_third_party_app == true
        assert!(pi.is_third_party_app());
    }

    #[test]
    fn with_icon_on_third_party_keeps_source() {
        // 已是 ThirdParty，加图标保持不变
        let pi = PackageInfo::third_party(PackageId::new("x"), "1")
            .with_icon(PathBuf::from("/icon.png"));
        assert_eq!(pi.source, PackageSource::ThirdParty);
        assert!(pi.is_third_party_app());
    }

    #[test]
    fn with_desktop_does_not_flip_source() {
        // 仅加 .desktop 不改 source（无图标 → 不算 third-party app）
        let pi = PackageInfo::official(PackageId::new("x"), "1")
            .with_desktop(PathBuf::from("/x.desktop"));
        assert_eq!(pi.source, PackageSource::Official);
        assert!(!pi.is_third_party_app());
    }

    #[test]
    fn is_third_party_app_requires_both_icon_and_source() {
        // 仅 ThirdParty 无图标
        let mut pi = PackageInfo::third_party(PackageId::new("x"), "1");
        assert!(!pi.is_third_party_app());
        // 加图标 → 通过
        pi.icon_path = Some(PathBuf::from("/i.png"));
        assert!(pi.is_third_party_app());
        // source 改回 Official
        pi.source = PackageSource::Official;
        assert!(!pi.is_third_party_app());
    }

    #[test]
    fn builder_chain_combines_all_setters() {
        let pi = PackageInfo::official(PackageId::new("code"), "1.85")
            .with_description("Code editor")
            .with_icon(PathBuf::from("/icons/code.png"))
            .with_desktop(PathBuf::from("/applications/code.desktop"));
        // icon 翻转了 source
        assert_eq!(pi.source, PackageSource::ThirdParty);
        assert_eq!(pi.description, "Code editor");
        assert!(pi.is_third_party_app());
    }

    #[test]
    fn package_info_serde_roundtrip() {
        let pi = PackageInfo::third_party(PackageId::new("x"), "1.0")
            .with_description("desc")
            .with_icon(PathBuf::from("/i.png"))
            .with_desktop(PathBuf::from("/d.desktop"));
        let json = serde_json::to_string(&pi).unwrap();
        let back: PackageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, pi.id);
        assert_eq!(back.version, pi.version);
        assert_eq!(back.description, pi.description);
        assert_eq!(back.icon_path, pi.icon_path);
        assert_eq!(back.desktop_file, pi.desktop_file);
        assert_eq!(back.source, pi.source);
    }

    #[test]
    fn package_info_serde_skips_none_optional_fields() {
        let pi = PackageInfo::official(PackageId::new("x"), "1");
        let json = serde_json::to_string(&pi).unwrap();
        // None 字段应被 skip_serializing
        assert!(!json.contains("icon_path"));
        assert!(!json.contains("desktop_file"));
    }
}
