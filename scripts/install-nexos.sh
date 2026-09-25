#!/usr/bin/env bash
# ============================================================
# NexOS 一键安装引导（仓库独立副本）
# （与源节点 GET /api/v1/provisioning/install.sh 动态生成版本同源；
#   本副本缺省指向公网入口 aliyun，可用环境变量覆盖：
#   NEXOS_INSTALL_SOURCE / NEXOS_INSTALL_BOOTSTRAP）
#
# 用法（在一台全新的 NAT 后 Ubuntu 22.04/24.04 上执行一条命令完成安装入网）：
#   sudo bash -c "$(curl -fsSL http://<任一公网入口>:8558/api/v1/provisioning/install.sh)"
#
# 架构自动分流：脚本开头 uname -m 探测——x86_64 下载 dist/os-api，
# aarch64（DGX Spark 等 ARM 机）下载 dist/os-api-aarch64，其他架构报错终止。
#
# 参数：
#   --source URL      安装源 HTTP 入口
#   --name NAME       节点昵称（缺省 = 主机名）
#   --token TOKEN     NEXOS_ADMIN_TOKEN（缺省 change-me-admin-token，装完请更换）
#   --bootstrap LIST  P2P 引导节点，逗号分隔 host:port
#   --port PORT       os-api HTTP 监听端口（缺省 8558）
#   --force           二进制已存在也无条件重新下载（版本一致也强制刷新）
#
# 幂等：重复执行安全（已存在的步骤自动跳过）。**版本感知升级**：本地二进制
# sha256 与源端分发件（NEXOS_INSTALL_SHA256 注入的期望值）一致 → 跳过下载；
# 不一致 → 自动重下替换并提示"升级 X→Y"。老版本脚本无此逻辑，首次升级需 --force。
# 卸载：systemctl disable --now nexos-os-api && rm -rf /opt/nexos \
#       /etc/systemd/system/nexos-os-api.service
# ============================================================
set -euo pipefail

NEXOS_SOURCE_DEFAULT="${NEXOS_INSTALL_SOURCE:-http://203.0.113.2:8558}"
NEXOS_BOOTSTRAP_DEFAULT="${NEXOS_INSTALL_BOOTSTRAP:-203.0.113.2:7070,198.51.100.114:7070}"
NEXOS_SHA256_EXPECTED="${NEXOS_INSTALL_SHA256:-}"
NEXOS_SHA256_EXPECTED_AARCH64="${NEXOS_INSTALL_SHA256_AARCH64:-}"

SRC='' BOOTSTRAP='' NAME='' TOKEN='' PORT='8558' FORCE=0

log()  { printf '\033[32m[nexos]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[nexos]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31m[nexos]\033[0m %s\n' "$*" >&2; exit 1; }

# —— 0) 架构分流（uname -m 自动探测；DGX Spark 等 aarch64 主机同一条命令）——
MACHINE_ARCH="$(uname -m)"
case "$MACHINE_ARCH" in
  x86_64)  ARTIFACT='os-api';         EXPECTED_SHA="$NEXOS_SHA256_EXPECTED" ;;
  aarch64) ARTIFACT='os-api-aarch64'; EXPECTED_SHA="$NEXOS_SHA256_EXPECTED_AARCH64" ;;
  *)
    die "不支持的架构: $MACHINE_ARCH（当前可用: x86_64 → os-api, aarch64 → os-api-aarch64）"
    ;;
esac

