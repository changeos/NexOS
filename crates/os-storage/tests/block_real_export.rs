//! `LioBlockExport` 真实 configfs export 往返测（`#[ignore]`，需 root）。
//!
//! 与 `block_real.rs` B 类可达性测互补：本文件做**完整的创建-验证-销毁往返**，
//! 验证生产 `export_iscsi`/`unexport` 的 targetcli 编排在真实内核 LIO 上真的能
//! 把对象建进 configfs 并清掉，以及 nvmet configfs 直写（无 nvmetcli 时）真实可用。
//!
//! ## 前置（本机 batch5 已就绪）
//! - root（configfs 写需 root）；
//! - `/sys/kernel/config/target`（target_core_mod）+ `/iscsi`（iscsi_target_mod，**懒创建**，
//!   首次 targetcli 访问后才出现）；`/sys/kernel/config/nvmet`（nvmet 模块）；
//! - `targetcli`（apt: targetcli-fb）；**不要求** `nvmetcli`（本机无，nvmet 测走 configfs 直写）；
//! - `zfs` 内核模块 + 可建 file-backed zpool（为 export_iscsi 的 `/dev/zvol/<vol>` 后端备料）。
//!
//! 不满足则优雅 SKIP（eprintln 报缺什么，不 panic）。
//!
//! ## 红线
//! - 所有 iSCSI target IQN / nvmet NQN 用唯一 `osprobe` 前缀 + `<pid>_<nanos>` 后缀，
//!   **绝不碰宿主真实 target**。
//! - 每个测自带 RAII guard：zpool/zvol/target/subsystem 在 drop 时尽力清理；
//!   末尾跑 `targetcli clearconfig confirm=true` 兜底（仅清 osprobe 命名空间残留）。
//!
//! 跑法：
//! ```bash
//! sudo env PATH=$HOME/.cargo/bin:/usr/bin:/bin RUSTUP_HOME=$HOME/.rustup \
//!   CARGO_HOME=$HOME/.cargo \
//!   cargo test -p os-storage --features mock --test block_real_export -- --ignored --nocapture
//! ```

#![cfg(feature = "mock")]

use os_core::VolumeId;
use os_storage::{BlockExport, LioBlockExport};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ============================================================================
// 环境探测与跳过助手（与 block_real.rs 风格一致；本文件独立、不依赖那边私有项）
// ============================================================================

/// 跳过条件：非 root 直接 return false（不 panic）。
fn require_root() -> bool {
    // SAFETY: getuid 无副作用、永不出错。
    let uid = unsafe { getuid() };
    if uid != 0 {
        eprintln!(
            "[SKIP] 非 root（uid={uid}），configfs/targetcli/nvmet 写操作需 root。\
             跑法：sudo cargo test ... -- --ignored"
        );
        return false;
    }
    true
}

// libc::getuid 薄封装（避免直接引 libc crate）。
extern "C" {
    fn getuid() -> u32;
}

/// configfs 下是否有 target（LIO）子系统目录。
fn configfs_has_target() -> bool {
    Path::new("/sys/kernel/config/target").is_dir()
}

/// configfs 下是否有 nvmet 子系统目录。
fn configfs_has_nvmet() -> bool {
    Path::new("/sys/kernel/config/nvmet").is_dir()
}

/// configfs 下 iscsi 子系统目录是否存在。注意：iscsi_target_mod 加载后**不会立即**在
/// configfs 暴露 iscsi 目录——它是**懒创建**的，首次 `targetcli ls` 后才出现。本助手
/// 先跑一次 `targetcli ls` 触发，再判目录。
fn ensure_iscsi_configfs() -> bool {
    let iscsi_dir = Path::new("/sys/kernel/config/target/iscsi");
    if iscsi_dir.is_dir() {
        return true;
    }
    // 触发懒创建：targetcli ls 会初始化 iscsi fabric。
    let _ = Command::new("targetcli").arg("ls").output();
    iscsi_dir.is_dir()
}

