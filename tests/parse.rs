//! Document → entry rows, without a database or network (cases 5, 6, 16, 17 and 22 at the
//! parsing level; the database tests cover storing them).

use std::path::Path;

use chrono::{TimeZone, Utc};
use feed_index::entry::{self, BodySource, KeySource, ParsedFeed, parse_document};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn parse(name: &str) -> ParsedFeed {
    parse_document(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn case05_rss2_with_guids() {
    let f = parse("rss2.xml");
    assert_eq!(f.declared_title.as_deref(), Some("Example Wire"));
    assert_eq!(f.entries.len(), 2);
    let a = &f.entries[0];
    assert_eq!(a.entry_key, "news-1", "trimmed guid");
    assert_eq!(a.key_source, KeySource::Id);
    assert_eq!(a.title.as_deref(), Some("First   story"), "title as given");
    assert_eq!(
        a.link.as_deref(),
        Some("https://news.example.org/2026/first?utm_source=rss"),
        "link as given"
    );
    assert_eq!(a.author.as_deref(), Some("Ada Example"));
    assert_eq!(
        a.published_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 5, 7, 30, 0).unwrap())
    );
    assert_eq!(
        a.summary_html.as_deref(),
        Some("<p>Short <b>summary</b> one.</p>")
    );
    assert!(
        a.content_html
            .as_deref()
            .unwrap()
            .starts_with("<p>Full text")
    );
    assert_eq!(a.body_source, BodySource::Content);
    assert_eq!(
        a.body_text.as_deref(),
        Some(
            "Full text of the first[1] story.\n\nSecond paragraph.\n\n[1]: https://news.example.org/ref"
        ),
        "rendered, normalised, links reduced as in maildir-index"
    );
    let b = &f.entries[1];
    assert_eq!(b.entry_key, "https://news.example.org/2026/second");
    assert_eq!(b.content_html, None);
    assert_eq!(b.body_source, BodySource::Summary);
    assert_eq!(
        b.body_text.as_deref(),
        Some("Only a summary, in plain words.")
    );
    assert_eq!(b.author, None);
}

#[test]
fn case05_atom_with_ids() {
    let f = parse("atom.xml");
    assert_eq!(f.declared_title.as_deref(), Some("Example Atom"));
    let a = &f.entries[0];
    assert_eq!(a.entry_key, "tag:writer.example.com,2026:one");
    assert_eq!(a.key_source, KeySource::Id);
    assert_eq!(a.link.as_deref(), Some("https://writer.example.com/one"));
    assert_eq!(a.author.as_deref(), Some("Writer Person"));
    assert_eq!(
        a.published_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap())
    );
    assert_eq!(
        a.updated_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 2, 11, 0, 0).unwrap())
    );
    assert_eq!(a.summary_html.as_deref(), Some("<p>Atom summary</p>"));
    assert_eq!(a.content_html.as_deref(), Some("<p>Atom content body.</p>"));
    assert_eq!(a.body_text.as_deref(), Some("Atom content body."));
    let b = &f.entries[1];
    assert_eq!(b.link.as_deref(), Some("https://writer.example.com/two"));
    assert_eq!(b.published_at, None);
    assert_eq!(b.body_source, BodySource::Summary);
    assert_eq!(b.body_text.as_deref(), Some("Plain summary text."));
}

#[test]
fn case05_rss1_rdf() {
    let f = parse("rss1.rdf");
    assert_eq!(f.declared_title.as_deref(), Some("Example RDF"));
    assert_eq!(f.entries.len(), 2);
    let a = &f.entries[0];
    // feed-rs does not expose rdf:about as an id, so RSS 1.0 items are fingerprinted.
    assert_eq!(a.key_source, KeySource::Fingerprint);
    assert_eq!(
        a.entry_key,
        entry::fingerprint("https://old.example.org/a", "RDF item A")
    );
    assert_eq!(a.title.as_deref(), Some("RDF item A"));
    assert_eq!(a.author.as_deref(), Some("Rdf Author"));
    assert_eq!(
        a.published_at,
        Some(Utc.with_ymd_and_hms(2026, 9, 30, 7, 0, 0).unwrap())
    );
    assert_eq!(a.body_text.as_deref(), Some("Description of A."));
    assert_eq!(f.entries[1].body_source, BodySource::None);
    assert_eq!(f.entries[1].body_text, None);
}

