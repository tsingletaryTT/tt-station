// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Community model packages (`src/community.rs`) through the API: what the
//! package list shows, and every way a download, a start or a removal is
//! refused before it touches the network or the chips -- bad package names
//! (traversal and shell characters included), unknown and unfit packages,
//! chips held by something else, weights not on the box, a model already
//! serving. Then what a download, pause, cancel and removal do on disk.
//!
//! The whole binary runs against a scratch `HOME` (set once, in [`fake_box`],
//! before any test touches the agent; env changes are process-wide):
//!
//! - `~/.local/bin/tt-model` is a fake that logs its arguments and never
//!   reaches Hugging Face: `info` always fails, so the agent never goes on to
//!   ask Hugging Face for sizes itself, and `search` prints a canned listing.
//! - `systemctl` and `systemd-run` are fakes earlier on `PATH`: they log, and
//!   a unit is "active" only while the test has made a marker for it.
//!   `systemd-run` never runs the command it's handed.
//! - `~/.cache/tt-model/installed.json` lists the packages "on the box".
//! - `HTTPS_PROXY` points at a closed local port: belt and braces, so a
//!   request that did slip out to Hugging Face fails here instead.
//!
//! Not covered: the free-space refusal on `POST /community/pull` needs the
//! package's size, which only comes from Hugging Face; `GET /community` with
//! a download in progress asks Hugging Face for its total; and a start that
//! passes every check resets the chips and runs `tt-model serve`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use reqwest::Method as M;
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState, Phase};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

/// The Hugging Face token in the scratch home: it must never show up in a
/// unit's command line (where `systemctl --user show` prints it).
const TOKEN: &str = "hf_community_test_token_never_in_argv";

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn manifest(home: &Path, id: &str, m: Value) -> Value {
    let p = home.join("manifests").join(id.replace('/', "__") + ".json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, m.to_string()).unwrap();
    json!({ "manifest": p, "container": true, "profile": "default" })
}

/// Scratch homes left by earlier runs of this binary (a static is never
/// dropped, so each run leaves its own): removed once their process is gone.
fn sweep_old_homes(prefix: &str) {
    for e in std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(prefix) else {
            continue;
        };
        if pid.parse::<u32>().is_ok() && !Path::new("/proc").join(pid).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The scratch home, set up once for the whole binary.
fn fake_box() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        sweep_old_homes("tt-community-home-");
        let dir = std::env::temp_dir().join(format!("tt-community-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home = std::fs::canonicalize(dir).unwrap();
        let log = home.join("calls.log");
        let active = home.join("active");
        std::fs::create_dir_all(&active).unwrap();

        let bin = home.join("fakebin");
        write_script(
            &bin.join("systemctl"),
            &format!(
                r#"echo "systemctl $*" >> '{log}'
case "$2" in
  is-active) [ -e '{active}'/"$4" ]; exit $? ;;
  stop) rm -f '{active}'/"$3"; exit 0 ;;
esac
exit 0
"#,
                log = log.display(),
                active = active.display()
            ),
        );
        write_script(
            &bin.join("systemd-run"),
            &format!(
                r#"echo "systemd-run $*" >> '{log}'
case "$*" in
  *fail-to-launch*) echo "Failed to start transient service unit: Unit already exists." >&2; exit 1 ;;
esac
exit 0
"#,
                log = log.display()
            ),
        );
        write_script(
            &home.join(".local/bin/tt-model"),
            &format!(
                r#"echo "tt-model $*" >> '{log}'
case "$1" in
  search)
    if [ "$2" = "--catalog" ]; then
      echo '[{{"id":"tt-community/qwen3-p300x2","last_modified":"2026-09-01"}},{{"id":"someone/llama-galaxy"}},{{"id":"x/thing-p150"}},{{"id":"x/unknown"}},{{"no_id":1}}]'
    else
      echo "Error: the hub didn't answer"
    fi ;;
  info) echo "Error: couldn't reach the hub"; exit 1 ;;
  rm)
    case "$2" in
      *locked*) echo "Removing $2" ; echo "Error: $2 is in use" >&2; exit 1 ;;
    esac ;;
