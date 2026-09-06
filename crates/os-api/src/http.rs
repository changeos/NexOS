//! 真实 HTTP 服务端适配（Axum 集成，规划文档 §3.6 / §9.1#10）。
//!
//! 本模块把 [`crate::gateway_impl::InProcessGateway`] 的"内部分发算法"接到真实的
//! `axum::Router` + `axum::serve` 上，完成"接通真实实现"的目标。
//!
//! 设计要点：
//! - **内嵌网关不独立成层**（§9.1#10 红线）：Router 由网关在 `start` 时构建并直接
//!   `axum::serve` 监听于 osd 进程内，不拆为独立服务（架构变更须 ADR）。
//! - **路由表聚合 → Router**：各组件经 `RouteHandler` 声明的路由规格被映射到
//!   axum 路由（含 `:param`/`*` 通配转换）；每条路由共用同一个 `dispatch_handler`，
//!   它把 `axum::Request` 还原为 [`crate::gateway::ApiRequest`] 后调用
//!   `InProcessGateway::dispatch`（保留中间件链 + 路由匹配 + 组件分发）。
//!   这样既复用既有分发算法（保持 57 测不变），又得到真实 HTTP/WS 监听能力。
//! - **WebSocket**：可选地把一个路径挂为 axum WS 升级端点，握手时强制校验
//!   `?user=<pubkey>&token=<IM token>`（IM 区块链认证，失败 401），成功后接入
//!   [`crate::ws_impl::WsHub`] 的订阅-广播通道（见 [`ws_handler`]）。
//! - **终端 WebSocket**：`/ws/terminal/{session_id}`（「管理」应用 Web 终端，
//!   始终挂载）——握手即验 `?token=<admin token>`（`NEXOS_ADMIN_TOKEN` 精确
//!   匹配，失败 401），通过后把浏览器 xterm.js 的 JSON 帧接到 PTY 会话
//!   （input 写 PTY / output 聚合推流 / resize 同步 / exit 终结，见
//!   [`terminal_ws_handler`] 与 docs/ADMIN_CONSOLE.md）。
//! - **JWT 认证**：HTTP 入口从 `Authorization: Bearer <token>` 头解析 JWT
//!   （经注入的 `os_security::JwtIssuer`），把 `Principal` 填充进 `ApiRequest.auth`，
//!   下游 `AuthMiddleware`/路由鉴权直接消费（避免中间件链每层重复解析）。
//!
//! TLS 终止：rustls/openssl TLS 加载在 workspace 未启用对应 feature，留 TODO。

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{
        ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, Request, State,
    },
    http::Method,
    response::{IntoResponse, Response},
    Router,
};
use os_security::{JwtIssuer, JwtIssuerImpl, Principal, Role, User, UserId};

use crate::gateway::{ApiRequest, ApiResponse, HttpMethod};
use crate::gateway_impl::InProcessGateway;

// ----------------------------------------------------------------------------
// HTTP 方法转换
// ----------------------------------------------------------------------------

/// axum `Method` → 内部 `HttpMethod`；不支持的动词返回 None。
pub(crate) fn method_from_axum(m: &Method) -> Option<HttpMethod> {
    match *m {
        Method::GET => Some(HttpMethod::Get),
        Method::POST => Some(HttpMethod::Post),
        Method::PUT => Some(HttpMethod::Put),
        Method::DELETE => Some(HttpMethod::Delete),
        Method::PATCH => Some(HttpMethod::Patch),
        _ => None,
    }
}

/// 内部 `HttpMethod` → axum `Method`。
pub(crate) fn method_to_axum(m: HttpMethod) -> Method {
    match m {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Patch => Method::PATCH,
    }
}

// ----------------------------------------------------------------------------
// Router 构建：把 RouteRegistry 映射为 axum 路由
// ----------------------------------------------------------------------------

/// 内部辅助：把内部路径模式（`:param` / `*`）转换为 axum 路径模式。
///
/// axum 0.8 起参数段语法从 `:name` 改为 `{name}`；catch-all 用 `{*name}`。
/// - `:name` 段转为 `{name}`（保留参数名）
/// - `*` 段（catch-all）转为 `{*wildcard}`（axum 通配语法）
/// - 其余原样保留
fn to_axum_path_pattern(internal: &str) -> String {
    // 去掉可能的 query 串（防御性）
    let internal = internal.split('?').next().unwrap_or(internal);
    let mut out = String::with_capacity(internal.len());
    for seg in internal.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        if seg == "*" {
            // axum catch-all：`{*name}` 必须出现在末尾
            out.push_str("{*wildcard}");
        } else if let Some(name) = seg.strip_prefix(':') {
            // `:id` → `{id}`（axum 0.8 新语法）
            out.push('{');
            out.push_str(name);
            out.push('}');
        } else {
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        out
    }
}

/// 网关的 axum 共享状态：持有网关自身 + 可选 JWT issuer。
///
/// 用 `Arc<InProcessGateway>` 而非 `&self`：axum handler 必须 `'static`，
/// 网关需在 `start` 内被同时持有与共享。
#[derive(Clone)]
pub struct GatewayState {
    /// 网关本体（Arc 共享，handler 与 serve 各持一份）
    pub gateway: Arc<InProcessGateway>,
    /// JWT 签发/校验器（None = 不做 HTTP 入口 JWT 解析）
    pub jwt: Option<Arc<JwtIssuerImpl>>,
    /// 固定 admin token（来自 OS_ADMIN_TOKEN 环境变量）。
    ///
    /// 漏洞2 修复：当请求头 `Authorization: Bearer <token>` 与本字段精确匹配时，
    /// 注入一个 admin Principal（绕过 JWT 解析）。这是最简单的鉴权引导方案，
    /// 避免依赖复杂的 JWT 签发链路即可让鉴权强制生效。
    /// None = 不启用固定 admin token（仅 JWT 鉴权）。
    pub admin_token: Option<Arc<String>>,
    /// Git Smart HTTP（`/git/*`）的仓库根目录覆盖。
    ///
    /// None = 沿用 [`os_nexhub::repos_dir`]（`NEXOS_GIT_REPOS_DIR`
    /// / `OS_GIT_REPOS_DIR` / `/tank/git-repos`），与 SSH clone 共用同一批裸仓库
    /// （NexHub 独立化后该函数随 code_repo 迁至 os-nexhub）。
    /// 注入点主要供单测隔离（避免测试写真实 `/tank/git-repos`）。
    pub git_repos_root: Option<String>,
    /// API 网关共享实例（SSE 流式转发用，2026-08-31）。
    ///
    /// `POST /api/v1/gateway/v1/{chat/,}completions` 的 `stream:true` 请求由
    /// [`gateway_openai_handler`] 逐块透传——鉴权/选路/计费必须与组件内非流式
    /// 转发**同一实例**（`Mutex<Connection>` 是查-扣原子的边界，两个实例各持
    /// 一条连接会引入 SELECT→UPDATE 竞态，同 media-gen 共享模式）。main.rs 经
    /// `InProcessGateway::set_api_gateway` 注入；None = 未装配（流式请求回落
    /// 非流式整包路径，行为同旧版）。
    pub api_gateway: Option<Arc<crate::handlers::api_gateway::ApiGatewayRouteHandler>>,
    /// LLM 外部 API 接入共享状态（SSE 流式直通用，2026-08-31）。
    ///
    /// `POST /api/v1/llm/external-apis/{id}/chat` 的 `stream:true` 请求由
    /// [`crate::handlers::llm_external::chat_stream_handler`] 逐块透传——查行
    /// （base_url/api_key）必须与 "llm" 组件的 REST CRUD **同一实例**（同一条
    /// `Mutex<Connection>`，api_gateway 同款共享模式）。main.rs 经
    /// `InProcessGateway::set_llm_external` 注入；None = 未装配（流式请求回落
    /// 组件整包非流式路径）。
    pub llm_external: Option<Arc<crate::handlers::llm_external::LlmExternalState>>,
}

// ----------------------------------------------------------------------------
// JWT → Principal 解析
// ----------------------------------------------------------------------------

/// 从 `Authorization: Bearer <token>` 头解析出 `Principal`。
///
/// 鉴权顺序（漏洞2 修复）：
/// 1. 若 `admin_token` 提供，且 bearer 与之精确匹配 → 注入 admin Principal。
/// 2. 否则用 `JwtIssuer` 解析 JWT；失败返回 None。
///
/// 失败（无头/格式错/JWT 校验失败且无 admin_token 匹配）返回 None；网关随后由
/// `AuthMiddleware`/路由 `requires_auth` 决策 401。
pub(crate) async fn extract_principal(
    headers: &serde_json::Value,
    jwt: Option<&Arc<JwtIssuerImpl>>,
    admin_token: Option<&Arc<String>>,
) -> Option<Principal> {
    // 测试期默认最高权限（用户 2026-08-26 指示：所有鉴权先取消，都默认 admin，
    // 不手动输 token）：无 Authorization 头时直接注入 admin Principal。
    // env `NEXOS_AUTH_DEFAULT_ADMIN=0` 可关闭；带凭据的请求仍走正常比对
    //（错误 token 照样拒绝——只豁免「完全不带凭据」的调用）。
    if std::env::var("NEXOS_AUTH_DEFAULT_ADMIN")
        .map(|v| v != "0")
        .unwrap_or(true)
        && headers.get("authorization").is_none()
        && headers.get("Authorization").is_none()
    {
        let now = chrono::Utc::now();
        let roles = vec![Role::Admin];
        if let Ok(user) = User::new(
            UserId::new("admin".to_string()),
            "admin".to_string(),
            roles.clone(),
            now,
        ) {
            if let Ok(p) = Principal::new(user, roles, now) {
                return Some(p);
            }
        }
    }

    // 头部键名在 request_to_api 已统一为小写；此处两种都查（防御性）
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.as_str())
        .or_else(|| headers.get("Authorization").and_then(|v| v.as_str()))?;
    let token = auth_header.strip_prefix("Bearer ").or_else(|| {
        // 大小写宽容：bearer
        auth_header.strip_prefix("bearer ")
    })?;

    // 1) 固定 admin token 优先（OS_ADMIN_TOKEN）：精确匹配 → 注入 admin Principal。
    if let Some(expected) = admin_token {
        if token == expected.as_str() && !expected.is_empty() {
            let now = chrono::Utc::now();
            let roles = vec![Role::Admin];
            let user = User::new(
                UserId::new("admin".to_string()),
                "admin".to_string(),
                roles.clone(),
                now,
            )
            .ok()?;
            return Principal::new(user, roles, now).ok();
        }
    }

    // 2) JWT 解析
    let issuer = jwt?;
    let claims = issuer.verify(token).await.ok()?;
    // JWT 已校验签名/过期，直接信任 claims 构造 Principal（不再次查用户库）
    let now = chrono::Utc::now();
    let user = User::new(
        UserId::new(claims.sub.as_str().to_string()),
        claims.sub.as_str().to_string(),
        claims.roles.clone(),
        now,
    )
    .ok()?;
    Principal::new(user, claims.roles, now).ok()
}

// ----------------------------------------------------------------------------
// axum 请求 → ApiRequest 转换
// ----------------------------------------------------------------------------

/// 把 axum `Request` 还原为内部 `ApiRequest`（含 JWT 解析出的 `Principal`）。
///
/// 注：路径参数提取由内部 `RouteRegistry` 在 `dispatch` 阶段重新匹配完成
/// （保持单一真相），这里只把 URL/方法/头/体传齐。
async fn request_to_api(
    req: Request,
    state: &GatewayState,
) -> Result<ApiRequest, (axum::http::StatusCode, String)> {
    // 拆解 Request（method/uri 在 parts，body 单独读）
    let (parts, body_axum) = req.into_parts();

    let method = method_from_axum(&parts.method);
    let method = match method {
        Some(m) => m,
        None => {
            return Err((
                axum::http::StatusCode::METHOD_NOT_ALLOWED,
                "method not allowed".to_string(),
            ))
        }
    };

    // 路径（含 query）
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    // 头部 → serde_json::Value（对象：小写键名以便下游一致读取）
    let mut headers_map = serde_json::Map::new();
    for (name, value) in &parts.headers {
        let key = name.as_str().to_lowercase();
        if let Ok(s) = value.to_str() {
            headers_map.insert(key, serde_json::Value::String(s.to_string()));
        }
    }
    let headers = serde_json::Value::Object(headers_map);

    // body → JSON；非 JSON 体以字符串承载（保持可读，不强失败）
    let bytes = match axum::body::to_bytes(body_axum, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!("read body failed: {e}"),
            ))
        }
    };
    let body = decode_body(&bytes);

    // JWT / admin_token 解析（若配置了 issuer 或 admin_token）
    let auth = extract_principal(&headers, state.jwt.as_ref(), state.admin_token.as_ref()).await;

    Ok(ApiRequest {
        method,
        path,
        headers,
        body,
        auth,
    })
}

/// 把请求体字节解码为 JSON；空体/解析失败回退为字符串/null（不破坏分发）。
fn decode_body(bytes: &Bytes) -> serde_json::Value {
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    // 先尝试 JSON
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        return v;
    }
    // 回退为 UTF-8 字符串
    match std::str::from_utf8(bytes) {
        Ok(s) => serde_json::Value::String(s.to_string()),
        Err(_) => serde_json::Value::Null,
    }
}

// ----------------------------------------------------------------------------
// ApiResponse → axum Response 转换
// ----------------------------------------------------------------------------

/// 直传（raw passthrough）判定的载荷字节：Some = 按原文直传，None = 走 JSON 序列化。
///
/// 约束极窄，只对「handler 显式声明了直传 content-type 且 body 是 JSON 字符串」的
/// 响应生效——全部既有对象型响应零影响：
/// - `text/*`（如 `text/x-shellscript`）与两个文本形应用 MIME（
///   `image/svg+xml`、`application/json`，2026-09-04 应用静态资源托管
///   `/apps-assets/:id/*` 引入——js/css 天然 `text/*`，svg/json 需显式列出）：
///   body 字符串按 UTF-8 原样返回——浏览器 `<script src>` / `fetch().json()`
///   需要原文而非 JSON 引号包裹的字符串；
/// - `application/octet-stream` / `image/png` / `font/woff2`：body 字符串按
///   标准 base64 解码后返回原始字节（JSON 信封无法承载任意二进制；
///   png/woff2 为应用静态资源二进制白名单）。解码失败回退 JSON 序列化。
fn direct_passthrough_bytes(resp: &ApiResponse) -> Option<Vec<u8>> {
    let serde_json::Value::String(s) = &resp.body else {
        return None;
    };
    let serde_json::Value::Object(map) = &resp.headers else {
        return None;
    };
    let ct = map
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))?
        .1
        .as_str()?
        .to_ascii_lowercase();
    if ct.starts_with("text/") || ct == "image/svg+xml" || ct == "application/json" {
        return Some(s.clone().into_bytes());
    }
    if matches!(
        ct.as_str(),
        "application/octet-stream" | "image/png" | "font/woff2"
    ) {
        use base64::Engine as _;
        return base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .ok();
    }
    None
}

fn api_to_response(resp: ApiResponse) -> Response {
    let mut builder = axum::response::Response::builder().status(resp.status);
    // 透传 headers（若为对象）
    if let serde_json::Value::Object(map) = &resp.headers {
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                builder = builder.header(k.as_str(), s);
            }
        }
    }
    // 直传响应：content-type 已由 handler 声明并在上方透传，不再追加 application/json
    if let Some(bytes) = direct_passthrough_bytes(&resp) {
        return builder
            .body(axum::body::Body::from(bytes))
            .unwrap_or_else(|_| {
                axum::response::Response::builder()
                    .status(500)
                    .body(axum::body::Body::from("internal error"))
                    .unwrap()
            });
    }
    // 默认 application/json；body 用紧凑 JSON 序列化
    let body_bytes = serde_json::to_vec(&resp.body).unwrap_or_else(|_| b"null".to_vec());
    builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    builder
        .body(axum::body::Body::from(body_bytes))
        .unwrap_or_else(|_| {
            axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from("internal error"))
                .unwrap()
        })
}

// ----------------------------------------------------------------------------
// axum handler：单兜底分发
// ----------------------------------------------------------------------------

/// 单一分发 handler：所有注册路由共用——把 axum 请求转回 ApiRequest，
/// 调用 `dispatch`，再把响应转回 axum。
pub async fn dispatch_handler(State(state): State<GatewayState>, req: Request) -> Response {
    let api_req = match request_to_api(req, &state).await {
        Ok(r) => r,
        Err((code, msg)) => {
            return (code, msg).into_response();
        }
    };
    let (resp, _matched) = state.gateway.dispatch(api_req).await;
    api_to_response(resp)
}

// ----------------------------------------------------------------------------
// WebSocket handler：握手 → WsHub 订阅
// ----------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
pub struct UserQuery {
    user: Option<String>,
    /// IM token（`POST /api/v1/im/auth/verify` 签发；设计 §2 握手强制）。
    token: Option<String>,
}

