//! IM 测试套（2026-09-25 大文件拆分批随域迁移：整体迁 im/tests.rs 单文件，
//! 与 film_hub/tests.rs 同款惯例；`use super::*` 经 mod.rs 私有 glob use
//! 链可达全部域符号，内容逐字节原样仅去一层 mod 包装缩进）。

use super::*;

fn empty_handler() -> ImRouteHandler {
    ImRouteHandler::with_empty()
}

/// 快速联邦节流版 handler（1ms 级双时延注入）：双节点端到端与延迟队列
/// 测试用——默认 10s/60s 太慢，语义不变仅缩短时钟。
fn fast_fed_handler() -> ImRouteHandler {
    ImRouteHandler::with_empty()
        .with_fed_throttle_delays(Duration::from_millis(1), Duration::from_millis(1))
}

fn demo_handler() -> ImRouteHandler {
    ImRouteHandler::with_demo_data()
}

fn get_req(path: &str) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Get,
        path: path.into(),
        headers: serde_json::json!({}),
        body: serde_json::Value::Null,
        auth: None,
    }
}

fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Post,
        path: path.into(),
        headers: serde_json::json!({}),
        body,
        auth: None,
    }
}

// —— 区块链认证测试辅助（真密钥对，k256 与生产同栈）——

/// 生成真 secp256k1 密钥对（CSPRNG）。
fn new_key() -> k256::ecdsa::SigningKey {
    use k256::elliptic_curve::rand_core::OsRng;
    k256::ecdsa::SigningKey::random(&mut OsRng)
}

/// 私钥 → IM 用户名（0x + 66 hex 压缩公钥）。
fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
    format!(
        "0x{}",
        hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
    )
}

/// 客户端签名：SHA-256(nonce UTF-8) → RFC6979 ECDSA（65 字节 r||s||v，
/// v 为真实恢复位——与前端 @noble/secp256k1 sign(sha256(nonce)) 同构）。
fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> [u8; 65] {
    use sha2::Digest;
    let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
    let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = u8::from(recid);
    out
}

/// 带 IM token 的 GET。
fn authed_get(path: &str, token: &str) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Get,
        path: path.into(),
        headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
        body: serde_json::Value::Null,
        auth: None,
    }
}

/// 带 IM token 的 POST。
fn authed_post(path: &str, token: &str, body: serde_json::Value) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Post,
        path: path.into(),
        headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
        body,
        auth: None,
    }
}

/// 真密钥对全流程登录：challenge → sign → verify → `(pubkey, token)`。
async fn login(h: &ImRouteHandler, sk: &k256::ecdsa::SigningKey) -> (String, String) {
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

// 0. DB 读不阻塞 runtime 并发（审计 A1-1/Top1 批 1 冒烟，2026-09-25）：
//    current_thread 单 worker 运行时下——后台 blocking 任务持 db 锁 200ms
//    （模拟慢查询），期间并发跑 (a) 纯异步心跳（5×5ms sleep）与 (b) 经
//    db_call（spawn_blocking）的 DB 读。改造前 async 里直接 `db.lock()`
//    会把唯一 worker 线程同步卡死在锁上（心跳饿到锁释放才动）；改造后
//    读挂到 blocking 池，心跳在 DB 忙碌期间照常推进——断言心跳完成时刻
//    显著早于锁释放时刻。
#[tokio::test(flavor = "current_thread")]
async fn db_read_does_not_block_runtime_concurrency() {
    let h = empty_handler();
    let started = Instant::now();
    // 基线行数（持锁前无竞争直读；测试模块与实现同模块可触私有面）
    let before = {
        let conn = h.shared.db.lock().expect("db poisoned");
        count_rows(&conn, "im_messages")
    };
    // 持锁慢查询（blocking 池线程，200ms 后释放）
    let db = Arc::clone(&h.shared);
    let holder = tokio::task::spawn_blocking(move || {
        let _guard = db.db.lock().expect("db poisoned");
        std::thread::sleep(Duration::from_millis(200));
    });
    // 确保持锁已生效（10ms 缓冲）
    tokio::time::sleep(Duration::from_millis(10)).await;

    let heartbeat = async {
        let mut beats = 0u32;
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            beats += 1;
        }
        (beats, started.elapsed())
    };
    let read = h.db_call(|conn| count_rows(conn, "im_messages"));
    let ((beats, heartbeat_done), msg_count) = tokio::join!(heartbeat, read);
    holder.await.expect("持锁任务 join");

    assert_eq!(beats, 5, "心跳 5 拍应全部完成");
    assert_eq!(msg_count, before, "并发读到的行数应与持锁前一致（无写入）");
    let holder_done = started.elapsed();
    assert!(
        heartbeat_done + Duration::from_millis(50) < holder_done,
        "DB 忙碌期间心跳应持续推进（heartbeat_done={heartbeat_done:?} 应显著早于锁释放 {holder_done:?}）\
         ——否则说明 DB 读又回到了 async 上下文同步持锁"
    );
}

// 1. routes 数量（19 原有 + 2 认证 + 1 离线补拉 + 2 附件 + 3 推送通知 + 2 联邦开关
//    + 2 大厅开放开关 + 2 远程大厅互联 + 3 联邦大厅 + 3 直通消息 = 39）
#[tokio::test]
async fn routes_declares_all_im_endpoints() {
    let h = empty_handler();
    let routes = h.routes().await;
    assert_eq!(
        routes.len(),
        39,
        "应声明 39 条路由（19 原有 + 2 认证 + 1 补拉 + 2 附件 + 3 推送通知 + 2 联邦开关 + 2 大厅开放 + 2 远程大厅 + 3 联邦大厅 + 3 直通消息）"
    );
    assert!(routes.iter().all(|r| r.handler_component == COMPONENT));
    let pairs: Vec<(HttpMethod, &str)> =
        routes.iter().map(|r| (r.method, r.path.as_str())).collect();
    // 认证 2 条（公开）
    assert!(pairs.contains(&(HttpMethod::Post, PATH_AUTH_CHALLENGE)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_AUTH_VERIFY)));
    // 原有 3 条扩展端点
    assert!(pairs.contains(&(HttpMethod::Post, PATH_MSG_READ)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_CONV_UNREAD)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_SEARCH)));
    // 离线补拉 1 条（IM token 在 handler 内验，不走系统中间件）
    assert!(pairs.contains(&(HttpMethod::Get, PATH_MESSAGES_CATCHUP)));
    // 大厅 4 条
    assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_MESSAGES)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_LOBBY_MESSAGES)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_MEMBERS)));
    // 附件 2 条（IM token 在 handler 内验）
    assert!(pairs.contains(&(HttpMethod::Post, PATH_FILES)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_FILE_DOWNLOAD)));
    // 推送通知 webhook 3 条（IM token 在 handler 内验，owner=pubkey）
    assert!(pairs.contains(&(HttpMethod::Post, PATH_NOTIFY_REGISTER)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_NOTIFY_LIST)));
    assert!(pairs.contains(&(HttpMethod::Delete, PATH_NOTIFY_UNREGISTER)));
    // 联邦接收开关 2 条（IM token / admin 在 handler 内验）
    assert!(pairs.contains(&(HttpMethod::Get, PATH_FEDERATION)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_FEDERATION)));
    // 大厅开放开关 2 条（admin 或 IM token 在 handler 内验）
    assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_ACCESS)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_LOBBY_ACCESS)));
    // 远程大厅互联 2 条（IM token 在 handler 内验）
    assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_REMOTE)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_LOBBY_REMOTE_MESSAGES)));
    // 联邦大厅 3 条（跨节点共享频道，IM token 在 handler 内验）
    assert!(pairs.contains(&(HttpMethod::Get, PATH_FED_LOBBY)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_FED_LOBBY_MESSAGES)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_FED_LOBBY_MESSAGES)));
    // 直通消息 DM 3 条（POST 发起 + GET/POST access 开关；IM token 在
    // handler 内验）
    assert!(pairs.contains(&(HttpMethod::Post, PATH_DM)));
    assert!(pairs.contains(&(HttpMethod::Get, PATH_DM_ACCESS)));
    assert!(pairs.contains(&(HttpMethod::Post, PATH_DM_ACCESS)));
    // 用户面端点不走系统中间件（IM token 在 handler 内验）；
    // 仅 POST /peers（管理面）保留系统级 requires_auth。
    for r in &routes {
        let expected = r.path == PATH_PEERS && r.method == HttpMethod::Post;
        assert_eq!(
            r.requires_auth, expected,
            "{:?} {} 的 requires_auth 应为 {expected}",
            r.method, r.path
        );
    }
}

// 2. seed 数据验证（2 对话 + 5 demo 消息 + 1 大厅欢迎消息 + 1 群组）
#[tokio::test]
async fn seed_data_validation() {
    let h = demo_handler();
    let resp = h.handle(get_req(PATH_STATUS)).await.unwrap();
    assert_eq!(resp.body["conversations"], 2);
    assert_eq!(resp.body["messages"], 6, "5 条 demo + 1 条大厅欢迎系统消息");
    assert_eq!(resp.body["groups"], 1);
}

// 3. SQLite conversations roundtrip（认证后 created_by = token pubkey）
#[tokio::test]
async fn conversations_roundtrip() {
    let h = empty_handler();
    let sk = new_key();
    let (pubkey, token) = login(&h, &sk).await;
    let resp = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "测试对话", "created_by": "forged-attacker" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    let id = resp.body["id"].as_str().unwrap().to_string();
    assert_eq!(resp.body["name"], "测试对话");
    assert_eq!(
        resp.body["created_by"], pubkey,
        "created_by 应为 token pubkey"
    );
    // 列表含新对话
    let list = h.handle(authed_get(PATH_CONV_LIST, &token)).await.unwrap();
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    // snapshot 也能查到（DB 真实写入）
    let snap = h.conversations_snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].id, id);
}

// 4. SQLite messages roundtrip（含增强字段；sender 一律 = token pubkey）
#[tokio::test]
async fn messages_roundtrip_enhanced_fields() {
    let h = empty_handler();
    let sk = new_key();
    let (pubkey, token) = login(&h, &sk).await;
    let display_name = derive_display_name(&parse_im_pubkey(&pubkey).unwrap());
    // 先建对话
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c1" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    // 发一条带 file_url 的消息（自报 sender 字段应被忽略/覆盖）
    let resp = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({
                "content": "见附件",
                "sender_id": "alice",
                "sender_name": "Alice",
                "msg_type": "file",
                "file_url": "/tank/a.pdf"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body["msg_type"], "file");
    assert_eq!(resp.body["file_url"], "/tank/a.pdf");
    assert_eq!(resp.body["sender_id"], pubkey, "sender_id 应被服务端覆盖");
    assert_eq!(resp.body["sender_name"], display_name);
    // 历史可查回
    let hist = h
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
        ))
        .await
        .unwrap();
    let arr = hist.body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["content"], "见附件");
    assert_eq!(arr[0]["file_url"], "/tank/a.pdf");
}

// 5. SQLite groups + members roundtrip（owner/joiner = token pubkey）
#[tokio::test]
async fn groups_and_members_roundtrip() {
    let h = empty_handler();
    let (pubkey1, token1) = login(&h, &new_key()).await;
    let (pubkey2, token2) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            PATH_GROUPS,
            &token1,
            serde_json::json!({ "name": "team", "owner": "forged-attacker" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body["owner"], pubkey1, "owner 应为 token pubkey");
    let gid = resp.body["id"].as_str().unwrap().to_string();
    // 列表含新群组，含 members
    let list = h.handle(authed_get(PATH_GROUPS, &token1)).await.unwrap();
    let arr = list.body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["kind"], "group");
    let members = arr[0]["members"].as_array().unwrap();
    assert!(members.contains(&serde_json::json!(pubkey1)));
    // 第二个用户 join（body member 自报值被忽略）
    h.handle(authed_post(
        &format!("/api/v1/im/groups/{gid}/join"),
        &token2,
        serde_json::json!({ "member": "forged-attacker" }),
    ))
    .await
    .unwrap();
    let members_resp = h
        .handle(authed_get(
            &format!("/api/v1/im/groups/{gid}/members"),
            &token2,
        ))
        .await
        .unwrap();
    let m = members_resp.body["members"].as_array().unwrap();
    assert!(
        m.contains(&serde_json::json!(pubkey2)),
        "join 应以 token pubkey 入组"
    );
    assert!(!m.contains(&serde_json::json!("forged-attacker")));
}

