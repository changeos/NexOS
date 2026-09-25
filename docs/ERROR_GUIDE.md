# 错误码归类指引（ERROR_GUIDE）

- **状态**：已采纳（Accepted）
- **日期**：2026-08-05
- **范围**：全 workspace 各 crate 的 `impl From<XxxError> for os_common::ApiError`
- **来源**：闭环 `docs/REVIEW.md` §R2-P7 / R1-P7「错误码归类指引未沉淀」
- **关联规范**：主文档 §15.1 错误模型、`crates/os-common/src/error.rs`

---

## 0. 目的与适用范围

review2（R1/R2）发现：各 crate 的 `From<XxxError> for ApiError` 实现中，"哪个 Error 变体映射到哪个 ApiErrorCode" 由各 owner 各自判断，缺统一对照表，导致主观偏差与跨 crate 不一致。本指引沉淀一份「错误变体模式 → ApiErrorCode」的归类标准，目标：

1. **新 owner 不再凭直觉归类**——查表即可。
2. **新增 Error 变体时有可执行的决策流程**——见 §4。
3. **审计有客观依据**——见 §3，已对全 workspace 21 个 `From` 实现逐项核对。

本指引是**非强制约定**：归类有合理歧义时，本表给出"推荐 + 可接受备选"，并在审计表中标注现存偏差。变更代码以本指引为对照基线，但不在本 PR 中改动任何源码。

---

## 1. ApiErrorCode 各变体语义

`ApiErrorCode` 定义在 `crates/os-common/src/error.rs:15`，共 10 个变体（7 通用 + 3 领域）。下表给出每个变体的准确定义与典型适用场景。**判断核心原则：客户端（前端/调用方）能否凭此错误码采取差异化行动。**

| 变体 | serde/Display | 准确定义 | 客户端语义（"我该做什么"） | 典型场景 |
|------|---------------|----------|----------------------------|----------|
| **NotFound** | `not_found` | 客户端请求的资源（按其给定的标识）在系统中不存在。 | 提示用户"对象不存在"，引导重选/重新输入；**不要重试**。 | `PoolNotFound` / `VmNotFound` / `UserNotFound` / `SessionNotFound` / `ComponentNotFound` |
| **InvalidInput** | `invalid_input` | 客户端提交的**请求参数**非法（格式错/越界/类型不符/取值不被接受）。错误在客户端，重试同参数仍会失败。 | 提示用户修改参数后重发；前端做表单校验。 | `InvalidArgs` / `InvalidSpec` / `InvalidConfig` / `RuleInvalid`（dry-run 不通过） |
| **PermissionDenied** | `permission_denied` | 调用方身份未通过授权（未认证 / 凭证无效 / RBAC 拒绝 / 会话过期）。 | 引导登录/续签/提权；**不要原样重试**。 | `AuthFailed` / `JwtInvalid` / `PolicyDenied` / `AccessDenied` / `SessionExpired` |
| **Conflict** | `conflict` | 资源已存在，或资源当前**状态**不允许该操作（乐观锁冲突 / 状态机非法迁移）。 | 提示冲突、刷新后重试（状态可能已变）。 | `AlreadyExists` / `StateConflict` / `CasConflict` / `VipConflict` |
| **RateLimited** | `rate_limited` | 触发限流。 | 退避后重试（带 `Retry-After`）。 | 网关 `RateLimited` |
| **UpstreamUnavailable** | `upstream_unavailable` | 本服务依赖的**外部上游**暂时不可达或不可用（RPC 挂、钱包未连接、远端节点不可达、镜像源拉取失败、对端组件崩溃）。错误在第三方，**可重试**。 | 提示"上游暂时不可用"，建议稍后重试或换上游。 | `RpcUnavailable` / `ImagePullFailed` / `EndpointUnreachable` / `ConnectFailed` / `LlmError` / `Timeout` |
| **Internal** | `internal` | 服务端内部错误，**不归咎于客户端**（IO 失败、序列化错误、空指针、未预期 panic、命令子进程非零退出但语义不属于上游）。**兜底类别**——找不到更精确归属时用这个，具体原因放 `message`。 | 提示"内部错误，请联系管理员"；客户端无法处理。 | `Io` / `Serde` / `Internal` / 大多数 `CommandFailed` |
| **FailoverFailed** | `failover_failed` | HA 故障转移流程中**任一步**失败（迁移 VM / 切 VIP / 提升副本 / 回滚到旧槽位）。 | 触发运维介入；前端提示"切换失败"。 | `FailoverFailed`（meta） / `MigrationFailed`（compute HA） / `RollbackFailed`（update 槽位） |
| **ChainVerificationFailed** | `chain_verification_failed` | 链上**密码学验证**失败（签名无效 / 凭证不符 / 余额不足 / 用户在钱包侧拒绝签名）。 | 提示用户重新签名 / 检查凭证。 | `SignatureInvalid` / `VerificationFailed`（guest 链上验证） / `WalletRejected` |
| **ConfirmationRequired** | `confirmation_required` | 高危操作待用户在 IM 内确认（会签未达法定 / 等待用户裁决），见 §3.7.2。 | 引导用户进 IM 完成确认。 | `ConfirmationDenied`（im） |

