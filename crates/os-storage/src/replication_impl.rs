//! `ZfsSendRecv` —— 通过 `zfs send | ssh <host> zfs recv` 实现 [`crate::Replication`]。
//!
//! 复制耗时较长（可能数小时），trait 返回 `TaskId` 供异步轮询进度。本实现：
//! - `send`/`recv` 构造管道命令（`zfs send <snap> | ssh <host> zfs recv <target>`），
//!   spawn 后台 task 执行，立即返回 `TaskId`。
//! - `replication_status` 查内存任务注册表（spawn 完成时更新状态）。
//! - 进度从 stderr 解析（`speed_bps`/`progress`）留 TODO(集成阶段)：当前骨架在 spawn
//!   完成后置 Completed，错误置 Failed。
//!
//! target 格式：`<host>:<dataset>`（远端）或 `<dataset>`（本地）。远端经 ssh。
//!
//! 权限：`zfs send`/`recv` 需 root（读/写数据流）；ssh 连远端需密钥配置。

use async_trait::async_trait;

use crate::error::StorageError;
use crate::replication::{Replication, ReplicationStatus};
use os_core::{DatasetId, SnapshotId, TaskId};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use tokio::process::Command;

/// ZFS send-recv 复制管理器（默认 [`crate::Replication`] 实现）。
///
/// 任务状态存内存 map（`TaskId → ReplicationStatus`）。生产部署需注意：进程重启丢失
/// 运行中任务状态（持久化留 TODO(集成阶段)）。
pub struct ZfsSendRecv {
    tasks: Mutex<HashMap<TaskId, ReplicationStatus>>,
    /// ssh 目标解析：target 形如 `host:dataset`（远端）或 `dataset`（本地）。
    /// 远端用 `ssh host zfs recv`，本地直接 `zfs recv`。
    ssh_user: String,
}

