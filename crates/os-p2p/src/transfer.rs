//! P2P 传输组件——多源下载管理 + 网状分发（迅雷式 / CDN 式 swarming）。
//!
//! # 定位（2026-08-25 新增，用户定调）
//!
//! 「传输组件」= 文件在**已打通 NAT 的 os-p2p 叠加层**上分发：提供方把本地
//! 文件「发布为可传输」（生成分块清单），消费方拿 sha256 即可向连接的 peer
//! 询问源、逐块拉取。**分发不依赖任何公网 IP / underlay 地址**——所有帧走
//! 既有 overlay 消息通道（`Handle::send` / `on_msg`：直连 / TCP 打洞 / 中继
//! 信箱三阶梯送达），NAT 后节点互传与 aliyun↔内网 106 这类不可直连场景天然
//! 打通。本模块是「下载中心（aria2 公网 HTTP/BT）」的网状补位：公网走
//! downloads，节点间走 transfer。架构全貌见 docs/TRANSFER_COMPONENT.md。
//!
//! ```text
//!  ┌──────── 提供方（种子节点）────────┐        ┌──────── 消费方 ────────┐
//!  │ 本地文件 /tank/x.iso              │        │ fetch {sha256}         │
//!  │  └ publish → TransferManifest     │ ◀───── │  transfer_query 扇出   │
//!  │     {sha256, chunk_size, chunks[]}│ ─────▶ │  transfer_offer（源）  │
//!  │  └ registry 持久化（JSON）        │        │  逐批拉取（≤4 块在途） │
//!  │ 收到 transfer_chunk{index}        │ ◀───── │  transfer_chunk        │
//!  │  └ 定位读 → transfer_chunk_data   │ ─────▶ │  逐块 sha256 校验→落盘 │
//!  └──────────────────────────────────┘        │  完成→整文件校验→落名   │
//!        下载完成自动登记 registry ────────────▶│  → 自动成为新种子（swarm）│
//! ```
//!
//! # 协议帧（载荷 tag = `payload.transfer`，非 `fed`——与 FederationBridge 互不感知）
//!
//! | 帧 | 方向 | 载荷 | 语义 |
//! |---|---|---|---|
//! | `transfer_query` | 消费→各 peer | `{transfer, req_id, sha256?, transfer_id?}` | 「你有这个文件吗」（复用 im_lobby_query 请求-应答模式） |
//! | `transfer_offer` | 提供方→消费 | `{transfer, req_id, manifest}` | 有——回应完整清单 |
//! | `transfer_chunk` | 消费→提供方 | `{transfer, req_id, transfer_id, index}` | 拉取第 index 块 |
//! | `transfer_chunk_data` | 提供方→消费 | `{transfer, req_id, transfer_id, index, bytes(base64), sha256}` | 块字节 + 摘要 |
//! | `transfer_error` | 提供方→消费 | `{transfer, req_id, reason}` | 明确否定（读块失败 / 清单失配等） |
//!
//! # 分块大小的裁决：1 MiB（而非规划的 4 MiB）
//!
//! 勘察 transport.rs 结论：overlay 帧 = 长度前缀 JSON 信封（AES-GCM 加密后
//! 同限），单帧上限 [`crate::transport::MAX_FRAME_LEN`] = 4 MiB。4 MiB 原始
//! 块经 base64（×4/3 ≈ 5.6 MiB）+ 信封/密文开销**必然超限断连**，故缺省块
//! 1 MiB（线上 ≈ 1.4 MiB，留足余量）。清单带 `chunk_size` 字段，协议本身不
//! 锁死——后续二进制化路线：给 FrameKind 加专用二进制帧（免 base64/JSON 双
//! 重开销，块可提至 3 MiB），见 docs/TRANSFER_COMPONENT.md「后续路线」。
//! JSON+base64 起步的带宽代价：约 +33% 传输体积 + JSON 编解码 CPU，叠加层
//! 已加密场景可接受；另注意 on_msg 是 broadcast——每订阅者克隆一份载荷
//! （os-api 部署里联邦桥 + 本服务 = 2 份/块），大流量场景的二进制化收益翻倍。
//!
//! # 消费方引擎（背压 / 校验 / 重试 / 断点续传）
//!
//! - **背压**：在途 chunk ≤ [`MAX_INFLIGHT_CHUNKS`]（4）——一批（tokio::spawn
//!   真并发）收齐再发下一批，不把对端 pending_out（上限 128/目标）打满；
//! - **逐块校验**：每块 sha256 对清单 `chunks[index]`，坏块重试
//!   [`CHUNK_RETRIES`] 次（多源时轮转换源——单源则同源重试）；
//! - **断点续传**：完成块位图持久化为 `<name>.<sha256 前 8>.progress.json`
//!   （含清单本体），重新 fetch 同 sha256 只补缺失块；
//! - **完成即做种**：整文件 sha256 复核 → 原子 rename（`.part` → 终名）→
//!   自动登记 registry（CDN 式 swarming：每个下载完的节点都是新源）。
//!
//! # 测试
//!
//! `cargo test -p os-p2p transfer`：分块几何/清单摘要纯函数、registry 持久化、
//! 进度位图往返、双节点 spawn 实测（query→offer 经 overlay 送达、分块端到端
//! 字节级一致 + 自动做种、坏块重试耗尽、断点续传只补缺失块、并发上限、任务
//! 状态机 cancel/pause/resume、落地目录/名称覆盖、发布路径校验、非 transfer
//! 帧静默让路）。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::{oneshot, Notify, RwLock};

use crate::api::Handle;
use crate::identity::NodeId;

// ============================================================================
// 协议常量
// ============================================================================

/// 缺省分块大小：1 MiB（4 MiB 块经 base64 必超 4 MiB 帧上限——见模块文档裁决）。
pub const CHUNK_SIZE: u64 = 1024 * 1024;
/// 消费方在途 chunk 上限（背压：一批收齐再发下一批）。
pub const MAX_INFLIGHT_CHUNKS: usize = 4;
/// 单块拉取失败重试次数。
pub const CHUNK_RETRIES: u32 = 2;
/// fetch 的 query 阶段总窗口（多源聚合：窗口内到达的 offer 都计入源列表）。
pub const QUERY_WINDOW: Duration = Duration::from_secs(8);
/// 单块请求超时（含中继路径往返 + 提供方磁盘读）。
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// 清单分块数上限（防恶意清单：10 万块 × 1 MiB = 100 GiB 单文件封顶足够）。
pub const MANIFEST_MAX_CHUNKS: usize = 100_000;

/// 载荷类型标记：`payload.transfer == "transfer_query"`——「你有这个文件吗」。
pub const KIND_QUERY: &str = "transfer_query";
/// 载荷类型标记：`payload.transfer == "transfer_offer"`——query 的肯定应答（带清单）。
pub const KIND_OFFER: &str = "transfer_offer";
/// 载荷类型标记：`payload.transfer == "transfer_chunk"`——拉取单块。
pub const KIND_CHUNK_REQ: &str = "transfer_chunk";
/// 载荷类型标记：`payload.transfer == "transfer_chunk_data"`——块字节（base64）。
pub const KIND_CHUNK_DATA: &str = "transfer_chunk_data";
/// 载荷类型标记：`payload.transfer == "transfer_error"`——明确否定（读块失败等）。
pub const KIND_ERROR: &str = "transfer_error";

/// sha256 hex（64 字符小写）。
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

// ============================================================================
// 清单（manifest）
// ============================================================================

/// 文件传输清单——提供方的「可传输」描述（query 应答与分块校验的唯一真理源）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransferManifest {
    /// 传输 ID：`tr_` + 文件 sha256 前 16 hex（确定性——同内容文件全网同 ID，
    /// 消费方粘贴 sha256 或 transfer_id 二选一即可发起拉取）。
    pub transfer_id: String,
    /// 文件名（落盘缺省名）。
    pub name: String,
    /// 文件总字节数。
    pub size: u64,
    /// 整文件 sha256（64 hex）——内容寻址主键。
    pub sha256: String,
    /// 分块大小（协议不锁死；缺省 [`CHUNK_SIZE`]）。
    pub chunk_size: u64,
    /// 每块 sha256（长度 = 块数；末块可短）。
    pub chunks: Vec<String>,
    /// MIME 类型（可选，前端展示用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// 发布时间（unix 秒）。
    pub published_at: u64,
}

/// 块数（size=0 → 0 块——空文件无块可拉，完成路径直接整文件校验）。
#[must_use]
pub fn chunk_count(size: u64, chunk_size: u64) -> u64 {
    if size == 0 || chunk_size == 0 {
        0
    } else {
        size.div_ceil(chunk_size)
    }
}

/// 第 index 块的文件内偏移。
#[must_use]
pub fn chunk_offset(index: u64, chunk_size: u64) -> u64 {
    index * chunk_size
}

/// 第 index 块的字节长（末块取余数；越界 0）。
#[must_use]
pub fn chunk_len(index: u64, size: u64, chunk_size: u64) -> u64 {
    let start = chunk_offset(index, chunk_size);
    if start >= size {
        0
    } else {
        (size - start).min(chunk_size)
    }
}

/// 从 reader 流式建清单（不整读入内存：逐块缓冲摘要 + 全文摘要）。
fn build_manifest_from_reader<R: Read>(
    name: &str,
    size: u64,
    chunk_size: u64,
    mut reader: R,
) -> std::io::Result<TransferManifest> {
    use sha2::Digest;
    let mut whole = sha2::Sha256::new();
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; chunk_size as usize];
    let mut remaining = size;
    while remaining > 0 {
        let want = buf.len().min(remaining as usize);
        let mut got = 0usize;
        while got < want {
            let n = reader.read(&mut buf[got..want])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        if got < want {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("文件在读取中途变短（期望 {size} 字节）"),
            ));
        }
        whole.update(&buf[..got]);
        chunks.push(hex::encode(sha2::Sha256::digest(&buf[..got])));
        remaining -= got as u64;
    }
    if chunks.len() > MANIFEST_MAX_CHUNKS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "清单过大（{} 块 > 上限 {MANIFEST_MAX_CHUNKS}）",
                chunks.len()
            ),
        ));
    }
    let sha256 = hex::encode(whole.finalize());
    Ok(TransferManifest {
        transfer_id: format!("tr_{}", &sha256[..16]),
        name: name.to_string(),
        size,
        sha256,
        chunk_size,
        chunks,
        mime: None,
        published_at: crate::api::unix_now(),
    })
}

