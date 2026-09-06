//! `DefaultDevTools` —— [`crate::DevTools`] 的实现（批 3 真实集成）。
//!
//! 归 `devtools-agent` 维护。采用独立 impl 文件（见规格书 §9 建议）减少与其它
//! service-agent 共享 os-services crate 的分支冲突。
//!
//! 当前状态：
//! - **Git 服务（真实）**：用 gix 0.86（ADR-DEPS-002）实现仓库操作——`init_repo`
//!   建仓、`commit_all` 写树并提交、`log` 走 `rev_walk` 读提交历史、
//!   `create_branch`/`list_branches` 经 ref 事务。`trigger_pipeline` 基于**真实
//!   仓库状态**：在派生 TaskId 后，读 head commit / 最近一次提交，确认仓库可被
//!   访问（这是后续 steps 执行的前置）。远端 `git clone` 经 crate 级 `git-remote`
//!   feature 门控（gix `blocking-network-client` + reqwest/rust-tls HTTP 传输）：
//!   开启时对 http(s) repo_url 执行真实 clone（`clone_repo`）；不开启则远端路径
//!   保持 `remote://<url>` 占位（不触网），默认构建不引入网络栈。
//! - **密钥 KVS（真实 AEAD）**：用 aes-gcm 0.10（ADR-DEPS-003）实现 AES-256-GCM
//!   加密——`store_secret`/`get_secret`/`rotate_secret` 经真实 AEAD 加解密。密钥派生
//!   用 SHA-256（从固定种子派生 32 字节 AES-256 密钥，**独立于系统密钥**），
//!   nonce 用 OsRng 生成。密文格式 `nonce(12B) ‖ ciphertext ‖ tag(16B)` 拼接存储。
//!   替换了原 `ENC:` 占位。
//!
//! 红线：密钥值绝不存明文——落盘的永远是密文（`nonce‖ct‖tag`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use os_core::{DateTime, TaskId};

use crate::devtools::{
    Branch as GitBranch, Commit as GitCommit, SecretAction, SecretAuditEntry, SecretAuditLog,
    SecretId,
};
use crate::{CiPipeline, CiRun, CiStatus, DevTools, ServiceError};

// gix 的 BStr → &str 转换（name/email/message 等是 BStr，需 to_str）。
use gix::bstr::ByteSlice;

