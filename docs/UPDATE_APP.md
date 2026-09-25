# 更新应用（Update）

> 源码：`crates/os-api/src/handlers/update.rs`（`UpdateRouteHandler`，组件名 `update`）·
> 前端：`crates/os-api/web/src/views/Update.vue`（路由 `/update`，appRegistry id=`update`）·
> 依赖：`os-update` crate 纯逻辑层（`version.rs` semver / `slot.rs` A/B 槽位状态机）·
> 登记：2026-08-24 · 2026-08-25 apply 做实为真实安装管线（工件登记 + staged 拷贝
> + 备份 + rename + systemctl 自重启，见 §1/§4/§5）· 2026-09-02 更新源升级为
> 两级解析链：本地裸仓库优先，缺失时走 `NEXOS_UPDATE_REPO_URL` 远端
> `git ls-remote`（install.sh 新节点开箱即有更新源，见 §3/§7）· 2026-09-03
> 双缺口修复：prepare-distributable 自动登记同版本工件（§1a，页内应用
> 更新=prepare 后即可用）+ 联邦 auto-pull fetch 显式镜像 tag（§1 更新源）

## 1. 功能说明

桌面「更新」应用的后端 REST 入口：**当前版本与 A/B 槽位视图 + 更新通道管理 +
更新源检查 + 工件登记 + 更新任务真实安装管线**。核心定位是**远程升级 os-api
自身的闭环通道**：版本发现（NexHub tag）→ 工件运输（Files API 上传 + 工件
登记）→ 安装与自重启（apply 状态机）。

### 设计动机（2026-08-25 aliyun sshd 事件）

当天 aliyun 机器 sshd 挂掉、SSH 不可达，但 os-api 网关仍活着——彼时「更新」
应用的 apply 还是预留桩（推进到 writing 即标记「通道已预留，等待真实镜像源
接入」，不真装），明明有更新应用却换不了二进制，最后靠 Files API 传新构建
+ cron 定时替换文件才救回来。本次做实后，同样的场景只需：Files API 上传新
os-api → `POST /artifact` 登记 → `POST /apply` → 服务自动换二进制并自重启，
纯更新应用即可完成远程升级，不再依赖 cron 旁路。

- **当前版本**：env `NEXOS_VERSION` 优先，缺省取 os-api 包版本
  （`CARGO_PKG_VERSION` = 工作区 `0.1.0`，与 NexHub 发版 tag `v0.1.0` 对应）。
- **A/B 槽位视图**：复用 `os_update::slot::SlotManager` 纯状态机，初始化
  A=active（当前版本）、B=inactive（更新写入目标）。**内存态**，重启重建。
- **更新源（本生态闭环，两级解析链）**：
  1. **本地裸仓库** `/tank/git-repos/nexos.git`（env `NEXOS_UPDATE_REPO`
     覆盖）——发版即 git tag（os-nexhub release 功能打的 tag，形如 `v0.1.0`）。
     `POST /check` 用 `git for-each-ref refs/tags` 子进程一次取回 tag 名 +
     打 tag时间（creatordate）。有本地副本的节点（联邦 auto-pull 跟随，
     `NEXOS_LOBBY_AUTO_PULL`）走本级（本地优先，现状不变）。**2026-09-03
     起 auto-pull fetch 显式镜像 tag**（refspec 追加 `+refs/tags/*:
     refs/tags/*`——此前 heads-only refspec 下 tag 只能靠 git 机会主义
     auto-follow，不保证覆盖旧对象上的 tag、绝不更新被强推的 tag，实测
     下游副本 refs/tags 全空 → 更新检查失明；显式强制 refspec 保证 tag
     必达且与源对齐，HEAD 判等跳过逻辑不受影响）。
  2. **远端 git URL**（env `NEXOS_UPDATE_REPO_URL`，http(s)，如
     `http://203.0.113.2:8558/git/nexos.git`）——本地路径缺失/无 tag 时
     用 `git ls-remote --tags --refs <url>` 纯网络查询（无需本地克隆，
     15s 超时封顶）。ls-remote 协议不携带 creatordate，`created_at` 为
     null；tag 解析/通道过滤（stable/beta/nightly）与本地 tag 同一口径。
     install.sh 装的新节点无 `/tank`，开箱即走本级（见 §7 新节点行为）。
  semver 比较复用 `os_update::version`（`Version::parse` / `Ord`，
  预发布 < 正式版）。
