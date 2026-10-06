//! Is something on this desktop showing tray icons? (the tray-host check)
//!
//! The tray talks StatusNotifierItem (SNI). Icons only appear when a "watcher" owns the session
//! bus name `org.kde.StatusNotifierWatcher`. KDE provides one. Stock GNOME does not, so there the
//! icon is invisible until the person enables an AppIndicator extension. This module answers
//! "is a watcher there?" ([`probe`]), says what to do if not ([`install_hint`]), and keeps the
//! "tell them once" marker ([`should_show_notice`], [`mark_notice_shown`]).
//!
//! Everything but [`probe`] is pure so it can be tested without a desktop. `main.rs` wiring is a
//! separate task.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Whether a StatusNotifier watcher exists on the session bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Present,
    Missing,
}

/// Turn the result of `NameHasOwner` into a [`HostStatus`].
/// An error (no bus, bus refused, timeout) counts as `Missing`: if the bus cannot be read there
/// is no tray to show, and the app keeps running as a window.
pub fn classify(watcher_has_owner: Result<bool, String>) -> HostStatus {
    match watcher_has_owner {
        Ok(true) => HostStatus::Present,
        Ok(false) | Err(_) => HostStatus::Missing,
    }
}

/// How long [`probe`] waits for the bus before giving up and reporting `Missing`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask the session bus whether `org.kde.StatusNotifierWatcher` has an owner. Never panics;
/// every failure maps to `Missing` through [`classify`]. Read-only (`NameHasOwner`).
pub async fn probe() -> HostStatus {
    async fn ask() -> Result<bool, String> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| e.to_string())?;
        let dbus = zbus::fdo::DBusProxy::new(&conn)
            .await
            .map_err(|e| e.to_string())?;
        let name = zbus::names::BusName::try_from("org.kde.StatusNotifierWatcher")
            .map_err(|e| e.to_string())?;
        dbus.name_has_owner(name).await.map_err(|e| e.to_string())
    }
    // A stuck bus must not hang start-up, so the whole exchange is bounded.
    let answer = match tokio::time::timeout(PROBE_TIMEOUT, ask()).await {
        Ok(r) => r,
        Err(_) => Err("timed out talking to the session bus".to_string()),
    };
    classify(answer)
}

/// Advice for getting a tray host, or `None` when the desktop needs none (KDE).
///
/// `os_release` is the text of `/etc/os-release`; `desktop` is `XDG_CURRENT_DESKTOP`, which may
/// hold several colon-separated names (`ubuntu:GNOME`). Names below were checked on 2026-10-05:
/// * Ubuntu: `apt-cache policy gnome-shell-extension-appindicator` on Ubuntu 24.04 finds the
///   package, and its `metadata.json` (`dpkg -L`) has uuid `ubuntu-appindicators@ubuntu.com`.
///   Ubuntu ships it enabled-on-request, so only the enable command is given.
/// * Fedora: packages.fedoraproject.org lists `gnome-shell-extension-appindicator`; its extension
///   uuid is `appindicatorsupport@rgcjonas.gmail.com`, which matches upstream's `metadata.json`
///   (github.com/ubuntu/gnome-shell-extension-appindicator). Package page fetched through a
///   summarising tool, not read raw; the uuid was cross-checked against upstream.
/// * Anything else: the extension's display name, with no package name guessed.
pub fn install_hint(os_release: &str, desktop: &str) -> Option<String> {
    let desktops: Vec<String> = desktop
        .split(':')
        .map(|d| d.trim().to_ascii_lowercase())
        .collect();
    let has = |n: &str| desktops.iter().any(|d| d == n);
    if has("kde") {
        return None; // Plasma hosts StatusNotifierItem itself.
    }
    let mut ids: Vec<String> = Vec::new();
    for line in os_release.lines() {
        let line = line.trim();
        for key in ["ID_LIKE=", "ID="] {
            if let Some(v) = line.strip_prefix(key) {
                ids.extend(
                    v.trim_matches(|c| c == '"' || c == '\'')
                        .split_whitespace()
                        .map(|w| w.to_ascii_lowercase()),
                );
            }
        }
    }
    let is = |n: &str| ids.iter().any(|i| i == n);
    if has("gnome") && is("ubuntu") {
        return Some(
            "Tray icons need a GNOME extension. Run: gnome-extensions enable ubuntu-appindicators@ubuntu.com \
             (then log out and in if the icon does not appear)."
                .to_string(),
        );
    }
    if has("gnome") && is("fedora") {
        return Some(
            "Tray icons need a GNOME extension. Run: sudo dnf install gnome-shell-extension-appindicator, \
             then: gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com \
             (log out and in if the icon does not appear)."
                .to_string(),
        );
    }
    Some(
        "No system tray was found. On GNOME, install the AppIndicator and KStatusNotifierItem Support \
         extension and enable it. The window works without a tray."
            .to_string(),
    )
}