/// 读本地文件生成清单（同步阻塞——服务层经 spawn_blocking 调用）。
///
/// `name` 缺省取路径 basename；`mime` 按扩展名粗判（前端展示用，非安全边界）。
pub fn build_manifest(
    path: &Path,
    name: Option<&str>,
    chunk_size: u64,
) -> std::io::Result<TransferManifest> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("路径是目录，不可发布: {}", path.display()),
        ));
    }
    let size = meta.len();
    let name = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "transfer.bin".to_string())
        });
    let file = std::fs::File::open(path)?;
    let mut m = build_manifest_from_reader(&name, size, chunk_size, file)?;
    m.mime = mime_guess(&m.name);
    Ok(m)
}

/// 整文件 sha256 复核（流式；完成路径的最终防线）。
pub fn verify_whole_file(path: &Path, expected_sha256: &str) -> std::io::Result<bool> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()) == expected_sha256)
}

/// 扩展名 → MIME 粗判（展示用）。
fn mime_guess(name: &str) -> Option<String> {
    let ext = name.rsplit('.').next()?.to_ascii_lowercase();
    let m = match ext.as_str() {
        "iso" => "application/x-cd-image",
        "img" | "qcow2" | "vhd" => "application/x-disk-image",
        "zip" => "application/zip",
        "tar" | "tgz" | "gz" | "xz" | "zst" => "application/x-tar",
        "mp4" | "mkv" => "video/mp4",
        "mp3" | "flac" => "audio/mpeg",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "pdf" => "application/pdf",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    Some(m.to_string())
}

// ============================================================================
// 断点续传进度文件（块位图持久化）
// ============================================================================

/// 进度文件内容：清单本体 + 已完成块下标集（断点续传的持久化形态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressState {
    /// 清单（含块摘要——续传时无需再等 offer 才知道几何）。
    pub manifest: TransferManifest,
    /// 已完成块下标（升序持久化）。
    pub done: Vec<u64>,
}

/// 进度文件路径：`<dest_dir>/<name>.<sha256 前 8 hex>.progress.json`。
#[must_use]
pub fn progress_path(dest_dir: &Path, manifest: &TransferManifest) -> PathBuf {
    let safe_name = sanitize_filename(&manifest.name);
    dest_dir.join(format!(
        "{safe_name}.{}.progress.json",
        &manifest.sha256[..8]
    ))
}

/// 落地文件名清洗（防清单 name 携带路径穿越——只留 basename 语义字符）。
#[must_use]
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.rsplit('\\').next().unwrap_or(base);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    if cleaned.is_empty() {
        "transfer.bin".to_string()
    } else {
        cleaned
    }
}

/// 原子写进度文件（tmp + rename，同 bootstrap 私钥写法）。
pub fn save_progress(dest_dir: &Path, manifest: &TransferManifest, done: &HashSet<u64>) {
    let path = progress_path(dest_dir, manifest);
    let mut sorted: Vec<u64> = done.iter().copied().collect();
    sorted.sort_unstable();
    let state = ProgressState {
        manifest: manifest.clone(),
        done: sorted,
    };
    if let Ok(text) = serde_json::to_string(&state) {
        let tmp = path.with_extension("tmp");
        if std::fs::create_dir_all(dest_dir).is_ok() && std::fs::write(&tmp, &text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// 读进度文件（不存在 → None；损坏 → None 并告警——续传降级为全量重拉）。
pub fn load_progress(path: &Path) -> Option<ProgressState> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<ProgressState>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("进度文件损坏（{path:?}），忽略续传: {e}");
            None
        }
    }
}

// ============================================================================
// 种子注册表（本机可传输清单）
// ============================================================================

/// 注册表条目：清单 + 本地落点（提供方按它应答 query / 读块）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// 清单。
    pub manifest: TransferManifest,
    /// 本地文件路径（提供方读块源头）。
    pub path: PathBuf,
}

/// 本机种子注册表（发布 + 下载完成自动登记；JSON 持久化）。
#[derive(Debug, Default)]
pub struct TransferRegistry {
    entries: Vec<RegistryEntry>,
    file: Option<PathBuf>,
}

impl TransferRegistry {
    /// 空注册表（纯内存——测试用）。
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// 从文件加载（损坏 → 空表 + 告警，不阻塞服务）。
    #[must_use]
    pub fn load(file: Option<PathBuf>) -> Self {
        let mut reg = Self {
            entries: Vec::new(),
            file,
        };
        if let Some(path) = &reg.file {
            if let Ok(text) = std::fs::read_to_string(path) {
                match serde_json::from_str::<Vec<RegistryEntry>>(&text) {
                    Ok(entries) => reg.entries = entries,
                    Err(e) => tracing::warn!("传输注册表损坏（{path:?}），重建空表: {e}"),
                }
            }
        }
        reg
    }

    /// 原子持久化（tmp + rename）。
    pub fn persist(&self) {
        let Some(path) = &self.file else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(&self.entries) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, &text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// 发布本地文件为可传输（建清单 + 入表；同 sha256 去重——幂等）。
    pub fn publish(
        &mut self,
        path: &Path,
        name: Option<&str>,
        chunk_size: u64,
    ) -> std::io::Result<TransferManifest> {
        let manifest = build_manifest(path, name, chunk_size)?;
        self.insert_entry(manifest.clone(), path.to_path_buf());
        Ok(manifest)
    }

    /// 下载完成自动登记（CDN 式再分发的种子；同 sha256 去重）。
    pub fn register_completed(&mut self, manifest: TransferManifest, path: PathBuf) {
        self.insert_entry(manifest, path);
    }

    /// 入表去重半程（publish / register_completed 共用）。
    fn insert_entry(&mut self, manifest: TransferManifest, path: PathBuf) {
        if self
            .entries
            .iter()
            .any(|e| e.manifest.sha256 == manifest.sha256)
        {
            return;
        }
        self.entries.push(RegistryEntry { manifest, path });
        self.persist();
    }

    /// 下架（按 transfer_id 或 sha256 二选一匹配）。
    pub fn unpublish(&mut self, id_or_sha: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| e.manifest.sha256 != id_or_sha && e.manifest.transfer_id != id_or_sha);
        let removed = before != self.entries.len();
        if removed {
            self.persist();
        }
        removed
    }

    /// 按 sha256 / transfer_id 查找。
    #[must_use]
    pub fn find(&self, key: &str) -> Option<&RegistryEntry> {
        self.entries
            .iter()
            .find(|e| e.manifest.sha256 == key || e.manifest.transfer_id == key)
    }

    /// 全部条目（清单列表端点）。
    #[must_use]
    pub fn list(&self) -> Vec<RegistryEntry> {
        self.entries.clone()
    }

    /// 条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// 任务模型
// ============================================================================

/// 任务阶段（状态机：Querying→Downloading→Paused⇄/Completed/Failed/Cancelled）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    /// 询问源中（query 扇出等 offer）。
    Querying,
    /// 分块拉取中。
    Downloading,
    /// 已暂停（保留进度，可 resume）。
    Paused,
    /// 已完成（整文件校验过 + 已登记种子）。
    Completed,
    /// 失败（无源 / 块重试耗尽 / 整文件校验不符）。
    Failed,
    /// 已取消（保留进度文件，重新 fetch 可续传）。
    Cancelled,
}

impl TaskPhase {
    /// 前端状态词（与 downloads 任务词汇对齐：pending/downloading/paused/completed/error）。
    #[must_use]
    pub fn as_status(&self) -> &'static str {
        match self {
            TaskPhase::Querying => "pending",
            TaskPhase::Downloading => "downloading",
            TaskPhase::Paused => "paused",
            TaskPhase::Completed => "completed",
            TaskPhase::Failed => "error",
            TaskPhase::Cancelled => "error",
        }
    }
}

/// 任务观察面 DTO（`GET /transfer/tasks` 单条）。
#[derive(Debug, Clone, Serialize)]
pub struct TransferTaskView {
    /// 任务 ID（`task-<seq>`）。
    pub id: String,
    /// 文件名（offer 到达前用 name 提示或缺省）。
    pub name: String,
    /// 目标 sha256（fetch 入参归一；transfer_id 发起时首份 offer 后补全）。
    pub sha256: String,
    /// 清单 transfer_id（offer 到达后填充）。
    pub transfer_id: Option<String>,
    /// 阶段。
    pub phase: TaskPhase,
    /// 前端状态词。
    pub status: String,
    /// 文件总字节。
    pub size_bytes: u64,
    /// 已完成字节。
    pub done_bytes: u64,
    /// 进度 0-100。
    pub progress: u32,
    /// 总块数。
    pub chunks_total: u64,
    /// 已完成块数。
    pub chunks_done: u64,
    /// 当前速度（快照窗口差分）。
    pub speed_bytes_sec: u64,
    /// 源节点短 ID（前 4 + 后 4 hex）。
    pub sources: Vec<String>,
    /// 落地路径（完成前为 `.part` 路径）。
    pub dest_path: Option<String>,
    /// 失败原因（Failed 时）。
    pub error: Option<String>,
    /// 创建时间（unix 秒）。
    pub created_at: u64,
    /// 在途 chunk 批量的实际观测峰值（测试/诊断背压）。
    pub max_inflight_seen: usize,
}

/// 任务控制信号（pause/resume/cancel 经原子位 + 唤醒器作用于引擎循环）。
#[derive(Default)]
struct TaskControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
    resume: Arc<Notify>,
}

/// 引擎持有的任务共享态（观察面与控制面同源）。
struct TaskShared {
    id: String,
    /// 目标 sha256（fetch 入参归一；transfer_id 发起时首份 offer 后补全）。
    sha256: Mutex<String>,
    name_hint: Option<String>,
    dest_dir: PathBuf,
    manifest: Mutex<Option<TransferManifest>>,
    phase: Mutex<TaskPhase>,
    done: Mutex<HashSet<u64>>,
    sources: Mutex<Vec<NodeId>>,
    error: Mutex<String>,
    dest_path: Mutex<Option<PathBuf>>,
    done_bytes: AtomicU64,
    speed_window: Mutex<Option<(Instant, u64)>>,
    speed: AtomicU64,
    max_inflight: AtomicUsize,
    control: TaskControl,
    created_at: u64,
}

