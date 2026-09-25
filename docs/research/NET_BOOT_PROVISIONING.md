# 网络装机能力调研方案书（NET_BOOT_PROVISIONING）

> **调研日期**：2026-09-18（实测数据当日取自 boot.ipxe.org / 镜像站 / 本机 `df`）
>
> **需求原话要点**：参考再生龙（Clonezilla）；系统自举里加一个小化的引导 ISO（可能就
> 几百 KB/MB 级），作用是**引导设备但不是完整安装**；引导后输入局域网 NexOS 的 IP，
> 即可进行系统级安装——设备现状是 Ubuntu 24.04 就装 24.04、26.04 就装 26.04（按目标
> 现状/需求选版本）；自举里可放多种 ISO；**架构不同没有 ISO 时可经 NexOS 为跳板去下载**。
>
> **关联**：`docs/PROVISIONING.md`（自举应用本体）、`docs/BOOTSTRAP_INSTALL.md`
> （install.sh 一键入集群）、`crates/os-provision`（PXE/初始化脚本纯逻辑）、
> `scripts/build-iso.sh` + `crates/os-iso`（自制 ISO 链路）。

---

## 0. 结论速览（三项关键选型，各一句话）

| 决策点 | 推荐 | 一句话理由 |
|--------|------|-----------|
| **引导器（小 ISO）** | **iPXE** | 72KB（undionly.kpxe）/ 9.4MB（官方预编译 ipxe.iso，BIOS+UEFI 双启）实测可达"几百 KB~几 MB"，原生 HTTP chainload + `prompt/read` 交互输 IP + `chain` 拉服务端菜单，且仓库 `os-provision::pxe` 已有 iPXE 脚本生成器可直接复用 |
| **装机方式** | **subiquity autoinstall + NexOS 本地完整 ISO 仓** | 24.04+ 官方 mini.iso/netboot 已死（d-i 止于 23.04），唯一官方无人值守路径 = live-server ISO 内核/initrd + `url=` HTTP 拉完整 ISO + `autoinstall ds=nocloud-net` 拉 YAML；`late-commands` 里 `curtin in-target -- curl install.sh` 即"装完 Ubuntu 自动入 NexOS 集群"闭环 |
| **克隆/批量铺机** | **Clonezilla Live（lite）+ /tank 镜像仓 + firstboot 重身份** | 不引入整套 DRBL/SE（企业网 DHCP 冲突面大）；Clonezilla live ISO 仅 ~462MB，U 盘/PXE 起后连 Samba/NFS 存取镜像，镜像放 `/tank/clonezilla` 经 106 既有 SMB 通道分发；克隆机身份冲突按业界 sysprep 惯例（machine-id/ssh host keys/密钥重生成）做 NexOS firstboot 自愈 |

**空间账一句话**：P0（ISO 仓 版本×架构矩阵 + 引导件）≈ **13GB**；P1 加克隆镜像仓预算
**200~400GB**；P2 加 deb 缓存代理 **~50GB** —— 106 tank 实测 **879GB 可用**，全量富裕。

**P0 一句话**：官方 iPXE 小 ISO（或自编译 EMBED 版）+ U 盘启动 → 输 NexOS IP →
服务端动态 `boot.ipxe` 菜单（按 ISO 仓矩阵列出版本/架构）→ HTTP 拉 vmlinuz/initrd/ISO
→ autoinstall 无人值守 → `late-commands` 跑 install.sh 自动入集群。

---

## 1. 现有基础盘点（仓库内可复用的先例）

| 先例 | 位置 | 对本方案的复用价值 |
|------|------|-------------------|
| **iPXE 脚本/PXE 配置生成器（纯函数）** | `crates/os-provision/src/pxe.rs`（`PxeConfigBuilder`：bootstrap.ipxe / pxelinux.cfg/default / DHCP bootfile / TFTP 清单，UEFI/BIOS/ARM64 三分支） | P0 引导脚本与 P1 PXE 全自动的生成核心，已含单测，直接扩展"交互输 IP + 动态菜单 chain" |
| **阶段1 初始化脚本生成器** | `crates/os-provision/src/init_script.rs`（ZFS 分区/建池/unsquashfs/first-boot.flag） | 自研 rootfs 路线（os-iso squashfs+ZFS）的安装执行体，与 Ubuntu 底座路线并行 |
| **install.sh 动态生成 + dist 白名单分发** | `crates/os-api/src/handlers/provisioning.rs`（`GET install.sh` 按 Host 头渲染 / `POST prepare-distributable` / `GET dist/:artifact` 精确白名单 `["os-api","os-api-aarch64"]` + `x-nexos-sha256` 对拍） | ISO 仓 HTTP 分发直接沿用该模式（白名单=版本×架构矩阵）；**注意**：现通道 base64 全内存装载，3GB ISO 不可用（见 §7 风险 R3） |
| **ISO 真实构建链路** | `crates/os-iso`（XorrisoIsoBuilder）+ `scripts/build-iso.sh` + `docs/PROVISIONING.md` §4（任务状态机/build_log） | 引导小 ISO 打包（iPXE 二进制 + EMBED 脚本 → xorriso 双启 ISO）可复用此链路 |
| **电源控制层（WoL/IPMI）** | `handlers/power.rs`（WoL 魔术包/IPMI RMCP+ 扫描/上电） | P1 全自动流水线第一环已就位：唤醒/上电 → PXE → autoinstall → install.sh |
| **jumpbox 下载先例** | MEMORY.md（apt 镜像 403 → 跳板机 198.51.100.114 拉官方 tar；GitHub 走跳板） | P2"缺 ISO 经 NexOS 跳板下载"的直接先例：国内镜像直连失败 → 跳板隧道拉取 |
| **SMB 分发通道** | 106 已跑 Samba（`nexos-downloads` 共享，见 MEMORY） | Clonezilla 镜像仓 `/tank/clonezilla` 用同款 SMB 出给克隆端存取 |
| **PXE/TFTP 契约** | `crates/os-network/src/services.rs`（`PxeServer` trait + `set_boot_file`，现有 Mock） | P1 落地 dnsmasq/TFTP 后端的挂点 |

