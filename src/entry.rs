//! A feed document to `feeds.entries` rows.
//!
//! Untrusted input: nothing here panics on a document (the parser and the renderer run under
//! `catch_unwind`), and every text field is NUL-stripped before it can reach the database.

use std::collections::HashSet;

use chrono::{DateTime, Datelike, Utc};
use feed_rs::model;
use feed_rs::parser::{self, ParseErrorKind, ParseFeedError};
use sha2::{Digest, Sha256};
use url::Url;

use crate::text::{self, strip_nul};

/// What the parser's id generator returns for an entry without an id. feed-rs synthesises an
/// id for such entries (a hash of link and title, or a random UUID), which would be
/// indistinguishable from a real one; this cannot occur in XML (NUL is not a legal character),
/// and in JSON only if a feed sends exactly this string, which then just means "no id".
pub const NO_ID: &str = "\u{0}feed-index:no-id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Id,
    Fingerprint,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::Id => "id",
            KeySource::Fingerprint => "fingerprint",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    Content,
    Summary,
    None,
}

impl BodySource {
    pub fn as_str(self) -> &'static str {
        match self {
            BodySource::Content => "content",
            BodySource::Summary => "summary",
            BodySource::None => "none",
        }
    }
}

/// One `feeds.entries` row, minus the columns the database or the writer fills in.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub entry_key: String,
    pub key_source: KeySource,
    pub link: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub summary_html: Option<String>,
    pub content_html: Option<String>,
    pub body_text: Option<String>,
    pub body_source: BodySource,
    pub content_sha256: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct ParsedFeed {
    /// The title the document declares.
    pub declared_title: Option<String>,
    /// Entries in the document, duplicates included.
    pub entries_in_doc: usize,
    /// Distinct keys, first occurrence kept, in document order.
    pub entries: Vec<EntryRow>,
}

/// Parse a feed document (RSS 0.9x/1.0/2.0, Atom, JSON Feed). The error is a short
/// description that never quotes the document.
pub fn parse_document(bytes: &[u8]) -> Result<ParsedFeed, String> {
    let parsed = std::panic::catch_unwind(|| {
        parser::Builder::new()
            .id_generator(|_links, _title, _uri| NO_ID.to_string())
            .sanitize_content(false)
            .build()
            .parse(bytes)
    });
    let feed = match parsed {
        Ok(Ok(feed)) => feed,
        Ok(Err(e)) => return Err(describe_parse_error(&e)),
        Err(_) => return Err("parser panicked".into()),
    };
    let entries_in_doc = feed.entries.len();
    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(entries_in_doc);
    for e in feed.entries {
        let row = entry_row(e);
        if seen.insert(row.entry_key.clone()) {
            entries.push(row);
        }
    }
    Ok(ParsedFeed {
        declared_title: feed
            .title
            .map(|t| strip_nul(t.content.trim()))
            .filter(|t| !t.is_empty()),
        entries_in_doc,
        entries,
    })
}

fn describe_parse_error(e: &ParseFeedError) -> String {
    match e {
        ParseFeedError::ParseError(ParseErrorKind::NoFeedRoot) => {
            "not a feed (no feed root element)"
        }
        ParseFeedError::ParseError(_) => "not a feed (invalid feed structure)",
        ParseFeedError::IoError(_) => "not a feed (read error)",
        ParseFeedError::JsonSerde(_) => "not a feed (invalid JSON)",
        ParseFeedError::JsonUnsupportedVersion(_) => "not a feed (unsupported JSON Feed version)",
        ParseFeedError::XmlReader(_) => "not a feed (malformed XML)",
    }
    .to_string()
}

