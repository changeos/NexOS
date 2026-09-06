#!/usr/bin/env bash
# ============================================================
# nexhub —— NexHub 代码托管 / 应用分发 CLI（单文件自包含）
#
# 由节点端点 GET /api/v1/coderepo/cli.sh 动态生成（脚本内烘焙缺省节点
# 地址；安装 / 自升级都回到同一端点）。安装（一条命令）：
#
#   curl -fsSL http://<节点地址>:8558/api/v1/coderepo/cli.sh | sh
#
# 命令面：
#   nexhub login [token] | [node-url] | <node-url> <token>
#                              保存凭据（~/.config/nexhub/credentials，0600）
#   nexhub whoami              节点信息 + token 有效性
#   nexhub ping                连通性 + token 校验（退出码 0/1，供脚本）
#   nexhub repo list [--json]  仓库列表
#   nexhub repo create <name> [desc]
#   nexhub repo delete <name> [--yes]        删除仓库（确认提示）
#   nexhub repo info <name> [--json]         详情（Clone (SSH)/(HTTP) 两行）
#   nexhub clone <repo>        git clone（token 不进 URL）
#   nexhub apps list [--json]  应用目录（nexos-app-* 仓库）
#   nexhub apps deploy <repo> [--json]       部署/升级（action=install/upgrade/noop）
#   nexhub apps remove <id> [--yes]          卸载应用（确认提示）
#   nexhub self-update         重新拉取 cli.sh 覆盖自身
#   nexhub help                本帮助
#
# 环境：NEXHUB_NODE 覆盖已存节点（如 http://10.0.0.2:8558）；
#       NEXHUB_TOKEN 覆盖已存 token（CI 场景）。
# 依赖：curl 必须；jq 首选（缺失降级 python3；两者皆无仅支持 --json）。
# 兼容：POSIX sh 语法（bash/dash 均可解释；`curl | sh` 在 dash 下同样可安装），
#       避免 bash4 特性以兼容 macOS 自带 bash 3.2。
# 安全：token 只从 env/凭据文件读取，经 `curl -H @file` 注入，不进 argv（防 ps 泄漏）。
# ============================================================

set -eu

# —— 端点渲染时烘焙的缺省值（@@...@@ 占位符由服务端替换）——
NEXHUB_NODE_DEFAULT='@@NEXHUB_NODE@@'
NEXHUB_CLI_URL='@@NEXHUB_CLI_URL@@'
NEXHUB_VERSION='@@NEXHUB_VERSION@@'

CRED_DIR="${HOME}/.config/nexhub"
CRED_FILE="${CRED_DIR}/credentials"

NH_AUTH_FILE=''   # curl -H @file 凭据头临时文件（EXIT 清理）
NH_STATUS=''      # 最近一次 nh_api 的 HTTP 状态码
NH_BODY=''        # 最近一次 nh_api 的响应体
NH_JSON_RAW=0     # --json：原样输出响应体
NH_ASSUME_YES=0   # --yes：跳过确认

# ----------------------------------------------------------------------------
# 输出辅助（错误统一 stderr 红字；非 TTY 退色）
# ----------------------------------------------------------------------------

if [ -t 2 ]; then
  C_RED=$(printf '\033[31m'); C_GRN=$(printf '\033[32m'); C_YEL=$(printf '\033[33m'); C_RST=$(printf '\033[0m')
else
  C_RED=''; C_GRN=''; C_YEL=''; C_RST=''
fi

log()  { printf '%s[nexhub]%s %s\n' "$C_GRN" "$C_RST" "$*"; }
warn() { printf '%s[nexhub]%s %s\n' "$C_YEL" "$C_RST" "$*" >&2; }
die()  { printf '%s[nexhub]%s %s\n' "$C_RED" "$C_RST" "$*" >&2; exit 1; }
usage_err() { printf '%s[nexhub]%s %s\n' "$C_RED" "$C_RST" "$*" >&2; exit 2; }

nh_have() { command -v "$1" >/dev/null 2>&1; }

nh_cleanup() {
  if [ -n "$NH_AUTH_FILE" ]; then
    rm -f "$NH_AUTH_FILE" 2>/dev/null || true
  fi
  return 0
}

nh_is_2xx() {
  case "$1" in
    2*) return 0 ;;
  esac
  return 1
}

# ----------------------------------------------------------------------------
# 凭据（~/.config/nexhub/credentials，0600，格式 NODE_URL=…\nTOKEN=…）
# ----------------------------------------------------------------------------

nh_cred_get() { # $1=键名（NODE_URL / TOKEN）→ 值（文件缺失输出空）
  if [ -r "$CRED_FILE" ]; then
    sed -n "s/^$1=//p" "$CRED_FILE" | head -n 1
  fi
  return 0
}

