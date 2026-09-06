//! os-compute `virt-ffi` 真实 libvirt 路径集成测（feature `virt-ffi`）。
//!
//! ## 目的
//! 验证 `LibvirtVmManager` 真实路径（`virt` crate 经 libvirt C API）能真实连接
//! libvirt、列举 domain、跑完整 domain 生命周期。**不**测内存态骨架路径
//! （那条路径由 `impl_vm::fallback` 的内联 unit test 覆盖）。
//!
//! ## test fixture：`test:///default`
//! 只用 libvirt 内置的 **`test:///default`** 驱动（libvirt 自带、纯内存、**不需要
//! libvirtd 守护进程、不需要 root、不碰任何真实 VM**）。该 fixture 启动后内置 1 个
//! 名为 `test` 的 running domain（`virsh -c test:///default list --all` 可见）。
//!
//! **红线：本文件绝不连接 `qemu:///system`（需 root + libvirtd + 可能触碰真实
//! VM）。**所有连接一律 `test:///default`。
//!
//! ## 运行门控
//! - 全文件 `#![cfg(feature = "virt-ffi")]`：feature 关闭时本文件不参与编译
//!   （fallback 内存态路径无 `virt` crate，且语义不同）。
//! - 全部 `#[ignore]`：默认套件不跑，需 `--ignored` 显式触发。
//! - 运行时机 libvirt 不可用（连接失败）：优雅 SKIP（eprintln + return），不 panic。
//!
//! ## 运行
//! ```bash
//! # 需本机装 libvirt-dev（提供 libvirt.so）+ virsh（验证 fixture）。
//! cargo test -p os-compute --features virt-ffi --test vm_real -- --ignored --nocapture
//! ```

// 全文件门控：feature 关闭时不编译（fallback 路径无 virt crate 且语义不同）。
#![cfg(feature = "virt-ffi")]

use os_compute::{LibvirtVmManager, VmManager, VmSpec, VmState};

use virt::connect::Connect;
use virt::domain::Domain;
use virt::sys;

/// 测试用 libvirt URI——test 驱动内置 fixture（无 KVM/libvirtd/root 需求）。
const TEST_URI: &str = "test:///default";

/// test 驱动内置的 fixture domain 名（`test:///default` 启动即含 1 个 running 域）。
const FIXTURE_DOMAIN_NAME: &str = "test";

/// 一个合法 UUID 串，用作本测创建临时 domain 的 VmId（libvirt domain `<uuid>`
/// 会被校验，必须 36 字符标准格式 `8-4-4-4-12` hex）。
fn fresh_uuid() -> String {
    // 用纳秒保证并发跑多次不撞名（test 驱动 domain 名唯一约束）。
    // 取低 48 位（12 hex）确保末段恰好 12 字符——`{:012x}` 在数值超宽时会扩展
    // 宽度破坏 UUID 长度，故先掩码。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tail = (nanos & 0xFFFF_FFFF_FFFF) as u64; // 低 48 位
    format!("123e4567-e89b-12d3-a456-{tail:012x}")
}

/// 构造一个最小可用的 VmSpec（test 驱动不强校验 disk 路径，故用占位 zvol）。
fn minimal_spec() -> VmSpec {
    use os_compute::{CpuTopology, VmFirmware, VmNic};
    VmSpec {
        cpus: CpuTopology::new(1),
        memory_mb: 128,
        disk_vol_id: os_core::VolumeId::new("tank/vm/test"),
        nics: vec![VmNic::virtio("br0")],
        firmware: VmFirmware::Bios,
    }
}

/// 谓词：本机 libvirt test 驱动可达。不可用时真实测整体优雅 SKIP。
fn test_driver_reachable() -> bool {
    Connect::open(Some(TEST_URI)).is_ok()
}

/// SKIP 守卫宏：不可达时打印原因并 return（不能直接 `return` 出 test fn，故做宏）。
macro_rules! skip_if_no_libvirt {
    () => {
        if !test_driver_reachable() {
            eprintln!(
                "SKIP: libvirt test 驱动不可用（无 libvirt-dev / libvirt 连接失败）。\
                 本测仅在装了 libvirt-dev 且 test:///default 可用的环境执行。"
            );
            return;
        }
    };
}

