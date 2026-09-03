//! Resolves the shell's declared font chain to concrete font files and loads only those
//! into a `fontdb::Database` (ADR-0043 decision 2).
//!
//! `cosmic_text::FontSystem::new()` calls `Database::load_system_fonts()`, which parses face
//! metadata for the whole system set (2648 faces on the dev machine ADR-0043 measured against,
//! roughly a second of cold-cache I/O) to use a handful of them. This module is the fix: ask
//! fontconfig which file backs each requested family, and load only those files. The chain is
//! then handed to both cosmic-text (measurement, `text::shaping`) and femtovg (paint,
//! `text::atlas`), which is the other half of the defect this closes -- see `shaping::shape`'s
//! doc comment for the "measured one font, painted a different one" bug this replaces.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use fontdb::{Database, Family, Query};

/// Generic CSS family names fontconfig always resolves by substitution -- there is no literal
/// "sans-serif" family to compare a match against, so the miss check (does the resolved family
/// equal what was asked for) doesn't apply: whatever fontconfig returns for them *is* the answer.
const GENERIC_ALIASES: &[&str] = &["sans-serif", "serif", "monospace", "cursive", "fantasy"];

/// What a config that declares no `fonts { ... }` gets: a system sans serif, then CJK coverage,
/// then color emoji (ADR-0043 decision 2), covering codepoints that arrive from outside the
/// shell's own strings (MPRIS titles, notification bodies, window titles).
///
/// This is the fallback, not the chain. `crate::lua::fonts` records what a config asked for and
/// `ShapingHandle::set_chain` installs it after the startup evaluation. A shell drawing its chrome
/// with Nerd Font private-use glyphs has to declare one: none of the three families here carries
/// those codepoints, so without a declaration they render as tofu.
pub const DEFAULT_CHAIN: &[&str] = &["sans-serif", "Noto Sans CJK JP", "Noto Color Emoji"];

/// A font chain resolved to loaded faces: the database holding exactly those faces, and the
/// family name shaping asks cosmic-text for via `Family::Name`.
pub struct ResolvedFonts {
    pub db: Database,
    pub primary_family: String,
}

/// Resolves `chain` in order and loads every entry that hits into one `Database`.
///
/// A chain entry that fontconfig can't honor (not installed, or genuinely absent) is skipped,
/// not silently substituted -- see `fc_match`'s doc comment for how a "hit" is told apart from
/// fontconfig's own substitution, which never fails on its own.
///
/// Runs once at process startup, so an `eprintln!` per chain entry costs nothing and is how a
/// user diagnoses "why is my bar drawing in the wrong font" without reaching for a debugger.
pub fn resolve_chain(chain: &[&str]) -> ResolvedFonts {
    let mut db = Database::new();
    let mut primary_family: Option<String> = None;
    let mut loaded_paths: HashSet<PathBuf> = HashSet::new();

    for &name in chain {
        let Some((path, resolved_family)) = fc_match(name) else {
            eprintln!("font chain: {name:?} -> no installed match, skipped");
            continue;
        };
        eprintln!("font chain: {name:?} -> {path:?} ({resolved_family:?})");

        // Two chain entries can resolve to the same file (e.g. a generic alias and the literal
        // family name it happens to expand to) -- load it once.
        if !loaded_paths.contains(&path) {
            match db.load_font_file(&path) {
                Ok(()) => {
                    loaded_paths.insert(path);
                }
                Err(e) => {
                    eprintln!("font chain: {name:?} resolved to {path:?}, which failed to load: {e}");
                    continue;
                }
            }
        }

        if primary_family.is_none() {
            load_variants(&mut db, name, &resolved_family, &mut loaded_paths);
            primary_family = Some(resolved_family);
        }
    }

    match primary_family {
        Some(primary_family) => ResolvedFonts { db, primary_family },
        None => system_fallback(chain),
    }
}

