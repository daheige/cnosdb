use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::MetaResult;

pub mod limiter_kind;
pub mod local_request_limiter;
pub mod none_limiter;
pub mod remote_request_limiter;

pub use limiter_kind::RequestLimiterKind;
pub use local_request_limiter::LocalRequestLimiter;
pub use none_limiter::NoneLimiter;

pub type LimiterRef = Arc<dyn RequestLimiter>;

/// The distributed current limiter is divided into two parts.
/// There is a cached LocalBucket on the data node,
/// and a RateBucket on the meta node.
///
/// When a request comes,
/// the token bucket on the data node is first accessed.
/// If the token bucket in the data cache has enough tokens, the request can pass.
/// If the token bucket in the data cache is not enough,
/// the greedy method is used to request tokens from the Meta's RateBucket.
///
/// When the current limiter configuration changes,
/// the local LocalBucket is changed through the watch mechanism.
// │                                                               x
// │
// │                                                               x
// │                                                     ┌────────────────────┐
// │                 ┌────────────────┐                  │                    │
// │  require local  │                │  require remote  │      x     x       │
// │      6 x        │                ├─────────────────►│         x          │
// ├────────────────►│  x    x     x  │                  │                    │
// │                 │                │                  │      x x     x     │
// │                 └────────────────┘                  │                    │
// │                                                     └────────────────────┘
///
#[async_trait]
pub trait RequestLimiter: Send + Sync + Debug {
    async fn check_coord_data_in(&self, data_len: usize) -> MetaResult<()>;
    async fn check_coord_data_out(&self, data_len: usize) -> MetaResult<()>;
    async fn check_coord_queries(&self) -> MetaResult<()>;
    async fn check_coord_writes(&self) -> MetaResult<()>;
}
