//! `osd` 守护进程 binary 入口 —— OS 系统编排守护进程（PID1 后）的 `main.rs`。
//!
//! 定位（规格书 §3.13 / §9.1#8）：
//! - 进程监管：拉起/停止/重启各业务组件进程（os-storage / os-network / os-api ...）
//! - cgroup v2 资源隔离：每个组件按 `ResourceQuota` 限制 CPU/内存/IO
//! - NTP 时间同步：由 [`osd::NtpManager`]（ChronyNtp）实现，本入口不强持有
//!
//! # 命令行模式
//!
//! - `osd`：正常启动 —— 拓扑排序拉起全部已注册组件，阻塞等待 SIGTERM/SIGINT，
//!   收到信号后逆拓扑序优雅停止。
//! - `osd --check`：预检模式 —— 不真启，只做诊断：
//!   1. CPU 虚拟化前置检查（[`os_compute::preflight_virt_check`]）
//!   2. 组件依赖拓扑排序 + 循环检测（[`SystemdOrchestrator::startup_order`]）
//!   3. cgroup v2 / systemd 可达性探测
//! - `osd --component <id>`：单组件模式 —— 只拉起指定组件（其依赖也一并拉起），
//!   不启动其他组件；收到信号后只停该组件。
//! - `osd --config <path>`：从 JSON 配置文件读取组件描述符列表（未提供时用内置默认）。
//!
//! # 后端选择（root 探测）
//!
//! - root + systemd（PID1）环境：注入真实 [`osd::TokioSystemdRunner`]（跑 systemctl）。
//!   cgroup 后端由 [`SystemdOrchestrator::with_systemd_runner`] 默认注入
//!   [`osd::InMemoryCgroupBackend`]（启动阶段不强写配额，生产配额由 set_quota 按需）。
//! - 非 root / 无 systemd：注入 [`osd::InMemorySystemdRunner`]（no-op）+
//!   [`osd::InMemoryCgroupBackend`]（不真写 cgroup），保证 `--check` 与启动状态机可跑。
//!
//! # 红线说明
//!
//! 本入口是**可编译可运行可 --check**的目标；组件注册为示例性默认集合（storage/network/api），
//! 真实生产部署的完整依赖注入（zvol/network/api 进程的真实 ExecStart）是后续工作。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use os_core::ResourceQuota;
use osd::component::{ComponentDescriptor, ComponentId, HealthProbeConfig};
use osd::orchestrator::Orchestrator;
use osd::systemd_runner::TokioSystemdRunner;
use osd::{ComponentRegistry, InMemoryCgroupBackend, SystemdOrchestrator};
// --serve-api 内嵌网关：复用 os-api 的 InProcessGateway + RouteHandler 装配模式。
use os_api::gateway::Gateway;
use os_api::handlers::{ComputeRouteHandler, StorageRouteHandler, SystemRouteHandler};
use os_api::{AuditMiddleware, AuthMiddleware, InProcessGateway, StatefulRateLimiter};
use os_compute::LibvirtVmManager;
use os_storage::ZfsCliBackend;

/// 构造 1.0 CPU 核默认配额（cgroup v2 cpu.max）。
fn default_quota(cpu: f32) -> ResourceQuota {
    ResourceQuota {
        cpu_cores: Some(cpu),
        memory_bytes: None,
        io_bps_limit: None,
    }
}

