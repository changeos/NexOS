//! TOTP（RFC 6238）——时间窗口计算与校验逻辑。
//!
//! 设计：
//! - **时间窗口 / 步长 / 计数器推导**（`time_step_counter`/`is_within_window`）：
//!   纯算法，自实现，有完整单元测试。这是 2FA 校验的核心业务逻辑，
//!   与具体 HMAC 实现无关，可被 `TotpTwoFactor` 与 mock 复用。
//! - **HMAC-SHA1 → 6 位 code**（`generate_code`）：RFC 6238 §4 的 dynamic truncation
//!   纯算法部分已实现（输入 HMAC 原始字节即得 code）；
//!   `compute_hmac_sha1` 用 `hmac` + `sha1` crate 真实实现（RFC 2104 HMAC）。
//!
//! 默认参数：SHA1 + 30s 步长 + 6 位 code（与 Google Authenticator 兼容）。

/// 默认时间步长（秒）——RFC 6238 推荐值，与多数验证器 App 兼容。
pub const DEFAULT_STEP: u64 = 30;
/// 默认 TOTP code 位数。
pub const DEFAULT_DIGITS: u32 = 6;
/// 默认允许的前后窗口数（防时钟漂移）——前后各 1 个窗口（即 ±30s）。
pub const DEFAULT_WINDOW: u32 = 1;

/// 由 Unix 时间戳推导 TOTP 计数器（时间窗口序号）。
///
/// `counter = unix_seconds / step`。对 `step == 0` 返回错误（防御性）。
pub fn time_step_counter(unix_seconds: i64, step: u64) -> Result<u64, crate::SecurityError> {
    if step == 0 {
        return Err(crate::SecurityError::Internal("TOTP step 不能为 0".into()));
    }
    if unix_seconds < 0 {
        return Err(crate::SecurityError::Internal("TOTP 时间戳不能为负".into()));
    }
    Ok((unix_seconds as u64) / step)
}

/// 判断 `provided_counter` 是否落在以 `current_counter` 为中心、半径 `window` 的窗口内。
///
/// 用于校验：用户提交的 code 对应的计数器若在 `[current - window, current + window]`
/// 之间则接受（防时钟漂移）。`window == 0` 表示严格匹配当前窗口。
pub fn is_within_window(current_counter: u64, provided_counter: u64, window: u32) -> bool {
    let w = window as u64;
    let lo = current_counter.saturating_sub(w);
    let hi = current_counter.saturating_add(w);
    provided_counter >= lo && provided_counter <= hi
}

/// 校验 6 位 code 字符串格式——必须为恰好 `digits` 位的数字。
pub fn validate_code_format(code: &str, digits: u32) -> bool {
    code.len() == digits as usize && code.bytes().all(|b| b.is_ascii_digit())
}

/// 由 HMAC-SHA1 的 20 字节输出，按 RFC 6238 §4 dynamic truncation 推导 `digits` 位 code。
///
/// 这是纯算法：输入满足 RFC 的 HMAC-SHA1 原始字节即得标准 code。
/// 调用方需先通过 `compute_hmac_sha1` 得到 `hmac_bytes`（见下，当前阻塞）。
pub fn dynamic_truncation(hmac_bytes: &[u8], digits: u32) -> Result<u32, crate::SecurityError> {
    if hmac_bytes.len() < 20 {
        return Err(crate::SecurityError::Internal(
            "HMAC-SHA1 输出不足 20 字节".into(),
        ));
    }
    // RFC 4226 §5.3：offset 取最后一个字节的低 4 位。
    let offset = (hmac_bytes[19] & 0x0f) as usize;
    let truncated: u32 = ((u32::from(hmac_bytes[offset]) & 0x7f) << 24)
        | (u32::from(hmac_bytes[offset + 1]) << 16)
        | (u32::from(hmac_bytes[offset + 2]) << 8)
        | u32::from(hmac_bytes[offset + 3]);
    Ok(truncated % 10u32.pow(digits))
}

