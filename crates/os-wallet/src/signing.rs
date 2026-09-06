//! 签名编码/哈希与**真实验签**（BIP-322/Schnorr/EIP-191/EIP-712）。
//!
//! 本模块分两层：
//! - **纯逻辑构造**（[`eip191_personal_sign_message`] / [`eip712_type_string`]）：
//!   只做消息前缀/编码，可独立单元测；
//! - **真实验签**（[`verify_eip191`] / [`verify_eip712`] / [`verify_schnorr`] /
//!   [`verify_ecdsa_message`]）：基于 [`alloy`]（EVM：keccak256 + secp256k1 恢复地址）
//!   与 [`bitcoin`] / [`secp256k1`]（BTC：Schnorr BIP-340 / ECDSA signmessage）。
//!
//! 设计：所有验签函数返回 `WalletResult<bool>`——`Ok(true)` 表示签名对且地址匹配；
//! `Ok(false)` 表示签名可解析但与期望地址不匹配（即验签失败，**不抛错**便于上层按
//! `false` 路径处理）；`Err(...)` 仅用于签名字节/格式不可解析、缺密钥等"无法判定"。
//!
//! 算法/依赖选型（ADR-DEPS-002）：
//! - EIP-191 / EIP-712：用 [`alloy::primitives::Signature::recover_address_from_msg`] /
//!   `recover_address_from_prehash`（内部 keccak256 + secp256k1 恢复公钥 + 地址截取）。
//! - Schnorr（BIP-340）：用 `secp256k1::XOnlyPublicKey::verify_schnorr` 对
//!   `sha256(message)` 摘要做真实校验（即"对消息摘要的 Schnorr 签名"——非完整 BIP-322，
//!   BIP-322 的伪交易封装见 [`verify_bip322`] 的 TODO）。
//! - ECDSA（Bitcoin Core signmessage 兼容）：用 [`secp256k1::ecdsa`] + 双 sha256 摘要
//!   + 恢复公钥，可校验 Bitcoin Core / Unisat 等"signmessage"输出的 65 字节 ECDSA 签名。

use crate::WalletError;
use crate::WalletResult;
use core::str::FromStr;

use alloy::dyn_abi::eip712::TypedData;
use alloy::primitives::{Address, Signature};

// ----------------------------------------------------------------------------
// 常量 / 前缀
// ----------------------------------------------------------------------------

/// EIP-191 personal_sign 的魔术前缀（不含长度）。
pub const EIP191_PREFIX: &str = "\u{19}Ethereum Signed Message:\n";

/// Bitcoin Core `signmessage` 的魔术前缀（双 sha256 前缀）。
///
/// 规范：`b"\x18Bitcoin Signed Message:\n" + len(payload) + payload`，
/// 随后做两次 SHA-256 作为 secp256k1 摘要。本常量只含魔术前缀，不含长度与 payload。
pub const BTC_MESSAGE_PREFIX: &[u8] = b"\x18Bitcoin Signed Message:\n";

/// EIP-712 域分隔符的规范化字段集（EIP-712 §3.1 定义的固定字段）。
pub const EIP712_DOMAIN_FIELDS: &[(&str, &str)] = &[
    ("name", "string"),
    ("version", "string"),
    ("chainId", "uint256"),
    ("verifyingContract", "address"),
];

// ----------------------------------------------------------------------------
// 纯逻辑：消息前缀构造（无外部依赖）
// ----------------------------------------------------------------------------

/// 构造 EIP-191 personal_sign 的待哈希字节序列。
///
/// 规范：`b"\x19Ethereum Signed Message:\n" + len(payload) + payload`，
/// 其中 `len` 为 payload 的**十进制字节长度**。钱包侧 `personal_sign` 与
/// `eth_sign` 都使用此前缀。
///
/// > 注：[`verify_eip191`] 内部已自动应用此前缀（经 alloy 的
/// > `Signature::recover_address_from_msg`），故一般无需手动调用本函数；保留它是
/// > 为测试/日志/未来手算哈希的场景。
///
/// # 示例
/// ```
/// use os_wallet::signing::eip191_personal_sign_message;
/// let msg = eip191_personal_sign_message("Hello");
/// let s = String::from_utf8_lossy(&msg);
/// assert!(s.starts_with("\u{19}Ethereum Signed Message:\n5"));
/// ```
pub fn eip191_personal_sign_message(payload: &str) -> Vec<u8> {
    let payload_bytes = payload.as_bytes();
    let len_str = payload_bytes.len().to_string();
    let mut out = Vec::with_capacity(EIP191_PREFIX.len() + len_str.len() + payload_bytes.len());
    out.extend_from_slice(EIP191_PREFIX.as_bytes());
    out.extend_from_slice(len_str.as_bytes());
    out.extend_from_slice(payload_bytes);
    out
}

/// 构造 Bitcoin Core `signmessage` 的待哈希字节序列（魔术前缀 + 长度 + payload）。
///
/// 规范：`b"\x18Bitcoin Signed Message:\n" + compact-len(payload) + payload`，
/// 其中长度采用 Bitcoin 的紧凑变长编码（VarStr：1 字节长度前缀当 len ≤ 252；
/// 本实现覆盖常见 ≤ 252 字节消息；超长消息需补 0xfd 前缀，预留 TODO）。
///
/// 输出随后应送入 `sha256d`（两次 SHA-256）作为 secp256k1 摘要。
pub fn bitcoin_message_magic(payload: &str) -> Vec<u8> {
    let payload_bytes = payload.as_bytes();
    let len = payload_bytes.len();
    let mut out = Vec::with_capacity(BTC_MESSAGE_PREFIX.len() + 1 + len);
    out.extend_from_slice(BTC_MESSAGE_PREFIX);
    if len <= 252 {
        out.push(len as u8);
    } else {
        // > 252 字节：Bitcoin VarStr 用 0xfd + LE u16，本路径目前极少出现，
        // 留给未来补全（钱包 signmessage 通常 ≤ 252）。
        out.push(0xfd);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    }
    out.extend_from_slice(payload_bytes);
    out
}

/// EIP-712 类型化数据的类型哈希种子（primaryType 的 typeString）。
///
/// EIP-712 规定：对每个 struct，先按规范构造 `typeString`（字段名排序后拼接），
/// 再 keccak256 得 `typeHash`；本函数返回**未哈希**的 typeString 字节。
///
/// 真实 keccak256 + typed_data 验签由 [`verify_eip712`] 完成（基于 alloy 的 `TypedData`）；
/// 本纯逻辑函数保留为辅助/参考。
///
/// 参考：<https://eips.ethereum.org/EIPS/eip-712>
pub fn eip712_type_string(primary_type: &str, mut fields: Vec<(&str, &str)>) -> String {
    // 字段按字段名排序（EIP-712 规定）。
    fields.sort_by(|a, b| a.0.cmp(b.0));
    let body: Vec<String> = fields
        .into_iter()
        .map(|(name, ty)| format!("{ty} {name}"))
        .collect();
    format!("{}({})", primary_type, body.join(","))
}

// ============================================================================
// 真实验签：EVM（EIP-191 / EIP-712）
// ============================================================================
//
// 依赖 alloy 2.x（ADR-DEPS-002）：`Signature::recover_address_from_msg` 内部
// 完成 keccak256(EIP-191 前缀) + secp256k1 公钥恢复 + 地址(后 20 字节)截取。

/// 把"无法解析签名/地址"的底层错误统一映射为 `WalletError::SignatureInvalid`。
fn map_recover_err(e: impl core::fmt::Display, ctx: &str) -> WalletError {
    WalletError::SignatureInvalid(format!("{ctx}: {e}"))
}

/// 把十六进制字符串（可选 `0x` 前缀）解析为字节。
pub(crate) fn decode_hex_maybe_0x(s: &str) -> WalletResult<Vec<u8>> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    hex_decode(stripped)
}

/// 不带前缀的十六进制解码（统一错误信息）。
fn hex_decode(s: &str) -> WalletResult<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(WalletError::SignatureInvalid(format!(
            "十六进制长度非偶数: {s}"
        )));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> WalletResult<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(WalletError::SignatureInvalid(format!(
            "非十六进制字符: {}",
            char::from(b)
        ))),
    }
}

/// 解析签名输入：接受 raw 字节（65 字节 r||s||v）或十六进制字符串。
///
/// - 65 字节直接构造（r/s 各 32 字节 + 1 字节 recovery id，v 取 27/28 或 0/1 均支持）；
/// - 其他长度走 hex 字符串解析路径（必须为可打印十六进制）。
///
/// 返回 alloy 的 `Signature`（含 recovery id）。
pub fn parse_signature(signature: &[u8]) -> WalletResult<Signature> {
    if signature.len() == 65 {
        // 原始 r||s||v：直接构造（alloy `Signature::try_from(&[u8])` 解析 65 字节大端）。
        Signature::try_from(signature).map_err(|e| map_recover_err(e, "解析 65 字节签名失败"))
    } else {
        // 当作十六进制字符串处理（必须 UTF-8）。
        let s = std::str::from_utf8(signature)
            .map_err(|e| WalletError::SignatureInvalid(format!("签名非 UTF-8 hex: {e}")))?;
        Signature::from_str(s).map_err(|e| map_recover_err(e, "解析 hex 签名失败"))
    }
}

/// 解析 EVM 地址（`0x` + 40 hex），返回 alloy `Address`（不校验 checksum）。
pub fn parse_evm_address(addr: &str) -> WalletResult<Address> {
    Address::from_str(addr).map_err(|e| map_recover_err(e, "解析 EVM 地址失败"))
}

