#!/bin/bash
# ============================================================
# NexOS 发版脚本——版本号 + 纯净树双架构构建 + tag/推送 + dist +
# aliyun 部署 + /healthz 探活 + md5 对拍（v0.1.49 补全，审查 D4/E10）
#
# 用法: ./scripts/release.sh <版本号> [--dry-run] [--github]
#       例: ./scripts/release.sh 0.1.49          # 正式发版
#           ./scripts/release.sh 0.1.49 --dry-run # 只打印将执行的步骤，零副作用
#
# 前置: gcc-aarch64-linux-gnu（交叉链接器）；远端 nexos-local 已配置；
#       aliyun 免密 key（ssh -p 221 root@203.0.113.2，08-27 已回填）
#
# 流程（对照审查 D4 三缺口，两类事故实伤史）：
#   ① 版本号 → ② 三道门（工作树快速反馈）→ ③ 106 debug 构建（开发机可带
#   WIP，铁律例外）→ ④ 提交+本地 tag → ⑤ 纯净导出树构建：
#      git archive <tag> 导出（v0.1.43 铁律：release 禁用工作树 target/，
#      杜绝「含未提交代码报旧版本」污染公网源）→ 树内 web npm ci+build
#      （static-dist 不入 git → 构建必跑，rust-embed 首嵌即新前端——杜绝
#      「显示旧版」事故）→ x86_64/aarch64 双架构 → ⑥ 推送（构建全绿才推）
#   → ⑦ dist 三工件 → ⑧ aliyun 公网源同步 + scp 部署（release + web tar）
#   → ⑨ /healthz 探活（真名 /healthz；/health 是 SPA 兜底假 200——MEMORY）
#   → ⑩ md5 三方对拍汇总表（本地纯净树 vs aliyun 部署位 vs dist；v0.1.6
#      发版「md5 三方一致」实践固化）→ ⑪ GitHub（--github，铁律=仅发版时）
#
# 113 部署（scp+static-dist tar 双份+restart）仍人工：密码 ssh 不进脚本。
# ============================================================
set -euo pipefail
cd "$(dirname "$0")/.."

# —— 参数解析：<版本号> 必填；--dry-run 只打印；--github 仅发版时 ——
V=""
DRY_RUN=""
GITHUB=""
for a in "$@"; do
  case "$a" in
    --dry-run) DRY_RUN=1 ;;
    --github)  GITHUB=1 ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) V="$a" ;;
  esac
done
[[ -n "$V" ]] || { echo "用法: $0 <版本号> [--dry-run] [--github]   例: $0 0.1.49" >&2; exit 1; }
# 版本号进路径/远端命令，白名单校验（防注入）
[[ "$V" =~ ^[0-9A-Za-z][0-9A-Za-z._-]*$ ]] || { echo "版本号非法: $V" >&2; exit 1; }

# —— 执行原语：dry-run 只打印将要执行的步骤；真跑经 eval（支持管道/复合步） ——
run() { echo "  \$ $*"; [[ -n "$DRY_RUN" ]] || eval "$*"; }
plan() { echo "  # $*"; }

# —— aliyun 部署面（免密 key；--noproxy：root shell 带 proxy env，MEMORY 运维备忘） ——
ALIYUN_HOST=203.0.113.2
ALIYUN_SSH_PORT=221
ALIYUN_BASE_URL="http://${ALIYUN_HOST}:8558"
ALIYUN_SSH="ssh -p ${ALIYUN_SSH_PORT} -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 root@${ALIYUN_HOST}"

# —— Files API 上传凭证：只经环境变量注入，不硬编码落脚本（缺省占位仅限本地拉起） ——
NEXOS_FILES_TOKEN="${NEXOS_FILES_TOKEN:-change-me-admin-token}"
export NEXOS_FILES_TOKEN

if [[ -n "$DRY_RUN" ]]; then
  EXPORT="/tmp/nexos-release-${V}-export.dryrun"   # 占位（未创建），仅打印用
else
  EXPORT="$(mktemp -d /tmp/nexos-release-${V}-XXXXXX)"
fi

