//! `status`: one line per active feed, for a human.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, sqlx::FromRow)]
pub struct FeedStatus {
    pub feed_id: i32,
    pub folder: Option<String>,
    pub title: String,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_new_entry_at: Option<DateTime<Utc>>,
    pub consecutive_failures: i32,
    pub last_outcome: Option<String>,
    pub entries: i64,
    /// No new entry (or, for a feed that never had one, no existence) for `quiet_days`.
    pub quiet: bool,
}

pub async fn feeds(pool: &PgPool, quiet_days: i32) -> Result<Vec<FeedStatus>> {
    sqlx::query_as(
        "SELECT f.feed_id, f.folder, f.title, f.last_ok_at, f.last_new_entry_at,
                f.consecutive_failures,
                (SELECT x.outcome FROM feeds.fetches x WHERE x.feed_id = f.feed_id
                 ORDER BY x.started_at DESC, x.fetch_id DESC LIMIT 1) AS last_outcome,
                (SELECT count(*) FROM feeds.entries e WHERE e.feed_id = f.feed_id) AS entries,
                coalesce(f.last_new_entry_at, f.created_at)
                    < now() - make_interval(days => $1) AS quiet
         FROM feeds.feeds f
         WHERE f.retired_at IS NULL
         ORDER BY f.folder NULLS FIRST, f.title, f.feed_id",
    )
    .bind(quiet_days)
    .fetch_all(pool)
    .await
    .context("reading feed status")
}

pub fn render(rows: &[FeedStatus], quiet_days: i32) -> String {
    let when = |t: Option<DateTime<Utc>>| {
        t.map_or_else(
            || "-".to_string(),
            |t| t.format("%Y-%m-%d %H:%M").to_string(),
        )
    };
    let mut table: Vec<[String; 9]> = vec![[
        "ID".into(),
        "FOLDER".into(),
        "TITLE".into(),
        "LAST_OK".into(),
        "LAST_NEW".into(),
        "FAILS".into(),
        "LAST_OUTCOME".into(),
        "ENTRIES".into(),
        "MARK".into(),
    ]];
    for r in rows {
        let mut mark = Vec::new();
        if r.consecutive_failures > 0 {
            mark.push("FAILING".to_string());
        }
        if r.quiet {
            mark.push(format!("QUIET>{quiet_days}d"));
        }
        table.push([
            r.feed_id.to_string(),
            clip(r.folder.as_deref().unwrap_or("-"), 20),
            clip(&r.title, 40),
            when(r.last_ok_at),
            when(r.last_new_entry_at),
            r.consecutive_failures.to_string(),
            r.last_outcome.clone().unwrap_or_else(|| "-".into()),
            r.entries.to_string(),
            mark.join(","),
        ]);
    }
    let mut widths = [0usize; 9];
    for row in &table {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let mut out = String::new();
    for row in &table {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// One line, at most `max` characters, no control characters.
fn clip(s: &str, max: usize) -> String {
    let s = crate::probe::printable(s);
    if s.chars().count() <= max {
        return s;
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t.push('…');
    t
}