nh_save_credentials() { # $1=node_url $2=token（临时文件 0600 + 原子 rename）
  mkdir -p "$CRED_DIR" || die "创建凭据目录失败: $CRED_DIR"
  tmp_cred="${CRED_FILE}.tmp.$$"
  ( umask 077 && printf 'NODE_URL=%s\nTOKEN=%s\n' "$1" "$2" > "$tmp_cred" ) \
    || die "写凭据临时文件失败: $tmp_cred"
  chmod 600 "$tmp_cred"
  mv -f "$tmp_cred" "$CRED_FILE" || die "保存凭据失败: $CRED_FILE"
  chmod 600 "$CRED_FILE" 2>/dev/null || true
}

nh_node() { # 节点地址：NEXHUB_NODE env > 已存凭据 > 端点烘焙缺省
  local saved
  if [ -n "${NEXHUB_NODE:-}" ]; then
    printf '%s' "$NEXHUB_NODE"
    return 0
  fi
  saved="$(nh_cred_get NODE_URL)"
  if [ -n "$saved" ]; then
    printf '%s' "$saved"
  else
    printf '%s' "$NEXHUB_NODE_DEFAULT"
  fi
}

nh_token() { # token：NEXHUB_TOKEN env > 已存凭据
  if [ -n "${NEXHUB_TOKEN:-}" ]; then
    printf '%s' "$NEXHUB_TOKEN"
  else
    nh_cred_get TOKEN
  fi
}

nh_auth_file() { # $1=token → 写临时头文件（0600），token 不进 argv
  if [ -z "$NH_AUTH_FILE" ]; then
    NH_AUTH_FILE="$(mktemp "${TMPDIR:-/tmp}/nexhub-hdr.XXXXXX")" || die '创建临时头文件失败'
    chmod 600 "$NH_AUTH_FILE"
  fi
  printf 'Authorization: Bearer %s\n' "$1" > "$NH_AUTH_FILE"
}

# ----------------------------------------------------------------------------
# HTTP（curl 必须）+ 响应解析（jq 首选，降级 python3）
# ----------------------------------------------------------------------------

nh_api() { # $1=METHOD $2=PATH [$3=JSON body] → NH_STATUS / NH_BODY
  method="$1" path="$2" api_body="${3:-}"
  api_base="$(nh_node)"
  api_base="${api_base%/}"
  api_tok="$(nh_token)"
  api_tmp="$(mktemp "${TMPDIR:-/tmp}/nexhub-res.XXXXXX")" || die '创建临时文件失败'
  api_code=''
  if [ -n "$api_tok" ] && [ -n "$api_body" ]; then
    nh_auth_file "$api_tok"
    api_code="$(curl -sS -X "$method" -H "@$NH_AUTH_FILE" \
      -H 'Content-Type: application/json' --data-binary "$api_body" \
      -o "$api_tmp" -w '%{http_code}' "$api_base$path" 2>/dev/null)" || api_code=''
  elif [ -n "$api_tok" ]; then
    nh_auth_file "$api_tok"
    api_code="$(curl -sS -X "$method" -H "@$NH_AUTH_FILE" \
      -o "$api_tmp" -w '%{http_code}' "$api_base$path" 2>/dev/null)" || api_code=''
  elif [ -n "$api_body" ]; then
    api_code="$(curl -sS -X "$method" -H 'Content-Type: application/json' \
      --data-binary "$api_body" -o "$api_tmp" -w '%{http_code}' "$api_base$path" 2>/dev/null)" || api_code=''
  else
    api_code="$(curl -sS -X "$method" \
      -o "$api_tmp" -w '%{http_code}' "$api_base$path" 2>/dev/null)" || api_code=''
  fi
  if [ -z "$api_code" ]; then
    rm -f "$api_tmp"
    die "无法连接节点 $api_base（网络错误或节点不可达；NEXHUB_NODE 可覆盖节点地址）"
  fi
  NH_STATUS="$api_code"
  NH_BODY="$(cat "$api_tmp")"
  rm -f "$api_tmp"
  return 0
}

# JSON→字符串/TSV 提取的 python3 实现（jq 缺失时的降级路径）。
NH_PY_SRC="$(cat <<'NH_PY_EOF'
import json, sys

