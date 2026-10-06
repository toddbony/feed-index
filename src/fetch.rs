//! The `fetch` run: lock, migrate, sync, fetch what is due, write entries, schedule, prune.
//!
//! Logs carry feed ids, hosts, status codes, outcomes, counts and durations only: never entry
//! titles, links or content, and never full feed URLs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::postgres::PgConnectOptions;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::FETCHER_VERSION;
use crate::config::Config;
use crate::db::{self, RunLock};
use crate::entry::ParsedFeed;
use crate::http::{self, Attempt, Outcome, Validators};
use crate::opml::OpmlFeed;
use crate::sync;

/// Longest wait between fetches of a failing feed, and the cap on `Retry-After`.
const MAX_DELAY_SECS: f64 = 24.0 * 3600.0;
/// Successful fetches are rescheduled `poll_minutes` plus up to this fraction later.
const JITTER: f64 = 0.10;

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Ignore `next_fetch_at`.
    pub force: bool,
    /// Only these feed ids (empty: all).
    pub feed_ids: Vec<i32>,
}

#[derive(Debug, Clone)]
pub struct FeedResult {
    pub feed_id: i32,
    pub host: String,
    /// `None` when the attempt could not be recorded (database error or panic).
    pub outcome: Option<Outcome>,
    pub http_status: Option<u16>,
    pub new_entries: u64,
    pub changed_entries: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub run_id: i64,
    pub due: u64,
    pub ok: u64,
    pub not_modified: u64,
    pub failed: u64,
    pub new_entries: u64,
    pub changed_entries: u64,
    pub retired: u64,
    pub duration_s: u64,
    /// Feeds that did not succeed, in feed id order.
    pub failures: Vec<FeedResult>,
    /// Attempts that could not be recorded: a run-level failure.
    pub errors: u64,
}

impl RunReport {
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "feed-index run={} due={} ok={} not_modified={} failed={} new_entries={} changed_entries={} retired={} duration_s={}",
            self.run_id,
            self.due,
            self.ok,
            self.not_modified,
            self.failed,
            self.new_entries,
            self.changed_entries,
            self.retired,
            self.duration_s
        )];
        for f in &self.failures {
            let mut line = format!(
                "feed-index feed_id={} host={} outcome={}",
                f.feed_id,
                f.host,
                f.outcome.map_or("database_error", Outcome::as_str)
            );
            if let Some(s) = f.http_status {
                line.push_str(&format!(" status={s}"));
            }
            lines.push(line);
        }
        lines
    }
}

/// The whole `fetch` command once the configuration and the feed list have been read.
/// `Ok(None)`: another run holds the lock, and nothing was done.
pub async fn run(
    lock_options: &PgConnectOptions,
    pool: &PgPool,
    config: &Config,
    feeds: &[OpmlFeed],
    options: &FetchOptions,
) -> Result<Option<RunReport>> {
    let Some(lock) = RunLock::try_acquire(lock_options).await? else {
        tracing::info!("another feed-index run holds the run lock; nothing fetched");
        return Ok(None);
    };
    let result = run_locked(pool, config, feeds, options).await;
    let released = lock.release().await;
    let report = result?;
    released?;
    Ok(Some(report))
}

/// The `sync` command: the same steps as a run up to and including the sync.
pub async fn sync_command(
    lock_options: &PgConnectOptions,
    pool: &PgPool,
    feeds: &[OpmlFeed],
) -> Result<sync::SyncReport> {
    let Some(lock) = RunLock::try_acquire(lock_options).await? else {
        anyhow::bail!("another feed-index run holds the run lock; nothing synced");
    };
    let result = async {
        db::migrate(pool).await?;
        sync::sync(pool, feeds).await
    }
    .await;
    let released = lock.release().await;
    let report = result?;
    released?;
    Ok(report)
}

