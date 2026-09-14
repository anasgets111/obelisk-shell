//! `fuzzy(haystack, needle)`: fzf's score for one candidate, or `nil` when the needle's characters
//! do not appear in order (ADR-0201).
//!
//! Ported from `Services/Utils/Fzf.qml` in the Quickshell config this shell mirrors, itself a
//! JavaScript port of fzf: BSD-3-Clause, copyright 2021 Ajit. Constants are unchanged, so fzf's
//! thresholds read across.
//!
//! The scorer only: iterate, sort, tiebreak and cap stay in the config. No backtrack
//! pass, since the mirror computes no match positions either; `start` is returned for its tiebreak.

use mlua::Lua;

const SCORE_MATCH: i32 = 16;
const SCORE_GAP_START: i32 = -3;
const SCORE_GAP_EXTENSION: i32 = -1;
const BONUS_BOUNDARY: i32 = SCORE_MATCH / 2;
const BONUS_CAMEL_OR_NUMBER: i32 = BONUS_BOUNDARY + SCORE_GAP_EXTENSION;
const BONUS_CONSECUTIVE: i32 = -(SCORE_GAP_START + SCORE_GAP_EXTENSION);
const BONUS_NON_WORD: i32 = SCORE_MATCH / 2;
const BONUS_FIRST_CHAR_MULTIPLIER: i32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    NonWord,
    Lower,
    Upper,
    Number,
}

/// Punctuation and spaces are `NonWord`, which is what makes the character after one a boundary.
/// ASCII only: the DP below is the sole caller and runs on nothing else.
fn char_class(byte: u8) -> CharClass {
    match byte {
        b'a'..=b'z' => CharClass::Lower,
        b'A'..=b'Z' => CharClass::Upper,
        b'0'..=b'9' => CharClass::Number,
        _ => CharClass::NonWord,
    }
}

fn bonus_for(previous: CharClass, current: CharClass) -> i32 {
    if previous == CharClass::NonWord && current != CharClass::NonWord {
        return BONUS_BOUNDARY;
    }
    if (previous == CharClass::Lower && current == CharClass::Upper)
        || (previous != CharClass::Number && current == CharClass::Number)
    {
        return BONUS_CAMEL_OR_NUMBER;
    }
    if current == CharClass::NonWord { BONUS_NON_WORD } else { 0 }
}

fn try_skip(input: &[u8], case_sensitive: bool, wanted: u8, start: usize) -> Option<usize> {
    let upper = (!case_sensitive && wanted.is_ascii_lowercase()).then(|| wanted - 32);
    (start..input.len()).find(|&index| input[index] == wanted || Some(input[index]) == upper)
}

/// Where the scan may start: one character before the earliest in-order occurrence, so the
/// character preceding the first possible match still sets its word class. `None` when the needle
/// is not a subsequence at all, which rejects most candidates before the DP allocates anything.
fn ascii_fuzzy_index(input: &[u8], pattern: &[u8], case_sensitive: bool) -> Option<usize> {
    let mut search = 0;
    let mut first_match_start = 0;
    for (pattern_index, &wanted) in pattern.iter().enumerate() {
        search = try_skip(input, case_sensitive, wanted, search)?;
        if pattern_index == 0 && search > 0 {
            first_match_start = search - 1;
        }
        search += 1;
    }
    Some(first_match_start)
}