impl TaskShared {
    fn new(id: String, sha256: String, name_hint: Option<String>, dest_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            id,
            sha256: Mutex::new(sha256),
            name_hint,
            dest_dir,
            manifest: Mutex::new(None),
            phase: Mutex::new(TaskPhase::Querying),
            done: Mutex::new(HashSet::new()),
            sources: Mutex::new(Vec::new()),
            error: Mutex::new(String::new()),
            dest_path: Mutex::new(None),
            done_bytes: AtomicU64::new(0),
            speed_window: Mutex::new(None),
            speed: AtomicU64::new(0),
            max_inflight: AtomicUsize::new(0),
            control: TaskControl::default(),
            created_at: crate::api::unix_now(),
        })
    }

    fn set_phase(&self, phase: TaskPhase) {
        *self.phase.lock().expect("phase poisoned") = phase;
    }

    fn phase(&self) -> TaskPhase {
        *self.phase.lock().expect("phase poisoned")
    }

    fn set_error(&self, msg: impl Into<String>) {
        *self.error.lock().expect("error poisoned") = msg.into();
    }

    /// 观察面 DTO（进度按完成块字节精确计；速度 = 两次快照的窗口差分）。
    fn view(&self) -> TransferTaskView {
        let manifest = self.manifest.lock().expect("manifest poisoned").clone();
        let (total, chunk_total, chunk_size) = match &manifest {
            Some(m) => (m.size, m.chunks.len() as u64, m.chunk_size),
            None => (0, 0, 0),
        };
        let done_set = self.done.lock().expect("done poisoned");
        let chunks_done = done_set.len() as u64;
        let computed: u64 = if chunk_total > 0 {
            done_set
                .iter()
                .map(|&i| chunk_len(i, total, chunk_size))
                .sum()
        } else {
            self.done_bytes.load(Ordering::Relaxed)
        };
        drop(done_set);
        let progress = if total > 0 {
            ((computed.min(total) as f64 / total as f64) * 100.0).round() as u32
        } else if self.phase() == TaskPhase::Completed {
            100
        } else {
            0
        };
        // 速度窗口差分：首次快照开窗，其后每 ≥0.5s 结算一次
        let mut window = self.speed_window.lock().expect("speed poisoned");
        if let Some((at, base)) = *window {
            let elapsed = Instant::now().duration_since(at).as_secs_f64();
            if elapsed >= 0.5 {
                let bps = ((self.done_bytes.load(Ordering::Relaxed)).saturating_sub(base) as f64
                    / elapsed) as u64;
                self.speed.store(bps, Ordering::Relaxed);
                *window = Some((Instant::now(), self.done_bytes.load(Ordering::Relaxed)));
            }
        } else {
            *window = Some((Instant::now(), self.done_bytes.load(Ordering::Relaxed)));
        }
        drop(window);
        let sources: Vec<String> = self
            .sources
            .lock()
            .expect("sources poisoned")
            .iter()
            .map(short_node)
            .collect();
        let error = self.error.lock().expect("error poisoned").clone();
        let phase = self.phase();
        TransferTaskView {
            id: self.id.clone(),
            name: manifest
                .as_ref()
                .map(|m| m.name.clone())
                .or_else(|| self.name_hint.clone())
                .unwrap_or_else(|| self.sha256.lock().expect("sha256 poisoned").clone()),
            sha256: self.sha256.lock().expect("sha256 poisoned").clone(),
            transfer_id: manifest.as_ref().map(|m| m.transfer_id.clone()),
            status: phase.as_status().to_string(),
            phase,
            size_bytes: total,
            done_bytes: computed,
            progress: progress.min(100),
            chunks_total: chunk_total,
            chunks_done,
            speed_bytes_sec: self.speed.load(Ordering::Relaxed),
            sources,
            dest_path: self
                .dest_path
                .lock()
                .expect("dest poisoned")
                .as_ref()
                .map(|p| p.display().to_string()),
            error: if error.is_empty() { None } else { Some(error) },
            created_at: self.created_at,
            max_inflight_seen: self.max_inflight.load(Ordering::Relaxed),
        }
    }
}

/// NodeID 短式（`0x1234…cdef`——观察面/日志）。
fn short_node(id: &NodeId) -> String {
    let hex = id.to_hex();
    let n = hex.len();
    format!("{}…{}", &hex[..6], &hex[n - 4..])
}

// ============================================================================
// 服务
// ============================================================================

/// 传输服务配置（os-api 装配层经 [`TransferConfig::from_env`] 构建；测试直构）。
#[derive(Debug, Clone)]
pub struct TransferConfig {
    /// 落地目录（缺省 `/tank/downloads`，env `NEXOS_TRANSFER_DIR`）。
    pub dest_dir: PathBuf,
    /// 种子注册表持久化文件（None = 纯内存——测试用）。
    pub registry_file: Option<PathBuf>,
    /// 缺省分块大小（测试可压小——协议字段，双端按清单对齐）。
    pub chunk_size: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            dest_dir: PathBuf::from("/tank/downloads"),
            registry_file: None,
            chunk_size: CHUNK_SIZE,
        }
    }
}

impl TransferConfig {
    /// env 装配：`NEXOS_TRANSFER_DIR`（缺省 /tank/downloads）+
    /// `NEXOS_TRANSFER_REGISTRY`（缺省 `<dir>/.transfer-registry.json`）+
    /// `NEXOS_TRANSFER_CHUNK`（缺省 1 MiB；压小仅建议测试用）。
    #[must_use]
    pub fn from_env() -> Self {
        let dest_dir = std::env::var("NEXOS_TRANSFER_DIR")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tank/downloads"));
        let registry_file = std::env::var("NEXOS_TRANSFER_REGISTRY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(dest_dir.join(".transfer-registry.json")));
        let chunk_size = std::env::var("NEXOS_TRANSFER_CHUNK")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(CHUNK_SIZE);
        Self {
            dest_dir,
            registry_file,
            chunk_size,
        }
    }
}

/// 服务运行统计（观察面）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct TransferStats {
    /// 本机已发布清单数。
    pub manifests: usize,
    /// 任务总数。
    pub tasks: usize,
    /// 进行中（querying + downloading）任务数。
    pub active: usize,
    /// 已完成字节数（全部任务累计）。
    pub done_bytes_total: u64,
    /// 本机应答 query 次数（有源命中）。
    pub queries_answered: u64,
    /// 本机已供出块数（做种贡献）。
    pub chunks_served: u64,
    /// 本机已供出字节数。
    pub bytes_served: u64,
}

/// 请求-应答等待条目：应答到达时携带来源 NodeID（offer 归属真实应答方）。
type PeerReply = (NodeId, serde_json::Value);

/// 服务共享内核（ingress 任务与下载引擎共持）。
struct Inner {
    handle: Handle,
    config: TransferConfig,
    registry: Mutex<TransferRegistry>,
    tasks: Mutex<HashMap<String, Arc<TaskShared>>>,
    /// 请求-应答关联：req_id → 应答 oneshot（query/chunk 共用）。
    pending: Mutex<HashMap<String, oneshot::Sender<PeerReply>>>,
    req_seq: AtomicU64,
    task_seq: AtomicU64,
    queries_answered: AtomicU64,
    chunks_served: AtomicU64,
    bytes_served: AtomicU64,
}

/// P2P 传输服务：一个 os-p2p Handle 上的提供方（应答 query / 供块）+ 消费方
/// （任务引擎）。[`TransferService::spawn`] 起常驻 ingress 任务订阅 `on_msg`，
/// 只消费 `payload.transfer` 帧（其余静默让路——与联邦桥互不干扰）。
pub struct TransferService {
    inner: Arc<Inner>,
}

impl TransferService {
    /// 起服务（**必须在 tokio runtime 内**）：加载/持久化注册表 + spawn
    /// ingress 任务。返回共享句柄（Arc——os-api handler 与装配层共持）。
    pub fn spawn(handle: Handle, config: TransferConfig) -> Arc<Self> {
        let registry = TransferRegistry::load(config.registry_file.clone());
        let inner = Arc::new(Inner {
            handle,
            config,
            registry: Mutex::new(registry),
            tasks: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            req_seq: AtomicU64::new(1),
            task_seq: AtomicU64::new(1),
            queries_answered: AtomicU64::new(0),
            chunks_served: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
        });
        let service = Arc::new(Self {
            inner: inner.clone(),
        });
        let mut rx = inner.handle.on_msg();
        let svc = service.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => svc.handle_inbound(&msg.from, msg.payload),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("[transfer] 观测落后 {n} 条（跳过）");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        service
    }

    // ------------------------------------------------------------------
    // 入站分发（提供方 + 应答路由）
    // ------------------------------------------------------------------

