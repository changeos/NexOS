//! 可安装 ISO 打包（规划文档 §3.11 / §3.19）
//!
//! 两种变体：
//! - `Standard`：通用安装 ISO（构建期含选定组件二进制）
//! - `Clone`：克隆变体（内嵌当前节点配置快照，用于整机复刻）
//!
//! 构建编排 xorriso + squashfs；安装期排除 §3.19 清单中的敏感项。

use os_core::{DateTime, TaskId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ----------------------------------------------------------------------------
// ISO 变体与规格
// ----------------------------------------------------------------------------

/// ISO 变体
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IsoVariant {
    /// 标准安装 ISO（通用）
    Standard,
    /// 克隆变体（内嵌配置快照，用于复刻当前节点）
    Clone {
        /// 配置快照（结构化导出，已按 §3.19 排除敏感项）
        config_snapshot: serde_json::Value,
    },
}

impl IsoVariant {
    /// 是否克隆变体。
    pub fn is_clone(&self) -> bool {
        matches!(self, Self::Clone { .. })
    }

    /// 取克隆变体内嵌的配置快照（标准变体返回 None）。
    pub fn config_snapshot(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Clone { config_snapshot } => Some(config_snapshot),
            Self::Standard => None,
        }
    }
}

/// §3.19 排除清单中的敏感键（克隆快照内嵌前须剔除）。
///
/// 命中即丢弃——克隆变体可复刻拓扑与组件配置，但绝不携带任何可还原凭据的原始材料
/// （密码哈希、令牌、私钥、证书私钥、SSH 私钥等）。匹配大小写不敏感。
pub const SENSITIVE_CONFIG_KEYS: &[&str] = &[
    "password",
    "password_hash",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "private_key",
    "priv_key",
    "privatekey",
    "ssh_key",
    "ssh_private_key",
    "certificate_key",
    "cert_key",
    "mnemonic",
    "seed",
    "refresh_token",
    "access_token",
];

/// 递归过滤 JSON 值中键名命中 §3.19 排除清单的字段（大小写不敏感）。
///
/// - 对 Object：递归每个字段，键名（转小写）命中清单或包含清单任一子串则丢弃整支子树。
/// - 对 Array：对每个元素递归过滤（数组本身不命键名）。
/// - 原始类型原样返回。
///
/// 返回过滤后的新值（不修改输入）。供克隆变体内嵌 config_snapshot 前清洗。
pub fn filter_sensitive(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if is_sensitive_key(k) {
                    // 命中排除清单：整支丢弃（不进 out）
                    continue;
                }
                out.insert(k.clone(), filter_sensitive(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(filter_sensitive).collect()),
        other => other.clone(),
    }
}

/// 判断键名是否为敏感项（命中 SENSITIVE_CONFIG_KEYS 任一，大小写不敏感）。
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_CONFIG_KEYS
        .iter()
        .any(|s| lower == *s || lower.contains(s))
}

/// ISO 构建规格
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsoSpec {
    /// 变体（标准/克隆）
    pub variant: IsoVariant,
    /// 基础镜像（rootfs/squashfs 来源）
    pub base_image: String,
    /// 包含的组件二进制列表（如 `["osd","os-storage","os-wallet"]`）
    pub components: Vec<String>,
    /// Ubuntu 基础版本（如 `24.04`）
    pub ubuntu_version: String,
    /// 目标架构（`x86_64` / `aarch64`）
    pub arch: String,
    /// 区域（如 `zh_CN.UTF-8`）
    pub locale: String,
}

impl IsoSpec {
    /// 校验构建规格合法（架构、版本、组件非空、base_image 非空）。
    ///
    /// 不校验工具链存在性（属运行期硬阻塞，由 builder 在调用前探针）。
    pub fn validate(&self) -> Result<(), crate::IsoError> {
        if self.base_image.trim().is_empty() {
            return Err(crate::IsoError::BuildFailed(
                "base_image 不能为空".to_string(),
            ));
        }
        if self.ubuntu_version.trim().is_empty() {
            return Err(crate::IsoError::BuildFailed(
                "ubuntu_version 不能为空".to_string(),
            ));
        }
        if self.components.is_empty() {
            return Err(crate::IsoError::BuildFailed(
                "components 至少需要一个组件".to_string(),
            ));
        }
        match self.arch.as_str() {
            "x86_64" | "aarch64" => {}
            other => {
                return Err(crate::IsoError::BuildFailed(format!(
                    "不支持的架构：{other}（仅 x86_64 / aarch64）"
                )));
            }
        }
        if self.locale.trim().is_empty() {
            return Err(crate::IsoError::BuildFailed("locale 不能为空".to_string()));
        }
        Ok(())
    }

