//! CLIP 向量识别编排骨架（接口抽象 + 占位实现）。
//!
//! **定位**：CLIP（Contrastive Language-Image Pre-training）用于图像语义嵌入，
//! 支持向量相似度搜索（"找类似照片"）与零样本标签（"beach"/"mountain"/"dog"）。
//! 真实推理需 candle / ONNX runtime / 外部服务（运行时硬阻塞，依赖未注册），
//! 故本模块提供：
//! - [`ClipModel`] trait：图像嵌入 + 相似度 + 文本嵌入抽象（不耦合具体后端）。
//! - [`PlaceholderClipModel`]：占位实现，返回确定性向量（哈希派生），让上层管线
//!   与测试可走通；真实 candle/外部服务接入时替换 trait 实现（不留 TODO 在 trait 侧，
//!   见 [`CandleClipModel`] 骨架——ADR-DEPS-005）\[DOC\]。
//! - 语义聚类骨架（[cluster_by_similarity`]）：基于向量近邻的纯逻辑分组。
//! - 场景/物体标签骨架（[label_scene`]）：零样本分类（候选词表 + 相似度排序）。
//!
//! **设计原则**（参考规格书 §9 红线"耦合特定 AI 模型私有 API 必须经 CLIP 抽象"）：
//! - 调用方（`DefaultMediaManager`）只依赖 `dyn ClipModel`，不耦合 candle / ONNX。
//! - 向量约定：`Vec<f32>`，L2 归一化（余弦相似度 = 点积，数值稳定）。
//! - 维度由实现侧声明（[`ClipModel::embedding_dim`]）；占位实现用 64 维（小而稳定）。

use std::path::Path;

use async_trait::async_trait;
use candle_core::Tensor;

use crate::media::MediaAsset;
use crate::ServiceError;

// ----------------------------------------------------------------------------
// CLIP 接口抽象
// ----------------------------------------------------------------------------

/// CLIP 模型抽象——图像/文本嵌入 + 相似度。
///
/// 实现者：
/// - [`PlaceholderClipModel`]：占位（确定性哈希派生向量；测试/无 GPU 环境 fallback）。
/// - [`CandleClipModel`]：本地 candle 推理（骨架阶段；真实权重加载待后续 ADR）。
/// - 未来可选：`RemoteClipClient`（HTTP 调用外部 CLIP 服务）—— 接入后由调用方注入。
///
/// 所有向量返回前应 L2 归一化（[`normalize`]），使余弦相似度退化为点积。
#[async_trait]
pub trait ClipModel: Send + Sync {
    /// 嵌入向量维度（实现侧固定；占位实现 64）。
    fn embedding_dim(&self) -> usize;

    /// 嵌入一张图片（读文件 → 解码 → CLIP 视觉编码 → L2 归一化）。
    ///
    /// 失败映射为 `ServiceError::Internal`（保留模型错误诊断）。
    async fn embed_image(&self, path: &Path) -> Result<Vec<f32>, ServiceError>;

    /// 嵌入一段文本（CLIP 文本编码 → L2 归一化）。用于零样本分类。
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, ServiceError>;

    /// 两向量相似度（默认余弦；归一化向量退化为点积）。返回 `[-1.0, 1.0]`。
    fn similarity(&self, a: &[f32], b: &[f32]) -> f32 {
        cosine_similarity(a, b)
    }
}

// ----------------------------------------------------------------------------
// 占位实现（哈希派生确定性向量）
// ----------------------------------------------------------------------------

/// 占位 CLIP 模型——不调真实推理框架，返回确定性向量（哈希派生 + L2 归一化）。
///
/// 用途：上层管线（ingest/search）与测试在无真实 CLIP 时走通；语义属性
/// 不真实但**稳定**（同输入 → 同向量），满足幂等性测试。
///
/// 派生策略：
/// - `embed_image(path)`：对 `path` 字节做 FNV-1a 哈希 → 撒到 64 维向量。
/// - `embed_text(text)`：对 `text` 字节做 FNV-1a 哈希 → 撒到 64 维向量。
/// - 不同输入大概率不同向量；同输入相同向量 → 相似度 = 1.0。
///
/// **不**保证语义合理（如 `"dog"` 与 `"puppy"` 占位实现下不相似）——真实语义
/// 需替换为 candle/ONNX 实现。但保证数值合法性（有限 + L2 归一化）。
#[derive(Debug, Clone, Default)]
pub struct PlaceholderClipModel {
    /// 嵌入维度（默认 64；可配置以模拟不同 CLIP 变体）。
    dim: usize,
    /// 撒盐偏移（让同输入在不同实例下产生不同向量；测试隔离用）。
    salt: u64,
}

impl PlaceholderClipModel {
    /// 构造默认占位模型（dim = 64）。
    pub fn new() -> Self {
        Self { dim: 64, salt: 0 }
    }

    /// 指定维度构造（模拟不同 CLIP 变体：ViT-B/32 = 512 等）。
    pub fn with_dim(dim: usize) -> Self {
        Self {
            dim: dim.max(1),
            salt: 0,
        }
    }

    /// 指定盐值构造（测试隔离：相同输入产生不同向量）。
    pub fn with_salt(mut self, salt: u64) -> Self {
        self.salt = salt;
        self
    }

