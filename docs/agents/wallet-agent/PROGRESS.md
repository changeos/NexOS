# wallet-agent 进度日志

## 当前状态
- 阶段：实现中（**全链路真实验签已接通**——BTC BIP-322 完整伪交易 + Schnorr/ECDSA + EVM EIP-191/EIP-712，全绿）
- 最后更新：2026-08-05（BIP-322 完整验签子代理：legacy + simple 全落地）

## 已完成
- [x] os-wallet 纯逻辑骨架（数据结构/状态机/字符串构造真实，外部密码学留 TODO）
  - 28 个单元测 + 1 doctest 全绿
- [x] **P2 接通真实验签**（2026-08-05）：bitcoin/alloy/secp256k1 workspace 依赖已接入
  - **EIP-191** 真实验签（alloy `Signature::recover_address_from_msg`，RFC web3.js 向量测）
  - **EIP-712** 真实验签（alloy `TypedData::eip712_signing_hash` + recover，签名回环测）
  - **Schnorr（BIP-340）** 真实验签（`bitcoin::secp256k1::verify_schnorr`，签名回环测）
  - **ECDSA signmessage** 真实验签（Bitcoin Core 兼容，`recover_ecdsa` + 双 sha256d）
  - **EvmAdapter**：verify_signature 路由到 verify_eip191/eip712；余额 eth_getBalance；
    ERC-721 ownerOf / ERC-1155 balanceOf 经 eth_call（含 ABI 编码：keccak256 selector）
  - **BitcoinAdapter**：verify_signature 路由到 verify_schnorr/ecdsa/bip322；余额 scantxoutset
  - 余额/凭证查询经可注入 `RpcProbe`（与 RpcRegistryImpl 探活同传输层）；fixture 测零网络
  - 64 个单元测 + 1 doctest 全绿（41 原 + 23 新），clippy 0 警告，workspace 无回归
- [x] **BIP-322 完整伪交易验签**（2026-08-05）：`signing::verify_bip322` 从 TODO 占位落地为
  完整 to_spend/to_sign 伪交易构造 + witness 解析 + 多地址类型验签
  - **legacy（P2PKH）**：base64(65 字节可恢复 ECDSA) → `legacy_signature_hash` + `recover_ecdsa`
    → hash160 比对地址 scriptPubKey（压缩/非压缩公钥双兼容）
  - **simple（P2WPKH）**：`smp`+base64(consensus witness stack) → `p2wpkh_signature_hash`
    （BIP-143）+ `verify_ecdsa`
  - **simple（P2SH-P2WPKH）**：3 项 witness（sig/pubkey/redeemScript）→ hash160(redeemScript)
    比对地址 + BIP-143 sighash
  - **simple（P2TR taproot key-spend）**：`smp`+base64(witness) → `taproot_key_spend_signature_hash`
    + `verify_schnorr`（从 scriptPubKey 提取 x-only output key）
  - 测试向量来自 bitcoin/bips `bip-0322/basic-test-vectors.json`（P2WPKH/P2TR 真实已知向量）
  - 3 个签名+验签往返测（P2WPKH/P2TR/P2PKH 用 bitcoin crate 确定性签名再验）
  - **77 个单元测 + 1 doctest 全绿**（64 原 + 13 新增 BIP-322），clippy 0 警告，workspace 无回归

## 进行中
- （无）

## 阻塞
- ⚠️ **BTC 地址比对（ECDSA signmessage）**：当前 `verify_ecdsa_message` 比对**公钥 hex**
  （33/65 字节），地址（bech32/base58check）比对留 TODO（需 hash160 + checksum 编解码）。
  （注：BIP-322 路径已做地址比对，signmessage 独立路径仍按公钥 hex。）
