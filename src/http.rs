//! One fetch of one feed: the request, the capped body, and the outcome. Shared by `fetch` and
//! `probe`, so that `probe` reports exactly what `fetch` would store.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::redirect;
use url::Url;

use crate::config::Config;
use crate::entry::{self, ParsedFeed};

const ACCEPT: &str = "application/rss+xml, application/atom+xml, application/feed+json, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.5";
const MAX_REDIRECTS: usize = 5;
/// Longest `error` text stored.
const MAX_ERROR_CHARS: usize = 200;

/// The schema's `fetches.outcome` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    NotModified,
    HttpError,
    NetworkError,
    ParseError,
    TooLarge,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::NotModified => "not_modified",
            Outcome::HttpError => "http_error",
            Outcome::NetworkError => "network_error",
            Outcome::ParseError => "parse_error",
            Outcome::TooLarge => "too_large",
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Outcome::Ok | Outcome::NotModified)
    }
}

/// Conditional GET validators from the last 200.
#[derive(Debug, Clone, Default)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Everything one attempt produced. `parsed` is set exactly when `outcome` is `Ok`.
#[derive(Debug)]
pub struct Attempt {
    pub outcome: Outcome,
    pub http_status: Option<u16>,
    /// Where the request ended up after redirects.
    pub final_url: Option<Url>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// From `Retry-After` on a 429 or 503.
    pub retry_after: Option<DateTime<Utc>>,
    /// Body bytes read (after decompression).
    pub bytes: Option<u64>,
    /// Short description; never the response body.
    pub error: Option<String>,
    pub parsed: Option<ParsedFeed>,
}

impl Attempt {
    fn failed(outcome: Outcome, error: String) -> Attempt {
        Attempt {
            outcome,
            http_status: None,
            final_url: None,
            content_type: None,
            etag: None,
            last_modified: None,
            retry_after: None,
            bytes: None,
            error: Some(truncate(&error)),
            parsed: None,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    max_bytes: u64,
}

impl Client {
    pub fn new(config: &Config) -> Result<Client> {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
        // Only http(s), at most MAX_REDIRECTS hops. The configured URL is never rewritten.
        let policy = redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error(RedirectRefused("too many redirects"))
            } else if !matches!(attempt.url().scheme(), "http" | "https") {
                attempt.error(RedirectRefused("redirect to a non-http(s) URL"))
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(config.user_agent())
            .default_headers(headers)
            .redirect(policy)
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .context("building the HTTP client")?;
        Ok(Client {
            client,
            max_bytes: config.max_feed_bytes,
        })
    }

    /// GET `url`, read at most `max_feed_bytes`, and parse a 200. Never fails: every problem
    /// is an outcome.
    pub async fn fetch(&self, url: &str, validators: &Validators) -> Attempt {
        let parsed_url = match Url::parse(url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => u,
            _ => {
                return Attempt::failed(Outcome::NetworkError, "not an http(s) URL".into());
            }
        };
        let mut request = self.client.get(parsed_url);
        if let Some(v) = validators.etag.as_deref().and_then(header_value) {
            request = request.header(header::IF_NONE_MATCH, v);
        }
        if let Some(v) = validators.last_modified.as_deref().and_then(header_value) {
            request = request.header(header::IF_MODIFIED_SINCE, v);
        }
        let mut response = match request.send().await {
            Ok(r) => r,
            Err(e) => return Attempt::failed(Outcome::NetworkError, describe(&e)),
        };
        let status = response.status();
        let text_header = |name: header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().replace('\0', ""))
                .filter(|s| !s.is_empty())
        };
        let mut attempt = Attempt {
            outcome: Outcome::Ok,
            http_status: Some(status.as_u16()),
            final_url: Some(response.url().clone()),
            content_type: text_header(header::CONTENT_TYPE),
            etag: text_header(header::ETAG),
            last_modified: text_header(header::LAST_MODIFIED),
            retry_after: None,
            bytes: None,
            error: None,
            parsed: None,
        };
        if status == reqwest::StatusCode::NOT_MODIFIED {
            attempt.outcome = Outcome::NotModified;
            return attempt;
        }
        if status != reqwest::StatusCode::OK {
            if matches!(status.as_u16(), 429 | 503) {
                attempt.retry_after = text_header(header::RETRY_AFTER)
                    .and_then(|v| parse_retry_after(&v, Utc::now()));
            }
            attempt.outcome = Outcome::HttpError;
            attempt.error = Some(format!("HTTP {}", status.as_u16()));
            return attempt;
        }
        // Content-Length is the compressed size when the body is compressed, so it can only
        // prove a body too large; the streamed count below is what the cap is enforced on.
        if response
            .content_length()
            .is_some_and(|n| n > self.max_bytes)
        {
            attempt.outcome = Outcome::TooLarge;
            attempt.bytes = response.content_length();
            attempt.error = Some(format!("body larger than {} bytes", self.max_bytes));
            return attempt;
        }
        let mut body: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() as u64 + chunk.len() as u64 > self.max_bytes {
                        attempt.outcome = Outcome::TooLarge;
                        attempt.bytes = Some(body.len() as u64 + chunk.len() as u64);
                        attempt.error = Some(format!("body larger than {} bytes", self.max_bytes));
                        return attempt;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    attempt.outcome = Outcome::NetworkError;
                    attempt.bytes = Some(body.len() as u64);
                    attempt.error = Some(describe(&e));
                    return attempt;
                }
            }
        }
        attempt.bytes = Some(body.len() as u64);
        // Parsing and rendering are CPU work on untrusted input: off the async threads.
        let parsed = tokio::task::spawn_blocking(move || entry::parse_document(&body)).await;
        match parsed {
            Ok(Ok(feed)) => attempt.parsed = Some(feed),
            Ok(Err(e)) => {
                attempt.outcome = Outcome::ParseError;
                attempt.error = Some(match attempt.content_type.as_deref().and_then(html_type) {
                    Some(t) => format!("got {t}, not a feed"),
                    None => e,
                });
            }
            Err(_) => {
                attempt.outcome = Outcome::ParseError;
                attempt.error = Some("parser panicked".into());
            }
        }
        attempt
    }
}

