//! `StorageRouteHandler` —— 把 HTTP 请求路由到真实 os-storage ZFS 后端。
//!
//! 定位（规划文档 §3.6 / §9.1#10）：
//! - 实现 [`RouteHandler`]（`#[async_trait]`），声明 `/api/v1/pools`、
//!   `/api/v1/datasets`、`/api/v1/snapshots` 等路由。
//! - `handle` 把 [`ApiRequest`] 翻译成对 `os_storage::StorageBackend` 的调用，
//!   结果经 `serde_json::to_value` 装进 [`ApiResponse::body`]。
//!
//! # 为什么持有具体类型 `Arc<ZfsCliBackend>`
//!
//! `StorageBackend` trait 是**原生 async fn in trait**（`backend.rs` 顶部
//! `#![allow(async_fn_in_trait)]`，无 `#[async_trait]`）——它**非 dyn 兼容**，
//! 无法 `Box<dyn StorageBackend>`。而 [`RouteHandler`] 本身用 `#[async_trait]`
//! 注册为 `Box<dyn RouteHandler>`，故 `StorageRouteHandler` 内部必须持有**具体类型**
//! `Arc<ZfsCliBackend>`，在其方法里直接调原生 async trait 方法（编译期单态化）。
//!
//! # 路径参数
//!
//! 网关 dispatch 当前不向 handler 传递 `PathParams`，故 `handle` 从 `req.path`
//! 字符串解析（参考 `PlaceholderHandler` 的 `split('?')` 模式）。本 handler 的路由
//! 都是静态路径（无 `:id`），仅按 (method, path) 精确分发；如需 `:id` 风格，未来
//! 在 dispatch 注入 PathParams 后再扩展。
//!
//! # 错误转换
//!
//! [`StorageError`] 已有 `From<StorageError> for os_common::ApiError`，但 handler
//! 返回 [`ApiGatewayError`]。本模块用 `map_storage_err` 把 `StorageError` 归类到
//! `ApiGatewayError` 的相应变体（NotFound→`Internal` 携带 404 提示、Conflict→`Internal` 携带 409 提示，
//! 其余→`Internal`）。注：dispatch 把任意 `Err(ApiGatewayError)` 都渲染成 HTTP 500，
//! 故错误分类只影响响应体文本，不影响状态码——如需细粒度状态码，应在 handle 内
//! 显式返回 `Ok(ApiResponse{ status: <code>, .. })`（见 `error_response`）。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use os_core::{DatasetId, PoolId};
use os_storage::backend::StorageBackend;
use os_storage::model::{Pool, Snapshot, VdevSpec};
use os_storage::options::DatasetOptions;
use os_storage::{
    parse_zpool_status, CommandRunner, StorageError, TokioCommandRunner, ZfsCliBackend,
};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// ZFS 工具可用性探测（2026-09-02：无 ZFS 节点优雅降级）
// ----------------------------------------------------------------------------

/// 强制视 ZFS 为不可用的 env 开关名（值 `0` / `false` / `no`）。
///
/// 测试与特殊环境用——例如想在装有 zfsutils 的机器上验证降级路径，或反向强制
/// 探测结果。探测结果进程内只算一次（[`zfs_available`] 缓存），故该开关须在
/// 进程首次触达存储端点前设置。
const ENV_ZFS_PROBE: &str = "NEXOS_STORAGE_ZFS_PROBE";

/// 进程内缓存的探测结果（首次触达存储端点时计算一次）。
///
/// 并非所有节点都要存储池（用户原话）——install.sh 装的最小节点（如 Spark）
/// 没有 zfsutils，`zpool list` 经 sudo 报 `sudo: zpool: command not found`
/// 直接 500 红幅是错的。本探测用 PATH 查找 `zpool`/`zfs` 二进制（不 spawn
/// 进程、不走 sudo），缺失即判定「本节点无 ZFS 工具」，读端点降级 200 空态。
static ZFS_AVAILABLE: Lazy<bool> = Lazy::new(|| {
    let avail = zfs_probe_from(
        std::env::var_os("PATH"),
        std::env::var(ENV_ZFS_PROBE).ok(),
    );
    if avail {
        eprintln!("[storage] ZFS 工具可用（zpool/zfs 均在 PATH 中）");
    } else {
        eprintln!(
            "[storage] ZFS 工具不可用（PATH 未同时含 zpool 与 zfs，或 {ENV_ZFS_PROBE}=0）——\
             存储池/数据集/快照读端点降级为 200 空态，写端点返回 400"
        );
    }
    avail
});

/// 生产入口：读缓存的 ZFS 可用性。
#[must_use]
pub fn zfs_available() -> bool {
    *ZFS_AVAILABLE
}

/// 纯探测逻辑（可单测，不读真实 env）：
///
/// 1. `gate`（`NEXOS_STORAGE_ZFS_PROBE`）为 `0`/`false`/`no` → 强制不可用；
/// 2. 否则在 `path_var`（PATH 变量值）里查找 `zpool` **与** `zfs` 两个可执行
///    文件——两者齐备才算可用（zfs 数据集/快照命令与 zpool 池命令各走各的二进制）。
fn zfs_probe_from(path_var: Option<OsString>, gate: Option<String>) -> bool {
    if let Some(v) = gate {
        let v = v.trim().to_ascii_lowercase();
        if v == "0" || v == "false" || v == "no" || v.is_empty() {
            return false;
        }
    }
    let Some(path_var) = path_var else {
        return false;
    };
    find_executable_in_path(&path_var, "zpool") && find_executable_in_path(&path_var, "zfs")
}

/// 在 PATH 变量值中查找名为 `name` 的可执行文件（存在且有执行位）。
///
/// 与 shell 的 `which` 语义对齐：逐目录拼接候选路径，第一个命中即返回 true。
/// 目录不存在/候选不是普通文件/无执行位均跳过。纯函数，可单测。
fn find_executable_in_path(path_var: &OsString, name: &str) -> bool {
    std::env::split_paths(path_var).any(|dir: PathBuf| {
        let candidate = dir.join(name);
        // 元数据失败（不存在/无权限）按“没有”处理；只认普通文件 + 任一执行位
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// 判定一个 [`StorageError`] 是否属「ZFS 二进制缺失」（读端点降级的唯一错误类别）。
///
/// 覆盖两条真实路径：
/// - spawn 直接失败（sudo 本体缺失等）→ `StorageError::Io` 且 kind == NotFound；
/// - sudo 包装路径（os-api 常态：zpool 在 sudo 内部找不到）→ `CommandFailed`
///   且 stderr 文案含 `command not found`（`sudo: zpool: command not found`），
///   或退出码 127（shell 语义的 command-not-found）。
///
/// **错误分级红线**：只有二进制缺失走降级——权限（sudo 未免密）、池损坏、
/// 内部错误等真实故障照旧 500，不掩盖（诚实原则）。
fn is_zfs_binary_missing(e: &StorageError) -> bool {
    match e {
        StorageError::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        StorageError::CommandFailed(msg) => {
            let m = msg.to_lowercase();
            m.contains("command not found")
                || m.contains("退出码 127")
                || m.contains("exit code: 127")
                || m.contains("exit status 127")
        }
        _ => false,
    }
}

/// ZFS 不可用时读端点的降级响应：200 + 空列表 + `zfs_available:false` 标志。
///
/// `list_key` 为列表字段名（`pools` / `datasets` / `snapshots` / `importable`），
/// 与各端点可用路径的原字段对齐——前端按「响应是数组（可用）还是带
/// `zfs_available:false` 的对象（不可用）」区分两种空态。
fn zfs_degraded_response(list_key: &str) -> ApiResponse {
    ok_json(serde_json::json!({ list_key: [], "zfs_available": false }))
}

#[cfg(test)]
use {
    os_core::SnapshotId,
    // CommandOutput 仅测试 fixture 构造用；CommandRunner 已在上方无条件导入（结构体字段）
    os_storage::CommandOutput,
};

// ----------------------------------------------------------------------------
// Handler 主体
// ----------------------------------------------------------------------------

/// 存储路由处理器——HTTP 边界适配到 [`ZfsCliBackend`]（真实 ZFS CLI 后端）。
///
/// 持有具体类型 `Arc<ZfsCliBackend>`（非 `Box<dyn StorageBackend>`）——理由见模块文档。
/// 生产构造：`StorageRouteHandler::new(Arc::new(ZfsCliBackend::new()))`；
/// 测试构造：注入带 fixture runner 的 backend（见本文件 `#[cfg(test)]`）。
pub struct StorageRouteHandler {
    backend: Arc<ZfsCliBackend>,
    /// 直接命令通道——`zpool import` 探测/导入、`zpool status` 活跃池成员探测。
    ///
    /// 生产 = [`TokioCommandRunner`]（zpool/zfs 自动 sudo 包装）；测试注入 fixture
    /// （`with_cmd_runner`）。与 backend 内部的 runner 解耦：import 端点的
    /// `zpool import <name>` 走 self.runner，导入成功后的池信息复用 backend 的
    /// `list_pools`（zpool list）——同一 endpoint 两条命令通道各测各的。
    runner: Arc<dyn CommandRunner>,
    /// ZFS 工具可用性探测函数（进程内缓存，见 [`zfs_available`]）。
    ///
    /// 生产 = [`zfs_available`]（PATH 查找 zpool/zfs + env 强制开关）；
    /// 测试注入固定值（fixture runner 模拟的是“ZFS 存在”的世界，注入 `|| true`
    /// 保证既有测试不依赖宿主机是否装 zfsutils；降级测试注入 `|| false`）。
    zfs_probe: fn() -> bool,
}

impl StorageRouteHandler {
    /// 用一个已构造的 ZFS CLI 后端构造 handler。
    ///
    /// 通常 `backend` 由网关启动时 `Arc::new(ZfsCliBackend::new())` 创建并共享给
    /// 多个 handler（pool/dataset/snapshot 可能各自一个 handler，此处合并到单 handler）。
    #[must_use]
    pub fn new(backend: Arc<ZfsCliBackend>) -> Self {
        Self {
            backend,
            runner: Arc::new(TokioCommandRunner),
            zfs_probe: zfs_available,
        }
    }

    /// 测试构造：注入带 fixture runner 的 handler（探测/导入命令不走真实进程）。
    ///
    /// 探测函数固定 `|| true`——fixture 模拟的是 ZFS 正常工作的世界；降级路径
    /// 用 [`StorageRouteHandler::with_zfs_probe`] 注入 `|| false`。
    #[cfg(test)]
    fn with_cmd_runner(backend: Arc<ZfsCliBackend>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            backend,
            runner,
            zfs_probe: || true,
        }
    }

    /// 测试构造：额外注入 ZFS 可用性探测结果（降级路径注入 `|| false`）。
    #[cfg(test)]
    fn with_zfs_probe(
        backend: Arc<ZfsCliBackend>,
        runner: Arc<dyn CommandRunner>,
        zfs_probe: fn() -> bool,
    ) -> Self {
        Self {
            backend,
            runner,
            zfs_probe,
        }
    }

    /// 写操作前置守卫：ZFS 工具不可用时返回 400 响应（明确原因，防前端误触）。
    ///
    /// 可用返回 None 继续原流程。选 400 而非 500：这是「本节点不支持该操作」的
    /// 请求方问题，不是服务端故障；错误体一句话说明原因 + 启用方式。
    fn zfs_unavailable_guard(&self, action: &str) -> Option<ApiResponse> {
        if (self.zfs_probe)() {
            return None;
        }
        eprintln!("[storage] 拒绝{action}：本节点未安装 ZFS 工具");
        Some(error_response(
            400,
            &format!(
                "本节点未安装 ZFS 工具，无法{action}。\
                 如需启用存储池功能，请安装 zfsutils-linux（或 zfsutils）后重启 os-api"
            ),
        ))
    }

    /// `POST /api/v1/pools/:id/scrub` —— 启动 scrub。
    ///
    /// spawn_blocking 跑 `sudo zpool scrub <pool>`。成功返回 `{ok:true}`，
    /// 失败（无 sudo / 无 zpool / 无权限 / pool 不存在）降级为 `{ok:false, warning}`，不 panic。
    async fn handle_scrub_start(&self, pool: &str) -> Result<ApiResponse, ApiGatewayError> {
        // 无 ZFS 工具：沿用本端点的降级契约（200 + ok:false + warning），不 500
        if !(self.zfs_probe)() {
            return Ok(ok_json(serde_json::json!({
                "ok": false, "action": "scrub", "pool": pool,
                "warning": "本节点未安装 ZFS 工具，无法启动 scrub",
            })));
        }
        let pool_owned = pool.to_string();
        let pool_for_resp = pool_owned.clone();
        let result = tokio::task::spawn_blocking(move || {
            std::process::Command::new("sudo")
                .args(["zpool", "scrub", &pool_owned])
                .output()
        })
        .await;
        let body = match result {
            Ok(Ok(out)) if out.status.success() => serde_json::json!({
                "ok": true, "action": "scrub", "pool": pool_for_resp,
            }),
            Ok(Ok(out)) => serde_json::json!({
                "ok": false, "action": "scrub", "pool": pool_for_resp,
                "warning": format!(
                    "zpool scrub 失败（exit={}）: {}",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            }),
            Ok(Err(e)) => serde_json::json!({
                "ok": false, "action": "scrub", "pool": pool_for_resp,
                "warning": format!("执行 zpool scrub 失败: {e}"),
            }),
            Err(e) => serde_json::json!({
                "ok": false, "action": "scrub", "pool": pool_for_resp,
                "warning": format!("scrub 任务 join 失败: {e}"),
            }),
        };
        Ok(ok_json(body))
    }

    /// `GET /api/v1/pools/:id/scrub-status` —— 查询 scrub 进度。
    ///
    /// spawn_blocking 跑 `sudo zpool status <pool>`，用 [`parse_scrub_status`] 解析输出。
    /// 命令不可用 / 失败时降级为 `{status:"none", warning}`，不 panic。
    async fn handle_scrub_status(&self, pool: &str) -> Result<ApiResponse, ApiGatewayError> {
        // 无 ZFS 工具：降级为 status:none + warning（与命令失败的降级同型，不 500）
        if !(self.zfs_probe)() {
            return Ok(ok_json(serde_json::json!({
                "status": "none",
                "progress_pct": null,
                "start_time": null,
                "end_time": null,
                "errors": 0,
                "pool": pool,
                "warning": "本节点未安装 ZFS 工具，无法查询 scrub 状态",
            })));
        }
        let pool_owned = pool.to_string();
        let pool_for_resp = pool_owned.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let out = std::process::Command::new("sudo")
                .args(["zpool", "status", &pool_owned])
                .output()
                .map_err(|e| format!("执行 zpool status 失败: {e}"))?;
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if stderr.is_empty() {
                    Ok(String::from_utf8_lossy(&out.stdout).to_string())
                } else {
                    Err(format!(
                        "zpool status 失败（exit={}）: {stderr}",
                        out.status.code().unwrap_or(-1)
                    ))
                }
            }
        })
        .await;
        match result {
            Ok(Ok(text)) => {
                let scrub = parse_scrub_status(&text);
                let mut body = to_value(&scrub)?;
                if let serde_json::Value::Object(ref mut map) = body {
                    map.insert("pool".into(), serde_json::json!(pool_for_resp));
                }
                Ok(ok_json(body))
            }
            Ok(Err(msg)) => Ok(ok_json(serde_json::json!({
                "status": "none",
                "progress_pct": null,
                "start_time": null,
                "end_time": null,
                "errors": 0,
                "pool": pool_for_resp,
                "warning": msg,
            }))),
            Err(e) => Ok(ok_json(serde_json::json!({
                "status": "none",
                "progress_pct": null,
                "start_time": null,
                "end_time": null,
                "errors": 0,
                "pool": pool_for_resp,
                "warning": format!("scrub-status 任务 join 失败: {e}"),
            }))),
        }
    }

    /// `POST /api/v1/disks/:name/initialize` —— 初始化磁盘（admin，两步确认后调用）。
    ///
    /// 流程：校验磁盘名白名单（防路径穿越）→ `sudo wipefs /dev/<name>` 扫描签名
    /// （解析 wiped 列表）→ `sudo wipefs -a /dev/<name>` 清除全部分区表与签名。
    /// 成功返回 `{ok:true, disk, wiped:[...]}`；非法名 400；sudo/wipefs 无权限 401；
    /// 其余执行失败 500。不 panic（sudo/设备缺失均降级为错误响应）。
    async fn handle_initialize_disk(&self, disk: &str) -> Result<ApiResponse, ApiGatewayError> {
        // 白名单校验必须先于任何命令执行——"../etc" / "sda1" / "mmcblk0" 等一律 400。
        if !valid_disk_name(disk) {
            return Ok(error_response(
                400,
                &format!(
                    "非法磁盘名 {disk:?}：仅接受整盘名（sd[a-z]+ / nvme[0-9]+n[0-9]+），\
                     不接受分区名或路径"
                ),
            ));
        }
        let disk_resp = disk.to_string();

        // 1) 扫描签名（wipefs 不带 -a 只列出不清除；-a 静默故先扫后擦）
        let scan_dev = format!("/dev/{disk}");
        let scan = tokio::task::spawn_blocking(move || {
            std::process::Command::new("sudo")
                .args(["wipefs", &scan_dev])
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await;
        let wiped = match scan {
            Ok(Ok(out)) if out.status.success() => {
                parse_wipefs_signatures(&String::from_utf8_lossy(&out.stdout))
            }
            Ok(Ok(out)) => {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let status = if is_permission_denied(&msg) { 401 } else { 500 };
                return Ok(error_response(
                    status,
                    &format!(
                        "wipefs 扫描 /dev/{disk} 失败（exit={}）: {msg}",
                        out.status.code().unwrap_or(-1)
                    ),
                ));
            }
            Ok(Err(e)) => {
                return Ok(error_response(500, &format!("执行 wipefs 失败: {e}")));
            }
            Err(e) => {
                return Ok(error_response(500, &format!("wipefs 任务 join 失败: {e}")));
            }
        };

        // 2) 擦除全部分区表与签名（wipefs -a）
        let wipe_dev = format!("/dev/{disk}");
        let wipe = tokio::task::spawn_blocking(move || {
            std::process::Command::new("sudo")
                .args(["wipefs", "-a", &wipe_dev])
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await;
        match wipe {
            Ok(Ok(out)) if out.status.success() => Ok(ok_json(serde_json::json!({
                "ok": true,
                "disk": disk_resp,
                "wiped": wiped,
            }))),
            Ok(Ok(out)) => {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let status = if is_permission_denied(&msg) { 401 } else { 500 };
                Ok(error_response(
                    status,
                    &format!(
                        "wipefs -a /dev/{disk} 失败（exit={}）: {msg}",
                        out.status.code().unwrap_or(-1)
                    ),
                ))
            }
            Ok(Err(e)) => Ok(error_response(500, &format!("执行 wipefs -a 失败: {e}"))),
            Err(e) => Ok(error_response(
                500,
                &format!("wipefs -a 任务 join 失败: {e}"),
            )),
        }
    }

    /// `GET /api/v1/disks/:name/partitions` —— 磁盘分区详情（前端判断是否有其他系统分区）。
    ///
    /// 跑 `lsblk -J -o NAME,SIZE,FSTYPE,LABEL /dev/<name>`，经 [`parse_lsblk_partitions`]
    /// 归一化为 `{disk, has_partitions, signatures, partitions}`。lsblk 失败 / 设备不存在
    /// 时降级 200 + `warning`（不阻断向导），绝不 panic。
    async fn handle_disk_partitions(&self, disk: &str) -> Result<ApiResponse, ApiGatewayError> {
        if !valid_disk_name(disk) {
            return Ok(error_response(
                400,
                &format!("非法磁盘名 {disk:?}：仅接受整盘名（sd[a-z]+ / nvme[0-9]+n[0-9]+）"),
            ));
        }
        let dev = format!("/dev/{disk}");
        let disk_resp = disk.to_string();
        let result = tokio::task::spawn_blocking(move || {
            std::process::Command::new("lsblk")
                .args(["-J", "-o", "NAME,SIZE,FSTYPE,LABEL", &dev])
                .stdin(std::process::Stdio::null())
                .output()
        })
        .await;
        match result {
            Ok(Ok(out)) if out.status.success() => {
                match parse_lsblk_partitions(&String::from_utf8_lossy(&out.stdout), &disk_resp) {
                    Some(dp) => Ok(ok_json(to_value(&dp)?)),
                    None => Ok(error_response(
                        500,
                        "解析 lsblk JSON 输出失败（blockdevices 为空）",
                    )),
                }
            }
            // 降级：设备不存在 / lsblk 异常 → 200 + warning（向导不应因此卡死）
            Ok(Ok(out)) => {
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok(ok_json(serde_json::json!({
                    "disk": disk_resp,
                    "has_partitions": false,
                    "signatures": [],
                    "partitions": [],
                    "warning": format!("lsblk 失败（exit={}）: {msg}", out.status.code().unwrap_or(-1)),
                })))
            }
            Ok(Err(e)) => Ok(ok_json(serde_json::json!({
                "disk": disk_resp,
                "has_partitions": false,
                "signatures": [],
                "partitions": [],
                "warning": format!("执行 lsblk 失败: {e}"),
            }))),
            Err(e) => Ok(ok_json(serde_json::json!({
                "disk": disk_resp,
                "has_partitions": false,
                "signatures": [],
                "partitions": [],
                "warning": format!("分区查询任务 join 失败: {e}"),
            }))),
        }
    }

    /// `POST /api/v1/datasets/:id/quota` —— 设配额。
    ///
    /// body: `{quota_bytes, refreservation_bytes?}`。spawn_blocking 跑
    /// `sudo zfs set quota=<v> [refreservation=<v>] <dataset>`（参数由 [`build_quota_cmd`] 构造）。
    /// 失败降级为 `{ok:false, warning}`，不 panic。
    async fn handle_set_quota(
        &self,
        dataset: &str,
        body: serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        // 无 ZFS 工具：沿用本端点的降级契约（200 + ok:false + warning），不 500
        if !(self.zfs_probe)() {
            return Ok(ok_json(serde_json::json!({
                "ok": false, "action": "set-quota", "dataset": dataset,
                "warning": "本节点未安装 ZFS 工具，无法设置配额",
            })));
        }
        #[derive(serde::Deserialize)]
        struct QuotaReq {
            quota_bytes: u64,
            #[serde(default)]
            refreservation_bytes: Option<u64>,
        }
        let req: QuotaReq = serde_json::from_value(body)
            .map_err(|e| ApiGatewayError::Internal(format!("解析配额请求体失败: {e}")))?;
        let dataset_owned = dataset.to_string();
        let quota = req.quota_bytes;
        let refres = req.refreservation_bytes;
        let dataset_resp = dataset.to_string();
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
            let args = build_quota_cmd(&dataset_owned, quota, refres);
            let _out = std::process::Command::new("sudo")
                .arg("zfs")
                .args(&args)
                .output()?;
            Ok(args)
        })
        .await;
        match result {
            Ok(Ok(args)) => Ok(ok_json(serde_json::json!({
                "ok": true,
                "action": "set-quota",
                "dataset": dataset_resp,
                "quota_bytes": req.quota_bytes,
                "refreservation_bytes": req.refreservation_bytes,
                "cmd_args": args,
            }))),
            Ok(Err(e)) => Ok(ok_json(serde_json::json!({
                "ok": false,
                "action": "set-quota",
                "dataset": dataset_resp,
                "warning": format!("quota 任务 join 失败: {e}"),
            }))),
            Err(e) => Ok(ok_json(serde_json::json!({
                "ok": false,
                "action": "set-quota",
                "dataset": dataset_resp,
                "warning": format!("quota 任务 join 失败: {e}"),
            }))),
        }
    }

    /// `GET /api/v1/disks/importable` —— 探测可导入（已导出/未导入）的 ZFS 池。
    ///
    /// 执行 `zpool import`（**无参数 = 只列出**可导入池，绝不真导入；15s 超时），
    /// 经 [`parse_importable_pools`] 解析为 `[{name, id, state, raw}]`。任何失败
    /// （zpool 缺失 / sudo 未免密 / 超时 / 输出不可解析）一律降级为
    /// `{importable: []}`——探测是纯只读增强，不应让存储页报错。
    async fn handle_importable(&self) -> Result<ApiResponse, ApiGatewayError> {
        // 无 ZFS 工具：直接空态 + 标志（省一次注定失败的 zpool 子进程）
        if !(self.zfs_probe)() {
            return Ok(zfs_degraded_response("importable"));
        }
        let pools = probe_importable_pools(self.runner.as_ref()).await;
        Ok(ok_json(
            serde_json::json!({ "importable": pools, "zfs_available": true }),
        ))
    }

    /// `POST /api/v1/disks/import` —— 导入一个已导出的 ZFS 池（admin，用户显式点
    /// 「导入」后才调用；路由层已要求 admin）。
    ///
    /// body: `{name}`。校验池名白名单（防 flag 注入）→ 执行 `zpool import <name>`
    /// （15s 超时）→ 成功后复用既有 pools 逻辑（`backend.list_pools`）返回新池信息
    /// `{ok:true, pool}`；失败按 stderr 分类状态码：权限 401、池名冲突/已导入 409、
    /// 其余 400——错误体携带 zpool 原始 stderr 供前端展示。
    async fn handle_import_pool(&self, name: &str) -> Result<ApiResponse, ApiGatewayError> {
        // 池名白名单必须先于任何命令执行——"-" 开头会被 zpool 当 flag，其余拒绝
        if !valid_pool_name(name) {
            return Ok(error_response(400, &format!("非法池名 {name:?}")));
        }
        // 无 ZFS 工具：导入无从谈起——400 明确原因（防止前端误触后看到 500）
        if let Some(resp) = self.zfs_unavailable_guard("导入存储池") {
            return Ok(resp);
        }
        let args = vec!["import".to_string(), name.to_string()];
        let result = tokio::time::timeout(
            Duration::from_secs(ZPOOL_TIMEOUT_SECS),
            self.runner.run("zpool", &args),
        )
        .await;
        match result {
            Ok(Ok(out)) if out.exit_code == 0 => {
                // 导入成功 → 与 GET /api/v1/pools 同源，返回新池完整信息
                let pools: Vec<Pool> = self.backend.list_pools().await.map_err(map_storage_err)?;
                let imported = pools
                    .into_iter()
                    .find(|p| p.name == name || p.id.as_str() == name);
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "action": "import",
                    "pool": imported,
                })))
            }
            Ok(Ok(out)) => {
                let stderr = out.stderr.trim().to_string();
                let status = import_error_status(&stderr);
                Ok(error_response(
                    status,
                    &format!(
                        "zpool import {name} 失败（exit={}）: {stderr}",
                        out.exit_code
                    ),
                ))
            }
            Ok(Err(e)) => Ok(error_response(500, &format!("执行 zpool import 失败: {e}"))),
            Err(_) => Ok(error_response(
                504,
                &format!("zpool import {name} 超时（{ZPOOL_TIMEOUT_SECS}s）"),
            )),
        }
    }

    /// `DELETE /api/v1/pools/:name` —— 删除池 + 删除后盘处置（TrueNAS
    /// Export/Destroy 语义，2026-08-30）。
    ///
    /// **wipe 语义**（query `?wipe=true|1` 或 body `{wipe}`，默认 false）：
    /// - `wipe=false`（默认，「仅删除池」）→ 执行 `zpool export <name>`：池从
    ///   活跃列表消失、数据集卸载，但磁盘 ZFS 标签**保留**——`zpool import`
    ///   （无参列表）会列出该池，前端既有「可导入的存储池」横幅自动认它，
    ///   一键导入即恢复。选 export 而非 destroy 的依据：zpool-import(8) 明载
    ///   「Destroyed pools … are not listed unless the -D option is specified」
    ///   ——destroy 后无参 `zpool import` **不列出**（标签虽在但只 `zpool import -D`
    ///   可见），既有探测端点认不到；export 的池则正常列出。
    /// - `wipe=true`（「彻底擦除」）→ 执行 `zpool destroy <name>` 并对每个成员盘
    ///   `sudo wipefs -a /dev/<disk>`：清掉 zfs_member 残留签名，盘变完全空白，
    ///   直接出现在创建池向导的可选列表。**数据不可恢复**。
    ///
    /// **成员盘必须在 export/destroy 前抓取**（删池后 `zpool status` 不再有该池）：
    /// 先跑 `zpool status <name>` 经 [`pool_member_disks`] 解析。探测失败时：
    /// - 池不存在 → 404（不执行任何删除命令）；
    /// - wipe=true 且拿不到成员盘 → 中止（绝不盲擦，防止把残留标签盘漏擦或误擦）；
    /// - wipe=false → 降级 members=[] 继续（export 本身不依赖成员列表），响应带 warning。
    ///
    /// 高危操作：路由要求 admin，且前端要求输入池名确认（TrueNAS 式防误删）。
    /// 返回 `{ok, action, destroyed, wipe, members, wiped_disks[, wipe_errors][, warning]}`。
    async fn handle_delete_pool(
        &self,
        name: &str,
        raw_path: &str,
        body: serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        // 池名白名单必须先于任何命令执行——"-" 开头会被 zpool 当 flag，其余拒绝
        if !valid_pool_name(name) {
            return Ok(error_response(400, &format!("非法池名 {name:?}")));
        }
        // 无 ZFS 工具：删除/导出无从谈起——400 明确原因（防止前端误触后看到 500）
        if let Some(resp) = self.zfs_unavailable_guard("删除存储池") {
            return Ok(resp);
        }
        // wipe 参数：query 优先（DELETE 请求体常被中间层丢弃），其次 body，默认 false
        let wipe = query_param(raw_path, "wipe")
            .map(|v| v == "true" || v == "1")
            .or_else(|| body.get("wipe").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);

        // 1) 成员盘抓取（必须在删池前——destroy/export 后 status 不再有该池）
        let status_args = vec!["status".to_string(), name.to_string()];
        let status = tokio::time::timeout(
            Duration::from_secs(ZPOOL_TIMEOUT_SECS),
            self.runner.run("zpool", &status_args),
        )
        .await;
        let (members, warning) = match status {
            Ok(Ok(out)) if out.exit_code == 0 => (pool_member_disks(&out.stdout, name), None),
            Ok(Ok(out)) => {
                let stderr = out.stderr.trim().to_string();
                let lower = stderr.to_lowercase();
                if lower.contains("no such pool") || lower.contains("does not exist") {
                    return Ok(error_response(
                        404,
                        &format!(
                            "池 {name} 不存在（zpool status exit={}）: {stderr}",
                            out.exit_code
                        ),
                    ));
                }
                if wipe {
                    // 拿不到成员盘就无法定向 wipefs——中止，绝不盲擦
                    let code = if is_permission_denied(&stderr) {
                        401
                    } else {
                        500
                    };
                    return Ok(error_response(
                        code,
                        &format!(
                            "无法确定池 {name} 的成员盘，已中止删除（zpool status exit={}）: {stderr}",
                            out.exit_code
                        ),
                    ));
                }
                // export 不依赖成员列表——降级为空 + warning
                (
                    Vec::new(),
                    Some(format!(
                        "成员盘探测失败，members 为空（zpool status exit={}）: {stderr}",
                        out.exit_code
                    )),
                )
            }
            Ok(Err(e)) => {
                if wipe {
                    return Ok(error_response(500, &format!("执行 zpool status 失败: {e}")));
                }
                (Vec::new(), Some(format!("成员盘探测失败: {e}")))
            }
            Err(_) => {
                if wipe {
                    return Ok(error_response(
                        504,
                        &format!("zpool status {name} 超时（{ZPOOL_TIMEOUT_SECS}s），已中止删除"),
                    ));
                }
                (
                    Vec::new(),
                    Some(format!(
                        "成员盘探测超时（{ZPOOL_TIMEOUT_SECS}s），members 为空"
                    )),
                )
            }
        };

        // 2) 执行删除：wipe=false → export（保留标签可再导入）；wipe=true → destroy
        //    （不带头 '-f'：数据集 busy 时如实报错交前端展示，不强行卸载在用数据）
        let verb = if wipe { "destroy" } else { "export" };
        let del_args = vec![verb.to_string(), name.to_string()];
        let deleted = tokio::time::timeout(
            Duration::from_secs(ZPOOL_TIMEOUT_SECS),
            self.runner.run("zpool", &del_args),
        )
        .await;
        match deleted {
            Ok(Ok(out)) if out.exit_code == 0 => {}
            Ok(Ok(out)) => {
                let stderr = out.stderr.trim().to_string();
                let code = pool_delete_error_status(&stderr);
                return Ok(error_response(
                    code,
                    &format!(
                        "zpool {verb} {name} 失败（exit={}）: {stderr}",
                        out.exit_code
                    ),
                ));
            }
            Ok(Err(e)) => return Ok(error_response(500, &format!("执行 zpool {verb} 失败: {e}"))),
            Err(_) => {
                return Ok(error_response(
                    504,
                    &format!("zpool {verb} {name} 超时（{ZPOOL_TIMEOUT_SECS}s）"),
                ))
            }
        }

        // 3) wipe=true → 逐盘 wipefs -a（清 zfs_member 残留，盘变完全空白可建新池）。
        //    经 runner 以 `sudo wipefs -a /dev/<disk>` 执行（TokioCommandRunner 只对
        //    zpool/zfs 自动包装 sudo，故 program 显式给 sudo；与磁盘初始化端点同一
        //    sudoers 依赖）。单盘失败不中断其余盘（池已删，逐盘如实上报）。
        let mut wiped_disks: Vec<String> = Vec::new();
        let mut wipe_errors: Vec<serde_json::Value> = Vec::new();
        if wipe {
            for disk in &members {
                let dev = format!("/dev/{disk}");
                let wipe_args = vec!["wipefs".to_string(), "-a".to_string(), dev.clone()];
                let r = tokio::time::timeout(
                    Duration::from_secs(ZPOOL_TIMEOUT_SECS),
                    self.runner.run("sudo", &wipe_args),
                )
                .await;
                match r {
                    Ok(Ok(out)) if out.exit_code == 0 => wiped_disks.push(disk.clone()),
                    Ok(Ok(out)) => wipe_errors.push(serde_json::json!({
                        "disk": disk,
                        "error": format!(
                            "wipefs -a {dev} 失败（exit={}）: {}",
                            out.exit_code,
                            out.stderr.trim()
                        ),
                    })),
                    Ok(Err(e)) => wipe_errors.push(serde_json::json!({
                        "disk": disk,
                        "error": format!("执行 wipefs 失败: {e}"),
                    })),
                    Err(_) => wipe_errors.push(serde_json::json!({
                        "disk": disk,
                        "error": format!("wipefs -a {dev} 超时（{ZPOOL_TIMEOUT_SECS}s）"),
                    })),
                }
            }
        }

        // 4) 组装响应：warning 合成（探测降级 / 擦除失败均如实上报，不静默）
        let mut warning = warning;
        if !wipe_errors.is_empty() {
            let msg = format!(
                "有 {} 块成员盘擦除失败，池已删除但盘上签名可能残留（可在磁盘列表逐一初始化）",
                wipe_errors.len()
            );
            warning = Some(match warning {
                Some(w) => format!("{w}；{msg}"),
                None => msg,
            });
        }
        let mut resp = serde_json::json!({
            "ok": true,
            "action": verb,
            "destroyed": name,
            "wipe": wipe,
            "members": members,
            "wiped_disks": wiped_disks,
        });
        if !wipe_errors.is_empty() {
            resp["wipe_errors"] = serde_json::Value::Array(wipe_errors);
        }
        if let Some(w) = warning {
            resp["warning"] = serde_json::Value::String(w);
        }
        Ok(ok_json(resp))
    }
}

