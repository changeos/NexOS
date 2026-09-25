//! NexHub 大厅·联邦传输通道域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! LobbyFedTransport 契约 + 接收端 LobbyFedEndpoint（os-p2p 注入/广播/
//! ingest 去重——`repo\0node\0published_at` 键只拦逐字节重放）。
//! 对外面经 lobby/mod.rs 重导出（crate::nexhub_lobby::* 路径零变化）。

use super::*;

/// 联邦传输通道：os-api 装配层注入 os-p2p 广播实现（**os-nexhub 不依赖
/// os-p2p**——审计 §6 独立性红线，通道抽象反转依赖方向）。
///
/// 语义：fire-and-forget 把载荷发给**所有已连接 peer**（实现方负责 fan-out）；
/// 未连接/失败静默丢弃（联邦是尽力而为的传播，不是可靠队列）。
pub trait LobbyFedTransport: Send + Sync {
    /// 广播一条联邦载荷给全部已连接 peer。
    fn broadcast(&self, payload: serde_json::Value);
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
    pub(super) db: Arc<Mutex<Connection>>,
    /// 注入的联邦传输通道 + 本节点名（None = 未装配 os-p2p，广播静默跳过）。
    pub(super) transport: Mutex<Option<(Arc<dyn LobbyFedTransport>, String)>>,
    /// 近期已见联邦条目键（`repo\0node\0published_at`）内存缓存，容量
    /// [`FED_SEEN_LIMIT`]。键含 `published_at`：只拦逐字节相同的**重放**；
    /// 同源**新快照**（发布侧重新 publish）键不同 → 穿透到 DB 权威路径
    /// （`Refreshed`）——否则对端刷新快照永远到不了本节点（2026-08-23 修复）。
    pub(super) seen: Mutex<std::collections::VecDeque<String>>,
    /// 仓库根目录（本地 bare 副本落点 `<repos_root>/<name>.git`——nexos 自动
    /// 跟随拉取的更新目标；与 handler 同源注入，测试可指向临时目录）。
    pub(super) repos_root: String,
    /// nexos 本地副本自动拉取节流登记（repo → 上次触发时刻）：同一仓库
    /// [`AUTO_PULL_THROTTLE`] 内最多触发一次后台拉取（10 分钟防抖）。
    pub(super) auto_pull_last: Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// 内存去重缓存容量（最近 1000 条——超出丢最旧，DB 判定兜底）。
pub(super) const FED_SEEN_LIMIT: usize = 1000;

impl LobbyFedEndpoint {
    pub(super) fn new(db: Arc<Mutex<Connection>>, repos_root: &str) -> Self {
        Self {
            db,
            transport: Mutex::new(None),
            seen: Mutex::new(std::collections::VecDeque::new()),
            repos_root: repos_root.to_string(),
            auto_pull_last: Mutex::new(std::collections::HashMap::new()),
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
    pub(super) fn push_federated_seed(&self) {
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
