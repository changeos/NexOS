//! IM 群组 + 直通消息（DM）域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! 群组 CRUD 与成员管理 + DM 会话编排（`dm-` 前缀/成员判定/对端路由
//! `im_dm_peers`）+ DM 定向 WS 推送。对外面经 im/mod.rs 重导出。

use super::*;

/// 群组成员（im_group_members 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub group_id: String,
    pub user_id: String,
    /// `owner` / `admin` / `member`。
    #[serde(default = "default_member_role")]
    pub role: String,
    pub joined_at: String,
}

/// 群组的对外表示（聚合 members + last_activity，前端 `ImGroup` 兼容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    /// 前端用：`group` / `direct`。群组恒为 `group`。
    #[serde(default = "default_kind_group")]
    pub kind: String,
    #[serde(default)]
    pub members: Vec<String>,
    /// 最后一条消息时间（RFC3339，可空）。
    #[serde(default)]
    pub last_activity: Option<String>,
    pub created_at: String,
}

/// DM（直通消息）会话 id 前缀（`im_conversations.id` 以此开头的会话即 DM）。
pub const DM_CONV_PREFIX: &str = "dm-";

/// 会话 id 是否是 DM（直通消息会话）。
pub(super) fn is_dm_conversation(cid: &str) -> bool {
    cid.starts_with(DM_CONV_PREFIX)
}

/// DM 确定性会话 id（纯函数）：`dm-` + sha256(`<小 pubkey>\n<大 pubkey>`)
/// 前 8 字节 hex——**与发起方向无关**（先排序再散列），双方节点各自落库
/// 天然得到同一会话 id。
#[must_use]
pub fn dm_conversation_id(a: &str, b: &str) -> String {
    use sha2::Digest;
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    let digest = sha2::Sha256::digest(format!("{x}\n{y}").as_bytes());
    format!("{DM_CONV_PREFIX}{}", hex::encode(&digest[..8]))
}

/// DM 跨节点消息 id（纯函数）：`dm-msg-` + sha256(载荷要素) 前 12 字节 hex
/// ——发送端与接收端对同一条消息算出同一 id，天然去重（重投/回环只落一份）。
#[must_use]
pub fn dm_message_id(from: &str, to: &str, content: &str, ts: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{from}\n{to}\n{content}\n{ts}").as_bytes());
    format!("{DM_CONV_PREFIX}msg-{}", hex::encode(&digest[..12]))
}

/// DM 会话展示名（纯函数，对称确定性）：`直通 <a 短显> · <b 短显>`——
/// 双端一致；前端侧栏按 members 里的「对方」覆盖展示为对方名字。
pub(super) fn dm_conversation_name(a: &str, b: &str) -> String {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    format!("直通 {} · {}", short_pubkey_label(x), short_pubkey_label(y))
}

/// pubkey 短显标签：`0x1234…cdef`（前 6 后 4 字符；短串原样——UTF-8 边界安全）。
pub(super) fn short_pubkey_label(pubkey: &str) -> String {
    let chars: Vec<char> = pubkey.chars().collect();
    if chars.len() > 14 {
        let head: String = chars[..6].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    } else {
        pubkey.to_string()
    }
}

/// 某 DM 会话的成员列表（im_dm_members，按加入序；空 = 非成员表会话）。
pub(super) fn load_dm_members(conn: &Connection, cid: &str) -> Vec<String> {
    let Ok(mut stmt) = conn
        .prepare("SELECT user_id FROM im_dm_members WHERE conversation_id=? ORDER BY joined_at")
    else {
        return Vec::new();
    };
    let Ok(iter) = stmt.query_map(params![cid], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    iter.filter_map(Result::ok).collect()
}

/// 某身份是否是某 DM 会话成员（收发双方之一）。
pub(super) fn dm_is_member(conn: &Connection, cid: &str, pubkey: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM im_dm_members WHERE conversation_id=? AND user_id=?",
        params![cid, pubkey],
        |_| Ok(true),
    )
    .optional()
    .unwrap_or(Some(false))
    .unwrap_or(false)
}

/// 登记跨节点 DM 对端（ingest 回程路由）：pubkey → 发送方 NodeID（P2P 层
/// 验签真值，非载荷自报）+ 展示名。回复时 POST /im/dm 无需带 to_node。
pub(super) fn upsert_dm_peer(
    conn: &Connection,
    pubkey: &str,
    node_id_hex: &str,
    display_name: &str,
) {
    let name: Option<String> = if display_name.trim().is_empty() {
        None
    } else {
        Some(display_name.chars().take(64).collect())
    };
    let _ = conn.execute(
        "INSERT INTO im_dm_peers (pubkey,node,display_name,last_seen) VALUES (?,?,?,?)
         ON CONFLICT(pubkey) DO UPDATE SET node=excluded.node,
            display_name=COALESCE(excluded.display_name, im_dm_peers.display_name),
            last_seen=excluded.last_seen",
        params![pubkey, node_id_hex, name, now_iso()],
    );
}