/// fzf's `FuzzyMatchV2` on ASCII, returning `(score, start)`.
fn fuzzy_match_v2(case_sensitive: bool, input: &[u8], pattern: &[u8]) -> Option<(i32, usize)> {
    let match_start = ascii_fuzzy_index(input, pattern, case_sensitive)?;

    let mut first_row_scores = vec![0i16; input.len()];
    let mut first_row_consecutive = vec![0i16; input.len()];
    let mut bonuses = vec![0i16; input.len()];
    let mut first_match_by_pattern = vec![0usize; pattern.len()];
    let mut folded = input.to_vec();

    let first_pattern_byte = pattern[0];
    let mut current_pattern_byte = first_pattern_byte;
    let mut max_score = 0;
    let mut max_score_index = 0;
    let mut pattern_index = 0;
    let mut last_match_index = 0;
    let mut previous_class = CharClass::NonWord;
    let mut previous_score = 0;
    let mut in_gap = false;

    for input_index in match_start..input.len() {
        let mut byte = folded[input_index];
        let current_class = char_class(byte);
        if !case_sensitive && current_class == CharClass::Upper {
            byte += 32;
        }
        folded[input_index] = byte;
        bonuses[input_index] = bonus_for(previous_class, current_class) as i16;
        previous_class = current_class;

        if byte == current_pattern_byte {
            if pattern_index < pattern.len() {
                first_match_by_pattern[pattern_index] = input_index;
                pattern_index += 1;
                current_pattern_byte = pattern[pattern_index.min(pattern.len() - 1)];
            }
            // Keeps advancing past a complete subsequence: the DP's window ends at the last
            // occurrence of the needle's final character, not the earliest one that finishes it.
            // Stopping early hides a better run further along, which is most of a long haystack.
            last_match_index = input_index;
        }

        if byte == first_pattern_byte {
            let score = SCORE_MATCH + i32::from(bonuses[input_index]) * BONUS_FIRST_CHAR_MULTIPLIER;
            first_row_scores[input_index] = score as i16;
            first_row_consecutive[input_index] = 1;
            if pattern.len() == 1 && score > max_score {
                max_score = score;
                max_score_index = input_index;
                // A boundary hit by a one-character needle is the best this candidate can do.
                if i32::from(bonuses[input_index]) == BONUS_BOUNDARY {
                    break;
                }
            }
            in_gap = false;
        } else {
            let gap = if in_gap { SCORE_GAP_EXTENSION } else { SCORE_GAP_START };
            first_row_scores[input_index] = (previous_score + gap).max(0) as i16;
            first_row_consecutive[input_index] = 0;
            in_gap = true;
        }
        previous_score = i32::from(first_row_scores[input_index]);
    }

    if pattern_index != pattern.len() {
        return None;
    }
    if pattern.len() == 1 {
        return Some((max_score, max_score_index));
    }
    Some(score_multi_byte_match(
        pattern,
        &folded,
        &bonuses,
        &first_row_scores,
        &first_row_consecutive,
        &first_match_by_pattern,
        last_match_index,
        max_score,
    ))
}

/// The remaining rows of the DP. Two arrays rather than a full matrix per row: each cell needs only
/// the cell left of it and the one diagonally back, and the row above is the previous slice.
#[allow(clippy::too_many_arguments)]
fn score_multi_byte_match(
    pattern: &[u8],
    input: &[u8],
    bonuses: &[i16],
    first_row_scores: &[i16],
    first_row_consecutive: &[i16],
    first_match_by_pattern: &[usize],
    last_match_index: usize,
    initial_max_score: i32,
) -> (i32, usize) {
    let first_match_index = first_match_by_pattern[0];
    let width = last_match_index - first_match_index + 1;
    let mut scores = vec![0i16; width * pattern.len()];
    let mut consecutive_matches = vec![0i16; width * pattern.len()];
    let mut max_score = initial_max_score;

    scores[..width].copy_from_slice(&first_row_scores[first_match_index..=last_match_index]);
    consecutive_matches[..width].copy_from_slice(&first_row_consecutive[first_match_index..=last_match_index]);

    for pattern_index in 1..pattern.len() {
        let first_input_index = first_match_by_pattern[pattern_index];
        let pattern_byte = pattern[pattern_index];
        let row_offset = pattern_index * width;
        let mut in_gap = false;

        for relative_index in 0..=(last_match_index - first_input_index) {
            let input_index = relative_index + first_input_index;
            let cell = row_offset + input_index - first_match_index;
            let gap = if in_gap { SCORE_GAP_EXTENSION } else { SCORE_GAP_START };
            let left = if relative_index > 0 { i32::from(scores[cell - 1]) } else { 0 } + gap;
            let mut diagonal = 0;
            let mut consecutive = 0i16;

            if pattern_byte == input[input_index] {
                // `first_match_by_pattern` strictly increases, so this never reaches behind the row.
                let previous_cell = cell - width - 1;
                diagonal = i32::from(scores[previous_cell]) + SCORE_MATCH;
                consecutive = consecutive_matches[previous_cell] + 1;

                let mut bonus = i32::from(bonuses[input_index]);
                if consecutive > 1 {
                    let run_bonus = i32::from(bonuses[input_index + 1 - consecutive as usize]);
                    if bonus >= BONUS_BOUNDARY && bonus > run_bonus {
                        consecutive = 1; // a boundary starts a better run than the one in progress.
                    } else {
                        bonus = bonus.max(BONUS_CONSECUTIVE.max(run_bonus));
                    }
                }
                // A run's bonus is kept only when it beats stepping sideways; otherwise the plain
                // one applies and the run ends, so it cannot claim credit the path did not take.
                if diagonal + bonus < left {
                    diagonal += i32::from(bonuses[input_index]);
                    consecutive = 0;
                } else {
                    diagonal += bonus;
                }
            }

            consecutive_matches[cell] = consecutive;
            in_gap = diagonal < left;
            scores[cell] = diagonal.max(left).max(0) as i16;
            if pattern_index == pattern.len() - 1 && i32::from(scores[cell]) > max_score {
                max_score = i32::from(scores[cell]);
            }
        }
    }

    (max_score, first_match_index)
}

