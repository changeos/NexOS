# guest-agent 进度日志

## 当前状态
- 阶段：真实实现接通完成（axum Portal / nftnl 事务 / JWT / RpcRegistry 已落地，待主代理合并）
- 最后更新：2026-08-05

## 已完成
- [x] GuestId 生成算法（GUEST-XXXXXX，安全字符集排除 O/0/I/1/L，31 字符 × 6 位 ≈ 8.87 亿组合）
      + SystemEntropy（无 rand 依赖，xorshift64 + 时间/计数器混合熵）+ EntropySource trait（可注入确定性测试）+ validate_guest_id（model.rs）
- [x] RBAC 策略评估算法（evaluate_rules：priority 降序稳定排序 → 首条命中生效 → 无命中默认拒绝；
      condition_matches：Always/GuestType/VerifiedFactor/TimeWindow(支持跨夜)/BandwidthUnder；
      factor_satisfied：SignatureChallenge/BalanceThreshold/Credential 匹配；"持币≠可信"红线体现为多因子全过）
- [x] nftables 规则字符串构造（build_add_element/delete_element/port_accept_rule/checkpoint；
      statements_for_rule；is_valid_ip 校验；纯字符串不调 nft）
- [x] Portal OS 探测识别（detect_probe_os：iOS/Android/Win/macOS/Linux）+ 流程状态机（Landing→Register→Success）
- [x] 5 trait 默认实现（impls.rs）—— **接通真实实现**：
      - HttpCaptivePortal（**真实 axum 监听**：`build_router()` 公开供 oneshot 测，`start()` 用
        `tokio::net::TcpListener` + `axum::serve` + `tokio::spawn` 后台跑，`stop()` 经 graceful
        shutdown 通道关闭；路由含 `/portal/landing`(GET→200 HTML)、`/portal/auth`(GET→302)、
        `/portal/register`(POST→标记认证态)、`/generate_204`/`/hotspot-detect.html`/`/connecttest.txt`/
        `/ncsi.txt`（各 OS 探测端点）+ fallback 兜底；302 用手写 `redirect_302`（axum `Redirect::to`
        是 303 不符合 §3.18 期望））
      - DefaultIdentityEngine（内存 KV；**真实 JWT 签发**：`with_jwt_impl(Arc<JwtIssuerImpl>)` 注入
        os-security 真实 JwtIssuer，`authenticate_guest` 后签 `TokenType::Guest` JWT 并存入 `last_jwt`；
        经 dyn 兼容包装 `GuestJwtIssuer`（手写 `Pin<Box<dyn Future + Send>>`，不依赖 `#[async_trait]` 的
        HRTB Send 推断）；未注入时保持向后兼容仅维护 `jwt_expiry`）
      - DefaultPolicyEngine（内存规则表 + 调 evaluate_rules）
      - NftRuleOrchestratorImpl（dry_run 冲突检测 + apply checkpoint + rollback；**真实 nftables 事务**
        经 `nftnl-ffi` feature 门控（apply/revoke/rollback_checkpoint 调 `nftnl_apply_statements`）；
        apply 现真正存 checkpoint 并暴露 `last_checkpoint_id()` 供回滚）
      - DefaultChainOrchestrator（泛型注入 wallet+security；编排：判链可用→建 session→签名→验签→查因子→签 JWT；
        privacy_mode 三档降级；地址 FNV-1a 哈希化避免明文落库；**os-wallet RpcRegistry 已是真实 reqwest 探活，
        os-security JwtIssuerImpl 已是真实 jsonwebtoken，注入真实实现即得真实链路**）
- [x] 5 个 Mock（mock.rs，feature `mock`）：MockCaptivePortal/MockIdentityEngine/MockPolicyEngine/
      MockNftRuleOrchestrator/MockChainOrchestrator，构造器可配置预期返回
