//! 身份账本（IdentityLedger）——指纹证据登记 + 对比判定 + 冲突/失配观测 + 持久化。
//!
//! 本 crate 是 NexOS 的**独立身份组件**（2026-08-25 从 os-p2p 抽离，用户定调：
//! 「指纹信息对比现有的库里的指纹信息就可以，是不是单独做一个组件完成指纹对比
//! 更好？不要集成在 p2p 里面了」）：os-p2p 回归纯传输层（握手验证是协议、留在
//! p2p），「记谁、信谁、地址属于谁」的账本与对比策略全部由本 crate 承担。架构
//! 地位对照 Bitcoin addrman（地址管理独立于网络层）与 Tailscale 协调面。详见
//! `docs/IDENTITY_COMPONENT.md`。
//!
//! # 数据模型
//!
//! - **NodeID = 指纹**：secp256k1 压缩公钥（`0x` + 66 hex）。本 crate 视其为
//!   **不透明字符串**（解析/验签属于身份发行方 os-p2p / os-common chain_auth），
//!   只负责「同一 NodeID 在哪些地址被谁验证过」的记账与对比。
//! - [`IdentityRecord`]：`{node_id, verified_addrs, unverified_addrs, first_seen,
//!   last_seen, conflict_entries, mismatch_events}`——一个身份一条。
//! - **verified/unverified 地址集**：verified 仅两种证据可入（直连握手天然验证 /
//!   指纹验证探测成功）；gossip 转述地址只入 unverified（未经本机验证的地址
//!   不得作为该节点的凭据——与 os-p2p meta 组件同一语义，meta 侧仍保留自己的
//!   展示位，本账本是身份事实的**唯一权威源**）。
//!
//! # 证据语义（[`EvidenceKind`]）
//!
//! | 证据 | 效果 |
//! |---|---|
//! | `Handshake` | 地址升 verified（最强证据：握手签名验证过地址背后确为该 NodeID） |
//! | `ProbeVerified` | 地址升 verified（指纹验证探测成功——TCP connect + 握手比对 NodeID） |
//! | `ProbeMismatch { actual }` | 目标身份降级该地址（verified → unverified）+ 记 [`MismatchEvent`]；**同时**把地址升到 `actual` 名下 verified（探测完成了真实握手——地址换人被实证） |
//! | `Gossip { verified }` | 他节点转述：`verified=false` 只入 unverified；`verified=true` 透传（报告方验证过）——但**不覆盖**本机已验证结论 |
//!
//! **地址换人**：同一地址同一时刻只属于一个身份——任何 verified 级证据（握手/
//! 探测）把地址归到新身份时，从其他身份的地址集中移除（IP 换人由指纹机制天然
//! 处理，无需修复——新身份靠自广播重新被全网感知）。
//!
//! # 回环定调（2026-08-25 用户原话：「127.0.0.1 无论怎么产生的，都应该屏蔽」）
//!
//! - **地址归属证据一律拒收回环**（record_evidence）：127.0.0.0/8 / ::1 对全网
//!   没有凭据价值（对端机器的 127.0.0.1 不是我的 127.0.0.1），verified/
//!   unverified 集永不收录；持久化加载时无条件剔除历史存量。
//! - **冲突观测（record_conflict）例外**：同 NodeID 多地址观测恰恰常发生在同机
//!   多实例（回环进入）——`remote_addr` 是 socket 观测地址（知情面），不是可拨
//!   凭据，照记不拒。
//!
//! # 持久化
//!
//! JSON 文件（`Vec<IdentityRecord>`），防抖落盘（脏标记 + 10s 间隔 + 停机强刷），
//! 原子写（同目录临时文件 + fsync + rename——中途崩溃不留半截文件）。加载失败
//! 告警并重建空账本。I/O 由宿主（os-p2p 引擎 / os-api 装配层）在锁外执行，
//! [`IdentityLedger::flush_due`]/[`IdentityLedger::flush_final`] 只产出 JSON 串。
//!
//! # 并发约定
//!
//! 账本本体非线程安全——宿主以 `Arc<Mutex<IdentityLedger>>` 共享（os-p2p
//! [`SharedLedger`] 类型别名），std Mutex 短临界区、**持锁绝不 await**（与
//! os-p2p `api::State` 同款约定）。
//!
//! # 测试
//!
//! `cargo test -p os-identity`：证据登记/升降级/地址换人/owns_addr 判定/
//! mismatch 观测/冲突计数/回环拒绝/持久化往返与损坏重建，≥8 组。

pub mod ledger;

pub use ledger::{
    write_atomic, AddrOwnership, ConflictEntry, EvidenceKind, IdentityConflict, IdentityLedger,
    IdentityRecord, MismatchEvent, MismatchReport, FLUSH_DEBOUNCE, LEDGER_ADDRS_CAP,
    LEDGER_CONFLICTS_CAP, LEDGER_MISMATCH_CAP,
};

/// 共享账本句柄（os-p2p 引擎与 os-api 装配层共用同一实例的注入形态）。
pub type SharedLedger = std::sync::Arc<std::sync::Mutex<IdentityLedger>>;