- **更新通道**：通道即 tag 过滤策略，四选一，切换即 JSON 持久化（见 §4）。
- **工件登记**：os-api 新二进制经 Files API 上传到本机后，`POST /artifact`
  登记（校验存在/可读/≥1MB/ELF 魔数，算好 sha256 落 update-state.json；
  重复 version 覆盖）。apply 只安装**已登记**工件。
- **更新任务**：`POST /apply` 前置校验（版本合法且新于当前 + 已登记工件）
  后建任务，状态机 `pending → verifying → writing → reboot_pending → done`
  （`failed` 为失败终态）；`GET /tasks/:id` 每次轮询推进一步：verifying 做
  sha256 重算比对 + ELF 魔数复核，writing 做**真实安装**（staged 拷贝 + 备份
  + rename + 自重启调度）。任务列表内存 + JSON 持久化，`GET /history` 过滤
  done / reboot_pending。
- **降级策略**：本地与远端**均**不可达（仓库不存在 / git 不可用 / URL 不可达 /
  无 tag）→ check 返回空清单 + `repo_reachable: false`，**不报错**。`CheckResult`
  另带三态字段：`repo_mode`（`local` 本地裸仓库 / `remote` 远端 git URL /
  `none` 均不可达）与 `repo_url`（配置的远端 URL 回显，未配置 null）——前端
  「更新源不可达」文案据此区分三态（本地仓库模式 / 远端 git 模式显示 URL /
  均不可达）。

### 1a. 第二条登记路径：prepare-distributable 自动登记（2026-09-03）

发版流程的实测缺口：发版后三节点各跑一次 `POST /provisioning/
prepare-distributable` 只喂饱了 dist 下载通道——页内 apply 要求先
`POST /update/artifact {version, path}` 登记工件，没人登记，「更新」页点
「应用更新」即报「版本 X 尚未登记更新工件」（2026-09-03 真机踩坑）。

修复：**prepare-distributable 成功后自动登记同版本更新工件**——

- 调用链：provisioning handler 持装配层注入的共享 update 实例
  （`main.rs` 以 `Arc` 双持，`SharedUpdateHandler` 纯转发注册——两条登记
  路径写**同一** Mutex 态 + 同一 update-state.json），prepare 暂存成功后调
  `UpdateRouteHandler::register_artifact_and_persist`（与 `POST /artifact`
  同一入口：semver / 绝对路径 / 存在 / ≥1MB / ELF 魔数全套校验 + sha256
  计算 + 入库 + 持久化）；
- 登记内容：`version` = 运行二进制的 `CARGO_PKG_VERSION`（暂存的正是
  current_exe，版本号与字节同源；若 unit 显式设置了 `NEXOS_VERSION`，
  登记口径仍以二进制内嵌版本为准），`path` = 分发产物
  `/tank/os-data/latest-os-api.bin`；
- 幂等：重复 version 覆盖——重跑 prepare 即重登记（同 sha），不增条目；
  登记结果随 prepare 响应 `update_artifact` 字段回传（失败不拦 prepare，
  原因在 `update_artifact_error`，分发主通道不受影响）；
- 运维口径：**发版流程「三节点各跑一次 prepare」即同时喂饱两条更新通道，
  页内应用更新 = prepare 后即可用**（不再需要手动 curl 登记）。注意登记的
  是本机正在运行二进制的版本——节点升级并重启后再跑 prepare，登记的才是
  新版工件；无本地构建的节点（Spark）页内升级仍走「dist 下载 → Files API
  上传 → 手动登记」或重跑 install.sh（见 docs/BOOTSTRAP_INSTALL.md
  「发版流程要点」）。

### 前端分区与数据源对应（Update.vue）

| 分区 | 数据源端点 |
|------|-----------|
| 当前版本卡（版本号大字 + 通道徽章 + 槽 A/B 小图示） | `GET /status`（`current_version` / `channel` / `slot_a` / `slot_b`） |
| 更新通道四选一卡片（切换即 POST） | `GET /channels` + `POST /channel` |
| 检查更新按钮 → 可用版本列表（版本/tag/通道/时间）→ 每行「应用更新」 | `POST /check` → `POST /apply` |
| 任务进度条（轮询推进） | `GET /tasks/:id`（1.5s 轮询至 done/failed） |
| 更新历史列表 | `GET /history` |

注：工件登记（`POST /artifact`）与列表（`GET /artifacts`）当前为 API 驱动
（远程运维经 Files API 上传后 curl 登记），前端工件管理界面为后续接入点。

