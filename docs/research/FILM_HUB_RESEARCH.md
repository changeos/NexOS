# FilmHub + 画布：AI 原生的影片创作平台——架构方案书（2026-09-05 调研）

> 面向零上下文读者：读完本文即可理解「影片制作升级为FilmHub + 画布双形态大项目」
> 的完整方案。本文为**纯调研产物**（只读仓库源码 + 业界参照检索），唯一写操作
> 即本文本身；不含任何已实施的代码改动。
>
> 背景契约：`docs/FILM_STUDIO.md`（film 引擎 21 端点现状）、
> `docs/NEXHUB.md` + `docs/NEXHUB_ONBOARDING.md`（代码枢纽协议）、
> `docs/APPS.md`（应用包规范/引擎门控）、
> `docs/research/FILM_CONSISTENCY_RESEARCH.md`（角色一致性 P0-P2，已落地其 P0）。
> 关键源码：`crates/os-api/src/handlers/film.rs`（7172 行）、
> `crates/os-api/src/http.rs`（git Smart HTTP + push→CI 钩子）、
> `crates/os-api/src/handlers/nexhub_ci.rs`（内置 CI）、
> `crates/os-nexhub/src/code_repo.rs`（repos CRUD/contents/sessions）、
> `apps/film/src/FilmStudio.vue`（2914 行五区前端）。

---

## 0. 一页结论

**能不能搞：能，而且绝大部分基建已经存在。**「AI 像在 NexHub 写 md 一样写剧本」
所需的 git 托管 / Smart HTTP push / push 自动 CI / CLI / 浏览端点 / 联邦大厅，
NexHub 全部现成；film 引擎的六阶段管线、角色库、产物布局也已稳定。缺的只是
三样：**① 一个 agent 友好的项目文件规范（md+json 混合）**、**② hub⇄画布的
同步语义（文件 ⇄ 编辑态 ⇄ 生成产物）**、**③ 成本记账数据层**。

**怎么搞最省：不新造系统，把「项目」升级为「NexHub 上的一个 git 仓」。**
项目文件（剧本/分镜/角色卡/账本）进仓——AI agent 用与写代码完全相同的工作流
（clone/编辑/push/看 CI）；重产物（mp4/png/mp3）不进 git，留在产物目录，以
`artifacts.json` 清单（sha256/bytes/task_id）进仓。画布（编辑页）保留现有
时间轴/监视器/previewEngine，新增自由分镜卡画布作为创作主面。

**分期铁律：P0 只做「文件是真值 + 成本能记账」这一件小事**（项目文件规范 +
导入导出 + 成本事件层，约 5-8 人日），git 仓化（P1）、画布重构（P2）、
CI 校验与大厅（P3）依次展开，每期独立可用、可回滚。

---

# A. 术语与产品形态

## A.1 业界术语参照（检索核实，2026-09-05）

| 软件 | 项目组织术语 | 编辑主界面术语 | 备注 |
|------|-------------|---------------|------|
| DaVinci Resolve | Project > **Media Pool** > **Bin**（箱）> Clip；跨项目共享 Power Bin | 按**页**切换：Edit（剪辑页）/ Fusion（**节点图**）/ Color / Fairlight | 混合形态：轨道时间轴剪辑 + 节点图合成（Fusion），两者服务不同工序 |
| Premiere Pro | Project 面板 + **Bin**；**Sequence**（序列）= 时间轴容器 | Sequence 编辑器（时间轴优先） | 「sequence 就是你放时间轴的容器」（Vyond 术语表） |
| Avid / FCP | Bin / Event-Project 两级 | Timeline | NLE 传统：Bin=媒体文件夹 |
| 剪映（CapCut） | **草稿**（Draft） | 剪辑页：**画布**（注意：剪映的「画布」指**预览画面区域**及其比例/背景设置）+ 时间轴 + 素材面板 | 国产语境里「画布」常与预览区绑定 |
| Figma / Miro / 节点式工具 | File / Board | **Canvas**（自由画布：卡片自由排布/连线/缩放） | 「画布」在创作工具语境=自由面，不与时间轴绑定 |

关键判断：

1. **时间轴（Timeline）与节点图（Node Graph）是工序之别，不是替代关系**——
   DaVinci 同时保留两者（Edit 页时间轴 + Fusion 页节点图）。我们的「镜头 →
   生成任务 → 产物」本来就是 DAG，节点图是天然的远期视图，但 v1 不必做。
2. **「画布」在 NLE 语境（剪映）与自由创作面语境（Figma）语义不同**。用户
   愿景里的「画布」是「像 Figma 一样摆分镜卡、点一键生成」的自由面——本文
   按此义采用，并把预览区明确叫「监视器 Monitor」（现有 PreviewMonitor 已是
   剪映/PR Program Monitor 风格），避免撞词。
3. AI-native 先例（Saga / Higgsfield / Invideo Agent / 多 agent 剧组流水线）
   均为**封闭 SaaS**：剧本→分镜→生成在站内黑盒完成。**没有发现「项目=git 仓、
   agent 用代码工作流写剧本」的先例**——这正是 NexOS 的差异化定位
   （参考 Saga/Higgsfield 定位描述、Medium 多 agent 制片管线拆解，见文末来源）。

## A.2 术语建议表（推荐 + 四语言）

