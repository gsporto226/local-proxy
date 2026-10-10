use futures_util::stream::BoxStream;
use serde_json::Value;

/// A named live event (`request`, `config`, `rate_limits`, `log`, ...).
pub type LiveEvent = (&'static str, Value);

/// Fan-out of live events to `/admin/events` subscribers.
pub trait EventBus: Send + Sync {
    /// Publish an event to every current subscriber (no-op without any).
    fn publish(&self, event: &'static str, data: Value);

    /// Whether anyone is listening, so costly events can be skipped.
    fn has_subscribers(&self) -> bool;

    /// Subscribe to every event published from now on.
    fn subscribe(&self) -> BoxStream<'static, LiveEvent>;
}
