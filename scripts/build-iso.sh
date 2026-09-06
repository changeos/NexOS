#!/usr/bin/env bash
# =============================================================================
# OS ISO 安装包一键构建脚本
# ============================
# 从源码构建可启动 OS ISO 安装包：
#   1. cargo build --release --workspace   （编译 4 个 binary: osd/os/os-api/os-mcp）
#   2. 组装 rootfs 目录树                  （binary + systemd unit + 系统骨架）
#   3. mksquashfs 打包 rootfs              （-comp zstd -b 256K -noappend）
#   4. 组装 ISO 源树 + 引导镜像占位         （casper/filesystem.squashfs + eltorito/efi 占位）
#   5. xorriso 生成 ISO                    （BIOS + UEFI 双启动 El Torito）
#   6. sha256sum 校验 + 产物信息
#
# 命令构造与 crates/os-iso/src/cli.rs 的 squashfs_pack_args /
# xorriso_build_args（纯函数）保持一致，本脚本是这些纯函数的 shell 端真实执行。
#
# 用法：
#   ./scripts/build-iso.sh            # 默认产物 target/iso/os-install.iso
#   make iso                          # 同上
#
# 依赖：cargo（Rust 1.97+）、xorriso、mksquashfs、sha256sum。
#   sudo apt install -y xorriso squashfs-tools
#
# 可启动性说明：
#   本脚本产出的 ISO 含 BIOS + UEFI 双 El Torito 引导记录（xorriso -boot-info-table
#   在占位 eltorito.img 上打信息表），故是合法的「可启动 ISO 结构」。但占位的
#   eltorito.img / efi.img 是零字节占位（无真实 GRUB 引导代码），实际从该 ISO
#   启动需要用真实 GRUB 引导镜像（grub-mkimage / grub-mkstandalone 产出）替换
#   boot/grub/i386-pc/eltorito.img 与 boot/efi.img。即：构建链路完整可用，
#   真实可启动需补 GRUB 引导镜像（标注于产物 manifest）。
# =============================================================================
set -euo pipefail

# ----------------------------- 配置（可环境变量覆盖）-----------------------------
# 仓库根（脚本所在目录的上一级）。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# 构建产物根。
TARGET_DIR="${TARGET_DIR:-${REPO_ROOT}/target}"
ISO_DIR="${ISO_DIR:-${TARGET_DIR}/iso}"
ISO_TREE="${ISO_TREE:-${ISO_DIR}/tree}"

# 产物文件名。
ROOTFS_SQUASHFS="${ISO_TREE}/casper/filesystem.squashfs"
ISO_OUT="${ISO_OUT:-${ISO_DIR}/os-install.iso}"
ISO_VOLUME="${ISO_VOLUME:-OS_INSTALL}"
MANIFEST="${ISO_DIR}/MANIFEST.txt"

# squashfs 压缩参数（呼应规格书 §3 + cli.rs SquashfsConfig）。
SQFS_COMP="${SQFS_COMP:-zstd}"        # zstd: 解压快，适合安装 ISO
SQFS_BLOCK="${SQFS_BLOCK:-256K}"      # 256 KiB 块（cli.rs 默认 1M，安装 ISO 用 256K 平衡）

# 引导镜像路径（相对 ISO 根；与 cli.rs BootConfig 默认一致）。
BIOS_BOOT_IMAGE="boot/grub/i386-pc/eltorito.img"
EFI_BOOT_IMAGE="boot/efi.img"

# binary 列表（workspace 4 个 [[bin]]）。
BINS=(osd os os-api os-mcp)
BIN_INSTALL_DIR="usr/local/bin"

# OS API 监听地址（systemd unit 自启 osd --serve-api）。
OS_SERVE_API_ADDR="${OS_SERVE_API_ADDR:-0.0.0.0:8080}"

# ----------------------------- 辅助函数 -----------------------------
log()  { printf '\033[1;34m[build-iso]\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m[build-iso]\033[0m \033[32m✓\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[build-iso]\033[0m \033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "缺少依赖命令: $1（安装: sudo apt install -y $2）"
}