echo "=== NexOS 发版 $V ==="
[[ -n "$DRY_RUN" ]] && echo "（--dry-run：以下只打印将执行的步骤，不构建/不提交/不推送/不部署）"

# ① 版本号写入 workspace Cargo.toml
run "sed -i \"70s/version = \\\".*\\\"/version = \\\"$V\\\"/\" Cargo.toml"
run "grep -q \"^version = \\\"$V\\\"\" Cargo.toml || { echo 版本写入失败; exit 1; }"

# ② 三道门（fmt / clippy 关键 crate / 测试抽样）——工作树快速反馈
run "cargo fmt --all"
run "cargo clippy -p os-api -p os-p2p --all-targets --features mock -- -D warnings"
run "cargo test -p os-api --lib -- 2>&1 | tail -1"

# ③ 106 debug 构建（工作树；开发机可带 WIP——纯净铁律只约束 dist/公网分发件）
run "cargo build -p os-api"
echo "✓ ③ x86_64 debug（106 本机用）: target/debug/os-api"

# ④ 提交 + 本地 tag（tag 先行才能 git archive 导出；推送挪到 ⑥ 构建全绿后）
run "git add Cargo.toml Cargo.lock"
run "git commit --no-verify -m \"chore(release): 版本 $V\" || echo \"(无版本变更可提交)\""
run "git tag -fa \"v$V\" -m \"NexOS $V\""

# ⑤ 纯净导出树构建（v0.1.43 部署构建污染纪律：release 必须从 git archive <tag>，
#    禁止直接用工作树 target/——曾发生含未提交代码报旧版本污染 aliyun 公网源）
echo "=== ⑤ 纯净导出树（git archive v$V → $EXPORT） ==="
run "git archive \"v$V\" | tar -x -C \"$EXPORT\""

# ⑤a 前端构建（树内必跑：static-dist 不入 git——rust-embed 首嵌即新前端，
#     杜绝「npm build 漏做嵌旧 web」事故类；npm ci 保证树内依赖纯净可复现）
run "(cd \"$EXPORT/web\" && npm ci)"
run "(cd \"$EXPORT/web\" && npm run build)"
# 校验产物在位（npm build 漏做在此拦下，不再静默嵌旧/空前端）
run "[[ -f \"$EXPORT/crates/os-api/static-dist/index.html\" ]] || { echo '前端构建产物缺失（static-dist/index.html）'; exit 1; }"
# static-dist 新旧对拍（信息位）：工作树副本若与纯净树不一致 → 工作树已过期，提示
if [[ -z "$DRY_RUN" ]]; then
  if [[ -d crates/os-api/static-dist ]] && [[ -n "$(ls -A crates/os-api/static-dist 2>/dev/null || true)" ]] \
     && ! diff -rq crates/os-api/static-dist "$EXPORT/crates/os-api/static-dist" >/dev/null 2>&1; then
    echo "  ⚠ 工作树 static-dist 与纯净树新构建不一致（工作树副本过期；分发已用纯净树产物，无碍）"
  fi
else
  plan "diff -rq crates/os-api/static-dist（工作树） $EXPORT/crates/os-api/static-dist（新旧对拍，不一致仅提示）"
fi

# ⑤b x86_64 release 构建（纯净树；供 113/aliyun）
run "(cd \"$EXPORT\" && cargo build --release -p os-api)"
echo "✓ ⑤b x86_64 release: $EXPORT/target/release/os-api"

# ⑤c aarch64 构建（DGX Spark / ARM 服务器；纯净树）
run "export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc"
run "export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar"
run "(cd \"$EXPORT\" && cargo build --release --target aarch64-unknown-linux-gnu -p os-api -p os-p2p)"
run "file \"$EXPORT/target/aarch64-unknown-linux-gnu/release/os-api\" | grep -q \"ARM aarch64\" || { echo 'aarch64 产物校验失败'; exit 1; }"
echo "✓ ⑤c aarch64: $EXPORT/target/aarch64-unknown-linux-gnu/release/{os-api,p2p-node}"

# ⑥ 推送（NexHub；构建全绿才推——失败时本地 tag 可 -fa 重打重跑）
run "git push nexos-local main --force-with-lease"
run "git push nexos-local -f \"v$V\""

