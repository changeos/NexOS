//! DatasetOptions —— 创建数据集时的可选参数（ZFS 属性集合）
//!
//! 这些字段直接映射到 `zfs create -o <prop>=<val>` 选项。
//! 所有字段为 Option，缺省时实现使用 ZFS 默认值。

use crate::model::{EncryptionConfig, Quota};
use os_core::{Deserialize, Serialize};

/// 压缩算法
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    Off,
    Lz4,
    Gzip,
    Zstd,
    /// 自定义（如 `gzip-9`）
    Custom(String),
}

/// atime 行为（文件访问时间是否更新）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Atime {
    On,
    Off,
    /// `relatime`（仅当 mtime 更新或超过 24h 才更新 atime）
    Relatime,
}

/// 数据集创建选项
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatasetOptions {
    /// 压缩算法（None = 继承/ZFS 默认）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,

    /// 加密配置（None = 不加密）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<EncryptionConfig>,

    /// 配额
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,

    /// atime 行为
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atime: Option<Atime>,

    /// 记录大小（字节，影响吞吐/放大；常见 128KiB / 1MiB）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recordsize: Option<u32>,

    /// 是否作为块设备导出（zvol）的容量（字节）；None = 文件系统数据集
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volsize: Option<u64>,

    /// 预留空间（reservation，含子数据集）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<u64>,

    /// dedup（去重）开关
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup: Option<bool>,

    /// 挂载点（None = 默认 `/<pool>/<dataset>`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mountpoint: Option<String>,
}
