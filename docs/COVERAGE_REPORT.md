# 测试覆盖率分析报告（cargo-tarpaulin）

> ⚠️ **快照口径：2026-08-06（main `1c65557` 段，2,096 测试时代）**。此后测试增至 4,100+
> （新增 handler / os-nexhub / 产品化功能均未入本统计），各 crate 百分比仅作历史基线参考；
> 若需新口径需重跑 tarpaulin。现况见 [CHANGELOG.md](../CHANGELOG.md) 累计统计。

> 分支：`feature/coverage-tarpaulin`（基于 main `1c65557`）
> 工具：`cargo-tarpaulin 0.37.0`（ptrace 引擎，Linux x86_64）
> 日期：2026-08-06
> 目的：项目 2096 测试 + 109 ignored 从未做过覆盖率分析，本次首次量化各 crate 的代码路径覆盖情况，
> 识别低覆盖区域与覆盖盲区，为后续补测提供方向。

---

## §1 覆盖率统计

### 1.1 工具与环境

- **cargo-tarpaulin 0.37.0**（`cargo install cargo-tarpaulin`，编译耗时约 5m48s，安装成功）。
- 引擎：ptrace（tarpaulin 默认；`--engine Auto` 解析为 Ptrace）。
- 运行方式：分批按 crate 跑（`-p <crate> --features mock --skip-clean`），避免单次全 workspace 跑耗时过长。
- 覆盖率口径：**src 目录行覆盖（line coverage）**，不含 `tests/`、`benches/` 测试代码本身。
  - 理由：tarpaulin 编译测试二进制时会把整个 workspace 依赖图都链进来，若把测试文件自身的行也算进
    分母会严重稀释单 crate 覆盖率；故本报告统一只统计各 crate 的 `src/` 生产代码。

### 1.2 各 crate 覆盖率（src 行覆盖）

按覆盖率从高到低排列。覆盖 = 被测试执行到的 src 行数；总 = src 可执行行数。

| Crate | 覆盖 / 总 | 覆盖率 | 说明 |
|-------|----------:|------:|------|
| os-provision | 583 / 621 | **93.9%** | PXE/检查点/传输，覆盖最充分 |
| os-common | 14 / 15 | **93.3%** | ApiError/Versioned，纯 DTO + 构造器测全 |
| os-iso | 538 / 588 | **91.5%** | ISO 构建/校验 |
| os-discover | 560 / 630 | **88.9%** | mDNS/SSDP 设备发现 |
| os-update | 785 / 921 | **85.2%** | A/B 槽位更新/回滚 |
| os-security | 742 / 887 | **83.7%** | ACME/TOTP/口令，impls 达 93% |
| os-storage | 632 / 771 | **82.0%** | ZFS backend/block/mock 高覆盖 |
| os-cli | 280 / 344 | **81.4%** | 命令行解析/分发 |
| os-im | 322 / 400 | **80.5%** | IM agent/会话 |
| os-guest | 609 / 753 | **80.9%** | 访客认证/过期 |
| os-i18n | 125 / 157 | **79.6%** | 国际化加载 |
| os-api | 500 / 635 | **78.7%** | HTTP/WS 网关/路由/链上校验 |
| os-meta | 559 / 746 | **74.9%** | openraft 共识/状态机（mock + impls） |
| os-protocols | 941 / 1324 | **71.1%** | SMB/NFS/FTP/SFTP 协议后端 |
| os-network | 712 / 1011 | **70.4%** | 接口/防火墙高覆盖，rtnetlink 真实后端低 |
| os-core | 86 / 130 | **66.2%** | EventBus 高（bus 92%/eventbus 100%），但 DTO 类型 0% |
| osd | 420 / 628 | **66.9%** | 编排/NTP/cgroup 中等，systemd_runner 仅 36% |
| os-wallet | 746 / 938 | **79.5%** | 签名/链/注册高，connector 真实后端低 |
| **合计（18 crate）** | **9154 / 11499** | **79.6%** | 加权平均 |

> **整体 src 覆盖率 ≈ 79.6%**（18 个 crate 的 src 行加权）。这是相当健康的水平——
> 多数 crate 落在 70%-94% 区间，主要"欠债"集中在真实环境/特权操作门控代码与纯 DTO。

### 1.3 未纳入本次统计的 crate

