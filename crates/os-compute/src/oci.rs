//! OCI 运行时规格（`config.json`）生成 + **真实落盘**。
//!
//! 定位：把容器规格 [`crate::ContainerSpec`] 翻译成 [OCI Runtime Spec][oci-spec]
//! 的 `config.json` 结构，并写入 OCI bundle 目录，交由 youki（OCI runtime）消费。
//!
//! 分两步：
//! - [`build_oci_spec`]：纯函数，`ContainerSpec → OciSpec`（内存结构）；
//! - [`write_config_json`] / [`write_bundle`]：把 spec 序列化为 `config.json` 并落盘到
//!   bundle 目录（youki create 要求 bundle 根有 `config.json` + `rootfs/`）。
//!
//! **为什么不引第三方 `oci-spec` crate**：该 crate 尚未在 workspace 注册
//! （见规格书 §9 红线「不得虚构未注册依赖」）。本模块以最小自洽 serde 结构
//! 覆盖 youki create 所需字段（process/root/mounts/linux），实现层（`YoukiRuntime`，
//! 批 3 引入 youki 后）可直接消费本模块写盘的 `config.json`。
//!
//! [oci-spec]: https://github.com/opencontainers/runtime-spec/blob/main/config.md
//!
//! 设计要点：
//! - `MountSource::Bind` → OCI `mount.type=bind` + `source=<host path>`；
//! - `MountSource::Volume` → zvol 块卷，挂载点解析由实现层从 `VolumeId` 取宿主路径
//!   （此处仅记录 volume_id 字符串，youki 实现层负责解析 `/dev/zvol/...`）；
//! - 端口映射通过 annotations 透传给 CNI portmap 插件（youki 自身不处理 NAT）；
//! - env 按 `KEY=VALUE` 列表注入 `process.env`。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::container::{ContainerMount, ContainerSpec, MountSource, PortMapping};
use crate::error::{ComputeError, ComputeResult};

// ----------------------------------------------------------------------------
// OCI Runtime Spec 最小子集（仅覆盖本 crate 生成的字段）
// ----------------------------------------------------------------------------

/// OCI `config.json` 顶层结构（runtime spec 最小子集）。
///
/// 字段命名严格对齐 OCI spec（`ociVersion`/`process`/`root`/`mounts`/`linux`/`annotations`），
/// 便于未来切换到官方 `oci-spec` crate 时零迁移成本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OciSpec {
    /// OCI runtime spec 版本（当前固定 `1.0.2-dev`）
    #[serde(rename = "ociVersion")]
    pub oci_version: String,
    /// 进程（入口 + 参数 + env）
    pub process: OciProcess,
    /// 根文件系统
    pub root: OciRoot,
    /// 主机名（容器名）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// 挂载点列表（bind / volume → OCI mount）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<OciMount>,
    /// Linux 平台特定配置（cgroups/namespace 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linux: Option<OciLinux>,
    /// 自由标注——端口映射 / 卷来源等元信息透传给 CNI/youki 插件
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

/// OCI 进程描述
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OciProcess {
    /// 终端模式（默认 false，容器作为后台服务运行）
    #[serde(default)]
    pub terminal: bool,
    /// 用户（默认 root:root，匹配多数镜像 ENTRYPOINT 期望）
    #[serde(default)]
    pub user: OciUser,
    /// 启动参数（argv\[0\] = 可执行 + argv\[1..\] = 参数）
    pub args: Vec<String>,
    /// 环境变量（`KEY=VALUE`）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// 工作目录（None = 镜像默认）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// OCI 用户
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OciUser {
    /// UID（默认 0 = root）
    pub uid: u32,
    /// GID（默认 0 = root）
    pub gid: u32,
}

/// OCI 根文件系统
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OciRoot {
    /// bundle 内根 fs 路径（固定 `rootfs`，youki 标准）
    pub path: PathBuf,
    /// 是否只读根 fs（默认 false，多数容器需可写）
    #[serde(default)]
    pub readonly: bool,
}

/// OCI 挂载（覆盖 bind/proc/sys/tmpfs，volume 由实现层翻译为 bind）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OciMount {
    /// 目标路径（容器内）
    pub destination: PathBuf,
    /// 挂载类型（bind / proc / sysfs / tmpfs / none）
    #[serde(rename = "type")]
    pub kind: String,
    /// 源（host 路径，None = 伪文件系统如 proc）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// 挂载选项（ro / rw / rshared 等）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// OCI Linux 平台特定配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OciLinux {
    /// 命名空间（pid/net/ipc/uts/mount/user/cgroup）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<OciNamespace>,
    /// cgroup 路径（如 `/os/<container_id>`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroups_path: Option<String>,
    /// 资源限制（内存/CPU）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<OciResources>,
}

