//! IM 会话域（2026-09-25 大文件拆分批，原 im.rs 单文件按域拆分，纯搬运零
//! 行为变化）：会话/消息数据层（CRUD + 历史/增量分页 `load_messages_after`/
//! `load_recent_*`）+ 大厅成员/信息 + demo seed + @mention 解析与内置助手
//! NexOS助手（`maybe_spawn_assistant`）。对外面经 im/mod.rs 重导出。

use super::*;

/// 大厅成员（im_lobby_members 行 + 派生 online 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyMember {
    pub user_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// 最近一次心跳（RFC3339；60s 内活跃 = 在线）。
    #[serde(default)]
    pub last_seen: Option<String>,
    #[serde(default)]
    pub joined_at: Option<String>,
    /// 派生字段：last_seen 距今 < 60s。
    #[serde(default)]
    pub online: bool,
}

/// 大厅信息（GET /api/v1/im/lobby 响应体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyInfo {
    /// 恒为 "lobby"。
    pub id: String,
    pub name: String,
    /// 成员总数。
    pub member_count: usize,
    /// 在线成员数（60s 心跳窗口）。
    pub online_count: usize,
    /// 最近一条消息（可空）。
    #[serde(default)]
    pub last_message: Option<Message>,
}

// ----------------------------------------------------------------------------
// 区块链认证：共享内核 os_common::chain_auth（设计 §2；泛化见
// docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C——NexHub 同款挑战-签名模式）
// ----------------------------------------------------------------------------

/// 构造"欢迎加入大厅"系统消息（纯函数，conversation_id 恒为 lobby）。
#[must_use]
pub fn build_welcome_message(user: &str) -> Message {
    Message {
        id: new_uuid(),
        conversation_id: LOBBY_ID.to_string(),
        sender_id: "system".to_string(),
        sender_name: Some("NexOS".to_string()),
        content: format!("欢迎 {user} 加入 NexOS 大厅"),
        msg_type: "system".to_string(),
        file_url: None,
        reply_to: None,
        created_at: now_iso(),
        read_by: Vec::new(),
        sender_kind: "human".to_string(),
        mentions: Vec::new(),
        attachment: None,
    }
}

pub(super) fn default_sender_kind_human() -> String {
    "human".to_string()
}

/// 归一 sender_kind（纯函数）：`agent` / `system` 白名单放行，其余（缺失/
/// 垃圾值）一律 `human`——展示层自声明语义的兜底（见 Message 字段注释；
/// `system` 为 2026-08-23 身份冲突警告等本地系统消息保留）。
pub(super) fn normalize_sender_kind(kind: Option<&str>) -> String {
    match kind.map(str::trim) {
        Some("agent") => "agent".to_string(),
        Some("system") => "system".to_string(),
        _ => "human".to_string(),
    }
}

// ----------------------------------------------------------------------------
// @mention 解析 + 内置助手 NexOS助手（2026-08-21 agent 批次）
// ----------------------------------------------------------------------------

/// 内置 agent 名字：@提及该名字触发内置助手（常量对外——前端高亮/外部
/// agent 避免撞名用）。
pub const NEXOS_ASSISTANT: &str = "NexOS助手";
/// 助手合成 sender_id（非链上身份——无私钥，仅服务端代发；归因恒可信）。
pub(super) const ASSISTANT_SENDER_ID: &str = "agent:nexos-assistant";
/// 助手回复正文字符上限（"（AI 生成）"后缀不计入）。
pub(super) const ASSISTANT_REPLY_MAX_CHARS: usize = 800;
/// 助手回复固定后缀（AI 生成标识，前端/审计用）。
pub(super) const ASSISTANT_SUFFIX: &str = "（AI 生成）";
/// LLM 不可达/出错时的固定降级话术。
pub(super) const ASSISTANT_FALLBACK_TEXT: &str = "抱歉，本地推理服务暂时不可用，请稍后再试。";
/// 默认防风暴去抖窗口：同会话该窗口内多条 @ 只响应最后一条。
pub(super) const ASSISTANT_STORM_WINDOW: Duration = Duration::from_secs(3);
/// 默认推理端点（OpenAI 兼容 chat/completions；env `NEXOS_IM_AGENT_LLM_URL` 覆盖）。
pub(super) const ASSISTANT_LLM_URL_DEFAULT: &str = "http://127.0.0.1:8000/v1/chat/completions";
/// 默认模型名（env `NEXOS_IM_AGENT_MODEL` 覆盖）。
pub(super) const ASSISTANT_LLM_MODEL_DEFAULT: &str = "qwen3.5-9b";
/// 助手推理请求超时（与 llm.rs chat 通道同款 60s）。
pub(super) const ASSISTANT_LLM_TIMEOUT: Duration = Duration::from_secs(60);
/// 防风暴代次表条目 TTL（顺手清理，防无界增长）。
pub(super) const ASSISTANT_GEN_TTL: Duration = Duration::from_secs(3600);
/// @ 名字字符集：CJK 基本区（一-龥 = U+4E00..=U+9FA5）+ ASCII 字母数字 + `_`/`-`。
pub(super) fn is_mention_char(c: char) -> bool {
    ('\u{4E00}'..='\u{9FA5}').contains(&c) || c.is_ascii_alphanumeric() || c == '_' || c == '-'
}
/// @ 名字长度上限（字符）。
pub(super) const MENTION_MAX_CHARS: usize = 42;

