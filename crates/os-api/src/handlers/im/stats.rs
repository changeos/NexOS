//! IM 状态观测域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! /api/v1/im/status 的 ImStatus 聚合 + Federation 节点（Peer CRUD）+
//! 在线判定 `is_online`（60s 心跳窗口）+ 行数统计。对外面经 im/mod.rs 重导出。

use super::*;

/// 已连接的 Federation 节点（im_peers 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// 形如 `tcp://ip:port`（Federation）/ `ip:port`（兼容前端 `addr`）。
    pub endpoint: String,
    /// `online` / `offline` / `connecting`（前端兼容）。
    #[serde(default = "default_peer_online")]
    pub status: String,
    #[serde(default)]
    pub last_seen: Option<String>,
}

/// IM 服务状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImStatus {
    pub ready: bool,
    pub conversations: usize,
    pub groups: usize,
    pub peers: usize,
    pub messages: usize,
}

/// 在线判定（纯函数）：`last_seen`（RFC3339）距 `now_secs`（unix 秒）< 60s 即在线。
///
/// 解析失败 / 缺失时间一律离线（宁可少算在线，不可误报）。
#[must_use]
pub fn is_online(last_seen: &str, now_secs: i64) -> bool {
    match chrono::DateTime::parse_from_rfc3339(last_seen) {
        Ok(t) => (now_secs - t.timestamp()).abs() < ONLINE_WINDOW_SECS,
        Err(_) => false,
    }
}

pub(super) fn count_rows(conn: &Connection, table: &str) -> usize {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as usize
}

// ---- conversations CRUD ----

pub(super) fn insert_peer(conn: &Connection, p: &Peer) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_peers (id,name,endpoint,status,last_seen) VALUES (?,?,?,?,?)",
        params![
            p.id,
            p.name.as_deref(),
            p.endpoint,
            p.status,
            p.last_seen.as_deref(),
        ],
    )?;
    Ok(())
}

pub(super) fn load_all_peers(conn: &Connection) -> rusqlite::Result<Vec<Peer>> {
    let mut stmt =
        conn.prepare("SELECT id,name,endpoint,status,last_seen FROM im_peers ORDER BY id")?;
    let iter = stmt.query_map([], peer_from_row)?;
    let mut out = Vec::new();
    for p in iter {
        out.push(p?);
    }
    Ok(out)
}

pub(super) fn peer_from_row(row: &rusqlite::Row) -> rusqlite::Result<Peer> {
    Ok(Peer {
        id: row.get(0)?,
        name: row.get(1)?,
        endpoint: row.get(2)?,
        status: row
            .get::<_, Option<String>>(3)?
            .unwrap_or_else(|| "offline".into()),
        last_seen: row.get(4)?,
    })
}

// ---- im_files CRUD（文档传输，2026-08-21）----
