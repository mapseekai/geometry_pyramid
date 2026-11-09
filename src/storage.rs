//! Bind SQL relations once and keep their PostgreSQL identities across requests.
use pgrx::prelude::*;
use std::error::Error;

pub(crate) type DbError = Box<dyn Error + Send + Sync>;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct Relation {
    pub oid: u32,
    pub schema: String,
    pub name: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct Bindings {
    pub source: Option<Relation>,
    pub storage: Option<Relation>,
    bound: bool,
}

impl Bindings {
    pub fn validate(&self, name: &str) -> Result<(), DbError> {
        if !self.bound {
            return Err(format!(
                "pyramid '{name}' needs explicit source_rel and storage_rel bindings after upgrade"
            )
            .into());
        }
        if let Some(target) = &self.storage {
            if target.name != tile_table_name(name)
                || self.source.as_ref().is_some_and(|s| s.oid == target.oid)
            {
                return Err(format!("invalid tile table binding for pyramid '{name}'").into());
            }
        }
        Ok(())
    }
}

impl Relation {
    pub fn sql(&self) -> String {
        spi::quote_qualified_identifier(&self.schema, &self.name)
    }

    pub fn lookup(sql_name: &str) -> Result<Option<Self>, DbError> {
        let row: Option<pgrx::JsonB> = Spi::get_one_with_args(
            "SELECT (SELECT jsonb_build_object('oid',c.oid::bigint,'schema',n.nspname,'name',c.relname)
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
             WHERE c.oid=pg_catalog.to_regclass($1))",
            &[sql_name.into()],
        )?;
        Ok(row.map(|v| serde_json::from_value(v.0)).transpose()?)
    }

    /// Hold the relation across name resolution and use, including concurrent replacement.
    pub fn lock(&self) -> Result<(), DbError> {
        unsafe { pg_sys::LockRelationOid(self.oid.into(), pg_sys::AccessShareLock as i32) };
        let matches = Spi::get_one_with_args::<bool>(
            "SELECT pg_catalog.to_regclass($1)::oid = $2",
            &[self.sql().into(), pg_sys::Oid::from(self.oid).into()],
        )?;
        if matches != Some(true) {
            return Err("registered relation changed during lookup; retry the operation".into());
        }
        Ok(())
    }

    pub fn lock_output(&self) -> Result<(), DbError> {
        self.lock()?;
        // Serialize trigger/rule changes with the eligibility check and the writes.
        Spi::run(&format!(
            "LOCK TABLE ONLY {} IN ROW EXCLUSIVE MODE",
            self.sql()
        ))?;
        if Spi::get_one_with_args::<bool>(
            "SELECT pg_catalog.row_security_active($1)",
            &[pg_sys::Oid::from(self.oid).into()],
        )? == Some(true)
        {
            return Err("cannot replace tile storage while an RLS policy filters its rows".into());
        }
        Ok(())
    }

    pub fn drop_table(&self) -> Result<(), DbError> {
        self.lock()?;
        Spi::run(&format!("DROP TABLE {}", self.sql()))?;
        if Spi::get_one_with_args::<bool>(
            "SELECT EXISTS (SELECT FROM pg_catalog.pg_class WHERE oid=$1)",
            &[pg_sys::Oid::from(self.oid).into()],
        )? == Some(true)
        {
            return Err("tile relation changed during DROP; rolling back".into());
        }
        Ok(())
    }
}

pub fn tile_table_name(name: &str) -> String {
    format!("_gp_tiles_{name}")
}

/// Call before looking up the catalog for every operation that changes a pyramid.
pub fn lock_pyramid(name: &str) -> Result<(), spi::Error> {
    Spi::run_with_args(
        "SELECT pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtextextended('geometry_pyramid:' || $1, 0))",
        &[name.into()],
    )
}

pub fn tile_table_create_sql(table: &str, stage: bool) -> String {
    let persistence = if stage { "UNLOGGED " } else { "" };
    let id = if stage { "" } else { "id bigserial," };
    format!(
        "CREATE {persistence}TABLE {table} (
        {id} z smallint NOT NULL, x integer NOT NULL, y integer NOT NULL,
        format text NOT NULL, srid integer NOT NULL, mvt bytea NOT NULL, PRIMARY KEY(z,x,y))"
    )
}

pub fn create_output(name: &str) -> Result<Relation, DbError> {
    let schema: String = Spi::get_one("SELECT pg_catalog.current_schema()::text")?
        .ok_or("no writable schema in the search path")?;
    let table = spi::quote_qualified_identifier(&schema, &tile_table_name(name));
    // CREATE without IF NOT EXISTS is also the atomic ownership check.
    Spi::run(&tile_table_create_sql(&table, false))?;
    Relation::lookup(&table)?.ok_or_else(|| "new tile table missing".into())
}
