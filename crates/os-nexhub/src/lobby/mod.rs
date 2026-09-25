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
// 域子模块（2026-09-25 大文件拆分批，方法论同 film_hub v0.1.42 域子模块模式）：
// 原 ~11.8k 行单文件按域拆为 lobby/ 目录——本 mod.rs 保留核心（条目模型/
// git 快照/handler 构造/RouteHandler 路由表与 handle 分发/共享管道与建库/
// 条目 CRUD 与大厅统计），各域子模块承载专属数据层与逻辑（纯搬运，零行为
// 变化）。模块路径 crate::nexhub_lobby::* 与 os_nexhub::nexhub_lobby::*
// 经 lib.rs 的 #[path = "lobby/mod.rs"] 与下方重导出零变化。
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests;

mod identity;
mod releases;
mod sync;
mod transfer;

use identity::*;
use releases::*;
use sync::*;
// （transfer 域经下方 pub use 暴露——mod.rs 自身无直接引用，免私有 glob）

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

// 对外面（原 nexhub_lobby.rs 顶层 pub / pub(crate) 项随域搬家后在此重导出——
// lib.rs 的 pub use nexhub_lobby::{...} 与 os-api 侧 os_nexhub::nexhub_lobby::*
// 引用零变化）。
pub use identity::{
    chain_verify_json, check_chain_payment, evm_native_currency, parse_chain_rpc_env,
    resolve_chain_id, to_min_unit_str, to_wei_str, usdt_currency, verdict_for, Bounty,
    ChainPayCheck, ChainPayGate, ChainPayHints, ChainPayVerdict, Entitlement, EvmTxVerifier,
};
pub use releases::{
    build_nexhub_lobby_fed_payload, build_nexhub_release_fed_payload, sanitize_fed_node,
    PullRequest, Release, FED_KIND_NEXHUB_LOBBY, FED_KIND_NEXHUB_RELEASE,
};
pub use transfer::{LobbyFedEndpoint, LobbyFedIngest, LobbyFedTransport};
// pub(crate)：issues.rs（项目级 PR/Issues 协作层）与本 crate 内复用的合并/校验原语。
pub(crate) use releases::{
    merge_pr_blocking, merge_with_strategy_blocking, pr_diff_stat_blocking, validate_branch_name,
    validate_tag_name, MergeStrategy,
};

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
                // webhook 事件（release：published——webhooks.rs 全局槽未装配时 no-op）
                crate::webhooks::fire_release(&release, caller.actor());
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
    let _ = conn.busy_timeout(std::time::Duration::from_millis(3000)); // 防 SQLITE_BUSY 立败（审计 E#6）
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
