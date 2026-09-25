# `container-agent` 规格书

> 显示名：`Container Agent`
> 拥有 crate：`os-compute`（部分 trait）
> 启动批次：`2`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `container-agent` |
| 显示名 | Container Agent |
| 拥有的 crate | os-compute（`ContainerRuntime`/`ContainerNetwork`/`PackageManager` 三 trait） |
| Git 长期分支 | `agent/container-agent` |
| 上游依赖 agent | `core-agent`（`ContainerId`/`VolumeId`）、`storage-agent`（zvol/bind 挂载源）、`network-agent`（`IpCidr`/`Protocol`） |
| 下游被依赖 agent | `api-agent`（容器/镜像/网络/包管理路由）、`guest-agent`（应用沙箱，软） |
| 启动批次 | `2`，同批可与 vm-agent / object-agent / protocol-agent / wallet-agent / meta-agent / iso-agent 并行（与 vm-agent 共享 os-compute crate 但独占 container.rs/container_net.rs/pkg.rs，须协调分支冲突） |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供容器与第三方包管理能力——基于 youki（OCI runtime）的容器生命周期、CNI 容器网络、apt/dpkg 第三方包管理。

**边界**：
- ✅ 做：实现 `os-compute` 的 `ContainerRuntime`（youki：create/start/stop/remove/get/list + pull_image/list_images/remove_image）、`ContainerNetwork`（CNI：create/delete/connect/disconnect/list）、`PackageManager`（install/uninstall/upgrade/list_installed/search）；为下游提供 mock。
- ❌ 不做：不实现虚拟机（`VmManager` 归 vm-agent，同 crate 不同文件，不得改动）；不修改 trait 签名（须走 ADR）；不直接管理 zvol 创建（归 storage-agent，仅消费 `VolumeId` 挂载源）；不管理底层网络桥/VLAN（归 network-agent）；不实现官方 apt 源本身（仅编排 apt/dpkg 命令）；不耦合 youki 私有内部 API（一律经 OCI runtime 接口）。

## 3. 拥有的契约

> 本 agent 从原 `compute-agent` 拆分而来（VM 与容器领域差异大）。拥有以下三 trait，位于 `os-compute` crate（与 vm-agent 共享 crate 但独占 container.rs/container_net.rs/pkg.rs）。

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-compute | `ContainerRuntime` | `crates/os-compute/src/container.rs` | P1（批 2 核心能力） |
| os-compute | `ContainerNetwork` | `crates/os-compute/src/container_net.rs` | P1（youki 短板补齐） |
| os-compute | `PackageManager` | `crates/os-compute/src/pkg.rs` | P2（第三方应用） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `ContainerState`（Created/Running/Stopped/Paused） | `os-compute/src/container.rs` | 容器运行状态（Paused = cgroup freezer） |
| `ContainerSpec`（image/command/env/mounts/ports/network） | `os-compute/src/container.rs` | 创建时声明的规格 |
| `ContainerMount`、`MountSource`（Bind{path}/Volume{volume_id}） | `os-compute/src/container.rs` | 挂载源（绑定路径或 zvol 块卷） |
| `PortMapping`（host_port/container_port/protocol） | `os-compute/src/container.rs` | 端口映射（复用 `os_network::Protocol`） |
| `Container`、`ImageInfo`（digest/name/size/pulled_at） | `os-compute/src/container.rs` | 容器实例与本地镜像信息 |
| `NetworkDriver`（Bridge/Host/None）、`NetworkInfo`（name/subnet/driver/count） | `os-compute/src/container_net.rs` | CNI 网络驱动与信息（subnet 用 `IpCidr`） |
| `PackageId`、`PackageInfo`、`PackageSource`（ThirdParty/Official） | `os-compute/src/pkg.rs` | deb 包标识/信息/来源（第三方带图标应用归 ThirdParty） |
| `ComputeError`/`ComputeResult` | `os-compute/src/error.rs` | 共享错误枚举（`ContainerNotFound`/`ImagePullFailed`/`NetworkNotFound`/`PackageNotFound`/`InstallFailed`/`InvalidSpec`/`CommandFailed` 归本 agent 维护；VM 相关 variant 归 vm-agent，**不得改动**） |