esac
exit 0
"#,
                log = log.display()
            ),
        );

        // Packages "on the box".
        let installed = json!({
            "tt-community/fits-p300x2": manifest(&home, "tt-community/fits-p300x2", json!({
                "device_count": 4,
                "container": { "kind": "vllm-plugin", "serve": {
                    "hardware": "p300x2", "max_num_seqs": 8,
                    "capabilities": { "tool_parser": "hermes" } } },
                "weights": { "repo_id": "tt-community/weights-not-here" },
            })),
            "tt-community/galaxy-only": manifest(&home, "tt-community/galaxy-only", json!({
                "device_count": 32,
                "container": { "serve": { "hardware": "galaxy" } },
            })),
            "tt-community/shares-weights": manifest(&home, "tt-community/shares-weights", json!({
                "device_count": 2,
                "container": { "serve": { "hardware": "p300" } },
                "weights": { "repo_id": "tt-community/shared-weights" },
            })),
        });
        let cache = home.join(".cache/tt-model");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("installed.json"), installed.to_string()).unwrap();
        std::fs::create_dir_all(home.join(".cache/huggingface")).unwrap();
        std::fs::write(home.join(".cache/huggingface/token"), TOKEN).unwrap();
        let run = home.join("run");
        std::fs::create_dir_all(&run).unwrap();

        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_RUNTIME_DIR", &run);
        for v in ["HF_TOKEN", "HF_HOME", "HF_HUB_CACHE", "TT_CONFIG_DIR"] {
            std::env::remove_var(v);
        }
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:9");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
        home
    })
}

