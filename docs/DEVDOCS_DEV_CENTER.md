# 开发者中心（DevDocs / devdocs）

> 桌面应用「开发者中心」——开发者功能与文档的常驻门户。用户需求原话：
> 「增加重要功能组件，开发者功能和文档，这是一个不断更新的组件，独立开来，
> 如怎么让应用的图标在桌面显示，怎么安装自己的应用，调用区块链 SDK 等等」。

## 1. 架构：文档唯一事实源 = 仓库 `docs/`

开发者中心是**渲染与服务层**，不含文档本体——文档随仓库演进（受
「功能文档同步铁律」约束），`git push` 即更新（NexHub post-receive 钩子
已自动化）→「不断更新」零额外机制：

```text
仓库 docs/*.md（唯一事实源，随代码演进）
   │ git push（钩子已自动化）
   ▼
os-api devdocs handler（读 NEXOS_DEVDOCS_DIR 目录，缺省 /home/oem/NexOS/docs；
   无该目录的部署节点：配置 NEXOS_DEVDOCS_FALLBACK_URL 后联邦回退——
   从源节点代理拉取 index/doc 原样透传，不再显示空目录；未配置则降级：
   空清单 + 「文档服务在本仓库节点」提示）
   ▼
桌面应用「开发者中心」devdocs（/devdocs，DevDocs.vue：文档目录树
   + Markdown 渲染【marked】+ 搜索 + 文档语言切换【AI 翻译，见 §5】）
```

## 2. 路由表（3 条，component="devdocs"，开发期公开读）

| method | path | 动作 |
|--------|------|------|
| GET | `/api/v1/devdocs/index` | 文档索引：`{docs:[{path,title,category,size,mtime}], categories, source_available, root, note}`（扫描根 + 一级子目录 `*.md`；缓存 30s，目录 mtime 变化立即失效） |
| GET | `/api/v1/devdocs/doc/*path` | 单篇原文 `{path, title, markdown, mtime}`（markdown 原文由前端渲染）；`?lang=en\|zh-TW` 走 AI 翻译管线（见 §5）；`retry=1` 清除翻译失败态重试 |
| GET | `/api/v1/devdocs/translate/tasks/:id` | 翻译任务视图（状态机 + 环形日志，前端轮询用；未知 id 404） |

分类规则：frontmatter `category:` 优先 > 一级子目录名 > `docs`。标题 =
首个 `# ` 行（无则回退文件名）。

**路径安全三闸**：① 仅 `.md`（400）；② canonicalize 后必须仍在文档根内
——`..` 穿越 / 符号链接出根一律 403（URL 百分号编码先解码再过闸）；
③ 不存在 404。

**降级与联邦回退（已实现）**：文档根全部不存在时，若配置了
`NEXOS_DEVDOCS_FALLBACK_URL`（113/aliyun 指向 `http://192.0.2.106:8558`），
则从源节点**联邦回退**代理拉取，不再显示空目录——

- `index`：GET `{fallback}/api/v1/devdocs/index`（10s 超时）原样透传 JSON，
  仅 `note` 覆写为「联邦文档分发：<源节点>」；透传结果缓存 30s（同本地），
  拉取失败落回本地降级（空清单 + 提示），失败不缓存（下次重试）；
- `doc`：GET `{fallback}/api/v1/devdocs/doc/<rel>` 状态码与 JSON 原样透传
  （不缓存；防穿越主责在源节点，本地仍先拒含 `..` 的路径——明文与
  百分号编码形态一致）；源不可达 → 503 + 说明。`?lang=` 一并透传源节点
  （译文由源节点缓存/翻译；源节点 202 的任务 id 不在本节点任务表——前端
  对任务 404 回退为定时重取文档）。

## 3. 环境变量

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_DEVDOCS_DIR` | `/home/oem/NexOS/docs` | 文档根目录（106 主节点 checkout）。目录不存在时回退二进制旁 `./docs`、`../../docs`（workspace target/ 运行形态）；全部不存在进入降级/联邦回退模式（index 200 空清单 + note 或联邦透传；doc 503 或透传） |
| `NEXOS_DEVDOCS_FALLBACK_URL` | 未设 | 联邦回退源节点 base URL（仅本地无文档根时生效；如 113/aliyun 的 systemd env 设 `http://192.0.2.106:8558` 即启用，未设保持纯降级） |
| `NEXOS_DEVDOCS_I18N_DIR` | `/tank/os-data/devdocs-i18n` | AI 译文缓存根（`<dir>/<lang>/<相对路径>`）。构造期 `create_dir_all` 失败 → 翻译停用（lang 请求 503 说明），原文不受影响 |
| `NEXOS_DEVDOCS_GATEWAY_URL` | `http://127.0.0.1:8558` | 翻译走的本节点 API 网关 base URL（chat/completions 挂在 `/api/v1/gateway/v1/` 下） |
| `NEXOS_DEVDOCS_GATEWAY_TOKEN` | 未设 | 翻译的网关服务端凭据（**sk-os- 网关令牌**，构造期定格）；未设回落 `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`——注意网关 chat 鉴权查令牌表，admin 回落通道仅在该值同时注册为网关令牌时可用 |
| `NEXOS_DEVDOCS_TRANSLATE_MODEL` | `qwen3.5-9b` | 网关渠道对外模型名（缺省取 106 现网渠道 ch-101「Qwen3.5-9B」的模型名；无渠道支持该模型 → 503 降级） |