| 概念 | 推荐（中/英） | zh-CN | zh-TW | en-US | ja-JP | 说明 |
|------|--------------|-------|-------|-------|-------|------|
| 项目文件中心（类 NexHub） | **影片枢纽 / FilmHub** | 影片枢纽 | 影片樞紐 | FilmHub | フィルムハブ | 与 NexHub 命名同构；对 agent 即「一个 git 仓」 |
| 编辑主界面（整体页面） | **剪辑室 / Studio** | 剪辑室 | 剪輯室 | Studio | スタジオ | 应用内两态之一（列表态=枢纽态的项目卡入口） |
| 自由分镜创作面（主创作区） | **画布 / Canvas** | 画布 | 畫布 | Canvas | キャンバス | 分镜卡自由排布 + 生成动作聚合；Figma 语义 |
| 装配/播放视图 | **时间轴 / Timeline** | 时间轴 | 時間軸 | Timeline | タイムライン | 现有 TimelineTracks 四轨保留 |
| 预览区 | **监视器 / Monitor** | 监视器 | 監視器 | Monitor | モニター | 现有 PreviewMonitor 保留命名 |
| 分镜卡 | **镜头卡 / Shot Card** | 镜头卡 | 鏡頭卡 | Shot Card | ショットカード | 画布上的原子单位=一个镜头 |
| 角色档案 | **角色卡 / Character Card** | 角色卡 | 角色卡 | Character Card | キャラクターカード | `characters/<name>.md` |
| 成本区域 | **成本账本 / Budget** | 成本账本 | 成本帳本 | Budget | 予算台帳 | `budget.json` + 面板 |
| 大厅（拓展） | **大厅 / Lobby** | 大厅 | 大廳 | Lobby | ロビー | 复用 NexHub 大厅语义 |
| 远期高级视图（预留词） | 节点图 / Node Graph | 节点图 | 節點圖 | Node Graph | ノードグラフ | **v1 不用**；生成 DAG 可视化的预留名 |

> 四语言与 film 应用现有 i18n 目录（`apps/film/src/i18n/{zh-CN,zh-TW,en-US,ja-JP}.json`）
> 一一对应，新增键直接落这四个文件。

**编辑主界面叫什么（推荐）**：整页叫「剪辑室 Studio」（对应剪映「剪辑页」/
DaVinci「Edit 页」），其中主创作区叫「画布 Canvas」——满足用户原话的「画布」，
又不与时间轴/监视器抢词。**hub 叫什么（推荐）**：「影片枢纽 FilmHub」——
与 NexHub（代码枢纽）形成产品家族命名。

## A.3 双形态产品结构图

```text
┌──────────────────────────────────────────────────────────────────────────┐
│                          FilmHub（影片枢纽，项目文件中心）                  │
│  一个项目 = NexHub 一个 git 仓（film-<slug>.git，/tank/git-repos/）        │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │ project.md │ story.md │ storyboard.json │ characters/*.md           │  │
│  │ refs/ │ assets/ │ budget.json │ artifacts.json（重产物清单）        │  │
│  └────────────────────────────────────────────────────────────────────┘  │
│      ▲ 读/写（AI agent：clone→改→push；人：网页浏览/批注）                 │
│      │ Smart Git HTTP（匿名读/token 写）+ REST 文件端点 + nexhub CLI      │
│      │ push ──▶ 内置 CI（schema 校验，P3）                                │
│      ▼ 同步语义（导入 import / 回写 commit / 冲突检测，§C.3）             │
┌──────────────────────────────────────────────────────────────────────────┐
│                     剪辑室 Studio（可视化编辑 + 生成执行）                  │
│  ┌── 画布 Canvas（自由分镜卡，主创作面）──┐ ┌─ 监视器 Monitor ─┐          │
│  │ [镜头卡1]──[镜头卡2]──[镜头卡3] …      │ │  PreviewMonitor   │          │
│  │  卡=资产状态+生成动作聚合+成本角标      │ │  （现有复用）      │          │
│  └───────────────────────────────────────┘ └───────────────────┘          │
│  ┌─ 时间轴 Timeline（现有四轨+播放头，装配/预览视图）────────────────┐    │
│  └──────────────────────────────────────────────────────────────────┘    │
│  生成执行：六阶段管线（script/image/video/tts/music/compose）             │
│      ▼ 任务完成 → 成本事件（§D）+ 产物落产物目录 + artifacts.json 回写     │
└──────────────────────────────────────────────────────────────────────────┘
      ▼
┌──────────────────────────┐   ┌──────────────────────────────────────┐
│ 成品：final.mp4（export_dir）│   │ 资产：定妆图/参考图/分镜图（可复用）    │
└──────────────────────────┘   └──────────────────────────────────────┘
      ▼（P3 预留）
┌──────────────────────────────────────────────────────────────────────────┐
│ 大厅 Lobby：项目模板 / 成品片段的联邦分享（manifest 式描述符，§E）          │
└──────────────────────────────────────────────────────────────────────────┘
```

---

# B. FilmHub 架构（核心）

## B.1 项目文件规范（md + json 混合，AI 友好）

### B.1.1 目录树草案

```text
film-<slug>/                      # = NexHub 裸仓 film-<slug>.git 的工作树
├── project.md                    # 项目身份证（人读为主 + frontmatter 机读）
├── story.md                      # 剧本（人读；agent 写、人批注）
├── storyboard.json               # 分镜（机读真值；与后端 ScriptShot 对齐）
├── characters/
│   ├── 小明.md                    # 角色卡（一角色一文件，文件名=角色名）
│   └── 小红.md
├── refs/                         # 参考图（png/jpeg/webp，单张 ≤10MB）
│   └── style-ref-01.png
├── assets/                       # 小体积生成资产（定妆图等；重产物不进仓，见 B.1.3）
│   └── portraits/小明.png
├── budget.json                   # 成本账本（事件追加式，§D）
├── artifacts.json                # 重产物清单（生成器写：stage/bytes/sha256/task_id）
└── .gitignore                    # 忽略重产物与本地态（由建仓模板写入）
```

### B.1.2 逐文件：为什么 agent 好写

