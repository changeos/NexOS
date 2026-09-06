//! os-protocols —— 文件共享协议层契约 + 编排器（真实协议栈已接通）
//!
//! 定位（规划文档 §3.3）：
//! - SMB（编排 Samba）/ NFS（v3 编排 nfsserve，v4 编排 nfs-ganesha）/ WebDAV / FTP / SFTP
//! - 对象存储：S3 兼容（基于 RustFS）
//!
//! 协议栈现状：
//! - **WebDAV/FTP/SFTP**：已接通真实协议栈（`dav-server`/`libunftp`/`russh`，ADR-DEPS-002），
//!   不真监听端口（红线）；测试用离线 fixture 驱动真实协议处理器。
//! - **SMB/NFS**：协议栈由 C 实现（Samba / nfs-ganesha），Rust 侧做**编排**——生成配置
//!   （如 smb.conf / ganesha.conf，真实可用纯函数）、管理共享生命周期、监控会话。
//!
//! 模块布局：
//! - `common`：协议无关类型（`Protocol`/`Share`/`ShareOptions`/`Session`）+ `FileProtocol` 父 trait
//! - `smb`/`nfs`/`webdav`/`ftp`/`sftp`：各协议子 trait + 配置生成（纯函数渲染）
//! - `state`：共享生命周期状态机（`ShareState`）+ 内存 `ShareStore`
//! - `orchestrators`：5 个协议编排器（`SambaOrchestrator` 等，落 `FileProtocol` 到内存 +
//!   接通 WebDAV/FTP/SFTP 真实协议栈对象）
//! - `ftp_backend`：libunftp 的内存 `StorageBackend<DefaultUser>`（离线可测）
//! - `sftp_backend`：russh 的 `server::Server`/`Handler`（authorized_keys 公钥认证 + SFTP 子系统）
//! - `object`：对象存储契约（object-agent 拥有，本 agent 不改）
//! - `mock`（feature gate）：下游测试注入用 `MockXxxManager`
//!
//! 设计要点：
//! - [`FileProtocol`] 是统一抽象的父 trait（共享生命周期/会话管理）；
//!   各协议子 trait（[`SmbManager`] / [`NfsManager`] / [`WebDavManager`] / [`FtpManager`] /
//!   [`SftpManager`]）以 `pub trait X: FileProtocol` 继承
//! - 对象存储模型不同（bucket/object 而非 share），独立为 [`ObjectStore`] trait
//! - 所有数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`）
//!
//! # 关键 trait
//!
//! - [`FileProtocol`]：所有文件共享协议的父 trait（共享生命周期/会话管理/统计）。
//! - [`SmbManager`] / [`NfsManager`] / [`WebDavManager`] / [`FtpManager`] / [`SftpManager`]：
//!   各协议子 trait（继承 `FileProtocol`，附加协议特定配置生成）。
//! - [`ObjectStore`]：对象存储契约（bucket/object 模型，独立于文件共享）。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，导出 `MockSmbManager`/`MockNfsManager`/... 供下游测试注入。

#![allow(async_fn_in_trait)]

pub mod common;
pub mod error;
pub mod ftp;
pub mod ftp_backend;
pub mod nfs;
pub mod object;
pub mod orchestrators;
pub mod sftp;
pub mod sftp_backend;
pub mod smb;
pub mod state;
pub mod webdav;

// —— Mock（仅 `mock` feature；供下游 api-agent 测试注入）——
// 注：object-agent 用 object_mock，protocol-agent 用 mock，物理隔离避免冲突。
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
mod object_mock;

pub use common::{FileProtocol, Protocol, Session, Share, ShareId, ShareOptions};
pub use error::{ProtocolError, ProtocolResult};
pub use ftp::{FtpConfig, FtpManager};
pub use ftp_backend::{InMemoryFtpBackend, MemFtpMetadata};
pub use nfs::{
    GaneshaAccess, GaneshaClient, GaneshaConfig, GaneshaExport, GaneshaSquash, NfsClientExport,
    NfsExportOptions, NfsExportsEntry, NfsManager,
};
pub use object::{
    validate_bucket_name, AccessKey, Bucket, BucketPermission, CompleteMultipartUploadRequest,
    CreateMultipartUploadRequest, CreatedAccessKey, DeleteObjectRequest, GetObjectRequest,
    GetObjectResponse, LifecycleAction, LifecycleRule, ListObjectsRequest, ListObjectsResponse,
    ObjectMeta, ObjectStore, ObjectVersion, PutObjectRequest, PutObjectResponse, RustFsObjectStore,
    UploadedPart, VersioningConfig,
};
pub use orchestrators::{
    DavServerBackend, LibunftpBackend, NfsOrchestrator, ReloadPolicy, RusshSftpBackend,
    SambaOrchestrator,
};
pub use sftp::{SftpConfig, SftpManager};
pub use sftp_backend::{build_ssh_config, parse_pubkey_line, OsSshHandler, OsSshServer};
pub use smb::{render_smb_conf, SambaConfig, SambaShareSpec, SmbManager};
pub use state::{apply_options, ShareState, ShareStore};
pub use webdav::{WebDavConfig, WebDavManager};

// —— Mock 导出（object + protocol）——
#[cfg(feature = "mock")]
pub use mock::{
    MockFtpManager, MockNfsManager, MockSftpManager, MockSmbManager, MockWebDavManager,
};
#[cfg(feature = "mock")]
pub use object_mock::MockObjectStore;
