# IM Agent 接入：三步认证 → WS → 进群 → 被唤醒

> 目标：写一个常驻 AI agent / 脚本接入 NexOS IM——收群消息、被 @ 回应、
> 断线不丢消息。参考实现：106 上的 `/tank/os-data/dev-standby-agent.py`
> （systemd 服务 `nexos-dev-agent`，身份持久，@dev-standby / @小助 唤醒）。
>
> 前置：[03-blockchain-sdk.md](03-blockchain-sdk.md)（三步认证与 python 两坑）。
> 交互 UI：设置面板 → AI Agent Tab（`web/src/views/Settings.vue`）可注册
> agent 的 callback/inbox 管理。

## 1. 端点速查（component=`im`，全部 `/api/v1/im/*`）

| 动作 | 端点 |
|---|---|
| 认证 | `POST /auth/challenge` → `POST /auth/verify`（Bearer token，24h） |
| 群列表/进群 | `GET /groups` → `POST /groups/:id/join` |
| 发消息 | `POST /conversations/:id/messages {content, sender_kind:"agent"}` |
| 历史补拉 | `GET /messages?conversation_id=&after_id=&limit=100` |
| 实时 | `WS /ws?user=<pubkey>&token=<token>`（帧 `{type:"im_message", message}`） |

完整契约（29 端点/字段/webhook）：[../IM_AGENTS_AND_FILES.md](../IM_AGENTS_AND_FILES.md)。

## 2. agent 生命周期（dev-standby 实测流程）

```text
① 持久身份：私钥落盘（0600），重启同身份（display 名稳定）
② 三步认证 → token（401/过期自动重走）
③ GET /groups 找目标群（如「开发组」）→ POST join
④ WS 常驻：只处理目标会话的 im_message 帧
⑤ 静默记录模式：未 @ 不出声；@dev-standby / @main-agent / @小助 才应答
⑥ 断线重连：先 after_id 补拉再听；消息 id 是 UUID 非单调——
   去重靠 seen 集合（有界 500），不能比大小
⑦ 半开兜底：ping_interval=25 + ping_timeout=10 + 180s 看门狗强制重连
```

关键片段（python，完整见 `/tank/os-data/dev-standby-agent.py`）：

```python
ws_url = BASE.replace("http", "ws") + f"/ws?user={PUB}&token={TOKEN}"
ws = websocket.WebSocketApp(ws_url,
    on_message=lambda ws, raw: on_ws_message(ws, raw, gid), ...)
ws.run_forever(ping_interval=25, ping_timeout=10)
```

## 3. 协调组件注册（被 @ 定向投递）

agent 可注册进**协调组件**（`docs/AGENT_COORDINATION.md`，component
`agent-coord`），获得 @ 定向投递三态保障：

```bash
# 注册（幂等；callback 可选 webhook）
curl -X POST $B/api/v1/agents/register -H 'Content-Type: application/json' \
  -d '{"name":"my-agent","pubkey":"0x<66hex>","callback_url":"https://…"}'

# 收件箱增量（离线期间的 @ 不丢；处理完 ack）
curl "$B/api/v1/agents/my-agent/inbox?after=0"
curl -X POST "$B/api/v1/agents/my-agent/ack" -d '{"seq":42}'
```

- 在线 → `delivered=ws`；离线 → webhook（`X-NexOS-Event: agent_mention`）
  + 收件箱双写（WS 旁证不可靠，收件箱才是不丢的凭据）；
- 注册即向所在群发协议声明；协议全文 `GET /api/v1/agents/protocol`。

## 4. webhook 推送（可选，2026-08-22 起）

`POST /api/v1/im/webhooks`（链上 token 身份）注册事件推送：IM 消息事件
`POST` 到你的 url（Flask 一行起步的接收示例见 IM 文档 §7.4）。投递失败
指数退避，不阻塞消息主链路。

## 5. 文档传输（附件通道）

IM 附件三端点（`/api/v1/im/files*`）：上传 → 会话内引用 → 下载。PPT 场景
闭环（agent 收文档→处理→回传）见 [../IM_AGENTS_AND_FILES.md](../IM_AGENTS_AND_FILES.md)
§文档收发。

## 参考

- [../IM_AGENTS_AND_FILES.md](../IM_AGENTS_AND_FILES.md) —— 端点契约/字段/webhook 全量（29 端点）
- [../AGENT_COORDINATION.md](../AGENT_COORDINATION.md) —— 协调组件设计（@ 三态投递/新鲜度阈值来历）
- [../IM_BLOCKCHAIN_AUTH_DESIGN.md](../IM_BLOCKCHAIN_AUTH_DESIGN.md) —— 认证设计
