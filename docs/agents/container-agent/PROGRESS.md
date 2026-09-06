# container-agent 进度日志

## 当前状态
- 阶段：实现中（批 2 骨架完成，待评审）
- 最后更新：2026-08-05

## 已完成
- [x] 批 2 骨架：容器/包/网络数据模型 + OCI config 生成 + CNI conflist 生成 + .desktop 解析 + apt 命令构造 + 三个 Mock（commit: 待提交）
  - `crates/os-compute/src/oci.rs`：`ContainerSpec → OciSpec`（OCI runtime config.json 最小子集），含 10 测
  - `crates/os-compute/src/cni.rs`：CNI 1.0.0 `.conflist`（bridge/portmap/firewall 插件链）纯构造，含 12 测
  - `crates/os-compute/src/desktop.rs`：freedesktop `.desktop` entry 解析（locale 折叠/多段容忍），含 10 测
  - `crates/os-compute/src/apt.rs`：apt/dpkg 命令 argv 构造 + `.deb` 文件名/dpkg-query 解析（不真执行），含 13 测
  - `crates/os-compute/src/container.rs`：`ContainerSpec::validate()`/构造器 + 生命周期状态机 `can_transition`/`validate_transition`
  - `crates/os-compute/src/pkg.rs`：`PackageState` 状态机 + `PackageInfo` 构造器（含 `is_third_party_app` 归类）
  - `crates/os-compute/src/mock.rs`：`MockContainerRuntime`/`MockContainerNetwork`/`MockPackageManager`（feature `mock`），含 10 测
  - `Cargo.toml`：加 `mock` feature + dev-deps 自引用
  - 共 59 测全过；`cargo clippy --features mock --all-targets -- -D warnings` 0 警告

## 进行中
- [ ] 等待批 3 引入 youki/oci-distribution/CNI/rtnetlink 后实现 `YoukiRuntime`/`CniContainerNetwork`/`DpkgPackageManager`

## 阻塞
- ⛔ `YoukiRuntime` 实现：阻塞于 youki/oci-distribution crate 未在 workspace 注册（批 3 决策）
- ⛔ `CniContainerNetwork` 实现：阻塞于 libcni/rtnetlink/nftnl crate 未注册
- ⛔ `DpkgPackageManager` 实现：可立即推进（apt 是 CLI，tokio::process 已在）—— 下批任务

## 下一步
1. 批 3：实现 `DpkgPackageManager`（apt CLI 编排，可立即做）
2. 批 3：youki/CNI 集成（待 crate 注册 ADR）
3. 容器/网络/包 trait 的集成测（待实现层就绪）

## 协作备注
- 与 vm-agent 共享 os-compute crate；本批只动 container.rs/pkg.rs/lib.rs/Cargo.toml + 新增独立文件，**未碰 vm.rs**
- Mock 放独立 `mock.rs`（只含容器/网络/包 Mock，不含 `MockVmManager`），避免与 vm-agent 分支冲突
- 三 trait 签名零改动；`ComputeError` 未新增 variant
