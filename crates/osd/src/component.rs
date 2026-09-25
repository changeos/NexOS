//! 组件模型 —— ComponentId / ComponentStatus / ComponentDescriptor
//!
//! osd 编排的对象是「业务组件进程」（如 os-storage、os-meta、os-api）。
//! 每个组件有：唯一 ID、依赖列表、资源配额、健康探针配置。

use os_core::{Deserialize, ResourceQuota, Serialize};

/// 组件 ID（如 `"os-storage"` / `"os-meta"` / `"os-api"`）
///
/// 与 crate 名对应，便于日志/告警溯源。newtype 防止与裸 String 混淆。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComponentId(pub String);

impl ComponentId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ComponentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// 组件运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentStatus {
    /// 启动中（拉起进程后、健康探针首次通过前）
    Starting,
    /// 运行中（健康探针通过）
    Running,
    /// 已停止（手动停止或正常退出）
    Stopped,
    /// 失败（异常退出 / 健康探针连续失败）
    Failed,
    /// 已禁用（配置中标记为不启动，编排器跳过）
    Disabled,
}

/// 健康探针配置（osd 如何探测该组件是否存活）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthProbeConfig {
    /// 探针类型（实现自定义，如 `"tcp"` / `"http"` / `"exec"`）
    pub kind: String,
    /// 探针目标（如 `"127.0.0.1:8080/health"` 或可执行路径）
    pub target: String,
    /// 探测间隔（秒）
    pub interval_secs: u32,
    /// 超时（秒）
    pub timeout_secs: u32,
    /// 连续失败多少次才判定 Failed
    pub failure_threshold: u32,
}

/// 组件描述符（声明式注册表项）
///
/// osd 启动时读取所有 `ComponentDescriptor`，按 `dependencies` 做拓扑排序后逐个拉起。
/// 配置来源：嵌入式默认 + 运行时可覆盖（实现在 owner agent 提供）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentDescriptor {
    /// 组件 ID
    pub id: ComponentId,
    /// 依赖的其他组件 ID（必须先于本组件启动）
    pub dependencies: Vec<ComponentId>,
    /// 资源配额（cgroup v2 限制）
    pub quota: ResourceQuota,
    /// 健康探针配置
    pub health_probe: HealthProbeConfig,
    /// 启动命令（实现可解释，如二进制路径 + 参数）
    #[serde(default)]
    pub command: Option<String>,
    /// 是否启用（false 则编排器跳过该组件）
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