## 4. 前端（web/src/views/DevDocs.vue）

- 顶部说明条：事实源声明（「docs/ 唯一事实源，git push 即更新」）+ 实际
  服务目录 + **文档语言切换**（中文原文 / English / 繁體中文，分段按钮，
  选择持久化 `localStorage('devdocs.docLang')`）；文档头展示源路径 +
  mtime（随仓库更新）+ 非中文时的「AI 译文」徽标；
- 左侧目录树（分类分组 + 搜索：标题/路径/分类过滤）+ 右侧 Markdown 渲染；
- 非中文且未命中：内容区显示「AI 翻译生成中 · 块 i/N」进度（2s 轮询任务
  端点，附最近一行任务日志）；done 自动重取渲染；error/503 显示服务端
  降级文案 + 「重试翻译」（`retry=1`）/「中文原文」按钮；
- 渲染选型：**marked**（GFM，同步渲染）——既有依赖无 markdown 库也无
  dompurify，文档源是自家仓库可信内容（非任意用户输入），v-html 直插，
  不新增 DOMPurify；代码块深底等宽（复用 CodeHub 接入说明 ob-pre 风格）；
- chrome 文案走 vue-i18n 四语言（`devdocs.*` 30 键 × zh-CN/zh-TW/en-US/ja-JP）；
  **文档语言是内容维度**，独立于界面语言；
- 注册：appRegistry（key `devdocs`，分类 devtools）+ router `/devdocs`
  + DashboardView 图标（翻开的书 + 尖括号）+ AppIcon.vue。

## 5. AI 翻译管线（本地 LLM，NexOS 吃自己的狗粮）

文档全中文；目标语言 v1：`en` + `zh-TW`（缺省或 `lang=zh` 原文直读零开销）。
**不直连 vLLM、不改 llm.rs**——翻译调用走本节点 API 网关的
`POST /api/v1/gateway/v1/chat/completions`（服务端凭据，构造期定格 env），
网关的渠道选择 / 故障转移 / 计费 / 日志全复用：

```text
GET /api/v1/devdocs/doc/x.md?lang=en
   │ ① 缓存 <I18N_DIR>/en/x.md 命中且未过期
   │    → 200 译文 + 响应头 X-Translation: cached（即时切换零等待）
   ▼ ② miss（或原文已更新）→ 异步翻译任务
   │    首次请求返回 202 + 任务视图（id/lang/chunks_total/环形日志）
   │    同文同语言 running 期间复用同一任务（不重复翻译）
   ▼ ③ 后台逐块调网关 → 完成 tmp+rename 原子写缓存
GET /api/v1/devdocs/translate/tasks/:id 轮询（done/error/chunks_done）
   → 完成后再取 ?lang= 即 200 译文
```

### 5.1 分块（16K 上下文约束）

- 按**二级标题**（`## ` 行首）分节，每块 ≤6K 字符；
- 超长节再按空行段落累积切（单段落仍超长按行硬切）；
- **fence 感知**：代码围栏（``` / ~~~）内的 `##` 与空行不作为切分边界
  （代码块永不拦腰切开）；
- frontmatter（`---` 元数据块）**不翻译**原样回接——`category:` 是索引
  契约，翻译会破坏分类。

### 5.2 prompt 契约（技术文档翻译）

系统提示词要求：只输出译文；代码块/行内代码/命令/URL/文件路径原样保留；
mermaid / ASCII 图 / 表格结构与对齐不变；Markdown 结构（标题层级/列表/
引用/链接目标）不变；**术语表不译**（v1 收录 35 词：NodeID / OverlayAddr /
overlay / ZFS / vLLM / NexOS / NexHub / os-api / axum / Vue / Rust / cargo /
JWT / API / SDK / SSE / WebSocket / systemd / Markdown / frontmatter /
post-receive / GLM / Qwen / USDT / EVM / secp256k1 / keccak 等——增补改
`GLOSSARY` 常量即可）；忠实全文不增删。模型偶尔包裹整回复的外层代码围栏
会被剥离（仅剥裸/markdown 标记围栏且内部围栏配平时，防误剥业务代码块）。

### 5.3 思考模型适配（Qwen3.5 禁思考）+ 输出预算

真机首验（2026-09-03，106：网关渠道 ch-101 `Qwen3.5-9B` → vLLM
`http://127.0.0.1:8123/v1`）踩坑：思考模型把翻译输出全放进思考段、
`content` 为 null——网关 60s 超时（前两发 502）已由网关代理修 300s，
思考占用由以下三层修复（**真机 curl 验证结论**）：

