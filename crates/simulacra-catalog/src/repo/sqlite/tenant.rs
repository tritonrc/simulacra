use async_trait::async_trait;
use chrono::Utc;
use rusqlite::OptionalExtension;
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::TenantId;
use crate::models::Tenant;
use crate::repo::TenantRepository;
use crate::repo::sqlite::{SharedConn, blocking, parse_ts};

pub struct SqliteTenantRepository {
    conn: SharedConn,
}

impl SqliteTenantRepository {
    pub fn new(conn: SharedConn) -> Self {
        Self { conn }
    }
}

fn row_to_tenant(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tenant> {
    Ok(Tenant {
        id: TenantId(row.get::<_, String>(0)?),
        namespace: row.get(1)?,
        display_name: row.get(2)?,
        created_at: parse_ts(row.get::<_, String>(3)?)?,
        updated_at: parse_ts(row.get::<_, String>(4)?)?,
    })
}

#[async_trait]
impl TenantRepository for SqliteTenantRepository {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError> {
        let ns = namespace.to_owned();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                "SELECT id, namespace, display_name, created_at, updated_at \
                 FROM tenants WHERE namespace = ?1",
                [&ns],
                row_to_tenant,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("tenant ns={ns}")))
        })
        .await
    }

    async fn get_by_id(&self, id: &TenantId) -> Result<Tenant, CatalogError> {
        let id = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                "SELECT id, namespace, display_name, created_at, updated_at \
                 FROM tenants WHERE id = ?1",
                [&id],
                row_to_tenant,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("tenant id={id}")))
        })
        .await
    }

    async fn create(
        &self,
        namespace: &str,
        display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError> {
        let id = Ulid::new().to_string();
        let now = Utc::now().to_rfc3339();
        let ns = namespace.to_owned();
        let dn = display_name.map(str::to_owned);

        blocking(self.conn.clone(), move |c| {
            c.execute(
                "INSERT INTO tenants (id, namespace, display_name, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![&id, &ns, &dn, &now],
            )
            // The tenants table has no FK columns, so the only constraint that
            // can fire here is the namespace UNIQUE index. With the catalog's
            // single Arc<Mutex<Connection>> serialization, no concurrent insert
            // can race this one. If the catalog later moves to a multi-
            // connection pool, distinguish SQLITE_CONSTRAINT_UNIQUE vs
            // SQLITE_CONSTRAINT_FOREIGNKEY here.
            .map_err(|e| match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CatalogError::Conflict(format!("tenant namespace already exists: {ns}"))
                }
                _ => CatalogError::Sqlite(e),
            })?;
            c.query_row(
                "SELECT id, namespace, display_name, created_at, updated_at \
                 FROM tenants WHERE id = ?1",
                [&id],
                row_to_tenant,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    async fn get_or_create(
        &self,
        namespace: &str,
        display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError> {
        match self.get_by_namespace(namespace).await {
            Ok(t) => Ok(t),
            Err(CatalogError::NotFound(_)) => self.create(namespace, display_name).await,
            Err(e) => Err(e),
        }
    }
}
