//! 对象存储（S3 兼容，基于 RustFS）
//!
//! 模型与文件共享（Share）不同——以 bucket / object / access key 为核心，
//! 故独立于 `FileProtocol`，定义独立的 `ObjectStore` trait。
//!
//! 本模块内容分四块：
//! 1. **数据模型**：[`Bucket`] / [`ObjectMeta`] / [`ObjectVersion`] / [`VersioningConfig`] /
//!    [`LifecycleRule`] / [`AccessKey`] / [`BucketPermission`] 等——含构造器与校验
//!    （bucket 命名规则：小写、3-63 字符、DNS 兼容）。
//! 2. **操作模型**：[`PutObjectRequest`]/[`PutObjectResponse`] / [`GetObjectRequest`]/
//!    [`GetObjectResponse`] / [`DeleteObjectRequest`] / [`ListObjectsRequest`]/
//!    [`ListObjectsResponse`] / 多段上传 [`CreateMultipartUploadRequest`] 等。
//! 3. **SigV4 签名字符串构造**：[`sigv4`] 子模块——`canonical_request` / `string_to_sign` /
//!    `credential_scope` 等纯字符串逻辑（不依赖外部 HTTP/加密 crate，可单测）。
//! 4. **`ObjectStore` trait 与默认实现骨架**：[`RustFsObjectStore`]——方法签名完整，
//!    真实 RustFS / HTTP 调用留 `TODO`（RustFS 客户端尚未在 workspace 注册）。
//!    所有 `TODO` 标记均属 **\[RUNTIME\]** 类——需 RustFS 客户端 + reqwest HTTP 栈 + sigv4
//!    HMAC 实跑（workspace 未在本 crate 注册 reqwest/hmac），逻辑骨架与命名校验已就绪。

use bytes::Bytes;
use os_core::{Deserialize, PageRequest, PageResponse, Serialize};

use crate::ProtocolResult;

// ----------------------------------------------------------------------------
// bucket / object / version
// ----------------------------------------------------------------------------

/// 对象存储 bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bucket {
    /// bucket 名（S3 命名规则：全局唯一小写）
    pub name: String,
    /// 创建时间
    pub created: chrono::DateTime<chrono::Utc>,
    /// 是否启用版本控制
    pub versioning: bool,
    /// 对象数量
    pub object_count: u64,
}

impl Bucket {
    /// 构造一个新 bucket——名称先经 [`validate_bucket_name`] 校验。
    ///
    /// `versioning` 默认关闭；`object_count` 初始 0。
    pub fn new(name: impl Into<String>) -> ProtocolResult<Self> {
        let name = name.into();
        validate_bucket_name(&name)?;
        Ok(Self {
            name,
            created: os_core::Utc::now(),
            versioning: false,
            object_count: 0,
        })
    }
}

/// 对象版本（启用 versioning 时存在）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectVersion {
    /// 版本 ID
    pub version_id: String,
    /// 该版本大小（字节）
    pub size: u64,
    /// 最后修改时间
    pub last_modified: chrono::DateTime<chrono::Utc>,
    /// 是否为删除标记（delete marker）
    pub deleted: bool,
}

impl ObjectVersion {
    /// 构造一个普通（非 delete marker）版本。
    pub fn new(version_id: impl Into<String>, size: u64) -> Self {
        Self {
            version_id: version_id.into(),
            size,
            last_modified: os_core::Utc::now(),
            deleted: false,
        }
    }

    /// 构造一个删除标记（delete marker）版本——`size` 为 0、`deleted` 为 true。
    pub fn delete_marker(version_id: impl Into<String>) -> Self {
        Self {
            version_id: version_id.into(),
            size: 0,
            last_modified: os_core::Utc::now(),
            deleted: true,
        }
    }

    /// 是否为删除标记。
    pub fn is_delete_marker(&self) -> bool {
        self.deleted
    }
}

/// 对象元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMeta {
    /// 所属 bucket
    pub bucket: String,
    /// 对象 key（完整路径）
    pub key: String,
    /// 大小（字节）
    pub size: u64,
    /// ETag（MD5 / 多段上传合成）
    pub etag: String,
    /// Content-Type
    pub content_type: String,
    /// 最后修改时间
    pub last_modified: chrono::DateTime<chrono::Utc>,
    /// 所有版本（versioning 关闭时为空或单元素）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<ObjectVersion>,
}

impl ObjectMeta {
    /// 构造对象元数据——`etag` 由调用方算好后传入（实现侧重算，模型层只承载）。
    pub fn new(
        bucket: impl Into<String>,
        key: impl Into<String>,
        size: u64,
        etag: impl Into<String>,
        content_type: impl Into<String>,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            size,
            etag: etag.into(),
            content_type: content_type.into(),
            last_modified: os_core::Utc::now(),
            versions: Vec::new(),
        }
    }

    /// 推入一个历史版本（仅 versioning 开启时调用方使用）。
    pub fn with_version(mut self, v: ObjectVersion) -> Self {
        self.versions.push(v);
        self
    }
}

// ----------------------------------------------------------------------------
// versioning / lifecycle 配置
// ----------------------------------------------------------------------------

/// bucket 版本控制配置（S3 `<VersioningConfiguration>` 的 Rust 映射）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersioningConfig {
    /// 是否启用版本控制（S3 三态：Enabled / Suspended / 未配置；此处用 bool 折叠）。
    pub enabled: bool,
    /// 是否同时启用 MFA Delete（S3 可选项，默认 false）。
    pub mfa_delete: bool,
}

impl VersioningConfig {
    /// 构造一个关闭版本控制的默认配置。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            mfa_delete: false,
        }
    }

    /// 构造一个启用版本控制的配置。
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            mfa_delete: false,
        }
    }
}

/// 生命周期规则的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    /// 转换到指定存储类（如 GLACIER）。
    Transition,
    /// 过期删除对象（含删除标记清理）。
    Expiration,
    /// 清理未完成的多段上传。
    AbortMultipartUpload,
}

