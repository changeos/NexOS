//! §3.19 敏感项排除清单——纯路径匹配过滤算法。
//!
//! 背景：迁移源节点的配置/共享/用户定义走迁移包（结构化导出/导入），其中可能引用
//! 敏感密钥/密码。按 §3.19 统一排除清单，**这些项绝不随迁移包传输**——目标节点须
//! 重新生成或独立导入（如：TLS 私钥重新签发、SSH 私钥由管理员独立导入、集群密钥
//! 在 join 时由 leader 下发）。
//!
//! 本模块是**纯函数**算法（无 IO、无外部依赖），输入"待迁移文件/键列表 + 排除规则"，
//! 输出"应传输 / 应排除"两组。可被 `MigrationEngine::execute` 在打包前调用做过滤，
//! 也可单测覆盖所有匹配规则。
//!
//! 匹配语义：
//! - 规则分两类：`Exact`（完全相等）/ `Prefix`（前缀，含目录边界）/ `Glob`（简单通配，
//!   `*` 匹配单层任意字符，`**` 跨层；不支持 `?`/`[]`，避免误匹配风险）。
//! - 路径统一按 POSIX 风格（`/` 分隔，前导 `/` 表绝对）。键名按字符串比较。
//! - 默认提供 [`default_excludes`]：覆盖 §3.19 列举的全部敏感类别。

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 排除规则
// ----------------------------------------------------------------------------

/// 一条排除规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExcludeRule {
    /// 匹配模式
    pub pattern: ExcludePattern,
    /// 该规则所属的敏感类别（用于审计/日志，不参与匹配）
    pub category: ExcludeCategory,
    /// 人读说明（迁移日志中标注"为何排除此项"）
    pub reason: String,
}

/// 匹配模式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExcludePattern {
    /// 完全相等（键名/文件名）
    Exact(String),
    /// 前缀匹配——`candidate` 以该字符串开头即命中（纯子串前缀）。
    /// 例：`Prefix("/etc/ssh/ssh_host_")` 命中 `/etc/ssh/ssh_host_rsa_key`；
    /// `Prefix("/etc/os/cluster/")` 命中该目录下所有文件。
    /// 若需"目录边界"语义（避免 `/etc/ssh` 命中 `/etc/sshextra`），用 Glob
    /// （`/etc/ssh/*`）或在 prefix 末尾加 `/`。
    Prefix(String),
    /// 简单 glob——`*` 单层任意字符，`**` 跨 `/`。
    /// 例：`/var/lib/os/secrets/**` 命中其下所有文件；
    /// `/etc/*.pem` 命中 `/etc/a.pem` 但不命中 `/etc/sub/a.pem`。
    Glob(String),
}

/// §3.19 敏感类别。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExcludeCategory {
    /// `/etc/shadow`、`/etc/gshadow` 等系统口令文件
    SystemCredential,
    /// TLS 私钥（`*.key`/`*.pem` 私钥部分）
    TlsPrivateKey,
    /// SSH 私钥 / 主机密钥
    SshPrivateKey,
    /// SMB 凭证 / passwd-db
    SmbCredential,
    /// 数据库密码（osd 连接串中的 password）
    DatabasePassword,
    /// JWT 签名密钥 / TOTP secret
    JwtTotpSecret,
    /// 钱包密钥（钱包私钥 / 助记词——见 wallet-agent）
    WalletKey,
    /// 集群密钥（openraft/member-encrypt-key，由 leader 下发，不迁）
    ClusterSecret,
    /// 其它显式标注的敏感项
    Other,
}

// ----------------------------------------------------------------------------
// 默认排除清单（§3.19）
// ----------------------------------------------------------------------------

