//! os-im 错误类型
//!
//! 设计：每 crate 自定义 `ImError`（thiserror），并实现
//! `From<ImError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-im 错误
#[derive(Debug, Error)]
pub enum ImError {
    /// 对话不存在
    #[error("对话不存在: {0}")]
    ConversationNotFound(String),

    /// agent 不存在
    #[error("agent 不存在: {0}")]
    AgentNotFound(String),

    /// tool 不存在/未注册
    #[error("tool 不存在: {0}")]
    ToolNotFound(String),

    /// 委派循环（任务依赖图出现环，违反 §3.7.2 无环约束）
    #[error("委派循环: {0}")]
    TaskCycle(String),

    /// 高危操作被拒绝（用户否决或会签未达法定）
    #[error("操作被拒绝: {0}")]
    ConfirmationDenied(String),

    /// LLM 后端错误（调用失败/超时/额度）
    #[error("LLM 错误: {0}")]
    LlmError(String),

    /// 操作超时
    #[error("操作超时: {0}")]
    Timeout(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),

    // —— P2P 传输层网络错误（节点间 TCP/消息路由）——
    /// 连接失败（远端不可达 / 拨号被拒 / 端口未监听）
    #[error("P2P 连接失败: {0}")]
    ConnectionFailed(String),

    /// 连接已断开（对端主动关闭 / IO 错误 / 超时无响应）
    #[error("P2P 连接已断开: {0}")]
    Disconnected(String),

    /// 消息过大（超过 length-delimited 帧上限）
    #[error("P2P 消息过大: {0}")]
    MessageTooLarge(String),

    /// 握手失败（NodeHello 校验未通过 / 协议版本不兼容）
    #[error("P2P 握手失败: {0}")]
    HandshakeFailed(String),

    // —— Federation（跨 OS 节点互联）错误 ——
    // 注：`HandshakeFailed` 复用上方传输层变体（P2P 握手失败语义一致），
    //     此处仅补充 Federation 特有的信任/认证/会话错误。
    /// 节点未信任（仅 Hello/Welcome，不能加入群或继续握手）
    #[error("节点未信任: {0}")]
    NodeNotTrusted(String),

    /// Federation 认证失败（预共享密钥 / 签名校验不通过）
    #[error("认证失败: {0}")]
    AuthFailed(String),

    /// 节点已存在（重复添加同一 endpoint / node_id）
    #[error("节点已存在: {0}")]
    NodeAlreadyExists(String),

    /// Federation 会话令牌过期/失效
    #[error("会话已过期: {0}")]
    SessionExpired(String),

    // —— 群组管理（见 group.rs / group_impl.rs）——
    /// 群组不存在
    #[error("群组不存在: {0}")]
    GroupNotFound(String),

    /// 群组已满（成员数达上限）
    #[error("群组已满: {0}")]
    GroupFull(String),

    /// 节点不是该群成员
    #[error("非群成员: {0}")]
    NotMember(String),

    /// 邀请码已过期（默认 24 小时有效）
    #[error("邀请码已过期: {0}")]
    InviteExpired(String),

    /// 邀请码无效（不存在/格式不符）
    #[error("邀请码无效: {0}")]
    InviteInvalid(String),

    /// 权限不足（如仅 Owner/Admin 可踢人）
    #[error("权限不足: {0}")]
    PermissionDenied(String),
}

/// os-im Result 别名
pub type ImResult<T> = Result<T, ImError>;

// —— From 转换：ImError → ApiError（统一对外错误码）——
impl From<ImError> for os_common::ApiError {
    fn from(e: ImError) -> Self {
        use os_common::ApiErrorCode as Code;
        use ImError as E;
        let (code, msg) = match e {
            E::ConversationNotFound(m) => (Code::NotFound, m),
            E::AgentNotFound(m) => (Code::NotFound, m),
            E::ToolNotFound(m) => (Code::NotFound, m),
            E::TaskCycle(m) => (Code::Conflict, m),
            E::ConfirmationDenied(m) => (Code::ConfirmationRequired, m),
            E::LlmError(m) => (Code::UpstreamUnavailable, m),
            E::Timeout(m) => (Code::UpstreamUnavailable, m),
            E::Internal(m) => (Code::Internal, m),
            E::ConnectionFailed(m) => (Code::UpstreamUnavailable, m),
            E::Disconnected(m) => (Code::UpstreamUnavailable, m),
            E::MessageTooLarge(m) => (Code::InvalidInput, m),
            E::HandshakeFailed(m) => (Code::PermissionDenied, m),
            // —— Federation 错误 ——
            E::NodeNotTrusted(m) => (Code::PermissionDenied, m),
            E::AuthFailed(m) => (Code::PermissionDenied, m),
            E::NodeAlreadyExists(m) => (Code::Conflict, m),
            E::SessionExpired(m) => (Code::PermissionDenied, m),
            // —— 群组错误 ——
            E::GroupNotFound(m) => (Code::NotFound, m),
            E::GroupFull(m) => (Code::Conflict, m),
            E::NotMember(m) => (Code::PermissionDenied, m),
            E::InviteExpired(m) => (Code::PermissionDenied, m),
            E::InviteInvalid(m) => (Code::InvalidInput, m),
            E::PermissionDenied(m) => (Code::PermissionDenied, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::ImError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::ConversationNotFound("c".into())).contains("对话不存在"));
        assert!(format!("{}", E::AgentNotFound("a".into())).contains("agent 不存在"));
        assert!(format!("{}", E::ToolNotFound("t".into())).contains("tool 不存在"));
        assert!(format!("{}", E::TaskCycle("t".into())).contains("委派循环"));
        assert!(format!("{}", E::ConfirmationDenied("c".into())).contains("操作被拒绝"));
        assert!(format!("{}", E::LlmError("l".into())).contains("LLM 错误"));
        assert!(format!("{}", E::Timeout("t".into())).contains("操作超时"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
        assert!(format!("{}", E::ConnectionFailed("c".into())).contains("P2P 连接失败"));
        assert!(format!("{}", E::Disconnected("d".into())).contains("P2P 连接已断开"));
        assert!(format!("{}", E::MessageTooLarge("m".into())).contains("P2P 消息过大"));
        assert!(format!("{}", E::HandshakeFailed("h".into())).contains("P2P 握手失败"));
        // Federation 错误
        assert!(format!("{}", E::NodeNotTrusted("n".into())).contains("节点未信任"));
        assert!(format!("{}", E::AuthFailed("a".into())).contains("认证失败"));
        assert!(format!("{}", E::NodeAlreadyExists("d".into())).contains("节点已存在"));
        assert!(format!("{}", E::SessionExpired("s".into())).contains("会话已过期"));
    }
}
