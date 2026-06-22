use crate::Message;

/// Strategy for compacting conversation context to fit within token limits.
/// Object-safe.
pub trait ContextStrategy: Send + Sync + 'static {
    fn compact(&self, messages: &[Message], token_limit: u64) -> Vec<Message>;
}
