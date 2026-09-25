//! NexHub 大厅测试套（2026-09-25 大文件拆分批随域迁移：整体迁 lobby/tests.rs
//! 单文件，与 film_hub/tests.rs 同款惯例；`use super::*` 经 mod.rs 私有 glob
//! use 链可达全部域符号，内容逐字节原样仅去一层 mod 包装缩进）。

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
async fn create_bounty(h: &NexHubLobbyRouteHandler, reward_sats: u64, currency: &str) -> String {
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
    let h =
        NexHubLobbyRouteHandler::with_db_path(&db_path, &dir).with_admin_token(TEST_ADMIN_TOKEN);
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
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
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
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
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
fn federate_fixture(repo: &str, readme: &str) -> (NexHubLobbyRouteHandler, Arc<CapturedTransport>) {
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
        let h = NexHubLobbyRouteHandler::with_repos_dir(&dir).with_admin_token(TEST_ADMIN_TOKEN);
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
async fn publish_paid_entry(h: &NexHubLobbyRouteHandler, name: &str, price: u64, currency: &str) {
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
async fn purchase(h: &NexHubLobbyRouteHandler, name: &str, body: serde_json::Value) -> ApiResponse {
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

// —— Merge 策略三态拓扑（2026-09-24 方案 §top4；git 级单测，issues.rs 另有
//    HTTP 级三策略测试）——

/// 造一个「main 已领先」的分离仓库：main = init + main2，feature = init +
/// featA + featB（merge-base=init，两分支各有独占提交——变基重放路径 fixture）。
fn make_diverged_repo(dir: &str, name: &str) -> String {
    let bare = format!("{dir}/{name}.git");
    assert!(run(&["git", "init", "--bare", &bare]).0);
    let work = format!("{dir}/.{name}-work");
    std::fs::create_dir_all(&work).unwrap();
    assert!(run(&["git", "-c", "init.defaultBranch=main", "init", &work]).0);
    let commit = |msg: &str| {
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
                msg
            ])
            .0
        );
    };
    std::fs::write(format!("{work}/README.md"), "# t\n").unwrap();
    commit("init");
    assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:main"]).0);
    assert!(run(&["git", "-C", &work, "checkout", "-q", "-b", "feature"]).0);
    std::fs::write(format!("{work}/a.txt"), "A\n").unwrap();
    commit("featA");
    std::fs::write(format!("{work}/b.txt"), "B\n").unwrap();
    commit("featB");
    assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:feature"]).0);
    assert!(run(&["git", "-C", &work, "checkout", "-q", "main"]).0);
    std::fs::write(format!("{work}/m.txt"), "M\n").unwrap();
    commit("main2");
    assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:main"]).0);
    let _ = std::fs::remove_dir_all(&work);
    bare
}

/// `git rev-list --parents -n 1 <ref>` → (commit, [parents…])。
fn head_parents(bare: &str, branch: &str) -> Vec<String> {
    let (ok, out) = run_git_sync(
        bare,
        &[
            "rev-list",
            "--parents",
            "-n",
            "1",
            &format!("refs/heads/{branch}"),
        ],
    );
    assert!(ok);
    out.split_whitespace().map(String::from).collect()
}

#[test]
fn merge_strategy_squash_single_commit_linear() {
    let dir = tempdir();
    let bare = make_diverged_repo(&dir, "sq");
    let sha = merge_with_strategy_blocking(
        &bare,
        "main",
        "feature",
        "feat batch (#1)",
        &MergeStrategy::Squash,
    )
    .expect("squash 应成功");
    let hp = head_parents(&bare, "main");
    assert_eq!(hp[0], sha, "main 头应即 squash 产物");
    assert_eq!(hp.len(), 2, "squash=单 parent（线性）: {hp:?}");
    // 内容合入：main 应同时有 feature 的 a.txt/b.txt 与自己的 m.txt
    for f in ["a.txt", "b.txt", "m.txt"] {
        let (ok, _) = run_git_sync(&bare, &["cat-file", "-e", &format!("refs/heads/main:{f}")]);
        assert!(ok, "squash 后 main 应含 {f}");
    }
    // 提交信息 = 自定义（缺省=PR 标题+#编号在 issues.rs 层测）
    let (_, msg) = run_git_sync(&bare, &["show", "-s", "--format=%B", &sha]);
    assert!(msg.contains("feat batch (#1)"), "信息应为自定义: {msg}");
    // 来源分支提交不进 main 历史（feature 头不是 main 的祖先；退出码 1=否）
    let (anc, _) = run_git_sync(
        &bare,
        &[
            "merge-base",
            "--is-ancestor",
            "refs/heads/feature",
            "refs/heads/main",
        ],
    );
    assert!(!anc, "squash 后 feature 头不应成为 main 的祖先");
}

#[test]
fn merge_strategy_rebase_fast_forward_keeps_shas() {
    // main 未领先（merge-base == main 头）→ rebase = 快进，原 sha 全保留
    let dir = tempdir();
    let bare = format!("{dir}/ff.git");
    assert!(run(&["git", "init", "--bare", &bare]).0);
    let work = format!("{dir}/.ff-work");
    std::fs::create_dir_all(&work).unwrap();
    assert!(run(&["git", "-c", "init.defaultBranch=main", "init", &work]).0);
    let commit = |msg: &str| {
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
                msg
            ])
            .0
        );
    };
    std::fs::write(format!("{work}/README.md"), "# t\n").unwrap();
    commit("init");
    assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:main"]).0);
    std::fs::write(format!("{work}/x.txt"), "X\n").unwrap();
    commit("one");
    assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:feature"]).0);
    let _ = std::fs::remove_dir_all(&work);
    let (_, feat) = run_git_sync(&bare, &["rev-parse", "refs/heads/feature"]);
    let sha = merge_with_strategy_blocking(&bare, "main", "feature", "", &MergeStrategy::Rebase)
        .expect("rebase 快进应成功");
    assert_eq!(sha, feat.trim(), "快进应直接指向 feature 头（sha 保留）");
    let hp = head_parents(&bare, "main");
    assert_eq!(hp.len(), 2, "快进后 main 头仍单 parent");
}