| 文件 | 内容形态 | 为什么 agent 好写 |
|------|---------|------------------|
| `project.md` | YAML frontmatter（title/idea/ratio/style_hint/status/export_dir）+ 正文自由叙述（创意阐述/世界观/美术设定） | frontmatter 三五行键值=参数面（LLM 结构化输出最稳的形态）；正文长文=流式生成天然形态。人与 agent 各取所需，git diff 一目了然 |
| `story.md` | 纯 markdown 剧本（场次/对白/舞台指示） | **纯自然语言**，agent 写作零 schema 负担；人可直接改。约定为「人读派生物」，机读真值在 storyboard.json——单一真源原则防双写漂移 |
| `storyboard.json` | `{"shots":[ScriptShot…], "generated_by", "created_at"}`——**字段与 `film.rs` 现有 `ScriptShot` 逐一对齐**（shot/desc/image_prompt/video_prompt/line/duration_secs/characters[]），零翻译层 | LLM 输出 JSON 已有全套解析容错（围栏/切片/字段钳制/重试，FILM_STUDIO.md §4）；agent diff 一个镜头=几行 JSON，评审成本最低。**这就是「像写代码一样」的落点** |
| `characters/<name>.md` | frontmatter（voice/portrait_ref/updated）+ 正文外形描述 | **一角色一文件**：改一个角色不碰其它（小 diff、无合并冲突）；文件名即身份，与现 `film_characters.name` 唯一约束同构；md 正文承载长描述不被 JSON 转义折磨 |
| `refs/`、`assets/` | 二进制图片 | agent 上传走既有 b64 端点后由服务端落盘并 add；体积小（≤10MB 闸门现有）可进 git，天然随仓联邦分发 |
| `budget.json` | `{"events":[…], "budget_limit": null}` 追加式账本 | 只追加不改写（append-only），agent/服务端都无合并冲突；缺失字段可重建（events 在 film.db 有真值，§D） |
| `artifacts.json` | 重产物索引：`[{stage, shot?, name, bytes, sha256, task_id, created_at}]` | 重产物（shot-N.mp4 等数百 MB）**不进 git**；清单进 git 让仓保持「clone 秒下」；sha256 让产物可校验、可被联邦大厅引用（§E） |
| `.gitignore` | 忽略 `artifacts/`（重产物）、`final.mp4`、`compose-*` 等中间产物 | 由服务端建仓时写模板，agent 不用管 |

### B.1.3 重产物不进 git（关键决策）

- git 仓（Smart HTTP body 上限 1 GiB，`http.rs` `GIT_HTTP_MAX_BODY`）放文本 +
  小图完全没压力；放 mp4 会让仓无限膨胀、clone 变慢、CI clone 超时。
- 方案：**仓 = 意图真值（剧本/分镜/角色/账本/清单）；产物目录
  （`NEXOS_FILM_DIR/<id>/`）= 字节真值**。两者由 `artifacts.json` 桥接
  （name → sha256 → 产物目录绝对路径可由服务端换算）。
- 远期若要产物联邦分发：走 os-storage 共享 / 大厅购买下载（复用现有面），
  不走 git pack。

## B.2 存储与版本：三方案比较

| 维度 | ① 项目=独立 git 仓（NexHub）【推荐】 | ② 目录 + 快照 | ③ film.db 扩展（现状延伸） |
|------|-----------------------------------|---------------|--------------------------|
| AI agent 工作流 | **完整**：clone/branch/commit/push/PR/看 CI——「像写代码一样写剧本」字面成立 | 文件可读写，但无历史、无版本、无协作语义 | DB 不是 agent 面：无法 clone/diff，须走 REST 逐字段 |
| 版本/历史 | git 原生（谁改了哪个镜头、何时、为什么） | 快照文件（粗粒度、体积膨胀） | updated_at 列（无内容历史） |
| 复用红利 | **全拿**：Smart HTTP（读匿名/写 token）、push→CI 自动触发（`nexhub_ci::push_hook`）、nexhub CLI、contents/file/commits 浏览端点、大厅联邦/付费下载、Issues/PR 协作 | 几乎零复用 | 与现状同（无新增红利） |
| 服务端实现量 | 中：项目目录初始化为 clone + 同步端点（git CLI 子进程，与 code_repo 同手法） | 低 | 低 |
| 仓规模/性能 | 每项目一仓，`/tank/git-repos/` 平铺；`code_repo` 列表是目录扫描——**数百仓实测无压力；>1000 仓需前缀分桶（film/ 子目录），列为本期不做+开放问题** | 无此问题 | 无此问题 |
| 迁移成本 | 存量 `film-101` 项目：**惰性迁移**（首次 export 时建仓回填，不搬 film.db） | 无 | 无 |
| 主要风险 | 大二进制误 push（需体积闸门）；git 依赖（节点本就依赖 git，NexHub 已跑通） | 双源漂移、丢全部协作面 | 语义与 NexHub 割裂，愿景落空 |

**推荐①，附三条护栏**：

1. **仓命名约定 `film-<slug>`**：应用商店扫描的是 `nexos-app-*` 前缀
   （APPS.md §6），`film-*` 不冲突；大厅发布时可按前缀分类。
2. **push 体积闸门**：CI/同步端点校验单文件 >20MB 拒收（文案引导走
   refs/assets 上传端点或产物目录）。
3. **项目目录 = 非裸 clone**：`NEXOS_FILM_DIR/<id>/` 直接 `git clone
   /tank/git-repos/film-<slug>.git` 初始化——生成器写产物、回写文件、
   commit 都是本地 `git -C <dir>` 子进程（与 code_repo spawn git 同手法），
   不需要走 HTTP；外部 agent 的 push 落在裸仓，服务端 `pull --ff-only` 进项目目录。

> ②/③ 何时仍可选：若坚定不引入 git 依赖（极简部署形态），② 可作降级档——
> 但「AI 像写代码一样用」的产品语义即宣告放弃，不建议。

## B.3 AI agent 工作流走查（像写代码一样，逐步骤）

以「agent 接到一句话创意，产出分镜，人审后一键生成」为例：

