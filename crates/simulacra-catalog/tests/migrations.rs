use simulacra_catalog::Catalog;
use tempfile::TempDir;

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get::<_, i32>(0),
    )
    .unwrap()
        == 1
}

fn index_exists(conn: &rusqlite::Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
        [index],
        |row| row.get::<_, i32>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn fresh_db_runs_migrations_and_creates_tables() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("catalog.db");

    let _catalog = Catalog::open(&path).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();

    for expected in [
        "tenants",
        "agents",
        "skills",
        "memory_pools",
        "agent_skills",
        "agent_capabilities",
        "seeds_applied",
        "schema_meta",
        "agent_files",
        "agent_file_bytes",
        "channels",
        "agent_channels",
    ] {
        assert!(table_exists(&conn, expected), "missing table: {expected}");
    }

    for expected in [
        "idx_agents_tenant",
        "idx_skills_tenant",
        "idx_memory_pools_tenant",
        "idx_agent_files_agent",
        "idx_channels_tenant",
        "idx_agent_channels_channel",
    ] {
        assert!(index_exists(&conn, expected), "missing index: {expected}");
    }
}

#[test]
fn reopen_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("catalog.db");

    // Open the catalog once and seed a tenant via raw SQL on conn_for_tests().
    let now = chrono::Utc::now().to_rfc3339();
    {
        let first = Catalog::open(&path).unwrap();
        let conn = first.conn_for_tests();
        let guard = conn.lock().unwrap();
        guard
            .execute(
                "INSERT INTO tenants (id, namespace, display_name, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["t-1", "acme", Option::<&str>::None, now, now],
            )
            .unwrap();
    }

    // Reopen the catalog from the same path.
    let _second = Catalog::open(&path).unwrap();

    // The seeded tenant must survive the reopen.
    let conn = rusqlite::Connection::open(path).unwrap();
    let surviving_namespace: String = conn
        .query_row(
            "SELECT namespace FROM tenants WHERE id = ?1",
            ["t-1"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(surviving_namespace, "acme");

    // Schema version must remain at the latest (3) — migrations are not re-applied.
    let version: i32 = conn
        .query_row("SELECT MAX(version) FROM schema_meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);

    // One row per applied migration; the second open must NOT add duplicates.
    let row_count: i32 = conn
        .query_row("SELECT COUNT(*) FROM schema_meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        row_count, 3,
        "schema_meta should have exactly three rows after reopen"
    );
}

#[test]
fn pragmas_set_per_connection() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("catalog.db");

    let catalog = Catalog::open(&path).unwrap();
    let conn = catalog.conn_for_tests();
    let guard = conn.lock().unwrap();

    let journal_mode: String = guard
        .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    let foreign_keys: i32 = guard
        .query_row("PRAGMA foreign_keys;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(foreign_keys, 1);

    let synchronous: i32 = guard
        .query_row("PRAGMA synchronous;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(synchronous, 1);
}