## 2. 端点契约（前缀 /api/v1/update，共 10 条）

| method | path | 鉴权 | 请求体 | 响应 | 语义 |
|--------|------|------|--------|------|------|
| GET | `/status` | 公开 | — | `UpdateStatusView` | 当前版本 / 通道 / 槽 A/B 视图（SlotState 序列化）/ active 与 writable 槽 / 上次检查时间 / 待应用清单 / 状态文件路径 |
| GET | `/channels` | 公开 | — | `{current, channels[4]}` | 通道目录（id/名称/一句话说明）+ 当前选中 |
| POST | `/channel` | admin | `{channel}` | `{channel}` | 切换通道并持久化；非法值 400 |
| POST | `/check` | admin | `{}`（可空） | `CheckResult` | 按通道走两级源解析链读 tag → semver 严格新于当前 → 通道过滤 → 可用清单（版本降序）；记录 last_check；源均不可达时空清单 + `repo_reachable:false` + `repo_mode:"none"` |
| POST | `/artifact` | admin | `{version, path}` | `201 UpdateArtifact` | 登记更新工件（Files API 上传产物）；version 须 semver、path 须本机绝对路径且存在/可读/≥1MB/ELF 魔数（`\x7fELF`），登记时算好 sha256；任一不过 400；重复 version 覆盖 |
| GET | `/artifacts` | 公开 | — | `UpdateArtifact[]` | 已登记工件列表（版本/大小/sha256/登记时间） |
| POST | `/apply` | admin | `{version}` | `201 UpdateTask` | 建更新任务（pending，快照工件 path+sha256）；版本非法/降级 400；无已登记工件 400 指引先走 Files API + `POST /artifact` |
| GET | `/tasks` | 公开 | — | `UpdateTask[]` | 全部任务（新在前） |
| GET | `/tasks/:id` | 公开 | — | `UpdateTask` | 任务详情；**每次轮询推进一步状态机**（verifying 复核 / writing 真实安装）；不存在 404 |
| GET | `/history` | 公开 | — | `UpdateTask[]` | 已应用历史（status ∈ {done, reboot_pending}，新在前） |

关键 DTO 字段：

- `AvailableUpdate`：`tag`（如 `v0.2.0`）/ `version`（`0.2.0`）/ `channel`
  （`stable` | `beta` | `prerelease`——归属桶，展示用）/ `created_at`（git
  creatordate，解析失败 null）。
- `UpdateArtifact`：`version`（semver 归一化）/ `path`（本机绝对路径）/
  `size`（字节）/ `sha256`（登记时快照）/ `registered_at`（ISO）。
- `UpdateTask`：`id`（`update-N`）/ `version` / `tag`（从上次 check 结果反查，
  未知 null）/ `channel` / `status`（pending / verifying / writing /
  reboot_pending / done / failed）/ `slot_target`（`b`）/ `progress`
  （0-100 阶段启发值：40/70/90/100）/ `created_at` / `updated_at` /
  `error`（failed 原因）/ `note`（writing→reboot_pending 起写入
  「已写入，服务将在数秒内自重启」）/ `artifact_path` + `artifact_sha256`
  （建任务时从登记快照，避免任务进行中工件被重新登记导致基准漂移）。
- `UpdateStatusView.slot_a` / `slot_b`：os-update `SlotState` 原样序列化
  （`slot`/`status`/`version`/`last_activated_at`/`last_written_at`）。

## 3. env 清单

| env | 缺省 | 说明 |
|-----|------|------|
| `NEXOS_VERSION` | os-api `CARGO_PKG_VERSION`（= `0.1.0`） | 当前系统版本（`GET /status` 与 check/apply 的比较基准；非空才生效） |
| `NEXOS_UPDATE_STATE` | `/tank/os-data/update-state.json` | 状态持久化 JSON（通道 + 上次检查 + 待应用清单 + 任务列表）；父目录不存在自动创建；**原子写**（`.tmp` + rename） |
| `NEXOS_UPDATE_REPO` | `/tank/git-repos/nexos.git` | 更新源 NexHub 裸仓库路径（源解析链第一级，check 读其 tag 列表；本地优先） |
| `NEXOS_UPDATE_REPO_URL` | （空 = 不启用） | 远端更新源 git URL（源解析链第二级）：http(s) 如 `http://<源节点>:8558/git/nexos.git`。本地裸仓库缺失/无 tag 时 check 走 `git ls-remote --tags --refs` 纯网络查询（15s 超时）；非空才生效。**契约**：URL 须指向源节点 Git Smart HTTP 通道（`/git/<repo>`，os-api 内建），仓库须为 NexHub 发版仓（tag 即版本）；apply 路径不消费该 env（工件仍走 Files API + `POST /artifact` 双轨） |