| 步骤 | 角色 | 动作 | 触发方式与工具 |
|------|------|------|---------------|
| 1 | agent | 建项目：`nexhub repo create film-sunset`（REST `POST /api/v1/coderepo/repos`）或 `POST /api/v1/film/projects`（服务端代建仓，P1） | CLI / REST |
| 2 | agent | `git clone http://node:8558/git/film-sunset.git`（匿名读） | git（Smart HTTP，现有） |
| 3 | agent | 写 `project.md`（创意/比例/风格）+ `story.md`（剧本）+ `characters/小明.md`（角色卡）——纯文件写作，无 API 心智负担 | 任意编辑器/agent 工具 |
| 4 | agent | 写/改 `storyboard.json`（分镜：镜头/提示词/角色绑定/voice 引用/时长）——schema 与后端 ScriptShot 对齐 | 同上 |
| 5 | agent | `git add -A && git commit -m "分镜 v1：8 镜头" && git push`（写需 token：Basic 密码=token） | git（现有） |
| 6 | 系统 | **push 成功自动触发 CI**（`http.rs` CGI 200 → `push_hook`，现有机制）→ CI 探测到 `storyboard.json` 跑 **film 校验流水线**（P3：schema 校验 + 角色名存在性 + 预算上限检查；探测不到流水线记 skipped——现有 detect_pipeline 加一个 film 分支） | 自动（旁路，不阻塞 push） |
| 7 | 人 | 打开剪辑室 → 画布顶部黄条「hub 有新提交（abc1234）」→ 点**同步导入**：文件→画布状态（ storyboard.json→镜头卡、角色卡→角色区、refs→参考图） | Canvas UI（P1/P2） |
| 8 | 人 | 逐镜头/批量点生成（六阶段任务照旧走 `/api/v1/film/*`）；镜头卡实时显示任务态与**成本角标** | Canvas UI（现有任务面） |
| 9 | 系统 | 任务完成：产物落产物目录 + **成本事件落库**（§D）+ `artifacts.json` 追加 + **服务端自动 commit**（`[film] artifacts: shot-3.mp4 (task-42)`，仅清单文件，可 `NEXOS_FILM_AUTO_COMMIT=0` 关） | 服务端（P1） |
| 10 | agent | `git pull` 看到 artifacts.json 变化 / CI 绿灯 / budget.json 成本——形成「人机同一真源」闭环 | git（现有） |

### CLI 命令面扩展草案（nexhub 增 `film` 子命令组，不另造 CLI）

```text
nexhub film list [--json]                 # 列 film-* 仓（映射 film_projects：仓有↔项目在）
nexhub film new <slug> [--title --ratio]  # 建仓+写 project.md 骨架+首 commit（=步骤1-3骨架）
nexhub film pull <slug>                   # git pull 同义（糖：校验 + 提示冲突）
nexhub film push <slug> -m <msg>          # git add/commit/push 同义（糖：体积闸门预检）
nexhub film validate <slug>               # 本地/远端跑 schema 校验（与 CI 同内核）
nexhub film cost <slug> [--by stage|channel|day]   # 读 budget.json/成本端点聚合打印
```

实现形态：`nexhub-cli.sh`（POSIX sh，`include_str!` 随二进制分发）加一段
子命令分派，全部映射既有/新增 REST 端点——**零新二进制**。备选「独立 film
CLI」不推荐：凭据/自更新/依赖面全部重复。

---

# C. Canvas 重构（剪辑室）

## C.1 现有五区盘点：保留项

| 现有组件 | 处置 | 理由 |
|---------|------|------|
| `TimelineTracks.vue`（四轨+播放头，182px→28px 折叠） | **保留**，降为「装配/预览视图」 | 装配语义（时长/对齐/字幕轨）时间轴仍是最优表达；DaVinci 混合形态先例 |
| `PreviewMonitor.vue`（Program Monitor 风格） | **保留** | 监视器是 NLE 刚需；previewEngine provide/inject 共享播放不动 |
| `previewEngine.ts`（710 行播放引擎） | **保留零改** | 与画布无耦合，纯播放面 |
| 底部任务条（任务轮询 2s） | **保留**，信息上移汇聚到镜头卡角标 | 任务可视化从「全局条」变「卡上即时态」 |
| 左侧镜头卡纵列（24%） | **升级为画布**（见 C.2） | 用户主诉求：从纵向列表到自由画布 |
| 中部镜头面板 40%（含角色区） | 保留为「选中镜头的检视面板」（Inspector） | 画布选卡 → 面板编辑详情，Figma/节点工具通例 |

## C.2 新画布形态：布局草案

推荐「分镜卡自由画布」而非纯纵向列表升级——镜头卡=**资产状态 + 生成动作 +
成本**三合一聚合：

```text
┌─ 剪辑室 Studio ────────────────────────────────────────────────────────────┐
│ ←返回 │ 片名 · 16:9 · producing │ [同步 hub: abc1234 ▼] [回写 commit] [成本 ¥12.4] │
├───────────────────────────────────────────┬───────────────────────────────┤
│  画布 Canvas（主创作面，自由排布/缩放）        │  监视器 Monitor               │
│                                           │  ┌───────────────────────┐   │
│  ┌────────┐   ┌────────┐   ┌────────┐    │  │                       │   │
│  │ 镜头 1  │──▶│ 镜头 2  │──▶│ 镜头 3  │    │  │   （预览播放，现有）     │   │
│  │ [缩略图] │   │ [缩略图] │   │ [◻待生成]│   │  │                       │   │
│  │ ✓图✓视频 │   │ ✓图⏵视频 │   │ ¥0.0    │   │  └───────────────────────┘   │
│  │ ✓音 ¥2.1│   │ ¥3.4    │   │         │   │  检视面板 Inspector           │
│  │ [重生成▾]│   │ [生成视频]│   │ [生图]   │   │  ┌───────────────────────┐   │
│  └────────┘   └────────┘   └────────┘    │  │ desc / image_prompt      │   │
│       │            (自由拖拽/框选/框选批量    │  │ video_prompt / line      │   │
│       ▼             生成/连线=镜头顺序)      │  │ 角色 chips / 时长 / voice │   │
│  ┌────────┐  ⊕ 新增镜头卡                     │  └───────────────────────┘   │
│  │ 镜头 4  │                                  │  角色卡列（现有角色区迁入）     │
│  └────────┘                                  │  [小明👩‍🦰 voice:alloy] [+]    │
├─────────────────────────────────────────────┴───────────────────────────────┤
│ 时间轴 Timeline（四轨+播放头，可折叠 182px→28px；点击镜头卡↔播放头联动）        │
├─────────────────────────────────────────────────────────────────────────────┤
│ 任务条（紧凑）：[video task-42 ⏳ 37s] … ────────── [合成 final.mp4] [导出]     │
└─────────────────────────────────────────────────────────────────────────────┘
```

