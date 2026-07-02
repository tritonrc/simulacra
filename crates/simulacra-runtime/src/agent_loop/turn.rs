use super::*;

pub(super) struct TurnExecution {
    pub(super) result: TurnResult,
    pub(super) token_usage: TokenUsage,
    pub(super) budget_exhausted: Option<simulacra_types::BudgetExhausted>,
}

enum ProviderCallOutcome {
    Response {
        response: simulacra_types::ProviderResponse,
        streamed: bool,
    },
    Cancelled,
}

impl AgentLoop {
    /// Run exactly one LLM turn: call provider, process response, dispatch tool calls, return.
    ///
    /// The caller owns the messages vec and controls the loop. This is the
    /// building block for interactive mode.
    pub async fn run_single_turn(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<TurnResult, RuntimeError> {
        Ok(self.execute_turn(messages, true).await?.result)
    }

    pub(super) async fn execute_turn(
        &mut self,
        messages: &mut Vec<Message>,
        emit_turn_complete: bool,
    ) -> Result<TurnExecution, RuntimeError> {
        if self.is_cancelled() {
            return Ok(Self::cancelled_execution());
        }
        let active_turn = self.active_turn();

        // 1. Check budget BEFORE the operation
        if let Err(exhausted) = self.budget.check_budget() {
            RuntimeMeters::get().budget_exhaustions.add(
                1,
                &[
                    KeyValue::new("simulacra.budget.resource", exhausted.resource.clone()),
                    KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                ],
            );
            return Ok(TurnExecution {
                result: TurnResult::BudgetExhausted,
                token_usage: TokenUsage::default(),
                budget_exhausted: Some(exhausted),
            });
        }

        let tool_defs = self.tools.definitions();

        // 2. Journal TurnStart — must succeed before the LLM call side effect.
        self.journal_entry(JournalEntryKind::TurnStart)?;
        self.consume_replay_entry(&JournalEntryKind::TurnStart)?;

        // 3. Compact context. The compaction window is bounded by model
        // context, not only the cumulative cost budget.
        let remaining_tokens =
            compaction_token_limit(self.budget.max_tokens, self.budget.used_tokens);
        let compacted = self.context_strategy.compact(messages, remaining_tokens);
        let step = StepContext::new(compacted, tool_defs);

        // 4. Journal LlmRequest — must succeed before invoking the provider.
        let llm_request = JournalEntryKind::LlmRequest {
            model: self.config.model.clone(),
            message_count: step.messages().len(),
        };
        self.journal_entry(llm_request.clone())?;
        self.consume_replay_entry(&llm_request)?;

        // 5. Get LLM response (with optional governance hooks)
        // BEFORE hook
        if let Some(ref pipeline) = self.pipeline {
            let before_ctx = serde_json::json!({
                "model": &self.config.model,
                "message_count": step.messages().len(),
            })
            .to_string();
            match pipeline.run_before(simulacra_hooks::verdict::Operation::Llm, &before_ctx) {
                Ok((simulacra_hooks::Verdict::Continue(_), _)) => {}
                Ok((simulacra_hooks::Verdict::Deny(reason), _)) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: "llm:before".into(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookDenial(reason));
                }
                Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
                    unreachable!("Kill is returned as Err from run_before")
                }
                Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: hook.clone(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookKill { hook, reason });
                }
                Err(e) => {
                    return Err(RuntimeError::HookError(e.to_string()));
                }
            }
        }

        let provider_outcome = if self.has_replay_entry() {
            let kind = self.take_replay_entry()?;
            ProviderCallOutcome::Response {
                response: replay_llm_response(&kind)?,
                streamed: false,
            }
        } else {
            if self.is_cancelled() {
                active_turn.mark_cancelled();
                return Ok(Self::cancelled_execution());
            }
            self.call_provider(&step, &active_turn).await?
        };
        let (response, streamed) = match provider_outcome {
            ProviderCallOutcome::Response { response, streamed } => (response, streamed),
            ProviderCallOutcome::Cancelled => return Ok(Self::cancelled_execution()),
        };

        // AFTER hook
        if let Some(ref pipeline) = self.pipeline {
            let after_ctx = serde_json::json!({
                "model": &response.model,
                "content": &response.message.content,
                "tool_calls": &response.message.tool_calls,
                "usage": {
                    "input_tokens": response.token_usage.input_tokens,
                    "output_tokens": response.token_usage.output_tokens,
                },
            })
            .to_string();
            match pipeline.run_after(simulacra_hooks::verdict::Operation::Llm, &after_ctx) {
                Ok(_) => {}
                Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: hook.clone(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookKill { hook, reason });
                }
                Err(e) => {
                    return Err(RuntimeError::HookError(e.to_string()));
                }
            }
        }

        // S019: Emit Token event for the response text (non-streaming path).
        // When Provider supports streaming, tokens are emitted incrementally.
        if !streamed && !response.message.content.is_empty() {
            self.sink.emit(ActivityEvent::Token {
                text: response.message.content.clone(),
            });
        }

        // 6. Journal LlmResponse — must succeed before we expose the LLM output
        // to the caller / tool dispatch below.
        self.journal_entry(JournalEntryKind::LlmResponse {
            model: response.model.clone(),
            token_usage: response.token_usage.clone(),
            finish_reason: format!("{:?}", response.finish_reason),
            assistant_message: Some(response.message.clone()),
        })?;

        // 7. Update budget
        self.budget.used_tokens = self
            .budget
            .used_tokens
            .saturating_add(response.token_usage.total());
        self.budget.used_turns += 1;
        self.sync_proc_state();

        // Budget remaining gauges
        let remaining_turns = self.budget.max_turns.saturating_sub(self.budget.used_turns);
        let remaining_tokens = self
            .budget
            .max_tokens
            .saturating_sub(self.budget.used_tokens);
        tracing::info!(
            simulacra.agent.budget.remaining = remaining_turns as u64,
            simulacra.agent.budget.resource = "turns",
            "budget remaining"
        );
        tracing::info!(
            simulacra.agent.budget.remaining = remaining_tokens,
            simulacra.agent.budget.resource = "tokens",
            "budget remaining"
        );
        tracing::info!(simulacra.agent.turns = 1u64, "agent turn completed");

        // S010: Record OTel meter observations for turn completion
        {
            let meters = RuntimeMeters::get();
            let attrs = &[
                KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                KeyValue::new("gen_ai.request.model", self.config.model.clone()),
            ];
            meters.turns_counter.add(1, attrs);
            meters
                .budget_tokens_used
                .add(response.token_usage.total(), attrs);
            meters.budget_turns_used.add(1, attrs);
        }

        // 8. Append assistant message
        messages.push(response.message.clone());

        // 9. If no tool calls, return Complete
        if response.message.tool_calls.is_empty() {
            // S019: Emit TurnComplete on every return path
            if emit_turn_complete {
                self.sink.emit(ActivityEvent::TurnComplete);
            }
            return Ok(TurnExecution {
                result: TurnResult::Complete(response.message),
                token_usage: response.token_usage,
                budget_exhausted: None,
            });
        }

        // 10. Dispatch tool calls
        let tool_results = self
            .dispatch_tool_calls(&response.message.tool_calls, &active_turn)
            .await?;
        messages.extend(tool_results.iter().cloned());

        // S019: Emit TurnComplete on every return path
        if emit_turn_complete {
            self.sink.emit(ActivityEvent::TurnComplete);
        }

        Ok(TurnExecution {
            result: TurnResult::ToolCallsProcessed {
                assistant_message: response.message,
                tool_results,
            },
            token_usage: response.token_usage,
            budget_exhausted: None,
        })
    }

    fn active_turn(&self) -> ActiveTurn {
        ActiveTurn::new(TurnContext::new(
            self.config.agent_id.clone(),
            self.config.model.clone(),
            self.config.capability.clone(),
            self.cancellation.clone(),
        ))
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(crate::CancellationToken::is_cancelled)
    }

    fn cancelled_execution() -> TurnExecution {
        TurnExecution {
            result: TurnResult::Cancelled,
            token_usage: TokenUsage::default(),
            budget_exhausted: None,
        }
    }

    async fn call_provider(
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
                    () = wait_for_provider_cancellation(cancellation) => {
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

    fn tool_call_runtime(&self) -> ToolCallRuntime {
        ToolCallRuntime::new(
            Arc::clone(&self.tools),
            self.config.capability.clone(),
            self.config.agent_id.0.clone(),
            self.cancellation.clone(),
        )
    }

    async fn dispatch_tool_calls(
        &mut self,
        tool_calls: &[simulacra_types::ToolCallMessage],
        active_turn: &ActiveTurn,
    ) -> Result<Vec<Message>, RuntimeError> {
        let runtime = self.tool_call_runtime();
        if !self.has_replay_entry() && runtime.supports_parallel_batch(tool_calls) {
            let mut starts = Vec::with_capacity(tool_calls.len());
            for tc in tool_calls {
                active_turn.record_tool_call();
                self.start_tool_call(tc)?;
                starts.push(Instant::now());
            }
            let results = runtime.execute_parallel_batch(tool_calls).await;
            let mut messages = Vec::with_capacity(results.len());
            for ((tc, result), started) in tool_calls.iter().zip(results).zip(starts) {
                messages.push(self.finish_tool_call(tc, result, started, active_turn)?);
            }
            return Ok(messages);
        }

        let mut messages = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls {
            active_turn.record_tool_call();
            self.start_tool_call(tc)?;
            let started = Instant::now();
            let replayed_result = self.take_replay_tool_result(&tc.id, &tc.name)?;
            let result = match replayed_result {
                Some((content, is_error)) => ToolExecutionResult {
                    content,
                    is_error,
                    cancelled: false,
                },
                None => runtime.execute_one(tc).await,
            };
            messages.push(self.finish_tool_call(tc, result, started, active_turn)?);
        }
        Ok(messages)
    }

    fn start_tool_call(
        &mut self,
        tc: &simulacra_types::ToolCallMessage,
    ) -> Result<(), RuntimeError> {
        tracing::info!(
            "gen_ai.tool.message" = format!("tool_call: {}", tc.name),
            tool_name = tc.name.as_str(),
            tool_call_id = tc.id.as_str(),
        );

        // S019: Emit ToolStart before execution
        self.sink.emit(ActivityEvent::ToolStart {
            tool_call_id: tc.id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        });

        let tool_call = JournalEntryKind::ToolCall {
            tool_call_id: Some(tc.id.clone()),
            tool_name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        };
        self.journal_entry(tool_call.clone())?;
        self.consume_replay_entry(&tool_call)
    }

    fn finish_tool_call(
        &mut self,
        tc: &simulacra_types::ToolCallMessage,
        result: ToolExecutionResult,
        started: Instant,
        active_turn: &ActiveTurn,
    ) -> Result<Message, RuntimeError> {
        if result.cancelled {
            active_turn.mark_cancelled();
        }

        let tool_duration_ms = started.elapsed().as_millis() as u64;

        // S019: Emit ToolOutput for each line of tool result content.
        // The agent loop owns the sink — tools return output through existing
        // channels, and the loop emits ToolOutput events per line.
        for line in result.content.lines() {
            self.sink.emit(ActivityEvent::ToolOutput {
                tool_call_id: tc.id.clone(),
                line: line.to_string(),
            });
        }

        // S019: Emit ToolFinish after execution
        self.sink.emit(ActivityEvent::ToolFinish {
            tool_call_id: tc.id.clone(),
            name: tc.name.clone(),
            is_error: result.is_error,
            duration_ms: tool_duration_ms,
            exit_code: None,
        });

        self.journal_entry(JournalEntryKind::ToolResult {
            tool_call_id: Some(tc.id.clone()),
            tool_name: tc.name.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
        })?;

        let error_prefix = if result.is_error { "ERROR: " } else { "" };
        Ok(Message {
            role: Role::Tool,
            content: format!("{error_prefix}{}", result.content),
            tool_calls: vec![],
            tool_call_id: Some(tc.id.clone()),
        })
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

async fn wait_for_provider_cancellation(cancellation: crate::CancellationToken) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
