use std::future::Future;
use std::pin::Pin;

/// Cooperative cancellation token for the transmux pipeline.
///
/// Implementations wrap an external cancellation signal (e.g.
/// `tokio_util::sync::CancellationToken`). The transmuxer checks
/// [`is_cancelled`](Self::is_cancelled) at the top of each segment loop
/// iteration and races [`cancelled`](Self::cancelled) against
/// `Source::read_bytes` await points for fast response.
///
/// This is a trait (rather than a concrete type depending on `tokio-util`)
/// so the crate stays dependency-light; callers wire in whatever
/// cancellation primitive they already use.
pub trait CancelToken: Send + Sync + std::fmt::Debug {
    /// Returns `true` once the operation has been cancelled.
    fn is_cancelled(&self) -> bool;

    /// Resolves when the operation is cancelled. Used to race against
    /// `Source::read_bytes` await points via `tokio::select!` for fast
    /// cancellation response.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
