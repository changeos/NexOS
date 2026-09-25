# 系统自举 · 一键安装引导（BOOTSTRAP_INSTALL）

> **本文目标**：在一台**全新的 NAT 后 Ubuntu 22.04/24.04 机器**上，执行**一条 curl
> 命令**即完成 NexOS（os-api）安装、systemd 服务化并**自动加入集群**——新节点的
> P2P bootstrap 自动指向公网入口，第一个交互对象就是发起安装的那台节点。
>
> 关联：`docs/NEXOS_P2P_NETWORK_DESIGN.md`（组网）、`docs/PROVISIONING.md`
> （系统自举应用本体）、`docs/DEPLOYMENT.md` §9（实机部署现状）。

---

## 一条命令

```bash
sudo bash -c "$(curl -fsSL http://203.0.113.2:8558/api/v1/provisioning/install.sh)"
```

- `203.0.113.2` 是 aliyun 公网入口；**任一公网入口都可以换用**，例如云锚点
  `http://198.51.100.114:8558/api/v1/provisioning/install.sh` 同样成立——脚本由
  源节点动态生成，安装源 URL 按 HTTP Host 头自动推导。
- 内网节点（如 106）不可作安装源：NAT 外的新机不可达，这是设计边界而非缺陷。

## 架构自动分流（x86_64 / aarch64）

脚本开头用 `uname -m` 探测本机架构，**同一条命令**在不同架构机器上自动取对
应二进制（DGX Spark 等 ARM 机器无需任何额外参数）：

| `uname -m` | 下载工件 | 源节点文件 |
|------------|----------|------------|
| `x86_64`   | `/api/v1/provisioning/dist/os-api`         | `/tank/os-data/latest-os-api.bin`（`POST prepare-distributable` 暂存） |
| `aarch64`  | `/api/v1/provisioning/dist/os-api-aarch64` | `/tank/os-data/dist/os-api-aarch64-latest`（`scripts/release.sh` 双架构构建刷新） |
| 其他       | 直接报错终止，提示不支持的架构并列出可用项 | — |

sha256 完整性对拍同样按架构独立：源节点生成 install.sh 时分别读取两份分发件，
把各自摘要烘焙进脚本（`NEXOS_SHA256_EXPECTED` / `NEXOS_SHA256_EXPECTED_AARCH64`），
下载端按探测到的架构取对应期望值对拍；某架构工件未就绪时对拍值为空（脚本跳过
对拍，下载时会得到 404 与刷新指引）。

## 架构

```
 NAT 后新机 (Ubuntu, 无 NexOS)          公网入口节点 (aliyun / 锚点 / 任一节点)
 ─────────────────────────────         ──────────────────────────────────────────
 sudo bash -c "$(curl ...)"   ──HTTP──▶ GET /api/v1/provisioning/install.sh
                                        （按 Host 头 + env 动态渲染 bash 脚本，
                                          经网关 text/* 直传通道原样返回）
        ◀────────────── 未加引号的脚本文本 ───────────────
 apt 装 curl/git/cron/openssh-client
 uname -m 探测架构（x86_64 / aarch64 自动分流）
 探测本机公网出口 IP（NEXOS_GIT_ADVERTISE_HOST）
 curl -o os-api.bin           ◀─HTTP── GET /api/v1/provisioning/dist/os-api
                                        （aarch64 取 dist/os-api-aarch64；
                                          base64 → octet-stream 直传原始字节；
                                          x-nexos-sha256 与本架构烘焙值对拍）
 写 systemd unit nexos-os-api.service
   NEXOS_P2P_BOOTSTRAP=<公网入口>:7070[,203.0.113.2:7070,198.51.100.114:7070]
 systemctl enable --now       ──P2P───▶ 7070 引导连接 + 观测端点八卦
 print NodeID / 控制台地址    ◀─curl──  http://127.0.0.1:8558/api/v1/p2p/status
```

安装完成后：新机自动经 bootstrap 拨入 overlay，注册进全网 node-meta 注册表，
在任意节点控制台的「网络」页可见。

## 服务端三端点（crates/os-api/src/handlers/provisioning.rs）

| method | path | 鉴权 | 说明 |
|--------|------|------|------|
| GET  | `/api/v1/provisioning/install.sh`            | 公开 | 动态生成安装脚本文本（`text/x-shellscript` 原文直传，见 `http.rs::direct_passthrough_bytes`）。bootstrap 缺省列表 = 源节点通告地址（env `NEXOS_GIT_ADVERTISE_HOST`/`NEXOS_P2P_ADVERTISE`，回环/0.0.0.0 剔除）+ `203.0.113.2:7070` + `198.51.100.114:7070` |
| POST | `/api/v1/provisioning/prepare-distributable` | admin | 把当前 os-api 可执行文件暂存到分发路径并流式计算 sha256（tmp+rename 原子替换，幂等）。发新版只需重跑一次。**2026-09-03 起成功后自动登记同版本更新工件**（version=运行二进制 `CARGO_PKG_VERSION`、path=分发产物；复用 `POST /update/artifact` 的校验+sha256，重复 version 覆盖；登记结果随响应 `update_artifact` 字段回传）——「更新」页的应用更新自此 prepare 后即可用，无需再手动 curl 登记 |
| GET  | `/api/v1/provisioning/dist/:artifact`        | 公开 | 分发下载。artifact 精确白名单（`os-api` + `os-api-aarch64`，防穿越）；响应 body 为标准 base64，`content-type: application/octet-stream` 经网关直传解码回原始字节，`x-nexos-sha256` 头供完整性对拍 |

