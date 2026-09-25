# 区块链 SDK：secp256k1 身份与三步认证

> 目标：让任何语言的客户端（python / Rust / 浏览器 JS）用**一把 secp256k1 私钥**
> 作为身份，接入 IM / NexHub / agent 协调等所有链上身份组件。
> 内核源码：`crates/os-common/src/chain_auth.rs`（`ChainAuth`）。
>
> 前置：懂 ECDSA 基本概念即可，本文给出可直接运行的代码。

## 1. 身份模型

- **身份 = 压缩公钥**：`0x` + 66 hex（33 字节，`02`/`03` 前缀 + X）；
- **权限 = 私钥持有者**：无用户名密码，签名即登录；
- **展示名 = EVM 地址**：`keccak256(未压缩公钥[1..])[12..]`（
  `chain_auth.rs` 的 `derive_display_name`，与以太坊地址规则一致）；
- **一钥多组件**：IM 与 NexHub 各挂**独立** `ChainAuth` 实例（token 桶互不
  相通），但客户端可用同一密钥对分别在两侧完成认证。

## 2. 三步认证（挑战-签名-换 token）

```text
① POST <prefix>/auth/challenge {pubkey}          → {nonce}        60s 单次有效
② 本地签名：sign(SHA-256(nonce 的 UTF-8 字节))    → 65 字节 r||s||v hex
③ POST <prefix>/auth/verify {pubkey,nonce,sig}   → {token}        24h，单点登录
④ 业务端点 Authorization: Bearer <token>（服务端反查 pubkey，body 自报身份一律忽略）
```

| 组件 | prefix | 说明 |
|---|---|---|
| IM | `/api/v1/im` | token 同时用于 WS 握手 `?user=<pubkey>&token=<token>` |
| NexHub | `/api/v1/nexhub` | 项目所有权 = 私钥持有者（publish 自动归因） |

## 3. python 客户端（两个必踩坑 + 正确代码）

**坑 ① 低 S 规范化**：python-ecdsa 签名可能产生高 S。服务端 k256 按 BIP62
**拒绝高 S 签名**（@noble/secp256k1 网页端默认低 S 所以前端无感，python 必踩）。
修法：`s > N//2 时 s = N - s`。

**坑 ② 压缩公钥前缀**：`02`/`03` 由 **Y 坐标奇偶**决定（Y 偶=02，Y 奇=03），
不是 X 的首字节。取错前缀时 challenge 只验格式能过、verify 必 401——
极易误判为服务端 bug。

正确代码（实测在跑，摘自 106 常驻 agent `/tank/os-data/dev-standby-agent.py`）：

```python
import ecdsa, hashlib

# secp256k1 阶 N（低 S 规范化用）
_N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141

def compress_pubkey(sk):
    vk = sk.get_verifying_key()
    xy = vk.to_string()              # 64 字节 = X(32) || Y(32)
    # 坑②：前缀按 Y 坐标奇偶——取 Y 最后一字节的低位
    pub = b"\x02" if xy[63] & 1 == 0 else b"\x03"
    return "0x" + (pub + xy[:32]).hex()   # 0x + 66 hex

def sign_hex(sk, msg):
    sig = sk.sign(msg, hashfunc=hashlib.sha256,
                  sigencode=ecdsa.util.sigencode_string)  # r||s 各 32 字节
    r = sig[:32]
    s = int.from_bytes(sig[32:], "big")
    # 坑①：低 S 规范化——高 S 签名服务端直接拒
    if s > _N // 2:
        s = _N - s
    return (r + s.to_bytes(32, "big") + b"\x00").hex()   # 65 字节 r||s||v
```

认证调用（IM 侧）：

```python
nonce = requests.post(f"{BASE}/api/v1/im/auth/challenge",
                      json={"pubkey": PUB}).json()["nonce"]
sig = sign_hex(SK, nonce.encode())          # 对 nonce 的 UTF-8 字节签
token = requests.post(f"{BASE}/api/v1/im/auth/verify",
                      json={"pubkey": PUB, "nonce": nonce,
                            "signature": sig}).json()["token"]
```

## 4. Rust 客户端（直接用 os_common）

```rust
use os_common::chain_auth::{self, ChainAuth};

// 客户端侧生成密钥对与签名（与 chain_auth.rs 单测同栈：k256 + sha2）
let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
let pubkey = format!("0x{}", hex::encode(
    sk.verifying_key().to_encoded_point(true).as_bytes()));   // 压缩 66 hex
let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());  // SHA-256(nonce UTF-8)
let (sig, _recid) = sk.sign_digest_recoverable(digest).unwrap();
// 服务端验签：65 字节 r||s||v，v 忽略（k256 内部再 SHA-256 一次摘要）

// 服务端侧（handler 内）装配独立实例：
let auth = ChainAuth::new();
let nonce = auth.create_nonce(&pubkey);        // ① 挑战
assert!(auth.take_nonce(&pubkey, &nonce));     // ② 匹配即焚（60s TTL）
let (token, _) = auth.issue_token(&pubkey);    // ③ 24h token
let pk = auth.verify_token(&token);            // ④ Bearer 反查
```

## 5. 浏览器 JS

前端已用 `@noble/secp256k1`（`web/package.json` 依赖），`sign(sha256(new
TextEncoder().encode(nonce)), priv)` 直接兼容（服务端忽略 v）——参考
`docs/NEXHUB_ONBOARDING.md` §链上身份。

## 参考

- 内核源码与单测：`crates/os-common/src/chain_auth.rs`（桶语义/单点登录/已知向量）
- [../IM_BLOCKCHAIN_AUTH_DESIGN.md](../IM_BLOCKCHAIN_AUTH_DESIGN.md) —— IM 侧设计与 §6 平台通用性
- [../MEDIA_GEN_AND_CHAIN_AUTH.md](../MEDIA_GEN_AND_CHAIN_AUTH.md) §C —— 共享内核的由来
- 完整 python 实战：`/tank/os-data/dev-standby-agent.py`（106），分析见 [04-im-agent.md](04-im-agent.md)