pub fn entry_row(e: model::Entry) -> EntryRow {
    let link = pick_link(&e.links)
        .map(|l| strip_nul(l.href.trim()))
        .filter(|l| !l.is_empty());
    let title = e.title.as_ref().map(|t| strip_nul(&t.content));
    let author = e
        .authors
        .first()
        .and_then(|p| p.name.as_deref())
        .map(|n| strip_nul(n.trim()))
        .filter(|a| !a.is_empty());
    let summary = e
        .summary
        .as_ref()
        .map(|t| (strip_nul(&t.content), is_plain(t.content_type.as_str())));
    let content = e.content.as_ref().and_then(|c| {
        c.body
            .as_ref()
            .map(|b| (strip_nul(b), is_plain(c.content_type.as_str())))
    });

    let id = e.id.trim();
    let (entry_key, key_source) = if e.id == NO_ID || id.is_empty() || strip_nul(id).is_empty() {
        (
            fingerprint(
                link.as_deref().unwrap_or(""),
                title.as_deref().unwrap_or(""),
            ),
            KeySource::Fingerprint,
        )
    } else {
        (strip_nul(id), KeySource::Id)
    };

    let (body_text, body_source) = body_text(content.as_ref(), summary.as_ref());
    let summary_html = summary.map(|(s, _)| s);
    let content_html = content.map(|(c, _)| c);
    let content_sha256 = content_hash(&[
        title.as_deref(),
        link.as_deref(),
        summary_html.as_deref(),
        content_html.as_deref(),
    ]);
    EntryRow {
        entry_key,
        key_source,
        link,
        title,
        author,
        published_at: e.published.and_then(plausible),
        updated_at: e.updated.and_then(plausible),
        summary_html,
        content_html,
        body_text,
        body_source,
        content_sha256,
    }
}

/// The `alternate` link if there is one (a link without `rel` is alternate in Atom), else the
/// first link.
fn pick_link(links: &[model::Link]) -> Option<&model::Link> {
    links
        .iter()
        .find(|l| {
            l.rel
                .as_deref()
                .is_none_or(|r| r.trim().eq_ignore_ascii_case("alternate"))
        })
        .or_else(|| links.first())
}

fn is_plain(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/plain"))
}

/// Content if it has text, else summary: HTML rendered with html2text, plain text as is; then
/// link-reduced and normalised exactly as maildir-index does. A renderer failure (including a
/// contained panic) gives no text at all.
fn body_text(
    content: Option<&(String, bool)>,
    summary: Option<&(String, bool)>,
) -> (Option<String>, BodySource) {
    for (field, source) in [
        (content, BodySource::Content),
        (summary, BodySource::Summary),
    ] {
        let Some((value, plain)) = field else {
            continue;
        };
        let rendered = if *plain {
            value.clone()
        } else {
            match text::html_to_text(value) {
                Some(t) => t,
                None => return (None, BodySource::None),
            }
        };
        let t = text::normalize_text(&crate::links::simplify_urls(&rendered));
        if !t.is_empty() {
            return (Some(t), source);
        }
    }
    (None, BodySource::None)
}

/// Feeds put placeholder and garbage dates in; only 1971–2100 is believed.
fn plausible(d: DateTime<Utc>) -> Option<DateTime<Utc>> {
    (1971..=2100).contains(&d.year()).then_some(d)
}

/// `fp:` + hex SHA-256 of `normalised_link + "\n" + normalised_title`.
pub fn fingerprint(link: &str, title: &str) -> String {
    let input = format!("{}\n{}", normalize_link(link), normalize_title(title));
    format!("fp:{}", hex::encode(Sha256::digest(input.as_bytes())))
}

/// Lowercase scheme and host, no fragment, no `utm_*` query parameters, otherwise as written.
/// A link that does not parse as an absolute URL only loses its fragment.
pub fn normalize_link(link: &str) -> String {
    let link = link.trim();
    let Ok(mut url) = Url::parse(link) else {
        return link.split('#').next().unwrap_or("").to_string();
    };
    url.set_fragment(None);
    if let Some(q) = url.query() {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|p| !p.is_empty() && !p.starts_with("utm_"))
            .collect();
        let kept = kept.join("&");
        url.set_query((!kept.is_empty()).then_some(kept.as_str()));
    }
    url.to_string()
}