/// 解析 content 中的 @ 提及（纯函数）：每个 `@` 后跟 1..=42 个合法名字字符的
/// 连续段即一次提及（超 42 字符截断到 42；`@` 后跟非法字符不算——如邮箱
/// 前缀后的 `@example.com` 会截出 `example`，属既定语义）。去重保序。
///
/// 中文/英文/多 @ 均可：`"你好 @NexOS助手 请看 @alice 的稿"` →
/// `["NexOS助手", "alice"]`。
#[must_use]
pub fn parse_mentions(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        let mut name = String::new();
        let mut j = i + 1;
        while j < chars.len()
            && is_mention_char(chars[j])
            && name.chars().count() < MENTION_MAX_CHARS
        {
            name.push(chars[j]);
            j += 1;
        }
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        i = if j > i + 1 { j } else { i + 1 };
    }
    out
}

/// 剥掉 content 中已解析的 `@名字` 片段（纯函数，助手 prompt 用——"除 @ 外文本"）。
/// 每个名字只剥首个匹配；结果 trim。全部剥完为空（用户只发了 @）时由调用方
/// 回退原文。
#[must_use]
pub fn strip_mentions(content: &str, names: &[String]) -> String {
    let mut out = content.to_string();
    for n in names {
        out = out.replacen(&format!("@{n}"), "", 1);
    }
    out.trim().to_string()
}

/// 按**字符**安全截断（UTF-8 边界安全）：超过 max 字符取前 max 个。
#[must_use]
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// 助手推理用共享 HTTP 客户端（进程级连接池复用，llm.rs 同款）。
pub(super) static AGENT_HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("构建助手推理 reqwest Client 失败")
});

/// 调本地推理（OpenAI 兼容 `POST /v1/chat/completions`，llm.rs chat 通道同款）。
///
/// 失败（连接拒绝/HTTP 错误/响应缺字段）一律 `Err`——调用方降级到固定话术，
/// 绝不 panic、绝不阻塞发消息请求（本函数只在 spawn 的回复任务里调用）。
async fn assistant_chat_complete(url: &str, model: &str, prompt: &str) -> Result<String, String> {
    let payload = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "你是 NexOS IM 内置的「NexOS助手」，用简洁中文回答用户问题。"},
            {"role": "user", "content": prompt},
        ],
        "max_tokens": 4096,  // 思考模型（qwen3.5 reasoning）推理段耗 token，512 会 finish=length 致 content=null（演示agent实测 F3）
        "temperature": 0.7,
    });
    // vLLM 实例启用 --api-key 时（NEXOS_VLLM_API_KEY 透传），助手直连同样要带
    let mut req = AGENT_HTTP.post(url).timeout(ASSISTANT_LLM_TIMEOUT);
    if let Ok(k) = std::env::var("NEXOS_VLLM_API_KEY") {
        if !k.trim().is_empty() {
            req = req.bearer_auth(k);
        }
    }
    let resp = req
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("推理请求发送失败（本地 LLM 未运行？）: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("推理请求失败（HTTP 错误）: {e}"))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析推理响应失败: {e}"))?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| "推理响应缺少 choices[0].message.content".to_string())
}

