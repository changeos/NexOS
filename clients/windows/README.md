# NexOS Windows 客户端（子项目）

Windows 桌面侧出入口：不开浏览器即可看服务器仪表盘、进 IM 大厅实时聊天、
浏览 NexHub 大厅并一键克隆仓库。纯 HTTP/WS 客户端，服务端零改动。

## 怎么开始

1. **先读 [PLANNING.md](./PLANNING.md)**——唯一权威文档，完全自包含：
   项目使命、MVP 范围、逐端点 API 契约（含 WebSocket 协议细节）、技术选型
   （建议 Tauri 2 + Rust）、M1~M4 里程碑与可机验的验收标准、开发工作流、
   服务端环境与 curl 自测清单。
2. 日常写代码时把 [API_QUICKREF.md](./API_QUICKREF.md) 当速查表（按端点组一张表）。
3. 获取代码（NexHub 自举，clone 地址即本仓库的 git 服务）：

   ```bash
   git clone http://<user>:<token>@192.0.2.106:8080/git/nexos.git
   # token 当前测试值 change-me-admin-token；主机名 ub2604 与 IP 等价
   ```

## 铁律

- **只改 `clients/windows/`**，不碰 `crates/`、`docs/`、`web/`。
- 分支 `feature/windows-*`，Conventional Commits（`feat(windows): …`），push 回 origin。
- 服务端契约以 PLANNING.md 为准；若与主仓
  `crates/os-api/web/src/api/client.ts` 或 handler 源码冲突，以后者为准。

## 与主仓的关系

- 本目录是 **Rust workspace 之外的独立子项目**：根 `Cargo.toml` 的 members
  不含 `clients/`，**不要把它加进去**；自带工具链（Tauri / C# / Electron 均可）。
- 不参与主仓 CI；只要不改本目录之外的文件，无需跑主仓 `cargo test`。
- 服务端（os-api，端口 8080）24h 在线，只消费、不部署。

## 快速冒烟（任意机器）

```bash
curl http://192.0.2.106:8080/healthz                 # {"status":"ok"}
curl "http://192.0.2.106:8080/api/v1/im/lobby"       # 大厅信息
curl http://192.0.2.106:8080/api/v1/monitor/metrics  # 仪表盘指标
```

更多见 PLANNING.md §8 验收自测清单。