镜头卡信息架构（=现有 ShotPatch 字段 + 资产状态 + 成本的聚合）：

- **头**：镜头号 + 时长 + 出场角色 chips（点击跳角色卡）。
- **身**：关键帧缩略图（有 shot-N.png 即显）；无则显示 image_prompt 首行。
- **资产状态行**：图/视频/音频三格 ✓（done，点击监视器播放）/ ⏳（running，
  轮询复用现有 2s 面）/ ◻（未生成）/ ✗（error，点击看任务日志尾）。
- **动作聚合**：`[生图] [生视频] [配音]`（按缺什么亮什么）+ `[重生成▾]`
  （选阶段）——把现有逐镜头六阶段按钮收进卡。
- **成本角标**：该镜头累计成本（budget 事件按 shot 聚合，§D）。

v1 简化（防过度设计）：画布先用**自动网格布局 + 顺序连线**（镜头序即连线，
不支持自由拖位改序——改序=检视面板改 shot 号或拖卡换格）。自由拖拽/分支
（多方案对比 A/B roll）列 P3+。缩放（fit/100%/滚轮）做，成本低收益高。

## C.3 hub ⇄ canvas 同步语义

三方数据：**仓（HEAD）⇄ 项目工作目录（clone，可 dirty）⇄ 画布编辑态（内存）**。
v1 从简，明确单向门：

| 操作 | 语义 | v1 策略 |
|------|------|--------|
| **导入（pull）** | 仓 HEAD → 工作目录 → 画布状态 | 打开剪辑室时自动 `git -C <dir> status`；HEAD 干净→直接装载 storyboard.json/角色卡/refs；**HEAD ≠ 上次记录的 `head_commit` → 顶部黄条「hub 有新提交」，人点确认后才 pull**（防 agent push 打断编辑中的会话） |
| **回写（push/commit）** | 画布编辑 → 文件 → commit | 显式动作：`[回写 commit]` 按钮把分镜/角色/设定变更写成文件并 `git -C <dir> add/commit`（本地 clone）；是否 push 裸仓由开关（单机用本地即可，联邦才 push）。message 模板 `[canvas] 分镜编辑：镜头 3 时长 5→8s` |
| **冲突（双向改动）** | 工作目录 dirty 且仓又有新 HEAD | v1 **不做 merge UI**：回写前检测 `git pull --ff-only` 失败 → 提示三选一：「以画布为准（stash+commit 后强制覆盖推送）」「以 hub 为准（丢弃画布改动重导）」「导出 patch 手工合」。md/角色卡按文件级 whole-file 覆盖（本产品单人+agent 场景，文件级足够）；storyboard.json 是单文件 JSON，不承诺字段级合并 |

服务端新增同步端点（P1，见 §G）：`GET /film/projects/:id/sync/status`
（head_commit/dirty/ahead/behind）、`POST /film/projects/:id/sync`
（direction=pull|push，push 带 commit message）。

---

# D. 成本区域（Budget）

## D.1 现状盘点（成本数据缺口）

- `film.rs` **任何阶段都不记录耗时/字节/用量**：`channel_roundtrip_bytes` 只返回
  `Vec<u8>`；`forward_channel` 返回 `(String, Option<(u32,u32,u32)>)`（usage 三元组，
  film 的 chat 分支已拿到但**用后即弃**）；任务只有 status/output/error。
- `Channel` 结构无任何价格字段（仅 request_count 累计）。
- api_gateway 的 `sk-os-` 计费只作用于 **ApiToken 消费者面**
  （`/api/v1/gateway/v1/*`，billing_mode per_token/per_image/credits/free）；
  film 走 admin 内部 `forward_channel` 直呼渠道——**完全绕过计费**（现状即如此，
  单用户节点合理）。

## D.2 设计：事件源 → 账本 → 单价 → 面板

```text
阶段任务完成点（film.rs run_*_stage / task_finish 扩展）
   │ 记事件（同步写库，不依赖任务内存态存活）
   ▼
film.db 新表 film_cost_events（真值）
   id/project_id/task_id/stage/shot/source/channel_id/model/ok/
   wall_secs/bytes_out/prompt_tokens/completion_tokens/est_cost/currency/created_at
   │ 双写（P1 起，仓存在时）
   ▼
budget.json（仓内账本，append-only：同字段数组 + budget_limit）
   ▲ 单价配置（可选拦）
Channel 增可选字段：price_per_call / price_per_sec / price_per_token + price_currency
   （serde default 全可缺省——旧渠道 JSON 零迁移；SQLite ALTER 幂等补列，同 export_dir 先例）
   ▼
项目成本面板 UI（§D.3）
```

要点：

1. **事件在「任务完成点」记，不在渠道转发层记**——local 与 channel 两源都覆盖
   （local 记 wall_secs + 显存占用可选；channel 另记 bytes/tokens）；error 任务
   也记（ok=false，est_cost 按 0——上游失败一般不计费，如实保留字段供修正）。
