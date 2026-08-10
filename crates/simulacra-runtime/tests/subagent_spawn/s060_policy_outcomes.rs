// S060 A39/A40 policy-outcome REDs. These tests use the real parent AgentLoop,
// SpawnAgentTool, supervisor actor, AgentTaskFactory, hook pipeline, and ACP
// runtime boundary. Journal wrappers control only persistence/race timing.

struct S060SelectiveHookKillJournal {
    inner: Arc<InMemoryJournalStorage>,
    failed_hook_kills: AtomicUsize,
    attempted_hook_kills: Mutex<Vec<JournalEntry>>,
    attempted_notify: tokio::sync::Notify,
}

impl S060SelectiveHookKillJournal {
    fn new(inner: Arc<InMemoryJournalStorage>) -> Self {
        Self {
            inner,
            failed_hook_kills: AtomicUsize::new(0),
            attempted_hook_kills: Mutex::new(Vec::new()),
            attempted_notify: tokio::sync::Notify::new(),
        }
    }
}

impl JournalStorage for S060SelectiveHookKillJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), simulacra_types::JournalError> {
        if matches!(&entry.entry, JournalEntryKind::HookKill { .. }) {
            self.failed_hook_kills.fetch_add(1, Ordering::SeqCst);
            self.attempted_hook_kills
                .lock()
                .expect("attempted HookKill lock")
                .push(entry);
            self.attempted_notify.notify_waiters();
            return Err(simulacra_types::JournalError::Storage(
                "injected HookKill audit failure".into(),
            ));
        }
        self.inner.append(entry)
    }

    fn read_all(
        &self,
        agent_id: &AgentId,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.read_all(agent_id)
    }

    fn query_token_usage(
        &self,
        agent_id: &AgentId,
    ) -> Result<TokenUsage, simulacra_types::JournalError> {
        self.inner.query_token_usage(agent_id)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: simulacra_types::CheckpointData,
    ) -> Result<(), simulacra_types::JournalError> {
        self.inner.save_checkpoint(agent_id, after_entry, data)
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.fork_from(agent_id, checkpoint_idx)
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.read_from(agent_id, start_index)
    }
}

fn s060_assert_failed_hook_kill_attempt(
    journal: &S060SelectiveHookKillJournal,
    hook: &str,
    reason: &str,
) {
    let attempts = journal
        .attempted_hook_kills
        .lock()
        .expect("attempted HookKill lock");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].schema_version, 3);
    assert_eq!(
        attempts[0].agent_id,
        AgentId("parent-s060-policy-outcomes".into())
    );
    assert!(matches!(
        &attempts[0].entry,
        JournalEntryKind::HookKill {
            hook_name,
            operation,
            reason: attempted_reason,
        } if hook_name == hook && operation == "spawn" && attempted_reason == reason
    ));
}

struct S060FinalPollRaceJournal {
    inner: Arc<InMemoryJournalStorage>,
    child_release: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    hook_kill_appended: Mutex<bool>,
    hook_kill_cv: std::sync::Condvar,
    race_triggered: std::sync::atomic::AtomicBool,
}

impl S060FinalPollRaceJournal {
    fn new(
        inner: Arc<InMemoryJournalStorage>,
        child_release: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        Self {
            inner,
            child_release: Mutex::new(Some(child_release)),
            hook_kill_appended: Mutex::new(false),
            hook_kill_cv: std::sync::Condvar::new(),
            race_triggered: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl JournalStorage for S060FinalPollRaceJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), simulacra_types::JournalError> {
        let is_spawn_kill = matches!(
            &entry.entry,
            JournalEntryKind::HookKill { operation, .. } if operation == "spawn"
        );
        self.inner.append(entry)?;
        if is_spawn_kill {
            *self
                .hook_kill_appended
                .lock()
                .expect("hook-kill signal lock") = true;
            self.hook_kill_cv.notify_all();
        }
        Ok(())
    }

    fn read_all(
        &self,
        agent_id: &AgentId,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.read_all(agent_id)
    }

    fn query_token_usage(
        &self,
        agent_id: &AgentId,
    ) -> Result<TokenUsage, simulacra_types::JournalError> {
        self.inner.query_token_usage(agent_id)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: simulacra_types::CheckpointData,
    ) -> Result<(), simulacra_types::JournalError> {
        self.inner.save_checkpoint(agent_id, after_entry, data)
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.fork_from(agent_id, checkpoint_idx)
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        // Snapshot first. If this is the final no-tool completion poll, release
        // the child only after the snapshot and wait until its after-hook kill
        // is durable. Returning the pre-kill snapshot places the kill exactly
        // after the parent's existing final poll.
        let snapshot = self.inner.read_from(agent_id, start_index)?;
        let is_final_no_tool_poll = snapshot.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::LlmResponse {
                    finish_reason,
                    assistant_message: Some(message),
                    ..
                } if finish_reason == "EndTurn" && message.tool_calls.is_empty()
            )
        });
        if is_final_no_tool_poll && !self.race_triggered.swap(true, Ordering::SeqCst) {
            self.child_release
                .lock()
                .expect("child release lock")
                .take()
                .expect("final poll should release child exactly once")
                .send(())
                .expect("child should still await final-poll release");
            let appended = self
                .hook_kill_appended
                .lock()
                .expect("hook-kill signal lock");
            let (appended, timeout) = self
                .hook_kill_cv
                .wait_timeout_while(appended, Duration::from_secs(2), |seen| !*seen)
                .expect("hook-kill wait lock");
            assert!(
                !timeout.timed_out(),
                "after-hook kill should arrive at final poll"
            );
            assert!(
                *appended,
                "after-hook kill should be durable before poll returns"
            );
        }
        Ok(snapshot)
    }
}