def main():
    shape = sys.argv[1] if len(sys.argv) > 1 else ""
    arg = sys.argv[2] if len(sys.argv) > 2 else ""
    try:
        d = json.load(sys.stdin)
    except Exception:
        return
    if shape == "version":
        print(d.get("version") or "")
    elif shape == "errmsg":
        print(d.get("error") or "")
    elif shape == "repos":
        for r in d.get("repos") or []:
            cells = [r.get("name") or "", r.get("description") or "",
                     str(r.get("branch_count") or 0),
                     str(r.get("commit_count") or 0)]
            print("\t".join(cells))
    elif shape == "repo":
        for r in d.get("repos") or []:
            if r.get("name") == arg:
                for k in ("description", "clone_url_ssh", "clone_url_http"):
                    print(r.get(k) or "")
                print(str(r.get("branch_count") or 0))
                print(str(r.get("commit_count") or 0))
                print(r.get("last_commit") or "")
                print(r.get("last_commit_date") or "")
                break
    elif shape == "clone":
        for r in d.get("repos") or []:
            if r.get("name") == arg:
                print(r.get("clone_url_http") or "")
                print(r.get("clone_url_ssh") or "")
                break
    elif shape == "catalog":
        for a in d.get("apps") or []:
            if a.get("error"):
                st = "error"
            elif a.get("installed"):
                st = "installed"
            else:
                st = "available"
            cells = [a.get("repo") or "", a.get("version") or "-",
                     a.get("installed_version") or "-", st, a.get("error") or ""]
            print("\t".join(cells))
    elif shape == "install":
        print(d.get("action") or "")
        app = d.get("app") or {}
        print(app.get("version") or "")
    elif shape == "contents":
        print(d.get("default_branch") or "")
        print(" ".join(d.get("branches") or []))
    elif shape == "uninstall":
        print(d.get("id") or "")

main()
NH_PY_EOF
)"

nh_parse() { # $1=shape [$2=参数]；解析 NH_BODY → stdout；退出 3=无解析器
  parse_shape="$1"
  parse_arg="${2:-}"
  if nh_have jq; then
    case "$parse_shape" in
      version)   printf '%s' "$NH_BODY" | jq -r '.version // empty' ;;
      errmsg)    printf '%s' "$NH_BODY" | jq -r '.error // empty' ;;
      uninstall) printf '%s' "$NH_BODY" | jq -r '.id // empty' ;;
      repos)     printf '%s' "$NH_BODY" | jq -r \
                   '.repos[] | [.name, (.description // ""), ((.branch_count // 0)|tostring), ((.commit_count // 0)|tostring)] | @tsv' ;;
      repo)      printf '%s' "$NH_BODY" | jq -r --arg n "$parse_arg" \
                   '.repos[] | select(.name == $n) |
                    [(.description // ""), (.clone_url_ssh // ""), (.clone_url_http // ""),
                     ((.branch_count // 0)|tostring), ((.commit_count // 0)|tostring),
                     (.last_commit // ""), (.last_commit_date // "")] | .[]' ;;
      clone)     printf '%s' "$NH_BODY" | jq -r --arg n "$parse_arg" \
                   '.repos[] | select(.name == $n) | [(.clone_url_http // ""), (.clone_url_ssh // "")] | .[]' ;;
      catalog)   printf '%s' "$NH_BODY" | jq -r \
                   '.apps[] | [.repo, (.version // "-"), (.installed_version // "-"),
                    (if .error then "error" elif .installed then "installed" else "available" end),
                    (.error // "")] | @tsv' ;;
      install)   printf '%s' "$NH_BODY" | jq -r \
                   '[(.action // ""), ((.app // {}) | (.version // ""))] | .[]' ;;
      contents)  printf '%s' "$NH_BODY" | jq -r \
                   '[(.default_branch // ""), ((.branches // []) | join(" "))] | .[]' ;;
      *) return 2 ;;
    esac
  elif nh_have python3; then
    printf '%s' "$NH_BODY" | python3 -c "$NH_PY_SRC" "$parse_shape" "$parse_arg"
  else
    return 3
  fi
}

nh_err_msg() { # 错误响应体 → 人类可读消息（解析失败回落原文截断）
  err_msg=''
  err_msg="$(printf '%s' "$NH_BODY" | nh_parse errmsg 2>/dev/null)" || err_msg=''
  if [ -n "$err_msg" ]; then
    printf '%s' "$err_msg"
  else
    printf '%s' "$NH_BODY" | head -c 300
  fi
}

nh_api_ok() { # $1=动作说明；NH_STATUS 非 2xx → 红字报错退出 1
  if nh_is_2xx "$NH_STATUS"; then
    return 0
  fi
  die "$1 失败（HTTP $NH_STATUS）: $(nh_err_msg)"
}

nh_need_parser() {
  if [ "$NH_JSON_RAW" -eq 1 ]; then
    return 0
  fi
  if ! nh_have jq && ! nh_have python3; then
    die '解析响应需要 jq 或 python3 之一（或追加 --json 原样输出）'
  fi
}