/// §3.19 统一排除清单（默认）。
///
/// 覆盖：系统口令 / TLS 私钥 / SSH 私钥与主机密钥 / SMB 凭证 / 数据库密码
/// / JWT-TOTP 密钥 / 钱包密钥 / 集群密钥。调用方可在此之上追加自定义规则
/// （见 [`ExcludeRules::with_extra`]）。
pub fn default_excludes() -> Vec<ExcludeRule> {
    use ExcludeCategory as C;
    use ExcludePattern as P;
    vec![
        // —— 系统口令 ——
        ExcludeRule {
            pattern: P::Exact("/etc/shadow".into()),
            category: C::SystemCredential,
            reason: "系统口令哈希文件，目标节点首启强制重设".into(),
        },
        ExcludeRule {
            pattern: P::Exact("/etc/shadow-".into()),
            category: C::SystemCredential,
            reason: "shadow 备份文件".into(),
        },
        ExcludeRule {
            pattern: P::Exact("/etc/gshadow".into()),
            category: C::SystemCredential,
            reason: "组口令文件".into(),
        },
        // —— TLS 私钥 ——
        ExcludeRule {
            pattern: P::Glob("/etc/ssl/private/**/*.key".into()),
            category: C::TlsPrivateKey,
            reason: "TLS 私钥，目标节点重新签发".into(),
        },
        ExcludeRule {
            pattern: P::Glob("/etc/ssl/private/**/*.pem".into()),
            category: C::TlsPrivateKey,
            reason: "TLS 私钥(pem)，目标节点重新签发".into(),
        },
        ExcludeRule {
            pattern: P::Glob("/etc/nginx/ssl/**/*.key".into()),
            category: C::TlsPrivateKey,
            reason: "Web 服务 TLS 私钥".into(),
        },
        // —— SSH 私钥 / 主机密钥 ——
        ExcludeRule {
            pattern: P::Prefix("/etc/ssh/ssh_host_".into()),
            category: C::SshPrivateKey,
            reason: "SSH 主机密钥，目标节点重新生成".into(),
        },
        ExcludeRule {
            pattern: P::Glob("/root/.ssh/id_*".into()),
            category: C::SshPrivateKey,
            reason: "root 用户 SSH 私钥，独立导入".into(),
        },
        ExcludeRule {
            pattern: P::Glob("/home/*/.ssh/id_*".into()),
            category: C::SshPrivateKey,
            reason: "普通用户 SSH 私钥，独立导入".into(),
        },
        ExcludeRule {
            pattern: P::Exact("/root/.ssh/authorized_keys".into()),
            category: C::SshPrivateKey,
            reason: "authorized_keys 不迁，目标节点重新部署".into(),
        },
        // —— SMB 凭证 ——
        ExcludeRule {
            pattern: P::Exact("/etc/samba/smbpasswd".into()),
            category: C::SmbCredential,
            reason: "SMB 旧式口令库，目标节点重建".into(),
        },
        ExcludeRule {
            pattern: P::Glob("/var/lib/samba/private/**/*.tdb".into()),
            category: C::SmbCredential,
            reason: "Samba 私有库（含机密）".into(),
        },
        // —— 数据库密码 ——
        ExcludeRule {
            pattern: P::Glob("/etc/os/*.db-password".into()),
            category: C::DatabasePassword,
            reason: "osd 数据库连接密码文件".into(),
        },
        ExcludeRule {
            pattern: P::Exact("/etc/os/pg_password".into()),
            category: C::DatabasePassword,
            reason: "PostgreSQL 连接密码".into(),
        },
        // —— JWT / TOTP 密钥 ——
        ExcludeRule {
            pattern: P::Exact("/etc/os/jwt-signing.key".into()),
            category: C::JwtTotpSecret,
            reason: "JWT 签名密钥，目标节点重新生成（旧 token 失效）".into(),
        },
        ExcludeRule {
            pattern: P::Exact("/etc/os/totp-secret".into()),
            category: C::JwtTotpSecret,
            reason: "TOTP 共享密钥，2FA 须重新绑定".into(),
        },
        // —— 钱包密钥 ——
        ExcludeRule {
            pattern: P::Prefix("/var/lib/os/wallet/keystore/".into()),
            category: C::WalletKey,
            reason: "钱包 keystore，绝不随迁移传输（见 §3.19）".into(),
        },
        ExcludeRule {
            pattern: P::Glob("/var/lib/os/wallet/**/*.mnemonic".into()),
            category: C::WalletKey,
            reason: "助记词备份，绝不传输".into(),
        },
        // —— 集群密钥 ——
        ExcludeRule {
            pattern: P::Prefix("/etc/os/cluster/".into()),
            category: C::ClusterSecret,
            reason: "集群加密/成员密钥，由 leader 在 join 时下发".into(),
        },
        ExcludeRule {
            pattern: P::Exact("/etc/os/member-encrypt.key".into()),
            category: C::ClusterSecret,
            reason: "成员加密密钥，不迁".into(),
        },
    ]
}

