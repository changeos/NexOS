//! CPU 硬件虚拟化能力检测（KVM 前置检查）
//!
//! 背景：用户点"启动 VM"后，若 CPU 不支持虚拟化 / BIOS 未开 VT-x / KVM 模块未加载，
//! libvirt 会返回晦涩错误（"unsupported configuration" / "internal error" 等），
//! 用户无从定位根因。本模块在启动 VM 前做前置检测，把原始信号翻译成用户能懂的
//! 诊断信息（如"请在 BIOS 中开启 VT-x"）。
//!
//! 设计要点：
//! - **诊断信息必须用户友好**——这是本功能的核心价值（不应是"vmx flag not found"）。
//! - **纯逻辑（解析/诊断生成）与真实 I/O（读文件）分离**——前者可全覆盖单测，
//!   不依赖真实系统状态；后者只在 `#[ignore]` 真实测中跑。
//! - 不触碰 `VmManager` trait（只加新模块 + 新函数 + 新错误变体）。

use std::fs;
use std::path::Path;

use os_core::{Deserialize, Serialize};

use crate::error::ComputeResult;
use crate::ComputeError;

// ----------------------------------------------------------------------------
// 数据结构（纯数据，可序列化、可单测）
// ----------------------------------------------------------------------------

/// CPU 厂商
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CpuVendor {
    /// Intel（flags 中含 vmx，对应 VT-x）
    Intel,
    /// AMD（flags 中含 svm，对应 AMD-V）
    Amd,
    /// 其他厂商（保留原始 vendor_id 字符串，便于排错）
    Unknown(String),
}

/// 嵌套虚拟化状态
///
/// 嵌套虚拟化指"在虚拟机内再开虚拟机"的能力（KVM 内核模块的 `nested` 参数）。
/// 检测依赖 `/sys/module/kvm_intel|kvm_amd/parameters/nested`，若文件不存在
/// （如 KVM 模块未加载）则归为 `Unknown`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NestedVirtStatus {
    /// 已检测，true=启用 / false=禁用
    Supported(bool),
    /// 无法检测（参数文件不存在）
    Unknown,
}

/// 虚拟化能力检测结果（每项都是纯数据，检测逻辑分离在 `detect_virt_capability`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtCheckResult {
    /// CPU 是否有 vmx(Inttel) 或 svm(AMD) 标志位
    pub cpu_has_virt_flags: bool,
    /// CPU 厂商（Intel/AMD/其他）
    pub cpu_vendor: CpuVendor,
    /// /dev/kvm 是否存在（KVM 模块加载后由 udev 创建）
    pub kvm_device_present: bool,
    /// kvm/kvm_intel/kvm_amd 模块是否加载（读 /proc/modules）
    pub kvm_module_loaded: bool,
    /// 嵌套虚拟化是否启用（若可检测）
    pub nested_virt: NestedVirtStatus,
}

impl VirtCheckResult {
    /// 全部 OK 才可运行 KVM 虚拟机。
    ///
    /// 判定条件：CPU 有虚拟化标志位 且 /dev/kvm 存在（KVM 模块已加载的强信号）。
    /// `kvm_module_loaded` 仅作辅助信息——`/dev/kvm` 存在即可证明 KVM 可用，
    /// 避免在某些容器/无 `/proc/modules` 环境下误判。
    pub fn is_usable(&self) -> bool {
        self.cpu_has_virt_flags && self.kvm_device_present
    }

    /// 生成用户友好的中文诊断字符串（针对每种失败给出具体建议）。
    ///
    /// 优先级：CPU 硬件支持 > KVM 内核模块 > 模块加载状态 > 全 OK。
    pub fn to_user_diagnostic(&self) -> String {
        let vendor_str = match &self.cpu_vendor {
            CpuVendor::Intel => "Intel",
            CpuVendor::Amd => "AMD",
            CpuVendor::Unknown(v) => v,
        };

        // 1. CPU 不支持硬件虚拟化（最根本的硬件限制）
        if !self.cpu_has_virt_flags {
            return format!(
                "你的 CPU（{vendor}）不支持硬件虚拟化（无 vmx/svm 标志位），无法运行 KVM 虚拟机",
                vendor = vendor_str
            );
        }

        // 2. CPU 支持，但 KVM 设备不存在（模块未加载 或 BIOS 未开 VT-x）
        if !self.kvm_device_present {
            let modprobe_hint = match self.cpu_vendor {
                CpuVendor::Intel => "`sudo modprobe kvm kvm_intel`",
                CpuVendor::Amd => "`sudo modprobe kvm kvm_amd`",
                CpuVendor::Unknown(_) => {
                    "`sudo modprobe kvm kvm_intel`（Intel）或 `sudo modprobe kvm kvm_amd`（AMD）"
                }
            };
            return format!(
                "CPU 支持虚拟化，但 KVM 内核模块未加载。请在 BIOS 中确认已开启 VT-x/AMD-V，然后执行 {modprobe}",
                modprobe = modprobe_hint
            );
        }

        // 3. /dev/kvm 存在但 /proc/modules 未列出 kvm（罕见——如在无 /proc/modules 的容器里）
        if !self.kvm_module_loaded {
            return format!(
                "/dev/kvm 存在，但 /proc/modules 未检测到 kvm 模块（可能在容器/沙箱环境）。{vendor} 虚拟化可能仍可用，建议以 /dev/kvm 是否可访问为准。",
                vendor = vendor_str
            );
        }

        // 4. 全 OK
        format!(
            "硬件虚拟化就绪（{vendor} /dev/kvm 可用）",
            vendor = vendor_str
        )
    }
}