## 4. 更新通道架构

### 通道语义（"留好的更新通道"）

| 通道 | tag 过滤规则 | 定位 |
|------|--------------|------|
| `stable` | 仅正式 tag（**排除一切预发布** `x.y.z-*`） | 生产节点 |
| `beta` | 仅 `*-beta*` tag（预发布标识含 `beta`） | 提前体验 |
| `nightly` | 任意最新 tag（**全收**，含 rc 等其它预发布） | 跟进最快 |
| `manual` | 过滤同 nightly 全收 | 不自动检查，仅手动触发（当前 `POST /check` 本身即手动；未来接自动轮询时跳过本通道） |

semver 口径：tag 剥前导 `v` 后须为 `MAJOR.MINOR.PATCH[-PRE]`（PRE 仅单段
alnum，os-update `Version::parse` 子集）；预发布 < 同号正式版。非 semver tag
（如 `windows-m1`）直接忽略。仅 **严格新于** 当前版本的 tag 入清单，按版本降序。

### 拓扑与数据流（真实安装链路）

```
NexHub 发版（os-nexhub release：git tag，如 v0.2.0 / v0.3.0-beta）
        │
        ▼
源节点裸仓库 /tank/git-repos/nexos.git（tag 即版本源）
   ┌────┴─────────────────────────────────────────────┐
   │ 本地副本存在（联邦 auto-pull 跟随）                 │ 本地缺失（install.sh 新节点）
   ▼                                                   ▼
git for-each-ref refs/tags                     git ls-remote --tags --refs <URL>
（tag 名 + creatordate）                       （NEXOS_UPDATE_REPO_URL，纯网络查询）
   └────┬─────────────────────────────────────────────┘
        ▼
POST /api/v1/update/check ── 通道过滤（stable 排除预发布 / beta 只收 *-beta* /
        │                    nightly·manual 全收）
        ▼
   semver 比较（os_update::Version，仅严格新于当前；非 semver tag 忽略）
        │
        ▼
   可用清单（版本降序）──▶ /status 待应用清单 + 前端「可用更新」列表
        │
        │  新 os-api 二进制经 Files API 上传到本机（工件运输）
        ▼
POST /api/v1/update/artifact {version, path}（admin）
        │  校验：semver + 绝对路径 + 存在 + 可读 + ≥1MB + ELF 魔数（\x7fELF）
        │  算 sha256 → update-state.json artifacts 列表（重复 version 覆盖）
        ▼
        │  用户点「应用更新」
POST /api/v1/update/apply {version}（admin）
        │  前置：版本合法且新于当前（禁降级）；无已登记工件 400 指引先登记
        │  建任务（快照工件 path + sha256）
        ▼
   更新任务状态机（GET /tasks/:id 每次轮询推进一步）
   pending → verifying ── sha256 重算比对 + ELF 魔数复核（不过 → failed）
        │
        ▼
     writing ── 真实安装：
        │      1. current_exe() 推导 exec_dir（失败 → failed，绝不盲装）
        │      2. 工件 fs::copy → <exec_dir>/os-api.staged + chmod 755
        │      3. 备份当前二进制 os-api.bak-<ts>（保留最近 3 个）
        │      4. rename staged → 当前二进制路径（Linux 对运行中二进制
        │         rename-over 合法，ETXTBSY 只挡 open-for-write）
        ▼
   reboot_pending（note：已写入，服务将在数秒内自重启）
        │      spawn 分离进程 sh -c "sleep 1; systemctl restart os-api ||
        │      systemctl restart nexos-os-api"（stdout/stderr null）
        ▼
   done（/history 收录 done / reboot_pending）

   A/B 槽位视图（os_update::SlotManager，内存态：A=active 当前版本、B=写入目标）

【仍预留（整 OS 镜像级，区别于本期的 os-api 自升级）】
   整镜像下载   → os_update::real::download_to_file（reqwest）
   ed25519 验签 → os_update::real::verify_package（os-update 已有实现/校验位）
   A/B 写槽     → SlotManager::begin_write / finish_write（真实块设备/ostree 层）
   bootloader   → AbUpdateEngine::activate_slot（GRUB grub2-reboot /
                  systemd-boot bootctl set-oneshot，next-boot 一次性切换）
   看门狗回滚   → AbRollbackManager + SlotManager::on_boot_failed（探活失败回切）
```

