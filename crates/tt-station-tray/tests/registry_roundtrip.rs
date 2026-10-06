//! The station registry on disk: round trip, corrupt files, address validation, per-model run
//! memory, and atomic saves.

use std::os::unix::fs::PermissionsExt;

use libttstation::run_options::RunOptions;
use tt_station_tray::registry::{AddError, Registry};
use tt_station_tray::station::StationId;

fn opts(port: u16) -> RunOptions {
    RunOptions {
        port: Some(port),
        ..RunOptions::default()
    }
}

#[test]
fn registry_roundtrips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/stations.json");

    let mut reg = Registry::load(&path);
    assert!(reg.stations().is_empty(), "missing file loads as empty");
    let id = reg.add_manual("franklinstower", 8765).unwrap();
    reg.remember_run(&id, "Llama-3.1-8B-Instruct", &opts(9000));
    reg.save().unwrap();

    let again = Registry::load(&path);
    assert_eq!(again.stations().len(), 1);
    assert_eq!(again.stations()[0].host, "franklinstower");
    assert_eq!(again.stations()[0].port, 8765);
    assert_eq!(id.0, "franklinstower:8765");
    assert_eq!(
        again.last_run(&id, "Llama-3.1-8B-Instruct"),
        Some(&opts(9000))
    );

    // The file never holds a token, whatever the station is called.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.to_lowercase().contains("token"), "got {text}");
}

#[test]
fn registry_survives_a_corrupt_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stations.json");
    std::fs::write(&path, [0xff, 0xfe, 0x00, b'{', b'x']).unwrap();

    let mut reg = Registry::load(&path);
    assert!(reg.stations().is_empty());
    reg.add_manual("192.168.4.38", 8765).unwrap();
    reg.save().unwrap();

    let again = Registry::load(&path);
    assert_eq!(
        again.stations().len(),
        1,
        "the next save replaced the garbage"
    );
}

#[test]
fn add_manual_rejects_bad_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::load(&dir.path().join("s.json"));

    for bad in [
        "",
        "http://x",
        "a b",
        "host:notaport",
        "host:8765",
        "a/b",
        "x?y",
        "user@host",
        "-leading",
        "999.1.1.1",
        "[::1]",
        "bad_char!",
    ] {
        assert!(
            matches!(reg.add_manual(bad, 8765), Err(AddError::BadAddress(_))),
            "{bad:?} should be rejected"
        );
    }
    assert!(
        matches!(reg.add_manual("ok", 0), Err(AddError::BadAddress(_))),
        "port 0 should be rejected"
    );

    assert_eq!(
        reg.add_manual("franklinstower", 8765).unwrap().0,
        "franklinstower:8765"
    );
    assert_eq!(
        reg.add_manual("192.168.4.38", 8765).unwrap().0,
        "192.168.4.38:8765"
    );
    // An IPv6 literal is accepted and its id is bracketed, the way a person types it.
    assert_eq!(reg.add_manual("::1", 8765).unwrap().0, "[::1]:8765");
    assert_eq!(
        reg.add_manual("franklinstower", 8765),
        Err(AddError::Duplicate)
    );
    assert_eq!(
        reg.add_manual("FranklinsTower", 8765),
        Err(AddError::Duplicate),
        "host names compare case-insensitively"
    );
    assert!(
        reg.add_manual("franklinstower", 8766).is_ok(),
        "another port is another station"
    );
    assert_eq!(reg.stations().len(), 4);
}

#[test]
fn remove_forgets_the_station_and_its_run_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::load(&dir.path().join("s.json"));
    let a = reg.add_manual("a", 1).unwrap();
    let b = reg.add_manual("b", 1).unwrap();
    reg.remember_run(&a, "m", &opts(1));
    reg.remember_run(&b, "m", &opts(2));
    reg.remove(&a);
    assert_eq!(reg.stations().len(), 1);
    assert!(reg.last_run(&a, "m").is_none());
    assert_eq!(reg.last_run(&b, "m"), Some(&opts(2)));
    reg.remove(&StationId("nope:1".into())); // unknown id: no-op
}