// ============================================================================
// A. fixture 级：直接用 virt crate 验证 test:///default 可连接 + 可列举
// ============================================================================

/// 验证 `virConnectOpen("test:///default")` 连接成功（libvirt test 驱动真实可达）。
#[tokio::test]
#[ignore = "真实 libvirt 测：需本机 libvirt-dev + test:///default fixture"]
async fn real_connect_test_driver_default() {
    skip_if_no_libvirt!();

    let conn = Connect::open(Some(TEST_URI)).expect("virConnectOpen(test:///default) 应成功");
    // 连接保持期间可查询。Drop 时自动 close。
    let hostname = conn
        .get_hostname()
        .unwrap_or_else(|_| "<unknown>".to_string());
    eprintln!("已连接 test:///default，hostname={hostname}");
    // test 驱动 hostname 不保证内容，仅验证 API 不 panic 即可；conn 在作用域末自动 close。
}

/// 验证 `virConnectListAllDomains` 在 test:///default 至少返回 1 个 fixture 域。
#[tokio::test]
#[ignore = "真实 libvirt 测：需本机 libvirt-dev + test:///default fixture"]
async fn real_list_domains_has_fixture() {
    skip_if_no_libvirt!();

    let conn = Connect::open(Some(TEST_URI)).expect("连接 test:///default");
    let flags = sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE | sys::VIR_CONNECT_LIST_DOMAINS_INACTIVE;
    let domains = conn
        .list_all_domains(flags)
        .expect("list_all_domains 应成功");
    assert!(
        !domains.is_empty(),
        "test:///default 应至少含 1 个内置 fixture domain"
    );
    // 验证内置 `test` 域存在（名匹配）。
    let names: Vec<String> = domains.iter().filter_map(|d| d.get_name().ok()).collect();
    assert!(
        names.iter().any(|n| n == FIXTURE_DOMAIN_NAME),
        "test:///default 应含名为 `{FIXTURE_DOMAIN_NAME}` 的 fixture 域，实际: {names:?}"
    );
}

// ============================================================================
// B. fixture 级：domain 生命周期（define/create/suspend/resume/destroy/undefine）
//    test 驱动能力有限但支持上述基本转换；不支持则优雅跳过对应断言。
// ============================================================================

/// 在 test:///default 上跑一个临时 domain 的完整生命周期：
/// define（Shutoff）→ create（Running）→ suspend（Paused）→ resume（Running）
/// → destroy（Shutoff）→ undefine（消失）。
///
/// 使用临时 UUID + 临时名，测完 undefine 清理，**绝不碰 fixture `test` 域**。
#[tokio::test]
#[ignore = "真实 libvirt 测：需本机 libvirt-dev + test:///default fixture"]
async fn real_domain_lifecycle_round_trip() {
    skip_if_no_libvirt!();

    let conn = Connect::open(Some(TEST_URI)).expect("连接 test:///default");
    let uuid = fresh_uuid();
    let name = format!("vmreal-lc-{}", &uuid[..8]);

    // 最小 domain XML（test 驱动接受）。借用 os-compute 的 XML 渲染保证语义正确。
    let id = os_core::VmId::new(&uuid);
    let xml = minimal_spec()
        .to_libvirt_xml(&id, &name)
        .expect("VmSpec 渲染 domain XML 应成功");

    // 1) define：定义新 domain（→ Shutoff）。
    let dom = Domain::define_xml(&conn, &xml).expect("virDomainDefineXML 应成功");
    let (raw, _) = dom.get_state().expect("get_state 应成功");
    assert_eq!(raw, sys::VIR_DOMAIN_SHUTOFF, "新定义 domain 应为 Shutoff");

    // 2) create：启动（→ Running）。
    dom.create().expect("virDomainCreate 应成功");
    let (raw, _) = dom.get_state().expect("get_state");
    assert_eq!(raw, sys::VIR_DOMAIN_RUNNING, "create 后应 Running");

    // 3) suspend（→ Paused）。
    dom.suspend().expect("virDomainSuspend 应成功");
    let (raw, _) = dom.get_state().expect("get_state");
    assert_eq!(raw, sys::VIR_DOMAIN_PAUSED, "suspend 后应 Paused");

    // 4) resume（→ Running）。
    dom.resume().expect("virDomainResume 应成功");
    let (raw, _) = dom.get_state().expect("get_state");
    assert_eq!(raw, sys::VIR_DOMAIN_RUNNING, "resume 后应 Running");

    // 5) destroy：硬断电（→ Shutoff）。
    dom.destroy().expect("virDomainDestroy 应成功");
    let (raw, _) = dom.get_state().expect("get_state");
    assert_eq!(raw, sys::VIR_DOMAIN_SHUTOFF, "destroy 后应 Shutoff");

    // 6) undefine：删除 domain 定义。
    dom.undefine().expect("virDomainUndefine 应成功");
    // undefine 后再 lookup 应失败（domain not found）。
    let gone = Domain::lookup_by_uuid_string(&conn, &uuid);
    assert!(
        gone.is_err(),
        "undefine 后按 UUID 查找应失败（domain 已删除）"
    );
}

