//! 节点元数据组件——设计 §3「meta」：集群节点注册表 + 专用心跳检测引擎 +
//! 节点间元数据交互 + 健康排名 + 持久化。
//!
//! 本组件是 os-p2p 内**唯一**做节点存活判定的位置（其他组件以后只从这里取
//! 信息，os-api 接入是下一批）：
//!
//! - **注册表**（[`NodeMetaStore`]）：所有连接过本节点的节点都留档
//!   `{NodeID → 地址历史 / first_seen / last_seen / 状态 / 来源}`。两个写入口：
//!   `register_conn` 成功后的 [`NodeMetaStore::record_conn`]（Direct——本机
//!   直连观测，第一手信号）与他节点交互摘要的 [`NodeMetaStore::merge_digest`]
//!   （Gossip——转述信号）。JSON 文件持久化（`P2pConfig::meta_file`，
//!   None = 纯内存），重启不丢。
//! - **专用心跳引擎**（[`meta_engine`]）：按分数分级节奏探测已知节点——
//!   分数 ≥80 每 6 tick / 50-79 每 3 tick / <50 每 tick（分数高=一直健康=探得
//!   稀）。有活连接 = 活性证据（更新 last_seen 即可，不额外探测）；无连接走
//!   **指纹验证探测**（TCP connect + 复用既有握手路径完成挑战-签名握手，
//!   [`fingerprint_probe`] 比对握手返回的对端真实 NodeID == 条目 NodeID 才算
//!   心跳成功——裸 connect 会把"任何监听端口"判成存活，gossip 谎报一个内网
//!   地址就能制造假条目；握手成本高于裸 connect，节奏规则不变）。**红线**：
//!   探测不 `register_conn`（本机不产生 P2P 连接/入桶）——握手完成读完 hello
//!   即 drop 两条半边流（对端 accept 侧按普通入站连接处理，随即因 EOF 关闭，
//!   属既有入站路径的标准行为）。指纹不匹配（地址背后是别的节点——谎报/
//!   陈旧 gossip 观测）记心跳失败 + `warn` 并撤销 verified（指纹机制天然
//!   处理 IP 换人：握到别人即旧信息不可信，无需修复——新身份靠自广播
//!   重新被全网感知）。连续 5 次失败移入
//!   `Inactive`（**不再心跳**）。复活仅两条路：手动 `Handle::meta_reactivate` /
//!   他节点交互报告其存活（last_seen 新鲜）。
//! - **非活跃 TTL 清除**（2026-09-02，「集群节点里加个规则，非活跃节点，三天
//!   不心跳就移除」）：Inactive 条目 `last_seen` 距今超 TTL（默认 3 天 = 259,200
//!   秒；env `NEXOS_P2P_INACTIVE_TTL_SECS` 覆盖，`0` = 禁用——向后兼容开关）
//!   即**整条删除**（内存 + 置脏 → 防抖落盘重写 node-meta.json，重启不再
//!   出现）。只清 Inactive——Active 永不过期（远古僵尸交评分/五振机制处理，
//!   漏判好过误杀）；复活的三条路（直连 / 手动 / 新鲜报告）都刷新 last_seen，
//!   复活即自然续命。扫描节奏见 [`META_PURGE_EVERY_TICKS`]（节流，不逐 tick
//!   扫全表）+ 引擎启动加载后即时机扫一次（存量超期条目上线即清）。
//! - **指纹（verified）语义**：条目与每条地址都带 verified 位——本机直连
//!   （握手天然验证）/ 指纹验证探测成功 → true；gossip 新建 → false（经探测
//!   "洗白"）；探测发现指纹不匹配 → 置回 false。digest 条目的指纹即其 NodeID
//!   （握手签名验证的公钥），接收侧必须经心跳验证才能采信地址；远端报告的
//!   verified 位随地址透传（未验证地址不参与 LAN 判定的语义由下游处理）。
//! - **元数据交互**：每 6 tick 向所有已连节点广播注册表摘要
//!   （[`crate::transport::FrameKind::MetaGossip`]，条目见 [`MetaDigestEntry`]），
//!   收到即合并入库（学习新节点 / 新鲜度更新 / 复活 Inactive）。
//!   **首条固定为自广播**：`{self_id, advertise 地址, 活着, 已验证}`——上线即
//!   广播自身活性（`advertise=None` 时只带 id/alive），新身份（重装换新
//!   NodeID）经一次 gossip 即被全网感知；合并侧对 `id == self_id` 的条目（含
//!   自广播回声）跳过不落库——注册表不收录本机。
//!   **回环地址彻底屏蔽**（2026-08-25 用户定调：「127.0.0.1 无论怎么产生
//!   的，都应该屏蔽」——取代早前「本机直连回环照记、只是不外传」的不对称
//!   语义）：127.0.0.1/::1 **不入注册表**——`record_conn` 观测到回环跳过
//!   记录（不建档、不往 addrs 里加，条目活性照常维护）、[`push_meta_addr`]
//!   入口直接拒绝（防御兜底——merge/指纹等所有来源都过不了）、加载旧文件
//!   时无条件剔除历史存量（曾因测试进程拨本机生产端口，回环观测地址被
//!   广播到全网成节点发现页噪声）。digest 出口剔除 / merge 入口拒收保留为
//!   纵深防御。同机多实例的观测由 `register_conn` 的 identity_conflicts
//!   记账承担，注册表不需要回环条目。
//! - **健康排名**：心跳一直正常排名靠前——成功 +5（封顶 100，起始 50）、
//!   失败 -20（下限 0）；`Handle::node_meta` 按分数降序输出（Inactive 殿后）。
//!
//! 持久化防抖：脏标记 + 每 10s 落盘一次 + 停机（`P2pNode::shutdown`）同步
//! 刷盘一次；原子写（同目录临时文件 + rename，参考 bootstrap 私钥的写法）；
//! 加载失败告警并重建空注册表。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::api::{unix_now, Conn, Shared};
use crate::identity::NodeId;
use crate::transport::Frame;

/// 起始分数（新建档即 50——中性起点，探活路径再拉开差距）。
pub const META_SCORE_START: u8 = 50;
/// 分数上限（一直健康者封顶）。
pub const META_SCORE_MAX: u8 = 100;
/// 心跳成功的加分步长。
pub const META_SCORE_SUCCESS_STEP: u8 = 5;
/// 心跳失败的减分步长。
pub const META_SCORE_FAIL_STEP: u8 = 20;
/// 连续失败几次出局（移入 Inactive，不再心跳）。
pub const META_MAX_CONSEC_FAIL: u8 = 5;
/// 复活后的起始分数（手动复活 / 交互报告复活同值——给观察窗口但排在健康者后）。
pub const META_REVIVE_SCORE: u8 = 30;
/// 单条目地址历史上限（去重，最新在前）。
pub const META_ADDRS_CAP: usize = 8;
/// 交互摘要只携带 last_seen 在此窗口内（unix 秒）的条目（压缩——死档案不广播）。
pub const META_DIGEST_FRESH_SECS: u64 = 24 * 3600;
/// 交互摘要单帧编码上限（超出截断最旧条目并记 debug 日志）。
pub const META_DIGEST_MAX_BYTES: usize = 64 * 1024;
/// 元数据交互周期（tick 数）：每 6 tick 广播一次注册表摘要。
pub const META_GOSSIP_EVERY_TICKS: u64 = 6;
/// 持久化防抖间隔（脏标记起效后至少隔这么久才落盘；停机时无视间隔强制刷）。
pub const META_FLUSH_DEBOUNCE: Duration = Duration::from_secs(10);
/// 非活跃条目 TTL 默认：**3 天**（259,200 秒）——Inactive 且 `last_seen` 距今
/// 超过此值的条目从注册表整条删除（env [`ENV_INACTIVE_TTL_SECS`] 覆盖，
/// `0` = 禁用清除）。
pub const META_INACTIVE_TTL_DEFAULT_SECS: u64 = 3 * 24 * 3600;
/// TTL 清除扫描节流：**每 300 tick 扫一次全表**（默认 `meta_tick` = 5s →
/// 约 25 分钟一次；测试节奏 150ms → 45s 一次）。不做逐 tick 扫描——全表
/// 线性过滤对大注册表是纯开销，TTL 粒度为天级，漏过一个节流窗口无影响。
/// 引擎启动（注册表加载完成）后另有一次即时机扫描——存量超期条目（如重装
/// 换代节点的旧 NodeID）上线即清，不等首个节流窗口。
pub const META_PURGE_EVERY_TICKS: u64 = 300;
/// 非活跃 TTL env（秒）：未设置 = 默认 3 天；`0` = 禁用清除（向后兼容开关）；
/// 非法值告警并回落默认。引擎启动时读取一次（meta 组件自读——不经
/// `P2pConfig` 装配，CLI 与 os-api 内嵌节点同一路径生效）。
pub const ENV_INACTIVE_TTL_SECS: &str = "NEXOS_P2P_INACTIVE_TTL_SECS";

// ============================================================================
// 数据模型
// ============================================================================

/// 节点存活状态（注册表条目的状态机）。
///
/// `Active`：心跳对象（携带健康分与连续失败计数）；`Inactive`：五振出局——
/// **不再心跳**，仅手动复活或他节点交互报告可恢复。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetaState {
    /// 活跃（心跳周期内）。
    Active {
        /// 健康分（成功 +5 封顶 100 / 失败 -20 下限 0；排名依据）。
        score: u8,
        /// 连续心跳失败计数（成功清零；到 [`META_MAX_CONSEC_FAIL`] 出局）。
        consec_fail: u8,
    },
    /// 非活跃（五振出局；`since` = 出局时刻 unix 秒）。
    Inactive {
        /// 出局时刻（unix 秒）。
        since: u64,
    },
}

/// 注册表条目的知识来源。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetaSource {
    /// 本机直连观测（register_conn——第一手信号）。
    Direct,
    /// 他节点交互报告（Gossip 转述）。
    Gossip,
}

/// 注册表地址条目：地址 + 指纹验证位。
///
/// `verified=true` 仅两种来源：本机直连（握手天然验证）或指纹验证探测成功
/// （[`fingerprint_probe`] 比对握手返回的 NodeID）。gossip 转述地址按报告方
/// 的 verified 位透传——**未经本机验证的地址不得作为该节点的凭据**（下游
/// LAN 判定只认验证地址；gossip 谎报一个内网地址无法凭裸 connect 洗白）。
///
/// 序列化向后兼容：旧持久化格式为裸 `SocketAddr` 字符串数组，加载时迁移为
/// `verified=false`（本层无从考证旧档是否验证过，一律交心跳探测重验）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MetaAddr {
    /// 观测地址。
    pub addr: SocketAddr,
    /// 指纹验证位（true = 本机/报告方确认过地址背后确为该 NodeID 的节点）。
    pub verified: bool,
}

impl MetaAddr {
    /// 未验证地址条目（gossip 转述 / 旧格式迁移）。
    #[must_use]
    pub fn unverified(addr: SocketAddr) -> Self {
        Self {
            addr,
            verified: false,
        }
    }

    /// 已验证地址条目（直连观测 / 指纹验证探测成功）。
    #[must_use]
    pub fn verified(addr: SocketAddr) -> Self {
        Self {
            addr,
            verified: true,
        }
    }
}

impl<'de> Deserialize<'de> for MetaAddr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// 线上/磁盘双格式：旧格式为裸地址字符串（迁移为 verified=false），
        /// 新格式为 {addr, verified} 对象。
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum MetaAddrWire {
            /// 旧持久化格式：裸 SocketAddr。
            Legacy(SocketAddr),
            /// 新格式（verified 缺省视为 false——对端旧版本 digest 兼容）。
            Current {
                addr: SocketAddr,
                #[serde(default)]
                verified: bool,
            },
        }
        match MetaAddrWire::deserialize(deserializer)? {
            MetaAddrWire::Legacy(addr) => Ok(MetaAddr::unverified(addr)),
            MetaAddrWire::Current { addr, verified } => Ok(MetaAddr { addr, verified }),
        }
    }
}

/// 注册表单条目（观察面 `Handle::node_meta` 的元素；持久化 JSON 的单元）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeMetaEntry {
    /// 节点身份（即指纹——握手挑战-签名验证的公钥，digest 交互以此为准）。
    pub id: NodeId,
    /// 观测地址历史（去重，最新在前，上限 [`META_ADDRS_CAP`]；每条带指纹
    /// 验证位）。
    pub addrs: Vec<MetaAddr>,
    /// 首次见到（unix 秒）。
    pub first_seen: u64,
    /// 最近一次确认存活（unix 秒）。
    pub last_seen: u64,
    /// 存活状态（Active 携带分数 / Inactive 携带出局时刻）。
    pub state: MetaState,
    /// 知识来源（直连观测 / 交互转述）。
    pub source: MetaSource,
    /// 条目级指纹验证位：本机直连或指纹验证探测成功过 == true；gossip 合并
    /// **不改变**本位（新建为 false，靠探测洗白）；探测发现指纹不匹配置回
    /// false。旧持久化格式无此字段 → 加载为 false（探测重验）。
    #[serde(default)]
    pub verified: bool,
    /// 该节点是否声明可作网络出口（network-exit，2026-08-30）。来源：对端
    /// digest 自广播/转述的 `exit_offered` 位透传（本地不自判——本节点自己
    /// 的 offer 状态在 [`NodeMetaStore::self_exit`]，不落注册表）。旧持久化
    /// 格式无此字段 → 加载为 false。
    #[serde(default)]
    pub exit_offered: bool,
}

/// 元数据交互摘要条目（每 6 tick 广播给所有已连节点）。
///
/// **指纹 = `id`（NodeID）**——握手签名验证的公钥，不可谎报；`addr` 只是
/// 报告方视角的观测地址，**接收侧必须经心跳指纹验证才能采信**（merge 只
/// 入库标记，不作为存活/LAN 凭据）。`Inactive` 的条目也携带（`alive=false`）
/// ——让对端知道"我们还见过它"；对端仅在报告 `alive=true` 且 `last_seen`
/// 新鲜时复活本地 Inactive 条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetaDigestEntry {
    /// 被报告节点（指纹——NodeID 公钥）。
    pub id: NodeId,
    /// 最新观测地址。`None` = 报告不携带地址——自广播节点未配置 advertise 时
    /// 仅告 id/alive/last_seen（[`NodeMetaStore::digest_with_self`]）；合并侧
    /// 等同"仅回环报告"：不新建条目、不动 addrs/source。旧版本 digest 恒为
    /// 地址字符串 → 反序列化为 `Some`（线格式向后兼容；新→旧方向缺 addr 字段
    /// 会被旧节点判非法帧丢弃，可接受）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<SocketAddr>,
    /// 报告方的最近确认存活时刻（unix 秒）。
    pub last_seen: u64,
    /// 报告方视角该节点是否活跃。
    pub alive: bool,
    /// 报告方视角该地址是否通过指纹验证（直连或探测洗白；接收侧透传标记，
    /// 未验证地址不参与 LAN 判定的语义由下游处理）。旧版本 digest 无此
    /// 字段 → 解析为 false（当作未验证）。
    #[serde(default)]
    pub verified: bool,
    /// 该节点是否声明自己可作**网络出口**（network-exit 组件，2026-08-30：
    /// 出口节点 offer 后经自广播带 true，其他节点据此发现可用出口；转述条目
    /// 透传该位）。旧版本 digest 无此字段 → 解析为 false（不当出口）。
    #[serde(default)]
    pub exit_offered: bool,
}