/// The non-ASCII path, greedy rather than the DP: one pass taking the earliest occurrence of each
/// needle character, with the boundary and consecutive bonuses but no search for a better path.
///
/// ponytail: this is the mirror's own `fuzzyMatchUnicode` and carries its ceiling. Scores are not
/// comparable with the DP's, so a list mixing ASCII and non-ASCII names orders the two groups by
/// slightly different rules. Widening `fuzzy_match_v2` to `char` fixes it at the cost of an index
/// map; nothing in this config has non-ASCII application names to make that pay.
fn fuzzy_match_unicode(case_sensitive: bool, input: &str, pattern: &str) -> Option<(i32, usize)> {
    // `start` is a byte offset into the unfolded haystack, as on the ASCII path; lowercasing can
    // turn one char into several.
    let offsets: Vec<usize> = input
        .char_indices()
        .flat_map(|(at, c)| std::iter::repeat_n(at, if case_sensitive { 1 } else { c.to_lowercase().count() }))
        .collect();
    let fold = |text: &str| -> Vec<char> {
        if case_sensitive { text.chars().collect() } else { text.to_lowercase().chars().collect() }
    };
    let input = fold(input);
    let pattern = fold(pattern);

    let mut input_index = 0;
    let mut first_index = None;
    let mut previous_index: i64 = -2;
    let mut score = 0;

    for &wanted in &pattern {
        while input_index < input.len() && input[input_index] != wanted {
            input_index += 1;
        }
        if input_index >= input.len() {
            return None;
        }
        first_index.get_or_insert(input_index);

        let previous = input_index.checked_sub(1).map(|index| input[index]);
        let at_boundary = previous.is_none_or(|character| character.to_uppercase().eq(character.to_lowercase()));
        score += SCORE_MATCH;
        if input_index as i64 == previous_index + 1 {
            score += BONUS_CONSECUTIVE;
        } else if at_boundary {
            score += BONUS_BOUNDARY;
        }
        score += SCORE_GAP_EXTENSION * (input_index as i64 - previous_index - 1).max(0) as i32;
        previous_index = input_index as i64;
        input_index += 1;
    }

    Some((score, offsets[first_index.unwrap_or(0)]))
}

/// Score `needle` against `haystack`: `(score, match start)`, or `None` when it does not match.
///
/// Smart case, as `Fzf.createFinder`'s default: an all-lowercase needle folds the haystack, and one
/// uppercase character anywhere makes the whole comparison exact.
pub fn score(haystack: &str, needle: &str) -> Option<(i32, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    let lowered = needle.to_lowercase();
    let case_sensitive = needle != lowered;
    let needle = if case_sensitive { needle } else { &lowered };

    if haystack.is_ascii() && needle.is_ascii() {
        if needle.len() > haystack.len() {
            return None;
        }
        return fuzzy_match_v2(case_sensitive, haystack.as_bytes(), needle.as_bytes());
    }
    if needle.chars().count() > haystack.chars().count() {
        return None;
    }
    fuzzy_match_unicode(case_sensitive, haystack, needle)
}

