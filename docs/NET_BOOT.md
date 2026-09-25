# 网络装机（NET_BOOT P0）：U 盘 iPXE → 输 IP → 全自动装 Ubuntu + 入集群

> 对应桌面应用「系统自举 → 网络装机」Tab（`crates/os-api/web/src/views/Provisioning.vue`）；
> 后端 `crates/os-api/src/handlers/provisioning.rs`（端点 + 流式直传 + ISO 仓管理）与
> `crates/os-api/src/http.rs`（特挂流式路由）；纯逻辑契约在 `crates/os-provision/src/netboot.rs`
> + `iso9660.rs`（内置 ISO9660 提取器）。调研依据 `docs/research/NET_BOOT_PROVISIONING.md`。
>
> **一句话**：给任意裸机/新机装 Ubuntu Server 并自动加入 NexOS 集群——U 盘只烧一次
> （官方 `ipxe.iso`，9.4MB），版本/菜单/种子永远来自局域网 NexOS 节点；全程内网可用，
> 装完自动执行 `install.sh` 入集群（闭环）。

## 0. 工作原理（引导链）

```
裸机/新设备                          局域网 NexOS 节点（:8558）
──────────                          ─────────────────────────
① U 盘 iPXE 引导（官方 ipxe.iso）
② dhcp → prompt 输 NexOS IP
   （回车 = 缺省源 IP）   ──HTTP──▶  GET /api/v1/provisioning/bootstrap.ipxe
③ 菜单选版本             ◀──iPXE──  GET /api/v1/provisioning/ipxe/menu?arch=&platform=
                                   （按 ISO 仓实时生成：每架构只列最新稳定版）
④ kernel/initrd          ◀────────  GET /api/v1/provisioning/boot/<id>/{vmlinuz,initrd}
⑤ casper url= 拉完整 ISO  ◀────────  GET /api/v1/provisioning/isos/<file>
   （流式直传 + Range 断点）
⑥ autoinstall 种子        ◀────────  GET /api/v1/provisioning/seed/<id>/{user-data,meta-data}
   （无人值守：dhcp/单盘直铺/账号/SSH）
⑦ late-commands           ──HTTP──▶  GET /api/v1/provisioning/install.sh
   curtin in-target 执行            （装完 Ubuntu 自动 systemd 化 + 入集群）
⑧ 重启 → 新 Ubuntu + osd ──P2P──▶   :7070 bootstrap 拨入 = 闭环
```

内核 cmdline（冻结形态）：

```
kernel http://<ip>:8558/api/v1/provisioning/boot/<id>/vmlinuz \
    root=/dev/ram0 ramdisk_size=1500000 ip=dhcp \
    url=http://<ip>:8558/api/v1/provisioning/isos/<file> \
    autoinstall ds=nocloud-net\;s=http://<ip>:8558/api/v1/provisioning/seed/<id>/
initrd http://<ip>:8558/api/v1/provisioning/boot/<id>/initrd
boot
```

## 1. 版本策略（滚动只跟最新稳定版——用户拍板，固化）

1. **自动装机（iPXE 菜单/autoinstall）只提供仓内最新稳定版**（当前
   `26.04.1`）：菜单对每架构只渲染 `latest=true` 的条目。
2. **新点版本发布后滚动替换**（如 26.04.2）：新版 ISO 入仓（下载/放置 +
   `POST /isos/scan`）→ **删除旧版**（`DELETE /isos/:file`，或自举页「删除」
   按钮）——本批提供"标记最新 + 删除旧版"两步操作；**自动检查新版并下载留
   P2 跳板下载批**。
3. **老版本（如 24.04）自动流程不提供**：需要时用户手动装机，或**手动放
   ISO 仓**（目录约定见 §4——任何 `ubuntu-<版本>-live-server-<amd64|arm64>.iso`
   形态的文件 `scan` 后即入清单，可下载手动装机，但不进自动菜单）。
4. 空间口径：26.04 双架构 ≈ **5.7GB**（amd64 2.87GB + arm64 3.08GB）+ 提取件
   ≈0.29GB + ipxe.iso 9.4MB；106 tank 875GB 可用，无压力。