// ============================================================================
// 注册表
// ============================================================================

/// 节点元数据注册表（纯内存结构 + 持久化防抖记账；无 I/O 副作用的读写均不落盘）。
///
/// 并发约定：整体住在 `api::State`（std Mutex 短临界区，持锁不 await），
/// 文件 I/O 由引擎在锁外执行（[`flush_due`]/[`flush_final`] 只产出 JSON 串）。
pub struct NodeMetaStore {
    /// 持久化文件（None = 纯内存——测试用）。
    path: Option<PathBuf>,
    entries: HashMap<NodeId, NodeMetaEntry>,
    /// 脏标记（有变更待落盘）。
    dirty: bool,
    /// 最近一次落盘时刻（防抖计时）。
    last_flush: Instant,
    /// 本节点是否声明可作网络出口（network-exit）：digest 自广播首条携带
    /// （`P2pConfig::exit_offered` 初始 / `Handle::set_exit_offered` 运行期
    /// 切换）。**不持久化**——权威源在 network-exit 组件的状态文件，本层只
    /// 负责随 gossip 广播。
    self_exit: bool,
}

impl NodeMetaStore {
    /// 空注册表；`path` 为 Some 时尝试加载（失败告警并重建空表）。
    /// `self_exit` = 本节点出口声明初始值（P2pConfig::exit_offered——env
    /// `NEXOS_P2P_EXIT_OFFER=1`；运行期经 [`NodeMetaStore::set_self_exit`]）。
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self::with_exit_offer(path, false)
    }

    /// [`NodeMetaStore::new`] 的带出口声明形态（api.rs 装配用）。
    #[must_use]
    pub fn with_exit_offer(path: Option<PathBuf>, exit_offered: bool) -> Self {
        let mut store = Self {
            path,
            entries: HashMap::new(),
            dirty: false,
            last_flush: Instant::now(),
            self_exit: exit_offered,
        };
        store.load_from_disk();
        store
    }

    /// 切换本节点出口声明（network-exit 组件 `POST /net-exit/offer` 的落点）：
    /// 下一轮 gossip（≤6 tick）自广播即带新值，全网 1-2 轮内感知。
    pub fn set_self_exit(&mut self, offered: bool) {
        if self.self_exit != offered {
            self.self_exit = offered;
        }
    }

    /// 本节点当前出口声明（观察面）。
    #[must_use]
    pub fn self_exit(&self) -> bool {
        self.self_exit
    }

    /// 持久化文件路径（引擎落盘用）。
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 强制置脏（落盘失败后重试用）。
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 加载持久化文件：合法 JSON 数组 → 还原；缺失 → 空表；损坏 → 告警重建
    /// （置脏——下次落盘覆盖坏文件）。加载时**无条件剔除回环存量**（2026-08-25
    /// 定调，取代 a8515e9 的「仅 gossip 来源 / 仅 Inactive」条件）：条目内的
    /// 回环地址逐条剥离（active + direct 的也剔），剥离后仅剩空地址的条目
    /// （原本仅回环）整条丢弃——置脏，下次落盘把剔除结果写回文件。
    fn load_from_disk(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Vec<NodeMetaEntry>>(&content) {
                Ok(list) => {
                    let mut dropped_entries = 0usize;
                    let mut stripped_addrs = 0usize;
                    let list: Vec<NodeMetaEntry> = list
                        .into_iter()
                        .filter_map(|mut e| {
                            let before = e.addrs.len();
                            e.addrs.retain(|ma| !is_loopback(ma.addr));
                            stripped_addrs += before - e.addrs.len();
                            if before > 0 && e.addrs.is_empty() {
                                dropped_entries += 1;
                                None
                            } else {
                                Some(e)
                            }
                        })
                        .collect();
                    if dropped_entries > 0 || stripped_addrs > 0 {
                        tracing::info!(
                            meta_file = %path.display(),
                            dropped_entries,
                            stripped_addrs,
                            "加载时剔除回环存量（仅回环条目整条丢弃，混合条目剥回环地址；下次落盘覆盖）"
                        );
                        self.dirty = true; // 下次落盘把剔除结果写回文件
                    }
                    let count = list.len();
                    self.entries = list.into_iter().map(|e| (e.id.clone(), e)).collect();
                    tracing::info!(
                        meta_file = %path.display(),
                        count,
                        "加载节点元数据注册表（重启不丢）"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        meta_file = %path.display(),
                        error = %e,
                        "节点元数据文件损坏——重建空注册表（旧文件将在下次落盘时覆盖）"
                    );
                    self.entries.clear();
                    self.dirty = true;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    meta_file = %path.display(),
                    error = %e,
                    "节点元数据文件不可读——以空注册表启动"
                );
            }
        }
    }

    /// 连接记账（register_conn 成功后调用）：first_seen 建档 / last_seen+addrs
    /// 更新 / Inactive 复活为 Active（连接成功是最强活性证据）。直连经握手
    /// 天然完成指纹验证 → 条目与地址均 verified=true。
    ///
    /// **回环不入册**（2026-08-25 用户定调：「127.0.0.1 无论怎么产生的，都
    /// 应该屏蔽」）：观测地址是回环 → 不新建条目、不往既有条目 addrs 里加
    /// （同机多实例的观测由 `register_conn` 的 identity_conflicts 记账承担，
    /// 注册表不需要回环条目——早前「直连回环照记」的不对称语义已废弃）。
    /// 若该握手是**既有条目**（经 gossip 学到公网地址）的新连接，活性照常
    /// 维护：last_seen 续期 / source 翻 Direct / Inactive 复活——连接仍是
    /// 第一手活性与指纹证据，只是回环地址本身不入册。
    pub fn record_conn(&mut self, id: &NodeId, addr: SocketAddr, now: u64) {
        if is_loopback(addr) {
            if let Some(entry) = self.entries.get_mut(id) {
                entry.last_seen = now;
                entry.source = MetaSource::Direct;
                entry.verified = true;
                if let MetaState::Inactive { .. } = entry.state {
                    entry.state = MetaState::Active {
                        score: META_REVIVE_SCORE,
                        consec_fail: 0,
                    };
                }
                self.dirty = true;
            }
            return;
        }
        let entry = self
            .entries
            .entry(id.clone())
            .or_insert_with(|| NodeMetaEntry {
                id: id.clone(),
                addrs: Vec::new(),
                first_seen: now,
                last_seen: now,
                state: MetaState::Active {
                    score: META_SCORE_START,
                    consec_fail: 0,
                },
                source: MetaSource::Direct,
                verified: true,
                exit_offered: false,
            });
        // 直连不产生出口声明知识（对端是否 offer 由其自广播/转述决定）——
        // 既有条目的 exit_offered 位保持。
        push_meta_addr(&mut entry.addrs, addr, true);
        entry.last_seen = now;
        entry.source = MetaSource::Direct;
        entry.verified = true;
        if let MetaState::Inactive { .. } = entry.state {
            entry.state = MetaState::Active {
                score: META_REVIVE_SCORE,
                consec_fail: 0,
            };
        }
        self.dirty = true;
    }

    /// 心跳成功（活连接或指纹验证探测通过）：+5（封顶 100）、连续失败清零、
    /// last_seen 续期、条目 verified=true（两种成功路径都以握手签名为凭）；
    /// `verified_addr` = 探测验证通过的具体地址（活连接路径为 None——其
    /// observed 地址在 record_conn 时已标记）。Inactive 条目直接忽略。
    fn heartbeat_success(&mut self, id: &NodeId, verified_addr: Option<SocketAddr>, now: u64) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        let MetaState::Active { score, consec_fail } = &mut entry.state else {
            return;
        };
        *score = (*score)
            .saturating_add(META_SCORE_SUCCESS_STEP)
            .min(META_SCORE_MAX);
        *consec_fail = 0;
        entry.last_seen = now;
        entry.verified = true;
        if let Some(addr) = verified_addr {
            if let Some(ma) = entry.addrs.iter_mut().find(|ma| ma.addr == addr) {
                ma.verified = true;
            }
        }
        self.dirty = true;
    }

    /// 心跳失败：-20（下限 0）、连续失败 +1；到 [`META_MAX_CONSEC_FAIL`] 移入
    /// Inactive（记 since）。verified 不动（不可达 ≠ 谎报——指纹结论未知）。
    /// 返回是否**刚刚出局**（日志用）。
    fn heartbeat_failure(&mut self, id: &NodeId, now: u64) -> bool {
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        let MetaState::Active { score, consec_fail } = &mut entry.state else {
            return false;
        };
        *score = score.saturating_sub(META_SCORE_FAIL_STEP);
        *consec_fail = consec_fail.saturating_add(1);
        self.dirty = true;
        if *consec_fail >= META_MAX_CONSEC_FAIL {
            entry.state = MetaState::Inactive { since: now };
            true
        } else {
            false
        }
    }

    /// 指纹不匹配（地址背后是**别的节点**——gossip 谎报/陈旧观测被探测实锤）：
    /// 记一次心跳失败（同 [`Self::heartbeat_failure`] 的分数与五振规则）+
    /// 条目 verified 置回 false + 该地址撤销验证标记（它已确证不属于此节点，
    /// 不得再经 digest 以验证地址身份外传）。返回是否**刚刚出局**（日志用）。
    fn heartbeat_mismatch(&mut self, id: &NodeId, addr: Option<SocketAddr>, now: u64) -> bool {
        let struck = self.heartbeat_failure(id, now);
        if let Some(entry) = self.entries.get_mut(id) {
            entry.verified = false;
            if let Some(addr) = addr {
                if let Some(ma) = entry.addrs.iter_mut().find(|ma| ma.addr == addr) {
                    ma.verified = false;
                }
            }
            self.dirty = true;
        }
        struck
    }

    /// 单轮心跳扫描：按分数节奏挑出本轮应探测的无连接节点（返回待探测清单），
    /// 有活连接的节点当场视为心跳成功（连接即活性证据，不额外探测），
    /// Inactive 完全跳过。`counter` = 引擎 tick 计数（从 1 起）。
    fn sweep_due(
        &mut self,
        counter: u64,
        live_conn: impl Fn(&NodeId) -> bool,
        now: u64,
    ) -> Vec<(NodeId, Vec<SocketAddr>)> {
        enum Decision {
            Skip,
            Success,
            Probe(Vec<SocketAddr>),
        }
        let mut probes = Vec::new();
        for id in self.entries.keys().cloned().collect::<Vec<_>>() {
            let decision = {
                let Some(entry) = self.entries.get(&id) else {
                    continue;
                };
                match entry.state {
                    MetaState::Inactive { .. } => Decision::Skip,
                    MetaState::Active { score, .. } => {
                        if live_conn(&id) {
                            Decision::Success
                        } else if counter % probe_period(score) == 0 {
                            Decision::Probe(entry.addrs.iter().map(|ma| ma.addr).collect())
                        } else {
                            Decision::Skip
                        }
                    }
                }
            };
            match decision {
                Decision::Skip => {}
                Decision::Success => self.heartbeat_success(&id, None, now),
                Decision::Probe(addrs) => probes.push((id, addrs)),
            }
        }
        probes
    }

    /// 手动复活（`Handle::meta_reactivate`）：Inactive → Active{score:30}
    /// （Active 保持不变——同样允许立即探测）。返回探测地址表
    /// （None = 未知节点）。
    fn reactivate(&mut self, id: &NodeId, now: u64) -> Option<Vec<SocketAddr>> {
        let entry = self.entries.get_mut(id)?;
        if let MetaState::Inactive { .. } = entry.state {
            entry.state = MetaState::Active {
                score: META_REVIVE_SCORE,
                consec_fail: 0,
            };
            entry.last_seen = now;
            self.dirty = true;
        }
        Some(entry.addrs.iter().map(|ma| ma.addr).collect())
    }

    /// TTL 清除（2026-09-02）：状态为 Inactive 且 `last_seen` 距 `now` **超过**
    /// `ttl_secs` 的条目整条删除（返回被清条目原样快照——日志用；空 = 无可清，
    /// 不置脏）。内存删除 + 置脏 → 防抖落盘把 node-meta.json 同步重写（重启后
    /// 不再出现）。
    ///
    /// - **只清 Inactive**：Active 条目永不过期——哪怕是远古僵尸（评分/五振
    ///   机制自会处理：探败出局后就进了本规则的管辖），漏判好过误杀；
    /// - 复活无需特判：三条路（直连 `record_conn` / 手动 `reactivate` / 他节点
    ///   新鲜报告 `merge_digest`）都刷新 last_seen，复活即自然续命；
    /// - `ttl_secs = 0` 禁用（恒返回空——调用方已挡，此处双保险）。
    fn purge_expired_inactive(&mut self, now: u64, ttl_secs: u64) -> Vec<NodeMetaEntry> {
        if ttl_secs == 0 {
            return Vec::new();
        }
        let expired: Vec<NodeId> = self
            .entries
            .values()
            .filter(|e| matches!(e.state, MetaState::Inactive { .. }))
            .filter(|e| now.saturating_sub(e.last_seen) > ttl_secs)
            .map(|e| e.id.clone())
            .collect();
        let mut purged = Vec::with_capacity(expired.len());
        for id in expired {
            if let Some(entry) = self.entries.remove(&id) {
                purged.push(entry);
            }
        }
        if !purged.is_empty() {
            self.dirty = true; // 持久化重写同步（删除也是脏——不是只增不改）
        }
        purged
    }

    /// 合并他节点的交互摘要。`fresh` = 复活新鲜度线（两个交互周期，即
    /// `2 × meta_tick × 6`）。规则：
    ///
    /// - 本地未知节点 → 新建条目（Gossip / Active / score 50 / **verified=false**
    ///   ——指纹靠本机心跳探测洗白，转述不作数）；
    /// - 本地 Active 且远端 last_seen 更新 → 更新本地 last_seen/addrs（Gossip；
    ///   **verified 保持原值**，地址按远端报告位透传——远端 verified=false 的
    ///   地址合入时不带验证标记，不污染本地已验证地址）；
    /// - 本地 Inactive 且远端**报告活着**（alive=true）且 last_seen 距 now 在
    ///   新鲜度线内 → **复活**为 Active{score:30}（陈旧报告不复活）。
    ///
    /// **回环入口过滤**：远端报告的回环地址（对方机器的 127.0.0.1——本机拨
    /// 不通）不入本地 addrs。仅含回环地址的报告：本地无该条目则**不新建**
    /// （无可用地址的档案探不了活，纯噪声）；已有条目只更新 last_seen（复活
    /// 照常——活性信号与地址信号分离），addrs 与 source 均不动（未获得任何
    /// 地址知识，不降级既有 Direct 来源结论）。**无地址报告**（`addr=None`，
    /// 对端自广播未配置 advertise）走同一路径——只有活性与新鲜度信号。
    ///
    /// 返回复活条数（日志用）。**自身条目跳过不落库**（`id == self_id`，含
    /// 对端把我们的自广播转述回来的回声）：自己的活性自己最清楚；同私钥多
    /// 实例场景本机已有 `identity_conflicts` 观测面，注册表不收自己。
    pub fn merge_digest(
        &mut self,
        self_id: &NodeId,
        remote: &[MetaDigestEntry],
        now: u64,
        fresh: Duration,
    ) -> usize {
        let fresh_bound = fresh.as_secs().max(1);
        let mut revived = 0usize;
        for r in remote {
            // 自身条目（含自广播回声）跳过：注册表不收录本机
            if r.id == *self_id {
                continue;
            }
            // 入口过滤：None = 这条报告不携带任何本机可拨的地址（回环被剔 /
            // 对端自广播未配置 advertise）
            let usable = r.addr.filter(|a| !is_loopback(*a));
            match self.entries.get_mut(&r.id) {
                // 未知节点：仅回环地址的报告不建档；正常报告新建
                // （Gossip / Active / score 50 / verified=false）
                None => {
                    let Some(addr) = usable else {
                        continue;
                    };
                    self.entries.insert(
                        r.id.clone(),
                        NodeMetaEntry {
                            id: r.id.clone(),
                            addrs: vec![MetaAddr {
                                addr,
                                verified: r.verified,
                            }],
                            first_seen: now,
                            last_seen: r.last_seen,
                            state: MetaState::Active {
                                score: META_SCORE_START,
                                consec_fail: 0,
                            },
                            source: MetaSource::Gossip,
                            verified: false,
                            exit_offered: r.exit_offered,
                        },
                    );
                    self.dirty = true;
                }
                Some(entry) => match entry.state {
                    MetaState::Inactive { .. } => {
                        // 复活仅认"报告活着 + last_seen 新鲜"（两个交互周期内）
                        if r.alive && now.saturating_sub(r.last_seen) <= fresh_bound {
                            entry.state = MetaState::Active {
                                score: META_REVIVE_SCORE,
                                consec_fail: 0,
                            };
                            entry.last_seen = r.last_seen;
                            // 可用地址入史；回环地址只采纳新鲜度与复活，不动
                            // addrs/source
                            if let Some(addr) = usable {
                                push_meta_addr(&mut entry.addrs, addr, r.verified);
                                entry.source = MetaSource::Gossip;
                            }
                            entry.exit_offered = r.exit_offered;
                            self.dirty = true;
                            revived += 1;
                        }
                        // 陈旧报告：保持 Inactive（不复活）
                    }
                    MetaState::Active { .. } => {
                        // 出口声明位：报告不旧于本地即采纳（>=——活连接的本地
                        // last_seen 被心跳持续顶到当前秒，同秒自广播用 > 会永远
                        // 不生效；该位非地址知识，属声明转发，等秒即新鲜）。
                        if r.last_seen >= entry.last_seen {
                            entry.exit_offered = r.exit_offered;
                            self.dirty = true;
                        }
                        // 远端更新鲜 → 更新 last_seen/addrs（Gossip 转述；地址
                        // verified 位透传——远端未验证地址入史但不带验证标记）
                        if r.last_seen > entry.last_seen {
                            entry.last_seen = r.last_seen;
                            // 可用地址入史；回环地址只采纳新鲜度，不动 addrs/source
                            if let Some(addr) = usable {
                                push_meta_addr(&mut entry.addrs, addr, r.verified);
                                entry.source = MetaSource::Gossip;
                            }
                            self.dirty = true;
                        }
                    }
                },
            }
        }
        revived
    }

    /// 交互摘要（只带 last_seen 在 [`META_DIGEST_FRESH_SECS`] 内的条目——压缩，
    /// 死档案不广播；Inactive 也带，alive=false）。指纹 = 条目 NodeID；地址
    /// 的 verified 位随所选观测地址透传（对端合并时只作标记，不作凭据）。
    ///
    /// **回环出口过滤**（纵深防御——源头已不记回环，正常情况下此处恒无回环
    /// 可剔）：条目地址里的回环地址（127.0.0.0/8 / ::1）一律剔除——对端机器
    /// 拨不通本机的 127.0.0.1，广播纯属噪声（回环观测地址曾被扩散到全网、
    /// 出现在节点发现页）。摘要取**首个非回环**观测地址（地址历史最新在前）；
    /// 只剩回环地址的条目整条不发。
    fn digest(&self, now: u64) -> Vec<MetaDigestEntry> {
        self.entries
            .values()
            .filter(|e| now.saturating_sub(e.last_seen) <= META_DIGEST_FRESH_SECS)
            .filter_map(|e| {
                e.addrs
                    .iter()
                    .find(|ma| !is_loopback(ma.addr))
                    .map(|ma| MetaDigestEntry {
                        id: e.id.clone(),
                        addr: Some(ma.addr),
                        last_seen: e.last_seen,
                        alive: matches!(e.state, MetaState::Active { .. }),
                        verified: ma.verified,
                        exit_offered: e.exit_offered,
                    })
            })
            .collect()
    }

    /// 广播摘要组装（[`gossip_broadcast`] 用）：**首条固定为自身**（自广播），
    /// 其后为注册表转述（[`Self::digest`] 规则不变——注册表不收录本机，首条
    /// 只能由这里补上）。空注册表也广播——只有自广播一条。
    fn digest_with_self(
        &self,
        self_id: &NodeId,
        advertise: Option<SocketAddr>,
        now: u64,
    ) -> Vec<MetaDigestEntry> {
        let mut list = Vec::with_capacity(self.entries.len() + 1);
        list.push(self_announcement(self_id, advertise, self.self_exit, now));
        list.extend(self.digest(now));
        list
    }

    /// 观察面快照：Active 按分数降序在前（同分按 last_seen 新者在前），
    /// Inactive 殿后（按出局时刻降序，再按 last_seen）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<NodeMetaEntry> {
        let mut list: Vec<NodeMetaEntry> = self.entries.values().cloned().collect();
        // 排名：段位 Active(0) 在前 / Inactive(1) 殿后；段内主键（健康分或出局
        // 时刻）降序——心跳一直正常的靠前、掉过线的靠后；再以 last_seen 新者
        // 在前稳定次序
        let seg = |e: &NodeMetaEntry| match e.state {
            MetaState::Active { .. } => 0u8,
            MetaState::Inactive { .. } => 1u8,
        };
        let key = |e: &NodeMetaEntry| match e.state {
            MetaState::Active { score, .. } => u64::from(score),
            MetaState::Inactive { since } => since,
        };
        list.sort_by(|a, b| {
            seg(a)
                .cmp(&seg(b))
                .then_with(|| key(b).cmp(&key(a)))
                .then_with(|| b.last_seen.cmp(&a.last_seen))
        });
        list
    }

    /// 防抖到期且有脏数据 → 产出待写 JSON（并清脏 / 重置计时）。锁内调用，
    /// 仅序列化不做 I/O。
    fn flush_due(&mut self, now: Instant) -> Option<String> {
        if !self.dirty || now.duration_since(self.last_flush) < META_FLUSH_DEBOUNCE {
            return None;
        }
        self.take_snapshot_json()
    }

    /// 停机强制刷盘：脏即产出（无视防抖间隔）。锁内调用，仅序列化不做 I/O。
    fn flush_final(&mut self) -> Option<String> {
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
}