/// `text/html` or `application/xhtml+xml`, from a Content-Type value.
fn html_type(content_type: &str) -> Option<&'static str> {
    let essence = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match essence.as_str() {
        "text/html" => Some("text/html"),
        "application/xhtml+xml" => Some("application/xhtml+xml"),
        _ => None,
    }
}

fn header_value(s: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(s).ok()
}

#[derive(Debug)]
struct RedirectRefused(&'static str);

impl std::fmt::Display for RedirectRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for RedirectRefused {}

/// A short description of a request error, without the URL (reqwest's Display includes it).
fn describe(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_redirect() {
        "redirect refused"
    } else if e.is_connect() {
        "connect"
    } else if e.is_body() || e.is_decode() {
        "reading body"
    } else {
        "request"
    };
    // The innermost cause is the useful part ("Connection refused", "invalid peer
    // certificate: ...", "dns error: ...") and holds no URL.
    let mut source: &dyn std::error::Error = e;
    while let Some(s) = source.source() {
        source = s;
    }
    if std::ptr::addr_eq(source, e as &dyn std::error::Error) {
        return kind.to_string();
    }
    truncate(&format!("{kind}: {source}"))
}

fn truncate(s: &str) -> String {
    let s: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_CHARS)
        .collect();
    s
}

/// `Retry-After` as delay-seconds or an HTTP date.
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        let secs = i64::try_from(secs.min(10 * 365 * 86400)).ok()?;
        return now.checked_add_signed(chrono::Duration::seconds(secs));
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn retry_after_forms() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            parse_retry_after("3600", now),
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap())
        );
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 2026 02:00:00 GMT", now),
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 2, 0, 0).unwrap())
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
    }

    #[test]
    fn html_content_types() {
        assert_eq!(html_type("text/html; charset=utf-8"), Some("text/html"));
        assert_eq!(html_type("TEXT/HTML"), Some("text/html"));
        assert_eq!(html_type("application/rss+xml"), None);
    }

    #[test]
    fn errors_are_short_and_single_line() {
        let t = truncate(&format!("a\nb{}", "x".repeat(500)));
        assert!(!t.contains('\n'));
        assert_eq!(t.chars().count(), MAX_ERROR_CHARS);
    }
}