/// 单条生命周期规则（S3 `<Rule>` 的精简版）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRule {
    /// 规则 ID（可选，缺省自动生成）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 触发的对象 key 前缀（空表示匹配全部）。
    pub prefix: String,
    /// 是否启用本规则。
    pub enabled: bool,
    /// 触发的动作。
    pub action: LifecycleAction,
    /// 触发天数（对象创建后第 N 天）。
    pub days: u32,
}

impl LifecycleRule {
    /// 构造一条过期删除规则。
    pub fn expire(prefix: impl Into<String>, days: u32) -> Self {
        Self {
            id: None,
            prefix: prefix.into(),
            enabled: true,
            action: LifecycleAction::Expiration,
            days,
        }
    }

    /// 构造一条转换存储类规则。
    pub fn transition(prefix: impl Into<String>, days: u32) -> Self {
        Self {
            id: None,
            prefix: prefix.into(),
            enabled: true,
            action: LifecycleAction::Transition,
            days,
        }
    }

    /// 设置规则 ID（builder 风格）。
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// 关闭本规则（builder 风格）。
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

// ----------------------------------------------------------------------------
// access key / 权限
// ----------------------------------------------------------------------------

/// bucket 级权限
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketPermission {
    /// 只读
    Read,
    /// 读写
    Write,
    /// 管理员（含 bucket 配置/删除）
    Admin,
}

impl BucketPermission {
    /// 是否包含读权限（Read/Write/Admin 都允许读）。
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::Write | Self::Admin)
    }

    /// 是否包含写权限（Write/Admin）。
    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write | Self::Admin)
    }

    /// 是否包含管理员权限（Admin）。
    pub fn allows_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// 对象存储 access key（S3 凭证）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessKey {
    /// access key id（公开标识）
    pub access_key_id: String,
    /// secret 的哈希（明文 secret 仅在创建时返回一次，不存储）
    pub secret_hash: String,
    /// 授权的 bucket 权限列表
    pub permissions: Vec<BucketPermission>,
    /// 创建时间
    pub created: chrono::DateTime<chrono::Utc>,
}

impl AccessKey {
    /// 构造 access key——`secret_hash` 由调用方算好（实现层用 Argon2/bcrypt）传入。
    pub fn new(
        access_key_id: impl Into<String>,
        secret_hash: impl Into<String>,
        permissions: Vec<BucketPermission>,
    ) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_hash: secret_hash.into(),
            permissions,
            created: os_core::Utc::now(),
        }
    }
}

/// `create_access_key` 的完整返回——包含**仅此一次可见**的明文 secret。
///
/// 安全约定（见规格书 §8/§9 红线）：明文 `secret` 仅在此结构中返回一次，
/// 调用方须立即安全转交用户；`access_key` 字段（含 `secret_hash`）方可持久化。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedAccessKey {
    /// 可持久化的 access key 元信息（含 secret_hash，不含明文 secret）。
    pub access_key: AccessKey,
    /// 明文 secret——**仅创建时返回一次**，调用方不得落日志/不得持久化明文。
    #[serde(skip_serializing)]
    pub secret: String,
}

// ----------------------------------------------------------------------------
// 校验：bucket 命名规则
// ----------------------------------------------------------------------------

/// 校验 bucket 名称是否符合 S3 命名规则。
///
/// 规则（参考 AWS S3 文档）：
/// - 长度 3-63 字符；
/// - 只能含小写字母 `a-z`、数字 `0-9`、连字符 `-`、点 `.`；
/// - 必须以字母或数字开头和结尾；
/// - 不能是 IP 地址格式（如 `192.168.1.1`）——四个点分段且每段全数字视为非法；
/// - 不能含两个连续点 `..`；
/// - 不能含连字符-点相邻（`-.` / `.-`）。
///
/// 返回 `Err(ProtocolError::CommandFailed)` 包装校验失败原因（沿用 CommandFailed，
/// 因本 crate 无专门 `InvalidBucketName` variant；新增 variant 须 ADR + 会签 protocol-agent）。
pub fn validate_bucket_name(name: &str) -> ProtocolResult<()> {
    let len = name.len();
    if !(3..=63).contains(&len) {
        return Err(crate::ProtocolError::CommandFailed(format!(
            "bucket 名长度须为 3-63 字符，实际 {len}"
        )));
    }
    // 字符集 + 开头/结尾
    let mut chars = name.chars();
    let first = chars.next().expect("len>=3 已保证非空");
    let last = name.chars().last().expect("len>=3 已保证非空");
    if !first.is_ascii_alphanumeric() {
        return Err(crate::ProtocolError::CommandFailed(
            "bucket 名须以字母或数字开头".into(),
        ));
    }
    if !last.is_ascii_alphanumeric() {
        return Err(crate::ProtocolError::CommandFailed(
            "bucket 名须以字母或数字结尾".into(),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.') {
            return Err(crate::ProtocolError::CommandFailed(format!(
                "bucket 名含非法字符 {c:?}（仅允许小写字母/数字/连字符/点）"
            )));
        }
    }
    // 不允许大写（前面字符集已排除小写之外的字母，这里显式兜底）
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(crate::ProtocolError::CommandFailed(
            "bucket 名不允许大写字母".into(),
        ));
    }
    // 连续点 / 点-连字符相邻
    if name.contains("..") {
        return Err(crate::ProtocolError::CommandFailed(
            "bucket 名不允许连续两个点".into(),
        ));
    }
    if name.contains("-.") || name.contains(".-") {
        return Err(crate::ProtocolError::CommandFailed(
            "bucket 名不允许连字符与点相邻".into(),
        ));
    }
    // IP 地址格式：4 段全数字点分
    let dotted: Vec<&str> = name.split('.').collect();
    if dotted.len() == 4
        && dotted
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(crate::ProtocolError::CommandFailed(
            "bucket 名不能是 IP 地址格式".into(),
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 操作模型：PutObject / GetObject / DeleteObject / ListObjects / Multipart
// ----------------------------------------------------------------------------

/// `PutObject` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutObjectRequest {
    /// 目标 bucket。
    pub bucket: String,
    /// 对象 key。
    pub key: String,
    /// 对象内容。
    #[serde(skip)]
    pub data: Bytes,
    /// Content-Type（None 时由实现层推断/默认 `application/octet-stream`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl PutObjectRequest {
    /// 构造 PutObject 请求。
    pub fn new(bucket: impl Into<String>, key: impl Into<String>, data: Bytes) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            data,
            content_type: None,
        }
    }

    /// 设置 Content-Type（builder 风格）。
    #[must_use]
    pub fn with_content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = Some(ct.into());
        self
    }
}

