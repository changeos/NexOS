# 存储与共享（SMB 链路）——运维手册

> 范围：真实运行中的 SMB 共享（nexos-downloads）、mDNS/NetBIOS 品牌广播、下载落地
> 目录、Files/Storage 两页面的文件浏览能力，以及后续扩共享的操作模板。
> 涉及源码：`crates/os-api/src/handlers/files.rs`、`storage.rs`、`downloads.rs`、
> `share.rs`；前端 `crates/os-api/web/src/components/FileBrowser.vue`、
> `views/Files.vue`、`views/Storage.vue`。

## 1. SMB 共享 nexos-downloads（配置位置）

实机配置在 **`/etc/samba/smb.conf`**（手工维护，非 share.rs 生成）：

```ini
[global]
   netbios name = NEXOS          # NetBIOS 名统一品牌（nmbd 已禁用时主要供 SMB1 客户端显示）
   workgroup = WORKGROUP
   server string = NexOS Storage # 服务器描述统一品牌
   ...

[nexos-downloads]
   comment = NexOS download landing zone
   path = /tank/downloads
   browseable = yes
   read only = no
   valid users = oem
   create mask = 0664
   directory mask = 2775
```

- **账号**：`oem`（Samba passdb，经 `smbpasswd -a oem` 设置；当前口令见
  MEMORY.md #17，不在本文重复）。共享仅 `valid users = oem` 可写。
- **落地目录**：`/tank/downloads`（ZFS 池 tank 下；属主 `oem:oem`，setgid `s`
  位 + `directory mask = 2775` 保证新目录组共享）。
- 下载中心 aria2 同样落这里：`downloads.rs` spawn
  `aria2c --enable-rpc --rpc-listen-all --rpc-listen-port=6800 -d /tank/downloads`，
  与 SMB 共享同一目录（迅雷下载 → SMB 取走，一条链路）。

## 2. mDNS 品牌统一（avahi）

`/etc/avahi/services/` 两个服务文件，广播名统一 "NexOS"：

| 文件 | 广播 | 内容 |
|------|------|------|
| `smb.service` | `NexOS`（_smb._tcp） | `_smb._tcp` 端口 445 + `_device-info._tcp` `model=NexOS`（替代 Samba 自带的 model=MacSamba 广播，避免 NAS 客户端把设备归为"群晖"） |
| `nexos.service` | `NexOS on %h`（_nexos._tcp） | 自定义 `_nexos._tcp` 服务，端口 **8080**（os-api Web UI，即 NEXOS_HTTP_PORT 默认值） |

品牌三件套：avahi 服务名 NexOS、`smb.conf` 的 `server string = NexOS Storage`、
`netbios name = NEXOS`。

## 3. 迅雷等客户端接入坐标

| 客户端 | 坐标 | 认证 |
|--------|------|------|
| Windows / 迅雷NAS版 / 资源管理器 | `\\192.0.2.106\nexos-downloads` 或 `\\NEXOS\nexos-downloads` | 用户 `oem` + smbpasswd 口令 |
| macOS Finder（前往 → 连接服务器） | `smb://192.0.2.106/nexos-downloads` | 同上 |
| Linux / 任何 SMB 客户端 | `//192.0.2.106/nexos-downloads`（mDNS 发现名 `NexOS._smb._tcp.local`） | 同上 |

`192.0.2.106` 为本机当前局域网地址；主机名/mDNS 名不稳时优先用 IP。

## 4. Files 与 Storage 页面能力

### FileBrowser 组件（`web/src/components/FileBrowser.vue`，Files/Storage 两页复用）

- 面包屑导航 + 整行点击进目录；列排序；**5s 静默轮询**刷新（不闪 loading）；
- 目录用量**懒加载**：hover 目录名 400ms（`USAGE_HOVER_DELAY_MS`）才请求
  `GET /api/v1/files/usage?path=`，结果缓存（`Map<path, DirUsage>`），换目录后缓存失效；
