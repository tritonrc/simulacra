use rusqlite::Connection;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub(crate) fn open_sqlite(path: &Path, schema_sql: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    initialize_sqlite(conn, schema_sql)
}

pub(crate) fn open_in_memory_sqlite(schema_sql: &str) -> Result<Connection, String> {
    let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
    initialize_sqlite(conn, schema_sql)
}

fn initialize_sqlite(conn: Connection, schema_sql: &str) -> Result<Connection, String> {
    conn.execute_batch(schema_sql).map_err(|e| e.to_string())?;
    Ok(conn)
}

pub(crate) fn lock_mutex<'a, T>(mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, String> {
    mutex.lock().map_err(|e| format!("lock poisoned: {e}"))
}