/// File whose existence means "the person has already been told".
pub fn notice_marker_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tray-host-notice-shown")
}

/// Show the notice only when the host is missing, we have something to say, and we have not
/// said it before.
pub fn should_show_notice(status: HostStatus, hint: &Option<String>, marker_exists: bool) -> bool {
    status == HostStatus::Missing && hint.is_some() && !marker_exists
}

/// Record that the notice was shown. Creates `config_dir` if needed.
///
/// The marker is an empty file; creating it is a single step, so a crash leaves either no marker
/// (the notice shows once more) or a whole one.
pub fn mark_notice_shown(config_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(notice_marker_path(config_dir))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n";
    const FEDORA: &str = "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=45\n";
    const ARCH: &str = "NAME=\"Arch Linux\"\nID=arch\n";

    #[test]
    fn classify_true_is_present() {
        assert_eq!(classify(Ok(true)), HostStatus::Present);
    }
    #[test]
    fn classify_false_is_missing() {
        assert_eq!(classify(Ok(false)), HostStatus::Missing);
    }
    #[test]
    fn classify_error_is_missing() {
        assert_eq!(classify(Err("no bus".into())), HostStatus::Missing);
    }

    #[test]
    fn hint_for_ubuntu_gnome_says_enable_ubuntu_appindicators() {
        let h = install_hint(UBUNTU, "ubuntu:GNOME").unwrap();
        assert!(
            h.contains("gnome-extensions enable ubuntu-appindicators@ubuntu.com"),
            "{h}"
        );
    }
    #[test]
    fn hint_for_fedora_gnome_says_install_the_appindicator_extension_package() {
        let h = install_hint(FEDORA, "GNOME").unwrap();
        assert!(
            h.contains("sudo dnf install gnome-shell-extension-appindicator"),
            "{h}"
        );
        assert!(
            h.contains("gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com"),
            "{h}"
        );
    }
    #[test]
    fn hint_for_kde_is_none() {
        assert_eq!(install_hint(ARCH, "KDE"), None);
        assert_eq!(install_hint(UBUNTU, "KDE"), None);
    }
    #[test]
    fn hint_for_unknown_desktop_is_generic_but_present() {
        let h = install_hint(ARCH, "GNOME").unwrap();
        assert!(
            h.contains("AppIndicator and KStatusNotifierItem Support"),
            "{h}"
        );
        let h = install_hint(ARCH, "").unwrap();
        assert!(
            h.contains("AppIndicator and KStatusNotifierItem Support"),
            "{h}"
        );
    }
    #[test]
    fn ubuntu_derivative_via_id_like_counts_as_ubuntu() {
        let mint = "ID=pop\nID_LIKE=\"ubuntu debian\"\n";
        let h = install_hint(mint, "GNOME").unwrap();
        assert!(h.contains("ubuntu-appindicators@ubuntu.com"), "{h}");
    }
    #[test]
    fn ubuntu_with_non_gnome_desktop_gets_generic_hint() {
        let h = install_hint(UBUNTU, "XFCE").unwrap();
        assert!(!h.contains("ubuntu-appindicators"), "{h}");
    }

    #[test]
    fn notice_shown_only_when_missing_hinted_and_unmarked() {
        let hint = Some("x".to_string());
        assert!(should_show_notice(HostStatus::Missing, &hint, false));
        assert!(!should_show_notice(HostStatus::Present, &hint, false));
        assert!(!should_show_notice(HostStatus::Missing, &None, false));
        assert!(!should_show_notice(HostStatus::Missing, &hint, true));
    }
    #[test]
    fn marker_roundtrip_creates_dir_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("nested/tt-station");
        let p = notice_marker_path(&cfg);
        assert!(p.starts_with(&cfg));
        assert!(!p.exists());
        mark_notice_shown(&cfg).unwrap();
        assert!(p.exists());
        mark_notice_shown(&cfg).unwrap();
        assert!(p.exists());
    }
    #[test]
    fn probe_never_panics() {
        // Read-only NameHasOwner (or a harmless failure); either answer is valid.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(probe());
    }
}
