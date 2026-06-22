//! S046 — SQLite-backed `ChannelRepository`.

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::{AgentId, ChannelId, TenantId};
use crate::models::{Channel, ChannelKind, ChannelPatch, NewChannel, Page, PageRequest};
use crate::repo::ChannelRepository;
use crate::repo::sqlite::{SharedConn, blocking, decode_cursor, encode_cursor, parse_ts};

pub struct SqliteChannelRepository {
    conn: SharedConn,
}

impl SqliteChannelRepository {
    pub fn new(conn: SharedConn) -> Self {
        Self { conn }
    }
}

pub(crate) fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    let kind_str: String = row.get(3)?;
    let kind = ChannelKind::parse(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown channel kind: {kind_str}"),
            )),
        )
    })?;
    let config_str: String = row.get(4)?;
    let config = serde_json::from_str(&config_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Channel {
        id: ChannelId(row.get(0)?),
        tenant_id: TenantId(row.get(1)?),
        name: row.get(2)?,
        kind,
        config,
        created_at: parse_ts(row.get::<_, String>(5)?)?,
        updated_at: parse_ts(row.get::<_, String>(6)?)?,
    })
}

const SELECT_CHANNEL_COLUMNS: &str =
    "SELECT id, tenant_id, name, kind, config, created_at, updated_at FROM channels";