section() {
    printf '\n\033[1;36m══════════════════════════════════════════════════════════════\033[0m\n' >&2
    printf '\033[1;36m %s\033[0m\n' "$*" >&2
    printf '\033[1;36m══════════════════════════════════════════════════════════════\033[0m\n' >&2
}

# ----------------------------- 前置检查 -----------------------------
require_cmd cargo    ""
require_cmd xorriso  xorriso
require_cmd mksquashfs squashfs-tools
require_cmd sha256sum coreutils

[ -f "${REPO_ROOT}/Cargo.toml" ] || die "未在仓库根找到 Cargo.toml: ${REPO_ROOT}"

# 清理旧产物（幂等）
rm -rf "${ISO_TREE}"
mkdir -p "${ISO_DIR}" "${ISO_TREE}/casper" "${ISO_TREE}/boot/grub/i386-pc" "${ISO_TREE}/EFI/BOOT"

ROOTFS="$(mktemp -d /tmp/os-rootfs-XXXX)"
trap 'rm -rf "${ROOTFS}"' EXIT

# =============================================================================
# 阶段 0：构建 Vue3 前端（rust-embed 编译期内嵌所需）
# =============================================================================
# crates/os-api/web/ 的 `npm run build` 产物输出到 crates/os-api/static-dist/，
# 由 os-api 的 rust-embed（#[folder = "static-dist/"]）在编译期内嵌。
# 故必须在 cargo build 之前生成 static-dist/，否则 rust-embed 编译失败。
section "阶段 0/5：构建 Vue3 前端（crates/os-api/web → static-dist/）"

WEB_DIR="${REPO_ROOT}/crates/os-api/web"
if [ -d "${WEB_DIR}" ]; then
    require_cmd npm "npm"
    log "npm install + npm run build（${WEB_DIR}）"
    (
        cd "${WEB_DIR}"
        npm install
        npm run build
    )
    [ -f "${REPO_ROOT}/crates/os-api/static-dist/index.html" ] \
        || die "Vue3 构建未产出 index.html（crates/os-api/static-dist/）"
    ok "Vue3 前端产物已就绪：crates/os-api/static-dist/"
else
    log "未找到 ${WEB_DIR}，跳过前端构建（os-api 将仅内嵌旧版 static/）"
fi

# =============================================================================
# 阶段 1：cargo build --release --workspace
# =============================================================================
section "阶段 1/5：cargo build --release --workspace"

log "编译 4 个 binary: ${BINS[*]}"
(
    cd "${REPO_ROOT}"
    cargo build --release --workspace
)

# 校验 4 个 binary 均产出
for b in "${BINS[@]}"; do
    bin_path="${TARGET_DIR}/release/${b}"
    [ -x "${bin_path}" ] || die "binary 未产出: ${bin_path}"
done
ok "4 个 binary 编译完成: ${BINS[*]}"

# =============================================================================
# 阶段 2：组装 rootfs 目录树
# =============================================================================
section "阶段 2/5：组装 rootfs 目录树"

log "rootfs 临时目录: ${ROOTFS}"

# 目录骨架（标准 Linux FHS + OS 自有目录）
mkdir -p "${ROOTFS}/"{\
usr/local/bin,\
etc/systemd/system,\
etc/os,\
boot/grub,\
var/lib/os,\
var/log/os,\
run/os,\
opt/os}

# 2.1 安装 4 个 binary 到 /usr/local/bin
for b in "${BINS[@]}"; do
    install -m 0755 "${TARGET_DIR}/release/${b}" "${ROOTFS}/${BIN_INSTALL_DIR}/${b}"
done
ok "binary 已安装到 rootfs/${BIN_INSTALL_DIR}/"

