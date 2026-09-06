# 影片制作——角色/音频一致性技术方案书（2026-09-04 调研）

> 面向零上下文读者：读完本文即可理解 film 引擎「人物不变样、声音不变样」的
> 完整技术方案——现状、可选路线的量化对比、分阶段落地计划、端点契约草案与
> 风险清单。本文为**纯调研产物**（只读仓库 + 官方文档核实 + 本机环境检查），
> 不含任何已实施的代码改动。
>
> 背景契约：`docs/FILM_STUDIO.md`（film 引擎 14 条端点 / model_ref / 渠道约定）、
> `docs/APPS.md`（应用包与引擎门控）。后端实现：
> `crates/os-api/src/handlers/film.rs`（4144 行）、
> `crates/os-api/src/handlers/media_gen.rs`（2476 行）。

---

## 0. 一页结论（推荐路线）

**用户需求原话**：影片制作缺——人物效果、导入参考、人物定妆、约束同一人物在
不同分镜不变样；音频（配音）同一角色声音要一致。

**推荐三阶段路线**（P0 全部零权重下载、零本地推理改动，一周量级；P1/P2 按需）：

| 阶段 | 一句话 | 依赖 |
|------|--------|------|
| **P0 角色库 + 渠道参考注入 + voice 透传** | 项目内建角色库（定妆图上传/生成 + 三视图），script 每镜头绑定角色 id；渠道 image/video 调用新增**可选** `reference_images` 扩展字段（不破坏 OpenAI 形态，不识别的服务端忽略）；TTS 的 `voice` 从硬编码 `"alloy"` 改为**按角色透传**（枚举或渠道 voice_id）。弱一致兜底：同一角色固定「外观描述 token + 固定 seed」拼进 image_prompt（零成本，现有管线即可做） | 仅 os-api 代码；无任何模型下载 |
| **P1 本地定妆真注入（IP-Adapter 档）** | `NEXOS_SD_MODEL` 指向 SDXL-base fp16 + **IP-Adapter plus-face（ViT-H）**加载定妆图；可选叠 SDXL-Lightning 4 步 LoRA 保速度。**不换 sd-turbo 上打补丁**（官方 IP-Adapter 权重只有 SD1.5/SDXL，sd-turbo=SD2.1 蒸馏，不兼容） | 权重下载 ≈10.3 GB（魔搭可下）；生图脚本加分支 |
| **P2 本地声音克隆档（local.tts）** | CosyVoice2-0.5B（首选，零样本 3–10s 参考音频即克隆，3090 首包 ~150ms）或 GPT-SoVITS v2（加 5 分钟微调音色更稳）。作为 `local.tts` 能力档接入 film 的 model_ref 分流矩阵（现在 local 仅 chat/image） | 权重下载 3.1–5 GB；新增 TTS 子进程管线 |

**为什么这个顺序**：一致性问题的 80% 价值在「渠道 + 角色库 + 数据模型」——
主流国产视频/图像渠道已原生提供主体参考注入（Seedream `image` 数组、MiniMax
`subject_reference`、Vidu `subjects`+`@name`），把定妆图**上传并绑定到分镜**后，
渠道侧自动获得跨分镜一致性；本地 IP-Adapter 档是断网/免费场景的补充，而非前提。

---

## 1. 现状盘点（代码实况，2026-09-04 main）

### 1.1 本机环境（106 节点实查）

| 项 | 实测值 |
|----|--------|
| GPU | RTX 3090 24 GB（驱动 595.84 / CUDA 13.2） |
| 当前显存占用 | 21.7/24.5 GB（vLLM 实例占 21.6 GB）——**与本地生图互斥**，靠显存闸门保证 |
| 内存/磁盘 | 61 GB RAM；`/` 1.6 TB 空闲 |
| 本地模型 | `/tank/models/sd-turbo`（4.0 GB 实测 du）；SenseNova-U1.5-8B-MoT |

### 1.2 生图内核（media_gen.rs）

- 管线：`IMGGEN_SCRIPT_PY`（media_gen.rs:142）——diffusers
  `AutoPipelineForText2Image`，fp16，**纯文生图，无任何参考图入口**；
  模型路径 env `NEXOS_SD_MODEL`（缺省 `/tank/models/sd-turbo`）。
- 参数面：prompt + width/height（256..=1024、64 步进）+ steps（1..=8）；
  film 固定 `steps: 4`（film.rs:1330）。
- 闸门：空闲显存 < 6000 MiB 拒绝（`VRAM_FREE_MIN_MIB`，media_gen.rs:106）；
  超时缺省 60 s（钳 1..=300）；**每次生成 spawn 新 python 进程**（模型冷加载
  含在每次生成内）。
- film 复用面：`probe_vram_free_mib_with` / `vram_gate` / `ensure_imggen_script`
  / `run_imggen_with` 已 pub(crate)（film.rs:1301-1334 直接调用）。

### 1.3 film 引擎与一致性相关的四个缺口

| 缺口 | 代码位置 | 现状 |
|------|----------|------|
| ① 无角色数据模型 | `ScriptShot`（film.rs:261） | 只有 `{shot, desc, image_prompt, video_prompt, line, duration_secs}`——**没有角色绑定字段**；人物全靠 image_prompt 文字描述，跨分镜必漂 |
| ② 无参考图注入 | `run_image_stage`（film.rs:1584-1592） | prompt = `image_prompt + "，" + style_hint` 直接送 sd-turbo / 渠道文生图；`gen_image_channel`（film.rs:1345）只发 `{model,prompt,size,response_format}` |
| ③ TTS 音色硬编码 | `tts_channel`（film.rs:1426） | `"voice": "alloy"` 写死——所有角色同音色；无 voice 透传 |
| ④ 无上传入口 | film.rs 全文 | 无 multipart/b64 上传端点；参考图、定妆图无处可传 |

