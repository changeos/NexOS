//! `MockObjectStore` —— 纯内存 [`crate::ObjectStore`] 实现，供下游测试注入。
//!
//! 仅在 `mock` feature 下编译。下游（api-agent 等）在 `[dev-dependencies]` 加
//! `os-protocols = { workspace = true, features = ["mock"] }`。
//!
//! 命名与放置说明（见 `_conventions.md §5` 与 object-agent 规格 §3）：
//! - 文件命名为 `object_mock.rs` 而非 `mock.rs`——后者归 protocol-agent 维护
//!   （FileProtocol/SmbManager/... 的 mock）。本文件由 object-agent 独占，避免合并冲突。
//! - 仅 mock `ObjectStore`（本 agent 唯一 trait）。
//!
//! 设计：
//! - 实现完整 `ObjectStore` trait，**不依赖外部状态**（无 HTTP/无 RustFS）。
//! - 构造器预置返回值：`MockObjectStore::new().with_bucket(b).with_object(m, data)`。
//! - 写操作更新内部状态，使后续读反映变更（"创建后列出" / "删除后不存在"）。
//! - 错误注入：`with_error` 强制下次任一方法返回指定错误（一次性）。
//! - 所有方法永不 spawn 子进程、永不 panic（锁中毒除外）。

#![cfg(feature = "mock")]

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use os_core::{PageRequest, PageResponse};

use crate::object::{validate_bucket_name, AccessKey, Bucket, BucketPermission, ObjectMeta};
use crate::{ObjectStore, ProtocolError, ProtocolResult};

/// Mock 对象存储——纯内存、确定性。
///
/// 内部状态：buckets / objects / access_keys 三张 HashMap，加可选的强制错误。
/// objects 以 `(bucket, key)` 复合键索引，值含对象元数据与原始字节。
pub struct MockObjectStore {
    inner: Mutex<MockState>,
}

/// 内存对象条目：元数据 + 内容。
struct StoredObject {
    meta: ObjectMeta,
    data: Bytes,
}

struct MockState {
    buckets: HashMap<String, Bucket>,
    objects: HashMap<(String, String), StoredObject>,
    access_keys: HashMap<String, AccessKey>,
    /// 强制错误：下次任一方法返回此错误（None = 正常），一次性。
    forced_error: Option<ProtocolError>,
    /// access key id 自增计数（构造确定性 id）。
    ak_counter: u64,
}

impl MockState {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            objects: HashMap::new(),
            access_keys: HashMap::new(),
            forced_error: None,
            ak_counter: 0,
        }
    }

    fn check_forced(&mut self) -> Result<(), ProtocolError> {
        if let Some(e) = self.forced_error.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl MockObjectStore {
    /// 构造空 mock（无 bucket / 无 object）。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockState::new()),
        }
    }

    /// 预置一个 bucket（随后 `list_buckets`/`put_object` 冲突检测可见）。
    ///
    /// 不校验 bucket 名（预置数据可能来自 fixture），直接入库。
    #[must_use]
    pub fn with_bucket(self, bucket: Bucket) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.buckets.insert(bucket.name.clone(), bucket);
        }
        self
    }

    /// 预置一个对象（含内容）。若其所属 bucket 不存在，会自动创建一个占位 bucket。
    #[must_use]
    pub fn with_object(self, meta: ObjectMeta, data: Bytes) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            // 自动补 bucket 占位
            st.buckets
                .entry(meta.bucket.clone())
                .or_insert_with(|| Bucket {
                    name: meta.bucket.clone(),
                    created: os_core::Utc::now(),
                    versioning: false,
                    object_count: 0,
                });
            st.objects.insert(
                (meta.bucket.clone(), meta.key.clone()),
                StoredObject { meta, data },
            );
        }
        self
    }

    /// 强制下次（任一）方法返回指定错误。一次性——只触发一次后清除。
    #[must_use]
    pub fn with_error(self, err: ProtocolError) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.forced_error = Some(err);
        }
        self
    }

    /// 当前 bucket 数量（断言用）。
    pub fn bucket_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").buckets.len()
    }

    /// 当前对象数量（断言用）。
    pub fn object_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").objects.len()
    }

    /// 当前 access key 数量。
    pub fn access_key_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").access_keys.len()
    }
}