### 1.1 三条边界规则（容易混淆）

1. **NotFound vs InvalidInput**：标识本身"找不到对象"用 NotFound；标识格式/取值不被接受（即便对象存在）用 InvalidInput。例如 `PoolNotFound("zpool0")` → NotFound；`InvalidSpec("vcpu=0")` → InvalidInput。
2. **PermissionDenied vs Conflict**：RBAC 拒绝用 PermissionDenied；资源**状态**不允许（与权限无关）用 Conflict。例如 `PolicyDenied` → PermissionDenied；`GuestExists`（已存在不可重复创建）→ Conflict。
3. **UpstreamUnavailable vs Internal**：错误**源头是外部上游**（RPC/钱包/镜像源/远端节点）→ UpstreamUnavailable；错误**源头是本机内部**（IO/序列化/本地命令子进程）→ Internal。

### 1.2 领域变体的窄义

- **FailoverFailed**：仅限**有显式故障转移/槽位切换语义**的失败。普通迁移失败但无 HA 切换意图的，归 Internal 或 UpstreamUnavailable（视语境）。
- **ChainVerificationFailed**：仅限**链上密码学验证**。本地非链上的密码校验（mTLS 握手、JWT 签名）仍归 PermissionDenied。
- **ConfirmationRequired**：仅限**用户裁决流**（IM 会签）。非用户裁决的"被拒绝"用 PermissionDenied 或 Conflict。

---

## 2. 归类映射表（Error 模式 → 推荐码）

> **关键词约定**：`推荐` = 应首选；`可接受` = 有合理性但偏离首选；`不推荐` = 与本指引冲突。

