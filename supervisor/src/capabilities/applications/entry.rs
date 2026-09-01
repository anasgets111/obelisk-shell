//! One `.desktop` file, parsed (ADR-0061). The desktop entry specification's file format
//! and its `Exec` quoting rules, hand-written rather than pulled from a crate: the whole of what
//! this needs is one group header, seven keys and one tokenizer, and owning the semantics is
//! worth more here than a dependency's opinion about them.

use std::collections::HashMap;
use std::path::Path;

/// Every `Exec` field code the specification defines. All of them expand to something a launcher
/// with no file, no URL and no invoking-menu-item does not have, so all of them are dropped.
///
/// `%i` is the interesting one: it expands to `--icon <Icon>`, which is real and which this
/// drops anyway. An app that renders differently without it is an app whose own `Icon=` key we
/// already read and hand to the config, so passing it again through argv would only let the two
/// disagree.
const FIELD_CODES: [char; 13] = ['f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'i', 'c', 'k', 'v', 'm'];

/// The `[Desktop Entry]` group's keys, as written. Values are unescaped only for `Exec`, by
/// [`tokenize_exec`]; every other key is a display string this hands through verbatim.
pub type Group = HashMap<String, String>;

/// Reads the `[Desktop Entry]` group out of a `.desktop` file's contents.
///
/// Stops at the next group header, which is not a detail: a `.desktop` file routinely carries
/// `[Desktop Action new-window]` groups after the main one, each with its own `Name` and `Exec`.
/// Reading the whole file into one flat map would let an action's `Exec` overwrite the
/// application's, and the launcher would open a new window of an app that was not running.
///
/// Localized keys (`Name[de]`) are skipped rather than merged, so `Name` always means the
/// unlocalized value. ponytail: that makes the launcher read English names on a localized
/// system. Picking the right one is `$LC_MESSAGES`/`$LANG` and a `ll_CC` then `ll` then bare
/// lookup, about a dozen lines, and it goes in the day someone runs this in a locale that has
/// translations. Leaving it half-done (matching `ll` but not `ll_CC`) would be worse than not
/// matching at all, since it would silently prefer the wrong regional variant.
pub fn parse_group(contents: &str) -> Option<Group> {
    let mut group = Group::new();
    let mut inside = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if inside {
                break; // the next group starts here -- see this function's doc comment.
            }
            inside = line.eq_ignore_ascii_case("[Desktop Entry]");
            continue;
        }
        if !inside {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue; // not a key/value line; the specification has no other line shape.
        };
        let key = key.trim();
        if key.contains('[') {
            continue; // a localized key, skipped deliberately.
        }
        group.insert(key.to_string(), value.trim().to_string());
    }
    inside.then_some(group)
}

/// Whether a key's value is the specification's `true`. Absent is false, which is what every
/// boolean key here (`NoDisplay`, `Hidden`, `Terminal`) wants as its default.
pub fn flag(group: &Group, key: &str) -> bool {
    group.get(key).is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Splits an `Exec` value into a command and its arguments, applying the specification's quoting
/// rules and dropping its field codes.
///
/// Quoting is double-quote only, with backslash escapes inside them, exactly as the
/// specification writes it. Single quotes are not quoting characters in a `.desktop` file even
/// though every shell treats them as such, which is the one rule a reader coming from shell
/// syntax gets wrong, and getting it wrong here would split `Exec=foo 'a b'` into three
/// arguments instead of two.
///
/// Returns `None` for an `Exec` that has no command left after the field codes come out, which
/// is a malformed entry rather than an empty one.
pub fn tokenize_exec(exec: &str) -> Option<(String, Vec<String>)> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    // Tracks a token that exists but is empty, so `Exec=foo ""` keeps its empty argument
    // instead of silently dropping it. A bare run of spaces has no token to flush.
    let mut started = false;
    let mut chars = exec.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if quoted => {
                // Inside quotes a backslash escapes the next character. Pushed verbatim rather
                // than matched against the escapable set: an entry escaping something the
                // specification does not list means the literal character either way.
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            ch => {
                current.push(ch);
                started = true;
            }
        }
    }
    if started {
        tokens.push(current);
    }

    let mut expanded = tokens.into_iter().filter_map(|token| expand_field_codes(&token));
    let command = expanded.next()?;
    Some((command, expanded.collect()))
}

/// Removes a token's field codes and unescapes `%%`, or drops the token entirely if that leaves
/// nothing behind.
///
/// An unknown `%x` is kept as written rather than dropped. The specification reserves `%` and
/// lists the codes exhaustively, so an unknown one is a malformed entry; keeping it means the
/// app receives a visibly wrong argument it can complain about, where dropping it would hand
/// over a silently different command line.
///
/// ponytail: a token that mixes a field code with other text (`--file=%f`) keeps the other text
/// and loses only the code, leaving `--file=`. Dropping the whole token would be right for that
/// shape and wrong for `%c` inside a longer string. Neither is common enough to have a caller
/// worth deciding against, and the launcher passes no files, so both spellings are unreachable
/// today.
fn expand_field_codes(token: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = token.chars().peekable();
    let mut had_code = false;
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(code) if FIELD_CODES.contains(&code) => had_code = true,
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    // An empty token that never held a code is a real empty argument (`Exec=foo ""`) and is
    // kept; one that held a code is the code's own remains and is dropped.
    if out.is_empty() && had_code { None } else { Some(out) }
}

