# ADR-DEPS-005：CLIP 推理后端选型——candle（纯 Rust + CUDA）

- **状态**：已采纳（Accepted）+ 真实接通验证通过（2026-08-05，RTX 3090 实测）
- **日期**：2026-08-05
- **背景决策来源**：`docs/agents/media-agent.md` §9「风险红线」（不耦合特定 AI 模型私有 API，须经 CLIP 抽象）+ `crates/os-services/src/media_clip.rs` 的 `PlaceholderClipModel` 占位 + 本机 RTX 3090（CUDA 可用）
- **影响范围**：workspace 根 `Cargo.toml` 的 `[workspace.dependencies]`（新增 candle 三件套）+ `crates/os-services/src/media_clip.rs`（新增 `CandleClipModel`）
- **前置 ADR**：ADR-DEPS-001（`tantivy` 已注册）、ADR-DEPS-002（P2 领域依赖已注册）

---

## 背景

OS 系统的 media-agent 负责媒体入库与语义检索（参考 Immich）。其中 CLIP（Contrastive Language-Image
Pre-training）模型提供图像/文本向量嵌入，是语义搜索（"找类似照片"）与零样本标签（"beach"/"mountain"）
的核心能力。当前 `media_clip.rs` 已有：

- [`ClipModel`] trait：图像/文本嵌入 + 相似度抽象（不耦合具体后端）。
- [`PlaceholderClipModel`]：占位实现，返回 FNV-1a 哈希派生的确定性向量（64 维），让上层管线
  与测试可走通。

占位实现无法提供真实语义（如 `"dog"` 与 `"puppy"` 不相似），须选型真实推理后端替换。本机配备
RTX 3090（CUDA 11+ 可用），GPU 推理可显著加速嵌入计算。

## 决策

选型 **candle**（HuggingFace 出品的纯 Rust ML 框架）作为 CLIP 推理后端，注册
`candle-core`/`candle-nn`/`candle-transformers` 三件套到 workspace `[workspace.dependencies]`，
在 `media_clip.rs` 新增 `CandleClipModel`（`impl ClipModel`），经 candle-transformers 的
CLIP 模型加载 + 推理。

### 注册清单

| crate | workspace 声明 | feature | 用途（归属 crate） |
|-------|---------------|---------|-------------------|
| `candle-core` | `"0.11"` | 默认（无 GPU 特性；GPU feature 在 crate 级按需开启） | os-services(media) CLIP 推理张量/运算 |
| `candle-nn` | `"0.11"` | 默认 | os-services(media) 神经网络层（线性/归一化/激活） |
| `candle-transformers` | `"0.11"` | 默认 | os-services(media) CLIP 模型定义/加载/前向传播 |

### 选型理由

1. **纯 Rust**：candle 是 HuggingFace 出品的纯 Rust ML 框架，无 Python/C++ 运行时依赖。
   与 workspace 已选型的「纯 Rust 栈」一脉相承（`reqwest` 用 rustls 避开 OpenSSL、`gix`
   替代 libgit2、`russh` 替代 libssh2）。构建链干净、交叉编译友好。

2. **CUDA 支持（RTX 3090 可用）**：candle-core 提供 `cuda` feature（引入 `cudarc` + CUDA
   kernel），可在 RTX 3090 上做 GPU 推理。默认注册不带 GPU feature（保持 CI/无 GPU 环境
   可编译），GPU 支持通过 crate 级 feature flag 按需开启（`candle-core = { workspace = true,
   features = ["cuda"] }`）。

3. **CLIP 模型开箱即用**：candle-transformers 已内置 CLIP ViT-B/32/ViT-L/14 等模型定义
   （`clip::ClipModel`/`clip::ClipConfig`），可直接加载 HuggingFace Hub 上的 safetensors
   权重。无需自行实现模型架构。

4. **safetensors 格式**：candle 生态原生支持 safetensors（安全、快速、 mmap 友好的模型权重
   格式），规避 pickle/PyTorch 安全风险。权重文件可从 HuggingFace Hub 运行时下载或预置。

5. **与 Rust 异步栈一致**：candle 推理是同步 CPU/GPU 操作，可在 `tokio::task::spawn_blocking`
   中运行（`ClipModel` trait 已是 `async_trait`，`CandleClipModel` 内部 spawn_blocking →
   candle 同步调用），与 OS 系统的 tokio 异步运行时无缝集成。