| Crate | 原因 |
|-------|------|
| **os-services** | **编译失败**：dev 依赖链 `candle-core → gemm → pulp-0.22.3` 在 Rust 1.97.1 上
触发 const-eval panic（`CheckSameSize::<i8, u16>::VALID` 断言失败，pulp 上游 bug），测试二进制无法编译。
属上游依赖问题，非本仓代码缺陷；需升级 pulp / candle 或等上游修复。 |
| os-compute | VM/容器管理，含较多真实环境门控，未纳入本次批次 |
| os-mobile | 移动端 HTTP 客户端，二进制性质 crate |
| os-desktop | 桌面端 |
| os-integration | 跨 crate 端到端集成测骨架（自身即测试 crate） |
| nettest | 临时网络连通性验证 crate（全 `#[ignore]`） |

> 上述 5 个跳过 crate 多为二进制/集成性质，单测覆盖意义有限；os-services 是被上游依赖阻塞，
> 建议优先解阻塞后补测。

---

## §2 低覆盖区域清单（src 覆盖率 < 50%）

> 仅列出 `src/` 下、可执行行 ≥ 9 且覆盖率 < 50% 的文件。`error.rs` 全员 0% 单列于 §3.1。

### 2.1 真实环境 / 特权操作门控（可解释的低覆盖）

| 文件 | 覆盖 / 总 | 覆盖率 | 原因 |
|------|----------:|------:|------|
| `osd/src/systemd_runner.rs` | 46 / 129 | **35.7%** | `TokioSystemdRunner` 真跑 `systemctl`/`systemd-run`，需 **root + systemd(PID1) + CAP_SYS_ADMIN**；真实集成测全 `#[ignore]`，仅 trait 抽象与纯函数被单测。 |
| `os-network/src/rtnetlink_real.rs` | 51 / 208 | **24.5%** | 真实 rtnetlink 写操作需 **root + CAP_NET_ADMIN**；仅纯函数（`map_bond_mode`/`classify_interface`/`link_message_to_interface`）被单测，netlink 实操标 `#[ignore]`（见 `docs/SANDBOX.md`）。 |
| `os-wallet/src/connector.rs` | 34 / 76 | **44.7%** | WalletConnect 真实连接/签名需外部钱包节点，部分路径标 `#[ignore]` 或 mock 注入。 |

### 2.2 纯 DTO / newtype（可解释的低覆盖）

| 文件 | 覆盖 / 总 | 覆盖率 | 原因 |
|------|----------:|------:|------|
| `os-core/src/types.rs` | 0 / 20 | **0.0%** | 纯领域 DTO（`HealthReport`/`Capacity`/`ResourceSpec` 等），只有结构定义 + 少量派生方法（`free_bytes`/`used_ratio`），无直接单测；下游 crate 间接用到但未在 os-core 内断言。 |
| `os-core/src/ids.rs` | 0 / 14 | **0.0%** | `string_id!` 宏展开的 newtype（`PoolId`/`VmId`/...），构造器/Display/From 实现未单测（宏展开代码 tarpaulin 计为可执行行但无测覆盖）。 |
| `os-i18n/src/locale.rs` | 5 / 12 | **41.7%** | locale 解析/匹配部分分支未覆盖。 |

### 2.3 测试基础设施自身的"真实测"文件（`tests/` 下，不计入 src 统计但值得注意）

> 这些是 `#[ignore]` 标记的真实环境集成测，tarpaulin 默认不跑 `--ignored`，故几乎 0 覆盖。
> 列出以说明它们存在但未计入（见 §3.2）。

| 文件 | 覆盖 / 总 | 覆盖率 | 性质 |
|------|----------:|------:|------|
| `os-storage/tests/block_real_export.rs` | 1 / 364 | 0.3% | 真实 ZFS zvol/iSCSI export，需 root + ZFS |
| `os-storage/tests/real_zfs_ops.rs` | 1 / 203 | 0.5% | 真实 ZFS 池/快照操作，需 root + ZFS |
| `os-storage/tests/replication_real.rs` | 96 / 360 | 26.7% | 真实 ZFS 复制，部分 mock 路径跑了 |

---

## §3 覆盖盲区说明（为何不计入 / 不应苛求）

### 3.1 `error.rs` 全员 0%（系统性，非缺陷）