# 2.2 systemd unit：osd --serve-api 自启（生产 PID1 后编排 + 内嵌 HTTP 网关）
cat > "${ROOTFS}/etc/systemd/system/osd.service" <<UNIT
[Unit]
Description=OS System Orchestrator (osd --serve-api)
Documentation=https://github.com/os/os-system
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/osd --serve-api ${OS_SERVE_API_ADDR}
Restart=on-failure
RestartSec=3
# 资源限制（cgroup v2 由 osd 内部管理；此处仅 systemd 层兜底）
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
ok "systemd unit: rootfs/etc/systemd/system/osd.service"

# 2.3 启用 osd.service（创建 symlink，systemd 读 ISO 后即可自启）
mkdir -p "${ROOTFS}/etc/systemd/system/multi-user.target.wants"
ln -sf ../osd.service "${ROOTFS}/etc/systemd/system/multi-user.target.wants/osd.service"

# 2.4 系统骨架文件
cat > "${ROOTFS}/etc/hostname" <<'EOF'
os
EOF

cat > "${ROOTFS}/etc/os-release" <<EOF
NAME="OS System"
VERSION="1.0"
ID=os
ID_LIKE=ubuntu
PRETTY_NAME="OS System 1.0 (based on Ubuntu 24.04)"
VERSION_ID="1.0"
HOME_URL="https://github.com/os/os-system"
SUPPORT_URL="https://github.com/os/os-system/issues"
EOF
ok "os-release: $(head -1 <"${ROOTFS}/etc/os-release")"

# fstab 骨架（squashfs 只读根 + tmpfs 覆盖层占位）
cat > "${ROOTFS}/etc/fstab" <<'EOF'
# OS System fstab（live-boot 占位，真实挂载由 initramfs/tools 完成）
# <device>              <mount>  <type>     <options>        <dump> <pass>
/dev/loop0              /        squashfs   ro,defaults       0      0
tmpfs                   /run     tmpfs      mode=0755,size=64M 0      0
tmpfs                   /var/lib/os tmpfs mode=0755,size=128M 0     0
EOF

# OS 配置骨架
cat > "${ROOTFS}/etc/os/config.json" <<'EOF'
{
  "version": "1.0",
  "api": { "listen": "0.0.0.0:8080" },
  "storage": { "root": "/var/lib/os" }
}
EOF

# rootfs 说明
cat > "${ROOTFS}/README.rootfs" <<'EOF'
OS System rootfs (squashfs source tree)
=========================================
This directory tree is packed into casper/filesystem.squashfs by mksquashfs,
then placed on the ISO. Contains:
  /usr/local/bin/{osd,os,os-api,os-mcp}   OS 4 binaries
  /etc/systemd/system/osd.service            systemd autostart unit
  /etc/{hostname,os-release,fstab}            system skeleton
  /etc/os/config.json                        OS config skeleton
  /var/lib/os, /var/log/os, /run/os        runtime dirs
EOF

ok "rootfs 组装完成: $(find "${ROOTFS}" -type f | wc -l) 个文件"

# =============================================================================
# 阶段 3：mksquashfs 打包 rootfs
# =============================================================================
section "阶段 3/5：mksquashfs 打包 rootfs"

log "源: ${ROOTFS}"
log "产物: ${ROOTFS_SQUASHFS}"
log "压缩: ${SQFS_COMP}, 块大小: ${SQFS_BLOCK}"

# mksquashfs 需要先建好产物目录
mkdir -p "$(dirname "${ROOTFS_SQUASHFS}")"

# mksquashfs 对某些权限场景需 root；普通用户能跑则直接跑，否则用 sudo。
if mksquashfs "${ROOTFS}" "${ROOTFS_SQUASHFS}" \
        -noappend -comp "${SQFS_COMP}" -b "${SQFS_BLOCK}" 2>/tmp/mksqfs.err; then
    :
else
    log "mksquashfs 直接执行失败（可能权限不足），改用 sudo 重试..."
    echo "${SUDO_PASSWORD:?set SUDO_PASSWORD}" | sudo -S mksquashfs "${ROOTFS}" "${ROOTFS_SQUASHFS}" \
        -noappend -comp "${SQFS_COMP}" -b "${SQFS_BLOCK}" \
        || { cat /tmp/mksqfs.err >&2; die "mksquashfs 失败（含 sudo 重试）"; }
