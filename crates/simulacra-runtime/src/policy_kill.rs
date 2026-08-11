use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};

use simulacra_types::AgentId;
use simulacra_types::JournalStorage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicyKillDecision {
    pub(crate) hook: String,
    pub(crate) reason: String,
}

/// First-wins live policy signal for one running parent agent.
///
/// This is intentionally independent of journal persistence: audit failure
/// cannot erase an already-decided policy kill.
pub(crate) struct PolicyKillSignal {
    decision: OnceLock<PolicyKillDecision>,
}

impl PolicyKillSignal {
    fn new() -> Self {
        Self {
            decision: OnceLock::new(),
        }
    }

    pub(crate) fn decision(&self) -> Option<PolicyKillDecision> {
        self.decision.get().cloned()
    }

    #[cfg(feature = "spawn")]
    fn signal(&self, decision: PolicyKillDecision) {
        let _ = self.decision.set(decision);
    }
}

type PolicyKillKey = (String, usize);

static POLICY_KILL_SIGNALS: LazyLock<Mutex<HashMap<PolicyKillKey, Weak<PolicyKillSignal>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_signals() -> std::sync::MutexGuard<'static, HashMap<PolicyKillKey, Weak<PolicyKillSignal>>>
{
    match POLICY_KILL_SIGNALS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("recovering poisoned live policy-kill registry");
            poisoned.into_inner()
        }
    }
}

fn key(parent_id: &AgentId, journal: &Arc<dyn JournalStorage>) -> PolicyKillKey {
    (
        parent_id.0.clone(),
        Arc::as_ptr(journal) as *const () as usize,
    )
}

pub(crate) fn subscribe(
    parent_id: &AgentId,
    journal: &Arc<dyn JournalStorage>,
) -> Arc<PolicyKillSignal> {
    let key = key(parent_id, journal);
    let mut signals = lock_signals();
    if let Some(signal) = signals.get(&key).and_then(Weak::upgrade) {
        return signal;
    }
    let signal = Arc::new(PolicyKillSignal::new());
    signals.insert(key, Arc::downgrade(&signal));
    signal
}

#[cfg(feature = "spawn")]
pub(crate) fn signal(
    parent_id: &AgentId,
    journal: &Arc<dyn JournalStorage>,
    hook: String,
    reason: String,
) {
    let live = lock_signals()
        .get(&key(parent_id, journal))
        .and_then(Weak::upgrade);
    if let Some(live) = live {
        live.signal(PolicyKillDecision { hook, reason });
    }
}
