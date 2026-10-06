//! Test harness: a throwaway database per test on the local server named by `DATABASE_URL`,
//! and a local HTTP server (wiremock on 127.0.0.1) serving hand-written feeds. Nothing here
//! touches the internet.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use feed_index::config::Config;
use feed_index::fetch::{self, FetchOptions, RunReport};
use feed_index::opml::{self, OpmlFeed};
use feed_index::sync::SyncReport;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A migrated throwaway database; `None` (after printing why) when `DATABASE_URL` is unset.
pub async fn test_db() -> Option<TestDb> {
    let db = test_db_unmigrated().await?;
    feed_index::db::migrate(&db.pool).await.unwrap();
    Some(db)
}

pub async fn test_db_unmigrated() -> Option<TestDb> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL is not set (point it at a local, disposable PostgreSQL)");
        return None;
    };
    static N: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "fdi_test_{}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let admin: PgConnectOptions = url.parse().expect("DATABASE_URL");
    let mut conn = PgConnection::connect_with(&admin)
        .await
        .expect("connect DATABASE_URL");
    // The name is generated above from digits and underscores only.
    conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .unwrap();
    conn.close().await.unwrap();
    let options = admin.clone().database(&name);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.clone())
        .await
        .unwrap();
    Some(TestDb {
        name,
        admin,
        options,
        pool,
        password: password_from_url(&url),
    })
}

fn password_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let userinfo = rest.split_once('@')?.0;
    Some(userinfo.split_once(':')?.1.to_string())
}

pub struct TestDb {
    pub name: String,
    admin: PgConnectOptions,
    pub options: PgConnectOptions,
    pub pool: PgPool,
    password: Option<String>,
}