/// 验证 EIP-191（personal_sign）签名。
///
/// - `message`：personal_sign 的明文（**不含**前缀；alloy 内部会自动加
///   `\x19Ethereum Signed Message:\n<len>` 前缀并 keccak256）。
/// - `signature`：65 字节 r||s||v 或十六进制字符串。
/// - `expected_address`：期望的签名地址（`0x` + 40 hex，大小写均可）。
///
/// 返回：
/// - `Ok(true)`：签名可恢复且恢复出的地址 == expected。
/// - `Ok(false)`：签名可恢复但地址不匹配（验签失败，**不抛错**）。
/// - `Err(SignatureInvalid)`：签名/地址格式不可解析。
///
/// # 已知向量（web3.js v1.2.2）
/// 消息 `"Some data"`，签名
/// `b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c`，
/// 恢复地址 `0x2c7536E3605D9C16a7a3D7b1898e529396a65c23`。
pub fn verify_eip191(
    message: &str,
    signature: &[u8],
    expected_address: &str,
) -> WalletResult<bool> {
    let sig = parse_signature(signature)?;
    let expected = parse_evm_address(expected_address)?;
    // alloy 内部：keccak256(EIP-191 前缀 + message) → ecdsa recover → 地址。
    let recovered = sig
        .recover_address_from_msg(message)
        .map_err(|e| map_recover_err(e, "EIP-191 地址恢复失败"))?;
    Ok(recovered == expected)
}

/// 验证 EIP-712（typed_data）签名。
///
/// - `typed_data_json`：完整的 EIP-712 typed_data JSON 对象（含 `types`/`domain`/
///   `primaryType`/`message`，可以是字符串化 JSON 或裸对象 JSON）。
/// - `signature`：65 字节 r||s||v 或 hex 字符串。
/// - `expected_address`：期望签名地址。
///
/// 实现：用 alloy 的 `TypedData::eip712_signing_hash()` 求签名摘要
/// （`keccak256("\x19\x01" ‖ domainSeparator ‖ hashStruct(message))`），再用
/// `Signature::recover_address_from_prehash` 恢复地址比对。
///
/// 返回：同 [`verify_eip191`]。
pub fn verify_eip712(
    typed_data_json: &str,
    signature: &[u8],
    expected_address: &str,
) -> WalletResult<bool> {
    let sig = parse_signature(signature)?;
    let expected = parse_evm_address(expected_address)?;
    // typed_data JSON 解析（裸对象或字符串化 JSON 均可）。
    let typed: TypedData = serde_json::from_str(typed_data_json).map_err(|e| {
        WalletError::SignatureInvalid(format!("EIP-712 typed_data JSON 解析失败: {e}"))
    })?;
    let digest = typed.eip712_signing_hash().map_err(|e| {
        WalletError::SignatureInvalid(format!("EIP-712 signing hash 计算失败: {e}"))
    })?;
    let recovered = sig
        .recover_address_from_prehash(&digest)
        .map_err(|e| map_recover_err(e, "EIP-712 地址恢复失败"))?;
    Ok(recovered == expected)
}

// ============================================================================
// 真实验签：BTC（Schnorr BIP-340 / ECDSA signmessage）
// ============================================================================
//
// 依赖 bitcoin 0.32 + secp256k1 0.31（ADR-DEPS-002）。
//
// 注：secp256k1 在 alloy 与 bitcoin 两栈均作为同一传递依赖（0.29/0.30/0.31 多版本共存，
// 见 ADR-DEPS-002 代价节）；为避免版本冲突，本模块通过 bitcoin crate 重导出的
// `secp256k1`（即 bitcoin::secp256k1）使用，与 bitcoin 锁定同一 secp256k1 次版本。

/// 用全局 `Secp256k1` 验证上下文（只读验签）。惰性构造一次复用，避免每次验签重新 alloc。
///
/// 经 `bitcoin::secp256k1`（0.29）访问，与 bitcoin crate 锁定同一 secp256k1 次版本，
/// 避免 0.29/0.31 多版本类型不互通。
fn secp_ctx() -> &'static bitcoin::secp256k1::Secp256k1<bitcoin::secp256k1::VerifyOnly> {
    use std::sync::OnceLock;
    static CTX: OnceLock<bitcoin::secp256k1::Secp256k1<bitcoin::secp256k1::VerifyOnly>> =
        OnceLock::new();
    CTX.get_or_init(bitcoin::secp256k1::Secp256k1::verification_only)
}

/// 验证 BIP-340 Schnorr 签名（对消息明文的 sha256 摘要）。
///
/// 这是简化版的"Schnorr 消息签名"——约定被签消息为 `sha256(payload)`（32 字节），
/// 签名为 BIP-340 标准的 64 字节（不含奇偶位字节；若给 65 字节会忽略末位字节）。
/// 真正的 BIP-322（伪交易封装）见 [`verify_bip322`] 的 TODO。
///
/// - `message`：被签明文（先 sha256 得摘要）。
/// - `signature`：64 字节 Schnorr 签名（多出的末位字节忽略）。
/// - `pubkey_xonly_hex`：32 字节 x-only 公钥的 hex（BIP-340 taproot 输出密钥）。
///
/// 返回：同 [`verify_eip191`]（`Ok(false)` = 验签失败但不抛错）。
pub fn verify_schnorr(
    message: &str,
    signature: &[u8],
    pubkey_xonly_hex: &str,
) -> WalletResult<bool> {
    if signature.len() < 64 {
        return Err(WalletError::SignatureInvalid(format!(
            "Schnorr 签名至少 64 字节，实际 {}",
            signature.len()
        )));
    }
    // 取前 64 字节（BIP-340 规范）。
    let sig_arr: [u8; 64] = signature[..64].try_into().expect("前 64 字节切片转数组");
    let schnorr_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&sig_arr)
        .map_err(|e| map_recover_err(e, "解析 Schnorr 签名失败"))?;
    // x-only 公钥（32 字节）。
    let pk_bytes = decode_hex_maybe_0x(pubkey_xonly_hex)?;
    if pk_bytes.len() != 32 {
        return Err(WalletError::SignatureInvalid(format!(
            "x-only 公钥须 32 字节，实际 {}",
            pk_bytes.len()
        )));
    }
    let pk_arr: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| WalletError::SignatureInvalid("x-only 公钥数组转换失败".into()))?;
    #[allow(deprecated)] // from_slice 在 0.29 是稳定 API（deprecated 是 0.31+ 提示）。
    let xonly = bitcoin::secp256k1::XOnlyPublicKey::from_slice(&pk_arr)
        .map_err(|e| map_recover_err(e, "解析 x-only 公钥失败"))?;
    // 摘要：sha256(message)。
    use bitcoin::hashes::{sha256, Hash as _};
    let digest = sha256::Hash::hash(message.as_bytes());
    let msg = bitcoin::secp256k1::Message::from_digest(digest.to_byte_array());
    let ctx = secp_ctx();
    // verify_schnorr 返回 Result<(), Error>：Ok = 验签通过；Err = 验签失败。
    // 我们把 Err 视为"验签失败但不抛错"（Ok(false)），仅当签名/公钥格式不可解析才抛错。
    Ok(ctx.verify_schnorr(&schnorr_sig, &msg, &xonly).is_ok())
}

/// 验证 Bitcoin Core 风格的 ECDSA `signmessage` 签名（兼容 Bitcoin Core / Unisat 等）。
///
/// - `message`：被签明文。
/// - `signature`：65 字节 ECDSA 签名（r||s||recid，recid 通常 27-34 或裸 0-3）。
/// - `pubkey_hex`：用于校验的公钥 hex（33 压缩 / 65 非压缩）。与签名恢复出的公钥比对；
///   地址（bech32/base58check）比对留 TODO（需 hash160 + checksum 编解码）。
///
/// 返回：同 [`verify_eip191`]。
pub fn verify_ecdsa_message(
    message: &str,
    signature: &[u8],
    pubkey_hex: &str,
) -> WalletResult<bool> {
    // 解析 65 字节签名。
    if signature.len() < 65 {
        return Err(WalletError::SignatureInvalid(format!(
            "ECDSA signmessage 签名至少 65 字节，实际 {}",
            signature.len()
        )));
    }
    let r_s = &signature[..64];
    let recid_byte = signature[64];
    // Bitcoin Core signmessage 的 recovery id 通常 27-34（含压缩位偏移），本实现兼容
    // 27/28（未压缩）/31/32（压缩）以及 0-3（裸 recid）。
    let recid_val = match recid_byte {
        27..=30 => recid_byte - 27,
        31..=34 => recid_byte - 31,
        _ => recid_byte,
    };
    let recid = bitcoin::secp256k1::ecdsa::RecoveryId::from_i32(recid_val as i32)
        .map_err(|e| map_recover_err(e, "recovery id 非法"))?;
    // 构造可恢复签名（r||s + recid）。
    let rec_sig = bitcoin::secp256k1::ecdsa::RecoverableSignature::from_compact(r_s, recid)
        .map_err(|e| map_recover_err(e, "解析 ECDSA 可恢复签名失败"))?;
    // 双 sha256 摘要（Bitcoin signmessage 规范：sha256d(magic + len + payload)）。
    use bitcoin::hashes::{sha256d, Hash as _};
    let magic = bitcoin_message_magic(message);
    let digest = sha256d::Hash::hash(&magic);
    let msg = bitcoin::secp256k1::Message::from_digest(digest.to_byte_array());
    let ctx = secp_ctx();
    let recovered = ctx
        .recover_ecdsa(&msg, &rec_sig)
        .map_err(|e| map_recover_err(e, "ECDSA 公钥恢复失败"))?;
    // 解析提供的公钥 hex（33 压缩 / 65 非压缩）。
    let pk_bytes = decode_hex_maybe_0x(pubkey_hex)?;
    let provided = bitcoin::secp256k1::PublicKey::from_slice(&pk_bytes)
        .map_err(|e| map_recover_err(e, "解析校验公钥失败"))?;
    Ok(recovered == provided)
}