/// 内置默认组件集合（示例性：osd 守护进程自身能编排的最小核心三件套）。
///
/// 真实生产应从 `--config <path>` 读取完整 ComponentDescriptor 列表（含各组件真实
/// ExecStart / 配额 / 健康探针）。此处占位命令为 `/bin/sleep infinity`（内存 systemd
/// 后端 no-op；真实后端拉起 sleep 长跑进程，便于演示生命周期）。
fn default_components() -> Vec<ComponentDescriptor> {
    let probe = |target: &str| HealthProbeConfig {
        kind: "exec".into(),
        target: target.into(),
        interval_secs: 30,
        timeout_secs: 3,
        failure_threshold: 3,
    };
    let quota = default_quota(1.0);
    vec![
        ComponentDescriptor {
            id: ComponentId::new("storage"),
            dependencies: vec![],
            quota: quota.clone(),
            health_probe: probe("/usr/bin/pgrep os-storage"),
            command: Some("/bin/sleep infinity".into()),
            enabled: true,
        },
        ComponentDescriptor {
            id: ComponentId::new("network"),
            dependencies: vec![ComponentId::new("storage")],
            quota: quota.clone(),
            health_probe: probe("/usr/bin/pgrep os-network"),
            command: Some("/bin/sleep infinity".into()),
            enabled: true,
        },
        ComponentDescriptor {
            id: ComponentId::new("api"),
            dependencies: vec![ComponentId::new("network")],
            quota,
            health_probe: probe("/usr/bin/pgrep os-api"),
            command: Some("/bin/sleep infinity".into()),
            enabled: true,
        },
    ]
}

/// 从 JSON 配置文件读取组件列表（失败时退回内置默认）。
///
/// 配置格式：`[ComponentDescriptor, ...]`（serde 反序列化）。文件不存在或解析失败
/// 时打印警告并使用 [`default_components`]，避免阻塞 `--check` 诊断。
fn load_components(config: Option<&PathBuf>) -> Vec<ComponentDescriptor> {
    let Some(path) = config else {
        return default_components();
    };
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<Vec<ComponentDescriptor>>(&content) {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => {
                eprintln!("警告: 配置文件 {path:?} 解析为空组件列表，退回内置默认");
                default_components()
            }
            Err(e) => {
                eprintln!("警告: 配置文件 {path:?} 解析失败（{e}），退回内置默认");
                default_components()
            }
        },
        Err(e) => {
            eprintln!("警告: 配置文件 {path:?} 读取失败（{e}），退回内置默认");
            default_components()
        }
    }
}

/// osd 守护进程命令行参数（clap derive）。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "osd",
    version,
    about = "OS 系统编排守护进程（PID1 后）：组件生命周期 + cgroup v2 配额 + NTP",
    long_about = "拉起/停止/重启业务组件进程（按依赖拓扑序），cgroup v2 资源隔离，\
 监听 SIGTERM/SIGINT 优雅停止。--check 做预检不真启。"
)]
struct Cli {
    /// 配置文件路径（JSON：`[ComponentDescriptor, ...]`）。未提供时用内置默认组件集。
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// 预检模式：做 CPU 虚拟化 / 组件依赖拓扑 / cgroup+systemd 可达性诊断，不真启。
    #[arg(long)]
    check: bool,

    /// 单组件模式：只拉起指定组件（含其依赖），不启动其他组件。
    #[arg(long, value_name = "ID")]
    component: Option<String>,

    /// 内嵌 os-api 网关模式：同进程 tokio::spawn 跑 axum::serve，注册
    /// Storage/Compute/System RouteHandler + 中间件链（RateLimit→Auth→Audit）。
    ///
    /// 提供时（如 `--serve-api 0.0.0.0:8080`），osd 一体化启动 HTTP 网关，
    /// SIGTERM 时随组件逆序停止一起优雅关闭（Gateway::stop）。
    /// 未提供时保持纯组件编排行为（不启 HTTP）。
    #[arg(long, value_name = "SOCKET")]
    serve_api: Option<String>,
}

/// 当前进程是否以 root 运行（决定是否注入真实 systemd 后端）。
///
/// 读 `/proc/self/status` 的 `Uid:` 行第四字段（effective uid）。EUID=0 即 root。
/// 不引入 libc 依赖（workspace 未注册 libc）。
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(3))
                .and_then(|s| s.parse::<u32>().ok())
        })
        .map(|euid| euid == 0)
        .unwrap_or(false)
}

/// PID1 是否是 systemd（决定 systemd 后端是否可用）。
///
/// systemd 是 PID1 时 `/run/systemd/system` 存在（systemd 启动时创建）。
fn have_systemd() -> bool {
    Path::new("/run/systemd/system").exists()
}