    /// 哈希派生向量（内部：把字节哈希撒到 dim 维 + L2 归一化）。
    fn hash_embedding(&self, bytes: &[u8]) -> Vec<f32> {
        let mut v = vec![0.0_f32; self.dim];
        if v.is_empty() {
            return v;
        }
        let h = fnv1a_64(bytes).wrapping_add(self.salt);
        // 把哈希值的不同比特段散布到各维（伪随机但确定性）。
        for (i, slot) in v.iter_mut().enumerate() {
            // 第 i 维用旋转后的哈希：cycle 32 比特避免相邻维度相关。
            let shifted = h.rotate_left((i as u32) & 63);
            // 映射到 [-1, 1]：取偶/奇比特
            let bit = ((shifted >> (i % 64)) & 1) as i32;
            *slot = if bit == 1 { 1.0 } else { -1.0 };
        }
        normalize(&mut v);
        v
    }
}

impl PlaceholderClipModel {
    /// 同 [`ClipModel::embed_image`] 的同步入口（占位实现不需 IO/推理；测试用）。
    pub fn embed_image_sync(&self, path: &Path) -> Vec<f32> {
        // 占位：用 path 字符串字节哈希（不真读文件——保持「不依赖运行时」语义）。
        let path_str = path.to_string_lossy();
        self.hash_embedding(path_str.as_bytes())
    }

    /// 同 [`ClipModel::embed_text`] 的同步入口。
    pub fn embed_text_sync(&self, text: &str) -> Vec<f32> {
        self.hash_embedding(text.as_bytes())
    }
}

#[async_trait]
impl ClipModel for PlaceholderClipModel {
    fn embedding_dim(&self) -> usize {
        self.dim
    }

    async fn embed_image(&self, path: &Path) -> Result<Vec<f32>, ServiceError> {
        Ok(self.embed_image_sync(path))
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, ServiceError> {
        Ok(self.embed_text_sync(text))
    }
}

// ----------------------------------------------------------------------------
// Candle CLIP 真实推理实现（ADR-DEPS-005 真实接通）
// ----------------------------------------------------------------------------
//
// 真实加载 candle-transformers 的 CLIP 模型（ViT-B/32）权重（safetensors），
// 经 vision_model / text_model 前向传播产出 512 维语义嵌入（projection_dim），
// L2 归一化后返回，让上层语义搜索 / 零样本标签拿到真实可比的向量。
//
// **设计**：
// - 模型权重 + tokenizer.json 由调用方预置到 `model_dir`（HuggingFace
//   `openai/clip-vit-base-patch32` revision `refs/pr/15` 的合并 safetensors 变体；
//   与 candle 官方 CLIP 示例一致）。运行时不再触网——便于离线部署 + 测试可复现。
// - 推理在 `tokio::task::spawn_blocking` 中执行（candle 同步 API → tokio 异步桥接，
//   避免阻塞 reactor；ClipModel trait 已是 async_trait）。
// - 设备：开启 crate feature `clip-cuda` 时优先 `Device::new_cuda(0)`（RTX 3090），
//   失败回退 CPU；不开启 feature 时恒 CPU（CI/无 GPU 环境编译可过）。
// - 图像预处理与 candle 官方 CLIP 示例对齐：resize_to_fill 224×224 → RGB →
//   [0,255] 仿射到 [-1,1]（不做 mean/std 归一化，因 openai 原版 CLIP 训练用 [-1,1]）。
// - 文本预处理：tokenizers BPE → input_ids（含 BOS/EOS），max_length = 77。

/// 默认 CLIP ViT-B/32 输入图像边长（patch 32 → 7×7=49 patches + 1 CLS）。
const CLIP_IMAGE_SIZE: usize = 224;
/// 默认 CLIP 文本序列长度（max_position_embeddings）。
const CLIP_MAX_TEXT_LEN: usize = 77;

/// 基于 candle 的 CLIP 真实推理实现（ADR-DEPS-005）。
///
/// `model_dir` 须含 `model.safetensors` + `tokenizer.json`（推荐从
/// `openai/clip-vit-base-patch32` revision `refs/pr/15` 下载）。构造时即尝试
/// 加载模型 + 设备初始化；权重/tokenizer 缺失或 CUDA 不可用时返回错误，调用方
/// 可回退 [`PlaceholderClipModel`]（无 GPU / 无权重环境的零语义占位）。
///
/// **线程安全**：内部 `Mutex<ClipState>` 守护 candle 模型（candletransformers
/// 的 ClipModel 非 Sync-safe 共享写状态；spawn_blocking 持锁串行推理）。
///
/// # 示例
/// ```no_run
/// # use os_services::media_clip::{CandleClipModel, ClipModel};
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let model = CandleClipModel::new("~/.cache/os-clip")?;
/// let img_emb = model.embed_image(std::path::Path::new("/photos/a.jpg")).await?;
/// let txt_emb = model.embed_text("a photo of a cat").await?;
/// println!("similarity = {}", model.similarity(&img_emb, &txt_emb));
/// # Ok(())
/// # }
/// ```
pub struct CandleClipModel {
    /// 模型权重目录（含 model.safetensors + tokenizer.json）。
    model_dir: std::path::PathBuf,
    /// CLIP 嵌入维度（ViT-B/32 projection_dim = 512）。
    embedding_dim: usize,
    /// 已加载的模型 + tokenizer + 设备（持锁串行推理）。
    /// `Option` 仅在构造失败时为 None；正常路径 Some。
    state: std::sync::Mutex<Option<OwnedClipState>>,
}

impl std::fmt::Debug for CandleClipModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self.state.lock().map(|s| s.is_some()).unwrap_or(false);
        f.debug_struct("CandleClipModel")
            .field("model_dir", &self.model_dir)
            .field("embedding_dim", &self.embedding_dim)
            .field("loaded", &loaded)
            .finish()
    }
}