nh_json_escape() { # $1 → JSON 字符串字面量（含引号）
  if nh_have jq; then
    printf '%s' "$1" | jq -Rs .
  elif nh_have python3; then
    printf '%s' "$1" | python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.stdin.read()))'
  else
    printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
  fi
}

nh_table() { # $1=空格分隔表头；stdin=TSV 行 → 对齐表格
  table_hdr="$1"
  table_data="$(cat)"
  if [ -z "$table_data" ]; then
    log '(无记录)'
    return 0
  fi
  printf '%s\n' "$table_data" | awk -F'\t' -v hdr="$table_hdr" '
    { rows[NR] = $0; if (NF > maxn) maxn = NF
      for (i = 1; i <= NF; i++) { n = length($i); if (n > w[i]) w[i] = n } }
    END {
      m = split(hdr, H, " ")
      if (m > maxn) maxn = m
      for (i = 1; i <= m; i++) { n = length(H[i]); if (n > w[i]) w[i] = n }
      for (i = 1; i <= maxn; i++) if (!w[i]) w[i] = 0
      printf "  "
      for (i = 1; i <= maxn; i++) { f = "%-" w[i] "s"; printf "%s  ", sprintf(f, H[i]) }
      printf "\n  "
      for (i = 1; i <= maxn; i++) { for (j = 0; j < w[i]; j++) printf "-"; printf "  " }
      printf "\n"
      for (r = 1; r <= NR; r++) {
        printf "  "
        split(rows[r], C, "\t")
        for (i = 1; i <= maxn; i++) { f = "%-" w[i] "s"; printf "%s  ", sprintf(f, C[i]) }
        printf "\n"
      }
    }'
}

nh_confirm() { # $1=提示 → 0=确认（--yes 直接通过）
  if [ "$NH_ASSUME_YES" -eq 1 ]; then
    return 0
  fi
  printf '%s' "$1"
  if IFS= read -r confirm_ans; then
    case "$confirm_ans" in
      y|Y|yes|YES|Yes) return 0 ;;
      *)               return 1 ;;
    esac
  fi
  return 1
}

nh_mask_token() { # token 尾 4 位脱敏展示
  mask_t="$1"
  if [ -z "$mask_t" ]; then
    printf '(未登录)'
    return 0
  fi
  if [ "${#mask_t}" -le 8 ]; then
    printf '****'
    return 0
  fi
  printf '****%s' "$(printf '%s' "$mask_t" | tail -c 5)"
}

# ----------------------------------------------------------------------------
# 命令：login / whoami / ping
# ----------------------------------------------------------------------------

nh_read_secret() { # $1=提示（%b 解释 \n）→ NH_SECRET（TTY 上关闭回显）
  printf '%b' "$1"
  NH_SECRET=''
  if [ -t 0 ] && nh_have stty; then
    stty -echo 2>/dev/null || true
    if IFS= read -r NH_SECRET; then
      : 
    else
      NH_SECRET=''
    fi
    stty echo 2>/dev/null || true
    printf '\n'
  else
    if IFS= read -r NH_SECRET; then
      :
    else
      NH_SECRET=''
    fi
  fi
  return 0
}

cmd_login() { # login [token] | login [node-url] | login <node-url> <token>
  login_node=''
  login_tok=''
  case $# in
    0)
      login_node="$(nh_node)"
      nh_read_secret "NexHub 节点: ${login_node}\n粘贴 token（回车确认）: "
      login_tok="$NH_SECRET"
      [ -n "$login_tok" ] || usage_err 'token 不可为空（用法: nexhub login [token] 或 login <node-url> <token>）'
      ;;
    1)
      case "$1" in
        *://*) # 形似节点地址 → 只给了节点，token 交互输入
          login_node="$1"
          nh_read_secret "NexHub 节点: ${login_node}\n粘贴 token（回车确认）: "
          login_tok="$NH_SECRET"
          [ -n "$login_tok" ] || usage_err 'token 不可为空'
          ;;
        *)
          login_node="$(nh_node)"
          login_tok="$1"
          ;;
      esac
      ;;
    2)
      login_node="$1"
      login_tok="$2"
      ;;
    *)
      usage_err '用法: nexhub login [token] 或 nexhub login <node-url> <token>'
      ;;
  esac
  case "$login_node" in
    http://*|https://*) : ;;
    *) login_node="http://$login_node" ;;
  esac
  login_node="${login_node%/}"
  nh_save_credentials "$login_node" "$login_tok"
  log "凭据已保存: $CRED_FILE（0600）节点 $login_node"
  log '下一步: nexhub ping   或   nexhub repo list'
}