// ----------------------------------------------------------------------------
// 过滤结果
// ----------------------------------------------------------------------------

/// 一项待迁移条目的过滤结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterOutcome {
    /// 应传输（无规则命中）。
    Transfer,
    /// 应排除（附命中的规则，便于审计日志）。
    Excluded {
        /// 命中的规则（多条命中时取第一条；通常互斥）。
        rule: ExcludeRule,
    },
}

// ----------------------------------------------------------------------------
// 规则集 + 匹配引擎
// ----------------------------------------------------------------------------

/// 排除规则集（默认 + 追加）。
///
/// 设计为值类型、不可变（构造期确定）；多线程共享时用 `&`。
#[derive(Debug, Clone, Default)]
pub struct ExcludeRules {
    rules: Vec<ExcludeRule>,
}

impl ExcludeRules {
    /// 用 §3.19 默认清单构造。
    pub fn defaults() -> Self {
        Self {
            rules: default_excludes(),
        }
    }

    /// 在默认清单之上追加自定义规则。
    pub fn with_extra(mut self, extra: impl IntoIterator<Item = ExcludeRule>) -> Self {
        self.rules.extend(extra);
        self
    }

    /// 仅用给定规则（不含默认清单）。
    pub fn from_rules(rules: impl IntoIterator<Item = ExcludeRule>) -> Self {
        Self {
            rules: rules.into_iter().collect(),
        }
    }

    /// 规则总数。
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 内部规则快照（用于审计）。
    pub fn rules(&self) -> &[ExcludeRule] {
        &self.rules
    }

    /// 判定单个条目是否命中任一排除规则。
    ///
    /// `entry` 是文件绝对路径（如 `/etc/shadow`）或键名（如 `jwt-signing.key`）。
    /// 返回 `Excluded { rule }` 表示命中（应排除），`Transfer` 表示应传输。
    pub fn evaluate(&self, entry: &str) -> FilterOutcome {
        for rule in &self.rules {
            if rule.pattern.matches(entry) {
                return FilterOutcome::Excluded { rule: rule.clone() };
            }
        }
        FilterOutcome::Transfer
    }

    /// 对一批待迁移条目做过滤，返回 `(应传输, 应排除)`。
    ///
    /// `excluded` 中的元组为 `(条目, 命中规则)`，便于在迁移日志中输出审计信息。
    pub fn partition<'a, I>(&self, entries: I) -> (Vec<&'a str>, Vec<(&'a str, ExcludeRule)>)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut transfer = Vec::new();
        let mut excluded = Vec::new();
        for e in entries {
            match self.evaluate(e) {
                FilterOutcome::Transfer => transfer.push(e),
                FilterOutcome::Excluded { rule } => excluded.push((e, rule)),
            }
        }
        (transfer, excluded)
    }
}

impl ExcludePattern {
    /// 判定给定字符串是否命中本模式。
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            ExcludePattern::Exact(s) => s == candidate,
            ExcludePattern::Prefix(p) => match_prefix_dir_aware(p, candidate),
            ExcludePattern::Glob(g) => match_glob(g, candidate),
        }
    }
}