/// The desktop file id: the path relative to the applications directory it was found under,
/// with `/` replaced by `-` and the `.desktop` suffix removed.
///
/// The subdirectory rule is the specification's and it is load-bearing for deduplication rather
/// than cosmetic: `kde4/konsole.desktop` is the id `kde4-konsole`, distinct from a top-level
/// `konsole.desktop`, so two different applications in one tree do not collapse into one.
pub fn desktop_file_id(path: &Path, base: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let text = relative.to_str()?.strip_suffix(".desktop")?;
    Some(text.replace('/', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trap this parser exists to avoid: `[Desktop Action ...]` groups carry their own `Name`
    /// and `Exec`, and a flat read of the whole file lets the last one win. A launcher built on
    /// that opens a new window of an application instead of starting it.
    #[test]
    fn parse_group_stops_at_the_next_group_header() {
        let contents = "\
[Desktop Entry]
Name=Firefox
Exec=firefox %u

[Desktop Action new-window]
Name=New Window
Exec=firefox --new-window
";
        let group = parse_group(contents).expect("the file has a [Desktop Entry] group");
        assert_eq!(group.get("Name").map(String::as_str), Some("Firefox"));
        assert_eq!(
            group.get("Exec").map(String::as_str),
            Some("firefox %u"),
            "the action's Exec must not overwrite the application's"
        );
    }

    #[test]
    fn parse_group_skips_localized_keys_so_name_is_always_the_unlocalized_value() {
        let group = parse_group("[Desktop Entry]\nName=Files\nName[de]=Dateien\n").unwrap();
        assert_eq!(group.get("Name").map(String::as_str), Some("Files"));
        assert!(!group.contains_key("Name[de]"));
    }

    #[test]
    fn parse_group_returns_nothing_for_a_file_with_no_desktop_entry_group() {
        assert!(parse_group("[Desktop Action foo]\nName=x\n").is_none());
    }

    #[test]
    fn parse_group_ignores_comments_and_blank_lines() {
        let group = parse_group("[Desktop Entry]\n# a comment\n\nName=Thing\n").unwrap();
        assert_eq!(group.get("Name").map(String::as_str), Some("Thing"));
        assert_eq!(group.len(), 1);
    }

    #[test]
    fn flag_reads_the_specifications_true_and_treats_an_absent_key_as_false() {
        let group = parse_group("[Desktop Entry]\nNoDisplay=true\nHidden=false\n").unwrap();
        assert!(flag(&group, "NoDisplay"));
        assert!(!flag(&group, "Hidden"));
        assert!(!flag(&group, "Terminal"), "an absent boolean key is false, not an error");
    }

    #[test]
    fn tokenize_exec_splits_on_whitespace() {
        assert_eq!(
            tokenize_exec("kitty --single-instance"),
            Some(("kitty".to_string(), vec!["--single-instance".to_string()]))
        );
    }

    /// A `.desktop` file quotes with double quotes only. Treating single quotes as quoting, the
    /// way every shell does, would split this into three arguments instead of two.
    #[test]
    fn tokenize_exec_quotes_with_double_quotes_and_leaves_single_quotes_literal() {
        assert_eq!(
            tokenize_exec(r#"prog "one two" 'three"#),
            Some(("prog".to_string(), vec!["one two".to_string(), "'three".to_string()]))
        );
    }

    #[test]
    fn tokenize_exec_honours_backslash_escapes_inside_quotes() {
        assert_eq!(tokenize_exec(r#"prog "a\"b""#), Some(("prog".to_string(), vec![r#"a"b"#.to_string()])));
    }

    /// Every field code expands to a file, URL or menu detail a launcher does not have, so a
    /// token that is only a field code has to disappear rather than reach argv as a literal
    /// `%u` the target application would try to open.
    #[test]
    fn tokenize_exec_drops_standalone_field_codes() {
        assert_eq!(tokenize_exec("firefox %u"), Some(("firefox".to_string(), Vec::new())));
        assert_eq!(tokenize_exec("gimp %U %f %F"), Some(("gimp".to_string(), Vec::new())));
    }

    #[test]
    fn tokenize_exec_unescapes_a_doubled_percent_into_a_literal_one() {
        assert_eq!(tokenize_exec("prog 100%%"), Some(("prog".to_string(), vec!["100%".to_string()])));
    }

    #[test]
    fn tokenize_exec_keeps_an_unknown_percent_escape_as_written() {
        // Not a specified field code, so it is a malformed entry. Kept, so the application can
        // complain about an argument it can see, rather than silently receiving a different one.
        assert_eq!(tokenize_exec("prog %z"), Some(("prog".to_string(), vec!["%z".to_string()])));
    }

    #[test]
    fn tokenize_exec_returns_nothing_when_no_command_survives() {
        assert_eq!(tokenize_exec("%f"), None);
        assert_eq!(tokenize_exec("   "), None);
    }

    #[test]
    fn desktop_file_id_turns_a_subdirectory_into_a_dash() {
        let base = Path::new("/usr/share/applications");
        assert_eq!(
            desktop_file_id(Path::new("/usr/share/applications/kde4/konsole.desktop"), base),
            Some("kde4-konsole".to_string())
        );
        assert_eq!(
            desktop_file_id(Path::new("/usr/share/applications/firefox.desktop"), base),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn desktop_file_id_rejects_a_file_that_is_not_a_desktop_entry() {
        assert_eq!(
            desktop_file_id(Path::new("/usr/share/applications/mimeinfo.cache"), Path::new("/usr/share/applications")),
            None
        );
    }
}