async fn run_locked(
    pool: &PgPool,
    config: &Config,
    feeds: &[OpmlFeed],
    options: &FetchOptions,
) -> Result<RunReport> {
    let started = Instant::now();
    db::migrate(pool).await?;
    let synced = sync::sync(pool, feeds).await?;
    tracing::info!(
        added = synced.added,
        updated = synced.updated,
        retired = synced.retired,
        "feed list synced"
    );
    let client = http::Client::new(config)?;
    let due = select_due(pool, options).await?;
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO feeds.fetch_runs (feeds_due, fetcher_version) VALUES ($1, $2) RETURNING run_id",
    )
    .bind(i32::try_from(due.len()).unwrap_or(i32::MAX))
    .bind(FETCHER_VERSION)
    .fetch_one(pool)
    .await
    .context("recording fetch run")?;
    tracing::info!(run_id, due = due.len(), "fetch run started");

    let mut report = RunReport {
        run_id,
        due: due.len() as u64,
        retired: synced.retired,
        ..RunReport::default()
    };
    let semaphore = Arc::new(Semaphore::new(config.concurrency));
    let mut tasks = JoinSet::new();
    let mut task_feeds = HashMap::new();
    for feed in due {
        let (client, pool, semaphore) = (client.clone(), pool.clone(), semaphore.clone());
        let (feed_id, host) = (feed.feed_id, host_of(&feed.xml_url));
        let handle = tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            fetch_one(&client, &pool, run_id, feed).await
        });
        task_feeds.insert(handle.id(), (feed_id, host));
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((_, r)) => results.push(r),
            Err(e) => {
                let (feed_id, host) = task_feeds.remove(&e.id()).unwrap_or((0, "-".into()));
                tracing::error!(feed_id, host = %host, "feed task failed (panic or cancellation)");
                results.push(FeedResult {
                    feed_id,
                    host,
                    outcome: None,
                    http_status: None,
                    new_entries: 0,
                    changed_entries: 0,
                });
            }
        }
    }
    results.sort_by_key(|r| r.feed_id);
    for r in results {
        report.new_entries += r.new_entries;
        report.changed_entries += r.changed_entries;
        match r.outcome {
            Some(Outcome::Ok) => report.ok += 1,
            Some(Outcome::NotModified) => report.not_modified += 1,
            Some(_) => {
                report.failed += 1;
                report.failures.push(r);
            }
            None => {
                report.failed += 1;
                report.errors += 1;
                report.failures.push(r);
            }
        }
    }

    let pruned = sqlx::query(
        "DELETE FROM feeds.fetches WHERE started_at < now() - make_interval(days => $1)",
    )
    .bind(config.fetch_history_days)
    .execute(pool)
    .await
    .context("pruning fetch history")?
    .rows_affected();
    sqlx::query(
        "UPDATE feeds.fetch_runs SET finished_at = now(), ok = $2, not_modified = $3, failed = $4,
             new_entries = $5, changed_entries = $6
         WHERE run_id = $1",
    )
    .bind(run_id)
    .bind(clamp_i32(report.ok))
    .bind(clamp_i32(report.not_modified))
    .bind(clamp_i32(report.failed))
    .bind(clamp_i32(report.new_entries))
    .bind(clamp_i32(report.changed_entries))
    .execute(pool)
    .await
    .context("finishing fetch run")?;
    report.duration_s = started.elapsed().as_secs();
    tracing::info!(
        run_id,
        ok = report.ok,
        not_modified = report.not_modified,
        failed = report.failed,
        new_entries = report.new_entries,
        changed_entries = report.changed_entries,
        fetches_pruned = pruned,
        duration_s = report.duration_s,
        "fetch run finished"
    );
    Ok(report)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct DueFeed {
    feed_id: i32,
    xml_url: String,
    poll_minutes: i32,
    etag: Option<String>,
    last_modified: Option<String>,
    consecutive_failures: i32,
    /// No successful fetch yet: what this fetch finds is the back catalogue.
    never_ok: bool,
}