nh_check_token() { # token 有效性（经既有 admin 读端点探测；无 token=未登录）
  check_tok="$(nh_token)"
  if [ -z "$check_tok" ]; then
    printf 'token:  未登录（nexhub login 配置；写操作将 401）\n'
    return 0
  fi
  nh_api GET /api/v1/provisioning/ssh/deploys
  case "$NH_STATUS" in
    200) printf 'token:  有效（%s，admin 权限）\n' "$(nh_mask_token "$check_tok")" ;;
    401) die "token 无效（401）——重新 nexhub login 或检查 NEXHUB_TOKEN" ;;
    403) printf 'token:  有效（%s，权限受限）\n' "$(nh_mask_token "$check_tok")" ;;
    *)   printf 'token:  校验未定（HTTP %s）\n' "$NH_STATUS" ;;
  esac
}

cmd_whoami() {
  nh_api GET /api/v1/version
  nh_api_ok '查询节点版本'
  who_ver="$(nh_parse version 2>/dev/null)" || who_ver=''
  [ -n "$who_ver" ] || who_ver='?'
  who_base="$(nh_node)"
  who_base="${who_base%/}"
  printf '节点:   %s\n' "$who_base"
  printf '版本:   %s\n' "$who_ver"
  printf '凭据:   %s\n' "$CRED_FILE"
  nh_check_token
}

cmd_ping() { # 连通 + token 有效（401 → 报"token 无效"退出 1）
  nh_api GET /api/v1/version
  ping_base="$(nh_node)"
  ping_base="${ping_base%/}"
  if ! nh_is_2xx "$NH_STATUS"; then
    die "节点不可达: $ping_base（HTTP $NH_STATUS）"
  fi
  ping_ver="$(nh_parse version 2>/dev/null)" || ping_ver=''
  if [ -n "$(nh_token)" ]; then
    nh_check_token
  fi
  log "ok $ping_base（v${ping_ver:-?}）"
}

# ----------------------------------------------------------------------------
# 命令：repo
# ----------------------------------------------------------------------------

cmd_repo_list() {
  nh_api GET /api/v1/coderepo/repos
  nh_api_ok '获取仓库列表'
  if [ "$NH_JSON_RAW" -eq 1 ]; then
    printf '%s\n' "$NH_BODY"
    return 0
  fi
  nh_need_parser
  nh_parse repos | nh_table 'NAME DESCRIPTION BRANCH COMMITS'
}

cmd_repo_create() { # repo create <name> [desc]
  if [ $# -lt 1 ]; then
    usage_err '用法: nexhub repo create <name> [描述]'
  fi
  create_name="$1"
  create_desc="${2:-}"
  nh_api POST /api/v1/coderepo/repos \
    "{\"name\":$(nh_json_escape "$create_name"),\"description\":$(nh_json_escape "$create_desc")}"
  case "$NH_STATUS" in
    201) log "仓库已创建: $create_name" ;;
    409) die "创建仓库失败: 仓库已存在: $create_name" ;;
    *)   nh_api_ok '创建仓库' ;;
  esac
}

