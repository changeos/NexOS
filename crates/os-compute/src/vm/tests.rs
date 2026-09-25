//! vm 模块单元测试：CPU 拓扑校验、规格校验、MAC 校验、状态机转换、libvirt XML 渲染。

use super::*;
use crate::ComputeError;
use os_core::{NodeId, VmId, VolumeId};

fn sample_spec() -> VmSpec {
    VmSpec {
        cpus: CpuTopology::new(2),
        memory_mb: 2048,
        disk_vol_id: VolumeId::new("tank/vm/test-root"),
        nics: vec![VmNic::virtio("br0")],
        firmware: VmFirmware::Bios,
    }
}

// ---------------- CPU 拓扑 ----------------

#[test]
fn cpu_topology_new_is_symmetric() {
    let t = CpuTopology::new(4);
    assert_eq!(t.vcpus, 4);
    assert_eq!(t.sockets, 1);
    assert_eq!(t.cores, 4);
    assert_eq!(t.threads, 1);
    assert!(t.validate().is_ok());
}

#[test]
fn cpu_topology_with_topology_consistent() {
    let t = CpuTopology::with_topology(2, 4, 2);
    assert_eq!(t.vcpus, 16);
    assert!(t.validate().is_ok());
}

#[test]
fn cpu_topology_validate_rejects_zero() {
    let t = CpuTopology {
        vcpus: 0,
        sockets: 1,
        cores: 1,
        threads: 1,
    };
    assert!(matches!(t.validate(), Err(ComputeError::InvalidSpec(_))));
}

#[test]
fn cpu_topology_validate_rejects_mismatch() {
    let t = CpuTopology {
        vcpus: 4,
        sockets: 2,
        cores: 2,
        threads: 1, // 2*2*1=4 ok；改 threads 让乘积≠vcpus
    };
    assert!(t.validate().is_ok());
    let bad = CpuTopology {
        vcpus: 5,
        sockets: 2,
        cores: 2,
        threads: 1, // 4 != 5
    };
    assert!(matches!(bad.validate(), Err(ComputeError::InvalidSpec(_))));
}

// ---------------- MAC / VmNic ----------------

#[test]
fn mac_validation_accepts_valid() {
    assert!(is_valid_mac("52:54:00:aa:bb:cc"));
    assert!(is_valid_mac("AA:BB:CC:DD:EE:FF"));
}

#[test]
fn mac_validation_rejects_invalid() {
    assert!(!is_valid_mac("not-a-mac"));
    assert!(!is_valid_mac("52:54:00:aa:bb")); // 段数不足
    assert!(!is_valid_mac("52:54:00:aa:bb:cc:dd")); // 段数过多
    assert!(!is_valid_mac("GG:54:00:aa:bb:cc")); // 非十六进制
}

#[test]
fn vmnic_rejects_empty_bridge() {
    let nic = VmNic {
        bridge: "  ".into(),
        mac: None,
        model: NicModel::Virtio,
    };
    assert!(matches!(nic.validate(), Err(ComputeError::InvalidSpec(_))));
}

#[test]
fn vmnic_rejects_bad_mac() {
    let nic = VmNic {
        bridge: "br0".into(),
        mac: Some("nope".into()),
        model: NicModel::Virtio,
    };
    assert!(matches!(nic.validate(), Err(ComputeError::InvalidSpec(_))));
}

// ---------------- VmSpec 校验 ----------------

#[test]
fn spec_validate_ok_for_sample() {
    assert!(sample_spec().validate().is_ok());
}

#[test]
fn spec_validate_rejects_zero_memory() {
    let mut s = sample_spec();
    s.memory_mb = 0;
    assert!(matches!(s.validate(), Err(ComputeError::InvalidSpec(_))));
}

#[test]
fn spec_validate_rejects_oversize_memory() {
    let mut s = sample_spec();
    s.memory_mb = MAX_VM_MEMORY_MB + 1;
    assert!(matches!(s.validate(), Err(ComputeError::InvalidSpec(_))));
}

#[test]
fn spec_validate_rejects_no_nics() {
    let mut s = sample_spec();
    s.nics.clear();
    assert!(matches!(s.validate(), Err(ComputeError::InvalidSpec(_))));
}

// ---------------- 状态机 ----------------

#[test]
fn state_transitions_legal() {
    use VmState::*;
    assert!(Stopped.can_transition_to(Running));
    assert!(Running.can_transition_to(Paused));
    assert!(Paused.can_transition_to(Running));
    assert!(Running.can_transition_to(Stopped));
    assert!(Stopped.can_transition_to(Stopped)); // 幂等
    assert!(Failed.can_transition_to(Stopped));
}

#[test]
fn state_transitions_illegal() {
    use VmState::*;
    assert!(!Paused.can_transition_to(Migrating));
    assert!(!Stopped.can_transition_to(Paused));
    assert!(!Failed.can_transition_to(Migrating));
}

#[test]
fn state_transition_to_returns_target_or_err() {
    use VmState::*;
    assert_eq!(Stopped.transition_to(Running).unwrap(), Running);
    assert!(matches!(
        Paused.transition_to(Migrating),
        Err(ComputeError::InvalidSpec(_))
    ));
}

// ---------------- Vm 构造器 ----------------