图生视频已有首帧注入（`gen_video_channel` film.rs:1386 发
`image: "data:image/png;base64,…"` + `image_base64`），但**只有首帧、没有
角色主体参考**——首帧对了，人物在后续镜头仍会变。

### 1.4 上传先例（C 部分结论：有现成形态可抄）

- **`POST /api/v1/files/upload`**（files.rs:99-108, 160-164）：JSON 体
  `{filename, content_base64}`（标准字母表 b64、无 data: 前缀），admin 鉴权，
  重名自动加后缀、防穿越、大小闸门。**全仓库无 multipart 先例**（axum
  multipart 未在任何 handler 使用）——refs 上传应沿用 **b64 JSON** 形态。
- **live.rs** 中继帧：1 MiB 分块 base64（live.rs:109）——大文件 b64 的体积
  信封余量先例（b64 膨胀 ×4/3）。
- 响应面：files.rs download 用 `{encoding:"base64", content_base64}` 信封。

---

## 2. A 部分：角色视觉一致性

### 2.1 本地路径调研（问题 A1）

#### 2.1.1 关键事实：sd-turbo 上做不了参考图注入

- 官方 IP-Adapter 权重（tencent-ailab/IP-Adapter）**只发布了 SD1.4/1.5 与
  SDXL 1.0 两族**，SD2.1 不在列表；
  [diffusers#9528](https://github.com/huggingface/diffusers/issues/9528) 专门
  讨论蒸馏模型（sd-turbo）的 `load_ip_adapter` 兼容性——SDXL-Lightning/Hyper
  可开箱即用，**sd-turbo（SD2.1 蒸馏）无官方适配权重**，社区魔改不可靠。
- **InstantID**：[官方仓库](https://huggingface.co/InstantX/InstantID)明确
  **SDXL only**，且需 InsightFace `antelopev2` 人脸库 + InstantID ControlNet +
  IP-Adapter 三件套；社区实测整机显存 **12–20 GB**
  （[stable-diffusion-art 实测近 20GB](https://stable-diffusion-art.com/instantid/)、
  [Clore 基础档 12GB](https://docs.clore.ai/guides/face-and-identity/instantid)）。
- **PhotoMaker V2**（TencentARC）：SDXL 基座 + 自带人脸编码器
  （`photomaker-v2.bin` **1.8 GB**），显存约 **11 GB**，ID 保真好且**官方声明
  兼容 SDXL-Lightning 少步数加速**
  （[GitHub](https://github.com/TencentARC/PhotoMaker)、
  [HF 权重页](https://huggingface.co/TencentARC/PhotoMaker-V2/tree/main)）。

**结论：要做真注入必须换 SDXL 基座；sd-turbo 只保留为「档 1 弱一致」的文生图底座。**

#### 2.1.2 权重体积与下载（魔搭实据，2026-09-04 实测 API 拉取文件清单）

SDXL base 1.0（[AI-ModelScope/stable-diffusion-xl-base-1.0](https://modelscope.cn/models/AI-ModelScope/stable-diffusion-xl-base-1.0)）：

| 文件 | 字节数 |
|------|--------|
| 单文件 `sd_xl_base_1.0.safetensors` | 6,938,078,334（≈6.46 GiB） |
| diffusers fp16 四件合计 | unet 5.135 GB + te 246 MB + te2 1.389 GB + vae 167 MB ≈ **6.94 GB** |

IP-Adapter（[soulteary/h94-IP-Adapter 魔搭镜像](https://modelscope.cn/models/soulteary/h94-IP-Adapter/)，字节数为 API 实测）：

| 文件 | 字节数 | 用途 |
|------|--------|------|
| `models/ip-adapter_sd15.safetensors` | 44,642,768 | SD1.5 基础档 |
| `models/ip-adapter-plus-face_sd15.safetensors` | 98,183,288 | SD1.5 人脸档 |
| `models/image_encoder/model.safetensors`（ViT-H） | 2,528,373,448 | SD1.5 与 SDXL-vit-h 系**共用** |
| `sdxl_models/ip-adapter_sdxl.safetensors` | 702,585,376 | SDXL 基础档（配 bigG 编码器 3.69 GB） |
| `sdxl_models/ip-adapter-plus-face_sdxl_vit-h.safetensors` | **847,517,512** | **推荐档**：SDXL 人脸，配 ViT-H 编码器（2.53 GB，比 bigG 省 1.16 GB） |

**P1 档总下载 ≈ 10.3 GB**（SDXL fp16 6.94 + ViT-H 编码器 2.53 + plus-face
适配器 0.85），魔搭全可下，3090 节点磁盘 1.6 TB 无压力。

#### 2.1.3 显存与速度权衡（4 步 turbo vs 30 步 base）

| 方案 | 权重盘上 | 显存峰值（3090 实测量级） | 单张速度 | 一致性强度 |
|------|---------|--------------------------|----------|-----------|
| 档1：sd-turbo，prompt 描述 + 固定 seed | 4.0 GB（已装） | ~4–5 GB | 4 步，512² 秒级（<2 s） | ★☆☆ 同 prompt+seed 仅统计相似，跨分镜服装发型必漂 |
| 档2：SDXL fp16 + IP-Adapter plus-face（vit-h），配 Lightning 4 步 LoRA | ~11.2 GB（10.3 + LoRA 0.4） | ~11–14 GB（1024²） | Lightning 4 步 ≈1–2 s；30 步 base ≈4–8 s | ★★★ 定妆图真注入，同人脸跨分镜稳定 |
| 档2'：SDXL + PhotoMaker V2（+Lightning） | ~8.7 GB（6.94+1.8） | ~11 GB | Lightning 4 步可用 | ★★★ 与档2 二选一，人脸档更强、非人脸（二次元/物件）不如 IP-Adapter |
| 档3：InstantID（SDXL+ControlNet+antelopev2） | +~4 GB | **12–20 GB** | 30 步，最慢 | ★★★+ 单图换脸最强，但最重最慢，本期不推荐 |
| 渠道（Seedream/Vidu 等，见 2.2） | 0 | 0 | 渠道侧 | ★★★–★★★★（商业模型） |

> 显存推算口径：SDXL fp16 推理 1024² 约 10–12 GB（社区公认区间）；
> ViT-H 图像编码器 fp16 常驻约 +1.3 GB；IP-Adapter 权重 fp16 +0.85 GB。
> **3090 24 GB 单卡放得下档2 全家**，但与 vLLM（21.6 GB）互斥——沿用现有
> 显存闸门即可，建议档2 时把门槛从 6000 MiB 提到 ~11000 MiB（新常量，不改
> media_gen 旧值，避免影响 media_gen 自身的 sd-turbo 档）。

#### 2.1.4 推荐档位

- **档1（P0 内置，零成本）**：角色卡带 `canonical_prompt`（外观 token 串：
  发型/发色/服装/年龄段……），生成时拼接 + 固定 `seed`。不改任何模型管线，
  一致性弱但聊胜于无；同时它是档2/渠道的 prompt 基座。
- **档2（P1）**：SDXL-base fp16 + `ip-adapter-plus-face_sdxl_vit-h` +
  SDXL-Lightning 4 步 LoRA。理由：单权重族通吃（vs PhotoMaker 只管人脸）、
  魔搭全可下、3090 显存充裕、Lightning 保住「秒级出图」体验；diffusers 用法
  `pipe.load_ip_adapter(...)` + `ip_adapter_image=定妆图`，与现有
  IMGGEN_SCRIPT_PY 同构（新增 env：`NEXOS_IP_ADAPTER` / `NEXOS_IP_IMAGE` /
  `NEXOS_IP_SCALE`）。
- **档3（渠道，与档2 并行存在）**：见下。

### 2.2 渠道路径调研（问题 A2）——4 家官方文档核实

| 渠道 | 端点 | 参考图字段（官方原名） | 形态与限制 | 来源 |
|------|------|------------------------|-----------|------|
| **火山方舟 Seedream（即梦同源）** | `POST {base}/api/v3/images/generations` | **`image`**：string 或 array | 参考图 URL 或 **Base64**；Seedream 4.0/即梦4.0 单次**最多 10 张**做多图融合/组图；另有 `sequential_image_generation`（"auto"/"disabled"）+ `sequential_image_generation_options.max_images`；响应 `data[0].url`/`b64_json`，`seed`/`watermark`/`response_format` 与 OpenAI 同名 | [火山方舟图片生成API](https://docs.volcengine.com/docs/ark/image-generation-api?lang=zh)、[即梦4.0 产品文档](https://www.volcengine.com/docs/85621/1820192)、[Seedream 调用文档（镜像）](https://www.yunqi.tech/documents/aigw_volcengine_seedream_calling) |
| **MiniMax 海螺** | `POST /v1/video_generation` | **`subject_reference`**：数组 `{type:"character", image:[...]}` | 仅 `model="S2V-01"`（主体参考视频）；image 为 URL（b64 支持待验）；图 ≤20 MB、短边 >300px、宽高比 2:5..5:2；单主体；prompt ≤2000 字；task_id 异步轮询 | [MiniMax S2V 官方文档](https://platform.minimaxi.com/docs/api-reference/video-generation-s2v) |
| **Vidu（生数科技）** | `POST https://api.vidu.cn/ent/v2/reference2video` | **`subjects`**：数组 `{name, images[≤3], voice_id?}` | prompt 里 **`@name` 语法引用角色**（如 `"@1 和 @2 在一起吃火锅"`）；主体（图/文）≤7；b64 需带 content-type 前缀且解码后 <20 MB；prompt ≤5000 字；**subject 还可带 `voice_id`（音色/克隆 id）——视听一致一起给了**；task_id/state 异步 | [Vidu 参考生视频文档](https://platform.vidu.cn/docs/reference-to-video) |
| **可灵 Kling（快手）** | `POST /v1/videos/image2video`（v1.6） | **`image`**（首帧，b64/URL，jpg/png ≤10 MB、≥300×300）+ **`image_tail`**（尾帧） | `model_name` 枚举 kling-v1…v2-6；v3.0 Omni 支持多图参考输入；纯 b64 **不加 data: 前缀**；`notify_hook` 回调、task_id 轮询 | [Kling 图生视频文档](https://kling.ai/document-api/api/video/1-6)、[Kling 3.0 Omni](https://kling.ai/document-api/api/video/3-0-omni/image-to-video)、[可灵图像生成（百炼镜像）](https://help.aliyun.com/zh/model-studio/kling-image-generation-api-reference) |

另：**通义万相**文生图 v1 支持 `ref_img`（垫图，≤10 MB，风格/主体参考）
（[阿里云万相 API](https://help.aliyun.com/zh/model-studio/text-to-image-api-reference)）。

**形态归纳**：字段名三派——`image`（数组化，Seedream/Kling）、
`subject_reference`（MiniMax）、`subjects`+`@name`（Vidu，带提示词内引用语法，
表达力最强且唯一同时挂 `voice_id`）。全部是 **OpenAI `images/generations`
请求体的超集**：额外顶层可选字段，标准服务端会忽略或由薄适配翻译。

#### film 渠道调用扩展字段建议（不破坏标准 OpenAI 形态）

```jsonc
// images/generations 请求体（现有 {model,prompt,size,response_format} 之上追加）
{
  "model": "...", "prompt": "...", "size": "1272x720", "response_format": "b64_json",
  "reference_images": ["<b64 或 data URI>", "…"],   // 可选：定妆图/三视图/参考图（顺序即优先级）
  "reference_strength": 0.8                          // 可选 0..1，映射 IP-Adapter scale / 渠道 strength
}

// video/generations 请求体（现有 {model,prompt,image,image_base64,duration_secs} 之上追加）
{
  "reference_images": ["<b64>"],                     // 可选：主体参考（与首帧 image 语义分离：image=首帧，reference_images=角色身份）
  "subject_type": "character"                        // 可选：主体类别
}

// audio/speech 请求体（见 §3.1）
```

- 不识别 `reference_images` 的渠道**自然忽略**→行为与今天完全一致（与
  film.rs:1151 现有先例同思路：channel 侧不加本地私有 kwargs，防严格服务端
  拒绝——扩展字段走同一原则：**发给上游，但失败不因字段被拒而误报**，可按
  渠道 `provider` 灰度）。
- 薄适配翻译表（渠道原生形态 ↔ film 扩展字段）：`reference_images[0]` →
  Seedream `image` 数组 / Kling omni 多图；`reference_images` → MiniMax
  `subject_reference.image` / Vidu `subjects[].images`（并自动把
  `@{character.name}` 拼进 prompt）；聚合站渠道则原样透传。

### 2.3 业界工作流（问题 A3）

「LibTV 类」AI 影片产品（libTV 一键出片、即梦+豆包+剪映短剧流、智剧通 ZJT、
文镜画师等）的共同管线（公开教程/开源项目归纳）：

1. **人物定妆**：先为每个角色生成/上传一张定妆图（正脸半身，光影干净）；
   再派生**三视图**（正面/侧面/背面 turnaround）——用于后续不同机位的分镜。
   即梦的做法是「智能参考/主体一致性」：定妆图作为参考图注入后续所有生图。
2. **分镜与角色绑定建模**：剧本拆解成镜头表后，**每个镜头声明出场角色**，
   生成该镜头静帧时把绑定角色的定妆图全部注入（多主体参考），prompt 用
   角色名/编号引用（对应 Vidu 的 `@name` 机制）；开源项目
   [ZJT 智剧通](https://github.com/jeffstric/ZJT)把这个环节叫「角色形象锁定」，
   专门解决「脸崩」。
3. **定妆图的三种用法**（我们的方案三种都接）：
   ① 本地 IP-Adapter 输入（`ip_adapter_image`，档2）；
   ② 渠道主体参考注入（`reference_images` → `image`/`subject_reference`/`subjects`）；
   ③ 兜底拼 prompt（角色 canonical_prompt + 风格 hint，档1）。

来源：[AI 短剧全流程教程（B站）](https://www.bilibili.com/video/BV1pSLJ6vEu3/)、
[3步做出一整部AI短剧（YouTube）](https://www.youtube.com/watch?v=seAmKpXfOK8)、
[可编辑分镜工具选型（CSDN）](https://blog.csdn.net/2603_96735916/article/details/163920762)、
[角色一致性工具实测（知乎）](https://zhuanlan.zhihu.com/p/2072489923572144060)。

---

## 3. B 部分：音频（配音）一致性

### 3.1 TTS 声线一致性的三层机制

| 层 | 机制 | 渠道约定实据 |
|----|------|--------------|
| L1 枚举音色（零成本基础一致） | 同一角色固定同一 voice 枚举值 | OpenAI `/v1/audio/speech`：voice 枚举 **11 个**——`alloy, ash, ballad, coral, echo, fable, onyx, nova, sage, shimmer, verse`（tts-1/tts-1-hd 支持其中 9 个，无 ballad/verse）；`gpt-4o-mini-tts` 另支持 `instructions` 自然语言引导（[OpenAI TTS 文档](https://developers.openai.com/api/docs/guides/text-to-speech)）。film 现状是硬编码 `alloy`（film.rs:1426）→ **改为按角色透传即得 L1** |
| L2 渠道克隆音色（voice_id） | 上传参考音频换一个 id，之后所有合成带该 id | ElevenLabs：`POST /v1/voices/add`（multipart `name`+`files[]`）→ 返回 `voice_id` → `POST /v1/text-to-speech/{voice_id}`（[官方文档](https://elevenlabs.io/docs/api-reference/voices/ivc/create)）；国内渠道（MiniMax 语音等）同构 voice_id 模式 |
| L3 零样本参考音频直传 | 请求体直接带参考音频+其文本 | GPT-SoVITS `api_v2 /tts`：`{text, text_lang, ref_audio_path, prompt_text, prompt_lang, speed_factor, seed, media_type, streaming_mode}`（[api_v2.py](https://github.com/RVC-Boss/GPT-SoVITS/blob/main/api_v2.py)）；CosyVoice2 零样本同构（prompt wav + prompt text）；AIGCPanel 实测 **3–10 秒参考音频即可克隆** |

#### film 渠道 TTS 扩展字段建议

```jsonc
// audio/speech 请求体（现有 {model,input,voice,response_format} 之上）
{
  "model": "...", "input": "台词…", "response_format": "mp3",
  "voice": "onyx",              // 已有字段：改为按角色透传（枚举，L1）
  "voice_ref": "el-voice-xxx",  // 可选：渠道侧克隆 voice_id（L2；优先级高于 voice）
  "ref_audio_b64": "<b64>",     // 可选：零样本参考音频（L3；薄适配写成临时文件→GPT-SoVITS ref_audio_path）
  "ref_audio_text": "参考音频说的话", // 可选：配 ref_audio_b64（prompt_text/prompt_lang）
  "instructions": "低沉沙哑、中年男性、语速慢" // 可选：gpt-4o-mini-tts 引导
}
```

同一字段三元组 `voice → voice_ref → ref_audio_b64` 优先级递增，渠道按自身
能力取用，不识别即忽略——与 §2.2 扩展字段同一兼容原则。

### 3.2 本地 TTS（问题 B2）：3090 可跑性实据

| 项 | CosyVoice2-0.5B | GPT-SoVITS v2 |
|----|-----------------|---------------|
| 权重体积 | **魔搭 iic/CosyVoice2-0.5B 实测**：llm.pt 2.02 GB + flow.pt 450 MB + hift.pt 83 MB + speech_tokenizer_v2.onnx 496 MB ≈ **3.1 GB 核心**（另 CosyVoice-BlankEN 分词器 988 MB 可选） | v2 预训练（gsv-v2final-pretrained）+ G2PW 文本前端 + hubert，HF lj1995/GPT-SoVITS，合计 **~2–3 GB**；一键全环境 ~40 GB（含训练链）为上限口径 |
| 显存 | 推理 ~4–6 GB；**3090 流式首包 ~150–160 ms**（[社区实测](https://adg.csdn.net/694cf5655b9f5f31781aaac0.html)） | 推理 ≥4–6 GB；**微调训练 8–12 GB**（8 GB 下限，[实战指南](https://cloud.baidu.com/article/4033243)） |
| 克隆质量 | 零样本 3–10 s 参考音频；MOS 5.53 接近商用；流式音色相似度略降（[GitHub #1396](https://github.com/QwenAudio/CosyVoice/issues/1396)）；支持 instruct 指令（笑声/停顿）与跨语种 | 5 s 参考音频零样本约 95% 相似（百度实测口径）；**加 ~5 分钟目标音色微调后最稳**——角色音色「录制一次、终身一致」的最强本地方案 |
| 接口形态 | python 库 / webui；CLI 零样本 `prompt_text+prompt_wav` | `api_v2.py` HTTP `/tts`（上面 L3 字段即原生形态），端口缺省 9880 |
| 3090 结论 | **值得作为 local.tts 档**：显存与 sd-turbo 同量级、体量 3 GB、质量商用级 | **值得作为 local.tts 进阶档**：推理门槛低；微调期显存 8–12 GB 可与生图错峰 |

**建议**：P2 先接 CosyVoice2（零样本即用、无训练环节、与现有「spawn python
子进程 + env 传参」内核同构）；GPT-SoVITS 作为可选进阶（每角色微调 LoRA 化
音色）。`local.tts` 接入点即 film 的 model_ref 分流矩阵
（validate_model_ref 现对 `local+tts` 直接 400，改为放行到新管线）。

---

## 4. C 部分：导入参考——上传管线与目录布局

### 4.1 上传端点设计素材（现有先例）

- **形态**：沿用 files.rs 的 **b64 JSON**（`{filename, content_base64}`）——
  仓库无 multipart 先例，网关转发面（forward_channel String 化）对 JSON 最友好；
  b64 膨胀 4/3，10 MB 参考图 ≈ 13.4 MB 请求体，可接受（live 中继帧即 b64 先例）。
- **鉴权**：读公开 / 写 admin（film 现行约定一致）。
- **大小闸门**：图片单张建议 ≤10 MB（对齐 Kling 渠道上限）；b64 解码后校验
  PNG/JPEG/WebP 魔数（复用 `sniff_audio_bytes` 的 sniff 思路）。

### 4.2 端点契约草案（新增，全部挂在现有门控/任务框架下）

```text
POST   /api/v1/film/projects/:id/characters                      {name, desc_prompt?, voice?{mode:"enum"|"voice_ref"|"clone", voice?, voice_ref?, ref_audio_ref?}} → 201 Character
GET    /api/v1/film/projects/:id/characters                      → Character[]
GET    /api/v1/film/projects/:id/characters/:cid                 → {character, refs:[…]}
PUT    /api/v1/film/projects/:id/characters/:cid                 部分更新（缺省保留）
DELETE /api/v1/film/projects/:id/characters/:cid                 {deleted, dir_removed}

POST   /api/v1/film/projects/:id/characters/:cid/refs            {filename, content_base64, kind:"portrait"|"ref"} → 201 {ref_id, path, kind}
DELETE /api/v1/film/projects/:id/characters/:cid/refs/:ref_id    {deleted}

POST   /api/v1/film/projects/:id/characters/:cid/portrait        {model_ref} → 202 任务（渠道多图组图/本地档2 生成定妆图 + 可选三视图）
POST   /api/v1/film/projects/:id/shots/:n/image                  现有 body + character_ids?: [cid]（缺省用 script.json 绑定）
POST   /api/v1/film/projects/:id/shots/:n/tts                    现有 body + voice?: …（缺省用镜头绑定角色的 voice 配置）
```

- `portrait` 复用现有 202 任务生命周期（queued→running→done|error + 环形日志）。
- `script.json` 每镜头追加可选字段 `"characters": ["char-…", …]`（分镜 LLM
  提示词同步要求输出角色名→建角色时回填 id；解析容错规则沿用 film.rs 现有三
  候选 + 钳制模式）。

### 4.3 产物目录布局（扩展现有布局，向后兼容）

```text
<tank/os-data/film>/<film-101>/            # NEXOS_FILM_DIR/<id>/（现有）
├── script.json              # shots[] 增可选 characters:[cid…]
├── shot-1.png / .mp4 …      # 现有
├── line-1.mp3 …             # 现有
├── refs/                    # 项目级导入参考（未绑定角色：场景图/风格图）
│   └── <ref-id>.png
└── characters/
    └── char-a1b2/
        ├── character.json   # {id,name,desc_prompt,voice{mode,voice,voice_ref,ref_audio_ref},created_at}
        ├── portrait.png     # 定妆图（主参考；kind="portrait" 上传或生成的落点）
        ├── sheet-front.png / sheet-side.png / sheet-back.png   # 三视图（portrait 任务可选产物）
        └── refs/            # 该角色的导入参考原片
```

- DB：`film.db` 增 `film_characters` 表（id/project_id/name/desc_prompt/
  voice_json/portrait_path/dir/created_at/updated_at），refs 直接扫目录 +
  character.json（对齐「产物文件与表才是真值」的现行哲学）。
- DELETE project 连 `characters/`、`refs/` 一起删（现删目录逻辑天然覆盖）。

---

## 5. 拓扑图（P0→P2 目标形态）

```text
                         ┌────────────────────────── os-api (axum) ──────────────────────────┐
                         │                                                                   │
 前端「影片工作室」        │  film 引擎                                                        │
 ┌──────────────┐        │  ┌────────────────┐   ①定妆图上传/生成   ┌─────────────────────┐  │
 │ 角色库 UI     │───────▶│  │ characters CRUD │◀───────────────────│ portrait 任务        │  │
 │ 定妆/三视图   │  b64   │  │ + refs 上传      │                    │ (渠道组图 / 档2本地) │  │
 │ 分镜绑定      │───────▶│  └───────┬────────┘                    └─────────────────────┘  │
 └──────────────┘        │          │ characters:[cid] 绑定进 script.json                  │
                         │          ▼                                                      │
                         │  ┌──────────────────── run_image_stage ─────────────────────┐   │
                         │  │ prompt = shot.image_prompt + 角色 canonical_prompt(档1)   │   │
                         │  │        + style_hint                                       │   │
                         │  │ local  ─▶ [档2] SDXL+IP-Adapter(定妆图) │ [现装] sd-turbo  │   │
                         │  │ channel─▶ images/generations + reference_images(扩展字段) │   │
                         │  │            ├─直连──▶ Seedream image[] / 万相 ref_img      │   │
                         │  │            ├─薄适配─▶ MiniMax subject_reference           │   │
                         │  │            │          Vidu subjects[]+@name               │   │
                         │  │            └─via_node 中继（现有执行面）                   │   │
                         │  └──────────────────────────────────────────────────────────┘   │
                         │  ┌──────────────────── run_video_stage ─────────────────────┐   │
                         │  │ channel: image=首帧 shot-N.png + reference_images=定妆图  │   │
                         │  └──────────────────────────────────────────────────────────┘   │
                         │  ┌──────────────────── run_tts_stage ───────────────────────┐   │
                         │  │ channel: audio/speech + voice/voice_ref/ref_audio_b64     │   │
                         │  │ [P2] local ─▶ CosyVoice2-0.5B 子进程（零样本克隆音色）     │   │
                         │  └──────────────────────────────────────────────────────────┘   │
                         └───────────────────────────────────────────────────────────────────┘
                                              │ 显存闸门互斥（现装 vLLM 21.6GB ↔ 生图/TTS 子进程）
                                       RTX 3090 24GB
```

---

## 6. 比较矩阵（方案 × 一致性 × 速度 × 成本 × 接入难度）

| # | 方案 | 一致性强度 | 单张/句速度 | 显存 | 权重盘上体积 | 接入难度 | 成本 | 建议阶段 |
|---|------|-----------|-------------|------|--------------|----------|------|----------|
| 1 | 档1：角色 canonical_prompt + 固定 seed（sd-turbo/渠道通用） | ★☆☆ | 不变（秒级） | 不变 | 0 | 极低（纯 prompt 组装） | 0 | **P0** |
| 2 | 渠道 `reference_images` 扩展（Seedream/即梦 4.0 多图） | ★★★★（商业模型） | 渠道侧（10–20 s/张量级） | 0 | 0 | 低（字段透传+聚合站/薄适配） | 按张计费 | **P0** |
| 3 | 渠道视频主体参考（MiniMax S2V-01 / Vidu subjects） | ★★★★（跨镜头身份保持） | 渠道异步（分钟级/条） | 0 | 0 | 中（异步任务需薄适配收敛同步响应，FILM_STUDIO.md §4 已有预案） | 按条计费 | P0（图）→P0.5（视频异步适配） |
| 4 | TTS voice 枚举透传（L1） | ★★☆（同音色不同音调内容） | 不变 | 0 | 0 | 极低（一行透传+角色卡字段） | 0 | **P0** |
| 5 | 渠道 voice_id 克隆（L2，ElevenLabs 型） | ★★★★ | 渠道侧 | 0 | 0 | 低 | 渠道计费 | P0（字段）→实际渠道接入按需 |
| 6 | 档2：SDXL+IP-Adapter plus-face（+Lightning 4 步） | ★★★ | 4 步 1–2 s / 30 步 4–8 s | 11–14 GB | 10.3 GB | 中（生图脚本分支+闸门提额） | 0（电费） | **P1** |
| 7 | 档2'：SDXL+PhotoMaker V2（+Lightning） | ★★★（人脸向） | 4 步可用 | ~11 GB | 8.7 GB | 中（另一套 loader） | 0 | P1 备选 |
| 8 | 档3：InstantID | ★★★+ | 30 步最慢 | 12–20 GB | +~4 GB | 高（insightface+ControlNet 三件套） | 0 | 不推荐（本期） |
| 9 | P2：本地 CosyVoice2-0.5B 零样本克隆（L3 本地版） | ★★★★ | RTF<1，首包 150 ms | 4–6 GB | 3.1 GB | 中（local.tts 新管线） | 0 | **P2** |
| 10 | P2'：本地 GPT-SoVITS v2（零样本+每角色微调） | ★★★★★（微调后） | 快（流式支持） | 推 4–6 GB / 训 8–12 GB | 2–3 GB | 中高（微调链路） | 0 | P2 进阶 |

---

## 7. 分阶段落地清单

### P0（角色库 + 渠道 reference 扩展 + voice 一致；零模型下载）
1. `film_characters` 表 + characters CRUD + refs 上传（b64 JSON，§4.2 契约）；
   项目目录增 `characters/<cid>/`、`refs/`（§4.3）。
2. `ScriptShot` 增 `characters: Vec<String>`（serde default，向后兼容旧 script.json）；
   分镜提示词要求输出角色名并在建角色后回填。
3. `run_image_stage`：档1 组装（canonical_prompt+seed）作为缺省；channel 分支
   请求体追加 `reference_images`/`reference_strength`（有定妆图时）。
4. `gen_video_channel`：追加 `reference_images`（定妆图）与 `subject_type`。
5. `tts_channel`：`voice` 由硬编码改透传（角色 voice 配置 > 请求参数 > 缺省
   `alloy`）；请求体支持 `voice_ref`/`ref_audio_b64`/`ref_audio_text`/
   `instructions` 可选透传。
6. portrait 任务（渠道组图生成定妆图/三视图，Seedream `sequential_image_generation`
   或多图参考）；`portrait.png` 即为后续引用的主参考。
7. 测试：mock 渠道断言扩展字段在线上、缺角色配置回退旧行为（零回归面）。

### P1（本地 IP-Adapter 档）
1. 魔搭下载 SDXL fp16 + ViT-H 编码器 + plus-face 适配器（+Lightning LoRA）
   共 ≈10.7 GB → `/tank/models/sdxl-base-1.0` 等。
2. `IMGGEN_SCRIPT_PY` 增分支：`NEXOS_IP_ADAPTER` 存在时走
   `StableDiffusionXLPipeline` + `load_ip_adapter` + `ip_adapter_image`
   （读 `NEXOS_IP_IMAGE` 定妆图路径、`NEXOS_IP_SCALE`）；`ImageJob` 增三个
   可选字段（pub(crate)，film 传入）。
3. 生图闸门：IP-Adapter 档要求空闲 ≥~11 GB（新常量，不动 media_gen 旧值）；
   超时建议该档 300 s（冷加载 SDXL 比 sd-turbo 慢数倍）。
4. 定妆图三视图落 `characters/<cid>/sheet-*.png`；分镜注入时 `portrait.png`
   为 `ip_adapter_image`，多角色镜头取首角色（P1 限制，见 §8）。

### P2（本地 TTS 克隆档）
1. 魔搭下载 CosyVoice2-0.5B（≈3.1 GB）→ `/tank/models/CosyVoice2-0.5B`。
2. 新 TTS 子进程管线（对齐生图内核形态：脚本落盘 + env 传参 + 超时 kill +
   显存闸门 ~5 GB）；角色卡 `voice.mode="clone"` + `ref_audio_ref`（refs 里的
   参考音频）→ 零样本合成。
3. `validate_model_ref` 放行 `local+tts`；film 分流矩阵同步更新（docs/FILM_STUDIO.md §2）。
4. （进阶）GPT-SoVITS `api_v2.py` 常驻 9880 端口形态：与 vLLM 实例管理同思路
   做实例面，微调任务用空闲显存窗口错峰跑。

---

## 8. 风险与开放问题

| # | 问题 | 现状/影响 | 缓解或待决 |
|---|------|-----------|-----------|
| 1 | **sd-turbo 无 IP-Adapter** | 档2 必须换 SDXL；sd-turbo 保留档1 | 已定论（§2.1.1），无需再验证 |
| 2 | **生图尺寸 1272×720 超基座原生分辨率** | film 16:9 画幅 1272×720（film.rs:132）；SD2.1 原生 512/768、SDXL 原生 1024——超原生出图构图易崩；且 media_gen 请求级校验上限 1024（film 走 ImageJob 直通绕过） | 候选：1024×576 生成 + Lanczos 放大到 1272×720；或 SDXL 接受微超采。**待实现时定** |
| 3 | **每张图冷加载模型** | 现 spawn-per-image 设计下 SDXL 冷加载 ~10–30 s/张，12 分镜浪费数分钟 | P1 可选优化：常驻 python 守护（stdin/stdout 协议）或单进程批量；先实测再决定是否值得 |
| 4 | **vLLM 与生图/TTS 互斥** | 24 GB 装不下 vLLM(21.6 GB)+SDXL(12 GB)；现有闸门只是「拒绝」不是「调度」 | P0 用渠道即可绕开；本地档需用户先停实例（如实报错指引），实例自动让位调度列为开放设计 |
| 5 | **多角色同镜头的本地注入** | IP-Adapter 单参考输入；双角色同框需 region 控制（如 IP-Adapter regional / masks） | P1 先支持「单主角色注入+其余角色 prompt 描述」；多角色区域化列为 P1.5 |
| 6 | **MiniMax `subject_reference.image` 的 b64 支持** | 官方文档示例为 URL；b64 是否接受未验证 | 需实测；不行则薄适配需临时图床或走 Vidu（b64 明确支持）/Seedream（b64 明确支持） |
| 7 | **异步任务形态**（Seedance/Kling/MiniMax video 返回 task_id） | film 本期不做上游轮询（响应无 url/b64 即 error，FILM_STUDIO.md §4） | 沿用既定薄适配预案；S2V/reference2video 接入时同一适配层顺带解决 |
| 8 | **渠道扩展字段被严格服务端拒绝的可能** | 个别严格 OpenAI 兼容实现可能 422 未知字段 | 发送侧按渠道灰度（渠道元数据加「支持 reference」标记）；失败报错原文透出，不静默重试污染 |
| 9 | **声线克隆的授权与合规** | 克隆他人音色有法律风险 | 产品面：参考音频仅允许用户自有/已授权素材；角色卡记录来源声明（字段预留） |
| 10 | **一致性无量化验收** | 「不变样」目前靠人眼 | 开放项：后期可引入 insightface 余弦相似度做 portrait↔shot-N 自动评分（antelopev2 已在 P2 依赖清单生态内），本期不做 |
| 11 | **PhotoMaker/IP-Adapter 的魔搭镜像完备性** | SDXL/IP-Adapter 镜像已实测存在；PhotoMaker-V2 魔搭未证实（HF 1.8 GB，Gitee AI 有镜像） | 档2 主选 IP-Adapter（镜像已验证）；PhotoMaker 仅备选 |
| 12 | **`verse`/`ballad` 等 voice 枚举的渠道兼容** | 国内 OpenAI 兼容 TTS 渠道枚举不齐（有的只有 6 音色或自定义中文音色名） | voice 透传 + 渠道 models/naming 约定（现有「前端按渠道名选」哲学延伸）；非法枚举由上游报错如实透出 |

---

## 9. 参考链接汇总

**渠道官方文档**：[火山方舟图片生成 API](https://docs.volcengine.com/docs/ark/image-generation-api?lang=zh) ·
[即梦 4.0 产品文档](https://www.volcengine.com/docs/85621/1820192) ·
[Seedream 调用文档（云器镜像）](https://www.yunqi.tech/documents/aigw_volcengine_seedream_calling) ·
[MiniMax S2V-01 主体参考](https://platform.minimaxi.com/docs/api-reference/video-generation-s2v) ·
[Vidu 参考生视频](https://platform.vidu.cn/docs/reference-to-video) ·
[Kling 图生视频 v1.6](https://kling.ai/document-api/api/video/1-6) ·
[Kling 3.0 Omni](https://kling.ai/document-api/api/video/3-0-omni/image-to-video) ·
[万相文生图 v1（ref_img）](https://help.aliyun.com/zh/model-studio/text-to-image-api-reference) ·
[可灵图像生成（百炼镜像）](https://help.aliyun.com/zh/model-studio/kling-image-generation-api-reference)

**本地模型**：[tencent-ailab/IP-Adapter](https://github.com/tencent-ailab/IP-Adapter) ·
[diffusers#9528（蒸馏模型 IP-Adapter）](https://github.com/huggingface/diffusers/issues/9528) ·
[InstantX/InstantID](https://huggingface.co/InstantX/InstantID) ·
[InstantID 显存实测](https://stable-diffusion-art.com/instantid/) ·
[TencentARC/PhotoMaker](https://github.com/TencentARC/PhotoMaker) ·
[PhotoMaker-V2 权重](https://huggingface.co/TencentARC/PhotoMaker-V2/tree/main) ·
[ByteDance/SDXL-Lightning](https://huggingface.co/ByteDance/SDXL-Lightning) ·
[魔搭 SDXL 镜像](https://modelscope.cn/models/AI-ModelScope/stable-diffusion-xl-base-1.0) ·
[魔搭 IP-Adapter 镜像](https://modelscope.cn/models/soulteary/h94-IP-Adapter/)

**音频**：[OpenAI TTS 文档](https://developers.openai.com/api/docs/guides/text-to-speech) ·
[ElevenLabs 声线克隆](https://elevenlabs.io/docs/api-reference/voices/ivc/create) ·
[GPT-SoVITS api_v2](https://github.com/RVC-Boss/GPT-SoVITS/blob/main/api_v2.py) ·
[GPT-SoVITS 中文 README](https://github.com/RVC-Boss/GPT-SoVITS/blob/main/docs/cn/README.md) ·
[CosyVoice2 显存/流式实测](https://adg.csdn.net/694cf5655b9f5f31781aaac0.html) ·
[CosyVoice GitHub #1396](https://github.com/QwenAudio/CosyVoice/issues/1396) ·
[GPT-SoVITS 实战指南（百度云）](https://cloud.baidu.com/article/4033243)

**业界工作流**：[AI 漫剧一致性全流程（B站）](https://www.bilibili.com/video/BV1pSLJ6vEu3/) ·
[3步出片 libTV 工作流（YouTube）](https://www.youtube.com/watch?v=seAmKpXfOK8) ·
[ZJT 智剧通（开源）](https://github.com/jeffstric/ZJT) ·
[可编辑分镜选型（CSDN）](https://blog.csdn.net/2603_96735916/article/details/163920762) ·
[角色一致性工具实测（知乎）](https://zhuanlan.zhihu.com/p/2072489923572144060)

**仓库内部**：`docs/FILM_STUDIO.md` · `docs/APPS.md` ·
`crates/os-api/src/handlers/film.rs` · `crates/os-api/src/handlers/media_gen.rs` ·
`crates/os-api/src/handlers/files.rs`（上传先例） · `docs/MEDIA_GEN_AND_CHAIN_AUTH.md`
