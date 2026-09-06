//! 迁移包格式——`MigrationPackage` 结构 + 打包/解包骨架。
//!
//! 迁移包承载"配置/共享/用户定义"的结构化导出（数据集内容走 ZFS send/recv，不在此包）。
//! 打包前必须经 §3.19 排除清单过滤（见 [`crate::exclude`]），包内**绝不**含密钥/密码。
//!
//! 设计：
//! - 逻辑结构（[`MigrationPackage`]）：清单 + 条目列表，纯数据，可序列化为 JSON。
//! - 打包/解包是**骨架**：本 crate 不引入 tar/zstd 等新依赖（红线：不虚构依赖），
//!   只定义 `pack_to_bytes`/`unpack_from_bytes`（JSON 序列化）+ `Manifest` 校验逻辑。
//!   物理打包（tar+压缩+签名）由下游（如 os-cli / iso-agent）实现，本 crate 只提供
//!   "经排除过滤后的纯数据视图 + JSON 序列化 + 完整性自检"。
//!
//! 安全自检：[`MigrationPackage::audit`] 扫描条目路径，若发现命中排除清单的项，
//! 返回错误（防御性——理论上 pack 前已过滤，这里是最后一道防线）。

use serde::{Deserialize, Serialize};

use crate::error::ProvisionError;
use crate::exclude::ExcludeRules;

// ----------------------------------------------------------------------------
// 清单
// ----------------------------------------------------------------------------

/// 迁移包清单——描述包的元数据，置于包首部。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    /// 迁移计划 ID
    pub plan_id: String,
    /// 源节点 ID
    pub source_node: String,
    /// 目标节点 ID
    pub target_node: String,
    /// 包格式版本（当前 `1`）
    pub format_version: u32,
    /// 创建时间（UTC ISO-8601）
    pub created_at: String,
    /// 条目总数
    pub entry_count: usize,
    /// 累计未压缩字节数
    pub total_bytes: u64,
    /// 内容 SHA-256（对 entries 内容计算，打包后填充；解包时校验）
    pub content_sha256: Option<String>,
}

impl PackageManifest {
    /// 当前包格式版本。
    pub const CURRENT_VERSION: u32 = 1;
}

// ----------------------------------------------------------------------------
// 条目
// ----------------------------------------------------------------------------

/// 迁移包内单条目（一个配置文件 / 共享定义 / 用户定义等）。
///
/// 安全：`path` 是逻辑路径（如 `samba/smb.conf`、`users/alice.json`），
/// 经排除清单过滤后写入；`content` 为 UTF-8 文本（结构化配置）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageEntry {
    /// 逻辑路径（包内相对路径，POSIX 风格）
    pub path: String,
    /// 文本内容（结构化配置）
    pub content: String,
}

impl PackageEntry {
    /// 构造。
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }

    /// 字节数（UTF-8）。
    pub fn byte_len(&self) -> u64 {
        self.content.len() as u64
    }
}

// ----------------------------------------------------------------------------
// MigrationPackage
// ----------------------------------------------------------------------------

/// 迁移包（逻辑结构）。
///
/// 包含清单 + 条目列表。打包前应已经过 [`ExcludeRules`] 过滤——本结构的
/// [`MigrationPackage::audit`] 提供最后一道防御性自检。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPackage {
    /// 清单
    pub manifest: PackageManifest,
    /// 条目（已过滤敏感项）
    pub entries: Vec<PackageEntry>,
}

impl MigrationPackage {
    /// 从条目构造（自动生成清单；`content_sha256` 留空，由 pack 时回填）。
    pub fn from_entries(
        plan_id: impl Into<String>,
        source_node: impl Into<String>,
        target_node: impl Into<String>,
        created_at: impl Into<String>,
        entries: Vec<PackageEntry>,
    ) -> Self {
        let total_bytes = entries.iter().map(|e| e.byte_len()).sum();
        let manifest = PackageManifest {
            plan_id: plan_id.into(),
            source_node: source_node.into(),
            target_node: target_node.into(),
            format_version: PackageManifest::CURRENT_VERSION,
            created_at: created_at.into(),
            entry_count: entries.len(),
            total_bytes,
            content_sha256: None,
        };
        Self { manifest, entries }
    }