/// 自广播条目（digest 首条固定为自身）：上线即广播自身活性——"我是谁、我在
/// 哪、活着"。重装系统换新 NodeID 后无需先被对端直连观测到，经一次 gossip 即被
/// 全网感知（旧身份条目则由指纹探测天然判不可信，无需额外修复）。
/// `advertise=None` 时跳过 addr——id/alive/last_seen 仍带（活性信号独立于地址
/// 信号）。verified=true：本机对自身活性的第一手结论。`exit_offered` = 本机
/// 出口声明（network-exit：`NEXOS_P2P_EXIT_OFFER=1` 或运行期 offer 端点）——
/// 全网凭这一位发现可用出口。
fn self_announcement(
    self_id: &NodeId,
    advertise: Option<SocketAddr>,
    exit_offered: bool,
    now: u64,
) -> MetaDigestEntry {
    MetaDigestEntry {
        id: self_id.clone(),
        addr: advertise,
        last_seen: now,
        alive: true,
        verified: true,
        exit_offered,
    }
}

/// 地址是否回环（127.0.0.0/8 / ::1）。回环地址**只在本机可拨**，对注册表
/// 没有任何凭据价值（对端机器的 127.0.0.1 不是我的 127.0.0.1；本机直连的
/// 回环也只是同机多实例，观测由 identity_conflicts 承担）——一律不入册
/// （[`NodeMetaStore::record_conn`] 跳过 / [`push_meta_addr`] 拒收 / merge
/// 拒收 / 加载剔除 / digest 出口剔除，层层纵深防御）。
fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// 地址历史写入：去重 + 最新在前 + 上限 [`META_ADDRS_CAP`]。重复观测不降级
/// 验证标记（本机验证过的地址不会因一条 gossip 转述翻回未验证；新观测
/// verified=true 则升级）。
///
/// **回环拒绝**（入口兜底）：无论调用方是谁（record_conn / merge /
/// 指纹探测路径 / 未来新增来源），回环地址一律不入地址历史——2026-08-25
/// 定调「127.0.0.1 无论怎么产生的，都应该屏蔽」的唯一收口点。
fn push_meta_addr(addrs: &mut Vec<MetaAddr>, addr: SocketAddr, verified: bool) {
    if is_loopback(addr) {
        return;
    }
    let prior = addrs
        .iter()
        .position(|ma| ma.addr == addr)
        .map(|i| addrs.remove(i));
    let merged = match prior {
        Some(ma) => MetaAddr {
            addr,
            verified: ma.verified || verified,
        },
        None => MetaAddr { addr, verified },
    };
    addrs.insert(0, merged);
    addrs.truncate(META_ADDRS_CAP);
}

/// 探测节奏（分数高 = 一直健康 = 探得稀）：≥80 每 6 tick / 50-79 每 3 / <50 每 tick。
fn probe_period(score: u8) -> u64 {
    if score >= 80 {
        6
    } else if score >= 50 {
        3
    } else {
        1
    }
}

// ============================================================================
// 引擎
// ============================================================================

/// 元数据引擎（tracked task，随 shutdown 停止）：每 tick 心跳扫描 + 每 6 tick
/// 交互广播 + 每 [`META_PURGE_EVERY_TICKS`] tick 非活跃 TTL 清除（节流）+
/// 防抖落盘。探测经 spawn 的子任务执行（TCP connect + 握手可能耗满超时，
/// 不阻塞引擎节奏）。
pub(crate) async fn meta_engine(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(shared.timing.meta_tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shutdown_rx = shared.shutdown_watch();
    let ttl_secs = inactive_ttl_secs_from_env();
    // 启动加载后即时机扫一次：node-meta.json 里的存量超期条目（如 aliyun
    // 重装换代的旧 NodeID，半个月不心跳）上线即清，不等首个节流窗口（25 分钟）。
    purge_tick(&shared, ttl_secs);
    let mut counter: u64 = 0;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            _ = tick.tick() => {
                counter = counter.wrapping_add(1);
                heartbeat_tick(&shared, counter);
                if purge_due(counter) {
                    purge_tick(&shared, ttl_secs);
                }
                if counter % META_GOSSIP_EVERY_TICKS == 0 {
                    gossip_broadcast(&shared);
                }
                maybe_flush(&shared);
            }
        }
    }
}

/// TTL 清除扫描是否在本 tick 到期（节流判定：counter 为
/// [`META_PURGE_EVERY_TICKS`] 的倍数——引擎 counter 从 1 起，首个周期性窗口
/// 在第 300 tick；启动即时机扫描不经过此处）。
fn purge_due(counter: u64) -> bool {
    counter % META_PURGE_EVERY_TICKS == 0
}

/// TTL 清除扫描（启动即时机 + 引擎节流调用）：删 Inactive 且 last_seen 超期
/// 的条目（删除置脏 → 防抖落盘重写 node-meta.json）。逐条 eprintln 记被清
/// NodeID 与非活跃时长——eprintln 而非 tracing：os-api 网关进程不装 tracing
/// subscriber，tracing 在 journald 里无声（[os-p2p] 入站审计同款考量）。
fn purge_tick(shared: &Arc<Shared>, ttl_secs: u64) {
    if ttl_secs == 0 {
        return; // 禁用（env 显式 0——向后兼容开关）
    }
    let now = unix_now();
    let purged = {
        let mut st = shared.state.lock().expect("state poisoned");
        st.meta.purge_expired_inactive(now, ttl_secs)
    };
    for e in purged {
        // 非活跃时长 = 距出局时刻；未存活时长 = 距最后一次确认存活（清判据）
        let inactive_secs = match e.state {
            MetaState::Inactive { since } => now.saturating_sub(since),
            MetaState::Active { .. } => 0,
        };
        let unseen_secs = now.saturating_sub(e.last_seen);
        eprintln!(
            "[os-p2p][meta] 清除非活跃超期条目（{ttl_secs}s 无心跳 TTL）node={} inactive={inactive_secs}s unseen={unseen_secs}s",
            crate::short_hex(&e.id.to_hex()),
        );
    }
}

/// TTL 配置读取（env [`ENV_INACTIVE_TTL_SECS`]，引擎启动一次）：未设置 = 默认
/// 3 天；`0` = 禁用清除；非法值告警回落默认（既不静默吞配置错误，也不因笔误
/// 关掉清除）。
fn inactive_ttl_secs_from_env() -> u64 {
    let raw = std::env::var(ENV_INACTIVE_TTL_SECS).ok();
    parse_inactive_ttl(raw.as_deref())
}

/// [`inactive_ttl_secs_from_env`] 的纯解析（测试用——env 全局可变态不进单测）。
fn parse_inactive_ttl(raw: Option<&str>) -> u64 {
    let Some(raw) = raw else {
        return META_INACTIVE_TTL_DEFAULT_SECS;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return META_INACTIVE_TTL_DEFAULT_SECS;
    }
    match trimmed.parse::<u64>() {
        Ok(secs) => secs,
        Err(_) => {
            tracing::warn!(
                env = ENV_INACTIVE_TTL_SECS,
                raw,
                "非活跃 TTL 配置非法（应为秒数，0=禁用）——回落默认 3 天"
            );
            META_INACTIVE_TTL_DEFAULT_SECS
        }
    }
}

/// 单轮心跳：活连接当场记成功（连接即指纹验证过的活性证据）；无连接且到期
/// 的节点交给子任务做指纹验证探测（握手可耗满超时，不阻塞引擎节奏）。
fn heartbeat_tick(shared: &Arc<Shared>, counter: u64) {
    let probes: Vec<(NodeId, Vec<SocketAddr>)> = {
        let mut guard = shared.state.lock().expect("state poisoned");
        // 显式类型标注 &mut State——字段级拆分借用（meta 可变 / conns 只读）
        let st: &mut crate::api::State = &mut guard;
        let (meta, conns) = (&mut st.meta, &st.conns);
        meta.sweep_due(
            counter,
            |id| conns.get(id).is_some_and(|c| !c.is_closed()),
            unix_now(),
        )
    };
    for (id, addrs) in probes {
        let worker = shared.clone();
        crate::api::spawn_tracked(shared, async move {
            let outcome = fingerprint_probe(&worker, &id, &addrs).await;
            record_probe_result(&worker, &id, outcome);
        });
    }
}