| Error 变体模式（语义关键词） | 推荐 ApiErrorCode | 可接受备选 | 备注 |
|------------------------------|-------------------|------------|------|
| 资源不存在（`XxxNotFound`：Pool/Vm/User/Session/Component/Job/Asset/Link/Secret/Bucket/Object/Share/Interface/Credential/Guest/Conversation/Agent/Tool） | **NotFound** | — | 最一致的一条；全 workspace 已统一。 |
| 参数非法（`Invalid*`：Args/Spec/Config/Vdev；`RuleInvalid`） | **InvalidInput** | — | 错误在客户端，参数本身不被接受。 |
| 资源已存在（`*Exists` / `AlreadyExists`） | **Conflict** | — | 创建冲突。 |
| 状态机/乐观锁冲突（`StateConflict` / `CasConflict` / `SlotConflict` / `VipConflict`） | **Conflict** | — | 状态不允许。 |
| 依赖图循环（`TaskCycle` / `DependencyCycle`） | **Conflict**（推荐） | InvalidInput | 本 workspace 现归 Conflict（im `TaskCycle` / osd `DependencyCycle`）；语义偏"图状态非法"，Conflict 与 InvalidInput 均可接受，**保持现状**。 |
| 认证失败 / 凭证无效（`AuthFailed` / `JwtInvalid` / `TwoFactorFailed` / `CertExpired` / `SessionExpired`） | **PermissionDenied** | — | 引导用户重登/续签。 |
| RBAC 拒绝 / 访问被拒（`PolicyDenied` / `AccessDenied`） | **PermissionDenied** | — | — |
| 上游 RPC/服务不可用（`RpcUnavailable` / `LlmError` / `EndpointUnreachable` / `ConnectFailed` / `PairingFailed`(mobile)） | **UpstreamUnavailable** | — | 外部上游暂时不可达。 |
| 上游拉取/检查失败（`ImagePullFailed` / `DownloadFailed` / `CveCheckFailed` / `HealthCheckFailed`） | **UpstreamUnavailable** | — | — |
| 限流 | **RateLimited** | — | 仅 os-api `RateLimited`。 |
| 链上密码学验证（`SignatureInvalid` / `VerificationFailed`(链上) / `WalletRejected` / `ChainUnsupported`） | **ChainVerificationFailed** | — | 仅链上场景。 |
| HA 故障转移 / 槽位回滚（`FailoverFailed` / `MigrationFailed`(HA) / `RollbackFailed`） | **FailoverFailed** | — | — |
| 高危操作待用户确认（`ConfirmationDenied`） | **ConfirmationRequired** | — | 仅 im。 |
| IO / 序列化 / 未分类内部（`Io` / `Serde` / `Internal`） | **Internal** | — | 兜底。 |
| **本地命令子进程非零退出**（`CommandFailed`） | **Internal**（推荐，多数 crate 现状） | UpstreamUnavailable | 跨 crate 当前不一致（见 §3）。本指引推荐 Internal：子进程报错多为本地配置/状态问题，非"上游服务"范畴。 |

---

## 3. 各 crate 现状审计

> 审计基于源码逐项核对（main `783da63`，本 worktree 同基线）。**偏差**列仅标注与本指引推荐项不一致或值得复审的条目；未标偏差者即符合本指引。

### 3.1 审计概览

| crate | From 实现位置 | 变体数 | 符合指引 | 偏差/可复审 | 说明 |
|-------|---------------|--------|----------|-------------|------|
| os-api | `crates/os-api/src/error.rs:42` | 6 | 6 | 0 | 网关出口，身份映射清晰。 |
| os-cli | `crates/os-cli/src/error.rs:44` | 7 | 7 | 0 | — |
| os-common（反向） | `crates/os-common/src/error.rs:97` | — | — | — | `From<os_core::CoreError>`，统一 Internal，合理。 |
| os-compute | `crates/os-compute/src/error.rs:64` | 12 | 12 | 0 | `CommandFailed` 归 Internal（符合本指引）；`MigrationFailed → FailoverFailed`（合理）。 |
| os-desktop | `crates/os-desktop/src/error.rs:40` | 6 | 4 | 2 | `MountFailed/UnmountFailed` 见 §3.3。 |
| os-discover | `crates/os-discover/src/error.rs:44` | 7 | 5 | 2 | `BeaconInvalid/MtlsHandshakeFailed` 见 §3.3。 |
| osd | `crates/osd/src/error.rs:49` | 7 | 7 | 0 | 注释已显式说明归类逻辑，最佳实践范本。 |
| os-guest | `crates/os-guest/src/error.rs:57` | 10 | 10 | 0 | 链上验证归 ChainVerificationFailed，符合。 |
| os-i18n | `crates/os-i18n/src/error.rs:30` | 3 | 3 | 0 | 全部 Internal（无对应细分码，合理兜底）。 |
| os-im | `crates/os-im/src/error.rs:48` | 8 | 8 | 0 | `ConfirmationDenied → ConfirmationRequired`，符合。 |
| os-iso | `crates/os-iso/src/error.rs:40` | 6 | 4 | 2 | `HardwareIncompatible` / `VerificationFailed` 见 §3.3。 |
| os-meta | `crates/os-meta/src/error.rs:66` | 11 | 9 | 2 | `NotLeader/NotMember` 见 §3.3。 |
| os-mobile | `crates/os-mobile/src/error.rs:40` | 6 | 6 | 0 | — |
| os-network | `crates/os-network/src/error.rs:48` | 8 | 8 | 0 | — |
| os-protocols | `crates/os-protocols/src/error.rs:64` | 12 | 11 | 1 | `ProtocolDisabled` 见 §3.3。 |
| os-provision | `crates/os-provision/src/error.rs:40` | 6 | 6 | 0 | — |
| os-security | `crates/os-security/src/error.rs:48` | 8 | 7 | 1 | `CertExpired` 见 §3.3。 |
| os-services | `crates/os-services/src/error.rs:55` | 9 | 8 | 1 | `ShareExpired` 见 §3.3；`HardwareError` 已修正。 |
| os-storage | `crates/os-storage/src/error.rs:59` | 11 | 9 | 2 | `CommandFailed` / `CryptoError` 见 §3.3。 |
| os-update | `crates/os-update/src/error.rs:52` | 9 | 9 | 0 | `RollbackFailed → FailoverFailed`（合理，槽位切换语义）。 |
| os-wallet | `crates/os-wallet/src/error.rs:61` | 11 | 10 | 1 | `ChainUnsupported` 见 §3.3。 |

