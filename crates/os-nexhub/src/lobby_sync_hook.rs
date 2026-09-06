//! post-receive 自动同步钩子——「nexos 联邦大厅条目自动同步最新提交」的触发链
//! （设计文档 `docs/NEXHUB_LOBBY_DESIGN.md` §15，2026-08-25）。
//!
//! # 问题
//!
//! nexos 是联邦大厅的第一个条目（启动 [`crate::nexhub_lobby`] 的
//! `ensure_nexos_published` 自动发布 + 联邦广播），但大厅条目的 commit 数/最新提交/
//! README 摘要是**发布时的快照**——106 节点推了新提交后，本地与联邦条目都不会
//! 自动更新，需手动重发 publish。系统自举（新节点从联邦大厅拿系统代码）依赖
//! 条目反映真实最新状态，故补全自动同步链。
//!
//! # 链路
//!
//! ```text
//! git push → <repos>/nexos.git/hooks/post-receive（本模块生成/补装）
//!          → 后台 curl POST /api/v1/nexhub/lobby/publish   （重取快照：
//!              commit_count / latest_commit（短 hash+subject+作者+时间）/
//!              README 摘要 / pushed_at=刷新时间；保留 download_count）
//!          → 后台 curl POST /api/v1/nexhub/lobby/nexos/federate（重广播）
//!          → 各联邦节点 ingest 按 name 幂等合并（Refreshed：字段更新、
//!              条目不重复、本地克隆计数保留）
//! ```
//!
//! # 安装（幂等）
//!
//! [`ensure_nexos_published`]（os-api 启动 ensure 流程）每次启动顺手补装：钩子
//! 缺失或生成内容漂移（地址/token 变更）→ 写入并 chmod 755；已一致 → no-op；
//! 存量钩子**不含本生成器标记**（用户自管）→ 不动。任何部署形态（systemd /
//! docker / 手动）启动即获得自动同步能力，无需人工装钩子。
//!
//! # 性能与安全
//!
//! - 钩子绝不阻塞 git push：curl 带 `-m 5` 硬超时且整体 `( … ) &` 后台执行；
//! - 钩子只打 127.0.0.1 回环地址（地址可经 [`ENV_LOBBY_SYNC_API`] 覆盖，如容器
//!   里 os-api 不在 8558 端口的部署）；
//! - token 经 [`lobby_sync_admin_token`] 生成时代入（`NEXOS_ADMIN_TOKEN`/
//!   `OS_ADMIN_TOKEN` → 默认 `change-me-admin-token`，与现网开发默认一致；**生产
//!   须设置真实 admin token 环境变量**——脚本内注释同标注）。

/// 钩子生成器标识：补装逻辑据此区分「本生成器产物」（缺则补/漂移则覆盖）与
/// 「用户自管钩子」（一律不动）。版本号随脚本契约升级递增。
pub const HOOK_MARKER: &str = "# nexhub-lobby-auto-sync v1";

/// 钩子目标 API 地址覆盖 env（默认 `http://127.0.0.1:8558`，即现网 os-api 端口；
/// 容器/自定义端口部署用它改指向）。
pub const ENV_LOBBY_SYNC_API: &str = "NEXOS_LOBBY_SYNC_API";

/// 开发默认 admin token（与现网部署一致；生产须设 `NEXOS_ADMIN_TOKEN`）。
const DEFAULT_ADMIN_TOKEN: &str = "change-me-admin-token";

/// 默认 API 基址（os-api 现网端口 8558，docs/NEXHUB_ONBOARDING.md）。
const DEFAULT_API_BASE: &str = "http://127.0.0.1:8558";

