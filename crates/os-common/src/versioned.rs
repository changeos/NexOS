//! Versioned trait —— API 版本规范（呼应规划文档 §12.3）
//!
//! 所有对外 DTO 实现 `Versioned`，序列化时带 `api_version` 字段；
//! 客户端（手机/桌面）据此做版本兼容降级。

use serde::{Deserialize, Serialize};

/// 当前 API 主版本
pub const CURRENT_API_VERSION: u16 = 1;

/// 带 API 版本的数据包装（用于对外接口的序列化信封）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedEnvelope<T> {
    pub api_version: u16,
    #[serde(flatten)]
    pub data: T,
}

impl<T> VersionedEnvelope<T> {
    pub fn new(data: T) -> Self {
        Self {
            api_version: CURRENT_API_VERSION,
            data,
        }
    }
}

/// 实现此 trait 的类型能报告自身 API 版本
pub trait Versioned {
    /// 返回此 DTO/接口的 API 版本
    fn api_version(&self) -> u16 {
        CURRENT_API_VERSION
    }
}
