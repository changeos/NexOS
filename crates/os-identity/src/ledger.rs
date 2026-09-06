//! 账本实现——数据模型 + [`IdentityLedger`] + 持久化。模块文档见 crate 顶层。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 单身份地址集上限（verified / unverified 各自去重，最新在前）。
pub const LEDGER_ADDRS_CAP: usize = 8;
/// 单身份指纹失配事件上限（超出丢最旧——观测面不是审计日志）。
pub const LEDGER_MISMATCH_CAP: usize = 64;
/// 单身份冲突观测条数上限（按观测地址分条，超出丢最旧）。
pub const LEDGER_CONFLICTS_CAP: usize = 32;
/// 账本身份条目全局上限（超出按 last_seen 淘汰最旧——账本是实证面不是
/// 无限档案：gossip 谎报可制造海量垃圾身份，观测价值随 staleness 衰减）。
pub const LEDGER_RECORDS_CAP: usize = 4096;
/// 持久化防抖间隔（脏标记起效后至少隔这么久才落盘；停机时无视间隔强制刷）。
pub const FLUSH_DEBOUNCE: Duration = Duration::from_secs(10);

// ============================================================================
// 数据模型
// ============================================================================

/// 指纹证据种类（谁看见的、验证过没有——决定地址在身份名下的记账动作）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceKind {
    /// 本机直连握手（最强证据——挑战-签名天然验证地址背后确为该 NodeID）。
    Handshake,
    /// 指纹验证探测成功（TCP connect + 复用握手路径比对 NodeID，见 os-p2p
    /// meta 组件的 fingerprint_probe）。
    ProbeVerified,
    /// 探测发现指纹不匹配：地址背后是**别的节点**（gossip 谎报/陈旧观测被
    /// 实锤）。`actual` = 握手实际返回的 NodeID——探测完成了真实握手，地址
    /// 同时升到 actual 名下 verified（地址换人被实证）。
    ProbeMismatch {
        /// 握手实际验证到的 NodeID（`0x`+66 hex）。
        actual: String,
    },
    /// 他节点转述（gossip）。`verified` = 报告方视角该地址是否通过指纹验证
    /// （透传标记——未验证地址只入 unverified，不污染本机已验证结论）。
    Gossip {
        /// 报告方的验证位。
        verified: bool,
    },
}

/// 单身份账本条目（观察面/持久化的单元）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityRecord {
    /// 节点身份（即指纹——secp256k1 压缩公钥 `0x`+66 hex；本 crate 视为不透明串）。
    pub node_id: String,
    /// 已验证地址集（握手/指纹探测实证——最新在前，上限 [`LEDGER_ADDRS_CAP`]）。
    pub verified_addrs: Vec<SocketAddr>,
    /// 未验证地址集（gossip 转述 / 失配降级——最新在前，上限同上）。
    pub unverified_addrs: Vec<SocketAddr>,
    /// 首次见到（unix 秒）。
    pub first_seen: u64,
    /// 最近一次证据（unix 秒）。
    pub last_seen: u64,
    /// 同 NodeID 多地址观测（原 os-p2p identity_conflicts 语义迁入——仅提示
    /// 不阻断：身份=密钥是设计特性，多 OS 共用同一私钥时权限共享，本观测面
    /// 只让本机用户知情）。按观测地址分条。
    pub conflict_entries: Vec<ConflictEntry>,
    /// 指纹失配事件（探测实锤地址背后是别的节点——gossip 谎报/陈旧观测的
    /// 取证面）。最新在前，上限 [`LEDGER_MISMATCH_CAP`]。
    pub mismatch_events: Vec<MismatchEvent>,
}

/// 冲突观测条目（同 NodeID 从某地址重复进入的记账——按观测地址分条累计）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictEntry {
    /// 观测地址（socket `ip:port`——同公钥从不同地址进入的判据；含回环：
    /// 同机多实例恰恰经回环进入，观测面照记）。
    pub addr: SocketAddr,
    /// 首次发现（unix 秒）。
    pub first_seen: u64,
    /// 最近发现（unix 秒）。
    pub last_seen: u64,
    /// 累计警告次数（同地址重复连接每次 +1）。
    pub warning_count: u64,
}

/// 指纹失配事件（地址 X 被探测时期望是 A、握手实际是 B）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MismatchEvent {
    /// 被探测地址。
    pub addr: SocketAddr,
    /// 握手实际验证到的 NodeID（地址真正的主人）。
    pub actual: String,
    /// 事件时刻（unix 秒）。
    pub at: u64,
}

/// 指纹失配事件的全账本查询视图（含归属身份——REST `records` 内嵌单身份事件，
/// 此结构供跨身份时间线消费）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MismatchReport {
    /// 期望身份（被谎报/陈旧观测的一方）。
    pub node_id: String,
    /// 被探测地址。
    pub addr: SocketAddr,
    /// 握手实际验证到的 NodeID。
    pub actual: String,
    /// 事件时刻（unix 秒）。
    pub at: u64,
}

