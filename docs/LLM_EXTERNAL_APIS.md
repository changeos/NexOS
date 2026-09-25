# 模型管理 · 外部 API 接入（llm_external）

> 场景（2026-08-31 用户需求）：在 113 节点的「模型管理」里，把 106 节点网关提供的
> qwen3.5-9b API（OpenAI 兼容端点，如 `http://192.0.2.106:8000/v1`）接进来当模型
> 用——对话 / 连通测试，**不在本地拉 vLLM**，纯服务端转发。
>
> 跨网中继（2026-09-02，见 §6）：联邦大厅导入的条目 `endpoint_url` 常是发布者
> 内网地址，跨网节点直连够不着——此类条目带 `via_node`（来源 NodeID），chat/test
> 经 os-p2p overlay 定向**源节点代发**，不直连。

- 后端：`crates/os-api/src/handlers/llm_external.rs`（`llm` 组件子模块，挂进
  `LlmRouteHandler` 的 `routes()`/`handle()`，与实例/环境共用 llm.db 连接）
- 前端：「模型管理」页（LlmModels.vue）「外部 API」Tab（登记卡 / 连接测试面板 /
  精简聊天窗 SSE 流式）
- 关联：`docs/LLM_INSTANCES.md`（本地 vLLM 实例）、`docs/EXTERNAL_LLM_CHANNELS.md`
  （API 网关渠道，见下文边界）、`docs/API_MARKET.md` §10（中继协议与白名单）

## 1. 拓扑

```
浏览器（LlmModels.vue「外部 API」Tab）
  │  REST（同源 /api/v1/llm/external-apis*）
  ▼
osd（os-api，llm 组件）
  │ ① GET  /external-apis            → llm.db 读表（key 脱敏出参）
  │ ② POST /external-apis            → 落表 llm_external_apis
  │ ③ POST /:id/test                → 真实 GET <base_url>/models（带 Bearer）
  │ ④ POST /:id/chat  stream:false  → 整包转发 <base_url>/chat/completions
  │ ⑤ POST /:id/chat  stream:true   → SSE 逐块透传（http.rs 特挂路由，旁路 dispatch）
  ▼
外部 OpenAI 兼容端点（106 节点网关 / SiliconFlow / 自建 vLLM…）
```

**流式旁路**：`stream:true` 的对话请求由 `build_router` 特挂的
`llm_external::chat_stream_handler`（`POST /api/v1/llm/external-apis/{id}/chat`）
直接处理——`ApiRequest/ApiResponse` 是整包 JSON 模型，装不下字节流，所以照抄
API 网关 SSE 特挂（`gateway_openai_handler`）的手法：鉴权（`extract_principal` +
`AuthMiddleware::authorize`，与 dispatch 同口径 admin）→ 查行（base_url/api_key）
→ reqwest `bytes_stream()` 逐块透传 `text/event-stream`。**首字节前**失败（连接
失败 / 非 2xx / 首块 120s 超时）回 JSON 错误；**首字节后**不再回退，上游中断只
断流并末尾补 `: llm-ext:` SSE 注释帧。非流式 / 未装配状态回落
`dispatch_handler`（组件整包路径，行为与不特挂完全一致）。

**共享状态**：`main.rs` 把 `LlmRouteHandler::external_state()` 经
`InProcessGateway::set_llm_external` 注入 `GatewayState`——特挂路由与组件 REST
走**同一条** `Mutex<Connection>`（`api_gateway` 共享模式同款）。

## 2. 数据表（llm.db 同库）

`llm_external_apis(id, name, base_url, api_key, models, status, last_check_at,
notes, created_at, via_node)`；`models` 列存 JSON 数组字符串。建表幂等（llm.rs
`create_schema` 同步建；老库 `ALTER … ADD COLUMN via_node` 幂等迁移），首次开库
即空表——**不 seed 演示条目**（真实数据铁律）。

- `status`：`unknown`（新建未测）/ `ok`（最近一次测试成功）/ `error`（失败），
  只由真实探测翻转；
- `api_key`：明文只存服务端库内，**任何响应都不出明文**（列表/创建只回
  `api_key_masked`：`sk-a***3456` + `has_api_key`）；
- `via_node`（2026-09-02）：来源 NodeID（`0x`+66hex）——联邦大厅一键导入时由
  前端写入（取 api_market 条目的 `source_node_id`，源端验签落列不可伪造）。
  **非空 → chat/test 经 os-p2p overlay 定向该源节点代发（§6）；空 → 直连语义
  不变（存量行即空）**。PUT 可覆盖（空串=清除回直连）；POST/PUT 校验非空值
  须为合法 NodeID（400）；