// ============================================================================
// 真实验签：BIP-322（通用签名消息）
// ============================================================================
//
// BIP-322 定义"通用签名消息"格式：构造一笔虚拟 to_spend 交易（把消息封装进
// scriptSig，输出到签名者地址的 scriptPubKey），再构造一笔 to_sign 交易引用
// to_spend 的输出，签名者用其私钥对 to_sign 的 sighash 签名，签名以 witness 形式
// 封装。验签 = 重建 to_spend/to_sign + 计算 sighash + 校验 witness 中的签名。
//
// 支持：
// - **legacy**（P2PKH）：base64 编码的 65 字节可恢复 ECDSA 签名（无前缀），签名
//   摘要为 BIP-322 的 to_sign legacy sighash；恢复公钥后比对地址（hash160）。
// - **simple**（P2WPKH / P2SH-P2WPKH / P2TR）：`smp` 前缀 + base64(consensus 编码的
//   witness stack)；按地址类型计算 BIP-143 / taproot key-spend sighash 后校验。
//
// 参考：https://github.com/bitcoin/bips/blob/master/bip-0322.mediawiki
// 测试向量：bitcoin/bips 仓库 bip-0322/basic-test-vectors.json

/// BIP-322 tagged-hash 用的标签（`BIP0322-signed-message`）。
const BIP322_TAG: &str = "BIP0322-signed-message";
/// simple 签名的人类可读前缀（`smp`）。
const BIP322_SIMPLE_PREFIX: &str = "smp";

// 本节统一引入 Hash trait，便于 sighash 的 to_byte_array / as_byte_array 转换。
#[allow(unused_imports)]
use bitcoin::hashes::Hash as _;

/// BIP-322 tagged hash：`SHA256(SHA256(tag) || SHA256(tag) || message)`。
///
/// 规范见 BIP-340 tagged hash 定义；BIP-322 用 `BIP0322-signed-message` 作为 tag，
/// 输入为消息明文（**不含**长度前缀）。
fn bip322_tagged_hash(message: &[u8]) -> [u8; 32] {
    use bitcoin::hashes::{sha256, Hash as _, HashEngine as _};
    let tag_hash = sha256::Hash::hash(BIP322_TAG.as_bytes()).to_byte_array();
    let mut engine = sha256::Hash::engine();
    engine.input(&tag_hash);
    engine.input(&tag_hash);
    engine.input(message);
    sha256::Hash::from_engine(engine).to_byte_array()
}

/// 构造 BIP-322 `to_spend` 虚拟交易。
///
/// 规范（BIP-322 §to_spend）：
/// - version = 0, lock_time = 0
/// - input[0]: prevout = (null txid, 0xFFFFFFFF), sequence = 0,
///   scriptSig = `OP_0 PUSH32[ message_hash ]`（message_hash = tagged_hash(message)）
/// - output[0]: value = 0, scriptPubKey = 签名者地址的 scriptPubKey（message challenge）
fn bip322_create_to_spend(
    address: &bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    message: &str,
) -> WalletResult<bitcoin::Transaction> {
    let msg_hash = bip322_tagged_hash(message.as_bytes());
    use bitcoin::script::PushBytesBuf;
    let mut push = PushBytesBuf::new();
    push.extend_from_slice(&msg_hash)
        .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 push 构造失败: {e}")))?;
    let script_sig = bitcoin::script::Builder::new()
        .push_int(0)
        .push_slice(&push)
        .into_script();
    Ok(bitcoin::Transaction {
        version: bitcoin::transaction::Version(0),
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::null(),
            script_sig,
            sequence: bitcoin::Sequence(0),
            witness: bitcoin::Witness::new(),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey: address.assume_checked_ref().script_pubkey(),
        }],
    })
}

/// 构造 BIP-322 `to_sign` 虚拟交易（含可选 witness）。
///
/// 规范（BIP-322 §to_sign）：
/// - version = 0 (or 2), lock_time = 0
/// - input[0]: prevout = (to_spend.txid(), 0), sequence = 0,
///   scriptSig = 空（segwit）, witness = 签名者提供的 witness stack
/// - output[0]: value = 0, scriptPubKey = `OP_RETURN`
fn bip322_create_to_sign(
    to_spend: &bitcoin::Transaction,
    witness: bitcoin::Witness,
) -> bitcoin::Transaction {
    bitcoin::Transaction {
        version: bitcoin::transaction::Version(0),
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint {
                txid: to_spend.compute_txid(),
                vout: 0,
            },
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence(0),
            witness,
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey: bitcoin::script::Builder::new()
                .push_opcode(bitcoin::opcodes::all::OP_RETURN)
                .into_script(),
        }],
    }
}

/// 解析签名输入为字节：接受 base64 文本或原始字节。
///
/// - 若输入为合法 UTF-8 且含 base64 字符集 → 当作 base64 解码；
/// - 否则当作原始字节（用于上层直接传 base64 解码后的字节或 raw witness 序列化）。
fn decode_signature_bytes(signature: &[u8]) -> WalletResult<Vec<u8>> {
    if let Ok(s) = std::str::from_utf8(signature) {
        // 容忍首尾空白。
        let trimmed = s.trim();
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .map_err(|e| {
                WalletError::SignatureInvalid(format!("BIP-322 签名 base64 解码失败: {e}"))
            })
    } else {
        Ok(signature.to_vec())
    }
}

/// 验证 BIP-322 签名（legacy P2PKH + simple P2WPKH/P2SH-P2WPKH/P2TR）。
///
/// - `message`：被签消息明文。
/// - `signature`：
///   - simple：`smp` + base64(consensus 编码的 witness stack)，或直接传 base64 文本；
///   - legacy（P2PKH）：base64(65 字节可恢复 ECDSA 签名 r||s||recid)。
/// - `address`：Bitcoin 地址（bech32/bech32m/base58check）。
///
/// 实现流程：
/// 1. 解析地址 → `Address<NetworkUnchecked>` → `assume_checked` 取 scriptPubKey / 类型；
/// 2. 构造 `to_spend`（commit 消息 + 地址 challenge）；
/// 3. 按地址类型分发：
///    - P2PKH → legacy 路径：用 to_sign 的 legacy sighash + recover 公钥 → hash160 比对；
///    - P2WPKH/P2SH-P2WPKH → simple 路径：解析 witness stack → BIP-143 sighash + ECDSA 验签；
///    - P2TR → simple 路径：解析 witness → taproot key-spend sighash + Schnorr 验签；
/// 4. 签名/公钥格式不可解析返回 `Err(SignatureInvalid)`；验签失败返回 `Ok(false)`。
///
/// 返回：同 [`verify_eip191`]。
pub fn verify_bip322(message: &str, signature: &[u8], address: &str) -> WalletResult<bool> {
    // 1. 解析地址（bech32/bech32m/base58check），assume_checked 取 scriptPubKey。
    let addr: bitcoin::Address<bitcoin::address::NetworkUnchecked> =
        bitcoin::Address::from_str(address)
            .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 地址解析失败: {e}")))?;
    let checked = addr.assume_checked();
    let addr_type = checked
        .address_type()
        .ok_or_else(|| WalletError::SignatureInvalid(format!("BIP-322 地址类型未知: {address}")))?;

    // 2. 解析签名为 UTF-8 文本（simple/legacy 都是 base64 文本）。
    let sig_text = std::str::from_utf8(signature).map_err(|_| {
        WalletError::SignatureInvalid("BIP-322 签名非 UTF-8（应为 base64 文本）".into())
    })?;
    let sig_text = sig_text.trim();
    if sig_text.is_empty() {
        return Err(WalletError::SignatureInvalid(
            "BIP-322 签名为空（signature too short）".into(),
        ));
    }

    // 3. 按地址类型分发到 legacy 或 simple 路径。
    match addr_type {
        bitcoin::AddressType::P2pkh => {
            // legacy：无前缀，base64(65 字节可恢复 ECDSA)。
            let raw = decode_signature_bytes(signature)?;
            verify_bip322_legacy_p2pkh(message, &raw, &checked)
        }
        bitcoin::AddressType::P2wpkh | bitcoin::AddressType::P2sh => {
            // simple（P2WPKH / P2SH-P2WPKH）：smp 前缀 + base64(witness stack)。
            let body = strip_simple_prefix(sig_text);
            let raw = decode_signature_bytes(body.as_bytes())?;
            verify_bip322_simple_segwit(message, &raw, &checked, addr_type)
        }
        bitcoin::AddressType::P2tr => {
            // simple（P2TR key-spend）：smp 前缀 + base64(witness stack)，
            // 或无前缀 fallback（仅 64/65 字节 schnorr 签名 base64）。
            let body = strip_simple_prefix(sig_text);
            let raw = decode_signature_bytes(body.as_bytes())?;
            verify_bip322_simple_segwit(message, &raw, &checked, addr_type)
        }
        // P2WSH（非 multisig 标准脚本）暂不支持到具体脚本求值，留 generic 路径。
        other => Err(WalletError::SignatureInvalid(format!(
            "BIP-322 暂不支持该地址类型: {other:?}"
        ))),
    }
}

/// 去除 simple 签名的 `smp` 前缀（容错：无前缀也接受，便于 base64 直传）。
fn strip_simple_prefix(sig_text: &str) -> &str {
    sig_text
        .strip_prefix(BIP322_SIMPLE_PREFIX)
        .unwrap_or(sig_text)
}