/// 单引号内安全转义（POSIX sh 字符串 `'…'\''…'` 惯例）。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 生成 post-receive 钩子脚本（纯函数，可单测）。
///
/// `repo_name` 须已过名称校验（调用方 [`ensure_post_receive_hook`] 只对内置
/// `nexos` 调用；手工为其他仓库生成时注意防注入——名字含引号会被原样转义但
/// 仍建议先走大厅同名校验规则）。
///
/// 脚本契约（设计文档 §15）：
/// - `#!/bin/sh` + [`HOOK_MARKER`] 标识；
/// - 顺序 curl 两个端点：publish（刷新快照）→ federate（联邦重广播）。publish
///   是 federate 的前置（federate 广播的是本地条目最新快照，不先 publish 广播
///   的还是旧快照）；
/// - 每个 curl `-m 5` 硬超时 + 整体 `( … ) &` 后台子 shell——git push 永不等待；
/// - `exit 0` 恒成功（钩子失败不影响 push 本身）。
#[must_use]
pub fn build_post_receive_hook_script(
    repo_name: &str,
    api_base: &str,
    admin_token: &str,
) -> String {
    let base = api_base.trim().trim_end_matches('/');
    let publish_url = format!("{base}/api/v1/nexhub/lobby/publish");
    let federate_url = format!("{base}/api/v1/nexhub/lobby/{repo_name}/federate");
    let auth = format!("Authorization: Bearer {admin_token}");
    let publish_body = format!("{{\"repo\":\"{repo_name}\"}}");
    format!(
        "#!/bin/sh\n\
         {HOOK_MARKER} —— os-nexhub 生成（勿手改：启动 ensure 流程缺则补装/内容漂移则覆盖）\n\
         # 作用：git push 本仓库 → 后台触发 NexHub 大厅重新发布（publish 刷新 commit 数/\n\
         # latest_commit/README 摘要/pushed_at 快照，保留 download_count）并重新推送联邦\n\
         # （federate 重广播；联邦各节点按 name 幂等合并为快照更新）——大厅条目自动\n\
         # 反映最新提交，系统自举（新节点从联邦拿系统代码）不再看到过期状态。\n\
         # 性能：curl -m 5 硬超时 + 整体后台执行，绝不阻塞 git push；恒 exit 0。\n\
         # 安全：仅打 127.0.0.1 回环；token 生成时取自 NEXOS_ADMIN_TOKEN（默认\n\
         # change-me-admin-token 为开发默认值，生产环境必须设置真实 admin token）。\n\
         ( curl -s -m 5 -X POST {publish_url} \\\n\
             -H {auth_hdr} -H 'Content-Type: application/json' \\\n\
             -d {body} >/dev/null 2>&1\n\
           curl -s -m 5 -X POST {federate_url} \\\n\
             -H {auth_hdr} -H 'Content-Type: application/json' \\\n\
             -d '{{}}' >/dev/null 2>&1 ) &\n\
         exit 0\n",
        publish_url = shell_single_quote(&publish_url),
        federate_url = shell_single_quote(&federate_url),
        auth_hdr = shell_single_quote(&auth),
        body = shell_single_quote(&publish_body),
    )
}

/// 钩子目标 API 基址：env [`ENV_LOBBY_SYNC_API`] 覆盖（尾 `/` 容错剔除），
/// 默认 [`DEFAULT_API_BASE`]。
#[must_use]
pub fn lobby_sync_api_base() -> String {
    std::env::var(ENV_LOBBY_SYNC_API)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// 钩子携带的 admin token：`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`（与 os-api 网关
/// 同一变量）→ 默认 [`DEFAULT_ADMIN_TOKEN`]（生产必须显式设置环境变量）。
#[must_use]
pub fn lobby_sync_admin_token() -> String {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| DEFAULT_ADMIN_TOKEN.to_string())
}