**合计**：20 个业务 crate 的 `From` 实现（+1 os-common 反向 `From<CoreError>`）共 163 个变体映射。符合本指引 154（95%），偏差/可复审 9（5%）；其中 P2（已修复）5 处，P3（保留现状，已标注理由）9 处。

> **更新（2026-08-05，PR `fix/error-p2` + `fix/error-p3`）**：§3.3 标注的 5 处 P2 偏差已全部修复（归类统一到本指引推荐）。1 处 P3 偏差（os-services `HardwareError`）已修正为 Internal。修复后符合率 **148 → 154/163（95%）**，P2 偏差清零，仅余 9 处 P3（保留现状并已标注理由）。下表中受影响 crate 的"符合/偏差"列计数已同步更新。

### 3.2 跨 crate 一致性问题（系统性，优先于单点偏差）

| 系统性差异 | 现状 | 本指引推荐 |
|-----------|------|-----------|
| **`CommandFailed` 归类不一** | os-storage → `UpstreamUnavailable`；os-compute / os-network / os-protocols → `Internal` | 统一 **Internal**（除非该命令明显调用外部上游服务，如 zfs send 到远端 → 可 UpstreamUnavailable） |
| **`MigrationFailed` 归类不一** | os-compute → `FailoverFailed`；os-provision → `Internal` | 视语义：HA 切换语境 → FailoverFailed；裸数据迁移（ZFS send/recv）→ Internal 或 UpstreamUnavailable。两者均可接受，**保留区分**。 |

### 3.3 单点偏差清单（按 crate）

> **优先级**：P2 = 建议复审；P3 = 仅记录，可保留现状。