可分发件路径（env `NEXOS_DISTRIBUTABLE_DIR` 覆盖根目录）：

- x86_64 主件：`/tank/os-data/latest-os-api.bin`（`POST prepare-distributable` 暂存）
- aarch64 件：`/tank/os-data/dist/os-api-aarch64-latest`（`scripts/release.sh` 双架构构建刷新）

Web 前端无需单独分发：rust-embed 已把 Vue3 产物内嵌进 os-api 二进制
（`webui.rs`，磁盘 static-dist 仅作缓存绕行），装好二进制即带桌面。

## 参数表（脚本 flags）

| flag | 缺省 | 说明 |
|------|------|------|
| `--source URL`     | 安装来源的 Host 头推导值 | 换一台源节点下载二进制 |
| `--name NAME`      | 主机名                   | 节点昵称（`NEXOS_P2P_NAME`） |
| `--token TOKEN`    | `change-me-admin-token`       | `NEXOS_ADMIN_TOKEN`；**默认值仅供拉起，装完请更换** |
| `--bootstrap LIST` | 见上表 install.sh        | P2P 引导节点逗号列表（`NEXOS_P2P_BOOTSTRAP`） |
| `--port PORT`      | `8558`                   | os-api HTTP 监听端口 |
| `--force`          | 关                       | 二进制已存在也重新下载 |

仓库独立副本：`scripts/install-nexos.sh`（可用环境变量 `NEXOS_INSTALL_SOURCE` /
`NEXOS_INSTALL_BOOTSTRAP` / `NEXOS_INSTALL_SHA256` / `NEXOS_INSTALL_SHA256_AARCH64`
覆盖缺省；不经 os-api 托管时 scp 上去直接跑也可）。

## 幂等与重复执行

- 依赖包检测式安装（齐了就跳过 apt）。
- 二进制已存在且未给 `--force` → 跳过下载。
- systemd unit 每次整文件重写（参数变更即时生效），`daemon-reload` +
  `enable --now` 天然幂等；服务未起来时兜底一次 `restart`。
- 收尾轮询 `GET /api/v1/p2p/status` 最长 30s，摘取 NodeID 与已连节点数打印汇总。

升级已有节点：`POST prepare-distributable` 后对新节点重跑一条命令即可；
已装机器需追加 `--force` 刷新二进制。

## 发版流程要点（运维）

每次发版（`scripts/release.sh` 打 tag + 推送后）**三节点各跑一次
`POST /api/v1/provisioning/prepare-distributable`**，一个动作同时喂饱两条
更新通道（2026-09-03 起）：

1. **dist 下载通道**（install.sh / `--force` 升级）：prepare 暂存
   `latest-os-api.bin`，`GET /dist/os-api` 即可下载；
2. **页内应用更新通道**（「更新」桌面应用 apply）：prepare 自动登记同版本
   更新工件（version=运行二进制版本、path=暂存产物、sha256 同源），此后
   `POST /api/v1/update/apply` 直接可用——**不再需要先手动
   `POST /api/v1/update/artifact` 登记**（此前漏登记会报「版本 X 尚未
   登记更新工件」）。prepare 幂等：重跑覆盖同 version 工件，不增条目。

注意：prepare 登记的是**本机正在运行的二进制**的版本——节点升级到新版
os-api 并重启后再跑 prepare，登记的才是新版工件；无本地构建的节点
（如 DGX Spark）页内升级仍走「下载 dist 产物 → Files API 上传 → 手动
登记」或重跑 install.sh 的既有路径。

## 卸载

```bash
sudo systemctl disable --now nexos-os-api
sudo rm -rf /opt/nexos /etc/systemd/system/nexos-os-api.service
```

## 安全边界

- install.sh / dist 两条公开读只吐脚本与二进制工件，无任何元数据泄露面；
  dist 的 artifact 参数走精确白名单（`DISTRIBUTABLE_ARTIFACTS`），任何
  `..`/编码形态路径注入都到不了文件系统（单测覆盖）。
- 二进制通道无签名体系（A/B 槽位的 ed25519 包验证属 os-update 职域），
  当前以「源内 sha256 对拍 + ELF magic 校验」做完整性底线；`--token` 默认值
  仅用于测试期，正式部署务必显式传入强 token。
