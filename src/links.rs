//! Link simplification for `body_text`, which is for indexing, not for reconstructing the
//! entry (`summary_html` / `content_html` keep every URL intact). Copied from maildir-index.
//!
//! Every `http(s)://` URL in the text is replaced by a compact form:
//! 1. redirect wrappers are unwrapped to the URL they carry: Microsoft Safe Links
//!    (`*.safelinks.protection.outlook.com/?url=…`) and Google (`google.com/url?q=…`);
//! 2. only scheme, host (and a non-default port) and path are kept: no user info, query string
//!    or fragment, where marketing mail keeps its tracking tokens;
//! 3. a path longer than [`MAX_PATH_CHARS`] is cut and marked with `…`.
//!
//! Anything that doesn't parse as a URL is left exactly as it was.

use url::Url;

/// Longest path kept, in characters.
pub const MAX_PATH_CHARS: usize = 100;
/// Wrappers are unwrapped at most this many levels deep.
const MAX_UNWRAP: usize = 3;

/// Replace every `http://` / `https://` URL in `text` with its simplified form.
pub fn simplify_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(1 << 20));
    let mut rest = text;
    while let Some(start) = find_url_start(rest) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let len = url_len(tail);
        let (candidate, trailing) = split_trailing_punct(&tail[..len]);
        match simplify(candidate) {
            Some(s) => out.push_str(&s),
            None => out.push_str(candidate),
        }
        out.push_str(trailing);
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Byte offset of the next `http://` or `https://` (ASCII case-insensitive).
fn find_url_start(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 7 <= b.len() {
        if b[i].eq_ignore_ascii_case(&b'h') {
            let t = &b[i..];
            let https = t.len() >= 8 && t[..8].eq_ignore_ascii_case(b"https://");
            let http = t[..7].eq_ignore_ascii_case(b"http://");
            if https || http {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Length in bytes of the URL starting at `s`: up to whitespace or a delimiter that can't be
/// part of a URL in running text.
fn url_len(s: &str) -> usize {
    s.char_indices()
        .find(|&(_, c)| {
            c.is_whitespace()
                || matches!(
                    c,
                    '<' | '>' | '"' | '\'' | '`' | '|' | '[' | ']' | '{' | '}' | '\\' | '^'
                )
        })
        .map_or(s.len(), |(i, _)| i)
}

/// Sentence punctuation and unbalanced closing brackets at the end belong to the text.
fn split_trailing_punct(s: &str) -> (&str, &str) {
    let mut end = s.len();
    while let Some(c) = s[..end].chars().next_back() {
        let strip = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '*' => true,
            ')' => s[..end].matches('(').count() < s[..end].matches(')').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= c.len_utf8();
    }
    (&s[..end], &s[end..])
}

/// The simplified form of one URL, or `None` if it doesn't parse as http(s).
pub fn simplify(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    for _ in 0..MAX_UNWRAP {
        match unwrap_once(&url) {
            Some(inner) => url = inner,
            None => break,
        }
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let mut out = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    let path = url.path();
    if path != "/" {
        let mut chars = path.chars();
        let kept: String = chars.by_ref().take(MAX_PATH_CHARS).collect();
        out.push_str(&kept);
        if chars.next().is_some() {
            out.push('…');
        }
    }
    Some(out)
}

/// The URL a known redirect wrapper points to.
fn unwrap_once(url: &Url) -> Option<Url> {
    let host = url.host_str()?.to_ascii_lowercase();
    let key = if host.ends_with(".safelinks.protection.outlook.com") {
        "url"
    } else if (host == "google.com" || host == "www.google.com") && url.path() == "/url" {
        if url.query_pairs().any(|(k, _)| k == "q") {
            "q"
        } else {
            "url"
        }
    } else {
        return None;
    };
    let target = url.query_pairs().find(|(k, _)| k == key)?.1;
    let inner = Url::parse(&target).ok()?;
    matches!(inner.scheme(), "http" | "https").then_some(inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFE: &str = "https://nam12.safelinks.protection.outlook.com/?url=https%3A%2F%2Fwww.example.org%2Fdeals%2Ffall-sale%3Futm_source%3Dnews%26id%3D9&data=05%7C02%7C%7Cabc%7C84df9e7fe9f640afb435aaaaaaaaaaaa%7C1%7C0%7C638000000000000000&sdata=Zm9vYmFyYmF6&reserved=0";

    #[test]
    fn unwraps_safelinks() {
        assert_eq!(
            simplify(SAFE).as_deref(),
            Some("https://www.example.org/deals/fall-sale")
        );
        let text = format!("Shop now[1]\n\n[1]: {SAFE}\n");
        assert_eq!(
            simplify_urls(&text),
            "Shop now[1]\n\n[1]: https://www.example.org/deals/fall-sale\n"
        );
    }

    #[test]
    fn unwraps_google_redirects() {
        assert_eq!(
            simplify("https://www.google.com/url?q=https://example.com/a/b?x%3D1&sa=D&usg=AOv")
                .as_deref(),
            Some("https://example.com/a/b")
        );
    }

    #[test]
    fn drops_query_fragment_userinfo_keeps_port() {
        assert_eq!(
            simplify("https://user:pw@example.org:8443/p/q?id=5&utm_source=x#top").as_deref(),
            Some("https://example.org:8443/p/q")
        );
        assert_eq!(
            simplify("http://example.org/?a=1").as_deref(),
            Some("http://example.org")
        );
    }

    #[test]
    fn caps_long_paths() {
        let long = format!("https://click.example.org/{}", "a".repeat(400));
        let s = simplify(&long).unwrap();
        assert!(s.ends_with('…'));
        assert_eq!(
            s.chars().count(),
            "https://click.example.org".len() + MAX_PATH_CHARS + 1
        );
    }

    #[test]
    fn running_text_and_punctuation() {
        assert_eq!(
            simplify_urls(
                "See https://example.org/p?id=5. Or (https://example.org/q#x), <HTTPS://Example.org/r?z=1>"
            ),
            "See https://example.org/p. Or (https://example.org/q), <https://example.org/r>"
        );
        assert_eq!(
            simplify_urls("wiki https://example.org/A_(b)?x=1 end"),
            "wiki https://example.org/A_(b) end"
        );
    }

    #[test]
    fn leaves_other_text_alone() {
        for t in [
            "mailto:a@example.org",
            "no links here",
            "http:// broken",
            "ftp://example.org/x",
            "",
        ] {
            assert_eq!(simplify_urls(t), t, "{t}");
        }
    }
}
