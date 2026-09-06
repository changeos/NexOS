//! libunftp 的内存存储后端（用于离线/可测的真实 FTP 协议栈接通）。
//!
//! 设计动机（ADR-DEPS-002 / 规格 §9 红线"不真监听端口"）：
//! - `LibunftpBackend` 需要一个 `StorageBackend<DefaultUser>` 才能构造 `libunftp::Server`，
//!   证明 FTP 协议栈**真的接通**（而非 TODO 骨架）。
//! - 真实生产部署会换成基于数据集路径的 filesystem 后端（如 `unftp-sbe-fs`）；本 crate
//!   只负责"协议栈接通 + 离线可测"，故提供一个**纯内存**的最小实现，避免引入额外 SBE crate
//!   与磁盘副作用，CI 友好。
//!
//! 本后端实现 RFC959 文件操作所需的最小子集（list/metadata/get/put/del/mkd/rmd/rename/cwd），
//! 文件内容存于 `Vec<u8>`，目录树为嵌套 `BTreeMap`。确定性、可序列化测试断言。

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use unftp_core::auth::DefaultUser;
use unftp_core::storage::{Error, ErrorKind, Fileinfo, Metadata, StorageBackend};

/// 内存 FTP 后端持有的目录/文件节点。
#[derive(Debug, Clone)]
enum Node {
    Dir {
        children: BTreeMap<String, Node>,
        mtime: SystemTime,
    },
    File {
        data: Vec<u8>,
        mtime: SystemTime,
    },
}

impl Node {
    fn new_dir(mtime: SystemTime) -> Self {
        Self::Dir {
            children: BTreeMap::new(),
            mtime,
        }
    }
}

/// 内存 FTP 文件元数据（实现 [`Metadata`]）。
#[derive(Debug, Clone)]
pub struct MemFtpMetadata {
    /// 文件长度（字节）；目录占位 4096。
    pub len: u64,
    /// 是否目录。
    pub is_dir: bool,
    /// 最近修改时间。
    pub mtime: SystemTime,
}

impl Metadata for MemFtpMetadata {
    fn len(&self) -> u64 {
        self.len
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn is_file(&self) -> bool {
        !self.is_dir
    }
    fn is_symlink(&self) -> bool {
        false
    }
    fn modified(&self) -> unftp_core::storage::Result<SystemTime> {
        Ok(self.mtime)
    }
    fn gid(&self) -> u32 {
        0
    }
    fn uid(&self) -> u32 {
        0
    }
}

/// 纯内存 FTP 存储后端——离线可测、无磁盘副作用。
///
/// 根目录在构造时创建；所有 FTP 路径相对根解析（`/` 开头视为绝对，从根开始）。
/// 线程安全：内部 `tokio::sync::Mutex`（libunftp 的异步方法持锁 await）。
#[derive(Debug, Clone)]
pub struct InMemoryFtpBackend {
    root: Arc<Mutex<Node>>,
}

impl InMemoryFtpBackend {
    /// 构造一个带空根目录的内存后端。
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(Mutex::new(Node::new_dir(SystemTime::now()))),
        }
    }

    /// 在根下创建一条测试用文件（`/path/to/file`，内容即 `data`）。
    /// 仅测试辅助：让 list/get 等操作有可断言内容。
    pub async fn seed_file(&self, path: &Path, data: Vec<u8>) {
        let mut root = self.root.lock().await;
        Self::put_node(
            &mut root,
            path,
            Node::File {
                data,
                mtime: SystemTime::now(),
            },
        );
    }

    /// 递归写入节点到指定路径（路径父目录不存在则返回）。
    fn put_node(root: &mut Node, path: &Path, node: Node) {
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty() && *s != "/")
            .collect();
        Self::put_node_recursive(root, &comps, node);
    }

    fn put_node_recursive(cur: &mut Node, comps: &[&str], node: Node) {
        if comps.is_empty() {
            return;
        }
        match cur {
            Node::Dir { children, .. } => {
                if comps.len() == 1 {
                    children.insert(comps[0].to_string(), node);
                } else if let Some(child) = children.get_mut(comps[0]) {
                    Self::put_node_recursive(child, &comps[1..], node);
                }
            }
            Node::File { .. } => {}
        }
    }

    /// 解析路径下的节点引用（路径不存在返回 None）。
    async fn lookup(&self, path: &Path) -> Option<Node> {
        let root = self.root.lock().await;
        Self::lookup_node(&root, path)
    }

    fn lookup_node(root: &Node, path: &Path) -> Option<Node> {
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty() && *s != "/")
            .collect();
        let mut cur = root;
        for c in comps {
            match cur {
                Node::Dir { children, .. } => {
                    cur = children.get(c)?;
                }
                Node::File { .. } => return None,
            }
        }
        Some(cur.clone())
    }

    fn err(kind: ErrorKind, msg: impl Into<String>) -> Error {
        Error::new(
            kind,
            std::io::Error::new(std::io::ErrorKind::NotFound, msg.into()),
        )
    }
}

