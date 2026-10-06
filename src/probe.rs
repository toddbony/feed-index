//! `probe`: fetch and parse feeds exactly as `fetch` would (without conditional headers), with
//! no database, and print what was found. Output is for a human, so it names URLs and, with
//! `--show`, entry titles and text; all of it is untrusted, so control characters are removed
//! before anything reaches the terminal.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::entry::KeySource;
use crate::http::{self, Attempt, Validators};

/// Characters of `body_text` shown per entry with `--show`.
const SHOW_CHARS: usize = 300;
const SHOW_ENTRIES: usize = 3;

/// Probe every URL; prints a block per URL to stdout, in the given order. `Ok(false)` if any
/// fetch did not end in `ok`.
pub async fn run(config: &Config, urls: Vec<String>, show: bool) -> Result<bool> {
    let client = http::Client::new(config)?;
    let semaphore = Arc::new(Semaphore::new(config.concurrency));
    let mut tasks = JoinSet::new();
    for (i, url) in urls.iter().enumerate() {
        let (client, semaphore, url) = (client.clone(), semaphore.clone(), url.clone());
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            (i, client.fetch(&url, &Validators::default()).await)
        });
    }
    let mut results: Vec<Option<Attempt>> = urls.iter().map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((i, attempt)) = joined {
            results[i] = Some(attempt);
        }
    }
    let mut all_ok = true;
    for (url, attempt) in urls.iter().zip(results) {
        let report = match &attempt {
            Some(a) => report(a, show),
            None => "  outcome=error (probe task failed)\n".to_string(),
        };
        all_ok &= attempt.is_some_and(|a| a.outcome == http::Outcome::Ok);
        print!("{}\n{report}\n", printable(url));
    }
    Ok(all_ok)
}

pub fn report(a: &Attempt, show: bool) -> String {
    let mut out = format!("  outcome={}", a.outcome.as_str());
    match a.http_status {
        Some(s) => out.push_str(&format!(" status={s}")),
        None => out.push_str(" status=-"),
    }
    if let Some(p) = &a.parsed {
        let ids = p
            .entries
            .iter()
            .filter(|e| e.key_source == KeySource::Id)
            .count();
        let newest = p
            .entries
            .iter()
            .filter_map(|e| e.published_at)
            .max()
            .map_or_else(|| "-".to_string(), |d| d.to_rfc3339());
        out.push_str(&format!(
            " entries={} distinct={} ids={} fingerprints={} newest_published={newest}",
            p.entries_in_doc,
            p.entries.len(),
            ids,
            p.entries.len() - ids
        ));
    }
    out.push('\n');
    if let Some(u) = &a.final_url {
        out.push_str(&format!("  final_url={}\n", printable(u.as_str())));
    }
    if let Some(ct) = &a.content_type {
        out.push_str(&format!("  content_type={}\n", printable(ct)));
    }
    if let Some(p) = &a.parsed {
        out.push_str(&format!(
            "  declared_title={}\n",
            p.declared_title.as_deref().map_or("-".into(), printable)
        ));
    }
    if let Some(e) = &a.error {
        out.push_str(&format!("  error={}\n", printable(e)));
    }
    if show && let Some(p) = &a.parsed {
        for (i, e) in p.entries.iter().take(SHOW_ENTRIES).enumerate() {
            out.push_str(&format!(
                "  [{}] {}\n",
                i + 1,
                e.title.as_deref().map_or("(no title)".into(), printable)
            ));
            let body: String = e
                .body_text
                .as_deref()
                .unwrap_or("(no body text)")
                .chars()
                .take(SHOW_CHARS)
                .collect();
            for line in body.lines() {
                out.push_str(&format!("      {}\n", printable(line)));
            }
        }
    }
    out
}

/// One line of untrusted text made safe for a terminal: control characters (escape sequences,
/// newlines) and bidirectional overrides become spaces.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_removes_escapes() {
        assert_eq!(printable("a\u{1b}[31mb\nc\u{202E}d"), "a [31mb c d");
    }
}