fi

# 校验 squashfs 魔数（hsqs = little-endian squashfs magic）
[ -s "${ROOTFS_SQUASHFS}" ] || die "squashfs 产物为空"
sq_magic="$(head -c 4 "${ROOTFS_SQUASHFS}" | od -An -tx1 | tr -d ' \n')"
[ "${sq_magic}" = "68737173" ] || die "squashfs 魔数校验失败: 期望 hsqs(68737173) 实得 ${sq_magic}"
ok "rootfs.squashfs 打包成功: $(du -h "${ROOTFS_SQUASHFS}" | cut -f1) (magic hsqs ✓)"

# =============================================================================
# 阶段 4：组装 ISO 源树 + 引导镜像占位
# =============================================================================
section "阶段 4/5：组装 ISO 源树 + 引导镜像占位"

# 4.1 casper/ 已含 filesystem.squashfs（阶段 3 产物）

# 4.2 BIOS eltorito.img：4×512 = 2048 字节零占位
#     （xorriso -boot-info-table 会在镜像内打引导信息表，零占位足够触发 El Torito 记录）
dd if=/dev/zero of="${ISO_TREE}/${BIOS_BOOT_IMAGE}" bs=512 count=4 status=none
ok "BIOS 引导镜像占位: ${BIOS_BOOT_IMAGE} (2048 bytes)"

# 4.3 UEFI efi.img：1.44 MiB 零占位（xorriso 仅要求文件存在即可写 El Torito 记录）
#     若 mkfs.vfat 可用则建真实 FAT（提升 UEFI 兼容性）；否则零占位。
EFI_IMG_PATH="${ISO_TREE}/${EFI_BOOT_IMAGE}"
if command -v mkfs.vfat >/dev/null 2>&1; then
    dd if=/dev/zero of="${EFI_IMG_PATH}" bs=512 count=2880 status=none
    if mkfs.vfat -n EFI "${EFI_IMG_PATH}" >/dev/null 2>&1; then
        ok "UEFI 引导镜像: ${EFI_BOOT_IMAGE} (FAT 1.44MiB, mkfs.vfat)"
    else
        ok "UEFI 引导镜像: ${EFI_BOOT_IMAGE} (1.44MiB 零占位, mkfs.vfat 失败回退)"
    fi
else
    dd if=/dev/zero of="${EFI_IMG_PATH}" bs=512 count=2880 status=none
    ok "UEFI 引导镜像: ${EFI_BOOT_IMAGE} (1.44MiB 零占位, 无 mkfs.vfat)"
fi

# 4.4 EFI/BOOT 目录占位（消除 xorriso "no directory /EFI/BOOT" 警告）
echo "minimal EFI boot stub" > "${ISO_TREE}/EFI/BOOT/BOOTX64.EFI"

# 4.5 ISO 根 README
cat > "${ISO_TREE}/README.txt" <<'EOF'
OS System Install ISO
======================
Contents:
  /casper/filesystem.squashfs   OS rootfs (zstd-compressed)
  /boot/grub/i386-pc/eltorito.img  BIOS El Torito boot image (placeholder)
  /boot/efi.img                 UEFI El Torito boot image (placeholder)
  /EFI/BOOT/BOOTX64.EFI         EFI boot stub

Boot note:
  Boot images are zero-byte placeholders to exercise the full xorriso El Torito
  pipeline. For a truly bootable ISO, replace them with real GRUB images
  produced by `grub-mkimage` / `grub-mkstandalone`.
EOF

ok "ISO 源树组装完成: ${ISO_TREE}"

# =============================================================================
# 阶段 5：xorriso 生成 ISO（BIOS + UEFI 双启动 El Torito）
# =============================================================================
section "阶段 5/5：xorriso 生成 ISO"

log "源树: ${ISO_TREE}"
log "产物: ${ISO_OUT}"
log "卷标: ${ISO_VOLUME}"
log "启动模式: BIOS + UEFI 双 El Torito"