/// `PutObject` 响应——返回对象元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutObjectResponse {
    /// 上传后对象的元数据。
    pub meta: ObjectMeta,
}

/// `GetObject` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetObjectRequest {
    /// 所属 bucket。
    pub bucket: String,
    /// 对象 key。
    pub key: String,
    /// 版本 ID（versioning 开启时指定具体版本；None 取最新）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// 可选 Range 起（字节偏移，含）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_start: Option<u64>,
    /// 可选 Range 止（字节偏移，含）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_end: Option<u64>,
}

impl GetObjectRequest {
    /// 构造 GetObject 请求（取最新版本、无 Range）。
    pub fn new(bucket: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            version_id: None,
            range_start: None,
            range_end: None,
        }
    }

    /// 指定版本 ID。
    #[must_use]
    pub fn with_version(mut self, version_id: impl Into<String>) -> Self {
        self.version_id = Some(version_id.into());
        self
    }

    /// 指定字节范围 `[start, end]`（含两端）。
    #[must_use]
    pub fn with_range(mut self, start: u64, end: u64) -> Self {
        self.range_start = Some(start);
        self.range_end = Some(end);
        self
    }
}

/// `GetObject` 响应——返回内容与元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetObjectResponse {
    /// 对象元数据。
    pub meta: ObjectMeta,
    /// 对象内容。
    #[serde(skip)]
    pub data: Bytes,
}

/// `DeleteObject` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteObjectRequest {
    /// 所属 bucket。
    pub bucket: String,
    /// 对象 key。
    pub key: String,
    /// 版本 ID（versioning 开启时，须指定版本以物理删除或写 delete marker）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// 是否绕过版本控制直接物理删除（实现层按需；默认 false = 写 delete marker）。
    ///
    /// 对应 S3 `x-amz-bypass-governance-retention` 头。
    pub bypass_governance_retention: bool,
}

impl DeleteObjectRequest {
    /// 构造 DeleteObject 请求（默认非 bypass）。
    pub fn new(bucket: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            version_id: None,
            bypass_governance_retention: false,
        }
    }
}

/// `ListObjects`（S3 ListObjectsV2 风格）请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListObjectsRequest {
    /// 所属 bucket。
    pub bucket: String,
    /// key 前缀过滤（空表示列出全部）。
    pub prefix: String,
    /// 分页参数。
    pub page: PageRequest,
    /// 续页 token（来自上一页响应的 `next_continuation_token`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
    /// 单页最大条目数（None = 用 `page.limit` 或服务端默认 1000）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_keys: Option<u32>,
}

impl ListObjectsRequest {
    /// 构造 ListObjects 请求。
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>, page: PageRequest) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: prefix.into(),
            page,
            continuation_token: None,
            max_keys: None,
        }
    }
}

/// `ListObjects` 响应（S3 ListObjectsV2 风格，但用 PageResponse 统一分页）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListObjectsResponse {
    /// 命中的对象元数据。
    pub items: Vec<ObjectMeta>,
    /// 总数（服务端 IsTruncated 之前的全量计数）。
    pub total: u32,
    /// 当前页偏移。
    pub offset: u32,
    /// 当前页 limit。
    pub limit: u32,
    /// 下一页 token（None 表示已到末尾）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_continuation_token: Option<String>,
}

// ----------------------------------------------------------------------------
// 多段上传（Multipart Upload）
// ----------------------------------------------------------------------------

/// `CreateMultipartUpload` 请求——初始化一次多段上传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMultipartUploadRequest {
    /// 所属 bucket。
    pub bucket: String,
    /// 对象 key。
    pub key: String,
    /// Content-Type。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl CreateMultipartUploadRequest {
    /// 构造多段上传初始化请求。
    pub fn new(bucket: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            content_type: None,
        }
    }
}

/// 单个已上传分段的元信息（用于 Complete 时拼装 parts 列表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedPart {
    /// 分段号（1-10000）。
    pub part_number: u32,
    /// 该分段的 ETag（服务端上传完成后返回）。
    pub etag: String,
    /// 该分段大小（字节）。
    pub size: u64,
}

/// `UploadPart` / `CompleteMultipartUpload` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteMultipartUploadRequest {
    /// 所属 bucket。
    pub bucket: String,
    /// 对象 key。
    pub key: String,
    /// 多段上传 ID（CreateMultipartUpload 时获得）。
    pub upload_id: String,
    /// 所有已上传分段（按 part_number 升序）。
    pub parts: Vec<UploadedPart>,
}

impl CompleteMultipartUploadRequest {
    /// 构造 Complete 请求。
    pub fn new(
        bucket: impl Into<String>,
        key: impl Into<String>,
        upload_id: impl Into<String>,
        parts: Vec<UploadedPart>,
    ) -> Self {
        Self {
            bucket: bucket.into(),
            key: key.into(),
            upload_id: upload_id.into(),
            parts,
        }
    }
}

// ----------------------------------------------------------------------------
// ObjectStore trait（async，S3 兼容）
// ----------------------------------------------------------------------------

/// S3 兼容对象存储——基于 RustFS。
///
/// 实现说明：bucket/object 操作走 S3 API；access key 由内置 IAM 颁发。
/// `create_access_key` 返回的明文 secret 仅此一次可见，调用方须安全转交用户。
#[allow(async_fn_in_trait)]
pub trait ObjectStore: Send + Sync {
    /// 创建 bucket。
    async fn create_bucket(&self, name: &str) -> ProtocolResult<Bucket>;

    /// 删除 bucket（须为空或强制）。
    async fn delete_bucket(&self, name: &str) -> ProtocolResult<()>;