- 测试成功且 `models` 为空时回填上游清单（`data[].id`）；用户手工登记的清单
  不被覆盖。

## 3. 端点契约（5 条；GET 公开读，写 / test / chat 需 admin）

| method | path | 请求 | 响应 |
|--------|------|------|------|
| GET | `/api/v1/llm/external-apis` | — | `{apis: [{id, name, base_url, api_key_masked, has_api_key, models[], status, last_check_at, notes, via_node, created_at}]}` |
| POST | `/api/v1/llm/external-apis` | `{name, base_url, api_key?, models?: string[], notes?, via_node?}` | `201` 脱敏行；name 空 / base_url 空 / 非 http(s) / via_node 非法 → `400` |
| DELETE | `/api/v1/llm/external-apis/:id` | — | `{ok: true}`；不存在 `404` |
| POST | `/api/v1/llm/external-apis/:id/test` | `{}` | `{ok, models[], latency_ms, error?}`——真实 GET `<base_url>/models`（10s 超时，带 `Authorization: Bearer <key>`；via_node 非空经中继，§6）；失败也是 `200 + ok:false`（探测失败不是 HTTP 错误），状态落行 |
| POST | `/api/v1/llm/external-apis/:id/chat` | `{model, messages[{role,content}], max_tokens?, temperature?, stream?}` | `stream:false` → 上游 JSON **原样透传**（含 usage，不重写不估算）；`stream:true` → SSE 逐块透传（via_node 非空两路径均经中继）。model/messages 空 → `400`；行不存在 → `404`；上游失败 → `502` |

URL 拼接：`<base_url 去尾斜杠>/models`、`/chat/completions`（base_url 填 OpenAI
兼容根地址，**含 /v1**）。组件内非流式路径强制 `stream:false` 转发（防上游按
SSE 推流而整包等待超时）。

## 4. env

| env | 作用 | 缺省 |
|-----|------|------|
| `NEXOS_LLM_DB` | 表所在 SQLite 文件（与实例/环境表同库同链） | `/tank/os-data/llm.db` → `/var/lib/os/llm.db` → `./llm.db` |

本模块无新增 env；上游超时常量（test 10s / chat 整包 120s / 流式首字节 120s /
流式总 600s）为代码常量（`llm_external.rs` 顶部）。

## 5. 与 API 网关渠道（channels）的边界（2026-09-03 起双向打通）

| | 外部 API（本表） | 网关渠道（`/api/v1/gateway/channels`） |
|---|---|---|
| 场景 | **我要用**别家的模型（模型管理页直连对话） | **我要卖**我的模型（One API 式聚合转售） |
| 计费 | 无 | sk-os- 令牌 / 费率 / 配额 / 调用日志 |
| 路由 | 单端点直通 | 多渠道优先级 / 加权 / 故障转移 |
| 消费者 | 模型管理页面（admin） | 任意 OpenAI 兼容客户端 |

两套表仍不耦合（登记行删了不影响已导入的渠道）。**双向打通**（原「本期不做」
的单向导入已落地，2026-09-03）：

- **导入 ← 联邦**：联邦大厅/API 市场一键导入 → 本表登记行（via_node 自动写入，
  §6）；
- **发布 → 网关**：`POST /api/v1/gateway/channels` body 带
  `from_external_api: <登记 id>` → 按行生成一条网关渠道——复制
  name/base_url/api_key/models/via_node（via_node 非空即**中继渠道**：网关
  转发经 overlay 定向源节点代发，非流式/流式语义与计费见
  `docs/GATEWAY_MONETIZATION.md` §6）；models 空则导入时先探回填，探测失败
  不阻塞（响应带 warning）；登记不存在 404。
- 前端入口：外部 API 卡片「发布到网关」按钮；网关「添加渠道 → 从外部 API
  导入」。典型场景：把 P2P 收到的联邦 API 再发布给本局域网的 AI 工具用
  （本节点网关 + sk-os- 令牌，接入面不变）。

## 6. via_node 跨网中继（2026-09-02）

**缺陷（Spark 实测）**：从联邦大厅导入 `qwen3.5-9b@ub2604`（发布于 106，
endpoint=`192.0.2.106:8558` 内网地址）→ 对话报
`上游请求失败: error sending request for url (http://192.0.2.106:8558/...)`。
联邦数据同步走 overlay 没问题，但本模块的 chat/test 是**直连 HTTP**——跨网
节点够不着发布者的内网 endpoint。

**语义**：

