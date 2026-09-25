//! IM 推送通知 webhook 域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! ImWebhook CRUD + 事件匹配（lobby/conversation）+ 异步投递
//! `dispatch_webhooks`（`X-NexOS-Event` 头 + 连续失败 5 次自动停用）。

use super::*;

/// 消息推送 webhook（im_webhooks 行；docs/IM_AGENTS_AND_FILES.md §7）。
///
/// owner = 注册时的 token 反查 pubkey（链上身份）；匹配的消息成功写入后
/// 服务端异步 POST 完整 Message JSON 到 `url`（Header `X-NexOS-Event`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImWebhook {
    pub id: String,
    /// agent 的 HTTP 接收端点（POST 目标，http/https）。
    pub url: String,
    /// 注册者 pubkey（token 反查，自报值一律忽略）。
    pub owner_pubkey: String,
    /// 订阅事件：`lobby`（大厅新消息）/ `conversation`（会话新消息），
    /// JSON 列持久化。缺省（注册时不传）= 双开。
    #[serde(default = "default_webhook_events_all")]
    pub events: Vec<String>,
    /// 绑定单个会话（仅 conversation 事件生效；None=全部会话）。
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// `active` / `disabled`（连败 ≥5 次自动注销）。
    #[serde(default = "default_webhook_active")]
    pub status: String,
    /// 连败计数（成功投递清零）。
    #[serde(default)]
    pub fail_count: u32,
    /// 最近一次投递时刻（RFC3339，可空）。
    #[serde(default)]
    pub last_fired_at: Option<String>,
    /// 最近一次投递错误（含自动注销原因；成功投递清空）。
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: String,
}

/// 订阅事件名：大厅新消息（conversation_id == lobby 的消息）。
pub const NOTIFY_EVENT_LOBBY: &str = "lobby";
/// 订阅事件名：会话新消息（大厅以外的全部会话/群组消息）。
pub const NOTIFY_EVENT_CONVERSATION: &str = "conversation";
/// webhook 注册/管理路径前缀（错误消息提示用）。
/// 投递时的自定义事件头：`lobby_message` / `conversation_message`。
pub(super) const WEBHOOK_EVENT_HEADER: &str = "X-NexOS-Event";
/// 单次投递超时（超时计一次失败）。
pub(super) const WEBHOOK_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// 连败 ≥ 该值自动注销（status=disabled + last_error 记录原因）。
pub(super) const WEBHOOK_MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// webhook url 长度上限（字符；防把超大串塞进 DB/每次投递）。
pub(super) const WEBHOOK_URL_MAX_CHARS: usize = 2048;
/// 注册表 status 取值：活跃（参与派发）。
pub(super) const WEBHOOK_STATUS_ACTIVE: &str = "active";
/// 注册表 status 取值：连败自动注销（不参与派发；重新注册即恢复）。
pub(super) const WEBHOOK_STATUS_DISABLED: &str = "disabled";

pub(super) fn default_webhook_events_all() -> Vec<String> {
    vec![
        NOTIFY_EVENT_LOBBY.to_string(),
        NOTIFY_EVENT_CONVERSATION.to_string(),
    ]
}

pub(super) fn default_webhook_active() -> String {
    WEBHOOK_STATUS_ACTIVE.to_string()
}

/// 校验 webhook url（纯函数）：`http://`/`https://` scheme + 非空主机段 +
/// 无空白字符 + ≤2048 字符（防把超大串/畸形串塞进 DB 和每次投递）。
#[must_use]
pub fn is_valid_webhook_url(url: &str) -> bool {
    let u = url.trim();
    let Some((scheme, rest)) = u.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    !rest.is_empty()
        && !rest.chars().any(char::is_whitespace)
        && u.chars().count() <= WEBHOOK_URL_MAX_CHARS
}