/// Lines the fakes logged that mention `needle`.
fn calls(needle: &str) -> Vec<String> {
    std::fs::read_to_string(fake_box().join("calls.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_string)
        .collect()
}

/// Make `systemctl --user is-active <unit>` say yes.
fn mark_active(unit: &str) {
    std::fs::write(fake_box().join("active").join(unit), "").unwrap();
}

/// A package's systemd unit for its download, as the agent names it.
fn pull_unit(id: &str) -> String {
    format!(
        "tt-station-pull-{}",
        id.replace('/', "__").replace('.', "-").to_lowercase()
    )
}

/// The serving record (`community/serving.json`) and the model it names live
/// in a process-wide cache too: tests that set a model serving take this, so
/// one doesn't clear the other's record from under it.
static SERVING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Station {
    base: String,
    owner: String,
    member: String,
    state: AppState,
    config: tempfile::TempDir,
    client: reqwest::Client,
}

async fn harness() -> Station {
    fake_box();
    let config = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_config_dir(config.path().to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        base,
        owner,
        member,
        state,
        config,
        client: reqwest::Client::new(),
    }
}

impl Station {
    async fn call(&self, method: M, path: &str, token: &str, body: Value) -> (u16, Value) {
        let mut req = self
            .client
            .request(method.clone(), format!("{}{path}", self.base));
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
        if method != M::GET && method != M::DELETE {
            req = req.json(&body);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    fn work_dir(&self, id: &str) -> PathBuf {
        self.config
            .path()
            .join("community")
            .join(id.replace('/', "__"))
    }
}

#[tokio::test]
async fn package_routes_need_a_paired_device() {
    let b = harness().await;
    let id = "tt-community/needs-a-device";
    for token in ["", "not-a-token"] {
        for (method, path, body) in [
            (M::GET, "/community".to_string(), Value::Null),
            (M::GET, "/community/catalog".to_string(), Value::Null),
            (M::GET, format!("/community/info/{id}"), Value::Null),
            (M::POST, "/community/pull".to_string(), json!({ "id": id })),
            (
                M::POST,
                "/community/pull/pause".to_string(),
                json!({ "id": id }),
            ),
            (
                M::POST,
                "/community/pull/cancel".to_string(),
                json!({ "id": id }),
            ),
            (M::POST, "/community/serve".to_string(), json!({ "id": id })),
            (M::DELETE, format!("/community/{id}"), Value::Null),
        ] {
            let (status, out) = b.call(method.clone(), &path, token, body).await;
            assert_eq!(status, 401, "{method} {path} with {token:?}: {out}");
        }
    }
    assert!(calls(id).is_empty(), "nothing ran: {:?}", calls(id));
    assert!(!b.config.path().join("community").exists());

    // A member manages community packages like an owner (as with every other
    // model): the routes take any paired device.
    let (status, body) = b.call(M::GET, "/community", &b.member, Value::Null).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["available"], true, "{body}");
}

#[tokio::test]
async fn bad_package_names_are_refused_before_anything_runs() {
    let b = harness().await;
    let long = format!("org/{}", "a".repeat(97));
    let bad = [
        "",
        "org",
        "org/",
        "/name",
        "../etc/passwd",
        "org/../name",
        "org/..",
        "a..b/name",
        "org/name/extra",
        "org/na me",
        "org/name;touch pwned",
        "org/$(id)",
        "org/name'quote",
        "org\\name",
        long.as_str(),
    ];
    for id in bad {
        for path in [
            "/community/pull",
            "/community/pull/pause",
            "/community/pull/cancel",
            "/community/serve",
        ] {
            for token in [&b.owner, &b.member] {
                let (status, body) = b.call(M::POST, path, token, json!({ "id": id })).await;
                assert_eq!(status, 400, "{path} {id:?}: {body}");
                assert_eq!(body["error"], "That isn't a package name.", "{path} {id:?}");
            }
        }
    }
    // In the URL: an encoded slash or a `..` in a segment, and over-long names.
    for path in [
        "..%2F..%2Fetc/passwd",
        "org/..%2F..%2Fconfig",
        "a..b/name",
        "org/name%20x",
        "org/name%3Bid",
        &long,
    ] {
        for route in ["/community/info/", "/community/"] {
            let method = if route == "/community/" {
                M::DELETE
            } else {
                M::GET
            };
            let (status, body) = b
                .call(
                    method.clone(),
                    &format!("{route}{path}"),
                    &b.owner,
                    Value::Null,
                )
                .await;
            assert_eq!(status, 400, "{method} {route}{path}: {body}");
            assert_eq!(body["error"], "That isn't a package name.");
        }
    }
    assert!(
        !b.config.path().join("community").exists(),
        "no work folder for a bad name"
    );
    let log = std::fs::read_to_string(fake_box().join("calls.log")).unwrap_or_default();
    for needle in ["pwned", "$(id)", "passwd", "..", "na me", "'quote", &long] {
        assert!(!log.contains(needle), "{needle:?} reached a command: {log}");
    }
}

#[tokio::test]
async fn the_catalog_guesses_what_fits_from_the_name() {
    let b = harness().await;
    let (status, body) = b
        .call(M::GET, "/community/catalog", &b.member, Value::Null)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["all"], false);
    let models = body["models"].as_array().unwrap();
    // The entry with no id is dropped.
    assert_eq!(models.len(), 4, "{body}");
    let find = |id: &str| models.iter().find(|m| m["id"] == id).unwrap().clone();
    let qwen = find("tt-community/qwen3-p300x2");
    assert_eq!(
        (
            qwen["fits"].clone(),
            qwen["fit"].clone(),
            qwen["author"].clone()
        ),
        (
            json!(true),
            json!("Uses all 4 chips"),
            json!("tt-community")
        )
    );
    assert_eq!(qwen["name"], "qwen3-p300x2");
    assert_eq!(qwen["updated"], "2026-09-01");
    let galaxy = find("someone/llama-galaxy");
    assert_eq!(galaxy["fits"], false);
    assert_eq!(galaxy["fit"], "Made for a Galaxy, not this box.");
    assert_eq!(find("x/thing-p150")["fit"], "Uses 1 chip");
    assert_eq!(
        find("x/unknown")["fits"],
        Value::Null,
        "no guess from the name"
    );
    assert!(!calls("tt-model search --catalog --arch blackhole").is_empty());

    // Every published package (not just the catalog): tt-model's answer isn't
    // a listing here, which is a bad gateway, not an empty catalog.
    let (status, body) = b
        .call(M::GET, "/community/catalog?all=true", &b.owner, Value::Null)
        .await;
    assert_eq!(status, 502, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("community catalog"));
}

#[tokio::test]
async fn package_details_tt_model_cant_give_are_a_bad_gateway() {
    let b = harness().await;
    let (status, body) = b
        .call(
            M::GET,
            "/community/info/tt-community/no-details",
            &b.member,
            Value::Null,
        )
        .await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(
        body["error"],
        "Couldn't read tt-community/no-details's details: Error: couldn't reach the hub"
    );
    assert_eq!(calls("tt-model info tt-community/no-details").len(), 1);
}

#[tokio::test]
async fn the_package_list_shows_whats_on_the_box_and_failed_downloads() {
    let b = harness().await;
    // A download that failed, and one that finished (nothing left to show).
    let failed = b.work_dir("tt-community/failed-dl");
    std::fs::create_dir_all(&failed).unwrap();
    std::fs::write(
        failed.join("pull.log"),
        "fetching\n\u{1b}[31mError: 401 Unauthorized\u{1b}[0m\n",
    )
    .unwrap();
    std::fs::write(failed.join("pull.exit"), "1\n").unwrap();
    let done = b.work_dir("tt-community/finished-dl");
    std::fs::create_dir_all(&done).unwrap();
    std::fs::write(done.join("pull.log"), "done\n").unwrap();
    std::fs::write(done.join("pull.exit"), "0\n").unwrap();

    let (status, body) = b.call(M::GET, "/community", &b.owner, Value::Null).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["available"], true);
    let installed = body["installed"].as_array().unwrap();
    let ids: Vec<&str> = installed
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "tt-community/fits-p300x2",
            "tt-community/galaxy-only",
            "tt-community/shares-weights"
        ],
        "sorted by id"
    );
    let fits = &installed[0];
    assert_eq!(fits["fits"], true);
    assert_eq!(fits["fit"], "Uses all 4 chips");
    assert_eq!(fits["chips"], 4);
    assert_eq!(fits["hardware"], "p300x2");
    assert_eq!(fits["tools"], true);
    assert_eq!(fits["max_num_seqs"], 8);
    assert_eq!(fits["weights"], "tt-community/weights-not-here");
    assert_eq!(fits["name"], "fits-p300x2");
    assert_eq!(installed[1]["fits"], false);
    assert_eq!(installed[1]["fit"], "Made for galaxy, not this box.");
    assert_eq!(installed[2]["fit"], "Uses 2 chips");

    let pulls = body["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1, "only the failed one: {body}");
    assert_eq!(pulls[0]["id"], "tt-community/failed-dl");
    assert_eq!(pulls[0]["failed"], true);
    assert_eq!(pulls[0]["running"], false);
    assert_eq!(pulls[0]["paused"], false);
    assert_eq!(
        pulls[0]["line"], "Error: 401 Unauthorized",
        "colours stripped"
    );
    assert_eq!(body["starting"], Value::Null);
}