#[test]
fn last_run_is_per_station_and_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::load(&dir.path().join("s.json"));
    let a = reg.add_manual("a", 8765).unwrap();
    let b = reg.add_manual("b", 8765).unwrap();

    reg.remember_run(&a, "m1", &opts(1));
    reg.remember_run(&a, "m2", &opts(2));
    reg.remember_run(&b, "m1", &opts(3));
    assert_eq!(reg.last_run(&a, "m1"), Some(&opts(1)));
    assert_eq!(reg.last_run(&a, "m2"), Some(&opts(2)));
    assert_eq!(reg.last_run(&b, "m1"), Some(&opts(3)));
    assert!(reg.last_run(&b, "m2").is_none());

    reg.remember_run(&a, "m1", &opts(4));
    assert_eq!(reg.last_run(&a, "m1"), Some(&opts(4)), "latest run wins");
}

#[test]
fn last_model_is_the_most_recent_run_per_station_and_survives_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");
    let mut reg = Registry::load(&path);
    let a = reg.add_manual("a", 8765).unwrap();
    let b = reg.add_manual("b", 8765).unwrap();
    assert_eq!(reg.last_model(&a), None);

    reg.remember_run(&a, "zeta", &opts(1));
    reg.remember_run(&a, "alpha", &opts(2)); // later, and sorts first: recency must win
    reg.remember_run(&b, "beta", &opts(3));
    assert_eq!(reg.last_model(&a), Some("alpha"));
    assert_eq!(reg.last_model(&b), Some("beta"));
    reg.save().unwrap();

    let mut again = Registry::load(&path);
    assert_eq!(again.last_model(&a), Some("alpha"));
    again.remove(&a);
    assert_eq!(
        again.last_model(&a),
        None,
        "forgetting a station forgets it"
    );
    assert_eq!(again.last_model(&b), Some("beta"));
}

#[test]
fn a_file_without_last_model_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");
    std::fs::write(&path, r#"{"stations":[],"last_run":[]}"#).unwrap();
    let reg = Registry::load(&path);
    assert_eq!(reg.last_model(&StationId("a:8765".into())), None);
}

#[test]
fn save_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stations.json");

    let mut reg = Registry::load(&path);
    reg.add_manual("old", 1).unwrap();
    reg.save().unwrap();
    let before = std::fs::read(&path).unwrap();

    // A read-only directory refuses new files and renames but still lets an existing file be
    // opened for writing. A save that writes in place would get through and corrupt the file;
    // an atomic save fails and leaves the old bytes alone.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable_anyway = std::fs::File::create(dir.path().join("probe")).is_ok();
    reg.add_manual("new", 2).unwrap();
    let result = reg.save();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    if writable_anyway {
        eprintln!("running with permissions that ignore directory modes (root?); skipping");
        return;
    }
    assert!(result.is_err(), "save into a read-only dir must fail");
    assert_eq!(std::fs::read(&path).unwrap(), before, "old file is intact");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "no temp file left behind: {leftovers:?}"
    );
}

#[test]
fn set_mac_is_kept_across_save_and_load_and_ignores_unknown_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stations.json");
    let mut reg = Registry::load(&path);
    let id = reg.add_manual("qb2-lab.local", 8765).unwrap();
    reg.set_mac(&StationId("nobody:1".into()), "00:00:00:00:00:00");
    assert_eq!(reg.stations()[0].mac, None);
    reg.set_mac(&id, "aa:bb:cc:dd:ee:ff");
    reg.save().unwrap();
    let again = Registry::load(&path);
    assert_eq!(
        again.stations()[0].mac.as_deref(),
        Some("aa:bb:cc:dd:ee:ff")
    );
}
