# `media-agent` 规格书

> 显示名：`Media Agent`
> 拥有 crate：`os-services`（部分 trait）
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `media-agent` |
| 显示名 | Media Agent |
| 拥有的 crate | os-services（仅 `MediaManager` 一 trait） |
| Git 长期分支 | `agent/media-agent` |
| 上游依赖 agent | `core-agent`（`TaskId`/`DateTime`/`PageRequest`/`PageResponse`）、`storage-agent`（媒体文件存储） |
| 下游被依赖 agent | `api-agent`（相册/转码/流媒体管理路由）、`client-agent`（移动端相册，软） |
| 启动批次 | `3`，同批可与 backup/monitor/files/devtools/power/discover/guest/provision/update 并行（与其他五个 service-agent 共享 os-services crate 但独占 media.rs，须协调分支冲突） |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供媒体（相册/视频/音频）管理能力——入库（元数据提取 + CLIP 向量 + 人脸检测）、检索（全文 + 向量语义混合搜索）、HLS 转码流媒体。参考 Immich。

**边界**：
- ✅ 做：实现 `os-services` 的 `MediaManager`（ingest/search/transcode/stream_playlist/list_albums）；为下游提供 mock。
- ❌ 不做：不实现备份/监控/文件/开发工具/电源（归其他五个 service-agent，同 crate 不同文件，不得改动）；不修改 trait 签名（须走 ADR）；不直接管理底层文件系统（归 storage/files，仅消费文件路径）；不实现视频编辑（仅转码与流媒体）；不耦合特定 AI 模型私有 API（经 CLIP 抽象）。

## 3. 拥有的契约

> 本 agent 从原 `service-agent` 拆分而来（§2.1 拆分理由：转码/识别重，独立）。仅拥有以下 trait，位于 `os-services` crate（与其他五个 service-agent 共享 crate 但独占 media.rs）。

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-services | `MediaManager` | `crates/os-services/src/media.rs` | P1（批 3 核心能力） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum，定义在 `media.rs`）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `BBox`（x/y/w/h） | `os-services/src/media.rs` | 边界框（人脸/物体矩形区域） |
| `FaceTag`（name/bbox） | `os-services/src/media.rs` | 人脸标签（name = None 未命名聚类） |
| `MediaAsset`（id/path/mime_type/size_bytes/width/height/taken_at/faces/clip_embedding） | `os-services/src/media.rs` | 媒体资源（clip_embedding = None 未计算） |
| `Album`（id/name/asset_count） | `os-services/src/media.rs` | 相册 |
| `TranscodeProfile`（Hls1080p/Hls720p/Hls480p/Original） | `os-services/src/media.rs` | HLS 转码档位 |
| `ServiceError`/`ServiceResult` | `os-services/src/error.rs` | 共享错误枚举（`AssetNotFound` 归本 agent 维护；其他 variant 由各 service-agent 维护，**不得改动**） |

**关键实现**：
- `DefaultMediaManager`：`impl MediaManager`，基于 FFmpeg + CLIP 模型 + 向量库 + tantivy；`ingest` 扫描文件、提取元数据（尺寸/拍摄时间/MIME）、计算 CLIP 向量嵌入、检测人脸（FaceTag）；`search` 全文 + 向量（语义）混合搜索（tantivy 索引），分页返回；`transcode` 触发 FFmpeg HLS 转码（返回 `TaskId`）；`stream_playlist` 返回 HLS m3u8 url（未转码则触发即时转码）；`list_albums` 列出相册。参考 Immich。
- `MockMediaManager`：feature `mock`，内存态维护 asset/album，返回确定性值，供下游测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `StorageBackend`（文件存储） | `os-storage` | `storage-agent` | `crates/os-storage/src/mock.rs`（上游提供） | 媒体文件路径承载 |
| `TaskId`/`DateTime`/`PageRequest`/`PageResponse` | `os-core` | `core-agent` | —（newtype/数据结构，无 mock） | 任务追踪/时间戳/分页 |
| `ApiError`/`ApiErrorCode` | `os-common` | core-agent 间接 | — | 错误码映射 |

**mock 策略**：领域类型是纯结构，core-agent `cargo check` 通过即可消费；storage-agent mock 就绪前用本地临时文件路径跑通；无真实 FFmpeg/CLIP 模型时用 `MockMediaManager` 覆盖逻辑分支，模型推理为加分项。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `DefaultMediaManager`，不挂 agent 前缀。
- **错误**：实现方法返回 `Result<T, ServiceError>`；映射 `AssetNotFound(String)`/`Io`/`Internal`；不新增/改动其他归各 service-agent 的 variant。
- **测试**：每个公开方法有单元测试；元数据提取（EXIF 解析）、混合搜索排序、转码档位选择有专门测；`MockMediaManager` 覆盖返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；FFmpeg/CLIP/向量检索编排补 `//` 内联注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `MediaManager` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-services` 通过（与其他 service-agent 同 crate，须分支不冲突）
- [ ] `cargo test -p os-services` 通过
- [ ] `cargo clippy -p os-services -- -D warnings` 无警告
- [ ] 为下游提供 `MockMediaManager`（`crates/os-services/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `storage-agent` 交付 `StorageBackend` mock | **软依赖** | 文件路径承载；可先用临时文件并行 |
| `core-agent` 交付 os-core 类型可用 | **软依赖** | core 已是契约层 |
| FFmpeg 二进制 + CLIP 模型（运行时） | **运行时硬阻塞** | 转码/向量计算需 FFmpeg + 模型；CI 用 mock 覆盖，真实推理为加分项 |
| 其他 service-agent 编辑同 crate 分支协调 | **协调依赖** | 共享 `os-services` crate；约定：lib.rs/mock.rs/error.rs 改动走 PR 互评 + 子分支命名带前缀 |