#[tokio::test]
async fn a_download_already_running_isnt_started_twice() {
    let b = harness().await;
    let id = "tt-community/already-pulling";
    let dir = b.work_dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pull.log"), "weights tt-community/w@main 40%\n").unwrap();
    mark_active(&pull_unit(id));

    let (status, body) = b
        .call(M::POST, "/community/pull", &b.member, json!({ "id": id }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "pulling": true }));
    assert!(calls(&format!("--unit={}", pull_unit(id))).is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.join("pull.log")).unwrap(),
        "weights tt-community/w@main 40%\n",
        "its log is left alone"
    );
}

#[tokio::test]
async fn a_download_runs_in_its_own_unit_with_the_token_as_a_private_file() {
    use std::os::unix::fs::PermissionsExt;
    let b = harness().await;
    let id = "tt-community/Fresh.Pkg";
    let dir = b.work_dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    // Left from an earlier try that was paused, then failed.
    std::fs::write(dir.join("pull.log"), "old lines\n").unwrap();
    std::fs::write(dir.join("pull.exit"), "1").unwrap();
    std::fs::write(dir.join("paused"), "1").unwrap();

    let (status, body) = b
        .call(M::POST, "/community/pull", &b.owner, json!({ "id": id }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "pulling": true }));
    assert_eq!(std::fs::read_to_string(dir.join("pull.log")).unwrap(), "");
    assert!(!dir.join("pull.exit").exists());
    assert!(!dir.join("paused").exists());

    let unit = pull_unit(id);
    assert_eq!(unit, "tt-station-pull-tt-community__fresh-pkg");
    let launched = calls(&format!("--unit={unit}"));
    assert_eq!(launched.len(), 1, "{launched:?}");
    let line = &launched[0];
    assert!(
        line.starts_with("systemd-run --user --collect --quiet"),
        "{line}"
    );
    assert!(
        line.contains(&format!(
            "--property=StandardOutput=append:{}",
            dir.join("pull.log").display()
        )),
        "{line}"
    );
    assert!(
        line.contains("pull 'tt-community/Fresh.Pkg' --with-weights"),
        "{line}"
    );
    assert!(line.contains(&format!("echo $? > '{}'", dir.join("pull.exit").display())));
    // The token reaches tt-model as a private file, never as a value in argv.
    assert!(!line.contains(TOKEN), "{line}");
    let token_file = fake_box().join("run/tt-station/hf_token");
    assert!(
        line.contains(&format!("--setenv=HF_TOKEN_PATH={}", token_file.display())),
        "{line}"
    );
    assert_eq!(std::fs::read_to_string(&token_file).unwrap(), TOKEN);
    let mode = std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o400);
    // Its details were asked for first (for the free-space check).
    assert!(!calls("tt-model info tt-community/Fresh.Pkg").is_empty());
}