2. **est_cost 计价纯函数**：`per_call + per_sec×duration + per_token×tokens`
   三项叠加，缺省单价=0（成本显示「未配置单价，仅计量」——诚实不假装）。
   currency 缺省 `CNY`（渠道上游以人民币计价为主）；单位约定元/秒/千 token
   由面板注明。
3. **budget_limit**：`budget.json` 顶层可空字段；超限后面板黄条 + 新任务提交时
   警示（v1 不硬拦，防误伤——硬闸门列开放问题）。
4. **events（DB）为真值，budget.json 为仓内投影**——重建方向永远 DB→json，
   防追加式文件被手工改坏。

## D.3 项目成本面板 UI 草案

```text
┌─ 成本账本 Budget ─────────────────────────────────────────────────┐
│ 本项目累计：¥47.62（限额 ¥100.00 ██████████░░░░ 47.6%）            │
│ 按阶段： script ¥0.12 │ image ¥8.40 │ video ¥31.20 │ tts ¥4.90 │   │
│         music ¥2.00 │ compose ¥0（本地 ffmpeg 不计外部成本）        │
│ 按渠道： Seedance-视频 ¥31.20 │ OpenAI官方 ¥13.42 │ 本地 ¥0        │
│ 最近事件（可展开）：                                                │
│  09-05 14:22 video ch-101 seedance-1-lite shot-3 5s ¥3.10 ✓        │
│  09-05 14:19 image ch-102 seedream-4.0 shot-3 0s ¥0.35 ✓           │
│ [导出 budget.json] [配置渠道单价 →网关渠道设置]                       │
└────────────────────────────────────────────────────────────────────┘
```

落位：① 剪辑室顶栏成本徽章（累计额，点击开面板）；② 枢纽项目卡角标；
③ 镜头卡成本角标（D.2 按 shot 聚合，视频 5s×单价是主要成本项）。

## D.4 film 接入 sk-os- 计费的评估（结论：P0 不接）

| 方案 | 说明 | 评估 |
|------|------|------|
| A. film 层记账（推荐 P0） | 事件+账本+面板如上；不碰 ApiToken | 零侵入、单用户节点完全够用；数据形态预留兼容（事件含 channel/model/tokens，可后喂 credits） |
| B. film 走网关 token 面 | film 为每项目造 sk-os- 令牌，调用过 `/api/v1/gateway/v1/*` 让网关统一扣费 | chat/image 天然兼容；**video/tts/music 二进制响应网关 OpenAI 面不支持**（forward_channel String 化）——需先给网关加字节面；延迟/复杂度上升。仅当多租户/变现（把影片管线作为能力卖给别人）时值得，列 P3+ 开放问题 |

---

# E. 拓展预留：大厅（Lobby）

大厅（`nexhub_lobby`，现支持发布/克隆/付费/悬赏/链上身份/联邦）不需要新协议
——film 项目本来就是 `film-*` 仓，天然可 publish。预留一个 **manifest 式项目
描述符**（放仓根 `film.toml`，或复用 project.md frontmatter 扩展段）：

```toml
[hub]                      # 大厅门面（publish 时服务端读取快照）
title = "落日航线"
kind = "template"          # template（可复用项目骨架）/ showcase（成品展示）
tags = ["sci-fi", "9:16"]
duration_secs = 92         # 成品时长（showcase 用）
cover = "assets/cover.png" # 封面（发布时快照入库）
license = "CC-BY-4.0"      # 模板/成片段的授权面（开放问题 #7）

[hub.showcase]
final_artifact = "final.mp4"     # artifacts.json 键；联邦下载走 os-storage/大厅，不走 git
cost_total = 47.62               # 发布时 budget 汇总快照（复刻成本透明——差异化卖点）
```

接口预留：大厅列表 `GET /api/v1/nexhub/lobby` 增 `kind=template|showcase`
过滤参数；克隆模板 = 现有 `POST /lobby/:name/clone` + 服务端建 film_projects
行（P3 落地，本期只留 schema 不做端点）。

---

# F. 分期实施（每期小而完整）

| 期 | 范围（一句话） | 交付物 | 工作量估 | 主要风险 |
|----|---------------|--------|---------|---------|
| **P0 文件与账本地基** | 项目文件规范落地 + 双向导出导入 + 成本事件数据层 | ① storyboard/角色卡/project.md 的 import/export 端点（项目 ⇄ 文件树，落 `export_dir` 或项目目录 `files/`）；② `film_cost_events` 表 + 各阶段完成点记事件 + Channel 三个可选单价字段；③ `GET /film/projects/:id/cost` 聚合端点；④ 前端：顶栏成本徽章 + 只读成本面板；⑤ schema 文档（本文件 §G 冻结） | 5-8 人日 | 低：全部是现有框架内增量（表列 ALTER 幂等、b64/JSON 形态有先例、任务面不动） |
| **P1 git 仓化 + FilmHub** | 项目=NexHub 仓，双向同步 + CLI + 枢纽页 | ① 建项目=建仓+项目目录 clone（惰性迁移存量）；② sync/status 端点 + 回写 commit + artifacts.json 自动 commit；③ push 体积闸门；④ `nexhub film` 子命令组；⑤ 前端枢纽页：film 项目卡 ⇄ 仓浏览（复用 coderepo contents/file/commits 端点，**后端零新增**） | 8-12 人日 | 中：git 子进程错误面、冲突三选一交互、存量迁移回归 |
| **P2 画布重构** | 剪辑室五区 → 画布主面 | ① StoryboardCanvas.vue（网格+连线+缩放，镜头卡三合一聚合）；② 现镜头纵列降级为画布的「列表视图」toggle（**增量共存，不替换**——回归面控制）；③ hub 同步黄条/回写按钮 UI；④ i18n 四语言新键 | 8-12 人日 | 中：FilmStudio.vue 2914 行大组件，须拆子组件渐进迁移（StoryboardCanvas/Inspector/CostPanel/SyncBar） |
| **P3 CI 校验 + 大厅 + 计费评估** | 闭环增值 | ① CI film 流水线（detect_pipeline 增 film 分支：schema/角色名/预算校验）；② `film.toml` 描述符 + 大厅 template/showcase 发布；③ sk-os- 接入 spike（多租户决策点） | 按需 | 大厅授权/版权面需产品决策 |