/// 归一注册 events（纯函数）：白名单过滤 + 去重保序。
/// 返回 None = 传入里有非法值或全空（调用方回 400）；Some(空) 不会出现。
pub(super) fn normalize_webhook_events(events: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
        let t = e.trim();
        if t != NOTIFY_EVENT_LOBBY && t != NOTIFY_EVENT_CONVERSATION {
            return None;
        }
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// 派发事件名（纯函数）：大厅消息 → `lobby_message`，其余 → `conversation_message`。
#[must_use]
pub fn webhook_event_name(conversation_id: &str) -> &'static str {
    if conversation_id == LOBBY_ID {
        "lobby_message"
    } else {
        "conversation_message"
    }
}

/// webhook 是否匹配该消息（纯函数，派发过滤 + 测试共用）：
/// - status 须 active（自动注销的不再派发）；
/// - 事件过滤：大厅消息须订阅 `lobby`，会话消息须订阅 `conversation`；
/// - conversation 事件可绑定单个会话（None=全部会话）；lobby 事件不与
///   conversation_id 绑定（大厅是单一公共频道，绑了也无意义）。
#[must_use]
pub fn webhook_matches(hook: &ImWebhook, msg: &Message) -> bool {
    if hook.status != WEBHOOK_STATUS_ACTIVE {
        return false;
    }
    let is_lobby = msg.conversation_id == LOBBY_ID;
    let event_hit = if is_lobby {
        hook.events.iter().any(|e| e == NOTIFY_EVENT_LOBBY)
    } else {
        hook.events.iter().any(|e| e == NOTIFY_EVENT_CONVERSATION)
    };
    if !event_hit {
        return false;
    }
    if is_lobby {
        return true;
    }
    hook.conversation_id
        .as_deref()
        .map_or(true, |cid| cid == msg.conversation_id)
}

impl ImShared {
    /// 消息成功写入后：对所有匹配的注册 webhook spawn 异步 POST。
    ///
    /// **完全不阻塞消息路径**——同步段只做一次短锁查表，逐 webhook
    /// `tokio::spawn`（单 hook 失败/超时互不影响，也不影响 HTTP 响应）。
    /// body = 完整 Message JSON（不含任何 token），Header
    /// `X-NexOS-Event: lobby_message|conversation_message`，超时 5s；
    /// 成功清零连败并记 last_fired_at，失败连败 +1，连败 ≥5 自动注销。
    pub(super) async fn dispatch_webhooks(self: &Arc<Self>, msg: &Message) {
        let hooks: Vec<ImWebhook> = {
            let shared = Arc::clone(self);
            let msg_for_db = msg.clone();
            im_db_call(shared, move |conn| {
                load_all_webhooks(conn)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|hook| webhook_matches(hook, &msg_for_db))
                    .collect()
            })
            .await
        };
        if hooks.is_empty() {
            return;
        }
        let event = webhook_event_name(&msg.conversation_id);
        let payload = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
        for hook in hooks {
            let shared = self.clone();
            let payload = payload.clone();
            tokio::spawn(async move {
                let outcome: Result<(), String> = async {
                    let resp = AGENT_HTTP
                        .post(&hook.url)
                        .timeout(WEBHOOK_HTTP_TIMEOUT)
                        .header(WEBHOOK_EVENT_HEADER, event)
                        .json(&payload)
                        .send()
                        .await
                        .map_err(|e| format!("投递失败（超时/不可达）: {e}"))?;
                    if resp.status().is_success() {
                        Ok(())
                    } else {
                        Err(format!("接收端返回 HTTP {}", resp.status().as_u16()))
                    }
                }
                .await;
                let shared_for_db = Arc::clone(&shared);
                let outcome_for_db = outcome;
                im_db_call(shared_for_db, move |conn| match outcome_for_db {
                    Ok(()) => {
                        let _ = webhook_record_success(conn, &hook.id);
                    }
                    Err(err) => {
                        let _ = webhook_record_failure(
                            conn,
                            &hook.id,
                            &err,
                            WEBHOOK_MAX_CONSECUTIVE_FAILURES,
                        );
                    }
                })
                .await;
            });
        }
    }
}

/// im_webhooks 查询列（与 [`webhook_from_row`] 的索引一一对应）。
pub(super) const WEBHOOK_COLS: &str =
    "id,url,owner_pubkey,events,conversation_id,status,fail_count,last_fired_at,last_error,created_at";