impl TestDb {
    pub async fn drop_db(self) {
        self.pool.close().await;
        let mut conn = PgConnection::connect_with(&self.admin).await.unwrap();
        // FORCE cannot terminate an autovacuum worker that has just started in the database
        // ("permission denied to terminate process"); it is gone a moment later.
        for attempt in 1.. {
            let dropped = conn
                .execute(AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    self.name
                )))
                .await;
            match dropped {
                Ok(_) => break,
                Err(e) if attempt < 20 => {
                    eprintln!("drop {} failed, retrying: {e}", self.name);
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(e) => panic!("drop {}: {e}", self.name),
            }
        }
    }

    /// One `fetch` run through the library, exactly as the binary does it after reading the
    /// configuration. Panics if another run held the lock.
    pub async fn fetch(&self, site: &Site, options: FetchOptions) -> RunReport {
        self.try_fetch(site, options)
            .await
            .unwrap()
            .expect("run lock was free")
    }

    pub async fn try_fetch(
        &self,
        site: &Site,
        options: FetchOptions,
    ) -> anyhow::Result<Option<RunReport>> {
        let config = site.config();
        let feeds = site.feeds()?;
        fetch::run(&self.options, &self.pool, &config, &feeds, &options).await
    }

    pub async fn fetch_due(&self, site: &Site) -> RunReport {
        self.fetch(site, FetchOptions::default()).await
    }

    pub async fn fetch_forced(&self, site: &Site) -> RunReport {
        self.fetch(
            site,
            FetchOptions {
                force: true,
                ..FetchOptions::default()
            },
        )
        .await
    }

    pub async fn sync(&self, site: &Site) -> anyhow::Result<SyncReport> {
        let feeds = site.feeds()?;
        fetch::sync_command(&self.options, &self.pool, &feeds).await
    }

    pub async fn scalar_i64(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    pub async fn feed_id(&self, xml_url: &str) -> i32 {
        sqlx::query_scalar("SELECT feed_id FROM feeds.feeds WHERE xml_url = $1")
            .bind(xml_url)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Everything about the feeds and entries tables that a write would change, including
    /// each row's xmin (so that even a no-op UPDATE shows).
    pub async fn snapshot(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT 'feed ' || xmin::text || ' ' || row_to_json(f)::text FROM feeds.feeds f
             UNION ALL
             SELECT 'entry ' || xmin::text || ' ' || row_to_json(e)::text FROM feeds.entries e
             ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    pub async fn entries_xmin(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT entry_id::text || ':' || xmin::text FROM feeds.entries ORDER BY entry_id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Run the real binary against this database, configured the way a deployment is: libpq
    /// environment only, password (if any) through a 0600 PGPASSFILE. Runs on a blocking
    /// thread so that the in-process HTTP server keeps answering.
    pub async fn run_binary(&self, site: &Site, args: &[&str], rust_log: &str) -> Output {
        let config_path = site.write_config();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_feed-index"));
        cmd.arg("--config").arg(&config_path).args(args);
        for k in [
            "PGPASSWORD",
            "PGPASSFILE",
            "PGSERVICE",
            "DATABASE_URL",
            "PGSSLMODE",
        ] {
            cmd.env_remove(k);
        }
        cmd.env("PGHOST", self.options.get_host())
            .env("PGPORT", self.options.get_port().to_string())
            .env("PGUSER", self.options.get_username())
            .env("PGDATABASE", &self.name)
            .env("PGSSLMODE", "prefer")
            .env("RUST_LOG", rust_log);
        if let Some(pw) = &self.password {
            let pgpass = site.dir.path().join("pgpass");
            std::fs::write(
                &pgpass,
                format!("*:*:*:*:{}\n", pw.replace('\\', "\\\\").replace(':', "\\:")),
            )
            .unwrap();
            std::fs::set_permissions(&pgpass, std::fs::Permissions::from_mode(0o600)).unwrap();
            cmd.env("PGPASSFILE", pgpass);
        }
        tokio::task::spawn_blocking(move || cmd.output().expect("run feed-index"))
            .await
            .unwrap()
    }
}

/// A local feed server plus the configuration and OPML file that point at it.
pub struct Site {
    pub dir: tempfile::TempDir,
    pub server: MockServer,
    pub max_feed_bytes: u64,
    pub default_poll_minutes: i32,
}

impl Site {
    pub async fn new() -> Site {
        let site = Site {
            dir: tempfile::tempdir().unwrap(),
            server: MockServer::start().await,
            max_feed_bytes: 1 << 20,
            default_poll_minutes: 60,
        };
        site.write_opml("");
        site
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.uri())
    }

    /// Serve `body` at `path` with 200 and the given content type.
    pub async fn serve(&self, path_: &str, body: impl Into<Vec<u8>>, content_type: &str) {
        Mock::given(method("GET"))
            .and(path(path_))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body.into(), content_type))
            .mount(&self.server)
            .await;
    }

    pub async fn serve_fixture(&self, path_: &str, name: &str) {
        let ct = if name.ends_with(".json") {
            "application/feed+json"
        } else if name.ends_with(".html") {
            "text/html; charset=utf-8"
        } else {
            "application/xml"
        };
        self.serve(path_, fixture(name), ct).await;
    }

    pub async fn respond(&self, path_: &str, template: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(path_))
            .respond_with(template)
            .mount(&self.server)
            .await;
    }

    pub async fn reset(&self) {
        self.server.reset().await;
    }

    /// Requests received for `path`, oldest first.
    pub async fn requests(&self, path_: &str) -> Vec<wiremock::Request> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == path_)
            .collect()
    }

    pub fn opml_path(&self) -> PathBuf {
        self.dir.path().join("feeds.opml")
    }

    /// Replace the feed list with these outlines (raw OPML body content).
    pub fn write_opml(&self, outlines: &str) {
        std::fs::write(
            self.opml_path(),
            format!(
                "<?xml version=\"1.0\"?>\n<opml version=\"2.0\"><head><title>t</title></head><body>\n{outlines}\n</body></opml>\n"
            ),
        )
        .unwrap();
    }

    /// Replace the feed list with one top-level outline per path on this server.
    pub fn set_feeds(&self, paths: &[&str]) {
        let outlines: String = paths
            .iter()
            .map(|p| format!("<outline text=\"Feed {p}\" xmlUrl=\"{}\"/>\n", self.url(p)))
            .collect();
        self.write_opml(&outlines);
    }

    pub fn feeds(&self) -> anyhow::Result<Vec<OpmlFeed>> {
        opml::load(&self.opml_path(), self.default_poll_minutes)
    }

    pub fn config_text(&self) -> String {
        format!(
            "opml = {:?}\ncontact = \"https://example.org/feed-index-tests\"\ndefault_poll_minutes = {}\nconcurrency = 3\nmax_feed_bytes = {}\ntimeout_seconds = 10\nfetch_history_days = 30\n",
            self.opml_path().to_str().unwrap(),
            self.default_poll_minutes,
            self.max_feed_bytes
        )
    }

    pub fn config(&self) -> Config {
        Config::parse(&self.config_text()).unwrap()
    }

    pub fn write_config(&self) -> PathBuf {
        let p = self.dir.path().join("config.toml");
        std::fs::write(&p, self.config_text()).unwrap();
        p
    }
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap()
}

pub fn fixture_text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap()
}

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn force() -> FetchOptions {
    FetchOptions {
        force: true,
        ..FetchOptions::default()
    }
}