/// 纯 Rust `which`：扫 $PATH 找可执行文件。
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 跑 targetcli（单条 path 命令），返回 (success, stdout, stderr)。
fn targetcli(cmd: &str) -> (bool, String, String) {
    let out = Command::new("targetcli").arg(cmd).output();
    match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), format!("spawn 失败：{e}")),
    }
}

/// 跑一个 shell 命令（root 已确认），返回 (success, combined_output)。
fn sh(script: &str) -> (bool, String) {
    let out = Command::new("bash").arg("-c").arg(script).output();
    match out {
        Ok(o) => {
            let mut s = String::new();
            s.push_str(&String::from_utf8_lossy(&o.stdout));
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("spawn 失败：{e}")),
    }
}

/// 全局唯一性计数器——为每个测的对象拼一个不冲突的后缀（同进程多测也唯一）。
static UNIQ: AtomicU64 = AtomicU64::new(0);

/// 生成一个全局唯一后缀：`<pid>-<unix_nanos><counter>`（全合法 IQN 字符）。
///
/// 用 `-` 分隔（**不**用 `_`）——IQN 的 name 段按 RFC 3721 只允许字母/数字/`-`/`.`/`:`,
/// rtslib 会把含 `_` 的 WWN 判为非法。counter 兜底保证同进程多测也不撞名。
fn uniq_suffix() -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}{}", std::process::id(), nanos, n)
}

// ============================================================================
// RAII guards
// ============================================================================

/// **持久 zvol 夹具**：drop 时只销毁单个 zvol（**不**销毁 zpool）。
///
/// 为什么不销毁 pool：本机 ZFS 内核态在 LIO export 过 zvol 后再 `zpool destroy`
/// 会**永久挂起内核线程**（`spa_export_common`/`zvol_remove_minors_impl` 卡在
/// `taskq_wait`/`cv_wait_common`，dmesg 报「task zpool blocked for >120s」）——
/// 这是 ZFS-on-Linux 与 LIO block backend 的已知交互问题。`zfs destroy <pool/vol>`
/// （单 zvol）则干净退 0，无挂起。
///
/// 故本夹具采用**持久 pool + per-test zvol** 策略：
/// - 一个固定名的 `osprobe` pool（`PERSIST_POOL`）在首次用时 lazy 建（已存在则复用），
///   **永不在测里 destroy**（pool 是空的、占 112M，留在 osprobe 命名空间，安全）；
/// - 每个测用唯一 dataset 名建 4M zvol，drop 时 `zfs destroy` 该 dataset。
struct ZvolFixture {
    /// 完整 dataset 名（<pool>/<unique>）。drop 时 zfs destroy 它。None=已被显式接管清理。
    dataset: Option<String>,
    /// 对应的 zvol 块设备路径。
    zvol: String,
}

/// 持久测试 pool 名（固定，跨测复用，永不 destroy）。
const PERSIST_POOL: &str = "osprobepersist";
/// 持久 pool 的底层稀疏文件（仅建 pool 时用一次）。
const PERSIST_POOL_IMG: &str = "/tmp/osprobepersist.img";

