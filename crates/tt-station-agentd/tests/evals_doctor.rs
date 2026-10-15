// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Box Doctor through the router: `GET /doctor`'s verdicts from what the box's
//! tools report, `POST /doctor/fix/{fix}` (which fixes exist, who may apply
//! them, when they're refused), and the support bundle (`POST /diagnostics`,
//! `GET /diagnostics/{id}`): what's in it, that secrets in logs are masked,
//! and that its id can't reach outside `diagnostics/`.
//!
//! Nothing here touches the real chips, containers or services: `tt-smi` and
//! the chip-reset command are per-test scripts (`with_telemetry_config`,
//! `with_power_config`), and `docker`, `journalctl` and `tt` are fakes put
//! first on this test binary's PATH (see [`fakes`]). Read-only system probes
//! (`df`, `ip route`, `loginctl`, `uname`, ...) run for real.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use axum::{extract::Json as AxJson, routing, Router};
use libttstation::model::Endpoint;
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

/// Secrets the fake logs print; none may reach a bundle as written.
const HF: &str = "hf_AbCdEfGhIjKlMnOpQrStUvWx123";
const SK: &str = "sk-ABCDEFGHIJKLMNOPQRSTUVWX";
const BEARER_1: &str = "upstreamtoken12345";
const BEARER_2: &str = "clienttoken67890";

/// A model server for the whole binary (its own thread and runtime, so it
/// outlives each test's runtime): the fake `docker ps` lists an inference
/// container publishing this port, and a test that adopts it makes that
/// container the box's own.
fn model_port() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let router = Router::new().route(
                    "/v1/models",
                    routing::get(|| async {
                        AxJson(json!({ "data": [{ "id": "acme/stub-chat-8b" }] }))
                    }),
                );
                let l = tokio::net::TcpListener::from_std(l).unwrap();
                axum::serve(l, router).await.unwrap();
            });
        });
        port
    })
}

/// Fake `docker`, `journalctl` and `tt`, first on this binary's PATH. Set once,
/// before any test reaches code that spawns them (every test calls this
/// first); PATH is process-wide and this binary is its own process. Under the
/// target dir, so a leftover is harmless.
fn fakes() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("doctor-fakes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let port = model_port();
        let log = dir.join("docker.log");
        let docker = format!(
            r#"#!/bin/sh
echo "$@" >> '{log}'
case "$1" in
  ps)
    case "$*" in
      *" -q"*) echo stray0001 ;;
      *" -a "*) echo "stray0001 ghcr.io/tenstorrent/tt-inference-server/vllm:x Up 0.0.0.0:{port}->8000/tcp" ;;
      *) echo "stray0001 ghcr.io/tenstorrent/tt-inference-server/vllm:x 0.0.0.0:{port}->8000/tcp"
         echo "db0002 postgres:16 0.0.0.0:5432->5432/tcp" ;;
    esac ;;
  version) echo 27.3.1 ;;
  logs)
    echo "INFO starting with HF_TOKEN={HF}"
    echo "proxy: {{\"authorization\": \"Bearer {BEARER_1}\", \"x-forwarded\": \"Bearer {BEARER_2}\"}}" ;;
esac
exit 0
"#,
            log = log.display()
        );
        let journalctl = format!(
            r#"#!/bin/sh
case "$*" in
  *"-k "*) printf 'kernel: INFO: task tt-smi:4242 blocked for more than 122 seconds.\nkernel: Call Trace:\nkernel:  tt_ioctl+0x1/0x2 [tenstorrent]\n' ;;
  *) echo "agentd: anthropic key {SK} configured"
     echo "agentd: downloading with HF_TOKEN={HF}" ;;
esac
"#
        );
        for (name, body) in [
            ("docker", docker),
            ("journalctl", journalctl),
            ("tt", "#!/bin/sh\necho tt 9.9.9-fake\n".to_string()),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", dir.display()));
        dir
    })
}

