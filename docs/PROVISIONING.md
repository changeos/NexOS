# 系统自举（Provisioning）：PXE / ISO 生成 / SSH 远程部署 / 电源控制

> 对应桌面应用「系统自举」（`crates/os-api/web/src/views/Provisioning.vue`），
> 后端入口 `crates/os-api/src/handlers/provisioning.rs`（`ProvisioningRouteHandler`）
> 与 `crates/os-api/src/handlers/power.rs`（`PowerRouteHandler`，电源控制层）。
> 前期把两处留桩做实：**SSH 部署真实 scp/ssh 执行**、**ISO 真实驱动
> os-iso `XorrisoIsoBuilder` 构建**（mksquashfs → xorriso → sha256sum 子进程）；
> 2026-08-25 新增**电源控制层**（IPMI 对内对外 + LAN 魔术唤醒）——
> PXE 装机流水线的**第一环**：先唤醒/上电，再 PXE 引导，最后 SSH 部署收尾。

## 1. 定位：更新通道 + 自举 = 后续两大更新/部署手段

系统自举与「更新通道」（docs/UPDATE_APP.md）互补：

| 维度 | 更新通道（update） | 系统自举（provisioning） |
|------|-------------------|--------------------------|
| 对象 | 已在运行的 OS 实例（A/B 槽位滚动） | 裸机 / 新节点 / 不可启动的节点 |
| 手段 | semver 通道 + 产物下载 + 槽位切换 | PXE 网络引导 / ISO 镜像 / SSH 远程推送 |
| 时机 | 例行升级 | 冷启动、批量铺机、**救援** |

**SSH 远程部署是运维兜底手段**：当日（2026-08-25）cron 救援实战再次验证了
「网关进程 spawn ssh/scp 子进程（BatchMode 密钥认证）」这一模式的可靠性——
节点侧 agent / 更新通道 / P2P 全都不可用时，只要 SSH 还活着，就还能把修复
文件（配置、二进制、cron 单元）推上去并把命令跑了。本应用的 `POST /ssh/deploy`
就是把这条兜底通道产品化：文件级进度 + 命令输出全部落任务记录，页面上可见。

## 2. 端点契约（21 条，前缀 `/api/v1/provisioning`）

| method | path | 鉴权 | 动作 |
|--------|------|------|------|
| GET  | `/pxe/config` | 公开 | PXE 配置 |
| POST | `/pxe/config` | admin | 更新配置 |
| GET  | `/pxe/boot-entries` | 公开 | 启动条目列表 |
| POST | `/pxe/boot-entries` | admin | 添加条目（新 default 会清掉旧 default，单锁作用域） |
| DELETE | `/pxe/boot-entries/:id` | admin | 删条目 |
| GET  | `/pxe/status` | 公开 | 服务状态（内存态 running/stopped） |
| POST | `/pxe/start` / `/pxe/stop` | admin | 启停（本期仍为标记位） |
| GET  | `/iso/tasks` | 公开 | ISO 任务列表（building 态带实时 step/progress/build_log） |
| POST | `/iso/tasks` | admin | 建任务（pending，不触发构建） |
| DELETE | `/iso/tasks/:id` | admin | 删任务 |
| GET  | `/iso/tasks/:id` | 公开 | 任务详情（同上 hydration） |
| POST | `/iso/tasks/:id/build` | admin | **启动真实构建**（见 §4） |
| GET  | `/ssh/targets` | 公开 | SSH 目标列表（无 password 字段） |
| POST | `/ssh/targets` | admin | 添加目标 |
| DELETE | `/ssh/targets/:id` | admin | 删目标 |
| POST | `/ssh/targets/:id/test` | admin | 测试连接（真实 `ssh echo os-ok`） |
| POST | `/ssh/deploy` | admin | **发起真实部署**（见 §3） |
| GET  | `/ssh/deploys` | admin | 部署任务列表（最新在前；含输出故收紧） |
| GET  | `/ssh/deploy/:id` | admin | 部署任务详情（文件级结果 + 命令输出） |
| GET  | `/stats` | 公开 | 聚合统计 |

### 部署请求体

