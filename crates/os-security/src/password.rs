//! 密码哈希与校验——常量时间比较 + Argon2id 真实实现。
//!
//! 设计：
//! - **常量时间比较**（`constant_time_eq`）：纯算法，自实现，防 timing attack，
//!   有完整单元测试。这是认证链路的核心防御，不依赖任何外部 crate。
//! - **哈希/验证**（`hash_password`/`verify_password`）：真实 Argon2id 实现，
//!   基于 `argon2` crate（workspace 已注册，ADR-DEPS-001）。产出/解析 PHC 字符串，
//!   由 `DbAuthProvider` 调用。
//!
//! 安全约束：本模块不持久化任何值；明文密码仅在参数中流转，绝不进入返回值或日志。

/// 常量时间字节序列比较——防 timing attack。
///
/// 特性：
/// - 运行时间仅依赖两个切片的**长度**，不依赖内容（即使长度不同也走完整比较）。
/// - 长度不等时返回 `false`，但同样耗费与等长比较相近的迭代次数（取较大长度）。
/// - 纯算法，无外部依赖；用作 `verify_password` 的底层比较原语。
///
/// 实现说明：用累加 XOR 差值的方式，遍历到 `max(a.len, b.len)`，任何提前返回都会
/// 泄露长度信息外的内容差异位置——故严格不提前 return。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff: u8 = 0;
    let len_eq = a.len() == b.len();
    // 将长度差异折叠进 diff，保证长度不等时最终结果必为 false。
    diff |= (a.len() as u8).wrapping_sub(b.len() as u8);
    let n = a.len().max(b.len());
    for i in 0..n {
        // 越界取 0：长度差已记录在 diff 中，此处只做内容差异累积。
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    len_eq && diff == 0
}

/// 校验密码（哈希比对）。
///
/// 真实实现：解析 `stored_hash`（Argon2id PHC 字符串）→ 用 argon2 crate 验证。
/// 解析失败或哈希不匹配均返回 `false`（不区分错误类型，防侧信道）。
///
/// 兼容性：若 `stored_hash` 不是合法 PHC 字符串（如 mock 路径的明文/任意占位），
/// 回退到 `constant_time_eq` 直接比较——仅供 mock/测试流转，不应出现在生产路径。
pub fn verify_password(plaintext: &str, stored_hash: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    // 先尝试 PHC 解析：合法 Argon2id 哈希走标准验证路径。
    if let Ok(parsed) = PasswordHash::new(stored_hash) {
        return match Argon2::default().verify_password(plaintext.as_bytes(), &parsed) {
            Ok(()) => true,
            Err(argon2::password_hash::Error::Password) => false,
            // 其他错误（参数不合法等）视为校验失败，不向上抛错（保持 bool 契约）。
            Err(_) => false,
        };
    }
    // 回退：非 PHC 字符串（mock 路径）走常量时间比较。
    constant_time_eq(plaintext.as_bytes(), stored_hash.as_bytes())
}

/// 哈希密码（产出 Argon2id PHC 字符串）。
///
/// 使用 `argon2` crate 默认参数（Argon2id，OWASP 推荐档：m=19456 KiB / t=2 / p=1）
/// 与随机 19 字节 salt，产出形如 `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>` 的
/// PHC 字符串。同一明文多次哈希结果不同（随机 salt）。
pub fn hash_password(plaintext: &str) -> Result<String, crate::SecurityError> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::Argon2;
    use rand::rngs::OsRng;
    // OsRng 是密码学安全 RNG；SaltString 内部用 OsRng 生成 19 字节随机 salt。
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| crate::SecurityError::Internal(format!("argon2 哈希失败: {e}")))?;
    Ok(hash.to_string())
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_equal() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(&[1u8, 2, 3], &[1, 2, 3]));
    }

    #[test]
    fn ct_eq_unequal_content() {
        assert!(!constant_time_eq(b"abcdef", b"abcdez"));
        // 单字节差异
        assert!(!constant_time_eq(b"x", b"y"));
        let other: Vec<u8> = [0u8; 31].iter().chain(&[1u8]).copied().collect();
        assert!(!constant_time_eq(&[0u8; 32], &other));
    }

    #[test]
    fn ct_eq_unequal_length() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn ct_eq_prefix_no_short_circuit() {
        // 前缀相同但长度不同——必须返回 false（不能因前缀匹配就 true）。
        assert!(!constant_time_eq(b"hello", b"hello world"));
    }

    #[test]
    fn verify_password_fallback_plaintext() {
        // 非 PHC 字符串（mock 路径）回退到常量时间比较。
        assert!(verify_password("s3cret", "s3cret"));
        assert!(!verify_password("s3cret", "wrong"));
    }

    #[test]
    fn hash_password_succeeds() {
        // 真实 argon2：哈希成功，输出是合法 PHC 字符串。
        let h = hash_password("anything").expect("hash ok");
        assert!(h.starts_with("$argon2id$"), "应为 Argon2id PHC 字符串: {h}");
    }

    #[test]
    fn hash_verify_roundtrip() {
        // 真实往返：hash → verify 应通过。
        let pw = "correct horse battery staple";
        let h = hash_password(pw).expect("hash ok");
        assert!(verify_password(pw, &h), "正确密码应校验通过");
        assert!(!verify_password("wrong password", &h), "错误密码应失败");
        assert!(!verify_password("", &h), "空密码应失败");
    }

    #[test]
    fn hash_password_random_salt() {
        // 同明文两次哈希，salt 随机 → PHC 字符串不同。
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b, "随机 salt 应使每次哈希结果不同");
        // 但两者都能通过校验。
        assert!(verify_password("same", &a));
        assert!(verify_password("same", &b));
    }

    #[test]
    fn verify_password_malformed_phc_falls_back() {
        // 非法 PHC 字符串（含 $ 但解析失败）→ 回退到常量时间比较。
        // 注意：含 $ 的非 PHC 字符串不应误判为合法哈希。
        assert!(!verify_password("x", "$argon2id$garbage"));
    }
}
