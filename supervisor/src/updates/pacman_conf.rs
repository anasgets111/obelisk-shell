//! `pacman.conf`/mirrorlist parsing for `oblisk.updates` (ADR-0034): the real, non-hardcoded
//! repo list and mirror server set this machine's pacman is actually configured with. This
//! dev machine's real `/etc/pacman.conf` has repos in two forms -- most `Include =` a
//! mirrorlist file, `omarchy` inlines a `Server =` line directly -- both handled here.

use std::path::Path;

/// One configured repo's resolved, `$repo`/`$arch`-substituted mirror server URLs, ready to
/// hand to `alpm::Db::add_server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoServers {
    pub name: String,
    pub servers: Vec<String>,
}

fn strip_comment_and_trim(line: &str) -> &str {
    line.trim()
}

/// Parses `text` (real `pacman.conf` syntax) into every non-`[options]` section's name, its
/// `Include = ` paths, and its inline `Server = ` lines, in file order. Pure -- directly
/// testable against literal `pacman.conf` text without needing real mirrorlist files.
fn parse_pacman_conf(text: &str) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut repos = Vec::new();
    let mut current: Option<(String, Vec<String>, Vec<String>)> = None;

    for line in text.lines() {
        let line = strip_comment_and_trim(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            if let Some(finished) = current.take()
                && finished.0 != "options"
            {
                repos.push(finished);
            }
            current = Some((name.to_string(), Vec::new(), Vec::new()));
            continue;
        }
        let Some((_, includes, inline_servers)) = current.as_mut() else { continue };
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "Include" => includes.push(value.trim().to_string()),
            "Server" => inline_servers.push(value.trim().to_string()),
            _ => {}
        }
    }
    if let Some(finished) = current.take()
        && finished.0 != "options"
    {
        repos.push(finished);
    }
    repos
}

/// Extracts every `Server = ` line's value from a mirrorlist file's text -- same key parsing as
/// `parse_pacman_conf`'s inline-`Server` case, applied to a standalone mirrorlist file instead
/// of a `pacman.conf` section.
fn parse_mirrorlist(text: &str) -> Vec<String> {
    text.lines()
        .map(strip_comment_and_trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| key.trim() == "Server")
        .map(|(_, value)| value.trim().to_string())
        .collect()
}

fn substitute(template: &str, repo_name: &str) -> String {
    template.replace("$repo", repo_name).replace("$arch", std::env::consts::ARCH)
}