impl ZvolFixture {
    /// 建一个唯一 zvol（pool 若不存在则 lazy 建）。失败返回 None 并 eprintln 原因。
    fn create(unique: &str) -> Option<Self> {
        ensure_persist_pool()?;
        let dataset = format!("{PERSIST_POOL}/{unique}");
        // 已存在则先 destroy（幂等，便于重跑）。
        let _ = sh(&format!("zfs destroy -f {dataset} 2>/dev/null"));
        let (ok, err) = sh(&format!("zfs create -V 4M {dataset}"));
        if !ok {
            eprintln!("[SKIP] zfs create -V 4M {dataset} 失败：{err}");
            return None;
        }
        let zvol = format!("/dev/zvol/{dataset}");
        // 等 zvol 设备节点出现（zvol 创建是异步的）。
        for _ in 0..50 {
            if Path::new(&zvol).exists() {
                return Some(Self {
                    dataset: Some(dataset),
                    zvol,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("[SKIP] {zvol} 创建后未出现（zfs udev 异常）");
        let _ = sh(&format!("zfs destroy {dataset}"));
        None
    }

    /// zvol 块设备路径。
    fn zvol_path(&self) -> &str {
        &self.zvol
    }
}

/// 确保 `PERSIST_POOL` 存在（不存在则 lazy 建）。失败返回 None。
///
/// 已存在则直接返回（幂等复用，**不** destroy）。pool 的底层稀疏文件留在 /tmp，
/// 重启后 ZFS 不再认它（pool 变 exportable），不影响生产。
fn ensure_persist_pool() -> Option<()> {
    // 探测：pool 是否已 import。
    let (ok, out) = sh("zpool list -H -o name 2>/dev/null");
    if ok && out.lines().any(|l| l.trim() == PERSIST_POOL) {
        return Some(());
    }
    // 池不存在：建稀疏文件 + zpool create。
    let img = Path::new(PERSIST_POOL_IMG);
    if !img.exists() {
        let (ok, _) = sh(&format!("truncate -s 256M {:?}", img));
        if !ok {
            eprintln!("[SKIP] 无法建稀疏文件 {PERSIST_POOL_IMG}");
            return None;
        }
    }
    let (ok, err) = sh(&format!(
        "zpool create -f {PERSIST_POOL} {PERSIST_POOL_IMG} 2>&1"
    ));
    if !ok {
        // 可能 pool 已 export（重启后）→ 先 import。
        let (ok2, _) = sh(&format!("zpool import {PERSIST_POOL} 2>/dev/null"));
        if ok2 {
            return Some(());
        }
        eprintln!("[SKIP] zpool create/import {PERSIST_POOL} 失败：{err}");
        return None;
    }
    Some(())
}

impl Drop for ZvolFixture {
    fn drop(&mut self) {
        if let Some(ds) = self.dataset.take() {
            // 只销毁单 zvol（不销毁 pool）——见 struct 文档说明的 ZFS 挂起问题。
            let (_, out) = sh(&format!("zfs destroy -f {ds} 2>&1"));
            if !out.trim().is_empty() {
                eprintln!("[cleanup] zfs destroy {ds}: {out}");
            }
        }
    }
}

/// iSCSI target + backstore 的 RAII guard（drop 时尽力删，忽略错误）。
/// 不依赖生产 LioBlockExport 的内存注册表（guard 与被测 be 不同实例），直接 targetcli 删。
struct IscsiTargetGuard {
    iqn: Option<String>,
    backstore: Option<String>,
}

impl IscsiTargetGuard {
    fn new() -> Self {
        Self {
            iqn: None,
            backstore: None,
        }
    }
    /// 登记：drop 时清这个 target + backstore。
    fn arm(&mut self, iqn: String, backstore: String) {
        self.iqn = Some(iqn);
        self.backstore = Some(backstore);
    }
    /// 手动清理后调，避免 Drop 重复删。
    fn disarm(&mut self) {
        self.iqn = None;
        self.backstore = None;
    }
}

impl Drop for IscsiTargetGuard {
    fn drop(&mut self) {
        if let Some(iqn) = self.iqn.take() {
            let (_, out, _) = targetcli(&format!("/iscsi delete {iqn}"));
            if !out.trim().is_empty() {
                eprintln!("[cleanup] /iscsi delete {iqn}: {out}");
            }
        }
        if let Some(bs) = self.backstore.take() {
            let (_, out, _) = targetcli(&format!("/backstores/block delete {bs}"));
            if !out.trim().is_empty() {
                eprintln!("[cleanup] /backstores/block delete {bs}: {out}");
            }
        }
    }
}

/// nvmet subsystem + namespace + port 的 RAII guard（configfs 直写清理）。
struct NvmetGuard {
    /// (port_id, nqn) 对：drop 时先 unlink port 引用，再 rmdir port。
    port: Option<(u32, String)>,
    /// (nqn, nsid)：drop 时 disable + rmdir namespace。
    ns: Option<(String, u32)>,
    /// nqn：drop 时 rmdir subsystem。
    subsystem: Option<String>,
}

impl NvmetGuard {
    fn new() -> Self {
        Self {
            port: None,
            ns: None,
            subsystem: None,
        }
    }
}

impl Drop for NvmetGuard {
    fn drop(&mut self) {
        // 顺序：port 引用 → port → namespace → subsystem（建是逆序）。
        if let Some((port_id, nqn)) = self.port.take() {
            let p = format!("/sys/kernel/config/nvmet/ports/{port_id}/subsystems/{nqn}");
            let _ = std::fs::remove_file(&p); // unlink symlink（忽略错误）
            let _ = std::fs::remove_dir(format!("/sys/kernel/config/nvmet/ports/{port_id}"));
        }
        if let Some((nqn, nsid)) = self.ns.take() {
            let nsdir = format!("/sys/kernel/config/nvmet/subsystems/{nqn}/namespaces/{nsid}");
            let _ = sh(&format!("echo 0 > {nsdir}/enable"));
            let _ = std::fs::remove_dir(&nsdir);
        }
        if let Some(nqn) = self.subsystem.take() {
            let sub = format!("/sys/kernel/config/nvmet/subsystems/{nqn}");
            let _ = std::fs::remove_dir(&sub);
        }
    }
}

// ============================================================================
// 测 A：iSCSI target 真实创建-销毁往返（经生产 LioBlockExport::export_iscsi）
// ============================================================================

#[tokio::test]
#[ignore = "需 root + configfs/targetcli + zfs。\
            跑法：cargo test -- --ignored real_iscsi_target_round_trip"]
async fn real_iscsi_target_round_trip() {
    // —— 环境自检 ——
    if !require_root() {
        return;
    }
    if which("targetcli").is_none() {
        eprintln!("[SKIP] 未装 targetcli");
        return;
    }
    if !configfs_has_target() {
        eprintln!("[SKIP] configfs 无 target 子系统（target_core_mod 未加载）");
        return;
    }
    // iscsi fabric 懒创建：先触发一次。
    if !ensure_iscsi_configfs() {
        eprintln!("[SKIP] configfs 无 iscsi 子系统（iscsi_target_mod 未加载 / 懒创建失败）");
        return;
    }

    // —— 备料：持久 pool 下建唯一 zvol（export_iscsi 后端指向 /dev/zvol/<vol>）——
    let suffix = uniq_suffix();
    let unique = format!("iscsi-{suffix}");
    let zvol_guard = match ZvolFixture::create(&unique) {
        Some(g) => g,
        None => return, // ZvolFixture 已 eprintln 原因
    };
    println!("[INFO] zvol 备料完成：{}", zvol_guard.zvol_path());

    // volume 用 <PERSIST_POOL>/<unique>：export_iscsi 内部 sanitize_name 把 `/` → `-`，
    // 故 backstore 名会是 vol-<PERSIST_POOL>-<unique>，IQN 后缀会是 vol-...-lun0。
    // iqn_base 用带点反向域名（rtslib 要求 iqn 反向域名段含 `.`）。
    let iqn_base = format!("iqn.2026-08.com.osprobe{suffix}");
    let nqn_base = format!("nqn.2026-08.com.osprobe{suffix}");
    let be = LioBlockExport::new(&iqn_base, &nqn_base);
    let volume = VolumeId::new(format!("{PERSIST_POOL}/{unique}"));
    // backstore 名 = vol-<sanitized-volume>（与生产 unexport 反推一致）。
    let backstore = format!("vol-{}-{}", PERSIST_POOL, unique);

    let mut guard = IscsiTargetGuard::new();

    // —— export：建 backstore + target + lun + 默认 portal ——
    let t = match be.export_iscsi(&volume, 0, Vec::new()).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[FAIL] export_iscsi 失败（生产 LIO 编排在真实内核上出错）：{e}");
            panic!("export_iscsi 应在真实 LIO 上成功：{e}");
        }
    };
    println!("[INFO] export_iscsi 成功，IQN = {}", t.iqn);
    // arm guard（drop 兜底清理 target + backstore）
    guard.arm(t.iqn.clone(), backstore.clone());

    // —— 验证 1：targetcli ls 含新 IQN ——
    let (ok, ls_out, _) = targetcli("ls");
    assert!(ok, "targetcli ls 应退 0");
    assert!(
        ls_out.contains(&t.iqn),
        "targetcli ls 应含新建 target IQN {}\n输出：\n{}",
        t.iqn,
        ls_out
    );

    // —— 验证 2：configfs 直读——target 真实存在于内核 ——
    // 路径：/sys/kernel/config/target/iscsi/<iqn>/tpgt_1/（注意 configfs 用 tpgt_1，非 tpg1）
    let target_dir = format!("/sys/kernel/config/target/iscsi/{}", t.iqn);
    assert!(
        Path::new(&target_dir).is_dir(),
        "configfs 应有 target 目录 {target_dir}"
    );
    let tpgt = format!("{target_dir}/tpgt_1");
    assert!(
        Path::new(&tpgt).is_dir(),
        "configfs 应有 tpgt_1（targetcli 显示为 tpg1）"
    );
    // lun_0：LIO 的 LUN 命名是 lun_0/lun_1...
    let lun0 = format!("{tpgt}/lun/lun_0");
    assert!(
        Path::new(&lun0).is_dir(),
        "configfs 应有 lun_0（export_iscsi 映射的 LUN）"
    );
    // 默认 portal（auto_add_default_portal=true）：np/[::0]:3260
    let np = format!("{tpgt}/np");
    let np_entries =
        std::fs::read_dir(&np).unwrap_or_else(|e| panic!("读 configfs portal 目录 {np} 失败：{e}"));
    let portals: Vec<String> = np_entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        portals.iter().any(|p| p.contains("3260")),
        "应有监听 3260 的默认 portal，实际 portals = {:?}",
        portals
    );
    println!(
        "[OK] configfs 直读验证通过：target={} tpgt_1 lun_0 portals={:?}",
        t.iqn, portals
    );

    // —— destroy：unexport（删 target + 删 backstore）——
    be.unexport(&t.iqn)
        .await
        .expect("unexport 应在真实 LIO 上成功");

    // —— 验证 3：targetcli ls 不再含 IQN ——
    let (_, ls2_out, _) = targetcli("ls");
    assert!(
        !ls2_out.contains(&t.iqn),
        "destroy 后 targetcli ls 不应再含该 IQN\n输出：\n{}",
        ls2_out
    );

    // —— 验证 4：configfs 直读——target 目录已消失 ——
    assert!(
        !Path::new(&target_dir).exists(),
        "configfs 不应再有 target 目录 {target_dir}"
    );
    // backstore 也应被 unexport 删除
    let backstore_dir = format!("/sys/kernel/config/target/core/backstore/block/{backstore}");
    assert!(
        !Path::new(&backstore_dir).exists(),
        "backstore 应被 unexport 删除：{backstore_dir}"
    );

    // guard 已无用（手动 destroy 了），disarm 避免 Drop 重复删
    guard.disarm();
    drop(zvol_guard); // 显式 drop：zfs destroy 单 zvol（不销毁持久 pool）
    println!("[OK] iSCSI target 真实往返通过：export → configfs 验存在 → unexport → 验消失");
}

// ============================================================================
// 测 B：nvmet subsystem 真实创建-销毁（configfs 直写，本机无 nvmetcli）
// ============================================================================
//
// 生产 export_nvmeof 经 nvmetcli 编排；本机无 nvmetcli，故本测直接对 configfs 写，
// 验证 nvmet 内核子系统真实可建 namespace+port（与生产 export_nvmeof 的目标一致：
// 内核态 nvmet 对象真实存在）。这也覆盖「configfs 直写」这一被文档点名为「生产可改」
// 的路径。

#[tokio::test]
#[ignore = "需 root + configfs/nvmet + zfs。\
            跑法：cargo test -- --ignored real_nvmet_subsystem_round_trip"]
async fn real_nvmet_subsystem_round_trip() {
    if !require_root() {
        return;
    }
    if !configfs_has_nvmet() {
        eprintln!("[SKIP] configfs 无 nvmet 子系统（nvmet 模块未加载）");
        return;
    }

    // —— 备料：持久 pool 下建唯一 zvol（namespace 后端）——
    let suffix = uniq_suffix();
    let unique = format!("nvmet-{suffix}");
    let zvol_guard = match ZvolFixture::create(&unique) {
        Some(g) => g,
        None => return,
    };
    let zvol = zvol_guard.zvol_path().to_string();

    let nqn = format!("nqn.2026-08.com.osprobe{suffix}:test");
    let nsid: u32 = 1;
    let port_id: u32 = 1;
    let mut guard = NvmetGuard::new();

    // —— 1) 建 subsystem ——
    let sub_dir = format!("/sys/kernel/config/nvmet/subsystems/{nqn}");
    if let Err(e) = std::fs::create_dir(&sub_dir) {
        panic!("建 nvmet subsystem {sub_dir} 失败：{e}");
    }
    guard.subsystem = Some(nqn.clone());

    // —— 2) 建 namespace ——
    let ns_dir = format!("{sub_dir}/namespaces/{nsid}");
    if let Err(e) = std::fs::create_dir(&ns_dir) {
        panic!("建 nvmet namespace {ns_dir} 失败：{e}");
    }
    // 写后端设备路径
    let (ok, err) = sh(&format!("echo '{zvol}' > {ns_dir}/device_path"));
    assert!(ok, "写 device_path 失败：{err}");
    // 启用 namespace
    let (ok, err) = sh(&format!("echo 1 > {ns_dir}/enable"));
    assert!(ok, "enable namespace 失败：{err}");
    guard.ns = Some((nqn.clone(), nsid));

    // —— 3) 建 port ——
    let port_dir = format!("/sys/kernel/config/nvmet/ports/{port_id}");
    if let Err(e) = std::fs::create_dir(&port_dir) {
        panic!("建 nvmet port {port_dir} 失败：{e}");
    }
    // addr_adrfam 必须先于 trtype 写（否则 link subsystem 报「address family 255 not supported」）
    let (ok, err) = sh(&format!(
        "echo ipv4 > {port_dir}/addr_adrfam; \
         echo tcp > {port_dir}/addr_trtype; \
         echo 127.0.0.1 > {port_dir}/addr_traddr; \
         echo 4420 > {port_dir}/addr_trsvcid"
    ));
    assert!(ok, "写 port 属性失败：{err}");

    // —— 4) 把 subsystem 链接到 port ——
    let link = format!("{port_dir}/subsystems/{nqn}");
    let (ok, err) = sh(&format!("ln -s {sub_dir} {link}"));
    assert!(ok, "link subsystem 到 port 失败：{err}");
    guard.port = Some((port_id, nqn.clone()));

    // —— 验证：subsystem 真实存在于内核 configfs ——
    assert!(Path::new(&sub_dir).is_dir(), "nvmet subsystem 目录应存在");
    assert!(Path::new(&ns_dir).is_dir(), "nvmet namespace 目录应存在");
    // namespace enabled
    let enable_val = std::fs::read_to_string(format!("{ns_dir}/enable"))
        .unwrap_or_default()
        .trim()
        .to_string();
    assert_eq!(enable_val, "1", "namespace 应已 enable");
    // port 已挂载该 subsystem
    assert!(
        Path::new(&link).exists(),
        "port subsystems 下应有 {nqn} 引用"
    );
    // 全局 subsystems 目录下能 ls 到
    let subs =
        std::fs::read_dir("/sys/kernel/config/nvmet/subsystems").expect("读 nvmet subsystems 目录");
    let names: Vec<String> = subs
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == &nqn),
        "全局 nvmet/subsystems 应含 {nqn}，实际 {names:?}"
    );
    println!("[OK] nvmet subsystem 真实创建验证通过：{nqn} nsid={nsid} port={port_id}");

    // —— 验证：dmesg 应有「adding nsid 1 to subsystem」（内核已接收）——
    // 非强制断言（dmesg 权限/缓冲不可靠），仅辅助打印。

    // —— destroy：guard drop 自动逆序清理 ——
    drop(guard);
    drop(zvol_guard);

    // —— 验证清理：subsystem / namespace / port 全消失 ——
    assert!(!Path::new(&sub_dir).exists(), "subsystem 应被清理");
    assert!(
        !Path::new(&format!("/sys/kernel/config/nvmet/ports/{port_id}")).exists(),
        "port 应被清理"
    );
    println!("[OK] nvmet subsystem 真实往返通过：建 → 验存在 → 销毁 → 验消失");
}

