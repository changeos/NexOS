//! os-storage 错误类型 + ApiError 转换

use thiserror::Error;

/// 存储层错误
#[derive(Debug, Error)]
pub enum StorageError {
    /// 存储池不存在
    #[error("存储池不存在: {0}")]
    PoolNotFound(String),

    /// 数据集不存在
    #[error("数据集不存在: {0}")]
    DatasetNotFound(String),

    /// 快照不存在
    #[error("快照不存在: {0}")]
    SnapshotNotFound(String),

    /// 存储池已存在（创建时冲突）
    #[error("存储池已存在: {0}")]
    PoolExists(String),

    /// 数据集已存在
    #[error("数据集已存在: {0}")]
    DatasetExists(String),

    /// vdev 规格非法（成员盘数不足 / 已被其他池使用）
    #[error("非法 vdev 规格: {0}")]
    InvalidVdev(String),

    /// 复制失败（网络中断 / target 不可达 / 增量流断裂）
    #[error("复制失败: {0}")]
    ReplicationFailed(String),

    /// 块存储 export 失败（LUN 冲突 / 内核 target 模块不可用）
    #[error("块存储 export 失败: {0}")]
    ExportFailed(String),

    /// 加密操作失败（密钥错误 / 数据集已加密 / 卸载时未卸载挂载点）
    #[error("加密错误: {0}")]
    CryptoError(String),

    /// 底层命令执行失败（zpool/zfs/scst 等子进程非零退出；保留 stderr）
    #[error("命令执行失败: {0}")]
    CommandFailed(String),

    /// 底层 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// os-storage Result 别名
pub type StorageResult<T> = Result<T, StorageError>;

// —— From 转换：StorageError → os-common::ApiError ——
//
// os-api 网关把任意 crate Error 统一转 ApiError 序列化返回前端（呼应 §12.3）。
impl From<StorageError> for os_common::ApiError {
    fn from(e: StorageError) -> Self {
        use os_common::ApiErrorCode as Code;
        let (code, msg) = match &e {
            StorageError::PoolNotFound(s)
            | StorageError::DatasetNotFound(s)
            | StorageError::SnapshotNotFound(s) => (Code::NotFound, s.clone()),

            // ERROR_GUIDE §3.3 P3 保留：加密失败（密钥错误/已加密）视作"参数不被接受"，
            // 归 InvalidInput 可接受。
            StorageError::InvalidVdev(s) | StorageError::CryptoError(s) => {
                (Code::InvalidInput, s.clone())
            }

            StorageError::PoolExists(s) | StorageError::DatasetExists(s) => {
                (Code::Conflict, s.clone())
            }

            // 复制/export 失败多涉及远端对端/网络 → UpstreamUnavailable。
            StorageError::ReplicationFailed(s) | StorageError::ExportFailed(s) => {
                (Code::UpstreamUnavailable, s.clone())
            }

            // 按 ERROR_GUIDE §3.2/§3.3：本地 zpool/zfs 子进程非零退出是本地状态/配置问题，
            // 非"上游服务"；与 os-compute/os-network/os-protocols 一致归 Internal
            // （原 UpstreamUnavailable 已修正）。
            StorageError::CommandFailed(s) => (Code::Internal, s.clone()),

            StorageError::Io(io) => (Code::Internal, io.to_string()),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::StorageError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::PoolNotFound("p".into())).contains("存储池不存在"));
        assert!(format!("{}", E::DatasetNotFound("d".into())).contains("数据集不存在"));
        assert!(format!("{}", E::SnapshotNotFound("s".into())).contains("快照不存在"));
        assert!(format!("{}", E::PoolExists("p".into())).contains("存储池已存在"));
        assert!(format!("{}", E::DatasetExists("d".into())).contains("数据集已存在"));
        assert!(format!("{}", E::InvalidVdev("v".into())).contains("非法 vdev 规格"));
        assert!(format!("{}", E::ReplicationFailed("r".into())).contains("复制失败"));
        assert!(format!("{}", E::ExportFailed("e".into())).contains("块存储 export 失败"));
        assert!(format!("{}", E::CryptoError("c".into())).contains("加密错误"));
        assert!(format!("{}", E::CommandFailed("c".into())).contains("命令执行失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
    }
}