/// Whitespace runs collapsed to one space, trimmed.
pub fn normalize_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// SHA-256 over the fields, each prefixed with a presence byte and its length, so that no
/// change can move bytes from one field to another without changing the hash.
pub fn content_hash(fields: &[Option<&str>]) -> [u8; 32] {
    let mut h = Sha256::new();
    for f in fields {
        match f {
            None => h.update([0u8]),
            Some(s) => {
                h.update([1u8]);
                h.update((s.len() as u64).to_be_bytes());
                h.update(s.as_bytes());
            }
        }
    }
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_normalisation() {
        let base = normalize_link("https://Blog.Example.org/p/1?id=5");
        assert_eq!(base, "https://blog.example.org/p/1?id=5");
        for variant in [
            "HTTPS://blog.example.org/p/1?id=5#comments",
            "https://blog.example.org/p/1?utm_source=rss&id=5&utm_medium=feed",
            " https://blog.example.org/p/1?id=5&utm_campaign=x#top ",
        ] {
            assert_eq!(normalize_link(variant), base, "{variant}");
        }
        assert_eq!(
            normalize_link("https://example.org/a?utm_source=x"),
            "https://example.org/a"
        );
        // Case and encoding of the path and other parameters are kept.
        assert_eq!(
            normalize_link("https://example.org/A%20b?Q=Z"),
            "https://example.org/A%20b?Q=Z"
        );
        assert_eq!(normalize_link("/relative/path#x"), "/relative/path");
    }

    #[test]
    fn fingerprints() {
        let a = fingerprint("https://example.org/a?utm_source=x#f", "  Hello \n  world ");
        let b = fingerprint("https://EXAMPLE.org/a", "Hello world");
        assert_eq!(a, b);
        assert!(a.starts_with("fp:") && a.len() == 3 + 64);
        assert_ne!(a, fingerprint("https://example.org/a", "Hello world!"));
        assert_ne!(a, fingerprint("https://example.org/b", "Hello world"));
    }

    #[test]
    fn hash_fields_cannot_shift() {
        assert_ne!(
            content_hash(&[Some("ab"), Some("c")]),
            content_hash(&[Some("a"), Some("bc")])
        );
        assert_ne!(
            content_hash(&[None, Some("")]),
            content_hash(&[Some(""), None])
        );
        assert_eq!(
            content_hash(&[Some("x"), None]),
            content_hash(&[Some("x"), None])
        );
    }

    #[test]
    fn not_a_feed() {
        for doc in [
            &b""[..],
            b"hello",
            b"<html><body>hi</body></html>",
            b"{\"a\":1}",
            b"<rss><channel>",
        ] {
            assert!(
                parse_document(doc).is_err(),
                "{:?}",
                String::from_utf8_lossy(doc)
            );
        }
    }

    #[test]
    fn missing_ids_are_detected() {
        // feed-rs 3.0 would otherwise synthesise ids for these entries.
        let doc = br#"<rss version="2.0"><channel><title>t</title>
            <item><title>A</title><link>https://example.org/a</link></item>
            <item><title>B</title><link>https://example.org/b</link><guid>  </guid></item>
            <item><title>C</title><guid isPermaLink="false"> c-1 </guid></item>
            </channel></rss>"#;
        let f = parse_document(doc).unwrap();
        let keys: Vec<_> = f
            .entries
            .iter()
            .map(|e| (e.key_source, e.entry_key.as_str()))
            .collect();
        assert_eq!(keys[0].0, KeySource::Fingerprint);
        assert_eq!(keys[0].1, fingerprint("https://example.org/a", "A"));
        assert_eq!(keys[1].0, KeySource::Fingerprint);
        assert_eq!(keys[2], (KeySource::Id, "c-1"));
    }

    #[test]
    fn link_choice() {
        let doc = br#"<feed xmlns="http://www.w3.org/2005/Atom"><title>t</title><id>f</id>
            <entry><id>1</id><title>x</title>
              <link rel="enclosure" href="https://example.org/a.mp3"/>
              <link rel="alternate" href="https://example.org/post"/></entry>
            <entry><id>2</id><title>y</title><link rel="related" href="https://example.org/r"/></entry>
            </feed>"#;
        let f = parse_document(doc).unwrap();
        assert_eq!(
            f.entries[0].link.as_deref(),
            Some("https://example.org/post")
        );
        assert_eq!(f.entries[1].link.as_deref(), Some("https://example.org/r"));
    }
}
