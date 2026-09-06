//! 容器（youki，OCI runtime）
//!
//! 实现说明（规划文档 §3.4）：
//! - `pull_image` 走 oci-distribution（从 registry 拉取镜像，返回 digest）
//! - 容器卷可通过 `VolumeId`（zvol/块设备）或 `PathBuf`（绑定挂载）挂载

use std::collections::HashMap;
use std::path::PathBuf;

use os_core::{ContainerId, Deserialize, Serialize, VolumeId};
use os_network::Protocol;

use crate::error::ComputeError;
use crate::ComputeResult;

// ----------------------------------------------------------------------------
// 容器状态 / 规格
// ----------------------------------------------------------------------------

/// 容器运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerState {
    /// 已创建未启动
    Created,
    /// 运行中
    Running,
    /// 已停止
    Stopped,
    /// 已暂停（cgroup freezer）
    Paused,
}

/// 容器挂载（源 + 目标 + 读写标志）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerMount {
    /// 挂载源（绑定路径或块卷）
    pub source: MountSource,
    /// 挂载目标路径（容器内）
    pub target: PathBuf,
    /// 是否只读
    pub read_only: bool,
}

/// 挂载源（绑定路径或块卷）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MountSource {
    /// 绑定挂载（host 路径）
    Bind {
        /// 宿主机源路径
        path: PathBuf,
    },
    /// 块卷挂载（zvol 等）
    Volume {
        /// 卷 ID
        volume_id: VolumeId,
    },
}

/// 端口映射（host:port -> container:port）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PortMapping {
    /// 宿主机端口
    pub host_port: u16,
    /// 容器端口
    pub container_port: u16,
    /// 协议（复用 os_network::Protocol）
    pub protocol: Protocol,
}

/// 容器规格（创建时声明）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSpec {
    /// 镜像（如 `nginx:1.25`）
    pub image: String,
    /// 启动命令（覆盖镜像 ENTRYPOINT/CMD）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// 环境变量
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// 挂载列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<ContainerMount>,
    /// 端口映射列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMapping>,
    /// 接入的容器网络名（None = default）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

/// 容器实例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    /// 容器 ID
    pub id: ContainerId,
    /// 名称
    pub name: String,
    /// 规格
    pub spec: ContainerSpec,
    /// 运行状态
    pub state: ContainerState,
    /// 镜像 digest（sha256:...，启动后回填）
    pub image_digest: String,
    /// 创建时间
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ----------------------------------------------------------------------------
// 镜像信息
// ----------------------------------------------------------------------------

/// 本地镜像信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    /// 镜像 digest（sha256:...）
    pub digest: String,
    /// 镜像名（如 `nginx:1.25`）
    pub name: String,
    /// 占用大小（字节）
    pub size: u64,
    /// 拉取时间
    pub pulled_at: chrono::DateTime<chrono::Utc>,
}

// ----------------------------------------------------------------------------
// ContainerRuntime trait（async，youki）
// ----------------------------------------------------------------------------

/// 容器运行时——基于 youki（OCI runtime）。
#[allow(async_fn_in_trait)]
pub trait ContainerRuntime: Send + Sync {
    /// 创建容器（不启动）。
    async fn create_container(
        &self,
        id: &ContainerId,
        name: &str,
        spec: ContainerSpec,
    ) -> ComputeResult<Container>;

    /// 启动容器。
    async fn start_container(&self, id: &ContainerId) -> ComputeResult<Container>;

    /// 停止容器（force=true 发送 SIGKILL）。
    async fn stop_container(&self, id: &ContainerId, force: bool) -> ComputeResult<Container>;

    /// 删除容器（须先停止）。
    async fn remove_container(&self, id: &ContainerId) -> ComputeResult<()>;

    /// 查询单个容器。
    async fn get_container(&self, id: &ContainerId) -> ComputeResult<Container>;

    /// 列出所有容器。
    async fn list_containers(&self) -> ComputeResult<Vec<Container>>;