/// 冲突观测查询条目（**与原 os-p2p `IdentityConflict` 字段同形**——REST 端点
/// `GET /api/v1/identity/conflicts` 与 `GET /api/v1/p2p/identity-conflicts`
/// 输出形状兼容，前端无感）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IdentityConflict {
    /// 冲突 NodeID（`0x`+66 hex 全量）。
    pub node_id: String,
    /// 对端观测地址（socket `ip:port`——同公钥从不同地址进入的判据）。
    pub remote_addr: String,
    /// 首次发现（unix 秒）。
    pub first_seen: u64,
    /// 最近发现（unix 秒）。
    pub last_seen: u64,
    /// 累计警告次数（同地址重复连接每次 +1）。
    pub warning_count: u64,
}

/// 地址归属判定（[`IdentityLedger::owns_addr`] 的输出——对比库的核心结论）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddrOwnership {
    /// 地址在目标身份的 verified 集（本机实证过——信任）。
    Verified,
    /// 地址在目标身份的 unverified 集（仅 gossip 转述/已降级——不作凭据）。
    Unverified,
    /// 地址在**其他**身份的 verified 集（冲突——地址换人已被实证）。
    Foreign {
        /// 实证持有该地址的身份。
        owner: String,
    },
    /// 账本无此地址的任何记录。
    Unknown,
}

// ============================================================================
// 账本
// ============================================================================

/// 身份账本（纯内存结构 + 持久化防抖记账；无 I/O 副作用的读写均不落盘）。
///
/// 并发约定：宿主以 `Arc<Mutex<IdentityLedger>>` 共享（短临界区、持锁不 await），
/// 文件 I/O 由宿主在锁外执行（[`Self::flush_due`]/[`Self::flush_final`] 只产出
/// JSON 串，[`write_atomic`] 负责原子写）。
pub struct IdentityLedger {
    /// 持久化文件（None = 纯内存——测试用）。
    path: Option<PathBuf>,
    records: HashMap<String, IdentityRecord>,
    /// 脏标记（有变更待落盘）。
    dirty: bool,
    /// 最近一次落盘时刻（防抖计时）。
    last_flush: Instant,
}