# 命令形态与 crates/os-iso/src/cli.rs::xorriso_build_args 一致：
#   -as mkisofs -r -V <vol> -J -joliet-long
#   -b <bios_img> -boot-info-table -boot-load-size 4 -no-emul-boot
#   -eltorito-alt-boot -e <efi_img> -no-emul-boot
#   -o <iso> <tree>
xorriso -as mkisofs \
    -r -V "${ISO_VOLUME}" -J -joliet-long \
    -b "${BIOS_BOOT_IMAGE}" \
    -boot-info-table \
    -boot-load-size 4 \
    -no-emul-boot \
    -eltorito-alt-boot \
    -e "${EFI_BOOT_IMAGE}" \
    -no-emul-boot \
    -o "${ISO_OUT}" \
    "${ISO_TREE}"

[ -s "${ISO_OUT}" ] || die "ISO 产物为空: ${ISO_OUT}"
ok "ISO 生成完成: ${ISO_OUT}"

# 校验 ISO9660 魔数（CD001 @ VD ID 偏移 0x8001）
iso_size=$(stat -c %s "${ISO_OUT}")
if [ "${iso_size}" -ge 32770 ]; then
    iso_magic="$(dd if="${ISO_OUT}" bs=1 skip=32769 count=5 2>/dev/null)"
    [ "${iso_magic}" = "CD001" ] \
        && ok "ISO9660 魔数校验通过 (CD001 @ 0x8001) ✓" \
        || log "警告: ISO9660 魔数未匹配 (实得 '${iso_magic}')"
fi

# =============================================================================
# 产物校验 + manifest
# =============================================================================
section "产物校验 + manifest"

ISO_SHA256="$(sha256sum "${ISO_OUT}" | awk '{print $1}')"

# 写 manifest（产物元数据，便于交付/追溯）
{
    echo "OS System Install ISO - Build Manifest"
    echo "======================================="
    echo "Build time : $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "Built on   : $(hostname) ($(uname -srm))"
    echo "Builder    : $(whoami)"
    echo "Repo root  : ${REPO_ROOT}"
    echo ""
    echo "Artifacts:"
    echo "  ISO       : ${ISO_OUT}"
    echo "  Size      : $(ls -lh "${ISO_OUT}" | awk '{print $5}') (${iso_size} bytes)"
    echo "  SHA256    : ${ISO_SHA256}"
    echo "  Volume ID : ${ISO_VOLUME}"
    echo "  Squashfs  : ${ROOTFS_SQUASHFS##*/} (${SQFS_COMP}, ${SQFS_BLOCK} block)"
    echo ""
    echo "Binaries included:"
    for b in "${BINS[@]}"; do
        printf '  /%s/%s\n' "${BIN_INSTALL_DIR}" "${b}"
    done
    echo ""
    echo "Boot configuration:"
    echo "  BIOS  : -b ${BIOS_BOOT_IMAGE} -boot-info-table -boot-load-size 4 -no-emul-boot"
    echo "  UEFI  : -eltorito-alt-boot -e ${EFI_BOOT_IMAGE} -no-emul-boot"
    echo ""
    echo "NOTE: Boot images are zero-byte placeholders. For a truly bootable ISO,"
    echo "      replace ${BIOS_BOOT_IMAGE} and ${EFI_BOOT_IMAGE} with real GRUB"
    echo "      images (grub-mkimage / grub-mkstandalone)."
    echo ""
    echo "Verify:"
    echo "  sha256sum -c <(echo '${ISO_SHA256}  ${ISO_OUT}')"
} > "${MANIFEST}"

ok "Manifest: ${MANIFEST}"

# =============================================================================
# 输出产物信息（终端可见）
# =============================================================================
section "构建完成"

ls -lh "${ISO_OUT}" >&2
echo "SHA256: ${ISO_SHA256}" >&2
echo "" >&2
cat "${MANIFEST}" >&2

echo ""
ok "OS ISO 安装包构建完成"