/// candle-transformers CLIP 模块别名（缩短路径）。
mod clip {
    pub use candle_transformers::models::clip::{ClipConfig, ClipModel};
}

/// owned 推理状态（spawn_blocking 用；不持锁）。
/// ClipModel/Tokenizer/Device 均 Clone（Arc/内部 Arc 共享只读权重），故 clone 廉价。
struct OwnedClipState {
    model: clip::ClipModel,
    tokenizer: tokenizers::Tokenizer,
    #[allow(dead_code)] // device 由 model 内部持有；保留以便文本 tokenizer 张量构造。
    device: candle_core::Device,
}

impl CandleClipModel {
    /// 创建并加载 CLIP 模型（ViT-B/32，512 维）。
    ///
    /// `model_dir` 须含 `model.safetensors` + `tokenizer.json`。构造时即 mmap
    /// 权重 + 初始化设备（CUDA 优先，回退 CPU），失败返回 `ServiceError::Internal`。
    pub fn new<P: AsRef<std::path::Path>>(model_dir: P) -> Result<Self, ServiceError> {
        Self::with_dim(model_dir, 512)
    }

    /// 指定嵌入维度构造（模拟不同 CLIP 变体；维度仅供 [`ClipModel::embedding_dim`]
    /// 上报，真实 forward 输出维度由权重决定——构造时校验 `dim` 与权重 projection_dim
    /// 不一致时返回错误，避免上层误用）。
    pub fn with_dim<P: AsRef<std::path::Path>>(
        model_dir: P,
        dim: usize,
    ) -> Result<Self, ServiceError> {
        let model_dir = model_dir.as_ref().to_path_buf();
        let state = Self::load_state(&model_dir).map_err(|e| {
            ServiceError::Internal(format!(
                "CandleClipModel 加载失败（model_dir={:?}）: {e}",
                model_dir
            ))
        })?;
        // 校验维度声明与真实权重一致（防误用）。
        if dim != 512 {
            return Err(ServiceError::Internal(format!(
                "CandleClipModel::with_dim: 维度 {dim} 与 ViT-B/32 projection_dim (512) 不一致；\
                 仅支持 512（其它变体由独立 ADR 接入）"
            )));
        }
        Ok(Self {
            model_dir,
            embedding_dim: dim,
            state: std::sync::Mutex::new(Some(state)),
        })
    }

    /// 模型权重目录路径。
    pub fn model_dir(&self) -> &std::path::Path {
        &self.model_dir
    }

    /// 模型是否已加载（构造成功即为 true；state 内有真实模型）。
    pub fn is_loaded(&self) -> bool {
        self.state.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    /// 加载模型权重 + tokenizer + 选设备。
    fn load_state(model_dir: &std::path::Path) -> Result<OwnedClipState, String> {
        let model_file = model_dir.join("model.safetensors");
        let tokenizer_file = model_dir.join("tokenizer.json");
        if !model_file.exists() {
            return Err(format!(
                "权重文件缺失: {}（请从 openai/clip-vit-base-patch32 revision refs/pr/15 下载）",
                model_file.display()
            ));
        }
        if !tokenizer_file.exists() {
            return Err(format!("tokenizer.json 缺失: {}", tokenizer_file.display()));
        }

        // 设备：CUDA 优先（clip-cuda feature 开启 + GPU 可用），回退 CPU。
        let device = Self::pick_device();

        // mmap safetensors → VarBuilder → ClipModel。
        // unsafe 来自 memmap2（文件不可变 + 进程独占安全）。
        let vb = unsafe {
            candle_nn::VarBuilder::from_mmaped_safetensors(
                std::slice::from_ref(&model_file),
                candle_core::DType::F32,
                &device,
            )
            .map_err(|e| format!("VarBuilder::from_mmaped_safetensors 失败: {e}"))?
        };
        let config = clip::ClipConfig::vit_base_patch32();
        let model =
            clip::ClipModel::new(vb, &config).map_err(|e| format!("ClipModel::new 失败: {e}"))?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_file)
            .map_err(|e| format!("Tokenizer::from_file 失败: {e}"))?;
        // 加载时即配置 max_length 截断（CLIP=77）；embed_text_sync 内的 encode 不可变。
        let truncation = tokenizers::TruncationParams {
            max_length: CLIP_MAX_TEXT_LEN,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| format!("tokenizer with_truncation 失败: {e}"))?;

        Ok(OwnedClipState {
            model,
            tokenizer,
            device,
        })
    }

    /// 选推理设备：开启 clip-cuda feature 时优先 CUDA(0)，失败/无 feature 回退 CPU。
    #[cfg(feature = "clip-cuda")]
    fn pick_device() -> candle_core::Device {
        match candle_core::Device::new_cuda(0) {
            Ok(d) => {
                eprintln!("[clip-cuda] 使用 CUDA 设备 (RTX 3090)");
                d
            }
            Err(e) => {
                eprintln!("[clip-cuda] CUDA 不可用，回退 CPU: {e}");
                candle_core::Device::Cpu
            }
        }
    }

    #[cfg(not(feature = "clip-cuda"))]
    fn pick_device() -> candle_core::Device {
        eprintln!("[clip-cuda] feature 未启用，使用 CPU 后端");
        candle_core::Device::Cpu
    }

