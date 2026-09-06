//! CryptoManager trait —— 数据集加密管理契约（ZFS native encryption）
//!
//! ZFS 原生加密在数据集层（不依赖 LUKS/堆叠文件系统），密钥可独立加载/卸载。
//! os-storage 提供统一密钥管理入口，避免上层各处直接调用 `zfs load-key`。

use os_core::DatasetId;

/// 加密管理 trait（异步）
///
/// 实现者：默认实现封装 `zfs create -o encryption=...` / `zfs load-key` / `zfs change-key`。
/// 密钥传输：passphrase 以 `&str` 传入，实现应避免日志记录（敏感）。
pub trait CryptoManager: Send + Sync {
    /// 对已有数据集启用加密（in-place 加密，需数据集空闲）
    ///
    /// 失败：数据集已加密 / 含活跃快照 / 密钥格式非法，见 [`crate::StorageError::CryptoError`]
    async fn encrypt_dataset(
        &self,
        dataset: &DatasetId,
        passphrase: &str,
    ) -> crate::StorageResult<()>;

    /// 加载密钥（解锁加密数据集，使其可挂载/读写）
    async fn load_key(&self, dataset: &DatasetId, passphrase: &str) -> crate::StorageResult<()>;

    /// 卸载密钥（锁定数据集，不可访问；前提：数据集已卸载）
    async fn unload_key(&self, dataset: &DatasetId) -> crate::StorageResult<()>;

    /// 更改密钥（轮换；需先 load_key）
    async fn change_key(
        &self,
        dataset: &DatasetId,
        new_passphrase: &str,
    ) -> crate::StorageResult<()>;
}
