//! Database connection from the standard libpq environment only, the migrator, and the run
//! lock.
//!
//! `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGSSLMODE` and, for the password, `PGPASSFILE`
//! (pgpass format). A password is never accepted from anywhere else: not the config file, not
//! the command line, not a URL, and not `PGPASSWORD`.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Connection, PgConnection, PgPool};

/// Connection options from the libpq environment variables.
pub fn connect_options() -> Result<PgConnectOptions> {
    connect_options_from(|k| std::env::var_os(k))
}

fn connect_options_from(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PgConnectOptions> {
    if var("PGPASSWORD").is_some() {
        bail!("PGPASSWORD is set; the database password is accepted only via PGPASSFILE");
    }
    // sqlx silently falls back to `prefer` on an unrecognised PGSSLMODE; refuse instead, so a
    // typo cannot turn `require` into a possible plaintext connection.
    if let Some(mode) = var("PGSSLMODE") {
        let mode = mode.to_str().context("PGSSLMODE is not valid UTF-8")?;
        mode.parse::<PgSslMode>()
            .map_err(|_| anyhow::anyhow!("PGSSLMODE={mode:?} is not a valid sslmode"))?;
    }
    // `new()` reads PGPASSFILE, but also falls back to ~/.pgpass; only PGPASSFILE is allowed.
    let options = if var("PGPASSFILE").is_some() {
        PgConnectOptions::new()
    } else {
        PgConnectOptions::new_without_pgpass()
    };
    Ok(options.application_name("feed-index"))
}

pub async fn connect(options: &PgConnectOptions, max_connections: u32) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options.clone())
        .await
        .context("connecting to PostgreSQL (libpq environment: PGHOST, PGDATABASE, ...)")
}

/// The embedded migrations, recorded in `feeds._sqlx_migrations`.
///
/// The database may also hold other sqlx-migrated programs (maildir-index) whose migrator uses
/// `public._sqlx_migrations` with its own version 1; sharing that table breaks both programs
/// (see docs/schema.md). The migrator creates the schema, so the SQL does not.
pub fn migrator() -> Migrator {
    let mut m = sqlx::migrate!("./migrations");
    m.dangerous_set_table_name("feeds._sqlx_migrations");
    m.create_schema("feeds");
    m
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    migrator().run(pool).await.context("applying migrations")
}

/// Key of the session advisory lock that keeps runs from overlapping: the first eight bytes of
/// SHA-256("feed-index fetch").
pub fn lock_key() -> i64 {
    let d = Sha256::digest(b"feed-index fetch");
    i64::from_be_bytes(d[..8].try_into().expect("8 bytes"))
}

/// The run lock, held on a dedicated connection for the whole run. A session lock on a pooled
/// connection would outlive the code that took it.
pub struct RunLock {
    conn: PgConnection,
}

impl RunLock {
    /// `None` if another run holds the lock.
    pub async fn try_acquire(options: &PgConnectOptions) -> Result<Option<RunLock>> {
        let mut conn = PgConnection::connect_with(options)
            .await
            .context("connecting to PostgreSQL for the run lock (libpq environment)")?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(lock_key())
            .fetch_one(&mut conn)
            .await
            .context("taking the run lock")?;
        if locked {
            Ok(Some(RunLock { conn }))
        } else {
            conn.close().await.ok();
            Ok(None)
        }
    }

    pub async fn release(mut self) -> Result<()> {
        let released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(lock_key())
            .fetch_one(&mut self.conn)
            .await
            .context("releasing the run lock")?;
        if !released {
            bail!("the run lock was not held at release");
        }
        self.conn
            .close()
            .await
            .context("closing the lock connection")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.into())
    }

    #[test]
    fn rejects_pgpassword() {
        let e = connect_options_from(env(&[("PGPASSWORD", "x")])).unwrap_err();
        assert!(e.to_string().contains("PGPASSFILE"));
    }

    #[test]
    fn rejects_bad_sslmode() {
        assert!(connect_options_from(env(&[("PGSSLMODE", "requre")])).is_err());
        assert!(connect_options_from(env(&[("PGSSLMODE", "require")])).is_ok());
    }

    #[test]
    fn application_name() {
        let o = connect_options_from(env(&[])).unwrap();
        assert_eq!(o.get_application_name(), Some("feed-index"));
    }

    #[test]
    fn lock_key_is_fixed() {
        assert_eq!(lock_key(), lock_key());
        assert_ne!(lock_key(), 0);
    }

    #[test]
    fn migrator_uses_own_table() {
        let m = migrator();
        assert_eq!(m.table_name, "feeds._sqlx_migrations");
        assert_eq!(m.create_schemas.as_ref(), ["feeds"]);
    }
}
