# 影片制作——定妆 actions 动作/运镜结构化升级技术方案书（2026-09-06 调研）

> 面向零上下文读者：读完本文即可理解 film 引擎「定妆 actions 拆子选项——①动作
> 参考（视频参考/骨架参考）②运镜」的完整技术方案——业界 API 实据、我们引擎的
> 差距盘点、数据模型与生成链注入设计、分期落地与契约草案、风险清单。本文为
> **纯调研产物**（只读仓库 + 官方文档核实），不含任何已实施的代码改动。
>
> 姊妹篇：`docs/research/FILM_CONSISTENCY_RESEARCH.md`（2026-09-04——角色/音频
> 一致性，`reference_images` 扩展字段与异步任务薄适配的既定原则在本文沿用）。
> 背景契约：`docs/FILM_STUDIO.md`（渠道约定 / §4 异步任务三接法）。

---

## 0. 一页结论（推荐路线）

**用户需求原话**：定妆 actions 类拆子选项——①动作参考（含视频参考/骨架参考）
②运镜；目标：这些参考注入图生视频生成链（海螺 H3 / Kling / 即梦类渠道 + 本地
路线）。

**核心判断**：业界 2026 年的动作/运镜控制已经收敛到两种形态——

1. **参考视频引用**（主流）：请求体里塞 1-3 段参考视频（role=reference_video），
   prompt 内用「视频1」引用其**动作/运镜/表演**（海螺 H3、Seedance 2.0、Kling
   Motion Control 3.0、Vidu q2-pro、Runway Act-Two 全是这个形态）；**没有一家
   吃裸骨架 PNG**——骨架在服务端内部提取。
2. **运镜参数化**（补充）：Kling v1 系 `camera_control`（5 预设 + 6 轴 -10..10）、
   PixVerse `camera_control` 字符串枚举、Seedance 1.x `camera_fixed` 布尔；新一代
   模型（Kling v2.1+/Seedance 2.x/海螺 H3）**反而去参数化**，改 prompt 描述词 +
   参考视频运镜迁移。

**推荐三阶段路线**（P0 纯数据模型+运镜枚举，一周量级零权重下载；P1 视频参考
注入渠道扩展+异步轮询；P2 本地骨架 AnimateDiff 路线）：

| 阶段 | 一句话 | 依赖 |
|------|--------|------|
| **P0 actions 子选项数据模型 + 资产导入 + 运镜枚举注入** | actions 类 card.md 增 `motion:`/`camera:` 两个结构化子选项；资产类型扩展 mp4/骨架 png（views/import 放开 mime + mp4 上限 + 视频魔数嗅探）；分镜/生成链把运镜枚举拼进 video_prompt 并透传 `camera_control` 扩展字段（对齐渠道原生形态） | 仅 os-api + 前端；无模型下载 |
| **P1 视频参考注入渠道扩展 + 异步任务轮询适配** | video/generations 请求体扩展 `reference_videos`（**推荐本地文件路径形态**而非 b64——见 §5.2）；渠道薄适配把扩展字段翻译成海螺 H3 `content[]{role:reference_video}` / Seedance 2.0 同构 / Kling PMC 专用端点；film 侧补 task_id 上游轮询（上次已备案，本批一起做） | 薄适配层（30 行 Web 服务/聚合渠道）；无本地模型 |
| **P2 本地骨架路线（AnimateDiff + ControlNet DWPose）** | 定妆动作参考视频 → DWPose 逐帧提骨架 → SD1.5/SDXL AnimateDiff（+Lightning 蒸馏）+ ControlNet OpenPose 生成本地视频；作为 `local.video` 能力档（现 local 仅 chat/image）；可叠加首帧（定妆 pose 主帧）做图生视频 | 权重下载 ≈8-13 GB（魔搭可下）；3090 24GB 单卡可行（与 vLLM 互斥沿用显存闸门） |

**为什么这个顺序**：运镜枚举与数据模型是零成本、立刻可做且所有渠道都能吃到
（prompt 注入兜底）；视频参考是业界主流但要求先解决「mp4 资产进树 + b64 体积 +
异步轮询」三件事；骨架本地路线价值在断网/免费/精细控制，成本最高放最后。

---

## 1. A 部分：业界视频生成的参考形态（官方文档核实）

### 1.1 A1 视频参考（motion/subject reference video）——五家实据表

