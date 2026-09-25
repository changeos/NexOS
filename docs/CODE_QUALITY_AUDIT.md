# 代码质量审计报告（clippy pedantic + cargo-audit + cargo-deny + 覆盖率）

> 审计时间：2026-08-06（batch7 clippy pedantic + batch8 cargo-audit/覆盖率 + batch9 cargo-deny/cargo-udeps）
> §3 cargo-audit 补完：2026-08-06（绕过代理直连 fetch advisory-db，完成真实扫描）
> §6 cargo-deny / §7 cargo-udeps：2026-08-06（batch9 供应链深化 + 未用依赖检测）
> 基线：main `1c65557`（2096 passed + 109 ignored = 2205）

---

## 1. clippy pedantic 统计（batch7，2026-08-06）

全量 `-W clippy::pedantic` 扫描：**3304 个 warning**。

Top lint 规则分布（从 `/tmp/clippy-pedantic-full.log` 统计）：

| 类别 | 代表 lint | 说明 |
|------|-----------|------|
| 文档类（最多） | `missing_errors_doc` / `missing_panics_doc` / `missing_errors_doc` | 纯文档缺失，不藏 bug，量大未修 |
| 风格类 | `must_use_candidate` / `needless_pass_by_value` / `return_self_not_must_use` | 改需调签名/加 attribute，工作量大 |
| 高价值（已修） | `explicit_iter_loop` / `single_char_add_str` / `redundant_closure` / `unnecessary_unwrap` / `inefficient_to_string` | 可能藏真实代码质量问题，**batch7 已修 25 文件** |

## 2. batch7 已修高价值 pedantic lint（25 文件）

修复类型（跨 12 crate）：
- `explicit_iter_loop`（`iter()` → `&` / `iter_mut()` → `&mut`）：显式引用，更清晰
- `single_char_add_str`（`"x"` → `'x'`）：单字符字符串改 char
- `redundant_closure` / `unnecessary_unwrap` / `inefficient_to_string` 等

纯文档类 lint（`missing_errors_doc` 等）未修——量大且不藏 bug。未改 trait 签名。

## 3. cargo-audit 供应链（batch8 → 本批补完，2026-08-06）

**状态：✅ 已完成**（上批因 github 不稳定 + SOCKS5 代理 `127.0.0.1:1080` 不通、advisory-db fetch 被中断。本批绕过代理直接 clone advisory-db 后用 `cargo audit --no-fetch` 完成真实扫描。）

- 工具：`cargo-audit 0.22.2`
- advisory-db：1190 条 advisory，commit `1237bbe`（2026-08-06 fetch）
- 扫描范围：`Cargo.lock` 全量 **1046 个 crate 依赖**
- 结果：**4 vulnerabilities found + 5 warnings**（exit code 1）
- 完整日志：`/tmp/cargo-audit-finish.log` / JSON：`/tmp/cargo-audit-finish.json`

### 3.1 漏洞（4，无 high/critical；1 个 medium，3 个无 CVSS 的 normal advisory）

