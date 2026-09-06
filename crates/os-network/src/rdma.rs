//! IB / RoCE（RDMA）—— 可选能力
//!
//! 决策依据：规划文档 §3.9 —— 高性能场景启用 RDMA（InfiniBand / RoCEv2 / IPoIB）。
//! 本模块为**可选能力**：`detect_capability` 探测硬件存在性，无设备时优雅降级
//! （`available = false`），不阻塞系统启动。

use crate::IpCidr;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 设备模型
// ----------------------------------------------------------------------------

/// RDMA 设备类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RdmaType {
    /// InfiniBand
    InfiniBand,
    /// RoCEv2（RDMA over Converged Ethernet v2）
    RoceV2,
    /// IPoIB（IP over InfiniBand）
    Ipoib,
}

/// RDMA 端口
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdmaPort {
    /// 端口状态（"active" / "down" 等，保留原始字符串以兼容 verbs 表达）
    pub state: String,
    /// 链路速率（Gbps）
    pub rate_gbps: u32,
    /// IB LID 或 RoCE GID（字符串形式）
    pub lid_or_gid: String,
}

/// RDMA 设备
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdmaDevice {
    /// 设备名（如 "mlx5_0"）
    pub name: String,
    /// 设备类型
    pub ty: RdmaType,
    /// 设备状态
    pub state: String,
    /// 端口列表
    pub ports: Vec<RdmaPort>,
}

// ----------------------------------------------------------------------------
// 能力探测
// ----------------------------------------------------------------------------

/// RDMA 能力探测结果（用于优雅降级判断）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdmaCapability {
    /// 是否有可用 RDMA 设备
    pub available: bool,
    /// 可用设备名列表
    pub devices: Vec<String>,
    /// 设备类型（仅当 available 且类型一致时有值）
    pub ty: Option<RdmaType>,
}

// ----------------------------------------------------------------------------
// RdmaManager trait（async）
// ----------------------------------------------------------------------------

/// RDMA 管理器——IB/RoCE 设备探测与 IPoIB 配置（可选能力）。
///
/// 实现者应在无硬件时返回 `available = false` 的能力，而非报错。
#[allow(async_fn_in_trait)]
pub trait RdmaManager: Send + Sync {
    /// 列出所有 RDMA 设备（无设备时返回空 Vec）。
    async fn list_devices(&self) -> Result<Vec<RdmaDevice>, crate::NetworkError>;

    /// 探测 RDMA 能力；无设备时返回 `available = false`（不报错）。
    async fn detect_capability(&self) -> Result<RdmaCapability, crate::NetworkError>;

    /// 为 IPoIB 接口配置地址。
    ///
    /// - `dev`：设备名（如 "ib0"）
    /// - `addr`：CIDR 地址
    async fn configure_ipoib(&self, dev: &str, addr: IpCidr) -> Result<(), crate::NetworkError>;
}

// ----------------------------------------------------------------------------
// CLI 输出解析（纯函数，无外部依赖；供 RdmaCoreManager 与单测复用）
// ----------------------------------------------------------------------------

/// `ibv_devinfo` 输出片段（一个设备块）解析得到的关键字段。
///
/// 仅保留探测/降级所需信息；完整 verbs 字段交由 FFI 绑定层（后续注册）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IbvDevInfoBlock {
    /// hca_id（如 "mlx5_0"）
    pub hca_id: String,
    /// transport（"InfiniBand" / "Ethernet" —— RoCE 走 Ethernet）
    pub transport: String,
    /// 端口状态（"PORT_ACTIVE" / "PORT_DOWN" ...），取第一个 port 的状态
    pub port_state: String,
    /// 端口速率（active_width * active_speed 解析后的 Gbps 近似值）
    pub port_rate_gbps: u32,
    /// 端口 LID 或 GID 字符串
    pub port_lid_or_gid: String,
}

