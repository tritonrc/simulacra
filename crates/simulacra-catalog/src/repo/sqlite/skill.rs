use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::{AgentId, SkillId, TenantId};
use crate::models::{NewSkill, Page, PageRequest, Skill, SkillPatch};
use crate::repo::SkillRepository;
use crate::repo::sqlite::{SharedConn, blocking, decode_cursor, encode_cursor, parse_ts};

pub struct SqliteSkillRepository {
    conn: SharedConn,
}

impl SqliteSkillRepository {
    pub fn new(conn: SharedConn) -> Self {
        Self { conn }
    }
}

pub(crate) fn row_to_skill(row: &rusqlite::Row<'_>) -> rusqlite::Result<Skill> {
    let metadata_str: Option<String> = row.get(5)?;
    let metadata = match metadata_str {
        Some(s) => Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(Skill {
        id: SkillId(row.get(0)?),
        tenant_id: TenantId(row.get(1)?),
        name: row.get(2)?,
        description: row.get(3)?,
        body: row.get(4)?,
        metadata,
        created_at: parse_ts(row.get::<_, String>(6)?)?,
        updated_at: parse_ts(row.get::<_, String>(7)?)?,
    })
}

const SELECT_SKILL_COLUMNS: &str =
    "SELECT id, tenant_id, name, description, body, metadata, created_at, updated_at FROM skills";

#[async_trait]
impl SkillRepository for SqliteSkillRepository {
    async fn get(&self, tenant_id: &TenantId, id: &SkillId) -> Result<Skill, CatalogError> {
        let tid = tenant_id.0.clone();
        let sid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_SKILL_COLUMNS} WHERE tenant_id = ?1 AND id = ?2"),
                params![&tid, &sid],
                row_to_skill,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("skill id={sid} tenant={tid}")))
        })
        .await
    }

    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Skill, CatalogError> {
        let tid = tenant_id.0.clone();
        let name = name.to_owned();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_SKILL_COLUMNS} WHERE tenant_id = ?1 AND name = ?2"),
                params![&tid, &name],
                row_to_skill,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("skill name={name} tenant={tid}")))
        })
        .await
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Skill>, CatalogError> {
        let tid = tenant_id.0.clone();
        let limit = page.first.unwrap_or(20).max(1) as i64;
        let after_decoded = match &page.after {
            Some(c) => Some(decode_cursor(c)?),
            None => None,
        };
        // Pre-format the LIKE pattern; bind it as a parameter to avoid SQL
        // injection. SQLite's LIKE is case-insensitive for ASCII by default,
        // which matches the GraphQL `nameContains` semantics callers expect.
        let name_like = name_contains.map(|n| format!("%{n}%"));

        blocking(self.conn.clone(), move |c| {
            // Fetch one extra row to compute has_next_page.
            let fetch_limit = limit + 1;
            let mut items: Vec<Skill> = match (after_decoded, name_like.as_deref()) {
                (Some((created_at, id)), Some(needle)) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_SKILL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND name LIKE ?2 \
                           AND (created_at > ?3 OR (created_at = ?3 AND id > ?4)) \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?5"
                    ))?;
                    let rows = stmt.query_map(
                        params![&tid, needle, &created_at, &id, fetch_limit],
                        row_to_skill,
                    )?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (Some((created_at, id)), None) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_SKILL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND (created_at > ?2 OR (created_at = ?2 AND id > ?3)) \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?4"
                    ))?;
                    let rows =
                        stmt.query_map(params![&tid, &created_at, &id, fetch_limit], row_to_skill)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (None, Some(needle)) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_SKILL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND name LIKE ?2 \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?3"
                    ))?;
                    let rows = stmt.query_map(params![&tid, needle, fetch_limit], row_to_skill)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (None, None) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_SKILL_COLUMNS} \
                         WHERE tenant_id = ?1 \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?2"
                    ))?;
                    let rows = stmt.query_map(params![&tid, fetch_limit], row_to_skill)?;
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
    ) -> Result<Vec<Skill>, CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = agent_id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let mut stmt = c.prepare(
                "SELECT s.id, s.tenant_id, s.name, s.description, s.body, s.metadata, \
                        s.created_at, s.updated_at \
                 FROM skills s \
                 JOIN agent_skills a ON a.skill_id = s.id \
                 WHERE a.agent_id = ?1 AND s.tenant_id = ?2 \
                 ORDER BY s.created_at ASC, s.id ASC",
            )?;
            let rows = stmt.query_map(params![&aid, &tid], row_to_skill)?;
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
        input: NewSkill<'_>,
    ) -> Result<Skill, CatalogError> {
        let tid = tenant_id.0.clone();
        let id = Ulid::new().to_string();
        let now = Utc::now().to_rfc3339();
        let name = input.name.to_owned();
        let description = input.description.map(str::to_owned);
        let body = input.body.to_owned();
        let metadata = match input.metadata {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };

        blocking(self.conn.clone(), move |c| {
            c.execute(
                "INSERT INTO skills \
                 (id, tenant_id, name, description, body, metadata, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![&id, &tid, &name, &description, &body, &metadata, &now],
            )
            // The skills table has no FK columns, so the only constraint that
            // can fire here is the (tenant_id, name) UNIQUE index. With the
            // catalog's single Arc<Mutex<Connection>> serialization, no
            // concurrent insert can race this one. If the catalog later moves
            // to a multi-connection pool, distinguish SQLITE_CONSTRAINT_UNIQUE
            // vs SQLITE_CONSTRAINT_FOREIGNKEY here.
            .map_err(|e| match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CatalogError::Conflict(format!(
                        "skill name already exists: {name} (tenant={tid})"
                    ))
                }
                _ => CatalogError::Sqlite(e),
            })?;

            c.query_row(
                &format!("{SELECT_SKILL_COLUMNS} WHERE id = ?1"),
                [&id],
                row_to_skill,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &SkillId,
        input: SkillPatch<'_>,
    ) -> Result<Skill, CatalogError> {
        let tid = tenant_id.0.clone();
        let sid = id.0.clone();
        let now = Utc::now().to_rfc3339();
        let name = input.name.map(str::to_owned);
        let description = input.description.map(|inner| inner.map(str::to_owned));
        let body = input.body.map(str::to_owned);
        let metadata = match input.metadata {
            Some(inner) => Some(match inner {
                Some(v) => Some(serde_json::to_string(v)?),
                None => None,
            }),
            None => None,
        };

        blocking(self.conn.clone(), move |c| {
            let exists: Option<String> = c
                .query_row(
                    "SELECT id FROM skills WHERE id = ?1 AND tenant_id = ?2",
                    params![&sid, &tid],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(CatalogError::NotFound(format!(
                    "skill id={sid} tenant={tid}"
                )));
            }

            let mut sets: Vec<&str> = Vec::new();
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            if let Some(n) = &name {
                sets.push("name = ?");
                values.push(rusqlite::types::Value::Text(n.clone()));
            }
            if let Some(d) = &description {
                sets.push("description = ?");
                values.push(match d {
                    Some(s) => rusqlite::types::Value::Text(s.clone()),
                    None => rusqlite::types::Value::Null,
                });
            }
            if let Some(b) = &body {
                sets.push("body = ?");
                values.push(rusqlite::types::Value::Text(b.clone()));
            }
            if let Some(m) = &metadata {
                sets.push("metadata = ?");
                values.push(match m {
                    Some(s) => rusqlite::types::Value::Text(s.clone()),
                    None => rusqlite::types::Value::Null,
                });
            }
            sets.push("updated_at = ?");
            values.push(rusqlite::types::Value::Text(now.clone()));

            let sql = format!(
                "UPDATE skills SET {} WHERE id = ? AND tenant_id = ?",
                sets.join(", ")
            );
            values.push(rusqlite::types::Value::Text(sid.clone()));
            values.push(rusqlite::types::Value::Text(tid.clone()));

            let params_refs: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();

            c.execute(&sql, params_refs.as_slice())
                .map_err(|e| match &e {
                    rusqlite::Error::SqliteFailure(err, _)
                        if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        CatalogError::Conflict("skill update conflicts on unique key".into())
                    }
                    _ => CatalogError::Sqlite(e),
                })?;

            c.query_row(
                &format!("{SELECT_SKILL_COLUMNS} WHERE id = ?1"),
                [&sid],
                row_to_skill,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn delete(&self, tenant_id: &TenantId, id: &SkillId) -> Result<(), CatalogError> {
        let tid = tenant_id.0.clone();
        let sid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let n = c.execute(
                "DELETE FROM skills WHERE id = ?1 AND tenant_id = ?2",
                params![&sid, &tid],
            )?;
            if n == 0 {
                return Err(CatalogError::NotFound(format!(
                    "skill id={sid} tenant={tid}"
                )));
            }
            Ok(())
        })
        .await
    }
}
