use super::*;

mod provider;
mod tools;

pub(super) struct TurnExecution {
    pub(super) result: TurnResult,
    pub(super) token_usage: TokenUsage,
    pub(super) budget_exhausted: Option<simulacra_types::BudgetExhausted>,
}

pub(super) enum ProviderCallOutcome {
    Response {
        response: simulacra_types::ProviderResponse,
        streamed: bool,
    },
    Cancelled,
}

pub(super) enum ToolApprovalDecision {
    Approved,
    Denied(String),
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
        self.check_pending_spawn_hook_kill()?;
        if self.is_cancelled() {
            return Ok(Self::cancelled_execution());
        }
        self.drain_input_queue(messages);
        let active_turn = self.active_turn();
        self.refresh_budget_from_mirror();

        // The child-count limit governs delegation, not ordinary parent work.
        // SpawnAgentTool and AgentSupervisor enforce it at the spawn boundary.
        let mut operation_budget = self.budget.clone();
        operation_budget.max_sub_agents = 0;
        if let Err(exhausted) = operation_budget.check_budget() {
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

        self.journal_entry(JournalEntryKind::TurnStart)?;
        self.consume_replay_entry(&JournalEntryKind::TurnStart)?;

        let remaining_tokens =
            compaction_token_limit(self.budget.max_tokens, self.budget.used_tokens);
        let compacted = self.context_strategy.compact(messages, remaining_tokens);
        let step = StepContext::new(compacted, tool_defs);
        let budget_before_operation = self.budget.clone();

        let llm_request = JournalEntryKind::LlmRequest {
            model: self.config.model.clone(),
            message_count: step.messages().len(),
        };
        self.journal_entry(llm_request.clone())?;
        self.consume_replay_entry(&llm_request)?;

        if let Some(ref pipeline) = self.pipeline {
            let before_ctx = serde_json::json!({
                "model": &self.config.model,
                "message_count": step.messages().len(),
            })
            .to_string();
            match pipeline
                .run_before_attributed(simulacra_hooks::verdict::Operation::Llm, &before_ctx)
            {
                Ok((simulacra_hooks::Verdict::Continue(_), _, _)) => {}
                Ok((simulacra_hooks::Verdict::Deny(reason), _, Some(hook))) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: hook,
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookDenial(reason));
                }
                Ok((simulacra_hooks::Verdict::Deny(_), _, None)) => {
                    return Err(RuntimeError::HookError(
                        "LLM before-hook denied without hook attribution".into(),
                    ));
                }
                Ok((simulacra_hooks::Verdict::Kill(reason), _, Some(hook))) => {
                    self.journal_entry(JournalEntryKind::HookKill {
                        hook_name: hook.clone(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookKill { hook, reason });
                }
                Ok((simulacra_hooks::Verdict::Kill(_), _, None)) => {
                    return Err(RuntimeError::HookError(
                        "LLM before-hook killed without hook attribution".into(),
                    ));
                }
                Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                    self.journal_entry(JournalEntryKind::HookKill {
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
            Ok(ProviderCallOutcome::Response {
                response: replay_llm_response(&kind)?,
                streamed: false,
            })
        } else {
            if self.is_cancelled() {
                active_turn.mark_cancelled();
                return Ok(Self::cancelled_execution());
            }
            self.call_provider(&step, &active_turn).await
        };

        // Check before propagating a provider cancellation/error so an
        // asynchronous governance kill that arrived during the await is never
        // hidden by the provider's concurrent terminal outcome.
        self.check_pending_spawn_hook_kill()?;
        let provider_outcome = provider_outcome?;
        let (response, streamed) = match provider_outcome {
            ProviderCallOutcome::Response { response, streamed } => (response, streamed),
            ProviderCallOutcome::Cancelled => return Ok(Self::cancelled_execution()),
        };

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

        if !streamed && !response.message.content.is_empty() {
            self.sink.emit(ActivityEvent::Token {
                text: response.message.content.clone(),
            });
        }

        self.journal_entry(JournalEntryKind::LlmResponse {
            model: response.model.clone(),
            token_usage: response.token_usage.clone(),
            finish_reason: format!("{:?}", response.finish_reason),
            assistant_message: Some(response.message.clone()),
        })?;

        self.budget.used_tokens = self
            .budget
            .used_tokens
            .saturating_add(response.token_usage.total());
        self.budget.used_turns += 1;
        self.merge_budget_into_mirror(&budget_before_operation);

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

        messages.push(response.message.clone());

        if response.message.tool_calls.is_empty() {
            self.check_pending_spawn_hook_kill()?;
            if emit_turn_complete {
                self.sink.emit(ActivityEvent::TurnComplete);
            }
            return Ok(TurnExecution {
                result: TurnResult::Complete(response.message),
                token_usage: response.token_usage,
                budget_exhausted: None,
            });
        }

        let tool_results = self
            .dispatch_tool_calls(&response.message.tool_calls, &active_turn)
            .await?;
        self.check_pending_spawn_hook_kill()?;
        messages.extend(tool_results.iter().cloned());

        self.check_pending_spawn_hook_kill()?;

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

    /// Advance the per-loop journal cursor and surface the first new spawn kill.
    /// Existing entries present when the loop was constructed are deliberately
    /// outside the cursor, so resuming an agent cannot consume a kill belonging
    /// to an earlier run.
    fn check_pending_spawn_hook_kill(&mut self) -> Result<(), RuntimeError> {
        if let Some(decision) = self.policy_kill_signal.decision() {
            return Err(RuntimeError::HookKill {
                hook: decision.hook,
                reason: decision.reason,
            });
        }
        let Some(frontier) = self.spawn_hook_journal_frontier else {
            self.spawn_hook_journal_frontier = Some(
                self.journal
                    .read_all(&self.config.agent_id)
                    .map_err(RuntimeError::Journal)?
                    .len(),
            );
            return Ok(());
        };
        let entries = match self.journal.read_from(&self.config.agent_id, frontier) {
            Ok(entries) => entries,
            Err(error) => {
                if let Some(decision) = self.policy_kill_signal.decision() {
                    return Err(RuntimeError::HookKill {
                        hook: decision.hook,
                        reason: decision.reason,
                    });
                }
                return Err(RuntimeError::Journal(error));
            }
        };
        self.spawn_hook_journal_frontier = Some(frontier.saturating_add(entries.len()));
        if let Some((hook, reason)) = entries.into_iter().find_map(|entry| match entry.entry {
            JournalEntryKind::HookKill {
                hook_name,
                operation,
                reason,
            } if operation == "spawn" => Some((hook_name, reason)),
            _ => None,
        }) {
            return Err(RuntimeError::HookKill { hook, reason });
        }
        // The journal read may have blocked while a child settled. Recheck the
        // live signal after that boundary so a kill decided just after the
        // journal snapshot cannot lose to a normal completion.
        if let Some(decision) = self.policy_kill_signal.decision() {
            return Err(RuntimeError::HookKill {
                hook: decision.hook,
                reason: decision.reason,
            });
        }
        Ok(())
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

    fn drain_input_queue(&mut self, messages: &mut Vec<Message>) {
        let Some(input_queue) = self.input_queue.as_mut() else {
            return;
        };
        messages.extend(input_queue.drain().into_iter().map(|content| Message {
            role: Role::User,
            content,
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }));
    }

    fn cancelled_execution() -> TurnExecution {
        TurnExecution {
            result: TurnResult::Cancelled,
            token_usage: TokenUsage::default(),
            budget_exhausted: None,
        }
    }
}
