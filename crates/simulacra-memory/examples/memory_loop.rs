// S037 Wave C demo: end-to-end memory loop without an LLM.
//
// Exercises the full subsystem stack:
//   1. SqliteMemoryStore + SqliteVectorIndex + DefaultEmbedder
//   2. BackgroundEmbedder consuming MemoryEvents from the store
//   3. Markdown chunker splitting policy docs into sections
//   4. semantic_search through the VectorIndex
//   5. Multi-tenant isolation
//   6. RRWB read-your-writes within a single "run"
//
// Run:
//   cargo run -p simulacra-memory --example memory_loop

use std::sync::Arc;
use std::time::Duration;

use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunker, ChunkerSelector, DefaultEmbedder,
    Embedder, MarkdownSectionChunker, MemoryStore, RecentWritesBuffer, SqliteMemoryStore,
    SqliteVectorIndex, VectorIndex,
};
use simulacra_types::{MemoryPath, TenantId};

#[tokio::main]
async fn main() {
    println!("\n╭──────────────────────────────────────────────╮");
    println!("│  S037 Wave C — Memory Loop Demo (no LLM)    │");
    println!("╰──────────────────────────────────────────────╯\n");

    // ── Setup ────────────────────────────────────────────────────────────
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    println!("[1/6] Setting up memory subsystem at {}", root.display());

    let embedder = Arc::new(DefaultEmbedder::load_default().expect("default embedder"));
    let memory_store: Arc<dyn MemoryStore> =
        Arc::new(SqliteMemoryStore::new(root).expect("memory store"));
    let vector_index: Arc<dyn VectorIndex> =
        Arc::new(SqliteVectorIndex::new(root, embedder.id().clone()).expect("vector index"));

    println!(
        "       Embedder:     {} (dim {})",
        embedder.id(),
        embedder.dim()
    );
    println!("       Store:        SqliteMemoryStore");
    println!("       Index:        SqliteVectorIndex (sqlite-vec)");

    // Markdown chunker for everything that ends in .md.
    let chunker_selector: ChunkerSelector = {
        let md = Arc::new(MarkdownSectionChunker) as Arc<dyn Chunker>;
        Arc::new(move |path| {
            if path.as_str().ends_with(".md") {
                Some(md.clone())
            } else {
                None
            }
        })
    };

    let _embedder_handle = BackgroundEmbedder::spawn(
        Arc::clone(&memory_store),
        Arc::clone(&vector_index),
        embedder.clone() as Arc<dyn Embedder>,
        chunker_selector,
        BackgroundEmbedderConfig::default(),
    )
    .expect("background embedder");
    println!("       Background embedder: spawned");

    // ── Tenant A: admin ingests HR policy docs ──────────────────────────
    let tenant_a = TenantId::parse("acme").unwrap();
    println!("\n[2/6] Tenant '{tenant_a}': admin ingests 4 HR policy docs into /mnt/hr/");

    let policy_docs: &[(&str, &str)] = &[
        (
            "/mnt/hr/pto.md",
            "# PTO Policy\n\nFull-time employees accrue 2.5 days of paid time off per month.\n\n## Carry-over\n\nUnused PTO carries over to the next year, capped at 30 days. Excess balances are forfeit on January 1.\n",
        ),
        (
            "/mnt/hr/remote-work.md",
            "# Remote Work Policy\n\nEmployees may work remotely up to 4 days per week with manager approval.\n\n## Equipment\n\nAcme provides a laptop, monitor, and ergonomic chair stipend for full-time remote workers.\n",
        ),
        (
            "/mnt/hr/expenses.md",
            "# Expense Reimbursement\n\nSubmit receipts within 30 days of purchase. Meals are reimbursed up to $50 per day during business travel.\n\n## Approval\n\nExpenses over $500 require manager pre-approval.\n",
        ),
        (
            "/mnt/hr/security.md",
            "# Security Policy\n\nAll employees must enable 2FA on company accounts. Personal devices accessing company data must use a managed MDM profile.\n\n## Incidents\n\nReport suspected security incidents to security@acme.com immediately.\n",
        ),
    ];

    for (path, content) in policy_docs {
        let mp = MemoryPath::parse(path).unwrap();
        let v = memory_store
            .put(&tenant_a, &mp, content.as_bytes())
            .expect("put policy doc");
        println!("       wrote {path:35} → version {v}");
    }

    // ── Tenant A: Atlas writes some self-notes ──────────────────────────
    println!(
        "\n[3/6] Tenant '{tenant_a}': agent 'Atlas' writes 2 self-notes into /var/memory/self/"
    );

    let agent_notes: &[(&str, &str)] = &[
        (
            "/var/memory/self/notes/q1-pipeline-bug.md",
            "# BigQuery Schema Bug — Q1 Pipeline\n\nThe deals table renamed `close_date` to `expected_close_date` on March 1. Queries that reference the old column name fail with `unrecognized column`. Use `expected_close_date` going forward.\n",
        ),
        (
            "/var/memory/self/failures/slack-403.md",
            "# Failed Slack Post — #announcements\n\nAttempted to post to #announcements at 09:14 — got 403 channel_is_archived. The channel was archived in late February. Use #all-hands instead.\n",
        ),
    ];

    for (path, content) in agent_notes {
        let mp = MemoryPath::parse(path).unwrap();
        let v = memory_store
            .put(&tenant_a, &mp, content.as_bytes())
            .expect("put agent note");
        println!("       wrote {path:50} → version {v}");
    }

    // ── Wait for the background embedder to catch up ────────────────────
    println!("\n[4/6] Waiting for background embedder to chunk + embed + upsert (Guarantee 3)...");
    let policy_scope = MemoryPath::parse("/mnt/hr").unwrap();
    let self_scope = MemoryPath::parse("/var/memory/self").unwrap();
    wait_for_indexed(
        &*vector_index,
        &tenant_a,
        &policy_scope,
        embedder.as_ref(),
        4,
    )
    .await;
    wait_for_indexed(&*vector_index, &tenant_a, &self_scope, embedder.as_ref(), 2).await;
    println!("       All 6 documents are searchable.");

    // ── Run semantic searches ───────────────────────────────────────────
    println!("\n[5/6] Running semantic_search queries via the VectorIndex...");

    let queries: &[(&str, &MemoryPath, &str)] = &[
        (
            "How many PTO days do I get?",
            &policy_scope,
            "RAG over admin-ingested HR policies",
        ),
        (
            "remote work equipment laptop",
            &policy_scope,
            "RAG retrieval matching keyword soup",
        ),
        (
            "BigQuery deals table column",
            &self_scope,
            "agent recalls its own past discoveries",
        ),
        (
            "channel archived slack 403",
            &self_scope,
            "agent recalls its own failure to avoid repeating it",
        ),
    ];

    for (q, scope, label) in queries {
        let qe = embedder.embed(&[q]).unwrap().into_iter().next().unwrap();
        let hits = vector_index
            .search(&tenant_a, scope, &qe, embedder.id(), 3, None)
            .unwrap();
        println!("\n       Query:   {q}");
        println!("       Scope:   {scope}");
        println!("       Label:   {label}");
        if hits.is_empty() {
            println!("       ▷ no hits");
        }
        for (i, hit) in hits.iter().enumerate() {
            let snippet = hit.snippet.replace('\n', " ");
            let snippet = if snippet.len() > 90 {
                format!("{}…", &snippet[..90])
            } else {
                snippet
            };
            println!(
                "       ▷ #{} {} (score {:.3})",
                i + 1,
                hit.path,
                hit.cosine_score
            );
            println!("           {snippet}");
        }
    }

    // ── Tenant isolation: Beta Inc cannot see Acme's content ────────────
    println!("\n[6/6] Tenant isolation check: 'beta' does NOT see 'acme' content");
    let tenant_b = TenantId::parse("beta").unwrap();
    let qe = embedder
        .embed(&["PTO policy"])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let cross_hits = vector_index
        .search(&tenant_b, &policy_scope, &qe, embedder.id(), 5, None)
        .unwrap();
    if cross_hits.is_empty() {
        println!("       ✓ tenant 'beta' search returned 0 hits (physical DB isolation)");
    } else {
        println!("       ✗ LEAK: tenant 'beta' got {} hits", cross_hits.len());
        std::process::exit(1);
    }

    // ── Bonus: RRWB read-your-writes ────────────────────────────────────
    println!("\n[bonus] RRWB read-your-writes within a single run (Guarantee 2)");
    let mut rrwb = RecentWritesBuffer::new();
    let just_written = MemoryPath::parse("/var/memory/self/scratch/just-now.md").unwrap();
    rrwb.record(
        just_written.clone(),
        simulacra_types::MemoryVersion(1),
        b"This note was just recorded into the RRWB and should be searchable in the same run.",
    );
    // RRWB MVP uses whole-query substring matching (case-insensitive),
    // not token-level. The query must be a contiguous substring of the
    // recorded text. The Wave D upgrade swaps this for embed-on-query
    // with real cosine similarity (see rrwb.rs module docs).
    let rrwb_hits = rrwb.search("just recorded into the rrwb", &self_scope);
    if rrwb_hits.is_empty() {
        println!("       ✗ RRWB search returned 0 hits");
        std::process::exit(1);
    }
    println!(
        "       ✓ RRWB returned {} hit(s) for the just-written note",
        rrwb_hits.len()
    );
    for hit in &rrwb_hits {
        let snippet = hit.snippet.replace('\n', " ");
        println!("         {} (score {:.3})", hit.path, hit.cosine_score);
        println!("           {snippet}");
    }

    println!("\n╭──────────────────────────────────────────────╮");
    println!("│  RESULT: Wave C memory loop verified         │");
    println!("╰──────────────────────────────────────────────╯");
    println!("  Store path:    {}", root.display());
    println!("  Tenant 'acme': 4 policy docs + 2 self-notes indexed");
    println!("  Tenant 'beta': 0 hits (isolation enforced)");
    println!("  RRWB:          1 in-run write recorded and searchable");
    println!();
}

/// Poll the index until at least `expected_chunks` chunks exist under the
/// given scope, or fail after a deadline.
async fn wait_for_indexed(
    index: &dyn VectorIndex,
    tenant: &TenantId,
    scope: &MemoryPath,
    embedder: &dyn Embedder,
    expected_chunks: usize,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let probe = embedder
        .embed(&["wait probe"])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    loop {
        let hits = index
            .search(tenant, scope, &probe, embedder.id(), 100, None)
            .unwrap_or_default();
        if hits.len() >= expected_chunks {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for {expected_chunks} chunks under {scope}; got {}",
                hits.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
