use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::{AgentId, MemoryPoolId, TenantId};
use crate::models::{Agent, AgentFile, AgentPatch, NewAgent, Page, PageRequest, ResolvedAgent};
use crate::repo::AgentRepository;
use crate::repo::sqlite::agent_file::row_to_agent_file;
use crate::repo::sqlite::memory_pool::row_to_pool;
use crate::repo::sqlite::skill::row_to_skill;
use crate::repo::sqlite::{SharedConn, blocking, decode_cursor, encode_cursor, parse_ts};

pub struct SqliteAgentRepository {
    conn: SharedConn,
}

impl SqliteAgentRepository {
    pub fn new(conn: SharedConn) -> Self {
        Self { conn }
    }
}

fn row_to_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<Agent> {
    let memory_pool_id_str: Option<String> = row.get(8)?;
    let max_turns: i64 = row.get(6)?;
    let max_tokens: Option<i64> = row.get(7)?;
    Ok(Agent {
        id: AgentId(row.get(0)?),
        tenant_id: TenantId(row.get(1)?),
        name: row.get(2)?,
        description: row.get(3)?,
        system_prompt: row.get(4)?,
        model: row.get(5)?,
        max_turns: max_turns.max(0) as u32,
        max_tokens: max_tokens.map(|n| n.max(0) as u32),
        memory_pool_id: memory_pool_id_str.map(MemoryPoolId),
        created_at: parse_ts(row.get::<_, String>(9)?)?,
        updated_at: parse_ts(row.get::<_, String>(10)?)?,
    })
}

const SELECT_AGENT_COLUMNS: &str = "SELECT id, tenant_id, name, description, system_prompt, model, \
        max_turns, max_tokens, memory_pool_id, created_at, updated_at FROM agents";