    /// 列出所有 bucket。
    async fn list_buckets(&self) -> ProtocolResult<Vec<Bucket>>;

    /// 上传对象（返回对象元数据）。
    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Bytes,
        content_type: Option<String>,
    ) -> ProtocolResult<ObjectMeta>;

    /// 下载对象内容。
    async fn get_object(&self, bucket: &str, key: &str) -> ProtocolResult<Bytes>;

    /// 删除对象（versioning 时写入 delete marker）。
    async fn delete_object(&self, bucket: &str, key: &str) -> ProtocolResult<()>;

    /// 列举对象（按前缀分页）。
    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        page: PageRequest,
    ) -> ProtocolResult<PageResponse<ObjectMeta>>;

    /// 创建 access key（返回含明文 secret 一次）。
    async fn create_access_key(
        &self,
        permissions: Vec<BucketPermission>,
    ) -> ProtocolResult<AccessKey>;

    /// 吊销 access key。
    async fn delete_access_key(&self, access_key_id: &str) -> ProtocolResult<()>;
}

// ----------------------------------------------------------------------------
// RustFsObjectStore 骨架（真实 RustFS / HTTP 调用留 TODO [RUNTIME]）
// ----------------------------------------------------------------------------

/// 基于 RustFS 的 `ObjectStore` 默认实现（骨架）。
///
/// **当前状态（批 2 骨架）**：方法签名完整，真实 S3 API / RustFS 客户端调用留
/// `TODO` \[RUNTIME\]——RustFS 客户端尚未在 workspace 注册（见红线「不虚构依赖」）。每个方法
/// 返回 `Err(ProtocolError::Internal(...))` 表明未接通，便于下游编译期消费 trait。
/// **下游测试请用 `crate::MockObjectStore`**（`mock` feature，纯内存、不依赖 HTTP）。
///
/// 接通路径（后续批次）：
/// - bucket/object 操作走标准 S3 REST（`PUT /{bucket}` / `GET /{bucket}` /
///   `PUT /{bucket}/{key}` / `GET /{bucket}/{key}` / `DELETE ...` / `GET ?list-type=2`）；
/// - 请求经 [`sigv4`] 签名后发出；
/// - access key 由内置 IAM 颁发（`create_access_key` 生成明文 secret 后 Argon2 哈希）。
pub struct RustFsObjectStore {
    /// RustFS 端点（如 `http://127.0.0.1:9000`）。
    endpoint: String,
    /// 默认 region（S3 兼容通常 `us-east-1`）。
    region: String,
    /// 是否使用 path-style（RustFS / MinIO 默认 true；AWS S3 默认 virtual-host-style）。
    path_style: bool,
}

impl RustFsObjectStore {
    /// 构造 RustFS 后端——endpoint 末尾自动去 `/`。
    pub fn new(endpoint: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            region: region.into(),
            path_style: true,
        }
    }

    /// 当前 endpoint（含测试用）。
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 当前 region。
    pub fn region(&self) -> &str {
        &self.region
    }

    /// 设置 path-style（默认 true）。
    #[must_use]
    pub fn with_path_style(mut self, path_style: bool) -> Self {
        self.path_style = path_style;
        self
    }

    /// 构造对象 URL——path-style（默认）：`{endpoint}/{bucket}/{key}`；
    /// virtual-host-style：`{scheme}://{bucket}.{host}/{key}`。
    ///
    /// 纯字符串逻辑，可测——真实 HTTP 请求由后续批次接通。
    pub fn object_url(&self, bucket: &str, key: &str) -> String {
        if self.path_style {
            format!("{}/{}/{}", self.endpoint, bucket, key)
        } else {
            // virtual-host-style：把 bucket 作为 host 的子域名。
            let (scheme, host) = self
                .endpoint
                .strip_prefix("https://")
                .map(|h| ("https", h))
                .or_else(|| self.endpoint.strip_prefix("http://").map(|h| ("http", h)))
                .unwrap_or(("http", self.endpoint.as_str()));
            format!("{scheme}://{bucket}.{host}/{key}")
        }
    }
}

impl Default for RustFsObjectStore {
    fn default() -> Self {
        Self::new("http://127.0.0.1:9000", "us-east-1")
    }
}

impl ObjectStore for RustFsObjectStore {
    // ⚠️ 以下所有方法内的 `// TODO` 均属 [RUNTIME] 类——需 RustFS 客户端 + reqwest HTTP 栈
    //    + sigv4 HMAC + Argon2（workspace 未在本 crate 注册）。骨架返回 Internal 表未接通。

    async fn create_bucket(&self, name: &str) -> ProtocolResult<Bucket> {
        // 先做本地命名校验（与 S3 服务端规则一致），尽早暴露错误。
        validate_bucket_name(name)?;
        // TODO(batch>2): PUT {endpoint}/{name} 经 sigv4 签名发出；
        //   2xx → 返回 Bucket；409 → BucketExists(沿用 CommandFailed)；4xx → AccessDenied。
        let _ = self.endpoint;
        Err(crate::ProtocolError::Internal(format!(
            "RustFsObjectStore::create_bucket 未接通（bucket={name}）"
        )))
    }