impl Default for MockObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectStore for MockObjectStore {
    async fn create_bucket(&self, name: &str) -> ProtocolResult<Bucket> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        // 先做 S3 命名校验（与 RustFsObjectStore 一致）
        validate_bucket_name(name)?;
        if st.buckets.contains_key(name) {
            return Err(ProtocolError::CommandFailed(format!(
                "bucket 已存在：{name}"
            )));
        }
        let b = Bucket {
            name: name.to_string(),
            created: os_core::Utc::now(),
            versioning: false,
            object_count: 0,
        };
        st.buckets.insert(name.to_string(), b.clone());
        Ok(b)
    }

    async fn delete_bucket(&self, name: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if st.buckets.remove(name).is_none() {
            return Err(ProtocolError::BucketNotFound(name.into()));
        }
        // 级联删除 bucket 内对象
        st.objects.retain(|(b, _), _| b != name);
        Ok(())
    }

    async fn list_buckets(&self) -> ProtocolResult<Vec<Bucket>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let mut buckets: Vec<Bucket> = st.buckets.values().cloned().collect();
        buckets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(buckets)
    }

    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Bytes,
        content_type: Option<String>,
    ) -> ProtocolResult<ObjectMeta> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if !st.buckets.contains_key(bucket) {
            return Err(ProtocolError::BucketNotFound(bucket.into()));
        }
        let size = data.len() as u64;
        // 简化 etag：用字节数 + 一个稳定后缀（不计算真实 MD5，避免引依赖）。
        let etag = format!("mock-{:x}", size);
        let ct = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
        let meta = ObjectMeta::new(bucket, key, size, &etag, &ct);
        // 维护 bucket object_count
        let is_new = !st
            .objects
            .contains_key(&(bucket.to_string(), key.to_string()));
        st.objects.insert(
            (bucket.to_string(), key.to_string()),
            StoredObject {
                meta: meta.clone(),
                data,
            },
        );
        if is_new {
            if let Some(b) = st.buckets.get_mut(bucket) {
                b.object_count += 1;
            }
        }
        Ok(meta)
    }

    async fn get_object(&self, bucket: &str, key: &str) -> ProtocolResult<Bytes> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        match st.objects.get(&(bucket.to_string(), key.to_string())) {
            Some(obj) => Ok(obj.data.clone()),
            None => {
                if st.buckets.contains_key(bucket) {
                    Err(ProtocolError::ObjectNotFound(format!("{bucket}/{key}")))
                } else {
                    Err(ProtocolError::BucketNotFound(bucket.into()))
                }
            }
        }
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if !st.buckets.contains_key(bucket) {
            return Err(ProtocolError::BucketNotFound(bucket.into()));
        }
        if st
            .objects
            .remove(&(bucket.to_string(), key.to_string()))
            .is_some()
        {
            if let Some(b) = st.buckets.get_mut(bucket) {
                b.object_count = b.object_count.saturating_sub(1);
            }
        }
        // key 不存在也视为成功（幂等，符合 S3 语义）
        Ok(())
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        page: PageRequest,
    ) -> ProtocolResult<PageResponse<ObjectMeta>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if !st.buckets.contains_key(bucket) {
            return Err(ProtocolError::BucketNotFound(bucket.into()));
        }
        // 过滤 + 排序（按 key 升序，确定性）
        let mut matched: Vec<ObjectMeta> = st
            .objects
            .values()
            .filter(|o| o.meta.bucket == bucket && o.meta.key.starts_with(prefix))
            .map(|o| o.meta.clone())
            .collect();
        matched.sort_by(|a, b| a.key.cmp(&b.key));
        let total = matched.len() as u32;
        // 分页切片
        let offset = page.offset;
        let limit = page.limit.max(1) as usize;
        let start = (offset as usize).min(matched.len());
        let end = (start + limit).min(matched.len());
        let items = matched[start..end].to_vec();
        Ok(PageResponse {
            items,
            total,
            offset,
            limit: page.limit,
        })
    }

    async fn create_access_key(
        &self,
        permissions: Vec<BucketPermission>,
    ) -> ProtocolResult<AccessKey> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.ak_counter += 1;
        let id = format!("MOCKAK{:08x}", st.ak_counter);
        let secret = format!("mock-secret-{:08x}", st.ak_counter);
        // mock 下用占位 hash（不计算真实 Argon2，避免引依赖）。
        let hash = format!("mock-hash-{id}");
        let ak = AccessKey::new(&id, &hash, permissions);
        st.access_keys.insert(id.clone(), ak.clone());
        // 注意：真实实现层须把明文 secret 仅返回一次（见 CreatedAccessKey）。
        // 这里 mock 直接返回 AccessKey（含 secret_hash），明文 secret 不暴露——
        // 下游测试如需明文 secret，可读取本函数内的局部 `secret`（未暴露）。
        let _ = secret; // 占位，避免未使用告警
        Ok(ak)
    }

    async fn delete_access_key(&self, access_key_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if st.access_keys.remove(access_key_id).is_none() {
            return Err(ProtocolError::CommandFailed(format!(
                "access key 不存在：{access_key_id}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(name: &str) -> Bucket {
        Bucket {
            name: name.into(),
            created: os_core::Utc::now(),
            versioning: false,
            object_count: 0,
        }
    }

    #[tokio::test]
    async fn create_bucket_then_list_and_conflict() {
        let s = MockObjectStore::new();
        let b = s.create_bucket("photos").await.unwrap();
        assert_eq!(b.name, "photos");
        assert_eq!(s.bucket_count(), 1);
        let list = s.list_buckets().await.unwrap();
        assert_eq!(list.len(), 1);
        // 二次 create 报 CommandFailed（沿用：无 BucketExists variant）
        assert!(matches!(
            s.create_bucket("photos").await.unwrap_err(),
            ProtocolError::CommandFailed(_)
        ));
    }

    #[tokio::test]
    async fn create_bucket_rejects_invalid_name() {
        let s = MockObjectStore::new();
        assert!(matches!(
            s.create_bucket("UPPER").await.unwrap_err(),
            ProtocolError::CommandFailed(_)
        ));
        assert_eq!(s.bucket_count(), 0);
    }

    #[tokio::test]
    async fn delete_bucket_cascade_objects() {
        let s = MockObjectStore::new();
        s.create_bucket("bkt").await.unwrap();
        s.put_object("bkt", "k1", Bytes::from_static(b"v1"), None)
            .await
            .unwrap();
        assert_eq!(s.object_count(), 1);
        s.delete_bucket("bkt").await.unwrap();
        // 级联：object 也清空
        assert_eq!(s.object_count(), 0);
        assert_eq!(s.bucket_count(), 0);
        // 二次删除报 BucketNotFound
        assert!(matches!(
            s.delete_bucket("bkt").await.unwrap_err(),
            ProtocolError::BucketNotFound(_)
        ));
    }

    #[tokio::test]
    async fn put_get_delete_object_lifecycle() {
        let s = MockObjectStore::new();
        s.create_bucket("bkt").await.unwrap();
        let meta = s
            .put_object(
                "bkt",
                "a.txt",
                Bytes::from_static(b"hello"),
                Some("text/plain".into()),
            )
            .await
            .unwrap();
        assert_eq!(meta.size, 5);
        assert_eq!(meta.content_type, "text/plain");
        assert_eq!(s.bucket_count(), 1);
        // 校验 bucket object_count 计数
        let b = s.list_buckets().await.unwrap();
        assert_eq!(b[0].object_count, 1);

        let data = s.get_object("bkt", "a.txt").await.unwrap();
        assert_eq!(data.as_ref(), b"hello");

        // 覆盖写不计新增 count
        s.put_object("bkt", "a.txt", Bytes::from_static(b"hello2"), None)
            .await
            .unwrap();
        let b = s.list_buckets().await.unwrap();
        assert_eq!(b[0].object_count, 1);

        s.delete_object("bkt", "a.txt").await.unwrap();
        assert!(matches!(
            s.get_object("bkt", "a.txt").await.unwrap_err(),
            ProtocolError::ObjectNotFound(_)
        ));
        // count 递减
        let b = s.list_buckets().await.unwrap();
        assert_eq!(b[0].object_count, 0);
        // 幂等：删不存在的 key 不报错
        s.delete_object("bkt", "a.txt").await.unwrap();
    }

    #[tokio::test]
    async fn put_object_to_missing_bucket() {
        let s = MockObjectStore::new();
        assert!(matches!(
            s.put_object("ghost", "k", Bytes::from_static(b"x"), None)
                .await
                .unwrap_err(),
            ProtocolError::BucketNotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_objects_pagination() {
        let s = MockObjectStore::new();
        s.create_bucket("bkt").await.unwrap();
        // 放 5 个对象：a1..a5
        for i in 1..=5u8 {
            s.put_object("bkt", &format!("a{i}"), Bytes::copy_from_slice(&[i]), None)
                .await
                .unwrap();
        }
        // 另放不同前缀
        s.put_object("bkt", "b1", Bytes::from_static(b"x"), None)
            .await
            .unwrap();

        // 前缀过滤
        let page = PageRequest {
            offset: 0,
            limit: 100,
        };
        let res = s.list_objects("bkt", "a", page).await.unwrap();
        assert_eq!(res.items.len(), 5);
        assert_eq!(res.total, 5);
        // 排序：a1..a5
        assert_eq!(res.items[0].key, "a1");
        assert_eq!(res.items[4].key, "a5");

        // 分页：limit=2, offset=0 → 2 条
        let p1 = PageRequest {
            offset: 0,
            limit: 2,
        };
        let r1 = s.list_objects("bkt", "a", p1).await.unwrap();
        assert_eq!(r1.items.len(), 2);
        assert_eq!(r1.items[0].key, "a1");
        assert_eq!(r1.items[1].key, "a2");
        assert_eq!(r1.offset, 0);

        // offset=2, limit=2 → a3,a4
        let p2 = PageRequest {
            offset: 2,
            limit: 2,
        };
        let r2 = s.list_objects("bkt", "a", p2).await.unwrap();
        assert_eq!(r2.items.len(), 2);
        assert_eq!(r2.items[0].key, "a3");
        assert_eq!(r2.items[1].key, "a4");

        // offset=4, limit=2 → a5（末页不足）
        let p3 = PageRequest {
            offset: 4,
            limit: 2,
        };
        let r3 = s.list_objects("bkt", "a", p3).await.unwrap();
        assert_eq!(r3.items.len(), 1);
        assert_eq!(r3.items[0].key, "a5");
    }

    #[tokio::test]
    async fn access_key_create_and_delete() {
        let s = MockObjectStore::new();
        let ak = s
            .create_access_key(vec![BucketPermission::Read, BucketPermission::Write])
            .await
            .unwrap();
        assert!(ak.access_key_id.starts_with("MOCKAK"));
        assert_eq!(ak.permissions.len(), 2);
        assert_eq!(s.access_key_count(), 1);

        // 删除
        s.delete_access_key(&ak.access_key_id).await.unwrap();
        assert_eq!(s.access_key_count(), 0);
        // 二次删除报错
        assert!(matches!(
            s.delete_access_key(&ak.access_key_id).await.unwrap_err(),
            ProtocolError::CommandFailed(_)
        ));
    }

    #[tokio::test]
    async fn forced_error_injects_once() {
        let s = MockObjectStore::new().with_error(ProtocolError::Internal("boom".into()));
        let err = s.list_buckets().await.unwrap_err();
        assert!(matches!(err, ProtocolError::Internal(_)));
        // 一次性：再调正常（空 bucket list）
        assert!(s.list_buckets().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn with_object_auto_creates_bucket() {
        let meta = ObjectMeta::new("pre", "k", 3, "etag", "text/plain");
        let s = MockObjectStore::new().with_object(meta, Bytes::from_static(b"abc"));
        assert_eq!(s.bucket_count(), 1);
        let data = s.get_object("pre", "k").await.unwrap();
        assert_eq!(data.as_ref(), b"abc");
    }

    #[test]
    fn bucket_fixture_unused_warning_avoid() {
        // bucket() helper 也在 mock tests 复用——确保非 unused
        let _ = bucket("x");
    }
}
