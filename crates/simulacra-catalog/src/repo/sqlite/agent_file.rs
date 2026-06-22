//! S045 — SQLite-backed `AgentFileRepository` and `AgentFileStore`.
//!
//! Bytes live in `agent_file_bytes` separately from metadata so list/get
//! views (which only need metadata) don't pull blob payloads. The repo
//! delegates byte storage to an [`AgentFileStore`] trait so non-SQLite
//! backends (filesystem, S3) can swap in later without changing repo
//! semantics.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::{AgentFileId, AgentId, TenantId};
use crate::models::{AgentFile, NewAgentFile};
use crate::repo::AgentFileRepository;
use crate::repo::sqlite::{SharedConn, blocking, parse_ts};

/// Storage seam for agent file bytes. Repo metadata is in the catalog DB;
/// the byte body goes here. v1 default is [`SqliteBlobAgentFileStore`],
/// which writes to the same SQLite connection. Filesystem and S3 backends
/// can implement this trait without changing the repo.
#[async_trait]
pub trait AgentFileStore: Send + Sync {
    async fn put(&self, file_id: &AgentFileId, bytes: &[u8]) -> Result<(), CatalogError>;
    async fn get(&self, file_id: &AgentFileId) -> Result<Vec<u8>, CatalogError>;
    async fn delete(&self, file_id: &AgentFileId) -> Result<(), CatalogError>;
}

/// Default `AgentFileStore` impl: bytes go in the `agent_file_bytes`
/// table on the catalog DB.
pub struct SqliteBlobAgentFileStore {
    conn: SharedConn,
}

impl SqliteBlobAgentFileStore {
    pub fn new(conn: SharedConn) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl AgentFileStore for SqliteBlobAgentFileStore {
    async fn put(&self, file_id: &AgentFileId, bytes: &[u8]) -> Result<(), CatalogError> {
        let fid = file_id.0.clone();
        let owned = bytes.to_vec();
        blocking(self.conn.clone(), move |c| {
            c.execute(
                "INSERT OR REPLACE INTO agent_file_bytes (file_id, bytes) VALUES (?1, ?2)",
                params![&fid, &owned],
            )?;
            Ok(())
        })
        .await
    }

    async fn get(&self, file_id: &AgentFileId) -> Result<Vec<u8>, CatalogError> {
        let fid = file_id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                "SELECT bytes FROM agent_file_bytes WHERE file_id = ?1",
                params![&fid],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("agent_file_bytes file_id={fid}")))
        })
        .await
    }

    async fn delete(&self, file_id: &AgentFileId) -> Result<(), CatalogError> {
        let fid = file_id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.execute(
                "DELETE FROM agent_file_bytes WHERE file_id = ?1",
                params![&fid],
            )?;
            Ok(())
        })
        .await
    }
}

pub struct SqliteAgentFileRepository {
    conn: SharedConn,
    store: Arc<dyn AgentFileStore>,
}

impl SqliteAgentFileRepository {
    pub fn new(conn: SharedConn, store: Arc<dyn AgentFileStore>) -> Self {
        Self { conn, store }
    }
}

pub(crate) fn row_to_agent_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentFile> {
    Ok(AgentFile {
        id: AgentFileId(row.get(0)?),
        agent_id: AgentId(row.get(1)?),
        name: row.get(2)?,
        mime_type: row.get(3)?,
        size_bytes: row.get::<_, i64>(4)? as u64,
        created_at: parse_ts(row.get::<_, String>(5)?)?,
        updated_at: parse_ts(row.get::<_, String>(6)?)?,
    })
}

const SELECT_AGENT_FILE_COLUMNS: &str = "SELECT agent_files.id, agent_files.agent_id, agent_files.name, agent_files.mime_type, \
     agent_files.size_bytes, agent_files.created_at, agent_files.updated_at FROM agent_files";

