use super::*;

impl AgentLoop {
    /// Run the loop: receive task -> [LLM -> tool calls -> journal -> repeat] -> exit.
    pub async fn run(&mut self, task: &str) -> Result<AgentLoopOutput, RuntimeError> {
        use tracing::Instrument;
        let agent_span = tracing::info_span!(
            "invoke_agent",
            "gen_ai.operation.name" = "invoke_agent",
            "gen_ai.agent.name" = self.config.agent_id.0.as_str(),
        );
        self.run_inner(task).instrument(agent_span).await
    }

    async fn run_inner(&mut self, task: &str) -> Result<AgentLoopOutput, RuntimeError> {
        // Default conversation: [system, user(task)]. Overridden below if the
        // replay journal carries a checkpoint with captured messages.
        let mut messages = vec![
            Message {
                role: Role::System,
                content: self.config.system_prompt.clone(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: task.to_string(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];
        let mut total_usage = TokenUsage::default();
        let tool_defs = self.tools.definitions();

        // S006/S005: Restore full agent state from the latest checkpoint in replay.
        //
        // A `CheckpointData` captures three things taken at checkpoint time:
        //   - `budget_snapshot`: budget counters (always restored)
        //   - `messages`: conversation state up to the checkpoint
        //   - `vfs_snapshot`: serialized VFS state
        //
        // Previously only `budget_snapshot` was honored, meaning replay-from-
        // checkpoint would start with a fresh [system, user] conversation and
        // an un-restored filesystem — silently diverging from the checkpointed
        // state. We now restore all three so replay resumes at the exact
        // point the checkpoint was taken (LangGraph-style fork-from-checkpoint).
        if let Some(ref replay) = self.replay {
            let mut latest_checkpoint: Option<CheckpointData> = None;
            for entry in replay.entries() {
                if let JournalEntryKind::Checkpoint { snapshot_data } = &entry.entry
                    && let Ok(checkpoint) = serde_json::from_slice::<CheckpointData>(snapshot_data)
                {
                    // Keep the latest checkpoint — replay resumes from the most
                    // recent snapshot, matching the checkpoint/fork semantics.
                    latest_checkpoint = Some(checkpoint);
                }
            }
            if let Some(checkpoint) = latest_checkpoint {
                self.budget = checkpoint.budget_snapshot;
                self.sync_proc_state();
                if !checkpoint.messages.is_empty() {
                    messages = checkpoint.messages;
                }
                if let Some(ref vfs_bytes) = checkpoint.vfs_snapshot {
                    if let Some(ref vfs) = self.vfs {
                        let snapshot = VfsSnapshot {
                            data: vfs_bytes.clone(),
                        };
                        if let Err(e) = vfs.restore(&snapshot) {
                            tracing::warn!(
                                error = %e,
                                "failed to restore vfs snapshot from checkpoint — replay may diverge"
                            );
                        }
                    } else {
                        tracing::warn!(
                            "checkpoint contains vfs_snapshot but AgentLoop has no VFS — \
                             vfs state will not be restored (call AgentLoop::set_vfs)"
                        );
                    }
                }
            }
        }

        // Track replay metrics for ratio gauge
        let total_replay_entries = self.replay.as_ref().map(|r| r.remaining()).unwrap_or(0);

        // 0 = unlimited (use u32::MAX as sentinel)
        let effective_max_turns = if self.config.max_turns == 0 {
            u32::MAX
        } else {
            self.config.max_turns
        };
        for _turn in 0..effective_max_turns {
            // 1. Check budget BEFORE the operation
            if let Err(exhausted) = self.budget.check_budget() {
                tracing::warn!(
                    simulacra.agent.budget.resource = %exhausted.resource,
                    simulacra.agent.budget.used = %exhausted.used,
                    simulacra.agent.budget.limit = %exhausted.limit,
                    "budget exhausted"
                );
                RuntimeMeters::get().budget_exhaustions.add(
                    1,
                    &[
                        KeyValue::new("simulacra.budget.resource", exhausted.resource.clone()),
                        KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                    ],
                );
                return Err(exhausted.into());
            }

            // 2. Journal TurnStart — before any side effect in this turn.
            self.journal_entry(JournalEntryKind::TurnStart)?;
            // Consume TurnStart from replay if before frontier
            self.consume_replay_entry(&JournalEntryKind::TurnStart)?;

            // 3. Compact context (0 = unlimited → use u64::MAX as sentinel)
            let remaining_tokens = if self.budget.max_tokens == 0 {
                u64::MAX
            } else {
                self.budget
                    .max_tokens
                    .saturating_sub(self.budget.used_tokens)
            };
            let compacted = self.context_strategy.compact(&messages, remaining_tokens);

            // 4. Journal LlmRequest — BEFORE invoking the provider.
            let llm_request = JournalEntryKind::LlmRequest {
                model: self.config.model.clone(),
                message_count: compacted.len(),
            };
            self.journal_entry(llm_request.clone())?;
            // Consume LlmRequest from replay if before frontier
            self.consume_replay_entry(&llm_request)?;

            // 5. Get LLM response — from replay if available, otherwise live
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
            // Extended thinking blocks emit ThinkStart, ThinkDelta, ThinkEnd
            // with think_duration_ms and think_tokens derived from the stream.
            if !response.message.content.is_empty() {
                self.sink.emit(ActivityEvent::Token {
                    text: response.message.content.clone(),
                });
            }

            // 6. Journal LlmResponse BEFORE tool dispatch / returning result.
            self.journal_entry(JournalEntryKind::LlmResponse {
                model: response.model.clone(),
                token_usage: response.token_usage.clone(),
                finish_reason: format!("{:?}", response.finish_reason),
                assistant_message: Some(response.message.clone()),
            })?;

            // 7. Update usage — AgentLoop owns all budget accounting
            total_usage.input_tokens += response.token_usage.input_tokens;
            total_usage.output_tokens += response.token_usage.output_tokens;
            self.budget.used_tokens = self
                .budget
                .used_tokens
                .saturating_add(response.token_usage.total());
            self.budget.used_turns += 1;
            self.sync_proc_state();

            // S006: Emit budget remaining gauge after each budget-consuming operation
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

            // S009: Emit simulacra.agent.turns counter for per-agent turn tracking
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

            // 9. If no tool calls, exit Complete
            if response.message.tool_calls.is_empty() {
                self.emit_replay_ratio(total_replay_entries);
                let exit_reason = ExitReason::Complete;
                // S009: Log agent completion at INFO with exit reason and token total
                tracing::info!(
                    "gen_ai.agent.name" = self.config.agent_id.0.as_str(),
                    "simulacra.agent.exit_reason" = format!("{:?}", exit_reason).as_str(),
                    "simulacra.agent.token_total" = total_usage.total(),
                    "agent completed"
                );
                return Ok(AgentLoopOutput {
                    exit_reason,
                    messages,
                    token_usage: total_usage,
                    used_turns: self.budget.used_turns,
                    used_cost: self.budget.used_cost,
                });
            }

            // 10. Dispatch tool calls
            for tc in &response.message.tool_calls {
                // S010: Emit gen_ai.tool.message event for OTel observability
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

                // Journal ToolCall BEFORE execution.
                self.journal_entry(JournalEntryKind::ToolCall {
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })?;
                // Consume ToolCall from replay if before frontier
                self.consume_replay_entry(&JournalEntryKind::ToolCall {
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })?;

                // Get tool result — from replay if available, otherwise live
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

                // Journal ToolResult BEFORE returning to the loop (S005).
                self.journal_entry(JournalEntryKind::ToolResult {
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: tc.name.clone(),
                    content: content.clone(),
                    is_error,
                })?;

                // 11. Append tool result as Tool message
                // is_error is preserved for the provider to signal tool failure to the model
                let error_prefix = if is_error { "ERROR: " } else { "" };
                messages.push(Message {
                    role: Role::Tool,
                    content: format!("{error_prefix}{content}"),
                    tool_calls: vec![],
                    tool_call_id: Some(tc.id.clone()),
                });
            }
        }

        // Max turns reached
        self.emit_replay_ratio(total_replay_entries);
        Ok(AgentLoopOutput {
            exit_reason: ExitReason::MaxTurns,
            messages,
            token_usage: total_usage,
            used_turns: self.budget.used_turns,
            used_cost: self.budget.used_cost,
        })
    }

    /// Emit thinking block events from a provider's extended thinking content.
    ///
    /// Called when the provider stream delivers thinking blocks. The agent loop
    /// measures think_duration_ms from ThinkStart to ThinkEnd and estimates
    /// think_tokens by dividing character count by 4 (approximate token count).
    #[allow(dead_code)]
    fn emit_thinking_events(&self, thinking_text: &str, think_start: Instant) {
        self.sink.emit(ActivityEvent::ThinkStart);
        // Emit the thinking text as ThinkDelta chunks
        self.sink.emit(ActivityEvent::ThinkDelta {
            text: thinking_text.to_string(),
        });
        let think_duration_ms = think_start.elapsed().as_millis() as u64;
        let think_tokens = (thinking_text.len() as u64) / 4;
        self.sink.emit(ActivityEvent::ThinkEnd {
            think_duration_ms,
            think_tokens,
        });
    }

    /// Emit the replay ratio gauge if replay was active.
    fn emit_replay_ratio(&self, total_replay_entries: usize) {
        if total_replay_entries > 0 {
            let replayed = self
                .replay
                .as_ref()
                .map(|r| total_replay_entries - r.remaining())
                .unwrap_or(0);
            let ratio = replayed as f64 / total_replay_entries as f64;
            tracing::info!(
                simulacra.journal.replay.ratio = ratio,
                "journal replay ratio"
            );
        }
    }
}
