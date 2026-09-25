# 协作约定（Conventions）

> 所有 owner agent 共用此约定。开工前必读。违反约定的 PR 会被 ReviewAgent 打回。

---

## 1. Git 工作流

### 1.1 分支模型
- **每 agent 一个长期分支**：`agent/<agent_id>`（如 `agent/storage`）。这是该 agent 的主线，长期存在。
- **每任务一个子分支**：`agent/<agent_id>/<task-slug>`（如 `agent/storage/create-pool`）。任务完成后合并回 `agent/<agent_id>`。
- **集成分支**：`integration`（可选，用于跨 agent 集成测）；最终进 `main`。

### 1.2 提交规范
格式：`[<agent_id>] <type>(<scope>): <subject>`

- **type**：`feat`/`fix`/`test`/`docs`/`refactor`/`chore`/`mock`/`adr`
- **scope**：crate 名或模块（如 `os-storage`/`backend`）
- **subject**：祈使句，≤50 字

示例：
```
[storage] feat(os-storage): 实现 ZfsCliBackend.create_pool
[storage] test(os-storage): create_pool 单元测（loop 设备临时池）
[storage] mock(os-storage): 提供 MockStorageBackend 供下游
[storage] adr: ADR-007 BlockExport 归属调整（会签 network）
```

### 1.3 PR 规范
- **标题**：`[<agent_id>] <task-slug>`
- **描述**须含：
  - 改了什么、为什么
  - DoD 勾选状态（引用规格书 §5.2）
  - 是否破坏 trait 签名（如是，附 ADR 链接 + 会签状态）
  - 影响的下游 agent（如有）
- **目标分支**：`agent/<agent_id>`（单 agent）；`integration`（跨 agent 集成）

---

## 2. ADR（架构决策记录）

> 任何影响契约或跨 agent 的决策须落 ADR。存放 `OS_System/docs/adr/`。

### 2.1 何时需要 ADR
- 修改 trait 签名（增删方法、改参数类型、改返回类型）
- 新增/删除/重命名 pub 项（struct/enum/type alias）
- 跨 crate 的依赖关系变更
- 引入新的第三方 crate
- 章节编号/SSOT 总表调整

### 2.2 ADR 格式
文件名：`ADR-NNN-<slug>.md`（NNN 递增，如 `ADR-007-block-export-relocation.md`）

```markdown
# ADR-NNN：<标题>

- 状态：proposed | accepted | superseded by ADR-MMM
- 日期：YYYY-MM-DD
- 提出者：<agent_id>
- 会签：<受影响 agent_id 列表>

## 背景
<为什么需要此决策>

## 决策
<做了什么>

## 替代方案
<考虑过但放弃的方案 + 原因>

## 影响
<对哪些 crate/agent 产生影响；迁移步骤>
```

### 2.3 ADR 编号
- 已用编号见 `docs/adr/README.md` 索引
- 提 ADR 前先占号（在索引占位），避免冲突

---

## 3. 会签（Multi-agent Sign-off）

> 破坏性变更需受影响 agent 会签；呼应 §3.7.2 确认与投票原语、§13.4 评审链。

### 3.1 会签触发条件
- PR 含 ADR 且影响 ≥1 个其他 agent 的 crate
- 修改 trait 签名
- 跨 agent 集成变更

### 3.2 会签流程
1. 提交者在 PR 描述 @ 受影响 agent，附 ADR
2. 受影响 agent 评审：`+1`（同意）/ `-1`（反对 + 理由）/ `?`（需讨论）
3. **全部 +1 方可合并**；任一 `-1` 须先解决分歧
4. 高危（架构性/安全）额外需 **人类复核**（主代理升级）

### 3.3 会签超时
- 会签请求 48 小时无响应，视为 `?`；提交者 ping 主代理仲裁
- 主代理可强制合并（记录仲裁理由）或回退

---

## 4. 进度日志格式

### 4.1 PROGRESS.md（每 agent 一份）
路径：`OS_System/docs/agents/<agent_id>/PROGRESS.md`