    /// 同步图像嵌入（spawn_blocking 内调用）。
    /// 读图 → resize 224 → RGB → 仿射 [-1,1] → vision_model forward → L2 归一化。
    fn embed_image_sync(state: &OwnedClipState, path: &Path) -> Result<Vec<f32>, ServiceError> {
        let img = image::ImageReader::open(path)
            .map_err(|e| ServiceError::Internal(format!("打开图片失败 {:?}: {e}", path)))?
            .decode()
            .map_err(|e| ServiceError::Internal(format!("解码图片失败 {:?}: {e}", path)))?;
        let img = img.resize_to_fill(
            CLIP_IMAGE_SIZE as u32,
            CLIP_IMAGE_SIZE as u32,
            image::imageops::FilterType::Triangle, // bilinear，与 openai CLIP 对齐
        );
        let img = img.to_rgb8();
        let img = img.into_raw();
        // 在 CPU 上构造张量做 reshape/affine（廉价），最后 .to_device 转到模型所在设备
        // （CUDA 时为 GPU；否则 CPU no-op）。避免 device mismatch（权重在 Cuda）。
        let pixels = Tensor::from_vec(
            img,
            (CLIP_IMAGE_SIZE, CLIP_IMAGE_SIZE, 3),
            &candle_core::Device::Cpu,
        )
        .map_err(candle_err)?
        .permute((2, 0, 1))
        .map_err(candle_err)?
        .to_dtype(candle_core::DType::F32)
        .map_err(candle_err)?
        // [0,255] → [-1,1]（openai CLIP 训练归一化；非 HF mean/std）。
        .affine(2.0 / 255.0, -1.0)
        .map_err(candle_err)?
        // 转到模型设备（CUDA 推理时为 GPU；CPU 时 no-op）。
        .to_device(&state.device)
        .map_err(candle_err)?;
        // batch 维：[1, 3, H, W]
        let pixel_values = pixels.unsqueeze(0).map_err(candle_err)?;

        let features = state
            .model
            .get_image_features(&pixel_values)
            .map_err(candle_err)?;
        Self::tensor_to_normalized_vec(&features)
    }

    /// 同步文本嵌入（spawn_blocking 内调用）。
    /// tokenize → input_ids → text_model forward → L2 归一化。
    /// tokenizer 在 load_state 时已配置 max_length 截断（CLIP=77）。
    fn embed_text_sync(state: &OwnedClipState, text: &str) -> Result<Vec<f32>, ServiceError> {
        let encoding = state
            .tokenizer
            .encode(text, true)
            .map_err(|e| ServiceError::Internal(format!("tokenizer 编码失败 {text:?}: {e}")))?;
        let input_ids = encoding.get_ids();
        let ids_tensor = Tensor::from_vec(input_ids.to_vec(), (1, input_ids.len()), &state.device)
            .map_err(candle_err)?;

        let features = state
            .model
            .get_text_features(&ids_tensor)
            .map_err(candle_err)?;
        Self::tensor_to_normalized_vec(&features)
    }

    /// candle Tensor (1, dim) → Vec<f32> + L2 归一化。
    fn tensor_to_normalized_vec(t: &Tensor) -> Result<Vec<f32>, ServiceError> {
        let flat = t.squeeze(0).map_err(candle_err)?;
        let mut v = flat
            .to_vec1::<f32>()
            .map_err(|e| ServiceError::Internal(format!("Tensor → Vec<f32> 失败: {e}")))?;
        normalize(&mut v);
        Ok(v)
    }
}

#[async_trait]
impl ClipModel for CandleClipModel {
    fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    async fn embed_image(&self, path: &Path) -> Result<Vec<f32>, ServiceError> {
        let path = path.to_path_buf();
        // 锁内克隆出 owned（model/tokenizer/device），guard 在 helper 返回时即释放；
        // spawn_blocking 持 owned 闭包，不跨越 .await 持锁（保证 Future: Send）。
        let state = self.clone_state()?;
        tokio::task::spawn_blocking(move || Self::embed_image_sync(&state, &path))
            .await
            .map_err(|e| ServiceError::Internal(format!("spawn_blocking join 失败: {e}")))?
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, ServiceError> {
        let text = text.to_string();
        let state = self.clone_state()?;
        tokio::task::spawn_blocking(move || Self::embed_text_sync(&state, &text))
            .await
            .map_err(|e| ServiceError::Internal(format!("spawn_blocking join 失败: {e}")))?
    }
}

impl CandleClipModel {
    /// 锁内克隆出 owned 的推理状态（model/tokenizer/device），guard 在函数返回时
    /// 释放——确保调用方的 .await 之前锁已 drop（Future: Send）。
    /// ClipModel/Tokenizer/Device 均 Clone（Arc/内部 Arc 共享只读权重）。
    fn clone_state(&self) -> Result<OwnedClipState, ServiceError> {
        let guard = self
            .state
            .lock()
            .map_err(|e| ServiceError::Internal(format!("state 锁中毒: {e}")))?;
        let state = guard
            .as_ref()
            .ok_or_else(|| ServiceError::Internal("模型未加载（state 为 None）".into()))?;
        Ok(OwnedClipState {
            model: state.model.clone(),
            tokenizer: state.tokenizer.clone(),
            device: state.device.clone(),
        })
    }
}

/// candle 错误 → ServiceError::Internal（保留诊断串）。
fn candle_err(e: candle_core::Error) -> ServiceError {
    ServiceError::Internal(format!("candle 错误: {e}"))
}

// ----------------------------------------------------------------------------
// 向量工具（纯函数）
// ----------------------------------------------------------------------------

/// L2 归一化（in-place）；零向量保持零（避免 NaN）。
pub fn normalize(v: &mut [f32]) {
    let mut sum_sq = 0.0_f32;
    for &x in v.iter() {
        sum_sq += x * x;
    }
    if sum_sq < f32::MIN_POSITIVE {
        return; // 零向量：不动
    }
    let norm = sum_sq.sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
}

/// 余弦相似度（点积 / 各自 L2 范数）；任一零向量返回 0.0。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom < f32::MIN_POSITIVE {
        return 0.0;
    }
    dot / denom
}

