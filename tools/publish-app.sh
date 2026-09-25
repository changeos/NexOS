#!/usr/bin/env bash
# =============================================================================
# tools/publish-app.sh —— NexOS 应用一键发布（v0.1.34）
#
# 用法：
#   ./tools/publish-app.sh <film|qrtransfer|streaming> [--patch] [--no-install]
#
# 流程（六步，任一步失败即停 set -euo pipefail）：
#   1. 前置检查：应用目录 / manifest.json / node+npm / 发布裸仓存在
#   2. 版本管理：--patch 则 manifest.json + package.json 版本 +0.0.1；
#      无 flag 用当前版本（重发布，tag 强制重打）
#   3. 构建：apps/<name> 下 npm install（仅 node_modules 缺时）+ npm run build
#      （package.json 有 build:standalone 则再补跑一段）
#   4. 发布仓同步：clone 裸仓到 /tmp 临时目录 → rsync 对齐——
#      仓库根 = 应用包根（布局铁律）：manifest.json/package*.json/README/
#      tsconfig/vite 配置/src/standalone/scripts 在根，dist/web/* → web/，
#      dist/standalone.html → web/standalone.html
#   5. commit（oem <oem@ub2604>）+ tag v<版本> + push（file:// 路径直推）
#   6. CI 触发（file:// 直推不经 git-http push 钩子，脚本显式调 CI 端点，
#      不等终态只打印查询命令）；除 --no-install 外再 POST
#      /api/v1/apps/install 触发安装/升级并打印 action 结果
#
# token 来源：NEXHUB_TOKEN env → ~/.config/nexhub/credentials 的 TOKEN= 行；
# 都没有则打印手动命令提示（发布本体已成功，不算失败）。
#
# env 覆盖：NEXHUB_API（缺省 http://127.0.0.1:8558）、
#           NEXOS_GIT_REPOS_DIR（缺省 /tank/git-repos，与 os-nexhub 同名同义）。
# =============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APPS_DIR="$REPO_ROOT/apps"
REPOS_DIR="${NEXOS_GIT_REPOS_DIR:-/tank/git-repos}"
API="${NEXHUB_API:-http://127.0.0.1:8558}"

APP=""
DO_PATCH=0
DO_INSTALL=1

usage() {
  # 头注释块全文（第 3 行起到收口围栏线）
  sed -n '3,/^# ===/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 1
}

log()  { printf '\033[1;36m[publish-app]\033[0m %s\n' "$*"; }
step() { log "[$1] $2"; }
die()  { printf '\033[1;31m[publish-app] 失败:\033[0m %s\n' "$*" >&2; exit 1; }

# ---- 参数解析 ----
while [ $# -gt 0 ]; do
  case "$1" in
    film|qrtransfer|streaming) [ -z "$APP" ] || die "应用名只给一次"; APP="$1" ;;
    --patch)      DO_PATCH=1 ;;
    --no-install) DO_INSTALL=0 ;;
    -h|--help)    usage ;;
    *) die "未知参数: $1（用法见 --help）" ;;
  esac
  shift
done
[ -n "$APP" ] || usage

APP_DIR="$APPS_DIR/$APP"
MANIFEST="$APP_DIR/manifest.json"
PACKAGE="$APP_DIR/package.json"
REPO="nexos-app-$APP"
BARE="$REPOS_DIR/$REPO.git"
TMP=""

cleanup() { [ -n "$TMP" ] && [ -d "$TMP" ] && rm -rf "$TMP"; }
trap cleanup EXIT

# ---- 1/6 前置检查 ----
step "1/6" "前置检查"
[ -d "$APP_DIR" ]      || die "应用目录不存在: $APP_DIR"
[ -r "$MANIFEST" ]     || die "manifest.json 不可读: $MANIFEST"
[ -r "$PACKAGE" ]      || die "package.json 不可读: $PACKAGE"
command -v node >/dev/null 2>&1 || die "node 不可用（PATH）"
command -v npm  >/dev/null 2>&1 || die "npm 不可用（PATH）"
command -v git  >/dev/null 2>&1 || die "git 不可用（PATH）"
command -v rsync >/dev/null 2>&1 || die "rsync 不可用（PATH）"
[ -d "$BARE" ]         || die "发布裸仓库不存在: $BARE"
jq -e . "$MANIFEST" >/dev/null 2>&1 || die "manifest.json 不是合法 JSON"
log "  应用=$APP 仓库=$BARE ✓"

# ---- 2/6 版本管理 ----
OLD_VER="$(jq -r '.version // empty' "$MANIFEST")"
[ -n "$OLD_VER" ] || die "manifest.json 缺 version 字段"
case "$OLD_VER" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) die "manifest version 非三段式（X.Y.Z）: $OLD_VER" ;;
esac
if [ "$DO_PATCH" -eq 1 ]; then
  IFS='.' read -r MA MI PA <<<"$OLD_VER"
  NEW_VER="$MA.$MI.$((PA + 1))"
  step "2/6" "版本 +0.0.1：$OLD_VER → $NEW_VER（manifest.json + package.json）"
  jq --arg v "$NEW_VER" '.version = $v' "$MANIFEST" > "$MANIFEST.tmp" && mv "$MANIFEST.tmp" "$MANIFEST"
  jq --arg v "$NEW_VER" '.version = $v' "$PACKAGE" > "$PACKAGE.tmp" && mv "$PACKAGE.tmp" "$PACKAGE"
  VER="$NEW_VER"
  COMMIT_SUMMARY="publish-app 一键发布（版本 $OLD_VER → $NEW_VER，源码与 web 产物同步）"