/// 探测结果入库（指纹匹配 +5 且 verified=true / 不匹配或不可达 -20；五振出局
/// 记日志）。
///
/// 身份事实同步 os-identity 账本（2026-08-25 组件抽离）：Verified → 该地址
/// 升 verified（`ProbeVerified` 证据）；Mismatch → 期望身份记失配事件 + 地址
/// 改判到实际身份名下（`ProbeMismatch` 证据——探测完成了真实握手，地址换人
/// 被实证）；Unreachable 无身份结论（活性未知 ≠ 谎报）。锁外写入：identity
/// 锁绝不与 state 锁嵌套。
fn record_probe_result(shared: &Arc<Shared>, id: &NodeId, outcome: ProbeOutcome) {
    let now = unix_now();
    {
        let mut st = shared.state.lock().expect("state poisoned");
        match &outcome {
            ProbeOutcome::Verified(addr) => {
                st.meta.heartbeat_success(id, *addr, now);
            }
            ProbeOutcome::Mismatch { addr, actual } => {
                tracing::warn!(
                    peer = %crate::short_hex(&id.to_hex()),
                    addr = %addr,
                    actual = %crate::short_hex(&actual.to_hex()),
                    "元数据心跳指纹不匹配——地址背后是其他节点（gossip 谎报/陈旧观测），记失败并撤销验证标记"
                );
                if st.meta.heartbeat_mismatch(id, Some(*addr), now) {
                    tracing::info!(
                        peer = %crate::short_hex(&id.to_hex()),
                        "元数据心跳连续 5 次失败——移入非活跃（不再探测；手动复活或他节点报告可恢复）"
                    );
                }
            }
            ProbeOutcome::Unreachable => {
                if st.meta.heartbeat_failure(id, now) {
                    tracing::info!(
                        peer = %crate::short_hex(&id.to_hex()),
                        "元数据心跳连续 5 次失败——移入非活跃（不再探测；手动复活或他节点报告可恢复）"
                    );
                }
            }
        }
    }
    // 身份证据 → os-identity 账本（锁外）
    {
        let mut ledger = shared
            .identity_ledger
            .lock()
            .expect("identity ledger poisoned");
        match outcome {
            ProbeOutcome::Verified(addr) => {
                if let Some(addr) = addr {
                    ledger.record_evidence(
                        &id.to_hex(),
                        addr,
                        os_identity::EvidenceKind::ProbeVerified,
                        now,
                    );
                }
            }
            ProbeOutcome::Mismatch { addr, actual } => {
                ledger.record_evidence(
                    &id.to_hex(),
                    addr,
                    os_identity::EvidenceKind::ProbeMismatch {
                        actual: actual.to_hex(),
                    },
                    now,
                );
            }
            ProbeOutcome::Unreachable => {}
        }
    }
}

/// 指纹验证探测结果。
pub(crate) enum ProbeOutcome {
    /// 指纹匹配（心跳成功）：`Some(addr)` = 探测验证通过的具体地址（活连接
    /// 短路路径无探测地址 → None）。
    Verified(Option<SocketAddr>),
    /// 握手成功但指纹不匹配（地址背后是别的节点——gossip 谎报/陈旧观测）。
    Mismatch { addr: SocketAddr, actual: NodeId },
    /// 全部地址不可达 / 握手失败（活性未知，指纹无从比对）。
    Unreachable,
}

/// 指纹验证探测：对地址逐个「TCP connect → 复用既有握手路径（`dial_socket` +
/// `handshake_stream`，与 `dial_addr` 同款）完成挑战-签名握手 → 比对握手返回
/// 的对端真实 NodeID == 目标条目 NodeID」。任一地址匹配即成功；握手成功但
/// 指纹不匹配继续试其余地址（地址历史里可能有 NAT 重映射前的旧地址），全部
/// 试毕仍无匹配 → Mismatch；连不上/握手失败 → Unreachable。
///
/// **红线**：探测**不 `register_conn`**（本机不产生 P2P 连接/入桶）——握手
/// 完成读完 hello 即 drop `AcceptedConn`（连同两条半边流；`handshake_stream`
/// 已确认无其他状态副作用）。被探测端 accept 侧按普通入站连接走完注册又因
/// EOF 立即关闭——既有入站路径的标准行为，不改变其语义。
async fn fingerprint_probe(shared: &Shared, target: &NodeId, addrs: &[SocketAddr]) -> ProbeOutcome {
    let mut mismatch: Option<(SocketAddr, NodeId)> = None;
    for addr in addrs {
        let Some(stream) = crate::api::dial_socket(shared, *addr, None).await else {
            continue;
        };
        if let Some(accepted) = crate::api::handshake_stream(shared, stream).await {
            let actual = accepted.hello.node_id;
            if actual == *target {
                return ProbeOutcome::Verified(Some(*addr));
            }
            mismatch = mismatch.or(Some((*addr, actual)));
        }
    }
    match mismatch {
        Some((addr, actual)) => ProbeOutcome::Mismatch { addr, actual },
        None => ProbeOutcome::Unreachable,
    }
}

/// 元数据交互：把注册表摘要广播给所有已连节点（每连接一帧，dst = 对端）。
/// 摘要**首条固定为自身**（自广播——上线即广播自身活性，新身份快速被全网
/// 感知，见 [`self_announcement`]）。压缩：只带 24h 内条目；单帧编码超
/// [`META_DIGEST_MAX_BYTES`] 截断最旧并记日志（自广播不参与截断——恒在首条）。
fn gossip_broadcast(shared: &Arc<Shared>) {
    let now = unix_now();
    let (mut digest, conns): (Vec<MetaDigestEntry>, Vec<Arc<Conn>>) = {
        let st = shared.state.lock().expect("state poisoned");
        (
            st.meta
                .digest_with_self(&shared.self_id, shared.advertise, now),
            st.conns
                .values()
                .filter(|c| !c.is_closed())
                .cloned()
                .collect(),
        )
    };
    // digest 恒非空（首条自广播）——只看对端是否在线
    if conns.is_empty() {
        return;
    }
    // 自广播移出排序/截断范围（首条恒在位）；截断策略：先按均值估算一刀切，
    // 再逐条微调（编码 ≤ 上限为止）——只作用于注册表转述部分
    let self_entry = digest.remove(0);
    digest.sort_by_key(|d| std::cmp::Reverse(d.last_seen));
    let mut encoded = serde_json::to_vec(&digest).unwrap_or_default();
    if encoded.len() > META_DIGEST_MAX_BYTES {
        let per_entry = (encoded.len() / digest.len()).max(1);
        digest.truncate(META_DIGEST_MAX_BYTES / per_entry);
        loop {
            encoded = serde_json::to_vec(&digest).unwrap_or_default();
            if encoded.len() <= META_DIGEST_MAX_BYTES || digest.is_empty() {
                break;
            }
            digest.pop();
        }
        tracing::debug!(
            bytes = encoded.len(),
            count = digest.len(),
            "元数据交互摘要超 64KB 上限，截断最旧条目"
        );
    }
    digest.insert(0, self_entry);
    for conn in conns {
        conn.try_send(Frame::meta_gossip(&shared.self_id, &conn.peer, &digest));
    }
}

/// 手动触发心跳（`Handle::meta_reactivate`）：Inactive → Active{score:30} 后
/// **立即指纹验证探测一次**并返回结果；Active 节点同样允许立即探测；未知
/// 节点 false。
pub(crate) async fn reactivate_probe(shared: &Arc<Shared>, id: &NodeId) -> bool {
    let addrs = {
        let mut st = shared.state.lock().expect("state poisoned");
        st.meta.reactivate(id, unix_now())
    };
    let Some(addrs) = addrs else {
        tracing::debug!(
            peer = %crate::short_hex(&id.to_hex()),
            "meta_reactivate：注册表无此节点"
        );
        return false;
    };
    // 活连接即（指纹验证过的）活性证据；否则指纹验证探测（不建 P2P 连接）
    let live = shared
        .state
        .lock()
        .expect("state poisoned")
        .conns
        .get(id)
        .is_some_and(|c| !c.is_closed());
    let outcome = if live {
        ProbeOutcome::Verified(None)
    } else {
        fingerprint_probe(shared, id, &addrs).await
    };
    let ok = matches!(outcome, ProbeOutcome::Verified(_));
    record_probe_result(shared, id, outcome);
    tracing::info!(
        peer = %crate::short_hex(&id.to_hex()),
        ok,
        "手动触发元数据心跳（meta_reactivate，指纹验证探测）"
    );
    ok
}

/// 防抖落盘（引擎每 tick 检查；间隔未到或无脏数据直接返回）。meta 注册表与
/// os-identity 身份账本共用引擎节奏（各自独立脏标记与防抖计时）。
///
/// 账本检查在 meta 早退**之前**（2026-08-25 审查修复）：原实现把
/// `flush_ledger` 挂在 meta 待写判断之后，meta 无脏数据（或未配落盘路径）时
/// `let Some(..) else { return }` 提前返回——只脏账本不脏 meta 的场景（同
/// NodeID 冲突观测、静态 gossip 转述）账本可能整个运行期不落盘。
fn maybe_flush(shared: &Arc<Shared>) {
    // 账本独立检查（两账本脏标记互不联动）
    flush_ledger(shared, false);
    let pending = {
        let mut st = shared.state.lock().expect("state poisoned");
        let meta = &mut st.meta;
        meta.flush_due(Instant::now())
            .map(|json| (meta.path().map(PathBuf::from), json))
    };
    let Some((Some(path), json)) = pending else {
        return;
    };
    if let Err(e) = write_meta_atomic(&path, &json) {
        tracing::warn!(meta_file = %path.display(), error = %e, "节点元数据防抖落盘失败（下次重试）");
        shared
            .state
            .lock()
            .expect("state poisoned")
            .meta
            .mark_dirty();
    }
}

/// 停机强制刷盘（`P2pNode::shutdown` 调用——脏即写，无视防抖间隔）。
pub(crate) fn flush_now(shared: &Shared) {
    let pending = {
        let mut st = shared.state.lock().expect("state poisoned");
        let meta = &mut st.meta;
        meta.flush_final()
            .map(|json| (meta.path().map(PathBuf::from), json))
    };
    if let Some((Some(path), json)) = pending {
        if let Err(e) = write_meta_atomic(&path, &json) {
            tracing::warn!(meta_file = %path.display(), error = %e, "节点元数据停机落盘失败（下次启动重建）");
            shared
                .state
                .lock()
                .expect("state poisoned")
                .meta
                .mark_dirty();
        }
    }
    flush_ledger(shared, true);
}

/// 身份账本落盘（与 meta 注册表同节奏：防抖 + 停机强刷；I/O 在锁外执行）。
/// meta 无脏数据时也独立检查账本（两账本脏标记互不联动）。`force` = 停机
/// 路径（`flush_final`——脏即写，无视防抖间隔）；否则防抖 `flush_due`。
fn flush_ledger(shared: &Shared, force: bool) {
    let pending = {
        let mut ledger = shared
            .identity_ledger
            .lock()
            .expect("identity ledger poisoned");
        let json = if force {
            ledger.flush_final()
        } else {
            ledger.flush_due(Instant::now())
        };
        json.map(|json| (ledger.path().map(PathBuf::from), json))
    };
    let Some((Some(path), json)) = pending else {
        return;
    };
    if let Err(e) = os_identity::write_atomic(&path, &json) {
        tracing::warn!(ledger_file = %path.display(), error = %e, "身份账本落盘失败（下次重试）");
        shared
            .identity_ledger
            .lock()
            .expect("identity ledger poisoned")
            .mark_dirty();
    }
}