usage() {
  cat <<'USAGE'
用法: install-nexos.sh [--source URL] [--name NAME] [--token TOKEN]
                       [--bootstrap LIST] [--port PORT] [--force]
USAGE
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source)    [[ $# -ge 2 ]] || die '--source 需要参数'; SRC="$2"; shift 2 ;;
    --name)      [[ $# -ge 2 ]] || die '--name 需要参数'; NAME="$2"; shift 2 ;;
    --token)     [[ $# -ge 2 ]] || die '--token 需要参数'; TOKEN="$2"; shift 2 ;;
    --bootstrap) [[ $# -ge 2 ]] || die '--bootstrap 需要参数'; BOOTSTRAP="$2"; shift 2 ;;
    --port)      [[ $# -ge 2 ]] || die '--port 需要参数'; PORT="$2"; shift 2 ;;
    --force)     FORCE=1; shift ;;
    -h|--help)   usage ;;
    *) die "未知参数: $1（--help 查看用法）" ;;
  esac
done

[[ $EUID -eq 0 ]] || die '需要 root：sudo bash -c "$(curl -fsSL <安装源>/api/v1/provisioning/install.sh)"'

SRC="${SRC:-$NEXOS_SOURCE_DEFAULT}"
case "$SRC" in http://*|https://*) : ;; *) SRC="http://$SRC" ;; esac
SRC="${SRC%/}"

INSTALL_DIR=/opt/nexos
BIN_PATH="$INSTALL_DIR/os-api"
UNIT_PATH=/etc/systemd/system/nexos-os-api.service
SERVICE_NAME=nexos-os-api

# —— 1) 系统依赖（幂等：缺什么装什么）——
NEED=()
command -v curl    >/dev/null 2>&1 || NEED+=(curl)
command -v git     >/dev/null 2>&1 || NEED+=(git)
command -v crontab >/dev/null 2>&1 || NEED+=(cron)
command -v ssh     >/dev/null 2>&1 || NEED+=(openssh-client)
dpkg -s ca-certificates >/dev/null 2>&1 || NEED+=(ca-certificates)
if [[ ${#NEED[@]} -gt 0 ]]; then
  log "安装系统依赖: ${NEED[*]}"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq || warn 'apt-get update 失败（继续尝试安装）'
  apt-get install -y -qq "${NEED[@]}" || die 'apt 安装依赖失败'
else
  log '系统依赖齐备，跳过 apt 安装'
fi

# —— 2) 本机公网出口 IP（写进 NEXOS_GIT_ADVERTISE_HOST，供集群内他方寻址）——
EGRESS_IP="$(ip -4 route get 1.1.1.1 2>/dev/null | grep -oE 'src [0-9.]+' | awk '{print $2}' | head -n1 || true)"
if [[ -z $EGRESS_IP ]]; then
  EGRESS_IP="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
fi
[[ -n $EGRESS_IP ]] || EGRESS_IP='127.0.0.1'
log "本机出口 IP: $EGRESS_IP"

# —— 3) 下载 os-api 二进制（版本感知升级；Web 前端 rust-embed 已内嵌于二进制，
#       无需单独拉取）——
# 升级判定取舍：用烘焙的源端分发件 sha256 对拍本地文件 sha（零额外请求、不下载
# 即可判定有无新版）；版本号仅用于提示——确定替换后对临时文件跑 --version 与
# 本地版本对比展示"升级 X→Y"。同版本重新编译 sha 也变 → 视为"构建刷新"仍替换。
mkdir -p "$INSTALL_DIR"
NEED_DOWNLOAD=1
LOCAL_SHA=''
if [[ -x $BIN_PATH && $FORCE -ne 1 ]]; then
  LOCAL_SHA="$(sha256sum "$BIN_PATH" | awk '{print $1}')"
  if [[ -n $EXPECTED_SHA && "$LOCAL_SHA" == "$EXPECTED_SHA" ]]; then
    log "os-api 已是源端最新构建（$BIN_PATH, sha256=$LOCAL_SHA），跳过下载"
    NEED_DOWNLOAD=0
  elif [[ -z $EXPECTED_SHA ]]; then
    warn "源端分发件未就绪（install.sh 未烘焙 sha256），保持现有二进制（--force 可强制刷新）"
    NEED_DOWNLOAD=0
  else
    log "检测到新构建（本地 sha256=$LOCAL_SHA ≠ 源端 $EXPECTED_SHA），自动升级..."
  fi
fi
if [[ $NEED_DOWNLOAD -eq 1 ]]; then
  # 本地版本（提示用；跑不动 --version 的旧/异构二进制 → 空串 = 首装口径）
  LOCAL_VER=''
  if [[ -x $BIN_PATH ]]; then
    LOCAL_VER="$("$BIN_PATH" --version 2>/dev/null | head -n1 | awk '{print $NF}' || true)"
  fi
  TMP_FILE="$BIN_PATH.new.$$"
  log "检测到架构 $MACHINE_ARCH，从 $SRC 下载 $ARTIFACT ..."
  curl -fL --progress-bar "$SRC/api/v1/provisioning/dist/$ARTIFACT" -o "$TMP_FILE" \
    || die "下载失败: $SRC/api/v1/provisioning/dist/$ARTIFACT（x86_64: 源节点是否已 POST prepare-distributable？aarch64: 是否已跑 scripts/release.sh 刷新 dist/os-api-aarch64-latest？）"
  [[ "$(head -c 4 "$TMP_FILE")" == $'\x7fELF' ]] || { rm -f "$TMP_FILE"; die '下载内容不是 ELF 二进制，已中止'; }
  ACTUAL_SHA="$(sha256sum "$TMP_FILE" | awk '{print $1}')"
  if [[ -n $EXPECTED_SHA && "$ACTUAL_SHA" != "$EXPECTED_SHA" ]]; then
    rm -f "$TMP_FILE"
    die "sha256 不匹配: 期望 $EXPECTED_SHA, 实际 $ACTUAL_SHA"
  fi
  chmod 755 "$TMP_FILE"
  NEW_VER="$("$TMP_FILE" --version 2>/dev/null | head -n1 | awk '{print $NF}' || true)"
  mv -f "$TMP_FILE" "$BIN_PATH"
  if [[ -n $LOCAL_VER && -n $NEW_VER && "$LOCAL_VER" != "$NEW_VER" ]]; then
    log "升级 os-api：$LOCAL_VER → $NEW_VER（$BIN_PATH, sha256=$ACTUAL_SHA）"
  elif [[ -n $LOCAL_VER ]]; then
    log "os-api 构建已刷新（版本 ${NEW_VER:-?} 不变, $BIN_PATH, sha256=$ACTUAL_SHA）"
  else
    log "os-api 就绪（$BIN_PATH, v${NEW_VER:-未知}, sha256=$ACTUAL_SHA）"
  fi
fi

# —— 4) systemd 服务（整文件重写 + enable --now 天然幂等）——
# 更新源引导：NEXOS_UPDATE_REPO_URL 指向安装源节点的 NexHub git HTTP 通道
# （$SRC/git/nexos.git——新节点无 /tank 本地裸仓库，「更新」应用的 check 走
# git ls-remote --tags 纯网络查询，开箱即有可用更新源）。unit 整文件重写，
# 重复执行天然幂等更新该行（--source 换源后随 $SRC 跟随）。
NODE_NAME="${NAME:-$(hostname)}"
BOOTSTRAP_LIST="${BOOTSTRAP:-$NEXOS_BOOTSTRAP_DEFAULT}"
ADMIN_TOKEN="${TOKEN:-change-me-admin-token}"

cat > "$UNIT_PATH" <<UNIT
[Unit]
Description=NexOS API Gateway (os-api) — one-shot bootstrap install
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$INSTALL_DIR
ExecStart=$BIN_PATH --addr 0.0.0.0:$PORT
Restart=always
RestartSec=3
Environment=NEXOS_ADMIN_TOKEN=$ADMIN_TOKEN
Environment=NEXOS_P2P_ENABLE=1
Environment=NEXOS_P2P_NAME=$NODE_NAME
Environment=NEXOS_P2P_BOOTSTRAP=$BOOTSTRAP_LIST
Environment=NEXOS_P2P_LISTEN=:7070
Environment=NEXOS_GIT_ADVERTISE_HOST=$EGRESS_IP
Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

command -v systemctl >/dev/null 2>&1 || die '未检测到 systemd（目标环境为 Ubuntu 22.04/24.04）'
systemctl daemon-reload
# 升级路径必须显式 restart：`enable --now` 对已运行服务是空操作（幂等成功、
# 不换进程）——Spark 实测踩坑：二进制升到 0.1.13 页面仍显示 0.1.4（旧进程
# 还在内存里跑旧 rust-embed 前端）。仅在二进制被替换过时 restart（首装/
# 未变化不动，避免无谓打断）。
if [[ $NEED_DOWNLOAD -eq 1 ]]; then
  log '二进制已更新，重启服务生效...'
  systemctl enable "$SERVICE_NAME" >/dev/null 2>&1 || true
  systemctl restart "$SERVICE_NAME" || die "服务重启失败（journalctl -u $SERVICE_NAME 排查）"
else
  systemctl enable --now "$SERVICE_NAME" >/dev/null 2>&1 || systemctl restart "$SERVICE_NAME"
fi

# —— 5) 健康确认：等 P2P 自身份出现并摘取 NodeID ——
STATUS=''
for _ in $(seq 1 30); do
  STATUS="$(curl -sf "http://127.0.0.1:$PORT/api/v1/p2p/status" || true)"
  [[ -n $STATUS ]] && break
  sleep 1
done
NODE_ID="$(printf '%s' "$STATUS" | sed -n 's/.*"node_id":"\([^"]*\)".*/\1/p')"
[[ -n $NODE_ID ]] || NODE_ID='(获取失败, 查看 journalctl -u nexos-os-api)'
PEER_COUNT="$(printf '%s' "$STATUS" | sed -n 's/.*"peers_connected":\([0-9]*\).*/\1/p')"
[[ -n $PEER_COUNT ]] || PEER_COUNT='?'

cat <<SUMMARY

============================================================
  NexOS 安装完成
------------------------------------------------------------
  控制台:      http://$EGRESS_IP:$PORT
  NodeID:      $NODE_ID
  节点昵称:    $NODE_NAME
  P2P 引导:    $BOOTSTRAP_LIST
  已连节点数:  $PEER_COUNT
  Admin Token: $ADMIN_TOKEN   <- 默认值, 请尽快更换
------------------------------------------------------------
  集群确认: curl http://127.0.0.1:$PORT/api/v1/p2p/status
  服务日志: journalctl -u $SERVICE_NAME -f
============================================================
SUMMARY