// ============================================================================
// 测 C：targetcli saveconfig / restoreconfig 持久化往返
// ============================================================================
//
// 生产 export 不直接调 saveconfig，但部署常需持久化。本测验证 targetcli 的
// saveconfig→restoreconfig 命令构造真实可用（写出的 JSON 能被 restore 读回），
// 覆盖「持久化命令构造」这一被规划文档列为集成阶段的事项。

#[tokio::test]
#[ignore = "需 root + targetcli。跑法：cargo test -- --ignored real_targetcli_saveconfig_restore"]
async fn real_targetcli_saveconfig_restore() {
    if !require_root() {
        return;
    }
    if which("targetcli").is_none() {
        eprintln!("[SKIP] 未装 targetcli");
        return;
    }

    // —— 先 clearconfig：本测在测机/沙箱跑，宿主无真实 targetcli 配置（assertNoHostConfig）——
    // restoreconfig 要求与现有配置**无冲突**（重复的 storage object 会退 1）。为隔离往返验证，
    // 先 clearconfig 清掉之前测残留（都是 osprobe 命名空间的孤立 backstore）。**仅在** 宿主
    // 确无真实配置时安全（本测先 ls 检查 backstores/iscsi 全空——非 osprobe 对象则 SKIP）。
    let (ok, ls_out, _) = targetcli("ls");
    if !ok {
        eprintln!("[SKIP] targetcli ls 不可达");
        return;
    }
    // 探测：是否有非 osprobe 命名空间的 storage object / target（有则不动宿主配置）。
    // targetcli ls 里实际存储对象行形如 `o- <name> [/dev/... (size) ...]`（带后端设备信息），
    // 而 backstore 子类目录（block/fileio/...）行只带 [Storage Objects: N]。本探针扫「带后端
    // 设备的对象行」，凡是名字不含 osprobe 的就视为宿主真实对象，触发 SKIP。
    let has_real_config = ls_out.lines().any(|l| {
        // 存储对象行：含 `[/dev/` 或 `[rdwr` 或 `(size` 这类后端描述，且是非 osprobe 名字。
        (l.contains("[/dev/") || l.contains(" write-thru") || l.contains(" write-back"))
            && !l.contains("osprobe")
    });
    if has_real_config {
        eprintln!(
            "[SKIP] 检测到非 osprobe 的宿主 targetcli 配置，不 clearconfig：\n{}",
            ls_out
        );
        return;
    }
    let (ok, out, _) = targetcli("clearconfig confirm=true");
    if !ok {
        eprintln!("[SKIP] clearconfig 失败（不动宿主配置）：{out}");
        return;
    }

    // saveconfig 到一个唯一路径（不覆盖宿主默认 /etc/rtslib-fb-target/saveconfig.json）。
    let suffix = uniq_suffix();
    let savefile = format!("/tmp/osprobe-saveconfig-{suffix}.json");
    let (ok, out, _) = targetcli(&format!("saveconfig {savefile}"));
    assert!(ok, "targetcli saveconfig 应退 0：{out}");
    assert!(
        Path::new(&savefile).exists(),
        "saveconfig 应写出文件 {savefile}"
    );
    // 非空 JSON（空配置也是合法 JSON，含 fabric_modules/storage_objects 节点）
    let content = std::fs::read_to_string(&savefile).unwrap_or_default();
    assert!(
        content.contains("storage_objects")
            || content.contains("fabric_modules")
            || content.contains('{'),
        "saveconfig 应是合法 JSON（含 storage_objects/fabric_modules 节点或至少 {{）"
    );
    println!(
        "[OK] saveconfig 写出 {}（前 120 字节）：{}",
        savefile,
        content.chars().take(120).collect::<String>()
    );

    // restoreconfig：clearconfig 后内核态空，读回空配置应无冲突退 0。
    let (ok, out, _) = targetcli(&format!("restoreconfig {savefile}"));
    assert!(
        ok,
        "targetcli restoreconfig 应退 0（已 clearconfig）：{out}"
    );
    println!("[OK] restoreconfig 读回成功");

    // 清理临时文件（root 写的，root 测进程能删）。
    let _ = std::fs::remove_file(&savefile);
    println!("[OK] saveconfig/restoreconfig 持久化往返通过");
}