// 6. 消息已读标记（已读人 = token pubkey）
#[tokio::test]
async fn mark_message_read() {
    let h = demo_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    // msg-1（conv-general, sender alice）初始 read_by=["alice"]
    let before = h
        .handle(authed_post(
            "/api/v1/im/messages/msg-1/read",
            &token,
            serde_json::json!({ "user_id": "forged-attacker" }),
        ))
        .await
        .unwrap();
    assert_eq!(before.status, 200);
    let read_by = before.body["read_by"].as_array().unwrap();
    assert!(
        read_by.contains(&serde_json::json!(pubkey)),
        "已读人应为 token pubkey"
    );
    assert!(read_by.contains(&serde_json::json!("alice")));
    assert!(!read_by.contains(&serde_json::json!("forged-attacker")));
    // 不存在消息 → 404
    let miss = h
        .handle(authed_post(
            "/api/v1/im/messages/nope/read",
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(miss.status, 404);
}

// 7. 未读计数（user = token pubkey，查询参数 ?user= 被忽略）
#[tokio::test]
async fn unread_count() {
    let h = demo_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    // conv-general 2 条 demo 消息对新身份（pubkey）全未读；?user=alice 自报应被忽略
    let resp = h
        .handle(authed_get(
            "/api/v1/im/conversations/conv-general/unread?user=alice",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["unread"], 2);
    assert_eq!(resp.body["user"], pubkey);
    // 标记 msg-1 已读后再查 → 1
    h.handle(authed_post(
        "/api/v1/im/messages/msg-1/read",
        &token,
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    let resp2 = h
        .handle(authed_get(
            "/api/v1/im/conversations/conv-general/unread",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp2.body["unread"], 1);
}

// 8. 搜索面（2026-08-22 迭代：会话范围 + member 门 + limit 钳制 + LIKE 转义 + 空 q 400）
// 8a. 会话搜索（指定 conversation_id）：直接对话可读 → 命中 demo msg-3
#[tokio::test]
async fn search_scoped_conversation() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_get(
            "/api/v1/im/search?q=设计文档&conversation_id=conv-dev",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "直接对话搜索应 200: {}", resp.body);
    assert_eq!(resp.body["count"], 1);
    let results = resp.body["results"].as_array().unwrap();
    assert_eq!(results[0]["content"], "看看这份设计文档");
    // 回显：q 原文（前端高亮用）+ 实际搜索的会话 id
    assert_eq!(resp.body["q"], "设计文档");
    assert_eq!(resp.body["conversation_id"], "conv-dev");
    // 范围隔离：搜 conv-general 不命中 conv-dev 的消息
    let miss = h
        .handle(authed_get(
            "/api/v1/im/search?q=设计文档&conversation_id=conv-general",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(miss.body["count"], 0);
}

// 8b. 大厅搜索（conversation_id 缺省 = lobby）：先 GET /lobby 自动加入，
//     欢迎消息含 "大厅"；新发消息按 created_at 倒序排最前
#[tokio::test]
async fn search_lobby_default_scope() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    // 未加入大厅 → 403（member 门同补拉）
    let denied = h
        .handle(authed_get("/api/v1/im/search?q=大厅", &token))
        .await
        .unwrap();
    assert_eq!(denied.status, 403, "未加入大厅搜索应 403");
    // GET /lobby 自动加入（落欢迎消息）
    let join = h
        .handle(authed_get("/api/v1/im/lobby", &token))
        .await
        .unwrap();
    assert_eq!(join.status, 200);
    // 再发一条含关键词的消息 → 倒序第一条
    let sent = h
        .handle(authed_post(
            "/api/v1/im/lobby/messages",
            &token,
            serde_json::json!({ "content": "这条也提到大厅测试" }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let resp = h
        .handle(authed_get("/api/v1/im/search?q=大厅", &token))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "加入后默认搜大厅应 200: {}", resp.body);
    assert_eq!(resp.body["conversation_id"], "lobby");
    let results = resp.body["results"].as_array().unwrap();
    assert!(results.len() >= 2, "欢迎消息 + 新消息都应命中");
    assert_eq!(results[0]["content"], "这条也提到大厅测试", "最新在前");
}

// 8c. 会话搜索权限：群组非成员 403（join 后 200）；未知会话 404
#[tokio::test]
async fn search_conversation_permission() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    // 群组 demo 数据成员是 alice/bob/carol，新身份非成员 → 403
    let denied = h
        .handle(authed_get(
            "/api/v1/im/search?q=群组&conversation_id=group-dev-team",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(denied.status, 403, "非群组成员搜索应 403");
    // join 后 → 200，"群组" 命中 msg-5（Carol 加入了群组）
    let join = h
        .handle(authed_post(
            "/api/v1/im/groups/group-dev-team/join",
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(join.status, 200);
    let resp = h
        .handle(authed_get(
            "/api/v1/im/search?q=群组&conversation_id=group-dev-team",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["count"], 1);
    // 未知会话 → 404
    let missing = h
        .handle(authed_get(
            "/api/v1/im/search?q=hi&conversation_id=nope",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status, 404);
}

// 8d. LIKE 特殊字符按字面匹配（% / _ / \ 转义，ESCAPE '\'）
#[tokio::test]
async fn search_like_wildcard_escaped() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    h.handle(authed_get("/api/v1/im/lobby", &token))
        .await
        .unwrap();
    for content in ["折扣 100% off", "打五折不含百分号", "a_b 下划线"] {
        let r = h
            .handle(authed_post(
                "/api/v1/im/lobby/messages",
                &token,
                serde_json::json!({ "content": content }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
    }
    // q="100%" 只命中含字面 % 的那条（未转义时 % 是通配符，会命中 "100" 开头的一切）
    let pct = h
        .handle(authed_get("/api/v1/im/search?q=100%25", &token))
        .await
        .unwrap();
    assert_eq!(pct.status, 200);
    let results = pct.body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "百分号按字面匹配: {}", pct.body);
    assert_eq!(results[0]["content"], "折扣 100% off");
    // q="%" 不命中无百分号的消息（未转义的 LIKE "%%%" 匹配一切）
    let only_pct = h
        .handle(authed_get("/api/v1/im/search?q=%25", &token))
        .await
        .unwrap();
    assert_eq!(
        only_pct.body["results"].as_array().unwrap().len(),
        1,
        "裸 % 只匹配字面百分号: {}",
        only_pct.body
    );
    // q="_" 只命中含字面下划线的消息（未转义的 "%_%" 匹配任意非空）
    let und = h
        .handle(authed_get("/api/v1/im/search?q=_", &token))
        .await
        .unwrap();
    let und_results = und.body["results"].as_array().unwrap();
    assert_eq!(und_results.len(), 1, "下划线按字面匹配: {}", und.body);
    assert_eq!(und_results[0]["content"], "a_b 下划线");
}

// 8e. limit 钳制（默认 50，钳到 1..=200）
#[tokio::test]
async fn search_limit_clamped() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    h.handle(authed_get("/api/v1/im/lobby", &token))
        .await
        .unwrap();
    for i in 1..=3 {
        let r = h
            .handle(authed_post(
                "/api/v1/im/lobby/messages",
                &token,
                serde_json::json!({ "content": format!("限额测试条目{i}") }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
    }
    // limit=2 → 2 条
    let two = h
        .handle(authed_get("/api/v1/im/search?q=限额测试&limit=2", &token))
        .await
        .unwrap();
    assert_eq!(two.body["count"], 2);
    // limit=0 → 钳到 1
    let one = h
        .handle(authed_get("/api/v1/im/search?q=限额测试&limit=0", &token))
        .await
        .unwrap();
    assert_eq!(one.body["count"], 1, "limit=0 应钳到 1: {}", one.body);
    // limit=99999 → 钳到 200（3 条全返回，不报错）
    let big = h
        .handle(authed_get(
            "/api/v1/im/search?q=限额测试&limit=99999",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(big.status, 200);
    assert_eq!(big.body["count"], 3);
    // 非法 limit → 回退默认 50
    let bad = h
        .handle(authed_get("/api/v1/im/search?q=限额测试&limit=abc", &token))
        .await
        .unwrap();
    assert_eq!(bad.body["count"], 3);
}

// 8f. 空 q → 400（缺省 / 空串 / 纯空白）
#[tokio::test]
async fn search_empty_q_rejected() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    h.handle(authed_get("/api/v1/im/lobby", &token))
        .await
        .unwrap();
    for path in [
        "/api/v1/im/search",
        "/api/v1/im/search?q=",
        "/api/v1/im/search?q=%20%20",
    ] {
        let resp = h.handle(authed_get(path, &token)).await.unwrap();
        assert_eq!(resp.status, 400, "{path} 空 q 应 400");
    }
    // 无 token → 401（语义同其它 IM 用户面端点）
    let anon = h.handle(get_req("/api/v1/im/search?q=hi")).await.unwrap();
    assert_eq!(anon.status, 401);
}

// 9. 消息按时间排序
#[tokio::test]
async fn messages_ordered_by_time() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_get(
            "/api/v1/im/conversations/conv-general/messages",
            &token,
        ))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // msg-1 (09:00) 在 msg-2 (09:01) 之前
    assert_eq!(arr[0]["id"], "msg-1");
    assert_eq!(arr[1]["id"], "msg-2");
}

// 10. file_url 消息类型（msg-3 为 file）
#[tokio::test]
async fn file_message_type() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_get(
            "/api/v1/im/conversations/conv-dev/messages",
            &token,
        ))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["msg_type"], "file");
    assert_eq!(arr[0]["file_url"], "/tank/docs/design.pdf");
}

// 11. 系统消息类型（msg-5 为 system）
#[tokio::test]
async fn system_message_type() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_get(
            "/api/v1/im/conversations/group-dev-team/messages",
            &token,
        ))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[1]["msg_type"], "system");
}

// 12. peers 增查（兼容前端 addr 字段）
#[tokio::test]
async fn peers_add_and_list() {
    let h = empty_handler();
    let resp = h
        .handle(post_req(
            PATH_PEERS,
            serde_json::json!({ "addr": "10.0.0.5:8443", "id": "node-5", "name": "nodeA" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body["id"], "node-5");
    assert_eq!(resp.body["endpoint"], "10.0.0.5:8443");
    assert_eq!(resp.body["status"], "online");
    assert_eq!(h.peers_snapshot().len(), 1);
}

// 13. 向群组发消息（group id 可作为 conversation_id）+ WS 广播
#[tokio::test]
async fn send_message_to_group() {
    let hub = WsHub::default();
    let (_id, _rx) = hub.subscribe_raw("probe");
    // 注入 ws_hub + ImAuth 的 handler（内存库 + seed）
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn).unwrap();
    seed_demo(&conn).unwrap();
    let h = ImRouteHandler::from_parts(conn, Some(hub.clone()), Arc::new(ImAuth::default()));
    let (pubkey, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            "/api/v1/im/conversations/group-dev-team/messages",
            &token,
            serde_json::json!({ "content": "新消息", "sender_id": "forged-attacker" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body["conversation_id"], "group-dev-team");
    assert_eq!(resp.body["sender_id"], pubkey);
    // hub 有 1 个订阅 → 广播应送达 1 个
    assert_eq!(hub.subscriber_count(), 1);
}

// 14. send_message 不存在的对话 → 404（无 token 先 401）
#[tokio::test]
async fn send_message_unknown_conversation_404() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            "/api/v1/im/conversations/nope/messages",
            &token,
            serde_json::json!({ "content": "hi" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404);
}

// 15. 兜底未匹配 → 404
#[tokio::test]
async fn unmatched_route_404() {
    let h = empty_handler();
    let resp = h.handle(get_req("/api/v1/im/unknown")).await.unwrap();
    assert_eq!(resp.status, 404);
    assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
}

// =========================================================================
// 离线补拉（GET /api/v1/im/messages?conversation_id=&after_id=&limit=）
// 单元测——WS 断线重连后的缺口增量语义
// =========================================================================

// C1. after_id 语义：缺省 → 全量升序；严格大于（不含 after_id 本身）；
//     未知 after_id → 从头升序；结果按插入序（rowid）升序
#[tokio::test]
async fn catchup_after_id_semantics() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    // conv-general 有 msg-1（09:00）/ msg-2（09:01）
    let base = "/api/v1/im/messages?conversation_id=conv-general";
    // 缺省 after_id → 全量升序
    let resp = h.handle(authed_get(base, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "msg-1", "升序：msg-1 在前");
    assert_eq!(arr[1]["id"], "msg-2");
    // after_id=msg-1 → 严格大于 → 只剩 msg-2（不含 after_id 本身）
    let resp = h
        .handle(authed_get(&format!("{base}&after_id=msg-1"), &token))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 1, "严格晚于 msg-1 的只有 msg-2");
    assert_eq!(arr[0]["id"], "msg-2");
    // after_id 指向别的会话的消息 → 本会话无此 id → 从头升序
    let resp = h
        .handle(authed_get(&format!("{base}&after_id=msg-3"), &token))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 2, "未知 after_id 应回退到从头取");
    assert_eq!(arr[0]["id"], "msg-1");
}

// C2. limit：缺省 50；上限 200 钳制；非法值回退默认；下限 1 钳制
#[tokio::test]
async fn catchup_limit_clamped() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "灌水群" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    // 直接落库 205 条（经 insert_message 保持 rowid = 写入序）
    {
        let conn = h.shared.db.lock().expect("db poisoned");
        for i in 1..=205 {
            let m = Message {
                id: format!("flood-{i}"),
                conversation_id: cid.clone(),
                sender_id: "bob".into(),
                sender_name: Some("Bob".into()),
                content: format!("第 {i} 条"),
                msg_type: "text".into(),
                file_url: None,
                reply_to: None,
                created_at: format!("2026-01-01T00:{:02}:{:02}+08:00", i / 60, i % 60),
                read_by: Vec::new(),
                sender_kind: "human".into(),
                mentions: Vec::new(),
                attachment: None,
            };
            insert_message(&conn, &m).unwrap();
        }
    }
    let base = format!("/api/v1/im/messages?conversation_id={cid}");
    // 缺省 → 50（从最早一条开始升序）
    let resp = h.handle(authed_get(&base, &token)).await.unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 50, "默认 limit=50");
    assert_eq!(arr[0]["id"], "flood-1", "升序从头取");
    assert_eq!(arr[49]["id"], "flood-50");
    // 上限钳制：limit=10000 → 200
    let resp = h
        .handle(authed_get(&format!("{base}&limit=10000"), &token))
        .await
        .unwrap();
    assert_eq!(resp.body.as_array().unwrap().len(), 200, "上限 200 钳制");
    // 超上限但不足全量：limit=201+ 且总量更少时按 200
    let resp = h
        .handle(authed_get(&format!("{base}&limit=99999"), &token))
        .await
        .unwrap();
    assert_eq!(resp.body.as_array().unwrap().len(), 200);
    // 小 limit
    let resp = h
        .handle(authed_get(&format!("{base}&limit=3"), &token))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[2]["id"], "flood-3");
    // 非法 limit → 默认 50；limit=0 → 钳到 1
    let resp = h
        .handle(authed_get(&format!("{base}&limit=abc"), &token))
        .await
        .unwrap();
    assert_eq!(
        resp.body.as_array().unwrap().len(),
        50,
        "非法 limit 回退 50"
    );
    let resp = h
        .handle(authed_get(&format!("{base}&limit=0"), &token))
        .await
        .unwrap();
    assert_eq!(resp.body.as_array().unwrap().len(), 1, "limit=0 钳到 1");
    // after_id + limit 组合：从 flood-200 之后只剩 5 条
    let resp = h
        .handle(authed_get(
            &format!("{base}&after_id=flood-200&limit=200"),
            &token,
        ))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 5);
    assert_eq!(arr[0]["id"], "flood-201");
    assert_eq!(arr[4]["id"], "flood-205");
}

// C3. 越权与存在性：群组非成员 403（join 后 200）；直接对话沿用全员可读；
//     未知会话 404；缺 conversation_id 400；无 token 401
#[tokio::test]
async fn catchup_membership_and_existence_gates() {
    let h = empty_handler();
    let (pubkey1, token1) = login(&h, &new_key()).await;
    let (_pubkey2, token2) = login(&h, &new_key()).await;
    // 用户 1 建群（自己是 owner）+ 建直接对话
    let g = h
        .handle(authed_post(
            PATH_GROUPS,
            &token1,
            serde_json::json!({ "name": "私密群" }),
        ))
        .await
        .unwrap();
    let gid = g.body["id"].as_str().unwrap().to_string();
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token1,
            serde_json::json!({ "name": "直接对话" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    // 用户 2 非群成员 → 403
    let denied = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={gid}"),
            &token2,
        ))
        .await
        .unwrap();
    assert_eq!(denied.status, 403, "非群组成员补拉应 403");
    // join 后 → 200
    h.handle(authed_post(
        &format!("/api/v1/im/groups/{gid}/join"),
        &token2,
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    let allowed = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={gid}"),
            &token2,
        ))
        .await
        .unwrap();
    assert_eq!(allowed.status, 200, "join 后应可补拉");
    assert_eq!(allowed.body.as_array().unwrap().len(), 0);
    // 直接对话：非创建者也可读（沿用现状，保持兼容）
    let conv = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={cid}"),
            &token2,
        ))
        .await
        .unwrap();
    assert_eq!(conv.status, 200, "直接对话沿用全员可读");
    // 大厅：未加入 → 403；GET /lobby 自动加入后 → 200
    let lobby_denied = h
        .handle(authed_get(
            "/api/v1/im/messages?conversation_id=lobby",
            &token2,
        ))
        .await
        .unwrap();
    assert_eq!(lobby_denied.status, 403, "未加入大厅补拉应 403");
    let _ = h.handle(authed_get(PATH_LOBBY, &token2)).await.unwrap();
    let lobby_ok = h
        .handle(authed_get(
            "/api/v1/im/messages?conversation_id=lobby",
            &token2,
        ))
        .await
        .unwrap();
    assert_eq!(lobby_ok.status, 200);
    // 未知会话 → 404
    let miss = h
        .handle(authed_get(
            "/api/v1/im/messages?conversation_id=no-such",
            &token2,
        ))
        .await
        .unwrap();
    assert_eq!(miss.status, 404);
    // 缺 conversation_id → 400；无 token → 401
    let bad = h
        .handle(authed_get("/api/v1/im/messages", &token2))
        .await
        .unwrap();
    assert_eq!(bad.status, 400);
    let anon = h
        .handle(get_req("/api/v1/im/messages?conversation_id=x"))
        .await
        .unwrap();
    assert_eq!(anon.status, 401);
    // 自查：用户 1（owner）读自己的群没问题
    let own = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={gid}"),
            &token1,
        ))
        .await
        .unwrap();
    assert_eq!(own.status, 200);
    assert_eq!(own.body.as_array().unwrap().len(), 0);
    assert!(!pubkey1.is_empty());
}

// C4. 空结果：after_id 已是最新一条 → 返回空数组（重连补拉常见的"无缺口"）
#[tokio::test]
async fn catchup_empty_result_when_after_latest() {
    let h = demo_handler();
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_get(
            "/api/v1/im/messages?conversation_id=conv-general&after_id=msg-2",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body.as_array().unwrap().len(),
        0,
        "after 最新一条应返回空数组"
    );
    // 空会话（新建对话无消息）→ 也是空数组而非错误
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "空对话" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let resp = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={cid}&after_id=whatever"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_array().unwrap().len(), 0);
}

// C5. 大厅同语义：GET /lobby/messages?after_id= 严格晚于、升序、
//     after 最新 → 空；未知 after_id → 从头（与补拉端点一致）
#[tokio::test]
async fn lobby_after_id_same_semantics() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    // 进大厅（自动加入 + 欢迎消息）：消息流 = seed 欢迎 + 我的欢迎
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // 再发 2 条
    for content in ["第一条", "第二条"] {
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": content }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{content} 应发成功");
    }
    // 全量（旧行为：最近 50 条正序）拿 id 基准
    let full = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let arr = full.body.as_array().unwrap();
    assert_eq!(arr.len(), 4, "seed 欢迎 + 用户欢迎 + 2 条发言");
    let ids: Vec<&str> = arr.iter().map(|m| m["id"].as_str().unwrap()).collect();
    // after_id = 第 2 条（用户欢迎）→ 只剩 2 条发言，且顺序升序
    let resp = h
        .handle(authed_get(
            &format!("{}?after_id={}", PATH_LOBBY_MESSAGES, ids[1]),
            &token,
        ))
        .await
        .unwrap();
    let arr = resp.body.as_array().unwrap();
    assert_eq!(arr.len(), 2, "严格晚于第 2 条的是后 2 条发言");
    assert_eq!(arr[0]["content"], "第一条");
    assert_eq!(arr[1]["content"], "第二条");
    // after 最新 → 空
    let resp = h
        .handle(authed_get(
            &format!("{}?after_id={}", PATH_LOBBY_MESSAGES, ids[3]),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.body.as_array().unwrap().len(), 0);
    // 未知 after_id → 从头（4 条全量，≤50）
    let resp = h
        .handle(authed_get(
            &format!("{}?after_id=nonexistent", PATH_LOBBY_MESSAGES),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.body.as_array().unwrap().len(), 4);
    // 无 token → 401
    let anon = h
        .handle(get_req(&format!("{}?after_id=x", PATH_LOBBY_MESSAGES)))
        .await
        .unwrap();
    assert_eq!(anon.status, 401);
}

// 辅助函数自测
#[test]
fn path_segments_parses_correctly() {
    assert_eq!(
        path_segments("/api/v1/im/groups"),
        vec!["api", "v1", "im", "groups"]
    );
    assert_eq!(
        path_segments("/api/v1/im/search?q=hi"),
        vec!["api", "v1", "im", "search"]
    );
}

#[test]
fn parse_query_str_works() {
    assert_eq!(parse_query_str("q=hi&x=1", "q"), Some("hi".into()));
    assert_eq!(parse_query_str("a=1&q=hello", "q"), Some("hello".into()));
    assert_eq!(parse_query_str("", "q"), None);
}

#[test]
fn default_trait_is_implemented() {
    fn assert_default<T: Default>() {}
    assert_default::<ImRouteHandler>();
}

#[test]
fn dto_round_trips_serde() {
    let m = Message {
        id: "x".into(),
        conversation_id: "c1".into(),
        sender_id: "alice".into(),
        sender_name: Some("Alice".into()),
        content: "hi".into(),
        msg_type: "text".into(),
        file_url: None,
        reply_to: None,
        created_at: "2026-01-01T00:00:00+08:00".into(),
        read_by: vec!["alice".into()],
        sender_kind: "human".into(),
        mentions: Vec::new(),
        attachment: None,
    };
    let v = serde_json::to_value(&m).unwrap();
    let back: Message = serde_json::from_value(v).unwrap();
    assert_eq!(back.id, "x");
    assert_eq!(back.read_by.len(), 1);
}

// =========================================================================
// 大厅（Lobby）单元测
// =========================================================================

/// 辅助：生成距 now 偏移 offset_secs 秒的 RFC3339 时间串。
fn iso_offset_secs(offset_secs: i64) -> String {
    (chrono::Local::now() + chrono::Duration::seconds(offset_secs))
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

// L1. 大厅 seed：空库建表即有大厅 + 欢迎系统消息（无 token → 401）
#[tokio::test]
async fn lobby_seed_creates_hall_and_welcome() {
    let h = empty_handler();
    let sk = new_key();
    let (_, token) = login(&h, &sk).await;
    // 大厅端点一律要求 IM token
    let anon = h.handle(get_req(PATH_LOBBY)).await.unwrap();
    assert_eq!(anon.status, 401, "无 token 的大厅访问应 401");
    let resp = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["id"], "lobby");
    assert_eq!(resp.body["name"], "大厅");
    assert_eq!(resp.body["member_count"], 1, "首次 GET 即自动加入");
    // seed 欢迎消息（im_messages，conversation_id=lobby）仍可见
    // （seed 1 条 + 本次 GET /lobby 自动加入的欢迎 1 条）
    let msgs = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let arr = msgs.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["msg_type"], "system");
    assert_eq!(
        arr[0]["content"],
        "欢迎来到 NexOS 大厅 — 连接每一个超级个体"
    );
    // info.last_message = 本次新用户的欢迎系统消息（最新的那条）
    assert_eq!(
        resp.body["last_message"]["content"],
        format!(
            "欢迎 {} 加入 NexOS 大厅",
            derive_display_name(sk.verifying_key())
        )
    );
}

// L2. 新用户首次 GET /lobby（Bearer）：自动加入 + 欢迎系统消息（展示名）
#[tokio::test]
async fn lobby_first_visit_joins_and_welcomes() {
    let h = empty_handler();
    let sk = new_key();
    let (pubkey, token) = login(&h, &sk).await;
    let display_name = derive_display_name(&parse_im_pubkey(&pubkey).unwrap());
    let resp = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.body["member_count"], 1);
    assert_eq!(resp.body["online_count"], 1, "刚心跳过应在线");
    // 欢迎消息已入大厅消息流（seed 1 条 + 欢迎 1 条），内容用展示名
    let msgs = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let arr = msgs.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(
        arr[1]["content"],
        format!("欢迎 {display_name} 加入 NexOS 大厅")
    );
    assert_eq!(arr[1]["msg_type"], "system");
    // 重复 GET 不再重复欢迎
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let msgs2 = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    assert_eq!(
        msgs2.body.as_array().unwrap().len(),
        2,
        "重复进入不再发欢迎"
    );
}

// L3. 发大厅消息：非成员 403；加入后 201 + 消息可查（sender = token pubkey）
#[tokio::test]
async fn lobby_send_message_member_gating() {
    let h = empty_handler();
    let sk = new_key();
    let (pubkey, token) = login(&h, &sk).await;
    // 未加入（从未 GET /lobby）→ 403
    let denied = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "user_id": "forged-attacker", "content": "hi" }),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status, 403);
    // 先进大厅（自动加入）→ 发言成功
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let sent = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "user_id": "forged-attacker", "content": "大家好！" }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    assert_eq!(sent.body["conversation_id"], "lobby");
    assert_eq!(sent.body["sender_id"], pubkey, "sender 应为 token pubkey");
    assert_eq!(sent.body["msg_type"], "text");
    // 大厅消息流可见（seed + 欢迎系统消息 + 发言）
    let msgs = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let contents: Vec<&str> = msgs
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(contents.contains(&"大家好！"));
    // 空内容 → 400
    let empty = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "  " }),
        ))
        .await
        .unwrap();
    assert_eq!(empty.status, 400);
}

// L4. 大厅成员列表：在线/离线区分（成员以 pubkey 记录）
#[tokio::test]
async fn lobby_members_online_offline() {
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // 把该成员的 last_seen 拨回 1 小时前 → 离线
    {
        let conn = h.shared.db.lock().expect("db poisoned");
        conn.execute(
            "UPDATE im_lobby_members SET last_seen=? WHERE user_id=?",
            params![iso_offset_secs(-3600), pubkey],
        )
        .unwrap();
    }
    let resp = h
        .handle(authed_get(PATH_LOBBY_MEMBERS, &token))
        .await
        .unwrap();
    assert_eq!(resp.body["member_count"], 1);
    assert_eq!(resp.body["online_count"], 0);
    let members = resp.body["members"].as_array().unwrap();
    assert_eq!(members[0]["user_id"], pubkey);
    assert_eq!(members[0]["online"], false);
}

// L5. 心跳：GET /lobby/messages（Bearer）刷新 last_seen（离线 → 在线）
#[tokio::test]
async fn lobby_heartbeat_touches_last_seen() {
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // 拨回 5 分钟前 → 离线
    {
        let conn = h.shared.db.lock().expect("db poisoned");
        conn.execute(
            "UPDATE im_lobby_members SET last_seen=? WHERE user_id=?",
            params![iso_offset_secs(-300), pubkey],
        )
        .unwrap();
    }
    let before = h
        .handle(authed_get(PATH_LOBBY_MEMBERS, &token))
        .await
        .unwrap();
    assert_eq!(before.body["online_count"], 0);
    // GET /lobby/messages（Bearer 心跳）→ 重新在线
    let _ = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let after = h
        .handle(authed_get(PATH_LOBBY_MEMBERS, &token))
        .await
        .unwrap();
    assert_eq!(after.body["online_count"], 1, "心跳后应在线");
}

// L6. is_online 纯函数：60s 窗口 / 解析失败
#[test]
fn is_online_window_semantics() {
    let now = chrono::Local::now().timestamp();
    // RFC3339 串
    let just_now = iso_offset_secs(-5);
    let stale = iso_offset_secs(-120);
    assert!(is_online(&just_now, now), "5s 前在线");
    assert!(!is_online(&stale, now), "120s 前离线");
    assert!(is_online(&iso_offset_secs(3), now), "轻微未来时间容忍在线");
    // 解析失败 / 空串 → 离线
    assert!(!is_online("not-a-time", now));
    assert!(!is_online("", now));
}

// L7. build_welcome_message 纯函数形状
#[test]
fn welcome_message_shape() {
    let m = build_welcome_message("dave");
    assert_eq!(m.conversation_id, "lobby");
    assert_eq!(m.sender_id, "system");
    assert_eq!(m.msg_type, "system");
    assert_eq!(m.content, "欢迎 dave 加入 NexOS 大厅");
    assert!(m.read_by.is_empty());
    assert!(!m.id.is_empty(), "id 为生成的 uuid");
}

// =========================================================================
// 区块链认证（IM_BLOCKCHAIN_AUTH_DESIGN §2）单元测——真密钥对全流程
// =========================================================================

// A1. challenge：合法公钥 → 256-bit nonce + TTL + 展示名
#[tokio::test]
async fn auth_challenge_valid_pubkey() {
    let h = empty_handler();
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
    assert_eq!(resp.body["expires_in"], IM_NONCE_TTL_SECS);
    let display = resp.body["display_name"].as_str().unwrap();
    assert!(
        display.starts_with("0x") && display.len() == 42,
        "EVM 地址 0x+40hex"
    );
}

