//! Activity sink implementations for S019 activity events.
//!
//! The `ActivitySink` trait defines a non-blocking push interface for activity
//! events. Three implementations are provided:
//!
//! - `NoopActivitySink` — discards all events (headless mode, tests)
//! - `ChannelActivitySink` — sends via `tokio::sync::mpsc::UnboundedSender<ActivityEvent>`
//! - `ForwardingActivitySink` — wraps child events in `ChildActivity` and forwards to parent

use std::sync::Arc;

use simulacra_types::ActivityEvent;

/// Non-blocking push interface for activity events.
///
/// Implementations MUST be non-blocking: buffer or drop events rather than
/// blocking the agent loop. The trait is object-safe for `Arc<dyn ActivitySink>`.
pub trait ActivitySink: Send + Sync + 'static {
    fn emit(&self, event: ActivityEvent);
}

/// Discards all events. Used in headless mode and tests where no consumer is listening.
pub struct NoopActivitySink;

impl ActivitySink for NoopActivitySink {
    fn emit(&self, _event: ActivityEvent) {
        // intentionally empty
    }
}

/// Sends events through a `tokio::sync::mpsc::UnboundedSender<ActivityEvent>`.
///
/// `emit()` uses `UnboundedSender::send()` which never blocks. If the receiver
/// has been dropped, the event is silently discarded.
pub struct ChannelActivitySink {
    sender: tokio::sync::mpsc::UnboundedSender<ActivityEvent>,
}

impl ChannelActivitySink {
    pub fn new(sender: tokio::sync::mpsc::UnboundedSender<ActivityEvent>) -> Self {
        Self { sender }
    }
}

impl ActivitySink for ChannelActivitySink {
    fn emit(&self, event: ActivityEvent) {
        // Non-blocking, never drops (unbounded). If the receiver is gone,
        // the send fails silently — the agent loop must not block.
        let _ = self.sender.send(event);
    }
}

/// Wraps each child event in `ChildActivity` and forwards to the parent's sink.
///
/// Used when creating a child `AgentLoop` so that the parent sees all child
/// events nested under the child's identity.
pub struct ForwardingActivitySink {
    child_id: String,
    agent_type: String,
    parent_sink: Arc<dyn ActivitySink>,
}

impl ForwardingActivitySink {
    pub fn new(child_id: String, agent_type: String, parent_sink: Arc<dyn ActivitySink>) -> Self {
        Self {
            child_id,
            agent_type,
            parent_sink,
        }
    }
}

impl ActivitySink for ForwardingActivitySink {
    fn emit(&self, event: ActivityEvent) {
        // Wrap in ChildActivity and forward immediately — no buffering.
        // The event field is Box<ActivityEvent> for recursive nesting.
        let wrapped = ActivityEvent::ChildActivity {
            child_id: self.child_id.clone(),
            agent_type: self.agent_type.clone(),
            event: Box::new(event),
        };
        self.parent_sink.emit(wrapped);
    }
}