fn docker_calls() -> Vec<String> {
    std::fs::read_to_string(fakes().join("docker.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

fn chip(temp: f32, fw: &str) -> Value {
    json!({ "board_info": { "bus_id": "0000:01:00.0", "board_type": "p300c", "pcie_speed": 4, "pcie_width": "4" },
            "telemetry": { "asic_temperature": format!("{temp:.1}") },
            "limits": { "therm_trip_l1_limit": "90" },
            "firmwares": { "fw_bundle_version": fw } })
}

fn tt_smi_json(chips: Vec<Value>) -> String {
    json!({ "host_info": { "Driver": "TT-KMD 2.4.1" }, "device_info": chips }).to_string()
}

fn dead_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Station {
    state: AppState,
    base: String,
    owner: String,
    member: String,
    client: reqwest::Client,
    dir: tempfile::TempDir,
}

/// A "4xBH" box whose `tt-smi -s` prints `smi` (or fails, for `None`), whose
/// chip reset touches `<dir>/reset-ran`, with an owner and a member paired.
async fn station(smi: Option<String>) -> Station {
    fakes();
    let dir = tempfile::tempdir().unwrap();
    let smi_body = match smi {
        Some(json) => {
            std::fs::write(dir.path().join("smi.json"), json).unwrap();
            format!(
                "echo \"$@\" >> '{0}/tt-smi.log'\ncat '{0}/smi.json'",
                dir.path().display()
            )
        }
        None => format!(
            "echo \"$@\" >> '{}/tt-smi.log'\necho 'no chips' >&2\nexit 1",
            dir.path().display()
        ),
    };
    let tt_smi = script(&dir.path().join("tt-smi"), &smi_body);
    let reset = script(
        &dir.path().join("reset"),
        &format!("touch '{}/reset-ran'", dir.path().display()),
    );
    let config = dir.path().join("config");
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_telemetry_config(tt_smi.display().to_string(), 1)
        .with_power_config(
            vec![reset.display().to_string()],
            vec!["false".into()],
            vec!["false".into()],
            vec!["false".into()],
        )
        .with_serving_config("127.0.0.1".into(), dead_port())
        .with_config_dir(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        state,
        base,
        owner,
        member,
        client: reqwest::Client::new(),
        dir,
    }
}

fn healthy() -> String {
    tt_smi_json(vec![chip(50.0, "19.13.1.0"); 4])
}

impl Station {
    async fn call(&self, method: reqwest::Method, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut r = self.client.request(method, format!("{}{path}", self.base));
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        let r = r.send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn doctor(&self) -> Value {
        let (status, v) = self
            .call(reqwest::Method::GET, "/doctor", Some(&self.owner))
            .await;
        assert_eq!(status, 200, "{v}");
        v
    }

    async fn fix(&self, fix: &str, token: &str) -> (u16, Value) {
        self.call(
            reqwest::Method::POST,
            &format!("/doctor/fix/{fix}"),
            Some(token),
        )
        .await
    }

    fn reset_ran(&self) -> bool {
        self.dir.path().join("reset-ran").exists()
    }

    fn tt_smi_runs(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("tt-smi.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn adopt(&self, port: u16) {
        self.state.adopt_serving(Endpoint {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            model: "acme/stub-chat-8b".into(),
            requires_key: false,
        });
    }
}

fn check<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no {id} check in {v}"))
}

fn has(v: &Value, id: &str) -> bool {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == id)
}

// ---------------------------------------------------------------- verdicts

#[tokio::test]
async fn the_doctor_reads_the_chips_docker_and_kernel_through_the_boxs_tools() {
    let s = station(Some(healthy())).await;
    let v = s.doctor().await;

    let chips = check(&v, "chips");
    assert_eq!(
        (&chips["status"], &chips["title"]),
        (&json!("ok"), &json!("4 chips found"))
    );
    assert!(
        chips["detail"].as_str().unwrap().contains("Gen4 x4"),
        "{chips}"
    );
    assert_eq!(check(&v, "cooling")["status"], "ok");
    let fw = check(&v, "firmware");
    assert_eq!(fw["title"], "Firmware 19.13.1.0");
    assert!(fw["detail"].as_str().unwrap().contains("TT-KMD 2.4.1"));
    assert!(s.tt_smi_runs() >= 1, "read with `tt-smi -s`");
    assert_eq!(check(&v, "docker")["status"], "ok");

    // An inference container the box didn't start is a stray (with its fix);
    // other containers aren't the Doctor's business.
    let stray = check(&v, "stray");
    assert_eq!(
        (&stray["status"], &stray["fix"]),
        (&json!("warn"), &json!("stop_stray_containers"))
    );
    assert!(stray["detail"].as_str().unwrap().contains("stray0001"));
    assert!(!stray["detail"].as_str().unwrap().contains("db0002"));
    // The kernel saw a program stuck in the driver.
    let hang = check(&v, "driver_hang");
    assert_eq!(hang["status"], "warn");
    assert!(
        hang["detail"].as_str().unwrap().starts_with("1 program "),
        "{hang}"
    );
    assert!(!has(&v, "serving"), "nothing serving, nothing to say");
    assert_eq!(v["status"], "warn", "the worst verdict");
}

#[tokio::test]
async fn missing_hot_or_unreadable_chips_fail_with_a_chip_reset_offered() {
    let s = station(Some(tt_smi_json(vec![
        chip(85.0, "19.13.1.0"),
        chip(50.0, "18.0.0"),
        chip(50.0, "19.13.1.0"),
    ])))
    .await;
    let v = s.doctor().await;
    let chips = check(&v, "chips");
    assert_eq!(chips["title"], "3 of 4 chips found");
    assert_eq!(
        (&chips["status"], &chips["fix"]),
        (&json!("fail"), &json!("reset_chips"))
    );
    assert_eq!(
        check(&v, "cooling")["status"],
        "warn",
        "85 °C is within 10 of the 90 °C trip"
    );
    let fw = check(&v, "firmware");
    assert_eq!(
        (&fw["title"], &fw["status"]),
        (&json!("Mixed firmware"), &json!("warn"))
    );
    assert!(
        fw["command"].as_str().unwrap().contains("tt-flash"),
        "a command, not an automatic fix"
    );
    assert!(fw.get("fix").is_none());
    assert_eq!(v["status"], "fail");

    let s = station(None).await;
    let v = s.doctor().await;
    let chips = check(&v, "chips");
    assert_eq!(
        (&chips["title"], &chips["fix"]),
        (&json!("Chips not readable"), &json!("reset_chips"))
    );
    assert!(!has(&v, "cooling") && !has(&v, "firmware"));
}

#[tokio::test]
async fn a_serving_model_is_probed_and_its_own_container_is_not_a_stray() {
    // Serving from the container docker lists: it answers, and it's ours.
    let s = station(Some(healthy())).await;
    s.adopt(model_port());
    let v = s.doctor().await;
    let serving = check(&v, "serving");
    assert_eq!(serving["status"], "ok", "{serving}");
    assert!(serving["title"].as_str().unwrap().ends_with("is serving"));
    assert!(!has(&v, "stray"), "the box's own container: {v}");

    // Serving on a port nothing answers on: restart it.
    let s = station(Some(healthy())).await;
    s.adopt(dead_port());
    let v = s.doctor().await;
    let serving = check(&v, "serving");
    assert_eq!(
        (&serving["status"], &serving["fix"]),
        (&json!("fail"), &json!("restart_model"))
    );
    assert!(serving["title"]
        .as_str()
        .unwrap()
        .ends_with("isn't answering"));
    assert!(has(&v, "stray"), "that container is someone else's now");
}

#[tokio::test]
async fn while_a_bring_up_has_the_chips_they_arent_read_and_no_fix_runs() {
    let s = station(Some(healthy())).await;
    s.state.set_bringup_busy(true);
    let v = s.doctor().await;
    let chips = check(&v, "chips");
    assert_eq!(
        (&chips["status"], &chips["title"]),
        (&json!("ok"), &json!("Chips in use"))
    );
    assert_eq!(
        s.tt_smi_runs(),
        0,
        "tt-smi would open chips a bring-up is driving"
    );
    assert!(!has(&v, "stray"), "a bring-up's containers aren't strays");

    for fix in [
        "reset_chips",
        "stop_stray_containers",
        "restart_model",
        "nope",
    ] {
        let (status, v) = s.fix(fix, &s.owner).await;
        assert_eq!(status, 409, "{fix}: {v}");
        assert!(v["error"].as_str().unwrap().contains("bring-up"), "{v}");
    }
    assert!(!s.reset_ran());
}

// ---------------------------------------------------------------- fixes

#[tokio::test]
async fn only_the_three_known_fixes_exist() {
    let s = station(Some(healthy())).await;
    for bad in [
        "nope",
        "reset-chips",
        "RESET_CHIPS",
        "..%2F..%2Fbin%2Fsh",
        "reset_chips%00",
    ] {
        let (status, v) = s.fix(bad, &s.owner).await;
        assert_eq!(status, 400, "{bad}: {v}");
        assert!(
            v["error"].as_str().unwrap().starts_with("No fix called"),
            "{v}"
        );
    }
    assert!(!s.reset_ran());
    let (status, v) = s.fix("restart_model", &s.owner).await;
    assert_eq!(
        (status, v["error"].as_str()),
        (409, Some("Nothing to restart."))
    );
}

#[tokio::test]
async fn an_owner_resets_the_chips_only_while_nothing_is_serving() {
    let s = station(Some(healthy())).await;
    s.adopt(model_port());
    let (status, v) = s.fix("reset_chips", &s.owner).await;
    assert_eq!(status, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("Stop the model"));
    assert!(
        !s.reset_ran(),
        "a reset under a serving model would kill it"
    );

    let s = station(Some(healthy())).await;
    let (status, v) = s.fix("reset_chips", &s.owner).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["applied"], "reset_chips");
    assert!(s.reset_ran(), "the configured reset command ran");
    assert!(has(&v, "chips"), "and the box was checked again: {v}");
}

#[tokio::test]
async fn a_member_cannot_reset_the_chips_through_the_doctor() {
    // `POST /power {"action": "reset-chips"}` is an owner's; the Doctor's fix
    // runs the very same whole-box reset, so it must be too.
    let s = station(Some(healthy())).await;
    let (status, v) = s.fix("reset_chips", &s.member).await;
    assert_eq!(status, 403, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("Only an owner"),
        "{v}"
    );
    assert!(!s.reset_ran());
    // Reading the Doctor stays open to members.
    let (status, _) = s
        .call(reqwest::Method::GET, "/doctor", Some(&s.member))
        .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn stopping_strays_stops_only_inference_containers_the_box_didnt_start() {
    let s = station(Some(healthy())).await;
    let (status, v) = s.fix("stop_stray_containers", &s.owner).await;
    assert_eq!(status, 200, "{v}");
    let calls = docker_calls();
    assert!(calls.iter().any(|c| c == "stop stray0001"), "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("stop") && c.contains("db0002")),
        "{calls:?}"
    );
}

// ---------------------------------------------------------------- the support bundle

fn unpack(bytes: &[u8]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().display().to_string();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        out.insert(name, text);
    }
    out
}

#[tokio::test]
async fn the_support_bundle_has_what_support_needs_and_no_secrets() {
    let s = station(Some(healthy())).await;
    let (status, v) = s
        .call(reqwest::Method::POST, "/diagnostics", Some(&s.owner))
        .await;
    assert_eq!(status, 200, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("ttdiag-"), "{id}");
    assert!(v["bytes"].as_u64().unwrap() > 0);

    let r = s
        .client
        .get(format!("{}/diagnostics/{id}", s.base))
        .bearer_auth(&s.owner)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/gzip");
    assert!(r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains(&format!("{id}.tar.gz")));
    let files = unpack(&r.bytes().await.unwrap());
    let names: Vec<String> = files
        .keys()
        .map(|k| k.trim_start_matches(&format!("{id}/")).to_string())
        .collect();
    for want in [
        "doctor.json",
        "tt-smi.json",
        "events.json",
        "versions.txt",
        "agent.log",
        "serving.log",
    ] {
        assert!(names.iter().any(|n| n == want), "no {want} in {names:?}");
    }
    let file = |n: &str| files[&format!("{id}/{n}")].clone();
    let doctor: Value = serde_json::from_str(&file("doctor.json")).unwrap();
    assert_eq!(doctor["box"], "qb2-lab");
    assert!(file("tt-smi.json").contains("19.13.1.0"));
    assert!(
        file("versions.txt").contains("27.3.1") && file("versions.txt").contains("tt 9.9.9-fake")
    );
    assert!(file("serving.log").contains("docker logs --tail 800 stray0001"));
    assert!(
        file("serving.log").contains("hf_***"),
        "{}",
        file("serving.log")
    );
    assert!(
        file("agent.log").contains("sk-***"),
        "{}",
        file("agent.log")
    );

    // No secret, wherever it was printed, and none of the box's device tokens.
    for (name, text) in &files {
        for secret in [
            HF,
            SK,
            BEARER_1,
            BEARER_2,
            s.owner.as_str(),
            s.member.as_str(),
        ] {
            assert!(!text.contains(secret), "{name} carries {secret}:\n{text}");
        }
    }
    // Kept as the archive alone: the folder it was built in is gone.
    let kept: Vec<String> = std::fs::read_dir(s.dir.path().join("config/diagnostics"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(kept, [format!("{id}.tar.gz")]);
}

#[tokio::test]
async fn a_bundle_id_cannot_reach_outside_the_diagnostics_folder() {
    let s = station(Some(healthy())).await;
    let config = s.dir.path().join("config");
    // What a traversal would reach: `diagnostics/ttdiag-/../../secret.tar.gz`.
    std::fs::create_dir_all(config.join("diagnostics/ttdiag-")).unwrap();
    std::fs::write(config.join("secret.tar.gz"), b"not for you").unwrap();
    std::fs::write(config.join("diagnostics/x.tar.gz"), b"not a bundle").unwrap();
    for bad in [
        "ttdiag-%2F..%2F..%2Fsecret",
        "ttdiag-..%2F..%2Fsecret",
        "..%2Fsecret",
        "x",
        "ttdiag-1.tar",
        "ttdiag-404-abcdef",
    ] {
        let r = s
            .client
            .get(format!("{}/diagnostics/{bad}", s.base))
            .bearer_auth(&s.owner)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = r.bytes().await.unwrap();
        assert_eq!(status, 404, "{bad}");
        assert!(
            !body.windows(5).any(|w| w == b"not a"),
            "{bad} served a file"
        );
    }
}

#[tokio::test]
async fn every_doctor_route_needs_a_paired_device() {
    let s = station(Some(healthy())).await;
    for (m, path) in [
        (reqwest::Method::GET, "/doctor"),
        (reqwest::Method::POST, "/doctor/fix/reset_chips"),
        (reqwest::Method::POST, "/doctor/fix/stop_stray_containers"),
        (reqwest::Method::POST, "/diagnostics"),
        (reqwest::Method::GET, "/diagnostics/ttdiag-1-abcdef"),
    ] {
        for token in [None, Some("not-a-token")] {
            let (status, _) = s.call(m.clone(), path, token).await;
            assert_eq!(status, 401, "{m} {path} with {token:?}");
        }
    }
    assert!(!s.reset_ran());
    assert!(
        !s.dir.path().join("config/diagnostics").exists(),
        "no bundle was built"
    );
    assert_eq!(s.tt_smi_runs(), 0, "nothing was examined");
}