**关键实现**：
- `YoukiRuntime`：`impl ContainerRuntime`，基于 youki（OCI runtime）；`pull_image` 走 oci-distribution（从 registry 拉取，返回 sha256 digest）；卷挂载支持 `MountSource::Bind`（绑定路径）或 `MountSource::Volume`（zvol 块卷 via VolumeId）。
- `CniContainerNetwork`：`impl ContainerNetwork`，编排 CNI 插件（veth + bridge）；`create_network` 创建 Linux bridge，`connect` 创建 veth 对挂到 bridge；用 rtnetlink/nftnl 配置。
- `DpkgPackageManager`：`impl PackageManager`，编排 `dpkg -i`/`apt-get install`/`apt-get remove`/`apt-get upgrade`；第三方带图标应用归 `PackageSource::ThirdParty`（"未知来源"），区别于官方源。
- `MockContainerRuntime`/`MockContainerNetwork`/`MockPackageManager`：feature `mock`，内存态模拟，返回确定性值，供下游测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `StorageBackend`（zvol 卷） | `os-storage` | `storage-agent` | `crates/os-storage/src/mock.rs`（上游提供） | `MountSource::Volume` 的 zvol 块卷 |
| `NetworkManager`（桥接） | `os-network` | `network-agent` | `crates/os-network/src/mock.rs`（上游提供） | CNI bridge 底层依赖 |
| `IpCidr`、`Protocol`（数据类型） | `os-network` | `network-agent` | —（newtype/enum，无 mock） | `NetworkInfo.subnet`、`PortMapping.protocol` |
| `ContainerId`/`VolumeId` | `os-core` | `core-agent` | —（newtype，无 mock） | 领域 ID |
| `ApiError`/`ApiErrorCode` | `os-common` | core-agent 间接 | — | 错误码映射 |

**mock 策略**：`IpCidr`/`Protocol`/`ContainerId`/`VolumeId` 是纯类型，上游 `cargo check` 通过即可消费；storage/network mock 就绪前用本地临时 stub 跑通；无真实 youki/CNI 环境时用各 Mock 覆盖逻辑分支。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `YoukiRuntime`、`CniContainerNetwork`、`DpkgPackageManager`，不挂 agent 前缀。
- **错误**：实现方法返回 `ComputeResult<T>`；映射 `ContainerNotFound(String)`/`ImagePullFailed(String)`/`NetworkNotFound(String)`/`PackageNotFound(String)`/`InstallFailed(String)`/`InvalidSpec(String)`/`CommandFailed`/`Io`；不新增/改动其他归 vm-agent 的 variant。
- **测试**：每个公开方法有单元测试；oci-distribution 拉取与 CNI veth/bridge 编排用 cfg gate 或 mock registry/CNI 隔离；各 Mock 覆盖返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；youki/CNI/apt 编排补 `//` 内联注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `ContainerRuntime`/`ContainerNetwork`/`PackageManager` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-compute` 通过（与 vm-agent 同 crate，须分支不冲突）
- [ ] `cargo test -p os-compute` 通过
- [ ] `cargo clippy -p os-compute -- -D warnings` 无警告
- [ ] 为下游提供 `MockContainerRuntime`/`MockContainerNetwork`/`MockPackageManager`（`crates/os-compute/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `storage-agent` 交付 `StorageBackend` mock | **软依赖** | zvol 卷挂载源；可先用临时块设备路径并行 |
| `network-agent` 交付 `IpCidr`/`Protocol` 类型稳定 | **硬阻塞** | `NetworkInfo.subnet`/`PortMapping.protocol` 参数类型依赖 |
| `network-agent` 交付 `NetworkManager` mock | **软依赖** | CNI bridge 底层；可先用 rtnetlink 直连并行 |
| `vm-agent` 编辑同 crate 分支协调 | **协调依赖** | 共享 `os-compute` crate，两 agent 分支可能冲突 mock.rs/lib.rs/error.rs；约定：lib.rs/mock.rs/error.rs 改动走 PR 互评 + 子分支命名带前缀 |
| root 权限（youki/CNI/rtnetlink/nftnl/dpkg） | **运行时硬阻塞** | 容器/网络/包操作需 root；测试在沙箱（rootless 容器或 mock） |

**可立即启动的部分**：`Container`/`ContainerSpec`/`ImageInfo`/`NetworkInfo`/`PackageInfo` 等数据结构已存在；oci-distribution 客户端封装（不依赖真实 registry）；各 Mock 内存态实现（纯函数）。

## 7. 并行性分析

