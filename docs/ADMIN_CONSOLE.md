# 管理应用（Admin Console / Web 终端）

> 源码：`crates/os-api/src/handlers/terminal.rs`（`TerminalRouteHandler`，组件名 `terminal`）·
> 前端：`crates/os-api/web/src/views/AdminConsole.vue`（路由 `/terminal`，appRegistry id=`terminal`，桌面名「管理」）·
> WS 升级层：`crates/os-api/src/http.rs`（`terminal_ws_handler`，`/ws/terminal/{session_id}`）·
> 依赖：`portable-pty`（纯 Rust PTY）+ `@xterm/xterm` / `@xterm/addon-fit`（前端）·
> 登记：2026-08-25

## 0. 用户定调（原话拆解）

「增加管理功能，与设置功能不冲突，管理功能得有 ssh 终端，可以在打开终端」——

- **独立的「管理」应用**：不并入设置（Settings.vue 零改动），桌面/Dock/Launchpad
  单列图标（终端 `>_` 风格 SVG）；
- **SSH 终端**：可打开远程主机的交互式 shell；
- **本地终端**：本机 shell 的交互式会话。

## 1. 架构

```
┌──────────────────────────── 浏览器 ────────────────────────────┐
│  AdminConsole.vue（多会话 Tab + 会话栏）                        │
│    ┌────────── xterm.js 实例（每会话一个） ──────────┐          │
│    │  onData(bytes) ──→ base64 ──→ input 帧          │          │
│    │  output 帧 ──→ base64 解码 ──→ write(bytes)     │          │
│    │  FitAddon.fit() ──→ resize 帧（cols/rows）      │          │
│    └────────────────────┬────────────────────────────┘          │
└─────────────────────────┬───────────────────────────────────────┘
              WebSocket（JSON 文本帧，?token=<admin token>）
                          │ ws(s)://host/ws/terminal/{session_id}
┌─────────────────────────┴──── os-api 网关 ──────────────────────┐
│  http.rs terminal_ws_handler（admin token 握手即验，失败 401）  │
│    │ 帧路由：input → PTY 写端（spawn_blocking）                  │
│    │          resize → MasterPty::resize（SIGWINCH）             │
│    │          output/exit/error ← broadcast 通道                 │
│  terminal.rs TerminalSessions（进程级共享注册表）                │
│    │ spawn：openpty → spawn_command → reader(阻塞线程)           │
│    │        → 聚合任务（50ms 节流）→ broadcast                   │
│    │ EOF：flush 余量 → child.wait() 退出码 → exit 帧 → 自清理    │
└────┬──────────────────────┬─────────────────────────────────────┘
     │ PTY (portable-pty)   │ PTY
┌────┴──────────┐    ┌──────┴──────────────────────────┐
│ bash（本地）    │    │ ssh -tt -p <port> [-i key]      │
│ $SHELL, cwd=HOME│   │   -o StrictHostKeyChecking=     │
│                │    │      accept-new user@host（远程） │
│                │    │ 密码提示经 PTY 透传回浏览器 ⌨     │
└────────────────┘    └─────────────────────────────────┘
```

### 关键机制

- **PTY 方案**（`portable-pty`，wezterm 抽出的纯 Rust 库）：本地终端 spawn
  `$SHELL`（缺省 `/bin/bash`）于 PTY（cwd=HOME）；SSH 终端 spawn `ssh -tt
  ...` 于 PTY——**密码提示/交互全部透传**，服务端不碰凭据，密码认证在浏览器
  里直接输（与 provisioning SSH 部署的 BatchMode 无人值守是刻意分界）。
- **输出聚合节流**：PTY 读端（阻塞线程）把输出块送无界通道；聚合任务按
  50ms 窗口合并成单个 output 帧（超 64KB 立即冲刷防大输出内存堆积）——
  `cat` 大文件时不会以高频小帧打爆 WS。
- **会话与 WS 连接解耦**：会话存活于服务端注册表，浏览器断线/刷新重连
  （同 session_id）即续流；显式关闭走 `DELETE /sessions/:id`。
- **EOF 自清理**：子进程退出 → PTY 读端 EOF → 聚合任务 `child.wait()` 取
  真实退出码 → 广播 exit 帧 → 从注册表自移除（进程退出即清理，不泄漏）。
- **kill 语义**：DELETE / 空闲回收 → SIGHUP 会话首进程（portable-pty
  `ChildKiller`，与 `Child::wait` 分离持有避免互等死锁）+ 关 PTY 主端
  （内核对前台进程组再发 SIGHUP）→ 读端 EOF 走自清理路径。
  `ChildKiller`/`Child` 分离是刻意设计：wait 阻塞等退出与 kill 发信号若
  争同一把锁会互等。

## 2. REST 端点（component=terminal，全部 admin）