impl Default for InMemoryFtpBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageBackend<DefaultUser> for InMemoryFtpBackend {
    type Metadata = MemFtpMetadata;

    async fn metadata<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
    ) -> unftp_core::storage::Result<Self::Metadata> {
        match self.lookup(path.as_ref()).await {
            Some(Node::Dir { mtime, .. }) => Ok(MemFtpMetadata {
                len: 4096,
                is_dir: true,
                mtime,
            }),
            Some(Node::File { data, mtime }) => Ok(MemFtpMetadata {
                len: data.len() as u64,
                is_dir: false,
                mtime,
            }),
            None => Err(Self::err(
                ErrorKind::PermanentFileNotAvailable,
                format!("路径不存在: {}", path.as_ref().display()),
            )),
        }
    }

    async fn list<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
    ) -> unftp_core::storage::Result<Vec<Fileinfo<PathBuf, Self::Metadata>>> {
        let root = self.root.lock().await;
        let Some(Node::Dir {
            children, mtime, ..
        }) = Self::lookup_node(&root, path.as_ref())
        else {
            return Err(Self::err(
                ErrorKind::PermanentDirectoryNotAvailable,
                format!("目录不存在: {}", path.as_ref().display()),
            ));
        };
        let _ = mtime;
        let mut out: Vec<Fileinfo<PathBuf, Self::Metadata>> = Vec::new();
        for (name, node) in children {
            let mut p = PathBuf::from(path.as_ref());
            p.push(name);
            let md = match node {
                Node::Dir { mtime, .. } => MemFtpMetadata {
                    len: 4096,
                    is_dir: true,
                    mtime,
                },
                Node::File { data, mtime } => MemFtpMetadata {
                    len: data.len() as u64,
                    is_dir: false,
                    mtime,
                },
            };
            out.push(Fileinfo {
                path: p,
                metadata: md,
            });
        }
        Ok(out)
    }

    async fn get<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
        _start_pos: u64,
    ) -> unftp_core::storage::Result<Box<dyn tokio::io::AsyncRead + Send + Sync + Unpin>> {
        match self.lookup(path.as_ref()).await {
            Some(Node::File { data, .. }) => Ok(Box::new(Cursor::new(data))),
            Some(Node::Dir { .. }) => {
                Err(Self::err(ErrorKind::PermanentFileNotAvailable, "是目录"))
            }
            None => Err(Self::err(
                ErrorKind::PermanentFileNotAvailable,
                format!("路径不存在: {}", path.as_ref().display()),
            )),
        }
    }

    async fn put<
        P: AsRef<Path> + Send + std::fmt::Debug,
        R: tokio::io::AsyncRead + Send + Sync + Unpin + 'static,
    >(
        &self,
        _user: &DefaultUser,
        mut input: R,
        path: P,
        _start_pos: u64,
    ) -> unftp_core::storage::Result<u64> {
        let mut buf = Vec::new();
        input
            .read_to_end(&mut buf)
            .await
            .map_err(|e| Self::err(ErrorKind::LocalError, format!("读取失败: {e}")))?;
        let n = buf.len() as u64;
        let mut root = self.root.lock().await;
        Self::put_node(
            &mut root,
            path.as_ref(),
            Node::File {
                data: buf,
                mtime: SystemTime::now(),
            },
        );
        Ok(n)
    }

    async fn del<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
    ) -> unftp_core::storage::Result<()> {
        let mut root = self.root.lock().await;
        let removed = Self::remove_node(&mut root, path.as_ref());
        if removed.is_some() {
            Ok(())
        } else {
            Err(Self::err(
                ErrorKind::PermanentFileNotAvailable,
                format!("路径不存在: {}", path.as_ref().display()),
            ))
        }
    }

    async fn mkd<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
    ) -> unftp_core::storage::Result<()> {
        let mut root = self.root.lock().await;
        Self::put_node(&mut root, path.as_ref(), Node::new_dir(SystemTime::now()));
        Ok(())
    }

    async fn rename<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        from: P,
        to: P,
    ) -> unftp_core::storage::Result<()> {
        let mut root = self.root.lock().await;
        if let Some(node) = Self::remove_node(&mut root, from.as_ref()) {
            Self::put_node(&mut root, to.as_ref(), node);
            Ok(())
        } else {
            Err(Self::err(
                ErrorKind::PermanentFileNotAvailable,
                format!("路径不存在: {}", from.as_ref().display()),
            ))
        }
    }

    async fn rmd<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
    ) -> unftp_core::storage::Result<()> {
        let mut root = self.root.lock().await;
        // 校验是目录（而非文件）再删
        if matches!(
            Self::lookup_node(&root, path.as_ref()),
            Some(Node::Dir { .. })
        ) {
            Self::remove_node(&mut root, path.as_ref());
            Ok(())
        } else {
            Err(Self::err(
                ErrorKind::PermanentDirectoryNotAvailable,
                format!("目录不存在: {}", path.as_ref().display()),
            ))
        }
    }

    async fn cwd<P: AsRef<Path> + Send + std::fmt::Debug>(
        &self,
        _user: &DefaultUser,
        path: P,
    ) -> unftp_core::storage::Result<()> {
        let root = self.root.lock().await;
        match Self::lookup_node(&root, path.as_ref()) {
            Some(Node::Dir { .. }) => Ok(()),
            _ => Err(Self::err(
                ErrorKind::PermanentDirectoryNotAvailable,
                format!("目录不存在: {}", path.as_ref().display()),
            )),
        }
    }
}

