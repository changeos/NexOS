# 方案 B：QEMU 嵌套虚拟化沙箱（环境配置指引）

> 目的：在受控宿主上用 QEMU/KVM 跑一个完整 Ubuntu 26.04 VM，作为"真实裸机环境"
> 跑那些动块设备 / bootloader / 真 KVM 域 / 嵌套虚拟化的高危集成测。
> 详见 `docs/SANDBOX.md` §3。
>
> **何时用 QEMU 而非 Docker**：
> - `os-compute` 真实 KVM 域（`virt-ffi` 全路径，`virConnectOpen("qemu:///system")`
>   → 真实 KVM 域生命周期）—— 容器内跑不了真 KVM。
> - `os-iso` 真实 install（分区/建 ZFS 池/首启钩子）—— 动块设备。
> - `os-provision` PXE 自举 + 阶段化迁移 —— 需真实网络引导。
> - `os-update` A/B 槽 activate + 首启 —— 动 bootloader + 重启验槽位切换。
>
> 这些路径 Docker privileged 容器**跑不了或不安全**（动宿主 bootloader/磁盘）。

---

## 1. 宿主前置

### 1.1 硬件 + BIOS

- **CPU 虚拟化**：BIOS 开 Intel VT-x 或 AMD-V。
- **嵌套虚拟化**（VM 内再跑 KVM，用于 os-compute 真实域测）：
  ```sh
  # Intel
  cat /sys/module/kvm_intel/parameters/nested   # 应输出 Y 或 1
  # AMD
  cat /sys/module/kvm_amd/parameters/nested     # 应输出 1
  ```
  若不是 Y/1，开启：
  ```sh
  # Intel 临时开（持久化写 /etc/modprobe.d/kvm-intel.conf: options kvm-intel nested=1）
  sudo modprobe -r kvm_intel
  sudo modprobe kvm_intel nested=1
  ```

### 1.2 宿主软件包（Ubuntu 26.04 示例）

```sh
sudo apt update
sudo apt install -y \
    qemu-system-x86 \
    qemu-utils \
    qemu-kvm \
    libvirt-daemon-system \
    libvirt-clients \
    bridge-utils \
    virtinst \
    ovmf \
    cloud-image-utils \
    xorriso \
    squashfs-tools \
    dosfstools \
    whois            # mkpasswd（首启 root 密码哈希）
```

确认 QEMU 支持 KVM 加速：
```sh
qemu-system-x86_64 -accel help | grep kvm     # 应含 kvm
ls -l /dev/kvm                                 # 应存在且有权限
```

---

## 2. 准备 VM 镜像

### 2.1 拉 Ubuntu 26.04 cloud image

cloud image 自带 cloud-init，免手动装系统。

```sh
mkdir -p ~/os-sandbox-qemu && cd ~/os-sandbox-qemu

# Ubuntu 26.04 cloud image（qcow2）。若 26.04 镜像暂未发布，退回 25.04 测同样路径。
BASE_IMG=ubuntu-26.04-server-cloudimg-amd64.img
curl -fL -o "$BASE_IMG" \
    https://cloud-images.ubuntu.com/releases/26.04/release/ubuntu-26.04-server-cloudimg-amd64.img

# 复制成可写盘（保留 base 干净，便于复用）
cp "$BASE_IMG" os-sandbox.qcow2
qemu-img resize os-sandbox.qcow2 40G   # 给 ZFS 池 / 容器预留空间
```

### 2.2 cloud-init：注入 SSH key + 测试脚本

写 `user-data`（cloud-init 用户脚本）。要点：
- 装与 Docker 镜像相同的系统包（libvirt-dev / libnftnl-dev / ZFS / chrony）。
- 装 Rust stable + sccache。
- 克隆仓库（或从宿主 rsync 进 VM）。
- 跑 `scripts/sandbox/docker/run-tests.sh`（脚本与方案 A 共用）。

示例 `user-data`（最小骨架，按需扩）：

```yaml
#cloud-config
hostname: os-sandbox
users:
  - name: ci
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - ssh-ed25519 AAAA...   # 替换为你的公钥

packages:
  - build-essential
  - pkg-config
  - curl
  - ca-certificates
  - git
  - libvirt-dev
  - libvirt-clients
  - libnftnl-dev
  - libmnl-dev
  - nftables
  - zfsutils-linux
  - iproute2
  - chrony

runcmd:
  - curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,rustfmt
  - |
    su - ci -c '
      set -e
      source "$HOME/.cargo/env"
      cd "$HOME"
      # 从宿主 rsync 仓库（见 §2.3），或 git clone <repo>
      cd OS_System
      sudo -E ./scripts/sandbox/docker/run-tests.sh
    '
```

写 `meta-data`：
```yaml
instance-id: os-sandbox-001
local-hostname: os-sandbox
```

生成 seed 盘：
```sh
cloud-localds seed.img user-data meta-data
```