/// 幂等补装 `<repos_root>/<repo_name>.git/hooks/post-receive`：
///
/// - 钩子不存在 → 写入 [`build_post_receive_hook_script`] 产物 + chmod 755 → `Ok(true)`；
/// - 已存在且与本生成器当前产物**逐字节一致** → no-op `Ok(false)`（重复安装内容恒一致）；
/// - 已存在且是本生成器旧产物（含 [`HOOK_MARKER`]）但内容漂移（地址/token 变更）
///   → 覆盖为新产物 `Ok(true)`；
/// - 已存在但**不含** [`HOOK_MARKER`]（用户自管钩子）→ 不动 `Ok(false)`。
///
/// hooks 目录不存在则一并创建（`git init --bare` 本会建，防御手工搬运的仓库）。
/// 调用方：[`crate::nexhub_lobby`] 的启动 ensure 流程（`ensure_nexos_published`）。
pub fn ensure_post_receive_hook(
    repos_root: &str,
    repo_name: &str,
    api_base: &str,
    admin_token: &str,
) -> std::io::Result<bool> {
    let hooks_dir = format!("{repos_root}/{repo_name}.git/hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = format!("{hooks_dir}/post-receive");
    let content = build_post_receive_hook_script(repo_name, api_base, admin_token);
    if let Ok(existing) = std::fs::read_to_string(&hook_path) {
        if existing == content {
            return Ok(false); // 已装且一致 → 幂等 no-op
        }
        if !existing.contains(HOOK_MARKER) {
            return Ok(false); // 用户自管钩子 → 不覆盖
        }
        // 本生成器旧产物但内容漂移 → 落到下方覆盖
    }
    std::fs::write(&hook_path, &content)?;
    make_executable(&hook_path)?;
    Ok(true)
}

/// chmod 755（unix；git 钩子必须可执行才生效。非 unix 平台 no-op）。
fn make_executable(path: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::metadata(path)?.permissions();
        if perm.mode() & 0o755 != 0o755 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 唯一临时目录（与 nexhub_lobby.rs 测试同款惯例）。
    fn tempdir() -> String {
        let p = std::env::temp_dir().join(format!(
            "os-nexhub-hook-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// 执行外部命令（git/curl fixture 用，与 nexhub_lobby.rs 测试 run 同款）。
    fn run(cmd: &[&str]) -> (bool, String) {
        match std::process::Command::new(cmd[0]).args(&cmd[1..]).output() {
            Ok(out) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
            ),
            Err(_) => (false, String::new()),
        }
    }

    /// K1. 钩子脚本生成（纯函数）：契约要素齐全——marker/两个端点/认证头/仓库名/
    ///     -m 5 超时/后台执行/恒 exit 0；token 含单引号时安全转义。
    #[test]
    fn hook_script_content_matches_contract() {
        let s =
            build_post_receive_hook_script("nexos", "http://127.0.0.1:8558", "change-me-admin-token");
        assert!(s.starts_with("#!/bin/sh\n"), "sh 解释器: {s}");
        assert!(s.contains(HOOK_MARKER), "生成器标识（补装判定依据）: {s}");
        // 发布端点：刷新快照（latest_commit/pushed_at 即在此路径重取）
        assert!(
            s.contains("'http://127.0.0.1:8558/api/v1/nexhub/lobby/publish'"),
            "publish URL: {s}"
        );
        // 联邦端点：重广播（repo 名进路径）
        assert!(
            s.contains("'http://127.0.0.1:8558/api/v1/nexhub/lobby/nexos/federate'"),
            "federate URL: {s}"
        );
        assert!(
            s.contains("-H 'Authorization: Bearer change-me-admin-token'"),
            "admin 认证头: {s}"
        );
        assert!(
            s.contains("-d '{\"repo\":\"nexos\"}'"),
            "publish 体带仓库名: {s}"
        );
        assert_eq!(
            s.matches("curl -s -m 5 -X POST").count(),
            2,
            "两个 curl 各带 5s 硬超时"
        );
        assert!(s.contains(") &"), "整体后台执行（不阻塞 git push）: {s}");
        assert!(s.trim_end().ends_with("exit 0"), "恒成功: {s}");
        // 基址尾斜杠容错
        let s2 = build_post_receive_hook_script("nexos", "http://127.0.0.1:9999/", "tk");
        assert!(s2.contains("'http://127.0.0.1:9999/api/v1/nexhub/lobby/publish'"));
        // token 含单引号 → POSIX 转义（不破脚本语法）
        let s3 = build_post_receive_hook_script("nexos", "http://127.0.0.1:8558", "a'b");
        assert!(
            s3.contains("-H 'Authorization: Bearer a'\\''b'"),
            "引号转义: {s3}"
        );
    }

    /// K2. 幂等补装：缺失 → 装上（可执行）；再装两次 → no-op 且内容逐字节一致；
    ///     用户自管钩子（无 marker）不被覆盖；生成器旧产物内容漂移 → 覆盖为新产物。
    #[test]
    fn ensure_hook_install_is_idempotent() {
        let dir = tempdir();
        let bare = format!("{dir}/nexos.git");
        assert!(run(&["git", "init", "--bare", &bare]).0, "init 失败");
        let hook = format!("{bare}/hooks/post-receive");
        // 缺失 → 补装
        assert!(ensure_post_receive_hook(&dir, "nexos", "http://127.0.0.1:8558", "tk1").unwrap());
        let first = std::fs::read_to_string(&hook).unwrap();
        assert!(first.contains(HOOK_MARKER));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "钩子必须可执行: {mode:o}");
        }
        // 再装两次（同参） → no-op，内容一致（幂等）
        assert!(!ensure_post_receive_hook(&dir, "nexos", "http://127.0.0.1:8558", "tk1").unwrap());
        assert!(!ensure_post_receive_hook(&dir, "nexos", "http://127.0.0.1:8558", "tk1").unwrap());
        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            first,
            "重复安装内容一致"
        );
        // 生成器旧产物漂移（地址变化）→ 覆盖
        assert!(
            ensure_post_receive_hook(&dir, "nexos", "http://127.0.0.1:9999", "tk2").unwrap(),
            "生成器产物内容漂移应覆盖"
        );
        let updated = std::fs::read_to_string(&hook).unwrap();
        assert!(updated.contains(":9999"), "覆盖为新地址: {updated}");
        assert_ne!(updated, first);
        // 用户自管钩子（无 marker）→ 不动
        std::fs::write(&hook, "#!/bin/sh\n# 我的自定义钩子\nexit 0\n").unwrap();
        assert!(
            !ensure_post_receive_hook(&dir, "nexos", "http://127.0.0.1:8558", "tk1").unwrap(),
            "用户自管钩子不覆盖"
        );
        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            "#!/bin/sh\n# 我的自定义钩子\nexit 0\n"
        );
        // hooks 目录缺失（手工搬运仓库）→ 一并创建后成功
        let bare2 = format!("{dir}/moved.git");
        assert!(run(&["git", "init", "--bare", &bare2]).0);
        std::fs::remove_dir_all(format!("{bare2}/hooks")).unwrap();
        assert!(ensure_post_receive_hook(&dir, "moved", "http://127.0.0.1:8558", "tk").unwrap());
        assert!(Path::new(&format!("{bare2}/hooks/post-receive")).exists());
    }

    /// K3. 端到端（真实 git push）：临时裸仓 + 本地 sink HTTP 服务 + 钩子指向它 →
    ///     push 触发钩子，后台 curl 依次打 publish 与 federate（路径/认证头/请求体
    ///     契约核对），且 push 不被钩子阻塞。环境无 curl 则跳过（不空报错）。
    #[test]
    fn post_receive_hook_fires_publish_and_federate_on_real_push() {
        if !run(&["curl", "--version"]).0 {
            eprintln!("[skip] 环境无 curl，跳过钩子端到端测试");
            return;
        }
        let dir = tempdir();
        let bare = format!("{dir}/nexos.git");
        assert!(run(&["git", "init", "--bare", &bare]).0);
        // 先做一个初始提交并推上（钩子未装，不触发）
        let work = format!("{dir}/w");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(format!("{work}/README.md"), "# hook e2e").unwrap();
        assert!(run(&["git", "-c", "init.defaultBranch=main", "init", &work]).0);
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init"
            ])
            .0
        );
        assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:refs/heads/main"]).0);
        // 本地 sink HTTP 服务：收两个请求（publish → federate），逐条回 200
        let (base, rx) = sink_server();
        assert!(
            ensure_post_receive_hook(&dir, "nexos", &base, "e2e-token").unwrap(),
            "补装钩子"
        );
        // 真实 push 新提交 → post-receive 触发（计时：钩子必须不阻塞 push）
        std::fs::write(format!("{work}/extra.txt"), "x").unwrap();
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "second"
            ])
            .0
        );
        let started = std::time::Instant::now();
        assert!(
            run(&["git", "-C", &work, "push", &bare, "HEAD:refs/heads/main"]).0,
            "push 应成功"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "钩子后台执行，push 不被阻塞: {:?}",
            started.elapsed()
        );
        // 钩子后台 curl：publish 先到、federate 后到（顺序执行），10s 内收齐
        let req1 = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("publish 请求未到达");
        assert!(
            req1.0.starts_with("POST /api/v1/nexhub/lobby/publish"),
            "路径: {req1:?}"
        );
        assert!(
            req1.1.contains("Authorization: Bearer e2e-token"),
            "认证头: {req1:?}"
        );
        assert!(
            req1.2.contains("\"repo\":\"nexos\""),
            "请求体带仓库名: {req1:?}"
        );
        let req2 = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("federate 请求未到达");
        assert!(
            req2.0
                .starts_with("POST /api/v1/nexhub/lobby/nexos/federate"),
            "路径: {req2:?}"
        );
        assert!(
            req2.1.contains("Authorization: Bearer e2e-token"),
            "认证头: {req2:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 极简 HTTP sink：监听 127.0.0.1 随机端口，收一条请求 → 回 200 → 关连接。
    /// 返回 (基址, 请求通道)——请求为 (请求行, 头部原文, 体)。
    fn sink_server() -> (String, std::sync::mpsc::Receiver<(String, String, String)>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let addr = listener.local_addr().expect("addr 失败");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut stream = stream;
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                // 读到头部结束 + Content-Length 体收完（或超时/对端关）
                let (req_line, headers, body) = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break Default::default();
                    };
                    if n == 0 {
                        break Default::default();
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some(hdr_end) = text.find("\r\n\r\n") {
                        let headers = text[..hdr_end].to_string();
                        let len: usize = headers
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= hdr_end + 4 + len {
                            let req_line = headers.lines().next().unwrap_or_default().to_string();
                            let body = text[hdr_end + 4..hdr_end + 4 + len].to_string();
                            break (req_line, headers, body);
                        }
                    }
                };
                if !req_line.is_empty() {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    );
                    tx.send((req_line, headers, body)).ok();
                }
                // Connection: close → 每请求一连接（curl 两个进程两次连接）
            }
        });
        (format!("http://{addr}"), rx)
    }
}