#[async_trait]
impl ChannelRepository for SqliteChannelRepository {
    async fn get(&self, tenant_id: &TenantId, id: &ChannelId) -> Result<Channel, CatalogError> {
        let tid = tenant_id.0.clone();
        let cid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_CHANNEL_COLUMNS} WHERE tenant_id = ?1 AND id = ?2"),
                params![&tid, &cid],
                row_to_channel,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("channel id={cid} tenant={tid}")))
        })
        .await
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Channel>, CatalogError> {
        let tid = tenant_id.0.clone();
        let limit = page.first.unwrap_or(20).max(1) as i64;
        let after_decoded = match &page.after {
            Some(c) => Some(decode_cursor(c)?),
            None => None,
        };
        let name_like = name_contains.map(|n| format!("%{n}%"));

        blocking(self.conn.clone(), move |c| {
            let fetch_limit = limit + 1;
            let mut items: Vec<Channel> = match (after_decoded, name_like.as_deref()) {
                (Some((created_at, id)), Some(needle)) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_CHANNEL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND name LIKE ?2 \
                           AND (created_at > ?3 OR (created_at = ?3 AND id > ?4)) \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?5"
                    ))?;
                    let rows = stmt.query_map(
                        params![&tid, needle, &created_at, &id, fetch_limit],
                        row_to_channel,
                    )?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (Some((created_at, id)), None) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_CHANNEL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND (created_at > ?2 OR (created_at = ?2 AND id > ?3)) \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?4"
                    ))?;
                    let rows = stmt
                        .query_map(params![&tid, &created_at, &id, fetch_limit], row_to_channel)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (None, Some(needle)) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_CHANNEL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND name LIKE ?2 \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?3"
                    ))?;
                    let rows =
                        stmt.query_map(params![&tid, needle, fetch_limit], row_to_channel)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (None, None) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_CHANNEL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?2"
                    ))?;
                    let rows = stmt.query_map(params![&tid, fetch_limit], row_to_channel)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
            };

            let has_next_page = items.len() as i64 > limit;
            if has_next_page {
                items.truncate(limit as usize);
            }
            let start_cursor = items
                .first()
                .map(|s| encode_cursor(&s.created_at.to_rfc3339(), s.id.as_str()));
            let end_cursor = items
                .last()
                .map(|s| encode_cursor(&s.created_at.to_rfc3339(), s.id.as_str()));

            Ok(Page {
                items,
                end_cursor,
                start_cursor,
                has_next_page,
                has_previous_page: false,
            })
        })
        .await
    }

    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<Channel>, CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = agent_id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let mut stmt = c.prepare(
                "SELECT c.id, c.tenant_id, c.name, c.kind, c.config, c.created_at, c.updated_at \
                 FROM channels c \
                 JOIN agent_channels ac ON ac.channel_id = c.id \
                 WHERE ac.agent_id = ?1 AND c.tenant_id = ?2 \
                 ORDER BY c.created_at ASC, c.id ASC",
            )?;
            let rows = stmt.query_map(params![&aid, &tid], row_to_channel)?;
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
        input: NewChannel<'_>,
    ) -> Result<Channel, CatalogError> {
        let tid = tenant_id.0.clone();
        let id = Ulid::new().to_string();
        let now = Utc::now().to_rfc3339();
        let name = input.name.to_owned();
        let kind = input.kind.as_str().to_owned();
        let config_json = match input.config {
            Some(v) => serde_json::to_string(v)?,
            None => "{}".to_owned(),
        };

        blocking(self.conn.clone(), move |c| {
            c.execute(
                "INSERT INTO channels \
                 (id, tenant_id, name, kind, config, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![&id, &tid, &name, &kind, &config_json, &now],
            )
            .map_err(|e| match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CatalogError::Conflict(format!(
                        "channel name already exists: {name} (tenant={tid})"
                    ))
                }
                _ => CatalogError::Sqlite(e),
            })?;

            c.query_row(
                &format!("{SELECT_CHANNEL_COLUMNS} WHERE id = ?1"),
                [&id],
                row_to_channel,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &ChannelId,
        input: ChannelPatch<'_>,
    ) -> Result<Channel, CatalogError> {
        let tid = tenant_id.0.clone();
        let cid = id.0.clone();
        let now = Utc::now().to_rfc3339();
        let name = input.name.map(str::to_owned);
        let kind_str = input.kind.map(|k| k.as_str().to_owned());
        // Two layers of Option: outer = "field present in patch", inner =
        // "JSON null clears to {} / Some replaces". `Some(None)` means the
        // patch sets the config to `{}`.
        let config_json = match input.config {
            Some(Some(v)) => Some(Some(serde_json::to_string(v)?)),
            Some(None) => Some(Some("{}".to_owned())),
            None => None,
        };

        blocking(self.conn.clone(), move |c| {
            // Verify the row exists for this tenant before updating; otherwise
            // a partial WHERE clause still matches 0 rows and we'd silently
            // succeed. NotFound is the right surface for cross-tenant ids too.
            let exists: Option<String> = c
                .query_row(
                    "SELECT id FROM channels WHERE id = ?1 AND tenant_id = ?2",
                    params![&cid, &tid],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(CatalogError::NotFound(format!(
                    "channel id={cid} tenant={tid}"
                )));
            }

            // Build the SET clause dynamically; bind values in order.
            let mut sets: Vec<&str> = Vec::new();
            let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(ref n) = name {
                sets.push("name = ?");
                vals.push(Box::new(n.clone()));
            }
            if let Some(ref k) = kind_str {
                sets.push("kind = ?");
                vals.push(Box::new(k.clone()));
            }
            if let Some(Some(ref cj)) = config_json {
                sets.push("config = ?");
                vals.push(Box::new(cj.clone()));
            }
            if sets.is_empty() {
                // No-op patch: re-fetch and return.
                return c
                    .query_row(
                        &format!("{SELECT_CHANNEL_COLUMNS} WHERE id = ?1"),
                        [&cid],
                        row_to_channel,
                    )
                    .map_err(CatalogError::from);
            }
            sets.push("updated_at = ?");
            vals.push(Box::new(now.clone()));
            vals.push(Box::new(cid.clone()));
            vals.push(Box::new(tid.clone()));

            let sql = format!(
                "UPDATE channels SET {} WHERE id = ? AND tenant_id = ?",
                sets.join(", ")
            );
            let params_refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
            c.execute(&sql, params_refs.as_slice())
                .map_err(|e| match &e {
                    rusqlite::Error::SqliteFailure(err, _)
                        if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        CatalogError::Conflict(format!(
                            "channel name already exists for tenant={tid}"
                        ))
                    }
                    _ => CatalogError::Sqlite(e),
                })?;

            c.query_row(
                &format!("{SELECT_CHANNEL_COLUMNS} WHERE id = ?1"),
                [&cid],
                row_to_channel,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn delete(&self, tenant_id: &TenantId, id: &ChannelId) -> Result<(), CatalogError> {
        let tid = tenant_id.0.clone();
        let cid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let n = c.execute(
                "DELETE FROM channels WHERE id = ?1 AND tenant_id = ?2",
                params![&cid, &tid],
            )?;
            if n == 0 {
                Err(CatalogError::NotFound(format!(
                    "channel id={cid} tenant={tid}"
                )))
            } else {
                Ok(())
            }
        })
        .await
    }
}
