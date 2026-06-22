use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::{MemoryPoolId, TenantId};
use crate::models::{MemoryPool, MemoryPoolPatch, NewMemoryPool};
use crate::repo::MemoryPoolRepository;
use crate::repo::sqlite::{SharedConn, blocking, parse_ts};

pub struct SqliteMemoryPoolRepository {
    conn: SharedConn,
}

impl SqliteMemoryPoolRepository {
    pub fn new(conn: SharedConn) -> Self {
        Self { conn }
    }
}

pub(crate) fn row_to_pool(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryPool> {
    let config_json: String = row.get(4)?;
    let config: serde_json::Value = serde_json::from_str(&config_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(MemoryPool {
        id: MemoryPoolId(row.get(0)?),
        tenant_id: TenantId(row.get(1)?),
        name: row.get(2)?,
        embedding_model: row.get(3)?,
        config,
        created_at: parse_ts(row.get::<_, String>(5)?)?,
        updated_at: parse_ts(row.get::<_, String>(6)?)?,
    })
}

const SELECT_POOL_COLUMNS: &str =
    "SELECT id, tenant_id, name, embedding_model, config, created_at, updated_at FROM memory_pools";

#[async_trait]
impl MemoryPoolRepository for SqliteMemoryPoolRepository {
    async fn get(
        &self,
        tenant_id: &TenantId,
        id: &MemoryPoolId,
    ) -> Result<MemoryPool, CatalogError> {
        let tid = tenant_id.0.clone();
        let pid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_POOL_COLUMNS} WHERE tenant_id = ?1 AND id = ?2"),
                params![&tid, &pid],
                row_to_pool,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("memory_pool id={pid} tenant={tid}")))
        })
        .await
    }

    async fn get_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<MemoryPool, CatalogError> {
        let tid = tenant_id.0.clone();
        let name = name.to_owned();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_POOL_COLUMNS} WHERE tenant_id = ?1 AND name = ?2"),
                params![&tid, &name],
                row_to_pool,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("memory_pool name={name} tenant={tid}")))
        })
        .await
    }

    async fn list(&self, tenant_id: &TenantId) -> Result<Vec<MemoryPool>, CatalogError> {
        let tid = tenant_id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let mut stmt = c.prepare(&format!(
                "{SELECT_POOL_COLUMNS} WHERE tenant_id = ?1 ORDER BY created_at ASC, id ASC"
            ))?;
            let rows = stmt.query_map([&tid], row_to_pool)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewMemoryPool<'_>,
    ) -> Result<MemoryPool, CatalogError> {
        let tid = tenant_id.0.clone();
        let id = Ulid::new().to_string();
        let now = Utc::now().to_rfc3339();
        let name = input.name.to_owned();
        let embedding_model = input.embedding_model.map(str::to_owned);
        let config = serde_json::to_string(input.config)?;

        blocking(self.conn.clone(), move |c| {
            c.execute(
                "INSERT INTO memory_pools \
                 (id, tenant_id, name, embedding_model, config, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![&id, &tid, &name, &embedding_model, &config, &now],
            )
            // The memory_pools table's only enforced FK is tenant_id, which we
            // accept from a typed TenantId; pre-validation in callers ensures
            // the row exists. Combined with the catalog's single
            // Arc<Mutex<Connection>> serialization, the only constraint that
            // can fire here is the (tenant_id, name) UNIQUE index. If the
            // catalog later moves to a multi-connection pool, distinguish
            // SQLITE_CONSTRAINT_UNIQUE vs SQLITE_CONSTRAINT_FOREIGNKEY here.
            .map_err(|e| match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CatalogError::Conflict(format!(
                        "memory_pool name already exists: {name} (tenant={tid})"
                    ))
                }
                _ => CatalogError::Sqlite(e),
            })?;

            c.query_row(
                &format!("{SELECT_POOL_COLUMNS} WHERE id = ?1"),
                [&id],
                row_to_pool,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &MemoryPoolId,
        input: MemoryPoolPatch<'_>,
    ) -> Result<MemoryPool, CatalogError> {
        let tid = tenant_id.0.clone();
        let pid = id.0.clone();
        let now = Utc::now().to_rfc3339();
        let name = input.name.map(str::to_owned);
        let embedding_model = input.embedding_model.map(|inner| inner.map(str::to_owned));
        let config = match input.config {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };

        blocking(self.conn.clone(), move |c| {
            // Verify the row exists and belongs to the tenant before patching.
            let exists: Option<String> = c
                .query_row(
                    "SELECT id FROM memory_pools WHERE id = ?1 AND tenant_id = ?2",
                    params![&pid, &tid],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(CatalogError::NotFound(format!(
                    "memory_pool id={pid} tenant={tid}"
                )));
            }

            // Build a dynamic UPDATE statement based on which patch fields are set.
            let mut sets: Vec<&str> = Vec::new();
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            if let Some(n) = &name {
                sets.push("name = ?");
                values.push(rusqlite::types::Value::Text(n.clone()));
            }
            if let Some(em) = &embedding_model {
                sets.push("embedding_model = ?");
                values.push(match em {
                    Some(s) => rusqlite::types::Value::Text(s.clone()),
                    None => rusqlite::types::Value::Null,
                });
            }
            if let Some(cfg) = &config {
                sets.push("config = ?");
                values.push(rusqlite::types::Value::Text(cfg.clone()));
            }
            sets.push("updated_at = ?");
            values.push(rusqlite::types::Value::Text(now.clone()));

            let sql = format!(
                "UPDATE memory_pools SET {} WHERE id = ? AND tenant_id = ?",
                sets.join(", ")
            );
            values.push(rusqlite::types::Value::Text(pid.clone()));
            values.push(rusqlite::types::Value::Text(tid.clone()));

            let params_refs: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();

            c.execute(&sql, params_refs.as_slice())
                .map_err(|e| match &e {
                    rusqlite::Error::SqliteFailure(err, _)
                        if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        CatalogError::Conflict("memory_pool update conflicts on unique key".into())
                    }
                    _ => CatalogError::Sqlite(e),
                })?;

            c.query_row(
                &format!("{SELECT_POOL_COLUMNS} WHERE id = ?1"),
                [&pid],
                row_to_pool,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn delete(&self, tenant_id: &TenantId, id: &MemoryPoolId) -> Result<(), CatalogError> {
        let tid = tenant_id.0.clone();
        let pid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let n = c.execute(
                "DELETE FROM memory_pools WHERE id = ?1 AND tenant_id = ?2",
                params![&pid, &tid],
            )?;
            if n == 0 {
                return Err(CatalogError::NotFound(format!(
                    "memory_pool id={pid} tenant={tid}"
                )));
            }
            Ok(())
        })
        .await
    }
}