```json
POST /api/v1/provisioning/ssh/deploy
{
  "target_id": "ssh-1",
  "files": [{"local_path": "/tank/cfg/hosts", "remote_path": "/etc/hosts"}],
  "run_cmd": "systemctl restart cron"
}
```

- `files` 与 `run_cmd` 至少提供一项（否则 400）。
- 同一 target 同时只允许一个进行中的部署任务：重复发起返回 **409**
  `{"error": "...", "deploy_id": "<进行中的任务 id>"}`。
- 内存态最多保留最近 100 条部署记录。

### 部署任务（DeployTask）响应字段

`results[]`（与请求 files 对齐）：`local_path / remote_path / status
(pending|success|failed|skipped) / exit_code / duration_ms / error`；
`cmd_output`：`exit_code / stdout / stderr / duration_ms`（各截 8KB）；
另有 `started_at / finished_at / error`。

## 3. SSH 部署：子进程参数清单与状态机

### 3.1 参数（安全基线）

公共选项（ssh 与 scp 一致，scp 端口用大写 `-P`）：

```
-o BatchMode=yes                 # 禁密码交互（纯密钥认证，杜绝挂死在提示符）
-o ConnectTimeout=10             # TCP 建连超时
-o StrictHostKeyChecking=accept-new  # 首连自动接受新主机密钥（已知主机变更仍拒绝）
-p <port>（ssh）/ -P <port>（scp）
-i <private_key_path>            # 未配置时默认 ~/.ssh/id_ed25519
```

执行形态（本地 spawn 不经 shell，argv 直传，无本地注入面）：

1. 远端目录预创建（多个父目录去重后一次完成）：
   `ssh <opts> user@host "mkdir -p '/etc/app' '/usr/local/bin'"`
   （路径经单引号 POSIX 引用：`'` → `'"'"'`）
2. 逐文件传输：`scp <opts> local user@host:remote`
3. 可选远程命令（单 argv 直传，远端经 `sh -c` 解析）：
   `ssh <opts> user@host "sh -c 'systemctl restart cron'"`

超时（`tokio::time::timeout` 包 spawn + `kill_on_drop(true)` 强杀）：
- 单文件 scp：**300s**；mkdir / run_cmd：**120s**；test 连接整体：15s。
- 超时判 `failed`，`error` 附「超时（Ns）被终止」。

输出捕获：stdout/stderr 各截 **8KB**（`…[截断，原 N 字节]` 标记），防内存放大。

### 3.2 状态机

```
POST /ssh/deploy ──► pending ──► transferring ──► (running) ──► completed
                       │            │                │
                       │            ├─ scp 失败 ──────┴──► failed（后续文件 skipped）
                       │            └─ mkdir 失败 ────────► failed（全部文件 failed，未尝试传输）
                       └─ run_cmd 非零退出 ────────────────► failed（cmd_output 已记录）
                                  （无 files 时 pending → running）
```

每次状态流转即时落任务记录（`patch_deploy` 短临界区回写），前端 1.2s 轮询
`GET /ssh/deploy/:id` 即可看到文件级 ✓/✗ 与命令输出。

### 3.3 锁序（防死锁）

`busy_deploys`（target→deploy 互斥）与 `deploy_tasks` **不嵌套**：
POST 侧 busy 短临界区预留 → 释放 → 写任务表；后台 `finish_deploy` 先写
任务表（释放）再释放 busy。全库统一锁序：busy → counter；deploy_tasks → busy（顺序获取）。

## 4. ISO 构建：os-iso 驱动 + 日志 runner

### 4.1 流程

```
POST /iso/tasks/:id/build
  ├─ 状态预检：building → 409；completed → 409（重建请新建任务）
  ├─ 工具链探测（os_iso::env::IsoEnvironment::probe，xorriso + mksquashfs）
  │    缺失 → 任务 failed，error 附「sudo apt install xorriso squashfs-tools」指引
  ├─ 产物根目录 create_dir_all（env NEXOS_ISO_OUT，默认 ./build/iso）
  ├─ IsoSpec 派生（std/clone 变体、arch 校验 x86_64/aarch64）
  └─ tokio::spawn 后台构建（os-iso XorrisoIsoBuilder + LoggingIsoRunner）
       pending → building(step/progress) → completed(iso_path/sha256/size_bytes) / failed(reason)
```