## 5. 当前实现边界（真实 vs 预留）

**已真实**：

- **更新源读取（两级解析链）**：本地裸仓库 `git for-each-ref`（本机
  `/tank/git-repos/nexos.git` 存在且有 tag 即真实生效，本地优先）；本地
  缺失/无 tag 且配置了 `NEXOS_UPDATE_REPO_URL` 时 `git ls-remote --tags
  --refs` 网络查询（真实生效，15s 超时封顶）。
- semver 解析/比较/通道过滤：`os_update::version` 纯逻辑，`v` 前缀剥离、
  预发布排序、非 semver tag 忽略均真实。
- 通道切换：真实持久化（JSON 原子写，重启读回）。
- **工件登记**：真实校验（存在/可读/≥1MB/ELF 魔数）+ sha256 计算 +
  持久化（update-state.json `artifacts` 列表，重复 version 覆盖）。
- **verifying 复核**：sha256 重算比对（登记值 vs 实测值）+ ELF 魔数复核，
  不过 → failed 带原因（防登记后文件被替换/截断）。
- **writing 安装**：staged 拷贝（`fs::copy` + chmod 755）→ 备份
  （`os-api.bak-<ts>`，保留最近 3 个）→ `rename` 原子切换当前二进制 →
  spawn 分离进程 `sh -c "sleep 1; systemctl restart os-api || systemctl
  restart nexos-os-api"` 自重启。防呆：exec 路径解析失败 → 任务 failed
  带原因，绝不盲装；自重启从外部 systemd 触发，对任意 Restart 策略成立。
- 任务列表与历史：真实持久化（同一 JSON；重启后历史可见，非终态任务停在原
  状态）。
- 鉴权与路由：读公开 / 写 admin（网关 Bearer 契约）。

**预留（整 OS 镜像级更新，区别于已做实的 os-api 自升级）**：

- **整镜像下载 + ed25519 验签**：本期工件经 Files API 运输 + sha256 校验；
  网络下载与 ed25519 验签的接入点：os-update `AbUpdateEngine::download` /
  `real::verify_package`。
- **A/B 写槽**：本期安装目标是 os-api 单二进制（exec_dir 内 staged/backup/
  rename），非整 OS 镜像写槽。接入点：`SlotManager::begin_write` /
  `finish_write`（缺真实块设备/ostree 写入层）。
- **bootloader 激活**：本期「重启」= systemctl restart os-api（服务级）；
  整 OS 级激活接入点：`AbUpdateEngine::activate_slot`（GRUB/systemd-boot
  编排 os-update 已接通，需 root 运行环境）。
- **看门狗回滚**：未接入。接入点：`AbRollbackManager` + `SlotManager::
  on_boot_failed`（探活失败回切旧槽）。os-api 级的人工回滚兜底：exec_dir
  内 `os-api.bak-<ts>` 手动复制回原名再 restart。
- **自动检查轮询**：无后台任务；`manual` 通道语义已为其留位。

自重启的边界说明：systemd 单元名匹配 `os-api`（开发机）→ fallback
`nexos-os-api`；两个都不匹配的部署上 restart 失败仅日志可见（工件已换新，
人工 `systemctl restart` 即生效）。

## 6. 测试与验证

- 单测 25 个（`cargo test -p os-api --lib handlers::update`）：通道切换持久化
  往返（重启读回）、非法通道 400（含大写归一化）、stable 排除 `-beta`/`rc`、
  beta 只收 `*-beta*`、nightly/manual 全收且版本降序、仓库缺失降级空清单、
  **ls-remote 输出解析纯函数**（peeled `^{}` 过滤 / 异常行防御 / 通道语义
  复用 nightly 口径）、**远端分支端到端**（本地缺失 → ls-remote 真子进程 →
  repo_mode=remote + repo 回显 URL + created_at 为 null）、**本地优先**
  （本地与远端都有 tag 时读本地、保留 creatordate）、**均不可达三态**
  （repo_mode=none + repo_url 回显 / 未配置时 null）、
  **apply 端到端真实安装**（/bin/true 复制+填充当工件：登记→apply→轮询
  verifying/writing/reboot_pending/done→断言 rename 生效/staged 清理/备份
  生成与保留 3 个清理/tag 反查/history 收录）、apply 非法/降级版本 400、
  **工件登记形状+持久化+重复覆盖**、**登记拒绝非 ELF（1MB 文本）**、
  **登记拒绝不存在路径/相对路径/非 semver**、**登记拒绝 <1MB 小文件**、
  **apply 无工件 400 指引（artifact 端点 + Files API）**、**apply sha256
  不匹配 failed 且绝不安装**（二进制未动/无 staged/无新备份/history 不
  收录）、**任务持久化重启读回（非终态停在原状态）**、status 形状（槽位
  视图）、tag 解析与 semver 比较纯函数、路由声明与鉴权惯例（10 条）、
  通道目录形状、状态 JSON 原子写往返。