| method | path | 语义 | 响应 |
|--------|------|------|------|
| GET | `/api/v1/terminal/sessions` | 活跃会话列表（id/kind/目标/尺寸/创建时间，按创建顺序） | 200 `[{session_id, kind, target, cols, rows, created_at}]` |
| POST | `/api/v1/terminal/sessions` | 创建会话（spawn PTY） | 201 会话信息；400 参数非法；404 target_id 不存在；429 超上限；500 spawn 失败 |
| DELETE | `/api/v1/terminal/sessions/:id` | 删除会话（kill 进程组 + 关 PTY） | 204；404 不存在 |
| GET | `/api/v1/terminal/node-snapshot` | 节点常用状态快照（管理页顶部状态条聚合，2026-08-30） | 200 `{version, uptime_secs, p2p_connected, disk_use_pct, mem_use_pct}` |

`node-snapshot` 聚合口径（只读聚合，无执行面）：

- `version`：与 `/update/status` 的 `current_version` 同源（env `NEXOS_VERSION`
  优先，缺省包版本）；
- `uptime_secs`：`/proc/uptime`（monitor 同款读取函数复用）；
- `p2p_connected`：os-p2p Handle 的 peers 已连接计数（main.rs 注入，与 p2p/
  node_view 共享同一 clone）；P2P 未启用（`NEXOS_P2P_ENABLE` 未开）时 `null`；
- `disk_use_pct` / `mem_use_pct`：根分区与内存使用率（0-100 一位小数；内存
  used = total - available，与 `/monitor/metrics` 同口径；读取失败 0）。

POST 请求体：

```jsonc
{
  "kind": "local" | "ssh",   // 必填
  // ssh 二选一来源：
  "target_id": "ssh-1",      // provisioning SSH 目标（只读复用，优先）
  "host": "10.0.0.2", "port": 22, "user": "root", "key_path": "/abs/path/id_ed25519",
  // 直连 key_path 限绝对路径（~ 与相对路径 400；target 来源的路径原样透传）
  "cols": 80, "rows": 24     // 缺省 80×24，clamp 到 [2,500]×[2,300]
}
```

## 3. WebSocket 帧协议

连接：`ws(s)://<host>/ws/terminal/<session_id>?token=<admin token>`

- 握手即验 token：与 `NEXOS_ADMIN_TOKEN`（`OS_ADMIN_TOKEN`）精确匹配，失败
  401；未配置 admin token 的部署**一律拒绝**（终端无匿名通道）；会话不存在 404。
- 全部 JSON **文本帧**（binary 帧回 error 提示）：

| 方向 | 帧 | 字段 | 语义 |
|------|----|------|------|
| C→S | `{"type":"input","data":"<base64>"}` | data：字节流 base64 | 终端输入（含控制键/粘贴，全透传写 PTY） |
| C→S | `{"type":"resize","cols":N,"rows":N}` | cols/rows | 窗口尺寸（PTY winsize + SIGWINCH） |
| S→C | `{"type":"output","data":"<base64>"}` | data | 终端输出（50ms 聚合批） |
| S→C | `{"type":"exit","code":N}` | code | 子进程退出码（EOF 后取真实值；发完即关连接） |
| S→C | `{"type":"error","msg":"..."}` | msg | 协议/IO 错误（不关连接） |

未知 `type` 解析失败回 error 帧；慢消费者丢帧（broadcast Lagged）回 error
提示后继续（流式最新语义，不阻塞 PTY 读端）。

## 4. 鉴权模型

- **终端 = 最高权限面**（等效 root shell），三个 REST 端点全部
  `requires_auth=true + roles=["admin"]`，WS 握手 token 与 REST 的
  `Authorization: Bearer` **同源**（`NEXOS_ADMIN_TOKEN`）；
- 浏览器端 token 来自「设置 → API 令牌」（localStorage `os-api-token`），
  AdminConsole 的 REST 调用经 client.ts 自动带 Bearer 头，WS 连接以
  `?token=` 查询参数携带（与 IM `/ws` 的 query token 模式同款）；
- 未配置 admin token 的部署：REST 写操作 401（网关 AuthMiddleware），WS
  一律 401——默认拒绝，无降级通道。

## 5. 资源限制

| 项 | 值 | 行为 |
|----|----|------|
| 并发会话上限 | 8（`MAX_SESSIONS`） | 超限 POST 429（文案附上限与提示） |
| 空闲回收 | 30 分钟无输入且无输出（`IDLE_TIMEOUT`） | 后台任务 60s 巡检，`reap_idle` kill+清理 |
| 输出聚合 | 50ms 窗口 / 64KB 硬上限 | 防高频小帧 & 大输出内存堆积 |
| 广播通道 | 1024 帧（≈51s @20帧/s） | Lagged 丢旧帧 + error 提示 |
| 尺寸范围 | cols∈[2,500]，rows∈[2,300] | 防御 0/巨幅值 |