/// OCI namespace
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OciNamespace {
    /// namespace 类型（pid / network / ipc / uts / mount / user / cgroup）
    #[serde(rename = "type")]
    pub kind: String,
    /// 路径（None = 创建新 namespace）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// OCI 资源限制
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OciResources {
    /// 内存限制（字节，None = 不限）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<u64>,
    /// CPU 配额（微秒/100ms 周期，None = 不限）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_quota: Option<u64>,
}

// ----------------------------------------------------------------------------
// 默认命名空间集（标准容器隔离）
// ----------------------------------------------------------------------------

/// 返回标准容器隔离的命名空间集（pid/network/ipc/uts/mount/cgroup，均新建）。
///
/// 缺 user namespace——OS 容器默认以 root 跑（youki rootful），user namespace
/// 由实现层按安全策略决定（规格书 §9「不耦合 youki 内部 API」）。
pub fn default_namespaces() -> Vec<OciNamespace> {
    ["pid", "network", "ipc", "uts", "mount", "cgroup"]
        .into_iter()
        .map(|k| OciNamespace {
            kind: k.to_string(),
            path: None,
        })
        .collect()
}

// ----------------------------------------------------------------------------
// 转换：ContainerSpec → OciSpec
// ----------------------------------------------------------------------------

/// OCI spec 当前固定版本（runtime-spec 1.0.2-dev）。
pub const OCI_VERSION: &str = "1.0.2-dev";

/// 把单个 [`ContainerMount`] 翻译成 OCI [`OciMount`]。
///
/// - `MountSource::Bind { path }` → `type=bind, source=path`
/// - `MountSource::Volume { volume_id }` → `type=bind, source=<resolved>`
///   （此处 source 写占位 `/dev/zvol/<id>`，实现层在调 youki 前可改写为真实路径；
///   保留 volume_id 在 annotations 供 youki hook 解析）
///
/// `read_only=true` 时附加 `ro` 选项，否则 `rw` + `rshared`（双向传播，便于挂载变更可见）。
pub fn mount_to_oci(m: &ContainerMount, annotations: &mut HashMap<String, String>) -> OciMount {
    let (kind, source) = match &m.source {
        MountSource::Bind { path } => ("bind".to_string(), Some(path.clone())),
        MountSource::Volume { volume_id } => {
            // 占位路径——youki 实现层负责把 volume_id 解析成 /dev/zvol/<pool>/<vol>
            // 后改写 source（或交由 storage-agent hook）。annotations 透传 volume_id 以便审计。
            annotations.insert(
                format!("io.os.volume.{}", m.target.display()),
                volume_id.to_string(),
            );
            let placeholder = PathBuf::from(format!("/dev/zvol/{}", volume_id));
            ("bind".to_string(), Some(placeholder))
        }
    };

    let mut options = Vec::new();
    if m.read_only {
        options.push("ro".to_string());
    } else {
        options.push("rw".to_string());
        options.push("rshared".to_string());
    }

    OciMount {
        destination: m.target.clone(),
        kind,
        source,
        options,
    }
}

/// 把端口映射列表序列化进 annotations（`io.os.ports` = JSON）。
///
/// youki/OCI 本身不处理 NAT——端口映射由 CNI portmap 插件在 ADD 阶段读 annotations
/// 完成。序列化格式与 [`crate::cni`] 的 portmap capabilities 对齐。
pub fn ports_to_annotations(ports: &[PortMapping], annotations: &mut HashMap<String, String>) {
    if ports.is_empty() {
        return;
    }
    // 紧凑 JSON：[{"hostPort":H,"containerPort":C,"protocol":"tcp"|"udp"}, ...]
    let entries: Vec<String> = ports
        .iter()
        .map(|p| {
            let proto = protocol_str(p.protocol);
            format!(
                r#"{{"hostPort":{},"containerPort":{},"protocol":"{}"}}"#,
                p.host_port, p.container_port, proto
            )
        })
        .collect();
    annotations.insert(
        "io.os.ports".to_string(),
        format!("[{}]", entries.join(",")),
    );
}

/// 把 os_network::Protocol 映射成 CNI/OCI 期望的小写串（tcp/udp/空）。
fn protocol_str(p: os_network::Protocol) -> &'static str {
    use os_network::Protocol;
    match p {
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
        Protocol::Any => "",
    }
}