/// 原子写注册表 JSON：同目录临时文件（`<名>.tmp.<pid>`）→ fsync → rename
/// （参考 bootstrap 私钥的写法——中途崩溃不留半截文件）。父目录不存在先创建。
fn write_meta_atomic(path: &Path, json: &str) -> std::io::Result<()> {
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
// 单元测——注册表记账 / 分数与五振出局 / 探测节奏 / 交互合并 / 排序 / 持久化
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;

    fn nid(seed: u8) -> NodeId {
        NodeIdentity::from_seed(&[seed; 32]).node_id()
    }

    fn addr(port: u16) -> SocketAddr {
        format!("203.0.113.{port}:41000").parse().unwrap()
    }

    /// 回环观测地址（模拟经 127.0.0.1 直连看到的对端地址——ephemeral 端口）。
    fn lo(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    /// 已验证地址条目（record_conn / 探测成功路径的观测）。
    fn ma_ok(port: u16) -> MetaAddr {
        MetaAddr::verified(addr(port))
    }

    /// 未验证地址条目（gossip 转述 / 旧格式迁移）。
    fn ma_raw(port: u16) -> MetaAddr {
        MetaAddr::unverified(addr(port))
    }

    fn active_score(store: &NodeMetaStore, id: &NodeId) -> u8 {
        match store
            .snapshot()
            .into_iter()
            .find(|e| e.id == *id)
            .unwrap()
            .state
        {
            MetaState::Active { score, .. } => score,
            MetaState::Inactive { .. } => panic!("条目应处于 Active"),
        }
    }

    fn entry_of(store: &NodeMetaStore, id: &NodeId) -> NodeMetaEntry {
        store.snapshot().into_iter().find(|e| e.id == *id).unwrap()
    }

    fn strike_out(store: &mut NodeMetaStore, id: &NodeId, now: u64) {
        for _ in 0..META_MAX_CONSEC_FAIL {
            store.heartbeat_failure(id, now);
        }
    }

    // 1. record_conn 建档/更新/复活：新连接建 Direct 档（score 50，直连经握手
    //    天然完成指纹验证 → 条目与地址 verified=true）；重复连接续期 last_seen
    //    并把地址提到最前；五振出局后重连复活为 Active{score:30}
    #[test]
    fn record_conn_create_update_revive() {
        let mut store = NodeMetaStore::new(None);
        let a = nid(1);
        store.record_conn(&a, addr(1), 1000);
        let e = entry_of(&store, &a);
        assert_eq!(e.id, a);
        assert_eq!(e.addrs, vec![ma_ok(1)], "直连地址带验证标记");
        assert!(e.verified, "直连建档即指纹验证过");
        assert_eq!((e.first_seen, e.last_seen), (1000, 1000));
        assert_eq!(
            e.state,
            MetaState::Active {
                score: META_SCORE_START,
                consec_fail: 0
            }
        );
        assert_eq!(e.source, MetaSource::Direct);
        // 更新：新地址最新在前、last_seen 续期、first_seen 不变
        store.record_conn(&a, addr(2), 1010);
        let e = entry_of(&store, &a);
        assert_eq!(e.addrs, vec![ma_ok(2), ma_ok(1)], "地址历史最新在前");
        assert_eq!(e.last_seen, 1010);
        assert_eq!(e.first_seen, 1000, "建档时刻不漂移");
        // 复活：出局后重连 → Active{score:30}，地址入史（verified 保持）
        strike_out(&mut store, &a, 1020);
        assert!(matches!(
            entry_of(&store, &a).state,
            MetaState::Inactive { .. }
        ));
        store.record_conn(&a, addr(3), 1030);
        let e = entry_of(&store, &a);
        assert_eq!(
            e.state,
            MetaState::Active {
                score: META_REVIVE_SCORE,
                consec_fail: 0
            },
            "重连复活为 Active{{score:30}}"
        );
        assert_eq!(e.addrs[0], ma_ok(3));
        assert!(e.verified);
        assert_eq!(e.last_seen, 1030);
    }

    // 2. 地址历史：去重（重观测提前）+ 上限 8（最旧挤出）+ 验证标记不降级
    #[test]
    fn addrs_dedup_and_cap() {
        let mut addrs = Vec::new();
        for p in 0..10u16 {
            push_meta_addr(&mut addrs, addr(p), false);
        }
        assert_eq!(addrs.len(), META_ADDRS_CAP, "上限 8");
        assert_eq!(addrs[0].addr, addr(9), "最新在前");
        assert!(!addrs.iter().any(|ma| ma.addr == addr(0)), "最旧被挤出");
        // 重复地址去重置前（NAT 重映射回旧口的场景）
        push_meta_addr(&mut addrs, addr(2), false);
        assert_eq!(addrs.len(), META_ADDRS_CAP);
        assert_eq!(addrs[0].addr, addr(2), "重观测地址提到最前");
        assert_eq!(
            addrs.iter().filter(|ma| ma.addr == addr(2)).count(),
            1,
            "去重"
        );
        // 验证标记：新观测 verified=true 升级既有条目；verified=false 不降级
        push_meta_addr(&mut addrs, addr(2), true);
        assert!(addrs[0].verified, "探测洗白升级既有地址的验证标记");
        push_meta_addr(&mut addrs, addr(2), false);
        assert!(addrs[0].verified, "gossip 转述不降级本机已验证的地址");
    }

    // 3. 分数规则与五振出局：成功 +5 封顶 100；失败 -20 下限 0；连续 5 败移入
    //    Inactive（记 since），此后成功/失败均不再生效（心跳停摆）
    #[test]
    fn score_math_and_five_strikes() {
        let mut store = NodeMetaStore::new(None);
        let a = nid(2);
        store.record_conn(&a, addr(1), 100);
        // 成功 +5：50 → 55 → … 封顶 100
        for _ in 0..30 {
            store.heartbeat_success(&a, None, 200);
        }
        assert_eq!(active_score(&store, &a), META_SCORE_MAX);
        assert_eq!(entry_of(&store, &a).last_seen, 200, "成功续期 last_seen");
        // 失败 -20：100 → 80 → 60 → 40 → 20 → 0（下限）→ 第 5 败出局
        let mut since = 0;
        for i in 0..META_MAX_CONSEC_FAIL {
            since = 300 + u64::from(i);
            store.heartbeat_failure(&a, since);
        }
        assert_eq!(
            entry_of(&store, &a).state,
            MetaState::Inactive { since },
            "五振出局并记出局时刻"
        );
        // Inactive 后成功/失败均忽略（不再心跳；last_seen 保持出局前的值）
        store.heartbeat_success(&a, None, 999);
        store.heartbeat_failure(&a, 999);
        let e = entry_of(&store, &a);
        assert_eq!(e.state, MetaState::Inactive { since });
        assert_eq!(e.last_seen, 200, "Inactive 后 last_seen 不再变动");
        // 成功清零连续失败：2 败 → 1 成 → 再 2 败仍不出局（50-20-20+5=15）
        let b = nid(3);
        store.record_conn(&b, addr(2), 100);
        store.heartbeat_failure(&b, 101);
        store.heartbeat_failure(&b, 102);
        store.heartbeat_success(&b, None, 103);
        assert_eq!(active_score(&store, &b), 15, "加减分按步长累积");
        store.heartbeat_failure(&b, 104);
        store.heartbeat_failure(&b, 105);
        assert!(
            matches!(entry_of(&store, &b).state, MetaState::Active { .. }),
            "成功清零连续失败——不出局"
        );
    }

    // 3b. 指纹验证记账：探测成功洗白 Gossip 条目（verified=true + 命中地址带
    //     标记）；指纹不匹配记一次失败并撤销条目与该地址的验证标记；不可达
    //     失败不动 verified（活性未知 ≠ 谎报）
    #[test]
    fn fingerprint_verified_flag_transitions() {
        let mut store = NodeMetaStore::new(None);
        let a = nid(9);
        // Gossip 新建：verified=false，地址不带标记
        store.merge_digest(
            &nid(90),
            &[MetaDigestEntry {
                id: a.clone(),
                addr: Some(addr(1)),
                last_seen: 100,
                alive: true,
                verified: false,
                exit_offered: false,
            }],
            100,
            Duration::from_secs(12),
        );
        assert!(!entry_of(&store, &a).verified, "Gossip 新建未验证");
        // 探测成功（指纹匹配）：洗白 + 命中地址带标记 + last_seen 续期
        store.heartbeat_success(&a, Some(addr(1)), 110);
        let e = entry_of(&store, &a);
        assert!(e.verified, "指纹验证探测成功洗白条目");
        assert_eq!(e.addrs[0], ma_ok(1), "命中的地址带验证标记");
        assert_eq!(e.last_seen, 110);
        assert_eq!(
            active_score(&store, &a),
            META_SCORE_START + META_SCORE_SUCCESS_STEP
        );
        // 指纹不匹配：记失败（-20）+ 条目/地址撤销标记
        store.heartbeat_mismatch(&a, Some(addr(1)), 120);
        let e = entry_of(&store, &a);
        assert!(!e.verified, "指纹不匹配撤销条目验证标记");
        assert_eq!(e.addrs[0], ma_raw(1), "不匹配地址撤销验证标记");
        assert_eq!(
            active_score(&store, &a),
            META_SCORE_START + META_SCORE_SUCCESS_STEP - META_SCORE_FAIL_STEP,
            "不匹配记一次心跳失败"
        );
        // 不可达失败：分数继续掉但 verified 不再变动（false 保持）
        store.heartbeat_failure(&a, 130);
        assert!(!entry_of(&store, &a).verified, "不可达不改验证结论");
        // 直连观测再次洗白
        store.record_conn(&a, addr(2), 140);
        let e = entry_of(&store, &a);
        assert!(e.verified, "直连重新验证");
        assert_eq!(e.addrs[0], ma_ok(2));
    }

    // 4. 探测节奏（分数分级）+ 活连接视为成功 + Inactive 跳过：
    //    ≥80 每 6 tick / 50-79 每 3 / <50 每 tick；有活连接的节点不入探测表
    //    而是当场 +5；Inactive 完全跳过
    #[test]
    fn sweep_due_cadence_conn_shortcut_and_skip() {
        let mut store = NodeMetaStore::new(None);
        let now = 100u64;
        let hi = nid(4);
        let mid = nid(5);
        let lo = nid(6);
        let dead = nid(7);
        let conned = nid(8);
        store.record_conn(&hi, addr(1), now);
        for _ in 0..12 {
            store.heartbeat_success(&hi, None, now); // 50+5×12 → 封顶 100（≥80 档）
        }
        store.record_conn(&mid, addr(2), now); // 50（50-79 档）
        store.record_conn(&lo, addr(3), now);
        store.heartbeat_failure(&lo, now); // 30（<50 档）
        store.record_conn(&dead, addr(4), now);
        strike_out(&mut store, &dead, now); // Inactive
        store.record_conn(&conned, addr(5), now); // 50，有活连接
                                                  // counter=1：仅 lo（每 tick）到期；conned 走连接路径 +5；mid/hi 未到期
        let due = store.sweep_due(1, |id| *id == conned, now);
        assert_eq!(
            due,
            vec![(lo.clone(), vec![addr(3)])],
            "仅 <50 档每 tick 到期"
        );
        assert_eq!(
            active_score(&store, &conned),
            55,
            "连接存在即心跳成功（+5）"
        );
        // counter=3：mid（3%3==0）与 lo 到期；hi（3%6≠0）不到期
        let due = store.sweep_due(3, |_| false, now);
        let ids: Vec<NodeId> = due.into_iter().map(|(id, _)| id).collect();
        assert!(ids.contains(&mid) && ids.contains(&lo) && !ids.contains(&hi));
        // counter=6：hi（6%6==0）到期（mid/lo 同刻也到期——分数低者探得更密）
        let due = store.sweep_due(6, |_| false, now);
        assert!(
            due.into_iter().any(|(id, _)| id == hi),
            "≥80 档每 6 tick 到期"
        );
        // Inactive 全程不在探测表
        for counter in 1..=12u64 {
            let due = store.sweep_due(counter, |_| false, now);
            assert!(
                !due.into_iter().any(|(id, _)| id == dead),
                "Inactive 不再心跳"
            );
        }
    }

    // 5. 交互合并：未知新建（Gossip/50/verified=false）；远端更新鲜 → 更新
    //    Active 条目（verified 保持原值；地址按远端报告位透传）；新鲜远端
    //    报告复活 Inactive；陈旧报告不复活；alive=false 不复活；自身条目跳过；
    //    远端 unverified 地址不污染本地已验证地址
    #[test]
    fn merge_digest_learning_update_and_revival() {
        let mut store = NodeMetaStore::new(None);
        let self_id = nid(50);
        let gone = nid(10);
        let known = nid(11);
        let stranger = nid(12);
        let dead_report = nid(13);
        let now = 1000u64;
        let window = Duration::from_secs(12); // 新鲜度线（两个交互周期）
        store.record_conn(&gone, addr(1), 900);
        strike_out(&mut store, &gone, 950); // Inactive（last_seen=900）
        store.record_conn(&known, addr(5), 900); // Active（last_seen=900，verified）
        store.record_conn(&dead_report, addr(6), 900);
        strike_out(&mut store, &dead_report, 950);
        // 陈旧报告（last_seen 距 now 超窗）：不复活
        let revived = store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: gone.clone(),
                addr: Some(addr(2)),
                last_seen: now - 60,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        assert_eq!(revived, 0);
        assert!(
            matches!(entry_of(&store, &gone).state, MetaState::Inactive { .. }),
            "陈旧报告不复活"
        );
        // 新鲜报告（距 now 在窗内 + alive）：复活 Active{score:30}，addr 入史
        // （远端 verified 位透传到地址），source=Gossip；条目 verified 保持
        let revived = store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: gone.clone(),
                addr: Some(addr(2)),
                last_seen: now - 5,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        assert_eq!(revived, 1, "新鲜报告复活");
        let e = entry_of(&store, &gone);
        assert_eq!(
            e.state,
            MetaState::Active {
                score: META_REVIVE_SCORE,
                consec_fail: 0
            }
        );
        assert_eq!(e.addrs[0], ma_ok(2), "远端验证位透传到地址");
        assert_eq!(e.source, MetaSource::Gossip);
        assert!(
            e.verified,
            "gossip 复活不改变条目验证结论（原 true——record_conn 验证过）"
        );
        assert_eq!(e.last_seen, now - 5, "远端新鲜度采纳");
        // 未知节点：新建 Active{score:50}（Gossip / verified=false；远端
        // unverified 地址入史但不带标记）
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: stranger.clone(),
                addr: Some(addr(9)),
                last_seen: now - 1,
                alive: true,
                verified: false,
                exit_offered: false,
            }],
            now,
            window,
        );
        let e = entry_of(&store, &stranger);
        assert_eq!(
            e.state,
            MetaState::Active {
                score: META_SCORE_START,
                consec_fail: 0
            }
        );
        assert_eq!(e.source, MetaSource::Gossip);
        assert!(!e.verified, "Gossip 新建不验证");
        assert_eq!(e.addrs[0], ma_raw(9), "远端 unverified 地址不带标记");
        assert_eq!(e.last_seen, now - 1);
        // Active 条目：远端更新鲜才更新（陈旧远端不覆盖）——本条 known 经
        // record_conn 验证过（verified=true），远端 unverified 新地址合入不带
        // 标记，但**不降级**条目结论与既有验证地址
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: known.clone(),
                addr: Some(addr(7)),
                last_seen: now - 2,
                alive: true,
                verified: false,
                exit_offered: false,
            }],
            now,
            window,
        );
        let e = entry_of(&store, &known);
        assert_eq!(e.last_seen, now - 2);
        assert_eq!(e.addrs[0], ma_raw(7), "远端 unverified 地址入史但不带标记");
        assert_eq!(e.addrs[1], ma_ok(5), "本地已验证地址不被污染");
        assert!(e.verified, "gossip 更新不改变条目验证结论（原 true）");
        assert_eq!(e.source, MetaSource::Gossip);
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: known.clone(),
                addr: Some(addr(8)),
                last_seen: 100, // 远比本地陈旧
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        let e = entry_of(&store, &known);
        assert_eq!(e.last_seen, now - 2, "陈旧远端不回灌");
        assert_eq!(e.addrs[0].addr, addr(7), "地址不被陈旧报告覆盖");
        // alive=false 的新鲜报告不复活（仅"报告活着"才恢复心跳）
        let revived = store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: dead_report.clone(),
                addr: Some(addr(6)),
                last_seen: now - 1,
                alive: false,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        assert_eq!(revived, 0);
        assert!(
            matches!(
                entry_of(&store, &dead_report).state,
                MetaState::Inactive { .. }
            ),
            "alive=false 不复活"
        );
        // 自身条目跳过（注册表不收录本机）
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: self_id.clone(),
                addr: Some(addr(99)),
                last_seen: now,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        assert!(store.snapshot().into_iter().all(|e| e.id != self_id));
    }

    // 6. 交互摘要：只带 24h 内条目；Inactive 也带（alive=false）；取最新地址
    //    并透传其验证位（指纹 = 条目 NodeID，见 MetaDigestEntry 文档）
    #[test]
    fn digest_filters_and_alive_flag() {
        let mut store = NodeMetaStore::new(None);
        let live = nid(20);
        let gone = nid(21);
        let ancient = nid(22);
        let gossiped = nid(23);
        store.record_conn(&live, addr(1), 2000);
        store.record_conn(&gone, addr(2), 1990);
        strike_out(&mut store, &gone, 1995); // Inactive（last_seen=1990）
        store.record_conn(&ancient, addr(3), 10); // 远古档案
                                                  // 地址历史：最新观测在首位
        store.record_conn(&live, addr(4), 2000);
        // Gossip 学来的未验证条目：digest 带出 verified=false（对端只作标记）
        store.merge_digest(
            &nid(51),
            &[MetaDigestEntry {
                id: gossiped.clone(),
                addr: Some(addr(9)),
                last_seen: 2000,
                alive: true,
                verified: false,
                exit_offered: false,
            }],
            2000,
            Duration::from_secs(12),
        );
        // now 取 gone 的 24h 边界（age == 窗口 → 仍携带；远古条目超窗排除）
        let now = 1990 + META_DIGEST_FRESH_SECS;
        let digest = store.digest(now);
        assert_eq!(digest.len(), 3, "24h 外的远古条目不广播");
        let l = digest.iter().find(|d| d.id == live).unwrap();
        assert_eq!(l.addr, Some(addr(4)), "摘要取最新地址");
        assert!(l.alive);
        assert!(l.verified, "直连地址的验证位随摘要透传");
        assert_eq!(l.last_seen, 2000);
        let g = digest.iter().find(|d| d.id == gone).unwrap();
        assert!(
            !g.alive,
            "Inactive 也携带（alive=false——让对端知道我们还见过它）"
        );
        let u = digest.iter().find(|d| d.id == gossiped).unwrap();
        assert!(!u.verified, "未验证地址透传 verified=false");
        assert!(!digest.iter().any(|d| d.id == ancient));
    }

    // 7. 排序：Active 按分数降序在前（同分按 last_seen 新者在前），Inactive 殿后
    #[test]
    fn snapshot_orders_active_by_score_then_inactive_last() {
        let mut store = NodeMetaStore::new(None);
        let top = nid(30);
        let high = nid(31);
        let mid = nid(32);
        let low = nid(33);
        let out1 = nid(34);
        let out2 = nid(35);
        store.record_conn(&top, addr(1), 100);
        for _ in 0..10 {
            store.heartbeat_success(&top, None, 100);
        } // 100
        store.record_conn(&high, addr(2), 100); // 50
        for _ in 0..10 {
            store.heartbeat_success(&high, None, 100);
        } // 100（与 top 同分，last_seen 相同 → 次序无妨）
        store.record_conn(&mid, addr(3), 100); // 50
        store.record_conn(&low, addr(4), 100);
        strike_out(&mut store, &low, 100); // Inactive
        store.record_conn(&out1, addr(5), 100);
        strike_out(&mut store, &out1, 100); // Inactive（last_seen=100）
        store.record_conn(&out2, addr(6), 300);
        strike_out(&mut store, &out2, 300); // Inactive（last_seen=300——殿后内部较新在前）
        let list = store.snapshot();
        assert_eq!(list.len(), 6);
        let position = |id: &NodeId| list.iter().position(|e| e.id == *id).unwrap();
        assert!(position(&top) < position(&mid), "高分在前");
        assert!(
            position(&mid) < position(&low),
            "Active 全部在 Inactive 之前"
        );
        assert!(position(&low) > position(&mid));
        // Inactive 殿后且内部按 last_seen 降序
        assert!(position(&out2) < position(&out1) && position(&out2) > position(&mid));
        // Active 段全为 Active（top/high/mid 三条）、Inactive 段全为 Inactive
        for e in &list[..3] {
            assert!(matches!(e.state, MetaState::Active { .. }));
        }
        for e in &list[3..] {
            assert!(matches!(e.state, MetaState::Inactive { .. }));
        }
    }

    // 8. 持久化（store 层）：flush 产出 → 原子写 → 新 store 同路径加载条目保真
    //    （含 verified 与地址级验证位）；防抖（间隔未到不产出）；损坏文件告警
    //    重建空表
    #[test]
    fn store_level_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta.json");
        let _ = std::fs::remove_file(&file);
        let mut store = NodeMetaStore::new(Some(file.clone()));
        assert!(store.snapshot().is_empty(), "无文件 → 空表");
        let a = nid(40);
        store.record_conn(&a, addr(1), 1000);
        store.heartbeat_success(&a, Some(addr(1)), 1010);
        let b = nid(41);
        store.record_conn(&b, addr(2), 1000);
        strike_out(&mut store, &b, 1020);
        // 防抖：刚建档（last_flush=now）→ 未到 10s 不产出
        assert!(
            store.flush_due(Instant::now()).is_none(),
            "防抖间隔内不落盘"
        );
        // 强制刷（停机路径）：产出 JSON + 原子写
        let json = store.flush_final().expect("脏数据应产出");
        write_meta_atomic(&file, &json).expect("临时目录写入必成功");
        assert!(store.flush_final().is_none(), "刷后不再脏");
        assert!(file.exists(), "文件已落盘");
        // "重启"：新 store 同路径 → 条目保真（含 Inactive 状态 / 来源 / 验证位）
        let reloaded = NodeMetaStore::new(Some(file.clone()));
        let list = reloaded.snapshot();
        assert_eq!(list.len(), 2);
        let ea = list.iter().find(|e| e.id == a).unwrap();
        assert_eq!(
            ea.state,
            MetaState::Active {
                score: META_SCORE_START + META_SCORE_SUCCESS_STEP,
                consec_fail: 0
            }
        );
        assert_eq!(ea.addrs, vec![ma_ok(1)], "地址验证位持久化保真");
        assert!(ea.verified, "条目验证位持久化保真");
        assert_eq!((ea.first_seen, ea.last_seen), (1000, 1010));
        assert_eq!(ea.source, MetaSource::Direct);
        let eb = list.iter().find(|e| e.id == b).unwrap();
        assert_eq!(
            eb.state,
            MetaState::Inactive { since: 1020 },
            "出局状态持久化"
        );
        // 损坏文件 → 告警重建空表
        std::fs::write(&file, "{ not json !!!").unwrap();
        let healed = NodeMetaStore::new(Some(file.clone()));
        assert!(healed.snapshot().is_empty(), "损坏文件重建空注册表");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 8b. 旧持久化格式迁移：裸 SocketAddr 数组（commit e75a0e2 时代的
    //     node-meta.json，无 verified 字段）→ 加载为 MetaAddr{verified=false}
    //     且条目 verified=false（本层无从考证旧档，一律交心跳探测重验）
    #[test]
    fn legacy_persisted_bare_addrs_migrate_unverified() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta-legacy.json");
        let a = nid(42);
        let b = nid(43);
        // 手写旧格式：addrs 为裸地址字符串数组，条目无 verified 字段
        let legacy = format!(
            "[{{\"id\":\"{}\",\"addrs\":[\"203.0.113.1:41000\",\"203.0.113.2:41000\"],\
             \"first_seen\":100,\"last_seen\":200,\
             \"state\":{{\"active\":{{\"score\":60,\"consec_fail\":0}}}},\
             \"source\":\"direct\"}},\
             {{\"id\":\"{}\",\"addrs\":[\"203.0.113.3:41000\"],\
             \"first_seen\":100,\"last_seen\":150,\
             \"state\":{{\"inactive\":{{\"since\":999}}}},\
             \"source\":\"gossip\"}}]",
            a.to_hex(),
            b.to_hex(),
        );
        std::fs::write(&file, legacy).unwrap();
        let store = NodeMetaStore::new(Some(file.clone()));
        let list = store.snapshot();
        assert_eq!(list.len(), 2, "旧格式完整加载（条目数不丢）");
        let ea = list.iter().find(|e| e.id == a).unwrap();
        assert!(!ea.verified, "旧格式无 verified 字段 → false");
        assert_eq!(
            ea.addrs,
            vec![ma_raw(1), ma_raw(2)],
            "裸地址数组迁移为未验证 MetaAddr"
        );
        assert_eq!(
            ea.state,
            MetaState::Active {
                score: 60,
                consec_fail: 0
            },
            "状态与分数保真"
        );
        let eb = list.iter().find(|e| e.id == b).unwrap();
        assert!(!eb.verified);
        assert_eq!(eb.addrs, vec![ma_raw(3)]);
        // 新格式写盘后可再读（对象形式 {addr, verified}，向后可读）
        let mut store = store;
        store.mark_dirty();
        let json = store.flush_final().expect("迁移后置脏应产出新格式");
        write_meta_atomic(&file, &json).unwrap();
        let reloaded = NodeMetaStore::new(Some(file.clone()));
        let ea = reloaded.snapshot().into_iter().find(|e| e.id == a).unwrap();
        assert_eq!(ea.addrs, vec![ma_raw(1), ma_raw(2)], "新格式往返保真");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 9. 回环彻底屏蔽（record_conn 侧）：观测地址是回环 → 不建档（快照/
    //    digest 全无此节点）；既有条目的回环连接只维护活性（last_seen 续期），
    //    地址不入册——digest 恒不携带回环地址（出口过滤保留为纵深防御）
    #[test]
    fn record_conn_loopback_skipped_and_digest_clean() {
        let mut store = NodeMetaStore::new(None);
        let local_only = nid(60);
        let mixed = nid(61);
        let now = 3000u64;
        // 未知节点的回环连接：根本不建档（同机多实例的观测由 register_conn 的
        // identity_conflicts 记账承担，注册表不需要回环条目）
        store.record_conn(&local_only, lo(33516), now);
        assert!(
            store.snapshot().into_iter().all(|e| e.id != local_only),
            "回环观测不建档（快照全无此节点）"
        );
        // 既有条目（公网建档）+ 回环连接：活性照常维护，地址不入册
        store.record_conn(&mixed, addr(1), now);
        store.record_conn(&mixed, lo(40000), now + 10);
        let e = entry_of(&store, &mixed);
        assert_eq!(e.addrs, vec![ma_ok(1)], "回环地址不入地址历史");
        assert_eq!(e.last_seen, now + 10, "既有条目的回环连接仍续期 last_seen");
        store.record_conn(&mixed, addr(2), now + 20);
        let digest = store.digest(now + 20);
        let m = digest.iter().find(|d| d.id == mixed).unwrap();
        assert_eq!(m.addr, Some(addr(2)), "摘要取最新地址（历史里本就无回环）");
        assert!(m.verified, "公网地址的验证位照常透传");
        assert!(
            !digest.iter().any(|d| d.addr.is_some_and(is_loopback)),
            "摘要不携带任何回环地址"
        );
        assert!(
            !digest.iter().any(|d| d.id == local_only),
            "未建档的回环节点不出现在摘要"
        );
    }

    // 9b. record_conn 回环跳过细则：gossip 建档的既有条目经回环重连——
    //     last_seen 续期 / source 翻 Direct / verified 置位 / Inactive 复活
    //     （连接仍是最强活性与指纹证据），唯回环地址不入册
    #[test]
    fn record_conn_loopback_keeps_activity_but_not_addr() {
        let mut store = NodeMetaStore::new(None);
        let self_id = nid(66);
        let a = nid(67);
        let now = 1000u64;
        // 经 gossip 学到的既有条目（公网地址 / 未验证）
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: a.clone(),
                addr: Some(addr(5)),
                last_seen: now - 100,
                alive: true,
                verified: false,
                exit_offered: false,
            }],
            now,
            Duration::from_secs(12),
        );
        // 五振出局 → 回环重连：复活为 Active{score:30}，地址不入册
        strike_out(&mut store, &a, now - 50);
        assert!(matches!(
            entry_of(&store, &a).state,
            MetaState::Inactive { .. }
        ));
        store.record_conn(&a, lo(33516), now);
        let e = entry_of(&store, &a);
        assert_eq!(
            e.state,
            MetaState::Active {
                score: META_REVIVE_SCORE,
                consec_fail: 0
            },
            "回环重连照常复活（活性信号独立于地址信号）"
        );
        assert_eq!(e.last_seen, now, "回环重连续期 last_seen");
        assert_eq!(e.source, MetaSource::Direct, "直连观测照常翻来源");
        assert!(e.verified, "握手天然验证照常置位");
        assert_eq!(e.addrs, vec![ma_raw(5)], "既有公网地址保持，回环不入册");
    }

    // 9c. push_meta_addr 回环拒绝（地址历史唯一收口点）：任何来源的回环地址
    //     都不入历史（不新建、不前移、不借道升级验证位）
    #[test]
    fn push_meta_addr_rejects_loopback() {
        let mut addrs = Vec::new();
        push_meta_addr(&mut addrs, lo(33516), true);
        assert!(addrs.is_empty(), "回环地址直接拒绝（不新建条目）");
        push_meta_addr(&mut addrs, addr(1), true);
        push_meta_addr(&mut addrs, lo(40000), true);
        assert_eq!(
            addrs,
            vec![MetaAddr::verified(addr(1))],
            "回环不入历史（既有地址不受影响）"
        );
    }

    // 10. 回环入口过滤（merge_digest）：远端回环地址不入本地 addrs；仅回环
    //     报告不新建未知条目；已知条目只采纳新鲜度（复活照常）不动
    //     addrs/source；非回环地址报告行为不变（对照组）
    #[test]
    fn merge_digest_rejects_remote_loopback() {
        let mut store = NodeMetaStore::new(None);
        let self_id = nid(62);
        let stranger = nid(63);
        let known = nid(64);
        let gone = nid(65);
        let now = 1000u64;
        let window = Duration::from_secs(12);
        store.record_conn(&known, addr(5), 900);
        store.record_conn(&gone, addr(6), 900);
        strike_out(&mut store, &gone, 950); // Inactive（last_seen=900）
                                            // 未知节点 + 仅回环地址的报告：不新建（对方机器的 127.0.0.1 本机拨不通）
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: stranger.clone(),
                addr: Some(lo(33516)),
                last_seen: now - 1,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        assert!(
            store.snapshot().into_iter().all(|e| e.id != stranger),
            "仅回环地址的报告不新建条目（无可用地址的档案纯噪声）"
        );
        // 已知 Active 条目：远端更新鲜但地址是回环 → 只采纳 last_seen，
        // addrs/source 不动
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: known.clone(),
                addr: Some(lo(33517)),
                last_seen: now - 1,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        let e = entry_of(&store, &known);
        assert_eq!(e.last_seen, now - 1, "新鲜度照常采纳");
        assert_eq!(e.addrs, vec![ma_ok(5)], "远端回环地址不入本地 addrs");
        assert_eq!(e.source, MetaSource::Direct, "无地址知识不改来源结论");
        // 已知 Inactive 条目：新鲜 alive 报告复活照常（活性信号独立于地址），
        // 但回环地址不入史
        let revived = store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: gone.clone(),
                addr: Some(lo(33518)),
                last_seen: now - 1,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now,
            window,
        );
        assert_eq!(revived, 1, "回环报告仍可复活");
        let e = entry_of(&store, &gone);
        assert!(matches!(e.state, MetaState::Active { .. }), "复活生效");
        assert_eq!(e.addrs, vec![ma_ok(6)], "复活不带入回环地址");
        // 对照组：非回环地址的报告行为不变（入史 + source=Gossip）
        store.merge_digest(
            &self_id,
            &[MetaDigestEntry {
                id: known.clone(),
                addr: Some(addr(7)),
                last_seen: now,
                alive: true,
                verified: false,
                exit_offered: false,
            }],
            now,
            window,
        );
        let e = entry_of(&store, &known);
        assert_eq!(e.addrs[0], ma_raw(7), "非回环地址照常入史");
        assert_eq!(e.source, MetaSource::Gossip);
    }

    // 11. 加载剔除回环存量（**无条件**——2026-08-25 定调，取代 a8515e9 的
    //     「仅 gossip 来源 / 仅 Inactive」条件）：仅回环条目整条丢弃（active
    //     + direct 也算）；回环+公网混合条目剥离回环地址、保留公网部分；
    //     正常条目原样保留
    #[test]
    fn load_strips_loopback_addrs_unconditionally() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-loopback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta-loopback.json");
        let _ = std::fs::remove_file(&file);
        let dead = nid(70); // gossip + 仅回环 + Active——清理对象
        let direct_active = nid(71); // direct + 仅回环 + Active——同样清理（新语义）
        let keep_gossip = nid(72); // gossip + 公网——正常条目，保留
        let dead_inactive = nid(73); // direct + 仅回环 + Inactive——清理对象
        let mixed = nid(74); // direct + 回环/公网混合——剥回环保留公网
        let mk = |id: NodeId, addrs: Vec<MetaAddr>, source: MetaSource, state: MetaState| {
            NodeMetaEntry {
                id,
                addrs,
                first_seen: 100,
                last_seen: 200,
                state,
                source,
                verified: false,
                exit_offered: false,
            }
        };
        let active = || MetaState::Active {
            score: 50,
            consec_fail: 0,
        };
        let entries = vec![
            mk(
                dead.clone(),
                vec![MetaAddr::verified(lo(33516))],
                MetaSource::Gossip,
                active(),
            ),
            mk(
                direct_active.clone(),
                vec![MetaAddr::verified(lo(40000))],
                MetaSource::Direct,
                active(),
            ),
            mk(
                keep_gossip.clone(),
                vec![MetaAddr::unverified(addr(9))],
                MetaSource::Gossip,
                active(),
            ),
            mk(
                dead_inactive.clone(),
                vec![MetaAddr::verified(lo(50000))],
                MetaSource::Direct,
                MetaState::Inactive { since: 300 },
            ),
            mk(
                mixed.clone(),
                vec![MetaAddr::verified(lo(33517)), MetaAddr::verified(addr(10))],
                MetaSource::Direct,
                active(),
            ),
        ];
        std::fs::write(&file, serde_json::to_string(&entries).unwrap()).unwrap();
        let store = NodeMetaStore::new(Some(file.clone()));
        let ids: Vec<NodeId> = store.snapshot().into_iter().map(|e| e.id).collect();
        assert!(!ids.contains(&dead), "gossip + 仅回环地址的条目加载即清理");
        assert!(
            !ids.contains(&direct_active),
            "direct + 仅回环 + Active 的条目同样清理（无条件剔回环）"
        );
        assert!(ids.contains(&keep_gossip), "带公网地址的 gossip 条目保留");
        assert!(
            !ids.contains(&dead_inactive),
            "direct + 仅回环 + Inactive 条目加载即清理"
        );
        assert_eq!(ids.len(), 2, "混合条目剥回环保留、非回环条目不动");
        let e = entry_of(&store, &mixed);
        assert_eq!(
            e.addrs,
            vec![MetaAddr::verified(addr(10))],
            "混合条目剥离回环地址、公网部分保留"
        );
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 摘要条目速记（12/13 用）。
    fn ent(
        id: &NodeId,
        addr: Option<SocketAddr>,
        last_seen: u64,
        alive: bool,
        verified: bool,
    ) -> MetaDigestEntry {
        MetaDigestEntry {
            id: id.clone(),
            addr,
            last_seen,
            alive,
            verified,
            exit_offered: false,
        }
    }

    // 12. 自广播（digest 首条固定为自身）：含 advertise 地址（alive/verified/
    //     last_seen=now——本机第一手活性结论），注册表转述紧随其后；advertise=None
    //     时首条仍携带（id/alive）但无地址；空注册表也广播（仅自广播一条——
    //     新身份上线即被全网感知的最小摘要）；自广播回声经 merge 跳过不落库
    #[test]
    fn digest_announces_self_first() {
        let mut store = NodeMetaStore::new(None);
        let self_id = nid(80);
        let peer = nid(81);
        let now = 5000u64;
        store.record_conn(&peer, addr(1), now - 10);
        // advertise=Some：首条 == 自身
        let digest = store.digest_with_self(&self_id, Some(addr(70)), now);
        assert_eq!(digest.len(), 2, "自广播首条 + 注册表转述");
        assert_eq!(digest[0].id, self_id, "首条固定为自身");
        assert_eq!(digest[0].addr, Some(addr(70)), "自广播携带 advertise 地址");
        assert_eq!(digest[0].last_seen, now, "上线即当前时刻");
        assert!(digest[0].alive, "自己必然活着");
        assert!(digest[0].verified, "本机对自身活性的第一手结论");
        assert_eq!(digest[1].id, peer, "注册表转述紧随其后");
        assert_eq!(digest[1].addr, Some(addr(1)));
        // advertise=None：首条仍带（id/alive/last_seen），但无地址
        let digest = store.digest_with_self(&self_id, None, now);
        assert_eq!(digest[0].id, self_id, "未配置 advertise 也广播自身活性");
        assert_eq!(digest[0].addr, None, "无通告地址——只带 id/alive/last_seen");
        assert!(digest[0].alive);
        // 空注册表：仅自广播一条
        let empty = NodeMetaStore::new(None);
        assert_eq!(
            empty.digest_with_self(&self_id, Some(addr(70)), now).len(),
            1,
            "空注册表也广播"
        );
        // 自广播回声：对端把它转述回来（带地址/不带地址两种形态）→ merge 跳过
        // 不落库（自己的活性自己最清楚；注册表不收自己）
        store.merge_digest(
            &self_id,
            &[
                ent(&self_id, Some(addr(70)), now, true, true),
                ent(&self_id, None, now, true, true),
            ],
            now,
            Duration::from_secs(12),
        );
        assert!(
            store.snapshot().into_iter().all(|e| e.id != self_id),
            "自广播回声不落库"
        );
    }

    // 13. 无地址报告（对端自广播未配置 advertise 的合并侧形态）：未知身份不
    //     建档；已知条目仅采纳新鲜度（Inactive 复活照常——活性信号独立于地址
    //     信号），addrs/source 不动
    #[test]
    fn merge_digest_addrless_report_updates_freshness_only() {
        let mut store = NodeMetaStore::new(None);
        let self_id = nid(82);
        let known = nid(83);
        let stranger = nid(84);
        let gone = nid(85);
        let now = 6000u64;
        let window = Duration::from_secs(12);
        store.record_conn(&known, addr(5), 5900);
        store.record_conn(&gone, addr(6), 5900);
        strike_out(&mut store, &gone, 5950); // Inactive
                                             // 未知身份的无地址报告：不建档（无可用地址的档案探不了活，纯噪声）
        store.merge_digest(
            &self_id,
            &[ent(&stranger, None, now - 1, true, true)],
            now,
            window,
        );
        assert!(
            store.snapshot().into_iter().all(|e| e.id != stranger),
            "无地址报告不建档"
        );
        // 已知 Active：仅采纳 last_seen，addrs/source 不动
        store.merge_digest(
            &self_id,
            &[ent(&known, None, now - 1, true, true)],
            now,
            window,
        );
        let e = entry_of(&store, &known);
        assert_eq!(e.last_seen, now - 1, "无地址报告的新鲜度照常采纳");
        assert_eq!(e.addrs, vec![ma_ok(5)], "不带入任何地址");
        assert_eq!(e.source, MetaSource::Direct, "无地址知识不改来源结论");
        // 已知 Inactive：新鲜 alive 报告复活照常，不带入地址
        let revived = store.merge_digest(
            &self_id,
            &[ent(&gone, None, now - 1, true, true)],
            now,
            window,
        );
        assert_eq!(revived, 1, "无地址报告仍可复活");
        assert!(matches!(
            entry_of(&store, &gone).state,
            MetaState::Active { .. }
        ));
        assert_eq!(
            entry_of(&store, &gone).addrs,
            vec![ma_ok(6)],
            "复活不带入地址"
        );
    }

    // 14. 账本防抖落盘不被 meta 早退短路（审查 A1 回归，端到端）：账本脏 +
    //     meta 干净（无连接/无 gossip——meta_file=None 恒无落盘动作）→ 防抖
    //     到期后引擎 tick 仍应把账本 JSON 落盘。原实现 flush_ledger 挂在 meta
    //     待写判断之后，本场景整个运行期不落盘（强杀即丢账本）。
    #[tokio::test]
    async fn ledger_flush_survives_clean_meta_short_circuit() {
        use os_identity::IdentityLedger;
        let dir =
            std::env::temp_dir().join(format!("p2p-meta-ledger-flush-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ledger_file = dir.join("identity-ledger.json");
        let _ = std::fs::remove_file(&ledger_file);
        let ledger: os_identity::SharedLedger = std::sync::Arc::new(std::sync::Mutex::new(
            IdentityLedger::new(Some(ledger_file.clone())),
        ));
        let node = crate::P2pNode::spawn(crate::P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: crate::Timing::testing(),
            mdns_enabled: false,
            meta_file: None, // meta 纯内存——引擎节奏内恒走早退分支
            identity_ledger: Some(ledger.clone()),
            ..crate::P2pConfig::default()
        })
        .unwrap();
        // 只脏账本不脏 meta：同 NodeID 冲突观测（register_conn 冲突路径的账本
        // 动作——不触碰 meta 注册表）
        {
            let mut l = ledger.lock().unwrap();
            l.record_conflict(&node.self_id().to_hex(), lo(33516), 17_000_000_000);
        }
        // 防抖 10s（FLUSH_DEBOUNCE）+ 引擎 tick 150ms：轮询等账本文件出现
        let deadline = Instant::now() + os_identity::FLUSH_DEBOUNCE + Duration::from_secs(4);
        loop {
            if ledger_file.exists() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "账本脏数据应在防抖到期后落盘（meta 干净不得短路账本落盘）"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // 落盘内容：一条同 NodeID 冲突观测（node_id 为本机身份）
        let raw = std::fs::read_to_string(&ledger_file).unwrap();
        let list: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let arr = list.as_array().expect("账本 JSON 为记录数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["node_id"], node.self_id().to_hex());
        assert_eq!(arr[0]["conflict_entries"].as_array().unwrap().len(), 1);
        node.shutdown().await;
        let _ = std::fs::remove_file(&ledger_file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 15. 网络出口声明（network-exit，2026-08-30）：set_self_exit → digest
    //     自广播首条带 exit_offered；merge 对新条目/新鲜更新/复活三种路径
    //     透传该位；陈旧报告不回灌；转述 digest 继续透传；旧 JSON 缺字段 → false
    #[test]
    fn exit_offer_flag_broadcast_and_merge() {
        let mut store = NodeMetaStore::new(None);
        let self_id = nid(86);
        let now = 7000u64;
        let exit_node = nid(87);
        let known = nid(88);
        store.record_conn(&exit_node, addr(1), now - 10);
        store.record_conn(&known, addr(2), now - 10);
        // 关（默认）：自广播 exit_offered=false
        let digest = store.digest_with_self(&self_id, None, now);
        assert!(!digest[0].exit_offered, "默认不声明出口");
        assert!(!digest.iter().any(|d| d.exit_offered), "全表无出口声明");
        // 开：自广播首条带 true；注册表转述条目不带（它们没声明）
        store.set_self_exit(true);
        let digest = store.digest_with_self(&self_id, None, now);
        assert!(digest[0].exit_offered, "开启后自广播携带出口声明");
        assert!(
            digest.iter().skip(1).all(|d| !d.exit_offered),
            "其他节点的声明不冒名顶替"
        );
        assert!(store.self_exit());

        // 对端视角合并：未知出口节点新建即带 exit_offered=true
        let mut peer = NodeMetaStore::new(None);
        peer.merge_digest(
            &nid(89),
            &[MetaDigestEntry {
                id: exit_node.clone(),
                addr: Some(addr(3)),
                last_seen: now,
                alive: true,
                verified: true,
                exit_offered: true,
            }],
            now,
            Duration::from_secs(12),
        );
        assert!(
            entry_of(&peer, &exit_node).exit_offered,
            "新建条目学到出口声明"
        );
        // 转述：peer 的 digest 继续透传该位（全网 1-2 轮感知）
        let relayed = peer.digest(now + 1);
        assert!(
            relayed
                .iter()
                .find(|d| d.id == exit_node)
                .unwrap()
                .exit_offered
        );
        // 已知条目：新鲜报告更新声明（含撤销——报告 false）
        peer.merge_digest(
            &nid(89),
            &[MetaDigestEntry {
                id: exit_node.clone(),
                addr: Some(addr(3)),
                last_seen: now + 5,
                alive: true,
                verified: true,
                exit_offered: false,
            }],
            now + 5,
            Duration::from_secs(12),
        );
        assert!(
            !entry_of(&peer, &exit_node).exit_offered,
            "新鲜报告撤销出口声明"
        );
        // 陈旧报告不回灌（last_seen 更旧 → 保持现状）
        peer.merge_digest(
            &nid(89),
            &[MetaDigestEntry {
                id: exit_node.clone(),
                addr: Some(addr(3)),
                last_seen: now - 100,
                alive: true,
                verified: true,
                exit_offered: true,
            }],
            now + 6,
            Duration::from_secs(12),
        );
        assert!(
            !entry_of(&peer, &exit_node).exit_offered,
            "陈旧报告不改声明"
        );
        // 复活路径也透传：出局条目经带 exit_offered 的新鲜 alive 报告复活
        peer.record_conn(&known, addr(2), now);
        strike_out(&mut peer, &known, now);
        peer.merge_digest(
            &nid(89),
            &[MetaDigestEntry {
                id: known.clone(),
                addr: Some(addr(4)),
                last_seen: now + 7,
                alive: true,
                verified: true,
                exit_offered: true,
            }],
            now + 7,
            Duration::from_secs(12),
        );
        assert!(entry_of(&peer, &known).exit_offered, "复活条目学到出口声明");
    }

    // 16. 旧持久化格式（无 exit_offered 字段）加载 → false；直连建档不产生
    //     出口声明知识（对端 offer 由其自广播/转述决定）
    #[test]
    fn exit_offer_legacy_persist_and_direct_default_false() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta-exit.json");
        let a = nid(91);
        let legacy = format!(
            "[{{\"id\":\"{}\",\"addrs\":[\"203.0.113.9:41000\"],\"first_seen\":100,\
             \"last_seen\":200,\"state\":{{\"active\":{{\"score\":60,\"consec_fail\":0}}}},\
             \"source\":\"direct\",\"verified\":true}}]",
            a.to_hex(),
        );
        std::fs::write(&file, legacy).unwrap();
        let store = NodeMetaStore::new(Some(file.clone()));
        assert!(
            !entry_of(&store, &a).exit_offered,
            "旧格式无 exit_offered 字段 → false"
        );
        // 直连建档默认不声明
        let mut fresh = NodeMetaStore::new(None);
        fresh.record_conn(&nid(92), addr(5), 100);
        assert!(!entry_of(&fresh, &nid(92)).exit_offered);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 17. 线格式 + 配置接线：digest JSON 携带 exit_offered（旧节点缺字段解析
    //     为 false）；P2pConfig::exit_offered → 注册表初始声明；Handle::
    //     set_exit_offered 命令链路往返（不 panic / 不悬挂）。
    //     （说明：双节点 gossip 端到端在回环拓扑下无法覆盖——回环观测地址按
    //     设计不入注册表（2026-08-25 定调），无地址的自广播也不建档；真实
    //     跨机 gossip 由 store 级测试 15 覆盖合并/转述语义。）
    #[tokio::test]
    async fn exit_offer_wire_format_and_handle_wiring() {
        // 线格式：新节点 digest 带 exit_offered 位
        let entry = MetaDigestEntry {
            id: nid(95),
            addr: Some(addr(1)),
            last_seen: 42,
            alive: true,
            verified: true,
            exit_offered: true,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["exit_offered"], serde_json::json!(true));
        let back: MetaDigestEntry = serde_json::from_value(json).unwrap();
        assert!(back.exit_offered);
        // 旧节点/旧文件缺字段 → false（向后兼容）
        let legacy = serde_json::json!({
            "id": nid(95).to_hex(),
            "addr": "203.0.113.1:41000",
            "last_seen": 42,
            "alive": true,
            "verified": true,
        });
        let old: MetaDigestEntry = serde_json::from_value(legacy).unwrap();
        assert!(!old.exit_offered);
        // 配置接线：with_exit_offer 初始声明 → 自广播携带
        let mut store = NodeMetaStore::with_exit_offer(None, true);
        assert!(store.self_exit());
        assert!(
            store.digest_with_self(&nid(96), None, 100)[0].exit_offered,
            "配置开启即自广播携带出口声明"
        );
        store.set_self_exit(false);
        assert!(
            !store.digest_with_self(&nid(96), None, 101)[0].exit_offered,
            "关闭后自广播撤销"
        );
        assert!(!crate::P2pConfig::default().exit_offered, "默认关");
        // Handle 命令链路：spawn 节点 → set_exit_offered 往返（不悬挂/不 panic）
        let node = crate::P2pNode::spawn(crate::P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: crate::Timing::testing(),
            mdns_enabled: false,
            ..crate::P2pConfig::default()
        })
        .unwrap();
        assert!(node.set_exit_offered(true).await, "命令应答切换后值");
        assert!(!node.set_exit_offered(false).await, "再切回应答 false");
        node.shutdown().await;
    }

    /// 造一条「Inactive 且 last_seen 距 `now` 远超 TTL」的条目（TTL 测试用：
    /// 建档于远古 → 五振出局——出局时刻同样远古）。
    fn ancient_inactive(store: &mut NodeMetaStore, id: &NodeId, now: u64, ttl: u64) {
        store.record_conn(id, addr(1), now - ttl - 10_000);
        strike_out(store, id, now - ttl - 9_000);
    }

    // 18. TTL 清除规则（2026-09-02「非活跃节点，三天不心跳就移除」）：Inactive
    //     且 last_seen 距今 > TTL → 整条删除（返回被清条目原样快照）；边界
    //     （恰好 == TTL）保留；未到期保留；Active 永不清（远古僵尸交评分/
    //     五振机制管辖）；无可清时不置脏
    #[test]
    fn purge_ttl_expiry_boundary_and_active_immunity() {
        let mut store = NodeMetaStore::new(None);
        let now = 100_000_000u64;
        let ttl = META_INACTIVE_TTL_DEFAULT_SECS;
        let expired = nid(100); // Inactive，last_seen 距今 TTL+1（清除对象）
        let boundary = nid(101); // Inactive，恰好 TTL（age == ttl 非 > → 保留）
        let kept = nid(102); // Inactive，出局不满一天
        let zombie = nid(103); // Active，远古（90 天没见过——永不清）
        store.record_conn(&expired, addr(1), now - ttl - 1);
        strike_out(&mut store, &expired, now - ttl);
        store.record_conn(&boundary, addr(2), now - ttl);
        strike_out(&mut store, &boundary, now - ttl + 1);
        store.record_conn(&kept, addr(3), now - 3600);
        strike_out(&mut store, &kept, now - 3500);
        store.record_conn(&zombie, addr(4), now - 90 * 24 * 3600);
        let purged = store.purge_expired_inactive(now, ttl);
        assert_eq!(purged.len(), 1, "仅超期 Inactive 被清");
        assert_eq!(purged[0].id, expired, "返回被清条目原样快照（日志用）");
        assert_eq!(purged[0].last_seen, now - ttl - 1);
        let ids: Vec<NodeId> = store.snapshot().into_iter().map(|e| e.id).collect();
        assert!(!ids.contains(&expired), "超期 Inactive 整条删除");
        assert!(ids.contains(&boundary), "age == TTL（严格大于才清）保留");
        assert!(ids.contains(&kept), "未到期 Inactive 保留");
        assert!(
            ids.contains(&zombie),
            "Active 永不清（评分/五振机制自会处理）"
        );
        assert_eq!(ids.len(), 3);
        // 无可清 → 不置脏（flush_final 无产出）
        let _ = store.flush_final().expect("上轮清除已置脏"); // 清脏（JSON 不落盘）
        let mut quiet = NodeMetaStore::new(None);
        quiet.record_conn(&nid(104), addr(5), 100);
        strike_out(&mut quiet, &nid(104), 100);
        let _ = quiet.flush_final(); // 清脏——之后无变更
        assert!(
            quiet
                .purge_expired_inactive(200, META_INACTIVE_TTL_DEFAULT_SECS)
                .is_empty()
                && quiet.flush_final().is_none(),
            "无可清时不置脏（删除才是脏——不是只增不改）"
        );
    }

    // 19. TTL 禁用（=0）与复活续命：ttl=0 恒不清（向后兼容开关，远古 Inactive
    //     也保留）；复活三条路（他节点新鲜报告 / 手动 reactivate / 直连重连）
    //     都刷新 last_seen → 超龄条目复活后不被清（对照组：不复活即被清）
    #[test]
    fn purge_ttl_disabled_and_revival_renews() {
        let now = 2_000_000u64;
        let ttl = META_INACTIVE_TTL_DEFAULT_SECS;
        // 对照组：超龄 Inactive 不复活 → 清
        let mut store = NodeMetaStore::new(None);
        let control = nid(110);
        ancient_inactive(&mut store, &control, now, ttl);
        assert!(
            store
                .purge_expired_inactive(now, ttl)
                .iter()
                .any(|e| e.id == control),
            "对照组：超龄 Inactive 被清"
        );
        // 禁用（=0）：同样的超龄条目保留
        let mut off = NodeMetaStore::new(None);
        let ancient = nid(111);
        ancient_inactive(&mut off, &ancient, now, ttl);
        assert!(
            off.purge_expired_inactive(now, 0).is_empty(),
            "ttl=0 禁用清除（向后兼容开关）"
        );
        assert!(
            matches!(entry_of(&off, &ancient).state, MetaState::Inactive { .. }),
            "禁用态下超龄条目原样保留"
        );
        // 复活路径 ①：他节点新鲜 alive 报告（merge_digest）→ last_seen 刷新
        let mut gossip = NodeMetaStore::new(None);
        let a = nid(112);
        ancient_inactive(&mut gossip, &a, now, ttl);
        gossip.merge_digest(
            &nid(113),
            &[ent(&a, Some(addr(2)), now - 5, true, true)],
            now,
            Duration::from_secs(12),
        );
        assert!(
            gossip.purge_expired_inactive(now, ttl).is_empty(),
            "gossip 复活续命"
        );
        assert!(matches!(
            entry_of(&gossip, &a).state,
            MetaState::Active { .. }
        ));
        // 复活路径 ②：手动 reactivate → last_seen = now
        let mut manual = NodeMetaStore::new(None);
        let b = nid(114);
        ancient_inactive(&mut manual, &b, now, ttl);
        let addrs = manual.reactivate(&b, now);
        assert!(addrs.is_some(), "reactivate 返回探测地址表");
        assert!(
            manual.purge_expired_inactive(now, ttl).is_empty(),
            "手动复活续命"
        );
        // 复活路径 ③：直连重连（record_conn）→ last_seen = now
        let mut direct = NodeMetaStore::new(None);
        let c = nid(115);
        ancient_inactive(&mut direct, &c, now, ttl);
        direct.record_conn(&c, addr(3), now);
        assert!(
            direct.purge_expired_inactive(now, ttl).is_empty(),
            "直连复活续命"
        );
    }

    // 20. TTL 持久化重写：先落盘（模拟部署前的文件：三条都在）→ 清除置脏 →
    //     flush 产出不含被清条目 → 原子写 → 新 store 同路径加载（模拟重启）
    //     超期条目不再出现；未到期 Inactive 与 Active 保真
    #[test]
    fn purge_ttl_persistence_rewrite() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-ttl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta-ttl.json");
        let _ = std::fs::remove_file(&file);
        let now = 3_000_000u64;
        let ttl = META_INACTIVE_TTL_DEFAULT_SECS;
        let expired = nid(116); // 超期 Inactive（如 aliyun 重装换代的旧 NodeID）
        let kept = nid(117); // 未到期 Inactive
        let active = nid(118); // Active
        let mut store = NodeMetaStore::new(Some(file.clone()));
        store.record_conn(&expired, addr(1), now - ttl - 1);
        strike_out(&mut store, &expired, now - ttl);
        store.record_conn(&kept, addr(2), now - 100);
        strike_out(&mut store, &kept, now - 90);
        store.record_conn(&active, addr(3), now);
        // 部署前文件：三条都在
        let json = store.flush_final().expect("建档即脏");
        write_meta_atomic(&file, &json).unwrap();
        assert_eq!(
            NodeMetaStore::new(Some(file.clone())).snapshot().len(),
            3,
            "清除前文件含三条"
        );
        // 清除 + 落盘
        let purged = store.purge_expired_inactive(now, ttl);
        assert_eq!(purged.len(), 1);
        let json = store.flush_final().expect("清除置脏应产出重写");
        write_meta_atomic(&file, &json).unwrap();
        // "重启"：新 store 加载——超期条目不再出现，其余保真
        let reloaded = NodeMetaStore::new(Some(file.clone()));
        let list = reloaded.snapshot();
        assert_eq!(list.len(), 2, "重启后超期条目不再出现");
        assert!(list.iter().all(|e| e.id != expired));
        let ek = list.iter().find(|e| e.id == kept).unwrap();
        assert_eq!(
            ek.state,
            MetaState::Inactive { since: now - 90 },
            "未到期保真"
        );
        assert!(
            list.iter()
                .any(|e| e.id == active && matches!(e.state, MetaState::Active { .. })),
            "Active 保真"
        );
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 21. 扫描节流（tick 注入快进）：purge_due 仅在 300 的倍数到期；模拟引擎
    //     循环注入 counter + 人造时钟（now = counter，1s/tick，ttl=100）——
    //     条目 a（last_seen=0，第 101 tick 起超期）在第 300 tick 窗口被清；
    //     条目 b（last_seen=350，第 451 tick 起超期）跨过第 300 窗口存活、在
    //     第 600 窗口被清（窗口之间不扫全表——节流语义）
    #[test]
    fn purge_scan_throttle_fast_forward_ticks() {
        assert_eq!(META_PURGE_EVERY_TICKS, 300, "节流契约：每 300 tick 一扫");
        assert!(!purge_due(1));
        assert!(!purge_due(299));
        assert!(purge_due(300));
        assert!(!purge_due(301));
        assert!(purge_due(600));
        let mut store = NodeMetaStore::new(None);
        let ttl = 100u64;
        let a = nid(120);
        let b = nid(121);
        store.record_conn(&a, addr(1), 0);
        strike_out(&mut store, &a, 1); // last_seen=0：age(counter) > 100 自第 101 tick
        store.record_conn(&b, addr(2), 350);
        strike_out(&mut store, &b, 351); // last_seen=350：第 451 tick 起超期
        let mut purged_a_at = None;
        let mut purged_b_at = None;
        for counter in 1..=600u64 {
            if purge_due(counter) {
                for e in store.purge_expired_inactive(counter, ttl) {
                    if e.id == a {
                        purged_a_at = Some(counter);
                    } else if e.id == b {
                        purged_b_at = Some(counter);
                    }
                }
            }
        }
        assert_eq!(purged_a_at, Some(300), "a 在首个到期窗口（300）被清");
        assert_eq!(
            purged_b_at,
            Some(600),
            "b 跨过 300 窗口（未超期）存活，600 窗口被清"
        );
    }

    // 22. 引擎端到端（启动扫描接线）：spawn 节点（meta_file 预置三条——超期
    //     Inactive / 未到期 Inactive / 远古 Active）→ 引擎启动加载后即时机
    //     扫描立即清掉超期条目（Handle::node_meta 可观察）；停机刷盘把清除
    //     结果写回文件（持久化同步）；其余两条保留。远古 Active 短窗口内会被
    //     心跳探败出局（不可达测试地址）但不会在本测试的时间尺度内被 TTL 清
    //     （周期性扫描 300 tick = 45s@testing），presence 断言不受影响。
    #[tokio::test]
    async fn purge_engine_startup_scan_wiring() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-ttl-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta-ttl-engine.json");
        let _ = std::fs::remove_file(&file);
        let now = crate::api::unix_now();
        let ttl = META_INACTIVE_TTL_DEFAULT_SECS;
        let expired = nid(130); // 超期 Inactive（存量——启动即清对象）
        let kept = nid(131); // 未到期 Inactive
        let zombie = nid(132); // 远古 Active（TTL 不清——评分机制管辖）
        let entries = vec![
            NodeMetaEntry {
                id: expired.clone(),
                addrs: vec![MetaAddr::verified(addr(1))],
                first_seen: now - ttl - 5000,
                last_seen: now - ttl - 3600,
                state: MetaState::Inactive { since: now - ttl },
                source: MetaSource::Direct,
                verified: true,
                exit_offered: false,
            },
            NodeMetaEntry {
                id: kept.clone(),
                addrs: vec![MetaAddr::verified(addr(2))],
                first_seen: now - 5000,
                last_seen: now - 60,
                state: MetaState::Inactive { since: now - 50 },
                source: MetaSource::Direct,
                verified: true,
                exit_offered: false,
            },
            NodeMetaEntry {
                id: zombie.clone(),
                addrs: vec![MetaAddr::verified(addr(3))],
                first_seen: now - 30 * 24 * 3600,
                last_seen: now - 30 * 24 * 3600,
                state: MetaState::Active {
                    score: 80,
                    consec_fail: 0,
                },
                source: MetaSource::Direct,
                verified: true,
                exit_offered: false,
            },
        ];
        std::fs::write(&file, serde_json::to_string(&entries).unwrap()).unwrap();
        let node = crate::P2pNode::spawn(crate::P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: crate::Timing::testing(),
            mdns_enabled: false,
            meta_file: Some(file.clone()),
            ..crate::P2pConfig::default()
        })
        .unwrap();
        // 启动即时机扫描（引擎 pre-loop）——轮询等超期条目消失（毫秒级）
        let deadline = Instant::now() + Duration::from_secs(5);
        let metas = loop {
            let metas = node.node_meta().await;
            if metas.iter().all(|e| e.id != expired) {
                break metas;
            }
            assert!(
                Instant::now() < deadline,
                "启动扫描应立即清除超期 Inactive 条目"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(metas.iter().any(|e| e.id == kept), "未到期 Inactive 保留");
        assert!(
            metas.iter().any(|e| e.id == zombie),
            "远古 Active 不清（启动窗口）"
        );
        node.shutdown().await;
        // 停机刷盘（清除置脏）：文件同步重写——超期条目不在，其余在
        let raw = std::fs::read_to_string(&file).unwrap();
        let list: Vec<NodeMetaEntry> = serde_json::from_str(&raw).unwrap();
        assert!(
            list.iter().all(|e| e.id != expired),
            "持久化同步重写——重启后不再出现"
        );
        assert!(list.iter().any(|e| e.id == kept) && list.iter().any(|e| e.id == zombie));
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 23. TTL env 契约：未设置 = 默认 3 天（259,200s）；"0" = 禁用；数值覆盖；
    //     空白/非法值回落默认（纯函数测试——env 全局可变态不进单测）
    #[test]
    fn purge_ttl_env_parsing() {
        assert_eq!(META_INACTIVE_TTL_DEFAULT_SECS, 259_200, "默认 3 天");
        assert_eq!(parse_inactive_ttl(None), META_INACTIVE_TTL_DEFAULT_SECS);
        assert_eq!(parse_inactive_ttl(Some("")), META_INACTIVE_TTL_DEFAULT_SECS);
        assert_eq!(
            parse_inactive_ttl(Some("   ")),
            META_INACTIVE_TTL_DEFAULT_SECS
        );
        assert_eq!(parse_inactive_ttl(Some("0")), 0, "0 = 禁用清除");
        assert_eq!(parse_inactive_ttl(Some("3600")), 3600);
        assert_eq!(parse_inactive_ttl(Some(" 86400 \n")), 86_400, "容忍空白");
        assert_eq!(
            parse_inactive_ttl(Some("three-days")),
            META_INACTIVE_TTL_DEFAULT_SECS,
            "非法值回落默认"
        );
        assert_eq!(
            parse_inactive_ttl(Some("-1")),
            META_INACTIVE_TTL_DEFAULT_SECS,
            "负数非法 → 默认"
        );
    }
}
