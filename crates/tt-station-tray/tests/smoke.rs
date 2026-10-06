//! `tt-station-tray --smoke` runs the window's logic headless, prints `ok` and exits 0, without
//! initialising GTK. `DISPLAY` and `WAYLAND_DISPLAY` are removed so the test would fail if smoke
//! mode ever reached for a display.

use assert_cmd::Command;

#[test]
fn smoke_prints_ok_and_exits_zero_without_a_display() {
    let out = Command::cargo_bin("tt-station-tray")
        .unwrap()
        .arg("--smoke")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .assert()
        .success();
    assert_eq!(String::from_utf8_lossy(&out.get_output().stdout), "ok\n");
}

#[test]
fn unknown_arguments_exit_two() {
    Command::cargo_bin("tt-station-tray")
        .unwrap()
        .arg("--no-such-flag")
        .assert()
        .code(2);
}