/// 把 [`ContainerSpec`] 翻译成 OCI [`OciSpec`]，可用于写盘交给 youki。
///
/// 校验：
/// - `image` 非空（OCI 无镜像概念，但本 crate 用 image 作 bundle 根 fs 来源标识）；
/// - `command` 非空时作为 `process.args`，否则用 `/bin/sh`（占位，实现层应从镜像 config
///   读 ENTRYPOINT/CMD 回填，此处无法访问镜像）。
///
/// `bundle_root`：OCI bundle 目录，rootfs 路径固定为 `bundle_root/rootfs`。
/// `cgroup_path`：可选 cgroup 路径（如 `/os/<id>`），None 时省略。
pub fn build_oci_spec(
    spec: &ContainerSpec,
    bundle_root: &std::path::Path,
    cgroup_path: Option<&str>,
) -> ComputeResult<OciSpec> {
    if spec.image.trim().is_empty() {
        return Err(ComputeError::InvalidSpec("容器镜像不能为空".to_string()));
    }

    let args = if spec.command.is_empty() {
        // 占位——实现层（YoukiRuntime）应在 pull_image 后从镜像 manifest.config
        // 读出 Entrypoint/Cmd 覆盖此处。无法在此访问镜像，故回退到 /bin/sh。
        vec!["/bin/sh".to_string()]
    } else {
        spec.command.clone()
    };

    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let mut annotations = HashMap::new();
    annotations.insert("io.os.image".to_string(), spec.image.clone());
    if let Some(net) = &spec.network {
        annotations.insert("io.os.network".to_string(), net.clone());
    }
    let mounts: Vec<OciMount> = spec
        .mounts
        .iter()
        .map(|m| mount_to_oci(m, &mut annotations))
        .collect();
    ports_to_annotations(&spec.ports, &mut annotations);

    let linux = OciLinux {
        namespaces: default_namespaces(),
        cgroups_path: cgroup_path.map(|s| s.to_string()),
        resources: None,
    };

    Ok(OciSpec {
        oci_version: OCI_VERSION.to_string(),
        process: OciProcess {
            terminal: false,
            user: OciUser::default(),
            args,
            env,
            cwd: None,
        },
        root: OciRoot {
            path: bundle_root.join("rootfs"),
            readonly: false,
        },
        hostname: None,
        mounts,
        linux: Some(linux),
        annotations,
    })
}

/// 把 OCI spec 序列化成 `config.json` 字符串（pretty，便于人审/diff）。
pub fn to_config_json(spec: &OciSpec) -> ComputeResult<String> {
    serde_json::to_string_pretty(spec)
        .map_err(|e| ComputeError::Internal(format!("OCI spec 序列化失败: {e}")))
}

/// OCI bundle 目录里 `config.json` 的文件名（youki/runtime-spec 约定）。
pub const CONFIG_JSON_FILENAME: &str = "config.json";

/// 把 OCI spec 序列化后写入 `<bundle_dir>/config.json`。
///
/// `bundle_dir` 须存在（调用方负责 `create_dir_all`）；本函数不创建 bundle 目录，
/// 仅写文件——这样调用方可控决定目录布局（`write_bundle` 封装了含建目录的便捷路径）。
/// 若文件已存在则覆盖（幂等重写）。
pub fn write_config_json(
    spec: &OciSpec,
    bundle_dir: &std::path::Path,
) -> ComputeResult<std::path::PathBuf> {
    let json = to_config_json(spec)?;
    let target = bundle_dir.join(CONFIG_JSON_FILENAME);
    std::fs::write(&target, json)?;
    Ok(target)
}

