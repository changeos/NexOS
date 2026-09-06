# 远程转发（Forwarding）——SSH 隧道 + RDP 转发

> 源码：`crates/os-api/src/handlers/forwarding.rs`（handler 组件名 `forwarding`）。
> 定位：让 Web UI / CLI / MCP 经 HTTP 管理两类端口转发——SSH 隧道（spawn 系统
> `ssh` 子进程）与 Windows RDP 转发（纯 Rust TCP 代理 + `.rdp` 客户端配置下载）。

## 1. 功能说明

- **SSH 隧道**（`ssh/*`）：spawn 系统 `ssh` 子进程实现三种隧道——
  - `local`（`-L`）：本地 `local_bind` → 经 SSH 服务器 → `remote_host:remote_port`
  - `remote`（`-R`）：SSH 服务器上的 `local_bind`（当作远端绑定）→ 本机可达的 `remote_host:remote_port`
  - `dynamic`（`-D`）：本地 `local_bind` 起 SOCKS5 动态代理（不允许带 remote_host/remote_port）
  - spawn 后 ~800ms 探测退出码：起不来立刻 `failed` 带 stderr 摘要（stderr 落临时日志
    文件 `/tmp/os-ssh-tunnel-<id>.log`，避免管道满阻塞），存活则 `running` + pid。
- **RDP 转发**（`rdp/*`）：纯 Rust TCP 代理（tokio `TcpListener` + `copy_bidirectional`），
  绑定 `0.0.0.0:<listen_port>` 转发到远端 Windows `target_host:target_port`（默认 3389），
  accept 计数（累计连接数持久化）；端口冲突/占用降级 `error` 状态带原因，绝不 panic。
- **重启恢复**：定义全部落 SQLite；os-api 启动时 `resume_autostart`（后台任务）先把上一
  进程遗留的 `running` 态做存活探测（pid 活 → 收养保持 running；死 → stopped），再对
  `autostart=true` 的条目尝试 start。
- **不 seed demo 数据**：真实工具配置（autostart 隧道会真实 spawn ssh / 绑定端口），
  各 GET 首次返回空数组。

## 2. 路由表（13 条，component="forwarding"）

读端点免认证；写端点 `requires_auth` + `admin`。

| method | path | 动作 | 鉴权 |
|--------|------|------|------|
| GET | `/api/v1/forwarding/ssh` | SSH 隧道列表 | 公开 |
| POST | `/api/v1/forwarding/ssh` | 创建隧道 | admin |
| GET | `/api/v1/forwarding/ssh/:id` | 隧道详情（实时存活探测） | 公开 |
| DELETE | `/api/v1/forwarding/ssh/:id` | 删隧道（运行中先停） | admin |
| POST | `/api/v1/forwarding/ssh/:id/start` | 启动（spawn ssh） | admin |
| POST | `/api/v1/forwarding/ssh/:id/stop` | 停止（kill 子进程） | admin |
| GET | `/api/v1/forwarding/rdp` | RDP 转发列表 | 公开 |
| POST | `/api/v1/forwarding/rdp` | 创建转发 | admin |
| DELETE | `/api/v1/forwarding/rdp/:id` | 删转发（运行中先停） | admin |
| POST | `/api/v1/forwarding/rdp/:id/start` | 启动 TCP 代理 | admin |
| POST | `/api/v1/forwarding/rdp/:id/stop` | 停止代理 | admin |
| GET | `/api/v1/forwarding/rdp/:id/rdp-file?username=` | 下载 `.rdp` 客户端配置 | 公开 |
| GET | `/api/v1/forwarding/stats` | 两类总数/运行数聚合 | 公开 |

创建请求体校验（400）：name/ssh_host/ssh_user/local_bind 非空；`ssh_port`/`remote_port`/
`listen_port`/`target_port` 须 1..=65535；`mode` ∈ {local, remote, dynamic}；
`local_bind` 须 `host:port` 纯格式（不做 DNS 解析）；local/remote 模式必须提供
remote_host+remote_port，dynamic 模式禁止携带。

## 3. 数据结构（DTO 字段）

### SshTunnel（表 `ssh_tunnels`）