/// axum WS 升级 handler：`?user=<pubkey>&token=<IM token>` 握手即验，
/// 通过后调用 `WsHub::subscribe_raw`，把 `WsMessage` 序列化为文本帧推给
/// 客户端；客户端断开时 `unsubscribe`。
///
/// 认证（设计 docs/IM_BLOCKCHAIN_AUTH_DESIGN.md §2，一次性破坏性变更）：
/// token 必须有效且反查出的 pubkey 与 `user` 精确一致（经网关共享的
/// [`crate::handlers::im::ImAuth`] 校验）。缺失/无效/不匹配 → 握手 401 拒绝
/// （旧裸 `?user=` 不带 token 一律拒绝，无兼容通道）。
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<GatewayState>,
    Query(params): Query<UserQuery>,
) -> Response {
    let authorized_user = params.user.zip(params.token).filter(|(user, token)| {
        state
            .gateway
            .im_auth()
            .is_some_and(|auth| auth.verify_ws(user, token))
    });
    let Some((user, _token)) = authorized_user else {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "im: WS 握手需要 ?user=<pubkey>&token=<IM token>（先 POST /api/v1/im/auth/challenge + /auth/verify）",
        )
            .into_response();
    };
    let hub = state.gateway.ws_hub();
    ws.on_upgrade(move |socket| run_ws(socket, hub, user))
}

/// WS 连接生命周期：订阅 → 转发 → 退出时取消订阅。
async fn run_ws(mut socket: WebSocket, hub: crate::ws_impl::WsHub, user: String) {
    let (sub_id, mut rx) = hub.subscribe_raw(&user);
    loop {
        tokio::select! {
            // WsHub 推送的消息 → 序列化后写回客户端
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
                        let text = match serde_json::to_string(&msg) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        if socket.send(AxumWsMessage::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 慢消费者丢消息，继续（保持连接）
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // 客户端消息：本网关不做业务上行（只推），收到 Close/None 即退出；
            // 任意其他客户端帧（含协议层 Ping/Pong）= 连接活性证据 → 刷新订阅
            // last_active（agent-coord online 新鲜度判定的数据源。出向 send
            // 不刷新——半开 TCP 下写进发送缓冲仍"成功"，不构成对端存活证据）
            msg = socket.recv() => {
                match msg {
                    Some(Ok(AxumWsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => hub.touch_raw(sub_id),
                    Some(Err(_)) => {}
                }
            }
        }
    }
    hub.unsubscribe_raw(sub_id);
}

// ----------------------------------------------------------------------------
// 终端 WebSocket handler：「管理」应用 Web 终端（PTY ↔ xterm.js 桥）
// ----------------------------------------------------------------------------

/// `/ws/terminal/{session_id}` 握手 query（只认 admin token）。
#[derive(serde::Deserialize, Default)]
pub struct TerminalWsQuery {
    token: Option<String>,
}

/// 终端 WS 升级 handler：`?token=<admin token>` 握手即验（与 REST 端点的
/// `Authorization: Bearer` 同源——`NEXOS_ADMIN_TOKEN`；终端 = 最高权限面，
/// 不设 admin_token 的部署一律拒绝，无匿名通道），通过后按会话 id 接入
/// PTY 输出广播 + 终端输入回写。
///
/// 协议（JSON 文本帧，详见 handlers/terminal.rs 与 docs/ADMIN_CONSOLE.md）：
/// - 上行 `{"type":"input","data":"<base64>"}` / `{"type":"resize","cols":N,"rows":N}`
/// - 下行 `{"type":"output","data":"<base64>"}` / `{"type":"exit","code":N}`
///   / `{"type":"error","msg":"..."}`
pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<GatewayState>,
    Path(session_id): Path<String>,
    Query(params): Query<TerminalWsQuery>,
) -> Response {
    // 鉴权：token 与 admin_token 精确匹配（None/空/不匹配均 401——默认拒绝）。
    // 测试期豁免（与 extract_principal 默认 admin 同开关 NEXOS_AUTH_DEFAULT_ADMIN，
    // 未设或非 "0" 即启用）：空 query 也放行——Web 终端前端可能无 token 可带
    // （localStorage 无旧值），不能把用户锁死在门外。
    let default_admin = !std::env::var("NEXOS_AUTH_DEFAULT_ADMIN")
        .map(|v| v == "0")
        .unwrap_or(false);
    let authorized = match (&state.admin_token, params.token.as_deref()) {
        (Some(expected), Some(token)) => !expected.is_empty() && token == expected.as_str(),
        _ => false,
    };
    if !authorized && !default_admin {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "terminal: WS 握手需要 ?token=<admin token>（测试期默认放行；关闭设 NEXOS_AUTH_DEFAULT_ADMIN=0）",
        )
            .into_response();
    }
    // 会话存在性校验（升级前拒绝，客户端能拿到 HTTP 404 而非 WS 空转）。
    let sessions = crate::handlers::terminal::TerminalSessions::shared();
    let Some(session) = sessions.get(&session_id) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("terminal: 终端会话不存在: {session_id}"),
        )
            .into_response();
    };
    ws.on_upgrade(move |socket| run_terminal_ws(socket, session))
}

