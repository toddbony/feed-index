//! OPML → `feeds.feeds`: upsert by `xml_url`, retire what is no longer listed.

use anyhow::{Context, Result};
use sqlx::PgPool;

use crate::opml::OpmlFeed;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub added: u64,
    pub updated: u64,
    pub retired: u64,
    pub unchanged: u64,
}

impl SyncReport {
    pub fn summary_line(&self) -> String {
        format!(
            "feed-index sync added={} updated={} retired={} unchanged={}",
            self.added, self.updated, self.retired, self.unchanged
        )
    }
}

/// The feed list looks wrong (empty, or far shorter than what is active): nothing was changed.
#[derive(Debug)]
pub struct RetireGuard {
    pub in_file: usize,
    pub active: i64,
}

impl std::fmt::Display for RetireGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "retire guard: the feed list holds {} feeds but {} are active; refusing to retire \
             (nothing changed). Fix the feed list, or retire feeds in smaller steps",
            self.in_file, self.active
        )
    }
}

impl std::error::Error for RetireGuard {}

/// One transaction. Fails with [`RetireGuard`] (and changes nothing) if the list holds no feeds
/// or fewer than half of the currently active ones.
pub async fn sync(pool: &PgPool, feeds: &[OpmlFeed]) -> Result<SyncReport> {
    let mut tx = pool.begin().await.context("starting sync")?;
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM feeds.feeds WHERE retired_at IS NULL")
            .fetch_one(&mut *tx)
            .await
            .context("counting active feeds")?;
    if feeds.is_empty() || (feeds.len() as i64) * 2 < active {
        return Err(RetireGuard {
            in_file: feeds.len(),
            active,
        }
        .into());
    }

    let mut report = SyncReport::default();
    for f in feeds {
        // Only a real change is written. A new feed is due at once (next_fetch_at defaults to
        // now()); a changed poll_minutes leaves next_fetch_at alone.
        let row: Option<bool> = sqlx::query_scalar(
            "INSERT INTO feeds.feeds AS f (xml_url, title, folder, html_url, poll_minutes, weight)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (xml_url) DO UPDATE SET
                 title = EXCLUDED.title, folder = EXCLUDED.folder, html_url = EXCLUDED.html_url,
                 poll_minutes = EXCLUDED.poll_minutes, weight = EXCLUDED.weight, retired_at = NULL
             WHERE (f.title, f.folder, f.html_url, f.poll_minutes, f.weight, f.retired_at IS NULL)
                   IS DISTINCT FROM
                   (EXCLUDED.title, EXCLUDED.folder, EXCLUDED.html_url, EXCLUDED.poll_minutes,
                    EXCLUDED.weight, true)
             RETURNING (xmax = 0)",
        )
        .bind(&f.xml_url)
        .bind(&f.title)
        .bind(&f.folder)
        .bind(&f.html_url)
        .bind(f.poll_minutes)
        .bind(f.weight)
        .fetch_optional(&mut *tx)
        .await
        .context("upserting feed")?;
        match row {
            Some(true) => report.added += 1,
            Some(false) => report.updated += 1,
            None => report.unchanged += 1,
        }
    }

    let urls: Vec<&str> = feeds.iter().map(|f| f.xml_url.as_str()).collect();
    report.retired = sqlx::query(
        "UPDATE feeds.feeds SET retired_at = now()
         WHERE retired_at IS NULL AND NOT (xml_url = ANY($1))",
    )
    .bind(&urls)
    .execute(&mut *tx)
    .await
    .context("retiring feeds")?
    .rows_affected();
    tx.commit().await.context("committing sync")?;
    Ok(report)
}
