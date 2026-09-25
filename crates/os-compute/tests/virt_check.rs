//! virt_check 集成测试
//!
//! 两层：
//! - **A. 纯逻辑单测**（默认跑，不依赖任何特权/真实系统状态）：覆盖
//!   `parse_cpuinfo` / `parse_modules` / `VirtCheckResult::is_usable` +
//!   `to_user_diagnostic` 的各种组合。
//! - **B. 真实测**（`#[ignore]`，需在本机 Intel Ultra 5 + /dev/kvm + kvm_intel
//!   环境跑）：`detect_virt_capability` / `preflight_virt_check`。
//!
//! 真实测运行：`cargo test -p os-compute --features mock --test virt_check -- --ignored --nocapture`

use os_compute::{
    detect_virt_capability, parse_cpuinfo, parse_modules, preflight_virt_check, ComputeError,
    CpuVendor, NestedVirtStatus, VirtCheckResult,
};

// ============================================================================
// A. 纯逻辑单测（默认跑）
// ============================================================================

// ---------------- parse_cpuinfo ----------------

#[test]
fn parse_cpuinfo_intel_vmx() {
    // 精简的 Intel cpuinfo 片段（只保留 vendor_id 与 flags 两行）
    let content = "processor\t: 0\n\
                   vendor_id\t: GenuineIntel\n\
                   cpu family\t: 6\n\
                   model\t: 183\n\
                   flags\t\t: fpu vme de pse tsc msr vmx smx ht lm\n";
    let (has_flags, vendor) = parse_cpuinfo(content);
    assert!(has_flags, "vmx 标志位应被检测到");
    assert_eq!(vendor, CpuVendor::Intel);
}

#[test]
fn parse_cpuinfo_amd_svm() {
    let content = "processor\t: 0\n\
                   vendor_id\t: AuthenticAMD\n\
                   flags\t\t: fpu vme de pse tsc msr svm ht lm\n";
    let (has_flags, vendor) = parse_cpuinfo(content);
    assert!(has_flags, "svm 标志位应被检测到");
    assert_eq!(vendor, CpuVendor::Amd);
}

#[test]
fn parse_cpuinfo_other_vendor_no_virt_flags() {
    // 未知厂商、无 vmx/svm 标志位（如老 CPU / 虚拟化被 BIOS 关闭）
    let content = "processor\t: 0\n\
                   vendor_id\t: SomeOtherVendor\n\
                   flags\t\t: fpu vme de pse tsc msr ht lm\n";
    let (has_flags, vendor) = parse_cpuinfo(content);
    assert!(!has_flags, "无 vmx/svm 应返回 false");
    assert_eq!(vendor, CpuVendor::Unknown("SomeOtherVendor".to_string()));
}

#[test]
fn parse_cpuinfo_intel_vendor_no_vmx_flags() {
    // Intel CPU 但 vmx 关闭（flags 中无 vmx）—— 模拟 BIOS 关 VT-x
    let content = "vendor_id\t: GenuineIntel\n\
                   flags\t\t: fpu vme de pse tsc msr ht lm\n";
    let (has_flags, vendor) = parse_cpuinfo(content);
    assert!(!has_flags, "无 vmx 标志位");
    assert_eq!(vendor, CpuVendor::Intel, "厂商仍由 vendor_id 判定");
}

#[test]
fn parse_cpuinfo_token_boundary_not_substring() {
    // 确保 vmx/svm 是 token 精确匹配，不是子串匹配
    // 如 "myvmxtoken" 不应被误判为 vmx
    let content = "vendor_id\t: GenuineIntel\n\
                   flags\t\t: myvmxtoken svmext\n";
    let (has_flags, _vendor) = parse_cpuinfo(content);
    assert!(
        !has_flags,
        "vmx/svm 必须是独立 token，myvmxtoken/svmext 不应命中"
    );
}

#[test]
fn parse_cpuinfo_empty_input() {
    let (has_flags, vendor) = parse_cpuinfo("");
    assert!(!has_flags);
    assert!(matches!(vendor, CpuVendor::Unknown(_)));
}

// ---------------- parse_modules ----------------

#[test]
fn parse_modules_with_kvm_intel() {
    let content = "kvm_intel 552960 0 - Live 0x0000\n\
                   kvm 1527808 1 kvm_intel, Live 0x0000\n\
                   loop 32768 0 - Live 0x0000\n";
    assert!(parse_modules(content));
}

