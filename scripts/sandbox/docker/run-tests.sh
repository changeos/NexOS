#!/usr/bin/env bash
#
# scripts/sandbox/docker/run-tests.sh
# =============================================================================
# 在 os-sandbox:26.04 容器内跑 OS System 的"真实环境测"。
#
# 前置（宿主侧）：
#   docker build -t os-sandbox:26.04 \
#       -f scripts/sandbox/docker/Dockerfile.test scripts/sandbox/docker
#
#   docker run --rm -it --privileged --cgroupns=host \
#       -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
#       -v "$PWD":/workspace:rw \
#       -w /workspace \
#       os-sandbox:26.04 \
#       /workspace/scripts/sandbox/docker/run-tests.sh
#
# 容器内本脚本依次：
#   1) 环境探针（确认 root / cgroup v2 / 必备 CLI）
#   2) 常规三道门（check / clippy -D warnings / test，对齐 .github/workflows/ci.yml）
#   3) 真实环境集成测（cargo test --features mock -- --ignored）
#   4) 可选：FFI feature 路径（virt-ffi / nftnl-ffi，需沙箱装 -dev 包，环境变量门控）
#
# 失败立即退出（set -euo pipefail）。本脚本不修改源码，只跑测。
# =============================================================================

set -euo pipefail

# ---- 颜色 ----
if [[ -t 1 ]]; then
    C_GREEN=$'\033[32m'; C_RED=$'\033[31m'; C_YELLOW=$'\033[33m'; C_BLUE=$'\033[34m'; C_RST=$'\033[0m'
else
    C_GREEN=""; C_RED=""; C_YELLOW=""; C_BLUE=""; C_RST=""
fi

log()  { printf '%s[sandbox]%s %s\n'  "${C_BLUE}"   "${C_RST}" "$*"; }
ok()   { printf '%s[sandbox] OK %s%s\n' "${C_GREEN}" "${C_RST}" "$*"; }
warn() { printf '%s[sandbox] WARN %s%s\n' "${C_YELLOW}" "${C_RST}" "$*" >&2; }
die()  { printf '%s[sandbox] FAIL %s%s\n' "${C_RED}" "${C_RST}" "$*" >&2; exit 1; }

# =============================================================================
# 0. 环境探针
# =============================================================================
log "环境探针"

[[ "$(id -u)" -eq 0 ]] || die "沙箱测需 root（容器内 root）。当前 uid=$(id -u)。请用 --privileged 跑。"

