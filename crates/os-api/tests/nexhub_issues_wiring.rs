//! os-api 集成测——NexHub 项目级 Issues / Pull Requests 端点接线与鉴权
//! （2026-08-24，后端 `os_nexhub::issues`，文档 `docs/NEXHUB_ISSUES_PR.md`）。
//!
//! 验证三层契约（handler 单测在 os-nexhub，此处只测 **os-api 装配层**关心的部分）：
//!
//! 1. **路由接线**：`CodeRepoRouteHandler`（os-api main.rs 经 `register_component`
//!    注册的组件）声明 12 条 issues/pulls 路由，且全部 `requires_auth=false`
//!    ——网关中间件不拦链上 token 调用方（handler 内自验，同 nexhub-lobby 模式）；
//!    原生 coderepo 写路由仍 `requires_auth=true + admin`（不受协作层影响）。
//! 2. **网关强制鉴权不被绕过**：经 `InProcessGateway::dispatch` 完整链路
//!    （中间件 → 路由 → handler）：
//!     - 原生 admin 路由无身份 → 401（网关层拦）；
//!     - issues 公开读 → 200（无需任何身份）；
//!     - issues 写无 token → 401（handler 层拦）；
//!     - issues 写带链上 token → 201（author=pubkey 归因，body 自报忽略）。
//!
//! git fixture 真实 spawn 系统 git（与 os-nexhub 单测同栈），数据隔离到 tempdir。

use std::sync::Arc;

use os_api::gateway::{ApiRequest, Gateway, HttpMethod, RouteHandler};
use os_api::InProcessGateway;
use os_common::chain_auth::ChainAuth;
use os_nexhub::{CodeRepoRouteHandler, IssuesService};

/// 测试 admin token（注入 IssuesService，绕开 env 并行竞态）。
const TEST_ADMIN_TOKEN: &str = "os-api-issues-wiring-admin";

