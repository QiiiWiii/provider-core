//! SQLite persistence for observed usage facts.
//!
//! Enum values are mapped to column text by explicit `match`, not by reusing a
//! serde rename. The vocabulary in the database is a schema decision guarded by
//! `CHECK` constraints, so adding a variant should fail to compile here and force
//! that decision, rather than silently write a value the schema rejects.
//!
//! Token counts follow one rule end to end: the column holds a known number, and
//! `NULL` means "not a known number". The reason it is not known lives in
//! `token_kinds_json`, which carries an entry for every metric that is *not* a
//! plain provider-reported value — so a fully reported attempt stores `{}`.

mod codec;
mod repository;

use sqlx::SqlitePool;

use crate::sqlite::SqliteWriter;

pub(crate) use codec::{attempt_facts, logical_status_from, usage_error};

/// Observed-usage facts stored in the same SQLite database as accounts and auth.
///
/// One database keeps the deployment a single file to back up and a single set of
/// migrations. Reads use the shared pool under WAL; all mutations go through the
/// exclusive write connection so in-process callers never race `BEGIN IMMEDIATE`.
#[derive(Clone)]
pub struct SqliteUsageRepository {
    pub(crate) pool: SqlitePool,
    pub(crate) write: SqliteWriter,
}

impl SqliteUsageRepository {
    #[must_use]
    pub(crate) fn new(pool: SqlitePool, write: SqliteWriter) -> Self {
        Self { pool, write }
    }
}

#[cfg(test)]
mod tests;