| 渠道 | 端点 | 参考视频字段（官方原名） | 参数形态 | 与首帧图可否同时给 | 时长/大小限制 | 异步形态 | 来源 |
|------|------|--------------------------|----------|--------------------|---------------|----------|------|
| **MiniMax 海螺 H3 / H3-Max** | `POST /v2/video_generation` | `content[]` 元素 `{type:"video_url", video_url:{url}, role:"reference_video"}` | URL / `mm_file://{file_id}` / **b64 data URI**（`data:video/mp4;base64,…`）三态；整请求体 ≤64 MB | **不可**——「图生视频与参考生视频互斥」（first_frame/last_frame 与 reference_* 两族 role 不得混用）；单图无 role 缺省视为 first_frame | 参考视频 ≤3 段，每段 2–15 s、总 ≤15 s、≤50 MB/段，MP4/MOV（H.264/H.265+AAC/MP3），23.976–60 fps；prompt ≤7000 字按「视频1」引用 | task_id 轮询 + callback_url（challenge 回显） | [Video Generation V2 Create](https://platform.minimax.io/docs/api-reference/video-generation-v2-create)、[fal H3 reference-to-video](https://fal.ai/models/minimax/h3/reference-to-video) |
| **Kling 可灵（Motion Control 3.0）** | 独立动作控制端点（fal 镜像 `fal-ai/kling-video/v3/pro/motion-control`） | `image_url`（人物图，角色占比 >5%）+ `video_url`（动作参考视频）+ `character_orientation: image\|video` + `elements[]`（≤1 人脸元素保身份） | URL（fal：URL / b64 data URI / fal.storage 上传三态）；官方原生 b64 不加 data: 前缀（≤10 MB 图同款约定） | 形态上就是「参考图+参考视频」双输入——**首帧=人物参考图**，动作全来自视频 | 参考视频：`character_orientation=image` ≤10 s（更跟随运镜）/ `=video` ≤30 s（复杂动作）；模型 v3.0 / v2.6；按秒计费（720p ≈$0.06/s、1080p ≈$0.10/s） | task_id 轮询（status queue→done） | [Kling Motion Control 3.0 官方](https://kling.ai/document-api/api/video/motion-control)、[fal v3 pro motion-control API](https://fal.ai/models/fal-ai/kling-video/v3/pro/motion-control/api)、[useapi motion-control](https://useapi.net/docs/api-runwayml-v1/post-runwayml-gen3turbo-actone) 同构先例 |
| **Seedance 2.0（即梦同源，火山方舟）** | `POST /api/v3/contents/generations/tasks` | `content[]` 元素 `{type:"video_url", video_url:{url}, role:"reference_video"}` | **URL only**（文档无 b64）；role 族：reference_image(≤9)/reference_video(≤3)/reference_audio(≤3)/first_frame/last_frame | **部分**——严格首尾帧走专用 role 模式；多模态参考模式下参考图可经 prompt「参考词」当首尾帧（软约束） | 参考视频 ≤3 段（单段限制未公布）；输出 4–15 s；分辨率至 4K；prompt 按「视频1/图片1」引用（类型内序号，不支持 asset id） | task_id 轮询（queued/running/succeeded/failed）+ callback；video_url 24 h 有效 | [金山云 Seedance 2.0 文档](https://docs.ksyun.com/documents/45628)、[火山方舟视频生成](https://www.volcengine.com/docs/82379/1366799)、[apiyi 镜像](https://docs.apiyi.com/api-capabilities/seedance2/video-generation) |
| **Vidu（生数）** | `POST https://api.vidu.cn/ent/v2/reference2video` | `subjects[].videos[]`（主体模式）或顶层 `videos[]`（非主体模式） | URL 或 **b64**（须带 `data:video/mp4;base64,` 前缀，解码后 <20 MB；URL ≤100 MB）；整 POST ≤20 MB | **视频主体仅 viduq2-pro 支持**：主体模式 1 段 ≤5 s；非主体模式「1 段 8 s 或 2 段 5 s」；images+videos 共享槽位（1–7 图 / 1–4 图） | mp4/avi/mov；≥128×128；宽高比 <1:4 或 >4:1 拒收 | task_id + state（created/queueing/processing/success/failed）+ callback_url（带签名算法） | [Vidu 参考生视频官方](https://platform.vidu.cn/docs/reference-to-video) |
| **Runway（Act-Two，Act-One 后继）** | `gen4_act_two`（性能捕捉端点） | 表演驱动视频（driving performance video）+ 角色 image/video | 三态：**URL**（视频 ≤32 MB）/ **Data URI**（视频 ≤16 MB 编码后 ≈12 MB 原始）/ ephemeral 上传 `runway://`（≤200 MB、24 h 有效，官方推荐大文件） | 角色+表演视频双输入即形态本身；Gen-3（act_one 所基）已弃用 | 表演视频约束见官方（人脸占比等）；Gen-4 References（图像 ≤10 张）另族 | 异步 task 轮询（dev.runwayml.com） | [Runway 开发者门户](https://dev.runwayml.com/)、[API 输入参数](https://docs.dev.runwayml.com/assets/inputs/)、[Act-Two 帮助页](https://help.runwayml.com/hc/en-us/articles/42311337895827-Performance-Capture-with-Act-Two) |

**形态归纳（对设计最要紧的四条）**：

1. **字段名收敛为「content[] + role」**：海螺 H3 与 Seedance 2.0 完全同构
   （`{type:"video_url", video_url:{url}, role:"reference_video"}` + prompt 按
   「视频1」引用）；Kling 是专用端点（image+video 双参考）；Vidu 把视频挂在
   subjects 下。→ 我们的 `reference_videos` 扩展字段可以一套字段翻译三家。
2. **视频 b64 普遍受限**：海螺官方「大文件建议 URL 或 mm_file，b64 膨胀 33%」；
   Seedance 只收 URL；Runway URL 32MB / Data URI 16MB / 临时上传 200MB；
   Vidu b64 <20MB。→ **注入形态必须给「非 b64」选项**（见 §5.2 推荐文件路径）。
3. **首帧图与视频参考的关系各家不同**：海螺/Seedance 互斥或软约束、Kling/Vidu/
   Runway 天然双输入。→ 生成链要按渠道能力声明分流（capability 标记），不能假设
   一定能同帧。
4. **动作与运镜语义都走「视频1」引用**：prompt 里写「按视频1的动作」或「按视频1
   的运镜」即可分别迁移（海螺 H3 官方博客示例即两种都给）。

### 1.2 A2 骨架参考（skeleton/pose）

#### 1.2.1 渠道侧：主流不收骨架图，骨架在服务端内部提取

| 渠道 | 骨架/姿态输入 | 说明 |
|------|---------------|------|
| Kling | **无直接骨架参数** | Motion Control 内部「分析骨骼结构与关节运动再映射」（[kling2-6.com 指南](https://kling2-6.com/en/blog/mastering-kling-motion-control-guide)）；输入形态=参考视频 |
| 海螺 H3 | 无 | 参考视频即动作源 |
| Seedance 2.0 | 无 | 同上 |
| **AnimateAnyone（阿里云百炼）** | **有，但形态是「动作模板 id」** | 三模型链：`animate-anyone-detect`（人物图合规检测，图 URL only <5MB）→ `animate-anyone-template`（**从参考视频提取动作模板** → template_id `AACT.xxx…`，同账号才可用）→ `animate-anyone-gen2`（`input.image_url` + `input.template_id` 生成）；异步 DashScope 轮询；输出比例仅 9:16 / 3:4；按视频秒计费 | 
| PixVerse 动作模仿 | 参考视频驱动 | 人物图 + 运动参考视频 → 动作迁移（[百炼 PixVerse](https://help.aliyun.com/zh/model-studio/)） |

→ **结论**：渠道侧「骨架参考」没有直接消费面；真要做显式骨架输入只有本地路线
（或把骨架当图传给支持多模态参考的渠道——语义弱，不推荐）。AnimateAnyone 的
「template_id」三段式是唯一接近姿态结构化的 API，可作为 P1 可选渠道适配。

来源：[AnimateAnyone 视频生成 API](https://help.aliyun.com/zh/model-studio/animateanyone-video-generation-api)、
[AnimateAnyone-template](https://www.alibabacloud.com/help/zh/model-studio/animate-anyone-template-api)、
[AnimateAnyone 快速入门](https://help.aliyun.com/zh/model-studio/animateanyone-quick-start/)。

#### 1.2.2 本地路线：SDXL/SD1.5 + ControlNet openpose/DWPose + AnimateDiff 在 3090 的可行性

| 项 | SD1.5 路线 | SDXL 路线 |
|----|-----------|-----------|
| 运动模块 | AnimateDiff v3（mm_sd_v15_v3）或 **AnimateDiff-Lightning**（字节，1/2/4/8 步蒸馏，比原版快 10 倍+，跨模型可挂 SD1.5 与 SDXL） | AnimateDiff-Lightning SDXL 档 / Hotshot-XL 运动模块（SDXL 原生 t2v） |
| ControlNet | control_v11p_sd15_openpose / **DWPose 版**（thibaud 控件） | SDXL openpose ControlNet（thibaud sdxl-controlnet-openpose-scribble 等） |
| 显存（3090 24GB） | ~10–12 GB（AnimateDiff+2×ControlNet 在 12GB 卡社区可跑实证） | ~16–20 GB（SDXL 基座 10–12 + 运动模块 + ControlNet） |
| 长视频 | Context Options（AnimateDiff-Evolved 滑窗）+ ControlNet 联动 | 同左，更吃显存 |
| 速度 | Lightning 4 步 512×720×16 帧秒级–十秒级；每次 spawn 新进程冷加载 10–30 s | 约为 SD1.5 的 1.5–2 倍耗时 |
| 生态 | ComfyUI AnimateDiff-Evolved + Advanced-ControlNet（[Kosinkadink](https://github.com/Kosinkadink/ComfyUI-AnimateDiff-Evolved)）；现成 vid2pose→AnimateDiff 工作流（[comfy.org Video to Pose Map](https://comfy.org/workflows/utility-openpose-video-dc73712c1842/)） | 同左 |

**结论**：3090 单卡两条路线都可行；**推荐 P2 先落 SD1.5+AnimateDiff-Lightning+
DWPose**（显存余量大、生态成熟、权重魔搭全可下），SDXL 档作为画质升级项。
与 vLLM（21.6GB）互斥沿用现有显存闸门（`vram_gate`），建议骨架档门槛
~12000 MiB（新常量）。

进阶备选（视频直接驱动、跳过骨架，同为开源 3090 可跑）：MimicMotion / MusePose /
Moore-AnimateAnyone（动作迁移族）——P2 若骨架路线效果好可不引入。

#### 1.2.3 骨架图从哪来（提取器）

- **DWPose**（`DW_openpose_full` 预处理器，[comfyui_controlnet_aux](https://github.com/Fannovel16/comfyui_controlnet_aux)）：
  当前事实标准——身体+手部+面部一体，手部显著优于旧 OpenPose；**支持视频逐帧
  提取**并输出 OpenPose 格式 JSON（每帧一条，可再渲染回骨架图）。
- **OpenPose**（经典）：腿脚检测比 DWPose 稳，复杂全身动作建议双提取器互补
  （社区惯例）。
- 手绘/上传：OpenPose 骨架图本身只是彩色线段小人（18/133 关键点），可手绘或用
  OpenPose Editor 类插件摆姿态——对「自定义动作」用户是低成本入口（画一张骨架
  = 定死一个动作帧）。
- **从参考视频提骨架 = P2 本地管线第一步**：视频 → DWPose 逐帧 → 骨架帧序列
  （png 序列或合成图）→ ControlNet 条件。这一步 CPU/GPU 都可跑（DWPose 约
  2–4 GB 显存或纯 CPU 慢速档），**可在 P1 就作为独立工具落地**（骨架预览）。

### 1.3 A3 运镜（camera control）

| 渠道 | 参数/机制 | 枚举与取值 | 运镜参考视频 |
|------|-----------|------------|--------------|
| **Kling** | `camera_control{type,config}`（图生视频端点） | type=simple 时 config 二选一形态：**5 预设枚举**（forward_up / forward_down / down_back / right_turn_forward / left_turn_forward）或 **6 轴数值**（horizontal/vertical/pan/tilt/roll/zoom，各 -10..10 int）。**仅 kling-v1 系 + std 模式 + 5s**；v2.1 起官方明确「不支持尾帧、运动笔刷、镜头控制」，替代=prompt 描述或 Motion Control | PMC `character_orientation=image` 时「更跟随参考视频运镜」（≤10s）——**间接支持** |
| **PixVerse** | `camera_control` 字符串（`POST /openapi/v2/video/img/generate`） | zoom_in/zoom_out/pan_left/pan_right/tilt_up/tilt_down/roll_clockwise/roll_anticlockwise（V4.5 参数化；V6 扩到 20+ 电影级控制：焦距/光圈/景深/追踪/环绕） | 动作模仿端点（图+运动视频）兼带 |
| **Seedance（即梦同源）** | 1.x：`camera_fixed` 布尔（true=固定镜头）；2.0/2.5：**去参数化**，prompt 描述词 + 参考视频运镜迁移 | 无运镜枚举；提示词指南给运镜词表（推/拉/摇/移/升/降/环绕/跟随…） | **支持**——「按视频1的运镜方式」prompt 引用 |
| **海螺 H3（MiniMax）** | API 无 camera 枚举参数；Director 系模型（T2V-01-Director / I2V-01-Director）网页端有摄像机图标运镜选择器（≈15 种指令 `[推镜头]``[环绕]`），API 侧等价物=prompt 运镜描述词 | 无 API 枚举；描述词惯例同上 | **支持**——H3 官方示例「Reference the Hitchcock camera movement from Video 1」 |
| **Runway** | Gen-3 时代 web 端 camera control；API 侧 Gen-4 无 camera 参数（prompt 驱动）；Act-Two 表演视频含镜头表演 | 无枚举 | Act-Two 表演视频天然含（无独立 camera 通道） |
| **Vidu** | `movement_amplitude: auto/small/medium/large`（运动幅度，非运镜方向）；q2/q3 上该字段惰性 | 无运镜枚举；运镜靠 prompt | 仅 viduq2-pro 视频主体参考可间接带 |

**归纳**：① 参数化运镜的「最大公约数」是 **8 个方向枚举**（Kling 6 轴与其互译；
PixVerse 8 枚举）；② 无参数渠道统一吃 **prompt 运镜描述词**（各家提示词指南
词表高度趋同：推/拉/摇/移/升/降/环绕/跟随/手持/特写…）；③ **运镜参考视频**
没有专用参数，但海螺 H3 / Seedance 2.0 的「视频1引用」+ Kling PMC 均可表达——
落在我们的「camera 子选项=枚举 | 自由描述 | 参考资产」三态上刚好。

来源：[Kling 图生视频 1.6](https://www.klingai.com/document-api/api/video/1-6/image-to-video)、
[Kling 2.1 Master camera](https://kling.ai/document-api/api/video/2-1-master)、
[胜算云 camera_control 参数说明](https://docs.router.shengsuanyun.com/7438181m0)、
[mcp-kling「V1 only」注记](https://github.com/199-mcp/mcp-kling)、
[Kling API Updates（v2.1+ 不支持运镜类控制）](https://kling.ai/document-api/updates/api)、
[PixVerse image-to-video](https://docs.platform.pixverse.ai/image-to-video-generation-13016633e0)、
[海螺 T2V-01-Director 运镜](https://zhuanlan.zhihu.com/p/21627196419)、
[I2V-01-Director 组合运镜](https://www.atyun.com/65765.html)、
[ComfyUI Kling Camera Controls 节点](https://docs.comfy.org/built-in-nodes/partner-node/video/kwai_vgi/kling-camera-controls)。

---

## 2. B 部分：我们引擎差距盘点（代码实况，2026-09-06 main）

### 2.1 定妆 actions 现状

| 面 | 代码位置 | 现状 |
|----|----------|------|
| actions 模板 | `apps/film/src/flow/castParts.ts:94-102` + 后端 `film_hub.rs:129` `CASTING_PART_KEYS` | mainSlot=`pose`（动作序列主帧）；4 个部件槽全是**文字预设**：姿态（站立/奔跑/腾跃/挥击/施法）、力度、方向、特效——无任何结构化参考资产概念 |
| card.md | `film_hub.rs:2644-2727` | front-matter 仅 `name/voice/portrait/parts`；`parts` 为单键 JSON 串（`{"姿态":"奔跑",…}`），round-trip 靠引号转义 |
| views | `film_hub.rs:2746`（清单）/ `4425` run_view_stage | 只产 png/jpg/webp（`sniff_image_ext` 三类魔数）；actions 的 pose 主帧也是一张静态图。**注意**：读白名单 `is_binary_ext`（`film_hub.rs:474`）已含 `mp4`——`casting/<type>/<name>/views/xx.mp4` 的 **GET 读面已天然放行**（前端 `hubPreviewKind` 也会给 video 预览），缺的只是写入口 |
| 视图导入 | `film_hub.rs:5949-6032` views/import | b64 JSON 信封 `{view, image_b64, mime}`；mime 白名单 png/jpeg/webp（`ext_for_mime`），≤`IMAGE_MAX_BYTES`=10MB（`film.rs:222`），魔数校验——**视频/骨架图进不来** |
| 树布局 | hub 树 `casting/actions/<name>/{card.md, views/}` | 与五类同构；无 media/ 子目录、无资产类型字段 |

### 2.2 生成链（图生视频）现状与注入缺口

`run_video_stage`（`film.rs:2485-2684`）→ `gen_video_channel`（`film.rs:2121-2175`）：

```text
请求体 = {model, prompt, image:"data:image/png;base64,…", image_base64,
          duration_secs}
        + 可选 reference_images:[<b64>]（出场「角色」定妆图）+ reference_strength
```

| # | 缺口 | 代码证据 |
|---|------|----------|
| 1 | **actions 定妆对象完全不进视频链**——`shot.actions[]`（storyboard.json 引用字段）只在 hub import 校验时对名（`film_hub.rs:6365-6377`），生成时既不拼 prompt 也不做参考注入 | `run_video_stage` 只读 `shot.video_prompt` 原文 + `collect_reference_b64`（仅 characters 的 portrait_ref，`film.rs:2280-2301`） |
| 2 | **无视频参考通道**——`reference_images` 仅 b64 图数组；无 `reference_videos` 字段 | `gen_video_channel` body 组装 `film.rs:2131-2146` |
| 3 | **无运镜参数**——video_prompt 文本里若 LLM 写了运镜词就随缘，没有结构化 camera_control 透传 | 同上 |
| 4 | **异步任务形态不支持**——响应无 url/b64 即 error（「可能为异步任务形态，暂不支持上游任务轮询」）；而海螺 H3/Seedance/Kling/AnimateAnyone 全是 task_id 轮询（§1.1 表）——**这是视频参考注入的前置依赖**（海螺/即梦/Kling 渠道原生就是异步） | `film.rs:2170-2174`；备案：`FILM_STUDIO.md` §4 三接法（聚合渠道/自建薄适配/via_node 联邦中继），`FILM_CONSISTENCY_RESEARCH.md` 风险 #7 |
| 5 | b64 体积压力——5s mp4（720p H.264）≈2–10 MB，b64 膨胀 ×4/3 后 2.7–13.4 MB；多段+首帧+参考图叠一个请求体，网关/上游 64MB 级 body 上限虽不在主分发面（`usize::MAX`），但上游渠道自身限制（海螺 64MB、Vidu 20MB、Runway Data URI 16MB）+ 内存/超时都在惩罚 b64 | `film_hub.rs:42-47`（body 上限链路结论）；渠道限制见 §1.1 表 |

### 2.3 资产上传面现状

| 面 | 现状 | 代码位置 |
|----|------|----------|
| b64 信封上限 | story 导入 64MB（env `NEXOS_FILM_SOURCE_MAX_MB`）；BGM mp3 20MB（`BGM_IMPORT_MAX_BYTES`）；定妆视图 10MB（`IMAGE_MAX_BYTES`）——**视频无任何入口**（cache 面 mp4 只能由生成端写入，上传仅 `shot-<n>.png/mp4` 转正 rename，无直传） | `film_hub.rs:210/256/5983`、`film.rs:222` |
| mime 白名单 | 视图导入 png/jpeg/webp；`is_binary_ext` 读面含 mp3/mp4；**写面无视频、无骨架 png 语义区分**（骨架 png 与普通图同mime，只能靠 view 名约定） | `film_hub.rs:474/5990-6005` |
| multipart | 全仓库无先例（网关契约 body 恒 JSON，multipart 入站即丢弃）——**视频上传沿用 b64 JSON 信封 + 分块可选** | `film_hub.rs:42-44` |

### 2.4 差距 Top 5（本批必须解决的排序）

1. **actions 子选项数据模型缺失**——只有 4 个文字部件槽，没有「动作参考资产
   （视频/骨架）」与「运镜」两个结构化维度（§2.1）。
2. **视频资产进不了树**——views/import mime 白名单 + 10MB 上限把 mp4 与骨架
   png（语义面）都挡在外面；虽然读面 mp4 已放行（§2.1/2.3）。
3. **生成链无 actions 注入**——shot.actions 引用与生成完全脱节；无
   reference_videos、无 camera 透传（§2.2 #1-3）。
4. **异步任务形态不支持**——视频参考主力渠道（海螺 H3/Seedance/Kling）原生
   全异步，不做 task_id 轮询则 P1 无法落地（§2.2 #4；上次已备案，本批一起）。
5. **b64 视频体积/渠道差异**——上游对视频 b64 普遍不友好（Seedance 只收 URL、
   Runway URI 16MB、Vidu 20MB），需要文件路径/URL 形态的注入设计（§2.2 #5）。

---

## 3. C 部分：actions 子选项数据模型设计

### 3.1 数据模型（card.md front-matter 扩展，向后兼容）

actions 类 card.md 在既有 `parts`（文字部件槽）之外新增两个**可选**结构化键
（其余五类不受影响；解析宽容——缺省/非法即空，与 `parse_parts_fm` 同哲学）：

```yaml
# casting/actions/<name>/card.md
---
name: 雨中拔剑
parts: {"姿态":"挥击","力度":"爆发","方向":"正面","特效":"雨雾"}   # 既有（文字预设）
motion:                                                            # ①动作参考（新）
  kind: video            # video | skeleton | none（缺省 none=纯文字档）
  asset: views/motion.mp4        # 相对对象目录（视频）；骨架=views/pose-seq/（帧序列目录）或 views/pose.png（单帧）
  source: import         # import | ai（assets.json 登记同口径）
  note: 武术指导参考片段   # 自由备注（拼 prompt 兜底）
camera:                                                            # ②运镜（新）
  kind: enum            # enum | describe | reference | none
  preset: orbit_right   # enum 档：8+ 枚举（下表）；与渠道 6 轴/8 枚举互译
  describe: 镜头从脚起摇升至面部特写，缓慢推近   # describe 档：自由描述词
  asset: views/cam.mp4  # reference 档：运镜参考视频（同 motion.asset 形态）
---
（正文=动作外形描述，沿用）
```

**运镜枚举表（最大公约数 8 项 + 组合语义）**：

| 我们枚举 | 语义 | Kling 6 轴互译 | PixVerse 枚举 | prompt 描述词（无参数渠道） |
|----------|------|----------------|----------------|------------------------------|
| push_in | 推近 | zoom>0（+forward） | zoom_in | 镜头缓缓推近 |
| pull_out | 拉远 | zoom<0 | zoom_out | 镜头缓缓拉远 |
| pan_left / pan_right | 左右摇 | pan ∓ | pan_left/pan_right | 镜头左摇/右摇 |
| tilt_up / tilt_down | 俯仰 | tilt ± | tilt_up/tilt_down | 镜头上升/下降 |
| roll_cw / roll_ccw | 滚转 | roll ± | roll_clockwise/anticlockwise | 画面顺/逆时针旋转 |
| orbit | 环绕 | pan+horizontal 复合 | V6 orbit | 镜头环绕主体 |
| follow | 跟随 | —（prompt） | — | 跟随镜头 |
| fixed | 固定 | 全 0 | —（Seedance camera_fixed=true） | 固定镜头 |

（组合档可留 v2：`preset: [push_in, tilt_up]`——海螺 Director 系明确支持多运镜
组合。）

### 3.2 资产类型扩展与树布局

```text
casting/actions/<name>/
├── card.md                  # 增 motion:/camera: front-matter（§3.1）
└── views/
    ├── pose.png             # 既有：动作序列主帧（首帧候选）
    ├── pose-v2.png          # 既有：主槽版本化沿用
    ├── motion.mp4           # 新：动作参考视频（is_slug_like 视图名沿用）
    ├── motion-skeleton/     # 新（P1/P2 可选）：DWPose 提取的骨架帧序列
    │   ├── 0001.png …       #   （或合成单图 pose-seq.png——先落单图档，序列 P2）
    └── cam.mp4              # 新：运镜参考视频（camera.kind=reference）
```

- `assets.json` 条目天然兼容（path/sha256/bytes/source/ref 通用），无需改结构。
- 前端：CastingPage actions Tab + CastCustomizer 增「动作参考」「运镜」两个
  子面板（选择/上传资产 + 运镜枚举下拉/自由描述/参考视频三态）；HubBrowse
  mp4 预览已支持（`hubPreviewKind`），树卡即得。
- 五槽位机制（front/side/back/action/custom）不动——motion.mp4 等走 custom 名
  空间或专用视图名（is_slug_like 校验沿用）。

### 3.3 分镜与生成链的绑定

- storyboard.json 的 `ScriptShot` 已有 `actions: Vec<String>`（按名引用）——
  生成时按名读 actions 对象 card，取其 motion/camera 配置注入（与 characters
  portrait 注入同构）；多 actions 对象时取第一个携带 motion 配置者（v1 限制，
  见 §7 风险 5）。
- video_prompt 组装顺序（P0）：
  `video_prompt（+ camera describe/枚举→描述词）+（+ motion.note）+ style_hint`
- camera 参数透传（P0 即做，仅 channel 档）：body 增可选
  `camera_control: {type:"simple", config:{…}}`（Kling v1 系原生形态原样透传，
  不识别的渠道忽略——与 reference_images 同一兼容原则）。

---

## 4. 生成链注入设计

### 4.1 video/generations 请求体扩展（在既有字段之上，全部可选）

```jsonc
{
  // 既有：model/prompt/image/image_base64/duration_secs
  //      + reference_images[<b64>]（角色定妆图）+ reference_strength
  // P0 新增：
  "camera_control": { "type": "simple", "config": { "zoom": 5, "pan": -3 } },
  // P1 新增：
  "reference_videos": ["<形态三选一，见 4.2>"],
  "motion_mode": "motion | camera | both"   // 告诉薄适配「视频1」引用语义侧重
}
```

### 4.2 视频参考注入形态：分块 b64 / URL / 文件路径 三选一（推荐文件路径）

| 形态 | 做法 | 优点 | 缺点 | 结论 |
|------|------|------|------|------|
| ① 整体 b64（沿用 reference_images 模式） | mp4 base64 进 JSON | 零新端点；聚合渠道直接吃 | 2-10MB→2.7-13.4MB/段；Vidu 20MB / Runway 16MB / Seedance 不收 b64；内存与超时惩罚 | 只留作小视频兜底 |
| ② URL | 先传公网图床/对象存储，body 只带 URL | 渠道兼容性最好（五家全收 URL） | 需要「资产外发到公网」服务（本项目 hub 树是本地文件系统，无现成图床）；联邦场景可走源节点 | 视部署形态，via_node 联邦中继可用 |
| **③ 本地文件路径（推荐）** | `reference_videos: ["file:///…/hub/casting/actions/xx/views/motion.mp4"]` 或相对 hub 根路径 `casting/actions/xx/views/motion.mp4` | 不膨胀 body；薄适配/本地路线（P2）读同一文件；`assets.json` 已有 path+sha256 可校验；跨进程（film→适配层）传递自然 | 直连官方 API 的渠道不认识——必须经薄适配/聚合层翻译成 URL/b64/上传 | **推荐**：与 FILM_STUDIO.md §4「自建薄适配」三接法天然咬合 |

**薄适配翻译表**（film 扩展字段 → 渠道原生）：

| film 扩展 | 海螺 H3（V2 content[]） | Seedance 2.0（content[]） | Kling（PMC 端点） | Vidu（q2-pro） | 聚合站 |
|-----------|--------------------------|----------------------------|--------------------|----------------|---------|
| `reference_videos[0]`（motion_mode=motion） | `{type:"video_url",video_url:{url},role:"reference_video"}` + prompt 追加「动作参考视频1」 | 同构 role=reference_video + 「视频1 的动作」 | 独立端点：`image_url`=首帧/人物图 + `video_url`=参考视频 + `character_orientation` | `videos[]`（或 subjects） | 原样透传（聚合站自翻译） |
| `reference_videos[0]`（motion_mode=camera） | 同上 + prompt「按视频1的运镜」 | 同上 | PMC `character_orientation=image`（跟随运镜档） | —（间接） | 同上 |
| `camera_control` | 忽略（无参数）→适配层转 prompt 描述词 | 1.x 模型转 `camera_fixed`；2.x 转 prompt | **原生透传**（仅 v1 系有效；v2.1+ 适配层降级 prompt） | 忽略/转 movement_amplitude | 原样透传 |

### 4.3 骨架注入：本地 ControlNet 主路 + 渠道透传备路

- **主路（P2 本地）**：`motion.kind=skeleton` 时——骨架帧序列 →
  AnimateDiff+ControlNet（DWPose）管线（新 `local.video` 档，子进程形态对齐
  `media_gen` 生图内核：脚本落盘 + env 传参 + 超时 kill + 显存闸门 ~12GB）；
  首帧可用 pose.png（定妆主帧）→ 图生视频语义（ControlNet 条件 + init image）。
- **备路（P1 可选）**：骨架序列先本地合成「骨架预览 mp4」（DWPose 渲染帧→
  ffmpeg 合成），再当普通 reference_videos 走渠道（Kling PMC/海螺 H3 都能从
  骨架视频里学到姿态）——**把骨架问题降维成视频问题**，零本地大模型。
- 骨架提取器（DWPose）独立小工具：P1 即可做「上传动作参考视频 → 生成骨架
  预览」端点（CPU 可跑），为 P2 铺路且当期就有交付价值（用户可视化确认动作）。

### 4.4 异步任务轮询适配（P1，与视频参考同批）

film 侧 `gen_video_channel` 增加「响应含 task_id → 进入上游轮询」分支：
- 轮询循环：间隔 5–10s、总预算 ≤ video_timeout（现 60–1800s 放宽口径沿用），
  状态机（queued/running/succeeded/failed → url 下载 → b64/字节归一）；
- 渠道差异经薄适配归一（适配层把各家轮询收敛成一次同步响应仍是最省事的第一
  接法——FILM_STUDIO.md §4 既定）；film 原生轮询作为「无适配层直连官方 API」
  的第二接法；
- 成本事件（CostSpec）照记，task 环形日志加「上游任务 <id> 状态 <s>」行。

---

## 5. 分期落地清单

### P0：actions 子选项数据模型 + 资产导入 + 运镜枚举注入（零模型下载）

1. **数据模型**：`CastingCard` 增 `motion`/`camera` 可选结构（serde 宽容）；card.md
   front-matter 写读（嵌套 YAML 用单键 JSON 串，与 parts 同 trick 保 round-trip）；
   PUT casting/:type/:name body 增两字段；前端 PART_TEMPLATES/actions 模板与
   CastCustomizer 增两子面板。
2. **资产导入**：views/import 放开 mime——视频 `video/mp4`（上限新常量
   `MOTION_VIDEO_MAX_BYTES` 建议 50MB 对齐海螺单段限制；魔数嗅探 ftyp；
   env 可调）+ 骨架 png（image/png 语义由 view 名 `pose-seq`/`motion-skeleton`
   约定，mime 不变）；`is_binary_ext` 已含 mp4 读面零改。
3. **运镜注入**：枚举表（§3.1）前后端同契约（跨端一致性测试沿用）；video 阶段
   组装：camera.kind=enum/describe → prompt 追加描述词；=reference → P1 前仅
   记日志「运镜参考视频待 P1 注入」；channel 档透传 `camera_control`（Kling v1
   系原生；其余渠道忽略）。
4. **分镜 LLM 提示词**：storyboard/generate 的 video_prompt 描述规范里加运镜
   词表引导（LLM 可先产文字运镜，用户再结构化覆盖）。
5. 测试：mock 渠道断言 camera_control 在线/离线两态；card round-trip；mp4 导入
   上下限与魔数；跨端枚举一致性。

### P1：视频参考注入渠道扩展 + 异步轮询适配 + 骨架预览工具

1. `reference_videos`（**文件路径形态**）+ `motion_mode` 扩展字段进
   `gen_video_channel`；actions 对象 motion 资产按 shot.actions 名解析注入。
2. 薄适配翻译表（§4.2）落地（海螺 H3 / Seedance 2.0 / Kling PMC 三家优先；
   聚合站透传档同时生效）。
3. film 原生上游 task_id 轮询分支（§4.4；FILM_STUDIO.md §4 更新为双接法）。
4. DWPose 骨架预览端点：上传/选中 motion.mp4 → 骨架帧序列落
   `views/motion-skeleton/` + 合成预览 mp4（ffmpeg 既有合成面）。
5. （可选）AnimateAnyone 百炼三段式渠道适配（detect/template/gen2——姿态最
   结构化的 API 形态）。

### P2：本地骨架生成路线（local.video 档）

1. 权重下载（魔搭）：SD1.5 AnimateDiff 档 ≈8–9 GB（SD1.5 base ~4GB + AnimateDiff
   模块 + openpose/DWPose ControlNet + Lightning 蒸馏可选）；SDXL 档 ≈13 GB。
2. 新视频子进程管线（对齐生图内核形态）：输入=首帧 pose.png + 骨架序列目录 +
   prompt；输出 mp4 落 `cache/shot-<n>.mp4`（cache/commit 转正链零改）。
3. `validate_model_ref` 放行 `local+video`；models.json 八能力位 `video` 的
   local 档接入；显存闸门新常量 ~12000 MiB（不动 media_gen 旧值）。
4. 每次生成 spawn 新进程冷加载 10–30s 的老问题在视频档更痛（生成本体分钟级
   可容忍）——常驻守护列为 P2 观察项（与一致性方案书 §8 #3 同结论）。

---

## 6. 契约草案（端点级）

```text
# P0
PUT    /api/v1/film/projects/:id/casting/actions/:name
       body 增 motion?{kind,asset?,note?} / camera?{kind,preset?,describe?,asset?}
POST   /api/v1/film/projects/:id/casting/actions/:name/views/import
       {view, video_b64, mime:"video/mp4"}     # mp4 新档：≤50MB(env)、ftyp 魔数
GET    /api/v1/film/projects/:id/casting/:type/:name            # 回传 motion/camera
POST   /api/v1/film/projects/:id/shots/:n/video
       body 不变（camera/motion 由 actions 对象配置解析，非请求级参数）

# video/generations 请求体（channel 档；全部可选、不识别即忽略）
       {…既有…,
        camera_control?: {type:"simple", config:{horizontal?,vertical?,pan?,tilt?,roll?,zoom?}},
        reference_videos?: ["casting/actions/<name>/views/motion.mp4"],   # P1：hub 相对路径
        motion_mode?: "motion"|"camera"|"both"}

# P1
POST   /api/v1/film/projects/:id/casting/:type/:name/skeleton/preview
       {asset:"views/motion.mp4"} → 202 任务 → views/motion-skeleton/0001.png… + preview.mp4
GET    /api/v1/film/tasks/:id                     # 既有轮询；日志增上游任务状态行

# P2（models.json video 位 local 档 + local.video 分流，无新端点）
```

---

## 7. 风险与开放问题

| # | 问题 | 影响 | 缓解/待决 |
|---|------|------|-----------|
| 1 | **异步渠道是 P1 硬前置** | 海螺 H3/Seedance/Kling/AnimateAnyone 全 task_id 轮询；不落地则视频参考只覆盖聚合站 | P1 双管：薄适配收敛同步响应（首选）+ film 原生轮询分支；本批方案已含 |
| 2 | **视频 b64/URL 渠道差异** | Seedance 只收 URL；Runway Data URI 16MB；Vidu 20MB | 注入形态走文件路径（§4.2③），翻译责任在薄适配；直连官方 API 的渠道暂不支持，文档如实标注 |
| 3 | **首帧图与视频参考互斥（海螺/Seedance 软硬约束）** | 我们链路默认 image_first=true | 渠道能力声明（channel 元数据加 `ref_video_mode: exclusive|compatible`）；互斥渠道注入时二选一（首帧优先，日志提示） |
| 4 | **Kling camera_control 仅 v1 系** | 参数化运镜在 v2.1+（主力画质档）不可用 | 枚举→描述词自动降级（适配层与 film prompt 组装双保险）；Kling PMC 参考视频做精控 |
| 5 | **多 actions 对象/多参考视频语义** | 一个镜头引用多个动作对象时注入歧义 | v1 取首个有 motion 配置者；v2 支持 reference_videos 多段 + prompt 按序引用（海螺/Seedance 的「视频N」机制天然支持） |
| 6 | **骨架序列的存储与版本化** | 帧序列目录 vs 主槽 png 版本化机制（-vN）不兼容 | 骨架序列独立于主槽版本化（views/motion-skeleton/ 整目录覆盖式更新）；assets.json 按目录登记聚合条目 |
| 7 | **本地 AnimateDiff 与 vLLM 互斥 + 冷加载** | 24GB 装不下并存；spawn 冷加载 10-30s | 沿用显存闸门（新阈值 12GB）+ 如实报错指引；常驻守护 P2 观察项 |
| 8 | **动作/运镜参考的版权合规** | 上传他人影片片段有侵权风险 | 与音频克隆同政策：用户自有/已授权素材；card.note 记录来源声明（字段预留） |
| 9 | **运镜枚举表的表达力** | 8 枚举覆盖不了复杂组合运镜 | describe 自由描述档兜底（P0 即有）；组合枚举 v2；海螺 Director 系多运镜组合对齐后续跟进 |
| 10 | **海螺 Director 系运镜选择器无 API 等价物** | 网页端 15 种运镜指令 API 侧拿不到 | prompt 描述词实测效果验证（开放项）；H3 参考视频运镜迁移为主要替代 |
| 11 | **mp4 上限与 duration_secs 关系** | 我们 duration 1..=60，而渠道参考视频普遍 ≤15s | motion.mp4 是参考素材（渠道各自限制 2–15s），上传上限 50MB 但**注入前按渠道裁剪提示**（P1 适配层做检查+日志） |
| 12 | **AnimateAnyone template_id 跨账号限制** | 模板仅同云账号可用 | 若接入则 template 生成与视频生成同渠道同 key；或仅文档化不接入 |

---

## 8. 参考链接汇总

**视频参考（A1）**：
[MiniMax V2 video_generation](https://platform.minimax.io/docs/api-reference/video-generation-v2-create) ·
[MiniMax H3 指南](https://platform.minimax.io/docs/guides/video-generation) ·
[fal H3 reference-to-video](https://fal.ai/models/minimax/h3/reference-to-video) ·
[Kling Motion Control 3.0](https://kling.ai/document-api/api/video/motion-control) ·
[fal Kling v3 pro motion-control](https://fal.ai/models/fal-ai/kling-video/v3/pro/motion-control/api) ·
[金山云 Seedance 2.0](https://docs.ksyun.com/documents/45628) ·
[火山方舟视频生成](https://www.volcengine.com/docs/82379/1366799) ·
[Vidu 参考生视频](https://platform.vidu.cn/docs/reference-to-video) ·
[Runway 开发者门户](https://dev.runwayml.com/) ·
[Runway API 输入参数](https://docs.dev.runwayml.com/assets/inputs/) ·
[Act-Two 表演捕捉](https://help.runwayml.com/hc/en-us/articles/42311337895827-Performance-Capture-with-Act-Two)

**骨架（A2）**：
[AnimateAnyone API（百炼）](https://help.aliyun.com/zh/model-studio/animateanyone-video-generation-api) ·
[AnimateAnyone-template](https://www.alibabacloud.com/help/zh/model-studio/animate-anyone-template-api) ·
[AnimateDiff-Lightning（字节）](https://huggingface.co/ByteDance/AnimateDiff-Lightning) ·
[AnimateDiff-Lightning 论文](https://arxiv.org/html/2403.12706v1) ·
[ComfyUI-AnimateDiff-Evolved](https://github.com/Kosinkadink/ComfyUI-AnimateDiff-Evolved) ·
[comfyui_controlnet_aux（DWPose）](https://github.com/Fannovel16/comfyui_controlnet_aux) ·
[Video to Pose Map 工作流](https://comfy.org/workflows/utility-openpose-video-dc73712c1842/) ·
[ComfyUI Pose ControlNet 教程](https://docs.comfy.org/zh/tutorials/controlnet/pose-controlnet-2-pass) ·
[12GB 跑 AnimateDiff+2×ControlNet（Reddit）](https://www.reddit.com/r/StableDiffusion/comments/17sanlt/12gb_enough_for_animatediff_2_controlnet/)

**运镜（A3）**：
[Kling 图生视频 1.6（camera_control）](https://www.klingai.com/document-api/api/video/1-6/image-to-video) ·
[Kling 2.1 Master（运镜枚举）](https://kling.ai/document-api/api/video/2-1-master) ·
[Kling API Updates（v2.1+ 不支持运镜类控制）](https://kling.ai/document-api/updates/api) ·
[mcp-kling（camera V1 only 注记）](https://github.com/199-mcp/mcp-kling) ·
[胜算云 Kling camera_control](https://docs.router.shengsuanyun.com/7438181m0) ·
[ComfyUI Kling Camera Controls 节点](https://docs.comfy.org/built-in-nodes/partner-node/video/kwai_vgi/kling-camera-controls) ·
[PixVerse image-to-video](https://docs.platform.pixverse.ai/image-to-video-generation-13016633e0) ·
[海螺 T2V-01-Director 运镜](https://zhuanlan.zhihu.com/p/21627196419) ·
[I2V-01-Director 组合运镜](https://www.atyun.com/65765.html) ·
[Seedance 提示词指南](https://docs.volcengine.com/docs/82379/1631633) ·
[Runware H3 运动运镜表演指南](https://runware.ai/docs/models/minimax-h3/guides/motion-camera-performance) ·
[MiniMax H3 博客（运镜引用示例）](https://www.minimax.io/blog/minimax-h3)

**仓库内部**：
`docs/FILM_STUDIO.md`（渠道约定/§4 异步三接法） ·
`docs/research/FILM_CONSISTENCY_RESEARCH.md`（reference_images 既定原则/异步备案） ·
`crates/os-api/src/handlers/film_hub.rs`（定妆/card/views/导入白名单） ·
`crates/os-api/src/handlers/film.rs`（gen_video_channel/run_video_stage/IMAGE_MAX_BYTES） ·
`apps/film/src/flow/castParts.ts`（PART_TEMPLATES actions） ·
`apps/film/src/flow/flowFiles.ts`（CAST_VIEW_SLOTS/hubPreviewKind）