/// 该会话的最新触发代次是否仍是 `gen`（防风暴提交闸门）。
pub(super) fn assistant_is_latest(shared: &ImShared, cid: &str, gen: u64) -> bool {
    shared
        .assistant_gen
        .lock()
        .expect("assistant_gen poisoned")
        .get(cid)
        .is_some_and(|(g, _)| *g == gen)
}

/// 大厅 seed：im_lobby 为空时插入固定大厅行 + 1 条系统欢迎消息。
///
/// 欢迎消息落在 im_messages（conversation_id='lobby'），复用现有消息读写路径。
pub(super) fn seed_lobby_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    let lobby_count: i64 = conn.query_row("SELECT COUNT(*) FROM im_lobby", [], |r| r.get(0))?;
    if lobby_count == 0 {
        conn.execute(
            "INSERT OR REPLACE INTO im_lobby (id,name,created_at) VALUES (?,?,?)",
            params![LOBBY_ID, "大厅", now_iso()],
        )?;
        let welcome = Message {
            id: "msg-lobby-seed".to_string(),
            conversation_id: LOBBY_ID.to_string(),
            sender_id: "system".to_string(),
            sender_name: Some("NexOS".to_string()),
            content: "欢迎来到 NexOS 大厅 — 连接每一个超级个体".to_string(),
            msg_type: "system".to_string(),
            file_url: None,
            reply_to: None,
            created_at: now_iso(),
            read_by: Vec::new(),
            sender_kind: "human".to_string(),
            mentions: Vec::new(),
            attachment: None,
        };
        insert_message(conn, &welcome)?;
    }
    Ok(())
}

/// 首次空表时 seed demo 数据（2 对话 + 5 消息 + 1 群组）。已存在数据则跳过。
///
/// 注意：大厅欢迎消息在 [`create_schema`] 里先落库（也写 im_messages），
/// 故这里不能以 im_messages 计数判空——只看对话/群组是否为空。
pub(super) fn seed_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    let conv_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM im_conversations", [], |r| r.get(0))?;
    let group_count: i64 = conn.query_row("SELECT COUNT(*) FROM im_groups", [], |r| r.get(0))?;
    if conv_count == 0 && group_count == 0 {
        seed_demo(conn)?;
    }
    Ok(())
}