6. **活跃维护 + HuggingFace 背书**：candle 由 HuggingFace 官方维护，0.11.0 是当前稳定主线，
   与 transformers Python 生态模型格式兼容（可直接复用 HuggingFace Hub 上的 CLIP 权重）。

### CUDA feature 策略

- workspace 注册时**不带** GPU feature（`candle-core = "0.11"`），保证无 GPU 环境 / CI
  可编译。
- `CandleClipModel` 默认走 CPU 后端（candle-core 的 CPU 实现基于纯 Rust，无需额外依赖）。
- GPU 加速通过 crate 级 feature flag `clip-cuda` 按需开启（加 `candle-core = { workspace =
  true, features = ["cuda"] }` 到 os-services）。部署环境有 RTX 3090 时启用，CI/测试
  不启用——与 `mock` feature 正交。

## 备选方案与否定理由

1. **ort (ONNX Runtime Rust 绑定)**：
   - 否定理由：ort 依赖 ONNX Runtime C++ 库（libonnxruntime），需系统级安装或 bundled
     编译（bundle 后 ~200MB+），引入 C++ FFI 构建复杂度。OS 系统已明确「最小系统依赖」
     原则（见 ADR-DEPS-001/002 的 rusqlite bundled、gix 替代 libgit2 等决策）。candle 纯
     Rust 无此问题。
   - 额外问题：ort 的 CUDA EP（Execution Provider）需额外安装 CUDA 版 ONNX Runtime，
     版本与系统 CUDA toolkit 耦合，部署复杂度高。

2. **tract (Sonos 出品的 Rust ONNX/TFLite 推理)**：
   - 否定理由：tract 对 CLIP Vision Transformer (ViT) 的 op 支持不完整（ViT 需要
     attention/layer_norm/gelu 等算子），CLIP ViT-B/32 在 tract 上推理曾遇到兼容性问题。
     candle-transformers 对 CLIP 是一等公民支持。

3. **Python 桥接（PyO3 + transformers）**：
   - 否定理由：引入 Python 运行时依赖，破坏「纯 Rust 构建」目标。部署时需安装 Python +
     PyTorch + transformers，容器镜像膨胀（~2GB+），与 OS 系统「轻量嵌入式部署」矛盾。
   - CLIP 推理延迟增加（Python ↔ Rust IPC 开销）。

4. **保持 PlaceholderClipModel 不替换**：
   - 否定理由：占位实现无语义（哈希派生），无法支撑真实语义搜索/零样本标签——media-agent
     的核心价值无法交付。规格书 §9 红线要求「耦合特定 AI 模型私有 API 须经理 Clip 抽象」，
     candle 经 `ClipModel` trait 抽象注入，满足此约束。

5. **外部 HTTP CLIP 服务（RemoteClipClient）**：
   - 部分否定：外部服务（如 OpenAI CLIP API / 自部署 CLIP HTTP 服务）是可选的高层抽象，
     但 OS 系统定位为**自托管**（不依赖外部云 API）。candle 本地推理更符合自托管定位。
   - 保留：`RemoteClipClient` 作为备选方案——若 candle 性能不满足（大规模嵌入批量计算），
     可后续通过独立 ADR 接入外部服务，`ClipModel` trait 已预留此扩展路径。

## 代价

- **编译时间**：candle 三件套 + 传递依赖（safetensors/byteorder/num-traits 等）首次编译
  约 ~2-5 分钟（CPU only；CUDA feature 额外编译 cudarc CUDA kernel）。但 candle 是纯
  Rust，无 C/C++ 编译环节（CUDA kernel 是 NVPTX 编译，非 C++），可接受。
- **运行时模型权重**：真实推理需下载 CLIP 模型权重（ViT-B/32 safetensors ~600MB）。首次
  运行从 HuggingFace Hub 下载；可预置到 `/var/lib/os/models/clip/`。`CandleClipModel`
  的骨架实现阶段**不依赖真实权重**——模型加载失败时回退到错误提示（不 panic），测试用
  `PlaceholderClipModel` 覆盖。
- **GPU feature 复杂度**：CUDA feature 需系统安装 CUDA toolkit（nvcc 编译 kernel）。本机
  RTX 3090 已有 CUDA 环境，CI/无 GPU 环境走 CPU 后端。feature flag 策略已设计为正交可配。
