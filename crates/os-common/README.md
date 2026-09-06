# os-common

> API 通用层 · 统一错误码 / 版本封装 / 网关契约 / 链上身份内核 · owner：随 os-api 演进

OS 系统的 API 通用层：`ApiError` 统一对外错误码、`Versioned` API 版本封装、
`RouteHandler` 网关契约与 `chain_auth` 链上身份认证内核——所有领域 crate 对外暴露
HTTP 路由与错误汇聚的公共底座。

## 核心能力

- **统一错误码**（`error`）：`ApiError` / `ApiErrorCode` / `ApiResult`——11 个错误码
  变体（`not_found` / `invalid_input` / `permission_denied` / `rate_limited` /
  `failover_failed` 等）；各 crate Error 经 `From<XxxError> for ApiError` 汇聚，
  由 os-api 网关统一序列化返回前端。
- **错误码序列化契约**：`ApiErrorCode` serde 输出 snake_case 且与 `Display` 一致，
  拒绝 PascalCase 输入；可选字段 `task_id` / `details` 为 `None` 时跳过。
- **API 版本规范**（`versioned`）：`Versioned` trait（默认返回
  `CURRENT_API_VERSION`）+ `VersionedEnvelope<T>`（`#[serde(flatten)]` 统一带
  `api_version` 字段，呼应规划文档 §12.3）。
- **网关契约**（`gateway`，NexHub 独立化下沉，审计 §6.2 方案 1）：`RouteHandler`
  trait（`#[async_trait]` dyn 兼容）+ `HttpMethod` / `RouteSpec` / 契约版
  `ApiRequest` / `ApiResponse` / `HandlerError`——领域 crate（如 os-nexhub）自带
  handler，os-api 装配层桥接注册。
- **链上身份认证内核**（`chain_auth`，2026-08-18 随 b96affd 加入）：`ChainAuth`
  nonce/token 桶 + k256 验签 + secp256k1 压缩公钥身份——IM 认证与 NexHub 大厅写操作
  共用的挑战-签名三步契约（挑战 → 签名 → 发 token；token 桶管理过期）。第三方依赖
  +k256 / tiny-keccak。设计见 `docs/IM_BLOCKCHAIN_AUTH_DESIGN.md` §6 与
  `docs/MEDIA_GEN_AND_CHAIN_AUTH.md` §C。

## 架构位置

**依赖**（上游）：`os-core`（`From<CoreError> for ApiError`）；第三方 serde /
serde_json / thiserror / async-trait（chain_auth 再加 k256 / tiny-keccak）。

**被用**（下游）：workspace 全部业务 crate（含 os-nexhub 与所有客户端/服务
crate）——是依赖面最广的公共契约层。

## 独立使用

- **仓库外引用**：`os-common = { git = "http://ub2604:8080/git/nexos.git" }`。
- **关键接口**：
  - `ApiError::not_found / invalid / permission / internal`：错误码快捷构造器。
  - `Versioned` / `VersionedEnvelope`：DTO 声明自身 API 版本的最小成本方式。
  - `gateway::RouteHandler`：领域 crate 自带 HTTP 路由的注册抽象
    （与 os-api 网关版同形，装配层做 `HandlerError → ApiGatewayError` 映射）。
- **无 feature**：零 feature 门控，纯契约 + DTO。

## 测试

```bash
cargo test -p os-common
```

lib 内 13 个单测（review2 P6 补齐）：错误码 Display/serde 双向一致、构造器映射、
`From<CoreError>` 转换、envelope flatten 序列化往返、`Versioned` 默认实现。