// ----------------------------------------------------------------------------
// 单元测试：构造 + Display + From + serde 往返 + 默认值
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ComponentId ----

    #[test]
    fn component_id_new_and_as_str() {
        let id = ComponentId::new("os-storage");
        assert_eq!(id.as_str(), "os-storage");
        assert_eq!(id.0, "os-storage");
    }

    #[test]
    fn component_id_new_accepts_string() {
        let owned = String::from("os-meta");
        let id = ComponentId::new(owned);
        assert_eq!(id.as_str(), "os-meta");
    }

    #[test]
    fn component_id_display_writes_inner() {
        let id = ComponentId::new("osd-api");
        assert_eq!(format!("{id}"), "osd-api");
    }

    #[test]
    fn component_id_from_string_preserves_value() {
        let s = String::from("from-string");
        let id = ComponentId::from(s);
        assert_eq!(id.as_str(), "from-string");
    }

    #[test]
    fn component_id_eq_hash_consistent() {
        let a = ComponentId::new("x");
        let b = ComponentId::new("x");
        let c = ComponentId::new("y");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Hash 一致（HashMap 能定位）
        let mut m = std::collections::HashMap::new();
        m.insert(a.clone(), 1);
        assert_eq!(m.get(&b), Some(&1));
    }

    #[test]
    fn component_id_serde_roundtrip() {
        let id = ComponentId::new("serde-id");
        let json = serde_json::to_string(&id).expect("序列化");
        // newtype 序列化为裸字符串
        assert_eq!(json, "\"serde-id\"");
        let back: ComponentId = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, id);
    }

    // ---- ComponentStatus ----

    #[test]
    fn component_status_serde_snake_case_roundtrip() {
        for s in [
            ComponentStatus::Starting,
            ComponentStatus::Running,
            ComponentStatus::Stopped,
            ComponentStatus::Failed,
            ComponentStatus::Disabled,
        ] {
            let json = serde_json::to_string(&s).expect("序列化");
            let back: ComponentStatus = serde_json::from_str(&json).expect("反序列化");
            assert_eq!(back, s, "状态 {s:?} 往返失败（json={json}）");
        }
    }

    #[test]
    fn component_status_serde_uses_snake_case() {
        // 验证 serde rename_all = "snake_case"：Running → "running"（非 "Running"）
        assert_eq!(
            serde_json::to_string(&ComponentStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ComponentStatus::Starting).unwrap(),
            "\"starting\""
        );
    }

    #[test]
    fn component_status_copy_clone_eq() {
        let a = ComponentStatus::Running;
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a; // Clone via Copy
        assert_eq!(a, c);
    }

    // ---- HealthProbeConfig ----

    #[test]
    fn health_probe_config_serde_roundtrip() {
        let cfg = HealthProbeConfig {
            kind: "tcp".into(),
            target: "127.0.0.1:8080".into(),
            interval_secs: 5,
            timeout_secs: 2,
            failure_threshold: 3,
        };
        let json = serde_json::to_string(&cfg).expect("序列化");
        let back: HealthProbeConfig = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.kind, cfg.kind);
        assert_eq!(back.target, cfg.target);
        assert_eq!(back.interval_secs, cfg.interval_secs);
        assert_eq!(back.timeout_secs, cfg.timeout_secs);
        assert_eq!(back.failure_threshold, cfg.failure_threshold);
    }

    // ---- ComponentDescriptor ----

    fn sample_quota() -> os_core::ResourceQuota {
        os_core::ResourceQuota {
            cpu_cores: Some(2.0),
            memory_bytes: Some(1024),
            io_bps_limit: None,
        }
    }

    #[test]
    fn component_descriptor_serde_roundtrip_full() {
        let desc = ComponentDescriptor {
            id: ComponentId::new("svc"),
            dependencies: vec![ComponentId::new("dep")],
            quota: sample_quota(),
            health_probe: HealthProbeConfig {
                kind: "exec".into(),
                target: "/bin/true".into(),
                interval_secs: 10,
                timeout_secs: 1,
                failure_threshold: 3,
            },
            command: Some("/bin/true".into()),
            enabled: true,
        };
        let json = serde_json::to_string(&desc).expect("序列化");
        let back: ComponentDescriptor = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.id, desc.id);
        assert_eq!(back.dependencies, desc.dependencies);
        assert_eq!(back.quota.cpu_cores, desc.quota.cpu_cores);
        assert_eq!(back.command, desc.command);
        assert!(back.enabled);
    }

    #[test]
    fn component_descriptor_command_defaults_to_none_when_absent() {
        // serde(default)：command 字段缺失时反序列化为 None
        let json = r#"{
            "id": "svc",
            "dependencies": [],
            "quota": {"cpu_cores": null, "memory_bytes": null, "io_bps_limit": null},
            "health_probe": {
                "kind": "exec",
                "target": "/bin/true",
                "interval_secs": 10,
                "timeout_secs": 1,
                "failure_threshold": 3
            }
        }"#;
        let desc: ComponentDescriptor = serde_json::from_str(json).expect("反序列化");
        assert_eq!(desc.command, None);
    }

    #[test]
    fn component_descriptor_enabled_defaults_to_true_when_absent() {
        // serde(default = "default_enabled")：enabled 缺失时反序列化为 true
        let json = r#"{
            "id": "svc",
            "dependencies": [],
            "quota": {"cpu_cores": null, "memory_bytes": null, "io_bps_limit": null},
            "health_probe": {
                "kind": "exec",
                "target": "/bin/true",
                "interval_secs": 10,
                "timeout_secs": 1,
                "failure_threshold": 3
            }
        }"#;
        let desc: ComponentDescriptor = serde_json::from_str(json).expect("反序列化");
        assert!(desc.enabled, "enabled 缺省应为 true");
    }

    #[test]
    fn component_descriptor_enabled_false_when_explicitly_set() {
        let json = r#"{
            "id": "svc",
            "dependencies": [],
            "quota": {"cpu_cores": null, "memory_bytes": null, "io_bps_limit": null},
            "health_probe": {
                "kind": "exec",
                "target": "/bin/true",
                "interval_secs": 10,
                "timeout_secs": 1,
                "failure_threshold": 3
            },
            "enabled": false
        }"#;
        let desc: ComponentDescriptor = serde_json::from_str(json).expect("反序列化");
        assert!(!desc.enabled);
    }

    #[test]
    fn default_enabled_helper_returns_true() {
        assert!(default_enabled());
    }
}