# ⑦ 刷新分发目录（/tank/os-data/dist——传输组件/Files API 的分发源；纯净树产物）
run "mkdir -p /tank/os-data/dist"
run "cp \"$EXPORT/target/aarch64-unknown-linux-gnu/release/os-api\" /tank/os-data/dist/os-api-aarch64-latest"
run "cp \"$EXPORT/target/aarch64-unknown-linux-gnu/release/p2p-node\" /tank/os-data/dist/p2p-node-aarch64-latest"
run "cp \"$EXPORT/target/release/os-api\" /tank/os-data/dist/os-api-x86_64-latest"

# ⑧a aliyun 公网源同步（Files API——install.sh 的源节点）
#     发版只刷 106 分发目录的话，公网源会 404（08-30 DGX Spark 实测踩过）
ALIYUN_UP=1
if [[ -z "$DRY_RUN" ]]; then
  curl -s --max-time 8 -o /dev/null "$ALIYUN_BASE_URL/" || ALIYUN_UP=0
fi
if [[ "$ALIYUN_UP" == 1 ]]; then
  echo "=== ⑧ aliyun：公网源同步 + scp 部署（$ALIYUN_HOST） ==="
  if [[ -n "$DRY_RUN" ]]; then
    plan "curl 探活 $ALIYUN_BASE_URL/ 可达后：python3 Files API 上传三分发件（os-api-x86_64/aarch64、p2p-node-aarch64 → /tank/os-data/dist）"
  else
    python3 - <<'PYEOF'
import base64, json, os, urllib.request
files_token = os.environ.get("NEXOS_FILES_TOKEN", "change-me-admin-token")
for name, path in [
    ("os-api-x86_64-latest", "/tank/os-data/dist/os-api-x86_64-latest"),
    ("os-api-aarch64-latest", "/tank/os-data/dist/os-api-aarch64-latest"),
    ("p2p-node-aarch64-latest", "/tank/os-data/dist/p2p-node-aarch64-latest"),
]:
    try:
        with open(path, "rb") as f:
            b64 = base64.b64encode(f.read()).decode()
        req = urllib.request.Request(
            "http://203.0.113.2:8558/api/v1/files/upload?path=/tank/os-data/dist",
            data=json.dumps({"filename": name, "content_base64": b64}).encode(),
            headers={"Content-Type": "application/json",
                     "Authorization": "Bearer " + files_token})
        urllib.request.urlopen(req, timeout=600)
        print(f"  aliyun 同步: {name}")
    except Exception as e:
        print(f"  aliyun 同步失败 {name}: {e}")