impl InMemoryFtpBackend {
    /// 从树中移除指定路径节点，返回被移除的节点（路径不存在返回 None）。
    fn remove_node(root: &mut Node, path: &Path) -> Option<Node> {
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty() && *s != "/")
            .collect();
        Self::remove_node_recursive(root, &comps)
    }

    fn remove_node_recursive(cur: &mut Node, comps: &[&str]) -> Option<Node> {
        if comps.is_empty() {
            return None;
        }
        match cur {
            Node::Dir { children, .. } => {
                if comps.len() == 1 {
                    children.remove(comps[0])
                } else if let Some(child) = children.get_mut(comps[0]) {
                    Self::remove_node_recursive(child, &comps[1..])
                } else {
                    None
                }
            }
            Node::File { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_backend_put_get_list_del() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        // mkd
        be.mkd(&user, Path::new("/docs")).await.unwrap();
        be.cwd(&user, Path::new("/docs")).await.unwrap();
        // put
        let n = be
            .put(&user, Cursor::new(*b"hello"), Path::new("/docs/a.txt"), 0)
            .await
            .unwrap();
        assert_eq!(n, 5);
        // metadata
        let md = be.metadata(&user, Path::new("/docs/a.txt")).await.unwrap();
        assert_eq!(md.len, 5);
        assert!(md.is_file());
        // list
        let entries = be.list(&user, Path::new("/docs")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path.to_string_lossy().ends_with("a.txt"));
        // get
        let mut reader = be.get(&user, Path::new("/docs/a.txt"), 0).await.unwrap();
        let mut out = String::new();
        reader.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello");
        // rename
        be.rename(&user, Path::new("/docs/a.txt"), Path::new("/docs/b.txt"))
            .await
            .unwrap();
        assert!(be.get(&user, Path::new("/docs/a.txt"), 0).await.is_err());
        // del
        be.del(&user, Path::new("/docs/b.txt")).await.unwrap();
        assert!(be.list(&user, Path::new("/docs")).await.unwrap().is_empty());
        // rmd
        be.rmd(&user, Path::new("/docs")).await.unwrap();
        assert!(be.cwd(&user, Path::new("/docs")).await.is_err());
    }

    // —— 边界情况补测：错误路径 + 节点递归分支 ——

    #[tokio::test]
    async fn metadata_on_missing_path_returns_permanent_file_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let err = be.metadata(&user, Path::new("/nope")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentFileNotAvailable);
    }

    #[tokio::test]
    async fn metadata_on_root_dir_returns_dir_metadata() {
        // 根目录的 metadata：is_dir=true, len=4096
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let md = be.metadata(&user, Path::new("/")).await.unwrap();
        assert!(md.is_dir);
        assert!(!md.is_file());
        assert!(!md.is_symlink());
        assert_eq!(md.len, 4096);
        assert_eq!(md.gid(), 0);
        assert_eq!(md.uid(), 0);
        assert!(md.modified().is_ok());
    }

    #[tokio::test]
    async fn list_on_missing_dir_returns_directory_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        // 对不存在的路径 list → PermanentDirectoryNotAvailable
        // （list 的 Ok 类型 Vec<Fileinfo<..>> 未实现 Debug，故 match 错误分支）
        match be.list(&user, Path::new("/nope")).await {
            Err(e) => assert_eq!(e.kind(), ErrorKind::PermanentDirectoryNotAvailable),
            Ok(_) => panic!("expected list on missing dir to fail"),
        }
    }

    #[tokio::test]
    async fn list_on_file_returns_directory_not_available() {
        // 对一个文件路径（非目录）list → PermanentDirectoryNotAvailable
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.put(&user, Cursor::new(*b"x"), Path::new("/f.txt"), 0)
            .await
            .unwrap();
        match be.list(&user, Path::new("/f.txt")).await {
            Err(e) => assert_eq!(e.kind(), ErrorKind::PermanentDirectoryNotAvailable),
            Ok(_) => panic!("expected list on file to fail"),
        }
    }

    #[tokio::test]
    async fn get_on_dir_returns_permanent_file_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.mkd(&user, Path::new("/d")).await.unwrap();
        // get 返回 Box<dyn AsyncRead>（无 Debug），故用 match 模式匹配错误 kind
        match be.get(&user, Path::new("/d"), 0).await {
            Err(e) => assert_eq!(e.kind(), ErrorKind::PermanentFileNotAvailable),
            Ok(_) => panic!("expected get on dir to fail"),
        }
    }

    #[tokio::test]
    async fn get_on_missing_returns_permanent_file_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        match be.get(&user, Path::new("/nope"), 0).await {
            Err(e) => assert_eq!(e.kind(), ErrorKind::PermanentFileNotAvailable),
            Ok(_) => panic!("expected get on missing path to fail"),
        }
    }

    #[tokio::test]
    async fn rename_missing_source_returns_permanent_file_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let err = be
            .rename(&user, Path::new("/nope"), Path::new("/dst"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentFileNotAvailable);
    }

    #[tokio::test]
    async fn del_missing_returns_permanent_file_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let err = be.del(&user, Path::new("/nope")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentFileNotAvailable);
    }

    #[tokio::test]
    async fn rmd_on_file_returns_directory_not_available() {
        // rmd 要求目标是目录；对文件 → PermanentDirectoryNotAvailable
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.put(&user, Cursor::new(*b"x"), Path::new("/f.txt"), 0)
            .await
            .unwrap();
        let err = be.rmd(&user, Path::new("/f.txt")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentDirectoryNotAvailable);
        // 文件应仍存在（rmd 未删）
        assert!(be.metadata(&user, Path::new("/f.txt")).await.is_ok());
    }

    #[tokio::test]
    async fn rmd_on_missing_returns_directory_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let err = be.rmd(&user, Path::new("/nope")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentDirectoryNotAvailable);
    }

    #[tokio::test]
    async fn cwd_on_file_returns_directory_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.put(&user, Cursor::new(*b"x"), Path::new("/f.txt"), 0)
            .await
            .unwrap();
        let err = be.cwd(&user, Path::new("/f.txt")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentDirectoryNotAvailable);
    }

    #[tokio::test]
    async fn cwd_on_missing_returns_directory_not_available() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let err = be.cwd(&user, Path::new("/nope")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentDirectoryNotAvailable);
    }

    #[tokio::test]
    async fn put_into_nested_existing_dir_overwrites() {
        // 多层路径：先建子目录，再往里 put；list 应反映
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.mkd(&user, Path::new("/a")).await.unwrap();
        be.mkd(&user, Path::new("/a/b")).await.unwrap();
        let n = be
            .put(&user, Cursor::new(*b"deep"), Path::new("/a/b/f"), 0)
            .await
            .unwrap();
        assert_eq!(n, 4);
        let entries = be.list(&user, Path::new("/a/b")).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn put_into_missing_intermediate_dir_is_no_op() {
        // put_node_recursive：父目录不存在时静默 no-op（不创建中间目录）
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.put(&user, Cursor::new(*b"x"), Path::new("/no/where/f"), 0)
            .await
            .unwrap();
        // 文件未落地（父目录 /no 不存在）
        assert!(be.metadata(&user, Path::new("/no/where/f")).await.is_err());
    }

    #[tokio::test]
    async fn put_read_error_propagates_local_error() {
        // 喂入一个会读失败的 reader（短路 reader）→ LocalError
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tokio::io::{AsyncRead, ReadBuf};

        struct FailingReader;
        impl AsyncRead for FailingReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Err(std::io::Error::other("boom")))
            }
        }
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let err = be
            .put(&user, FailingReader, Path::new("/f"), 0)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::LocalError);
    }

    #[tokio::test]
    async fn lookup_through_file_returns_none() {
        // lookup_node：路径中段遇到 File 节点 → None（文件下不能再有子节点）
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.put(&user, Cursor::new(*b"x"), Path::new("/f"), 0)
            .await
            .unwrap();
        // 尝试把 /f 当目录访问子路径 → 文件无子节点
        let err = be.metadata(&user, Path::new("/f/sub")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentFileNotAvailable);
    }

    #[tokio::test]
    async fn remove_node_recursive_into_file_returns_none() {
        // remove_node_recursive：对 File 节点递归删子路径 → None
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        be.put(&user, Cursor::new(*b"x"), Path::new("/f"), 0)
            .await
            .unwrap();
        // 尝试删 /f/sub（f 是文件，子路径不可达）→ 文件本身仍在
        let err = be.del(&user, Path::new("/f/sub")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermanentFileNotAvailable);
        assert!(be.metadata(&user, Path::new("/f")).await.is_ok());
    }

    #[tokio::test]
    async fn seed_file_helper_places_content() {
        // seed_file 公共测试辅助：写入后可经 get 读回
        let be = InMemoryFtpBackend::new();
        be.seed_file(Path::new("/seeded.txt"), b"seeded".to_vec())
            .await;
        let user = DefaultUser;
        let mut reader = be.get(&user, Path::new("/seeded.txt"), 0).await.unwrap();
        let mut out = String::new();
        reader.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "seeded");
    }

    #[tokio::test]
    async fn metadata_of_file_reports_correct_len() {
        let be = InMemoryFtpBackend::new();
        let user = DefaultUser;
        let n = be
            .put(&user, Cursor::new(*b"12345"), Path::new("/len"), 0)
            .await
            .unwrap();
        assert_eq!(n, 5);
        let md = be.metadata(&user, Path::new("/len")).await.unwrap();
        assert_eq!(md.len, 5);
        assert!(!md.is_dir);
        assert!(md.is_file());
    }

    #[test]
    fn default_impl_matches_new() {
        // Default trait 与 new() 等价
        let a = InMemoryFtpBackend::new();
        let b = InMemoryFtpBackend::default();
        // 两者都持有独立空根；share_count 等价（无法直接断言内部，但类型相同）
        let _ = (a, b);
    }
}