---

## 2. 调研 A：引导器选型（"小 ISO"）

### 2.1 iPXE（推荐）

| 维度 | 事实（2026-09-18 实测/官方） |
|------|------------------------------|
| **体积** | `undionly.kpxe`（BIOS 链式 NBP）实测 **72,373 B ≈ 71KB**；官方预编译 `ipxe.iso` 实测 **9,437,184 B ≈ 9.4MB**（bootable ISO9660，含全部驱动）；`ipxe.usb`（U 盘写盘镜像）≈ 1MB 级；自编译裁剪（`make bin/ipxe.iso EMBED=bootstrap.ipxe`，仅留所需网卡驱动）可到**几百 KB~1MB** —— 完全命中"几百 KB/MB"预期 |
| **ISO/USB 双形态** | `ipxe.iso`（刻盘/VENTOY/虚拟光驱）与 `ipxe.usb`（`dd` 写 U 盘）官方均有预编译；自编译同出一源（bin/i386-pc、bin-x86_64-efi、bin-arm64-efi） |
| **HTTP chainload** | iPXE 原生 HTTP(S)/iSCSI/AoE 栈：`kernel http://srv/vmlinuz` + `initrd http://srv/initrd` + `boot` 直接网络引导内核；`chain http://srv/boot.ipxe` 拉取**服务端动态生成的脚本**（菜单/参数全部服务端可控，客户端永远不用重烧） |
| **交互输 IP 脚本形态** | iPXE 脚本（`#!ipxe`）自带 `console`/`prompt`/`read`/`params`/`menu`/`choose`/`shell` 命令——先 `dhcp`（失败则 `prompt`+`read` 让用户输服务器 IP），再 `chain http://${srv}:8558/.../boot.ipxe?arch=${buildarch}`。`${buildarch}`/`${platform}` 内置变量让**同一 U 盘 amd64/arm64 通吃** |
| **UEFI/legacy 双启** | BIOS：`undionly.kpxe`（或 pxelinux 链）；UEFI x64：`ipxe.efi`；UEFI arm64：`ipxe-arm64.efi`（ARM 服务器/DGX Spark）。官方预编译 ISO 已双启；自编译 grub-mkrescue 包装双 El Torito 亦可（复用 os-iso 链路） |
| **短板** | EFI 二进制**未签名**（boot.ipxe.org 的 .efi 实测 404，需发行版包或自编译）→ UEFI **Secure Boot** 机器需走 shim+grub 签名链或关 SB（见 §7 R1）；`ipxe.iso` 全驱动版 9.4MB 略超"几百 KB"（自编译可缩） |

**NexOS 引导脚本骨架（P0 交付物之一，扩展 `os-provision::pxe::PxeConfigBuilder`）**：

```
#!ipxe
# NexOS bootstrap —— 烧一次 U 盘，全网通用（服务端菜单永远最新）
dhcp net0 || true
:ask
echo 已探测网卡: ${net0/mac}
prompt --key 0x0d 输入局域网 NexOS 服务器 IP 并回车:
read nexos_srv || goto ask
chain http://${nexos_srv}:8558/api/v1/provisioning/netboot/boot.ipxe?arch=${buildarch}&platform=${platform} || goto ask
```

**服务端动态菜单（NexOS 按 ISO 仓矩阵实时生成，含"缺架构→提示跳板下载"分支）**：

