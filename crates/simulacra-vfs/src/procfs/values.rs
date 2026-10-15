//! Values of the computed `/proc` files (budget, capabilities, session, agent).

use std::sync::atomic::Ordering;

use super::ProcState;

pub(super) fn budget_value(state: &ProcState, name: &str) -> Option<String> {
    let b = state.budget.lock().unwrap();
    match name {
        "max_tokens" => Some(b.max_tokens.to_string()),
        "used_tokens" => Some(b.used_tokens.to_string()),
        "remaining_tokens" => {
            if b.max_tokens == 0 {
                Some("0".to_string())
            } else {
                Some(b.max_tokens.saturating_sub(b.used_tokens).to_string())
            }
        }
        "max_turns" => Some(b.max_turns.to_string()),
        "used_turns" => Some(b.used_turns.to_string()),
        "remaining_turns" => {
            if b.max_turns == 0 {
                Some("0".to_string())
            } else {
                Some(
                    (b.max_turns as u64)
                        .saturating_sub(b.used_turns as u64)
                        .to_string(),
                )
            }
        }
        "max_fuel" => Some(b.max_fuel.to_string()),
        "used_fuel" => Some(b.used_fuel.to_string()),
        "max_cost" => Some(format!("{:.2}", b.max_cost)),
        "used_cost" => Some(format!("{:.2}", b.used_cost)),
        _ => None,
    }
}

pub(super) fn capabilities_value(state: &ProcState, name: &str) -> Option<String> {
    let c = &state.capabilities;
    match name {
        "shell" => Some(if c.shell { "true" } else { "false" }.to_string()),
        "javascript" => Some(if c.javascript { "true" } else { "false" }.to_string()),
        "python" => Some(if c.python { "true" } else { "false" }.to_string()),
        "network" => Some(
            c.network
                .iter()
                .map(|n| n.0.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "mcp_tools" => Some(c.mcp_tools.join("\n")),
        "paths_read" => Some(
            c.paths_read
                .iter()
                .map(|p| p.0.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "paths_write" => Some(
            c.paths_write
                .iter()
                .map(|p| p.0.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

pub(super) fn session_value(state: &ProcState, name: &str) -> Option<String> {
    match name {
        "id" => Some(state.session_id.clone()),
        "uptime_ms" => Some(state.session_start.elapsed().as_millis().to_string()),
        "journal_entries" => Some(state.journal_entries.load(Ordering::Relaxed).to_string()),
        _ => None,
    }
}

pub(super) fn agent_value(state: &ProcState, name: &str) -> Option<String> {
    match name {
        "id" => Some(state.agent_id.clone()),
        "name" => Some(state.agent_name.clone()),
        "model" => Some(state.model.clone()),
        "turn" => Some(state.turn.load(Ordering::Relaxed).to_string()),
        "parent_id" => Some(state.parent_id.as_deref().unwrap_or("").to_string()),
        _ => None,
    }
}