### 2.3 把仓库送进 VM（两种方式，二选一）

- **rsync（推荐，保留本地改动）**：VM 起来后从宿主推：
  ```sh
  rsync -az --exclude target --exclude .git \
      /home/oem/OS_System/OS_System/ ci@<VM_IP>:~/OS_System/
  ```
- **git clone（CI/复现）**：在 `user-data` 的 `runcmd` 里 clone 远端仓库。

---

## 3. 启动 VM

### 3.1 命令行（直跑 QEMU）

```sh
cd ~/os-sandbox-qemu

# 给 VM 挂一块额外的"空数据盘"（给 os-iso install / os-provision 分区 / ZFS 建池用）
qemu-img create -f qcow2 data-empty.qcow2 20G

qemu-system-x86_64 \
    -enable-kvm \
    -cpu host -smp 4 -m 4096 \
    -drive file=os-sandbox.qcow2,if=virtio,format=qcow2 \
    -drive file=seed.img,if=virtio,format=raw \
    -drive file=data-empty.qcow2,if=virtio,format=qcow2 \
    -netdev user,id=n0,hostfwd=tcp::2222-:22 \
    -device virtio-net-pci,netdev=n0 \
    -nographic
```

SSH 进 VM：
```sh
ssh -p 2222 ci@localhost
```

### 3.2 用 libvirt / virt-install（更易管理快照）

```sh
virt-install \
    --name os-sandbox \
    --ram 4096 --vcpus 4 \
    --os-variant ubuntu26.04 \
    --disk path=$(pwd)/os-sandbox.qcow2,bus=virtio \
    --disk path=$(pwd)/seed.img,device=cdrom \
    --network default \
    --graphics none \
    --import

# 快照（跑高危测前回滚点）
virsh snapshot-create-as os-sandbox pre-test
virsh snapshot-revert os-sandbox pre-test
```

> `--os-variant ubuntu26.04` 若 osinfo-db 太旧不识别，退回 `ubuntu25.04` 或
> `generic`。

---

## 4. 在 VM 内跑测试

VM 起来 + SSH 进去后（或经 cloud-init 自动跑），等同方案 A：

```sh
# VM 内（已是 root 或 sudo 提权）
cd ~/OS_System
sudo -E ./scripts/sandbox/docker/run-tests.sh

# 嵌套虚拟化测（os-compute 真实 KVM 域）需 SANDBOX_RUN_FFI=1 + 嵌套 KVM 已开
sudo -E SANDBOX_RUN_FFI=1 ./scripts/sandbox/docker/run-tests.sh
```

> **共享脚本说明**：`scripts/sandbox/docker/run-tests.sh` 在 Docker 与 QEMU 两种
> 沙箱里通用——它只做"环境探针 + 跑 cargo"，对运行载体无假设。

---

## 5. CI 集成建议（不在本任务实施）

QEMU 启动慢（分钟级）+ 嵌套虚拟化 runner 难寻，**不建议每 PR 跑**。建议：

- **夜间 cron**：完整 QEMU 回归（KVM / iso / provision / update 高危路径）。
- **发版前**：手动触发，跑 `SANDBOX_RUN_FFI=1` + iso/provision/update 真实 install。
- **runner 选型**：自建 bare-metal runner，或 GCP nested-vpc / AWS `.metal` 实例
  （GitHub-hosted runner 不支持嵌套 KVM）。
- 与 `.github/workflows/ci.yml`（每 PR 三道门）解耦，不阻断合并。

---

## 6. 常见坑

- **`/dev/kvm` 权限**：VM 内访问 `/dev/kvm` 需 `kvm` 组或 root；
  cloud-init 的 `ci` 用户已 `NOPASSWD:ALL`，`sudo -E` 即可。
- **cloud-init 不跑**：检查 `seed.img` 是否作为 cdrom 挂载；VM 内
  `cloud-init status --long` 看日志。
- **ZFS 模块未加载**：VM 内 `sudo modprobe zfs`（cloud image 通常已装 zfs-dkms）。
- **libvirtd 未起**：`sudo systemctl enable --now libvirtd`。
- **QEMU 报 "nested=0"**：见 §1.1，宿主须开嵌套虚拟化，否则 os-compute 真实 KVM
  域测失败（降级用 `test:///default` fixture 跑方案 A）。
- **网络**：`-netdev user,hostfwd=tcp::2222-:22` 是 NAT 模式，无公网入站；
  PXE 自举测需 `-netdev bridge` 或 TAP，按 os-provision 测试需求配。

---

## 7. 参考

- `docs/SANDBOX.md` §3（方案 B 总览 + crate 适用表）。
- Ubuntu cloud images: https://cloud-images.ubuntu.com/releases/26.04/release/
- cloud-init 示例: https://cloudinit.readthedocs.io/
- libvirt `test:///default` 驱动（无 KVM 也能跑 libvirt fixture）:
  https://libvirt.org/drvesc.html#test