else
  VER="$OLD_VER"
  step "2/6" "沿用当前版本：$VER（重发布口径，tag 强制重打）"
  COMMIT_SUMMARY="publish-app 重发布（版本 $VER，源码与 web 产物再同步）"
fi

# ---- 3/6 构建 ----
step "3/6" "构建（$APP_DIR）"
cd "$APP_DIR"
if [ ! -d node_modules ]; then
  log "  node_modules 缺，先 npm install"
  npm install --no-audit --no-fund
fi
npm run build
# package.json 若声明 build:standalone（当前三应用并入 build，预留分支）
if [ -n "$(jq -r '.scripts["build:standalone"] // empty' "$PACKAGE")" ]; then
  npm run build:standalone
fi
[ -f dist/web/entry.js ] || die "构建产物缺 dist/web/entry.js"
log "  构建完成：$(ls dist/web | tr '\n' ' ')"

# ---- 4/6 发布仓同步（仓库根 = 应用包根）----
step "4/6" "发布仓同步（clone $BARE → /tmp 临时目录）"
TMP="$(mktemp -d "/tmp/publish-app-$APP-XXXXXX")"
PUB="$TMP/publish"
git clone -q "$BARE" "$PUB"
# dist/web/* → web/（--delete 清陈旧产物）；standalone.html 归位 web/
mkdir -p "$PUB/web"
rsync -a --delete "$APP_DIR/dist/web/" "$PUB/web/"
[ -f "$APP_DIR/dist/standalone.html" ] && rsync -a "$APP_DIR/dist/standalone.html" "$PUB/web/standalone.html"
# 根级目录/文件：src standalone scripts + manifest/README/package*/tsconfig/vite 配置
for d in src standalone scripts; do
  [ -d "$APP_DIR/$d" ] && rsync -a --delete "$APP_DIR/$d/" "$PUB/$d/"
done
for f in manifest.json README.md package.json package-lock.json tsconfig.json vite.config.ts vite.standalone.config.ts; do
  [ -f "$APP_DIR/$f" ] && rsync -a "$APP_DIR/$f" "$PUB/$f"
done
log "  同步完成：$(cd "$PUB" && ls | tr '\n' ' ')"

# ---- 5/6 commit + tag + push ----
step "5/6" "commit + tag v$VER + push"
cd "$PUB"
git add -A
if git diff --cached --quiet; then
  log "  无内容变更（同版本重发布且产物一致），复用现有提交与 tag"
else
  git -c user.name=oem -c user.email=oem@ub2604 commit -q -m "v$VER: $COMMIT_SUMMARY"
  log "  commit: $(git log -1 --format='%h %s')"
fi
git tag -f "v$VER" >/dev/null
git push -q origin HEAD:main
git push -q -f origin "refs/tags/v$VER"
log "  已推送 main + tag v$VER → $BARE"

# ---- 6/6 CI 触发 + 安装/升级 ----
step "6/6" "CI 触发与安装"

TOKEN="${NEXHUB_TOKEN:-}"
if [ -z "$TOKEN" ] && [ -f "$HOME/.config/nexhub/credentials" ]; then
  TOKEN="$(grep -E '^TOKEN=' "$HOME/.config/nexhub/credentials" | head -1 | cut -d= -f2- || true)"
fi

# file:// 直推不经 git-http push 钩子 → 显式触发 CI（手动 trigger，等效）
if [ -n "$TOKEN" ]; then
  CI_RESP="$(curl -sf -X POST "$API/api/v1/coderepo/repos/$REPO/ci" \
    -H "Authorization: Bearer $TOKEN" 2>/dev/null || true)"
  if [ -n "$CI_RESP" ]; then
    CI_RUN="$(printf '%s' "$CI_RESP" | jq -r '.run.id // empty')"
    log "  CI 已触发：run $CI_RUN（trigger=manual，file:// 直推不经 push 钩子故显式触发）"
  else
    log "  ⚠ CI 触发失败（服务未起或鉴权拒绝），手动：curl -X POST $API/api/v1/coderepo/repos/$REPO/ci -H 'Authorization: Bearer <token>'"
  fi
else
  log "  ⚠ 无 token（NEXHUB_TOKEN / ~/.config/nexhub/credentials），CI 未触发"
fi
log "  CI 进度查询：curl -s $API/api/v1/coderepo/repos/$REPO/ci | jq"

if [ "$DO_INSTALL" -eq 1 ]; then
  if [ -z "$TOKEN" ]; then
    log "  ⚠ 无 token，跳过自动安装，手动执行："
    log "    curl -X POST $API/api/v1/apps/install -H 'Authorization: Bearer <token>' \\"
    log "      -H 'Content-Type: application/json' -d '{\"repo\":\"$REPO\"}'"
  else
    log "  安装/升级：POST /api/v1/apps/install {\"repo\":\"$REPO\"}"
    INST_RESP="$(curl -s -o "$TMP/install.json" -w '%{http_code}' -X POST "$API/api/v1/apps/install" \
      -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
      -d "{\"repo\":\"$REPO\"}")" || true
    ACTION="$(jq -r '.action // .error // empty' "$TMP/install.json" 2>/dev/null || true)"
    AVER="$(jq -r '.app.version // empty' "$TMP/install.json" 2>/dev/null || true)"
    if [ "$INST_RESP" = "201" ] || [ "$INST_RESP" = "200" ]; then
      log "  安装结果：action=$ACTION version=$AVER（HTTP $INST_RESP）"
    else
      cat "$TMP/install.json" >&2 || true
      die "安装失败（HTTP $INST_RESP）"
    fi
  fi
else
  log "  --no-install：跳过安装"
fi

log "✓ $APP v$VER 发布完成（仓库 $REPO，tag v$VER）"
