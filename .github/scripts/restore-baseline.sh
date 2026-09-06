#!/usr/bin/env bash
# ============================================================================
# restore-baseline.sh — 从历史 CI run 拉取 criterion baseline artifact
# ----------------------------------------------------------------------------
# 作用：在 GitHub Actions 的 bench job 里，跨 run 恢复"上一次保存的 baseline
# 快照"（target/criterion 目录），供本次 cargo bench -- --baseline 比对用。
#
# 流程：
#   1. 用 gh api 列出本 workflow（CI）最近 N 次成功的 run（conclusion=success）。
#   2. 跳过当前 run（github.run_id）。
#   3. 对每个候选 run，查其 artifacts，找名为 BASELINE_ARTIFACT 的。
#   4. 下载并解压到 DEST 目录（通常 target/criterion）。
#   5. 成功 → exit 0；找不到 → exit 1（首次跑场景，调用方据此走 save-baseline）。
#
# 用法（在 CI step 里）：
#   ./.github/scripts/restore-baseline.sh <artifact-name> <dest-dir>
#   echo $?   # 0=恢复成功 / 1=无可用 baseline / 2=参数或工具错误
#
# 依赖：gh CLI（已认证，GH_TOKEN 来自 secrets.GITHUB_TOKEN）。
# 本脚本**只在 CI runner 内运行**，本地不跑（本地直接用 target/criterion）。
# ============================================================================
set -u

if [ "$#" -ne 2 ]; then
  echo "用法: $0 <artifact-name> <dest-dir>" >&2
  echo "  例: $0 criterion-baseline target/criterion" >&2
  exit 2
fi

ARTIFACT_NAME="$1"
DEST_DIR="$2"
CURRENT_RUN_ID="${GITHUB_RUN_ID:-}"

if ! command -v gh >/dev/null 2>&1; then
  echo "✗ 未找到 gh CLI（本脚本仅在 GitHub Actions runner 内运行）。" >&2
  exit 2
fi

REPO="${GITHUB_REPOSITORY:-}"
if [ -z "$REPO" ]; then
  echo "✗ GITHUB_REPOSITORY 未设置（非 CI 环境？）。" >&2
  exit 2
fi

# 当前 workflow 的 .yml 文件名（ci.yml）——用于过滤只看本 workflow 的历史 run。
WORKFLOW_FILE="${GITHUB_WORKFLOW_REF#*@}"
WORKFLOW_FILE="${WORKFLOW_FILE%%:*}"

echo "→ 在 $REPO 查找 workflow=$WORKFLOW_FILE 的最近成功 run（排除当前 run=$CURRENT_RUN_ID）..."

# 列最近 20 个成功的 run（本 workflow）。gh api 返回 JSON，用 jq 取 run_id 列表。
if ! command -v jq >/dev/null 2>&1; then
  echo "✗ 未找到 jq（runner 镜像默认带；若缺失请改用 gh api --jq）。" >&2
  exit 2
fi

# 逐页扫最近 20 个 successful run（status=completed, conclusion=success），
# 在每个 run 的 artifacts 里找 BASELINE_ARTIFACT。
RUN_IDS=$(gh api --paginate \
  "repos/$REPO/actions/workflows/$WORKFLOW_FILE/runs?status=success&per_page=20" \
  --jq '.workflow_runs[].id' 2>/dev/null || true)

if [ -z "$RUN_IDS" ]; then
  echo "ℹ 未找到本 workflow 的历史成功 run —— 视为首次跑。" >&2
  exit 1
fi

for run_id in $RUN_IDS; do
  # 跳过当前 run（其 artifact 尚未上传）。
  [ "$run_id" = "$CURRENT_RUN_ID" ] && continue

  # 查该 run 的 artifacts，找目标名。
  art_url=$(gh api "repos/$REPO/actions/runs/$run_id/artifacts?per_page=100" \
    --jq ".artifacts[] | select(.name == \"$ARTIFACT_NAME\") | .archive_download_url" \
    2>/dev/null | head -n 1)
  if [ -z "$art_url" ]; then
    echo "  run $run_id：无 $ARTIFACT_NAME artifact，跳过。"
    continue
  fi

  echo "  run $run_id：命中 $ARTIFACT_NAME，下载中..."
  mkdir -p "$DEST_DIR"
  tmp_zip=$(mktemp --suffix=.zip)
  if ! gh api "$art_url" > "$tmp_zip" 2>/dev/null; then
    echo "  ⚠ 下载失败（artifact 可能已过期），继续找下一个 run。"
    rm -f "$tmp_zip"
    continue
  fi

  # 解压到 DEST_DIR（artifact 打包的是 target/criterion/... 与 criterion-bench.log）。
  # artifact 内路径顶层即 criterion/（与上传 path 一致），直接解压进 DEST_DIR 父目录。
  dest_parent=$(dirname "$DEST_DIR")
  mkdir -p "$dest_parent"
  if ! unzip -o -q "$tmp_zip" -d "$dest_parent"; then
    echo "  ⚠ 解压失败。"
    rm -f "$tmp_zip"
    continue
  fi
  rm -f "$tmp_zip"

  # 校验：DEST_DIR 应存在且非空（含 criterion/<group>/<id>/base 等子目录）。
  if [ -d "$DEST_DIR" ] && [ -n "$(ls -A "$DEST_DIR" 2>/dev/null)" ]; then
    echo "✓ baseline 已恢复到 $DEST_DIR（来源 run $run_id）。"
    exit 0
  else
    echo "  ⚠ 解压后 $DEST_DIR 为空，继续找下一个 run。"
  fi
done

echo "ℹ 所有候选 run 均无可用 $ARTIFACT_NAME —— 视为首次跑（或 baseline 已过期）。"
exit 1