- **可并行实现的 trait**：`ContainerRuntime`、`ContainerNetwork`、`PackageManager` 三者领域独立，可三子任务并行。
- **有内部顺序的 trait**：`ContainerRuntime` 的 `start_container` 依赖 `create_container`，`remove_container` 须先 `stop_container`；`ContainerNetwork.connect` 依赖容器已 `create_container` 且网络已 `create_network`。
- **瓶颈点**：youki 集成是串行关键路径（OCI runtime 接口正确性）；`pull_image` 的 oci-distribution 认证与镜像层下载性能。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-compute` 通过 |
| 测试 | `cargo test -p os-compute` 通过；关键路径（容器生命周期、镜像拉取、CNI veth/bridge、dpkg 编排、mock 返回）覆盖率 ≥ 75% |
| 契约 | 未修改 trait 签名（除非有 ADR）；未改动 `ComputeError` 归 vm-agent 的 variant；`cargo doc` 无警告 |
| mock | `MockContainerRuntime`/`MockContainerNetwork`/`MockPackageManager` 已提交（下游可用） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 来源分类 | 第三方带图标应用正确归 `PackageSource::ThirdParty`（"未知来源"） |

## 9. 风险红线

🔴 **严禁**：
- 修改 vm-agent 拥有的 trait（`VmManager`；改动须经 ADR + 会签 vm-agent）
- 修改 `ComputeError` 中归 vm-agent 的 variant（仅可维护 `ContainerNotFound`/`ImagePullFailed`/`NetworkNotFound`/`PackageNotFound`/`InstallFailed`）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（youki/oci-distribution/rtnetlink/nftnl 须在 workspace 已注册）
- 耦合 youki 私有内部 API（必须经 OCI runtime 接口）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **同 crate 分支冲突**：与 vm-agent 共享 `os-compute`，lib.rs/mock.rs/error.rs 改动须互评；建议各自独立 impl 文件（`impl_container.rs`/`impl_container_net.rs`/`impl_pkg.rs`）减少冲突
- 改 CNI 插件实现（rtnetlink/nftnl ↔ 系统 CNI 二进制，架构性变更，须 ADR）
- 引入新第三方 crate（如 OCI 库）须经 ReviewAgent 评估维护性/安全
- 第三方 .deb 来源标注（安全相关，须 ReviewAgent 评审）

## 10. 示例工作流

> 以"实现 `ContainerRuntime.create_container`"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-compute/src/container.rs`（`ContainerRuntime` trait + `ContainerSpec`/`ContainerMount`/`MountSource`）+ `crates/os-compute/src/error.rs`（`ComputeError::ContainerNotFound`/`ImagePullFailed`/`InvalidSpec`）+ 相关 ADR
3. **切分支**：`git checkout agent/container-agent`；建子分支 `agent/container-agent/create-container`
4. **实现**：新建 `impl_container.rs`，定义 `YoukiRuntime`，`impl ContainerRuntime for YoukiRuntime`；`create_container` 校验 spec（image 非空、mount 路径合法），生成 OCI spec.json（mounts 按 MountSource 转 bind/volume、ports 映射、env 注入），调 youki create（不启动）；失败映射 `CommandFailed`/`InvalidSpec`。
5. **测试**：单元测（OCI spec 生成正确性、MountSource 转换）；mock youki 调用；`cargo test -p os-compute`
6. **提 PR**：推到远程，PR 标题 `[container-agent] create-container`，描述含 DoD 勾选状态 + 同 crate 协调备注（CC vm-agent）+ 所需权限（root）
7. **响应评审**：按 ReviewAgent 意见修订；契约/错误枚举变更触发 ADR + 会签 vm-agent
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Container Agent（agent_id: container-agent）。
你的规格书在 OS_System/docs/agents/container-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-compute/src/container.rs、container_net.rs、pkg.rs（三 trait 归你；vm.rs 归 vm-agent，不得改动）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付三个 Mock 解锁下游">

开工前必读：
1. OS_System/docs/agents/container-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/container-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/container-agent/TASKS.md（你的任务队列）
5. 你拥有的 trait：crates/os-compute/src/container.rs、container_net.rs、pkg.rs、error.rs（仅 ContainerNotFound/ImagePullFailed/NetworkNotFound/PackageNotFound/InstallFailed/InvalidSpec 等容器/网络/包相关 variant）
6. 相关 ADR（OS_System/docs/adr/）

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）；不得改动 ComputeError 归 vm-agent 的 variant。
特殊注意：与 vm-agent 共享 os-compute crate，分支改动须互评；youki/CNI/dpkg 操作需 root；第三方带图标应用归 PackageSource::ThirdParty（"未知来源"）。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/container-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/container-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/container-agent/TASKS.md`（下一个任务）
5. `git log agent/container-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-compute`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`ContainerRuntime`/`ContainerNetwork`/`PackageManager` 三 trait），从 `git log` 推断进度，重建 PROGRESS.md。优先确认三个 Mock 是否已交付（下游 api-agent 依赖，未交付则阻塞下游并行）。
