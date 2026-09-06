# 子代理集群（AI Agent Swarm）

> 本目录是 OS 系统 **AI agent 集群开发**的规划与规格中心。17 个 owner agent 各自独立会话、经 Git 协作，实现 `OS_System/crates/` 下的 22 个 crate。
>
> 呼应主规划文档 §13（开发期工程方法论）与 §16（子代理集群规划索引）。本目录是 §13.1"组件→Agent 映射"的可执行细化。

---

## 1. 编排模型

- **多会话 agent + Git 协作**：每 agent 独立 AI 会话（独立上下文窗口），经 Git 分支 + PR 协作。
- **主代理 = OrchestratorAgent**（人类操作的调度会话）：分派任务、仲裁会签、升级人类决策。**不写 crate 代码**。
- **17 个 owner agent**（执行方）：各自拥有一组 crate 的实现权。
- **ReviewAgent**（评审方，主代理兼任或独立）：PR 评审、契约校验。

详见 `_conventions.md`。

## 2. agent 清单（27 owner + 4 辅助 = 31）

> **智能拆分原则**：高内聚低耦合。领域差异大的组件拆为独立 agent（如 service 七组件、compute 的 VM/容器、protocol 的文件/对象、network 的基础/RDMA、provision 的三阶段）；内聚的不拆（storage 围绕 ZFS、im 围绕协作、meta 围绕共识）。拆分增加并行度，但每个 agent 仍保有足够内聚以减少协作开销。

### 2.1 owner agent（27 个，按启动批次）

| 批次 | agent_id | 显示名 | 拥有 crate/trait | 依赖前置 | 拆分说明 |
|------|----------|--------|-----------------|---------|---------|
| **0** | `core-agent` | Core Agent | os-core, os-common | 无 | — |
| **0** | `i18n-agent` | i18n Agent | os-i18n | core | — |
| **0** | `orchestrator-agent` | Orchestrator Agent | osd | core | — |
| **1** | `storage-agent` | Storage Agent | os-storage（4 trait） | core | — |
| **1** | `network-agent` | Network Agent | os-network 基础 5 trait（NetworkManager/Firewall/Dhcp/Dns/Pxe） | core | ↓ 从原 network 拆出 RDMA/DPU |
| **1** | `rdma-agent` | RDMA Agent | os-network 的 RdmaManager/DpuBackend | core, network | 新（从 network 拆：高速互联/卸载，可选独立） |
| **1** | `security-agent` | Security Agent | os-security（5 trait） | core, network | — |
| **2** | `protocol-agent` | Protocol Agent | os-protocols 文件协议（FileProtocol+5 子） | storage | ↓ 从原 protocol 拆出 ObjectStore |
| **2** | `object-agent` | Object Store Agent | os-protocols 的 ObjectStore（S3/RustFS） | storage | 新（从 protocol 拆：对象模型与文件协议不同） |
| **2** | `vm-agent` | VM Agent | os-compute 的 VmManager | storage, network | 新（从 compute 拆：VM 与容器领域不同） |
| **2** | `container-agent` | Container Agent | os-compute 的 ContainerRuntime/ContainerNetwork/PackageManager | storage, network | 新（从 compute 拆） |
| **2** | `wallet-agent` | Wallet Agent | os-wallet（3 trait） | core, security | — |
| **2** | `meta-agent` | Meta Agent | os-meta（5 trait） | core, network | — |
| **2** | `iso-agent` | ISO Agent | os-iso | core | 新（从 provision 拆：打包是构建期，独立于运行时迁移/更新） |
| **3** | `discover-agent` | Discover Agent | os-discover | core, meta | — |
| **3** | `guest-agent` | Guest Agent | os-guest（5 trait） | network, security, wallet, im(软) | — |
| **3** | `provision-agent` | Provision Agent | os-provision（PXE 自举+迁移） | network, discover, meta, storage | ↓ 从原 provision 拆出 iso/update |
| **3** | `update-agent` | Update Agent | os-update（OTA/回滚/CVE/滚动） | iso, meta | 新（从 provision 拆：OTA 是运行时升级，独立阶段） |
| **3** | `backup-agent` | Backup Agent | os-services 的 BackupManager | storage | 新（从 service 拆） |
| **3** | `monitor-agent` | Monitor Agent | os-services 的 Monitor | core | 新（从 service 拆） |
| **3** | `media-agent` | Media Agent | os-services 的 MediaManager | storage | 新（从 service 拆：转码/识别重，独立） |
| **3** | `files-agent` | Files Agent | os-services 的 FileManager | storage | 新（从 service 拆） |
| **3** | `devtools-agent` | DevTools Agent | os-services 的 DevTools | core | 新（从 service 拆） |
| **3** | `power-agent` | Power Agent | os-services 的 PowerManager | core | 新（从 service 拆：UPS/硬件监控） |
| **4** | `im-agent` | IM Agent | os-im（7 trait，多 agent 协作中枢） | 全体（最后接入） | — |
| **4** | `api-agent` | API Agent | os-api, os-cli | 全体（网关聚合） | — |
| **4** | `client-agent` | Client Agent | os-mobile, os-desktop | api, discover | — |

