# BLOCKCHAIN_NODES — 节点运行管理（geth / bitcoind 真实子进程）

> 组件：`blockchain_nodes`（`crates/os-api/src/handlers/blockchain_nodes.rs`，
> blockchain.rs 子模块，component=`blockchain`）
> 前端：区块链管理 → 「节点运行」Tab（`web/src/views/Blockchain.vue`，i18n 键前缀 `bcn.*`）
> 关联文档：`docs/BLOCKCHAIN.md`（docker-compose 编排视图，与本模块互补）

定位：把 ETH 主网/Sepolia/dev（geth）与 BTC 主网/testnet/regtest（bitcoind）
作为**本机真实子进程**管理——SQLite 持久化节点定义 + spawn 进程表 + 按实例
日志 + 状态修正 + **全节点空间预检**。与既有 `/api/v1/blockchain/nodes`
（docker-compose 编排）互不影响，前缀独立为 `/api/v1/blockchain/chain-nodes`。

---

## 1. 调研数据表（2026-09，诚实口径）

> 结论先行：**ETH 主网没有"轻量快速模式"**——geth LES 轻客户端在 PoS 后已
> 移除（`--syncmode light` 不再可用）。所谓"快速模式"= snap sync + 默认剪枝，
> 仍是全量下载验证，主网落盘 ~700GB。若只需 RPC 锚点而不落盘本地链，用远程
> RPC（本 OS 的 dApp 验真 fallback 已走该路线），不要硬凑本地"轻节点"。

### 1.1 Ethereum（geth）

| 网络 | 模式 | 生成旗标 | 预估落盘 | 同步时长 | 备注 |
|------|------|----------|----------|----------|------|
| mainnet | fast | `--syncmode snap --gcmode full` | **~700GB**（2025-26 实测口径 650-700GB，官方规划值 1TB SSD） | 1-3 天（NVMe+带宽） | LES 已移除；仅最近 ~128 个状态可查 |
| mainnet | full | `--syncmode snap --gcmode archive` | **~2.2TB**（v1.16+ path-based；旧 hash-based 14-16TB 不可行） | 数天 | 另需 40GB 空闲才能跑 prune |
| sepolia | fast | `--sepolia --syncmode snap --gcmode full` | **~120GB**（测试网 spam 波动大，会被推翻） | 数小时 | ethPandaOps 快照可缩至 <1h |
| sepolia | full | `--sepolia --gcmode archive` | ~300GB（估算） | 1 天+ | |
| dev | fast | `--dev` | <1GB | 秒级 | 本地 PoA，不连外网 |

**CL（共识客户端）硬要求**：mainnet/sepolia 为 post-merge PoS——geth 只跑执行层
（EL），没有共识客户端就停在 merge 高度、不跟链头。本模块如实标注
（presets 的 `requires_consensus_client: true` + 创建响应 warnings），**不代跑
CL**；推荐搭配（文档口径，用户自行安装运行）：

```bash
# Sepolia 示例（lighthouse checkpoint sync，EL 已由本模块拉起）
lighthouse bn --network sepolia \
  --execution-endpoint http://127.0.0.1:8555 \
  --checkpoint-sync-url https://sepolia.checkpoint.sigp.io \
  --datadir /tank/blockchain/sepolia-cl
# 主网：--network mainnet + https://mainnet.checkpoint.sigp.io（CL 另需 ~150-300GB）
```

### 1.2 Bitcoin（bitcoind）

| 网络 | 模式 | 生成配置 | 预估落盘 | 同步时长 | 备注 |
|------|------|----------|----------|----------|------|
| mainnet | fast | `prune=550` | **~12GB**（实测 9-11GB） | 6-24h | **仍全量下载+验证整条链**（网络流量≈700GB），旧块验证后即删；prune 与 txindex 互斥 |
| mainnet | full | 无 prune | **~750GB**（2026 现值 700-800GB，年增 50-80GB） | 1-3 天 | |
| mainnet | full+txindex | `txindex=1` | +~160GB ≈ **910GB** | 同上 | 任意历史交易查询 |
| testnet | fast | `testnet=1`+`prune=550` | ~12GB | 数小时 | |
| testnet | full | `testnet=1` | ~350GB（testnet3 历史长） | ~1 天 | |
| regtest | fast | `regtest=1` | <1GB | 秒级 | 本地回归网（冒烟/开发） |