**P0 必须小而完整（一页话）**：不碰 git、不碰画布布局。只做三件事——
**(1) 文件规范**：定义并冻结 project.md/story.md/storyboard.json/characters/
budget.json 的 schema（§G），提供 `POST /film/projects/:id/export`（项目 DB+产物
目录状态 → 完整文件树，含从 film_characters 生成角色卡、从 script.json 平移
storyboard.json——**字段零翻译**）与 `POST /film/projects/import`（文件树 → 建
项目，v1 从目录导入；zip/仓导入 P1）；**(2) 成本记账**：film.db 增
`film_cost_events`（ALTER 幂等），六个阶段任务完成点各记一条（stage/source/
channel/model/ok/wall_secs/bytes/tokens），Channel 增
price_per_call/price_per_sec/price_per_token 可选列（缺省 0，不配则只计量），
`GET /film/projects/:id/cost?by=stage|channel|day` 聚合；**(3) 前端最小面**：
剪辑室顶栏成本徽章 + 成本面板（只读聚合表）。验收：任一现有项目 export 后
目录树与本文件 §B.1.1 逐文件一致；跑一轮生成后 cost 端点有非空事件且聚合数
与任务数吻合；旧渠道 JSON 不配单价时一切行为与现状逐字节一致。

---

# G. 契约草案

## G.1 新端点（组件 `film`，读公开 / 写 admin，沿用现有门控）