async fn select_due(pool: &PgPool, options: &FetchOptions) -> Result<Vec<DueFeed>> {
    let due: Vec<DueFeed> = sqlx::query_as(
        "SELECT feed_id, xml_url, poll_minutes, etag, last_modified, consecutive_failures,
                last_ok_at IS NULL AS never_ok
         FROM feeds.feeds
         WHERE retired_at IS NULL
           AND ($1 OR next_fetch_at <= now())
           AND (cardinality($2::integer[]) = 0 OR feed_id = ANY($2))
         ORDER BY next_fetch_at, feed_id",
    )
    .bind(options.force)
    .bind(&options.feed_ids)
    .fetch_all(pool)
    .await
    .context("selecting due feeds")?;
    if !options.feed_ids.is_empty() {
        let active: HashSet<i32> = sqlx::query_scalar(
            "SELECT feed_id FROM feeds.feeds WHERE retired_at IS NULL AND feed_id = ANY($1)",
        )
        .bind(&options.feed_ids)
        .fetch_all(pool)
        .await
        .context("checking --feed ids")?
        .into_iter()
        .collect();
        for id in &options.feed_ids {
            if !active.contains(id) {
                tracing::warn!(feed_id = id, "--feed names no active feed; ignored");
            }
        }
    }
    Ok(due)
}

async fn fetch_one(client: &http::Client, pool: &PgPool, run_id: i64, feed: DueFeed) -> FeedResult {
    let host = host_of(&feed.xml_url);
    let started_at = Utc::now();
    let clock = Instant::now();
    let validators = Validators {
        etag: feed.etag.clone(),
        last_modified: feed.last_modified.clone(),
    };
    let attempt = client.fetch(&feed.xml_url, &validators).await;
    let duration_ms = i32::try_from(clock.elapsed().as_millis()).unwrap_or(i32::MAX);
    let mut result = FeedResult {
        feed_id: feed.feed_id,
        host,
        outcome: Some(attempt.outcome),
        http_status: attempt.http_status,
        new_entries: 0,
        changed_entries: 0,
    };
    match record(pool, run_id, &feed, &attempt, started_at, duration_ms).await {
        Ok((new, changed)) => {
            result.new_entries = new;
            result.changed_entries = changed;
            tracing::info!(
                feed_id = feed.feed_id,
                host = %result.host,
                outcome = attempt.outcome.as_str(),
                status = attempt.http_status,
                entries = attempt.parsed.as_ref().map(|p| p.entries_in_doc),
                new = new,
                changed = changed,
                ms = duration_ms,
                "fetched"
            );
        }
        Err(e) => {
            tracing::error!(
                feed_id = feed.feed_id,
                host = %result.host,
                outcome = attempt.outcome.as_str(),
                "recording the fetch failed: {e:#}"
            );
            result.outcome = None;
        }
    }
    result
}

