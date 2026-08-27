//! Markup allowlist parser: five allowlisted body-markup constructs (`<b>`, `<i>`, `<u>`,
//! `<a href>`, `<img src>`). Split from `dbus::notifications` -- see `dbus/notifications/mod.rs`
//! for the module-level doc.

use std::sync::LazyLock;

use regex::Regex;

use super::NotificationSpan;

// -------------------------------------------------------------------------------------------
// Markup allowlist parser (TDD seam 2): five constructs, everything else stripped.
// -------------------------------------------------------------------------------------------

/// Matches one HTML-ish tag (`<name ...>`, `</name>`, or a self-closing `<name .../>`), double-
/// quoted attribute values only -- matching every example in the spec docs and ADR-0033's own
/// grammar. `regex`'s guaranteed-linear-time matching keeps this "non-backtracking", the same
/// property §1.1's superseded flat-text sanitizer named explicitly.
static TAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"</?[a-zA-Z][a-zA-Z0-9]*(?:\s+[a-zA-Z_:][a-zA-Z0-9_:-]*\s*=\s*"[^"]*")*\s*/?>"#)
        .expect("TAG_PATTERN is a valid, hand-checked regex literal")
});

/// Extracts `key="value"` attribute pairs from a tag's own inner text (double-quoted only).
static ATTR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([a-zA-Z_:][a-zA-Z0-9_:-]*)\s*=\s*"([^"]*)""#).expect("ATTR_PATTERN is a valid, hand-checked regex literal"));

/// One recognized (or explicitly rejected) tag construct -- [`classify_tag`]'s output.
#[derive(Debug, Clone, PartialEq)]
enum ClassifiedTag {
    OpenBold,
    CloseBold,
    OpenItalic,
    CloseItalic,
    OpenUnderline,
    CloseUnderline,
    /// `<a href="URL">` -- an anchor with no `href` attribute is [`ClassifiedTag::Ignored`]
    /// instead, since it isn't a usable anchor construct.
    OpenAnchor(String),
    CloseAnchor,
    /// `<img src="PATH">` (self-closing or not; `alt`, if present, is parsed but discarded --
    /// nothing in this round's scope reads it). No `src` is [`ClassifiedTag::Ignored`].
    Image(String),
    /// `<script>`/`<style>` -- the opening half of an opaque block whose entire content (including
    /// any markup inside it) is discarded up to its matching close tag.
    OpaqueOpen(String),
    /// Anything else: an unrecognized element, a malformed construct, or an allowed tag missing a
    /// required attribute. The tag markup itself is stripped; unlike `OpaqueOpen`, its surrounding
    /// text is not touched -- only script/style content is executable/non-visual enough to drop
    /// outright (§1.1's original "strips out all executable scripts, style tags" carried forward).
    Ignored,
}

fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    ATTR_PATTERN.captures_iter(attrs).find_map(|caps| if caps[1].eq_ignore_ascii_case(key) { Some(caps[2].to_string()) } else { None })
}

/// Classifies one `TAG_PATTERN` match (including its surrounding `<`/`>`) into a
/// [`ClassifiedTag`]. Never panics on malformed input -- everything not recognized falls to
/// [`ClassifiedTag::Ignored`].
fn classify_tag(raw: &str) -> ClassifiedTag {
    let inner = &raw[1..raw.len() - 1];
    let is_closing = inner.starts_with('/');
    let body = if is_closing { &inner[1..] } else { inner };
    let body = body.trim_end_matches('/').trim();

    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let attrs = &body[name_end..];

    if is_closing {
        return match name.as_str() {
            "b" => ClassifiedTag::CloseBold,
            "i" => ClassifiedTag::CloseItalic,
            "u" => ClassifiedTag::CloseUnderline,
            "a" => ClassifiedTag::CloseAnchor,
            _ => ClassifiedTag::Ignored,
        };
    }

    match name.as_str() {
        "b" => ClassifiedTag::OpenBold,
        "i" => ClassifiedTag::OpenItalic,
        "u" => ClassifiedTag::OpenUnderline,
        "a" => extract_attr(attrs, "href").map(ClassifiedTag::OpenAnchor).unwrap_or(ClassifiedTag::Ignored),
        "img" => extract_attr(attrs, "src").map(ClassifiedTag::Image).unwrap_or(ClassifiedTag::Ignored),
        "script" | "style" => ClassifiedTag::OpaqueOpen(name),
        _ => ClassifiedTag::Ignored,
    }
}

