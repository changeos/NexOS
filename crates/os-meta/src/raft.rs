//! Raft 纯算法核心——选举投票判定 / 日志复制水线（commitIndex 推进）/ 法定数。
//!
//! 本模块**不依赖 openraft**，纯函数实现 Raft 协议的核心判定逻辑，可独立单元测试。
//! 实现 `OpenraftConsensus` 时，这些纯函数作为内部算法被复用（见规格书 §3.5）。
//!
//! 设计依据（Raft 论文 §3.3 / §3.4 / §5.4.1 / §5.4.2 + 规划文档 §3.5）：
//! - 法定数（majority）：N 节点集群的 quorum = floor(N/2) + 1
//! - 选举：候选者收到至少 quorum 张投票（含自己）即当选
//! - commitIndex 推进：仅 leader 可推进；从高到低找第一个"被 quorum 复制 + 同 term"的索引
//!
//! 关于 openraft：openraft 是状态机实现，其内部已封装完整的 Raft 协议。
//! 本模块的纯函数用于：①上层测试/可视化决策路径；②自研简化场景（如单测 fixture）；
//! ③作为 `OpenraftConsensus::status` 等查询方法的辅助计算。openraft 引入前不阻塞。

use os_core::{Deserialize, NodeId, Serialize};

// ----------------------------------------------------------------------------
// Raft 基础类型
// ----------------------------------------------------------------------------

/// Raft 日志索引（单调递增，从 1 起；0 表示"空"）
pub type LogIndex = u64;

/// Raft 任期号（单调递增，每次选举自增）
pub type Term = u64;

/// 法定数（quorum = floor(N/2) + 1），N 为集群投票成员数
pub fn majority(cluster_size: u32) -> u32 {
    if cluster_size == 0 {
        return 0;
    }
    cluster_size / 2 + 1
}

// ----------------------------------------------------------------------------
// LogEntry（与 MetaStore.apply_log 配合：业务命令进 log，apply 时作用到状态机）
// ----------------------------------------------------------------------------

/// Raft 日志条目（论文 §5.3）。
///
/// - `term`：该条目被创建时的任期（leader 写入时填自己的 term）
/// - `index`：日志位置（1 起）
/// - `command`：业务命令（JSON），apply 时由 MetaStore.apply_log 作用到本地 SQLite
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// 该条目的任期
    pub term: Term,
    /// 该条目的日志索引
    pub index: LogIndex,
    /// 业务命令（JSON 序列化；apply 时由 MetaStore 解释）
    pub command: serde_json::Value,
}

// ----------------------------------------------------------------------------
// QuorumConfig（成员集 + 法定数）
// ----------------------------------------------------------------------------

/// 集群法定数配置——成员集合 + 派生的 quorum。
///
/// 不直接含 `Vec<NodeInfo>`（业务字段过多），仅持投票成员 ID 集合，
/// 法定数由 `majority(members.len())` 派生。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuorumConfig {
    /// 投票成员集合（peer 节点不进法定，不在此集合，见 §3.5）
    pub members: Vec<NodeId>,
}

impl QuorumConfig {
    /// 用成员列表构造。
    pub fn new(members: Vec<NodeId>) -> Self {
        Self { members }
    }

    /// 投票成员数。
    pub fn size(&self) -> u32 {
        // members 不会超过 u32 范围（集群规模上限远小于 2^32）
        u32::try_from(self.members.len()).unwrap_or(u32::MAX)
    }

    /// 法定数（quorum = floor(N/2) + 1）。
    pub fn quorum(&self) -> u32 {
        majority(self.size())
    }

    /// 判断给定投票数是否已达法定数。
    pub fn has_quorum(&self, votes: u32) -> bool {
        votes >= self.quorum()
    }

    /// 判断给定成员子集是否已达法定数。
    pub fn has_quorum_set(&self, granted: &[NodeId]) -> bool {
        // 同一节点重复投票不计入；去重后计数
        let distinct = granted
            .iter()
            .filter(|n| self.members.contains(n))
            .collect::<std::collections::HashSet<_>>()
            .len();
        self.has_quorum(u32::try_from(distinct).unwrap_or(u32::MAX))
    }
}