- **candle API 快速迭代**：candle 0.8 → 0.11 跨多个 minor 版本，API 有 breaking changes。
  锁定 0.11 大版本，后续升级由独立 ADR 评估迁移成本。

## 验证

1. **版本解析 + 编译**：`cargo check -p os-services --features mock` 通过（exit 0），
   candle 三件套 0.11.x 及其传递依赖完整编译（CPU only，无 CUDA feature）。
2. **CandleClipModel 结构体编译通过**：新增 `CandleClipModel`（`impl ClipModel`），编译
   通过。骨架阶段模型加载为占位（返回 `ServiceError::Internal` + 诊断信息），不依赖真实
   权重文件。
3. **PlaceholderClipModel 保留**：占位实现不动，`--features mock` 下测试全部通过。
4. **测试数 ≥ 原有**：新增 `CandleClipModel` 的构造器/维度测试 + 错误路径测试。
5. **clippy 无警告**：`cargo clippy -p os-services --features mock -- -D warnings` 通过。

## 对既有约定的影响

- workspace 根 `Cargo.toml` `[workspace.dependencies]` 新增「CLIP 推理（os-services media CLIP 嵌入）」
  分区（归属标注），紧随 ADR-DEPS-002 的 P2 分区。
- `crates/os-services/Cargo.toml` 新增 `candle-core`/`candle-nn`/`candle-transformers` 三条
  `workspace = true` 引用（仅 media 组件 use 这些类型）。
- `crates/os-services/src/media_clip.rs`：新增 `CandleClipModel`（`impl ClipModel`）。
  **trait 签名零改动**，`ServiceError` variant 零改动。
- `PlaceholderClipModel` 保留不动（测试/无 GPU 环境 fallback）。
- 不影响 os-services 的其他五个组件（backup/monitor/files/devtools/power）。

## 后续

- ~~**真实模型加载**：`CandleClipModel` 从骨架（占位错误）推进到真实 candle CLIP 推理~~
  **（已完成，见下方「真实接通验证结果」）**。
- ~~**CUDA feature 接入**：RTX 3090 部署环境启用 `clip-cuda` feature~~ **（已完成）**。
- **外部 CLIP 服务备选**：若 candle 性能不满足大规模嵌入计算，可后续通过独立 ADR 接入
  `RemoteClipClient`（HTTP 调用外部 CLIP 服务），`ClipModel` trait 已预留此扩展路径。
- **candle 版本升级**：锁定 0.11 大版本；后续若 0.12 stable 落地，由独立 ADR 评估
  breaking change 与迁移成本。

---

## 真实接通验证结果（2026-08-05 更新）

`CandleClipModel` 已从骨架推进到 **真实 candle CLIP 推理**，并在本机 RTX 3090 实测通过。

### 接通内容

1. **crate feature `clip-cuda`**（`crates/os-services/Cargo.toml`）：
   `clip-cuda = ["candle-core/cuda", "candle-nn/cuda", "candle-transformers/cuda"]`。
   开启后启用 candle CUDA 后端（cudarc 0.19.8 + candle-kernels PTX），RTX 3090 GPU 推理；
   不开启则走 CPU 后端（CI/无 GPU 环境编译可过）。与 `mock` feature 正交。
2. **辅助依赖**：新增 `image` 0.25（图像解码/resize，jpeg/png/webp/bmp feature）、
   `tokenizers` 0.21（CLIP BPE tokenizer）。`hf-hub` 不引入（权重 + tokenizer.json 由部署预置，
   不入运行时触网依赖——便于离线 OS 部署 + 测试可复现）。
3. **`CandleClipModel` 真实实现**（`crates/os-services/src/media_clip.rs`）：
   - 构造时 mmap `model.safetensors` → `VarBuilder::from_mmaped_safetensors` →
     `clip::ClipModel::new(vb, &ClipConfig::vit_base_patch32())`。
   - `embed_image`：`image` 解码 → `resize_to_fill(224,224)` → RGB → 仿射 [-1,1]（openai CLIP
     归一化，非 HF mean/std）→ `to_device(GPU)` → `get_image_features` → L2 归一化。
   - `embed_text`：`tokenizers` BPE（`with_truncation(max_length=77)` 加载时设定）→
     `get_text_features` → L2 归一化。
   - 推理在 `tokio::task::spawn_blocking` 中执行（candle 同步 API → tokio 异步桥接）；
     `OwnedClipState`（model/tokenizer/device clone，Arc 共享只读权重）避免跨 `.await` 持锁。
   - 设备选择：`pick_device()` 在 `clip-cuda` feature 下 `Device::new_cuda(0)`，失败回退 CPU。