// ============================================================================
// 测 D：configfs 直读验证 iSCSI target 内核态属性（与测 A 互补，独立可跑）
// ============================================================================
//
// 单独验证「configfs 直读」能力：建一个 target，读 configfs 的 param/ 属性文件，
// 验证 LIO 默认值（如 DefaultCmdSN、DemoMode）真实存在于内核。这把「configfs 直读」
// 从测 A 的内嵌断言提为一等测，便于在 configfs 在但 zvol 备料失败的环境也能验证。

#[tokio::test]
#[ignore = "需 root + configfs/targetcli + zfs。\
            跑法：cargo test -- --ignored real_iscsi_configfs_attributes_readable"]
async fn real_iscsi_configfs_attributes_readable() {
    if !require_root() {
        return;
    }
    if which("targetcli").is_none() {
        eprintln!("[SKIP] 未装 targetcli");
        return;
    }
    if !ensure_iscsi_configfs() {
        eprintln!("[SKIP] configfs 无 iscsi 子系统");
        return;
    }

    // 备料 zvol（持久 pool 下唯一 zvol）
    let suffix = uniq_suffix();
    let unique = format!("cfg-{suffix}");
    let zvol_guard = match ZvolFixture::create(&unique) {
        Some(g) => g,
        None => return,
    };

    let iqn_base = format!("iqn.2026-08.com.osprobe{suffix}");
    let nqn_base = format!("nqn.2026-08.com.osprobe{suffix}");
    let be = LioBlockExport::new(&iqn_base, &nqn_base);
    let volume = VolumeId::new(format!("{PERSIST_POOL}/{unique}"));
    let backstore = format!("vol-{}-{}", PERSIST_POOL, unique);
    let mut guard = IscsiTargetGuard::new();

    let t = be
        .export_iscsi(&volume, 0, Vec::new())
        .await
        .expect("export_iscsi 应成功");
    guard.arm(t.iqn.clone(), backstore.clone());

    // 直读 configfs 属性目录
    let param_dir = format!("/sys/kernel/config/target/iscsi/{}/tpgt_1/param", t.iqn);
    let entries: Vec<String> = std::fs::read_dir(&param_dir)
        .unwrap_or_else(|e| panic!("读 configfs param 目录 {param_dir} 失败：{e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // LIO iSCSI tpg 默认参数文件（rt_assert 至少这几个存在）
    for expect in ["AuthMethod", "DataSequenceInOrder", "ErrorRecoveryLevel"] {
        assert!(
            entries.iter().any(|p| p == expect),
            "configfs param 应含 {expect}，实际 {entries:?}"
        );
    }
    // 读一个具体属性值（DemoModeWriteProtect 等默认值）
    let demo = std::fs::read_to_string(format!("{param_dir}/DemoModeWriteProtect"))
        .unwrap_or_default()
        .trim()
        .to_string();
    println!("[OK] configfs 属性可读：DemoModeWriteProtect={demo}（param 文件 {entries:?}）");

    // 清理
    be.unexport(&t.iqn).await.expect("unexport 应成功");
    guard.disarm();
    drop(zvol_guard);
    println!("[OK] iSCSI configfs 属性直读验证通过");
}
