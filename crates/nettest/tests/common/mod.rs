//! nettest 集成测试公共辅助。
//!
//! 把「给真实网络测一个总超时上限，超时则 panic 并报告哪一步卡住」这个能力
//! 抽出来，避免某个 ignored 测因为公网/组播卡死而无限挂住。

use std::future::Future;
use std::time::Duration;

/// 包裹一个 future，给它一个宽裕的总超时上限（默认 60s）。
///
/// 真实网络测里每一步都有自己的细粒度超时（reqwest::timeout / recv 窗口等），
/// 这里只是一个兜底的「绝对不能挂死超过 60s」保险，防止 ignored 测在异常环境
/// 下无限阻塞 CI/手动运行。
pub async fn timeout_or_panic<F>(fut: F)
where
    F: Future<Output = ()>,
{
    tokio::time::timeout(Duration::from_secs(60), fut)
        .await
        .expect("[nettest] 测试总超时（60s）—— 某一步网络操作卡死");
}