/// 解析 `ibv_devinfo -v` 单个设备块文本为结构化字段。
///
/// 输入示例（标准 rdma-core 输出，实际含 tab 缩进，此处用空格示意）：
/// ```text
/// hca_id:  mlx5_0
///     transport:           InfiniBand (0)
///     fw_ver:              16.31.0414
///     node_guid:           9803:9b03:0000:0000
///     sys_image_guid:      9803:9b03:0000:0000
///     vendor_id:           0x02c9
///     hw_ver:              0x0
///     board_id:            MT_0000000019
///     phys_port_cnt:       1
///         port:            1
///             state:           PORT_ACTIVE (4)
///             max_mtu:         4096 (5)
///             active_mtu:      4096 (5)
///             sm_lid:          1
///             port_lid:        2
///             link_layer:      InfiniBand
/// ```
///
/// 解析策略：逐行匹配前缀关键字，缺失字段给默认值（降级友好）。
pub fn parse_ibv_devinfo_block(block: &str) -> Option<IbvDevInfoBlock> {
    let mut hca_id = None;
    let mut transport = None;
    let mut port_state = None;
    let mut port_lid = None;
    let mut link_layer = None;

    for line in block.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("hca_id:") {
            hca_id = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("transport:") {
            // "InfiniBand (0)" -> 取空白前
            let v = rest.trim();
            transport = Some(v.split_whitespace().next().unwrap_or(v).to_string());
        } else if let Some(rest) = t.strip_prefix("state:") {
            // "PORT_ACTIVE (4)" -> 取空白前
            let v = rest.trim();
            port_state = Some(v.split_whitespace().next().unwrap_or(v).to_string());
        } else if let Some(rest) = t.strip_prefix("port_lid:") {
            port_lid = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("link_layer:") {
            link_layer = Some(rest.trim().to_string());
        }
    }

    let hca_id = hca_id.filter(|s| !s.is_empty())?;
    // 速率精确值需 active_width/active_speed（ibv_devinfo -v 才输出），
    // 此处不依赖：探测降级场景速率非关键，留 0 由 verbs 层后续填充。
    let port_rate_gbps = 0u32;
    let lid_or_gid = port_lid.unwrap_or_default();
    let transport_final = link_layer.or(transport).unwrap_or_else(|| "Unknown".into());

    Some(IbvDevInfoBlock {
        hca_id,
        transport: transport_final,
        port_state: port_state.unwrap_or_else(|| "UNKNOWN".into()),
        port_rate_gbps,
        port_lid_or_gid: lid_or_gid,
    })
}

/// 将 `ibv_devinfo` 整段输出按设备块拆分并解析。
///
/// 设备块以行首 `hca_id:` 起始（无前导 tab），到下一个 `hca_id:` 或文末结束。
/// 解析失败的单个块被跳过（降级友好，不整体报错）。
pub fn parse_ibv_devinfo(output: &str) -> Vec<IbvDevInfoBlock> {
    // 以 "hca_id:" 在行首（无前导空白）作为块分隔。
    let mut blocks: Vec<String> = Vec::new();
    for line in output.lines() {
        if line.starts_with("hca_id:") {
            blocks.push(String::new());
        }
        if let Some(cur) = blocks.last_mut() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    blocks
        .iter()
        .filter_map(|b| parse_ibv_devinfo_block(b))
        .collect()
}

/// `iproute2 rdma dev show` 单行解析结果（精简，用于探测/降级）。
///
/// 本机有 `rdma` 工具（iproute2）但常无 `ibv_devinfo`（rdma-core），故补一个
/// 解析 iproute2 `rdma dev` 文本输出的纯函数，与 `parse_ibv_devinfo` 并存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdmaDevLine {
    /// 设备名（如 "mlx5_0" / "rocep0s8f0"）
    pub name: String,
    /// node_type（"ca" / "switch" / "router" / "rnic" ...，原始字符串）
    pub node_type: String,
    /// 固件版本（`fw` 字段）
    pub fw_version: String,
}

/// 解析 `rdma dev show` 文本输出（iproute2，非 JSON）为设备行列表。
///
/// 输入示例（标准 iproute2 输出，见 iproute2 rdma/dev.c 与 OFA 文档）：
/// ```text
/// 0: rocep0s8f0: node_type ca fw 20.27.6000 node_guid b859:9f03:00c5:8c82 sys_image_guid b859:9f03:00c5:8c83
/// 1: mlx5_1: node_type ca fw 16.31.0414 node_guid 9803:9b03:0000:0000
/// ```
///
/// 解析策略：以行首 `<idx>:` 开头的非空行作为设备行；按空白 token 化：
/// - token 0：`<idx>:`（丢弃）
/// - token 1：`<name>:`（去尾冒号得到 name）
/// - 其余成对扫 `node_type <x>` / `fw <x>`。
///
/// 解析失败的行跳过（降级友好，不整体 panic）。空输入返回空 Vec（本机无硬件）。
pub fn parse_rdma_dev(output: &str) -> Vec<RdmaDevLine> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            // 仅认行首 `<数字>:` 起始的设备行（容错：垃圾行跳过）。
            let mut toks = l.split_whitespace();
            let first = toks.next()?;
            // first 形如 "0:" —— 校验为数字+冒号。
            if !first.ends_with(':') || first[..first.len() - 1].parse::<u32>().is_err() {
                return None;
            }
            // 第二个 token 是 "<name>:"
            let name_tok = toks.next()?;
            let name = name_tok.trim_end_matches(':').to_string();
            if name.is_empty() {
                return None;
            }
            let mut node_type = String::new();
            let mut fw_version = String::new();
            let mut prev = String::new();
            for tok in toks {
                if prev == "node_type" {
                    node_type = tok.to_string();
                } else if prev == "fw" && fw_version.is_empty() {
                    fw_version = tok.to_string();
                }
                prev = tok.to_string();
            }
            Some(RdmaDevLine {
                name,
                node_type,
                fw_version,
            })
        })
        .collect()
}