| 字段 | 类型 | 说明 |
|------|------|------|
| id | String | UUID v4 |
| name | String | 显示名 |
| ssh_host | String | SSH 服务器主机名/IP |
| ssh_port | u16（默认 22） | SSH 端口 |
| ssh_user | String | SSH 用户名 |
| private_key_path | Option\<String\> | 私钥路径；None = 默认 `~/.ssh/id_ed25519`（透传 ssh `-i`，支持 `~` 展开） |
| mode | String（默认 `local`） | `local`(-L) / `remote`(-R) / `dynamic`(-D SOCKS) |
| local_bind | String | 本地绑定地址（`127.0.0.1:8080` / `0.0.0.0:8080`）；remote 模式下当作远端绑定 |
| remote_host | Option\<String\> | 转发目标主机（local/remote 必填；dynamic 必须为空） |
| remote_port | Option\<u16\> | 转发目标端口（同上） |
| autostart | bool | os-api 启动时自动拉起 |
| status | String（默认 `stopped`） | `stopped` / `running` / `failed` |
| pid | Option\<u32\> | 运行中 ssh 子进程 pid |
| error | Option\<String\> | 最近一次错误摘要（spawn 失败/异常退出/stale） |
| created_at | String | 创建时间（RFC3339） |
| last_started | Option\<String\> | 最近一次成功启动时间（可空） |

### RdpForward（表 `rdp_forwards`）

| 字段 | 类型 | 说明 |
|------|------|------|
| id | String | UUID v4 |
| name | String | 显示名 |
| target_host | String | 远端 Windows 主机（RDP 服务器） |
| target_port | u16（默认 3389） | 远端 RDP 端口 |
| listen_port | u16 | 本机监听端口（`0.0.0.0:<listen_port>` → target） |
| autostart | bool | os-api 启动时自动拉起 |
| status | String（默认 `stopped`） | `running` / `stopped` / `error` |
| connections | u64 | 累计接受的连接数（持久化） |
| error | Option\<String\> | 最近一次错误（端口绑定失败等） |
| created_at | String | 创建时间 |

### ForwardingStats（`GET /stats` 响应）

`ssh_tunnels_total` / `ssh_tunnels_running` / `rdp_forwards_total` /
`rdp_forwards_running` / `rdp_total_connections`（RDP 累计连接数之和）。

## 4. 环境变量

全部从 `crates/os-api/src/handlers/forwarding.rs` grep 核实：

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_SSH_BIN` | `ssh` | ssh 二进制路径覆写。测试注入 `/bin/false`（必失败）或 shell 脚本（模拟存活），不起真实网络；运维可指向非默认安装的 ssh |
| `NEXOS_FORWARDING_HOST` | `hostname` 命令输出（再回退 `localhost`） | `.rdp` 文件的 `full address` 生成时本机地址覆写（Host 头缺失时的回退；模式同 os-nexhub code_repo） |
| `OS_FORWARDING_HOST` | 同上（次优先） | 同 `NEXOS_FORWARDING_HOST` 的旧名兼容，仅在 NEXOS_ 前缀未设置时生效 |

注意：`.rdp` 文件地址解析优先级是 **请求 Host 头（去端口） > NEXOS_FORWARDING_HOST /
OS_FORWARDING_HOST > hostname 命令 > "localhost"**；env 只影响回退路径。

## 5. SSH 密钥认证红线

- **模型与请求体没有任何密码字段**：`POST /api/v1/forwarding/ssh` 请求体若出现
  `password` 字段直接 **400**（"SSH 隧道仅支持密钥认证（private_key_path），
  不接受 password 字段"）。
- spawn 时强制 `BatchMode=yes` 禁密码交互——ssh 无可用密钥即失败退出，绝不挂起等密码。
- 完整命令行（`build_ssh_args` 纯函数）：
  `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=30
  -o StrictHostKeyChecking=accept-new -i <key> -p <port> [-L|-R|-D ...] user@host`

## 6. .rdp 文件说明

`GET /api/v1/forwarding/rdp/:id/rdp-file?username=` 生成 Windows 远程桌面客户端配置：

- `full address` = `<host>:<listen_port>`（host 解析见 §4 优先级；username 为空则省略
  `username:s:` 行，值经 URL decode）；
- 其余为合理默认：全屏（1920×1080 / 32bpp）、允许断线重连、剪贴板重定向开、打印机
  重定向关、`authentication level:i:2`；
- 附注释行 `# smart sizing:s:1`（删除行首 `#` 启用窗口自适应缩放）；
- 响应头：`content-type: application/rdp` + `content-disposition: attachment;
  filename="<净化后的转发名>.rdp"`（非字母数字/-/_/. 替换为 `-`，中文等 Unicode
  字母保留）。

## 7. 持久化路径（forwarding.db）

- 默认路径探测顺序：`/tank/os-data/forwarding.db` → `/var/lib/os/forwarding.db` →
  `./forwarding.db`（父目录存在或可创建即选中；仓库根的 `forwarding.db` 即 cwd 兜底产物，
  伴生 `-shm`/`-wal` 为 SQLite WAL 模式文件）。
- 打开失败时降级内存库（eprintln 警告，不 panic）；测试用 `with_db_path` /
  `with_empty` 注入。
- 两张表：`ssh_tunnels`、`rdp_forwards`（列与 §3 DTO 一一对应，`IF NOT EXISTS` 建表）。