// A2. challenge：公钥格式非法 → 400（缺 0x / 长度错 / 非 hex / 非法 sec1 点）
#[tokio::test]
async fn auth_challenge_rejects_invalid_pubkey() {
    let h = empty_handler();
    let valid = pubkey_hex(&new_key());
    for bad in [
        valid[2..].to_string(),             // 缺 0x 前缀
        format!("0x{}", &valid[2..66]),     // 长度不足（64 hex）
        format!("0x{}zz", &valid[2..66]),   // 非 hex 字符
        format!("0x04{}", "ab".repeat(32)), // 0x04 未压缩标签 + 33 字节 → sec1 解析失败
        "0x".to_string(),
        String::new(),
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

// A3. verify：真密钥对全流程 challenge→sign→verify→token（24h + 展示名）
#[tokio::test]
async fn auth_verify_full_flow_issues_token() {
    let h = empty_handler();
    let sk = new_key();
    let (pubkey, token) = login(&h, &sk).await;
    assert_eq!(token.len(), 64, "256-bit hex token");
    // verify 响应带 expires_in=86400 + pubkey + 展示名（再走一次校验响应字段）
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
                "signature": hex::encode(sig),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["expires_in"], IM_TOKEN_TTL_SECS);
    assert_eq!(resp.body["pubkey"], pubkey);
    assert!(resp.body["display_name"]
        .as_str()
        .unwrap()
        .starts_with("0x"));
}

// A4. nonce 重放拒绝（单次使用：verify 成功后同一 nonce 再验 → 401）
#[tokio::test]
async fn auth_verify_nonce_replay_rejected() {
    let h = empty_handler();
    let sk = new_key();
    let pubkey = pubkey_hex(&sk);
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
}

// A5. 未签发/不匹配的 nonce → 401
#[tokio::test]
async fn auth_verify_wrong_nonce_rejected() {
    let h = empty_handler();
    let sk = new_key();
    let pubkey = pubkey_hex(&sk);
    let sig = hex::encode(sign_nonce(&sk, "0".repeat(64).as_str()));
    let resp = h
        .handle(post_req(
            PATH_AUTH_VERIFY,
            serde_json::json!({
                "pubkey": pubkey,
                "nonce": "deadbeef",
                "signature": sig,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 401, "未知 nonce 应 401");
}

// A6. 伪造签名（另一把私钥签）→ 401
#[tokio::test]
async fn auth_verify_forged_signature_rejected() {
    let h = empty_handler();
    let sk = new_key();
    let attacker = new_key();
    let pubkey = pubkey_hex(&sk);
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
}

// A7. 签名格式非法（非 hex / 非 65 字节）→ 400
#[tokio::test]
async fn auth_verify_malformed_signature_rejected() {
    let h = empty_handler();
    let sk = new_key();
    let pubkey = pubkey_hex(&sk);
    let resp = h
        .handle(post_req(
            PATH_AUTH_CHALLENGE,
            serde_json::json!({ "pubkey": pubkey }),
        ))
        .await
        .unwrap();
    let nonce = resp.body["nonce"].as_str().unwrap().to_string();
    for bad_sig in [
        "zzzz".to_string(),
        hex::encode([0u8; 64]),
        hex::encode([0u8; 66]),
    ] {
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": bad_sig,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status,
            400,
            "签名格式非法应 400（len={}）",
            bad_sig.len()
        );
    }
}

// A8. 过期 token → 401
#[tokio::test]
async fn auth_expired_token_rejected() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    h.auth.expire_token_for_test(&token);
    let resp = h.handle(authed_get(PATH_CONV_LIST, &token)).await.unwrap();
    assert_eq!(resp.status, 401, "过期 token 应 401");
}

// A9. 单点登录：同 pubkey 二次 verify 顶掉旧 token
#[tokio::test]
async fn auth_single_login_replaces_old_token() {
    let h = empty_handler();
    let sk = new_key();
    let (_, old_token) = login(&h, &sk).await;
    // 旧 token 先确认可用
    let ok = h
        .handle(authed_get(PATH_CONV_LIST, &old_token))
        .await
        .unwrap();
    assert_eq!(ok.status, 200);
    // 同一密钥再登录 → 旧 token 失效、新 token 可用
    let (_, new_token) = login(&h, &sk).await;
    assert_ne!(old_token, new_token);
    let stale = h
        .handle(authed_get(PATH_CONV_LIST, &old_token))
        .await
        .unwrap();
    assert_eq!(stale.status, 401, "旧 token 应被顶掉（401）");
    let fresh = h
        .handle(authed_get(PATH_CONV_LIST, &new_token))
        .await
        .unwrap();
    assert_eq!(fresh.status, 200, "新 token 应可用");
}

// A10. REST 全端点强制 token：缺失 / 伪值 → 401（GET /status 与 POST /peers 除外）
#[tokio::test]
async fn auth_rest_endpoints_require_token() {
    let h = empty_handler();
    for (desc, req) in [
        ("GET /conversations", get_req(PATH_CONV_LIST)),
        ("GET /search", get_req(PATH_SEARCH)),
        ("GET /lobby", get_req(PATH_LOBBY)),
        (
            "POST /lobby/messages",
            post_req(PATH_LOBBY_MESSAGES, serde_json::json!({ "content": "hi" })),
        ),
        (
            "POST /conversations",
            post_req(PATH_CONV_LIST, serde_json::json!({ "name": "x" })),
        ),
        ("GET unread", get_req("/api/v1/im/conversations/c/unread")),
    ] {
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 401, "无 token 的 {desc} 应 401");
    }
    // 伪造 token 同样 401
    let forged = h
        .handle(authed_get(PATH_CONV_LIST, "0".repeat(64).as_str()))
        .await
        .unwrap();
    assert_eq!(forged.status, 401);
    // 公开端点不受影响：GET /status
    let status = h.handle(get_req(PATH_STATUS)).await.unwrap();
    assert_eq!(status.status, 200);
}

// A11. 请求体/查询参数伪造身份一律被服务端覆盖（join member / read user_id）
#[tokio::test]
async fn auth_identity_overrides_self_reported_fields() {
    let h = empty_handler();
    let (pubkey1, token1) = login(&h, &new_key()).await;
    let (pubkey2, token2) = login(&h, &new_key()).await;
    // 群组：body.member 自报 "admin" 应被忽略，实际入组的是 token pubkey
    let g = h
        .handle(authed_post(
            PATH_GROUPS,
            &token1,
            serde_json::json!({ "name": "g" }),
        ))
        .await
        .unwrap();
    let gid = g.body["id"].as_str().unwrap().to_string();
    let joined = h
        .handle(authed_post(
            &format!("/api/v1/im/groups/{gid}/join"),
            &token2,
            serde_json::json!({ "member": "admin" }),
        ))
        .await
        .unwrap();
    let members = joined.body["members"].as_array().unwrap();
    assert!(members.contains(&serde_json::json!(pubkey2)));
    assert!(!members.contains(&serde_json::json!("admin")));
    // leave：member = token pubkey（不是 body 自报）
    let left = h
        .handle(authed_post(
            &format!("/api/v1/im/groups/{gid}/leave"),
            &token2,
            serde_json::json!({ "member": pubkey1 }),
        ))
        .await
        .unwrap();
    let members = left.body["members"].as_array().unwrap();
    assert!(
        !members.contains(&serde_json::json!(pubkey2)),
        "token 本人应已退组"
    );
    assert!(
        members.contains(&serde_json::json!(pubkey1)),
        "自报他人不应被退组"
    );
}

// A12. REST 发消息：sender = token 反查 pubkey（全链路：建对话→发消息→历史）
#[tokio::test]
async fn auth_rest_send_message_sender_from_token() {
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let sent = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({
                "content": "来自链上身份",
                "sender_id": "victim",
                "sender_name": "受害者的名字",
                "role": "admin",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    assert_eq!(sent.body["sender_id"], pubkey);
    assert_eq!(
        sent.body["sender_name"],
        derive_display_name(&parse_im_pubkey(&pubkey).unwrap())
    );
    // 历史里的 sender 也归因到 pubkey
    let hist = h
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(hist.body[0]["sender_id"], pubkey);
    assert_eq!(
        hist.body[0]["sender_name"],
        derive_display_name(&parse_im_pubkey(&pubkey).unwrap())
    );
}

// A13. 展示名派生：公开测试向量（secp256k1 生成元 ↔ EVM 地址，私钥=1）
#[test]
fn auth_display_name_known_vector() {
    // 私钥 1 的公钥即生成元（压缩 0x0279be…）；其 EVM 地址是著名常量
    let vk =
        parse_im_pubkey("0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
            .expect("生成元公钥应可解析");
    assert_eq!(
        derive_display_name(&vk),
        "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
    // 随机密钥往返：parse(derive 用的公钥) 恒成功、两次派生一致
    let sk = new_key();
    let vk2 = parse_im_pubkey(&pubkey_hex(&sk)).unwrap();
    assert_eq!(derive_display_name(&vk2), derive_display_name(&vk2));
}

// A14. ImAuth 桶语义：nonce 覆盖 / token 反查 / WS 匹配
#[test]
fn auth_store_bucket_semantics() {
    let auth = ImAuth::default();
    let pk = pubkey_hex(&new_key());
    // nonce 新 challenge 覆盖旧值
    let n1 = auth.create_nonce(&pk);
    let n2 = auth.create_nonce(&pk);
    assert_ne!(n1, n2);
    assert!(!auth.take_nonce(&pk, &n1), "旧 nonce 已被覆盖");
    assert!(auth.take_nonce(&pk, &n2), "最新 nonce 可用");
    assert!(!auth.take_nonce(&pk, &n2), "nonce 单次使用");
    // token 反查 + WS 匹配
    let (token, _) = auth.issue_token(&pk);
    assert_eq!(auth.verify_token(&token), Some(pk.clone()));
    assert_eq!(auth.verify_token("bogus"), None);
    assert!(auth.verify_ws(&pk, &token), "user 与 token 匹配");
    assert!(
        !auth.verify_ws(&pubkey_hex(&new_key()), &token),
        "user 不匹配应拒绝"
    );
}

// —— WebSocket 握手强制（真服务器 + tokio-tungstenite）——

/// 启动带 IM 认证的网关（内存 im handler + 共享 ImAuth），返回
/// `(网关, 绑定地址, 认证存储)`。端口取临时空闲端口。
async fn start_im_gateway() -> (
    crate::gateway_impl::InProcessGateway,
    std::net::SocketAddr,
    Arc<ImAuth>,
) {
    use crate::gateway::Gateway;
    let gw = crate::gateway_impl::InProcessGateway::new();
    let auth = Arc::new(ImAuth::default());
    gw.register_component(
        "im",
        Box::new(ImRouteHandler::with_empty_ws(gw.ws_hub(), auth.clone())),
    )
    .await
    .expect("注册 im handler");
    gw.set_im_auth(Some(auth.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    gw.start(&format!("127.0.0.1:{}", addr.port()), None)
        .await
        .expect("start");
    (gw, addr, auth)
}

// A15. WS 握手：?user=<pubkey>&token=<token> 成功 + 广播可达
//     （token 经真挑战-签名流程获取——challenge/verify 只碰共享 ImAuth，
//      用同 auth 的探针 handler 走完整链路）
#[tokio::test]
async fn auth_ws_handshake_with_token_succeeds() {
    use futures::StreamExt;
    let (gw, addr, auth) = start_im_gateway().await;
    let probe = ImRouteHandler::with_empty_ws(gw.ws_hub(), auth.clone());
    let (pubkey, token) = login(&probe, &new_key()).await;

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = format!(
        "ws://127.0.0.1:{}/ws?user={pubkey}&token={token}",
        addr.port()
    );
    let req = url.as_str().into_client_request().unwrap();
    let (mut ws_stream, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("带合法 token 的 WS 握手应成功");
    // 握手后该连接应已订阅（以 pubkey 身份），能收到广播
    let pushed = WsMessage::Event {
        event: os_core::Event::new("test", os_core::Topic::System, "im.auth.ws"),
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(gw.ws_hub().broadcast_n(pushed), 1, "应有 1 个订阅");
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next())
        .await
        .expect("WS 收到不应超时")
        .expect("stream 不应结束")
        .expect("帧应无错");
    let text = frame.into_text().expect("应为文本帧");
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["type"], "event");
}

// A16. WS 握手：错误 token / user 不匹配 → 握手被拒（HTTP 401）
#[tokio::test]
async fn auth_ws_handshake_wrong_token_rejected() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (_gw, addr, auth) = start_im_gateway().await;
    let sk = new_key();
    let pubkey = pubkey_hex(&sk);
    let (token, _) = auth.issue_token(&pubkey);
    let base = format!("ws://127.0.0.1:{}/ws", addr.port());
    // 错误 token
    let bad = format!("{base}?user={pubkey}&token={}", "0".repeat(64));
    let req = bad.as_str().into_client_request().unwrap();
    assert!(
        tokio_tungstenite::connect_async(req).await.is_err(),
        "错误 token 握手应被拒"
    );
    // token 有效但 user 不匹配（冒充他人）
    let other = pubkey_hex(&new_key());
    let mismatch = format!("{base}?user={other}&token={token}");
    let req = mismatch.as_str().into_client_request().unwrap();
    assert!(
        tokio_tungstenite::connect_async(req).await.is_err(),
        "user 与 token 不匹配应被拒"
    );
}

// A17. WS 握手：裸 ?user= 无 token → 拒绝（一次性破坏性变更，无兼容通道）
#[tokio::test]
async fn auth_ws_handshake_no_token_rejected() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (_gw, addr, _auth) = start_im_gateway().await;
    let pubkey = pubkey_hex(&new_key());
    let url = format!("ws://127.0.0.1:{}/ws?user={pubkey}", addr.port());
    let req = url.as_str().into_client_request().unwrap();
    assert!(
        tokio_tungstenite::connect_async(req).await.is_err(),
        "裸 ?user= 不带 token 应被拒"
    );
    // 完全无参数同样拒绝
    let bare = format!("ws://127.0.0.1:{}/ws", addr.port());
    let req = bare.as_str().into_client_request().unwrap();
    assert!(
        tokio_tungstenite::connect_async(req).await.is_err(),
        "无 user/token 应被拒"
    );
}

// =========================================================================
// 多 AI agent 接入 + 文档传输（2026-08-21）单元测
// —— G 面：mentions/sender_kind/助手闭环；F 面：附件上传下载/核对
// =========================================================================

use std::sync::Mutex as StdMutex;

/// 测试用防风暴窗口（默认 3s 太慢；3 条连发 POST ≪ 200ms，去抖语义稳定）。
const TEST_STORM_WINDOW: Duration = Duration::from_millis(200);

/// 假 LLM 服务器（本地 TcpListener 手写 HTTP/1.1）：echo 模式回
/// `ECHO:<user prompt>`（同时证明请求形状与"除 @ 外文本"剥取）；
/// 捕获原始请求体到 `seen` 供断言（模型名/prompt）。返回完整 endpoint url。
async fn spawn_fake_llm(seen: Arc<StdMutex<Vec<String>>>) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut acc: Vec<u8> = Vec::new();
            let mut buf = [0u8; 16384];
            // 读全：按 Content-Length 判断请求体收完
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
                let head = String::from_utf8_lossy(&acc).into_owned();
                if let Some(pos) = head.find("\r\n\r\n") {
                    let cl = head[..pos]
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if acc.len() >= pos + 4 + cl {
                        break;
                    }
                }
            }
            let req_text = String::from_utf8_lossy(&acc).into_owned();
            // echo：取 messages[1].content（user 消息）
            let user_prompt = serde_json::from_str::<serde_json::Value>(
                req_text.split("\r\n\r\n").nth(1).unwrap_or(""),
            )
            .ok()
            .and_then(|v| v["messages"][1]["content"].as_str().map(str::to_string))
            .unwrap_or_default();
            seen.lock().unwrap().push(req_text);
            let body = serde_json::json!({
                "choices": [{"message": {"content": format!("ECHO:{user_prompt}")}}]
            });
            let body = body.to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (format!("http://{addr}/v1/chat/completions"), task)
}

/// 必然连接失败的推理端点（127.0.0.1:1 无服务 → 秒级 ECONNREFUSED，
/// 走"LLM 不可达降级"路径）。
const DEAD_LLM_URL: &str = "http://127.0.0.1:1/v1/chat/completions";

/// 等待条件成立（25ms 轮询；超时返回最后一次求值）。
async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    f()
}

/// 某会话里的助手回复（按合成 sender_id 判定——外部 agent 自声明
/// sender_kind=agent 的用户消息不算助手回复）。
fn agent_replies(h: &ImRouteHandler, cid: &str) -> Vec<Message> {
    h.messages_snapshot()
        .into_iter()
        .filter(|m| m.conversation_id == cid && m.sender_id == ASSISTANT_SENDER_ID)
        .collect()
}

/// 独立临时附件根目录（测试隔离；返回 (路径字符串, PathBuf)）。
fn temp_files_root(tag: &str) -> (String, PathBuf) {
    let dir = std::env::temp_dir().join(format!("nexos-im-test-{tag}-{}", new_uuid()));
    (dir.to_string_lossy().into_owned(), dir)
}

// G1. mentions 解析：中文/英文/多 @/混合
#[test]
fn mentions_parse_chinese_english_multi() {
    assert_eq!(
        parse_mentions("你好 @NexOS助手 请看 @alice 的稿"),
        vec!["NexOS助手", "alice"]
    );
    assert_eq!(parse_mentions("@bob-dev_1 收到没"), vec!["bob-dev_1"]);
    assert_eq!(parse_mentions("@甲 @乙 @甲"), vec!["甲", "乙"], "去重保序");
    assert_eq!(parse_mentions("没有提及"), Vec::<String>::new());
    assert_eq!(parse_mentions(""), Vec::<String>::new());
}

// G2. mentions 边界：裸 @/@@/邮箱式/超长截断/标点截断
#[test]
fn mentions_parse_edges() {
    assert_eq!(parse_mentions("@"), Vec::<String>::new(), "裸 @ 不算");
    assert_eq!(
        parse_mentions("@ 你好"),
        Vec::<String>::new(),
        "@ 后空格不算"
    );
    assert_eq!(
        parse_mentions("@@NexOS助手"),
        vec!["NexOS助手"],
        "首个 @ 后跟 @ 非法 → 第二个 @ 起解析"
    );
    // 邮箱式：名字到非法字符（.）为止——既定语义
    assert_eq!(parse_mentions("mail me a@b.example.com"), vec!["b"]);
    // 超过 42 字符截断到 42（EVM 地址 0x+40hex 需完整容纳）
    let long = "字".repeat(50);
    let parsed = parse_mentions(&format!("@{long}"));
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].chars().count(), 42, "名字截断到 42 字符");
    // EVM 地址（0x+40hex = 42 字符）完整解析不截断
    let evm = "0xe4461efaca05117631277fbb3c7f7e40e01179fc";
    assert_eq!(parse_mentions(&format!("@{evm}")), vec![evm]);
    // 标点截断：中文顿号后停止
    assert_eq!(parse_mentions("@张三、@李四"), vec!["张三", "李四"]);
    // strip_mentions：剥 @ 片段、全剥空
    assert_eq!(
        strip_mentions("@NexOS助手 帮我总结这份文档", &["NexOS助手".to_string()]),
        "帮我总结这份文档"
    );
    assert_eq!(strip_mentions("@NexOS助手", &["NexOS助手".to_string()]), "");
}

// G3. sender_kind 兼容：缺省 human；agent 放行；垃圾值归一 human
#[tokio::test]
async fn sender_kind_default_agent_and_garbage_normalized() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let base = format!("/api/v1/im/conversations/{cid}/messages");
    // 缺省 → human（存量客户端零迁移）
    let r1 = h
        .handle(authed_post(
            &base,
            &token,
            serde_json::json!({ "content": "hi" }),
        ))
        .await
        .unwrap();
    assert_eq!(r1.body["sender_kind"], "human", "缺省应为 human");
    // agent 放行（外部 agent 自声明）
    let r2 = h
        .handle(authed_post(
            &base,
            &token,
            serde_json::json!({ "content": "agent 说", "sender_kind": "agent" }),
        ))
        .await
        .unwrap();
    assert_eq!(r2.body["sender_kind"], "agent");
    // 垃圾值归一 human（白名单）
    let r3 = h
        .handle(authed_post(
            &base,
            &token,
            serde_json::json!({ "content": "x", "sender_kind": "robot" }),
        ))
        .await
        .unwrap();
    assert_eq!(r3.body["sender_kind"], "human", "白名单外归一 human");
}

// G4. mentions + sender_kind 全链路往返：响应/历史/补拉三处一致
#[tokio::test]
async fn sender_kind_mentions_roundtrip_history_and_catchup() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let sent = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({
                "content": "@NexOS助手 @alice 帮我把这页做成 PPT",
                "sender_kind": "agent"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let m = &sent.body;
    assert_eq!(m["sender_kind"], "agent");
    assert_eq!(
        m["mentions"],
        serde_json::json!(["NexOS助手", "alice"]),
        "服务端应解析 mentions"
    );
    assert_eq!(m["attachment"], serde_json::Value::Null);
    // 历史
    let hist = h
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(hist.body[0]["sender_kind"], "agent");
    assert_eq!(hist.body[0]["mentions"][0], "NexOS助手");
    // 补拉
    let catchup = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={cid}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(catchup.body[0]["sender_kind"], "agent");
    assert_eq!(catchup.body[0]["mentions"][1], "alice");
    // 大厅同语义
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let lobby = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "大厅 @张三 到了吗" }),
        ))
        .await
        .unwrap();
    assert_eq!(lobby.body["sender_kind"], "human");
    assert_eq!(lobby.body["mentions"], serde_json::json!(["张三"]));
}

// G5. 存量行兼容：老列集插入（缺三新列）读回默认 human/[]
#[tokio::test]
async fn legacy_rows_default_to_human() {
    let h = demo_handler();
    {
        let conn = h.shared.db.lock().expect("db poisoned");
        conn.execute(
            "INSERT INTO im_messages (id,conversation_id,sender_id,sender_name,content,msg_type,created_at,read_by)
             VALUES ('legacy-1','conv-general','alice','Alice','旧消息','text','2026-01-02T09:00:00+08:00','[]')",
            [],
        )
        .unwrap();
    }
    let (_, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_get(
            "/api/v1/im/conversations/conv-general/messages",
            &token,
        ))
        .await
        .unwrap();
    let legacy = resp
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "legacy-1")
        .expect("legacy 行应可见");
    assert_eq!(legacy["sender_kind"], "human", "旧行默认 human");
    assert_eq!(legacy["mentions"], serde_json::json!([]));
    assert_eq!(legacy["attachment"], serde_json::Value::Null);
}

// G6. 助手触发（大厅，假 LLM echo）：回复字段全形状 + prompt 剥 @ + 模型名
#[tokio::test]
async fn assistant_lobby_reply_full_shape_with_fake_llm() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let (url, _srv) = spawn_fake_llm(seen.clone()).await;
    let h = empty_handler()
        .with_agent_llm_url(&url)
        .with_agent_model("test-model")
        .with_agent_storm_window(TEST_STORM_WINDOW);
    let (_, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let sent = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "@NexOS助手 帮我总结这份季度报告" }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let trigger_id = sent.body["id"].as_str().unwrap().to_string();
    // 等助手回复落库（窗口 200ms + 本地 echo 秒回）
    let ok = wait_until(Duration::from_secs(5), || {
        !agent_replies(&h, LOBBY_ID).is_empty()
    })
    .await;
    assert!(ok, "助手应在窗口后回复大厅");
    let replies = agent_replies(&h, LOBBY_ID);
    assert_eq!(replies.len(), 1, "单次 @ 应恰有一条回复");
    let r = &replies[0];
    assert_eq!(r.sender_kind, "agent");
    assert_eq!(r.sender_name.as_deref(), Some("NexOS助手"));
    assert_eq!(r.sender_id, "agent:nexos-assistant");
    assert_eq!(r.reply_to.as_deref(), Some(trigger_id.as_str()));
    assert_eq!(
        r.content, "ECHO:帮我总结这份季度报告（AI 生成）",
        "prompt 应剥掉 @，回显带后缀"
    );
    // 请求形状：模型名 + system/user 双消息
    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 1, "只有最后一次触发真调了 LLM");
    assert!(
        reqs[0].contains("\"model\":\"test-model\""),
        "模型名应透传: {}",
        reqs[0]
    );
    assert!(reqs[0].contains("帮我总结这份季度报告"));
    assert!(!reqs[0].contains("@NexOS助手"), "prompt 不应含 @ 片段");
}

// G7. 会话内 @ 同样生效（LLM 不可达 → 固定降级话术）
#[tokio::test]
async fn assistant_conversation_trigger_llm_unreachable_fallback() {
    let h = empty_handler()
        .with_agent_llm_url(DEAD_LLM_URL)
        .with_agent_storm_window(TEST_STORM_WINDOW);
    let (_, token) = login(&h, &new_key()).await;
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "带助手" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let sent = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({ "content": "@NexOS助手 列三个要点" }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let ok = wait_until(Duration::from_secs(5), || {
        !agent_replies(&h, &cid).is_empty()
    })
    .await;
    assert!(ok, "LLM 不可达也应降级回复");
    let replies = agent_replies(&h, &cid);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].conversation_id, cid, "回复落原会话");
    assert_eq!(
        replies[0].content,
        format!("{ASSISTANT_FALLBACK_TEXT}{ASSISTANT_SUFFIX}"),
        "不可达 → 固定话术 + 后缀"
    );
}

// G8. 触发/不触发矩阵：无 @、@他人、agent 消息 → 一律不回复
#[tokio::test]
async fn assistant_no_trigger_matrix() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let (url, _srv) = spawn_fake_llm(seen.clone()).await;
    let h = empty_handler()
        .with_agent_llm_url(&url)
        .with_agent_storm_window(TEST_STORM_WINDOW);
    let (_, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    for (content, kind) in [
        ("普通消息，谁也不@", None),
        ("@alice 帮我个忙", None),                    // @ 他人不触发
        ("@NexOS助手 agent 不应触发", Some("agent")), // agent 消息跳过（防自激）
    ] {
        let mut body = serde_json::json!({ "content": content });
        if let Some(k) = kind {
            body["sender_kind"] = serde_json::json!(k);
        }
        let r = h
            .handle(authed_post(PATH_LOBBY_MESSAGES, &token, body))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
    }
    // 等满 3× 窗口 + 余量 → 仍应零回复、零 LLM 请求
    tokio::time::sleep(TEST_STORM_WINDOW * 4).await;
    assert!(
        agent_replies(&h, LOBBY_ID).is_empty(),
        "无 @ / @他人 / agent 消息都不应触发助手"
    );
    assert!(seen.lock().unwrap().is_empty(), "不应发生任何 LLM 请求");
}