### 1.3 106 主机容量核对（2026-09-03）

`df /tank`：**965,645,041,664B 总量 / 944,109,912,064B 可用（≈880GiB）**。
推论（诚实）：
- BTC full（750GB）单独可放，但余量告急（<15%），**不建议**；
- ETH mainnet fast（700GB）+ BTC full（750GB）**同时放不下**——用户要的
  "eth 主网 + 常用测试网 + btc 主网"三节点组合，在 106 上只有全 fast 模式
  （700+120+12 ≈ 832GB，仍需清理空间）或"BTC fast + ETH sepolia fast"最稳。
  预检 UI 会按实时 df 阻断/警示。

## 2. 端点契约（9 条，前缀 `/api/v1/blockchain/chain-nodes`，component=blockchain）

| method | path | 鉴权 | 语义 |
|--------|------|------|------|
| GET | `` | 公开 | 列节点（数组；读取时做状态修正：running/syncing 但进程已死 → stopped+error） |
| POST | `` | admin | 创建（见下；full 空间不足 → **409**） |
| GET | `/presets` | 公开 | `{presets:[...], size_hint_source}`：6 网络 × 模式（预估体积/时长/诚实备注/CL 要求/二进制探测/安装指引） |
| GET | `/space-check` | 公开 | `?kind=&network=&mode=&data_dir=&txindex=` → `SpaceCheck`（required/available/sufficient/blocking/filesystem） |
| GET | `/:id` | 公开 | `{node, binary_installed, binary_path, rpc_url, log_path}` |
| POST | `/:id/start` | admin | 真实 spawn；**409**=二进制缺失（附安装指引）/空间不足；**502**=spawn 失败（落 error 态） |
| POST | `/:id/stop` | admin | SIGTERM → 10s → SIGKILL；收尸后 stopped |
| DELETE | `/:id` | admin | 先尽力停止再删行；**链数据目录不自动删**（响应 `data_dir_kept:true`） |
| GET | `/:id/logs` | 公开 | `?tail=200`（上限 2000）→ `{lines[], log_path, size_bytes, status}` |

POST 创建体：`{name, kind(ethereum|bitcoin), network, mode(fast|full), client?,
data_dir?, rpc_port?, p2p_port?, txindex?, extra_flags?}`；默认
`data_dir=<数据根>/<network>`（数据根解析见 §5）、client=geth/bitcoind、端口按预设
（eth 8545/8555/8546，btc 8332/18332/18443；P2P 30303/30304/30305、8333/18333/18444）。
响应 `{node, space, binary_installed, install_hint, rpc_url,
requires_consensus_client, warnings[]}`。

错误码：400 入参非法（txindex×prune 互斥、相对路径、未知 kind/network）；
409 full 空间不足（创建与启动双重拦截）；404 不存在。

## 3. 模式语义与命令生成

- **ETH fast** = `--syncmode snap --gcmode full`（geth 默认剪枝——最近 128
  状态 + 全部近期数据）；**ETH full** = `--gcmode archive`（全历史状态）。
- **BTC fast** = conf `prune=550`；**BTC full** = 无 prune，可选 `txindex=1`。
- bitcoind 不用旗标传网络/mode，而是生成 `datadir/nexos-bitcoin.conf`
  （`regtest=1`/`testnet=1`/`port=/rpcport=/rpcbind=127.0.0.1/rpcallowip=127.0.0.1/prune|txindex`），
  argv 仅 `-datadir=<dir> -conf=<dir>/nexos-bitcoin.conf`（**前台**运行，
  由本模块作为子进程托管；RPC 走 datadir 内 cookie 认证）。
- geth HTTP RPC 绑 `127.0.0.1`（不对外暴露）；`--http.api eth,net,web3,txpool`。
- `extra_flags`：空白拆分追加到命令尾（geth 同名旗标后者生效可覆盖默认；
  argv 数组直传，无 shell 注入面）。

## 4. 空间预检（用户点名实现）

- 预估体积表内置（`builtin_size_hints()`，键 `"<kind>/<network>/<mode>"`，
  值 GB），**env `NEXOS_CHAIN_NODE_SIZE_HINTS`** JSON 整体覆盖：
  `{"ethereum/mainnet/fast": 650, "bitcoin/mainnet/full": 730}`。
