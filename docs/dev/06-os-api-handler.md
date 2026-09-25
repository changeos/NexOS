# os-api Handler 开发模式：新增 RouteHandler 装配流程

> 目标：给 os-api 网关新增一个业务组件（`RouteHandler`）——以
> `agent_coord.rs`（agent 协调组件，25 个单测）为范例走通装配六步。
>
> 前置：Rust async（`#[async_trait]`）；读过 `crates/os-api/src/gateway.rs`
> 顶部契约注释。**红线：不动既有 handler 文件**（mod.rs / main.rs 的注册行除外）。

## 0. 装配六步总览

```text
① crates/os-api/src/handlers/foo.rs   新 handler（routes() + handle() + 单测）
② handlers/mod.rs                      pub mod foo; pub use foo::FooRouteHandler;（+模块注释）
③ main.rs                             use 导入 + gw.register_component("foo", …)（+组件清单日志）
④ 测试                                cargo test -p os-api --lib foo（≥6，全绿）
⑤ 质量                                cargo clippy -p os-api --all-targets（-D warnings 零告警）
⑥ 文档                                docs/FOO.md（端点契约/env 表/拓扑图）——功能文档同步铁律
```

## 1. 新建 handler 文件（范例骨架 = agent_coord.rs / update.rs）

```rust
//! `FooRouteHandler` —— 一句话定位（模块文档：设计来源/持久化/鉴权/路由表）。
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

pub struct FooRouteHandler { /* 状态：Mutex/RwLock/句柄 */ }

impl FooRouteHandler {
    pub fn new() -> Self { /* env 读取 + 缺省路径 */ }
}

#[async_trait]
impl RouteHandler for FooRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![spec(HttpMethod::Get, "/api/v1/foo")]   // requires_auth=false 开发期公开
    }
    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            (HttpMethod::Get, ["api", "v1", "foo"]) => Ok(ok_json(serde_json::json!({...}))),
            _ => Ok(error_response(404, "未知 foo 端点")),
        }
    }
}
```

每个 handler 文件自带私有小工具（`spec` / `ok_json` / `error_response` /
`to_value` / `path_segments`——见 `update.rs` 文件尾，不共享避免跨文件耦合）。

## 2. handlers/mod.rs 注册

```rust
pub mod foo;                       // 按字母序插入
pub use foo::FooRouteHandler;      // 同序
```

并在模块文档注释链（`//! - [\`foo\`]：…`）加一条——这是其他 agent 的目录。

## 3. main.rs 装配

```rust
// ① use 链（按字母序）：
use os_api::handlers::{…, FooRouteHandler, …};
// ② build_gateway() 内 register_component（带注释块：端点/env/鉴权）：
gw.register_component("foo", Box::new(FooRouteHandler::new()))
    .await
    .expect("注册 foo handler");
// ③ 组件计数日志 [check] 已注册组件（… + foo）与阈值同步 +1
```

路由匹配由 `routing.rs` 统一处理：静态路由 O(1)，`:id` 参数段与 `*`
catch-all 按 specificity 排序（静态段 2 分/参数 1 分/通配 0 分）——静态
路由优先于参数路由，同名不冲突。

## 4. 测试惯例（agent_coord.rs 同款）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // 请求构造器（get_req/post_req）+ TempDirGuard（workspace 无 tempfile，自管清理）
    // 每个端点至少一条正向 + 一条边界（404/400/403/降级）
    #[tokio::test]
    async fn routes_declared_and_auth_conventions() {
        let h = FooRouteHandler::new();
        for r in h.routes().await {
            assert!(!r.requires_auth);            // 开发期公开读
            assert_eq!(r.handler_component, "foo");
        }
    }
}
```

- 跑法：`cargo test -p os-api --lib foo`（全量 `cargo test -p os-api --lib`）；
- 持久化 handler（JSON 状态）用 `std::env::temp_dir()` fixture 走
  写→重开→读回三段（参考 `update.rs::channel_switch_persists_and_survives_reopen`）；
- env 注入用 `with_config(Some(path), …)` 构造器，不用 set_var。

## 5. 鉴权约定

| 场景 | requires_auth / roles |
|---|---|
| 开发期公开读（观察面/文档） | `false`，roles 空 |
| 写操作 | `true` + `["admin"]`（网关 Bearer，`NEXOS_ADMIN_TOKEN`） |
| 链上身份端点 | 自挂 `ChainAuth`（`chain_auth::bearer_token(headers)` 手工解析） |

## 6. 持久化约定

JSON 文件 + **原子写**（先写 `.tmp` 再 rename，`update.rs` 同款）；目录不存在
自动创建；读取缺失/损坏 → 空态降级不阻塞启动。env 命名 `NEXOS_XXX_FILE`，
缺省 `/tank/os-data/xxx.json`。

## 参考

- 范例全量：`crates/os-api/src/handlers/agent_coord.rs`（钩子注入/桥接/25 测试）
- 契约：`crates/os-api/src/gateway.rs`（RouteHandler trait）与
  `crates/os-common/src/gateway.rs`（下沉的 RouteSpec/ApiResponse）
- 领域 crate 独立化先例：os-nexhub（经 blanket impl 桥接，
  [../COMPONENT_INDEPENDENCE_AUDIT.md](../COMPONENT_INDEPENDENCE_AUDIT.md) §6）