// G9. 防风暴：3s（测试 200ms）窗口内 3 条 @ 只响应最后一条
#[tokio::test]
async fn assistant_storm_window_last_trigger_wins() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let (url, _srv) = spawn_fake_llm(seen.clone()).await;
    let h = empty_handler()
        .with_agent_llm_url(&url)
        .with_agent_storm_window(TEST_STORM_WINDOW);
    let (_, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    for i in 1..=3 {
        let r = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": format!("@NexOS助手 问{i}") }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
    }
    // 恰一条回复，且回复内容对应最后一条（问3）
    let ok = wait_until(Duration::from_secs(5), || {
        agent_replies(&h, LOBBY_ID).len() == 1
    })
    .await;
    assert!(ok, "防风暴后应恰有一条回复");
    tokio::time::sleep(TEST_STORM_WINDOW * 2).await;
    let replies = agent_replies(&h, LOBBY_ID);
    assert_eq!(replies.len(), 1, "窗口内多条 @ 只响应最后一条");
    assert_eq!(replies[0].content, "ECHO:问3（AI 生成）");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "旧代次任务应被去抖放弃，不调 LLM"
    );
}

// G10. 超长回复截断（≤800 字 + 后缀），UTF-8 边界安全
#[tokio::test]
async fn assistant_reply_truncated_to_800_chars() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let (url, _srv) = spawn_fake_llm(seen.clone()).await;
    let h = empty_handler()
        .with_agent_llm_url(&url)
        .with_agent_storm_window(TEST_STORM_WINDOW);
    let (_, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // echo 服务器回 5 + 900 = 905 字符 → 截到 800 + 后缀 7 = 807
    let r = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": format!("@NexOS助手 {}", "长".repeat(900)) }),
        ))
        .await
        .unwrap();
    assert_eq!(r.status, 201);
    let ok = wait_until(Duration::from_secs(5), || {
        !agent_replies(&h, LOBBY_ID).is_empty()
    })
    .await;
    assert!(ok);
    let reply = &agent_replies(&h, LOBBY_ID)[0];
    assert!(reply.content.starts_with("ECHO:长长"));
    assert!(reply.content.ends_with(ASSISTANT_SUFFIX));
    assert_eq!(
        reply.content.chars().count(),
        ASSISTANT_REPLY_MAX_CHARS + ASSISTANT_SUFFIX.chars().count(),
        "正文截到 800 字 + 后缀"
    );
    assert!(!seen.lock().unwrap().is_empty());
}

// G11. 纯函数：truncate_chars UTF-8 边界安全 + normalize_sender_kind
#[test]
fn truncate_and_normalize_pure() {
    assert_eq!(truncate_chars("abcd", 3), "abc");
    assert_eq!(truncate_chars("中文安全", 2), "中文");
    assert_eq!(truncate_chars("短", 10), "短");
    assert_eq!(truncate_chars("😀😀😀", 2), "😀😀", "emoji 按字符截断");
    assert_eq!(normalize_sender_kind(None), "human");
    assert_eq!(normalize_sender_kind(Some("human")), "human");
    assert_eq!(normalize_sender_kind(Some("agent")), "agent");
    assert_eq!(normalize_sender_kind(Some(" agent ")), "agent", "trim 宽容");
    assert_eq!(
        normalize_sender_kind(Some("ADMIN")),
        "human",
        "白名单外归一"
    );
}

// —— F 面：文档传输 ——（上传落盘/超限/净化/下载鉴权/attachment 核对/往返）

/// 上传小工具：登录 → POST /im/files → (file_id, token)。
async fn upload(
    h: &ImRouteHandler,
    token: &str,
    filename: &str,
    bytes: &[u8],
) -> serde_json::Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    h.handle(authed_post(
        PATH_FILES,
        token,
        serde_json::json!({ "filename": filename, "content_base64": b64 }),
    ))
    .await
    .unwrap()
    .body
}

// F1. 上传 → 落盘路径形状（月目录 + uuid 前缀 + 净化名）→ 下载往返
#[tokio::test]
async fn imfile_upload_roundtrip_disk_shape_and_download() {
    let (root_str, root) = temp_files_root("rt");
    let h = empty_handler().with_files_root(&root_str);
    let (_, token) = login(&h, &new_key()).await;
    let payload = b"nexos im file payload \xe4\xb8\xad\xe6\x96\x87".to_vec();
    let up = upload(&h, &token, "季度报告.pptx", &payload).await;
    assert_eq!(up["size_bytes"], payload.len() as u64);
    assert_eq!(
        up["mime"],
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    );
    let file_id = up["file_id"].as_str().unwrap().to_string();
    // 落盘形状：<root>/<YYYYMM>/<uuid>-季度报告.pptx
    let month = chrono::Local::now().format("%Y%m").to_string();
    let dir = root.join(&month);
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert_eq!(entries.len(), 1);
    let stored = entries[0].to_string_lossy().into_owned();
    assert!(
        stored.contains(&format!("{file_id}-季度报告.pptx")),
        "落盘名 = uuid-净化名: {stored}"
    );
    assert_eq!(
        std::fs::metadata(&entries[0]).unwrap().len(),
        payload.len() as u64
    );
    // url 形状：/api/v1/im/files/<id>?token=<token>
    assert_eq!(
        up["url"],
        format!("/api/v1/im/files/{file_id}?token={token}")
    );
    // Bearer 下载往返：base64 信封 + content-disposition
    let dl = h
        .handle(authed_get(&format!("/api/v1/im/files/{file_id}"), &token))
        .await
        .unwrap();
    assert_eq!(dl.status, 200);
    assert_eq!(dl.body["filename"], "季度报告.pptx");
    assert_eq!(dl.body["encoding"], "base64");
    assert_eq!(dl.body["size_bytes"], payload.len() as u64);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(dl.body["content_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, payload, "下载内容逐字节一致");
    let cd = dl.headers["content-disposition"].as_str().unwrap();
    assert!(cd.starts_with("attachment; filename=\""));
    assert!(cd.contains("filename*=UTF-8''"), "RFC 5987: {cd}");
    let _ = std::fs::remove_dir_all(&root);
}

// F2. 超限 413（base64 长度前置估算，不实际解码）+ store_im_file 解码后闸门
#[tokio::test]
async fn imfile_upload_rejects_oversize() {
    let (root_str, root) = temp_files_root("over");
    let h = empty_handler().with_files_root(&root_str);
    let (_, token) = login(&h, &new_key()).await;
    // 长度使 len/4*3 恰超 64MiB（约 86MiB 字符串——只测闸门不解码）
    let huge_b64 = "A".repeat(IM_FILE_MAX_BYTES / 3 * 4 + 16);
    let resp = h
        .handle(authed_post(
            PATH_FILES,
            &token,
            serde_json::json!({ "filename": "huge.bin", "content_base64": huge_b64 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 413, "前置估算即拒");
    // 解码后闸门（spawn_blocking 路径的兜底；API 面先被前置拦截）
    let big = vec![b'x'; IM_FILE_MAX_BYTES + 1];
    let err = store_im_file(&root, "x.bin", &big).unwrap_err();
    assert_eq!(err.0, 413);
    // 缺字段 / 坏 base64 → 400
    let miss = h
        .handle(authed_post(
            PATH_FILES,
            &token,
            serde_json::json!({ "filename": "a" }),
        ))
        .await
        .unwrap();
    assert_eq!(miss.status, 400);
    let bad = h
        .handle(authed_post(
            PATH_FILES,
            &token,
            serde_json::json!({ "filename": "a.txt", "content_base64": "@@@not-base64@@@" }),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status, 400);
    // 无 token → 401
    let anon = h
        .handle(post_req(
            PATH_FILES,
            serde_json::json!({ "filename": "a", "content_base64": "YQ==" }),
        ))
        .await
        .unwrap();
    assert_eq!(anon.status, 401);
    let _ = std::fs::remove_dir_all(&root);
}

// F3. 文件名净化：路径分隔/穿越/控制字符 → `_`；全非法回退 file
#[tokio::test]
async fn imfile_filename_sanitization() {
    assert_eq!(sanitize_im_filename("../evil/x.sh"), ".._evil_x.sh");
    assert_eq!(sanitize_im_filename("a\\b\\c.txt"), "a_b_c.txt");
    assert_eq!(
        sanitize_im_filename("报告 最终版(1).pptx"),
        "报告 最终版(1).pptx"
    );
    assert_eq!(sanitize_im_filename("  \n\t  "), "file", "全非法/空白回退");
    assert_eq!(sanitize_im_filename(""), "file");
    let long = "a".repeat(300);
    assert_eq!(
        sanitize_im_filename(&long).chars().count(),
        120,
        "截到 120 字符"
    );
    // 端到端：恶意名落盘后无路径穿越
    let (root_str, root) = temp_files_root("san");
    let h = empty_handler().with_files_root(&root_str);
    let (_, token) = login(&h, &new_key()).await;
    let up = upload(&h, &token, "../../etc/passwd", b"x").await;
    let file_id = up["file_id"].as_str().unwrap().to_string();
    assert_eq!(up["filename"], ".._.._etc_passwd", "净化后展示名");
    let month = chrono::Local::now().format("%Y%m").to_string();
    let dir = root.join(&month);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n.contains(&file_id) && n.contains(".._.._etc_passwd")),
        "落盘单段名: {names:?}"
    );
    // root 外无泄漏
    assert!(!root.parent().unwrap().join("etc").exists());
    let _ = std::fs::remove_dir_all(&root);
}

// F4. 下载鉴权矩阵：无/坏 token 401；query IM token / Bearer / admin token 200；
//     未知 id 404
#[tokio::test]
async fn imfile_download_auth_matrix() {
    let (root_str, root) = temp_files_root("auth");
    let h = empty_handler()
        .with_files_root(&root_str)
        .with_admin_token("adm-tk-1");
    let (_, token) = login(&h, &new_key()).await;
    let up = upload(&h, &token, "a.txt", b"hello").await;
    let file_id = up["file_id"].as_str().unwrap().to_string();
    // 无 token → 401
    let anon = h
        .handle(get_req(&format!("/api/v1/im/files/{file_id}")))
        .await
        .unwrap();
    assert_eq!(anon.status, 401);
    // 坏 token → 401
    let bad = h
        .handle(get_req(&format!(
            "/api/v1/im/files/{file_id}?token={}",
            "0".repeat(64)
        )))
        .await
        .unwrap();
    assert_eq!(bad.status, 401);
    // query IM token（直链场景）→ 200
    let q = h
        .handle(get_req(&format!(
            "/api/v1/im/files/{file_id}?token={token}"
        )))
        .await
        .unwrap();
    assert_eq!(q.status, 200);
    assert_eq!(q.body["filename"], "a.txt");
    // Bearer 头 → 200
    let b = h
        .handle(authed_get(&format!("/api/v1/im/files/{file_id}"), &token))
        .await
        .unwrap();
    assert_eq!(b.status, 200);
    // admin token → 200
    let adm = h
        .handle(get_req(&format!(
            "/api/v1/im/files/{file_id}?token=adm-tk-1"
        )))
        .await
        .unwrap();
    assert_eq!(adm.status, 200);
    // 未知 id → 404
    let miss = h
        .handle(get_req("/api/v1/im/files/no-such-id"))
        .await
        .unwrap();
    assert_eq!(miss.status, 404);
    let _ = std::fs::remove_dir_all(&root);
}

// F5. attachment 核对：伪造 size/filename 被服务端真值覆盖；未知 file_id 400
#[tokio::test]
async fn attachment_verified_against_server_truth() {
    let (root_str, root) = temp_files_root("att");
    let h = empty_handler().with_files_root(&root_str);
    let (_, token) = login(&h, &new_key()).await;
    let payload = b"0123456789".to_vec(); // 10 字节
    let up = upload(&h, &token, "真名.docx", &payload).await;
    let file_id = up["file_id"].as_str().unwrap().to_string();
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    // 伪造 size_bytes=1 / filename="forged" → 服务端覆盖为真值
    let sent = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({
                "content": "见附件",
                "attachment": {
                    "file_id": file_id,
                    "filename": "forged.exe",
                    "size_bytes": 1
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    assert_eq!(sent.body["attachment"]["file_id"], file_id);
    assert_eq!(
        sent.body["attachment"]["filename"], "真名.docx",
        "伪造文件名被覆盖"
    );
    assert_eq!(
        sent.body["attachment"]["size_bytes"], 10,
        "伪造 size 被覆盖"
    );
    assert_eq!(
        sent.body["attachment"]["mime"],
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    );
    // 历史与补拉同真值
    let hist = h
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(hist.body[0]["attachment"]["size_bytes"], 10);
    let catchup = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={cid}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(catchup.body[0]["attachment"]["filename"], "真名.docx");
    // 未知 file_id → 400
    let bad = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({
                "content": "坏附件",
                "attachment": { "file_id": "no-such", "size_bytes": 99 }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status, 400, "未知附件应 400");
    assert!(bad.body["error"].as_str().unwrap().contains("不存在"));
    let _ = std::fs::remove_dir_all(&root);
}

// F6. WS 广播帧携带新字段：sender_kind / mentions / attachment（大厅帧）
#[tokio::test]
async fn ws_frame_carries_agent_fields_and_attachment() {
    let (root_str, root) = temp_files_root("ws");
    let hub = WsHub::default();
    let h = ImRouteHandler::with_empty_ws(hub.clone(), Arc::new(ImAuth::default()))
        .with_files_root(&root_str);
    let (_, token) = login(&h, &new_key()).await;
    let up = upload(&h, &token, "slides.pptx", b"pptx-bytes").await;
    let file_id = up["file_id"].as_str().unwrap().to_string();
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // GET /lobby 已触发欢迎帧广播——之后才订阅，首帧即我们的消息
    let (_sub, mut rx) = hub.subscribe_raw("probe");
    // 大厅消息（避开 @NexOS助手 免触发助手；@alice 仅验证 mentions 透传）
    let sent = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({
                "content": "@alice 请看附件",
                "sender_kind": "agent",
                "attachment": { "file_id": file_id, "size_bytes": 999 }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("WS 帧不应超时")
        .expect("通道不应关闭");
    let v = serde_json::to_value(&frame).unwrap();
    assert_eq!(v["type"], "im_lobby_message");
    assert_eq!(v["lobby_id"], "lobby");
    assert_eq!(v["message"]["sender_kind"], "agent", "帧透传 sender_kind");
    assert_eq!(v["message"]["mentions"], serde_json::json!(["alice"]));
    assert_eq!(
        v["message"]["attachment"]["size_bytes"], 10,
        "帧内附件为服务端真值（伪造 999 被覆盖）"
    );
    assert_eq!(v["message"]["attachment"]["file_id"], file_id);
    assert_eq!(v["message"]["attachment"]["filename"], "slides.pptx");
    let _ = std::fs::remove_dir_all(&root);
}

// F7. mime 猜测 + Content-Disposition 纯函数
#[test]
fn imfile_mime_and_disposition_pure() {
    assert_eq!(
        guess_mime_im("a.pptx"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    );
    assert_eq!(guess_mime_im("a.pdf"), "application/pdf");
    assert_eq!(guess_mime_im("noext"), "application/octet-stream");
    let cd = content_disposition_im("中文 名.pptx");
    assert!(cd.contains("filename=\"______.pptx\"") || cd.contains("filename=\""));
    assert!(cd.contains("filename*=UTF-8''"), "RFC 5987 编码段: {cd}");
    assert!(cd.contains("%E4%B8%AD"), "中文按 UTF-8 百分号编码: {cd}");
}

// =========================================================================
// 消息推送通知 webhook（2026-08-22）单元测 —— N 面
// —— 注册归因/owner 过滤/注销权限/大厅与会话触发/事件过滤/超时不阻塞/
//    连败自动注销/无 token 泄漏/纯函数
// =========================================================================

/// 假 webhook 接收端应答模式：Ok（回 200）/ Hang（收下不回——客户端等满
/// 5s 超时，用于验证消息路径不被阻塞）。
#[derive(Clone, Copy)]
enum FakeHookMode {
    Ok,
    Hang,
}

/// 假 webhook 接收端（本地 TcpListener 手写 HTTP/1.1，spawn_fake_llm 同款
/// 手法）：逐请求把**原始请求文本**（请求行+headers+body）记进 seen，
/// 按 mode 应答；至多服务 8 个连接。返回完整接收端点 url。
async fn spawn_webhook_receiver(seen: Arc<StdMutex<Vec<String>>>, mode: FakeHookMode) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..8 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut acc: Vec<u8> = Vec::new();
            let mut buf = [0u8; 16384];
            // 按 Content-Length 判断请求体收完
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
                let head = String::from_utf8_lossy(&acc).into_owned();
                if let Some(pos) = head.find("\r\n\r\n") {
                    let cl = head[..pos]
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if acc.len() >= pos + 4 + cl {
                        break;
                    }
                }
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&acc).into_owned());
            match mode {
                FakeHookMode::Ok => {
                    let resp =
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
                FakeHookMode::Hang => {
                    // 收下不回：让客户端等满 5s 超时（测试只关心发消息端
                    // 不被阻塞；测试结束 runtime 回收本任务）
                    tokio::time::sleep(Duration::from_secs(600)).await;
                }
            }
        }
    });
    format!("http://{addr}/agent-hook")
}

/// 带 IM token 的 DELETE。
fn authed_delete(path: &str, token: &str) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Delete,
        path: path.into(),
        headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
        body: serde_json::Value::Null,
        auth: None,
    }
}

/// 注册小工具：POST /notify/register，返回响应。
async fn notify_register(
    h: &ImRouteHandler,
    token: &str,
    url: &str,
    events: Option<Vec<&str>>,
    conversation_id: Option<&str>,
) -> ApiResponse {
    let mut body = serde_json::json!({ "url": url });
    if let Some(ev) = events {
        body["events"] = serde_json::json!(ev);
    }
    if let Some(cid) = conversation_id {
        body["conversation_id"] = serde_json::json!(cid);
    }
    h.handle(authed_post(PATH_NOTIFY_REGISTER, token, body))
        .await
        .unwrap()
}

/// 直查 im_webhooks 行（测试轮询派发结果用）。
fn webhook_row(h: &ImRouteHandler, id: &str) -> Option<ImWebhook> {
    let conn = h.shared.db.lock().expect("db poisoned");
    find_webhook(&conn, id).unwrap_or(None)
}

/// 必然投递失败的接收端点（127.0.0.1:1 无服务 → 秒级 ECONNREFUSED）。
const DEAD_HOOK_URL: &str = "http://127.0.0.1:1/agent-hook";

// N1. 注册归因：owner = token pubkey；缺省 events 双开；非法 url/events 400；
//     conversation_id 未知 404 / 非成员群组 403 / 空串 400；无 token 401
#[tokio::test]
async fn notify_register_attribution_and_validation() {
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    // 无 token → 401
    let anon = h
        .handle(post_req(
            PATH_NOTIFY_REGISTER,
            serde_json::json!({ "url": "http://127.0.0.1:9/x" }),
        ))
        .await
        .unwrap();
    assert_eq!(anon.status, 401);
    // 正常注册：owner 归因 + 缺省 events 双开
    let resp = notify_register(&h, &token, "http://127.0.0.1:9900/hook", None, None).await;
    assert_eq!(resp.status, 201, "注册应 201: {}", resp.body);
    assert!(!resp.body["id"].as_str().unwrap().is_empty());
    assert_eq!(resp.body["owner_pubkey"], pubkey, "owner 应为 token pubkey");
    assert_eq!(
        resp.body["events"],
        serde_json::json!(["lobby", "conversation"]),
        "缺省 events = 双开"
    );
    assert_eq!(resp.body["status"], "active");
    assert_eq!(resp.body["fail_count"], 0);
    assert_eq!(resp.body["last_fired_at"], serde_json::Value::Null);
    assert_eq!(resp.body["conversation_id"], serde_json::Value::Null);
    // events 部分订阅合法；非法值/空数组 → 400
    let only_lobby = notify_register(
        &h,
        &token,
        "http://127.0.0.1:9900/hook2",
        Some(vec!["lobby"]),
        None,
    )
    .await;
    assert_eq!(only_lobby.status, 201);
    assert_eq!(only_lobby.body["events"], serde_json::json!(["lobby"]));
    for bad_events in [vec!["nope"], vec!["lobby", "bogus"], Vec::<&str>::new()] {
        let r = notify_register(
            &h,
            &token,
            "http://127.0.0.1:9900/hook3",
            Some(bad_events.clone()),
            None,
        )
        .await;
        assert_eq!(r.status, 400, "events={bad_events:?} 应 400");
    }
    // 非法 url → 400
    for bad_url in ["ftp://x/y", "", "not-a-url", "http://"] {
        let r = notify_register(&h, &token, bad_url, None, None).await;
        assert_eq!(r.status, 400, "url={bad_url} 应 400");
    }
    // conversation_id：未知会话 404；空串 400
    let miss = notify_register(&h, &token, "http://127.0.0.1:9900/h", None, Some("no-such")).await;
    assert_eq!(miss.status, 404);
    let empty_cid = notify_register(&h, &token, "http://127.0.0.1:9900/h", None, Some("")).await;
    assert_eq!(empty_cid.status, 400);
    // 非成员群组 → 403（与离线补拉同款 member 门）
    let (_, token2) = login(&h, &new_key()).await;
    let g = h
        .handle(authed_post(
            PATH_GROUPS,
            &token2,
            serde_json::json!({ "name": "私密群" }),
        ))
        .await
        .unwrap();
    let gid = g.body["id"].as_str().unwrap().to_string();
    let denied = notify_register(&h, &token, "http://127.0.0.1:9900/h", None, Some(&gid)).await;
    assert_eq!(denied.status, 403, "非群组成员注册该会话 webhook 应 403");
}

// N2. list owner 过滤：各自只见自己的；无 token 401
#[tokio::test]
async fn notify_list_owner_filter() {
    let h = empty_handler();
    let (pubkey1, token1) = login(&h, &new_key()).await;
    let (pubkey2, token2) = login(&h, &new_key()).await;
    let r1 = notify_register(&h, &token1, "http://127.0.0.1:9901/a", None, None).await;
    let r2 = notify_register(&h, &token1, "http://127.0.0.1:9901/b", None, None).await;
    let r3 = notify_register(&h, &token2, "http://127.0.0.1:9901/c", None, None).await;
    assert_eq!(r1.status, 201);
    assert_eq!(r2.status, 201);
    assert_eq!(r3.status, 201);
    // 用户 1：恰 2 条，全部归因自己
    let l1 = h
        .handle(authed_get(PATH_NOTIFY_LIST, &token1))
        .await
        .unwrap();
    let arr = l1.body.as_array().unwrap();
    assert_eq!(arr.len(), 2, "用户 1 只看到自己的 2 条");
    assert!(arr.iter().all(|w| w["owner_pubkey"] == pubkey1));
    // 用户 2：恰 1 条
    let l2 = h
        .handle(authed_get(PATH_NOTIFY_LIST, &token2))
        .await
        .unwrap();
    let arr2 = l2.body.as_array().unwrap();
    assert_eq!(arr2.len(), 1);
    assert_eq!(arr2[0]["owner_pubkey"], pubkey2);
    assert_eq!(arr2[0]["url"], "http://127.0.0.1:9901/c");
    // 无 token → 401
    let anon = h.handle(get_req(PATH_NOTIFY_LIST)).await.unwrap();
    assert_eq!(anon.status, 401);
}

// N3. 注销权限矩阵：非 owner 403（注册表不动）；owner 200 后消失；
//     再删 404；未知 id 404；无 token 401
#[tokio::test]
async fn notify_unregister_owner_only() {
    let h = empty_handler();
    let (_, token1) = login(&h, &new_key()).await;
    let (_, token2) = login(&h, &new_key()).await;
    let r = notify_register(&h, &token1, "http://127.0.0.1:9902/a", None, None).await;
    let wid = r.body["id"].as_str().unwrap().to_string();
    // 无 token → 401
    let anon = h
        .handle(ApiRequest {
            method: HttpMethod::Delete,
            path: format!("/api/v1/im/notify/{wid}"),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        })
        .await
        .unwrap();
    assert_eq!(anon.status, 401);
    // 他人注销 → 403，注册表不动
    let denied = h
        .handle(authed_delete(&format!("/api/v1/im/notify/{wid}"), &token2))
        .await
        .unwrap();
    assert_eq!(denied.status, 403, "非 owner 注销应 403");
    assert!(webhook_row(&h, &wid).is_some(), "403 后行应保留");
    // owner 注销 → 200；列表清空
    let ok = h
        .handle(authed_delete(&format!("/api/v1/im/notify/{wid}"), &token1))
        .await
        .unwrap();
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body["deleted"], true);
    assert!(webhook_row(&h, &wid).is_none(), "行应已删除");
    let list = h
        .handle(authed_get(PATH_NOTIFY_LIST, &token1))
        .await
        .unwrap();
    assert_eq!(list.body.as_array().unwrap().len(), 0);
    // 重复注销 / 未知 id → 404
    let again = h
        .handle(authed_delete(&format!("/api/v1/im/notify/{wid}"), &token1))
        .await
        .unwrap();
    assert_eq!(again.status, 404);
    let miss = h
        .handle(authed_delete("/api/v1/im/notify/no-such", &token1))
        .await
        .unwrap();
    assert_eq!(miss.status, 404);
}

// N4. 大厅消息触发 webhook：假接收端收到完整 Message JSON
//     （sender_kind/mentions/attachment 真值）+ X-NexOS-Event 头；
//     投递成功后 fail_count=0 + last_fired_at 落位
#[tokio::test]
async fn notify_lobby_message_dispatches_full_payload() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
    let (root_str, root) = temp_files_root("notify");
    let h = empty_handler().with_files_root(&root_str);
    let (pubkey, token) = login(&h, &new_key()).await;
    let reg = notify_register(&h, &token, &url, None, None).await;
    assert_eq!(reg.status, 201);
    let wid = reg.body["id"].as_str().unwrap().to_string();
    // 进大厅 + 带附件 + @ + agent 自声明发一条
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let up = upload(&h, &token, "路演.pptx", b"pptx").await;
    let file_id = up["file_id"].as_str().unwrap().to_string();
    let sent = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({
                "content": "@alice 请看附件",
                "sender_kind": "agent",
                "attachment": { "file_id": file_id, "size_bytes": 999 }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let msg_id = sent.body["id"].as_str().unwrap().to_string();
    // 等投递到达（异步 spawn）
    let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
    assert!(ok, "webhook 应收到大厅消息推送");
    let raw = seen.lock().unwrap()[0].clone();
    assert!(
        raw.starts_with("POST /agent-hook HTTP/1.1"),
        "应为 POST 到注册端点: {raw}"
    );
    assert!(
        raw.to_ascii_lowercase()
            .contains(&"x-nexos-event: lobby_message".to_ascii_lowercase()),
        "事件头应为 lobby_message: {raw}"
    );
    assert!(
        !raw.contains("authorization"),
        "不应携带任何 Authorization 头"
    );
    // body = 完整 Message JSON（附件真值覆盖 + mentions + sender_kind）
    let body_json: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or(""))
            .expect("请求体应为合法 JSON");
    assert_eq!(body_json["id"], msg_id);
    assert_eq!(body_json["conversation_id"], "lobby");
    assert_eq!(body_json["sender_id"], pubkey);
    assert_eq!(body_json["sender_kind"], "agent");
    assert_eq!(body_json["mentions"], serde_json::json!(["alice"]));
    assert_eq!(body_json["attachment"]["file_id"], file_id);
    assert_eq!(body_json["attachment"]["filename"], "路演.pptx");
    assert_eq!(
        body_json["attachment"]["size_bytes"], 4,
        "附件 size 为落盘真值"
    );
    // 投递成功：fail_count=0 + last_fired_at 落位
    let ok = wait_until(Duration::from_secs(5), || {
        webhook_row(&h, &wid)
            .as_ref()
            .is_some_and(|w| w.last_fired_at.is_some())
    })
    .await;
    assert!(ok, "投递成功应记 last_fired_at");
    let w = webhook_row(&h, &wid).unwrap();
    assert_eq!(w.fail_count, 0);
    assert!(w.last_error.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

// N5. conversation 过滤：绑定 conv-1 的 webhook 只收 conv-1 的消息
#[tokio::test]
async fn notify_conversation_filter_pinned_only() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let c1 = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c1" }),
        ))
        .await
        .unwrap();
    let cid1 = c1.body["id"].as_str().unwrap().to_string();
    let c2 = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c2" }),
        ))
        .await
        .unwrap();
    let cid2 = c2.body["id"].as_str().unwrap().to_string();
    // 只订阅 conversation 事件 + 绑定 cid1
    let reg = notify_register(&h, &token, &url, Some(vec!["conversation"]), Some(&cid1)).await;
    assert_eq!(reg.status, 201, "{}", reg.body);
    // cid1 消息 → 推送
    let to1 = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid1}/messages"),
            &token,
            serde_json::json!({ "content": "给 conv1" }),
        ))
        .await
        .unwrap();
    assert_eq!(to1.status, 201);
    let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
    assert!(ok, "绑定的会话消息应推送");
    assert!(seen.lock().unwrap()[0].contains("给 conv1"));
    assert!(seen.lock().unwrap()[0].contains("x-nexos-event: conversation_message"));
    // cid2 消息 → 不推送（负向断言：留足派发窗口后仍只有 1 条）
    let to2 = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid2}/messages"),
            &token,
            serde_json::json!({ "content": "给 conv2" }),
        ))
        .await
        .unwrap();
    assert_eq!(to2.status, 201);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(seen.lock().unwrap().len(), 1, "非绑定会话的消息不应推送");
}