- 探测：`df -B1 <datadir>`（monitor.rs `read_root_disk` 同款口径，第 4 列
  available），datadir 不存在先 `create_dir_all`。
- 判定：`required = 预估GB × 1024³ (+ txindex 160GB)`；`full` 模式
  **不足即 409 阻断**（创建+启动两处），`fast` 模式只在响应带 warning。
- 前端创建向导 250ms 防抖实时调 `/space-check`：full 不足 → 红框 + 禁用
  「创建」按钮；fast → 黄色提示不阻断。

## 5. 生命周期与进程语义

- **持久化**：SQLite `blockchain_nodes` 表（env `NEXOS_CHAIN_NODES_DB`，缺省
  `/tank/os-data/chain-nodes.db`；/tank 缺失回退 `./chain-nodes.db`）。
- **数据根目录**：默认 datadir = `<root>/<network>`；`root` 解析顺序 =
  env `NEXOS_CHAIN_NODE_DATA_ROOT` → `/tank/blockchain`（可创建/可写时）→
  `/tank/os-data/blockchain`（**106 实测**：/tank 为 root 属主，os-api 服务
  用户 oem 建不了 /tank/blockchain；/tank/os-data 同 tank 文件系统、oem 可写，
  故 106 上实际落到这里）→ `./blockchain-nodes-data`。presets 响应带
  `default_data_root`，前端占位符同源。
- **进程表**：`HashMap<id, NodeProcess{pid, log_path, argv, Child}>`；
  Child 句柄 `Arc<Mutex<Option<Child>>>`——监测任务 `try_wait` 收尸（防僵尸：
  /proc 对僵尸恒存在，不收尸会把"已死"误判为"存活"）；stop 走
  SIGTERM → take+wait（10s 超时）→ SIGKILL+wait。
- **状态机**：`stopped →（start）syncing|running →（stop）stopped；异常退出
  → error`（附日志尾部）。syncing→running 修正：geth 经 `eth_syncing ==
  false`，bitcoind 经 RPC 端口 TCP 探活（spawn 后 30s 监测窗口内）。
- **重启恢复**：进程表重启即空；首次读取时状态修正把 running 行收敛为
  stopped + "进程已退出（服务重启或异常终止）"。**不自动恢复运行态**
  （llm_instances 同款；几百 GB 的主网同步由用户经 UI 重新点启动）。
- **日志**：`<log_dir>/<id>.log`（env `NEXOS_CHAIN_NODE_LOG_DIR`，缺省
  `/tank/os-data/chain-node-logs`），stdout+stderr 追加；每次启动写一行
  `===== nexos start <时间> client/network/mode =====` 锚点。
- **绝不自动启动主网同步**：创建只落记录；真实同步（几百 GB/数天）只在
  用户点击「启动」后发生。

## 6. 客户端安装指引（不在节点内自动 apt）

二进制解析顺序：env `NEXOS_CHAIN_NODE_BIN_GETH` / `NEXOS_CHAIN_NODE_BIN_BITCOIND`
→ PATH 扫描（纯文件系统探测）→ `/usr/local/bin`、`/usr/bin`、`~/.local/bin`、
`~/bin`。缺失时 start 返回 409 + 下列指引（前端同步展示）：

**geth**
```bash
# x86_64（PPA 仅 amd64）
sudo add-apt-repository -y ppa:ethereum/ethereum && sudo apt update && sudo apt install ethereum
# aarch64（PPA 无 arm64——官方 tar.gz；Spark 等 arm 机器用这条）
#   https://geth.ethereum.org/downloads → geth-linux-arm64-<ver>.tar.gz
tar -xzf geth-linux-arm64-*.tar.gz && sudo install -m 0755 geth-*/geth /usr/local/bin/
```

**bitcoind**
```bash
# x86_64（Ubuntu universe 版本较旧；PPA 更新）
sudo apt install bitcoind
sudo add-apt-repository -y ppa:bitcoin/bitcoin && sudo apt update && sudo apt install bitcoind
# aarch64（PPA 无 arm64——bitcoincore.org 官方 tar.gz）
#   https://bitcoincore.org/en/download/ → bitcoin-<ver>-aarch64-linux-gnu.tar.gz
tar -xzf bitcoin-*-aarch64-linux-gnu.tar.gz && sudo install -m 0755 -t /usr/local/bin bitcoin-*/bin/*
```