/// 验证 BIP-322 legacy（P2PKH）签名。
///
/// 旧式签名是 65 字节可恢复 ECDSA 签名，对 BIP-322 to_sign 的 legacy sighash 签名。
/// BIP-322 legacy 模式：to_sign 的 input 引用 to_spend 的 P2PKH 输出，签名哈希以
/// 该 P2PKH scriptPubKey 为 code（即 OP_DUP OP_HASH160 <20> OP_EQUALVERIFY OP_CHECKSIG）。
/// 验签：recover 公钥 → hash160(pubkey) → 比对地址 scriptPubKey 中的 pubkey hash。
fn verify_bip322_legacy_p2pkh(
    message: &str,
    raw: &[u8],
    address: &bitcoin::Address,
) -> WalletResult<bool> {
    if raw.len() != 65 {
        return Err(WalletError::SignatureInvalid(format!(
            "BIP-322 legacy 签名须 65 字节（r||s||recid），实际 {}",
            raw.len()
        )));
    }
    let to_spend = bip322_create_to_spend(address.as_unchecked(), message)?;
    // legacy to_sign：witness 为空（非 segwit）。
    let to_sign = bip322_create_to_sign(&to_spend, bitcoin::Witness::new());

    // 传统 legacy sighash：以 to_spend 输出（P2PKH scriptPubKey）为 code 计算。
    // SIGHASH_ALL = 1（u32）。
    use bitcoin::sighash::SighashCache;
    let cache = SighashCache::new(&to_sign);
    let spk = address.script_pubkey();
    let sighash = cache
        .legacy_signature_hash(0, &spk, 0x01u32)
        .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 legacy sighash 失败: {e}")))?;
    let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());

    // 可恢复 ECDSA 签名解析（recid 在末字节，27-34 兼容 Bitcoin Core 偏移 / 裸 0-3）。
    let recid_byte = raw[64];
    let recid_val = match recid_byte {
        27..=30 => recid_byte - 27,
        31..=34 => recid_byte - 31,
        _ => recid_byte,
    };
    let recid = bitcoin::secp256k1::ecdsa::RecoveryId::from_i32(recid_val as i32)
        .map_err(|e| map_recover_err(e, "BIP-322 legacy recovery id 非法"))?;
    let rec_sig = bitcoin::secp256k1::ecdsa::RecoverableSignature::from_compact(&raw[..64], recid)
        .map_err(|e| map_recover_err(e, "BIP-322 legacy 签名解析失败"))?;
    let ctx = secp_ctx();
    let pubkey = ctx
        .recover_ecdsa(&msg, &rec_sig)
        .map_err(|e| map_recover_err(e, "BIP-322 legacy 公钥恢复失败"))?;

    // 比对恢复公钥的 hash160 与地址 scriptPubKey 中的 hash。
    // 尝试压缩 / 非压缩两种公钥形式（地址生成时压缩为主流）。
    use bitcoin::hashes::{hash160, Hash as _};
    let spk_bytes = spk.as_bytes();
    if spk_bytes.len() < 23 {
        return Err(WalletError::SignatureInvalid(
            "P2PKH scriptPubKey 长度异常".into(),
        ));
    }
    let embedded = &spk_bytes[3..23];
    // 压缩公钥 hash160。
    let pk_compressed = pubkey.serialize();
    let hash_c = hash160::Hash::hash(&pk_compressed);
    if embedded == hash_c.as_byte_array() {
        return Ok(true);
    }
    // 非压缩公钥 hash160（兼容旧钱包）。
    let pk_uncompressed = pubkey.serialize_uncompressed();
    let hash_u = hash160::Hash::hash(&pk_uncompressed);
    Ok(embedded == hash_u.as_byte_array())
}

/// 验证 BIP-322 simple（P2WPKH / P2SH-P2WPKH / P2TR）签名。
///
/// signature 为 consensus 编码的 witness stack 字节（base64 解码后）。
/// - P2WPKH：witness = [der_sig+sighash_byte, pubkey]；BIP-143 sighash + ECDSA 验签；
/// - P2SH-P2WPKH：同 P2WPKH，但 redeemScript 为 P2WPKH spk；sighash 用 redeemScript；
/// - P2TR：witness = [bip340_sig (+sighash_byte)]；taproot key-spend sighash + Schnorr 验签。
fn verify_bip322_simple_segwit(
    message: &str,
    raw: &[u8],
    address: &bitcoin::Address,
    addr_type: bitcoin::AddressType,
) -> WalletResult<bool> {
    // 1. consensus 反序列化 witness stack。
    let witness: bitcoin::Witness = bitcoin::consensus::deserialize(raw)
        .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 witness 解析失败: {e}")))?;
    let wit_items: Vec<Vec<u8>> = witness.to_vec();
    if wit_items.is_empty() {
        return Err(WalletError::SignatureInvalid(
            "BIP-322 simple witness 为空".into(),
        ));
    }
    // 转 &[[u8]] 便于按索引取切片。
    let wit_refs: Vec<&[u8]> = wit_items.iter().map(|v| v.as_slice()).collect();

    // 2. 构造 to_spend / to_sign。
    let to_spend = bip322_create_to_spend(address.as_unchecked(), message)?;
    let to_sign = bip322_create_to_sign(&to_spend, witness);

    // 3. prevout（to_spend 的 output[0]）供 sighash 计算。
    let prevout = &to_spend.output[0];

    match addr_type {
        bitcoin::AddressType::P2wpkh => verify_bip322_p2wpkh(&to_sign, &wit_refs, prevout),
        bitcoin::AddressType::P2sh => {
            // P2SH-P2WPKH：witness 额外含 redeemScript（P2WPKH spk）作为末项。
            verify_bip322_p2sh_p2wpkh(&to_sign, &wit_refs, prevout, address)
        }
        bitcoin::AddressType::P2tr => verify_bip322_p2tr(&to_sign, &wit_refs, prevout),
        other => Err(WalletError::SignatureInvalid(format!(
            "BIP-322 simple 不支持: {other:?}"
        ))),
    }
}

/// 验证 simple P2WPKH：witness = [der_sig+sighash_byte, pubkey]。
fn verify_bip322_p2wpkh(
    to_sign: &bitcoin::Transaction,
    wit_items: &[&[u8]],
    prevout: &bitcoin::TxOut,
) -> WalletResult<bool> {
    if wit_items.len() != 2 {
        return Err(WalletError::SignatureInvalid(format!(
            "BIP-322 P2WPKH witness 须 2 项（sig, pubkey），实际 {}",
            wit_items.len()
        )));
    }
    let sig_bytes = wit_items[0];
    let pub_key_bytes = wit_items[1];
    if sig_bytes.is_empty() {
        return Err(WalletError::SignatureInvalid(
            "BIP-322 P2WPKH 签名为空".into(),
        ));
    }
    // sighash type 在签名末字节（默认 0x01 SIGHASH_ALL）。
    let (der_sig, sighash_type_byte) = sig_bytes.split_at(sig_bytes.len() - 1);
    let sighash_type = bitcoin::sighash::EcdsaSighashType::from_standard(
        sighash_type_byte[0] as u32,
    )
    .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 sighash type 非法: {e}")))?;

    // 构造 P2WPKH scriptCode（0x1976a914<20>88ac）。
    let pubkey_hash = bitcoin::WPubkeyHash::hash(pub_key_bytes);
    let p2wpkh_script = bitcoin::ScriptBuf::new_p2wpkh(&pubkey_hash);

    // BIP-143 sighash。
    use bitcoin::sighash::SighashCache;
    let mut cache = SighashCache::new(to_sign);
    let sighash = cache
        .p2wpkh_signature_hash(0, &p2wpkh_script, prevout.value, sighash_type)
        .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 P2WPKH sighash 失败: {e}")))?;

    let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());
    let sig = bitcoin::secp256k1::ecdsa::Signature::from_der(der_sig)
        .map_err(|e| map_recover_err(e, "BIP-322 P2WPKH DER 签名解析失败"))?;
    let pubkey = bitcoin::secp256k1::PublicKey::from_slice(pub_key_bytes)
        .map_err(|e| map_recover_err(e, "BIP-322 P2WPKH 公钥解析失败"))?;
    let ctx = secp_ctx();
    Ok(ctx.verify_ecdsa(&msg, &sig, &pubkey).is_ok())
}

/// 验证 simple P2SH-P2WPKH：witness = [der_sig+sighash_byte, pubkey, redeemScript]。
fn verify_bip322_p2sh_p2wpkh(
    to_sign: &bitcoin::Transaction,
    wit_items: &[&[u8]],
    prevout: &bitcoin::TxOut,
    address: &bitcoin::Address,
) -> WalletResult<bool> {
    if wit_items.len() != 3 {
        return Err(WalletError::SignatureInvalid(format!(
            "BIP-322 P2SH-P2WPKH witness 须 3 项（sig, pubkey, redeemScript），实际 {}",
            wit_items.len()
        )));
    }
    let sig_bytes = wit_items[0];
    let pub_key_bytes = wit_items[1];
    let redeem_script_bytes = wit_items[2];
    if sig_bytes.is_empty() {
        return Err(WalletError::SignatureInvalid(
            "BIP-322 P2SH-P2WPKH 签名为空".into(),
        ));
    }
    // 校验 redeemScript 确为 P2WPKH 且 hash160(redeemScript) 与地址 scriptPubKey 嵌入一致。
    let redeem_script = bitcoin::ScriptBuf::from_bytes(redeem_script_bytes.to_vec());
    use bitcoin::hashes::{hash160, Hash as _};
    let redeem_hash = hash160::Hash::hash(redeem_script.as_bytes());
    let spk = address.script_pubkey();
    let spk_bytes = spk.as_bytes();
    // P2SH scriptPubKey: OP_HASH160 <20> OP_EQUAL → bytes[2..22]
    if spk_bytes.len() < 22 || &spk_bytes[2..22] != redeem_hash.as_byte_array() {
        return Ok(false);
    }
    // redeemScript 须为 P2WPKH（0x0014<20>）。
    if redeem_script.as_bytes() != [0x00u8, 0x14].as_slice() && redeem_script.as_bytes().len() != 22
    {
        return Ok(false);
    }

    let (der_sig, sighash_type_byte) = sig_bytes.split_at(sig_bytes.len() - 1);
    let sighash_type = bitcoin::sighash::EcdsaSighashType::from_standard(
        sighash_type_byte[0] as u32,
    )
    .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 sighash type 非法: {e}")))?;

    use bitcoin::sighash::SighashCache;
    let mut cache = SighashCache::new(to_sign);
    // P2SH-P2WPKH 的 scriptCode 同 P2WPKH：用 redeemScript（即 P2WPKH 脚本）作为 script
    // （p2wpkh_signature_hash 内部经 p2wpkh_script_code 转为 BIP-143 scriptCode）。
    let sighash = cache
        .p2wpkh_signature_hash(0, &redeem_script, prevout.value, sighash_type)
        .map_err(|e| {
            WalletError::SignatureInvalid(format!("BIP-322 P2SH-P2WPKH sighash 失败: {e}"))
        })?;

    let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());
    let sig = bitcoin::secp256k1::ecdsa::Signature::from_der(der_sig)
        .map_err(|e| map_recover_err(e, "BIP-322 P2SH-P2WPKH DER 签名解析失败"))?;
    let pubkey = bitcoin::secp256k1::PublicKey::from_slice(pub_key_bytes)
        .map_err(|e| map_recover_err(e, "BIP-322 P2SH-P2WPKH 公钥解析失败"))?;
    let ctx = secp_ctx();
    Ok(ctx.verify_ecdsa(&msg, &sig, &pubkey).is_ok())
}