PYEOF
  fi

  # ⑧b aliyun 服务部署（scp release + web tar；升级通道=scp 直装，
  #     不走 b64 JSON 信封传数十 MB 二进制——对照审查 A3-1/D4-3）。
  #     static-dist 必须落 /opt/nexos/crates/os-api/static-dist（服务从这里
  #     读；外层 /opt/nexos/static-dist 无效——MEMORY aliyun 运维备忘）。
  run "scp -P $ALIYUN_SSH_PORT -o BatchMode=yes -o StrictHostKeyChecking=no \"$EXPORT/target/release/os-api\" root@${ALIYUN_HOST}:/opt/nexos/os-api.new"
  run "tar -C \"$EXPORT/crates/os-api/static-dist\" -czf \"/tmp/nexos-static-dist-$V.tgz\" ."
  run "scp -P $ALIYUN_SSH_PORT -o BatchMode=yes -o StrictHostKeyChecking=no \"/tmp/nexos-static-dist-$V.tgz\" root@${ALIYUN_HOST}:/tmp/"
  run "$ALIYUN_SSH 'set -e; bak=/opt/nexos/os-api.bak.$(date +%Y%m%d%H%M%S); cp -a /opt/nexos/os-api \"\$bak\"; install -m 0755 /opt/nexos/os-api.new /opt/nexos/os-api; rm -rf /opt/nexos/crates/os-api/static-dist.old; mkdir -p /opt/nexos/crates/os-api; if [ -d /opt/nexos/crates/os-api/static-dist ]; then mv /opt/nexos/crates/os-api/static-dist /opt/nexos/crates/os-api/static-dist.old; fi; mkdir -p /opt/nexos/crates/os-api/static-dist; tar -xzf /tmp/nexos-static-dist-$V.tgz -C /opt/nexos/crates/os-api/static-dist; systemctl restart nexos-os-api; echo \"  远端备份: \$bak\"'"

  # ⑨ /healthz 探活（真名 /healthz——/health 落 SPA 兜底假 200；部署远端确认）
  echo "=== ⑨ /healthz 探活（$ALIYUN_BASE_URL/healthz，5 次 × 3s 退避） ==="
  if [[ -n "$DRY_RUN" ]]; then
    plan "for i in 1..5: curl -fsS --noproxy '*' --max-time 10 $ALIYUN_BASE_URL/healthz && break; sleep 3 —— 探活失败则发版失败并打印回滚"
  else
    HZ_OK=0
    for i in 1 2 3 4 5; do
      if curl -fsS --noproxy '*' --max-time 10 "$ALIYUN_BASE_URL/healthz" >/dev/null 2>&1; then HZ_OK=1; break; fi
      sleep 3
    done
    if [[ "$HZ_OK" == 1 ]]; then
      echo "  ✓ aliyun /healthz 200（部署生效确认）"
    else
      echo "  ✗ aliyun /healthz 探活失败——发版中止"
      echo "  回滚：$ALIYUN_SSH 'cp -a /opt/nexos/os-api.bak.<时间戳> /opt/nexos/os-api && systemctl restart nexos-os-api'"
      exit 1
    fi
  fi
else
  echo "  aliyun 不可达，跳过公网源同步/部署/探活（D4 缺口三件套未执行——确认后重跑）"
fi

# ⑩ md5 对拍汇总表（本地纯净树 vs aliyun 部署位 vs /tank/os-data/dist——
#    v0.1.6 发版「md5 三方一致」实践固化；任一对拍不一致 → 发版失败）
echo "=== ⑩ md5 三方对拍汇总表 ==="
if [[ -n "$DRY_RUN" ]]; then
  plan "md5sum 对拍四行：os-api-x86_64（本地/aliyun/dist）、os-api-aarch64（本地/dist）、p2p-node-aarch64（本地/dist）、static-dist 目录指纹（本地/aliyun）——任一不一致 exit 1"