## 2. 使用手册（真机三步）

### 第 1 步：制作 U 盘（三法任选）

先在自举页「网络装机」Tab 下载**官方 `ipxe.iso`**（`GET /api/v1/provisioning/isos/ipxe.iso`，
9.4MB，BIOS+UEFI 双启动；也可从 [boot.ipxe.org](http://boot.ipxe.org/ipxe.iso) 自取——
仓内件与本机 sha256 一致）。

| 方法 | 操作 | 适用 |
|------|------|------|
| **balenaEtcher**（图形，推荐） | 打开 Etcher → *Flash from file* 选 `ipxe.iso` → *Flash to* U 盘 | Windows/macOS/Linux |
| **dd**（命令行） | `sudo dd if=ipxe.iso of=/dev/sdX bs=4M status=progress oflag=direct`（`/dev/sdX` = U 盘设备，**别写错盘**） | Linux/macOS |
| **Ventoy**（多 ISO 随身库） | 装 Ventoy 到 U 盘 → 把 `ipxe.iso` 拷进 U 盘 → 启动菜单里选它 | 一个 U 盘装多个 ISO |

> 自编译 EMBED 版（几百 KB、内置 bootstrap.ipxe 免交互）留 P1 优化；
> `bootstrap.ipxe` 可随时从 `GET /api/v1/provisioning/bootstrap.ipxe` 下载（iPXE
> `chain` 用的就是它，U 盘版 ipxe.iso 无 EMBED，菜单链路不受影响）。

### 第 2 步：目标机引导，输入 NexOS IP

1. 插 U 盘开机，选 U 盘启动项（UEFI 机器**先关 Secure Boot**，见 §6 故障）。
2. 官方 `ipxe.iso` 起来后停在 `iPXE>` 提示符（开机瞬间若自动跳过，按
   `Ctrl+B` 中断进 shell），输入一条命令拉起引导链（`<ip>` = NexOS 节点 IP，
   生产 unit 端口 8558）：
   ```
   iPXE> chain http://<ip>:8558/api/v1/provisioning/bootstrap.ipxe
   ```
3. 之后全自动：iPXE 自动 `dhcp`，然后提示：
   ```
   === NexOS 网络装机 ===
   本机 efi/x86_64  网卡 52:54:00:xx:xx:xx
   请输入局域网 NexOS 服务器 IP（直接回车 = <缺省>）
   NexOS IP:
   ```
4. **直接回车** = 缺省源 IP（DHCP next-server/DHCP 服务器——NexOS 兼任 DHCP 时
   即它），或手工输入 NexOS 节点 IP（如 `192.0.2.106`）。连不上会回到输入
   界面重试。
   > 注：自编译 EMBED 版（把 bootstrap.ipxe 内置进 ipxe.iso，免输 chain 命令）
   > 留 P1；P0 用官方件，多打一行命令。

### 第 3 步：选版本 → 全自动安装 + 入集群

1. 菜单列出该架构**最新稳定版**（如 `Ubuntu Server 26.04.1（amd64）`，缺省项），
   上下键选择回车（`Clonezilla 克隆整机` 为 P1 占位灰显；另有 iPXE Shell 救援、重启）。
2. 确认提示后自动执行：拉 vmlinuz/initrd → casper HTTP 拉完整 ISO（内网千兆
   30~60s）→ **autoinstall 无人值守安装**：
   - 网络 dhcp；键盘/语言固定；
   - **存储单盘全清直铺**（`layout: direct` + 最大盘——**目标盘数据会全部清除**）；
   - 账号 `nexos` / `nexos`（**首启请改密**）；SSH 装服务端（允许密码登录）；
   - apt 国内镜像（26.04 显式 `geoip: false` 防内网卡探活）+ `offline-install` 兜底。
3. 安装收尾 `late-commands` 在 chroot 内执行本节点的 `install.sh`
   （`--source`/`--bootstrap` 指回本节点）→ 装完即 **systemd 服务化 + P2P 拨入
   本节点**：新机出现在「网络」页 = 闭环完成（Admin Token 为缺省 `change-me-admin-token`，
   请尽快更换）。

## 3. 端点表（前缀 `/api/v1/provisioning`）

| method | path | 鉴权 | 说明 |
|--------|------|------|------|
| GET | `/bootstrap.ipxe` | 公开 | U 盘 iPXE 第一跳脚本（text/plain；dhcp→prompt 输 IP→chain 菜单） |
| GET | `/ipxe/menu?arch=&platform=` | 公开 | 动态菜单（按仓实时渲染；每架构只列最新稳定版；Clonezilla 占位灰显） |
| GET | `/seed/:id/meta-data` | 公开 | nocloud 空 meta（约定） |
| GET | `/seed/:id/user-data` | 公开 | autoinstall 种子（版本/架构/Host 头来源参数化；24.04/26.04 分支） |
| GET | `/isos` | 公开 | ISO 仓清单（文件/版本/架构/size/sha256/提取状态/latest） |
| POST | `/isos/scan` | admin | 登记校验 + **后台** sha256 实算对账（写 `<file>.sha256` sidecar）+ casper 提取 |
| DELETE | `/isos/:file` | admin | 删旧版（滚动策略：ISO + sidecar + `boot/<id>/` 提取件） |
| GET | `/isos/:file` | 公开 | **完整 ISO 流式直传**（tokio::fs 分块流；`Range`→206/416；`accept-ranges`；`x-nexos-sha256`） |
| GET | `/isos/ipxe.iso` | 公开 | 引导介质直传（精确名白名单，同上 Range 语义） |
| GET | `/boot/:id/:file` | 公开 | casper 提取件直传（`{id}`=版本-架构白名单，`{file}`∈{vmlinuz,initrd}） |
| GET | `/install.sh` | 公开 | 一键安装脚本（⑦ 闭环复用；chroot 容忍：无运行 systemd 时离线 enable） |

> 后两条流式路由为 **axum 特挂**（`http.rs` build_router，同 `/git/*` 先例）：
> 3GB 级 ISO 走 `tokio::fs` 分块流直出 socket，**不进** base64 网关 body
> （`direct_passthrough_bytes` 的流式扩展）。`GET /isos/:file` 白名单 = 仓文件名
> 精确形态（`ubuntu-<点分版本>-live-server-<amd64|arm64>.iso`）+ 引导介质常量表；
> `boot/:id/:file` 白名单 = id 回构校验 + 文件名枚举——任何 `../`、`\`、编码、
> 绝对路径形态都无法命中（不存在基于用户输入的路径拼接）。

## 4. ISO 仓目录约定（手动放置兜底）

```
/tank/os-data/provision/                    # 仓根（env NEXOS_PROVISION_REPO 覆盖）
├── isos/
│   ├── ubuntu-26.04.1-live-server-amd64.iso     # OS 件（2.87GB，官方 sha256 对拍过）
│   ├── ubuntu-26.04.1-live-server-amd64.iso.sha256   # 对账 sidecar（scan 后自动生成）
│   ├── ubuntu-26.04.1-live-server-arm64.iso     # 3.08GB
│   ├── ubuntu-26.04.1-live-server-arm64.iso.sha256
│   ├── ipxe.iso                                  # 官方引导介质（9.4MB，boot.ipxe.org）
│   └── SHA256SUMS*                               # 镜像站原始对账文件（留档，不参与逻辑）
└── boot/
    └── 26.04.1-amd64/
        ├── vmlinuz        # 从 ISO casper/ 提取（17.3MB）
        └── initrd         # 99.7MB（arm64: 23.8MB / 152MB）
```

- **手动放置**：把任何 `ubuntu-<版本>-live-server-<amd64|arm64>.iso` 形态文件放进
  `isos/` → 自举页点「扫描登记」（或 `POST /isos/scan`）→ 后台自动 sha256 实算 +
  写 sidecar + **内置 ISO9660 提取器**提取 casper 引导件（纯 Rust，无需 7z/bsdtar/
  xorriso，目录记录布局按真实 Ubuntu ISO 校准）。
- 下载落仓（本批实际操作）：amd64 走 NJU 镜像（mirror.nju.edu.cn，实测 ~9MB/s；
  aliyun 302 到内网不可达、tuna 403）；arm64 走 cdimage.ubuntu.com 直连（唯一
  官方源，国内镜像均只同步 streams 元数据）。sha256 与官方 `SHA256SUMS` 对拍一致。
- 点版本升级（如 26.04.2 发布）：下新 ISO 入仓 → scan → `DELETE` 旧版（滚动策略 §1）。

## 5. 环境变量

| env | 缺省 | 说明 |
|-----|------|------|
| `NEXOS_PROVISION_REPO` | `/tank/os-data/provision` | 仓根（isos/ + boot/） |
| `NEXOS_GIT_ADVERTISE_HOST` / `NEXOS_P2P_ADVERTISE` | — | Host 头缺失时种子/install.sh 来源回退（同 dist 通道） |
| `NEXOS_AUTH_DEFAULT_ADMIN` | `1` | 测试期默认 admin（隔离验证用；生产关） |

## 6. 故障排查

| 症状 | 原因/处置 |
|------|-----------|
| U 盘引导后黑屏/直接跳过 | 启动项没选对；BIOS 机器选 legacy USB 启动项；官方 ipxe.iso 自检后停 `iPXE>` 提示符是正常状态（见 §2 第 2 步，`Ctrl+B` 可中断进 shell） |
| **iPXE 报 "Could not start" / 安全启动拒绝** | **UEFI Secure Boot 开着**——官方 ipxe.iso 未签名，进 BIOS/UEFI 关闭 Secure Boot 后重试（P0 主路径；shim+grub 签名链留 P1） |
| 输 IP 后 "Connection refused" | NexOS 节点 8558 端口不可达：防火墙放行 / 服务未起（`curl http://<ip>:8558/healthz` 自检） |
| 菜单提示"仓内没有该架构的可装机 ISO" | 该架构 ISO 未入仓或未 scan；`GET /isos` 看 `extraction_status`，必要时重跑 scan |
| `isos/<file>` 返回 404 + scan 指引 | 文件不在仓/未登记/名字形态不符（目录约定 §4） |
| 安装卡在拉 ISO | 目标机与节点间带宽/网线；流式直传支持 Range 断点，iPXE 会自动重试；服务端日志看请求 |
| sha256_status=mismatch | ISO 损坏（下载中断）：重下替换后重跑 scan（sidecar 自动重算） |
| 装完没入集群 | 目标机控制台/`journalctl -u nexos-os-api` 查 install.sh 步骤；确认 `--bootstrap` 指向的节点 7070 可达 |
| 默认凭据忘了 | `nexos`/`nexos`（autoinstall 种子写死，`docs` 即本文件 §2）；Admin Token 缺省 `change-me-admin-token`（install.sh 既有口径） |

## 7. 实现分层与测试

- **os-provision**（纯逻辑契约，`netboot.rs`/`iso9660.rs`）：引导链脚本渲染、
  种子模板（24.04/26.04 分支）、仓条目模型（latest 标记）、ISO9660 只读提取器；
  `tests/netboot_real.rs` 对仓内真实 ISO 提取 casper 并校验体积/幻数
  （`--ignored` 真机跑）。
- **os-api**（HTTP 侧自包含实现，与 os-provision 无依赖边——Cargo.toml 不动；
  PXE 域"搬自 pxe.rs"同款双轨）：端点 + 流式直传 + 仓管理；
  单测覆盖 bootstrap/菜单快照、种子版本差异、白名单穿越拒、Range 206/416、
  scan/对账/删除、伪 ISO 提取失败路径。
- P1 展望：proxyDHCP+TFTP 免 U 盘、Clonezilla 克隆铺机、WoL/IPMI 全自动流水线；
  P2：跳板下载补齐其它版本 ISO、zsync 增量、一次性 token 种子鉴权。
