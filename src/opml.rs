//! The feed list: OPML 2.0.
//!
//! A feed is any `<outline>` with an `xmlUrl`, at any depth. Every problem is a configuration
//! error that stops the run before anything changes. Errors name line numbers, never URLs: the
//! feed list is personal and errors are logged.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpmlFeed {
    /// As written (trimmed); the identity of the feed.
    pub xml_url: String,
    pub title: String,
    /// Nearest enclosing outline without `xmlUrl`; `None` at top level.
    pub folder: Option<String>,
    pub html_url: Option<String>,
    pub poll_minutes: i32,
    pub weight: i32,
}

pub fn load(path: &Path, default_poll_minutes: i32) -> Result<Vec<OpmlFeed>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading feed list {}", path.display()))?;
    parse(&text, default_poll_minutes).with_context(|| format!("feed list {}", path.display()))
}

pub fn parse(text: &str, default_poll_minutes: i32) -> Result<Vec<OpmlFeed>> {
    // roxmltree refuses DTDs by default, so no entity expansion.
    let doc = roxmltree::Document::parse(text).map_err(|e| {
        let p = e.pos();
        anyhow::anyhow!("not well-formed XML at line {}, column {}", p.row, p.col)
    })?;
    let root = doc.root_element();
    if root.tag_name().name() != "opml" {
        bail!("root element is not <opml>");
    }
    let Some(body) = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "body")
    else {
        bail!("no <body> element");
    };
    let mut out = Vec::new();
    let mut seen: HashMap<String, u32> = HashMap::new();
    walk(&doc, body, None, default_poll_minutes, &mut out, &mut seen)?;
    Ok(out)
}

fn walk(
    doc: &roxmltree::Document<'_>,
    parent: roxmltree::Node<'_, '_>,
    folder: Option<&str>,
    default_poll_minutes: i32,
    out: &mut Vec<OpmlFeed>,
    seen: &mut HashMap<String, u32>,
) -> Result<()> {
    for node in parent
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "outline")
    {
        let line = doc.text_pos_at(node.range().start).row;
        let attr = |name: &str| {
            node.attribute(name)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        match node.attribute("xmlUrl") {
            Some(raw) => {
                let feed = feed_from(raw, &attr, folder, default_poll_minutes)
                    .with_context(|| format!("outline at line {line}"))?;
                if let Some(first) = seen.get(&feed.xml_url) {
                    bail!("outline at line {line}: duplicate xmlUrl (first at line {first})");
                }
                seen.insert(feed.xml_url.clone(), line);
                out.push(feed);
                // An outline with xmlUrl is not a folder; its children keep the outer folder.
                walk(doc, node, folder, default_poll_minutes, out, seen)?;
            }
            None => {
                let name = attr("text").or_else(|| attr("title"));
                walk(doc, node, name, default_poll_minutes, out, seen)?;
            }
        }
    }
    Ok(())
}