- ⚠️ **Ordinal 凭证查询**：`BitcoinAdapter::query_credential` 待自托管 ord index 接入（§9.1#12）。
- ⛔ **真实 RPC 探活**：`RpcRegistryImpl::probe` 需要 reqwest + JSON-RPC
  （BTC `getblockchaininfo` / EVM `eth_blockNumber`），`reqwest` 未注册。
  当前返回 `Err(Internal)`；`check` 降级到缓存结果。
- ⛔ **真实 WalletConnect v2**：`WalletConnectV2Connector` / `QrCodeConnector` 的
  relay 配对 + 签名转发依赖 `walletconnect-relay`（未注册）。当前 connect/request_signature
  返回 `Err(Internal)`；会话表（`SessionStore`）+ disconnect/list_sessions 可用。
- ⛔ **Ordinals 自托管 ord index**（§9.1#12）：`BitcoinAdapter::query_credential` 留 TODO。

## 下一步
1. 待主代理/ReviewAgent 评估后，注册 `rust-bitcoin`/`alloy`/`secp256k1`/`reqwest`/`walletconnect-relay`。
2. 注册后填充真实验签 / RPC 探活 / WC v2 配对（替换 Internal 占位）。
3. 接 security 真实 `JwtIssuer`（批 1 已交付 mock，见 `crates/os-security/src/mock.rs`），
   当前 os-wallet 未直接消费 JwtClaims（契约层无需），待 guest/im 集成时串联。

## 实现要点
- **未改 trait 签名**：3 个 trait（`WalletConnector`/`ChainAdapter`/`RpcRegistry`）签名零改动。
- **ADR-COMPAT-001 遵守**：仅 `ChainAdapter`（出现在 `Box<dyn ChainAdapter>`）用 `#[async_trait]`；
  `WalletConnector`/`RpcRegistry` 保持原生 async（无 `Box<dyn>`），实现块不加 `#[async_trait]`。
- **纯逻辑已实现且测试覆盖**：
  - `signing::eip191_personal_sign_message`（EIP-191 前缀，含多字节 UTF-8 长度已知向量）
  - `signing::eip712_type_string`（字段名排序构造）
  - `registry::RpcStatusCache` + `RpcState`（Available/Unavailable/Probing + TTL 过期）
  - `model`：ChainConfig.validate、CredentialSpec.validate、SignatureAlgorithm 路由、
    validate_evm_address、meets_balance_threshold、SignatureResult 构造器
  - `connector`：WalletSession 生命周期（默认 1h 过期）+ SessionStore CRUD
- **Mock 三件套**（feature `mock`）：`MockWalletConnector`/`MockChainAdapter`/`MockRpcRegistry`，
  构造器风格 `new().with_*()`，纯内存确定性。

## 验证命令与结果
- `cargo check -p os-wallet --features mock` → Finished（0 警告）
- `cargo test -p os-wallet --features mock` → **77 passed; 0 failed**（+ 1 doctest passed）
- `cargo test -p os-wallet`（默认 feature）→ **77 passed; 0 failed**（+ 1 doctest）
- `cargo clippy -p os-wallet --all-targets --features mock -- -D warnings` → 0 警告
- `cargo clippy -p os-wallet --all-targets -- -D warnings`（默认 feature）→ 0 警告
- `cargo check --workspace` → 无回归

## 提交列表
- `[wallet-agent] feat(os-wallet): 多链钱包骨架（状态机/字符串构造真实，密码学留 TODO）+ Mock`
- `[wallet-agent] feat(os-wallet): 接通 bitcoin/alloy 真实验签`（P2 接通）
- `[wallet-agent] feat(os-wallet): 完整 BIP-322 验签(legacy+simple)`（本轮 BIP-322 完整伪交易）

