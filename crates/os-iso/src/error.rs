//! os-iso 错误类型
//!
//! 设计：每 crate 自定义 `IsoError`（thiserror），并实现
//! `From<IsoError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-iso 错误
#[derive(Debug, Error)]
pub enum IsoError {
    /// ISO 构建失败（xorriso/squashfs 调用失败/组件缺失）
    #[error("ISO 构建失败: {0}")]
    BuildFailed(String),

    /// 校验失败（sha256 不匹配/签名无效）
    #[error("校验失败: {0}")]
    VerificationFailed(String),

    /// 安装失败（分区/建池/装系统出错）
    #[error("安装失败: {0}")]
    InstallFailed(String),

    /// 硬件不兼容（不满足 HCL，呼应 §10.2#17）
    #[error("硬件不兼容: {0}")]
    HardwareIncompatible(String),

    /// IO 错误（读写镜像/磁盘）
    #[error("IO 错误: {0}")]
    Io(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-iso Result 别名
pub type IsoResult<T> = Result<T, IsoError>;

// —— From 转换：IsoError → ApiError（统一对外错误码）——
impl From<IsoError> for os_common::ApiError {
    fn from(e: IsoError) -> Self {
        use os_common::ApiErrorCode as Code;
        use IsoError as E;
        let (code, msg) = match e {
            E::BuildFailed(m) => (Code::Internal, m),
            // ERROR_GUIDE §3.3 P3 保留：ISO 完整性校验失败（sha256 不匹配），
            // 用户提供的文件不合法，归 InvalidInput 可接受。
            E::VerificationFailed(m) => (Code::InvalidInput, m),
            E::InstallFailed(m) => (Code::Internal, m),
            // 按 ERROR_GUIDE §3.3：硬件不满足 HCL 是部署前置条件非法（参数/环境），
            // 非"状态冲突"，故归 InvalidInput（原 Conflict 已修正）。
            E::HardwareIncompatible(m) => (Code::InvalidInput, m),
            E::Io(m) => (Code::Internal, m),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::IsoError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::BuildFailed("b".into())).contains("ISO 构建失败"));
        assert!(format!("{}", E::VerificationFailed("v".into())).contains("校验失败"));
        assert!(format!("{}", E::InstallFailed("i".into())).contains("安装失败"));
        assert!(format!("{}", E::HardwareIncompatible("h".into())).contains("硬件不兼容"));
        assert!(format!("{}", E::Io("i".into())).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
