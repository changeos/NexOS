# client-agent 进度日志

## 当前状态
- 阶段：接通真实实现（reqwest HTTP 传输）已完成，待主代理合并
- 最后更新：2026-08-05

## 已完成
- [x] 接续前 owner 未完成的"无依赖骨架"工作（commit: 本批，见 `git log`）
- [x] os-mobile（3 trait + 实现 + 3 Mock，原 61 测 + 2 doctest）：
  - `HttpOsClient`（OsClient 骨架）：会话状态机（connect/disconnect/pair）、请求构造（build_status/discover/pair_request 用 http 模块）；真实 reqwest HTTP 留 TODO
  - `InMemoryPushSubscriber`（PushSubscriber）：Arc<dyn PushCallback> 持有、订阅状态机、NotificationQueue 缓存、deliver 桥接（不持锁跨 await）；修复 deliver 的 moved-value 逻辑（DeliverAction 枚举）
  - `PushCallback` trait 已 `#[async_trait]`（ADR-COMPAT-001，前 owner 已加）
  - `http` 模块（URL 构造/查询编码/JSON 解析纯函数）：RequestSpec、build_url、encode_query、percent_encode（RFC3986 %20）、JsonResponse、parse_json_response
  - `retry` 模块（重试退避决策纯函数）：RetryPolicy（指数退避封顶）、decide_retry、RetryableError（5xx/429/超时可重试，4xx 不重试）
  - 3 个 Mock：`MockOsClient` / `MockPushSubscriber` / `MockPushCallback`
- [x] os-desktop（1 trait + 实现 + 1 Mock，原 20 测）：
  - `SystemMountManager`（MountManager 骨架）：内存挂载表、mount/unmount/list_mounts/make_persistent 状态机
  - 命令构造纯函数：`build_net_use_command`（Windows SMB net use Z: \\\\host\\share /USER）、`build_davfs2_command`（Linux WebDAV mount -t davfs）、`build_fstab_line`（持久化）；真实 std::process 执行留 TODO
  - 两端契约一致性：os-desktop `pub use os_mobile::client::{OsClient, ClientSession, SystemStatus}`（复用，未重复定义）
  - 1 个 Mock：`MockMountManager`
- [x] DoD 自检：`cargo check/test/clippy -p os-mobile -p os-desktop --features mock --all-targets -- -D warnings` 全绿

### 本批：接通 reqwest 真实 HTTP 传输（branch `real/client-agent`）
- [x] **os-mobile/Cargo.toml**：加 `reqwest.workspace = true`（ADR-DEPS-001：rustls-tls，无 openssl——编译观察到 `tokio-rustls`/`hyper-rustls`，无 `native-tls`/`openssl-sys`）
- [x] **`transport.rs`（新模块）**：`HttpTransport` trait（`#[async_trait]`，Box<dyn> 动态分发——ADR-COMPAT-001）+ `ReqwestTransport`（reqwest::Client 真实实现）+ `TransportError`/`TransportResult`。reqwest 错误归一成 `RetryableError`（连接/超时/DNS/状态码分类），与 retry.rs 解耦。
- [x] **`client_impl.rs` 重写**：`HttpOsClient` 持有 `Arc<dyn HttpTransport>`（生产 `ReqwestTransport`，测试注入 `FakeTransport`）。`get_system_status`/`discover_nodes`/`pair` 真实发 HTTP：
  - `send()` 编排重试循环：transport.send → 失败分类 → `decide_retry`（复用既有纯决策）→ sleep(+10% jitter，零依赖，不引 rand) → 重试
  - `pair` 用 `RetryPolicy::no_retry()`（重复提交可能创建多会话）
  - 错误映射：401/403 → EndpointUnreachable("鉴权失败: ...")；其余 → EndpointUnreachable
  - 会话状态机不变（connect/disconnect 本地变更）
- [x] **测试新增 17 项**（无网络请求）：
  - os-mobile +13：FakeTransport 重试编排（503 重试后成功 / 用尽 max_attempts GiveUp / 404 不重试）、错误映射、CountingTransport、**3 项真实 reqwest 经 loopback HTTP**（GET /status 解析 / 404 映射 / 连接被拒重试后 GiveUp）
  - os-desktop +4：list_available_shares 经 os-mobile HttpTransport（FakeTransport 解析 / 404 映射 Internal / 无 transport 回退注入 / **真实 reqwest loopback**）
- [x] **os-desktop**：`SystemMountManager::with_transport(Arc<dyn HttpTransport>)`，`list_available_shares` 注入 transport 时经网关 `GET /shares`（复用 os-mobile `ReqwestTransport`，不改 Cargo）；未注入时回退本地注入 shares（向后兼容）。挂载命令真实执行仍留 TODO（不真挂载，符合任务约束）。

## 测试计数（本批后）
| crate | 原有 | 本批后 |
|-------|------|--------|
| os-mobile | 61 + 2 doctest | **74 + 2 doctest** |
| os-desktop | 20 | **24** |

`cargo check/test/clippy -p os-mobile -p os-desktop --all-targets --features mock -- -D warnings` 全绿；默认 features 同样全绿。

## 阻塞
- ⛔（本批已接通 reqwest HTTP，原 HTTP 阻塞已解除）：
  - 真实挂载命令执行（SystemMountManager.mount）：net use / mount -t davfs 命令已构造，真实 std::process 执行待桌面运行时
  - 持久化写注册表/fstab：构造了配置行，真实落盘待集成
  - FCM/APNs 平台桥接：PushSubscriber.deliver 是桥接入口，平台 SDK 接入待移动端运行时
  - WS 推送长连接：PushSubscriber.deliver 是桥接入口；真实 WS（tokio-tungstenite）未在 workspace 注册，待后续 ADR

## 契约兼容性（非破坏性）
- `PushSubscriptionState` 加 `#[derive(Default)]` + `#[default]`（原手写 impl Default 被 clippy 标记可派生）
- 三个 trait（OsClient/PushSubscriber/MountManager）原生 async fn，impl 用原生 async（不挂 #[async_trait]，与 trait 一致）
- 本批新增 `transport.rs` + `HttpOsClient::with_transport`/`with_retry_policy`/`new` 改为返回 `Result`（原 `new()` 无参，Default 保留）。`Default::default()` 内部 expect（reqwest::Client 默认可用）。
- os-desktop `SystemMountManager` 加字段 `transport` + `with_transport`（向后兼容，原 `new()`/`with_shares()` 签名不变）。

## 下一步
1. 主代理统一合并 `real/client-agent` 分支
2. 待桌面运行时接入后执行真实挂载命令
3. 前端工程（Vue/Capacitor/Tauri）用本批 Mock 并行开发
4. WS 推送长连接待 tokio-tungstenite 注册后接入

## DoD 勾选（规格 §5.2）
- [x] 4 个 trait 有具体实现（OsClient/PushSubscriber/PushCallback + MountManager）
- [x] `cargo check -p os-mobile -p os-desktop` 通过
- [x] `cargo test -p os-mobile -p os-desktop` 通过（74+2 + 24 测）
- [x] `cargo clippy -p os-mobile -p os-desktop -- -D warnings` 无警告
- [x] mock 已提交（os-mobile/src/mock.rs 3 个 + os-desktop/src/mock.rs 1 个，feature gate `mock`）
- [x] PROGRESS.md 已更新