/// Validate a file's flat name. Allows `[A-Za-z0-9 ._-]+`, max 255 bytes.
fn validate_name(name: &str) -> Result<(), CatalogError> {
    if name.is_empty() {
        return Err(CatalogError::Validation(
            "filename must not be empty".into(),
        ));
    }
    if name.len() > 255 {
        return Err(CatalogError::Validation(format!(
            "filename too long ({} bytes); max 255",
            name.len()
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'_' | b'-'))
    {
        return Err(CatalogError::Validation(format!(
            "filename contains disallowed characters: {name:?}"
        )));
    }
    Ok(())
}

#[async_trait]
impl AgentFileRepository for SqliteAgentFileRepository {
    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewAgentFile<'_>,
    ) -> Result<AgentFile, CatalogError> {
        validate_name(input.name)?;
        let tid = tenant_id.0.clone();
        let aid = input.agent_id.0.clone();
        let name = input.name.to_owned();
        let mime = input.mime_type.to_owned();
        let bytes = input.bytes.to_vec();
        let size = bytes.len() as i64;
        let id_str = Ulid::new().to_string();
        let id = AgentFileId(id_str.clone());
        let now = Utc::now().to_rfc3339();

        // 1) Validate parent agent + dup name + INSERT metadata in one tx.
        let metadata_outcome = {
            let id_str = id_str.clone();
            let name = name.clone();
            let mime = mime.clone();
            let now = now.clone();
            blocking(self.conn.clone(), move |c| {
                let owned_by_tenant: Option<String> = c
                    .query_row(
                        "SELECT id FROM agents WHERE id = ?1 AND tenant_id = ?2",
                        params![&aid, &tid],
                        |row| row.get(0),
                    )
                    .optional()?;
                if owned_by_tenant.is_none() {
                    return Err(CatalogError::NotFound(format!(
                        "agent id={aid} tenant={tid}"
                    )));
                }

                let existing: Option<String> = c
                    .query_row(
                        "SELECT id FROM agent_files WHERE agent_id = ?1 AND name = ?2",
                        params![&aid, &name],
                        |row| row.get(0),
                    )
                    .optional()?;
                if existing.is_some() {
                    return Err(CatalogError::Conflict(format!(
                        "agent_file name={name} agent_id={aid}"
                    )));
                }

                c.execute(
                    "INSERT INTO agent_files (id, agent_id, name, mime_type, size_bytes, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![&id_str, &aid, &name, &mime, size, &now],
                )?;
                Ok::<_, CatalogError>(())
            })
            .await
        };

        metadata_outcome?;

        // 2) Delegate byte storage to the AgentFileStore. If this fails,
        // roll back the metadata row so we don't leave orphan rows behind.
        if let Err(store_err) = self.store.put(&id, &bytes).await {
            let id_str_for_rollback = id_str.clone();
            let _ = blocking(self.conn.clone(), move |c| {
                c.execute(
                    "DELETE FROM agent_files WHERE id = ?1",
                    params![&id_str_for_rollback],
                )?;
                Ok::<_, CatalogError>(())
            })
            .await;
            return Err(store_err);
        }

        let parsed_now = parse_ts(now.clone())
            .map_err(|e| CatalogError::Validation(format!("created_at parse failed: {e}")))?;

        Ok(AgentFile {
            id,
            agent_id: AgentId(input.agent_id.0.clone()),
            name,
            mime_type: mime,
            size_bytes: size as u64,
            created_at: parsed_now,
            updated_at: parsed_now,
        })
    }

    async fn get(&self, tenant_id: &TenantId, id: &AgentFileId) -> Result<AgentFile, CatalogError> {
        let tid = tenant_id.0.clone();
        let fid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let row = c
                .query_row(
                    &format!(
                        "{SELECT_AGENT_FILE_COLUMNS} \
                         JOIN agents ON agents.id = agent_files.agent_id \
                         WHERE agent_files.id = ?1 AND agents.tenant_id = ?2"
                    ),
                    params![&fid, &tid],
                    row_to_agent_file,
                )
                .optional()?;
            row.ok_or_else(|| CatalogError::NotFound(format!("agent_file id={fid} tenant={tid}")))
        })
        .await
    }

    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<AgentFile>, CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = agent_id.0.clone();
        blocking(self.conn.clone(), move |c| {
            // Stricter than `agents.capabilities` (which returns empty on
            // missing): listing files for an agent the tenant doesn't own
            // returns NotFound, so callers don't silently observe a
            // foreign agent's empty file set.
            let visible: Option<String> = c
                .query_row(
                    "SELECT id FROM agents WHERE id = ?1 AND tenant_id = ?2",
                    params![&aid, &tid],
                    |row| row.get(0),
                )
                .optional()?;
            if visible.is_none() {
                return Err(CatalogError::NotFound(format!(
                    "agent id={aid} tenant={tid}"
                )));
            }

            let mut stmt = c.prepare(&format!(
                "{SELECT_AGENT_FILE_COLUMNS} WHERE agent_files.agent_id = ?1 \
                 ORDER BY agent_files.created_at ASC, agent_files.id ASC"
            ))?;
            let rows = stmt.query_map([&aid], row_to_agent_file)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    async fn read_bytes(
        &self,
        tenant_id: &TenantId,
        id: &AgentFileId,
    ) -> Result<Vec<u8>, CatalogError> {
        // Verify tenant scope first; bypassing this would let a foreign
        // tenant id read bytes via the AgentFileStore.
        let _meta = self.get(tenant_id, id).await?;
        self.store.get(id).await
    }

    async fn delete(&self, tenant_id: &TenantId, id: &AgentFileId) -> Result<(), CatalogError> {
        let tid = tenant_id.0.clone();
        let fid = id.0.clone();
        let owned: Option<String> = blocking(self.conn.clone(), {
            let fid = fid.clone();
            let tid = tid.clone();
            move |c| {
                let r: Option<String> = c
                    .query_row(
                        "SELECT agent_files.id \
                         FROM agent_files JOIN agents ON agents.id = agent_files.agent_id \
                         WHERE agent_files.id = ?1 AND agents.tenant_id = ?2",
                        params![&fid, &tid],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(r)
            }
        })
        .await?;
        if owned.is_none() {
            return Err(CatalogError::NotFound(format!(
                "agent_file id={fid} tenant={tid}"
            )));
        }

        // Delete metadata first; the FK on agent_file_bytes cascades for
        // the SQLite default store. For non-SQLite stores, the explicit
        // store.delete() call below is what cleans up the byte body.
        let fid_for_delete = fid.clone();
        blocking(self.conn.clone(), move |c| {
            c.execute(
                "DELETE FROM agent_files WHERE id = ?1",
                params![&fid_for_delete],
            )?;
            Ok::<_, CatalogError>(())
        })
        .await?;

        // Best-effort byte cleanup for non-SQLite stores. The default
        // SqliteBlobAgentFileStore is a no-op here (cascade already
        // fired); other stores see this call.
        let _ = self.store.delete(id).await;
        Ok(())
    }
}