- 构建是**真实子进程**：`mksquashfs`（源树→squashfs）→ `xorriso -as mkisofs`
  （打可启动 ISO）→ `sha256sum`（产物摘要）。os-iso 只派生路径不建目录，
  os-api 侧 `LoggingIsoRunner` 补齐关键目录预创建（mksquashfs 源/输出、
  xorriso `-o` 产物、ISO 根树）。
- **日志**：`LoggingIsoRunner` 实现 os-iso 的 `runner::IsoBuildRunner` trait，
  每步记录完整命令行 + 退出码 + stdout/stderr 摘要（各截 2KB）到任务
  `build_log`（上限 500 行）；building 期间 GET 端点实时合并，终态后存快照。
  **不改 os-iso pub API**（os-iso 本期零改动）。
- 取舍说明：真实可启动 ISO 仍需已准备的 rootfs 源树与引导文件
  （`NEXOS_ISO_OUT/<task-uuid>/tree/`）；源树缺失时 mksquashfs/xorriso 会
  以失败告终，stderr 在 build_log 里可直接看到——这是有意的最小真实版：
  子进程真实执行 + 失败可见，而不是假成功。

### 4.2 环境变量

| env | 默认 | 说明 |
|-----|------|------|
| `NEXOS_ISO_OUT` | `./build/iso` | ISO 产物输出根目录（NAS 上建议 `/tank/iso`） |

## 5. PXE 子项

逻辑自原 pxe.rs 搬移（前缀改 `/pxe/*`），本期唯一改动：POST boot-entries 的
「清旧 default + 入表」合并到**单次锁作用域**内完成（原三段锁在并发插入时可能交错）。
start/stop 仍为内存态标记位（真实 TFTP/DHCP 服务接入留待后续）。

## 6. 电源控制层（`power/*`，PowerRouteHandler）

PXE 装机流水线的第一环——裸机冷启动时 PXE/DHCP 还无处安放，必须先把电送上：

```text
┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│ WoL 魔术唤醒  │ →  │  PXE 网络引导 │ →  │ SSH 部署收尾  │
│ / IPMI 上电   │    │  (pxe/*)     │    │  (ssh/*)     │
│ (power/*)     │    │              │    │              │
└──────────────┘    └──────────────┘    └──────────────┘
   第一环：唤醒/上电      第二环：引导装机       第三环：配置部署
```

两个入口：**WoL**（网卡魔术包，目标机 BIOS 开启 Wake-on-LAN 即可，无需 BMC）；
**IPMI**（BMC 带外管理，可上电/断电/重启/读传感器，无需目标机 OS 存活）。
IPMI 又分**对内**（本机 BMC，in-band）与**对外**（远程 IPMI 2.0 设备，RMCP+ over
lanplus）。ipmitool 由构造时 PATH 一次性探测；**缺失或本机无 `/dev/ipmi0` 时
端点返回明确降级说明（200/503 + 指引），绝不 500**；WoL 域不依赖 ipmitool，
始终可用。

### 6.1 端点契约（16 条，前缀 `/api/v1/provisioning/power`）