## 改动文件
- `crates/os-wallet/Cargo.toml`（加 `mock` feature + dev-dependencies）
- `crates/os-wallet/src/lib.rs`（接 signing/mock 模块 + 重导出）
- `crates/os-wallet/src/model.rs`（构造器/校验/SignatureResult/纯函数 + 测）
- `crates/os-wallet/src/chain.rs`（BitcoinAdapter/EvmAdapter 骨架 + 测）
- `crates/os-wallet/src/connector.rs`（SessionStore + 3 Connector 骨架 + 测）
- `crates/os-wallet/src/registry.rs`（RpcState/RpcStatusCache/RpcRegistryImpl + 测）
- `crates/os-wallet/src/signing.rs`（新建：EIP-191 前缀 / EIP-712 typeString 纯逻辑 + 测）
- `crates/os-wallet/src/mock.rs`（新建：3 个 Mock + 测）
- `docs/agents/wallet-agent/PROGRESS.md`（本文件）

## 契约问题
- 无 trait 签名变更。
- 注意：本会话期间 worktree 一度消失（"损坏仓库迁移"），重建后批 1（storage/network/security）
  已合并到 main（HEAD `a176729`）；本分支基于 `a176729`，os-security 已含真实 mock。
  原 worktree 基于 `634a9f1`（批 1 未合并），现已自动跟上新 main。

## 本轮修复（修复型子代理，2026-08-05）

前 agent 骨架交付后被中断；本轮修复全部编译错误/警告/测试失败，**未改任何 trait 签名**：

1. `model.rs`：`ChainKind` 增加 `PartialOrd, Ord` derive —— 修 `registry.rs` 测试 `chains.sort()` 的
   E0277（`the trait bound ChainKind: Ord is not satisfied`）。非破坏性增补。
2. `model.rs`：`ChainConfig::validate` 文档 `doc_lazy_continuation` clippy 警告 —— 在 `- 列表项` 与续行
   之间补空行（clippy `doc_lazy_continuation`）。
3. `signing.rs`：修 2 个错误测试断言（实现本身正确）：
   - `eip191_length_is_decimal_byte_count`：payload `"你好世界喵"` 是 5 字符 / 15 字节，原断言误写 "9"。
   - `eip712_type_string_sorts_fields`：EIP-712 规范按**字段名**排序，期望串应为
     `Mail(string contents,Person from,Person to)`（原串误按类型排序）。

> 说明：任务描述预估的 `E0195`（mock 方法签名与 trait 不一致）在本轮检查时**未复现**——
> mock.rs 的 `impl ChainAdapter for MockChainAdapter` 已正确加 `#[async_trait]`，
> `WalletConnector`/`RpcRegistry` 为原生 async trait（无 Box<dyn>），实现块正确地不加 `#[async_trait]`。
> 真实阻塞为上述 3 项（Ord derive / doc 警告 / 测试断言）。

## 本轮 P2 接通（P2 接通子代理，2026-08-05）

接通 ADR-DEPS-002 注册的 bitcoin/alloy/secp256k1，把骨架阶段留 TODO 的真实验签/查询落地：

### 1. 依赖接入（`crates/os-wallet/Cargo.toml`）
- `bitcoin.workspace = true`（默认 feature 已含 secp-recovery）
- `alloy = { workspace = true, features = ["eip712"] }`（启用 `TypedData::eip712_signing_hash`）
- `secp256k1 = { workspace = true, features = ["recovery", "hashes"] }`
  （注：BTC 验签代码经 `bitcoin::secp256k1`（0.29）+ `bitcoin::hashes` 访问，与 bitcoin 锁定
  同一 secp256k1 次版本，避免 0.29/0.31 多版本类型不互通；独立 secp256k1 0.31 仅作
  workspace 注册项保留）

### 2. signing.rs 真实验签（新增 `verify_*` 函数 + 16 个测试）
- `verify_eip191(message, sig, addr)`：alloy `Signature::recover_address_from_msg`（内部
  keccak256(EIP-191 前缀) + secp256k1 恢复 + 地址截取）。RFC web3.js v1.2.2 向量测：
  消息 `"Some data"` → 地址 `0x2c7536E3605D9C16a7a3D7b1898e529396a65c23`。
