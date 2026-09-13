//! Allowlist parser for `<b>`, `<i>`, `<u>`, `<a href>`, and `<img src>` body markup. Split from
//! `dbus::notifications`, see `dbus/notifications/mod.rs` for the module-level doc.

use std::sync::LazyLock;

use regex::Regex;

use super::NotificationSpan;

/// Matches HTML-ish opening, closing, or self-closing tags with double-quoted attributes.
/// ADR-0033's grammar stays linear-time and non-backtracking.
static TAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"</?[a-zA-Z][a-zA-Z0-9]*(?:\s+[a-zA-Z_:][a-zA-Z0-9_:-]*\s*=\s*"[^"]*")*\s*/?>"#)
        .expect("TAG_PATTERN is a valid, hand-checked regex literal")
});

/// Extracts double-quoted `key="value"` attributes.
static ATTR_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([a-zA-Z_:][a-zA-Z0-9_:-]*)\s*=\s*"([^"]*)""#)
        .expect("ATTR_PATTERN is a valid, hand-checked regex literal")
});

/// [`classify_tag`]'s recognized or rejected tag forms.
#[derive(Debug, Clone, PartialEq)]
enum ClassifiedTag {
    OpenBold,
    CloseBold,
    OpenItalic,
    CloseItalic,
    OpenUnderline,
    CloseUnderline,
    /// `<a href="URL">`; no `href` is [`ClassifiedTag::Ignored`].
    OpenAnchor(String),
    CloseAnchor,
    /// `<img src="PATH">`, self-closing or not; `alt` is discarded. No `src` is ignored.
    Image(String),
    /// `<script>`/`<style>` opening an opaque block discarded through its matching close tag.
    OpaqueOpen(String),
    /// Unrecognized/malformed tags or missing required attributes. Strip only the tag; unlike
    /// `OpaqueOpen`, preserve surrounding text. Script/style content is dropped.
    Ignored,
}

fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    ATTR_PATTERN
        .captures_iter(attrs)
        .find_map(|caps| if caps[1].eq_ignore_ascii_case(key) { Some(caps[2].to_string()) } else { None })
}

/// Classifies a `TAG_PATTERN` match; malformed input falls to [`ClassifiedTag::Ignored`].
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

/// Whether `raw` closes `opaque_name`.
fn is_closing_tag_named(raw: &str, opaque_name: &str) -> bool {
    let inner = raw.trim_start_matches('<').trim_end_matches('>');
    inner.strip_prefix('/').is_some_and(|name| name.trim().eq_ignore_ascii_case(opaque_name))
}

/// Flushes non-empty `current` into a styled [`NotificationSpan::Text`].
fn flush_text(
    spans: &mut Vec<NotificationSpan>,
    current: &mut String,
    bold: u32,
    italic: u32,
    underline: u32,
    href: Option<String>,
) {
    if current.is_empty() {
        return;
    }
    spans.push(NotificationSpan::Text {
        text: std::mem::take(current),
        bold: bold > 0,
        italic: italic > 0,
        underline: underline > 0,
        href,
    });
}

/// Parses exactly the five allowlisted constructs (ADR-0033). `<img>` paths stay unvalidated;
/// [`sanitize_body`] applies [`validate_trusted_path`] separately so this remains filesystem-free.
///
/// Style depth counters compose nesting (`<b><i>x</i></b>` is both styles), and unclosed tags
/// style through end-of-input, matching mako. `href` uses a stack; the innermost target wins.
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
    use super::super::test_support::text;
    use super::*;

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