```
#!ipxe   # http://<nexos>:8558/api/v1/provisioning/netboot/boot.ipxe?arch=amd64
menu NexOS 网络装机（本机 arch=amd64）
item 2404s Ubuntu Server 24.04.3（amd64）
item 2604s Ubuntu Server 26.04.1（amd64）
item --key s shell    iPXE Shell（救援）
choose --default 2404s target && goto ${target}
:2404s
kernel http://<nexos>:8558/api/v1/provisioning/netboot/24.04/amd64/vmlinuz ip=dhcp url=http://<nexos>:8558/api/v1/provisioning/iso/24.04/amd64.iso autoinstall ds=nocloud-net\;s=http://<nexos>:8558/api/v1/provisioning/autoinstall/24.04/amd64/ cloud-config-url=/dev/null
initrd http://<nexos>:8558/api/v1/provisioning/netboot/24.04/amd64/initrd
boot
```

### 2.2 Ubuntu 官方 netboot / mini.iso 现状（重要变化）

- **mini.iso 已死**：传统 mini.iso/netboot.tar.gz 由 debian-installer（d-i）产出，
  **23.04 起 d-i 停产，24.04/26.04 无 mini.iso**（22.04 是末代 LTS；社区称
  "death of netboot mini.iso"，Ubuntu Discourse "Netbooting the live server installer"）。
- **现行官方 netboot 方案**（ubuntu.com/server/docs "How to netboot the server
  installer"）：从 **live-server ISO 内提取 `casper/vmlinuz` + `casper/initrd`** 走
  TFTP/HTTP，内核 cmdline 加 **`url=http://<server>/…live-server-amd64.iso`** —— casper
  会经 HTTP **拉取完整 ISO**（千兆内网约 30~60s）并 loop 挂载为安装源；再加
  `autoinstall ds=nocloud-net;s=http://<server>/seed/` 实现无人值守。
  UEFI 安全启动路径官方用 shim+grub 签名件（`shim-signed` + `grubnetx64_efi_signed`）。
- **结论**：24.04+ 的"小"只能小在**引导器**（iPXE 几百 KB），安装源无法回到 mini.iso
  时代的 100MB 级——空间/流量账按完整 ISO 规划（§5）。netboot.xyz 可作通用菜单参考
  （其本质也是 iPXE 脚本拉各发行版安装器）。

### 2.3 替代方案与对比矩阵

| 方案 | 体积 | 网络/交互能力 | 双启/SB | 评价 |
|------|------|--------------|---------|------|
| **iPXE（推荐）** | 71KB~9.4MB（可裁剪） | HTTP chainload + `prompt/read` 交互 + 服务端动态菜单 | BIOS+UEFI(x64/arm64)；SB 需 shim 链 | 唯一同时满足"小、可输 IP、服务端可控菜单、双架构"；仓库已有生成器 |
| grub2-mkrescue 自制小 ISO | ~10~30MB（双平台） | 需 grubnet 模块编入才有网络；无交互输 IP 脚本能力（grub.cfg 静态） | 双启；Ubuntu 的 `grubnetx64_efi_signed` **签名可过 SB** | 适合"SB 刚性环境"的 B 方案（官方 netboot 文档即此法）；交互/动态性弱 |
| Ventoy | U 盘工具 ~2MB + ISO 原样放盘 | 无网络安装逻辑（虚拟光驱本地引导） | 好 | 离线多 ISO 随身库利器，但与"输 IP 网络装机"目标不符；可作运维兜底 |
| systemd-boot | <1MB | **无网络栈** | UEFI only | 排除 |
| netboot.xyz | ISO ~MB 级 | iPXE 通用菜单（拉公网源） | 同 iPXE | 思路同源但面向公网/社区源；菜单逻辑参考价值 > 直接采用 |

---

## 3. 调研 B：网络自动安装（输 IP 之后的系统级安装）

### 3.1 subiquity autoinstall（24.04 / 26.04 server，推荐主路径）

**与源 ISO 的关系（关键结论）**：autoinstall 运行在 live 环境里（casper/subiquity），
**需要完整 live-server ISO**（squashfs 是安装源）；不存在"mini.iso 级小安装源"。
网络形态 = kernel+initrd（ISO 内提取）经 iPXE 引导 + `url=` 让 casper HTTP 拉 ISO。
桌面版 ISO 从 24.04 起同样 subiquity 化（见 §3.2）。

**autoinstall YAML**（顶层 `autoinstall:`，主要键：`version/identity/storage/ssh/apt/
proxy/network/locale/packages/late-commands/user-data/shutdown` 等；参考手册
canonical-subiquity readthedocs，2026-07 仍为现行版）：

```yaml
#cloud-config   # 由 NexOS GET /provisioning/autoinstall/{ver}/{arch}/user-data 动态渲染
autoinstall:
  version: 1
  locale: en_US.UTF-8
  identity: { hostname: nexos-node-<mac尾4>, username: ubuntu, password: '$6$…哈希…' }
  ssh: { install-server: true, authorized-keys: [<运营公钥>] }
  storage: { layout: { name: direct } }        # 或 lvm；ZFS root 是自研路线（init_script.rs）
  apt:
    mirror-selection:
      primary: [{ uri: "http://mirrors.tuna.tsinghua.edu.cn/ubuntu", arches: [amd64, arm64] }]
    fallback: offline-install                   # 断外网也能装完基础系统
  late-commands:                                # ★ 闭环：装完 Ubuntu 自动入 NexOS 集群
    - curtin in-target -- bash -c "$(curl -fsSL http://<nexos>:8558/api/v1/provisioning/install.sh)" -- --bootstrap <nexos-ip>:7070 --source http://<nexos>:8558
  shutdown: reboot
```