4. **权重来源**：`openai/clip-vit-base-patch32`（HF safetensors 格式，~605MB）+ `tokenizer.json`
   + `config.json`。默认缓存 `~/.cache/os-clip/`（`.gitignore` 排除 `*.safetensors` / `os-clip/`，
   红线：权重不入 git）。可用 `OS_CLIP_MODEL_DIR` 环境变量覆盖路径。
5. **`#[ignore]` 真实测**（需 GPU + 权重）：
   - `candle_real_embed_image_and_text`：断言 embedding 512 维 + L2 归一化 +
     图文相似度 > 0.20 + 相关文本相似度 > 不相关。
   - `candle_real_embed_text_text_similarity`：断言 sim(cat,kitten) > sim(cat,car)（真实语义）。
6. **`PlaceholderClipModel` 保留**：无 GPU / 无权重环境的零语义占位 fallback，不动。

### RTX 3090 实测数据（CUDA 13.3 + cudarc 0.19.8 + candle 0.11）

测试命令（须 nvcc 在 PATH + CUDA_HOME + LD_LIBRARY_PATH，权重在 `~/.cache/os-clip/`）：

```
PATH=/usr/local/cuda/bin:$PATH CUDA_HOME=/usr/local/cuda \
LD_LIBRARY_PATH=/usr/local/cuda/lib64 \
cargo test -p os-services --features mock,clip-cuda --lib media_clip -- --ignored --nocapture
```

结果（2 passed; 0 failed）：

| 指标 | 值 |
|------|-----|
| `embed_image` 首次（含 CUDA kernel JIT 编译） | ~886 ms |
| `embed_image` 稳态（kernel 已缓存） | ~99 ms |
| `embed_text` 稳态 | ~16 ms |
| cos_sim(red_square.jpg, "a solid red square shape") | **0.2934** (> 0.20 阈值) |
| cos_sim(red_square.jpg, "a blue ocean wave") | 0.2161 (相关项更高，语义正确) |
| sim(cat, kitten) | 0.9416 |
| sim(cat, car) | 0.8953 (cat-kitten > cat-car，语义正确) |

### 关键技术细节

- **CUDA 编译前置条件**：cudarc 0.19.8 的 build script 调 `nvcc --version` 探测 CUDA 版本
  （系统 CUDA 13.3 → 启用 `cuda-13030` feature）。本机 `nvcc` 不在默认 PATH（在
  `/usr/local/cuda/bin/`），故编译/测试须显式 `export PATH=/usr/local/cuda/bin:$PATH`。
- **device mismatch 修复**：图像张量先在 CPU 构造（reshape/affine 廉价），再 `.to_device(GPU)`
  转到模型所在设备，避免 `conv2d` 的 lhs(Cpu)/rhs(Cuda) device mismatch。
- **tokenizer 截断**：`tokenizers` 0.21 的 `with_truncation` 在加载时一次性设定 max_length=77
  （消耗/借用 self 两版重载，借用版返回 `Result<&mut Self>`），`encode` 不可变。
- **HuggingFace 直连受阻**：`huggingface.co` 在本机网络不可达（DNS/连接超时），权重经
  `hf-mirror.com` 镜像下载（`sentence-transformers/clip-ViT-B-32` 的 `0_CLIPModel/model.safetensors`
  为 HF 格式 ViT-B/32 权重，与 candle `clip::ClipModel` 命名兼容；`tokenizer.json` 取自
  `openai/clip-vit-base-patch32` main）。

### 验证

1. **CPU 编译 + 测试**：`cargo test -p os-services --features mock --lib media_clip` →
   28 passed; 0 failed; 2 ignored（真实 GPU 测试）。
2. **CUDA 编译**：`cargo build -p os-services --features mock,clip-cuda` → exit 0
   （cudarc + candle-kernels PTX 编译 ~54s）。
3. **真实 GPU 推理**：上述 `#[ignore]` 测试在 RTX 3090 全部通过（相似度 + 维度断言）。
4. **trait 签名零改动**：`ClipModel` trait / `ServiceError` variant 不动；`PlaceholderClipModel`
   保留；不影响其他五个组件。