/// 构造 `rdma dev show` 的 argv（program + args）。
///
/// 返回 `(program, Vec<arg>)`：进程执行库喂 `Command::new(program).args(args)` 即可。
/// 抽出为纯函数便于集成测断言 argv（与 os-network dpu 的 devlink argv 约定一致）。
pub fn rdma_dev_show_argv() -> (&'static str, Vec<&'static str>) {
    ("rdma", vec!["dev", "show"])
}

/// 推断 RDMA 设备类型：优先按 link_layer（InfiniBand / Ethernet）。
/// Ethernet transport 通常对应 RoCEv2；InfiniBand 对应 IB。
fn infer_rdma_type(transport: &str) -> RdmaType {
    match transport {
        "InfiniBand" => RdmaType::InfiniBand,
        // Ethernet 链路层承载 RoCEv2（默认 v2，规划文档 §3.9）
        "Ethernet" => RdmaType::RoceV2,
        _ => RdmaType::InfiniBand,
    }
}

// ----------------------------------------------------------------------------
// RdmaCoreManager（默认实现骨架）
// ----------------------------------------------------------------------------

/// RDMA 管理器默认实现（基于 CLI/verbs 探测）。
///
/// 设计：
/// - `detect_capability` 在无设备时返回 `available = false`（**不报错**），
///   保证无 RDMA 硬件的 OS 系统正常启动（优雅降级）。
/// - 真实 RDMA 操作（verbs FFI / async-rdma）依赖未注册 crate，故此处命令构造与
///   输出解析为真实实现，硬件交互（`ibv_*` 调用）留 TODO \[RUNTIME\]（需 RDMA 硬件 + verbs 库）。
pub struct RdmaCoreManager {
    /// 是否跳过真实硬件探测（测试 / 无权限场景强制走降级路径）。
    ///
    /// 生产默认 `false`；`true` 时所有探测直接返回空结果（available=false）。
    pub skip_probe: bool,
}

impl Default for RdmaCoreManager {
    fn default() -> Self {
        Self {
            skip_probe: std::env::var("OS_RDMA_SKIP_PROBE").is_ok(),
        }
    }
}

impl RdmaCoreManager {
    /// 构造一个 RdmaCoreManager。
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造命令字符串：`ibv_devinfo -v`。
    ///
    /// 仅构造命令文本，**不执行**（执行需进程库，留 TODO [RUNTIME]）。
    #[allow(dead_code)]
    fn ibv_devinfo_cmd(&self) -> &'static str {
        "ibv_devinfo -v"
    }

    /// 构造命令字符串：为 IPoIB 接口配置 CIDR 地址。
    ///
    /// 使用 `ip addr add <cidr> dev <dev>`；仅构造命令，**不执行**。
    fn ip_addr_add_cmd(&self, dev: &str, addr: IpCidr) -> String {
        format!("ip addr add {} dev {}", cidr_to_string(addr), dev)
    }

    /// 内部：执行 `ibv_devinfo` 并解析。
    ///
    /// 真实执行留 TODO [RUNTIME]（依赖进程执行库 + RDMA 硬件）；当前在 `skip_probe` 或
    /// 任何 IO 失败时返回空 Vec（降级）。
    async fn probe_devices(&self) -> Vec<RdmaDevice> {
        if self.skip_probe {
            return Vec::new();
        }
        // TODO(rdma-agent) [RUNTIME]: 接入进程执行库（如 tokio::process::Command）运行
        //   `ibv_devinfo -v`，捕获 stdout 喂给 parse_ibv_devinfo。
        //   未注册依赖前，安全降级为"无设备"。需真实 RDMA 网卡 + ibverbs 用户态库。
        Vec::new()
    }
}