## 6. 与 provisioning SSH targets 的复用关系

**只读复用，零复制状态**：

- 目标注册表唯一权威源仍是 `ProvisioningRouteHandler`（「系统自举 → SSH
  远程部署」维护，`GET /api/v1/provisioning/ssh/targets` 公开读）；
- main.rs 装配把 provisioning 包成 `Arc` 共享实例（`SharedProvisioningHandler`
  纯转发注册——`SharedApiGatewayHandler` 同款先例），`terminal` 组件经
  `SshTargetsProvider` 闭包调 `ssh_targets_snapshot()` 实时读同一份内存态；
- 「管理」应用 SSH 目标下拉的数据即 `provisioningSshTargets()`（前端同源）；
  选目标开终端 = `POST {kind:"ssh", target_id}` → 后端按注册表解析
  host/port/user/key；provisioning.rs 本体**零改动**；
- 分工：provisioning 是**无人值守部署**（BatchMode 禁密码、scp/ssh 单发），
  terminal 是**交互式运维**（-tt 透传、密码可直接输）——同一批目标两种用法。

## 7. 安全考量

1. **admin 面收敛**：全部端点 + WS 握手 admin；文档/代码注释均标注「终端 =
   最高权限面」。
2. **无 shell 拼接**：ssh 参数 argv 直传（`ssh_argv` 纯函数组装，
   `CommandBuilder` 不经 shell），注入面为零——测试断言 argv 恰为 8 个参数。
3. **key_path 限绝对路径**：直连参数相对路径/`~` 一律 400（防按服务端 cwd
   解析到意外位置）；target 来源路径由 provisioning 域负责原样透传
   （OpenSSH 对 `-i` 自行做 `~` 展开）。
4. **服务端不碰凭据**：密码只在 PTY 字节流里过境（浏览器 ↔ PTY ↔ ssh），
   terminal.rs 不解析不存储。
5. **资源红线**：会话上限 8 / 空闲 30 分钟回收 / 输出聚合限流，防止终端
   被用作资源滥用入口。
6. **StrictHostKeyChecking=accept-new**（TOFU）：与 provisioning SSH 同款，
   首连自动接受新主机密钥、已记录主机密钥变化仍拒绝。

## 8. 测试（21 个，`cargo test -p os-api --lib`）

terminal.rs（16）：

| # | 测试 | 覆盖 |
|---|------|------|
| 1 | `ssh_argv_assembles_flags_port_key_and_destination` | ssh 参数组装纯函数（-tt/-o/-p/-i/目标，无 key 不出 -i） |
| 2 | `ws_frame_json_codec_roundtrip` | input/output/resize/exit/error 帧 JSON 编解码；未知 type 拒绝 |
| 3 | `throttle_buf_batches_within_window` | 输出节流（注入时钟：窗口内合并/到期冲刷/超上限立即冲） |
| 4 | `routes_declare_admin_only_endpoints` | 路由声明（4 条）+ 全端点 admin（含 node-snapshot 鉴权） |
| 5 | `ssh_key_path_must_be_absolute` | key_path 非绝对路径 → 400 |
| 6 | `ssh_requires_host_and_known_kind` | 缺 host / 非法 kind → 400 |
| 7 | `ssh_unknown_target_id_returns_404` | 未知 target_id → 404 |
| 8 | `local_pty_spawn_and_echo_roundtrip` | 本地 PTY spawn + echo 往返（写 `echo os$((6*7))term` 读 `os42term`）+ resize |
| 9 | `session_lifecycle_create_list_delete` | 创建→列表→删除→404 |
| 10 | `session_limit_returns_429` | 上限 2 时第 3 个 429；删除释放配额 |
| 11 | `eof_cleanup_sends_exit_frame_and_removes_session` | `exit` 命令 → exit 帧（code=0）+ 注册表自移除 |
| 12 | `idle_reap_kills_inactive_sessions` | 空闲回收（注入时钟拨回 last_active）只杀超时会话 |
| 13 | `ssh_session_spawns_fake_ssh_with_expected_argv` | PATH 注入假 ssh 落盘 argv：-tt/accept-new/-p 2222/-i 绝对路径/root@host，恰 8 参数 |
| 14 | `delete_session_kills_child_process` | kill 连带清理孙进程（扫 /proc 验证 sleep 子进程终止） |
| 15 | `node_snapshot_aggregates_shape` | node-snapshot 聚合形状：五字段齐全/百分比 0-100/P2P 未注入 null/未知子路径 404 |
| 16 | `use_pct_handles_zero_total_and_clamps` | 使用率纯函数：total=0 防除零/一位小数/超界钳制 |

http.rs（5，真实 serve + tokio-tungstenite）：

