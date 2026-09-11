//! `pacman` half of `updates:install()` (ADR-0034): progress syntax and reboot heuristic. The
//! command is `pkexec pacman -Syu --noconfirm`, run by `controller.rs` through
//! `process::spawn_group_leader_piped`; `pkexec` talks to polkit and triggers the Obelisk polkit
//! agent's interactive prompt rather than a manual `CheckAuthorization` call. It modifies the real
//! `/etc/pacman.conf`/`/var/lib/pacman` as root, unlike `check.rs`.
//!
//! Progress parsing is best-effort against pacman's real format
//! (`"(2/5) installing nss (3.127-1 -> 3.128-1)"`), not end-to-end verified because a privileged
//! run needs the user's polkit prompt. A missed line affects only UI progress; success comes from
//! process exit status.

use super::super::backend::InstallStep;

/// Parses `(current/total) installing|upgrading|reinstalling <package> ...` install lines. `None`
/// for sync messages, download bars, and blanks; the previous progress remains.
pub fn parse_install_step(line: &str) -> Option<InstallStep> {
    let rest = line.trim().strip_prefix('(')?;
    let (counts, rest) = rest.split_once(')')?;
    let (current, total) = counts.split_once('/')?;
    let current: u32 = current.trim().parse().ok()?;
    let total: u32 = total.trim().parse().ok()?;
    let rest = rest.trim();
    let rest = rest
        .strip_prefix("installing ")
        .or_else(|| rest.strip_prefix("upgrading "))
        .or_else(|| rest.strip_prefix("reinstalling "))?;
    let package = rest.split_whitespace().next()?.to_string();
    Some(InstallStep { current, total, package })
}

/// Whether `package_names` includes `linux` or `linux-<variant>` such as `linux-lts`, `linux-zen`,
/// or `linux-hardened` (ADR-0034 `rebootRequired`). Heuristic only; firmware or glibc can also
/// require reboot without a kernel package.
pub fn needs_reboot(package_names: &[String]) -> bool {
    package_names.iter().any(|name| name == "linux" || name.starts_with("linux-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_install_step ----

    #[test]
    fn parse_install_step_reads_a_real_installing_line() {
        let step = parse_install_step("(2/5) installing nss (3.127-1 -> 3.128-1)").expect("should parse");
        assert_eq!(step, InstallStep { current: 2, total: 5, package: "nss".to_string() });
    }

    #[test]
    fn parse_install_step_reads_an_upgrading_line() {
        let step = parse_install_step("(1/3) upgrading ca-certificates-mozilla").expect("should parse");
        assert_eq!(step, InstallStep { current: 1, total: 3, package: "ca-certificates-mozilla".to_string() });
    }

    #[test]
    fn parse_install_step_reads_a_reinstalling_line() {
        let step = parse_install_step("(1/1) reinstalling linux").expect("should parse");
        assert_eq!(step, InstallStep { current: 1, total: 1, package: "linux".to_string() });
    }

    #[test]
    fn parse_install_step_is_none_for_a_database_sync_line() {
        assert!(parse_install_step(":: Synchronizing package databases...").is_none());
    }

    #[test]
    fn parse_install_step_is_none_for_a_download_progress_line() {
        assert!(parse_install_step("nss-3.128-1-x86_64  1811951 KiB  4.05 MiB/s 00:00:03 [#####] 100%").is_none());
    }

    #[test]
    fn parse_install_step_is_none_for_a_blank_line() {
        assert!(parse_install_step("").is_none());
    }

    #[test]
    fn parse_install_step_is_none_for_malformed_counts() {
        assert!(parse_install_step("(a/b) installing nss").is_none());
    }

    // ---- needs_reboot ----

    #[test]
    fn needs_reboot_is_true_for_the_base_linux_package() {
        assert!(needs_reboot(&["linux".to_string(), "nss".to_string()]));
    }

    #[test]
    fn needs_reboot_is_true_for_a_linux_variant_package() {
        assert!(needs_reboot(&["linux-zen".to_string()]));
    }

    #[test]
    fn needs_reboot_is_false_with_no_kernel_package() {
        assert!(!needs_reboot(&["nss".to_string(), "gnome-autoar".to_string()]));
    }

    #[test]
    fn needs_reboot_does_not_false_positive_on_a_name_merely_starting_with_linux() {
        // No hyphen after `linux`, so this is not a kernel-variant name.
        assert!(!needs_reboot(&["linuxfoo".to_string()]));
    }
}