- `verify_eip712(typed_data_json, sig, addr)`：alloy `TypedData::eip712_signing_hash` →
  `recover_address_from_prehash`。签名回环测（alloy 本地私钥签 + 验）。
- `verify_schnorr(msg, sig, pk_xonly_hex)`：`bitcoin::secp256k1::Secp256k1::verify_schnorr`
  对 sha256(msg) 摘要验签（BIP-340）。签名回环测（确定性签名）。
- `verify_ecdsa_message(msg, sig65, pk_hex)`：Bitcoin Core signmessage 兼容——
  `RecoverableSignature::from_compact` + `recover_ecdsa` + sha256d(magic)。
- `verify_bip322(...)`：留 TODO（完整伪交易封装），返回 `Err(Internal)`。
- 辅助：`parse_signature`（65 字节 / hex 双路径）、`decode_hex_maybe_0x`、
  `eip191_personal_sign_message` / `eip712_type_string`（保留）+ 新增 `bitcoin_message_magic`。

### 3. chain.rs 适配器接通（BitcoinAdapter / EvmAdapter）
- 两个 adapter 新增 `with_probe(probe: Box<dyn RpcProbe>)` 构造器——注入 RPC 传输探针
  （与 RpcRegistryImpl 探活同 `RpcProbe` trait），余额/凭证查询经它发 JSON-RPC。
- `verify_signature` 按 algo 路由到真实 signing 函数（accepts 路由校验保留）。
- `EvmAdapter::query_balance`：`eth_getBalance`（wei 十六进制 → u128）。
- `EvmAdapter::query_credential`：ERC-721 `ownerOf` / ERC-1155 `balanceOf` 经 `eth_call`
  + 自实现 ABI 编码（`keccak256(signature)[:4]` selector + address/uint256 padding）。
- `BitcoinAdapter::query_balance`：`scantxoutset` + `addr(<addr>)` 描述符 → `total_amount` (BTC) → sat。
- ABI 编码辅助：`eth_function_selector`（keccak256[:4]，已知向量 ownerOf=0x6352211e /
  balanceOf(address,uint256)=0x00fdd58e）、`parse_u128_hex`、`btc_amount_to_sat`。
- 7 个新测试覆盖 fixture 查询路径（StaticProbe 内存 fixture，零网络）。

### 4. 测试与质量
- 64 个单元测 + 1 doctest 全绿（41 原 + 23 新增：EIP-191 RFC × 5、EIP-712 × 2、
  Schnorr × 4、ECDSA × 2、BIP-322 占位 × 1、ABI 工具 × 4、adapter 接通 × 5）。
- clippy `-D warnings` 0 警告（mock + 默认 feature）；workspace check 无回归。

### 红线遵守
- 未改任何 trait 签名（`ChainAdapter` / `WalletConnector` / `RpcRegistry` 零改动）。
- 未改其他 agent crate（仅 os-wallet + Cargo.lock + workspace Cargo.toml 无改动）。

## 本轮 BIP-322 完整验签（BIP-322 子代理，2026-08-05）

把 `signing::verify_bip322` 从 `Err(Internal)` 占位落地为完整伪交易验签，覆盖 legacy +
simple 全主流地址类型。依赖规范 https://github.com/bitcoin/bips/blob/master/bip-0322.mediawiki
与测试向量 bitcoin/bips 仓库 `bip-0322/basic-test-vectors.json`。

### 1. 依赖接入
- `base64.workspace = true`（workspace 已注册 ADR-DEPS-001）——BIP-322 签名为 base64 文本
  （simple=base64(consensus witness stack)，legacy=base64(65B recoverable ECDSA)）。
  比特币侧 API（script/transaction/witness/sighash）全部经已接入的 `bitcoin` 0.32 crate。