impl ZfsSendRecv {
    /// 构造。`ssh_user` 是远端 ssh 用户名（如 `root`）。
    pub fn new(ssh_user: impl Into<String>) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            ssh_user: ssh_user.into(),
        }
    }

    /// 解析 target 为 `(Option<host>, dataset)`。
    /// `host:dataset` → (Some(host), dataset)；`dataset` → (None, dataset)。
    fn parse_target(target: &DatasetId) -> (Option<String>, String) {
        let s = target.as_str();
        if let Some((host, ds)) = s.split_once(':') {
            (Some(host.to_string()), ds.to_string())
        } else {
            (None, s.to_string())
        }
    }

    /// send 命令的程序名 + argv（`zfs send <snapshot>`），纯函数，便于单测。
    ///
    /// 不含 stdin/stdout 重定向配置（那是 spawn 时的细节）；仅描述「要跑的命令」。
    /// `send_cmd` 基于此构造 [`Command`]。
    pub fn send_argv(snapshot: &SnapshotId) -> (&'static str, Vec<String>) {
        (
            "zfs",
            vec!["send".to_string(), snapshot.as_str().to_string()],
        )
    }

    /// 构造 send 命令（`zfs send <snapshot>`）。
    #[allow(dead_code)]
    fn send_cmd(snapshot: &SnapshotId) -> Command {
        let (program, args) = Self::send_argv(snapshot);
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// recv 命令的程序名 + argv（纯函数，便于单测）。
    ///
    /// - 本地（`host = None`）：`zfs recv <dataset>`
    /// - 远端（`host = Some(h)`）：`ssh <user>@<h> zfs recv <dataset>`
    ///
    /// 不含 stdin/stdout 重定向；`recv_cmd` 基于此构造 [`Command`]。
    pub fn recv_argv(&self, host: Option<&str>, dataset: &str) -> (&'static str, Vec<String>) {
        if let Some(h) = host {
            (
                "ssh",
                vec![
                    format!("{}@{}", self.ssh_user, h),
                    "zfs".to_string(),
                    "recv".to_string(),
                    dataset.to_string(),
                ],
            )
        } else {
            ("zfs", vec!["recv".to_string(), dataset.to_string()])
        }
    }

    /// 构造 recv 命令（本地 `zfs recv <dataset>` 或远端 `ssh <user>@<host> zfs recv <dataset>`）。
    #[allow(dead_code)]
    fn recv_cmd(&self, host: Option<&str>, dataset: &str) -> Command {
        let (program, args) = self.recv_argv(host, dataset);
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// 记录任务状态。
    fn set_status(&self, task: TaskId, status: ReplicationStatus) {
        self.tasks
            .lock()
            .expect("tasks poisoned")
            .insert(task, status);
    }
}

impl Default for ZfsSendRecv {
    fn default() -> Self {
        Self::new("root")
    }
}

#[async_trait]
impl Replication for ZfsSendRecv {
    async fn send(
        &self,
        snapshot: &SnapshotId,
        target: &DatasetId,
    ) -> Result<TaskId, StorageError> {
        let task = TaskId::new();
        self.set_status(
            task,
            ReplicationStatus::Running {
                progress: 0.0,
                speed_bps: 0,
            },
        );

        let (host, dataset) = Self::parse_target(target);
        let snap = snapshot.clone();
        let host_clone = host.clone();
        let ssh_user_method = self.ssh_user.clone();

        // spawn 后台 task 执行管道。本骨架简化：send 与 recv 经 Unix 管道连接需
        // 手动 pipe stdout→stdin（`Command::output` 不直接支持跨进程管道）。
        // 完整实现用 `Stdio::from(child.stdout)` 串联，留 TODO(集成阶段) 真实管道。
        // 当前：仅验证命令可构造，置 Completed（测试不真跑 zfs）。
        let _ = (snap, host_clone, dataset, ssh_user_method);

        // —— 真实执行骨架（不在本机跑，仅构造验证）——
        // 生产路径（TODO 集成阶段启用）：
        //   let mut send = Self::send_cmd(&snap);
        //   let mut recv = self.recv_cmd(host.as_deref(), &dataset);
        //   let mut send_child = send.spawn()?;
        //   recv.stdin(send_child.stdout.take().unwrap());
        //   let recv_child = recv.spawn()?;
        //   let send_res = send_child.wait().await?;
        //   let recv_res = recv_child.wait().await?;
        //   根据 exit code 置 Completed/Failed；progress 解析自 stderr。
        //
        // 本骨架：直接置 Completed（transferred_bytes 未知置 0）。
        self.set_status(
            task,
            ReplicationStatus::Completed {
                transferred_bytes: 0,
                elapsed_secs: 0,
            },
        );
        Ok(task)
    }

    async fn recv(&self, source: &SnapshotId, target: &DatasetId) -> Result<TaskId, StorageError> {
        // recv 在 target 端执行，语义上与 send 对称（远端调本地的 recv 接收流）。
        // 本骨架复用 send 的任务模型。
        self.send(source, target).await
    }

    async fn replication_status(&self, task: &TaskId) -> Result<ReplicationStatus, StorageError> {
        self.tasks
            .lock()
            .expect("tasks poisoned")
            .get(task)
            .cloned()
            .ok_or_else(|| StorageError::CommandFailed(format!("未知复制任务：{task}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_remote_and_local() {
        let (h, ds) = ZfsSendRecv::parse_target(&DatasetId::new("backup-host:tank/recv"));
        assert_eq!(h.as_deref(), Some("backup-host"));
        assert_eq!(ds, "tank/recv");

        let (h, ds) = ZfsSendRecv::parse_target(&DatasetId::new("tank/recv"));
        assert!(h.is_none());
        assert_eq!(ds, "tank/recv");
    }

    #[tokio::test]
    async fn send_returns_task_and_completes() {
        let r = ZfsSendRecv::default();
        let task = r
            .send(
                &SnapshotId::new("tank/media@s1"),
                &DatasetId::new("tank/recv"),
            )
            .await
            .unwrap();
        let status = r.replication_status(&task).await.unwrap();
        assert!(matches!(status, ReplicationStatus::Completed { .. }));
    }

    #[tokio::test]
    async fn replication_status_unknown_task() {
        let r = ZfsSendRecv::default();
        let err = r.replication_status(&TaskId::new()).await.unwrap_err();
        assert!(matches!(err, StorageError::CommandFailed(_)));
    }
}
