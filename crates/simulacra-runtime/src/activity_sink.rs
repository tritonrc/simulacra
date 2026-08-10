//! Activity sink implementations for S019 activity events.
//!
//! The `ActivitySink` trait defines a non-blocking push interface for activity
//! events. Three implementations are provided:
//!
//! - `NoopActivitySink` — discards all events (headless mode, tests)
//! - `ChannelActivitySink` — sends via `tokio::sync::mpsc::UnboundedSender<ActivityEvent>`
//! - `ForwardingActivitySink` — wraps child events in `ChildActivity` and forwards to parent

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use simulacra_types::{ActivityEvent, AgentId};

/// Non-blocking push interface for activity events.
///
/// Implementations MUST be non-blocking: buffer or drop events rather than
/// blocking the agent loop. The trait is object-safe for `Arc<dyn ActivitySink>`.
pub trait ActivitySink: Send + Sync + 'static {
    fn emit(&self, event: ActivityEvent);

    /// Return the forwarding sink owned by an accepted immediate parent, when
    /// this sink is routing a supervised child tree.
    ///
    /// The default preserves the simple sink contract used by headless and
    /// unit-test sinks. Production hosts that supervise descendants install a
    /// `RoutedActivitySink` at the root.
    fn immediate_parent_sink(&self, _parent_id: &AgentId) -> Option<Arc<dyn ActivitySink>> {
        None
    }

    /// Register the sink through which this child's own events reach its
    /// immediate parent. This is deliberately separate from `emit`: lifecycle
    /// events are owned by the supervisor, while normal child activity is
    /// emitted by the child loop itself.
    fn register_child_sink(&self, _child_id: AgentId, _sink: Arc<dyn ActivitySink>) {}
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

/// Root activity sink with an immediate-parent routing table.
///
/// A child loop registers its forwarding sink once it is constructed. Later,
/// when that child spawns a descendant, the supervisor uses the registered
/// sink for the descendant's lifecycle events. This keeps every supervision
/// hop explicit: root -> child -> grandchild is never flattened into a direct
/// root -> grandchild event.
pub struct RoutedActivitySink {
    inner: Arc<dyn ActivitySink>,
    // The router owns a child route until the supervisor tree is dropped.
    // `ForwardingActivitySink` holds its parent weakly, which prevents this
    // table from forming a shutdown cycle with the root router.
    child_sinks: Mutex<HashMap<AgentId, Arc<dyn ActivitySink>>>,
}

impl RoutedActivitySink {
    pub fn new(inner: Arc<dyn ActivitySink>) -> Self {
        Self {
            inner,
            child_sinks: Mutex::new(HashMap::new()),
        }
    }
}

impl ActivitySink for RoutedActivitySink {
    fn emit(&self, event: ActivityEvent) {
        self.inner.emit(event);
    }

    fn immediate_parent_sink(&self, parent_id: &AgentId) -> Option<Arc<dyn ActivitySink>> {
        self.child_sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(parent_id)
            .cloned()
    }

    fn register_child_sink(&self, child_id: AgentId, sink: Arc<dyn ActivitySink>) {
        self.child_sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(child_id, sink);
    }
}

/// Wraps each child event in `ChildActivity` and forwards to the parent's sink.
///
/// Used when creating a child `AgentLoop` so that the parent sees all child
/// events nested under the child's identity.
pub struct ForwardingActivitySink {
    child_id: String,
    placement: String,
    parent_sink: Weak<dyn ActivitySink>,
}

impl ForwardingActivitySink {
    pub fn new(child_id: String, placement: String, parent_sink: Arc<dyn ActivitySink>) -> Self {
        Self {
            child_id,
            placement,
            parent_sink: Arc::downgrade(&parent_sink),
        }
    }
}

impl ActivitySink for ForwardingActivitySink {
    fn emit(&self, event: ActivityEvent) {
        // Wrap in ChildActivity and forward immediately — no buffering.
        // The event field is Box<ActivityEvent> for recursive nesting.
        let wrapped = ActivityEvent::ChildActivity {
            child_id: self.child_id.clone(),
            placement: self.placement.clone(),
            event: Box::new(event),
        };
        if let Some(parent_sink) = self.parent_sink.upgrade() {
            parent_sink.emit(wrapped);
        }
    }
}