/// seed 具体数据：2 对话 + 1 群组（3 成员）+ 5 条消息。
pub(super) fn seed_demo(conn: &Connection) -> rusqlite::Result<()> {
    let now = now_iso();
    // 2 对话
    let c1 = Conversation {
        id: "conv-general".into(),
        name: "通用聊天".into(),
        is_group: false,
        created_by: Some("alice".into()),
        created_at: now.clone(),
        members: Vec::new(),
    };
    let c2 = Conversation {
        id: "conv-dev".into(),
        name: "开发讨论".into(),
        is_group: false,
        created_by: Some("bob".into()),
        created_at: now.clone(),
        members: Vec::new(),
    };
    insert_conversation(conn, &c1)?;
    insert_conversation(conn, &c2)?;
    // 1 群组（owner alice + 成员 bob/carol）
    let g = Group {
        id: "group-dev-team".into(),
        name: "Dev Team".into(),
        owner: Some("alice".into()),
        kind: "group".into(),
        members: vec!["alice".into(), "bob".into(), "carol".into()],
        last_activity: None,
        created_at: now.clone(),
    };
    insert_group(conn, &g)?;
    for (uid, role) in [("alice", "owner"), ("bob", "member"), ("carol", "member")] {
        insert_group_member(conn, &g.id, uid, role, &now)?;
    }
    // 5 条消息（conv-general×2，conv-dev×1，group-dev-team×2）
    let msgs = [
        Message {
            id: "msg-1".into(),
            conversation_id: "conv-general".into(),
            sender_id: "alice".into(),
            sender_name: Some("Alice".into()),
            content: "大家好！".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T09:00:00+08:00".into(),
            read_by: vec!["alice".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-2".into(),
            conversation_id: "conv-general".into(),
            sender_id: "bob".into(),
            sender_name: Some("Bob".into()),
            content: "hi Alice".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: Some("msg-1".into()),
            created_at: "2026-01-01T09:01:00+08:00".into(),
            read_by: vec!["bob".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-3".into(),
            conversation_id: "conv-dev".into(),
            sender_id: "carol".into(),
            sender_name: Some("Carol".into()),
            content: "看看这份设计文档".into(),
            msg_type: "file".into(),
            file_url: Some("/tank/docs/design.pdf".into()),
            reply_to: None,
            created_at: "2026-01-01T10:00:00+08:00".into(),
            read_by: vec!["carol".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-4".into(),
            conversation_id: "group-dev-team".into(),
            sender_id: "alice".into(),
            sender_name: Some("Alice".into()),
            content: "今晚发版".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T11:00:00+08:00".into(),
            read_by: vec!["alice".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-5".into(),
            conversation_id: "group-dev-team".into(),
            sender_id: "system".into(),
            sender_name: Some("System".into()),
            content: "Carol 加入了群组".into(),
            msg_type: "system".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T11:05:00+08:00".into(),
            read_by: Vec::new(),
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
    ];
    for m in &msgs {
        insert_message(conn, m)?;
    }
    Ok(())
}

// ---- 表行数统计 ----

pub(super) fn insert_conversation(conn: &Connection, c: &Conversation) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_conversations (id,name,is_group,created_by,created_at) VALUES (?,?,?,?,?)",
        params![c.id, c.name, c.is_group as i64, c.created_by.as_deref(), c.created_at],
    )?;
    Ok(())
}

pub(super) fn load_all_conversations(conn: &Connection) -> rusqlite::Result<Vec<Conversation>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,is_group,created_by,created_at FROM im_conversations ORDER BY created_at",
    )?;
    let iter = stmt.query_map([], conversation_from_row)?;
    let mut out = Vec::new();
    for c in iter {
        let mut conv = c?;
        // DM 会话附带成员（双方 pubkey，前端据此识别「对方」并路由私信）
        if is_dm_conversation(&conv.id) {
            conv.members = load_dm_members(conn, &conv.id);
        }
        out.push(conv);
    }
    Ok(out)
}

pub(super) fn conversation_from_row(row: &rusqlite::Row) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: row.get(0)?,
        name: row.get(1)?,
        is_group: row.get::<_, i64>(2)? != 0,
        created_by: row.get(3)?,
        created_at: row.get(4)?,
        members: Vec::new(), // DM 成员由 load_all_conversations 按需回填
    })
}

// ---- messages CRUD ----

/// im_messages 查询列（与 [`message_from_row`] 的索引一一对应；2026-08-21
/// 起含 sender_kind/mentions/attachment 三列——存量库缺列由 create_schema
/// 的幂等 ALTER 补齐，故 SELECT 恒可带全列）。
pub(super) const MSG_COLS: &str =
    "id,conversation_id,sender_id,sender_name,content,msg_type,file_url,reply_to,created_at,read_by,sender_kind,mentions,attachment";

pub(super) fn insert_message(conn: &Connection, m: &Message) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_messages
         (id,conversation_id,sender_id,sender_name,content,msg_type,file_url,reply_to,created_at,read_by,sender_kind,mentions,attachment)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            m.id,
            m.conversation_id,
            m.sender_id,
            m.sender_name.as_deref(),
            m.content,
            m.msg_type,
            m.file_url.as_deref(),
            m.reply_to.as_deref(),
            m.created_at,
            serde_json::to_string(&m.read_by).unwrap_or_else(|_| "[]".into()),
            normalize_sender_kind(Some(&m.sender_kind)),
            serde_json::to_string(&m.mentions).unwrap_or_else(|_| "[]".into()),
            m.attachment
                .as_ref()
                .map(|a| serde_json::to_string(a).unwrap_or_default()),
        ],
    )?;
    Ok(())
}

pub(super) fn load_messages_by_conversation(
    conn: &Connection,
    cid: &str,
) -> rusqlite::Result<Vec<Message>> {
    let sql =
        format!("SELECT {MSG_COLS} FROM im_messages WHERE conversation_id=? ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![cid], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

pub(super) fn load_all_messages(conn: &Connection) -> rusqlite::Result<Vec<Message>> {
    let sql = format!("SELECT {MSG_COLS} FROM im_messages ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map([], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

pub(super) fn find_message(conn: &Connection, id: &str) -> rusqlite::Result<Option<Message>> {
    let sql = format!("SELECT {MSG_COLS} FROM im_messages WHERE id=?");
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id], message_from_row).optional()
}

pub(super) fn update_message_read_by(conn: &Connection, m: &Message) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE im_messages SET read_by=? WHERE id=?",
        params![
            serde_json::to_string(&m.read_by).unwrap_or_else(|_| "[]".into()),
            m.id
        ],
    )?;
    Ok(())
}