/// A bare global rather than a capability: a capability is an async invoke answered by a payload,
/// and this is read inside `computed`s, which must be pure and synchronous (ADR-0021).
///
/// Bytes that are not UTF-8 score as no match rather than raising, since a desktop entry's name is
/// whatever was on disk and one bad file must not take a whole list with it.
pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "fuzzy",
        lua.create_function(|_, (haystack, needle): (mlua::LuaString, mlua::LuaString)| {
            let (Ok(haystack), Ok(needle)) = (haystack.to_str(), needle.to_str()) else {
                return Ok((None, None));
            };
            Ok(match score(&haystack, &needle) {
                Some((score, start)) => (Some(score), Some(start)),
                None => (None, None),
            })
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(haystack: &str, needle: &str) -> i32 {
        score(haystack, needle).expect("expected a match").0
    }

    /// The case the five-tier scorer this replaced got wrong: initials all land on word boundaries,
    /// which is worth more than the same letters buried mid-word. The old tiers put both candidates
    /// in the same bucket and then preferred the shorter name, which is the wrong one.
    #[test]
    fn initials_on_word_boundaries_beat_the_same_letters_found_mid_word() {
        assert!(scored("Visual Studio Code", "vsc") > scored("Advanced Settings Configurator", "vsc"));
    }

    #[test]
    fn a_prefix_beats_the_same_run_found_later_in_the_name() {
        assert!(scored("Files", "fil") > scored("Profile Editor", "fil"));
    }

    /// The DP window once ended at the earliest complete subsequence, so a literal run past it was
    /// never scored: LibreOffice Calc, whose keywords end in `xlsx`, lost "xlsx" to two text
    /// editors holding no x at all beyond `text`.
    #[test]
    fn a_run_late_in_a_long_haystack_is_still_found_past_an_earlier_scattered_subsequence() {
        let decoyed = "Excel Works analyze lists in spreadsheets ods xls xlsx";
        assert_eq!(scored(decoyed, "xlsx"), scored("ods xls xlsx", "xlsx"));
        assert!(scored(decoyed, "xlsx") > scored("Neovim Edit text files Text Editor", "xlsx"));
    }

    #[test]
    fn a_consecutive_run_beats_the_same_characters_spread_apart() {
        assert!(scored("firefox", "fire") > scored("fine resolver", "fire"));
    }

    #[test]
    fn a_camel_hump_is_a_match_position_the_way_a_space_is() {
        assert!(scored("GnuCash", "gc") > scored("genuine cash", "gc"));
    }

    #[test]
    fn characters_out_of_order_are_not_a_match() {
        assert_eq!(score("Firefox", "xof"), None);
        assert_eq!(score("Files", "filesx"), None);
    }

    /// Smart case: lowercase asks for either case, and one capital makes the whole needle exact.
    #[test]
    fn an_uppercase_character_anywhere_makes_the_match_case_sensitive() {
        assert!(score("firefox", "FF").is_none());
        assert!(score("FireFox", "FF").is_some());
        assert!(score("FireFox", "ff").is_some());
    }

    /// An empty needle matches everything at zero, which is what an empty launcher query relies on.
    #[test]
    fn an_empty_needle_matches_at_zero() {
        assert_eq!(score("Firefox", ""), Some((0, 0)));
    }

    /// The greedy path matches, rejects, and folds case on multi-byte input. It is not asserted to
    /// rank like the DP: it scores a boundary above a consecutive run, and every caseless character
    /// counts as a boundary, so `文器` outscores `文件` in one CJK name. That is the ceiling named
    /// on `fuzzy_match_unicode`, carried over from the mirror, not a defect of this port.
    #[test]
    fn a_non_ascii_haystack_takes_the_greedy_path_rather_than_failing() {
        assert!(scored("Дисковая утилита", "ду") > 0);
        assert!(scored("文件管理器", "文件") > 0);
        assert_eq!(score("Дисковая утилита", "zz"), None);
        assert!(score("дисковая утилита", "ДИ").is_none(), "an uppercase needle stays exact here too");
        assert!(score("Дисковая утилита", "ди").is_some());
    }

    /// `start` is the mirror's first tiebreaker, so it has to be the first matched position.
    #[test]
    fn start_is_where_the_match_begins() {
        assert_eq!(score("a Firefox", "fire").map(|(_, start)| start), Some(2));
    }

    /// A byte offset on the greedy path too, or `string.sub` slices mid-codepoint.
    #[test]
    fn a_non_ascii_start_is_a_byte_offset_into_the_original_haystack() {
        assert_eq!(score("Дисковая Утилита", "ут").map(|(_, start)| start), Some("Дисковая ".len()));
        assert_eq!(score("İİ ab", "ab").map(|(_, start)| start), Some("İİ ".len()));
    }
}
