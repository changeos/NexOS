#!/usr/bin/env bash
# OS System - git pre-commit hook 骨架（devops-agent）
# ============================================================
# 在 commit 推送前本地复现 CI 的三道快门：fmt + check + clippy。
# 目的：把 CI 红拦截在本地，避免 push 后才发现 -D warnings / fmt 漂移失败。
#
# 安装：
#   make install-hooks         # 软链到 .git/hooks/pre-commit
#   或手动：
#     ln -sf ../../scripts/pre-commit.sh .git/hooks/pre-commit
#     chmod +x scripts/pre-commit.sh
#
# 临时跳过（紧急情况）：git commit --no-verify
#   （注意：跳过本地 hook 不等于跳过 CI；CI 仍会跑这三道门）
# ============================================================

set -euo pipefail

# 颜色输出
RED=$'\033[31m'
GREEN=$'\033[32m'
YELLOW=$'\033[33m'
NC=$'\033[0m'

log()  { echo "${GREEN}[pre-commit]${NC} $*"; }
warn() { echo "${YELLOW}[pre-commit]${NC} $*" >&2; }
err()  { echo "${RED}[pre-commit]${NC} $*" >&2; }

# 仅在变更触及 .rs / Cargo.toml / Cargo.lock 时才跑（文档/纯 markdown 变更跳过）。
# 通过 git diff --cached 取暂存区文件名。
mapfile -t staged < <(git diff --cached --name-only --diff-filter=ACMR | grep -E '\.(rs|toml)$|^Cargo\.lock$' || true)

if [ "${#staged[@]}" -eq 0 ]; then
    log "无 Rust/Cargo 变更，跳过 check/clippy。"
    exit 0
fi

# 命令存在性检查。
if ! command -v cargo >/dev/null 2>&1; then
    err "未找到 cargo，跳过 pre-commit 检查（请安装 Rust 工具链）。"
    warn "若已安装，确认 ~/.cargo/bin 在 PATH 中。"
    exit 0  # 不阻塞：避免环境不全时锁死 commit（CI 仍会兜底）。
fi

FEATURES="${FEATURES:-mock}"

# fmt 快门最先跑（最便宜：只解析 + 比对，不编译），对应 CI 第一步。
if ! cargo fmt --all -- --check >/dev/null 2>&1; then
    err "cargo fmt 漂移。修复：cargo fmt --all（重跑校验：cargo fmt --all -- --check）"
    err "提交中止。临时跳过用：git commit --no-verify（CI 仍会校验）。"
    exit 1
fi

log "running cargo check --workspace --features ${FEATURES} ..."
if ! cargo check --workspace --features "${FEATURES}" --all-targets >/dev/null 2>&1; then
    err "cargo check 失败。重新跑查看详情：cargo check --workspace --features ${FEATURES} --all-targets"
    err "提交中止。临时跳过用：git commit --no-verify（CI 仍会校验）。"
    exit 1
fi

log "running cargo clippy --workspace --features ${FEATURES} (-D warnings) ..."
if ! cargo clippy --workspace --all-targets --features "${FEATURES}" -- -D warnings >/dev/null 2>&1; then
    err "cargo clippy 失败（-D warnings）。重新跑查看详情："
    err "  cargo clippy --workspace --all-targets --features ${FEATURES} -- -D warnings"
    err "提交中止。临时跳过用：git commit --no-verify（CI 仍会校验）。"
    exit 1
fi

log "fmt + check + clippy 全绿，允许提交。"
exit 0
