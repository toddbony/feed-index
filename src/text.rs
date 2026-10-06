//! Plain text for `body_text`: HTML rendering and normalisation, copied from maildir-index
//! (`src/extract.rs`) so that both programs produce the same text from the same HTML.

/// Wrap width for HTML rendering: effectively "never wrap".
const HTML_WIDTH: usize = 1_000_000;

/// Render HTML to plain text: no wrapping, links as numbered footnotes, images as alt text
/// only, no decoration. `None` if html2text fails.
///
/// A panic inside html2text is contained here (html2text 0.17.1 slices out of bounds on some
/// `rowspan="0"` / `colspan="0"` tables).
pub fn html_to_text(html: &str) -> Option<String> {
    contain_panic(|| {
        html2text::config::with_decorator(html2text::render::TrivialDecorator::new())
            .link_footnotes(true)
            .no_table_borders()
            .allow_width_overflow()
            .string_from_read(html.as_bytes(), HTML_WIDTH)
            .ok()
    })
}

/// Run `f`, turning a panic into `None`. The panic hook still logs where it happened.
pub fn contain_panic<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .ok()
        .flatten()
}

/// Invisible format characters used as padding (preheader filler, spacer cells). They carry no
/// text and are dropped.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\0' | '\u{00AD}' // soft hyphen
            | '\u{034F}' // combining grapheme joiner
            | '\u{180E}' // Mongolian vowel separator
            | '\u{200B}'
            ..='\u{200D}' // zero-width space, non-joiner, joiner
            | '\u{2060}' // word joiner
            | '\u{FEFF}' // byte-order mark / zero-width no-break space
    )
}

/// Normalise text for search and blurbs: drop NULs and invisible padding characters, collapse
/// runs of horizontal whitespace (including no-break spaces) to one space, trim each line,
/// collapse runs of blank lines to a single blank line, and trim blank lines at both ends.
pub fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(1 << 20));
    let mut line = String::new();
    let mut blank_pending = false;
    for raw in s.split('\n') {
        line.clear();
        let mut space = false;
        for c in raw.chars() {
            if is_invisible(c) {
                continue;
            }
            if c.is_whitespace() {
                space = true;
                continue;
            }
            if space && !line.is_empty() {
                line.push(' ');
            }
            space = false;
            line.push(c);
        }
        if line.is_empty() {
            blank_pending = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_pending {
                out.push('\n');
            }
        }
        blank_pending = false;
        out.push_str(&line);
    }
    out
}

pub fn strip_nul(s: &str) -> String {
    s.replace('\0', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_rendering() {
        let t = html_to_text(
            "<html><head><style>p{color:red}</style></head><body><h1>Hi</h1><p><b>x</b> <a href=\"https://example.org/a\">link</a></p><img src=\"t.gif\"><img src=\"l.png\" alt=\"Logo\"></body></html>",
        )
        .unwrap();
        assert!(t.contains("link[1]"), "{t}");
        assert!(t.contains("[1]: https://example.org/a"), "{t}");
        assert!(t.contains("Logo"), "{t}");
        assert!(
            !t.contains('<') && !t.contains("**") && !t.contains("# "),
            "{t}"
        );
        assert!(!t.contains("color:red"), "{t}");
        let long = format!("<p>{}</p>", "word ".repeat(1000));
        assert_eq!(html_to_text(&long).unwrap().trim().lines().count(), 1);
    }

    #[test]
    fn normalization() {
        assert_eq!(
            normalize_text("\n\n  a   b\u{00A0}\u{00A0}c  \r\n\n\n\n \u{200C}\u{00A0} \n\td\n\n"),
            "a b c\n\nd"
        );
        assert_eq!(normalize_text("x\u{200B}y\u{FEFF}\u{034F}"), "xy");
        assert_eq!(normalize_text("one\ntwo\n\nthree"), "one\ntwo\n\nthree");
        assert_eq!(normalize_text("\u{200C}\u{00A0}\n \n\u{00AD}"), "");
        assert_eq!(normalize_text("nul\0byte"), "nulbyte");
    }

    #[test]
    fn renderer_panic_is_contained() {
        // html2text 0.17.1 panics on this table (rowspan="0" with colspan="0").
        let table = r#"<table><tr><td rowspan="0">a</td><td rowspan="2">b</td></tr><tr><td>c</td></tr><tr><td colspan="0">wide wide wide wide</td></tr></table>"#;
        assert_eq!(html_to_text(table), None);
        assert_eq!(contain_panic::<()>(|| panic!("boom")), None);
    }
}