#[async_trait]
impl AgentRepository for SqliteAgentRepository {
    async fn get(&self, tenant_id: &TenantId, id: &AgentId) -> Result<Agent, CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_AGENT_COLUMNS} WHERE tenant_id = ?1 AND id = ?2"),
                params![&tid, &aid],
                row_to_agent,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("agent id={aid} tenant={tid}")))
        })
        .await
    }

    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Agent, CatalogError> {
        let tid = tenant_id.0.clone();
        let name = name.to_owned();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                &format!("{SELECT_AGENT_COLUMNS} WHERE tenant_id = ?1 AND name = ?2"),
                params![&tid, &name],
                row_to_agent,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("agent name={name} tenant={tid}")))
        })
        .await
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Agent>, CatalogError> {
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
            let fetch_limit = limit + 1;
            let mut items: Vec<Agent> = match (after_decoded, name_like.as_deref()) {
                (Some((created_at, id)), Some(needle)) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_AGENT_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND name LIKE ?2 \
                           AND (created_at > ?3 OR (created_at = ?3 AND id > ?4)) \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?5"
                    ))?;
                    let rows = stmt.query_map(
                        params![&tid, needle, &created_at, &id, fetch_limit],
                        row_to_agent,
                    )?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (Some((created_at, id)), None) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_AGENT_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND (created_at > ?2 OR (created_at = ?2 AND id > ?3)) \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?4"
                    ))?;
                    let rows =
                        stmt.query_map(params![&tid, &created_at, &id, fetch_limit], row_to_agent)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (None, Some(needle)) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_AGENT_COLUMNS} \
                         WHERE tenant_id = ?1 \
                           AND name LIKE ?2 \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?3"
                    ))?;
                    let rows = stmt.query_map(params![&tid, needle, fetch_limit], row_to_agent)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push(row?);
                    }
                    out
                }
                (None, None) => {
                    let mut stmt = c.prepare(&format!(
                        "{SELECT_AGENT_COLUMNS} \
                         WHERE tenant_id = ?1 \
                         ORDER BY created_at ASC, id ASC \
                         LIMIT ?2"
                    ))?;
                    let rows = stmt.query_map(params![&tid, fetch_limit], row_to_agent)?;
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
                .map(|a| encode_cursor(&a.created_at.to_rfc3339(), a.id.as_str()));
            let end_cursor = items
                .last()
                .map(|a| encode_cursor(&a.created_at.to_rfc3339(), a.id.as_str()));

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

    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewAgent<'_>,
    ) -> Result<Agent, CatalogError> {
        let tid = tenant_id.0.clone();
        let id = Ulid::new().to_string();
        let now = Utc::now().to_rfc3339();
        let name = input.name.to_owned();
        let description = input.description.map(str::to_owned);
        let system_prompt = input.system_prompt.to_owned();
        let model = input.model.to_owned();
        let max_turns = input.max_turns.unwrap_or(100) as i64;
        let max_tokens = input.max_tokens.map(|n| n as i64);
        let memory_pool_id = input.memory_pool_id.map(|id| id.0.clone());
        let skill_ids: Vec<String> = input.skill_ids.iter().map(|s| s.0.clone()).collect();
        let capabilities: Vec<String> = input.capabilities.to_vec();
        let channel_ids: Vec<String> = input.channel_ids.iter().map(|c| c.0.clone()).collect();

        blocking(self.conn.clone(), move |c| {
            let tx = c.transaction()?;

            // Validate that the memory pool (if specified) belongs to the tenant.
            if let Some(mpid) = &memory_pool_id {
                let exists: Option<String> = tx
                    .query_row(
                        "SELECT id FROM memory_pools WHERE id = ?1 AND tenant_id = ?2",
                        params![mpid, &tid],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Err(CatalogError::Validation(format!(
                        "memory_pool id={mpid} not found for tenant={tid}"
                    )));
                }
            }

            // Validate that all referenced skills exist in this tenant.
            for sid in &skill_ids {
                let exists: Option<String> = tx
                    .query_row(
                        "SELECT id FROM skills WHERE id = ?1 AND tenant_id = ?2",
                        params![sid, &tid],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Err(CatalogError::Validation(format!(
                        "skill id={sid} not found for tenant={tid}"
                    )));
                }
            }

            // Validate that all referenced channels exist in this tenant.
            for chid in &channel_ids {
                let exists: Option<String> = tx
                    .query_row(
                        "SELECT id FROM channels WHERE id = ?1 AND tenant_id = ?2",
                        params![chid, &tid],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Err(CatalogError::Validation(format!(
                        "channel id={chid} not found for tenant={tid}"
                    )));
                }
            }

            tx.execute(
                "INSERT INTO agents (id, tenant_id, name, description, system_prompt, model, \
                                     max_turns, max_tokens, memory_pool_id, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    &id,
                    &tid,
                    &name,
                    &description,
                    &system_prompt,
                    &model,
                    &max_turns,
                    &max_tokens,
                    &memory_pool_id,
                    &now
                ],
            )
            // Pre-validation above ensures referenced FKs (memory_pool, skills)
            // exist before this insert. Combined with the catalog's single
            // Arc<Mutex<Connection>> serialization, FK race conditions are
            // structurally impossible in this increment, so any
            // ConstraintViolation here is necessarily a UNIQUE collision on
            // (tenant_id, name). If the catalog later moves to a multi-
            // connection pool, distinguish SQLITE_CONSTRAINT_UNIQUE vs
            // SQLITE_CONSTRAINT_FOREIGNKEY here.
            .map_err(|e| match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CatalogError::Conflict(format!(
                        "agent name already exists: {name} (tenant={tid})"
                    ))
                }
                _ => CatalogError::Sqlite(e),
            })?;

            for sid in &skill_ids {
                tx.execute(
                    "INSERT INTO agent_skills (agent_id, skill_id) VALUES (?1, ?2)",
                    params![&id, sid],
                )?;
            }
            for cap in &capabilities {
                tx.execute(
                    "INSERT INTO agent_capabilities (agent_id, capability) VALUES (?1, ?2)",
                    params![&id, cap],
                )?;
            }
            for chid in &channel_ids {
                tx.execute(
                    "INSERT INTO agent_channels (agent_id, channel_id) VALUES (?1, ?2)",
                    params![&id, chid],
                )?;
            }

            let agent = tx.query_row(
                &format!("{SELECT_AGENT_COLUMNS} WHERE id = ?1"),
                [&id],
                row_to_agent,
            )?;
            tx.commit()?;
            Ok(agent)
        })
        .await
    }

    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &AgentId,
        input: AgentPatch<'_>,
    ) -> Result<Agent, CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = id.0.clone();
        let now = Utc::now().to_rfc3339();
        let description = input.description.map(|inner| inner.map(str::to_owned));
        let system_prompt = input.system_prompt.map(str::to_owned);
        let model = input.model.map(str::to_owned);
        let max_turns = input.max_turns.map(|n| n as i64);
        let max_tokens = input.max_tokens.map(|inner| inner.map(|n| n as i64));
        let memory_pool_id = input
            .memory_pool_id
            .map(|inner| inner.map(|mpid| mpid.0.clone()));
        let skill_ids: Option<Vec<String>> = input
            .skill_ids
            .map(|ids| ids.iter().map(|s| s.0.clone()).collect());
        let capabilities: Option<Vec<String>> = input.capabilities.map(|caps| caps.to_vec());
        let channel_ids: Option<Vec<String>> = input
            .channel_ids
            .map(|ids| ids.iter().map(|c| c.0.clone()).collect());

        blocking(self.conn.clone(), move |c| {
            let tx = c.transaction()?;

            // Verify the row exists and belongs to the tenant before patching.
            let exists: Option<String> = tx
                .query_row(
                    "SELECT id FROM agents WHERE id = ?1 AND tenant_id = ?2",
                    params![&aid, &tid],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(CatalogError::NotFound(format!(
                    "agent id={aid} tenant={tid}"
                )));
            }

            // Validate referenced FKs before mutating join tables.
            if let Some(Some(mpid)) = &memory_pool_id {
                let exists: Option<String> = tx
                    .query_row(
                        "SELECT id FROM memory_pools WHERE id = ?1 AND tenant_id = ?2",
                        params![mpid, &tid],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Err(CatalogError::Validation(format!(
                        "memory_pool id={mpid} not found for tenant={tid}"
                    )));
                }
            }
            if let Some(ids) = &skill_ids {
                for sid in ids {
                    let exists: Option<String> = tx
                        .query_row(
                            "SELECT id FROM skills WHERE id = ?1 AND tenant_id = ?2",
                            params![sid, &tid],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if exists.is_none() {
                        return Err(CatalogError::Validation(format!(
                            "skill id={sid} not found for tenant={tid}"
                        )));
                    }
                }
            }
            if let Some(ids) = &channel_ids {
                for chid in ids {
                    let exists: Option<String> = tx
                        .query_row(
                            "SELECT id FROM channels WHERE id = ?1 AND tenant_id = ?2",
                            params![chid, &tid],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if exists.is_none() {
                        return Err(CatalogError::Validation(format!(
                            "channel id={chid} not found for tenant={tid}"
                        )));
                    }
                }
            }

            // Build dynamic UPDATE statement on the agents row.
            let mut sets: Vec<&str> = Vec::new();
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            if let Some(d) = &description {
                sets.push("description = ?");
                values.push(match d {
                    Some(s) => rusqlite::types::Value::Text(s.clone()),
                    None => rusqlite::types::Value::Null,
                });
            }
            if let Some(s) = &system_prompt {
                sets.push("system_prompt = ?");
                values.push(rusqlite::types::Value::Text(s.clone()));
            }
            if let Some(m) = &model {
                sets.push("model = ?");
                values.push(rusqlite::types::Value::Text(m.clone()));
            }
            if let Some(n) = max_turns {
                sets.push("max_turns = ?");
                values.push(rusqlite::types::Value::Integer(n));
            }
            if let Some(t) = &max_tokens {
                sets.push("max_tokens = ?");
                values.push(match t {
                    Some(n) => rusqlite::types::Value::Integer(*n),
                    None => rusqlite::types::Value::Null,
                });
            }
            if let Some(mp) = &memory_pool_id {
                sets.push("memory_pool_id = ?");
                values.push(match mp {
                    Some(s) => rusqlite::types::Value::Text(s.clone()),
                    None => rusqlite::types::Value::Null,
                });
            }
            sets.push("updated_at = ?");
            values.push(rusqlite::types::Value::Text(now.clone()));

            let sql = format!(
                "UPDATE agents SET {} WHERE id = ? AND tenant_id = ?",
                sets.join(", ")
            );
            values.push(rusqlite::types::Value::Text(aid.clone()));
            values.push(rusqlite::types::Value::Text(tid.clone()));

            let params_refs: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            tx.execute(&sql, params_refs.as_slice())
                .map_err(|e| match &e {
                    rusqlite::Error::SqliteFailure(err, _)
                        if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        CatalogError::Conflict("agent update conflicts on unique key".into())
                    }
                    _ => CatalogError::Sqlite(e),
                })?;

            // Replace the skill set if patched.
            if let Some(ids) = &skill_ids {
                tx.execute(
                    "DELETE FROM agent_skills WHERE agent_id = ?1",
                    params![&aid],
                )?;
                for sid in ids {
                    tx.execute(
                        "INSERT INTO agent_skills (agent_id, skill_id) VALUES (?1, ?2)",
                        params![&aid, sid],
                    )?;
                }
            }

            // Replace the capabilities set if patched.
            if let Some(caps) = &capabilities {
                tx.execute(
                    "DELETE FROM agent_capabilities WHERE agent_id = ?1",
                    params![&aid],
                )?;
                for cap in caps {
                    tx.execute(
                        "INSERT INTO agent_capabilities (agent_id, capability) VALUES (?1, ?2)",
                        params![&aid, cap],
                    )?;
                }
            }

            // S046 — replace the channel set if patched.
            if let Some(ids) = &channel_ids {
                tx.execute(
                    "DELETE FROM agent_channels WHERE agent_id = ?1",
                    params![&aid],
                )?;
                for chid in ids {
                    tx.execute(
                        "INSERT INTO agent_channels (agent_id, channel_id) VALUES (?1, ?2)",
                        params![&aid, chid],
                    )?;
                }
            }

            let agent = tx.query_row(
                &format!("{SELECT_AGENT_COLUMNS} WHERE id = ?1"),
                [&aid],
                row_to_agent,
            )?;
            tx.commit()?;
            Ok(agent)
        })
        .await
    }

    async fn delete(&self, tenant_id: &TenantId, id: &AgentId) -> Result<(), CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let n = c.execute(
                "DELETE FROM agents WHERE id = ?1 AND tenant_id = ?2",
                params![&aid, &tid],
            )?;
            if n == 0 {
                return Err(CatalogError::NotFound(format!(
                    "agent id={aid} tenant={tid}"
                )));
            }
            Ok(())
        })
        .await
    }

    async fn capabilities(
        &self,
        tenant_id: &TenantId,
        id: &AgentId,
    ) -> Result<Vec<String>, CatalogError> {
        let tid = tenant_id.0.clone();
        let aid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            let visible: Option<String> = c
                .query_row(
                    "SELECT id FROM agents WHERE tenant_id = ?1 AND id = ?2",
                    params![&tid, &aid],
                    |row| row.get(0),
                )
                .optional()?;
            if visible.is_none() {
                return Ok(Vec::new());
            }

            let mut stmt = c.prepare(
                "SELECT capability FROM agent_capabilities \
                 WHERE agent_id = ?1 ORDER BY capability ASC",
            )?;
            let rows = stmt.query_map([&aid], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    async fn resolve(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<ResolvedAgent, CatalogError> {
        let tid = tenant_id.0.clone();
        let name = name.to_owned();
        blocking(self.conn.clone(), move |c| {
            let tx = c.transaction()?;

            let agent: Agent = tx
                .query_row(
                    &format!("{SELECT_AGENT_COLUMNS} WHERE tenant_id = ?1 AND name = ?2"),
                    params![&tid, &name],
                    row_to_agent,
                )
                .optional()?
                .ok_or_else(|| CatalogError::NotFound(format!("agent name={name} tenant={tid}")))?;

            let skills: Vec<crate::models::Skill> = {
                let mut stmt = tx.prepare(
                    "SELECT s.id, s.tenant_id, s.name, s.description, s.body, s.metadata, \
                            s.created_at, s.updated_at \
                     FROM skills s \
                     JOIN agent_skills a ON a.skill_id = s.id \
                     WHERE a.agent_id = ?1 AND s.tenant_id = ?2 \
                     ORDER BY s.created_at ASC, s.id ASC",
                )?;
                let rows = stmt.query_map(params![agent.id.as_str(), &tid], row_to_skill)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };

            let capabilities: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT capability FROM agent_capabilities \
                     WHERE agent_id = ?1 ORDER BY capability ASC",
                )?;
                let rows = stmt.query_map([agent.id.as_str()], |r| r.get::<_, String>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };

            let memory_pool = if let Some(mpid) = &agent.memory_pool_id {
                tx.query_row(
                    "SELECT id, tenant_id, name, embedding_model, config, created_at, updated_at \
                     FROM memory_pools WHERE id = ?1 AND tenant_id = ?2",
                    params![mpid.as_str(), &tid],
                    row_to_pool,
                )
                .optional()?
            } else {
                None
            };

            // S045: per-agent files. Joined inside the same transaction so
            // ResolvedAgent is a consistent snapshot. Tenant scoping via
            // the parent agent's row (already verified above by the
            // get_by_name lookup that produced `agent`).
            let files: Vec<AgentFile> = {
                let mut stmt = tx.prepare(
                    "SELECT id, agent_id, name, mime_type, size_bytes, created_at, updated_at \
                     FROM agent_files WHERE agent_id = ?1 \
                     ORDER BY created_at ASC, id ASC",
                )?;
                let rows = stmt.query_map([agent.id.as_str()], row_to_agent_file)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };

            // S046: channels. Same tenant-scoped JOIN as files.
            let channels: Vec<crate::models::Channel> = {
                let mut stmt = tx.prepare(
                    "SELECT c.id, c.tenant_id, c.name, c.kind, c.config, \
                            c.created_at, c.updated_at \
                     FROM channels c \
                     JOIN agent_channels ac ON ac.channel_id = c.id \
                     WHERE ac.agent_id = ?1 AND c.tenant_id = ?2 \
                     ORDER BY c.created_at ASC, c.id ASC",
                )?;
                let rows = stmt.query_map(
                    params![agent.id.as_str(), &tid],
                    crate::repo::sqlite::channel::row_to_channel,
                )?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };

            tx.commit()?;

            Ok(ResolvedAgent {
                id: agent.id,
                name: agent.name,
                system_prompt: agent.system_prompt,
                model: agent.model,
                max_turns: agent.max_turns,
                max_tokens: agent.max_tokens,
                skills,
                capabilities,
                memory_pool,
                files,
                channels,
            })
        })
        .await
    }
}
