use rusqlite::Connection;

use crate::error::CatalogError;

const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (2, include_str!("../migrations/0002_agent_files.sql")),
    (3, include_str!("../migrations/0003_channels.sql")),
];

pub fn run(conn: &mut Connection) -> Result<(), CatalogError> {
    // Ensure the schema_meta tracking table exists before reading from it.
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_meta (version INTEGER PRIMARY KEY);")?;

    let current: i32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_meta",
        [],
        |row| row.get(0),
    )?;

    let tx = conn.transaction()?;
    for (version, sql) in MIGRATIONS {
        if *version > current {
            tx.execute_batch(sql)?;
            tx.execute("INSERT INTO schema_meta (version) VALUES (?1)", [version])?;
        }
    }
    tx.commit()?;
    Ok(())
}
