//! 节点能力模型 + HA 资格检测算法（纯逻辑，无外部依赖）
//!
//! 本模块把"给定本机 + 远程 capabilities + 硬指标 → 是否符合 HA"沉淀为纯算法，
//! 便于单测覆盖边界值（刚好达标 / 差一项 / 版本边界）。`HaRequirement` 门槛定义在
//! `federation.rs`，这里只实现"对照单条 capability 与门槛"的判定逻辑，
//! 供 `DefaultFederationPolicy::check_eligibility` 调用。

use crate::discovery::{NodeCapabilities, PeerNode};

/// 单个 peer 相对 HA 硬指标的局部检测结果
///
/// `reasons` 非空即代表该 peer 至少一项硬指标不达标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerQualification {
    /// peer 节点 ID（便于上层定位不达标节点）
    pub node_id: os_core::NodeId,
    /// 不达标原因列表（空 = 全部达标）
    pub reasons: Vec<String>,
}

impl PeerQualification {
    /// 该 peer 是否完全达标
    pub fn is_qualified(&self) -> bool {
        self.reasons.is_empty()
    }
}

/// 版本兼容性判定
///
/// 纯字符串前缀/比较的轻量实现（不引入 semver crate）：
/// - 支持 `">=X.Y.Z"`、`"<=X.Y.Z"`、`">X.Y.Z"`、`"<X.Y.Z"`、`"=X.Y.Z"` 五种约束
/// - 支持单字符串内逗号分隔的组合约束（如 `">=1.0.0,<2.0.0"`，AND 语义）
/// - 版本号按 `.` 分段，逐段数值比较；段数不同时短的视为补 0
/// - 无法解析的约束视为不通过（保守策略，避免误放行）
///
/// 返回 `true` 表示 `version` 满足**所有** `constraints`（AND 语义）。
pub fn version_satisfies(version: &str, constraints: &[String]) -> bool {
    let Some(parsed) = parse_version(version) else {
        return false;
    };
    for raw in constraints {
        // 单个字符串内可能含逗号分隔的多个约束（如 ">=1.0.0,<2.0.0"），拆分后逐条 AND
        for c in raw.split(',') {
            match parse_constraint(c) {
                Some((op, bound)) => {
                    if !op.satisfied(&parsed, &bound) {
                        return false;
                    }
                }
                None => return false, // 无法解析的约束保守视为不通过
            }
        }
    }
    true
}

/// 把 "1.2.3" / "1.2" 解析为数值段向量（自动 trim 空白）
fn parse_version(s: &str) -> Option<Vec<u64>> {
    s.trim()
        .split('.')
        .map(|p| p.trim().parse::<u64>().ok())
        .collect()
}

/// 约束运算符
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Ge, // >=
    Le, // <=
    Gt, // >
    Lt, // <
    Eq, // =
}

impl CmpOp {
    fn satisfied(self, v: &[u64], b: &[u64]) -> bool {
        let cmp = compare_versions(v, b);
        match self {
            CmpOp::Ge => cmp >= 0,
            CmpOp::Le => cmp <= 0,
            CmpOp::Gt => cmp > 0,
            CmpOp::Lt => cmp < 0,
            CmpOp::Eq => cmp == 0,
        }
    }
}