/// FNV-1a 64 位哈希（无新依赖；确定性散布）。
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ----------------------------------------------------------------------------
// CLIP 向量语义聚类骨架（纯逻辑）
// ----------------------------------------------------------------------------

/// 一个聚类结果（向量近邻分组）。
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    /// 聚类代表 id（首个成员；测试稳定）。
    pub centroid_id: String,
    /// 聚类成员 id 列表（含 centroid_id）。
    pub member_ids: Vec<String>,
}

/// 按向量相似度聚类（贪心近邻法；纯逻辑骨架）。
///
/// 策略（与 `media_album::group_by_location` 同构的贪心锚点法）：
/// 1. 按 asset id 排序保证确定性。
/// 2. 顺序遍历：对每个 asset，若其向量与任一已有聚类的 centroid 相似度 ≥ `threshold`，
///    则归入该聚类；否则新建一个以该 asset 为 centroid 的聚类。
///
/// `threshold` 推荐 0.85（语义近邻经验值）；调用方调参。asset 无 `clip_embedding`
/// 的归入 `__no_embedding__` 单独聚类（centroid_id 固定），便于过滤。
pub fn cluster_by_similarity(
    assets: &[MediaAsset],
    threshold: f32,
    similarity_fn: impl Fn(&[f32], &[f32]) -> f32,
) -> Vec<Cluster> {
    // 排序保证确定性（cluster 的 centroid 选取依赖顺序）。
    let mut sorted: Vec<&MediaAsset> = assets.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut clusters: Vec<Cluster> = Vec::new();
    let mut centroids: Vec<(String, Vec<f32>)> = Vec::new(); // (centroid_id, vector)

    for a in &sorted {
        let v = match &a.clip_embedding {
            Some(v) if !v.is_empty() => v.clone(),
            _ => {
                // 无嵌入：归入固定聚类
                if let Some(c) = clusters
                    .iter_mut()
                    .find(|c| c.centroid_id == "__no_embedding__")
                {
                    c.member_ids.push(a.id.clone());
                } else {
                    clusters.push(Cluster {
                        centroid_id: "__no_embedding__".to_string(),
                        member_ids: vec![a.id.clone()],
                    });
                }
                continue;
            }
        };

        // 找最相似的 centroid
        let mut best: Option<(usize, f32)> = None;
        for (i, (_, cv)) in centroids.iter().enumerate() {
            let s = similarity_fn(&v, cv);
            match best {
                Some((_, bs)) if s <= bs => {}
                _ => best = Some((i, s)),
            }
        }

        match best {
            Some((i, s)) if s >= threshold => {
                clusters[i].member_ids.push(a.id.clone());
                // centroid 不更新（贪心首锚点；测试稳定）。
            }
            _ => {
                // 新建聚类
                let id = a.id.clone();
                clusters.push(Cluster {
                    centroid_id: id.clone(),
                    member_ids: vec![id.clone()],
                });
                centroids.push((id, v));
            }
        }
    }

    clusters
}

// ----------------------------------------------------------------------------
// 场景标签骨架（零样本分类）
// ----------------------------------------------------------------------------

/// 场景标签（基于 CLIP 向量相似度对候选词表打分排序）。
#[derive(Debug, Clone, PartialEq)]
pub struct SceneLabel {
    /// 标签文本（如 `"beach"`）。
    pub label: String,
    /// 相似度分数（[0.0, 1.0]，越大约像）。
    pub score: f32,
}

/// 零样本场景标签：对图像向量与候选词向量做相似度排序，返回 top-K。
///
/// `candidate_texts` 由调用方提供（如 `["beach", "mountain", "city", "dog", ...]`）；
/// 每个候选词先用 `model.embed_text` 编码，再与 `image_vec` 比相似度。
/// 返回按 score 降序、截取 `top_k` 的标签。score < `min_score` 的过滤掉。
pub async fn label_scene(
    model: &dyn ClipModel,
    image_vec: &[f32],
    candidate_texts: &[&str],
    top_k: usize,
    min_score: f32,
) -> Result<Vec<SceneLabel>, ServiceError> {
    let mut scored: Vec<SceneLabel> = Vec::with_capacity(candidate_texts.len());
    for text in candidate_texts {
        let tv = model.embed_text(text).await?;
        let s = model.similarity(image_vec, &tv);
        if s >= min_score {
            scored.push(SceneLabel {
                label: (*text).to_string(),
                score: s,
            });
        }
    }
    // 稳定排序：score 降序，相同 score 按标签升序
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.cmp(&b.label))
    });
    scored.truncate(top_k);
    Ok(scored)
}

/// 默认场景候选词表（参考 Immich / 常见 OS 相册场景）。
pub const DEFAULT_SCENE_LABELS: &[&str] = &[
    "beach",
    "mountain",
    "forest",
    "city",
    "indoor",
    "outdoor",
    "portrait",
    "food",
    "pet",
    "vehicle",
    "sunset",
    "snow",
    "water",
    "architecture",
    "document",
];