fn feed_from<'a>(
    raw_url: &str,
    attr: &impl Fn(&str) -> Option<&'a str>,
    folder: Option<&str>,
    default_poll_minutes: i32,
) -> Result<OpmlFeed> {
    let xml_url = raw_url.trim().to_string();
    let host = match Url::parse(&xml_url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => match u.host_str() {
            Some(h) => h.to_string(),
            None => bail!("xmlUrl has no host"),
        },
        _ => bail!("xmlUrl is not an http(s) URL"),
    };
    let poll_minutes = match attr("poll_minutes") {
        None => default_poll_minutes,
        Some(v) => match v.parse::<i32>() {
            Ok(n) if (5..=1440).contains(&n) => n,
            _ => bail!("poll_minutes must be an integer between 5 and 1440"),
        },
    };
    let weight = match attr("weight") {
        None => 0,
        Some(v) => v
            .parse::<i32>()
            .map_err(|_| anyhow::anyhow!("weight must be an integer"))?,
    };
    let title = attr("title")
        .or_else(|| attr("text"))
        .map(str::to_string)
        .unwrap_or(host);
    Ok(OpmlFeed {
        xml_url,
        title,
        folder: folder.map(str::to_string),
        html_url: attr("htmlUrl").map(str::to_string),
        poll_minutes,
        weight,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opml(body: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?>\n<opml version=\"2.0\"><head/><body>\n{body}\n</body></opml>"
        )
    }

    #[test]
    fn parses_example() {
        let feeds = parse(include_str!("../example.opml"), 60).unwrap();
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].folder.as_deref(), Some("Breaking"));
        assert_eq!(feeds[0].poll_minutes, 10);
        assert_eq!(feeds[0].weight, 50);
        assert_eq!(feeds[0].title, "Example Wire");
        assert_eq!(
            feeds[0].html_url.as_deref(),
            Some("https://news.example.org/")
        );
        assert_eq!(feeds[1].poll_minutes, 60);
    }

    #[test]
    fn nested_folders_defaults_and_title_fallbacks() {
        let text = opml(
            r#"<outline type="rss" text="Top" xmlUrl="https://a.example.org/feed"/>
            <outline title="Only title">
              <outline text="News">
                <outline text="text only" xmlUrl="https://b.example.org/feed"/>
                <outline title="T" text="ignored" xmlUrl="https://c.example.org/feed" weight="-3"/>
              </outline>
              <outline xmlUrl="http://d.example.com:8080/x.xml" poll_minutes="1440" foo="bar"/>
            </outline>"#,
        );
        let f = parse(&text, 30).unwrap();
        let summary: Vec<_> = f
            .iter()
            .map(|f| {
                (
                    f.title.as_str(),
                    f.folder.as_deref(),
                    f.poll_minutes,
                    f.weight,
                    f.html_url.is_some(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("Top", None, 30, 0, false),
                ("text only", Some("News"), 30, 0, false),
                ("T", Some("News"), 30, -3, false),
                ("d.example.com", Some("Only title"), 1440, 0, false),
            ]
        );
    }

    #[test]
    fn configuration_errors() {
        let cases = [
            (
                r#"<outline xmlUrl="https://a.example.org/f"/><outline xmlUrl="https://a.example.org/f"/>"#,
                "duplicate xmlUrl",
            ),
            (
                r#"<outline xmlUrl="https://a.example.org/f" poll_minutes="4"/>"#,
                "poll_minutes",
            ),
            (
                r#"<outline xmlUrl="https://a.example.org/f" poll_minutes="ten"/>"#,
                "poll_minutes",
            ),
            (
                r#"<outline xmlUrl="https://a.example.org/f" weight="1.5"/>"#,
                "weight",
            ),
            (r#"<outline xmlUrl="file:///etc/passwd"/>"#, "http(s)"),
            (r#"<outline xmlUrl="ftp://a.example.org/f"/>"#, "http(s)"),
            (r#"<outline xmlUrl=""/>"#, "http(s)"),
            (r#"<outline xmlUrl="feeds/relative.xml"/>"#, "http(s)"),
        ];
        for (body, expect) in cases {
            let err = format!("{:#}", parse(&opml(body), 60).unwrap_err());
            assert!(err.contains(expect), "{body}: {err}");
            assert!(err.contains("line 3"), "{body}: {err}");
            assert!(!err.contains("example.org"), "URL in error: {err}");
        }
        assert!(parse("<rss/>", 60).is_err());
        assert!(parse("<opml>", 60).is_err());
        assert!(parse("<opml><head/></opml>", 60).is_err());
        assert!(
            parse(
                "<!DOCTYPE opml [<!ENTITY a \"x\">]><opml><body/></opml>",
                60
            )
            .is_err()
        );
    }

    #[test]
    fn empty_body_is_zero_feeds() {
        assert!(parse(&opml(""), 60).unwrap().is_empty());
    }
}