#[async_trait]
impl RouteHandler for StorageRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— Pool ——
            spec(HttpMethod::Get, "/api/v1/pools", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/pools",
                true,
                vec!["admin".into()],
            ),
            // —— 池删除 ——（export 保留标签可再导入 / destroy+wipefs 彻底擦除；
            // 高危：admin + 前端输入池名确认，TrueNAS Export/Destroy 式）
            spec(
                HttpMethod::Delete,
                "/api/v1/pools/:name",
                true,
                vec!["admin".into()],
            ),
            // —— Disk 探测 ——（创建池时前端列出可选磁盘用）
            spec(HttpMethod::Get, "/api/v1/disks", false, vec![]),
            // —— 可导入池探测 ——（只读 `zpool import` 列表；绝不真导入）
            spec(HttpMethod::Get, "/api/v1/disks/importable", false, vec![]),
            // —— 池导入 ——（用户显式点「导入此池」才执行 zpool import <name>；高危：admin）
            spec(
                HttpMethod::Post,
                "/api/v1/disks/import",
                true,
                vec!["admin".into()],
            ),
            // —— Disk 分区详情 ——（只读探测，公开；前端判断是否有其他系统分区）
            spec(
                HttpMethod::Get,
                "/api/v1/disks/:name/partitions",
                false,
                vec![],
            ),
            // —— Disk 初始化 ——（wipefs -a 清除分区表/签名，高危：两步确认后仅 admin）
            spec(
                HttpMethod::Post,
                "/api/v1/disks/:name/initialize",
                true,
                vec!["admin".into()],
            ),
            // —— Dataset ——
            spec(HttpMethod::Get, "/api/v1/datasets", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/datasets",
                true,
                vec!["admin".into()],
            ),
            // —— Snapshot ——
            spec(HttpMethod::Get, "/api/v1/snapshots", false, vec![]),
            // —— Scrub ——（admin 启动，状态查询无需 admin）
            spec(
                HttpMethod::Post,
                "/api/v1/pools/:id/scrub",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/pools/:id/scrub-status",
                false,
                vec![],
            ),
            // —— Quota ——（admin 设配额）
            spec(
                HttpMethod::Post,
                "/api/v1/datasets/:id/quota",
                true,
                vec!["admin".into()],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        // 去掉 query 串，只按路径分发（参考 PlaceholderHandler 的 split('?') 模式）
        let path = req.path.split('?').next().unwrap_or("");

        // —— 动态路由（:id 参数）—— scrub / scrub-status / quota ——
        // 用前缀 + 后缀剥离提取 :id，兼容 dataset 名含 '/'（如 tank/media）。
        // 这些路由的 :id 位于路径中段，无法用静态路径精确匹配，故优先剥离后 early return。
        if req.method == HttpMethod::Post {
            if let Some(pool) = path
                .strip_prefix("/api/v1/pools/")
                .and_then(|r| r.strip_suffix("/scrub"))
            {
                return self.handle_scrub_start(pool).await;
            }
        }
        if req.method == HttpMethod::Get {
            if let Some(pool) = path
                .strip_prefix("/api/v1/pools/")
                .and_then(|r| r.strip_suffix("/scrub-status"))
            {
                return self.handle_scrub_status(pool).await;
            }
        }
        if req.method == HttpMethod::Post {
            if let Some(dataset) = path
                .strip_prefix("/api/v1/datasets/")
                .and_then(|r| r.strip_suffix("/quota"))
            {
                return self.handle_set_quota(dataset, req.body).await;
            }
        }
        // —— 动态路由（:name 参数）—— disks 分区详情 / 初始化 ——
        // 磁盘名（sd[a-z]+ / nvme\d+n\d+）不含 '/'，前缀+后缀剥离无歧义；
        // 白名单校验在各 handler 内做（非法名 400，不执行任何命令）。
        if req.method == HttpMethod::Get {
            if let Some(disk) = path
                .strip_prefix("/api/v1/disks/")
                .and_then(|r| r.strip_suffix("/partitions"))
            {
                return self.handle_disk_partitions(disk).await;
            }
        }
        if req.method == HttpMethod::Post {
            if let Some(disk) = path
                .strip_prefix("/api/v1/disks/")
                .and_then(|r| r.strip_suffix("/initialize"))
            {
                return self.handle_initialize_disk(disk).await;
            }
        }
        // —— 动态路由（:name 参数）—— 池删除 ——
        // 池名不含 '/'（valid_pool_name 在 handler 内校验并 400），剥掉 query 后
        // 整段剩余路径即池名；?wipe=true|1 走 query_param 解析（DELETE 体常被丢弃）。
        if req.method == HttpMethod::Delete {
            if let Some(pool) = path.strip_prefix("/api/v1/pools/") {
                return self.handle_delete_pool(pool, &req.path, req.body).await;
            }
        }

        match (req.method, path) {
            // —— GET /api/v1/pools —— 列出所有池
            //
            // 无 ZFS 工具的节点（如 install.sh 最小节点）降级为
            // `{pools: [], zfs_available: false}`（200），不再 500 红幅——
            // 并不是所有终端都要存储池。错误分级：探测不可用 / 后端报「二进制
            // 缺失」（spawn ENOENT、sudo: command not found、退出码 127）走降级；
            // 其它失败（权限、池损坏等真实故障）照旧 500，不掩盖。
            (HttpMethod::Get, "/api/v1/pools") => {
                if !(self.zfs_probe)() {
                    return Ok(zfs_degraded_response("pools"));
                }
                let pools: Vec<Pool> = match self.backend.list_pools().await {
                    Ok(p) => p,
                    Err(e) if is_zfs_binary_missing(&e) => {
                        eprintln!("[storage] zpool 二进制缺失，池列表降级为空态: {e}");
                        return Ok(zfs_degraded_response("pools"));
                    }
                    Err(e) => return Err(map_storage_err(e)),
                };
                Ok(ApiResponse {
                    status: 200,
                    body: to_value(&pools)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/disks —— 探测本机可用磁盘（非系统盘；ZFS 池成员标注归属）
            //
            // 跑 `lsblk -npo NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE --json --bytes`，过滤：
            //   - loop 类型（snap 设备）
            //   - 整盘 type != disk（如 part/rom）
            //   - 该盘或其任一分区挂载了 `/`、`/boot`*、`[SWAP]`（系统盘——系统盘
            //     永不提示初始化：根本不进列表）
            // 已属 ZFS 池的盘**不排除**，改为标注归属（2026-08-30「已建池识别与导入」）：
            //   - member_of：活跃（已导入）池成员（zpool status config 命中）——永不提示
            //     初始化，UI 标「池内成员」；删除该池后才能重新初始化。
            //   - zfs_pool_hint：可导入（已导出/未导入）池成员（signatures 含 zfs_member
            //     且 zpool import 列表 config 命中）——永不提示初始化，UI 标「属于可导入池」
            //     并给「导入此池」入口。
            // 返回 `[{name, size_bytes, model, available, in_use, has_partitions,
            // signatures, member_of?, zfs_pool_hint?}]`。
            //
            // 用 spawn_blocking 跑 lsblk（外部进程阻塞调用，避免卡 tokio runtime）；
            // zpool import / zpool status 探测经 runner（sudo 包装）并发跑，15s 超时
            // 互不阻断、失败降级为空。
            (HttpMethod::Get, "/api/v1/disks") => {
                let (importable, active) = tokio::join!(
                    probe_importable_pools(self.runner.as_ref()),
                    probe_active_pool_members(self.runner.as_ref()),
                );
                let disks = tokio::task::spawn_blocking(detect_disks)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("磁盘探测任务 join 失败: {e}"))
                    })??;
                let importable_map = importable_pool_disk_map(&importable);
                let disks = enrich_disks_with_zfs(disks, &importable_map, &active);
                Ok(ApiResponse {
                    status: 200,
                    body: to_value(&disks)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/disks/importable —— 探测可导入（已导出/未导入）的 ZFS 池
            (HttpMethod::Get, "/api/v1/disks/importable") => self.handle_importable().await,

            // —— POST /api/v1/disks/import —— 导入已导出池（admin，用户显式确认后调用）
            (HttpMethod::Post, "/api/v1/disks/import") => {
                #[derive(serde::Deserialize)]
                struct ImportPoolReq {
                    name: String,
                }
                let body: ImportPoolReq = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析导入池请求体失败: {e}")))?;
                self.handle_import_pool(&body.name).await
            }

            // —— POST /api/v1/pools —— 创建池（body: { "name": "tank", "vdevs": [VdevSpec,...] }）
            (HttpMethod::Post, "/api/v1/pools") => {
                // 无 ZFS 工具：400 明确原因（本节点建不了池，前端应禁用入口）
                if let Some(resp) = self.zfs_unavailable_guard("创建存储池") {
                    return Ok(resp);
                }
                #[derive(serde::Deserialize)]
                struct CreatePoolReq {
                    name: String,
                    vdevs: Vec<VdevSpec>,
                }
                let body: CreatePoolReq = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析创建池请求体失败: {e}")))?;
                let pool: Pool = self
                    .backend
                    .create_pool(&PoolId::new(body.name), body.vdevs)
                    .await
                    .map_err(map_storage_err)?;
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&pool)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/datasets —— 列出数据集（可选 ?pool=tank 限定单池）
            //
            // 无 ZFS 工具：降级 `{datasets: [], zfs_available: false}`（同 GET /pools
            // 的错误分级——只有二进制缺失降级，其它失败照旧 500）。
            (HttpMethod::Get, "/api/v1/datasets") => {
                if !(self.zfs_probe)() {
                    return Ok(zfs_degraded_response("datasets"));
                }
                let pool = query_param(&req.path, "pool").map(PoolId::new);
                let datasets = match self.backend.list_datasets(pool.as_ref()).await {
                    Ok(d) => d,
                    Err(e) if is_zfs_binary_missing(&e) => {
                        eprintln!("[storage] zfs 二进制缺失，数据集列表降级为空态: {e}");
                        return Ok(zfs_degraded_response("datasets"));
                    }
                    Err(e) => return Err(map_storage_err(e)),
                };
                Ok(ApiResponse {
                    status: 200,
                    body: to_value(&datasets)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/datasets —— 创建数据集（body: { "name": "tank/media", "options": DatasetOptions }）
            (HttpMethod::Post, "/api/v1/datasets") => {
                // 无 ZFS 工具：400 明确原因（没有池就没有数据集）
                if let Some(resp) = self.zfs_unavailable_guard("创建数据集") {
                    return Ok(resp);
                }
                #[derive(serde::Deserialize)]
                struct CreateDatasetReq {
                    name: String,
                    #[serde(default)]
                    options: DatasetOptions,
                }
                let body: CreateDatasetReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建数据集请求体失败: {e}"))
                })?;
                let ds = self
                    .backend
                    .create_dataset(&DatasetId::new(body.name), body.options)
                    .await
                    .map_err(map_storage_err)?;
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&ds)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/snapshots —— 列出快照（可选 ?dataset=tank/media 限定单数据集）
            //
            // 无 ZFS 工具：降级 `{snapshots: [], zfs_available: false}`（同上错误分级）。
            (HttpMethod::Get, "/api/v1/snapshots") => {
                if !(self.zfs_probe)() {
                    return Ok(zfs_degraded_response("snapshots"));
                }
                let dataset = query_param(&req.path, "dataset").map(DatasetId::new);
                let snaps: Vec<Snapshot> = match self.backend.list_snapshots(dataset.as_ref()).await
                {
                    Ok(s) => s,
                    Err(e) if is_zfs_binary_missing(&e) => {
                        eprintln!("[storage] zfs 二进制缺失，快照列表降级为空态: {e}");
                        return Ok(zfs_degraded_response("snapshots"));
                    }
                    Err(e) => return Err(map_storage_err(e)),
                };
                Ok(ApiResponse {
                    status: 200,
                    body: to_value(&snaps)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— 未覆盖的路由 —— 兜底 404（不报错，让网关聚合层 conflict 检测更易定位）
            _ => Ok(error_response(404, "storage: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 磁盘探测（lsblk）
// ----------------------------------------------------------------------------

/// `GET /api/v1/disks` 返回的单个磁盘信息。
///
/// 字段命名与前端 `DiskInfo` 接口对齐（snake_case）。`available` 恒为 true，
/// 因为不满足「可用」条件的盘在探测阶段就被过滤掉，不会出现在数组里。
/// `has_partitions`（含文件系统签名）为 true 的盘**不能直接建池**——前端要求
/// 先走「初始化磁盘」（wipefs -a）确认流程（2026-08-23 定稿）。
#[derive(Debug, serde::Serialize)]
struct DiskInfo {
    name: String,
    size_bytes: u64,
    model: String,
    available: bool,
    /// 是否已被某个 ZFS 池占用（true=使用中，前端灰显不可选）。
    in_use: bool,
    /// 是否残留分区表或文件系统签名（如 BitLocker / GPT / MBR / ext4）。
    /// true = 需先初始化（POST /api/v1/disks/:name/initialize）才能加入新池。
    /// 活跃池成员 / 可导入池成员盘恒为 false（见 member_of / zfs_pool_hint）——
    /// 永不引导用户 wipefs 一块属于某个池的盘（2026-08-30 nvme 池事故）。
    has_partitions: bool,
    /// 检测到的签名类型列表（子分区与整盘 fstype 汇总，保序去重；空 = 干净盘）。
    signatures: Vec<String>,
    /// 活跃（已导入）池成员盘：所属池名（`zpool status` config 解析）。
    /// 有值的盘**永不**提示初始化——删除该池后才能重新初始化。
    #[serde(skip_serializing_if = "Option::is_none")]
    member_of: Option<String>,
    /// 可导入（已导出/未导入）池提示：signatures 含 `zfs_member` 且被
    /// `zpool import` 列表的 config 命中时为池名。有值的盘**永不**提示初始化——
    /// 数据没丢，导入即恢复（POST /api/v1/disks/import）。
    #[serde(skip_serializing_if = "Option::is_none")]
    zfs_pool_hint: Option<String>,
}

/// `lsblk --json` 输出的反序列化结构（仅取所需字段）。
///
/// `size` 是人类可读字符串（如 "931.5G"），需要换算成字节——此处用 `-b`
/// 字节模式时 size 仍是字符串但表整数；本实现**不依赖** size 字段，
/// 改用更稳妥的「不带 `-b`，size 当展示用」策略，并通过 lsblk 的 `--bytes`
/// 让 size 直接是字节数字符串。
#[derive(Debug, serde::Deserialize)]
struct LsblkRoot {
    #[serde(default)]
    blockdevices: Vec<LsblkDevice>,
}

#[derive(Debug, serde::Deserialize)]
struct LsblkDevice {
    name: String,
    /// lsblk 输出的 size：`--bytes` 时为整数，否则为字符串（如 "931.5G"）。
    /// 用 `serde_json::Value` 兼容两种类型，再由 [`parse_size_value`] 统一换算。
    #[serde(default)]
    size: serde_json::Value,
    #[serde(default)]
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// 文件系统/签名类型（如 ext4 / BitLocker / gpt 上的分区会有值；干净盘为 null）。
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    children: Vec<LsblkDevice>,
}

impl LsblkDevice {
    /// 判定本设备（含递归子分区）是否挂载了系统关键路径（/、/boot*、swap）。
    ///
    /// lsblk 中 swap 分区的 mountpoint 显示为 `[SWAP]`（取决于版本，也可能是 None
    /// 但 fstype=swap；此处保守按 mountpoint 字符串匹配）。
    fn is_system_mount(&self) -> bool {
        if let Some(mp) = self.mountpoint.as_deref() {
            if mp == "/" || mp.starts_with("/boot") || mp == "[SWAP]" {
                return true;
            }
        }
        self.children.iter().any(|c| c.is_system_mount())
    }
}

/// 解析 size（`serde_json::Value`，可能是整数或字符串）为字节数。
///
/// lsblk 在 `--bytes` 模式下输出整数（如 `1000204886016`）；不带 bytes 时
/// 输出字符串（如 `"931.5G"`），本函数也尝试用幂次表（K/M/G/T/P）估算。
/// 解析失败返回 0（不阻断列表，仅展示字段缺失）。
fn parse_size_value(v: &serde_json::Value) -> u64 {
    match v {
        // --bytes 模式：纯整数
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        // 字符串模式："931.5G" / "1T"
        serde_json::Value::String(s) => parse_size_str(s),
        _ => 0,
    }
}

/// 解析 size 字符串为字节数（人类可读单位估算）。
fn parse_size_str(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    // 纯数字 → 直接解析
    if let Ok(n) = s.parse::<u64>() {
        return n;
    }
    // 形如 "931.5G" / "1.82T" → 取尾部单位字符 + 幂次
    let (num_part, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len()),
    );
    let val: f64 = num_part.parse().unwrap_or(0.0);
    let factor: f64 = match unit.chars().next() {
        Some('K') | Some('k') => 1024.0,
        Some('M') | Some('m') => 1024f64.powi(2),
        Some('G') | Some('g') => 1024f64.powi(3),
        Some('T') | Some('t') => 1024f64.powi(4),
        Some('P') | Some('p') => 1024f64.powi(5),
        _ => 1.0,
    };
    (val * factor) as u64
}

/// 跑 `lsblk` 并返回过滤后的可用磁盘列表。
///
/// 过滤维度（三层，按顺序）：
/// 1. `type != "disk"`：排除 loop / part / rom 等；
/// 2. 系统盘：本盘或任一子分区挂载了 `/`、`/boot`*、`[SWAP]`；
/// 3. ZFS 池成员：跑 `zpool list -H -v` 收集所有已被池占用的整盘，排除之——
///    避免用户在「创建池」对话框里选到已属于另一池的盘（如 tank 的数据盘、
///    cache 池的 L2ARC/slog 设备）。
///
/// 注意：`zpool list -H -v` 不可用（如未装 zfsutils、无 pool）时**不阻断**，
/// 视为「无池成员」继续返回 lsblk 过滤结果——ZFS 未启用时这是合理降级。
///
/// 输出补充（2026-08-23 初始化流程）：每盘附带 `has_partitions`（有子分区或
/// 整盘 fstype 签名）与 `signatures`（fstype 汇总）——有残留的盘前端禁选，
/// 要求先走初始化（wipefs -a）确认。
///
/// 使用 `std::process::Command`（同步阻塞）——本函数由 `tokio::task::spawn_blocking`
/// 调度到阻塞线程池，故不必用 `tokio::process::Command`（少一个依赖）。
fn detect_disks() -> Result<Vec<DiskInfo>, ApiGatewayError> {
    let output = std::process::Command::new("lsblk")
        .args([
            "-n", // 不打印树形表头
            "-p", // NAME 用完整设备路径（/dev/sda 而非 sda）
            "-o",
            "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE",
            "--json",
            "--bytes", // SIZE 用字节整数（更精确，避免 931.5G 估算误差）
        ])
        .output()
        .map_err(|e| ApiGatewayError::Internal(format!("执行 lsblk 失败: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiGatewayError::Internal(format!(
            "lsblk 失败（exit={}）: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let root: LsblkRoot = serde_json::from_slice(&output.stdout)
        .map_err(|e| ApiGatewayError::Internal(format!("解析 lsblk JSON 输出失败: {e}")))?;

    // 收集所有已被 ZFS 池占用的整盘（归一化为 /dev/<name> 形式）。
    // zpool 不可用 → 返回空集，按「无池成员」降级，不报错。
    let zpool_members = collect_zpool_members();

    let disks = root
        .blockdevices
        .into_iter()
        // 只保留整盘（type=="disk"）；过滤 loop / part / rom 等
        .filter(|d| d.kind == "disk")
        // 过滤系统盘（本盘或任一分区挂了 /、/boot*、[SWAP]）
        .filter(|d| !d.is_system_mount())
        // 不排除池成员——改为 in_use 标记（用户能看到所有硬件，使用中的盘灰显）
        .map(|d| {
            let in_use = zpool_members.contains(&d.name);
            let signatures = collect_fstypes(&d);
            DiskInfo {
                in_use,
                name: d.name.clone(),
                size_bytes: parse_size_value(&d.size),
                model: d.model.clone().unwrap_or_default().trim().to_string(),
                available: true,
                // 有子分区表 或 整盘带文件系统签名 → 需先初始化
                //（活跃池/可导入池成员随后由 enrich_disks_with_zfs 摘出）
                has_partitions: !d.children.is_empty() || !signatures.is_empty(),
                signatures,
                member_of: None,
                zfs_pool_hint: None,
            }
        })
        .collect();
    Ok(disks)
}

/// 递归收集设备（含子分区）的 fstype 签名（保序去重，空值跳过）。
fn collect_fstypes(dev: &LsblkDevice) -> Vec<String> {
    let mut out = Vec::new();
    collect_fstypes_into(dev, &mut out);
    out
}

fn collect_fstypes_into(dev: &LsblkDevice, out: &mut Vec<String>) {
    if let Some(f) = dev.fstype.as_deref() {
        if !f.is_empty() && !out.iter().any(|s| s == f) {
            out.push(f.to_string());
        }
    }
    for c in &dev.children {
        collect_fstypes_into(c, out);
    }
}

/// 跑 `zpool list -H -v` 收集所有已被 ZFS 池占用的整盘路径（`/dev/<name>` 形式）。
///
/// 失败（zpool 未安装 / 无 pool / 命令异常）时返回空集——降级为「不过滤池成员」，
/// 不阻断 `detect_disks`（ZFS 未启用环境仍应能列盘）。
fn collect_zpool_members() -> std::collections::HashSet<String> {
    let output = std::process::Command::new("zpool")
        .args(["list", "-H", "-v"])
        .output();
    let out = match output {
        Ok(o) => o,
        Err(_) => return std::collections::HashSet::new(),
    };
    if !out.status.success() {
        return std::collections::HashSet::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_zpool_members(&stdout).collect()
}

/// 解析 `zpool list -H -v` 输出，逐行 yield 已被池占用的整盘路径（`/dev/<name>`）。
///
/// `zpool list -H -v` 输出形如（成员行相对池名行有缩进，但版本差异下不保证，
/// 故本解析**不依赖缩进**，而是按「首列是否是内核磁盘名」识别）：
///
/// ```text
/// tank   928G   612K   928G  -  -  0%  0%  1.00x  ONLINE  -
///   sdb   932G   612K   928G  -  -  0%  0%  -  ONLINE  -
/// cache   -   -   -  -  -  -  -  -  -
///   sda   224G   0   224G  -  -  0%  0%  -  ONLINE  -
/// ```
///
/// 池名行（`tank`/`cache`）不是磁盘；成员行首列是内核设备名（`sda`、`nvme0n1`、
/// `vdb` 等）。本函数用 [`is_kernel_disk_name`] 区分，输出 `/dev/<name>`。
/// 同时兼容成员行可能已带 `/dev/` 前缀或带分区号（如 `sda1` 用于特殊 vdev），
/// 后者会被归一化到整盘 `/dev/sda`（与 lsblk 的 NAME 一致）。
fn parse_zpool_members(stdout: &str) -> impl Iterator<Item = String> + '_ {
    stdout.lines().filter_map(|line| {
        // 缩进或首列切分；取首段非空 token 作为候选设备名
        let first = line.split_whitespace().next()?;
        let name = first.strip_prefix("/dev/").unwrap_or(first);
        if !is_kernel_disk_name(name) {
            return None;
        }
        // 归一化到整盘：去掉尾部分区号（sda1 → sda，nvme0n1p2 → nvme0n1），
        // 避免把 cache/slog 单独占用某分区时整盘被误判为可用。
        let whole = strip_partition_suffix(name);
        Some(format!("/dev/{whole}"))
    })
}

/// 判定一个 token 是否像「内核磁盘整盘/分区名」（用于过滤掉池名行）。
///
/// 覆盖常见命名：`sd[a-z]+`、`nvme\d+n\d+`（可带 `p\d+` 分区）、`vd[a-z]+`、
/// `hd[a-z]+`、`xvd[a-z]+`、`mmcblk\d+`（可带 `p\d+`）。保守：只匹配明显是块设备的
/// 前缀，避免误吞池名（池名通常含字母但不以这些前缀开头）。
fn is_kernel_disk_name(s: &str) -> bool {
    // 去掉可能的分区后缀后再判前缀，使 sda1 / nvme0n1p2 也命中
    let whole = strip_partition_suffix(s);
    let starts = ["sd", "nvme", "vd", "hd", "xvd", "mmcblk"];
    starts.into_iter().any(|pfx| whole.starts_with(pfx))
}

/// 去掉内核磁盘名的分区后缀，归一化到整盘名。
///
/// 规则（与内核命名一致）：
/// - `nvme\d+n\d+` 后的 `p\d+` 分区：`nvme0n1p2` → `nvme0n1`；
/// - `mmcblk\d+` 后的 `p\d+` 分区：`mmcblk0p1` → `mmcblk0`；
/// - `sd[a-z]+` / `vd[a-z]+` 等后跟纯数字分区：`sda1` → `sda`；
/// - 无分区后缀时原样返回。
fn strip_partition_suffix(s: &str) -> &str {
    // nvme/mmcblk 风格：p<digits>
    if let Some(idx) = s.rfind('p') {
        let after = &s[idx + 1..];
        let before = &s[..idx];
        if after.chars().all(|c| c.is_ascii_digit())
            && !after.is_empty()
            && (before.starts_with("nvme") || before.starts_with("mmcblk"))
        {
            return before;
        }
    }
    // sd/vd/hd/xvd 风格：纯数字分区后缀
    let split_at = s
        .find(|c: char| c.is_ascii_digit())
        .filter(|&i| i > 0 && s.starts_with(['s', 'v', 'h', 'x']));
    if let Some(i) = split_at {
        let (name, digits) = s.split_at(i);
        // 仅当 name 是字母前缀（sd/vd/hd/xvd…）且 digits 全数字时才剥离
        if name.chars().all(|c| c.is_ascii_alphabetic()) && !digits.is_empty() {
            return name;
        }
    }
    s
}

// ----------------------------------------------------------------------------
// 已建池识别与导入（2026-08-30）—— zpool import 探测 / zpool status 活跃成员 / disks 富化
// ----------------------------------------------------------------------------

/// zpool import 探测/导入与 zpool status 探测的超时（秒）。
const ZPOOL_TIMEOUT_SECS: u64 = 15;

/// `GET /api/v1/disks/importable` 的单个可导入池条目。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct ImportablePool {
    /// 池名（如 `nvme`）。
    pub name: String,
    /// 池 GUID（`id:` 行原文；`zpool import <id>` 亦可用数字 id 导入）。
    pub id: String,
    /// 池状态（如 ONLINE / DEGRADED）。
    pub state: String,
    /// 该池分段原文（含 config 盘列表；排障/展示用）。
    pub raw: String,
}

/// 跑 `zpool import`（无参数 = **只列出**可导入池，绝不真导入）并解析。
///
/// 经 `runner` 执行（生产 = TokioCommandRunner，zpool 自动 sudo 包装），15s 超时。
/// 任何失败（命令缺失 / sudo 未免密 / 非零退出 / 超时）降级为空 Vec——探测是
/// 纯只读增强，不应让存储页报错。
async fn probe_importable_pools(runner: &dyn CommandRunner) -> Vec<ImportablePool> {
    let args = ["import".to_string()];
    let fut = runner.run("zpool", &args);
    match tokio::time::timeout(Duration::from_secs(ZPOOL_TIMEOUT_SECS), fut).await {
        Ok(Ok(out)) if out.exit_code == 0 => parse_importable_pools(&out.stdout),
        _ => Vec::new(),
    }
}

/// 跑 `zpool status` 解析「成员盘 → 活跃池名」映射；任何失败降级为空 map。
async fn probe_active_pool_members(runner: &dyn CommandRunner) -> HashMap<String, String> {
    let args = ["status".to_string()];
    let fut = runner.run("zpool", &args);
    match tokio::time::timeout(Duration::from_secs(ZPOOL_TIMEOUT_SECS), fut).await {
        Ok(Ok(out)) if out.exit_code == 0 => active_pool_disk_map(&out.stdout),
        _ => HashMap::new(),
    }
}

/// 解析 `zpool import`（无参列表模式）输出为可导入池列表。
///
/// 输出按池分段，每段形如（真实样本；config 内是 tab 缩进的设备行）：
/// ```text
///    pool: nvme
///      id: 12345678901234567890
///   state: ONLINE
///  action: The pool can be imported using its name or numeric identifier.
///  config:
///
///     nvme        ONLINE
///       nvme1n1   ONLINE
/// ```
/// 以 `pool:` 行（trim 后前缀匹配）开新段；段内提取 `id:`/`state:`，整段原文
/// 存入 `raw`。缺 name/id 的段丢弃（无法稳定标识与导入）。纯函数，可单测。
pub fn parse_importable_pools(output: &str) -> Vec<ImportablePool> {
    let mut pools: Vec<ImportablePool> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("pool:") {
            pools.push(ImportablePool {
                name: name.trim().to_string(),
                id: String::new(),
                state: String::new(),
                raw: format!("{line}\n"),
            });
            continue;
        }
        let Some(cur) = pools.last_mut() else {
            continue;
        };
        cur.raw.push_str(line);
        cur.raw.push('\n');
        if let Some(v) = trimmed.strip_prefix("id:") {
            cur.id = v.trim().to_string();
        } else if let Some(v) = trimmed.strip_prefix("state:") {
            cur.state = v.trim().to_string();
        }
    }
    pools.retain(|p| !p.name.is_empty() && !p.id.is_empty());
    pools
}

/// 从 [`ImportablePool`] 列表（raw 的 config 段）提取「整盘裸名 → 池名」映射。
///
/// config 段（`config:` 行之后）每行首 token 是设备名（裸名 `nvme1n1`、带路径
/// `/dev/sda` 或 by-id 路径）：取末段路径分量，经 [`is_kernel_disk_name`] 过滤 +
/// [`strip_partition_suffix`] 归一化到整盘。纯函数，可单测。
pub fn importable_pool_disk_map(pools: &[ImportablePool]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for p in pools {
        let mut in_config = false;
        for line in p.raw.lines() {
            let t = line.trim();
            if t.starts_with("config:") {
                in_config = true;
                continue;
            }
            if !in_config || t.is_empty() {
                continue;
            }
            if let Some(tok) = t.split_whitespace().next() {
                let last = tok.rsplit('/').next().unwrap_or(tok);
                // 池根行（首 token == 池名，如名为 nvme 的池在 config 里的根行
                // `nvme  ONLINE`）不是成员盘，跳过——避免池名恰似内核盘名时误映射
                if last == p.name {
                    continue;
                }
                if is_kernel_disk_name(last) {
                    map.insert(strip_partition_suffix(last).to_string(), p.name.clone());
                }
            }
        }
    }
    map
}

/// 解析 `zpool status` 输出为「成员盘 → 活跃池名」映射（disks 的 member_of）。
///
/// 复用 os-storage 的 [`parse_zpool_status`]（段切分 + config 数据行解析 +
/// 嵌套 mirror/raidz 折叠到叶子盘），对每个池的 vdev 成员盘归一化到整盘裸名。
/// by-id/by-path 路径取末段（命中内核盘名才收录）。纯函数，可单测。
pub fn active_pool_disk_map(zpool_status_output: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for ps in parse_zpool_status(zpool_status_output) {
        for vdev in &ps.vdevs {
            for disk in &vdev.disks {
                let last = disk.rsplit('/').next().unwrap_or(disk.as_str());
                if is_kernel_disk_name(last) {
                    map.insert(strip_partition_suffix(last).to_string(), ps.name.clone());
                }
            }
        }
    }
    map
}

/// 把 ZFS 池归属信息富化进 [`detect_disks`] 的结果（纯函数，可单测）。
///
/// 三条硬规则（2026-08-30 定稿——「已建池识别与导入」）：
/// 1. 活跃池成员（`zpool status` 命中）→ `member_of = <池名>`，**永不**提示初始化
///    （`has_partitions` 置 false），`in_use` 置 true；
/// 2. 可导入池成员（signatures 含 `zfs_member` 且 `zpool import` 列表 config 命中）
///    → `zfs_pool_hint = <池名>`，同样**永不**提示初始化——数据没丢，导入即恢复；
/// 3. 其余带签名盘（无主残留）维持原 `has_partitions` 语义（需初始化）。
///
/// 系统盘在 [`detect_disks`] 阶段已整盘过滤（挂 `/`、`/boot`*、`[SWAP]` 的盘不进
/// 列表）——系统盘白名单由该过滤保证，本函数无需再处理。
fn enrich_disks_with_zfs(
    mut disks: Vec<DiskInfo>,
    importable: &HashMap<String, String>,
    active: &HashMap<String, String>,
) -> Vec<DiskInfo> {
    for d in disks.iter_mut() {
        let bare = d.name.strip_prefix("/dev/").unwrap_or(&d.name);
        if let Some(pool) = active.get(bare) {
            d.member_of = Some(pool.clone());
        }
        if d.signatures.iter().any(|s| s == "zfs_member") {
            if let Some(pool) = importable.get(bare) {
                d.zfs_pool_hint = Some(pool.clone());
            }
        }
        // 活跃池成员 / 可导入池成员：永不提示「需初始化」——防止引导 wipefs 毁池
        if d.member_of.is_some() || d.zfs_pool_hint.is_some() {
            d.has_partitions = false;
        }
        if d.member_of.is_some() {
            d.in_use = true;
        }
    }
    disks
}

/// ZFS 池名白名单（`zpool import <name>` 的 name 参数）。
///
/// 允许 ZFS 池名字符集（字母/数字/`_- .: ` 与空格）；拒绝空名、`.`/`..`、
/// 超长（>128，ZFS 限制内）与 `-` 开头（防被 zpool 解析为 flag）。
fn valid_pool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 || name.starts_with('-') {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | ' '))
}

/// 按 `zpool import` 失败 stderr 分类 HTTP 状态码。
///
/// 权限（sudo 未免密等）→ 401；池名冲突/已导入 → 409；其余（设备缺失、
/// 池损坏等）→ 400。原始 stderr 由调用方附带进错误体。
fn import_error_status(stderr: &str) -> u16 {
    if is_permission_denied(stderr) {
        return 401;
    }
    let m = stderr.to_lowercase();
    if m.contains("already exists") || m.contains("already imported") {
        return 409;
    }
    400
}

/// 解析 `zpool status <pool>` 输出提取成员盘裸名列表（保序去重，归一化整盘）。
///
/// 删池端点在 export/destroy **前**抓取成员盘（删池后 status 不再有该池），
/// 用途：a) 响应 `members` 字段（前端确认对话框展示盘去向）；b) `wipe=true`
/// 时逐盘 wipefs 的目标列表。
///
/// **不依赖** [`parse_zpool_status`] 的 vdev 分组——该分组在「多顶层 vdev」
/// （如 data mirror + cache 单盘）时成员归属有缺陷，会**漏掉部分顶层盘**
/// （对 wipe 目标列表是危险的）。本函数改为直接扫 config 段数据行：任何首
/// token 是内核盘名的行即成员（mirror-0/raidz1-0/cache/logs/spares 等合成名
/// 天然被 [`is_kernel_disk_name`] 过滤）；池根行（首 token == 池名，如用户真
/// 实存在的名为 `nvme` 的池根行 `nvme ONLINE 0 0 0`）显式跳过，避免把池名
/// 误当盘名产生 `/dev/<池名>` 假目标。by-id/by-path 路径取末段，
/// [`strip_partition_suffix`] 归一化整盘（与 [`active_pool_disk_map`] 同规则：
/// by-id 合成名（如 `ata-WDC_sdd1`）不是内核盘名，**不收录**——宁缺勿错，
/// 与既有探测端点同一约定）。纯函数，可单测。
pub fn pool_member_disks(zpool_status_output: &str, pool_name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_config = false;
    for line in zpool_status_output.lines() {
        let t = line.trim();
        if t.starts_with("config:") {
            in_config = true;
            continue;
        }
        if !in_config {
            continue;
        }
        // `errors:` 行标志 config 段结束（本函数只解析 `zpool status <pool>` 单池输出）
        if t.is_empty() || t.starts_with("errors:") {
            continue;
        }
        // 表头行（NAME STATE READ WRITE CKSUM）
        if t.starts_with("NAME") {
            continue;
        }
        let Some(tok) = t.split_whitespace().next() else {
            continue;
        };
        if tok == pool_name {
            continue; // 池根行（如名为 nvme 的池根行 "nvme ONLINE 0 0 0"）
        }
        let last = tok.rsplit('/').next().unwrap_or(tok);
        if is_kernel_disk_name(last) {
            let whole = strip_partition_suffix(last).to_string();
            if !out.contains(&whole) {
                out.push(whole);
            }
        }
    }
    out
}

/// 按 `zpool export/destroy` 失败 stderr 分类 HTTP 状态码。
///
/// 权限（sudo 未免密等）→ 401；池不存在 → 404；池/数据集 busy（有数据集在用、
/// 挂载中、共享 spare 等 export/destroy 的典型拒绝原因）→ 409；其余 → 400。
/// 原始 stderr 由调用方附带进错误体（前端直接展示 zpool 原始报错）。
fn pool_delete_error_status(stderr: &str) -> u16 {
    if is_permission_denied(stderr) {
        return 401;
    }
    let m = stderr.to_lowercase();
    if m.contains("no such pool") || m.contains("does not exist") {
        return 404;
    }
    if m.contains("busy") || m.contains("in use") || m.contains("currently mounted") {
        return 409;
    }
    400
}

// ----------------------------------------------------------------------------
// 磁盘初始化（wipefs）—— 纯函数，可单测
// ----------------------------------------------------------------------------

/// 磁盘名白名单校验：`sd[a-z]+`（sda、sdb、sdaa…）或 `nvme[0-9]+n[0-9]+`（nvme0n1…）。
///
/// 只接受**整盘裸名**（不带 `/dev/` 前缀、不带分区号）——
/// `POST /api/v1/disks/:name/initialize` 的 `:name` 是单段路径参数，磁盘名不含
/// `/`，天然无法穿越；本函数是第二道防线，拒绝 `../etc`、`sda1`（分区）、
/// `mmcblk0`（不在白名单）等一切非整盘名，杜绝 wipefs 擦错设备。
fn valid_disk_name(name: &str) -> bool {
    // sd 风格：sd + 至少 1 个小写字母
    if let Some(rest) = name.strip_prefix("sd") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase());
    }
    // nvme 风格：nvme + 数字段 + 'n' + 数字段（nvme0n1、nvme10n2）
    if let Some(rest) = name.strip_prefix("nvme") {
        let bytes = rest.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || i >= bytes.len() || bytes[i] != b'n' {
            return false;
        }
        i += 1; // 消耗 'n'
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        return i > start && i == bytes.len();
    }
    false
}

/// 解析 `wipefs /dev/<name>`（无 -a 的扫描）输出，提取唯一签名类型列表（保序去重）。
///
/// 输出形如（TYPE 列 + 前两列 DEVICE/OFFSET）：
/// ```text
/// DEVICE       OFFSET TYPE UUID LABEL
/// nvme1n1p3    0x1c0 BitLocker
/// nvme1n1      0x200 gpt
/// nvme1n1p3    0x1000 ntfs
/// ```
/// → `["BitLocker", "gpt", "ntfs"]`。空输出 / 仅表头 → `[]`。
fn parse_wipefs_signatures(stdout: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("DEVICE") {
            continue; // 表头 / 空行
        }
        let cols: Vec<&str> = t.split_whitespace().collect();
        if let Some(ty) = cols.get(2) {
            if !out.iter().any(|s| s == ty) {
                out.push((*ty).to_string());
            }
        }
    }
    out
}

/// 判定 wipefs/sudo 失败信息是否属「无权限」（映射 HTTP 401）。
///
/// 覆盖免密 sudo 未配置时的典型 stderr：
/// - `sudo: a password is required`
/// - `sudo: interactive authentication is required`（stdin 关闭、非 tty）
/// - `sudo: oem is not in the sudoers file.`
/// - `wipefs: error: /dev/sdb: probing initialization failed: Permission denied`
fn is_permission_denied(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("sudoers")
        || m.contains("password")
        || m.contains("permission denied")
        || m.contains("authentication is required")
        || m.contains("not authorized")
        || m.contains("terminal")
}

// ----------------------------------------------------------------------------
// 磁盘分区详情（lsblk -J）—— DTO + 纯解析函数，可单测
// ----------------------------------------------------------------------------

/// `GET /api/v1/disks/:name/partitions` 响应 DTO。
#[derive(Debug, serde::Serialize)]
struct DiskPartitions {
    /// 整盘裸名（如 nvme1n1）。
    disk: String,
    /// 是否有分区表 / 文件系统签名（true = 前端禁选，需先初始化）。
    has_partitions: bool,
    /// fstype 签名汇总（保序去重）。
    signatures: Vec<String>,
    /// 子分区明细（递归展平嵌套容器如 LUKS）。
    partitions: Vec<PartitionEntry>,
}

/// 单个分区明细（lsblk NAME/SIZE/FSTYPE/LABEL 四列）。
#[derive(Debug, serde::Serialize)]
struct PartitionEntry {
    name: String,
    size: String,
    fstype: Option<String>,
    label: Option<String>,
}

/// `lsblk -J`（单设备）输出的反序列化结构。
#[derive(Debug, serde::Deserialize)]
struct LsblkPartRoot {
    #[serde(default)]
    blockdevices: Vec<LsblkPartDevice>,
}

#[derive(Debug, serde::Deserialize)]
struct LsblkPartDevice {
    name: String,
    /// SIZE 可能是 "931.5G"（字符串）或 --bytes 整数；用 Value 兼容。
    #[serde(default)]
    size: serde_json::Value,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    children: Vec<LsblkPartDevice>,
}

/// 把 lsblk JSON Value（字符串或数字）转展示字符串。
fn lsblk_size_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// 解析 `lsblk -J -o NAME,SIZE,FSTYPE,LABEL /dev/<name>` 输出为 [`DiskPartitions`]。
///
/// 取 `blockdevices[0]`（即目标整盘）；子分区递归展平。签名 = 整盘与全部分区的
/// fstype 汇总；`has_partitions` = 有子分区或有签名。输出异常（无 blockdevices
/// 或 JSON 坏）返回 None（handler 层转 500）。
fn parse_lsblk_partitions(stdout: &str, disk: &str) -> Option<DiskPartitions> {
    let root: LsblkPartRoot = serde_json::from_str(stdout).ok()?;
    let dev = root.blockdevices.into_iter().next()?;
    let mut partitions = Vec::new();
    flatten_partitions(&dev, &mut partitions);
    let mut signatures = Vec::new();
    collect_part_fstypes(&dev, &mut signatures);
    Some(DiskPartitions {
        disk: disk.to_string(),
        has_partitions: !partitions.is_empty() || !signatures.is_empty(),
        signatures,
        partitions,
    })
}

/// 递归展平子分区（跳过整盘自身，只收 children）。
fn flatten_partitions(dev: &LsblkPartDevice, out: &mut Vec<PartitionEntry>) {
    for c in &dev.children {
        out.push(PartitionEntry {
            name: c.name.clone(),
            size: lsblk_size_to_string(&c.size),
            fstype: c.fstype.clone(),
            label: c.label.clone(),
        });
        flatten_partitions(c, out); // LUKS/LVM 嵌套容器也展平
    }
}

/// 递归收集整盘 + 子分区的 fstype 签名（保序去重）。
fn collect_part_fstypes(dev: &LsblkPartDevice, out: &mut Vec<String>) {
    if let Some(f) = dev.fstype.as_deref() {
        if !f.is_empty() && !out.iter().any(|s| s == f) {
            out.push(f.to_string());
        }
    }
    for c in &dev.children {
        collect_part_fstypes(c, out);
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 构造一条 [`RouteSpec`]（参数顺序与字段一致，便于上方紧凑声明）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "storage".to_string(),
        requires_auth,
        required_roles,
    }
}

/// 把可序列化结果转成 `serde_json::Value`，序列化失败统一映射为 `ApiGatewayError::Internal`。
fn to_value<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 构造一个 200 JSON 成功响应（scrub/quota 降级响应也用此，body 内自带 ok 字段）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 从 URL query 串中取首个匹配参数（如 `?pool=tank` → `Some("tank")`）。
///
/// 不引第三方 URL 解析库（横切规则），用 `&key=value` 朴素匹配；值经
/// percent-decoding-free 处理（ZFS 名不含需转义字符）。无匹配返回 None。
fn query_param(path_with_query: &str, key: &str) -> Option<String> {
    let query = path_with_query.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next()? == key {
            let v = it.next().unwrap_or("");
            if v.is_empty() {
                return None;
            }
            return Some(v.to_string());
        }
    }
    None
}

/// 把 [`StorageError`] 归类到 [`ApiGatewayError`]。
///
/// 映射策略（呼应 `StorageError → ApiError` 的 `error.rs` 语义）：
/// - NotFound 类（Pool/Dataset/Snapshot 不存在）→ `Internal` 携带 404 提示。
/// - Conflict 类（Pool/Dataset 已存在）→ `Internal` 携带 409 提示。
/// - InvalidVdev / CryptoError → `Internal` 携带 400 提示。
/// - 其余（CommandFailed / Io / Replication / Export）→ `Internal`。
///
/// 注：dispatch 把 `Err(ApiGatewayError)` 都渲染为 HTTP 500 + `{"error": <msg>}`，
/// 故 404/409/400 只是消息前缀，便于调用方/日志定位真实状态码语义；如需真实
/// 非 500 状态码，应在 handle 内显式返回 `Ok(error_response(<code>, ...))`。
fn map_storage_err(e: StorageError) -> ApiGatewayError {
    let (tag, msg) = match &e {
        StorageError::PoolNotFound(m)
        | StorageError::DatasetNotFound(m)
        | StorageError::SnapshotNotFound(m) => ("404", m.clone()),
        StorageError::PoolExists(m) | StorageError::DatasetExists(m) => ("409", m.clone()),
        StorageError::InvalidVdev(m) | StorageError::CryptoError(m) => ("400", m.clone()),
        StorageError::ReplicationFailed(m) | StorageError::ExportFailed(m) => ("502", m.clone()),
        StorageError::CommandFailed(m) => ("500", m.clone()),
        StorageError::Io(io) => ("500", io.to_string()),
    };
    let _ = e; // StorageError 已在 match 里读过；显式标注避免 move 警告
    ApiGatewayError::Internal(format!("[storage/{tag}] {msg}"))
}

// ----------------------------------------------------------------------------
// Scrub 状态解析 / Quota 命令构造（纯函数，可单测）
// ----------------------------------------------------------------------------

/// `GET /api/v1/pools/:id/scrub-status` 解析产物——`zpool status` 中 scan 段的 scrub 摘要。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct ScrubStatus {
    /// `running` / `completed` / `none`
    pub status: String,
    /// 进度百分比（0.0–100.0），仅 running 时有值。
    pub progress_pct: Option<f64>,
    /// scrub 开始时间（zpool status 文本，尽力解析）。
    pub start_time: Option<String>,
    /// scrub 结束时间。
    pub end_time: Option<String>,
    /// scrub 发现的错误数。
    pub errors: u64,
}

impl ScrubStatus {
    /// 默认 none 状态（无 scan 行或解析不出 scrub）。
    fn none() -> Self {
        Self {
            status: "none".into(),
            progress_pct: None,
            start_time: None,
            end_time: None,
            errors: 0,
        }
    }
}

/// 解析 `zpool status <pool>` 输出提取 scrub 状态。
///
/// 识别三种状态：
/// - `scan: scrub in progress ...` → `running`（尽力提取 `% done` 进度）
/// - `scan: scrub repaired ...` / `scrub canceled ...` → `completed`
/// - 无 `scan:` 行 → `none`
///
/// progress_pct / start_time / end_time / errors 尽力提取，解析失败的字段为 None/0。
/// 纯函数，可单测——不依赖真实 zpool 输出格式稳定性（仅按关键字匹配）。
pub fn parse_scrub_status(zpool_status_output: &str) -> ScrubStatus {
    // 检测是否存在 scan 行（以 "scan:" 开头，前导空白容忍）
    let has_scan = zpool_status_output
        .lines()
        .any(|l| l.trim_start().starts_with("scan:"));
    if !has_scan {
        return ScrubStatus::none();
    }

    // 把 scan 块及其后续缩进行合并为一行文本（scan 详情常跨多行）
    let mut collecting = false;
    let scan_text: String = zpool_status_output
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.starts_with("scan:") {
                collecting = true;
                Some(t.to_string())
            } else if collecting {
                // scan 块延续行：缩进的详情行；遇到空行或下一个段落标记则停止
                if t.is_empty()
                    || t.starts_with("errors:")
                    || t.starts_with("config:")
                    || t.starts_with("pool:")
                    || t.starts_with("state:")
                {
                    collecting = false;
                    None
                } else {
                    Some(t.to_string())
                }
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let status = if scan_text.contains("scrub in progress") {
        "running"
    } else if scan_text.contains("scrub repaired") || scan_text.contains("scrub canceled") {
        "completed"
    } else {
        "none"
    };

    let progress_pct = extract_percent(&scan_text);
    let errors = extract_scrub_errors(&scan_text);
    let start_time = if status == "running" {
        extract_date_after(&scan_text, "since ")
    } else {
        None
    };
    let end_time = if status == "completed" {
        extract_date_after(&scan_text, " on ")
    } else {
        None
    };

    ScrubStatus {
        status: status.into(),
        progress_pct,
        start_time,
        end_time,
        errors,
    }
}

/// 从文本提取 `NN.NN% done` 的百分比数值（向后收集数字与小数点）。
fn extract_percent(text: &str) -> Option<f64> {
    let marker = "% done";
    let idx = text.find(marker)?;
    let before = &text[..idx];
    let num_str: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    num_str.parse().ok()
}

/// 提取 `with N errors` 中的 N（scrub repaired 行的错误计数）。
fn extract_scrub_errors(text: &str) -> u64 {
    if let Some(idx) = text.find("with ") {
        let after = &text[idx + 5..];
        if let Some(eidx) = after.find(" errors") {
            return after[..eidx].trim().parse().unwrap_or(0);
        }
    }
    0
}

/// 提取 marker 之后的日期串（zpool 状态日期形如 `Wed Jun 14 12:00:00 2023`，取前 5 个 token）。
fn extract_date_after(text: &str, marker: &str) -> Option<String> {
    let idx = text.find(marker)?;
    let after = text[idx + marker.len()..].trim();
    let tokens: Vec<&str> = after.split_whitespace().take(5).collect();
    if tokens.len() >= 4 {
        Some(tokens.join(" "))
    } else {
        None
    }
}

/// 构造 `zfs set quota` 命令参数（`set` 子命令 + 属性赋值 + dataset 名）。
///
/// 返回的 Vec 是 `zfs` 程序的参数（即 `sudo zfs <args>` 中 args 部分）：
/// `["set", "quota=<v>", ("refreservation=<v>"?), "<dataset>"]`。
/// 多属性一次 `zfs set` 完成（ZFS 支持单条 set 设多个属性）。
pub fn build_quota_cmd(
    dataset: &str,
    quota_bytes: u64,
    refreservation: Option<u64>,
) -> Vec<String> {
    let mut args = vec!["set".to_string(), format!("quota={quota_bytes}")];
    if let Some(r) = refreservation {
        args.push(format!("refreservation={r}"));
    }
    args.push(dataset.to_string());
    args
}

// ----------------------------------------------------------------------------
// 单元测——注入 FixtureRunner（伪造 zpool/zfs 输出），测路由→后端→JSON 全链路
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use os_storage::model::VdevKind;

    /// 测试用 [`CommandRunner`]——按 (program, args[0]) 分发预设 fixture。
    ///
    /// 复刻 `os-storage::backend_impl::tests::FixtureRunner` 的模式（该 fixture 是
    /// os-storage crate 私有，无法在此直接复用），保持一致的 (program, 子命令) 匹配规则。
    struct FixtureRunner {
        fixtures: std::sync::Mutex<Vec<FixtureEntry>>,
    }

    struct FixtureEntry {
        program: &'static str,
        subcmd: &'static str,
        output: CommandOutput,
    }

    impl FixtureRunner {
        fn new() -> Self {
            Self {
                fixtures: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// 注册一条 fixture：当 `<program> <subcmd> ...` 被调用时返回 `output`。
        fn on(
            mut self,
            program: &'static str,
            subcmd: &'static str,
            output: CommandOutput,
        ) -> Self {
            self.fixtures.get_mut().unwrap().push(FixtureEntry {
                program,
                subcmd,
                output,
            });
            self
        }
    }

    #[async_trait]
    impl CommandRunner for FixtureRunner {
        async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
            let subcmd = args.first().map(String::as_str);
            let fixtures = self.fixtures.lock().unwrap();
            for f in fixtures.iter() {
                if f.program == program && subcmd == Some(f.subcmd) {
                    return Ok(f.output.clone());
                }
            }
            Err(StorageError::CommandFailed(format!(
                "FixtureRunner 无匹配 fixture: {program} {:?}",
                args.join(" ")
            )))
        }
    }

    /// 构造一条真实 `zpool list -p -H` 输出行（testpool，10TB，ONLINE）。
    fn pool_list_line(name: &str) -> String {
        format!(
            "{name}\t10995116277760\t1374389534720\t9620726743040\t-\t-\t12\t12\t1.00x\tONLINE\t-"
        )
    }

    /// 构造一个挂了 FixtureRunner 的 StorageRouteHandler。
    ///
    /// 探测固定 `|| true`（fixture 模拟 ZFS 正常工作），不依赖宿主机是否装
    /// zfsutils——降级路径见 `handler_zfs_offline`。
    fn handler_with(fixture: FixtureRunner) -> StorageRouteHandler {
        let backend = Arc::new(ZfsCliBackend::with_runner(Box::new(fixture)));
        StorageRouteHandler::with_cmd_runner(backend, Arc::new(TokioCommandRunner))
    }

    /// 构造一个「无 ZFS 工具」的 handler（探测注入 false）：读端点应降级空态、
    /// 写端点应 400。backend/runner 走 fixture——降级路径不应触碰它们（可断言零调用）。
    fn handler_zfs_offline() -> (StorageRouteHandler, Arc<FixedCmdRunner>) {
        let cmd = Arc::new(FixedCmdRunner {
            output: CommandOutput::ok(),
            spawn_err: false,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let backend = Arc::new(ZfsCliBackend::with_runner(Box::new(FixtureRunner::new())));
        let h = StorageRouteHandler::with_zfs_probe(backend, cmd.clone(), || false);
        (h, cmd)
    }

    // —— routes() 声明 ——

    #[tokio::test]
    async fn routes_declares_storage_endpoints() {
        let h = handler_with(FixtureRunner::new());
        let routes = h.routes().await;
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/pools")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/pools")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/disks")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/datasets")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/datasets")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/snapshots")));
        // 所有路由归属 storage 组件
        assert!(routes.iter().all(|r| r.handler_component == "storage"));
        // 写操作要求 admin
        let post_pools = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/pools")
            .unwrap();
        assert!(post_pools.requires_auth);
        assert_eq!(post_pools.required_roles, vec!["admin".to_string()]);
        // —— 新增路由（scrub / quota）——
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/pools/:id/scrub")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/pools/:id/scrub-status")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/datasets/:id/quota")));
        // scrub 启动 + quota 要求 admin
        let post_scrub = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/pools/:id/scrub")
            .unwrap();
        assert!(post_scrub.requires_auth);
        assert_eq!(post_scrub.required_roles, vec!["admin".to_string()]);
        let post_quota = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/datasets/:id/quota")
            .unwrap();
        assert!(post_quota.requires_auth);
        assert_eq!(post_quota.required_roles, vec!["admin".to_string()]);
        // scrub-status 查询无需 admin
        let get_scrub = routes
            .iter()
            .find(|r| r.method == HttpMethod::Get && r.path == "/api/v1/pools/:id/scrub-status")
            .unwrap();
        assert!(!get_scrub.requires_auth);
    }

    // —— GET /api/v1/pools ——

    #[tokio::test]
    async fn get_pools_returns_real_zfs_data_as_json() {
        let stdout = format!("{}\n", pool_list_line("testpool"));
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            },
        ));
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/pools".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.expect("list_pools 应成功");
        assert_eq!(resp.status, 200);
        // body 是数组，含 testpool
        let arr = resp.body.as_array().expect("body 应为数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "testpool");
        assert_eq!(arr[0]["id"], "testpool");
        // 容量字段精确（验证不是占位 JSON）
        assert_eq!(arr[0]["capacity"]["total_bytes"], 10_995_116_277_760_u64);
    }

    #[tokio::test]
    async fn get_pools_empty_returns_empty_array() {
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        ));
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/pools".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_pools_query_string_stripped() {
        // ?verbose=1 不影响路径分发（验证 split('?') 兜底）
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: pool_list_line("tank"),
                stderr: String::new(),
            },
        ));
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/pools?verbose=1".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body[0]["name"], "tank");
    }

    // —— POST /api/v1/pools ——

    #[tokio::test]
    async fn post_pools_creates_and_returns_201() {
        // create 成功空输出，随后 list 返回新池（与 backend_impl::create_pool_round_trips 同款）
        let fixture = FixtureRunner::new()
            .on("zpool", "create", CommandOutput::ok())
            .on(
                "zpool",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: pool_list_line("tank"),
                    stderr: String::new(),
                },
            );
        let h = handler_with(fixture);
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/pools".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "name": "tank",
                "vdevs": [
                    { "kind": "mirror", "disks": ["/dev/sdb", "/dev/sdc"] }
                ]
            }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create_pool 应成功");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["name"], "tank");
        assert_eq!(resp.body["capacity"]["total_bytes"], 10_995_116_277_760_u64);
    }

    #[tokio::test]
    async fn post_pools_invalid_body_returns_err() {
        // 缺 name 字段 → 解析失败 → ApiGatewayError::Internal
        let h = handler_with(FixtureRunner::new());
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/pools".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "vdevs": [] }),
            auth: None,
        };
        let err = h.handle(req).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn post_pools_already_exists_maps_to_conflict_hint() {
        // create 命中 "already exists" → backend 返回 PoolExists → map_storage_err 标 [409]
        let fixture = FixtureRunner::new().on(
            "zpool",
            "create",
            CommandOutput::fail(1, "cannot create 'tank': pool already exists"),
        );
        let h = handler_with(fixture);
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/pools".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "name": "tank", "vdevs": [] }),
            auth: None,
        };
        let err = h.handle(req).await.unwrap_err();
        match err {
            ApiGatewayError::Internal(msg) => {
                assert!(msg.contains("[storage/409]"), "应带 409 标签: {msg}");
            }
            other => panic!("应为 Internal，实际: {other:?}"),
        }
    }

    // —— GET /api/v1/datasets ——

    #[tokio::test]
    async fn get_datasets_returns_array() {
        let stdout =
            "tank/media\t5497558138880\t5497558138880\tyes\toff\ntank/docs\t50\t150\tyes\toff";
        let h = handler_with(FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        ));
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/datasets".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "tank/media");
        assert_eq!(arr[0]["pool"], "tank");
    }

    #[tokio::test]
    async fn get_datasets_with_pool_query_param() {
        // ?pool=tank 限定单池——验证 query_param 解析
        let stdout = "tank/media\t100\t200\tyes\toff";
        let h = handler_with(FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        ));
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/datasets?pool=tank".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
    }

    // —— GET /api/v1/snapshots ——

    #[tokio::test]
    async fn get_snapshots_returns_array() {
        let stdout = "tank/media@s1\t1024\t1700000000\ntank/media@s2\t2048\t1700000100";
        let h = handler_with(FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        ));
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/snapshots".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "tank/media@s1");
        assert_eq!(arr[0]["dataset"], "tank/media");
    }

    // —— 兜底 ——

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        // DELETE /api/v1/pools 未声明 → 兜底 404（Ok，非 Err）
        let h = handler_with(FixtureRunner::new());
        let req = ApiRequest {
            method: HttpMethod::Delete,
            path: "/api/v1/pools".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    // —— 辅助函数自测 ——

    #[test]
    fn query_param_parses_simple() {
        assert_eq!(
            query_param("/api/v1/datasets?pool=tank", "pool"),
            Some("tank".into())
        );
        assert_eq!(
            query_param("/api/v1/snapshots?dataset=tank/media&x=1", "dataset"),
            Some("tank/media".into())
        );
        assert_eq!(query_param("/api/v1/datasets", "pool"), None);
        assert_eq!(query_param("/api/v1/datasets?pool=", "pool"), None);
        assert_eq!(query_param("/api/v1/datasets?other=1", "pool"), None);
    }

    #[test]
    fn vdevspec_round_trips_for_post_body() {
        // 验证 POST /api/v1/pools 的 body 反序列化（VdevSpec serde 与 model 一致）
        let body = serde_json::json!({
            "name": "tank",
            "vdevs": [
                { "kind": "mirror", "disks": ["/dev/sdb", "/dev/sdc"] },
                { "kind": "disk", "disks": ["/dev/sdd"] }
            ]
        });
        #[derive(serde::Deserialize)]
        struct Req {
            name: String,
            vdevs: Vec<VdevSpec>,
        }
        let req: Req = serde_json::from_value(body).unwrap();
        assert_eq!(req.name, "tank");
        assert_eq!(req.vdevs.len(), 2);
        assert_eq!(req.vdevs[0].kind, VdevKind::Mirror);
        assert_eq!(req.vdevs[1].kind, VdevKind::Disk);
        assert_eq!(
            req.vdevs[0].disks,
            vec!["/dev/sdb".to_string(), "/dev/sdc".to_string()]
        );
    }

    // —— 磁盘探测辅助函数自测（不依赖真实 lsblk，纯逻辑覆盖）——

    #[test]
    fn parse_size_handles_formats() {
        // --bytes 模式：JSON 整数（用 u64 显式构造，避免 json! 宏推断为 i32 溢出）
        let big: u64 = 1_000_204_886_016;
        assert_eq!(parse_size_value(&serde_json::Value::from(big)), big);
        assert_eq!(parse_size_value(&serde_json::Value::from(0u64)), 0);
        // 字符串模式：纯整数字符串
        assert_eq!(parse_size_str("1000204886016"), big);
        assert_eq!(parse_size_str("0"), 0);
        // 字符串模式：人类可读单位（兜底估算）
        assert_eq!(parse_size_str("1K"), 1024);
        assert_eq!(parse_size_str("1M"), 1024 * 1024);
        assert_eq!(parse_size_str("1G"), 1024 * 1024 * 1024);
        assert_eq!(parse_size_str("1T"), 1024u64.pow(4));
        // 小数 + 单位
        assert_eq!(parse_size_str("931.5G"), (931.5 * 1024f64.powi(3)) as u64);
        // 空串 / 无法解析 → 0
        assert_eq!(parse_size_str(""), 0);
        assert_eq!(parse_size_str("n/a"), 0);
        // null / 其它类型 → 0
        assert_eq!(parse_size_value(&serde_json::Value::Null), 0);
    }

    #[test]
    fn lsblk_device_system_mount_detection() {
        // 整盘直接挂载 /
        let sys_root = LsblkDevice {
            name: "/dev/sda".into(),
            size: serde_json::json!(0),
            kind: "disk".into(),
            mountpoint: Some("/".into()),
            model: None,
            fstype: None,
            children: vec![],
        };
        assert!(sys_root.is_system_mount());

        // 分区挂载 /boot/efi（递归子节点）
        let sys_boot = LsblkDevice {
            name: "/dev/nvme0n1".into(),
            size: serde_json::json!(0),
            kind: "disk".into(),
            mountpoint: None,
            model: None,
            fstype: None,
            children: vec![LsblkDevice {
                name: "/dev/nvme0n1p1".into(),
                size: serde_json::json!(0),
                kind: "part".into(),
                mountpoint: Some("/boot/efi".into()),
                model: None,
                fstype: None,
                children: vec![],
            }],
        };
        assert!(sys_boot.is_system_mount());

        // swap 分区（mountpoint 显示为 [SWAP]）
        let sys_swap = LsblkDevice {
            name: "/dev/sda".into(),
            size: serde_json::json!(0),
            kind: "disk".into(),
            mountpoint: None,
            model: None,
            fstype: None,
            children: vec![LsblkDevice {
                name: "/dev/sda2".into(),
                size: serde_json::json!(0),
                kind: "part".into(),
                mountpoint: Some("[SWAP]".into()),
                model: None,
                fstype: None,
                children: vec![],
            }],
        };
        assert!(sys_swap.is_system_mount());

        // 纯数据盘（无系统挂载）
        let data_disk = LsblkDevice {
            name: "/dev/sdb".into(),
            size: serde_json::json!(0),
            kind: "disk".into(),
            mountpoint: None,
            model: Some("ST1000LM035".into()),
            fstype: None,
            children: vec![LsblkDevice {
                name: "/dev/sdb1".into(),
                size: serde_json::json!(0),
                kind: "part".into(),
                mountpoint: Some("/mnt/data".into()),
                model: None,
                fstype: None,
                children: vec![],
            }],
        };
        assert!(!data_disk.is_system_mount());
    }

    // —— zpool list -H -v 解析（ZFS 池成员过滤）——

    #[test]
    fn parse_zpool_members_real_output() {
        // 复刻真实 `zpool list -H -v` 输出：tank 数据池含 sdb，cache 池含 sda，
        // 池名行（tank/cache）不是磁盘，缩进行（sda/sdb）才是成员。
        let stdout = "\
tank\t928G\t612K\t928G\t-\t-\t0%\t0%\t1.00x\tONLINE\t-\n\
\tsdb\t932G\t612K\t928G\t-\t-\t0%\t0.00%\t-\tONLINE\t-\n\
cache          -      -      -        -         -      -      -      -         -        -\n\
\tsda\t224G\t0\t224G\t-\t-\t0%\t0.00%\t-\tONLINE\t-\n";
        let members: Vec<String> = parse_zpool_members(stdout).collect();
        assert_eq!(
            members,
            vec!["/dev/sdb".to_string(), "/dev/sda".to_string()]
        );
    }

    #[test]
    fn parse_zpool_members_handles_nvme_and_partition() {
        // 混合设备：nvme0n1（整盘）、sda1（cache/slog 单分区）、vd b
        let stdout = "\
fast\t100G\t-\t100G\t-\n\
\tnvme0n1\t500G\t-\t500G\t-\n\
log\t-\t-\t-\t-\n\
\tsda1\t10G\t0\t10G\t-\n\
\tvdb\t20G\t0\t20G\t-\n";
        let members: Vec<String> = parse_zpool_members(stdout).collect();
        // sda1 归一化为整盘 /dev/sda，避免 cache 占用单分区时整盘被误判可用
        assert_eq!(
            members,
            vec![
                "/dev/nvme0n1".to_string(),
                "/dev/sda".to_string(),
                "/dev/vdb".to_string(),
            ]
        );
    }

    #[test]
    fn parse_zpool_members_empty_and_no_pool() {
        // 空输出 / 无池行 → 不 yield 任何成员
        assert_eq!(parse_zpool_members("").count(), 0);
        // 仅含一个池名行（无成员）→ 不 yield
        let only_pool = "no pools available\n";
        assert_eq!(parse_zpool_members(only_pool).count(), 0);
        // 池名是普通词（testpool），不匹配内核盘名 → 不 yield
        assert_eq!(parse_zpool_members("testpool\t1T\t-\t1T\t-\n").count(), 0);
    }

    #[test]
    fn strip_partition_suffix_cases() {
        // 无分区：原样
        assert_eq!(strip_partition_suffix("sda"), "sda");
        assert_eq!(strip_partition_suffix("nvme0n1"), "nvme0n1");
        assert_eq!(strip_partition_suffix("mmcblk0"), "mmcblk0");
        // sd/vd/hd 风格：尾数字分区剥离
        assert_eq!(strip_partition_suffix("sda1"), "sda");
        assert_eq!(strip_partition_suffix("vdb2"), "vdb");
        assert_eq!(strip_partition_suffix("hdc3"), "hdc");
        // nvme/mmcblk 风格：p<digits> 剥离
        assert_eq!(strip_partition_suffix("nvme0n1p2"), "nvme0n1");
        assert_eq!(strip_partition_suffix("mmcblk0p1"), "mmcblk0");
        // 非磁盘词不被误处理
        assert_eq!(strip_partition_suffix("cache"), "cache");
    }

    #[test]
    fn is_kernel_disk_name_distinguishes_pool_from_disk() {
        // 池名行不应误判为磁盘
        assert!(!is_kernel_disk_name("tank"));
        assert!(!is_kernel_disk_name("cache"));
        assert!(!is_kernel_disk_name("no"));
        assert!(!is_kernel_disk_name("testpool"));
        // 整盘 / 分区名应命中
        assert!(is_kernel_disk_name("sda"));
        assert!(is_kernel_disk_name("sdb"));
        assert!(is_kernel_disk_name("sda1"));
        assert!(is_kernel_disk_name("nvme0n1"));
        assert!(is_kernel_disk_name("nvme0n1p2"));
        assert!(is_kernel_disk_name("vdb"));
        assert!(is_kernel_disk_name("mmcblk0"));
    }

    #[test]
    fn lsblk_json_parses_typical_output() {
        // 验证 LsblkRoot 反序列化与过滤逻辑（不跑真实 lsblk）。
        // size 用 --bytes 模式的整数（与生产 lsblk 调用一致）。
        let json = r#"{
            "blockdevices": [
                {"name":"/dev/loop0","size":4194304,"type":"loop","mountpoint":"/snap/x","model":null},
                {"name":"/dev/sda","size":240057409536,"type":"disk","mountpoint":null,"model":"Kingston",
                 "children":[{"name":"/dev/sda1","size":1048576,"type":"part","mountpoint":"/boot","model":null}]},
                {"name":"/dev/sdb","size":1000204886016,"type":"disk","mountpoint":null,"model":"Seagate","children":[]},
                {"name":"/dev/sdb1","size":500107861504,"type":"part","mountpoint":null,"model":null}
            ]
        }"#;
        let root: LsblkRoot = serde_json::from_str(json).unwrap();
        let disks: Vec<DiskInfo> = root
            .blockdevices
            .into_iter()
            .filter(|d| d.kind == "disk")
            .filter(|d| !d.is_system_mount())
            .map(|d| {
                let signatures = collect_fstypes(&d);
                DiskInfo {
                    name: d.name.clone(),
                    size_bytes: parse_size_value(&d.size),
                    model: d.model.unwrap_or_default().trim().to_string(),
                    available: true,
                    in_use: false,
                    has_partitions: !d.children.is_empty() || !signatures.is_empty(),
                    signatures,
                    member_of: None,
                    zfs_pool_hint: None,
                }
            })
            .collect();
        // 只剩 /dev/sdb（loop 过滤、sda 因 /boot 系统挂载过滤、sdb1 因 part 过滤）
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sdb");
        assert_eq!(disks[0].size_bytes, 1_000_204_886_016);
        assert_eq!(disks[0].model, "Seagate");
        assert!(disks[0].available);
        // sdb 无子分区无 fstype → 干净盘，可直接建池
        assert!(!disks[0].has_partitions);
        assert!(disks[0].signatures.is_empty());
    }

    #[test]
    fn detect_disks_filters_out_zpool_members() {
        // 复刻 detect_disks 的完整过滤链（含 ZFS 池成员过滤），验证：
        // sda 已在 cache 池、sdb 已在 tank 池 → 两者都被排除，
        // nvme0n1（系统盘 /boot+root）也被排除 → 最终只剩 sdc。
        let json = r#"{
            "blockdevices": [
                {"name":"/dev/sda","size":240057409536,"type":"disk","mountpoint":null,"model":"Kingston","children":[]},
                {"name":"/dev/sdb","size":1000204886016,"type":"disk","mountpoint":null,"model":"Seagate","children":[]},
                {"name":"/dev/sdc","size":2000398934016,"type":"disk","mountpoint":null,"model":"WD","children":[]},
                {"name":"/dev/nvme0n1","size":500107861504,"type":"disk","mountpoint":null,"model":"Samsung",
                 "children":[{"name":"/dev/nvme0n1p1","size":1048576,"type":"part","mountpoint":"/boot/efi","model":null},
                             {"name":"/dev/nvme0n1p2","size":499000000000,"type":"part","mountpoint":"/","model":null}]}
            ]
        }"#;
        let root: LsblkRoot = serde_json::from_str(json).unwrap();
        let zpool_stdout = "\
tank\t928G\t612K\t928G\t-\t-\t0%\t0%\t1.00x\tONLINE\t-\n\
\tsdb\t932G\t612K\t928G\t-\t-\t0%\t0.00%\t-\tONLINE\t-\n\
cache          -      -      -        -         -      -      -      -         -        -\n\
\tsda\t224G\t0\t224G\t-\t-\t0%\t0.00%\t-\tONLINE\t-\n";
        let members: std::collections::HashSet<String> =
            parse_zpool_members(zpool_stdout).collect();

        let disks: Vec<DiskInfo> = root
            .blockdevices
            .into_iter()
            .filter(|d| d.kind == "disk")
            .filter(|d| !d.is_system_mount())
            .filter(|d| !members.contains(&d.name))
            .map(|d| {
                let signatures = collect_fstypes(&d);
                DiskInfo {
                    name: d.name.clone(),
                    size_bytes: parse_size_value(&d.size),
                    model: d.model.unwrap_or_default().trim().to_string(),
                    available: true,
                    in_use: false,
                    has_partitions: !d.children.is_empty() || !signatures.is_empty(),
                    signatures,
                    member_of: None,
                    zfs_pool_hint: None,
                }
            })
            .collect();
        // sda/sdb 被 zpool 占用排除，nvme0n1 系统挂载排除 → 只剩 sdc
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sdc");
    }

    // —— 磁盘初始化（2026-08-23：先提示确认，再 wipefs -a）——

    #[test]
    fn disk_name_whitelist_accepts_and_rejects() {
        // sd 风格：sd + 小写字母（sda / sdz / sdaa）
        for ok in ["sda", "sdb", "sdz", "sdaa"] {
            assert!(valid_disk_name(ok), "{ok} 应在白名单");
        }
        // nvme 风格：nvme + 数字 + n + 数字
        for ok in ["nvme0n1", "nvme1n1", "nvme10n2", "nvme12n34"] {
            assert!(valid_disk_name(ok), "{ok} 应在白名单");
        }
        // 分区名 / 路径穿越 / 前缀缺失 / 其他总线 → 一律拒绝
        for bad in [
            "",
            "sd",       // 无字母后缀
            "sd1",      // sd 后不允许数字（这是分区号）
            "sda1",     // 分区名
            "sdaB",     // 大写
            "nvme0",    // 缺 n1
            "nvme0n",   // 缺命名空间号
            "nvme0np1", // p 不是数字
            "nvmе0n1",  // 含非 ASCII（西里尔 е）
            "/dev/sda", // 带路径前缀
            "../../etc/passwd",
            "mmcblk0", // 不在白名单（保守：只放开 sd/nvme）
            "vdb",
            "hdс",
        ] {
            assert!(!valid_disk_name(bad), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn parse_wipefs_signatures_typical_output() {
        // 真实 wipefs 扫描输出（无 -a）：表头 + 多行签名（TYPE 是第 3 列）
        let stdout = "\
DEVICE       OFFSET TYPE UUID LABEL
nvme1n1p3    0x1c0 BitLocker
nvme1n1      0x200 gpt
nvme1n1p3    0x1000 ntfs
nvme1n1p1    0x1000 vfat SYSTEM";
        let sigs = parse_wipefs_signatures(stdout);
        // 保序去重
        assert_eq!(
            sigs,
            vec![
                "BitLocker".to_string(),
                "gpt".to_string(),
                "ntfs".to_string(),
                "vfat".to_string()
            ]
        );
        // 重复类型去重（同一签名出现在多个偏移）
        let dup = "\
DEVICE  OFFSET TYPE UUID LABEL
sda1    0x0    gpt
sda2    0x200  gpt
sda     0x200  gpt";
        assert_eq!(
            parse_wipefs_signatures(dup),
            vec!["gpt".to_string()],
            "重复签名应去重"
        );
        // 空输出 / 仅表头 → 空列表
        assert!(parse_wipefs_signatures("").is_empty());
        assert!(parse_wipefs_signatures("DEVICE OFFSET TYPE UUID LABEL\n").is_empty());
    }

    #[test]
    fn is_permission_denied_classification() {
        // sudo 免密未配置的典型 stderr → 401 语义
        for denied in [
            "sudo: a password is required",
            "sudo: interactive authentication is required",
            "sudo: oem is not in the sudoers file.  This incident will be reported.",
            "wipefs: error: /dev/sdb: probing initialization failed: Permission denied",
            "sudo: no terminal was available",
        ] {
            assert!(is_permission_denied(denied), "应判为无权限: {denied}");
        }
        // 其它失败（设备不存在 / 命令缺失）不是权限问题
        for other in [
            "wipefs: error: /dev/sdzz: probing initialization failed: No such device",
            "sudo: wipefs: command not found",
            "",
        ] {
            assert!(!is_permission_denied(other), "不应误判为无权限: {other}");
        }
    }

    #[test]
    fn parse_lsblk_partitions_builds_dto() {
        // 带 BitLocker + vfat 分区的 NVMe（真实 lsblk -J 单设备输出形态）
        let json = r#"{
            "blockdevices": [
                {"name":"nvme1n1","size":"931.5G","fstype":null,"label":null,
                 "children":[
                    {"name":"nvme1n1p1","size":"100M","fstype":"vfat","label":"EFI",
                     "children":[]},
                    {"name":"nvme1n1p3","size":"800G","fstype":"BitLocker","label":null,
                     "children":[]}
                 ]}
            ]
        }"#;
        let dp = parse_lsblk_partitions(json, "nvme1n1").expect("应解析成功");
        assert_eq!(dp.disk, "nvme1n1");
        assert!(dp.has_partitions, "有子分区 → 需初始化");
        assert_eq!(
            dp.signatures,
            vec!["vfat".to_string(), "BitLocker".to_string()]
        );
        assert_eq!(dp.partitions.len(), 2);
        assert_eq!(dp.partitions[0].name, "nvme1n1p1");
        assert_eq!(dp.partitions[0].size, "100M");
        assert_eq!(dp.partitions[0].fstype.as_deref(), Some("vfat"));
        assert_eq!(dp.partitions[0].label.as_deref(), Some("EFI"));
        assert_eq!(dp.partitions[1].fstype.as_deref(), Some("BitLocker"));

        // 干净盘：无子分区无签名 → has_partitions=false（可直接建池）
        let blank = r#"{"blockdevices":[{"name":"sdc","size":"2T","fstype":null,"label":null,"children":[]}]}"#;
        let dp = parse_lsblk_partitions(blank, "sdc").unwrap();
        assert!(!dp.has_partitions);
        assert!(dp.partitions.is_empty());
        assert!(dp.signatures.is_empty());

        // 整盘直挂文件系统（无分区表但有 ext4 签名）→ 也算需初始化
        let whole_fs = r#"{"blockdevices":[{"name":"sdd","size":"1T","fstype":"ext4","label":"data","children":[]}]}"#;
        let dp = parse_lsblk_partitions(whole_fs, "sdd").unwrap();
        assert!(dp.has_partitions);
        assert_eq!(dp.signatures, vec!["ext4".to_string()]);

        // 嵌套容器（LUKS 内 ext4）→ 分区展平 + 签名递归收集
        let luks = r#"{"blockdevices":[
            {"name":"sde","size":"1T","fstype":null,"label":null,"children":[
                {"name":"sde1","size":"1T","fstype":"crypto_LUKS","label":null,"children":[
                    {"name":"cryptroot","size":"1T","fstype":"ext4","label":null,"children":[]}
                ]}
            ]}
        ]}"#;
        let dp = parse_lsblk_partitions(luks, "sde").unwrap();
        assert_eq!(dp.partitions.len(), 2, "嵌套分区应展平");
        assert!(dp.signatures.contains(&"crypto_LUKS".to_string()));
        assert!(dp.signatures.contains(&"ext4".to_string()));

        // 异常输出（空 blockdevices / 坏 JSON）→ None（handler 转 500/降级）
        assert!(parse_lsblk_partitions(r#"{"blockdevices":[]}"#, "sda").is_none());
        assert!(parse_lsblk_partitions("not json", "sda").is_none());
    }

    // —— 初始化 / 分区端点：handle 层（非法名 400 在执行任何命令前短路）——

    fn post_disk_action(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    #[tokio::test]
    async fn initialize_invalid_disk_name_returns_400() {
        // 路径穿越 / 分区名 / 白名单外 → 400（不触碰任何真实设备）
        let h = handler_with(FixtureRunner::new());
        for bad in [
            "/api/v1/disks/../../etc/initialize",
            "/api/v1/disks/sda1/initialize",    // 分区
            "/api/v1/disks/mmcblk0/initialize", // 白名单外
        ] {
            let resp = h.handle(post_disk_action(bad)).await.unwrap();
            assert_eq!(resp.status, 400, "{bad} 应 400");
            assert!(resp.body["error"].as_str().unwrap().contains("非法磁盘名"));
        }
    }

    #[tokio::test]
    async fn initialize_endpoint_returns_error_response_not_panic() {
        // 合法名但设备不存在（sdzz）：wipefs 经 sudo 失败 → 401（免密未配置）或
        // 500（设备不存在/命令缺失），绝不 panic、绝不静默成功。
        let h = handler_with(FixtureRunner::new());
        let resp = h
            .handle(post_disk_action("/api/v1/disks/sdzz/initialize"))
            .await
            .expect("初始化端点应返回响应不 panic");
        assert!(
            resp.status == 401 || resp.status == 500,
            "应返回 401/500 错误响应，实际 {}",
            resp.status
        );
        let err = resp.body["error"].as_str().unwrap_or_default();
        assert!(!err.is_empty(), "错误响应应带 error 文本");
    }

    #[tokio::test]
    async fn partitions_invalid_disk_name_returns_400() {
        let h = handler_with(FixtureRunner::new());
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/disks/sda1/partitions".into(), // 分区名非法
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("非法磁盘名"));
    }

    #[tokio::test]
    async fn partitions_endpoint_degrades_without_panic() {
        // 合法名但设备不存在（sdzz）→ 降级 200 + warning（不阻断向导）
        let h = handler_with(FixtureRunner::new());
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/disks/sdzz/partitions".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.expect("分区详情端点应返回响应不 panic");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["disk"], "sdzz");
        assert_eq!(resp.body["has_partitions"], false);
        assert_eq!(resp.body["partitions"].as_array().unwrap().len(), 0);
        assert!(
            resp.body["warning"].is_string(),
            "降级响应应带 warning: {}",
            resp.body
        );
    }

    // —— 磁盘端点鉴权矩阵（路由声明层）——

    #[tokio::test]
    async fn disk_routes_auth_matrix() {
        let h = handler_with(FixtureRunner::new());
        let routes = h.routes().await;
        // 路由归属 storage 组件
        assert!(routes.iter().all(|r| r.handler_component == "storage"));
        // GET /disks/:name/partitions：只读探测，无需登录
        let part = routes
            .iter()
            .find(|r| r.method == HttpMethod::Get && r.path == "/api/v1/disks/:name/partitions")
            .expect("应声明分区详情路由");
        assert!(!part.requires_auth);
        assert!(part.required_roles.is_empty());
        // POST /disks/:name/initialize：擦盘高危操作，必须 admin
        let init = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/disks/:name/initialize")
            .expect("应声明磁盘初始化路由");
        assert!(init.requires_auth);
        assert_eq!(init.required_roles, vec!["admin".to_string()]);
    }

    // —— map_storage_err 覆盖 ——

    #[test]
    fn map_storage_err_tags_all_categories() {
        // NotFound → [404]
        let e = map_storage_err(StorageError::PoolNotFound("p".into()));
        assert!(matches!(e, ApiGatewayError::Internal(ref m) if m.contains("[storage/404]")));
        // Conflict → [409]
        let e = map_storage_err(StorageError::DatasetExists("d".into()));
        assert!(matches!(e, ApiGatewayError::Internal(ref m) if m.contains("[storage/409]")));
        // InvalidVdev → [400]
        let e = map_storage_err(StorageError::InvalidVdev("v".into()));
        assert!(matches!(e, ApiGatewayError::Internal(ref m) if m.contains("[storage/400]")));
        // Replication → [502]
        let e = map_storage_err(StorageError::ReplicationFailed("r".into()));
        assert!(matches!(e, ApiGatewayError::Internal(ref m) if m.contains("[storage/502]")));
        // CommandFailed → [500]
        let e = map_storage_err(StorageError::CommandFailed("c".into()));
        assert!(matches!(e, ApiGatewayError::Internal(ref m) if m.contains("[storage/500]")));
        // Io → [500]
        let e = map_storage_err(StorageError::Io(std::io::Error::other("x")));
        assert!(matches!(e, ApiGatewayError::Internal(ref m) if m.contains("[storage/500]")));
    }

    // —— 验证 SnapshotId/DatasetId 不被忽略（避免 unused import 警告误判）——
    // 注：这些 newtype 在 handle 里已使用（DatasetId::new / PoolId::new），
    // SnapshotId 当前路由未直接用，这里补一个引用避免 dead_code 误报（实际由 list_snapshots 间接覆盖）。
    #[test]
    fn snapshot_id_unused_guard() {
        let s = SnapshotId::new("tank/media@s1");
        assert_eq!(s.as_str(), "tank/media@s1");
    }

    // —— scrub 状态解析（纯函数）——

    #[test]
    fn parse_scrub_status_running() {
        let status_output = "\
  pool: tank
 state: ONLINE
  scan: scrub in progress since Wed Jun 14 12:00:00 2023
        1.20T scanned at 100M/s, 800G issued, 1.50T total
        800G repaired, 53.33% done, 0 days 12:00:00 to go
errors: No known data errors";
        let s = parse_scrub_status(status_output);
        assert_eq!(s.status, "running");
        assert_eq!(s.progress_pct, Some(53.33));
        assert!(s.start_time.is_some(), "running 应有 start_time");
    }

    #[test]
    fn parse_scrub_status_completed() {
        let status_output = "\
  pool: tank
 state: ONLINE
  scan: scrub repaired 0B in 00:01:23 with 0 errors on Wed Jun 14 12:01:23 2023
errors: No known data errors";
        let s = parse_scrub_status(status_output);
        assert_eq!(s.status, "completed");
        assert_eq!(s.errors, 0);
        assert!(s.end_time.is_some(), "completed 应有 end_time");
    }

    #[test]
    fn parse_scrub_status_completed_with_errors() {
        let status_output = "\
  scan: scrub repaired 12K in 00:00:01 with 3 errors on Fri Aug  1 03:00:01 2026
errors: 3 data errors";
        let s = parse_scrub_status(status_output);
        assert_eq!(s.status, "completed");
        assert_eq!(s.errors, 3);
    }

    #[test]
    fn parse_scrub_status_none_no_scan_line() {
        // 无 scan 行 → none
        let status_output = "\
  pool: tank
 state: ONLINE
config:
    NAME        STATE     READ WRITE CKSUM
    tank        ONLINE       0     0     0
errors: No known data errors";
        let s = parse_scrub_status(status_output);
        assert_eq!(s.status, "none");
        assert!(s.progress_pct.is_none());
        assert_eq!(s.errors, 0);
    }

    #[test]
    fn parse_scrub_status_empty_output() {
        assert_eq!(parse_scrub_status("").status, "none");
    }

    // —— build_quota_cmd ——（纯函数）

    #[test]
    fn build_quota_cmd_has_quota_and_dataset() {
        let args = build_quota_cmd("tank/media", 1_099_511_627_776, None);
        assert_eq!(args[0], "set");
        assert!(
            args.iter().any(|a| a.starts_with("quota=")),
            "应含 quota= 属性: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "tank/media"),
            "应含 dataset 名: {args:?}"
        );
        // 无 refreservation 时不出现
        assert!(!args.iter().any(|a| a.starts_with("refreservation=")));
    }

    #[test]
    fn build_quota_cmd_with_refreservation() {
        let args = build_quota_cmd("tank/data", 500_000_000_000, Some(100_000_000_000));
        assert!(args.iter().any(|a| a == "quota=500000000000"));
        assert!(args.iter().any(|a| a == "refreservation=100000000000"));
        assert!(args.iter().any(|a| a == "tank/data"));
    }

    // —— handle 层：scrub / quota 不 panic（真实命令不可用时降级）——

    fn handler_real() -> StorageRouteHandler {
        // 用真实 ZfsCliBackend（routes 声明不需要 fixture runner）。
        // scrub/quota 不经过 backend，直接 spawn 命令——测试环境大概率无 sudo/zpool，
        // 应降级为 {ok:false,...}，绝不 panic。
        StorageRouteHandler::new(Arc::new(ZfsCliBackend::new()))
    }

    #[tokio::test]
    async fn scrub_start_does_not_panic() {
        let h = handler_real();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/pools/testpool/scrub".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.expect("scrub 启动应返回响应不 panic");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["action"], "scrub");
        assert_eq!(resp.body["pool"], "testpool");
        assert!(resp.body["ok"].is_boolean());
    }

    #[tokio::test]
    async fn scrub_status_does_not_panic() {
        let h = handler_real();
        let req = ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/pools/testpool/scrub-status".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h
            .handle(req)
            .await
            .expect("scrub-status 应返回响应不 panic");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["pool"], "testpool");
        assert!(resp.body["status"].is_string());
    }

    #[tokio::test]
    async fn set_quota_does_not_panic() {
        let h = handler_real();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/datasets/tank/media/quota".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({"quota_bytes": 1099511627776_u64}),
            auth: None,
        };
        let resp = h.handle(req).await.expect("set-quota 应返回响应不 panic");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["action"], "set-quota");
        assert_eq!(resp.body["dataset"], "tank/media");
        assert!(resp.body["ok"].is_boolean());
    }

    #[tokio::test]
    async fn set_quota_invalid_body_returns_err() {
        let h = handler_real();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/datasets/tank/media/quota".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({}), // 缺 quota_bytes
            auth: None,
        };
        let err = h.handle(req).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn set_quota_dataset_with_slash_in_path() {
        // dataset 名含 '/'（tank/media）——验证路径剥离正确提取
        let h = handler_real();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/datasets/tank/sub/deep/quota".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({"quota_bytes": 100_u64, "refreservation_bytes": 50_u64}),
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["dataset"], "tank/sub/deep");
    }

    // ========================================================================
    // 已建池识别与导入（2026-08-30）—— zpool import 解析 / 导入端点 / disks 富化
    // ========================================================================

    /// 复刻真实 `zpool import`（无参列表）输出：nvme（ONLINE，整盘 nvme1n1）
    /// + tank2（DEGRADED，缺成员），中间以空行分段。
    fn import_sample() -> String {
        "\
   pool: nvme
     id: 14098283232656026497
  state: ONLINE
 status: The pool was last accessed by another system.
 action: The pool can be imported using its name or numeric identifier.
 config:

\tnvme        ONLINE
\t  nvme1n1   ONLINE

   pool: tank2
     id: 99887766554433221100
  state: DEGRADED
 status: One or more devices are missing.
 action: The pool can be imported despite missing or damaged devices.
 config:

\ttank2       DEGRADED
\t  sda       ONLINE
\t  sdb       UNAVAIL

   pool: byid
     id: 11223344556677889900
  state: ONLINE
 action: The pool can be imported using its name or numeric identifier.
 config:

\tbyid                    ONLINE
\t  /dev/disk/by-id/ata-WDC_nvme1n1p1 ONLINE
"
        .to_string()
    }

    /// import 探测/导入端点专用 fixture：固定 (program, args[0]="import") 输出，
    /// 并记录全部调用参数（验证「非法名不执行命令」「探测不真导入」等红线）。
    struct FixedCmdRunner {
        output: CommandOutput,
        /// true = run 返回 Err（模拟进程启动失败，如 zpool 缺失）。
        spawn_err: bool,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl FixedCmdRunner {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FixedCmdRunner {
        async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            if self.spawn_err {
                return Err(StorageError::CommandFailed("模拟 zpool 启动失败".into()));
            }
            Ok(self.output.clone())
        }
    }

    /// 构造 handler：探测/导入走 FixedCmdRunner，backend 的 zpool list 走独立 fixture。
    /// 返回 handler 与 FixedCmdRunner 的 Arc 引用（供断言调用记录）。
    fn handler_with_fixed_cmd(
        cmd: FixedCmdRunner,
        backend_list_stdout: String,
    ) -> (StorageRouteHandler, Arc<FixedCmdRunner>) {
        let cmd = Arc::new(cmd);
        let backend = Arc::new(ZfsCliBackend::with_runner(Box::new(
            FixtureRunner::new().on(
                "zpool",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: backend_list_stdout,
                    stderr: String::new(),
                },
            ),
        )));
        let h = StorageRouteHandler::with_cmd_runner(backend, cmd.clone());
        (h, cmd)
    }

    fn fixed_runner(output: CommandOutput, spawn_err: bool) -> FixedCmdRunner {
        FixedCmdRunner {
            output,
            spawn_err,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn get(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn post_json(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    // —— zpool import 输出解析（纯函数）——

    #[test]
    fn parse_importable_pools_multi_single_empty() {
        // 多池：nvme + tank2（DEGRADED）+ byid；name/id/state 逐字段提取
        let pools = parse_importable_pools(&import_sample());
        assert_eq!(pools.len(), 3, "样本含 3 个可导入池: {pools:?}");
        assert_eq!(pools[0].name, "nvme");
        assert_eq!(pools[0].id, "14098283232656026497");
        assert_eq!(pools[0].state, "ONLINE");
        // raw 保留整段原文（含 pool: 行与 config 盘列表）
        assert!(pools[0].raw.contains("pool: nvme"), "raw 应含池名行");
        assert!(pools[0].raw.contains("nvme1n1"), "raw 应含成员盘");
        assert_eq!(pools[1].name, "tank2");
        assert_eq!(pools[1].state, "DEGRADED");
        assert_eq!(pools[1].id, "99887766554433221100");

        // 单池：只取第一段
        let single = "  pool: solo\n    id: 42\n state: ONLINE\n";
        let pools = parse_importable_pools(single);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].name, "solo");
        assert_eq!(pools[0].id, "42");
        assert_eq!(pools[0].state, "ONLINE");

        // 空 / 无池段 / 缺 id 的段 → 空列表（无法稳定标识即不报）
        assert!(parse_importable_pools("").is_empty());
        assert!(parse_importable_pools("no pools available\n").is_empty());
        let no_id = "  pool: ghost\n state: ONLINE\n";
        assert!(parse_importable_pools(no_id).is_empty());
        // 乱码不 panic
        assert!(parse_importable_pools("\0\x01garbage [[[").is_empty());
    }

    #[test]
    fn importable_pool_disk_map_matches_config_members() {
        let pools = parse_importable_pools(&import_sample());
        let map = importable_pool_disk_map(&pools);
        // nvme 池：整盘裸名命中（106 事故场景 nvme1n1 → nvme 池）
        assert_eq!(map.get("nvme1n1").map(String::as_str), Some("nvme"));
        // tank2 池：sda/sdb 命中
        assert_eq!(map.get("sda").map(String::as_str), Some("tank2"));
        assert_eq!(map.get("sdb").map(String::as_str), Some("tank2"));
        // by-id 路径取末段 + 分区后缀归一化到整盘
        assert_eq!(
            map.get("nvme1n1p1").map(String::as_str),
            None,
            "归一化后不存分区名"
        );
        assert_eq!(
            map.get("ata-WDC_nvme1n1p1").map(String::as_str),
            None,
            "by-id 合成名不是内核盘名，不收录"
        );
        // 非 config 段文本不产生映射
        assert_eq!(map.len(), 3, "只有 nvme1n1/sda/sdb 三个成员");
        // 空输入 → 空 map
        assert!(importable_pool_disk_map(&[]).is_empty());
    }

    #[test]
    fn active_pool_disk_map_parses_zpool_status() {
        // 复刻真实 zpool status：tank = mirror(sda,sdb)，solo = 单盘 nvme0n1。
        // cache/log 行不是数据盘根——被 parse_zpool_status 按数据行折叠，这里只断言
        // 数据盘归属（cache 盘若出现也归属同池，不破坏「永不提示初始化」语义）。
        let status = "\
  pool: tank
 state: ONLINE
  scan: scrub repaired 0B in 00:01:23 with 0 errors on Wed Jun 14 12:01:23 2023
config:

\tNAME        STATE     READ WRITE CKSUM
\ttank        ONLINE       0     0     0
\t  mirror-0  ONLINE       0     0     0
\t    sda     ONLINE       0     0     0
\t    sdb     ONLINE       0     0     0
errors: No known data errors

  pool: solo
 state: ONLINE
  scan: none
config:

\tNAME        STATE     READ WRITE CKSUM
\tsolo        ONLINE       0     0     0
\t  nvme0n1   ONLINE       0     0     0
errors: No known data errors
";
        let map = active_pool_disk_map(status);
        assert_eq!(map.get("sda").map(String::as_str), Some("tank"));
        assert_eq!(map.get("sdb").map(String::as_str), Some("tank"));
        assert_eq!(map.get("nvme0n1").map(String::as_str), Some("solo"));
        // /dev/ 前缀形式同样命中（zpool status 可能带路径）
        let with_path = "\
  pool: p
 state: ONLINE
config:

\tp        ONLINE       0     0     0
\t  /dev/sdz ONLINE       0     0     0
";
        let map = active_pool_disk_map(with_path);
        assert_eq!(map.get("sdz").map(String::as_str), Some("p"));
        // 空输出 / 乱码 → 空 map
        assert!(active_pool_disk_map("").is_empty());
        assert!(active_pool_disk_map("not a status output").is_empty());
    }

    // —— GET /api/v1/disks/importable（handle 层）——

    #[tokio::test]
    async fn importable_endpoint_returns_parsed_pools() {
        let (h, cmd) = handler_with_fixed_cmd(
            fixed_runner(
                CommandOutput {
                    exit_code: 0,
                    stdout: import_sample(),
                    stderr: String::new(),
                },
                false,
            ),
            String::new(),
        );
        let resp = h.handle(get("/api/v1/disks/importable")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body["importable"]
            .as_array()
            .expect("importable 应为数组");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["name"], "nvme");
        assert_eq!(arr[0]["id"], "14098283232656026497");
        assert_eq!(arr[0]["state"], "ONLINE");
        assert!(arr[0]["raw"].is_string(), "raw 应为原文文本");
        // 红线：探测只跑无参 `zpool import`（列表），绝不带池名（真导入）
        assert_eq!(cmd.calls(), vec!["zpool import".to_string()]);
    }

    #[tokio::test]
    async fn importable_endpoint_degrades_to_empty_on_failure() {
        // zpool 非零退出（无可导入池时部分版本亦非零）→ 空数组
        let (h, _cmd) = handler_with_fixed_cmd(
            fixed_runner(
                CommandOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "no pools available".into(),
                },
                false,
            ),
            String::new(),
        );
        let resp = h.handle(get("/api/v1/disks/importable")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["importable"].as_array().unwrap().is_empty());

        // 进程启动失败（zpool 缺失 / sudo 拒绝）→ 同样空数组降级，不报错
        let (h, _) = handler_with_fixed_cmd(fixed_runner(CommandOutput::ok(), true), String::new());
        let resp = h.handle(get("/api/v1/disks/importable")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["importable"].as_array().unwrap().is_empty());
    }

    // —— POST /api/v1/disks/import（handle 层）——

    #[tokio::test]
    async fn import_endpoint_success_returns_new_pool() {
        // import exit 0 + backend zpool list 返回新池 → 200 {ok, pool}
        let (h, cmd) = handler_with_fixed_cmd(
            fixed_runner(CommandOutput::ok(), false),
            pool_list_line("nvme"),
        );
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "nvme" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["pool"]["name"], "nvme");
        assert_eq!(
            resp.body["pool"]["capacity"]["total_bytes"],
            10_995_116_277_760_u64
        );
        // 执行的就是 `zpool import nvme`
        assert_eq!(cmd.calls(), vec!["zpool import nvme".to_string()]);
    }

    #[tokio::test]
    async fn import_endpoint_failure_carries_stderr_and_status() {
        // 池名冲突（已存在同名池）→ 409，错误体带 zpool 原始 stderr
        let (h, _) = handler_with_fixed_cmd(
            fixed_runner(
                CommandOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "cannot import 'nvme': a pool with that name already exists".into(),
                },
                false,
            ),
            String::new(),
        );
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "nvme" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "池名冲突应 409: {}", resp.body);
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("already exists"), "应携带原始 stderr: {err}");

        // 设备缺失等一般失败 → 400
        let (h, _) = handler_with_fixed_cmd(
            fixed_runner(
                CommandOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "cannot import 'nvme': one or more devices is currently unavailable"
                        .into(),
                },
                false,
            ),
            String::new(),
        );
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "nvme" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("unavailable"));

        // 进程启动失败 → 500（非权限/非冲突）
        let (h, _) = handler_with_fixed_cmd(fixed_runner(CommandOutput::ok(), true), String::new());
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "nvme" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 500);
    }

    #[tokio::test]
    async fn import_endpoint_rejects_invalid_name_before_running() {
        let (h, cmd) =
            handler_with_fixed_cmd(fixed_runner(CommandOutput::ok(), false), String::new());
        for bad in ["", "-f", "../../etc", "a/b", ".", "..", "含中文"] {
            let resp = h
                .handle(post_json(
                    "/api/v1/disks/import",
                    serde_json::json!({ "name": bad }),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{bad:?} 应 400");
            assert!(resp.body["error"].as_str().unwrap().contains("非法池名"));
        }
        // 红线：非法名在任何命令执行前短路——零次调用
        assert!(
            cmd.calls().is_empty(),
            "不应执行任何命令: {:?}",
            cmd.calls()
        );

        // 合法名（含 ZFS 允许的字符）通过校验并执行
        let (h, cmd) =
            handler_with_fixed_cmd(fixed_runner(CommandOutput::ok(), false), String::new());
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "nvme-1.2:3_4" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(cmd.calls(), vec!["zpool import nvme-1.2:3_4".to_string()]);
    }

    #[tokio::test]
    async fn importable_and_import_routes_declared() {
        let h = handler_with(FixtureRunner::new());
        let routes = h.routes().await;
        // 探测端点：只读公开
        let imp = routes
            .iter()
            .find(|r| r.method == HttpMethod::Get && r.path == "/api/v1/disks/importable")
            .expect("应声明 importable 路由");
        assert!(!imp.requires_auth);
        assert!(imp.required_roles.is_empty());
        // 导入端点：写操作，必须 admin
        let post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/disks/import")
            .expect("应声明 import 路由");
        assert!(post.requires_auth);
        assert_eq!(post.required_roles, vec!["admin".to_string()]);
        // 未匹配的 import 子路径仍走兜底 404
        let resp = h.handle(get("/api/v1/disks/importxyz")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    // —— disks 富化（纯函数）：可导入池成员 / 活跃池成员 / 无主残留盘 ——

    /// 造一个 detect_disks 形状的 DiskInfo（sigs 非空即视为 has_partitions）。
    fn disk(name: &str, sigs: &[&str]) -> DiskInfo {
        DiskInfo {
            name: name.to_string(),
            size_bytes: 0,
            model: String::new(),
            available: true,
            in_use: false,
            has_partitions: !sigs.is_empty(),
            signatures: sigs.iter().map(|s| (*s).to_string()).collect(),
            member_of: None,
            zfs_pool_hint: None,
        }
    }

    #[test]
    fn enrich_disks_marks_importable_pool_member() {
        // 106 事故场景：nvme1n1 有 zfs_member+gpt 签名，属于可导入池 nvme →
        // zfs_pool_hint = nvme，永不提示初始化（has_partitions 置 false）
        let disks = vec![
            disk("/dev/nvme1n1", &["zfs_member", "gpt"]),
            disk("/dev/sdz", &["zfs_member"]), // 无主 zfs 签名：维持需初始化
            disk("/dev/sdc", &[]),             // 裸盘：不变
        ];
        let mut importable = HashMap::new();
        importable.insert("nvme1n1".to_string(), "nvme".to_string());
        let active = HashMap::new();

        let out = enrich_disks_with_zfs(disks, &importable, &active);
        // 可导入池成员：蓝标 + 可导入 + 不再「需初始化」
        assert_eq!(out[0].zfs_pool_hint.as_deref(), Some("nvme"));
        assert!(out[0].member_of.is_none());
        assert!(!out[0].has_partitions, "可导入池成员永不提示初始化");
        assert!(!out[0].in_use);
        // 无主 zfs_member 残留盘：维持原「需初始化」流程
        assert!(out[1].zfs_pool_hint.is_none());
        assert!(out[1].has_partitions, "无主残留盘仍需初始化");
        // 裸盘不受影响
        assert!(!out[2].has_partitions);
        assert!(out[2].zfs_pool_hint.is_none());
    }

    #[test]
    fn enrich_disks_marks_active_pool_member_never_init() {
        // 活跃池成员（tank 的 sda/sdb）：member_of = tank，永不提示初始化，
        // in_use = true——即使签名含 zfs_member、且不在可导入列表（池已导入）
        let disks = vec![
            disk("/dev/sda", &["zfs_member"]),
            disk("/dev/sdb", &["gpt", "zfs_member"]),
        ];
        let importable = HashMap::new();
        let mut active = HashMap::new();
        active.insert("sda".to_string(), "tank".to_string());
        active.insert("sdb".to_string(), "tank".to_string());

        let out = enrich_disks_with_zfs(disks, &importable, &active);
        for d in &out {
            assert_eq!(d.member_of.as_deref(), Some("tank"));
            assert!(!d.has_partitions, "活跃池成员永不提示初始化: {}", d.name);
            assert!(d.in_use, "活跃池成员应标 in_use: {}", d.name);
        }
    }

    #[test]
    fn enrich_disks_system_disk_never_in_list() {
        // 系统盘白名单：挂 / 的盘在 detect_disks 阶段就被过滤（is_system_mount），
        // enrich 层只负责「进了列表的盘」——这里复刻过滤链验证系统盘不产出。
        let json = r#"{
            "blockdevices": [
                {"name":"/dev/nvme0n1","size":500,"type":"disk","mountpoint":null,"model":"SAMSUNG",
                 "children":[
                    {"name":"/dev/nvme0n1p2","size":400,"type":"part","mountpoint":"/","model":null,
                     "fstype":"zfs_member","children":[]}
                 ]},
                {"name":"/dev/sdz","size":100,"type":"disk","mountpoint":null,"model":"ORPHAN",
                 "fstype":"zfs_member","children":[]}
            ]
        }"#;
        let root: LsblkRoot = serde_json::from_str(json).unwrap();
        // ZFS 根池系统盘（fstype=zfs_member + 挂 /）→ is_system_mount 过滤
        let kept: Vec<&LsblkDevice> = root
            .blockdevices
            .iter()
            .filter(|d| d.kind == "disk")
            .filter(|d| !d.is_system_mount())
            .collect();
        assert_eq!(kept.len(), 1, "系统盘不进列表");
        assert_eq!(kept[0].name, "/dev/sdz");

        // 即便系统盘因挂载探测误差漏进列表，富化层也不会引导初始化 ZFS 系统盘：
        // ZFS 根池是活跃池（zpool status 可见）→ member_of 命中 → has_partitions=false
        let mut active = HashMap::new();
        active.insert("nvme0n1".to_string(), "rpool".to_string());
        let out = enrich_disks_with_zfs(
            vec![disk("/dev/nvme0n1", &["zfs_member", "gpt"])],
            &HashMap::new(),
            &active,
        );
        assert_eq!(out[0].member_of.as_deref(), Some("rpool"));
        assert!(
            !out[0].has_partitions,
            "ZFS 系统盘（活跃根池成员）永不提示初始化"
        );
    }

    // —— valid_pool_name / import_error_status（纯函数）——

    #[test]
    fn valid_pool_name_accepts_zfs_charset_only() {
        for ok in ["nvme", "tank", "pool-1", "a.b", "x:y", "z_9", "my pool"] {
            assert!(valid_pool_name(ok), "{ok:?} 应合法");
        }
        for bad in [
            "",               // 空
            "-f",             // flag 注入
            "--dry-run",      // flag 注入
            ".",              // 保留名
            "..",             // 保留名
            "a/b",            // 非法字符
            "池",             // 非 ASCII
            "a\tb",           // 控制字符
            &"x".repeat(129), // 超长
        ] {
            assert!(!valid_pool_name(bad), "{bad:?} 应拒绝");
        }
    }

    #[test]
    fn import_error_status_classifies_stderr() {
        // 权限 → 401
        assert_eq!(import_error_status("sudo: a password is required"), 401);
        // 池名冲突 / 已导入 → 409
        assert_eq!(
            import_error_status("cannot import 'x': a pool with that name already exists"),
            409
        );
        assert_eq!(
            import_error_status("cannot import 'x': pool already imported"),
            409
        );
        // 其余（设备缺失等）→ 400
        assert_eq!(
            import_error_status("cannot import 'x': one or more devices unavailable"),
            400
        );
        assert_eq!(import_error_status(""), 400);
    }

    // ========================================================================
    // 池删除与删除后盘处置（2026-08-30）—— export（保留标签）/ destroy+wipefs
    // ========================================================================

    /// 复刻真实 `zpool status tank` 输出：mirror(sda, sdb) + 独立 cache 盘 nvme0n1。
    fn pool_status_sample() -> String {
        "\
  pool: tank
 state: ONLINE
config:

\tNAME        STATE     READ WRITE CKSUM
\ttank        ONLINE       0     0     0
\t  mirror-0  ONLINE       0     0     0
\t    sda     ONLINE       0     0     0
\t    sdb     ONLINE       0     0     0
\tcache
\t  nvme0n1   ONLINE       0     0     0
errors: No known data errors
"
        .to_string()
    }

    /// 删池端点专用 runner：按 (program, args[0]) 分发 fixture（zpool status /
    /// export / destroy / sudo wipefs 各自预设），并**记录全部调用**——
    /// 断言「成员抓取在删池前」「wipe 逐盘恰好一次」「非法名零命令」等红线。
    struct RecordingFixtureRunner {
        fixtures: std::sync::Mutex<Vec<FixtureEntry>>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingFixtureRunner {
        fn new() -> Self {
            Self {
                fixtures: std::sync::Mutex::new(Vec::new()),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// 注册一条 fixture：当 `<program> <subcmd> ...` 被调用时返回 `output`。
        fn on(
            mut self,
            program: &'static str,
            subcmd: &'static str,
            output: CommandOutput,
        ) -> Self {
            self.fixtures.get_mut().unwrap().push(FixtureEntry {
                program,
                subcmd,
                output,
            });
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for RecordingFixtureRunner {
        async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            let subcmd = args.first().map(String::as_str);
            let fixtures = self.fixtures.lock().unwrap();
            for f in fixtures.iter() {
                if f.program == program && subcmd == Some(f.subcmd) {
                    return Ok(f.output.clone());
                }
            }
            Err(StorageError::CommandFailed(format!(
                "RecordingFixtureRunner 无匹配 fixture: {program} {:?}",
                args.join(" ")
            )))
        }
    }

    /// 构造删池测试 handler：命令走 RecordingFixtureRunner（可断言调用顺序），
    /// backend 的 zpool list 走独立 fixture（删池端点不触达 backend，兜底用）。
    fn handler_with_recording(
        runner: RecordingFixtureRunner,
    ) -> (StorageRouteHandler, Arc<RecordingFixtureRunner>) {
        let runner = Arc::new(runner);
        let backend = Arc::new(ZfsCliBackend::with_runner(Box::new(
            FixtureRunner::new().on(
                "zpool",
                "list",
                CommandOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ),
        )));
        let h = StorageRouteHandler::with_cmd_runner(backend, runner.clone());
        (h, runner)
    }

    fn delete_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    /// 成功出口的常用 fixture：status 返回 tank 布局，export/destroy/wipefs 全 ok。
    fn delete_ok_fixtures() -> RecordingFixtureRunner {
        RecordingFixtureRunner::new()
            .on(
                "zpool",
                "status",
                CommandOutput {
                    exit_code: 0,
                    stdout: pool_status_sample(),
                    stderr: String::new(),
                },
            )
            .on("zpool", "export", CommandOutput::ok())
            .on("zpool", "destroy", CommandOutput::ok())
            .on("sudo", "wipefs", CommandOutput::ok())
    }

    // —— 默认（wipe=false）：zpool export，保留 ZFS 标签可再导入 ——

    #[tokio::test]
    async fn delete_pool_default_exports_and_captures_members_first() {
        let (h, cmd) = handler_with_recording(delete_ok_fixtures());
        let resp = h
            .handle(delete_req("/api/v1/pools/tank", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["action"], "export", "默认走 export（保留标签）");
        assert_eq!(resp.body["destroyed"], "tank");
        assert_eq!(resp.body["wipe"], false);
        // 成员盘从删池前的 zpool status 抓到（mirror 双盘 + cache 盘）
        assert_eq!(
            resp.body["members"],
            serde_json::json!(["sda", "sdb", "nvme0n1"]),
            "members 应为 status 解析的成员盘: {}",
            resp.body
        );
        assert_eq!(resp.body["wiped_disks"], serde_json::json!([]));
        // 红线：成员抓取必须发生在 export 之前（顺序断言），且不跑任何 wipefs
        assert_eq!(
            cmd.calls(),
            vec![
                "zpool status tank".to_string(),
                "zpool export tank".to_string()
            ],
            "调用顺序应为 status → export"
        );
    }

    // —— wipe=true：zpool destroy + 逐盘 wipefs -a ——

    #[tokio::test]
    async fn delete_pool_wipe_destroys_then_wipes_each_member_once() {
        // query 形式 ?wipe=1
        let (h, cmd) = handler_with_recording(delete_ok_fixtures());
        let resp = h
            .handle(delete_req(
                "/api/v1/pools/tank?wipe=1",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["action"], "destroy");
        assert_eq!(resp.body["wipe"], true);
        assert_eq!(
            resp.body["wiped_disks"],
            serde_json::json!(["sda", "sdb", "nvme0n1"]),
            "每个成员盘都应被 wipefs 一次: {}",
            resp.body
        );
        assert_eq!(
            resp.body["members"],
            serde_json::json!(["sda", "sdb", "nvme0n1"])
        );
        // 顺序：status（先抓成员）→ destroy → 逐盘 wipefs（每个成员恰好一次）
        assert_eq!(
            cmd.calls(),
            vec![
                "zpool status tank".to_string(),
                "zpool destroy tank".to_string(),
                "sudo wipefs -a /dev/sda".to_string(),
                "sudo wipefs -a /dev/sdb".to_string(),
                "sudo wipefs -a /dev/nvme0n1".to_string(),
            ],
            "调用顺序应为 status → destroy → 逐盘 wipefs"
        );

        // body 形式 {wipe: true} 等价（DELETE 体部分中间层会丢，query 优先，此处验证兜底）
        let (h, cmd) = handler_with_recording(delete_ok_fixtures());
        let resp = h
            .handle(delete_req(
                "/api/v1/pools/tank",
                serde_json::json!({ "wipe": true }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["action"], "destroy");
        assert!(
            cmd.calls().contains(&"sudo wipefs -a /dev/sda".to_string()),
            "body wipe=true 也应逐盘 wipefs: {:?}",
            cmd.calls()
        );
    }

    // —— 非法池名：在任何命令执行前 400（零命令）——

    #[tokio::test]
    async fn delete_pool_invalid_name_runs_no_commands() {
        let (h, cmd) = handler_with_recording(delete_ok_fixtures());
        for bad in [
            "",
            "-f",
            "../../etc",
            "a/b",
            ".",
            "..",
            "含中文",
            "tank/scrub",
        ] {
            let path = format!("/api/v1/pools/{bad}");
            let resp = h
                .handle(delete_req(&path, serde_json::Value::Null))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{bad:?} 应 400");
            assert!(resp.body["error"].as_str().unwrap().contains("非法池名"));
        }
        // 红线：非法名零命令执行
        assert!(
            cmd.calls().is_empty(),
            "不应执行任何命令: {:?}",
            cmd.calls()
        );
    }

    // —— export/destroy 失败：错误透传（busy→409、权限→401、其余 400 带 stderr）——

    #[tokio::test]
    async fn delete_pool_failure_carries_stderr_and_status() {
        // 数据集在用（busy）→ 409，错误体带 zpool 原始 stderr
        let runner = RecordingFixtureRunner::new()
            .on(
                "zpool",
                "status",
                CommandOutput {
                    exit_code: 0,
                    stdout: pool_status_sample(),
                    stderr: String::new(),
                },
            )
            .on(
                "zpool",
                "export",
                CommandOutput::fail(1, "cannot export 'tank': pool is busy; datasets are in use"),
            );
        let (h, cmd) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req("/api/v1/pools/tank", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "busy 应 409: {}", resp.body);
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("pool is busy"),
            "应携带原始 stderr: {}",
            resp.body
        );
        // 失败发生在 export 阶段——没有 wipefs
        assert!(!cmd.calls().iter().any(|c| c.contains("wipefs")));

        // 一般失败（如 pool 内部错误）→ 400 带 stderr
        let runner = RecordingFixtureRunner::new()
            .on(
                "zpool",
                "status",
                CommandOutput {
                    exit_code: 0,
                    stdout: pool_status_sample(),
                    stderr: String::new(),
                },
            )
            .on(
                "zpool",
                "export",
                CommandOutput::fail(1, "cannot export 'tank': internal error"),
            );
        let (h, _) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req("/api/v1/pools/tank", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("internal error"));

        // sudo 免密未配置 → 401
        let runner = RecordingFixtureRunner::new()
            .on(
                "zpool",
                "status",
                CommandOutput {
                    exit_code: 0,
                    stdout: pool_status_sample(),
                    stderr: String::new(),
                },
            )
            .on(
                "zpool",
                "export",
                CommandOutput::fail(1, "sudo: a password is required"),
            );
        let (h, _) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req("/api/v1/pools/tank", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 401);
    }

    // —— 池不存在：status 报 no such pool → 404，且不执行任何删除命令 ——

    #[tokio::test]
    async fn delete_pool_missing_pool_returns_404_without_deleting() {
        let runner = RecordingFixtureRunner::new().on(
            "zpool",
            "status",
            CommandOutput::fail(1, "cannot open 'ghost': no such pool"),
        );
        let (h, cmd) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req("/api/v1/pools/ghost", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "池不存在应 404: {}", resp.body);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("no such pool"));
        // 红线：不存在即 404 短路——绝不执行 export/destroy
        assert_eq!(cmd.calls(), vec!["zpool status ghost".to_string()]);
    }

    // —— 成员盘探测失败：wipe=true 中止（不盲擦）；wipe=false 降级 + warning ——

    #[tokio::test]
    async fn delete_pool_member_probe_failure_semantics() {
        let runner = RecordingFixtureRunner::new().on(
            "zpool",
            "status",
            CommandOutput::fail(2, "cannot open 'tank': permission denied"),
        );
        // wipe=true：拿不到成员盘 → 中止，一条删除命令都不跑
        let (h, cmd) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req(
                "/api/v1/pools/tank?wipe=1",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "权限类 stderr 应 401: {}", resp.body);
        assert_eq!(
            cmd.calls(),
            vec!["zpool status tank".to_string()],
            "探测失败 + wipe 应中止，不执行 destroy/wipefs"
        );

        // wipe=false：export 不依赖成员列表 → 降级 members=[] 成功 + warning
        let runner = RecordingFixtureRunner::new()
            .on(
                "zpool",
                "status",
                CommandOutput::fail(2, "cannot open 'tank': some error"),
            )
            .on("zpool", "export", CommandOutput::ok());
        let (h, cmd) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req("/api/v1/pools/tank", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "export 降级应成功: {}", resp.body);
        assert_eq!(resp.body["members"], serde_json::json!([]));
        assert!(
            resp.body["warning"]
                .as_str()
                .unwrap()
                .contains("成员盘探测失败"),
            "降级响应应带 warning: {}",
            resp.body
        );
        assert_eq!(
            cmd.calls(),
            vec![
                "zpool status tank".to_string(),
                "zpool export tank".to_string()
            ]
        );
    }

    // —— 鉴权矩阵（路由声明层）：DELETE /api/v1/pools/:name 必须 admin ——

    #[tokio::test]
    async fn delete_pool_route_requires_admin() {
        let h = handler_with(FixtureRunner::new());
        let routes = h.routes().await;
        assert!(routes.iter().all(|r| r.handler_component == "storage"));
        let del = routes
            .iter()
            .find(|r| r.method == HttpMethod::Delete && r.path == "/api/v1/pools/:name")
            .expect("应声明 DELETE /api/v1/pools/:name 路由");
        assert!(del.requires_auth, "删池高危操作必须登录");
        assert_eq!(
            del.required_roles,
            vec!["admin".to_string()],
            "删池仅 admin"
        );
        // 无名形式（DELETE /api/v1/pools）不匹配动态路由，仍走兜底 404
        let resp = h
            .handle(delete_req("/api/v1/pools", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // —— wipefs 部分失败：池已删、逐盘如实上报（wiped_disks + wipe_errors + warning）——

    #[tokio::test]
    async fn delete_pool_wipe_partial_failure_reports_per_disk() {
        // wipefs 全部成员都失败（sudo wipefs 免密未配置）→ ok=true 但带 wipe_errors
        let runner = RecordingFixtureRunner::new()
            .on(
                "zpool",
                "status",
                CommandOutput {
                    exit_code: 0,
                    stdout: pool_status_sample(),
                    stderr: String::new(),
                },
            )
            .on("zpool", "destroy", CommandOutput::ok())
            .on(
                "sudo",
                "wipefs",
                CommandOutput::fail(1, "sudo: a password is required"),
            );
        let (h, cmd) = handler_with_recording(runner);
        let resp = h
            .handle(delete_req(
                "/api/v1/pools/tank?wipe=true",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "池本身已删成功: {}", resp.body);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["wiped_disks"], serde_json::json!([]));
        let errs = resp.body["wipe_errors"]
            .as_array()
            .expect("应带 wipe_errors");
        assert_eq!(errs.len(), 3, "三块成员盘逐盘上报: {errs:?}");
        assert!(errs[0]["disk"].is_string());
        assert!(
            errs[0]["error"].as_str().unwrap().contains("password"),
            "单盘错误应带原始 stderr: {errs:?}"
        );
        assert!(
            resp.body["warning"].as_str().unwrap().contains("擦除失败"),
            "应带 warning 汇总: {}",
            resp.body
        );
        // 逐盘尝试了三块（单盘失败不中断其余盘）
        assert_eq!(
            cmd.calls()
                .iter()
                .filter(|c| c.starts_with("sudo wipefs"))
                .count(),
            3
        );
    }

    // —— pool_member_disks（纯函数）——

    #[test]
    fn pool_member_disks_parses_status_members() {
        // mirror 双盘 + cache 盘 → 保序去重的整盘裸名（cache 顶层盘也不漏——
        // wipe 目标列表必须穷尽全部成员）
        assert_eq!(
            pool_member_disks(&pool_status_sample(), "tank"),
            vec!["sda".to_string(), "sdb".to_string(), "nvme0n1".to_string()]
        );
        // 池根行（首 token == 池名）必须跳过：名为 nvme 的池根行 "nvme ONLINE"
        // 不产生假目标 /dev/nvme（用户真实存在的池名场景）
        let nvme_pool = "\
  pool: nvme
 state: ONLINE
config:

\tNAME        STATE     READ WRITE CKSUM
\tnvme        ONLINE       0     0     0
\t  nvme1n1   ONLINE       0     0     0
errors: No known data errors
";
        assert_eq!(
            pool_member_disks(nvme_pool, "nvme"),
            vec!["nvme1n1".to_string()],
            "池根行 nvme 应跳过，只留真成员 nvme1n1"
        );
        // /dev/ 前缀取末段；分区名归一化整盘；by-id 合成名不收录（与既有
        // importable_pool_disk_map / active_pool_disk_map 同一约定：合成名无法
        // 稳定映射回内核盘名，宁缺勿错）
        let with_paths = "\
  pool: p
 state: ONLINE
config:

\tNAME          STATE     READ WRITE CKSUM
\tp            ONLINE       0     0     0
\t  /dev/sdz   ONLINE       0     0     0
\t  sdd1       ONLINE       0     0     0
\t  /dev/disk/by-id/ata-WDC_sde1 ONLINE 0 0 0
";
        let members = pool_member_disks(with_paths, "p");
        assert_eq!(
            members,
            vec!["sdz".to_string(), "sdd".to_string()],
            "含 /dev/ 前缀归一化 + sdd1 分区归一化整盘；by-id 合成名不收录"
        );
        // 空输出 / 乱码 / 无 config 段 → 空
        assert!(pool_member_disks("", "tank").is_empty());
        assert!(pool_member_disks("not a status output", "tank").is_empty());
    }

    // —— pool_delete_error_status（纯函数）——

    #[test]
    fn pool_delete_error_status_classifies_stderr() {
        // 权限 → 401
        assert_eq!(
            pool_delete_error_status("sudo: a password is required"),
            401
        );
        // 池不存在 → 404
        assert_eq!(
            pool_delete_error_status("cannot destroy 'x': no such pool"),
            404
        );
        // busy / 在用 / 挂载中 → 409（export 的典型拒绝原因）
        assert_eq!(
            pool_delete_error_status("cannot export 'x': pool is busy"),
            409
        );
        assert_eq!(
            pool_delete_error_status("cannot export 'x': dataset is in use"),
            409
        );
        assert_eq!(
            pool_delete_error_status("cannot destroy 'x': filesystem is currently mounted"),
            409
        );
        // 其余 → 400
        assert_eq!(
            pool_delete_error_status("cannot destroy 'x': unhandled error"),
            400
        );
        assert_eq!(pool_delete_error_status(""), 400);
    }

    // ========================================================================
    // 无 ZFS 节点优雅降级（2026-09-02）—— 探测 / 空态契约 / 写操作 400 / 错误分级
    // ========================================================================

    // —— 纯函数：探测逻辑（env 强制 + PATH 查找）——

    /// 造一个只含 `names` 这些可执行文件的临时目录（返回其 OsString 形式的 PATH 值）。
    /// `executable=false` 时文件无执行位（验证 which 语义只认可执行文件）。
    fn temp_bin_dir(tag: &str, names: &[&str], executable: bool) -> OsString {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "nexos-storage-zfs-probe-{}-{tag}-{}",
            std::process::id(),
            names.len()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        for n in names {
            let f = dir.join(n);
            std::fs::write(&f, "#!/bin/sh\n").expect("写占位可执行文件");
            let perm = std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 });
            std::fs::set_permissions(&f, perm).expect("设置执行位");
        }
        dir.into_os_string()
    }

    #[test]
    fn zfs_probe_requires_both_binaries_in_path() {
        // zpool + zfs 齐备且可执行 → 可用
        let both = temp_bin_dir("both", &["zpool", "zfs"], true);
        assert!(zfs_probe_from(Some(both.clone()), None));
        // gate=1（显式启用）不改变 PATH 判定
        assert!(zfs_probe_from(Some(both), Some("1".to_string())));

        // 只有 zpool 没有 zfs → 不可用（数据集/快照命令走 zfs 二进制）
        let only_zpool = temp_bin_dir("zpool-only", &["zpool"], true);
        assert!(!zfs_probe_from(Some(only_zpool), None));

        // 文件存在但无执行位 → 不可用（which 语义）
        let no_exec = temp_bin_dir("noexec", &["zpool", "zfs"], false);
        assert!(!zfs_probe_from(Some(no_exec), None));

        // PATH 指向不存在目录 / PATH 缺失 → 不可用
        let missing = OsString::from("/nonexistent-definitely-not-here");
        assert!(!zfs_probe_from(Some(missing), None));
        assert!(!zfs_probe_from(None, None));
    }

    #[test]
    fn zfs_probe_env_gate_forces_unavailable() {
        // NEXOS_STORAGE_ZFS_PROBE=0/false/no/空 → 即使 PATH 齐备也强制不可用
        let both = temp_bin_dir("gated", &["zpool", "zfs"], true);
        for gate in ["0", "false", "no", ""] {
            assert!(
                !zfs_probe_from(Some(both.clone()), Some(gate.to_string())),
                "gate={gate:?} 应强制不可用"
            );
        }
        // 大小写不敏感 + 容忍空白
        assert!(!zfs_probe_from(Some(both), Some(" FALSE ".to_string())));
    }

    #[test]
    fn is_zfs_binary_missing_classification() {
        // —— 走降级 ——
        // sudo 包装路径（Spark 实测文案）：sudo 找不到 zpool，退出码 1
        let e = StorageError::CommandFailed(
            "zpool \"list -p -H\" 退出码 1：sudo: zpool: command not found".into(),
        );
        assert!(is_zfs_binary_missing(&e));
        // zfs 同款
        let e = StorageError::CommandFailed(
            "zfs \"list -p -H -o name\" 退出码 1：sudo: zfs: command not found".into(),
        );
        assert!(is_zfs_binary_missing(&e));
        // 直接 spawn 的 127（shell 语义 command-not-found）
        assert!(is_zfs_binary_missing(&StorageError::CommandFailed(
            "zpool \"list\" 退出码 127：".into()
        )));
        assert!(is_zfs_binary_missing(&StorageError::CommandFailed(
            "zpool list exit status 127".into()
        )));
        // spawn 直接失败（sudo 本体缺失 → ENOENT）
        assert!(is_zfs_binary_missing(&StorageError::Io(
            std::io::Error::from_raw_os_error(2)
        )));

        // —— 照旧 500（真实故障不掩盖）——
        // sudo 未免密（权限问题，不是二进制缺失）
        assert!(!is_zfs_binary_missing(&StorageError::CommandFailed(
            "zpool \"list -p -H\" 退出码 1：sudo: a password is required".into()
        )));
        // 池内部错误
        assert!(!is_zfs_binary_missing(&StorageError::CommandFailed(
            "zpool \"list -p -H\" 退出码 1：cannot open: internal error".into()
        )));
        // 其它 Io kind（如权限）
        assert!(!is_zfs_binary_missing(&StorageError::Io(
            std::io::Error::from_raw_os_error(13)
        )));
        // 业务错误（池不存在等）与二进制无关
        assert!(!is_zfs_binary_missing(&StorageError::PoolNotFound("p".into())));
    }

    // —— handle 层：探测不可用 → 读端点 200 空态 + 标志，零命令执行 ——

    #[tokio::test]
    async fn zfs_unavailable_read_endpoints_return_empty_state() {
        let (h, cmd) = handler_zfs_offline();

        // GET /pools → {pools: [], zfs_available: false}
        let resp = h.handle(get("/api/v1/pools")).await.unwrap();
        assert_eq!(resp.status, 200, "降级是 200 空态而非 500: {}", resp.body);
        assert_eq!(resp.body["pools"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);

        // GET /datasets → 同型空态
        let resp = h.handle(get("/api/v1/datasets")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["datasets"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);
        // query 参数不影响降级（分发前先短路）
        let resp = h.handle(get("/api/v1/datasets?pool=tank")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["zfs_available"], false);

        // GET /snapshots → 同型空态
        let resp = h.handle(get("/api/v1/snapshots")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["snapshots"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);

        // GET /disks/importable → 同型空态
        let resp = h.handle(get("/api/v1/disks/importable")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["importable"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);

        // 红线：降级路径不执行任何 zpool/zfs 命令（探测在 handler 层短路）
        assert!(
            cmd.calls().is_empty(),
            "降级不应触碰命令通道: {:?}",
            cmd.calls()
        );
    }

    // —— handle 层：探测不可用 → 写操作 400 明确原因，零命令执行 ——

    #[tokio::test]
    async fn zfs_unavailable_write_endpoints_return_400() {
        let (h, cmd) = handler_zfs_offline();

        // POST /pools（创建池）
        let resp = h
            .handle(post_json(
                "/api/v1/pools",
                serde_json::json!({ "name": "tank", "vdevs": [
                    { "kind": "disk", "disks": ["/dev/sdb"] }
                ] }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "创建池应 400: {}", resp.body);
        assert!(
            resp.body["error"].as_str().unwrap().contains("未安装 ZFS"),
            "错误体应带明确原因: {}",
            resp.body
        );

        // POST /datasets（创建数据集）
        let resp = h
            .handle(post_json(
                "/api/v1/datasets",
                serde_json::json!({ "name": "tank/media", "options": {} }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("未安装 ZFS"));

        // POST /disks/import（导入池；合法名通过白名单后触达守卫）
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "tank" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("未安装 ZFS"));

        // DELETE /pools/tank（删池）
        let resp = h
            .handle(delete_req("/api/v1/pools/tank", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("未安装 ZFS"));

        // 非法名仍然先 400「非法池名」（白名单先于守卫，防 flag 注入语义不变）
        let resp = h
            .handle(post_json(
                "/api/v1/disks/import",
                serde_json::json!({ "name": "-f" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("非法池名"));

        // 红线：零命令执行
        assert!(
            cmd.calls().is_empty(),
            "写操作守卫不应执行任何命令: {:?}",
            cmd.calls()
        );
    }

    // —— handle 层：探测不可用 → scrub/scrub-status/quota 沿用降级契约（200）——

    #[tokio::test]
    async fn zfs_unavailable_scrub_quota_degrade_without_500() {
        let (h, _cmd) = handler_zfs_offline();

        // POST /pools/tank/scrub → 200 {ok:false, warning}
        let resp = h
            .handle(post_json("/api/v1/pools/tank/scrub", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], false);
        assert!(resp.body["warning"].as_str().unwrap().contains("未安装 ZFS"));

        // GET /pools/tank/scrub-status → 200 {status:none, warning}
        let resp = h.handle(get("/api/v1/pools/tank/scrub-status")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "none");
        assert!(resp.body["warning"].as_str().unwrap().contains("未安装 ZFS"));

        // POST /datasets/tank/media/quota → 200 {ok:false, warning}
        let resp = h
            .handle(post_json(
                "/api/v1/datasets/tank/media/quota",
                serde_json::json!({ "quota_bytes": 1099511627776_u64 }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], false);
        assert!(resp.body["warning"].as_str().unwrap().contains("未安装 ZFS"));
    }

    // —— 错误分级：探测说可用但命令报「二进制缺失」→ 反应式降级（不再 500）——

    #[tokio::test]
    async fn zfs_binary_missing_backend_error_degrades_reactively() {
        // zpool list 报 sudo: zpool: command not found（Spark 实测路径：
        // 探测时 PATH 有 zpool，运行时 sudo 的 PATH 找不到——如实降级）
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput::fail(1, "sudo: zpool: command not found"),
        ));
        let resp = h.handle(get("/api/v1/pools")).await.unwrap();
        assert_eq!(resp.status, 200, "二进制缺失应降级 200: {}", resp.body);
        assert_eq!(resp.body["pools"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);

        // zfs list 同款（datasets/snapshots 走 zfs 二进制）
        let h = handler_with(FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput::fail(1, "sudo: zfs: command not found"),
        ));
        let resp = h.handle(get("/api/v1/datasets")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["datasets"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);
        let resp = h.handle(get("/api/v1/snapshots")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["snapshots"], serde_json::json!([]));
        assert_eq!(resp.body["zfs_available"], false);

        // 直接 spawn 的退出码 127 同样降级
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput::fail(127, ""),
        ));
        let resp = h.handle(get("/api/v1/pools")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["zfs_available"], false);
    }

    // —— 错误分级：非「二进制缺失」失败照旧 500（真实故障不掩盖）——

    #[tokio::test]
    async fn non_binary_failure_still_returns_500() {
        // zpool 内部错误 → 500（带 [storage/500] 标签）
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput::fail(1, "cannot open: internal error"),
        ));
        let err = h.handle(get("/api/v1/pools")).await.unwrap_err();
        match err {
            ApiGatewayError::Internal(msg) => {
                assert!(msg.contains("[storage/500]"), "应带 500 标签: {msg}");
            }
            other => panic!("应为 Internal，实际: {other:?}"),
        }

        // sudo 未免密（权限问题）→ 500，不降级
        let h = handler_with(FixtureRunner::new().on(
            "zpool",
            "list",
            CommandOutput::fail(1, "sudo: a password is required"),
        ));
        let err = h.handle(get("/api/v1/pools")).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));

        // datasets 路径同理：非二进制缺失的 zfs 失败 → 500
        let h = handler_with(FixtureRunner::new().on(
            "zfs",
            "list",
            CommandOutput::fail(1, "cannot open 'tank': dataset does not exist"),
        ));
        let err = h.handle(get("/api/v1/datasets")).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    // —— 可用路径回归：importable 正常响应显式携带 zfs_available:true ——
    //（pools/datasets/snapshots 可用路径仍返回裸数组——既有测试
    // get_pools_returns_real_zfs_data_as_json 等已覆盖，零形状变更。）

    #[tokio::test]
    async fn importable_available_response_carries_flag() {
        let (h, _cmd) = handler_with_fixed_cmd(
            fixed_runner(
                CommandOutput {
                    exit_code: 0,
                    stdout: import_sample(),
                    stderr: String::new(),
                },
                false,
            ),
            String::new(),
        );
        let resp = h.handle(get("/api/v1/disks/importable")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["zfs_available"], true);
        assert_eq!(resp.body["importable"].as_array().unwrap().len(), 3);
    }
}