pub(super) fn insert_webhook(conn: &Connection, w: &ImWebhook) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_webhooks
         (id,url,owner_pubkey,events,conversation_id,status,fail_count,last_fired_at,last_error,created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![
            w.id,
            w.url,
            w.owner_pubkey,
            serde_json::to_string(&w.events).unwrap_or_else(|_| "[]".into()),
            w.conversation_id.as_deref(),
            w.status,
            w.fail_count as i64,
            w.last_fired_at.as_deref(),
            w.last_error.as_deref(),
            w.created_at
        ],
    )?;
    Ok(())
}

pub(super) fn load_all_webhooks(conn: &Connection) -> rusqlite::Result<Vec<ImWebhook>> {
    let sql = format!("SELECT {WEBHOOK_COLS} FROM im_webhooks ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map([], webhook_from_row)?;
    let mut out = Vec::new();
    for w in iter {
        out.push(w?);
    }
    Ok(out)
}

/// 某 owner 的全部 webhook（list 端点 owner 过滤）。
pub(super) fn load_webhooks_by_owner(
    conn: &Connection,
    owner: &str,
) -> rusqlite::Result<Vec<ImWebhook>> {
    let sql =
        format!("SELECT {WEBHOOK_COLS} FROM im_webhooks WHERE owner_pubkey=? ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![owner], webhook_from_row)?;
    let mut out = Vec::new();
    for w in iter {
        out.push(w?);
    }
    Ok(out)
}

pub(super) fn find_webhook(conn: &Connection, id: &str) -> rusqlite::Result<Option<ImWebhook>> {
    let sql = format!("SELECT {WEBHOOK_COLS} FROM im_webhooks WHERE id=?");
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id], webhook_from_row).optional()
}

pub(super) fn delete_webhook(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM im_webhooks WHERE id=?", params![id])
}

/// 投递成功：连败清零 + 记 last_fired_at + 清 last_error。
pub(super) fn webhook_record_success(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE im_webhooks SET fail_count=0, last_fired_at=?, last_error=NULL WHERE id=?",
        params![now_iso(), id],
    )?;
    Ok(())
}

/// 投递失败：连败 +1 + 记 last_error；连败 ≥ max_fails 自动注销
/// （status=disabled，last_error 换成注销原因——注册表保留行供 owner 审计，
/// 重新注册同 url 即恢复）。
pub(super) fn webhook_record_failure(
    conn: &Connection,
    id: &str,
    err: &str,
    max_fails: u32,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE im_webhooks SET fail_count=fail_count+1, last_error=?1, last_fired_at=?2 WHERE id=?3",
        params![err, now_iso(), id],
    )?;
    let fails: Option<i64> = conn
        .query_row(
            "SELECT fail_count FROM im_webhooks WHERE id=?",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if fails.is_some_and(|f| f >= max_fails as i64) {
        conn.execute(
            "UPDATE im_webhooks SET status=?1, last_error=?2 WHERE id=?3",
            params![
                WEBHOOK_STATUS_DISABLED,
                format!("连败 {max_fails} 次自动注销（最近错误: {err}）"),
                id
            ],
        )?;
    }
    Ok(())
}

pub(super) fn webhook_from_row(row: &rusqlite::Row) -> rusqlite::Result<ImWebhook> {
    let events_json: String = row
        .get::<_, Option<String>>(3)?
        .unwrap_or_else(|| "[]".into());
    let events: Vec<String> = serde_json::from_str(&events_json).unwrap_or_default();
    Ok(ImWebhook {
        id: row.get(0)?,
        url: row.get(1)?,
        owner_pubkey: row.get(2)?,
        events,
        conversation_id: row.get(4)?,
        status: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(|| WEBHOOK_STATUS_ACTIVE.into()),
        fail_count: row.get::<_, i64>(6)?.max(0) as u32,
        last_fired_at: row.get(7)?,
        last_error: row.get(8)?,
        created_at: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
    })
}

// ---- lobby（大厅公共频道）----