| 层 | 机制 | 106 真机验证结论 |
|----|------|------------------|
| 主开关 | 请求体 `chat_template_kwargs: {"enable_thinking": false}`（vLLM Qwen3 官方开关，网关 body 原样透传） | **生效**：content 直出（"Developer Center"，completion_tokens=3），无 reasoning |
| 软开关 | user 内容尾追加 `/no_think`（Qwen3 软开关） | **该后端无效**：max_tokens=256 仍被思考吃光（content=null、finish=length）——保留作换非 vLLM 后端时的降级重试通道 |
| 判据兜底 | content 为空且（reasoning_content/reasoning 非空 **或** finish=length）→ 判「思考段占用」→ /no_think 重试一次 → 仍空才落 error | 106 形态为 content=null + reasoning_content **不回传** + finish=length——判据必须含 finish=length |

- **输出预算（动态）**：`max_tokens = 输入字符数 / 2 + 2048`（输入 = 系统
  提示词 + 分块；中文→英/繁 token 量约为字符一半 + 裕量）。静态小值会被
  思考段或长译文吃穿（finish=length 截断）。
- **单块超时 300s**：对齐网关代理转发上限（2026-09-03 网关 60s→300s）——
  客户端 ≥ 网关，超时先到的是网关的明确错误而非本地掐断。
- 思考占用重试的动作与终态文案均进任务日志/错误（前端可见
  「⚠ 输出被思考段占用——/no_think 软开关重试一次」；做尽则
  「模型思考段占用输出预算（已用 chat_template_kwargs 禁思考并 /no_think
  重试一次仍空）」）。

### 5.4 缓存与失效

- 缓存路径 `<NEXOS_DEVDOCS_I18N_DIR>/<lang>/<相对路径>`（相对路径取
  canonicalize 后剥文档根前缀——`rel` 原串拼缓存路径有越界风险）；
- **失效（v1 简化）**：原文 mtime 新于译文 mtime → 缓存判 miss 直接重译，
  **旧译不返回**（文档注明；不做「返回旧译 + 过期提示」）；
- 写入 tmp + rename 原子化：翻译途中崩溃不会留半截译文被命中。

### 5.5 任务语义（内存表，agenthub_toolchain 同款手法）

- 任务态进程内（重启即清，译文在磁盘缓存上）：环形日志 200 行，表容量
  128（超出丢最旧已完结任务），并发上限 4 篇（超出 503「并发已满」）；
- running 超过 45min 惰性转 error（后台线程异常死亡的自愈）；
- 同 (lang, path) 有 running 任务时再次请求复用（202 同任务 id）；
- 上一任务 error 后：GET ?lang → 503 + 错误文案（附任务视图）；
  `?retry=1` 清除失败态重新翻译。

### 5.6 诚实降级（不假翻译）

- 网关 404（无可用渠道支持该模型）/ 502（所有渠道转发失败）/ 503 →
  任务 error，文案：**「本节点无可用本地模型，暂无法生成 <English|繁體中文>
  翻译（中文原文可用）」**（附网关原始细节）；后续 GET ?lang → 同文案 503；
- 未配置网关凭据 → 503 + env 配置指引（`NEXOS_DEVDOCS_GATEWAY_TOKEN`）；
- 凭据无效（网关 401）→ 任务 error 指明注册网关令牌的路径；
- **中文原文任何时刻不受影响**（?lang 请求失败 ≠ 原文不可读）。

## 6. 初始文档集（docs/dev/，随本组件交付）

8 篇开发者指南（全部来自真实代码勘察）：01 应用开发 / 02 安装自己的应用 /
03 区块链 SDK（python 低 S + 压缩公钥两坑）/ 04 IM agent / 05 NexHub 协作 /
06 os-api handler 开发 / 07 多节点部署 / README 索引（如何贡献：放
docs/ 即自动出现在开发者中心）。

## 7. 测试

`cargo test -p os-api --lib devdocs`（24 个）：索引扫描（子目录分类/
frontmatter/标题回退）、缓存（30s 复用 + mtime 失效重扫）、原文读取、
穿越/非 md/编码穿越拒绝、降级模式、联邦回退（index 代理透传 + note 覆写
+ 30s 缓存复用、源不可达落降级、doc 透传形状、`..` 本地先拒、源不可达
503、`?lang=` 透传源节点）、路由与公开读约定、percent-decode 边界；AI 翻译
（**mock 网关 = 真 TCP，不跑真实翻译**——质量由 106 实测验收）：分块器
（二级标题/上限/fence 感知/frontmatter 不译）、外层围栏剥离边界、lang=zh
零开销、非法 lang 400、全链路（202 → 轮询 done → 缓存命中 X-Translation:
cached + 凭据/模型/分块内容逐请求断言 + zh-TW 独立缓存槽）、running 去重
（不重复翻译）、原文 mtime 失效重译、无渠道 503 诚实降级 + retry=1 恢复、
任务 404 / 无凭据 503、思考占用 /no_think 软开关重试成功、思考占用重试做尽终态文案区分。

## 参考

- 源码：`crates/os-api/src/handlers/devdocs.rs`（装配：main.rs `register_component("devdocs",…)`）
- 客户端：`web/src/api/client.ts` 的 `devdocsIndex` / `devdocsDoc` /
  `devdocsDocLang` / `devdocsTranslateTask`
- 指南集：[dev/README.md](dev/README.md)
