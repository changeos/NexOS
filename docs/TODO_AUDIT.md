# TODO 审计报告：os-services / os-protocols / os-network

> **审计时间**：2026-08-05（feature/todo-audit 分支，基于 main `ed165a9`）。
> **审计范围**：3 个高 TODO crate（`os-services` / `os-protocols` / `os-network`）的源码 TODO。
> **审计目标**：逐条分类标注（[RUNTIME] / [STUB] / [DOC] / [OBSOLETE]），补可实现占位桩，
> 清理已实现残留，不改 trait 签名，不虚构依赖。
> **核实**：`cargo test --workspace --features mock` = **1998 passed + 30 ignored = 2028**
> （基线 2024 → +4 测，新增来自 [STUB] 实现）；`cargo clippy --workspace --all-targets
> --features mock -- -D warnings` 0 warning；`cargo fmt --all -- --check` 零差异。

---

## 0. 分类定义

| 类别 | 含义 | 处置 |
|------|------|------|
| **[RUNTIME]** | 运行时阻塞：需真实环境（root/系统库/外部二进制/模型权重/未注册 HTTP 客户端），逻辑骨架与配置生成已就绪 | **保留**，标注 `[RUNTIME]` 说明阻塞原因 |
| **[STUB]** | 当前是占位桩：本可纯逻辑实现但未做（纯算法/数据结构用了占位） | **补真实实现** + 测试 |
| **[DOC]** | 仅文档说明性 TODO（如「真实协议栈集成在编排器侧 TODO」，实际已接通、此为说明性文字） | 补充文档说明，标注 `[DOC]` |
| **[OBSOLETE]** | 已实现但 TODO 注释未删（依赖/模块已注册接通，注释过时） | **删 TODO** 或改写为准确说明 |

---

## 1. 总览统计

| crate | 审计前 TODO 数 | [RUNTIME] | [STUB]（已补） | [DOC] | [OBSOLETE]（已清） |
|-------|---------------|-----------|----------------|-------|---------------------|
| **os-services** | 36 | 24 | 2（hash_password / delete_by_path） | 6 | 4（lib.rs/media_clip/text_search/devtools git 文档改写） |
| **os-protocols** | 34 | 24（14 smb/nfs 协议栈 + 10 RustFS HTTP） | 0 | 6（webdav/ftp/sftp/ftp_backend/sftp_backend 说明性） | 4（object.rs 模块文档改写） |
| **os-network** | 25 | 25（nftnl-ffi 11 + dpu 9 + rdma 5） | 0 | 1（backend topology dry-run） | 0 |
| **合计** | **95** | **73** | **2** | **13** | **8** |

> 说明：分类数为「TODO 注释归属」口径；同一条 TODO 可能既改写（[OBSOLETE]→准确文档）又保留
> 运行时阻塞说明（如 devtools git 文档）。源码现仍保留约 95 条 `TODO` 字样（含新增的
> `[RUNTIME]`/`[DOC]` 标注），但每条都已分类标注，无未分类残留。

---

## 2. os-services（36 条）

### 2.1 [STUB] 补实现（2 条）

#### S1. `files_model.rs` `hash_password` —— FNV-1a 占位 → SHA-256 拉伸

- **位置**：`crates/os-services/src/files_model.rs:358`（原 `fnon1:` 占位）。
- **问题**：原实现用 FNV-1a 128bit（**非密码学哈希**，不可作密码存储），TODO 注「引入 argon2 后替换」。
- **补实现**：`sha2` 已为本 crate 依赖（ADR-DEPS-003 devtools AEAD 引入），改用
  **SHA-256 + 应用域盐 + 8192 轮 PBKDF2 风格拉伸**（每轮混入上轮摘要 + 域常量 + round-counter）。
  算法前缀 `ssh256:8192:<hex>`，便于将来切 Argon2id 时识别旧哈希。
- **限制说明**：仍弱于 Argon2id（无内存硬度），仅用于**分享链接访问密码**（短生命、低价值）。
  **系统用户登录密码**走 `os-security::password::hash_password`（真实 Argon2id + 随机 salt），两者不共用。
- **测试**：`hash_password_deterministic`（更新断言新前缀）+ 新增 `hash_password_avalanche`（雪崩效应）。