| method | path | 请求 | 成功响应 | 备注 |
|--------|------|------|----------|------|
| POST | `/api/v1/film/projects/:id/export` | `{target?: "files"\|"export_dir"}` | 202 任务（output=文件树根） | P0；生成 §B.1.1 全树 |
| POST | `/api/v1/film/projects/import` | `{dir}`（绝对路径，须在 `NEXOS_FILM_EXPORT_BASE` 内或本机 admin 面） | 201 `FilmProject` | P0；从文件树建项目（storyboard.json→script.json 平移） |
| GET | `/api/v1/film/projects/:id/files` | — | `[{path, bytes, kind}]` | P0；文件树清单 |
| GET | `/api/v1/film/projects/:id/files/<path>` | — | `{content_b64, mime}` | P0；单文件读（b64 信封同 files.rs download） |
| PUT | `/api/v1/film/projects/:id/files/<path>` | `{content_b64}`（写 project.md/storyboard.json/characters/*） | `{written, bytes}` | P0；agent 写文件面（防穿越同 refs；storyboard.json 写入即热装载到 script.json） |
| GET | `/api/v1/film/projects/:id/cost` | `?by=stage\|channel\|day` | `{total, currency, groups:[{key, cost, events}], limit?}` | P0 |
| GET/PUT | `/api/v1/film/projects/:id/budget` | PUT `{budget_limit?}` | `budget.json` 内容 | P0 |
| GET | `/api/v1/film/projects/:id/sync/status` | — | `{repo, head_commit, dirty, ahead, behind, last_synced_commit}` | P1 |
| POST | `/api/v1/film/projects/:id/sync` | `{direction: "pull"\|"push", message?}` | pull：`{applied, head_commit}` / push：`{committed, pushed, head_commit}` | P1；冲突 409 附三选一指引 |
| POST | `/api/v1/film/validate` | body=storyboard.json 原文 | `{ok, errors:[{path, msg}]}` | P1（CLI/CI 共用内核） |

## G.2 文件 schema（冻结草案）

`storyboard.json`（与 `ScriptShot` 逐字段对齐，仅外层增版本号）：

```json
{
  "version": 1,
  "shots": [
    {
      "shot": 1,
      "desc": "黄昏，灯塔剪影",
      "image_prompt": "…",
      "video_prompt": "…",
      "line": "我们回不去了。",
      "duration_secs": 5,
      "characters": ["小明"]
    }
  ],
  "generated_by": "channel ch-101 · seedance-1-lite",
  "created_at": "2026-09-05T14:00:00+08:00"
}
```

`characters/小明.md`：

```markdown
---
voice: alloy
portrait_ref: assets/portraits/小明.png
updated: 2026-09-05
---
黑发少年，红色围巾，身形瘦高。左眉有小疤（近景特写需保留）。
性格固执但心软。服装：洗旧的藏蓝校服外套。
```

`budget.json`：

```json
{
  "version": 1,
  "currency": "CNY",
  "budget_limit": 100.0,
  "events": [
    {"id": "ce-1", "task_id": "task-42", "stage": "video", "shot": 3,
     "source": "channel", "channel_id": "ch-101", "model": "seedance-1-lite",
     "ok": true, "wall_secs": 5, "bytes_out": 8421000,
     "prompt_tokens": 0, "completion_tokens": 0,
     "est_cost": 3.10, "created_at": "2026-09-05T14:22:01+08:00"}
  ]
}
```

`artifacts.json`：

```json
{"version": 1, "items": [
  {"stage": "video", "shot": 3, "name": "shot-3.mp4", "bytes": 8421000,
   "sha256": "…", "task_id": "task-42", "created_at": "…"}
]}
```

## G.3 CLI 命令（nexhub film 子命令组，见 §B.3）

## G.4 前端组件树增量

```text
apps/film/src/
├── FilmStudio.vue          # 保留（列表态+剪辑室骨架），列表态改称枢纽态
├── StoryboardCanvas.vue    # 新（P2）：网格+连线画布、镜头卡、缩放、批量选择
├── ShotCard.vue            # 新（P2）：卡=缩略图+资产状态行+动作聚合+成本角标
├── Inspector.vue           # 演进（P2）：现中部镜头面板抽出（含角色 chips 编辑）
├── CostPanel.vue           # 新（P0 只读聚合；P2 嵌剪辑室）
├── SyncBar.vue             # 新（P1）：hub 同步黄条+回写按钮+冲突三选一
├── TimelineTracks.vue      # 保留零改（P2 仅事件联动）
├── PreviewMonitor.vue      # 保留零改
└── previewEngine.ts        # 保留零改
```

i18n 新键（四语言齐上）：`film.hub`（影片枢纽/FilmHub/…）、`film.canvas`、
`film.sync.pull/push/conflict.*`、`film.cost.total/byStage/byChannel/limit`、
`film.shotCard.*`。

---

# H. 风险与开放问题

## H.1 风险

| 风险 | 等级 | 缓解 |
|------|------|------|
| 仓规模膨胀（>1000 项目平铺扫描慢） | 中 | `code_repo` 列表是目录扫描；P1 保持平铺 + `film-` 前缀约定，>500 仓时评估 `film/` 子目录分桶（code_repo 扫描逻辑小改） |
| 大二进制误 push（把 mp4 提交进仓） | 中 | push 闸门单文件 >20MB 拒收；建仓模板 .gitignore 预排除产物模式 |
| FilmStudio.vue 2914 行单组件重构回归 | 高 | P2 增量共存：画布/列表视图 toggle，不删旧路径；组件拆分先行（ShotCard/Inspector 抽出可独立验证） |
| 双真源漂移（film.db script.json ⇄ 仓 storyboard.json） | 中 | 单一装载方向：仓文件 → 画布（导入即覆盖 DB 派生态）；DB 内 script.json 定位为「工作副本」，仓为真值（P1 起） |
| 任务内存态与成本事件不一致（服务重启丢任务） | 低 | 事件在完成点同步落库（不依赖 tasks HashMap 存活）——任务丢了账不丢 |
| story.md 与 storyboard.json 语义漂移 | 中 | 约定 storyboard.json 为机读真值、story.md 为人读派生物（可由分镜再生成）；不做双向同步 |
| 成本单价失真（渠道实际计费口径差异：按量/包月/异步任务部分计费） | 中 | 事件保留原始计量（secs/bytes/tokens），est_cost 仅估算展示；单价可随时改 + 面板标注「估算」 |

## H.2 开放问题（需用户/产品拍板）

1. **仓粒度**：一项目一仓（推荐）vs 一用户一仓多项目目录（少仓但 diff/权限面混）？
2. **产物要不要可选进仓**：v1 不进（§B.1.3）；若将来要「clone 即完整项目」，
   是否引入 git LFS 或 os-storage 外链清单？（倾向后者）
3. **计价货币与单位**：CNY 元 / credits / sats？按秒还是按次为主？（渠道以
   按秒计费的视频为主，建议 price_per_sec 为主键）
4. **预算硬闸门**：超限是警示（推荐 v1）还是拒绝新任务？
5. **sk-os- 接入时机**：仅当「影片管线作为能力开放给多租户」立项时做（需先给
   网关加二进制响应面）——现在只做记账。
6. **画布 v1 自由度**：网格自动布局（推荐）vs 真自由拖拽 + 连线改序？
7. **大厅分享授权**：模板/成片段的 license 默认值与付费分享边界（CC-BY-4.0
   起步？沿用大厅 price_sats？）。
8. **CI 校验执行面**：内置 Rust 校验器（detect_pipeline 加 film 分支，进程内
   校验——推荐）vs CI 步骤 curl 本机 `/api/v1/film/validate`（零新代码但依赖
   节点自连）。
9. **story.md 的地位**：纯派生物（推荐）还是允许人直接改后反向生成 storyboard
   （需 LLM 反解，漂移面大）？

---

## 附：业界参照来源

- NLE/组织术语：[Clockwise 后期术语表](https://clockwiseproductions.com/a-post-production-glossary-for-editing/)、
  [Vyond 视频制作术语](https://www.vyond.com/blog/video-production-terms-you-need-to-know/)、
  [Riverside 剪辑术语表](https://riverside.com/video-editor/video-editing-glossary)、
  [Reddit r/editors 术语清单](https://www.reddit.com/r/editors/comments/1f5a24/list_of_video_editing_terminology/)
- DaVinci 组织/节点图：[davinciresolve21.com Bin 命名实践](https://davinciresolve21.com/blog/davinci-resolve-folder-and-bin-naming-conventions-for-organized-projects)、
  [DVResolve Power Bins](https://dvresolve.com/tutorial/organizing-projects-power-bins/)、
  [The Post Flow 项目目录结构](https://thepostflow.com/post-production/a-project-folder-structure-designed-for-davinci-resolve/)、
  [Frame.io Resolve 节点图模板](https://blog.frame.io/2024/04/01/new-resolve-template-node-graphs-color-grading/)
- 时间轴 vs 节点图：[Videomaker: Nodes vs. Layers](https://www.videomaker.com/article/c3/17836-nodes-vs-layers/)、
  [Reddit r/vfx Layers vs Nodes](https://www.reddit.com/r/vfx/comments/13sdldo/layers_vs_nodes_what_are_the_differences_for/)、
  [Video StackExchange: 节点合成中的时间轴](https://video.stackexchange.com/questions/28066/timeline-in-node-based-compositing)
- AI-native 创作工具：[Saga（AI 编剧+分镜+previz）](https://writeonsaga.com/)、
  [Higgsfield AI Movie Maker](https://higgsfield.ai/ai-movie-generator)、
  [Medium：自治 AI 制片管线拆解（DoP agent）](https://medium.com/@jengas/dissecting-an-autonomous-ai-filmmaking-pipeline-0192b7a69636)、
  [Filmustage（AI 前期制作平台）](https://filmustage.com/)