/// Whether `raw` (a `TAG_PATTERN` match) is the closing tag matching `opaque_name`.
fn is_closing_tag_named(raw: &str, opaque_name: &str) -> bool {
    let inner = raw.trim_start_matches('<').trim_end_matches('>');
    inner.strip_prefix('/').is_some_and(|name| name.trim().eq_ignore_ascii_case(opaque_name))
}

/// Flushes `current` into a new [`NotificationSpan::Text`] carrying the currently-active style,
/// if it's non-empty. A no-op otherwise -- callers flush unconditionally on every style change and
/// at end-of-input, so most calls see an already-empty `current`.
fn flush_text(spans: &mut Vec<NotificationSpan>, current: &mut String, bold: u32, italic: u32, underline: u32, href: Option<String>) {
    if current.is_empty() {
        return;
    }
    spans.push(NotificationSpan::Text { text: std::mem::take(current), bold: bold > 0, italic: italic > 0, underline: underline > 0, href });
}

/// Parses `input` into [`NotificationSpan`]s, accepting exactly `<b>`, `<i>`, `<u>`,
/// `<a href="URL">`, `<img src="PATH" alt="ALT">` (self-closing or not) and rejecting/stripping
/// everything else (ADR-0033). Pure grammar only -- an `<img>`'s `src` is carried through
/// unvalidated; the real filesystem/path-trust check is a separate step ([`validate_trusted_path`],
/// applied by [`sanitize_body`]) so this function stays testable with no filesystem I/O.
///
/// Style depth counters (not a generic stack) mean nesting composes naturally
/// (`<b><i>x</i></b>` is both bold and italic) and an *unclosed* allowed tag simply applies its
/// style through to end-of-input instead of erroring -- the same lenient convention real
/// notification daemons (mako) use for malformed markup. `href` uses a real stack since nested
/// anchors with different targets are meaningful; the innermost one wins.
pub(super) fn parse_markup(input: &str) -> Vec<NotificationSpan> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut bold = 0u32;
    let mut italic = 0u32;
    let mut underline = 0u32;
    let mut href_stack: Vec<String> = Vec::new();
    let mut opaque: Option<String> = None;
    let mut last_end = 0;

    for m in TAG_PATTERN.find_iter(input) {
        let literal = &input[last_end..m.start()];
        last_end = m.end();

        if let Some(opaque_name) = &opaque {
            if is_closing_tag_named(m.as_str(), opaque_name) {
                opaque = None;
            }
            continue;
        }

        current.push_str(literal);
        match classify_tag(m.as_str()) {
            ClassifiedTag::OpenBold => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                bold += 1;
            }
            ClassifiedTag::CloseBold => {
                if bold > 0 {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    bold -= 1;
                }
            }
            ClassifiedTag::OpenItalic => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                italic += 1;
            }
            ClassifiedTag::CloseItalic => {
                if italic > 0 {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    italic -= 1;
                }
            }
            ClassifiedTag::OpenUnderline => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                underline += 1;
            }
            ClassifiedTag::CloseUnderline => {
                if underline > 0 {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    underline -= 1;
                }
            }
            ClassifiedTag::OpenAnchor(href) => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                href_stack.push(href);
            }
            ClassifiedTag::CloseAnchor => {
                if !href_stack.is_empty() {
                    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                    href_stack.pop();
                }
            }
            ClassifiedTag::Image(src) => {
                flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
                spans.push(NotificationSpan::Image { image_path: src });
            }
            ClassifiedTag::OpaqueOpen(name) => opaque = Some(name),
            ClassifiedTag::Ignored => {}
        }
    }

    if opaque.is_none() {
        current.push_str(&input[last_end..]);
    }
    flush_text(&mut spans, &mut current, bold, italic, underline, href_stack.last().cloned());
    spans
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_support::text;

    // ---- parse_markup (TDD seam 2) ----

    #[test]
    fn parse_markup_plain_text_is_a_single_unstyled_span() {
        assert_eq!(parse_markup("hello world"), vec![text("hello world", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_empty_input_is_empty() {
        assert_eq!(parse_markup(""), Vec::new());
    }

    #[test]
    fn parse_markup_handles_a_single_allowed_tag() {
        assert_eq!(parse_markup("<b>bold</b>"), vec![text("bold", true, false, false, None)]);
        assert_eq!(parse_markup("<i>italic</i>"), vec![text("italic", false, true, false, None)]);
        assert_eq!(parse_markup("<u>underline</u>"), vec![text("underline", false, false, true, None)]);
    }

    #[test]
    fn parse_markup_nests_allowed_tags() {
        assert_eq!(parse_markup("<b><i>text</i></b>"), vec![text("text", true, true, false, None)]);
    }

    #[test]
    fn parse_markup_handles_an_unclosed_tag_by_applying_style_to_end_of_input() {
        assert_eq!(parse_markup("<b>bold text"), vec![text("bold text", true, false, false, None)]);
    }

    #[test]
    fn parse_markup_strips_a_script_tag_mixed_with_legal_markup() {
        let spans = parse_markup(r#"<b>bold</b><script>alert(1)</script>more text"#);
        assert_eq!(spans, vec![text("bold", true, false, false, None), text("more text", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_strips_a_style_tag_and_its_content() {
        let spans = parse_markup("before<style>.x{color:red}</style>after");
        assert_eq!(spans, vec![text("beforeafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_drops_an_img_with_no_src() {
        let spans = parse_markup(r#"before<img alt="no src">after"#);
        assert_eq!(spans, vec![text("beforeafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_emits_an_image_span_for_a_self_closing_img() {
        let spans = parse_markup(r#"<img src="/usr/share/icons/x.png" alt="x"/>"#);
        assert_eq!(spans, vec![NotificationSpan::Image { image_path: "/usr/share/icons/x.png".to_string() }]);
    }

    #[test]
    fn parse_markup_emits_an_image_span_for_a_non_self_closing_img() {
        let spans = parse_markup(r#"<img src="/usr/share/icons/x.png">"#);
        assert_eq!(spans, vec![NotificationSpan::Image { image_path: "/usr/share/icons/x.png".to_string() }]);
    }

    #[test]
    fn parse_markup_handles_an_anchor_with_href() {
        let spans = parse_markup(r#"<a href="https://example.com">link</a>"#);
        assert_eq!(spans, vec![text("link", false, false, false, Some("https://example.com"))]);
    }

    #[test]
    fn parse_markup_drops_an_anchor_with_no_href() {
        let spans = parse_markup("before<a>no href</a>after");
        assert_eq!(spans, vec![text("beforeno hrefafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_strips_unrecognized_tags_but_keeps_their_surrounding_text() {
        let spans = parse_markup("before<div>middle</div>after");
        assert_eq!(spans, vec![text("beforemiddleafter", false, false, false, None)]);
    }

    #[test]
    fn parse_markup_mixes_multiple_constructs() {
        let spans = parse_markup(r#"plain <b>bold</b> and <a href="url">link</a>"#);
        assert_eq!(
            spans,
            vec![
                text("plain ", false, false, false, None),
                text("bold", true, false, false, None),
                text(" and ", false, false, false, None),
                text("link", false, false, false, Some("url")),
            ]
        );
    }
}
