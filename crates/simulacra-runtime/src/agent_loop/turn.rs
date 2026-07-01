use super::*;

impl AgentLoop {
    /// Run exactly one LLM turn: call provider, process response, dispatch tool calls, return.
    ///
    /// The caller owns the messages vec and controls the loop. This is the
    /// building block for interactive mode.
    pub async fn run_single_turn(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<TurnResult, RuntimeError> {
        // 1. Check budget BEFORE the operation
        if let Err(exhausted) = self.budget.check_budget() {
            RuntimeMeters::get().budget_exhaustions.add(
                1,
                &[
                    KeyValue::new("simulacra.budget.resource", exhausted.resource.clone()),
                    KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                ],
            );
            return Ok(TurnResult::BudgetExhausted);
        }

        let tool_defs = self.tools.definitions();

        // 2. Journal TurnStart — must succeed before the LLM call side effect.
        self.journal_entry(JournalEntryKind::TurnStart)?;
        self.consume_replay_entry(&JournalEntryKind::TurnStart)?;

        // 3. Compact context (0 = unlimited → use u64::MAX as sentinel)
        let remaining_tokens = if self.budget.max_tokens == 0 {
            u64::MAX
        } else {
            self.budget
                .max_tokens
                .saturating_sub(self.budget.used_tokens)
        };
        let compacted = self.context_strategy.compact(messages, remaining_tokens);

        // 4. Journal LlmRequest — must succeed before invoking the provider.
        let llm_request = JournalEntryKind::LlmRequest {
            model: self.config.model.clone(),
            message_count: compacted.len(),
        };
        self.journal_entry(llm_request.clone())?;
        self.consume_replay_entry(&llm_request)?;

        // 5. Get LLM response (with optional governance hooks)
        // BEFORE hook
        if let Some(ref pipeline) = self.pipeline {
            let before_ctx = serde_json::json!({
                "model": &self.config.model,
                "message_count": compacted.len(),
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

        let response = if self.has_replay_entry() {
            let kind = self.take_replay_entry()?;
            replay_llm_response(&kind)?
        } else {
            self.provider
                .chat(&compacted, &tool_defs, &mut self.budget)
                .await
                .map_err(RuntimeError::from)?
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
        if !response.message.content.is_empty() {
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
            self.sink.emit(ActivityEvent::TurnComplete);
            return Ok(TurnResult::Complete(response.message));
        }

        // 10. Dispatch tool calls
        let mut tool_results = Vec::new();
        for tc in &response.message.tool_calls {
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

            self.journal_entry(JournalEntryKind::ToolCall {
                tool_call_id: Some(tc.id.clone()),
                tool_name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })?;
            self.consume_replay_entry(&JournalEntryKind::ToolCall {
                tool_call_id: Some(tc.id.clone()),
                tool_name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })?;

            let tool_start = Instant::now();
            let replayed_result = self.take_replay_tool_result(&tc.id, &tc.name)?;
            let (content, is_error) = match replayed_result {
                Some(result) => result,
                None => {
                    execute_tool_live(
                        &self.tools,
                        tc,
                        &self.config.capability,
                        &self.config.agent_id.0,
                    )
                    .await
                }
            };
            let tool_duration_ms = tool_start.elapsed().as_millis() as u64;

            // S019: Emit ToolOutput for each line of tool result content.
            // The agent loop owns the sink — tools return output through existing
            // channels, and the loop emits ToolOutput events per line.
            for line in content.lines() {
                self.sink.emit(ActivityEvent::ToolOutput {
                    tool_call_id: tc.id.clone(),
                    line: line.to_string(),
                });
            }

            // S019: Emit ToolFinish after execution
            self.sink.emit(ActivityEvent::ToolFinish {
                tool_call_id: tc.id.clone(),
                name: tc.name.clone(),
                is_error,
                duration_ms: tool_duration_ms,
                exit_code: None,
            });

            self.journal_entry(JournalEntryKind::ToolResult {
                tool_call_id: Some(tc.id.clone()),
                tool_name: tc.name.clone(),
                content: content.clone(),
                is_error,
            })?;

            let error_prefix = if is_error { "ERROR: " } else { "" };
            let tool_msg = Message {
                role: Role::Tool,
                content: format!("{error_prefix}{content}"),
                tool_calls: vec![],
                tool_call_id: Some(tc.id.clone()),
            };
            messages.push(tool_msg.clone());
            tool_results.push(tool_msg);
        }

        // S019: Emit TurnComplete on every return path
        self.sink.emit(ActivityEvent::TurnComplete);

        Ok(TurnResult::ToolCallsProcessed {
            assistant_message: response.message,
            tool_results,
        })
    }
}