/// 按运行环境构造编排器：root + systemd 注入真实 systemd 后端，否则内存后端。
fn build_orchestrator(registry: ComponentRegistry) -> SystemdOrchestrator {
    let root = is_root();
    let systemd = have_systemd();
    if root && systemd {
        // 生产路径：真实 systemd 后端（跑 systemctl）。
        // cgroup 后端用 with_systemd_runner 默认的 InMemoryCgroupBackend
        // （启动阶段不强写配额；生产配额由 set_quota 按需调用）。
        eprintln!("[osd] 检测到 root + systemd，注入真实 systemd 后端（TokioSystemdRunner）");
        SystemdOrchestrator::with_systemd_runner(registry, Box::new(TokioSystemdRunner::new()))
    } else {
        // 框架/开发路径：内存后端（no-op），保证 --check 与状态机可跑
        eprintln!("[osd] 非 root 或无 systemd，注入内存后端（no-op，状态机可跑但不真启进程）");
        SystemdOrchestrator::with_cgroup_backend(
            registry,
            "os",
            Box::new(InMemoryCgroupBackend::new()),
        )
    }
}

/// 构造内嵌 os-api 网关：注册真实业务 RouteHandler（Storage/Compute/System）
/// + 中间件链（RateLimit → Auth → Audit），供 `--serve-api` 一体化模式启动。
///
/// 复用 os-api binary 的 `build_gateway` 装配模式（§9.1#10）：
/// - `storage` → `StorageRouteHandler(Arc<ZfsCliBackend>)`：`GET /api/v1/pools` 跑真实 `zpool list`
/// - `compute` → `ComputeRouteHandler(Arc<LibvirtVmManager>)`：`GET /api/v1/vms` 列 VM
/// - `system`  → `SystemRouteHandler`：`/healthz` `/version` `/status`
///
/// 中间件链顺序：RateLimit（1000 rps，宽松避免预检/调试被限流）→ Auth → Audit。
/// TLS 由反向代理终止（与 os-api binary 一致，rustls feature 未注册）。
///
/// 漏洞2 修复：注入 OS_ADMIN_TOKEN / OS_JWT_SECRET 凭据，使 HTTP 入口能解析
/// Bearer token 填充 `ApiRequest.auth`，下游 dispatch 强制鉴权才能生效。
async fn build_embedded_gateway() -> InProcessGateway {
    let mut gw = InProcessGateway::new();

    // 1) 注册真实业务 handler（各组件经 RouteHandler 装配进网关）
    let storage_backend = std::sync::Arc::new(ZfsCliBackend::new());
    let vm_manager = std::sync::Arc::new(LibvirtVmManager::new("local"));
    gw.register_component(
        "storage",
        Box::new(StorageRouteHandler::new(storage_backend)),
    )
    .await
    .expect("注册 storage handler");
    gw.register_component(
        "compute",
        Box::new(ComputeRouteHandler::with_arc(vm_manager)),
    )
    .await
    .expect("注册 compute handler");
    gw.register_component("system", Box::new(SystemRouteHandler::new()))
        .await
        .expect("注册 system handler");

    // 2) 中间件链：RateLimit → Auth → Audit（与 os-api binary 同款）
    gw.add_middleware(Box::new(StatefulRateLimiter::new(1000)));
    gw.add_middleware(Box::new(AuthMiddleware::new()));
    gw.add_middleware(Box::new(AuditMiddleware::new()));

    // 3) 漏洞2 修复：注入鉴权凭据（OS_ADMIN_TOKEN）。注：osd 嵌入式网关最小修复
    //    仅支持固定 admin token（避免引入 os-security 直接依赖）；如需 JWT，请用
    //    os-api binary（它注入完整的 OS_JWT_SECRET / OS_ADMIN_TOKEN 链路）。
    if let Ok(tok) = std::env::var("NEXOS_ADMIN_TOKEN").or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
    {
        if !tok.trim().is_empty() {
            eprintln!("[osd] 鉴权: OS_ADMIN_TOKEN 已启用");
            gw.set_admin_token(Some(std::sync::Arc::new(tok)));
        } else {
            eprintln!("[osd] 鉴权: OS_ADMIN_TOKEN 为空（写操作将被拒绝，需配置该环境变量）");
        }
    } else {
        eprintln!(
            "[osd] 鉴权: OS_ADMIN_TOKEN 未设置（写操作将被拒绝；请配置该环境变量，或用 os-api binary）"
        );
    }

    gw
}