// N6. 事件过滤：lobby-only 不收会话消息；conversation-only 不收大厅消息
#[tokio::test]
async fn notify_event_filter_lobby_vs_conversation() {
    let seen_lobby = Arc::new(StdMutex::new(Vec::new()));
    let seen_conv = Arc::new(StdMutex::new(Vec::new()));
    let url_lobby = spawn_webhook_receiver(seen_lobby.clone(), FakeHookMode::Ok).await;
    let url_conv = spawn_webhook_receiver(seen_conv.clone(), FakeHookMode::Ok).await;
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let r1 = notify_register(&h, &token, &url_lobby, Some(vec!["lobby"]), None).await;
    let r2 = notify_register(&h, &token, &url_conv, Some(vec!["conversation"]), None).await;
    assert_eq!(r1.status, 201);
    assert_eq!(r2.status, 201);
    // 会话消息：只有 conversation-only 收到
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let conv_msg = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({ "content": "会话消息" }),
        ))
        .await
        .unwrap();
    assert_eq!(conv_msg.status, 201);
    let ok = wait_until(Duration::from_secs(5), || {
        !seen_conv.lock().unwrap().is_empty()
    })
    .await;
    assert!(ok, "conversation-only webhook 应收到会话消息");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        seen_lobby.lock().unwrap().is_empty(),
        "lobby-only webhook 不应收会话消息"
    );
    // 大厅消息：只有 lobby-only 收到
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let lobby_msg = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "大厅消息" }),
        ))
        .await
        .unwrap();
    assert_eq!(lobby_msg.status, 201);
    let ok = wait_until(Duration::from_secs(5), || {
        !seen_lobby.lock().unwrap().is_empty()
    })
    .await;
    assert!(ok, "lobby-only webhook 应收到大厅消息");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        seen_conv.lock().unwrap().len(),
        1,
        "conversation-only webhook 不应收大厅消息"
    );
}

// N7. 超时不阻塞消息路径：接收端收下不回（触发 5s 超时），发消息的
//     201 仍秒回（远小于 5s）
#[tokio::test]
async fn notify_timeout_does_not_block_message_path() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Hang).await;
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let reg = notify_register(&h, &token, &url, None, None).await;
    assert_eq!(reg.status, 201);
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // 计时发消息：201 必须在 webhook 5s 超时之前返回
    let started = Instant::now();
    let sent = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "不阻塞我" }),
        ))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(sent.status, 201);
    assert!(
        elapsed < Duration::from_secs(3),
        "消息响应应秒回（实际 {elapsed:?}，webhook 超时 5s）"
    );
    // 接收端确实收到了请求（挂起中）——证明派发真实发生只是不等它
    let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
    assert!(ok, "挂起接收端应已收到投递请求");
}

// N8. 连败 5 次自动注销：死端口注册 → 5 条消息 5 连败 → status=disabled
//     + last_error 记录；第 6 条消息不再尝试（fail_count 停在 5）
#[tokio::test]
async fn notify_auto_deregister_after_consecutive_failures() {
    let h = empty_handler();
    let (_, token) = login(&h, &new_key()).await;
    let reg = notify_register(&h, &token, DEAD_HOOK_URL, None, None).await;
    assert_eq!(reg.status, 201);
    let wid = reg.body["id"].as_str().unwrap().to_string();
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    // 5 条消息 → 5 次秒败（ECONNREFUSED）→ 自动注销
    for i in 1..=5 {
        let r = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": format!("第{i}条") }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
    }
    let ok = wait_until(Duration::from_secs(5), || {
        webhook_row(&h, &wid)
            .as_ref()
            .is_some_and(|w| w.status == "disabled")
    })
    .await;
    assert!(ok, "连败 5 次应自动注销（status=disabled）");
    let w = webhook_row(&h, &wid).unwrap();
    assert_eq!(w.fail_count, 5, "连败计数恰为 5");
    assert!(
        w.last_error.as_deref().unwrap_or("").contains("自动注销"),
        "last_error 应记录注销原因: {:?}",
        w.last_error
    );
    // 注销后不再尝试：第 6 条消息不推进 fail_count
    let r = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "第六条" }),
        ))
        .await
        .unwrap();
    assert_eq!(r.status, 201);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let w = webhook_row(&h, &wid).unwrap();
    assert_eq!(w.fail_count, 5, "注销后不再派发，连败计数应停在 5");
    // 注册表仍在（owner 可见注销原因），重新注册同 url 即恢复
    let list = h
        .handle(authed_get(PATH_NOTIFY_LIST, &token))
        .await
        .unwrap();
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    assert_eq!(list.body[0]["status"], "disabled");
}

// N9. 推送 body 不含敏感 token：请求原文（头+体）找不到发送者的
//     IM token / admin token 字样；body 键集合 ⊆ Message 字段
#[tokio::test]
async fn notify_payload_carries_no_sensitive_token() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
    let h = empty_handler().with_admin_token("adm-secret-1");
    let (_, token) = login(&h, &new_key()).await;
    let reg = notify_register(&h, &token, &url, None, None).await;
    assert_eq!(reg.status, 201);
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let sent = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "机密测试" }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
    assert!(ok);
    let raw = seen.lock().unwrap()[0].clone();
    assert!(!raw.contains(&token), "推送不得泄露发送者 IM token: {raw}");
    assert!(!raw.contains("adm-secret-1"), "推送不得泄露 admin token");
    assert!(!raw.contains("authorization"), "不应带 Authorization 头");
    // body 键集合 = Message DTO 字段（无任何凭证类键）
    let body: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for k in &keys {
        assert!(
            matches!(
                *k,
                "id" | "conversation_id"
                    | "sender_id"
                    | "sender_name"
                    | "content"
                    | "msg_type"
                    | "file_url"
                    | "reply_to"
                    | "created_at"
                    | "read_by"
                    | "sender_kind"
                    | "mentions"
                    | "attachment"
            ),
            "payload 出现非 Message 字段: {k}"
        );
    }
}

// N10. 纯函数矩阵：url 校验 / 事件名 / 匹配判定 / events 归一
#[test]
fn notify_pure_functions_matrix() {
    // url 校验
    assert!(is_valid_webhook_url("http://127.0.0.1:9000/hook"));
    assert!(is_valid_webhook_url("https://agent.example.com/im"));
    assert!(!is_valid_webhook_url("ftp://x/y"));
    assert!(!is_valid_webhook_url(""));
    assert!(!is_valid_webhook_url(&format!(
        "http://x/{}",
        "a".repeat(2100)
    )));
    // 事件名
    assert_eq!(webhook_event_name(LOBBY_ID), "lobby_message");
    assert_eq!(webhook_event_name("conv-1"), "conversation_message");
    // events 归一：合法去重 / 非法 None / 空 None
    assert_eq!(
        normalize_webhook_events(&["lobby".into(), "lobby".into(), "conversation".into()]),
        Some(vec!["lobby".to_string(), "conversation".to_string()])
    );
    assert_eq!(normalize_webhook_events(&["nope".into()]), None);
    assert_eq!(normalize_webhook_events(&[]), None);
    // 匹配矩阵
    let mk = |events: &[&str], cid: Option<&str>, status: &str| ImWebhook {
        id: "w1".into(),
        url: "http://127.0.0.1:9/h".into(),
        owner_pubkey: "pk".into(),
        events: events.iter().map(|s| s.to_string()).collect(),
        conversation_id: cid.map(str::to_string),
        status: status.into(),
        fail_count: 0,
        last_fired_at: None,
        last_error: None,
        created_at: "t".into(),
    };
    let lobby_msg: Message = serde_json::from_value(serde_json::json!({
        "id": "m1", "conversation_id": LOBBY_ID, "sender_id": "a",
        "content": "hi", "created_at": "t"
    }))
    .unwrap();
    let conv_msg: Message = serde_json::from_value(serde_json::json!({
        "id": "m2", "conversation_id": "conv-1", "sender_id": "a",
        "content": "hi", "created_at": "t"
    }))
    .unwrap();
    // disabled 永不匹配
    assert!(!webhook_matches(
        &mk(&["lobby", "conversation"], None, "disabled"),
        &lobby_msg
    ));
    // 大厅消息：订阅 lobby 才收；conversation_id 绑定不影响大厅
    assert!(webhook_matches(&mk(&["lobby"], None, "active"), &lobby_msg));
    assert!(webhook_matches(
        &mk(&["lobby"], Some("conv-1"), "active"),
        &lobby_msg
    ));
    assert!(!webhook_matches(
        &mk(&["conversation"], None, "active"),
        &lobby_msg
    ));
    // 会话消息：订阅 conversation 才收；绑定须一致
    assert!(webhook_matches(
        &mk(&["conversation"], None, "active"),
        &conv_msg
    ));
    assert!(webhook_matches(
        &mk(&["conversation"], Some("conv-1"), "active"),
        &conv_msg
    ));
    assert!(!webhook_matches(
        &mk(&["conversation"], Some("conv-2"), "active"),
        &conv_msg
    ));
    assert!(!webhook_matches(&mk(&["lobby"], None, "active"), &conv_msg));
    // 双开全收
    assert!(webhook_matches(
        &mk(&["lobby", "conversation"], None, "active"),
        &conv_msg
    ));
}

// N11. 会话消息触发：pinned webhook 收到 conversation_message 头 + 完整 body
//      （覆盖 N5 未验的头部/会话 id 断言）
#[tokio::test]
async fn notify_conversation_message_event_header() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    let c = h
        .handle(authed_post(
            PATH_CONV_LIST,
            &token,
            serde_json::json!({ "name": "c" }),
        ))
        .await
        .unwrap();
    let cid = c.body["id"].as_str().unwrap().to_string();
    let reg = notify_register(&h, &token, &url, Some(vec!["conversation"]), Some(&cid)).await;
    assert_eq!(reg.status, 201);
    let sent = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &token,
            serde_json::json!({ "content": "会话推送", "sender_kind": "agent" }),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status, 201);
    let msg_id = sent.body["id"].as_str().unwrap().to_string();
    let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
    assert!(ok);
    let raw = seen.lock().unwrap()[0].clone();
    assert!(raw.contains("x-nexos-event: conversation_message"), "{raw}");
    let body: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
    assert_eq!(body["id"], msg_id);
    assert_eq!(body["conversation_id"], cid);
    assert_eq!(body["sender_id"], pubkey);
    assert_eq!(body["sender_kind"], "agent");
}

// ---- P3 联邦大厅（docs/NEXOS_P2P_NETWORK_DESIGN.md §8）----

/// 人类大厅消息 fixture（pubkey 发送者）。
fn human_lobby_msg(id: &str, sender: &str, content: &str) -> Message {
    Message {
        id: id.to_string(),
        conversation_id: LOBBY_ID.to_string(),
        sender_id: sender.to_string(),
        sender_name: Some("远程用户".to_string()),
        content: content.to_string(),
        msg_type: "text".to_string(),
        file_url: None,
        reply_to: None,
        created_at: "2026-08-22T10:00:00+08:00".to_string(),
        read_by: vec![sender.to_string()],
        sender_kind: "human".to_string(),
        mentions: Vec::new(),
        attachment: None,
    }
}

// 1. 联邦纯函数：载荷形状 + federable 裁决（agent/系统不联邦，human 联邦）
#[test]
fn fed_payload_shape_and_federable_rules() {
    let msg = human_lobby_msg("m-1", "0xabc", "hello fed");
    assert!(lobby_message_federable(&msg), "人类消息应联邦");
    let payload = build_im_lobby_fed_payload("node-106", &msg);
    assert_eq!(payload["fed"], FED_KIND_IM_LOBBY);
    assert_eq!(payload["node"], "node-106");
    assert_eq!(payload["message"]["id"], "m-1");
    assert_eq!(payload["message"]["content"], "hello fed");
    assert_eq!(payload["message"]["sender_id"], "0xabc");
    // agent 消息（助手回复）不联邦——联邦网内不重复 AI 回答
    let mut agent = msg.clone();
    agent.sender_kind = "agent".to_string();
    agent.sender_id = "agent:nexos-assistant".to_string();
    assert!(!lobby_message_federable(&agent), "agent 消息不联邦");
    // 系统欢迎消息不联邦（入廊是本地事件）
    let welcome = build_welcome_message("alice");
    assert!(!lobby_message_federable(&welcome), "欢迎消息不联邦");
    let mut sys = msg.clone();
    sys.msg_type = "system".to_string();
    assert!(!lobby_message_federable(&sys), "system 类型不联邦");
}

// 2. P2P 未启用：POST /lobby/messages 与 POST /fed-lobby/messages 均照常
//    201（本地写入不受联邦影响；federate 显式调用静默跳过返回 false）
#[tokio::test]
async fn fed_post_lobby_without_p2p_silently_skips() {
    let h = empty_handler();
    assert!(!h.federation().is_federated(), "未注入 P2P");
    let (_pubkey, token) = login(&h, &new_key()).await;
    // 自动加入大厅（两个大厅端点共用在场表）
    let resp = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    let resp = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "无 P2P 也能发" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "未启用 P2P 时发消息照常 201");
    // 联邦大厅发言（GET /fed-lobby 自动加入在场表）
    let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({ "content": "联邦大厅无 P2P 也能发" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "未启用 P2P 时联邦大厅发言照常 201");
    // federate 显式调用也返回 false（静默跳过）
    let msg: Message = serde_json::from_value(resp.body).unwrap();
    assert!(
        !h.federation().federate_fed_lobby_message(&msg).await,
        "无 Handle 时广播跳过"
    );
}

// 3. 联邦接收：新旧两种 fed 载荷 → Written，恒落 fed-lobby 会话
//    （sender_id 前缀 + sender_name 🌐 来源标注；与我的大厅隔离）
#[tokio::test]
async fn fed_ingest_writes_message_with_fed_sender_prefix() {
    let h = empty_handler();
    let fed = h.federation();
    // 新 kind（现行 fed-lobby 发言广播）
    let msg = human_lobby_msg("fed-m-1", "0xdeadbeef", "来自远程的问候");
    let payload = build_im_fed_lobby_payload("node-106", &msg);
    assert_eq!(payload["fed"], FED_KIND_IM_FED_LOBBY);
    assert_eq!(fed.ingest(&payload), ImFedIngest::Written);
    let saved = h
        .messages_snapshot()
        .into_iter()
        .find(|m| m.id == "fed-m-1")
        .expect("应写入本地 im_messages");
    assert_eq!(
        saved.conversation_id, FED_LOBBY_ID,
        "远程联邦消息恒落联邦大厅（与我的大厅隔离）"
    );
    assert_eq!(
        saved.sender_id,
        format!("{FED_SENDER_PREFIX}node-106:0xdeadbeef"),
        "sender_id 加来源前缀"
    );
    assert_eq!(
        saved.sender_name.as_deref(),
        Some("🌐 远程用户（node-106）"),
        "sender_name 加 🌐 来源标注"
    );
    assert_eq!(saved.content, "来自远程的问候");
    assert_eq!(saved.sender_kind, "human");
    // 旧 kind（旧版节点 im_lobby 广播）兼容接收——同样落 fed-lobby
    let legacy = human_lobby_msg("fed-m-legacy", "0xcafe", "旧版节点的广播");
    assert_eq!(
        fed.ingest(&build_im_lobby_fed_payload("node-old", &legacy)),
        ImFedIngest::Written
    );
    let saved_legacy = h
        .messages_snapshot()
        .into_iter()
        .find(|m| m.id == "fed-m-legacy")
        .expect("旧载荷兼容落地");
    assert_eq!(saved_legacy.conversation_id, FED_LOBBY_ID);
    assert_eq!(saved_legacy.sender_id, "fed:node-old:0xcafe");
    assert!(
        !h.messages_snapshot()
            .iter()
            .any(|m| m.conversation_id == LOBBY_ID && m.sender_id.starts_with(FED_SENDER_PREFIX)),
        "我的大厅（lobby）不再出现联邦消息"
    );
}

// 4. 联邦接收去重：同 id 二次收不重写（内存缓存 + DB 双重判定）
#[tokio::test]
async fn fed_ingest_dedups_same_message_id() {
    let h = empty_handler();
    let fed = h.federation();
    let msg = human_lobby_msg("fed-dup", "0x1", "只写一次");
    let payload = build_im_lobby_fed_payload("node-a", &msg);
    assert_eq!(fed.ingest(&payload), ImFedIngest::Written);
    assert_eq!(fed.ingest(&payload), ImFedIngest::Duplicate, "缓存命中");
    // 重启语义：新端点（缓存为空）仍靠 DB 兜底不重写
    let fresh = ImRouteHandler::with_empty();
    // 同一 DB 需共享：直接用同一 handler 的另一端点视角——ImFederation
    // 是 Arc<ImShared> 封装，缓存属端点私有；DB 兜底用同 handler 验证：
    // （清缓存不可行，故 DB 兜底路径由 nexhub/bridge 测试与下面 5 覆盖）
    assert_eq!(
        fed.ingest(&payload),
        ImFedIngest::Duplicate,
        "三次收仍不重写"
    );
    assert_eq!(
        h.messages_snapshot()
            .iter()
            .filter(|m| m.id == "fed-dup")
            .count(),
        1,
        "库中仅一条"
    );
    drop(fresh);
}