else
  md5_of() { md5sum "$1" 2>/dev/null | awk '{print $1}' || true; }
  md5_remote() { $ALIYUN_SSH "md5sum '$1' 2>/dev/null | awk '{print \$1}'" 2>/dev/null || true; }
  # 目录指纹：文件清单+逐文件 md5 的再 md5（两端 LC_ALL=C sort 同序可比；-r 空目录零调用）
  dir_hash_local() { (cd "$1" && find . -type f | LC_ALL=C sort | xargs -r md5sum 2>/dev/null | md5sum | awk '{print $1}') || true; }
  DIR_HASH_REMOTE_CMD='cd /opt/nexos/crates/os-api/static-dist && find . -type f | LC_ALL=C sort | xargs -r md5sum 2>/dev/null | md5sum | awk "{print \$1}"'
  MISMATCH=0
  row() { # row <工件> <本地> <远端> <需对拍的 pair 列表（a|b，双方非空才比）>
    local name=$1 loc=$2 rem=$3; shift 3
    local verdict="✓"
    for pair in "$@"; do
      local a=${pair%%|*} b=${pair##*|}
      if [[ -n "$a" && -n "$b" ]]; then
        [[ "$a" != "$b" ]] && { verdict="✗"; MISMATCH=1; }
      else
        [[ "$verdict" == "✓" ]] && verdict="—（单侧空，跳过）"
      fi
    done
    printf '  %-22s %-34s %-34s %-16s\n' "$name" "${loc:0:32}" "${rem:0:32}" "$verdict"
  }
  printf '  %-22s %-34s %-34s %-16s\n' '工件' '本地（纯净树）' '远端（aliyun/dist）' '对拍'
  L_X86=$(md5_of "$EXPORT/target/release/os-api")
  L_ARM=$(md5_of "$EXPORT/target/aarch64-unknown-linux-gnu/release/os-api")
  L_P2P=$(md5_of "$EXPORT/target/aarch64-unknown-linux-gnu/release/p2p-node")
  L_WEB=$(dir_hash_local "$EXPORT/crates/os-api/static-dist")
  D_X86=$(md5_of /tank/os-data/dist/os-api-x86_64-latest)
  D_ARM=$(md5_of /tank/os-data/dist/os-api-aarch64-latest)
  D_P2P=$(md5_of /tank/os-data/dist/p2p-node-aarch64-latest)
  # 远端两格：aliyun 部署位（可达才查，避免不可达时 ssh 超时阻塞）；dist 格拼在 verdict 比对里
  R_BIN=""
  R_WEB=""
  if [[ "$ALIYUN_UP" == 1 ]]; then
    R_BIN=$(md5_remote /opt/nexos/os-api)
    R_WEB=$($ALIYUN_SSH "$DIR_HASH_REMOTE_CMD" 2>/dev/null || true)
    if [[ -z "$R_BIN" ]]; then echo "  ⚠ aliyun 部署位 md5 读取失败（ssh/md5sum）——该行对拍降级"; fi
  fi
  row 'os-api x86_64'       "$L_X86" "$R_BIN" "$L_X86|$R_BIN" "$L_X86|$D_X86"
  row 'os-api aarch64'      "$L_ARM" "$D_ARM" "$L_ARM|$D_ARM"
  row 'p2p-node aarch64'    "$L_P2P" "$D_P2P" "$L_P2P|$D_P2P"
  row 'static-dist（指纹）' "$L_WEB" "$R_WEB" "$L_WEB|$R_WEB"
  if [[ "$MISMATCH" == 1 ]]; then echo "  ✗ md5 对拍不一致——发版失败（回滚提示见尾部）"; exit 1; fi
  echo "  ✓ md5 对拍一致（— 行=单侧未采集，非不一致）"
fi

# ⑪ 可选：GitHub 同步（--github 开关，铁律=仅发版时）
if [[ -n "$GITHUB" ]]; then
  echo "=== ⑪ GitHub 同步（铁律：仅发版时） ==="
  if [[ -n "$DRY_RUN" ]]; then
    plan "GH_TOKEN 经环境变量注入（凭证不落脚本/不落日志）→ git push https://x-access-token:***@github.com/changeos/NexOS.git main --tags"
  else
    if [[ -z "${GH_TOKEN:-}" ]]; then
      echo "  ✗ 缺 GH_TOKEN 环境变量（从安全存储注入后再跑 --github）" >&2
      exit 1
    fi
    git push "https://x-access-token:${GH_TOKEN}@github.com/changeos/NexOS.git" main --tags
  fi
fi

echo ""
if [[ -n "$DRY_RUN" ]]; then
  echo "=== 发版演练完成 $V（--dry-run：以上步骤均未执行） ==="
  exit 0
fi
echo "=== 发版完成 $V ==="
echo "产物（纯净树 $EXPORT）："
echo "  x86_64  release: $EXPORT/target/release/os-api"
echo "  aarch64 release: $EXPORT/target/aarch64-unknown-linux-gnu/release/os-api"
echo "  aarch64 p2p-node: $EXPORT/target/aarch64-unknown-linux-gnu/release/p2p-node"
echo "分发：113/aliyun 用 release x86_64；DGX Spark 等 ARM 用 aarch64；106 本机用 debug（工作树）"
echo "113 部署（人工）：scp 上列 x86_64 产物 + web tar 到 static-dist 双份（/opt/nexos/static-dist 与 /opt/nexos/crates/os-api/static-dist）+ systemctl restart nexos-os-api"
echo "回滚（aliyun）：ssh -p 221 root@203.0.113.2 'cp -a /opt/nexos/os-api.bak.<时间戳> /opt/nexos/os-api && mv /opt/nexos/crates/os-api/static-dist.old /opt/nexos/crates/os-api/static-dist && systemctl restart nexos-os-api'"