    /// 处理一条入站 overlay 消息：transfer 帧三种去向——应答类（query/chunk）
    /// 走提供方路径，回应类（offer/chunk_data/error）按 req_id 唤醒等待者。
    fn handle_inbound(&self, from: &NodeId, payload: serde_json::Value) {
        let Some(kind) = payload.get("transfer").and_then(|v| v.as_str()) else {
            return; // 非传输帧（联邦桥/调试消息），让路
        };
        match kind {
            KIND_QUERY => self.answer_query(from, &payload),
            KIND_CHUNK_REQ => {
                // 供块可耗时（磁盘读 + 大块序列化）——spawn 独立任务，不阻塞
                // ingress 循环（多消费方并发拉块不被串行化）。
                let svc = Self {
                    inner: self.inner.clone(),
                };
                let from = from.clone();
                tokio::spawn(async move {
                    svc.serve_chunk(&from, &payload).await;
                });
            }
            KIND_OFFER | KIND_CHUNK_DATA | KIND_ERROR => {
                let req_id = payload
                    .get("req_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if let Some(tx) = self
                    .inner
                    .pending
                    .lock()
                    .expect("pending poisoned")
                    .remove(&req_id)
                {
                    let _ = tx.send((from.clone(), payload));
                }
            }
            _ => {}
        }
    }

    /// 应答 transfer_query：注册表命中（且本地文件仍在）→ offer 带完整清单；
    /// 未命中保持沉默（query 是扇出探测，沉默 = 没有；消费方按窗口超时聚合）。
    fn answer_query(&self, from: &NodeId, payload: &serde_json::Value) {
        let sha256 = payload.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
        let transfer_id = payload
            .get("transfer_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if sha256.is_empty() && transfer_id.is_empty() {
            return;
        }
        let registry = self.inner.registry.lock().expect("registry poisoned");
        let Some(entry) = registry.find(sha256).or_else(|| registry.find(transfer_id)) else {
            return;
        };
        if !entry.path.exists() {
            tracing::warn!(
                "[transfer] query 命中但本地文件已失：{}",
                entry.path.display()
            );
            return;
        }
        let manifest = entry.manifest.clone();
        drop(registry);
        self.inner.queries_answered.fetch_add(1, Ordering::Relaxed);
        let req_id = payload
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.inner.handle.send(
            from,
            serde_json::json!({
                "transfer": KIND_OFFER,
                "req_id": req_id,
                "manifest": manifest,
            }),
        );
    }

    /// 应答 transfer_chunk：定位读单块 → 供出前自校验摘要 → chunk_data（base64）。
    async fn serve_chunk(&self, from: &NodeId, payload: &serde_json::Value) {
        let req_id = payload
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let transfer_id = payload
            .get("transfer_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let index = payload
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX);
        let reply_error = |reason: String| {
            self.inner.handle.send(
                from,
                serde_json::json!({
                    "transfer": KIND_ERROR,
                    "req_id": req_id,
                    "reason": reason,
                }),
            );
        };
        let entry = {
            let registry = self.inner.registry.lock().expect("registry poisoned");
            registry.find(transfer_id).cloned()
        };
        let Some(entry) = entry else {
            reply_error(format!("未知 transfer_id: {transfer_id}"));
            return;
        };
        let m = &entry.manifest;
        let Some(expected) = m.chunks.get(index as usize) else {
            reply_error(format!("块下标越界: {index}（共 {} 块）", m.chunks.len()));
            return;
        };
        let want = chunk_len(index, m.size, m.chunk_size) as usize;
        if want == 0 {
            reply_error(format!("块 {index} 长度为 0（文件大小 {}）", m.size));
            return;
        }
        // 定位读（tokio 异步；1 MiB 级块不占 worker 过久）
        let mut file = match tokio::fs::File::open(&entry.path).await {
            Ok(f) => f,
            Err(e) => {
                reply_error(format!("本地文件不可读（{}）: {e}", entry.path.display()));
                return;
            }
        };
        if file
            .seek(SeekFrom::Start(chunk_offset(index, m.chunk_size)))
            .await
            .is_err()
        {
            reply_error("定位读取失败".into());
            return;
        }
        let mut buf = vec![0u8; want];
        if let Err(e) = file.read_exact(&mut buf).await {
            reply_error(format!("读块失败: {e}"));
            return;
        }
        // 供出前自校验（文件在发布后被改动 → 拒供，防污染扩散）
        let digest = sha256_hex(&buf);
        if digest != *expected {
            reply_error(format!("本地块摘要不符（文件疑似被改动），拒供块 {index}"));
            return;
        }
        self.inner.chunks_served.fetch_add(1, Ordering::Relaxed);
        self.inner
            .bytes_served
            .fetch_add(buf.len() as u64, Ordering::Relaxed);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        self.inner.handle.send(
            from,
            serde_json::json!({
                "transfer": KIND_CHUNK_DATA,
                "req_id": req_id,
                "transfer_id": transfer_id,
                "index": index,
                "sha256": digest,
                "bytes": b64,
            }),
        );
    }

    // ------------------------------------------------------------------
    // 提供方 API（发布 / 下架 / 清单）
    // ------------------------------------------------------------------

    /// 发布本地文件为可传输（spawn_blocking 建清单——大文件摘要不占 worker）。
    pub async fn publish(
        &self,
        path: &Path,
        name: Option<&str>,
    ) -> std::io::Result<TransferManifest> {
        let chunk_size = self.inner.config.chunk_size;
        let path_for_build = path.to_path_buf();
        let name = name.map(str::to_string);
        let manifest = tokio::task::spawn_blocking(move || {
            build_manifest(&path_for_build, name.as_deref(), chunk_size)
        })
        .await
        .map_err(|e| std::io::Error::other(format!("清单任务失败: {e}")))??;
        // 上表（同 sha256 幂等去重）
        self.inner
            .registry
            .lock()
            .expect("registry poisoned")
            .insert_entry(manifest.clone(), path.to_path_buf());
        Ok(manifest)
    }

    /// 下架清单（按 transfer_id 或 sha256）。
    pub fn unpublish(&self, key: &str) -> bool {
        self.inner
            .registry
            .lock()
            .expect("registry poisoned")
            .unpublish(key)
    }

    /// 本机已发布清单（含本地路径——管理面展示）。
    #[must_use]
    pub fn manifests(&self) -> Vec<RegistryEntry> {
        self.inner
            .registry
            .lock()
            .expect("registry poisoned")
            .list()
    }

    // ------------------------------------------------------------------
    // 消费方 API（fetch / 任务控制 / 观察）
    // ------------------------------------------------------------------

    /// 发起 P2P 拉取任务。`sha256_or_id`：64 hex 全文摘要或 `tr_…` 清单 ID。
    /// 立即返回任务 ID（query→offer→分块拉取全部在后台任务推进）。
    pub async fn fetch(&self, sha256_or_id: &str, name: Option<&str>) -> String {
        let key = sha256_or_id.trim().to_string();
        let id = format!(
            "task-{}",
            self.inner.task_seq.fetch_add(1, Ordering::Relaxed)
        );
        let task = TaskShared::new(
            id.clone(),
            key.clone(),
            name.map(str::to_string),
            self.inner.config.dest_dir.clone(),
        );
        self.inner
            .tasks
            .lock()
            .expect("tasks poisoned")
            .insert(id.clone(), task.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_fetch(inner, task).await;
        });
        id
    }