- **最小无人值守配置**：`version: 1` + `identity`（唯一必填组）即可全自动。
- **装后 hook（闭环卖点）**：`late-commands` 在安装收尾、目标系统挂 `/target` 时以
  root 执行；`curtin in-target --` 进 chroot。**一行 curl 复用现有 install.sh**——
  Ubuntu 装完即自动 systemd 服务化 + P2P bootstrap 指向发起装机的 NexOS 节点，与
  BOOTSTRAP_INSTALL.md 的"一条命令入集群"完全同源（安装源 URL 由渲染时注入，
  不依赖公网入口）。
- **26.04 注意**：subiquity 新增**镜像自动择优**（`mirror-selection.primary` 列表逐个
  探活取第一个可用；`geoip: true` 会查 geoip.ubuntu.com 选国家镜像，内网装机建议
  显式关 geoip 写死国内镜像）；`apt.fallback` 支持 `abort/offline-install/continue-anyway`。
- **网络下发的种子**：`ds=nocloud-net;s=http://<nexos>/…/` 目录下两文件
  `user-data` + 空 `meta-data`，由 NexOS 模板引擎按（版本×架构×目标盘×token）渲染。

### 3.2 桌面版 autoinstall 现状

- **24.04 起桌面安装器重构为 subiquity 基座，autoinstall 官方支持**（Subiquity 24.04.1
  release notes：desktop + 7 个 flavor 支持 autoinstall），同一套 YAML；Canonical
  官方示例仓 `canonical/autoinstall-desktop`。22.04 时代需 hack（preseed 式）已成历史。
- 26.04 延续该架构；桌面 ISO 体积 ~6GB 级（24.04 desktop ≈ 5.7~6.7GB），入仓成本高，
  建议按需入仓（默认只备 server）。
- 桌面 ISO 不含 openssh-server，`late-commands` 里需 `curtin in-target -- apt-get
  install -y curl openssh-server`（或用 `packages:` 键）后再跑 install.sh。

### 3.3 安装源策略：本地完整 ISO 仓（推荐）vs 最小引导+deb 缓存混合

| 策略 | 空间 | 优点 | 缺点 |
|------|------|------|------|
| **A. NexOS 本地托管完整 ISO（P0 推荐）** | 矩阵 4 件 ≈ 11.2GB | **离线可装**（`fallback: offline-install`）；sha256 官方可验；casper `url=` 官方支持零改造；同一仓兼作"人工装机/救援介质下载点" | 每版本×架构 ~2.7~3.1GB；点版本更新需重下（zsync 增量可省 90%+，P2） |
| B. 最小引导 + deb 网络源（apt-cacher-ng/squid-deb-proxy 缓存） | 缓存稳态 20~50GB | ISO 不用全存；包级缓存首装+后续 `apt update` 都受益 | **仍需完整 ISO 引导**（subiquity 在 live 里跑，squashfs 是安装源——绕不开）；断外网不可装；autoinstall 需配 `apt.proxy`/`proxy` 指向 `http://<nexos>:3142` |

**推荐组合**：P0 用 A（保证"输 IP 即装、不依赖外网"的体验闭环）；P2 叠加
apt-cacher-ng（3142）作为**装后包更新缓存层**（autoinstall `apt: proxy:` 一等支持），
squid-deb-proxy 的 avahi 零配置优势在 PXE 场景无意义（地址本就由我们注入）。

---

## 4. 调研 C：Clonezilla（再生龙）

### 4.1 lite（Live）vs SE（DRBL）与 PXE 形态