#[test]
fn parse_modules_with_kvm_amd() {
    let content = "kvm_amd 114688 0 - Live 0x0000\n\
                   kvm 1527808 1 kvm_amd, Live 0x0000\n";
    assert!(parse_modules(content));
}

#[test]
fn parse_modules_without_kvm() {
    let content = "loop 32768 0 - Live 0x0000\n\
                   ext4 1024000 2 - Live 0x0000\n";
    assert!(!parse_modules(content));
}

#[test]
fn parse_modules_empty() {
    assert!(!parse_modules(""));
}

#[test]
fn parse_modules_kvm_substring_not_matched() {
    // 如 "kvmgt" / "mykvm_mod" 不应被误判
    let content = "kvmgt 32768 0 - Live 0x0000\n\
                   mykvm_mod 16384 0 - Live 0x0000\n";
    assert!(
        !parse_modules(content),
        "kvm 必须是独立 token，kvmgt/mykvm_mod 不应命中"
    );
}

// ---------------- VirtCheckResult::is_usable + to_user_diagnostic ----------------

#[test]
fn is_usable_all_ok() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: true,
        kvm_module_loaded: true,
        nested_virt: NestedVirtStatus::Supported(true),
    };
    assert!(r.is_usable());
    let diag = r.to_user_diagnostic();
    assert!(
        diag.contains("硬件虚拟化就绪"),
        "全 OK 诊断应含'就绪'，实际: {diag}"
    );
    assert!(diag.contains("Intel"));
    assert!(diag.contains("/dev/kvm"));
}

#[test]
fn is_usable_false_when_cpu_lacks_flags() {
    // 老 CPU：无 vmx/svm
    let r = VirtCheckResult {
        cpu_has_virt_flags: false,
        cpu_vendor: CpuVendor::Unknown("OldCPU".into()),
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(!r.is_usable());
    let diag = r.to_user_diagnostic();
    assert!(
        diag.contains("不支持硬件虚拟化"),
        "无 virt flags 诊断应含'不支持硬件虚拟化'，实际: {diag}"
    );
    assert!(diag.contains("vmx/svm"));
    assert!(diag.contains("OldCPU"));
}

#[test]
fn diagnostic_cpu_ok_but_kvm_missing_suggests_modprobe_intel() {
    // CPU 支持，但 KVM 模块未加载（BIOS 关 VT-x 或未 modprobe）
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(!r.is_usable());
    let diag = r.to_user_diagnostic();
    assert!(
        diag.contains("KVM 内核模块未加载"),
        "kvm 缺失诊断应含'KVM 内核模块未加载'，实际: {diag}"
    );
    assert!(diag.contains("BIOS"), "应建议检查 BIOS");
    assert!(
        diag.contains("modprobe kvm kvm_intel"),
        "Intel 应建议 modprobe kvm kvm_intel，实际: {diag}"
    );
}

#[test]
fn diagnostic_cpu_ok_but_kvm_missing_amd() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Amd,
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    let diag = r.to_user_diagnostic();
    assert!(
        diag.contains("modprobe kvm kvm_amd"),
        "AMD 应建议 modprobe kvm kvm_amd，实际: {diag}"
    );
}

#[test]
fn diagnostic_device_present_but_module_not_listed() {
    // 罕见：/dev/kvm 存在但 /proc/modules 未列 kvm（容器/沙箱环境）
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: true,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(r.is_usable(), "/dev/kvm 存在即视为可用（模块列表为辅）");
    let diag = r.to_user_diagnostic();
    assert!(
        diag.contains("/proc/modules 未检测到 kvm"),
        "罕见情况诊断应说明 /proc/modules 状态，实际: {diag}"
    );
}

#[test]
fn diagnostic_priority_cpu_before_kvm() {
    // 同时缺 flags 和 kvm：优先报 CPU 不支持（根因优先）
    let r = VirtCheckResult {
        cpu_has_virt_flags: false,
        cpu_vendor: CpuVendor::Unknown("Ancient".into()),
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    let diag = r.to_user_diagnostic();
    assert!(
        diag.contains("不支持硬件虚拟化"),
        "CPU 不可用优先于 KVM，实际: {diag}"
    );
}

#[test]
fn serde_roundtrip_preserves_result() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Amd,
        kvm_device_present: true,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Supported(false),
    };
    let json = serde_json::to_string(&r).unwrap();
    let back: VirtCheckResult = serde_json::from_str(&json).unwrap();
    assert_eq!(r, back);
}