pub(super) fn message_from_row(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    let read_by_json: String = row.get(9)?;
    let read_by: Vec<String> = serde_json::from_str(&read_by_json).unwrap_or_default();
    let mentions_json: String = row
        .get::<_, Option<String>>(11)?
        .unwrap_or_else(|| "[]".into());
    let mentions: Vec<String> = serde_json::from_str(&mentions_json).unwrap_or_default();
    let attachment = row
        .get::<_, Option<String>>(12)?
        .and_then(|s| serde_json::from_str(&s).ok());
    Ok(Message {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        sender_id: row.get(2)?,
        sender_name: row.get(3)?,
        content: row.get(4)?,
        msg_type: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(|| "text".into()),
        file_url: row.get(6)?,
        reply_to: row.get(7)?,
        created_at: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
        read_by,
        sender_kind: row
            .get::<_, Option<String>>(10)?
            .unwrap_or_else(|| "human".into()),
        mentions,
        attachment,
    })
}

/// 统计某对话中某用户未读消息数（read_by 不含该用户）。
pub(super) fn count_unread(conn: &Connection, cid: &str, user: &str) -> usize {
    let all = load_messages_by_conversation(conn, cid).unwrap_or_default();
    all.iter()
        .filter(|m| !m.read_by.contains(&user.to_string()))
        .count()
}