| method | path | 鉴权 | 动作 |
|--------|------|------|------|
| GET  | `/bmc` | 公开 | 本机 BMC 聚合：chassis status / sel info / mc info 键值 + `system_power` |
| POST | `/bmc/power` | admin | 本机电源控制 `{action: on\|off\|cycle\|soft}`（ipmitool 不可用 → 503 降级说明） |
| GET  | `/bmc/sensors` | 公开 | 传感器表（竖线分隔解析，截 200 行 + truncated 标记） |
| GET  | `/ipmi/devices` | 公开 | 远程设备列表（**密码脱敏**：`password:null` + `has_password`） |
| POST | `/ipmi/devices` | admin | 注册 `{name, host, port=623, username, password, cipher?}` |
| DELETE | `/ipmi/devices/:id` | admin | 删设备 |
| POST | `/ipmi/devices/:id/test` | admin | 连通性测试（真实 `ipmitool -I lanplus ... chassis status`，10s 超时） |
| POST | `/ipmi/devices/:id/power` | admin | 远程电源控制（同 on/off/cycle/soft） |
| GET  | `/ipmi/devices/:id/status` | 公开 | 实时 chassis status（电源开/关、识别灯；不可达附 error） |
| POST | `/ipmi/scan` | admin | 发起网段扫描（后台任务，202 返回任务对象） |
| GET  | `/ipmi/scan/:id` | 公开 | 扫描任务状态 + 命中设备列表（前端轮询） |
| GET  | `/wol/targets` | 公开 | WoL 目标列表（SecureOn 密码脱敏为 `has_secureon`） |
| POST | `/wol/targets` | admin | 注册 `{name, mac, broadcast=255.255.255.255, port=9, secureon_password?}` |
| DELETE | `/wol/targets/:id` | admin | 删目标 |
| POST | `/wol/wake` | 公开（开发期） | 发送魔术包 `{name}` 或 `{mac}`（可临时覆盖 broadcast/port） |
| GET  | `/wol/arp` | 公开 | 局域网邻居（`ip neigh` 解析 MAC↔IP，辅助选 MAC） |

`wol/wake` 开发期公开的理由：魔术包本身无凭据（任何人向广播地址发包即可），
攻击面仅为「误唤醒」；生产收紧策略见 §6.6。

### 6.2 子进程参数（安全基线）

本机（in-band）：`ipmitool chassis status` / `chassis power <action>` /
`sel info` / `mc info` / `sensor list`，10s 超时（kill_on_drop 强杀）。

远程（RMCP+ / lanplus）：

```
ipmitool -I lanplus -H <host> -p <port> -U <user> -P <password> [-C <cipher>] chassis ...
```

全部经 **argv 直传**（tokio Command 不经 shell）——host/用户名/密码即使含
shell 元字符（空格、引号、`;`）也无注入面，测试用含 `rm -rf` 的密码断言
argv 原样保留。输出各截 16KB；传感器表截 200 行。

### 6.3 网段扫描原理（RMCP Presence Ping，免凭据）

**依据**：IPMI 2.0 规范 §13.5——RMCP+（IPMI 2.0）BMC **必须**应答
RMCP Presence Ping（ASF 发现机制，UDP/623，无会话无凭据）。因此向网段
内全部地址发 12 字节探测帧、收集 Pong，即可免凭据发现所有 BMC。

探测帧字节布局（与 ipmitool `lan.c` `ipmi_lan_ping` 一致，openbmc 实机互证）：

```text
06 00 FF 06 00 00 11 BE 80 00 00 00
│ │    │  │           │  │  └─┬─┴─ reserved / data len=0
│ │    │  │           │  └──── tag（应答回显）
│ │    │  │           └─────── ASF 类型 0x80 = Presence Ping
│ │    │  └─────────────────── ASF IANA 企业号 4542（0x011BE，网络序）
│ │    └────────────────────── RMCP 消息类 0x06 = ASF-presence-ping
│ └───────────────────────────── reserved
└─────────────────────────────── RMCP 版本 1.0（0x06）
```

Pong 应答（28 字节 = RMCP 头 4 + ASF 头 8 + 数据 16）解析出：
消息类字节（多数 BMC 回显 0x06，IPMI 2.0 规范定义为 0x07，均接受）、
ASF 版本（数据区 bit0）、**IPMI 实体位**（数据区第 9 字节 bit7——判定 BMC
身份的核心位）、ASF 企业号（4542；不同 BMC 字节序不一，两种序任一命中
即归一）、OEM 自定义区、交互位。`rmcp_plus_supported` = 消息类 0x07 **或**
IPMI 实体位置位；严格 RMCP+ 会话能力（RAKP/cipher suite）在设备 `test`
（lanplus Get Channel Auth Capabilities）时确认。