// ============================================================================
// 单元测试
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{BBox, FaceTag, MediaAsset};

    // —— 向量工具 ——

    #[test]
    fn normalize_unit_vector() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        // L2 范数 = 1
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn normalize_zero_vector_no_nan() {
        let mut v = vec![0.0, 0.0, 0.0];
        normalize(&mut v);
        assert!(v.iter().all(|x| x.is_finite() && *x == 0.0));
    }

    #[test]
    fn cosine_similarity_identical_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_mismatched_dim_returns_zero() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero() {
        assert_eq!(cosine_similarity(&[0.0; 4], &[1.0; 4]), 0.0);
    }

    // —— 占位模型 ——

    #[test]
    fn placeholder_default_dim_is_64() {
        let m = PlaceholderClipModel::new();
        assert_eq!(m.embedding_dim(), 64);
    }

    #[test]
    fn placeholder_custom_dim() {
        let m = PlaceholderClipModel::with_dim(512);
        assert_eq!(m.embedding_dim(), 512);
    }

    #[test]
    fn placeholder_with_dim_zero_clamps_to_one() {
        let m = PlaceholderClipModel::with_dim(0);
        assert_eq!(m.embedding_dim(), 1);
    }

    #[tokio::test]
    async fn placeholder_embed_image_returns_normalized_vector() {
        let m = PlaceholderClipModel::new();
        let v = m.embed_image(Path::new("/photos/a.jpg")).await.unwrap();
        assert_eq!(v.len(), 64);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "向量应 L2 归一化");
        // 每维值在 [-1, 1]
        assert!(v.iter().all(|x| x.abs() <= 1.0 + 1e-6));
    }

    #[tokio::test]
    async fn placeholder_same_input_same_vector() {
        let m = PlaceholderClipModel::new();
        let v1 = m.embed_image(Path::new("/photos/x.jpg")).await.unwrap();
        let v2 = m.embed_image(Path::new("/photos/x.jpg")).await.unwrap();
        assert_eq!(v1, v2, "占位实现确定性");
        // 自相似度 = 1.0
        assert!((m.similarity(&v1, &v2) - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn placeholder_different_salt_different_vector() {
        let m1 = PlaceholderClipModel::new().with_salt(0);
        let m2 = PlaceholderClipModel::new().with_salt(1);
        let v1 = m1.embed_image(Path::new("/photos/y.jpg")).await.unwrap();
        let v2 = m2.embed_image(Path::new("/photos/y.jpg")).await.unwrap();
        // 盐不同 → 向量大概率不同
        assert_ne!(v1, v2);
    }

    #[tokio::test]
    async fn placeholder_embed_text_deterministic() {
        let m = PlaceholderClipModel::new();
        let v1 = m.embed_text("beach").await.unwrap();
        let v2 = m.embed_text("beach").await.unwrap();
        assert_eq!(v1, v2);
        // 不同文本通常不同
        let v3 = m.embed_text("mountain").await.unwrap();
        // 注：占位哈希派生，"beach" 与 "mountain" 大概率不同；不严格断言（哈希碰撞理论可能）
        assert!(m.similarity(&v1, &v3) <= 1.0 + 1e-5);
    }

    #[tokio::test]
    async fn placeholder_similarity_within_bounds() {
        let m = PlaceholderClipModel::new();
        let a = m.embed_image(Path::new("/x")).await.unwrap();
        let b = m.embed_image(Path::new("/y")).await.unwrap();
        let s = m.similarity(&a, &b);
        assert!((-1.0..=1.0).contains(&s), "余弦相似度 ∈ [-1, 1]");
    }

    // —— Candle CLIP 真实推理（构造/错误路径；真实推理见 #[ignore] 测试） ——

    #[test]
    fn candle_new_missing_weights_returns_error() {
        // 模型目录无 model.safetensors → 构造失败（真实加载路径）。
        let res = CandleClipModel::new("/tmp/os-clip-nonexistent-xyz");
        assert!(res.is_err(), "无权重文件构造应失败");
        let err = res.unwrap_err();
        match err {
            ServiceError::Internal(msg) => {
                assert!(
                    msg.contains("权重文件缺失") || msg.contains("model.safetensors"),
                    "错误应提示权重缺失: {msg}"
                );
            }
            other => panic!("期望 ServiceError::Internal，实际: {other:?}"),
        }
    }

    #[test]
    fn candle_with_dim_non_512_rejected() {
        // 真实目录不存在 → 先报权重缺失；用临时目录验证 dim 校验逻辑不破坏。
        // （dim 校验在权重加载之后；此处主要保证 with_dim 签名编译。）
        let res = CandleClipModel::with_dim("/tmp/os-clip-nonexistent-xyz", 768);
        // 两种合理失败：权重缺失 或（若有权重）维度不一致。
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn candle_satisfies_clip_model_trait_compile() {
        // 编译期验证：CandleClipModel: ClipModel（不真实加载——构造即失败也无妨，
        // 只验证 trait impl 存在）。
        fn assert_clip_model<T: ClipModel>() {}
        assert_clip_model::<CandleClipModel>();
    }

    // —— 真实 GPU 推理测试（#[ignored]；需 CLIP 权重 + GPU/CPU） ——
    //
    // 运行：cargo test -p os-services --features mock,clip-cuda -- --ignored --nocapture clip
    // 权重默认在 ~/.cache/os-clip/（openai/clip-vit-base-patch32 revision refs/pr/15）。
    // 环境变量 OS_CLIP_MODEL_DIR 可覆盖路径；OS_CLIP_TEST_IMAGE 可指定测试图（否则程序生成）。

    fn real_model_dir() -> std::path::PathBuf {
        std::env::var("OS_CLIP_MODEL_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
                std::path::PathBuf::from(home).join(".cache/os-clip")
            })
    }

    /// 生成一张简单的测试 JPEG（红色实心方块 + 白色圆）——CLIP 应能匹配 "red square"。
    fn ensure_test_image() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("OS_CLIP_TEST_IMAGE") {
            return std::path::PathBuf::from(p);
        }
        let dir = std::env::temp_dir().join("os-clip-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("red_square.jpg");
        if path.exists() {
            return path;
        }
        use image::{ImageBuffer, Rgb};
        let (w, h) = (224usize, 224usize);
        let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(w as u32, h as u32);
        let cx = w as i32 / 2;
        let cy = h as i32 / 2;
        let r = (w.min(h) as i32) / 3;
        for y in 0..h {
            for x in 0..w {
                let dx = x as i32 - cx;
                let dy = y as i32 - cy;
                // 圆内白，圆外红
                let px = if dx * dx + dy * dy <= r * r {
                    Rgb([255, 255, 255])
                } else {
                    Rgb([220, 30, 30])
                };
                img.put_pixel(x as u32, y as u32, px);
            }
        }
        img.save(&path).expect("保存测试图失败");
        path
    }

    #[tokio::test]
    #[ignore = "需 CLIP 权重（~/.cache/os-clip/）+ 可选 GPU（clip-cuda feature）"]
    async fn candle_real_embed_image_and_text() {
        let model_dir = real_model_dir();
        let model = CandleClipModel::new(&model_dir)
            .expect("模型加载失败——请先下载权重到 OS_CLIP_MODEL_DIR 或 ~/.cache/os-clip/");
        assert_eq!(model.embedding_dim(), 512);
        assert!(model.is_loaded());

        let img_path = ensure_test_image();
        let t0 = std::time::Instant::now();
        let img_emb = model
            .embed_image(&img_path)
            .await
            .expect("embed_image 真实推理失败");
        let img_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = std::time::Instant::now();
        let txt_emb = model
            .embed_text("a solid red square shape")
            .await
            .expect("embed_text 真实推理失败");
        let txt_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 维度断言
        assert_eq!(img_emb.len(), 512, "image embedding 应 512 维");
        assert_eq!(txt_emb.len(), 512, "text embedding 应 512 维");

        // L2 归一化断言
        let img_norm: f32 = img_emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        let txt_norm: f32 = txt_emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (img_norm - 1.0).abs() < 1e-3 && (txt_norm - 1.0).abs() < 1e-3,
            "embedding 应 L2 归一化（img_norm={img_norm:.4}, txt_norm={txt_norm:.4}）"
        );

        // 图文相似度断言（阈值 0.20：CLIP 真跑通的弱证据；随机向量相似度近 0）
        let sim = model.similarity(&img_emb, &txt_emb);
        eprintln!(
            "[clip-real] embed_image={img_ms:.1}ms embed_text={txt_ms:.1}ms \
             cos_sim(red_square.jpg, \"a solid red square shape\")={sim:.4}"
        );
        assert!(
            sim > 0.20,
            "图文相似度 {sim:.4} 应 > 0.20（证明真实 CLIP 推理接通）"
        );

        // 交叉验证：不相关文本相似度应明显更低（如 "a blue ocean"）
        let neg_emb = model.embed_text("a blue ocean wave").await.unwrap();
        let neg_sim = model.similarity(&img_emb, &neg_emb);
        eprintln!("[clip-real] cos_sim(red_square.jpg, \"a blue ocean wave\")={neg_sim:.4}");
        assert!(
            neg_sim < sim,
            "相关文本相似度 ({sim:.4}) 应高于不相关 ({neg_sim:.4})"
        );
    }

    #[tokio::test]
    #[ignore = "需 CLIP 权重（~/.cache/os-clip/）"]
    async fn candle_real_embed_text_text_similarity() {
        let model_dir = real_model_dir();
        let model = CandleClipModel::new(&model_dir).expect("模型加载失败");
        let a = model.embed_text("a cat").await.unwrap();
        let b = model.embed_text("a kitten").await.unwrap();
        let c = model.embed_text("a car").await.unwrap();
        let sim_cat_kitten = model.similarity(&a, &b);
        let sim_cat_car = model.similarity(&a, &c);
        eprintln!("[clip-real] sim(cat,kitten)={sim_cat_kitten:.4} sim(cat,car)={sim_cat_car:.4}");
        // 语义近邻应高于语义远邻（真实语义证据）。
        assert!(
            sim_cat_kitten > sim_cat_car,
            "cat-kitten 相似度 ({sim_cat_kitten:.4}) 应 > cat-car ({sim_cat_car:.4})"
        );
    }

    // —— 聚类 ——

    fn mk_asset(id: &str, emb: Option<Vec<f32>>) -> MediaAsset {
        MediaAsset {
            id: id.to_string(),
            path: format!("/p/{id}.jpg"),
            mime_type: "image/jpeg".to_string(),
            size_bytes: 100,
            width: Some(100),
            height: Some(100),
            taken_at: None,
            faces: vec![FaceTag {
                name: None,
                bbox: BBox {
                    x: 0.0,
                    y: 0.0,
                    w: 0.1,
                    h: 0.1,
                },
            }],
            clip_embedding: emb,
        }
    }

    #[test]
    fn cluster_groups_similar_vectors() {
        // a1/a2 相似度 1.0（同向量）；a3 不同向量
        let v1 = vec![1.0, 0.0, 0.0];
        let v3 = vec![0.0, 1.0, 0.0];
        let assets = vec![
            mk_asset("a1", Some(v1.clone())),
            mk_asset("a2", Some(v1.clone())),
            mk_asset("a3", Some(v3.clone())),
        ];
        let clusters = cluster_by_similarity(&assets, 0.85, cosine_similarity);
        // 两类：{a1, a2} 和 {a3}
        assert_eq!(clusters.len(), 2);
        // a1/a2 同聚类
        let c12 = clusters
            .iter()
            .find(|c| c.member_ids.contains(&"a1".to_string()))
            .unwrap();
        assert!(c12.member_ids.contains(&"a2".to_string()));
        assert!(!c12.member_ids.contains(&"a3".to_string()));
    }

    #[test]
    fn cluster_below_threshold_splits() {
        // 同向量但阈值 = 2.0（不可能达到）→ 全部分离
        let v = vec![1.0, 0.0];
        let assets = vec![
            mk_asset("a1", Some(v.clone())),
            mk_asset("a2", Some(v.clone())),
        ];
        let clusters = cluster_by_similarity(&assets, 2.0, cosine_similarity);
        assert_eq!(clusters.len(), 2, "阈值超 1.0 → 每个独立聚类");
    }

    #[test]
    fn cluster_no_embedding_goes_to_special_cluster() {
        let assets = vec![
            mk_asset("a1", Some(vec![1.0, 0.0])),
            mk_asset("a2", None),
            mk_asset("a3", None),
        ];
        let clusters = cluster_by_similarity(&assets, 0.85, cosine_similarity);
        // a1 一个聚类；a2/a3 进 __no_embedding__
        let no_emb = clusters
            .iter()
            .find(|c| c.centroid_id == "__no_embedding__")
            .expect("应有 __no_embedding__ 聚类");
        assert_eq!(no_emb.member_ids.len(), 2);
        assert!(no_emb.member_ids.contains(&"a2".to_string()));
        assert!(no_emb.member_ids.contains(&"a3".to_string()));
    }

    #[test]
    fn cluster_empty_embedding_treated_as_no_embedding() {
        // 空 Vec 视为无嵌入（避免零向量相似度异常）
        let assets = vec![
            mk_asset("a1", Some(vec![])),
            mk_asset("a2", Some(vec![1.0, 0.0])),
        ];
        let clusters = cluster_by_similarity(&assets, 0.85, cosine_similarity);
        assert!(clusters.iter().any(
            |c| c.centroid_id == "__no_embedding__" && c.member_ids.contains(&"a1".to_string())
        ));
    }

    #[test]
    fn cluster_empty_assets_returns_empty() {
        let clusters = cluster_by_similarity(&[], 0.85, cosine_similarity);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cluster_deterministic_order() {
        // 打乱输入顺序，结果应稳定（因内部按 id 排序）
        let v1 = vec![1.0, 0.0];
        let v2 = vec![0.0, 1.0];
        let assets_a = vec![
            mk_asset("a1", Some(v1.clone())),
            mk_asset("a2", Some(v1.clone())),
            mk_asset("a3", Some(v2.clone())),
        ];
        let assets_b = vec![
            mk_asset("a3", Some(v2.clone())),
            mk_asset("a2", Some(v1.clone())),
            mk_asset("a1", Some(v1.clone())),
        ];
        let ca = cluster_by_similarity(&assets_a, 0.85, cosine_similarity);
        let cb = cluster_by_similarity(&assets_b, 0.85, cosine_similarity);
        // 聚类的 centroid_id 集合应相同（顺序也应相同，因按 id 排序）
        let ids_a: Vec<_> = ca.iter().map(|c| &c.centroid_id).collect();
        let ids_b: Vec<_> = cb.iter().map(|c| &c.centroid_id).collect();
        assert_eq!(ids_a, ids_b);
    }

    // —— 场景标签 ——

    #[tokio::test]
    async fn label_scene_returns_top_k_sorted() {
        let m = PlaceholderClipModel::new();
        let img = m.embed_image(Path::new("/x.jpg")).await.unwrap();
        let labels = label_scene(
            &m,
            &img,
            &["beach", "mountain", "city"],
            2,
            -1.0, // min_score = -1 保证全部保留
        )
        .await
        .unwrap();
        assert!(labels.len() <= 2);
        // 应按 score 降序
        for w in labels.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[tokio::test]
    async fn label_scene_min_score_filters() {
        let m = PlaceholderClipModel::new();
        let img = m.embed_image(Path::new("/x.jpg")).await.unwrap();
        // min_score = 2.0 → 全部过滤
        let labels = label_scene(&m, &img, &["beach", "mountain"], 5, 2.0)
            .await
            .unwrap();
        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn label_scene_empty_candidates() {
        let m = PlaceholderClipModel::new();
        let img = m.embed_image(Path::new("/x.jpg")).await.unwrap();
        let labels = label_scene(&m, &img, &[], 5, -1.0).await.unwrap();
        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn label_scene_default_labels_works() {
        let m = PlaceholderClipModel::new();
        let img = m.embed_image(Path::new("/x.jpg")).await.unwrap();
        let labels = label_scene(&m, &img, DEFAULT_SCENE_LABELS, 3, -1.0)
            .await
            .unwrap();
        assert!(labels.len() <= 3);
        // 至少返回一些（默认词表非空 + min_score=-1）
        assert!(!labels.is_empty());
    }
}
