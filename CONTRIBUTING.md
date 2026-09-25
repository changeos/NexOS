# 贡献指南（CONTRIBUTING）

> 面向新开发者：如何在本仓库搭建环境、写代码、跑测试、提交 PR。
>
> 本项目是 24 crate 的纯 Rust OS 系统。代码规范、提交流程、CI 门控已由
> `docs/agents/_conventions.md` + `scripts/pre-commit.sh` + `.github/workflows/ci.yml`
> 三件套锁定，本文是它们的**新手友好汇总**——更详细的协作约定见上述源文件。
>
> 阅读顺序建议：第 1 节搭好环境 → 第 2~3 节照着写代码和测 → 第 4 节提交 →
> 改架构前看第 5 节 ADR 流程 → 跑真实环境前看第 6 节沙箱。

---

## 1. 开发环境搭建

### 1.1 Rust 工具链

- **版本**：workspace 声明 `rust-version = "1.75"`（见根 `Cargo.toml` `[workspace.package]`，
  这是 async fn in trait 的稳定线）。CI 用 **stable**（`dtolnay/rust-toolchain@stable`）。
  推荐用 [rustup](https://rustup.rs) 装最新的 stable 即可，不必锁定 1.75。
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup default stable
  rustc --version    # 确认 ≥ 1.75
  ```

- **编译验证**（拉到代码后第一件事）：
  ```bash
  cargo build --workspace
  # 带 mock feature（CI 与 pre-commit 默认用的 feature，解锁下游 Mock 注入路径）：
  cargo build --workspace --features mock --all-targets
  ```

### 1.2 可选系统依赖（按需安装）

默认 `cargo build --workspace` **不需要**任何系统库。下列依赖只在启用特定 **feature**
或跑特定 `#[ignore]` 真实测时才需要——按你正在做的 crate 选择性安装：

| 系统包 | 启用方式 / 触发场景 | 说明 |
|--------|--------------------|------|
| `libnftnl-dev` + `libmnl-dev` | `--features nftnl-ffi`（os-network / os-guest / nettest） | nftables 真实 netlink 事务；缺 `-dev` 包时该 feature 编译失败 |
| `libvirt-dev` | `--features virt-ffi`（os-compute） | 真实 libvirt/KVM FFI；缺时 virt 自动 feature 门控回退 |
| `ffmpeg` | os-services FFmpeg 转码（运行时 spawn 外部进程） | 不影响编译，影响真实转码测 |
| `zfsutils-linux` | os-storage ZFS-on-loop 真实测 + nettest `zfs_real_smoke` | 提供 `zfs`/`zpool` 命令；需加载 zfs 内核模块 |
| `xorriso` + `squashfs-tools` | os-iso 真实 ISO 构建（`#[ignore]`） | 产出可启动 ISO；无特权要求 |

一次性装齐（Ubuntu/Debian）：
```bash
sudo apt-get install -y libnftnl-dev libmnl-dev libvirt-dev ffmpeg \
                        zfsutils-linux xorriso squashfs-tools
```

> 装不全也没关系：默认套件（`cargo test --workspace --features mock`）不依赖上述任何包，
> 新开发者可以先跳过，等需要跑真实环境测时再补（见第 6 节）。

### 1.3 推荐工具

```bash
cargo install cargo-watch      # 文件变更自动重编译/重测
cargo install cargo-edit       # cargo add / cargo upgrade 管理依赖
```

### 1.4 一键命令（Makefile）

仓库根 `Makefile` 与 CI 保持一致的本地复现命令，新手直接用 `make`：

| 命令 | 等价 cargo | 说明 |
|------|-----------|------|
| `make check` | `cargo check --workspace --features mock --all-targets` | 类型检查 |
| `make clippy` | `cargo clippy --workspace --all-targets --features mock -- -D warnings` | lint（0 warning） |
| `make fmt` | `cargo fmt --all` | 自动格式化 |
| `make fmt-check` | `cargo fmt --all -- --check` | CI 风格校验（不写盘） |
| `make test` | `cargo test --workspace --features mock` | 默认测试套件 |
| `make all` | check + clippy + test | **三道门全跑**（推 CI 前本地复现） |
| `make install-hooks` | 软链 `scripts/pre-commit.sh` → `.git/hooks/pre-commit` | 装本地 hook |

**开工第一件事**：`make install-hooks`（让 pre-commit 帮你把 CI 红拦截在本地）。

---

## 2. 代码规范

### 2.1 格式化与 lint（两道硬门）

```bash
cargo fmt --all                              # 必须零差异
cargo clippy --workspace --all-targets --features mock -- -D warnings   # 0 warning
```

- **fmt 零差异**：CI 第一道门最便宜（只解析+比对，不编译），最先跑。本地养成随手
  `cargo fmt --all` 的习惯，推 CI 前 `make fmt-check` 自查。
- **clippy `-D warnings`**：任何 warning 都会让 CI 红。不允许本地放行后再推（CI 仍会兜底）。
  这是项目红线（见 `Makefile` 顶部注释）。
- **`--features mock`**：CI、pre-commit、Makefile 全程带这个 feature——它解锁下游 Mock
  注入路径，是默认测试基线。本地复现 CI 行为时务必带上。

> pedantic 级 lint 不强制（历史审计过 3304 条，已修高价值项归档到
> `docs/CODE_QUALITY_AUDIT.md`），新代码尽量顺手清掉常见 pedantic 提示即可。

### 2.2 trait 契约先行（最重要的约定）

> 项目是 24 crate 协作，trait 是跨 crate 契约。**trait 签名稳定 = 全员能并行。**

- **不改 trait 签名**（增删方法、改参数/返回类型）——除非走 §5 ADR + 受影响 agent 会签。
- 新方法优先在 trait 上定义清楚再写实现（契约先行，见 `_conventions.md` §6 可恢复性）。
- 上游 crate 必须为下游提供 Mock（`crates/<crate>/src/mock.rs`，`#[cfg(feature = "mock")]` 守护，
  命名 `Mock<Trait>`），否则下游无法并行开发（见 `_conventions.md` §5 Mock 约定）。

### 2.3 红线（_conventions.md §2 + HANDOVER §11）

🔴 **严禁**（违反会被 ReviewAgent 打回，且可能污染宿主/破坏协作）：
- 改既有 trait 签名（除非走 ADR + 会签）
- 为过编译删 trait 方法
- 虚构未注册的依赖（workspace `[workspace.dependencies]` 是 SSOT，新依赖必须走 ADR，见 §5）
- 在本机直接跑破坏性命令（`zfs` / `ip` / `nft` / cgroup / systemd / bootloader 改宿主状态）
  ——一律用 fixture / 骨架 / 沙箱（见第 6 节）

🟡 **谨慎**（需记 ADR 或评估影响）：
- 引入新 crate（必须记 ADR，见 §5）
- 改 `pub` 命名（破坏性，下游会断编）

---

## 3. 测试规范

### 3.1 默认套件（必须全绿）

```bash
cargo test --workspace --features mock      # 或 make test
```

这是 CI 第四道门，**必须全绿**。新功能提交前本地跑一遍。默认套件设计原则：
**不依赖 root / 系统库 / 外部进程 / 公网**——任何带这些依赖的测必须 `#[ignore]`。

### 3.2 新功能必须配测

每份新功能 PR 必须包含：
- **单元测**：纯函数 / 数据结构 / 状态机，放 `#[cfg(test)] mod tests` 或 `tests/` 目录。
- **mock feature 测**：通过上游 `Mock<Trait>` 注入测试，不依赖真实实现。
- 如该 crate 是上游（有下游依赖它）：必须提供 Mock（否则阻塞下游并行）。

### 3.3 真实环境测（`#[ignore]` + 自动 teardown）

凡是需要 **root / 特定内核设施 / 外部二进制 / 公网** 的测，规则：
- **必须标 `#[ignore]`**——不进默认套件，不污染 `cargo test` 绿。
- **必须自动 teardown**——测自建自毁（如建临时池/临时表，测完删），不留残留。
- 跑法（单测手跑）：
  ```bash
  sudo cargo test -p <crate> --features mock --test <name> -- --ignored --nocapture
  ```
- 详见 `docs/SANDBOX.md` §5 的 `#[ignore]` 测试清单（含每条的权限/设施需求）。

### 3.4 破坏性命令

破坏性命令（改宿主状态的 zfs/ip/nft/cgroup/systemd/bootloader）**禁止直接在宿主跑**——
用 fixture（注入假命令 stdout 的 `CommandRunner`）或沙箱（第 6 节）。这是红线（§2.3）。

---

## 4. 提交流程

### 4.1 pre-commit hook（本地第一道门）

`scripts/pre-commit.sh` 在你 `git commit` 时自动跑：
1. 检测暂存区是否有 `.rs` / `Cargo.toml` / `Cargo.lock` 变更（纯文档变更自动跳过）。
2. 跑 `cargo check --workspace --features mock --all-targets`。
3. 跑 `cargo clippy --workspace --all-targets --features mock -- -D warnings`。
4. 任一失败 → 提交中止。

安装：`make install-hooks`。
紧急跳过：`git commit --no-verify`（**注意：跳过本地 hook 不等于跳过 CI**，CI 仍会跑这两道门）。

### 4.2 CI 三道门（`.github/workflows/ci.yml`）

PR 到 `main` 时触发：

| Job | 内容 | 何时阻塞 PR |
|-----|------|-------------|
| **check-clippy-test** | fmt --check → check → clippy(-D warnings) → test（全带 `--features mock`） | **是**（PR 必须四道全绿） |
| **bench** | criterion 微基准 + 回归门控（strict 15% / loose 30%） | 否（仅手动触发或 main push 跑，不阻塞 PR） |
| **iso-build** | 真实 xorriso + squashfs ISO 构建 `#[ignore]` 测 | 否（仅手动勾选 `workflow_dispatch` 触发） |

> PR 上只跑 `check-clippy-test`。bench 和 iso-build 因为耗时长/需特殊工具，不进 PR 反馈循环。

### 4.3 分支与提交格式

- **分支**：本项目用多 agent worktree 协作，约定分支 `agent/<id>`（长期）或
  `agent/<id>/<task>`（任务子分支）。新开发者可直接开 `feature/<描述>` 分支提 PR。
- **提交格式**：`[<scope>] <描述>`
  - `<scope>` = crate 名（如 `os-storage`）或 `docs` / `ci` / `contributing-guide`。
  - 更完整的 agent 约定格式见 `_conventions.md` §1.2：
    `[<agent_id>] <type>(<scope>): <subject>`（type = feat/fix/test/docs/refactor/chore/mock/adr）。
  - 示例：`[storage] feat(os-storage): 实现 ZfsCliBackend.create_pool`
- **PR 标题**：`[<scope>] <描述>`
- **PR 描述**须含：改了什么/为什么、影响的下游、是否破坏 trait 签名（如是，附 ADR 链接 + 会签状态）。

### 4.4 ReviewAgent 校验项（每 PR）

`_conventions.md` §8 列的核对清单，提 PR 前自查：
- [ ] 契约未破坏（trait 签名未变，除非有 ADR）
- [ ] 跨 crate 类型复用（未重复定义 os-core/os-common 已有类型）
- [ ] 错误码映射完整（Error 实现 `From for ApiError`）
- [ ] mock 已提供（若该 crate 是上游）
- [ ] 测试存在且通过
- [ ] 文档注释完整
- [ ] PROGRESS.md / TASKS.md 已更新（若你认领了 agent 角色）

---

## 5. ADR 流程（架构决策记录）

> 任何影响契约或跨 crate 的决策必须落 ADR，存 `docs/adr/`。
> 现有 8 个范例（`ADR-COMPAT-001~003`、`ADR-DEPS-001~005`）。

### 5.1 何时必须写 ADR

- **引入新依赖**（新 crate）——workspace.dependencies 是 SSOT，未注册的依赖禁止直接用（红线）。
- **改 trait 签名**（增删方法、改参数/返回类型）。
- **新增/删除/重命名 `pub` 项**（struct/enum/type alias）。
- 跨 crate 依赖关系变更 / 章节编号调整。

### 5.2 文件名与编号

- 文件名：`ADR-<类型>-<序号>-<标题>.md`
  - 类型 = `COMPAT`（兼容性/契约）或 `DEPS`（依赖）
  - 序号递增（COMPAT 和 DEPS 各自独立编号）
  - 示例：`ADR-COMPAT-001-async-trait-dyn-compat.md`、`ADR-DEPS-005-clip-backend.md`
- **提 ADR 前先在 `docs/adr/` 占号**（建占位文件），避免冲突。

### 5.3 ADR 模板

```markdown
# ADR-<类型>-<序号>：<标题>

- 状态：proposed | accepted | superseded by ADR-<类型>-<MMM>
- 日期：YYYY-MM-DD
- 提出者：<agent_id / 你的名字>
- 会签：<受影响 agent_id 列表>

## 背景
<为什么需要此决策——当前遇到了什么问题>

## 决策
<做了什么——具体到代码层面>

## 替代方案
<考虑过但放弃的方案 + 原因>

## 影响
<对哪些 crate/agent 产生影响；迁移步骤>
```

### 5.4 会签（破坏性变更）

- PR 含 ADR 且影响 ≥1 个其他 crate → 提交者在 PR 描述 @ 受影响方，附 ADR。
- 受影响方评审：`+1` / `-1`（+理由）/ `?`（需讨论）。
- **全部 +1 方可合并**；任一 `-1` 须先解决分歧。
- 高危（架构性/安全）额外需人类复核。
- 会签 48 小时无响应视为 `?`，可升级仲裁。

---

## 6. 沙箱测试（真实环境）

> 当单元测/mock 测不够，需要"在真内核/真守护上跑得通"的证据时——用沙箱，不污染宿主。
> 完整方案见 `docs/SANDBOX.md`。

### 6.1 三套沙箱（按场景选）

| 方案 | 适合 | 启动开销 | 推荐场景 |
|------|------|----------|----------|
| **A. Docker**（privileged + systemd + cgroup v2） | osd cgroup/NTP、nftables（单 netns）、ZFS-on-loop、libvirt `test:///default` | 秒级 | **首选**：日常本地复现、CI 沙箱 |
| **B. QEMU/KVM 嵌套虚拟化** | 真实 KVM 域、裸机 install、PXE/分区/建池、A/B 槽首启 | 分钟级 | 发版前完整回归、裸机路径 |
| **C. systemd-nspawn** | osd systemd unit 真实拉起、cgroup v2、nftables（独立 netns） | 秒级 | systemd PID1 行为复现（可选） |

优先级：先落方案 A（覆盖 ~80% root 阻塞项），B 补裸机/嵌套虚拟化，C 作轻量替代。

### 6.2 真实环境测怎么跑

`#[ignore]` 测默认不跑，手动单测命令（含 root 写操作的用 `sudo`）：

```bash
# nftables 真实事务（需 libnftnl-dev + libmnl-dev + root）
sudo apt-get install -y libnftnl-dev libmnl-dev
sudo cargo test -p nettest --features nftnl-ffi --test nftnl_real -- --ignored --nocapture

# ZFS 真实全链（需 zfsutils-linux + zfs 内核模块 + root）
sudo cargo test -p os-storage --features mock --test real_zfs_ops -- --ignored --nocapture

# ISO 真实构建（需 xorriso + squashfs-tools，无特权）
cargo test -p os-iso --features mock --test real_xorriso_build -- --ignored --nocapture
```

> 所有真实测都设计为**自动 teardown**（测自建自毁临时池/临时表），跑完不留残留。
> 完整 `#[ignore]` 测试清单 + 每条的权限/设施需求见 `docs/SANDBOX.md` §5。

---

## 7. 多 agent 协作（项目经验提炼，可选阅读）

本项目用多 agent（子代理）并行开发 24 crate，沉淀了两条关键经验，理解它们有助于看懂
仓库的分支结构/文档组织：

### 7.1 单波子代理 ≤ 6（限流）

实际执行发现：子代理并行度上限约 **5–6 个**同时运行，超过会触发上游 AI 限流（rate limit），
导致一批子代理集体卡住或失败（见 `HANDOVER.md` §3 限流教训）。
派新 agent 时**单波不超过 6 个**。

### 7.2 worktree 隔离开发

每个子代理一个 `git worktree`（独立工作树 + 独立分支），互不干扰：
- 分支模型：每 agent 长期分支 `agent/<id>`，任务子分支 `agent/<id>/<task>`，集成分支 `integration`，最终进 `main`。
- 5–6 个 worktree 同时干活，合并时 `git merge --no-ff`，文件不重叠则零冲突。
- 新开发者不需要这套机制（开个 `feature/` 分支即可），但理解它能解释为什么历史 commit
  带有 `[<agent_id>]` 前缀、为什么有 `docs/agents/<id>/PROGRESS.md` 这类文件。

### 7.3 相关文档导航

| 想了解 | 看哪 |
|--------|------|
| 完整协作约定（分支/PR/Mock/可恢复性） | `docs/agents/_conventions.md` |
| 项目现状与全程里程碑 | `docs/HANDOVER.md` |
| 真实环境测沙箱方案 | `docs/SANDBOX.md` |
| 部署全流程 | `docs/DEPLOYMENT.md` |
| 第三方依赖清单与裁决 | `docs/DEPENDENCIES.md` |
| 错误码指引 | `docs/ERROR_GUIDE.md` |
| 性能基线与回归门控 | `docs/PERFORMANCE_BASELINE.md` |
| 覆盖率现状 | `docs/COVERAGE_REPORT.md` |
| 已决策的架构决策 | `docs/adr/`（8 个 ADR） |

---

## 附录：新开发者快速上手清单

1. `rustup default stable`（确认 ≥ 1.75）
2. `git clone` + `cargo build --workspace --features mock --all-targets`（验证能编）
3. `make install-hooks`（装 pre-commit hook）
4. `make all`（跑 check + clippy + test 三道门，确认本地全绿）
5. 开 `feature/<描述>` 分支，写代码 + 测试
6. `make fmt` + `make all`（推前自查）
7. 提交（commit message `[<scope>] <描述>`），push，开 PR
8. 等 CI `check-clippy-test` 绿，等 ReviewAgent +1

遇到改 trait 签名 / 引新依赖 → 先看 §5 ADR 流程。
遇到要跑真内核测 → 先看 §6 沙箱。