impl IdentityLedger {
    /// 空账本；`path` 为 Some 时尝试加载（失败告警并重建空账本）。
    ///
    /// 加载时**无条件剔除地址集中的回环存量**（2026-08-25 定调）：verified/
    /// unverified 集里的 127.0.0.0/8 / ::1 逐条剥离（回环对全网没有凭据价值）；
    /// conflict_entries 不剔（观测面，含同机多实例的回环进入）。
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut ledger = Self {
            path,
            records: HashMap::new(),
            dirty: false,
            last_flush: Instant::now(),
        };
        ledger.load_from_disk();
        ledger
    }

    /// 持久化文件路径（宿主落盘用）。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 强制置脏（落盘失败后重试用）。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn load_from_disk(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Vec<IdentityRecord>>(&content) {
                Ok(list) => {
                    let mut stripped = 0usize;
                    let list: Vec<IdentityRecord> = list
                        .into_iter()
                        .map(|mut r| {
                            let before = r.verified_addrs.len() + r.unverified_addrs.len();
                            r.verified_addrs.retain(|a| !is_loopback(*a));
                            r.unverified_addrs.retain(|a| !is_loopback(*a));
                            stripped += before - r.verified_addrs.len() - r.unverified_addrs.len();
                            r
                        })
                        .collect();
                    if stripped > 0 {
                        tracing::info!(
                            ledger_file = %path.display(),
                            stripped,
                            "加载身份账本：剔除地址集回环存量（回环无凭据价值；下次落盘覆盖）"
                        );
                        self.dirty = true;
                    }
                    let count = list.len();
                    self.records = list.into_iter().map(|r| (r.node_id.clone(), r)).collect();
                    tracing::info!(
                        ledger_file = %path.display(),
                        count,
                        "加载身份账本（重启不丢）"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        ledger_file = %path.display(),
                        error = %e,
                        "身份账本文件损坏——重建空账本（旧文件将在下次落盘时覆盖）"
                    );
                    self.records.clear();
                    self.dirty = true;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    ledger_file = %path.display(),
                    error = %e,
                    "身份账本文件不可读——以空账本启动"
                );
            }
        }
    }

    // —— 写入面 ——

    /// 登记一条指纹证据（os-p2p 传输层发来的事实事件——**证据**唯一写入口；
    /// 同 NodeID 冲突观测另有 [`Self::record_conflict`]，两口径不混）。
    ///
    /// - `Handshake` / `ProbeVerified`：地址升 verified + last_seen 续期 +
    ///   **从其他身份的地址集移除**（同一地址同一时刻只属于一个身份——地址
    ///   换人被实证，旧持有者的历史保留在 mismatch/观测里）；
    /// - `ProbeMismatch { actual }`：目标身份该地址 verified → unverified +
    ///   记 [`MismatchEvent`]；同时地址升到 `actual` 名下 verified（探测完成了
    ///   真实握手）；
    /// - `Gossip { verified }`：未验证转述只入 unverified；报告方验证位透传，
    ///   不覆盖本机已验证结论（已 verified 保持）。
    ///
    /// **回环拒绝**（2026-08-25 定调）：任何证据的回环地址不入地址集（不新建、
    /// 不升级、不降级——直接丢弃；同机多实例的观测由 [`Self::record_conflict`]
    /// 的观测面承担，地址集不需要回环条目）。回环的 ProbeMismatch 仍记失配
    /// 事件（谎报回环地址也是谎报），只是不动 actual 的地址集。
    pub fn record_evidence(
        &mut self,
        node_id: &str,
        addr: SocketAddr,
        kind: EvidenceKind,
        now: u64,
    ) {
        match kind {
            EvidenceKind::Handshake | EvidenceKind::ProbeVerified => {
                self.promote_verified(node_id, addr, now);
            }
            EvidenceKind::Gossip { verified } => {
                if is_loopback(addr) {
                    return;
                }
                // 写放大修复（2026-08-25 审查）：稳态 gossip 每 10s 重述同样
                // 的地址——仅集合**实际变更**（新增地址 / 转述升级 / 迁出
                // unverified）才置脏，无变化的重复观测不再触发全量重写。
                // last_seen 仍在内存续期（下次真实变更一并落盘）。
                let mut changed = false;
                {
                    let rec = self.entry_or_new(node_id, now);
                    rec.last_seen = rec.last_seen.max(now);
                    if verified {
                        // 报告方验证过的地址透传升级（重复观测去重置前，不降级本机结论）
                        changed |=
                            push_front_capped(&mut rec.verified_addrs, addr, LEDGER_ADDRS_CAP);
                        let before = rec.unverified_addrs.len();
                        rec.unverified_addrs.retain(|a| *a != addr);
                        changed |= rec.unverified_addrs.len() != before;
                    } else if !rec.verified_addrs.contains(&addr)
                        && !rec.unverified_addrs.contains(&addr)
                    {
                        changed |=
                            push_front_capped(&mut rec.unverified_addrs, addr, LEDGER_ADDRS_CAP);
                    }
                }
                if changed {
                    self.dirty = true;
                }
            }
            EvidenceKind::ProbeMismatch { actual } => {
                // 失配取证：期望身份记事件（观测面——即使尚无条目也留痕）；该地址
                // 若在其 verified 集则降级
                {
                    let rec = self.entry_or_new(node_id, now);
                    rec.mismatch_events.insert(
                        0,
                        MismatchEvent {
                            addr,
                            actual: actual.clone(),
                            at: now,
                        },
                    );
                    rec.mismatch_events.truncate(LEDGER_MISMATCH_CAP);
                    if !is_loopback(addr) && rec.verified_addrs.contains(&addr) {
                        rec.verified_addrs.retain(|a| *a != addr);
                        push_front_capped(&mut rec.unverified_addrs, addr, LEDGER_ADDRS_CAP);
                    }
                    rec.last_seen = rec.last_seen.max(now);
                }
                self.dirty = true;
                // 地址换人实证：探测完成了真实握手——地址升到 actual 名下
                //（回环除外：actual 的地址集同样不收回环；随后 promote 会把地址
                // 从期望身份的地址集整体移除——失配历史保留在 mismatch_events）
                self.promote_verified(&actual, addr, now);
            }
        }
    }

    /// 地址升级为某身份的 verified（握手/探测成功路径；同时从其他身份移除）。
    fn promote_verified(&mut self, node_id: &str, addr: SocketAddr, now: u64) {
        if is_loopback(addr) {
            return;
        }
        // 地址换人：同一地址同一时刻只属于一个身份
        for (other_id, rec) in self.records.iter_mut() {
            if other_id != node_id {
                rec.verified_addrs.retain(|a| *a != addr);
                rec.unverified_addrs.retain(|a| *a != addr);
            }
        }
        let rec = self.entry_or_new(node_id, now);
        rec.unverified_addrs.retain(|a| *a != addr);
        // 重复观测去重置前（最新在前——与 os-p2p meta 地址历史同款）
        push_front_capped(&mut rec.verified_addrs, addr, LEDGER_ADDRS_CAP);
        rec.last_seen = rec.last_seen.max(now);
        self.dirty = true;
    }

    /// 记一条同 NodeID 多地址观测（原 os-p2p `register_conn` 的
    /// identity_conflicts 记账迁入——对端握手自报 NodeID == 本机 NodeID 时由
    /// 传输层调用）。按观测地址分条累计，返回该地址累计警告次数（宿主日志用）。
    ///
    /// **仅提示不阻断**：身份=密钥是设计特性，多 OS 共用同一私钥时权限共享
    /// ——本观测面只让本机用户知情。**回环照记**：同机多实例恰恰经回环进入，
    /// remote_addr 是 socket 观测地址（知情面）而非可拨凭据。
    pub fn record_conflict(&mut self, node_id: &str, addr: SocketAddr, now: u64) -> u64 {
        let rec = self.entry_or_new(node_id, now);
        rec.last_seen = rec.last_seen.max(now);
        let count = match rec.conflict_entries.iter_mut().find(|c| c.addr == addr) {
            Some(entry) => {
                entry.last_seen = now;
                entry.warning_count += 1;
                entry.warning_count
            }
            None => {
                rec.conflict_entries.insert(
                    0,
                    ConflictEntry {
                        addr,
                        first_seen: now,
                        last_seen: now,
                        warning_count: 1,
                    },
                );
                // 超限丢最旧（观测面非审计日志）
                rec.conflict_entries.truncate(LEDGER_CONFLICTS_CAP);
                1
            }
        };
        self.dirty = true;
        count
    }

    // —— 查询面 ——

    /// 按键取单条记录（REST addr 归属查询用——全量快照再线性找是 O(N) 克隆
    /// 写放大，键控账本直接拿）。
    #[must_use]
    pub fn get_record(&self, node_id: &str) -> Option<IdentityRecord> {
        self.records.get(node_id).cloned()
    }

    /// 地址归属判定（对比库核心）：地址在目标身份 verified 集 = [`AddrOwnership::Verified`]
    /// （信任）；在目标 unverified 集 = [`AddrOwnership::Unverified`]；在**其他**
    /// 身份 verified 集 = [`AddrOwnership::Foreign`]（冲突——地址换人已被实证）；
    /// 无记录 = [`AddrOwnership::Unknown`]。
    #[must_use]
    pub fn owns_addr(&self, addr: SocketAddr, node_id: &str) -> AddrOwnership {
        if let Some(rec) = self.records.get(node_id) {
            if rec.verified_addrs.contains(&addr) {
                return AddrOwnership::Verified;
            }
            if rec.unverified_addrs.contains(&addr) {
                return AddrOwnership::Unverified;
            }
        }
        for (other, rec) in &self.records {
            if other != node_id && rec.verified_addrs.contains(&addr) {
                return AddrOwnership::Foreign {
                    owner: other.clone(),
                };
            }
        }
        AddrOwnership::Unknown
    }

    /// 查地址的当前主人（verified 优先于 unverified）：REST 地址归属查询用。
    /// 返回 `(owner_node_id, verified)`；无任何记录 → None。
    #[must_use]
    pub fn owner_of(&self, addr: SocketAddr) -> Option<(String, bool)> {
        let mut unverified_owner: Option<String> = None;
        for (id, rec) in &self.records {
            if rec.verified_addrs.contains(&addr) {
                return Some((id.clone(), true));
            }
            if unverified_owner.is_none() && rec.unverified_addrs.contains(&addr) {
                unverified_owner = Some(id.clone());
            }
        }
        unverified_owner.map(|id| (id, false))
    }

    /// 全量快照（观察面/REST records 端点）：按 last_seen 降序（最近活跃在前）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<IdentityRecord> {
        let mut list: Vec<IdentityRecord> = self.records.values().cloned().collect();
        list.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.node_id.cmp(&b.node_id))
        });
        list
    }

    /// 冲突观测快照（全账本摊平，按最近发现降序——前端警告条首条 = 最活跃
    /// 冲突源；输出形状与原 os-p2p identity_conflicts 端点一致）。
    #[must_use]
    pub fn conflicts(&self) -> Vec<IdentityConflict> {
        let mut list: Vec<IdentityConflict> = self
            .records
            .values()
            .flat_map(|rec| {
                rec.conflict_entries.iter().map(move |c| IdentityConflict {
                    node_id: rec.node_id.clone(),
                    remote_addr: c.addr.to_string(),
                    first_seen: c.first_seen,
                    last_seen: c.last_seen,
                    warning_count: c.warning_count,
                })
            })
            .collect();
        list.sort_by_key(|c| std::cmp::Reverse(c.last_seen));
        list
    }

    /// 指纹失配事件全账本时间线（跨身份摊平，按事件时刻降序）。
    #[must_use]
    pub fn mismatch_events(&self) -> Vec<MismatchReport> {
        let mut list: Vec<MismatchReport> = self
            .records
            .values()
            .flat_map(|rec| {
                rec.mismatch_events.iter().map(move |m| MismatchReport {
                    node_id: rec.node_id.clone(),
                    addr: m.addr,
                    actual: m.actual.clone(),
                    at: m.at,
                })
            })
            .collect();
        list.sort_by_key(|m| std::cmp::Reverse(m.at));
        list
    }

    /// 条目数（日志/诊断用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 账本是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    // —— 持久化 ——

    /// 防抖到期且有脏数据 → 产出待写 JSON（并清脏/重置计时）。锁内调用，
    /// 仅序列化不做 I/O。
    pub fn flush_due(&mut self, now: Instant) -> Option<String> {
        if !self.dirty || now.duration_since(self.last_flush) < FLUSH_DEBOUNCE {
            return None;
        }
        self.take_snapshot_json()
    }

    /// 停机强制刷盘：脏即产出（无视防抖间隔）。锁内调用，仅序列化不做 I/O。
    pub fn flush_final(&mut self) -> Option<String> {
        if !self.dirty {
            return None;
        }
        self.take_snapshot_json()
    }

    fn take_snapshot_json(&mut self) -> Option<String> {
        let json = serde_json::to_string(&self.snapshot()).ok()?;
        self.dirty = false;
        self.last_flush = Instant::now();
        Some(json)
    }

    /// 取条目（无则建档：first_seen=last_seen=now；建档前按全局上限腾位）。
    fn entry_or_new(&mut self, node_id: &str, now: u64) -> &mut IdentityRecord {
        self.evict_for_capacity(node_id);
        self.records.entry(node_id.to_string()).or_insert_with(|| {
            self.dirty = true;
            IdentityRecord {
                node_id: node_id.to_string(),
                verified_addrs: Vec::new(),
                unverified_addrs: Vec::new(),
                first_seen: now,
                last_seen: now,
                conflict_entries: Vec::new(),
                mismatch_events: Vec::new(),
            }
        })
    }

    /// 全局上限淘汰（插入路径检查，2026-08-25 审查补 A5）：条目数达到
    /// [`LEDGER_RECORDS_CAP`] 且即将插入**新键**时，按 last_seen 淘汰最旧
    /// （并列按 node_id 稳定取舍；`keep` 自身永不淘汰——续写既有身份不触发
    /// 淘汰，新键也让位于它自己之外的更旧者）。淘汰即置脏。加载了超限的
    /// 历史文件同样在此自愈收敛到上限。
    fn evict_for_capacity(&mut self, keep: &str) {
        while self.records.len() >= LEDGER_RECORDS_CAP && !self.records.contains_key(keep) {
            let victim = self
                .records
                .iter()
                .min_by(|a, b| {
                    a.1.last_seen
                        .cmp(&b.1.last_seen)
                        .then_with(|| a.1.node_id.cmp(&b.1.node_id))
                })
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else { break };
            tracing::debug!(
                node_id = %victim,
                cap = LEDGER_RECORDS_CAP,
                "身份账本达全局上限——按 last_seen 淘汰最旧条目"
            );
            self.records.remove(&victim);
            self.dirty = true;
        }
    }
}