struct S060ReleasedAcpRuntime {
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl simulacra_runtime::AcpChildRuntime for S060ReleasedAcpRuntime {
    fn start_child(
        &self,
        _request: simulacra_runtime::AcpChildRequest,
        _cancellation: CancellationToken,
        _activity_sink: Arc<dyn simulacra_runtime::ActivitySink>,
        _input_queue: simulacra_runtime::AgentInputQueue,
    ) -> simulacra_runtime::AcpChildFuture {
        let release = self
            .release
            .lock()
            .expect("ACP release lock")
            .take()
            .expect("ACP runtime should start once");
        Box::pin(async move {
            release
                .await
                .map_err(|_| RuntimeError::Session("final-poll release sender dropped".into()))?;
            Ok(child_success_output())
        })
    }
}

struct S060PolicyOutcomeStack {
    tool: SpawnAgentTool,
    join: JoinChildAgentTool,
    capability: CapabilityToken,
    budget: Arc<Mutex<ResourceBudget>>,
    activity: Arc<Mutex<Vec<simulacra_types::ActivityEvent>>>,
    actor: tokio::task::JoinHandle<()>,
    sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

fn s060_policy_outcome_stack(
    pipeline: simulacra_hooks::HookPipeline,
    journal: Arc<dyn JournalStorage>,
    runtime: Arc<dyn simulacra_runtime::AcpChildRuntime>,
) -> S060PolicyOutcomeStack {
    let mut capability = s060_capability(&["workspace"]);
    capability.shell = true;
    let parent_id = AgentId("parent-s060-policy-outcomes".into());
    let activity = Arc::new(Mutex::new(Vec::new()));
    let activity_sink: Arc<dyn simulacra_runtime::ActivitySink> =
        Arc::new(S060LifecycleSink(Arc::clone(&activity)));
    let factory = Arc::new(AgentTaskFactory {
        config: s060_hook_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal: Arc::clone(&journal),
        activity_sink: Arc::clone(&activity_sink),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: Some(Arc::new(pipeline)),
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
        acp_child_runtime: Some(runtime),
    });
    let budget = Arc::new(Mutex::new(ResourceBudget::new(100, 10, Decimal::ZERO, 4)));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        factory,
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal));
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    S060PolicyOutcomeStack {
        tool: SpawnAgentTool {
            sender: sender.clone(),
            allowed_placements: vec!["workspace".into()],
            activity_sink,
            parent_id: parent_id.clone(),
            parent_budget: Arc::clone(&budget),
            guidance: None,
        },
        join: JoinChildAgentTool {
            sender: sender.clone(),
            caller_id: parent_id,
        },
        capability,
        budget,
        activity,
        actor,
        sender,
    }
}

struct S060ExactTwoTurnProvider {
    calls: Arc<AtomicUsize>,
}

struct S060ControlledFinalProvider {
    calls: Arc<AtomicUsize>,
    entered_final: Arc<tokio::sync::Barrier>,
    release_final: Arc<tokio::sync::Barrier>,
}