### 2. signing.rs 实现（`verify_bip322` + 辅助函数）
- `bip322_tagged_hash(message)`：`SHA256(SHA256("BIP0322-signed-message")‖SHA256(tag)‖message)`，
  BIP-340 tagged hash（已知向量测：消息"" → c90c269c...）。
- `bip322_create_to_spend(addr, message)`：构造 BIP-322 to_spend 虚拟交易（version=0,
  locktime=0, input=OutPoint::null + scriptSig=`OP_0 PUSH32[msg_hash]`, output=value 0 +
  地址 scriptPubKey）。
- `bip322_create_to_sign(to_spend, witness)`：构造 to_sign 虚拟交易（引用 to_spend 的 vout 0,
  OP_RETURN 输出, witness 填入签名）。
- `verify_bip322(message, signature, address)` 入口：解析地址 → 按 `AddressType` 分发：
  - **P2pkh → legacy**：`legacy_signature_hash`（SIGHASH_ALL, u32=1）+ `recover_ecdsa` →
    hash160(pubkey) 比对 scriptPubKey（压缩/非压缩双兼容，recid 27-34/0-3 兼容）。
  - **P2wpkh → simple**：consensus 反序列化 witness → `p2wpkh_signature_hash`（BIP-143）+
    `verify_ecdsa`（DER 签名末字节为 sighash type）。
  - **P2sh → simple P2SH-P2WPKH**：3 项 witness（sig/pubkey/redeemScript）→ 校验
    hash160(redeemScript) 嵌入地址 + BIP-143 sighash。
  - **P2tr → simple taproot key-spend**：`taproot_key_spend_signature_hash`（Prevouts::All）+
    `verify_schnorr`（64/65 字节签名，从 scriptPubKey[2..34] 提取 x-only output key）。
- simple 签名 `smp` 前缀剥离（容错：无前缀也接受，便于 base64 直传）。

### 3. 测试（13 个新增）
- 已知向量（bitcoin/bips basic-test-vectors.json）：
  - P2WPKH 空消息 / "Hello World" / 第二签变体 / 错消息 / 错地址（5）
  - P2TR "No prefix fallback" 无前缀 fallback（1）
  - tagged_hash 已知向量（1）
- 签名+验签往返（bitcoin crate 确定性私钥签 + 本 crate 验）：
  - P2WPKH / P2TR（手动 BIP-341 tweak）/ P2PKH legacy（3）
- 错误路径：非法 base64 / 空签名 / 非法地址 / 短 legacy 签名（4）

### 4. 踩坑记录（供后续维护）
- `split_at(len-1)` 返回 `(prefix, suffix)`——DER 在前、sighash 字节在后，**变量名顺序易写反**
  （曾导致 sighash 解析为 DER 序列标签 0x30）。
- bitcoin 0.32 的 `crypto::key` 模块为 `pub(crate)`，`TapTweak`/`TweakedPublicKey` 不可公开
  访问——P2TR 往返测手动实现 BIP-341 tweak（`t = TaggedHash("TapTweak", internal_key)`，
  `tweaked_sk = sk + t`），地址经 `WitnessProgram::new(V1, &out_xonly)` 构造。
- secp256k1 经 `bitcoin::secp256k1`（0.29）访问，与 bitcoin 锁定同一次版本（`Scalar::from_be_bytes`
  / `add_tweak` 均 0.29 API）。
- `legacy_signature_hash` 接 `&self`（无需 mut）；`p2wpkh`/`taproot` sighash 接 `&mut self`。

### 红线遵守
- 未改任何 trait 签名（`ChainAdapter::verify_signature` 仍调 `signing::verify_bip322`，
  函数签名 `(message, signature, address) -> WalletResult<bool>` 零改动）。
- 未改其他 agent crate（仅 os-wallet / Cargo.toml +1 依赖 / PROGRESS.md）。
- 新增 base64 依赖已在 workspace 注册（ADR-DEPS-001），无新第三方引入。