// ============================================================================
// C. LibvirtVmManager 真实路径（virt-ffi）：端到端走 trait 抽象
// ============================================================================

/// 验证 `LibvirtVmManager::with_uri(_, "test:///default")` 真实路径：
/// 构造 → list_vms（应至少含 fixture `test` 域）→ 跑 create/start/pause/resume/
/// stop/destroy 一个临时 VM，验证状态转换与 VmManager trait 契约。
#[tokio::test]
#[ignore = "真实 libvirt 测：需本机 libvirt-dev + test:///default fixture"]
async fn real_libvirt_vm_manager_test_driver() {
    skip_if_no_libvirt!();

    let mgr = LibvirtVmManager::with_uri("node-real", TEST_URI);
    assert_eq!(mgr.virt_uri(), TEST_URI);
    assert_eq!(mgr.local_node().as_str(), "node-real");

    // list_vms：fixture 自带 `test` 域，至少 1 个。
    let listed = mgr.list_vms().await.expect("list_vms 应成功");
    assert!(
        !listed.is_empty(),
        "test:///default list_vms 应至少含 fixture `test` 域"
    );

    // 跑一个临时 VM 的完整生命周期（VmManager trait 契约）。
    let uuid = fresh_uuid();
    let id = os_core::VmId::new(&uuid);
    let name = format!("vmreal-mgr-{}", &uuid[..8]);

    // create_vm：define（→ Stopped，未调度）。
    let vm = mgr
        .create_vm(&id, &name, minimal_spec())
        .await
        .expect("create_vm 应成功");
    assert_eq!(vm.state, VmState::Stopped);
    assert!(vm.node_id.is_none(), "新建未启动 VM 不应已调度到节点");

    // start_vm：→ Running，调度到本节点。
    let started = mgr.start_vm(&id).await.expect("start_vm 应成功");
    assert_eq!(started.state, VmState::Running);
    assert_eq!(
        started.node_id.as_ref().expect("started 应已调度").as_str(),
        "node-real"
    );

    // pause_vm：→ Paused。
    let paused = mgr.pause_vm(&id).await.expect("pause_vm 应成功");
    assert_eq!(paused.state, VmState::Paused);

    // resume_vm：→ Running。
    let resumed = mgr.resume_vm(&id).await.expect("resume_vm 应成功");
    assert_eq!(resumed.state, VmState::Running);

    // get_vm：按 ID 反查应一致。
    let got = mgr.get_vm(&id).await.expect("get_vm 应成功");
    assert_eq!(got.id, id);

    // stop_vm(force)：→ Stopped。
    let stopped = mgr.stop_vm(&id, true).await.expect("stop_vm 应成功");
    assert_eq!(stopped.state, VmState::Stopped);

    // destroy_vm：undefine，后续 get_vm 应 VmNotFound。
    mgr.destroy_vm(&id).await.expect("destroy_vm 应成功");
    let err = mgr.get_vm(&id).await.unwrap_err();
    assert!(
        matches!(err, os_compute::ComputeError::VmNotFound(_)),
        "destroy 后 get_vm 应返回 VmNotFound，实际: {err:?}"
    );
}