#[test]
fn merge_strategy_rebase_replay_is_linear_and_preserves_authors() {
    // main 领先 → 逐 commit 变基重放：线性拓扑 + 原 author/message 保留
    let dir = tempdir();
    let bare = make_diverged_repo(&dir, "rb");
    let sha = merge_with_strategy_blocking(&bare, "main", "feature", "", &MergeStrategy::Rebase)
        .expect("rebase 重放应成功");
    let hp = head_parents(&bare, "main");
    assert_eq!(hp[0], sha);
    assert_eq!(hp.len(), 2, "重放头单 parent: {hp:?}");
    // 线性历史：main 日志（旧→新）= init → main2 → featA → featB
    let (ok, out) = run_git_sync(
        &bare,
        &["log", "--reverse", "--format=%s", "refs/heads/main"],
    );
    assert!(ok);
    let subjects: Vec<&str> = out.lines().map(str::trim).collect();
    assert_eq!(
        subjects,
        vec!["init", "main2", "featA", "featB"],
        "rebase 重放应线性保留全部提交: {subjects:?}"
    );
    // 作者保留（fixture 作者 T <t@t>，committer 应为 NexHub）
    let (ok, out) = run_git_sync(&bare, &["show", "-s", "--format=%an|%cn", &sha]);
    assert!(ok);
    let parts: Vec<&str> = out.trim().split('|').collect();
    assert_eq!(parts[0], "T", "重放提交应保留原作者");
    assert_eq!(parts[1], "NexHub", "committer 应为 NexHub");
    // 每个重放提交都是单 parent
    let (ok, out) = run_git_sync(&bare, &["log", "--format=%P", "refs/heads/main"]);
    assert!(ok);
    assert!(
        out.lines().all(|l| l.trim().split(' ').count() <= 1),
        "全部提交单 parent（线性）: {out}"
    );
}

#[test]
fn merge_strategy_rebase_conflict_returns_409_prefix() {
    // 双方改同一行 → merge-tree 冲突 → Err 前缀「合并冲突」（issues.rs 转 409）
    let dir = tempdir();
    let bare = make_diverged_repo(&dir, "cf");
    // main 与 feature 同时改 README 同一行
    for (branch, text) in [("main", "MAIN\n"), ("feature", "FEAT\n")] {
        let work = format!("{dir}/.cf-{branch}");
        assert!(run(&["git", "clone", "-q", &bare, &work]).0);
        assert!(run(&["git", "-C", &work, "checkout", "-q", branch]).0);
        std::fs::write(format!("{work}/README.md"), text).unwrap();
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
                "clash-{branch}"
            ])
            .0
        );
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "push",
                "-q",
                "origin",
                &format!("HEAD:{branch}")
            ])
            .0
        );
        let _ = std::fs::remove_dir_all(&work);
    }
    let err = merge_with_strategy_blocking(&bare, "main", "feature", "", &MergeStrategy::Rebase)
        .expect_err("同文件冲突应 Err");
    assert!(
        err.starts_with("合并冲突"),
        "冲突错误前缀约定（调用方转 409）: {err}"
    );
    // squash 同样冲突
    let err = merge_with_strategy_blocking(&bare, "main", "feature", "x", &MergeStrategy::Squash)
        .expect_err("squash 冲突应 Err");
    assert!(err.starts_with("合并冲突"), "squash 冲突错误前缀: {err}");
}

#[test]
fn merge_strategy_parse_accepts_three_names() {
    assert_eq!(MergeStrategy::parse(None), Some(MergeStrategy::Merge));
    assert_eq!(MergeStrategy::parse(Some("")), Some(MergeStrategy::Merge));
    assert_eq!(
        MergeStrategy::parse(Some("merge")),
        Some(MergeStrategy::Merge)
    );
    assert_eq!(
        MergeStrategy::parse(Some("SQUASH")),
        Some(MergeStrategy::Squash)
    );
    assert_eq!(
        MergeStrategy::parse(Some(" rebase ")),
        Some(MergeStrategy::Rebase)
    );
    assert_eq!(MergeStrategy::parse(Some("bogus")), None);
}