安装后点「重新探测二进制」或用 env 指定路径即可，**无需重启 os-api**。

## 7. 一键流程：用户经 UI 起三个节点（主网同步由用户触发）

1. 区块链管理 → 「节点运行」Tab → 确认预设区 geth/bitcoind 徽章为
   「已安装」（未装先按 §6 装好）；
2. 「+ 创建节点」×3（创建只落记录 + 空间预检，**不开始同步**）：
   - `ETH 主网`：kind=ethereum / network=mainnet / mode=fast（≈700GB，
     空间预检红框时先清理 /tank 或改 sepolia）；
   - `Sepolia`：kind=ethereum / network=sepolia / mode=fast（≈120GB）；
   - `BTC 主网`：kind=bitcoin / network=mainnet / mode=fast（prune=550，
     ≈12GB；要历史交易查询选 full+txindex，注意 ≈910GB）；
3. 在节点卡上逐个点「启动」→ 状态 syncing（BTC fast 6-24h、Sepolia 数小时、
   ETH 主网 1-3 天；「日志」按钮看进度）；
4. ETH 两节点如需跟链头/出块数据，按 §1.1 另起 lighthouse（本模块不管 CL）；
5. 删除节点只删记录——几百 GB 链数据目录手动清理（响应里有路径）。

## 8. 实测冒烟记录（106，x86_64，2026-09-03）

> 门禁红线：绝不在调研/测试中触发主网或 Sepolia 实际同步；冒烟只用
> `geth --dev`（本地 PoA，秒级）与 `bitcoind` regtest（本地链，秒级）。

### 8.1 二进制安装（用户态，无 sudo）

- **geth v1.17.5-stable**（9621c6ad）：官方 gethstore tar.gz（23.5MB，
  MD5 `925de60686b5eaa15d5a21acfc878ecf` 校验通过）→ `~/.local/bin/geth`
  （模块探测路径之一，免 sudo，服务重启即可见）。
- **bitcoind v31.1.0**：bitcoincore.org x86_64 tar.gz（90MB）→
  `~/.local/bin/bitcoind`。
- aarch64 机器（如 Spark）走 §6 的 arm64 tar.gz 路线（PPA 均无 arm64）。

### 8.2 CLI 冒烟（真实二进制直跑）

- **geth --dev**（datadir /tmp，RPC 18546）：8s 起来；
  `eth_blockNumber → 0x0`、`eth_chainId → 0x539`（1337 dev 链）；
  SIGTERM 优雅退出（日志 "Blockchain stopped"）。
- **bitcoind regtest**（conf 由本模块 `build_bitcoin_conf` 同款生成）：
  `getblockchaininfo → chain=regtest, blocks=0`（regtest 创世块
  0f9188f1…，cookie 认证 RPC 18443）；DNS seed 为 `dummySeed.invalid`
  （regtest 隔离，0 外连）；SIGTERM "Shutdown done"；datadir 共 17MB。

### 8.3 模块端到端冒烟（经 handler 生命周期：创建→start→logs→stop→delete）

`SMOKE_GETH_BIN=/home/oem/.local/bin/geth cargo test -p os-api --lib smoke_geth` →
0.52s 通过（spawn 真实 geth `--dev`，监测经 `eth_syncing=false` 把 syncing 修正
running，日志非空，stop 后 /proc 消失=已收尸）。
`SMOKE_BITCOIND_BIN=… cargo test -p os-api --lib smoke_bitcoind` → 2.05s 通过
（含 spawn 后 2s 存活断言 + 日志含 bitcoind 输出 + 干净停止）。CI 未设 env
时自动跳过（打印 skip 说明）。

### 8.4 实测抓出的三个真 bug（已修，测试固化）

