//! 网络装机（P0）——iPXE 引导链脚本 + autoinstall 种子模板 + ISO 仓清单模型
//!
//! 依据 `docs/research/NET_BOOT_PROVISIONING.md` §P0（冻结）与 v0.1.48 任务书：
//! U 盘 iPXE → 输 NexOS IP → 服务端动态菜单（按 ISO 仓实时生成）→ HTTP 拉
//! vmlinuz/initrd/完整 ISO（casper `url=`）→ subiquity autoinstall 无人值守 →
//! `late-commands` 跑 install.sh 自动入集群。
//!
//! 本模块纯逻辑（无 I/O，`String` 进出，照 [`crate::pxe::PxeConfigBuilder`] 风格）：
//! - [`bootstrap_ipxe_script`]：U 盘 iPXE 起来后 `chain` 的第一跳脚本（dhcp→
//!   prompt 输服务器 IP→chain 动态菜单）；
//! - [`ipxe_menu_script`]：按仓内条目实时生成的菜单（**滚动版本策略**：自动装机
//!   只列每架构最新稳定版；Clonezilla 项 P0 占位灰显）；
//! - [`render_user_data`] / [`render_meta_data`]：nocloud-net 种子（24.04/26.04
//!   版本差异参数化：26.04 显式 `mirror-selection` + `geoip: false`）；
//! - [`parse_iso_repo_filename`] / [`IsoRepoEntry`]：ISO 仓文件名解析（白名单
//!   形态，防穿越）与"最新稳定版"判定（版本号数值比较）。
//!
//! HTTP 侧（os-api `handlers/provisioning.rs`）负责目录扫描/流式直传/端点编排。

use serde::Serialize;

use crate::error::{ProvisionError, ProvisionResult};

/// os-api HTTP 端口缺省（与 os-api `DEFAULT_API_PORT` 同值；iPXE URL 拼接用）。
pub const NETBOOT_API_PORT: u16 = 8558;

/// P2P 端口缺省（late-commands 的 `--bootstrap <ip>:7070` 用）。
pub const NETBOOT_P2P_PORT: u16 = 7070;

/// 引导件提取目标（ISO 内路径，大小写不敏感匹配）。
pub const CASPER_VMLINUZ: &str = "casper/vmlinuz";
/// initrd 的 ISO 内路径。
pub const CASPER_INITRD: &str = "casper/initrd";

/// 支持的 ISO 仓架构（内核架构形态，与 Ubuntu 文件名一致）。
pub const REPO_ARCHES: [&str; 2] = ["amd64", "arm64"];

// ----------------------------------------------------------------------------
// ISO 仓条目模型 + 文件名白名单解析
// ----------------------------------------------------------------------------

/// ISO 仓内一件可装机 ISO（由文件名解析而来，含"是否本架构最新稳定版"标记）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IsoRepoEntry {
    /// 仓内文件名（=下载名）。例：`ubuntu-26.04.1-live-server-amd64.iso`
    pub file: String,
    /// Ubuntu 版本（点分数字）。例：`26.04.1`
    pub version: String,
    /// 架构：`amd64` / `arm64`
    pub arch: String,
    /// 稳定条目 ID（版本-架构）。例：`26.04.1-amd64`（boot/seed URL 段）
    pub id: String,
    /// 是否该架构下的最新稳定版（滚动版本策略：自动装机菜单只列它）。
    pub latest: bool,
}

/// iPXE `buildarch` → ISO 仓架构（`x86_64`/`i386` → amd64，`arm64` → arm64）。
pub fn map_ipxe_arch(buildarch: &str) -> &'static str {
    match buildarch.trim().to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "arm64",
        // 32 位 BIOS/EFI 亦装 amd64 内核（Ubuntu 不再有 i386 server）
        _ => "amd64",
    }
}