// ----------------------------------------------------------------------------
// 选举：投票判定（论文 §5.4.1 / §5.4.2）
// ----------------------------------------------------------------------------

/// 一张投票记录（候选者 → 投票给谁）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    /// 投票者节点
    pub voter: NodeId,
    /// 被投的候选者
    pub candidate: NodeId,
    /// 该投票发生时的任期
    pub term: Term,
}

/// 选举结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElectionOutcome {
    /// 候选者已获 quorum 投票，当选
    Won,
    /// 投票数不足 quorum，未能当选（选举继续或失败）
    Lost,
}

/// 判定候选者是否在给定投票集（含自投）下当选（论文 §5.4.1）。
///
/// 规则：
/// 1. 仅统计 `term` 与 `candidate_term` 一致、且 `candidate` 匹配的票；
/// 2. 投票者必须是法定成员；
/// 3. 去重后票数 ≥ quorum 即 `Won`。
pub fn check_election(
    cfg: &QuorumConfig,
    candidate: &NodeId,
    candidate_term: Term,
    votes: &[Vote],
) -> ElectionOutcome {
    let mut granted: Vec<NodeId> = Vec::new();
    for v in votes {
        if v.term == candidate_term && &v.candidate == candidate {
            granted.push(v.voter.clone());
        }
    }
    if cfg.has_quorum_set(&granted) {
        ElectionOutcome::Won
    } else {
        ElectionOutcome::Lost
    }
}

/// 计算候选者在指定任期内已获得的（去重）有效票数。
pub fn granted_votes(
    cfg: &QuorumConfig,
    candidate: &NodeId,
    candidate_term: Term,
    votes: &[Vote],
) -> u32 {
    let mut granted: Vec<NodeId> = Vec::new();
    for v in votes {
        if v.term == candidate_term && &v.candidate == candidate {
            granted.push(v.voter.clone());
        }
    }
    u32::try_from(
        granted
            .iter()
            .filter(|n| cfg.members.contains(n))
            .collect::<std::collections::HashSet<_>>()
            .len(),
    )
    .unwrap_or(u32::MAX)
}

// ----------------------------------------------------------------------------
// 日志复制水线（commitIndex 推进，论文 §5.4.2 + §5.4.3 leader commit 规则）
// ----------------------------------------------------------------------------

/// 单个 peer 的复制进度（leader 维护 matchIndex，论文 §5.3）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMatch {
    /// 该 peer 已确认复制的最高 log index（0 = 尚无）
    pub match_index: LogIndex,
}

/// 推进 commitIndex（论文 §5.4.2：leader 找到最大的 N 满足：
/// N > commitIndex、多数 matchIndex ≥ N、且 `log[N].term` == current_term）。
///
/// 参数：
/// - `commit_index`：当前 commitIndex
/// - `last_log_index`：本地最后一条日志的 index（N 的上界）
/// - `current_term`：leader 当前任期（§5.4.3：仅提交本任期日志防脑裂）
/// - `log_term_at`：闭包，返回 `log[index]` 的 term（index 越界由调用方保证不会发生：
///   N 从 last_log_index 开始递减且 last_log_index 合法）
/// - `self_match`：leader 自身的 matchIndex（= last_log_index，自身总持全部日志）
/// - `peer_matches`：其余法定成员的 matchIndex 列表
///
/// 返回推进后的 commitIndex（≥ 原 commit_index）。
pub fn advance_commit_index<F>(
    cfg: &QuorumConfig,
    commit_index: LogIndex,
    last_log_index: LogIndex,
    current_term: Term,
    log_term_at: F,
    self_match: LogIndex,
    peer_matches: &[LogIndex],
) -> LogIndex
where
    F: Fn(LogIndex) -> Term,
{
    // 防御：last_log_index 至少是 0（空日志）；空日志时无可提交，直接返回当前值。
    if last_log_index == 0 {
        return commit_index;
    }

    // 从高到低扫描候选 N（last_log_index ..= commit_index+1）
    let mut new_commit = commit_index;
    let mut n = last_log_index;
    while n > commit_index {
        // 收集所有 matchIndex ≥ N 的副本计数
        let mut count: u32 = 0;
        if self_match >= n {
            count += 1;
        }
        for pm in peer_matches {
            if *pm >= n {
                count += 1;
            }
        }
        // 是否达法定数？
        if cfg.has_quorum(count) {
            // §5.4.3：仅提交本任期（current_term）的条目，避免旧任期的脑裂条目被提交
            if log_term_at(n) == current_term {
                new_commit = n;
                break;
            }
            // 该 N 属于旧任期 → 不能提交，继续向下找（更小的 N 可能属本任期）
        }
        n -= 1;
    }
    new_commit
}