/// LIKE 通配符转义（`\` / `%` / `_` 前插 `\`，配合 SQL 的 `ESCAPE '\'`）：
/// 用户输入的 `%`/`_` 按**字面字符**匹配，不再当通配符（搜 "100%" 不会命中
/// "100" 开头的一切）。返回值已含两侧 `%` 包裹。
pub(super) fn like_pattern_literal(q: &str) -> String {
    let mut escaped = String::with_capacity(q.len() + 2);
    for c in q.chars() {
        if c == '%' || c == '_' || c == '\\' {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("%{escaped}%")
}

/// 搜索某会话消息（content LIKE %q%，通配符字面转义），created_at 倒序
/// （最新在前），至多 limit 条。
pub(super) fn search_messages(
    conn: &Connection,
    cid: &str,
    q: &str,
    limit: i64,
) -> rusqlite::Result<Vec<Message>> {
    let like = like_pattern_literal(q);
    let sql = format!(
        "SELECT {MSG_COLS} FROM im_messages
         WHERE conversation_id=?1 AND content LIKE ?2 ESCAPE '\\'
         ORDER BY created_at DESC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![cid, like, limit], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

// ---- groups CRUD ----

/// 某 conversation/group 的最后一条消息时间（None=无消息）。
pub(super) fn last_message_time(conn: &Connection, cid: &str) -> Option<String> {
    conn.query_row(
        "SELECT created_at FROM im_messages WHERE conversation_id=? ORDER BY created_at DESC LIMIT 1",
        params![cid],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

// ---- group_members CRUD ----

/// upsert 大厅成员：新用户插入（joined_at/last_seen=now）返回 true；老用户仅刷新
/// last_seen/display_name（心跳）返回 false。全员不可退出大厅——无删除路径。
pub(super) fn upsert_lobby_member(conn: &Connection, user_id: &str, display_name: &str) -> bool {
    let now = now_iso();
    let existing: bool = conn
        .query_row(
            "SELECT 1 FROM im_lobby_members WHERE user_id=?",
            params![user_id],
            |_| Ok(true),
        )
        .optional()
        .unwrap_or(Some(false))
        .unwrap_or(false);
    let res = if existing {
        conn.execute(
            "UPDATE im_lobby_members SET last_seen=?, display_name=? WHERE user_id=?",
            params![now, display_name, user_id],
        )
    } else {
        conn.execute(
            "INSERT INTO im_lobby_members (user_id,display_name,last_seen,joined_at) VALUES (?,?,?,?)",
            params![user_id, display_name, now, now],
        )
    };
    if res.is_err() {
        return false;
    }
    !existing
}

/// 某用户是否已是大厅成员。
pub(super) fn lobby_is_member(conn: &Connection, user_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM im_lobby_members WHERE user_id=?",
        params![user_id],
        |_| Ok(true),
    )
    .optional()
    .unwrap_or(Some(false))
    .unwrap_or(false)
}

/// 全量大厅成员（按加入时间排序；online 由 is_online 派生）。
pub(super) fn load_lobby_members(conn: &Connection) -> rusqlite::Result<Vec<LobbyMember>> {
    let mut stmt = conn.prepare(
        "SELECT user_id,display_name,last_seen,joined_at FROM im_lobby_members ORDER BY joined_at",
    )?;
    let now = chrono::Local::now().timestamp();
    let iter = stmt.query_map([], |row| {
        let last_seen: Option<String> = row.get(2)?;
        Ok(LobbyMember {
            user_id: row.get(0)?,
            display_name: row.get(1)?,
            online: last_seen
                .as_deref()
                .map(|t| is_online(t, now))
                .unwrap_or(false),
            last_seen,
            joined_at: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

/// 按 after_id 增量拉会话消息（离线补拉核心查询）：
/// 返回 conversation 下 **插入序（rowid）严格大于 after_id** 的消息，升序，
/// 至多 limit 条。after_id 为 None / 空串 / 该会话中不存在的 id → 从头升序取
/// （COALESCE 回退 rowid 0，自然全量）。消息 id 是随机 uuid，不可字符串排序，
/// 故以 rowid（= 写入顺序，单调递增）作为全序基准。
pub(super) fn load_messages_after(
    conn: &Connection,
    cid: &str,
    after_id: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Vec<Message>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM im_messages
         WHERE conversation_id=?1
           AND rowid > COALESCE((SELECT rowid FROM im_messages WHERE id=?2 AND conversation_id=?1), 0)
         ORDER BY rowid ASC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(
        params![cid, after_id.unwrap_or(""), limit],
        message_from_row,
    )?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

/// 大厅/联邦大厅最近 limit 条消息（时间正序返回，conversation_id 由调用方给定）。
///
/// 带 after_id 时切换为增量语义：严格晚于该消息的该会话消息升序取 limit 条
/// （复用 [`load_messages_after`]，与 `/api/v1/im/messages` 端点同语义）；
/// 不带 after_id 维持旧行为（DESC 取最近 limit 条再反转为正序）。
pub(super) fn load_recent_conversation_messages(
    conn: &Connection,
    cid: &str,
    limit: usize,
    after_id: Option<&str>,
) -> rusqlite::Result<Vec<Message>> {
    if after_id.is_some_and(|s| !s.is_empty()) {
        return load_messages_after(conn, cid, after_id, limit as i64);
    }
    let sql = format!(
        "SELECT {MSG_COLS} FROM im_messages WHERE conversation_id=? ORDER BY created_at DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![cid, limit as i64], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    out.reverse(); // DESC 取最近 N 条 → 反转为时间正序
    Ok(out)
}

/// 我的大厅最近 limit 条消息（[`load_recent_conversation_messages`] 的 lobby 特化）。
pub(super) fn load_recent_lobby_messages(
    conn: &Connection,
    limit: usize,
    after_id: Option<&str>,
) -> rusqlite::Result<Vec<Message>> {
    load_recent_conversation_messages(conn, LOBBY_ID, limit, after_id)
}

/// 大厅最后一条消息（无消息返回 None）。
pub(super) fn last_lobby_message(conn: &Connection) -> Option<Message> {
    load_recent_lobby_messages(conn, 1, None)
        .ok()?
        .into_iter()
        .next()
}

/// 大厅信息聚合（成员数/在线数/最近消息）。
pub(super) fn lobby_info(conn: &Connection) -> LobbyInfo {
    let members = load_lobby_members(conn).unwrap_or_default();
    let online_count = members.iter().filter(|m| m.online).count();
    let name: String = conn
        .query_row(
            "SELECT name FROM im_lobby WHERE id=?",
            params![LOBBY_ID],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "大厅".to_string());
    LobbyInfo {
        id: LOBBY_ID.to_string(),
        name,
        member_count: members.len(),
        online_count,
        last_message: last_lobby_message(conn),
    }
}

/// 联邦大厅信息聚合（GET /api/v1/im/fed-lobby 响应内核）。
///
/// 跨节点共享频道没有独立成员表——在场/在线沿用本节点大厅成员表（每个
/// 节点的本地用户即该节点在联邦频道的参与者）；最近消息取 fed-lobby 会话。
pub(super) fn fed_lobby_info(conn: &Connection) -> LobbyInfo {
    let members = load_lobby_members(conn).unwrap_or_default();
    let online_count = members.iter().filter(|m| m.online).count();
    LobbyInfo {
        id: FED_LOBBY_ID.to_string(),
        name: "联邦大厅".to_string(),
        member_count: members.len(),
        online_count,
        last_message: load_recent_conversation_messages(conn, FED_LOBBY_ID, 1, None)
            .ok()
            .and_then(|mut v| v.pop()),
    }
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

impl ImRouteHandler {
    /// 多条 @ 只有最后一条得到回复**。
    pub(super) fn maybe_spawn_assistant(&self, trigger: &Message) {
        if trigger.sender_kind == "agent" {
            return; // 助手/外部 agent 消息不再触发（防风暴第二道闸）
        }
        if !trigger.mentions.iter().any(|m| m == NEXOS_ASSISTANT) {
            return;
        }
        let shared = self.shared.clone();
        let url_configured = self.agent_llm_url_configured();
        let model = self.agent_model();
        let window = self.agent_storm_window();
        let cid = trigger.conversation_id.clone();
        let reply_to = trigger.id.clone();
        // prompt = 除 @ 外文本；全剥空（用户只发 @）回退原文
        let prompt = {
            let stripped = strip_mentions(&trigger.content, &trigger.mentions);
            if stripped.is_empty() {
                trigger.content.trim().to_string()
            } else {
                stripped
            }
        };
        // 登记代次（顺手按 TTL 清理旧条目，防表无界增长）
        let gen = {
            let mut gens = shared.assistant_gen.lock().expect("assistant_gen poisoned");
            gens.retain(|_, (_, at)| at.elapsed() < ASSISTANT_GEN_TTL);
            let entry = gens.entry(cid.clone()).or_insert((0, Instant::now()));
            entry.0 += 1;
            entry.1 = Instant::now();
            entry.0
        };
        tokio::spawn(async move {
            // 睡满去抖窗口：让同窗口内的后续 @ 有机会顶掉本任务
            tokio::time::sleep(window).await;
            if !assistant_is_latest(&shared, &cid, gen) {
                return;
            }
            // env/测试未覆盖时动态探测活跃 LLM 端口（8123 优先）
            let url = match url_configured.clone() {
                Some(u) => u,
                None => Self::probe_live_llm_url().await,
            };
            let body = assistant_chat_complete(&url, &model, &prompt)
                .await
                .unwrap_or_else(|_| ASSISTANT_FALLBACK_TEXT.to_string());
            // LLM 往返期间可能又有新 @——提交前再核一次代次
            if !assistant_is_latest(&shared, &cid, gen) {
                return;
            }
            let mut content = truncate_chars(body.trim(), ASSISTANT_REPLY_MAX_CHARS);
            content.push_str(ASSISTANT_SUFFIX);
            let reply = Message {
                id: new_uuid(),
                conversation_id: cid.clone(),
                sender_id: ASSISTANT_SENDER_ID.to_string(),
                sender_name: Some(NEXOS_ASSISTANT.to_string()),
                content,
                msg_type: "text".to_string(),
                file_url: None,
                reply_to: Some(reply_to),
                created_at: now_iso(),
                read_by: Vec::new(),
                sender_kind: "agent".to_string(),
                mentions: Vec::new(),
                attachment: None,
            };
            {
                let shared_for_db = Arc::clone(&shared);
                let reply_for_db = reply.clone();
                let _ = im_db_call(shared_for_db, move |conn| {
                    insert_message(conn, &reply_for_db)
                })
                .await;
            }
            if cid == LOBBY_ID {
                Self::broadcast_lobby(&shared.ws_hub, &reply);
            } else {
                Self::broadcast_conversation(&shared.ws_hub, &cid, &reply);
            }
            // 助手回复也是一条新消息——同样触发匹配的 webhook（参与的
            // agent 对 @ 的回答也能收到推送，无需轮询）
            shared.dispatch_webhooks(&reply).await;
        });
    }
}