cmd_repo_delete() { # repo delete <name> [--yes]
  if [ $# -lt 1 ]; then
    usage_err '用法: nexhub repo delete <name> [--yes]'
  fi
  del_name="$1"
  if nh_confirm "确认删除仓库 $del_name（不可恢复）? [y/N] "; then
    nh_api DELETE "/api/v1/coderepo/repos/$del_name"
    nh_api_ok '删除仓库'
    log "仓库已删除: $del_name"
  else
    warn '已取消'
  fi
}

cmd_repo_info() { # repo info <name> —— clone URL ssh+http 两行 + 元数据
  if [ $# -lt 1 ]; then
    usage_err '用法: nexhub repo info <name>'
  fi
  info_name="$1"
  nh_need_parser
  nh_api GET /api/v1/coderepo/repos
  nh_api_ok '获取仓库列表'
  info_meta="$(nh_parse repo "$info_name")" || info_meta=''
  if [ -z "$info_meta" ]; then
    die "仓库不存在: $info_name（nexhub repo list 查看可用仓库）"
  fi
  info_desc="$(printf '%s\n' "$info_meta" | sed -n '1p')"
  info_ssh="$(printf '%s\n' "$info_meta" | sed -n '2p')"
  info_http="$(printf '%s\n' "$info_meta" | sed -n '3p')"
  info_branches="$(printf '%s\n' "$info_meta" | sed -n '4p')"
  info_commits="$(printf '%s\n' "$info_meta" | sed -n '5p')"
  info_last="$(printf '%s\n' "$info_meta" | sed -n '6p')"
  info_last_date="$(printf '%s\n' "$info_meta" | sed -n '7p')"
  info_default='?'
  info_branch_list=''
  nh_api GET "/api/v1/coderepo/repos/$info_name/contents"
  if nh_is_2xx "$NH_STATUS"; then
    info_contents="$(nh_parse contents 2>/dev/null)" || info_contents=''
    info_default="$(printf '%s\n' "$info_contents" | sed -n '1p')"
    info_branch_list="$(printf '%s\n' "$info_contents" | sed -n '2p')"
  fi
  info_date_suffix=''
  if [ -n "$info_last_date" ]; then
    info_date_suffix="（$info_last_date）"
  fi
  printf '名称:     %s\n' "$info_name"
  printf '描述:     %s\n' "$info_desc"
  printf '分支:     %s（默认 %s，共 %s 个）\n' \
    "${info_branch_list:-?}" "$info_default" "$info_branches"
  printf '提交数:   %s\n' "$info_commits"
  printf '最近提交: %s%s\n' "${info_last:-（空仓库）}" "$info_date_suffix"
  printf 'Clone (SSH):  %s\n' "${info_ssh:-（不可用）}"
  printf 'Clone (HTTP): %s\n' "${info_http:-（不可用）}"
}

cmd_clone() { # clone <repo> —— 拉 clone_url_http 并 git clone（token 不进 URL）
  if [ $# -lt 1 ]; then
    usage_err '用法: nexhub clone <repo>'
  fi
  clone_repo="$1"
  clone_http=''
  clone_ssh=''
  case "$clone_repo" in
    http://*|https://*|ssh://*|git@*)
      clone_http="$clone_repo"
      ;;
    *)
      nh_need_parser
      nh_api GET /api/v1/coderepo/repos
      nh_api_ok '获取仓库列表'
      clone_pair="$(nh_parse clone "$clone_repo")" || clone_pair=''
      clone_http="$(printf '%s\n' "$clone_pair" | sed -n '1p')"
      clone_ssh="$(printf '%s\n' "$clone_pair" | sed -n '2p')"
      if [ -z "$clone_http" ] && [ -z "$clone_ssh" ]; then
        die "仓库不存在: $clone_repo（nexhub repo list 查看可用仓库）"
      fi
      if [ -z "$clone_http" ]; then
        clone_http="$(nh_node)/git/$clone_repo.git"
      fi
      ;;
  esac
  if ! nh_have git; then
    die "未安装 git。手动克隆: git clone $clone_http"
  fi
  log "克隆 $clone_http"
  warn '说明: 匿名读已放行；push 时 HTTPS 凭据用户名任意、密码=token（git 会主动提示）；或用 SSH 地址'
  if [ -n "$clone_ssh" ]; then
    warn "SSH: $clone_ssh"
  fi
  nh_cleanup
  exec git clone "$clone_http"
}

# ----------------------------------------------------------------------------
# 命令：apps
# ----------------------------------------------------------------------------

cmd_apps_list() {
  nh_api GET /api/v1/apps/catalog
  nh_api_ok '获取应用目录'
  if [ "$NH_JSON_RAW" -eq 1 ]; then
    printf '%s\n' "$NH_BODY"
    return 0
  fi
  nh_need_parser
  # 列: repo / 最新版本 / 已装版本 / 状态（已装(最新)/可升级/未装/错误）/错误详情
  nh_parse catalog | awk -F'\t' '
    {
      st = $4
      if (st == "error")                          label = "错误"
      else if (st == "installed" && $2 == $3)     label = "已装(最新)"
      else if (st == "installed")                 label = "可升级"
      else                                        label = "未装"
      printf "%s\t%s\t%s\t%s\t%s\n", $1, $2, $3, label, $5
    }' | nh_table 'REPO VERSION INSTALLED STATUS DETAIL'
}

cmd_apps_deploy() { # apps deploy <repo> → action（install/upgrade/noop）+ 版本
  if [ $# -lt 1 ]; then
    usage_err '用法: nexhub apps deploy <repo>'
  fi
  deploy_repo="$1"
  nh_need_parser
  nh_api POST /api/v1/apps/install "{\"repo\":$(nh_json_escape "$deploy_repo")}"
  nh_api_ok "部署应用 $deploy_repo"
  if [ "$NH_JSON_RAW" -eq 1 ]; then
    printf '%s\n' "$NH_BODY"
    return 0
  fi
  deploy_out="$(nh_parse install)" || deploy_out=''
  deploy_action="$(printf '%s\n' "$deploy_out" | sed -n '1p')"
  deploy_ver="$(printf '%s\n' "$deploy_out" | sed -n '2p')"
  case "$deploy_action" in
    install) log "已安装 $deploy_repo v${deploy_ver:-?}（桌面 / Launchpad 可见，免刷新热注册）" ;;
    upgrade) log "已升级 $deploy_repo → v${deploy_ver:-?}" ;;
    noop)    log "$deploy_repo 已是最新 v${deploy_ver:-?}（无需操作）" ;;
    *)       warn "未知安装结果 action=$deploy_action" ;;
  esac
}