/// 前缀匹配：纯子串前缀（`candidate.starts_with(prefix)`）。
/// 简单可预测；如需目录边界语义用 Glob 或在 prefix 末尾加 `/`。
fn match_prefix_dir_aware(prefix: &str, candidate: &str) -> bool {
    candidate.starts_with(prefix)
}

/// 简单 glob 匹配：
/// - `*` 匹配单层（不含 `/`）任意字符
/// - `**` 匹配跨 `/` 任意字符（含空串）
/// - 其它字符原义比较
///
/// 实现：先把 pattern 解析为 token 序列（连续 `*` 合并为 `**` token，单 `*`
/// 为单星 token，其余为字面字符），再做 DP 匹配。这样避免逐字符 DP 中
/// 连续 `*` 互相干扰。
fn match_glob(pattern: &str, text: &str) -> bool {
    let tokens = parse_glob_tokens(pattern);
    glob_match_dp(&tokens, text)
}

/// glob token：`**/`（跨目录段）/ `**`（跨目录尾）/ `*`（单层）/ 字面字符。
#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobToken {
    /// 单星：匹配 0+ 个非 `/` 字符
    SingleStar,
    /// 双星段（`**/`）：匹配 0 个目录（空串）或多个目录（`dir/dir/...`，含末尾 `/`）
    DoubleStarSegment,
    /// 双星尾（`**` 不跟 `/`）：匹配 0+ 个任意字符（含 `/`），用于模式末尾的 `**`
    DoubleStarTail,
    /// 字面字符
    Lit(char),
}

fn parse_glob_tokens(pattern: &str) -> Vec<GlobToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' {
            // 合并连续 '*'：≥2 个都视为 DoubleStar
            let mut count = 0;
            while i < chars.len() && chars[i] == '*' {
                count += 1;
                i += 1;
            }
            if count >= 2 {
                // 检查后面是否紧跟 '/'：若是，合并为 DoubleStarSegment（消费该 '/'）
                if i < chars.len() && chars[i] == '/' {
                    tokens.push(GlobToken::DoubleStarSegment);
                    i += 1; // 消费 '/'
                } else {
                    tokens.push(GlobToken::DoubleStarTail);
                }
            } else {
                tokens.push(GlobToken::SingleStar);
            }
        } else {
            tokens.push(GlobToken::Lit(chars[i]));
            i += 1;
        }
    }
    tokens
}

