//! Side paths of the hardened fetch loop: the module fetcher inherits the
//! redirect authorization, and every hop journals in order.

use crate::http::fetch_http_inner;
use crate::http_fetch_guard_support::*;
use crate::module_fetcher::AgentCellModuleFetcher;
use crate::test_support::{CapturingJournal, ScriptedHttpClient};
use simulacra_quickjs::ModuleFetcher;
use simulacra_types::JournalStorage;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn module_fetcher_denies_redirect_off_granted_host_without_destination_request() {
    let client = Arc::new(
        ScriptedHttpClient::default()
            .with(
                "https://allowed.example/mod.js",
                redirect(
                    "https://allowed.example/mod.js",
                    "https://evil.example/mod.js",
                ),
            )
            .with(
                "https://evil.example/mod.js",
                ok("https://evil.example/mod.js", "export default 1;"),
            ),
    );
    let fetcher = AgentCellModuleFetcher {
        capability: capability(&["allowed.example"]),
        budget: budget(),
        journal: journal(),
        agent_id: agent_id(),
        http_client: client.clone(),
        stubs: HashMap::new(),
    };

    let err = fetcher
        .fetch("https://allowed.example/mod.js")
        .expect_err("module redirect to ungranted host must be denied");

    assert!(
        err.contains("capability denied"),
        "module fetch should surface capability denial, got {err}"
    );
    assert_request_sequence(&client, &["https://allowed.example/mod.js"]);
    assert_all_no_follow(&client);
}

/// records one HttpRequest entry per hop, in order.
#[test]
fn each_hop_is_journaled_in_order() {
    let journal: Arc<CapturingJournal> = Arc::new(CapturingJournal::default());
    let client = ScriptedHttpClient::default()
        .with(
            "https://a.example/start",
            redirect("https://a.example/start", "https://b.example/mid"),
        )
        .with("https://b.example/mid", ok("https://b.example/mid", "done"));
    let caps = capability(&["a.example", "b.example"]);

    fetch_http_inner(
        "https://a.example/start",
        "GET",
        &[],
        None,
        &caps,
        &budget(),
        &(journal.clone() as Arc<dyn JournalStorage>),
        &agent_id(),
        true,
        "fetch_http",
        &client,
        None,
    )
    .expect("the chain should resolve");

    assert_eq!(
        journal.http_entries(),
        vec![
            (
                "GET".to_string(),
                "https://a.example/start".to_string(),
                302
            ),
            ("GET".to_string(), "https://b.example/mid".to_string(), 200),
        ],
        "one journal entry per hop, in order"
    );
}