**可立即启动的部分**：`MediaAsset`/`FaceTag`/`Album` 等数据结构已存在；EXIF 元数据解析（纯函数）；混合搜索排序算法（纯函数）；`MockMediaManager` 内存态实现。

## 7. 并行性分析

- **可并行实现的 trait**：仅一个 trait；方法内部分三组可并行：入库管线（ingest：元数据/CLIP/人脸）、检索（search）、转码流媒体（transcode/stream）。
- **有内部顺序的 trait**：`transcode`/`stream_playlist` 依赖 asset 已 `ingest`；`stream_playlist` 未转码时触发即时转码。
- **瓶颈点**：CLIP 向量计算与人脸检测是 CPU/GPU 密集型串行关键路径；FFmpeg HLS 转码性能（大视频）。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-services` 通过 |
| 测试 | `cargo test -p os-services` 通过；关键路径（元数据提取、混合搜索排序、转码档位、mock 返回）覆盖率 ≥ 75% |
| 契约 | 未修改 trait 签名（除非有 ADR）；未改动 `ServiceError` 归其他 service-agent 的 variant；`cargo doc` 无警告 |
| mock | `MockMediaManager` 已提交（下游可用） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 service-agent 拥有的 trait（`BackupManager`/`Monitor`/`FileManager`/`DevTools`/`PowerManager`；改动须经 ADR + 会签）
- 修改 `ServiceError` 中归其他 service-agent 的 variant（仅可维护 `AssetNotFound`）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（FFmpeg 绑定/CLIP runtime/tantivy 须在 workspace 已注册）
- 耦合特定 AI 模型私有 API（必须经 CLIP 抽象，避免锁定）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **同 crate 分支冲突**：与其他五个 service-agent 共享 `os-services`，lib.rs/mock.rs/error.rs 改动须互评；建议各自独立 impl 文件（`impl_media.rs`）减少冲突
- 改向量库后端（tantivy ↔ 其他，架构性变更，须 ADR）
- 改 CLIP 模型版本（影响已索引向量兼容性，须 ADR + 重建索引）
- 引入新第三方 crate（如 AI 推理库）须经 ReviewAgent 评估维护性/安全
- 人脸识别隐私相关（须 ReviewAgent + 安全评审）

## 10. 示例工作流

> 以"实现 `MediaManager.ingest`（入库：元数据 + CLIP + 人脸）"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-services/src/media.rs`（`MediaManager` trait + `MediaAsset`/`FaceTag`/`BBox`）+ `crates/os-services/src/error.rs`（`ServiceError::AssetNotFound`/`Internal`）+ 相关 ADR
3. **切分支**：`git checkout agent/media-agent`；建子分支 `agent/media-agent/ingest`
4. **实现**：新建 `impl_media.rs`，定义 `DefaultMediaManager`，`impl MediaManager for DefaultMediaManager`；`ingest` 读文件 → 提取 MIME/size → 解析 EXIF（width/height/taken_at）→ 计算 CLIP 向量嵌入 → 检测人脸（FaceTag + BBox）→ 持久化 MediaAsset → 索引到 tantivy + 向量库；返回 MediaAsset。
5. **测试**：单元测（EXIF 解析正确性、CLIP embedding 维度、人脸检测 bbox 合法性）；mock 模型推理；`cargo test -p os-services`
6. **提 PR**：推到远程，PR 标题 `[media-agent] ingest`，描述含 DoD 勾选状态 + 同 crate 协调备注（CC 其他 service-agent）+ 所需运行时（FFmpeg/CLIP）
7. **响应评审**：按 ReviewAgent 意见修订；隐私相关（人脸）须安全评审
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Media Agent（agent_id: media-agent）。
你的规格书在 OS_System/docs/agents/media-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-services/src/media.rs（仅 MediaManager trait 归你；backup.rs/monitor.rs/files.rs/devtools.rs/power.rs 归其他 service-agent，不得改动）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockMediaManager 解锁下游">

开工前必读：
1. OS_System/docs/agents/media-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/media-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/media-agent/TASKS.md（你的任务队列）
5. 你拥有的 trait：crates/os-services/src/media.rs、error.rs（仅 AssetNotFound variant）
6. 相关 ADR（OS_System/docs/adr/）

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）；不得改动 ServiceError 归其他 service-agent 的 variant。
特殊注意：与其他五个 service-agent 共享 os-services crate，分支改动须互评；FFmpeg/CLIP 运行时依赖；人脸识别隐私相关须安全评审；参考 Immich。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/media-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/media-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/media-agent/TASKS.md`（下一个任务）
5. `git log agent/media-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-services`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`MediaManager` 一 trait），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 `MockMediaManager` 是否已交付（下游 api-agent/client-agent 依赖，未交付则阻塞下游并行）。