/// 计算 HOTP/TOTP 用的 HMAC-SHA1（RFC 2104）。
///
/// 真实实现：用 `hmac` + `sha1` crate，以 `counter` 的大端 8 字节表示作为消息，
/// 输出固定 20 字节摘要。任何 key 长度均可（HMAC 内部做 padding/ hashing）。
pub fn compute_hmac_sha1(key: &[u8], counter: u64) -> Result<[u8; 20], crate::SecurityError> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key)
        .map_err(|e| crate::SecurityError::Internal(format!("HMAC key 非法: {e}")))?;
    mac.update(&counter.to_be_bytes());
    let bytes = mac.finalize().into_bytes();
    // HMAC-SHA1 输出固定 20 字节；切片拷贝进定长数组。
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// 由 secret + Unix 时间戳生成 `digits` 位 TOTP code。
///
/// 组合：`time_step_counter` → `compute_hmac_sha1` → `dynamic_truncation`。
/// 默认 step = 30s（与 `DEFAULT_STEP` 一致）。
pub fn generate_code(
    key: &[u8],
    unix_seconds: i64,
    step: u64,
    digits: u32,
) -> Result<u32, crate::SecurityError> {
    let counter = time_step_counter(unix_seconds, step)?;
    let hmac = compute_hmac_sha1(key, counter)?;
    dynamic_truncation(&hmac, digits)
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_step_30() {
        assert_eq!(time_step_counter(0, 30).unwrap(), 0);
        assert_eq!(time_step_counter(29, 30).unwrap(), 0);
        assert_eq!(time_step_counter(30, 30).unwrap(), 1);
        assert_eq!(time_step_counter(59, 30).unwrap(), 1);
        assert_eq!(time_step_counter(60, 30).unwrap(), 2);
        // RFC 6238 测试向量时间戳（59 → window 1）
        assert_eq!(time_step_counter(59, 30).unwrap(), 1);
        assert_eq!(time_step_counter(1111111109, 30).unwrap(), 37037036);
    }

    #[test]
    fn counter_zero_step_rejected() {
        assert!(time_step_counter(100, 0).is_err());
    }

    #[test]
    fn counter_negative_rejected() {
        assert!(time_step_counter(-1, 30).is_err());
    }

    #[test]
    fn window_strict_match() {
        assert!(is_within_window(100, 100, 0));
        assert!(!is_within_window(100, 101, 0));
        assert!(!is_within_window(100, 99, 0));
    }

    #[test]
    fn window_with_tolerance() {
        // window=1：[99,101]
        assert!(is_within_window(100, 99, 1));
        assert!(is_within_window(100, 100, 1));
        assert!(is_within_window(100, 101, 1));
        assert!(!is_within_window(100, 98, 1));
        assert!(!is_within_window(100, 102, 1));
    }

    #[test]
    fn window_saturating_low() {
        // current=0, window=1：下界 saturating 到 0
        assert!(is_within_window(0, 0, 1));
        assert!(is_within_window(0, 1, 1));
        assert!(!is_within_window(0, 2, 1));
    }

    #[test]
    fn code_format_ok() {
        assert!(validate_code_format("000000", 6));
        assert!(validate_code_format("123456", 6));
        assert!(validate_code_format("00000000", 8));
    }

    #[test]
    fn code_format_bad() {
        assert!(!validate_code_format("12345", 6)); // 太短
        assert!(!validate_code_format("1234567", 6)); // 太长
        assert!(!validate_code_format("12a456", 6)); // 非数字
        assert!(!validate_code_format("", 6));
        assert!(!validate_code_format(" 12345", 6)); // 含空格
    }

    #[test]
    fn truncation_too_short_rejected() {
        assert!(dynamic_truncation(&[0u8; 10], 6).is_err());
    }

    #[test]
    fn truncation_deterministic_and_bounded() {
        // 固定 20 字节输入，结果应稳定且 < 10^6。
        let h = [0xabu8; 20];
        let c = dynamic_truncation(&h, 6).unwrap();
        assert!(c < 1_000_000);
        // 同输入同输出
        assert_eq!(dynamic_truncation(&h, 6).unwrap(), c);
    }

    #[test]
    fn hmac_sha1_known_vector() {
        // RFC 4226 §4 演示用 secret："12345678901234567890"（ASCII）。
        // counter = 1：HOTP code 已知为 287082，故 dynamic_truncation(hmac, 6) == 287082。
        // 这里验证 HMAC 输出经 dynamic_truncation 后与 RFC 公开测试向量一致。
        let key = b"12345678901234567890";
        let h = compute_hmac_sha1(key, 1).expect("hmac ok");
        assert_eq!(h.len(), 20, "HMAC-SHA1 输出应为 20 字节");
        let code = dynamic_truncation(&h, 6).unwrap();
        assert_eq!(code, 287082, "counter=1 HOTP 应为 287082（RFC 4226）");
    }

    #[test]
    fn hmac_sha1_rfc4226_sequence() {
        // RFC 4226 附录 B 完整 HOTP 测试向量（counter 0..9）。
        let key = b"12345678901234567890";
        let expected = [
            755224u32, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (counter, want) in expected.iter().enumerate() {
            let h = compute_hmac_sha1(key, counter as u64).unwrap();
            let code = dynamic_truncation(&h, 6).unwrap();
            assert_eq!(code, *want, "counter={counter} HOTP 不匹配 RFC 4226");
        }
    }

    #[test]
    fn totp_rfc6238_vectors_sha1() {
        // RFC 6238 §B 测试向量（SHA1 模式，secret 为 20 字节 ASCII "12345678901234567890"）。
        // 注意 RFC 用 Base32 解码后的 20 字节；ASCII 形式恰好就是这 20 字节。
        let key = b"12345678901234567890";
        // (Unix 时间戳, 期望 8 位 code)
        let cases: &[(i64, u32)] = &[
            (59, 94287082),
            (1111111109, 7081804),
            (1111111111, 14050471),
            (1234567890, 89005924),
            (2000000000, 69279037),
            (20000000000, 65353130),
        ];
        for (ts, want) in cases {
            let code = generate_code(key, *ts, DEFAULT_STEP, 8).expect("code ok");
            assert_eq!(
                code, *want,
                "T=0x{ts:X} TOTP(SHA1, 8 位) 应为 {want}（RFC 6238）"
            );
        }
    }

    #[test]
    fn totp_default_digits_six() {
        // 6 位 code 应 < 10^6，且同一窗口内多次计算结果一致。
        let key = b"12345678901234567890";
        let a = generate_code(key, 1234567890, DEFAULT_STEP, DEFAULT_DIGITS).unwrap();
        assert!(a < 1_000_000);
        let b = generate_code(key, 1234567890, DEFAULT_STEP, DEFAULT_DIGITS).unwrap();
        assert_eq!(a, b, "同一时间窗口应稳定");
    }

    #[test]
    fn truncation_known_vector() {
        // 构造：h[19] 低 4 位 = 1 → offset=1。
        // truncated = (h[1]&0x7f)<<24 | h[2]<<16 | h[3]<<8 | h[4]
        // 令 h[1]=0x80 → &0x7f=0；h[2]=h[3]=0；h[4]=5 → truncated=5 → code=000005
        let mut h = [0u8; 20];
        h[1] = 0x80;
        h[4] = 5;
        h[19] = 0x01;
        assert_eq!(dynamic_truncation(&h, 6).unwrap(), 5);
    }
}
