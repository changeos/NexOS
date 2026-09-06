#!/usr/bin/env awk -f
# ============================================================================
# parse-criterion-changes.awk — 解析 criterion --baseline 比对输出
# ----------------------------------------------------------------------------
# 输入：criterion 文本日志（cargo bench ... -- --baseline <name> 2>&1 的输出）。
# 输出：每行一个 bench 的回归判定字段（TSV，制表符分隔）：
#     <bench_id>\t<mean_change_pct>\t<p_significant>\t<is_loose_bucket>
#
# 字段说明：
#   bench_id            criterion 输出里的 group/bench 标识（如 routing/hit_static/5000）。
#   mean_change_pct     change: time: 行 3 个百分比的中位数（point estimate）。
#                       正值 = 变慢（回归），负值 = 变快（改进）。
#   p_significant       1 = 统计显著 (p < 0.05)；0 = 不显著 / 未报 p。
#   is_loose_bucket     1 = 该 bench 命中 LOOSE_PATTERN（走宽松阈值）；
#                       0 = 走严格阈值。
#
# 匹配规则：
#   - bench id = 紧邻 "change: time:" 行之前、列 1 起始的非空白行。
#   - change 行特征：缩进的 "time:" 后跟 "[..% ..% ..%]" 三百分比（区分于原始
#     测量行，原始行无 % 符号），可含 "(p = ..)" 尾注。
#   - thrpt: 变化行虽也带 %，但行首是 "thrpt:" 非 "time:"，被排除。
#
# 用法：
#   awk -f parse-criterion-changes.awk -v loose_pattern="search_index_build" bench.log
# ============================================================================
# 默认宽松阈值匹配关键字（可被 -v loose_pattern=... 覆盖）。
BEGIN { if (length(loose_pattern) == 0) loose_pattern = "search_index_build" }

# 抓取最近的 bench id：列 1 起始、非空白、非 "Benchmarking ..." 进度行。
# criterion 在 Analyzing 后打印 bench id（裸名，无 "Benchmarking" 前缀、无缩进）。
/^[A-Za-z0-9_]/ && $0 !~ /^Benchmarking / { bench = $0 }

# 匹配 change: 下的 time: 行（三百分比 + 可选 p 值）。
# 原始测量 time: 行无 % 符号，故不匹配。
/^[[:space:]]+time:[[:space:]]*\[[-+]?[0-9][^]]*%[[:space:]]+[-+]?[0-9][^]]*%[[:space:]]+[-+]?[0-9][^]]*%\]/ {
    line = $0
    # 用 match() 循环提取所有 "[-+]?<num>%" token，取中位数。
    cnt = 0
    s = line
    while (match(s, /[-+]?[0-9]+(\.[0-9]+)?%/)) {
        m = substr(s, RSTART, RLENGTH)
        sub(/%$/, "", m)
        cnt++
        pcts[cnt] = m + 0
        s = substr(s, RSTART + RLENGTH)
    }
    if (cnt >= 2) {
        mid = pcts[2]   # point estimate（中位数）
        # 统计显著性：criterion 写 "p = 0.xx < 0.05" 表显著。
        sig = (line ~ /p = 0(\.[0-9]+)? < 0\.05/) ? 1 : 0
        is_loose = (index(bench, loose_pattern) > 0) ? 1 : 0
        printf "%s\t%s\t%d\t%d\n", bench, mid, sig, is_loose
    }
    delete pcts
}