    /// 安全自检：扫描所有条目，确保没有命中 §3.19 排除清单的敏感项。
    ///
    /// `path_prefix` 是条目逻辑路径到系统路径的前缀映射（如 `etc` → `/etc`）。
    /// 返回 `Ok(())` 表示干净；`Err` 列出第一个命中项（中止，避免泄露全量）。
    pub fn audit(&self, rules: &ExcludeRules) -> Result<(), ProvisionError> {
        for e in &self.entries {
            // 用逻辑路径直接评估（排除规则按系统路径写，此处用前缀映射近似）
            // 简化：直接用 path 评估；调用方应保证 path 已是系统路径风格
            if let crate::exclude::FilterOutcome::Excluded { rule } = rules.evaluate(&e.path) {
                return Err(ProvisionError::MigrationFailed(format!(
                    "安全审计失败：迁移包含敏感条目 {}（类别 {:?}，原因：{}）",
                    e.path, rule.category, rule.reason
                )));
            }
        }
        Ok(())
    }

    /// 打包为字节（JSON 序列化，含清单与回填的 content_sha256）。
    ///
    /// 物理压缩/签名由下游实现；本方法保证可逆 + 完整性指纹。
    pub fn pack_to_bytes(mut self) -> Result<Vec<u8>, ProvisionError> {
        // 计算内容指纹（对 entries 序列化后的字节做简单聚合 hash）
        // 注意：不引入 sha2 依赖（红线），用 std::hash::DefaultHasher 做指纹
        // （非密码学安全，但足以检测传输损坏；密码学签名由下游补）
        let fingerprint = content_fingerprint(&self.entries);
        self.manifest.content_sha256 = Some(format!("{:016x}", fingerprint));
        serde_json::to_vec(&self)
            .map_err(|e| ProvisionError::Internal(format!("迁移包序列化失败: {}", e)))
    }

    /// 从字节解包，并校验清单与内容指纹。
    pub fn unpack_from_bytes(bytes: &[u8]) -> Result<Self, ProvisionError> {
        let pkg: MigrationPackage = serde_json::from_slice(bytes)
            .map_err(|e| ProvisionError::Internal(format!("迁移包反序列化失败: {}", e)))?;

        // 校验条目数
        if pkg.entries.len() != pkg.manifest.entry_count as usize {
            return Err(ProvisionError::MigrationFailed(format!(
                "迁移包条目数不匹配：清单 {} 实际 {}",
                pkg.manifest.entry_count,
                pkg.entries.len()
            )));
        }

        // 校验内容指纹
        if let Some(expected) = &pkg.manifest.content_sha256 {
            let actual = format!("{:016x}", content_fingerprint(&pkg.entries));
            if expected != &actual {
                return Err(ProvisionError::MigrationFailed(format!(
                    "迁移包内容指纹不匹配：期望 {} 实际 {}",
                    expected, actual
                )));
            }
        }

        Ok(pkg)
    }
}

