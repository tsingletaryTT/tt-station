//! `tt-station-tray --smoke` runs the window's logic (registry, view models, a command preview)
//! and exits without initialising GTK.
//!
//! **What this proves.** The child gets no way to reach any display: `DISPLAY` and
//! `WAYLAND_DISPLAY` are removed, `XDG_RUNTIME_DIR` points at an empty temporary directory (so
//! libwayland's fallback to `$XDG_RUNTIME_DIR/wayland-0` finds nothing, which matters on a box
//! with a live desktop session), and `GDK_BACKEND` names a backend that does not exist. A
//! timeout turns a hang into a failure. So if this passes, `--smoke` printed exactly `ok` and
//! exited 0 without a display, within the timeout. If smoke mode ever initialised GTK, GTK
//! would fail to open a display and the test would fail or time out.
//!
//! **What it cannot catch.** If `main` printed `ok` unconditionally, this test would still pass.
//! The failure path (`smoke_check` returns `Err`, the binary exits 1 with the reason) is wired,
//! and was seen red once by breaking an expectation in `smoke_check` (Task 8 report).

use std::time::Duration;

use assert_cmd::Command;

/// A command for the tray binary with every route to a display closed off.
fn displayless(runtime_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("tt-station-tray").unwrap();
    cmd.env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("GDK_BACKEND", "no-such-backend")
        .timeout(Duration::from_secs(30));
    cmd
}

#[test]
fn smoke_prints_ok_and_exits_zero_without_a_display() {
    let runtime = tempfile::tempdir().unwrap();
    let out = displayless(runtime.path())
        .arg("--smoke")
        .assert()
        .success();
    assert_eq!(String::from_utf8_lossy(&out.get_output().stdout), "ok\n");
}

#[test]
fn unknown_arguments_exit_two() {
    let runtime = tempfile::tempdir().unwrap();
    displayless(runtime.path())
        .arg("--no-such-flag")
        .assert()
        .code(2);
}
