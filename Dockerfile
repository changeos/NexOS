# syntax=docker/dockerfile:1.7
#
# OS System —— 一体化交付镜像（多阶段构建）
# =============================================================================
#
# 产出：可 `docker run` 即体验的完整 OS 镜像。容器入口 = osd 守护进程
# `--serve-api 0.0.0.0:8080`（同进程内嵌 os-api HTTP 网关）。
#
# 包含 3 个 binary：
#   - osd     系统编排守护进程 + 内嵌 API 网关（PID1 候选，本镜像 ENTRYPOINT）
#   - os-api  独立 HTTP 网关（备用入口）
#   - os      运维 CLI（容器内 `os ...` 调用本机网关）
#
# 设计要点：
#   - 多阶段构建：builder（rust:1.97）→ runtime（debian:bookworm-slim），最终镜像最小化。
#   - FFI 依赖（libvirt / nftnl / mnl）在源码中是 optional + 非默认 feature
#     （virt-ffi / nftnl-ffi），默认 `cargo build --workspace` 不编译 FFI 路径，
#     故 builder 阶段无需 libvirt-dev / libnftnl-dev / libmnl-dev。
#   - os-storage 的 ZfsCliBackend 走 `zfs`/`zpool` CLI（非 FFI），故 builder 不需要
#     ZFS 开发头；仅运行期需要 zfsutils-linux。
#   - 运行期装 zfsutils-linux（ZFS CLI）+ ca-certificates + libnftnl0（nftables 运行库）
#     + iproute2（rtnetlink 用户态）+ curl（健康探针/调试），覆盖 osd/os-storage/
#     os-network/os-guest 的运行期外部命令依赖。
#
# 构建：
#   docker build -t os-system .
#
# 运行（ZFS/nftables/cgroup 操作需特权 + 内核设施）：
#   docker run --rm -it --privileged --cgroupns=host \
#     -p 8080:8080 -v os-data:/data os-system
#
# 健康检查（容器内网关已起后）：
#   curl http://localhost:8080/healthz   # → {"status":"ok"}
#
# 注：完整 ZFS/KVM/nftables 真实操作依赖宿主内核模块与特权；纯 API 体验（/healthz、
# /api/v1/vms、/api/v1/users、/api/v1/nodes 等内存态路由）在非特权模式下亦可工作。

# ============================================================================
# Stage 1: 构建（rust:1.97，对齐 README 推荐 Rust 1.97+）
# ============================================================================
FROM rust:1.97 AS builder

# 避免 apt 交互卡构建。
ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# builder 系统依赖：仅通用编译工具链。
# - build-essential / pkg-config / curl / ca-certificates：Rust 编译 + 任何 build
#   script 的 pkg-config 探测通用前置。
# - libclang-dev：保险（部分 FFI 绑定的 bindgen 走 clang；默认 feature 路径不强依赖，
#   但装上避免偶发 build script 失败）。
# 注：故意不装 libvirt-dev / libnftnl-dev / libmnl-dev——对应 crate 的 FFI 是 optional
# + 非默认 feature，默认 workspace 构建不进入 FFI 路径（见各 crate [features]）。
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        curl \
        ca-certificates \
        libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 拷贝 workspace 描述与全部 crate 源码。
# （workspace 用相对 path 引用 crate，整体拷贝即可；进一步层缓存拆分收益有限。）
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# 编译整个 workspace（release）。FFI feature（virt-ffi/nftnl-ffi）默认关闭，
# 无需 libvirt-dev / libnftnl-dev / libmnl-dev。产出 3 个 binary 于 target/release/。
# rust:1.97 满足 workspace MSRV 1.75。
RUN cargo build --release --workspace

# ----------------------------------------------------------------------------
# Stage 2: 运行（最小化镜像，debian:bookworm-slim）
# ----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    RUST_BACKTRACE=short \
    TZ=UTC

# 运行期系统依赖：
# - zfsutils-linux：os-storage ZfsCliBackend 调 `zfs`/`zpool`（真实 ZFS 需宿主内核模块）。
# - libnftnl0：os-network/os-guest nftables 运行库（FFI feature 开启时；默认内存态也兼容）。
# - iproute2：os-network rtnetlink 用户态命令（ip / bridge）。
# - ca-certificates / curl：HTTPS 出站 + 健康探针（curl /healthz）。
# - tini：轻量 init，正确转发信号（SIGTERM/SIGINT）给 osd 做优雅关闭，避免 PID1 僵尸。
# procps / less：容器内调试辅助（ps / 日志翻页）。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        zfsutils-linux \
        libnftnl0 \
        iproute2 \
        ca-certificates \
        curl \
        tini \
        procps \
        less \
    && rm -rf /var/lib/apt/lists/*

# 拷贝 3 个 release binary（cargo build --release --workspace 产物路径固定）。
COPY --from=builder /build/target/release/osd    /usr/local/bin/osd
COPY --from=builder /build/target/release/os-api /usr/local/bin/os-api
COPY --from=builder /build/target/release/os     /usr/local/bin/os

# 数据卷（OS 元数据 / 配置 / ZFS loop 设备Backing 文件等持久化目录）。
VOLUME ["/data"]

# API 网关端口（osd --serve-api 0.0.0.0:8080）。
EXPOSE 8080

# tini 接管 PID1（正确转发 SIGTERM/SIGINT 给 osd 优雅关闭），osd 作 ENTRYPOINT。
# 默认启动 osd 一体化模式：组件编排 + 内嵌 HTTP 网关 @ 0.0.0.0:8080。
# 用户可覆盖 CMD 参数，如：docker run os-system osd --check
# 注：exec 形式用全路径，避免依赖 PATH 查找（exec form 不经 shell）。
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/osd"]
CMD ["--serve-api", "0.0.0.0:8080"]