**现象**：每个 crate 的 `error.rs`（thiserror `Error` 派生）覆盖率几乎都是 0%：
`os-core/error.rs 0/2`、`os-common`（无独立 error 统计）、`os-storage/error.rs 0/12`、
`os-meta/error.rs 0/15`、`os-network/error.rs 0/12`、`os-protocols/error.rs 0/15`、
`os-wallet/error.rs 0/14`、`os-security/error.rs 0/11`、`os-im/error.rs 0/11`、
`os-api/error.rs 0/9`、`osd/error.rs 0/12`、`os-provision/error.rs 0/9`、
`os-update/error.rs 0/12`、`os-discover/error.rs 0/10`、`os-iso/error.rs 0/9`、
`os-guest/error.rs 0/13`、`os-cli/error.rs 0/10`、`os-i18n/error.rs 0/2`。

**原因**：thiserror 的 `#[error("...")]` 派生的 `Display::fmt` 实现体，tarpaulin 计为可执行行，
但单测通常只断言 `Error` 变体本身（`assert_eq!(e.code, ...)`）而不调用 `e.to_string()`/`format!("{e}")`。
部分 crate（如 os-common）专门补了 Display 一致性测后 error.rs 才有覆盖。

**结论**：这是 thiserror + line coverage 的系统性盲区，**不应苛求 100%**。如要覆盖，给每个
错误变体加一行 `format!("{e}")` 冒烟测即可（os-common 已示范）。

### 3.2 `#[ignore]` 真实环境测（109 个）不计入

项目有 **109 个 `#[ignore]` 测试**，全部需要真实环境（root / systemd / ZFS / 网络特权 / 外部钱包 / GPU），
默认 `cargo test` 不跑，tarpaulin 默认也不跑（除非加 `-- --ignored`）。

涉及的真实环境维度：
- **systemd**：`osd` 的 `TokioSystemdRunner`（需 root + systemd PID1）
- **ZFS**：`os-storage` 的真实池/快照/复制/export（需 root + ZFS 内核模块）
- **netlink**：`os-network` 的 rtnetlink 写操作（需 root + CAP_NET_ADMIN）
- **网络栈**：`nettest` crate 全部（reqwest/axum/mdns-sd/rustls 真实连通性）
- **钱包**：`os-wallet` 的 WalletConnect 真实连接/签名（需外部钱包节点）
- **GPU**：`os-services` 的 candle CUDA 推理（需 NVIDIA GPU + `clip-cuda` feature）
- **集群**：`os-meta` 的多节点 openraft 共识

这些是**有意设计的分层**（见 `docs/SANDBOX.md` 与各 crate 的真实测文档）：单测保证逻辑正确性，
真实环境测在沙箱/CI 特殊 job 验证集成。**覆盖率工具默认不跑它们是预期行为**，不应视为覆盖缺口。

### 3.3 FFI / 系统调用门控代码

部分代码用 `#[cfg(feature = "...")]` 或运行时能力探测门控，非目标环境编译时直接排除
（如 `clip-cuda` / `git-remote` feature）。tarpaulin 在默认 feature 下跑，这些门控分支天然不覆盖。

### 3.4 os-services 编译失败（上游依赖阻塞）

`pulp-0.22.3`（经 `candle-core` ML 推理依赖引入）在 Rust 1.97.1 的 const-eval 阶段 panic：
```
error[E0080]: evaluation panicked: assertion failed:
  core::mem::size_of::<T>() == core::mem::size_of::<U>()
  --> pulp-0.22.3/src/lib.rs:3291:3 (CheckSameSize::<i8, u16>::VALID)
```
这是 pulp 上游在新 rustc 上的已知兼容问题，导致 os-services 测试二进制无法编译，
tarpaulin 无法对其收集覆盖率。**非本仓代码缺陷**。建议：升级 `candle-core`/`pulp` 或 pin 兼容版本。

---

## §4 提升建议

按"性价比"（低成本高收益）排序：

### 4.1 高性价比（纯 DTO / 构造器冒烟测，快速拉高 0% 文件）

1. **`os-core/src/types.rs` (0/20) 与 `ids.rs` (0/14)**：
   - 补一组冒烟测：构造各 DTO、调 `free_bytes()`/`used_ratio()`、`TaskId::new()` 唯一性、
     各 newtype 的 `new`/`as_str`/`Display`/`From<String>`。
   - 预期：直接把 os-core 从 66.2% 拉到 ~85%+。零业务风险。

2. **各 `error.rs`（全员 0%）**：参照 `os-common` 的 `error_code_display_matches_expected_snake_case`
   模式，给每个 thiserror 变体加一行 `let _ = format!("{e}");` 冒烟测。系统性消除盲区。

3. **`os-i18n/src/locale.rs` (41.7%)**：补 locale 解析边界分支（非法输入 / 大小写 / fallback）。

### 4.2 中性价比（真实后端的纯函数 / mock 补强）

