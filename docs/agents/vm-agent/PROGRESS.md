# vm-agent 进度日志

## 当前状态
- 阶段：实现中（批 2 真实 libvirt 接通，门控）
- 最后更新：2026-08-05

## 已完成
- [x] VM 数据模型校验 + libvirt XML 渲染 + 生命周期状态机（vm.rs + vm/tests.rs，26 测）
- [x] LibvirtVmManager 骨架（默认编译路径：内存态，9 方法签名完整，6 测）
- [x] MockVmManager（mock_vm.rs，feature `mock`，与 container-agent 物理隔离，6 测）
- [x] lib.rs / Cargo.toml 注册模块 + mock feature
- [x] **接通 virt 真实 KVM（feature 门控）**（commit 见下）：
  - `Cargo.toml` 加 `virt = { workspace = true, optional = true }` + feature `virt-ffi = ["dep:virt"]`
  - `impl_vm.rs` 重构为**双互斥路径**（`#[cfg(not(feature="virt-ffi"))]` 内存骨架 / `#[cfg(feature="virt-ffi")]` 真实 virt）
  - 真实路径：`virConnectOpen`（惰性缓存）+ `virDomainDefineXML/Create/Shutdown/Destroy/Suspend/Resume/Undefine/GetState/LookupByUUIDString` + `virConnectListAllDomains` + `virDomainMigrateToURI`（active-passive：PEER2PEER|LIVE|UNDEFINE_SOURCE）
  - `LibvirtDomainState` 增 `from_raw(u32)`，原始 `virDomainState` → `VmState` 映射覆盖 0..=7 全枚举
  - virt-ffi 路径测试用 `test:///default` 驱动 fixture（无 KVM/root 即可），无 libvirt-dev 时优雅跳过
  - FFI 前置依赖注明：`apt install libvirt-dev`（本机无；`cargo check --features virt-ffi` 通过，`build/test` 链接失败属预期）
- [x] DoD：cargo check / test（99 测全过）/ clippy（`-D warnings` 干净，默认 + virt-ffi 两路径）/ doc（两路径无警告）

## 进行中
- 无

## 阻塞
- ⛔ virt-ffi 路径真实运行测：本机无 libvirt-dev，`cargo test --features virt-ffi` 链接失败（undefined symbol: virConnectOpen）；测试代码已就绪并优雅跳过，需在装 libvirt-dev 的环境验证。
- ⛔ 运行期 root/libvirt 组权限：真实 KVM 操作需提权环境。

## 下一步
1. 在装 libvirt-dev + libvirtd 的环境运行 `cargo test -p os-compute --features virt-ffi`，确认 `test:///default` 生命周期/列表测真实通过。
2. 跨 agent 集成测（与 storage-agent 的 zvol、network-agent 的桥接）。
3. 迁移 active-passive 编排细节（共享存储一致性检查 + 迁移进度回调）。

## 文件清单（本 agent 拥有）
- `crates/os-compute/src/vm.rs`（数据模型 + 校验 + 状态机 + XML 渲染 + trait）
- `crates/os-compute/src/vm/tests.rs`（vm 单元测）
- `crates/os-compute/src/impl_vm.rs`（LibvirtVmManager 双路径实现 + LibvirtDomainState 映射）
- `crates/os-compute/src/mock_vm.rs`（MockVmManager，feature `mock`）
- `crates/os-compute/Cargo.toml`（feature `virt-ffi` + optional virt dep；feature `mock`）

## 协作备注
- 与 container-agent 共享 os-compute crate：本 agent 仅触碰 vm.rs / vm/ / impl_vm.rs / mock_vm.rs / Cargo.toml 的 vm 相关 feature；container.rs/container_net.rs/pkg.rs/error.rs 未改动。
- mock 文件命名 `mock_vm.rs`（非 `mock.rs`），物理隔离，避免与 container-agent 的容器 mock 冲突。
- Cargo.toml 新增 feature `virt-ffi`（独立 feature，不影响 container-agent 的 mock feature）。