### 2.2 辅助 agent（4 个，非 owner，横切支持）

| agent_id | 显示名 | 职责 | 启动时机 |
|----------|--------|------|---------|
| `review-agent` | Review Agent | PR 评审、契约一致性校验、安全审查、会签仲裁。独立于主代理，减轻主代理负担。 | 批 0 即可启动（首批 PR 就需评审） |
| `integration-agent` | Integration Agent | 跨 crate 端到端集成测试（如 guest→wallet→security 链路）、依赖图健康检查、集成分支维护。im-agent 专注协作不兼集成。 | 批 2 起（多 crate 交互时） |
| `devops-agent` | DevOps Agent | CI/CD pipeline、构建脚本、发布打包、环境管理、`cargo check --workspace` 守护。让 owner agent 专注业务代码。 | 批 0 即可启动（CI 是首批需求） |
| `docs-agent` | Docs Agent | API 文档（cargo doc）、用户手册、ADR 索引维护、规格书修订、术语表。 | 批 1 起 |

> **主代理 = OrchestratorAgent**（人类操作的调度会话）：分派任务、仲裁会签冲突、升级人类决策、维护本 SSOT。**不写 crate 代码，不兼任 owner**。可兼任 review/integration/devops/docs 在初期，但规模上来后建议独立。

> **批次 = 可启动顺序**：同批可并行；批 N 的 agent 须等批 N-1 的相关 agent 交付 mock 后启动（硬阻塞）或可先用 stub 并行（软依赖）。批次经智能拆分后并行度显著提升（批 3 从 4→10 个 agent 并行）。

## 3. 启动指南

### 3.1 启动前检查
- [ ] `OS_System/` workspace 存在，22 crate 契约已定义（§15）
- [ ] Rust 工具链可用：`cargo check --workspace` 通过（契约层）
- [ ] Git 仓库已初始化，`main` 分支保护
- [ ] 本目录（`docs/agents/`）与 `_conventions.md` 就绪

### 3.2 启动顺序（分批，智能拆分后并行度提升）

**批 0（最先，3 owner + 2 辅助）**：core / i18n / orchestrator + devops / review
- owner 无上游依赖，立即启动；这批是全员基础，必须最先
- devops（CI 守护）+ review（首批 PR 评审）辅助可同步启动
- 交付 mock 解锁批 1

**批 1（4 owner，core mock 后）**：storage / network(基础5) / rdma / security
- 全依赖 core；rdma 依赖 network 基础（同批内顺序：network 先或并行+软依赖）
- security 依赖 network 的 IpCidr
- 交付 mock 解锁批 2

**批 2（7 owner，前批 mock 后）**：protocol(文件) / object / vm / container / wallet / meta / iso
- protocol/object 依赖 storage；vm/container 依赖 storage+network；wallet 依赖 security；meta 依赖 network；iso 依赖 core
- 高度并行批；integration 辅助可于此批启动（多 crate 交互）

**批 3（10 owner，前批就绪后）**：discover / guest / provision / update / backup / monitor / media / files / devtools / power
- 智能拆分后并行度最高的批次（原 4 → 10）
- service 七组件全拆，各自独立并行；provision/update 分离
- docs 辅助可于此批启动

**批 4（3 owner，最后）**：im / api / client
- im 依赖全体（最后接入 agent 调度）；api 依赖全体（网关聚合）；client 依赖 api+discover
- 集成收尾

### 3.3 启动单个 agent
1. 复制该 agent 规格书 §11 的启动 prompt
2. 填入具体任务（或"按 TASKS.md 取下一个任务"）
3. 在新 AI 会话粘贴启动
4. agent 读规格书 + 约定 + 进度 + 契约后开工
5. 主代理在 `docs/agents/<agent_id>/TASKS.md` 分派任务

## 4. 文件索引

| 文件 | 说明 |
|------|------|
| `_template.md` | 规格书统一模板（12 章节） |
| `_conventions.md` | 协作约定（Git/PR/ADR/会签/进度/mock） |
| `<agent_id>.md` ×31 | 各 agent 规格书（27 owner + 4 辅助） |
| `<agent_id>/PROGRESS.md` | 各 agent 进度日志（运行时生成） |
| `<agent_id>/TASKS.md` | 各 agent 任务队列（主代理分派） |
| `../adr/` | 架构决策记录（运行时生成） |

## 5. 关键原则

1. **接口先行**（§13.2）：trait 契约已定（§15），agent 实现 trait，不经 agent 间口头约定
2. **mock 解锁并行**（§13.2）：上游先交付 mock，下游即可并行，不必等真实实现
3. **可恢复**（`_conventions.md` §6）：任何 agent 会话重启后能仅凭文件恢复
4. **主代理不写码**（`_conventions.md` §7）：调度与实现分离，避免裁判兼运动员
5. **ADR 治变更**（`_conventions.md` §2）：破坏性变更必须 ADR + 会签，禁止擅改契约
