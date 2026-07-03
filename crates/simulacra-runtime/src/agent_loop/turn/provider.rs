use super::*;

impl AgentLoop {
    pub(super) async fn call_provider(
        &mut self,
        step: &StepContext,
        active_turn: &ActiveTurn,
    ) -> Result<ProviderCallOutcome, RuntimeError> {
        if let Some(streaming_provider) = self.provider.as_streaming() {
            let stream_sink = ProviderActivityStreamSink::new(Arc::clone(&self.sink));
            let chat = streaming_provider.chat_stream(
                step.messages(),
                step.tool_definitions(),
                &mut self.budget,
                &stream_sink,
            );
            let response = if let Some(cancellation) = self.cancellation.clone() {
                tokio::select! {
                    result = chat => result.map_err(RuntimeError::from)?,
                    () = wait_for_cancellation(cancellation) => {
                        active_turn.mark_cancelled();
                        return Ok(ProviderCallOutcome::Cancelled);
                    }
                }
            } else {
                chat.await.map_err(RuntimeError::from)?
            };
            if self.is_cancelled() || active_turn.state().cancelled {
                return Ok(ProviderCallOutcome::Cancelled);
            }
            Ok(ProviderCallOutcome::Response {
                response,
                streamed: true,
            })
        } else {
            let response = self
                .provider
                .chat(step.messages(), step.tool_definitions(), &mut self.budget)
                .await
                .map_err(RuntimeError::from)?;
            Ok(ProviderCallOutcome::Response {
                response,
                streamed: false,
            })
        }
    }
}

pub(super) async fn wait_for_cancellation(cancellation: crate::CancellationToken) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

struct ProviderActivityStreamSink {
    sink: Arc<dyn ActivitySink>,
    thinking: Mutex<Option<ThinkingState>>,
}

struct ThinkingState {
    started: Instant,
    chars: u64,
}

impl ProviderActivityStreamSink {
    fn new(sink: Arc<dyn ActivitySink>) -> Self {
        Self {
            sink,
            thinking: Mutex::new(None),
        }
    }
}

impl simulacra_types::ProviderStreamSink for ProviderActivityStreamSink {
    fn emit(&self, event: simulacra_types::ProviderStreamEvent) {
        match event {
            simulacra_types::ProviderStreamEvent::TextDelta { text } => {
                self.sink.emit(ActivityEvent::Token { text });
            }
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index,
                tool_call_id,
                name,
                arguments_delta,
            } => {
                self.sink.emit(ActivityEvent::ToolCallDelta {
                    index,
                    tool_call_id,
                    name,
                    arguments_delta,
                });
            }
            simulacra_types::ProviderStreamEvent::ThinkingStart => {
                if let Ok(mut thinking) = self.thinking.lock() {
                    *thinking = Some(ThinkingState {
                        started: Instant::now(),
                        chars: 0,
                    });
                }
                self.sink.emit(ActivityEvent::ThinkStart);
            }
            simulacra_types::ProviderStreamEvent::ThinkingDelta { text } => {
                if let Ok(mut thinking) = self.thinking.lock()
                    && let Some(state) = thinking.as_mut()
                {
                    state.chars = state.chars.saturating_add(text.chars().count() as u64);
                }
                self.sink.emit(ActivityEvent::ThinkDelta { text });
            }
            simulacra_types::ProviderStreamEvent::ThinkingEnd => {
                let (think_duration_ms, think_tokens) = self
                    .thinking
                    .lock()
                    .ok()
                    .and_then(|mut thinking| thinking.take())
                    .map(|state| (state.started.elapsed().as_millis() as u64, state.chars / 4))
                    .unwrap_or((0, 0));
                self.sink.emit(ActivityEvent::ThinkEnd {
                    think_duration_ms,
                    think_tokens,
                });
            }
        }
    }
}
