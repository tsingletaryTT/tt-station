//! `tt console --snapshot`: with no agent listening on the given
//! `--ctrl-port` (and, in CI, likely no `tt-station-agentd.service` systemd
//! unit installed either), the command must still print valid JSON that
//! deserializes as a `BoxLifecycleSnapshot` with `reachable: false` --
//! "the agent is down" is a normal, representable snapshot, never a hard
//! error (see `console::env::collect_snapshot`'s doc for the same contract
//! at the library level). This is the one guarantee the GTK box panel
//! depends on when it polls `tt console --snapshot`.
//!
//! Not `#[ignore]`d: unlike `e2e_mock.rs`, this needs no `mock-box` binary
//! and no free-standing server -- the whole point is that NOTHING is
//! listening on `port`. `systemctl`/`journalctl` calls inside
//! `collect_snapshot` degrade to `ServiceState::Unknown`/an empty journal on
//! any error (missing binary, no such unit, no systemd at all in a
//! container), so this is safe to run in any CI environment.

use assert_cmd::Command as AssertCommand;
use libttstation::model::BoxLifecycleSnapshot;

/// A high, unusual port nothing else in this test suite binds to, so
/// `tt console --snapshot` against it is guaranteed to see "agent down".
const UNUSED_CTRL_PORT: u16 = 18901;

#[test]
fn snapshot_prints_valid_json_with_reachable_false_when_agent_is_down() {
    let output = AssertCommand::cargo_bin("tt")
        .expect("locate tt binary")
        .args([
            "console",
            "--snapshot",
            "--ctrl-port",
            &UNUSED_CTRL_PORT.to_string(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("stdout is valid UTF-8");
    let snap: BoxLifecycleSnapshot = serde_json::from_str(&text)
        .expect("--snapshot output must deserialize as BoxLifecycleSnapshot");

    assert!(
        !snap.reachable,
        "no agent is listening on {UNUSED_CTRL_PORT}; snapshot must report reachable=false"
    );
    assert!(snap.status.is_none());
    assert!(snap.config.is_none());
    assert!(snap.serving.is_empty());
}