/// Loads the primary family's bold, italic and bold-italic files, when fontconfig has them, so a
/// styled run (ADR-0104) is shaped and painted in a real face rather than the regular one standing
/// in. Only for the primary: a fallback entry is there for codepoint coverage and nothing asks it
/// for a weight.
///
/// Three more `fc-match` calls at startup, each asking for `family:weight=bold` and the like. A
/// family shipped as one `.ttc` resolves every variant to the file already loaded, and
/// `loaded_paths` makes that free; one shipped as separate files loads each. fontconfig answers a
/// variant the family does not have with the nearest face it does, which is the regular file again
/// or a different family entirely -- the same family check `fc_match` applies everywhere rejects the
/// latter, and the former is a duplicate path and skipped.
fn load_variants(db: &mut Database, name: &str, resolved_family: &str, loaded_paths: &mut HashSet<PathBuf>) {
    for (variant, pattern) in
        [("bold", "weight=bold"), ("italic", "slant=italic"), ("bold italic", "weight=bold:slant=italic")]
    {
        let Some((path, family)) = fc_match(&format!("{name}:{pattern}")) else {
            continue;
        };
        if !family.eq_ignore_ascii_case(resolved_family) || loaded_paths.contains(&path) {
            continue;
        }
        match db.load_font_file(&path) {
            Ok(()) => {
                eprintln!("font chain: {name:?} {variant} -> {path:?}");
                loaded_paths.insert(path);
            }
            Err(e) => eprintln!("font chain: {name:?} {variant} resolved to {path:?}, which failed to load: {e}"),
        }
    }
}

/// The whole chain missed -- no fontconfig installed, `fc-match` missing from `$PATH`, or every
/// entry genuinely absent. A real degradation path, not a panic.
///
/// ponytail: this is `load_system_fonts()` plus a `SansSerif` query, exactly the ~1s-startup,
/// whole-system-scan path ADR-0043 decision 2 spends this module avoiding. Correct only
/// because it's the last resort, not the common case -- if this line shows up in a real user's
/// logs, the fix is diagnosing why `fc-match` isn't reachable, not tuning this fallback.
fn system_fallback(chain: &[&str]) -> ResolvedFonts {
    eprintln!(
        "font chain: none of {chain:?} resolved via fc-match (fontconfig not installed, or \
         fc-match missing from $PATH) -- falling back to a full system font scan, which is \
         the slow, memory-heavy path ADR-0043 decision 2 exists to avoid"
    );
    let mut db = Database::new();
    db.load_system_fonts();
    let query = Query { families: &[Family::SansSerif], ..Default::default() };
    let id = db
        .query(&query)
        .or_else(|| db.faces().next().map(|face| face.id))
        .expect("fontdb has no loaded faces at all, system fallback exhausted");
    let primary_family =
        db.face(id).expect("queried id must be in the database that produced it").families[0].0.clone();
    ResolvedFonts { db, primary_family }
}

/// Runs `fc-match` for `name` and returns the file it resolved to plus the family fontconfig
/// actually gave back, or `None` if that doesn't count as a hit.
///
/// fontconfig substitutes rather than failing: `fc-match "ZZ No Such Family"` still exits 0 and
/// returns a real font, so a nonempty result alone can't tell a hit from a miss. The check is
/// whether one of the returned families (comma-separated for a multi-name face) matches what was
/// asked for, case-insensitively -- except `GENERIC_ALIASES`, where substitution is intended and
/// any result counts.
///
/// ponytail: one `fc-match` subprocess per chain entry, roughly three at startup, a few
/// milliseconds total. Linking libfontconfig directly (the `fontconfig-parser` crate fontdb's
/// own `fontconfig` feature already pulls in) is the fix if that ever shows up in a profile.
/// A subprocess is preferred today for two reasons: it adds no new crate to this workspace, and
/// fontconfig's own configuration file is the system's declared font policy -- a better default
/// than "first face in scan order", which is what this whole module replaces.
fn fc_match(name: &str) -> Option<(PathBuf, String)> {
    let output = Command::new("fc-match").args(["-f", "%{file}\t%{family}\n", name]).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next()?;
    let (file, families) = line.split_once('\t')?;
    if file.is_empty() {
        return None;
    }

    // `Family:weight=bold` is fontconfig's own pattern syntax and the family is the part before
    // the colon; a bare family name has no colon and is itself.
    let family_asked = name.split(':').next().unwrap_or(name);
    let is_generic = GENERIC_ALIASES.iter().any(|generic| generic.eq_ignore_ascii_case(family_asked));
    let resolved_family = if is_generic {
        families.split(',').next().unwrap_or(families).trim().to_string()
    } else {
        let hit = families.split(',').find(|family| family.trim().eq_ignore_ascii_case(family_asked))?;
        hit.trim().to_string()
    };

    Some((PathBuf::from(file), resolved_family))
}