#### S2. `search_index.rs` `SearchIndex::delete_by_path` —— no-op 占位 → 真实 tantivy 删除

- **位置**：`crates/os-services/src/search_index.rs:152`（原 no-op 返回 `Ok(())`）。
- **问题**：原 `path` 字段 schema 为 `STORED`-only（无倒排），无法用 `Term` 精确删除；TODO 注
  「改 schema 或维护 path→doc_id 映射」。tantivy 0.22 已注册（ADR-DEPS-001），可补。
- **补实现**：将 `path` 字段 schema 改为 `TextOptions` + `"raw"` 分词器（精确匹配、不分词）+ STORED，
  使 `delete_term(Term::from_field_text(f_path, path))` 生效。删除需后续 `commit()` 对 `search`
  可见（与 tantivy 增量语义一致）。**不改 trait**（`SearchIndex` 是具体类型，非 trait）。
- **测试**：新增 3 个——`delete_by_path_removes_single_doc_after_commit`（删除 + commit + 验证
  num_docs/search）、`delete_by_path_nonexistent_is_noop`（删不存在 no-op）、
  `delete_by_path_exact_match_only`（精确匹配，不误删前缀/子串同名）。

### 2.2 [RUNTIME] 保留（24 条）

| # | 文件:行 | 内容 | 阻塞原因 |
|---|---------|------|----------|
| 1 | `lib.rs:17` | CLIP 真实推理（candle 骨架已就位） | 需下载 CLIP 模型权重 + 可选 GPU |
| 2-4 | `media_impl.rs:14,194,206` | CLIP 模型嵌入 / 人脸检测 | 需模型权重（隐私相关须评审） |
| 5 | `media_impl.rs:17` | 人脸检测器接入 | 需模型权重 + 安全评审 |
| 6 | `media_impl.rs:185` | 人脸检测（ingest 内） | 同上 |
| 7-9 | `media_ffmpeg.rs:22,68,410` | FFmpeg 真实二进制 / 超时回调 / 并行优化 | 需真实 ffmpeg 子进程 + 资源画像 |
| 10 | `media_search.rs:27` | FFmpeg/CLIP 转码与向量识别 | 需 ffmpeg 二进制 + CLIP 权重 |
| 11-15 | `impl_devtools.rs:12,478,492,655,662,937` | 远端 git clone | 需 gix `blocking-network-client` feature |
| 16-19 | `impl_backup.rs:6,12,141,160,177` | 远程复制 / scrub 查询 / restore 原语 | 需真实 zfs send\|ssh recv（root + zfs 内核模块）|

### 2.3 [DOC] 说明性（6 条）

- `devtools.rs` KVS 模块文档（已改写说明真实 AEAD 已在 `DefaultDevTools` 接通）。
- `devtools.rs` 测试桩 `MemKvs`（`store`/`get`）的 `ENC:` 占位——**测试桩**，非生产路径，已标注 `[STUB-test-only]`。
- `media_clip.rs:9`（trait 侧不留 TODO 的说明性）。

### 2.4 [OBSOLETE] 清理（4 条，改写为准确说明）

- `lib.rs`：原「tantivy files 待接入」→ tantivy 已接入 files（`search_index`），改写。
- `devtools.rs:7`：原「gix 真实集成留 TODO」→ gix 已接通（本地 init/commit/log/branch），改写为「远端 clone 留 TODO [RUNTIME]」。
- `files_model.rs:548`（`text_search`）：原「tantivy 未注册前的占位」→ tantivy 已接通至 `SearchIndex`，改写为「纯函数回退工具」定位。
- `files_model.rs:14`（全文搜索骨架文档）：改写指向 `search_index`。

---

## 3. os-protocols（34 条）

### 3.1 [STUB] 补实现（0 条）

无。本 crate 所有占位桩均依赖未注册依赖（RustFS 客户端 / reqwest / hmac），属 [RUNTIME]。

### 3.2 [RUNTIME] 保留（24 条）

#### SMB/NFS 协议栈（14 条，`orchestrators.rs`）

所有 `TODO(协议栈) [RUNTIME]` 标记（smb.conf 落盘 + reload_smbd / smbcontrol close-share /
smbstatus -p -J 解析 / exportfs -ra / ganesha reload / 写 /etc/exports 等）——**需真实 samba /
nfs-ganesha / exportfs 二进制 + root/CAP_SYS_ADMIN**。配置生成（smb.conf / exports / ganesha.conf
渲染）已是真实纯函数，仅缺协议栈进程编排（红线：不真改 /etc 配置）。