| 形态 | 机制 | 适用 |
|------|------|------|
| **Clonezilla Live（lite）** | 单机 U 盘/CD 启动（ISO ~462MB，3.2.x amd64），镜像存取走本地盘/**SSH/Samba/NFS** | 单台/少量、救援、做金像 —— **NexOS P1 推荐** |
| **Live 的 lite server 模式**（≥2.5.2-17） | 一台机用 Live ISO 起"轻服务器"，其余机器 PXE 连它批量部署 | 免装 DRBL 的中间档，批量铺机小规模可用 |
| **Clonezilla SE（DRBL 全家桶）** | 专用 DRBL 服务器（DHCP+TFTP+NFS+clonezilla），客户端**磁盘-less PXE**，支持 multicast 组播批量 | 机房级大规模；需独占 DHCP 或 proxyDHCP，企业网协调成本高 —— 本期不引入 |

- **镜像格式**：partclone 流式镜像 + gzip/zstd（可选 xz）压缩 + 分卷，**不可直接挂载**；
  `dd` 模式全盘位级保真（兼容所有文件系统但体积大）。支持 ZFS/btrfs/LVM 感知（按
  分区类型选 partclone.* 或 dd 兜底）。
- **PXE 起 Clonezilla**：其 ISO 内含 pxelinux 网络引导件（vmlinuz+initrd+filesystem
  .squashfs 提取到 TFTP/HTTP + `ocs_live_run` 参数脚本化），即"小 ISO 输 IP"的同一
  iPXE 通道也能 chain Clonezilla——**引导器复用 iPXE，Clonezilla 只是菜单里的一项**。

### 4.2 与 NexOS 集成（镜像仓 + 参考机金像流程）

- **镜像仓**：`/tank/clonezilla/<image-name>/`（partclone 分卷）。106 已有 Samba
  （`nexos-downloads` 先例）——新增只读共享或专用账户，克隆端"device-image"存取走
  `smb://<nexos>/clonezilla`；HTTP 只出清单/元数据（体积、时间、源机、含 NexOS 版本）。
- **参考机（如 106）→ 金像 → 批量铺新机流程**：
  1. 参考机装好 Ubuntu+NexOS+驱动，跑 **NexOS sysprep 化**（§4.3，清除机器态）；
  2. iPXE 菜单选 Clonezilla（或 U 盘）→ `savedisk` 到 `smb://<nexos>/clonezilla`；
  3. 新机 iPXE 菜单选"恢复镜像" → `restoredisk`（lite server 模式可小批量并发）；
  4. 首启 firstboot unit 重生成身份 → 自动入集群（与 autoinstall 路线同一下车点）。

### 4.3 克隆身份冲突与业界惯例（关键合规点）

业界"Linux sysprep"惯例（virt-sysprep 清单 / Super User 经典问答）：

```sh
truncate -s 0 /etc/machine-id /var/lib/dbus/machine-id   # systemd 首启重建 machine-id
rm -f /etc/ssh/ssh_host_*                                # 首启 ssh-keygen -A / systemd 重生成
echo "" > /etc/hostname                                  # 由 firstboot/用户重命名
rm -f /var/lib/systemd/random-seed /var/lib/dhcp/*       # 随机种子/DHCP 租约残留
```

**NexOS 特有必须再加**（机器态，p2p 密钥/certs/NodeID —— 参照 §3.19 敏感排除清单
与 `os-provision/src/exclude.rs` 的既有边界）：
- P2P 节点密钥对/证书/NodeID 注册态：firstboot 调 identity 组件**重生成**，并向
  bootstrap 节点重新注册（旧 NodeID 在注册表中自然过期）；
- `/tank/os-data/*` 运行期 DB（gateway/monitor/…）：金像化时清空或重建（与
  `init_script.rs` 的 `first-boot.flag` 首启强制初始化机制同构）；
- 推荐顺序：**镜像做成"已 sysprep 化的干净金像"**（步骤 1 在 save 前做），首启
  firstboot 兜底自检（发现 ID 冲突/first-boot.flag 即自愈重生成）——双保险。

---

## 5. 空间账（106 tank 实测：900G 总 / 21G 已用 / **879G 可用**）

| 项 | 单件体积 | 数量 | 小计 |
|----|----------|------|------|
| Ubuntu live-server ISO（实测/口径见注） | 24.04.3 amd64 **3.08GB**；26.04.1 amd64 **2.73GB**；24.04.x arm64 ≈2.8GB；26.04.x arm64 ≈2.6GB | 4（2版本×2架构） | **≈11.2GB** |
| vmlinuz+initrd（ISO 内提取、白名单分发） | ~30MB+150MB/对 | 4 | ≈0.7GB |
| 引导小 ISO 产物（ipxe.iso 官方/自编译 EMBED） | 0.4KB~9.4MB | 出厂件+自制件 | <0.1GB |
| Clonezilla live ISO | ≈462MB | 1 | 0.5GB |
| **P0+P1 固定件合计** | | | **≈12.5GB** |
| 克隆镜像（压缩率按 used×0.4~0.7 估） | 干净 Ubuntu+NexOS 底座（used ~20-40G）→ 8~28G/像；106 现机全盘（used 244G）→ 100~170G/像 | 预算 8~12 张底座级 | **预算 200~400GB** |
| apt-cacher-ng 稳态（P2） | — | — | 20~50GB |

> 注：24.04.3/26.04.1 amd64 为镜像站 zsync 元数据实测；arm64 为 cdimage 公开列表值。
> **用户预期"~2GB/件"需修正为 2.7~3.1GB/件**。结论：全矩阵 + 克隆仓 + 缓存在 879G
> 内占比 <50%，无压力；大头永远是要不要存"现机全盘金像"。

---

## 6. NexOS 侧架构草案

### 6.1 拓扑

```
【P0 装机主流水线：小 ISO → 输 IP → autoinstall → 入集群】

 裸机/新设备                     局域网 NexOS 节点（如 106, :8558）
 ───────────                    ─────────────────────────────────────────
 ① U 盘/刻盘 iPXE 小 ISO
    （几百 KB~9.4MB，出厂件或
     POST bootstrap-iso 自制）
 ② 启动 → dhcp → prompt 输 IP ──HTTP──▶ GET /provisioning/netboot/boot.ipxe?arch=…
    read nexos_srv                      （按 ISO 仓矩阵动态生成 iPXE 菜单；
                                          缺版本/架构 → 列出并提示 P2 跳板下载）
 ③ menu choose 版本 ◀── iPXE 菜单 ──────┘
 ④ kernel/initrd（HTTP 拉）  ◀────────── /provisioning/netboot/{ver}/{arch}/{vmlinuz,initrd}
 ⑤ casper url= 拉 ISO（~3GB）◀────────── /provisioning/iso/{ver}/{arch}.iso（流式直传+Range）
 ⑥ subiquity autoinstall     ◀────────── /provisioning/autoinstall/{ver}/{arch}/user-data
    （无人值守分区/建用户/装 SSH）           （模板引擎渲染：identity/apt 国内源/proxy）
 ⑦ late-commands:             ─HTTP──▶  GET /provisioning/install.sh（复用现有动态生成）
    curtin in-target curl install.sh      （--source 本 NexOS --bootstrap 本机 P2P）
 ⑧ 重启 → 新 Ubuntu + osd 服务 ──P2P──▶ 7070 bootstrap 拨入 → 网络页可见 = 闭环

【P1 克隆支线：Clonezilla 镜像流】
 参考机(106) --iPXE/U盘 Clonezilla--> savedisk ──SMB──▶ /tank/clonezilla/<gold>/
 新机     --iPXE 菜单"恢复镜像"------> restoredisk ◀─SMB── 同仓
 新机首启 firstboot unit：machine-id/ssh keys/hostname/NexOS NodeID+certs 重生成 → 入集群

【P1 PXE 全自动（免 U 盘）】：WoL/IPMI 上电(power/*) → proxyDHCP+TFTP（PxeServer 落地
 dnsmasq 后端，bootfile 由 PxeConfigBuilder 产出）→ 免输 IP 直入 ③；MAC→版本预注册表。

【P2 跳板下载】：POST iso-repo/sync → aliyun/tuna 直连（实测可达）→ 失败转 jumpbox
 （198.51.100.114 隧道先例）→ zsync 增量刷新点版本 → sha256 校验入仓。
```

### 6.2 provisioning 扩展（端点草案，沿用现有鉴权/白名单风格）

| method/path | 鉴权 | 说明 |
|-------------|------|------|
| `GET /provisioning/netboot/boot.ipxe?arch=&platform=` | 公开 | 动态 iPXE 菜单（`text/plain` 直传）；按 ISO 仓矩阵列版本；扩展 `os-provision::pxe` |
| `GET /provisioning/netboot/{ver}/{arch}/{file}` | 公开 | vmlinuz/initrd 白名单分发（`{file}` 精确名防穿越，同 `DISTRIBUTABLE_ARTIFACTS` 先例） |
| `GET /provisioning/iso/{ver}/{arch}.iso` | 公开 | ISO 仓下载；**须走新的文件流式直传**（现 base64 全内存通道不可承载 3GB，风险 R3） |
| `GET/POST /provisioning/iso-repo`，`POST /iso-repo/sync` | 公开/admin | 矩阵清单（版本×架构×sha256×size×路径）/手动上传登记 / 从国内镜像源同步（P2 加 jumpbox 回退 + zsync 增量） |
| `GET /provisioning/autoinstall/{ver}/{arch}/user-data`（+`meta-data`） | 公开 | nocloud 种子动态渲染（token/hostname/bootstrap 参数注入；可选一次性 token） |
| `POST /provisioning/bootstrap-iso` | admin | 引导小 ISO 构建：官方 iPXE 二进制 + EMBED 引导脚本 → xorriso 双启 ISO（复用 os-iso XorrisoIsoBuilder + build-iso.sh 任务形态） |

磁盘布局（env `NEXOS_ISO_REPO`，缺省 `/tank/os-data/iso-repo/<ver>/<arch>/`，
镜像仓 `/tank/clonezilla/`）。

---

## 7. 选型对比矩阵（汇总）

### 引导器
| 方案 | 体积 | 输 IP 交互 | 服务端动态菜单 | SB | 双架构 | 推荐 |
|---|---|---|---|---|---|---|
| iPXE | 71KB~9.4MB | prompt/read 原生 | chain 脚本 | 需 shim | x64/arm64 EFI+BIOS | ★★★★★ |
| grub2-mkrescue+grubnet | 10~30MB | 弱 | 静态 grub.cfg | 签名件可过 | 是 | ★★★☆（SB 备选） |
| Ventoy | 工具+ISO 原样 | 无 | 无 | 好 | 是 | ★★（离线兜底） |
| systemd-boot | <1MB | 无网络栈 | — | — | UEFI | ✗ |

### 装机方式
| 方案 | 无人值守 | 依赖外网 | 版本贴合（24.04→24.04） | 空间 | 推荐 |
|---|---|---|---|---|---|
| subiquity autoinstall + 本地 ISO 仓 | ✓（version+identity 即全自动） | 否（offline-install 兜底） | ✓（按目标选版本菜单项） | 11.2GB 矩阵 | ★★★★★（P0） |
| autoinstall + apt-cacher-ng | ✓ | 部分 | ✓ | 缓存 20~50GB | ★★★★（P2 叠加） |
| 自研 initramfs 脚本（init_script.rs+ZFS squashfs） | ✓ | 否 | 自定义 rootfs | 单 squashfs | ★★★（NexOS 自有 OS 路线并行，非"装 Ubuntu"诉求） |
| Clonezilla 整机恢复 | ✓ | 否 | 金像固定不可选版本 | 100G+/像 | ★★★★（同硬件批量，P1） |

### 克隆方案
| 方案 | 企业网友好 | 批量 | 与 NexOS 存储 | 推荐 |
|---|---|---|---|---|
| Clonezilla Live + /tank SMB/NFS 仓 | ✓（不动网络） | 小批量（lite server 可并发） | 复用 Samba 先例 | ★★★★★（P1） |
| Clonezilla SE/DRBL | ✗（要 DHCP/TFTP 主导权） | 组播大规模 | NFS | 暂不引入 |
| 自研 ZFS send/recv（os-provision::transfer） | ✓ | 点对点 | tank 原生 | ★★★★（NexOS 节点间迁移主路径，与 Clonezilla 互补：异构新机用克隆、同池节点用 ZFS） |

---

## 8. 分期（P0–P2）

**P0（小 ISO + 手动选版本 autoinstall + ISO 仓）—— 本期目标**
1. ISO 仓：`iso-repo` 目录约定 + 清单端点 + `netboot/{ver}/{arch}` 白名单分发；
   ISO 获取先手动（scp/脚本下载 24.04.x/26.04.x × amd64/arm64 四件，官方 sha256 对拍）；
   从每件 ISO 提取 vmlinuz/initrd 落仓。
2. autoinstall 模板引擎：user-data 渲染（identity/apt 国内源/fallback offline/`late-commands`
   curl install.sh）+ nocloud 种子目录端点。
3. 引导小 ISO：直接下发官方 `ipxe.iso`（9.4MB）+ 仓库内 `bootstrap.ipxe`（扩展
   `PxeConfigBuilder`：dhcp→prompt 输 IP→chain 动态菜单）；自编译 EMBED 版作可选优化。
4. **新文件流式直传通道**（`direct_passthrough_bytes` 旁路：fs 流 + Range + sha256 头），
   供 ISO/kernel/initrd 大件使用（现 base64 通道仅留给小件）。
5. 前端 Provisioning 页新增"网络装机"Tab：ISO 仓矩阵 + autoinstall 参数表单 +
   bootstrap-iso 下载 + 装机进度观察（目标机 subiquity 日志拉取可后置）。

**P1（PXE 全自动 + 克隆铺机）**
1. proxyDHCP+TFTP（dnsmasq 后端实现 `os-network::PxeServer` trait；与既有 DHCP 共存）；
   MAC→版本/镜像 预注册表 → 免输 IP 全自动。
2. WoL/IPMI（power/* 已就绪）→ PXE → 装机/恢复 一键流水线编排。
3. Clonezilla：live ISO 入仓 + iPXE 菜单项 + `/tank/clonezilla` SMB 共享 + 参考机
   sysprep 化脚本 + firstboot 重身份 unit（machine-id/ssh keys/NexOS NodeID+certs）。

**P2（跳板下载 + 缓存代理）**
1. `iso-repo/sync` 自动化：aliyun/tuna 直连优先（实测 amd64 直连可达；arm64 走
   ubuntu-cdimage 类镜像）→ 失败转 **jumpbox 隧道**（Blender tar 先例）→ zsync 增量
   刷新点版本；点版本升级提醒。
2. apt-cacher-ng（3142）部署 + autoinstall `apt.proxy` 注入 + 既有节点 apt 配置回收。
3. 一段式 token：装机种子的一次性凭证（替代 install.sh 默认 token 暴露面）。

---

## 9. 风险

| # | 风险 | 影响 | 缓解 |
|---|------|------|------|
| R1 | **UEFI Secure Boot**：iPXE EFI 未签名，SB 开启的机器拒启 | 新 UEFI 机首启失败 | a) 官方路径：shim+grubnet 签名链（`shim-signed`+`grubnetx64_efi_signed`，TFTP/HTTP 引导 grub.cfg）；b) 引导菜单引导用户关 SB；c) BIOS 模式 U 盘不受影响（P0 主路径） |
| R2 | **企业网 PXE 权限/冲突**：既有 DHCP 不可控、交换机 DHCP snooping | P1 全自动受阻 | P0 刻意"U 盘小 ISO+输 IP"完全绕开 PXE；P1 用 proxyDHCP（不夺租约）+ 失败回退 U 盘路径 |
| R3 | **3GB ISO 经现有 base64→octet-stream 通道全内存装载**：OOM/不可用 | 装机中断 | P0 必做文件流式直传（fs→socket 流 + Range 断点续传；casper url= 拉取可并发受益） |
| R4 | **克隆镜像身份冲突**：machine-id/ssh host keys/NexOS p2p 密钥/NodeID/证书重复 | 集群身份互撞、SSH 中间人告警 | 金像前 sysprep 化 + 首启 firstboot 重生成双保险（§4.3 清单）；恢复后立即向新 bootstrap 重注册 |
| R5 | **mini.iso 已死、ISO 比预期大**（2.7~3.1GB/件） | 空间/下载时长预期偏差 | 空间账按 §5 修正（12.5GB 固定件）；zsync 增量（P2）；内网千兆 30~60s/次可接受 |
| R6 | **26.04 subiquity 行为变化**（mirror 自动择优/geoip） | 无人值守镜像选择不可控 | autoinstall 显式 `mirror-selection.primary`（国内镜像）+ `geoip: false` |
| R7 | **arm64 ISO 国内镜像不全**（ubuntu-ports/cdimage 路径与 amd64 不同源） | ARM（DGX Spark 等）装机源缺失 | sync 端点维护 per-arch 镜像源表；P2 jumpbox 回退（既有先例） |
| R8 | 桌面版 autoinstall 虽官方化但桌面 ISO ~6GB/件 | 仓体积翻倍 | 默认只备 server；desktop 按需手动入仓（清单端点标注 missing→按需下载） |

---

## 10. 开放问题（需拍板）

1. **双路线关系**：Ubuntu 底座（autoinstall 装标准系统 + install.sh 装 NexOS，P0 主线）
   与自研 rootfs（os-iso squashfs+ZFS、init_script.rs 阶段1）如何并存/收敛？
   建议定位：前者服务"把外部 Ubuntu 设备收编入集群"，后者服务"NexOS 自有 OS 发行"。
2. autoinstall 装机时 **install.sh 的 token 注入**：一次性 token 端点（P2）之前的
   过渡方案——渲染种子时嵌入临时强 token？
3. 克隆金像选型：**干净底座金像**（小、通用）vs **106 现机全盘**（大、含全部数据）；
   建议底座金像 + 数据走 ZFS send/recv（transfer.rs 路线）分而治之。
4. 是否需要**断网环境的完整 deb 镜像**（apt-mirror 全量 ~1TB 级不可行；
   仅 `offline-install` 兜底是否满足目标场景）？
5. bootstrap-iso 自编译 EMBED（需 iPXE 构建工具链入 os-iso 依赖清单）vs 直接分发
   官方 ipxe.iso + 独立引导脚本——P0 取简，P1 再裁剪体积？

---

## 11. 参考资料（调研来源）

- iPXE：chainloading howto / download（ipxe.org；boot.ipxe.org 实测体积 2026-09-18）
- Ubuntu 官方：How to netboot the server installer（ubuntu.com/server/docs/install/netboot-amd64）；
  Automated Server install quickstart / Autoistall configuration reference
  （canonical-subiquity.readthedocs-hosted.com，2026-07）；"Netbooting the live server
  installer"（discourse.ubuntu.com——d-i/mini.iso 23.04 起停产）
- Subiquity 24.04.1 release notes（桌面 autoinstall 官方化）；canonical/autoinstall-desktop（GitHub）
- 镜像实测：aliyun/tuna `ubuntu-releases` 24.04.3=3.08GB / 26.04.1=2.73GB（zsync 元数据）；
  cdimage arm64 ≈2.8GB（24.04.x）
- Clonezilla：clonezilla.org（SE/Live/lite-server 模式说明）、changelog（3.2.x ISO ≈462MB，
  DistroWatch）；DRBL fine print（drbl.org）
- 克隆身份惯例：Super User "What must be changed for cloned Linux systems"、
  virt-sysprep 操作清单（libguestfs）、openssh host key 重生成讨论（mindrot）
- apt 缓存：apt-cacher-ng（Ubuntu Community Help / Debian Wiki）vs squid-deb-proxy（AskUbuntu 对比帖）
- 仓库内：`docs/PROVISIONING.md`、`docs/BOOTSTRAP_INSTALL.md`、`crates/os-provision/src/{pxe,init_script,transfer,exclude}.rs`、
  `crates/os-api/src/handlers/{provisioning,power}.rs`、`scripts/build-iso.sh`、MEMORY.md（jumpbox/Samba 先例）