/// True if `fc-match` is reachable at all -- gates this module's tests and `text::shaping`'s:
/// print a skip line and return, don't fail a machine or CI image without fontconfig installed.
/// `pub(crate)` since `text::shaping`'s tests need the same gate.
#[cfg(test)]
pub(crate) fn fc_match_available() -> bool {
    Command::new("fc-match").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// True if `fc-list` finds at least one face for `family`. Unlike `fc-match`, `fc-list` never
/// substitutes -- an absent family prints nothing rather than someone else's font -- so this
/// gates a test on "is this specific family actually here", which varies by machine (no bundled
/// dev font set in this workspace). `pub(crate)` for the same reason as `fc_match_available`.
#[cfg(test)]
pub(crate) fn family_installed(family: &str) -> bool {
    Command::new("fc-list").args([family, "file"]).output().is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picked because it happens to be installed on machines this runs on, and resolves to
    /// itself rather than a substitute -- both checked via `family_installed`/`resolve_chain`
    /// rather than assumed. There is no bundled dev font set in this workspace, so a hardcoded
    /// family must be verified, not assumed present.
    const TEST_FAMILY: &str = "Noto Sans Mono";

    #[test]
    fn an_installed_family_resolves_to_a_file_whose_family_matches_the_request() {
        if !fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        if !family_installed(TEST_FAMILY) {
            eprintln!("{TEST_FAMILY:?} not installed on this machine, skip");
            return;
        }
        let resolved = resolve_chain(&[TEST_FAMILY]);
        assert_eq!(resolved.primary_family, TEST_FAMILY);
        // Not an exact count: a `.ttc` match legitimately loads more than one face.
        assert!(!resolved.db.is_empty(), "expected at least one loaded face");
    }

    #[test]
    fn an_uninstalled_family_is_skipped_rather_than_silently_substituted() {
        if !fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        if !family_installed(TEST_FAMILY) {
            eprintln!("{TEST_FAMILY:?} not installed on this machine, skip");
            return;
        }
        // If the miss were silently accepted, `primary_family` would be whatever fontconfig
        // picked for the absent name instead of the real hit that follows it.
        let resolved = resolve_chain(&["ZZ No Such Family 9184", TEST_FAMILY]);
        assert_eq!(resolved.primary_family, TEST_FAMILY);
        assert!(!resolved.db.is_empty(), "the real hit's own face should have loaded");
    }

    #[test]
    fn the_default_chain_loads_only_its_own_faces_not_the_system_set() {
        if !fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        let resolved = resolve_chain(DEFAULT_CHAIN);
        // The system has 2648 faces on the machine ADR-0043 measured against; three requests
        // stay well under 100 -- not a fragile exact count.
        assert!(
            resolved.db.faces().count() < 100,
            "expected a small chain-sized database, got {} faces",
            resolved.db.faces().count()
        );
        assert!(!resolved.primary_family.is_empty());
    }
}