/// Everything one attempt changes, in one transaction: entries, the feed row and the
/// `fetches` row. Returns (new, changed) entry counts.
async fn record(
    pool: &PgPool,
    run_id: i64,
    feed: &DueFeed,
    attempt: &Attempt,
    started_at: DateTime<Utc>,
    duration_ms: i32,
) -> Result<(u64, u64)> {
    let mut tx = pool.begin().await?;
    let (new, changed) = match &attempt.parsed {
        Some(parsed) => write_entries(&mut tx, feed, parsed).await?,
        None => (0, 0),
    };
    let poll_secs = f64::from(feed.poll_minutes) * 60.0;
    match attempt.outcome {
        Outcome::Ok => {
            let parsed = attempt.parsed.as_ref().expect("Ok has a parsed feed");
            let resolved = attempt
                .final_url
                .as_ref()
                .filter(|u| url::Url::parse(&feed.xml_url).ok().as_ref() != Some(*u))
                .map(|u| u.as_str().to_string());
            sqlx::query(
                "UPDATE feeds.feeds SET
                     etag = $2, last_modified = $3, declared_title = $4, resolved_url = $5,
                     last_attempt_at = now(), last_ok_at = now(),
                     consecutive_failures = 0, last_error = NULL,
                     next_fetch_at = now() + make_interval(secs => $6),
                     last_new_entry_at = CASE WHEN $7 THEN now() ELSE last_new_entry_at END
                 WHERE feed_id = $1",
            )
            .bind(feed.feed_id)
            .bind(&attempt.etag)
            .bind(&attempt.last_modified)
            .bind(&parsed.declared_title)
            .bind(resolved)
            .bind(poll_secs * (1.0 + jitter()))
            .bind(new > 0)
            .execute(&mut *tx)
            .await?;
        }
        Outcome::NotModified => {
            sqlx::query(
                "UPDATE feeds.feeds SET
                     last_attempt_at = now(), last_ok_at = now(),
                     consecutive_failures = 0, last_error = NULL,
                     next_fetch_at = now() + make_interval(secs => $2)
                 WHERE feed_id = $1",
            )
            .bind(feed.feed_id)
            .bind(poll_secs * (1.0 + jitter()))
            .execute(&mut *tx)
            .await?;
        }
        _ => {
            let failures = feed.consecutive_failures.saturating_add(1);
            let delay = failure_delay_secs(poll_secs, failures, attempt.retry_after, Utc::now());
            sqlx::query(
                "UPDATE feeds.feeds SET
                     last_attempt_at = now(),
                     consecutive_failures = consecutive_failures + 1, last_error = $2,
                     next_fetch_at = now() + make_interval(secs => $3)
                 WHERE feed_id = $1",
            )
            .bind(feed.feed_id)
            .bind(&attempt.error)
            .bind(delay)
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "INSERT INTO feeds.fetches (run_id, feed_id, started_at, duration_ms, outcome, http_status,
             bytes, entries_in_doc, new_entries, changed_entries, error)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(run_id)
    .bind(feed.feed_id)
    .bind(started_at)
    .bind(duration_ms)
    .bind(attempt.outcome.as_str())
    .bind(
        attempt
            .http_status
            .map(|s| i16::try_from(s).unwrap_or(i16::MAX)),
    )
    .bind(attempt.bytes.map(clamp_i32))
    .bind(
        attempt
            .parsed
            .as_ref()
            .map(|p| i32::try_from(p.entries_in_doc).unwrap_or(i32::MAX)),
    )
    .bind(clamp_i32(new))
    .bind(clamp_i32(changed))
    .bind(&attempt.error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((new, changed))
}

/// New keys are inserted; a known key with the same hash is not written at all; a known key
/// with a different hash is updated and counts a revision.
async fn write_entries(
    tx: &mut Transaction<'_, Postgres>,
    feed: &DueFeed,
    parsed: &ParsedFeed,
) -> Result<(u64, u64)> {
    let keys: Vec<&str> = parsed
        .entries
        .iter()
        .map(|e| e.entry_key.as_str())
        .collect();
    let existing: HashMap<String, Vec<u8>> = sqlx::query_as(
        "SELECT entry_key, content_sha256 FROM feeds.entries
         WHERE feed_id = $1 AND entry_key = ANY($2)",
    )
    .bind(feed.feed_id)
    .bind(&keys)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    let (mut new, mut changed) = (0, 0);
    for e in &parsed.entries {
        match existing.get(&e.entry_key) {
            None => {
                sqlx::query(
                    "INSERT INTO feeds.entries (feed_id, entry_key, key_source, link, title, author,
                         published_at, updated_at, summary_html, content_html, body_text,
                         body_source, content_sha256, backfill, fetcher_version)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
                )
                .bind(feed.feed_id)
                .bind(&e.entry_key)
                .bind(e.key_source.as_str())
                .bind(&e.link)
                .bind(&e.title)
                .bind(&e.author)
                .bind(e.published_at)
                .bind(e.updated_at)
                .bind(&e.summary_html)
                .bind(&e.content_html)
                .bind(&e.body_text)
                .bind(e.body_source.as_str())
                .bind(&e.content_sha256[..])
                .bind(feed.never_ok)
                .bind(FETCHER_VERSION)
                .execute(&mut **tx)
                .await?;
                new += 1;
            }
            Some(hash) if hash.as_slice() == e.content_sha256 => {}
            Some(_) => {
                sqlx::query(
                    "UPDATE feeds.entries SET link = $3, title = $4, author = $5,
                         published_at = $6, updated_at = $7, summary_html = $8, content_html = $9,
                         body_text = $10, body_source = $11, content_sha256 = $12,
                         revisions = revisions + 1, last_changed_at = now(), fetcher_version = $13
                     WHERE feed_id = $1 AND entry_key = $2",
                )
                .bind(feed.feed_id)
                .bind(&e.entry_key)
                .bind(&e.link)
                .bind(&e.title)
                .bind(&e.author)
                .bind(e.published_at)
                .bind(e.updated_at)
                .bind(&e.summary_html)
                .bind(&e.content_html)
                .bind(&e.body_text)
                .bind(e.body_source.as_str())
                .bind(&e.content_sha256[..])
                .bind(FETCHER_VERSION)
                .execute(&mut **tx)
                .await?;
                changed += 1;
            }
        }
    }
    Ok((new, changed))
}

/// `min(poll × 2^(failures−1), 24 h)`, then at least until `Retry-After` (itself capped at
/// 24 h).
pub fn failure_delay_secs(
    poll_secs: f64,
    failures: i32,
    retry_after: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> f64 {
    let exp = failures.saturating_sub(1).clamp(0, 30);
    let backoff = (poll_secs * f64::from(1u32 << exp)).min(MAX_DELAY_SECS);
    match retry_after {
        Some(at) => {
            let wait = (at - now).num_milliseconds() as f64 / 1000.0;
            backoff.max(wait.min(MAX_DELAY_SECS))
        }
        None => backoff,
    }
}

/// A random fraction in [0, JITTER), without a random-number crate: `RandomState` is
/// seeded from the OS once and advanced on every call.
fn jitter() -> f64 {
    use std::hash::BuildHasher;
    let x = std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now());
    (x >> 11) as f64 / (1u64 << 53) as f64 * JITTER
}

/// The host of a feed URL: what logs may name.
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "-".into())
}

