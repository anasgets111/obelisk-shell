//! Resolves the shell's declared font chain to concrete font files and loads only those
//! into a `fontdb::Database` (docs/adr/0043 decision 2, docs/build-steps.md Phase 19 item 10).
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
/// "sans-serif" family to compare a match against, so decision 2's miss check (does the
/// resolved family equal what was asked for) doesn't apply to these: whatever fontconfig
/// returns for them *is* the answer.
const GENERIC_ALIASES: &[&str] = &["sans-serif", "serif", "monospace", "cursive", "fantasy"];

/// Until the Lua `fonts = {...}` declaration lands (a separate commit -- its return shape isn't
/// settled), every surface resolves against this chain: a system sans serif, then CJK coverage,
/// then color emoji. Matches the shape docs/adr/0043 decision 2 names as what the default
/// config ships, covering the codepoints that arrive from outside the shell's own strings
/// (MPRIS track titles, notification bodies, window titles).
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
/// not silently substituted -- see `fc_match`'s own doc comment for how a "hit" is told apart
/// from fontconfig's own substitution, which never fails on its own.
///
/// Runs once at process startup (`ShapingHandle::spawn`'s worker calls this exactly once, not
/// per frame), so an `eprintln!` per chain entry costs nothing and is genuinely useful: it is
/// how a user diagnoses "why is my bar drawing in the wrong font" without reaching for a
/// debugger, matching every miss `fc_match` reports against the exact family name they wrote.
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
            primary_family = Some(resolved_family);
        }
    }

    match primary_family {
        Some(primary_family) => ResolvedFonts { db, primary_family },
        None => system_fallback(chain),
    }
}

/// The whole chain missed -- no fontconfig installed, `fc-match` missing from `$PATH`, or every
/// entry genuinely absent. A real degradation path, not a panic: fall back to the full system
/// scan this module exists to avoid, so the shell still draws text.
///
/// ponytail: this is `load_system_fonts()` plus a `SansSerif` query, exactly the ~1s-startup,
/// whole-system-scan path docs/adr/0043 decision 2 spends this module avoiding. Correct only
/// because it's the last resort, not the common case -- if this line shows up in a real user's
/// logs, the fix is diagnosing why `fc-match` isn't reachable, not tuning this fallback.
fn system_fallback(chain: &[&str]) -> ResolvedFonts {
    eprintln!(
        "font chain: none of {chain:?} resolved via fc-match (fontconfig not installed, or \
         fc-match missing from $PATH) -- falling back to a full system font scan, which is \
         the slow, memory-heavy path docs/adr/0043 decision 2 exists to avoid"
    );
    let mut db = Database::new();
    db.load_system_fonts();
    let query = Query { families: &[Family::SansSerif], ..Default::default() };
    let id = db.query(&query).or_else(|| db.faces().next().map(|face| face.id)).expect("fontdb has no loaded faces at all, system fallback exhausted");
    let primary_family = db.face(id).expect("queried id must be in the database that produced it").families[0].0.clone();
    ResolvedFonts { db, primary_family }
}

/// Runs `fc-match` for `name` and returns the file it resolved to plus the family fontconfig
/// actually gave back, or `None` if that doesn't count as a hit.
///
/// fontconfig substitutes rather than failing: `fc-match "ZZ No Such Family"` still exits 0 and
/// returns a real font (commonly a generic default), so a nonempty result alone can't tell a hit
/// from a miss. The check is whether one of the returned families (fontconfig prints a
/// comma-separated list for a multi-name face) matches what was asked for, case-insensitively --
/// except the generic aliases in `GENERIC_ALIASES`, where substitution *is* the intended
/// behavior and any result counts.
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

    let is_generic = GENERIC_ALIASES.iter().any(|generic| generic.eq_ignore_ascii_case(name));
    let resolved_family = if is_generic {
        families.split(',').next().unwrap_or(families).trim().to_string()
    } else {
        let hit = families.split(',').find(|family| family.trim().eq_ignore_ascii_case(name))?;
        hit.trim().to_string()
    };

    Some((PathBuf::from(file), resolved_family))
}

/// True if `fc-match` is reachable at all -- gates this module's own tests, and
/// `text::shaping`'s, the same way the paint module's surfaceless-EGL harness gates on EGL
/// init: print a skip line and return, don't fail a developer machine or CI image that simply
/// doesn't have fontconfig installed. `pub(crate)` rather than private to this module's own
/// `mod tests`, since `text::shaping`'s tests need the same gate and there is no reason to
/// duplicate it.
#[cfg(test)]
pub(crate) fn fc_match_available() -> bool {
    Command::new("fc-match").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// True if `fc-list` finds at least one face for `family`. Unlike `fc-match`, `fc-list` never
/// substitutes -- an absent family prints nothing rather than someone else's font -- so this is
/// the right tool to gate a test on "is this specific family actually here", which varies by
/// machine (there is no bundled dev font set in this workspace; every font these tests can name
/// is whatever the machine running them happens to have installed). `pub(crate)` for the same
/// reason as `fc_match_available` above.
#[cfg(test)]
pub(crate) fn family_installed(family: &str) -> bool {
    Command::new("fc-list").args([family, "file"]).output().is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picked only because it happens to be installed on the machines this has been run on so
    /// far, and resolves to itself rather than to a substitute -- both properties this module's
    /// tests check for via `family_installed`/`resolve_chain` rather than assuming. A test that
    /// hardcodes a family without that check breaks the moment it runs somewhere else, which is
    /// exactly what happened here: an earlier version of this file claimed this family "ships
    /// with this workspace's dev fonts" (there is no such thing) to explain away a machine-local
    /// fontconfig quirk that made a *different* installed family resolve to a substitute.
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
        // Not an exact count: if this family happened to resolve to a `.ttc` on some machine,
        // one request still legitimately loads more than one face (`text::shaping`'s
        // `font_chain_bytes` dedups by file, not by face, precisely because of this).
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
        // fontconfig will happily substitute something else for the absent name -- the chain
        // entry after it is what proves the substitution was rejected rather than accepted:
        // if the miss were silently accepted, its substituted file would load too, and
        // `primary_family` would be whatever fontconfig picked for the absent name instead of
        // the real hit that follows it.
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
        // The system has 2648 faces on the machine ADR-0043 measured against; this chain is
        // three requests, each resolving to at most a handful of faces (a collection file could
        // hold more than one). Well under 100 either way -- not a fragile exact count.
        assert!(
            resolved.db.faces().count() < 100,
            "expected a small chain-sized database, got {} faces",
            resolved.db.faces().count()
        );
        assert!(!resolved.primary_family.is_empty());
    }
}
