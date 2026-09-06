#!/bin/bash
# ============================================================
# NexOS 发版脚本——双架构构建 + 版本号 + tag + 推送
# 用法: ./scripts/release.sh <版本号>   例: ./scripts/release.sh 0.1.5
# 前置: gcc-aarch64-linux-gnu（交叉链接器）；远端 nexos-local 已配置
# ============================================================
set -euo pipefail
cd "$(dirname "$0")/.."

V="${1:?用法: $0 <版本号>}"
echo "=== NexOS 发版 $V ==="

# ① 版本号写入 workspace Cargo.toml
sed -i "70s/version = \".*\"/version = \"$V\"/" Cargo.toml
grep -q "^version = \"$V\"" Cargo.toml || { echo "版本写入失败"; exit 1; }

# ② 三道门（fmt / clippy 关键 crate / 测试抽样）
cargo fmt --all
cargo clippy -p os-api -p os-p2p --all-targets --features mock -- -D warnings
cargo test -p os-api --lib -- 2>&1 | tail -1

# ③ x86_64 构建（debug 供 106；release 供 113/aliyun）
cargo build -p os-api
cargo build --release -p os-api
echo "✓ x86_64: target/release/os-api"

# ④ aarch64 构建（DGX Spark / ARM 服务器）
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
cargo build --release --target aarch64-unknown-linux-gnu -p os-api -p os-p2p
file target/aarch64-unknown-linux-gnu/release/os-api | grep -q "ARM aarch64" || { echo "aarch64 产物校验失败"; exit 1; }
echo "✓ aarch64: target/aarch64-unknown-linux-gnu/release/{os-api,p2p-node}"

# ⑤ 提交 + tag + 推送（NexHub；GitHub 仅发版时同步——铁律）
git add Cargo.toml Cargo.lock
git commit --no-verify -m "chore(release): 版本 $V" || echo "(无版本变更可提交)"
git tag -fa "v$V" -m "NexOS $V"
git push nexos-local main --force-with-lease
git push nexos-local -f "v$V"

# ⑤b 刷新分发目录（/tank/os-data/dist——传输组件/Files API 的分发源）
mkdir -p /tank/os-data/dist
cp target/aarch64-unknown-linux-gnu/release/os-api /tank/os-data/dist/os-api-aarch64-latest
cp target/aarch64-unknown-linux-gnu/release/p2p-node /tank/os-data/dist/p2p-node-aarch64-latest
cp target/release/os-api /tank/os-data/dist/os-api-x86_64-latest

# ⑤c 同步分发件到公网安装源（aliyun——install.sh 的源节点，Files API 推送）
# 发版只刷 106 分发目录的话，公网源会 404（08-30 DGX Spark 实测踩过）
if curl -s --max-time 8 -o /dev/null http://203.0.113.2:8558/; then
  python3 - <<'PYEOF'
import base64, json, urllib.request
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
                     "Authorization": "Bearer change-me-admin-token"})
        urllib.request.urlopen(req, timeout=600)
        print(f"  aliyun 同步: {name}")
    except Exception as e:
        print(f"  aliyun 同步失败 {name}: {e}")
PYEOF
else
  echo "  aliyun 不可达，跳过公网源同步"
fi

# ⑥ 可选：GitHub 同步（--github 开关，铁律=仅发版时）
if [[ "${1:-}" == "--github" || "${2:-}" == "--github" ]]; then
  GH_TOKEN=$(ssh -p 179 -o BatchMode=yes -o StrictHostKeyChecking=no root@198.51.100.114 'cat /root/.gh_token_changeos' | tr -d '\n\r ')
  git push "https://x-access-token:${GH_TOKEN}@github.com/changeos/NexOS.git" main --tags
fi

echo ""
echo "=== 发版完成 $V ==="
echo "产物："
echo "  x86_64  release: target/release/os-api"
echo "  aarch64 release: target/aarch64-unknown-linux-gnu/release/os-api"
echo "  aarch64 p2p-node: target/aarch64-unknown-linux-gnu/release/p2p-node"
echo "分发：113/aliyun 用 release x86_64；DGX Spark 等 ARM 用 aarch64；106 本机用 debug"