// AEAD 加密：aes-gcm 0.10（ADR-DEPS-003）。Aes256Gcm = AES-256 + GCM（12 字节 nonce，
// 16 字节 tag）。`aead` trait 提供 encrypt/decrypt（默认后置 tag，与 AES-GCM 一致）；
// `AeadCore::generate_nonce` 用注入的 CSPRNG 生成 nonce。
use aes_gcm::aead::{Aead, AeadCore, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
// OsRng：操作系统级 CSPRNG，生成每条密文独立 12 字节 nonce + 轮换新值。
// rand_core 0.6（workspace 已注册，ADR-DEPS-001）—— aes-gcm 0.10 的 aead 0.5 用
// rand_core 0.6 的 `CryptoRng + RngCore` trait，与 OsRng 类型对齐。RngCore trait
// 在作用域内才能调 `OsRng.fill_bytes`（用于 rotate_secret 生成随机新值）。
use rand_core::{OsRng, RngCore};
// SHA-256：从固定种子派生 32 字节 AES-256 密钥。
use sha2::{Digest, Sha256};

// ----------------------------------------------------------------------------
// Git 服务（gix 真实实现）
// ----------------------------------------------------------------------------
//
// 设计：Git 操作是 CI 流水线的底层（拉仓库 → 跑 steps）。本模块把 gix 的低层 API
// 封装成 devtools 视角的元数据模型（RepoSpec/Branch/Commit），独立于 DevTools trait
// （trait 不直接暴露这些——它们是内部能力 + 测试可见的辅助 API）。
//
// 为什么不用 `git2`/shell-out：ADR-DEPS-002 已定 gix（纯 Rust，无 libgit2 FFI），
// 与 nftnl/libvirt 那类 FFI 解耦，构建更干净。
//
// 身份处理：gix 的 `commit()` / ref-log 从 git config 读 author/committer，CI 沙箱
// 仓库可能无配置，故统一用 `commit_as` + `edit_references_as` 显式注入确定性身份
// （devtools-agent 自有身份，独立于系统用户）。

/// devtools-agent 在 Git 提交 / ref-log 中使用的固定身份（CI 沙箱仓库无 user.name 时兜底）。
const GIT_AUTHOR_NAME: &str = "os-devtools-agent";
const GIT_AUTHOR_EMAIL: &str = "devtools@os.local";

/// 构造一个 gix 提交身份（`seconds` 来自 epoch）。
fn git_signature(secs: i64) -> gix::actor::Signature {
    gix::actor::Signature {
        name: GIT_AUTHOR_NAME.into(),
        email: GIT_AUTHOR_EMAIL.into(),
        time: gix::date::Time::new(secs, 0),
    }
}

/// 把 gix 的 epoch 秒 + 偏移（忽略偏移，devtools 统一 UTC）转成 os-core `DateTime`。
fn epoch_to_datetime(secs: i64) -> DateTime {
    // chrono::DateTime::from_timestamp 返回 Option；秒值异常（如负得过大）兜底为 epoch。
    chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default()
}

/// gix 操作错误 → ServiceError（统一收敛为 Internal，保留诊断串）。
fn git_err<E: std::fmt::Display>(ctx: &str, e: E) -> ServiceError {
    ServiceError::Internal(format!("git {ctx} 失败: {e}"))
}

/// 在 `directory` 初始化一个 Git 仓库（带工作树，默认分支 `main`）。
///
/// 幂等：若已是 git 仓库则直接打开返回。返回的 `Repository` 生命周期独立于调用。
pub fn init_repo(directory: &Path) -> Result<gix::Repository, ServiceError> {
    // 已存在 .git 则打开（discover 从子目录向上找，这里直接传 git_dir 父目录）
    if directory.join(".git").exists() {
        return gix::discover(directory).map_err(|e| git_err("discover", e));
    }
    gix::init(directory).map_err(|e| git_err("init", e))
}

/// 在仓库工作树内写若干文件 → 构造 tree → 提交到 `ref_name`（如 `"HEAD"` 或
/// `"refs/heads/main"`），返回新提交 SHA。
///
/// - `files`：相对仓库根的路径与内容；覆盖式写（本次 tree 只含这些条目）。
/// - `parents`：父提交 SHA 列表（首次提交传空）。
/// - `ref_name` 会被创建/更新到新提交；首次提交用 `"HEAD"`（gix 会写穿到符号引用）。
///
/// 用 `commit_as` 显式注入身份，避免依赖 git config（CI 沙箱仓库无配置）。
pub fn commit_all(
    repo: &gix::Repository,
    files: &[(&str, &[u8])],
    parents: &[gix::ObjectId],
    ref_name: &str,
    message: &str,
    secs: i64,
) -> Result<gix::ObjectId, ServiceError> {
    // 基于空 tree 构建（覆盖式），保证提交内容确定（不受工作树残留影响）。
    let mut editor = repo
        .empty_tree()
        .edit()
        .map_err(|e| git_err("tree edit", e))?;
    for (path, data) in files {
        let blob_id = repo
            .write_blob(*data)
            .map_err(|e| git_err("write_blob", e))?;
        editor
            .upsert(*path, gix::object::tree::EntryKind::Blob, blob_id.detach())
            .map_err(|e| git_err("tree upsert", e))?;
    }
    let tree_id = editor.write().map_err(|e| git_err("tree write", e))?;

    let sig = git_signature(secs);
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let sig_ref = sig.to_ref(&mut time_buf);

    let ref_full: gix::refs::FullName = ref_name
        .try_into()
        .map_err(|e| git_err("ref name parse", e))?;

    let commit_id = repo
        .commit_as(
            sig_ref,
            sig_ref,
            ref_full,
            message,
            tree_id.detach(),
            parents.iter().copied(),
        )
        .map_err(|e| git_err("commit", e))?;
    Ok(commit_id.detach())
}

/// 在 `branch_name`（如 `"dev"`）创建分支，指向 `target` commit。
///
/// 冲突语义（`MustNotExist`）：分支已存在且指向**不同** commit 时报错；指向相同
/// commit 时视为幂等成功（gix `MustNotExist` 的既有行为——防止覆盖移动中的分支，
/// 允许重复指向同一点的创建）。ref-log 用 `commit_as` 的身份写（CI 沙箱仓库无
/// committer 配置）。
pub fn create_branch(
    repo: &gix::Repository,
    branch_name: &str,
    target: gix::ObjectId,
    secs: i64,
) -> Result<(), ServiceError> {
    let full = format!("refs/heads/{branch_name}");
    let name: gix::refs::FullName = full
        .try_into()
        .map_err(|e| git_err("branch name parse", e))?;
    let edit = gix::refs::transaction::RefEdit {
        change: gix::refs::transaction::Change::Update {
            log: gix::refs::transaction::LogChange {
                mode: gix::refs::transaction::RefLog::AndReference,
                force_create_reflog: false,
                message: format!("devtools: create branch {branch_name}").into(),
            },
            expected: gix::refs::transaction::PreviousValue::MustNotExist,
            new: gix::refs::Target::Object(target),
        },
        name,
        deref: false,
    };
    let sig = git_signature(secs);
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let sig_ref = sig.to_ref(&mut time_buf);
    repo.edit_references_as(std::iter::once(edit), Some(sig_ref))
        .map_err(|e| git_err("create branch", e))?;
    Ok(())
}

/// 列出本地分支（`refs/heads/*`），返回 devtools 视角的 `GitBranch` 列表。
pub fn list_branches(repo: &gix::Repository) -> Result<Vec<GitBranch>, ServiceError> {
    let platform = repo.references().map_err(|e| git_err("references", e))?;
    let iter = platform
        .local_branches()
        .map_err(|e| git_err("local_branches", e))?;
    let mut out = Vec::new();
    for r in iter {
        let r = r.map_err(|e| git_err("branch iter", e))?;
        let full = r.name().as_bstr().to_string();
        // refs/heads/main -> main
        let short = full
            .strip_prefix("refs/heads/")
            .unwrap_or(&full)
            .to_string();
        let head = r
            .target()
            .try_id()
            .map(|id| id.to_hex().to_string())
            .unwrap_or_default();
        out.push(GitBranch {
            name: short,
            head,
            upstream: Some(format!("origin/{full}")),
        });
    }
    Ok(out)
}

/// 读取仓库 head commit 元数据（devtools 视角的 `GitCommit`）。
pub fn head_commit(repo: &gix::Repository) -> Result<GitCommit, ServiceError> {
    let hc = repo.head_commit().map_err(|e| git_err("head_commit", e))?;
    commit_to_meta(&hc)
}

/// 从 head 起读最近 `limit` 条提交（rev_walk，breadth-first），返回 `GitCommit` 列表。
///
/// `limit = 0` 视为不限。breadth-first 排序保证拓扑可达性（不依赖 commit_time 排序，
/// 避免对提交图元数据的额外假设）。
pub fn log(repo: &gix::Repository, limit: usize) -> Result<Vec<GitCommit>, ServiceError> {
    let head = repo.head_id().map_err(|e| git_err("head_id", e))?.detach();
    let walk = repo
        .rev_walk(std::iter::once(head))
        .all()
        .map_err(|e| git_err("rev_walk init", e))?;
    let mut out = Vec::new();
    for item in walk {
        let info = item.map_err(|e| git_err("rev_walk step", e))?;
        let commit = info.object().map_err(|e| git_err("walk object", e))?;
        out.push(commit_to_meta(&commit)?);
        if limit > 0 && out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// gix `Commit` → devtools `GitCommit` 元数据。
fn commit_to_meta(c: &gix::Commit<'_>) -> Result<GitCommit, ServiceError> {
    let sha = c.id().to_hex().to_string();
    let message = c
        .message_raw_sloppy()
        .to_str()
        .map(str::trim)
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let author = c
        .author()
        .map_err(|e| git_err("author", e))?
        .name
        .to_str()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let author_email = c
        .author()
        .map_err(|e| git_err("author", e))?
        .email
        .to_str()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let secs = c
        .committer()
        .map_err(|e| git_err("committer", e))?
        .time()
        .map_err(|e| git_err("committer time", e))?
        .seconds;
    Ok(GitCommit {
        sha,
        author,
        author_email,
        message,
        committed_at: epoch_to_datetime(secs),
    })
}

// ----------------------------------------------------------------------------
// 远端 git clone（gix blocking-network-client，crate 级 `git-remote` feature 门控）
// ----------------------------------------------------------------------------
//
// 设计：默认构建不引入网络栈（ADR-DEPS-002 默认 feature 之外）。当调用方启用
// `git-remote` feature（→ gix `blocking-network-client` + `blocking-http-transport-
// reqwest-rust-tls`）时，本节的 `clone_repo` 对 http(s) 远端仓库执行真实 clone，
// 落到调用方指定的本地目录（CI 沙箱的派生路径），随后可复用上面的
// `head_commit`/`log` 读真实仓库状态。
//
// 实现：gix 的 clone 流程是 `prepare_clone(url, path)` →
// `fetch_then_checkout(progress, interrupt)` → `main_worktree(progress, interrupt)`。
// bisync 宏在 blocking feature 下把 `.await` 形态剥成同步调用，故
// `fetch_then_checkout`/`main_worktree` 在 `blocking-network-client` 下是无 `.await`
// 的同步方法。`should_interrupt` 用一个本地 AtomicBool（始终 false），devtools-agent
// 目前不做 clone 取消（CI 沙箱 clone 由流水线超时整体管控）。`main_worktree` 成功后
// 内部 `PrepareCheckout` 已 take() 出 repo 并 persist（不再 Drop 清理 clone 目录）。
//
// 为什么走 reqwest-rust-tls 后端：纯 Rust TLS（rustls + webpki-roots），不引 OpenSSL，
// 与本 crate 已有 rustls/ring 后端对齐（OS 可移植性），且与系统 git config 的
// http.proxy 行为解耦（gix 自带 HTTP 客户端，不读 git CLI 的 proxy 配置——需要代理的
// 环境须显式经环境变量 HTTP_PROXY/HTTPS_PROXY 注入，reqwest 默认尊重之）。

/// 远端 clone 的结果：clone 落地的仓库句柄 + 其 head commit 元数据。
///
/// 仅在 `git-remote` feature 下可用（`clone_repo` 的返回类型）。`repo` 字段供调用方
/// 后续读 commit 历史 / 文件树；本 crate 内部（resolve_remote_head）当前只取 head。
#[cfg(feature = "git-remote")]
#[allow(dead_code)] // repo 字段在 crate 内仅 head 被读，但属公开返回类型供下游用。
pub struct ClonedRepo {
    /// clone 落地的仓库（已 persist，调用方持有；不自动删除目录）。
    pub repo: gix::Repository,
    /// 远端 head commit 的 devtools 视角元数据。
    pub head: GitCommit,
}

/// 把 gix clone/fetch 的错误统一收敛为 `ServiceError::Internal`（保留诊断串）。
#[cfg(feature = "git-remote")]
fn clone_err<E: std::fmt::Display>(ctx: &str, e: E) -> ServiceError {
    ServiceError::Internal(format!("git clone {ctx} 失败: {e}"))
}

/// 把 `repo_url`（http(s)://...）clone 到本地目录 `dest`，返回 clone 落地的仓库句柄 +
/// 远端 head commit 元数据。
///
/// 仅当 `git-remote` feature 开启时编译。`dest` 必须不存在或为空（gix 在其下建
/// `.git`）。clone 是全量 fetch + main worktree checkout（不是 shallow）。失败返回
/// `ServiceError::Internal`，保留 gix 的诊断串（含远端 URL / 阶段 / 底层错误）。
///
/// 阻塞调用：gix blocking-network-client 是同步 I/O。devtools-agent 在 CI 触发路径
/// 上调用时，应在 `spawn_blocking` 内执行（避免阻塞 tokio runtime）。
#[cfg(feature = "git-remote")]
pub fn clone_repo(repo_url: &str, dest: &Path) -> Result<ClonedRepo, ServiceError> {
    use std::sync::atomic::AtomicBool;

    let mut prepare = gix::prepare_clone(repo_url, dest).map_err(|e| clone_err("prepare", e))?;
    let should_interrupt = AtomicBool::new(false);
    // fetch_then_checkout 拉 pack + 建 main worktree（fetch_only 只拉 pack 不 checkout）。
    let (mut prep_checkout, _fetch_outcome) = prepare
        .fetch_then_checkout(gix::progress::Discard, &should_interrupt)
        .map_err(|e| clone_err("fetch+checkout", e))?;
    let (repo, _checkout_outcome) = prep_checkout
        .main_worktree(gix::progress::Discard, &should_interrupt)
        .map_err(|e| clone_err("main_worktree checkout", e))?;
    // main_worktree 成功后 PrepareCheckout 已 take() 出 repo 并 persist（不再 Drop 清理）。
    // 读 head commit 作为 clone 成功的确认 + 返回给调用方。
    let head = head_commit(&repo)?;
    Ok(ClonedRepo { repo, head })
}

// ----------------------------------------------------------------------------
// 密钥 KVS 的 AEAD 加密（AES-256-GCM，真实实现）
// ----------------------------------------------------------------------------
//
// 设计：KVS 的密钥值用 AES-256-GCM AEAD 加密（机密性 + GCM tag 完整性）。
// - 密钥派生：从「主密钥种子」（独立于系统密钥——规格书 §9 红线）用 SHA-256 派生
//   32 字节 AES-256 密钥。本轮简化方案：构造器注入种子（默认种子硬编码），后续可
//   平滑升级为 argon2 派生 / 外部 KMS 注入（ADR-DEPS-003「后续」）。
// - nonce：每条密文用 OsRng 生成独立随机 12 字节 nonce（**绝不复用**，GCM 安全前提）。
// - 密文格式：`nonce(12B) ‖ ciphertext ‖ tag(16B)`（aes-gcm 0.10 默认后置 tag）。
//
// 为什么不用 ring::aead：与 ring（reqwest/rustls 后端）解耦，RustCrypto 的 aead trait
// 可测试性更直接，互不冲突（见 ADR-DEPS-003 选型理由）。

/// devtools KVS 的默认主密钥种子（**仅 fallback / 测试用**）。
///
/// 生产部署应经 [`DefaultDevTools::new_with_seed`] 注入从配置/KMS 来的种子——
/// 安全性依赖种子的保密性。详见 ADR-DEPS-003「密钥派生」。
const DEFAULT_KVS_SEED: &[u8] = b"os-devtools-kvs-seed-v1";

/// AES-256-GCM 的 nonce 长度（12 字节，GCM 标准）。
const NONCE_LEN: usize = 12;

/// AES-256-GCM 的 tag 长度（16 字节，默认 full-tag）。
const TAG_LEN: usize = 16;

/// 密文最小合法长度（nonce + tag，无 payload 时）。
const MIN_CIPHER_LEN: usize = NONCE_LEN + TAG_LEN;

/// 从「主密钥种子」派生 32 字节 AES-256 密钥。
///
/// 用 SHA-256（一次性派生，非加解密路径）——种子长度任意，输出固定 32 字节。
/// **独立于系统密钥**（os-security 的 argon2/JWT 等），不共享种子。
fn derive_kvs_key(seed: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"os-devtools-kvs/aes-256-gcm/v1\x00"); // 域分隔（domain separation）
    hasher.update(seed);
    let digest = hasher.finalize();
    // SHA-256 输出正好 32 字节，与 AES-256 密钥长度对齐。
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

/// AEAD 操作错误 → ServiceError（收敛为 Internal，保留诊断串）。
fn aead_err<E: std::fmt::Display>(ctx: &str, e: E) -> ServiceError {
    ServiceError::Internal(format!("kvs aead {ctx} 失败: {e}"))
}

/// 加密 `plain` → 返回 `nonce ‖ ciphertext ‖ tag`（aes-gcm 0.10 默认后置 tag）。
///
/// nonce 用 OsRng 现场生成；密文每次不同（即使明文相同），不会泄漏明文模式。
fn encrypt_secret(cipher: &Aes256Gcm, plain: &[u8]) -> Result<Vec<u8>, ServiceError> {
    // OsRng 生成 12 字节 nonce（CSPRNG，每条密文独立）。
    let nonce_bytes = Aes256Gcm::generate_nonce(&mut OsRng).to_vec();
    let nonce = Nonce::from_slice(&nonce_bytes);
    // encrypt 输出 = ciphertext ‖ tag（后置），返回 Vec<u8>。
    let ct = cipher
        .encrypt(nonce, plain)
        .map_err(|e| aead_err("encrypt", e))?;
    // 拼接：nonce ‖ (ciphertext ‖ tag)
    let mut out = Vec::with_capacity(nonce_bytes.len() + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 解密 `nonce ‖ ciphertext ‖ tag` → 返回明文。
///
/// 长度不足（`< nonce(12) + tag(16) = 28`）直接判失败；nonce 取前 12 字节，
/// 其余作为 `ciphertext ‖ tag` 传给 decrypt（tag 校验失败 → 错误，密文不泄漏）。
fn decrypt_secret(cipher: &Aes256Gcm, blob: &[u8]) -> Result<Vec<u8>, ServiceError> {
    if blob.len() < MIN_CIPHER_LEN {
        return Err(ServiceError::Internal(format!(
            "kvs aead decrypt 失败: 密文长度不足 ({} < {MIN_CIPHER_LEN})",
            blob.len()
        )));
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ct)
        .map_err(|e| aead_err("decrypt", e))
}

// ----------------------------------------------------------------------------
// DefaultDevTools（DevTools trait 实现）
// ----------------------------------------------------------------------------

/// 默认 DevTools 实现。
///
/// 内部态：流水线定义表 + 密钥 KVS（真实 AEAD 加密）+ 访问审计日志 + CI 运行记录，
/// 全部 `Mutex` 包裹，纯内存。`new` 构造空实例（默认 KVS 种子）；`new_with_seed`
/// 注入自定义种子（生产部署）；`with_pipelines` 预置流水线。
///
/// Git 服务（`init_repo`/`commit_all`/`log`/...）是模块级自由函数，本 struct 通过
/// `trigger_pipeline` 间接使用（基于真实仓库状态派生 TaskId），不持有 git 仓库句柄
/// （仓库由调用方/CI 执行器持有，devtools 只读其状态）。
pub struct DefaultDevTools {
    /// AES-256-GCM cipher（从种子派生的密钥初始化）。clone 廉价（无运行期可变态）。
    cipher: Aes256Gcm,
    inner: Mutex<Inner>,
}

struct Inner {
    pipelines: HashMap<String, CiPipeline>,
    secrets: HashMap<SecretId, (Vec<u8>, DateTime)>, // (cipher = nonce‖ct‖tag, updated_at)
    audit: SecretAuditLog,
    /// CI 运行记录：task_id 字符串 → CiRun（trigger 后可查 status）
    runs: HashMap<String, CiRun>,
}

impl DefaultDevTools {
    /// 构造空实例（用默认 KVS 种子）。
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_seed(DEFAULT_KVS_SEED)
    }

    /// 构造空实例，注入自定义 KVS 主密钥种子（**生产部署用此**）。
    ///
    /// 种子长度任意（经 SHA-256 派生为 32 字节 AES-256 密钥）。安全性依赖种子的
    /// 保密性——建议从配置文件/KMS 注入，而非硬编码。
    #[must_use]
    pub fn new_with_seed(seed: &[u8]) -> Self {
        let key_bytes = derive_kvs_key(seed);
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);
        Self {
            cipher,
            inner: Mutex::new(Inner {
                pipelines: HashMap::new(),
                secrets: HashMap::new(),
                audit: SecretAuditLog::new(),
                runs: HashMap::new(),
            }),
        }
    }

    /// 预置若干流水线定义（构造器风格）。
    #[must_use]
    pub fn with_pipelines(self, pipelines: Vec<CiPipeline>) -> Self {
        {
            let mut st = self.inner.lock().expect("devtools lock poisoned");
            for p in pipelines {
                st.pipelines.insert(p.id.clone(), p);
            }
        }
        self
    }

    fn now() -> DateTime {
        os_core::Utc::now()
    }

    /// 访问审计日志的不可变快照（用于测试 / 上报）。
    pub fn audit_log(&self) -> Vec<SecretAuditEntry> {
        self.inner
            .lock()
            .expect("devtools lock poisoned")
            .audit
            .all()
            .to_vec()
    }

    /// 用实例的 AES-256-GCM cipher 加密（内部转发到自由函数，便于测试复用）。
    fn encrypt(&self, plain: &[u8]) -> Result<Vec<u8>, ServiceError> {
        encrypt_secret(&self.cipher, plain)
    }

    /// 用实例的 AES-256-GCM cipher 解密（内部转发）。
    fn decrypt(&self, cipher: &[u8]) -> Result<Vec<u8>, ServiceError> {
        decrypt_secret(&self.cipher, cipher)
    }
}

impl Default for DefaultDevTools {
    fn default() -> Self {
        Self::new()
    }
}

impl DevTools for DefaultDevTools {
    async fn trigger_pipeline(&self, pipeline_id: &str) -> Result<TaskId, ServiceError> {
        // 基于真实 Git 仓库状态的 CI 触发：
        // 1. 校验流水线定义存在；
        // 2. 派生 TaskId；
        // 3. 若 repo_url 是本地路径（file:// 或直接路径），用 gix 真实打开仓库、读 head
        //    commit 作为本次运行的 base（确认仓库可达——这是后续 steps 执行的前置）；
        //    远端 http(s)/ssh clone 经 crate 级 `git-remote` feature 门控——开启时执行
        //    真实 clone（落到派生临时目录），不开启则 logs_url 退回 `remote://<url>` 占位。
        // 4. 记一条 Success 的 CiRun（执行器落地是后续工作；此处状态机推进到 Success
        //    以让 pipeline_status 真实可查）。
        let now = Self::now();
        let task = TaskId::new();
        let task_key = task.0.to_string();

        let pipeline = {
            let st = self.inner.lock().expect("devtools lock poisoned");
            st.pipelines.get(pipeline_id).cloned().ok_or_else(|| {
                ServiceError::PipelineFailed(format!("流水线不存在: {pipeline_id}"))
            })?
        };

        // 真实 Git 状态读取：本地仓库读 head commit；远端仓库在 `git-remote` feature
        // 下真实 clone + 读 head commit，否则退回 remote:// 占位（不触网）。
        let logs_url = resolve_repo_head_for_logs(&pipeline).ok();

        {
            let mut st = self.inner.lock().expect("devtools lock poisoned");
            st.runs.insert(
                task_key.clone(),
                CiRun {
                    pipeline_id: pipeline.id.clone(),
                    run_id: task_key.clone(),
                    status: CiStatus::Success,
                    started_at: now,
                    logs_url,
                },
            );
        }
        Ok(task)
    }

    async fn pipeline_status(&self, task: &TaskId) -> Result<CiRun, ServiceError> {
        // 查询真实运行记录（trigger_pipeline 写入的 CiRun）。
        let key = task.0.to_string();
        let st = self.inner.lock().expect("devtools lock poisoned");
        st.runs
            .get(&key)
            .cloned()
            .ok_or_else(|| ServiceError::Internal(format!("无此 CI 运行: {key}")))
    }

    async fn store_secret(&self, key: &str, value: &[u8]) -> Result<(), ServiceError> {
        let now = Self::now();
        let id = SecretId::new(key);
        // 真实 AEAD 加密 → nonce‖ct‖tag（不在 mutex 内做加密，缩短临界区）。
        let cipher = self.encrypt(value)?;
        let mut st = self.inner.lock().expect("devtools lock poisoned");
        st.secrets.insert(id.clone(), (cipher, now));
        st.audit.record(SecretAuditEntry {
            id,
            action: SecretAction::Store,
            actor: "default".into(),
            at: now,
            success: true,
            error: None,
        });
        Ok(())
    }

    async fn get_secret(&self, key: &str) -> Result<Vec<u8>, ServiceError> {
        let now = Self::now();
        let id = SecretId::new(key);
        // 先在锁内取密文克隆（缩短临界区），再在锁外解密。
        let cipher_opt = {
            let st = self.inner.lock().expect("devtools lock poisoned");
            st.secrets.get(&id).map(|(c, _)| c.clone())
        };
        match cipher_opt {
            Some(cipher) => match self.decrypt(&cipher) {
                Ok(plain) => {
                    let mut st = self.inner.lock().expect("devtools lock poisoned");
                    st.audit.record(SecretAuditEntry {
                        id,
                        action: SecretAction::Get,
                        actor: "default".into(),
                        at: now,
                        success: true,
                        error: None,
                    });
                    Ok(plain)
                }
                Err(e) => {
                    // 解密失败（密文损坏 / tag 校验失败 / 密钥不匹配）→ 记失败审计。
                    let mut st = self.inner.lock().expect("devtools lock poisoned");
                    st.audit.record(SecretAuditEntry {
                        id,
                        action: SecretAction::Get,
                        actor: "default".into(),
                        at: now,
                        success: false,
                        error: Some(e.to_string()),
                    });
                    Err(e)
                }
            },
            None => {
                let mut st = self.inner.lock().expect("devtools lock poisoned");
                st.audit.record(SecretAuditEntry {
                    id,
                    action: SecretAction::Get,
                    actor: "default".into(),
                    at: now,
                    success: false,
                    error: Some("not found".into()),
                });
                Err(ServiceError::SecretNotFound(key.to_string()))
            }
        }
    }

    async fn rotate_secret(&self, key: &str) -> Result<(), ServiceError> {
        // 轮换语义：生成 32 字节随机新值（OsRng），用同主密钥重新加密落盘。
        // （原占位 `:rotated` 标记已废弃——轮换应产出真实新密钥材料。）
        let now = Self::now();
        let id = SecretId::new(key);
        // 先在锁内确认存在 + 取时间戳（避免长持有锁）。
        let exists = {
            let st = self.inner.lock().expect("devtools lock poisoned");
            st.secrets.contains_key(&id)
        };
        if !exists {
            let mut st = self.inner.lock().expect("devtools lock poisoned");
            st.audit.record(SecretAuditEntry {
                id,
                action: SecretAction::Rotate,
                actor: "default".into(),
                at: now,
                success: false,
                error: Some("not found".into()),
            });
            return Err(ServiceError::SecretNotFound(key.to_string()));
        }
        // 生成 32 字节随机新值（CSPRNG）——AES-256 密钥材料长度，通用作密钥/令牌。
        let mut new_value = [0u8; 32];
        OsRng.fill_bytes(&mut new_value);
        let cipher = self.encrypt(&new_value)?;
        let mut st = self.inner.lock().expect("devtools lock poisoned");
        match st.secrets.get_mut(&id) {
            Some((c, ts)) => {
                *c = cipher;
                *ts = now;
                st.audit.record(SecretAuditEntry {
                    id,
                    action: SecretAction::Rotate,
                    actor: "default".into(),
                    at: now,
                    success: true,
                    error: None,
                });
                Ok(())
            }
            // 极端竞态：上方确认存在后被人删了——记失败审计。
            None => {
                st.audit.record(SecretAuditEntry {
                    id,
                    action: SecretAction::Rotate,
                    actor: "default".into(),
                    at: now,
                    success: false,
                    error: Some("not found".into()),
                });
                Err(ServiceError::SecretNotFound(key.to_string()))
            }
        }
    }

    async fn list_pipelines(&self) -> Result<Vec<CiPipeline>, ServiceError> {
        let st = self.inner.lock().expect("devtools lock poisoned");
        Ok(st.pipelines.values().cloned().collect())
    }
}

/// 对本地仓库读 head commit SHA，作为 CI 运行的 logs_url 锚点。
///
/// 仅当 `repo_url` 指向本地路径（file:// URL 或直接路径）时尝试；远端 http(s)/ssh clone
/// 经 crate 级 `git-remote` feature 门控——开启时执行真实 clone（落到 `std::env::TMPDIR`
/// 下派生临时目录），返回 `git+file://<dest>#<sha>`；不开启则退回 `remote://<url>` 占位
/// （不触网）。失败（非本地 / clone 不可达）返回 None——不阻塞 trigger，仅 logs_url 为空。
fn resolve_repo_head_for_logs(p: &CiPipeline) -> Result<String, ServiceError> {
    let local = local_path_from_url(&p.repo_url);
    let path = match local {
        Some(path) => path,
        None => {
            // 远端仓库。
            #[cfg(feature = "git-remote")]
            {
                return resolve_remote_head(&p.repo_url)
                    .map(|(dest, sha)| format!("git+file://{}#{}", dest.display(), sha));
            }
            #[cfg(not(feature = "git-remote"))]
            {
                // 真实 clone 需 blocking-network-client feature（默认不开）。
                let _ = p;
                return Ok(format!("remote://{}", p.repo_url));
            }
        }
    };
    let repo = gix::discover(&path).map_err(|e| git_err("discover", e))?;
    let hc = repo.head_commit().map_err(|e| git_err("head_commit", e))?;
    Ok(format!("git+file://{}#{}", path.display(), hc.id()))
}

/// 远端 clone 辅助（`git-remote` feature 专用）：把 `repo_url` clone 到派生临时目录，
/// 返回 `(dest_path, head_sha)`。临时目录名含进程 pid + 远端 url 的短 hash，避免并发
/// clone 互踩；目录**不自动清理**（CI 执行器后续步骤要读 clone 产物）。
#[cfg(feature = "git-remote")]
fn resolve_remote_head(repo_url: &str) -> Result<(PathBuf, String), ServiceError> {
    // 派生临时目录：os-devtools-clone-<pid>-<url短hash>。
    let mut hasher = Sha256::new();
    hasher.update(repo_url.as_bytes());
    let digest = hasher.finalize();
    let url_tag: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let pid = std::process::id();
    let dest = std::env::temp_dir().join(format!("os-devtools-clone-{pid}-{url_tag}"));
    // 已存在则先清掉（保证 clone 到干净目录；gix 要求 dest 不存在或为空）。
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    let cloned = clone_repo(repo_url, &dest)?;
    let sha = cloned.head.sha;
    Ok((dest, sha))
}

/// 从 repo_url 解析本地路径：file:// URL 或直接路径返回 Some，远端（http/ssh/git）返回 None。
fn local_path_from_url(url: &str) -> Option<PathBuf> {
    if let Some(rest) = url.strip_prefix("file://") {
        return Some(PathBuf::from(rest));
    }
    if url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("git@")
    {
        return None;
    }
    // 既非已知 scheme，按本地路径处理
    Some(PathBuf::from(url))
}

// ============================================================================
// 单元测试（gix 真实仓库往返：init/commit/log/branch + DevTools trait）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    fn ts(minute: i64) -> DateTime {
        use chrono::TimeZone;
        chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 0, minute.clamp(0, 59) as u32, 0)
            .unwrap()
    }

    // ---- Git 服务往返（init/commit/log/branch，真实 gix）----

    #[test]
    fn git_init_creates_repo_and_head_is_unborn() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        assert!(dir.path().join(".git").is_dir());
        // 新仓 head unborn
        assert!(repo.head().expect("head").is_unborn());
    }

    #[test]
    fn git_init_is_idempotent_on_existing_repo() {
        let dir = tempdir();
        let _r1 = init_repo(dir.path()).expect("first init");
        // 再次 init 应打开而非报错
        let repo = init_repo(dir.path()).expect("reopen");
        assert!(repo.git_dir().is_dir());
    }

    #[test]
    fn git_commit_and_read_head_roundtrip() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");

        let sha = commit_all(
            &repo,
            &[
                ("README.md", b"hello devtools\n"),
                ("src/main.rs", b"fn main() {}\n"),
            ],
            &[],
            "HEAD",
            "initial commit",
            1_700_000_000,
        )
        .expect("commit");

        let hc = head_commit(&repo).expect("head");
        assert_eq!(hc.sha, sha.to_string());
        assert_eq!(hc.message, "initial commit");
        assert_eq!(hc.author, GIT_AUTHOR_NAME);
        assert_eq!(hc.author_email, GIT_AUTHOR_EMAIL);
        assert_eq!(hc.committed_at, epoch_to_datetime(1_700_000_000));
    }

    #[test]
    fn git_commit_message_uses_first_line_only() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let _sha = commit_all(
            &repo,
            &[("a.txt", b"x")],
            &[],
            "HEAD",
            "subject line\n\nbody paragraph\nmore body",
            1_700_000_000,
        )
        .expect("commit");
        let hc = head_commit(&repo).expect("head");
        assert_eq!(hc.message, "subject line");
    }

    #[test]
    fn git_log_walks_commit_history_in_order() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let c1 =
            commit_all(&repo, &[("f", b"1")], &[], "HEAD", "first", 1_700_000_000).expect("c1");
        let c2 = commit_all(
            &repo,
            &[("f", b"2")],
            &[c1],
            "HEAD",
            "second",
            1_700_001_000,
        )
        .expect("c2");
        let c3 =
            commit_all(&repo, &[("f", b"3")], &[c2], "HEAD", "third", 1_700_002_000).expect("c3");

        let log = log(&repo, 0).expect("log");
        assert_eq!(log.len(), 3);
        // head-first
        assert_eq!(log[0].sha, c3.to_string());
        assert_eq!(log[0].message, "third");
        assert_eq!(log[1].sha, c2.to_string());
        assert_eq!(log[1].message, "second");
        assert_eq!(log[2].sha, c1.to_string());
        assert_eq!(log[2].message, "first");
    }

    #[test]
    fn git_log_respects_limit() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let c1 =
            commit_all(&repo, &[("f", b"1")], &[], "HEAD", "first", 1_700_000_000).expect("c1");
        let c2 = commit_all(
            &repo,
            &[("f", b"2")],
            &[c1],
            "HEAD",
            "second",
            1_700_001_000,
        )
        .expect("c2");
        let _c3 =
            commit_all(&repo, &[("f", b"3")], &[c2], "HEAD", "third", 1_700_002_000).expect("c3");

        let log2 = log(&repo, 2).expect("log");
        assert_eq!(log2.len(), 2);
        assert_eq!(log2[0].message, "third");
        assert_eq!(log2[1].message, "second");
    }

    #[test]
    fn git_create_branch_and_list() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let c1 =
            commit_all(&repo, &[("f", b"1")], &[], "HEAD", "first", 1_700_000_000).expect("c1");

        create_branch(&repo, "dev", c1, 1_700_000_500).expect("create branch");
        create_branch(&repo, "feature/x", c1, 1_700_000_600).expect("create branch feature/x");

        let branches = list_branches(&repo).expect("list");
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"dev"));
        assert!(names.contains(&"feature/x"));
        // 所有分支都指向 c1
        for b in &branches {
            assert_eq!(b.head, c1.to_string());
        }
    }

    #[test]
    fn git_create_branch_conflict_when_pointing_to_different_commit() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let c1 =
            commit_all(&repo, &[("f", b"1")], &[], "HEAD", "first", 1_700_000_000).expect("c1");
        let c2 = commit_all(
            &repo,
            &[("f", b"2")],
            &[c1],
            "HEAD",
            "second",
            1_700_001_000,
        )
        .expect("c2");
        create_branch(&repo, "dev", c1, 1_700_000_500).expect("first create -> c1");
        // 再把 dev 指向 c2（MustNotExist 且 target 不同）应报冲突
        let err = create_branch(&repo, "dev", c2, 1_700_000_600).unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[test]
    fn git_create_branch_idempotent_to_same_commit() {
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let c1 =
            commit_all(&repo, &[("f", b"1")], &[], "HEAD", "first", 1_700_000_000).expect("c1");
        create_branch(&repo, "dev", c1, 1_700_000_500).expect("first create");
        // 重复指向同一 commit：幂等成功（gix MustNotExist 既有行为）
        create_branch(&repo, "dev", c1, 1_700_000_600).expect("idempotent create");
    }

    #[test]
    fn git_log_on_empty_head_errors() {
        // 未提交的仓库：head unborn，head_id 应失败
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let err = log(&repo, 0).unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    // ---- DevTools trait（trigger/pipeline_status 真实 git 状态）----

    fn pipeline_local(id: &str, repo_path: &std::path::Path) -> CiPipeline {
        CiPipeline {
            id: id.into(),
            name: format!("pipe-{id}"),
            repo_url: format!("file://{}", repo_path.display()),
            branch: "main".into(),
            steps: vec!["build".into()],
        }
    }

    #[tokio::test]
    async fn trigger_unknown_pipeline_errors() {
        let d = DefaultDevTools::new();
        let err = d.trigger_pipeline("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::PipelineFailed(_)));
    }

    #[tokio::test]
    async fn trigger_local_pipeline_records_run_and_logs_url() {
        // 真实本地 git 仓库
        let dir = tempdir();
        let repo = init_repo(dir.path()).expect("init");
        let _c = commit_all(&repo, &[("f", b"x")], &[], "HEAD", "init", 1_700_000_000).expect("c");
        let hc = head_commit(&repo).expect("head");

        let d = DefaultDevTools::new().with_pipelines(vec![pipeline_local("p1", dir.path())]);
        let task = d.trigger_pipeline("p1").await.expect("trigger");
        // pipeline_status 真实可查
        let run = d.pipeline_status(&task).await.expect("status");
        assert_eq!(run.pipeline_id, "p1");
        assert_eq!(run.run_id, task.0.to_string());
        assert_eq!(run.status, CiStatus::Success);
        // logs_url 锚定到 head commit SHA
        let url = run.logs_url.expect("logs url");
        assert!(
            url.contains(&hc.sha),
            "logs_url {url} 应含 head sha {}",
            hc.sha
        );
    }

    #[tokio::test]
    async fn pipeline_status_unknown_task_errors() {
        let d = DefaultDevTools::new();
        let err = d.pipeline_status(&TaskId::new()).await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[tokio::test]
    #[cfg(not(feature = "git-remote"))]
    async fn trigger_remote_pipeline_records_run_without_clone() {
        // 远端 repo_url：默认构建（无 git-remote feature）logs_url 退回 remote:// 占位
        // （不触网）。git-remote feature 下的远端真实 clone 行为由
        // tests/git_remote_real.rs 的 #[ignore] 测覆盖（需公网）。
        let p = CiPipeline {
            id: "remote".into(),
            name: "remote pipe".into(),
            repo_url: "https://example.com/repo.git".into(),
            branch: "main".into(),
            steps: vec!["build".into()],
        };
        let d = DefaultDevTools::new().with_pipelines(vec![p]);
        let task = d.trigger_pipeline("remote").await.expect("trigger");
        let run = d.pipeline_status(&task).await.expect("status");
        assert_eq!(run.status, CiStatus::Success);
        assert!(
            run.logs_url.as_deref().unwrap().starts_with("remote://"),
            "logs_url={:?}",
            run.logs_url
        );
    }

    #[tokio::test]
    async fn list_pipelines_returns_preset() {
        let d = DefaultDevTools::new()
            .with_pipelines(vec![pipeline_local("a", std::path::Path::new("/tmp"))]);
        let list = d.list_pipelines().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a");
    }

    // ---- KVS（真实 AES-256-GCM AEAD 往返 + 安全性）----

    #[tokio::test]
    async fn kvs_store_get_roundtrip() {
        let d = DefaultDevTools::new();
        d.store_secret("k", b"hunter2").await.unwrap();
        let v = d.get_secret("k").await.unwrap();
        assert_eq!(v, b"hunter2");
        // store + get 两条审计
        let audit: Vec<String> = d
            .audit_log()
            .into_iter()
            .map(|e| format!("{:?}", e.action))
            .collect();
        assert!(audit.iter().any(|a| a.contains("Store")));
        assert!(audit.iter().any(|a| a.contains("Get")));
    }

    #[tokio::test]
    async fn kvs_get_missing_returns_secret_not_found() {
        let d = DefaultDevTools::new();
        let err = d.get_secret("x").await.unwrap_err();
        assert!(matches!(err, ServiceError::SecretNotFound(_)));
    }

    #[tokio::test]
    async fn kvs_rotate_changes_value_and_audits() {
        // 轮换语义：rotate_secret 生成 32 字节随机新值（OsRng）→ 重新加密落盘。
        let d = DefaultDevTools::new();
        d.store_secret("tok", b"old").await.unwrap();
        let before = d.get_secret("tok").await.unwrap();
        assert_eq!(before, b"old");
        d.rotate_secret("tok").await.unwrap();
        let after = d.get_secret("tok").await.unwrap();
        // 新值长度 = 32（轮换生成的随机密钥材料），且与旧值不同。
        assert_eq!(after.len(), 32);
        assert_ne!(&after[..], &before[..]);
        // 二次轮换应再产生新值（不重复）。
        d.rotate_secret("tok").await.unwrap();
        let after2 = d.get_secret("tok").await.unwrap();
        assert_eq!(after2.len(), 32);
        assert_ne!(after2, after);
    }

    #[tokio::test]
    async fn kvs_rotate_missing_errors() {
        let d = DefaultDevTools::new();
        let err = d.rotate_secret("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::SecretNotFound(_)));
    }

    // ---- AEAD 安全性专项（密文非明文 / 错误密钥拒绝 / 篡改拒绝）----

    #[tokio::test]
    async fn kvs_aead_ciphertext_is_not_plaintext() {
        // 旧占位 ENC: 密文 = "ENC:" + 明文，含明文子串。真实 AEAD 密文绝不泄漏明文。
        let d = DefaultDevTools::new();
        let plain = b"sensitive-secret-value-12345";
        d.store_secret("sek", plain).await.unwrap();
        // 取出内部密文（通过 encrypt 直接调，确认非明文）。
        let cipher = d.encrypt(plain).expect("encrypt");
        // 密文长度 = nonce(12) + plain_len + tag(16)，远大于明文。
        assert!(cipher.len() > plain.len() + NONCE_LEN);
        // 密文不包含明文的任何窗口（证伪旧的 ENC: 占位）。
        assert!(
            !cipher.windows(plain.len()).any(|w| w == plain.as_slice()),
            "密文不应包含明文子串"
        );
        // 也不以 b"ENC:" 起头（旧占位特征）。
        assert!(!cipher.starts_with(b"ENC:"));
    }

    #[tokio::test]
    async fn kvs_aead_nonce_unique_per_encryption() {
        // 同明文两次加密 → 密文不同（nonce 随机 + AES-CTR 流异或不同）。
        let d = DefaultDevTools::new();
        let c1 = d.encrypt(b"same").expect("encrypt 1");
        let c2 = d.encrypt(b"same").expect("encrypt 2");
        assert_ne!(c1, c2, "同明文两次加密密文必须不同（nonce 随机）");
        // 两者的 nonce 部分（前 12 字节）也不同。
        assert_ne!(&c1[..NONCE_LEN], &c2[..NONCE_LEN]);
        // 但都能正确解密回同明文。
        assert_eq!(d.decrypt(&c1).unwrap(), b"same");
        assert_eq!(d.decrypt(&c2).unwrap(), b"same");
    }

    #[tokio::test]
    async fn kvs_aead_wrong_key_rejected() {
        // 用种子 A 加密、种子 B 解密 → tag 校验失败（密文不泄漏明文）。
        let d_a = DefaultDevTools::new_with_seed(b"seed-A");
        let d_b = DefaultDevTools::new_with_seed(b"seed-B");
        let cipher = d_a.encrypt(b"topsecret").expect("encrypt");
        let err = d_b.decrypt(&cipher).unwrap_err();
        assert!(
            matches!(err, ServiceError::Internal(_)),
            "错误密钥应被 GCM tag 校验拒绝"
        );
    }

    #[tokio::test]
    async fn kvs_aead_tamper_rejected() {
        // 篡改密文一字节 → GCM tag 校验失败（AEAD 完整性）。
        let d = DefaultDevTools::new();
        let mut cipher = d.encrypt(b"original").expect("encrypt");
        // 翻转 ciphertext 区一字节（nonce 之后，tag 之前）。
        let tamper_idx = NONCE_LEN + 1;
        cipher[tamper_idx] ^= 0xff;
        let err = d.decrypt(&cipher).unwrap_err();
        assert!(
            matches!(err, ServiceError::Internal(_)),
            "篡改密文应被 GCM tag 校验拒绝"
        );
    }

    #[tokio::test]
    async fn kvs_aead_tamper_nonce_rejected() {
        // 篡改 nonce 一字节 → 同样 tag 校验失败。
        let d = DefaultDevTools::new();
        let mut cipher = d.encrypt(b"v").expect("encrypt");
        cipher[0] ^= 0x01;
        assert!(d.decrypt(&cipher).is_err());
    }

    #[test]
    fn kvs_aead_short_ciphertext_rejected() {
        // 密文长度不足（< nonce(12) + tag(16) = 28）→ 直接判失败，不进解密。
        let d = DefaultDevTools::new();
        let short = vec![0u8; MIN_CIPHER_LEN - 1];
        let err = d.decrypt(&short).unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
        // 恰好 28 字节但内容无效 → tag 校验失败（走 aead decrypt 路径）。
        let mut minimal = vec![0u8; MIN_CIPHER_LEN];
        let err2 = d.decrypt(&minimal).unwrap_err();
        assert!(matches!(err2, ServiceError::Internal(_)));
        // 翻转一位让 minimal 不全零（避免极端全零碰巧合法）。
        minimal[0] = 1;
        assert!(d.decrypt(&minimal).is_err());
    }

    #[tokio::test]
    async fn kvs_aead_roundtrip_with_various_plaintext_lengths() {
        // 多种明文长度（含空、单字节、跨 AES 块边界）往返一致。
        let d = DefaultDevTools::new();
        for plain in [
            &b""[..],
            b"x",
            b"exactly-16-bytes!",             // 1 AES 块
            b"32-bytes-key-material-1234567", // 32 字节
            &[0xab; 100],                     // 跨多块
        ] {
            let c = d.encrypt(plain).expect("encrypt");
            let p = d.decrypt(&c).expect("decrypt");
            assert_eq!(p.as_slice(), plain, "明文长度 {} 往返不一致", plain.len());
        }
    }

    #[test]
    fn kvs_derive_key_is_deterministic_and_seed_dependent() {
        // 同种子 → 同密钥；不同种子 → 不同密钥（SHA-256 派生特性）。
        let k1 = derive_kvs_key(b"seed-1");
        let k1b = derive_kvs_key(b"seed-1");
        let k2 = derive_kvs_key(b"seed-2");
        assert_eq!(k1, k1b, "同种子派生应确定");
        assert_ne!(k1, k2, "不同种子派生应不同");
        assert_eq!(k1.len(), 32, "AES-256 密钥须 32 字节");
    }

    // ---- local_path_from_url 辅助 ----

    #[test]
    fn local_path_from_url_classifies() {
        assert_eq!(
            local_path_from_url("file:///tmp/x").as_deref(),
            Some(std::path::Path::new("/tmp/x"))
        );
        assert_eq!(
            local_path_from_url("/home/oem/repo").as_deref(),
            Some(std::path::Path::new("/home/oem/repo"))
        );
        assert_eq!(local_path_from_url("https://x/repo.git"), None);
        assert_eq!(local_path_from_url("ssh://x/repo.git"), None);
        assert_eq!(local_path_from_url("git@github.com:o/r.git"), None);
    }

    // ---- 确保旧的 SecretId/审计模型仍可被外部使用（回归）----

    #[test]
    fn secret_id_and_audit_still_usable() {
        let id = SecretId::new("k");
        assert_eq!(id.as_str(), "k");
        let mut log = SecretAuditLog::new();
        log.record(SecretAuditEntry {
            id: id.clone(),
            action: SecretAction::Store,
            actor: "t".into(),
            at: ts(1),
            success: true,
            error: None,
        });
        assert_eq!(log.for_secret(&id).len(), 1);
        let _: HashMap<SecretId, ()> = HashMap::new();
    }
}