- 写操作：新建文件夹 / 删除（两步确认）/ 重命名；
- 上传/下载为 TODO（见组件头注释），本期范围外。
- `Files.vue` 只做页面壳（root="/"，根映射 /tank）；`Storage.vue` 的"文件浏览"
  tab 复用同组件（固定 `root="/tank"`，只读模式）。

### files 端点契约（`crates/os-api/src/handlers/files.rs`，读公开 / 写 admin）

| method | path | 参数 | 说明 |
|--------|------|------|------|
| GET | `/api/v1/files/list` | `?path=<dir>`（可空=根） | 列目录 → `FileEntry[]`（name/path/is_dir/size_bytes/modified_at/mime_type）。path 为空或 `/` 时映射根 `/tank`（不存在回退 `/var/lib/os/files`） |
| GET | `/api/v1/files/stat` | `?path=<file>` | 单文件 stat |
| GET | `/api/v1/files/usage` | `?path=<dir>` | 目录递归用量 → `DirUsage{path,total_bytes,file_count,dir_count,partial}`。上限：条目 50,000 / 深度 32 / 软超时 3s，超限即停置 `partial:true`（数值为下界） |
| POST | `/api/v1/files/mkdir` | body `{path}` | 创建目录（admin） |
| POST | `/api/v1/files/delete` | body `{path}` | 删除（admin） |
| POST | `/api/v1/files/rename` | body `{from,to}` | 重命名（admin） |

安全红线：path 含 `..` 直接 400（防路径穿越）。

### Storage 页（`views/Storage.vue`，池/数据集/快照/文件浏览四 tab）

- 创建池：飞牛 fnOS 风格三步向导（选盘 → RAID 模式/高级选项 → 确认），
  `POST /api/v1/pools` body 组装 `VdevSpec[]`；
- 端点（`storage.rs`，读公开 / 写 admin）：`GET /api/v1/pools`、`POST /api/v1/pools`、
  `GET /api/v1/disks`（lsblk 探测，已过滤系统盘/loop）、`GET /api/v1/datasets`、
  `POST /api/v1/datasets`、`GET /api/v1/snapshots`、
  `POST /api/v1/pools/:id/scrub`、`GET /api/v1/pools/:id/scrub-status`、
  `POST /api/v1/datasets/:id/quota`（scrub 经 `sudo zpool scrub`，失败降级
  `{ok:false,warning}` 不 panic）。

## 5. 后续扩共享操作步骤模板

1. 建落地目录：`sudo mkdir -p /tank/<name> && sudo chown oem:oem /tank/<name> &&
   sudo chmod 2775 /tank/<name>`；
2. `/etc/samba/smb.conf` 追加 section（模板）：

   ```ini
   [<name>]
      comment = <用途说明>
      path = /tank/<name>
      browseable = yes
      read only = no
      valid users = oem
      create mask = 0664
      directory mask = 2775
   ```

3. `sudo testparm` 校验语法 → `sudo smbcontrol all reload-config`（失败再
   `sudo systemctl restart smbd`）；
4. （可选）需要 mDNS 可见性时无需新 avahi 文件——`smb.service` 广播的是整机 SMB
   端点，新共享自动出现在 `\\NEXOS\<name>` 下；
5. 客户端坐标换成 `\\192.0.2.106\<name>`。

> 注意：`share.rs` 也提供 `/shares` POST/DELETE 的程序化建共享能力（写 smb.conf +
> reload），但受 `NEXOS_APPLY_SYSTEM` / `OS_APPLY_SYSTEM` env 门禁（未设置时仅落
> shares.json 不动系统）；当前实机共享为手工配置，与该 handler 的 JSON 状态相互独立。

## 6. 无 ZFS 节点的优雅降级（2026-09-02）

