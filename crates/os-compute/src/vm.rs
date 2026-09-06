//! 虚拟机（KVM + libvirt）
//!
//! 实现说明（规划文档 §3.4）：
//! - 磁盘后端用 zvol（`VolumeId`），libvirt domain 的 disk 段指向 `/dev/zvol/<pool>/<vol>`
//! - 迁移初版为 active-passive（共享存储 + domain 切换运行节点）

use os_core::{Deserialize, NodeId, Serialize, TaskId, VmId, VolumeId};

use crate::{ComputeError, ComputeResult};

// ----------------------------------------------------------------------------
// VM 状态 / 规格
// ----------------------------------------------------------------------------

/// 虚拟机运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    /// 运行中
    Running,
    /// 已暂停（libvirt suspended）
    Paused,
    /// 已停止（libvirt shut off）
    Stopped,
    /// 失败（libvirt 异常）
    Failed,
    /// 迁移中
    Migrating,
}

impl VmState {
    /// 判断从当前状态到目标状态是否为合法的生命周期转换。
    ///
    /// 合法转换图（与 libvirt domain 状态机一致）：
    /// - `Stopped`（含已定义未启动）→ `Running`（start）
    /// - `Running` → `Paused`（suspend）/ `Stopped`（shutdown）/ `Failed`（异常）/ `Migrating`
    /// - `Paused` → `Running`（resume）/ `Stopped`（强制停止）/ `Failed`
    /// - `Migrating` → `Running`（迁移完成回到源）/ `Stopped`（目标接管）/ `Failed`
    /// - `Failed` → `Stopped`（清理后）/ `Running`（重启恢复）
    /// - 同态转换（`x → x`）恒允许，便于幂等操作。
    pub fn can_transition_to(self, target: VmState) -> bool {
        if self == target {
            return true;
        }
        match self {
            VmState::Stopped => matches!(
                target,
                VmState::Running | VmState::Failed | VmState::Migrating
            ),
            VmState::Running => matches!(
                target,
                VmState::Paused | VmState::Stopped | VmState::Failed | VmState::Migrating
            ),
            VmState::Paused => matches!(
                target,
                VmState::Running | VmState::Stopped | VmState::Failed
            ),
            VmState::Migrating => matches!(
                target,
                VmState::Running | VmState::Stopped | VmState::Failed
            ),
            VmState::Failed => matches!(target, VmState::Stopped | VmState::Running),
        }
    }

    /// 执行状态转换，非法转换返回 `InvalidSpec`。
    pub fn transition_to(self, target: VmState) -> ComputeResult<VmState> {
        if self.can_transition_to(target) {
            Ok(target)
        } else {
            Err(ComputeError::InvalidSpec(format!(
                "非法状态转换: {self:?} -> {target:?}"
            )))
        }
    }
}

/// CPU 拓扑（vcpu 总数 = sockets * cores * threads）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CpuTopology {
    /// vCPU 总数
    pub vcpus: u32,
    /// socket 数
    pub sockets: u32,
    /// 每 socket 核心数
    pub cores: u32,
    /// 每核心线程数
    pub threads: u32,
}

impl CpuTopology {
    /// 构造一个对称拓扑（单 socket、单线程，vcpus = cores）。
    pub fn new(vcpus: u32) -> Self {
        Self {
            vcpus,
            sockets: 1,
            cores: vcpus,
            threads: 1,
        }
    }

    /// 显式指定每维度的拓扑。
    pub fn with_topology(sockets: u32, cores: u32, threads: u32) -> Self {
        let vcpus = sockets.saturating_mul(cores).saturating_mul(threads);
        Self {
            vcpus,
            sockets,
            cores,
            threads,
        }
    }

