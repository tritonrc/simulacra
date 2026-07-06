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
                provider_content: vec![],
            },
            Message {
                role: Role::User,
                content: task.to_string(),
                tool_calls: vec![],
                tool_call_id: None,
                provider_content: vec![],
            },
        ];
        let mut total_usage = TokenUsage::default();

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
            let turn = self.execute_turn(&mut messages, true).await?;
            total_usage.input_tokens += turn.token_usage.input_tokens;
            total_usage.output_tokens += turn.token_usage.output_tokens;

            match turn.result {
                TurnResult::Complete(_) => {
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
                        reported_tool_uses: None,
                        used_turns: self.budget.used_turns,
                        used_cost: self.budget.used_cost,
                    });
                }
                TurnResult::ToolCallsProcessed { .. } => {}
                TurnResult::BudgetExhausted => {
                    let exhausted = turn.budget_exhausted.ok_or_else(|| {
                        RuntimeError::Session(
                            "budget exhausted turn result did not include error details".into(),
                        )
                    })?;
                    tracing::warn!(
                        simulacra.agent.budget.resource = %exhausted.resource,
                        simulacra.agent.budget.used = %exhausted.used,
                        simulacra.agent.budget.limit = %exhausted.limit,
                        "budget exhausted"
                    );
                    return Err(exhausted.into());
                }
                TurnResult::Cancelled => {
                    self.emit_replay_ratio(total_replay_entries);
                    let exit_reason = ExitReason::Cancelled;
                    tracing::info!(
                        "gen_ai.agent.name" = self.config.agent_id.0.as_str(),
                        "simulacra.agent.exit_reason" = format!("{:?}", exit_reason).as_str(),
                        "simulacra.agent.token_total" = total_usage.total(),
                        "agent cancelled"
                    );
                    return Ok(AgentLoopOutput {
                        exit_reason,
                        messages,
                        token_usage: total_usage,
                        reported_tool_uses: None,
                        used_turns: self.budget.used_turns,
                        used_cost: self.budget.used_cost,
                    });
                }
            }
        }

        // Max turns reached
        self.emit_replay_ratio(total_replay_entries);
        Ok(AgentLoopOutput {
            exit_reason: ExitReason::MaxTurns,
            messages,
            token_usage: total_usage,
            reported_tool_uses: None,
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