// ============================================================================
// A2. 纯逻辑补充测（parse_cpuinfo 边界 / parse_modules 边界 / CpuVendor
//     NestedVirtStatus serde / 诊断 Unknown 厂商 / VirtCheckResult 字段）
// ============================================================================

// ---------------- parse_cpuinfo 补充边界 ----------------

#[test]
fn parse_cpuinfo_vmx_takes_precedence_over_vendor_unknown() {
    // 含 vmx 标志 + 非 Intel/AMD vendor_id：以 vmx 标志位判定为 Intel（vmx 优先）
    let content = "vendor_id\t: SomeOther\nflags\t: fpu vmx lm\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(has);
    assert_eq!(vendor, CpuVendor::Intel, "vmx 优先于 vendor_id 文本判定");
}

#[test]
fn parse_cpuinfo_both_vmx_and_svm_intel_wins() {
    // 同时含 vmx 和 svm（理论不会发生）—— vmx 分支优先，归 Intel
    let content = "vendor_id\t: GenuineIntel\nflags\t: vmx svm\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(has);
    assert_eq!(vendor, CpuVendor::Intel);
}

#[test]
fn parse_cpuinfo_amd_no_svm_vendor_from_vendor_id() {
    // AuthenticAMD 但 flags 无 svm（BIOS 关 AMD-V）→ 厂商仍 AMD（vendor_id 判定）
    let content = "vendor_id\t: AuthenticAMD\nflags\t: fpu lm\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(!has);
    assert_eq!(vendor, CpuVendor::Amd);
}

#[test]
fn parse_cpuinfo_no_vendor_id_line_unknown() {
    // 无 vendor_id 行，无 vmx/svm → Unknown("unknown")
    let content = "flags\t: fpu lm\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(!has);
    assert!(matches!(vendor, CpuVendor::Unknown(s) if s == "unknown"));
}

#[test]
fn parse_cpuinfo_flags_with_extra_whitespace() {
    // flags 行含多余空格：split_whitespace 应正确 tokenize
    let content = "vendor_id\t: GenuineIntel\nflags\t:   fpu    vmx    lm  \n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(has);
    assert_eq!(vendor, CpuVendor::Intel);
}

#[test]
fn parse_cpuinfo_line_without_colon_skipped() {
    // 无 `:` 的行应被跳过（splitn 返回空 value）
    let content = "garbage line no colon\nvendor_id\t: GenuineIntel\nflags\t: vmx\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(has);
    assert_eq!(vendor, CpuVendor::Intel);
}

#[test]
fn parse_cpuinfo_multiple_processors_blocks() {
    // 多 processor 块（多核 CPU 真实 cpuinfo 有多个）：应仍正确聚合
    let content = "processor\t: 0\nvendor_id\t: GenuineIntel\nflags\t: vmx\n\
                   processor\t: 1\nvendor_id\t: GenuineIntel\nflags\t: vmx lm\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(has);
    assert_eq!(vendor, CpuVendor::Intel);
}

#[test]
fn parse_cpuinfo_only_vendor_id_no_flags() {
    // 仅 vendor_id，无 flags 行
    let content = "vendor_id\t: GenuineIntel\n";
    let (has, vendor) = parse_cpuinfo(content);
    assert!(!has, "无 flags 行 → 无 vmx/svm");
    assert_eq!(vendor, CpuVendor::Intel, "vendor_id 仍能判定厂商");
}

// ---------------- parse_modules 补充边界 ----------------

#[test]
fn parse_modules_kvm_only_no_intel_amd() {
    // 仅 kvm 主模块（无 kvm_intel/kvm_amd）—— 任一出现即视为加载
    let content = "kvm 1527808 0 - Live 0x0000\nloop 32768 0 - Live 0x0000\n";
    assert!(parse_modules(content));
}

#[test]
fn parse_modules_only_whitespace_line() {
    // 仅空行 / 空白行 → false
    assert!(!parse_modules("   \n\t\n"));
}