    /// 校验拓扑自洽：各维度 > 0，且 vcpus == sockets * cores * threads。
    ///
    /// libvirt 要求显式声明拓扑且三者乘积必须等于 vcpu 总数，否则 domain 定义失败。
    pub fn validate(&self) -> ComputeResult<()> {
        if self.vcpus == 0 || self.sockets == 0 || self.cores == 0 || self.threads == 0 {
            return Err(ComputeError::InvalidSpec(
                "CPU 拓扑各维度（vcpus/sockets/cores/threads）必须 > 0".into(),
            ));
        }
        let product = self
            .sockets
            .checked_mul(self.cores)
            .and_then(|v| v.checked_mul(self.threads))
            .ok_or_else(|| ComputeError::InvalidSpec("CPU 拓扑乘积溢出".into()))?;
        if product != self.vcpus {
            return Err(ComputeError::InvalidSpec(format!(
                "CPU 拓扑不自洽：vcpus={} 但 sockets*cores*threads={}",
                self.vcpus, product
            )));
        }
        Ok(())
    }
}

/// 网卡模型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NicModel {
    /// virtio-net（高性能，推荐）
    Virtio,
    /// Intel e1000（兼容性好，性能较低）
    E1000,
}

/// 虚拟机网卡
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmNic {
    /// 接入的桥（如 `br0`）
    pub bridge: String,
    /// MAC 地址（None = 自动生成）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// 网卡模型
    pub model: NicModel,
}

impl VmNic {
    /// 构造一张 virtio 网卡（推荐默认）。
    pub fn virtio(bridge: impl Into<String>) -> Self {
        Self {
            bridge: bridge.into(),
            mac: None,
            model: NicModel::Virtio,
        }
    }

    /// 校验：桥名非空；若指定 MAC 则格式合法（IEEE 802 十六进制，大小写不敏感）。
    pub fn validate(&self) -> ComputeResult<()> {
        if self.bridge.trim().is_empty() {
            return Err(ComputeError::InvalidSpec("网卡 bridge 不能为空".into()));
        }
        if let Some(mac) = &self.mac {
            if !is_valid_mac(mac) {
                return Err(ComputeError::InvalidSpec(format!(
                    "MAC 地址格式非法: {mac}"
                )));
            }
        }
        Ok(())
    }
}

/// 简易 MAC 地址校验：6 组两位十六进制，以 `:` 分隔（不引入 regex crate）。
fn is_valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.as_bytes().iter().all(|b| b.is_ascii_hexdigit()))
}

/// 固件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmFirmware {
    /// 传统 BIOS
    Bios,
    /// UEFI（OVMF）
    Uefi,
}

/// 虚拟机规格（创建时声明）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    /// CPU 拓扑
    pub cpus: CpuTopology,
    /// 内存（MB）
    pub memory_mb: u64,
    /// 系统盘 zvol 卷 ID
    pub disk_vol_id: VolumeId,
    /// 网卡列表
    pub nics: Vec<VmNic>,
    /// 固件
    pub firmware: VmFirmware,
}

/// 内存上限（4 TiB），防止误传 MB/字节单位导致的规格失控。
pub const MAX_VM_MEMORY_MB: u64 = 4 * 1024 * 1024;

impl VmSpec {
    /// 校验规格：CPU 拓扑自洽、内存在合理区间、每张网卡合法。
    pub fn validate(&self) -> ComputeResult<()> {
        self.cpus.validate()?;
        if self.memory_mb == 0 {
            return Err(ComputeError::InvalidSpec("memory_mb 必须 > 0".into()));
        }
        if self.memory_mb > MAX_VM_MEMORY_MB {
            return Err(ComputeError::InvalidSpec(format!(
                "memory_mb={} 超过上限 {} MB",
                self.memory_mb, MAX_VM_MEMORY_MB
            )));
        }
        if self.nics.is_empty() {
            return Err(ComputeError::InvalidSpec("至少需要一张网卡".into()));
        }
        for (i, nic) in self.nics.iter().enumerate() {
            nic.validate()
                .map_err(|e| ComputeError::InvalidSpec(format!("网卡[{i}] 非法: {e}")))?;
        }
        Ok(())
    }