#### RustFS 对象存储（10 条，`object.rs` `RustFsObjectStore`）

`create_bucket` / `delete_bucket` / `list_buckets` / `put_object` / `get_object` /
`delete_object` / `list_objects` / `create_access_key` / `delete_access_key` 共 9 方法 +
模块文档——**需 RustFS 客户端 + reqwest HTTP 栈 + sigv4 HMAC 实跑 + Argon2**（workspace 未在
本 crate 注册 reqwest/hmac）。命名校验（`validate_bucket_name`）与 sigv4 字符串构造已真实纯函数。
**下游测试请用 `MockObjectStore`**（`mock` feature，纯内存）。

### 3.3 [DOC] 说明性（6 条）

- `webdav.rs:7` / `ftp.rs:6` / `sftp.rs:7` —— 真实 dav-server/libunftp/russh **已注册并接通**
  （`DavServerBackend`/`LibunftpBackend`/`RusshSftpBackend`），这些 TODO 说明「端口监听由上层挂载」，
  属文档说明性，已标注 `[DOC]`。
- `ftp_backend.rs:5` / `sftp_backend.rs:5` —— 说明「协议栈真的接通（而非 TODO 骨架）」，正向表述。
- `object.rs:16` —— 模块文档说明 RustFS 客户端未注册。

### 3.4 [OBSOLETE] 清理（4 条，object.rs 模块/结构文档改写）

- `object.rs:16` 模块文档 + `RustFsObjectStore` 结构文档（原「批 2 骨架」→ 补 `[RUNTIME]` 分类 +
  指向 `MockObjectStore`），并在 impl 块顶部加统一 `[RUNTIME]` 横幅注释。

---

## 4. os-network（25 条）

### 4.1 [STUB] 补实现（0 条）

无。本 crate 所有占位桩均依赖硬件/系统库/未注册客户端，属 [RUNTIME]。

### 4.2 [RUNTIME] 保留（25 条）

#### nftnl 真实事务（11 条，`nftnl_real.rs`，`#[cfg(feature = "nftnl-ffi")]` 门控）

- `list_rules`（NFT_MSG_GETRULE + 解析回包）/ `delete_rule`（id→handle→DELRULE 事务）/
  `add_nat`/`delete_nat`（nat 表 + postrouting/prerouting 链）/ 完整 nftnl expr（src/dst IP 匹配）。
- **阻塞原因**：需 root/CAP_NET_ADMIN + 宿主内核 netfilter 子系统 + libnftnl-dev/libmnl-dev。
  FFI 绑定与 `add_rule` 主路径已接通（`send_batch` 真实 netlink 提交）。

#### DPU 硬件卸载（9 条，`dpu.rs` `BlueFieldBackend`）

- `probe_dp_us`（devlink 执行）/ `offload_nvmeof`（SF + NVMe-oF target）/ `offload_ovs`（tc offload）/
  `redfish_power`（Redfish HTTP POST）/ `redfish_firmware_status`（Redfish GET）。
- **阻塞原因**：需真实 DPU（BlueField）硬件 + 厂商工具链（mlnx-sf）+ Redfish 客户端依赖（未注册）。
  devlink 命令构造与输出解析已是真实纯函数，仅缺进程执行与硬件交互。

#### RDMA 硬件探测（5 条，`rdma.rs` `RdmaCoreManager`）

- `probe_devices`（ibv_devinfo 执行）/ `configure_ipoib`（ip addr add 执行）/ 命令构造文档。
- **阻塞原因**：需真实 RDMA 网卡 + ibverbs 用户态库 + 进程执行库（未注册）。
  ibv_devinfo 输出解析与 IPoIB 命令构造已是真实纯函数，缺硬件交互。`skip_probe=true` 时优雅降级。

### 4.3 [DOC] 说明性（1 条）

- `backend.rs:843`（`topology-aware-dry-run`）—— 当前 dry_run 为纯逻辑校验，拓扑感知（管理源地址
  判断）需注入拓扑；属增强型待办，非运行时阻塞，已标注 `[DOC]`。