// 5. 联邦接收：agent/系统/非 im_lobby/缺字段载荷一律 Ignored 零写入
#[tokio::test]
async fn fed_ingest_ignores_agent_system_and_foreign_payloads() {
    let h = empty_handler();
    let fed = h.federation();
    // agent 消息（远端助手回复不落本地）
    let mut agent = human_lobby_msg("fed-agent", "0x2", "AI 回复");
    agent.sender_kind = "agent".to_string();
    assert_eq!(
        fed.ingest(&build_im_lobby_fed_payload("n", &agent)),
        ImFedIngest::Ignored
    );
    // 系统消息
    let sys = build_welcome_message("bob");
    assert_eq!(
        fed.ingest(&build_im_lobby_fed_payload("n", &sys)),
        ImFedIngest::Ignored
    );
    // 非 im_lobby（NexHub 大厅条目等他类载荷）
    assert_eq!(
        fed.ingest(&serde_json::json!({"fed": "nexhub_lobby", "node": "n", "entry": {}})),
        ImFedIngest::Ignored
    );
    // 无 fed 标记（P2b 调试消息 {text}）
    assert_eq!(
        fed.ingest(&serde_json::json!({"text": "hi"})),
        ImFedIngest::Ignored
    );
    // 缺 node / 缺 message / message 非法
    let m = human_lobby_msg("x", "0x3", "c");
    assert_eq!(
        fed.ingest(&serde_json::json!({"fed": FED_KIND_IM_LOBBY, "message": m})),
        ImFedIngest::Ignored
    );
    assert_eq!(
        fed.ingest(&serde_json::json!({"fed": FED_KIND_IM_LOBBY, "node": "n"})),
        ImFedIngest::Ignored
    );
    assert_eq!(
        fed.ingest(&serde_json::json!({"fed": FED_KIND_IM_LOBBY, "node": "n", "message": 42})),
        ImFedIngest::Ignored
    );
    assert!(
        !h.messages_snapshot()
            .iter()
            .any(|m| m.sender_id.starts_with(FED_SENDER_PREFIX)),
        "全部非法/不可联邦载荷零写入（库中仅有 schema 的欢迎消息）"
    );
}

// 6. 联邦接收触发 WS 广播（本地在线用户实时看到远程联邦大厅消息——
//    帧型 im_fed_lobby_message，路由到联邦大厅会话而非我的大厅）
#[tokio::test]
async fn fed_ingest_broadcasts_ws_to_local_users() {
    let hub = WsHub::new(8);
    let h = ImRouteHandler::with_empty_ws(hub, std::sync::Arc::new(ImAuth::new()));
    let (_sid, mut rx) = {
        let hub2 = match &h.shared.ws_hub {
            Some(h) => h,
            None => panic!("应持有 Hub"),
        };
        hub2.subscribe_raw("ws-user")
    };
    let fed = h.federation();
    let msg = human_lobby_msg("fed-ws-1", "0x4", "远程 WS 推送");
    assert_eq!(
        fed.ingest(&build_im_fed_lobby_payload("node-106", &msg)),
        ImFedIngest::Written
    );
    let ws = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("WS 广播应即时")
        .expect("订阅存活");
    match ws {
        WsMessage::ImFedLobbyMessage { lobby_id, message } => {
            assert_eq!(lobby_id, FED_LOBBY_ID);
            assert_eq!(message["id"], "fed-ws-1");
            assert_eq!(message["conversation_id"], FED_LOBBY_ID);
            assert_eq!(
                message["sender_id"],
                format!("{FED_SENDER_PREFIX}node-106:0x4")
            );
        }
        other => panic!("应为 ImFedLobbyMessage，实际 {other:?}"),
    }
}

// ---- 联邦接收开关（2026-08-23：GET/POST /api/v1/im/federation）----

// 7. 开关端点鉴权矩阵 + 状态读写：GET 默认开（匿名 401）→ POST（IM token）
//    关闭 → GET 反映 → POST（admin token）重开；匿名 POST 401 / 缺字段 400
#[tokio::test]
async fn fed_toggle_endpoints_auth_and_state() {
    let h = empty_handler().with_admin_token("adm-fed-1");
    let (_pubkey, token) = login(&h, &new_key()).await;
    // 匿名 GET / POST 一律 401
    assert_eq!(
        h.handle(get_req(PATH_FEDERATION)).await.unwrap().status,
        401
    );
    assert_eq!(
        h.handle(post_req(
            PATH_FEDERATION,
            serde_json::json!({"enabled": false})
        ))
        .await
        .unwrap()
        .status,
        401
    );
    // 默认开 + note 文案
    let resp = h.handle(authed_get(PATH_FEDERATION, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["enabled"], true, "默认开");
    assert!(
        resp.body["note"].as_str().unwrap().contains("开启"),
        "note 应说明开启状态: {}",
        resp.body["note"]
    );
    // body 缺 enabled → 400
    let resp = h
        .handle(authed_post(
            PATH_FEDERATION,
            &token,
            serde_json::json!({"foo": 1}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400);
    // IM token 关闭 → enabled=false + note 说明暂停
    let resp = h
        .handle(authed_post(
            PATH_FEDERATION,
            &token,
            serde_json::json!({"enabled": false}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["enabled"], false);
    assert!(
        resp.body["note"].as_str().unwrap().contains("暂停"),
        "note 应说明暂停状态: {}",
        resp.body["note"]
    );
    // GET 反映新状态
    let resp = h.handle(authed_get(PATH_FEDERATION, &token)).await.unwrap();
    assert_eq!(resp.body["enabled"], false);
    // admin token（无 IM token）可重开——Bearer 头同格式
    let resp = h
        .handle(authed_post(
            PATH_FEDERATION,
            "adm-fed-1",
            serde_json::json!({"enabled": true}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "admin token 应可切换");
    assert_eq!(resp.body["enabled"], true);
    assert!(h.federation().fed_enabled(), "内核状态同步");
}

// 8. 关闭后 ingest 入口短路：Paused 零写入（合法载荷也不落地）
#[tokio::test]
async fn fed_ingest_paused_when_disabled_zero_write() {
    let h = empty_handler();
    let fed = h.federation();
    assert!(fed.fed_enabled(), "默认开");
    // 经端点关闭（端点→内核同一条路）
    let (_pk, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            PATH_FEDERATION,
            &token,
            serde_json::json!({"enabled": false}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.body["enabled"], false);
    // 合法 im_lobby 载荷也短路 Paused
    let msg = human_lobby_msg("fed-paused-1", "0x9", "暂停期间的远程消息");
    let payload = build_im_lobby_fed_payload("node-x", &msg);
    assert_eq!(fed.ingest(&payload), ImFedIngest::Paused);
    // 他类载荷同样在入口短路（开关优先于载荷解析）
    assert_eq!(
        fed.ingest(&serde_json::json!({"fed": "nexhub_lobby", "entry": {}})),
        ImFedIngest::Paused
    );
    assert!(
        !h.messages_snapshot().iter().any(|m| m.id == "fed-paused-1"),
        "暂停期间零写入"
    );
}

// 9. 重新打开即恢复：同载荷正常落地（Written）
#[tokio::test]
async fn fed_ingest_resumes_after_reenable() {
    let h = empty_handler();
    let fed = h.federation();
    fed.set_fed_enabled(false);
    let msg = human_lobby_msg("fed-resume-1", "0xa", "恢复后的远程消息");
    let payload = build_im_lobby_fed_payload("node-y", &msg);
    assert_eq!(fed.ingest(&payload), ImFedIngest::Paused);
    // 经端点重开（POST enabled=true）
    let (_pk, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            PATH_FEDERATION,
            &token,
            serde_json::json!({"enabled": true}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.body["enabled"], true);
    // 同一载荷正常写入（未被暂停期间的丢弃污染去重缓存）
    assert_eq!(fed.ingest(&payload), ImFedIngest::Written);
    assert!(
        h.messages_snapshot()
            .iter()
            .any(|m| m.id == "fed-resume-1" && m.sender_id == "fed:node-y:0xa"),
        "恢复后正常写入（带来源前缀）"
    );
}

// 10. 接收开关不影响发送：关闭接收后 POST /fed-lobby/messages 照常 201，
//     且内部 federate 照常广播——对端节点收到 im_fed_lobby_message 载荷
//     （双节点端到端）
#[tokio::test]
async fn fed_receive_toggle_does_not_affect_send() {
    use os_p2p::{P2pConfig, P2pNode, Timing};
    // 双节点 mesh（handlers/p2p.rs 测试同款：A 公网锚点 + B 引导到 A）
    let a = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .expect("A 随机端口绑定必成功");
    let b = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![a.listen_addr()],
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .expect("B 随机端口绑定必成功");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let peers = a.peers().await;
        if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // 1ms 级节流时延注入：联邦广播走延迟队列（默认 10s 太慢）
    let h = fast_fed_handler();
    let fed = h.federation();
    fed.set_p2p(a.clone(), "node-a".into());
    let (_pk, token) = login(&h, &new_key()).await;
    // 先加入联邦大厅（POST /fed-lobby/messages 须成员）
    let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    // 关闭接收 → 本地发送 + 广播均不受影响
    fed.set_fed_enabled(false);
    let mut brx = b.on_msg();
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "关了接收也能发"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "本地发消息不受接收开关影响");
    let msg_id = resp.body["id"].as_str().unwrap().to_string();
    // 对端收到联邦广播（POST 内部的 federate_fed_lobby_message 照常执行——
    // 现经延迟队列，测试注入 1ms 时延快速到期）
    let got = tokio::time::timeout(Duration::from_secs(3), brx.recv())
        .await
        .expect("对端应收到广播（发送不受接收开关影响）")
        .expect("broadcast 存活");
    assert_eq!(got.payload["fed"], FED_KIND_IM_FED_LOBBY);
    assert_eq!(got.payload["node"], "node-a");
    assert_eq!(got.payload["message"]["id"], msg_id.as_str());
    assert_eq!(got.payload["message"]["conversation_id"], FED_LOBBY_ID);
    a.shutdown().await;
    b.shutdown().await;
}

// ---- 联邦大厅独立会话（fed-lobby，2026-08-23 用户纠正：可写、与我的大厅隔离）----

// 11. GET /fed-lobby：心跳 + 加入（在场表记录成员）+ 信息聚合
//     （id 恒为 fed-lobby；无欢迎系统消息——联邦大厅是跨节点频道）
#[tokio::test]
async fn fed_lobby_join_info_and_heartbeat() {
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    // 匿名 401
    assert_eq!(h.handle(get_req(PATH_FED_LOBBY)).await.unwrap().status, 401);
    let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.body["id"], FED_LOBBY_ID);
    assert_eq!(resp.body["name"], "联邦大厅");
    assert_eq!(resp.body["member_count"], 1, "加入后在场表 +1");
    assert_eq!(resp.body["online_count"], 1, "心跳即时在线");
    assert_eq!(
        resp.body["last_message"],
        serde_json::Value::Null,
        "暂无消息"
    );
    // 加入记录在在场表（与我的大厅共用本节点 IM 在场）
    let members = {
        let conn = h.shared.db.lock().expect("db poisoned");
        load_lobby_members(&conn).unwrap_or_default()
    };
    assert!(members.iter().any(|m| m.user_id == pubkey));
    // 无欢迎系统消息落 fed-lobby
    assert!(
        !h.messages_snapshot()
            .iter()
            .any(|m| m.conversation_id == FED_LOBBY_ID),
        "加入联邦大厅不产生系统消息"
    );
}

// 12. POST /fed-lobby/messages：写入 fed-lobby 会话 + 列表/增量补拉
#[tokio::test]
async fn fed_lobby_post_writes_lists_and_incremental() {
    let h = empty_handler();
    let (pubkey, token) = login(&h, &new_key()).await;
    let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    let first = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "第一条 @NexOS助手", "sender_kind": "agent"}),
        ))
        .await
        .unwrap();
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.body["conversation_id"], FED_LOBBY_ID);
    assert_eq!(
        first.body["sender_id"], pubkey,
        "sender = token 反查 pubkey"
    );
    assert_eq!(
        first.body["sender_kind"], "agent",
        "sender_kind 展示层自声明（agent 声明保留；联邦广播按 federable 规则跳过）"
    );
    assert_eq!(
        first.body["mentions"],
        serde_json::json!(["NexOS助手"]),
        "mentions 服务端解析"
    );
    assert!(first.body["attachment"].is_null(), "联邦通道不承载附件");
    let second = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "第二条"}),
        ))
        .await
        .unwrap();
    assert_eq!(second.status, 201);
    // 全量列表（最近 50 条，时间正序）
    let list = h
        .handle(authed_get(PATH_FED_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let arr = list.body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["content"], "第一条 @NexOS助手");
    assert_eq!(arr[1]["content"], "第二条");
    // 增量补拉：after_id=第一条 → 只剩第二条
    let first_id = first.body["id"].as_str().unwrap();
    let gap = h
        .handle(authed_get(
            &format!("{PATH_FED_LOBBY_MESSAGES}?after_id={first_id}"),
            &token,
        ))
        .await
        .unwrap();
    let gap_arr = gap.body.as_array().unwrap();
    assert_eq!(gap_arr.len(), 1, "增量只补缺口");
    assert_eq!(gap_arr[0]["content"], "第二条");
    // 通用补拉端点 conversation_id=fed-lobby 也可读（跨节点公共频道）
    let catchup = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={FED_LOBBY_ID}&after_id={first_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(catchup.status, 200, "{}", catchup.body);
    assert_eq!(catchup.body.as_array().unwrap().len(), 1);
    // 信息聚合的最近消息跟随
    let info = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    assert_eq!(info.body["last_message"]["content"], "第二条");
    // sender_kind 非 agent 白名单值 → 归一 human（展示层自声明兜底）
    let junk = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "垃圾声明", "sender_kind": "robot"}),
        ))
        .await
        .unwrap();
    assert_eq!(junk.status, 201);
    assert_eq!(junk.body["sender_kind"], "human", "非 agent 归一 human");
}

// 13. fed-lobby 端点鉴权矩阵：匿名 401 ×3 / 未加入 403 / 空正文 400 /
//     加入后 201（GET 心跳即加入）
#[tokio::test]
async fn fed_lobby_endpoints_auth_matrix() {
    let h = empty_handler();
    let (_pubkey, token) = login(&h, &new_key()).await;
    // 匿名三端点一律 401
    assert_eq!(h.handle(get_req(PATH_FED_LOBBY)).await.unwrap().status, 401);
    assert_eq!(
        h.handle(get_req(PATH_FED_LOBBY_MESSAGES))
            .await
            .unwrap()
            .status,
        401
    );
    assert_eq!(
        h.handle(post_req(
            PATH_FED_LOBBY_MESSAGES,
            serde_json::json!({"content": "x"})
        ))
        .await
        .unwrap()
        .status,
        401
    );
    // 未加入（GET /fed-lobby 尚未调用）直接发言 → 403
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "还没加入"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 403, "{}", resp.body);
    // GET /fed-lobby/messages 心跳同样自动加入
    let resp = h
        .handle(authed_get(PATH_FED_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    // 空白正文 → 400
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "   "}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400);
    // 加入后发言 → 201
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "加入了"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
}

// 14. fed-lobby 与我的大厅完全隔离：互不串消息（本地发言/远程联邦消息各归各会话）
#[tokio::test]
async fn fed_lobby_and_my_lobby_fully_isolated() {
    let h = empty_handler();
    let fed = h.federation();
    let (pubkey, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap(); // 加入我的大厅
    let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap(); // 加入联邦大厅
                                                                         // 两边各发一条
    let _ = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "我的大厅消息"}),
        ))
        .await
        .unwrap();
    let _ = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "联邦大厅消息"}),
        ))
        .await
        .unwrap();
    // 远程联邦消息（ingest）只落 fed-lobby
    let remote = human_lobby_msg("iso-remote", "0x77", "来自对端的联邦消息");
    assert_eq!(
        fed.ingest(&build_im_fed_lobby_payload("node-b", &remote)),
        ImFedIngest::Written
    );
    // 列表互不含对方的消息
    let lobby_list = h
        .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    for m in lobby_list.body.as_array().unwrap() {
        assert_ne!(m["content"], "联邦大厅消息", "我的大厅不含 fed-lobby 发言");
        assert_ne!(m["content"], "来自对端的联邦消息");
    }
    let fed_list = h
        .handle(authed_get(PATH_FED_LOBBY_MESSAGES, &token))
        .await
        .unwrap();
    let fed_arr = fed_list.body.as_array().unwrap();
    assert_eq!(fed_arr.len(), 2, "本地发言 + 远程联邦消息");
    for m in fed_arr {
        assert_ne!(m["content"], "我的大厅消息", "联邦大厅不含我的大厅发言");
    }
    // 快照按 conversation_id 严格分离
    let snap = h.messages_snapshot();
    assert_eq!(
        snap.iter()
            .filter(|m| m.conversation_id == LOBBY_ID && m.sender_id == pubkey)
            .count(),
        1
    );
    assert_eq!(
        snap.iter()
            .filter(|m| m.conversation_id == FED_LOBBY_ID && m.sender_id == pubkey)
            .count(),
        1
    );
    assert_eq!(
        snap.iter()
            .filter(|m| m.conversation_id == FED_LOBBY_ID && m.sender_id == "fed:node-b:0x77")
            .count(),
        1
    );
}

// 15. POST /fed-lobby/messages P2P 广播（双节点端到端：对端收到
//     im_fed_lobby_message 载荷，message.conversation_id=fed-lobby）——
//     2026-08-24 起经延迟队列到期广播（测试注入 1ms 时延）
#[tokio::test]
async fn fed_lobby_post_p2p_broadcast_two_nodes() {
    use os_p2p::{P2pConfig, P2pNode, Timing};
    let a = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let b = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![a.listen_addr()],
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let peers = a.peers().await;
        if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // 1ms 级节流时延注入：联邦广播走延迟队列（默认 10s 太慢）
    let h = fast_fed_handler();
    h.federation().set_p2p(a.clone(), "node-a".into());
    let (pubkey, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    let mut brx = b.on_msg();
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "跨节点你好"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    let got = tokio::time::timeout(Duration::from_secs(3), brx.recv())
        .await
        .expect("对端应收到联邦大厅广播")
        .expect("broadcast 存活");
    assert_eq!(got.payload["fed"], FED_KIND_IM_FED_LOBBY);
    assert_eq!(got.payload["node"], "node-a");
    assert_eq!(got.payload["message"]["conversation_id"], FED_LOBBY_ID);
    assert_eq!(got.payload["message"]["sender_id"], pubkey);
    assert_eq!(got.payload["message"]["content"], "跨节点你好");
    a.shutdown().await;
    b.shutdown().await;
}

// ---- 联邦大厅发言时延节流（2026-08-24：不限次、不拒绝、不丢消息——
//      仅延后联邦广播时刻；本地即时不变）----

// 15a. 状态机纯逻辑：首条发言 → 常态时延 10s
#[test]
fn fed_throttle_first_message_is_short_delay() {
    let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
    let t0 = Instant::now();
    assert_eq!(
        t.delay_for("0xa1", t0),
        FED_THROTTLE_SHORT,
        "首条（窗口内零历史）→ 10s"
    );
}

// 15b. 状态机纯逻辑：60s 窗口内第二条、第三条 → 升级时延 60s
#[test]
fn fed_throttle_second_and_third_within_window_are_long() {
    let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
    let t0 = Instant::now();
    assert_eq!(t.delay_for("0xa1", t0), FED_THROTTLE_SHORT);
    assert_eq!(
        t.delay_for("0xa1", t0 + Duration::from_secs(5)),
        FED_THROTTLE_LONG,
        "60s 内第二条 → 60s"
    );
    assert_eq!(
        t.delay_for("0xa1", t0 + Duration::from_secs(20)),
        FED_THROTTLE_LONG,
        "60s 内第三条仍 60s（不限次，只升时延）"
    );
}

// 15c. 状态机纯逻辑：安静 61s 后窗口滑空 → 回落 10s
#[test]
fn fed_throttle_falls_back_after_quiet_61s() {
    let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
    let t0 = Instant::now();
    assert_eq!(t.delay_for("0xa1", t0), FED_THROTTLE_SHORT);
    assert_eq!(
        t.delay_for("0xa1", t0 + Duration::from_secs(1)),
        FED_THROTTLE_LONG
    );
    // 距最后一条（t0+1）61s → 全部滑出 60s 窗口，按首条对待
    assert_eq!(
        t.delay_for("0xa1", t0 + Duration::from_secs(62)),
        FED_THROTTLE_SHORT,
        "安静 61s 后回落 10s"
    );
}

// 15d. 状态机纯逻辑：窗口滑动边界 + 发送者隔离
//      （30s 前一条 + 现在一条 = 2 → 60s；60.5s 前的不再计入 → 10s；
//        另一 sender 零历史不受他人影响 → 10s）
#[test]
fn fed_throttle_sliding_window_and_sender_isolation() {
    let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
    let t0 = Instant::now();
    assert_eq!(t.delay_for("0xa1", t0), FED_THROTTLE_SHORT);
    assert_eq!(
        t.delay_for("0xa1", t0 + Duration::from_secs(30)),
        FED_THROTTLE_LONG,
        "30s 前一条在窗口内 → 含本次 2 条 → 60s"
    );
    // 上一条在 t0+30，距今 60.5s → 滑出窗口
    assert_eq!(
        t.delay_for(
            "0xa1",
            t0 + Duration::from_secs(30) + Duration::from_millis(60_500)
        ),
        FED_THROTTLE_SHORT,
        "60.5s 前的发言不再计入 → 回落 10s"
    );
    // 发送者隔离：0xb2 的首条不受 0xa1 的密集发言影响
    assert_eq!(
        t.delay_for("0xb2", t0),
        FED_THROTTLE_SHORT,
        "per-sender 计数，互不影响"
    );
}

// 15e. HTTP 路径透出：首条 federate_delay_secs=10 + note；60s 内第二条
//      =60；agent 自声明消息不参与联邦（0 + 专属 note，且不占节流计数）
#[tokio::test]
async fn fed_lobby_post_response_exposes_federate_delay() {
    let h = empty_handler(); // 默认 10s/60s（无 P2P，只验证响应字段）
    let (pubkey, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    let first = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "首条"}),
        ))
        .await
        .unwrap();
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.body["federate_delay_secs"], 10, "首条常态 10s");
    assert_eq!(first.body["sender_id"], pubkey);
    assert!(
        first.body["note"]
            .as_str()
            .is_some_and(|n| n.contains("联邦广播将于 10 秒后发出")),
        "note 说明时延: {}",
        first.body["note"]
    );
    let second = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "二条"}),
        ))
        .await
        .unwrap();
    assert_eq!(second.status, 201);
    assert_eq!(
        second.body["federate_delay_secs"], 60,
        "同一 sender 60s 内第二条升至 60s"
    );
    assert!(
        second.body["note"]
            .as_str()
            .is_some_and(|n| n.contains("60 秒")),
        "note 升级说明: {}",
        second.body["note"]
    );
    // agent 自声明：不联邦（0 + 专属 note），且不改变他人计数
    let agent = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "agent 不出本节点", "sender_kind": "agent"}),
        ))
        .await
        .unwrap();
    assert_eq!(agent.status, 201, "{}", agent.body);
    assert_eq!(agent.body["federate_delay_secs"], 0, "agent 消息不联邦");
    assert!(
        agent.body["note"]
            .as_str()
            .is_some_and(|n| n.contains("不参与联邦广播")),
        "agent note: {}",
        agent.body["note"]
    );
}

