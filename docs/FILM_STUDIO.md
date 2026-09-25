# 影片制作管线 Film Studio（2026-09-04）

> ⚠️ **已剥离（v0.1.51 三引擎剥离批）**：film 引擎（film.rs / film_hub 全家）
> 已删除出 os-api——独立产品线 **FilmStudio**（NexHub 仓）承接；安装
> `nexos-app-film` 应用会被应用中心 400 拦截。本文为历史契约存档。

> 参考 LibTV AI 影片管线：一条「创意 → 成片」的六阶段 AI 影片流水线，每个阶段
> 可独立选择模型来源（本地能力 / 网关渠道，含 🌐 via_node 中继渠道）。
> 后端：`crates/os-api/src/handlers/film.rs`（组件 `film`，main.rs 装配注入
> api_gateway / llm 共享实例）+ `film_hub.rs`（2026-09-06 FilmHub 流程引擎，
> film.rs 超 5000 行的拆分模块——共享 FilmCtx/FilmRouteHandler/任务框架）；
> 前端：「影片工作室」桌面应用（NexHub `nexos-app-film` 独立应用包）。

```text
剧本分镜(chat) → 关键帧图(image) → 图生视频(video)
                → 台词配音(tts) → 背景音乐(music) → ffmpeg 合成(compose)
                    ↑角色定妆图/绑定注入（一致性，§1b）
```

## 0. FilmHub 流程引擎（2026-09-06，v0.1.35——本节为当前产品主线）

用户流程定型为：**hub 建项目 → 剧情页（①txt/小说导入 ②AI 写）→ hub 存剧情 →
分镜页（AI 读剧情分析生成分镜脚本）→ hub 存分镜 → 定妆（AI 提取 + 每对象
多视图）→ BGM（高频/场景触发式）→ 生成（cache 半成品与 dist 成品分离）**。
实现全在 `handlers/film_hub.rs`（树内核/新端点/新阶段执行器/成本记账），
经 film.rs 的兜底委托接入；实现细节与设计取舍见该文件模块头注释。

### 0.1 hub 文件树（冻结契约；`<dir>/hub/` 下）

```text
project.md          # front-matter: title/ratio/style_hint/export_dir；正文=idea
README.md           # 项目自述：front-matter stage/progress（story|storyboard|
                    #   casting|audio|compose）——AI agent 入口
story/source-*.txt  # 方式一：导入原文（多份支持；≤2MB UTF-8 文本）
story/story.md      # 剧情正稿（front-matter: source/words/summary；正文=分幕剧本）
storyboard/storyboard.json  # 分镜（shots[]：ScriptShot 全字段 + casting 引用
                    #   扩展 characters[]/props[]/pets[]/scenes[]/actions[]）
casting/extraction.json     # AI 提取报告 {characters|weapons|pets|formations|
                    #   actions|scenes: [{name,desc,frequency,reason}]}
                    #   （weapons 键对应 casting/props/ 目录）
casting/characters/<name>/card.md  # front-matter: name/voice/portrait；正文=外形描述
casting/characters/<name>/views/<view>.png  # 多视图 front/side/back/action-N/custom-*
casting/{props,pets,formations,actions,scenes}/<name>/…  # 同构（actions=动作序列
                    #   参考图；scenes=场景定妆图；formations=多人排列站位图）
audio/bgm/<track>/info.md    # front-matter: trigger(global|scene:<场景名>)/mood/
                    #   duration；+ track.mp3
budget.json          # 成本账本（film_cost_events 投影——DB 为真值）
assets.json          # 资产统一清单 [{path,sha256,bytes,source:ai|import,ref}]
ownership.json       # 多人分工：{members:[{name,joined_at}], sections:{<阶段>:
                    #   {owner,claimed_at}}, casting_objects:{"<type>/<name>":
                    #   {owner,claimed_at}}}（对象级认领）
activity.json        # 操作流水环形 200 条 [{ts,author,action,target}]
collab/issues.json   # v0.1.42 收尾批：Issue 数组（iid 递增；state 三态；
                     #   stage 环节归因；评论环形 200）——文件即真值（§0.8）
collab/prs.json      # PR 数组（pid 递增；branch→base git 分支映射；merge_commit
                     #   合并后回填；issue_refs 合并自动关）
cache/               # 试生成/半成品（shot 试生成图、临时音频；不进 dist）
dist/                # 成品 final-vYYYYMMDD-HHMM.mp4 + compose-report.json
                     #   （版本化；同分钟碰撞追加 -2/-3 后缀）
```

- **旧项目惰性初始化**：无 `hub/` 的存量项目首次调新端点（或 image/video/tts
  试生成落 cache）自动 export 建树——script.json 平移 storyboard.json（字段
  零翻译）、film_characters 旧角色库迁移为 `casting/characters/<slug>/`
  （card.md + 定妆图 → views/front）。新建项目即建树。