/// 查某身份的跨节点 DM 路由（None = 未登记，本节点不曾收过该身份的 DM）。
pub(super) fn lookup_dm_peer_node(conn: &Connection, pubkey: &str) -> Option<String> {
    conn.query_row(
        "SELECT node FROM im_dm_peers WHERE pubkey=?",
        params![pubkey],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

impl ImShared {
    /// 某链上身份是否「在本节点」：大厅在场（GET /lobby 的 Bearer 心跳自动
    /// 加入 im_lobby_members）或当前有 WS 在线订阅（by_user 表）——两者任一
    /// 命中即视为本地身份，DM 走本地投递；否则需要跨节点定向路由。
    pub(super) fn identity_local(&self, conn: &Connection, pubkey: &str) -> bool {
        if lobby_is_member(conn, pubkey) {
            return true;
        }
        self.ws_hub
            .as_ref()
            .is_some_and(|hub| hub.subscriber_count_for(pubkey) > 0)
    }

    /// 确保 DM 会话行 + 双方成员存在（幂等，**调用方持 db 锁**）：已存在 →
    /// 原样返回（保留首次创建的 created_by/时间）；不存在 → 插入确定性
    /// `dm-` 会话（name 对称确定性）+ 双方成员各一行。双端各自调用得到同一行。
    pub(super) fn ensure_dm_conversation(
        &self,
        conn: &Connection,
        cid: &str,
        a: &str,
        b: &str,
    ) -> Conversation {
        let existing: Option<(String, Option<String>, String)> = conn
            .query_row(
                "SELECT name,created_by,created_at FROM im_conversations WHERE id=?",
                params![cid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        let conv = match existing {
            Some((name, created_by, created_at)) => Conversation {
                id: cid.to_string(),
                name,
                is_group: false,
                created_by,
                created_at,
                members: load_dm_members(conn, cid),
            },
            None => {
                let conv = Conversation {
                    id: cid.to_string(),
                    name: dm_conversation_name(a, b),
                    is_group: false,
                    created_by: Some(a.to_string()),
                    created_at: now_iso(),
                    members: vec![a.to_string(), b.to_string()],
                };
                // INSERT OR IGNORE：双端并发创建同一确定性 id 时只落一行
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO im_conversations (id,name,is_group,created_by,created_at) VALUES (?,?,?,?,?)",
                    params![
                        conv.id,
                        conv.name,
                        conv.is_group as i64,
                        conv.created_by.as_deref(),
                        conv.created_at
                    ],
                );
                conv
            }
        };
        let now = now_iso();
        for uid in [a, b] {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO im_dm_members (conversation_id,user_id,joined_at) VALUES (?,?,?)",
                params![cid, uid, now],
            );
        }
        conv
    }

    /// DM 定向 WS 推送（`im_message` 帧 + `send_to_n` 按 pubkey）：只有收件
    /// 列表里的订阅者收到——区别于 [`ImRouteHandler::broadcast_conversation`]
    /// 的全员广播（DM 绝不广播）。收件人离线则空投递（落库已保证回看）。
    pub(super) fn push_dm_ws(&self, cid: &str, msg: &Message, recipients: &[&str]) {
        let Some(hub) = &self.ws_hub else {
            return;
        };
        let frame = WsMessage::ImMessage {
            conversation_id: cid.to_string(),
            message: serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
        };
        for user in recipients {
            hub.send_to_n(user, frame.clone());
        }
    }
}

// ----------------------------------------------------------------------------
// ImLobbyProbe —— 查询端探针：发 im_lobby_query / 收 im_lobby_reply / 30s 缓存
// ----------------------------------------------------------------------------

pub(super) fn insert_group(conn: &Connection, g: &Group) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_groups (id,name,owner,created_at) VALUES (?,?,?,?)",
        params![g.id, g.name, g.owner.as_deref(), g.created_at],
    )?;
    Ok(())
}

pub(super) fn find_group(conn: &Connection, id: &str) -> rusqlite::Result<Option<Group>> {
    let g_row: Option<(String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id,name,owner,created_at FROM im_groups WHERE id=?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((id, name, owner, created_at)) = g_row else {
        return Ok(None);
    };
    let members = load_group_members(conn, &id);
    let last_activity = last_message_time(conn, &id);
    Ok(Some(Group {
        id,
        name,
        owner,
        kind: "group".to_string(),
        members,
        last_activity,
        created_at,
    }))
}

pub(super) fn load_all_groups(conn: &Connection) -> rusqlite::Result<Vec<Group>> {
    // 先收集全部行（避免在迭代中嵌套 prepare/查询同一连接引发借用冲突）。
    let rows: Vec<(String, String, Option<String>, String)> = {
        let mut stmt =
            conn.prepare("SELECT id,name,owner,created_at FROM im_groups ORDER BY created_at")?;
        let iter = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in iter {
            out.push(r?);
        }
        out
    };
    let mut out = Vec::new();
    for (id, name, owner, created_at) in rows {
        let members = load_group_members(conn, &id);
        let last_activity = last_message_time(conn, &id);
        out.push(Group {
            id,
            name,
            owner,
            kind: "group".to_string(),
            members,
            last_activity,
            created_at,
        });
    }
    Ok(out)
}

pub(super) fn insert_group_member(
    conn: &Connection,
    gid: &str,
    uid: &str,
    role: &str,
    joined_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_group_members (group_id,user_id,role,joined_at) VALUES (?,?,?,?)",
        params![gid, uid, role, joined_at],
    )?;
    Ok(())
}

pub(super) fn remove_group_member(
    conn: &Connection,
    gid: &str,
    uid: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM im_group_members WHERE group_id=? AND user_id=?",
        params![gid, uid],
    )
}

pub(super) fn load_group_members(conn: &Connection, gid: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT user_id FROM im_group_members WHERE group_id=? ORDER BY joined_at")
        .ok();
    let Some(ref mut s) = stmt else {
        return Vec::new();
    };
    let rows = s.query_map(params![gid], |r| r.get::<_, String>(0));
    let Ok(iter) = rows else {
        return Vec::new();
    };
    let mut out = Vec::new();
    out.extend(iter.filter_map(Result::ok));
    out
}

// ---- peers CRUD ----