#[tokio::test]
async fn a_download_that_cant_be_launched_says_why() {
    let b = harness().await;
    let id = "tt-community/fail-to-launch";
    let (status, body) = b
        .call(M::POST, "/community/pull", &b.owner, json!({ "id": id }))
        .await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(
        body["error"],
        "Failed to start transient service unit: Unit already exists."
    );
}

#[tokio::test]
async fn pausing_needs_a_running_download_and_keeps_what_it_has() {
    let b = harness().await;
    let id = "tt-community/pause-me";
    let unit = pull_unit(id);
    let dir = b.work_dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pull.log"), "docker load\n").unwrap();

    let (status, body) = b
        .call(
            M::POST,
            "/community/pull/pause",
            &b.member,
            json!({ "id": id }),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "It isn't downloading.");
    assert!(!dir.join("paused").exists());
    assert!(calls(&format!("stop {unit}")).is_empty());

    mark_active(&unit);
    let (status, body) = b
        .call(
            M::POST,
            "/community/pull/pause",
            &b.member,
            json!({ "id": id }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "paused": true }));
    assert_eq!(calls(&format!("systemctl --user stop {unit}")).len(), 1);
    assert!(dir.join("paused").exists(), "marked paused by the person");
    assert_eq!(
        std::fs::read_to_string(dir.join("pull.log")).unwrap(),
        "docker load\n",
        "what it has is kept"
    );
}

/// A weights repo in the scratch Hugging Face cache: a blob in the shared
/// store, linked from the repo. Returns the blob.
fn cached_weights(repo: &str, bytes: usize) -> PathBuf {
    let hub = fake_box().join(".cache/huggingface/hub");
    let blob = hub.join("blobs").join(repo.replace('/', "-"));
    std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
    std::fs::write(&blob, vec![7u8; bytes]).unwrap();
    let blobs = hub
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    std::os::unix::fs::symlink(&blob, blobs.join("abc123")).unwrap();
    blob
}