```markdown
# <agent_id> 进度日志

## 当前状态
- 阶段：<启动中 | 实现中 | 测试中 | 待评审 | 阻塞 | 完成>
- 最后更新：YYYY-MM-DD HH:MM

## 已完成
- [x] <task>（commit: <sha>，PR #N）

## 进行中
- [ ] <task>（分支 agent/<id>/<task>，进度 <N>%）

## 阻塞
- ⛔ <task>：阻塞原因；等待 <agent_id> 交付 <X>；升级主代理：<是/否>

## 下一步
1. <task>
2. <task>
```

### 4.2 TASKS.md（每 agent 一份）
路径：`OS_System/docs/agents/<agent_id>/TASKS.md`

```markdown
# <agent_id> 任务队列

## 待办（由主代理分派）
- [ ] T-001：<task>（优先级 P0，依赖 <X>）
- [ ] T-002：<task>（P1）

## 进行中
- [~] T-001：<task>

## 完成
- [x] T-000：<task>（完成于 YYYY-MM-DD，PR #N）

## 阻塞
- [!] T-003：<task>（阻塞于 <X>）
```

任务 ID 格式 `<agent_id>-T-NNN`（如 `storage-T-001`），全局唯一。

---

## 5. Mock 约定

> 上游 agent 须为下游提供 mock，否则下游无法并行。见规格书 §4。

### 5.1 Mock 文件
- 路径：`crates/<crate>/src/mock.rs`
- feature gate：`[features] mock = []`；`#[cfg(feature = "mock")]` 守护
- 命名：`Mock<Trait>`（如 `MockStorageBackend`）

### 5.2 Mock 行为
- 实现完整 trait（不 panic 的默认返回）
- 提供构造器设置预期返回值（如 `MockStorageBackend::new().with_pool(pool)`）
- 不依赖外部状态（纯内存），下游测试可确定性运行

### 5.3 Mock 交付时点
- **批 0/1 agent**：trait 实现前先交付 mock（解锁下游并行）
- **下游**：上游 mock 就绪后切换；之前用本地临时 stub

---

## 6. 上下文管理（防会话重启丢失）

> 多会话 agent 的会话可能随时重启（上下文丢失）。约定确保可恢复。

### 6.1 可恢复性要求
每个 agent 必须能在**仅读取以下 4 类文件**的情况下恢复工作：
1. 自己的规格书（`docs/agents/<agent_id>.md`）
2. 本约定（`_conventions.md`）
3. 自己的进度/任务（`docs/agents/<agent_id>/{PROGRESS,TASKS}.md`）
4. 拥有的契约（`crates/<crate>/src/*.rs`）+ 相关 ADR

不得依赖"会话内记忆"——任何重要决策须落 ADR，任何进度须落 PROGRESS.md。

### 6.2 禁止
- 把关键信息只留在会话内（必须落文件）
- 改了代码但不更新 PROGRESS.md（重启后无法知道改了什么）
- 跨 agent 口头约定（必须落 ADR + 会签）

---

## 7. 主代理（OrchestratorAgent）职责

> 主代理（人类操作的调度会话）任 OrchestratorAgent，**不写 crate 代码**，只调度。

- 分派任务（写 TASKS.md）
- 仲裁会签冲突 / 超时
- 升级人类决策（架构性、安全、范围变更）
- 集成跨 agent 端到端测（或委托 integration-agent）
- 维护 `docs/adr/README.md` 索引与本规划文档 SSOT

**红线**：主代理不兼任 owner agent 写代码（避免既是裁判又运动员）。

---

## 8. ReviewAgent 职责

> 可由主代理兼任或独立 agent。评审所有 PR。

每 PR 校验：
- [ ] 契约未破坏（trait 签名未变，除非有 ADR）
- [ ] 跨 crate 类型复用（未重复定义 os-core/os-common 类型）
- [ ] 错误码映射完整（Error 实现 `From for ApiError`）
- [ ] mock 已提供（若该 agent 是上游）
- [ ] 测试存在且通过
- [ ] 文档注释完整
- [ ] PROGRESS.md/TASKS.md 已更新