impl Provider for S060ControlledFinalProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let entered_final = Arc::clone(&self.entered_final);
        let release_final = Arc::clone(&self.release_final);
        Box::pin(async move {
            match call {
                0 => Ok(ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: String::new(),
                        tool_calls: vec![ToolCallMessage {
                            id: "s060-policy-controlled-spawn".into(),
                            name: "spawn_agent".into(),
                            arguments: s060_hook_arguments(),
                        }],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    token_usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    finish_reason: FinishReason::ToolUse,
                    provider_response_id: Some("s060-policy-controlled-tool".into()),
                    model: "parent-model".into(),
                }),
                1 => {
                    entered_final.wait().await;
                    release_final.wait().await;
                    Ok(ProviderResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: "controlled final response".into(),
                            tool_calls: vec![],
                            tool_call_id: None,
                            provider_content: vec![],
                        },
                        token_usage: TokenUsage {
                            input_tokens: 7,
                            output_tokens: 3,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                        },
                        finish_reason: FinishReason::EndTurn,
                        provider_response_id: Some("s060-policy-controlled-final".into()),
                        model: "parent-model".into(),
                    })
                }
                _ => panic!("no provider call may begin after the policy kill"),
            }
        })
    }
}

impl Provider for S060ExactTwoTurnProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match call {
                0 => Ok(ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: String::new(),
                        tool_calls: vec![ToolCallMessage {
                            id: "s060-policy-spawn".into(),
                            name: "spawn_agent".into(),
                            arguments: s060_hook_arguments(),
                        }],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    token_usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    finish_reason: FinishReason::ToolUse,
                    provider_response_id: Some("s060-policy-tool".into()),
                    model: "parent-model".into(),
                }),
                1 => Ok(ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: "parent attempted normal completion".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    token_usage: TokenUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    finish_reason: FinishReason::EndTurn,
                    provider_response_id: Some("s060-policy-final".into()),
                    model: "parent-model".into(),
                }),
                _ => panic!("policy parent must never cross another provider boundary"),
            }
        })
    }
}

fn s060_policy_parent_loop(
    tool: SpawnAgentTool,
    journal: Arc<dyn JournalStorage>,
    calls: Arc<AtomicUsize>,
) -> AgentLoop {
    s060_policy_parent_loop_with_provider(
        tool,
        journal,
        Box::new(S060ExactTwoTurnProvider { calls }),
    )
}

fn s060_policy_parent_loop_with_provider(
    tool: SpawnAgentTool,
    journal: Arc<dyn JournalStorage>,
    provider: Box<dyn Provider>,
) -> AgentLoop {
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(tool))
        .expect("spawn tool should register");
    AgentLoop::with_clock_and_replay(
        AgentLoopConfig {
            agent_id: AgentId("parent-s060-policy-outcomes".into()),
            system_prompt: "You are the spawning parent.".into(),
            model: "parent-model".into(),
            max_turns: 4,
            capability: s060_capability(&["workspace"]),
        },
        provider,
        tools,
        Box::new(PassthroughContext),
        journal,
        ResourceBudget::new(100, 4, Decimal::ZERO, 1),
        Box::new(simulacra_types::SystemClock),
        None,
    )
}

fn s060_after_kill_pipeline(name: &str, reason: &str) -> simulacra_hooks::HookPipeline {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    let name = name.to_string();
    let reason = reason.to_string();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name,
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(move |phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::continue_unchanged())
                }
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::Kill(reason.clone())),
            }),
        }),
    );
    pipeline
}