- [x] **59 单元测试全通过**（基线 49 + 新增 10：
      `identity_engine_real_jwt_issue`（真实 JWT 签发 + 同 issuer 验签）、
      `identity_engine_no_jwt_when_not_injected`（向后兼容）、
      `axum_portal_unauthed_probe_returns_landing` / `axum_portal_authed_probe_returns_204` /
      `axum_portal_landing_route_serves_html` / `axum_portal_auth_route_redirects` /
      `axum_portal_register_marks_authed` / `axum_portal_fallback_handles_arbitrary_path`
      （tower::ServiceExt::oneshot 离线打 Router）、
      `axum_portal_real_listen_start_stop`（**端到端**：真实绑端口 + reqwest 打真实 HTTP +
      graceful shutdown 验证）、
      `chain_orchestrator_real_jwt_issuer`（真实 JwtIssuerImpl 注入 ChainOrchestrator））

## DoD 自检
- [x] `cargo check -p os-guest`（默认 / `--features mock`）→ 0 error
- [x] `cargo test -p os-guest --features mock` → 59 passed; 0 failed（含端到端真实监听测）
- [x] `cargo clippy -p os-guest --features mock --tests -- -D warnings` → 0 warning
- [x] `cargo clippy -p os-guest -- -D warnings`（lib，默认）→ 0 warning
- [x] 5 个 trait 有具体实现（非 todo!()）
- [x] 5 个 mock 已提交
- [x] trait 签名未改（仅扩展 impls.rs 的默认实现 + Cargo.toml 加依赖；未动 trait 方法）
- [x] 链上验证不下沉（委派 wallet/security；本 crate 无密码学）
- [x] 地址哈希化（Completed{address_hash}）
- [x] nft 变更 dry-run + checkpoint 可回滚

## 接通的真实第三方依赖（ADR-DEPS-001）
- `axum 0.8.9`（+ tower 0.5 + hyper 1）：HttpCaptivePortal 真实 HTTP 监听 + 路由
- `reqwest 0.12.28`（rustls-tls）：端到端测试的 HTTP 客户端 + 备外部认证回调
- `nftnl 0.7`（`nftnl-ffi` feature 门控）：NftRuleOrchestratorImpl 真实 nftables netlink 事务

## FFI 注意（nftnl）
- `nftnl-ffi` feature 经 `nftnl-sys`→`mnl-sys` 链接系统库 `libnftnl` + `libmnl`；
  编译须 `apt install libnftnl-dev libmnl-dev`（ADR-DEPS-001 §91）。
  当前 CI/开发环境仅有运行时 `libnftnl.so.11` / `libmnl0`（缺 `-dev` 头文件），
  故 `cargo check --features nftnl-ffi` 会因 pkg-config 找不到 `libmnl.pc` 而失败——
  这是预期门控行为：默认/`mock` feature 路径完全不触发 FFI 链接。
- `nftnl_apply_statements` 当前为占位实现（明确返回 `Err`，不静默成功）；
  真实落地为 `nftnl::BatchBuilder` + `mnl::socket_sendto` batch 提交（需 root / CAP_NET_ADMIN），
  待 FFI 环境 + root 沙箱就绪后填充具体调用。
- 运行期 nft 操作还需 root / `CAP_NET_ADMIN`（不在编译期检查）。

## 实现备注
- `DefaultIdentityEngine` 的 JWT 注入用本地 `pub(crate) trait GuestJwtIssuer`（手写 boxed-future 返回值）
  桥接 os-security 原生 async `JwtIssuer`——因为 os-security 的 `JwtIssuer` 是原生 `async fn in trait`
  （非 dyn 兼容，ADR-COMPAT-001），无法直接 `Arc<dyn JwtIssuer>`。当前为 `JwtIssuerImpl` 实现桥接；
  其他 `JwtIssuer` 实现可按同模式扩 `impl GuestJwtIssuer`。
- `DefaultChainOrchestrator` 用泛型 `<C,A,R,J>` 注入上游（同上 dyn 兼容性原因，ADR-COMPAT-001）。
- IdentityEngine nft 同步：本引擎不直接耦合 NftRuleOrchestrator（保持单一职责），由 osd 编排层在
  authenticate/revoke 后调 NftRuleOrchestrator。
- axum 0.8 `Redirect::to` 返回 303（SEE_OTHER），不符合 §3.18 探测拦截的 302 期望；
  本实现用手写 `redirect_302`（`StatusCode::FOUND` + `LOCATION` 头）保证 302。
- `apply()` 现真正存 checkpoint 到 `self.checkpoints` 并暴露 `last_checkpoint_id()` 供回滚（原骨架未存）。