/// 一站式：从 [`ContainerSpec`] 生成 spec、建 bundle 目录、写 `config.json`。
///
/// - `bundle_root`：OCI bundle 根（`config.json` 落盘处，rootfs 路径相对此为 `<root>/rootfs`）；
/// - `cgroup_path`：可选 cgroup 路径（写入 `linux.cgroupsPath`）。
///
/// 目录已存在不报错（幂等）；返回写入的 `config.json` 完整路径。
/// 注：本函数不创建 `rootfs/` 目录（rootfs 内容由 pull_image 后解包填充，归实现层）。
pub fn write_bundle(
    spec: &ContainerSpec,
    bundle_root: &std::path::Path,
    cgroup_path: Option<&str>,
) -> ComputeResult<std::path::PathBuf> {
    std::fs::create_dir_all(bundle_root)?;
    let oci = build_oci_spec(spec, bundle_root, cgroup_path)?;
    write_config_json(&oci, bundle_root)
}

/// 从已写盘的 bundle 目录读回 `config.json` 并反序列化（往返校验/审计用）。
pub fn read_config_json(bundle_dir: &std::path::Path) -> ComputeResult<OciSpec> {
    let target = bundle_dir.join(CONFIG_JSON_FILENAME);
    let content = std::fs::read_to_string(&target)?;
    serde_json::from_str(&content)
        .map_err(|e| ComputeError::Internal(format!("config.json 反序列化失败: {e}")))
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{ContainerMount, ContainerSpec, MountSource, PortMapping};
    use os_core::VolumeId;
    use os_network::Protocol;
    use std::path::Path;

    fn spec_fixture() -> ContainerSpec {
        let mut env = std::collections::HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        ContainerSpec {
            image: "nginx:1.25".to_string(),
            command: vec![
                "nginx".to_string(),
                "-g".to_string(),
                "daemon off;".to_string(),
            ],
            env,
            mounts: vec![
                ContainerMount {
                    source: MountSource::Bind {
                        path: PathBuf::from("/srv/www"),
                    },
                    target: PathBuf::from("/usr/share/nginx/html"),
                    read_only: true,
                },
                ContainerMount {
                    source: MountSource::Volume {
                        volume_id: VolumeId::new("tank/pg-data"),
                    },
                    target: PathBuf::from("/var/lib/postgresql/data"),
                    read_only: false,
                },
            ],
            ports: vec![
                PortMapping {
                    host_port: 8080,
                    container_port: 80,
                    protocol: Protocol::Tcp,
                },
                PortMapping {
                    host_port: 8443,
                    container_port: 443,
                    protocol: Protocol::Tcp,
                },
            ],
            network: Some("osnet".to_string()),
        }
    }

    #[test]
    fn build_spec_translates_command_env_and_root() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/var/lib/os/bundles/c1"), None).unwrap();

        assert_eq!(oci.process.args, vec!["nginx", "-g", "daemon off;"]);
        assert_eq!(oci.process.env, vec!["PATH=/usr/bin:/bin"]);
        assert_eq!(
            oci.root.path,
            PathBuf::from("/var/lib/os/bundles/c1/rootfs")
        );
        assert_eq!(oci.oci_version, OCI_VERSION);
    }

    #[test]
    fn build_spec_default_args_when_command_empty() {
        let mut spec = spec_fixture();
        spec.command.clear();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert_eq!(oci.process.args, vec!["/bin/sh"]);
    }

    #[test]
    fn build_spec_rejects_empty_image() {
        let mut spec = spec_fixture();
        spec.image = "  ".to_string();
        let err = build_oci_spec(&spec, Path::new("/b"), None).unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn bind_mount_becomes_bind_type_with_ro() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let html = oci
            .mounts
            .iter()
            .find(|m| m.destination == Path::new("/usr/share/nginx/html"))
            .unwrap();
        assert_eq!(html.kind, "bind");
        assert_eq!(html.source.as_deref(), Some(Path::new("/srv/www")));
        assert!(html.options.contains(&"ro".to_string()));
    }

    #[test]
    fn volume_mount_writes_placeholder_and_annotation() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let pg = oci
            .mounts
            .iter()
            .find(|m| m.destination == Path::new("/var/lib/postgresql/data"))
            .unwrap();
        // source 是占位路径
        assert_eq!(
            pg.source.as_deref(),
            Some(Path::new("/dev/zvol/tank/pg-data"))
        );
        assert!(pg.options.contains(&"rw".to_string()));
        // annotation 透传 volume_id
        let vol_ann = oci
            .annotations
            .get("io.os.volume./var/lib/postgresql/data")
            .unwrap();
        assert_eq!(vol_ann, "tank/pg-data");
    }

    #[test]
    fn ports_serialized_to_annotation_as_json() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let ports_ann = oci.annotations.get("io.os.ports").unwrap();
        assert!(ports_ann.contains(r#""hostPort":8080"#));
        assert!(ports_ann.contains(r#""containerPort":443"#));
        assert!(ports_ann.contains(r#""protocol":"tcp""#));
    }

    #[test]
    fn network_and_image_recorded_in_annotations() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert_eq!(oci.annotations.get("io.os.image").unwrap(), "nginx:1.25");
        assert_eq!(oci.annotations.get("io.os.network").unwrap(), "osnet");
    }

    #[test]
    fn default_namespaces_has_six_kinds() {
        let ns = default_namespaces();
        let kinds: Vec<&str> = ns.iter().map(|n| n.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["pid", "network", "ipc", "uts", "mount", "cgroup"]
        );
    }

    #[test]
    fn cgroup_path_propagates_to_linux_block() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), Some("/os/c1")).unwrap();
        assert_eq!(
            oci.linux.as_ref().unwrap().cgroups_path.as_deref(),
            Some("/os/c1")
        );
    }

    #[test]
    fn to_config_json_roundtrip_parses_back() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let json = to_config_json(&oci).unwrap();
        let back: OciSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, oci);
    }

    // --------------------------------------------------------------------
    // 落盘测（tempdir 真实文件系统往返）
    // --------------------------------------------------------------------

    #[test]
    fn write_config_json_creates_file_under_bundle_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path();
        let oci = build_oci_spec(&spec_fixture(), bundle, None).unwrap();

        let path = write_config_json(&oci, bundle).unwrap();
        assert_eq!(path.file_name().unwrap(), CONFIG_JSON_FILENAME);
        assert!(path.is_file());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(r#""ociVersion""#));
        assert!(content.contains(r#""args""#));
    }

    #[test]
    fn write_config_json_is_idempotent_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path();
        let oci = build_oci_spec(&spec_fixture(), bundle, None).unwrap();
        // 写两次：第二次覆盖第一次
        write_config_json(&oci, bundle).unwrap();
        write_config_json(&oci, bundle).unwrap();
        let entries = std::fs::read_dir(bundle).unwrap().count();
        assert_eq!(entries, 1, "config.json 应被覆盖而非新增");
    }

    #[test]
    fn write_bundle_creates_dir_and_writes_config_json() {
        let tmp = tempfile::tempdir().unwrap();
        // bundle 目录尚不存在
        let bundle = tmp.path().join("c1");
        assert!(!bundle.exists());

        let path = write_bundle(&spec_fixture(), &bundle, Some("/os/c1")).unwrap();
        assert!(bundle.is_dir());
        assert!(path.is_file());
        assert_eq!(path, bundle.join(CONFIG_JSON_FILENAME));

        // 读回校验 cgroup 路径落盘正确
        let back = read_config_json(&bundle).unwrap();
        assert_eq!(
            back.linux.as_ref().unwrap().cgroups_path.as_deref(),
            Some("/os/c1")
        );
        assert_eq!(back.process.args, vec!["nginx", "-g", "daemon off;"]);
    }

    #[test]
    fn write_bundle_roundtrip_preserves_mounts_and_annotations() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("c2");
        write_bundle(&spec_fixture(), &bundle, None).unwrap();
        let back = read_config_json(&bundle).unwrap();

        // mounts 完整保留
        assert_eq!(back.mounts.len(), 2);
        let html = back
            .mounts
            .iter()
            .find(|m| m.destination == Path::new("/usr/share/nginx/html"))
            .unwrap();
        assert_eq!(html.kind, "bind");
        // annotations 完整保留
        assert_eq!(back.annotations.get("io.os.image").unwrap(), "nginx:1.25");
        assert!(back.annotations.contains_key("io.os.ports"));
    }

    #[test]
    fn read_config_json_missing_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_json(tmp.path()).unwrap_err();
        assert!(matches!(err, ComputeError::Io(_)));
    }

    // --------------------------------------------------------------------
    // 补充测：结构验证 / JSON 字段名 / env 格式 / protocol 边界
    // --------------------------------------------------------------------

    #[test]
    fn oci_spec_serializes_with_camel_case_field_names() {
        // OCI spec 字段名严格对齐 OCI spec：ociVersion / process / root
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let json = to_config_json(&oci).unwrap();
        assert!(json.contains(r#""ociVersion""#), "应输出 ociVersion");
        assert!(!json.contains(r#""oci_version""#), "不应输出 snake_case");
    }

    #[test]
    fn oci_spec_serializes_oci_version_value() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let json = to_config_json(&oci).unwrap();
        assert!(json.contains(&format!(r#""ociVersion": "{}"#, OCI_VERSION)));
    }

    #[test]
    fn oci_spec_user_defaults_to_root() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert_eq!(oci.process.user.uid, 0);
        assert_eq!(oci.process.user.gid, 0);
    }

    #[test]
    fn oci_spec_process_terminal_false() {
        // 后台服务容器：terminal = false
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(!oci.process.terminal);
    }

    #[test]
    fn oci_spec_env_formatted_as_key_eq_value() {
        // 多个 env：KEY=VALUE 列表
        let mut spec = spec_fixture();
        spec.env.insert("LANG".to_string(), "C.UTF-8".to_string());
        spec.env.insert("TERM".to_string(), "xterm".to_string());
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        // 每条 env 应是 K=V 格式
        for env in &oci.process.env {
            assert!(env.contains('='), "env 应含 =: {env}");
        }
        assert!(oci.process.env.iter().any(|e| e == "PATH=/usr/bin:/bin"));
        assert!(oci.process.env.iter().any(|e| e == "LANG=C.UTF-8"));
    }

    #[test]
    fn oci_spec_env_empty_when_no_env() {
        let mut spec = spec_fixture();
        spec.env.clear();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(oci.process.env.is_empty());
    }

    #[test]
    fn oci_spec_root_readonly_false() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(!oci.root.readonly);
    }

    #[test]
    fn oci_spec_hostname_none_by_default() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(oci.hostname.is_none());
    }

    #[test]
    fn oci_spec_linux_resources_none_by_default() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let linux = oci.linux.as_ref().unwrap();
        assert!(linux.resources.is_none());
    }

    #[test]
    fn oci_spec_linux_has_six_namespaces_all_new() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let linux = oci.linux.as_ref().unwrap();
        assert_eq!(linux.namespaces.len(), 6);
        // 所有 namespace 路径都为 None（新建）
        for ns in &linux.namespaces {
            assert!(
                ns.path.is_none(),
                "namespace {:?} 应新建（无 path）",
                ns.kind
            );
        }
    }

    #[test]
    fn oci_spec_cwd_none_by_default() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(oci.process.cwd.is_none());
    }

    #[test]
    fn oci_spec_image_annotation_always_present() {
        // 即使无 mounts/ports/network，image annotation 也必有
        let mut spec = spec_fixture();
        spec.mounts.clear();
        spec.ports.clear();
        spec.network = None;
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert_eq!(oci.annotations.get("io.os.image").unwrap(), "nginx:1.25");
    }

    #[test]
    fn oci_spec_no_network_no_annotation() {
        let mut spec = spec_fixture();
        spec.network = None;
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(!oci.annotations.contains_key("io.os.network"));
    }

    #[test]
    fn oci_spec_empty_ports_no_annotation() {
        let mut spec = spec_fixture();
        spec.ports.clear();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        assert!(!oci.annotations.contains_key("io.os.ports"));
    }

    #[test]
    fn oci_spec_empty_mounts_no_volume_annotation() {
        let mut spec = spec_fixture();
        spec.mounts.clear();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        // 无 mount → 无任何 io.os.volume.* 注解
        assert!(oci
            .annotations
            .keys()
            .all(|k| !k.starts_with("io.os.volume.")));
    }

    #[test]
    fn oci_spec_udp_protocol_in_ports_annotation() {
        let mut spec = spec_fixture();
        spec.ports = vec![PortMapping {
            host_port: 53,
            container_port: 53,
            protocol: Protocol::Udp,
        }];
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let ann = oci.annotations.get("io.os.ports").unwrap();
        assert!(ann.contains(r#""protocol":"udp""#));
    }

    #[test]
    fn oci_spec_any_protocol_empty_string() {
        let mut spec = spec_fixture();
        spec.ports = vec![PortMapping {
            host_port: 80,
            container_port: 8080,
            protocol: Protocol::Any,
        }];
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let ann = oci.annotations.get("io.os.ports").unwrap();
        // protocol "Any" 映射为空串
        assert!(ann.contains(r#""protocol":"""#));
    }

    #[test]
    fn mount_to_oci_bind_readonly_options_only_ro() {
        let mut ann = HashMap::new();
        let m = ContainerMount {
            source: MountSource::Bind {
                path: PathBuf::from("/host"),
            },
            target: PathBuf::from("/container"),
            read_only: true,
        };
        let om = mount_to_oci(&m, &mut ann);
        assert_eq!(om.kind, "bind");
        assert_eq!(om.source.as_deref(), Some(Path::new("/host")));
        assert_eq!(om.options, vec!["ro"]);
        // bind 不产生 volume annotation
        assert!(ann.is_empty());
    }

    #[test]
    fn mount_to_oci_bind_writable_has_rshared() {
        let mut ann = HashMap::new();
        let m = ContainerMount {
            source: MountSource::Bind {
                path: PathBuf::from("/host"),
            },
            target: PathBuf::from("/container"),
            read_only: false,
        };
        let om = mount_to_oci(&m, &mut ann);
        assert_eq!(om.options, vec!["rw", "rshared"]);
    }

    #[test]
    fn ports_to_annotations_empty_does_nothing() {
        let mut ann = HashMap::new();
        ports_to_annotations(&[], &mut ann);
        assert!(!ann.contains_key("io.os.ports"));
    }

    #[test]
    fn oci_spec_partial_eq_on_roundtrip() {
        // PartialEq 派生：往返序列化后应相等
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), Some("/cg")).unwrap();
        let json = to_config_json(&oci).unwrap();
        let back: OciSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, oci);
    }

    #[test]
    fn oci_spec_clone_is_equal() {
        let spec = spec_fixture();
        let oci = build_oci_spec(&spec, Path::new("/b"), None).unwrap();
        let cloned = oci.clone();
        assert_eq!(cloned, oci);
    }

    #[test]
    fn write_config_json_missing_dir_returns_io_error() {
        // bundle_dir 父目录不存在 → Io 错误
        let oci = OciSpec {
            oci_version: OCI_VERSION.to_string(),
            process: OciProcess {
                terminal: false,
                user: OciUser::default(),
                args: vec!["sh".to_string()],
                env: vec![],
                cwd: None,
            },
            root: OciRoot {
                path: PathBuf::from("rootfs"),
                readonly: false,
            },
            hostname: None,
            mounts: vec![],
            linux: None,
            annotations: HashMap::new(),
        };
        let err = write_config_json(&oci, std::path::Path::new("/nonexistent/xyz123/bundle"))
            .unwrap_err();
        assert!(matches!(err, ComputeError::Io(_)));
    }

    #[test]
    fn build_oci_spec_image_with_only_whitespace_rejected() {
        let mut spec = spec_fixture();
        spec.image = "\t \n".to_string();
        let err = build_oci_spec(&spec, Path::new("/b"), None).unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn read_config_json_malformed_json_returns_internal() {
        let tmp = tempfile::tempdir().unwrap();
        // 写非法 JSON
        std::fs::write(tmp.path().join(CONFIG_JSON_FILENAME), "{not json").unwrap();
        let err = read_config_json(tmp.path()).unwrap_err();
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[test]
    fn write_bundle_creates_nested_dirs() {
        // bundle_root 含多层未存在的子目录
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("a").join("b").join("c");
        assert!(!bundle.exists());

        let path = write_bundle(&spec_fixture(), &bundle, None).unwrap();
        assert!(path.is_file());
        assert!(bundle.is_dir());
    }
}