cmd_apps_remove() { # apps remove <id> [--yes]
  if [ $# -lt 1 ]; then
    usage_err '用法: nexhub apps remove <id> [--yes]'
  fi
  rm_id="$1"
  if nh_confirm "确认卸载应用 $rm_id（删除应用目录，不可恢复）? [y/N] "; then
    nh_api DELETE "/api/v1/apps/$rm_id"
    nh_api_ok '卸载应用'
    log "应用已卸载: $rm_id"
  else
    warn '已取消'
  fi
}

cmd_apps() {
  apps_sub="${1:-list}"
  if [ $# -gt 0 ]; then
    shift
  fi
  case "$apps_sub" in
    list)             cmd_apps_list "$@" ;;
    deploy)           cmd_apps_deploy "$@" ;;
    remove|uninstall) cmd_apps_remove "$@" ;;
    *) usage_err "未知 apps 子命令: $apps_sub（list / deploy / remove）" ;;
  esac
}

cmd_repo() {
  repo_sub="${1:-list}"
  if [ $# -gt 0 ]; then
    shift
  fi
  case "$repo_sub" in
    list)      cmd_repo_list "$@" ;;
    create)    cmd_repo_create "$@" ;;
    delete|rm) cmd_repo_delete "$@" ;;
    info)      cmd_repo_info "$@" ;;
    *) usage_err "未知 repo 子命令: $repo_sub（list / create / delete / info）" ;;
  esac
}

# ----------------------------------------------------------------------------
# 命令：self-update（重新拉 cli.sh 覆盖自身）
# ----------------------------------------------------------------------------

cmd_self_update() {
  upd_self="$(command -v nexhub 2>/dev/null)" || upd_self=''
  if [ -z "$upd_self" ]; then
    upd_self="$0"
  fi
  if [ ! -f "$upd_self" ]; then
    die "无法定位 nexhub 安装路径（$upd_self）；手动: curl -fsSL <节点>/api/v1/coderepo/cli.sh -o /usr/local/bin/nexhub && chmod 755 /usr/local/bin/nexhub"
  fi
  upd_base="$(nh_node)"
  upd_base="${upd_base%/}"
  upd_url="$upd_base/api/v1/coderepo/cli.sh"
  upd_tmp="${upd_self}.update.$$"
  log "从 $upd_url 更新 nexhub..."
  if ! curl -fsSL "$upd_url" -o "$upd_tmp"; then
    rm -f "$upd_tmp"
    die "下载失败: $upd_url（节点是否已升级到含 cli.sh 端点的版本？）"
  fi
  if nh_have bash && ! bash -n "$upd_tmp" 2>/dev/null; then
    rm -f "$upd_tmp"
    die '下载内容语法校验失败（bash -n），已中止覆盖'
  fi
  chmod 755 "$upd_tmp"
  if mv -f "$upd_tmp" "$upd_self"; then
    log "nexhub 已更新: $upd_self（原版本 v$NEXHUB_VERSION → 最新）"
  else
    rm -f "$upd_tmp"
    die "覆盖 $upd_self 失败（权限不足？用 sudo 重试）"
  fi
}

# ----------------------------------------------------------------------------
# 帮助 / 全局分发
# ----------------------------------------------------------------------------

cmd_help() {
  cat <<HELP
nexhub —— NexHub 代码托管 / 应用分发 CLI（v$NEXHUB_VERSION，单文件随节点分发）

安装（一条命令）:
  curl -fsSL $NEXHUB_CLI_URL | sh

命令:
  nexhub login [token] | [node-url] | <node-url> <token>
                              保存凭据到 $CRED_FILE（0600）
  nexhub whoami               节点地址/版本 + token 有效性
  nexhub ping                 连通性 + token 校验（退出码 0/1，供脚本）
  nexhub repo list [--json]   仓库列表（name/描述/分支/提交）
  nexhub repo create <name> [desc]
  nexhub repo delete <name> [--yes]
  nexhub repo info <name>     详情（Clone (SSH)/(HTTP) 两行）
  nexhub clone <repo>         git clone（token 不进 URL；push 凭据=token）
  nexhub apps list [--json]   应用目录（repo/版本/已装/状态）
  nexhub apps deploy <repo>   部署/升级（action=install/upgrade/noop）
  nexhub apps remove <id> [--yes]
  nexhub self-update          重新拉取 cli.sh 覆盖自身
  nexhub help                 本帮助

全局:
  --json                      原样输出响应体（list/deploy/info）
  --yes                       跳过删除/卸载确认
  NEXHUB_NODE=URL             覆盖节点地址（env > 凭据 > 安装时烘焙缺省）
  NEXHUB_TOKEN=TOKEN          覆盖 token（CI 场景）

退出码: 0 成功 / 1 远端或运行时错误 / 2 参数错误
HELP
}