1. **bitcoind conf 网络节语义**（两轮教训）：网络激活时 `port/rpcport/rpcbind`
   放 conf 顶层 → bitcoind 拒绝启动（"only applied on regtest network when
   in [regtest] section"）；反之 `regtest=1` 写进 `[regtest]` 节内**不激活
   网络**——bitcoind 静默以 **mainnet** 启动并连上主网 DNS seed（冒烟中
   ~10 秒内 kill 并删除该 datadir，仅预同步了区块头、无区块落盘）。正确
   形态=`<net>=1` 顶层激活 + 网络设置进 `[<net>]` 节（`build_bitcoin_conf`
   与单测均已固化）。
2. **僵尸进程误判**：tokio Child 不收尸时 /proc/<pid> 恒存在，"进程已退出"
   会被判成存活（状态修正失效）。已改：Child 句柄入进程表，监测任务
   `try_wait` 收尸、stop 走 take+wait。
3. **非法 mode 静默矫正**：`mode=light` 曾被静默当 fast（此前被 /tank 权限
   报错掩盖）；默认数据根目录修好后测试立刻抓出，改为显式 400。


## 9. 与 docker 编排视图（/api/v1/blockchain/nodes）的关系

| | chain-nodes（本模块） | nodes（既有） |
|---|---|---|
| 运行方式 | 本机二进制子进程 | docker compose up -d |
| 客户端 | geth / bitcoind | geth/reth/erigon/ganache/anvil/op-geth/arbitrum |
| fast/full 语义 | snap+prune / archive、prune=550 / 无prune(+txindex) | snap/full/archive 字符串透传 |
| 空间预检 | 有（409 阻断） | 无 |
| 持久化 | SQLite（重启保留） | 内存态（重启丢） |
| 日志 | 按实例文件 + tail 端点 | 无 |

后续可把两者在 UI 层合并展示；契约层面互不侵入。

---

## 10. BSC 链上验真接入（v0.1.47，2026-09-18，公共 RPC 路线）

> 组件：`crates/os-nexhub/src/chain_verify.rs`（`fallback_rpc_for(56)` RPC 兜底表
> + `chain_info` 链目录）；真机验收 `crates/os-nexhub/tests/chain_verify_live.rs`
> （`bsc_real_tx_verify`，`--ignored` 手动门禁）
> 关联文档：`docs/GATEWAY_MONETIZATION.md`（支付验真）、`docs/NEXHUB_LOBBY_DESIGN.md` §10

定位：BNB Smart Chain 主网（chainId=56）接入 **dApp 链上支付验真**——走与
ETH 主网/Sepolia 完全相同的 EVM 通用核验路径（`eth_chainId` 对链 →
`eth_getTransactionByHash` → `eth_getTransactionReceipt`；native + ERC-20
Transfer 日志 + AmountRule 均适用），**核验代码零改动**，本批只新增 RPC
兜底表与链目录条目（`{56, "BNB Smart Chain", BNB, https://bscscan.com}`）。

### 10.1 RPC 兜底表与国内实测延迟（106 直连，curl POST eth_blockNumber×2 + eth_chainId×1 计时）

| 端点 | 实测延迟（秒，3 次） | 均值 | 结论 |
|------|----------------------|------|------|
| `https://bsc-rpc.publicnode.com` | 0.617 / 0.606 / 0.580 | **0.60** | 入表（首位） |
| `https://bsc-dataseed2.bnbchain.org` | 0.718 / 0.685 / 0.667 | 0.69 | 入表 |
| `https://bsc-dataseed.bnbchain.org` | 0.784 / 0.657 / 0.681 | 0.71 | 入表 |
| `https://bsc-dataseed1.bnbchain.org` | 0.680 / 0.881 / 0.681 | 0.75 | 入表 |
| `https://bsc-dataseed3.bnbchain.org` | 0.713 / 0.882 / 0.895 | 0.83 | 入表 |
| `https://bsc-dataseed4.bnbchain.org` | 0.708 / 1.393 / 0.884 | 1.00 | 入表 |
| `https://1rpc.io/bnb` | 0.770 / 1.477 / 0.877 | 1.12 | 入表 |
| `https://rpc.ankr.com/bsc` | — | — | **剔除**：JSON-RPC -32000 要 API key（与主网 ankr 同款教训） |
| `https://bsc.blockpi.network/v1/rpc/public` | — | — | **剔除**：HTTP 521（停服，同主网 llamarpc 形状） |
| `https://bsc-public.nodereal.io` | — | — | **剔除**：DNS 域名不存在 |