    async fn delete_bucket(&self, name: &str) -> ProtocolResult<()> {
        // TODO: DELETE {endpoint}/{name}；404 → BucketNotFound；409 BucketNotEmpty → CommandFailed。
        let _ = name;
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::delete_bucket 未接通".into(),
        ))
    }

    async fn list_buckets(&self) -> ProtocolResult<Vec<Bucket>> {
        // TODO: GET {endpoint} 解析 <Buckets> 列表。
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::list_buckets 未接通".into(),
        ))
    }

    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Bytes,
        content_type: Option<String>,
    ) -> ProtocolResult<ObjectMeta> {
        // TODO: PUT {object_url} with body=data, Content-Type, x-amz-* headers;
        //   响应 ETag → ObjectMeta；404 bucket → BucketNotFound。
        let _ = (bucket, key, data, content_type);
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::put_object 未接通".into(),
        ))
    }

    async fn get_object(&self, bucket: &str, key: &str) -> ProtocolResult<Bytes> {
        // TODO: GET {object_url}; 404 → ObjectNotFound；返回 body: Bytes。
        let _ = (bucket, key);
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::get_object 未接通".into(),
        ))
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> ProtocolResult<()> {
        // TODO: DELETE {object_url}; versioning 开启时服务端写 delete marker。
        let _ = (bucket, key);
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::delete_object 未接通".into(),
        ))
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        page: PageRequest,
    ) -> ProtocolResult<PageResponse<ObjectMeta>> {
        // TODO: GET {endpoint}/{bucket}?list-type=2&prefix=&continuation-token=&max-keys=;
        //   解析 <Contents> → Vec<ObjectMeta>，包成 PageResponse。
        let _ = (bucket, prefix, page);
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::list_objects 未接通".into(),
        ))
    }

    async fn create_access_key(
        &self,
        permissions: Vec<BucketPermission>,
    ) -> ProtocolResult<AccessKey> {
        // 安全要点：明文 secret 仅本次返回，不落日志；持久化只存 secret_hash。
        // TODO: 生成 {access_key_id, secret}（随机 20/40 字符），Argon2 哈希 secret,
        //   持久化 (access_key_id, secret_hash, permissions)；返回 AccessKey（含 hash）。
        let _ = permissions;
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::create_access_key 未接通".into(),
        ))
    }

    async fn delete_access_key(&self, access_key_id: &str) -> ProtocolResult<()> {
        // TODO: DELETE /iam/accesskey/{id}（内置 IAM）；404 → CommandFailed。
        let _ = access_key_id;
        Err(crate::ProtocolError::Internal(
            "RustFsObjectStore::delete_access_key 未接通".into(),
        ))
    }
}

// ----------------------------------------------------------------------------
// SigV4 签名字符串构造（纯字符串逻辑，可单测）
// ----------------------------------------------------------------------------

/// AWS Signature Version 4 签名辅助——纯字符串构造，**不含** HMAC/Hashing
/// （真正的 HMAC-SHA256 由实现层用 `sha2`/`hmac` crate 完成，待引入时补）。
///
/// 本模块实现「字符串构造」三件套：
/// - `canonical_request`：拼装 CanonicalRequest（HTTP 方法 / URI / query / headers / hash）；
/// - `credential_scope`：`{date}/{region}/{service}/aws4_request`；
/// - `string_to_sign`：`AWS4-HMAC-SHA256\n{date}\n{scope}\n{hashed_canonical_request}`。
///
/// 这些是确定性纯函数，故全部单测覆盖。
///
/// **黄金标准验证**：本模块的字符串构造经 AWS 官方 sig-v4 test suite
/// （`saibotsivad/aws-sig-v4-test-suite` 镜像，凭证 `AKIDEXAMPLE`）端到端验证——
/// 见 `tests/object_http_real.rs`（默认跑，对 `get-vanilla` /
/// `get-vanilla-query-order-key-case` / `get-header-value-trim` 三用例验证
/// canonical_request / string_to_sign / signing_key / 最终 signature 与 AWS 完全一致）。
pub mod sigv4 {
    /// 构造 SigV4 credential scope：`{YYYYMMDD}/{region}/{service}/aws4_request`。
    ///
    /// `date` 须为 `YYYYMMDD`（调用方从 `Utc::now().format("%Y%m%d")` 取）。
    pub fn credential_scope(date: &str, region: &str, service: &str) -> String {
        format!("{date}/{region}/{service}/aws4_request")
    }

    /// 一对规范化 header（name, value）。
    pub type CanonicalHeader = (String, String);

    /// 构造 SigV4 canonical request（不含签名本身，仅字符串）。
    ///
    /// 格式（每行以 `\n` 分隔）：
    /// ```text
    /// {HTTPMethod}\n
    /// {CanonicalURI}\n
    /// {CanonicalQueryString}\n
    /// {CanonicalHeaders}\n\n
    /// {SignedHeaders}\n
    /// {HashedPayload}
    /// ```
    ///
    /// - `canonical_uri`：URL 编码后的资源路径，以 `/` 开头；空路径用 `/`。
    /// - `canonical_query`：查询参数按 key 字典序升序，`k=v` 以 `&` 连，URL 编码。
    /// - `canonical_headers`：headers 按 name 小写排序，每行 `name:value\n`（value 去首尾空白）；
    ///   末尾紧跟一个空行。
    /// - `signed_headers`：上述 header names 以 `;` 连，小写字典序。
    /// - `hashed_payload`：调用方算好的十六进制 SHA256（未签名时用 `UNSIGNED-PAYLOAD`）。
    pub fn canonical_request(
        method: &str,
        canonical_uri: &str,
        canonical_query: &str,
        headers: &[CanonicalHeader],
        hashed_payload: &str,
    ) -> String {
        // AWS SigV4 规范化 header 值：①去首尾空白；②把连续空格折叠为单个空格。
        //   （AWS 官方 sig-v4 test suite 的 get-header-value-trim 用例锁此行为：
        //    `"a   b   c"` → `"a b c"`。仅 `trim()` 会漏掉内部折叠——签名会错。）
        let mut sorted: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), normalize_header_value(v)))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let mut canonical_headers = String::new();
        let mut signed_parts: Vec<String> = Vec::new();
        for (k, v) in &sorted {
            canonical_headers.push_str(k);
            canonical_headers.push(':');
            canonical_headers.push_str(v);
            canonical_headers.push('\n');
            signed_parts.push(k.clone());
        }
        let signed_headers = signed_parts.join(";");

        format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{hashed_payload}"
        )
    }

    /// 构造 SigV4 string to sign。
    ///
    /// 格式：
    /// ```text
    /// AWS4-HMAC-SHA256\n
    /// {ISO8601_basic_date_time}\n
    /// {credential_scope}\n
    /// {hex(sha256(canonical_request))}
    /// ```
    ///
    /// 注意：`hashed_canonical_request` 由调用方算好（本模块不引 sha2 crate），
    /// 便于在不引依赖的前提下对前两行做确定性单测。
    pub fn string_to_sign(
        amz_date: &str,
        credential_scope: &str,
        hashed_canonical_request: &str,
    ) -> String {
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{hashed_canonical_request}")
    }

    /// 规范化查询字符串——把 `[(k, v)]` 排序并 URL 编码（RFC 3986 unreserved 之外全编码）。
    pub fn canonical_query_string(params: &[(&str, &str)]) -> String {
        let mut pairs: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
            .collect();
        pairs.sort();
        pairs
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// 规范化 header 值：去首尾空白 + 把内部连续空格折叠为单个空格。
    ///
    /// AWS SigV4 规范（见 AWS 官方 sig-v4 test suite `get-header-value-trim` 用例）：
    /// > Trim any leading or trailing spaces.
    /// > Convert sequential spaces to a single space.
    ///
    /// 仅对 ASCII 空格 `0x20` 折叠（不对 `\t`/`\n` 等其它空白做处理——AWS
    /// 规范的 "space" 专指 0x20；其它控制字符应留在值中或被 HTTP 层拒绝）。
    fn normalize_header_value(v: &str) -> String {
        let trimmed = v.trim_matches(|c: char| c == ' ');
        let mut out = String::with_capacity(trimmed.len());
        let mut prev_space = false;
        for c in trimmed.chars() {
            if c == ' ' {
                if !prev_space {
                    out.push(' ');
                }
                prev_space = true;
            } else {
                out.push(c);
                prev_space = false;
            }
        }
        out
    }

    /// RFC 3986 编码——`encode_unreserved=true` 时 query 参数值也编码子分隔符。
    fn uri_encode(s: &str, encode_slash: bool) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                b'/' if !encode_slash => out.push('/'),
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}

