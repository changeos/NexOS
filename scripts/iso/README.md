# scripts/iso —— ISO rootfs 模板

本目录存放 **OS System 安装 ISO 的根文件系统（rootfs）模板**。
`build-iso.sh`（见下文「构建流程」）以此模板为骨架，注入编译产物后用
`mksquashfs` 打成 `filesystem.squashfs`，再由 `xorriso` 连同内核 / initrd /
GRUB 一起生成可引导 ISO。

> 红线：本目录是**纯模板**（静态配置 + 骨架），不打包仓库源码、不内嵌编译产物。
> binary 在构建时由 `build-iso.sh` 从 `target/release/` 注入。

---

## 目录结构

```
scripts/iso/
├── README.md                              ← 本文件
└── rootfs-template/                       ← squashfs 源目录（mksquashfs 的 source-root）
    ├── usr/
    │   └── local/
    │       └── bin/                       ← 4 个 Rust binary 安装位置（COPY from cargo build）
    │           └── .gitkeep               （占位；构建时注入 osd/os-api/os-mcp/os）
    ├── etc/
    │   ├── systemd/
    │   │   └── system/
    │   │       └── osd.service           ← osd 自启（--serve-api 0.0.0.0:8080）
    │   ├── os/
    │   │   ├── osd.conf                  ← osd 组件注册（storage→network→api 依赖图）
    │   │   └── api.conf                   ← os-api 路由 + 中间件（cors/auth/log/quota）
    │   ├── os-release                     ← OS OS 版本信息（lsb_release / cockpit 读取）
    │   └── fstab                          ← ZFS 挂载点骨架（installer 重写）
    ├── var/
    │   └── lib/
    │       └── os/                       ← OS 数据目录（osd 运行时写入）
    │           ├── pools/                 ← 池配置元数据
    │           ├── snapshots/             ← 快照元数据
    │           └── backups/               ← 备份元数据
    └── boot/
        └── grub/
            └── grub.cfg                   ← GRUB 菜单（OS Live / OS Install）
```

### 关键文件说明

| 文件 | 作用 |
|------|------|
| `etc/systemd/system/osd.service` | systemd unit：开机自启 `osd --serve-api 0.0.0.0:8080`，`on-failure` 重启（RestartSec=3s），`WantedBy=multi-user.target`。安装到目标盘后 `systemctl enable osd.service`。 |
| `etc/os/osd.conf` | 组件注册表（JSON 数组）：`storage`（无依赖）→ `network`（依赖 storage）→ `api`（依赖 network）。osd 按依赖图拓扑排序启动 / 逆序关停。 |
| `etc/os/api.conf` | HTTP 网关配置：监听 `0.0.0.0:8080`，路由前缀表（system/storage/network/snapshot/backup/compute），中间件开关（cors 开 / auth 关 / logging 开 / quota 600/min）。 |
| `etc/os-release` | `NAME="OS System"` / `VERSION="0.1.0"` / `ID=os-system`，供 `lsb_release -a`、cockpit、监控脚本识别系统身份。 |
| `etc/fstab` | Live 模式根由 casper 注入（overlay）；此处仅声明 ZFS 数据池挂载点骨架与 tmpfs，**installer 按 pool/dataset 自动重写**。 |
| `boot/grub/grub.cfg` | ISO 引导菜单：`OS System (Live)`（default）/ `OS System (Install)`（带 `install` 内核参数）/ `(Live, safe graphics)`（nomodeset）。casper 负责 live rootfs 加载。 |
| `var/lib/os/{pools,snapshots,backups}/` | osd 运行时数据目录骨架，安装时即就绪，避免首启因缺目录报错。 |

---

## 构建流程（build-iso.sh 如何使用本模板）

`build-iso.sh`（尚未提交，计划与 `scripts/iso/` 同批落地）的典型步骤：

```bash
#!/usr/bin/env bash
# scripts/iso/build-iso.sh （示意）
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
TPL="$REPO_ROOT/scripts/iso/rootfs-template"
WORK="$(mktemp -d)/rootfs"

# 1) 复制模板到工作目录（不污染模板源）。
rsync -a "$TPL/" "$WORK/"

# 2) 编译 4 个 binary（release，静态优先）。
cargo build --release -p osd -p os-api -p os-mcp -p os-cli

# 3) 注入 binary 到 usr/local/bin/。
install -m 0755 \
    "$REPO_ROOT/target/release/osd"     "$WORK/usr/local/bin/osd"
install -m 0755 \
    "$REPO_ROOT/target/release/os-api"  "$WORK/usr/local/bin/os-api"
install -m 0755 \
    "$REPO_ROOT/target/release/os-mcp"  "$WORK/usr/local/bin/os-mcp"
install -m 0755 \
    "$REPO_ROOT/target/release/os"      "$WORK/usr/local/bin/os"

# 4)（可选）启用 osd 自启：在 squashfs 内建立 symlink。
mkdir -p "$WORK/etc/systemd/system/multi-user.target.wants"
ln -sf ../osd.service \
    "$WORK/etc/systemd/system/multi-user.target.wants/osd.service"

# 5) 打 squashfs → xorriso 产出 ISO。
#    mksquashfs "$WORK" filesystem.squashfs -comp zstd -Xcompression-level 19
#    xorriso -as mkisofs ... -b boot/grub/... -o os-system-0.1.0.iso
```

> 注：第 5 步的 `mksquashfs` / `xorriso` 参数派生逻辑已在
> `crates/os-iso/src/impl_iso.rs`（`XorrisoIsoBuilder`）实现，
> `squashfs_pack_args` / `xorriso_build_args` 为纯函数。
> `build-iso.sh` 既可手写裸命令（如上示意），也可调用 `cargo run -p os-iso` 的
> ISO builder API；两条路径共用同一份模板。

### 与 os-iso crate 的关系

- **本模板** = 静态 rootfs 内容（配置 / 骨架）。
- **`crates/os-iso`** = ISO 构建引擎（xorriso + squashfs 命令派生 / runner spawn / spec 校验）。
- **`build-iso.sh`**（待落地）= 编排脚本：模板 + 编译产物 → `os-iso` builder → ISO。

### ISO 沙箱

真实 `xorriso` / `squashfs-tools` 端到端测试环境见
[`scripts/sandbox/docker/Dockerfile.iso`](../sandbox/docker/Dockerfile.iso)，
使用说明见 [`docs/SANDBOX.md`](../../docs/SANDBOX.md)。

---

## 修改约定

- 改 systemd unit / osd.conf / api.conf 需同步更新
  `crates/osd` 与 `crates/os-api` 的解析逻辑（契约对齐）。
- 改 `os-release` 版本号需与根 `Cargo.toml` `version` 一致（当前 0.1.0）。
- 改 `grub.cfg` 菜单项需同步 `crates/os-iso` 的 `BootConfig` 派生（若已硬编码）。
- 红线：本目录**只加 / 改模板文件**，不动 workspace Rust 源码。
