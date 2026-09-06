# os-iso

> 可安装 ISO 打包 + Rust 安装器 · 标准/克隆双变体 · owner：iso-agent（规划 §3.11/§3.19）

OS 的安装介质 crate：把组件二进制打成可启动 ISO（xorriso + squashfs），并用
Rust 安装器完成硬件兼容性检测（HCL）+ 分区/建池/装系统 + 首启强制重设密码
（§3.19 设计）。契约层 + 骨架实现（真执行留 TODO）。

## 核心能力

- **ISO 构建**（`iso`）：`IsoBuilder` trait + `IsoSpec` / `IsoVariant`
  （**标准** / **克隆**两种变体，构建期含组件二进制）；默认实现
  `XorrisoIsoBuilder` 编排 xorriso + mksquashfs 产出 ISO。
- **敏感信息过滤**（`iso`）：`filter_sensitive` / `is_sensitive_key` +
  `SENSITIVE_CONFIG_KEYS`——克隆变体打包时剔除密钥/凭证类配置键。
- **Rust 安装器**（`installer`）：`Installer` trait——`HardwareReport` /
  `DiskInfo` / `InstallTarget` / `InstallStep` / `InstallReport` HCL 检测与安装
  数据模型；`hcl_warnings` / `detect_kvm_support_from_cpuinfo` 纯函数；
  默认实现 `RustInstaller`（真写盘留 TODO）。
- **命令构造纯函数**（`cli` / `install_cmds`）：xorriso / mksquashfs /
  sha256sum 命令行构造，无工具链也可单测。
- **环境探测**（`env`）：探测 xorriso / mksquashfs 存在性，测试据此决定是否
  跳过真实构建；`runner` 提供命令执行抽象。

## 架构位置

**依赖**（上游）：`os-core`、`os-common`（`From<IsoError> for ApiError`）；
构建期依赖系统 `xorriso` / `mksquashfs`。

**被用**（下游）：os-integration（dev）；发布流水线 / 安装介质构建脚本消费。

## 独立使用

- **仓库外引用**：`os-iso = { git = "http://ub2604:8080/git/nexos.git" }`。
- **契约规范**：trait 保持原生 async（单实现为主，不能 `Box<dyn IsoBuilder>`，
  ADR-COMPAT-001），下游以具体类型/泛型注入；自定义 `IsoError`。
- **关键接口**：`IsoBuilder` / `Installer` 两 trait + `IsoVariant` 变体选择 +
  `env` 工具链探测。
- **feature**：`mock`（默认关）——`MockIsoBuilder` / `MockInstaller`
  （供下游 update-agent / api-agent 测试注入）。

## 测试

```bash
cargo test -p os-iso
```

纯函数单测（命令构造/敏感键过滤/HCL 判定）默认跑；真实 xorriso 构建测在
`tests/*real*.rs` 中以 `#[ignore]` 标记，需系统工具链手动执行。