#[tokio::test]
async fn cancelling_forgets_a_download_and_can_free_its_weights() {
    let b = harness().await;
    let hub = fake_box().join(".cache/huggingface/hub");

    // Without `delete_files`: forgotten, weights kept.
    let keep = "tt-community/cancel-keep";
    let blob = cached_weights("tt-community/cancel-keep-weights", 5000);
    let dir = b.work_dir(keep);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pull.log"),
        "weights tt-community/cancel-keep-weights@main\n",
    )
    .unwrap();
    std::fs::write(dir.join("paused"), "1").unwrap();
    mark_active(&pull_unit(keep));
    let (status, body) = b
        .call(
            M::POST,
            "/community/pull/cancel",
            &b.member,
            json!({ "id": keep }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "cancelled": true, "freed_bytes": 0 }));
    assert_eq!(
        calls(&format!("systemctl --user stop {}", pull_unit(keep))).len(),
        1
    );
    for f in ["pull.log", "pull.exit", "paused"] {
        assert!(!dir.join(f).exists(), "{f}");
    }
    assert!(blob.exists());

    // With it: the repo and the blob only it linked to are gone.
    let free = "tt-community/cancel-free";
    let blob = cached_weights("tt-community/cancel-free-weights", 7000);
    let dir = b.work_dir(free);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pull.log"),
        "✓ weights tt-community/cancel-free-weights@main\n",
    )
    .unwrap();
    std::fs::write(dir.join("pull.exit"), "1").unwrap();
    let (status, body) = b
        .call(
            M::POST,
            "/community/pull/cancel",
            &b.owner,
            json!({ "id": free, "delete_files": true }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["cancelled"], true);
    assert_eq!(body["freed_bytes"], 7000, "{body}");
    assert!(!blob.exists());
    assert!(!hub
        .join("models--tt-community--cancel-free-weights")
        .exists());
    assert!(!dir.join("pull.exit").exists());
    // Not running, so nothing to stop.
    assert!(calls(&format!("stop {}", pull_unit(free))).is_empty());
}

#[tokio::test]
async fn cancelling_keeps_weights_another_package_on_the_box_uses() {
    let b = harness().await;
    let id = "tt-community/cancel-shared";
    // `tt-community/shares-weights` (installed) uses these.
    let blob = cached_weights("tt-community/shared-weights", 3000);
    let dir = b.work_dir(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pull.log"),
        "weights tt-community/shared-weights@main\n",
    )
    .unwrap();
    let (status, body) = b
        .call(
            M::POST,
            "/community/pull/cancel",
            &b.owner,
            json!({ "id": id, "delete_files": true }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["freed_bytes"], 0);
    assert_eq!(
        body["kept"],
        "Another model on the box uses the same weights, so they were kept."
    );
    assert!(blob.exists());
    assert!(fake_box()
        .join(".cache/huggingface/hub/models--tt-community--shared-weights")
        .exists());
    assert!(
        !dir.join("pull.log").exists(),
        "the download is still forgotten"
    );
}

#[tokio::test]
async fn starts_are_refused_before_the_chips_are_touched() {
    let _serving = SERVING.lock().await;
    let b = harness().await;
    let serve = |id: &str, extra: Value| {
        let mut body = json!({ "id": id });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };

    // Not downloaded.
    let (status, body) = b
        .call(
            M::POST,
            "/community/serve",
            &b.member,
            serve("tt-community/not-pulled", json!({})),
        )
        .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], "Download it first.");

    // Made for other hardware.
    let (status, body) = b
        .call(
            M::POST,
            "/community/serve",
            &b.member,
            serve(
                "tt-community/galaxy-only",
                json!({ "anyway": true, "stop_serving": true }),
            ),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "Made for galaxy, not this box.");

    // Weights not on the box (and no `anyway`).
    let (status, body) = b
        .call(
            M::POST,
            "/community/serve",
            &b.owner,
            serve("tt-community/fits-p300x2", json!({})),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["needs_weights"], true);
    assert_eq!(
        (body["have"].clone(), body["need"].clone()),
        (json!(0), json!(0))
    );
    assert!(body["error"]
        .as_str()
        .unwrap()
        .starts_with("Its weights aren't on the box yet."));

    // A model serving, and not asked to stop it.
    b.state.adopt_serving(libttstation::model::Endpoint {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "org/something-else".into(),
        requires_key: false,
    });
    // Past every other check: only the serving model stands in the way of a
    // real start, so make sure it does before asking.
    assert!(matches!(b.state.lifecycle().phase, Phase::Serving));
    let (status, body) = b
        .call(
            M::POST,
            "/community/serve",
            &b.owner,
            serve("tt-community/fits-p300x2", json!({ "anyway": true })),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["needs_stop"], true, "{body}");

    // The chips held by a bring-up: refused even when asked to replace what's serving.
    b.state.set_bringup_busy(true);
    assert!(
        b.state.chips_holder().is_some(),
        "nothing else would stop this start"
    );
    let (status, body) = b
        .call(
            M::POST,
            "/community/serve",
            &b.owner,
            serve(
                "tt-community/fits-p300x2",
                json!({ "anyway": true, "stop_serving": true }),
            ),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        body["error"],
        "A model bring-up is using the chips. Stop it in Bring-up first."
    );
    b.state.set_bringup_busy(false);

    // None of it started anything.
    assert!(
        calls("tt-model serve").is_empty(),
        "{:?}",
        calls("tt-model serve")
    );
    assert!(calls("tt-model profiles").is_empty());
    assert!(calls("--unit=tt-orchard-serve").is_empty());
    assert!(!b.config.path().join("community/serving.json").exists());
}