实现为**纯 Rust**（tokio `UdpSocket`，非 shell 子进程）：并发分批
（默认 64/批）向 CIDR 全地址发帧，每批按 `timeout_ms`（默认 500，50~5000
钳制）deadline 收包，`recv_from` 源地址即命中 IP（同 IP 去重）。**仅允许
/24 ~ /32（≤256 地址）**，/23 及更宽直接 400（防误扫大网段）。长任务
后台化（`tokio::spawn`），内存态保留最近 50 条任务记录。

### 6.4 WoL 协议

魔术包（Magic Packet，AMD 1995 白皮书 / AMD Magic Packet 技术）：

```text
FF × 6 ＋ 目标 MAC × 16 ［＋ SecureOn 密码 6 字节］
= 102 字节（基础）/ 108 字节（含 SecureOn）
```

UDP 广播发送（缺省 255.255.255.255:9，discard 端口——多数网卡固件默认监听
9 或 7）：socket 开 `SO_BROADCAST`，**连发 3 次、间隔 100ms**（部分网卡
固件对首次广播包丢弃率高，多发提高成功率）。目标机 BIOS/网卡需开启
Wake-on-LAN；SecureOn 为网卡侧密码扩展（部分 Realtek 支持），格式与 MAC
同为 6 字节十六进制。MAC 接受 `aa:bb:cc:dd:ee:ff` / `AA-BB-...` /
`aabbccddeeff` 三种写法，存储统一规范化为小写冒号格式。

### 6.5 状态持久化与环境变量

| env | 缺省 | 含义 |
|-----|------|------|
| `NEXOS_POWER_STATE` | `/tank/os-data/power-state.json` | 设备表 + WoL 目标表（原子写：`.tmp` + rename，与 update-state 同款） |

首次无状态文件时预置 1 条示例 WoL 目标（IPMI 设备**不预置**——带密码的
实体不该有演示假数据）。扫描任务为短生命周期观测态，仅内存。

### 6.6 生产硬化清单

- **密码存储**：IPMI 密码 / SecureOn 密码当前明文落 state 文件（开发期取舍，
  与「SSH 仅密钥认证」红线不冲突但属已知债）——生产须迁 vault / OS keyring，
  state 文件权限收紧 0600。
- **网段扫描授权范围**：`POST /ipmi/scan` 已限 admin + /24 上限；生产应再加
  白名单（仅允许配置过的管理网段），避免被用作内网横向探测工具。
- **wol/wake 收紧**：开发期公开；生产建议限 admin + 目标限注册表
  （禁裸 MAC 广播），并在审计中间件记录调用者。
- **扫描速率**：/24 × 500ms 已很温和；更大规模建议改用 nmap `ipmi-version`
  脚本离线执行，本层保持轻量。
- **BMC 凭据轮换**：lanplus 密码长期有效，建议接入 BMC 侧账本周期轮换。

## 7. 测试手法（PATH 注入 / 工具路径注入）

## 7.1 provisioning（PATH 注入）

后端 34 个 provisioning 测试（`cargo test -p os-api --lib provisioning`）。
真实子进程测试不 mock 库层，而是**注入假二进制**：

- 建临时目录写假 `ssh`/`scp`/`mksquashfs`/`xorriso`/`sha256sum` 脚本（0755，
  记录 `$*` 到日志 / 定向 exit N / sleep 拖时），PATH 前置该目录（系统 PATH 追加，
  保证脚本内 sleep/yes/head 可解析）。
- 全局 `ENV_LOCK` 串行化所有改 PATH 的测试；异步测试体在锁内 `block_on` 跑完
  （含轮询到终态），确保子进程 spawn 全部落在假 PATH 窗口（std MutexGuard
  不跨 await，避开 clippy await_holding_lock）。
- 工具链缺失降级测试用**独占 PATH**（空目录）——保证任何机器上 xorriso 都探不到。

覆盖：全流程成功（参数形态断言：BatchMode/-P/accept-new/-i/sh -c 引用包裹/
mkdir 去重）、scp 失败跳过后续、mkdir 失败全败、run_cmd 非零捕获输出、
同目标互斥 409→完成后释放、300ms 超时强杀（3s 假 scp 必须秒败）、8KB 截断、
ISO 工具链缺失指引 / 假工具链真实构建成功（产物落盘+日志）/ building 409 /
非法架构、路由 21 条与鉴权矩阵、stats 聚合。