    /// 拉取镜像（oci-distribution），返回 digest。
    async fn pull_image(&self, image: &str) -> ComputeResult<String>;

    /// 列出本地镜像。
    async fn list_images(&self) -> ComputeResult<Vec<ImageInfo>>;

    /// 删除本地镜像（按 digest）。
    async fn remove_image(&self, digest: &str) -> ComputeResult<()>;
}

// ----------------------------------------------------------------------------
// 容器生命周期状态机（纯转换校验，不依赖 youki）
// ----------------------------------------------------------------------------

/// 容器生命周期合法迁移（Created → Running ⇄ Stopped/Paused → Removed）。
///
/// 纯函数，供实现层（`YoukiRuntime`/`MockContainerRuntime`）在 mutate 前校验
/// 状态合法性，把非法迁移映射成 `ComputeError::InvalidSpec`。
///
/// 合法迁移：
/// - Created → Running / Stopped / Removed
/// - Running → Stopped / Paused
/// - Paused → Running / Stopped
/// - Stopped → Running / Removed
///
/// 不含 Removed 作为源（Removed 是终态）。
pub fn can_transition(from: ContainerState, to: ContainerState) -> bool {
    use ContainerState::*;
    matches!(
        (from, to),
        (Created, Running)
            | (Created, Stopped)
            | (Created, Paused)
            | (Running, Stopped)
            | (Running, Paused)
            | (Paused, Running)
            | (Paused, Stopped)
            | (Stopped, Running)
            | (Stopped, Paused)
    )
}

/// 校验状态迁移合法，否则返回 `InvalidSpec`。
pub fn validate_transition(from: ContainerState, to: ContainerState) -> ComputeResult<()> {
    if can_transition(from, to) {
        Ok(())
    } else {
        Err(ComputeError::InvalidSpec(format!(
            "非法容器状态迁移: {from:?} -> {to:?}"
        )))
    }
}

// ----------------------------------------------------------------------------
// 构造器 / 校验
// ----------------------------------------------------------------------------

impl ContainerSpec {
    /// 构造空 spec（仅 image 必填，其余默认空）。
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            command: Vec::new(),
            env: HashMap::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            network: None,
        }
    }

    /// 追加启动命令参数。
    pub fn with_command(mut self, cmd: Vec<String>) -> Self {
        self.command = cmd;
        self
    }

    /// 追加单个环境变量。
    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    /// 追加挂载。
    pub fn with_mount(mut self, m: ContainerMount) -> Self {
        self.mounts.push(m);
        self
    }

    /// 追加端口映射。
    pub fn with_port(mut self, p: PortMapping) -> Self {
        self.ports.push(p);
        self
    }

    /// 指定接入的容器网络名。
    pub fn with_network(mut self, net: impl Into<String>) -> Self {
        self.network = Some(net.into());
        self
    }

    /// 校验 spec 基本合法性：
    /// - image 非空；
    /// - 端口范围合法（1..=65535）；
    /// - bind mount 源路径非空。
    pub fn validate(&self) -> ComputeResult<()> {
        if self.image.trim().is_empty() {
            return Err(ComputeError::InvalidSpec("容器镜像不能为空".into()));
        }
        for p in &self.ports {
            if p.host_port == 0 || p.container_port == 0 {
                return Err(ComputeError::InvalidSpec(format!(
                    "端口不能为 0: host={} container={}",
                    p.host_port, p.container_port
                )));
            }
        }
        for m in &self.mounts {
            if let MountSource::Bind { path } = &m.source {
                if path.as_os_str().is_empty() {
                    return Err(ComputeError::InvalidSpec(format!(
                        "bind 挂载源路径为空: target={}",
                        m.target.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

impl Container {
    /// 构造容器实例（默认状态 Created）。
    pub fn new(id: ContainerId, name: String, spec: ContainerSpec) -> Self {
        Self {
            id,
            name,
            spec,
            state: ContainerState::Created,
            image_digest: String::new(),
            created_at: chrono::Utc::now(),
        }
    }
}