### 4.4 [OBSOLETE] 清理（0 条）

无。`backend.rs:12` 模块文档（netlink-exec/nftnl-exec）改写为 `[RUNTIME]` 标注（原无类别）。

---

## 5. 补实现清单（[STUB] → 真实）

| # | 位置 | 原 | 新实现 | 测试 |
|---|------|----|--------|------|
| S1 | `files_model.rs:358` `hash_password` | FNV-1a 128bit（非密码学） | SHA-256 + 域盐 + 8192 轮拉伸（`ssh256:` 前缀） | 更新 `hash_password_deterministic` + 新增 `hash_password_avalanche` |
| S2 | `search_index.rs:152` `delete_by_path` | no-op（path 字段无倒排） | path 字段改 `"raw"` 分词索引 + `delete_term` 真实删除 | 新增 3 测（删后不可见 / no-op / 精确匹配） |

**红线遵守**：
- 不改 trait 签名（`hash_password` 签名 `fn(&str)->String` 保持；`delete_by_path` 是 `SearchIndex` 具体类型方法，非 trait）。
- 不虚构依赖（`sha2` 已为本 crate 依赖；tantivy 0.22 已注册）。
- [RUNTIME] 类全部保留，仅标注 `[RUNTIME]` + 阻塞原因。

---

## 6. 保留的运行时阻塞清单（[RUNTIME]，73 条，需特殊环境真实跑）

参见 `docs/HANDOVER.md` §7.1 完整清单。本次审计确认的 73 条 [RUNTIME] 分布：

- **os-services（24）**：FFmpeg 真实二进制 / CLIP 模型权重 / 人脸检测器 / 远端 git clone / zfs 远程复制+scrub+restore。
- **os-protocols（24）**：SMB/NFS 真实协议栈（samba/ganesha/exportfs + root）/ RustFS HTTP（RustFS 客户端 + reqwest 未注册）。
- **os-network（25）**：nftnl 真实事务（root + libnftnl）/ DPU 硬件卸载（BlueField + Redfish）/ RDMA 硬件探测（ibverbs）。

> 这些均**逻辑已就绪**（命令构造 / 配置生成 / 输出解析为真实纯函数），仅缺**真实运行时环境**
> （root/系统库/外部二进制/模型权重/未注册 HTTP 客户端）。建议用 `docs/SANDBOX.md` 的
> Docker/QEMU/nspawn 沙箱 + `#[ignore]` 标记的真实环境测逐项验证。

---

## 7. 核实命令

```bash
cd ~/OS_System/os-wt-todo-audit   # 或主工作树合并后
# 测试数（应 1998 passed + 30 ignored = 2028，较基线 2024 +4）
cargo test --workspace --features mock 2>&1 | grep "test result:" | \
  awk '{p+=$4; i+=$8} END {print p" passed + "i" ignored = "(p+i)}'
# clippy（应 0 warning）
cargo clippy --workspace --all-targets --features mock -- -D warnings 2>&1 | tail -3
# fmt（应零差异）
cargo fmt --all -- --check
# 三 crate TODO 分类标注核验（每条 TODO 应带 [RUNTIME]/[STUB]/[DOC]/[OBSOLETE] 之一）
grep -rn "\[RUNTIME\]\|\[STUB\]\|\[DOC\]\|\[OBSOLETE\]" crates/os-services/src \
  crates/os-protocols/src crates/os-network/src --include="*.rs" | wc -l
```

---

## 8. 后续建议（非本次范围）

1. **[RUNTIME] 真实环境测**：用 SANDBOX 沙箱逐项跑（nftnl-ffi / virt-ffi / 真实 zfs / chrony 等），
   每跑通一条即从 [RUNTIME] 移除并补 `#[ignore]` 测。
2. **`hash_password` 进一步升级**：评估引入 `argon2` 到 os-services（当前用 os-security 的
   Argon2id 是跨 crate 调用，本 crate 独立路径用 SHA-256 拉伸作过渡）。
3. **RustFS 接通**：注册 RustFS 客户端 + reqwest 到 os-protocols（需 ADR），补 `RustFsObjectStore`
   10 方法真实 HTTP 调用，届时移除 [RUNTIME] 标注。
4. **远端 git clone**：评估开启 gix `blocking-network-client` feature（需 ADR，影响默认 feature 集）。
