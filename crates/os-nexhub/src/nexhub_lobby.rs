//! `NexHubLobbyRouteHandler` —— NexHub 大厅（发现层）REST API
//! （设计文档 `docs/NEXHUB_LOBBY_DESIGN.md` §4/§5/§6）。
//!
//! 本模块原长在 os-api `handlers/nexhub_lobby.rs`，NexHub 独立化（审计
//! docs/COMPONENT_INDEPENDENCE_AUDIT.md §6）后随 crate 迁入 os-nexhub，经
//! `os_common::gateway::RouteHandler` 轻量契约与网关对接（os-api 装配层桥接）。
//!
//! 定位：NexHub 的**发现层**——个人项目可**发布**到大厅分享，也可从大厅
//! **一键克隆**到本地 `/tank/git-repos/`。对标 GitHub Explore/Public/clone。
//!
//! # 设计要点（设计文档 §3/§4）
//!
//! - **发布快照**：大厅存发布时的元数据快照（commit 数/大小/默认分支/最后提交/
//!   README 摘要），浏览零开销（不实时扫描仓库）；重复发布=刷新快照。
//! - **SQLite `hub_lobby` 表**：复用 IM 的 SQLite 模式（`Mutex<Connection>` 短锁
//!   快查快放，WAL，文件库优先 `/tank/os-data`）。
//! - **克隆**（[`NexHubLobbyRouteHandler::clone_entry_async`] 克隆源选择，
//!   [`select_clone_source`] 纯函数可单测）：本机条目（source_node/homepage_node
//!   =local 或 source_url 本机存在）→ 现行 `source_url` 路径 spawn
//!   `git clone --bare`（10s 超时）；**联邦条目 → 条目自带的
//!   `clone_url_http`（发布节点定格的 `/git/*` Smart HTTP 地址）跨节点拉取**
//!   （120s 超时——source_url 是源节点本机路径，消费节点不存在）；两者皆不可用
//!   才报错（错误区分「本机路径不存在 / 源节点不可达」）。成功 `download_count+1`。
//!   **一期不需要外置反代**（§6）：os-api 8080 已是
//!   统一入口（API + `/git/*` Smart HTTP），客户端只与本机 os-api 通信。
//! - **复用 code_repo**：`repos_dir()`（仓库根目录）、`build_clone_url`（SSH 通道）、
//!   `build_clone_url_http`（`/git/*` Smart HTTP 通道）；元数据统计参考
//!   `scan_repos_blocking`（spawn 系统 git）。
//!
//! # 链上身份与权限（设计 `docs/MEDIA_GEN_AND_CHAIN_AUTH.md` §C）
//!
//! 身份 = secp256k1 公钥（压缩 `0x`+66 hex），权限 = 私钥持有者。与 IM 同款
//! 挑战-签名三步认证（共享内核 [`os_common::chain_auth::ChainAuth`]，本 handler
//! 挂**独立实例**——IM 的 token 在此不可用，但同一密钥对可两侧分别认证）：
//!
//! 1. `POST /api/v1/nexhub/auth/challenge {pubkey}` → `{nonce}`（60s 单次有效）
//! 2. 客户端用私钥对 nonce 的 UTF-8 字节做 ECDSA 签名（65 字节 `r||s||v` hex）
//! 3. `POST /api/v1/nexhub/auth/verify {pubkey, nonce, signature}` → `{token}`（24h）
//! 4. 写端点 `Authorization: Bearer <nexhub token>`——服务端反查 pubkey 归因，
//!    body/query 自报身份字段（publisher/buyer/hunter/poster）**一律忽略并覆盖**。
//!
//! 身份解析顺序（全部写端点）：链上 token → pubkey；无/无效 token → 回落系统
//! admin 判定（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN` 精确比对）；两者皆非 → 401。
//! 因此原 `requires_auth=true` 的写路由全部改为 false（handler 内自验，同 IM
//! 用户面模式），网关中间件不再拦截链上身份调用方。
//!
//! **权限矩阵**：
//!
//! | 操作 | 链上身份（pubkey） | admin |
//! |------|--------------------|-------|
//! | publish（新条目） | publisher=pubkey，owner_kind=pubkey | 保留 body.publisher（缺省 local） |
//! | 重发布/下架 pubkey 条目 | 仅 owner 同 pubkey | 允许（平台管理） |
//! | federate（推送联邦） | 仅 owner 同 pubkey | 允许（含平台托管条目） |
//! | 重发布/下架存量字符串条目（NexOS/zcode/…） | 403（平台托管） | 允许 |
//! | bounty create | poster=pubkey | 保留 body.poster |
//! | bounty claim/submit | hunter=pubkey（submit 仅 claim 的 hunter） | hunter="admin" |
//! | bounty approve/reject/cancel | 仅 poster 同 pubkey | 允许 |
//! | purchase | buyer=pubkey（owner 豁免=buyer==条目 owner pubkey） | 代记 buyer="admin" |
//! | clone（免费条目） | **匿名可**（2026-08-25 起公开——克隆=只读动作） | 匿名可 |
//! | clone（付费条目） | 需已购授权或 owner 豁免（402 引导 purchase） | 恒可 |
//!
//! # 路由表（28 条，component="nexhub-lobby"；大厅前缀 /api/v1/nexhub/lobby，
//! 悬赏前缀 /api/v1/nexhub/bounty，认证前缀 /api/v1/nexhub/auth）
//!
//! 认证（公开挑战-签名）：
//!
//! | method | path                                    | 动作 |
//! |--------|-----------------------------------------|------|
//! | POST   | `/api/v1/nexhub/auth/challenge`          | 签发 nonce |
//! | POST   | `/api/v1/nexhub/auth/verify`            | 验签发 token |
//!
//! 大厅（发现/分享层）：
//!
//! | method | path                                    | 动作 |
//! |--------|-----------------------------------------|------|
//! | GET    | `/api/v1/nexhub/lobby`                  | 大厅列表（`?q=` 搜索 `?tag=` 过滤 `?sort=downloads|recent`）|
//! | GET    | `/api/v1/nexhub/lobby/stats`            | 发布数/总下载/top 标签聚合 |
//! | GET    | `/api/v1/nexhub/lobby/entitlements`     | 购买授权记录查询（`?repo=` `?buyer=` 可组合；需身份）|
//! | GET    | `/api/v1/nexhub/lobby/:name`            | 详情（readme_excerpt + 双通道 clone 地址）|
//! | POST   | `/api/v1/nexhub/lobby/publish`          | 发布本地仓库（链上身份/admin，快照元数据；可带价格/货币；**只写本地**）|
//! | POST   | `/api/v1/nexhub/lobby/:name/federate`   | 推送/重新推送到联邦大厅（两步联邦第二步；owner/admin）|
//! | DELETE | `/api/v1/nexhub/lobby/:name`            | 下架（owner pubkey/admin，仓库本身不动）|
//! | POST   | `/api/v1/nexhub/lobby/:name/purchase`   | 购买授权（付费条目；buyer=token 身份，§10；eth 条目接力**链上验真**——dApp 一期）|
//! | POST   | `/api/v1/nexhub/lobby/:name/clone`      | 克隆到本地（**公开**——只读动作免鉴权；付费条目仍需 purchase 或 owner 豁免）|
//!
//! PR 审核流（轻量版，2026-08-23 定稿：git 通道 + SQLite `hub_pull_requests` 表，
//! 不做 GitHub 式完整 PR 系统——分支由 git push 到裸仓既有通道提交，本层只做
//! 归因/状态机/合并执行）：
//!
//! | method | path                                          | 动作 |
//! |--------|-----------------------------------------------|------|
//! | GET    | `/api/v1/nexhub/lobby/:repo/pulls`            | PR 列表（`?status=` 过滤，公开）|
//! | POST   | `/api/v1/nexhub/lobby/:repo/pulls`            | 创建 PR（链上身份归因 author_pubkey；校验 source_branch 存在）|
//! | GET    | `/api/v1/nexhub/lobby/:repo/pulls/:id`        | PR 详情（含 `git diff base..source --stat` 摘要）|
//! | POST   | `/api/v1/nexhub/lobby/:repo/pulls/:id/merge`  | 合并（仅 admin / repo owner pubkey；裸仓 merge-tree 落地）|
//! | POST   | `/api/v1/nexhub/lobby/:repo/pulls/:id/reject` | 拒绝（仅 admin / repo owner pubkey，可带 reason）|
//! | POST   | `/api/v1/nexhub/lobby/:repo/pulls/:id/close`  | 关闭（author 本人或 admin）|
//!
//! 发版权限控制（2026-08-23 定稿：release = `git tag` + SQLite `hub_releases` 行，
//! **仅 admin** 可发版/删版——发版是平台级动作，repo owner 也不可）：
//!
//! | method | path                                         | 动作 |
//! |--------|----------------------------------------------|------|
//! | GET    | `/api/v1/nexhub/lobby/:repo/releases`        | release 列表（公开）|
//! | POST   | `/api/v1/nexhub/lobby/:repo/releases`        | 创建 release（仅 admin：`git tag` + 落库 + 联邦广播）|
//! | DELETE | `/api/v1/nexhub/lobby/:repo/releases/:tag`   | 删除 release（仅 admin：删库行 + `git tag -d`）|
//!
//! 悬赏（出资求活层，§11）：
//!
//! | method | path                                    | 动作 |
//! |--------|-----------------------------------------|------|
//! | GET    | `/api/v1/nexhub/bounty`                 | 悬赏列表（`?status=` `?q=`）|
//! | GET    | `/api/v1/nexhub/bounty/:id`             | 悬赏详情 |
//! | POST   | `/api/v1/nexhub/bounty`                 | 发布悬赏（奖励必须 >0，货币化复用 resolve_price）|
//! | POST   | `/api/v1/nexhub/bounty/:id/claim`       | hunter 认领（open→claimed）|
//! | POST   | `/api/v1/nexhub/bounty/:id/submit`      | hunter 提交交付物（→submitted）|
//! | POST   | `/api/v1/nexhub/bounty/:id/approve`     | poster 验收 + 放款（→paid；eth 悬赏接力**链上验真**，body 可带 pay_to/chain_id/rpc_url）|
//! | POST   | `/api/v1/nexhub/bounty/:id/reject`      | poster 驳回（→open 重开）|
//! | POST   | `/api/v1/nexhub/bounty/:id/cancel`      | poster 取消（open→cancelled）|
//!
//! **常驻**（设计文档 §5 + 2026-08-23 自动联邦）：`nexos` 主仓库**默认常驻
//! 大厅**——每次启动（建库路径）无条件确保已发布：条目不存在 → 自动发布第一条
//! （publisher: `NexOS`）；已存在 → 刷新快照（等价重复 publish：`INSERT OR REPLACE`
//! 语义，保留 `download_count`）——下架后重启会回来，推送新代码后 commit 数/
//! last_commit/README 摘要不过期。同时**自动联邦**：常驻条目直接置 `federated=true`
//! 并 `broadcast_entry`（nexos 一启动就在联邦大厅，无需手动点推送按钮）——构造期
//! P2P 通道尚未装配时广播静默跳过（标志仍置位），通道注入（`set_transport`）时
//! 补推常驻条目。逃生口：env `NEXOS_LOBBY_NO_AUTO_PUBLISH=1` 跳过发布**与**联邦
//! （用户显式下架 nexos 后不想被启动拉回的场景）。
//!
//! **自动同步**（2026-08-25，设计文档 §15）：常驻流程顺手补装 nexos.git 的
//! post-receive 钩子（[`crate::lobby_sync_hook`]）——此后 `git push` 新提交即
//! 后台触发 publish（重取 latest_commit/pushed_at 等快照）+ federate（联邦重
//! 广播），联邦消费端按 name 幂等合并——大厅条目随仓库最新提交自动更新，
//! 不再停留在发布/启动时的旧快照（系统自举依赖，见 §15.5）。
//!
//! **副本自动跟随**（2026-08-27，同步链最后一环）：消费端 [`LobbyFedEndpoint::ingest`]
//! 收到同源 nexos 新快照（Written/Refreshed）后，自动后台拉取本地 bare 副本
//! `NEXOS_GIT_REPOS_DIR/nexos.git`——此前只有大厅**显示**跟随快照刷新，本地
//! 副本仍停留旧提交，用户从本节点 NexHub clone 到的是旧代码。仅跟内置主仓
//! nexos；节流 10 分钟 + HEAD 判等省流；env `NEXOS_LOBBY_AUTO_PULL=0` 关闭。
//!
//! **链上支付验真**（dApp 一期，2026-08-31）：purchase/approve 的「txid 非空即
//! 过」升级为真实 EVM RPC 核验（核验本体 [`crate::chain_verify`]，接线层见本文件
//! 「链上支付验真」段——[`ChainPayGate`] 可注入网关 + [`check_chain_payment`]
//! 业务编排，os-api 网关 PaymentOrder confirm 复用同一套）。语义表 / env 清单 /
//! 降级策略见该段注释与 docs/NEXHUB_LOBBY_DESIGN.md §10、docs/GATEWAY_MONETIZATION.md。
//! **二期增量（2026-09-02）**：①ERC-20（USDT@EVM）Transfer 日志核验；②金额规则
//! [`AmountRule`]（网关 confirm / 悬赏 approve = AtLeast「≥应付额」，NexHub 购买
//! 保持 Exact 等值）——接线定稿见「链上支付验真」段与两份 docs。

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::chain_verify::{AmountRule, Erc20Spec, TxProof, VerifyOutcome};
use os_common::chain_auth::{self, ChainAuth};
use os_common::gateway::{
    ApiRequest, ApiResponse, HandlerError, HttpMethod, RouteHandler, RouteSpec,
};
// 复用 code_repo 的 pub 资产（同 crate 横向依赖，随迁即消跨 crate 耦合）：
// 仓库根目录 + 双通道 clone URL 构造 + 有效默认分支解析（含 main→master 回退）
// + git log 解析（latest_commit 结构化快照复用 parse_git_log）。
use crate::code_repo::{
    build_clone_url, build_clone_url_http, parse_git_log, repos_dir, resolve_default_branch_sync,
};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 大厅条目（hub_lobby 行，设计文档 §4 数据模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyEntry {
    /// 仓库名（唯一键，不含 `.git`）。
    pub repo_name: String,
    /// 描述（发布时未传则回退裸仓库 description 文件内容）。
    #[serde(default)]
    pub description: String,
    /// 标签（JSON 数组持久化）。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 发布者（用户/agent 名；nexos 常驻条目为 "NexOS"）。
    #[serde(default)]
    pub publisher: String,
    /// 克隆源（本机裸仓库路径 / http / ssh URL）。
    #[serde(default)]
    pub source_url: String,
    /// 来源节点 id（**联邦预留**，一期恒 "local"）。
    #[serde(default = "default_homepage_node")]
    pub homepage_node: String,
    /// 联邦来源节点（P3，docs/NEXOS_P2P_NETWORK_DESIGN.md §8）：本地发布恒
    /// `"local"`；经 os-p2p 联邦同步来的远程条目 = 发布节点的昵称/NodeID 短式
    /// （前端据此显示 🌐 远程徽章「来自 node-106」）。serde default 兼容存量
    /// JSON（无该字段的旧条目/旧 payload 一律解析为 "local"）。
    #[serde(default = "default_source_node")]
    pub source_node: String,
    /// 发布节点的 HTTP Smart Git 克隆 URL（`http://<host>:<port>/git/<name>.git`，
    /// 构造用 `code_repo::build_clone_url_http`——host 走 advertise_host 地址
    /// 优先链（env 覆盖 → 本机非回环 IPv4 → hostname 保底），跨节点可达）。
    /// 发布/常驻刷新时定格进条目，联邦载荷原样携带——**消费节点一键克隆联邦
    /// 条目经此 URL 从源节点 HTTP 拉取**（source_url 是源节点本机路径，跨节点
    /// 不存在，见 `select_clone_source`）。旧 payload/旧库无此字段 → 空串
    /// （历史条目需源节点重 publish 刷新出可达地址）。
    #[serde(default)]
    pub clone_url_http: String,
    /// 提交数快照（所有分支，`git rev-list --count --all`）。
    #[serde(default)]
    pub commit_count: u32,
    /// 仓库占用字节快照（裸仓库递归求和）。
    #[serde(default)]
    pub size_bytes: u64,
    /// 默认分支快照（`git symbolic-ref --short HEAD`；HEAD 指向的分支不存在时
    /// 回退探测 main → master，见 `code_repo::resolve_default_branch_sync`）。
    #[serde(default)]
    pub default_branch: String,
    /// 最近一次提交摘要（`<short-hash> - <subject>`；空仓库为 None）。
    #[serde(default)]
    pub last_commit: Option<String>,
    /// 最近一次提交日期（ISO；空仓库为 None）。
    #[serde(default)]
    pub last_commit_date: Option<String>,
    /// README.md 前 500 字符（卡片摘要）。
    #[serde(default)]
    pub readme_excerpt: String,
    /// 克隆计数（活跃度）。
    #[serde(default)]
    pub download_count: u64,
    /// 发布时间（RFC3339；重复发布刷新）。
    pub published_at: String,
    /// 价格（最小货币单位；BTC=聪 satoshi，NEX/USDC=其最小单位）。`0` = 免费。
    /// 设计文档 §10（货币化）：免费条目 `price_sats==0`，付费条目 `price_sats>0`
    /// 且 `currency` 非空；克隆前需先 `POST /:name/purchase` 取得授权（§10 授权门禁）。
    #[serde(default)]
    pub price_sats: u64,
    /// 计价货币：`free`（占位）/ `btc` / `nex`（NexOS 虚拟币）/ `usdc` / `eth` /
    /// `usdt`（二期：EVM 链 ERC-20 核验，最小单位=微 USDT）。
    /// 免费条目恒为 `free`，付费条目必须是指定链（与 os-wallet `ChainKind` 对齐）。
    #[serde(default = "default_currency")]
    pub currency: String,
    /// 是否已推送到联邦大厅（两步联邦：本地发布 → `POST /:name/federate` 推送）：
    /// - 本地发布恒为 `false`（不广播）——联邦条目只能从本地已发布条目推送；
    /// - federate 端点置 `true` 并广播最新快照；重复推送不改变值（重新广播）；
    /// - 重发布保留既有值（对端快照以「重新推送」刷新）。
    ///
    /// 记录的是**发布侧推送状态**——P2P 通道未装配时广播静默跳过，标志位仍置位；
    /// serde default 兼容存量 JSON/联邦 payload（缺字段 → 未推送）。
    #[serde(default)]
    pub federated: bool,
    /// 最新提交**结构化**快照（短 hash + subject + 作者 + 时间，`git log -1` 解析，
    /// 复用 `code_repo::parse_git_log`）——比 `last_commit`（仅 hash+subject 拼接串）
    /// 多作者维度，前端可直接展示结构字段。发布/常驻刷新即重取；None = 空仓库
    /// 或旧快照（serde default 兼容旧 payload/旧库 NULL 列）。
    ///
    /// 自动同步链（2026-08-25）：git push → post-receive 钩子（[`crate::lobby_sync_hook`]）
    /// → POST /publish 重取本字段 → POST /:name/federate 重广播——大厅条目随仓库
    /// 最新提交自动更新，联邦消费端按 name 幂等合并（详见设计文档 §15）。
    #[serde(default)]
    pub latest_commit: Option<LatestCommit>,
    /// 最近一次快照刷新时间（RFC3339）——publish/常驻刷新/钩子触发重发布均更新。
    /// 与 `published_at`（发布时间）区分：前者表达「大厅最后一次感知到仓库变化」，
    /// 联邦消费端/前端据此排序「最近有活力的条目」。serde default 兼容旧 payload。
    #[serde(default)]
    pub pushed_at: String,
}

/// 最新提交结构化快照（`git log -1 --format=%H%x1f%an%x1f%s%x1f%ai` 经
/// `code_repo::parse_git_log` 解析后取首条构造；hash 截短 7 位）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatestCommit {
    /// 短 hash（7 位十六进制）。
    pub short_hash: String,
    /// 提交标题（subject，首行）。
    pub subject: String,
    /// 作者名（`%an`）。
    pub author: String,
    /// 提交时间（ISO，`%ai`）。
    pub date: String,
}

/// 货币默认值（免费）。
fn default_currency() -> String {
    "free".to_string()
}

/// 联邦来源节点默认值（本地发布）。
fn default_source_node() -> String {
    "local".to_string()
}

/// 条目是否本机发布（联邦判定的一翼）：`source_node`/`homepage_node` 均 local。
///
/// `source_node` 是权威标记——联邦接收端（[`LobbyFedEndpoint::ingest`]）会把
/// 条目改写为来源节点 id；`homepage_node` 为联邦预留字段，本地发布恒 local
/// **且联邦载荷并不改写它**（远程条目也带 local），故必须与 source_node 同查
/// （AND），单查 homepage_node 会把联邦条目误判成本机。
fn entry_is_local(entry: &LobbyEntry) -> bool {
    entry.source_node == default_source_node() && entry.homepage_node == default_homepage_node()
}

/// [`clone_entry_async`](NexHubLobbyRouteHandler::clone_entry_async) 的克隆源
/// 选择结果（纯函数 [`select_clone_source`] 产物，可单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CloneSource {
    /// 本机路径克隆（条目的 `source_url`——本机条目，或路径恰在本机存在）。
    Local(String),
    /// 联邦 HTTP 克隆（发布节点定格的 `clone_url_http`，跨节点经 `/git/*`
    /// Smart HTTP 从源节点拉取；空串 = 历史条目未携带，调用方负责报错引导）。
    FederatedHttp(String),
}

/// 一键克隆的克隆源选择（纯函数，2026-08-25 跨节点修复）：
///
/// - **本机条目**（`source_node`/`homepage_node`=local，或 `source_url` 恰为本机
///   存在路径——跨节点同布局下本地直克隆更快）→ [`CloneSource::Local`]：
///   现行 `source_url` 路径 spawn git（10s 超时），行为不变；
/// - **联邦条目**（`source_node` ≠ local 且本机无该路径）→
///   [`CloneSource::FederatedHttp`]：用条目自带的 `clone_url_http` 经 HTTP 从
///   源节点拉取（120s 超时）——修复前误用源节点的本地路径（如 113 克隆
///   `/tank/git-repos/nexos.git` 报 "repository does not exist"，该路径只在
///   源节点 106 存在）。
fn select_clone_source(entry: &LobbyEntry) -> CloneSource {
    if entry_is_local(entry)
        || (!entry.source_url.is_empty() && Path::new(&entry.source_url).exists())
    {
        return CloneSource::Local(entry.source_url.clone());
    }
    CloneSource::FederatedHttp(entry.clone_url_http.trim().to_string())
}

/// 联邦 `clone_url_http` 疑似旧地址（784547f 地址链之前发布的历史条目）：
/// host 段非 IP 字面量（hostname 如 `ub2604` 跨节点解析不了）。克隆失败时
/// 据此附加提示「源节点需重 publish 刷新地址」。
fn fed_url_host_is_hostname(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let host = rest.split([':', '/']).next().unwrap_or_default();
    !host.is_empty() && host.parse::<std::net::IpAddr>().is_err()
}

/// 发布仓库元数据快照（scan 产物，不进 DB 的中间结构）。
#[derive(Debug, Clone, Default)]
struct RepoSnapshot {
    description: String,
    commit_count: u32,
    size_bytes: u64,
    default_branch: String,
    last_commit: Option<String>,
    last_commit_date: Option<String>,
    readme_excerpt: String,
    /// 最新提交结构化快照（latest_commit 列，JSON 持久化）。
    latest_commit: Option<LatestCommit>,
}

/// 大厅统计（GET /stats 响应体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyStats {
    /// 已发布条目数。
    pub published_count: usize,
    /// 总下载（download_count 之和）。
    pub total_downloads: u64,
    /// top 标签聚合（按出现次数降序，最多 10 个）。
    pub top_tags: Vec<TagCount>,
}

/// 单个标签的计数（top_tags 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub count: u64,
}

// ----------------------------------------------------------------------------
// 纯函数（可单测）
// ----------------------------------------------------------------------------

/// README 摘要截断（按字符取前 `limit` 个，避免切坏 UTF-8 多字节字符）。
#[must_use]
pub fn excerpt_of(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 校验仓库名（与 code_repo::validate_repo_name 同规则，该函数私有故本地实现）：
/// 非空、不含 `/` 与 `.`/`..` 段、不以 `-` 开头（避免 git 参数注入与路径穿越）。
fn validate_repo_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("repo 名不可为空".into());
    }
    if n.starts_with('-') {
        return Err("repo 名不可以 '-' 开头".into());
    }
    if n.contains('/') {
        return Err("repo 名不可包含 '/'".into());
    }
    if n == ".." || n == "." {
        return Err("repo 名不可为 '.' 或 '..'".into());
    }
    Ok(())
}

/// 校验大厅条目名（同 [`validate_repo_name`]，路由参数用）。
fn validate_lobby_name(name: &str) -> Result<(), String> {
    validate_repo_name(name).map_err(|e| format!("name 非法: {e}"))
}

/// 排序键合法性：`downloads` / `recent`（默认 recent）。
#[must_use]
pub fn normalize_sort(sort: Option<&str>) -> &'static str {
    match sort.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some("downloads") => "downloads",
        _ => "recent",
    }
}

/// 合法货币集合（与 os-wallet `ChainKind` 对齐；`free` 为免费占位）。
/// `usdt`（dApp 二期，2026-09-02）：EVM 链上走 ERC-20 Transfer 日志核验
/// （链 ID 定位不到 = TRON 人工通道）；`price_sats`/`amount_sats` 语义 =
/// 最小单位（微 USDT，10^-6，env `NEXOS_USDT_EVM_DECIMALS` 可调）。
#[must_use]
fn is_valid_currency(c: &str) -> bool {
    matches!(
        c.to_ascii_lowercase().as_str(),
        "free" | "btc" | "nex" | "usdc" | "eth" | "usdt"
    )
}

/// 解析发布时的价格/货币（设计文档 §10 货币化）：
/// - `price_sats` 缺省或 0 → 免费（currency 强制 `free`）
/// - `price_sats > 0` → 必须给定合法非空货币（缺省 `btc`），且不得为 `free`
///
/// 返回 `(price_sats, currency)`；非法组合返回 `Err`（调用方转 400）。
fn resolve_price(
    price_sats: Option<u64>,
    currency: Option<String>,
) -> Result<(u64, String), String> {
    let price = price_sats.unwrap_or(0);
    if price == 0 {
        return Ok((0, "free".to_string()));
    }
    let cur = currency
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "btc".to_string());
    if cur == "free" {
        return Err("付费条目 currency 不得为 free（请指定 btc/nex/usdc/eth/usdt）".into());
    }
    if !is_valid_currency(&cur) {
        return Err(format!(
            "不支持的 currency: {cur}（可选 free/btc/nex/usdc/eth/usdt）"
        ));
    }
    Ok((price, cur))
}

// ----------------------------------------------------------------------------
// blocking git/文件系统辅助（参考 code_repo::scan_repos_blocking）
// ----------------------------------------------------------------------------

/// 同步执行 `git --git-dir=<bare> <args>`，返回 `(success, stdout)`。失败降级 `(false, "")`。
fn run_git_sync(git_dir: &str, args: &[&str]) -> (bool, String) {
    let mut cmd = std::process::Command::new("git");
    cmd.arg(format!("--git-dir={git_dir}"));
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    match cmd.output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// 同步执行 `git --git-dir=<bare> <args>`，返回 `(success, 合并输出)`——stdout
/// 为空时回退 stderr（`git tag` 等命令的错误信息走 stderr，用于错误归因）。
fn run_git_sync_loud(git_dir: &str, args: &[&str]) -> (bool, String) {
    let mut cmd = std::process::Command::new("git");
    cmd.arg(format!("--git-dir={git_dir}"));
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            if stdout.trim().is_empty() {
                (
                    out.status.success(),
                    String::from_utf8_lossy(&out.stderr).to_string(),
                )
            } else {
                (out.status.success(), stdout)
            }
        }
        Err(_) => (false, String::new()),
    }
}

/// 递归求目录总字节（仓库 size 快照）。失败返回 0，不 panic。
fn dir_size_bytes(path: &str) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(path)];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if let Ok(meta) = e.metadata() {
                    if meta.is_dir() {
                        stack.push(e.path());
                    } else {
                        total += meta.len();
                    }
                }
            }
        }
    }
    total
}

/// 读取裸仓库 `description` 文件；默认文本（"Unnamed repository..."）视为空。
fn read_description(bare: &str) -> String {
    let raw = std::fs::read_to_string(format!("{bare}/description")).unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with("Unnamed repository") {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// 扫描单个裸仓库的元数据快照（spawn_blocking 内执行，不跨 await 持锁）：
/// commit 数 / 大小 / 默认分支 / 最后提交 / README.md 前 500 字符摘要。
fn snapshot_repo_blocking(repos_dir: &str, name: &str) -> RepoSnapshot {
    let bare = format!("{repos_dir}/{name}.git");
    let description = read_description(&bare);
    let size_bytes = dir_size_bytes(&bare);
    // 提交数（所有分支）
    let (cok, cout) = run_git_sync(&bare, &["rev-list", "--count", "--all"]);
    let commit_count = if cok {
        cout.trim().parse::<u32>().unwrap_or(0)
    } else {
        0
    };
    // 默认分支 + 有效分支 ref：先读 HEAD symref；HEAD 指向的分支不存在（空仓，
    // 或只推了非 HEAD 分支——如 init 落 master 而用户只推 main，外部 agent
    // 接入实测踩到的坑）→ 回退探测 main → master 取实际存在的分支，保证
    // "只推 main 的新仓"与"存量 master 仓"都能取到 README 与 last_commit
    // （详见 code_repo::resolve_default_branch_sync）。
    let default_branch = resolve_default_branch_sync(&bare);
    let branch_ref = format!("refs/heads/{default_branch}");
    // 最近一次提交：<short> \x1f <subject> \x1f <date>（用有效分支而非裸 HEAD）
    let (lok, lout) = run_git_sync(
        &bare,
        &["log", "-1", "--format=%h\x1f%s\x1f%ai", &branch_ref],
    );
    let (last_commit, last_commit_date) = if lok {
        let parts: Vec<&str> = lout.trim_end().split('\x1f').collect();
        if parts.len() >= 3 {
            (
                Some(format!("{} - {}", parts[0], parts[1])),
                Some(parts[2].to_string()),
            )
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    // 最新提交结构化快照（latest_commit 列）：`git log -1` 走 code_repo::parse_git_log
    // 同款 `%H\x1f%an\x1f%s\x1f%ai` 契约（复用既有解析器，含坏行降级），hash 截短
    // 7 位；空仓库/失败 → None（降级不 panic）。
    let (gok, gout) = run_git_sync(
        &bare,
        &["log", "-1", "--format=%H%x1f%an%x1f%s%x1f%ai", &branch_ref],
    );
    let latest_commit = if gok {
        parse_git_log(&gout)
            .into_iter()
            .next()
            .map(|c| LatestCommit {
                short_hash: c.hash.chars().take(7).collect(),
                subject: c.message,
                author: c.author,
                date: c.date,
            })
    } else {
        None
    };
    // README.md 摘要：git show <branch>:README.md（裸仓库无工作区，走 git 对象库）；
    // 不存在/空仓库 → 空摘要（降级不 panic）。
    let (rok, rout) = run_git_sync(&bare, &["show", &format!("{branch_ref}:README.md")]);
    let readme_excerpt = if rok {
        excerpt_of(rout.trim_start_matches('\u{feff}'), README_EXCERPT_CHARS)
    } else {
        String::new()
    };
    RepoSnapshot {
        description,
        commit_count,
        size_bytes,
        default_branch,
        last_commit,
        last_commit_date,
        readme_excerpt,
        latest_commit,
    }
}

// ----------------------------------------------------------------------------
// 链上身份（Caller）：token 反查 pubkey / admin 回落（设计 §C 权限执行）
// ----------------------------------------------------------------------------

/// 已认证的 NexHub 调用方（`Authorization: Bearer` 解析结果）。
///
/// 解析顺序（[`NexHubLobbyRouteHandler::caller`]）：
/// 1. nexhub 链上 token（`/api/v1/nexhub/auth/verify` 签发）→ 反查 pubkey；
/// 2. 无/无效 → 回落系统 admin 判定（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`
///    精确比对，与 os-api 网关同一环境变量语义）；
/// 3. 两者皆非 → None（调用方回 401）。
enum Caller {
    /// 链上身份：publisher/poster/hunter/buyer 全部归因到该 pubkey。
    Pubkey {
        pubkey: String,
        /// 展示名（pubkey 派生 EVM 地址）。
        display_name: String,
    },
    /// 系统 admin（平台托管/管理通道）。
    Admin,
}

impl Caller {
    /// 归因标识（写库的 owner/buyer/hunter 值）：pubkey 身份 → pubkey；
    /// admin → `"admin"`。
    fn actor(&self) -> &str {
        match self {
            Caller::Pubkey { pubkey, .. } => pubkey,
            Caller::Admin => "admin",
        }
    }

    /// 是否为链上 pubkey 身份（非 admin）。
    fn pubkey(&self) -> Option<&str> {
        match self {
            Caller::Pubkey { pubkey, .. } => Some(pubkey),
            Caller::Admin => None,
        }
    }
}

/// 条目 owner 是否为链上身份：publisher 字段是合法压缩公钥（`0x`+66 hex 可解析）
/// → owner_kind=pubkey；否则为存量字符串条目（NexOS/zcode/local/…）= 平台托管。
fn entry_owner_is_pubkey(publisher: &str) -> bool {
    chain_auth::parse_pubkey(publisher).is_some()
}

// ----------------------------------------------------------------------------
// NexHubLobbyRouteHandler
// ----------------------------------------------------------------------------

/// NexHub 大厅路由处理器——HTTP 边界适配到 SQLite `hub_lobby` 发布索引 +
/// 系统 git 子进程（快照扫描 / 服务端克隆）。
///
/// 持有 `Mutex<Connection>`（短锁快放）+ 仓库根目录（构造时定格，测试注入
/// 临时目录隔离，避免运行中读 env 的竞态）+ [`ChainAuth`]（链上身份
/// nonce/token 桶，main.rs 装配时经 [`Self::with_chain_auth`] 注入共享 `Arc`）
/// + 系统 admin token（构造时读 env，测试经 [`Self::with_admin_token`] 注入）。
pub struct NexHubLobbyRouteHandler {
    db: Arc<Mutex<Connection>>,
    /// 联邦端点（P3，与 `db` 共享同一把锁的连接——发布路径广播 + os-api 装配层
    /// 的 p2p 接收端写入走同一份 hub_lobby）。handler 被 Box 进网关后装配层
    /// 仍持 `fed_endpoint()` 的 Arc 继续操作。
    fed: Arc<LobbyFedEndpoint>,
    /// 仓库根目录（构造时取 `code_repo::repos_dir()`，测试可注入临时目录）。
    repos_dir: String,
    /// 链上身份认证存储（challenge/verify 的 nonce/token 桶；独立实例，
    /// 与 IM 的 token 桶互不相通）。
    auth: Arc<ChainAuth>,
    /// 系统 admin token（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`，构造时定格；
    /// None = 未启用 admin 回落，仅链上 token 可写）。
    admin_token: Option<String>,
    /// 链上支付验真网关（dApp 一期，2026-08-31）：构造时读 env 定格，测试经
    /// [`Self::with_chain_verify`] 注入（可替换执行器——见 [`ChainPayGate`]）。
    chain_verify: ChainPayGate,
}

impl NexHubLobbyRouteHandler {
    /// 组装字段（db 与联邦端点共享同一 `Arc<Mutex<Connection>>`——发布路径
    /// 与联邦接收端写同一份 hub_lobby，锁语义与重构前完全一致）。
    fn from_conn_parts(
        conn: Connection,
        repos_root: &str,
        auth: Arc<ChainAuth>,
        admin_token: Option<String>,
    ) -> Self {
        let db = Arc::new(Mutex::new(conn));
        Self {
            fed: Arc::new(LobbyFedEndpoint::new(db.clone(), repos_root)),
            db,
            repos_dir: repos_root.to_string(),
            auth,
            admin_token,
            chain_verify: ChainPayGate::from_env(),
        }
    }

    /// 构造 handler：打开默认 DB 路径 + 建表 + nexos 常驻（仓库存在时自动
    /// 发布/刷新快照；env `NEXOS_LOBBY_NO_AUTO_PUBLISH=1` 可跳过）。
    #[must_use]
    pub fn new() -> Self {
        Self::open(
            &default_db_path(),
            &repos_dir(),
            Arc::new(ChainAuth::new()),
            admin_token_from_env(),
        )
    }

    /// main.rs 装配构造：默认 DB 路径 + 仓库根 + **共享**链上认证存储
    /// （照 IM 的 Arc 共享模式——装配层与 handler 验同一批 token）。
    /// 同时把该 Arc 注册进项目协作层（[`crate::issues`]）的进程级共享槽——
    /// `/api/v1/nexhub/auth/*` 签发的 token 在 coderepo 的 Issues/PR 写端点
    /// 同样可验（agent 一处登录，两处可用；见 issues.rs「链上身份共享」）。
    #[must_use]
    pub fn with_chain_auth(auth: Arc<ChainAuth>) -> Self {
        crate::issues::register_shared_chain_auth(auth.clone());
        Self::open(
            &default_db_path(),
            &repos_dir(),
            auth,
            admin_token_from_env(),
        )
    }

    /// 用指定 DB 路径 + 仓库根目录构造（测试/诊断注入）。
    #[must_use]
    pub fn with_db_path(path: &str, repos_dir: &str) -> Self {
        Self::open(
            path,
            repos_dir,
            Arc::new(ChainAuth::new()),
            admin_token_from_env(),
        )
    }

    /// 用临时内存库 + 指定仓库根目录构造（测试注入：数据隔离；nexos 仓库
    /// 存在时常驻发布 + 自动联邦广播，与文件库构造路径同构）。
    #[must_use]
    pub fn with_repos_dir(repos_dir: &str) -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        let seeded = ensure_nexos_published(&conn, repos_dir).expect("nexos 常驻必成功");
        let handler = Self::from_conn_parts(
            conn,
            repos_dir,
            Arc::new(ChainAuth::new()),
            admin_token_from_env(),
        );
        // 自动联邦：常驻条目构造即广播（通道未装配时仅记跳过日志，标志已置位；
        // 通道注入时 set_transport 补推——生产装配顺序是先构造 handler 再起 p2p）。
        if let Some(entry) = seeded {
            handler.fed.broadcast_entry(&entry);
        }
        handler
    }

    /// 用临时内存库构造，**不做 nexos 常驻**（测试注入：纯 DB 行为验证，数据隔离）。
    #[must_use]
    pub fn with_empty() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self::from_conn_parts(
            conn,
            &repos_dir(),
            Arc::new(ChainAuth::new()),
            admin_token_from_env(),
        )
    }

    /// 注入系统 admin token（链式构造器，测试用：绕开 env 的并行竞态；
    /// 生产路径经 [`admin_token_from_env`] 构造时定格）。
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        self.admin_token = Some(token.to_string());
        self
    }

    /// 注入链上支付验真网关（链式构造器，测试用：固定 [`VerifyOutcome`] /
    /// 调用计数的执行器 + 全可控配置；生产路径经 [`ChainPayGate::from_env`]
    /// 构造时定格——os-api 网关 PaymentOrder confirm 复用同一网关类型）。
    #[must_use]
    pub fn with_chain_verify(mut self, gate: ChainPayGate) -> Self {
        self.chain_verify = gate;
        self
    }

    /// 链上认证存储引用（装配层/诊断共享）。
    #[must_use]
    pub fn chain_auth(&self) -> Arc<ChainAuth> {
        self.auth.clone()
    }

    /// 联邦端点引用（os-api 装配层持有——handler Box 进网关后仍可经此注入
    /// p2p 传输通道 / 接收联邦条目写入本地 hub_lobby）。
    #[must_use]
    pub fn fed_endpoint(&self) -> Arc<LobbyFedEndpoint> {
        self.fed.clone()
    }

    fn open(
        path: &str,
        repos_root: &str,
        auth: Arc<ChainAuth>,
        admin_token: Option<String>,
    ) -> Self {
        let (conn, seeded) = open_db(path, repos_root).unwrap_or_else(|e| {
            eprintln!("nexhub-lobby: 打开 SQLite {path} 失败（{e}），降级到内存库");
            let conn = Connection::open_in_memory().expect("内存库必成功");
            create_schema(&conn).expect("建表必成功");
            let seeded = ensure_nexos_published(&conn, repos_root).expect("nexos 常驻必成功");
            (conn, seeded)
        });
        let handler = Self::from_conn_parts(conn, repos_root, auth, admin_token);
        // 自动联邦：常驻条目构造即广播（通道未装配 → 跳过日志；注入时 set_transport 补推）
        if let Some(entry) = seeded {
            handler.fed.broadcast_entry(&entry);
        }
        handler
    }

    /// 解析调用方身份（设计 §C 权限执行的入口，见 [`Caller`] 文档）。
    fn caller(&self, req: &ApiRequest) -> Option<Caller> {
        let token = chain_auth::bearer_token(&req.headers)?;
        if let Some(pubkey) = self.auth.verify_token(token) {
            let vk = chain_auth::parse_pubkey(&pubkey)?;
            return Some(Caller::Pubkey {
                pubkey,
                display_name: chain_auth::derive_display_name(&vk),
            });
        }
        if self.admin_token.as_deref() == Some(token) {
            return Some(Caller::Admin);
        }
        None
    }

    /// 当前全量大厅条目快照（从 DB 查，测试/诊断用）。
    #[must_use]
    pub fn entries_snapshot(&self) -> Vec<LobbyEntry> {
        let conn = self.db.lock().expect("db poisoned");
        load_entries(&conn, None, None, "recent").unwrap_or_default()
    }

    /// 服务端克隆条目到本地 repos_dir（POST /:name/clone 核心）。
    ///
    /// 克隆源选择（[`select_clone_source`]，2026-08-25 跨节点修复）：
    ///
    /// - 目标 `repos_dir/<name>.git` 已存在 → 直接注册计数（不 spawn git）；
    /// - 本机条目（source_node/homepage_node=local 或 source_url 本机存在）→
    ///   现行 `source_url` 路径本地 `git clone --bare`（10s 超时）；
    /// - **联邦条目 → 条目自带的 `clone_url_http` 经 HTTP 从源节点拉取**
    ///   （[`FED_CLONE_TIMEOUT_SECS`] 120s 超时——跨节点网络 clone 比本机宽）；
    ///   空 URL（历史条目在字段加入前发布）→ 直接报错引导源节点重 publish；
    /// - 两者皆不可用才 `Err`（错误信息区分「本机路径不存在 / 源节点不可达」）。
    ///
    /// 返回 `Ok(cloned是否真的执行了clone)`；`Err(reason)` → 502。
    async fn clone_entry_async(repos_root: &str, entry: &LobbyEntry) -> Result<bool, String> {
        let target = format!("{repos_root}/{}.git", entry.repo_name);
        if Path::new(&target).exists() {
            // 已在本地（本机源发布的典型路径）→ 直接注册，不重复克隆
            return Ok(false);
        }
        // 目标不存在：确保仓库根目录存在，再按克隆源选择拉取（本机源=本地
        // clone 10s；联邦源=HTTP 跨节点 120s，超时 kill 兜底）
        let _ = std::fs::create_dir_all(repos_root);
        match select_clone_source(entry) {
            CloneSource::Local(source) => {
                spawn_git_clone_bare(&source, &target, CLONE_TIMEOUT_SECS)
                    .await
                    .map_err(|e| format!("本机克隆源不可用（路径不存在或不可达）: {e}"))?
            }
            CloneSource::FederatedHttp(url) => {
                if url.is_empty() {
                    return Err(format!(
                        "联邦条目（来自 {}）缺少源节点 HTTP 克隆地址（历史条目无 clone_url_http）——源节点需重 publish 刷新地址",
                        entry.source_node
                    ));
                }
                spawn_git_clone_bare(&url, &target, FED_CLONE_TIMEOUT_SECS)
                    .await
                    .map_err(|e| {
                        format!(
                            "源节点 {} 不可达（本机无 source_url 路径，HTTP 拉取 {} 失败）: {e}{}",
                            entry.source_node,
                            url,
                            if fed_url_host_is_hostname(&url) {
                                "；条目 URL 为旧主机名格式（历史条目），源节点需重 publish 刷新地址"
                            } else {
                                ""
                            }
                        )
                    })?
            }
        }
        Ok(true)
    }
}

impl Default for NexHubLobbyRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for NexHubLobbyRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 链上身份认证（公开挑战-签名，同 IM 契约）——
            spec(HttpMethod::Post, PATH_AUTH_CHALLENGE, false, vec![]),
            spec(HttpMethod::Post, PATH_AUTH_VERIFY, false, vec![]),
            spec(HttpMethod::Get, PATH_LIST, false, vec![]),
            spec(HttpMethod::Get, PATH_STATS, false, vec![]),
            spec(HttpMethod::Get, PATH_DETAIL, false, vec![]),
            // 写端点一律 requires_auth=false：链上 token / admin 回落在 handler
            // 内自验（同 IM 用户面模式——网关中间件无法识别链上 token，若走
            // 系统中间件会把 pubkey 调用方全部挡在 401）。
            spec(HttpMethod::Post, PATH_PUBLISH, false, vec![]),
            // 两步联邦第二步：推送本地已发布条目到联邦大厅（owner pubkey/admin）
            spec(HttpMethod::Post, PATH_FEDERATE, false, vec![]),
            spec(HttpMethod::Delete, PATH_UNPUBLISH, false, vec![]),
            spec(HttpMethod::Post, PATH_PURCHASE, false, vec![]),
            spec(HttpMethod::Post, PATH_CLONE, false, vec![]),
            // 授权记录查询：读授权数据但含购买凭据，不公开——需身份
            // （链上 token 或 admin；?buyer= 维度自查）
            spec(HttpMethod::Get, PATH_ENTITLEMENTS, false, vec![]),
            // —— 悬赏（bounty）子资源：读公开，写需身份（链上 token / admin）——
            spec(HttpMethod::Get, PATH_BOUNTY_LIST, false, vec![]),
            spec(HttpMethod::Get, PATH_BOUNTY_DETAIL, false, vec![]),
            spec(HttpMethod::Post, PATH_BOUNTY_CREATE, false, vec![]),
            spec(HttpMethod::Post, PATH_BOUNTY_CLAIM, false, vec![]),
            spec(HttpMethod::Post, PATH_BOUNTY_SUBMIT, false, vec![]),
            spec(HttpMethod::Post, PATH_BOUNTY_APPROVE, false, vec![]),
            spec(HttpMethod::Post, PATH_BOUNTY_REJECT, false, vec![]),
            spec(HttpMethod::Post, PATH_BOUNTY_CANCEL, false, vec![]),
            // —— PR 审核流：读公开，写需身份（创建=链上身份；merge/reject=admin
            //    或 repo owner pubkey；close=author 或 admin——均在 handler 内自验）——
            spec(HttpMethod::Get, PATH_PULLS, false, vec![]),
            spec(HttpMethod::Post, PATH_PULLS, false, vec![]),
            spec(HttpMethod::Get, PATH_PULL_DETAIL, false, vec![]),
            spec(HttpMethod::Post, PATH_PULL_MERGE, false, vec![]),
            spec(HttpMethod::Post, PATH_PULL_REJECT, false, vec![]),
            spec(HttpMethod::Post, PATH_PULL_CLOSE, false, vec![]),
            // —— 发版权限控制：列表公开，创建/删除仅 admin（handler 内自验）——
            spec(HttpMethod::Get, PATH_RELEASES, false, vec![]),
            spec(HttpMethod::Post, PATH_RELEASES, false, vec![]),
            spec(HttpMethod::Delete, PATH_RELEASE_DELETE, false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, HandlerError> {
        let segs = path_segments(&req.path);
        let query = query_params(&req.path);
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/nexhub/auth/challenge —— 签发挑战 nonce（公开）
            //    body: {pubkey} → {nonce, expires_in, display_name}（与 IM 同款契约）
            (HttpMethod::Post, ["api", "v1", "nexhub", "auth", "challenge"]) => {
                #[derive(serde::Deserialize)]
                struct ChallengeReq {
                    pubkey: String,
                }
                let body: ChallengeReq = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析挑战请求体失败: {e}")))?;
                let vk = match chain_auth::parse_pubkey(&body.pubkey) {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                        ))
                    }
                };
                let nonce = self.auth.create_nonce(&body.pubkey);
                Ok(ok_json(serde_json::json!({
                    "nonce": nonce,
                    "expires_in": chain_auth::NONCE_TTL_SECS,
                    "display_name": chain_auth::derive_display_name(&vk),
                })))
            }

            // —— POST /api/v1/nexhub/auth/verify —— 验签 + 签发 token（公开）
            //    body: {pubkey, nonce, signature(0x+130 hex, 65 字节 r||s||v)}
            //    → {token, expires_in, pubkey, display_name}（24h 单点登录）
            (HttpMethod::Post, ["api", "v1", "nexhub", "auth", "verify"]) => {
                #[derive(serde::Deserialize)]
                struct VerifyReq {
                    pubkey: String,
                    nonce: String,
                    signature: String,
                }
                let body: VerifyReq = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析验签请求体失败: {e}")))?;
                let vk = match chain_auth::parse_pubkey(&body.pubkey) {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                        ))
                    }
                };
                let sig_hex = body.signature.trim().trim_start_matches("0x");
                let sig = match hex::decode(sig_hex) {
                    Ok(s) if s.len() == 65 => s,
                    _ => {
                        return Ok(error_response(
                            400,
                            "signature 非法：应为 65 字节 r||s||v 的 hex（可带 0x 前缀）",
                        ))
                    }
                };
                // nonce 用后即焚（签名失败同样烧掉，防暴力尝试）
                if !self.auth.take_nonce(&body.pubkey, &body.nonce) {
                    return Ok(error_response(401, "nonce 无效、已用或已过期（60s）"));
                }
                if !chain_auth::verify_nonce_signature(&vk, &body.nonce, &sig) {
                    return Ok(error_response(401, "签名验证失败"));
                }
                let (token, expires_in) = self.auth.issue_token(&body.pubkey);
                Ok(ok_json(serde_json::json!({
                    "token": token,
                    "expires_in": expires_in,
                    "pubkey": body.pubkey,
                    "display_name": chain_auth::derive_display_name(&vk),
                })))
            }

            // —— GET /api/v1/nexhub/lobby —— 大厅列表（?q= ?tag= ?sort=downloads|recent）
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby"]) => {
                let q = query.get("q").map(|s| s.trim()).filter(|s| !s.is_empty());
                let tag = query.get("tag").map(|s| s.trim()).filter(|s| !s.is_empty());
                let sort = normalize_sort(query.get("sort").map(|s| s.as_str()));
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    load_entries(&conn, q, tag, sort).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/nexhub/lobby/stats —— 发布数/总下载/top 标签
            //    （静态路由先于 :name 匹配，"stats" 不会落到详情）
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby", "stats"]) => {
                let stats = {
                    let conn = self.db.lock().expect("db poisoned");
                    lobby_stats(&conn)
                };
                Ok(ok_json(to_value(&stats)?))
            }

            // —— GET /api/v1/nexhub/lobby/entitlements —— 授权记录查询（需身份）
            //    （静态路由先于 :name 匹配，同 stats；"entitlements" 不会落到详情）
            //    ?repo=<name> 审计某条目全部买家；?buyer=<b> 自查购买记录；可组合；
            //    都不带则全量（admin 审计用）。身份闸门：链上 token 或 admin。
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby", "entitlements"]) => {
                if self.caller(&req).is_none() {
                    return Ok(auth_required());
                }
                let repo = query
                    .get("repo")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                let buyer = query
                    .get("buyer")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    list_entitlements(&conn, repo, buyer).map_err(db_err)?
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/nexhub/lobby/:name —— 详情（readme + 双通道 clone 地址）
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby", name]) => {
                if let Err(msg) = validate_lobby_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let entry = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_entry(&conn, name).map_err(db_err)?
                };
                let Some(entry) = entry else {
                    return Ok(error_response(404, &format!("大厅条目不存在: {name}")));
                };
                let mut body = to_value(&entry)?;
                body["clone_url_ssh"] = serde_json::json!(build_clone_url(name));
                // 本机条目：补本机 HTTP 双通道地址；联邦条目：**不覆盖**条目自带
                // 的 clone_url_http（源节点地址——消费节点尚无副本，本机 /git/*
                // 会 404，直连源节点匿名读才是可达通道）。
                if entry_is_local(&entry) {
                    body["clone_url_http"] = serde_json::json!(build_clone_url_http(name));
                }
                Ok(ok_json(body))
            }

            // —— POST /api/v1/nexhub/lobby/publish —— 发布本地仓库
            //    body: { repo, description?, tags?, publisher?, price_sats?, currency? }
            //    重复发布=刷新快照。身份：链上 token → publisher=pubkey（body 自报
            //    忽略）、owner_kind=pubkey；admin → 保留现行字符串 publisher。
            //    权限：重发布仅 owner 同 pubkey 或 admin；存量字符串条目仅 admin。
            //    两步联邦（2026-08）：发布**只写本地大厅，不广播**——联邦大厅的
            //    条目只能经 POST /:name/federate 从本地已发布条目推送（不存在
            //    「直接发布到联邦」的路径）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", "publish"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct PublishBody {
                    repo: String,
                    #[serde(default)]
                    description: Option<String>,
                    #[serde(default)]
                    tags: Option<Vec<String>>,
                    #[serde(default)]
                    publisher: Option<String>,
                    /// 价格（最小单位）。省略或 0 = 免费。
                    #[serde(default)]
                    price_sats: Option<u64>,
                    /// 计价货币：free/btc/nex/usdc/eth。省略按 price_sats 推导
                    /// （>0 → 默认 `btc`，0 → `free`）。
                    #[serde(default)]
                    currency: Option<String>,
                }
                let body: PublishBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析发布请求体失败: {e}")))?;
                let name = body.repo.trim().to_string();
                if let Err(msg) = validate_repo_name(&name) {
                    return Ok(error_response(400, &msg));
                }
                // —— 重发布权限（设计 §C）：owner_kind=pubkey 的条目仅同 pubkey
                //    或 admin 可改；存量字符串条目=平台托管仅 admin；不匹配 403。
                let existing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_entry(&conn, &name).map_err(db_err)?
                };
                if let Some(old) = &existing {
                    if let Some(pubkey) = caller.pubkey() {
                        if !entry_owner_is_pubkey(&old.publisher) || old.publisher != pubkey {
                            return Ok(forbidden_owner());
                        }
                    } // admin 恒可改（含他人 pubkey 条目——平台管理）
                }
                let dir = self.repos_dir.clone();
                let bare = format!("{dir}/{name}.git");
                if !Path::new(&bare).is_dir() {
                    return Ok(error_response(
                        404,
                        &format!("仓库不存在（需在 {dir} 下有 {name}.git）: {name}"),
                    ));
                }
                // 快照元数据（blocking 任务内 spawn git）
                let snap_dir = dir.clone();
                let snap_name = name.clone();
                let snap = tokio::task::spawn_blocking(move || {
                    snapshot_repo_blocking(&snap_dir, &snap_name)
                })
                .await
                .map_err(|e| HandlerError::Internal(format!("快照任务 join 失败: {e}")))?;
                // 价格/货币解析（免费/付费校验，非法组合 → 400）
                let (price_sats, currency) = match resolve_price(body.price_sats, body.currency) {
                    Ok(v) => v,
                    Err(e) => return Ok(error_response(400, &e)),
                };
                // —— 归因（body 自报 publisher 一律忽略）——
                let (publisher, owner_kind) = match &caller {
                    Caller::Pubkey { pubkey, .. } => (pubkey.clone(), "pubkey"),
                    Caller::Admin => (
                        body.publisher
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "local".to_string()),
                        "admin",
                    ),
                };
                let entry = LobbyEntry {
                    repo_name: name.clone(),
                    description: body
                        .description
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(snap.description),
                    tags: body.tags.unwrap_or_default(),
                    publisher,
                    source_url: bare.clone(),
                    homepage_node: default_homepage_node(),
                    source_node: default_source_node(),
                    // 发布节点定格的 HTTP 克隆地址（advertise_host 地址优先链，
                    // 跨节点可达 IP）——联邦消费节点一键克隆经此 URL 从本节点
                    // 拉取；重 publish 即刷新地址。
                    clone_url_http: build_clone_url_http(&name),
                    commit_count: snap.commit_count,
                    size_bytes: snap.size_bytes,
                    default_branch: snap.default_branch,
                    last_commit: snap.last_commit,
                    last_commit_date: snap.last_commit_date,
                    readme_excerpt: snap.readme_excerpt,
                    download_count: 0,
                    published_at: now_iso(),
                    price_sats,
                    currency,
                    // 两步联邦：本地发布恒未推送（新条目 false）；重发布在下方
                    // 保留既有值——对端快照以「重新推送」（/:name/federate）刷新。
                    federated: false,
                    // 自动同步链快照增量：结构化最新提交 + 本次刷新时间
                    // （重发布即重取/重置——post-receive 钩子据此刷新大厅条目）。
                    latest_commit: snap.latest_commit,
                    pushed_at: now_iso(),
                };
                // INSERT OR REPLACE（重复发布=刷新快照，保留 download_count 与
                // federated 推送状态）
                let saved = {
                    let conn = self.db.lock().expect("db poisoned");
                    let preserved_count = existing.as_ref().map(|e| e.download_count).unwrap_or(0);
                    let preserved_fed = existing.as_ref().map(|e| e.federated).unwrap_or(false);
                    let mut e2 = entry.clone();
                    e2.download_count = preserved_count;
                    e2.federated = preserved_fed;
                    insert_entry(&conn, &e2).map_err(db_err)?;
                    e2
                };
                // 本地发布到此为止（不广播）——联邦推送走独立端点
                // POST /:name/federate（owner pubkey / admin 显式两步操作）。
                let mut resp = to_value(&saved)?;
                resp["clone_url_ssh"] = serde_json::json!(build_clone_url(&name));
                resp["clone_url_http"] = serde_json::json!(build_clone_url_http(&name));
                resp["owner_kind"] = serde_json::json!(owner_kind);
                if let Caller::Pubkey { display_name, .. } = &caller {
                    resp["publisher_display"] = serde_json::json!(display_name);
                }
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/nexhub/lobby/:name/federate —— 推送/重新推送到联邦大厅
            //    （两步联邦第二步：联邦条目只能从**本地大厅已发布条目**推送——
            //    不存在「直接发布到联邦」的路径）。
            //    权限：同重发布/下架——owner_kind=pubkey 条目仅 owner 同 pubkey
            //    或 admin；存量字符串条目（NexOS/local/平台托管）仅 admin。
            //    动作：条目置 federated=true 落库 + broadcast_entry 广播最新快照；
            //    重复调用=重新推送（对端同源刷新，接收端保留本地克隆计数）。
            //    P2P 未装配时广播静默跳过，但 federated 标志仍置位（发布侧决策）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", name, "federate"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let entry = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_entry(&conn, name).map_err(db_err)?
                };
                let Some(entry) = entry else {
                    return Ok(error_response(
                        404,
                        &format!("大厅条目不存在: {name}（先发布到本地大厅再推送联邦）"),
                    ));
                };
                if let Some(pubkey) = caller.pubkey() {
                    if !entry_owner_is_pubkey(&entry.publisher) || entry.publisher != pubkey {
                        return Ok(forbidden_owner());
                    }
                } // admin 恒可推送（含平台托管条目）
                let saved = {
                    let conn = self.db.lock().expect("db poisoned");
                    let mut e2 = entry.clone();
                    e2.federated = true;
                    insert_entry(&conn, &e2).map_err(db_err)?;
                    e2
                };
                let first_push = !entry.federated;
                self.fed.broadcast_entry(&saved);
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "name": name,
                    "action": "federate",
                    "federated": true,
                    "first_push": first_push,
                    "source_node": saved.source_node,
                    "published_at": saved.published_at,
                    "note": if first_push {
                        "已推送到联邦大厅（其他 NexOS 节点将自动收到）".to_string()
                    } else {
                        "已重新推送（广播最新快照，对端同源刷新）".to_string()
                    },
                })))
            }

            // —— DELETE /api/v1/nexhub/lobby/:name —— 下架（仓库本身不动）
            //    权限（设计 §C）：owner_kind=pubkey 条目仅 owner 同 pubkey 或 admin；
            //    存量字符串条目（NexOS/…）=平台托管仅 admin；不匹配 403。
            (HttpMethod::Delete, ["api", "v1", "nexhub", "lobby", name]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let entry = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_entry(&conn, name).map_err(db_err)?
                };
                let Some(entry) = entry else {
                    return Ok(error_response(404, &format!("大厅条目不存在: {name}")));
                };
                if let Some(pubkey) = caller.pubkey() {
                    if !entry_owner_is_pubkey(&entry.publisher) || entry.publisher != pubkey {
                        return Ok(forbidden_owner());
                    }
                } // admin 恒可下架
                {
                    let conn = self.db.lock().expect("db poisoned");
                    delete_entry(&conn, name).map_err(db_err)?;
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "name": name,
                    "action": "unpublish",
                    "note": "仅下架大厅条目，本地仓库不受影响",
                })))
            }

            // —— POST /api/v1/nexhub/lobby/:name/purchase —— 购买授权（付费条目）
            //    body: { txid, chain?, amount_sats?, currency?, chain_id?, rpc_url? }；
            //    免费条目 → 400。buyer = token 身份（链上 token → pubkey；无 token
            //    时 admin 可代记 buyer="admin"）；body 自报 buyer 一律忽略（设计 §C
            //    修复冒名豁免①）。自证面校验（金额/货币/txid）后接力**链上验真**
            //    （dApp 一期，check_chain_payment——eth 条目 + 链/收款地址可定位时
            //    强制真实 RPC 核验；语义表见「链上支付验真」段注释）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", name, "purchase"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let entry = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_entry(&conn, name).map_err(db_err)?
                };
                let Some(entry) = entry else {
                    return Ok(error_response(404, &format!("大厅条目不存在: {name}")));
                };
                if entry.price_sats == 0 {
                    return Ok(error_response(400, &format!("免费条目无需购买: {name}")));
                }
                #[derive(serde::Deserialize)]
                struct PurchaseBody {
                    #[serde(default)]
                    chain: Option<String>,
                    txid: String,
                    #[serde(default)]
                    amount_sats: Option<u64>,
                    #[serde(default)]
                    currency: Option<String>,
                    /// 链 ID（dApp 一期链上验真；缺省回落数值 chain → env
                    /// `NEXOS_EVM_CHAIN_ID`）。
                    #[serde(default)]
                    chain_id: Option<u64>,
                    /// 显式 RPC（admin/条目 owner 自配，候选链第一段）。
                    #[serde(default)]
                    rpc_url: Option<String>,
                    /// ERC-20 合约地址（二期，usdt@EVM 条目；缺省回落 env
                    /// `NEXOS_USDT_EVM_CONTRACT`）。
                    #[serde(default)]
                    erc20_contract: Option<String>,
                    /// ERC-20 小数位（二期，usdt@EVM 条目；缺省回落 env
                    /// `NEXOS_USDT_EVM_DECIMALS`，默认 6）。
                    #[serde(default)]
                    erc20_decimals: Option<u8>,
                }
                let body: PurchaseBody = serde_json::from_value(req.body.clone())
                    .map_err(|e| HandlerError::Internal(format!("解析购买请求体失败: {e}")))?;
                // 归因：链上身份 → pubkey；admin 代记 "admin"（自报 buyer 忽略）
                let buyer = caller.actor().to_string();
                let currency = body.currency.unwrap_or_else(|| entry.currency.clone());
                let amount = body.amount_sats.unwrap_or(entry.price_sats);
                let chain = body.chain.clone().unwrap_or_else(|| entry.currency.clone());
                let mut receipt = Entitlement {
                    repo_name: name.to_string(),
                    buyer: buyer.clone(),
                    chain,
                    txid: body.txid.trim().to_string(),
                    amount_sats: amount,
                    currency: currency.clone(),
                    paid_at: now_iso(),
                    chain_block: None,
                    chain_value_wei: None,
                };
                if let Err(e) = verify_payment(&receipt, entry.price_sats, &entry.currency) {
                    return Ok(error_response(402, &e));
                }
                // —— 链上验真（dApp 一期）：收款方 = env NEXOS_HUB_PAY_TO（节点
                //    运营者/条目 owner 配置；**不收 body 自报地址**——买家自指
                //    地址再自付是白嫖通道）；amount 即最小货币单位（eth 条目
                //    = wei，18 位小数假设；usdt 条目 = token 最小单位）。
                //    金额规则（二期定稿）：**Exact 等值**——商品定价对账，
                //    多打/少打都 Mismatch，须按应付额整额打款。——
                let check = check_chain_payment(
                    &self.chain_verify,
                    &currency,
                    &receipt.txid,
                    &amount.to_string(),
                    &ChainPayHints {
                        chain_id: body.chain_id,
                        chain_str: body.chain.as_deref(),
                        rpc_url: body.rpc_url.as_deref(),
                        pay_to: None,
                        fallback_default_pay_to: true,
                        amount_rule: AmountRule::Exact,
                        erc20_contract: body.erc20_contract.as_deref(),
                        erc20_decimals: body.erc20_decimals,
                    },
                )
                .await;
                if let ChainPayCheck::Denied { status, reason } = &check {
                    return Ok(error_response(*status, reason));
                }
                if let ChainPayCheck::Verified {
                    block_number,
                    value_wei,
                    ..
                } = &check
                {
                    receipt.chain_block = Some(*block_number);
                    receipt.chain_value_wei = Some(value_wei.clone());
                }
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_entitlement(&conn, &receipt).map_err(db_err)?;
                }
                let mut resp = serde_json::json!({
                    "ok": true,
                    "repo_name": name,
                    "buyer": buyer,
                    "chain": receipt.chain,
                    "txid": receipt.txid,
                    "amount_sats": amount,
                    "currency": currency,
                    "paid_at": receipt.paid_at,
                    "note": "授权已记录（buyer=token 身份），现在可克隆（POST /:name/clone）",
                });
                if let Some(marker) = chain_verify_json(&check) {
                    if let Some(map) = resp.as_object_mut() {
                        map.insert("chain_verify".into(), marker);
                    }
                }
                Ok(ok_json(resp))
            }

            // —— POST /api/v1/nexhub/lobby/:name/clone —— 克隆到本地（公开）
            //    2026-08-25 起**免鉴权**（开发期公开）：克隆=只读动作（git 读
            //    upload-pack 同样匿名），拉取不应鉴权，推送才需要——无写权限
            //    的外部贡献者走 Issues/PR 流程（docs/NEXHUB_ISSUES_PR.md）。
            //    写操作（publish/federate/purchase/悬赏/PR merge 等）仍全走鉴权。
            //    实现面安全性：clone_entry_async 只往本机 repos_dir/<校验过的
            //    name>.git 落副本（克隆源——本机 source_url 或联邦条目的
            //    clone_url_http——均来自库内条目，非请求入参），
            //    远端源也是 git 只读拉取——纯读路径，匿名放行安全。
            //    例外：**付费条目（price_sats>0）门禁不因匿名放开**——匿名无
            //    身份可比对授权，回 402 引导先认证再 purchase；购买/豁免判定
            //    与归因逻辑（§C 身份化）不变。
            //    body 自报 buyer 不参与豁免判定（修复冒名豁免①：旧实现是纯
            //    字符串比对 buyer==publisher，任意人可冒 publisher 名免购）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", name, "clone"]) => {
                let caller = self.caller(&req); // Option：匿名 clone 放行（None）
                if let Err(msg) = validate_lobby_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let entry = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_entry(&conn, name).map_err(db_err)?
                };
                let Some(entry) = entry else {
                    return Ok(error_response(404, &format!("大厅条目不存在: {name}")));
                };
                // —— 货币化门禁（已识别身份：buyer = token 身份，admin 恒可；
                //    匿名：不放开，402 引导认证后 purchase）——
                if entry.price_sats > 0 {
                    let allowed = match caller.as_ref().and_then(Caller::pubkey) {
                        // 发布者本人豁免：调用方 pubkey == 条目 owner pubkey
                        // （存量字符串条目无链上 owner，pubkey 调用方不豁免）
                        Some(pubkey) => {
                            (entry_owner_is_pubkey(&entry.publisher) && entry.publisher == pubkey)
                                || {
                                    let conn = self.db.lock().expect("db poisoned");
                                    find_entitlement(&conn, name, pubkey)
                                        .map_err(db_err)?
                                        .is_some()
                                }
                        }
                        // admin（已识别身份的回落通道）
                        None if caller.is_some() => true,
                        // 匿名：付费条目仍需购买（先认证再 purchase）
                        None => false,
                    };
                    if !allowed {
                        return Ok(error_response(
                            402,
                            &format!(
                                "该条目为付费内容（{} {}），请先认证并 POST /api/v1/nexhub/lobby/{}/purchase 取得授权",
                                entry.price_sats, entry.currency, name
                            ),
                        ));
                    }
                }
                let dir = self.repos_dir.clone();
                match Self::clone_entry_async(&dir, &entry).await {
                    Ok(cloned) => {
                        // 成功（新克隆 / 本机源直接注册）→ download_count+1
                        let count = {
                            let conn = self.db.lock().expect("db poisoned");
                            bump_download(&conn, name).map_err(db_err)?
                        };
                        // 联邦远程条目（source_node != local）：经条目自带的
                        // clone_url_http 从源节点 HTTP 拉取（source_url 是源节点
                        // 本机路径，本机不存在），响应带 source_node + 提示文案，
                        // 前端据此显示「将从远程节点拉取」。
                        let remote = entry.source_node != default_source_node();
                        Ok(ok_json(serde_json::json!({
                            "ok": true,
                            "name": name,
                            "cloned": cloned,
                            "source_url": entry.source_url,
                            "source_node": entry.source_node,
                            "note": if remote {
                                format!("联邦远程条目（来自 {}）：已从源节点 HTTP 地址拉取（{}）", entry.source_node, entry.clone_url_http)
                            } else {
                                "本地条目".to_string()
                            },
                            "local_path": format!("{dir}/{name}.git"),
                            "download_count": count,
                            "clone_url_ssh": build_clone_url(name),
                            "clone_url_http": build_clone_url_http(name),
                        })))
                    }
                    Err(e) => Ok(error_response(502, &e)),
                }
            }

            // —— GET /api/v1/nexhub/bounty —— 悬赏列表（?status= ?q=）
            (HttpMethod::Get, ["api", "v1", "nexhub", "bounty"]) => {
                let status = query
                    .get("status")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                let q = query.get("q").map(|s| s.trim()).filter(|s| !s.is_empty());
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    load_bounties(&conn, status, q).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/nexhub/bounty/:id —— 悬赏详情
            (HttpMethod::Get, ["api", "v1", "nexhub", "bounty", id]) => {
                let b = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_bounty(&conn, id).map_err(db_err)?
                };
                match b {
                    Some(b) => Ok(ok_json(to_value(&b)?)),
                    None => Ok(error_response(404, &format!("悬赏不存在: {id}"))),
                }
            }

            // —— POST /api/v1/nexhub/bounty —— 发布悬赏（奖励必须 >0，货币化复用 resolve_price）
            //    poster = token 身份（链上 token → pubkey；admin 回落 body.poster）
            //    ——body 自报 poster 一律忽略（设计 §C 修复已知限制②）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "bounty"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct CreateBody {
                    title: String,
                    #[serde(default)]
                    description: Option<String>,
                    #[serde(default)]
                    tags: Option<Vec<String>>,
                    #[serde(default)]
                    poster: Option<String>,
                    reward_sats: u64,
                    #[serde(default)]
                    currency: Option<String>,
                    #[serde(default)]
                    target_url: Option<String>,
                    #[serde(default)]
                    deadline: Option<String>,
                }
                let body: CreateBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析悬赏请求体失败: {e}")))?;
                let title = body.title.trim().to_string();
                if title.is_empty() {
                    return Ok(error_response(400, "悬赏标题不得为空"));
                }
                // 奖励解析（免费/无效货币 → 400）；悬赏必须 >0 且为真实链
                let (reward_sats, currency) =
                    match resolve_price(Some(body.reward_sats), body.currency) {
                        Ok(v) => v,
                        Err(e) => return Ok(error_response(400, &e)),
                    };
                if reward_sats == 0 {
                    return Ok(error_response(400, "悬赏奖励必须 > 0（无偿请求不算悬赏）"));
                }
                let poster = match &caller {
                    Caller::Pubkey { pubkey, .. } => pubkey.clone(),
                    Caller::Admin => body
                        .poster
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "local".to_string()),
                };
                let now = now_iso();
                let b = Bounty {
                    id: new_bounty_id(),
                    title,
                    description: body
                        .description
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default(),
                    tags: body.tags.unwrap_or_default(),
                    poster,
                    reward_sats,
                    currency,
                    target_url: body
                        .target_url
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default(),
                    status: "open".to_string(),
                    claimed_by: String::new(),
                    solution_url: String::new(),
                    deadline: body
                        .deadline
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default(),
                    created_at: now.clone(),
                    updated_at: now,
                    paid_at: String::new(),
                    payout_txid: String::new(),
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_bounty(&conn, &b).map_err(db_err)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&b)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/nexhub/bounty/:id/claim —— hunter 认领（open→claimed）
            //    hunter = token 身份（body 自报忽略）。原子 UPDATE（P1 竞态修复）：
            //    判定与写入合并为单语句，并发认领只有一个成功，后到者 409，
            //    不再出现跨锁段 last-writer-wins 双 200。
            (HttpMethod::Post, ["api", "v1", "nexhub", "bounty", id, "claim"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let hunter = caller.actor().to_string();
                let outcome = {
                    let conn = self.db.lock().expect("db poisoned");
                    claim_bounty(&conn, id, &hunter).map_err(db_err)?
                };
                match outcome {
                    ClaimOutcome::Claimed(b) => Ok(ok_json(to_value(&b)?)),
                    ClaimOutcome::NotFound => Ok(error_response(404, &format!("悬赏不存在: {id}"))),
                    ClaimOutcome::NotOpen(status) => Ok(error_response(
                        409,
                        &format!("仅 open 状态可认领（当前 {status}）"),
                    )),
                }
            }

            // —— POST /api/v1/nexhub/bounty/:id/submit —— hunter 提交交付物
            //    （open 直接认领并提交 / claimed 须本人；submitted/paid/cancelled 拒绝）。
            //    hunter = token 身份（body 自报忽略）；claimed 状态仅 claim 的 hunter
            //    可提交，越权 403（设计 §C）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "bounty", id, "submit"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct SubmitBody {
                    solution_url: String,
                }
                let body: SubmitBody = serde_json::from_value(req.body.clone())
                    .map_err(|e| HandlerError::Internal(format!("解析提交请求体失败: {e}")))?;
                let hunter = caller.actor().to_string();
                let solution = body.solution_url.trim().to_string();
                if solution.is_empty() {
                    return Ok(error_response(400, "solution_url 不得为空"));
                }
                let mut b = {
                    let conn = self.db.lock().expect("db poisoned");
                    match find_bounty(&conn, id).map_err(db_err)? {
                        Some(b) => b,
                        None => return Ok(error_response(404, &format!("悬赏不存在: {id}"))),
                    }
                };
                if b.status == "claimed" && b.claimed_by != hunter {
                    return Ok(error_response(403, "该悬赏已由他人认领"));
                }
                if b.status != "open" && b.status != "claimed" {
                    return Ok(error_response(
                        409,
                        &format!("当前状态 {} 不可提交", b.status),
                    ));
                }
                b.claimed_by = hunter;
                b.solution_url = solution;
                b.status = "submitted".to_string();
                b.updated_at = now_iso();
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_bounty(&conn, &b).map_err(db_err)?;
                }
                Ok(ok_json(to_value(&b)?))
            }

            // —— POST /api/v1/nexhub/bounty/:id/approve —— poster 验收 + 放款
            //    （submitted→paid；复用货币化 verify_payment 校验金额/货币/收据，
            //    再接力链上验真——dApp 一期，同 purchase 的语义表）。
            //    仅 poster 可验收（poster=pubkey 时同 pubkey；存量字符串 poster 的
            //    悬赏仅 admin——设计 §C 身份锁定，修复已知限制②），越权 403。
            //    body 新增可选 pay_to/chain_id/rpc_url（eth 悬赏核验定位用）。
            (HttpMethod::Post, ["api", "v1", "nexhub", "bounty", id, "approve"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct ApproveBody {
                    txid: String,
                    #[serde(default)]
                    amount_sats: Option<u64>,
                    #[serde(default)]
                    currency: Option<String>,
                    /// hunter 收款地址（poster 提供；eth 悬赏链上核验的 expected_to。
                    /// 信任模型：poster 自报地址可自付自证——平台核的是「真有一笔
                    /// 这笔金额的链上转账」，hunter 是否收到由其本人核对 tx）。
                    #[serde(default)]
                    pay_to: Option<String>,
                    /// 链 ID（缺省回落数值 chain → env `NEXOS_EVM_CHAIN_ID`）。
                    #[serde(default)]
                    chain_id: Option<u64>,
                    /// 显式 RPC（poster/admin 自配，候选链第一段）。
                    #[serde(default)]
                    rpc_url: Option<String>,
                    /// ERC-20 合约地址（二期，usdt@EVM 悬赏；缺省回落 env
                    /// `NEXOS_USDT_EVM_CONTRACT`）。
                    #[serde(default)]
                    erc20_contract: Option<String>,
                    /// ERC-20 小数位（二期；缺省回落 env
                    /// `NEXOS_USDT_EVM_DECIMALS`，默认 6）。
                    #[serde(default)]
                    erc20_decimals: Option<u8>,
                }
                let body: ApproveBody = serde_json::from_value(req.body.clone())
                    .map_err(|e| HandlerError::Internal(format!("解析验收请求体失败: {e}")))?;
                let mut b = {
                    let conn = self.db.lock().expect("db poisoned");
                    match find_bounty(&conn, id).map_err(db_err)? {
                        Some(b) => b,
                        None => return Ok(error_response(404, &format!("悬赏不存在: {id}"))),
                    }
                };
                if !caller_owns_bounty(&caller, &b.poster) {
                    return Ok(forbidden_bounty_poster());
                }
                if b.status != "submitted" {
                    return Ok(error_response(
                        409,
                        &format!("仅 submitted 状态可验收（当前 {}）", b.status),
                    ));
                }
                if b.claimed_by.is_empty() {
                    return Ok(error_response(400, "无认领者，无法验收"));
                }
                let currency = body.currency.clone().unwrap_or_else(|| b.currency.clone());
                let amount = body.amount_sats.unwrap_or(b.reward_sats);
                // 自证面校验（金额/货币/txid）→ 链上验真接力（dApp 一期）
                let receipt = Entitlement {
                    repo_name: b.id.clone(),
                    buyer: b.claimed_by.clone(),
                    chain: currency.clone(),
                    txid: body.txid.trim().to_string(),
                    amount_sats: amount,
                    currency: currency.clone(),
                    paid_at: now_iso(),
                    chain_block: None,
                    chain_value_wei: None,
                };
                if let Err(e) = verify_payment(&receipt, b.reward_sats, &b.currency) {
                    return Ok(error_response(402, &e));
                }
                // —— 链上验真：收款方 = body pay_to（hunter 地址；**不回落 env
                //    NEXOS_HUB_PAY_TO**——节点运营者地址会错杀发给 hunter 的真支付）。
                //    amount 即最小货币单位（eth 悬赏 = wei，18 位小数假设）。
                //    金额规则（二期定稿）：**AtLeast**——与自证面「金额足额」
                //    （verify_payment 要求 ≥ 奖励）对齐，放款多打不亏待 hunter。——
                let check = check_chain_payment(
                    &self.chain_verify,
                    &currency,
                    &receipt.txid,
                    &amount.to_string(),
                    &ChainPayHints {
                        chain_id: body.chain_id,
                        chain_str: None,
                        rpc_url: body.rpc_url.as_deref(),
                        pay_to: body.pay_to.as_deref(),
                        fallback_default_pay_to: false,
                        amount_rule: AmountRule::AtLeast,
                        erc20_contract: body.erc20_contract.as_deref(),
                        erc20_decimals: body.erc20_decimals,
                    },
                )
                .await;
                if let ChainPayCheck::Denied { status, reason } = &check {
                    return Ok(error_response(*status, reason));
                }
                if let ChainPayCheck::Verified {
                    block_number,
                    value_wei,
                    ..
                } = &check
                {
                    eprintln!(
                        "[nexhub] 悬赏 {id} 放款核验通过：block={block_number} value={value_wei} wei（链上事实已记入响应，悬赏行不落库）"
                    );
                }
                b.status = "paid".to_string();
                b.paid_at = now_iso();
                b.payout_txid = body.txid.trim().to_string();
                b.updated_at = b.paid_at.clone();
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_bounty(&conn, &b).map_err(db_err)?;
                }
                let mut resp = serde_json::json!({
                    "ok": true,
                    "id": b.id,
                    "winner": b.claimed_by,
                    "reward_sats": b.reward_sats,
                    "currency": b.currency,
                    "payout_txid": b.payout_txid,
                    "paid_at": b.paid_at,
                    "note": "奖励已标记支付（eth 悬赏经链上核验放行，见 chain_verify 标注）",
                });
                if let Some(marker) = chain_verify_json(&check) {
                    if let Some(map) = resp.as_object_mut() {
                        map.insert("chain_verify".into(), marker);
                    }
                }
                Ok(ok_json(resp))
            }

            // —— POST /api/v1/nexhub/bounty/:id/reject —— poster 驳回（submitted→open 重开）
            //    仅 poster 可驳回（同 approve 的身份锁定），越权 403。
            (HttpMethod::Post, ["api", "v1", "nexhub", "bounty", id, "reject"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let mut b = {
                    let conn = self.db.lock().expect("db poisoned");
                    match find_bounty(&conn, id).map_err(db_err)? {
                        Some(b) => b,
                        None => return Ok(error_response(404, &format!("悬赏不存在: {id}"))),
                    }
                };
                if !caller_owns_bounty(&caller, &b.poster) {
                    return Ok(forbidden_bounty_poster());
                }
                if b.status != "submitted" {
                    return Ok(error_response(
                        409,
                        &format!("仅 submitted 状态可驳回（当前 {}）", b.status),
                    ));
                }
                b.status = "open".to_string();
                b.claimed_by = String::new();
                b.solution_url = String::new();
                b.updated_at = now_iso();
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_bounty(&conn, &b).map_err(db_err)?;
                }
                Ok(ok_json(to_value(&b)?))
            }

            // —— POST /api/v1/nexhub/bounty/:id/cancel —— poster 取消（open→cancelled）
            //    仅 poster 可取消（同 approve 的身份锁定），越权 403。
            (HttpMethod::Post, ["api", "v1", "nexhub", "bounty", id, "cancel"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let mut b = {
                    let conn = self.db.lock().expect("db poisoned");
                    match find_bounty(&conn, id).map_err(db_err)? {
                        Some(b) => b,
                        None => return Ok(error_response(404, &format!("悬赏不存在: {id}"))),
                    }
                };
                if !caller_owns_bounty(&caller, &b.poster) {
                    return Ok(forbidden_bounty_poster());
                }
                if b.status != "open" {
                    return Ok(error_response(
                        409,
                        &format!("仅 open 状态可取消（当前 {}）", b.status),
                    ));
                }
                b.status = "cancelled".to_string();
                b.updated_at = now_iso();
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_bounty(&conn, &b).map_err(db_err)?;
                }
                Ok(ok_json(to_value(&b)?))
            }

            // —— POST /api/v1/nexhub/lobby/:repo/pulls —— 创建 PR（链上身份归因）
            //    body: {title, description?, source_branch}；校验 source_branch 已
            //    push 到裸仓（400）；仓库不存在 404。author=token 身份（body 自报
            //    一律忽略）；base_branch 定格为仓库实际默认分支（main→master 回退）。
            //    分支内容经既有 git push 通道提交——本端点只做归因与状态机起步。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", repo, "pulls"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                #[derive(serde::Deserialize)]
                struct CreatePrBody {
                    title: String,
                    #[serde(default)]
                    description: Option<String>,
                    source_branch: String,
                }
                let body: CreatePrBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析 PR 请求体失败: {e}")))?;
                let title = body.title.trim().to_string();
                if title.is_empty() {
                    return Ok(error_response(400, "PR 标题不得为空"));
                }
                let branch = body.source_branch.trim().to_string();
                if let Err(msg) = validate_branch_name(&branch) {
                    return Ok(error_response(400, &msg));
                }
                let dir = self.repos_dir.clone();
                let bare = format!("{dir}/{repo}.git");
                if !Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {repo}")));
                }
                // 分支存在性 + 实际默认分支（一次 blocking 任务内完成）
                let check_bare = bare.clone();
                let check_branch = branch.clone();
                let (exists, base_branch) = tokio::task::spawn_blocking(move || {
                    (
                        pr_branch_exists(&check_bare, &check_branch),
                        resolve_default_branch_sync(&check_bare),
                    )
                })
                .await
                .map_err(|e| HandlerError::Internal(format!("分支校验任务 join 失败: {e}")))?;
                if !exists {
                    return Ok(error_response(
                        400,
                        &format!("source_branch 在仓库中不存在（先 git push 到裸仓）: {branch}"),
                    ));
                }
                let (author_pubkey, author_display) = match &caller {
                    Caller::Pubkey {
                        pubkey,
                        display_name,
                    } => (pubkey.clone(), display_name.clone()),
                    Caller::Admin => ("admin".to_string(), "admin".to_string()),
                };
                let now = now_iso();
                let pr = PullRequest {
                    id: new_pr_id(),
                    repo_name: repo.to_string(),
                    title,
                    description: body
                        .description
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default(),
                    source_branch: branch,
                    source_node: default_source_node(),
                    author_pubkey,
                    author_display,
                    status: "open".to_string(),
                    base_branch,
                    reviewed_by: String::new(),
                    reviewed_at: String::new(),
                    created_at: now.clone(),
                    updated_at: now,
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_pr(&conn, &pr).map_err(db_err)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&pr)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/nexhub/lobby/:repo/pulls —— PR 列表（公开，?status= 过滤）
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby", repo, "pulls"]) => {
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                let status = query
                    .get("status")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                if let Some(s) = status {
                    if !PR_STATUSES.contains(&s) {
                        return Ok(error_response(
                            400,
                            &format!("非法 status（可选 {}）: {s}", PR_STATUSES.join("/")),
                        ));
                    }
                }
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    load_prs(&conn, repo, status).map_err(db_err)?
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/nexhub/lobby/:repo/pulls/:id —— PR 详情（公开，含 diff 摘要）
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby", repo, "pulls", id]) => {
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                let pr = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_pr(&conn, repo, id).map_err(db_err)?
                };
                let Some(pr) = pr else {
                    return Ok(error_response(404, &format!("PR 不存在: {id}")));
                };
                // diff stat（仓库被删/分支被删 → 空串降级，详情仍可看）
                let bare = format!("{}/{repo}.git", self.repos_dir);
                let stat = if Path::new(&bare).is_dir() {
                    let (b, s, t) = (
                        bare.clone(),
                        pr.base_branch.clone(),
                        pr.source_branch.clone(),
                    );
                    tokio::task::spawn_blocking(move || pr_diff_stat_blocking(&b, &s, &t))
                        .await
                        .map_err(|e| HandlerError::Internal(format!("diff 任务 join 失败: {e}")))?
                } else {
                    String::new()
                };
                let mut body = to_value(&pr)?;
                body["diff_stat"] = serde_json::json!(stat);
                Ok(ok_json(body))
            }

            // —— POST /api/v1/nexhub/lobby/:repo/pulls/:id/merge —— 合并 PR
            //    权限：admin 或 repo owner pubkey（大厅条目 publisher=pubkey 且同
            //    pubkey；无大厅条目/存量字符串条目 → 仅 admin）。执行：裸仓
            //    merge-tree 3-way 合成 + commit-tree 双 parent + update-ref 推进
            //    base 分支；冲突 409。已 merged/rejected/closed 的 PR 不可再合并。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", repo, "pulls", id, "merge"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                let (pr, entry) = {
                    let conn = self.db.lock().expect("db poisoned");
                    (find_pr(&conn, repo, id).map_err(db_err)?, {
                        find_entry(&conn, repo).map_err(db_err)?
                    })
                };
                let Some(mut pr) = pr else {
                    return Ok(error_response(404, &format!("PR 不存在: {id}")));
                };
                if !caller_can_review_pr(&caller, entry.as_ref()) {
                    return Ok(forbidden_pr_reviewer());
                }
                if pr.status != "open" {
                    return Ok(error_response(
                        409,
                        &format!("仅 open 状态可合并（当前 {}）", pr.status),
                    ));
                }
                let bare = format!("{}/{repo}.git", self.repos_dir);
                if !Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {repo}")));
                }
                let message = format!("Merge PR {}: {}", pr.id, pr.title);
                let (m_bare, m_base, m_src, m_msg) = (
                    bare.clone(),
                    pr.base_branch.clone(),
                    pr.source_branch.clone(),
                    message,
                );
                let merged = tokio::task::spawn_blocking(move || {
                    merge_pr_blocking(&m_bare, &m_base, &m_src, &m_msg)
                })
                .await
                .map_err(|e| HandlerError::Internal(format!("合并任务 join 失败: {e}")))?;
                let merged_sha = match merged {
                    Ok(sha) => sha,
                    Err(e) => {
                        return Ok(if e.starts_with("合并冲突") {
                            error_response(409, &e)
                        } else {
                            error_response(502, &e)
                        })
                    }
                };
                let now = now_iso();
                pr.status = "merged".to_string();
                pr.reviewed_by = caller.actor().to_string();
                pr.reviewed_at = now.clone();
                pr.updated_at = now;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_pr(&conn, &pr).map_err(db_err)?;
                }
                tracing_like_log(&format!(
                    "nexhub-pr: 合并 {}/{} → {}（by {}）",
                    pr.repo_name, pr.id, pr.base_branch, pr.reviewed_by
                ));
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": pr.id,
                    "repo_name": pr.repo_name,
                    "status": "merged",
                    "base_branch": pr.base_branch,
                    "merged_sha": merged_sha,
                    "reviewed_by": pr.reviewed_by,
                    "reviewed_at": pr.reviewed_at,
                })))
            }

            // —— POST /api/v1/nexhub/lobby/:repo/pulls/:id/reject —— 拒绝 PR
            //    权限同 merge（admin / repo owner pubkey）；body {reason?} 仅回显。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", repo, "pulls", id, "reject"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                #[derive(serde::Deserialize)]
                struct RejectBody {
                    #[serde(default)]
                    reason: Option<String>,
                }
                let body: RejectBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析拒绝请求体失败: {e}")))?;
                let reason = body
                    .reason
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_default();
                let (pr, entry) = {
                    let conn = self.db.lock().expect("db poisoned");
                    (find_pr(&conn, repo, id).map_err(db_err)?, {
                        find_entry(&conn, repo).map_err(db_err)?
                    })
                };
                let Some(mut pr) = pr else {
                    return Ok(error_response(404, &format!("PR 不存在: {id}")));
                };
                if !caller_can_review_pr(&caller, entry.as_ref()) {
                    return Ok(forbidden_pr_reviewer());
                }
                if pr.status != "open" {
                    return Ok(error_response(
                        409,
                        &format!("仅 open 状态可拒绝（当前 {}）", pr.status),
                    ));
                }
                let now = now_iso();
                pr.status = "rejected".to_string();
                pr.reviewed_by = caller.actor().to_string();
                pr.reviewed_at = now.clone();
                pr.updated_at = now;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_pr(&conn, &pr).map_err(db_err)?;
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": pr.id,
                    "status": "rejected",
                    "reviewed_by": pr.reviewed_by,
                    "reviewed_at": pr.reviewed_at,
                    "reason": reason,
                })))
            }

            // —— POST /api/v1/nexhub/lobby/:repo/pulls/:id/close —— 关闭 PR
            //    权限：author 本人（author_pubkey==token pubkey）或 admin；
            //    其余链上身份 403。仅 open 可关闭。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", repo, "pulls", id, "close"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                let mut pr = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_pr(&conn, repo, id).map_err(db_err)?
                };
                let Some(pr) = pr.as_mut() else {
                    return Ok(error_response(404, &format!("PR 不存在: {id}")));
                };
                let allowed = match caller.pubkey() {
                    Some(pk) => pr.author_pubkey == pk,
                    None => true, // admin
                };
                if !allowed {
                    return Ok(forbidden_pr_author());
                }
                if pr.status != "open" {
                    return Ok(error_response(
                        409,
                        &format!("仅 open 状态可关闭（当前 {}）", pr.status),
                    ));
                }
                pr.status = "closed".to_string();
                pr.updated_at = now_iso();
                let saved = pr.clone();
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_pr(&conn, &saved).map_err(db_err)?;
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": saved.id,
                    "status": "closed",
                    "closed_by": caller.actor(),
                })))
            }

            // —— POST /api/v1/nexhub/lobby/:repo/releases —— 创建 release（仅 admin）
            //    body: {tag, title?, notes?}；git tag 到默认分支头 + 落库
            //    hub_releases + 联邦广播（fed=nexhub_release）。链上身份 403。
            (HttpMethod::Post, ["api", "v1", "nexhub", "lobby", repo, "releases"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                // 发版是平台级权限：链上身份（pubkey）一律 403，仅系统 admin
                if caller.pubkey().is_some() {
                    return Ok(forbidden_admin_only());
                }
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                #[derive(serde::Deserialize)]
                struct ReleaseBody {
                    tag: String,
                    #[serde(default)]
                    title: Option<String>,
                    #[serde(default)]
                    notes: Option<String>,
                }
                let body: ReleaseBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析发版请求体失败: {e}")))?;
                let tag = body.tag.trim().to_string();
                if let Err(msg) = validate_tag_name(&tag) {
                    return Ok(error_response(400, &msg));
                }
                let dir = self.repos_dir.clone();
                let bare = format!("{dir}/{repo}.git");
                if !Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {repo}")));
                }
                // 同 (repo,tag) 已发版 → 409（发版不可变；删除后可重发）
                {
                    let conn = self.db.lock().expect("db poisoned");
                    if find_release(&conn, repo, &tag).map_err(db_err)?.is_some() {
                        return Ok(error_response(
                            409,
                            &format!("release 已存在: {repo}/{tag}"),
                        ));
                    }
                }
                let tag_bare = bare.clone();
                let tag_name = tag.clone();
                let tagged =
                    tokio::task::spawn_blocking(move || tag_release_blocking(&tag_bare, &tag_name))
                        .await
                        .map_err(|e| {
                            HandlerError::Internal(format!("打 tag 任务 join 失败: {e}"))
                        })?;
                if let Err(e) = tagged {
                    return Ok(if e.contains("已存在") {
                        error_response(409, &e)
                    } else {
                        error_response(502, &e)
                    });
                }
                let release = Release {
                    id: new_release_id(),
                    repo_name: repo.to_string(),
                    tag: tag.clone(),
                    title: body
                        .title
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| format!("{repo} {tag}")),
                    notes: body
                        .notes
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default(),
                    created_by: caller.actor().to_string(),
                    created_at: now_iso(),
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_release(&conn, &release).map_err(db_err)?;
                }
                // 联邦广播（通道未装配静默跳过——单机部署零开销）
                self.fed.broadcast_release(&release);
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&release)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/nexhub/lobby/:repo/releases —— release 列表（公开）
            (HttpMethod::Get, ["api", "v1", "nexhub", "lobby", repo, "releases"]) => {
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    list_releases(&conn, repo).map_err(db_err)?
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— DELETE /api/v1/nexhub/lobby/:repo/releases/:tag —— 删除 release
            //    （仅 admin；库行 + git tag 一并删除）
            (HttpMethod::Delete, ["api", "v1", "nexhub", "lobby", repo, "releases", tag]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                if caller.pubkey().is_some() {
                    return Ok(forbidden_admin_only());
                }
                if let Err(msg) = validate_lobby_name(repo) {
                    return Ok(error_response(400, &msg));
                }
                if let Err(msg) = validate_tag_name(tag) {
                    return Ok(error_response(400, &msg));
                }
                {
                    let conn = self.db.lock().expect("db poisoned");
                    if find_release(&conn, repo, tag).map_err(db_err)?.is_none() {
                        return Ok(error_response(
                            404,
                            &format!("release 不存在: {repo}/{tag}"),
                        ));
                    }
                }
                let bare = format!("{}/{repo}.git", self.repos_dir);
                if Path::new(&bare).is_dir() {
                    let (d_bare, d_tag) = (bare.clone(), tag.to_string());
                    let _ =
                        tokio::task::spawn_blocking(move || delete_tag_blocking(&d_bare, &d_tag))
                            .await;
                }
                {
                    let conn = self.db.lock().expect("db poisoned");
                    delete_release(&conn, repo, tag).map_err(db_err)?;
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "repo_name": repo,
                    "tag": tag,
                    "action": "release_delete",
                })))
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "nexhub-lobby: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 联邦大厅（P3，docs/NEXOS_P2P_NETWORK_DESIGN.md §8 + docs/NEXHUB_LOBBY_DESIGN.md
// §14）：发布路径经 os-p2p 广播 + 接收端去重写入本地 hub_lobby
// ----------------------------------------------------------------------------

/// 联邦载荷类型标记（`payload.fed == "nexhub_lobby"`）。
pub const FED_KIND_NEXHUB_LOBBY: &str = "nexhub_lobby";

/// 联邦载荷类型标记（`payload.fed == "nexhub_release"`，2026-08-23 发版广播）。
pub const FED_KIND_NEXHUB_RELEASE: &str = "nexhub_release";

/// 联邦传输通道：os-api 装配层注入 os-p2p 广播实现（**os-nexhub 不依赖
/// os-p2p**——审计 §6 独立性红线，通道抽象反转依赖方向）。
///
/// 语义：fire-and-forget 把载荷发给**所有已连接 peer**（实现方负责 fan-out）；
/// 未连接/失败静默丢弃（联邦是尽力而为的传播，不是可靠队列）。
pub trait LobbyFedTransport: Send + Sync {
    /// 广播一条联邦载荷给全部已连接 peer。
    fn broadcast(&self, payload: serde_json::Value);
}

/// 联邦节点名净化：空/超长（>64 字符）回退 `"peer"`——payload 的 `node` 字段
/// 来自对端自报，写库前限幅防病态值。
#[must_use]
pub fn sanitize_fed_node(node: &str) -> String {
    let n = node.trim();
    if n.is_empty() || n.chars().count() > 64 {
        "peer".to_string()
    } else {
        n.to_string()
    }
}

/// 构造 NexHub 联邦广播载荷（纯函数，发送端与测试共用）：
/// `{"fed":"nexhub_lobby","node":<发布节点>,"entry":{...完整 LobbyEntry JSON...}}`。
#[must_use]
pub fn build_nexhub_lobby_fed_payload(node: &str, entry: &LobbyEntry) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_NEXHUB_LOBBY,
        "node": sanitize_fed_node(node),
        "entry": entry,
    })
}

/// 构造发版联邦广播载荷（纯函数，发送端与测试共用）：
/// `{"fed":"nexhub_release","node":<发版节点>,"release":{...完整 Release JSON...}}`。
#[must_use]
pub fn build_nexhub_release_fed_payload(node: &str, release: &Release) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_NEXHUB_RELEASE,
        "node": sanitize_fed_node(node),
        "release": release,
    })
}

/// [`LobbyFedEndpoint::ingest`] 的处置结果（测试/诊断观测面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyFedIngest {
    /// 新条目已写入（source_node = 来源节点）。
    Written,
    /// 同源（repo_name+source_node 相同）重发 → 刷新快照，保留本地 download_count。
    Refreshed,
    /// 内存缓存命中重复（同 repo+node+**published_at**，即逐字节相同的重放），
    /// 未触碰 DB。同源**新快照**（published_at 已变）不算重复——穿透到 DB
    /// 走 [`LobbyFedIngest::Refreshed`]（2026-08-23 修复，见 [`LobbyFedEndpoint::ingest`]）。
    Duplicate,
    /// 本地已有同名条目且来源不同（本地/他节点）→ 保护本地条目，跳过。
    Skipped,
    /// 载荷非法（缺字段/name 非法/entry 解析失败），丢弃。
    Invalid,
}

/// 大厅联邦端点——`Arc` 共享给 os-api 装配层（p2p 接收端）与 handler 发布路径：
///
/// - **发送端**：[`Self::broadcast_entry`]（两步联邦第二步——`POST
///   /:name/federate` 推送本地已发布条目；本地 publish 只写本地不广播，条目
///   `federated` 标志随推送置位供前端 🌐 标记）；
/// - **接收端**：[`Self::ingest`]（os-api 的 FederationBridge 对 `fed ==
///   "nexhub_lobby"` 载荷调用）——去重（内存 `repo+node` 缓存 + DB 权威判定）
///   → 写本地 hub_lobby（`source_node` 标记来源）。
///
/// 与 handler 共享同一 `Arc<Mutex<Connection>>`（锁语义与重构前一致：短锁快放，
/// 不跨 await）。
pub struct LobbyFedEndpoint {
    db: Arc<Mutex<Connection>>,
    /// 注入的联邦传输通道 + 本节点名（None = 未装配 os-p2p，广播静默跳过）。
    transport: Mutex<Option<(Arc<dyn LobbyFedTransport>, String)>>,
    /// 近期已见联邦条目键（`repo\0node\0published_at`）内存缓存，容量
    /// [`FED_SEEN_LIMIT`]。键含 `published_at`：只拦逐字节相同的**重放**；
    /// 同源**新快照**（发布侧重新 publish）键不同 → 穿透到 DB 权威路径
    /// （`Refreshed`）——否则对端刷新快照永远到不了本节点（2026-08-23 修复）。
    seen: Mutex<std::collections::VecDeque<String>>,
    /// 仓库根目录（本地 bare 副本落点 `<repos_root>/<name>.git`——nexos 自动
    /// 跟随拉取的更新目标；与 handler 同源注入，测试可指向临时目录）。
    repos_root: String,
    /// nexos 本地副本自动拉取节流登记（repo → 上次触发时刻）：同一仓库
    /// [`AUTO_PULL_THROTTLE`] 内最多触发一次后台拉取（10 分钟防抖）。
    auto_pull_last: Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// 内存去重缓存容量（最近 1000 条——超出丢最旧，DB 判定兜底）。
const FED_SEEN_LIMIT: usize = 1000;

/// nexos 本地 bare 副本自动跟随的节流窗口：同一仓库两次触发之间最少间隔
/// 10 分钟——快照风暴（对端短时间多次 push → 多次重广播）只兑现最近一次，
/// 防抖不追帧（下一个窗口总会再同步到最新）。
const AUTO_PULL_THROTTLE: std::time::Duration = std::time::Duration::from_secs(600);

/// nexos 本地副本自动跟随的总开关 env 名（`=0` 关闭，缺省/其他值开启）。
const AUTO_PULL_ENV: &str = "NEXOS_LOBBY_AUTO_PULL";

/// 自动跟随是否启用（读 env [`AUTO_PULL_ENV`]，每次调度即时读取——运维
/// 改环境变量重启即生效；默认开）。
fn auto_pull_enabled() -> bool {
    std::env::var(AUTO_PULL_ENV).as_deref() != Ok("0")
}

impl LobbyFedEndpoint {
    fn new(db: Arc<Mutex<Connection>>, repos_root: &str) -> Self {
        Self {
            db,
            transport: Mutex::new(None),
            seen: Mutex::new(std::collections::VecDeque::new()),
            repos_root: repos_root.to_string(),
            auto_pull_last: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// nexos 本地 bare 副本自动跟随（2026-08-27，链路最后一环）：
    ///
    /// 源节点 push → post-receive → 重 publish + federate 广播新快照；消费端
    /// [`Self::ingest`] 落库后大厅条目已是最新，但**本地 `/…/git-repos/nexos.git`
    /// bare 副本仍停留旧提交**——用户从本节点 NexHub clone 到的是旧代码。本方法
    /// 在 Written/Refreshed 落地成功后由 ingest 触发：
    ///
    /// - **仅内置主仓** [`SEED_REPO`]（其他联邦仓不跟随——用户未要求全量同步）；
    /// - 拉取源**只用快照自带信号**（与一键克隆同构：source_url 本机存在走
    ///   本地直拉 10s；否则 clone_url_http 跨节点 HTTP 拉 120s；皆无 → 静默跳过，
    ///   不 spawn 无谓线程）；
    /// - **节流**：同仓库 [`AUTO_PULL_THROTTLE`]（10 分钟）内最多触发一次；
    /// - 真正的 git 操作全部投递到**无锁后台任务**（ingest 持 DB 锁期间零阻塞、
    ///   失败静默日志不影响返回值，下个快照再试）；
    /// - 总开关 env [`AUTO_PULL_ENV`]`=0` 关闭（默认开）。
    ///
    /// 后台任务内部再做两级省流判定（见 [`run_auto_pull_job`]）。
    fn schedule_nexos_auto_pull(&self, entry: &LobbyEntry) {
        if !auto_pull_enabled() {
            return;
        }
        if entry.repo_name != SEED_REPO {
            return; // 只跟内置主仓，其他联邦仓保持手动
        }
        // 解析拉取源（只用快照信号；解析不出 → 不占用节流窗口直接跳过）
        let Some(source) = resolve_auto_pull_source(entry) else {
            tracing_like_log("nexhub-fed: nexos 快照无可达拉取源（source_url 不在本机且无 clone_url_http），跳过副本跟随");
            return;
        };
        let now = std::time::Instant::now();
        if !self.try_acquire_auto_pull_slot(&entry.repo_name, now) {
            tracing_like_log(&format!(
                "nexhub-fed: nexos 副本跟随节流中（{}s 内已触发过），本次跳过",
                AUTO_PULL_THROTTLE.as_secs()
            ));
            return;
        }
        tracing_like_log(&format!(
            "nexhub-fed: 触发 nexos 本地副本跟随拉取（目标 {}/{}.git ← {}）",
            self.repos_root, entry.repo_name, source.url
        ));
        spawn_detached_future(run_auto_pull_job(
            self.repos_root.clone(),
            entry.clone(),
            source,
        ));
    }

    /// 节流窗口占位（登记即占坑）：同仓库距上次触发不足 [`AUTO_PULL_THROTTLE`]
    /// → false（不触发）；否则登记 now 并放行。纯内存判定，供单元测试注入
    /// 人造时钟验证边界。
    fn try_acquire_auto_pull_slot(&self, repo: &str, now: std::time::Instant) -> bool {
        let mut map = self.auto_pull_last.lock().expect("auto-pull slot poisoned");
        match map.get(repo) {
            Some(t) if now.duration_since(*t) < AUTO_PULL_THROTTLE => false,
            _ => {
                map.insert(repo.to_string(), now);
                true
            }
        }
    }

    /// 注入联邦传输通道 + 本节点名（os-api main.rs 装配：p2p spawn 成功后调用；
    /// 重复注入覆盖旧通道——测试/热替换友好）。
    ///
    /// 注入后**补推 nexos 常驻条目**（自动联邦的装配序缺口）：生产装配顺序是
    /// 先构造 handler（常驻发布 + federated=true，此刻通道未装配 → 广播跳过）
    /// 再起 p2p 注入通道——补推让「nexos 一启动就在联邦大厅」真正到达对端。
    pub fn set_transport(&self, transport: Arc<dyn LobbyFedTransport>, node: String) {
        *self.transport.lock().expect("fed transport poisoned") =
            Some((transport, sanitize_fed_node(&node)));
        self.push_federated_seed();
    }

    /// 补推常驻 nexos 条目：本地发布的（source_node=local）且已置联邦标志的
    /// 常驻条目广播一次（无条目/未联邦/env 逃生口 → no-op）。幂等安全：重复
    /// 注入通道只会重发快照，接收端同源 Refreshed 语义兜底。
    fn push_federated_seed(&self) {
        if auto_publish_disabled() {
            return; // 逃生口：常驻发布与联邦一并停用
        }
        let entry = {
            let conn = self.db.lock().expect("db poisoned");
            find_entry(&conn, SEED_REPO).ok().flatten()
        };
        if let Some(entry) = entry {
            if entry.source_node == default_source_node() && entry.federated {
                self.broadcast_entry(&entry);
            }
        }
    }

    /// 是否已装配传输通道（未装配时发布不联邦——单机部署零开销）。
    #[must_use]
    pub fn is_federated(&self) -> bool {
        self.transport
            .lock()
            .expect("fed transport poisoned")
            .is_some()
    }

    /// 发布路径联邦广播：构造载荷 → transport 广播给全部已连接 peer。
    ///
    /// 未装配通道（P2P 未启用）静默跳过；推送资格（owner pubkey / admin，条目
    /// 须已在本地大厅）由调用方（handler 的 federate 端点）裁决——本方法不重复判定。
    ///
    /// 观测日志（与 IM 侧 `[fed]` 面同款语义，journalctl 可查）：广播时记一条
    /// 条目名——联邦"发了没有"不再只能靠对端日志反推。
    pub fn broadcast_entry(&self, entry: &LobbyEntry) {
        let guard = self.transport.lock().expect("fed transport poisoned");
        let Some((transport, node)) = guard.as_ref() else {
            tracing_like_log(&format!(
                "nexhub-fed: 跳过广播 {}（P2P 通道未装配）",
                entry.repo_name
            ));
            return; // P2P 未启用：静默跳过（不阻塞本地发布语义）
        };
        tracing_like_log(&format!(
            "nexhub-fed: 广播条目 {}（node={node}）",
            entry.repo_name
        ));
        transport.broadcast(build_nexhub_lobby_fed_payload(node, entry));
    }

    /// 发版联邦广播（`POST /:repo/releases` 创建成功后调用）：构造 `nexhub_release`
    /// 载荷 → transport 广播。未装配通道静默跳过（单机部署零开销）。
    pub fn broadcast_release(&self, release: &Release) {
        let guard = self.transport.lock().expect("fed transport poisoned");
        let Some((transport, node)) = guard.as_ref() else {
            tracing_like_log(&format!(
                "nexhub-fed: 跳过广播 release {}/{}（P2P 通道未装配）",
                release.repo_name, release.tag
            ));
            return;
        };
        tracing_like_log(&format!(
            "nexhub-fed: 广播 release {}/{}（node={node}）",
            release.repo_name, release.tag
        ));
        transport.broadcast(build_nexhub_release_fed_payload(node, release));
    }

    /// 接收端：解析联邦载荷 → 去重 → 写本地 hub_lobby（`source_node` 标记来源）。
    ///
    /// 载荷契约 `{"fed":"nexhub_lobby","node":<来源节点>,"entry":{LobbyEntry}}`：
    /// - 非 nexhub_lobby / 缺 node / entry 解析失败 / repo_name 非法 → `Invalid`；
    /// - **完全相同载荷**（`repo+node+published_at` 内存缓存命中）→ `Duplicate`
    ///   （不触碰 DB）；
    /// - DB 无同名条目 → 写入（`source_node=node`，本地克隆计数清零起步）→ `Written`；
    /// - DB 有同名条目且同 source_node（同源重发=对端刷新快照）→ 覆盖刷新，
    ///   保留本地 `download_count` → `Refreshed`；
    /// - DB 有同名条目但来源不同（本地发布或他节点先到）→ `Skipped`（保护本地）。
    ///
    /// **缓存键含 `published_at`（修复 2026-08-23）**：发布路径每次 publish 都
    /// 重新生成 `published_at`（`now_iso()`），故"同源刷新快照"的载荷键必不同
    /// → 穿透缓存落 DB 权威路径（`Refreshed`）。修复前键只有 `repo+node`，
    /// 首次收件后同源重发在缓存存续期内（1000 条/重启前）一律被判 `Duplicate`
    /// 丢弃——对端推了新提交，本节点大厅永远停留在旧快照（`Refreshed` 分支
    /// 实际不可达，仅重启后偶发触发）。缓存仍拦得住的是**逐字节相同的重放**
    /// （p2p 层重投递），其去重语义不受影响。
    ///
    /// 写入路径与 REST 发布同构（insert_entry 的 INSERT OR REPLACE），锁内
    /// 同步执行不跨 await。各处置结果均打 `[os-nexhub]` 日志（journalctl 可查）。
    ///
    /// **副本自动跟随**（2026-08-27）：Written/Refreshed 落地成功后，对内置
    /// 主仓 nexos 触发本地 bare 副本后台拉取（节流 10 分钟 + hash 判等省流，
    /// 见 [`Self::schedule_nexos_auto_pull`]）——源节点 push 重广播后，本节点
    /// NexHub clone 出来的 nexos 即最新提交（链路最后一环：大厅显示已随快照
    /// 更新，本地副本此前停留在旧 commit）。git 操作全部在无锁后台执行，
    /// 不阻塞 ingest、不影响返回值。
    pub fn ingest(&self, payload: &serde_json::Value) -> LobbyFedIngest {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_NEXHUB_LOBBY) {
            return LobbyFedIngest::Invalid;
        }
        let node = sanitize_fed_node(
            payload
                .get("node")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        if node == "peer" {
            return LobbyFedIngest::Invalid; // node 缺失（空串被净化为 peer）→ 非法
        }
        let Some(entry_val) = payload.get("entry") else {
            return LobbyFedIngest::Invalid;
        };
        let Ok(mut entry) = serde_json::from_value::<LobbyEntry>(entry_val.clone()) else {
            return LobbyFedIngest::Invalid;
        };
        if validate_repo_name(&entry.repo_name).is_err() {
            return LobbyFedIngest::Invalid; // 路径穿越/非法名防护（与本地发布同规则）
        }
        // 来源标记覆盖：条目自身的 source_node（origin 恒 local）改写为发布节点
        entry.source_node = node.clone();
        // 去重键含 published_at：相同载荷（重放）→ Duplicate；新快照（发布侧
        // 重新 publish → published_at 变化）→ 穿透到 DB 权威判定（见方法文档）。
        let key = format!(
            "{}\u{0}{}\u{0}{}",
            entry.repo_name, node, entry.published_at
        );
        {
            let mut seen = self.seen.lock().expect("fed seen poisoned");
            if seen.contains(&key) {
                tracing_like_log(&format!(
                    "nexhub-fed: 重复载荷丢弃 {} ← {node}（重放）",
                    entry.repo_name
                ));
                return LobbyFedIngest::Duplicate;
            }
            seen.push_back(key);
            while seen.len() > FED_SEEN_LIMIT {
                seen.pop_front();
            }
        }
        let conn = self.db.lock().expect("db poisoned");
        match find_entry(&conn, &entry.repo_name) {
            Ok(None) => {
                // 新条目：本地克隆计数从 0 起步（对端的计数是它的活跃度）
                entry.download_count = 0;
                insert_entry(&conn, &entry).map_or(LobbyFedIngest::Invalid, |_| {
                    tracing_like_log(&format!(
                        "nexhub-fed: 收远程条目 {repo} ← {node}",
                        repo = entry.repo_name
                    ));
                    self.schedule_nexos_auto_pull(&entry);
                    LobbyFedIngest::Written
                })
            }
            Ok(Some(old)) if old.source_node == node => {
                // 同源重发 = 对端刷新快照：覆盖刷新但保留本地 download_count
                entry.download_count = old.download_count;
                insert_entry(&conn, &entry).map_or(LobbyFedIngest::Invalid, |_| {
                    tracing_like_log(&format!(
                        "nexhub-fed: 收远程刷新 {repo} ← {node}（保留本地计数）",
                        repo = entry.repo_name
                    ));
                    self.schedule_nexos_auto_pull(&entry);
                    LobbyFedIngest::Refreshed
                })
            }
            Ok(Some(_)) => {
                tracing_like_log(&format!(
                    "nexhub-fed: 跳过远程条目 {} ← {node}（本地已有同名条目，来源受保护）",
                    entry.repo_name
                ));
                LobbyFedIngest::Skipped
            }
            Err(_) => LobbyFedIngest::Invalid,
        }
    }

    /// 接收端：解析发版联邦载荷 → 去重 → 写本地 hub_releases（仅落元数据行，
    /// **不**在对端执行 git tag——远端可能尚未克隆仓库内容；tag 随仓库同步）。
    ///
    /// 载荷契约 `{"fed":"nexhub_release","node":<来源节点>,"release":{Release}}`：
    /// - 非 nexhub_release / 缺 node / release 解析失败 / repo_name/tag 非法 → `Invalid`；
    /// - 逐字节相同载荷（缓存命中）→ `Duplicate`；
    /// - 本地无同 (repo,tag) → 落地（保留原 id——幂等重放安全）→ `Written`；
    /// - 已有**同 id** 行（同源重发）→ `Refreshed`（覆盖刷新，幂等）；
    /// - 已有**不同 id** 的同 (repo,tag) 行（本地先发版）→ `Skipped`（保护本地）。
    pub fn ingest_release(&self, payload: &serde_json::Value) -> LobbyFedIngest {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_NEXHUB_RELEASE) {
            return LobbyFedIngest::Invalid;
        }
        let node = sanitize_fed_node(
            payload
                .get("node")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        if node == "peer" {
            return LobbyFedIngest::Invalid;
        }
        let Some(rel_val) = payload.get("release") else {
            return LobbyFedIngest::Invalid;
        };
        let Ok(release) = serde_json::from_value::<Release>(rel_val.clone()) else {
            return LobbyFedIngest::Invalid;
        };
        if validate_repo_name(&release.repo_name).is_err()
            || validate_tag_name(&release.tag).is_err()
        {
            return LobbyFedIngest::Invalid;
        }
        // 去重键带 `rel:` 前缀 + node（与条目键空间/语义隔离）：拦逐字节重放，
        // 新 id 穿透 DB 权威判定。
        let key = format!(
            "rel:\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
            release.repo_name, node, release.tag, release.id
        );
        {
            let mut seen = self.seen.lock().expect("fed seen poisoned");
            if seen.contains(&key) {
                tracing_like_log(&format!(
                    "nexhub-fed: 重复 release 载荷丢弃 {}/{} ← {node}（重放）",
                    release.repo_name, release.tag
                ));
                return LobbyFedIngest::Duplicate;
            }
            seen.push_back(key);
            while seen.len() > FED_SEEN_LIMIT {
                seen.pop_front();
            }
        }
        let conn = self.db.lock().expect("db poisoned");
        match find_release(&conn, &release.repo_name, &release.tag) {
            Ok(None) => insert_release(&conn, &release).map_or(LobbyFedIngest::Invalid, |_| {
                tracing_like_log(&format!(
                    "nexhub-fed: 收远程 release {}/{} ← {node}",
                    release.repo_name, release.tag
                ));
                LobbyFedIngest::Written
            }),
            Ok(Some(old)) if old.id == release.id => {
                insert_release(&conn, &release).map_or(LobbyFedIngest::Invalid, |_| {
                    tracing_like_log(&format!(
                        "nexhub-fed: 收远程 release 刷新 {}/{} ← {node}",
                        release.repo_name, release.tag
                    ));
                    LobbyFedIngest::Refreshed
                })
            }
            Ok(Some(_)) => {
                tracing_like_log(&format!(
                    "nexhub-fed: 跳过远程 release {}/{} ← {node}（本地同 tag 先到，受保护）",
                    release.repo_name, release.tag
                ));
                LobbyFedIngest::Skipped
            }
            Err(_) => LobbyFedIngest::Invalid,
        }
    }
}

/// 极简日志（os-nexhub 无 tracing 依赖——eprintln 与本模块其余降级日志同款）。
fn tracing_like_log(msg: &str) {
    eprintln!("[os-nexhub] {msg}");
}

// ----------------------------------------------------------------------------
// 服务端 git clone（async，本机 10s / 联邦 HTTP 120s 超时）
// ----------------------------------------------------------------------------

/// 本机克隆超时（秒）：本地路径 clone 与本机条目自报的 http/ssh 远端（设计
/// 文档 §5/§6 一期内置兜底）。
const CLONE_TIMEOUT_SECS: u64 = 10;

/// 联邦 HTTP 克隆超时（秒）：消费节点经 `/git/*` Smart HTTP 从源节点跨网络
/// 拉取（大仓/慢链路），比本机 10s 宽——10s 掐死的恰恰是跨节点拉取的主路径。
const FED_CLONE_TIMEOUT_SECS: u64 = 120;

/// spawn `git clone --bare <source> <target>`（`timeout_secs` 超时；kill_on_drop
/// 保证超时后子进程被回收；GIT_TERMINAL_PROMPT=0 防凭据交互挂起）。
async fn spawn_git_clone_bare(source: &str, target: &str, timeout_secs: u64) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone")
        .arg("--bare")
        .arg(source)
        .arg(target)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("`git` 调用失败（未安装？）: {e}"))?;
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Err(_) => Err(format!(
            "git clone 超时（{timeout_secs}s），已终止: {source}"
        )),
        Ok(Err(e)) => Err(format!("git clone 等待失败: {e}")),
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "git clone 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
    }
}

// ----------------------------------------------------------------------------
// nexos 本地 bare 副本自动跟随（联邦刷新 → 后台 fetch/clone，2026-08-27）
// ----------------------------------------------------------------------------

/// 自动跟随的拉取源解析结果：URL + 配套超时（本机路径 10s / 跨节点 HTTP 120s，
/// 与一键克隆 [`NexHubLobbyRouteHandler::clone_entry_async`] 同档）。
struct AutoPullSource {
    url: String,
    timeout_secs: u64,
}

/// 从快照解析副本跟随的拉取源——**只用快照自带信号**（与 `select_clone_source`
/// 同构，但从不 fallback 空串）：
///
/// - `source_url` 非空且本机存在该路径 → 本地直拉（同布局跨节点 / 源节点自身
///   场景），10s 超时；
/// - 否则 `clone_url_http` 非空 → 联邦 HTTP 拉（消费节点主路径），120s 超时；
/// - 两者皆无 → None（调度端直接跳过，不 spawn）。
fn resolve_auto_pull_source(entry: &LobbyEntry) -> Option<AutoPullSource> {
    if !entry.source_url.is_empty() && Path::new(&entry.source_url).exists() {
        return Some(AutoPullSource {
            url: entry.source_url.clone(),
            timeout_secs: CLONE_TIMEOUT_SECS,
        });
    }
    let http = entry.clone_url_http.trim();
    if !http.is_empty() {
        return Some(AutoPullSource {
            url: http.to_string(),
            timeout_secs: FED_CLONE_TIMEOUT_SECS,
        });
    }
    None
}

/// 副本跟随的后台执行结果（[`run_auto_pull_job`] 产物，测试/日志观测面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoPullOutcome {
    /// 本地 HEAD 已等于快照 short_hash → 跳过 fetch（省流量）。
    HeadMatchSkipped,
    /// 对既有 bare 副本 `git fetch --prune` 更新分支引用。
    Fetched,
    /// 本地无副本 → 完整 `git clone --bare` 落地。
    Cloned,
}

/// 运行时无关的后台任务投递：tokio 上下文内走 `Handle::spawn`；无上下文
/// （同步 ingest 调用方/单测线程）兜底起独立线程自建一次性 runtime 执行。
fn spawn_detached_future<F>(job: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(job);
        }
        Err(_) => {
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(job);
                }
            });
        }
    }
}

/// 通用 spawn `git <args>`（timeout_secs 超时 kill 兜底、GIT_TERMINAL_PROMPT=0
/// 防凭据挂起），成功返回 trim 后 stdout。与 [`spawn_git_clone_bare`] 同款
/// 子进程纪律（kill_on_drop + 全管道 + stdin null），供 fetch / rev-parse 复用。
async fn spawn_git_run(args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // HTTP 拉取低速熔断：30s 内 <1KiB/s 视为链路僵死提前退出（比整段
        // 超时更快止损；本地路径/file:// 不受影响）。
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
        .env("GIT_HTTP_LOW_SPEED_TIME", "30")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("`git` 调用失败（未安装？）: {e}"))?;
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Err(_) => Err(format!("git {args:?} 超时（{timeout_secs}s），已终止")),
        Ok(Err(e)) => Err(format!("git {args:?} 等待失败: {e}")),
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                Err(format!(
                    "git {args:?} 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
    }
}

/// bare 副本 HEAD 短 hash（7 位，与快照 latest_commit.short_hash 同宽）；
/// 空仓库/非法仓 → None（调用方放弃判等直接 fetch，幂等无害）。
async fn bare_head_short_hash(repo_dir: &str) -> Option<String> {
    spawn_git_run(&["-C", repo_dir, "rev-parse", "--short=7", "HEAD"], 10)
        .await
        .ok()
}

/// nexos 副本跟随后台任务（由 [`LobbyFedEndpoint::schedule_nexos_auto_pull`
/// ] 经 [`spawn_detached_future`] 投递；DB 锁已释放，可自由做文件/git 操作）：
///
/// 1. 目标 `<repos_root>/nexos.git` **已存在** → 先比对本地 HEAD 与快照
///    `latest_commit.short_hash`，相同即跳过（省流量）；不同则对副本执行
///    `git -C <dir> fetch <source> "+refs/heads/*:refs/heads/*"
///    "+refs/tags/*:refs/tags/*" --prune` ——bare 仓可直接 fetch 推进分支引用
///    （HEAD 所指分支随引用更新）；**tag refspec 为显式强制镜像**（2026-09-03
///    补：发版即 tag，heads-only refspec 下 tag 只能靠 git 的机会主义
///    auto-follow——不保证覆盖旧对象上的 tag、绝不更新已存在/被强推的 tag，
///    下游副本 refs/tags 可能永远为空 → 更新检查读 tag 失明。显式
///    `+refs/tags/*` 保证 tag 必达、`-f` 强推形态（release.sh `tag -fa` +
///    `push -f`）被强制对齐、源侧删 tag 时随 `--prune` 镜像清理）。
/// 2. 目标不存在 → 走完整 `git clone --bare`（首次联邦收件即落副本）。
///
/// 失败仅静默日志（下个快照再试），绝不 panic/影响 ingest 返回值。
async fn run_auto_pull_job(repos_root: String, entry: LobbyEntry, source: AutoPullSource) {
    let outcome = run_auto_pull_inner(&repos_root, &entry, &source).await;
    match outcome {
        Ok(AutoPullOutcome::HeadMatchSkipped) => {
            tracing_like_log("nexhub-fed: nexos 副本已是快照提交（HEAD 判等命中），跳过 fetch")
        }
        Ok(AutoPullOutcome::Fetched) => {
            tracing_like_log("nexhub-fed: nexos 副本 fetch 完成（分支引用已推进）")
        }
        Ok(AutoPullOutcome::Cloned) => {
            tracing_like_log("nexhub-fed: nexos 副本不存在 → 完整 clone 落地")
        }
        Err(e) => tracing_like_log(&format!(
            "nexhub-fed: nexos 副本跟随失败（静默，待下个快照重试）: {e}"
        )),
    }
}

/// [`run_auto_pull_job`] 的实质逻辑（独立纯化便于测试直调拿回结果）。
async fn run_auto_pull_inner(
    repos_root: &str,
    entry: &LobbyEntry,
    source: &AutoPullSource,
) -> Result<AutoPullOutcome, String> {
    let target = format!("{repos_root}/{}.git", entry.repo_name);
    if Path::new(&target).exists() {
        // hash 判等省流：本地 HEAD == 快照 short_hash → 无需 fetch
        if let Some(want) = entry.latest_commit.as_ref().map(|c| c.short_hash.as_str()) {
            if let Some(local) = bare_head_short_hash(&target).await {
                if local == want {
                    return Ok(AutoPullOutcome::HeadMatchSkipped);
                }
            }
        }
        spawn_git_run(
            &[
                "-C",
                &target,
                "fetch",
                &source.url,
                "+refs/heads/*:refs/heads/*",
                // tag 显式镜像（发版即 tag：显式 refspec 保证必达 + 强推对齐；
                // 见 fn 文档注释。auto-follow 机会主义语义不可依赖）
                "+refs/tags/*:refs/tags/*",
                "--prune",
            ],
            source.timeout_secs,
        )
        .await?;
        Ok(AutoPullOutcome::Fetched)
    } else {
        std::fs::create_dir_all(repos_root)
            .map_err(|e| format!("创建仓库根目录 {repos_root} 失败: {e}"))?;
        spawn_git_clone_bare(&source.url, &target, source.timeout_secs).await?;
        Ok(AutoPullOutcome::Cloned)
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// `POST /api/v1/nexhub/auth/challenge`（公开：签发挑战 nonce，与 IM 同款契约）
const PATH_AUTH_CHALLENGE: &str = "/api/v1/nexhub/auth/challenge";
/// `POST /api/v1/nexhub/auth/verify`（公开：验签 + 签发 nexhub token，24h）
const PATH_AUTH_VERIFY: &str = "/api/v1/nexhub/auth/verify";
/// `GET /api/v1/nexhub/lobby`
const PATH_LIST: &str = "/api/v1/nexhub/lobby";
/// `GET /api/v1/nexhub/lobby/stats`
const PATH_STATS: &str = "/api/v1/nexhub/lobby/stats";
/// `GET /api/v1/nexhub/lobby/:name`
const PATH_DETAIL: &str = "/api/v1/nexhub/lobby/:name";
/// `POST /api/v1/nexhub/lobby/publish`
const PATH_PUBLISH: &str = "/api/v1/nexhub/lobby/publish";
/// `POST /api/v1/nexhub/lobby/:name/federate`（两步联邦：推送本地已发布条目到联邦大厅）
const PATH_FEDERATE: &str = "/api/v1/nexhub/lobby/:name/federate";
/// `DELETE /api/v1/nexhub/lobby/:name`
const PATH_UNPUBLISH: &str = "/api/v1/nexhub/lobby/:name";
/// `POST /api/v1/nexhub/lobby/:name/clone`
const PATH_CLONE: &str = "/api/v1/nexhub/lobby/:name/clone";
/// `POST /api/v1/nexhub/lobby/:name/purchase`（付费条目：克隆前取得授权）
const PATH_PURCHASE: &str = "/api/v1/nexhub/lobby/:name/purchase";
/// `GET /api/v1/nexhub/lobby/entitlements`（授权记录查询，`?repo=` `?buyer=`；任意已认证）
const PATH_ENTITLEMENTS: &str = "/api/v1/nexhub/lobby/entitlements";

/// `GET /api/v1/nexhub/bounty`（悬赏列表，`?status=` `?q=`）
const PATH_BOUNTY_LIST: &str = "/api/v1/nexhub/bounty";
/// `GET /api/v1/nexhub/bounty/:id`（悬赏详情）
const PATH_BOUNTY_DETAIL: &str = "/api/v1/nexhub/bounty/:id";
/// `POST /api/v1/nexhub/bounty`（发布悬赏，奖励必须 >0）
const PATH_BOUNTY_CREATE: &str = "/api/v1/nexhub/bounty";
/// `POST /api/v1/nexhub/bounty/:id/claim`（hunter 认领，open→claimed）
const PATH_BOUNTY_CLAIM: &str = "/api/v1/nexhub/bounty/:id/claim";
/// `POST /api/v1/nexhub/bounty/:id/submit`（hunter 提交交付物，claimed/open→submitted）
const PATH_BOUNTY_SUBMIT: &str = "/api/v1/nexhub/bounty/:id/submit";
/// `POST /api/v1/nexhub/bounty/:id/approve`（poster 验收 + 自证支付，submitted→paid）
const PATH_BOUNTY_APPROVE: &str = "/api/v1/nexhub/bounty/:id/approve";
/// `POST /api/v1/nexhub/bounty/:id/reject`（poster 驳回，submitted→open 重开）
const PATH_BOUNTY_REJECT: &str = "/api/v1/nexhub/bounty/:id/reject";
/// `POST /api/v1/nexhub/bounty/:id/cancel`（poster 取消，open→cancelled）
const PATH_BOUNTY_CANCEL: &str = "/api/v1/nexhub/bounty/:id/cancel";

/// `GET/POST /api/v1/nexhub/lobby/:repo/pulls`（PR 列表公开 / 创建链上身份归因）
const PATH_PULLS: &str = "/api/v1/nexhub/lobby/:repo/pulls";
/// `GET /api/v1/nexhub/lobby/:repo/pulls/:id`（PR 详情含 diff stat）
const PATH_PULL_DETAIL: &str = "/api/v1/nexhub/lobby/:repo/pulls/:id";
/// `POST /api/v1/nexhub/lobby/:repo/pulls/:id/merge`（合并，admin/repo owner）
const PATH_PULL_MERGE: &str = "/api/v1/nexhub/lobby/:repo/pulls/:id/merge";
/// `POST /api/v1/nexhub/lobby/:repo/pulls/:id/reject`（拒绝，admin/repo owner）
const PATH_PULL_REJECT: &str = "/api/v1/nexhub/lobby/:repo/pulls/:id/reject";
/// `POST /api/v1/nexhub/lobby/:repo/pulls/:id/close`（关闭，author/admin）
const PATH_PULL_CLOSE: &str = "/api/v1/nexhub/lobby/:repo/pulls/:id/close";
/// `GET/POST /api/v1/nexhub/lobby/:repo/releases`（列表公开 / 创建仅 admin）
const PATH_RELEASES: &str = "/api/v1/nexhub/lobby/:repo/releases";
/// `DELETE /api/v1/nexhub/lobby/:repo/releases/:tag`（删除仅 admin）
const PATH_RELEASE_DELETE: &str = "/api/v1/nexhub/lobby/:repo/releases/:tag";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "nexhub-lobby";

/// README 摘要截断长度（设计文档 §4：前 500 字）。
pub const README_EXCERPT_CHARS: usize = 500;

/// 常驻条目固定名（nexos 主仓库）。
const SEED_REPO: &str = "nexos";
/// 常驻条目发布者。
const SEED_PUBLISHER: &str = "NexOS";
/// 常驻逃生口 env 名：置 `1` 时启动跳过 nexos 自动常驻（发布与刷新）——
/// 用户显式下架 nexos 后不想被启动拉回的场景。
const ENV_NO_AUTO_PUBLISH: &str = "NEXOS_LOBBY_NO_AUTO_PUBLISH";

fn default_homepage_node() -> String {
    "local".to_string()
}

/// 系统 admin token（env）：`NEXOS_ADMIN_TOKEN` 优先，回退 `OS_ADMIN_TOKEN`——
/// 与 os-api 网关（main.rs `set_admin_token`）同一环境变量语义，构造时定格
/// （避免运行中读 env 的竞态；None = 未启用 admin 回落）。
fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 悬赏操作者判定：admin 恒可（含存量字符串 poster 的平台托管悬赏）；
/// pubkey 调用方须与 poster 同 pubkey（字符串 poster 的悬赏对链上身份 403）。
fn caller_owns_bounty(caller: &Caller, poster: &str) -> bool {
    match caller.pubkey() {
        Some(pubkey) => poster == pubkey,
        None => true,
    }
}

/// PR 审核者判定（merge/reject）：admin 恒可；pubkey 调用方须为 repo owner
/// （大厅条目 publisher=pubkey 且同 pubkey）。无大厅条目（未发布到大厅的裸仓）
/// 或存量字符串条目 → 仅 admin——owner 判定以大厅发布索引为权威。
fn caller_can_review_pr(caller: &Caller, entry: Option<&LobbyEntry>) -> bool {
    match caller.pubkey() {
        Some(pubkey) => {
            entry.is_some_and(|e| entry_owner_is_pubkey(&e.publisher) && e.publisher == pubkey)
        }
        None => true,
    }
}

/// 统一 401：写端点缺/无效身份（无 nexhub 链上 token 且非系统 admin token，
/// 客户端应重走挑战-签名）。
fn auth_required() -> ApiResponse {
    error_response(
        401,
        "需要 Authorization: Bearer <nexhub token>（先 POST /api/v1/nexhub/auth/challenge + /auth/verify）或系统 admin token",
    )
}

/// 统一 403：大厅条目 owner 不匹配（重发布/下架，设计 §C 文案契约）。
fn forbidden_owner() -> ApiResponse {
    error_response(403, "仅项目所有者可操作")
}

/// 统一 403：悬赏操作者非 poster（approve/reject/cancel，设计 §C）。
fn forbidden_bounty_poster() -> ApiResponse {
    error_response(403, "仅悬赏发布者（poster）可操作")
}

/// 统一 403：PR 审核者非 admin/repo owner（merge/reject）。
fn forbidden_pr_reviewer() -> ApiResponse {
    error_response(403, "仅 admin 或仓库所有者可审核该 PR")
}

/// 统一 403：PR 关闭者非 author/admin。
fn forbidden_pr_author() -> ApiResponse {
    error_response(403, "仅 PR 作者或 admin 可关闭该 PR")
}

/// 统一 403：发版/删版仅系统 admin（链上身份不可，平台级权限）。
fn forbidden_admin_only() -> ApiResponse {
    error_response(403, "该操作仅系统 admin 可执行")
}

/// 构造一条 [`RouteSpec`]（component 固定 `nexhub-lobby`；读免认证，写要求 admin）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth,
        required_roles,
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, HandlerError> {
    serde_json::to_value(v).map_err(|e| HandlerError::Internal(format!("响应序列化失败: {e}")))
}

/// rusqlite 错误 → [`HandlerError`]（显式映射：契约错误不含 rusqlite From，避免
/// os-common 被拖入持久化依赖——审计 §6.2 方案 1）。消息与 os-api 侧既有
/// `From<rusqlite::Error> for ApiGatewayError` 的映射保持一致，错误输出零变化。
fn db_err(e: rusqlite::Error) -> HandlerError {
    HandlerError::Internal(format!("数据库错误: {e}"))
}

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 解析 query string 为 HashMap（含简易 URL 解码）。
fn query_params(path: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = path.split('?').nth(1) {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            if let Some(k) = it.next() {
                if k.is_empty() {
                    continue;
                }
                let v = it.next().unwrap_or("");
                out.insert(k.to_string(), url_decode(v));
            }
        }
    }
    out
}

/// 简易 URL 解码（仅 %XX + `+` → 空格）。按字节累积后整体转 UTF-8，
/// 避免逐字节转 `char` 破坏多字节中文等非 ASCII 查询参数。
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
            } else {
                out.push(b);
                i += 1;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 当前本地时间（RFC3339 / ISO8601 带时区）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（复用 IM 的建库模式）
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 `/tank/os-data/hub_lobby.db`，再 `/var/lib/os/hub_lobby.db`，
/// 最后 `./hub_lobby.db`（与 im.rs 的 default_db_path 同模式）。
/// （pub(crate)：issues.rs 的仓库 owner 判定读同一份发布索引。）
pub(crate) fn default_db_path() -> String {
    for p in &["/tank/os-data/hub_lobby.db", "/var/lib/os/hub_lobby.db"] {
        if Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./hub_lobby.db".to_string()
}

/// 打开 SQLite 文件，建表（WAL），nexos 仓库存在时确保常驻（发布/刷新快照 +
/// 自动联邦置 federated=true；env 逃生口 `NEXOS_LOBBY_NO_AUTO_PUBLISH=1` 可跳过）。
/// 返回连接与常驻写入的条目（跳过时 None——构造方据此广播）。
fn open_db(path: &str, repos_root: &str) -> rusqlite::Result<(Connection, Option<LobbyEntry>)> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    let seeded = ensure_nexos_published(&conn, repos_root)?;
    Ok((conn, seeded))
}

/// 建表（IF NOT EXISTS，设计文档 §4 数据模型）+ 下载量索引 + 旧库列迁移。
///
/// 旧库升级（P0 部署红线）：存量线上库是 14 列旧 schema（缺 `price_sats`/
/// `currency`），`CREATE TABLE IF NOT EXISTS` 对已存在的表是 no-op，若不补列，
/// 新代码 16 列 SELECT/INSERT 全部失败——列表被 `unwrap_or_default()` 吞成
/// 200 空数组（大厅静默清空）、详情/发布 500。故建表后必须跑
/// [`migrate_hub_lobby_columns`] 幂等补列。新表 `hub_entitlement`/`hub_bounty`
/// 为本次新增，旧库不存在，`IF NOT EXISTS` 建表即齐，无需迁移。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hub_lobby (
            repo_name       TEXT PRIMARY KEY,
            description     TEXT DEFAULT '',
            tags            TEXT DEFAULT '[]',
            publisher       TEXT DEFAULT '',
            source_url      TEXT DEFAULT '',
            homepage_node   TEXT DEFAULT 'local',
            source_node     TEXT DEFAULT 'local',
            clone_url_http  TEXT DEFAULT '',
            commit_count    INTEGER DEFAULT 0,
            size_bytes      INTEGER DEFAULT 0,
            default_branch  TEXT DEFAULT 'master',
            last_commit     TEXT,
            last_commit_date TEXT,
            readme_excerpt  TEXT DEFAULT '',
            download_count  INTEGER DEFAULT 0,
            published_at    TEXT,
            price_sats      INTEGER DEFAULT 0,
            currency        TEXT DEFAULT 'free',
            federated       INTEGER NOT NULL DEFAULT 0,
            latest_commit   TEXT,
            pushed_at       TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_hub_lobby_downloads ON hub_lobby(download_count);
        CREATE TABLE IF NOT EXISTS hub_entitlement (
            repo_name   TEXT NOT NULL,
            buyer       TEXT NOT NULL,
            chain       TEXT NOT NULL,
            txid        TEXT NOT NULL,
            amount_sats INTEGER NOT NULL,
            currency    TEXT NOT NULL,
            paid_at     TEXT NOT NULL,
            chain_block INTEGER,
            chain_value_wei TEXT,
            PRIMARY KEY (repo_name, buyer)
        );
        CREATE INDEX IF NOT EXISTS idx_entitlement_repo ON hub_entitlement(repo_name);
        CREATE TABLE IF NOT EXISTS hub_bounty (
            id           TEXT PRIMARY KEY,
            title        TEXT NOT NULL,
            description  TEXT DEFAULT '',
            tags         TEXT DEFAULT '[]',
            poster       TEXT DEFAULT '',
            reward_sats  INTEGER DEFAULT 0,
            currency     TEXT DEFAULT 'btc',
            target_url   TEXT DEFAULT '',
            status       TEXT DEFAULT 'open',
            claimed_by   TEXT DEFAULT '',
            solution_url TEXT DEFAULT '',
            deadline     TEXT DEFAULT '',
            created_at   TEXT,
            updated_at   TEXT,
            paid_at      TEXT DEFAULT '',
            payout_txid  TEXT DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_bounty_status ON hub_bounty(status);
        CREATE TABLE IF NOT EXISTS hub_pull_requests (
            id             TEXT PRIMARY KEY,
            repo_name      TEXT NOT NULL,
            title          TEXT NOT NULL,
            description    TEXT DEFAULT '',
            source_branch  TEXT NOT NULL,
            source_node    TEXT,
            author_pubkey  TEXT NOT NULL,
            author_display TEXT,
            status         TEXT DEFAULT 'open',
            base_branch    TEXT DEFAULT 'main',
            reviewed_by    TEXT,
            reviewed_at    TEXT,
            created_at     TEXT NOT NULL,
            updated_at     TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pull_requests_repo ON hub_pull_requests(repo_name, status);
        CREATE TABLE IF NOT EXISTS hub_releases (
            id          TEXT PRIMARY KEY,
            repo_name   TEXT NOT NULL,
            tag         TEXT NOT NULL,
            title       TEXT DEFAULT '',
            notes       TEXT DEFAULT '',
            created_by  TEXT DEFAULT '',
            created_at  TEXT NOT NULL,
            UNIQUE (repo_name, tag)
        );
        ",
    )?;
    migrate_hub_lobby_columns(conn)?;
    migrate_hub_entitlement_columns(conn)
}

/// `hub_entitlement` 列迁移（dApp 一期，2026-08-31）：`PRAGMA table_info` 探测
/// 缺列 → `ALTER TABLE ADD COLUMN` 幂等补齐——链上核验事实两列
/// （`chain_block` 块高 / `chain_value_wei` 链上实付 wei），存量行自动 NULL
/// （= 未核验的历史自证收据），语义见 [`Entitlement`]。
fn migrate_hub_entitlement_columns(conn: &Connection) -> rusqlite::Result<()> {
    const REQUIRED_COLUMNS: &[(&str, &str)] =
        &[("chain_block", "INTEGER"), ("chain_value_wei", "TEXT")];
    let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(hub_entitlement)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for name in rows {
            existing.insert(name?);
        }
    }
    for (col, ddl) in REQUIRED_COLUMNS {
        if !existing.contains(*col) {
            conn.execute(
                &format!("ALTER TABLE hub_entitlement ADD COLUMN {col} {ddl}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// `hub_lobby` 列迁移：`PRAGMA table_info` 探测缺列 → `ALTER TABLE ADD COLUMN`
/// 幂等补齐（新库齐全则 no-op）。清单与 [`ENTRY_COLUMNS`] 20 列逐一对照，
/// 凡旧表缺的都补；`repo_name` 是旧表主键，任何历史版本必存在，且 SQLite
/// 不允许 ALTER 补主键列，故不进清单。
fn migrate_hub_lobby_columns(conn: &Connection) -> rusqlite::Result<()> {
    /// 补列 DDL（与建表语句逐列对齐；`ADD COLUMN` 带 `NOT NULL` 必须给
    /// `DEFAULT`，存量行自动回填补省值——旧条目默认免费 `0`/`free`）。
    const REQUIRED_COLUMNS: &[(&str, &str)] = &[
        ("description", "TEXT NOT NULL DEFAULT ''"),
        ("tags", "TEXT NOT NULL DEFAULT '[]'"),
        ("publisher", "TEXT NOT NULL DEFAULT ''"),
        ("source_url", "TEXT NOT NULL DEFAULT ''"),
        ("homepage_node", "TEXT NOT NULL DEFAULT 'local'"),
        ("source_node", "TEXT NOT NULL DEFAULT 'local'"),
        // 联邦 HTTP 克隆地址（2026-08-25 跨节点拉取修复）：存量行回填空串
        // （历史条目缺地址，克隆报错引导源节点重 publish 刷新）。
        ("clone_url_http", "TEXT NOT NULL DEFAULT ''"),
        ("commit_count", "INTEGER NOT NULL DEFAULT 0"),
        ("size_bytes", "INTEGER NOT NULL DEFAULT 0"),
        ("default_branch", "TEXT NOT NULL DEFAULT 'master'"),
        ("last_commit", "TEXT"),
        ("last_commit_date", "TEXT"),
        ("readme_excerpt", "TEXT NOT NULL DEFAULT ''"),
        ("download_count", "INTEGER NOT NULL DEFAULT 0"),
        ("published_at", "TEXT"),
        ("price_sats", "INTEGER NOT NULL DEFAULT 0"),
        ("currency", "TEXT NOT NULL DEFAULT 'free'"),
        ("federated", "INTEGER NOT NULL DEFAULT 0"),
        // 自动同步链增量（2026-08-25）：结构化最新提交（JSON）+ 快照刷新时间。
        ("latest_commit", "TEXT"),
        ("pushed_at", "TEXT"),
    ];
    let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(hub_lobby)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for name in rows {
            existing.insert(name?);
        }
    }
    for (col, ddl) in REQUIRED_COLUMNS {
        if !existing.contains(*col) {
            conn.execute(&format!("ALTER TABLE hub_lobby ADD COLUMN {col} {ddl}"), [])?;
        }
    }
    Ok(())
}

/// nexos 常驻（设计文档 §5 + 2026-08-23 自动联邦）：`nexos` 主仓库**默认常驻
/// 大厅**——启动（建库路径）**无条件确保**已发布，不再「表空才 seed」：
///
/// - 条目不存在 → 自动发布为大厅第一条（publisher: NexOS，description 用仓库
///   description，空则回退占位文案）——下架后重启会回来；
/// - 条目已存在 → 刷新快照（等价重复 publish：`INSERT OR REPLACE` 语义，复用
///   [`snapshot_repo_blocking`] 重统计 commit 数/大小/last_commit/README 摘要，
///   保留 `download_count`）——推送新代码后快照不过期；
/// - **自动联邦**：常驻条目直接置 `federated=true`——nexos 一启动就在联邦
///   大厅，无需手动 `POST /:name/federate`。返回写入的条目（构造方/通道注入方
///   据此 `broadcast_entry`；P2P 未装配时广播静默跳过，标志仍置位）。
/// - **自动同步钩子**（2026-08-25，设计文档 §15）：常驻同时补装 nexos.git 的
///   post-receive 钩子——此后 106 等节点 `git push` 新提交即自动触发 publish
///   （刷新 latest_commit/pushed_at 快照）+ federate（重广播），本地与联邦大厅
///   条目不再停留在启动时的旧快照（幂等补装，见 [`crate::lobby_sync_hook`]）。
///
/// 跳过条件：`<repos_root>/nexos.git` 不存在（无从快照）；或 env
/// [`ENV_NO_AUTO_PUBLISH`] 置 `1`（逃生口：发布与联邦一并跳过）。幂等可重入：
/// 重复调用仍只此一条。
fn ensure_nexos_published(
    conn: &Connection,
    repos_root: &str,
) -> rusqlite::Result<Option<LobbyEntry>> {
    if auto_publish_disabled() {
        return Ok(None);
    }
    let bare = format!("{repos_root}/{SEED_REPO}.git");
    if !Path::new(&bare).is_dir() {
        return Ok(None);
    }
    let snap = snapshot_repo_blocking(repos_root, SEED_REPO);
    let mut entry = LobbyEntry {
        repo_name: SEED_REPO.to_string(),
        description: if snap.description.is_empty() {
            "NexOS 主仓库（本地节点）".to_string()
        } else {
            snap.description
        },
        tags: vec!["nexos".to_string(), "official".to_string()],
        publisher: SEED_PUBLISHER.to_string(),
        source_url: bare,
        homepage_node: default_homepage_node(),
        source_node: default_source_node(),
        // 常驻即定格本节点 HTTP 克隆地址（每次启动刷新——advertise_host 变化
        // /端口调整后重启即广播新地址）；联邦消费节点一键克隆经此拉取。
        clone_url_http: build_clone_url_http(SEED_REPO),
        commit_count: snap.commit_count,
        size_bytes: snap.size_bytes,
        default_branch: snap.default_branch,
        last_commit: snap.last_commit,
        last_commit_date: snap.last_commit_date,
        readme_excerpt: snap.readme_excerpt,
        download_count: 0,
        published_at: now_iso(),
        price_sats: 0,
        currency: "free".to_string(),
        // 自动联邦：常驻即推送（无需手动 federate）——广播由构造方在 handler
        // 组装完成后执行（open_db 期 LobbyFedEndpoint 尚未建好）。
        federated: true,
        // 自动同步链快照增量：结构化最新提交 + 本次刷新时间（每次启动刷新）。
        latest_commit: snap.latest_commit,
        pushed_at: now_iso(),
    };
    // 等价重复 publish：INSERT OR REPLACE 刷新快照，保留既有 download_count
    entry.download_count = find_entry(conn, SEED_REPO)?.map_or(0, |old| old.download_count);
    insert_entry(conn, &entry)?;
    // 顺手补装 post-receive 自动同步钩子（设计文档 §15）：git push nexos.git →
    // 钩子后台 curl 本地 publish（刷新快照）+ federate（重广播）——任何部署形态
    // （systemd/docker/手动）启动即自动获得「推送即同步大厅」能力，无需人工装
    // 钩子。幂等：缺失/生成内容漂移才写（用户自管钩子带自定义内容则不动）；
    // 失败仅记日志不阻塞启动（降级 = 自动同步退化为启动时刷新）。
    ensure_nexos_sync_hook(repos_root);
    Ok(Some(entry))
}

/// nexos 裸仓库补装 post-receive 自动同步钩子（[`ensure_nexos_published`] 尾步，
/// 独立函数便于日志聚焦）。API 地址/token 从 env 推导（见
/// [`crate::lobby_sync_hook`]），repos_root 即 `NEXOS_GIT_REPOS_DIR` 注入值。
fn ensure_nexos_sync_hook(repos_root: &str) {
    match crate::lobby_sync_hook::ensure_post_receive_hook(
        repos_root,
        SEED_REPO,
        &crate::lobby_sync_hook::lobby_sync_api_base(),
        &crate::lobby_sync_hook::lobby_sync_admin_token(),
    ) {
        Ok(true) => tracing_like_log(&format!(
            "nexhub-lobby: 已补装 {SEED_REPO} post-receive 自动同步钩子（push → publish+federate）"
        )),
        Ok(false) => {}
        Err(e) => tracing_like_log(&format!(
            "nexhub-lobby: 补装 {SEED_REPO} post-receive 钩子失败（不影响启动）: {e}"
        )),
    }
}

/// 常驻开关（env 逃生口）：[`ENV_NO_AUTO_PUBLISH`] 显式为 `1` → 禁用 nexos
/// 自动常驻（发布与刷新均跳过）；未设置或其余值 → 启用。
fn auto_publish_disabled() -> bool {
    std::env::var(ENV_NO_AUTO_PUBLISH).is_ok_and(|v| v.trim() == "1")
}

/// 列字段序（INSERT/SELECT 共用，21 列）。
const ENTRY_COLUMNS: &str = "repo_name,description,tags,publisher,source_url,homepage_node,\
     source_node,clone_url_http,commit_count,size_bytes,default_branch,last_commit,\
     last_commit_date,readme_excerpt,download_count,published_at,price_sats,currency,\
     federated,latest_commit,pushed_at";

fn insert_entry(conn: &Connection, e: &LobbyEntry) -> rusqlite::Result<()> {
    // latest_commit 结构体 → JSON 字符串落库（None → NULL；坏 JSON 读取侧降级 None）
    let latest_json = e
        .latest_commit
        .as_ref()
        .and_then(|c| serde_json::to_string(c).ok());
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_lobby ({ENTRY_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            e.repo_name,
            e.description,
            serde_json::to_string(&e.tags).unwrap_or_else(|_| "[]".into()),
            e.publisher,
            e.source_url,
            e.homepage_node,
            e.source_node,
            e.clone_url_http,
            e.commit_count,
            e.size_bytes,
            e.default_branch,
            e.last_commit.as_deref(),
            e.last_commit_date.as_deref(),
            e.readme_excerpt,
            e.download_count,
            e.published_at,
            e.price_sats,
            e.currency,
            e.federated,
            latest_json,
            e.pushed_at,
        ],
    )?;
    Ok(())
}

fn entry_from_row(row: &rusqlite::Row) -> rusqlite::Result<LobbyEntry> {
    let tags_json: String = row.get(2)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    Ok(LobbyEntry {
        repo_name: row.get(0)?,
        description: row.get(1)?,
        tags,
        publisher: row.get(3)?,
        source_url: row.get(4)?,
        homepage_node: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(default_homepage_node),
        source_node: row
            .get::<_, Option<String>>(6)?
            .unwrap_or_else(default_source_node),
        clone_url_http: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
        commit_count: row.get::<_, i64>(8)?.max(0) as u32,
        size_bytes: row.get::<_, i64>(9)?.max(0) as u64,
        default_branch: row.get(10)?,
        last_commit: row.get(11)?,
        last_commit_date: row.get(12)?,
        readme_excerpt: row.get(13)?,
        download_count: row.get::<_, i64>(14)?.max(0) as u64,
        published_at: row.get::<_, Option<String>>(15)?.unwrap_or_default(),
        price_sats: row.get::<_, i64>(16)?.max(0) as u64,
        currency: row
            .get::<_, Option<String>>(17)?
            .unwrap_or_else(default_currency),
        federated: row.get::<_, Option<i64>>(18)?.unwrap_or(0) != 0,
        // latest_commit：JSON 列解析（NULL/坏 JSON → None 降级不 panic）
        latest_commit: row
            .get::<_, Option<String>>(19)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        pushed_at: row.get::<_, Option<String>>(20)?.unwrap_or_default(),
    })
}

fn find_entry(conn: &Connection, name: &str) -> rusqlite::Result<Option<LobbyEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM hub_lobby WHERE repo_name=?"
    ))?;
    stmt.query_row(params![name], entry_from_row).optional()
}

/// 大厅列表：`q` 关键词（name/description/tags LIKE）、`tag` 精确标签过滤
/// （tags 是 JSON 数组，按 `"tag"` 带引号匹配避免前缀误命中）、`sort` 排序
/// （downloads=下载量降序；默认 recent=发布时间降序）。
fn load_entries(
    conn: &Connection,
    q: Option<&str>,
    tag: Option<&str>,
    sort: &str,
) -> rusqlite::Result<Vec<LobbyEntry>> {
    let mut conds: Vec<&'static str> = Vec::new();
    let mut bind: Vec<String> = Vec::new();
    if let Some(q) = q {
        conds.push("(repo_name LIKE ? OR description LIKE ? OR tags LIKE ?)");
        let like = format!("%{q}%");
        bind.push(like.clone());
        bind.push(like.clone());
        bind.push(like);
    }
    if let Some(t) = tag {
        conds.push("tags LIKE ?");
        bind.push(format!("%\"{t}\"%"));
    }
    let mut sql = format!("SELECT {ENTRY_COLUMNS} FROM hub_lobby");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(if sort == "downloads" {
        " ORDER BY download_count DESC, published_at DESC"
    } else {
        " ORDER BY published_at DESC"
    });
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), entry_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

fn delete_entry(conn: &Connection, name: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM hub_lobby WHERE repo_name=?", params![name])
}

/// download_count+1，返回新值。
fn bump_download(conn: &Connection, name: &str) -> rusqlite::Result<u64> {
    conn.execute(
        "UPDATE hub_lobby SET download_count = download_count + 1 WHERE repo_name=?",
        params![name],
    )?;
    conn.query_row(
        "SELECT download_count FROM hub_lobby WHERE repo_name=?",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .map(|c| c.max(0) as u64)
}

/// 大厅统计聚合：发布数 / 总下载 / top 标签（解析各行 tags JSON 计数，取前 10）。
fn lobby_stats(conn: &Connection) -> LobbyStats {
    let entries = load_entries(conn, None, None, "recent").unwrap_or_default();
    let total_downloads: u64 = entries.iter().map(|e| e.download_count).sum();
    let mut tag_count: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for e in &entries {
        for t in &e.tags {
            *tag_count.entry(t.clone()).or_insert(0) += 1;
        }
    }
    let mut top_tags: Vec<TagCount> = tag_count
        .into_iter()
        .map(|(tag, count)| TagCount { tag, count })
        .collect();
    top_tags.sort_by(|a, b| b.count.cmp(&a.count).then(a.tag.cmp(&b.tag)));
    top_tags.truncate(10);
    LobbyStats {
        published_count: entries.len(),
        total_downloads,
        top_tags,
    }
}

// ----------------------------------------------------------------------------
// 授权（购买）持久化层（设计文档 §10 货币化：付费条目克隆前需取得授权）
// ----------------------------------------------------------------------------

/// 单条购买授权（hub_entitlement 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entitlement {
    /// 大厅条目（仓库）名。
    pub repo_name: String,
    /// 购买者标识（钱包地址 / 用户 id；与 os-wallet `AddressId` 对齐）。
    pub buyer: String,
    /// 链：`btc` / `nex` / `usdc` / `eth`（与条目 currency 同域）。
    pub chain: String,
    /// 链上交易 id / 收据指纹（一期为收据指纹；二期对接 os-wallet 验真）。
    pub txid: String,
    /// 实际支付金额（最小单位），应 ≥ 条目 `price_sats`。
    pub amount_sats: u64,
    /// 计价货币（冗余存一份，便于审计）。
    pub currency: String,
    /// 支付时间（RFC3339）。
    pub paid_at: String,
    /// 链上核验事实（dApp 一期，2026-08-31）：核验通过时的**块高**；
    /// None = 未核验（自证收据 / RPC 降级 / 开关关闭）。
    #[serde(default)]
    pub chain_block: Option<u64>,
    /// 链上核验事实：链上**实付金额**（wei 十进制字符串，与 tx 的 value 一致）；
    /// None = 未核验。审计口径：`chain_block` 有值 ⇒ 该收据经真实 RPC 核验。
    #[serde(default)]
    pub chain_value_wei: Option<String>,
}

/// 列字段序（hub_entitlement INSERT/SELECT 共用）。
const ENTITLEMENT_COLUMNS: &str =
    "repo_name,buyer,chain,txid,amount_sats,currency,paid_at,chain_block,chain_value_wei";

fn insert_entitlement(conn: &Connection, e: &Entitlement) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_entitlement ({ENTITLEMENT_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?)"
        ),
        params![
            e.repo_name,
            e.buyer,
            e.chain,
            e.txid,
            e.amount_sats,
            e.currency,
            e.paid_at,
            e.chain_block,
            e.chain_value_wei,
        ],
    )?;
    Ok(())
}

fn entitlement_from_row(row: &rusqlite::Row) -> rusqlite::Result<Entitlement> {
    Ok(Entitlement {
        repo_name: row.get(0)?,
        buyer: row.get(1)?,
        chain: row.get(2)?,
        txid: row.get(3)?,
        amount_sats: row.get::<_, i64>(4)?.max(0) as u64,
        currency: row.get(5)?,
        paid_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        chain_block: row.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
        chain_value_wei: row.get(8)?,
    })
}

/// 查询某买家对某仓库的授权（存在即已付费，可克隆）。
fn find_entitlement(
    conn: &Connection,
    name: &str,
    buyer: &str,
) -> rusqlite::Result<Option<Entitlement>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTITLEMENT_COLUMNS} FROM hub_entitlement WHERE repo_name=? AND buyer=?"
    ))?;
    stmt.query_row(params![name, buyer], entitlement_from_row)
        .optional()
}

/// 授权记录列表（`GET /api/v1/nexhub/lobby/entitlements`）：`repo` 按仓库过滤
/// （admin 审计某条目的全部买家）、`buyer` 按买家过滤（自查购买记录），均可选
/// 可组合，都不给则全量；按支付时间降序。
fn list_entitlements(
    conn: &Connection,
    repo: Option<&str>,
    buyer: Option<&str>,
) -> rusqlite::Result<Vec<Entitlement>> {
    let mut conds: Vec<&'static str> = Vec::new();
    let mut bind: Vec<&str> = Vec::new();
    if let Some(r) = repo {
        conds.push("repo_name = ?");
        bind.push(r);
    }
    if let Some(b) = buyer {
        conds.push("buyer = ?");
        bind.push(b);
    }
    let mut sql = format!("SELECT {ENTITLEMENT_COLUMNS} FROM hub_entitlement");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY paid_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), entitlement_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

/// 验证购买收据（设计文档 §10 货币化）：货币一致 + 金额足额 + txid 非空
/// （收据指纹的**最低门槛**）。链上验真（dApp 一期，2026-08-31）不在此函数——
/// 它是同步纯函数，只做自证面校验；真实 RPC 核验在其通过后由
/// [`check_chain_payment`]（异步、可注入）接力，见 purchase/approve 两处接线。
///
/// 返回 `Ok(())` 或描述拒绝原因（调用方转 402/400）。
fn verify_payment(receipt: &Entitlement, price_sats: u64, currency: &str) -> Result<(), String> {
    // 货币必须一致
    if !receipt.currency.eq_ignore_ascii_case(currency) {
        return Err(format!(
            "货币不符：条目为 {currency}，收据为 {}",
            receipt.currency
        ));
    }
    // 金额必须足额
    if receipt.amount_sats < price_sats {
        return Err(format!(
            "支付不足：需 {price_sats} {currency}，实付 {}",
            receipt.amount_sats
        ));
    }
    // txid 非空是收据的最低门槛（链上核验在 check_chain_payment 接力）
    if receipt.txid.trim().is_empty() {
        return Err("txid/收据指纹不得为空".into());
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 链上支付验真（dApp 一期接线层，2026-08-31；二期增量 2026-09-02）
// ----------------------------------------------------------------------------
//
// 定位：把「自证收据」（txid 非空即过，安全隐患台账 S1）升级为真实 EVM RPC
// 核验（docs/DAPP_RESEARCH.md §3 方向 1）。**核验本体**在 [`crate::chain_verify`]
// （独立实现，契约冻结）；本段是**业务接线层**，被两条业务线共用：
//
// 1. NexHub：`POST /lobby/:name/purchase`（购买授权）与 `POST /bounty/:id/approve`
//    （悬赏验收放款）；
// 2. os-api 网关：`POST /gateway/payments/:id/confirm`（PaymentOrder 确认到账，
//   `crates/os-api/src/handlers/api_gateway.rs` 直接 import 本段的 pub 项）。
//
// 接缝设计（[`EvmTxVerifier`] trait）：生产实现 [`RpcVerifier`] 直调
// `chain_verify::verify_evm_tx`；测试注入固定 [`VerifyOutcome`]。这层抽象同时是
// 未来换核验后端（自建节点 / 商业 RPC / 索引服务）的替换点——业务语义全部在
// 本段，执行器一换即迁移。
//
// RPC 来源链（[`ChainPayGate::rpc_candidates`]，三段拼接成候选列表，
// `verify_evm_tx` 按序 failover）：
//
// ```text
// 请求显式 rpc_url（body 可选字段，admin/条目 owner 自配）
//   → env NEXOS_CHAIN_RPC_URLS（JSON {"<chain_id>": "<url>" 或 ["<url>",...]}，
//     解析失败 eprintln 警告并忽略，绝不 panic）
//   → chain_verify::fallback_rpc_for(chain_id)（链预设公共 RPC 兜底）
// ```
//
// 与 `chain_verify::ChainVerifyGate`（core 侧装配门面）的关系：本段
// [`ChainPayGate`] 是**业务接线网关**，在 core 契约（`verify_evm_tx` /
// `fallback_rpc_for`）之上多管三件事——① per-request 显式 rpc_url 前置进候选链；
// ② NexHub/网关侧缺省（`NEXOS_HUB_PAY_TO` / `NEXOS_EVM_CHAIN_ID`）；③ 开关关闭
// 的 `Skipped` 语义（不产生任何链上事实与标注，比 core 的 legacy_autopass
// 假 Verified 更干净）。两类型并存不冲突：core 门面服务 chain_verify 自身测试。
//
// 结果语义表（[`verdict_for`]；⚠️ 信任模型与降级策略同步维护在
// docs/NEXHUB_LOBBY_DESIGN.md §10 与 docs/GATEWAY_MONETIZATION.md）：
//
// | VerifyOutcome | 业务动作 | HTTP |
// |---------------|----------|------|
// | Verified      | 放行；`block_number`/`value_wei` 落库到收据结构 | 200 |
// | Pending       | 拒绝（**可重试**——未上块≠欺诈，稍后重试即可） | 409 |
// | Mismatch      | 拒绝（错误信息带字段名与链上实际值） | 409 |
// | NotFound      | 拒绝（txid 有误或已被节点裁剪） | 400 |
// | RpcError      | **降级放行** + 日志警告（网络故障不应阻断交易；S1 缓解 =「RPC 可用时核验，不白嫖」） | 200 |
//
// 无法构造凭证（非核验域货币 / 缺链 ID / 缺收款地址 / usdt 缺 ERC-20 合约配置）
// → 放行但响应标注 `chain_verify.status="unverified"` + 日志警告（真实数据铁律：
// 是否核验过必须可见，不静默假装成功）。`NEXOS_CHAIN_VERIFY_ENABLED=0` → [`ChainPayCheck::Skipped`]
// = 整体回旧行为（非空即过，响应不带任何标注）。
//
// **二期增量（2026-09-02）**：
//
// 1. **ERC-20（USDT@EVM）**：`currency=usdt` 且链 ID 可定位（=EVM 链；TRON 上的
//    USDT 定位不到 EVM 链 ID，仍 Unverified 人工）时构造 `TxProof.erc20` 凭证，
//    核验切换为 receipt Transfer 日志对账（见 chain_verify.rs 二期注记）。合约
//    地址来源：body `erc20_contract` → env `NEXOS_USDT_EVM_CONTRACT` → 都无则
//    Unverified（**不猜合约地址**——猜错合约=放行假代币转账）；小数位：body
//    `erc20_decimals` → env `NEXOS_USDT_EVM_DECIMALS`（默认 6）。金额口径=
//    最小单位（`to_min_unit_str`：整数透传 / 小数按 decimals 换算）。
// 2. **金额规则** [`AmountRule`]（三处接线定稿，docs 双写）：
//
//    | 业务线 | 规则 | 理由 |
//    |--------|------|------|
//    | 网关 confirm（充值） | `AtLeast` | 充值多打不亏待用户——超额照常入账订单积分，不足才拦 |
//    | NexHub purchase（购买） | `Exact` | 商品定价等值——须按应付额整额打款，多/少都不对账 |
//    | bounty approve（放款） | `AtLeast` | 与自证面「金额足额」（≥ 奖励）语义对齐，多打不亏待 hunter |

/// 可替换的 EVM 核验执行器（注入接缝）。
///
/// 生产实现 [`RpcVerifier`] 直调 `chain_verify::verify_evm_tx`；测试实现注入固定
/// [`VerifyOutcome`]（并可携带调用计数，断言「非 EVM 货币不触发核验」等接线语义）。
/// 返回 boxed future 而非 async trait 方法：dyn 兼容且零新增依赖。
pub trait EvmTxVerifier: Send + Sync {
    /// 核验一笔交易（rpc_urls 已按候选链排好序；timeout 来自网关配置）。
    fn verify(
        &self,
        rpc_urls: &[String],
        proof: &TxProof,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = VerifyOutcome> + Send>>;
}

/// 生产执行器：直调 [`crate::chain_verify::verify_evm_tx`]。
struct RpcVerifier;

impl EvmTxVerifier for RpcVerifier {
    fn verify(
        &self,
        rpc_urls: &[String],
        proof: &TxProof,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = VerifyOutcome> + Send>> {
        let rpcs = rpc_urls.to_vec();
        let proof = proof.clone();
        Box::pin(async move { crate::chain_verify::verify_evm_tx(&rpcs, &proof, timeout).await })
    }
}

/// 链上验真网关——env 配置（构造时定格，与 admin_token 同款模式）+ 可替换执行器。
///
/// env 清单（全部 `NEXOS_` 前缀；与 `chain_verify.rs` 模块头一致）：
///
/// | env | 默认 | 作用 |
/// |---|---|---|
/// | `NEXOS_CHAIN_VERIFY_ENABLED` | `1` | 总开关；`0`=回旧行为（非空即过，无标注） |
/// | `NEXOS_CHAIN_RPC_URLS` | （空） | 节点级 RPC 预设，JSON `{"<chain_id>": "<url>" 或 ["<url>",...]}` |
/// | `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` | `10` | 单次核验 RPC 超时（下限 1s） |
/// | `NEXOS_EVM_CHAIN_ID` | （无） | EVM 支付缺省链 ID（NexHub 购买/悬赏 + 网关 confirm 共用） |
/// | `NEXOS_HUB_PAY_TO` | （无） | NexHub 购买流缺省收款地址（节点运营者配置；悬赏不回落此值） |
/// | `NEXOS_USDT_EVM_CONTRACT` | （无） | USDT@EVM 的 ERC-20 合约地址（二期 ERC-20 核验；body `erc20_contract` 优先） |
/// | `NEXOS_USDT_EVM_DECIMALS` | `6` | USDT 小数位（主流链=6；body `erc20_decimals` 优先；非法值警告回默认） |
pub struct ChainPayGate {
    enabled: bool,
    timeout: Duration,
    /// `NEXOS_CHAIN_RPC_URLS` 原始串（构造时定格；解析在
    /// [`parse_chain_rpc_env`] 纯函数，坏配置警告+忽略不 panic）。
    rpc_env_raw: Option<String>,
    default_pay_to: Option<String>,
    default_chain_id: Option<u64>,
    /// USDT@EVM 合约地址缺省（env `NEXOS_USDT_EVM_CONTRACT`，二期）。
    usdt_evm_contract: Option<String>,
    /// USDT 小数位缺省（env `NEXOS_USDT_EVM_DECIMALS`，默认 6，二期）。
    usdt_evm_decimals: u8,
    verifier: Arc<dyn EvmTxVerifier>,
}

impl ChainPayGate {
    /// 生产构造：读 env 定格 + 生产执行器（handler 构造时调用一次）。
    #[must_use]
    pub fn from_env() -> Self {
        Self::with_parts(
            chain_verify_enabled_from_env(),
            std::env::var("NEXOS_CHAIN_RPC_URLS")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .as_deref(),
            non_empty_env("NEXOS_HUB_PAY_TO").as_deref(),
            std::env::var("NEXOS_EVM_CHAIN_ID")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok()),
            chain_verify_timeout_from_env(),
            non_empty_env("NEXOS_USDT_EVM_CONTRACT").as_deref(),
            usdt_evm_decimals_from_env(),
            Arc::new(RpcVerifier),
        )
    }

    /// 全字段注入构造（测试/诊断：绕开 env 并行竞态，执行器可控）。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn with_parts(
        enabled: bool,
        rpc_env_raw: Option<&str>,
        default_pay_to: Option<&str>,
        default_chain_id: Option<u64>,
        timeout: Duration,
        usdt_evm_contract: Option<&str>,
        usdt_evm_decimals: u8,
        verifier: Arc<dyn EvmTxVerifier>,
    ) -> Self {
        Self {
            enabled,
            timeout,
            rpc_env_raw: rpc_env_raw
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            default_pay_to: default_pay_to
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            default_chain_id,
            usdt_evm_contract: usdt_evm_contract
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            usdt_evm_decimals,
            verifier,
        }
    }

    /// 总开关状态（false = 调用方整体回旧行为）。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 节点级缺省收款地址（env `NEXOS_HUB_PAY_TO`）。
    #[must_use]
    pub fn default_pay_to(&self) -> Option<&str> {
        self.default_pay_to.as_deref()
    }

    /// 缺省链 ID（env `NEXOS_EVM_CHAIN_ID`）。
    #[must_use]
    pub fn default_chain_id(&self) -> Option<u64> {
        self.default_chain_id
    }

    /// USDT@EVM 合约地址缺省（env `NEXOS_USDT_EVM_CONTRACT`；二期 ERC-20）。
    #[must_use]
    pub fn usdt_evm_contract(&self) -> Option<&str> {
        self.usdt_evm_contract.as_deref()
    }

    /// USDT 小数位缺省（env `NEXOS_USDT_EVM_DECIMALS`，默认 6；二期 ERC-20）。
    #[must_use]
    pub fn usdt_evm_decimals(&self) -> u8 {
        self.usdt_evm_decimals
    }

    /// RPC 候选链：body 显式 → env `NEXOS_CHAIN_RPC_URLS[chain_id]` → 链预设兜底。
    #[must_use]
    pub fn rpc_candidates(&self, explicit: Option<&str>, chain_id: u64) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(url) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            out.push(url.to_string());
        }
        if let Some(raw) = &self.rpc_env_raw {
            out.extend(parse_chain_rpc_env(raw, chain_id));
        }
        out.extend(crate::chain_verify::fallback_rpc_for(chain_id));
        out
    }

    /// 执行核验（候选链为空视作 RpcError——降级放行语义，见语义表）。
    pub async fn verify(&self, proof: &TxProof, explicit_rpc: Option<&str>) -> VerifyOutcome {
        let candidates = self.rpc_candidates(explicit_rpc, proof.chain_id);
        if candidates.is_empty() {
            return VerifyOutcome::RpcError {
                detail: format!(
                    "chain {} 无可用 RPC 候选（未配置 NEXOS_CHAIN_RPC_URLS 且无兜底预设）",
                    proof.chain_id
                ),
            };
        }
        self.verifier.verify(&candidates, proof, self.timeout).await
    }
}

/// `NEXOS_CHAIN_VERIFY_ENABLED` 解析：未设置默认开；`0`/`false`/`off`（大小写
/// 不敏感）关，其余任意值视为开（与 chain_verify.rs 模块头契约一致）。
fn chain_verify_enabled_from_env() -> bool {
    !std::env::var("NEXOS_CHAIN_VERIFY_ENABLED")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off"
            )
        })
        .unwrap_or(false)
}

/// `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` 解析：默认 10s；非法值警告并回默认；下限 1s。
fn chain_verify_timeout_from_env() -> Duration {
    const DEFAULT_SECS: u64 = 10;
    match std::env::var("NEXOS_CHAIN_VERIFY_TIMEOUT_SECS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs.max(1)),
            Err(_) => {
                eprintln!("[chain-verify] NEXOS_CHAIN_VERIFY_TIMEOUT_SECS={v:?} 非法，回默认 {DEFAULT_SECS}s");
                Duration::from_secs(DEFAULT_SECS)
            }
        },
        Err(_) => Duration::from_secs(DEFAULT_SECS),
    }
}

/// `NEXOS_USDT_EVM_DECIMALS` 解析（二期 ERC-20）：默认 6（USDT 主流链小数位）；
/// 非法值警告回默认；上限 36（>36 必然溢出 u128，视为配置错误回默认）。
fn usdt_evm_decimals_from_env() -> u8 {
    const DEFAULT: u8 = 6;
    match std::env::var("NEXOS_USDT_EVM_DECIMALS") {
        Ok(v) => match v.trim().parse::<u8>() {
            Ok(d) if d <= 36 => d,
            Ok(d) => {
                eprintln!("[chain-verify] NEXOS_USDT_EVM_DECIMALS={d} 超 36（u128 必然溢出），回默认 {DEFAULT}");
                DEFAULT
            }
            Err(_) => {
                eprintln!("[chain-verify] NEXOS_USDT_EVM_DECIMALS={v:?} 非法，回默认 {DEFAULT}");
                DEFAULT
            }
        },
        Err(_) => DEFAULT,
    }
}

/// 读非空 env（trim 后非空才算配置）。
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 解析 `NEXOS_CHAIN_RPC_URLS`（纯函数，测试直测）：JSON 对象
/// `{"<chain_id>": "<url>" 或 ["<url>", ...]}`，取指定链的 URL 列表。
///
/// 容错（配置错误绝不 panic，坏值丢弃 + 警告）：非 JSON / 非对象 / 键形状非法
/// → 空列表 + eprintln；数组内非字符串/空串元素跳过。
#[must_use]
pub fn parse_chain_rpc_env(raw: &str, chain_id: u64) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[chain-verify] NEXOS_CHAIN_RPC_URLS 解析失败（{e}），已忽略该配置");
            return Vec::new();
        }
    };
    let Some(obj) = parsed.as_object() else {
        eprintln!("[chain-verify] NEXOS_CHAIN_RPC_URLS 须为 JSON 对象 {{\"<chain_id>\": \"<url>\"|[urls]}}，已忽略");
        return Vec::new();
    };
    match obj.get(&chain_id.to_string()) {
        None => Vec::new(),
        Some(serde_json::Value::String(url)) if !url.trim().is_empty() => {
            vec![url.trim().to_string()]
        }
        Some(serde_json::Value::Array(urls)) => urls
            .iter()
            .filter_map(|u| u.as_str())
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .collect(),
        Some(_) => {
            eprintln!("[chain-verify] NEXOS_CHAIN_RPC_URLS[{chain_id}] 形状非法（须 \"<url>\" 或 [urls]），已忽略");
            Vec::new()
        }
    }
}

/// 链 ID 解析（优先级）：body 显式 `chain_id` → `chain` 字符串可解析为数值时
/// （如 `"11155111"`；`"eth"` 等货币名忽略）→ env 缺省 `NEXOS_EVM_CHAIN_ID`。
#[must_use]
pub fn resolve_chain_id(
    explicit: Option<u64>,
    chain_str: Option<&str>,
    env_default: Option<u64>,
) -> Option<u64> {
    explicit
        .or_else(|| chain_str.and_then(|s| s.trim().parse::<u64>().ok()))
        .or(env_default)
}

/// 是否 EVM native 币（一期核验域）：NexHub 侧 `eth`、网关侧 `evm`。
/// `btc`/`nex`/`usdc` 不在核验域（usdc 的 ERC-20 接入是后续项，一期只配了
/// USDT 合约 env）。
#[must_use]
pub fn evm_native_currency(currency: &str) -> bool {
    matches!(currency.trim().to_ascii_lowercase().as_str(), "eth" | "evm")
}

/// 是否 USDT（二期 ERC-20 核验域）：`usdt`。配合**链 ID 可定位**才走 EVM
/// 路径——TRON 上的 USDT（无 EVM chain_id）仍 Unverified 人工确认。
#[must_use]
pub fn usdt_currency(currency: &str) -> bool {
    currency.trim().eq_ignore_ascii_case("usdt")
}

/// 金额 → 最小单位十进制字符串（通用版；**小数位由调用方给定**）。
///
/// - 纯整数（如 `"500"`、`"10000000"`）：视为**已是最小单位**，原样返回
///   （与 `LobbyEntry.price_sats`「最小货币单位」及网关 `PaymentOrder.amount_crypto`
///   的既有语义一致——NexHub usdt 条目的 amount_sats 即最小单位整数）；
/// - 带小数点（如 `"10.00"`，网关 usdt 订单的价目形状）：按 `decimals` 位换算
///   （USDT=6 → `"10000000"`；native 币=18），小数超 `decimals` 位/非数字 → None；
/// - 空串/非法 → None。
#[must_use]
pub fn to_min_unit_str(amount: &str, decimals: u8) -> Option<String> {
    let s = amount.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(min_unit) = s.parse::<u128>() {
        return Some(min_unit.to_string());
    }
    let (int_part, frac_part) = s.split_once('.')?;
    if int_part.is_empty() || !int_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac_len = usize::from(decimals);
    if frac_part.len() > frac_len || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scaled = format!(
        "{int_part}{frac_part}{}",
        "0".repeat(frac_len - frac_part.len())
    );
    scaled.parse::<u128>().ok().map(|v| v.to_string())
}

/// 金额 → wei 十进制字符串（**18 位小数假设**，注释与文档双写；native 币路径）。
///
/// - 纯整数（如 `"500"`、`"10000000000000000000"`）：视为**已是最小单位 wei**，
///   原样返回（与 `LobbyEntry.price_sats`「最小货币单位」及网关
///   `PaymentOrder.amount_crypto`（evm 订单即 wei 整数串）既有语义一致）；
/// - 带小数点（如 `"0.02"`）：按 **18 位小数**换算（EVM 主流链 native 币均为
///   18 位；**非 18 位链不适用**，见 docs 限制清单），小数超 18 位/非数字 → None；
/// - 空串/非法 → None。
///
/// 二期起这是 [`to_min_unit_str`] 的 18 位特化（ERC-20 走 `to_min_unit_str` +
/// token decimals，如 USDT=6）。
#[must_use]
pub fn to_wei_str(amount: &str) -> Option<String> {
    to_min_unit_str(amount, 18)
}

/// [`VerifyOutcome`] → 业务判定（纯函数，语义表的代码化）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChainPayVerdict {
    /// 放行：链上事实已核实（块高 + 实付最小单位 + ERC-20 时的代币合约）。
    Allow {
        block_number: u64,
        value_wei: String,
        /// ERC-20 路径 = 代币合约地址（展示/落库标注用）；native 恒 None。
        token: Option<String>,
    },
    /// 降级放行：RPC 故障（网络问题≠链上结论），调用方记警告日志。
    Degrade { detail: String },
    /// 拒绝：`status` + 人读原因；`retryable`=Pending（稍后重试）。
    Deny {
        status: u16,
        reason: String,
        retryable: bool,
    },
}

/// 语义映射（见上文语义表）。Pending 是**可重试**语义，错误文案已带「稍后重试」，
/// 不得当作欺诈处理。
#[must_use]
pub fn verdict_for(outcome: VerifyOutcome) -> ChainPayVerdict {
    match outcome {
        VerifyOutcome::Verified {
            block_number,
            value_wei,
            token,
            ..
        } => ChainPayVerdict::Allow {
            block_number,
            value_wei,
            token,
        },
        VerifyOutcome::Pending => ChainPayVerdict::Deny {
            status: 409,
            reason: "交易尚未上块确认（Pending）——非欺诈判定，请稍后重试".into(),
            retryable: true,
        },
        VerifyOutcome::Mismatch {
            field,
            expect,
            actual,
        } => ChainPayVerdict::Deny {
            status: 409,
            reason: format!("链上核验不符：{field} 期望 {expect}，链上实际 {actual}"),
            retryable: false,
        },
        VerifyOutcome::NotFound => ChainPayVerdict::Deny {
            status: 400,
            reason: "链上未找到该交易（txid 有误或已被节点裁剪）".into(),
            retryable: false,
        },
        VerifyOutcome::RpcError { detail } => ChainPayVerdict::Degrade { detail },
    }
}

/// 业务侧核验结论（[`check_chain_payment`] 的输出，调用方据此放行/拒绝/标注）。
#[derive(Debug, Clone)]
pub enum ChainPayCheck {
    /// 开关关闭（`NEXOS_CHAIN_VERIFY_ENABLED=0`）——回旧行为，调用方不加任何标注。
    Skipped,
    /// 核验通过：链上事实（落库到收据结构）。`token`=Some 表示 ERC-20 路径。
    Verified {
        chain_id: u64,
        block_number: u64,
        value_wei: String,
        token: Option<String>,
    },
    /// 未核验即放行（非 EVM 货币 / 缺链 ID / 缺收款地址 / 金额无法换算 /
    /// usdt 缺 ERC-20 合约配置）：自证收据 + 响应标注 `unverified` + 日志警告。
    Unverified(String),
    /// RPC 故障降级放行（自证收据 + 响应标注 `degraded` + 日志警告）。
    Degraded(String),
    /// 拒绝（status + 原因；Pending 可重试语义已写进原因文案）。
    Denied { status: u16, reason: String },
}

/// 链上核验输入 hints（两条业务线共用 [`check_chain_payment`]）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChainPayHints<'a> {
    /// 显式链 ID（body `chain_id`；缺省回落 `chain_str` 数值 → env 缺省）。
    pub chain_id: Option<u64>,
    /// 链字符串（可解析为数值时作链 ID，如 `"11155111"`；`"eth"` 等忽略）。
    pub chain_str: Option<&'a str>,
    /// 显式 RPC（body `rpc_url`，admin/条目 owner 自配——候选链第一段）。
    pub rpc_url: Option<&'a str>,
    /// 收款地址（悬赏 approve=poster 提供的 hunter 收款地址；网关=订单收款地址）。
    pub pay_to: Option<&'a str>,
    /// `pay_to` 缺失时是否回落节点级缺省（env `NEXOS_HUB_PAY_TO`）——**仅购买流**
    /// 置 true（条目收益归本节点运营者）；悬赏置 false（回落节点地址会错杀
    /// 发给 hunter 的真实支付），网关置 false（订单自带地址）。
    pub fallback_default_pay_to: bool,
    /// 金额规则（二期，默认 `Exact`）。接线定稿：**网关 confirm 与悬赏 approve
    /// 置 `AtLeast`**（充值/放款多打不亏待用户），**NexHub 购买保持 `Exact`**
    /// （商品定价等值——多打/少打都不对账，须按应付额整额打款）。
    pub amount_rule: AmountRule,
    /// ERC-20 合约地址（body `erc20_contract`，usdt@EVM 用；缺省回落网关 env
    /// `NEXOS_USDT_EVM_CONTRACT`。信任模型与 `rpc_url` 同款：请求方可指向
    /// 自选合约，链上事实/合约地址落库可审计）。
    pub erc20_contract: Option<&'a str>,
    /// ERC-20 小数位（body `erc20_decimals`；缺省回落网关 env
    /// `NEXOS_USDT_EVM_DECIMALS`，默认 6）。
    pub erc20_decimals: Option<u8>,
}

/// 业务核验编排（NexHub 购买/悬赏验收 + 网关 PaymentOrder confirm 共用的入口）。
///
/// 步骤：开关关 → [`ChainPayCheck::Skipped`]；txid 空 / 非 EVM 域货币
/// （native eth/evm 或 usdt）→ [`ChainPayCheck::Unverified`]（放行 + 标注，
/// 不静默）；链 ID 不可解析 → 同（usdt 特别说明：TRON 上的 USDT 无 EVM 链 ID，
/// 即落在此分支——人工通道）；金额换算 / ERC-20 合约定位 / 收款地址缺失 →
/// Unverified；否则构造 [`TxProof`]（按货币分 native / ERC-20 两路）走
/// [`ChainPayGate::verify`] 并按 [`verdict_for`] 语义映射。
///
/// `expected_value`：整数串 = 已是最小单位；带小数点 = native 按 18 位 /
/// ERC-20 按 token decimals 换算（[`to_min_unit_str`]）。
pub async fn check_chain_payment(
    gate: &ChainPayGate,
    currency: &str,
    txid: &str,
    expected_value: &str,
    hints: &ChainPayHints<'_>,
) -> ChainPayCheck {
    if !gate.enabled() {
        return ChainPayCheck::Skipped;
    }
    let txid = txid.trim();
    if txid.is_empty() {
        return ChainPayCheck::Unverified("txid 为空，无法链上核验".into());
    }
    // —— 货币分流（二期）：eth/evm 走 native；usdt 走 ERC-20（链 ID 可定位时）；
    //    其余（btc/nex/usdc/…）不在核验域。——
    let is_native = evm_native_currency(currency);
    let is_usdt = usdt_currency(currency);
    if !is_native && !is_usdt {
        return ChainPayCheck::Unverified(format!(
            "货币 {currency} 非 EVM 核验域（支持 eth/evm native 与 usdt@EVM ERC-20；btc/nex/usdc 仍自证）"
        ));
    }
    // —— 链 ID（usdt 也在此分流：定位不到 EVM 链 = TRON/人工通道）——
    let Some(chain_id) = resolve_chain_id(hints.chain_id, hints.chain_str, gate.default_chain_id())
    else {
        return ChainPayCheck::Unverified(if is_usdt {
            "usdt 未定位 EVM 链 ID（TRON 上的 USDT 不核验，走人工确认；EVM 链须 body chain_id / 数值 chain，或 env NEXOS_EVM_CHAIN_ID）".into()
        } else {
            "缺链 ID（body chain_id / 数值 chain，或 env NEXOS_EVM_CHAIN_ID）".into()
        });
    };
    // —— 金额换算 + ERC-20 凭证构造（usdt：body 合约 → env 合约；无则不猜）——
    let (expected_min_unit, erc20) = if is_native {
        match to_wei_str(expected_value) {
            Some(v) => (v, None),
            None => {
                return ChainPayCheck::Unverified(format!(
                    "应付金额 {expected_value:?} 无法换算为 wei（18 位小数假设）"
                ));
            }
        }
    } else {
        let contract = hints
            .erc20_contract
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| gate.usdt_evm_contract())
            .map(str::to_string);
        let Some(contract) = contract else {
            return ChainPayCheck::Unverified(
                "usdt@EVM 核验缺合约地址（body erc20_contract 或 env NEXOS_USDT_EVM_CONTRACT）——不猜合约地址，走人工确认".into(),
            );
        };
        let decimals = hints
            .erc20_decimals
            .unwrap_or_else(|| gate.usdt_evm_decimals());
        match to_min_unit_str(expected_value, decimals) {
            Some(v) => (v, Some(Erc20Spec { contract, decimals })),
            None => {
                return ChainPayCheck::Unverified(format!(
                    "应付金额 {expected_value:?} 无法换算为最小单位（USDT 小数位 {decimals}，env NEXOS_USDT_EVM_DECIMALS 可调）"
                ));
            }
        }
    };
    let pay_to = match hints
        .pay_to
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            hints
                .fallback_default_pay_to
                .then_some(())
                .and_then(|_| gate.default_pay_to())
        }) {
        Some(p) => p.to_string(),
        None => {
            return ChainPayCheck::Unverified(
                "缺收款地址（悬赏 approve 须 body pay_to；购买流须 env NEXOS_HUB_PAY_TO）".into(),
            );
        }
    };
    let proof = TxProof {
        chain_id,
        tx_hash: txid.to_string(),
        expected_to: pay_to,
        expected_value: expected_min_unit,
        amount_rule: hints.amount_rule,
        erc20,
    };
    let outcome = gate.verify(&proof, hints.rpc_url).await;
    match verdict_for(outcome) {
        ChainPayVerdict::Allow {
            block_number,
            value_wei,
            token,
        } => {
            let token_note = token
                .as_deref()
                .map(|c| format!(" token={c}"))
                .unwrap_or_default();
            eprintln!(
                "[chain-verify] 核验通过：chain={chain_id} tx={txid} block={block_number} value={value_wei}（最小单位）{token_note}"
            );
            ChainPayCheck::Verified {
                chain_id,
                block_number,
                value_wei,
                token,
            }
        }
        ChainPayVerdict::Degrade { detail } => {
            eprintln!(
                "[chain-verify] RPC 故障，降级放行（自证收据；S1 缓解=RPC 可用时核验）：{detail}"
            );
            ChainPayCheck::Degraded(detail)
        }
        ChainPayVerdict::Deny { status, reason, .. } => {
            eprintln!("[chain-verify] 拒绝：{reason}");
            ChainPayCheck::Denied { status, reason }
        }
    }
}

/// 把 [`ChainPayCheck`] 折成响应标注字段 `chain_verify`（None = 不标注——
/// 开关关闭的回旧行为 / 拒绝路径直接回错误响应）。
#[must_use]
pub fn chain_verify_json(check: &ChainPayCheck) -> Option<serde_json::Value> {
    match check {
        ChainPayCheck::Skipped | ChainPayCheck::Denied { .. } => None,
        ChainPayCheck::Verified {
            chain_id,
            block_number,
            value_wei,
            token,
        } => {
            let mut marker = serde_json::json!({
                "status": "verified",
                "chain_id": chain_id,
                "block_number": block_number,
                "value_wei": value_wei,
            });
            if let Some(contract) = token {
                marker["token"] = serde_json::json!(contract);
            }
            Some(marker)
        }
        ChainPayCheck::Degraded(detail) => Some(serde_json::json!({
            "status": "degraded",
            "detail": detail,
            "note": "RPC 故障降级放行（自证收据）",
        })),
        ChainPayCheck::Unverified(reason) => Some(serde_json::json!({
            "status": "unverified",
            "reason": reason,
        })),
    }
}

// ----------------------------------------------------------------------------
// ----------------------------------------------------------------------------
// 悬赏（bounty）持久化层（设计文档 §11 悬赏：大厅内「出资求活」的发现子资源）
// ----------------------------------------------------------------------------

/// 悬赏条目（hub_bounty 行）。
///
/// 与货币化（§10）的关系：货币化是「卖我的成果」（付费克隆），悬赏是「出钱求别人做
/// 某事」（如更新一个停更的 GitHub 仓库）。二者共用同一套虚拟货币与（一期自证 /
/// 二期链上）支付机制，但语义不同：悬赏必有奖励（`reward_sats>0` 且 `currency` 为
/// 真实链），且存在 `open→claimed→submitted→paid` 生命周期。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bounty {
    /// 悬赏 id（唯一键，服务端生成；不含空格/特殊字符）。
    pub id: String,
    /// 标题（想做什么）。
    #[serde(default)]
    pub title: String,
    /// 需求描述（目标 / 验收标准）。
    #[serde(default)]
    pub description: String,
    /// 标签（JSON 数组持久化）。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 悬赏发布者（出资方）。
    #[serde(default)]
    pub poster: String,
    /// 奖励金额（最小货币单位；BTC=聪）。悬赏**必须** > 0。
    #[serde(default)]
    pub reward_sats: u64,
    /// 奖励货币：btc/nex/usdc/eth（与 os-wallet `ChainKind` 对齐）。
    #[serde(default = "default_bounty_currency")]
    pub currency: String,
    /// 目标链接（可选）：如停更的 GitHub 仓库 URL / issue。仅作参考，不强制抓取。
    #[serde(default)]
    pub target_url: String,
    /// 状态：open / claimed / submitted / paid / cancelled。
    #[serde(default = "default_bounty_status")]
    pub status: String,
    /// 认领者（hunter）。open/cancelled 时为 ""。
    #[serde(default)]
    pub claimed_by: String,
    /// 交付物链接（hunter 提交：PR / 仓库 URL）。
    #[serde(default)]
    pub solution_url: String,
    /// 截止时间（可选，ISO）。
    #[serde(default)]
    pub deadline: String,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    #[serde(default)]
    pub updated_at: String,
    /// 支付时间（paid 时填）。
    #[serde(default)]
    pub paid_at: String,
    /// 支付收据（自证 txid；phase-2 替换为链上验真）。paid 时填。
    #[serde(default)]
    pub payout_txid: String,
}

/// 悬赏默认状态（open）。
fn default_bounty_status() -> String {
    "open".to_string()
}

/// 悬赏默认货币（btc）；创建时仍须 `reward_sats>0` 且非 `free`（由 `resolve_price` 校验）。
fn default_bounty_currency() -> String {
    "btc".to_string()
}

/// 生成悬赏 id（时间戳纳秒 base36，足够唯一）。
fn new_bounty_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bty{:x}", nanos)
}

/// 列字段序（hub_bounty INSERT/SELECT 共用）。
const BOUNTY_COLUMNS: &str = "id,title,description,tags,poster,reward_sats,currency,\
     target_url,status,claimed_by,solution_url,deadline,created_at,updated_at,paid_at,payout_txid";

fn insert_bounty(conn: &Connection, b: &Bounty) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_bounty ({BOUNTY_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            b.id,
            b.title,
            b.description,
            serde_json::to_string(&b.tags).unwrap_or_else(|_| "[]".into()),
            b.poster,
            b.reward_sats,
            b.currency,
            b.target_url,
            b.status,
            b.claimed_by,
            b.solution_url,
            b.deadline,
            b.created_at,
            b.updated_at,
            b.paid_at,
            b.payout_txid,
        ],
    )?;
    Ok(())
}

fn bounty_from_row(row: &rusqlite::Row) -> rusqlite::Result<Bounty> {
    let tags_json: String = row.get(3)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    Ok(Bounty {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        tags,
        poster: row.get(4)?,
        reward_sats: row.get::<_, i64>(5)?.max(0) as u64,
        currency: row
            .get::<_, Option<String>>(6)?
            .unwrap_or_else(default_bounty_currency),
        target_url: row.get(7)?,
        status: row
            .get::<_, Option<String>>(8)?
            .unwrap_or_else(default_bounty_status),
        claimed_by: row.get(9)?,
        solution_url: row.get(10)?,
        deadline: row.get(11)?,
        created_at: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
        updated_at: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
        paid_at: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
        payout_txid: row.get(15)?,
    })
}

/// 查询单条悬赏（按 id）。
fn find_bounty(conn: &Connection, id: &str) -> rusqlite::Result<Option<Bounty>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BOUNTY_COLUMNS} FROM hub_bounty WHERE id=?"
    ))?;
    stmt.query_row(params![id], bounty_from_row).optional()
}

/// [`claim_bounty`] 的判定结果（handler 映射 200 / 404 / 409）。
/// `Claimed` 装 Box：`Bounty` 本体 368 字节远大于另两个变体，避免整枚举膨胀
/// （clippy::large_enum_variant）。
enum ClaimOutcome {
    /// 认领成功（携带更新后的悬赏，响应体与旧实现一致）。
    Claimed(Box<Bounty>),
    /// 悬赏不存在。
    NotFound,
    /// 非 open 状态（携带当前状态，用于 409 提示文案）。
    NotOpen(String),
}

/// 原子认领（P1 竞态修复）：`UPDATE ... WHERE id=? AND status='open'` 把
/// 「查→判 open→写」压进单语句，以影响行数判定结果。两个并发认领只有一个
/// UPDATE 命中，后到者 0 行 → [`ClaimOutcome::NotOpen`]（409），杜绝旧
/// find(锁1)→判→insert(锁2) 跨锁段的后写覆盖先写者且双双 200 的问题。
fn claim_bounty(conn: &Connection, id: &str, hunter: &str) -> rusqlite::Result<ClaimOutcome> {
    let changed = conn.execute(
        "UPDATE hub_bounty SET status='claimed', claimed_by=?1, updated_at=?2 \
         WHERE id=?3 AND status='open'",
        params![hunter, now_iso(), id],
    )?;
    if changed == 0 {
        // 0 行两因：不存在（404）或已被认领/状态不符（409），补一次读区分
        return Ok(match find_bounty(conn, id)? {
            None => ClaimOutcome::NotFound,
            Some(b) => ClaimOutcome::NotOpen(b.status),
        });
    }
    let b = find_bounty(conn, id)?.expect("UPDATE 刚命中该行，回读必存在");
    Ok(ClaimOutcome::Claimed(Box::new(b)))
}

/// 悬赏列表：`status` 精确状态过滤、`q` 关键词（title/description/tags LIKE）、
/// 默认按创建时间降序。
fn load_bounties(
    conn: &Connection,
    status: Option<&str>,
    q: Option<&str>,
) -> rusqlite::Result<Vec<Bounty>> {
    let mut conds: Vec<String> = Vec::new();
    let mut bind: Vec<String> = Vec::new();
    if let Some(s) = status {
        conds.push("status = ?".to_string());
        bind.push(s.to_string());
    }
    if let Some(q) = q {
        conds.push("(title LIKE ? OR description LIKE ? OR tags LIKE ?)".to_string());
        let like = format!("%{q}%");
        bind.push(like.clone());
        bind.push(like.clone());
        bind.push(like);
    }
    let mut sql = format!("SELECT {BOUNTY_COLUMNS} FROM hub_bounty");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), bounty_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// PR 审核流（2026-08-23 定稿：轻量版——git 通道提交分支 + SQLite 状态机；
// 分支经既有 git push 到裸仓，本层只做归因/审核/合并执行）
// ----------------------------------------------------------------------------

/// 合法 PR 状态集合。
const PR_STATUSES: &[&str] = &["open", "merged", "rejected", "closed"];

/// 单条 PR（hub_pull_requests 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    /// PR id（服务端生成，`pr-<纳秒 hex>`）。
    pub id: String,
    /// 目标仓库名（裸仓 `<repo>.git`）。
    pub repo_name: String,
    /// 标题。
    pub title: String,
    /// 描述（可选）。
    #[serde(default)]
    pub description: String,
    /// 提交者分支名（须已 push 到裸仓）。
    pub source_branch: String,
    /// 提交者节点：本机 PR 恒 `"local"`；联邦 PR（后续期）= 来源节点名。
    #[serde(default = "default_source_node")]
    pub source_node: String,
    /// 提交者链上身份（pubkey；admin 代建为 `"admin"`）。
    pub author_pubkey: String,
    /// EVM 展示名（0x…40hex；admin 代建为 `"admin"`）。
    #[serde(default)]
    pub author_display: String,
    /// 状态：open / merged / rejected / closed。
    #[serde(default = "default_pr_status")]
    pub status: String,
    /// 目标分支（创建时定格为仓库实际默认分支，main→master 回退同快照逻辑）。
    #[serde(default = "default_pr_base")]
    pub base_branch: String,
    /// 审核者（merge/reject 执行者 pubkey/admin；未审核为空）。
    #[serde(default)]
    pub reviewed_by: String,
    /// 审核时间（未审核为空）。
    #[serde(default)]
    pub reviewed_at: String,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    pub updated_at: String,
}

/// PR 默认状态（open）。
fn default_pr_status() -> String {
    "open".to_string()
}

/// PR 默认目标分支（main；创建时按仓库实际默认分支覆盖）。
fn default_pr_base() -> String {
    "main".to_string()
}

/// 生成 PR id（时间戳纳秒 hex，足够唯一；前缀 `pr-` 契约）。
fn new_pr_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pr-{nanos:x}")
}

/// 分支名校验（防 git 参数注入）：非空、不以 `-` 开头、无空白、不含 `..`、
/// 不含 ref 非法字符（`~^:?*[\`）。
/// （pub(crate)：issues.rs 的 PR 分支名校验复用同一套规则。）
pub(crate) fn validate_branch_name(branch: &str) -> Result<(), String> {
    let b = branch.trim();
    if b.is_empty() {
        return Err("分支名不可为空".into());
    }
    if b.starts_with('-') {
        return Err("分支名不可以 '-' 开头".into());
    }
    if b != branch || b.chars().any(|c| c.is_whitespace()) {
        return Err("分支名不可包含空白".into());
    }
    if b.contains("..") || b.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(format!("分支名含非法字符: {b}"));
    }
    Ok(())
}

/// tag 名校验（同分支名校验 + 不可 `.` 开头 / `.lock` 结尾——git ref 规则）。
fn validate_tag_name(tag: &str) -> Result<(), String> {
    validate_branch_name(tag)?;
    let t = tag.trim();
    if t.starts_with('.') || t.starts_with('/') {
        return Err("tag 名不可以 '.' 或 '/' 开头".into());
    }
    if t.ends_with('/') || t.ends_with(".lock") {
        return Err("tag 名不可以 '/' 或 '.lock' 结尾".into());
    }
    if t.len() > 128 {
        return Err("tag 名过长（≤128 字符）".into());
    }
    Ok(())
}

/// 列字段序（hub_pull_requests INSERT/SELECT 共用）。
const PR_COLUMNS: &str = "id,repo_name,title,description,source_branch,source_node,\
     author_pubkey,author_display,status,base_branch,reviewed_by,reviewed_at,\
     created_at,updated_at";

fn insert_pr(conn: &Connection, p: &PullRequest) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_pull_requests ({PR_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            p.id,
            p.repo_name,
            p.title,
            p.description,
            p.source_branch,
            p.source_node,
            p.author_pubkey,
            p.author_display,
            p.status,
            p.base_branch,
            p.reviewed_by,
            p.reviewed_at,
            p.created_at,
            p.updated_at,
        ],
    )?;
    Ok(())
}

fn pr_from_row(row: &rusqlite::Row) -> rusqlite::Result<PullRequest> {
    Ok(PullRequest {
        id: row.get(0)?,
        repo_name: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        source_branch: row.get(4)?,
        source_node: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(default_source_node),
        author_pubkey: row.get(6)?,
        author_display: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
        status: row
            .get::<_, Option<String>>(8)?
            .unwrap_or_else(default_pr_status),
        base_branch: row
            .get::<_, Option<String>>(9)?
            .unwrap_or_else(default_pr_base),
        reviewed_by: row.get(10)?,
        reviewed_at: row.get(11)?,
        created_at: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
        updated_at: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
    })
}

/// 查询单条 PR（按 id + repo 双重定位——PR id 全局唯一，repo 是路由冗余校验）。
fn find_pr(conn: &Connection, repo: &str, id: &str) -> rusqlite::Result<Option<PullRequest>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PR_COLUMNS} FROM hub_pull_requests WHERE id=? AND repo_name=?"
    ))?;
    stmt.query_row(params![id, repo], pr_from_row).optional()
}

/// PR 列表：`repo` 维度 + `status` 可选过滤（须为合法状态），创建时间降序。
fn load_prs(
    conn: &Connection,
    repo: &str,
    status: Option<&str>,
) -> rusqlite::Result<Vec<PullRequest>> {
    let mut sql = format!("SELECT {PR_COLUMNS} FROM hub_pull_requests WHERE repo_name=?");
    let mut bind: Vec<String> = vec![repo.to_string()];
    if let Some(s) = status {
        sql.push_str(" AND status=?");
        bind.push(s.to_string());
    }
    sql.push_str(" ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), pr_from_row)?;
    let mut out = Vec::new();
    for p in iter {
        out.push(p?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// 发版（release）持久化层（2026-08-23 定稿：git tag + SQLite hub_releases）
// ----------------------------------------------------------------------------

/// 单条 release（hub_releases 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// release id（服务端生成，`rel-<纳秒 hex>`；联邦落地保留原 id）。
    pub id: String,
    /// 仓库名。
    pub repo_name: String,
    /// git tag 名（创建时已 `git tag` 到仓库默认分支头）。
    pub tag: String,
    /// 标题。
    #[serde(default)]
    pub title: String,
    /// 发版说明。
    #[serde(default)]
    pub notes: String,
    /// 发版人（恒 `"admin"`——发版是平台级权限；联邦落地保留原值）。
    #[serde(default)]
    pub created_by: String,
    /// 发版时间（RFC3339）。
    pub created_at: String,
}

/// 生成 release id（时间戳纳秒 hex）。
fn new_release_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rel-{nanos:x}")
}

/// 列字段序（hub_releases INSERT/SELECT 共用）。
const RELEASE_COLUMNS: &str = "id,repo_name,tag,title,notes,created_by,created_at";

fn insert_release(conn: &Connection, r: &Release) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_releases ({RELEASE_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?)"
        ),
        params![
            r.id,
            r.repo_name,
            r.tag,
            r.title,
            r.notes,
            r.created_by,
            r.created_at,
        ],
    )?;
    Ok(())
}

fn release_from_row(row: &rusqlite::Row) -> rusqlite::Result<Release> {
    Ok(Release {
        id: row.get(0)?,
        repo_name: row.get(1)?,
        tag: row.get(2)?,
        title: row.get(3)?,
        notes: row.get(4)?,
        created_by: row.get(5)?,
        created_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
    })
}

/// 查询某仓库某 tag 的 release（唯一性键 repo+tag）。
fn find_release(conn: &Connection, repo: &str, tag: &str) -> rusqlite::Result<Option<Release>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELEASE_COLUMNS} FROM hub_releases WHERE repo_name=? AND tag=?"
    ))?;
    stmt.query_row(params![repo, tag], release_from_row)
        .optional()
}

/// release 列表（按仓库，发版时间降序）。
fn list_releases(conn: &Connection, repo: &str) -> rusqlite::Result<Vec<Release>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELEASE_COLUMNS} FROM hub_releases WHERE repo_name=? ORDER BY created_at DESC"
    ))?;
    let iter = stmt.query_map(params![repo], release_from_row)?;
    let mut out = Vec::new();
    for r in iter {
        out.push(r?);
    }
    Ok(out)
}

/// 删除 release 行（repo+tag 定位），返回影响行数。
fn delete_release(conn: &Connection, repo: &str, tag: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM hub_releases WHERE repo_name=? AND tag=?",
        params![repo, tag],
    )
}

// ----------------------------------------------------------------------------
// PR / release 的 git 操作（blocking，spawn_blocking 内执行）
// ----------------------------------------------------------------------------

/// 分支是否真实存在（全 ref 形式杜绝选项注入；同 code_repo::branch_exists_sync）。
fn pr_branch_exists(bare: &str, branch: &str) -> bool {
    run_git_sync(
        bare,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .0
}

/// PR diff 摘要：`git diff <base>..<source> --stat`（spec 契约；全 ref 形式防注入）。
/// 失败（分支被删/仓库移除）降级空串——详情仍可看，不 500。
/// （pub(crate)：issues.rs 的项目级 PR 详情摘要复用同一实现。）
pub(crate) fn pr_diff_stat_blocking(bare: &str, base: &str, source: &str) -> String {
    let (ok, out) = run_git_sync(
        bare,
        &[
            "diff",
            &format!("refs/heads/{base}..refs/heads/{source}"),
            "--stat",
        ],
    );
    if ok {
        out.trim_end().to_string()
    } else {
        String::new()
    }
}

/// 裸仓合并 PR（blocking）：`merge-tree --write-tree`（git ≥2.38，无工作区 3-way
/// 合并）→ `commit-tree`（双 parent 合并提交，内置身份不依赖全局配置）→
/// `update-ref` 推进 base 分支。冲突（merge-tree 退出码 1）返回 `Err`（调用方
/// 转 409）。成功返回新 base 分支头 sha。
/// （pub(crate)：issues.rs 的项目级 PR merge 复用同一实现——两处 PR 语义不同的
/// 是状态机与权限，合并的 git 执行完全同源，不复制代码。）
pub(crate) fn merge_pr_blocking(
    bare: &str,
    base: &str,
    source: &str,
    message: &str,
) -> Result<String, String> {
    let base_ref = format!("refs/heads/{base}");
    let src_ref = format!("refs/heads/{source}");
    // 1. 双方 sha（commit-tree 的 parent 须完整 sha）
    let (bok, bout) = run_git_sync(bare, &["rev-parse", &base_ref]);
    if !bok {
        return Err(format!("目标分支不存在: {base}"));
    }
    let base_sha = bout.trim().to_string();
    let (sok, sout) = run_git_sync(bare, &["rev-parse", &src_ref]);
    if !sok {
        return Err(format!("来源分支不存在: {source}"));
    }
    let src_sha = sout.trim().to_string();
    // 2. 3-way 合成树（冲突 → git 退出码 1，输出含冲突清单）
    let identity = || vec!["-c", "user.name=NexHub", "-c", "user.email=nexhub@local"];
    let mut cmd: Vec<String> = vec!["git".into(), format!("--git-dir={bare}")];
    cmd.extend(identity().into_iter().map(String::from));
    cmd.extend([
        "merge-tree".into(),
        "--write-tree".into(),
        base_ref.clone(),
        src_ref.clone(),
    ]);
    let mt = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .output();
    let mt = mt.map_err(|e| format!("git merge-tree 调用失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&mt.stdout).to_string();
    if !mt.status.success() {
        let detail = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("存在冲突")
            .to_string();
        return Err(format!("合并冲突: {detail}"));
    }
    let tree = stdout.lines().next().unwrap_or_default().trim().to_string();
    if tree.is_empty() {
        return Err("merge-tree 未产出树对象".into());
    }
    // 3. 合并提交（双 parent；identical parent 时 git 自动去重）
    let mut cmd: Vec<String> = vec!["git".into(), format!("--git-dir={bare}")];
    cmd.extend(identity().into_iter().map(String::from));
    cmd.extend([
        "commit-tree".into(),
        tree,
        "-p".into(),
        base_sha.clone(),
        "-p".into(),
        src_sha.clone(),
        "-m".into(),
        message.to_string(),
    ]);
    let ct = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git commit-tree 调用失败: {e}"))?;
    let commit = String::from_utf8_lossy(&ct.stdout).trim().to_string();
    if !ct.status.success() || commit.is_empty() {
        return Err(format!(
            "git commit-tree 失败: {}",
            String::from_utf8_lossy(&ct.stderr).trim()
        ));
    }
    // 4. 推进 base 分支（原子 ref 更新）
    let (uok, _) = run_git_sync(bare, &["update-ref", &base_ref, &commit]);
    if !uok {
        return Err(format!("git update-ref {base} 失败"));
    }
    Ok(commit)
}

/// 打 tag（blocking）：`git tag <tag> <默认分支>`（轻量 tag 定格在默认分支头）。
/// tag 已存在（含用户手动 `git tag` 过、DB 无行的场景）→ Err（409）。
fn tag_release_blocking(bare: &str, tag: &str) -> Result<(), String> {
    let branch = resolve_default_branch_sync(bare);
    let target = format!("refs/heads/{branch}");
    let (ok, out) = run_git_sync_loud(bare, &["tag", tag, &target]);
    if ok {
        return Ok(());
    }
    let err = out.trim();
    if err.contains("already exists") {
        return Err(format!("tag 已存在: {tag}"));
    }
    Err(format!("git tag 失败: {err}"))
}

/// 删 tag（blocking）：`git tag -d <tag>`；tag 不在 git 对象库（如联邦落地行）→
/// 视为已删（Ok）——库行才是权威。
fn delete_tag_blocking(bare: &str, tag: &str) {
    let _ = run_git_sync(bare, &["tag", "-d", tag]);
}

// 单元测试（参考 code_repo.rs 测试风格：纯函数 + 临时目录真实 git fixture）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
        }
    }

    fn delete_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
        }
    }

    // —— 链上身份/admin 测试辅助（真密钥对，k256 与生产同栈）——

    /// 测试注入的系统 admin token（with_admin_token 构造器注入，绕开 env 竞态）。
    const TEST_ADMIN_TOKEN: &str = "nexhub-change-me-admin-token";

    /// 带 Bearer 的 GET。
    fn get_req_auth(path: &str, token: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body: serde_json::Value::Null,
        }
    }

    /// 带 Bearer 的 POST。
    fn post_req_auth(path: &str, token: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body,
        }
    }

    /// 带 Bearer 的 DELETE。
    fn delete_req_auth(path: &str, token: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body: serde_json::Value::Null,
        }
    }

    /// 系统 admin 身份的 POST（回落通道：存量字符串条目/平台托管操作）。
    fn admin_post(path: &str, body: serde_json::Value) -> ApiRequest {
        post_req_auth(path, TEST_ADMIN_TOKEN, body)
    }

    /// 系统 admin 身份的 DELETE。
    fn admin_delete(path: &str) -> ApiRequest {
        delete_req_auth(path, TEST_ADMIN_TOKEN)
    }

    /// 系统 admin 身份的 GET。
    fn admin_get(path: &str) -> ApiRequest {
        get_req_auth(path, TEST_ADMIN_TOKEN)
    }

    /// 生成真 secp256k1 密钥对（CSPRNG）。
    fn new_key() -> k256::ecdsa::SigningKey {
        use k256::elliptic_curve::rand_core::OsRng;
        k256::ecdsa::SigningKey::random(&mut OsRng)
    }

    /// 私钥 → 链上身份（0x + 66 hex 压缩公钥）。
    fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    /// 客户端签名：SHA-256(nonce UTF-8) → RFC6979 ECDSA（65 字节 r||s||v，
    /// 与前端 @noble/secp256k1 sign(sha256(nonce)) 同构）。
    fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> [u8; 65] {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        out
    }

    /// 真密钥对全流程登录：challenge → sign → verify → `(pubkey, token)`。
    async fn login(h: &NexHubLobbyRouteHandler, sk: &k256::ecdsa::SigningKey) -> (String, String) {
        let pubkey = pubkey_hex(sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "challenge 应成功: {}", resp.body);
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = sign_nonce(sk, &nonce);
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": format!("0x{}", hex::encode(sig)),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "verify 应成功: {}", resp.body);
        (pubkey, resp.body["token"].as_str().unwrap().to_string())
    }

    /// 内存库 handler + 测试 admin token（无 nexos 常驻，链上身份用 login 另行登录）。
    fn authed_empty() -> NexHubLobbyRouteHandler {
        NexHubLobbyRouteHandler::with_empty().with_admin_token(TEST_ADMIN_TOKEN)
    }

    /// 直插入库（不触发 git 扫描）——列表/搜索/统计类测试的轻量 fixture。
    fn insert_raw(h: &NexHubLobbyRouteHandler, e: LobbyEntry) {
        let conn = h.db.lock().expect("db poisoned");
        insert_entry(&conn, &e).expect("insert 必成功");
    }

    fn entry(name: &str, description: &str, tags: &[&str], downloads: u64, at: &str) -> LobbyEntry {
        LobbyEntry {
            repo_name: name.to_string(),
            description: description.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            publisher: "tester".to_string(),
            source_url: format!("/tmp/{}.git", name),
            homepage_node: "local".to_string(),
            source_node: default_source_node(),
            // 联邦 HTTP 克隆地址（跨节点拉取用）：fixture 默认空——联邦用例
            // 按需覆写（历史条目形态）。
            clone_url_http: String::new(),
            commit_count: 3,
            size_bytes: 1024,
            default_branch: "main".to_string(),
            last_commit: Some("abc1234 - init".to_string()),
            last_commit_date: Some("2026-08-01 10:00:00 +0800".to_string()),
            readme_excerpt: format!("{name} 的 README 摘要"),
            download_count: downloads,
            published_at: at.to_string(),
            price_sats: 0,
            currency: "free".to_string(),
            federated: false,
            latest_commit: None,
            pushed_at: String::new(),
        }
    }

    // ---- 测试辅助：唯一临时目录 + 真实 git 裸仓库 fixture（2 commits + README）----

    fn tempdir() -> String {
        let p = std::env::temp_dir().join(format!(
            "os-nexhub-lobby-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn run(cmd: &[&str]) -> (bool, String) {
        match std::process::Command::new(cmd[0]).args(&cmd[1..]).output() {
            Ok(out) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
            ),
            Err(_) => (false, String::new()),
        }
    }

    /// 在 repos_dir 下创建真实裸仓库 `<name>.git`（main 分支 + 2 个提交，
    /// HEAD:README.md 为给定文本）。返回裸仓库路径。
    fn make_bare_repo(repos_dir: &str, name: &str, description: &str, readme: &str) -> String {
        let bare = make_bare_repo_at_head(repos_dir, name, "main", "main", readme);
        if !description.is_empty() {
            std::fs::write(format!("{bare}/description"), description).unwrap();
        }
        bare
    }

    /// 造真实裸仓 fixture（默认分支回退探测专用）：工作区 2 个提交（README +
    /// extra.txt），推到裸仓 `HEAD:<pushed>` 分支，再把裸仓 HEAD 显式固定到
    /// `refs/heads/<head>`——模拟不同建仓路径的默认分支状态（如 init 落 master
    /// 而用户只推 main 的"默认分支坑"形态）。返回裸仓库路径。
    fn make_bare_repo_at_head(
        repos_dir: &str,
        name: &str,
        head: &str,
        pushed: &str,
        readme: &str,
    ) -> String {
        let bare = format!("{repos_dir}/{name}.git");
        assert!(
            run(&["git", "init", "--bare", &bare]).0,
            "git init --bare 失败"
        );
        let work = format!("{repos_dir}/.{name}-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(format!("{work}/README.md"), readme).unwrap();
        assert!(run(&["git", "-c", "init.defaultBranch=main", "init", &work]).0);
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init"
            ])
            .0
        );
        std::fs::write(format!("{work}/extra.txt"), "x").unwrap();
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "second"
            ])
            .0
        );
        assert!(
            run(&["git", "-C", &work, "push", &bare, &format!("HEAD:{pushed}")]).0,
            "push HEAD:{pushed} 失败"
        );
        // 显式固定裸仓 HEAD（不受系统/全局 init.defaultBranch 差异影响）
        assert!(
            run(&[
                "git",
                "--git-dir",
                &bare,
                "symbolic-ref",
                "HEAD",
                &format!("refs/heads/{head}")
            ])
            .0,
            "固定 HEAD → refs/heads/{head} 失败"
        );
        let _ = std::fs::remove_dir_all(&work);
        bare
    }

    // 1. 路由表（28 条：2 认证 + 9 lobby + 8 bounty + 6 PR + 3 release，
    //    全归属 nexhub-lobby；读公开 / 写在 handler 内自验链上 token / admin
    //    回落——网关中间件不再拦截）
    #[tokio::test]
    async fn routes_declares_twenty_eight_endpoints_all_nexhub_lobby() {
        let h = NexHubLobbyRouteHandler::with_empty();
        let routes = h.routes().await;
        assert_eq!(
            routes.len(),
            28,
            "应声明 28 条路由（2 认证 + 9 lobby + 8 bounty + 6 PR + 3 release）: {routes:?}"
        );
        assert!(
            routes.iter().all(|r| r.handler_component == COMPONENT),
            "全部归属 {COMPONENT} 组件"
        );
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        // 认证 2 条（公开挑战-签名）
        assert!(pairs.contains(&(HttpMethod::Post, PATH_AUTH_CHALLENGE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_AUTH_VERIFY)));
        // lobby
        assert!(pairs.contains(&(HttpMethod::Get, PATH_LIST)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_STATS)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_ENTITLEMENTS)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_DETAIL)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_PUBLISH)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_FEDERATE)));
        assert!(pairs.contains(&(HttpMethod::Delete, PATH_UNPUBLISH)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_PURCHASE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_CLONE)));
        // bounty
        assert!(pairs.contains(&(HttpMethod::Get, PATH_BOUNTY_LIST)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_BOUNTY_DETAIL)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_BOUNTY_CREATE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_BOUNTY_CLAIM)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_BOUNTY_SUBMIT)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_BOUNTY_APPROVE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_BOUNTY_REJECT)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_BOUNTY_CANCEL)));
        // PR 审核流（6 条：读公开，写 handler 内自验）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_PULLS)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_PULLS)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_PULL_DETAIL)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_PULL_MERGE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_PULL_REJECT)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_PULL_CLOSE)));
        // 发版（3 条：列表公开，创建/删除仅 admin——handler 内自验）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_RELEASES)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_RELEASES)));
        assert!(pairs.contains(&(HttpMethod::Delete, PATH_RELEASE_DELETE)));
        // 鉴权分层（设计 §C）：**全部路由 requires_auth=false、无网关角色**——
        // 公开端点（读 + 认证）天然放行；写端点与 entitlements 的身份闸门
        // （链上 token → pubkey / admin 回落）在 handler 内自验（同 IM 用户面
        // 模式——网关中间件识别不了链上 token，走系统中间件会把 pubkey 调用方
        // 全部挡在 401）。
        for r in &routes {
            assert!(!r.requires_auth, "网关层一律放行: {r:?}");
            assert!(
                r.required_roles.is_empty(),
                "角色判定在 handler 内（pubkey/admin）: {r:?}"
            );
        }
    }

    // 2. 建表 + 发布（真实 git fixture）：快照 commit 数/默认分支/README 摘要
    //    （admin 回落通道：body.publisher 保留）
    #[tokio::test]
    async fn publish_snapshots_real_repo_metadata() {
        let dir = tempdir();
        make_bare_repo(&dir, "demo", "demo repo desc", "# Demo\n这是演示仓库");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let resp = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "demo", "tags": ["rust"], "publisher": "alice"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "发布应 201: {resp:?}");
        assert_eq!(resp.body["repo_name"], "demo");
        assert_eq!(resp.body["commit_count"], 2, "2 个提交");
        assert_eq!(resp.body["default_branch"], "main");
        assert_eq!(resp.body["description"], "demo repo desc");
        assert_eq!(resp.body["owner_kind"], "admin", "admin 回落发布");
        assert_eq!(resp.body["publisher"], "alice", "admin 保留 body.publisher");
        assert!(
            resp.body["readme_excerpt"]
                .as_str()
                .unwrap()
                .contains("这是演示仓库"),
            "摘要应含 README 内容: {resp:?}"
        );
        assert!(resp.body["size_bytes"].as_u64().unwrap() > 0);
        assert!(resp.body["clone_url_ssh"]
            .as_str()
            .unwrap()
            .starts_with("ssh://"));
        // 列表（返回数组）含该条目
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        let arr = list.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["repo_name"], "demo");
        assert_eq!(arr[0]["tags"][0], "rust");
    }

    // 2a. 默认分支坑（外部 agent 接入实测）：init 落 master 而用户只推 main——
    //    裸 HEAD 解析不到内容。回退探测 main → 命中后 README/last_commit 照常可读。
    #[tokio::test]
    async fn snapshot_falls_back_to_main_when_head_branch_missing() {
        let dir = tempdir();
        make_bare_repo_at_head(
            &dir,
            "legacy-head",
            "master",
            "main",
            "# legacy\n只推了 main",
        );
        let snap = snapshot_repo_blocking(&dir, "legacy-head");
        assert_eq!(
            snap.default_branch, "main",
            "HEAD(master) 指向的分支不存在 → 应回退 main: {snap:?}"
        );
        assert!(snap.last_commit.is_some(), "应取到 last_commit: {snap:?}");
        assert!(
            snap.last_commit
                .as_deref()
                .is_some_and(|c| c.contains("second")),
            "last_commit 应为最新提交: {snap:?}"
        );
        assert!(
            snap.readme_excerpt.contains("只推了 main"),
            "README 摘要应可读: {snap:?}"
        );
        assert_eq!(snap.commit_count, 2);
    }

    // 2b. 存量兼容：只有 master 的存量仓（HEAD=master 且 master 存在）直接命中，
    //     README/last_commit 同样取到。
    #[tokio::test]
    async fn snapshot_reads_legacy_master_repo() {
        let dir = tempdir();
        make_bare_repo_at_head(
            &dir,
            "old-master",
            "master",
            "master",
            "# old\nmaster 存量仓",
        );
        let snap = snapshot_repo_blocking(&dir, "old-master");
        assert_eq!(
            snap.default_branch, "master",
            "HEAD=master 且存在 → 直接命中，不误切 main: {snap:?}"
        );
        assert!(snap.last_commit.is_some(), "应取到 last_commit: {snap:?}");
        assert!(
            snap.readme_excerpt.contains("master 存量仓"),
            "README 摘要应可读: {snap:?}"
        );
    }

    // 2c. 建仓 API 产出的新仓形态（HEAD=main 且 main 存在）直接命中。
    #[tokio::test]
    async fn snapshot_hits_main_head_repo_directly() {
        let dir = tempdir();
        make_bare_repo_at_head(&dir, "fresh-main", "main", "main", "# fresh\n新仓 main");
        let snap = snapshot_repo_blocking(&dir, "fresh-main");
        assert_eq!(snap.default_branch, "main", "HEAD=main 直接命中: {snap:?}");
        assert!(snap.last_commit.is_some(), "应取到 last_commit: {snap:?}");
        assert!(
            snap.readme_excerpt.contains("新仓 main"),
            "README 摘要应可读: {snap:?}"
        );
    }

    // 3. 重复发布=刷新快照，且保留 download_count（admin 回落通道）
    #[tokio::test]
    async fn republish_refreshes_snapshot_and_preserves_count() {
        let dir = tempdir();
        make_bare_repo(&dir, "demo", "old desc", "# Demo");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "demo"}),
        ))
        .await
        .unwrap();
        // 克隆一次 → count=1
        h.handle(admin_post(
            "/api/v1/nexhub/lobby/demo/clone",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        // 重复发布（新描述）
        let resp = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "demo", "description": "new desc"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["description"], "new desc", "快照刷新");
        assert_eq!(resp.body["download_count"], 1, "重复发布不重置计数");
        assert_eq!(h.entries_snapshot().len(), 1, "仍只有一条");
    }

    // 3a. 自动同步链快照字段（2026-08-25 §15）：publish 响应/DB 条目带结构化
    //     latest_commit（短 hash+subject+作者+时间——真实 git 解析）与 pushed_at
    //     （RFC3339）；重发布 pushed_at 单调递增（快照刷新时间随每次发布推进）。
    #[tokio::test]
    async fn publish_snapshots_latest_commit_and_pushed_at() {
        let dir = tempdir();
        make_bare_repo(&dir, "snap", "snap repo", "# Snap");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "snap"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        // latest_commit 形状：fixture 最新提交是 user.name=T 的 "second"
        let lc = r.body["latest_commit"].clone();
        assert!(lc.is_object(), "结构化对象: {lc:?}");
        let short = lc["short_hash"].as_str().unwrap();
        assert_eq!(short.len(), 7, "7 位短 hash: {short}");
        assert!(
            short.chars().all(|c| c.is_ascii_hexdigit()),
            "hex 短 hash: {short}"
        );
        assert_eq!(lc["subject"], "second", "subject=最新提交标题: {lc:?}");
        assert_eq!(lc["author"], "T", "author=git %an: {lc:?}");
        assert!(
            lc["date"].as_str().is_some_and(|d| d.contains("202")),
            "ISO 日期: {lc:?}"
        );
        // pushed_at：RFC3339 可解析；published_at 同刷新
        let pushed1 = r.body["pushed_at"].as_str().unwrap().to_string();
        chrono::DateTime::parse_from_rfc3339(&pushed1).expect("pushed_at 应为 RFC3339");
        // DB 落库同构（JSON 列往返不丢）
        let saved = h.entries_snapshot().remove(0);
        let saved_lc = saved.latest_commit.expect("DB 落库 latest_commit");
        assert_eq!(saved_lc.subject, "second");
        assert_eq!(saved_lc.author, "T");
        assert_eq!(saved_lc.short_hash, short);
        assert_eq!(saved.pushed_at, pushed1, "pushed_at 落库");
        // 重发布：pushed_at 单调递增（跨秒后严格递增；同秒也不回退）
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let r2 = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "snap"}),
            ))
            .await
            .unwrap();
        assert_eq!(r2.status, 201);
        let pushed2 = r2.body["pushed_at"].as_str().unwrap().to_string();
        let t1 = chrono::DateTime::parse_from_rfc3339(&pushed1).unwrap();
        let t2 = chrono::DateTime::parse_from_rfc3339(&pushed2).unwrap();
        assert!(t2 > t1, "pushed_at 递增: {pushed1} → {pushed2}");
        // HTTP 列表也返回新字段（前端契约）
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(list.body[0]["latest_commit"]["subject"], "second");
        assert_eq!(list.body[0]["pushed_at"], serde_json::json!(pushed2));
    }

    // 4. 发布不存在的仓库 → 404（需先过身份闸门——带 admin token）
    #[tokio::test]
    async fn publish_missing_repo_returns_404() {
        let dir = tempdir();
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let resp = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "nope"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 5. 搜索 ?q=（name/description/tags LIKE 三通道）
    #[tokio::test]
    async fn search_q_matches_name_description_tags() {
        let h = NexHubLobbyRouteHandler::with_empty();
        insert_raw(
            &h,
            entry(
                "alpha",
                "网络工具集",
                &["net"],
                0,
                "2026-08-01T10:00:00+08:00",
            ),
        );
        insert_raw(
            &h,
            entry(
                "beta",
                "a music player",
                &["audio"],
                0,
                "2026-08-02T10:00:00+08:00",
            ),
        );
        insert_raw(
            &h,
            entry(
                "gamma",
                "misc",
                &["blockchain"],
                0,
                "2026-08-03T10:00:00+08:00",
            ),
        );
        // 命中 name
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby?q=alpha"))
            .await
            .unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 1);
        // 命中 description
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby?q=music"))
            .await
            .unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 1);
        assert_eq!(r.body[0]["repo_name"], "beta");
        // 命中 tags（LIKE 走 JSON 字符串）
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby?q=blockchain"))
            .await
            .unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 1);
        assert_eq!(r.body[0]["repo_name"], "gamma");
        // 无命中
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby?q=zzz"))
            .await
            .unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 0);
    }

    // 6. 标签过滤 ?tag=（精确标签，不前缀误命中）
    #[tokio::test]
    async fn tag_filter_matches_exact_tag() {
        let h = NexHubLobbyRouteHandler::with_empty();
        insert_raw(
            &h,
            entry("r1", "d1", &["rust", "cli"], 0, "2026-08-01T10:00:00+08:00"),
        );
        insert_raw(
            &h,
            entry("r2", "d2", &["rustless"], 0, "2026-08-02T10:00:00+08:00"),
        );
        insert_raw(
            &h,
            entry("r3", "d3", &["ai"], 0, "2026-08-03T10:00:00+08:00"),
        );
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby?tag=rust"))
            .await
            .unwrap();
        let arr = r.body.as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "只命中带 \"rust\" 标签的条目（不误中 rustless）"
        );
        assert_eq!(arr[0]["repo_name"], "r1");
    }

    // 7. 排序：默认 recent（发布时间降序）+ ?sort=downloads（下载量降序）
    #[tokio::test]
    async fn sort_recent_default_and_downloads() {
        let h = NexHubLobbyRouteHandler::with_empty();
        insert_raw(
            &h,
            entry("old-but-hot", "d1", &[], 99, "2026-08-01T10:00:00+08:00"),
        );
        insert_raw(&h, entry("new", "d2", &[], 1, "2026-08-03T10:00:00+08:00"));
        insert_raw(&h, entry("mid", "d3", &[], 5, "2026-08-02T10:00:00+08:00"));
        // 默认 recent：新→旧
        let r = h.handle(get_req(PATH_LIST)).await.unwrap();
        let names: Vec<&str> = r
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["repo_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["new", "mid", "old-but-hot"]);
        // sort=downloads：下载量降序
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby?sort=downloads"))
            .await
            .unwrap();
        let names: Vec<&str> = r
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["repo_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["old-but-hot", "mid", "new"]);
        // 未知 sort 值回落 recent
        assert_eq!(normalize_sort(Some("bogus")), "recent");
        assert_eq!(normalize_sort(Some("downloads")), "downloads");
        assert_eq!(normalize_sort(None), "recent");
    }

    // 8. 详情：readme_excerpt + 双通道 clone 地址（复用 code_repo 构造器）
    #[tokio::test]
    async fn detail_contains_readme_and_dual_clone_urls() {
        let dir = tempdir();
        make_bare_repo(&dir, "proj", "pd", "# Proj readme body");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "proj"}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(get_req("/api/v1/nexhub/lobby/proj"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            resp.body["readme_excerpt"]
                .as_str()
                .unwrap()
                .contains("Proj readme body"),
            "详情应含 readme 摘要: {resp:?}"
        );
        let ssh = resp.body["clone_url_ssh"].as_str().unwrap();
        assert!(ssh.starts_with("ssh://"), "SSH 通道: {ssh}");
        assert!(ssh.ends_with("/proj.git"), "SSH 应以仓库名结尾: {ssh}");
        let http = resp.body["clone_url_http"].as_str().unwrap();
        assert!(http.starts_with("http://"), "HTTP 通道: {http}");
        assert!(http.contains("/git/"), "HTTP 走 Smart Git /git/*: {http}");
        assert!(http.ends_with("/proj.git"), "HTTP 应以仓库名结尾: {http}");
        // 不存在 → 404
        let resp = h
            .handle(get_req("/api/v1/nexhub/lobby/absent"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 8b. 发布定格 clone_url_http（2026-08-25 跨节点拉取修复）：本地条目自带
    //     本节点可达 HTTP 地址（联邦广播的原材料）；详情对联邦条目不覆盖该地址。
    #[tokio::test]
    async fn publish_stamps_clone_url_http_for_federation() {
        let dir = tempdir();
        make_bare_repo(&dir, "stamp-me", "", "# Stamp");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "stamp-me"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        let stamped = h.entries_snapshot().remove(0);
        assert!(
            stamped.clone_url_http.starts_with("http://")
                && stamped.clone_url_http.contains("/git/")
                && stamped.clone_url_http.ends_with("stamp-me.git"),
            "发布应定格本节点 HTTP 克隆地址: {}",
            stamped.clone_url_http
        );
        // 本机条目详情：clone_url_http 为本机双通道地址（与条目定格值一致）
        let d = h
            .handle(get_req("/api/v1/nexhub/lobby/stamp-me"))
            .await
            .unwrap();
        assert_eq!(d.body["clone_url_http"], stamped.clone_url_http);
    }

    // 9. 下架：条目删除但本地仓库不动；重复下架 → 404（admin 回落通道）
    #[tokio::test]
    async fn unpublish_removes_entry_but_keeps_repo() {
        let dir = tempdir();
        let bare = make_bare_repo(&dir, "demo", "", "# Demo");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "demo"}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(admin_delete("/api/v1/nexhub/lobby/demo"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert!(h.entries_snapshot().is_empty(), "条目应已下架");
        assert!(Path::new(&bare).is_dir(), "仓库本身不动（仍存在于 {bare}）");
        // 再删 → 404
        let resp = h
            .handle(admin_delete("/api/v1/nexhub/lobby/demo"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 10. 克隆（本机源，目标已存在=发布路径）：直接注册 + 计数，不 spawn git
    #[tokio::test]
    async fn clone_local_source_registers_and_counts() {
        let dir = tempdir();
        make_bare_repo(&dir, "demo", "", "# Demo");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "demo"}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/demo/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(
            resp.body["cloned"], false,
            "本机已有 → 直接注册，不重复克隆"
        );
        assert_eq!(resp.body["download_count"], 1);
        assert!(resp.body["local_path"]
            .as_str()
            .unwrap()
            .ends_with("demo.git"));
    }

    // 10b. 一键克隆公开（2026-08-25）：免费条目匿名免鉴权直接 200 + 计数；
    //      付费条目匿名不放开（402 引导认证后 purchase）
    #[tokio::test]
    async fn clone_is_public_anonymous_but_paid_still_gated() {
        let dir = tempdir();
        make_bare_repo(&dir, "pub-demo", "", "# Pub");
        make_bare_repo(&dir, "paid-demo", "", "# Paid");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "pub-demo"}),
        ))
        .await
        .unwrap();
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "paid-demo", "price_sats": 300, "currency": "btc"}),
        ))
        .await
        .unwrap();

        // 匿名（无 Authorization）克隆免费条目 → 200 + 计数（拉取不鉴权）
        let resp = h
            .handle(post_req(
                "/api/v1/nexhub/lobby/pub-demo/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "匿名克隆免费条目应放行: {resp:?}");
        assert_eq!(resp.body["cloned"], false, "本机已有 → 直接注册");
        assert_eq!(resp.body["download_count"], 1);
        assert!(
            resp.body["clone_url_http"]
                .as_str()
                .unwrap()
                .contains("/git/"),
            "响应仍带 HTTP clone 地址"
        );

        // 匿名克隆付费条目 → 402（门禁不因匿名放开；购买需身份）
        let resp = h
            .handle(post_req(
                "/api/v1/nexhub/lobby/paid-demo/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 402, "匿名不得绕过付费门禁: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("purchase"),
            "402 应引导 purchase: {resp:?}"
        );
        // 已识别身份（admin）克隆付费条目 → 200（门禁逻辑不变）
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/paid-demo/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "admin 克隆付费条目不受影响: {resp:?}");
    }

    // 11. 克隆（本机源，目标不存在）：git clone --bare 落地到 repos_dir + 计数
    #[tokio::test]
    async fn clone_local_source_into_new_target_clones_bare() {
        let dir = tempdir();
        let src_bare = make_bare_repo(&dir, "src-repo", "", "# Src readme");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        // 直接插入条目（source_url 指向另一个本机裸仓库）
        insert_raw(
            &h,
            LobbyEntry {
                source_url: src_bare.clone(),
                ..entry(
                    "dst-repo",
                    "cloned from src",
                    &["misc"],
                    0,
                    "2026-08-01T10:00:00+08:00",
                )
            },
        );
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/dst-repo/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(
            resp.body["cloned"], true,
            "目标不存在 → 真实 git clone --bare"
        );
        assert_eq!(resp.body["download_count"], 1);
        let target = format!("{dir}/dst-repo.git");
        assert!(Path::new(&target).is_dir(), "裸仓库应落地: {target}");
        // 克隆产物可用（HEAD 指向 main 且含提交）
        let (ok, out) = run_git_sync(&target, &["rev-list", "--count", "--all"]);
        assert!(ok, "克隆产物应是可用裸仓库");
        assert_eq!(out.trim(), "2");
    }

    // 12. 克隆（远端不可达）：502 + 计数不变（连接拒绝快速失败，不触 10s 超时）
    #[tokio::test]
    async fn clone_unreachable_remote_returns_502_without_count() {
        let h = authed_empty();
        insert_raw(
            &h,
            LobbyEntry {
                source_url: "http://127.0.0.1:1/unreachable.git".to_string(),
                ..entry("far", "remote", &[], 0, "2026-08-01T10:00:00+08:00")
            },
        );
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/far/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "远端不可达应 502: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("git clone"));
        assert_eq!(h.entries_snapshot()[0].download_count, 0, "失败不计数");
    }

    // 13. 克隆不存在的条目 → 404
    #[tokio::test]
    async fn clone_missing_entry_returns_404() {
        let dir = tempdir();
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/nope/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 13a. 克隆源选择（纯函数，2026-08-25 跨节点修复）：本机条目（source_node/
    //      homepage_node=local）→ source_url；联邦条目（113 形态：source_node=
    //      node-106 而 homepage_node 仍 local——联邦载荷不改写 homepage_node）
    //      → clone_url_http；source_url 恰在本机存在（跨节点同布局）→ 本机路径。
    #[test]
    fn select_clone_source_picks_local_or_federated_http() {
        // 本机条目 → source_url 路径（现行行为不变）
        assert_eq!(
            select_clone_source(&entry("mine", "d", &[], 0, "2026-08-01T10:00:00+08:00")),
            CloneSource::Local("/tmp/mine.git".to_string()),
            "本机条目（双 node 标记 local）应走 source_url"
        );
        // 联邦条目（113 收到 106 广播的真实形态）→ 条目自带 clone_url_http
        let fed = LobbyEntry {
            source_node: "node-106".to_string(),
            clone_url_http: "http://192.0.2.106:8558/git/nexos.git".to_string(),
            ..entry("nexos", "fed", &[], 0, "2026-08-01T10:00:00+08:00")
        };
        assert_eq!(
            select_clone_source(&fed),
            CloneSource::FederatedHttp("http://192.0.2.106:8558/git/nexos.git".to_string()),
            "联邦条目应走 clone_url_http（source_url 是源节点本机路径）"
        );
        // 联邦条目但 source_url 恰在本机存在（同路径布局）→ 本机直克隆
        let dir = tempdir();
        let bare = make_bare_repo(&dir, "same-layout", "", "# S");
        let fed_local_path = LobbyEntry {
            source_node: "node-106".to_string(),
            source_url: bare,
            ..entry("same-layout", "fed", &[], 0, "2026-08-01T10:00:00+08:00")
        };
        assert!(
            matches!(select_clone_source(&fed_local_path), CloneSource::Local(_)),
            "source_url 本机存在 → 本机路径克隆"
        );
        // 旧主机名 URL 判定（失败提示「重 publish 刷新地址」依据）
        assert!(fed_url_host_is_hostname("http://ub2604:8080/git/x.git"));
        assert!(!fed_url_host_is_hostname(
            "http://192.0.2.106:8558/git/x.git"
        ));
        assert!(!fed_url_host_is_hostname(""));
    }

    // 13b. 本机条目走 source_url：即使条目带 clone_url_http（且指向必死端口），
    //      克隆也只走本机路径——证明选择正确而非碰巧可用。
    #[tokio::test]
    async fn clone_local_entry_uses_source_url_not_fed_http() {
        let dir = tempdir();
        let src_bare = make_bare_repo(&dir, "src13b", "", "# Src 13b");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        insert_raw(
            &h,
            LobbyEntry {
                source_url: src_bare,
                clone_url_http: "http://127.0.0.1:1/dead.git".to_string(),
                ..entry("dst13b", "local first", &[], 0, "2026-08-01T10:00:00+08:00")
            },
        );
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/dst13b/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status, 200,
            "本机路径可用即成功（不走死的 http）: {resp:?}"
        );
        assert_eq!(resp.body["cloned"], true);
        let target = format!("{dir}/dst13b.git");
        assert!(Path::new(&target).is_dir(), "裸仓库应落地: {target}");
        let (ok, out) = run_git_sync(&target, &["rev-list", "--count", "--all"]);
        assert!(ok && out.trim() == "2", "克隆产物应是可用裸仓库: {out}");
    }

    // 13c. 联邦条目走 clone_url_http（113 一键克隆 106 nexos 的修复主路径）：
    //      source_node=node-106 + source_url 指向源节点本机路径（本机不存在），
    //      clone_url_http 用 file:// 指向真实仓库——git 收到的命令参数即条目
    //      自带的 URL（等价于 mock 校验构造的 git 参数），克隆落地为可用裸仓。
    #[tokio::test]
    async fn clone_federated_entry_pulls_via_clone_url_http() {
        let dir = tempdir();
        let src_bare = make_bare_repo(&dir, "fed-src", "", "# Fed src");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        insert_raw(
            &h,
            LobbyEntry {
                source_url: format!("{dir}/no-such-local-path.git"), // 本机不存在
                source_node: "node-106".to_string(),                 // 联邦来源
                clone_url_http: format!("file://{src_bare}"),        // git 远端 URL
                ..entry("fed-proj", "from 106", &[], 0, "2026-08-01T10:00:00+08:00")
            },
        );
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/fed-proj/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status, 200,
            "联邦条目应经 clone_url_http 拉取: {resp:?}"
        );
        assert_eq!(resp.body["cloned"], true);
        assert_eq!(resp.body["source_node"], "node-106");
        assert!(
            resp.body["note"].as_str().unwrap().contains("node-106"),
            "note 应标注来源节点: {resp:?}"
        );
        let target = format!("{dir}/fed-proj.git");
        let (ok, out) = run_git_sync(&target, &["rev-list", "--count", "--all"]);
        assert!(ok && out.trim() == "2", "HTTP 源克隆产物应可用: {out}");
    }

    // 13d. 两者皆无的错误分支（联邦条目 + 历史条目无 clone_url_http）：502 +
    //      「源节点需重 publish 刷新地址」引导 + 计数不变。
    #[tokio::test]
    async fn clone_federated_without_http_url_errors_with_republish_hint() {
        let h = authed_empty();
        insert_raw(
            &h,
            LobbyEntry {
                source_url: "/tank/git-repos/nope.git".to_string(), // 本机不存在
                source_node: "node-106".to_string(),
                clone_url_http: String::new(), // 历史条目（字段加入前发布）
                ..entry(
                    "stale-fed",
                    "old payload",
                    &[],
                    0,
                    "2026-08-01T10:00:00+08:00",
                )
            },
        );
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/stale-fed/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "两源皆无 → 502: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("源节点"), "错误应区分源节点侧: {err}");
        assert!(
            err.contains("重 publish 刷新地址"),
            "应引导重 publish: {err}"
        );
        assert_eq!(h.entries_snapshot()[0].download_count, 0, "失败不计数");
    }

    // 13e. 两者皆无的错误分支（本机条目 + 本机路径不存在）：502 错误信息标注
    //      「本机克隆源不可用」——与 13d 的「源节点不可达」区分定位。
    #[tokio::test]
    async fn clone_local_entry_missing_path_reports_local_error() {
        let h = authed_empty();
        insert_raw(
            &h,
            LobbyEntry {
                source_url: "/tmp/os-nexhub-definitely-missing-13e.git".to_string(),
                ..entry(
                    "gone",
                    "local but gone",
                    &[],
                    0,
                    "2026-08-01T10:00:00+08:00",
                )
            },
        );
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/gone/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "本机路径缺失 → 502: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("本机克隆源不可用"), "错误应区分本机侧: {err}");
        assert!(err.contains("git clone"), "保留 git 原始错误: {err}");
    }

    // 14. 统计聚合：发布数 / 总下载 / top 标签
    #[tokio::test]
    async fn stats_aggregates_counts_and_top_tags() {
        let h = NexHubLobbyRouteHandler::with_empty();
        insert_raw(
            &h,
            entry("a", "d", &["rust", "cli"], 5, "2026-08-01T10:00:00+08:00"),
        );
        insert_raw(
            &h,
            entry("b", "d", &["rust"], 3, "2026-08-02T10:00:00+08:00"),
        );
        insert_raw(&h, entry("c", "d", &["ai"], 1, "2026-08-03T10:00:00+08:00"));
        let resp = h.handle(get_req(PATH_STATS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["published_count"], 3);
        assert_eq!(resp.body["total_downloads"], 9);
        let top = resp.body["top_tags"].as_array().unwrap();
        assert_eq!(top[0]["tag"], "rust", "rust×2 应居首: {top:?}");
        assert_eq!(top[0]["count"], 2);
    }

    /// env 竞态防护（仿 code_repo.rs ENV_LOCK 惯例）：下方 nexos 常驻用例
    /// （15 / 15a / 16a）构造 `with_repos_dir` 时读全局 `NEXOS_LOBBY_NO_AUTO_PUBLISH`，
    /// 16a 会改它——并行测试线程下 15 / 15a 可能被改走 env 而跳过常驻断言失败。
    /// 用模块级 tokio Mutex 把三个 env 依赖用例串行化（覆盖不变；tokio Mutex
    /// 而非 std Mutex：锁需跨 `.await`（HTTP 请求/构造），且各测试独立 runtime）。
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // 15. 常驻：nexos 仓库存在 → 启动自动发布第一条（publisher=NexOS，description
    //     用仓库的）；再次走启动路径仍只 1 条（常驻=刷新，不重复插入）
    #[tokio::test]
    async fn seed_publishes_nexos_when_repo_exists() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempdir();
        make_bare_repo(&dir, "nexos", "NexOS system main repo", "# NexOS");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir);
        let entries = h.entries_snapshot();
        assert_eq!(entries.len(), 1, "开箱不空: {entries:?}");
        assert_eq!(entries[0].repo_name, "nexos");
        assert_eq!(entries[0].publisher, SEED_PUBLISHER);
        assert_eq!(entries[0].description, "NexOS system main repo");
        assert!(entries[0].source_url.ends_with("nexos.git"));
        // 常驻幂等：再次走启动路径不重复插入（直接调 ensure_nexos_published 验证）
        {
            let conn = h.db.lock().expect("db poisoned");
            ensure_nexos_published(&conn, &dir).unwrap();
        }
        assert_eq!(h.entries_snapshot().len(), 1, "常驻幂等（不重复插入）");
    }

    // 15a. 常驻刷新：条目已存在时启动路径**刷新快照**（commit 数/last_commit/
    //      README 摘要）且**保留 download_count**——推送新代码后快照不过期
    #[tokio::test]
    async fn startup_refreshes_existing_nexos_snapshot_and_keeps_count() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempdir();
        make_bare_repo(&dir, "nexos", "NexOS system main repo", "# NexOS v1");
        // 常驻会补装 post-receive 自动同步钩子（默认打本机 8558——开发机上真有
        // os-api 在跑）；本测试下方要真实 push，把钩子目标拨到必死端口隔离副作用
        // （后台 curl 秒败，不影响 push 与断言）。钩子链路本身见 lobby_sync_hook
        // 模块的端到端测试。
        std::env::set_var(
            crate::lobby_sync_hook::ENV_LOBBY_SYNC_API,
            "http://127.0.0.1:9",
        );
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let before = h.entries_snapshot().remove(0);
        assert_eq!(before.commit_count, 2, "fixture 2 个提交");
        // 克隆一次 → download_count=1（刷新必须保留）
        let resp = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/nexos/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        // 模拟推送新代码：clone 出 work 仓库，改 README 新提交后 push 回裸仓库
        let bare = format!("{dir}/nexos.git");
        let work = format!("{dir}/push-work");
        assert!(run(&["git", "clone", &bare, &work]).0, "clone work 失败");
        std::fs::write(format!("{work}/README.md"), "# NexOS v2\n刷新后的摘要").unwrap();
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "third"
            ])
            .0
        );
        assert!(
            run(&["git", "-C", &work, "push", "origin", "main"]).0,
            "push 失败"
        );
        std::env::remove_var(crate::lobby_sync_hook::ENV_LOBBY_SYNC_API);
        // 重启路径（open_db 的同一段逻辑）：无条件刷新既有条目
        {
            let conn = h.db.lock().expect("db poisoned");
            ensure_nexos_published(&conn, &dir).unwrap();
        }
        let entries = h.entries_snapshot();
        assert_eq!(entries.len(), 1, "刷新不重复插入");
        let e = &entries[0];
        assert_eq!(
            e.commit_count,
            before.commit_count + 1,
            "快照刷新：新提交被计入"
        );
        assert!(
            e.readme_excerpt.contains("刷新后的摘要"),
            "README 摘要刷新: {e:?}"
        );
        assert_ne!(e.last_commit, before.last_commit, "last_commit 刷新");
        assert_eq!(e.download_count, 1, "刷新保留 download_count");
    }

    // 16. 常驻：nexos 仓库不存在 → 跳过，大厅为空
    #[tokio::test]
    async fn seed_skipped_when_nexos_repo_absent() {
        let dir = tempdir();
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir);
        assert!(h.entries_snapshot().is_empty(), "无 nexos 仓库则不常驻");
        // 无 nexos.git → 钩子也不装（ensure 只对常驻仓库补装）
        assert!(
            !Path::new(&format!("{dir}/nexos.git/hooks/post-receive")).exists(),
            "无仓库则无钩子"
        );
    }

    // 16b. 自动同步钩子随启动 ensure 补装（2026-08-25 §15）：nexos 仓库存在时，
    //      常驻路径顺带在 <repos>/nexos.git/hooks/post-receive 落钩子脚本
    //      （内容 = 生成器当前产物，env 推导地址/token），幂等可重入；逃生口
    //      env=1 时一并跳过。
    #[tokio::test]
    async fn startup_ensure_installs_nexos_sync_hook() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempdir();
        make_bare_repo(&dir, "nexos", "NexOS system main repo", "# NexOS");
        std::env::set_var(
            crate::lobby_sync_hook::ENV_LOBBY_SYNC_API,
            "http://127.0.0.1:9527",
        );
        std::env::set_var("NEXOS_ADMIN_TOKEN", "hook-env-token");
        let hook = format!("{dir}/nexos.git/hooks/post-receive");
        {
            let h = NexHubLobbyRouteHandler::with_repos_dir(&dir);
            let content = std::fs::read_to_string(&hook).expect("启动即补装钩子");
            assert!(
                content.contains(crate::lobby_sync_hook::HOOK_MARKER),
                "{content}"
            );
            assert!(content.contains(":9527"), "地址取自 env: {content}");
            assert!(
                content.contains("hook-env-token"),
                "token 取自 env: {content}"
            );
            assert!(
                content.contains("/lobby/nexos/federate"),
                "federate 端点: {content}"
            );
            assert_eq!(h.entries_snapshot().len(), 1, "常驻照常");
        }
        // 幂等：重启（再走 ensure 路径）不改动钩子内容
        {
            let conn = Connection::open_in_memory().unwrap();
            create_schema(&conn).unwrap();
            ensure_nexos_published(&conn, &dir).unwrap();
        }
        let again = std::fs::read_to_string(&hook).unwrap();
        assert!(
            again.contains(crate::lobby_sync_hook::HOOK_MARKER)
                && again.contains(":9527")
                && again.contains("hook-env-token"),
            "重复 ensure 钩子内容一致: {again}"
        );
        // 逃生口：env=1 → 常驻与钩子补装一并跳过（删钩子后重跑不补）
        std::env::set_var(ENV_NO_AUTO_PUBLISH, "1");
        std::fs::remove_file(&hook).unwrap();
        {
            let conn = Connection::open_in_memory().unwrap();
            create_schema(&conn).unwrap();
            ensure_nexos_published(&conn, &dir).unwrap();
        }
        assert!(!Path::new(&hook).exists(), "env=1 → 不补装钩子");
        std::env::remove_var(ENV_NO_AUTO_PUBLISH);
        std::env::remove_var(crate::lobby_sync_hook::ENV_LOBBY_SYNC_API);
        std::env::remove_var("NEXOS_ADMIN_TOKEN");
    }

    // 16a. 逃生口：env NEXOS_LOBBY_NO_AUTO_PUBLISH=1 → 启动跳过常驻（发布与
    //      刷新均不做）——用户显式下架 nexos 后不想被启动拉回
    #[tokio::test]
    async fn env_escape_hatch_skips_auto_publish() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempdir();
        make_bare_repo(&dir, "nexos", "NexOS system main repo", "# NexOS");
        std::env::set_var(ENV_NO_AUTO_PUBLISH, "1");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir);
        assert!(h.entries_snapshot().is_empty(), "env=1 → 启动不自动发布");
        // 已有条目（如用户自管/重发布的 nexos）也不被启动刷新拉回（env 仍为 1）
        insert_raw(
            &h,
            entry(
                "nexos",
                "用户自管条目",
                &["custom"],
                7,
                "2026-08-01T10:00:00+08:00",
            ),
        );
        {
            let conn = h.db.lock().expect("db poisoned");
            ensure_nexos_published(&conn, &dir).unwrap();
        }
        std::env::remove_var(ENV_NO_AUTO_PUBLISH);
        let e = &h.entries_snapshot()[0];
        assert_eq!(
            e.description, "用户自管条目",
            "env=1 → 跳过刷新（描述不被覆盖）"
        );
        assert_eq!(e.download_count, 7, "计数不受影响");
    }

    // 17. 纯函数：README 摘要截断（UTF-8 安全）+ 名称校验
    #[test]
    fn excerpt_and_name_validation_pure_functions() {
        let long = "汉".repeat(600);
        let ex = excerpt_of(&long, README_EXCERPT_CHARS);
        assert_eq!(ex.chars().count(), 500);
        assert!(excerpt_of("short", 500) == "short");
        // 名称校验（防 git 参数注入 / 路径穿越）
        assert!(validate_repo_name("").is_err());
        assert!(validate_repo_name("../x").is_err());
        assert!(validate_repo_name("a/b").is_err());
        assert!(validate_repo_name("-evil").is_err());
        assert!(validate_repo_name("good-name_1").is_ok());
    }

    // 18. 兜底 404 + 非法名 400（publish 走 admin 回落通道）
    #[tokio::test]
    async fn unmatched_route_and_bad_name_return_4xx() {
        let h = authed_empty();
        let resp = h
            .handle(get_req("/api/v1/nexhub/lobby/x/y/z"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        // 以 '-' 开头的名（git 参数注入防护）→ 400
        let resp = h
            .handle(get_req("/api/v1/nexhub/lobby/-evil"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非法名应 400: {resp:?}");
        let resp = h
            .handle(admin_post(PATH_PUBLISH, serde_json::json!({"repo": "-x"})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<NexHubLobbyRouteHandler>();
    }

    // 19. 货币化：发布免费/付费 + 非法货币校验（§10，admin 回落通道）
    #[tokio::test]
    async fn publish_free_and_paid_persists_price_and_currency() {
        let dir = tempdir();
        make_bare_repo(&dir, "free-repo", "", "# Free");
        make_bare_repo(&dir, "paid-repo", "", "# Paid");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        // 免费（省略 price）→ currency 强制 free
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "free-repo"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["price_sats"], 0);
        assert_eq!(r.body["currency"], "free");
        // 付费（btc, 1000 聪）
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "paid-repo", "price_sats": 1000, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["price_sats"], 1000);
        assert_eq!(r.body["currency"], "btc");
        // 付费但 currency=free → 400
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "paid-repo", "price_sats": 1000, "currency": "free"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "付费 currency 不得为 free: {r:?}");
    }

    // 20. 货币化门禁（§10 + §C 身份化）：付费未购 → 402；购买后 → 200；
    //     owner pubkey 豁免（身份比对，非字符串冒名）；admin 恒可；支付不足 → 402
    #[tokio::test]
    async fn paid_clone_requires_purchase_then_succeeds() {
        let dir = tempdir();
        make_bare_repo(&dir, "paid-src", "", "# Paid src");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        // owner（链上身份）发布付费条目
        let owner = new_key();
        let (owner_pk, owner_token) = login(&h, &owner).await;
        let buyer_sk = new_key();
        let (buyer_pk, buyer_token) = login(&h, &buyer_sk).await;
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &owner_token,
                serde_json::json!({"repo": "paid-src", "price_sats": 500, "currency": "btc", "publisher": "forged-name"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["publisher"], owner_pk, "publisher=token pubkey");
        // 他人未购 → 402
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/paid-src/clone",
                &buyer_token,
                serde_json::json!({ "buyer": owner_pk }), // body 自报 buyer 已不参与豁免
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 402, "未购（冒名 buyer 也不豁免）应拒绝: {r:?}");
        // 购买（buyer=token 身份，自报 buyer 忽略）
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/paid-src/purchase",
                &buyer_token,
                serde_json::json!({"buyer": "forged-attacker", "txid": "tx_abc", "amount_sats": 500, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "购买应成功: {r:?}");
        assert_eq!(r.body["buyer"], buyer_pk, "buyer 应为 token 身份");
        // 已购 → 克隆 200
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/paid-src/clone",
                &buyer_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "已购应可克隆: {r:?}");
        assert_eq!(r.body["download_count"], 1);
        // owner 本人豁免（buyer==条目 owner pubkey 的身份比对）
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/paid-src/clone",
                &owner_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "owner pubkey 豁免: {r:?}");
        // admin 恒可
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/paid-src/clone",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 克隆放行: {r:?}");
        // 支付不足 → 402（第三个身份）
        let (carol_pk, carol_token) = login(&h, &new_key()).await;
        let _ = carol_pk;
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/paid-src/purchase",
                &carol_token,
                serde_json::json!({"txid": "tx_c", "amount_sats": 100, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 402, "支付不足应拒绝: {r:?}");
    }

    // 21. verify_payment 纯函数：货币/金额/收据指纹校验
    #[test]
    fn verify_payment_rejects_wrong_currency_shortfall_empty_txid() {
        let base = Entitlement {
            repo_name: "r".into(),
            buyer: "b".into(),
            chain: "btc".into(),
            txid: "tx1".into(),
            amount_sats: 1000,
            currency: "btc".into(),
            paid_at: "now".into(),
            chain_block: None,
            chain_value_wei: None,
        };
        assert!(verify_payment(&base, 1000, "btc").is_ok());
        assert!(verify_payment(&base, 1000, "eth").is_err(), "货币不符");
        assert!(verify_payment(&base, 2000, "btc").is_err(), "金额不足");
        let empty = Entitlement {
            txid: String::new(),
            ..base.clone()
        };
        assert!(verify_payment(&empty, 1000, "btc").is_err(), "空 txid");
    }

    // 22. resolve_price 纯函数：免费/付费推导与非法货币
    #[test]
    fn resolve_price_free_and_paid_rules() {
        assert_eq!(resolve_price(None, None).unwrap(), (0, "free".to_string()));
        assert_eq!(
            resolve_price(Some(0), Some("btc".into())).unwrap(),
            (0, "free".to_string())
        );
        assert_eq!(
            resolve_price(Some(100), None).unwrap(),
            (100, "btc".to_string())
        );
        assert_eq!(
            resolve_price(Some(100), Some("nex".into())).unwrap(),
            (100, "nex".to_string())
        );
        assert!(resolve_price(Some(100), Some("free".into())).is_err());
        assert!(resolve_price(Some(100), Some("doge".into())).is_err());
    }

    // ---- 悬赏（bounty）测试辅助：admin 身份发布一条悬赏（poster=alice，
    //      admin 回落通道保留 body.poster）并返回 id ----
    async fn create_bounty(
        h: &NexHubLobbyRouteHandler,
        reward_sats: u64,
        currency: &str,
    ) -> String {
        let resp = h
            .handle(admin_post(
                PATH_BOUNTY_CREATE,
                serde_json::json!({
                    "title": "更新停更的 github 项目",
                    "description": "给某 repo 修 CI 并发布新版本",
                    "reward_sats": reward_sats,
                    "currency": currency,
                    "target_url": "https://github.com/foo/bar",
                    "poster": "alice"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "发布悬赏应 201: {resp:?}");
        resp.body["id"].as_str().unwrap().to_string()
    }

    // 23. 悬赏必须 >0 且非 free；缺 currency 默认 btc（admin 回落通道）
    #[tokio::test]
    async fn bounty_requires_positive_reward_and_valid_currency() {
        let h = authed_empty();
        let r = h
            .handle(admin_post(
                PATH_BOUNTY_CREATE,
                serde_json::json!({"title": "x", "reward_sats": 0, "currency": "free"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "悬赏必须 >0 且非 free: {r:?}");
        let r = h
            .handle(admin_post(
                PATH_BOUNTY_CREATE,
                serde_json::json!({"title": "y", "reward_sats": 500}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "缺 currency 应默认 btc: {r:?}");
        assert_eq!(r.body["currency"], "btc");
        assert_eq!(r.body["status"], "open");
    }

    // 24. 悬赏完整生命周期：open → claimed → submitted → paid（自证支付）
    //     （hunter=链上 token 身份；poster=admin 回落——admin 恒可验收）
    #[tokio::test]
    async fn bounty_full_lifecycle_open_to_paid() {
        let h = authed_empty();
        let id = create_bounty(&h, 1000, "btc").await;
        let d = h
            .handle(get_req(&format!("/api/v1/nexhub/bounty/{id}")))
            .await
            .unwrap();
        assert_eq!(d.body["status"], "open");
        // hunter 登录（链上身份）
        let (hunter_pk, hunter_token) = login(&h, &new_key()).await;
        // claim（hunter = token 身份，body 自报忽略）
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/claim"),
                &hunter_token,
                serde_json::json!({"hunter": "forged-attacker"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["status"], "claimed");
        assert_eq!(r.body["claimed_by"], hunter_pk, "hunter 应为 token pubkey");
        // submit
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/submit"),
                &hunter_token,
                serde_json::json!({"hunter": "forged-attacker", "solution_url": "https://github.com/foo/bar/pull/1"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["status"], "submitted");
        // approve（admin 回落通道；支付足额）
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                serde_json::json!({"txid": "tx_pay", "amount_sats": 1000, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "验收支付应 200: {r:?}");
        assert_eq!(r.body["winner"], hunter_pk);
        assert_eq!(r.body["payout_txid"], "tx_pay");
        // 详情确认 paid
        let d = h
            .handle(get_req(&format!("/api/v1/nexhub/bounty/{id}")))
            .await
            .unwrap();
        assert_eq!(d.body["status"], "paid");
        assert_eq!(d.body["payout_txid"], "tx_pay");
        assert_eq!(d.body["claimed_by"], hunter_pk);
    }

    // 25. 验收支付不足 → 402；非 submitted 状态验收 → 409
    #[tokio::test]
    async fn bounty_approve_shortfall_or_wrong_state_returns_error() {
        let h = authed_empty();
        let id = create_bounty(&h, 1000, "btc").await;
        // 未提交直接验收 → 409
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                serde_json::json!({"txid": "t", "amount_sats": 1000, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "非 submitted 不可验收: {r:?}");
        // 提交后支付不足 → 402
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "u"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                serde_json::json!({"txid": "t", "amount_sats": 100, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 402, "支付不足应 402: {r:?}");
    }

    // 26. 取消仅 open 可；claim 后取消 → 409（admin 回落通道）
    #[tokio::test]
    async fn bounty_cancel_only_from_open() {
        let h = authed_empty();
        let id = create_bounty(&h, 1000, "btc").await;
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/cancel"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["status"], "cancelled");
        let id2 = create_bounty(&h, 1000, "btc").await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id2}/claim"),
            &hunter_token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id2}/cancel"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "claim 后不可取消: {r:?}");
    }

    // 27. 驳回（reject）submitted → open 重开，清除认领/交付（admin 回落通道）
    #[tokio::test]
    async fn bounty_reject_reopens_and_clears() {
        let h = authed_empty();
        let id = create_bounty(&h, 1000, "btc").await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "u"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/reject"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["status"], "open");
        assert_eq!(r.body["claimed_by"], "");
        assert_eq!(r.body["solution_url"], "");
    }

    // 28. 列表过滤：?status= 精确状态 + ?q= 关键词（title/description/tags）
    #[tokio::test]
    async fn bounty_list_filters_by_status_and_q() {
        let h = authed_empty();
        let id1 = create_bounty(&h, 1000, "btc").await;
        let id2 = create_bounty(&h, 500, "nex").await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id1}/claim"),
            &hunter_token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        // ?status=open → 仅 id2
        let r = h
            .handle(get_req("/api/v1/nexhub/bounty?status=open"))
            .await
            .unwrap();
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "只应返回 open 的: {arr:?}");
        assert_eq!(arr[0]["id"], id2);
        // ?q= 命中标题「更新停更」
        let r = h
            .handle(get_req("/api/v1/nexhub/bounty?q=更新停更"))
            .await
            .unwrap();
        assert_eq!(
            r.body.as_array().unwrap().len(),
            2,
            "q 应命中全部两条: {r:?}"
        );
    }

    // 29. 授权记录查询（GET /entitlements，需身份）：?buyer= 自查 / ?repo= 审计 /
    //     组合 / 全量（buyer 归因 = token 身份）
    #[tokio::test]
    async fn entitlements_query_by_repo_and_buyer() {
        let dir = tempdir();
        make_bare_repo(&dir, "paid-a", "", "# A");
        make_bare_repo(&dir, "paid-b", "", "# B");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        for repo in ["paid-a", "paid-b"] {
            let r = h
                .handle(admin_post(
                    PATH_PUBLISH,
                    serde_json::json!({"repo": repo, "price_sats": 100, "currency": "btc"}),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 201, "{r:?}");
        }
        // bob 买 paid-a；carol 买 paid-a 和 paid-b（buyer=各自 token 身份）
        let (bob_pk, bob_token) = login(&h, &new_key()).await;
        let (carol_pk, carol_token) = login(&h, &new_key()).await;
        for (repo, (buyer_pk, buyer_token)) in [
            ("paid-a", (&bob_pk, &bob_token)),
            ("paid-a", (&carol_pk, &carol_token)),
            ("paid-b", (&carol_pk, &carol_token)),
        ] {
            let r = h
                .handle(post_req_auth(
                    &format!("/api/v1/nexhub/lobby/{repo}/purchase"),
                    buyer_token,
                    serde_json::json!({"txid": format!("tx_{}_{}", &buyer_pk[2..10], repo), "amount_sats": 100, "currency": "btc"}),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 200, "{r:?}");
            assert_eq!(r.body["buyer"], *buyer_pk, "buyer 应为 token 身份");
        }
        // 无身份查询 → 401
        let anon = h.handle(get_req(PATH_ENTITLEMENTS)).await.unwrap();
        assert_eq!(anon.status, 401, "entitlements 需身份: {anon:?}");
        // ?buyer= 自查（carol 两条、bob 一条）
        let r = h
            .handle(get_req_auth(
                &format!("/api/v1/nexhub/lobby/entitlements?buyer={carol_pk}"),
                &carol_token,
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "carol 应有两条授权: {arr:?}");
        assert!(arr.iter().all(|e| e["buyer"] == carol_pk));
        // ?repo= 审计（paid-a 两个买家）
        let r = h
            .handle(admin_get("/api/v1/nexhub/lobby/entitlements?repo=paid-a"))
            .await
            .unwrap();
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "paid-a 应有两条授权: {arr:?}");
        assert!(arr.iter().all(|e| e["repo_name"] == "paid-a"));
        // 组合精确定位
        let r = h
            .handle(admin_get(&format!(
                "/api/v1/nexhub/lobby/entitlements?repo=paid-b&buyer={carol_pk}"
            )))
            .await
            .unwrap();
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["buyer"], carol_pk);
        // 无参数全量（admin 审计）
        let r = h.handle(admin_get(PATH_ENTITLEMENTS)).await.unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 3);
        // 记录含支付字段（审计留痕）
        assert!(r.body[0]["paid_at"].is_string());
        assert!(r.body[0]["amount_sats"].is_u64());
    }

    // 30. 重复认领 → 409（P1 竞态修复：原子 UPDATE，后到者不覆盖先认领者）；
    //     不存在的悬赏认领 → 404（保持既有行为）
    #[tokio::test]
    async fn bounty_double_claim_returns_409_keeps_first_hunter() {
        let h = authed_empty();
        let id = create_bounty(&h, 1000, "btc").await;
        let (bob_pk, bob_token) = login(&h, &new_key()).await;
        let (_, alice_token) = login(&h, &new_key()).await;
        // bob 先认领成功
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/claim"),
                &bob_token,
                serde_json::json!({"hunter": "self-reported"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["status"], "claimed");
        assert_eq!(
            r.body["claimed_by"], bob_pk,
            "hunter=token 身份（自报忽略）"
        );
        // alice 后到 → 409，不覆盖 bob
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/claim"),
                &alice_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "重复认领应 409: {r:?}");
        assert!(
            r.body["error"]
                .as_str()
                .unwrap()
                .contains("仅 open 状态可认领"),
            "409 文案应说明当前状态: {r:?}"
        );
        let d = h
            .handle(get_req(&format!("/api/v1/nexhub/bounty/{id}")))
            .await
            .unwrap();
        assert_eq!(d.body["status"], "claimed");
        assert_eq!(d.body["claimed_by"], bob_pk, "后到认领不得覆盖先认领者");
        // 已 paid 的悬赏再认领 → 409（状态机其余分支不回归）
        let id2 = create_bounty(&h, 500, "nex").await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id2}/submit"),
            &bob_token,
            serde_json::json!({"solution_url": "https://x"}),
        ))
        .await
        .unwrap();
        h.handle(admin_post(
            &format!("/api/v1/nexhub/bounty/{id2}/approve"),
            serde_json::json!({"txid": "tx_p", "amount_sats": 500, "currency": "nex"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id2}/claim"),
                &alice_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "paid 不可认领: {r:?}");
        // 不存在 → 404（与旧实现一致）
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/bounty/btynonexistent/claim",
                &alice_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 404, "悬赏不存在应 404: {r:?}");
    }

    // 31. 旧 14 列库迁移（P0 部署红线）：线上存量库缺 price_sats/currency，
    //     建表后幂等补列——旧数据保留、列表非空、新列回填默认值、16 列 INSERT 可用、
    //     迁移幂等可重入。
    #[tokio::test]
    async fn migrates_legacy_14_column_db_preserving_data() {
        let dir = tempdir();
        let db_path = format!("{dir}/hub_lobby_legacy.db");
        {
            // 照抄线上旧 schema（repo 根 hub_lobby.db 实测 14 列）+ 一条存量数据
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE hub_lobby (
                    repo_name       TEXT PRIMARY KEY,
                    description     TEXT DEFAULT '',
                    tags            TEXT DEFAULT '[]',
                    publisher       TEXT DEFAULT '',
                    source_url      TEXT DEFAULT '',
                    homepage_node   TEXT DEFAULT 'local',
                    commit_count    INTEGER DEFAULT 0,
                    size_bytes      INTEGER DEFAULT 0,
                    default_branch  TEXT DEFAULT 'master',
                    last_commit     TEXT,
                    last_commit_date TEXT,
                    readme_excerpt  TEXT DEFAULT '',
                    download_count  INTEGER DEFAULT 0,
                    published_at    TEXT
                );
                CREATE INDEX idx_hub_lobby_downloads ON hub_lobby(download_count);
                INSERT INTO hub_lobby (repo_name, description, tags, publisher, source_url,
                    homepage_node, commit_count, size_bytes, default_branch, last_commit,
                    last_commit_date, readme_excerpt, download_count, published_at)
                VALUES ('legacy-repo', '旧库存量条目', '[\"legacy\"]', 'old-publisher',
                    '/tmp/legacy-repo.git', 'local', 7, 4096, 'main', 'abc0001 - old commit',
                    '2026-01-01 00:00:00 +0800', 'legacy readme', 42,
                    '2026-01-01T00:00:00+08:00');",
            )
            .unwrap();
        }
        // 迁移前复现线上症状：16 列 SELECT 直接报 no such column
        {
            let conn = Connection::open(&db_path).unwrap();
            assert!(
                conn.execute_batch(&format!("SELECT {ENTRY_COLUMNS} FROM hub_lobby"))
                    .is_err(),
                "旧库缺列时 16 列 SELECT 必失败（P0 复现）"
            );
        }
        // 走真实构造路径（open_db → create_schema → 迁移；目录无 nexos.git → 常驻跳过）
        let h = NexHubLobbyRouteHandler::with_db_path(&db_path, &dir)
            .with_admin_token(TEST_ADMIN_TOKEN);
        let entries = h.entries_snapshot();
        assert_eq!(entries.len(), 1, "旧数据保留，列表非空");
        let e = &entries[0];
        assert_eq!(e.repo_name, "legacy-repo");
        assert_eq!(e.description, "旧库存量条目");
        assert_eq!(e.tags, vec!["legacy".to_string()]);
        assert_eq!(e.download_count, 42, "旧列数据不受迁移影响");
        assert_eq!(e.price_sats, 0, "补列回填默认 0（免费）");
        assert_eq!(e.currency, "free", "补列回填默认 free");
        // GET 列表返回旧条目（不再被吞成 200 空数组）
        let r = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(r.status, 200);
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "HTTP 列表非空: {arr:?}");
        assert_eq!(arr[0]["repo_name"], "legacy-repo");
        assert_eq!(arr[0]["price_sats"], 0);
        assert_eq!(arr[0]["currency"], "free");
        assert_eq!(arr[0]["federated"], false, "补列回填默认未联邦");
        // 迁移后发布（16 列 INSERT）在旧表上可用（admin 回落通道）
        make_bare_repo(&dir, "new-repo", "new desc", "# New");
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "new-repo", "price_sats": 300, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "迁移后发布（付费）应可用: {r:?}");
        assert_eq!(r.body["price_sats"], 300);
        assert_eq!(h.entries_snapshot().len(), 2, "新旧条目共存");
        // 幂等：重复跑 create_schema 不重复补列、不报错、数据不丢
        {
            let conn = h.db.lock().expect("db poisoned");
            create_schema(&conn).unwrap();
            create_schema(&conn).unwrap();
        }
        assert_eq!(h.entries_snapshot().len(), 2, "迁移幂等可重入");
    }

    // 32. 真实旧库验收（P0）：NEXHUB_TEST_LEGACY_DB 指向真实 14 列旧库文件时，
    //     复制到临时路径走迁移构造路径，断言列表非空（验收项：存量库升级即用）。
    //     未设置环境变量则静默跳过（CI 无该文件时不空跑）。
    #[tokio::test]
    async fn migrates_real_legacy_db_when_env_provided() {
        let Ok(src) = std::env::var("NEXHUB_TEST_LEGACY_DB") else {
            return;
        };
        let dir = tempdir();
        let dst = format!("{dir}/real-copy.db");
        std::fs::copy(&src, &dst).expect("复制旧库主文件失败");
        // WAL 侧文件一并复制，避免未 checkpoint 的已提交数据丢失
        for side in ["-wal", "-shm"] {
            if Path::new(&format!("{src}{side}")).exists() {
                std::fs::copy(format!("{src}{side}"), format!("{dst}{side}"))
                    .expect("复制旧库 WAL 侧文件失败");
            }
        }
        let h = NexHubLobbyRouteHandler::with_db_path(&dst, &dir);
        let entries = h.entries_snapshot();
        assert!(
            !entries.is_empty(),
            "真实旧库迁移后列表必须非空: {entries:?}"
        );
        for e in &entries {
            assert_eq!(e.currency, "free", "存量条目补列默认免费: {e:?}");
        }
        let r = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(r.status, 200);
        assert!(
            !r.body.as_array().unwrap().is_empty(),
            "HTTP 列表必须非空: {r:?}"
        );
    }

    // =========================================================================
    // 链上身份与权限（docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C）——真密钥对全流程
    // =========================================================================

    /// C1. challenge：合法公钥 → 256-bit nonce + TTL + EVM 展示名；非法公钥 → 400。
    #[tokio::test]
    async fn chain_auth_challenge_and_invalid_pubkey() {
        let h = authed_empty();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let nonce = resp.body["nonce"].as_str().unwrap();
        assert_eq!(nonce.len(), 64, "256-bit hex");
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(resp.body["expires_in"], chain_auth::NONCE_TTL_SECS);
        let display = resp.body["display_name"].as_str().unwrap();
        assert!(
            display.starts_with("0x") && display.len() == 42,
            "EVM 地址 0x+40hex: {display}"
        );
        // 非法 pubkey → 400（缺 0x / 非 hex / 长度错）
        for bad in [
            pubkey[2..].to_string(),
            format!("0x{}zz", &pubkey[2..66]),
            "0x".to_string(),
        ] {
            let resp = h
                .handle(post_req(
                    PATH_AUTH_CHALLENGE,
                    serde_json::json!({ "pubkey": bad }),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "非法 pubkey 应 400: {bad}");
        }
    }

    /// C2. verify：真密钥对全流程 challenge→sign→verify→token（24h + 身份回显）。
    #[tokio::test]
    async fn chain_auth_verify_full_flow() {
        let h = authed_empty();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk).await;
        assert_eq!(token.len(), 64, "256-bit hex token");
        // 再走一遍校验响应字段
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = sign_nonce(&sk, &nonce);
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": hex::encode(sig), // 不带 0x 前缀也应可解
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["expires_in"], chain_auth::TOKEN_TTL_SECS);
        assert_eq!(resp.body["pubkey"], pubkey);
        assert!(resp.body["display_name"]
            .as_str()
            .unwrap()
            .starts_with("0x"));
    }

    /// C3. nonce 重放拒绝（用后即焚）/ 错误 nonce / 伪造签名 / 非法签名格式。
    #[tokio::test]
    async fn chain_auth_replay_wrong_nonce_forged_sig_rejected() {
        let h = authed_empty();
        let sk = new_key();
        let attacker = new_key();
        let pubkey = pubkey_hex(&sk);
        // —— 重放：同 nonce 二次 verify → 401 ——
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = hex::encode(sign_nonce(&sk, &nonce));
        let first = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": sig }),
            ))
            .await
            .unwrap();
        assert_eq!(first.status, 200, "首次 verify 应成功");
        let replay = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": sig }),
            ))
            .await
            .unwrap();
        assert_eq!(replay.status, 401, "nonce 重放应 401（用后即焚）");
        // —— 伪造签名（另一把私钥签）→ 401 ——
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let forged = hex::encode(sign_nonce(&attacker, &nonce));
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": forged }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "伪造签名应 401");
        // —— 签名格式非法 → 400 ——
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        for bad in ["zzzz".to_string(), hex::encode([0u8; 64])] {
            let resp = h
                .handle(post_req(
                    PATH_AUTH_VERIFY,
                    serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": bad }),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "签名格式非法应 400: {bad}");
        }
    }

    /// C4. 单点登录 + token 实例独立：同 pubkey 二次 verify 顶掉旧 token；
    ///     handler 各自独立 ChainAuth 实例（IM 与 NexHub 的 token 桶互不相通）。
    #[tokio::test]
    async fn chain_auth_single_login_and_instance_isolation() {
        // h1：另一 handler（独立 ChainAuth 实例）——其 token 不应被 h 认
        let h1 = authed_empty();
        let sk = new_key();
        let (_, foreign_token) = login(&h1, &sk).await;
        // h：正式被测 handler
        let dir = tempdir();
        make_bare_repo(&dir, "iso-repo", "", "# Iso");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (pubkey, token) = login(&h, &sk).await;
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &token,
                serde_json::json!({"repo": "iso-repo"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "链上身份发布应 201: {r:?}");
        // 同一密钥再登录 → 旧 token 失效（单点登录）
        let (_, new_token) = login(&h, &sk).await;
        assert_ne!(token, new_token);
        let stale = h
            .handle(post_req_auth(
                PATH_BOUNTY_CREATE,
                &token,
                serde_json::json!({"title": "x", "reward_sats": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status, 401, "旧 token 应被顶掉");
        let fresh = h
            .handle(post_req_auth(
                PATH_BOUNTY_CREATE,
                &new_token,
                serde_json::json!({"title": "x", "reward_sats": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(fresh.status, 201, "新 token 应可用");
        // h1 上签发的 token 在 h（独立实例）不可用——token 桶互不相通
        let foreign = h
            .handle(post_req_auth(
                PATH_BOUNTY_CREATE,
                &foreign_token,
                serde_json::json!({"title": "x", "reward_sats": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(foreign.status, 401, "他实例 token 应 401（独立 ChainAuth）");
        let _ = pubkey;
    }

    /// C5. pubkey 发布：publisher=token pubkey（body 自报忽略）、owner_kind=pubkey、
    ///     publisher_display=EVM 地址。
    #[tokio::test]
    async fn chain_publish_attributes_owner_from_token() {
        let dir = tempdir();
        make_bare_repo(&dir, "mine", "desc", "# Mine");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (pubkey, token) = login(&h, &new_key()).await;
        let display = chain_auth::derive_display_name(
            &chain_auth::parse_pubkey(&pubkey).expect("pubkey 应可解析"),
        );
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &token,
                serde_json::json!({"repo": "mine", "publisher": "forged-attacker"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "链上身份发布应 201: {r:?}");
        assert_eq!(r.body["publisher"], pubkey, "publisher 应为 token pubkey");
        assert_eq!(r.body["owner_kind"], "pubkey");
        assert_eq!(r.body["publisher_display"], display);
        // DB 落库的 publisher 即 pubkey（owner_kind 可由 publisher 解析复核）
        assert!(entry_owner_is_pubkey(&h.entries_snapshot()[0].publisher));
    }

    /// C6. 重发布/下架权限：owner_kind=pubkey 条目仅同 pubkey 或 admin 可改；
    ///     他人 token 403；admin 覆盖放行。
    #[tokio::test]
    async fn chain_republish_unpublish_owner_gating() {
        let dir = tempdir();
        make_bare_repo(&dir, "gated", "", "# Gated");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (owner_pk, owner_token) = login(&h, &new_key()).await;
        let (_, other_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            PATH_PUBLISH,
            &owner_token,
            serde_json::json!({"repo": "gated"}),
        ))
        .await
        .unwrap();
        // 他人 token 重发布 → 403（统一文案契约）
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &other_token,
                serde_json::json!({"repo": "gated", "description": "hijack"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "他人重发布应 403: {r:?}");
        assert_eq!(r.body["error"], "仅项目所有者可操作");
        // 他人 token 下架 → 403
        let r = h
            .handle(delete_req_auth("/api/v1/nexhub/lobby/gated", &other_token))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "他人下架应 403: {r:?}");
        assert_eq!(r.body["error"], "仅项目所有者可操作");
        // 本人重发布 → 201（刷新快照）
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &owner_token,
                serde_json::json!({"repo": "gated", "description": "refreshed"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "本人重发布应放行: {r:?}");
        assert_eq!(r.body["description"], "refreshed");
        // admin 覆盖他人 pubkey 条目 → 201（平台管理；归因变更为字符串 owner）
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "gated", "publisher": "ops"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "admin 覆盖应放行: {r:?}");
        assert_eq!(r.body["publisher"], "ops");
        assert_eq!(r.body["owner_kind"], "admin");
        // 原 owner（pubkey）对被 admin 托管化的字符串条目再改 → 403（仅 admin）
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &owner_token,
                serde_json::json!({"repo": "gated"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "字符串条目对链上身份应 403: {r:?}");
        let _ = owner_pk;
    }

    /// C7. 存量字符串条目（NexOS/zcode，平台托管）：pubkey token 下架 → 403；
    ///     admin 下架 → 200。
    #[tokio::test]
    async fn chain_legacy_string_entry_admin_only() {
        let h = authed_empty();
        insert_raw(
            &h,
            LobbyEntry {
                publisher: "NexOS".to_string(),
                ..entry(
                    "nexos",
                    "平台托管条目",
                    &["official"],
                    0,
                    "2026-08-01T10:00:00+08:00",
                )
            },
        );
        let (_, token) = login(&h, &new_key()).await;
        let r = h
            .handle(delete_req_auth("/api/v1/nexhub/lobby/nexos", &token))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "存量字符串条目对链上身份应 403: {r:?}");
        assert_eq!(r.body["error"], "仅项目所有者可操作");
        let r = h
            .handle(admin_delete("/api/v1/nexhub/lobby/nexos"))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 下架平台托管条目应放行: {r:?}");
        assert!(h.entries_snapshot().is_empty());
    }

    /// C8. 无 token / 伪 token 写操作 → 401（回落 admin 判定前的身份闸门）。
    #[tokio::test]
    async fn chain_missing_identity_writes_return_401() {
        let h = authed_empty();
        for (desc, req) in [
            (
                "publish 无 token",
                post_req(PATH_PUBLISH, serde_json::json!({"repo": "x"})),
            ),
            (
                "bounty create 无 token",
                post_req(
                    PATH_BOUNTY_CREATE,
                    serde_json::json!({"title": "x", "reward_sats": 100}),
                ),
            ),
            (
                "claim 无 token",
                post_req("/api/v1/nexhub/bounty/bty1/claim", serde_json::json!({})),
            ),
            ("entitlements 无 token", get_req(PATH_ENTITLEMENTS)),
            ("unpublish 无 token", delete_req("/api/v1/nexhub/lobby/x")),
        ] {
            let r = h.handle(req).await.unwrap();
            assert_eq!(r.status, 401, "无 token 的 {desc} 应 401");
        }
        // 伪 token（不在任何桶中）同样 401
        let r = h
            .handle(post_req_auth(
                PATH_BOUNTY_CREATE,
                &"0".repeat(64),
                serde_json::json!({"title": "x", "reward_sats": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 401, "伪 token 应 401");
    }

    /// C9. bounty poster 身份锁定：create 的 poster=token pubkey（body 自报忽略）；
    ///     approve/reject/cancel 仅 poster（或 admin），越权 403。
    #[tokio::test]
    async fn chain_bounty_poster_locked_to_token() {
        let h = authed_empty();
        let (poster_pk, poster_token) = login(&h, &new_key()).await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        // poster 用链上身份发布（body 自报 "victim" 应被忽略）
        let r = h
            .handle(post_req_auth(
                PATH_BOUNTY_CREATE,
                &poster_token,
                serde_json::json!({"title": "T", "reward_sats": 100, "poster": "victim"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        let id = r.body["id"].as_str().unwrap().to_string();
        assert_eq!(r.body["poster"], poster_pk, "poster 应为 token pubkey");
        // hunter 认领 + 提交
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/claim"),
            &hunter_token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "https://s"}),
        ))
        .await
        .unwrap();
        // 第三个身份（非 poster 非 admin）验收 → 403
        let (_, stranger_token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                &stranger_token,
                serde_json::json!({"txid": "tx", "amount_sats": 100, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "非 poster 验收应 403: {r:?}");
        assert_eq!(r.body["error"], "仅悬赏发布者（poster）可操作");
        // poster 本人验收 → 200
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                &poster_token,
                serde_json::json!({"txid": "tx", "amount_sats": 100, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "poster 验收应放行: {r:?}");
        // reject/cancel 同样锁 poster：新悬赏，stranger reject/cancel → 403
        let r = h
            .handle(post_req_auth(
                PATH_BOUNTY_CREATE,
                &poster_token,
                serde_json::json!({"title": "T2", "reward_sats": 100}),
            ))
            .await
            .unwrap();
        let id2 = r.body["id"].as_str().unwrap().to_string();
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id2}/cancel"),
                &stranger_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "非 poster 取消应 403: {r:?}");
        // hunter（也非 poster）驳回路径同样 403（先提交再驳回复核）
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id2}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "u"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id2}/reject"),
                &hunter_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "非 poster 驳回应 403: {r:?}");
    }

    /// C10. bounty hunter 身份锁定：claim 的 hunter=token pubkey；submit 仅
    ///      claim 的 hunter 本人，他人 403。
    #[tokio::test]
    async fn chain_bounty_hunter_locked_to_claim() {
        let h = authed_empty();
        let id = create_bounty(&h, 100, "btc").await;
        let (hunter_pk, hunter_token) = login(&h, &new_key()).await;
        let (_, other_token) = login(&h, &new_key()).await;
        // hunter 认领（body 自报忽略）
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/claim"),
                &hunter_token,
                serde_json::json!({"hunter": "forged-attacker"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["claimed_by"], hunter_pk, "hunter 应为 token pubkey");
        // 他人提交 → 403（该悬赏已由他人认领）
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/submit"),
                &other_token,
                serde_json::json!({"solution_url": "https://steal"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "非认领者提交应 403: {r:?}");
        assert_eq!(r.body["error"], "该悬赏已由他人认领");
        // 本人提交 → 200
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/submit"),
                &hunter_token,
                serde_json::json!({"solution_url": "https://real"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "认领者本人提交应放行: {r:?}");
        assert_eq!(r.body["claimed_by"], hunter_pk);
    }

    /// C11. purchase：admin 无 token 时代记 buyer="admin"；链上身份 buyer=pubkey
    ///      （body 自报一律忽略）。
    #[tokio::test]
    async fn chain_purchase_buyer_attribution() {
        let dir = tempdir();
        make_bare_repo(&dir, "paid", "", "# Paid");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "paid", "price_sats": 100, "currency": "btc"}),
        ))
        .await
        .unwrap();
        // admin 代记 buyer="admin"
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/paid/purchase",
                serde_json::json!({"buyer": "whoever", "txid": "tx_a", "amount_sats": 100, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body["buyer"], "admin", "admin 代记 buyer=admin");
        // 链上身份 → buyer=pubkey
        let (pk, token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/paid/purchase",
                &token,
                serde_json::json!({"buyer": "forged", "txid": "tx_b", "amount_sats": 100, "currency": "btc"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body["buyer"], pk, "buyer 应为 token pubkey");
        // 授权记录按身份 buyer 归档
        let list = h
            .handle(admin_get("/api/v1/nexhub/lobby/entitlements?repo=paid"))
            .await
            .unwrap();
        let buyers: Vec<&str> = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["buyer"].as_str().unwrap())
            .collect();
        assert!(buyers.contains(&"admin"));
        assert!(buyers.contains(&pk.as_str()));
    }

    /// C12. admin 回落判定链：链上 token 无效但等于系统 admin token → admin 身份
    ///      （构造期注入）；有效期语义由 C6/C7/C11 覆盖，此处复核 env 读取路径
    ///      （with_admin_token 即等价注入，env 路径在 main.rs 装配测试）。
    #[tokio::test]
    async fn chain_admin_fallback_allows_legacy_writes() {
        let h = authed_empty();
        // admin 建悬赏（poster=body 字符串）
        let r = h
            .handle(admin_post(
                PATH_BOUNTY_CREATE,
                serde_json::json!({"title": "T", "reward_sats": 100, "poster": "zcode"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["poster"], "zcode");
        let id = r.body["id"].as_str().unwrap().to_string();
        // 链上身份对存量字符串 poster 的悬赏 approve → 403；admin → 通过状态机校验
        let (_, token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/bounty/{id}/cancel"),
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "存量 poster 对链上身份应 403: {r:?}");
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/cancel"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 取消存量悬赏应放行: {r:?}");
    }

    // ---- 联邦大厅（P3，docs/NEXHUB_LOBBY_DESIGN.md §14）----

    /// 捕获型联邦传输（测试 mock：记录全部广播载荷）。
    struct CapturedTransport(std::sync::Mutex<Vec<serde_json::Value>>);
    impl LobbyFedTransport for CapturedTransport {
        fn broadcast(&self, payload: serde_json::Value) {
            self.0.lock().unwrap().push(payload);
        }
    }

    /// 联邦测试 fixture：内存库 handler + 已注入捕获通道。
    fn federated(node: &str) -> (NexHubLobbyRouteHandler, Arc<CapturedTransport>) {
        let h = authed_empty();
        let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
        h.fed_endpoint().set_transport(t.clone(), node.to_string());
        (h, t)
    }

    // 21. 两步联邦：发布只写本地（不广播、federated=false）→ federate 端点推送
    //     → 广播载荷 {fed, node, entry} 且字段完整（pubkey owner 本人推送）
    #[tokio::test]
    async fn fed_publish_local_then_federate_broadcasts_payload() {
        let dir = tempdir();
        make_bare_repo(&dir, "fed-repo", "联邦测试仓", "# Fed");
        let (h, t) = {
            let h =
                NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
            let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
            h.fed_endpoint().set_transport(t.clone(), "node-106".into());
            (h, t)
        };
        let (pubkey, token) = login(&h, &new_key()).await;
        // 第一步：发布 → 仅本地（两步联邦，发布不广播）
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &token,
                serde_json::json!({"repo": "fed-repo", "tags": ["fed"]}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["federated"], false, "发布恒未推送（两步联邦第一步）");
        assert!(
            t.0.lock().unwrap().is_empty(),
            "发布不广播——联邦只能从本地已发布条目推送"
        );
        // 第二步：owner 本人推送 → 广播一次
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/fed-repo/federate",
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "推送应 200: {r:?}");
        assert_eq!(r.body["ok"], true);
        assert_eq!(r.body["federated"], true);
        assert_eq!(r.body["first_push"], true);
        {
            let payloads = t.0.lock().unwrap();
            assert_eq!(payloads.len(), 1, "推送应广播一次: {payloads:?}");
            let p = &payloads[0];
            assert_eq!(p["fed"], FED_KIND_NEXHUB_LOBBY);
            assert_eq!(p["node"], "node-106");
            assert_eq!(p["entry"]["repo_name"], "fed-repo");
            assert_eq!(p["entry"]["publisher"], pubkey);
            assert_eq!(p["entry"]["source_node"], "local", "发送端条目恒 local");
            assert_eq!(p["entry"]["federated"], true, "载荷携带推送标志");
            assert!(p["entry"]["commit_count"].as_u64().unwrap_or(0) >= 2);
            assert!(p["entry"]["readme_excerpt"]
                .as_str()
                .unwrap()
                .contains("Fed"));
        } // 锁作用域结束，不跨下方 await（clippy::await_holding_lock）
          // 标志落库：DB 快照 + HTTP 列表（前端 🌐 标记依据）
        assert!(h.entries_snapshot()[0].federated);
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(list.body[0]["federated"], true);
    }

    // 22. 两步联邦第一步回归：admin 字符串条目发布同样只写本地（不广播）——
    //     联邦推送是显式第二步（/:name/federate），与发布身份无关
    #[tokio::test]
    async fn fed_publish_admin_entry_not_broadcast() {
        let dir = tempdir();
        make_bare_repo(&dir, "admin-repo", "", "# A");
        let (h, t) = {
            let h =
                NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
            let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
            h.fed_endpoint().set_transport(t.clone(), "node-a".into());
            (h, t)
        };
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "admin-repo", "publisher": "local"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body["owner_kind"], "admin");
        assert_eq!(r.body["federated"], false, "发布恒未推送");
        assert!(
            t.0.lock().unwrap().is_empty(),
            "admin 发布不广播（推送走 /:name/federate）"
        );
    }

    // 23. P2P 未装配（无 transport）：发布与联邦推送均静默成功（不 panic 不阻塞）；
    //     推送侧 federated 标志仍置位（发布侧决策），单机部署零开销
    #[tokio::test]
    async fn fed_without_transport_silently_skips() {
        let dir = tempdir();
        make_bare_repo(&dir, "lonely-repo", "", "# L");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        assert!(!h.fed_endpoint().is_federated(), "未注入通道");
        let (_, token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &token,
                serde_json::json!({"repo": "lonely-repo"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "无 P2P 时发布照常 201");
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/lonely-repo/federate",
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status, 200,
            "无 P2P 时推送照常 200（广播静默跳过）: {r:?}"
        );
        assert_eq!(r.body["federated"], true);
        assert!(h.entries_snapshot()[0].federated, "标志仍置位");
    }

    // 24. 联邦接收：合法载荷 → 写入本地 + source_node 标记来源 + 本地计数清零
    #[test]
    fn fed_ingest_writes_entry_with_source_node() {
        let (h, _t) = federated("node-b");
        let remote = entry(
            "remote-proj",
            "远程项目",
            &["rust"],
            42,
            "2026-08-22T10:00:00+08:00",
        );
        let payload = build_nexhub_lobby_fed_payload("node-106", &remote);
        assert_eq!(h.fed_endpoint().ingest(&payload), LobbyFedIngest::Written);
        let saved = h
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "remote-proj")
            .expect("应写入本地");
        assert_eq!(saved.source_node, "node-106", "来源节点标记");
        assert_eq!(saved.description, "远程项目");
        assert_eq!(saved.publisher, "tester");
        assert_eq!(saved.download_count, 0, "远程计数不带入（本地活跃度独立）");
        assert_eq!(saved.commit_count, 3, "快照字段完整");
    }

    // 24a. 联邦载荷往返携带 clone_url_http（2026-08-25 跨节点拉取修复）：
    //      发布定格源节点 HTTP 地址 → 载荷原样携带 → 消费端落库保留——
    //      一键克隆据此从源节点拉取；旧 payload（无字段）解析为空串不炸。
    #[test]
    fn fed_payload_round_trips_clone_url_http() {
        let (h, _t) = federated("node-b");
        // 新条目：带可达 IP 的 clone_url_http（784547f 地址链产物）
        let remote = LobbyEntry {
            source_node: "local".to_string(), // 发送端恒 local（ingest 改写）
            clone_url_http: "http://192.0.2.106:8558/git/nexos.git".to_string(),
            ..entry("fed-url", "带地址", &[], 0, "2026-08-25T10:00:00+08:00")
        };
        let payload = build_nexhub_lobby_fed_payload("node-106", &remote);
        assert_eq!(
            payload["entry"]["clone_url_http"], "http://192.0.2.106:8558/git/nexos.git",
            "载荷应携带发布节点定格的 HTTP 克隆地址"
        );
        assert_eq!(h.fed_endpoint().ingest(&payload), LobbyFedIngest::Written);
        let saved = h
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "fed-url")
            .expect("应写入本地");
        assert_eq!(
            saved.clone_url_http, "http://192.0.2.106:8558/git/nexos.git",
            "消费端落库保留源节点地址（一键克隆拉取源）"
        );
        assert_eq!(saved.source_node, "node-106");
        // 旧 payload（字段加入前发布）：无 clone_url_http 键 → 空串（serde
        // default），克隆侧走「需重 publish」引导（13d）
        let legacy = serde_json::json!({
            "fed": FED_KIND_NEXHUB_LOBBY,
            "node": "node-106",
            "entry": {
                "repo_name": "fed-legacy",
                "description": "旧条目",
                "tags": [],
                "publisher": "tester",
                "source_url": "/tank/git-repos/fed-legacy.git",
                "source_node": "local",
                "commit_count": 1,
                "size_bytes": 8,
                "default_branch": "main",
                "readme_excerpt": "# l",
                "download_count": 0,
                "published_at": "2026-08-20T10:00:00+08:00",
                "price_sats": 0,
                "currency": "free",
                "federated": true,
            }
        });
        assert_eq!(h.fed_endpoint().ingest(&legacy), LobbyFedIngest::Written);
        let legacy_saved = h
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "fed-legacy")
            .expect("旧 payload 应可解析写入");
        assert_eq!(legacy_saved.clone_url_http, "", "旧 payload 无地址 → 空串");
    }

    // 25. 联邦接收去重：同 repo+node 二次收不重写（缓存命中 Duplicate）
    #[test]
    fn fed_ingest_dedups_same_name_and_node() {
        let (h, _t) = federated("node-b");
        let remote = entry("dup-proj", "v1", &[], 0, "2026-08-22T10:00:00+08:00");
        let payload = build_nexhub_lobby_fed_payload("node-106", &remote);
        assert_eq!(h.fed_endpoint().ingest(&payload), LobbyFedIngest::Written);
        assert_eq!(h.fed_endpoint().ingest(&payload), LobbyFedIngest::Duplicate);
        // 同名不同节点：DB 有条目且来源不同 → Skipped（本地/首到条目受保护）
        let other = build_nexhub_lobby_fed_payload("node-777", &remote);
        assert_eq!(h.fed_endpoint().ingest(&other), LobbyFedIngest::Skipped);
        assert_eq!(h.entries_snapshot().len(), 1);
    }

    // 26. 联邦接收：本地条目不受远程同名条目影响（Skipped 保护）
    #[test]
    fn fed_ingest_protects_local_entry() {
        let (h, _t) = federated("node-b");
        insert_raw(
            &h,
            entry(
                "nexos",
                "本地主仓库",
                &["official"],
                7,
                "2026-08-01T08:00:00+08:00",
            ),
        );
        let remote = entry(
            "nexos",
            "远程伪造描述",
            &[],
            99,
            "2026-08-22T11:00:00+08:00",
        );
        let payload = build_nexhub_lobby_fed_payload("node-evil", &remote);
        assert_eq!(h.fed_endpoint().ingest(&payload), LobbyFedIngest::Skipped);
        let saved = h
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "nexos")
            .unwrap();
        assert_eq!(saved.description, "本地主仓库", "本地条目不被覆盖");
        assert_eq!(saved.source_node, "local");
    }

    // 27. 联邦接收：同源重发（对端刷新快照）→ Refreshed 且保留本地 download_count
    //     （2026-08-23 修复回归：同端点活路径——旧实现缓存键只有 repo+node，
    //      首收后同源刷新在缓存存续期内一律 Duplicate，只有重启/换端点才能
    //      触发 Refreshed；现键含 published_at，新快照穿透缓存直达 DB 判定）
    #[test]
    fn fed_ingest_same_origin_refreshes_preserving_count() {
        let (h, _t) = federated("node-b");
        let fed = h.fed_endpoint();
        let v1 = entry("hot-proj", "v1 描述", &[], 0, "2026-08-20T10:00:00+08:00");
        let payload = build_nexhub_lobby_fed_payload("node-106", &v1);
        assert_eq!(fed.ingest(&payload), LobbyFedIngest::Written);
        // 本地克隆过两次（模拟）
        {
            let conn = h.db.lock().expect("db poisoned");
            bump_download(&conn, "hot-proj").unwrap();
            bump_download(&conn, "hot-proj").unwrap();
        }
        // 同源新快照（发布侧重新 publish → published_at 变化）：同一端点实例
        // （不重启、不换端点）即应 Refreshed——修复前这里返回 Duplicate。
        let v2 = entry("hot-proj", "v2 刷新", &[], 0, "2026-08-22T12:00:00+08:00");
        let p2 = build_nexhub_lobby_fed_payload("node-106", &v2);
        assert_eq!(fed.ingest(&p2), LobbyFedIngest::Refreshed);
        // 逐字节相同的重放仍被缓存拦住（Duplicate，不触碰 DB）
        assert_eq!(fed.ingest(&p2), LobbyFedIngest::Duplicate);
        let saved = h
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "hot-proj")
            .unwrap();
        assert_eq!(saved.description, "v2 刷新", "快照已刷新");
        assert_eq!(saved.download_count, 2, "本地克隆计数保留");
    }

    // 27a. 联邦刷新语义（自动同步链关键测试，2026-08-25 §15）：发布侧**两次
    //      publish 同 name**（钩子链的 v1 旧快照 → v2 新快照：latest_commit/
    //      pushed_at/commit_count 均推进）先后广播，消费端 ingest 后——
    //      条目数恒 1（按 name 幂等合并，不是新增重复条目）且字段为**最新**快照。
    #[test]
    fn fed_consumer_merges_snapshot_updates_by_name() {
        let (h, _t) = federated("node-b");
        let fed = h.fed_endpoint();
        // v1 旧快照（发布侧第一次 publish 广播）
        let v1 = entry(
            "nexos",
            "v1 旧描述",
            &["nexos"],
            0,
            "2026-08-20T10:00:00+08:00",
        );
        let mut v1 = v1;
        v1.commit_count = 100;
        v1.latest_commit = Some(LatestCommit {
            short_hash: "aaa0001".into(),
            subject: "旧提交".into(),
            author: "dev-a".into(),
            date: "2026-08-20 10:00:00 +0800".into(),
        });
        v1.pushed_at = "2026-08-20T10:00:05+08:00".into();
        let p1 = build_nexhub_lobby_fed_payload("node-106", &v1);
        assert_eq!(fed.ingest(&p1), LobbyFedIngest::Written);
        // v2 新快照（对端 git push → 钩子触发重 publish → 重广播：同 name、
        // published_at/pushed_at 均变 → 穿透缓存走 DB 权威合并）
        let mut v2 = entry(
            "nexos",
            "v2 新描述",
            &["nexos"],
            0,
            "2026-08-25T12:00:00+08:00",
        );
        v2.commit_count = 101;
        v2.latest_commit = Some(LatestCommit {
            short_hash: "bbb0002".into(),
            subject: "新提交：自动同步".into(),
            author: "dev-106".into(),
            date: "2026-08-25 12:00:00 +0800".into(),
        });
        v2.pushed_at = "2026-08-25T12:00:05+08:00".into();
        let p2 = build_nexhub_lobby_fed_payload("node-106", &v2);
        assert_eq!(
            fed.ingest(&p2),
            LobbyFedIngest::Refreshed,
            "同源新快照应刷新"
        );
        // 消费端：条目 1 条（不重复），字段为最新快照
        let entries = h.entries_snapshot();
        assert_eq!(
            entries.len(),
            1,
            "两次广播同 name → 条目仍 1 条: {entries:?}"
        );
        let e = &entries[0];
        assert_eq!(e.repo_name, "nexos");
        assert_eq!(e.source_node, "node-106");
        assert_eq!(e.description, "v2 新描述", "描述=最新快照");
        assert_eq!(e.commit_count, 101, "commit 数=最新快照");
        let lc = e.latest_commit.as_ref().expect("latest_commit=最新快照");
        assert_eq!(lc.short_hash, "bbb0002");
        assert_eq!(lc.subject, "新提交：自动同步");
        assert_eq!(lc.author, "dev-106");
        assert_eq!(
            e.pushed_at, "2026-08-25T12:00:05+08:00",
            "pushed_at=最新快照"
        );
        // HTTP 列表同（前端联邦大厅视图看到的即最新状态——自举依赖）
        // （列表接口在非 async 测试下不可用，DB 快照已覆盖同一路径）
    }

    // ---- nexos 本地 bare 副本自动跟随（2026-08-27，同步链最后一环）----
    //
    // 测试纪律：
    // - 全部用**临时 bare 源 + 真实 git**（init/commit/push/fetch 实跑），
    //   跨节点 HTTP 用 file:// URL 等价模拟 transport 语义（单测无网可依赖）；
    // - 跟随拉取是**后台任务**：断言一律走 `wait_for` 轮询（50ms 步进 +
    //   截止上限），不做无界 sleep-only 断言；
    // - 「不该发生」类断言（节流/env 关闭/非 nexos）用短暂观察窗 +
    //   节流登记表双向验证。

    /// 串行化 NEXOS_LOBBY_AUTO_PULL 环境变量敏感的跟随用例（并行 set/remove
    /// 互相污染——与 code_repo 的 ENV_LOCK 同款纪律；依赖默认开启态的用例
    /// 也持锁，防关闭态写入交错）。
    static AUTO_PULL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 源 bare 追加一个提交（clone 工作区 → 写文件 → commit → push 回），
    /// 返回新提交完整 hash。
    fn push_commit_to_bare(bare: &str, branch: &str, msg: &str) -> String {
        let work = std::env::temp_dir().join(format!(
            "os-nexhub-follow-work-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&work).unwrap();
        let w = work.to_str().unwrap();
        assert!(run(&["git", "clone", bare, w]).0, "clone 源仓失败");
        std::fs::write(work.join("follow.txt"), msg).unwrap();
        assert!(run(&["git", "-C", w, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                w,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                msg
            ])
            .0,
            "commit 失败"
        );
        assert!(
            run(&["git", "-C", w, "push", "origin", &format!("HEAD:{branch}")]).0,
            "push 失败"
        );
        let (_, full) = run(&[
            "git",
            "--git-dir",
            bare,
            "rev-parse",
            &format!("refs/heads/{branch}"),
        ]);
        let _ = std::fs::remove_dir_all(&work);
        full.trim().to_string()
    }

    /// bare 仓 HEAD 完整 hash（None = 空仓/目录不存在）。
    fn bare_head_full(bare: &str) -> Option<String> {
        let (ok, out) = run(&["git", "--git-dir", bare, "rev-parse", "HEAD"]);

        if ok {
            Some(out.trim().to_string())
        } else {
            None
        }
    }

    /// bare 仓指定 tag 指向的对象完整 hash（None = tag 不存在）——auto-pull
    /// fetch 后 tag 存在性/指向断言用（轻量 tag，hash 即目标提交）。
    fn bare_tag_hash(bare: &str, tag: &str) -> Option<String> {
        let (ok, out) = run(&[
            "git",
            "--git-dir",
            bare,
            "rev-parse",
            &format!("refs/tags/{tag}"),
        ]);
        if ok {
            Some(out.trim().to_string())
        } else {
            None
        }
    }

    /// 轮询等待 cond 成立（deadline 毫秒）；返回是否按时达成（「不该发生」
    /// 类断言取反消费——观察到满窗才算守住）。
    fn wait_for(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= std::time::Duration::from_millis(deadline_ms) {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 远程快照条目（发布侧重 publish 后广播的形态）：携带结构化 latest_commit
    /// 与拉取源（src_url 本机路径 / http_url 联邦地址，按用例任选其一或皆空）。
    fn fed_snapshot(
        name: &str,
        at: &str,
        src_url: &str,
        http_url: &str,
        commit_full: &str,
        subject: &str,
    ) -> LobbyEntry {
        let mut e = entry(name, name, &[], 0, at);
        e.source_url = src_url.to_string();
        e.clone_url_http = http_url.to_string();
        e.latest_commit = Some(LatestCommit {
            short_hash: commit_full.chars().take(7).collect(),
            subject: subject.to_string(),
            author: "dev-106".into(),
            date: "2026-08-27 12:00:00 +0800".into(),
        });
        e.pushed_at = format!("{at}+08:00");
        e
    }

    /// 仅带 latest_commit 快照的直接调用型条目（[tokio::test] 直调
    /// run_auto_pull_inner 用，不经 DB）。
    fn snap_entry_with_commit(commit_full: &str) -> LobbyEntry {
        let mut e = entry("nexos", "n", &[], 0, "2026-08-27T12:00:00+08:00");
        e.latest_commit = Some(LatestCommit {
            short_hash: commit_full.chars().take(7).collect(),
            subject: "s".into(),
            author: "a".into(),
            date: "d".into(),
        });
        e
    }

    /// 副本路径 → 仓库根目录（run_auto_pull_inner 的第一参数形态）。
    fn repos_root_of(copy: &str) -> String {
        std::path::Path::new(copy)
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// 把构造期常驻的本地种子（source_node=local）改写为「来自 node-106 的
    /// 远程条目」——消费节点部署形态（NEXOS_LOBBY_NO_AUTO_PUBLISH=1 时无本地
    /// 种子，联邦快照权威），否则远程同名 ingest 被 Skipped 保护挡住。
    fn seed_remote_row(h: &NexHubLobbyRouteHandler, repo_name: &str) {
        let mut row = entry(repo_name, "旧快照", &[], 0, "2026-08-20T10:00:00+08:00");
        row.source_node = "node-106".to_string();
        insert_raw(h, row);
    }

    /// 双仓 rig：源 bare（make_bare_repo 形态，HEAD→<branch> 两提交）+ 消费端
    /// `<name>.git` 副本路径（可选预置一份过期副本）。返回 (源路径, 副本路径)。
    fn follow_rig(name: &str, branch: &str, clone_stale_copy: bool) -> (String, String) {
        let root = tempdir();
        let upstream = make_bare_repo_at_head(&root, name, branch, branch, "# 跟随 rig\n");
        let repos_root = format!("{root}/hub-repos");
        std::fs::create_dir_all(&repos_root).unwrap();
        let copy = format!("{repos_root}/{name}.git");
        if clone_stale_copy {
            assert!(
                run(&["git", "clone", "--bare", &upstream, &copy]).0,
                "预置过期副本失败"
            );
        }
        (upstream, copy)
    }

    // 29a. 跟随后台任务实质逻辑直调：既有副本 fetch --prune 推进分支引用
    #[tokio::test]
    async fn auto_pull_inner_fetches_existing_copy_forward() {
        let (upstream, copy) = follow_rig("nexos", "main", true);
        let old = bare_head_full(&copy).expect("副本应有初始 HEAD");
        let new_full = push_commit_to_bare(&upstream, "main", "follow: fetch 目标提交");
        assert_ne!(new_full, old);
        let e = snap_entry_with_commit(&new_full);
        let src = AutoPullSource {
            url: upstream.clone(),
            timeout_secs: CLONE_TIMEOUT_SECS,
        };
        let outcome = run_auto_pull_inner(&repos_root_of(&copy), &e, &src).await;
        assert_eq!(outcome, Ok(AutoPullOutcome::Fetched));
        assert_eq!(
            bare_head_full(&copy),
            Some(new_full),
            "fetch 后分支引用（HEAD 所指）应推进"
        );
    }

    // 29a-tag. fetch 必须带 tag：发版即 tag（NexHub release 打 v* tag），下游
    // 副本的 refs/tags 是本节点更新检查（for-each-ref/ls-remote 读 tag）的
    // 版本源——旧 heads-only refspec 下 tag 只能靠 git 机会主义 auto-follow
    // （不保证覆盖旧对象上的 tag、绝不更新已存在/被强推的 tag），实测下游
    // 副本 refs/tags 全空（2026-09-03 真机踩坑）。断言三段：
    //   ① 既有对象上补打的 tag（auto-follow 最易漏的形态）随 fetch 到副本；
    //   ② 新提交上的新 tag 到副本且指向与源一致；
    //   ③ 源侧 `-f` 强挪 tag（release.sh `tag -fa` + `push -f` 形态）后，
    //     下轮 fetch 把副本 tag 强制对齐（auto-follow 语义下必失败——它
    //     从不更新已存在的 tag，只有显式 `+refs/tags/*` 强制 refspec 能对齐）。
    #[tokio::test]
    async fn auto_pull_inner_fetches_tags_to_copy() {
        let (upstream, copy) = follow_rig("nexos", "main", true);
        // 发版形态：在既有提交上补打 tag + 推进新提交并打新 tag。
        let first_head = bare_head_full(&upstream).unwrap();
        assert!(run(&["git", "--git-dir", &upstream, "tag", "v0.1.0", &first_head]).0);
        let new_full = push_commit_to_bare(&upstream, "main", "follow: 带 tag 的发版提交");
        assert!(run(&["git", "--git-dir", &upstream, "tag", "v0.2.0", &new_full]).0);
        let e = snap_entry_with_commit(&new_full);
        let src = AutoPullSource {
            url: upstream.clone(),
            timeout_secs: CLONE_TIMEOUT_SECS,
        };
        let outcome = run_auto_pull_inner(&repos_root_of(&copy), &e, &src).await;
        assert_eq!(outcome, Ok(AutoPullOutcome::Fetched));
        assert_eq!(bare_head_full(&copy), Some(new_full.clone()));
        // ①② 副本 tag 存在且指向与源一致（clone 早于打 tag → 副本初始无 tag，
        //    全部依赖 fetch 的显式 tag refspec 到位）。
        assert_eq!(
            bare_tag_hash(&copy, "v0.1.0"),
            Some(first_head.clone()),
            "旧对象上补打的 tag 应随 fetch 到副本"
        );
        assert_eq!(
            bare_tag_hash(&copy, "v0.2.0"),
            Some(new_full.clone()),
            "新提交上的新 tag 应随 fetch 到副本且指向一致"
        );
        // ③ 强推 tag：源把 v0.1.0 -f 挪到新提交，下轮 fetch 强制对齐。
        assert!(
            run(&[
                "git",
                "--git-dir",
                &upstream,
                "tag",
                "-f",
                "v0.1.0",
                &new_full
            ])
            .0
        );
        let c4 = push_commit_to_bare(&upstream, "main", "follow: tag 强推后的再推进");
        let e2 = snap_entry_with_commit(&c4);
        let outcome2 = run_auto_pull_inner(&repos_root_of(&copy), &e2, &src).await;
        assert_eq!(outcome2, Ok(AutoPullOutcome::Fetched));
        assert_eq!(
            bare_tag_hash(&copy, "v0.1.0"),
            Some(new_full),
            "被强推的 tag 应被 +refs/tags/* 强制 refspec 对齐（auto-follow 不更新既有 tag）"
        );
    }

    // 29b. 既有副本 + 本地 HEAD 已等于快照 short_hash → HeadMatchSkipped 省流
    //      （先推新提交制造「远端实况更新」假象，判等只认快照声明值）
    #[tokio::test]
    async fn auto_pull_inner_skips_when_head_matches_snapshot() {
        let (upstream, copy) = follow_rig("nexos", "main", true);
        let head = bare_head_full(&copy).unwrap();
        let new_full = push_commit_to_bare(&upstream, "main", "远端已走但快照未声明");
        assert_ne!(new_full, head);
        let e = snap_entry_with_commit(&head); // 快照声称 = 本地现值
        let src = AutoPullSource {
            url: upstream,
            timeout_secs: CLONE_TIMEOUT_SECS,
        };
        let outcome = run_auto_pull_inner(&repos_root_of(&copy), &e, &src).await;
        assert_eq!(
            outcome,
            Ok(AutoPullOutcome::HeadMatchSkipped),
            "HEAD 判等命中应跳过 fetch（省流量）"
        );
    }

    // 29c. 无副本（首次收件）→ 完整 clone 落地并对齐源 HEAD
    #[tokio::test]
    async fn auto_pull_inner_clones_missing_copy() {
        let (upstream, copy) = follow_rig("nexos", "main", false);
        assert!(!std::path::Path::new(&copy).exists(), "前置：无副本");
        let head = bare_head_full(&upstream).unwrap();
        let e = snap_entry_with_commit(&head);
        let src = AutoPullSource {
            url: upstream.clone(),
            timeout_secs: CLONE_TIMEOUT_SECS,
        };
        let outcome = run_auto_pull_inner(&repos_root_of(&copy), &e, &src).await;
        assert_eq!(outcome, Ok(AutoPullOutcome::Cloned));
        assert_eq!(bare_head_full(&copy), Some(head), "克隆即对齐源 HEAD");
    }

    // 30. e2e：同源 nexos 刷新广播 → ingest Refreshed → 后台拉取把本地副本
    //     HEAD 推进到新提交（用户从本节点 NexHub clone 到的即最新代码）
    #[test]
    fn auto_pull_federated_refresh_advances_local_bare_head() {
        let _env = AUTO_PULL_ENV_LOCK.lock().unwrap();
        let (upstream, copy) = follow_rig("nexos", "main", true);
        let old = bare_head_full(&copy).unwrap();

        let h = NexHubLobbyRouteHandler::with_repos_dir(&repos_root_of(&copy))
            .with_admin_token(TEST_ADMIN_TOKEN);
        let fed = h.fed_endpoint();
        seed_remote_row(&h, "nexos");

        // 上游推进新提交（真实 git push），发布侧重 publish 重广播
        let new_full = push_commit_to_bare(&upstream, "main", "follow: e2e 新提交");
        assert_ne!(new_full, old);
        let snap = fed_snapshot(
            "nexos",
            "2026-08-27T12:00:00",
            &upstream,
            "",
            &new_full,
            "follow: e2e 新提交",
        );
        let payload = build_nexhub_lobby_fed_payload("node-106", &snap);
        assert_eq!(fed.ingest(&payload), LobbyFedIngest::Refreshed);

        assert!(
            wait_for(15_000, || bare_head_full(&copy).as_deref()
                == Some(new_full.as_str())),
            "副本 HEAD 应自动推进到 {new_full}，实际 {:?}",
            bare_head_full(&copy)
        );
        assert!(
            fed.auto_pull_last.lock().unwrap().contains_key("nexos"),
            "触发过跟随应在节流表登记"
        );
    }

    // 31. e2e：副本缺失（首次收件）经 clone_url_http（file:// 等价跨节点传输）
    //     解析拉取源 → 完整 clone 落地
    #[test]
    fn auto_pull_clones_missing_copy_via_clone_url_http() {
        let _env = AUTO_PULL_ENV_LOCK.lock().unwrap();
        let (upstream, copy) = follow_rig("nexos", "main", false);
        let h = NexHubLobbyRouteHandler::with_repos_dir(&repos_root_of(&copy))
            .with_admin_token(TEST_ADMIN_TOKEN);
        let fed = h.fed_endpoint();
        seed_remote_row(&h, "nexos");

        let head = bare_head_full(&upstream).unwrap();
        // source_url 留空（跨节点形态：源节点本机路径在本机无意义），只带
        // clone_url_http —— 强制走联邦 HTTP 源解析分支
        let snap = fed_snapshot(
            "nexos",
            "2026-08-27T11:00:00",
            "",
            &format!("file://{upstream}"),
            &head,
            "初见即最新",
        );
        let payload = build_nexhub_lobby_fed_payload("node-106", &snap);
        assert_eq!(fed.ingest(&payload), LobbyFedIngest::Refreshed);

        assert!(
            wait_for(15_000, || bare_head_full(&copy).as_deref()
                == Some(head.as_str())),
            "应从 clone_url_http 克隆出副本并对齐 HEAD，实际 {:?}",
            bare_head_full(&copy)
        );
    }

    // 32. 非 nexos 仓库不自动跟随（只跟内置主仓 nexos——需求边界）
    #[test]
    fn auto_pull_skips_non_seed_repos() {
        let _env = AUTO_PULL_ENV_LOCK.lock().unwrap();
        let (upstream, other_copy) = follow_rig("tool-x", "main", true);
        let old = bare_head_full(&other_copy).unwrap();
        let h = NexHubLobbyRouteHandler::with_repos_dir(&repos_root_of(&other_copy))
            .with_admin_token(TEST_ADMIN_TOKEN);
        let fed = h.fed_endpoint();
        seed_remote_row(&h, "tool-x");

        let new_full = push_commit_to_bare(&upstream, "main", "不应被跟随之提交");
        let snap = fed_snapshot(
            "tool-x",
            "2026-08-27T12:30:00",
            &upstream,
            "",
            &new_full,
            "非主仓",
        );
        let payload = build_nexhub_lobby_fed_payload("node-106", &snap);
        assert_eq!(fed.ingest(&payload), LobbyFedIngest::Refreshed);

        // 观察窗内：既不登记节流槽，也不真的拉取副本
        assert!(
            !wait_for(1_500, || !fed.auto_pull_last.lock().unwrap().is_empty()),
            "非 nexos 不应占用任何节流槽"
        );
        assert_eq!(
            bare_head_full(&other_copy),
            Some(old),
            "非 nexos 副本必须原封不动"
        );
    }

    // 33. NEXOS_LOBBY_AUTO_PULL=0 关闭总开关：落库刷新语义不变，但不触发、
    //     不登记、不动本地副本
    #[test]
    fn auto_pull_disabled_by_env_zero() {
        let _env = AUTO_PULL_ENV_LOCK.lock().unwrap();
        std::env::set_var("NEXOS_LOBBY_AUTO_PULL", "0");
        let (upstream, copy) = follow_rig("nexos", "main", true);
        let old = bare_head_full(&copy).unwrap();
        let h = NexHubLobbyRouteHandler::with_repos_dir(&repos_root_of(&copy))
            .with_admin_token(TEST_ADMIN_TOKEN);
        let fed = h.fed_endpoint();
        seed_remote_row(&h, "nexos");

        let new_full = push_commit_to_bare(&upstream, "main", "开关关闭不应到达");
        let snap = fed_snapshot(
            "nexos",
            "2026-08-27T13:00:00",
            &upstream,
            "",
            &new_full,
            "关闭态快照",
        );
        let payload = build_nexhub_lobby_fed_payload("node-106", &snap);
        assert_eq!(fed.ingest(&payload), LobbyFedIngest::Refreshed);
        std::thread::sleep(std::time::Duration::from_millis(1_000));
        assert!(
            !fed.auto_pull_last.lock().unwrap().contains_key("nexos"),
            "关闭态不得登记跟随"
        );
        assert_eq!(bare_head_full(&copy), Some(old), "关闭态副本必须原地不动");
        std::env::remove_var("NEXOS_LOBBY_AUTO_PULL");
    }

    // 34. 节流防抖：10 分钟窗口内第二次快速刷新不再拉取（不追帧），窗口过后
    //     下个快照恢复同步；附占位判定时钟边界单测（注入人造 Instant）
    #[test]
    fn auto_pull_throttles_second_refresh_until_window_passes() {
        let _env = AUTO_PULL_ENV_LOCK.lock().unwrap();
        let (upstream, copy) = follow_rig("nexos", "main", true);
        let h = NexHubLobbyRouteHandler::with_repos_dir(&repos_root_of(&copy))
            .with_admin_token(TEST_ADMIN_TOKEN);
        let fed = h.fed_endpoint();
        seed_remote_row(&h, "nexos");

        // 第一次刷新：正常跟随（C3）
        let c3 = push_commit_to_bare(&upstream, "main", "follow: 第一波");
        let p1 = build_nexhub_lobby_fed_payload(
            "node-106",
            &fed_snapshot("nexos", "2026-08-27T14:00:00", &upstream, "", &c3, "第一波"),
        );
        assert_eq!(fed.ingest(&p1), LobbyFedIngest::Refreshed);
        assert!(
            wait_for(15_000, || bare_head_full(&copy).as_deref()
                == Some(c3.as_str())),
            "第一波应跟随到位"
        );

        // 第二次刷新紧随其后（C4 + 更晚 published_at）：10 分钟内不再拉取
        let c4 = push_commit_to_bare(&upstream, "main", "follow: 第二波（节流期内）");
        let p2 = build_nexhub_lobby_fed_payload(
            "node-106",
            &fed_snapshot("nexos", "2026-08-27T14:01:00", &upstream, "", &c4, "第二波"),
        );
        assert_eq!(fed.ingest(&p2), LobbyFedIngest::Refreshed);
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        assert_eq!(
            bare_head_full(&copy),
            Some(c3.clone()),
            "节流窗口内第二次刷新不得推进副本"
        );

        // 占位判定的时钟边界（注入人造 Instant，无需真等 10 分钟）
        let t0 = std::time::Instant::now();
        assert!(fed.try_acquire_auto_pull_slot("probe", t0), "首占应放行");
        assert!(
            !fed.try_acquire_auto_pull_slot("probe", t0 + std::time::Duration::from_secs(599)),
            "窗口内再占应拒绝"
        );
        assert!(
            fed.try_acquire_auto_pull_slot(
                "probe",
                t0 + std::time::Duration::from_secs(AUTO_PULL_THROTTLE.as_secs())
            ),
            "窗口期满应放行"
        );
        assert!(
            fed.try_acquire_auto_pull_slot("other-probe", t0),
            "不同仓库各自计时互不影响"
        );

        // 模拟窗口过期（登记时刻拨回 10+ 分钟前）→ 下个快照恢复同步到 C4
        fed.auto_pull_last.lock().unwrap().insert(
            "nexos".to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(700),
        );
        let p3 = build_nexhub_lobby_fed_payload(
            "node-106",
            &fed_snapshot("nexos", "2026-08-27T14:02:00", &upstream, "", &c4, "第三波"),
        );
        assert_eq!(fed.ingest(&p3), LobbyFedIngest::Refreshed);
        assert!(
            wait_for(15_000, || bare_head_full(&copy).as_deref()
                == Some(c4.as_str())),
            "窗口过后下个快照应把副本带到 {c4}"
        );
    }

    // 28. 联邦接收：非法载荷（非 nexhub_lobby / 缺 node / 非法名 / 坏 entry）→ Invalid
    #[test]
    fn fed_ingest_rejects_invalid_payloads() {
        let (h, _t) = federated("node-b");
        let e = entry("x", "", &[], 0, "2026-08-22T10:00:00+08:00");
        // 非 nexhub_lobby（IM 大厅消息等他类载荷）
        assert_eq!(
            h.fed_endpoint()
                .ingest(&serde_json::json!({"fed": "im_lobby", "node": "n", "message": {}})),
            LobbyFedIngest::Invalid
        );
        // 缺 node
        assert_eq!(
            h.fed_endpoint()
                .ingest(&serde_json::json!({"fed": FED_KIND_NEXHUB_LOBBY, "entry": e})),
            LobbyFedIngest::Invalid
        );
        // 缺 entry
        assert_eq!(
            h.fed_endpoint()
                .ingest(&serde_json::json!({"fed": FED_KIND_NEXHUB_LOBBY, "node": "n"})),
            LobbyFedIngest::Invalid
        );
        // 非法 repo_name（路径穿越防护）
        let bad = build_nexhub_lobby_fed_payload(
            "n",
            &entry("../evil", "", &[], 0, "2026-08-22T10:00:00+08:00"),
        );
        assert_eq!(h.fed_endpoint().ingest(&bad), LobbyFedIngest::Invalid);
        // entry 非对象
        assert_eq!(
            h.fed_endpoint().ingest(&serde_json::json!({"fed": FED_KIND_NEXHUB_LOBBY, "node": "n", "entry": "not-an-object"})),
            LobbyFedIngest::Invalid
        );
        assert!(h.entries_snapshot().is_empty(), "非法载荷一律零写入");
    }

    // 29. source_node 列迁移：旧 schema（16 列）库升级后自动补列且存量行回填 local
    #[test]
    fn source_node_column_migrates_legacy_db() {
        let dir = tempdir();
        let path = format!("{dir}/legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            // 旧 schema（P3 之前 16 列，无 source_node）
            conn.execute_batch(
                "CREATE TABLE hub_lobby (
                    repo_name TEXT PRIMARY KEY, description TEXT DEFAULT '', tags TEXT DEFAULT '[]',
                    publisher TEXT DEFAULT '', source_url TEXT DEFAULT '',
                    homepage_node TEXT DEFAULT 'local', commit_count INTEGER DEFAULT 0,
                    size_bytes INTEGER DEFAULT 0, default_branch TEXT DEFAULT 'master',
                    last_commit TEXT, last_commit_date TEXT, readme_excerpt TEXT DEFAULT '',
                    download_count INTEGER DEFAULT 0, published_at TEXT,
                    price_sats INTEGER DEFAULT 0, currency TEXT DEFAULT 'free'
                );
                INSERT INTO hub_lobby (repo_name, published_at) VALUES ('legacy-entry', '2026-08-01');",
            )
            .unwrap();
        }
        // 新代码打开（create_schema → migrate_hub_lobby_columns 幂等补列）
        let h = NexHubLobbyRouteHandler::with_db_path(&path, &dir);
        let legacy = h
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "legacy-entry")
            .expect("存量行可读");
        assert_eq!(legacy.source_node, "local", "存量行回填 local");
    }

    // 30. 联邦纯函数：载荷构造 + 节点名净化
    #[test]
    fn fed_pure_payload_builder_and_node_sanitize() {
        assert_eq!(sanitize_fed_node("  node-106 "), "node-106");
        assert_eq!(sanitize_fed_node(""), "peer");
        assert_eq!(sanitize_fed_node(&"x".repeat(65)), "peer");
        let e = entry("p", "", &[], 0, "2026-08-22T10:00:00+08:00");
        let p = build_nexhub_lobby_fed_payload("node-x", &e);
        assert_eq!(p["fed"], "nexhub_lobby");
        assert_eq!(p["node"], "node-x");
        assert_eq!(p["entry"]["repo_name"], "p");
    }

    // ---- 两步联邦（/:name/federate 端点）：本地发布 → 显式推送 ----

    /// federate 端点测试 fixture：临时目录裸仓库 + handler（admin token + 捕获通道）。
    fn federate_fixture(
        repo: &str,
        readme: &str,
    ) -> (NexHubLobbyRouteHandler, Arc<CapturedTransport>) {
        let dir = tempdir();
        make_bare_repo(&dir, repo, "", readme);
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
        h.fed_endpoint().set_transport(t.clone(), "node-opt".into());
        (h, t)
    }

    /// 31. federate 端点：admin 发布的本地条目 → 推送广播 + federated 置位；
    ///     重复推送=重新推送（first_push=false，再次广播刷新对端快照）。
    #[tokio::test]
    async fn federate_endpoint_admin_pushes_and_repushes() {
        let (h, t) = federate_fixture("admin-fed", "# Push");
        // 第一步：发布（admin 通道，字符串 publisher）→ 仅本地
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "admin-fed", "publisher": "local"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        assert_eq!(r.body["federated"], false, "发布恒未推送");
        // 第二步：推送（admin 恒可，含平台托管条目）
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/admin-fed/federate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body["ok"], true);
        assert_eq!(r.body["action"], "federate");
        assert_eq!(r.body["federated"], true);
        assert_eq!(r.body["first_push"], true, "首次推送");
        {
            let payloads = t.0.lock().unwrap();
            assert_eq!(payloads.len(), 1, "推送应广播: {payloads:?}");
            assert_eq!(payloads[0]["entry"]["repo_name"], "admin-fed");
            assert_eq!(payloads[0]["entry"]["federated"], true, "载荷携带标志");
        }
        // 重新推送：再次调用 → 再次广播（对端同源刷新），标志保持 true
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/admin-fed/federate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body["first_push"], false, "二次推送=重新推送");
        assert_eq!(t.0.lock().unwrap().len(), 2, "重新推送应再次广播");
        // 标志持久化（DB 快照 + HTTP 列表，前端 🌐 标记依据）
        assert!(h.entries_snapshot()[0].federated);
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(list.body[0]["federated"], true, "列表接口返回推送状态");
    }

    /// 32. federate 端点：未推送的本地条目才存在「推送」路径——不存在的条目 404
    ///     （不存在「直接发布到联邦」）；无身份 → 401。
    #[tokio::test]
    async fn federate_endpoint_missing_entry_404_and_requires_auth() {
        let (h, _t) = federate_fixture("fed-gate", "# G");
        // 无身份 → 401
        let r = h
            .handle(post_req(
                "/api/v1/nexhub/lobby/fed-gate/federate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 401, "推送需身份: {r:?}");
        // 条目不在本地大厅（未发布）→ 404：联邦只能从已发布条目推送
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/never-published/federate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 404, "未发布条目不可推送: {r:?}");
        assert!(
            r.body["error"]
                .as_str()
                .unwrap()
                .contains("先发布到本地大厅"),
            "404 文案引导两步流程: {r:?}"
        );
    }

    /// 33. federate 端点权限：owner_kind=pubkey 条目仅 owner 同 pubkey 或 admin
    ///     可推送（他人 403）；存量字符串条目仅 admin（pubkey token 403）。
    #[tokio::test]
    async fn federate_endpoint_owner_gating() {
        let (h, t) = federate_fixture("gated-fed", "# Gate");
        let (owner_pk, owner_token) = login(&h, &new_key()).await;
        let (_, other_token) = login(&h, &new_key()).await;
        // owner（pubkey）发布 → 仅本地
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &owner_token,
                serde_json::json!({"repo": "gated-fed"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        // 他人推送 → 403（统一文案契约）
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/gated-fed/federate",
                &other_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "他人推送应 403: {r:?}");
        assert_eq!(r.body["error"], "仅项目所有者可操作");
        assert!(t.0.lock().unwrap().is_empty(), "403 未广播");
        assert!(!h.entries_snapshot()[0].federated, "404/403 均不置位");
        // owner 本人推送 → 200 广播
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/gated-fed/federate",
                &owner_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "owner 推送应放行: {r:?}");
        assert_eq!(t.0.lock().unwrap().len(), 1);
        // admin 推送他人 pubkey 条目 → 放行（平台管理；admin 重发布托管化场景）
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/gated-fed/federate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 推送应放行: {r:?}");
        // 存量字符串条目（admin 发布）对 pubkey token → 403；admin → 放行
        let (h2, _t2) = federate_fixture("legacy-fed", "# L");
        let r = h2
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": "legacy-fed", "publisher": "NexOS"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        let (_, token2) = login(&h2, &new_key()).await;
        let r = h2
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/legacy-fed/federate",
                &token2,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "存量字符串条目对链上身份应 403: {r:?}");
        let r = h2
            .handle(admin_post(
                "/api/v1/nexhub/lobby/legacy-fed/federate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 推送平台托管条目应放行: {r:?}");
        let _ = owner_pk;
    }

    /// 34. 重发布保留推送状态：已推送条目重复发布（刷新快照）→ federated 不回退
    ///     （对端快照以「重新推送」刷新，本地标记持续有效）。
    #[tokio::test]
    async fn republish_preserves_federated_flag() {
        let (h, t) = federate_fixture("keep-fed", "# Keep");
        let (_, token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            PATH_PUBLISH,
            &token,
            serde_json::json!({"repo": "keep-fed"}),
        ))
        .await
        .unwrap();
        h.handle(post_req_auth(
            "/api/v1/nexhub/lobby/keep-fed/federate",
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        assert_eq!(t.0.lock().unwrap().len(), 1);
        assert!(h.entries_snapshot()[0].federated, "已推送");
        // 重发布（刷新描述快照）→ 不广播、标志保留
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &token,
                serde_json::json!({"repo": "keep-fed", "description": "refreshed"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        assert_eq!(r.body["description"], "refreshed");
        assert_eq!(r.body["federated"], true, "重发布保留推送状态");
        assert_eq!(t.0.lock().unwrap().len(), 1, "重发布不广播（两步联邦）");
    }

    // =========================================================================
    // nexos 自动联邦 + PR 审核流 + 发版权限控制（2026-08-23 定稿）
    // =========================================================================

    /// 造带 feature 分支的裸仓 fixture：main（2 commits）+ feature 分支（1 commit）。
    /// 返回裸仓库路径。
    fn make_repo_with_feature_branch(repos_dir: &str, name: &str) -> String {
        let bare = make_bare_repo(repos_dir, name, "", "# PR target");
        let work = format!("{repos_dir}/.{name}-prwork");
        assert!(run(&["git", "clone", &bare, &work]).0, "clone work 失败");
        std::fs::write(format!("{work}/feature.txt"), "feat").unwrap();
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "feature work"
            ])
            .0
        );
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "push",
                "origin",
                "HEAD:refs/heads/feature-x"
            ])
            .0,
            "push feature 分支失败"
        );
        let _ = std::fs::remove_dir_all(&work);
        bare
    }

    /// 造**分叉**分支 fixture（真实 3-way 合并场景）：feature 加 feature.txt、
    /// main 再加 main-extra.txt——两分支各有对方没有的提交。返回裸仓库路径。
    fn make_repo_with_diverged_branches(repos_dir: &str, name: &str) -> String {
        let bare = make_bare_repo(repos_dir, name, "", "# PR target");
        let work = format!("{repos_dir}/.{name}-divwork");
        assert!(run(&["git", "clone", &bare, &work]).0, "clone work 失败");
        // feature 分支从 main 分出 + 提交 feature.txt
        assert!(
            run(&["git", "-C", &work, "checkout", "-q", "-b", "feature"]).0,
            "开 feature 分支失败"
        );
        std::fs::write(format!("{work}/feature.txt"), "feat").unwrap();
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "feature work"
            ])
            .0
        );
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "push",
                "origin",
                "HEAD:refs/heads/feature"
            ])
            .0,
            "push feature 失败"
        );
        // main 再推进一提交 main-extra.txt（两分支分叉）
        assert!(
            run(&["git", "-C", &work, "checkout", "-q", "main"]).0,
            "切回 main 失败"
        );
        std::fs::write(format!("{work}/main-extra.txt"), "main").unwrap();
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "main advance"
            ])
            .0
        );
        assert!(
            run(&["git", "-C", &work, "push", "origin", "HEAD:refs/heads/main"]).0,
            "push main 失败"
        );
        let _ = std::fs::remove_dir_all(&work);
        bare
    }

    /// F1. nexos 自动联邦：常驻即 federated=true（构造期通道未装配 → 广播跳过
    ///     不 panic）；通道注入即补推常驻条目（生产装配序：构造在先、p2p 注入
    ///     在后）——「nexos 一启动就在联邦大厅」，无需手动 federate。
    #[tokio::test]
    async fn auto_federation_seeds_nexos_federated_and_broadcasts() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempdir();
        make_bare_repo(&dir, "nexos", "NexOS system main repo", "# NexOS");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir);
        // 常驻 + 自动联邦标志（DB 快照 + HTTP 列表两路断言——前端 🌐 标记依据）
        let entries = h.entries_snapshot();
        assert_eq!(entries.len(), 1, "常驻发布: {entries:?}");
        assert!(entries[0].federated, "自动联邦：常驻即置推送标志");
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(list.body[0]["federated"], true, "列表接口返回推送状态");
        // 通道注入 → 补推一条（载荷字段完整）
        let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
        h.fed_endpoint().set_transport(t.clone(), "node-106".into());
        {
            let payloads = t.0.lock().unwrap();
            assert_eq!(payloads.len(), 1, "注入即补推常驻条目: {payloads:?}");
            let p = &payloads[0];
            assert_eq!(p["fed"], FED_KIND_NEXHUB_LOBBY);
            assert_eq!(p["node"], "node-106");
            assert_eq!(p["entry"]["repo_name"], "nexos");
            assert_eq!(p["entry"]["publisher"], SEED_PUBLISHER);
            assert_eq!(p["entry"]["federated"], true);
            assert_eq!(p["entry"]["source_node"], "local");
        } // 锁不跨 await
          // 重复注入通道 → 再补推（幂等：对端同源 Refreshed 兜底）
        h.fed_endpoint().set_transport(
            Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new()))),
            "n2".into(),
        );
        let t2 = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
        h.fed_endpoint()
            .set_transport(t2.clone(), "node-106".into());
        assert_eq!(t2.0.lock().unwrap().len(), 1, "重复注入同样补推");
    }

    /// F1a. 逃生口回归：env NEXOS_LOBBY_NO_AUTO_PUBLISH=1 → 发布**与**联邦一并
    ///      跳过（常驻无条目、注入通道零广播）。
    #[tokio::test]
    async fn auto_federation_env_escape_hatch_skips_all() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempdir();
        make_bare_repo(&dir, "nexos", "NexOS system main repo", "# NexOS");
        std::env::set_var(ENV_NO_AUTO_PUBLISH, "1");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir);
        std::env::remove_var(ENV_NO_AUTO_PUBLISH);
        assert!(h.entries_snapshot().is_empty(), "env=1 → 不自动发布");
        let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
        h.fed_endpoint().set_transport(t.clone(), "node-x".into());
        assert!(
            t.0.lock().unwrap().is_empty(),
            "env=1 → 注入通道也不补推（联邦一并停用）"
        );
    }

    /// P1. PR 创建：链上身份归因（author_pubkey=token pubkey + EVM 展示名，
    ///     body 自报忽略）、base_branch 定格默认分支；分支不存在 400、仓库
    ///     不存在 404、无身份 401、admin 代建 author=admin。
    #[tokio::test]
    async fn pr_create_attributed_and_validated() {
        let dir = tempdir();
        make_repo_with_feature_branch(&dir, "pr-repo");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (pk, token) = login(&h, &new_key()).await;
        // 无身份 → 401
        let r = h
            .handle(post_req(
                "/api/v1/nexhub/lobby/pr-repo/pulls",
                serde_json::json!({"title": "x", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 401);
        // 仓库不存在 → 404
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/no-such-repo/pulls",
                &token,
                serde_json::json!({"title": "x", "source_branch": "main"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 404, "{r:?}");
        // 分支不存在 → 400
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-repo/pulls",
                &token,
                serde_json::json!({"title": "x", "source_branch": "no-such-branch"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "{r:?}");
        // 正常创建：归因 token 身份（body 自报 author 一律无此字段可传——忽略）
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-repo/pulls",
                &token,
                serde_json::json!({
                    "title": "Add feature",
                    "description": "from contributor",
                    "source_branch": "feature-x"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "创建应 201: {r:?}");
        let id = r.body["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("pr-"), "id 契约 pr-<n>: {id}");
        assert_eq!(r.body["author_pubkey"], pk, "author=token pubkey");
        assert!(
            r.body["author_display"].as_str().unwrap().starts_with("0x"),
            "EVM 展示名"
        );
        assert_eq!(r.body["status"], "open");
        assert_eq!(r.body["base_branch"], "main", "base=实际默认分支");
        assert_eq!(r.body["source_branch"], "feature-x");
        assert_eq!(r.body["source_node"], "local");
        // admin 代建 → author=admin（回落通道）
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/pr-repo/pulls",
                serde_json::json!({"title": "ops PR", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        assert_eq!(r.body["author_pubkey"], "admin");
        // 空标题 → 400
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-repo/pulls",
                &token,
                serde_json::json!({"title": "  ", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400);
    }

    /// P2. PR 列表：?status= 过滤（open/merged/rejected/closed）；非法 status 400；
    ///     公开（无身份可读）。
    #[tokio::test]
    async fn pr_list_filters_by_status() {
        let dir = tempdir();
        make_repo_with_feature_branch(&dir, "pr-list");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let mk = |title: &str| {
            admin_post(
                "/api/v1/nexhub/lobby/pr-list/pulls",
                serde_json::json!({"title": title, "source_branch": "feature-x"}),
            )
        };
        let id1 = h.handle(mk("one")).await.unwrap().body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let id2 = h.handle(mk("two")).await.unwrap().body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let id3 = h.handle(mk("three")).await.unwrap().body["id"]
            .as_str()
            .unwrap()
            .to_string();
        // 合并 one、拒绝 two、关闭 three → 全量 3 / 各状态 1
        h.handle(admin_post(
            &format!("/api/v1/nexhub/lobby/pr-list/pulls/{id1}/merge"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        h.handle(admin_post(
            &format!("/api/v1/nexhub/lobby/pr-list/pulls/{id2}/reject"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        h.handle(admin_post(
            &format!("/api/v1/nexhub/lobby/pr-list/pulls/{id3}/close"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        for (status, want_id) in [("merged", &id1), ("rejected", &id2), ("closed", &id3)] {
            let r = h
                .handle(get_req(&format!(
                    "/api/v1/nexhub/lobby/pr-list/pulls?status={status}"
                )))
                .await
                .unwrap();
            assert_eq!(r.status, 200);
            let arr = r.body.as_array().unwrap();
            assert_eq!(arr.len(), 1, "{status} 应只 1 条: {arr:?}");
            assert_eq!(arr[0]["id"], *want_id);
        }
        // 全量（公开无身份）
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby/pr-list/pulls"))
            .await
            .unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 3);
        // 非法 status → 400
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby/pr-list/pulls?status=bogus"))
            .await
            .unwrap();
        assert_eq!(r.status, 400);
    }

    /// P3. PR 详情：diff_stat（git diff base..source --stat 摘要，分叉分支可见
    ///     feature.txt）；不存在的 PR 404。
    #[tokio::test]
    async fn pr_detail_includes_diff_stat() {
        let dir = tempdir();
        make_repo_with_diverged_branches(&dir, "pr-detail");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (_, token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-detail/pulls",
                &token,
                serde_json::json!({"title": "feat", "source_branch": "feature"}),
            ))
            .await
            .unwrap();
        let id = r.body["id"].as_str().unwrap().to_string();
        let r = h
            .handle(get_req(&format!(
                "/api/v1/nexhub/lobby/pr-detail/pulls/{id}"
            )))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        let stat = r.body["diff_stat"].as_str().unwrap();
        assert!(
            stat.contains("feature.txt"),
            "diff stat 应含 feature 分支新增文件: {stat}"
        );
        assert!(stat.contains("changed"), "应带 git --stat 汇总行: {stat}");
        assert_eq!(r.body["source_branch"], "feature");
        assert_eq!(r.body["base_branch"], "main");
        // 不存在 → 404
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby/pr-detail/pulls/pr-nope"))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
    }

    /// P4. PR 合并（admin 通道）：merge-tree 3-way 落地——base 分支推进到
    ///     merged_sha、feature 内容进 main 树、status=merged + reviewed_by=admin；
    ///     已 merged 不可重复合并（409）；合并冲突 409。
    #[tokio::test]
    async fn pr_merge_executes_bare_merge_and_blocks_remerge() {
        let dir = tempdir();
        let bare = make_repo_with_diverged_branches(&dir, "pr-merge");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (author_pk, author_token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-merge/pulls",
                &author_token,
                serde_json::json!({"title": "merge me", "source_branch": "feature"}),
            ))
            .await
            .unwrap();
        let id = r.body["id"].as_str().unwrap().to_string();
        // admin 合并（未发布到大厅的裸仓 → owner 判定无条目，仅 admin）
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-merge/pulls/{id}/merge"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 合并应 200: {r:?}");
        assert_eq!(r.body["status"], "merged");
        assert_eq!(r.body["reviewed_by"], "admin");
        let merged_sha = r.body["merged_sha"].as_str().unwrap().to_string();
        assert!(!merged_sha.is_empty());
        // base 分支已推进到 merged_sha（merge 提交在 main 头）
        let (ok, out) = run_git_sync(&bare, &["rev-parse", "refs/heads/main"]);
        assert!(ok);
        assert_eq!(out.trim(), merged_sha, "main 头应推进到合并提交");
        // feature 内容进了 main 树（3-way 真合并，非仅 ref 移动）
        let (ok, out) = run_git_sync(&bare, &["ls-tree", "--name-only", "refs/heads/main"]);
        assert!(ok);
        assert!(
            out.contains("feature.txt") && out.contains("main-extra.txt"),
            "两侧分叉内容都应在合并后的 main: {out}"
        );
        // 合并提交是双 parent（merge 提交形态）
        let (ok, out) = run_git_sync(&bare, &["log", "-1", "--format=%P", "refs/heads/main"]);
        assert!(ok);
        assert_eq!(
            out.split_whitespace().count(),
            2,
            "合并提交应双 parent: {out}"
        );
        // 已 merged 再合并 → 409（不可重复）
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-merge/pulls/{id}/merge"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "已 merged 不可重复: {r:?}");
        let _ = author_pk;
        // 冲突场景：main 先改 feature.txt → 从该点开 evil 分支再改同一文件 →
        // main 又改一次——合并基之后两侧都改了同一文件，3-way 必冲突 → 409
        let work = format!("{dir}/.pr-merge-evilwork");
        assert!(run(&["git", "clone", &bare, &work]).0);
        let git = |args: &[&str]| run(args);
        let edit_commit_push = |val: &str, msg: &str| {
            std::fs::write(format!("{work}/feature.txt"), val).unwrap();
            assert!(git(&["git", "-C", &work, "add", "-A"]).0);
            assert!(
                git(&[
                    "git",
                    "-C",
                    &work,
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-m",
                    msg
                ])
                .0
            );
        };
        assert!(git(&["git", "-C", &work, "checkout", "-q", "main"]).0);
        edit_commit_push("MAIN-EDIT", "main edits feature");
        assert!(git(&["git", "-C", &work, "push", "origin", "HEAD:refs/heads/main"]).0);
        assert!(git(&["git", "-C", &work, "checkout", "-q", "-b", "evil"]).0);
        edit_commit_push("EVIL-EDIT", "evil");
        assert!(git(&["git", "-C", &work, "push", "origin", "HEAD:refs/heads/evil"]).0);
        // main 在 evil 分叉点之后再改同一文件（制造双侧变更）
        assert!(git(&["git", "-C", &work, "checkout", "-q", "main"]).0);
        edit_commit_push("MAIN-EDIT-2", "main edits again");
        assert!(git(&["git", "-C", &work, "push", "origin", "HEAD:refs/heads/main"]).0);
        let _ = std::fs::remove_dir_all(&work);
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/pr-merge/pulls",
                serde_json::json!({"title": "evil", "source_branch": "evil"}),
            ))
            .await
            .unwrap();
        let evil_id = r.body["id"].as_str().unwrap().to_string();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-merge/pulls/{evil_id}/merge"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "冲突应 409: {r:?}");
        assert!(r.body["error"].as_str().unwrap().contains("冲突"));
    }

    /// P5. PR 合并权限矩阵：repo owner pubkey ✓ / 他人 403 / 存量字符串条目
    ///     （平台托管）仅 admin——pubkey 403。
    #[tokio::test]
    async fn pr_merge_owner_gating() {
        let dir = tempdir();
        make_repo_with_feature_branch(&dir, "owned");
        make_repo_with_feature_branch(&dir, "legacy-owned");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        // owner 发布 owned（owner_kind=pubkey）；admin 发布 legacy-owned（字符串）
        let (owner_pk, owner_token) = login(&h, &new_key()).await;
        let (_, other_token) = login(&h, &new_key()).await;
        let (_, contributor_token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                PATH_PUBLISH,
                &owner_token,
                serde_json::json!({"repo": "owned"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        h.handle(admin_post(
            PATH_PUBLISH,
            serde_json::json!({"repo": "legacy-owned", "publisher": "NexOS"}),
        ))
        .await
        .unwrap();
        // contributor 在两仓各开一个 PR
        let mk_pr = |repo: &str| {
            post_req_auth(
                &format!("/api/v1/nexhub/lobby/{repo}/pulls"),
                &contributor_token,
                serde_json::json!({"title": "contribution", "source_branch": "feature-x"}),
            )
        };
        let pr1 = h.handle(mk_pr("owned")).await.unwrap().body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let pr2 = h.handle(mk_pr("legacy-owned")).await.unwrap().body["id"]
            .as_str()
            .unwrap()
            .to_string();
        // 他人（非 owner 非 admin）合并 → 403
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/owned/pulls/{pr1}/merge"),
                &other_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "他人合并应 403: {r:?}");
        assert_eq!(r.body["error"], "仅 admin 或仓库所有者可审核该 PR");
        // owner 本人合并 → 200（reviewed_by=owner pubkey）
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/owned/pulls/{pr1}/merge"),
                &owner_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "owner 合并应放行: {r:?}");
        assert_eq!(r.body["reviewed_by"], owner_pk);
        // 存量字符串条目：pubkey 403 / admin 放行
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/legacy-owned/pulls/{pr2}/merge"),
                &owner_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "平台托管条目对链上身份应 403: {r:?}");
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/legacy-owned/pulls/{pr2}/merge"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 合并平台托管条目应放行: {r:?}");
    }

    /// P6. PR 拒绝：owner/admin ✓（status=rejected + reviewed_by 落档）；
    ///     他人 403；非 open 状态 409。
    #[tokio::test]
    async fn pr_reject_owner_gating_and_state_machine() {
        let dir = tempdir();
        make_repo_with_feature_branch(&dir, "pr-reject");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (owner_pk, owner_token) = login(&h, &new_key()).await;
        let (_, other_token) = login(&h, &new_key()).await;
        let (_, contributor_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            PATH_PUBLISH,
            &owner_token,
            serde_json::json!({"repo": "pr-reject"}),
        ))
        .await
        .unwrap();
        let pr1 = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-reject/pulls",
                &contributor_token,
                serde_json::json!({"title": "r1", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap()
            .body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let pr2 = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-reject/pulls",
                &contributor_token,
                serde_json::json!({"title": "r2", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap()
            .body["id"]
            .as_str()
            .unwrap()
            .to_string();
        // 他人拒绝 → 403
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/pr-reject/pulls/{pr1}/reject"),
                &other_token,
                serde_json::json!({"reason": "no"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "他人拒绝应 403: {r:?}");
        // owner 拒绝 → 200（reason 回显 + reviewed_by 落档）
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/pr-reject/pulls/{pr1}/reject"),
                &owner_token,
                serde_json::json!({"reason": "不符合规范"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "owner 拒绝应放行: {r:?}");
        assert_eq!(r.body["status"], "rejected");
        assert_eq!(r.body["reviewed_by"], owner_pk);
        assert_eq!(r.body["reason"], "不符合规范");
        assert!(r.body["reviewed_at"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        // 已 rejected 再拒绝 → 409（状态机）
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-reject/pulls/{pr1}/reject"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "非 open 不可再拒绝: {r:?}");
        // admin 拒绝 → 200
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-reject/pulls/{pr2}/reject"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["reviewed_by"], "admin");
    }

    /// P7. PR 关闭：author 本人 ✓ / admin ✓ / 他人 403；非 open 409。
    #[tokio::test]
    async fn pr_close_author_or_admin_only() {
        let dir = tempdir();
        make_repo_with_feature_branch(&dir, "pr-close");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        let (author_pk, author_token) = login(&h, &new_key()).await;
        let (_, other_token) = login(&h, &new_key()).await;
        let pr1 = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-close/pulls",
                &author_token,
                serde_json::json!({"title": "c1", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap()
            .body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let pr2 = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/pr-close/pulls",
                &author_token,
                serde_json::json!({"title": "c2", "source_branch": "feature-x"}),
            ))
            .await
            .unwrap()
            .body["id"]
            .as_str()
            .unwrap()
            .to_string();
        // 他人关闭 → 403
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/pr-close/pulls/{pr1}/close"),
                &other_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "他人关闭应 403: {r:?}");
        assert_eq!(r.body["error"], "仅 PR 作者或 admin 可关闭该 PR");
        // author 本人关闭 → 200
        let r = h
            .handle(post_req_auth(
                &format!("/api/v1/nexhub/lobby/pr-close/pulls/{pr1}/close"),
                &author_token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "author 关闭应放行: {r:?}");
        assert_eq!(r.body["status"], "closed");
        assert_eq!(r.body["closed_by"], author_pk);
        // 已 closed 再关 → 409
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-close/pulls/{pr1}/close"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "非 open 不可再关闭: {r:?}");
        // admin 关闭他人 PR → 200
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/lobby/pr-close/pulls/{pr2}/close"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 关闭应放行: {r:?}");
        // 无身份 → 401
        let r = h
            .handle(post_req(
                "/api/v1/nexhub/lobby/pr-close/pulls/pr-x/close",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 401);
    }

    /// R1. Release 创建：仅 admin（链上身份 403 / 无身份 401）；git tag 落到默认
    ///     分支头；重复 tag 409；非法 tag 400。
    #[tokio::test]
    async fn release_create_admin_only_and_git_tag_lands() {
        let dir = tempdir();
        let bare = make_bare_repo(&dir, "rel-repo", "", "# Rel");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        // 无身份 → 401
        let r = h
            .handle(post_req(
                "/api/v1/nexhub/lobby/rel-repo/releases",
                serde_json::json!({"tag": "v0.1.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 401);
        // 链上身份（即便是 owner）→ 403（发版是平台级权限）
        let (_, token) = login(&h, &new_key()).await;
        let r = h
            .handle(post_req_auth(
                "/api/v1/nexhub/lobby/rel-repo/releases",
                &token,
                serde_json::json!({"tag": "v0.1.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "链上身份发版应 403: {r:?}");
        // admin 创建 → 201 + git tag 落到 main 头
        let main_sha = {
            let (ok, out) = run_git_sync(&bare, &["rev-parse", "refs/heads/main"]);
            assert!(ok);
            out.trim().to_string()
        };
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/rel-repo/releases",
                serde_json::json!({"tag": "v1.0.0", "title": "首个版本", "notes": "初始发版"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "admin 发版应 201: {r:?}");
        let id = r.body["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("rel-"), "id 契约: {id}");
        assert_eq!(r.body["tag"], "v1.0.0");
        assert_eq!(r.body["title"], "首个版本");
        assert_eq!(r.body["created_by"], "admin");
        let (ok, out) = run_git_sync(&bare, &["rev-parse", "refs/tags/v1.0.0^{}"]);
        assert!(ok, "tag 应存在于裸仓");
        assert_eq!(out.trim(), main_sha, "轻量 tag 定格在默认分支头");
        // 重复 tag → 409
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/rel-repo/releases",
                serde_json::json!({"tag": "v1.0.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "重复 tag 应 409: {r:?}");
        // 用户手动 git tag 过（DB 无行）→ git 侧冲突同样 409（stderr 归因）
        let (ok, _) = run_git_sync(&bare, &["tag", "manual-tag"]);
        assert!(ok, "手动打 tag 失败");
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/rel-repo/releases",
                serde_json::json!({"tag": "manual-tag"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "git 已有 tag（DB 无行）应 409: {r:?}");
        assert!(r.body["error"].as_str().unwrap().contains("已存在"));
        // 非法 tag（git 参数注入 / ref 规则）→ 400
        for bad in ["", "-evil", "a b", "v..x", ".starts-dot", "bad.lock"] {
            let r = h
                .handle(admin_post(
                    "/api/v1/nexhub/lobby/rel-repo/releases",
                    serde_json::json!({"tag": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 400, "非法 tag 应 400: {bad}");
        }
        // 仓库不存在 → 404
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/no-repo/releases",
                serde_json::json!({"tag": "v1"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
    }

    /// R2. Release 列表（公开）与删除（仅 admin）：删库行 + git tag 一并删；
    ///     链上身份删 403；删不存在 404。
    #[tokio::test]
    async fn release_list_and_delete() {
        let dir = tempdir();
        let bare = make_bare_repo(&dir, "rel-crud", "", "# Crud");
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
        for tag in ["v1.0.0", "v1.1.0"] {
            let r = h
                .handle(admin_post(
                    "/api/v1/nexhub/lobby/rel-crud/releases",
                    serde_json::json!({"tag": tag}),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 201, "{r:?}");
        }
        // 列表公开（无身份）
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby/rel-crud/releases"))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "应列 2 个 release: {arr:?}");
        let tags: Vec<&str> = arr.iter().map(|e| e["tag"].as_str().unwrap()).collect();
        assert!(tags.contains(&"v1.0.0") && tags.contains(&"v1.1.0"));
        assert_eq!(arr[0]["created_by"], "admin");
        // 链上身份删除 → 403
        let (_, token) = login(&h, &new_key()).await;
        let r = h
            .handle(delete_req_auth(
                "/api/v1/nexhub/lobby/rel-crud/releases/v1.0.0",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "链上身份删版应 403: {r:?}");
        // admin 删除 → 库行 + git tag 一并消失
        let r = h
            .handle(delete_req_auth(
                "/api/v1/nexhub/lobby/rel-crud/releases/v1.0.0",
                TEST_ADMIN_TOKEN,
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(r.body["action"], "release_delete");
        let r = h
            .handle(get_req("/api/v1/nexhub/lobby/rel-crud/releases"))
            .await
            .unwrap();
        assert_eq!(r.body.as_array().unwrap().len(), 1, "只剩 v1.1.0");
        let (ok, out) = run_git_sync(&bare, &["tag", "-l", "v1.0.0"]);
        assert!(ok);
        assert!(out.trim().is_empty(), "git tag 也应删除");
        // 删不存在 → 404
        let r = h
            .handle(delete_req_auth(
                "/api/v1/nexhub/lobby/rel-crud/releases/v9.9.9",
                TEST_ADMIN_TOKEN,
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
    }

    /// R3. Release 联邦广播：创建即广播 {fed=nexhub_release, node, release}；
    ///     对端 ingest 落地（Written，列表可见）；重放 Duplicate；本地同 tag
    ///     先到 Skipped；非法载荷 Invalid。
    #[tokio::test]
    async fn release_fed_broadcast_and_ingest() {
        let dir = tempdir();
        make_bare_repo(&dir, "rel-fed", "", "# FedRel");
        // 发版侧：捕获通道（authed_empty + 注入）
        let (h, t) = {
            let h =
                NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
            let t = Arc::new(CapturedTransport(std::sync::Mutex::new(Vec::new())));
            h.fed_endpoint().set_transport(t.clone(), "node-106".into());
            (h, t)
        };
        let r = h
            .handle(admin_post(
                "/api/v1/nexhub/lobby/rel-fed/releases",
                serde_json::json!({"tag": "v2.0.0", "title": "联邦版", "notes": "fed"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{r:?}");
        let release: Release = serde_json::from_value(r.body).unwrap();
        {
            let payloads = t.0.lock().unwrap();
            assert_eq!(payloads.len(), 1, "发版即广播: {payloads:?}");
            let p = &payloads[0];
            assert_eq!(p["fed"], FED_KIND_NEXHUB_RELEASE);
            assert_eq!(p["node"], "node-106");
            assert_eq!(p["release"]["repo_name"], "rel-fed");
            assert_eq!(p["release"]["tag"], "v2.0.0");
            assert_eq!(p["release"]["created_by"], "admin");
        } // 锁不跨 await
          // 接收侧（另一节点）：合法载荷 → Written + 列表可见（仅元数据，不打 tag）
        let (h2, _t2) = federated("node-b");
        let payload = build_nexhub_release_fed_payload("node-106", &release);
        assert_eq!(
            h2.fed_endpoint().ingest_release(&payload),
            LobbyFedIngest::Written
        );
        let r = h2
            .handle(get_req("/api/v1/nexhub/lobby/rel-fed/releases"))
            .await
            .unwrap();
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "远端 release 落地: {arr:?}");
        assert_eq!(arr[0]["tag"], "v2.0.0");
        assert_eq!(arr[0]["created_by"], "admin", "保留原发版人");
        // 重放 → Duplicate（不重复落地）
        assert_eq!(
            h2.fed_endpoint().ingest_release(&payload),
            LobbyFedIngest::Duplicate
        );
        assert_eq!(h2.entries_snapshot().len(), 0); // 不影响大厅条目
                                                    // 本地先发的同 tag（不同 id）→ Skipped（保护本地）
        let local_first = Release {
            id: new_release_id(),
            repo_name: "rel-fed".into(),
            tag: "v2.0.0".into(),
            title: "本地先到".into(),
            notes: String::new(),
            created_by: "admin".into(),
            created_at: now_iso(),
        };
        {
            let conn = h2.db.lock().expect("db poisoned");
            insert_release(&conn, &local_first).unwrap();
        }
        let conflicting = build_nexhub_release_fed_payload("node-777", &release);
        assert_eq!(
            h2.fed_endpoint().ingest_release(&conflicting),
            LobbyFedIngest::Skipped
        );
        let r = h2
            .handle(get_req("/api/v1/nexhub/lobby/rel-fed/releases"))
            .await
            .unwrap();
        assert_eq!(
            r.body.as_array().unwrap()[0]["title"],
            "本地先到",
            "本地同 tag 条目不被覆盖"
        );
        // 非法载荷 → Invalid（fed 类型错 / 缺 node / 坏 release / 非法 tag）
        assert_eq!(
            h2.fed_endpoint().ingest_release(
                &serde_json::json!({"fed": "nexhub_lobby", "node": "n", "release": release})
            ),
            LobbyFedIngest::Invalid
        );
        assert_eq!(
            h2.fed_endpoint()
                .ingest_release(&serde_json::json!({"fed": FED_KIND_NEXHUB_RELEASE})),
            LobbyFedIngest::Invalid
        );
        let bad_tag = Release {
            tag: "-evil".into(),
            ..release.clone()
        };
        assert_eq!(
            h2.fed_endpoint()
                .ingest_release(&build_nexhub_release_fed_payload("n", &bad_tag)),
            LobbyFedIngest::Invalid
        );
    }

    /// G1. 纯函数：分支名/tag 名校验（git 参数注入与 ref 规则防护）。
    #[test]
    fn branch_and_tag_name_validation_rules() {
        // 分支名
        assert!(validate_branch_name("feature-x").is_ok());
        assert!(validate_branch_name("feat/123_fix").is_ok());
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("-evil").is_err());
        assert!(validate_branch_name("a b").is_err());
        assert!(validate_branch_name("a..b").is_err());
        assert!(validate_branch_name("re^head").is_err());
        // tag 名（分支规则 + '.'/'/' 开头与 '.lock' 结尾禁用）
        assert!(validate_tag_name("v1.0.0").is_ok());
        assert!(validate_tag_name("release-2026-08").is_ok());
        assert!(validate_tag_name(".hidden").is_err());
        assert!(validate_tag_name("v1.lock").is_err());
        assert!(validate_tag_name(&"x".repeat(129)).is_err());
        assert!(validate_tag_name("-v1").is_err());
    }

    // ==========================================================================
    // 链上支付验真（dApp 一期接线，2026-08-31）
    //
    // 测试策略：核验本体（chain_verify.rs）由并行实现维护，这里**不触网**——
    // 经 ChainPayGate 注入固定 VerifyOutcome 的替身执行器（EvmTxVerifier 接缝），
    // 只断言**接线语义**：各结局映射的放行/拒绝/标注、链上事实落库、开关关闭
    // 回旧行为、非 EVM 货币不触发核验。
    // ==========================================================================

    /// 计数替身：verify 恒返回固定 outcome，并记录调用次数（断言「核了/没核」）。
    struct CountingVerifier {
        outcome: VerifyOutcome,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl EvmTxVerifier for CountingVerifier {
        fn verify(
            &self,
            _rpc_urls: &[String],
            _proof: &TxProof,
            _timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = VerifyOutcome> + Send>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let o = self.outcome.clone();
            Box::pin(async move { o })
        }
    }

    /// 构造带核验替身的 handler：开关注入 + 缺省收款地址/链 ID 注入（绕开 env
    /// 并行竞态），返回 (handler, 调用计数)。`repos_dir` 走真实 git fixture
    /// （发布付费条目前置）。
    fn hub_with_outcome(
        outcome: VerifyOutcome,
        enabled: bool,
        repos_dir: &str,
    ) -> (
        NexHubLobbyRouteHandler,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = ChainPayGate::with_parts(
            enabled,
            None,
            Some("0xpayto-recipient"),
            Some(11155111),
            Duration::from_secs(1),
            None,
            6,
            std::sync::Arc::new(CountingVerifier {
                outcome,
                calls: calls.clone(),
            }),
        );
        let h = NexHubLobbyRouteHandler::with_repos_dir(repos_dir)
            .with_admin_token(TEST_ADMIN_TOKEN)
            .with_chain_verify(gate);
        (h, calls)
    }

    /// 发布一条付费条目（admin 通道，publisher 保留字符串）并返回名。
    async fn publish_paid_entry(
        h: &NexHubLobbyRouteHandler,
        name: &str,
        price: u64,
        currency: &str,
    ) {
        let r = h
            .handle(admin_post(
                PATH_PUBLISH,
                serde_json::json!({"repo": name, "price_sats": price, "currency": currency}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "发布付费条目应 201: {r:?}");
    }

    /// admin 购买（buyer="admin" 回落通道）。
    async fn purchase(
        h: &NexHubLobbyRouteHandler,
        name: &str,
        body: serde_json::Value,
    ) -> ApiResponse {
        h.handle(admin_post(
            &format!("/api/v1/nexhub/lobby/{name}/purchase"),
            body,
        ))
        .await
        .unwrap()
    }

    // CV1. 纯函数：NEXOS_CHAIN_RPC_URLS 解析（好值/数组/坏 JSON/坏形状/他链忽略）
    #[test]
    fn chain_rpc_env_parse_variants() {
        let single = r#"{"11155111": "https://a.example"}"#;
        assert_eq!(
            parse_chain_rpc_env(single, 11155111),
            vec!["https://a.example".to_string()]
        );
        assert!(parse_chain_rpc_env(single, 1).is_empty(), "他链不取");
        let arr = r#"{"1337": ["http://127.0.0.1:8545", " http://127.0.0.1:8546 ", ""]}"#;
        assert_eq!(
            parse_chain_rpc_env(arr, 1337),
            vec![
                "http://127.0.0.1:8545".to_string(),
                "http://127.0.0.1:8546".to_string()
            ],
            "数组取全部非空项（trim，空串剔除）"
        );
        assert!(
            parse_chain_rpc_env("not-json", 1).is_empty(),
            "坏 JSON 忽略"
        );
        assert!(parse_chain_rpc_env("[1,2]", 1).is_empty(), "非对象忽略");
        assert!(
            parse_chain_rpc_env(r#"{"1": 42}"#, 1).is_empty(),
            "键形状非法（非串/数组）忽略"
        );
        assert!(parse_chain_rpc_env("", 1).is_empty(), "空串无配置");
        assert!(parse_chain_rpc_env("   ", 1).is_empty(), "空白无配置");
    }

    // CV2. 纯函数：RPC 候选链三段拼接（显式 → env → fallback）
    #[test]
    fn rpc_candidates_order_explicit_env_fallback() {
        let gate = ChainPayGate::with_parts(
            true,
            Some(r#"{"1337": ["http://env-first:8545"]}"#),
            None,
            None,
            Duration::from_secs(1),
            None,
            6,
            std::sync::Arc::new(CountingVerifier {
                outcome: VerifyOutcome::Pending,
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        );
        let c = gate.rpc_candidates(None, 1337);
        assert_eq!(
            c.first().map(String::as_str),
            Some("http://env-first:8545"),
            "无显式时 env 段在前（其后接 fallback 兜底）: {c:?}"
        );
        assert!(c.len() >= 2, "fallback_rpc_for(1337) 应垫后: {c:?}");
        let c = gate.rpc_candidates(Some("http://explicit:8545"), 1337);
        assert_eq!(c.first().map(String::as_str), Some("http://explicit:8545"));
        assert_eq!(c.get(1).map(String::as_str), Some("http://env-first:8545"));
        assert_eq!(c.len(), 3, "显式 → env → 兜底 三段: {c:?}");
    }

    // CV3. 纯函数：链 ID 解析优先级（显式 > 数值 chain 串 > env 缺省）
    #[test]
    fn resolve_chain_id_precedence() {
        assert_eq!(
            resolve_chain_id(Some(1337), Some("11155111"), Some(1)),
            Some(1337)
        );
        assert_eq!(
            resolve_chain_id(None, Some("11155111"), Some(1)),
            Some(11155111)
        );
        assert_eq!(
            resolve_chain_id(None, Some("eth"), Some(1)),
            Some(1),
            "货币名不作链 ID"
        );
        assert_eq!(resolve_chain_id(None, None, Some(1)), Some(1));
        assert_eq!(resolve_chain_id(None, None, None), None);
    }

    // CV4. 纯函数：金额 → wei（整数透传=最小单位；小数按 18 位换算；非法 None）
    #[test]
    fn to_wei_str_integer_and_decimal() {
        assert_eq!(
            to_wei_str("500"),
            Some("500".to_string()),
            "整数=已是最小单位"
        );
        assert_eq!(
            to_wei_str("10000000000000000000"),
            Some("10000000000000000000".to_string())
        );
        assert_eq!(
            to_wei_str("0.02"),
            Some("20000000000000000".to_string()),
            "0.02 ETH = 2e16 wei（18 位小数假设）"
        );
        assert_eq!(to_wei_str("1.5"), Some("1500000000000000000".to_string()));
        assert_eq!(to_wei_str("0.000000000000000001"), Some("1".to_string()));
        assert!(
            to_wei_str("0.0000000000000000001").is_none(),
            "小数超 18 位"
        );
        assert!(to_wei_str("").is_none());
        assert!(to_wei_str("abc").is_none());
        assert!(to_wei_str("1.2.3").is_none());
        assert!(to_wei_str("-1").is_none());
    }

    // CV5. 纯函数：VerifyOutcome → 业务判定（语义表全覆盖）
    #[test]
    fn verdict_for_maps_all_outcomes() {
        let v = verdict_for(VerifyOutcome::Verified {
            block_number: 42,
            to: "0xpayto".into(),
            value_wei: "500".into(),
            token: None,
        });
        assert_eq!(
            v,
            ChainPayVerdict::Allow {
                block_number: 42,
                value_wei: "500".into(),
                token: None,
            }
        );
        // ERC-20 形状的 Verified：token 透传到 Allow（展示/落库标注用）。
        let v = verdict_for(VerifyOutcome::Verified {
            block_number: 43,
            to: "0xpayto".into(),
            value_wei: "10000000".into(),
            token: Some("0xdac17f958d2ee523a2206206994597c13d831ec7".into()),
        });
        assert_eq!(
            v,
            ChainPayVerdict::Allow {
                block_number: 43,
                value_wei: "10000000".into(),
                token: Some("0xdac17f958d2ee523a2206206994597c13d831ec7".into()),
            }
        );
        let v = verdict_for(VerifyOutcome::Pending);
        match v {
            ChainPayVerdict::Deny {
                status,
                reason,
                retryable,
            } => {
                assert_eq!(status, 409);
                assert!(retryable, "Pending 是可重试语义，不是欺诈");
                assert!(reason.contains("重试"), "文案应引导稍后重试: {reason}");
            }
            other => panic!("Pending 应 Deny: {other:?}"),
        }
        let v = verdict_for(VerifyOutcome::Mismatch {
            field: "to".into(),
            expect: "0xpayto".into(),
            actual: "0xattacker".into(),
        });
        match v {
            ChainPayVerdict::Deny { status, reason, .. } => {
                assert_eq!(status, 409);
                assert!(
                    reason.contains("to") && reason.contains("0xattacker"),
                    "带字段与链上实际值: {reason}"
                );
            }
            other => panic!("Mismatch 应 Deny: {other:?}"),
        }
        let v = verdict_for(VerifyOutcome::NotFound);
        match v {
            ChainPayVerdict::Deny { status, .. } => assert_eq!(status, 400),
            other => panic!("NotFound 应 Deny: {other:?}"),
        }
        assert_eq!(
            verdict_for(VerifyOutcome::RpcError {
                detail: "timeout".into()
            }),
            ChainPayVerdict::Degrade {
                detail: "timeout".into()
            },
            "RpcError 降级放行"
        );
    }

    // CV6. 集成：Verified → 200 放行 + chain_verify 标注 + 链上事实落库（收据结构）
    #[tokio::test]
    async fn purchase_verified_persists_chain_facts() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-paid", "", "# E");
        let (h, calls) = hub_with_outcome(
            VerifyOutcome::Verified {
                block_number: 42,
                to: "0xpayto-recipient".into(),
                value_wei: "500".into(),
                token: None,
            },
            true,
            &dir,
        );
        publish_paid_entry(&h, "eth-paid", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-paid",
            serde_json::json!({"txid": "0xreal", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 200, "核验通过应放行: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "verified");
        assert_eq!(r.body["chain_verify"]["block_number"], 42);
        assert_eq!(r.body["chain_verify"]["chain_id"], 11155111);
        assert_eq!(r.body["chain_verify"]["value_wei"], "500");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "核验恰一次"
        );
        // 落库：GET /entitlements 审计可见链上事实
        let d = h
            .handle(admin_get("/api/v1/nexhub/lobby/entitlements?repo=eth-paid"))
            .await
            .unwrap();
        let list = d.body.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["chain_block"], 42, "块高落库: {list:?}");
        assert_eq!(list[0]["chain_value_wei"], "500", "实付 wei 落库: {list:?}");
    }

    // CV7. 集成：Mismatch → 409 拒绝（带字段与链上实际值），不落授权
    #[tokio::test]
    async fn purchase_mismatch_rejected_no_entitlement() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-mm", "", "# E");
        let (h, _calls) = hub_with_outcome(
            VerifyOutcome::Mismatch {
                field: "to".into(),
                expect: "0xpayto-recipient".into(),
                actual: "0xattacker".into(),
            },
            true,
            &dir,
        );
        publish_paid_entry(&h, "eth-mm", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-mm",
            serde_json::json!({"txid": "0xforged", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 409, "Mismatch 应拒绝: {r:?}");
        let err = r.body["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("to") && err.contains("0xattacker"),
            "错误带字段+实际值: {err}"
        );
        let d = h
            .handle(admin_get("/api/v1/nexhub/lobby/entitlements?repo=eth-mm"))
            .await
            .unwrap();
        assert!(
            d.body.as_array().unwrap().is_empty(),
            "不落授权（白嫖被挡）"
        );
    }

    // CV8. 集成：Pending → 409 可重试（不当欺诈），不落授权；稍后重试语义在文案
    #[tokio::test]
    async fn purchase_pending_is_retryable_409() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-pend", "", "# E");
        let (h, _calls) = hub_with_outcome(VerifyOutcome::Pending, true, &dir);
        publish_paid_entry(&h, "eth-pend", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-pend",
            serde_json::json!({"txid": "0xinflight", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 409, "Pending 应 409: {r:?}");
        assert!(
            r.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("重试"),
            "应提示稍后重试: {r:?}"
        );
        let d = h
            .handle(admin_get("/api/v1/nexhub/lobby/entitlements?repo=eth-pend"))
            .await
            .unwrap();
        assert!(d.body.as_array().unwrap().is_empty(), "Pending 不落授权");
    }

    // CV9. 集成：NotFound → 400（伪造 txid 直接挡）
    #[tokio::test]
    async fn purchase_notfound_is_400() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-nf", "", "# E");
        let (h, _calls) = hub_with_outcome(VerifyOutcome::NotFound, true, &dir);
        publish_paid_entry(&h, "eth-nf", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-nf",
            serde_json::json!({"txid": "0xdoesnotexist", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 400, "NotFound 应 400: {r:?}");
    }

    // CV10. 集成：RpcError → 降级放行（200）+ degraded 标注 + 无链上事实
    #[tokio::test]
    async fn purchase_rpc_error_degrades_to_pass() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-rpc", "", "# E");
        let (h, _calls) = hub_with_outcome(
            VerifyOutcome::RpcError {
                detail: "all rpc unreachable".into(),
            },
            true,
            &dir,
        );
        publish_paid_entry(&h, "eth-rpc", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-rpc",
            serde_json::json!({"txid": "0xmaybe-real", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 200, "RPC 故障不应阻断交易: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "degraded", "降级必须可见");
        let d = h
            .handle(admin_get("/api/v1/nexhub/lobby/entitlements?repo=eth-rpc"))
            .await
            .unwrap();
        let list = d.body.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0]["chain_block"],
            serde_json::Value::Null,
            "降级不产生链上事实"
        );
    }

    // CV11. 集成：开关关闭（NEXOS_CHAIN_VERIFY_ENABLED=0 语义）→ 完全回旧行为
    //       （伪造 txid 也过，且响应无任何 chain_verify 标注、核验 0 次调用）
    #[tokio::test]
    async fn purchase_disabled_gate_falls_back_to_legacy() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-off", "", "# E");
        let (h, calls) = hub_with_outcome(
            VerifyOutcome::NotFound, // 即使核了也会拒——证明根本没核
            false,
            &dir,
        );
        publish_paid_entry(&h, "eth-off", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-off",
            serde_json::json!({"txid": "0xforged", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 200, "开关关闭=旧行为（非空即过）: {r:?}");
        assert!(
            r.body.get("chain_verify").is_none(),
            "旧行为不带任何标注: {r:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "核验零调用"
        );
    }

    // CV12. 集成：缺收款地址（env NEXOS_HUB_PAY_TO 未配且 body 不收）→
    //        放行 + unverified 标注（不静默假装核过）
    #[tokio::test]
    async fn purchase_without_pay_to_marks_unverified() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-nopay", "", "# E");
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = ChainPayGate::with_parts(
            true,
            None,
            None, // 无缺省收款地址
            Some(11155111),
            Duration::from_secs(1),
            None,
            6,
            std::sync::Arc::new(CountingVerifier {
                outcome: VerifyOutcome::Verified {
                    block_number: 1,
                    to: String::new(),
                    value_wei: String::new(),
                    token: None,
                },
                calls: calls.clone(),
            }),
        );
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir)
            .with_admin_token(TEST_ADMIN_TOKEN)
            .with_chain_verify(gate);
        publish_paid_entry(&h, "eth-nopay", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-nopay",
            serde_json::json!({"txid": "0xwhatever", "amount_sats": 500, "currency": "eth"}),
        )
        .await;
        assert_eq!(r.status, 200, "信息不全不硬拒（标注放行）: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "unverified");
        assert!(
            r.body["chain_verify"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("收款地址"),
            "应说明缺什么: {r:?}"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // CV13. 集成：非 EVM 货币（btc）不触发核验（一期核验域=eth/evm）
    #[tokio::test]
    async fn purchase_btc_skips_chain_verify() {
        let dir = tempdir();
        make_bare_repo(&dir, "btc-paid", "", "# B");
        let (h, calls) = hub_with_outcome(
            VerifyOutcome::NotFound, // 若误触发即拒——证明没触发
            true,
            &dir,
        );
        publish_paid_entry(&h, "btc-paid", 500, "btc").await;
        let r = purchase(
            &h,
            "btc-paid",
            serde_json::json!({"txid": "btc_tx", "amount_sats": 500, "currency": "btc"}),
        )
        .await;
        assert_eq!(r.status, 200, "btc 走自证收据（旧行为）: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "unverified");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "核验零调用"
        );
    }

    // CV14. 集成：body 显式 chain_id 优先于网关缺省（TxProof 定位到用户指的链）
    #[tokio::test]
    async fn purchase_explicit_chain_id_wins() {
        let dir = tempdir();
        make_bare_repo(&dir, "eth-cid", "", "# E");
        let (h, _calls) = hub_with_outcome(
            VerifyOutcome::Verified {
                block_number: 7,
                to: "0xpayto-recipient".into(),
                value_wei: "500".into(),
                token: None,
            },
            true,
            &dir,
        );
        publish_paid_entry(&h, "eth-cid", 500, "eth").await;
        let r = purchase(
            &h,
            "eth-cid",
            serde_json::json!({"txid": "0xon137", "amount_sats": 500, "currency": "eth", "chain_id": 1337}),
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.body["chain_verify"]["chain_id"], 1337,
            "显式 chain_id 优先"
        );
    }

    // CV15. 集成：悬赏 approve（eth）——Verified 放行 + 标注；Mismatch 拒绝且
    //        悬赏停在 submitted（不误标 paid）。收款地址来自 body pay_to（hunter）。
    #[tokio::test]
    async fn bounty_approve_verified_and_mismatch() {
        let dir = tempdir();
        let (h, _calls) = hub_with_outcome(
            VerifyOutcome::Verified {
                block_number: 99,
                to: "0xhunter".into(),
                value_wei: "1000".into(),
                token: None,
            },
            true,
            &dir,
        );
        let id = create_bounty(&h, 1000, "eth").await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "https://example.com/pr/1"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                serde_json::json!({
                    "txid": "0xpayout", "amount_sats": 1000, "currency": "eth",
                    "pay_to": "0xhunter", "chain_id": 11155111
                }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "核验通过应放行: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "verified");
        assert_eq!(r.body["chain_verify"]["block_number"], 99);
        // Mismatch 翼：新悬赏 + 注入 Mismatch
        let (h2, _c2) = hub_with_outcome(
            VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "1000".into(),
                actual: "1".into(),
            },
            true,
            &dir,
        );
        let id2 = create_bounty(&h2, 1000, "eth").await;
        let (_, hunter2_token) = login(&h2, &new_key()).await;
        h2.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id2}/submit"),
            &hunter2_token,
            serde_json::json!({"solution_url": "u"}),
        ))
        .await
        .unwrap();
        let r = h2
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id2}/approve"),
                serde_json::json!({
                    "txid": "0xshort", "amount_sats": 1000, "currency": "eth",
                    "pay_to": "0xhunter", "chain_id": 11155111
                }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 409, "Mismatch 应拒绝验收: {r:?}");
        let d = h2
            .handle(get_req(&format!("/api/v1/nexhub/bounty/{id2}")))
            .await
            .unwrap();
        assert_eq!(d.body["status"], "submitted", "悬赏不应被误标 paid: {d:?}");
    }

    // CV16. 集成：approve 不带 pay_to（eth）→ unverified 放行（悬赏不回落节点
    //        收款地址——那会错杀发给 hunter 的真支付）
    #[tokio::test]
    async fn bounty_approve_without_pay_to_unverified() {
        let dir = tempdir();
        let (h, calls) = hub_with_outcome(
            VerifyOutcome::NotFound, // 若误触发即拒——证明没触发
            true,
            &dir,
        );
        let id = create_bounty(&h, 1000, "eth").await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "u"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                serde_json::json!({"txid": "0xnoaddr", "amount_sats": 1000, "currency": "eth"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "缺 pay_to 不硬拒: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "unverified");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "未构造凭证不核验"
        );
    }

    // ==========================================================================
    // 链上支付验真二期（2026-09-02）：ERC-20（USDT@EVM）+ AmountRule
    // ==========================================================================

    /// 凭证捕获替身：记录最近一次收到的 TxProof（断言 erc20/amount_rule/金额
    /// 换算的接线正确性），恒返回固定 outcome。
    struct ProofCaptureVerifier {
        outcome: VerifyOutcome,
        proof: std::sync::Arc<std::sync::Mutex<Option<TxProof>>>,
    }

    impl EvmTxVerifier for ProofCaptureVerifier {
        fn verify(
            &self,
            _rpc_urls: &[String],
            proof: &TxProof,
            _timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = VerifyOutcome> + Send>> {
            *self.proof.lock().expect("proof poisoned") = Some(proof.clone());
            let o = self.outcome.clone();
            Box::pin(async move { o })
        }
    }

    /// 带凭证捕获的网关构造（USDT 合约/decimals 可注入）。
    fn capture_gate(
        outcome: VerifyOutcome,
        usdt_evm_contract: Option<&str>,
    ) -> (
        ChainPayGate,
        std::sync::Arc<std::sync::Mutex<Option<TxProof>>>,
    ) {
        let proof = std::sync::Arc::new(std::sync::Mutex::new(None));
        let gate = ChainPayGate::with_parts(
            true,
            None,
            Some("0xpayto-recipient"),
            Some(11155111),
            Duration::from_secs(1),
            usdt_evm_contract,
            6,
            std::sync::Arc::new(ProofCaptureVerifier {
                outcome,
                proof: proof.clone(),
            }),
        );
        (gate, proof)
    }

    /// 主网 USDT 合约（Tether）——测试常量。
    const USDT_CONTRACT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";

    // CV17. 纯函数：to_min_unit_str（ERC-20 decimals 换算 + native 18 位等价 + 边界）
    #[test]
    fn to_min_unit_str_decimals_variants() {
        assert_eq!(
            to_min_unit_str("10.00", 6),
            Some("10000000".to_string()),
            "10.00 USDT = 1e7 最小单位（网关价目形状）"
        );
        assert_eq!(to_min_unit_str("0.01", 6), Some("10000".to_string()));
        assert_eq!(
            to_min_unit_str("10000000", 6),
            Some("10000000".to_string()),
            "整数=已是最小单位透传（NexHub 条目语义）"
        );
        assert_eq!(to_min_unit_str("0.000001", 6), Some("1".to_string()));
        assert!(to_min_unit_str("0.0000001", 6).is_none(), "小数超 6 位拒绝");
        assert_eq!(
            to_min_unit_str("0.02", 18),
            Some("20000000000000000".to_string()),
            "18 位与 to_wei_str 等价"
        );
        assert_eq!(to_wei_str("0.02"), to_min_unit_str("0.02", 18));
        assert_eq!(to_min_unit_str("7", 0), Some("7".to_string()));
        assert!(to_min_unit_str("1.5", 0).is_none(), "0 位小数不容小数");
        assert!(to_min_unit_str("", 6).is_none());
        assert!(to_min_unit_str("abc", 6).is_none());
        assert!(to_min_unit_str("1.2.3", 6).is_none());
    }

    // CV18. 编排：usdt + EVM 链 + env 合约 → 构造 ERC-20 凭证（decimals 换算
    //       金额、token 透传到结论）；hints.amount_rule=AtLeast 传达到凭证。
    #[tokio::test]
    async fn usdt_evm_builds_erc20_proof_and_token_marker() {
        let (gate, proof) = capture_gate(
            VerifyOutcome::Verified {
                block_number: 55,
                to: "0xpayto-recipient".into(),
                value_wei: "10000000".into(),
                token: Some(USDT_CONTRACT.into()),
            },
            Some(USDT_CONTRACT),
        );
        let check = check_chain_payment(
            &gate,
            "usdt",
            "0xusdt-tx",
            "10.00",
            &ChainPayHints {
                pay_to: Some("0xpayto-recipient"),
                amount_rule: AmountRule::AtLeast,
                ..Default::default()
            },
        )
        .await;
        match &check {
            ChainPayCheck::Verified {
                chain_id,
                block_number,
                value_wei,
                token,
            } => {
                assert_eq!(*chain_id, 11155111);
                assert_eq!(*block_number, 55);
                assert_eq!(value_wei, "10000000");
                assert_eq!(token.as_deref(), Some(USDT_CONTRACT), "ERC-20 结论带合约");
            }
            other => panic!("应 Verified: {other:?}"),
        }
        let p = proof.lock().unwrap().clone().expect("应捕获到凭证");
        assert_eq!(
            p.erc20,
            Some(Erc20Spec {
                contract: USDT_CONTRACT.to_string(),
                decimals: 6,
            }),
            "env 合约 + 默认 decimals=6"
        );
        assert_eq!(p.expected_value, "10000000", "10.00 按 6 位换算成最小单位");
        assert_eq!(p.amount_rule, AmountRule::AtLeast, "hints 规则透传");
        assert_eq!(p.expected_to, "0xpayto-recipient");
    }

    // CV19. 编排：usdt 但定位不到 EVM 链（TRON 形态）→ Unverified 人工通道，不构造凭证。
    #[tokio::test]
    async fn usdt_without_evm_chain_stays_manual() {
        let proof_cell = std::sync::Arc::new(std::sync::Mutex::new(None));
        let gate = ChainPayGate::with_parts(
            true,
            None,
            Some("0xpayto-recipient"),
            None, // 无缺省链 ID——TRON 场景没有 EVM chain_id
            Duration::from_secs(1),
            Some(USDT_CONTRACT),
            6,
            std::sync::Arc::new(ProofCaptureVerifier {
                outcome: VerifyOutcome::Verified {
                    block_number: 1,
                    to: String::new(),
                    value_wei: String::new(),
                    token: None,
                },
                proof: proof_cell.clone(),
            }),
        );
        let check = check_chain_payment(
            &gate,
            "usdt",
            "0xtron-tx",
            "10.00",
            &ChainPayHints {
                pay_to: Some("0xpayto-recipient"),
                ..Default::default()
            },
        )
        .await;
        match check {
            ChainPayCheck::Unverified(reason) => {
                assert!(
                    reason.contains("EVM") && (reason.contains("TRON") || reason.contains("人工")),
                    "应说明 TRON/人工通道: {reason}"
                );
            }
            other => panic!("应 Unverified: {other:?}"),
        }
        assert!(
            proof_cell.lock().unwrap().is_none(),
            "TRON usdt 不构造 EVM 凭证"
        );
    }

    // CV20. 编排：usdt + EVM 链但合约地址无处可寻 → Unverified（不猜合约地址）。
    #[tokio::test]
    async fn usdt_without_contract_does_not_guess() {
        let (gate, proof) = capture_gate(
            VerifyOutcome::Verified {
                block_number: 1,
                to: String::new(),
                value_wei: String::new(),
                token: None,
            },
            None, // env 未配
        );
        let check = check_chain_payment(
            &gate,
            "usdt",
            "0xusdt-tx",
            "10.00",
            &ChainPayHints {
                pay_to: Some("0xpayto-recipient"),
                ..Default::default()
            },
        )
        .await;
        match check {
            ChainPayCheck::Unverified(reason) => {
                assert!(reason.contains("合约"), "应说明缺合约配置: {reason}");
            }
            other => panic!("应 Unverified: {other:?}"),
        }
        assert!(proof.lock().unwrap().is_none(), "不猜合约=不构造凭证");
    }

    // CV21. 接线：purchase（usdt 条目 + body 合约/链 ID）→ **Exact** 规则 +
    //       ERC-20 凭证（body 合约优先于 env）。
    #[tokio::test]
    async fn purchase_usdt_exact_rule_and_erc20_proof() {
        let dir = tempdir();
        make_bare_repo(&dir, "usdt-paid", "", "# U");
        let body_contract = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"; // USDC 主网合约（模拟 body 覆盖）
        let (gate, proof) = capture_gate(
            VerifyOutcome::Verified {
                block_number: 66,
                to: "0xpayto-recipient".into(),
                value_wei: "10000000".into(),
                token: Some(body_contract.into()),
            },
            Some(USDT_CONTRACT),
        );
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir)
            .with_admin_token(TEST_ADMIN_TOKEN)
            .with_chain_verify(gate);
        publish_paid_entry(&h, "usdt-paid", 10_000_000, "usdt").await;
        let r = purchase(
            &h,
            "usdt-paid",
            serde_json::json!({
                "txid": "0xusdt", "amount_sats": 10_000_000, "currency": "usdt",
                "chain_id": 1, "erc20_contract": body_contract, "erc20_decimals": 6
            }),
        )
        .await;
        assert_eq!(r.status, 200, "ERC-20 核验通过应放行: {r:?}");
        assert_eq!(r.body["chain_verify"]["status"], "verified");
        assert_eq!(r.body["chain_verify"]["token"], body_contract);
        assert_eq!(r.body["chain_verify"]["value_wei"], "10000000");
        let p = proof.lock().unwrap().clone().expect("应捕获到凭证");
        assert_eq!(p.amount_rule, AmountRule::Exact, "购买流=等值对账");
        assert_eq!(
            p.erc20.as_ref().map(|s| s.contract.as_str()),
            Some(body_contract),
            "body 合约优先于 env"
        );
        assert_eq!(p.erc20.as_ref().map(|s| s.decimals), Some(6));
        assert_eq!(
            p.expected_value, "10000000",
            "amount_sats 整数=最小单位透传"
        );
        assert_eq!(p.chain_id, 1, "body chain_id 生效");
    }

    // CV22. 接线：bounty approve（eth）→ **AtLeast** 规则（多打不亏待 hunter）。
    #[tokio::test]
    async fn bounty_approve_uses_at_least_rule() {
        let dir = tempdir();
        let (gate, proof) = capture_gate(
            VerifyOutcome::Verified {
                block_number: 77,
                to: "0xhunter".into(),
                value_wei: "1200".into(),
                token: None,
            },
            None,
        );
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir)
            .with_admin_token(TEST_ADMIN_TOKEN)
            .with_chain_verify(gate);
        let id = create_bounty(&h, 1000, "eth").await;
        let (_, hunter_token) = login(&h, &new_key()).await;
        h.handle(post_req_auth(
            &format!("/api/v1/nexhub/bounty/{id}/submit"),
            &hunter_token,
            serde_json::json!({"solution_url": "u"}),
        ))
        .await
        .unwrap();
        let r = h
            .handle(admin_post(
                &format!("/api/v1/nexhub/bounty/{id}/approve"),
                serde_json::json!({
                    "txid": "0xoverpay", "amount_sats": 1000, "currency": "eth",
                    "pay_to": "0xhunter", "chain_id": 11155111
                }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "多打（1200>1000）应放行: {r:?}");
        assert_eq!(
            r.body["chain_verify"]["value_wei"], "1200",
            "Verified 携带链上实付"
        );
        let p = proof.lock().unwrap().clone().expect("应捕获到凭证");
        assert_eq!(p.amount_rule, AmountRule::AtLeast, "悬赏放款=AtLeast");
        assert!(p.erc20.is_none(), "eth 悬赏走 native 路径");
        assert_eq!(p.expected_value, "1000");
    }
}