cli_main() {
  trap nh_cleanup EXIT
  # 全局 flag 摘除（任意位置；POSIX 手法保留参数边界）：
  # for 的词表在进入时一次性展开，循环内 set -- 追加不影响遍历。
  NH_JSON_RAW=0
  NH_ASSUME_YES=0
  cli_n_orig=$#
  for cli_a in "$@"; do
    case "$cli_a" in
      --json) NH_JSON_RAW=1 ;;
      --yes)  NH_ASSUME_YES=1 ;;
      *)      set -- "$@" "$cli_a" ;;
    esac
  done
  if [ "$cli_n_orig" -gt 0 ]; then
    shift "$cli_n_orig"
  fi
  cli_cmd="${1:-help}"
  if [ $# -gt 0 ]; then
    shift
  fi
  case "$cli_cmd" in
    login)          cmd_login "$@" ;;
    whoami)         cmd_whoami ;;
    ping)           cmd_ping ;;
    repo)           cmd_repo "$@" ;;
    clone)          cmd_clone "$@" ;;
    apps)           cmd_apps "$@" ;;
    self-update)    cmd_self_update ;;
    version|--version) printf 'nexhub v%s\n' "$NEXHUB_VERSION" ;;
    help|-h|--help) cmd_help ;;
    *)              usage_err "未知命令: $cli_cmd（nexhub help 查看用法）" ;;
  esac
}

# 安装模式（curl -fsSL .../cli.sh | sh）：重新下载并写入可执行路径。
install_main() {
  case ":${PATH}:" in
    *":${HOME}/.local/bin:"*)
      inst_dir="${HOME}/.local/bin"
      ;;
    *)
      if [ -w /usr/local/bin ]; then
        inst_dir=/usr/local/bin
      else
        die "无安装权限。用 sudo 执行:
  sudo sh -c 'curl -fsSL $NEXHUB_CLI_URL -o /usr/local/bin/nexhub && chmod 755 /usr/local/bin/nexhub'
（或先把 ~/.local/bin 加入 PATH 后重试）"
      fi
      ;;
  esac
  mkdir -p "$inst_dir" || die "创建安装目录失败: $inst_dir"
  inst_dest="$inst_dir/nexhub"
  inst_tmp="$inst_dest.new.$$"
  log "安装 nexhub v$NEXHUB_VERSION → $inst_dest"
  if ! curl -fsSL "$NEXHUB_CLI_URL" -o "$inst_tmp"; then
    rm -f "$inst_tmp"
    die "下载失败: $NEXHUB_CLI_URL"
  fi
  if nh_have bash && ! bash -n "$inst_tmp" 2>/dev/null; then
    rm -f "$inst_tmp"
    die '下载内容语法校验失败（bash -n），已中止'
  fi
  chmod 755 "$inst_tmp"
  mv -f "$inst_tmp" "$inst_dest" || {
    rm -f "$inst_tmp"
    die "写入 $inst_dest 失败（权限不足？）"
  }
  case ":${PATH}:" in
    *":$inst_dir:"*) : ;;
    *) warn "$inst_dir 不在 PATH 中——请加入 PATH（如写入 ~/.bashrc）" ;;
  esac
  printf '\n下一步:\n'
  printf '  nexhub login <token>            # 或 nexhub login <节点URL> <token>\n'
  printf '  nexhub ping                     # 验证连通与 token\n'
  printf '  nexhub repo list                # 开始使用\n'
}

main() {
  case "${1:-}" in
    login|whoami|ping|repo|clone|apps|self-update|version|--version|help|-h|--help)
      cli_main "$@"
      ;;
    '')
      if [ "$(basename "$0")" = "nexhub" ]; then
        cli_main
      else
        install_main
      fi
      ;;
    *)
      if [ "$(basename "$0")" = "nexhub" ]; then
        cli_main "$@"
      else
        # curl | sh 场景携带任何参数 → 仍走安装
        install_main
      fi
      ;;
  esac
}

main "$@"