/// 计算条目内容的指纹（非密码学，用于检测损坏）。
fn content_fingerprint(entries: &[PackageEntry]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for e in entries {
        e.path.hash(&mut h);
        e.content.hash(&mut h);
    }
    h.finish()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_entries_computes_totals() {
        let pkg = MigrationPackage::from_entries(
            "p1",
            "node-a",
            "node-b",
            "2026-01-01T00:00:00Z",
            vec![
                PackageEntry::new("samba/smb.conf", "[global]"),
                PackageEntry::new("users/alice.json", "{}"),
            ],
        );
        assert_eq!(pkg.manifest.entry_count, 2);
        assert_eq!(pkg.manifest.total_bytes, "[global]".len() as u64 + 2);
        assert_eq!(pkg.manifest.format_version, 1);
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let pkg = MigrationPackage::from_entries(
            "p1",
            "node-a",
            "node-b",
            "2026-01-01T00:00:00Z",
            vec![
                PackageEntry::new("a/b", "hello"),
                PackageEntry::new("c", "world"),
            ],
        );
        let bytes = pkg.clone().pack_to_bytes().unwrap();
        let back = MigrationPackage::unpack_from_bytes(&bytes).unwrap();
        assert_eq!(back.entries, pkg.entries);
        assert!(back.manifest.content_sha256.is_some());
    }

    #[test]
    fn unpack_detects_corruption() {
        let pkg = MigrationPackage::from_entries(
            "p1",
            "a",
            "b",
            "t",
            vec![PackageEntry::new("x", "orig")],
        );
        let mut bytes = pkg.pack_to_bytes().unwrap();
        // 篡改：修改末尾内容字节
        let last = bytes.len() - 2;
        bytes[last] = b'X';
        let res = MigrationPackage::unpack_from_bytes(&bytes);
        assert!(res.is_err(), "应检测到内容损坏");
    }

    #[test]
    fn unpack_detects_count_mismatch() {
        // 手工构造一个 entry_count 与 entries 不一致的包
        let pkg =
            MigrationPackage::from_entries("p1", "a", "b", "t", vec![PackageEntry::new("x", "y")]);
        let mut bad = pkg.clone();
        bad.manifest.entry_count = 99; // 篡改清单
                                       // 重新序列化（绕过 pack 的指纹校验，模拟"恶意/损坏的清单"）
        let bytes = serde_json::to_vec(&bad).unwrap();
        let res = MigrationPackage::unpack_from_bytes(&bytes);
        // 指纹可能也对不上，但至少应失败
        assert!(res.is_err());
    }

    #[test]
    fn audit_passes_clean_package() {
        let rules = ExcludeRules::defaults();
        let pkg = MigrationPackage::from_entries(
            "p1",
            "a",
            "b",
            "t",
            vec![PackageEntry::new("samba/smb.conf", "[global]")],
        );
        assert!(pkg.audit(&rules).is_ok());
    }

    #[test]
    fn audit_rejects_sensitive_entry() {
        let rules = ExcludeRules::defaults();
        // 逻辑路径写成系统路径风格以命中默认清单
        let pkg = MigrationPackage::from_entries(
            "p1",
            "a",
            "b",
            "t",
            vec![PackageEntry::new("/etc/shadow", "root:$6$...")],
        );
        let err = pkg.audit(&rules).unwrap_err();
        match err {
            ProvisionError::MigrationFailed(msg) => {
                assert!(msg.contains("敏感条目"));
                assert!(msg.contains("/etc/shadow"));
            }
            _ => panic!("应为 MigrationFailed"),
        }
    }

    #[test]
    fn audit_rejects_jwt_key() {
        let rules = ExcludeRules::defaults();
        let pkg = MigrationPackage::from_entries(
            "p1",
            "a",
            "b",
            "t",
            vec![PackageEntry::new("/etc/os/jwt-signing.key", "supersecret")],
        );
        assert!(pkg.audit(&rules).is_err());
    }

    // —— 覆盖率补测：pack/unpack 边界 + fingerprint 不匹配分支 ——

    #[test]
    fn unpack_detects_fingerprint_mismatch() {
        // 构造一个包，pack 后手动篡改 manifest.content_sha256（保留条目数一致）
        let pkg = MigrationPackage::from_entries(
            "p1",
            "a",
            "b",
            "t",
            vec![PackageEntry::new("x", "original")],
        );
        let mut bytes = pkg.pack_to_bytes().unwrap();
        // 找到 content_sha256 字段值并替换（篡改指纹 → 期望与实际不符）
        // 直接重新构造：解析后改指纹再序列化
        let mut parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        parsed["manifest"]["content_sha256"] = serde_json::json!("deadbeefdeadbeef");
        bytes = serde_json::to_vec(&parsed).unwrap();
        let res = MigrationPackage::unpack_from_bytes(&bytes);
        assert!(res.is_err(), "指纹不匹配应失败");
        match res.unwrap_err() {
            ProvisionError::MigrationFailed(msg) => assert!(msg.contains("指纹不匹配")),
            other => panic!("应为 MigrationFailed(指纹不匹配)，got {:?}", other),
        }
    }

    #[test]
    fn unpack_accepts_when_no_fingerprint() {
        // content_sha256 = None → 跳过指纹校验，仅校验条目数
        let pkg =
            MigrationPackage::from_entries("p1", "a", "b", "t", vec![PackageEntry::new("x", "y")]);
        let mut bytes = serde_json::to_vec(&pkg).unwrap();
        // 手动清除 content_sha256（None）
        let mut parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        parsed["manifest"]["content_sha256"] = serde_json::Value::Null;
        bytes = serde_json::to_vec(&parsed).unwrap();
        let res = MigrationPackage::unpack_from_bytes(&bytes);
        assert!(res.is_ok(), "无指纹时应通过（仅条目数校验）");
    }

    #[test]
    fn unpack_rejects_invalid_json() {
        // 非 JSON 字节 → Internal(反序列化失败)
        let res = MigrationPackage::unpack_from_bytes(b"not json at all");
        assert!(res.is_err());
        match res.unwrap_err() {
            ProvisionError::Internal(msg) => assert!(msg.contains("反序列化失败")),
            _ => panic!("应为 Internal"),
        }
    }

    #[test]
    fn pack_to_bytes_succeeds() {
        // pack 成功路径（含 content_sha256 回填）
        let pkg = MigrationPackage::from_entries(
            "p1",
            "a",
            "b",
            "t",
            vec![PackageEntry::new("x", "y"), PackageEntry::new("z", "w")],
        );
        let bytes = pkg.pack_to_bytes().unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn package_entry_byte_len() {
        let e = PackageEntry::new("path", "hello");
        assert_eq!(e.byte_len(), 5);
        let e2 = PackageEntry::new("p", "");
        assert_eq!(e2.byte_len(), 0);
    }

    #[test]
    fn manifest_current_version_constant() {
        assert_eq!(PackageManifest::CURRENT_VERSION, 1);
    }
}