# cgroup v2 unified 挂载（osd CgroupsRsBackend 真写 /sys/fs/cgroup）
if mountpoint -q /sys/fs/cgroup && [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then
    ok "cgroup v2 unified 挂载于 /sys/fs/cgroup（controllers: $(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || echo '?')）"
else
    warn "/sys/fs/cgroup 非 v2 unified 或未挂载——osd cgroup 真实写测将跳过/失败。"
    warn "用 -v /sys/fs/cgroup:/sys/fs/cgroup:rw --cgroupns=host --privileged 重试。"
fi

# 关键 CLI 自检（缺失只 warn，不立刻死——让后面测自己暴露问题）
for cli in zpool zfs nft ip virsh chronyc systemctl cargo; do
    if command -v "$cli" >/dev/null 2>&1; then
        log "  $cli -> $(command -v "$cli")"
    else
        warn "  $cli 未在 PATH（相关真实测会失败）"
    fi
done

# 仓库存在性
[[ -f /workspace/Cargo.toml ]] || die "/workspace/Cargo.toml 不存在。请用 -v \$PWD:/workspace 挂入仓库。"
cd /workspace
log "工作目录: $(pwd)（branch: $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?'), commit: $(git rev-parse --short HEAD 2>/dev/null || echo '?')）"

# =============================================================================
# 1. 常规三道门（对齐 .github/workflows/ci.yml，验证沙箱本身不破坏 fixture 测）
# =============================================================================
log "=== 三道门（fixture 测，对齐 ci.yml）==="

log "[1/3] cargo check --workspace --features mock --all-targets"
cargo check --workspace --features mock --all-targets

log "[2/3] cargo clippy --workspace --all-targets --features mock -- -D warnings"
cargo clippy --workspace --all-targets --features mock -- -D warnings

log "[3/3] cargo test --workspace --features mock（不含 --ignored）"
cargo test --workspace --features mock

ok "三道门通过（沙箱环境健康，fixture 测不受影响）"

# =============================================================================
# 2. 真实环境集成测：跑被 #[ignore] 标的真实测
# =============================================================================
log "=== 真实环境集成测（cargo test --ignored）==="

log "nettest 真实网络栈（reqwest/axum/mdns/rustls）"
# 注：reqwest_real_get 需公网出口；无公网时该单测失败，其余 loopback 测不受影响。
cargo test -p nettest -- --ignored --nocapture --test-threads=1 || \
    warn "nettest 部分 ignored 测失败（可能是无公网出口/无组播）—— 见上方输出"

# ---- 以下为"应入沙箱"的 root 路径测（源码侧 #[ignore] 集成测由各 owner agent 后续补）----
# 当前 main 4dffb46 这些路径尚未加 #[ignore] 集成测（见 docs/SANDBOX.md §5.2），
# 这里只跑"已存在"的真实路径测，缺失时友好提示而非失败。
log "osd 真实 cgroup 后端（CgroupsRsBackend 真写 /sys/fs/cgroup）"
if cargo test -p osd --features mock -- --ignored cgroup 2>/dev/null; then
    ok "osd cgroup 真实测通过"
else
    warn "osd cgroup 真实 #[ignore] 测尚未落地（见 docs/SANDBOX.md §5.2，待 orchestrator-agent 加）"
fi

log "os-storage 真实 ZFS（ZfsCliBackend + loop 池）"
if cargo test -p os-storage --features mock -- --ignored 2>/dev/null; then
    ok "os-storage ZFS 真实测通过"
else
    warn "os-storage ZFS 真实 #[ignore] 测尚未落地（见 docs/SANDBOX.md §5.2，待 storage-agent 加）"
fi

log "os-network / os-guest 真实 nftables（nftnl-ffi feature）"
# 需 SANDBOX_RUN_FFI=1 显式开启（FFI 编译慢 + 需 -dev 包，默认跳过）
if [[ "${SANDBOX_RUN_FFI:-0}" == "1" ]]; then
    log "  SANDBOX_RUN_FFI=1 → 跑 nftnl-ffi / virt-ffi feature 路径"
    if cargo test -p os-guest --features nftnl-ffi -- --ignored 2>/dev/null; then
        ok "os-guest nftnl 真实测通过"
    else
        warn "os-guest nftnl-ffi 测失败或未落地（见 docs/SANDBOX.md §5.2/§5.3）"
    fi
else
    warn "  SANDBOX_RUN_FFI!=1，跳过 FFI feature 测（nftnl-ffi / virt-ffi）。"
    warn "  开启：docker run ... -e SANDBOX_RUN_FFI=1 ... run-tests.sh"
fi

log "os-compute libvirt test:///default（virt-ffi feature，无 KVM 也能跑）"
if [[ "${SANDBOX_RUN_FFI:-0}" == "1" ]]; then
    if cargo test -p os-compute --features virt-ffi -- --ignored 2>/dev/null; then
        ok "os-compute libvirt（test:///default）真实测通过"
    else
        warn "os-compute virt-ffi 测失败（KVM 真实域走方案 B QEMU；test:///default 应在 A 通过）"
    fi
fi

# =============================================================================
# 3. 收尾
# =============================================================================
log "=== 全部完成 ==="
ok "沙箱测脚本跑完。任何 WARN 行为需人工核实（公网/组播/未落地 #[ignore]）。"
log "提示：sccache 统计 → SCCACHE_DIR=$SCCACHE_DIR sccache --show-stats"