    /// 暂停任务（引擎在批间观察；在途块收完即停，进度已持久化）。
    pub async fn pause_task(&self, id: &str) -> bool {
        let Some(task) = self
            .inner
            .tasks
            .lock()
            .expect("tasks poisoned")
            .get(id)
            .cloned()
        else {
            return false;
        };
        if matches!(task.phase(), TaskPhase::Querying | TaskPhase::Downloading) {
            task.control.paused.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// 继续任务（Paused → Downloading 恢复拉取）。
    pub async fn resume_task(&self, id: &str) -> bool {
        let Some(task) = self
            .inner
            .tasks
            .lock()
            .expect("tasks poisoned")
            .get(id)
            .cloned()
        else {
            return false;
        };
        if task.phase() == TaskPhase::Paused {
            task.control.paused.store(false, Ordering::SeqCst);
            task.control.resume.notify_waiters();
            true
        } else {
            false
        }
    }

    /// 取消任务（保留进度文件——重新 fetch 同 sha256 续传）。
    pub fn cancel_task(&self, id: &str) -> bool {
        let Some(task) = self
            .inner
            .tasks
            .lock()
            .expect("tasks poisoned")
            .get(id)
            .cloned()
        else {
            return false;
        };
        if matches!(
            task.phase(),
            TaskPhase::Querying | TaskPhase::Downloading | TaskPhase::Paused
        ) {
            task.control.cancelled.store(true, Ordering::SeqCst);
            task.control.resume.notify_waiters(); // 唤醒暂停中的引擎使其观察取消
            true
        } else {
            false
        }
    }

    /// 全部任务快照（新→旧）。
    #[must_use]
    pub fn tasks(&self) -> Vec<TransferTaskView> {
        let tasks: Vec<Arc<TaskShared>> = self
            .inner
            .tasks
            .lock()
            .expect("tasks poisoned")
            .values()
            .cloned()
            .collect();
        let mut views: Vec<TransferTaskView> = tasks.iter().map(|t| t.view()).collect();
        views.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        views
    }

    /// 单任务快照。
    #[must_use]
    pub fn task(&self, id: &str) -> Option<TransferTaskView> {
        self.inner
            .tasks
            .lock()
            .expect("tasks poisoned")
            .get(id)
            .map(|t| t.view())
    }

    /// 服务统计（观察面）。
    #[must_use]
    pub fn stats(&self) -> TransferStats {
        let manifests = self.inner.registry.lock().expect("registry poisoned").len();
        let tasks = self.tasks();
        let active = tasks
            .iter()
            .filter(|t| matches!(t.phase, TaskPhase::Querying | TaskPhase::Downloading))
            .count();
        TransferStats {
            manifests,
            tasks: tasks.len(),
            active,
            done_bytes_total: tasks.iter().map(|t| t.done_bytes).sum(),
            queries_answered: self.inner.queries_answered.load(Ordering::Relaxed),
            chunks_served: self.inner.chunks_served.load(Ordering::Relaxed),
            bytes_served: self.inner.bytes_served.load(Ordering::Relaxed),
        }
    }

    /// 单轮询问源（query 扇出 → 首个 offer）：独立暴露供测试/诊断「谁有这个
    /// 文件」。窗口 2s。
    pub async fn query_manifest(&self, key: &str) -> Option<(TransferManifest, NodeId)> {
        let (sha256, transfer_id) = split_key(key);
        query_fanout(&self.inner, &sha256, &transfer_id, Duration::from_secs(2)).await
    }
}

/// 输入键拆分：`tr_…` → transfer_id；否则按 sha256（小写归一）。
fn split_key(key: &str) -> (String, String) {
    let key = key.trim();
    if let Some(rest) = key.strip_prefix("tr_") {
        (String::new(), format!("tr_{rest}"))
    } else {
        (key.to_ascii_lowercase(), String::new())
    }
}

// ============================================================================
// 消费方引擎（run_fetch：query → 分块拉取 → 校验落地 → 自动做种）
// ============================================================================

/// query 扇出：向全部已连接 peer 问「有没有」，返回首个对题 offer + 应答方。
async fn query_fanout(
    inner: &Arc<Inner>,
    sha256: &str,
    transfer_id: &str,
    window: Duration,
) -> Option<(TransferManifest, NodeId)> {
    let peers: Vec<NodeId> = inner
        .handle
        .peers()
        .await
        .into_iter()
        .filter(|p| p.connected && !inner.handle.is_local_target(&p.id))
        .map(|p| p.id)
        .collect();
    if peers.is_empty() {
        return None;
    }
    let req_id = format!("q{}", inner.req_seq.fetch_add(1, Ordering::Relaxed));
    let (tx, rx) = oneshot::channel();
    inner
        .pending
        .lock()
        .expect("pending poisoned")
        .insert(req_id.clone(), tx);
    let payload = serde_json::json!({
        "transfer": KIND_QUERY,
        "req_id": req_id,
        "sha256": sha256,
        "transfer_id": transfer_id,
    });
    for p in &peers {
        inner.handle.send(p, payload.clone());
    }
    let (from, offer) = tokio::time::timeout(window, rx).await.ok()?.ok()?;
    let manifest = serde_json::from_value::<TransferManifest>(
        offer
            .get("manifest")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .ok()?;
    // 对题校验：应答必须命中询问键（防串台）
    if manifest.sha256 != sha256 && manifest.transfer_id != transfer_id {
        return None;
    }
    Some((manifest, from))
}

/// 引擎主循环（tokio::spawn 拉起；状态机见 [`TaskPhase`]）。
async fn run_fetch(inner: Arc<Inner>, task: Arc<TaskShared>) {
    let key = task.sha256.lock().expect("sha256 poisoned").clone();

    // —— 0) 断点续传：落地目录里扫 progress 文件（sha256/transfer_id 匹配）——
    let mut manifest: Option<TransferManifest> = None;
    let mut resumed: HashSet<u64> = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&task.dest_dir) {
        for e in entries.flatten() {
            let p = e.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !name.ends_with(".progress.json") {
                continue;
            }
            if let Some(state) = load_progress(&p) {
                let m = &state.manifest;
                if m.sha256 == key || m.transfer_id == key {
                    resumed = state
                        .done
                        .iter()
                        .copied()
                        .filter(|&i| i < m.chunks.len() as u64)
                        .collect();
                    manifest = Some(m.clone());
                    break;
                }
            }
        }
    }
    // 位图与 .part 一致性校验：进度文件在而 .part 缺失/变短（被清理或上次
    // 写盘中断）→ 超出 .part 实际长度的"已完成"块作废重拉（宁可重拉不可落
    // 空洞——块校验只校验拉到的字节，空洞会让整文件校验兜底失败）。
    if let Some(m) = &manifest {
        if !resumed.is_empty() {
            let part = part_path_for(&task.dest_dir, &task.name_hint, m);
            let part_len = std::fs::metadata(&part).map(|md| md.len()).unwrap_or(0);
            resumed.retain(|&i| {
                chunk_offset(i, m.chunk_size) + chunk_len(i, m.size, m.chunk_size) <= part_len
            });
        }
    }

    // —— 1) Querying：逐源定向询问（首份清单 + 源列表；窗口内补源）——
    let peers: Vec<NodeId> = inner
        .handle
        .peers()
        .await
        .into_iter()
        .filter(|p| p.connected && !inner.handle.is_local_target(&p.id))
        .map(|p| p.id)
        .collect();
    if peers.is_empty() && manifest.is_none() {
        task.set_error("无可达源节点（当前无已连接 peer）");
        task.set_phase(TaskPhase::Failed);
        return;
    }
    if task.control.cancelled.load(Ordering::SeqCst) {
        task.set_phase(TaskPhase::Cancelled);
        return;
    }
    let mut sources: Vec<NodeId> = Vec::new();
    if manifest.is_none() {
        let deadline = Instant::now() + QUERY_WINDOW;
        'query: while Instant::now() < deadline {
            for p in &peers {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if let Some((m, src)) = query_peer(
                    inner.clone(),
                    p,
                    &key,
                    remaining.min(Duration::from_secs(3)),
                )
                .await
                {
                    if !sources.contains(&src) {
                        sources.push(src);
                    }
                    manifest = Some(m);
                    break 'query; // 首份清单到手即开始下载（下方补源）
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    let Some(manifest) = manifest else {
        task.set_error(format!(
            "查询窗口内无源应答（{QUERY_WINDOW:?}，问了 {} 个 peer）",
            peers.len()
        ));
        task.set_phase(TaskPhase::Failed);
        return;
    };
    if manifest.chunks.len() > MANIFEST_MAX_CHUNKS {
        task.set_error("清单块数超上限，拒绝（防恶意清单）");
        task.set_phase(TaskPhase::Failed);
        return;
    }
    // 回填任务（sha256 归一——transfer_id 发起时首份 offer 补全）
    {
        let mut m = task.manifest.lock().expect("manifest poisoned");
        *m = Some(manifest.clone());
    }
    *task.sha256.lock().expect("sha256 poisoned") = manifest.sha256.clone();
    // 补源：窗口内再快速探一轮其余 peer（多源 = 坏块换源 + 带宽分摊）
    let mut extra: Vec<NodeId> = Vec::new();
    for p in peers.iter().filter(|p| !sources.contains(p)) {
        if let Some((m, src)) = query_peer(inner.clone(), p, &key, Duration::from_millis(500)).await
        {
            if m.sha256 == manifest.sha256 {
                extra.push(src);
            }
        }
    }
    sources.extend(extra);
    if sources.is_empty() {
        sources = peers.clone(); // 续传场景（清单来自进度文件）：全连接 peer 兜底
    }
    *task.sources.lock().expect("sources poisoned") = sources.clone();

    // —— 2) Downloading：批式分块拉取（≤MAX_INFLIGHT_CHUNKS 真并发在途）——
    task.set_phase(TaskPhase::Downloading);
    let part_path = part_path_for(&task.dest_dir, &task.name_hint, &manifest);
    *task.dest_path.lock().expect("dest poisoned") = Some(part_path.clone());
    // 续传：登记已完成块 + 字节量回填（task.done 是观察面的位图真源——引擎
    // 每次推进后同步）
    let mut done = resumed;
    sync_done(&task, &done);
    let mut done_bytes: u64 = done
        .iter()
        .map(|&i| chunk_len(i, manifest.size, manifest.chunk_size))
        .sum();
    task.done_bytes.store(done_bytes, Ordering::Relaxed);
    let total = manifest.chunks.len() as u64;
    if total > 0 && done.len() as u64 == total {
        // 位图已满（上次中断在收尾前）——直接进收尾校验
        if finish_download(&inner, &task, &manifest).await {
            return;
        }
    }
    // 续传语义：显式 truncate(false)——.part 里已落块的内容就是断点续传的本钱；
    // 落地目录不存在则建（生产 /tank/downloads 可能尚未初始化）。
    if let Err(e) = std::fs::create_dir_all(&task.dest_dir) {
        task.set_error(format!(
            "落地目录不可建（{}）: {e}",
            task.dest_dir.display()
        ));
        task.set_phase(TaskPhase::Failed);
        return;
    }
    let file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&part_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            task.set_error(format!("落地文件打开失败（{}）: {e}", part_path.display()));
            task.set_phase(TaskPhase::Failed);
            return;
        }
    };
    let file = Arc::new(RwLock::new(file));

    let mut next_missing: Vec<u64> = (0..total).filter(|i| !done.contains(i)).collect();
    let mut attempts: HashMap<u64, u32> = HashMap::new();
    while !next_missing.is_empty() {
        if task.control.cancelled.load(Ordering::SeqCst) {
            persist_and_mark(&task, &manifest, &done, TaskPhase::Cancelled);
            return;
        }
        if task.control.paused.load(Ordering::SeqCst) {
            persist_and_mark(&task, &manifest, &done, TaskPhase::Paused);
            task.control.resume.notified().await;
            if task.control.cancelled.load(Ordering::SeqCst) {
                persist_and_mark(&task, &manifest, &done, TaskPhase::Cancelled);
                return;
            }
            task.set_phase(TaskPhase::Downloading);
        }
        // 取一批（≤并发上限）真并发拉取：tokio::spawn 各自在途，
        // 收齐再进下一批（背压），批内 slot 轮转分散到不同源
        let batch: Vec<u64> = next_missing
            .iter()
            .copied()
            .take(MAX_INFLIGHT_CHUNKS)
            .collect();
        task.max_inflight.store(
            task.max_inflight.load(Ordering::Relaxed).max(batch.len()),
            Ordering::Relaxed,
        );
        let mut handles = Vec::with_capacity(batch.len());
        for (slot, &index) in batch.iter().enumerate() {
            let source = pick_source(&sources, slot);
            let inner2 = inner.clone();
            let m2 = manifest.clone();
            handles.push((
                index,
                tokio::spawn(async move { pull_chunk(inner2, source, &m2, index).await }),
            ));
        }
        let mut progressed = false;
        let mut batch_failed_all = true;
        for (index, h) in handles {
            let result = h.await.unwrap_or_else(|e| Err(format!("块任务失败: {e}")));
            match result {
                Ok(bytes) => {
                    // 定位写（RwLock 串行化并发写——≤4 在途，无争用面）
                    let offset = chunk_offset(index, manifest.chunk_size);
                    {
                        let mut f = file.write().await;
                        let _ = f.seek(SeekFrom::Start(offset)).await;
                        if let Err(e) = f.write_all(&bytes).await {
                            task.set_error(format!("写块 {index} 失败: {e}"));
                            task.set_phase(TaskPhase::Failed);
                            return;
                        }
                    }
                    done.insert(index);
                    done_bytes = done_bytes.saturating_add(bytes.len() as u64);
                    task.done_bytes.store(done_bytes, Ordering::Relaxed);
                    progressed = true;
                    batch_failed_all = false;
                }
                Err(e) => {
                    let n = attempts.entry(index).or_insert(0);
                    *n += 1;
                    tracing::warn!("[transfer] 块 {index} 拉取失败（第 {n} 次）: {e}");
                    if *n > CHUNK_RETRIES {
                        task.set_error(format!("块 {index} 重试 {CHUNK_RETRIES} 次仍失败: {e}"));
                        persist_and_mark(&task, &manifest, &done, TaskPhase::Failed);
                        return;
                    }
                }
            }
        }
        // 重算缺失集（失败块留在缺失集里重试；成功块移出）
        next_missing = (0..total).filter(|i| !done.contains(i)).collect();
        sync_done(&task, &done);
        if progressed {
            save_progress(&task.dest_dir, &manifest, &done);
        }
        if batch_failed_all && !progressed {
            // 整批全败且零进展——退避防忙转（对端可能正在重连）
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    // 收尾前显式冲刷：tokio File 有内部写缓冲，drop 是后台异步刷——不等它
    // 就做整文件校验会读到缺尾数据（校验必败）。sync_all 顺带落盘。
    {
        let mut f = file.write().await;
        let _ = f.flush().await;
        let _ = f.sync_all().await;
    }
    drop(file);

    // —— 3) 收尾：整文件校验 → 原子落名 → 自动做种 ——
    if !finish_download(&inner, &task, &manifest).await {
        task.set_error("整文件 sha256 校验不符（落地 .part 已删除，可重试续传）");
        task.set_phase(TaskPhase::Failed);
    }
}

/// `.part` 落地路径（run_fetch 与 finish_download 共用——name_hint 覆盖时
/// 两处必须一致，否则收尾找不到半成品）。
fn part_path_for(
    dest_dir: &Path,
    name_hint: &Option<String>,
    manifest: &TransferManifest,
) -> PathBuf {
    let dest_name = sanitize_filename(&name_hint.clone().unwrap_or_else(|| manifest.name.clone()));
    dest_dir.join(format!("{dest_name}.{}.part", &manifest.sha256[..8]))
}

/// 收尾路径：整文件复核 + rename + 清进度 + 自动登记种子。
/// 返回 true = 终态已置（Completed 或 Failed——含失败原因）。
async fn finish_download(
    inner: &Arc<Inner>,
    task: &Arc<TaskShared>,
    manifest: &TransferManifest,
) -> bool {
    let part_path = part_path_for(&task.dest_dir, &task.name_hint, manifest);
    *task.dest_path.lock().expect("dest poisoned") = Some(part_path.clone());
    // 空文件场景：part 可能从未创建（0 块）——补建空文件
    if manifest.size == 0 && !part_path.exists() {
        let _ = std::fs::File::create(&part_path);
    }
    let verified = {
        let p = part_path.clone();
        let sha = manifest.sha256.clone();
        tokio::task::spawn_blocking(move || verify_whole_file(&p, &sha))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(false)
    };
    if !verified {
        let _ = tokio::fs::remove_file(&part_path).await;
        task.set_error("整文件 sha256 校验不符（落地 .part 已删除，可重试续传）");
        task.set_phase(TaskPhase::Failed);
        return true;
    }
    let final_path = unique_dest(
        &task.dest_dir.join(sanitize_filename(
            &task
                .name_hint
                .clone()
                .unwrap_or_else(|| manifest.name.clone()),
        )),
    );
    if let Err(e) = tokio::fs::rename(&part_path, &final_path).await {
        task.set_error(format!("落名失败（{part_path:?} → {final_path:?}）: {e}"));
        task.set_phase(TaskPhase::Failed);
        return true;
    }
    *task.dest_path.lock().expect("dest poisoned") = Some(final_path.clone());
    let _ = tokio::fs::remove_file(progress_path(&task.dest_dir, manifest)).await;
    task.done_bytes.store(manifest.size, Ordering::Relaxed);
    // CDN 式再分发：下载完成自动登记为种子（本机即成新源）
    inner
        .registry
        .lock()
        .expect("registry poisoned")
        .register_completed(manifest.clone(), final_path);
    task.set_phase(TaskPhase::Completed);
    true
}

/// 目标名已被占用时退让（`name.2.ext`——同 sha256 的重复落地不覆盖旧文件）。
fn unique_dest(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    for n in 2..1000 {
        let cand = dir.join(format!("{stem}.{n}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    path.to_path_buf()
}

/// 进度持久化 + 阶段标记（暂停/取消/失败共用收尾）。
fn persist_and_mark(
    task: &Arc<TaskShared>,
    manifest: &TransferManifest,
    done: &HashSet<u64>,
    phase: TaskPhase,
) {
    save_progress(&task.dest_dir, manifest, done);
    task.set_phase(phase);
}

/// 引擎位图 → 观察面位图（task.done 是 view() 的真源，每批推进后同步）。
fn sync_done(task: &Arc<TaskShared>, done: &HashSet<u64>) {
    *task.done.lock().expect("done poisoned") = done.clone();
}

/// 定向问单 peer（query→offer 请求-应答；短窗避免串行拖满总窗）。
async fn query_peer(
    inner: Arc<Inner>,
    peer: &NodeId,
    key: &str,
    window: Duration,
) -> Option<(TransferManifest, NodeId)> {
    let (sha256, transfer_id) = split_key(key);
    let req_id = format!("q{}", inner.req_seq.fetch_add(1, Ordering::Relaxed));
    let (tx, rx) = oneshot::channel();
    inner
        .pending
        .lock()
        .expect("pending poisoned")
        .insert(req_id.clone(), tx);
    inner.handle.send(
        peer,
        serde_json::json!({
            "transfer": KIND_QUERY,
            "req_id": req_id,
            "sha256": sha256,
            "transfer_id": transfer_id,
        }),
    );
    let (from, offer) = tokio::time::timeout(window, rx).await.ok()?.ok()?;
    let manifest = serde_json::from_value::<TransferManifest>(
        offer
            .get("manifest")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .ok()?;
    // 对题校验：应答必须命中询问键
    if manifest.sha256 != key && manifest.transfer_id != key {
        return None;
    }
    Some((manifest, from))
}

/// 批内选源（slot 轮转——多源时不把一批请求都压给同一源）。
fn pick_source(sources: &[NodeId], slot: usize) -> NodeId {
    sources[slot % sources.len()].clone()
}

/// 单块一次拉取（chunk→chunk_data 请求-应答 + 长度/摘要双重校验）。
async fn pull_chunk(
    inner: Arc<Inner>,
    source: NodeId,
    manifest: &TransferManifest,
    index: u64,
) -> Result<Vec<u8>, String> {
    let req_id = format!("c{}", inner.req_seq.fetch_add(1, Ordering::Relaxed));
    let (tx, rx) = oneshot::channel();
    inner
        .pending
        .lock()
        .expect("pending poisoned")
        .insert(req_id.clone(), tx);
    inner.handle.send(
        &source,
        serde_json::json!({
            "transfer": KIND_CHUNK_REQ,
            "req_id": req_id,
            "transfer_id": manifest.transfer_id,
            "index": index,
        }),
    );
    let (_from, resp) = tokio::time::timeout(CHUNK_TIMEOUT, rx)
        .await
        .map_err(|_| format!("块 {index} 超时（{CHUNK_TIMEOUT:?}）"))?
        .map_err(|_| "应答通道关闭".to_string())?;
    let kind = resp.get("transfer").and_then(|v| v.as_str()).unwrap_or("");
    if kind == KIND_ERROR {
        let reason = resp
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("未知原因");
        return Err(format!("提供方拒绝: {reason}"));
    }
    if kind != KIND_CHUNK_DATA {
        return Err(format!("意外应答类型: {kind}"));
    }
    let got_index = resp
        .get("index")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    if got_index != index {
        return Err(format!("应答块下标错位（要 {index} 得 {got_index}）"));
    }
    let b64 = resp
        .get("bytes")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "应答缺 bytes".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let expect_len = chunk_len(index, manifest.size, manifest.chunk_size) as usize;
    if bytes.len() != expect_len {
        return Err(format!(
            "块 {index} 长度不符（期望 {expect_len} 得 {}）",
            bytes.len()
        ));
    }
    let digest = sha256_hex(&bytes);
    let expected = manifest
        .chunks
        .get(index as usize)
        .ok_or_else(|| format!("清单缺块 {index} 摘要"))?;
    if digest != *expected {
        return Err(format!("块 {index} sha256 不符（坏块）"));
    }
    Ok(bytes)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用临时目录（进程 + 时间戳唯一，隔离并行测试）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "p2p-transfer-{tag}-{}-{}",
            std::process::id(),
            crate::api::unix_now()
        ));
        std::fs::create_dir_all(&dir).expect("临时目录创建");
        dir
    }

    /// 写确定性伪随机文件（xorshift——双端字节级比对可复现；种子先乘散列常数
    /// 混合，避免小种子 |1 后状态碰撞 → 同内容文件）。
    fn write_random_file(path: &Path, size: usize, seed: u8) -> Vec<u8> {
        let mut state = u32::from(seed).wrapping_mul(0x9E37_79B1) | 1;
        let data: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();
        std::fs::write(path, &data).expect("写入测试文件");
        data
    }

    fn spawn_test_node() -> Handle {
        crate::P2pNode::spawn(crate::P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: crate::Timing::testing(),
            mdns_enabled: false,
            ..crate::P2pConfig::default()
        })
        .expect("随机端口绑定必成功")
    }

    /// 双节点 mesh：A（提供方）↔ B（消费方）直连（bootstrap 拨号路径）。
    async fn two_node_mesh() -> (Handle, Handle) {
        let a = spawn_test_node();
        let b = spawn_test_node();
        let b_id = b.dial(a.listen_addr()).await.expect("B 拨 A 必成功");
        assert_eq!(b_id, *a.self_id());
        (a, b)
    }

    /// 轮询任务至终态（默认 20s——真实组网 + 分块传输余量）。
    async fn wait_terminal(s: &TransferService, id: &str, timeout: Duration) -> TransferTaskView {
        let deadline = Instant::now() + timeout;
        loop {
            let v = s.task(id).expect("任务在册");
            if matches!(
                v.phase,
                TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled
            ) || Instant::now() > deadline
            {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ---- 1. 纯函数：分块几何 ----

    #[test]
    fn chunk_geometry_covers_whole_file() {
        // 2.5 MiB / 1 MiB → 3 块：0/1 满，2 余 0.5
        assert_eq!(chunk_count(2_621_440, CHUNK_SIZE), 3);
        assert_eq!(chunk_len(0, 2_621_440, CHUNK_SIZE), CHUNK_SIZE);
        assert_eq!(chunk_len(2, 2_621_440, CHUNK_SIZE), 524_288);
        assert_eq!(chunk_offset(2, CHUNK_SIZE), 2 * CHUNK_SIZE);
        // 边界：空文件 0 块；恰好整块无余；越界块长 0
        assert_eq!(chunk_count(0, CHUNK_SIZE), 0);
        assert_eq!(chunk_count(CHUNK_SIZE, CHUNK_SIZE), 1);
        assert_eq!(chunk_len(1, CHUNK_SIZE, CHUNK_SIZE), 0);
        // 块字节总和 = 文件大小
        let size = 3 * CHUNK_SIZE + 123;
        let total: u64 = (0..chunk_count(size, CHUNK_SIZE))
            .map(|i| chunk_len(i, size, CHUNK_SIZE))
            .sum();
        assert_eq!(total, size);
    }

    // ---- 2. 清单生成：分块/摘要/确定性 transfer_id ----

    #[test]
    fn manifest_chunking_and_hashes() {
        let dir = temp_dir("manifest");
        let path = dir.join("blob.bin");
        let data = write_random_file(&path, 64 * 1024 * 2 + 1000, 7);
        let m = build_manifest(&path, Some("自定义名.bin"), 64 * 1024).expect("建清单");
        assert_eq!(m.name, "自定义名.bin");
        assert_eq!(m.size, data.len() as u64);
        assert_eq!(m.chunk_size, 64 * 1024);
        assert_eq!(m.chunks.len(), 3, "(128KiB+1000)/64KiB 向上取整");
        assert_eq!(m.sha256, sha256_hex(&data), "整文件摘要");
        assert_eq!(m.chunks[0], sha256_hex(&data[..64 * 1024]), "首块摘要");
        assert_eq!(
            m.transfer_id,
            format!("tr_{}", &m.sha256[..16]),
            "transfer_id = tr_ + sha256 前 16 hex（确定性）"
        );
        assert!(m.mime.is_some(), "MIME 粗判填充");
        // 摘要可复核整文件
        assert!(verify_whole_file(&path, &m.sha256).unwrap());
        assert!(!verify_whole_file(&path, &"0".repeat(64)).unwrap());
    }

    #[test]
    fn manifest_rejects_directory_and_missing() {
        let dir = temp_dir("manifest-bad");
        assert!(
            build_manifest(&dir, None, CHUNK_SIZE).is_err(),
            "目录不可发布"
        );
        assert!(
            build_manifest(&dir.join("nonexistent"), None, CHUNK_SIZE).is_err(),
            "缺文件不可发布"
        );
    }

    #[test]
    fn sanitize_filename_blocks_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a\\b\\c.txt"), "c.txt");
        assert_eq!(sanitize_filename(""), "transfer.bin");
        assert_eq!(sanitize_filename("正常文件.iso"), "正常文件.iso");
    }

    // ---- 3. 注册表：发布幂等 / 持久化 / 下架 ----

    #[test]
    fn registry_publish_persist_reload_unpublish() {
        let dir = temp_dir("registry");
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        write_random_file(&f1, 4096, 1);
        write_random_file(&f2, 4096, 2);
        let reg_file = dir.join("reg.json");
        let mut reg = TransferRegistry::load(Some(reg_file.clone()));
        let m1 = reg.publish(&f1, None, CHUNK_SIZE).unwrap();
        // 幂等：重复发布同内容文件返回同一清单
        assert_eq!(
            reg.publish(&f1, Some("别名"), CHUNK_SIZE).unwrap().sha256,
            m1.sha256
        );
        let m2 = reg.publish(&f2, None, CHUNK_SIZE).unwrap();
        assert_eq!(reg.len(), 2);
        // 下载完成自动登记（同 sha256 去重；新 sha256 入表）
        let f3 = dir.join("c.bin");
        write_random_file(&f3, 4096, 3);
        let m3 = build_manifest(&f3, None, CHUNK_SIZE).unwrap();
        reg.register_completed(m3.clone(), f3.clone());
        reg.register_completed(m1.clone(), f1.clone()); // 已有 → 不重复
        assert_eq!(reg.len(), 3);
        // 持久化 → 重载
        let reloaded = TransferRegistry::load(Some(reg_file.clone()));
        assert_eq!(reloaded.len(), 3);
        assert!(reloaded.find(&m2.sha256).is_some());
        assert!(
            reloaded.find(&m3.transfer_id).is_some(),
            "transfer_id 亦可查"
        );
        // 下架（两种键）+ 持久化生效
        let mut reg2 = TransferRegistry::load(Some(reg_file.clone()));
        assert!(reg2.unpublish(&m1.sha256));
        assert!(reg2.unpublish(&m2.transfer_id));
        assert!(!reg2.unpublish(&m2.transfer_id), "重复下架 false");
        assert_eq!(TransferRegistry::load(Some(reg_file)).len(), 1);
    }

    // ---- 4. 进度文件：位图持久化往返 ----

    #[test]
    fn progress_roundtrip_preserves_bitmap() {
        let dir = temp_dir("progress");
        let f = dir.join("x.bin");
        write_random_file(&f, 100, 9);
        let m = build_manifest(&f, None, 32).unwrap(); // 4 块
        let mut done = HashSet::new();
        done.insert(0);
        done.insert(2);
        save_progress(&dir, &m, &done);
        let p = progress_path(&dir, &m);
        assert!(p.exists(), "进度文件落地: {p:?}");
        let state = load_progress(&p).expect("可读");
        assert_eq!(state.manifest, m, "清单随进度持久化");
        assert_eq!(state.done, vec![0, 2], "位图升序往返");
        // 损坏文件降级 None
        std::fs::write(&p, "{broken").unwrap();
        assert!(load_progress(&p).is_none());
    }

    // ---- 5. 双节点：query→offer 请求-应答经 overlay 送达 ----

    #[tokio::test]
    async fn offer_query_roundtrip_over_overlay() {
        let (a, b) = two_node_mesh().await;
        let dir = temp_dir("offer");
        let file = dir.join("share.bin");
        write_random_file(&file, 8192, 42);
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir.clone(),
                registry_file: None,
                chunk_size: 4096,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir.join("b"),
                registry_file: None,
                chunk_size: 4096,
            },
        );
        let m = sa.publish(&file, None).await.expect("A 发布");
        // B 询问（经 overlay：query→A→offer→B），应答方是 A 本尊
        let got = sb.query_manifest(&m.sha256).await.expect("B 应拿到 offer");
        assert_eq!(got.0.sha256, m.sha256);
        assert_eq!(got.0.chunks, m.chunks);
        assert_eq!(got.1, *a.self_id(), "应答方 = 提供方 NodeID");
        // transfer_id 亦可查
        let got2 = sb
            .query_manifest(&m.transfer_id)
            .await
            .expect("transfer_id 命中");
        assert_eq!(got2.0.sha256, m.sha256);
        // 未知文件 → 静默（无 offer）
        assert!(sb.query_manifest(&"1".repeat(64)).await.is_none());
        // A 侧统计：应答过 query
        assert!(sa.stats().queries_answered >= 2);
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 6. 双节点端到端：A 发布 B 拉取字节级一致 + 自动做种 ----

    #[tokio::test]
    async fn fetch_end_to_end_two_nodes_byte_identical() {
        let (a, b) = two_node_mesh().await;
        let dir_a = temp_dir("e2e-a");
        let dir_b = temp_dir("e2e-b");
        let file = dir_a.join("dataset.bin");
        let data = write_random_file(&file, 300 * 1024, 5); // 300 KiB / 64 KiB → 5 块
        let chunk = 64 * 1024u64;
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir_a.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir_b.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let m = sa
            .publish(&file, Some("跨节点数据集.bin"))
            .await
            .expect("发布");
        let task_id = sb.fetch(&m.sha256, None).await;
        let view = wait_terminal(&sb, &task_id, Duration::from_secs(20)).await;
        assert_eq!(view.phase, TaskPhase::Completed, "任务应完成: {view:?}");
        assert_eq!(view.progress, 100);
        assert_eq!(view.chunks_total, 5);
        assert_eq!(view.chunks_done, 5);
        assert_eq!(view.status, "completed");
        assert!(!view.sources.is_empty(), "应有源节点");
        // 字节级一致 + 落在 B 的落地目录
        let landed = PathBuf::from(view.dest_path.expect("完成应有落地路径"));
        assert_eq!(
            std::fs::read(&landed).expect("读落地文件"),
            data,
            "字节级一致"
        );
        assert_eq!(
            landed.parent(),
            Some(dir_b.as_path()),
            "落在 NEXOS 落地目录"
        );
        assert_eq!(landed.file_name().unwrap(), "跨节点数据集.bin");
        // CDN 式再分发：B 完成后自动登记为种子——B 也能应答 query
        assert!(
            sb.query_manifest(&m.sha256).await.is_some(),
            "B 已成新源（swarm）: {:?}",
            sb.manifests()
        );
        // A 侧做种统计：5 块全由 A 供出
        let stats = sa.stats();
        assert!(stats.chunks_served >= 5, "A 供出全部块: {stats:?}");
        // 进度文件清理
        assert!(!progress_path(&dir_b, &m).exists(), "完成后进度文件清除");
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 7. 坏块重试：清单摘要被篡改 → 重试耗尽任务失败 ----

    #[tokio::test]
    async fn fetch_bad_chunk_exhausts_retries_and_fails() {
        let (a, b) = two_node_mesh().await;
        let dir_a = temp_dir("bad-a");
        let dir_b = temp_dir("bad-b");
        let file = dir_a.join("tamper.bin");
        write_random_file(&file, 100 * 1024, 11);
        let chunk = 32 * 1024u64;
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir_a.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir_b.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let m = sa.publish(&file, None).await.expect("发布");
        // 篡改 A 侧清单块摘要（模拟提供方清单与内容不符——消费方逐块校验必败）
        {
            let mut reg = sa.inner.registry.lock().unwrap();
            let idx = reg
                .entries
                .iter()
                .position(|e| e.manifest.sha256 == m.sha256)
                .expect("发布在册");
            reg.entries[idx].manifest.chunks[1] = "f".repeat(64);
        }
        let task_id = sb.fetch(&m.sha256, None).await;
        let view = wait_terminal(&sb, &task_id, Duration::from_secs(30)).await;
        assert_eq!(
            view.phase,
            TaskPhase::Failed,
            "坏块重试耗尽应失败: {view:?}"
        );
        assert!(
            view.error.as_deref().unwrap_or_default().contains("重试"),
            "错误应说明重试耗尽: {view:?}"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 8. 断点续传：进度保留 → 二次 fetch 只补缺失块 ----

    #[tokio::test]
    async fn fetch_resume_from_saved_progress() {
        let (a, b) = two_node_mesh().await;
        let dir_a = temp_dir("resume-a");
        let dir_b = temp_dir("resume-b");
        let file = dir_a.join("big.bin");
        write_random_file(&file, 200 * 1024, 13); // 13 块 × 16 KiB
        let chunk = 16 * 1024u64;
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir_a.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir_b.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let m = sa.publish(&file, None).await.expect("发布");
        // 模拟上次中断：手工落「已完成 6/13 块」的进度文件 + 对应 .part 内容
        //（位图与 .part 必须一致——引擎对不匹配的位图会作废全量重拉）
        let done: HashSet<u64> = (0..6u64).collect();
        save_progress(&dir_b, &m, &done);
        let part = dir_b.join(format!("big.bin.{}.part", &m.sha256[..8]));
        let src_bytes = std::fs::read(&file).unwrap();
        std::fs::write(&part, &src_bytes[..6 * chunk as usize]).expect("预置 .part 前 6 块");
        let task_id = sb.fetch(&m.sha256, None).await;
        let view = wait_terminal(&sb, &task_id, Duration::from_secs(20)).await;
        assert_eq!(view.phase, TaskPhase::Completed, "续传应完成: {view:?}");
        // 断言「只补缺失」：A 供块数 = 13 - 6 = 7（已完成 6 块不重拉）
        let stats = sa.stats();
        assert_eq!(stats.chunks_served, 7, "只补拉缺失 7 块: {stats:?}");
        // 字节一致
        let landed = PathBuf::from(view.dest_path.expect("落地"));
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            std::fs::read(&file).unwrap()
        );
        // 位图与 .part 不匹配场景：删 .part 留进度 → 位图作废全量重拉
        let done2: HashSet<u64> = (0..6u64).collect();
        save_progress(&dir_b, &m, &done2);
        std::fs::remove_file(dir_b.join(format!("big.bin.{}.part", &m.sha256[..8]))).ok();
        // 换名落地（big.bin 已被占用 → unique_dest 退让 big.2.bin）
        let t2 = sb.fetch(&m.sha256, Some("big.bin")).await;
        let v2 = wait_terminal(&sb, &t2, Duration::from_secs(20)).await;
        assert_eq!(
            v2.phase,
            TaskPhase::Completed,
            "位图作废后全量重拉应完成: {v2:?}"
        );
        assert_eq!(sa.stats().chunks_served, 20, "13 块全量重拉（7 + 13）");
        let landed2 = PathBuf::from(v2.dest_path.expect("落地"));
        assert_eq!(
            std::fs::read(&landed2).unwrap(),
            std::fs::read(&file).unwrap()
        );
        assert_eq!(landed2.file_name().unwrap(), "big.2.bin", "同名退让不覆盖");
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 9. 并发背压：在途块峰值 ≤ MAX_INFLIGHT_CHUNKS ----

    #[tokio::test]
    async fn fetch_backpressure_caps_inflight() {
        let (a, b) = two_node_mesh().await;
        let dir_a = temp_dir("bp-a");
        let dir_b = temp_dir("bp-b");
        let file = dir_a.join("wide.bin");
        write_random_file(&file, 20 * 32 * 1024, 17); // 20 块 × 32 KiB
        let chunk = 32 * 1024u64;
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir_a.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir_b.clone(),
                registry_file: None,
                chunk_size: chunk,
            },
        );
        let m = sa.publish(&file, None).await.expect("发布");
        let task_id = sb.fetch(&m.sha256, None).await;
        let view = wait_terminal(&sb, &task_id, Duration::from_secs(20)).await;
        assert_eq!(view.phase, TaskPhase::Completed, "应完成: {view:?}");
        assert!(
            view.max_inflight_seen <= MAX_INFLIGHT_CHUNKS,
            "在途块峰值 {} 超上限 {MAX_INFLIGHT_CHUNKS}",
            view.max_inflight_seen
        );
        assert!(view.max_inflight_seen >= 1, "至少观测到批量并发");
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 10. 任务状态机：无源失败 / 取消 / 暂停继续 ----

    #[tokio::test]
    async fn task_state_machine_control_paths() {
        let (a, b) = two_node_mesh().await;
        let dir = temp_dir("state");
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir.clone(),
                registry_file: None,
                chunk_size: 16 * 1024,
            },
        );
        // —— 无源 sha256：query 窗口内失败 ——
        let t1 = sb.fetch(&"a".repeat(64), None).await;
        let v = wait_terminal(&sb, &t1, Duration::from_secs(15)).await;
        assert_eq!(v.phase, TaskPhase::Failed, "无源应失败: {v:?}");
        assert_eq!(v.status, "error", "前端状态词");

        // —— 未知 transfer_id：查询中取消 ——
        let t2 = sb.fetch("tr_deadbeef", None).await;
        assert!(sb.cancel_task(&t2), "取消进行中任务");
        let v = wait_terminal(&sb, &t2, Duration::from_secs(10)).await;
        assert_eq!(v.phase, TaskPhase::Cancelled);
        // 已终态任务不可再暂停/取消；未知任务操作 false
        assert!(!sb.pause_task(&t2).await);
        assert!(!sb.cancel_task(&t2));
        assert!(!sb.pause_task("task-99999").await);
        assert!(sb.task("task-99999").is_none());

        // —— 有源任务：暂停 → 继续 → 完成 ——
        let file = dir.join("ctl.bin");
        write_random_file(&file, 64 * 1024, 23);
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir.clone(),
                registry_file: None,
                chunk_size: 16 * 1024,
            },
        );
        let m = sa.publish(&file, None).await.expect("发布");
        let t3 = sb.fetch(&m.sha256, None).await;
        assert!(sb.pause_task(&t3).await, "暂停");
        // 等引擎观察到暂停位（先置 Paused 再 await 唤醒——无丢失唤醒窗口）
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if sb.task(&t3).unwrap().phase == TaskPhase::Paused || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            sb.task(&t3).unwrap().phase,
            TaskPhase::Paused,
            "应达 Paused"
        );
        assert!(!sb.pause_task(&t3).await, "重复暂停 false");
        assert!(sb.resume_task(&t3).await, "继续");
        let v = wait_terminal(&sb, &t3, Duration::from_secs(20)).await;
        assert_eq!(v.phase, TaskPhase::Completed, "继续后应完成: {v:?}");
        // 终态后 pause/resume 均 false
        assert!(!sb.pause_task(&t3).await);
        assert!(!sb.resume_task(&t3).await);
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 11. 落地目录 / 名称覆盖 / transfer_id 发起 ----

    #[tokio::test]
    async fn fetch_lands_in_configured_dir_with_name_override() {
        let (a, b) = two_node_mesh().await;
        let dir_a = temp_dir("land-a");
        let dir_b = temp_dir("land-b");
        let file = dir_a.join("orig.bin");
        write_random_file(&file, 32 * 1024, 29);
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir_a.clone(),
                registry_file: None,
                chunk_size: 8 * 1024,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir_b.clone(),
                registry_file: None,
                chunk_size: 8 * 1024,
            },
        );
        let m = sa.publish(&file, None).await.expect("发布");
        // transfer_id 发起 + 名称覆盖
        let task_id = sb.fetch(&m.transfer_id, Some("改名落地.bin")).await;
        let v = wait_terminal(&sb, &task_id, Duration::from_secs(20)).await;
        assert_eq!(
            v.phase,
            TaskPhase::Completed,
            "transfer_id 发起也应完成: {v:?}"
        );
        assert_eq!(v.transfer_id.as_deref(), Some(m.transfer_id.as_str()));
        let landed = PathBuf::from(v.dest_path.unwrap());
        assert_eq!(landed.file_name().unwrap(), "改名落地.bin", "名称覆盖生效");
        assert_eq!(landed.parent(), Some(dir_b.as_path()), "落地目录生效");
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            std::fs::read(&file).unwrap()
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 12. 发布路径校验（服务层）----

    #[tokio::test]
    async fn publish_validates_source_path() {
        let a = spawn_test_node();
        let dir = temp_dir("pub");
        let s = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir.clone(),
                registry_file: None,
                chunk_size: CHUNK_SIZE,
            },
        );
        assert!(
            s.publish(&dir.join("nope.bin"), None).await.is_err(),
            "不存在路径 Err"
        );
        assert!(s.publish(&dir, None).await.is_err(), "目录 Err");
        // 正常发布 + 下架 + manifests 视图
        let f = dir.join("ok.bin");
        write_random_file(&f, 2048, 31);
        let m = s.publish(&f, None).await.expect("发布成功");
        assert_eq!(s.manifests().len(), 1);
        assert_eq!(s.manifests()[0].manifest.transfer_id, m.transfer_id);
        assert!(s.unpublish(&m.transfer_id));
        assert!(s.manifests().is_empty());
        // 下架后 query 无应答（本节点孤网 → 无 offer）
        assert!(s.query_manifest(&m.sha256).await.is_none());
        a.shutdown().await;
    }

    // ---- 13. 静默让路：非 transfer 帧不干扰（与联邦桥共存前提）----

    #[tokio::test]
    async fn inbound_ignores_non_transfer_payloads() {
        let a = spawn_test_node();
        let s = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: temp_dir("ignore"),
                registry_file: None,
                chunk_size: CHUNK_SIZE,
            },
        );
        // 模拟联邦桥载荷与调试消息进入（不 panic、无 pending 泄漏）
        s.handle_inbound(
            a.self_id(),
            serde_json::json!({"fed": "im_lobby", "text": "hi"}),
        );
        s.handle_inbound(a.self_id(), serde_json::json!({"text": "ping"}));
        s.handle_inbound(a.self_id(), serde_json::json!({"transfer": "unknown_kind"}));
        assert!(s.tasks().is_empty());
        assert!(s.inner.pending.lock().unwrap().is_empty(), "无残留等待者");
        a.shutdown().await;
    }

    // ---- 14. 空文件端到端（0 块边界：直接整文件校验）----

    #[tokio::test]
    async fn fetch_empty_file_boundary() {
        let (a, b) = two_node_mesh().await;
        let dir_a = temp_dir("empty-a");
        let dir_b = temp_dir("empty-b");
        let file = dir_a.join("empty.bin");
        std::fs::write(&file, b"").unwrap();
        let sa = TransferService::spawn(
            a.clone(),
            TransferConfig {
                dest_dir: dir_a.clone(),
                registry_file: None,
                chunk_size: 4096,
            },
        );
        let sb = TransferService::spawn(
            b.clone(),
            TransferConfig {
                dest_dir: dir_b.clone(),
                registry_file: None,
                chunk_size: 4096,
            },
        );
        let m = sa.publish(&file, None).await.expect("发布");
        assert_eq!(m.chunks.len(), 0, "空文件 0 块");
        let task_id = sb.fetch(&m.sha256, None).await;
        let v = wait_terminal(&sb, &task_id, Duration::from_secs(20)).await;
        assert_eq!(v.phase, TaskPhase::Completed, "空文件应完成: {v:?}");
        assert_eq!(v.progress, 100);
        let landed = PathBuf::from(v.dest_path.unwrap());
        assert_eq!(std::fs::metadata(&landed).unwrap().len(), 0);
        a.shutdown().await;
        b.shutdown().await;
    }
}
