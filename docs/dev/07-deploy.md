# 多节点部署：三节点布局 / 构建分发 / 运维通道

> 目标：知道 NexOS 部署在哪些机器上、一次变更如何到达全部节点、sshd 不可用
> 时的应急通道。面向「改完代码要上线」的开发者。
>
> 前置：[01-app-development.md](01-app-development.md) §6（web 构建与嵌入）。
> 全量运维手册：[../DEPLOYMENT.md](../DEPLOYMENT.md)（§9 实机现状）。

## 1. 三节点布局

| 节点 | 地址 | 形态 | 要点 |
|---|---|---|---|
| 106 ub2604 | 192.0.2.106 | 主开发机：debug 二进制 systemd `os-api`，WorkingDirectory=/home/oem/NexOS，**web 从磁盘读即时生效** | NexHub 所在（:8558）；主仓 checkout（开发者中心文档源） |
| 113 node-113 | 192.0.2.113 | release `/opt/nexos/os-api` + static-dist 双份 | 无 checkout（docs 服务降级模式） |
| aliyun | 203.0.113.2（SSH 端口 221） | release `/opt/nexos/os-api` | 公网 NAT 节点（`NEXOS_P2P_ADVERTISE`）；已禁 ping |

三节点经 os-p2p 组网互通（`NEXOS_P2P_ENABLE`，Kademlia + mDNS 种子 +
TCP 打洞阶梯，设计见 [../NEXOS_P2P_NETWORK_DESIGN.md](../NEXOS_P2P_NETWORK_DESIGN.md)）。

## 2. 日常构建分发

```bash
# 106（开发机）：直接构建 + 重启，web 磁盘直读即时生效
cargo build && sudo systemctl restart os-api

# 113 / aliyun：release 构建 → scp 二进制 + tar web → 重启
cargo build --release
scp target/release/os-api  <node>:/opt/nexos/os-api
tar -C crates/os-api/static-dist . | ssh <node> 'tar -x -C /opt/nexos/crates/os-api/static-dist'
# ⚠️ web 必须传 /opt/nexos/crates/os-api/static-dist（服务从这里读；
#    外层 static-dist 无效；重传前先 delete 防重名加后缀）
ssh <node> 'systemctl restart os-api'
```

质量门（与 CI / `make all` 一致）：`make check` / `make clippy`（-D
warnings）/ `make test`（`--features mock`）。

## 3. 更新通道（artifact 闭环，106 自验证过）

「更新」应用（`/update`）已做实自家闭环——发版 = NexHub 裸仓库打 tag：

```text
发版：workspace 版本 bump → commit → tag vX.Y.Z → push NexHub（+发版时同步 GitHub 镜像）
检查：POST /api/v1/update/check   → git for-each-ref 读 tag → semver 比较 → 通道过滤
工件：POST /api/v1/update/artifact {version, path}   （Files API 上传本机后登记：ELF 魔数 + sha256）
安装：POST /api/v1/update/apply   → 任务状态机 verifying→writing→reboot_pending→done
       （staged 拷贝 → 备份近 3 份 → rename-over 运行中二进制 → systemd 自重启）
```

详见 [../UPDATE_APP.md](../UPDATE_APP.md)。

## 4. aliyun cron 通道（sshd 不可用时的应急）

sshd 双端口拒连期间验证过的双通道（2026-08-24 实录）：

- **二进制**：Files API 上传 `apply.sh` + `os-api.new` → `/etc/cron.d` 一次性
  任务分钟级执行（安装→重启→自清理）；
- **web**：Files API 先 delete 整目录（防重名加后缀）再重传。

## 5. 发版与镜像策略（用户指示）

- 日常变更**只推 NexHub**，不碰 GitHub；
- 攒到里程碑发版：tag 后一次性
  `git push https://x-access-token:<TOKEN>@github.com/changeos/NexOS.git main --tags`
  （token 在跳板 /root/.gh_token_changeos）。

## 参考

- [../DEPLOYMENT.md](../DEPLOYMENT.md) —— §9 实机部署现状（os-api.service /
  /etc/default/os-api 全量 env 表 / 端口语义）
- [../UPDATE_APP.md](../UPDATE_APP.md) —— 更新应用全量契约
- MEMORY.md（仓库根）—— 节点凭据与最新运维实录（本页不复制密钥）