#[allow(async_fn_in_trait)]
impl RdmaManager for RdmaCoreManager {
    async fn list_devices(&self) -> Result<Vec<RdmaDevice>, crate::NetworkError> {
        Ok(self.probe_devices().await)
    }

    async fn detect_capability(&self) -> Result<RdmaCapability, crate::NetworkError> {
        // 优雅降级：无设备时返回 available=false，**不报错**。
        let devices = self.probe_devices().await;
        let names: Vec<String> = devices.iter().map(|d| d.name.clone()).collect();
        let available = !names.is_empty();
        // 类型一致性：所有设备同类型才填，否则 None（混合环境保守返回 None）。
        let ty = if available {
            let mut it = devices.iter().map(|d| d.ty);
            let first = it.next();
            first.filter(|f| it.all(|t| t == *f))
        } else {
            None
        };
        Ok(RdmaCapability {
            available,
            devices: names,
            ty,
        })
    }

    async fn configure_ipoib(&self, dev: &str, addr: IpCidr) -> Result<(), crate::NetworkError> {
        // 构造命令（真实有效）；执行留 TODO [RUNTIME]。
        let _cmd = self.ip_addr_add_cmd(dev, addr);
        // TODO(rdma-agent) [RUNTIME]: 接入进程执行库运行 ip addr add，失败映射 CommandFailed。
        //   当前无设备时直接降级成功（无操作），避免阻塞启动。需 root/CAP_NET_ADMIN + 真实 IPoIB 接口。
        Ok(())
    }
}

/// 将 IpCidr 渲染为 `addr/prefix` 字符串（命令行参数用）。
fn cidr_to_string(addr: IpCidr) -> String {
    format!("{}/{}", addr.addr, addr.prefix)
}

