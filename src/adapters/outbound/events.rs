//! [`EventBus`] over a tokio broadcast channel, plus the tracing writer that
//! turns log lines into `log` events.

use std::sync::Arc;

use futures_util::stream::BoxStream;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::ports::events::LiveEvent;
use crate::ports::EventBus;

/// In-process fan-out; slow subscribers drop the oldest events.
#[derive(Debug, Clone)]
pub struct BroadcastEventBus {
    sender: broadcast::Sender<LiveEvent>,
}

impl Default for BroadcastEventBus {
    fn default() -> Self {
        Self {
            sender: broadcast::channel(256).0,
        }
    }
}

impl EventBus for BroadcastEventBus {
    fn publish(&self, event: &'static str, data: Value) {
        let _ = self.sender.send((event, data));
    }

    fn has_subscribers(&self) -> bool {
        self.sender.receiver_count() > 0
    }

    fn subscribe(&self) -> BoxStream<'static, LiveEvent> {
        Box::pin(futures_util::stream::unfold(
            self.sender.subscribe(),
            |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => return Some((event, rx)),
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            },
        ))
    }
}

/// A tracing writer that forwards each formatted log line as a `log` event.
#[derive(Clone)]
pub struct LogTap {
    events: Arc<dyn EventBus>,
}

impl LogTap {
    /// A writer publishing to `events`.
    #[must_use]
    pub fn new(events: Arc<dyn EventBus>) -> Self {
        Self { events }
    }
}

impl std::io::Write for LogTap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.events.has_subscribers() {
            for line in String::from_utf8_lossy(buf).lines() {
                self.events.publish("log", json!({ "line": line }));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use std::io::Write as _;

    #[tokio::test]
    async fn subscribers_receive_published_events_and_log_lines() {
        let bus = Arc::new(BroadcastEventBus::default());
        assert!(!bus.has_subscribers());
        let mut events = bus.subscribe();
        assert!(bus.has_subscribers());

        bus.publish("config", json!({"model": "m"}));
        LogTap::new(bus.clone()).write_all(b"one\ntwo\n").unwrap();

        assert_eq!(events.next().await.unwrap().0, "config");
        let (name, data) = events.next().await.unwrap();
        assert_eq!((name, data["line"].as_str()), ("log", Some("one")));
        assert_eq!(events.next().await.unwrap().1["line"], "two");
    }
}