#[test]
fn parse_modules_leading_whitespace_around_name() {
    // 行首空格：split_whitespace().next() 应跳过
    let content = "   kvm_intel 552960 0 - Live 0x0000\n";
    assert!(parse_modules(content));
}

#[test]
fn parse_modules_kvm_in_field_not_first_token() {
    // "kvm" 出现在非首字段（如依赖列表中）—— 不应命中（首 token 才算模块名）
    let content = "loop 32768 1 kvm - Live 0x0000\n";
    assert!(!parse_modules(content), "kvm 在依赖列表中不算模块加载");
}

// ---------------- CpuVendor / NestedVirtStatus serde ----------------

#[test]
fn cpu_vendor_serde_intel() {
    let v = CpuVendor::Intel;
    let json = serde_json::to_string(&v).unwrap();
    let back: CpuVendor = serde_json::from_str(&json).unwrap();
    assert_eq!(back, v);
}

#[test]
fn cpu_vendor_serde_amd() {
    let v = CpuVendor::Amd;
    let json = serde_json::to_string(&v).unwrap();
    let back: CpuVendor = serde_json::from_str(&json).unwrap();
    assert_eq!(back, v);
}

#[test]
fn cpu_vendor_serde_unknown_preserves_string() {
    let v = CpuVendor::Unknown("SomeCPU Co.".to_string());
    let json = serde_json::to_string(&v).unwrap();
    let back: CpuVendor = serde_json::from_str(&json).unwrap();
    assert_eq!(back, v);
    // Unknown 的内部字符串应保留
    assert!(matches!(back, CpuVendor::Unknown(s) if s == "SomeCPU Co."));
}

#[test]
fn nested_virt_status_supported_true_serde() {
    let n = NestedVirtStatus::Supported(true);
    let json = serde_json::to_string(&n).unwrap();
    let back: NestedVirtStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back, n);
}

#[test]
fn nested_virt_status_supported_false_serde() {
    let n = NestedVirtStatus::Supported(false);
    let json = serde_json::to_string(&n).unwrap();
    let back: NestedVirtStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back, n);
}

#[test]
fn nested_virt_status_unknown_serde() {
    let n = NestedVirtStatus::Unknown;
    let json = serde_json::to_string(&n).unwrap();
    let back: NestedVirtStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back, n);
}

// ---------------- 诊断补充：AMD/Unknown 厂商各种组合 ----------------

#[test]
fn diagnostic_all_ok_amd() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Amd,
        kvm_device_present: true,
        kvm_module_loaded: true,
        nested_virt: NestedVirtStatus::Supported(true),
    };
    assert!(r.is_usable());
    let diag = r.to_user_diagnostic();
    assert!(diag.contains("硬件虚拟化就绪"));
    assert!(diag.contains("AMD"));
    assert!(diag.contains("/dev/kvm"));
}

#[test]
fn diagnostic_all_ok_unknown_vendor_uses_vendor_string() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Unknown("MyCPU".to_string()),
        kvm_device_present: true,
        kvm_module_loaded: true,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(r.is_usable());
    let diag = r.to_user_diagnostic();
    assert!(diag.contains("MyCPU"), "Unknown 厂商诊断应含 vendor 字符串");
    assert!(diag.contains("硬件虚拟化就绪"));
}

#[test]
fn diagnostic_kvm_missing_unknown_vendor_suggests_both() {
    // Unknown 厂商 + KVM 缺失：建议应同时提及 Intel 和 AMD
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Unknown("X".to_string()),
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    let diag = r.to_user_diagnostic();
    assert!(diag.contains("modprobe kvm kvm_intel"));
    assert!(diag.contains("modprobe kvm kvm_amd"));
}

