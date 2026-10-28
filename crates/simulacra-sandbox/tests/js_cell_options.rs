mod common;
#[allow(unused_imports)]
use common::*;

use std::sync::Arc;
use std::time::Duration;

use simulacra_quickjs::{BoxFuture, JsHost, JsRuntimeOptions};
use simulacra_sandbox::AgentCell;
use simulacra_types::{JournalStorage, VirtualFs};

struct Echo;

impl JsHost for Echo {
    fn names(&self) -> Vec<String> {
        vec!["echo".into()]
    }

    fn call(&self, _name: String, args_json: String) -> BoxFuture<'static, Result<String, String>> {
        Box::pin(async move { Ok(args_json) })
    }
}

fn cell() -> AgentCell {
    let vfs: Arc<dyn VirtualFs> = Arc::new(simulacra_vfs::MemoryFs::new());
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(
        vfs,
        capability(&[], &[], false, true),
        unlimited_budget(),
        journal,
        http,
    )
}

#[test]
fn cell_options_supply_the_host_and_timeout() {
    let mut cell = cell();
    let without = cell.execute_js("typeof tools").unwrap();
    assert_eq!(without.result.as_deref(), Some("undefined"));

    cell.set_js_options(JsRuntimeOptions {
        host: Some(Arc::new(Echo)),
        timeout: Some(Duration::from_millis(300)),
        ..Default::default()
    });
    let out = cell
        .execute_js("(async () => JSON.stringify(await tools.echo({a: 1})))()")
        .unwrap();
    assert_eq!(out.result.as_deref(), Some(r#"{"a":1}"#));

    let start = std::time::Instant::now();
    assert!(cell.execute_js("while (true) {}").is_err());
    assert!(start.elapsed() < Duration::from_secs(3));
}