/// 解析 ISO 仓文件名为条目（**白名单核心**：只认
/// `ubuntu-<点分数字>-live-server-<amd64|arm64>.iso` 精确形态；含 `/`、`\`、
/// `..`、大小写混杂或任何其他字符形态一律 None——不存在基于用户输入的路径拼接）。
pub fn parse_iso_repo_filename(file: &str) -> Option<IsoRepoEntry> {
    let stem = file.strip_suffix(".iso")?;
    let rest = stem.strip_prefix("ubuntu-")?;
    let (version, arch) = rest.rsplit_once("-live-server-")?;
    if !is_version_like(version) {
        return None;
    }
    if !REPO_ARCHES.contains(&arch) {
        return None;
    }
    // 防御：整个文件名不得再含路径形态字符（strip 后重新整体校验）
    if file.contains(['/', '\\', '\0']) || file.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    Some(IsoRepoEntry {
        file: file.to_string(),
        version: version.to_string(),
        arch: arch.to_string(),
        id: format!("{version}-{arch}"),
        latest: false,
    })
}

/// 版本串形态：`26.04` / `26.04.1`（≥2 段点分数字）。
fn is_version_like(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && v.len() <= 16
}

/// Ubuntu 版本数值比较（点分段逐段比，缺段当 0）：`Ok( Ordering )`。
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<u64> = a.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let pb: Vec<u64> = b.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let n = pa.len().max(pb.len());
    for i in 0..n {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// 给一组仓条目标记"每架构最新稳定版"（滚动版本策略：菜单/自动装机只认
/// latest=true 的条目；同版本双写时后者不覆盖——条目以文件名唯一）。
pub fn mark_latest_stable(entries: &mut [IsoRepoEntry]) {
    for e in entries.iter_mut() {
        e.latest = false;
    }
    for arch in REPO_ARCHES {
        let best = entries
            .iter()
            .filter(|e| e.arch == arch)
            .max_by(|a, b| version_cmp(&a.version, &b.version))
            .map(|e| e.file.clone());
        if let Some(file) = best {
            for e in entries.iter_mut() {
                if e.file == file {
                    e.latest = true;
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// iPXE 引导链脚本
// ----------------------------------------------------------------------------

/// 生成 `bootstrap.ipxe`（`GET /api/v1/provisioning/bootstrap.ipxe`）：
/// U 盘 iPXE 引导后的第一跳——dhcp → console → prompt 输 NexOS 服务器 IP
/// （直接回车 = 缺省值，缺省取 DHCP next-server / DHCP 服务器地址）→
/// `chain` 服务端动态菜单。
pub fn bootstrap_ipxe_script() -> String {
    let mut s = String::new();
    s.push_str("#!ipxe\n");
    s.push_str("# NexOS 网络装机引导（bootstrap.ipxe，服务端动态生成——勿手改）\n");
    s.push_str("# U 盘只需烧一次：菜单/版本/种子永远来自服务端（chain 下一跳）\n");
    s.push_str("console\n");
    s.push_str("dhcp net0 || true\n");
    s.push_str("# 缺省回车 = 源 IP（DHCP next-server / DHCP 服务器），通常即局域网 NexOS 节点\n");
    s.push_str("set nexos_srv ${next-server}\n");
    s.push_str("isset ${nexos_srv} || set nexos_srv ${net0/dhcp/server}\n");
    s.push_str(":ask\n");
    s.push_str("clear screen\n");
    s.push_str("echo === NexOS 网络装机 ===\n");
    s.push_str("echo 本机 ${platform}/${buildarch}  网卡 ${net0/mac}\n");
    s.push_str("echo 请输入局域网 NexOS 服务器 IP（直接回车 = ${nexos_srv}）\n");
    s.push_str("prompt --key 0x0d 'NexOS IP: ' && read nexos_srv || goto ask\n");
    s.push_str("isset ${nexos_srv} || goto ask\n");
    s.push_str(&format!(
        "chain --replace http://${{nexos_srv}}:{NETBOOT_API_PORT}/api/v1/provisioning/ipxe/menu?arch=${{buildarch}}&platform=${{platform}} || goto ask\n"
    ));
    s
}

/// 生成动态菜单脚本（`GET /api/v1/provisioning/ipxe/menu?arch=&platform=`）。
///
/// - 只列 `entries` 中 **latest=true 且 arch 匹配** 的自动装机项（滚动版本策略：
///   仓内老版本/其他架构不出现在自动流程，手动装机走仓管理下载）；
/// - 无可用项时给"仓为空"提示 + shell 兜底；
/// - Clonezilla 项 P0 占位灰显（`item --disabled`，P1 接通）；
/// - 兜底项：iPXE shell（救援）、重启。
pub fn ipxe_menu_script(entries: &[IsoRepoEntry], arch: &str, platform: &str) -> String {
    let installable: Vec<&IsoRepoEntry> = entries
        .iter()
        .filter(|e| e.latest && e.arch == arch)
        .collect();

    let mut s = String::new();
    s.push_str("#!ipxe\n");
    s.push_str("# NexOS 动态装机菜单（服务端按 ISO 仓实时生成）\n");
    s.push_str(&format!(
        "# 本机 {platform}/{arch}（仓内最新稳定版才进自动装机——滚动版本策略）\n"
    ));
    s.push_str("console\n");
    s.push_str(&format!("menu NexOS 网络装机（{platform}/{arch}）\n"));
    if installable.is_empty() {
        s.push_str("item --gap 仓内没有该架构的可装机 ISO\n");
        s.push_str("item --gap 请在 NexOS「系统自举→网络装机」登记 ISO 后重试\n");
    } else {
        for e in &installable {
            let label = menu_label(e);
            s.push_str(&format!("item --default {} {}\n", menu_key(e), label));
        }
    }
    s.push_str("item --gap\n");
    s.push_str("item --disabled clonezilla 克隆整机（Clonezilla，P1 提供——敬请期待）\n");
    s.push_str("item shell iPXE Shell（救援/诊断）\n");
    s.push_str("item reboot 重启计算机\n");
    s.push_str("choose --timeout 0 target && goto ${target}\n\n");
    for e in &installable {
        s.push_str(&ipxe_boot_entry_script(e));
    }
    s.push_str(":shell\n");
    s.push_str(
        "echo 输入 exit 返回菜单；也可手动 chain http://<ip>:8558/api/v1/provisioning/ipxe/menu\n",
    );
    s.push_str("shell ||\n");
    // shell 退出后重拉本菜单（base 已烘焙——菜单是 chain 来的，可无限自循环）
    s.push_str(&format!(
        "chain {base}/api/v1/provisioning/ipxe/menu?arch={arch}&platform={platform} ||\n",
        base = "@@NEXOS_BASE@@",
        arch = arch,
        platform = platform,
    ));
    s.push_str(":reboot\n");
    s.push_str("reboot\n");
    s
}

/// 菜单标签：`Ubuntu Server 26.04.1（amd64）`。
fn menu_label(e: &IsoRepoEntry) -> String {
    format!("Ubuntu Server {}（{}）", e.version, e.arch)
}

/// 菜单选择键（iPXE item key，合法标识符形态）。
fn menu_key(e: &IsoRepoEntry) -> String {
    format!("os{}", e.id.replace(['.', '-'], ""))
}

/// 单条 boot 条目脚本（菜单 `:label` 命中后执行）：kernel + initrd + boot。
///
/// cmdline 冻结形态（v0.1.48 任务书）：`root=/dev/ram0 ramdisk_size=1500000
/// ip=dhcp url=<ISO 直传> autoinstall ds=nocloud-net\;s=<seed 目录>/`
/// —— casper 经 `url=` HTTP 拉完整 ISO（流式直传 + Range 断点），subiquity 经
/// nocloud-net 拉种子实现无人值守。
pub fn ipxe_boot_entry_script(e: &IsoRepoEntry) -> String {
    let base = api_base_placeholder();
    let mut s = String::new();
    s.push_str(&format!(":{}\n", menu_key(e)));
    s.push_str(&format!(
        "echo 即将网络安装 Ubuntu Server {}（{}）：单盘将被全清，{} 秒内可按 Ctrl+B 中止\n",
        e.version, e.arch, 3
    ));
    s.push_str(&format!(
        "kernel {base}/api/v1/provisioning/boot/{id}/vmlinuz root=/dev/ram0 ramdisk_size=1500000 ip=dhcp url={base}/api/v1/provisioning/isos/{file} autoinstall ds=nocloud-net\\;s={base}/api/v1/provisioning/seed/{id}/\n",
        id = e.id,
        file = e.file,
    ));
    s.push_str(&format!(
        "initrd {base}/api/v1/provisioning/boot/{id}/initrd\n",
        id = e.id
    ));
    s.push_str("boot\n\n");
    s
}

/// 菜单脚本里相对 base 的占位：iPXE 脚本由服务端按请求 Host 渲染，base 即
/// `http://<请求来源 IP>:<端口>`（见 os-api 侧 `netboot_base_url`）。
fn api_base_placeholder() -> String {
    "@@NEXOS_BASE@@".to_string()
}

/// 把菜单/boot 条目脚本中的 base 占位替换为真实来源地址（服务端渲染第二步）。
pub fn render_base(script: &str, base: &str) -> String {
    script.replace("@@NEXOS_BASE@@", base)
}

// ----------------------------------------------------------------------------
// autoinstall 种子（nocloud-net）
// ----------------------------------------------------------------------------

/// user-data 渲染参数（版本/架构/来源地址参数化——照 PxeConfigBuilder 风格）。
#[derive(Debug, Clone)]
pub struct AutoinstallSeedParams {
    /// Ubuntu 版本（决定 24.04/26.04 模板分支）。
    pub version: String,
    /// 架构（amd64/arm64；进 apt arches 列表）。
    pub arch: String,
    /// 安装源/种子源 base（`http://<NexOS IP>:8558`——Host 头推导）。
    pub server_base: String,
    /// 主机名（缺省 nexos-node）。
    pub hostname: String,
    /// 装机用户名（缺省 nexos）。
    pub username: String,
    /// 密码 crypt 串（SHA-512 `$6$...`；缺省 DEFAULT_PASSWORD_CRYPT → nexos）。
    pub password_crypt: String,
    /// apt 主镜像 URI（缺省国内阿里云）。
    pub apt_mirror: String,
    /// NexOS P2P 引导端点（`<ip>:7070`；空 = 用服务器同 IP 缺省端口）。
    pub p2p_bootstrap: String,
}

/// 缺省装机用户。
pub const DEFAULT_SEED_USERNAME: &str = "nexos";

/// 缺省密码 `nexos` 的 SHA-512 crypt（首启请改密；openssl passwd -6 生成）。
pub const DEFAULT_PASSWORD_CRYPT: &str =
    "$6$NexOSBoot$QRd9jbbAo8wEAwE3rAyHfSF9WMaSXhruQ7X.4l2ljkmuH.p7VEej6VKhSqjX96RNKO8AF8tP3ySdEHQQjvvYw1";

/// 缺省 apt 主镜像（国内）。
pub const DEFAULT_APT_MIRROR: &str = "http://mirrors.aliyun.com/ubuntu";

impl Default for AutoinstallSeedParams {
    fn default() -> Self {
        Self {
            version: "26.04.1".to_string(),
            arch: "amd64".to_string(),
            server_base: format!("http://127.0.0.1:{NETBOOT_API_PORT}"),
            hostname: "nexos-node".to_string(),
            username: DEFAULT_SEED_USERNAME.to_string(),
            password_crypt: DEFAULT_PASSWORD_CRYPT.to_string(),
            apt_mirror: DEFAULT_APT_MIRROR.to_string(),
            p2p_bootstrap: String::new(),
        }
    }
}

/// 是否 26.04+ 世代（mirror-selection/geoip 语法；24.04 走旧 `primary` 键）。
fn is_new_apt_schema(version: &str) -> bool {
    matches!(
        version_cmp(version, "26.04"),
        std::cmp::Ordering::Greater | std::cmp::Ordering::Equal
    )
}

/// 渲染 nocloud `user-data`（`#cloud-config` autoinstall YAML）。
///
/// 要点：
/// - 存储全自动单盘清除（`layout: direct`——subiquity 直接铺满第一块盘）；
/// - 网络 dhcp、语言/键盘固定、SSH 装服务端；
/// - **版本差异**：26.04+ `apt.mirror-selection.primary` + `geoip: false`（显式关
///   geoip 防内网装机卡探活）；24.04 旧式 `apt.primary` 列表；
/// - **闭环**：`late-commands` 最后一刻 `curtin in-target` 下载并执行本 NexOS 的
///   install.sh（`--bootstrap <ip>:7070` 指回发起装机的节点）——装完 Ubuntu 自动
///   变成 NexOS 集群成员；
/// - identity 密码为缺省占位 `nexos`（文档：首启改密）。
pub fn render_user_data(p: &AutoinstallSeedParams) -> ProvisionResult<String> {
    let entry =
        parse_iso_repo_filename(&format!("ubuntu-{}-live-server-{}.iso", p.version, p.arch))
            .ok_or_else(|| {
                ProvisionError::InvalidConfig(format!(
                    "autoinstall 参数非法: version={} arch={}",
                    p.version, p.arch
                ))
            })?;
    let bootstrap = if p.p2p_bootstrap.is_empty() {
        let host = p
            .server_base
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        format!("{host}:{NETBOOT_P2P_PORT}")
    } else {
        p.p2p_bootstrap.clone()
    };
    let base = p.server_base.trim_end_matches('/');
    // YAML 双引号串内的转义（crypt 串含 $ 与 .，无引号；hostname/username 走
    // 白名单字符校验防 YAML 注入）
    if !is_safe_yaml_scalar(&p.hostname) || !is_safe_yaml_scalar(&p.username) {
        return Err(ProvisionError::InvalidConfig(
            "hostname/username 含非法字符（仅字母数字连字符下划线点）".to_string(),
        ));
    }

    let mut s = String::new();
    s.push_str("#cloud-config\n");
    s.push_str(&format!(
        "# NexOS 网络装机种子（nocloud user-data；id={} 由服务端渲染）\n",
        entry.id
    ));
    s.push_str("autoinstall:\n");
    s.push_str("  version: 1\n");
    s.push_str("  locale: en_US.UTF-8\n");
    s.push_str("  keyboard: { layout: us }\n");
    s.push_str("  network:\n");
    s.push_str("    version: 2\n");
    s.push_str("    ethernets:\n");
    s.push_str("      any:\n");
    s.push_str("        match:\n");
    s.push_str("          name: en*\n");
    s.push_str("        dhcp4: true\n");
    s.push_str("  storage:\n");
    s.push_str("    layout:\n");
    s.push_str("      name: direct\n");
    s.push_str("      match:\n");
    s.push_str("        size: largest\n");
    s.push_str("  identity:\n");
    s.push_str(&format!("    hostname: {}\n", p.hostname));
    s.push_str(&format!("    username: {}\n", p.username));
    s.push_str(&format!("    password: '{}'\n", p.password_crypt));
    s.push_str("  ssh:\n");
    s.push_str("    install-server: true\n");
    s.push_str("    allow-pw: true\n");
    s.push_str("  apt:\n");
    if is_new_apt_schema(&p.version) {
        // 26.04+：显式关 geoip（内网装机不查 geoip.ubuntu.com）+ 新择优键
        s.push_str("    geoip: false\n");
        s.push_str("    mirror-selection:\n");
        s.push_str("      primary:\n");
        s.push_str(&format!("        - uri: {}\n", p.apt_mirror));
        s.push_str("          arches: [amd64, arm64]\n");
    } else {
        // 24.04：旧式 primary URI 列表
        s.push_str("    primary:\n");
        s.push_str(&format!("      - uri: {}\n", p.apt_mirror));
        s.push_str("        arches: [amd64, arm64]\n");
    }
    s.push_str("    fallback: offline-install\n");
    s.push_str("  packages:\n");
    s.push_str("    - curl\n");
    s.push_str("    - openssh-server\n");
    s.push_str("  late-commands:\n");
    // 闭环：进 chroot 下载 install.sh 并执行——bootstrap 指回发起装机的 NexOS
    // 节点（装完 Ubuntu 即自动入集群）。curl 落盘再执行（管道形式会把 bash
    // 留在 live 环境、写到错误的根）。
    s.push_str(&format!(
        "    - curtin in-target -- bash -c 'curl -fsSL {base}/api/v1/provisioning/install.sh -o /tmp/nexos-install.sh && bash /tmp/nexos-install.sh --source {base} --bootstrap {bootstrap}'\n"
    ));
    s.push_str("  shutdown: reboot\n");
    Ok(s)
}

/// nocloud `meta-data`（约定空内容——instance-id 由 ds 忽略）。
pub fn render_meta_data() -> String {
    String::new()
}

/// YAML 标量安全（字母数字 + `-_.`；防引号/换行注入）。
fn is_safe_yaml_scalar(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file: &str) -> IsoRepoEntry {
        parse_iso_repo_filename(file).expect("合法仓文件名")
    }

    #[test]
    fn parse_valid_repo_filenames() {
        let e = entry("ubuntu-26.04.1-live-server-amd64.iso");
        assert_eq!(e.version, "26.04.1");
        assert_eq!(e.arch, "amd64");
        assert_eq!(e.id, "26.04.1-amd64");
        let e = entry("ubuntu-24.04.5-live-server-arm64.iso");
        assert_eq!(e.id, "24.04.5-arm64");
        let e = entry("ubuntu-26.04-live-server-amd64.iso");
        assert_eq!(e.version, "26.04");
    }

    #[test]
    fn parse_rejects_traversal_and_foreign_shapes() {
        // 穿越与路径形态
        for bad in [
            "../ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-amd64.iso/../../etc/passwd",
            "..\\ubuntu-26.04.1-live-server-amd64.iso",
            "/etc/passwd",
            "ubuntu-26.04.1-live-server-amd64.iso.exe",
            "ubuntu-26.04.1-live-server-amd64",
            "Ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-i386.iso",
            "ubuntu-26.04.1-desktop-amd64.iso",
            "ubuntu-live-server-amd64.iso",
            "ubuntu-26.04..1-live-server-amd64.iso",
            "ubuntu--live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-amd64.iso\x00",
            "ubuntu-26.04.1;rm -rf-live-server-amd64.iso",
        ] {
            assert!(parse_iso_repo_filename(bad).is_none(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn version_cmp_orders_point_releases() {
        assert_eq!(version_cmp("26.04.2", "26.04.10"), std::cmp::Ordering::Less);
        assert_eq!(version_cmp("26.04.1", "26.04"), std::cmp::Ordering::Greater);
        assert_eq!(version_cmp("24.04.5", "26.04.1"), std::cmp::Ordering::Less);
        assert_eq!(version_cmp("26.04.1", "26.04.1"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn mark_latest_per_arch() {
        let files = [
            "ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-24.04.5-live-server-amd64.iso", // 手动放的老版
            "ubuntu-26.04.1-live-server-arm64.iso",
            "ubuntu-26.04.2-live-server-arm64.iso", // 滚动新版
        ];
        let mut es: Vec<_> = files.iter().map(|f| entry(f)).collect();
        mark_latest_stable(&mut es);
        let latest: Vec<bool> = es.iter().map(|e| e.latest).collect();
        assert_eq!(latest, vec![true, false, false, true]);
    }

    #[test]
    fn map_ipxe_arch_variants() {
        assert_eq!(map_ipxe_arch("x86_64"), "amd64");
        assert_eq!(map_ipxe_arch("i386"), "amd64");
        assert_eq!(map_ipxe_arch("arm64"), "arm64");
        assert_eq!(map_ipxe_arch("aarch64"), "arm64");
    }

    #[test]
    fn bootstrap_script_shape() {
        let s = bootstrap_ipxe_script();
        assert!(s.starts_with("#!ipxe\n"));
        assert!(s.contains("console\n"));
        assert!(s.contains("dhcp net0"));
        assert!(s.contains("prompt --key 0x0d"));
        assert!(s.contains("read nexos_srv"));
        assert!(s.contains("set nexos_srv ${next-server}"));
        assert!(s.contains(&format!(
            "chain --replace http://${{nexos_srv}}:{NETBOOT_API_PORT}/api/v1/provisioning/ipxe/menu?arch=${{buildarch}}&platform=${{platform}} || goto ask"
        )));
        // 纯函数幂等
        assert_eq!(s, bootstrap_ipxe_script());
    }

    #[test]
    fn menu_lists_only_latest_matching_arch() {
        let mut es = vec![
            entry("ubuntu-26.04.1-live-server-amd64.iso"),
            entry("ubuntu-24.04.5-live-server-amd64.iso"),
            entry("ubuntu-26.04.1-live-server-arm64.iso"),
        ];
        mark_latest_stable(&mut es);
        let s = ipxe_menu_script(&es, "amd64", "efi");
        assert!(s.contains("Ubuntu Server 26.04.1（amd64）"));
        assert!(!s.contains("24.04"), "老版本不进自动装机菜单");
        assert!(!s.contains("arm64）"), "其他架构不进本机菜单");
        assert!(
            s.contains("item --disabled clonezilla"),
            "Clonezilla 占位灰显"
        );
        assert!(s.contains(":shell"));
        assert!(s.contains(":reboot"));
        // boot 条目：冻结 cmdline 形态（渲染 base 后）
        let r = render_base(&s, "http://192.0.2.106:8558");
        assert!(r.contains("kernel http://192.0.2.106:8558/api/v1/provisioning/boot/26.04.1-amd64/vmlinuz root=/dev/ram0 ramdisk_size=1500000 ip=dhcp url=http://192.0.2.106:8558/api/v1/provisioning/isos/ubuntu-26.04.1-live-server-amd64.iso autoinstall ds=nocloud-net\\;s=http://192.0.2.106:8558/api/v1/provisioning/seed/26.04.1-amd64/"));
        assert!(r.contains(
            "initrd http://192.0.2.106:8558/api/v1/provisioning/boot/26.04.1-amd64/initrd"
        ));
    }

    #[test]
    fn menu_empty_repo_gives_hint() {
        let s = ipxe_menu_script(&[], "arm64", "efi");
        assert!(s.contains("仓内没有该架构的可装机 ISO"));
        assert!(!s.contains("kernel "));
    }

    #[test]
    fn user_data_2604_branch() {
        let mut p = AutoinstallSeedParams::default();
        p.version = "26.04.1".into();
        p.arch = "amd64".into();
        p.server_base = "http://192.0.2.106:8558".into();
        let y = render_user_data(&p).unwrap();
        assert!(y.starts_with("#cloud-config\n"));
        assert!(y.contains("  version: 1\n"));
        // 26.04 新式 apt 键 + geoip 显式关
        assert!(y.contains("    geoip: false"));
        assert!(y.contains("    mirror-selection:"));
        assert!(!y.contains("    primary:\n      - uri"));
        assert!(y.contains("        - uri: http://mirrors.aliyun.com/ubuntu"));
        assert!(y.contains("    fallback: offline-install"));
        // 存储全自动单盘
        assert!(y.contains("      name: direct"));
        assert!(y.contains("        size: largest"));
        // 身份缺省
        assert!(y.contains("    username: nexos"));
        assert!(y.contains("    password: '$6$"));
        // 闭环 late-commands
        assert!(y.contains(
            "curtin in-target -- bash -c 'curl -fsSL http://192.0.2.106:8558/api/v1/provisioning/install.sh -o /tmp/nexos-install.sh && bash /tmp/nexos-install.sh --source http://192.0.2.106:8558 --bootstrap 192.0.2.106:7070'"
        ));
        assert!(y.contains("  shutdown: reboot"));
    }

    #[test]
    fn user_data_2404_branch() {
        let mut p = AutoinstallSeedParams::default();
        p.version = "24.04.5".into();
        p.server_base = "http://192.168.1.2:8558".into();
        p.p2p_bootstrap = "192.168.1.2:7070,203.0.113.2:7070".into();
        let y = render_user_data(&p).unwrap();
        // 24.04 旧式键；无 geoip/mirror-selection
        assert!(!y.contains("geoip"));
        assert!(!y.contains("mirror-selection"));
        assert!(y.contains("    primary:\n      - uri: http://mirrors.aliyun.com/ubuntu"));
        // 显式 bootstrap 列表透传
        assert!(y.contains("--bootstrap 192.168.1.2:7070,203.0.113.2:7070"));
    }

    #[test]
    fn user_data_rejects_bad_identity() {
        let mut p = AutoinstallSeedParams::default();
        p.hostname = "bad'host\n".into();
        assert!(render_user_data(&p).is_err());
        let mut p2 = AutoinstallSeedParams::default();
        p2.version = "26.04.1; DROP".into();
        assert!(render_user_data(&p2).is_err());
    }

    #[test]
    fn meta_data_is_empty() {
        assert_eq!(render_meta_data(), "");
    }

    #[test]
    fn render_base_replaces_all_occurrences() {
        let es = [entry("ubuntu-26.04.1-live-server-amd64.iso")];
        let s = ipxe_boot_entry_script(&es[0]);
        assert!(s.contains("@@NEXOS_BASE@@"));
        let r = render_base(&s, "http://1.2.3.4:8558");
        assert!(!r.contains("@@NEXOS_BASE@@"));
        assert!(r.starts_with(":os26041amd64\n"));
    }
}