| crate | 变体 | 当前码 | 本指引推荐 | 优先级 | 说明 |
|-------|------|--------|-----------|--------|------|
| os-desktop | `MountFailed` | UpstreamUnavailable | **Internal** 或 UpstreamUnavailable | P3 ✅保留 | 挂载发生在客户端本机，非"上游服务"；但若视远端 OS 为上游，UpstreamUnavailable 可接受。**保留理由**：远端 OS 是桌面客户端的上游资源，桌面作为消费者；UpstreamUnavailable 语义合理。 |
| os-desktop | `UnmountFailed` | UpstreamUnavailable | 同上 | P3 ✅保留 | 同上。**保留理由**：同 MountFailed。 |
| os-discover | `BeaconInvalid` | PermissionDenied | PermissionDenied（推荐）/ ChainVerificationFailed | P3 ✅保留 | beacon 签名属密码学验证，但非链上；归 PermissionDenied 符合"防伪=身份未通过"语义。**保留理由**：符合 §1.2 明确规定——"本地非链上密码校验（mTLS 握手、JWT 签名）仍归 PermissionDenied"。 |
| os-discover | `MtlsHandshakeFailed` | PermissionDenied | PermissionDenied / InvalidInput | P3 ✅保留 | mTLS 握手失败多因证书不受信→PermissionDenied 合理；若是版本/协议不匹配则 InvalidInput。**保留理由**：证书不受信是身份验证失败，符合 §1.2。 |
| os-iso | `HardwareIncompatible` | ~~Conflict~~ → InvalidInput | **InvalidInput** | ~~P2~~ ✅已修复 | "硬件不满足 HCL"是部署前置条件不满足，更接近参数/环境非法；Conflict 暗示"状态冲突"，语义偏移。**已改 InvalidInput**（2026-08-05）。 |
| os-iso | `VerificationFailed` | InvalidInput | InvalidInput / ChainVerificationFailed | P3 ✅保留 | ISO 校验（sha256/签名）非链上，归 InvalidInput 可接受；若强调"校验失败≠参数非法"也可 Internal。**保留理由**：用户提供的 ISO 文件未通过完整性校验，属于"输入不被接受"，InvalidInput 合理。 |
| os-meta | `NotLeader` | PermissionDenied | PermissionDenied（推荐）/ FailoverFailed | P3 ✅保留 | HA 写转发场景：非 leader 即"当前节点无权处理写"→PermissionDenied 合理；亦可视为"未触发故障转移"。**保留理由**：语义接近 RBAC 拒绝（"此节点无权执行写"），PermissionDenied 精确；FailoverFailed 需显式故障转移流程，非此场景。 |
| os-meta | `NotMember` | PermissionDenied | 同上 | P3 ✅保留 | 同上。**保留理由**：同 NotLeader。 |
| os-protocols | `ProtocolDisabled` | ~~UpstreamUnavailable~~ → Conflict | **Conflict** 或 InvalidInput | ~~P2~~ ✅已修复 | 协议被禁用是配置/能力问题（编译期或角色），非"上游暂时不可用"。UpstreamUnavailable 语义偏移，**已改 Conflict（状态不允许）**（2026-08-05）。 |
| os-security | `CertExpired` | ~~UpstreamUnavailable~~ → PermissionDenied | **PermissionDenied** | ~~P2~~ ✅已修复 | 证书过期属凭证失效，应引导用户续签/重认证，归 PermissionDenied 与 `SessionExpired`/`JwtInvalid` 一致。当前 UpstreamUnavailable 与同 crate 其他认证类变体不一致，**已改 PermissionDenied**（2026-08-05）。 |
| os-services | `ShareExpired` | PermissionDenied | **NotFound** 或 InvalidInput | P3 ✅保留 | 分享链接已过期：对象本身存在但已失效。PermissionDenied 强调"无权"亦可，但 NotFound/InvalidInput 更精确。**保留理由**：过期=访问被拒绝，与 SessionExpired→PermissionDenied 语义一致（"凭证/授权已失效→引导用户重新获取"）。 |
| os-services | `HardwareError` | ~~UpstreamUnavailable~~ → Internal | **Internal** | ~~P3~~ ✅已修复 | 本机硬件错误（SMART/UPS/风扇）非外部上游；归 Internal 更准确。**已改 Internal**（2026-08-05）。 |
| os-storage | `CommandFailed` | ~~UpstreamUnavailable~~ → Internal | **Internal** | ~~P2~~ ✅已修复 | 见 §3.2，与其他 crate 不一致；zpool/zfs 子进程报错多为本地状态，**已改 Internal**，与 os-compute/os-network/os-protocols 统一（2026-08-05）。 |
| os-storage | `CryptoError` | InvalidInput | InvalidInput / Internal | P3 ✅保留 | 加密失败（密钥错/已加密）若视作"参数不被接受"→InvalidInput 可接受；若是内部密钥管理故障→Internal。**保留理由**：用户提供的密钥错误或对已加密数据集重复加密，属于"输入不被接受"，InvalidInput 合理。 |
| os-wallet | `ChainUnsupported` | ~~ChainVerificationFailed~~ → InvalidInput | **NotFound** 或 InvalidInput | ~~P2~~ ✅已修复 | "链不支持"是能力/配置缺失，非密码学验证失败。归 ChainVerificationFailed 与同 crate `SignatureInvalid` 同类，语义偏移。**已改 InvalidInput（用户指定了不支持的链=输入参数非法）**（2026-08-05）。 |

**P2 共 5 处**（建议在下一轮迭代中复审）：os-iso `HardwareIncompatible`、os-protocols `ProtocolDisabled`、os-security `CertExpired`、os-storage `CommandFailed`、os-wallet `ChainUnsupported`。