#[test]
fn vm_new_defined_starts_stopped_unscheduled() {
    let vm = Vm::new_defined(VmId::new("vm-1"), "test", sample_spec());
    assert_eq!(vm.state, VmState::Stopped);
    assert!(vm.node_id.is_none());
    assert_eq!(vm.name, "test");
}

#[test]
fn vm_start_sets_running_and_node() {
    let mut vm = Vm::new_defined(VmId::new("vm-1"), "test", sample_spec());
    vm.start(NodeId::new("node-a")).unwrap();
    assert_eq!(vm.state, VmState::Running);
    assert_eq!(vm.node_id.as_ref().unwrap().as_str(), "node-a");
}

#[test]
fn vm_start_from_running_is_idempotent() {
    let mut vm = Vm::new_defined(VmId::new("vm-1"), "test", sample_spec());
    vm.start(NodeId::new("node-a")).unwrap();
    // 已 Running，再次 start 应为合法同态转换
    assert!(vm.start(NodeId::new("node-b")).is_ok());
}

#[test]
fn vm_stop_from_paused_ok() {
    let mut vm = Vm::new_defined(VmId::new("vm-1"), "test", sample_spec());
    vm.start(NodeId::new("node-a")).unwrap();
    vm.state = VmState::Paused;
    assert!(vm.stop().is_ok());
    assert_eq!(vm.state, VmState::Stopped);
}

// ---------------- libvirt XML 渲染 ----------------

#[test]
fn zvol_device_path_format() {
    let p = zvol_device_path(&VolumeId::new("tank/vm/foo"));
    assert_eq!(p, "/dev/zvol/tank/vm/foo");
}

#[test]
fn domain_xml_contains_required_sections_bios() {
    let spec = sample_spec();
    let xml = spec
        .to_libvirt_xml(&VmId::new("vm-uuid-1"), "my-vm")
        .unwrap();

    assert!(xml.starts_with("<domain type='kvm'>"));
    assert!(xml.contains("<name>my-vm</name>"));
    assert!(xml.contains("<uuid>vm-uuid-1</uuid>"));
    // 2048 MB = 2097152 KiB
    assert!(xml.contains("<memory unit='KiB'>2097152</memory>"));
    assert!(xml.contains("<currentMemory unit='KiB'>2097152</currentMemory>"));
    assert!(xml.contains("<vcpu placement='static'>2</vcpu>"));
    // 拓扑：sample 用 new(2) -> sockets=1 cores=2 threads=1
    assert!(xml.contains("<topology sockets='1' cores='2' threads='1'/>"));
    assert!(xml.contains("mode='host-passthrough'"));
    // zvol 磁盘
    assert!(xml.contains("<source dev='/dev/zvol/tank/vm/test-root'/>"));
    assert!(xml.contains("<target dev='vda' bus='virtio'/>"));
    assert!(xml.contains("type='raw'"));
    // 网卡桥接
    assert!(xml.contains("<interface type='bridge'>"));
    assert!(xml.contains("<source bridge='br0'/>"));
    assert!(xml.contains("<model type='virtio'/>"));
    // VNC
    assert!(xml.contains("<graphics type='vnc'"));
    // BIOS: 不应出现 OVMF loader
    assert!(!xml.contains("OVMF"));
    assert!(xml.ends_with("</domain>"));
}

#[test]
fn domain_xml_uefi_includes_ovmf_loader() {
    let mut spec = sample_spec();
    spec.firmware = VmFirmware::Uefi;
    let xml = spec.to_libvirt_xml(&VmId::new("u1"), "uefi-vm").unwrap();
    assert!(xml.contains("<loader readonly='yes' type='pflash'>"));
    assert!(xml.contains("OVMF_CODE.fd"));
}

#[test]
fn domain_xml_e1000_model_and_mac() {
    let mut spec = sample_spec();
    spec.nics = vec![VmNic {
        bridge: "br1".into(),
        mac: Some("52:54:00:aa:bb:cc".into()),
        model: NicModel::E1000,
    }];
    let xml = spec.to_libvirt_xml(&VmId::new("u1"), "vm").unwrap();
    assert!(xml.contains("<model type='e1000'/>"));
    assert!(xml.contains("<mac address='52:54:00:aa:bb:cc'/>"));
}

#[test]
fn domain_xml_escapes_special_chars_in_name() {
    let spec = sample_spec();
    let xml = spec.to_libvirt_xml(&VmId::new("u1"), "<bad>&name").unwrap();
    assert!(xml.contains("&lt;bad&gt;&amp;name"));
    assert!(!xml.contains("<bad>"));
}

#[test]
fn domain_xml_rejects_invalid_spec() {
    let mut spec = sample_spec();
    spec.memory_mb = 0;
    assert!(matches!(
        spec.to_libvirt_xml(&VmId::new("u1"), "vm"),
        Err(ComputeError::InvalidSpec(_))
    ));
}

#[test]
fn domain_xml_multiple_nics() {
    let mut spec = sample_spec();
    spec.nics = vec![VmNic::virtio("br0"), VmNic::virtio("br1")];
    let xml = spec.to_libvirt_xml(&VmId::new("u1"), "vm").unwrap();
    assert_eq!(xml.matches("<interface type='bridge'>").count(), 2);
    assert!(xml.contains("bridge='br0'"));
    assert!(xml.contains("bridge='br1'"));
}
