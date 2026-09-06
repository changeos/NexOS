# 安装自己的应用：现状与正确姿势

> 目标：回答「怎么把自己开发的应用装进 NexOS」。结论先行：**当前唯一正路 =
> 进主仓开发**（本文 §2 全流程）；应用中心 `install_type=nexos` 只覆盖第一方
> 预置应用，外部二进制/APK 式安装通道**尚未开放**（开放点见 §4）。
>
> 前置：读过 [01-app-development.md](01-app-development.md)（新增桌面应用全流程）。

## 1. 应用中心现状（install_type=nexos）

后端：`crates/os-api/src/handlers/app_store.rs`（组件名 `app_store`）。

- **预置目录全部为第一方应用**：`source="nexos"` / `install_type="nexos"`
  （每个预置条目一个内置 struct，见 `app_store.rs` 的目录构建函数）；
- **外部渠道一律拒绝**：`POST /api/v1/appstore/publish` 拒绝
  `install_type` 为 apt/deb/snap/flatpak 的发布请求（400，
  `EXTERNAL_INSTALL_TYPES` 常量——2026-08-23 需求：应用中心仅 NexOS 官方
  应用，无第三方上架渠道）；
- **安装语义**：`POST /install(app_id)` → 查目录拿 install_type/target →
  建 pending 任务（任务流）；`nexos` 类应用是「已随 os-api 内置」的语义
  标记，不是下载安装器；
- **卸载**：`POST /uninstall` 对 `install_type=="nexos"` 的内置应用直接拒绝。

详见 [../APPSTORE.md](../APPSTORE.md)（含拓扑图与 11 条路由表）。

## 2. 正确姿势：进主仓开发（当前唯一通道）

```text
① clone 主仓（NexHub: http://agent:<TOKEN>@192.0.2.106:8558/git/nexos.git）
② 按 01-app-development.md 新增应用（前端 view + 注册三处）
   + 按 06-os-api-handler.md 新增/扩展后端 handler
③ cargo test -p os-api --lib 全绿 → 单 commit（中文 conventional）
④ push 到 NexHub（日常只推 NexHub，不推 GitHub——用户指示）
⑤ 106 即时生效（debug 二进制 + web 磁盘直读）；113/aliyun 走 §3 部署
```

## 3. 分发到其他节点（当前形态）

- **二进制**：106 `cargo build --release` → scp 到 113/aliyun
  `/opt/nexos/os-api` → `systemctl restart os-api`；
- **web**：`npm run build` 后 **tar 整个 `static-dist/`** 传到
  `/opt/nexos/crates/os-api/static-dist`（服务从这里读；aliyun 曾因漏传
  显示旧版）；
- **应急通道（sshd 不可用）**：Files API 上传 + `/etc/cron.d` 一次性任务；
  更新工件走 `POST /api/v1/update/artifact` 登记（ELF + sha256 校验）
  → `POST /api/v1/update/apply` 真实安装（staged/备份/rename-over/自重启）。
  详见 [07-deploy.md](07-deploy.md)。

## 4. 后续开放点（未实现，标注以免误用）

| 开放点 | 现状 | 预期形态 |
|---|---|---|
| 第三方应用包 | ❌ 无包格式/签名/沙箱 | 候选：`install_type` 新增 `bundle`，经 NexHub 发布 + 链上身份签名 |
| 应用中心用户上架 | ❌ publish 仅接受 source=nexos | 上架=发布到 NexHub 的应用仓库目录，商店聚合展示 |
| 远程节点安装 | ❌ 各节点独立部署 | Files API / update artifact 通道扩展 |

> 在上述能力落地前，把外部二进制放到节点上**不会**出现在桌面/应用中心——
> 前端只渲染 appRegistry 内注册的视图。

## 参考

- [../APPSTORE.md](../APPSTORE.md) —— 应用中心全量契约（路由/存储/已知限制）
- `crates/os-api/src/handlers/app_store.rs` —— 来源策略与拒绝逻辑源码
- [07-deploy.md](07-deploy.md) —— 三节点部署与更新通道