/// 终端 WS 连接生命周期：PTY 输出广播 → 下行帧；上行帧 → input 写 PTY /
/// resize 调 PTY。exit 帧后关闭连接；客户端断开只结束本连接（会话保留，
/// 刷新/重连可续用，显式关闭走 DELETE /api/v1/terminal/sessions/:id）。
async fn run_terminal_ws(
    mut socket: WebSocket,
    session: Arc<crate::handlers::terminal::PtySession>,
) {
    use crate::handlers::terminal::{ClientFrame, ServerFrame};
    let mut rx = session.subscribe();
    loop {
        tokio::select! {
            // PTY 输出/exit/error 帧 → 序列化写回客户端
            frame = rx.recv() => {
                match frame {
                    Ok(f) => {
                        let text = match serde_json::to_string(&f) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let is_exit = matches!(f, ServerFrame::Exit { .. });
                        if socket.send(AxumWsMessage::Text(text.into())).await.is_err() {
                            break;
                        }
                        if is_exit {
                            break; // exit 帧即终点：子进程已退出，关连接
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 慢消费者丢帧：提示后继续（流式最新语义，不阻塞写端）
                        let text = serde_json::to_string(&ServerFrame::Error {
                            msg: "终端输出过快，连接缓冲溢出（已丢部分历史帧）".to_string(),
                        })
                        .unwrap_or_else(|_| "{}".to_string());
                        if socket.send(AxumWsMessage::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // 客户端上行帧
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(AxumWsMessage::Close(_))) => break,
                    Some(Ok(AxumWsMessage::Text(text))) => {
                        match serde_json::from_str::<ClientFrame>(&text) {
                            Ok(ClientFrame::Input { data }) => {
                                // base64 → 字节；写 PTY 是阻塞 IO → spawn_blocking。
                                let bytes = match crate::handlers::terminal::ws_input_decode(&data) {
                                    Ok(b) => b,
                                    Err(msg) => {
                                        let err = serde_json::to_string(&ServerFrame::Error { msg })
                                            .unwrap_or_else(|_| "{}".to_string());
                                        if socket.send(AxumWsMessage::Text(err.into())).await.is_err() {
                                            break;
                                        }
                                        continue;
                                    }
                                };
                                let s = session.clone();
                                let write_err = tokio::task::spawn_blocking(move || {
                                    s.write_input(&bytes).err()
                                })
                                .await
                                .ok()
                                .flatten();
                                if let Some(msg) = write_err {
                                    let err = serde_json::to_string(&ServerFrame::Error { msg })
                                        .unwrap_or_else(|_| "{}".to_string());
                                    if socket.send(AxumWsMessage::Text(err.into())).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Ok(ClientFrame::Resize { cols, rows }) => {
                                if let Err(msg) = session.resize(cols, rows) {
                                    let err = serde_json::to_string(&ServerFrame::Error { msg })
                                        .unwrap_or_else(|_| "{}".to_string());
                                    if socket.send(AxumWsMessage::Text(err.into())).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                let err = serde_json::to_string(&ServerFrame::Error {
                                    msg: format!("帧解析失败（需 input/resize JSON 帧）: {e}"),
                                })
                                .unwrap_or_else(|_| "{}".to_string());
                                if socket.send(AxumWsMessage::Text(err.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(AxumWsMessage::Binary(_))) => {
                        let err = serde_json::to_string(&ServerFrame::Error {
                            msg: "终端 WS 仅支持 JSON 文本帧".to_string(),
                        })
                        .unwrap_or_else(|_| "{}".to_string());
                        if socket.send(AxumWsMessage::Text(err.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {} // 协议层 Ping/Pong 由 axum 自动应答
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Git Smart HTTP：/git/* → 系统 git-http-backend（CGI）
// ----------------------------------------------------------------------------

/// git 请求体上限（1 GiB）——防御异常超大 push 把内存打爆。
const GIT_HTTP_MAX_BODY: usize = 1024 * 1024 * 1024;

/// 定位系统 `git-http-backend`（缓存一次）。
///
/// 优先探测 Debian/Ubuntu 固定路径 `/usr/lib/git-core/git-http-backend`；缺失时
/// 用 `git --exec-path` 解析（跨发行版）。均不可用 → None（handler 降级 503）。
fn git_http_backend_path() -> Option<&'static str> {
    use std::sync::OnceLock;
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| {
        let direct = "/usr/lib/git-core/git-http-backend";
        if std::path::Path::new(direct).is_file() {
            return Some(direct.to_string());
        }
        std::process::Command::new("git")
            .arg("--exec-path")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| format!("{}/git-http-backend", s.trim()))
            .filter(|p| std::path::Path::new(p).is_file())
    })
    .as_deref()
}

/// 仅解码 `%XX`（不做 `+`→空格——那是 query 语义，路径解码不适用）。
fn percent_decode_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析 `/git/<repo>[.git]/<endpoint>` 为 CGI `PATH_INFO` 相对路径。
///
/// 规则（安全优先）：
/// - 去掉 `/git/` 前缀，percent-decode 后校验；**任意路径段为 `..`/`.`/空 → 拒绝**
///   （含 `%2e%2e` 编码穿越；axum 通配段给的是原始编码路径，须自行解码后校验）。
/// - 端点白名单（Smart HTTP 协议仅三个）：`info/refs`、`git-upload-pack`、
///   `git-receive-pack`——拒绝 dumb 协议的 `HEAD`/`objects/*` 任意文件读。
/// - 仓库名无 `.git` 后缀时自动补齐（`/git/nexos` 与 `/git/nexos.git` 等价）；
///   不可以 `-` 开头（防御 git 参数注入）。
///
/// 返回 `Ok("<name>.git/<endpoint>")`（无前导 `/`）。
fn parse_git_path(uri_path: &str) -> Result<String, String> {
    let Some(rest) = uri_path.strip_prefix("/git/") else {
        return Err(format!("非 /git/ 路径: {uri_path}"));
    };
    // percent-decode 后再校验（拦截 %2e%2e / %2f 编码穿越），剥掉至多一个前导 '/'
    let decoded = percent_decode_path(rest);
    let decoded = decoded.strip_prefix('/').unwrap_or(&decoded);
    if decoded.is_empty() {
        return Err("空 git 路径".into());
    }
    let segs: Vec<&str> = decoded.split('/').collect();
    if segs.iter().any(|s| s.is_empty() || *s == ".." || *s == ".") {
        return Err(format!("路径含非法段（禁止 .. 穿越）: {uri_path}"));
    }
    if segs.len() < 2 {
        return Err(format!("缺少端点后缀（info/refs 等）: {uri_path}"));
    }
    let endpoint = segs[1..].join("/");
    if !matches!(
        endpoint.as_str(),
        "info/refs" | "git-upload-pack" | "git-receive-pack"
    ) {
        return Err(format!("非法 git 端点（仅 Smart HTTP 三端点）: {endpoint}"));
    }
    let mut repo = segs[0].to_string();
    if !repo.ends_with(".git") {
        repo.push_str(".git");
    }
    if repo.starts_with('-') {
        return Err("仓库名不可以 '-' 开头".into());
    }
    Ok(format!("{repo}/{endpoint}"))
}

/// git 鉴权失败类型（响应在调用点构造，避免大 Err 变体）。
enum GitAuthFail {
    /// token 缺失/不匹配 → 401 + WWW-Authenticate（git CLI 弹认证）。
    Unauthorized,
    /// admin_token 未配置 → 503（git 访问强依赖 token，不留匿名通道）。
    Disabled,
}

/// 校验 git HTTP **写**鉴权（仅 receive-pack/push 路径调用；upload-pack
/// 只读路径在 handler 内匿名放行，不经本函数）。
///
/// - `Authorization: Bearer <token>`：与 `state.admin_token` 精确匹配
/// - `Authorization: Basic <b64(user:pwd)>`：**密码字段** = admin token
///   （git CLI 收到 401 + `WWW-Authenticate: Basic` 后弹账号密码，用户名任意）
///
/// 通过返回 `REMOTE_USER`（固定 `nexos-agent`，git-http-backend 据此放行 push）。
fn git_authenticate(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<String, GitAuthFail> {
    let Some(expected) = state.admin_token.as_ref().filter(|t| !t.is_empty()) else {
        return Err(GitAuthFail::Disabled);
    };
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let matched = if let Some(token) = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    {
        token == expected.as_str()
    } else if let Some(b64) = auth.strip_prefix("Basic ") {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64.trim().as_bytes())
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|cred| cred.split_once(':').map(|(_, pwd)| pwd.to_string()))
            .is_some_and(|pwd| pwd == expected.as_str())
    } else {
        false
    };
    if matched {
        Ok("nexos-agent".to_string())
    } else {
        Err(GitAuthFail::Unauthorized)
    }
}

/// 把鉴权失败转为响应（401 带 `WWW-Authenticate: Basic`，让 git CLI 弹认证）。
fn git_auth_fail_response(fail: GitAuthFail) -> Response {
    match fail {
        GitAuthFail::Unauthorized => (
            axum::http::StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                r#"Basic realm="NexHub Git""#,
            )],
            "unauthorized: 需要 Authorization: Bearer <NEXOS_ADMIN_TOKEN>（或 Basic，密码填 token）",
        )
            .into_response(),
        GitAuthFail::Disabled => git_text_response(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "git http disabled: NEXOS_ADMIN_TOKEN 未配置（git 访问强制要求 token）",
        ),
    }
}

/// git-http-backend CGI 环境参数（聚合为结构体，避免长参数列表）。
struct GitCgiParams<'a> {
    /// 仓库根目录（`GIT_PROJECT_ROOT`）。
    project_root: &'a str,
    /// `/<repo>.git/<endpoint>`（含前导 `/`，`PATH_INFO`）。
    path_info: &'a str,
    /// HTTP 方法（`REQUEST_METHOD`）。
    method: &'a str,
    /// 原样查询串（`QUERY_STRING`）。
    query: &'a str,
    /// 认证用户（`REMOTE_USER`）。
    remote_user: &'a str,
    /// `Content-Type`（POST 时）。
    content_type: Option<&'a str>,
    /// `Content-Encoding`（gzip 请求体时透传）。
    content_encoding: Option<&'a str>,
    /// `Git-Protocol`（协议 v2 版本协商）。
    git_protocol: Option<&'a str>,
    /// 请求体字节数（`CONTENT_LENGTH`）。
    content_length: Option<u64>,
}

/// CGI 环境变量构造（纯函数，便于单测）。
///
/// 见 `git help git-http-backend`：`GIT_PROJECT_ROOT` + `PATH_INFO` 定位仓库，
/// `GIT_HTTP_EXPORT_ALL` 跳过 git-daemon-export-ok 检查，`REMOTE_USER` 决定
/// receive-pack（push）是否放行，`HTTP_GIT_PROTOCOL` 透传协议版本（v2）。
#[must_use]
fn build_cgi_env(p: &GitCgiParams) -> Vec<(String, String)> {
    let mut env = vec![
        ("GIT_PROJECT_ROOT".to_string(), p.project_root.to_string()),
        ("GIT_HTTP_EXPORT_ALL".to_string(), "1".to_string()),
        ("PATH_INFO".to_string(), p.path_info.to_string()),
        ("QUERY_STRING".to_string(), p.query.to_string()),
        ("REQUEST_METHOD".to_string(), p.method.to_string()),
        ("REMOTE_USER".to_string(), p.remote_user.to_string()),
        ("REMOTE_ADDR".to_string(), "127.0.0.1".to_string()),
    ];
    if let Some(ct) = p.content_type {
        env.push(("CONTENT_TYPE".into(), ct.to_string()));
    }
    if let Some(ce) = p.content_encoding {
        env.push(("HTTP_CONTENT_ENCODING".into(), ce.to_string()));
    }
    if let Some(gp) = p.git_protocol {
        env.push(("HTTP_GIT_PROTOCOL".into(), gp.to_string()));
    }
    if let Some(len) = p.content_length {
        env.push(("CONTENT_LENGTH".into(), len.to_string()));
    }
    env
}

/// 解析后的 CGI 响应（头区与 body 已分离）。
struct CgiResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// 在字节串中查找子串首现位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 解析 git-http-backend 的 CGI 输出（`headers\r\n\r\nbody`）。
///
/// - 头区以首个空行（`\r\n\r\n`，宽容 `\n\n`）结束；找不到 → None（非 CGI 输出）
/// - `Status: <code> ...` 头决定状态码（缺省 200），其余头原样透传
/// - body 为二进制（pkt-line / pack），按字节切分不受 UTF-8 约束
fn parse_cgi_output(out: &[u8]) -> Option<CgiResponse> {
    let (head_end, sep_len) = if let Some(p) = find_subslice(out, b"\r\n\r\n") {
        (p, 4)
    } else {
        find_subslice(out, b"\n\n").map(|p| (p, 2))?
    };
    let head = std::str::from_utf8(&out[..head_end]).ok()?;
    let body = out[head_end + sep_len..].to_vec();

    let mut status = 200u16;
    let mut headers = Vec::new();
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("status") {
            // "Status: 404 Not Found" → 取首个 token 为状态码
            if let Some(code) = value.split_whitespace().next().and_then(|c| c.parse().ok()) {
                status = code;
            }
            continue;
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Some(CgiResponse {
        status,
        headers,
        body,
    })
}

/// git 错误响应（纯文本，带 UTF-8 Content-Type）。
fn git_text_response(code: axum::http::StatusCode, msg: &str) -> Response {
    (
        code,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        msg.to_string(),
    )
        .into_response()
}

/// Git Smart HTTP handler：`/git/<repo>[.git]/<endpoint>` → 系统
/// `git-http-backend`（CGI）。
///
/// 让 AI agent 免 SSH 直接 `git clone http://host:8080/git/<repo>.git` /
/// `git push`。四类请求全走本 handler，**按读写分流鉴权**（git 托管惯例：
/// 读匿名放行、写必须 token——拉取不应鉴权，推送才需要；无写权限的外部
/// 贡献者走 Issues/PR 流程，见 docs/NEXHUB_ISSUES_PR.md）：
///
/// | 请求 | 用途 | 鉴权 |
/// |------|------|------|
/// | `GET  /git/<r>.git/info/refs?service=git-upload-pack`   | clone/fetch 握手 | **匿名放行**（只读）|
/// | `POST /git/<r>.git/git-upload-pack`                     | clone/fetch 数据 | **匿名放行**（只读）|
/// | `GET  /git/<r>.git/info/refs?service=git-receive-pack`  | push 握手 | 必须 token（401 触发凭据提示）|
/// | `POST /git/<r>.git/git-receive-pack`                    | push 数据 | 必须 token |
///
/// 流程：路径安全校验（.. 穿越 + 端点白名单，匿名请求同样必须过）→ 按读写
/// 分流鉴权（receive-pack 两路径 = Bearer/Basic = NEXOS_ADMIN_TOKEN，现行
/// 逻辑全保留；upload-pack = REMOTE_USER `anonymous`）→ 构造 CGI 环境
/// spawn git-http-backend（POST body 写 stdin，写完关管道给 EOF）→ 解析 CGI
/// 输出（headers/body 分离）转 axum Response。
async fn git_http_handler(State(state): State<GatewayState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    // 1) 路径解析（.. 穿越防护 + 端点白名单）——先于鉴权：读写分流依据路径
    //    决定，且匿名读请求同样必须过安全校验（穿越/白名单拦截不因匿名放松）
    let git_path = match parse_git_path(parts.uri.path()) {
        Ok(p) => p,
        Err(msg) => return git_text_response(axum::http::StatusCode::BAD_REQUEST, &msg),
    };

    // 2) 读写分流鉴权：请求含 receive-pack（POST push 数据路径，或
    //    info/refs?service=git-receive-pack push 握手）→ 现行 token 鉴权全
    //    保留；否则（upload-pack：clone/fetch 握手与数据）→ 匿名放行。
    //    端点白名单只有三端点，故 ends_with("/git-receive-pack") 精确圈定
    //    push 数据路径；query 只需含 git-receive-pack（service 参数）。
    let is_push = git_path.ends_with("/git-receive-pack")
        || parts
            .uri
            .query()
            .is_some_and(|q| q.contains("git-receive-pack"));
    let remote_user = if is_push {
        match git_authenticate(&state, &parts.headers) {
            Ok(u) => u,
            Err(fail) => return git_auth_fail_response(fail),
        }
    } else {
        // 匿名只读（clone/fetch）：REMOTE_USER=anonymous。receive-pack 端点
        // 到不了这里（上面已 401），git-http-backend 对 upload-pack 不看
        // REMOTE_USER，纯标记用途。
        "anonymous".to_string()
    };

    // 3) git-http-backend 可用性（缺失降级 503）
    let Some(backend) = git_http_backend_path() else {
        return git_text_response(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "git-http-backend 不可用（未安装 git 或找不到 git-http-backend）",
        );
    };

    // 4) 读取请求体（POST：pack 数据；GET：空）
    let body_bytes = match axum::body::to_bytes(body, GIT_HTTP_MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return git_text_response(
                axum::http::StatusCode::BAD_REQUEST,
                &format!("读取 git 请求体失败: {e}"),
            )
        }
    };

    let header_str = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let repos_root = state
        .git_repos_root
        .clone()
        .unwrap_or_else(os_nexhub::repos_dir);
    let envs = build_cgi_env(&GitCgiParams {
        project_root: &repos_root,
        path_info: &format!("/{git_path}"),
        method: parts.method.as_str(),
        query: parts.uri.query().unwrap_or(""),
        remote_user: &remote_user,
        content_type: header_str("content-type").as_deref(),
        content_encoding: header_str("content-encoding").as_deref(),
        git_protocol: header_str("git-protocol").as_deref(),
        content_length: (!body_bytes.is_empty()).then(|| body_bytes.len() as u64),
    });

    // 5) spawn git-http-backend：请求体 → stdin；stdout = CGI 响应
    let mut cmd = tokio::process::Command::new(backend);
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return git_text_response(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                &format!("spawn git-http-backend 失败: {e}"),
            )
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // 写完立即 drop → EOF（git-http-backend 依赖 stdin EOF 判断 pack 边界）
        if !body_bytes.is_empty() {
            let _ = stdin.write_all(&body_bytes).await;
        }
        drop(stdin);
    }
    let output = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            return git_text_response(
                axum::http::StatusCode::BAD_GATEWAY,
                &format!("git-http-backend 执行失败: {e}"),
            )
        }
    };

    // 6) CGI 输出 → axum Response（无合法头区 → 502 + stderr 诊断）
    match parse_cgi_output(&output.stdout) {
        Some(cgi) => {
            // push 成功（receive-pack **数据路径** CGI 200）→ 旁路自动触发内置
            // CI（NexHub v0.1.33）。只认 POST 数据路径（git_path 精确后缀）——
            // info/refs?service=git-receive-pack 握手同样带 receive-pack 字样
            // 且返回 200，若一起算每次 push 会双触发（握手时 refs 未更新，
            // 还会先跑出一条空 clone 的 skipped）。CI 是旁路：入队失败只记
            // [ci] 日志，绝不影响 push 响应；env NEXOS_CI_AUTO_PUSH=0 可关。
            if git_path.ends_with("/git-receive-pack") && cgi.status == 200 {
                let repo = git_path
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(".git")
                    .to_string();
                crate::handlers::nexhub_ci::push_hook(&repo);
            }
            let mut builder = axum::response::Response::builder().status(cgi.status);
            for (k, v) in cgi.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            if !builder
                .headers_ref()
                .is_some_and(|h| h.contains_key(axum::http::header::CONTENT_TYPE))
            {
                builder = builder.header(axum::http::header::CONTENT_TYPE, "text/plain");
            }
            builder
                .body(axum::body::Body::from(cgi.body))
                .unwrap_or_else(|_| {
                    git_text_response(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "CGI 响应构造失败（非法状态码？）",
                    )
                })
        }
        None => git_text_response(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!(
                "git-http-backend 输出无法解析（exit={:?}）: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
    }
}

// ----------------------------------------------------------------------------
// 网关 SSE 流式转发：POST /api/v1/gateway/v1/{chat/,}completions（stream:true）
// ----------------------------------------------------------------------------

/// 流式透传时保留的 SSE 文本**尾部**窗口（usage 解析用）。
///
/// OpenAI 语义下 usage 在流末尾的 data 块下发，只需保留尾部即可；不无限累计
/// （超长流不会把内存吃穿），64 KiB 远大于任何 usage 块。
const SSE_TAIL_KEEP: usize = 64 * 1024;

/// 请求体大小上限（与非流式路径 `usize::MAX` 不同：流式路径必须先整读 body
/// 做 stream 判定 + 选路，给个 64 MiB 上限防御异常超大请求把内存打爆）。
const GATEWAY_STREAM_MAX_BODY: usize = 64 * 1024 * 1024;

/// axum `HeaderMap` → 小写键名 JSON 对象（与 `request_to_api` 同一规约，
/// 供 `resolve_forward_plan` 读 `authorization`）。
fn headers_to_json(headers: &axum::http::HeaderMap) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_lowercase();
        if let Ok(s) = value.to_str() {
            map.insert(key, serde_json::Value::String(s.to_string()));
        }
    }
    serde_json::Value::Object(map)
}

/// 流式转发的记账状态：透传的同时保留 SSE 尾部窗口，流结束时解析 usage、
/// 记调用日志 + 扣配额（与非流式 [`crate::handlers::api_gateway`] 的
/// `record_success`/`record_failure` 同一套 DB 写入）。
struct StreamAccounting {
    /// 网关共享实例（记日志/扣配额）。
    gw: std::sync::Arc<crate::handlers::api_gateway::ApiGatewayRouteHandler>,
    token: crate::handlers::api_gateway::ApiToken,
    channel: crate::handlers::api_gateway::Channel,
    /// 对外模型名（记日志/查倍率）。
    model: String,
    started: std::time::Instant,
    /// SSE 文本尾部窗口（原始字节，任意边界可切）。
    tail: Vec<u8>,
    /// 终态守卫：成功/失败各记一次，不重复。
    recorded: bool,
}

impl StreamAccounting {
    fn push_tail(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > SSE_TAIL_KEEP {
            let cut = self.tail.len() - SSE_TAIL_KEEP;
            self.tail.drain(..cut);
        }
    }

    /// 流正常结束：从尾部窗口解析 usage（[`parse_stream_usage`]）；上游未上报
    /// 则记 0 并在日志 error 字段注明（真实数据铁律：不估算不编造）。
    fn record_success(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let usage = crate::handlers::api_gateway::parse_stream_usage(&String::from_utf8_lossy(
            &self.tail,
        ));
        let note = if usage.is_some() {
            None
        } else {
            Some(crate::handlers::api_gateway::STREAM_USAGE_MISSING_NOTE)
        };
        let (pt, ct, tt) = usage.unwrap_or((0, 0, 0));
        self.gw.record_success(
            &self.token,
            &self.channel,
            &self.model,
            pt,
            ct,
            tt,
            self.started.elapsed().as_millis() as u64,
            note,
        );
    }

    /// 流中途失败：记 failed 日志（流已对客户端开吐，不再切换渠道）。
    fn record_failure(&mut self, err: &str) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        self.gw.record_failure(&self.token, &self.channel, &self.model, err);
    }
}

/// 上游流打开后的统一形态（直连 / 中继汇流）：`(HTTP 状态, content-type, 字节流)`。
/// 直连路径把 `reqwest::Error` 在打开时映射 String（Display 文案不变），与
/// 中继分块流同一项类型——下游 [`sse_forward_stream`] 的透传语义零差异。
type OpenedUpstream = Result<
    (
        u16,
        String,
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, String>> + Send>>,
    ),
    String,
>;

/// 网关 OpenAI 兼容入口的流式特挂 handler。
///
/// 职责分派：
/// - `stream:true`（body JSON 顶层布尔）→ **真流式**：鉴权/选路复用网关的
///   `resolve_forward_plan`（与非流式完全同口径），逐块透传 `text/event-stream`
///   （直连渠道 reqwest `bytes_stream()`；**中继渠道**（via_node 非空，2026-09-03）
///   经 api_market relay stream:true 分块回传——首帧 Head 即上游响应头/状态，
///   后续 chunk 透传语义与直连完全一致），计费从 SSE 末块 usage 解析；
/// - 其余（非流式 / body 非 JSON / 网关实例未装配）→ 原样重建请求交
///   [`dispatch_handler`]（走组件内 `proxy_forward` 整包路径，零行为差）。
///
/// # 故障转移边界（首字节语义，注释即契约）
///
/// - **首字节前**（连接失败 / 非 2xx / 首个数据块读取失败或为空）：记 failed
///   日志后切换下一候选渠道——此时还没对客户端承诺任何字节，切换安全；
/// - **首字节后**（首个数据块已到手，响应开始透传）：**不再切换**——切换会把
///   两个渠道的流拼在一起（串流），客户端无法解析。上游中途断流只断开连接
///   （末尾补一条 `: gateway: ...` SSE 注释帧便于排查，SSE 解析器忽略注释），
///   记 failed 日志。
pub async fn gateway_openai_handler(State(state): State<GatewayState>, req: Request) -> Response {
    use futures::StreamExt;

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, GATEWAY_STREAM_MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("读取请求体失败: {e}"),
            )
                .into_response()
        }
    };
    // stream 判定：仅认 JSON 顶层布尔 true；其余一律走非流式整包路径
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
    let wants_stream = parsed
        .as_ref()
        .and_then(|v| v.get("stream"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // 网关共享实例未装配（纯测试 Router 等）→ 流式能力不可用，回落整包路径
    //（行为同旧版：整包收完按 JSON 返回）。
    let Some(gw) = state.api_gateway.clone() else {
        return dispatch_handler(
            State(state),
            Request::from_parts(parts, axum::body::Body::from(bytes)),
        )
        .await;
    };
    if !wants_stream {
        return dispatch_handler(
            State(state),
            Request::from_parts(parts, axum::body::Body::from(bytes)),
        )
        .await;
    }

    // —— 流式路径 ——
    let headers = headers_to_json(&parts.headers);
    // 解析失败的非 JSON body 不该走到这（wants_stream 已保证是 JSON 对象），
    // 防御性回落 dispatch 让 proxy_forward 给出它的"缺少 model"类错误。
    let Some(body_json) = parsed.filter(|v| v.is_object()) else {
        return dispatch_handler(
            State(state),
            Request::from_parts(parts, axum::body::Body::from(bytes)),
        )
        .await;
    };
    let suffix = if parts.uri.path().ends_with("chat/completions") {
        "chat/completions"
    } else {
        "completions"
    };
    let plan = match gw.resolve_forward_plan(&headers, &body_json) {
        Ok(p) => p,
        Err(resp) => return api_to_response(resp), // 401/429/400/403/404 同非流式
    };
    let started = std::time::Instant::now();

    let mut last_err = String::from("无渠道可转发");
    for ch in &plan.ordered {
        // 映射命中时把请求体 model 覆盖为上游真实名（与非流式同款）
        let mut fwd_body = body_json.clone();
        if let Some(up) = &plan.upstream_model_override {
            if let serde_json::Value::Object(ref mut map) = fwd_body {
                map.insert("model".into(), serde_json::Value::String(up.clone()));
            }
        }
        // —— 打开上游流（直连 / 中继二选一，统一为「状态 + 响应头 + 字节流」）——
        // 直连：reqwest bytes_stream（错误映射 String，与中继同项类型）。
        // 中继（via_node 非空）：relay stream:true 分块回传（首帧 Head 已被
        // open_channel_relay_stream 消化为状态/响应头；后续 chunk 逐块取）。
        // 两条路径的错误面统一 `Result<Bytes, String>`，下游 sse_forward_stream
        // 的透传语义不变（逐块写客户端 + 尾部窗口 usage 记账）。
        let opened: OpenedUpstream = if ch.via_node.trim().is_empty() {
            match crate::handlers::api_gateway::ApiGatewayRouteHandler::open_upstream_stream(
                &ch.base_url,
                &ch.api_key,
                suffix,
                &fwd_body,
            )
            .await
            {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let code = resp.status().as_u16();
                        // 错误体只取前 200 字符记日志；读错误体加 5s 超时（上游挂着
                        // 不回也能快速转移到下一渠道，不被单渠道拖死）
                        let detail = match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            resp.text(),
                        )
                        .await
                        {
                            Ok(Ok(text)) => text,
                            Ok(Err(_)) | Err(_) => String::new(),
                        };
                        let detail = detail.chars().take(200).collect::<String>();
                        Err(format!("上游返回错误: HTTP {code} {detail}"))
                    } else {
                        let content_type = resp
                            .headers()
                            .get(axum::http::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("text/event-stream")
                            .to_string();
                        let status = resp.status().as_u16();
                        let stream = resp
                            .bytes_stream()
                            .map(|r| r.map_err(|e| e.to_string()));
                        Ok((status, content_type, Box::pin(stream)))
                    }
                }
                Err(e) => Err(e),
            }
        } else {
            match gw.open_channel_relay_stream(ch, suffix, &fwd_body).await {
                Ok(mut rs) => {
                    if !(200..300).contains(&rs.status) {
                        // 上游非 2xx：源端把错误体单帧收尾——聚合读完记日志（未对
                        // 客户端吐字节，仍可故障转移）
                        let code = rs.status;
                        let mut detail = Vec::new();
                        while let Some(chunk) = rs.next_chunk().await {
                            match chunk {
                                Ok(b) => detail.extend_from_slice(&b),
                                Err(e) => {
                                    detail.extend_from_slice(e.as_bytes());
                                    break;
                                }
                            }
                        }
                        let detail: String =
                            String::from_utf8_lossy(&detail).chars().take(200).collect();
                        Err(format!("上游返回错误: HTTP {code} {detail}"))
                    } else {
                        let content_type = rs
                            .headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| "text/event-stream".to_string());
                        let status = rs.status;
                        Ok((status, content_type, Box::pin(relay_chunk_stream(rs))))
                    }
                }
                Err(e) => Err(e),
            }
        };
        let (status, content_type, stream) = match opened {
            Ok(v) => v,
            Err(e) => {
                // 首字节前失败：转移下一渠道
                last_err = e;
                gw.record_failure(&plan.token, ch, &plan.model, &last_err);
                continue;
            }
        };
        debug_assert!((200..300).contains(&status));
        // 2xx：拿到流。**先读首个数据块再承诺响应**——这是故障转移的最后边界。
        let mut stream = stream;
        // 首块读取加 120s 超时：思考模型 TTFT 可达数十秒，给足余量；超时视为
        // 该渠道失败 → 仍可故障转移（还没对客户端吐任何字节）
        let first = match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            stream.next(),
        )
        .await
        {
            Ok(Some(Ok(b))) => b,
            Ok(Some(Err(e))) => {
                last_err = format!("上游流读取失败（首字节前）: {e}");
                gw.record_failure(&plan.token, ch, &plan.model, &last_err);
                continue;
            }
            Ok(None) => {
                last_err = "上游流在首字节前结束（空响应体）".into();
                gw.record_failure(&plan.token, ch, &plan.model, &last_err);
                continue;
            }
            Err(_) => {
                last_err = "上游首字节超时（120s 无数据）".into();
                gw.record_failure(&plan.token, ch, &plan.model, &last_err);
                continue;
            }
        };
        // 首块在手：构建透传 body（首块 + 余流，流结束/出错时记账），
        // 自此不再切换渠道（切了会串流，见函数级注释）。
        let acc = StreamAccounting {
            gw,
            token: plan.token.clone(),
            channel: ch.clone(),
            model: plan.model.clone(),
            started,
            tail: Vec::new(),
            recorded: false,
        };
        let body_stream = sse_forward_stream(first, stream, acc);
        return axum::http::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap_or_else(|_| {
                axum::http::Response::builder()
                    .status(500)
                    .body(axum::body::Body::from("internal error"))
                    .unwrap()
            });
    }
    // 全渠道失败 → 502（与非流式同文案结构）
    api_to_response(crate::handlers::api_gateway::gateway_error_response(
        502,
        &format!("所有渠道转发失败: {last_err}"),
    ))
}

/// RelayStream → futures 流（空闲超时在 next_chunk 内部执行；Err 即断流，
/// 语义与 llm_external 的 relay_chunk_stream 同款——直连/中继在
/// [`gateway_openai_handler`] 汇成同一 `Result<Bytes, String>` 项类型）。
fn relay_chunk_stream(
    rs: crate::handlers::api_market::RelayStream,
) -> impl futures::Stream<Item = Result<Bytes, String>> {
    futures::stream::unfold(Some(rs), |mut st| async move {
        let stream = st.as_mut()?;
        match stream.next_chunk().await {
            Some(Ok(b)) => Some((Ok(Bytes::from(b)), st)),
            Some(Err(e)) => {
                eprintln!("[gateway-sse] 中继上游流中断: {e}");
                st = None; // 断流收尾（Err 项由 sse 注释帧语义收底后即结束）
                Some((Err(e), st))
            }
            None => None, // 正常收尾
        }
    })
}

/// 组装透传流：首块 + 上游余流 → `Result<Bytes, String>` 流。
///
/// 透传时保留 SSE 尾部窗口（usage 解析）；上游流**正常结束** → 记成功日志 +
/// 扣配额（usage 从尾部解析，未上报记 0 并注明）；**中途错误** → 记失败日志、
/// 末尾补一条 `: gateway: ...` SSE 注释帧（SSE 规范忽略 `:` 开头的注释行，
/// 不污染客户端数据帧）后收尾断流——此时已对客户端开吐，不再切换渠道。
///
/// 项类型 `Result<Bytes, String>`（2026-09-03 渠道中继）：直连路径把
/// `reqwest::Error` 在上游打开时即映射 String（Display 文案不变），与中继
/// 分块流同一项类型——两条路径汇流后再无类型分叉，透传语义零变化。
fn sse_forward_stream<S>(
    first: Bytes,
    rest: S,
    mut acc: StreamAccounting,
) -> impl futures::Stream<Item = Result<Bytes, String>>
where
    S: futures::Stream<Item = Result<Bytes, String>> + Send + 'static,
{
    use futures::StreamExt;

    struct FwdState {
        /// 首字节前读到的首个数据块（故障转移边界之后才吐给客户端）。
        first: Option<Bytes>,
        inner:
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, String>> + Send>>,
        acc: StreamAccounting,
        /// 已吐出错误注释帧 → 下一次 poll 直接收尾（记账只做一次）。
        errored: bool,
    }
    // 首块先记入尾部窗口（它也可能是含 usage 的块），再组装状态
    acc.push_tail(&first);
    let st = FwdState {
        first: Some(first),
        inner: Box::pin(rest),
        acc,
        errored: false,
    };
    futures::stream::unfold(st, |mut st| async move {
        if let Some(b) = st.first.take() {
            return Some((Ok(b), st));
        }
        if st.errored {
            return None;
        }
        match st.inner.next().await {
            Some(Ok(chunk)) => {
                st.acc.push_tail(&chunk);
                Some((Ok(chunk), st))
            }
            Some(Err(e)) => {
                st.acc.record_failure(&format!("上游流中途断开: {e}"));
                st.errored = true;
                let comment = Bytes::from(format!(": gateway: upstream stream aborted ({e})\n\n"));
                Some((Ok(comment), st))
            }
            None => {
                st.acc.record_success();
                None
            }
        }
    })
}

// ----------------------------------------------------------------------------
// 构建完整 Router
// ----------------------------------------------------------------------------

/// 构建真实的 axum `Router`（含全部注册路由 + 可选 WS 端点 + Web UI fallback）。
///
/// - 每条 `RouteSpec` 的 `method + path` 被映射为一条 axum 路由（共享同一
///   `dispatch_handler`）；axum 用自身匹配树完成静态/参数段路由分发。
/// - 注册的 WS 路径（如 `/ws`）以 `any(ws_handler)` 挂载。
/// - **Web UI fallback**：所有未匹配显式 API 路由的 `GET` 请求交给
///   `static_handler`——`GET /` 返回内嵌 `index.html`，`GET /static/<x>`
///   返回对应静态资源，其余未匹配路径回 404。API 路由（`/api/*`、`/status`、
///   `/healthz` 等显式注册项）优先匹配，不受 fallback 影响。
///
/// 注：本函数**不**调用 `axum::serve`（仅构建 Router），便于用
/// `tower::ServiceExt::oneshot` 做无监听端口的单测。
pub fn build_router(state: GatewayState, ws_path: Option<&str>) -> Router {
    let mut router = Router::new();

    // 1) WS 端点
    if let Some(p) = ws_path {
        router = router.route(p, axum::routing::any(ws_handler));
    }

    // 1b) 终端 WS：「管理」应用 Web 终端（PTY ↔ xterm.js），`?token=<admin
    //     token>` 握手即验（见 terminal_ws_handler）。始终挂载（不随 ws_path
    //     开关——终端有独立鉴权面，REST 端点由 terminal 组件路由表声明）。
    router = router.route(
        "/ws/terminal/{session_id}",
        axum::routing::any(terminal_ws_handler),
    );

    // 1c) 直播 WS：「直播」应用（/ws/live/{room_id}/{publish|view}，主播 webm
    //     chunk 上行扇出 / 观众 MSE 下行；升级前鉴权与契约见
    //     handlers/live.rs::live_ws_handler）。始终挂载（同终端 WS 模式）。
    router = router.route(
        "/ws/live/{room_id}/{action}",
        axum::routing::any(crate::handlers::live::live_ws_handler),
    );

    // 2) Git Smart HTTP：/git/<repo>.git/<endpoint> → git-http-backend（CGI）。
    //    放在 fallback 之前（显式路由优先于 fallback）；axum 0.8 catch-all 语法 {*path}。
    router = router.route("/git/{*path}", axum::routing::any(git_http_handler));

    // 2b) 网关 SSE 流式转发：POST /api/v1/gateway/v1/{chat/,}completions 的
    //     `stream:true` 请求逐块透传（见 gateway_openai_handler）。同路径与下方
    //     RouteSpec 循环重叠——特挂路由优先，spec 循环里跳过这两条（重复
    //     route 同 method 会 panic）。
    router = router.route(
        "/api/v1/gateway/v1/chat/completions",
        axum::routing::post(gateway_openai_handler),
    );
    router = router.route(
        "/api/v1/gateway/v1/completions",
        axum::routing::post(gateway_openai_handler),
    );

    // 2c) LLM 外部 API 对话直通（SSE 流式特挂，网关 2b 同款手法）：POST
    //     /api/v1/llm/external-apis/{id}/chat 的 `stream:true` 请求逐块透传
    //     （见 handlers/llm_external.rs::chat_stream_handler）。非流式/未装配
    //     状态在 handler 内回落 dispatch_handler（组件整包路径，行为一致）；
    //     spec 循环里跳过这条避免 axum 路由重复注册 panic。
    router = router.route(
        "/api/v1/llm/external-apis/{id}/chat",
        axum::routing::post(crate::handlers::llm_external::chat_stream_handler),
    );

    // 3) 各 RouteSpec → axum 路由（同 method+path 共享 dispatch_handler）
    let routes = state.gateway.list_routes_inner();
    for spec in routes {
        // 2b 的特挂路由已占用这两条 POST（流式判定 + 非流式回落 dispatch），
        // 这里跳过避免 axum 路由重复注册 panic。
        if spec.method == crate::gateway::HttpMethod::Post
            && matches!(
                spec.path.as_str(),
                "/api/v1/gateway/v1/chat/completions" | "/api/v1/gateway/v1/completions"
            )
        {
            continue;
        }
        // 2c 的特挂路由已占用这条 POST（流式透传 + 非流式回落 dispatch），
        // 这里跳过避免 axum 路由重复注册 panic。
        if spec.method == crate::gateway::HttpMethod::Post
            && spec.path == "/api/v1/llm/external-apis/:id/chat"
        {
            continue;
        }
        let path = to_axum_path_pattern(&spec.path);
        let method = method_to_axum(spec.method);
        router = router.route(&path, method_router_for(method));
    }

    // 4) Web UI fallback：未匹配 API 的 GET 请求交给静态资源 handler。
    //    fallback 只在没有任何显式路由匹配时触发，不会拦截已注册的 API。
    router = router.fallback(static_handler);

    router.with_state(state)
}

/// Web UI 静态资源 handler（作为 axum fallback）。
///
/// 仅处理 `GET`/`HEAD`；其余方法回 405（不劫持 POST/PUT 等已注册 API 的分发——
/// 那些由显式路由先匹配，fallback 仅在无显式路由命中时触发）。
///
/// 路径解析（顺序）：
/// 1. 非 GET/HEAD → 405
/// 2. `/` → `index.html`（Web UI 入口）
/// 3. 任意路径 → 用相对路径查内嵌资源（覆盖 `/assets/xxx` / `/static/xxx` /
///    `/favicon.svg` 等）；命中即返回
/// 4. **SPA fallback**：未命中静态资源、且非 API 路径（不以 `/api/` 开头）的
///    GET/HEAD 请求 → 返回 `index.html`，交由 Vue Router 在浏览器端处理前端
///    路由（如 `/storage` `/vms` `/shares`）。已注册的 API 路由（`/api/*`、
///    `/status`、`/healthz`、`/shares`、`/discover` 等）由显式路由优先匹配，
///    不会落到这里；此处仅对形如 API 的路径（`/api/...`）做防御性 404，
///    避免把打错的 API 调用静默降级为 HTML。
async fn static_handler(req: Request) -> Response {
    // 非 GET/HEAD（如对未注册路径的 POST）→ 405，避免静默吞掉写动词
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return (
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
        )
            .into_response();
    }

    let path = req.uri().path();

    // / → index.html（Web UI 入口）
    if path == "/" {
        return match crate::webui::get_asset("index.html") {
            Some((data, mime)) => serve_bytes(data, mime),
            None => not_found(),
        };
    }

    // 去掉前导 "/"，得到相对路径
    let rel = path.trim_start_matches('/');

    // 直接用相对路径查内嵌资源（覆盖 /assets/xxx /static/xxx /favicon.svg 等）
    if !rel.is_empty() {
        if let Some((data, mime)) = crate::webui::get_asset(rel) {
            return serve_bytes(data, mime);
        }
        // 尝试去掉 static/ 前缀（旧版兼容）
        if let Some(key) = rel.strip_prefix("static/") {
            if !key.is_empty() {
                if let Some((data, mime)) = crate::webui::get_asset(key) {
                    return serve_bytes(data, mime);
                }
            }
        }
    }

    // SPA fallback：前端路由（/storage /vms /shares 等）由 Vue Router 在浏览器
    // 端处理——后端对"看起来像前端路由"的未匹配 GET/HEAD 返回 index.html。
    // 仅对以下情形返回 404（不降级为 HTML）：
    //   - 形如 API 的路径（/api/...）：前端打错 API 时应见 404 而非 HTML
    //   - 静态资源形路径（含扩展名，或位于 /assets/ /static/ 资源目录下）：
    //     缺失的 .css/.js 应返回 404，避免浏览器把 HTML 当样式/脚本解析
    if is_api_path(path) || is_static_asset_path(path) {
        return not_found();
    }
    match crate::webui::get_asset("index.html") {
        Some((data, mime)) => serve_bytes(data, mime),
        None => not_found(),
    }
}

/// 判断路径是否"形如 API"（应返回 404 而非 SPA fallback）。
///
/// 已注册的 API 路由（`/api/*`、`/status`、`/healthz`、`/shares`、`/discover` 等）
/// 由显式路由优先匹配，不会到达 `static_handler`；这里只对 **未注册** 的 API 形
/// 路径做防御性 404，避免把打错的 API 调用静默降级为 HTML（前端难以排查）。
/// `/api/` 前缀覆盖全部 `/api/v1/*` 资源；其余已注册 API 已被显式路由消费。
fn is_api_path(path: &str) -> bool {
    path.starts_with("/api/") || path == "/api"
}

/// 判断路径是否"形如静态资源"（缺失时应返回 404 而非 SPA fallback）。
///
/// 标准：
/// - 位于构建产物的资源目录下（`/assets/...`、`/static/...`）：必然是资源请求，
///   缺失 → 404（避免浏览器把 index.html 当 .css/.js 解析报 MIME 错）。
/// - 最后一段含 `.`（文件扩展名，如 `/favicon.svg`、`/foo.png`）：视为资源，
///   缺失 → 404。前端路由段（`/storage`、`/vms`）不含 `.`，故不受影响。
fn is_static_asset_path(path: &str) -> bool {
    if path.starts_with("/assets/") || path.starts_with("/static/") {
        return true;
    }
    // 仅看最后一段（文件名），避免把含点的前缀目录误判
    match path.rsplit('/').next() {
        Some(seg) => seg.contains('.'),
        None => false,
    }
}

/// 把字节流 + MIME 包成 axum `Response`（200 + Content-Type）。
fn serve_bytes(data: Vec<u8>, mime: &'static str) -> Response {
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, mime)],
        axum::body::Body::from(data),
    )
        .into_response()
}

/// 统一 404 响应（纯文本，保持与 axum 默认行为一致）。
fn not_found() -> Response {
    (axum::http::StatusCode::NOT_FOUND, "not found").into_response()
}

/// 把 axum `Method` 包成 `MethodRouter` 并绑定到 `dispatch_handler`。
fn method_router_for(method: Method) -> axum::routing::MethodRouter<GatewayState> {
    match method {
        Method::GET => axum::routing::get(dispatch_handler),
        Method::POST => axum::routing::post(dispatch_handler),
        Method::PUT => axum::routing::put(dispatch_handler),
        Method::DELETE => axum::routing::delete(dispatch_handler),
        Method::PATCH => axum::routing::patch(dispatch_handler),
        _ => axum::routing::any(dispatch_handler),
    }
}

// ----------------------------------------------------------------------------
// 公共辅助：构造默认 GatewayState（无 JWT / 无 WS）
// ----------------------------------------------------------------------------

impl InProcessGateway {
    /// 构造本网关的 axum 共享状态（用于 [`build_router`]）。
    ///
    /// `jwt` 为 None 时不做 HTTP 入口 JWT 解析；auth 字段始终为 None，
    /// 由下游 `AuthMiddleware`/路由 `requires_auth` 决策。
    pub fn make_state(
        &self,
        jwt: Option<Arc<JwtIssuerImpl>>,
        admin_token: Option<Arc<String>>,
    ) -> GatewayState {
        GatewayState {
            gateway: Arc::new(self.clone()),
            jwt,
            admin_token,
            git_repos_root: None,
            api_gateway: self.api_gateway(),
            llm_external: self.llm_external(),
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测（无端口监听，用 tower::ServiceExt::oneshot 调用 Router）
// ----------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gateway::{Gateway, HttpMethod, RouteSpec};
    use async_trait::async_trait;
    use os_security::{JwtClaims, JwtIssuerImpl, TokenType};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::ServiceExt;

    struct StubHandler {
        routes: Vec<RouteSpec>,
        counter: Arc<AtomicU32>,
    }

    #[async_trait]
    impl crate::gateway::RouteHandler for StubHandler {
        async fn routes(&self) -> Vec<RouteSpec> {
            self.routes.clone()
        }
        async fn handle(
            &self,
            _req: crate::gateway::ApiRequest,
        ) -> Result<crate::gateway::ApiResponse, crate::ApiGatewayError> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(crate::gateway::ApiResponse {
                status: 200,
                body: serde_json::json!({"ok": true}),
                headers: serde_json::json!({}),
            })
        }
    }

    fn spec(m: HttpMethod, path: &str, comp: &str) -> RouteSpec {
        RouteSpec {
            method: m,
            path: path.to_string(),
            handler_component: comp.to_string(),
            requires_auth: false,
            required_roles: vec![],
        }
    }

    /// 构建一个最小网关 + router，便于复用。
    async fn setup_router(
        routes: Vec<RouteSpec>,
    ) -> (InProcessGateway, axum::Router, Arc<AtomicU32>) {
        let gw = InProcessGateway::new();
        let counter = Arc::new(AtomicU32::new(0));
        gw.register_component(
            "test",
            Box::new(StubHandler {
                routes,
                counter: counter.clone(),
            }),
        )
        .await
        .unwrap();
        let state = gw.make_state(None, None);
        let router = build_router(state, None);
        (gw, router, counter)
    }

    #[tokio::test]
    async fn build_router_serves_registered_route() {
        let (_gw, router, counter) =
            setup_router(vec![spec(HttpMethod::Get, "/api/v1/pools", "test")]).await;

        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/pools")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn build_router_unmatched_falls_back_to_spa() {
        // 未注册的非 API 路径（如 /nope，无扩展名）现在回退到 SPA index.html，
        // 而非 404。同时验证它不会命中 dispatch（counter 保持 0）。
        let (_gw, router, counter) =
            setup_router(vec![spec(HttpMethod::Get, "/api/v1/pools", "test")]).await;
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/nope")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "/nope 应回退到 index.html");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "未注册路径不应命中 dispatch"
        );
        let body = body_string(resp).await;
        assert!(body.contains("NexOS"), "/nope 应返回 index.html");
    }

    #[tokio::test]
    async fn build_router_param_route_dispatches() {
        let (_gw, router, counter) =
            setup_router(vec![spec(HttpMethod::Get, "/api/v1/pools/:id", "test")]).await;
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/pools/tank")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn build_router_method_mismatch_405() {
        // 已注册 GET，发 POST 应得 405（axum method router 行为）
        let (_gw, router, _counter) =
            setup_router(vec![spec(HttpMethod::Get, "/api/v1/pools", "test")]).await;
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/pools")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn build_router_decodes_json_body() {
        // 验证 body 透传：handler 应能读到 JSON
        let gw = InProcessGateway::new();
        struct EchoHandler;
        #[async_trait]
        impl crate::gateway::RouteHandler for EchoHandler {
            async fn routes(&self) -> Vec<RouteSpec> {
                vec![spec(HttpMethod::Post, "/echo", "echo")]
            }
            async fn handle(
                &self,
                req: crate::gateway::ApiRequest,
            ) -> Result<crate::gateway::ApiResponse, crate::ApiGatewayError> {
                Ok(crate::gateway::ApiResponse {
                    status: 200,
                    body: req.body,
                    headers: serde_json::json!({}),
                })
            }
        }
        gw.register_component("echo", Box::new(EchoHandler))
            .await
            .unwrap();
        let state = gw.make_state(None, None);
        let router = build_router(state, None);

        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/echo")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({"hi": 1})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["hi"], 1);
    }

    #[tokio::test]
    async fn jwt_principal_extracted_from_bearer() {
        // 真实 JWT 往返：issuer 签发 → HTTP 入口解析 → handler 见到 Principal
        let issuer = Arc::new(JwtIssuerImpl::new(b"unit-test-secret".to_vec()));
        let now = chrono::Utc::now().timestamp();
        let claims = JwtClaims {
            sub: UserId::new("alice"),
            roles: vec![os_security::Role::Admin],
            exp: now + 3600,
            iat: now,
            token_type: TokenType::Access,
            custom: serde_json::Value::Null,
        };
        let token = issuer.issue(claims).await.unwrap();

        let gw = InProcessGateway::new();
        let seen_user = Arc::new(std::sync::Mutex::new(None::<String>));
        struct AuthCapture {
            seen: Arc<std::sync::Mutex<Option<String>>>,
        }
        #[async_trait]
        impl crate::gateway::RouteHandler for AuthCapture {
            async fn routes(&self) -> Vec<RouteSpec> {
                vec![spec(HttpMethod::Get, "/whoami", "auth")]
            }
            async fn handle(
                &self,
                req: crate::gateway::ApiRequest,
            ) -> Result<crate::gateway::ApiResponse, crate::ApiGatewayError> {
                *self.seen.lock().unwrap() =
                    req.auth.as_ref().map(|p| p.user.id.as_str().to_string());
                Ok(crate::gateway::ApiResponse {
                    status: 200,
                    body: serde_json::json!({"authed": req.auth.is_some()}),
                    headers: serde_json::json!({}),
                })
            }
        }
        gw.register_component(
            "auth",
            Box::new(AuthCapture {
                seen: seen_user.clone(),
            }),
        )
        .await
        .unwrap();
        let state = gw.make_state(Some(issuer), None);
        let router = build_router(state, None);

        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/whoami")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            seen_user.lock().unwrap().as_deref(),
            Some("alice"),
            "JWT Bearer 应被解析为 Principal"
        );
    }

    #[tokio::test]
    async fn jwt_invalid_token_yields_anonymous() {
        // 无效 JWT → auth 为 None（不短路 401，留给路由 requires_auth 决策）
        let issuer = Arc::new(JwtIssuerImpl::new(b"unit-test-secret".to_vec()));
        let gw = InProcessGateway::new();
        gw.register_component(
            "t",
            Box::new(StubHandler {
                routes: vec![spec(HttpMethod::Get, "/x", "t")],
                counter: Arc::new(AtomicU32::new(0)),
            }),
        )
        .await
        .unwrap();
        let state = gw.make_state(Some(issuer), None);
        let router = build_router(state, None);
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/x")
                    .header("authorization", "Bearer not.a.jwt")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn to_axum_path_pattern_conversions() {
        assert_eq!(to_axum_path_pattern("/api/v1/pools"), "/api/v1/pools");
        assert_eq!(
            to_axum_path_pattern("/api/v1/pools/:id"),
            "/api/v1/pools/{id}"
        );
        assert_eq!(to_axum_path_pattern("/static/*"), "/static/{*wildcard}");
        assert_eq!(to_axum_path_pattern("/"), "/");
    }

    // ------------------------------------------------------------------
    // 网关 SSE 流式转发（gateway_openai_handler 特挂路由）
    // ------------------------------------------------------------------

    use crate::handlers::api_gateway::ApiGatewayRouteHandler;

    /// api_gateway 共享实例的测试包装（main.rs SharedApiGatewayHandler 同款：
    /// register_component 收 Box 独占，非流式回落 dispatch 与流式透传需同一实例）。
    struct SharedApiGateway(Arc<ApiGatewayRouteHandler>);

    #[async_trait]
    impl crate::gateway::RouteHandler for SharedApiGateway {
        async fn routes(&self) -> Vec<RouteSpec> {
            self.0.routes().await
        }
        async fn handle(
            &self,
            req: crate::gateway::ApiRequest,
        ) -> Result<crate::gateway::ApiResponse, crate::ApiGatewayError> {
            self.0.handle(req).await
        }
    }

    /// 测试 router 的固定 admin token（REST 建渠道/令牌用——显式凭据，不受
    /// NEXOS_AUTH_DEFAULT_ADMIN 与其他测试 env 改写的并行影响）。
    const GW_TEST_ADMIN_TOKEN: &str = "gw-test-admin-secret";

    /// 构建带 api_gateway 共享实例的 router（特挂流式路由 + 组件注册双通道）。
    async fn gateway_router(h: Arc<ApiGatewayRouteHandler>) -> axum::Router {
        let gw = InProcessGateway::new();
        gw.register_component("api_gateway", Box::new(SharedApiGateway(h.clone())))
            .await
            .unwrap();
        let state = GatewayState {
            gateway: Arc::new(gw),
            jwt: None,
            admin_token: Some(Arc::new(GW_TEST_ADMIN_TOKEN.to_string())),
            git_repos_root: None,
            api_gateway: Some(h),
            llm_external: None,
        };
        build_router(state, None)
    }

    /// 经 router 的 REST 端点建一条渠道（返回渠道 id）。admin 写接口带显式
    /// Bearer（不依赖测试期默认 admin 注入）。
    async fn seed_channel_via_rest(router: &axum::Router, base_url: &str, priority: u32) -> String {
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/gateway/channels")
                    .header("content-type", "application/json")
                    .header(
                        "authorization",
                        format!("Bearer {GW_TEST_ADMIN_TOKEN}"),
                    )
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "name": format!("ch-{}", base_url),
                            "provider": "openai",
                            "base_url": base_url,
                            "models": ["m-1"],
                            "priority": priority,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "建渠道应 201");
        let v: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// 经 router 的 REST 端点建一条令牌，返回完整 key（sk-os-）。
    async fn seed_token_via_rest(router: &axum::Router) -> String {
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/gateway/tokens")
                    .header("content-type", "application/json")
                    .header(
                        "authorization",
                        format!("Bearer {GW_TEST_ADMIN_TOKEN}"),
                    )
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({"name": "stream-test"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let v: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        v["key"].as_str().unwrap().to_string()
    }

    /// 构造流式 chat/completions 请求。
    fn stream_chat_request(key: &str, body: serde_json::Value) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/gateway/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// 起 mock 上游（真 TCP，llm.rs spawn_fake_v1_models_server 手法）：接受一个
    /// 连接交给 `script` 处理，返回监听端口。
    fn spawn_upstream<F>(script: F) -> u16
    where
        F: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            script(stream);
        });
        port
    }

    /// 读掉一条 HTTP 请求（尽力一次读；SSE 转发的 POST body 很小）。
    fn drain_request(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
    }

    /// 写 SSE 响应头（无 content-length：EOF 定界，逐块 flush 可见）。
    fn write_sse_head(stream: &mut std::net::TcpStream) {
        use std::io::Write;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
        let _ = stream.flush();
    }

    /// 写一段 SSE 块并 flush。
    fn write_sse_chunk(stream: &mut std::net::TcpStream, text: &str) {
        use std::io::Write;
        stream.write_all(text.as_bytes()).unwrap();
        let _ = stream.flush();
    }

    const SSE_CHUNK1: &str = "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n";
    const SSE_CHUNK2: &str = "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n";
    const SSE_USAGE_CHUNK: &str =
        "data: {\"id\":\"x\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4,\"total_tokens\":15}}\n\n";
    const SSE_DONE: &str = "data: [DONE]\n\n";

    #[tokio::test]
    async fn gateway_stream_forwards_chunks_incrementally_with_usage_billed() {
        use futures::StreamExt;
        use std::sync::mpsc;

        // mock 上游：吐首块后**压住后续块**等测试放行（证明网关逐块透传而非整包）。
        // 压 30s：远大于测试侧 10s 的首块窗口（并行测试高负载下留足余量），
        // 整包式实现必等不到放行 → 首块读取超时失败；真流式实现立即可读。
        let (tx, rx) = mpsc::channel::<()>();
        let port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK1);
            let _ = rx.recv_timeout(std::time::Duration::from_secs(30));
            write_sse_chunk(&mut s, SSE_CHUNK2);
            write_sse_chunk(&mut s, SSE_USAGE_CHUNK);
            write_sse_chunk(&mut s, SSE_DONE);
        });
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        let router = gateway_router(h.clone()).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{port}/v1"), 0).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [{"role":"user","content":"hi"}], "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/event-stream"
                .parse::<axum::http::HeaderValue>()
                .unwrap(),
            "content-type 应为 text/event-stream"
        );
        // 真流式判定：上游还压着后续块（30s 放行窗口），首块应 10s 内到达——
        // 整包式实现必须等上游放行后才能吐首字节，10s 内绝无可能
        let mut data_stream = resp.into_body().into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), data_stream.next())
            .await
            .expect("10s 内应收到首块（真流式；整包式实现必等到 30s 放行窗口）")
            .expect("流不应提前结束")
            .expect("首块读取应成功");
        let first_text = String::from_utf8_lossy(&first).to_string();
        assert_eq!(first_text, SSE_CHUNK1, "首块逐字节等于上游首块");
        // 放行后续块
        tx.send(()).unwrap();
        // 收齐余块（含 usage 与 DONE），顺序保持
        let rest = axum::body::to_bytes(
            axum::body::Body::from_stream(data_stream),
            usize::MAX,
        )
        .await
        .unwrap();
        let rest_text = String::from_utf8_lossy(&rest);
        let full = format!("{first_text}{rest_text}");
        let p1 = full.find("Hel").expect("chunk1");
        let p2 = full.find("lo").expect("chunk2");
        let p3 = full.find("usage").expect("usage 块");
        let p4 = full.find("[DONE]").expect("DONE");
        assert!(p1 < p2 && p2 < p3 && p3 < p4, "逐块顺序保持: {full:?}");
        // 计费诚实：SSE 末块 usage 解析进调用日志
        let logs = h.logs_snapshot();
        let log = logs.iter().find(|l| l.status == "success").expect("应有 success 日志");
        assert_eq!(log.prompt_tokens, 11);
        assert_eq!(log.completion_tokens, 4);
        assert_eq!(log.total_tokens, 15);
        assert!(log.error.is_none(), "usage 已上报，不应有备注: {log:?}");
    }

    #[tokio::test]
    async fn gateway_stream_usage_missing_records_zero_with_note() {
        // 上游不报 usage（客户端未带 stream_options.include_usage）→ 记 0 + 注明
        let port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK1);
            write_sse_chunk(&mut s, SSE_CHUNK2);
            write_sse_chunk(&mut s, SSE_DONE);
        });
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        let router = gateway_router(h.clone()).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{port}/v1"), 0).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [], "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!bytes.is_empty(), "应有透传内容");
        let logs = h.logs_snapshot();
        let log = logs.iter().find(|l| l.status == "success").expect("应有 success 日志");
        assert_eq!((log.prompt_tokens, log.completion_tokens, log.total_tokens), (0, 0, 0));
        assert_eq!(
            log.error.as_deref(),
            Some(crate::handlers::api_gateway::STREAM_USAGE_MISSING_NOTE),
            "未上报 usage 应注明，不编造: {log:?}"
        );
    }

    #[tokio::test]
    async fn gateway_stream_failover_before_first_byte_on_http_error() {
        // 首选渠道回 500（首字节前失败）→ 转移到次选渠道成功
        let bad_port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            use std::io::Write;
            let body = "{\"error\":\"boom\"}";
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
            let _ = s.flush();
        });
        let good_port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK1);
            write_sse_chunk(&mut s, SSE_USAGE_CHUNK);
            write_sse_chunk(&mut s, SSE_DONE);
        });
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        let router = gateway_router(h.clone()).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{bad_port}/v1"), 0).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{good_port}/v1"), 1).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [], "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "应经次选渠道成功: {resp:?}");
        assert!(resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .starts_with("text/event-stream"));
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Hel"), "内容来自好渠道: {text:?}");
        // 日志：失败渠道一条 + 成功渠道一条
        let logs = h.logs_snapshot();
        assert!(
            logs.iter().any(|l| l.status == "failed" && l.error.as_deref().unwrap_or("").contains("500")),
            "失败渠道应记 failed: {logs:?}"
        );
        assert!(logs.iter().any(|l| l.status == "success"), "好渠道应记 success: {logs:?}");
    }

    #[tokio::test]
    async fn gateway_stream_no_failover_after_first_byte() {
        // 首块已透传后上游断流（content-length 虚报提前断开）→ 不再切渠道：
        // 响应 200 已开始、断流收尾记 failed，备用渠道（真 TCP 但无人来连）零接触。
        let broken_port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            use std::io::Write;
            // 头声明 content-length: 500 但只写首块后断开 → reqwest 读到首块后报
            // incomplete body（首块前不报错：首块读取成功）
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 500\r\n\r\n",
            );
            let _ = s.flush();
            write_sse_chunk(&mut s, SSE_CHUNK1);
            // 直接 drop：连接中断
        });
        let fallback_port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK2);
        });
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        let router = gateway_router(h.clone()).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{broken_port}/v1"), 0).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{fallback_port}/v1"), 1).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [], "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "首块到手即 200（不再切换渠道）");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Hel"), "首块已透传: {text:?}");
        assert!(
            text.contains(": gateway: upstream stream aborted"),
            "断流应补 SSE 注释帧: {text:?}"
        );
        // 日志：主渠道 failed（中途断开），且**没有**成功日志（未碰备用渠道）
        let logs = h.logs_snapshot();
        assert!(
            logs.iter().any(|l| l.status == "failed" && l.error.as_deref().unwrap_or("").contains("上游流中途断开")),
            "应记中途断开: {logs:?}"
        );
        assert!(
            !logs.iter().any(|l| l.status == "success"),
            "首字节后不得转移渠道（备用渠道未被使用）: {logs:?}"
        );
    }

    #[tokio::test]
    async fn gateway_stream_unauthorized_returns_401_json() {
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        let router = gateway_router(h).await;
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/gateway/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "model": "m-1", "messages": [], "stream": true
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "无 Bearer 应 401");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"].is_string(), "错误形状与非流式一致: {v:?}");
    }

    #[tokio::test]
    async fn gateway_non_stream_request_via_special_route_unchanged() {
        // 非流式（无 stream 字段）：特挂路由回落 dispatch → 原整包转发路径零回归
        let body = "{\"id\":\"x\",\"object\":\"chat.completion\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}";
        let port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            use std::io::Write;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
            let _ = s.flush();
        });
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        let router = gateway_router(h.clone()).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{port}/v1"), 0).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [{"role":"user","content":"hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/json".parse::<axum::http::HeaderValue>().unwrap(),
            "非流式仍是 application/json（无 SSE 头）"
        );
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
        // 非流式 usage 从整包 JSON 解析（既有语义）
        let logs = h.logs_snapshot();
        let log = logs.iter().find(|l| l.status == "success").expect("success 日志");
        assert_eq!((log.prompt_tokens, log.completion_tokens, log.total_tokens), (3, 2, 5));
    }

    // ------------------------------------------------------------------
    // 中继渠道流式转发（via_node 非空 → relay stream:true 分块回传，2026-09-03）
    // ------------------------------------------------------------------

    /// fake 互连 overlay（api_market 测试同款手法）：消费者（注入 handler
    /// set_relay）↔ 源端（白名单=base）。返回（消费者端点, 源端 NodeID hex）。
    fn gw_sse_relay_pair(
        base: &str,
    ) -> (
        crate::handlers::api_market::ApiMarketFedEndpoint,
        String,
    ) {
        let consumer = crate::handlers::api_market::ApiMarketFedEndpoint::test_endpoint();
        let source = crate::handlers::api_market::ApiMarketFedEndpoint::test_endpoint_with_local_listing(base);
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let a_hex = a_id.to_hex();
        let b_hex = b_id.to_hex();
        let b2 = source.clone();
        let b_target = b_id.clone();
        let a_from = a_id.clone();
        consumer.set_full_transport(
            Arc::new(move |to, payload| {
                if *to == b_target {
                    b2.dispatch(&a_from, &payload);
                }
            }),
            Arc::new(|_| {}),
            a_hex,
            "sse-consumer".into(),
        );
        let a3 = consumer.clone();
        let a_target = a_id.clone();
        let b_from = b_id.clone();
        source.set_full_transport(
            Arc::new(move |to, payload| {
                if *to == a_target {
                    a3.dispatch(&b_from, &payload);
                }
            }),
            Arc::new(|_| {}),
            b_hex.clone(),
            "sse-source".into(),
        );
        (consumer, b_hex)
    }

    /// 经 REST 建一条中继渠道（via_node 非空）。返回渠道 id。
    async fn seed_relay_channel_via_rest(
        router: &axum::Router,
        base_url: &str,
        via_node: &str,
        priority: u32,
    ) -> String {
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/gateway/channels")
                    .header("content-type", "application/json")
                    .header(
                        "authorization",
                        format!("Bearer {GW_TEST_ADMIN_TOKEN}"),
                    )
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "name": "relay-ch",
                            "provider": "openai",
                            "base_url": base_url,
                            "models": ["m-1"],
                            "priority": priority,
                            "via_node": via_node,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "建中继渠道应 201");
        let v: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// GW-SSE-RL1. 中继渠道流式分块透传：客户端 stream:true → 网关 → relay
    /// stream:true 分块回传（源端 SSE 逐块透传成 resp 帧）→ 网关逐块写客户端；
    /// usage 从透传文本解析计费（本地鉴权/计费照常）。
    #[tokio::test]
    async fn gateway_stream_relay_channel_chunk_passthrough_with_usage_billed() {
        let port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK1);
            write_sse_chunk(&mut s, SSE_CHUNK2);
            write_sse_chunk(&mut s, SSE_USAGE_CHUNK);
            write_sse_chunk(&mut s, SSE_DONE);
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let (consumer, source_hex) = gw_sse_relay_pair(&base);
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        h.set_relay(Some(consumer));
        let router = gateway_router(h.clone()).await;
        seed_relay_channel_via_rest(&router, &base, &source_hex, 0).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [], "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "中继流式应 200: {resp:?}");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/event-stream"
                .parse::<axum::http::HeaderValue>()
                .unwrap(),
            "content-type 取自源端透传的响应头"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        // 分块顺序保持（Hel → lo → usage → [DONE]）
        let p1 = text.find("Hel").expect("chunk1");
        let p2 = text.find("lo").expect("chunk2");
        let p3 = text.find("usage").expect("usage 块");
        let p4 = text.find("[DONE]").expect("DONE");
        assert!(p1 < p2 && p2 < p3 && p3 < p4, "逐块顺序保持: {text:?}");
        // 计费照常：usage 从透传尾部窗口解析
        let logs = h.logs_snapshot();
        let log = logs.iter().find(|l| l.status == "success").expect("success 日志");
        assert_eq!((log.prompt_tokens, log.completion_tokens, log.total_tokens), (11, 4, 15));
        // 配额扣减（本地计费不受中继影响）
        let tokens = h.tokens_snapshot();
        assert_eq!(tokens[0].quota_used, 15, "per_token 扣 15");
    }

    /// GW-SSE-RL2. 中继渠道首字节前失败（通道未装配）→ 故障转移到直连渠道：
    /// 流式路径与非流式同边界（首字节前可切，切完照常透传）。
    #[tokio::test]
    async fn gateway_stream_relay_failure_fails_over_to_direct() {
        let good_port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK1);
            write_sse_chunk(&mut s, SSE_USAGE_CHUNK);
            write_sse_chunk(&mut s, SSE_DONE);
        });
        let h = Arc::new(ApiGatewayRouteHandler::with_empty());
        // 不 set_relay：中继渠道立即失败（通道未装配），转移直连渠道
        let router = gateway_router(h.clone()).await;
        let ghost_hex = os_p2p::NodeIdentity::generate().node_id().to_hex();
        seed_relay_channel_via_rest(&router, "http://10.0.0.9:8000/v1", &ghost_hex, 0).await;
        seed_channel_via_rest(&router, &format!("http://127.0.0.1:{good_port}/v1"), 1).await;
        let key = seed_token_via_rest(&router).await;

        let resp = router
            .oneshot(stream_chat_request(
                &key,
                serde_json::json!({"model": "m-1", "messages": [], "stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "应转移到直连渠道: {resp:?}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Hel"), "内容来自直连渠道: {text:?}");
        let logs = h.logs_snapshot();
        assert!(
            logs.iter().any(|l| l.status == "failed"
                && l.error.as_deref().unwrap_or("").contains("中继失败")),
            "中继渠道失败文案可区分: {logs:?}"
        );
        assert!(logs.iter().any(|l| l.status == "success"));
    }

    // ------------------------------------------------------------------
    // LLM 外部 API 对话直通（llm_external::chat_stream_handler 特挂路由）
    // ------------------------------------------------------------------

    use crate::handlers::llm::LlmRouteHandler;

    /// llm 组件共享实例的测试包装（main.rs SharedLlmHandler 同款：
    /// register_component 收 Box 独占，特挂流式路由需与组件同一外部 API 态）。
    struct SharedLlm(Arc<LlmRouteHandler>);

    #[async_trait]
    impl crate::gateway::RouteHandler for SharedLlm {
        async fn routes(&self) -> Vec<RouteSpec> {
            self.0.routes().await
        }
        async fn handle(
            &self,
            req: crate::gateway::ApiRequest,
        ) -> Result<crate::gateway::ApiResponse, crate::ApiGatewayError> {
            self.0.handle(req).await
        }
    }

    /// 构建带 llm 共享实例的 router（SSE 特挂 + 组件注册双通道，同
    /// [`gateway_router`] 手法）。
    async fn llm_ext_router(h: Arc<LlmRouteHandler>) -> axum::Router {
        let gw = InProcessGateway::new();
        gw.register_component("llm", Box::new(SharedLlm(h.clone())))
            .await
            .unwrap();
        let state = GatewayState {
            gateway: Arc::new(gw),
            jwt: None,
            admin_token: Some(Arc::new(GW_TEST_ADMIN_TOKEN.to_string())),
            git_repos_root: None,
            api_gateway: None,
            llm_external: Some(h.external_state()),
        };
        build_router(state, None)
    }

    /// 经 router 的 REST 端点登记一条外部 API（返回 id；admin 写带显式 Bearer）。
    async fn seed_external_api_via_rest(router: &axum::Router, base_url: &str) -> String {
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/llm/external-apis")
                    .header("content-type", "application/json")
                    .header(
                        "authorization",
                        format!("Bearer {GW_TEST_ADMIN_TOKEN}"),
                    )
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "name": "106 网关",
                            "base_url": base_url,
                            "api_key": "sk-upstream-key",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "登记外部 API 应 201");
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// 构造外部 API 对话请求（POST /:id/chat）。
    fn llm_ext_chat_request(
        id: &str,
        auth: Option<&str>,
        body: serde_json::Value,
    ) -> axum::http::Request<axum::body::Body> {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/llm/external-apis/{id}/chat"))
            .header("content-type", "application/json");
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b.body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn llm_ext_stream_forwards_chunks_incrementally() {
        use futures::StreamExt;
        use std::sync::mpsc;

        // mock 上游：吐首块后压住后续块等测试放行（证明逐块透传而非整包），
        // 手法同 gateway_stream_forwards_chunks_incrementally。
        let (tx, rx) = mpsc::channel::<()>();
        let port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            write_sse_head(&mut s);
            write_sse_chunk(&mut s, SSE_CHUNK1);
            let _ = rx.recv_timeout(std::time::Duration::from_secs(30));
            write_sse_chunk(&mut s, SSE_CHUNK2);
            write_sse_chunk(&mut s, SSE_USAGE_CHUNK);
            write_sse_chunk(&mut s, SSE_DONE);
        });
        let h = Arc::new(LlmRouteHandler::with_empty());
        let router = llm_ext_router(h).await;
        let id = seed_external_api_via_rest(&router, &format!("http://127.0.0.1:{port}/v1")).await;

        let resp = router
            .clone()
            .oneshot(llm_ext_chat_request(
                &id,
                Some(&format!("Bearer {GW_TEST_ADMIN_TOKEN}")),
                serde_json::json!({
                    "model": "qwen3.5-9b",
                    "messages": [{"role":"user","content":"hi"}],
                    "stream": true
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/event-stream".parse::<axum::http::HeaderValue>().unwrap(),
            "content-type 应为 text/event-stream"
        );
        // 真流式判定：上游压着后续块（30s），首块应 10s 内逐字节到达
        let mut data_stream = resp.into_body().into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), data_stream.next())
            .await
            .expect("10s 内应收到首块（真流式）")
            .expect("流不应提前结束")
            .expect("首块读取应成功");
        assert_eq!(
            String::from_utf8_lossy(&first),
            SSE_CHUNK1,
            "首块逐字节等于上游首块"
        );
        tx.send(()).unwrap();
        // 收齐余块：usage 与 [DONE] 原样透传，顺序保持
        let rest = axum::body::to_bytes(
            axum::body::Body::from_stream(data_stream),
            usize::MAX,
        )
        .await
        .unwrap();
        let full = format!("{SSE_CHUNK1}{}", String::from_utf8_lossy(&rest));
        let p1 = full.find("Hel").expect("chunk1");
        let p2 = full.find("lo").expect("chunk2");
        let p3 = full.find("total_tokens\":15").expect("usage 块透传");
        let p4 = full.find("[DONE]").expect("DONE");
        assert!(p1 < p2 && p2 < p3 && p3 < p4, "逐块顺序保持: {full:?}");
    }

    #[tokio::test]
    async fn llm_ext_stream_auth_and_missing_row() {
        let h = Arc::new(LlmRouteHandler::with_empty());
        let router = llm_ext_router(h).await;
        let body = serde_json::json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}], "stream": true
        });
        // 错误 Bearer → 401（特挂路由自带鉴权，与 dispatch 同口径）
        let resp = router
            .clone()
            .oneshot(llm_ext_chat_request("xapi-1", Some("Bearer wrong-token"), body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "错误凭据应 401");
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert!(v["error"].is_string(), "错误形状与非流式一致: {v:?}");
        // 正确 admin 凭据 + 不存在的行 → 404
        let resp = router
            .oneshot(llm_ext_chat_request(
                "xapi-999",
                Some(&format!("Bearer {GW_TEST_ADMIN_TOKEN}")),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn llm_ext_chat_nonstream_via_special_route_falls_back_to_dispatch() {
        // 非流式（无 stream 字段）：特挂路由回落 dispatch → 组件整包转发零回归
        let body = "{\"id\":\"x\",\"object\":\"chat.completion\",\"model\":\"qwen3.5-9b\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}";
        let port = spawn_upstream(move |mut s| {
            drain_request(&mut s);
            use std::io::Write;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
            let _ = s.flush();
        });
        let h = Arc::new(LlmRouteHandler::with_empty());
        let router = llm_ext_router(h).await;
        let id = seed_external_api_via_rest(&router, &format!("http://127.0.0.1:{port}/v1")).await;

        let resp = router
            .oneshot(llm_ext_chat_request(
                &id,
                Some(&format!("Bearer {GW_TEST_ADMIN_TOKEN}")),
                serde_json::json!({
                    "model": "qwen3.5-9b",
                    "messages": [{"role":"user","content":"hi"}]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/json".parse::<axum::http::HeaderValue>().unwrap(),
            "非流式仍是 application/json（无 SSE 头）"
        );
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
        assert_eq!(v["usage"]["total_tokens"], 5, "usage 原样透传不估算");
    }

    #[tokio::test]
    async fn method_roundtrip() {
        for m in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
            HttpMethod::Patch,
        ] {
            let ax = method_to_axum(m);
            assert_eq!(method_from_axum(&ax), Some(m));
        }
        assert_eq!(method_from_axum(&Method::OPTIONS), None);
    }

    // —— 覆盖率补测：HTTP 方法/状态码/请求响应模型 serde 边界 ——

    #[tokio::test]
    async fn build_router_rejects_unsupported_method_with_405() {
        // axum 收到未注册的 OPTIONS（method_from_axum 返回 None） → 经 dispatch_handler
        // 不过 axum 自身会拦截不支持的 method；这里测 axum MethodRouter fallback
        let (_gw, router, _counter) =
            setup_router(vec![spec(HttpMethod::Get, "/api/v1/pools", "test")]).await;
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("OPTIONS")
                    .uri("/api/v1/pools")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum 对未注册 method 返回 405（Method Not Allowed）
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn decode_body_handles_non_utf8_falls_back_to_null() {
        // 非法 UTF-8 字节且非合法 JSON → 回退为 null（不破坏分发）
        let bad = axum::body::Body::from(vec![0xff, 0xfe, 0xfd]);
        let bytes = axum::body::to_bytes(bad, usize::MAX).await.unwrap();
        let v = decode_body(&bytes);
        assert!(v.is_null(), "非法 UTF-8 应回退为 null");
    }

    #[tokio::test]
    async fn decode_body_string_fallback_for_non_json() {
        // 合法 UTF-8 但非 JSON → 回退为字符串
        let bytes = axum::body::to_bytes(axum::body::Body::from("plain text body"), usize::MAX)
            .await
            .unwrap();
        let v = decode_body(&bytes);
        assert_eq!(v.as_str().unwrap(), "plain text body");
    }

    #[tokio::test]
    async fn decode_body_empty_is_null() {
        let bytes: axum::body::Bytes = axum::body::Bytes::new();
        let v = decode_body(&bytes);
        assert!(v.is_null());
    }

    #[tokio::test]
    async fn api_to_response_includes_headers_and_content_type() {
        // 响应带 headers（对象）→ 透传到 axum Response
        let resp = crate::gateway::ApiResponse {
            status: 201,
            body: serde_json::json!({"ok": 1}),
            headers: serde_json::json!({"x-custom": "v"}),
        };
        let response = api_to_response(resp);
        assert_eq!(response.status(), 201);
        assert_eq!(
            response.headers().get("x-custom").unwrap(),
            &"v".parse::<axum::http::HeaderValue>().unwrap()
        );
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            &"application/json"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn api_to_response_ignores_non_object_headers() {
        // headers 非 Object（如 Null）→ 不 panic，正常返回
        let resp = crate::gateway::ApiResponse {
            status: 200,
            body: serde_json::Value::Null,
            headers: serde_json::Value::Null,
        };
        let response = api_to_response(resp);
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn api_to_response_raw_text_passthrough_unquoted() {
        // text/* content-type + String body → 原文直传（curl|bash 契约，不带 JSON 引号）
        let script = "#!/usr/bin/env bash\necho hi\n";
        let resp = crate::gateway::ApiResponse {
            status: 200,
            body: serde_json::Value::String(script.into()),
            headers: serde_json::json!({"content-type": "text/x-shellscript; charset=utf-8"}),
        };
        let response = api_to_response(resp);
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            &"text/x-shellscript; charset=utf-8"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            bytes.as_ref(),
            script.as_bytes(),
            "必须逐字节等于原脚本文本"
        );
    }

    #[tokio::test]
    async fn api_to_response_octet_stream_base64_passthrough_decodes() {
        // application/octet-stream + String body（标准 base64）→ 解码后的原始字节直传
        use base64::Engine as _;
        let payload: &[u8] = b"\x7fELF\x02\x01\x01\x00binary";
        let resp = crate::gateway::ApiResponse {
            status: 200,
            body: serde_json::Value::String(
                base64::engine::general_purpose::STANDARD.encode(payload),
            ),
            headers: serde_json::json!({
                "content-type": "application/octet-stream",
                "x-nexos-sha256": "deadbeef",
            }),
        };
        let response = api_to_response(resp);
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), payload, "应解码 base64 返回原始二进制");
    }

    #[tokio::test]
    async fn api_to_response_object_body_never_passthrough() {
        // 对象 body 即使声明 octet-stream 也走 JSON（守卫：既有响应零变化）
        let resp = crate::gateway::ApiResponse {
            status: 200,
            body: serde_json::json!({"ok": true}),
            headers: serde_json::json!({"content-type": "application/octet-stream"}),
        };
        let response = api_to_response(resp);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(std::str::from_utf8(bytes.as_ref())
            .unwrap()
            .contains("\"ok\":true"));
    }

    #[tokio::test]
    async fn api_to_response_app_asset_text_mimes_passthrough() {
        // 2026-09-04 应用静态资源（/apps-assets/*）：svg 与 json 虽非 text/*
        // 前缀，同为文本形——String body 原文直传（fetch().json() / <img> 需要
        // 原文，JSON 引号包裹会破坏解析）
        for (mime, body) in [
            ("image/svg+xml", "<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            ("application/json", "{\"i18n\":{\"zh\":{}}}"),
        ] {
            let resp = crate::gateway::ApiResponse {
                status: 200,
                body: serde_json::Value::String(body.into()),
                headers: serde_json::json!({ "content-type": mime }),
            };
            let response = api_to_response(resp);
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                &mime.parse::<axum::http::HeaderValue>().unwrap()
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                bytes.as_ref(),
                body.as_bytes(),
                "{mime} 应原文直传（不带 JSON 引号）"
            );
        }
    }

    #[tokio::test]
    async fn api_to_response_app_asset_binary_mimes_base64_decode() {
        // png / woff2 二进制白名单：base64 body 解码回原始字节（Content-Type 保留）
        use base64::Engine as _;
        for (mime, payload) in [
            ("image/png", b"\x89PNG\r\n\x1a\nfake".as_slice()),
            ("font/woff2", b"wOF2\x00\x01font".as_slice()),
        ] {
            let resp = crate::gateway::ApiResponse {
                status: 200,
                body: serde_json::Value::String(
                    base64::engine::general_purpose::STANDARD.encode(payload),
                ),
                headers: serde_json::json!({ "content-type": mime }),
            };
            let response = api_to_response(resp);
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                &mime.parse::<axum::http::HeaderValue>().unwrap()
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), payload, "{mime} 应解码回原始字节");
        }
    }

    #[tokio::test]
    async fn build_router_serves_delete_put_patch() {
        // 覆盖 method_router_for 的 DELETE/PUT/PATCH 分支
        let gw = InProcessGateway::new();
        let counter = Arc::new(AtomicU32::new(0));
        gw.register_component(
            "all",
            Box::new(StubHandler {
                routes: vec![
                    spec(HttpMethod::Delete, "/api/v1/x", "all"),
                    spec(HttpMethod::Put, "/api/v1/y", "all"),
                    spec(HttpMethod::Patch, "/api/v1/z", "all"),
                ],
                counter: counter.clone(),
            }),
        )
        .await
        .unwrap();
        let state = gw.make_state(None, None);
        let router = build_router(state, None);

        for (method, path) in [
            ("DELETE", "/api/v1/x"),
            ("PUT", "/api/v1/y"),
            ("PATCH", "/api/v1/z"),
        ] {
            let resp = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{method} {path} 应命中");
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn build_router_includes_ws_endpoint() {
        // ws_path 被注册时，/ws 任意方法都路由到 ws_handler（不升级则 426/400，但应可达）
        let gw = InProcessGateway::new();
        let state = gw.make_state(None, None);
        let _router = build_router(state, Some("/ws"));
        // 不做真实 WS 握手（已在 ws_impl::real_ws_endpoint_pushes_messages 覆盖），
        // 这里仅验证 build_router 不 panic 且能构造 Router。
    }

    // ============ 终端 WS（/ws/terminal/*）：鉴权 + 会话校验 + 真实往返 ============
    //
    // 注：oneshot 探测过不了 axum WebSocketUpgrade 提取器（无真实可升级连接，
    // 固定 426），故鉴权拒绝路径经真实 serve + tungstenite 握手断言 HTTP 状态。

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// 起一个真实网关（build_router 始终挂载 /ws/terminal/{id}），返回端口。
    async fn serve_gateway_with_admin_token(token: Option<&str>) -> u16 {
        let gw = InProcessGateway::new();
        gw.set_admin_token(token.map(|t| Arc::new(t.to_string())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .expect("start");
        addr.port()
    }

    /// 对 url 发起 WS 握手，返回服务端 HTTP 状态（101 成功；非升级拒绝路径
    /// 由 tungstenite 以 Error::Http(resp) 携带状态码）。
    async fn ws_handshake_status(url: &str) -> u16 {
        let req = url.to_string().into_client_request().unwrap();
        match tokio_tungstenite::connect_async(req).await {
            Ok((_stream, resp)) => resp.status().as_u16(),
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Err(other) => panic!("非预期握手错误: {other:?}"),
        }
    }

    // —— 终端 WS 鉴权：测试期豁免（NEXOS_AUTH_DEFAULT_ADMIN 默认开）与关闭态双路 ——
    /// env 竞态锁：改 NEXOS_AUTH_DEFAULT_ADMIN 的测试（env 进程级全局可见，
    /// 并行测试互染——2026-08-31 发现该对并行跑时随机互踩导致偶发红）都须持
    /// 本锁覆盖"改 env → 断言 → 还原"全程。2026-09-02 起 pub(crate)：跨模块
    /// 参与者（api_market 的脱敏 admin 注入矩阵测试）复用同一把锁串行。
    pub(crate) static TERMINAL_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // 豁免开启（默认）：无 token 也放行（HTTP 101/升级成功——用会话不存在 404 探测，
    // 说明鉴权已过、进入了会话查找阶段）
    #[tokio::test]
    async fn terminal_ws_default_admin_allows_empty_token() {
        let _env_guard = TERMINAL_ENV_LOCK.lock().await;
        std::env::remove_var("NEXOS_AUTH_DEFAULT_ADMIN");
        let port = serve_gateway_with_admin_token(Some("admin-secret")).await;
        // term-1 不存在：若鉴权未过应为 401；拿到 404 说明已进入会话查找=放行
        assert_eq!(
            ws_handshake_status(&format!("ws://127.0.0.1:{port}/ws/terminal/no-such")).await,
            404,
            "测试期豁免下空 token 应通过鉴权"
        );
    }

    // 关闭豁免（=0）：恢复默认拒绝语义
    #[tokio::test(flavor = "current_thread")]
    async fn terminal_ws_disabled_exempt_still_rejects() {
        let _env_guard = TERMINAL_ENV_LOCK.lock().await;
        // 单测试内顺序执行三个"豁免关闭"场景（避免并行 set/remove env 互染）
        std::env::set_var("NEXOS_AUTH_DEFAULT_ADMIN", "0");

        let port = serve_gateway_with_admin_token(Some("admin-secret")).await;
        assert_eq!(
            ws_handshake_status(&format!("ws://127.0.0.1:{port}/ws/terminal/term-1")).await,
            401,
            "豁免关闭后无 token 必须在升级前被拒"
        );

        let port = serve_gateway_with_admin_token(Some("admin-secret")).await;
        assert_eq!(
            ws_handshake_status(&format!(
                "ws://127.0.0.1:{port}/ws/terminal/term-1?token=wrong"
            ))
            .await,
            401,
            "错 token 必须被拒（豁免只针对无头调用）"
        );

        let port = serve_gateway_with_admin_token(None).await;
        assert_eq!(
            ws_handshake_status(&format!(
                "ws://127.0.0.1:{port}/ws/terminal/term-1?token=whatever"
            ))
            .await,
            401,
            "未配置 NEXOS_ADMIN_TOKEN 且豁免关闭时一律拒绝"
        );

        std::env::remove_var("NEXOS_AUTH_DEFAULT_ADMIN");
    }

    // —— 鉴权通过但会话不存在 → 404（升级前拒绝，客户端拿到 HTTP 状态） ——
    #[tokio::test]
    async fn terminal_ws_unknown_session_404_after_auth() {
        let port = serve_gateway_with_admin_token(Some("admin-secret")).await;
        assert_eq!(
            ws_handshake_status(&format!(
                "ws://127.0.0.1:{port}/ws/terminal/ghost-session?token=admin-secret"
            ))
            .await,
            404
        );
    }

    // —— 真实 e2e：admin token 握手 → input 帧 → PTY bash → output 帧往返 ——
    #[tokio::test]
    async fn terminal_ws_real_roundtrip_input_output() {
        use crate::handlers::terminal::{ClientFrame, ServerFrame, TerminalSessions};
        use base64::Engine as _;
        use futures::{SinkExt, StreamExt};

        let port = serve_gateway_with_admin_token(Some("e2e-admin-token")).await;

        // 会话建在共享注册表（WS 升级层消费同一实例）
        let sessions = TerminalSessions::shared();
        let info = sessions.spawn_local(80, 24).expect("spawn local bash");

        // 鉴权通过 → 101 升级
        let url = format!(
            "ws://127.0.0.1:{port}/ws/terminal/{}?token=e2e-admin-token",
            info.session_id
        );
        let req = url.clone().into_client_request().unwrap();
        let (mut ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("WS 握手（admin token）");
        assert_eq!(resp.status().as_u16(), 101);

        // input 帧：echo os$((6*7))ws —— 执行结果 os42ws 与键入回显可区分
        let input = serde_json::to_string(&ClientFrame::Input {
            data: base64::engine::general_purpose::STANDARD.encode(b"echo os$((6*7))ws\n"),
        })
        .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(input.into()))
            .await
            .unwrap();

        // 读帧直到 output 含 os42ws（或 exit/error 提前失败）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut acc = String::new();
        let mut early_exit = false;
        while std::time::Instant::now() < deadline {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await;
            match frame {
                Ok(Some(Ok(msg))) => {
                    let text = msg.into_text().expect("应为文本帧");
                    let Ok(f) = serde_json::from_str::<ServerFrame>(&text) else {
                        continue;
                    };
                    match f {
                        ServerFrame::Output { data } => {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(&data)
                                .unwrap();
                            acc.push_str(&String::from_utf8_lossy(&bytes));
                            if acc.contains("os42ws") {
                                break;
                            }
                        }
                        ServerFrame::Exit { .. } => {
                            early_exit = true;
                            break;
                        }
                        ServerFrame::Error { msg } => panic!("不应出现 error 帧: {msg}"),
                    }
                }
                _ => break,
            }
        }
        // 清理会话（防 bash 泄漏；kill 幂等）
        sessions.kill_session(&info.session_id);
        assert!(!early_exit, "bash 不应提前退出");
        assert!(
            acc.contains("os42ws"),
            "WS 往返应收到真实执行结果 os42ws: {acc:?}"
        );
        // serve task 由进程退出回收（ws_impl::real_ws_endpoint 同款约定）
    }

    #[tokio::test]
    async fn extract_principal_returns_none_without_jwt_issuer() {
        // jwt=None 且 admin_token=None → 直接返回 None（不解析）
        let headers = serde_json::json!({"authorization": "Bearer x"});
        let p = extract_principal(&headers, None, None).await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn extract_principal_returns_none_without_auth_header() {
        // 测试期默认 admin（NEXOS_AUTH_DEFAULT_ADMIN，默认开）：无头注入 admin
        let issuer = Arc::new(JwtIssuerImpl::new(b"k".to_vec()));
        let headers = serde_json::json!({});
        let p = extract_principal(&headers, Some(&issuer), None).await;
        assert!(p.is_some(), "无头应注入测试期默认 admin");
        // 显式关闭（=0）→ 恢复旧行为 None
        std::env::set_var("NEXOS_AUTH_DEFAULT_ADMIN", "0");
        let p = extract_principal(&headers, Some(&issuer), None).await;
        assert!(p.is_none(), "关闭默认 admin 后无头应为 None");
        std::env::remove_var("NEXOS_AUTH_DEFAULT_ADMIN");
    }

    #[tokio::test]
    async fn extract_principal_returns_none_with_bad_prefix() {
        let issuer = Arc::new(JwtIssuerImpl::new(b"k".to_vec()));
        // 非 "Bearer " / "bearer " 前缀 → None
        let headers = serde_json::json!({"authorization": "Token abc"});
        let p = extract_principal(&headers, Some(&issuer), None).await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn extract_principal_admin_token_match_yields_admin_principal() {
        // 漏洞2：OS_ADMIN_TOKEN 精确匹配 → 注入 admin Principal（无 JWT 也能鉴权）
        let admin_token = Arc::new("change-me-admin-token".to_string());
        let headers = serde_json::json!({"authorization": "Bearer change-me-admin-token"});
        let p = extract_principal(&headers, None, Some(&admin_token))
            .await
            .expect("admin_token 匹配应返回 Principal");
        assert_eq!(p.user.id.as_str(), "admin");
        assert!(p.roles.iter().any(|r| matches!(r, Role::Admin)));
    }

    #[tokio::test]
    async fn extract_principal_admin_token_mismatch_falls_through_to_none() {
        // admin_token 不匹配 + 无 JWT → None（不误授权）
        let admin_token = Arc::new("correct-secret".to_string());
        let headers = serde_json::json!({"authorization": "Bearer wrong-secret"});
        let p = extract_principal(&headers, None, Some(&admin_token)).await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn make_state_carries_jwt() {
        // make_state(Some) 把 issuer 注入 GatewayState
        let gw = InProcessGateway::new();
        let issuer = Arc::new(JwtIssuerImpl::new(b"k".to_vec()));
        let state = gw.make_state(Some(issuer), None);
        assert!(state.jwt.is_some());
        let state2 = gw.make_state(None, None);
        assert!(state2.jwt.is_none());
    }

    // —— Web UI 内嵌静态资源 fallback（rust-embed） ——

    /// 辅助：构建一个最小 router（无注册路由，仅触发 fallback）。
    fn empty_router() -> axum::Router {
        let gw = InProcessGateway::new();
        let state = gw.make_state(None, None);
        build_router(state, None)
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn root_serves_embedded_index_html() {
        let router = empty_router();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/html; charset=utf-8"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );
        let body = body_string(resp).await;
        assert!(body.contains("NexOS"), "/ 应返回 index.html 含标题");
    }

    #[tokio::test]
    async fn static_css_served_with_correct_mime() {
        let router = empty_router();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/static/css/style.css")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/css; charset=utf-8"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn static_js_served_with_correct_mime() {
        let router = empty_router();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/static/js/app.js")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/javascript; charset=utf-8"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn static_unknown_asset_returns_404() {
        // /static/ 下不存在的资源 → 404（不被 fallback 静默吞掉）
        let router = empty_router();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/static/nope.xyz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn fallback_rejects_non_get_with_405() {
        // 对 / 发 POST（无显式路由匹配）→ fallback 返回 405
        let router = empty_router();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn api_routes_not_intercepted_by_static_fallback() {
        // 关键红线：注册的 API 路由（/api/v1/pools）必须优先匹配 dispatch_handler，
        // 不被静态 fallback 拦截。
        let (_gw, router, counter) =
            setup_router(vec![spec(HttpMethod::Get, "/api/v1/pools", "test")]).await;
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/pools")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(counter.load(Ordering::SeqCst), 1, "API 路由应命中 dispatch");
        let body = body_string(resp).await;
        assert!(body.contains("true"), "API 应返回 JSON 业务响应");
    }

    #[tokio::test]
    async fn spa_route_falls_back_to_index_html() {
        // 关键修复：前端路由（如 /storage /vms）不是静态资源，后端应返回
        // index.html 交由 Vue Router 处理（SPA fallback），而非 404。
        for uri in [
            "/storage",
            "/vms",
            "/shares",
            "/users",
            "/nodes",
            "/settings",
        ] {
            let router = empty_router();
            let resp = router
                .oneshot(
                    axum::http::Request::builder()
                        .method("GET")
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{uri} 应回退到 index.html（200）");
            assert_eq!(
                resp.headers().get("content-type").unwrap(),
                &"text/html; charset=utf-8"
                    .parse::<axum::http::HeaderValue>()
                    .unwrap(),
                "{uri} Content-Type 应为 text/html"
            );
            let body = body_string(resp).await;
            assert!(body.contains("NexOS"), "{uri} 应返回 index.html");
        }
    }

    #[tokio::test]
    async fn api_like_unmatched_path_returns_404() {
        // /api/v1/unknown 形如 API 但未注册 → 404（不被 SPA fallback 降级为 HTML，
        // 否则前端拿到 HTML 难以排查 API 调用错误）
        let router = empty_router();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/does-not-exist")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[test]
    fn is_static_asset_path_classification() {
        // 资源目录前缀 → 视为资源
        assert!(is_static_asset_path("/assets/app-abc.js"));
        assert!(is_static_asset_path("/static/css/style.css"));
        assert!(is_static_asset_path("/assets/")); // 资源目录前缀即使无文件名也判资源
                                                   // 含扩展名的文件名 → 视为资源
        assert!(is_static_asset_path("/favicon.svg"));
        assert!(is_static_asset_path("/foo.png"));
        // 前端路由段（无扩展名、非资源目录）→ 不是资源（应 SPA fallback）
        assert!(!is_static_asset_path("/storage"));
        assert!(!is_static_asset_path("/vms"));
        assert!(!is_static_asset_path("/"));
        assert!(!is_static_asset_path("/shares/media"));
    }

    #[test]
    fn is_api_path_classification() {
        assert!(is_api_path("/api/"));
        assert!(is_api_path("/api/v1/pools"));
        assert!(is_api_path("/api"));
        // 非 API 前缀
        assert!(!is_api_path("/storage"));
        assert!(!is_api_path("/status"));
        assert!(!is_api_path("/api2")); // 相似但不同前缀
    }

    // —— Git Smart HTTP（/git/* → git-http-backend CGI）——

    /// 构建带 git 配置的 router（admin_token 必设；repos_root 可注入临时目录）。
    fn git_router(admin_token: Option<&str>, repos_root: Option<String>) -> axum::Router {
        let gw = InProcessGateway::new();
        let state = GatewayState {
            gateway: Arc::new(gw),
            jwt: None,
            admin_token: admin_token.map(|t| Arc::new(t.to_string())),
            git_repos_root: repos_root,
            api_gateway: None,
            llm_external: None,
        };
        build_router(state, None)
    }

    fn git_get(uri: &str, auth: Option<(&str, &str)>) -> axum::http::Request<axum::body::Body> {
        let mut b = axum::http::Request::builder().method("GET").uri(uri);
        if let Some((k, v)) = auth {
            b = b.header(k, v);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn git_parse_path_extracts_repo_and_endpoint() {
        // Smart HTTP 三个端点
        assert_eq!(
            parse_git_path("/git/nexos.git/info/refs").unwrap(),
            "nexos.git/info/refs"
        );
        assert_eq!(
            parse_git_path("/git/nexos.git/git-upload-pack").unwrap(),
            "nexos.git/git-upload-pack"
        );
        assert_eq!(
            parse_git_path("/git/nexos.git/git-receive-pack").unwrap(),
            "nexos.git/git-receive-pack"
        );
        // 无 .git 后缀自动补齐（clone URL 可省略 .git）
        assert_eq!(
            parse_git_path("/git/nexos/info/refs").unwrap(),
            "nexos.git/info/refs"
        );
    }

    #[test]
    fn git_parse_path_rejects_traversal_and_bad_endpoints() {
        // .. 穿越（明文、percent 编码、编码斜杠）
        assert!(parse_git_path("/git/../etc/passwd").is_err());
        assert!(parse_git_path("/git/a/../../x/info/refs").is_err());
        assert!(parse_git_path("/git/%2e%2e/evil/info/refs").is_err());
        assert!(parse_git_path("/git/..%2fx/info/refs").is_err());
        // 中间空段（a//b）；注：紧随 /git/ 的 // 会被剥一个前导 '/'，宽容等价单斜杠
        assert!(parse_git_path("/git/a//b/info/refs").is_err());
        // 端点白名单：拒绝 dumb 协议任意文件读 / 未知端点 / 无端点
        assert!(parse_git_path("/git/nexos.git/HEAD").is_err());
        assert!(parse_git_path("/git/nexos.git/objects/ab/cdef1234").is_err());
        assert!(parse_git_path("/git/nexos.git/evil").is_err());
        assert!(parse_git_path("/git/nexos.git").is_err());
        // 非 /git 前缀
        assert!(parse_git_path("/api/v1/x").is_err());
    }

    #[test]
    fn git_build_cgi_env_sets_required_vars() {
        // GET info/refs（clone 握手）场景
        let env = build_cgi_env(&GitCgiParams {
            project_root: "/tank/git-repos",
            path_info: "/nexos.git/info/refs",
            method: "GET",
            query: "service=git-upload-pack",
            remote_user: "nexos-agent",
            content_type: None,
            content_encoding: None,
            git_protocol: Some("version=2"),
            content_length: None,
        });
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("{k} 应存在"))
        };
        assert_eq!(get("GIT_PROJECT_ROOT"), "/tank/git-repos");
        assert_eq!(get("GIT_HTTP_EXPORT_ALL"), "1");
        assert_eq!(get("PATH_INFO"), "/nexos.git/info/refs");
        assert_eq!(get("QUERY_STRING"), "service=git-upload-pack");
        assert_eq!(get("REQUEST_METHOD"), "GET");
        assert_eq!(get("REMOTE_USER"), "nexos-agent");
        assert_eq!(get("HTTP_GIT_PROTOCOL"), "version=2");
        // GET 无体 → 不注入 CONTENT_TYPE / CONTENT_LENGTH
        assert!(!env.iter().any(|(n, _)| n == "CONTENT_TYPE"));
        assert!(!env.iter().any(|(n, _)| n == "CONTENT_LENGTH"));

        // POST git-upload-pack（fetch 数据）场景
        let env_post = build_cgi_env(&GitCgiParams {
            project_root: "/tank/git-repos",
            path_info: "/nexos.git/git-upload-pack",
            method: "POST",
            query: "",
            remote_user: "nexos-agent",
            content_type: Some("application/x-git-upload-pack-request"),
            content_encoding: Some("gzip"),
            git_protocol: None,
            content_length: Some(1234),
        });
        assert!(env_post
            .iter()
            .any(|(n, v)| n == "CONTENT_TYPE" && v == "application/x-git-upload-pack-request"));
        assert!(env_post
            .iter()
            .any(|(n, v)| n == "CONTENT_LENGTH" && v == "1234"));
        assert!(env_post
            .iter()
            .any(|(n, v)| n == "HTTP_CONTENT_ENCODING" && v == "gzip"));
    }

    #[test]
    fn git_parse_cgi_output_splits_headers_and_body() {
        // 404 场景（Status 头决定状态码；Status 不透传为普通头）
        let mut out = Vec::new();
        out.extend_from_slice(b"Status: 404 Not Found\r\n");
        out.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
        out.extend_from_slice(b"repository not found");
        let cgi = parse_cgi_output(&out).expect("应解析成功");
        assert_eq!(cgi.status, 404);
        assert!(cgi
            .headers
            .iter()
            .any(|(k, v)| k == "Content-Type" && v == "text/plain"));
        assert!(!cgi
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("status")));
        assert_eq!(cgi.body, b"repository not found");

        // 200 advertisement：缺 Status 头默认 200；body 为二进制 pkt-line 不受影响
        let mut out2 = Vec::new();
        out2.extend_from_slice(
            b"Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\nPragma: no-cache\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\nContent-Type: application/x-git-upload-pack-advertisement\r\n\r\n",
        );
        out2.extend_from_slice(b"001e# service=git-upload-pack\n0000");
        let cgi2 = parse_cgi_output(&out2).expect("应解析成功");
        assert_eq!(cgi2.status, 200);
        assert_eq!(cgi2.body, b"001e# service=git-upload-pack\n0000");
        assert_eq!(
            cgi2.headers
                .iter()
                .find(|(k, _)| k == "Content-Type")
                .map(|(_, v)| v.as_str()),
            Some("application/x-git-upload-pack-advertisement")
        );

        // 无头区分隔 → None（非 CGI 输出）；宽容 \n\n 分隔
        assert!(parse_cgi_output(b"garbage without separator").is_none());
        let cgi3 = parse_cgi_output(b"Status: 200 OK\n\nhi").unwrap();
        assert_eq!(cgi3.status, 200);
        assert_eq!(cgi3.body, b"hi");
    }

    fn git_post(
        uri: &str,
        auth: Option<(&str, &str)>,
        body: &[u8],
    ) -> axum::http::Request<axum::body::Body> {
        let mut b = axum::http::Request::builder().method("POST").uri(uri);
        if let Some((k, v)) = auth {
            b = b.header(k, v);
        }
        b.body(axum::body::Body::from(body.to_vec())).unwrap()
    }

    #[tokio::test]
    async fn git_http_push_paths_require_auth_401_or_503() {
        // 匿名 push 握手（GET info/refs?service=git-receive-pack）→ 401 +
        // WWW-Authenticate: Basic（git CLI 会弹认证）
        let router = git_router(Some("secret-token"), None);
        let resp = router
            .oneshot(git_get(
                "/git/nexos.git/info/refs?service=git-receive-pack",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(
            resp.headers().get("www-authenticate").unwrap(),
            &r#"Basic realm="NexHub Git""#.parse::<axum::http::HeaderValue>().unwrap()
        );

        // 匿名 push 数据（POST git-receive-pack）→ 401
        let router = git_router(Some("secret-token"), None);
        let resp = router
            .oneshot(git_post("/git/nexos.git/git-receive-pack", None, b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "匿名 push 数据路径必须鉴权");

        // 错误 Bearer token（push 握手）→ 401
        let router = git_router(Some("secret-token"), None);
        let resp = router
            .oneshot(git_get(
                "/git/nexos.git/info/refs?service=git-receive-pack",
                Some(("authorization", "Bearer wrong-token")),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // admin_token 未配置 → push 503（git 写访问强依赖 token，不留匿名写通道）
        let router = git_router(None, None);
        let resp = router
            .oneshot(git_get(
                "/git/nexos.git/info/refs?service=git-receive-pack",
                Some(("authorization", "Bearer anything")),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
    }

    #[tokio::test]
    async fn git_http_upload_pack_anonymous_read_allowed() {
        // 匿名读（clone/fetch）放行：临时目录建裸仓库 → 真实走 git-http-backend
        // （注入 repos_root 隔离；不设 admin_token 验证读路径与 token 无关）
        let tmp = std::env::temp_dir().join(format!(
            "os-git-http-anon-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--bare", tmp.join("demo.git").to_str().unwrap()])
            .output()
            .expect("git 应可用");
        assert!(
            init.status.success(),
            "git init --bare 失败: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        // ① 匿名 clone 握手 → 200 + smart advertisement（无任何凭据）
        let router = git_router(None, Some(tmp.to_string_lossy().into_owned()));
        let resp = router
            .oneshot(git_get(
                "/git/demo.git/info/refs?service=git-upload-pack",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "匿名 clone 握手应放行");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/x-git-upload-pack-advertisement"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );

        // ② 匿名 POST git-upload-pack（fetch 数据路径）→ 鉴权放行（非 401/503；
        //    空 body 的协议层错误由 git-http-backend 自行响应）
        let router = git_router(None, Some(tmp.to_string_lossy().into_owned()));
        let resp = router
            .oneshot(git_post("/git/demo.git/git-upload-pack", None, b""))
            .await
            .unwrap();
        assert_ne!(resp.status(), 401, "匿名 fetch 数据路径不应被鉴权拦截");
        assert_ne!(resp.status(), 503, "读路径不应依赖 admin_token");

        // ③ 穿越防护对匿名请求零放松（%2e%2e 编码穿越仍 400，先于任何放行）
        let router = git_router(None, Some(tmp.to_string_lossy().into_owned()));
        let resp = router
            .oneshot(git_get(
                "/git/%2e%2e/evil/info/refs?service=git-upload-pack",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn git_http_push_with_token_unaffected() {
        // 带 token 的 push 不受读写分流影响：receive-pack 握手带 Bearer → 200
        // advertisement（REMOTE_USER 已注入，git-http-backend 放行服务通告）
        let tmp = std::env::temp_dir().join(format!(
            "os-git-http-push-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--bare", tmp.join("demo.git").to_str().unwrap()])
            .output()
            .expect("git 应可用");
        assert!(init.status.success());

        let router = git_router(
            Some("secret-token"),
            Some(tmp.to_string_lossy().into_owned()),
        );
        let resp = router
            .oneshot(git_get(
                "/git/demo.git/info/refs?service=git-receive-pack",
                Some(("authorization", "Bearer secret-token")),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "token push 握手应 200: {resp:?}");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/x-git-receive-pack-advertisement"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn git_http_accepts_bearer_and_basic_token() {
        // 临时目录建裸仓库 → 真实走 git-http-backend（注入 repos_root 隔离）
        let tmp = std::env::temp_dir().join(format!(
            "os-git-http-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "--bare", tmp.join("demo.git").to_str().unwrap()])
            .output()
            .expect("git 应可用");
        assert!(
            init.status.success(),
            "git init --bare 失败: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        // Bearer token → 200 smart advertisement
        let router = git_router(
            Some("secret-token"),
            Some(tmp.to_string_lossy().into_owned()),
        );
        let resp = router
            .oneshot(git_get(
                "/git/demo.git/info/refs?service=git-upload-pack",
                Some(("authorization", "Bearer secret-token")),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"application/x-git-upload-pack-advertisement"
                .parse::<axum::http::HeaderValue>()
                .unwrap()
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            bytes.starts_with(b"001e# service=git-upload-pack"),
            "smart advertisement 应以 pkt-line service 头开始: {:?}",
            &bytes[..bytes.len().min(40)]
        );

        // Basic（用户名任意，密码 = admin token）→ 同样通过鉴权
        use base64::Engine;
        let basic_value = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("anyuser:secret-token")
        );
        let router = git_router(
            Some("secret-token"),
            Some(tmp.to_string_lossy().into_owned()),
        );
        let resp = router
            .oneshot(git_get(
                "/git/demo/info/refs?service=git-upload-pack",
                Some(("authorization", basic_value.as_str())),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "Basic（密码=token）应通过鉴权并 200");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