fn tempdir() -> String {
    let p = std::env::temp_dir().join(format!(
        "os-api-issues-wiring-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p.to_string_lossy().into_owned()
}

fn req(method: HttpMethod, path: &str, body: serde_json::Value) -> ApiRequest {
    ApiRequest {
        method,
        path: path.to_string(),
        headers: serde_json::json!({}),
        body,
        auth: None,
    }
}

fn req_bearer(method: HttpMethod, path: &str, token: &str, body: serde_json::Value) -> ApiRequest {
    let mut r = req(method, path, body);
    r.headers = serde_json::json!({ "authorization": format!("Bearer {token}") });
    r
}

/// 带 admin Principal 的请求（网关层身份——legacy admin 路由用）。
fn req_admin_principal(method: HttpMethod, path: &str, body: serde_json::Value) -> ApiRequest {
    use os_security::{Principal, Role, User, UserId};
    let now = chrono::Utc::now();
    let user = User::new(
        UserId::new("admin".to_string()),
        "admin".to_string(),
        vec![Role::Admin],
        now,
    )
    .unwrap();
    let principal = Principal::new(user, vec![Role::Admin], now).unwrap();
    let mut r = req(method, path, body);
    r.auth = Some(principal);
    r
}

/// 装配被测 handler 进网关（IssuesService 全注入：临时 DB / 仓库根 / 链上身份）。
async fn assembled(dir: &str, auth: Arc<ChainAuth>) -> InProcessGateway {
    let gw = InProcessGateway::new();
    let service = IssuesService::with_paths(
        &format!("{dir}/repo_issues.db"),
        &format!("{dir}/hub_lobby.db"),
        dir,
    )
    .with_admin_token(TEST_ADMIN_TOKEN)
    .with_chain_auth(auth);
    gw.register_component(
        "code_repo",
        Box::new(CodeRepoRouteHandler::with_issues(service)),
    )
    .await
    .expect("注册 code_repo 应成功");
    gw
}

/// 真实裸仓 fixture（main 分支 + feature 分支，系统 git spawn）。
fn make_repo(dir: &str, name: &str, extra_branch: bool) {
    let bare = format!("{dir}/{name}.git");
    let ok = |args: &[&str]| {
        matches!(
            std::process::Command::new(args[0]).args(&args[1..]).output(),
            Ok(o) if o.status.success()
        )
    };
    assert!(ok(&["git", "init", "--bare", &bare]));
    let work = format!("{dir}/.{name}-work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(format!("{work}/README.md"), "# wiring\n").unwrap();
    assert!(ok(&["git", "-c", "init.defaultBranch=main", "init", &work]));
    assert!(ok(&["git", "-C", &work, "add", "-A"]));
    assert!(ok(&[
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
    ]));
    assert!(ok(&["git", "-C", &work, "push", &bare, "HEAD:main"]));
    if extra_branch {
        std::fs::write(format!("{work}/feature.txt"), "feature\n").unwrap();
        assert!(ok(&["git", "-C", &work, "add", "-A"]));
        assert!(ok(&[
            "git",
            "-C",
            &work,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "feat"
        ]));
        assert!(ok(&["git", "-C", &work, "push", &bare, "HEAD:feature"]));
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ----------------------------------------------------------------------------
// 1) 路由接线：12 条协作路由 + 原生路由鉴权语义不变
// ----------------------------------------------------------------------------

#[tokio::test]
async fn routes_wire_issues_and_pulls_without_gateway_auth() {
    let h = CodeRepoRouteHandler::new();
    let routes = h.routes().await;
    let find = |m: HttpMethod, p: &str| {
        routes
            .iter()
            .find(|r| r.method == m && r.path == p)
            .unwrap_or_else(|| panic!("缺少路由 {p}"))
    };
    // 12 条协作路由全部挂 code_repo 组件且 handler 内自验（网关放行链上身份）
    for (m, p) in [
        (HttpMethod::Get, "/api/v1/coderepo/repos/:name/issues"),
        (HttpMethod::Post, "/api/v1/coderepo/repos/:name/issues"),
        (HttpMethod::Get, "/api/v1/coderepo/repos/:name/issues/:num"),
        (
            HttpMethod::Post,
            "/api/v1/coderepo/repos/:name/issues/:num/comments",
        ),
        (
            HttpMethod::Post,
            "/api/v1/coderepo/repos/:name/issues/:num/close",
        ),
        (
            HttpMethod::Post,
            "/api/v1/coderepo/repos/:name/issues/:num/open",
        ),
        (HttpMethod::Get, "/api/v1/coderepo/repos/:name/pulls"),
        (HttpMethod::Post, "/api/v1/coderepo/repos/:name/pulls"),
        (HttpMethod::Get, "/api/v1/coderepo/repos/:name/pulls/:num"),
        (
            HttpMethod::Post,
            "/api/v1/coderepo/repos/:name/pulls/:num/comments",
        ),
        (
            HttpMethod::Post,
            "/api/v1/coderepo/repos/:name/pulls/:num/merge",
        ),
        (
            HttpMethod::Post,
            "/api/v1/coderepo/repos/:name/pulls/:num/close",
        ),
    ] {
        let r = find(m, p);
        assert_eq!(r.handler_component, "code_repo", "{p} 应挂 code_repo 组件");
        assert!(
            !r.requires_auth,
            "{p} 应 handler 内自验（requires_auth=false）"
        );
        assert!(r.required_roles.is_empty(), "{p} 不应要求网关角色");
    }
    // 原生写路由语义不变（POST /repos 仍 admin）
    let legacy = find(HttpMethod::Post, "/api/v1/coderepo/repos");
    assert!(legacy.requires_auth);
    assert_eq!(legacy.required_roles, vec!["admin".to_string()]);
}

// ----------------------------------------------------------------------------
// 2) 网关全链路：公开读 / 身份写 / 无 token 401 / 网关层 admin 拦截不被绕过
// ----------------------------------------------------------------------------

#[tokio::test]
async fn gateway_dispatch_public_read_identity_write_and_auth_gates() {
    let dir = tempdir();
    make_repo(&dir, "demo", true);
    // 原生 coderepo 路由每请求读 env 仓库根——隔离到 tempdir，避免污染真实
    // /tank/git-repos（协作层路由用注入的 repos_root，不受影响）。
    std::env::set_var("NEXOS_GIT_REPOS_DIR", &dir);
    let auth = Arc::new(ChainAuth::new());
    let gw = assembled(&dir, auth.clone()).await;

    // —— issues 写无 token → handler 层 401（网关放行 requires_auth=false 路由，
    //    handler 自验链上 token / admin 回落）——
    let (resp, _) = gw
        .dispatch(req(
            HttpMethod::Post,
            "/api/v1/coderepo/repos/demo/issues",
            serde_json::json!({ "title": "bug" }),
        ))
        .await;
    assert_eq!(resp.status, 401, "无 token 建 Issue 应 401: {}", resp.body);

    // —— 链上身份写：直接在共享 ChainAuth 上签发 token（三步签名链路由
    //    nexhub-lobby 单测覆盖）→ 201 + author=pubkey 归因 + owner_kind=pubkey ——
    let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
    let pubkey = format!(
        "0x{}",
        hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
    );
    let (token, _) = auth.issue_token(&pubkey);
    let (resp, _) = gw
        .dispatch(req_bearer(
            HttpMethod::Post,
            "/api/v1/coderepo/repos/demo/issues",
            &token,
            serde_json::json!({
                "title": "构建失败",
                "body": "agent 巡检发现",
                "labels": ["bug"],
                "author": "fake-should-be-ignored"   // 自报 author 必须被忽略
            }),
        ))
        .await;
    assert_eq!(resp.status, 201, "链上身份建 Issue 应 201: {}", resp.body);
    assert_eq!(
        resp.body["issue"]["author"],
        pubkey.as_str(),
        "author 应为 token 反查 pubkey"
    );
    assert_eq!(resp.body["issue"]["owner_kind"], "pubkey");
    assert_eq!(resp.body["issue"]["number"], 1);

    // —— 评论（身份写）→ 201 ——
    let (resp, _) = gw
        .dispatch(req_bearer(
            HttpMethod::Post,
            "/api/v1/coderepo/repos/demo/issues/1/comments",
            &token,
            serde_json::json!({ "body": "我来复现" }),
        ))
        .await;
    assert_eq!(resp.status, 201, "链上身份评论应 201: {}", resp.body);

    // —— 公开读（无 token 无网关身份）：列表 + 详情（含评论）——
    let (resp, _) = gw
        .dispatch(req(
            HttpMethod::Get,
            "/api/v1/coderepo/repos/demo/issues",
            serde_json::Value::Null,
        ))
        .await;
    assert_eq!(resp.status, 200, "公开读 Issue 列表应 200: {}", resp.body);
    assert_eq!(resp.body["issues"].as_array().unwrap().len(), 1);
    assert_eq!(resp.body["issues"][0]["comment_count"], 1);
    let (resp, _) = gw
        .dispatch(req(
            HttpMethod::Get,
            "/api/v1/coderepo/repos/demo/issues/1",
            serde_json::Value::Null,
        ))
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body["comments"].as_array().unwrap().len(), 1);

    // —— PR：链上身份创建（feature→main）→ 非 owner merge 403 → admin token
    //    （服务层回落通道）merge 200 ——
    let (resp, _) = gw
        .dispatch(req_bearer(
            HttpMethod::Post,
            "/api/v1/coderepo/repos/demo/pulls",
            &token,
            serde_json::json!({ "title": "合入 feature", "from_branch": "feature" }),
        ))
        .await;
    assert_eq!(resp.status, 201, "链上身份建 PR 应 201: {}", resp.body);
    assert_eq!(
        resp.body["pull"]["to_branch"], "main",
        "to_branch 缺省应取仓库默认分支"
    );
    let (resp, _) = gw
        .dispatch(req_bearer(
            HttpMethod::Post,
            "/api/v1/coderepo/repos/demo/pulls/1/merge",
            &token,
            serde_json::json!({}),
        ))
        .await;
    assert_eq!(
        resp.status, 403,
        "无更改权限的链上身份 merge 应 403: {}",
        resp.body
    );
    let (resp, _) = gw
        .dispatch(req_bearer(
            HttpMethod::Post,
            "/api/v1/coderepo/repos/demo/pulls/1/merge",
            TEST_ADMIN_TOKEN,
            serde_json::json!({}),
        ))
        .await;
    assert_eq!(resp.status, 200, "admin merge 应 200: {}", resp.body);
    assert_eq!(resp.body["state"], "merged");
    assert_eq!(resp.body["merged_by"], "admin");

    // —— 网关层强制鉴权未被协作层绕过：原生 admin 路由无身份仍 401 ——
    let (resp, _) = gw
        .dispatch(req(
            HttpMethod::Post,
            "/api/v1/coderepo/repos",
            serde_json::json!({ "name": "another" }),
        ))
        .await;
    assert_eq!(resp.status, 401, "原生 admin 路由无身份应被网关拦 401");
    // 带 admin Principal → 201（真实 git init --bare）
    let (resp, _) = gw
        .dispatch(req_admin_principal(
            HttpMethod::Post,
            "/api/v1/coderepo/repos",
            serde_json::json!({ "name": "another", "description": "wiring" }),
        ))
        .await;
    assert_eq!(
        resp.status, 201,
        "admin Principal 建仓应 201: {}",
        resp.body
    );
    assert!(
        std::path::Path::new(&format!("{dir}/another.git")).is_dir(),
        "裸仓库应真实落地（env 隔离的 tempdir）"
    );
    std::env::remove_var("NEXOS_GIT_REPOS_DIR");
    std::env::remove_var("OS_GIT_REPOS_DIR");
}
