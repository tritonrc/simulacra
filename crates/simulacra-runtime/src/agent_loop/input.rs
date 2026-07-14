/// Runtime queue for parent-supplied child steering messages.
///
/// Steering is cooperative: messages are drained only between model turns and
/// appended as user messages before the next provider request.
pub struct AgentInputQueue {
    receiver: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// Cloneable handle held by the supervisor for a live child agent.
#[derive(Debug, Clone)]
pub struct ChildInputHandle {
    sender: tokio::sync::mpsc::UnboundedSender<String>,
}

impl AgentInputQueue {
    pub fn new() -> (Self, ChildInputHandle) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self { receiver }, ChildInputHandle { sender })
    }

    pub(super) fn drain(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        while let Ok(message) = self.receiver.try_recv() {
            messages.push(message);
        }
        messages
    }

    /// Await the next queued steering message.
    ///
    /// Returns `None` once every [`ChildInputHandle`] for this queue has
    /// dropped, without waiting for a message that will never arrive.
    pub async fn recv(&mut self) -> Option<String> {
        self.receiver.recv().await
    }
}

impl ChildInputHandle {
    pub fn enqueue(&self, message: String) -> Result<(), String> {
        self.sender
            .send(message)
            .map_err(|_| "child input queue is closed".to_string())
    }
}
