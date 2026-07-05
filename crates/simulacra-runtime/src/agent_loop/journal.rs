use super::*;

impl AgentLoop {
    /// Append a journal entry with the injected clock.
    ///
    /// ARCHITECTURE.md "Journal Before Return" makes journal append part of
    /// the side-effect contract: every operation must have its entry written
    /// before the result is returned. A failed append means replay would
    /// diverge silently, so we propagate the error to the caller; the caller
    /// must `?` this and abort the turn if journaling is critical for the
    /// next step (LLM calls, tool executions, hook denials).
    pub(super) fn journal_entry(&self, kind: JournalEntryKind) -> Result<(), RuntimeError> {
        let timestamp_ms = self.clock.now_ms();
        let entry_kind_name = entry_kind_name(&kind);
        let mode = if self.has_replay_entry() {
            "replayed"
        } else {
            "live"
        };

        let _span = tracing::info_span!(
            "journal_append",
            "simulacra.operation.name" = "journal_append",
            "simulacra.journal.entry_kind" = entry_kind_name,
            "simulacra.journal.mode" = mode,
        )
        .entered();

        // S005: Emit counter event for journal entries by kind
        tracing::info!(
            simulacra.journal.entries = 1u64,
            simulacra.journal.entry_kind = entry_kind_name,
            "journal entry appended"
        );

        let entry = JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.config.agent_id.clone(),
            timestamp_ms,
            entry: kind,
        };

        if let Err(e) = self.journal.append(entry) {
            self.journal_write_failures.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                error = %e,
                entry_kind = entry_kind_name,
                "journal append failed — aborting turn to preserve replay determinism"
            );
            return Err(RuntimeError::JournalAppendFailed {
                entry_kind: entry_kind_name,
                source: e,
            });
        }
        Ok(())
    }

    /// Check if replay has an entry available before the frontier.
    pub(super) fn has_replay_entry(&self) -> bool {
        self.replay.as_ref().is_some_and(|r| !r.at_frontier())
    }

    /// Consume and discard the next replay entry after verifying it matches
    /// the entry we just journaled. A shifted journal must fail at the first
    /// divergence instead of silently advancing the replay cursor.
    pub(super) fn consume_replay_entry(
        &mut self,
        expected: &JournalEntryKind,
    ) -> Result<(), RuntimeError> {
        if let Some(ref mut replay) = self.replay
            && !replay.at_frontier()
        {
            let actual = replay.next_recorded().cloned().ok_or_else(|| {
                RuntimeError::Journal(simulacra_types::JournalError::Storage(
                    "replay frontier reached while consuming expected entry".into(),
                ))
            })?;
            if !replay_entries_match(expected, &actual) {
                return Err(RuntimeError::Journal(
                    simulacra_types::JournalError::Storage(format!(
                        "replay divergence: expected {} but found {}",
                        describe_replay_entry(expected),
                        describe_replay_entry(&actual)
                    )),
                ));
            }
        }
        Ok(())
    }

    /// Take (clone) the next replay entry kind.
    pub(super) fn take_replay_entry(&mut self) -> Result<JournalEntryKind, RuntimeError> {
        self.replay
            .as_mut()
            .and_then(|r| r.next_recorded().cloned())
            .ok_or_else(|| {
                RuntimeError::Journal(simulacra_types::JournalError::Storage(
                    "take_replay_entry called but no replay entry available".into(),
                ))
            })
    }

    /// Consume replay entries after a ToolCall until its final ToolResult appears.
    pub(super) fn take_replay_tool_result(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
    ) -> Result<Option<(String, bool)>, RuntimeError> {
        if !self.has_replay_entry() {
            return Ok(None);
        }

        let Some(replay) = self.replay.as_ref() else {
            return Ok(None);
        };
        let start = replay.position();
        let entries = replay.entries();
        let mut selected: Option<usize> = None;
        let allow_legacy_tool_result_match =
            entries.get(start.saturating_sub(1)).is_some_and(|entry| {
                matches!(
                    entry.entry,
                    JournalEntryKind::ToolCall {
                        tool_call_id: None,
                        ..
                    }
                )
            });

        for (offset, entry) in entries[start..].iter().enumerate() {
            match &entry.entry {
                JournalEntryKind::ToolResult {
                    tool_call_id: Some(recorded_id),

                    tool_name: recorded,
                    ..
                } if recorded_id == tool_call_id && recorded == tool_name => {
                    selected = Some(offset);
                    break;
                }
                // Backward compatibility for old journals that predate
                // tool_call_id. New nested sandbox results do not set ids, so
                // they no longer collide with the top-level ToolResult.
                JournalEntryKind::ToolResult {
                    tool_call_id: None,

                    tool_name: recorded,
                    ..
                } if allow_legacy_tool_result_match && recorded == tool_name => {
                    selected = Some(offset)
                }
                JournalEntryKind::ToolResult { .. }
                | JournalEntryKind::ShellCommand { .. }
                | JournalEntryKind::CodeExecution { .. }
                | JournalEntryKind::SubAgentSpawned { .. }
                | JournalEntryKind::SubAgentCompleted { .. }
                | JournalEntryKind::FileWrite { .. }
                | JournalEntryKind::FileDelete { .. }
                | JournalEntryKind::FileMove { .. }
                | JournalEntryKind::HttpRequest { .. }
                | JournalEntryKind::Checkpoint { .. }
                | JournalEntryKind::HookDenial { .. }
                | JournalEntryKind::HookKill { .. } => {}
                other => {
                    if let Some(selected) = selected {
                        return self.consume_replay_tool_result_at(selected);
                    }
                    return Err(RuntimeError::Journal(
                        simulacra_types::JournalError::Storage(format!(
                            "expected ToolResult for {tool_name} ({tool_call_id}) during replay, found {}",
                            describe_replay_entry(other)
                        )),
                    ));
                }
            }
        }

        if let Some(selected) = selected {
            return self.consume_replay_tool_result_at(selected);
        }

        Err(RuntimeError::Journal(
            simulacra_types::JournalError::Storage(format!(
                "expected ToolResult for {tool_name} ({tool_call_id}) during replay, reached replay frontier"
            )),
        ))
    }

    pub(super) fn consume_replay_tool_result_at(
        &mut self,
        offset: usize,
    ) -> Result<Option<(String, bool)>, RuntimeError> {
        let mut selected = None;
        for idx in 0..=offset {
            let kind = self
                .replay
                .as_mut()
                .and_then(|r| r.next_recorded().cloned())
                .ok_or_else(|| {
                    RuntimeError::Journal(simulacra_types::JournalError::Storage(
                        "replay frontier reached while consuming tool result".into(),
                    ))
                })?;
            if idx == offset {
                selected = Some(replay_tool_result(&kind)?);
            }
        }
        Ok(selected)
    }
}