fn parse_constraint(s: &str) -> Option<(CmpOp, Vec<u64>)> {
    let s = s.trim();
    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (CmpOp::Ge, r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (CmpOp::Le, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (CmpOp::Gt, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (CmpOp::Lt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (CmpOp::Eq, r)
    } else {
        // 无前缀的纯版本号视为 "="
        (CmpOp::Eq, s)
    };
    let bound = parse_version(rest.trim())?;
    Some((op, bound))
}

/// 逐段比较；段数不同按补 0 处理
fn compare_versions(a: &[u64], b: &[u64]) -> i32 {
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            std::cmp::Ordering::Less => return -1,
            std::cmp::Ordering::Greater => return 1,
            std::cmp::Ordering::Equal => continue,
        }
    }
    0
}

/// 检测单个 peer 是否满足全部 HA 硬指标
///
/// 硬指标（规格 §3.14）：
/// - `supports_ha` 必须为 true（节点具备共识能力）
/// - `network_gbps` ≥ `min_bandwidth_gbps`
/// - `require_zfs` 为 true 时 `has_zfs` 必须为 true
/// - `require_kvm` 为 true 时 `has_kvm` 必须为 true
/// - `version` 必须满足全部 `version_compat` 约束
///
/// CPU / 内存硬指标在本批次不在 `NodeCapabilities` 内（规格未列字段），
/// 故暂不参与判定；如后续 ADR 扩展 capabilities 字段，在此扩展即可。
pub fn qualify_peer(peer: &PeerNode, req: &crate::federation::HaRequirement) -> PeerQualification {
    let caps = &peer.capabilities;
    let mut reasons: Vec<String> = Vec::new();

    if !caps.supports_ha {
        reasons.push(format!("{}: supports_ha=false", peer.node_id));
    }
    if caps.network_gbps + f32::EPSILON < req.min_bandwidth_gbps {
        reasons.push(format!(
            "{}: 带宽 {:.1} < 最小 {:.1} Gbps",
            peer.node_id, caps.network_gbps, req.min_bandwidth_gbps
        ));
    }
    if req.require_zfs && !caps.has_zfs {
        reasons.push(format!("{}: 缺少 ZFS（HA 存储复制依赖）", peer.node_id));
    }
    if req.require_kvm && !caps.has_kvm {
        reasons.push(format!("{}: 缺少 KVM（VM 迁移依赖）", peer.node_id));
    }
    if !version_satisfies(&peer.version, &req.version_compat) {
        reasons.push(format!(
            "{}: 版本 {} 不在兼容范围 {:?}",
            peer.node_id, peer.version, req.version_compat
        ));
    }

    PeerQualification {
        node_id: peer.node_id.clone(),
        reasons,
    }
}

/// 聚合多 peer + 本机能力，得到集群整体是否满足"最少节点数 + 全员达标"
///
/// 返回不满足的原因列表（空 = 集群整体达标）。
/// 调用方把本机 `NodeCapabilities` 包成 `PeerNode` 一并传入即可。
pub fn aggregate_qualifications(quals: &[PeerQualification], min_nodes: u32) -> Vec<String> {
    let mut reasons = Vec::new();
    let qualified = quals.iter().filter(|q| q.is_qualified()).count() as u32;
    if qualified < min_nodes {
        reasons.push(format!("达标节点数 {} < 最少 {}", qualified, min_nodes));
    }
    // 列出每个不达标节点的具体原因，便于排障
    for q in quals.iter().filter(|q| !q.is_qualified()) {
        reasons.extend(q.reasons.iter().cloned());
    }
    reasons
}

/// 给本机 capabilities 做一个合理默认（全 false / 0），便于测试构造
impl NodeCapabilities {
    /// 构造一个"最小能力"实例（全部关闭、容量 0）
    pub fn minimal() -> Self {
        Self {
            supports_ha: false,
            storage_capacity_gb: 0,
            network_gbps: 0.0,
            has_zfs: false,
            has_kvm: false,
            rdma: false,
            dpu: false,
        }
    }

    /// 构造一个"全能力"实例（全部开启、大容量）
    pub fn full() -> Self {
        Self {
            supports_ha: true,
            storage_capacity_gb: 64 * 1024,
            network_gbps: 25.0,
            has_zfs: true,
            has_kvm: true,
            rdma: true,
            dpu: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::HaRequirement;

    fn peer(version: &str, caps: NodeCapabilities) -> PeerNode {
        PeerNode {
            node_id: os_core::NodeId::new("node-1"),
            endpoints: vec!["10.0.0.1:8443".into()],
            version: version.into(),
            arch: "x86_64".into(),
            capabilities: caps,
            beacon_signature: None,
        }
    }

    fn req() -> HaRequirement {
        HaRequirement {
            min_nodes: 1,
            min_bandwidth_gbps: 10.0,
            require_zfs: true,
            require_kvm: true,
            version_compat: vec![">=1.0.0,<2.0.0".into()],
        }
    }

    #[test]
    fn version_satisfies_basic() {
        // 无前缀纯版本号视为 "="（精确匹配）
        assert!(version_satisfies("1.0.0", &["1.0.0".into()]));
        assert!(!version_satisfies("1.5.0", &["1.0.0".into()]));
        assert!(version_satisfies("1.5.0", &[">=1.0.0".into()]));
        assert!(version_satisfies("1.5.0", &["<2.0.0".into()]));
        assert!(!version_satisfies("2.0.0", &["<2.0.0".into()]));
        assert!(version_satisfies("1.5.0", &["=1.5.0".into()]));
        // 组合约束（AND，单字符串内逗号分隔）
        assert!(version_satisfies("1.5.0", &[">=1.0.0,<2.0.0".into()]));
    }

    #[test]
    fn version_satisfies_segment_diff() {
        // 段数不同按补 0
        assert!(version_satisfies("1.0", &[">=1.0.0".into()]));
        assert!(version_satisfies("1.0.0.0", &["=1.0".into()]));
    }

    #[test]
    fn version_satisfies_unparseable() {
        assert!(!version_satisfies("abc", &["1.0.0".into()]));
        assert!(!version_satisfies("1.0.0", &["garbage".into()]));
    }

    #[test]
    fn qualify_full_caps_passes() {
        let q = qualify_peer(&peer("1.5.0", NodeCapabilities::full()), &req());
        assert!(q.is_qualified(), "reasons: {:?}", q.reasons);
    }

    #[test]
    fn qualify_low_bandwidth_fails() {
        let mut caps = NodeCapabilities::full();
        caps.network_gbps = 5.0;
        let q = qualify_peer(&peer("1.5.0", caps), &req());
        assert!(!q.is_qualified());
        assert!(q.reasons.iter().any(|r| r.contains("带宽")));
    }

    #[test]
    fn qualify_missing_zfs_fails() {
        let mut caps = NodeCapabilities::full();
        caps.has_zfs = false;
        let q = qualify_peer(&peer("1.5.0", caps), &req());
        assert!(!q.is_qualified());
        assert!(q.reasons.iter().any(|r| r.contains("ZFS")));
    }

    #[test]
    fn qualify_version_out_of_range_fails() {
        let q = qualify_peer(&peer("2.5.0", NodeCapabilities::full()), &req());
        assert!(!q.is_qualified());
        assert!(q.reasons.iter().any(|r| r.contains("版本")));
    }

    #[test]
    fn qualify_boundary_exact() {
        // 刚好达标的带宽
        let mut caps = NodeCapabilities::full();
        caps.network_gbps = 10.0;
        let q = qualify_peer(&peer("1.0.0", caps), &req());
        assert!(q.is_qualified(), "boundary should pass: {:?}", q.reasons);
    }

    #[test]
    fn aggregate_under_min_nodes() {
        let q = PeerQualification {
            node_id: os_core::NodeId::new("n"),
            reasons: vec![],
        };
        let reasons = aggregate_qualifications(&[q], 3);
        assert!(reasons.iter().any(|r| r.contains("达标节点数")));
    }
}