/// 预检模式：做完整诊断，返回是否全过（true=无硬失败项）。
async fn run_check(orch: &SystemdOrchestrator) -> bool {
    let mut all_ok = true;

    // 1. CPU 虚拟化前置检查（VM 启动前置；osd 编排本身不依赖，作为系统就绪信号）
    print!("[check] CPU 虚拟化（KVM 前置）... ");
    match os_compute::preflight_virt_check().await {
        Ok(()) => println!("OK（/dev/kvm 可用 / vmx|svm 标志位存在）"),
        Err(e) => {
            // 虚拟化不是 osd 启动的硬前置（只有 VM 功能需要），打印诊断但不置失败
            println!("SKIP（{e}）");
        }
    }

    // 取组件列表（计数 + 依赖分析）
    let components = orch.list_components().await.unwrap_or_default();

    // 2. 组件依赖拓扑排序 + 循环检测
    print!(
        "[check] 组件依赖拓扑排序（{} 个组件）... ",
        components.len()
    );
    match orch.startup_order() {
        Ok(order) => {
            let order_str = order
                .iter()
                .map(ComponentId::as_str)
                .collect::<Vec<_>>()
                .join(" -> ");
            println!("OK（启动序: {order_str}）");
        }
        Err(e) => {
            println!("FAIL（{e}）");
            all_ok = false;
        }
    }

    // 3. systemd 可达性
    print!("[check] systemd 可达性... ");
    let backend_name = orch.systemd_runner().backend_name();
    if have_systemd() {
        println!("OK（PID1=systemd，后端={backend_name}）");
    } else {
        println!("SKIP（无 systemd，后端={backend_name} 内存态）");
    }

    // 4. cgroup v2 挂载检测
    print!("[check] cgroup v2 挂载... ");
    if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        println!("OK（/sys/fs/cgroup/cgroup.controllers 存在，cgroup v2 已挂载）");
    } else {
        println!("SKIP（/sys/fs/cgroup/cgroup.controllers 不存在，真实配额写入将失败）");
    }

    all_ok
}

/// 单组件模式：选出指定组件及其全部传递依赖（保持拓扑序）。
///
/// 返回 `Err(msg)` 表示组件未注册或编排器不可达；`Ok(set)` 为按拓扑序排列的待启动列表。
async fn select_component_and_deps(
    orch: &SystemdOrchestrator,
    target: &str,
    order: &[ComponentId],
) -> Result<Vec<ComponentId>, String> {
    let target_id = ComponentId::new(target);
    let list = orch.list_components().await.map_err(|e| e.to_string())?;
    if !list.iter().any(|d| d.id == target_id) {
        let available: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
        return Err(format!("组件 {target:?} 未注册（可用: {available:?}）"));
    }
    // 收集传递依赖（BFS over dependencies）
    let mut needed: HashSet<ComponentId> = HashSet::new();
    let mut stack = vec![target_id];
    while let Some(id) = stack.pop() {
        if needed.insert(id.clone()) {
            if let Some(desc) = list.iter().find(|d| d.id == id) {
                for dep in &desc.dependencies {
                    stack.push(dep.clone());
                }
            }
        }
    }
    // 按拓扑序过滤
    Ok(order
        .iter()
        .filter(|id| needed.contains(*id))
        .cloned()
        .collect())
}