    /// 若为克隆变体，对其 config_snapshot 做敏感项过滤（就地清洗）。
    ///
    /// 呼应 §3.19：克隆快照绝不携带密码/令牌/私钥等可还原凭据。
    pub fn sanitize_clone_snapshot(&mut self) {
        if let IsoVariant::Clone { config_snapshot } = &mut self.variant {
            let filtered = filter_sensitive(config_snapshot);
            *config_snapshot = filtered;
        }
    }
}

/// ISO 构建产物
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsoBuildResult {
    /// 产物 ISO 文件路径
    pub iso_path: PathBuf,
    /// SHA256 校验和
    pub sha256: String,
    /// 文件大小（字节）
    pub size_bytes: u64,
    /// 构建完成时间（UTC）
    pub built_at: DateTime,
}

// ----------------------------------------------------------------------------
// 构建状态
// ----------------------------------------------------------------------------

/// ISO 构建状态机
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum IsoBuildStatus {
    /// 排队中
    Pending,
    /// 构建中（附当前步骤与进度）
    Building {
        /// 当前步骤（如 `"squashfs"` / `"xorriso"`）
        step: String,
        /// 进度 0.0 ~ 1.0
        progress: f32,
    },
    /// 完成（附产物）
    Completed(IsoBuildResult),
    /// 失败（附原因）
    Failed {
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// IsoBuilder trait（async）
// ----------------------------------------------------------------------------

/// ISO 构建器——编排 xorriso/squashfs 产出可安装 ISO。
///
/// 实现者：`XorrisoIsoBuilder`（默认）。构建为异步任务，通过 `status` 轮询。
#[allow(async_fn_in_trait)]
pub trait IsoBuilder: Send + Sync {
    /// 异步发起 ISO 构建（返回任务 ID）。
    async fn build(&self, spec: IsoSpec) -> Result<TaskId, crate::IsoError>;

    /// 查询构建任务状态。
    async fn status(&self, task: &TaskId) -> IsoBuildStatus;

    /// 校验既有 ISO（sha256 与期望值比对）。
    async fn verify(
        &self,
        iso_path: &std::path::Path,
        expected_sha256: &str,
    ) -> Result<bool, crate::IsoError>;
}

// ----------------------------------------------------------------------------
// 单元测试（数据结构构造/校验/敏感项过滤）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn std_spec() -> IsoSpec {
        IsoSpec {
            variant: IsoVariant::Standard,
            base_image: "ubuntu-24.04-base.squashfs".to_string(),
            components: vec!["osd".to_string(), "os-storage".to_string()],
            ubuntu_version: "24.04".to_string(),
            arch: "x86_64".to_string(),
            locale: "zh_CN.UTF-8".to_string(),
        }
    }

    fn clone_spec(snapshot: serde_json::Value) -> IsoSpec {
        IsoSpec {
            variant: IsoVariant::Clone {
                config_snapshot: snapshot,
            },
            base_image: "ubuntu-24.04-base.squashfs".to_string(),
            components: vec!["osd".to_string()],
            ubuntu_version: "24.04".to_string(),
            arch: "aarch64".to_string(),
            locale: "en_US.UTF-8".to_string(),
        }
    }

    // —— IsoSpec::validate ——

    #[test]
    fn validate_ok() {
        assert!(std_spec().validate().is_ok());
        assert!(clone_spec(json!({})).validate().is_ok());
    }

    #[test]
    fn validate_empty_base_image() {
        let mut s = std_spec();
        s.base_image = String::new();
        let err = s.validate().unwrap_err();
        assert!(matches!(err, crate::IsoError::BuildFailed(_)));
        assert!(err.to_string().contains("base_image"));
    }

    #[test]
    fn validate_empty_ubuntu_version() {
        let mut s = std_spec();
        s.ubuntu_version = "  ".to_string();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_empty_components() {
        let mut s = std_spec();
        s.components.clear();
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("components"));
    }

    #[test]
    fn validate_bad_arch() {
        for bad in ["mips", "armv7", "", "x86"] {
            let mut s = std_spec();
            s.arch = bad.to_string();
            assert!(s.validate().is_err(), "arch={bad} 应被判非法");
        }
    }

    #[test]
    fn validate_good_arch() {
        for good in ["x86_64", "aarch64"] {
            let mut s = std_spec();
            s.arch = good.to_string();
            assert!(s.validate().is_ok());
        }
    }

    #[test]
    fn validate_empty_locale() {
        let mut s = std_spec();
        s.locale = String::new();
        assert!(s.validate().is_err());
    }

    // —— IsoVariant helpers ——

    #[test]
    fn variant_helpers() {
        let std = IsoVariant::Standard;
        assert!(!std.is_clone());
        assert!(std.config_snapshot().is_none());
        let cl = IsoVariant::Clone {
            config_snapshot: json!({"a": 1}),
        };
        assert!(cl.is_clone());
        assert_eq!(cl.config_snapshot(), Some(&json!({"a": 1})));
    }

    // —— 敏感项过滤 ——

    #[test]
    fn is_sensitive_key_matches() {
        assert!(is_sensitive_key("password"));
        assert!(is_sensitive_key("PASSWORD")); // 大小写不敏感
        assert!(is_sensitive_key("root_password_hash"));
        assert!(is_sensitive_key("api_key"));
        assert!(is_sensitive_key("private_key"));
        assert!(is_sensitive_key("ssh_private_key"));
        assert!(is_sensitive_key("my_token"));
        assert!(is_sensitive_key("refresh_token"));
        assert!(is_sensitive_key("mnemonic"));
    }

    #[test]
    fn is_sensitive_key_non_matches() {
        assert!(!is_sensitive_key("hostname"));
        assert!(!is_sensitive_key("pool_name"));
        assert!(!is_sensitive_key("component_list"));
        assert!(!is_sensitive_key("network_config"));
        assert!(!is_sensitive_key("disk_size"));
    }

    #[test]
    fn filter_removes_top_level_sensitive() {
        let cfg = json!({
            "hostname": "os-1",
            "root_password": "$6$secret",
            "api_token": "abc123",
            "components": ["osd"]
        });
        let out = filter_sensitive(&cfg);
        assert!(out.get("root_password").is_none());
        assert!(out.get("api_token").is_none());
        assert_eq!(out.get("hostname").unwrap(), "os-1");
        assert!(out.get("components").is_some());
    }

    #[test]
    fn filter_recurses_into_objects_and_arrays() {
        let cfg = json!({
            "users": [
                { "name": "admin", "password_hash": "xxx" },
                { "name": "guest" }
            ],
            "network": { "ip": "10.0.0.1", "api_key": "secret" }
        });
        let out = filter_sensitive(&cfg);
        let users = out.get("users").unwrap().as_array().unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].get("name").unwrap(), "admin");
        assert!(users[0].get("password_hash").is_none());
        assert_eq!(users[1].get("name").unwrap(), "guest");
        let net = out.get("network").unwrap();
        assert_eq!(net.get("ip").unwrap(), "10.0.0.1");
        assert!(net.get("api_key").is_none()); // api_key 命中清单
    }

    #[test]
    fn filter_preserves_non_sensitive_scalars() {
        let cfg = json!({"port": 22, "ssl": true, "name": null});
        let out = filter_sensitive(&cfg);
        assert_eq!(out.get("port").unwrap(), &json!(22));
        assert_eq!(out.get("ssl").unwrap(), &json!(true));
        assert_eq!(out.get("name").unwrap(), &json!(null));
    }

    #[test]
    fn sanitize_clone_strips_sensitive() {
        let mut s = clone_spec(json!({
            "hostname": "node-a",
            "private_key": "-----BEGIN PRIVATE KEY-----...",
            "pool": { "name": "tank", "size": 100 }
        }));
        s.sanitize_clone_snapshot();
        if let IsoVariant::Clone { config_snapshot } = &s.variant {
            assert!(config_snapshot.get("private_key").is_none());
            assert!(config_snapshot.get("hostname").is_some());
            assert_eq!(
                config_snapshot.get("pool").unwrap().get("name").unwrap(),
                "tank"
            );
        } else {
            panic!("应是 Clone");
        }
    }

    #[test]
    fn sanitize_does_not_touch_standard() {
        let mut s = std_spec();
        s.sanitize_clone_snapshot();
        assert!(matches!(s.variant, IsoVariant::Standard));
    }

    #[test]
    fn sensitive_keys_list_nonempty() {
        assert!(!SENSITIVE_CONFIG_KEYS.is_empty());
        assert!(SENSITIVE_CONFIG_KEYS.contains(&"password"));
    }
}