#[test]
fn diagnostic_cpu_lacks_flags_with_intel_vendor() {
    // Intel 厂商但无 vmx（BIOS 关 VT-x）：诊断应说"不支持硬件虚拟化"
    let r = VirtCheckResult {
        cpu_has_virt_flags: false,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(!r.is_usable());
    let diag = r.to_user_diagnostic();
    assert!(diag.contains("不支持硬件虚拟化"));
    assert!(diag.contains("Intel"));
    assert!(diag.contains("vmx/svm"));
}

// ---------------- is_usable 边界 ----------------

#[test]
fn is_usable_true_even_when_module_not_listed() {
    // /dev/kvm 存在 + flags OK，但 /proc/modules 未列 → is_usable 仍 true
    // （/dev/kvm 存在是 KVM 可用的强信号）
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: true,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(r.is_usable());
}

#[test]
fn is_usable_false_when_kvm_device_absent_even_with_flags() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: false,
        kvm_module_loaded: true,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(!r.is_usable(), "无 /dev/kvm 即使模块加载也不可用");
}

#[test]
fn is_usable_false_when_flags_absent_even_with_device() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: false,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: true,
        kvm_module_loaded: true,
        nested_virt: NestedVirtStatus::Unknown,
    };
    assert!(!r.is_usable(), "无 vmx/svm 即使 /dev/kvm 在也不可用");
}

// ---------------- VirtCheckResult debug/clone/eq ----------------

#[test]
fn virt_check_result_clone_eq() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: true,
        kvm_module_loaded: true,
        nested_virt: NestedVirtStatus::Supported(true),
    };
    let cloned = r.clone();
    assert_eq!(cloned, r);
}

#[test]
fn virt_check_result_debug_format() {
    let r = VirtCheckResult {
        cpu_has_virt_flags: false,
        cpu_vendor: CpuVendor::Unknown("X".to_string()),
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    let s = format!("{r:?}");
    assert!(s.contains("VirtCheckResult"));
    assert!(s.contains("cpu_has_virt_flags"));
}

// ============================================================================
// B. 真实测（#[ignore]，本机 Intel Ultra 5 + /dev/kvm + kvm_intel 跑）
// ============================================================================

#[tokio::test]
#[ignore = "需本机真实硬件/内核环境：Intel Ultra 5 245KF + vmx + /dev/kvm + kvm_intel"]
async fn real_detect_virt_capability() {
    let r = detect_virt_capability().expect("读取 /proc/cpuinfo 等不应失败");
    println!("=== 真实检测结果 ===");
    println!("{r:#?}");
    println!("=== 用户诊断 ===");
    println!("{}", r.to_user_diagnostic());

    // 本机已知环境：Intel + vmx + /dev/kvm + kvm_intel
    assert_eq!(r.cpu_vendor, CpuVendor::Intel);
    assert!(r.cpu_has_virt_flags, "本机应有 vmx 标志位");
    assert!(r.kvm_device_present, "/dev/kvm 应存在");
    assert!(r.kvm_module_loaded, "kvm_intel 模块应已加载");
    assert!(r.is_usable(), "本机虚拟化应可用");
    // 本机 nested=Y
    assert!(
        matches!(r.nested_virt, NestedVirtStatus::Supported(true)),
        "本机 nested 应为 Y/true，实际: {:?}",
        r.nested_virt
    );
}

#[tokio::test]
#[ignore = "需本机真实硬件/内核环境（preflight 依赖 detect_virt_capability）"]
async fn real_preflight_check_passes() {
    // 本机全绿，preflight 应返回 Ok
    preflight_virt_check()
        .await
        .expect("本机虚拟化就绪，preflight 应通过");
}

#[tokio::test]
#[ignore = "验证诊断生成不依赖真实系统（构造不可用结果看诊断）"]
async fn real_diagnostic_for_unusable_result() {
    // 不调真实检测，直接构造一个不可用结果，确认诊断生成正确
    let r = VirtCheckResult {
        cpu_has_virt_flags: true,
        cpu_vendor: CpuVendor::Intel,
        kvm_device_present: false,
        kvm_module_loaded: false,
        nested_virt: NestedVirtStatus::Unknown,
    };
    let diag = r.to_user_diagnostic();
    println!("=== 不可用结果诊断 ===\n{diag}");
    assert!(diag.contains("KVM 内核模块未加载"));
    // 确认 preflight 把它包成 HardwareVirtualizationUnavailable
    let err = ComputeError::HardwareVirtualizationUnavailable(diag.clone());
    let api_err: os_common::ApiError = err.into();
    assert_eq!(api_err.code, os_common::ApiErrorCode::InvalidInput);
    assert!(api_err.message.contains("KVM 内核模块未加载"));
    println!("=== ApiError ===\n{api_err:?}");
}