fn s060_before_kill_pipeline(name: &str, reason: &str) -> simulacra_hooks::HookPipeline {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    let name = name.to_string();
    let reason = reason.to_string();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name,
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(move |phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::Kill(reason.clone()))
                }
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
            }),
        }),
    );
    pipeline
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s060_a39_awaiting_approval_remains_running_without_terminal_effects() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "awaiting-approval-observer".into(),
            calls: Arc::clone(&calls),
            behavior: Arc::new(|_, _| Ok(simulacra_hooks::Verdict::continue_unchanged())),
        }),
    );
    let journal = Arc::new(InMemoryJournalStorage::new());
    let journal_port: Arc<dyn JournalStorage> = journal.clone();
    let runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> = Arc::new(S060RecordingAcpRuntime {
        observations: Arc::new(Mutex::new(Vec::new())),
        journal: Arc::clone(&journal_port),
        parent_id: AgentId("parent-s060-policy-outcomes".into()),
        outcome: S060ChildOutcome::AwaitingApproval,
    });
    let stack = s060_policy_outcome_stack(pipeline, journal_port, runtime);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("awaiting-approval child should first be accepted");
    let child_id = acknowledgement["child_id"]
        .as_str()
        .expect("accepted child id")
        .to_string();

    let wait = WaitChildAgentTool {
        sender: stack.sender.clone(),
        caller_id: AgentId("parent-s060-policy-outcomes".into()),
    }
    .call(
        serde_json::json!({"child_id": child_id, "timeout_ms": 25}),
        &stack.capability,
    )
    .await
    .expect("bounded wait should return ordinary running timeout");
    let status = ChildStatusTool {
        sender: stack.sender.clone(),
        caller_id: AgentId("parent-s060-policy-outcomes".into()),
    }
    .call(serde_json::json!({"child_id": child_id}), &stack.capability)
    .await
    .expect("awaiting child status should remain queryable");
    let roster = ListChildAgentTool {
        sender: stack.sender.clone(),
        caller_id: AgentId("parent-s060-policy-outcomes".into()),
    }
    .call(serde_json::json!({}), &stack.capability)
    .await
    .expect("awaiting child roster should remain queryable");
    let join_result = tokio::time::timeout(
        Duration::from_millis(25),
        stack
            .join
            .call(serde_json::json!({"child_id": child_id}), &stack.capability),
    )
    .await;
    let entries = journal
        .read_all(&AgentId("parent-s060-policy-outcomes".into()))
        .expect("parent journal");
    let activity = stack.activity.lock().expect("child activity").clone();
    let hook_calls = calls.lock().expect("hook calls").clone();

    assert_eq!(wait["status"], "running");
    assert_eq!(wait["ready"], false);
    assert_eq!(status["status"], "running");
    assert_eq!(status["ready"], false);
    let roster = roster.as_array().expect("roster array");
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["status"], "running");
    assert_eq!(roster[0]["ready"], false);
    assert!(
        join_result.is_err(),
        "join must remain pending while the same child awaits approval"
    );
    assert!(
        hook_calls
            .iter()
            .all(|(_, phase, _)| *phase != simulacra_hooks::Phase::After),
        "AwaitingApproval must not run spawn after-hooks"
    );
    assert!(
        activity
            .iter()
            .all(|event| !matches!(event, simulacra_types::ActivityEvent::ChildFinished { .. }))
    );
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
    );

    // Teardown is a real targeted cancellation of the same accepted child.
    // Keep it after every AwaitingApproval nonterminal assertion so cleanup
    // cannot make those assertions pass by manufacturing a terminal result.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    stack
        .sender
        .send(SupervisorMessage {
            priority: MessagePriority::Signal,
            agent_id: AgentId("parent-s060-policy-outcomes".into()),
            payload: SupervisorPayload::CancelChild(AgentId(child_id), cancel_tx),
        })
        .await
        .expect("targeted teardown cancellation should send");
    cancel_rx
        .await
        .expect("targeted teardown cancellation response")
        .expect("the exact awaiting child should accept cancellation");

    drop(stack.tool);
    drop(stack.join);
    drop(stack.sender);
    stack.actor.await.expect("supervisor actor should stop");
}