- **测试红线**：端到端安装测试经注入的临时 exec 目录执行（绝不触碰真实
  os-api / cargo test 进程二进制）；自重启在测试构造（with_config）下恒关闭
  （self_restart=false），绝不 spawn systemctl 重启开发机服务。
- git fixture：测试内临时 init work 仓（空提交 + 打 tag）→ `clone --bare`
  生成裸仓库 fixture，全流程真实走 git 子进程。远端分支复用同一 fixture：
  `git ls-remote` 对本地裸仓库路径与 http(s) URL 同协议处理，无需起 TCP
  mock 即可走真实 ls-remote 子进程。
- install.sh 侧（`handlers::provisioning` 测试）：更新源 env 行形状断言
  （`Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git` + 仓库副本同源
  防漂移）+ step-4 沙箱行为门（真跑片段：unit 文件 env 行写入 / 同源重跑
  幂等唯一 / 换源重跑随 `$SRC` 更新且旧 URL 不残留）。
- **prepare 自动登记（2026-09-03，`handlers::provisioning` 测试
  `prepare_distributable_registers_update_artifact`）**：共享 update 实例
  注入后 prepare → 响应 `update_artifact`（version=CARGO_PKG_VERSION、
  path=分发产物、sha256 与暂存同源）+ 共享实例 `GET /update/artifacts`
  立即可见 + **apply 直接 201 建任务**（不再报"尚未登记"，只建任务不轮询
  ——红线：不触碰 exec 路径）+ 重复 prepare 幂等（同 version 覆盖不增条目）
  + 登记随 update-state.json 重启读回；未注入 registry 的历史构造断言见
  `prepare_distributable_idempotent_and_overwrites`（`update_artifact`
  为 null，行为兼容）。
- **auto-pull fetch 带 tag（2026-09-03，`os-nexhub` 测试
  `auto_pull_inner_fetches_tags_to_copy`）**：既有对象上补打的 tag 与新
  提交上的新 tag 随 fetch 到副本且指向与源一致 + 源侧 `-f` 强挪 tag 后
  下轮 fetch 强制对齐（旧 heads-only refspec 下该断言必失败——auto-follow
  从不更新既有 tag，已做回归验证）。
- 前端：`npm run build`（vue-tsc + vite）通过，Update 视图按需分包。

## 7. 新节点开箱行为与存量节点手工配置

### 新节点（install.sh 引导）

`install.sh`（源节点 `GET /api/v1/provisioning/install.sh` 动态生成 /
仓库副本 `scripts/install-nexos.sh`，两者同源）在 step-4 写 systemd unit 时
注入：

```
Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git
```

`$SRC` = 安装源节点（`--source` 或脚本来源 Host）。新节点**开箱即有可用
更新源**：本机无 `/tank` → check 自动落到远端分支 `git ls-remote` 读安装源
节点的 NexHub git HTTP 通道（`/git/nexos.git`，os-api 内建 Smart HTTP）。
unit 整文件重写天然幂等：重复执行重写该行（换 `--source` 后 URL 随之更新，
单条不重复）。

### 存量节点（如 Spark，装的是旧版 install.sh）手工配置

装上新版 os-api 后（update handler 已支持远端分支），旧 unit 里没有该 env，
需要手工补一行：

```bash
# 编辑 /etc/systemd/system/nexos-os-api.service，[Service] 段加：
Environment=NEXOS_UPDATE_REPO_URL=http://203.0.113.2:8558/git/nexos.git
# （指向任一有 /tank 本地副本的联邦节点；Spark 由 aliyun 安装，用 aliyun）
systemctl daemon-reload && systemctl restart nexos-os-api
```

或者直接重跑一次新版 `install.sh`（等价自动写入，且幂等）。配置后「更新」
页 check 显示远端 git 模式（URL 可见），不再降级空清单。