| RUSTSEC | crate@版本 | 严重性 | 类型 | 修复版本 | 引入路径（→ workspace crate） |
|---------|-----------|--------|------|---------|------------------------------|
| [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071) | rsa 0.10.0-rc.18 | **medium（CVSS 5.9）** | Marvin Attack（时序侧信道密钥恢复） | **无可用修复** | ssh-key/russh → **os-protocols**（transitive） |
| [RUSTSEC-2025-0142](https://rustsec.org/advisories/RUSTSEC-2025-0142) | mnl 0.2.3 | normal（无 CVSS） | `mnl::cb_run` 段错误/OOB 读（libmnl `nlmsg_len` 整数截断） | `>=0.3.1` | **os-guest**（workspace 直依，`nftnl-ffi` feature 门控，可选） |
| [RUSTSEC-2025-0126](https://rustsec.org/advisories/RUSTSEC-2025-0126) | nftnl 0.7.0 | normal（无 CVSS） | `Batch::with_page_size` 堆溢出 | `>=0.9.0` | **os-guest**（workspace 直依，`nftnl-ffi` feature 门控，可选） |
| [RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235) | rkyv 0.7.46 | normal（无 CVSS） | archive 校验不足致 Rc/Arc OOB 读 | `>=0.8.17` | russh-util → russh → **os-protocols**（transitive） |

### 3.2 警告（5，无严重性；4 unmaintained + 1 unsound）

| RUSTSEC | crate@版本 | 类型 | 修复版本 | 引入路径 |
|---------|-----------|------|---------|---------|
| [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002) | lru 0.12.5 | **unsound** | `>=0.16.3` | transitive（`IterMut` 违反 Stacked Borrows） |
| [RUSTSEC-2024-0388](https://rustsec.org/advisories/RUSTSEC-2024-0388) | derivative 2.2.0 | unmaintained | 无 | transitive（derive 宏，2 处引用） |
| [RUSTSEC-2024-0384](https://rustsec.org/advisories/RUSTSEC-2024-0384) | instant 0.1.13 | unmaintained | 无 | measure_time → tantivy → **os-services** |
| [RUSTSEC-2025-0119](https://rustsec.org/advisories/RUSTSEC-2025-0119) | number_prefix 0.4.0 | unmaintained | 无 | indicatif → tokenizers → **os-services** |
| [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436) | paste 1.0.15 | unmaintained | 无 | alloy-primitives → … → **os-wallet → os-guest**（proc-macro） |

### 3.3 评估与修复建议（仅建议，**不自动升级**——升级需 ADR + 回归）

- **无 high/critical 漏洞**，供应链整体可控。所有漏洞均为内存安全/侧信道类，**非 RCE**。
- **rsa（medium，唯一有 CVSS 的）**：russh 的传递依赖，无上游修复版本（RUSTSEC 标 "No fixed upgrade is available"）。实际风险取决于 RSA 私钥操作是否走该 crate 时序敏感路径；os-protocols 的 russh 用作服务端 SSH，影响面有限。建议跟踪 russh 上游（待其切换到非 rc 的 rsa 或替代签名后端）。
- **mnl / nftnl（normal）**：均为 **os-guest 可选 feature `nftnl-ffi`** 门控（需 `libmnl-dev`/`libnftnl-dev` 才编译，默认不启用）。仅真实 nftables netlink 事务路径受影响。**建议**：workspace 升级 `nftnl = "0.9"`、`mnl = "0.3"`（均有修复版本），但属 FFI 破坏性升级，需单独 ADR + 真实 nftables 测试。
- **rkyv（normal）**：russh-util 传递依赖，`>=0.8.17` 是主版本升级（0.7→0.8 API 不兼容），需等 russh 上游升级，**本仓库不宜直接 pin**。
- **lru（unsound）**：`>=0.16.3` 可修，但属 transitive，需定位直接引入方后随上游升级。
- **unmaintained ×4（derivative/instant/number_prefix/paste）**：均为 transitive 宏/小工具 crate，无安全影响，**纯卫生问题**，跟随上游自然淘汰即可，无需主动处理。

**结论**：供应链干净度可接受。优先级排序：rsa 跟踪上游 > mnl/nftnl 升级（可选 feature，低风险）> 其余 transitive 跟随上游。所有升级需走 ADR + 测试流程，本审计不改依赖。

## 4. 测试覆盖率（batch8 cargo-tarpaulin，2026-08-06）

**整体 src 加权覆盖率 ≈ 79.6%**（18 crate 跑通，cargo-tarpaulin 0.37.0）。

详见 `docs/COVERAGE_REPORT.md`。

关键发现：
- 最高覆盖率：os-provision 93.9% / os-common 93.3% / os-iso 91.5%
- 最低覆盖率（可解释）：osd/systemd_runner 35.7%（需 root+systemd）/ os-network/rtnetlink_real 24.5%（需 CAP_NET_ADMIN）
- **最高 ROI 补测**：os-core 纯 DTO/newtype（types.rs 0% / ids.rs 0%）补冒烟测可拉到 ~85%；各 crate error.rs Display 补 `format!("{e}")` 一行测

## 5. 回归验证（batch8 基线）

- `cargo clippy -- -D warnings`：**0 warning**（默认 lint 级别全绿）。
- `cargo test --workspace --features mock`：**2096 passed + 109 ignored = 2205**（batch7→batch8 零回归）。
- `cargo fmt --all -- --check`：零差异。

## 6. cargo-deny 供应链深化（batch9，2026-08-06）

工具：`cargo-deny 0.20.2`。配置文件：仓库根 `deny.toml`（permissive 许可证白名单 + per-crate exception，与 workspace license `MIT OR Apache-2.0` 对齐）。

### 6.1 总览

| check | 结果 | 说明 |
|-------|------|------|
| advisories | ✅ 已跑通（与 cargo-audit §3 一致） | 清除 git 死代理（`socks5h://127.0.0.1:1080`）后直连 github clone advisory-db 成功。检测到 4 漏洞（rsa/mnl/nftnl/rkyv）+ 5 警告，与 §3 cargo-audit 完全一致。详见 §3 |
| licenses | ✅ ok | 配置 permissive 白名单后全部通过；无 GPL/AGPL；见 6.2 |
| bans | ✅ ok（69 重复版本警告） | 无被 ban crate；69 个 crate 存在多版本共存（非阻断），见 6.3 |
| sources | ✅ ok | 无禁用源（ crates.io ） |

### 6.2 licenses（许可证合规）

**配置策略**（`deny.toml` `[licenses] allow`）：MIT / MIT-0 / Apache-2.0 / Apache-2.0 WITH LLVM-exception / BSD-2-Clause / BSD-3-Clause / ISC / Zlib / BSL-1.0 / CC0-1.0 / Unicode-3.0 / CDLA-Permissive-2.0 / MPL-2.0（weak copyleft，文件级，可接受）。

**许可证分布**（按 crate 数，全工作区 ~1700 crate 实例）：

| 许可证 | 数量 | 类别 |
|--------|------|------|
| MIT | 757 | permissive ✅ |
| Apache-2.0 | 616 | permissive ✅ |
| Unicode-3.0 | 19 | permissive ✅ |
| BSD-3-Clause | 17 | permissive ✅ |
| Unlicense | 13 | permissive（Unlicense OR MIT 双授权，按 MIT 取）✅ |
| Zlib | 12 | permissive ✅ |
| CC0-1.0 | 11 | permissive ✅ |
| ISC | 9 | permissive ✅ |
| MPL-2.0 | 5 | weak copyleft（文件级，可接受）⚠️ |
| BSD-2-Clause | 4 | permissive ✅ |
| MIT-0 | 2 | permissive ✅ |
| LGPL-2.1-or-later | 2 | **copyleft**（仅 `r-efi` 5.3.0/6.0.0，UEFI SDK，linking exception，per-crate exception 已加）⚠️ |
| CDLA-Permissive-2.0 | 2 | permissive ✅（webpki-roots 证书数据） |
| BSD-1-Clause | 2 | permissive ✅（fiat-crypto） |
| MITNFA | 1 | permissive ✅（hex_lit） |
| BSL-1.0 | 1 | permissive ✅（ryu） |

**结论**：
- **无 GPL/AGPL**（强 copyleft）——商业项目安全。
- 唯一 copyleft：`r-efi`（LGPL-2.1-or-later，带 linking exception，UEFI 启动支持）。已加 per-crate exception 并记录待法务复核。`r-efi` 是 `rustls-platform-verifier`（间接由 `reqwest`/`hyper` 链引入）在 Windows/macOS 平台验证的 transitive 依赖，Linux 上不实际链接。
- 13 个 `Unlicense` crate 均为「Unlicense OR MIT」双授权（如 `aho-corasick`/`memchr`/`walkdir`/`jiff`），已在 `deny.toml` exceptions 按 MIT 选项放行。
- MPL-2.0（5 个：`htmlescape`/`slog`/`slog-scope`/`slog-stdlog`/`uluru`）为弱 copyleft（文件级），合规风险低。

### 6.3 bans（禁用 crate / 重复版本）

- **禁用 crate**：**0**（无 `[bans] deny` 命中；无 ring/等常见敏感 crate）。
- **多版本共存**：**69 个 crate** 存在 2+ 版本（`multiple-versions = "warn"`，非阻断）。集中在密码学（RustCrypto 系：`aes`/`cipher`/`digest`/`sha2`/`signature`/`ecdsa` 等，0.x 与 0.y 并存，因不同上游分别锁不同 0.x）、Windows 绑定（`windows-sys`）、`nix`/`rustix`/`getrandom`/`hashbrown`/`syn`/`thiserror`/`reqwest`/`tokenizers` 等。
- **建议**：多版本是 Rust 生态常态，cargo 自动处理，无需手动干预；若需瘦身（减小构建时间/二进制体积），可后续统计每个重复的「上游引入路径」做版本对齐（独立任务，非本批范围）。

### 6.4 advisories（安全公告）

✅ **已跑通**（清除 git 死代理后直连 github）：

根因排查：本环境 git 全局配置了 `http.https://github.com/.proxy socks5h://127.0.0.1:1080`，但跳板机 SSH 隧道不可达（198.51.100.114:179 Connection reset），导致所有 git/cargo 的 github 操作走死代理超时。清除该 git config 后直连 github.com（curl 200）成功 clone advisory-db。

- **cargo deny check advisories** 检测到 4 漏洞（rsa RUSTSEC-2023-0071 medium / mnl RUSTSEC-2025-0142 / nftnl RUSTSEC-2025-0126 / rkyv RUSTSEC-2026-0235），与 §3 cargo-audit 完全一致（同源 RustSec advisory-db，1190 条）。
- **结论**：advisories 与 cargo-audit 交叉验证通过，供应链漏洞清单确定。

### 6.5 sources（来源）

✅ ok：全部 crate 来自 crates.io（默认源），无私有/未授权源。

## 7. cargo-udeps 未用依赖（batch9，2026-08-06）

工具：`cargo-udeps 0.1.61`（已安装）。需 nightly toolchain。

⚠️ **未完成**：nightly toolchain 安装需从 `static.rust-lang.org` 下载（非 github），该域名在本环境不可达（curl 000 超时），与 github 死代理问题独立——清除 git 死代理后 github 通了但 static.rust-lang.org 仍不通。`rustup toolchain install nightly --profile minimal` 超时无响应。

- **后续**：在 static.rust-lang.org 可达的环境执行 `rustup toolchain install nightly --profile minimal` → `cargo +nightly udeps --features mock`，补未用依赖清单到本节。
- **红线**：本任务**只报告不删除**依赖；任何删除需单独 ADR + 回归。

## 8. 本批回归验证

- `cargo build --workspace`：✅ 编译通过（本批仅装工具 + 跑分析 + 更新文档，不改代码，无回归风险）。
- 详见 §5（batch8 基线：2096 passed + 109 ignored）。