/// 辅助：从日志切片直接计算 commitIndex（常用场景：内存持有完整日志）。
///
/// `log` 须按 index 升序排列；`log[i].index` 从 1 起连续（调用方保证）。
pub fn advance_commit_index_from_log(
    cfg: &QuorumConfig,
    log: &[LogEntry],
    commit_index: LogIndex,
    current_term: Term,
    self_match: LogIndex,
    peer_matches: &[LogIndex],
) -> LogIndex {
    let last = log.last().map(|e| e.index).unwrap_or(0);
    let term_of = |idx: LogIndex| -> Term {
        // log 按 index 升序连续，故 idx ∈ [1, last] 时位于 log[idx-1]
        log.get(
            usize::try_from(idx)
                .ok()
                .and_then(|i| i.checked_sub(1))
                .unwrap_or(0),
        )
        .map(|e| e.term)
        .unwrap_or(0)
    };
    advance_commit_index(
        cfg,
        commit_index,
        last,
        current_term,
        term_of,
        self_match,
        peer_matches,
    )
}

// ----------------------------------------------------------------------------
// 日志比较（up-to-date 判定，论文 §5.4.1 RequestVote 投票规则）
// ----------------------------------------------------------------------------

/// 判定候选者的日志是否"至少与本地一样新"（论文 §5.4.1）。
///
/// 规则：比较 (last_log_term, last_log_index)：
/// - term 大者更新；term 相同则 index 大者更新。
pub fn log_is_up_to_date(
    cand_last_term: Term,
    cand_last_index: LogIndex,
    my_last_term: Term,
    my_last_index: LogIndex,
) -> bool {
    if cand_last_term != my_last_term {
        cand_last_term > my_last_term
    } else {
        cand_last_index >= my_last_index
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(s: &str) -> NodeId {
        NodeId::new(s)
    }

    fn cfg_of(members: &[&str]) -> QuorumConfig {
        QuorumConfig::new(members.iter().map(|s| node(s)).collect())
    }

    // ---- majority / QuorumConfig ----

    #[test]
    fn majority_basic() {
        assert_eq!(majority(0), 0);
        assert_eq!(majority(1), 1);
        assert_eq!(majority(2), 2);
        assert_eq!(majority(3), 2);
        assert_eq!(majority(4), 3);
        assert_eq!(majority(5), 3);
        assert_eq!(majority(7), 4);
    }

    #[test]
    fn quorum_config_methods() {
        let cfg = cfg_of(&["a", "b", "c"]);
        assert_eq!(cfg.size(), 3);
        assert_eq!(cfg.quorum(), 2);
        assert!(cfg.has_quorum(2));
        assert!(!cfg.has_quorum(1));
        assert!(cfg.has_quorum_set(&[node("a"), node("b")]));
        assert!(!cfg.has_quorum_set(&[node("a")]));
    }

    #[test]
    fn quorum_set_dedups_and_filters_nonmembers() {
        let cfg = cfg_of(&["a", "b", "c"]);
        // 重复节点 + 非成员节点，去重后仅 1 个有效
        assert!(!cfg.has_quorum_set(&[node("a"), node("a"), node("z")]));
        // 2 个不同有效成员 → quorum
        assert!(cfg.has_quorum_set(&[node("a"), node("b"), node("z")]));
    }

    // ---- 选举 ----

    #[test]
    fn election_wins_on_quorum() {
        let cfg = cfg_of(&["a", "b", "c"]);
        let cand = node("a");
        let votes = vec![
            Vote {
                voter: node("a"),
                candidate: cand.clone(),
                term: 1,
            },
            Vote {
                voter: node("b"),
                candidate: cand.clone(),
                term: 1,
            },
        ];
        assert_eq!(check_election(&cfg, &cand, 1, &votes), ElectionOutcome::Won);
        assert_eq!(granted_votes(&cfg, &cand, 1, &votes), 2);
    }

    #[test]
    fn election_loses_below_quorum() {
        let cfg = cfg_of(&["a", "b", "c"]);
        let cand = node("a");
        let votes = vec![Vote {
            voter: node("a"),
            candidate: cand.clone(),
            term: 1,
        }];
        assert_eq!(
            check_election(&cfg, &cand, 1, &votes),
            ElectionOutcome::Lost
        );
    }

    #[test]
    fn election_filters_by_term_and_candidate() {
        let cfg = cfg_of(&["a", "b", "c"]);
        let cand = node("a");
        let votes = vec![
            Vote {
                voter: node("a"),
                candidate: cand.clone(),
                term: 1,
            },
            Vote {
                voter: node("b"),
                candidate: cand.clone(),
                term: 2,
            }, // 不同任期
            Vote {
                voter: node("c"),
                candidate: node("b"),
                term: 1,
            }, // 不同候选者
        ];
        // 仅 1 票对 (cand, term=1) 有效
        assert_eq!(
            check_election(&cfg, &cand, 1, &votes),
            ElectionOutcome::Lost
        );
    }

    #[test]
    fn election_5_node_needs_3() {
        let cfg = cfg_of(&["a", "b", "c", "d", "e"]);
        let cand = node("a");
        let votes = vec![
            Vote {
                voter: node("a"),
                candidate: cand.clone(),
                term: 5,
            },
            Vote {
                voter: node("b"),
                candidate: cand.clone(),
                term: 5,
            },
        ];
        assert_eq!(
            check_election(&cfg, &cand, 5, &votes),
            ElectionOutcome::Lost
        );
        // 加一张即 quorum(3)
        let votes2 = [
            votes,
            vec![Vote {
                voter: node("c"),
                candidate: cand.clone(),
                term: 5,
            }],
        ]
        .concat();
        assert_eq!(
            check_election(&cfg, &cand, 5, &votes2),
            ElectionOutcome::Won
        );
    }

    // ---- commitIndex ----

    #[test]
    fn commit_empty_log_no_advance() {
        let cfg = cfg_of(&["a", "b", "c"]);
        assert_eq!(advance_commit_index(&cfg, 0, 0, 1, |_| 0, 0, &[]), 0);
    }

    #[test]
    fn commit_advances_when_quorum_replicated() {
        // 3 节点：leader a + followers b/c，quorum=2
        let cfg = cfg_of(&["a", "b", "c"]);
        // log: index 1..=3, 全 term=1
        let term_of = |_: LogIndex| 1;
        // leader 持全量 (self_match=3)，b 复制到 3，c 落后到 1
        assert_eq!(advance_commit_index(&cfg, 0, 3, 1, term_of, 3, &[3, 1]), 3);
        // b 复制到 3，c 复制到 2 → 最大 N 满足 quorum(≥3) 是 3? 不：{3(leader),3(b)} quorum → N=3
        assert_eq!(advance_commit_index(&cfg, 0, 3, 1, term_of, 3, &[3, 2]), 3);
        // 仅 leader + c 复制到 3，b 落后到 1 → N=3 时 count=2 (leader+c) → quorum → 提交 3
        assert_eq!(advance_commit_index(&cfg, 0, 3, 1, term_of, 3, &[1, 3]), 3);
        // b/c 都只到 1 → N=3 不 quorum，N=2 不 quorum，N=1 quorum 但 ≤ commit_index(0)? 1>0 ok
        assert_eq!(advance_commit_index(&cfg, 0, 3, 1, term_of, 3, &[1, 1]), 1);
    }

    #[test]
    fn commit_no_advance_below_current_commit() {
        let cfg = cfg_of(&["a", "b", "c"]);
        // commitIndex 已 2，即便 peer 落后，不回退
        assert_eq!(advance_commit_index(&cfg, 2, 3, 1, |_| 1, 3, &[1, 1]), 2);
    }

    #[test]
    fn commit_only_current_term_brain_split_guard() {
        // §5.4.3 脑裂防护：旧 term 条目不能直接提交
        // log: index 1..=3, term [1,1,2]，current_term=2
        // 即使 quorum 复制了 index 3（term=2），可以提交；
        // 但若仅复制到 index 2（term=1），即使 quorum 也不提交（旧任期）
        let cfg = cfg_of(&["a", "b", "c"]);
        let terms = [1u64, 1, 2];
        let term_of = |idx: LogIndex| terms[usize::try_from(idx).unwrap() - 1];
        // 全员复制到 3（term=2）→ 提交 3
        assert_eq!(advance_commit_index(&cfg, 0, 3, 2, term_of, 3, &[3, 3]), 3);
        // 全员仅复制到 2（term=1）→ N=2 是旧任期，不能提交；N=3 无人复制 → 不提交 → 保持 0
        assert_eq!(advance_commit_index(&cfg, 0, 3, 2, term_of, 2, &[2, 2]), 0);
    }

    #[test]
    fn commit_finds_largest_with_quorum() {
        // 5 节点 quorum=3
        let cfg = cfg_of(&["a", "b", "c", "d", "e"]);
        let term_of = |_: LogIndex| 1u64;
        // leader@5, b@5, c@3, d@2, e@1
        // N=5: leader+b = 2 <3; N=4: 2<3; N=3: leader+b+c=3 ≥3 → 提交 3
        assert_eq!(
            advance_commit_index(&cfg, 0, 5, 1, term_of, 5, &[5, 3, 2, 1]),
            3
        );
    }

    #[test]
    fn advance_commit_index_from_log_works() {
        let cfg = cfg_of(&["a", "b", "c"]);
        let log: Vec<LogEntry> = (1..=3u64)
            .map(|i| LogEntry {
                term: 1,
                index: i,
                command: serde_json::json!({"i": i}),
            })
            .collect();
        assert_eq!(
            advance_commit_index_from_log(&cfg, &log, 0, 1, 3, &[3, 1]),
            3
        );
        assert_eq!(
            advance_commit_index_from_log(&cfg, &log, 0, 1, 3, &[1, 1]),
            1
        );
    }

    // ---- log up-to-date ----

    #[test]
    fn log_up_to_date_term_wins() {
        // cand term 大 → 更新
        assert!(log_is_up_to_date(2, 1, 1, 100));
        // cand term 小 → 更旧
        assert!(!log_is_up_to_date(1, 100, 2, 1));
    }

    #[test]
    fn log_up_to_date_index_tiebreak() {
        // term 相同，index 比较
        assert!(log_is_up_to_date(1, 10, 1, 5));
        assert!(log_is_up_to_date(1, 5, 1, 5)); // 等于也算"至少一样新"
        assert!(!log_is_up_to_date(1, 4, 1, 5));
    }

    // ---- LogEntry ----

    #[test]
    fn log_entry_serde_roundtrip() {
        let e = LogEntry {
            term: 7,
            index: 42,
            command: serde_json::json!({"op":"put","k":"a"}),
        };
        let s = serde_json::to_string(&e).unwrap();
        let e2: LogEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(e, e2);
    }

    #[test]
    fn quorum_config_serde_roundtrip() {
        let cfg = cfg_of(&["a", "b"]);
        let s = serde_json::to_string(&cfg).unwrap();
        let cfg2: QuorumConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, cfg2);
    }
}