    /// 渲染 libvirt domain XML（字符串构造，不依赖运行时）。
    ///
    /// 生成符合 libvirt schema 的 `<domain>` 文档：内存/KiB、vCPU 拓扑、
    /// 系统盘指向 zvol 块设备（`/dev/zvol/<vol>`）、网卡桥接、固件 UEFI 时
    /// 指向 OVMF。调用前应已通过 [`VmSpec::validate`]。
    pub fn to_libvirt_xml(&self, id: &VmId, name: &str) -> ComputeResult<String> {
        self.validate()?;
        Ok(render_domain_xml(id, name, self))
    }
}

/// 虚拟机实例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vm {
    /// VM ID
    pub id: VmId,
    /// 名称
    pub name: String,
    /// 规格
    pub spec: VmSpec,
    /// 运行状态
    pub state: VmState,
    /// 运行所在节点（迁移时会变化；None = 未调度）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<NodeId>,
    /// 创建时间
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Vm {
    /// 构造一个新定义的 VM（domain 已定义但未启动，状态 = Stopped，未调度节点）。
    pub fn new_defined(id: VmId, name: impl Into<String>, spec: VmSpec) -> Self {
        Self {
            id,
            name: name.into(),
            spec,
            state: VmState::Stopped,
            node_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// 在当前节点上启动该 VM（原地修改状态）。
    pub fn start(&mut self, node: NodeId) -> ComputeResult<()> {
        self.state = self.state.transition_to(VmState::Running)?;
        self.node_id = Some(node);
        Ok(())
    }

    /// 停止该 VM（原地修改状态；保留上次所在节点记录）。
    pub fn stop(&mut self) -> ComputeResult<()> {
        self.state = self.state.transition_to(VmState::Stopped)?;
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// libvirt domain XML 渲染（纯字符串构造，不依赖运行时）
// ----------------------------------------------------------------------------

/// 将 zvol `VolumeId` 映射到块设备路径。
///
/// zvol 在宿主机上以 `/dev/zvol/<pool>/<name>` 暴露为块设备；`VolumeId` 的字符串
/// 形态即 zvol 全名（含 pool 层级，如 `tank/vm/foo`），故直接拼接。
pub fn zvol_device_path(vol_id: &VolumeId) -> String {
    format!("/dev/zvol/{}", vol_id.as_str())
}

/// libvirt domain XML 转义（仅处理会破坏属性值的字符）。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 渲染单张网卡的 `<interface>` 段。
fn render_interface(nic: &VmNic) -> String {
    let model = match nic.model {
        NicModel::Virtio => "virtio",
        NicModel::E1000 => "e1000",
    };
    let mac = nic
        .mac
        .as_ref()
        .map(|m| format!("<mac address='{}'/>", xml_escape(m)))
        .unwrap_or_default();
    format!(
        "    <interface type='bridge'>\n      <source bridge='{}'/>\n      {mac}<model type='{model}'/>\n    </interface>",
        xml_escape(&nic.bridge)
    )
}

/// 渲染完整 libvirt domain XML 文档。
///
/// 结构（关键决策注释）：
/// - `type='kvm'`：本系统仅支持 KVM 加速
/// - `<memory unit='KiB'>`：libvirt 以 KiB 为单位
/// - `<vcpu>` + `<cpu><topology>`：显式声明 vCPU 拓扑，与 `CpuTopology::validate` 自洽
/// - `<disk type='block'>`：系统盘直接走 zvol 块设备（避免 qcow2 中间层，性能优先）
/// - `<interface type='bridge'>`：桥接网络（由 network-agent 管理底层桥）
/// - UEFI 固件：注入 `<os><loader>` 指向 OVMF，BIOS 则省略（libvirt 默认 SeaBIOS）
fn render_domain_xml(id: &VmId, name: &str, spec: &VmSpec) -> String {
    let mem_kib = spec.memory_mb.saturating_mul(1024);
    let disk_path = zvol_device_path(&spec.disk_vol_id);
    let interfaces: Vec<String> = spec.nics.iter().map(render_interface).collect();
    let interfaces_xml = interfaces.join("\n    ");

    let os_loader = match spec.firmware {
        VmFirmware::Uefi => {
            // OVMF 固件路径（Debian/Ubuntu 常见位置）；readonly='yes' + type='pflash'
            "      <loader readonly='yes' type='pflash'>/usr/share/OVMF/OVMF_CODE.fd</loader>\n"
        }
        VmFirmware::Bios => "",
    };

    format!(
        "<domain type='kvm'>\n\
         \x20  <name>{name}</name>\n\
         \x20  <uuid>{uuid}</uuid>\n\
         \x20  <memory unit='KiB'>{mem_kib}</memory>\n\
         \x20  <currentMemory unit='KiB'>{mem_kib}</currentMemory>\n\
         \x20  <vcpu placement='static'>{vcpus}</vcpu>\n\
         \x20  <os>\n\
         {os_loader}\
         \x20    <type arch='x86_64' machine='q35'>hvm</type>\n\
         \x20    <boot dev='hd'/>\n\
         \x20  </os>\n\
         \x20  <features>\n\
         \x20    <acpi/><apic/>\n\
         \x20  </features>\n\
         \x20  <cpu mode='host-passthrough' check='none'>\n\
         \x20    <topology sockets='{sockets}' cores='{cores}' threads='{threads}'/>\n\
         \x20  </cpu>\n\
         \x20  <devices>\n\
         \x20    <emulator>/usr/bin/qemu-system-x86_64</emulator>\n\
         \x20    <disk type='block' device='disk'>\n\
         \x20      <driver name='qemu' type='raw' cache='none' io='native'/>\n\
         \x20      <source dev='{disk_path}'/>\n\
         \x20      <target dev='vda' bus='virtio'/>\n\
         \x20    </disk>\n\
         \x20    {interfaces_xml}\n\
         \x20    <graphics type='vnc' port='-1' autoport='yes' listen='0.0.0.0'/>\n\
         \x20    <video>\n\
         \x20      <model type='virtio'/>\n\
         \x20    </video>\n\
         \x20  </devices>\n\
         </domain>",
        name = xml_escape(name),
        uuid = xml_escape(id.as_str()),
        vcpus = spec.cpus.vcpus,
        sockets = spec.cpus.sockets,
        cores = spec.cpus.cores,
        threads = spec.cpus.threads,
    )
}

// ----------------------------------------------------------------------------
// VmManager trait（async，编排 libvirt）
// ----------------------------------------------------------------------------

/// 虚拟机管理器——编排 libvirt。
///
/// 迁移返回 `TaskId`（异步任务，可由 osd 追踪进度），初版 active-passive。
#[allow(async_fn_in_trait)]
pub trait VmManager: Send + Sync {
    /// 创建虚拟机（定义 libvirt domain，不自动启动）。
    async fn create_vm(&self, id: &VmId, name: &str, spec: VmSpec) -> ComputeResult<Vm>;

    /// 销毁虚拟机（undefine，删除 domain 定义）。
    async fn destroy_vm(&self, id: &VmId) -> ComputeResult<()>;

    /// 启动虚拟机。
    async fn start_vm(&self, id: &VmId) -> ComputeResult<Vm>;

    /// 停止虚拟机（force=true 强制断电）。
    async fn stop_vm(&self, id: &VmId, force: bool) -> ComputeResult<Vm>;

    /// 暂停虚拟机（suspend）。
    async fn pause_vm(&self, id: &VmId) -> ComputeResult<Vm>;

    /// 恢复虚拟机（resume）。
    async fn resume_vm(&self, id: &VmId) -> ComputeResult<Vm>;

    /// 查询单个虚拟机。
    async fn get_vm(&self, id: &VmId) -> ComputeResult<Vm>;

    /// 列出所有虚拟机。
    async fn list_vms(&self) -> ComputeResult<Vec<Vm>>;

    /// 迁移虚拟机到目标节点（异步任务，返回 TaskId）。
    async fn migrate_vm(&self, id: &VmId, target_node: &NodeId) -> ComputeResult<TaskId>;
}

#[cfg(test)]
mod tests;