/// 地址是否回环（127.0.0.0/8 / ::1）。
fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// 集合写入：去重置前 + 上限截断（最旧挤出）。返回集合是否实际变更
/// （已在首位且未触发截断 = 无变化——调用方据此决定是否置脏）。
fn push_front_capped(list: &mut Vec<SocketAddr>, addr: SocketAddr, cap: usize) -> bool {
    if list.first() == Some(&addr) && list.len() <= cap {
        return false;
    }
    list.retain(|a| *a != addr);
    list.insert(0, addr);
    list.truncate(cap);
    true
}

/// 原子写账本 JSON：同目录临时文件（`<名>.tmp.<pid>`）→ fsync → rename
/// （与 os-p2p meta 注册表/私钥文件同款写法——中途崩溃不留半截文件）。父目录
/// 不存在先创建。
pub fn write_atomic(path: &Path, json: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ============================================================================
// 单元测——证据登记/升降级/地址换人/owns_addr/失配/冲突/回环/持久化
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn nid(seed: u8) -> String {
        // 测试身份串：格式与 NodeID（0x+66 hex）同形即可——本 crate 视其为不透明串
        format!("0x{seed:02x}{}", "a".repeat(64))
    }

    fn addr(port: u16) -> SocketAddr {
        format!("203.0.113.{port}:41000").parse().unwrap()
    }

    /// 回环观测地址。
    fn lo(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn record_of(ledger: &IdentityLedger, node_id: &str) -> IdentityRecord {
        ledger
            .snapshot()
            .into_iter()
            .find(|r| r.node_id == node_id)
            .unwrap_or_else(|| panic!("账本应有 {node_id} 条目"))
    }

    // 1. Handshake 建档：verified 入集、first/last_seen、重复观测去重置前
    #[test]
    fn handshake_creates_record_with_verified_addr() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(1);
        ledger.record_evidence(&a, addr(1), EvidenceKind::Handshake, 1000);
        let rec = record_of(&ledger, &a);
        assert_eq!(rec.verified_addrs, vec![addr(1)]);
        assert!(rec.unverified_addrs.is_empty());
        assert_eq!((rec.first_seen, rec.last_seen), (1000, 1000));
        // 第二个地址最新在前；重复观测去重置前
        ledger.record_evidence(&a, addr(2), EvidenceKind::Handshake, 1010);
        ledger.record_evidence(&a, addr(1), EvidenceKind::Handshake, 1020);
        let rec = record_of(&ledger, &a);
        assert_eq!(rec.verified_addrs, vec![addr(1), addr(2)], "去重置前");
        assert_eq!(rec.first_seen, 1000, "建档时刻不漂移");
        assert_eq!(rec.last_seen, 1020);
    }

    // 2. Gossip 分级：unverified 转述只入 unverified；报告方 verified 位透传
    //    升级；两者都不覆盖本机已验证结论
    #[test]
    fn gossip_records_unverified_and_transparent_verified() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(2);
        // unverified 转述
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: false }, 100);
        let rec = record_of(&ledger, &a);
        assert_eq!(
            rec.unverified_addrs,
            vec![addr(1)],
            "转述地址只入 unverified"
        );
        assert!(rec.verified_addrs.is_empty());
        // 报告方验证过的地址透传升级
        ledger.record_evidence(&a, addr(2), EvidenceKind::Gossip { verified: true }, 110);
        let rec = record_of(&ledger, &a);
        assert!(
            rec.verified_addrs.contains(&addr(2)),
            "透传 verified=true 升级"
        );
        // 本机已验证地址不被 unverified 转述降级
        ledger.record_evidence(&a, addr(1), EvidenceKind::ProbeVerified, 120);
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: false }, 130);
        let rec = record_of(&ledger, &a);
        assert!(
            rec.verified_addrs.contains(&addr(1)),
            "gossip 不降级本机结论"
        );
        assert!(!rec.unverified_addrs.contains(&addr(1)));
    }

    // 3. owns_addr 四态判定：Verified / Unverified / Foreign / Unknown
    #[test]
    fn owns_addr_returns_four_states() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(3);
        let b = nid(4);
        ledger.record_evidence(&a, addr(1), EvidenceKind::Handshake, 100);
        ledger.record_evidence(&a, addr(2), EvidenceKind::Gossip { verified: false }, 100);
        ledger.record_evidence(&b, addr(3), EvidenceKind::ProbeVerified, 100);
        assert_eq!(ledger.owns_addr(addr(1), &a), AddrOwnership::Verified);
        assert_eq!(ledger.owns_addr(addr(2), &a), AddrOwnership::Unverified);
        // addr(3) 在 b 的 verified 集 → 对 a 而言是 Foreign
        assert_eq!(
            ledger.owns_addr(addr(3), &a),
            AddrOwnership::Foreign { owner: b.clone() }
        );
        assert_eq!(ledger.owns_addr(addr(9), &a), AddrOwnership::Unknown);
        // owner_of：verified 优先
        assert_eq!(ledger.owner_of(addr(3)), Some((b.clone(), true)));
        assert_eq!(ledger.owner_of(addr(2)), Some((a.clone(), false)));
        assert_eq!(ledger.owner_of(addr(9)), None);
    }

    // 4. ProbeMismatch：期望身份记事件 + 降级；地址升到 actual 名下 verified
    //    （地址换人实证）；mismatch_events 时间线
    #[test]
    fn probe_mismatch_demotes_and_reassigns_addr() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(5);
        let b = nid(6);
        // a 名下 gossip 学到 addr(1)（未验证）+ 探测实证
        ledger.record_evidence(&a, addr(1), EvidenceKind::ProbeVerified, 100);
        // 探测 addr(1) 期望 a、实际是 b
        ledger.record_evidence(
            &a,
            addr(1),
            EvidenceKind::ProbeMismatch { actual: b.clone() },
            200,
        );
        let rec_a = record_of(&ledger, &a);
        assert_eq!(
            rec_a.mismatch_events,
            vec![MismatchEvent {
                addr: addr(1),
                actual: b.clone(),
                at: 200,
            }],
            "期望身份记失配事件（历史保留在事件流）"
        );
        assert!(
            rec_a.verified_addrs.is_empty() && rec_a.unverified_addrs.is_empty(),
            "失配地址从期望身份的地址集整体移除（已实证属于别人——owns_addr 结论才是 Foreign）"
        );
        let rec_b = record_of(&ledger, &b);
        assert!(
            rec_b.verified_addrs.contains(&addr(1)),
            "地址换人实证——actual 名下 verified"
        );
        // owns_addr 结论翻转
        assert_eq!(
            ledger.owns_addr(addr(1), &a),
            AddrOwnership::Foreign { owner: b.clone() }
        );
        assert_eq!(ledger.owns_addr(addr(1), &b), AddrOwnership::Verified);
        // 全账本时间线
        assert_eq!(
            ledger.mismatch_events(),
            vec![MismatchReport {
                node_id: a.clone(),
                addr: addr(1),
                actual: b.clone(),
                at: 200,
            }]
        );
    }

    // 5. 地址换人（Handshake 侧）：新身份握手证据把地址从旧身份集中移除
    #[test]
    fn handshake_reassigns_addr_from_previous_owner() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(7);
        let b = nid(8);
        ledger.record_evidence(&a, addr(1), EvidenceKind::Handshake, 100);
        // 同一地址后来由 b 握手进入（DHCP 换机/复用 IP）
        ledger.record_evidence(&b, addr(1), EvidenceKind::Handshake, 200);
        assert!(
            record_of(&ledger, &a).verified_addrs.is_empty(),
            "旧主人失去地址"
        );
        assert!(record_of(&ledger, &b).verified_addrs.contains(&addr(1)));
        assert_eq!(
            ledger.owns_addr(addr(1), &a),
            AddrOwnership::Foreign { owner: b.clone() }
        );
    }

    // 6. 冲突观测（原 identity_conflicts 语义迁入）：按地址分条累计
    //    warning_count、first/last_seen；conflicts() 按最近发现降序
    #[test]
    fn record_conflict_accumulates_by_addr() {
        let mut ledger = IdentityLedger::new(None);
        let self_id = nid(9);
        // 同公钥从两个地址进入（同机多实例——回环照记，观测面）
        assert_eq!(ledger.record_conflict(&self_id, lo(33516), 100), 1);
        assert_eq!(ledger.record_conflict(&self_id, lo(33516), 150), 2);
        assert_eq!(
            ledger.record_conflict(&self_id, "192.168.1.9:40000".parse().unwrap(), 160),
            1
        );
        let conflicts = ledger.conflicts();
        assert_eq!(conflicts.len(), 2, "按观测地址分条");
        // 最近发现降序（160 在前）
        assert_eq!(conflicts[0].remote_addr, "192.168.1.9:40000");
        assert_eq!(conflicts[0].warning_count, 1);
        assert_eq!(
            (conflicts[0].first_seen, conflicts[0].last_seen),
            (160, 160)
        );
        assert_eq!(conflicts[1].remote_addr, "127.0.0.1:33516");
        assert_eq!(conflicts[1].warning_count, 2, "同地址重复连接累计");
        assert_eq!(
            (conflicts[1].first_seen, conflicts[1].last_seen),
            (100, 150)
        );
        // 字段形状与原 os-p2p IdentityConflict 一致（node_id 全量 hex）
        assert_eq!(conflicts[0].node_id, self_id);
    }

    // 7. 回环拒绝（地址集侧）：任何证据的回环地址不入 verified/unverified
    #[test]
    fn loopback_never_enters_addr_sets() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(10);
        // 回环证据不建档（全部拒绝——账本无此身份）
        for kind in [
            EvidenceKind::Handshake,
            EvidenceKind::ProbeVerified,
            EvidenceKind::Gossip { verified: true },
        ] {
            ledger.record_evidence(&a, lo(33516), kind, 100);
        }
        assert!(
            ledger.snapshot().iter().all(|r| r.node_id != a),
            "纯回环证据不建档"
        );
        // 回环的失配事件仍记（谎报回环也是谎报——取证面留痕），但 actual 不收地址
        let b = nid(11);
        ledger.record_evidence(
            &a,
            lo(40000),
            EvidenceKind::ProbeMismatch { actual: b.clone() },
            120,
        );
        let rec = record_of(&ledger, &a);
        assert_eq!(rec.mismatch_events.len(), 1, "回环失配事件照记");
        assert!(rec.verified_addrs.is_empty() && rec.unverified_addrs.is_empty());
        assert!(
            ledger.snapshot().iter().all(|r| r.node_id != b),
            "actual 不因回环失配建档（不收回环地址）"
        );
    }

    // 8. 持久化往返：flush 产出 → 原子写 → 同路径重载保真（含 verified 集/
    //    冲突/失配）；防抖间隔内不产出；损坏文件告警重建空账本
    #[test]
    fn persistence_roundtrip_and_corruption_heal() {
        let dir = std::env::temp_dir().join(format!("os-identity-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("identity-ledger.json");
        let _ = std::fs::remove_file(&file);
        let mut ledger = IdentityLedger::new(Some(file.clone()));
        assert!(ledger.is_empty(), "无文件 → 空账本");
        let a = nid(12);
        let b = nid(13);
        ledger.record_evidence(&a, addr(1), EvidenceKind::Handshake, 1000);
        ledger.record_evidence(&a, addr(2), EvidenceKind::Gossip { verified: false }, 1010);
        ledger.record_evidence(
            &a,
            addr(2),
            EvidenceKind::ProbeMismatch { actual: b.clone() },
            1020,
        );
        ledger.record_conflict(&a, lo(33516), 1030);
        // 防抖：刚建档（last_flush=now）→ 未到 10s 不产出
        assert!(
            ledger.flush_due(Instant::now()).is_none(),
            "防抖间隔内不落盘"
        );
        // 强制刷（停机路径）+ 原子写
        let json = ledger.flush_final().expect("脏数据应产出");
        write_atomic(&file, &json).expect("临时目录写入必成功");
        assert!(ledger.flush_final().is_none(), "刷后不再脏");
        assert!(file.exists());
        // "重启"：同路径重载——全字段保真（冲突观测含回环也保真；a 的 addr(2)
        // 已因失配换到 b 名下——历史在 mismatch_events）
        let reloaded = IdentityLedger::new(Some(file.clone()));
        let rec_a = record_of(&reloaded, &a);
        assert_eq!(rec_a.verified_addrs, vec![addr(1)]);
        assert!(
            rec_a.unverified_addrs.is_empty(),
            "失配地址已换主（不在 a 集）"
        );
        assert_eq!((rec_a.first_seen, rec_a.last_seen), (1000, 1030));
        assert_eq!(rec_a.mismatch_events.len(), 1);
        assert_eq!(rec_a.conflict_entries.len(), 1);
        assert_eq!(rec_a.conflict_entries[0].addr, lo(33516));
        assert_eq!(record_of(&reloaded, &b).verified_addrs, vec![addr(2)]);
        // 损坏文件 → 告警重建空账本
        std::fs::write(&file, "{ not json !!!").unwrap();
        let healed = IdentityLedger::new(Some(file.clone()));
        assert!(healed.is_empty(), "损坏文件重建空账本");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 9. 加载剔除地址集回环存量：verified/unverified 里的回环逐条剥离
    //    （历史文件可能含回环——新定调下源头已拒收，此为纵深防御）
    #[test]
    fn load_strips_loopback_from_addr_sets() {
        let dir = std::env::temp_dir().join(format!("os-identity-lo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("identity-ledger-lo.json");
        let a = nid(14);
        let legacy = format!(
            "[{{\"node_id\":\"{a}\",\"verified_addrs\":[\"127.0.0.1:33516\",\"203.0.113.1:41000\"],\
             \"unverified_addrs\":[\"127.0.0.1:40000\"],\"first_seen\":100,\"last_seen\":200,\
             \"conflict_entries\":[],\"mismatch_events\":[]}}]"
        );
        std::fs::write(&file, legacy).unwrap();
        let ledger = IdentityLedger::new(Some(file.clone()));
        let rec = record_of(&ledger, &a);
        assert_eq!(rec.verified_addrs, vec![addr(1)], "回环剥离、公网保留");
        assert!(rec.unverified_addrs.is_empty(), "unverified 回环同样剥离");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 10. 上限：地址集 8 条（最旧挤出）；失配事件 64 条
    #[test]
    fn caps_truncate_oldest() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(15);
        for p in 0..10u16 {
            ledger.record_evidence(&a, addr(p), EvidenceKind::Handshake, 100);
        }
        let rec = record_of(&ledger, &a);
        assert_eq!(rec.verified_addrs.len(), LEDGER_ADDRS_CAP, "地址集上限 8");
        assert!(!rec.verified_addrs.contains(&addr(0)), "最旧被挤出");
        let b = nid(16);
        for p in 0..70u16 {
            ledger.record_evidence(
                &a,
                addr(p % 10),
                EvidenceKind::ProbeMismatch { actual: b.clone() },
                100 + u64::from(p),
            );
        }
        assert_eq!(
            record_of(&ledger, &a).mismatch_events.len(),
            LEDGER_MISMATCH_CAP,
            "失配事件上限 64"
        );
    }

    // 11. gossip 写放大修复（A2）：重复观测无集合变更不再置脏（稳态 10s 一轮
    //     的 gossip 转述不触发全量重写）；真实变更（新增/升级/迁出）照常置脏
    #[test]
    fn gossip_repeat_without_set_change_stays_clean() {
        let mut ledger = IdentityLedger::new(None);
        let a = nid(17);
        // 首次转述：unverified 入集 → 脏
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: false }, 100);
        assert!(ledger.flush_final().is_some(), "首次观测应产出");
        // 稳态重复：同一地址同一形态再来 → 集合无变化 → 不置脏
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: false }, 200);
        assert!(
            ledger.flush_final().is_none(),
            "重复观测无变化不应置脏（写放大修复）"
        );
        // 报告方验证位透传升级：unverified → verified = 实际变更 → 脏
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: true }, 210);
        assert!(ledger.flush_final().is_some(), "升级迁移应置脏");
        // 重复透传已 verified 且在首位 → 不置脏
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: true }, 220);
        assert!(ledger.flush_final().is_none(), "verified 重复透传不置脏");
        // 新地址（集合扩容）→ 脏
        ledger.record_evidence(&a, addr(2), EvidenceKind::Gossip { verified: false }, 230);
        assert!(ledger.flush_final().is_some(), "新地址入集应置脏");
        // 集合内地址换序（不在首位 → 去重置前）也是持久化内容变更 → 脏
        ledger.record_evidence(&a, addr(1), EvidenceKind::Gossip { verified: false }, 240);
        let rec = record_of(&ledger, &a);
        // addr(1) 在 verified 集（210 升级过）——unverified 分支不重复入集、不置脏
        assert!(rec.verified_addrs.contains(&addr(1)));
        assert!(ledger.flush_final().is_none());
    }

    // 12. 全局上限（A5）：身份条目达 LEDGER_RECORDS_CAP 后新键触发按 last_seen
    //     淘汰最旧；既有键续写不淘汰；账本长度收敛在上限
    #[test]
    fn global_records_cap_evicts_oldest() {
        let mut ledger = IdentityLedger::new(None);
        // 灌满 + 1 个身份（gossip 证据建档——O(1) 路径；last_seen 递增）
        for i in 0..=(LEDGER_RECORDS_CAP as u64) {
            let id = format!("0x{:04x}{}", i, "b".repeat(62));
            ledger.record_evidence(
                &id,
                format!("198.51.100.{}:40000", i % 251 + 1).parse().unwrap(),
                EvidenceKind::Gossip { verified: false },
                1000 + i,
            );
        }
        assert_eq!(ledger.len(), LEDGER_RECORDS_CAP, "全局上限收敛");
        // 最旧（i=0，last_seen=1000）被淘汰；最新存活
        assert!(
            ledger
                .get_record(&format!("0x0000{}", "b".repeat(62)))
                .is_none(),
            "最旧身份被淘汰"
        );
        assert!(
            ledger
                .get_record(&format!(
                    "0x{:04x}{}",
                    LEDGER_RECORDS_CAP as u64,
                    "b".repeat(62)
                ))
                .is_some(),
            "最新身份存活"
        );
        assert!(ledger.flush_final().is_some(), "淘汰/建档路径置脏");
        // 既有键续写：不触发淘汰（长度稳定，其他键不丢）
        let newest = format!("0x{:04x}{}", LEDGER_RECORDS_CAP as u64, "b".repeat(62));
        ledger.record_evidence(
            &newest,
            "198.51.100.9:40000".parse().unwrap(),
            EvidenceKind::Gossip { verified: false },
            999_999,
        );
        assert_eq!(ledger.len(), LEDGER_RECORDS_CAP);
        assert!(
            ledger
                .get_record(&format!("0x0001{}", "b".repeat(62)))
                .is_some(),
            "续写既有键不淘汰他者"
        );
    }
}