// ----------------------------------------------------------------------------
// 单元测试（纯逻辑：bucket 校验 / 构造器 / sigv4 字符串构造）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // —— bucket name 校验 ——

    #[test]
    fn valid_bucket_names() {
        for ok in [
            "my-bucket",
            "abc",
            "ab1",
            "a.b.c",
            "bucket.123",
            "a".repeat(63).as_str(),
            "data-2024",
        ] {
            assert!(
                validate_bucket_name(ok).is_ok(),
                "expected {ok:?} to be valid"
            );
        }
    }

    #[test]
    fn reject_too_short() {
        assert!(validate_bucket_name("ab").is_err()); // 2 字符
        assert!(validate_bucket_name("").is_err()); // 空
    }

    #[test]
    fn reject_too_long() {
        let name = "a".repeat(64);
        assert!(validate_bucket_name(&name).is_err());
    }

    #[test]
    fn reject_uppercase() {
        assert!(validate_bucket_name("MyBucket").is_err());
    }

    #[test]
    fn reject_bad_chars() {
        assert!(validate_bucket_name("my_bucket").is_err()); // 下划线
        assert!(validate_bucket_name("my bucket").is_err()); // 空格
        assert!(validate_bucket_name("my$bucket").is_err());
    }

    #[test]
    fn reject_leading_trailing_punct() {
        assert!(validate_bucket_name("-bucket").is_err()); // 开头连字符
        assert!(validate_bucket_name("bucket-").is_err()); // 结尾连字符
        assert!(validate_bucket_name(".bucket").is_err()); // 开头点
        assert!(validate_bucket_name("bucket.").is_err()); // 结尾点
    }

    #[test]
    fn reject_consecutive_dots() {
        assert!(validate_bucket_name("my..bucket").is_err());
    }

    #[test]
    fn reject_dot_dash_adjacent() {
        assert!(validate_bucket_name("my-.bucket").is_err());
        assert!(validate_bucket_name("my.-bucket").is_err());
    }

    #[test]
    fn reject_ip_format() {
        assert!(validate_bucket_name("192.168.1.1").is_err());
        assert!(validate_bucket_name("10.0.0.1").is_err());
    }

    #[test]
    fn dotted_non_ip_is_ok() {
        // 4 段但非全数字 → 允许
        assert!(validate_bucket_name("a.b.c.d").is_ok());
        // 3 段全数字也不是 IP（S3 仅禁 4 段点分）
        assert!(validate_bucket_name("1.2.3").is_ok());
    }

    // —— 构造器 ——

    #[test]
    fn bucket_new_validates_and_inits() {
        let b = Bucket::new("photos-2024").unwrap();
        assert_eq!(b.name, "photos-2024");
        assert!(!b.versioning);
        assert_eq!(b.object_count, 0);
    }

    #[test]
    fn bucket_new_rejects_invalid() {
        assert!(Bucket::new("UPPER").is_err());
    }

    #[test]
    fn object_meta_new_basic() {
        let m = ObjectMeta::new("b", "k.txt", 5, "etag1", "text/plain");
        assert_eq!(m.bucket, "b");
        assert_eq!(m.key, "k.txt");
        assert_eq!(m.size, 5);
        assert_eq!(m.etag, "etag1");
        assert_eq!(m.content_type, "text/plain");
        assert!(m.versions.is_empty());
    }

    #[test]
    fn object_version_delete_marker() {
        let v = ObjectVersion::delete_marker("v1");
        assert!(v.is_delete_marker());
        assert_eq!(v.size, 0);
    }

    #[test]
    fn bucket_permission_flags() {
        assert!(BucketPermission::Read.allows_read());
        assert!(!BucketPermission::Read.allows_write());
        assert!(BucketPermission::Write.allows_read());
        assert!(BucketPermission::Write.allows_write());
        assert!(!BucketPermission::Write.allows_admin());
        assert!(BucketPermission::Admin.allows_admin());
    }

    #[test]
    fn lifecycle_rule_builders() {
        let r = LifecycleRule::expire("logs/", 30).with_id("r1");
        assert_eq!(r.action, LifecycleAction::Expiration);
        assert_eq!(r.days, 30);
        assert_eq!(r.id.as_deref(), Some("r1"));
        assert!(r.enabled);

        let r2 = LifecycleRule::transition("archive/", 90).disabled();
        assert_eq!(r2.action, LifecycleAction::Transition);
        assert!(!r2.enabled);
    }

    #[test]
    fn versioning_config_default_disabled() {
        assert_eq!(VersioningConfig::disabled(), VersioningConfig::default());
        assert!(VersioningConfig::enabled().enabled);
        assert!(!VersioningConfig::disabled().enabled);
    }

    // —— 操作模型 builder ——

    #[test]
    fn put_request_builder() {
        let r = PutObjectRequest::new("b", "k", Bytes::from_static(b"hi"))
            .with_content_type("text/plain");
        assert_eq!(r.bucket, "b");
        assert_eq!(r.content_type.as_deref(), Some("text/plain"));
        assert_eq!(r.data.as_ref(), b"hi");
    }

    #[test]
    fn get_request_range_version() {
        let r = GetObjectRequest::new("b", "k")
            .with_version("v2")
            .with_range(0, 99);
        assert_eq!(r.version_id.as_deref(), Some("v2"));
        assert_eq!(r.range_start, Some(0));
        assert_eq!(r.range_end, Some(99));
    }

    // —— sigv4 字符串构造 ——

    #[test]
    fn credential_scope_format() {
        let s = sigv4::credential_scope("20240101", "us-east-1", "s3");
        assert_eq!(s, "20240101/us-east-1/s3/aws4_request");
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        let q = sigv4::canonical_query_string(&[("b", "x y"), ("a", "1")]);
        // 按 k 升序：a 在前；空格编码为 %20
        assert_eq!(q, "a=1&b=x%20y");
    }

    #[test]
    fn canonical_query_slash_encoded() {
        // query 参数值中的 / 也编码
        let q = sigv4::canonical_query_string(&[("prefix", "a/b")]);
        assert_eq!(q, "prefix=a%2Fb");
    }

    #[test]
    fn canonical_request_basic() {
        let h: &[sigv4::CanonicalHeader] = &[
            ("Host".into(), "s3.amazonaws.com".into()),
            ("x-amz-date".into(), "20240101T000000Z".into()),
            ("x-amz-content-sha256".into(), "UNSIGNED-PAYLOAD".into()),
        ];
        let cr = sigv4::canonical_request("GET", "/bucket/key", "", h, "UNSIGNED-PAYLOAD");
        // header 排序后 host < x-amz-content-sha256 < x-amz-date
        assert_eq!(
            cr,
            "GET\n\
             /bucket/key\n\
             \n\
             host:s3.amazonaws.com\n\
             x-amz-content-sha256:UNSIGNED-PAYLOAD\n\
             x-amz-date:20240101T000000Z\n\
             \n\
             host;x-amz-content-sha256;x-amz-date\n\
             UNSIGNED-PAYLOAD"
        );
    }

    #[test]
    fn string_to_sign_format() {
        let s = sigv4::string_to_sign(
            "20240101T000000Z",
            "20240101/us-east-1/s3/aws4_request",
            "deadbeef",
        );
        assert_eq!(
            s,
            "AWS4-HMAC-SHA256\n\
             20240101T000000Z\n\
             20240101/us-east-1/s3/aws4_request\n\
             deadbeef"
        );
    }

    // —— RustFsObjectStore 骨架行为 ——

    #[test]
    fn rustfs_new_strips_trailing_slash() {
        let s = RustFsObjectStore::new("http://127.0.0.1:9000/", "us-east-1");
        assert_eq!(s.endpoint(), "http://127.0.0.1:9000");
        assert_eq!(s.region(), "us-east-1");
    }

    #[test]
    fn rustfs_object_url_path_style() {
        let s = RustFsObjectStore::new("http://127.0.0.1:9000", "us-east-1");
        let url = s.object_url("mybucket", "path/to/key.txt");
        assert_eq!(url, "http://127.0.0.1:9000/mybucket/path/to/key.txt");
    }

    #[test]
    fn rustfs_object_url_virtual_host_style() {
        let s =
            RustFsObjectStore::new("https://s3.amazonaws.com", "us-east-1").with_path_style(false);
        let url = s.object_url("mybucket", "key.txt");
        assert_eq!(url, "https://mybucket.s3.amazonaws.com/key.txt");

        let s2 = RustFsObjectStore::new("http://minio:9000", "us-east-1").with_path_style(false);
        let url2 = s2.object_url("b", "k");
        assert_eq!(url2, "http://b.minio:9000/k");
    }

    #[tokio::test]
    async fn rustfs_skeleton_returns_internal_err() {
        // 骨架未接通——所有方法返回 Internal（除 create_bucket 先做本地校验）
        let s = RustFsObjectStore::default();
        let err = s.list_buckets().await.unwrap_err();
        assert!(matches!(err, crate::ProtocolError::Internal(_)));

        // create_bucket 对非法名先本地校验
        let err = RustFsObjectStore::default()
            .create_bucket("UPPER")
            .await
            .unwrap_err();
        assert!(matches!(err, crate::ProtocolError::CommandFailed(_)));
    }

    // —— sigv4 边界情况（补测：空 header / 多空格折叠 / 特殊字符 URL 编码）——

    #[test]
    fn canonical_request_empty_headers() {
        // 空 headers 列表：canonical_headers 段为空，signed_headers 也为空
        let cr = sigv4::canonical_request("GET", "/", "", &[], "UNSIGNED-PAYLOAD");
        assert_eq!(cr, "GET\n/\n\n\n\nUNSIGNED-PAYLOAD");
    }

    #[test]
    fn canonical_request_collapses_internal_spaces() {
        // AWS SigV4 规范：header 值去首尾空白 + 把内部连续空格折叠为单个空格
        // （get-header-value-trim 用例：`"a   b   c"` → `"a b c"`）
        let h: &[sigv4::CanonicalHeader] = &[("Host".into(), "  a   b   c  ".into())];
        let cr = sigv4::canonical_request("GET", "/", "", h, "e3b0c44b");
        assert!(
            cr.contains("host:a b c\n"),
            "expected internal spaces collapsed, got:\n{cr}"
        );
    }

    #[test]
    fn canonical_request_lowercases_header_names() {
        // header name 大小写不敏感：规范化为小写后排序
        let h: &[sigv4::CanonicalHeader] = &[
            ("X-AMZ-Date".into(), "20240101T000000Z".into()),
            ("HOST".into(), "s3.example.com".into()),
        ];
        let cr = sigv4::canonical_request("GET", "/", "", h, "UNSIGNED-PAYLOAD");
        // host 排在 x-amz-date 前（小写字典序）
        let host_pos = cr.find("host:").unwrap();
        let date_pos = cr.find("x-amz-date:").unwrap();
        assert!(host_pos < date_pos);
        // signed_headers 段也应是小写、排序后以 ; 连接
        assert!(cr.contains("host;x-amz-date"));
    }

    #[test]
    fn canonical_request_sorts_headers_by_name() {
        // 多 header 按 name 字典序排序（与值无关）
        let h: &[sigv4::CanonicalHeader] = &[
            ("z-header".into(), "z".into()),
            ("a-header".into(), "a".into()),
            ("m-header".into(), "m".into()),
        ];
        let cr = sigv4::canonical_request("GET", "/", "/", h, "x");
        let a = cr.find("a-header:").unwrap();
        let m = cr.find("m-header:").unwrap();
        let z = cr.find("z-header:").unwrap();
        assert!(a < m && m < z);
        // signed_headers 按同样顺序
        assert!(cr.contains("a-header;m-header;z-header"));
    }

    #[test]
    fn canonical_query_string_empty() {
        // 空参数 → 空字符串
        assert_eq!(sigv4::canonical_query_string(&[]), "");
    }

    #[test]
    fn canonical_query_string_encodes_special_chars() {
        // RFC 3986：unreserved（A-Za-z0-9-_.~）之外全编码（query 模式 / 也编码）
        let q = sigv4::canonical_query_string(&[("k", "a/b@c d")]);
        assert_eq!(q, "k=a%2Fb%40c%20d");
    }

    #[test]
    fn canonical_query_string_single_pair() {
        let q = sigv4::canonical_query_string(&[("foo", "bar")]);
        assert_eq!(q, "foo=bar");
    }

    #[test]
    fn uri_encode_query_preserves_unreserved() {
        // 通过 canonical_query_string 间接测 uri_encode：unreserved 不编码
        let q = sigv4::canonical_query_string(&[("k", "A-z0-9-_.~")]);
        assert_eq!(q, "k=A-z0-9-_.~");
    }

    #[test]
    fn credential_scope_components() {
        // 各分量独立 → 拼接正确
        let s = sigv4::credential_scope("20251231", "eu-west-1", "s3");
        assert_eq!(s, "20251231/eu-west-1/s3/aws4_request");
    }

    #[test]
    fn string_to_sign_includes_all_lines() {
        let s = sigv4::string_to_sign(
            "20240101T120000Z",
            "20240101/us-east-1/s3/aws4_request",
            "abcdef0123456789",
        );
        assert!(s.starts_with("AWS4-HMAC-SHA256\n"));
        assert!(s.contains("20240101T120000Z\n"));
        assert!(s.contains("20240101/us-east-1/s3/aws4_request\n"));
        assert!(s.ends_with("abcdef0123456789"));
    }

    #[test]
    fn canonical_request_post_method() {
        // 非GET方法 + 带 query + 单 header
        let h: &[sigv4::CanonicalHeader] = &[("host".into(), "example.com".into())];
        let cr = sigv4::canonical_request("POST", "/", "k=v", h, "hashed");
        assert!(cr.starts_with("POST\n/\nk=v\n"));
        assert!(cr.contains("host:example.com\n"));
        assert!(cr.ends_with("hashed"));
    }

    // —— RustFsObjectStore 骨架分支（每个方法均返回 Internal 未接通错误）——

    #[test]
    fn rustfs_default_has_expected_endpoint_region() {
        let s = RustFsObjectStore::default();
        assert_eq!(s.endpoint(), "http://127.0.0.1:9000");
        assert_eq!(s.region(), "us-east-1");
    }

    #[test]
    fn rustfs_with_path_style_builder() {
        // with_path_style 链式构造
        let s = RustFsObjectStore::new("http://x", "r").with_path_style(false);
        let url = s.object_url("b", "k");
        assert_eq!(url, "http://b.x/k");
    }

    #[tokio::test]
    async fn rustfs_create_bucket_invalid_name_returns_command_failed() {
        // create_bucket 在未接通前仍先做本地校验：非法名 → CommandFailed（来自 validate_bucket_name）
        let s = RustFsObjectStore::default();
        let err = s.create_bucket("UP").await.unwrap_err();
        assert!(matches!(err, crate::ProtocolError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn rustfs_create_bucket_valid_name_returns_internal_not_connected() {
        // 合法名但未接通 → Internal（含方法名 + bucket）
        let s = RustFsObjectStore::default();
        let err = s.create_bucket("valid-bucket").await.unwrap_err();
        match err {
            crate::ProtocolError::Internal(msg) => {
                assert!(msg.contains("create_bucket"));
                assert!(msg.contains("valid-bucket"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rustfs_all_unimplemented_methods_return_internal() {
        // 逐方法断言返回 Internal（未接通占位路径）
        let s = RustFsObjectStore::default();
        assert!(matches!(
            s.delete_bucket("b").await.unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.list_buckets().await.unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.put_object("b", "k", bytes::Bytes::from_static(b"x"), None)
                .await
                .unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.get_object("b", "k").await.unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.delete_object("b", "k").await.unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.list_objects("b", "p", PageRequest::default())
                .await
                .unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.create_access_key(Vec::new()).await.unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
        assert!(matches!(
            s.delete_access_key("ak").await.unwrap_err(),
            crate::ProtocolError::Internal(_)
        ));
    }

    #[tokio::test]
    async fn rustfs_put_object_with_content_type_unreachable() {
        // 带 content_type 的分支同样未接通
        let s = RustFsObjectStore::default();
        let err = s
            .put_object(
                "b",
                "k",
                bytes::Bytes::from_static(b"data"),
                Some("text/plain".into()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::ProtocolError::Internal(_)));
    }
}
