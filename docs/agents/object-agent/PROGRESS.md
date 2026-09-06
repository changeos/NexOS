# object-agent 进度日志

## 当前状态
- 阶段：批 2 骨架完成（待提交）
- 最后更新：2026-08-05

## 已完成
- [x] 批 2 ObjectStore S3 模型 + 操作模型 + SigV4 字符串构造 + RustFsObjectStore 骨架 + MockObjectStore（本地分支 agent/object-agent，未 push）
  - `crates/os-protocols/src/object.rs`（增强）：完整 S3 数据模型（Bucket/ObjectMeta/ObjectVersion/VersioningConfig/LifecycleRule/AccessKey/CreatedAccessKey/BucketPermission + 构造器/校验）；操作模型（PutObject/GetObject/DeleteObject/ListObjects/MultipartUpload 请求响应）；`sigv4` 子模块（canonical_request / string_to_sign / credential_scope / canonical_query_string 纯字符串逻辑）；`RustFsObjectStore` 骨架（方法签名完整，真实 RustFS/HTTP 留 TODO）；`validate_bucket_name`（小写/3-63/DNS 兼容/IP 拒绝）。
  - `crates/os-protocols/src/object_mock.rs`（新增，feature `mock`）：`MockObjectStore` 纯内存实现，含构造器（with_bucket/with_object/with_error）+ 状态查询（bucket_count/object_count/access_key_count）+ 完整 trait 实现 + 单测。
  - `crates/os-protocols/src/lib.rs`：声明 `mod object_mock`（`#[cfg(feature="mock")]`），re-export 新类型 + `MockObjectStore`。
  - `crates/os-protocols/Cargo.toml`：新增 `[features] mock = []`。

## DoD 自检（批 2）
- [x] S3 数据模型完整 + bucket name 校验测试（11 条校验测）
- [x] ObjectStore 骨架（RustFsObjectStore，签名完整，调用留 TODO）
- [x] `cargo check -p os-protocols` 0 error（含/不含 mock feature 均 clean）
- [x] `cargo test -p os-protocols --features mock` 通过（37 测全过）
- [x] `cargo clippy -p os-protocols --features mock -- -D warnings` 无警告
- [x] Mock 类型已提交（MockObjectStore，feature `mock`）

## 进行中
- 无

## 阻塞
- ⛔ RustFsObjectStore 真实接通（S3 REST 调用）：阻塞于 RustFS 客户端 crate 未在 workspace 注册（红线「不虚构依赖」）。等主代理决策引入哪个 S3 客户端库（aws-sdk-s3 / rusoto / 自实现 reqwest+sigv4）。
- ⛔ access key 明文 secret 安全转交（CreatedAccessKey）的端到端流程：依赖 wallet-agent 的凭证消费契约（软依赖）。

## 下一步
1. 主代理裁决 S3 客户端选型 → 接通 RustFsObjectStore（逐方法实现 + 集成测，需 mock RustFS server 或 testcontainers）。
2. versioning / delete marker / 多段上传的端到端测（依赖真实或 mock S3 server）。
3. access key 颁发流程与 wallet-agent 对接（secret 一次性返回 → 安全转交）。

## 与 protocol-agent 协作点（重要）
本批工作与 protocol-agent 共享 `os-protocols` crate。为避免合并冲突，采取以下边界：

1. **独占文件**：本 agent 只改 `object.rs`（+ 新增 `object_mock.rs`）；**未触碰** protocol-agent 的 `common.rs`/`smb.rs`/`nfs.rs`/`webdav.rs`/`ftp.rs`/`sftp.rs`/`error.rs`。
2. **error.rs 未改**：校验失败沿用既有 `ProtocolError::CommandFailed(String)`（与 RustFsObjectStore 骨架一致）。**未新增** `InvalidBucketName` variant——新增须 ADR + 会签 protocol-agent。如需专用 variant，提 ADR-NNN 后再补。
3. **lib.rs 改动**：仅在 `pub mod` 块后追加 `#[cfg(feature="mock")] mod object_mock;` 一行 + 扩充 `object::{...}` re-export 列表 + 末尾追加 `#[cfg(feature="mock")] pub use object_mock::MockObjectStore;`。**未删改**既有 `pub mod` / `pub use` 任何一行。如与 protocol-agent 的 lib.rs 改动冲突，合并时保留双方并互评。
4. **Mock 文件命名**：刻意用 `object_mock.rs` 而非 `mock.rs`——后者是 protocol-agent 的领地（_conventions §5 默认 `mock.rs`，但本 crate 双 agent 共享，故 object-agent 让出 `mock.rs` 主名，自用 `object_mock.rs`）。下游 import 路径仍为 `os_protocols::MockObjectStore`（经 lib.rs re-export）。
5. **trait 签名未改**：`ObjectStore` 9 方法签名与 main 上的契约完全一致（批 0 已落）。仅扩充实现 + 新增类型，未破坏契约。

## 测试计数（真实）
- `cargo test -p os-protocols`（default）：28 passed
- `cargo test -p os-protocols --features mock`：37 passed（含 mock 9 条 + object.rs 28 条）
- 覆盖：bucket name 校验（11 条）、构造器（6 条）、SigV4 字符串构造（4 条）、操作模型 builder（2 条）、RustFsObjectStore 骨架（4 条）、MockObjectStore 全 trait（10 条）。