/// DP 匹配：tokens 前 i 项 是否匹配 text 前 j 字符。
fn glob_match_dp(tokens: &[GlobToken], text: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let (n, m) = (tokens.len(), t.len());
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[0][0] = true;

    // 空文本：只有当 tokens 全是 Star 类时才能匹配
    for i in 1..=n {
        match &tokens[i - 1] {
            GlobToken::SingleStar | GlobToken::DoubleStarSegment | GlobToken::DoubleStarTail => {
                dp[i][0] = dp[i - 1][0];
            }
            GlobToken::Lit(_) => break,
        }
    }

    for i in 1..=n {
        for j in 1..=m {
            match &tokens[i - 1] {
                GlobToken::DoubleStarTail => {
                    // 匹配任意（含 '/'）：跳过本 token 或消费一个字符
                    dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
                }
                GlobToken::DoubleStarSegment => {
                    // `**/`：匹配空（跳过本 token，对应"零个目录"），或匹配
                    // "dir/"——当当前字符非 '/' 时消费并继续本 token（在目录名中）；
                    // 当当前字符为 '/' 时，消费它并"闭合"一个目录段：可选地停在
                    // 本 token（继续匹配下一目录）或推进到下一 token。
                    if t[j - 1] == '/' {
                        // 消费 '/'：要么作为某目录段的闭合（推进到 i-1 后还能再匹配
                        // 一个 DoubleStarSegment —— 通过保留 i 不动实现"贪婪多目录"）
                        dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
                    } else {
                        // 目录名内的字符：消费并继续本 token
                        dp[i][j] = dp[i][j - 1];
                    }
                }
                GlobToken::SingleStar => {
                    // 匹配空（跳过）或消费一个非 '/' 字符
                    dp[i][j] = dp[i - 1][j] || (dp[i][j - 1] && t[j - 1] != '/');
                }
                GlobToken::Lit(c) => {
                    if *c == t[j - 1] {
                        dp[i][j] = dp[i - 1][j - 1];
                    }
                }
            }
        }
    }

    dp[n][m]
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn r(pat: ExcludePattern, cat: ExcludeCategory, why: &str) -> ExcludeRule {
        ExcludeRule {
            pattern: pat,
            category: cat,
            reason: why.into(),
        }
    }

    // —— pattern 层 ——

    #[test]
    fn exact_match() {
        assert!(ExcludePattern::Exact("/etc/shadow".into()).matches("/etc/shadow"));
        assert!(!ExcludePattern::Exact("/etc/shadow".into()).matches("/etc/shadow-"));
    }

    #[test]
    fn prefix_dir_boundary() {
        let p = ExcludePattern::Prefix("/etc/ssh/ssh_host_".into());
        assert!(p.matches("/etc/ssh/ssh_host_rsa_key"));
        assert!(p.matches("/etc/ssh/ssh_host_ed25519_key"));
        assert!(!p.matches("/etc/ssh/sshd_config")); // 不在 ssh_host_ 前缀下
    }

    #[test]
    fn prefix_substring_semantics() {
        // 纯子串前缀：以该字符串开头即命中
        let p = ExcludePattern::Prefix("/etc/ssh".into());
        assert!(p.matches("/etc/ssh"));
        assert!(p.matches("/etc/ssh/id_rsa"));
        assert!(p.matches("/etc/ssh_host_key")); // 子串前缀命中
                                                 // 注意：substring 前缀也会命中 /etc/sshextra；如需目录边界用 Glob
        assert!(p.matches("/etc/sshextra"));
    }

    #[test]
    fn glob_single_star_no_slash() {
        let g = ExcludePattern::Glob("/home/*/.ssh/id_*".into());
        assert!(g.matches("/home/alice/.ssh/id_rsa"));
        assert!(g.matches("/home/bob/.ssh/id_ed25519"));
        assert!(!g.matches("/home/alice/sub/.ssh/id_rsa")); // 单 * 不跨目录
    }

    #[test]
    fn glob_double_star_cross_dir() {
        let g = ExcludePattern::Glob("/etc/ssl/private/**/*.key".into());
        assert!(g.matches("/etc/ssl/private/self-signed.key"));
        assert!(g.matches("/etc/ssl/private/sub/deep/key.key"));
        assert!(!g.matches("/etc/ssl/private/self-signed.crt"));
    }

    #[test]
    fn glob_double_star_tail() {
        let g = ExcludePattern::Glob("/var/lib/os/wallet/**".into());
        assert!(g.matches("/var/lib/os/wallet/keystore/x.json"));
        assert!(g.matches("/var/lib/os/wallet/a/b/c.mnemonic"));
        assert!(!g.matches("/var/lib/os/other/x"));
    }

    #[test]
    fn glob_plain_suffix() {
        let g = ExcludePattern::Glob("*.pem".into());
        assert!(g.matches("a.pem"));
        assert!(!g.matches("a.key"));
    }

    // —— 默认清单 ——

    #[test]
    fn defaults_cover_all_categories() {
        let rules = ExcludeRules::defaults();
        assert!(
            rules.len() >= 15,
            "默认清单应覆盖全部 8 个类别，got {}",
            rules.len()
        );

        let cats: Vec<_> = rules.rules().iter().map(|r| r.category).collect();
        use ExcludeCategory as C;
        for need in [
            C::SystemCredential,
            C::TlsPrivateKey,
            C::SshPrivateKey,
            C::SmbCredential,
            C::DatabasePassword,
            C::JwtTotpSecret,
            C::WalletKey,
            C::ClusterSecret,
        ] {
            assert!(cats.contains(&need), "默认清单缺类别 {:?}", need);
        }
    }

    #[test]
    fn defaults_evaluate_known_sensitive_paths() {
        let rules = ExcludeRules::defaults();
        let sensitive = [
            "/etc/shadow",
            "/etc/gshadow",
            "/etc/shadow-",
            "/etc/ssl/private/self-signed.key",
            "/etc/ssl/private/sub/deep/key.pem",
            "/etc/nginx/ssl/site.key",
            "/etc/ssh/ssh_host_rsa_key",
            "/root/.ssh/id_ed25519",
            "/home/alice/.ssh/id_rsa",
            "/root/.ssh/authorized_keys",
            "/etc/samba/smbpasswd",
            "/var/lib/samba/private/secrets.tdb",
            "/etc/os/jwt-signing.key",
            "/etc/os/totp-secret",
            "/etc/os/pg_password",
            "/etc/os/db.db-password",
            "/var/lib/os/wallet/keystore/btc.json",
            "/var/lib/os/wallet/x/a.mnemonic",
            "/etc/os/cluster/raft-key",
            "/etc/os/member-encrypt.key",
        ];
        for p in sensitive {
            assert!(
                matches!(rules.evaluate(p), FilterOutcome::Excluded { .. }),
                "应排除敏感路径 {}",
                p
            );
        }
    }

    #[test]
    fn defaults_do_not_exclude_safe_paths() {
        let rules = ExcludeRules::defaults();
        let safe = [
            "/etc/hostname",
            "/etc/hosts",
            "/etc/os/config.toml",
            "/etc/samba/smb.conf",           // 配置文件本身可迁，凭证不可迁
            "/var/lib/os/wallet-rules.json", // 注意：不含 /wallet/ 目录前缀
            "/home/alice/.ssh/known_hosts",
            "/etc/ssl/certs/ca-certificates.crt", // 公共 CA（非 private/）
        ];
        for p in safe {
            assert_eq!(
                rules.evaluate(p),
                FilterOutcome::Transfer,
                "安全路径 {} 不应被排除",
                p
            );
        }
    }

    // —— partition ——

    #[test]
    fn partition_splits_entries() {
        let rules = ExcludeRules::defaults();
        let entries = [
            "/etc/hostname",
            "/etc/shadow",
            "/etc/os/config.toml",
            "/etc/os/jwt-signing.key",
        ];
        let (transfer, excluded) = rules.partition(entries.iter().copied());
        assert_eq!(transfer, vec!["/etc/hostname", "/etc/os/config.toml"]);
        assert_eq!(excluded.len(), 2);
        assert_eq!(excluded[0].0, "/etc/shadow");
        assert_eq!(excluded[1].0, "/etc/os/jwt-signing.key");
    }

    // —— 自定义追加 ——

    #[test]
    fn with_extra_appends() {
        let rules = ExcludeRules::defaults().with_extra(vec![r(
            ExcludePattern::Exact("/custom/secret".into()),
            ExcludeCategory::Other,
            "test",
        )]);
        assert!(matches!(
            rules.evaluate("/custom/secret"),
            FilterOutcome::Excluded { .. }
        ));
        // 默认清单仍生效
        assert!(matches!(
            rules.evaluate("/etc/shadow"),
            FilterOutcome::Excluded { .. }
        ));
    }

    #[test]
    fn from_rules_only_custom() {
        let rules = ExcludeRules::from_rules(vec![r(
            ExcludePattern::Exact("/x".into()),
            ExcludeCategory::Other,
            "test",
        )]);
        assert!(matches!(
            rules.evaluate("/etc/shadow"),
            FilterOutcome::Transfer
        ));
        assert!(matches!(
            rules.evaluate("/x"),
            FilterOutcome::Excluded { .. }
        ));
    }

    #[test]
    fn empty_rules_transfer_all() {
        let rules = ExcludeRules::from_rules(vec![]);
        assert!(rules.is_empty());
        let entries = ["/etc/shadow", "/anything"];
        let (t, e) = rules.partition(entries.iter().copied());
        assert_eq!(t.len(), 2);
        assert!(e.is_empty());
    }
}