| # | 测试 | 覆盖 |
|---|------|------|
| 15 | `terminal_ws_without_token_rejected_401` | 无 token 握手 401 |
| 16 | `terminal_ws_wrong_token_rejected_401` | 错 token 401 |
| 17 | `terminal_ws_no_admin_token_configured_rejects_all` | 未配置 admin token 一律 401 |
| 18 | `terminal_ws_unknown_session_404_after_auth` | 鉴权通过 + 会话不存在 404 |
| 19 | `terminal_ws_real_roundtrip_input_output` | e2e：token 握手 101 → input 帧 → PTY bash → output 帧读到真实执行结果 |

注：oneshot 探测过不了 axum `WebSocketUpgrade` 提取器（无可升级连接，固定
426），故 15–18 经真实 `axum::serve` + tungstenite 握手断言 HTTP 状态。
PTY 测试以 `#[cfg(unix)]` 门（Linux 目标环境可用；理论移植面跳过）。

## 9. 前端集成要点（AdminConsole.vue）

- **xterm.js**：`@xterm/xterm` + `@xterm/addon-fit`（官方新包名）；CSS 经
  `import '@xterm/xterm/css/xterm.css'`（Vite 原生支持包内 CSS 导入）；
- **attach 自实现**（4 行核心：`onData → input 帧`；`output 帧 → write`），
  不引入 addon 依赖；
- **resize 同步**：`ResizeObserver` 监听容器 → `FitAddon.fit()`（100ms
  节流）→ 发 resize 帧；Tab 切换时重新 fit + 聚焦；
- **多会话**：每会话一个 Terminal 实例（`v-show` 保活，切 Tab 不丢回滚）；
  状态点（绿=连接 / 黄=连接中 / 红=已退出 / 灰=断开）；
- **黑色终端主题**（#0c0c10 底 + 16 色），与全站 Yaru 风格协调；
- **关闭 Tab = DELETE 会话**；exit 帧打印「进程已退出」提示（Tab 保留看
  回滚，关闭释放）；
- **恢复**：`onMounted` 读 `GET /sessions` 把服务端存活会话全部重连续上
  （页面刷新不丢会话）。

### 快捷命令面板 + 终端体验（2026-08-30）

- **快捷命令面板**（Tab 条下方，可折叠）：预置四分类命令集（系统/网络/
  NexOS 运维/Docker，17 条）+ 用户自定义命令；分类 pill 过滤 + 搜索框
  （名称/命令不区分大小写包含匹配）；点击 = 向当前激活终端 WS 发 input 帧
  （命令 + `\n`，与手敲完全等价——**无新增执行面**）；发送后面板自动收起，
  勾选「常驻」保持展开；
- **自定义命令持久化**：localStorage `admin-quick-cmds`（读取时净化：缺
  名/命令的条目丢弃），同名拒绝，面板内直接添加/删除；
- **终端工具**（面板头部右侧）：字号 A-/A+（9-22px，全部会话实时应用 +
  新会话沿用，localStorage `admin-term-fontsize`）、一键清屏
  （`terminal.clear()`）、复制全部输出（buffer 逐行 `translateToString`
  序列化含回滚区，Clipboard API 优先 + `execCommand` 回退）；
- **会话重命名**：Tab ✎ 按钮 / 双击 Tab 标签 → 行内输入（Enter 确认 / Esc
  取消 / 失焦确认），localStorage `admin-session-names` 记 id→名，关闭 Tab
  连带清理；
- **节点状态条**（页面顶部）：`GET /terminal/node-snapshot` 聚合
  版本/在线时长/P2P 连接数/磁盘/内存（磁盘 ≥85%、内存 ≥90% 黄色预警），
  **点击对应项直接往终端发对应快捷命令**（点磁盘→`df -h`，点内存→`free -h`，
  点版本→update/status，点在线→`uptime`，点 P2P→p2p/peers）；
  无 admin token / 请求失败时整体隐藏。

## 10. 相关文件索引

| 层 | 文件 |
|----|------|
| 后端 handler | `crates/os-api/src/handlers/terminal.rs` |
| WS 升级层 | `crates/os-api/src/http.rs`（`terminal_ws_handler` / `run_terminal_ws` / build_router 挂载） |
| 装配 | `crates/os-api/src/main.rs`（terminal 注册 + provisioning 共享实例 + 空闲回收任务） |
| 前端视图 | `crates/os-api/web/src/views/AdminConsole.vue` |
| 前端注册 | `appRegistry.ts`（组件/桌面/分类）· `router/index.ts` · `DashboardView.vue`（图标+桌面项）· `components/AppIcon.vue` |
| API 客户端 | `crates/os-api/web/src/api/client.ts`（`terminal*` 四端点 + 契约类型；WS 直连不走 client） |
| 依赖登记 | 根 `Cargo.toml`（`portable-pty`）· `web/package.json`（`@xterm/*`） |