> 背景：Spark 等 install.sh 装的最小节点没有 zfsutils，存储管理页曾红幅报
> `加载失败：500 — [storage/500] zpool "list -p -H" 退出码 1：sudo: zpool:
> command not found`。定调（用户原话）：并不是所有的终端都要存储池，自动检查
> 就行，没有多的盘/没有 ZFS 工具就不显示，不用报错。

### 探测与缓存（`storage.rs`）

- **探测方式**：PATH 查找 `zpool` **与** `zfs` 两个可执行文件（which 语义：
  普通文件 + 任一执行位）——不 spawn 进程、不走 sudo；
- **进程内缓存**：`once_cell::Lazy` 只算一次，首次触达存储端点时求值；
- **env 强制开关**：`NEXOS_STORAGE_ZFS_PROBE=0`（`false`/`no` 同义）强制视为
  不可用——测试/特殊环境用；须在进程首次触达存储端点前设置（缓存一次性）；
- 探测结果以 `eprintln!("[storage] …")` 落日志。

### 降级契约

| 端点 | ZFS 不可用时 | ZFS 可用时 |
|------|--------------|------------|
| `GET /api/v1/pools` | 200 `{pools: [], zfs_available: false}` | 200 裸数组（形状零变更） |
| `GET /api/v1/datasets` | 200 `{datasets: [], zfs_available: false}` | 200 裸数组 |
| `GET /api/v1/snapshots` | 200 `{snapshots: [], zfs_available: false}` | 200 裸数组 |
| `GET /api/v1/disks/importable` | 200 `{importable: [], zfs_available: false}` | 200 `{importable: [...], zfs_available: true}` |
| `POST /api/v1/pools`（创建池） | 400「本节点未安装 ZFS 工具…」 | 原行为 |
| `POST /api/v1/datasets`（创建数据集） | 400 同上 | 原行为 |
| `POST /api/v1/disks/import`（导入池） | 400 同上（池名白名单校验仍先行） | 原行为 |
| `DELETE /api/v1/pools/:name`（删池） | 400 同上 | 原行为 |
| `POST /pools/:id/scrub`、`GET /pools/:id/scrub-status`、`POST /datasets/:id/quota` | 200 沿用各自降级契约（`{ok:false,warning}` / `{status:"none",warning}`） | 原行为 |
| `GET /api/v1/disks`（磁盘列表） | **不受影响**——lsblk 探测，ZFS 探测分支本就失败降级为空 | 原行为 |

### 错误分级（诚实原则：真实故障不掩盖）

只有「**二进制缺失**」走降级，判定（`is_zfs_binary_missing`）：

- `StorageError::Io` 且 `ErrorKind::NotFound`（spawn ENOENT，sudo 本体缺失）；
- `StorageError::CommandFailed` 且文案含 `command not found`（`sudo: zpool:
  command not found`——Spark 实测路径）或退出码 127（`退出码 127` /
  `exit status 127`）。

其余失败（sudo 未免密、池损坏、内部错误等）**照旧 500**。除探测前置短路外，
读端点还有**反应式降级**：探测说可用但后端报「二进制缺失」（如 sudo 的
secure_path 与进程 PATH 不一致）时同样返回空态 + 标志。

### 前端（`views/Storage.vue`）

- 收到 `zfs_available: false`（`api/client.ts` 的 `isZfsUnavailable` 守卫）：
  不显示红幅错误，改为低调蓝色信息条（i18n `storageZfs.unavailableBanner`）：
  「本节点未安装 ZFS 存储工具——存储池/数据集/快照不可用（文件浏览不受影响）」；
- 「创建池 / 创建数据集 / 创建快照」按钮禁用 + tooltip（`storageZfs.createDisabledTip`）；
- 三个 Tab 的空表文案区分「不可用」与「可用但为空」（`storageZfs.poolsEmpty` 等）；
- i18n 四语言（zh-CN / zh-TW 繁化：儲存池/資料集/檔案瀏覽 / en-US / ja-JP），
  追加在 locale 文件末尾 `storageZfs` 节。