#[test]
fn s060_a40_real_factory_prepare_preserves_typed_before_kill_when_audit_fails() {
    let inner = Arc::new(InMemoryJournalStorage::new());
    let failing = Arc::new(S060SelectiveHookKillJournal::new(inner));
    let journal: Arc<dyn JournalStorage> = failing.clone();
    let mut parent_capability = s060_capability(&["workspace"]);
    parent_capability.shell = true;
    let factory = AgentTaskFactory {
        config: s060_hook_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: Some(Arc::new(s060_before_kill_pipeline(
            "direct-factory-before-kill",
            "factory kill stays typed",
        ))),
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
        acp_child_runtime: None,
    };
    let mut config = SpawnConfig {
        agent_id: AgentId("child-00000000000000000000000000000001".into()),
        parent_id: AgentId("parent-s060-policy-outcomes".into()),
        capability: None,
        budget: ResourceBudget::new(10, 2, Decimal::ZERO, 1),
        restart_strategy: RestartStrategy::LetCrash,
        placement: "workspace".into(),
        task: "bounded factory preparation".into(),
        instructions: Some("preserve policy provenance".into()),
    };

    let error = factory
        .prepare_spawn_config_for_caller(&mut config, &parent_capability)
        .expect_err("before-hook kill must remain typed despite audit failure");

    assert!(matches!(
        error,
        RuntimeError::HookKill { hook, reason }
            if hook == "direct-factory-before-kill" && reason == "factory kill stays typed"
    ));
    assert_eq!(failing.failed_hook_kills.load(Ordering::SeqCst), 1);
    s060_assert_failed_hook_kill_attempt(
        &failing,
        "direct-factory-before-kill",
        "factory kill stays typed",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s060_a40_after_hook_kill_after_final_poll_atomically_prevents_complete() {
    let inner = Arc::new(InMemoryJournalStorage::new());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let race_journal = Arc::new(S060FinalPollRaceJournal::new(
        Arc::clone(&inner),
        release_tx,
    ));
    let journal: Arc<dyn JournalStorage> = race_journal.clone();
    let runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> = Arc::new(S060ReleasedAcpRuntime {
        release: Mutex::new(Some(release_rx)),
    });
    let stack = s060_policy_outcome_stack(
        s060_after_kill_pipeline("final-poll-kill", "kill wins over Complete"),
        Arc::clone(&journal),
        runtime,
    );
    let S060PolicyOutcomeStack {
        tool,
        join,
        capability,
        budget: _,
        activity: _,
        actor,
        sender,
    } = stack;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut parent = s060_policy_parent_loop(tool, journal, Arc::clone(&provider_calls));
    let output = parent
        .run("delegate and then finish")
        .await
        .expect("policy kill should remain a typed terminal output");
    let entries = inner
        .read_all(&AgentId("parent-s060-policy-outcomes".into()))
        .expect("parent journal");
    let child_id = entries
        .iter()
        .find_map(|entry| match &entry.entry {
            JournalEntryKind::SubAgentSpawned { child_id, .. } => Some(child_id.0.clone()),
            _ => None,
        })
        .expect("accepted child should be journaled");
    let terminal = join
        .call(serde_json::json!({"child_id": child_id}), &capability)
        .await
        .expect("original child terminal should remain joinable");
    let entries = inner
        .read_all(&AgentId("parent-s060-policy-outcomes".into()))
        .expect("parent journal after join");
    drop(parent);
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");

    assert!(race_journal.race_triggered.load(Ordering::SeqCst));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        output.exit_reason,
        ExitReason::PolicyKill {
            hook: "final-poll-kill".into(),
            reason: "kill wins over Complete".into(),
        }
    );
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["message"], "child summary");
    assert!(entries.iter().any(|entry| matches!(
        entry.entry,
        JournalEntryKind::SubAgentCompleted { success: true, .. }
    )));
    assert!(entries.iter().any(|entry| matches!(
        entry.entry,
        JournalEntryKind::HookKill { ref hook_name, .. } if hook_name == "final-poll-kill"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s060_a40_after_hook_kill_enforcement_survives_hookkill_audit_failure() {
    let inner = Arc::new(InMemoryJournalStorage::new());
    let failing = Arc::new(S060SelectiveHookKillJournal::new(Arc::clone(&inner)));
    let journal: Arc<dyn JournalStorage> = failing.clone();
    let child_release = Arc::new(tokio::sync::Barrier::new(2));
    let runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> = Arc::new(S060RecordingAcpRuntime {
        observations: Arc::new(Mutex::new(Vec::new())),
        journal: Arc::clone(&journal),
        parent_id: AgentId("parent-s060-policy-outcomes".into()),
        outcome: S060ChildOutcome::CompleteAfter(Arc::clone(&child_release)),
    });
    let stack = s060_policy_outcome_stack(
        s060_after_kill_pipeline("auditless-after-kill", "enforce without audit"),
        Arc::clone(&journal),
        runtime,
    );
    let S060PolicyOutcomeStack {
        tool,
        join,
        capability,
        budget: _,
        activity: _,
        actor,
        sender,
    } = stack;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let parent_entered = Arc::new(tokio::sync::Barrier::new(2));
    let parent_release = Arc::new(tokio::sync::Barrier::new(2));
    let provider: Box<dyn Provider> = Box::new(S060ControlledFinalProvider {
        calls: Arc::clone(&provider_calls),
        entered_final: Arc::clone(&parent_entered),
        release_final: Arc::clone(&parent_release),
    });
    let mut parent = s060_policy_parent_loop_with_provider(tool, journal, provider);
    let parent_task = tokio::spawn(async move {
        parent
            .run("delegate despite failing audit storage")
            .await
            .expect("audit failure must not erase typed policy enforcement")
    });

    tokio::time::timeout(Duration::from_secs(2), parent_entered.wait())
        .await
        .expect("parent should be inside its known final provider call");
    let audit_attempted = failing.attempted_notify.notified();
    child_release.wait().await;
    tokio::time::timeout(Duration::from_secs(2), audit_attempted)
        .await
        .expect("after-hook kill decision should attempt its audit append");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert!(
        !parent_task.is_finished(),
        "parent remains in the already-active provider call until released"
    );
    parent_release.wait().await;
    let output = tokio::time::timeout(Duration::from_secs(2), parent_task)
        .await
        .expect("parent should terminate after its in-flight provider returns")
        .expect("parent task should join");
    let entries = inner
        .read_all(&AgentId("parent-s060-policy-outcomes".into()))
        .expect("surviving parent journal entries");
    let child_id = entries
        .iter()
        .find_map(|entry| match &entry.entry {
            JournalEntryKind::SubAgentSpawned { child_id, .. } => Some(child_id.0.clone()),
            _ => None,
        })
        .expect("accepted child id");
    let terminal = join
        .call(serde_json::json!({"child_id": child_id}), &capability)
        .await
        .expect("original child result should survive audit failure");
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");

    assert_eq!(failing.failed_hook_kills.load(Ordering::SeqCst), 1);
    s060_assert_failed_hook_kill_attempt(&failing, "auditless-after-kill", "enforce without audit");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        output.exit_reason,
        ExitReason::PolicyKill {
            hook: "auditless-after-kill".into(),
            reason: "enforce without audit".into(),
        }
    );
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["exit_reason"], "completed");
    assert_eq!(terminal["message"], "child summary");
    assert_eq!(terminal["token_usage"]["input_tokens"], 3);
    assert_eq!(terminal["token_usage"]["output_tokens"], 2);
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry.entry, JournalEntryKind::HookKill { .. }))
    );
    assert!(entries.iter().any(|entry| matches!(
        entry.entry,
        JournalEntryKind::SubAgentCompleted { success: true, .. }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s060_a40_direct_before_kill_is_fail_closed_even_when_audit_append_fails() {
    let inner = Arc::new(InMemoryJournalStorage::new());
    let failing = Arc::new(S060SelectiveHookKillJournal::new(Arc::clone(&inner)));
    let journal: Arc<dyn JournalStorage> = failing.clone();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> = Arc::new(S060RecordingAcpRuntime {
        observations: Arc::clone(&observations),
        journal: Arc::clone(&journal),
        parent_id: AgentId("parent-s060-policy-outcomes".into()),
        outcome: S060ChildOutcome::Complete,
    });
    let stack = s060_policy_outcome_stack(
        s060_before_kill_pipeline("auditless-before-kill", "direct kill must fail closed"),
        Arc::clone(&journal),
        runtime,
    );
    let S060PolicyOutcomeStack {
        tool,
        join,
        capability: _,
        budget,
        activity,
        actor,
        sender,
    } = stack;
    let budget_before = serde_json::to_value(
        budget
            .lock()
            .expect("shared budget before before-hook kill")
            .clone(),
    )
    .expect("budget should serialize");
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut parent = s060_policy_parent_loop(tool, journal, Arc::clone(&provider_calls));
    let output = parent
        .run("attempt a directly-killed spawn")
        .await
        .expect("direct kill should not panic or escape typed policy termination");
    let entries = inner
        .read_all(&AgentId("parent-s060-policy-outcomes".into()))
        .expect("surviving parent journal entries");
    drop(parent);
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");

    assert_eq!(failing.failed_hook_kills.load(Ordering::SeqCst), 1);
    s060_assert_failed_hook_kill_attempt(
        &failing,
        "auditless-before-kill",
        "direct kill must fail closed",
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "a synchronous before-hook kill must prevent another provider call"
    );
    assert_eq!(
        output.exit_reason,
        ExitReason::PolicyKill {
            hook: "auditless-before-kill".into(),
            reason: "direct kill must fail closed".into(),
        }
    );
    assert!(
        observations
            .lock()
            .expect("runtime observations")
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(
            budget
                .lock()
                .expect("shared budget after before-hook kill")
                .clone()
        )
        .expect("budget should serialize"),
        budget_before,
        "before-hook kill must leave shared budget unchanged"
    );
    assert!(
        activity.lock().expect("child activity").is_empty(),
        "before-hook kill must emit no accepted-child activity"
    );
    assert!(entries.iter().all(|entry| !matches!(
        entry.entry,
        JournalEntryKind::HookKill { .. }
            | JournalEntryKind::SubAgentSpawned { .. }
            | JournalEntryKind::SubAgentCompleted { .. }
    )));
}
