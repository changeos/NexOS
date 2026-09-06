//! os-compute 错误类型
//!
//! 设计：每 crate 自定义 `ComputeError`（thiserror），并实现
//! `From<ComputeError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-compute 错误
#[derive(Debug, Error)]
pub enum ComputeError {
    /// 虚拟机不存在
    #[error("虚拟机不存在: {0}")]
    VmNotFound(String),

    /// 容器不存在
    #[error("容器不存在: {0}")]
    ContainerNotFound(String),

    /// 镜像拉取失败（oci-distribution 拉取失败）
    #[error("镜像拉取失败: {0}")]
    ImagePullFailed(String),

    /// 迁移失败（libvirt migrate / 目标节点不可达）
    #[error("迁移失败: {0}")]
    MigrationFailed(String),

    /// 容器网络不存在
    #[error("容器网络不存在: {0}")]
    NetworkNotFound(String),

    /// 第三方包不存在
    #[error("第三方包不存在: {0}")]
    PackageNotFound(String),

    /// 包安装/升级失败（dpkg/apt 报错）
    #[error("包安装失败: {0}")]
    InstallFailed(String),

    /// 规格/参数非法（如 vcpu=0、内存超限）
    #[error("规格非法: {0}")]
    InvalidSpec(String),

    /// 硬件虚拟化不可用（CPU 不支持/BIOS 未开 VT-x/KVM 模块未加载）
    #[error("硬件虚拟化不可用: {0}")]
    HardwareVirtualizationUnavailable(String),

    /// libvirt 错误（virError 原始消息）
    #[error("libvirt 错误: {0}")]
    LibvirtError(String),

    /// 底层命令执行失败
    #[error("命令执行失败: {0}")]
    CommandFailed(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-compute Result 别名
pub type ComputeResult<T> = Result<T, ComputeError>;

// —— From 转换：ComputeError → ApiError（统一对外错误码）——
impl From<ComputeError> for os_common::ApiError {
    fn from(e: ComputeError) -> Self {
        use os_common::ApiErrorCode as Code;
        use ComputeError as E;
        let (code, msg) = match e {
            E::VmNotFound(m) => (Code::NotFound, m),
            E::ContainerNotFound(m) => (Code::NotFound, m),
            E::PackageNotFound(m) => (Code::NotFound, m),
            E::NetworkNotFound(m) => (Code::NotFound, m),
            E::ImagePullFailed(m) => (Code::UpstreamUnavailable, m),
            E::MigrationFailed(m) => (Code::FailoverFailed, m),
            E::InstallFailed(m) => (Code::Internal, m),
            E::InvalidSpec(m) => (Code::InvalidInput, m),
            E::HardwareVirtualizationUnavailable(m) => (Code::InvalidInput, m),
            E::LibvirtError(m) => (Code::UpstreamUnavailable, m),
            E::CommandFailed(m) => (Code::Internal, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::ComputeError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::VmNotFound("v".into())).contains("虚拟机不存在"));
        assert!(format!("{}", E::ContainerNotFound("c".into())).contains("容器不存在"));
        assert!(format!("{}", E::ImagePullFailed("i".into())).contains("镜像拉取失败"));
        assert!(format!("{}", E::MigrationFailed("m".into())).contains("迁移失败"));
        assert!(format!("{}", E::NetworkNotFound("n".into())).contains("容器网络不存在"));
        assert!(format!("{}", E::PackageNotFound("p".into())).contains("第三方包不存在"));
        assert!(format!("{}", E::InstallFailed("i".into())).contains("包安装失败"));
        assert!(format!("{}", E::InvalidSpec("s".into())).contains("规格非法"));
        assert!(
            format!("{}", E::HardwareVirtualizationUnavailable("h".into()))
                .contains("硬件虚拟化不可用")
        );
        assert!(format!("{}", E::LibvirtError("l".into())).contains("libvirt 错误"));
        assert!(format!("{}", E::CommandFailed("c".into())).contains("命令执行失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
