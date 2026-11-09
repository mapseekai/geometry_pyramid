//! Only parallel builds need committed scratch tables. An OID-bound ownership
//! marker distinguishes a crashed build's scratch from a user's same-named table.
use crate::storage::{DbError, Relation};
use pgrx::prelude::*;

fn marker(rel: &Relation) -> String {
    format!("geometry_pyramid:work:{}:{}", rel.oid, rel.sql())
}

fn owned(rel: &Relation) -> Result<bool, DbError> {
    let actual = Spi::get_one_with_args::<String>(
        "SELECT pg_catalog.obj_description($1, 'pg_class')",
        &[pg_sys::Oid::from(rel.oid).into()],
    )?;
    Ok(actual.as_deref() == Some(&marker(rel)))
}

pub(crate) fn check(table: &str) -> Result<Relation, DbError> {
    let rel = Relation::lookup(table)?.ok_or("worker stage is missing")?;
    rel.lock()?;
    if !owned(&rel)? {
        return Err(format!("refusing to use unmanaged worker table {table}").into());
    }
    Ok(rel)
}

pub(crate) fn discard_if_owned(table: &str) -> Result<(), DbError> {
    if let Some(rel) = Relation::lookup(table)? {
        if owned(&rel)? {
            check(table)?.drop_table()?;
        }
    }
    Ok(())
}

/// Recover a failed worker publication on the coordinator. Finish this build
/// locally so both the removal and its replacement commit together.
pub(crate) fn reclaim_for_retry(schema: &str, name: &str, prepared: bool) -> Result<bool, DbError> {
    let mut reclaimed = false;
    let names = [
        Some(format!("_gp_shadow_{name}")),
        prepared.then(|| format!("_gp_prep_{name}")),
    ];
    for name in names.into_iter().flatten() {
        let table = spi::quote_qualified_identifier(schema, &name);
        if Relation::lookup(&table)?.is_some() {
            check(&table)?.drop_table()?;
            reclaimed = true;
        }
    }
    Ok(reclaimed)
}

pub(crate) fn create(table: &str, ddl: &str) -> Result<(), DbError> {
    // CREATE fails atomically if a same-named object appeared after preflight.
    Spi::run(ddl)?;
    let rel = Relation::lookup(table)?.ok_or("worker stage creation failed")?;
    // Default privileges in the output schema can grant readers access to new
    // tables. Scratch can contain unfiltered source attributes.
    let grantees = Spi::get_one_with_args::<String>(
        "SELECT string_agg(CASE WHEN a.grantee=0 THEN 'PUBLIC' ELSE pg_catalog.quote_ident(pg_catalog.pg_get_userbyid(a.grantee)) END, ', ')
         FROM (SELECT DISTINCT c.relowner,(pg_catalog.aclexplode(c.relacl)).grantee
               FROM pg_catalog.pg_class c WHERE c.oid=$1) a WHERE a.grantee<>a.relowner",
        &[pg_sys::Oid::from(rel.oid).into()],
    )?;
    if let Some(grantees) = grantees {
        Spi::run(&format!("REVOKE ALL ON TABLE {table} FROM {grantees}"))?;
    }
    Spi::run(&format!(
        "COMMENT ON TABLE {table} IS {}",
        spi::quote_literal(marker(&rel))
    ))?;
    Ok(())
}