// ----------------------------------------------------------------------------
// 纯逻辑解析函数（不碰文件系统，可单测）
// ----------------------------------------------------------------------------

/// 解析 /proc/cpuinfo 内容，提取 flags 和 vendor（纯函数，可单测）。
///
/// 返回 `(cpu_has_virt_flags, cpu_vendor)`：
/// - flags 行含 `vmx` → Intel，含 `svm` → AMD；否则按 `vendor_id` 文本判厂商
/// - `cpu_has_virt_flags` 仅由 vmx/svm 标志位决定（与厂商独立）
pub fn parse_cpuinfo(content: &str) -> (bool, CpuVendor) {
    let mut has_vmx = false;
    let mut has_svm = false;
    let mut vendor_id: Option<String> = None;

    for line in content.lines() {
        // /proc/cpuinfo 每行形如 "key\t: value"
        let mut parts = line.splitn(2, ':');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }
        match key {
            "flags" => {
                // flags 是空格分隔的 token 列表
                for tok in value.split_whitespace() {
                    if tok == "vmx" {
                        has_vmx = true;
                    } else if tok == "svm" {
                        has_svm = true;
                    }
                }
            }
            "vendor_id" => {
                vendor_id = Some(value.to_string());
            }
            _ => {}
        }
    }

    let has_virt_flags = has_vmx || has_svm;
    let vendor = if has_vmx {
        CpuVendor::Intel
    } else if has_svm {
        CpuVendor::Amd
    } else {
        match vendor_id.as_deref() {
            Some(v) if v.eq_ignore_ascii_case("GenuineIntel") => CpuVendor::Intel,
            Some(v) if v.eq_ignore_ascii_case("AuthenticAMD") => CpuVendor::Amd,
            Some(v) => CpuVendor::Unknown(v.to_string()),
            None => CpuVendor::Unknown("unknown".to_string()),
        }
    };
    (has_virt_flags, vendor)
}

/// 解析 /proc/modules 内容，判断 kvm 相关模块是否加载（纯函数，可单测）。
///
/// 任一 `kvm` / `kvm_intel` / `kvm_amd` 出现即视为已加载。
pub fn parse_modules(content: &str) -> bool {
    content.lines().any(|line| {
        let name = line.split_whitespace().next().unwrap_or("");
        name == "kvm" || name == "kvm_intel" || name == "kvm_amd"
    })
}

// ----------------------------------------------------------------------------
// 真实检测函数（读 /proc/cpuinfo、/dev/kvm、/proc/modules）
// ----------------------------------------------------------------------------

/// 真实检测本机虚拟化能力（读 /proc/cpuinfo、/dev/kvm、/proc/modules）。
///
/// 仅在文件读取本身失败时返回 `Err(io::Error)`（如 /proc 不可读）；
/// 检测到的"不可用"状态（如 KVM 未加载）通过返回值字段表达，不视为错误。
pub fn detect_virt_capability() -> std::io::Result<VirtCheckResult> {
    // 1. /proc/cpuinfo → flags + vendor
    let cpuinfo = fs::read_to_string("/proc/cpuinfo")?;
    let (cpu_has_virt_flags, cpu_vendor) = parse_cpuinfo(&cpuinfo);

    // 2. /dev/kvm 是否存在
    let kvm_device_present = Path::new("/dev/kvm").exists();

    // 3. /proc/modules → kvm 相关模块（读取失败时视为未加载，不阻塞检测）
    let kvm_module_loaded = fs::read_to_string("/proc/modules")
        .map(|c| parse_modules(&c))
        .unwrap_or(false);

    // 4. 嵌套虚拟化参数
    let nested_virt = detect_nested_virt(&cpu_vendor);

    Ok(VirtCheckResult {
        cpu_has_virt_flags,
        cpu_vendor,
        kvm_device_present,
        kvm_module_loaded,
        nested_virt,
    })
}

/// 读 KVM 模块的 nested 参数（Intel/AMD 路径不同）。
fn detect_nested_virt(vendor: &CpuVendor) -> NestedVirtStatus {
    let param_path = match vendor {
        CpuVendor::Intel => "/sys/module/kvm_intel/parameters/nested",
        CpuVendor::Amd => "/sys/module/kvm_amd/parameters/nested",
        // 未知厂商：无对应内核模块路径
        CpuVendor::Unknown(_) => return NestedVirtStatus::Unknown,
    };
    match fs::read_to_string(param_path) {
        Ok(content) => {
            // 内核参数通常为 "Y"/"N"（或 "1"/"0"，大小写不一）
            let s = content.trim().to_ascii_lowercase();
            let enabled = matches!(s.as_str(), "y" | "yes" | "1" | "true" | "on");
            NestedVirtStatus::Supported(enabled)
        }
        Err(_) => NestedVirtStatus::Unknown,
    }
}

// ----------------------------------------------------------------------------
// 前置检查便捷函数（VM 启动前调用）
// ----------------------------------------------------------------------------

/// VM 启动前置检查：检测硬件虚拟化是否可用，不可用时返回
/// `ComputeError::HardwareVirtualizationUnavailable`（携带用户友好诊断）。
///
/// 用法：用户点"启动 VM"前先调它；不可用时把诊断字符串直接展示给用户，
/// 而不是等到 libvirt 启动失败才看到晦涩错误。
pub async fn preflight_virt_check() -> ComputeResult<()> {
    let result = detect_virt_capability()
        .map_err(|e| ComputeError::HardwareVirtualizationUnavailable(format!("检测失败: {e}")))?;
    if result.is_usable() {
        Ok(())
    } else {
        Err(ComputeError::HardwareVirtualizationUnavailable(
            result.to_user_diagnostic(),
        ))
    }
}
