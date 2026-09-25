# 外部 LLM API 接入指南（免费 / 虚拟货币购买）

> 背景（2026-08-16 用户决策）：本地推理**不常驻**——显存按需（生图/推理交替），
> AI 能力优先走**网络 API**：免费渠道或用虚拟货币购买的渠道。
> 基础设施已全部就绪：API 网关（One API 式）渠道聚合 + 按量计费 + 兑换码充值。

## 1. 链路总览

```
Windows/Web 客户端 ──sk-os-xxx──▶ /api/v1/gateway/v1/chat/completions
                                      │ 鉴权(token)→选渠道(优先级/权重/故障转移)
                                      │→渠道A: SiliconFlow(免费Qwen)     ← 免费档
                                      │→渠道B: DeepSeek(低价)            ← 购买档
                                      │→渠道C: 本地 vLLM(llm-101 运行时) ← 兜底档
                                      ▼
                              计费(ModelRatio×GroupRatio)→扣 token 余额→日志
```

- **虚拟货币闭环**：`/api/v1/gateway/redemptions`（admin 生成兑换码）→ 用户兑换充值
  sk-os- token 余额 → 按模型费率扣减。渠道的上游 key 由管理员统一持有，用户只见 sk-os-。
- **OpenAI 兼容**：客户端把 base_url 指到 `http://<host>:8080/api/v1/gateway/v1` 即可用
  任何标准 SDK。

## 2. 上游可达性实测（2026-08-16，本服务器国内网络）

| 上游 | 探测 | 免费额度 | 说明 |
|---|---|---|---|
| SiliconFlow 硅基流动 | ✅ 401 可达 | **有免费模型**（Qwen2.5-7B 等） | 首选免费档，注册送额度 |
| DeepSeek | ✅ 401 可达 | 无（但极低价） | 购买档首选 |
| 智谱 BigModel | ✅ 401 可达 | 新用户赠品 | 备选 |
| OpenAI | ❌ 000 不通 | — | 本网络不可达，勿配 |

## 3. 添加渠道（管理员一次性操作）

```bash
T='Authorization: Bearer <ADMIN_TOKEN>'; B=http://127.0.0.1:8080
curl -X POST -H "$T" -H 'Content-Type: application/json' $B/api/v1/gateway/channels -d '{
  "name": "SiliconFlow-免费Qwen",
  "provider": "openai",                       # openai 兼容协议都用 openai
  "base_url": "https://api.siliconflow.cn/v1",
  "api_key": "<在 siliconflow.cn 注册获取 sk-...>",
  "models": ["Qwen/Qwen2.5-7B-Instruct"],
  "priority": 10, "weight": 1, "enabled": true
}'   # 加完用 POST /channels/:id/test 验证连通
```

- **优先级策略建议**：免费渠道 priority 小（优先）→ 付费渠道兜底 → 本地 vLLM 最后
  （`本地vLLM-7B` 渠道指向 localhost:8000，即 llm-101 实例端口——实例停止时渠道测试
  自然失败，故障转移自动跳过，无需改配置）。
- 本地实例按需：`POST /api/v1/llm/instances/llm-101/start`（加载约 3.5 分钟，
  占 22G 显存；与 sd-turbo 生图互斥，用完 `stop` 释放）。

## 4. 当前渠道状态（2026-08-16 清理后）

| 渠道 | base_url | 状态 |
|---|---|---|
| 本地vLLM-7B | http://localhost:8000/v1 | enabled（随 llm-101 启停） |
| OpenAI官方 | https://api.openai.com/v1 | **建议删除或禁用**（网络不可达） |
| （待加）SiliconFlow / DeepSeek | — | 等管理员填 key |

已清理：`test-ch` / `btntest` / `持久化测试` 三个测试垃圾渠道。

## 5. 客户端接入（Windows agent 参考）

AI 工作台聊天不必直连 /llm/instances（那是本地 vLLM 管理）；走网关中转即可获得
外部模型 + 计费：`POST http://<host>:8080/api/v1/gateway/v1/chat/completions`，
`Authorization: Bearer sk-os-xxx`（token 在 Web 网关页创建），body 标准 OpenAI 格式。
`GET /api/v1/gateway/models` 可列当前可用模型（聚合自全部启用渠道）。
