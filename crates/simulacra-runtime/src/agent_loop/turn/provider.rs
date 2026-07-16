use super::*;

impl AgentLoop {
    pub(super) async fn call_provider(
        &mut self,
        step: &StepContext,
        active_turn: &ActiveTurn,
    ) -> Result<ProviderCallOutcome, RuntimeError> {
        // `max_sub_agents` is a delegation-only limit. Providers still call
        // `ResourceBudget::check_budget`, so give them a scoped working copy
        // that retains all ordinary limits and usage but disables this one.
        // Provider-authored usage (currently cost) is merged back below.
        let provider_budget_before = self.budget.clone();
        let mut provider_budget = self.budget.clone();
        provider_budget.max_sub_agents = 0;

        if let Some(streaming_provider) = self.provider.as_streaming() {
            let stream_sink = ProviderActivityStreamSink::new(Arc::clone(&self.sink));
            let chat = streaming_provider.chat_stream(
                step.messages(),
                step.tool_definitions(),
                &mut provider_budget,
                &stream_sink,
            );
            let provider_result = if let Some(cancellation) = self.cancellation.clone() {
                tokio::select! {
                    result = chat => Some(result),
                    () = wait_for_cancellation(cancellation) => {
                        active_turn.mark_cancelled();
                        None
                    }
                }
            } else {
                Some(chat.await)
            };
            self.merge_provider_budget_delta(&provider_budget_before, &provider_budget);
            let Some(provider_result) = provider_result else {
                return Ok(ProviderCallOutcome::Cancelled);
            };
            let response = provider_result.map_err(RuntimeError::from)?;
            if self.is_cancelled() || active_turn.state().cancelled {
                return Ok(ProviderCallOutcome::Cancelled);
            }
            Ok(ProviderCallOutcome::Response {
                response,
                streamed: true,
            })
        } else {
            let provider_result = self
                .provider
                .chat(
                    step.messages(),
                    step.tool_definitions(),
                    &mut provider_budget,
                )
                .await;
            self.merge_provider_budget_delta(&provider_budget_before, &provider_budget);
            let response = provider_result.map_err(RuntimeError::from)?;
            Ok(ProviderCallOutcome::Response {
                response,
                streamed: false,
            })
        }
    }

    fn merge_provider_budget_delta(
        &mut self,
        before: &ResourceBudget,
        provider_budget: &ResourceBudget,
    ) {
        self.budget.used_tokens = self.budget.used_tokens.saturating_add(
            provider_budget
                .used_tokens
                .saturating_sub(before.used_tokens),
        );
        self.budget.used_turns = self
            .budget
            .used_turns
            .saturating_add(provider_budget.used_turns.saturating_sub(before.used_turns));
        if provider_budget.used_cost > before.used_cost {
            self.budget.used_cost += provider_budget.used_cost - before.used_cost;
        }
        self.budget.used_vfs_bytes = self.budget.used_vfs_bytes.saturating_add(
            provider_budget
                .used_vfs_bytes
                .saturating_sub(before.used_vfs_bytes),
        );
        self.budget.used_fuel = self
            .budget
            .used_fuel
            .saturating_add(provider_budget.used_fuel.saturating_sub(before.used_fuel));
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
    tool_calls: Mutex<ToolCallStreamState>,
}

#[derive(Default)]
struct ToolCallStreamState {
    names_by_index: std::collections::HashMap<u64, String>,
    names_by_id: std::collections::HashMap<String, String>,
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
            tool_calls: Mutex::new(ToolCallStreamState::default()),
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
                mut arguments_delta,
            } => {
                let is_mcp_meta_tool = self
                    .tool_calls
                    .lock()
                    .ok()
                    .map(|mut tool_calls| {
                        if let Some(name) = name.as_ref() {
                            tool_calls.names_by_index.insert(index, name.clone());
                            if let Some(tool_call_id) = tool_call_id.as_ref() {
                                tool_calls
                                    .names_by_id
                                    .insert(tool_call_id.clone(), name.clone());
                            }
                        }
                        name.as_deref()
                            .or_else(|| {
                                tool_call_id.as_ref().and_then(|tool_call_id| {
                                    tool_calls.names_by_id.get(tool_call_id).map(String::as_str)
                                })
                            })
                            .or_else(|| tool_calls.names_by_index.get(&index).map(String::as_str))
                            .is_some_and(is_mcp_meta_tool)
                    })
                    .unwrap_or_else(|| name.as_deref().is_some_and(is_mcp_meta_tool));
                if is_mcp_meta_tool {
                    arguments_delta = "[REDACTED]".into();
                }
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

fn is_mcp_meta_tool(name: &str) -> bool {
    matches!(name, "mcp_search" | "mcp_call")
}