### 7.2 power（工具绝对路径注入 + 本地 UDP 环回）

后端 24 个 power 测试（`cargo test -p os-api --lib handlers::power`）。
与 provisioning 的「改 PATH」手法不同，power 的 ipmitool/`ip` 是**构造时
一次性解析的绝对路径**（`PowerRouteHandler::with_paths` 可注入）——测试
直接传假二进制路径或 `None`（降级），**全程不动进程 PATH**，天然免 ENV_LOCK、
与其它模块测试零竞争：

- 假 `ipmitool` 脚本按子命令回放真实格式输出（chassis status 键值 / SEL
  info / mc info / sensor list 竖线表），并把 `$@` 追加到 argv.log——
  「chassis power cycle」与「-I lanplus -H -p -U -P ...」的 argv 断言即取自
  该日志（不经 shell 的直接证据）。
- 降级测试：工具传 `None` → `GET /bmc` 200 + `available:false` + 安装指引、
  `POST /bmc/power` 503（均非 500）。
- 扫描测试：**本地 UDP 环回假 BMC**（绑定 127.0.0.1 随机端口，收到 presence
  ping 回 28 字节 openbmc 实机样本 pong）→ `POST /ipmi/scan`
  `{"cidr":"127.0.0.1/32"}` 202 → 轮询 `GET /ipmi/scan/:id` 到 completed，
  断言命中 IP / IPMI 实体位 / RMCP+ / 企业号 4542。
- WoL 测试：本地绑定 UDP 收包口，`POST /wol/wake`（broadcast 覆盖为
  127.0.0.1）后 `recv_from` 逐字节比对魔术包（6×FF + 16×MAC + SecureOn
  6 字节 = 108），并断言 3 次发送计数。
- 纯函数直测：魔术包字节级构造 / RMCP ping 帧常量与 pong 样本解析（openbmc
  实机帧 + 合成 class-07 帧 + 各类垃圾帧拒绝）/ CIDR 展开（/24=256、/32=1、
  /23 与 /16 拒绝）/ `ip neigh` 解析 / PATH 探测可执行位校验。
- 持久化测试：密码明文落 state 文件（开-dev 期契约）+ 响应脱敏（`password:null`）
  + 重开 handler 状态存活；WoL SecureOn 同款脱敏。

## 8. 前端（Provisioning.vue）

- **SSH Tab**：部署对话框提交后切「进度模式」（1.2s 轮询：文件级
  ✓/✗/–/⏳ + 耗时 + run_cmd exit 码 + stdout/stderr 折叠区，终态出「再次部署」）；
  下方部署任务历史表（admin 读），有进行中任务时 2s 自动刷新，行内「详情」
  展开同样内容。
- **ISO Tab**：pending/failed 行「开始构建」；building 行状态列内联进度条 +
  当前步骤（列表 2s 自动轮询）；「日志」按钮展开构建日志面板（实时/快照）；
  building 中禁删。
- **电源控制 Tab**（第 4 个）：本机 BMC 状态卡（电源/固件/SEL 摘要 +
  on/off/cycle/soft 按钮组，不可用时降级提示文案）+ 传感器折叠表；远程
  IPMI 设备表（测试/状态/上电/重启/删除 + 添加对话框）；网段扫描（CIDR
  表单 → 进度条轮询 → 命中列表带 RMCP+/IPMI 徽章，「加入设备」带出 host
  预填）；WoL 目标表 + 添加对话框（MAC 正则校验 + ARP 邻居下拉自动填
  MAC + SecureOn 可选）+ 唤醒按钮（结果提示含发包数与字节数）。
- 所有轮询定时器在 `onUnmounted` 统一清理（含电源 Tab 的扫描轮询）。
- client.ts：provisioning 域新增 `buildProvisioningIsoTask(id)` /
  `provisioningDeployTasks()`；power 域新增 16 个 `power*` 端点封装
  （powerBmc / powerBmcPower / powerIpmiDevices / startPowerIpmiScan /
  wakePowerWol / powerWolArp 等，注释标注鉴权与降级语义）。