- `via_node` 非空 → ③ test / ④⑤ chat（含流式）**经 os-p2p overlay 定向
  `via_node` 源节点**（fed kind `api_relay_req`/`api_relay_resp`，协议与源端
  白名单安全模型见 `docs/API_MARKET.md` §10）：消费者把 HTTP 请求（方法/URL/
  头/体）发给源节点，源节点核白名单（URL 必须命中其已发布条目的
  `{E, E/models, E/chat/completions}` 封闭集合，否则 403「该 URL 不属于本节点
  发布的条目」——绝不做开放代理）后代发，响应/SSE 逐块回传；
- `via_node` 空 → 直连（现状零变化）；
- 超时沿用本模块既有口径：test 10s / chat 整包 120s / 流式首字节 120s /
  流式空闲 60s（中继协议自身的 30s/15s/60s 缺省是 pending 清理口径，消费端
  按语义覆盖）；
- 错误信息与直连可区分：`经 <节点短式 0x1234…cdef> 中继失败：<原因>`（前端
  原样透传展示）；
- 接缝注入：`LlmExternalState::set_relay`（main.rs 在 `api_market_fed` 取出
  后注入——`ApiMarketFedEndpoint` Clone 共享内核；测试注 fake 互连端点端到端，
  照 live.rs Executor 模式）。

**限制**：单跳（出口=源节点，源节点失联无兜底）；流式背压未做（resp 帧经
无界通道）；发布者下架/换 endpoint 后旧 via_node 条目会收到 403（文案即引导）。

## 7. 实例接入说明面板的 `launch_command`（同批交付）

「模型管理 → 实例管理 → 接入说明」面板新增「④ 启动参数」块：完整 vLLM 启动
命令 + 一键复制。数据源为后端透出的**真实值**，前端只渲染不猜格式：

- `GET /instances`、`GET /instances/:id`、`POST /instances`、`/:id/start`、
  `/:id/stop` 的实例 JSON 恒带 `launch_command` 字段：
  - 曾拉起 → 最近一次**真实 argv**（`<venv>/bin/vllm serve <model> …`，含推理
    环境二进制路径；换口重试时同步替换 `--port`），落库列 `launch_command`；
  - 从未拉起 → 服务端按当前 config 用 `build_vllm_serve_cmd`（与 spawn 同函数
    同参，不漂移）构造，`vllm` 占位二进制名。
- 同批「实例参数速览」精简为四项：模型名（served_model_name）/ 上下文窗口
  （max_model_len）/ 端口 / API Key（沿用"有值显示，无则『无需（未启用鉴权）』"
  分支）；删除模型路径 / dtype / gpu_memory_utilization / extra_args 摘要项
  （extra_args 已含于完整启动命令）。
- 前端 `accessLaunchCommand` 在字段缺失（旧后端）时按 config 忠实重建，重建
  规则与 `build_vllm_serve_cmd` 一一对应（见 LlmModels.vue 注释）。

## 8. 测试（不连外网，mock 真 TCP）

`llm_external.rs` 内 20 项单测 + `http.rs` 3 项路由级测试：

- CRUD/脱敏：建列删、`api_key_masked` 断言 + 全响应无明文 key；name/base_url
  校验（空 400 / ftp 400 / 尾斜杠收敛）；
- via_node（§6）：建/改校验（非法 400 / 空串清除 / 未提供保留）+ masked 输出；
  中继未装配 → test/chat 错误含「经 <节点> 中继失败」；fake 互连 overlay 端到端
  （test 真实解析 models + chat usage 原样透传）；流式全链路（块序 + content-type
  透传 + 非 2xx 首 JSON 错误）；
- test 成败路径：本地 `TcpListener` mock 真 `/models`（断言请求行
  `GET /v1/models` + Bearer 头）→ 清单/延迟/状态翻转/空 models 回填；死端口 →
  `ok:false` + 原因 + status=error；
- chat 非流式：mock `/chat/completions` → 上游 body（含 usage）原样透传、
  model 透传、组件路径强制 `stream:false`、Bearer 头；500 → 502；body 校验 400；
- 流式透传：`sse_passthrough_stream` 函数级（块序 + 中断注释帧 + 注释帧后收尾）
  + 路由级（mpsc 压块证明**逐块**而非整包、首块 10s 内逐字节到达、usage/[DONE]
  顺序保持、错误 Bearer 401、不存在 404、非流式回落 dispatch）；
- 持久化：真实临时文件库重开恢复 + id 计数器越过存量最大后缀（含 via_node 列
  幂等迁移）；
- 直连行零回归：via_node 空的既有路径全部原断言不变（中继分支只在非空时接管）。
