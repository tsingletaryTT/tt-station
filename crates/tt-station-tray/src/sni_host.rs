//! Is something on this desktop showing tray icons? (the tray-host check)
//!
//! The tray talks StatusNotifierItem (SNI). Icons only appear when a "watcher" owns the session
//! bus name `org.kde.StatusNotifierWatcher`. KDE provides one. Stock GNOME does not, so there the
//! icon is invisible until the person enables an AppIndicator extension. This module answers
//! "is a watcher there?" ([`probe`]), says what to do if not ([`install_hint`]), and keeps the
//! "tell them once" marker ([`should_show_notice`], [`mark_notice_shown`]).
//!
//! [`probe`] must be called inside a tokio runtime (it reports `Missing` otherwise).
//!
//! Everything but [`probe`] is pure so it can be tested without a desktop. `app.rs` calls these at
//! start-up (`app::maybe_show_notice`, `app::run_no_window`).

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

/// True when called from within a tokio runtime (the guard [`probe`] uses before touching zbus).
fn runtime_available() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}

/// How long [`probe`] waits for the bus before giving up and reporting `Missing`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask the session bus whether `org.kde.StatusNotifierWatcher` has an owner. Never panics;
/// every failure maps to `Missing` through [`classify`]. Read-only (`NameHasOwner`).
///
/// **Must run inside a tokio runtime.** zbus's `tokio` feature spawns its tasks with
/// `tokio::spawn`, which panics without one. [`runtime_available`] is checked first, and with no
/// runtime the answer is `Missing`.
pub async fn probe() -> HostStatus {
    if !runtime_available() {
        return classify(Err("no tokio runtime".to_string()));
    }
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
///   The apt line is for machines where it is not already installed.
/// * Fedora: packages.fedoraproject.org lists `gnome-shell-extension-appindicator`; its extension
///   uuid is `appindicatorsupport@rgcjonas.gmail.com`, which matches upstream's `metadata.json`
///   (github.com/ubuntu/gnome-shell-extension-appindicator). Package page fetched through a
///   summarising tool, not read raw; the uuid was cross-checked against upstream.
/// * Derivatives (matched on `ID=` only, never `ID_LIKE=`) and other desktops: the extension's display name, with no package name guessed.
pub fn install_hint(os_release: &str, desktop: &str) -> Option<String> {
    let desktops: Vec<String> = desktop
        .split(':')
        .map(|d| d.trim().to_ascii_lowercase())
        .collect();
    let has = |n: &str| desktops.iter().any(|d| d == n);
    if has("kde") {
        return None; // Plasma hosts StatusNotifierItem itself.
    }
    // Only `ID=` is matched. `ID_LIKE=` would make Pop!_OS count as Ubuntu and Rocky/Alma count
    // as Fedora, and nothing here was verified for those: their package source or extension uuid
    // may differ. Every derivative gets the generic hint below.
    let id = os_release
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID="))
        .map(|v| {
            v.trim_matches(|c| c == '"' || c == '\'')
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if has("gnome") && id == "ubuntu" {
        return Some(
            "Tray icons need a GNOME extension. If it is not installed, run: sudo apt install gnome-shell-extension-appindicator. \
             Then run: gnome-extensions enable ubuntu-appindicators@ubuntu.com \
             (if it says the extension does not exist, log out and in first)."
                .to_string(),
        );
    }
    if has("gnome") && id == "fedora" {
        return Some(
            "Tray icons need a GNOME extension. Install it: sudo dnf install gnome-shell-extension-appindicator. \
             Then log out and in again, because a newly installed extension is not seen until the next login. \
             Then enable it: gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com."
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
pub fn should_show_notice(status: HostStatus, hint: Option<&str>, marker_exists: bool) -> bool {
    status == HostStatus::Missing && hint.is_some() && !marker_exists
}

/// Record that the notice was shown. Creates `config_dir` if needed.
///
/// A caller must treat `Err` as "not recorded": log it and carry on. The notice may then repeat
/// on the next start.
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
    /// Positions of the three steps, asserting each is present.
    fn pos(h: &str, needle: &str) -> usize {
        h.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from {h}"))
    }
    #[test]
    fn hint_for_fedora_gnome_installs_then_relogs_then_enables() {
        let h = install_hint(FEDORA, "GNOME").unwrap();
        let install = pos(&h, "sudo dnf install gnome-shell-extension-appindicator");
        let relog = pos(&h, "log out and in");
        let enable = pos(
            &h,
            "gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com",
        );
        assert!(install < relog && relog < enable, "{h}");
    }
    #[test]
    fn hint_for_ubuntu_gnome_installs_if_missing_then_enables() {
        let h = install_hint(UBUNTU, "ubuntu:GNOME").unwrap();
        let install = pos(&h, "sudo apt install gnome-shell-extension-appindicator");
        let enable = pos(
            &h,
            "gnome-extensions enable ubuntu-appindicators@ubuntu.com",
        );
        assert!(install < enable, "{h}");
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
    fn derivatives_via_id_like_get_the_generic_hint() {
        let pop = "ID=pop\nID_LIKE=\"ubuntu debian\"\n";
        let rocky = "ID=rocky\nID_LIKE=\"rhel centos fedora\"\n";
        for os in [pop, rocky] {
            let h = install_hint(os, "GNOME").unwrap();
            assert!(
                h.contains("AppIndicator and KStatusNotifierItem Support"),
                "{h}"
            );
            assert!(
                !h.contains("apt install") && !h.contains("dnf install"),
                "{h}"
            );
        }
    }
    #[test]
    fn ubuntu_with_non_gnome_desktop_gets_generic_hint() {
        let h = install_hint(UBUNTU, "XFCE").unwrap();
        assert!(!h.contains("ubuntu-appindicators"), "{h}");
    }

    #[test]
    fn notice_shown_only_when_missing_hinted_and_unmarked() {
        let hint = Some("x");
        assert!(should_show_notice(HostStatus::Missing, hint, false));
        assert!(!should_show_notice(HostStatus::Present, hint, false));
        assert!(!should_show_notice(HostStatus::Missing, None, false));
        assert!(!should_show_notice(HostStatus::Missing, hint, true));
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
    fn runtime_available_is_false_on_a_plain_thread() {
        assert!(!runtime_available());
    }
    #[tokio::test]
    async fn runtime_available_is_true_inside_a_runtime() {
        assert!(runtime_available());
    }
    #[test]
    fn probe_without_a_runtime_is_missing_not_a_panic() {
        // futures-free: poll the future once by hand on a plain thread (no tokio context).
        use std::future::Future;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn raw() -> RawWaker {
            fn clone(_: *const ()) -> RawWaker {
                raw()
            }
            fn noop(_: *const ()) {}
            static VT: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
            RawWaker::new(std::ptr::null(), &VT)
        }
        let waker = unsafe { Waker::from_raw(raw()) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(probe());
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(HostStatus::Missing));
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
