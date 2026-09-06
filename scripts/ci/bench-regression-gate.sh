#!/usr/bin/env bash
# ============================================================================
# bench-regression-gate.sh — criterion bench 性能回归门控
# ----------------------------------------------------------------------------
# 作用：criterion 0.5 在检测到回归时**不返回非零退出码**（恒 exit 0），CI 无法
# 靠 cargo bench 退出码判断性能退化。本脚本解析 criterion 文本输出，按"分桶阈值"
# 判定回归：
#   - 严格阈值（STRICT_THRESHOLD，默认 15%）：routing / meta / storage / topo 等
#     低方差纯算法 bench（见 docs/PERFORMANCE_BASELINE.md §3.1/3.2/3.3/3.5）。
#   - 宽松阈值（LOOSE_THRESHOLD，默认 30%）：tantivy_search 的 search_index_build
#     等高方差 bench（见 §3.4，相对方差 17–35%）。
#
# 判定规则（对每个 bench 的 change 段）：
#   - 取 criterion 报告的 mean 变化点估计（change: time: [low mid high] 的 mid 值）。
#   - 正值（变慢）= 回归；负值（变快）= 改进（不计回归）。
#   - 仅当 mid > 阈值 且 统计显著（p < 0.05，避免噪声误报）才判为回归。
#   - 任一 bench 触发阈值 → 脚本 exit 1（CI 标红）。
#   - 首次跑（无 baseline，无任何 change 行）→ exit 0（save-baseline 建基线场景）。
#
# 用法：
#   cargo bench ... -- --baseline <name> 2>&1 | tee bench.log
#   scripts/ci/bench-regression-gate.sh bench.log
#   echo $?   # 0=通过 / 1=有回归 / 2=配置错误（无输入文件）
#
# 环境变量覆盖阈值：
#   STRICT_THRESHOLD=15 LOOSE_THRESHOLD=30 scripts/ci/bench-regression-gate.sh bench.log
#
# 配套：scripts/ci/parse-criterion-changes.awk（criterion 输出 → TSV 解析器）。
# 参考文档：docs/PERFORMANCE_BASELINE.md §CI 回归检测。
# ============================================================================
set -u

STRICT_THRESHOLD="${STRICT_THRESHOLD:-15}"   # 纯算法 bench：>15% mean 回归判失败
LOOSE_THRESHOLD="${LOOSE_THRESHOLD:-30}"     # 高方差 bench（tantivy 建索引）：>30%

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AWK_PARSER="$SCRIPT_DIR/parse-criterion-changes.awk"

if [ "$#" -lt 1 ]; then
  echo "用法: $0 <criterion-output.log>" >&2
  echo "  例: $0 criterion-bench.log" >&2
  exit 2
fi

LOG="$1"
if [ ! -f "$LOG" ]; then
  echo "✗ 找不到输入文件: $LOG" >&2
  exit 2
fi
if [ ! -f "$AWK_PARSER" ]; then
  echo "✗ 找不到 awk 解析器: $AWK_PARSER" >&2
  exit 2
fi

# 解析 criterion 输出为 TSV（bench\tmid\tsig\tis_loose）。
TSV=$(awk -f "$AWK_PARSER" "$LOG")

# 无任何 change 行 → 首次跑（save-baseline 建基线场景），直接通过。
if [ -z "$TSV" ]; then
  echo "ℹ 未发现 criterion change 行（首次跑 / save-baseline 建基线）—— 不做回归判定，通过。"
  exit 0
fi

echo "── criterion 回归门控 ────────────────────────────────────────────"
echo "  严格阈值 (routing/meta/storage/topo): mean 回归 > ${STRICT_THRESHOLD}%"
echo "  宽松阈值 (tantivy search_index_build): mean 回归 > ${LOOSE_THRESHOLD}%"
echo "  判定：mid 值 > 阈值 且 统计显著 (p < 0.05) → 回归"
echo "──────────────────────────────────────────────────────────────────"
printf "  %-48s %10s %8s %10s\n" "bench" "meanΔ%" "p<0.05" "判定"

regressions=0
checked=0
while IFS=$'\t' read -r bench mid sig is_loose; do
  [ -z "$bench" ] && continue
  checked=$((checked + 1))
  if [ "$is_loose" = "1" ]; then
    thresh="$LOOSE_THRESHOLD"
  else
    thresh="$STRICT_THRESHOLD"
  fi
  # 仅当变慢（mid>0）且超阈值且显著 → 回归（awk 做浮点比较，bash 无浮点运算）。
  verdict="OK"
  if [ "$sig" = "1" ] && awk "BEGIN{exit !($mid > 0 && $mid > $thresh)}"; then
    verdict="REGRESSION"
    regressions=$((regressions + 1))
  fi
  printf "  %-48s %9s%% %8s %10s\n" "$bench" "$mid" "$sig" "$verdict"
done <<< "$TSV"
echo "──────────────────────────────────────────────────────────────────"

if [ "$regressions" -gt 0 ]; then
  echo "✗ 检测到 $regressions / $checked 个 bench 超阈值回归 —— 性能退化，CI 失败。"
  echo "  若为预期变更（算法/硬件/环境更新），请重跑 save-baseline 更新基线："
  echo "    make bench-baseline TAG=os-baseline   # 或 ci.yml 中 BASELINE_NAME"
  exit 1
fi

echo "✓ $checked 个 bench 全部通过回归门控（无超阈值回归）。"
exit 0