#[tokio::test]
async fn removing_a_package_drops_its_folder_and_only_its_folder() {
    let b = harness().await;
    let gone = "tt-community/remove-me";
    let other = "tt-community/keep-me";
    for id in [gone, other] {
        std::fs::create_dir_all(b.work_dir(id)).unwrap();
        std::fs::write(b.work_dir(id).join("check.json"), "{}").unwrap();
    }
    let (status, body) = b
        .call(
            M::DELETE,
            &format!("/community/{gone}"),
            &b.member,
            Value::Null,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "removed": gone }));
    assert_eq!(calls(&format!("tt-model rm {gone}")).len(), 1);
    assert!(!b.work_dir(gone).exists());
    assert!(b.work_dir(other).join("check.json").exists());

    // tt-model refusing: its last line, and the folder stays.
    let locked = "tt-community/locked-pkg";
    std::fs::create_dir_all(b.work_dir(locked)).unwrap();
    let (status, body) = b
        .call(
            M::DELETE,
            &format!("/community/{locked}"),
            &b.owner,
            Value::Null,
        )
        .await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(body["error"], "Error: tt-community/locked-pkg is in use");
    assert!(b.work_dir(locked).exists());
}

#[tokio::test]
async fn the_serving_package_cant_be_removed_and_a_stale_record_is_dropped() {
    let _serving = SERVING.lock().await;
    let b = harness().await;
    let id = "tt-community/serving-now";
    let record = b.config.path().join("community/serving.json");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, json!([id, "org/Served-Model"]).to_string()).unwrap();
    b.state.adopt_serving(libttstation::model::Endpoint {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "org/served-model".into(),
        requires_key: false,
    });

    let (status, body) = b
        .call(
            M::DELETE,
            &format!("/community/{id}"),
            &b.owner,
            Value::Null,
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "It's serving. Stop it first.");
    assert!(calls(&format!("tt-model rm {id}")).is_empty());
    let (_, list) = b.call(M::GET, "/community", &b.owner, Value::Null).await;
    assert_eq!(list["serving"], id, "{list}");

    // Something else serves now (started some other way): the record is
    // stale, cleared, and the package can go.
    b.state.adopt_serving(libttstation::model::Endpoint {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "org/another-model".into(),
        requires_key: false,
    });
    let (_, list) = b.call(M::GET, "/community", &b.owner, Value::Null).await;
    assert_eq!(list["serving"], Value::Null, "{list}");
    assert!(!record.exists(), "stale record removed");
    let (status, body) = b
        .call(
            M::DELETE,
            &format!("/community/{id}"),
            &b.owner,
            Value::Null,
        )
        .await;
    assert_eq!(status, 200, "{body}");
}