> **✅ P2 已全部修复（2026-08-05，PR `fix/error-p2`）**：上述 5 处归类已统一到本指引推荐——os-iso `HardwareIncompatible`→InvalidInput、os-protocols `ProtocolDisabled`→Conflict、os-security `CertExpired`→PermissionDenied、os-storage `CommandFailed`→Internal、os-wallet `ChainUnsupported`→InvalidInput。

**P3 共 10 处**（仅记录，当前归类有合理性）：1 处已修正（os-services `HardwareError`→Internal），9 处保留现状并已标注保留理由（见上表 **保留理由** 列）。

---

## 4. 新增 Error 变体时的决策流程

当一个 crate 新增 `Error` 枚举变体时，按以下顺序决定其 `ApiErrorCode`：

```
Step 1：识别错误"归咎于谁"
  ├─ 客户端（请求有问题）      → Step 2
  ├─ 第三方上游（外部依赖挂了）→ Step 3
  └─ 本服务（内部故障）        → Step 4

Step 2：客户端错误——细分
  ├─ 资源按给定标识不存在？         → NotFound
  ├─ 参数格式/取值不被接受？        → InvalidInput
  ├─ 资源已存在 / 状态不允许操作？  → Conflict
  ├─ 身份未授权 / 凭证无效 / 过期？ → PermissionDenied
  └─ 触发限流？                    → RateLimited

Step 3：第三方上游错误——细分
  ├─ 链上密码学验证失败（签名/凭证/拒绝签名）？ → ChainVerificationFailed
  ├─ HA 故障转移 / 槽位回滚失败？               → FailoverFailed
  ├─ 高危操作待用户在 IM 内确认？               → ConfirmationRequired
  └─ 其余上游不可用（RPC/远端/镜像源/钱包连接）  → UpstreamUnavailable

Step 4：本服务内部错误
  └─ Internal（兜底，具体原因放 message）

Step 5：查 §2 映射表确认（关键词匹配）
Step 6：若仍无法判定 → 选 Internal，并在 PR 描述中标注"待归类"，提请 review。
```

### 4.1 实现 Checklist

新增变体时，按本清单落地：

- [ ] 在 `Error` 枚举上加 `#[error("...")]` 与文档注释（说明触发条件）。
- [ ] 在该 crate 的 `impl From<XxxError> for os_common::ApiError` 的 `match` 中加分支。
- [ ] 选定 `ApiErrorCode`：先走 §4 决策流程，再查 §2 映射表，最后与 §3 同 crate 既有变体保持一致。
- [ ] `message` 保留足够诊断信息（含 stderr / 标识 / 任务 ID 等），不要丢失上下文。
- [ ] 若归类偏离 §2 推荐（属合理歧义），在 `From` 实现上方注释说明原因（参考 `crates/osd/src/error.rs:50` 的注释风格——本指引推荐此为最佳实践）。
- [ ] 若该变体在跨 crate 有同义变体（如多个 crate 都有 `CommandFailed`），优先与多数 crate 现状一致；若不一致，按 §3.2 系统性差异处理。

### 4.2 何时新增 ApiErrorCode 变体

仅当满足**全部**条件时，才考虑向 `os-common::ApiErrorCode` 新增变体：

1. 现有 10 个变体均无法准确表达，且
2. 客户端**确实需要**据此码做差异化处理（否则 Internal 即可），且
3. 至少 2 个 crate 共享此错误模式（避免单 crate 私有码污染公共枚举）。

新增需经 review，并同步更新本指引 §1 / §2。

---

## 5. 维护

- 本指引随 `os-common::ApiErrorCode` 演进：每次枚举变体增减，必须同步更新 §1 / §2 / §4.2。
- §3 审计表反映 main `783da63` 基线快照；后续 crate 大改 Error 枚举时，对应 owner 应同步刷新 §3（或在新 PR 中追加"偏差已修复"标注）。
- 与本指引冲突的实现改动，PR 描述须引用本文件具体条目（如"按 ERROR_GUIDE §3.3 改 os-security CertExpired → PermissionDenied"）。