// 15f. 延迟队列端到端（双节点 + 1ms 级时延注入）：本地即时落库，联邦
//      广播到期发出且**按入队序**送达对端（消息永不丢弃、不限次）
#[tokio::test]
async fn fed_throttle_delay_queue_end_to_end_two_nodes() {
    use os_p2p::{P2pConfig, P2pNode, Timing};
    let a = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let b = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![a.listen_addr()],
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let peers = a.peers().await;
        if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let h = fast_fed_handler();
    h.federation().set_p2p(a.clone(), "node-a".into());
    let (_pk, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    // 连发两条（本地应立即全在库——联邦延迟不影响本地体验）
    let r1 = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "队列一号"}),
        ))
        .await
        .unwrap();
    let r2 = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "队列二号"}),
        ))
        .await
        .unwrap();
    assert_eq!(r1.status, 201);
    assert_eq!(r2.status, 201, "第二条不被拒绝——不限次，仅升时延");
    assert!(
        r1.body["federate_delay_secs"].is_u64() && r2.body["federate_delay_secs"].is_u64(),
        "响应透出 federate_delay_secs 字段（1ms 注入下取整为 0）"
    );
    assert_eq!(
        h.messages_snapshot()
            .iter()
            .filter(|m| m.conversation_id == FED_LOBBY_ID)
            .count(),
        2,
        "本地两条均已即时落库（联邦延迟不动本地）"
    );
    // 对端按入队序收到两条联邦广播（经延迟队列到期发出）
    let mut brx = b.on_msg();
    let got1 = tokio::time::timeout(Duration::from_secs(3), brx.recv())
        .await
        .expect("对端应收到第一条联邦广播")
        .expect("broadcast 存活");
    let got2 = tokio::time::timeout(Duration::from_secs(3), brx.recv())
        .await
        .expect("对端应收到第二条联邦广播（不限次不丢消息）")
        .expect("broadcast 存活");
    assert_eq!(got1.payload["fed"], FED_KIND_IM_FED_LOBBY);
    assert_eq!(got1.payload["message"]["content"], "队列一号");
    assert_eq!(
        got2.payload["message"]["content"], "队列二号",
        "按入队序送达"
    );
    a.shutdown().await;
    b.shutdown().await;
}