- 全部入表端点 `eth_chainId` 均回 `0x38`（=56），表按实测均值升序（=failover 优先序）。
- dataseed 系是 BNB 官方 geo-DNS 同一集群的 5 个入口；publicnode 最快最稳，
  但真机验真中观测到**偶发 HTTP 403**（对程序化 UA/限流敏感）——failover 语义
  恰好兜住（实测第一笔即由 dataseed2 接住完成核验），无需人工干预。
- 换血纪律与 v0.1.8 ETH 批同源：**必须国内可达性实测筛过才入表**，剔除原因
  如实记录；生产环境用 `NEXOS_CHAIN_RPC_URLS` 自配（如
  `{"56": ["https://你的节点"]}`）。

### 10.2 确认数语义（与 ETH 主网/Sepolia 条目同构，无 per-chain 字段）

验真层**没有**按块数校准的确认数阈值：上块即查 receipt，`status==0x1` 即
Verified、无 receipt 即 Pending，不跟踪 `latest - blockNumber` 块差（ETH 主网
与 Sepolia 条目同样如此，故 BSC 不加 confirmations 字段）。该口径在 BSC 上
**更安全而非更弱**：

- BSC 2025 年 Maxwell 升级后出块 **0.75s**（本批探测窗口实证：8.45s 内链头
  推进 12 块 ≈0.7s/块），且带快速终结（BEP-520，~2 块 ≈1.5s 终结）；
- ETH 主网 PoS 需 ~2 epoch（≈13 分钟）才达经济终结——现行口径在 ETH 侧
  接受的回滚窗口本就远大于 BSC，沿用即安全边界不回退。

### 10.3 接线要点（env/前端零新增面）

- **币种与金额**：BSC 支付 = 网关 currency `evm`（BNB 同为 **18 位小数**，
  `to_wei_str` 的 18 位假设成立）+ body `chain_id: 56`（confirm 端点已支持
  显式 chain_id）或 env `NEXOS_EVM_CHAIN_ID=56` 作节点缺省。
- **收款地址**：共用 `NEXOS_PAY_EVM_ADDR`——EVM 地址格式（0x+40hex）全
  EVM 链通用，**不新增** `NEXOS_PAY_BSC_ADDR`（保持 env 面不膨胀；若未来
  运营者要在不同链收不同地址，再按需扩展）。
- **前端**：充值币种枚举 label 已更新为「EVM（ETH/BSC 系）」，单位提示
  「wei（1 ETH / 1 BNB = 1e18 wei）」——币种是链无关的 `evm`，链由
  chain_id 决定，无独立链选择枚举可加。
- **链目录**：`chain_verify::chain_info(56)` 提供 name/symbol/explorer
  （bscscan.com）展示元数据；`{chain_id, name, symbol, explorer}` 与 ETH
  主网条目同构。

### 10.4 真机验真记录（2026-09-18，106，国内直连）

fixture：区块 122,530,968 真实 native BNB 转账 1.351034742893322440 BNB——
`0x633d81ea…d0e9` → `0xa1b6183e…570e`。跑
`cargo test -p os-nexhub --test chain_verify_live bsc -- --ignored --nocapture`
三例断言全过：**Verified{block:122530968}** / Mismatch-to（改错收款方被拦，
actual 回报链上地址）/ Mismatch-value（改错金额被拦）；7 端点全表探测 7/7
健康。env 覆盖（`NEXOS_LIVE_TX_BSC` / `NEXOS_LIVE_RPC_BSC`）可换任意交易/端点复验。

### 10.5 与本地节点路线的关系

BSC 验真走**公共 RPC fallback 路线**（与 ETH 主网验真同款），不是本模块
（chain-nodes）的本地节点路线：BSC 全节点（bsc 客户端）落盘量级 2TB+ 且
同样有同步周期问题，106 容量（§1.3，≈880GiB 可用）放不下，短期不纳入
节点运行预设。若未来要自建 BSC 节点替代公共 RPC：在 `NEXOS_CHAIN_RPC_URLS`
配 `{"56": "<自建端点>"}` 即可整体前置，验真路径无需任何代码改动——这正是
fallback 表机制的既定替换点。