#[test]
fn case05_json_feed() {
    let f = parse("feed.json");
    assert_eq!(f.declared_title.as_deref(), Some("Example JSON"));
    let a = &f.entries[0];
    assert_eq!(
        (a.entry_key.as_str(), a.key_source),
        ("json-1", KeySource::Id)
    );
    assert_eq!(a.link.as_deref(), Some("https://json.example.org/1"));
    assert_eq!(a.author.as_deref(), Some("Json Author"));
    assert_eq!(
        a.content_html.as_deref(),
        Some("<p>JSON <em>html</em> content.</p>")
    );
    assert_eq!(a.summary_html.as_deref(), Some("JSON summary."));
    assert_eq!(a.body_text.as_deref(), Some("JSON html content."));
    let b = &f.entries[1];
    // content_text is plain: normalised, not rendered.
    assert_eq!(b.body_source, BodySource::Content);
    assert_eq!(b.body_text.as_deref(), Some("Plain text\n\ncontent."));
    assert_eq!(
        b.updated_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 2, 9, 0, 0).unwrap())
    );
}

#[test]
fn case06_fingerprints_ignore_utm_and_fragment() {
    let plain = parse("rss-noguid.xml");
    let tracked = parse("rss-noguid-utm.xml");
    assert_eq!(plain.entries.len(), 2);
    for (p, t) in plain.entries.iter().zip(&tracked.entries) {
        assert_eq!(p.key_source, KeySource::Fingerprint);
        assert!(p.entry_key.starts_with("fp:"));
        assert_eq!(p.entry_key, t.entry_key);
        assert_ne!(p.link, t.link, "the link itself is stored as given");
    }
    assert_ne!(plain.entries[0].entry_key, plain.entries[1].entry_key);
}

#[test]
fn case16_nul_bytes_are_stripped() {
    for name in ["nul.xml", "nul.json"] {
        let f = parse(name);
        let e = &f.entries[0];
        for (field, v) in [
            ("key", Some(e.entry_key.as_str())),
            ("title", e.title.as_deref()),
            ("link", e.link.as_deref()),
            ("summary", e.summary_html.as_deref()),
            ("content", e.content_html.as_deref()),
            ("body", e.body_text.as_deref()),
        ] {
            assert!(!v.unwrap_or("").contains('\0'), "{name} {field}: {v:?}");
        }
        assert!(e.body_text.as_deref().unwrap().contains("with nul") || name == "nul.json");
        assert!(!f.declared_title.unwrap().contains('\0'));
    }
}

#[test]
fn case16_renderer_panic_keeps_the_entry() {
    let f = parse("panic-table.xml");
    assert_eq!(f.entries.len(), 1);
    let e = &f.entries[0];
    assert_eq!(e.body_source, BodySource::None);
    assert_eq!(e.body_text, None);
    assert_eq!(e.title.as_deref(), Some("Has a hostile table"));
    assert!(e.content_html.as_deref().unwrap().contains("rowspan=\"0\""));
}

#[test]
fn case17_duplicate_guid_keeps_first() {
    let f = parse("dup-guid.xml");
    assert_eq!(f.entries_in_doc, 3);
    let titles: Vec<_> = f
        .entries
        .iter()
        .map(|e| e.title.as_deref().unwrap())
        .collect();
    assert_eq!(titles, ["Kept", "Other"]);
}

#[test]
fn case22_dates() {
    let f = parse("dates.xml");
    let by_key = |k: &str| f.entries.iter().find(|e| e.entry_key == k).unwrap();
    for k in ["none", "old", "future", "garbage"] {
        assert_eq!(by_key(k).published_at, None, "{k}");
        assert_eq!(by_key(k).updated_at, None, "{k}");
    }
    assert_eq!(
        by_key("good").published_at,
        Some(Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap())
    );
}

#[test]
fn html_page_is_not_a_feed() {
    assert!(parse_document(&fixture("html-page.html")).is_err());
}

#[test]
fn hostile_input_never_panics() {
    let mut docs: Vec<Vec<u8>> = vec![
        b"<".to_vec(),
        b"{".to_vec(),
        b"<rss version=\"2.0\"><channel><item><title>".to_vec(),
        b"<feed xmlns=\"http://www.w3.org/2005/Atom\"><entry><id>".to_vec(),
        b"<!DOCTYPE x [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;\">]><rss version=\"2.0\"><channel><title>&b;</title></channel></rss>".to_vec(),
        vec![0xff, 0xfe, 0x00, 0x3c],
    ];
    for name in ["rss2.xml", "atom.xml", "feed.json", "rss1.rdf"] {
        let full = fixture(name);
        for cut in (0..full.len()).step_by(37) {
            docs.push(full[..cut].to_vec());
        }
    }
    for d in docs {
        let _ = parse_document(&d);
    }
}
