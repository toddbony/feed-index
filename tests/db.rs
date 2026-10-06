//! Migration, sync and fetch tests against a local PostgreSQL (`DATABASE_URL`) and a local
//! in-process HTTP server. Case numbers follow the brief (CLAUDE.md, "Tests").

mod common;

use std::io::{Read, Write};
use std::time::Duration;

use chrono::{DateTime, Utc};
use common::*;
use feed_index::fetch::FetchOptions;
use feed_index::sync::RetireGuard;
use sqlx::{Connection, PgConnection};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Debug, sqlx::FromRow)]
struct EntryDb {
    entry_key: String,
    key_source: String,
    link: Option<String>,
    title: Option<String>,
    author: Option<String>,
    published_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    summary_html: Option<String>,
    content_html: Option<String>,
    body_text: Option<String>,
    body_source: String,
    content_sha256: Vec<u8>,
    revisions: i32,
    backfill: bool,
    first_seen_at: DateTime<Utc>,
    last_changed_at: Option<DateTime<Utc>>,
    fetcher_version: String,
}

async fn entries(db: &TestDb, feed_id: i32) -> Vec<EntryDb> {
    sqlx::query_as("SELECT * FROM feeds.entries WHERE feed_id = $1 ORDER BY entry_id")
        .bind(feed_id)
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct FeedDb {
    feed_id: i32,
    xml_url: String,
    title: String,
    folder: Option<String>,
    html_url: Option<String>,
    poll_minutes: i32,
    weight: i32,
    retired_at: Option<DateTime<Utc>>,
    declared_title: Option<String>,
    resolved_url: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    next_fetch_at: DateTime<Utc>,
    last_ok_at: Option<DateTime<Utc>>,
    last_new_entry_at: Option<DateTime<Utc>>,
    consecutive_failures: i32,
    last_error: Option<String>,
}

async fn feed(db: &TestDb, xml_url: &str) -> FeedDb {
    sqlx::query_as("SELECT * FROM feeds.feeds WHERE xml_url = $1")
        .bind(xml_url)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

/// Seconds from the last attempt to the next scheduled fetch.
async fn next_delay(db: &TestDb, xml_url: &str) -> f64 {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM next_fetch_at - last_attempt_at)::float8
         FROM feeds.feeds WHERE xml_url = $1",
    )
    .bind(xml_url)
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct FetchDb {
    outcome: String,
    http_status: Option<i16>,
    bytes: Option<i32>,
    entries_in_doc: Option<i32>,
    new_entries: i32,
    changed_entries: i32,
    error: Option<String>,
}

async fn fetches(db: &TestDb, feed_id: i32) -> Vec<FetchDb> {
    sqlx::query_as("SELECT * FROM feeds.fetches WHERE feed_id = $1 ORDER BY fetch_id")
        .bind(feed_id)
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

/// One feed at `path`, serving `fixture`, fetched once. Returns its xml_url and id.
async fn one_feed(db: &TestDb, site: &Site, path: &str, fixture: &str) -> (String, i32) {
    site.set_feeds(&[path]);
    site.serve_fixture(path, fixture).await;
    let r = db.fetch_due(site).await;
    assert_eq!((r.due, r.ok), (1, 1), "{fixture}: {:?}", r.summary_lines());
    let url = site.url(path);
    let id = db.feed_id(&url).await;
    (url, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case01_migration_coexistence() {
    for ours_first in [false, true] {
        let Some(db) = test_db_unmigrated().await else {
            return;
        };
        let standin = sqlx::migrate!("./tests/standin-migrations");
        for _ in 0..2 {
            if ours_first {
                feed_index::db::migrate(&db.pool).await.unwrap();
                standin.run(&db.pool).await.unwrap();
            } else {
                standin.run(&db.pool).await.unwrap();
                feed_index::db::migrate(&db.pool).await.unwrap();
            }
        }
        let ours: Vec<(i64, String)> = sqlx::query_as(
            "SELECT version, description FROM feeds._sqlx_migrations ORDER BY version",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(ours, [(1, "init".to_string())]);
        let theirs: Vec<(i64, String)> = sqlx::query_as(
            "SELECT version, description FROM public._sqlx_migrations ORDER BY version",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(theirs, [(1, "standin".to_string())]);
        // The binary's migrator agrees.
        let site = Site::new().await;
        let out = db.run_binary(&site, &["migrate"], "info").await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        db.drop_db().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case02_opml_folders_defaults_and_errors() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (a, b, c) = (site.url("/a.xml"), site.url("/b.xml"), site.url("/c.xml"));
    site.write_opml(&format!(
        r#"<outline text="Top level" xmlUrl="{a}"/>
        <outline text="News" title="ignored">
          <outline text="Sub">
            <outline title="By title" text="by text" xmlUrl="{b}" htmlUrl="https://b.example.org/" poll_minutes="10" weight="7"/>
          </outline>
          <outline xmlUrl="{c}"/>
        </outline>"#
    ));
    let r = db.sync(&site).await.unwrap();
    assert_eq!((r.added, r.updated, r.retired), (3, 0, 0));
    let fa = feed(&db, &a).await;
    assert_eq!(
        (
            fa.title.as_str(),
            fa.folder.as_deref(),
            fa.poll_minutes,
            fa.weight
        ),
        ("Top level", None, 60, 0)
    );
    assert_eq!(fa.html_url, None);
    let fb = feed(&db, &b).await;
    assert_eq!(
        (
            fb.title.as_str(),
            fb.folder.as_deref(),
            fb.poll_minutes,
            fb.weight
        ),
        ("By title", Some("Sub"), 10, 7)
    );
    assert_eq!(fb.html_url.as_deref(), Some("https://b.example.org/"));
    let fc = feed(&db, &c).await;
    assert_eq!(fc.title, "127.0.0.1", "falls back to the URL's host");
    assert_eq!(fc.folder.as_deref(), Some("News"));

    // Each configuration error stops the run before any change.
    let before = db.snapshot().await;
    let runs = db.scalar_i64("SELECT count(*) FROM feeds.fetch_runs").await;
    for bad in [
        format!(r#"<outline xmlUrl="{a}"/><outline text="x"><outline xmlUrl="{a}"/></outline>"#),
        format!(r#"<outline xmlUrl="{a}"/><outline xmlUrl="{b}" poll_minutes="2"/>"#),
        format!(r#"<outline xmlUrl="{a}"/><outline xmlUrl="file:///etc/passwd"/>"#),
    ] {
        site.write_opml(&bad);
        assert!(site.feeds().is_err());
        for cmd in ["sync", "fetch"] {
            let out = db.run_binary(&site, &[cmd], "info").await;
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{cmd}: {stderr}");
            assert!(stderr.contains("line "), "{cmd}: {stderr}");
        }
    }
    assert_eq!(db.snapshot().await, before, "database unchanged");
    assert_eq!(
        db.scalar_i64("SELECT count(*) FROM feeds.fetch_runs").await,
        runs
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case03_sync_retire_readd_update() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/a.xml", "/b.xml"]);
    let r = db.sync(&site).await.unwrap();
    assert_eq!((r.added, r.updated, r.retired, r.unchanged), (2, 0, 0, 0));
    let (a, b) = (site.url("/a.xml"), site.url("/b.xml"));
    assert!(
        feed(&db, &a).await.next_fetch_at <= Utc::now(),
        "new feed due at once"
    );

    let r = db.sync(&site).await.unwrap();
    assert_eq!((r.added, r.updated, r.unchanged), (0, 0, 2), "no-op sync");

    site.set_feeds(&["/a.xml"]);
    let r = db.sync(&site).await.unwrap();
    assert_eq!(r.retired, 1);
    assert!(feed(&db, &b).await.retired_at.is_some());
    assert!(feed(&db, &a).await.retired_at.is_none());

    site.set_feeds(&["/a.xml", "/b.xml"]);
    let r = db.sync(&site).await.unwrap();
    assert_eq!((r.added, r.updated, r.retired), (0, 1, 0));
    assert!(feed(&db, &b).await.retired_at.is_none(), "re-added");

    // Title and poll_minutes change; next_fetch_at does not move.
    sqlx::query("UPDATE feeds.feeds SET next_fetch_at = now() + interval '5 hours'")
        .execute(&db.pool)
        .await
        .unwrap();
    let next = feed(&db, &a).await.next_fetch_at;
    site.write_opml(&format!(
        r#"<outline text="Renamed" xmlUrl="{a}" poll_minutes="15"/><outline text="Feed /b.xml" xmlUrl="{b}"/>"#
    ));
    let r = db.sync(&site).await.unwrap();
    assert_eq!((r.updated, r.unchanged), (1, 1));
    let fa = feed(&db, &a).await;
    assert_eq!((fa.title.as_str(), fa.poll_minutes), ("Renamed", 15));
    assert_eq!(fa.next_fetch_at, next);

    // The sync command prints its counts.
    let out = db.run_binary(&site, &["sync"], "info").await;
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "feed-index sync added=0 updated=0 retired=0 unchanged=2"
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case04_retire_guard() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/1.xml", "/2.xml", "/3.xml", "/4.xml", "/5.xml"]);
    db.sync(&site).await.unwrap();
    let before = db.snapshot().await;
    let active = "SELECT count(*) FROM feeds.feeds WHERE retired_at IS NULL";

    site.write_opml("");
    let e = db.sync(&site).await.unwrap_err();
    assert!(e.downcast_ref::<RetireGuard>().is_some(), "{e:#}");
    // Two of five is fewer than half.
    site.set_feeds(&["/1.xml", "/9.xml"]);
    let e = db.sync(&site).await.unwrap_err();
    assert!(e.downcast_ref::<RetireGuard>().is_some(), "{e:#}");
    assert_eq!(db.scalar_i64(active).await, 5);
    assert_eq!(
        db.snapshot().await,
        before,
        "nothing retired, added or updated"
    );

    for opml in ["", "two"] {
        if opml.is_empty() {
            site.write_opml("");
        }
        let out = db.run_binary(&site, &["fetch"], "info").await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "non-zero exit");
        assert!(stderr.contains("retire guard"), "{stderr}");
        let out = db.run_binary(&site, &["sync"], "info").await;
        assert!(!out.status.success());
        site.set_feeds(&["/1.xml", "/9.xml"]);
    }
    assert_eq!(db.scalar_i64(active).await, 5);
    assert_eq!(
        db.scalar_i64("SELECT count(*) FROM feeds.fetch_runs").await,
        0
    );

    // Three of five passes.
    site.set_feeds(&["/1.xml", "/2.xml", "/3.xml"]);
    assert_eq!(db.sync(&site).await.unwrap().retired, 2);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case05_formats_are_stored() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let files = [
        ("/rss2", "rss2.xml"),
        ("/atom", "atom.xml"),
        ("/rss1", "rss1.rdf"),
        ("/json", "feed.json"),
    ];
    site.set_feeds(&files.map(|f| f.0));
    for (p, f) in files {
        site.serve_fixture(p, f).await;
    }
    let r = db.fetch_due(&site).await;
    assert_eq!((r.due, r.ok, r.failed, r.new_entries), (4, 4, 0, 8));

    let rss2 = db.feed_id(&site.url("/rss2")).await;
    let e = entries(&db, rss2).await;
    assert_eq!(e.len(), 2);
    assert_eq!(
        (e[0].entry_key.as_str(), e[0].key_source.as_str()),
        ("news-1", "id")
    );
    assert_eq!(e[0].title.as_deref(), Some("First   story"));
    assert_eq!(e[0].author.as_deref(), Some("Ada Example"));
    assert!(e[0].published_at.is_some());
    assert_eq!(e[0].updated_at, None, "RSS 2.0 declares no update time");
    assert_eq!(
        e[0].summary_html.as_deref(),
        Some("<p>Short <b>summary</b> one.</p>")
    );
    assert!(e[0].content_html.as_deref().unwrap().contains("<a href="));
    assert_eq!(e[0].body_source, "content");
    assert!(
        e[0].body_text
            .as_deref()
            .unwrap()
            .starts_with("Full text of the first[1] story.")
    );
    assert_eq!(e[0].content_sha256.len(), 32);
    assert_eq!(e[0].fetcher_version, feed_index::FETCHER_VERSION);
    assert_eq!(e[1].body_source, "summary");
    let f = feed(&db, &site.url("/rss2")).await;
    assert_eq!(f.declared_title.as_deref(), Some("Example Wire"));
    assert_eq!(f.title, "Feed /rss2", "the configured title is kept");

    let atom = entries(&db, db.feed_id(&site.url("/atom")).await).await;
    assert_eq!(atom[0].entry_key, "tag:writer.example.com,2026:one");
    assert_eq!(
        atom[0].link.as_deref(),
        Some("https://writer.example.com/one")
    );
    assert!(atom[0].updated_at.is_some());
    assert_eq!(atom[0].body_text.as_deref(), Some("Atom content body."));

    let rss1 = entries(&db, db.feed_id(&site.url("/rss1")).await).await;
    assert_eq!(rss1.len(), 2);
    assert!(
        rss1.iter()
            .all(|e| e.key_source == "fingerprint" && e.entry_key.starts_with("fp:"))
    );
    assert_eq!(rss1[1].body_source, "none");
    assert_eq!(rss1[1].body_text, None);

    let json = entries(&db, db.feed_id(&site.url("/json")).await).await;
    assert_eq!(json[0].entry_key, "json-1");
    assert_eq!(json[1].body_text.as_deref(), Some("Plain text\n\ncontent."));
    assert_eq!(
        db.scalar_i64("SELECT count(*) FROM feeds.entries WHERE NOT backfill")
            .await,
        0
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case06_fingerprint_keys_survive_utm_and_fragments() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (_, id) = one_feed(&db, &site, "/plain.xml", "rss-noguid.xml").await;
    let first: Vec<String> = entries(&db, id)
        .await
        .into_iter()
        .map(|e| e.entry_key)
        .collect();
    assert_eq!(first.len(), 2);
    site.reset().await;
    site.serve_fixture("/plain.xml", "rss-noguid-utm.xml").await;
    let r = db.fetch_forced(&site).await;
    assert_eq!(r.new_entries, 0, "same keys");
    let e = entries(&db, id).await;
    assert_eq!(
        e.iter().map(|e| e.entry_key.clone()).collect::<Vec<_>>(),
        first
    );
    assert!(e.iter().all(|e| e.key_source == "fingerprint"));
    // The stored link is what the feed now says, so the entry counts as edited.
    assert!(e[0].link.as_deref().unwrap().contains("utm_source"));
    assert_eq!(r.changed_entries, 2);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case07_idempotency() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (_, id) = one_feed(&db, &site, "/f.xml", "rss2.xml").await;
    let before = db.entries_xmin().await;
    let r = db.fetch_forced(&site).await;
    assert_eq!((r.ok, r.new_entries, r.changed_entries), (1, 0, 0));
    assert_eq!(db.entries_xmin().await, before, "no entry row written");
    let f = fetches(&db, id).await;
    assert_eq!(f.len(), 2);
    assert_eq!((f[1].new_entries, f[1].changed_entries), (0, 0));
    assert_eq!(f[1].entries_in_doc, Some(2));
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case08_edit_counts_a_revision() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (_, id) = one_feed(&db, &site, "/f.xml", "rss2.xml").await;
    let before = entries(&db, id).await;
    site.reset().await;
    site.serve(
        "/f.xml",
        fixture_text("rss2.xml").replace("First   story", "First story (corrected)"),
        "application/rss+xml",
    )
    .await;
    let r = db.fetch_forced(&site).await;
    assert_eq!((r.new_entries, r.changed_entries), (0, 1));
    let after = entries(&db, id).await;
    assert_eq!(after.len(), 2, "one row per entry");
    assert_eq!(after[0].title.as_deref(), Some("First story (corrected)"));
    assert_eq!(after[0].revisions, 1);
    assert!(after[0].last_changed_at.is_some());
    assert_ne!(after[0].content_sha256, before[0].content_sha256);
    assert_eq!(after[0].first_seen_at, before[0].first_seen_at);
    assert!(after[0].backfill, "backfill never changes");
    assert_eq!(after[1].revisions, 0);
    assert!(after[1].last_changed_at.is_none());
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case09_backfill_only_on_first_success() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    // A failed first attempt does not use up the backfill.
    site.set_feeds(&["/f.xml"]);
    site.respond("/f.xml", ResponseTemplate::new(500)).await;
    assert_eq!(db.fetch_due(&site).await.failed, 1);
    site.reset().await;
    site.serve_fixture("/f.xml", "rss2.xml").await;
    db.fetch_forced(&site).await;
    let url = site.url("/f.xml");
    let id = db.feed_id(&url).await;
    assert!(entries(&db, id).await.iter().all(|e| e.backfill));
    let first_new = feed(&db, &url).await.last_new_entry_at.unwrap();

    site.reset().await;
    let more = fixture_text("rss2.xml").replacen(
        "<item>",
        "<item><title>Breaking</title><link>https://news.example.org/3</link><guid>news-3</guid></item><item>",
        1,
    );
    site.serve("/f.xml", more, "application/rss+xml").await;
    let r = db.fetch_forced(&site).await;
    assert_eq!(r.new_entries, 1);
    let e = entries(&db, id).await;
    let new = e.iter().find(|e| e.entry_key == "news-3").unwrap();
    assert!(!new.backfill);
    assert_eq!(e.iter().filter(|e| e.backfill).count(), 2);
    assert!(feed(&db, &url).await.last_new_entry_at.unwrap() > first_new);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case10_conditional_get() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/c.xml"]);
    Mock::given(method("GET"))
        .and(path("/c.xml"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&site.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/c.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("rss2.xml"), "application/rss+xml")
                .insert_header("ETag", "\"v1\"")
                .insert_header("Last-Modified", "Mon, 05 Oct 2026 10:00:00 GMT"),
        )
        .with_priority(5)
        .mount(&site.server)
        .await;
    let r = db.fetch_due(&site).await;
    assert_eq!(r.ok, 1);
    let url = site.url("/c.xml");
    let f = feed(&db, &url).await;
    assert_eq!(f.etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        f.last_modified.as_deref(),
        Some("Mon, 05 Oct 2026 10:00:00 GMT")
    );
    let entries_before = db.entries_xmin().await;

    let r = db.fetch_forced(&site).await;
    assert_eq!((r.ok, r.not_modified, r.failed), (0, 1, 0));
    let reqs = site.requests("/c.xml").await;
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].headers.get("if-none-match").is_none());
    assert_eq!(reqs[1].headers.get("if-none-match").unwrap(), "\"v1\"");
    assert_eq!(
        reqs[1].headers.get("if-modified-since").unwrap(),
        "Mon, 05 Oct 2026 10:00:00 GMT"
    );
    let ua = reqs[0].headers.get("user-agent").unwrap().to_str().unwrap();
    assert_eq!(
        ua,
        format!(
            "feed-index/{} (+https://example.org/feed-index-tests)",
            feed_index::FETCHER_VERSION
        )
    );
    assert!(
        reqs[0]
            .headers
            .get("accept")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/rss+xml, application/atom+xml, application/feed+json")
    );
    assert!(
        reqs[0].headers.get("accept-encoding").is_some(),
        "compression offered"
    );

    let f2 = feed(&db, &url).await;
    assert_eq!(f2.etag.as_deref(), Some("\"v1\""), "ETag kept");
    assert_eq!(f2.last_modified, f.last_modified);
    assert!(f2.last_ok_at > f.last_ok_at);
    assert_eq!(db.entries_xmin().await, entries_before, "no entry writes");
    let h = fetches(&db, f.feed_id).await;
    assert_eq!(h[1].outcome, "not_modified");
    assert_eq!(h[1].http_status, Some(304));
    assert_eq!(h[1].entries_in_doc, None);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case11_failure_backoff_and_reset() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/gone.xml"]);
    site.respond("/gone.xml", ResponseTemplate::new(404)).await;
    let url = site.url("/gone.xml");

    let r = db.fetch_due(&site).await;
    assert_eq!(r.failed, 1);
    let f = feed(&db, &url).await;
    assert_eq!(f.consecutive_failures, 1);
    assert_eq!(f.last_error.as_deref(), Some("HTTP 404"));
    assert!(f.last_ok_at.is_none());
    assert_eq!(next_delay(&db, &url).await, 3600.0, "one interval");
    let h = fetches(&db, f.feed_id).await;
    assert_eq!(
        (h[0].outcome.as_str(), h[0].http_status),
        ("http_error", Some(404))
    );
    assert_eq!(
        r.summary_lines()[1],
        format!(
            "feed-index feed_id={} host=127.0.0.1 outcome=http_error status=404",
            f.feed_id
        )
    );

    db.fetch_forced(&site).await;
    assert_eq!(feed(&db, &url).await.consecutive_failures, 2);
    assert_eq!(next_delay(&db, &url).await, 7200.0, "doubled");
    db.fetch_forced(&site).await;
    assert_eq!(next_delay(&db, &url).await, 4.0 * 3600.0);

    site.reset().await;
    site.serve_fixture("/gone.xml", "atom.xml").await;
    let r = db.fetch_forced(&site).await;
    assert_eq!(r.ok, 1);
    let f = feed(&db, &url).await;
    assert_eq!(f.consecutive_failures, 0);
    assert_eq!(f.last_error, None);
    let d = next_delay(&db, &url).await;
    assert!(
        (3600.0..=3960.0).contains(&d),
        "poll interval plus 0-10% jitter: {d}"
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case12_retry_after() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (busy, down) = (site.url("/busy.xml"), site.url("/down.xml"));
    site.write_opml(&format!(
        r#"<outline xmlUrl="{busy}" poll_minutes="10"/><outline xmlUrl="{down}" poll_minutes="10"/>"#
    ));
    site.respond(
        "/busy.xml",
        ResponseTemplate::new(429).insert_header("Retry-After", "3600"),
    )
    .await;
    let in_two_days = (Utc::now() + chrono::Duration::days(2))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    site.respond(
        "/down.xml",
        ResponseTemplate::new(503).insert_header("Retry-After", in_two_days.as_str()),
    )
    .await;
    let r = db.fetch_due(&site).await;
    assert_eq!(r.failed, 2);
    let d = next_delay(&db, &busy).await;
    assert!(
        (3590.0..=3610.0).contains(&d),
        "about an hour, not 10 minutes: {d}"
    );
    let d = next_delay(&db, &down).await;
    assert!(
        (86390.0..=86400.0).contains(&d),
        "HTTP date, capped at 24 h: {d}"
    );
    assert_eq!(
        feed(&db, &busy).await.last_error.as_deref(),
        Some("HTTP 429")
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case13_body_over_the_cap() {
    let Some(db) = test_db().await else { return };
    let mut site = Site::new().await;
    site.max_feed_bytes = 4096;
    let item = "<item><title>t</title><guid>g{}</guid><description>filler filler filler filler</description></item>";
    let items: String = (0..200)
        .map(|i| item.replace("{}", &i.to_string()))
        .collect();
    let big = format!("<rss version=\"2.0\"><channel><title>Big</title>{items}</channel></rss>");
    assert!(big.len() > 10_000);
    // Plain (refused on Content-Length) and gzip (compressed size under the cap, so refused
    // while streaming the decompressed body).
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(big.as_bytes()).unwrap();
    let gz = gz.finish().unwrap();
    assert!(gz.len() < 4096, "{}", gz.len());
    site.set_feeds(&["/big.xml", "/big.xml.gz"]);
    site.serve("/big.xml", big.clone(), "application/rss+xml")
        .await;
    site.respond(
        "/big.xml.gz",
        ResponseTemplate::new(200)
            .set_body_raw(gz, "application/rss+xml")
            .insert_header("Content-Encoding", "gzip"),
    )
    .await;
    let r = db.fetch_due(&site).await;
    assert_eq!((r.ok, r.failed), (0, 2), "{:?}", r.summary_lines());
    assert_eq!(db.scalar_i64("SELECT count(*) FROM feeds.entries").await, 0);
    for p in ["/big.xml", "/big.xml.gz"] {
        let f = feed(&db, &site.url(p)).await;
        let h = fetches(&db, f.feed_id).await;
        assert_eq!(h[0].outcome, "too_large", "{p}");
        assert_eq!(h[0].entries_in_doc, None);
        assert!(h[0].bytes.unwrap() > 4096, "{p}");
        assert_eq!(f.consecutive_failures, 1);
        assert!(f.declared_title.is_none());
    }
    // Small compressed feeds decompress fine.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&fixture("rss2.xml")).unwrap();
    site.reset().await;
    site.respond(
        "/big.xml.gz",
        ResponseTemplate::new(200)
            .set_body_raw(gz.finish().unwrap(), "application/rss+xml")
            .insert_header("Content-Encoding", "gzip"),
    )
    .await;
    let r = db
        .fetch(
            &site,
            FetchOptions {
                force: true,
                feed_ids: vec![db.feed_id(&site.url("/big.xml.gz")).await],
            },
        )
        .await;
    assert_eq!((r.due, r.ok, r.new_entries), (1, 1, 2));
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case14_html_instead_of_a_feed() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/moved"]);
    site.serve_fixture("/moved", "html-page.html").await;
    let r = db.fetch_due(&site).await;
    assert_eq!(r.failed, 1);
    let f = feed(&db, &site.url("/moved")).await;
    let h = fetches(&db, f.feed_id).await;
    assert_eq!(h[0].outcome, "parse_error");
    assert_eq!(h[0].http_status, Some(200));
    let err = h[0].error.as_deref().unwrap();
    assert_eq!(err, "got text/html, not a feed");
    assert!(!err.contains("moved"), "never the body");
    assert_eq!(f.last_error.as_deref(), Some(err));
    assert_eq!(f.consecutive_failures, 1);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case15_redirects() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/old.xml", "/loop", "/to-file"]);
    site.respond(
        "/old.xml",
        ResponseTemplate::new(301).insert_header("Location", "/new.xml"),
    )
    .await;
    site.serve_fixture("/new.xml", "rss2.xml").await;
    site.respond(
        "/loop",
        ResponseTemplate::new(302).insert_header("Location", "/loop"),
    )
    .await;
    site.respond(
        "/to-file",
        ResponseTemplate::new(302).insert_header("Location", "file:///etc/passwd"),
    )
    .await;
    let r = db.fetch_due(&site).await;
    assert_eq!((r.ok, r.failed), (1, 2));
    let old = site.url("/old.xml");
    let f = feed(&db, &old).await;
    assert_eq!(f.xml_url, old, "xml_url never changes");
    assert_eq!(
        f.resolved_url.as_deref(),
        Some(site.url("/new.xml").as_str())
    );
    assert_eq!(entries(&db, f.feed_id).await.len(), 2);

    let looped = feed(&db, &site.url("/loop")).await;
    assert_eq!(
        looped.last_error.as_deref(),
        Some("redirect refused: too many redirects")
    );
    assert_eq!(
        site.requests("/loop").await.len(),
        6,
        "the request plus 5 redirects"
    );
    assert_eq!(
        fetches(&db, looped.feed_id).await[0].outcome,
        "network_error"
    );
    // A non-http(s) Location is never followed; the 302 itself is the answer.
    let file = feed(&db, &site.url("/to-file")).await;
    assert_eq!(
        file.last_error.as_deref(),
        Some("HTTP 302 (redirect not followed)")
    );
    let h = fetches(&db, file.feed_id).await;
    assert_eq!(
        (h[0].outcome.as_str(), h[0].http_status),
        ("http_error", Some(302))
    );

    // Without the redirect, resolved_url is cleared on the next success.
    site.reset().await;
    site.serve_fixture("/old.xml", "rss2.xml").await;
    db.fetch(
        &site,
        FetchOptions {
            force: true,
            feed_ids: vec![f.feed_id],
        },
    )
    .await;
    assert_eq!(feed(&db, &old).await.resolved_url, None);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case16_nul_bytes_and_renderer_panic() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/nul.xml", "/nul.json", "/table.xml"]);
    site.serve_fixture("/nul.xml", "nul.xml").await;
    site.serve_fixture("/nul.json", "nul.json").await;
    site.serve_fixture("/table.xml", "panic-table.xml").await;
    let r = db.fetch_due(&site).await;
    assert_eq!(
        (r.ok, r.failed, r.new_entries),
        (3, 0, 3),
        "{:?}",
        r.summary_lines()
    );

    let nul = entries(&db, db.feed_id(&site.url("/nul.xml")).await).await;
    assert_eq!(nul[0].title.as_deref(), Some("Titlewith nul"));
    assert_eq!(nul[0].content_html.as_deref(), Some("<p>Bodywith nul</p>"));
    assert_eq!(nul[0].body_text.as_deref(), Some("Bodywith nul"));
    let json = entries(&db, db.feed_id(&site.url("/nul.json")).await).await;
    assert_eq!(json[0].entry_key, "nul-json");
    assert_eq!(json[0].title.as_deref(), Some("Jsontitle"));
    assert_eq!(
        feed(&db, &site.url("/nul.xml"))
            .await
            .declared_title
            .as_deref(),
        Some("Nuls")
    );

    let table = entries(&db, db.feed_id(&site.url("/table.xml")).await).await;
    assert_eq!(table.len(), 1, "stored despite the renderer panic");
    assert_eq!(table[0].body_source, "none");
    assert_eq!(table[0].body_text, None);
    assert!(
        table[0]
            .content_html
            .as_deref()
            .unwrap()
            .contains("rowspan")
    );
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_links_and_ids_do_not_wedge_a_feed() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    // Incompressible, far over the ~2.7 kB btree row limit.
    let long: String = (0..400)
        .map(|i| format!("{:016x}", (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)))
        .collect();
    let doc = format!(
        "<rss version=\"2.0\"><channel><title>Long</title>\
         <item><title>Long link</title><link>https://long.example.org/{long}</link><guid>short</guid></item>\
         <item><title>Long id</title><link>https://long.example.org/b</link><guid>{long}</guid></item>\
         </channel></rss>"
    );
    site.set_feeds(&["/long.xml"]);
    site.serve("/long.xml", doc, "application/rss+xml").await;
    let r = db.fetch_due(&site).await;
    assert_eq!(
        (r.ok, r.new_entries, r.errors),
        (1, 2, 0),
        "{:?}",
        r.summary_lines()
    );
    let e = entries(&db, db.feed_id(&site.url("/long.xml")).await).await;
    assert_eq!(e[0].link.as_deref().unwrap().len(), 25 + long.len());
    assert_eq!(
        e[1].key_source, "fingerprint",
        "an over-long id is not a key"
    );
    assert_eq!(
        e[1].entry_key,
        feed_index::entry::fingerprint("https://long.example.org/b", "Long id")
    );
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM feeds.entries WHERE link = $1")
        .bind(e[0].link.as_deref())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(linked, 1);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case17_duplicate_guid_in_one_document() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (_, id) = one_feed(&db, &site, "/dup.xml", "dup-guid.xml").await;
    let e = entries(&db, id).await;
    assert_eq!(
        e.iter()
            .map(|e| e.title.clone().unwrap())
            .collect::<Vec<_>>(),
        ["Kept", "Other"]
    );
    let h = fetches(&db, id).await;
    assert_eq!((h[0].entries_in_doc, h[0].new_entries), (Some(3), 2));
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case18_due_selection_force_and_retired() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/a.xml", "/b.xml"]);
    site.serve_fixture("/a.xml", "rss2.xml").await;
    site.serve_fixture("/b.xml", "atom.xml").await;
    assert_eq!(db.fetch_due(&site).await.due, 2);
    let count = |p: &'static str| {
        let site = &site;
        async move { site.requests(p).await.len() }
    };

    let r = db.fetch_due(&site).await;
    assert_eq!((r.due, r.ok), (0, 0), "not yet due");
    assert_eq!((count("/a.xml").await, count("/b.xml").await), (1, 1));

    let r = db.fetch_forced(&site).await;
    assert_eq!((r.due, r.ok), (2, 2));

    let b = db.feed_id(&site.url("/b.xml")).await;
    let r = db
        .fetch(
            &site,
            FetchOptions {
                force: true,
                feed_ids: vec![b, 9999],
            },
        )
        .await;
    assert_eq!(r.due, 1);
    assert_eq!((count("/a.xml").await, count("/b.xml").await), (2, 3));

    // Due again once next_fetch_at has passed.
    sqlx::query(
        "UPDATE feeds.feeds SET next_fetch_at = now() - interval '1 minute' WHERE feed_id = $1",
    )
    .bind(b)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(db.fetch_due(&site).await.due, 1);

    // Retired: never fetched, not even forced or named.
    site.set_feeds(&["/a.xml"]);
    sqlx::query("UPDATE feeds.feeds SET next_fetch_at = now() - interval '1 day'")
        .execute(&db.pool)
        .await
        .unwrap();
    let r = db
        .fetch(
            &site,
            FetchOptions {
                force: true,
                feed_ids: vec![],
            },
        )
        .await;
    assert_eq!((r.retired, r.due), (1, 1));
    let r = db
        .fetch(
            &site,
            FetchOptions {
                force: true,
                feed_ids: vec![b],
            },
        )
        .await;
    assert_eq!(r.due, 0);
    assert_eq!(count("/b.xml").await, 4);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case19_fetch_history_is_pruned() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (_, id) = one_feed(&db, &site, "/f.xml", "rss2.xml").await;
    // fetch_history_days = 30 in the test configuration.
    sqlx::query(
        "INSERT INTO feeds.fetches (run_id, feed_id, started_at, duration_ms, outcome)
         SELECT max(run_id), $1, now() - make_interval(days => d), 1, 'ok'
         FROM feeds.fetch_runs, unnest(ARRAY[45, 31, 29, 1]) AS d GROUP BY d",
    )
    .bind(id)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(fetches(&db, id).await.len(), 5);
    db.fetch_forced(&site).await;
    let kept: Vec<i32> = sqlx::query_scalar(
        "SELECT extract(day FROM now() - started_at)::int FROM feeds.fetches
         WHERE feed_id = $1 ORDER BY started_at",
    )
    .bind(id)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(kept, [29, 1, 0, 0]);
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case20_run_lock() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/f.xml"]);
    site.serve_fixture("/f.xml", "rss2.xml").await;

    let mut holder = PgConnection::connect_with(&db.options).await.unwrap();
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(feed_index::db::lock_key())
        .fetch_one(&mut holder)
        .await
        .unwrap();
    assert!(got);

    assert!(db.try_fetch(&site, force()).await.unwrap().is_none());
    let out = db.run_binary(&site, &["fetch", "--force"], "info").await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "exit 0: {stderr}");
    assert!(out.stdout.is_empty());
    assert_eq!(stderr.matches("run lock").count(), 1, "{stderr}");
    let out = db.run_binary(&site, &["sync"], "info").await;
    assert!(!out.status.success(), "a manual sync says it did nothing");
    assert_eq!(
        db.scalar_i64("SELECT count(*) FROM feeds.fetch_runs").await,
        0
    );
    assert_eq!(db.scalar_i64("SELECT count(*) FROM feeds.feeds").await, 0);
    assert!(site.requests("/f.xml").await.is_empty());

    holder.close().await.unwrap();
    assert_eq!(db.fetch_due(&site).await.ok, 1);
    // A finished run released its lock.
    let mut probe = PgConnection::connect_with(&db.options).await.unwrap();
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(feed_index::db::lock_key())
        .fetch_one(&mut probe)
        .await
        .unwrap();
    assert!(free);
    probe.close().await.unwrap();
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case21_log_hygiene() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let secret = "/private-reading-list-wombat";
    let paths = [
        format!("{secret}/feed.xml"),
        format!("{secret}/missing.xml"),
        format!("{secret}/page.html"),
        format!("{secret}/redirect"),
        format!("{secret}/table.xml"),
    ];
    site.set_feeds(&paths.iter().map(String::as_str).collect::<Vec<_>>());
    Mock::given(method("GET"))
        .and(path(paths[0].as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("hygiene.xml"), "application/rss+xml")
                .insert_header("ETag", "\"wombat-etag\""),
        )
        .mount(&site.server)
        .await;
    site.respond(&paths[1], ResponseTemplate::new(404)).await;
    site.serve_fixture(&paths[2], "html-page.html").await;
    site.respond(
        &paths[3],
        ResponseTemplate::new(301).insert_header("Location", paths[0].as_str()),
    )
    .await;
    site.serve_fixture(&paths[4], "panic-table.xml").await;

    let mut logs = String::new();
    for args in [
        &["migrate"][..],
        &["sync"],
        &["fetch"],
        &["fetch", "--force"],
        &["fetch"],
    ] {
        let out = db.run_binary(&site, args, "trace").await;
        assert!(out.status.success(), "{args:?}");
        logs.push_str(&String::from_utf8_lossy(&out.stderr));
        logs.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    // status and probe print titles and URLs to stdout for a human; only their logs count.
    for args in [
        &["status"][..],
        &["probe", "--opml", site.opml_path().to_str().unwrap()],
    ] {
        let out = db.run_binary(&site, args, "trace").await;
        logs.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(
        db.scalar_i64("SELECT count(*) FROM feeds.entries").await,
        5,
        "the fixtures were stored (two via the redirect)"
    );
    assert!(logs.contains("feed-index run="), "summary printed");
    assert!(logs.contains("outcome=http_error status=404"));
    assert!(logs.contains("host=127.0.0.1"), "hosts may be logged");
    assert!(
        logs.len() > 5000,
        "trace logging produced output: {}",
        logs.len()
    );
    let full_url_prefix = format!("{}/", site.server.uri());
    for needle in [
        "wombat", // every feed path and the ETag
        full_url_prefix.as_str(),
        "Narwhal",
        "Quasar",
        "Platypus",
        "Nebula",
        "Axolotl",
        "Pulsar",
        "Okapi",
        "Meridian",
        "Zebrafinch",
        "hygiene.example.org",
        "table.example.org",
        "hostile table",
        "We moved",
        "another address",
    ] {
        if let Some(line) = logs.lines().find(|l| l.contains(needle)) {
            let at = line.find(needle).unwrap();
            let lo = line.floor_char_boundary(at.saturating_sub(200));
            panic!(
                "log output contains {needle:?}: ...{}",
                &line[lo..(at + needle.len()).min(line.len())]
            );
        }
    }
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case22_dates_are_stored_or_null() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    let (_, id) = one_feed(&db, &site, "/d.xml", "dates.xml").await;
    let e = entries(&db, id).await;
    for x in &e {
        let expect_date = x.entry_key == "good";
        assert_eq!(x.published_at.is_some(), expect_date, "{}", x.entry_key);
        if x.entry_key != "good" {
            assert_eq!(x.updated_at, None, "{}", x.entry_key);
        }
    }
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_marks_failing_and_quiet_feeds() {
    let Some(db) = test_db().await else { return };
    let site = Site::new().await;
    site.set_feeds(&["/ok.xml", "/bad.xml"]);
    site.serve_fixture("/ok.xml", "rss2.xml").await;
    site.respond("/bad.xml", ResponseTemplate::new(500)).await;
    db.fetch_due(&site).await;
    sqlx::query(
        "UPDATE feeds.feeds SET last_new_entry_at = now() - interval '20 days' WHERE xml_url = $1",
    )
    .bind(site.url("/ok.xml"))
    .execute(&db.pool)
    .await
    .unwrap();
    let out = db.run_binary(&site, &["status"], "info").await;
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("ID"), "{text}");
    let ok = lines.iter().find(|l| l.contains("Feed /ok.xml")).unwrap();
    assert!(
        ok.contains("QUIET>14d") && !ok.contains("FAILING") && ok.contains(" ok "),
        "{ok}"
    );
    let bad = lines.iter().find(|l| l.contains("Feed /bad.xml")).unwrap();
    assert!(
        bad.contains("FAILING") && bad.contains("http_error"),
        "{bad}"
    );
    let out = db
        .run_binary(&site, &["status", "--quiet-days", "30"], "info")
        .await;
    assert!(!String::from_utf8_lossy(&out.stdout).contains("QUIET"));
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_needs_no_database_or_config() {
    let site = Site::new().await;
    site.serve_fixture("/p.xml", "rss2.xml").await;
    site.respond("/x.xml", ResponseTemplate::new(410)).await;
    let (ok_url, gone_url) = (site.url("/p.xml"), site.url("/x.xml"));
    let run = |args: Vec<String>| async move {
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_feed-index"))
                .args(args)
                .env("PGHOST", "/nonexistent")
                .env("PGPASSWORD", "refused-if-a-database-were-used")
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    };
    let out = run(vec!["probe".into(), "--show".into(), ok_url.clone()]).await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains(&ok_url));
    assert!(
        text.contains("outcome=ok status=200 entries=2 distinct=2 ids=2 fingerprints=0"),
        "{text}"
    );
    assert!(
        text.contains("newest_published=2026-10-05T07:30:00+00:00"),
        "{text}"
    );
    assert!(text.contains("declared_title=Example Wire"), "{text}");
    assert!(text.contains("[1] First   story"), "{text}");
    assert!(text.contains("Full text of the first[1] story."), "{text}");
    let req = &site.requests("/p.xml").await[0];
    assert!(
        req.headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("feed-index/")
    );

    // From an OPML file; a failing feed makes the exit status non-zero.
    site.write_opml(&format!(
        r#"<outline xmlUrl="{ok_url}"/><outline xmlUrl="{gone_url}"/>"#
    ));
    let out = run(vec![
        "probe".into(),
        "--opml".into(),
        site.opml_path().to_str().unwrap().into(),
    ])
    .await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success());
    assert!(text.contains("outcome=http_error status=410"), "{text}");
    assert!(!text.contains("[1]"), "titles only with --show");
    // A named config file must exist.
    let out = run(vec![
        "--config".into(),
        "/nonexistent/c.toml".into(),
        "probe".into(),
        ok_url,
    ])
    .await;
    assert!(!out.status.success());
}

/// With `PGSSLMODE=require`, a server that declines TLS must get no plaintext startup
/// message (which would carry the user name and database) and the command must fail.
#[test]
fn sslmode_require_refuses_plaintext() {
    for (mode, expect_plaintext) in [("require", false), ("prefer", true)] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut ssl_request = [0u8; 8];
            s.read_exact(&mut ssl_request).unwrap();
            assert_eq!(ssl_request, [0, 0, 0, 8, 4, 210, 22, 47], "SSLRequest");
            s.write_all(b"N").unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
            rest.len()
        });
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_feed-index"))
            .arg("migrate")
            .env_remove("PGPASSWORD")
            .env_remove("PGPASSFILE")
            .env("PGHOST", "127.0.0.1")
            .env("PGPORT", port.to_string())
            .env("PGUSER", "nobody")
            .env("PGDATABASE", "nothing")
            .env("PGSSLMODE", mode)
            .env("PGCONNECT_TIMEOUT", "5")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{mode}");
        let plaintext = server.join().unwrap();
        assert_eq!(
            plaintext > 0,
            expect_plaintext,
            "{mode}: {plaintext} bytes after 'N'"
        );
    }
}