/// 正常启动模式：拓扑序拉起全部/指定组件 → 等信号 → 逆序停止。
///
/// 若 `serve_api` 提供（`--serve-api <addr>`）：在组件启动后、阻塞等信号前，
/// 同进程内嵌启动 os-api 网关（`InProcessGateway::start` tokio::spawn axum::serve）；
/// 收到信号后先停网关（`gw.stop()` 优雅 drain）再逆序停组件。
async fn run_serve(
    orch: &SystemdOrchestrator,
    component: Option<&str>,
    serve_api: Option<&str>,
) -> ExitCode {
    // 计算启动顺序（拓扑排序）
    let order = match orch.startup_order() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[osd] 启动失败：组件依赖拓扑排序出错（{e}）");
            return ExitCode::FAILURE;
        }
    };

    // 单组件模式：只拉起指定组件及其依赖
    let to_start: Vec<ComponentId> = if let Some(id) = component {
        match select_component_and_deps(orch, id, &order).await {
            Ok(set) => set,
            Err(msg) => {
                eprintln!("[osd] {msg}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        order
    };

    // 按拓扑序启动
    eprintln!("[osd] 按拓扑序启动组件: {:?}", to_start);
    let mut failed = false;
    for id in &to_start {
        match orch.start(id).await {
            Ok(()) => eprintln!("[osd]   ✓ {} 启动成功", id),
            Err(e) => {
                eprintln!("[osd]   ✗ {} 启动失败: {}", id, e);
                failed = true;
                // 继续尝试启动其他组件（不因单组件失败整体退出）
            }
        }
    }
    if failed {
        eprintln!("[osd] 部分组件启动失败（见上），继续运行；信号到达后优雅停止");
    } else {
        eprintln!("[osd] 全部组件已启动，阻塞等待信号（SIGTERM/SIGINT）");
    }

    // --serve-api 模式：同进程内嵌启动 os-api 网关（tokio::spawn axum::serve）。
    // 组件启动成功后、阻塞等信号前启动；若 start 失败则继续主流程（不阻塞组件编排）。
    let embedded_gw = if let Some(addr) = serve_api {
        let gw = build_embedded_gateway().await;
        let routes = gw.list_routes().await;
        eprintln!(
            "[osd] --serve-api 内嵌网关 @ {addr}（{} 条路由，{} 层中间件，{} 个组件）",
            routes.len(),
            gw.middleware_count(),
            gw.component_count()
        );
        match gw.start(addr, None).await {
            Ok(()) => {
                eprintln!("[osd] 内嵌网关已开始监听 {addr}（随 SIGTERM/SIGINT 优雅关闭）");
                Some(gw)
            }
            Err(e) => {
                eprintln!("[osd] 内嵌网关启动失败（{e}），继续纯组件编排模式");
                None
            }
        }
    } else {
        None
    };

    // 等待信号
    wait_for_signal().await;

    // 内嵌网关优先优雅关闭（drain 在途 HTTP 连接），再逆序停组件。
    if let Some(gw) = embedded_gw {
        eprintln!("[osd] 优雅关闭内嵌网关");
        gw.stop().await;
        eprintln!("[osd] 内嵌网关已关闭");
    }

    // 逆拓扑序停止（依赖者先停，被依赖者后停）
    for id in to_start.iter().rev() {
        match orch.stop(id).await {
            Ok(()) => eprintln!("[osd]   ✓ {} 已停止", id),
            Err(e) => eprintln!("[osd]   ✗ {} 停止失败: {}", id, e),
        }
    }
    eprintln!("[osd] 全部组件已停止，退出");
    ExitCode::SUCCESS
}

/// 阻塞等待 SIGTERM/SIGINT（unix）或 stdin（非 unix）。
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => eprintln!("[osd] 收到 SIGTERM，开始优雅停止"),
            _ = sigint.recv() => eprintln!("[osd] 收到 SIGINT，开始优雅停止"),
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("[osd] 非 unix 平台无信号处理，按回车退出");
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let components = load_components(cli.config.as_ref());
    let registry = ComponentRegistry::from_descriptors(components);
    eprintln!("[osd] 已加载 {} 个组件", registry.len());

    let orch = build_orchestrator(registry);

    if cli.check {
        let ok = run_check(&orch).await;
        if ok {
            eprintln!("[osd] 预检通过（无硬失败项）");
            ExitCode::SUCCESS
        } else {
            eprintln!("[osd] 预检发现失败项");
            ExitCode::FAILURE
        }
    } else {
        run_serve(&orch, cli.component.as_deref(), cli.serve_api.as_deref()).await
    }
}