4. **`os-network/src/rtnetlink_real.rs` (24.5%)**：纯函数 `map_bond_mode`/`classify_interface`/
   `link_message_to_interface` 已可测，但覆盖不全；补更多 `LinkMessage` 构造用例（Vlan/Bridge/Bond/Loopback）。

5. **`os-meta/src/raft_backend.rs` (54.1%)** 与 **`mock.rs` (68.9%)**：raft 日志后端的 mock 路径
   可再补几条；`impls.rs` (80.1%) 的未覆盖行多在错误分支。

6. **`os-protocols/src/mock.rs` (36.8%)**：协议 mock 实现覆盖偏低，补 mock 行为断言。

7. **`osd/src/systemd_runner.rs` (35.7%)**：trait 抽象层（`SystemdRunner` trait + `UnitType`）
   可在无 systemd 环境用 fake runner 测编排逻辑；真实 systemctl 路径保持 `#[ignore]`。

### 4.3 解阻塞优先级

8. **解 os-services 阻塞**：升级 `candle-core`（当前 0.11.0）/`pulp`（0.22.3）到兼容 Rust 1.97 的版本，
   或在 `Cargo.toml` 用 `[patch]`/`[workspace.dependencies]` pin 一个不触发 const-eval panic 的 pulp 版本。
   解阻塞后即可纳入覆盖率统计（os-services 体量大，含 media/backup/monitor 等多子域）。

### 4.4 不建议补（保持现状）

- **真实环境 `#[ignore]` 测**：设计上需特殊环境，单测覆盖率工具不应苛求；交给沙箱/CI 特殊 job。
- **`nettest` crate**：临时验证 crate，全 `#[ignore]`，不参与 CI。
- **FFI/feature 门控分支**：非目标环境编译期排除，覆盖率盲区是预期。

---

## §5 复现命令

### 5.1 安装

```bash
cargo install cargo-tarpaulin   # 首次约 5-6 分钟
cargo tarpaulin --version       # 应输出 cargo-tarpaulin-tarpaulin 0.37.0
```

### 5.2 单 crate 覆盖率（推荐分批跑）

```bash
# os-core（带 mock feature）
cargo tarpaulin -p os-core --features mock \
  --out Html Stdout --output-dir /tmp/coverage-core --skip-clean

# os-common（无 mock feature，去掉 --features）
cargo tarpaulin -p os-common \
  --out Html Stdout --output-dir /tmp/coverage-common --skip-clean

# 多 crate 合并
cargo tarpaulin -p os-storage -p os-meta --features mock \
  --out Html Stdout --output-dir /tmp/coverage-storage-meta --skip-clean
```

> **注意**：tarpaulin 编译测试二进制时会链入整个 workspace 依赖图，故 `--output-dir` 生成的
> HTML 报告会包含所有 crate 的文件（体积大，单报告 ~8MB）。看单 crate 覆盖率应以 **stdout 的
> `|| crates/<crate>/src/<file>: covered/total` 行**为准（本报告数据来源）。

### 5.3 关键 flag 说明

| Flag | 作用 |
|------|------|
| `--skip-clean` | 跳过 `cargo clean`，复用已有编译产物，省时 |
| `--features mock` | 解锁各 crate 的 Mock 注入路径（与 Makefile/CI 一致） |
| `-t 60` | 单测试无响应超时 60s（默认 1 分钟） |
| `--out Html Stdout` | 同时输出 HTML 报告与终端逐行统计 |
| `-- --ignored` | 跑 `#[ignore]` 真实环境测（需对应环境，慎用） |

---

## §6 结论

- **整体 src 覆盖率 ≈ 79.6%**（18 crate 加权），处于健康区间，主要"欠债"集中在可解释的盲区
  （真实环境门控 / 纯 DTO / thiserror Display），而非逻辑覆盖不足。
- **覆盖最充分**：os-provision (93.9%)、os-common (93.3%)、os-iso (91.5%)、os-discover (88.9%)。
- **最需补测**：os-core (66.2%，纯 DTO 0% 易补)、osd (66.9%，systemd 真实后端)。
- **最需解阻塞**：os-services（pulp 上游 const-eval panic，非本仓缺陷）。
- **覆盖盲区是设计产物**：109 个 `#[ignore]` 真实环境测 + FFI/feature 门控分支，按沙箱分层设计
  有意排除在单测覆盖率之外，不应苛求。

本报告为**首次覆盖率基线**，建议后续每次重大改动后复跑对应 crate 批次做回归对照。