fn clamp_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn backoff() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let hour = 3600.0;
        assert_eq!(failure_delay_secs(hour, 1, None, now), hour);
        assert_eq!(failure_delay_secs(hour, 2, None, now), 2.0 * hour);
        assert_eq!(failure_delay_secs(hour, 3, None, now), 4.0 * hour);
        assert_eq!(failure_delay_secs(hour, 6, None, now), 24.0 * hour);
        assert_eq!(failure_delay_secs(hour, i32::MAX, None, now), 24.0 * hour);
        let in_2h = now + chrono::Duration::hours(2);
        assert_eq!(failure_delay_secs(600.0, 1, Some(in_2h), now), 2.0 * hour);
        // Retry-After never shortens the backoff, and is capped at 24 h.
        assert_eq!(failure_delay_secs(hour, 3, Some(in_2h), now), 4.0 * hour);
        let in_9d = now + chrono::Duration::days(9);
        assert_eq!(failure_delay_secs(600.0, 1, Some(in_9d), now), 24.0 * hour);
    }

    #[test]
    fn jitter_range() {
        let xs: Vec<f64> = (0..200).map(|_| jitter()).collect();
        assert!(xs.iter().all(|x| (0.0..JITTER).contains(x)));
        assert!(xs.iter().any(|x| *x != xs[0]), "jitter varies");
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://News.Example.org/feed?x=1"),
            "news.example.org"
        );
        assert_eq!(host_of("nonsense"), "-");
    }
}