/// Parses `pacman_conf_path` and resolves every configured repo's full mirror server list --
/// `Server =` lines directly under its section, plus every `Include =` file's own `Server =`
/// lines. No root-injection parameter needed: an `Include` path in real `pacman.conf` is
/// already absolute. Silently skips a repo whose `Include` file can't be read.
pub fn resolve_repo_servers(pacman_conf_path: &Path) -> Vec<RepoServers> {
    let Ok(text) = std::fs::read_to_string(pacman_conf_path) else { return Vec::new() };

    parse_pacman_conf(&text)
        .into_iter()
        .map(|(name, includes, inline_servers)| {
            let mut servers = inline_servers;
            for include_path in includes {
                if let Ok(include_text) = std::fs::read_to_string(&include_path) {
                    servers.extend(parse_mirrorlist(&include_text));
                }
            }
            let servers = servers.iter().map(|template| substitute(template, &name)).collect();
            RepoServers { name, servers }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_pacman_conf ----

    #[test]
    fn parse_pacman_conf_skips_the_options_section() {
        let text = "[options]\nArchitecture = auto\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";
        let repos = parse_pacman_conf(text);
        assert_eq!(repos, vec![("core".to_string(), vec!["/etc/pacman.d/mirrorlist".to_string()], vec![])]);
    }

    #[test]
    fn parse_pacman_conf_collects_multiple_repos_in_file_order() {
        let text = "[options]\n[core]\nInclude = /a\n[extra]\nInclude = /b\n[multilib]\nInclude = /c\n";
        let repos = parse_pacman_conf(text);
        let names: Vec<&str> = repos.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, vec!["core", "extra", "multilib"]);
    }

    #[test]
    fn parse_pacman_conf_collects_an_inline_server_line_with_no_include() {
        // This dev machine's real `[omarchy]` section -- confirmed, not hypothetical.
        let text = "[omarchy]\nSigLevel = Required DatabaseOptional\nServer = https://pkgs.omarchy.org/edge/$arch\n";
        let repos = parse_pacman_conf(text);
        assert_eq!(repos, vec![("omarchy".to_string(), vec![], vec!["https://pkgs.omarchy.org/edge/$arch".to_string()])]);
    }

    #[test]
    fn parse_pacman_conf_ignores_comments_and_blank_lines() {
        let text = "# a comment\n\n[core]\n# another comment\nInclude = /etc/pacman.d/mirrorlist\n\n";
        let repos = parse_pacman_conf(text);
        assert_eq!(repos, vec![("core".to_string(), vec!["/etc/pacman.d/mirrorlist".to_string()], vec![])]);
    }

    #[test]
    fn parse_pacman_conf_is_empty_for_only_an_options_section() {
        let text = "[options]\nArchitecture = auto\nColor\n";
        assert!(parse_pacman_conf(text).is_empty());
    }

    // ---- parse_mirrorlist ----

    #[test]
    fn parse_mirrorlist_extracts_every_uncommented_server_line() {
        let text = "# comment\nServer = https://one.example/$repo/os/$arch\n#Server = https://commented.example/$repo/os/$arch\nServer = https://two.example/$repo/os/$arch\n";
        assert_eq!(parse_mirrorlist(text), vec!["https://one.example/$repo/os/$arch".to_string(), "https://two.example/$repo/os/$arch".to_string()]);
    }

    // ---- substitute ----

    #[test]
    fn substitute_replaces_repo_and_arch_placeholders() {
        assert_eq!(substitute("https://example/$repo/os/$arch", "core"), format!("https://example/core/os/{}", std::env::consts::ARCH));
    }

    // ---- resolve_repo_servers (real fs I/O against a tempdir -- docs/oblisk-tdd-test-harness.md's convention) ----

    #[test]
    fn resolve_repo_servers_follows_a_real_include_file() {
        let dir = tempfile::tempdir().unwrap();
        let mirrorlist_path = dir.path().join("mirrorlist");
        std::fs::write(&mirrorlist_path, "Server = https://example.test/$repo/os/$arch\n").unwrap();

        let conf_path = dir.path().join("pacman.conf");
        std::fs::write(&conf_path, format!("[options]\n[core]\nInclude = {}\n", mirrorlist_path.display())).unwrap();

        let repos = resolve_repo_servers(&conf_path);
        assert_eq!(repos, vec![RepoServers { name: "core".to_string(), servers: vec![format!("https://example.test/core/os/{}", std::env::consts::ARCH)] }]);
    }

    #[test]
    fn resolve_repo_servers_handles_an_inline_server_with_no_include() {
        let dir = tempfile::tempdir().unwrap();
        let conf_path = dir.path().join("pacman.conf");
        std::fs::write(&conf_path, "[omarchy]\nServer = https://pkgs.example/$arch\n").unwrap();

        let repos = resolve_repo_servers(&conf_path);
        assert_eq!(repos, vec![RepoServers { name: "omarchy".to_string(), servers: vec![format!("https://pkgs.example/{}", std::env::consts::ARCH)] }]);
    }

    #[test]
    fn resolve_repo_servers_is_empty_for_a_missing_pacman_conf() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_repo_servers(&dir.path().join("does-not-exist.conf")).is_empty());
    }

    #[test]
    fn resolve_repo_servers_gives_a_repo_zero_servers_when_its_include_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let conf_path = dir.path().join("pacman.conf");
        std::fs::write(&conf_path, format!("[core]\nInclude = {}\n", dir.path().join("does-not-exist").display())).unwrap();

        let repos = resolve_repo_servers(&conf_path);
        assert_eq!(repos, vec![RepoServers { name: "core".to_string(), servers: vec![] }]);
    }
}
