# protocol-agent 进度日志

## 当前状态
- 阶段：P2 真实协议栈已接通（WebDAV/FTP/SFTP），待评审
- 最后更新：2026-08-05

## 已完成
- [x] 批 2：文件协议配置生成 + 编排骨架 + Mock（分支 `agent/protocol-agent`）
  - 配置生成（真实纯函数实现 + 测试）：
    - `SambaConfig::render_global` / `SambaShareSpec::render` / `render_smb_conf`（smb.conf `[global]` + `[share]` 段，含 vfs_fruit Time Machine）
    - `NfsExportsEntry::render` / `render_exports`（NFSv3 `/etc/exports`）
    - `GaneshaConfig::render_global` / `GaneshaExport::render` / `GaneshaConfig::render`（NFSv4 ganesha.conf EXPORT 块）
    - `WebDavConfig::render` / `FtpConfig::render` / `SftpConfig::render` / `render_authorized_keys`
  - 会话/共享状态机：`ShareState`（Creating→Active→Stopping→Stopped，带迁移校验）+ 内存 `ShareStore`（共享/会话 CRUD + 级联）
  - Mock（feature `mock`，5 个）：`MockSmbManager`/`MockNfsManager`/`MockWebDavManager`/`MockFtpManager`/`MockSftpManager`

- [x] P2 真实协议栈接通（分支 `p2/protocol-agent`）：dav-server / libunftp / russh 已接入 `os-protocols`
  - **WebDAV（`DavServerBackend`）**：每共享一份真实 `dav_server::DavHandler`（MemFs 后端 + FakeLs 锁系统）；
    `handle_request(id, req)` 把真实 `http::Request` 喂入 dav-server 处理器，离线驱动 RFC4918
    （PROPFIND→207 / MKCOL→201 / PUT→成功）。`create_share`/`delete_share` 挂载/卸载处理器。
  - **FTP（`LibunftpBackend` + 新模块 `ftp_backend`）**：实现纯内存 `InMemoryFtpBackend`
    （`StorageBackend<DefaultUser>`，list/metadata/get/put/del/mkd/rmd/rename/cwd）；
    `build_server(id)` 构造真实但**未监听**的 `libunftp::Server`（匿名认证 + passive_ports + greeting）。
  - **SFTP（`RusshSftpBackend` + 新模块 `sftp_backend`）**：实现 `OsSshHandler`（`russh::server::Handler`，
    authorized_keys 公钥认证 + SFTP 子系统请求）+ `OsSshServer`（`russh::server::Server`）；
    `build_ssh_server()` / `build_ssh_config()` 构造真实但未监听的 SSH 服务端工厂 + 带 Ed25519 主机密钥的配置。
  - **依赖接入**（`os-protocols/Cargo.toml`）：`dav-server.workspace=true`（+`memfs` feature）、
    `libunftp.workspace=true`（`default-features=false`+`ring`，与 rustls/ring 共栈）、
    `russh.workspace=true`（`default-features=false`+`ring`+`flate2`+`rsa`）；
    辅助：`unftp-core`、`async-trait`、`http`/`http-body`、`rand`。
    workspace 根：libunftp/russh 改为 `default-features=false`（ADR-DEPS-002 ring 策略落地）+ 新增 `http` 注册。
  - **不真监听端口**（红线）：三个后端仅持有真实协议栈对象，端口绑定由上层（api/service）负责。
  - SMB/NFS 维持 CLI 骨架（未引入 samba crate，务实边界）；`object.rs` 未改（object-agent 负责）。

- DoD（真实输出）：
  - `cargo check -p os-protocols --features mock`：0 error
  - `cargo test -p os-protocols --features mock`：**91 passed** / 0 failed（基线 84 + 新增 7）
    - 新增协议栈测试：`webdav_drives_real_protocol_stack`（PROPFIND/MKCOL/PUT 真实驱动）、
      `ftp_builds_real_server_and_drives_storage`（Server 构造 + StorageBackend 驱动）、
      `sftp_builds_real_ssh_server_and_config`（SSH Server + Config 构造）、
      `webdav_lifecycle`/`ftp_lifecycle`（挂载计数）、`memory_backend_put_get_list_del`、
      `handler_accepts_known_pubkey_rejects_unknown`、`build_ssh_config_has_ed25519_key_and_pubkey_only` 等
  - `cargo clippy -p os-protocols --features mock --all-targets -- -D warnings`：0 warning
  - `cargo doc -p os-protocols --features mock --no-deps`：无警告
  - `cargo check --workspace --features mock`：通过（未破坏其他 agent crate）

## 进行中
- 无

## 阻塞
- 无

## 下一步
1. 真实协议栈的"端口监听"由 api/service agent 在 axum/hyper 路由中挂载
   （`DavServerBackend::handler`/`handle_request`、`LibunftpBackend::build_server`、
   `RusshSftpBackend::build_ssh_server`/`build_ssh_config` 均已就绪）。
2. SMB 编排落盘（`SambaOrchestrator::write_smb_conf` 写 /etc/samba/smb.conf + `reload_smbd` smbcontrol）。
3. NFS 编排（写 /etc/exports + exportfs -ra / ganesha.conf + reload）。
4. smbstatus 解析（SMB 会话）/ nfs v4 状态回收（真实有状态会话）。
5. 切换到 storage-agent 真实数据集路径（当前 Share.path 由上游传入，已解耦）。

## 设计要点 / 决策
- 真实协议栈接入但不监听端口（红线）：三个后端持有真实协议对象，暴露构造入口供上层绑定端口；
  测试用离线 fixture（MemFs / InMemoryFtpBackend / 直接驱动 handler）验证协议栈真的接通。
- dav-server 用 `memfs` feature（离线可测、无磁盘依赖）；libunftp/russh 用 `ring` 后端
  （与 workspace rustls/ring 栈共栈，符合 ADR-DEPS-002 选型理由）。
- trait 签名零改动（`FileProtocol`/各子 trait 未动），`common.rs`/`error.rs`/`object.rs` 未改。
- mock trait 用原生 `async fn in trait`（具体类型实现，非 `Box<dyn>`），与 network/storage mock 风格一致。
- `InMemoryFtpBackend` 为离线测试的最小实现（生产应换为 `unftp-sbe-fs` 等真实 fs 后端）。
- `OsSshHandler` 公钥认证以 base64 段比对（与 authorized_keys 行格式解耦）。