// 16. 我的大厅不再自动联邦广播（双节点：POST /lobby/messages 后对端收不到
//     任何载荷——完全隔离；联邦大厅发言才有广播）
#[tokio::test]
async fn my_lobby_no_longer_federates_two_nodes() {
    use os_p2p::{P2pConfig, P2pNode, Timing};
    let a = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let b = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![a.listen_addr()],
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let peers = a.peers().await;
        if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let h = empty_handler();
    h.federation().set_p2p(a.clone(), "node-a".into());
    let (_pk, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let mut brx = b.on_msg();
    let resp = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "只留本节点"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    assert!(
        tokio::time::timeout(Duration::from_millis(800), brx.recv())
            .await
            .is_err(),
        "我的大厅发言不再联邦广播（对端收不到任何载荷）"
    );
    a.shutdown().await;
    b.shutdown().await;
}

// 17. WS 广播正确路由：lobby 发言 → im_lobby_message 帧；fed-lobby 发言 →
//     im_fed_lobby_message 帧（同一 Hub 两帧型互不混淆）
#[tokio::test]
async fn ws_broadcast_routes_lobby_vs_fed_lobby() {
    let hub = WsHub::new(8);
    let h = ImRouteHandler::with_empty_ws(hub, std::sync::Arc::new(ImAuth::new()));
    let (pubkey, token) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
    let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
    // 订阅放在两次 GET 之后——避开 GET /lobby 的欢迎系统消息广播
    let (_sid, mut rx) = match &h.shared.ws_hub {
        Some(hub2) => hub2.subscribe_raw("ws-user"),
        None => panic!("应持有 Hub"),
    };
    // fed-lobby 发言 → ImFedLobbyMessage
    let resp = h
        .handle(authed_post(
            PATH_FED_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "联邦频道帧"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    let ws1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("联邦大厅 WS 广播应即时")
        .expect("订阅存活");
    match ws1 {
        WsMessage::ImFedLobbyMessage { lobby_id, message } => {
            assert_eq!(lobby_id, FED_LOBBY_ID);
            assert_eq!(message["conversation_id"], FED_LOBBY_ID);
            assert_eq!(message["content"], "联邦频道帧");
        }
        other => panic!("fed-lobby 发言应为 ImFedLobbyMessage，实际 {other:?}"),
    }
    // lobby 发言 → ImLobbyMessage（不受 fed-lobby 影响）
    let resp = h
        .handle(authed_post(
            PATH_LOBBY_MESSAGES,
            &token,
            serde_json::json!({"content": "本节点频道帧"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    let ws2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("大厅 WS 广播应即时")
        .expect("订阅存活");
    match ws2 {
        WsMessage::ImLobbyMessage { lobby_id, message } => {
            assert_eq!(lobby_id, LOBBY_ID);
            assert_eq!(message["conversation_id"], LOBBY_ID);
            assert_eq!(message["sender_id"], pubkey);
        }
        other => panic!("lobby 发言应为 ImLobbyMessage，实际 {other:?}"),
    }
}

// 18. 联邦接收暂停对 fed-lobby 同样生效：关闭后 ingest 短路 Paused
//     （暂停期间远程联邦消息不落 fed-lobby），恢复即写入
#[tokio::test]
async fn fed_lobby_ingest_paused_and_resume() {
    let h = empty_handler();
    let fed = h.federation();
    fed.set_fed_enabled(false);
    let remote = human_lobby_msg("fed-pause-1", "0x51", "暂停期间的联邦消息");
    assert_eq!(
        fed.ingest(&build_im_fed_lobby_payload("node-z", &remote)),
        ImFedIngest::Paused
    );
    assert!(
        !h.messages_snapshot().iter().any(|m| m.id == "fed-pause-1"),
        "暂停期间零写入"
    );
    fed.set_fed_enabled(true);
    assert_eq!(
        fed.ingest(&build_im_fed_lobby_payload("node-z", &remote)),
        ImFedIngest::Written
    );
    let saved = h
        .messages_snapshot()
        .into_iter()
        .find(|m| m.id == "fed-pause-1")
        .unwrap();
    assert_eq!(saved.conversation_id, FED_LOBBY_ID);
}

// ---- 大厅开放开关 + 远程大厅浏览/发言（2026-08-23，节点发现页联动）----

/// 直接向 handler 的内存库插一条大厅消息（绕过 REST——测试种子数据）。
fn seed_lobby_msg(h: &ImRouteHandler, msg: &Message) {
    let conn = h.shared.db.lock().expect("db poisoned");
    insert_message(&conn, msg).expect("种子消息写入必成功");
}

// 11. 开关端点：开发期默认 true + 鉴权矩阵 + IM/admin token 读写 + note 文案
#[tokio::test]
async fn lobby_access_endpoints_auth_default_and_toggle() {
    let h = empty_handler().with_admin_token("adm-lobby-1");
    let (_pubkey, token) = login(&h, &new_key()).await;
    // 匿名 GET/POST 一律 401
    assert_eq!(
        h.handle(get_req(PATH_LOBBY_ACCESS)).await.unwrap().status,
        401
    );
    assert_eq!(
        h.handle(post_req(
            PATH_LOBBY_ACCESS,
            serde_json::json!({"lobby_public": false})
        ))
        .await
        .unwrap()
        .status,
        401
    );
    // 开发期默认 true（缺省开放，允许其他节点浏览）+ note 说明
    let resp = h
        .handle(authed_get(PATH_LOBBY_ACCESS, &token))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["lobby_public"], true, "开发期缺省开放");
    assert!(
        resp.body["note"].as_str().unwrap().contains("开放"),
        "note 应说明开放状态: {}",
        resp.body["note"]
    );
    assert!(h.federation().lobby_public(), "内核状态同步默认 true");
    // body 缺字段 → 400
    let resp = h
        .handle(authed_post(
            PATH_LOBBY_ACCESS,
            &token,
            serde_json::json!({"foo": 1}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400);
    // IM token 关闭 → false + note 说明未开放语义
    let resp = h
        .handle(authed_post(
            PATH_LOBBY_ACCESS,
            &token,
            serde_json::json!({"lobby_public": false}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["lobby_public"], false);
    assert!(resp.body["note"].as_str().unwrap().contains("未开放"));
    // GET 反映新状态；admin token（无 IM token）可重新打开——Bearer 同格式
    assert_eq!(
        h.handle(authed_get(PATH_LOBBY_ACCESS, &token))
            .await
            .unwrap()
            .body["lobby_public"],
        false
    );
    let resp = h
        .handle(authed_post(
            PATH_LOBBY_ACCESS,
            "adm-lobby-1",
            serde_json::json!({"lobby_public": true}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "admin token 应可切换");
    assert_eq!(resp.body["lobby_public"], true);
    assert!(h.federation().lobby_public(), "内核状态同步开启");
}

// 12. 查询应答 denied 路径：开关关闭（lobby_public=false）回 denied、不读库不泄消息
#[test]
fn lobby_query_reply_denied_by_default() {
    let h = demo_handler();
    let fed = h.federation();
    assert!(fed.lobby_public(), "开发期默认开放");
    fed.set_lobby_public(false);
    assert!(!fed.lobby_public(), "手动关闭后为 false");
    let payload = fed.lobby_query_reply_payload("req-1");
    assert_eq!(payload["fed"], FED_KIND_IM_LOBBY_REPLY);
    assert_eq!(payload["req_id"], "req-1");
    assert_eq!(payload["public"], false);
    assert_eq!(payload["error"], "denied");
    assert!(
        payload.get("messages").is_none(),
        "denied 应答不带任何消息：{payload}"
    );
}

// 13. 开放路径 + 脱敏 + 上限：25 条种子（带附件/文件 URL）→ 恰好最近 20 条、
//     无 attachment/file_url/read_by/mentions 字段
#[test]
fn lobby_query_reply_open_sanitized_and_capped_at_20() {
    let h = empty_handler();
    // 25 条消息，created_at 递增（m-00 最旧 … m-24 最新），全部带附件元数据
    for i in 0..25 {
        let mut m = human_lobby_msg(&format!("m-{i:02}"), "0xabc", &format!("msg {i}"));
        // 时间取 2026-12-31（晚于 with_empty 预置的 msg-lobby-seed 当前时刻）
        m.created_at = format!("2026-12-31T10:{i:02}:00+08:00");
        m.attachment = Some(Attachment {
            file_id: format!("f-{i}"),
            filename: "secret.pdf".into(),
            size_bytes: 1024,
            mime: Some("application/pdf".into()),
        });
        m.file_url = Some(format!("/api/v1/im/files/f-{i}"));
        seed_lobby_msg(&h, &m);
    }
    let fed = h.federation();
    fed.set_lobby_public(true);
    let payload = fed.lobby_query_reply_payload("req-2");
    assert_eq!(payload["fed"], FED_KIND_IM_LOBBY_REPLY);
    assert_eq!(payload["public"], true);
    let msgs = payload["messages"].as_array().expect("开放应带消息数组");
    assert_eq!(msgs.len(), LOBBY_VIEW_LIMIT, "恰好 20 条（上限）");
    // 时间正序 + 是最近的 20 条（m-05..m-24，丢弃最旧 5 条）
    assert_eq!(msgs[0]["id"], "m-05");
    assert_eq!(msgs[19]["id"], "m-24");
    // 脱敏：无 attachment / file_url / read_by / mentions 字段（文件内容不出本机）
    for m in msgs {
        assert!(m.get("attachment").is_none(), "镜像不含附件: {m}");
        assert!(m.get("file_url").is_none(), "镜像不含文件 URL: {m}");
        assert!(m.get("read_by").is_none(), "镜像不含已读名单: {m}");
        assert!(m.get("mentions").is_none(), "镜像不含提及列表: {m}");
        // 展示必需字段齐备
        for field in ["id", "sender_id", "sender_name", "content", "created_at"] {
            assert!(m.get(field).is_some(), "镜像缺 {field}: {m}");
        }
    }
}

// 14. 远程发言（im_lobby_post）：开关关闭丢弃 → 开放后落地（fed: 前缀 + 无附件）
//     → 联邦接收暂停 Paused → 非法载荷 Ignored
#[tokio::test]
async fn fed_lobby_post_gated_until_public() {
    let h = empty_handler();
    let fed = h.federation();
    let payload = build_lobby_post_payload("node-113", "0xab", "0xC0FFEE", "远程问候");
    // 开发期默认开放 → 远程发言直接落地
    assert_eq!(fed.ingest_lobby_post(&payload), ImFedIngest::Written);
    assert!(
        h.messages_snapshot()
            .iter()
            .any(|m| m.content == "远程问候"),
        "缺省开放期间应落地"
    );
    // 手动关闭 → 静默丢弃（零新增写入）
    fed.set_lobby_public(false);
    assert_eq!(fed.ingest_lobby_post(&payload), ImFedIngest::Ignored);
    assert_eq!(
        h.messages_snapshot()
            .iter()
            .filter(|m| m.content == "远程问候")
            .count(),
        1,
        "关闭期间零新增写入"
    );
    // 重新开放 → 落地（sender_id = 远端原 pubkey——直接发到本机大厅的消息
    // 不加 fed: 前缀（fed: 前缀归属联邦大厅）；不承载附件）
    fed.set_lobby_public(true);
    assert_eq!(fed.ingest_lobby_post(&payload), ImFedIngest::Written);
    let got = h
        .messages_snapshot()
        .into_iter()
        .find(|m| m.content == "远程问候")
        .expect("开放后应落地");
    assert_eq!(
        got.sender_id, "0xab",
        "直接进入本机大厅：sender_id 不加前缀"
    );
    assert_eq!(
        got.sender_name.as_deref(),
        Some("🌐 0xC0FFEE（node-113）"),
        "sender_name 标注远端来源"
    );
    assert_eq!(got.conversation_id, LOBBY_ID);
    assert!(got.attachment.is_none(), "远程通道不承载附件");
    // 联邦接收暂停 → Paused（与 ingest 同一道闸门）
    fed.set_fed_enabled(false);
    assert_eq!(
        fed.ingest_lobby_post(&payload),
        ImFedIngest::Paused,
        "暂停接收时远程发言同样丢弃"
    );
    fed.set_fed_enabled(true);
    // 非法载荷：空正文 / 缺 sender / 他类 fed → Ignored
    assert_eq!(
        fed.ingest_lobby_post(&build_lobby_post_payload("node-113", "0xab", "n", "   ")),
        ImFedIngest::Ignored
    );
    assert_eq!(
        fed.ingest_lobby_post(&serde_json::json!({
            "fed": FED_KIND_IM_LOBBY_POST, "node": "node-113", "content": "缺 sender"
        })),
        ImFedIngest::Ignored
    );
    assert_eq!(
        fed.ingest_lobby_post(&serde_json::json!({"fed": "im_lobby"})),
        ImFedIngest::Ignored
    );
}

// 15. 远程大厅 REST 端点：鉴权/参数校验矩阵 + 无应答超时路径
//     （真实 p2p 节点但无应答端 → public=null error=timeout；?timeout_ms=300 快速）
#[tokio::test]
async fn remote_lobby_rest_auth_validation_and_timeout() {
    use os_p2p::{P2pConfig, P2pNode, Timing};
    let h = empty_handler();
    let (_pk, token) = login(&h, &new_key()).await;
    // 匿名 → 401；非法 node_id → 400
    assert_eq!(
        h.handle(get_req(PATH_LOBBY_REMOTE)).await.unwrap().status,
        401
    );
    let bad = "/api/v1/im/lobby/remote/0x00";
    let resp = h.handle(authed_get(bad, &token)).await.unwrap();
    assert_eq!(resp.status, 400, "非 66hex node_id 应 400");
    let resp = h
        .handle(authed_post(
            "/api/v1/im/lobby/remote/0x00/messages",
            &token,
            serde_json::json!({"content": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400);
    // 空正文 → 400
    let node = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let hex = node.self_id().to_hex();
    // 注入 Handle（探针就绪）但对端无应答桥 → timeout（?timeout_ms=300 钳制下限）
    h.federation().set_p2p(node.clone(), "node-a".into());
    let resp = h
        .handle(authed_get(
            &format!("/api/v1/im/lobby/remote/{hex}?timeout_ms=300"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "超时是数据态（200 + error 字段）非 5xx");
    assert_eq!(resp.body["node_id"], hex);
    assert_eq!(resp.body["public"], serde_json::Value::Null);
    assert_eq!(resp.body["error"], "timeout");
    // POST：空正文 400；无应答 → 504
    let resp = h
        .handle(authed_post(
            &format!("/api/v1/im/lobby/remote/{hex}/messages?timeout_ms=300"),
            &token,
            serde_json::json!({"content": "   "}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "空正文 400");
    let resp = h
        .handle(authed_post(
            &format!("/api/v1/im/lobby/remote/{hex}/messages?timeout_ms=300"),
            &token,
            serde_json::json!({"content": "有人在吗"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 504, "对方无应答 → 504");
    node.shutdown().await;
}

// 16. 双节点端到端：GET 镜像 denied → 对端开放后带脱敏消息 → POST 远程发言
//     落地对端大厅（fed: 前缀）——节点发现页「进入 IM」的完整数据流
#[tokio::test]
async fn remote_lobby_two_nodes_end_to_end() {
    use crate::handlers::p2p::FederationBridge;
    use os_p2p::{P2pConfig, P2pNode, Timing};
    // B（应答端，公网锚点）+ A（查询端，引导到 B）
    let b_node = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let a_node = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![b_node.listen_addr()],
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let h_a = empty_handler();
    let h_b = empty_handler();
    let fed_a = h_a.federation();
    let fed_b = h_b.federation();
    fed_a.set_p2p(a_node.clone(), "node-a".into());
    fed_b.set_p2p(b_node.clone(), "node-b".into());
    // B 侧入站桥（answer_lobby_query / ingest_lobby_post 在此触发）
    let bridge = FederationBridge {
        im: Some(fed_b.clone()),
        nexhub: None,
        live: None,
        api_market: None,
    };
    let mut brx = b_node.on_msg();
    tokio::spawn(async move {
        while let Ok(m) = brx.recv().await {
            bridge.dispatch(&m);
        }
    });
    // 等 A↔B 直连建立
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let peers = a_node.peers().await;
        if peers
            .iter()
            .any(|p| p.id == *b_node.self_id() && p.connected)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // —— 3) A 经 REST 远程发言 → 落地 B 的大厅（sender=远端原 pubkey，
    //     不加 fed: 前缀——直接进入对方大厅的消息在"我的大厅"显示）——
    let (pk, token) = login(&h_a, &new_key()).await;
    let b_hex = b_node.self_id().to_hex();
    let remote = format!("/api/v1/im/lobby/remote/{b_hex}?timeout_ms=8000");

    // —— 1) B 手动关闭（开发期缺省开放，显式关）→ denied ——
    fed_b.set_lobby_public(false);
    let resp = h_a.handle(authed_get(&remote, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["public"], false, "关闭后 denied");
    assert_eq!(resp.body["error"], "denied");

    // —— 2) B 开放 + 种子消息 → 镜像可见（脱敏）——
    fed_b.set_lobby_public(true);
    let mut seeded = human_lobby_msg("b-msg-1", "0xbb", "B 节点的消息");
    seeded.created_at = "2026-08-23T11:00:00+08:00".into();
    seeded.attachment = Some(Attachment {
        file_id: "f-b1".into(),
        filename: "b-secret.pdf".into(),
        size_bytes: 9,
        mime: None,
    });
    seed_lobby_msg(&h_b, &seeded);
    let resp = h_a.handle(authed_get(&remote, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["public"], true, "开放后允许浏览");
    let msgs = resp.body["messages"].as_array().expect("镜像消息数组");
    assert!(msgs.iter().any(|m| m["id"] == "b-msg-1"), "含 B 的种子消息");
    let mirror = msgs.iter().find(|m| m["id"] == "b-msg-1").unwrap();
    assert!(
        mirror.get("attachment").is_none(),
        "镜像脱敏：不带附件（含 B 侧附件）"
    );

    // —— 3) A 经 REST 远程发言 → 落地 B 的大厅（fed:node-a:<pubkey>）——
    let resp = h_a
        .handle(authed_post(
            &format!("/api/v1/im/lobby/remote/{b_hex}/messages?timeout_ms=8000"),
            &token,
            serde_json::json!({"content": "来自 A 的远程发言"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "开放后远程发言放行");
    assert_eq!(resp.body["ok"], true);
    // 轮询 B 落地（fire-and-forget，毫秒级但异步）
    let deadline = Instant::now() + Duration::from_secs(5);
    let landed = loop {
        let hit = h_b
            .messages_snapshot()
            .into_iter()
            .any(|m| m.content == "来自 A 的远程发言");
        if hit || Instant::now() > deadline {
            break hit;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(landed, "远程发言应落地 B 大厅");
    let got = h_b
        .messages_snapshot()
        .into_iter()
        .find(|m| m.content == "来自 A 的远程发言")
        .unwrap();
    assert_eq!(
        got.sender_id, pk,
        "直接进入对方大厅：sender_id 为远端原 pubkey（不加 fed: 前缀）"
    );
    assert!(
        got.sender_name
            .as_deref()
            .is_some_and(|n| n.starts_with("🌐 ") && n.contains("node-a")),
        "sender_name 标注远端来源: {:?}",
        got.sender_name
    );

    // —— 4) B 关闭开关 → GET 回 denied、POST 回 403（开关切换联动）——
    fed_b.set_lobby_public(false);
    let resp = h_a.handle(authed_get(&remote, &token)).await.unwrap();
    assert_eq!(resp.body["public"], false, "关闭后回 denied");
    let resp = h_a
        .handle(authed_post(
            &format!("/api/v1/im/lobby/remote/{b_hex}/messages?timeout_ms=8000"),
            &token,
            serde_json::json!({"content": "关闭后发言"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 403, "关闭后远程发言被拒");
    assert!(
        !h_b.messages_snapshot()
            .iter()
            .any(|m| m.content == "关闭后发言"),
        "关闭期间零写入"
    );
    a_node.shutdown().await;
    b_node.shutdown().await;
}

// 17. 本地指纹跳过 P2P 自回路（2026-08-23）：A 与 B 共用同一私钥（同
//     NodeID 的另一 OS 实例——身份=密钥，同指纹即同权限域）→ 联邦广播
//     （federate_fed_lobby_message → fed_broadcast）与大厅查询应答
//     （answer_lobby_query）对指纹==本机 NodeID 的目标都不经 P2P：消息
//     已在本地落库，发给同指纹节点只会自回路重复入库。
#[tokio::test]
async fn federation_skips_local_fingerprint_targets() {
    use os_p2p::{NodeIdentity, P2pConfig, P2pNode, Timing};
    let identity = NodeIdentity::generate();
    let a = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        identity: Some(identity.clone()),
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let b = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![a.listen_addr()],
        identity: Some(identity), // 同一私钥 → 同 NodeID 的对端
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    // 等同指纹对端连入（A 侧 identity_conflicts 记账即连接凭证）
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && a.identity_conflicts().await.is_empty() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !a.identity_conflicts().await.is_empty(),
        "同私钥实例应已连入（冲突记账为凭证）"
    );
    let h = empty_handler();
    let fed = h.federation();
    fed.set_p2p(a.clone(), "node-a".into());
    let mut rx = a.on_msg();
    // —— 1) 联邦大厅广播：唯一对端是本机指纹 → 0 目标，返回 false ——
    let msg = human_lobby_msg("m-local-fp", "0xabc", "同指纹广播");
    assert!(
        !fed.federate_fed_lobby_message(&msg).await,
        "指纹==本机的目标被跳过 → 广播 0 peer，返回 false"
    );
    // —— 2) 大厅查询应答：来自本机指纹的 im_lobby_query 跳过（本地自回路）——
    fed.answer_lobby_query(
        a.self_id(),
        &serde_json::json!({"fed": FED_KIND_IM_LOBBY_QUERY, "req_id": "r-local-fp"}),
    );
    // 两处均不得产生本地回声（send 到本机 NodeID 会本地回环交付到 on_msg）
    let echoed = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        echoed.is_err(),
        "本地指纹目标不得经 P2P 自回路回声: {:?}",
        echoed.ok()
    );
    a.shutdown().await;
    b.shutdown().await;
}

// =========================================================================
// 点对点直通消息 DM（2026-08-30）单元测 —— DM 面
// —— 开关读写/默认 true、403 关闭拒发、确定性会话 id（双向同 id）、
//    落库+定向推送形状、跨节点 ingest（双 handler，A 发 B 收）、
//    ingest 对方关闭丢弃、members 感知列表、去重、回程路由
// =========================================================================

/// DM 测试前置：登录 a/b 两身份，b 经 GET /lobby 心跳成为本节点在场身份
/// （identity_local 命中大厅成员路径）。
async fn dm_login_pair(h: &ImRouteHandler) -> ((String, String), (String, String)) {
    let a = login(h, &new_key()).await;
    let b = login(h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &b.1)).await.unwrap();
    (a, b)
}

// DM1. 开关端点：鉴权矩阵 / 开发阶段默认 true / 读写往返 / note 语义
#[tokio::test]
async fn dm_access_endpoints_auth_default_and_toggle() {
    let h = empty_handler().with_admin_token("adm-dm-1");
    let (_pubkey, token) = login(&h, &new_key()).await;
    // 匿名 GET/POST 一律 401
    assert_eq!(h.handle(get_req(PATH_DM_ACCESS)).await.unwrap().status, 401);
    assert_eq!(
        h.handle(post_req(
            PATH_DM_ACCESS,
            serde_json::json!({"dm_open": false})
        ))
        .await
        .unwrap()
        .status,
        401
    );
    // 开发阶段默认 true（用户裁决「当前开发阶段默认允许」）+ note 说明
    let resp = h.handle(authed_get(PATH_DM_ACCESS, &token)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["dm_open"], true, "开发阶段缺省允许直通消息");
    assert!(
        resp.body["note"].as_str().unwrap().contains("开放"),
        "note 应说明开放状态: {}",
        resp.body["note"]
    );
    assert!(h.federation().dm_open(), "内核状态同步默认 true");
    // body 缺字段 → 400
    let resp = h
        .handle(authed_post(
            PATH_DM_ACCESS,
            &token,
            serde_json::json!({"foo": 1}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400);
    // IM token 关闭 → false + note 说明关闭语义（对方发送被拒）
    let resp = h
        .handle(authed_post(
            PATH_DM_ACCESS,
            &token,
            serde_json::json!({"dm_open": false}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["dm_open"], false);
    assert!(resp.body["note"].as_str().unwrap().contains("关闭"));
    // GET 反映新状态；admin token（无 IM token）可重新打开
    assert_eq!(
        h.handle(authed_get(PATH_DM_ACCESS, &token))
            .await
            .unwrap()
            .body["dm_open"],
        false
    );
    let resp = h
        .handle(authed_post(
            PATH_DM_ACCESS,
            "adm-dm-1",
            serde_json::json!({"dm_open": true}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "admin token 应可切换");
    assert_eq!(resp.body["dm_open"], true);
    assert!(h.federation().dm_open(), "内核状态同步重开");
}

// DM2. 确定性会话 id（纯函数）：双向同 id、dm- 前缀、不同对不碰撞
#[test]
fn dm_conversation_id_symmetric_and_prefixed() {
    let a = "0x1111111111111111111111111111111111111111111111111111111111111111";
    let b = "0x2222222222222222222222222222222222222222222222222222222222222222";
    let c = "0x3333333333333333333333333333333333333333333333333333333333333333";
    let ab = dm_conversation_id(a, b);
    let ba = dm_conversation_id(b, a);
    assert_eq!(ab, ba, "发起方向无关——双方看到同一个会话 id");
    assert!(ab.starts_with("dm-"), "dm- 前缀: {ab}");
    assert_eq!(ab.len(), "dm-".len() + 16, "短 hash（8 字节 hex）");
    assert_ne!(ab, dm_conversation_id(a, c), "不同对不碰撞");
    assert_ne!(
        dm_message_id(a, b, "hi", "t1"),
        dm_message_id(a, b, "hi", "t2"),
        "消息 id 含时间戳要素"
    );
}

// DM3. 本地投递全链：route=local、确定性 id、双方会话列表可见（对方发起
//      的 DM 也可见——members 感知而非 created_by）、历史可读
#[tokio::test]
async fn dm_send_local_delivery_deterministic_conversation() {
    let h = empty_handler();
    let ((pa, ta), (pb, tb)) = dm_login_pair(&h).await;
    let resp = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "在吗？直接说"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{}", resp.body);
    assert_eq!(resp.body["route"], "local");
    let cid = resp.body["conversation_id"].as_str().unwrap().to_string();
    assert_eq!(cid, dm_conversation_id(&pa, &pb), "确定性会话 id");
    assert_eq!(resp.body["message"]["sender_id"], pa, "sender=token 反查");
    assert_eq!(resp.body["message"]["content"], "在吗？直接说");
    assert_eq!(
        resp.body["message"]["read_by"],
        serde_json::json!([pa]),
        "发送者自己已读"
    );
    // 双方会话列表都含该 dm 会话（B 看得到 A 发起的——成员感知）
    for tok in [&ta, &tb] {
        let list = h.handle(authed_get(PATH_CONV_LIST, tok)).await.unwrap();
        let hit = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == serde_json::json!(cid))
            .unwrap_or_else(|| panic!("dm 会话应出现在列表"));
        let members = hit["members"].as_array().unwrap();
        assert!(members.contains(&serde_json::json!(pa)));
        assert!(members.contains(&serde_json::json!(pb)));
    }
    // 历史可读（B 视角 1 条）
    let hist = h
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &tb,
        ))
        .await
        .unwrap();
    assert_eq!(hist.status, 200);
    assert_eq!(hist.body.as_array().unwrap().len(), 1);
    // 再发一条 → 复用同一会话（不重复建行）
    let again = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "第二条"}),
        ))
        .await
        .unwrap();
    assert_eq!(again.status, 201);
    assert_eq!(
        again.body["conversation_id"].as_str().unwrap(),
        cid,
        "同对私信复用同一确定性会话"
    );
    let list = h.handle(authed_get(PATH_CONV_LIST, &ta)).await.unwrap();
    assert_eq!(
        list.body
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["id"] == serde_json::json!(cid))
            .count(),
        1,
        "不重复建会话行"
    );
}

// DM4. 参数与开关闸门：dm_open=false → 403「对方未开放直通消息」；
//      非法 pubkey / 给自己发 / 空正文 / 未知对方节点（无路由）→ 4xx
#[tokio::test]
async fn dm_send_gates_closed_switch_and_bad_requests() {
    let h = empty_handler();
    let ((pa, ta), (pb, tb)) = dm_login_pair(&h).await;
    let _ = tb;
    // 非法 to_pubkey
    let bad = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": "not-a-pubkey", "content": "x"}),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status, 400);
    // 给自己发
    let self_dm = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pa, "content": "x"}),
        ))
        .await
        .unwrap();
    assert_eq!(self_dm.status, 400);
    // 空正文
    let empty = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "   "}),
        ))
        .await
        .unwrap();
    assert_eq!(empty.status, 400);
    // 对方不在本节点且无路由（全新身份，从未在本节点出现）→ 404
    let fresh = pubkey_hex(&new_key());
    let no_route = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": fresh, "content": "跨节点私信"}),
        ))
        .await
        .unwrap();
    assert_eq!(no_route.status, 404);
    assert!(
        no_route.body["error"]
            .as_str()
            .unwrap()
            .contains("不在本节点"),
        "404 应说明无路由: {}",
        no_route.body["error"]
    );
    // 关开关 → 本地投递被拒（对方=本节点身份，闸门即本节点 dm_open）
    h.federation().set_dm_open(false);
    let denied = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "在吗"}),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status, 403);
    assert!(
        denied.body["error"]
            .as_str()
            .unwrap()
            .contains("未开放直通消息"),
        "403 文案: {}",
        denied.body["error"]
    );
    // 开关关不影响自己发往**已登记对端**……本用例无登记，重开后恢复 201
    h.federation().set_dm_open(true);
    let ok = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "在吗"}),
        ))
        .await
        .unwrap();
    assert_eq!(ok.status, 201, "重开后恢复本地投递");
}

// DM5. 定向推送形状：收发双方各收 1 帧 im_message（conversation_id=dm-*），
//      无关订阅者收不到（绝不全员广播）
#[tokio::test]
async fn dm_local_push_is_targeted_not_broadcast() {
    let hub = WsHub::default();
    let h = ImRouteHandler::with_empty_ws(hub.clone(), Arc::new(ImAuth::default()));
    let ((pa, ta), (pb, _tb)) = dm_login_pair(&h).await;
    let (_pc, tc) = login(&h, &new_key()).await;
    let _ = h.handle(authed_get(PATH_LOBBY, &tc)).await.unwrap();
    // 心跳/欢迎广播之后再订阅（避免欢迎帧混入）
    let (_sa, mut rx_a) = hub.subscribe_raw(&pa);
    let (_sb, mut rx_b) = hub.subscribe_raw(&pb);
    let (_sc, mut rx_c) = hub.subscribe_raw(&_pc);
    let resp = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "定向私信"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    let cid = resp.body["conversation_id"].as_str().unwrap().to_string();
    for rx in [&mut rx_a, &mut rx_b] {
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("收发双方应各收到一帧")
            .expect("通道不应关闭");
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            v["type"], "im_message",
            "复用 im_message 帧（前端零改动路由）"
        );
        assert_eq!(v["conversation_id"], serde_json::json!(cid));
        assert_eq!(v["message"]["content"], "定向私信");
    }
    let leaked = tokio::time::timeout(Duration::from_millis(300), rx_c.recv()).await;
    assert!(
        leaked.is_err(),
        "无关订阅者不得收到 DM（不广播）: {:?}",
        leaked.ok()
    );
}

// DM6. 可见性收口：非成员的会话列表看不到 dm-*；历史/补拉 403；
//      dm 会话禁用通用发送端点（唯一入口 POST /im/dm，开关不旁路）
#[tokio::test]
async fn dm_visibility_members_only_and_endpoint_gated() {
    let h = empty_handler();
    let ((_pa, ta), (pb, _tb)) = dm_login_pair(&h).await;
    let (_pc, tc) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({"to_pubkey": pb, "content": "私聊内容"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    let cid = resp.body["conversation_id"].as_str().unwrap().to_string();
    // C 的会话列表不含该 dm 会话（members 感知过滤）
    let list = h.handle(authed_get(PATH_CONV_LIST, &tc)).await.unwrap();
    assert!(
        !list
            .body
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == serde_json::json!(cid)),
        "非成员看不到 dm 会话"
    );
    // C 读历史 → 403；离线补拉 → 403
    let hist = h
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &tc,
        ))
        .await
        .unwrap();
    assert_eq!(hist.status, 403, "非参与者无权读取私信");
    let catchup = h
        .handle(authed_get(
            &format!("/api/v1/im/messages?conversation_id={cid}"),
            &tc,
        ))
        .await
        .unwrap();
    assert_eq!(catchup.status, 403, "补拉同款成员门");
    // 通用发送端点对 dm 会话禁用（dm_open/成员收口不旁路）
    let direct = h
        .handle(authed_post(
            &format!("/api/v1/im/conversations/{cid}/messages"),
            &ta,
            serde_json::json!({"content": "绕过开关"}),
        ))
        .await
        .unwrap();
    assert_eq!(direct.status, 400);
    assert!(
        direct.body["error"].as_str().unwrap().contains("/im/dm"),
        "应指引走 /im/dm: {}",
        direct.body["error"]
    );
}

// DM7. 跨节点端到端（双 handler + 双 P2P 节点 + 真 FederationBridge 分发）：
//      A 节点 a 发给 B 节点 b（to_node 定向路由，非广播）→ B 侧 ingest 落
//      dm-* 会话 + b 可见 + 消息 id=载荷 hash；
//      随后 b 无 to_node 回信 → 按 im_dm_peers 登记自动路由回 A 节点
#[tokio::test]
async fn dm_cross_node_end_to_end_and_reply_routing() {
    use crate::handlers::p2p::FederationBridge;
    use os_p2p::{P2pConfig, P2pNode, Timing};
    let node_a = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public: true,
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let node_b = P2pNode::spawn(P2pConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        bootstrap: vec![node_a.listen_addr()],
        timings: Timing::testing(),
        mdns_enabled: false,
        ..P2pConfig::default()
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let peers = node_a.peers().await;
        if peers
            .iter()
            .any(|p| p.id == *node_b.self_id() && p.connected)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let h_a = empty_handler();
    h_a.federation().set_p2p(node_a.clone(), "node-a".into());
    let h_b = empty_handler();
    h_b.federation().set_p2p(node_b.clone(), "node-b".into());
    // a 只在 A 登录；b 只在 B 登录 + 大厅心跳（B 侧本地身份）
    let (pa, ta) = login(&h_a, &new_key()).await;
    let (pb, tb) = login(&h_b, &new_key()).await;
    let _ = h_b.handle(authed_get(PATH_LOBBY, &tb)).await.unwrap();
    let bridge_b = FederationBridge {
        im: Some(h_b.federation()),
        nexhub: None,
        live: None,
        api_market: None,
    };
    let mut rx_b_side = node_b.on_msg(); // 收 a→b 定向载荷（入站观测）
    let mut rx_a_side = node_a.on_msg(); // 收 b→a 回信载荷
                                         // a → b：显式 to_node 定向路由
    let resp = h_a
        .handle(authed_post(
            PATH_DM,
            &ta,
            serde_json::json!({
                "to_pubkey": pb,
                "content": "跨节点直通",
                "to_node": node_b.self_id().to_hex(),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{}", resp.body);
    assert_eq!(resp.body["route"], "p2p");
    let cid_a = resp.body["conversation_id"].as_str().unwrap().to_string();
    assert_eq!(cid_a, dm_conversation_id(&pa, &pb), "发送侧同确定性 id");
    // B 节点收到 im_dm 定向载荷 → 经真 FederationBridge 分发进 ingest
    let got = tokio::time::timeout(Duration::from_secs(3), rx_b_side.recv())
        .await
        .expect("B 节点应收到 im_dm 载荷")
        .expect("通道存活");
    assert_eq!(got.payload["fed"], FED_KIND_IM_DM);
    assert_eq!(got.payload["from_pubkey"], serde_json::json!(pa));
    assert_eq!(got.payload["to_pubkey"], serde_json::json!(pb));
    bridge_b.dispatch(&got);
    // B 侧：b 的会话列表出现 dm- 会话（对端发起也可见）+ 消息落库
    let list_b = h_b.handle(authed_get(PATH_CONV_LIST, &tb)).await.unwrap();
    let hit = list_b
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == serde_json::json!(cid_a))
        .expect("B 侧应出现同 id 的 dm 会话");
    let members = hit["members"].as_array().unwrap();
    assert!(members.contains(&serde_json::json!(pa)));
    assert!(members.contains(&serde_json::json!(pb)));
    let hist_b = h_b
        .handle(authed_get(
            &format!("/api/v1/im/conversations/{cid_a}/messages"),
            &tb,
        ))
        .await
        .unwrap();
    assert_eq!(hist_b.status, 200);
    let arr = hist_b.body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["content"], "跨节点直通");
    assert_eq!(
        arr[0]["sender_id"],
        serde_json::json!(pa),
        "保留原始 pubkey（收件人可据此回信）"
    );
    assert!(
        arr[0]["sender_name"].as_str().unwrap().contains("node-a"),
        "来源标注: {}",
        arr[0]["sender_name"]
    );
    // —— b 回信：不带 to_node —— 按 ingest 登记的回程路由自动定向回 A 节点
    let reply = h_b
        .handle(authed_post(
            PATH_DM,
            &tb,
            serde_json::json!({"to_pubkey": pa, "content": "收到，回你"}),
        ))
        .await
        .unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.body["route"], "p2p", "按 im_dm_peers 登记自动路由");
    assert_eq!(
        reply.body["conversation_id"].as_str().unwrap(),
        cid_a,
        "回信同确定性会话"
    );
    let back = tokio::time::timeout(Duration::from_secs(3), rx_a_side.recv())
        .await
        .expect("A 节点应收到回信载荷")
        .expect("通道存活");
    assert_eq!(back.payload["fed"], FED_KIND_IM_DM);
    assert_eq!(back.payload["from_pubkey"], serde_json::json!(pb));
    assert_eq!(back.payload["to_pubkey"], serde_json::json!(pa));
    node_a.shutdown().await;
    node_b.shutdown().await;
}

// DM8. ingest 闸门与去重：对方 dm_open=false 丢弃；收件人不在本节点丢弃
//      （错投）；同 msg_id 重投只落一份（Duplicate）
#[tokio::test]
async fn dm_ingest_gates_and_dedupe() {
    let h = empty_handler();
    let fed = h.federation();
    // NodeId = 33 字节压缩 secp256k1（0x+66hex）——用真密钥生成
    let from_node =
        os_p2p::NodeId::parse(&pubkey_hex(&new_key())).expect("真公钥应可解析为 NodeId");
    let sender = pubkey_hex(&new_key());
    let ((_, _ta), (pb, _tb)) = dm_login_pair(&h).await;
    let payload = |to: &str, msg_id: &str| {
        serde_json::json!({
            "fed": FED_KIND_IM_DM,
            "msg_id": msg_id,
            "from_pubkey": sender,
            "to_pubkey": to,
            "content": "跨节点私信",
            "node": "node-x",
            "ts": "2026-08-30T10:00:00Z",
        })
    };
    // 非法载荷（缺字段 / 非 im_dm kind）→ Ignored
    assert_eq!(
        fed.ingest_dm(&from_node, &serde_json::json!({"fed": "im_lobby"})),
        ImFedIngest::Ignored
    );
    assert_eq!(
        fed.ingest_dm(
            &from_node,
            &serde_json::json!({"fed": FED_KIND_IM_DM, "to_pubkey": pb})
        ),
        ImFedIngest::Ignored,
        "缺 from_pubkey → Ignored"
    );
    // 收件人不在本节点（错投）→ Ignored
    let fresh = pubkey_hex(&new_key());
    assert_eq!(
        fed.ingest_dm(&from_node, &payload(&fresh, "dm-msg-miss")),
        ImFedIngest::Ignored,
        "错投（收件人非本节点身份）丢弃"
    );
    // 正常落地 → Written
    assert_eq!(
        fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-1")),
        ImFedIngest::Written
    );
    // 同 msg_id 重投 → Duplicate（内存缓存路径）
    assert_eq!(
        fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-1")),
        ImFedIngest::Duplicate
    );
    // DB 兜底路径：绕过缓存直接插同 id → 仍 Duplicate（清空缓存后重投）
    {
        let mut seen = h.shared.fed_seen.lock().unwrap();
        seen.clear();
    }
    assert_eq!(
        fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-1")),
        ImFedIngest::Duplicate,
        "DB 查重兜底（重启后缓存为空）"
    );
    // 关开关 → 丢弃（Ignored，不落库不推送）
    fed.set_dm_open(false);
    assert_eq!(
        fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-2")),
        ImFedIngest::Ignored,
        "对方未开放直通消息 → 丢弃"
    );
    let msgs = h
        .messages_snapshot()
        .into_iter()
        .filter(|m| is_dm_conversation(&m.conversation_id))
        .collect::<Vec<_>>();
    assert_eq!(msgs.len(), 1, "只落了第一条（去重 + 关开关不落）");
    assert_eq!(msgs[0].id, "dm-msg-1");
}

// DM9. 联邦大厅发送方 DM 路由登记（register_fed_sender_route）：登记后
//      对远端身份发起 DM 无需 to_node（跨节点私信从联邦大厅即可发起）
#[tokio::test]
async fn dm_fed_lobby_sender_route_registration() {
    let h = empty_handler();
    let fed = h.federation();
    let from_node =
        os_p2p::NodeId::parse(&pubkey_hex(&new_key())).expect("真公钥应可解析为 NodeID");
    let sender = pubkey_hex(&new_key());
    // 联邦大厅载荷（发送方=远端身份）→ 登记路由
    fed.register_fed_sender_route(
        &from_node,
        &serde_json::json!({
            "fed": FED_KIND_IM_FED_LOBBY,
            "node": "node-x",
            "message": {"sender_id": sender, "sender_name": "远端同学", "content": "hi"},
        }),
    );
    // 非 0x 身份（系统/agent）不登记
    fed.register_fed_sender_route(
        &from_node,
        &serde_json::json!({
            "fed": FED_KIND_IM_FED_LOBBY,
            "node": "node-x",
            "message": {"sender_id": "system", "content": "欢迎"},
        }),
    );
    // 本节点身份发起 DM：对方不在本节点，但已登记路由 → 走 p2p 定向
    //（P2P 未装配 → 503，恰好证明路由解析成功而非 404 无路由）
    let (_me, token) = login(&h, &new_key()).await;
    let resp = h
        .handle(authed_post(
            PATH_DM,
            &token,
            serde_json::json!({"to_pubkey": sender, "content": "从联邦大厅私聊你"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status, 503,
        "登记路由应被解析（503=P2P 未启用，非 404 无路由）"
    );
    let fresh = pubkey_hex(&new_key());
    let resp = h
        .handle(authed_post(
            PATH_DM,
            &token,
            serde_json::json!({"to_pubkey": fresh, "content": "无路由"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404, "未登记的远端身份仍无路由");
}