- **export 语义**：`POST :id/export` 重刷 project.md/README（已存在则保留
  stage）/storyboard（仅 script.json 较新时覆盖——files PUT 手改不被冲掉）/
  budget.json（DB 真值）；story/casting/audio/**ownership/activity/assets**
  原样保留（文件是真值）。
- **import 语义**：`POST :id/import` 把树内 storyboard.json 校验后应用到
  script.json（画布状态，files PUT 手改后的显式应用面）——casting 引用与
  定妆对象名比对，未知名不硬拦（`unknown_casting_refs` 报告给 agent 修名）。

### 0.2 新端点（21 条，组件 film；读公开/写 admin；全部 eprintln! [filmhub]）

| method | path | 说明 |
|--------|------|------|
| POST | `/api/v1/film/projects/:id/story/import` | `{filename, content_b64, author?}` → `story/source-<slug>.txt`（≤2MB UTF-8；**multipart 不可行**——网关 body 恒 JSON，files.rs 同款 b64 信封先例）|
| POST | `/api/v1/film/projects/:id/story/generate` | `{model_ref(chat), prompt?, source_file?, author?}` → 202 任务（stage=story）→ story.md + README 阶段推进；source_file 给定则「基于原文改编浓缩」|
| POST | `/api/v1/film/projects/:id/storyboard/generate` | `{model_ref(chat), author?}` → 读 story.md 分幕生成分镜（无剧情回落【创意】）→ storyboard.json + script.json 镜像（**产出即应用**）|
| POST | `/api/v1/film/projects/:id/casting/extract` | `{model_ref(chat), author?}` → AI 读 story+storyboard 提取六类对象 → extraction.json |
| GET/POST | `/:id/casting/:type` | type∈characters\|props\|pets\|formations\|actions\|scenes；POST `{name, desc, voice?, author?}`（name slug 化，重名 **409**；author 触发对象级自动认领+activity）|
| PUT/DELETE | `/:id/casting/:type/:name` | card.md 读改删（PUT `{name?, desc?, voice?, portrait?, author?}`；改名迁移目录+认领键）|
| POST | `/:id/casting/:type/:name/views/generate` | `{model_ref(image), view, prompt?, author?}` → 202 → `views/<view>.png` + assets 登记 source=ai（人物类缺省模板见 §0.3；channel 档注入既有视图为参考）|
| POST | `/:id/casting/:type/:name/views/import` | `{image_b64, view, mime?, author?}`（mime/魔数双校验 ≤10MB）source=import |
| GET/POST | `/:id/audio/bgm` | POST `{name?, info:{trigger?, mood?, duration?}, track_b64?, author?}`（track 省略=先建条目；trigger 校验 global\|scene:\<名\>）|
| DELETE / POST | `/:id/audio/bgm/:track` / `…/generate` | 删音轨；生成 `{model_ref(music), prompt?, author?}`（trigger 从 info.md 读进缺省 prompt）→ track.mp3 |
| POST | `/:id/cache/:file/commit` | **确认采用**：把 cache 半成品转正到正式产物路径（file∈shot-<n>.png\|shot-<n>.mp4\|line-<n>.mp3，白名单校验）|
| GET | `/:id/files` | hub 树清单 `[{path,bytes,kind:text\|binary\|other}]`（depth≤6/≤2000 条）|
| GET/PUT | `/:id/files/<path>` | 白名单=树内全部文本 + 资产二进制读（casting 视图/BGM/dist/cache）；PUT 文本面（ownership.json 校验后落、budget.json 仅 budget_limit 生效、storyboard 手改不自动应用）|
| POST | `/:id/export` / `/:id/import` | 项目状态→hub 树 / 树→画布状态（见上）|
| GET | `/:id/cost?by=stage\|channel\|day` | 成本汇总 `{total,currency,events,limit?,totals:{wall_secs,bytes,tokens},groups:[{key,cost,events,wall_secs,bytes,tokens}]}` |

**兼容别名**：旧 `POST /:id/script` 保留——同一执行体（有剧情读 story.md，
无剧情回落【创意】），双写 storyboard.json + script.json，任务 output 仍指
script.json（旧契约不变）；其余 20 条既有端点零破坏（image/video/tts 产物
改落 cache，见 §0.4）。

### 0.3 提示词三份（原文冻结，实现在 film_hub.rs）

- **剧情**（story generate）：system=「你是专业的短片编剧。必须严格围绕用户
  给出的【创意】创作，禁止更换题材、禁止另编与【创意】无关的故事。只输出
  剧本正文，不要任何解释文字。」；user=「请为下面的影片创意创作剧情正稿
  （分幕剧本文本）。【创意】…【画幅】…【风格提示】…（【改编原文】请基于以下
  导入原文改编浓缩为分幕剧本…）要求：1. 输出分幕剧本：以「【第一幕】」
  「【第二幕】」……为幕标题，每幕写明场景、人物动作与关键对白，共 3 到 6 幕。
  2. 必须严格围绕【创意】的故事创作：禁止更换题材、禁止另编与【创意】无关的
  故事；每一幕都必须直接服务于该创意的叙事。3. 只输出分幕剧本正文（不要
  markdown 代码块标记、不要任何解释文字）；正文中反复出现的人物、武器、
  宠物、场景与动作请使用稳定统一的名字（后续定妆阶段将按这些名字建定妆
  对象）。最后再强调一次：所有幕必须讲【创意】本身的故事——【创意】是：…」
- **分镜**（storyboard generate）：system 同旧 script（题材硬约束）；user=
  「请为下面的影片剧情生成分镜脚本。【剧情】<story.md 正文>【画幅】…【风格
  提示】…要求：1. 从剧情逐幕分析，输出 5 到 12 个镜头，按叙事顺序（幕结构
  优先对齐：一幕可拆多个镜头，不要跳幕）。2. 必须严格围绕【剧情】的故事
  创作分镜：禁止更换题材、禁止另编与【剧情】无关的故事；每个镜头的画面都
  必须直接服务于该创意的叙事。3. 只输出一个 JSON 数组……每个元素形如
  {"shot":1,"desc":…,"image_prompt":…,"video_prompt":…,"line":…,
  "duration_secs":5,"characters":[…],"props":[…],"pets":[…],"scenes":[…],
  "actions":[…]}。4. duration_secs 取 2-10 的整数。5. casting 引用字段
  （characters/props/pets/scenes/actions）按名字引用剧情中反复出现的人物、
  武器、宠物、场景与动作——这些名字将在定妆阶段建定妆对象；剧情中没有的
  类别输出空数组，不要编造剧情里不存在的对象。（【角色表】…）最后再强调
  一次：所有镜头必须讲【剧情】本身的故事——【剧情】是：<首 80 字>」
- **提取**（casting extract）：system=「你是专业的影片定妆统筹。只输出
  JSON 对象，不要任何解释文字。」；user=「请从下面的影片剧情与分镜中提取
  「定妆对象」清单（六类）。【剧情】…【分镜】…六类定义（key 固定）：
  - characters：出场人物（主角与重要配角，按名字）- weapons：人物使用的
  武器 / 关键道具（按名字）- pets：出场的宠物 / 动物（按名字）-
  formations：多人同框的站位排列 - actions：跨镜头重复出现的高频动作 -
  scenes：跨镜头重复出现的高频场景。要求：1. 只输出一个 JSON 对象（以 {
  开头、以 } 结尾）……2. frequency 必须是整数 = 该对象出场的镜头数（对照
  【分镜】逐镜头统计；仅在剧情出现而分镜未覆盖的按 0）。3. 每类按
  frequency 降序；剧情中不存在的类别给空数组；名字须与剧情/分镜中使用的
  名字一致。」（解析容错：围栏/散文包裹/字段缺省/frequency 字符串归一）
- 人物类视图缺省模板（casting views generate）：「同一定妆对象的多视图：
  <desc>，<view> 视图，严格一致外形」。

### 0.4 cache / dist 语义（半成品与成品分离）

- image/video/tts 任务产物一律落 `<dir>/hub/cache/`（shot-N.png /
  shot-N.mp4 / line-N.mp3）；`POST /:id/cache/:file/commit` 把 cache 产物
  **转正**（rename）到项目根正式产物路径——compose 只认转正后的正式产物
  （缺失时若 cache 有试生成，报错附 commit 指引）。video 的首帧正式位缺失
  时回落 cache 试生成首帧（允许「图（cache）→视频（cache）」连续试）。
- compose 输出 `dist/final-vYYYYMMDD-HHMM.mp4` + `compose-report.json`
  （final/shots/duration/bgm{track,input}/voices/subtitles/bytes/ffmpeg/
  export_dir）；**export_dir 语义保留为 dist 落点**（设置时 dist 即导出
  目录，第二遍 ffmpeg 输出用绝对路径）；**BGM 选择**：body 可指定 bgm
  track，缺省 trigger=global 优先 → 旧根 bgm.mp3 兜底（不再固定 bgm.mp3）。
  项目详情 artifacts 清单合并扫描 hub/dist（export 分支本来即扫导出目录）。

### 0.5 成本记账（film_cost_events + Channel 三单价）

- `film.db` 新表 `film_cost_events`（id/project_id/task_id/stage/shot/source/
  channel_id/model/ok/wall_secs/bytes_out/prompt_tokens/completion_tokens/
  est_cost/currency/created_at）：**事件在任务完成点同步落库**（error 也记，
  ok=false、est=0——任务丢了账不丢）；DB 为真值、budget.json 为树内投影
  （每次落事件后重建，budget_limit 从现有文件保留；手写 events 无效）。
  埋点阶段：story/storyboard/image/video/tts/music/bgm/view/portrait/compose。
- `Channel` 增三可选单价列（serde default 0，SQLite ALTER 幂等）：
  `price_per_call` 元/次、`price_per_sec` 元/秒、`price_per_token` 元/千
  token；est = per_call + per_sec×wall + per_token×(tokens/1000)。不配置
  则只计量不计价。渠道面经 `POST/PUT /api/v1/gateway/channels` 透传。
- chat usage：channel 分支取上游 usage 三元组；local 分支取
  usage.total_tokens（记 (0,total,total)）。

### 0.6 多人分工与留名（ownership.json / activity.json / author）

- **所有写类新端点 body 可带 `author: String`**（缺省 "anonymous"）——任务
  完成点/文件写入时落一条 activity `{ts,author,action,target}`（环形 200）。
  action 命名表：`story.import` / `story.generate` / `storyboard.generate` /
  `casting.extract` / `casting.create` / `casting.claim` / `casting.update` /
  `casting.delete` / `casting.view.generate` / `casting.view.import` /
  `bgm.create` / `bgm.import` / `bgm.generate` / `shot.image` / `shot.video` /
  `shot.tts` / `music.generate` / `cache.commit` / `compose` / `files.put` /
  `export` / `hub.import`。
- **ownership.json** 经 files 面 GET/PUT 读写（PUT 校验：sections 键 ∈
  {story,storyboard,casting,audio,compose}；casting_objects 键须
  `<type>/<slug>`——type 六类枚举 + slug 名，**对象存在性宽容**（允许认领
  extraction 报告中尚未落地的对象）；owner 非空）。POST casting/:type 带
  author 时自动写对象级认领（owner=author）+ activity（action=casting.claim）；
  对象改名/删除时认领键随迁/随删。

### 0.7 项目 git 仓（2026-09-08，v0.1.42——每项目独立仓，§B 方案①）

**布局**（仓随项目目录走——aliyun/多节点 scp/rsync 同步天然带 git）：

```text
<dir>/               # 项目目录（产物目录：script.json/shot-N.png 等非 hub 面）
├── repo/            # 非裸 git 仓（git init 落点）
│   ├── .git/        # 分支/历史元数据（不含任何绝对路径配置——搬目录/换节点不失效）
│   └── excludes     # 排除清单（cache/ 半成品不进历史）
└── hub/             # 工作树（git --work-tree 指向——分支隔离的是这棵画布状态树）
```

- **实现**：命令行 git 子进程（`std::process`，os-nexhub code_repo.rs /
  lobby_sync_hook.rs 同款先例，**不引 git2**）。所有调用形如
  `git --git-dir=<dir>/repo/.git --work-tree=<dir>/hub <args>`（cwd=hub；
  excludes 经每命令 `-c core.excludesFile=<repo>/excludes` 注入，同样不
  持久化）；提交身份 `-c user.name=<author>` 注入（repo 本地另有 NexOS
  FilmHub 兜底身份）；`-c commit.gpgsign=false` 防宿主签名配置炸提交。
  git 二进制 env `NEXOS_FILM_GIT_BIN` 可覆写（缺省 PATH 上的 git）。
- **建仓时机**：① 新建项目即建（init+身份配置+全量首提 `init by system`）；
  ② 存量项目（如诛仙）首次调任一 git 端点惰性 init+全量首提（幂等）；
  ③ 存量项目首次**写**操作经 commit 钩惰性建仓——此时首提即业务提交
  （`<action> by <author>`，author 归触发者）。三条路径互不冲突。
- **写路径 commit 钩**：所有 hub 写操作完成点（story/storyboard/casting/
  audio/models/ownership/budget/assets/files/import/export/compose 及
  film.rs 侧 shot 试生成/music——共 26 处）自动
  `git add -A && git commit -m "<action> by <author>"`（author 从请求体带，
  缺省 anonymous）。**无 git 或失败只 `[filmhub-git]` 日志不阻断业务**
  （增强非硬依赖）。**commit 节流**：变更仅 activity.json（试生成写的是
  excluded 的 cache/，树里只剩流水一条改动）不单独提交——流水并入下一次
  业务提交，不产生空提交与流水提交风暴。
- **主干保护**：main 上的写操作照常（现状语义=直接提交 main）；分支=画布
  状态隔离的基础。cache/（半成品）经 excludes 不进历史且 restore 的
  `clean -fd` 不带 `-x`（ignored 文件保留）；dist/（成片）进历史。

**六端点**（组件 film；读公开/写 admin；全部先 ensure_hub 再 ensure_repo）：

| method | path | 说明 |
|--------|------|------|
| GET | `/:id/git/status` | 快照：`{git,repo,worktree,branch,head,clean,dirty:[{status,path}]}`；git 不可用 → 200 `{"git":false,detail}` 如实降级 |
| GET | `/:id/git/branches` | `{current,branches:[{name,hash,time,message}]}`（新→旧） |
| GET | `/:id/git/log?branch&limit` | 提交历史 `commits:[{hash,short,author,message,time}]`（branch 缺省当前；limit 缺省 50，钳 1..=500） |
| POST | `/:id/git/branch` | `{name, from?}` 建分支（from 缺省 main；分支名守卫：字母数字开头、允许 `-_./`；已存在 409） |
| POST | `/:id/git/checkout` | `{branch}` 切工作树分支；**切换前脏树校验**——工作树与 HEAD 不一致 → 400 `{"error","dirty":[文件清单]}`（防静默携带未提交修改） |
| POST | `/:id/git/restore` | `{ref}`（分支名或 commit hash）工作区回到该提交；**只回 hub 工作树，产物目录（非 hub）不动**；回滚前当前状态自动存 `backup/<YYYYMMDD-HHMMSS>` 分支（脏修改一并提交进备份，绝不丢数据）；流程=备份分支→切回原分支→`reset --hard`→`clean -fd` |

**护栏（多 agent 并发=git 工作树语义；锁待下批）**：

- 多 agent 并发写同一分支：后写覆盖/竞态提交失败（git index.lock）仅日志，
  不阻断业务——现状语义与直接写文件一致，git 只是留痕；**分布式/进程级
  写锁待后续**；PR/Issues 协作面已落地（§0.8）。
- checkout 脏树即拒（先提交或 restore）；restore 恒有 backup 分支兜底，
  不存在不可逆操作。
- 跨节点：仓与工作树同在项目目录内（scp/rsync 整目录即同步）；不做跨节点
  push/pull（联邦分发下批评估）；repo 配置零绝对路径依赖。

### 0.8 PR/Issues 协作层（2026-09-08，v0.1.42 收尾批——文件即真值 + git 分支映射）

**数据形态**（人/AI 共用：REST 便捷面 + agent 经 files 面直读 json 双通道；
两文件均服务端真值，**files PUT 拒改**（与 activity.json 同款护栏），files GET
直读开放）：

```text
<hub>/collab/issues.json   # 数组（append 序，iid 递增 = max+1）
[{
  "iid": 1, "title": "…", "body": "…",
  "state": "open|in-progress|closed",     # PUT 三态；closed 记 closed_at（重开清之）
  "labels": ["pipeline-error"],            # ≤10 个、各 ≤40 字符、去重保序
  "assignee": "ai-1", "author": "human-1",
  "stage": "story.clean",                  # 可选：管线环节名（环节错误归因；宽松收字符串）
  "created_at": "…", "closed_at": null,
  "comments": [{"author":"…","body":"…","created_at":"…"}]   # 环形 200
}]
<hub>/collab/prs.json      # 同构数组（pid 递增）
[{
  "pid": 1, "title": "…", "body": "…",
  "branch": "draft", "base": "main",       # head 分支 → 目标基线（建 PR 时分支须存在）
  "state": "open|merged|closed",
  "issue_refs": [1],                        # 合并时自动 close 这些 iid（未知编号宽容跳过）
  "author": "ai-1", "created_at": "…",
  "merged_at": null, "merged_by": null, "merge_commit": null, "closed_at": null,
  "comments": [ … ]
}]
```

**十三端点**（组件 film；读公开/写 admin；实现 film_hub/collab.rs）：

| method | path | 说明 |
|--------|------|------|
| GET | `/:id/collab/issues?state=open\|closed\|all` | 列表（缺省 open=未关，**含 in-progress**；返回新→旧 + counts{open,in_progress,closed}） |
| GET | `/:id/collab/issues/:iid` | 详情（含评论） |
| POST | `/:id/collab/issues` | `{title, body?, labels?, assignee?, author, stage?}` → 201（自动 created_at） |
| POST | `/:id/collab/issues/from-task` | **环节错误一键提 Issue**：`{task_id, author, note?}`——读任务 stage/error/日志尾 10 行/源文件线索自动成文：title=`[环节] <stage> 失败`、body=错误+日志尾+源+时间+备注、labels=`["pipeline-error"]`、stage=任务环节；任务不存在 404 |
| PUT | `/:id/collab/issues/:iid` | `{title?/body?/state?/assignee?/labels?}`（state 三态枚举校验；closed 记 closed_at、离开 closed 清之） |
| POST | `/:id/collab/issues/:iid/comments` | `{author, body}`（环形 200） |
| GET | `/:id/collab/prs?state=open\|merged\|closed\|all` | 列表（缺省 open）+ counts |
| GET | `/:id/collab/prs/:pid` | 详情（含评论） |
| GET | `/:id/collab/prs/:pid/diff` | `git diff <base>...<head>` 文本（merge-base→head；>256KB 截断 + truncated 标记） |
| POST | `/:id/collab/prs` | `{title, branch, base?=main, issue_refs?[], author, body?}`；branch 须为已存在分支（404）、branch==base 400、issue_refs 须正整数 ≤20 |
| PUT | `/:id/collab/prs/:pid` | `{title?/state?}`——state 仅可 `closed`（open→merged 走 merge）；已 closed/merged 幂等空转 |
| POST | `/:id/collab/prs/:pid/comments` | 同 issues 评论 |
| POST | `/:id/collab/prs/:pid/merge` | `{author}`——见下方 merge 语义；冲突 409 / 脏树 400（带清单）/ 已合并已关 409 |

全部写动作落 activity（action=`collab.issue.create|update|comment|from_task` /
`collab.pr.create|update|comment|merge`）+ git commit 钩（同一 action 名——
协作历史随仓走）。

**merge 语义**（collab.pr.merge）：

1. PR 须 open（已 merged/closed → 409）；base/branch 均须存在分支；
2. 脏树 → 400 + 变更文件清单（与 checkout 同守卫）；
3. `checkout <base>` → `git merge --no-ff <branch>`（作者经 `-c user.*` 注入；
   message=`collab.pr.merge: PR !<pid> <title> (by <author>)`）——恒产 merge
   commit（merge_commit 全 hash 回填 prs.json）；
4. **服务端真值 JSON 冲突自愈**：冲突文件全部 ∈ {activity.json,
   collab/issues.json, collab/prs.json} 时按**语义并集**解冲突（activity=条目
   精确去重并集；issues/prs=按 iid/pid 并集、同键取状态阶+评论数更后的一方、
   评论精确去重尾插）——两侧并发追加的环形流水/协作清单恒可自动并；任一
   业务文件冲突不可自愈 → `merge --abort` + 切回原分支 + 409；
5. prs.json 置 merged（merged_at/merged_by/merge_commit）+ issue_refs 命中的
   未关 issues 自动 closed（各附自动评论一行「PR !n 已合并，自动关闭」）；
6. 合并后工作树停在 base（画布=base+分支变更，GitHub 同语义）。

**协作流（人/AI 共用）**：管线环节失败（fake LLM 500 等）→ 任务中心/剧情页
失败行「🐞 提 Issue」（from-task 自动成文，stage 归因）→ 建分支改分镜 →
协作页建 PR（分支下拉=git branches；issue_refs 关联）→ diff 查看 → merge
确认 → issue 自动关闭 → git log 见 merge commit（双亲 `--no-ff`）。agent
亦可全程 REST，或经 files GET 直读 `collab/*.json` 感知协作态（双通道）。

**前端（film 0.1.10）**：SideNav「🧭 协作」项（设置前）→ CollabPage 两 Tab
（Issues：三色状态徽章/labels/assignee/stage 归因标签/评论展开+评论流/新建；
PRs：branch→base/diff pre 截断展开/merge 确认弹窗/新建分支下拉）；深链
`view=collab`；任务中心与剧情页失败行「提 Issue」按钮（成功 toast+跳协作页）。

### 0.9 源入库锁定（2026-09-08，v0.1.44 并入件——清理分章完成后即入库）

**规则**：目标源已有对应章节（`story/chapters/index.json` 存在且 `source`
基名匹配该源——匹配口径与 story/embed 前置分章门同款：`source-x.txt`/
`cleaned-x.txt`/`x.md` 同归基名 `x`；index **无** `source` 字段[早期产物/
手写]时存在即锁）→ `POST :id/story/clean` 与 `POST :id/story/chapterize`
一律 **409**（任务不建，冻结文案）：

```json
{"error": "该文件已入库（已分章定稿）：清理/分章已锁定。如需重新处理请先删除 story/chapters/（Hub 浏览页）后重试"}
```

- **不锁**：`story/embed`（向量化正是入库后的操作）、`story/profile`（支线
  只读）；乙源未入库（index 指向甲）照常清理/分章。
- **前端**（剧情页）：已入库源的 🧹清理/📖分章 按钮 disabled + tooltip
  「已入库：先删 chapters/ 可重新处理」；🧠向量化/👥人物梳理正常；源状态
  徽章加「已入库」档（绿实底，最高显示优先级，替代「已分章」）。
- **重处理出口**（有意不设 UI，防误操作）：在 Hub 浏览页手动删除
  `story/chapters/index.json`（即整组章节清单；`ch-<NN>.md` 留否均可——
  重新分章会整组覆盖）即解锁，clean/chapterize 恢复可用。

## 1. 端点契约（21 条既有基线；读公开 / 写 admin）

### 1a. 项目与管线（14 条基线）

| method | path | 请求 | 成功响应 | 错误码 |
|--------|------|------|----------|--------|
| POST | `/api/v1/film/projects` | `{title, idea, ratio, style_hint?}`（ratio ∈ 16:9/9:16/1:1） | 201 `FilmProject` | 400 非法 ratio / 空字段（缺必填字段反序列化失败）|
| GET | `/api/v1/film/projects` | — | `FilmProject[]` | — |
| GET | `/api/v1/film/projects/:id` | — | `{project, script(分镜数组|null), artifacts:[{name,bytes}], refs:[{name,bytes}]}` | 404 |
| PUT | `/api/v1/film/projects/:id` | `{title?, idea?, ratio?, style_hint?, clear_style_hint?, export_dir?, script?:[ShotPatch]}`（缺省字段保留；script=分镜局部合并，见 §2；export_dir=导出路径，见 §3） | `FilmProject`（响应附 `script` 回显 + `script_patched`；含 `final_path` 便捷字段） | 400 非法 ratio / 补丁镜头不存在 / export_dir 校验失败（相对路径、父目录不存在/不可写、出 `NEXOS_FILM_EXPORT_BASE` 界） / 404 |
| DELETE | `/api/v1/film/projects/:id` | — | `{deleted, dir, dir_removed}`（连产物目录删，角色行连删） | 404 |
| POST | `/api/v1/film/projects/:id/script` | `{model_ref, author?}`（capability=chat）——**兼容别名**：内部走 story→storyboard 新链（§0.2），output 仍指 script.json | 202 `TaskSummary` | 400 model_ref 非法 / 404 |
| POST | `/api/v1/film/projects/:id/shots/:n/image` | `{model_ref, author?}`（capability=image）——产物落 hub/cache（§0.4） | 202 | 400 / 404 项目 |
| POST | `/api/v1/film/projects/:id/shots/:n/video` | `{model_ref, image_first?, author?}`（缺省 true；正式首帧缺失回落 cache 试生成）——产物落 hub/cache | 202 | 400 / 404 项目 / **404 首帧缺失** |
| POST | `/api/v1/film/projects/:id/shots/:n/tts` | `{model_ref, text?, author?}`（缺省 text=script.line）——产物落 hub/cache | 202 | 400 / 404 |
| POST | `/api/v1/film/projects/:id/music` | `{model_ref, prompt?, author?}`（缺省按 idea+style_hint 构造；旧 bgm.mp3 面） | 202 | 400 / 404 |
| POST | `/api/v1/film/projects/:id/compose` | `{bgm?, author?}`（bgm 指定音轨；缺省 trigger=global 优先）——dist 版本化产物（§0.4） | 202 | 404 / 404 bgm 音轨不存在 |
| GET | `/api/v1/film/tasks` | — | `TaskSummary[]` | — |
| GET | `/api/v1/film/tasks/:id` | — | `FilmTask`（含 `log` 环形日志、`output` 产物路径、`error`） | 404 |
| GET | `/api/v1/film/tools` | — | `{ffmpeg:{available, path, install_hint}}` | — |

### 1b. 角色库与参考导入（2026-09-04 P0，7 条）

| method | path | 请求 | 成功响应 | 错误码 |
|--------|------|------|----------|--------|
| GET | `/api/v1/film/projects/:id/characters` | — | `[{id,name,description,voice?,portrait_ref?,portrait_url?,bound_shots:[n]}]` | 404 项目 |
| POST | `/api/v1/film/projects/:id/characters` | `{name, description, voice?}`（**name+description 必填**；voice 可选——缺省不设则 TTS 落全局默认；角色名项目内唯一） | 201 `FilmCharacter` | 400 空字段/重名 |
| PUT | `/api/v1/film/characters/:cid` | `{name?, description?, voice?}`（部分更新；**voice 传空串=清空**回落全局默认） | `FilmCharacter` | 400 重名 / 404 |
| DELETE | `/api/v1/film/characters/:cid` | — | `{deleted, dir, dir_removed}`（连 `characters/<cid>/` 目录删） | 404 |
| POST | `/api/v1/film/projects/:id/characters/:cid/portrait` | `{image_b64, mime?}`（**b64 JSON 形态**——仓库无 multipart 先例；原始标准 b64 不带 data: 前缀；≤10MB；mime 白名单 png/jpeg/webp，mime 与魔数双校验；mime 缺省按魔数嗅探） | 201 `FilmCharacter`（portrait_ref 已回写） | 400 大小/mime/解码 / 404 |
| POST | `/api/v1/film/projects/:id/characters/:cid/portrait/generate` | `{model_ref, prompt?}`（capability=image；prompt 缺省由 description 构造定妆照措辞；720×720） | 202 任务（stage=`portrait`） | 400 / 404 |
| POST | `/api/v1/film/projects/:id/refs` | `{image_b64, filename?}`（魔数嗅探 png/jpeg/webp，≤10MB） | 201 `{name, path, bytes, filename}`（落 `<dir>/refs/ref-<ts>-<n>.<ext>`） | 400 / 404 |

- **产物 URL 口径**：film 产物**不经 apps-assets**（那是应用包静态资源）。
  `portrait_url` 等一律走既有产物读取路径 `GET /api/v1/files/download?path=<绝对路径>`
  （b64 信封 `{mime_type, content_base64}`，公开读）；前端取 `content_base64`
  转 data URL 显示缩略。
- **`bound_shots`**：后端扫 script.json 出场角色名命中派生（每请求现算，非存储字段）。

- **`:n` 为 1 起的分镜号**（与 script.json 每镜头 `shot` 字段一致）。
- 阶段任务生命周期：`queued → running → done(output=产物路径) | error(如实原因)`；
  轮询 `GET /film/tasks/:id`，环形日志上限 200 行；服务重启任务态即清（产物文件与
  `film_projects` 表才是真值）。
- 项目状态：`draft → scripted（分镜完成）→ producing（任一产物阶段完成）→ done（合成完成）`。

## 2. model_ref（阶段模型来源，冻结契约）

```json
{"source": "local" | "channel",
 "channel_id": "ch-101",          // source=channel 时必填
 "capability": "chat" | "image" | "video" | "tts" | "music",
 "model": "seedance-1-lite"}       // 可选：channel 透传给上游的模型名
```

分流矩阵（`FilmCtx` 统一执行面）：

| source | capability | 执行 | 复用面 |
|--------|-----------|------|--------|
| local | chat | 本地 vLLM 实例直连：取 llm 实例表第一个 `running` 实例（port + served_model_name） | 复用 llm handler 的实例调用面 `LlmRouteHandler::chat_complete`（pub(crate) 化，零复制） |
| local | image | sd-turbo 生图：显存闸门（nvidia-smi/统一内存回退）→ 脚本落盘 → spawn python | 复用 media_gen 生图内核函数（`probe_vram_free_mib_with`/`vram_gate`/`ensure_imggen_script`/`run_imggen_with`，pub(crate) 化） |
| local | video/tts/music | 未接入 | 请求期 400 明确提示改用渠道（诚实，不假装排队） |
| channel | chat/image | OpenAI 兼容转发（chat/completions、images/generations） | 复用 api_gateway 的 `forward_channel`（pub(crate) 化）：直连 reqwest 与 **via_node 中继**（overlay 定向源节点代发）两形态同一口径，300s 网关超时 |
| channel | video/tts/music | 字节面转发（`channel_roundtrip_bytes`：直连 reqwest 或复用 `channel_relay_request`+relay 执行层）——二进制音频/视频不能走 String 化的 forward_channel | video/music 超时放宽 600s（`NEXOS_FILM_VIDEO_TIMEOUT_SECS` 钳 60..=1800） |

- **渠道 capability 判定不过度设计**：后端只透传——前端按渠道名让用户选
  （渠道表 `models` 字段 + 用户在渠道命名上的约定，如「Seedance-视频」）。
- 渠道须 OpenAI 兼容形态（见 §4 端点约定）；渠道不存在/已禁用 → 任务 error 如实报。

## 2b. 角色库与一致性（2026-09-04 P0）

### 数据模型与绑定

- DB：`film.db` 增 `film_characters` 表
  （id=`char-<n>`/project_id/name/description/voice/portrait_ref/created_at/updated_at）。
  `portrait_ref` 存**产物相对路径**（如 `characters/char-1/portrait.png`）。
- 分镜绑定：`ScriptShot.characters: Vec<String>`（**角色名数组**，serde default
  兼容旧 script.json）。script 生成提示词注入【角色表】（角色名+描述清单），
  要求 LLM 每镜头输出 `characters` 出场角色名（**须从角色表选**；项目无角色
  表则完全不注入该段、不诱导空字段）。解析容错：**未知名保留原样**落
  script.json + 任务日志逐个提示（不静默丢弃）；同名去重保序。
- 绑定编辑：`PUT /film/projects/:id` 的 `script` 局部合并——`ShotPatch`
  按镜头号只改给出的字段（`shot` 别名 `index`、`desc` 别名 `description`，
  兼容前端早期字段名）；前端镜头面板 chips 增删即 PUT。

### 生成注入（一致性核心，local vs channel 诚实差异）

| 阶段 | local（sd-turbo） | channel（OpenAI 兼容渠道） |
|------|-------------------|---------------------------|
| image | **仅 prompt 注入档**（弱一致）：image_prompt 前置出场角色描述块，sd-turbo 无参考图入口，不发任何参考字段 | prompt 注入档 **同款生效**，叠加 `reference_images`（绑定角色定妆图 b64 数组，顺序=绑定顺序）+ `reference_strength`（可选扩展字段——不识别字段的服务端自然忽略，OpenAI 形态不破坏） |
| video | 本地无 video 能力 | `video/generations` 请求体同款可选 `reference_images`+`reference_strength`（与首帧 `image` 语义分离：image=首帧画面，reference_images=角色身份） |
| tts | 本地无 tts 能力 | `voice` 由角色透传（见下），OpenAI 标准 voice 字段渠道天然兼容 |

- **prompt 注入块固定措辞**（顺序=绑定顺序稳定）：
  `角色「小明」外形：黑发少年，红色围巾（与其它镜头严格同一人物）；…`，多角色
  以「；」连接，整体前置在 image_prompt 之前（style_hint 仍在其后追加）。
- **voice 三态**：镜头出场角色中**第一个有 voice 的角色** → 透传该值（替换旧
  硬编码 `"alloy"`）；否则 env `NEXOS_FILM_TTS_VOICE`；再否则 `alloy` 兜底。
  Vidu 类带 `subjects[].voice_id` 的渠道克隆音色接法为 P2（本期不做
  voice_ref/ref_audio_b64 扩展字段）。
- 参考注入强度缺省 0.5（env `NEXOS_FILM_REF_STRENGTH` 可配，钳 0.0..=1.0）；
  **无绑定角色的镜头不发任何 reference 字段**（与旧行为逐字节一致——零回归面）。
- 一致性强度预期：local=prompt 档★☆☆（同描述+同风格仅统计相似）；channel=
  reference 真注入★★★–★★★★（**取决于所选模型/渠道是否支持主体参考**——
  Seedream/即梦 4.0 `image[]`、MiniMax `subject_reference`、Vidu `subjects` 等
  原生支持，普通文生图渠道则退化为 prompt 档）。

### 产物目录布局（扩展现有布局，向后兼容）

角色定妆图落 `<dir>/characters/<cid>/portrait.png`，项目参考图落
`<dir>/refs/ref-<ts>-<n>.<ext>`——完整布局见 §3。DELETE 项目 / DELETE 角色
分别连 `characters/` 与 `characters/<cid>/` 目录删。


## 3. 项目与产物布局

- DB：SQLite `film.db`（env `NEXOS_FILM_DB`，缺省 `/tank/os-data/film.db` →
  `/var/lib/os/film.db`，失败降级内存库）；表 `film_projects`
  （id/title/idea/ratio/style_hint/status/dir/export_dir/created_at/updated_at
  ——export_dir 为 2026-09-05 加列，存量库 ALTER 幂等补齐）+
  `film_characters`（§2b 角色库）。
- 产物根：env `NEXOS_FILM_DIR`（缺省 `/tank/os-data/film`），每项目一目录：

```text
<tank/os-data/film>/<film-101>/
├── script.json          # 分镜（{"shots":[…每镜头含可选 characters], "generated_by", "created_at"}）
├── shot-1.png …         # 关键帧图（尺寸按 ratio：16:9→1272×720 / 9:16→720×1272 / 1:1→720×720）
├── shot-1.mp4 …         # 图生视频
├── line-1.mp3 …         # 台词配音
├── bgm.mp3              # 背景音乐
├── refs/                # 项目参考图（§2b；ref-<ts>-<n>.<ext>）
├── characters/<cid>/    # 角色定妆图目录（§2b；portrait.png）
├── subs.srt             # 合成时生成（desc+line，按分镜时间轴）
├── compose-concat.txt   # ffmpeg concat 清单（中间产物）
├── compose-video.mp4    # 第一遍 concat 产物（中间产物）
└── final.mp4            # 成片
```

- `DELETE /projects/:id` 连产物目录一起删（目录删除失败仅降级提示，行已删；
  `film_characters` 角色行连删）。
- **导出路径（export_dir，2026-09-05）**：项目级成片落点设置（用户「影片
  保存到哪」）。`PUT /projects/:id` 传 `export_dir`：
  - 空串/null = 重置缺省（项目目录本身，`<dir>/final.mp4`，与旧行为同形）；
  - 非空须**绝对路径**（`~` 不展开）+ **父目录存在**（不自动创建，缺失 400
    附 mkdir 指引）+ **可写**（探针试写）；
  - env `NEXOS_FILM_EXPORT_BASE` 设置时还须位于其下（组件级前缀判定，
    防任意路径写；缺省不限制——单用户节点，写面本就 admin 鉴权）。
  设置后 compose 的 final.mp4 写 `<export_dir>/final.mp4`（输出参数绝对路径，
  导出目录缺失时 compose 补建）；GET 列表/详情回传 `export_dir`（null 或
  路径）+ `final_path` 便捷字段（两分支完整落点）；artifacts 清单照旧含
  `final.mp4` 名（合并扫描导出目录，同名以导出侧为准遮项目目录旧残留）。

## 4. 渠道端点约定（OpenAI 兼容形态，base_url + 固定后缀）

| capability | 后缀 | 请求体 | 响应取数 |
|------------|------|--------|----------|
| chat | `chat/completions` | `{model, messages, max_tokens:4096, temperature:0.7}` | `choices[0].message.content` |
| image | `images/generations` | `{model, prompt, size:"WxH", response_format:"b64_json"}` ＋可选 `reference_images:[b64]`/`reference_strength`（§2b，有绑定角色定妆图才发） | `data[0].b64_json` / `data[0].url`（URL 下载 300s） |
| video | `video/generations` | `{model, prompt, image:"data:image/png;base64,…", image_base64, duration_secs}` ＋可选 `reference_images`/`reference_strength`（§2b） | `url`/`video_url`/`data[0].url`（下载）/ `video_base64`/`b64` |
| tts | `audio/speech` | `{model, input, voice, response_format:"mp3"}`（voice=绑定角色透传 → env 缺省 → `alloy`，§2b） | 二进制音频（或 JSON `{audio:"b64"}`） |
| music | `music/generations` | `{model, prompt}` | `url`（下载）/ b64 / 二进制 |

- **扩展字段兼容原则**：`reference_images`/`reference_strength` 为 OpenAI 形态的
  超集顶层可选字段——不识别的服务端自然忽略（行为与不发完全一致）；**无绑定
  角色时整个字段不发**（与旧行为逐字节同形）。

- 分镜 LLM 输出**解析容错**：原文 / ```json 围栏块 / 首尾中括号切片三候选依次解析；
  字段缺省补齐（duration 缺省 5、钳 1..=60，`"5秒"` 字符串归一）；失败自动**重试一次**
  （更收紧提示词），两次失败任务 error 如实报。镜头数要求 5..=12，接受 1..=24（如实入库）。

### 外部视频模型渠道接入示例（Seedance / 海螺等）

渠道在「API 网关」（`POST /api/v1/gateway/channels`）登记，film 的 model_ref 按
channel_id 引用。渠道须在固定后缀上暴露上述 OpenAI 兼容形态——三种接法：

1. **聚合渠道（最省事）**：用已聚合多家视频模型的 OpenAI 兼容网关（如 OpenRouter
   风格的聚合站 / one-api 聚合部署），登记渠道：
   ```json
   POST /api/v1/gateway/channels
   {"name":"Seedance-视频","provider":"openai",
    "base_url":"https://<聚合站>/v1",
    "api_key":"sk-…","models":["seedance-1-lite"],
    "priority":10}
   ```
   film 侧 `{"source":"channel","channel_id":"ch-…","capability":"video","model":"seedance-1-lite"}`。
   聚合站负责把 `/v1/video/generations` 翻译成火山方舟
   `POST /api/v3/contents/generations/tasks`（Seedance）或 MiniMax
   `/v1/video_generation`（海螺 Hailuo）的原生形态并代轮询。
2. **自建薄适配**：官方原生 API 形态与上述约定不同时（Seedance 是**异步任务**：
   提交返回 task_id、轮询取产物），在渠道 base_url 前放一层薄适配（nginx lua /
   30 行 Web 服务）把「提交+轮询+取 MP4」收敛成一次同步 `video/generations`
   响应（`{url:…}`）。film 本期不做上游任务轮询（响应无 url/b64 时任务 error
   明确提示「异步任务形态暂不支持」）。
3. **联邦中继渠道（🌐 via_node）**：渠道带 `via_node`（0x+66hex NodeID，外部 API
   一键导入自动写入）→ 转发不直连上游，经 os-p2p overlay 定向该源节点代发——
   与网关 chat 转发同一执行面（`channel_relay_request` + relay 分块协议）。适合
   「模型在 A 节点、影片管线在 B 节点」的联邦形态；源节点白名单即渠道 base_url。

TTS/music 同理（OpenAI `/audio/speech` 兼容渠道最常见；Suno 风格 music API 用薄
适配收敛成 `{url}`/b64 响应）。

## 5. ffmpeg 合成（compose）

### 检测（不自动安装）

`GET /api/v1/film/tools` 实时探测；compose 任务执行前再探一次，缺失即任务 error
附安装指引（真实数据铁律：不假装合成）。解析链：

1. env `NEXOS_FFMPEG_BIN`（可执行才认）；
2. `PATH` 扫描；
3. 常规路径 `/usr/bin` `/usr/local/bin` `/bin` `/opt/homebrew/bin` `/snap/bin`。

**安装**（Debian/Ubuntu）：

```bash
sudo apt update && sudo apt install -y ffmpeg
```

静态构建（无 root / 精简系统；`uname -m` 自动选 amd64/arm64）：

```bash
curl -L https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-$(uname -m)-static.tar.xz \
  | tar -xJ
sudo mv ffmpeg-*-static/bin/ffmpeg /usr/local/bin/   # 或放任一 PATH 目录
# 验证：ffmpeg -version
```

### 两遍合成命令模板（cwd=项目目录，文件名全相对——subtitles 滤镜免转义）

第一遍（concat + 统一尺寸/fps 重编码，`build_concat_args`）：

```bash
ffmpeg -y -f concat -safe 0 -i compose-concat.txt \
  -vf 'scale=1272:720:force_original_aspect_ratio=decrease,pad=1272:720:(ow-iw)/2:(oh-ih)/2,fps=30' \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ar 44100 -ac 2 compose-video.mp4
```

第二遍（台词对齐 + BGM 混音 + 字幕烧录，`build_mix_args`）：

```bash
ffmpeg -y -i compose-video.mp4 -i line-1.mp3 -i line-3.mp3 \
  -stream_loop -1 -i bgm.mp3 \
  -filter_complex "\
[0:v]subtitles=subs.srt[vout];\
[1:a]aresample=44100,adelay=0|0[a0];\
[2:a]aresample=44100,adelay=9000|9000[a1];\
[a0][a1]amix=inputs=2:duration=longest:dropout_transition=0[voice];\
[3:a]volume=0.35[bgm];\
[voice][bgm]amix=inputs=2:duration=longest:normalize=0[aout]" \
  -map [vout] -map [aout] -c:v libx264 -pix_fmt yuv420p \
  -c:a aac -b:a 192k -ar 44100 -shortest final.mp4
```

- 台词 `adelay` 起始 = 前序镜头 `duration_secs` 累计（分镜时间轴；上游视频实际
  时长可能有出入，以脚本时间轴为准——已知限制，ffprobe 对齐列后续）。
- BGM `-stream_loop -1` 循环铺满 + `volume=0.35` 压低 + `-shortest` 收口。
- 无字幕 → `-c:v copy` 直通（省一遍编码）；无人声无 BGM → `-map 0:a?` 透传。
- 单遍超时 `NEXOS_FILM_COMPOSE_TIMEOUT_SECS`（缺省 600s，钳 60..=1800），
  超时 kill（kill_on_drop）；失败附 stderr 尾 400 字。
- 输出落点走 `final_path` 语义（§3 导出路径）：`export_dir` 设置时第二遍
  输出参数为绝对路径 `<export_dir>/final.mp4`（导出目录缺失由 compose 补建），
  任务 `output` 附完整路径；未设置时与旧行为逐字节同形（相对名落项目目录）。

## 6. env 清单

| env | 缺省 | 说明 |
|-----|------|------|
| `NEXOS_FILM_DB` | `/tank/os-data/film.db` → `/var/lib/os/film.db` | 项目表 SQLite（失败降级内存库） |
| `NEXOS_FILM_DIR` | `/tank/os-data/film` → `/var/lib/os/film` | 产物根目录 |
| `NEXOS_FILM_VIDEO_TIMEOUT_SECS` | 600（钳 60..=1800） | video/music 生成超时（网关 300s 口径放宽） |
| `NEXOS_FILM_COMPOSE_TIMEOUT_SECS` | 600（钳 60..=1800） | ffmpeg 单遍超时 |
| `NEXOS_FILM_REF_STRENGTH` | 0.5（钳 0.0..=1.0） | channel image/video 角色参考注入强度（local 不发 reference 字段） |
| `NEXOS_FILM_TTS_VOICE` | `alloy` | TTS 全局缺省 voice（绑定角色有 voice 时按角色透传优先） |
| `NEXOS_FILM_EXPORT_BASE` | 空（不限制） | 导出路径基目录：设置时 `export_dir` 必须位于其下（防任意路径写）；缺省关闭——单用户节点，写面本就 admin 鉴权 |
| `NEXOS_FILM_GIT_BIN` | `git` | 项目仓 git 二进制路径（§0.7；指向不存在路径=降级模式——写业务不受影响，git 端点如实报） |
| `NEXOS_FFMPEG_BIN` | — | ffmpeg 显式路径（可执行才认） |
| `NEXOS_IMGGEN_BIN/_SCRIPT/_TIMEOUT_SECS`、`NEXOS_SMI_BIN`、`NEXOS_SD_MODEL` | 见 media-gen | 生图内核注入点（local.image 复用同一 env 链，docs/MEDIA_GEN_AND_CHAIN_AUTH.md） |

## 7. 实现边界与复用关系（零复制原则）

- 生图内核：media_gen.rs 的函数复用（pub(crate) 化，内核零改动）——显存闸门 /
  脚本落盘 / spawn 全走原函数；本地 video/tts/music 能力未接入，诚实报错。
- 渠道转发：api_gateway.rs 的 `forward_channel` / `channel_relay_request` /
  `relay_endpoint` 复用（pub(crate) 化）——直连与 via_node 中继两形态与网关
  自身转发完全同口径；tts/music 二进制响应走 film 自有字节面
  （`channel_roundtrip_bytes`，中继形态复用同一请求组装面）。
- 本地 chat：llm.rs 的 `chat_complete` 复用（pub(crate) 化），实例解析取 llm
  实例表第一个 running 实例。
- 以上三处可见性提升均为**零行为回归**（仅 pub(crate)），双方测试全绿。

## 8. 测试（mock 注入，绝不真调外部模型 / 不装真实 ffmpeg；FilmHub 新链见 §0）

`handlers::film::tests` 62 例（2026-09-04 P0 起）：分镜 JSON 解析容错
（围栏/散文包裹/字段缺省钳制/垃圾拒绝/重试/characters 归一）、model_ref 校验
矩阵、各阶段任务状态机（local 直连 fake vLLM TCP / channel 直连 mock HTTP /
**via_node 中继互连端点** / 未知渠道如实 error）、compose argv 断言（假 ffmpeg
二进制记录两遍 argv：concat 滤镜、adelay 时间轴、amix 混音、subtitles 烧录、
-shortest）、ffmpeg 缺失安装指引、缺镜头视频清单、CRUD 连目录删、404 矩阵；
**角色库 19 例**：characters CRUD/重名/404 矩阵、prompt 注入块模板（固定措辞
「与其它镜头严格同一人物」+ 顺序稳定 + 去重）、绑定解析容错（未知名保留+日志）、
PUT script 局部合并（index/description 别名）、channel 请求体
reference_images+strength 断言（channel 有 / local 无 / 无绑定不发）、voice
三态（绑定角色 > env > alloy）、portrait 上传（大小/mime/魔数/穿越面）、
portrait 生成任务、refs 上传与详情列出。

**FilmHub 27 例**（2026-09-06，`handlers::film_hub::tests`）：纯函数
（slug/front-matter 往返/百分号解码/提示词三份断言/提取解析容错/计价/
ownership 校验）、hub 树建项初始化与旧项目惰性 export（storyboard 平移 +
角色迁移）、story 导入校验矩阵、story 生成（front-matter/README 阶段/
改编原文分支/author 流水）、storyboard 生成（casting 空槽/双写/提示词读剧情）、
script 别名、extract 缺剧情 error 与六类报告、casting CRUD+slug 重名 409+
对象级自动认领与改名迁移、视图双源（fake 生图 source=ai / 导入 source=import /
mime 校验）、BGM 建条目/导入 trigger/生成/compose 选择与 dist 版本化、
cache commit 转正（含 compose 未 commit 提示）、files 读/写/穿越/ownership PUT
校验/import 应用与未知引用、activity 环形截断、成本六阶段埋点+渠道单价+
三种聚合、export 保留文件真值。

**FilmHub git 仓 8 例**（2026-09-08 v0.1.42，真实 git 子进程隔离 tempdir）：
建项即建仓+首提+status/log 幂等、commit 钩（author=ai-1 入历史 +
activity-only 节流 + 真实变更照常提交）、分支建/切/画布隔离/脏树 400 清单/
分支历史互不污染、restore（hub 回滚+未跟踪清理+产物目录不动+backup 分支
生成+分支名 ref 复活）、存量项目 git 端点惰性迁移幂等、存量项目首次写
惰性建仓（首提=业务提交）、无 git 二进制降级不阻断业务+端点如实报错、
分支名守卫纯函数。
