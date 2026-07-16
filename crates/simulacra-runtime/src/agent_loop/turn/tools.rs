use super::*;

use super::provider::wait_for_cancellation;

impl AgentLoop {
    fn tool_call_runtime(&self) -> ToolCallRuntime {
        ToolCallRuntime::new(
            Arc::clone(&self.tools),
            self.config.capability.clone(),
            self.config.agent_id.0.clone(),
            self.cancellation.clone(),
        )
    }

    pub(super) async fn dispatch_tool_calls(
        &mut self,
        tool_calls: &[simulacra_types::ToolCallMessage],
        active_turn: &ActiveTurn,
    ) -> Result<Vec<Message>, RuntimeError> {
        let runtime = self.tool_call_runtime();
        if !self.requires_tool_approval()
            && !self.has_replay_entry()
            && runtime.supports_parallel_batch(tool_calls)
        {
            let mut starts = Vec::with_capacity(tool_calls.len());
            for tc in tool_calls {
                active_turn.record_tool_call();
                self.journal_tool_call(tc)?;
                self.emit_tool_start(tc);
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
            let started = Instant::now();
            self.journal_tool_call(tc)?;
            let replayed_result = self.take_replay_tool_result(&tc.id, &tc.name)?;
            let was_replayed = replayed_result.is_some();
            let result = match replayed_result {
                Some((content, is_error)) => ToolExecutionResult {
                    content,
                    is_error,
                    cancelled: false,
                },
                None => match self.await_tool_approval(tc).await? {
                    ToolApprovalDecision::Approved => {
                        self.emit_tool_start(tc);
                        runtime.execute_one(tc).await
                    }
                    ToolApprovalDecision::Denied(reason) => ToolExecutionResult {
                        content: format!("approval denied: {reason}"),
                        is_error: true,
                        cancelled: false,
                    },
                },
            };
            if was_replayed {
                self.emit_tool_start(tc);
            }
            messages.push(self.finish_tool_call(tc, result, started, active_turn)?);
        }
        Ok(messages)
    }

    fn requires_tool_approval(&self) -> bool {
        self.hitl
            .as_ref()
            .is_some_and(AgentHitlRuntime::require_tool_approval)
    }

    async fn await_tool_approval(
        &self,
        tc: &simulacra_types::ToolCallMessage,
    ) -> Result<ToolApprovalDecision, RuntimeError> {
        let Some(hitl) = &self.hitl else {
            return Ok(ToolApprovalDecision::Approved);
        };
        if !hitl.require_tool_approval() || tc.name == REQUEST_INPUT_TOOL_NAME {
            return Ok(ToolApprovalDecision::Approved);
        }

        self.sink.emit(ActivityEvent::ToolApprovalRequired {
            tool_call_id: tc.id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
            reason: Some("tool execution requires approval".into()),
        });

        loop {
            if self.is_cancelled() {
                return Ok(ToolApprovalDecision::Denied("cancelled by user".into()));
            }
            let next_response = if let Some(cancellation) = self.cancellation.clone() {
                tokio::select! {
                    response = hitl.recv_approval() => response,
                    () = wait_for_cancellation(cancellation) => {
                        return Ok(ToolApprovalDecision::Denied("cancelled by user".into()));
                    }
                }
            } else {
                hitl.recv_approval().await
            };
            let Some(response) = next_response else {
                return Err(RuntimeError::Session(
                    "tool approval response channel closed".into(),
                ));
            };
            if response.tool_call_id != tc.id {
                tracing::warn!(
                    expected_tool_call_id = %tc.id,
                    received_tool_call_id = %response.tool_call_id,
                    "ignoring approval response for non-current tool call"
                );
                continue;
            }
            if response.approved {
                return Ok(ToolApprovalDecision::Approved);
            }
            return Ok(ToolApprovalDecision::Denied(
                response.reason.unwrap_or_else(|| "denied by user".into()),
            ));
        }
    }

    fn journal_tool_call(
        &mut self,
        tc: &simulacra_types::ToolCallMessage,
    ) -> Result<(), RuntimeError> {
        tracing::info!(
            "gen_ai.tool.message" = format!("tool_call: {}", tc.name),
            tool_name = tc.name.as_str(),
            tool_call_id = tc.id.as_str(),
        );

        let tool_call = JournalEntryKind::ToolCall {
            tool_call_id: Some(tc.id.clone()),

            tool_name: tc.name.clone(),
            arguments: safe_outer_tool_arguments(&tc.name, &tc.arguments),
        };
        self.journal_entry(tool_call.clone())?;
        self.consume_replay_entry(&tool_call)
    }

    fn emit_tool_start(&self, tc: &simulacra_types::ToolCallMessage) {
        self.sink.emit(ActivityEvent::ToolStart {
            tool_call_id: tc.id.clone(),
            name: tc.name.clone(),
            arguments: safe_outer_tool_arguments(&tc.name, &tc.arguments),
        });
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

        for line in result.content.lines() {
            self.sink.emit(ActivityEvent::ToolOutput {
                tool_call_id: tc.id.clone(),
                line: line.to_string(),
            });
        }

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
            provider_content: vec![],
        })
    }
}

fn safe_outer_tool_arguments(tool_name: &str, arguments: &serde_json::Value) -> serde_json::Value {
    match tool_name {
        "mcp_search" => serde_json::json!({
            "query_length": arguments
                .get("query")
                .and_then(serde_json::Value::as_str)
                .map(str::len)
                .unwrap_or(0),
        }),
        "mcp_call" => {
            let remote_arguments = arguments
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "server": arguments
                    .get("server")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
                "tool": arguments
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
                "argument_length": remote_arguments.to_string().len(),
            })
        }
        _ => arguments.clone(),
    }
}