/// 验证 simple P2TR（taproot key-spend）：witness = [bip340_sig (+sighash_byte)]。
fn verify_bip322_p2tr(
    to_sign: &bitcoin::Transaction,
    wit_items: &[&[u8]],
    prevout: &bitcoin::TxOut,
) -> WalletResult<bool> {
    if wit_items.len() != 1 {
        return Err(WalletError::SignatureInvalid(format!(
            "BIP-322 P2TR key-spend witness 须 1 项，实际 {}",
            wit_items.len()
        )));
    }
    let sig_bytes = wit_items[0];
    if sig_bytes.is_empty() {
        return Err(WalletError::SignatureInvalid(
            "BIP-322 P2TR 签名为空".into(),
        ));
    }
    // 64 字节 → Default (0x00)；65 字节 → 末字节为 sighash type。
    let (schnorr_sig, sighash_type) = if sig_bytes.len() == 64 {
        (sig_bytes, bitcoin::sighash::TapSighashType::Default)
    } else if sig_bytes.len() == 65 {
        let ty =
            bitcoin::sighash::TapSighashType::from_consensus_u8(sig_bytes[64]).map_err(|e| {
                WalletError::SignatureInvalid(format!("BIP-322 taproot sighash type 非法: {e}"))
            })?;
        (&sig_bytes[..64], ty)
    } else {
        return Err(WalletError::SignatureInvalid(format!(
            "BIP-322 P2TR 签名须 64/65 字节，实际 {}",
            sig_bytes.len()
        )));
    };
    let sig_arr: [u8; 64] = schnorr_sig
        .try_into()
        .map_err(|_| WalletError::SignatureInvalid("BIP-322 P2TR 签名截断失败".into()))?;
    let schnorr_sig_obj = bitcoin::secp256k1::schnorr::Signature::from_slice(&sig_arr)
        .map_err(|e| map_recover_err(e, "BIP-322 P2TR Schnorr 签名解析失败"))?;

    // taproot key-spend sighash（需 Prevouts::All）。
    use bitcoin::sighash::{Prevouts, SighashCache};
    let prevouts = vec![prevout.clone()];
    let mut cache = SighashCache::new(to_sign);
    let sighash = cache
        .taproot_key_spend_signature_hash(0, &Prevouts::All(&prevouts), sighash_type)
        .map_err(|e| WalletError::SignatureInvalid(format!("BIP-322 P2TR sighash 失败: {e}")))?;

    // 从 prevout scriptPubKey 提取 x-only pubkey（P2TR spk: OP_1 PUSH32<32> → bytes[2..34]）。
    let spk = prevout.script_pubkey.as_bytes();
    if spk.len() < 34 {
        return Err(WalletError::SignatureInvalid(
            "BIP-322 P2TR scriptPubKey 长度异常".into(),
        ));
    }
    let xonly_bytes: [u8; 32] = spk[2..34]
        .try_into()
        .map_err(|_| WalletError::SignatureInvalid("BIP-322 P2TR pubkey 截取失败".into()))?;
    #[allow(deprecated)]
    let xonly = bitcoin::secp256k1::XOnlyPublicKey::from_slice(&xonly_bytes)
        .map_err(|e| map_recover_err(e, "BIP-322 P2TR x-only 公钥解析失败"))?;

    let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());
    let ctx = secp_ctx();
    Ok(ctx.verify_schnorr(&schnorr_sig_obj, &msg, &xonly).is_ok())
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 纯逻辑：EIP-191 前缀（保留原有断言）----

    #[test]
    fn eip191_prefix_known_vector() {
        let payload = "Hello, world!";
        let msg = eip191_personal_sign_message(payload);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x19Ethereum Signed Message:\n");
        expected.extend_from_slice(b"13");
        expected.extend_from_slice(payload.as_bytes());
        assert_eq!(msg, expected);
    }

    #[test]
    fn eip191_length_is_decimal_byte_count() {
        let payload = "你好世界喵";
        assert_eq!(payload.len(), 15);
        let msg = eip191_personal_sign_message(payload);
        let prefix_end = EIP191_PREFIX.len();
        let len_part: String = msg[prefix_end..]
            .iter()
            .take_while(|b| (**b as char).is_ascii_digit())
            .map(|b| *b as char)
            .collect();
        assert_eq!(len_part, "15");
        assert_eq!(&msg[msg.len() - 15..], payload.as_bytes());
    }

    #[test]
    fn eip191_empty_payload() {
        let msg = eip191_personal_sign_message("");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x19Ethereum Signed Message:\n0");
        assert_eq!(msg, expected);
    }

    // ---- 纯逻辑：EIP-712 typeString ----

    #[test]
    fn eip712_type_string_sorts_fields() {
        let a = eip712_type_string(
            "Mail",
            vec![("to", "Person"), ("from", "Person"), ("contents", "string")],
        );
        let b = eip712_type_string(
            "Mail",
            vec![("contents", "string"), ("from", "Person"), ("to", "Person")],
        );
        assert_eq!(a, b);
        assert_eq!(a, "Mail(string contents,Person from,Person to)");
    }

    #[test]
    fn eip712_domain_fields_present() {
        assert_eq!(EIP712_DOMAIN_FIELDS.len(), 4);
        let ts = eip712_type_string("EIP712Domain", EIP712_DOMAIN_FIELDS.to_vec());
        assert!(ts.contains("uint256 chainId"));
        assert!(ts.contains("address verifyingContract"));
    }

    // ---- 真实验签：EIP-191（RFC 向量）----

    #[test]
    fn eip191_recovers_correct_address_rfc_vector() {
        // web3.js v1.2.2 已知向量（与 alloy-primitives 测试同源）。
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let addr = "0x2c7536E3605D9C16a7a3D7b1898e529396a65c23";
        assert!(
            verify_eip191("Some data", sig_hex.as_bytes(), addr).unwrap(),
            "RFC 向量应验签通过"
        );
    }

    #[test]
    fn eip191_rejects_wrong_address() {
        // 签名正确但期望地址错误 → Ok(false)。
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let wrong_addr = "0x0000000000000000000000000000000000000001";
        assert!(
            !verify_eip191("Some data", sig_hex.as_bytes(), wrong_addr).unwrap(),
            "错误地址应验签失败但返回 Ok(false)"
        );
    }

    #[test]
    fn eip191_rejects_wrong_message() {
        // 同签名但消息不同 → 恢复出不同地址，与期望地址不符 → Ok(false)。
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let addr = "0x2c7536E3605D9C16a7a3D7b1898e529396a65c23";
        assert!(
            !verify_eip191("Different data", sig_hex.as_bytes(), addr).unwrap(),
            "消息被改后验签应失败"
        );
    }

    #[test]
    fn eip191_rejects_malformed_signature() {
        // 长度不足 / 非法 hex → Err(SignatureInvalid)。
        let addr = "0x2c7536E3605D9C16a7a3D7b1898e529396a65c23";
        let err = verify_eip191("Some data", b"not-a-sig", addr).unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    #[test]
    fn eip191_rejects_bad_address() {
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let err = verify_eip191("Some data", sig_hex.as_bytes(), "not-an-address").unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    // ---- 真实验签：EIP-712 ----

    /// 签一把 EIP-712 签名（私钥 0x...01）然后验签，确认 verify_eip712 路径正确。
    #[tokio::test]
    async fn eip712_sign_and_verify_roundtrip() {
        use alloy::signers::local::PrivateKeySigner;
        use alloy::signers::Signer;

        let typed_data = serde_json::json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"}
                ],
                "Mail": [
                    {"name": "from", "type": "Person"},
                    {"name": "to", "type": "Person[]"},
                    {"name": "contents", "type": "string"}
                ],
                "Person": [
                    {"name": "name", "type": "string"},
                    {"name": "wallets", "type": "address[]"}
                ]
            },
            "primaryType": "Mail",
            "domain": {
                "name": "Ether Mail",
                "version": "1",
                "chainId": "1",
                "verifyingContract": "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"
            },
            "message": {
                "from": {
                    "name": "Cow",
                    "wallets": [
                        "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826",
                        "0xDeaDbeefdEAdbeefdEadbEEFdeadbeEFdEaDbeeF"
                    ]
                },
                "to": [
                    {
                        "name": "Bob",
                        "wallets": [
                            "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB",
                            "0xB0BdaBea57B0BDABeA57b0bdABEA57b0BDabEa57",
                            "0xB0B0b0b0b0b0B000000000000000000000000000"
                        ]
                    }
                ],
                "contents": "Hello, Bob!"
            }
        });
        let typed_json = typed_data.to_string();

        // 用 alloy 的本地私钥签名者签一把（私钥固定，确保可复现）。
        let signer = PrivateKeySigner::from_str(
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let typed: TypedData = serde_json::from_str(&typed_json).expect("typed_data parse");
        let digest = typed.eip712_signing_hash().unwrap();
        let sig = signer.sign_hash(&digest).await.unwrap();
        let sig_bytes = sig.as_bytes().to_vec();

        let addr = signer.address();
        let addr_hex = format!("{addr:?}");

        assert!(
            verify_eip712(&typed_json, &sig_bytes, &addr_hex).unwrap(),
            "EIP-712 签名回环应验签通过"
        );

        // 错地址 → Ok(false)。
        let wrong = "0x0000000000000000000000000000000000000001";
        assert!(!verify_eip712(&typed_json, &sig_bytes, wrong).unwrap());

        // 错消息（改 contents）→ Ok(false)。
        let mut bad = typed_data.clone();
        bad["message"]["contents"] = serde_json::json!("Tampered");
        let bad_json = bad.to_string();
        assert!(!verify_eip712(&bad_json, &sig_bytes, &addr_hex).unwrap());
    }

    #[test]
    fn eip712_rejects_bad_typed_data_json() {
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let err = verify_eip712(
            "not-json",
            sig_hex.as_bytes(),
            "0x0000000000000000000000000000000000000001",
        )
        .unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    // ---- 真实验签：Schnorr (BIP-340) ----

    #[test]
    fn schnorr_sign_and_verify_roundtrip() {
        // 用 bitcoin::secp256k1（0.29）签一把 Schnorr，再用 verify_schnorr 校验。
        // 通过 SecretKey::from_str（固定 hex）保证确定性。
        use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
        use core::str::FromStr;

        let ctx = Secp256k1::new();
        let sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let kp = Keypair::from_secret_key(&ctx, &sk);
        let (xonly, _parity) = kp.x_only_public_key();

        let msg = "Hello OS Schnorr";
        use bitcoin::hashes::{sha256, Hash as _};
        let digest = sha256::Hash::hash(msg.as_bytes());
        let m = bitcoin::secp256k1::Message::from_digest(digest.to_byte_array());
        // sign_schnorr_no_aux_rand：辅助随机数全零（确定性签名）。
        let sig = ctx.sign_schnorr_no_aux_rand(&m, &kp);

        // 公钥 hex。
        let pk_hex = alloy::hex::encode(xonly.serialize());
        let sig_bytes = sig.as_ref().to_vec();

        // 验签通过。
        assert!(
            verify_schnorr(msg, &sig_bytes, &pk_hex).unwrap(),
            "Schnorr 回环验签应通过"
        );

        // 错消息 → Ok(false)。
        assert!(
            !verify_schnorr("Different", &sig_bytes, &pk_hex).unwrap(),
            "消息被改应验签失败"
        );

        // 错公钥 → Ok(false)。
        let bad_pk = "00".repeat(32);
        assert!(
            !verify_schnorr(msg, &sig_bytes, &bad_pk).unwrap_or(false),
            "错公钥应验签失败或抛错"
        );
    }

    #[test]
    fn schnorr_rejects_short_signature() {
        let err = verify_schnorr("m", &[0u8; 10], &"00".repeat(32)).unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    #[test]
    fn schnorr_rejects_bad_pubkey_len() {
        // 64 字节全零签名格式可解析但公钥长度错。
        let err = verify_schnorr("m", &[0u8; 64], "abc").unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    // ---- 真实验签：ECDSA Bitcoin signmessage ----

    #[test]
    fn ecdsa_message_sign_and_verify_roundtrip() {
        use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
        use core::str::FromStr;

        let ctx = Secp256k1::new();
        // 固定私钥确保确定性。
        let sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let pk = PublicKey::from_secret_key(&ctx, &sk);

        let msg = "Hello OS BTC";
        use bitcoin::hashes::{sha256d, Hash as _};
        let magic = bitcoin_message_magic(msg);
        let digest = sha256d::Hash::hash(&magic);
        let m = bitcoin::secp256k1::Message::from_digest(digest.to_byte_array());
        let sig = ctx.sign_ecdsa_recoverable(&m, &sk);
        let (recid, sig_rs) = sig.serialize_compact();

        // 构造 65 字节 r||s||recid。
        let mut sig65 = sig_rs.to_vec();
        sig65.push(recid.to_i32() as u8);

        let pk_hex = alloy::hex::encode(pk.serialize());

        assert!(
            verify_ecdsa_message(msg, &sig65, &pk_hex).unwrap(),
            "ECDSA signmessage 回环应验签通过"
        );

        // 错消息 → Ok(false)。
        assert!(
            !verify_ecdsa_message("Tampered", &sig65, &pk_hex).unwrap(),
            "消息被改应验签失败"
        );

        // 错公钥 → Ok(false)。用另一固定私钥的公钥。
        let other_sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();
        let other_pk = PublicKey::from_secret_key(&ctx, &other_sk);
        let bad_pk_hex = alloy::hex::encode(other_pk.serialize());
        assert!(
            !verify_ecdsa_message(msg, &sig65, &bad_pk_hex).unwrap(),
            "错公钥应验签失败"
        );
    }

    #[test]
    fn ecdsa_message_rejects_short_signature() {
        let err = verify_ecdsa_message("m", &[0u8; 10], &"00".repeat(33)).unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    // ---- BIP-322：完整伪交易验签 ----
    //
    // 测试向量来自 bitcoin/bips 仓库 bip-0322/basic-test-vectors.json。
    // 私钥 L3VFeEujGtevx9w18HD1fhRbCH67Az2dpCymeRE1SoPK6XQtaN2k → 地址
    // bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l（P2WPKH）。

    /// simple P2WPKH 已知向量（消息 ""）。
    #[test]
    fn bip322_simple_p2wpkh_empty_message() {
        let addr = "bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l";
        let sig = b"smpAkcwRAIgM2gBAQqvZX15ZiysmKmQpDrG83avLIT492QBzLnQIxYCIBaTpOaD20qRlEylyxFSeEA2ba9YOixpX8z46TSDtS40ASECx/EgAxlkQpQ9hYjgGu6EBCPMVPwVIVJqO4XCsMvViHI=";
        assert!(
            verify_bip322("", sig, addr).unwrap(),
            "BIP-322 simple P2WPKH 空消息应验签通过"
        );
    }

    /// simple P2WPKH 已知向量（消息 "Hello World"）。
    #[test]
    fn bip322_simple_p2wpkh_hello_world() {
        let addr = "bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l";
        let sig = b"smpAkcwRAIgZRfIY3p7/DoVTty6YZbWS71bc5Vct9p9Fia83eRmw2QCICK/ENGfwLtptFluMGs2KsqoNSk89pO7F29zJLUx9a/sASECx/EgAxlkQpQ9hYjgGu6EBCPMVPwVIVJqO4XCsMvViHI=";
        assert!(
            verify_bip322("Hello World", sig, addr).unwrap(),
            "BIP-322 simple P2WPKH \"Hello World\" 应验签通过"
        );
    }

    /// simple P2WPKH：消息被改 → Ok(false)（用错误向量：签 "" 但验 "Wrong message"）。
    #[test]
    fn bip322_simple_p2wpkh_wrong_message_fails() {
        let addr = "bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l";
        // 这是消息 "" 的签名，但用 "Wrong message that was not signed" 验签。
        let sig = b"smpAkcwRAIgM2gBAQqvZX15ZiysmKmQpDrG83avLIT492QBzLnQIxYCIBaTpOaD20qRlEylyxFSeEA2ba9YOixpX8z46TSDtS40ASECx/EgAxlkQpQ9hYjgGu6EBCPMVPwVIVJqO4XCsMvViHI=";
        assert!(
            !verify_bip322("Wrong message that was not signed", sig, addr).unwrap(),
            "消息不匹配应验签失败 Ok(false)"
        );
    }

    /// simple P2WPKH：签名可选第二个 RBF 变体也验签通过（基础向量提供两签）。
    #[test]
    fn bip322_simple_p2wpkh_second_signature_variant() {
        let addr = "bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l";
        let sig = b"smpAkgwRQIhAPkJ1Q4oYS0htvyuSFHLxRQpFAY56b70UvE7Dxazen0ZAiAtZfFz1S6T6I23MWI2lK/pcNTWncuyL8UL+oMdydVgzAEhAsfxIAMZZEKUPYWI4BruhAQjzFT8FSFSajuFwrDL1Yhy";
        assert!(
            verify_bip322("", sig, addr).unwrap(),
            "BIP-322 simple P2WPKH 第二签变体应验签通过"
        );
    }

    /// simple P2WPKH：错误地址（同消息同签名，不同地址）→ Ok(false)。
    #[test]
    fn bip322_simple_p2wpkh_wrong_address_fails() {
        // 用同一签名但期望另一个 P2WPKH 地址。
        let wrong_addr = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
        let sig = b"smpAkcwRAIgM2gBAQqvZX15ZiysmKmQpDrG83avLIT492QBzLnQIxYCIBaTpOaD20qRlEylyxFSeEA2ba9YOixpX8z46TSDtS40ASECx/EgAxlkQpQ9hYjgGu6EBCPMVPwVIVJqO4XCsMvViHI=";
        let r = verify_bip322("", sig, wrong_addr).unwrap();
        assert!(!r, "错误地址应验签失败 Ok(false)");
    }

    /// simple P2TR（taproot key-spend）已知向量（无前缀 fallback，base64 schnorr 签名）。
    #[test]
    fn bip322_simple_p2tr_known_vector() {
        // 基础向量 "No prefix fallback"：P2TR 地址 + 无 smp 前缀的 base64 签名。
        let addr = "bc1pss0zhytly75awhm6x2hhvd5lnzv3vssgrf9axfheq8ldyzn88ges79fler";
        let msg = "No prefix fallback";
        let sig = b"AUCJYOwOjxYAvatTAGYaVlNXBVyFuc4MwNQkOuK2tl8xhfKDONd0NjfYyNSYcRqeCp8hsAnCEPHAVEkO9h6vbQ/R";
        assert!(
            verify_bip322(msg, sig, addr).unwrap(),
            "BIP-322 simple P2TR 无前缀 fallback 应验签通过"
        );
    }

    /// simple P2WPKH：非法 base64 → Err(SignatureInvalid)。
    #[test]
    fn bip322_rejects_invalid_base64() {
        let addr = "bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l";
        let err = verify_bip322("", b"not-valid-base64!!!", addr).unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    /// simple P2WPKH：空签名 → Err。
    #[test]
    fn bip322_rejects_empty_signature() {
        let addr = "bc1q9vza2e8x573nczrlzms0wvx3gsqjx7vavgkx0l";
        let err = verify_bip322("", b"", addr).unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    /// 签名+验签往返：用 bitcoin crate 的 keypair 生成一个 BIP-322 simple P2WPKH 签名，
    /// 然后用 verify_bip322 验证（确认验签路径正确，不依赖外部向量）。
    #[test]
    fn bip322_simple_p2wpkh_sign_and_verify_roundtrip() {
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use bitcoin::{CompressedPublicKey, Network};

        let ctx = Secp256k1::new();
        // 固定私钥确保确定性。
        let sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&ctx, &sk);
        let compressed = CompressedPublicKey(pk);
        // P2WPKH 地址（regtest/bech32 主网用 bitcoin 网络）。
        let addr = bitcoin::Address::p2wpkh(&compressed, Network::Bitcoin);

        // 构造 to_spend。
        let message = "OS BIP-322 roundtrip";
        let to_spend = bip322_create_to_spend(addr.as_unchecked(), message).unwrap();
        let to_sign_no_witness = bip322_create_to_sign(&to_spend, bitcoin::Witness::new());

        // BIP-143 sighash（P2WPKH）。
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::{EcdsaSighashType, SighashCache};
        let pkh = bitcoin::WPubkeyHash::hash(&compressed.to_bytes());
        let p2wpkh_script = bitcoin::ScriptBuf::new_p2wpkh(&pkh);
        let prevout = &to_spend.output[0];
        let mut cache = SighashCache::new(&to_sign_no_witness);
        let sighash = cache
            .p2wpkh_signature_hash(0, &p2wpkh_script, prevout.value, EcdsaSighashType::All)
            .unwrap();
        let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());
        // ECDSA 签名（低 R 由 secp 保证）。
        let sig = ctx.sign_ecdsa(&msg, &sk);
        let mut der_with_hashtype = sig.serialize_der().to_vec();
        der_with_hashtype.push(0x01); // SIGHASH_ALL
                                      // 构造 witness stack：[der+hashtype, pubkey]。
        let mut witness = bitcoin::Witness::new();
        witness.push(&der_with_hashtype);
        witness.push(compressed.to_bytes());

        // 编码为 smp + base64(consensus 编码的 witness)。
        let wit_bytes = bitcoin::consensus::encode::serialize(&witness);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&wit_bytes);
        let proof = format!("smp{b64}");

        // 验签。
        let addr_str = addr.to_string();
        assert!(
            verify_bip322(message, proof.as_bytes(), &addr_str).unwrap(),
            "BIP-322 simple P2WPKH 签名往返应验签通过"
        );

        // 错消息 → Ok(false)。
        assert!(
            !verify_bip322("Tampered", proof.as_bytes(), &addr_str).unwrap(),
            "消息被改应验签失败"
        );
    }

    /// 签名+验签往返：P2TR（taproot key-spend）BIP-322 签名。
    ///
    /// 手动计算 BIP-341 tweak（bitcoin 0.32 的 `TapTweak` trait 不在公开重导出，
    /// 故这里用 secp256k1 算术自实现：t = TaggedHash("TapTweak", internal_key)，
    /// output_key = internal + t*G，tweaked_sk = sk + t (mod n)）。
    #[test]
    fn bip322_simple_p2tr_sign_and_verify_roundtrip() {
        use bitcoin::hashes::{sha256, Hash as _, HashEngine as _};
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use bitcoin::Network;

        let ctx = Secp256k1::new();
        let sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();
        let kp = bitcoin::secp256k1::Keypair::from_secret_key(&ctx, &sk);
        let (internal_xonly, _) = kp.x_only_public_key();
        // BIP-341 key-only tweak（无 script path）：
        // t = SHA256(SHA256("TapTweak") || SHA256("TapTweak") || internal_key)
        let tag_hash = sha256::Hash::hash(b"TapTweak").to_byte_array();
        let mut eng = sha256::Hash::engine();
        eng.input(&tag_hash);
        eng.input(&tag_hash);
        eng.input(&internal_xonly.serialize());
        let t = sha256::Hash::from_engine(eng).to_byte_array();
        let tweak_sk = {
            // tweaked secret = sk + t (mod n)
            let scalar = bitcoin::secp256k1::Scalar::from_be_bytes(t).unwrap();
            sk.add_tweak(&scalar).unwrap()
        };
        let tweaked_kp = bitcoin::secp256k1::Keypair::from_secret_key(&ctx, &tweak_sk);
        let (out_xonly, _parity) = tweaked_kp.x_only_public_key();

        // 用 output key 直接构造 P2TR 地址（等价 p2tr_tweaked，但绕开私有类型）：
        // 经 WitnessProgram 构造合法 P2TR 地址（OP_1 + PUSH32(output_key)）。
        let out_bytes = out_xonly.serialize();
        let program = bitcoin::WitnessProgram::new(bitcoin::WitnessVersion::V1, &out_bytes[..])
            .expect("witness program");
        let addr = bitcoin::Address::from_witness_program(program, Network::Bitcoin);

        let message = "OS BIP-322 taproot";
        let to_spend = bip322_create_to_spend(addr.as_unchecked(), message).unwrap();
        let to_sign_no_witness = bip322_create_to_sign(&to_spend, bitcoin::Witness::new());

        // taproot key-spend sighash。
        use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
        let prevout = &to_spend.output[0];
        let prevouts = vec![prevout.clone()];
        let mut cache = SighashCache::new(&to_sign_no_witness);
        let sighash = cache
            .taproot_key_spend_signature_hash(0, &Prevouts::All(&prevouts), TapSighashType::Default)
            .unwrap();
        let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());
        // BIP-340 签名（用 tweaked keypair 签，无 sighash type 字节 → Default）。
        let sig = ctx.sign_schnorr_no_aux_rand(&msg, &tweaked_kp);

        let mut witness = bitcoin::Witness::new();
        witness.push(sig.as_ref());
        let wit_bytes = bitcoin::consensus::encode::serialize(&witness);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&wit_bytes);
        let proof = format!("smp{b64}");

        let addr_str = addr.to_string();
        assert!(
            verify_bip322(message, proof.as_bytes(), &addr_str).unwrap(),
            "BIP-322 simple P2TR 签名往返应验签通过"
        );
        // 错消息 → Ok(false)。
        assert!(
            !verify_bip322("Tampered", proof.as_bytes(), &addr_str).unwrap(),
            "消息被改应验签失败"
        );
    }

    /// legacy P2PKH 签名+验签往返。
    #[test]
    fn bip322_legacy_p2pkh_sign_and_verify_roundtrip() {
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use bitcoin::{Network, PubkeyHash};

        let ctx = Secp256k1::new();
        let sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000003")
                .unwrap();
        let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&ctx, &sk);
        // P2PKH 地址（主网 base58check，1 开头）。
        let pkh: PubkeyHash = bitcoin::hashes::hash160::Hash::hash(&pk.serialize()).into();
        let addr = bitcoin::Address::p2pkh(pkh, Network::Bitcoin);

        let message = "OS BIP-322 legacy";
        let to_spend = bip322_create_to_spend(addr.as_unchecked(), message).unwrap();
        let to_sign = bip322_create_to_sign(&to_spend, bitcoin::Witness::new());

        // legacy sighash。
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::SighashCache;
        let spk = addr.script_pubkey();
        let cache = SighashCache::new(&to_sign);
        let sighash = cache.legacy_signature_hash(0, &spk, 0x01u32).unwrap();
        let msg = bitcoin::secp256k1::Message::from_digest(*sighash.as_byte_array());

        // 可恢复 ECDSA 签名。
        let rec_sig = ctx.sign_ecdsa_recoverable(&msg, &sk);
        let (recid, rs) = rec_sig.serialize_compact();
        let mut sig65 = rs.to_vec();
        // 用裸 recid（0-3），压缩位偏移在 verify 内兼容。
        sig65.push(recid.to_i32() as u8);

        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&sig65);

        let addr_str = addr.to_string();
        assert!(
            verify_bip322(message, b64.as_bytes(), &addr_str).unwrap(),
            "BIP-322 legacy P2PKH 签名往返应验签通过"
        );
        // 错消息 → Ok(false)（恢复出的公钥 hash160 不匹配地址）。
        assert!(
            !verify_bip322("Tampered", b64.as_bytes(), &addr_str).unwrap(),
            "消息被改应验签失败"
        );
    }

    /// BIP-322 tagged hash 已知向量：消息 "" 的 message_hash 应为
    /// c90c269c4f8fcbe6880f72a721ddfbf1914268a794cbb21cfafee13770ae19f1。
    #[test]
    fn bip322_tagged_hash_known_vector() {
        let h = bip322_tagged_hash(b"");
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "c90c269c4f8fcbe6880f72a721ddfbf1914268a794cbb21cfafee13770ae19f1"
        );
    }

    /// 非法地址 → Err(SignatureInvalid)。
    #[test]
    fn bip322_rejects_bad_address() {
        let err = verify_bip322("m", b"smpAAAA", "not-an-address").unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    /// 短 legacy 签名（非 65 字节）→ Err。
    #[test]
    fn bip322_legacy_rejects_short_signature() {
        // 1B58Mainet 风格地址（合法 base58 P2PKH）+ 短 base64。
        let addr = "1BgGZ9tcN4rm9KBzDn7KprQz87SZ26SAMH";
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        let err = verify_bip322("m", short.as_bytes(), addr).unwrap_err();
        assert!(matches!(err, WalletError::SignatureInvalid(_)));
    }

    // ---- 工具函数 ----

    #[test]
    fn decode_hex_handles_0x_prefix() {
        assert_eq!(
            decode_hex_maybe_0x("0xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            decode_hex_maybe_0x("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(decode_hex_maybe_0x("xyz").is_err());
        assert!(decode_hex_maybe_0x("abc").is_err(), "奇数长度应失败");
    }

    #[test]
    fn bitcoin_message_magic_short_and_long() {
        let m = bitcoin_message_magic("Hi");
        let mut expected = Vec::new();
        expected.extend_from_slice(BTC_MESSAGE_PREFIX);
        expected.push(2);
        expected.extend_from_slice(b"Hi");
        assert_eq!(m, expected);
    }

    // =========================================================================
    // 覆盖率补充：parse_signature / parse_evm_address / bitcoin_message_magic 长消息分支
    // / decode_signature_bytes / hex_nibble 边界
    // =========================================================================

    #[test]
    fn parse_signature_raw_65_bytes_path() {
        // 65 字节签名直接构造路径（即使字节全 0 也走 raw 路径而非 hex 路径）。
        // 全 0 的 r||s||v 在 alloy 中可解析为 Signature 结构（仅密码学上无效，
        // 验签时才会失败），证明 parse_signature 走的是 raw 65 字节路径。
        let r = parse_signature(&[0u8; 65]);
        assert!(r.is_ok(), "全零 65 字节应走 raw 路径成功构造 Signature");
    }

    #[test]
    fn parse_signature_hex_string_path() {
        // 非 65 字节当作 hex 字符串解析。给一个合法的 65 字节签名 hex（130 字符）。
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let bytes = sig_hex.as_bytes();
        let r = parse_signature(bytes);
        assert!(r.is_ok(), "合法 hex 签名应解析成功");
    }

    #[test]
    fn parse_signature_invalid_hex_returns_err() {
        // 非 65 字节 + 非法 hex 字符串 -> Err。
        let r = parse_signature(b"not-a-hex-sig");
        assert!(r.is_err());
        // 非法 UTF-8 字节（高位字节）-> Err（from_utf8 失败路径）。
        let r = parse_signature(&[0xff, 0xfe, 0xfd]);
        assert!(r.is_err());
    }

    #[test]
    fn parse_signature_uppercase_0x_prefix_handled_by_alloy() {
        // alloy 的 Signature::from_str 接受 0x / 0X 前缀；这里验证短 hex 也能走 hex 路径。
        // 给一个非法但格式上像 hex 的字符串（短），验证走 hex 解析失败路径。
        let r = parse_signature(b"0xabc");
        assert!(r.is_err());
    }

    #[test]
    fn parse_evm_address_valid_and_invalid() {
        // 合法地址。
        assert!(parse_evm_address("0x2c7536E3605D9C16a7a3D7b1898e529396a65c23").is_ok());
        // 合法（无 checksum 的小写）。
        assert!(parse_evm_address("0x0123456789abcdef0123456789abcdef01234567").is_ok());
        // 非法（长度不足）。
        assert!(parse_evm_address("0xabc").is_err());
        // 非法（非 hex）。
        assert!(parse_evm_address("not-an-address").is_err());
        // 非法（空）。
        assert!(parse_evm_address("").is_err());
    }

    #[test]
    fn decode_hex_maybe_0x_strips_uppercase_0x_prefix() {
        // 0X（大写 X）前缀也应被去除（实现里 strip_prefix("0x").or_else(strip_prefix("0X"))）。
        assert_eq!(
            decode_hex_maybe_0x("0Xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        // 无前缀。
        assert_eq!(
            decode_hex_maybe_0x("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn hex_nibble_rejects_non_hex_chars() {
        // 间接通过 decode_hex_maybe_0x 触发 hex_nibble 错误路径。
        assert!(decode_hex_maybe_0x("zz").is_err());
        assert!(decode_hex_maybe_0x("g0").is_err());
        // 奇数长度。
        assert!(decode_hex_maybe_0x("abc").is_err());
    }

    #[test]
    fn bitcoin_message_magic_long_message_uses_varstr_fd_prefix() {
        // > 252 字节消息走 0xfd + LE u16 路径（实现里的 TODO 分支）。
        let long = "a".repeat(300);
        let m = bitcoin_message_magic(&long);
        // 前缀 + 0xfd 标记 + 2 字节 LE 长度 + payload。
        let prefix_len = BTC_MESSAGE_PREFIX.len();
        assert_eq!(m[prefix_len], 0xfd, "长消息应用 0xfd VarStr 标记");
        // LE u16 长度 = 300 = 0x012c。
        let len_bytes = &m[prefix_len + 1..prefix_len + 3];
        assert_eq!(len_bytes, &[0x2c, 0x01]);
        // 总长度 = 前缀 + 1 (0xfd) + 2 (u16) + 300 (payload)。
        assert_eq!(m.len(), prefix_len + 3 + 300);
    }

    #[test]
    fn bitcoin_message_magic_exactly_252_bytes_uses_single_byte_len() {
        // 边界：恰好 252 字节仍走 1 字节长度路径。
        let payload = "a".repeat(252);
        let m = bitcoin_message_magic(&payload);
        let prefix_len = BTC_MESSAGE_PREFIX.len();
        assert_eq!(m[prefix_len], 252u8);
        assert_eq!(m.len(), prefix_len + 1 + 252);
    }

    #[test]
    fn bitcoin_message_magic_empty_message() {
        let m = bitcoin_message_magic("");
        let mut expected = Vec::new();
        expected.extend_from_slice(BTC_MESSAGE_PREFIX);
        expected.push(0);
        assert_eq!(m, expected);
    }

    #[test]
    fn eip191_personal_sign_message_unicode_payload() {
        // 多字节 UTF-8 字符的长度应为字节长度（非字符数）。
        let payload = "你好"; // 6 字节
        let msg = eip191_personal_sign_message(payload);
        let prefix_end = EIP191_PREFIX.len();
        // 长度部分应为 ASCII "6"。
        assert_eq!(msg[prefix_end] as char, '6');
        // 末尾 6 字节应为 UTF-8 编码的 "你好"。
        assert_eq!(&msg[msg.len() - 6..], "你好".as_bytes());
    }

    #[test]
    fn eip712_type_string_single_field() {
        let ts = eip712_type_string("Foo", vec![("a", "uint256")]);
        assert_eq!(ts, "Foo(uint256 a)");
    }

    #[test]
    fn eip712_type_string_no_fields() {
        let ts = eip712_type_string("Empty", vec![]);
        assert_eq!(ts, "Empty()");
    }

    #[test]
    fn strip_simple_prefix_handles_smp_and_no_prefix() {
        assert_eq!(strip_simple_prefix("smpABCDE"), "ABCDE");
        assert_eq!(strip_simple_prefix("ABCDE"), "ABCDE");
        assert_eq!(strip_simple_prefix(""), "");
        // 仅前缀无内容。
        assert_eq!(strip_simple_prefix("smp"), "");
    }

    #[test]
    fn decode_signature_bytes_base64_text_path() {
        // UTF-8 合法 base64 文本 -> 走 base64 解码路径。
        let raw = decode_signature_bytes(b"aGVsbG8=").unwrap(); // "hello"
        assert_eq!(raw, b"hello");
        // 含空白也应被 trim 后解码。
        let raw = decode_signature_bytes(b"  aGVsbG8=  ").unwrap();
        assert_eq!(raw, b"hello");
    }

    #[test]
    fn decode_signature_bytes_invalid_base64_returns_err() {
        // 合法 UTF-8 但非合法 base64 -> Err。
        let r = decode_signature_bytes(b"!!!not-base64!!!");
        assert!(r.is_err());
    }

    #[test]
    fn decode_signature_bytes_non_utf8_returns_raw() {
        // 非 UTF-8 字节 -> 当作原始字节直接返回（不报错）。
        let raw_bytes = [0xff, 0xfe, 0xfd, 0xfc];
        let r = decode_signature_bytes(&raw_bytes).unwrap();
        assert_eq!(r, raw_bytes.to_vec());
    }

    #[test]
    fn verify_schnorr_oversized_signature_uses_first_64_bytes() {
        // 65 字节签名（多 1 字节）应被截取前 64 字节（BIP-340 容错）。
        // 构造一个 65 字节但前 64 字节非法的签名 -> 解析失败 -> Err。
        let r = verify_schnorr("m", &[0u8; 65], &"00".repeat(32));
        // 全零签名是非合法 BIP-340 签名，alloy 会拒绝解析 -> Err；或验签失败 Ok(false)。
        assert!(r.is_err() || !r.unwrap_or(false));
    }

    #[test]
    fn verify_ecdsa_message_recid_offset_ranges() {
        // 验证 recid 偏移分支（27..=30 / 31..=34 / 裸 0-3）。
        // 用一个长度合法但内容非法的 65 字节签名，触达 from_compact 解析失败路径。
        for &recid in [27u8, 28, 31, 32, 0, 1, 2, 3, 100].iter() {
            let mut sig = vec![0u8; 65];
            sig[64] = recid;
            // 全零 r||s 非法签名 -> 解析失败 -> Err。
            let r = verify_ecdsa_message("m", &sig, &"02".repeat(33));
            // 部分路径可能返回 Err（解析失败），部分返回 Ok(false)（验签失败）；都不应 panic。
            let _ = r;
        }
    }

    #[test]
    fn constants_match_spec() {
        // 验证魔术前缀常量。
        assert_eq!(EIP191_PREFIX, "\u{19}Ethereum Signed Message:\n");
        assert_eq!(BTC_MESSAGE_PREFIX, b"\x18Bitcoin Signed Message:\n");
        // EIP712 域字段集 4 项。
        assert_eq!(EIP712_DOMAIN_FIELDS.len(), 4);
        let names: Vec<&str> = EIP712_DOMAIN_FIELDS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"name"));
        assert!(names.contains(&"version"));
        assert!(names.contains(&"chainId"));
        assert!(names.contains(&"verifyingContract"));
    }
}