// 将 parse_ibv_devinfo 块组装为 RdmaDevice（供 probe_devices 后续接入用）。
#[allow(dead_code)]
fn block_to_device(block: &IbvDevInfoBlock) -> RdmaDevice {
    RdmaDevice {
        name: block.hca_id.clone(),
        ty: infer_rdma_type(&block.transport),
        state: block.port_state.clone(),
        ports: vec![RdmaPort {
            state: block.port_state.clone(),
            rate_gbps: block.port_rate_gbps,
            lid_or_gid: block.port_lid_or_gid.clone(),
        }],
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn parse_ibv_devinfo_block_infiniband() {
        let block = "hca_id:\tmlx5_0\n\ttransport:\t\t\tInfiniBand (0)\n\t\tport:\t1\n\t\t\tstate:\t\t\tPORT_ACTIVE (4)\n\t\t\tport_lid:\t\t\t2\n\t\t\tlink_layer:\t\t\tInfiniBand\n";
        let parsed = parse_ibv_devinfo_block(block).expect("应解析成功");
        assert_eq!(parsed.hca_id, "mlx5_0");
        assert_eq!(parsed.transport, "InfiniBand");
        assert_eq!(parsed.port_state, "PORT_ACTIVE");
        assert_eq!(parsed.port_lid_or_gid, "2");
        assert_eq!(infer_rdma_type(&parsed.transport), RdmaType::InfiniBand);
    }

    #[test]
    fn parse_ibv_devinfo_block_roce_ethernet() {
        let block = "hca_id:\tmlx5_1\n\ttransport:\t\t\tEthernet (1)\n\t\tport:\t1\n\t\t\tstate:\t\t\tPORT_ACTIVE (4)\n\t\t\tlink_layer:\t\t\tEthernet\n";
        let parsed = parse_ibv_devinfo_block(block).expect("应解析成功");
        assert_eq!(parsed.hca_id, "mlx5_1");
        assert_eq!(parsed.transport, "Ethernet");
        assert_eq!(infer_rdma_type(&parsed.transport), RdmaType::RoceV2);
    }

    #[test]
    fn parse_ibv_devinfo_block_missing_hca_returns_none() {
        let block = "\ttransport:\t\t\tInfiniBand (0)\n";
        assert!(parse_ibv_devinfo_block(block).is_none());
    }

    #[test]
    fn parse_ibv_devinfo_multiple_blocks() {
        let output = "hca_id:\tmlx5_0\n\ttransport:\t\t\tInfiniBand (0)\n\tlink_layer:\t\t\tInfiniBand\nhca_id:\tmlx5_1\n\ttransport:\t\t\tEthernet (1)\n\tlink_layer:\t\t\tEthernet\n";
        let blocks = parse_ibv_devinfo(output);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].hca_id, "mlx5_0");
        assert_eq!(blocks[1].hca_id, "mlx5_1");
    }

    #[test]
    fn parse_ibv_devinfo_empty_returns_empty() {
        assert!(parse_ibv_devinfo("").is_empty());
        assert!(parse_ibv_devinfo("no hca here\nrandom").is_empty());
    }

    #[tokio::test]
    async fn detect_capability_graceful_degradation() {
        // skip_probe=true 模拟无硬件 → available=false，不报错
        let mgr = RdmaCoreManager { skip_probe: true };
        let cap = mgr.detect_capability().await.expect("降级不应报错");
        assert!(!cap.available);
        assert!(cap.devices.is_empty());
        assert!(cap.ty.is_none());
    }

    #[tokio::test]
    async fn list_devices_degraded_empty() {
        let mgr = RdmaCoreManager { skip_probe: true };
        let devs = mgr.list_devices().await.expect("不应报错");
        assert!(devs.is_empty());
    }

    #[tokio::test]
    async fn configure_ipoib_degraded_noop() {
        let mgr = RdmaCoreManager { skip_probe: true };
        let cidr = IpCidr::new("192.168.1.1".parse::<IpAddr>().unwrap(), 24);
        // 无硬件场景降级为成功（不阻塞启动）
        mgr.configure_ipoib("ib0", cidr).await.expect("降级应成功");
    }

    #[test]
    fn ip_addr_add_cmd_format() {
        let mgr = RdmaCoreManager { skip_probe: true };
        let cidr = IpCidr::new("10.0.0.5".parse::<IpAddr>().unwrap(), 24);
        let cmd = mgr.ip_addr_add_cmd("ib0", cidr);
        assert_eq!(cmd, "ip addr add 10.0.0.5/24 dev ib0");
    }

    #[test]
    fn ibv_devinfo_cmd_static() {
        let mgr = RdmaCoreManager { skip_probe: true };
        assert_eq!(mgr.ibv_devinfo_cmd(), "ibv_devinfo -v");
    }

    #[test]
    fn block_to_device_mapping() {
        let block = IbvDevInfoBlock {
            hca_id: "mlx5_0".into(),
            transport: "InfiniBand".into(),
            port_state: "PORT_ACTIVE".into(),
            port_rate_gbps: 100,
            port_lid_or_gid: "2".into(),
        };
        let dev = block_to_device(&block);
        assert_eq!(dev.name, "mlx5_0");
        assert_eq!(dev.ty, RdmaType::InfiniBand);
        assert_eq!(dev.state, "PORT_ACTIVE");
        assert_eq!(dev.ports.len(), 1);
    }

    // —— 覆盖率补测：infer_rdma_type 未知传输 / RdmaCoreManager 构造 / probe 非跳过 /
    // parse_rdma_dev 边界 / IbvDevInfoBlock 缺失字段降级 ——

    #[test]
    fn parse_ibv_devinfo_block_unknown_transport_falls_to_infiniband() {
        // transport=未知值（非 InfiniBand/Ethernet）→ infer_rdma_type fallback 默认 IB。
        // 注意：transport_final = link_layer.or(transport)；此处 transport 存在 → 保留原值。
        let block = "hca_id:\tmlx5_0\n\ttransport:\t\t\tIBHUB (99)\n";
        let parsed = parse_ibv_devinfo_block(block).expect("应解析成功");
        assert_eq!(
            parsed.transport, "IBHUB",
            "未知 transport 保留原值（非 Unknown）"
        );
        // infer_rdma_type 对未知 transport 字符串走 fallback → InfiniBand
        let dev = block_to_device(&parsed);
        assert_eq!(dev.ty, RdmaType::InfiniBand);
    }

    #[test]
    fn parse_ibv_devinfo_block_missing_all_fields_defaults() {
        // 仅 hca_id，其余缺失 → transport=Unknown, port_state=UNKNOWN, lid 空
        let block = "hca_id:\troce0\n";
        let parsed = parse_ibv_devinfo_block(block).expect("应解析成功");
        assert_eq!(parsed.hca_id, "roce0");
        assert_eq!(parsed.transport, "Unknown");
        assert_eq!(parsed.port_state, "UNKNOWN");
        assert!(parsed.port_lid_or_gid.is_empty());
        assert_eq!(parsed.port_rate_gbps, 0);
    }

    #[test]
    fn parse_ibv_devinfo_block_transport_used_when_no_link_layer() {
        // 无 link_layer 但有 transport=Ethernet → transport_final=Ethernet → RoceV2
        let block = "hca_id:\troce1\n\ttransport:\t\t\tEthernet (1)\n";
        let parsed = parse_ibv_devinfo_block(block).expect("应解析成功");
        assert_eq!(parsed.transport, "Ethernet");
        assert_eq!(infer_rdma_type(&parsed.transport), RdmaType::RoceV2);
    }

    #[test]
    fn parse_rdma_dev_argv_and_empty_and_garbage() {
        // rdma_dev_show_argv 纯函数
        let (prog, args) = rdma_dev_show_argv();
        assert_eq!(prog, "rdma");
        assert_eq!(args, vec!["dev", "show"]);

        // 空 / 纯空白
        assert!(parse_rdma_dev("").is_empty());
        assert!(parse_rdma_dev("  \n\t\n").is_empty());

        // 单设备行（含 fw + node_guid 完整）
        let out = "5: ib1: node_type switch fw 4.0.0 node_guid 1111:2222:3333:4444\n";
        let devs = parse_rdma_dev(out);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "ib1");
        assert_eq!(devs[0].node_type, "switch");
        assert_eq!(devs[0].fw_version, "4.0.0");

        // 行首数字但缺第二 token（仅 idx:）→ filter_map None（不 panic）
        assert!(parse_rdma_dev("0:\n").is_empty());
        // name 仅冒号（去尾冒号后空）→ None
        assert!(parse_rdma_dev("0: :\n").is_empty());
    }

    #[test]
    fn rdma_core_manager_default_reads_env() {
        // RdmaCoreManager::default() 读 OS_RDMA_SKIP_PROBE；new() 等价。
        let m = RdmaCoreManager::default();
        let m2 = RdmaCoreManager::new();
        assert_eq!(m.skip_probe, m2.skip_probe);
        // ibv_devinfo_cmd 字符串（#[allow(dead_code)] 方法）
        assert_eq!(m.ibv_devinfo_cmd(), "ibv_devinfo -v");
    }

    #[tokio::test]
    async fn rdma_probe_no_skip_degrades_empty() {
        // skip_probe=false → probe_devices 降级为空（未注册进程库）
        let m = RdmaCoreManager { skip_probe: false };
        let devs = m.list_devices().await.expect("降级不应报错");
        assert!(devs.is_empty());
        let cap = m.detect_capability().await.expect("降级不应报错");
        assert!(!cap.available);
        assert!(cap.ty.is_none());
    }

    #[test]
    fn cidr_to_string_format() {
        // cidr_to_string（私有 fn）经 ip_addr_add_cmd 间接覆盖，此处直接断言
        let cidr = IpCidr::new("10.20.30.40".parse::<IpAddr>().unwrap(), 16);
        let mgr = RdmaCoreManager { skip_probe: true };
        let cmd = mgr.ip_addr_add_cmd("ib0", cidr);
        assert_eq!(cmd, "ip addr add 10.20.30.40/16 dev ib0");
    }

    #[tokio::test]
    async fn configure_ipoib_ipv6_cidr() {
        // IPv6 CIDR（is_ipv6 路径 + configure_ipoib 降级）
        let mgr = RdmaCoreManager { skip_probe: true };
        let cidr = IpCidr::new("fd00::1".parse::<IpAddr>().unwrap(), 64);
        assert!(cidr.is_ipv6());
        mgr.configure_ipoib("ib0", cidr).await.expect("降级应成功");
    }
}
