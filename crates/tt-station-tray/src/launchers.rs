//! The Connect actions (Open WebUI, opencode, copy the `/v1` URL), as pure builders plus a
//! small [`Runner`] trait that does the actual process work.
//!
//! **Ported from `box-panel/panel_launchers.py`.** The container name (`ttstation-openwebui`),
//! the image (`ghcr.io/open-webui/open-webui:main`), the `host.docker.internal` wiring, the
//! data volume and the opencode config shape are the same as the box panel's. Three things
//! differ, on purpose:
//!
//! * **No shell string for docker.** The panel runs one `bash -lc` script. Here each docker step
//!   is its own argument vector ([`OpenWebUiPlan`]), and the "is it already running?" test is a
//!   pure function over `docker inspect` output ([`reuse_running`]). The panel's pull-retry loop
//!   is gone: `docker run` pulls a missing image itself.
//! * **The endpoint may be on another machine.** The panel always runs on the box that
//!   serves, so it always rewrites the endpoint to `host.docker.internal`. The tray runs on a
//!   workstation and the endpoint may be a QuietBox on the LAN. Only a loopback endpoint
//!   (`localhost`, `127.x.x.x`, `::1`) is rewritten to `host.docker.internal:<port>`, because
//!   inside the container loopback is the container itself. A LAN address is passed through as is.
//! * **A running container is reused only when it points at the same endpoint.** The panel only
//!   ever had one endpoint. The tray can switch between stations, so a running Open WebUI wired
//!   to another station is replaced.
//!
//! **Testing.** Everything that builds an argv or config text is a pure function with unit
//! tests below. The flows ([`launch_open_webui`], [`launch_opencode`], [`copy_to_clipboard`],
//! [`notify`]) only call the [`Runner`] they are given. Tests pass a recording fake, so no test
//! starts docker, a terminal, a clipboard tool or `notify-send`. [`SystemRunner`] is the real
//! implementation and is not exercised by tests.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::station::ServingNow;

/// The Open WebUI image, as in `panel_launchers.OPENWEBUI_IMAGE`.
pub const OPENWEBUI_IMAGE: &str = "ghcr.io/open-webui/open-webui:main";
/// The container (and data volume) name, as in `panel_launchers.OPENWEBUI_CONTAINER`.
pub const OPENWEBUI_CONTAINER: &str = "ttstation-openwebui";
/// The port Open WebUI listens on inside its container.
pub const OPENWEBUI_INTERNAL_PORT: u16 = 8080;
/// The host port Open WebUI is published on, as the panel's `TTS_OPENWEBUI_PORT` default.
pub const DEFAULT_OPENWEBUI_PORT: u16 = 3000;
/// How long [`launch_open_webui`] waits for Open WebUI's `/health` (the panel waits ~180 s: a
/// first run pulls the image and initialises its database).
pub const OPENWEBUI_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// What a finished command left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub success: bool,
    pub stdout: String,
}

/// The process work the Connect actions need. The real one is [`SystemRunner`]; tests use a
/// recording fake. Every method takes an argument vector. The only shell string in this file is
/// the `bash -lc` line opencode is started with (see [`opencode_terminal_argv`]).
pub trait Runner: Send + Sync {
    /// Absolute path of a tool, or `None` when it is not installed.
    fn resolve(&self, tool: &str) -> Option<PathBuf>;
    /// Run `argv` to completion with stdin closed and return its stdout.
    fn output(&self, argv: &[String]) -> std::io::Result<RunOutput>;
    /// Run `argv` to completion with `input` on stdin. Returns whether it exited 0.
    fn run_with_input(&self, argv: &[String], input: &str) -> std::io::Result<bool>;
    /// Start `argv` and do not wait for it (a terminal window, a browser, a notification).
    fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()>;
    /// Wait until `url` answers HTTP 200, up to `timeout`. Returns whether it did.
    fn wait_http_ok(&self, url: &str, timeout: Duration) -> bool;
}

// ---------------------------------------------------------------------------------------------
// URL handling (no URL crate in the tree for this crate; the shapes here are simple)
// ---------------------------------------------------------------------------------------------

/// The parts of an `http://host:port/path` URL the launchers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    /// Host as written, without brackets for an IPv6 literal.
    pub host: String,
    pub port: u16,
    /// Everything from the first `/` after the authority, or empty.
    pub path: String,
}

/// Parse an `http` or `https` URL. A missing port becomes 80 or 443. Returns `None` for
/// anything else (no scheme, empty host, a port that is not a number).
pub fn parse_url(url: &str) -> Option<UrlParts> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(UrlParts {
        scheme,
        host,
        port,
        path: path.to_string(),
    })
}

/// Whether `host` names this machine's loopback interface.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

// ---------------------------------------------------------------------------------------------
// Open WebUI
// ---------------------------------------------------------------------------------------------

/// The docker steps that (re)start Open WebUI for one endpoint, each an argument vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWebUiPlan {
    /// `docker inspect -f <format> ttstation-openwebui`: prints `true`/`false` and the env.
    pub inspect: Vec<String>,
    /// `docker rm -f ttstation-openwebui`, run before a fresh start. Its failure is ignored.
    pub remove: Vec<String>,
    /// `docker run -d ...`.
    pub run: Vec<String>,
    /// The OpenAI base URL the container is given.
    pub api_base: String,
    /// `http://localhost:<host_port>/health`.
    pub health_url: String,
    /// `http://localhost:<host_port>`, opened in the browser when ready.
    pub open_url: String,
}

/// The `docker inspect` format [`OpenWebUiPlan::inspect`] uses: the running flag, then each
/// environment entry, space separated. [`reuse_running`] reads it.
pub const INSPECT_FORMAT: &str = "{{.State.Running}}{{range .Config.Env}} {{.}}{{end}}";

/// The OpenAI base URL Open WebUI's container should use for `base_url`.
///
/// A loopback endpoint becomes `http://host.docker.internal:<port><path>` (the container's own
/// loopback is not this machine). Anything else is returned unchanged. An empty path becomes
/// `/v1`, matching the panel, which always appended `/v1`.
pub fn container_api_base(base_url: &str) -> Result<String, String> {
    let u = parse_url(base_url)
        .ok_or_else(|| format!("the serving URL {base_url:?} is not an http URL"))?;
    if !is_loopback(&u.host) {
        return Ok(base_url.trim_end_matches('/').to_string());
    }
    let path = if u.path.is_empty() || u.path == "/" {
        "/v1".to_string()
    } else {
        u.path.trim_end_matches('/').to_string()
    };
    Ok(format!(
        "{}://host.docker.internal:{}{}",
        u.scheme, u.port, path
    ))
}

/// Build the docker steps for `endpoint`, with Open WebUI published on `host_port`. `docker` is
/// the resolved docker binary. Fails with a message when the endpoint has no usable URL.
pub fn open_webui_command(
    docker: &str,
    endpoint: &ServingNow,
    host_port: u16,
) -> Result<OpenWebUiPlan, String> {
    if endpoint.base_url.trim().is_empty() {
        return Err(format!(
            "{} is serving, but its /v1 URL is not known yet",
            endpoint.model
        ));
    }
    let api_base = container_api_base(&endpoint.base_url)?;
    let s = |v: &str| v.to_string();
    let c = OPENWEBUI_CONTAINER;
    Ok(OpenWebUiPlan {
        inspect: vec![s(docker), s("inspect"), s("-f"), s(INSPECT_FORMAT), s(c)],
        remove: vec![s(docker), s("rm"), s("-f"), s(c)],
        run: vec![
            s(docker),
            s("run"),
            s("-d"),
            s("--name"),
            s(c),
            s("-p"),
            format!("{host_port}:{OPENWEBUI_INTERNAL_PORT}"),
            s("--add-host=host.docker.internal:host-gateway"),
            s("-e"),
            format!("OPENAI_API_BASE_URL={api_base}"),
            s("-e"),
            s("OPENAI_API_KEY=sk-none"),
            s("-e"),
            s("WEBUI_AUTH=false"),
            s("-v"),
            format!("{c}:/app/backend/data"),
            s(OPENWEBUI_IMAGE),
        ],
        api_base,
        health_url: format!("http://localhost:{host_port}/health"),
        open_url: format!("http://localhost:{host_port}"),
    })
}

/// Whether a running Open WebUI can be reused: `docker inspect` said it is running and its
/// environment has `OPENAI_API_BASE_URL=<api_base>` exactly. `inspect_stdout` is the output of
/// [`OpenWebUiPlan::inspect`].
pub fn reuse_running(inspect_stdout: &str, api_base: &str) -> bool {
    let mut words = inspect_stdout.split_whitespace();
    let running = words.next() == Some("true");
    let want = format!("OPENAI_API_BASE_URL={api_base}");
    running && words.any(|w| w == want)
}

/// Start (or reuse) Open WebUI for `endpoint` and open it in the browser.
///
/// Steps: resolve docker (when it is missing, the result is a message); inspect the container and
/// reuse it if it already points at this endpoint; otherwise remove it and `docker run` a new
/// one; wait for `/health`; open the page with `xdg-open` when that is installed. Returns the
/// message to show.
pub fn launch_open_webui(
    r: &dyn Runner,
    endpoint: &ServingNow,
    host_port: u16,
) -> Result<String, String> {
    let docker = r
        .resolve("docker")
        .ok_or_else(|| missing_tool_message("docker", "install docker, then try again"))?;
    let plan = open_webui_command(&docker.to_string_lossy(), endpoint, host_port)?;
    let reuse = r
        .output(&plan.inspect)
        .map(|o| o.success && reuse_running(&o.stdout, &plan.api_base))
        .unwrap_or(false);
    if !reuse {
        // A missing container makes `rm -f` fail; that is expected.
        let _ = r.output(&plan.remove);
        let out = r
            .output(&plan.run)
            .map_err(|e| format!("could not run docker: {e}"))?;
        if !out.success {
            return Err(format!(
                "docker run for Open WebUI failed: {}",
                out.stdout.trim()
            ));
        }
    }
    if !r.wait_http_ok(&plan.health_url, OPENWEBUI_READY_TIMEOUT) {
        return Err(format!(
            "Open WebUI did not answer on {} in time; try again shortly",
            plan.open_url
        ));
    }
    open_url(r, &plan.open_url, "Open WebUI is ready")
}

/// Open `url` with `xdg-open` when installed. Without it, the message names the URL.
fn open_url(r: &dyn Runner, url: &str, what: &str) -> Result<String, String> {
    match r.resolve("xdg-open") {
        Some(x) => {
            r.spawn_detached(&[x.to_string_lossy().into_owned(), url.to_string()])
                .map_err(|e| format!("{what} at {url}, but xdg-open failed: {e}"))?;
            Ok(format!("{what}: opened {url}"))
        }
        None => Ok(format!("{what} at {url} (xdg-open is not installed)")),
    }
}

// ---------------------------------------------------------------------------------------------
// opencode
// ---------------------------------------------------------------------------------------------

/// The `opencode.json` text that registers the `ttstation` provider for `endpoint`.
///
/// Same shape as `panel_launchers.build_opencode_config`: opencode splits the selection id
/// `ttstation/<model>` on the FIRST `/`, so a vendored id such as `meta-llama/Llama-3.3-70B`
/// still resolves under the `ttstation` provider. The full model id is registered with its
/// slashes and is not split again. Keys are sorted and indented by two spaces, as the panel's
/// `json.dumps(..., indent=2, sort_keys=True)` does.
pub fn opencode_config(endpoint: &ServingNow) -> String {
    let model = &endpoint.model;
    let mut models = serde_json::Map::new();
    models.insert(
        model.clone(),
        serde_json::json!({ "name": format!("{model} (TT)") }),
    );
    let cfg = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "ttstation": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "TT Station",
                "options": { "baseURL": endpoint.base_url },
                "models": models,
            }
        },
        "model": format!("ttstation/{model}"),
    });
    // serde_json's map is ordered by key here (no `preserve_order`), so this is sorted.
    serde_json::to_string_pretty(&cfg).unwrap_or_default()
}

/// The per-endpoint directory name for the opencode config: the base URL without its scheme,
/// with every character other than ASCII letters, digits, `.` and `-` replaced by `_`. The panel
/// replaced only `/` and `:`; here anything else is replaced too, so the name can never hold a
/// quote and is safe inside the single-quoted `cd` in [`opencode_terminal_argv`].
pub fn opencode_dir_name(base_url: &str) -> String {
    let bare = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    bare.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A terminal emulator and the flag after which it takes the command to run, or `None`.
///
/// Same order and flags as `panel_launchers.resolve_terminal_emulator`: x-terminal-emulator
/// (`-e`), gnome-terminal (`--`, because its `-e` takes one deprecated string), konsole (`-e`),
/// xterm (`-e`).
pub fn terminal_prefix(r: &dyn Runner) -> Option<Vec<String>> {
    const CANDIDATES: [(&str, &str); 4] = [
        ("x-terminal-emulator", "-e"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xterm", "-e"),
    ];
    CANDIDATES.iter().find_map(|(term, sep)| {
        r.resolve(term)
            .map(|p| vec![p.to_string_lossy().into_owned(), sep.to_string()])
    })
}

/// The argv that opens a terminal running opencode in `dir`: `<term> <sep> bash -lc "cd '<dir>'
/// && opencode"`, as the panel does. The login shell is there so `PATH` finds opencode (a
/// desktop session's `PATH` is often shorter than a shell's). Refuses a directory containing a
/// single quote, which would break out of the quoting.
pub fn opencode_terminal_argv(term: &[String], dir: &Path) -> Result<Vec<String>, String> {
    let d = dir.to_string_lossy();
    if d.contains('\'') {
        return Err(format!(
            "the opencode config directory {d:?} contains a quote; set XDG_DATA_HOME to a plain path"
        ));
    }
    let mut argv = term.to_vec();
    argv.extend([
        "bash".to_string(),
        "-lc".to_string(),
        format!("cd '{d}' && opencode"),
    ]);
    Ok(argv)
}

/// Write the opencode config for `endpoint` under `data_dir/tt-station/opencode/<name>/` and
/// open a terminal running opencode there. A missing opencode or terminal is a message.
pub fn launch_opencode(
    r: &dyn Runner,
    endpoint: &ServingNow,
    data_dir: &Path,
) -> Result<String, String> {
    if endpoint.base_url.trim().is_empty() {
        return Err(format!(
            "{} is serving, but its /v1 URL is not known yet",
            endpoint.model
        ));
    }
    if r.resolve("opencode").is_none() {
        return Err(missing_tool_message(
            "opencode",
            "install it (https://opencode.ai), then try again",
        ));
    }
    let term = terminal_prefix(r).ok_or_else(|| {
        "No terminal emulator was found (x-terminal-emulator, gnome-terminal, konsole or xterm)."
            .to_string()
    })?;
    let dir = data_dir
        .join("tt-station")
        .join("opencode")
        .join(opencode_dir_name(&endpoint.base_url));
    let argv = opencode_terminal_argv(&term, &dir)?;
    std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join("opencode.json"), opencode_config(endpoint)))
        .map_err(|e| {
            format!(
                "could not write the opencode config in {}: {e}",
                dir.display()
            )
        })?;
    r.spawn_detached(&argv)
        .map_err(|e| format!("could not open a terminal: {e}"))?;
    Ok(format!(
        "opencode opened in a terminal ({})",
        endpoint.model
    ))
}

// ---------------------------------------------------------------------------------------------
// Copy /v1 URL, notifications
// ---------------------------------------------------------------------------------------------

/// The text the Copy action puts on the clipboard: the endpoint's `/v1` base URL.
pub fn copy_text(endpoint: &ServingNow) -> String {
    endpoint.base_url.clone()
}

/// A clipboard tool that reads the text on stdin: `wl-copy` on a Wayland session, then
/// `xclip -selection clipboard`, then `xsel --clipboard --input`. `None` when none is installed.
pub fn clipboard_argv(r: &dyn Runner, wayland: bool) -> Option<Vec<String>> {
    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.push(("xclip", &["-selection", "clipboard"]));
    candidates.push(("xsel", &["--clipboard", "--input"]));
    candidates.into_iter().find_map(|(tool, args)| {
        r.resolve(tool).map(|p| {
            let mut v = vec![p.to_string_lossy().into_owned()];
            v.extend(args.iter().map(|a| a.to_string()));
            v
        })
    })
}

/// Put `text` on the clipboard with [`clipboard_argv`]. Without a tool the message still
/// carries the text, so the person can copy it from the notification.
pub fn copy_to_clipboard(r: &dyn Runner, text: &str, wayland: bool) -> Result<String, String> {
    let argv = clipboard_argv(r, wayland).ok_or_else(|| {
        format!(
            "No clipboard tool was found (install wl-clipboard or xclip). The /v1 URL is {text}"
        )
    })?;
    match r.run_with_input(&argv, text) {
        Ok(true) => Ok(format!("Copied {text}")),
        Ok(false) => Err(format!("{} failed. The /v1 URL is {text}", argv[0])),
        Err(e) => Err(format!(
            "could not run {}: {e}. The /v1 URL is {text}",
            argv[0]
        )),
    }
}

/// The `notify-send` argv for a message from the tray.
pub fn notify_argv(notify_send: &str, message: &str) -> Vec<String> {
    vec![
        notify_send.to_string(),
        "--app-name=TT Station".to_string(),
        "TT Station".to_string(),
        message.to_string(),
    ]
}

/// Show `message` as a desktop notification when `notify-send` is installed. The message is
/// always written to stderr as well, so it is never lost.
pub fn notify(r: &dyn Runner, message: &str) {
    eprintln!("tt-station-tray: {message}");
    if let Some(n) = r.resolve("notify-send") {
        if let Err(e) = r.spawn_detached(&notify_argv(&n.to_string_lossy(), message)) {
            eprintln!("tt-station-tray: notify-send failed: {e}");
        }
    }
}

/// "<tool> is not installed: <what to do>."
pub fn missing_tool_message(tool: &str, what_to_do: &str) -> String {
    format!("{tool} is not installed: {what_to_do}.")
}

// ---------------------------------------------------------------------------------------------
// The real runner
// ---------------------------------------------------------------------------------------------

/// Runs real processes. Not used by tests.
pub struct SystemRunner;

impl Runner for SystemRunner {
    /// Probes `~/.local/bin`, `/usr/local/bin` and `/usr/bin` before `PATH`, like the panel's
    /// `resolve_tool`, because a desktop session's `PATH` may lack them.
    fn resolve(&self, tool: &str) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/bin"));
        }
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        dirs.into_iter()
            .map(|d| d.join(tool))
            .find(|p| is_executable(p))
    }

    fn output(&self, argv: &[String]) -> std::io::Result<RunOutput> {
        let (prog, args) = split(argv)?;
        let out = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        let mut stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            // The failure reason is on stderr; keep it with the output for the message.
            stdout.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Ok(RunOutput {
            success: out.status.success(),
            stdout,
        })
    }

    fn run_with_input(&self, argv: &[String], input: &str) -> std::io::Result<bool> {
        let (prog, args) = split(argv)?;
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        Ok(child.wait()?.success())
    }

    fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()> {
        let (prog, args) = split(argv)?;
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // Reap it when it exits, so it does not linger as a zombie while the tray runs.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// A plain HTTP/1.0 GET over a TCP socket, once a second until `timeout`. Only used for
    /// `http://localhost:<port>/health`, so no TLS and no redirects.
    fn wait_http_ok(&self, url: &str, timeout: Duration) -> bool {
        let Some(u) = parse_url(url) else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        let path = if u.path.is_empty() { "/" } else { &u.path };
        while Instant::now() < deadline {
            if http_status_is_200(&u.host, u.port, path) {
                return true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        false
    }
}

fn split(argv: &[String]) -> std::io::Result<(&String, &[String])> {
    argv.split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn http_status_is_200(host: &str, port: u16, path: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Some(addr) = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    else {
        return false;
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split_whitespace().nth(1) == Some("200")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A [`Runner`] that runs nothing. It answers `resolve` from a fixed table and records every
    /// argv it is asked to run, so a test can assert what would have been started.
    #[derive(Default)]
    pub struct Recorder {
        pub tools: HashMap<String, PathBuf>,
        /// What `output` returns for an argv whose second word matches the key (`inspect`).
        pub outputs: HashMap<String, RunOutput>,
        pub ran: Mutex<Vec<Vec<String>>>,
        pub spawned: Mutex<Vec<Vec<String>>>,
        pub inputs: Mutex<Vec<(Vec<String>, String)>>,
        pub ready: bool,
    }

    impl Recorder {
        pub fn with(tools: &[&str]) -> Recorder {
            Recorder {
                tools: tools
                    .iter()
                    .map(|t| (t.to_string(), PathBuf::from(format!("/fake/bin/{t}"))))
                    .collect(),
                ready: true,
                ..Recorder::default()
            }
        }
        pub fn ran(&self) -> Vec<Vec<String>> {
            self.ran.lock().unwrap().clone()
        }
        pub fn spawned(&self) -> Vec<Vec<String>> {
            self.spawned.lock().unwrap().clone()
        }
    }

    impl Runner for Recorder {
        fn resolve(&self, tool: &str) -> Option<PathBuf> {
            self.tools.get(tool).cloned()
        }
        fn output(&self, argv: &[String]) -> std::io::Result<RunOutput> {
            self.ran.lock().unwrap().push(argv.to_vec());
            let key = argv.get(1).cloned().unwrap_or_default();
            Ok(self.outputs.get(&key).cloned().unwrap_or(RunOutput {
                success: key != "inspect",
                stdout: String::new(),
            }))
        }
        fn run_with_input(&self, argv: &[String], input: &str) -> std::io::Result<bool> {
            self.inputs
                .lock()
                .unwrap()
                .push((argv.to_vec(), input.to_string()));
            Ok(true)
        }
        fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()> {
            self.spawned.lock().unwrap().push(argv.to_vec());
            Ok(())
        }
        fn wait_http_ok(&self, _url: &str, _timeout: Duration) -> bool {
            self.ready
        }
    }

    fn ep(base_url: &str, model: &str) -> ServingNow {
        ServingNow {
            model: model.into(),
            base_url: base_url.into(),
        }
    }

    fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn open_webui_run_argv_matches_the_panel_and_has_no_shell() {
        let plan =
            open_webui_command("docker", &ep("http://127.0.0.1:8003/v1", "m"), 3000).unwrap();
        let run = &plan.run;
        assert_eq!(&run[..3], &["docker", "run", "-d"]);
        assert!(has_pair(run, "--name", "ttstation-openwebui"), "{run:?}");
        assert!(has_pair(run, "-p", "3000:8080"), "{run:?}");
        assert!(run.contains(&"--add-host=host.docker.internal:host-gateway".to_string()));
        assert!(
            has_pair(
                run,
                "-e",
                "OPENAI_API_BASE_URL=http://host.docker.internal:8003/v1"
            ),
            "{run:?}"
        );
        assert!(has_pair(run, "-e", "OPENAI_API_KEY=sk-none"));
        assert!(has_pair(run, "-e", "WEBUI_AUTH=false"));
        assert!(has_pair(run, "-v", "ttstation-openwebui:/app/backend/data"));
        assert_eq!(run.last().unwrap(), "ghcr.io/open-webui/open-webui:main");
        // No word is a shell: no sh/bash, no -c, nothing with shell operators.
        for argv in [&plan.inspect, &plan.remove, &plan.run] {
            assert!(!argv
                .iter()
                .any(|w| w == "bash" || w == "sh" || w == "-c" || w == "-lc"));
            assert!(
                !argv
                    .iter()
                    .any(|w| w.contains("&&") || w.contains(';') || w.contains('|')),
                "{argv:?}"
            );
        }
        assert_eq!(plan.inspect[..3], ["docker", "inspect", "-f"]);
        assert_eq!(plan.remove, ["docker", "rm", "-f", "ttstation-openwebui"]);
        assert_eq!(plan.health_url, "http://localhost:3000/health");
    }

    #[test]
    fn open_webui_uses_the_endpoint_port_and_host_port() {
        let plan =
            open_webui_command("docker", &ep("http://localhost:20000/v1", "m"), 3100).unwrap();
        assert_eq!(plan.api_base, "http://host.docker.internal:20000/v1");
        assert!(has_pair(&plan.run, "-p", "3100:8080"));
    }

    #[test]
    fn open_webui_passes_a_lan_endpoint_through() {
        let plan =
            open_webui_command("docker", &ep("http://qb2-lab.local:8003/v1", "m"), 3000).unwrap();
        assert_eq!(plan.api_base, "http://qb2-lab.local:8003/v1");
    }

    #[test]
    fn open_webui_refuses_an_endpoint_with_no_url() {
        assert!(open_webui_command("docker", &ep("", "m"), 3000).is_err());
        assert!(open_webui_command("docker", &ep("not a url", "m"), 3000).is_err());
    }

    #[test]
    fn reuse_only_a_running_container_with_the_same_endpoint() {
        let api = "http://host.docker.internal:8003/v1";
        assert!(reuse_running(
            "true PATH=/usr/bin OPENAI_API_BASE_URL=http://host.docker.internal:8003/v1 X=1\n",
            api
        ));
        assert!(!reuse_running(
            "false OPENAI_API_BASE_URL=http://host.docker.internal:8003/v1",
            api
        ));
        assert!(!reuse_running(
            "true OPENAI_API_BASE_URL=http://host.docker.internal:9000/v1",
            api
        ));
        assert!(!reuse_running("", api));
    }

    #[test]
    fn launch_open_webui_without_docker_is_a_message_and_runs_nothing() {
        let r = Recorder::with(&["xdg-open"]);
        let err = launch_open_webui(&r, &ep("http://127.0.0.1:8003/v1", "m"), 3000).unwrap_err();
        assert!(err.contains("docker is not installed"), "{err}");
        assert!(r.ran().is_empty() && r.spawned().is_empty());
    }

    #[test]
    fn launch_open_webui_removes_runs_waits_then_opens() {
        let r = Recorder::with(&["docker", "xdg-open"]);
        let msg = launch_open_webui(&r, &ep("http://127.0.0.1:8003/v1", "m"), 3000).unwrap();
        let ran = r.ran();
        assert_eq!(ran.len(), 3, "{ran:?}");
        assert_eq!(ran[0][1], "inspect");
        assert_eq!(ran[1][1], "rm");
        assert_eq!(ran[2][1], "run");
        assert_eq!(ran[2][0], "/fake/bin/docker");
        assert_eq!(
            r.spawned(),
            vec![vec![
                "/fake/bin/xdg-open".to_string(),
                "http://localhost:3000".to_string()
            ]]
        );
        assert!(msg.contains("http://localhost:3000"), "{msg}");
    }

    #[test]
    fn launch_open_webui_reuses_a_matching_running_container() {
        let mut r = Recorder::with(&["docker", "xdg-open"]);
        r.outputs.insert(
            "inspect".into(),
            RunOutput {
                success: true,
                stdout: "true OPENAI_API_BASE_URL=http://host.docker.internal:8003/v1".into(),
            },
        );
        launch_open_webui(&r, &ep("http://127.0.0.1:8003/v1", "m"), 3000).unwrap();
        let verbs: Vec<String> = r.ran().iter().map(|a| a[1].clone()).collect();
        assert_eq!(verbs, vec!["inspect"]);
    }

    #[test]
    fn launch_open_webui_not_ready_is_a_message() {
        let mut r = Recorder::with(&["docker", "xdg-open"]);
        r.ready = false;
        let err = launch_open_webui(&r, &ep("http://127.0.0.1:8003/v1", "m"), 3000).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        assert!(r.spawned().is_empty());
    }

    #[test]
    fn opencode_config_matches_the_panel_shape() {
        // Same expectations as box-panel/test_panel_launchers.py's TestOpenCodeConfig.
        let text = opencode_config(&ep("http://h:8003/v1", "meta-llama/L-70B"));
        let cfg: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg["model"], "ttstation/meta-llama/L-70B");
        let prov = &cfg["provider"]["ttstation"];
        assert_eq!(prov["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(prov["name"], "TT Station");
        assert_eq!(prov["options"]["baseURL"], "http://h:8003/v1");
        // The full id is registered, slashes intact, not split again.
        assert_eq!(
            prov["models"]["meta-llama/L-70B"]["name"],
            "meta-llama/L-70B (TT)"
        );
        assert_eq!(prov["models"].as_object().unwrap().len(), 1);
        assert_eq!(cfg["$schema"], "https://opencode.ai/config.json");
        // Sorted keys, two-space indent (the panel's sort_keys=True, indent=2).
        assert!(text.starts_with("{\n  \"$schema\""), "{text}");
        assert!(text.find("\"model\"").unwrap() < text.find("\"provider\"").unwrap());
        // Splitting the selection id on the FIRST '/' gives back the provider and the full id.
        let sel = cfg["model"].as_str().unwrap();
        let (provider, model) = sel.split_once('/').unwrap();
        assert_eq!((provider, model), ("ttstation", "meta-llama/L-70B"));
    }

    #[test]
    fn opencode_dir_name_has_no_quote_or_slash() {
        assert_eq!(opencode_dir_name("http://h:8003/v1"), "h_8003_v1");
        let odd = opencode_dir_name("http://e'vil;$(x)/v1");
        assert!(
            !odd.contains('\'') && !odd.contains('/') && !odd.contains('$'),
            "{odd}"
        );
    }

    #[test]
    fn opencode_terminal_argv_quotes_the_dir_like_the_panel() {
        let term = vec!["/usr/bin/gnome-terminal".to_string(), "--".to_string()];
        let argv = opencode_terminal_argv(
            &term,
            Path::new("/home/x/.local/share/tt-station/opencode/h_8003"),
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                "/usr/bin/gnome-terminal",
                "--",
                "bash",
                "-lc",
                "cd '/home/x/.local/share/tt-station/opencode/h_8003' && opencode"
            ]
        );
        assert!(opencode_terminal_argv(&term, Path::new("/tmp/it's")).is_err());
    }

    #[test]
    fn terminal_prefix_uses_the_panels_order_and_flags() {
        let r = Recorder::with(&["xterm", "gnome-terminal"]);
        assert_eq!(
            terminal_prefix(&r).unwrap(),
            vec!["/fake/bin/gnome-terminal", "--"]
        );
        let r = Recorder::with(&["xterm"]);
        assert_eq!(terminal_prefix(&r).unwrap(), vec!["/fake/bin/xterm", "-e"]);
        assert_eq!(terminal_prefix(&Recorder::with(&[])), None);
    }

    #[test]
    fn launch_opencode_missing_tools_are_messages_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let e = ep("http://h:8003/v1", "m");
        let r = Recorder::with(&["xterm"]);
        let err = launch_opencode(&r, &e, dir.path()).unwrap_err();
        assert!(err.contains("opencode is not installed"), "{err}");
        let r = Recorder::with(&["opencode"]);
        let err = launch_opencode(&r, &e, dir.path()).unwrap_err();
        assert!(err.contains("No terminal emulator"), "{err}");
        assert!(r.spawned().is_empty());
    }

    #[test]
    fn launch_opencode_writes_config_and_opens_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let r = Recorder::with(&["opencode", "konsole"]);
        launch_opencode(&r, &ep("http://h:8003/v1", "a/b"), dir.path()).unwrap();
        let cfg_dir = dir.path().join("tt-station/opencode/h_8003_v1");
        let text = std::fs::read_to_string(cfg_dir.join("opencode.json")).unwrap();
        assert!(text.contains("\"ttstation/a/b\""), "{text}");
        let spawned = r.spawned();
        assert_eq!(spawned.len(), 1);
        assert_eq!(
            &spawned[0][..4],
            &["/fake/bin/konsole", "-e", "bash", "-lc"]
        );
        assert_eq!(
            spawned[0][4],
            format!("cd '{}' && opencode", cfg_dir.display())
        );
    }

    #[test]
    fn copy_puts_the_v1_url_on_stdin_of_a_clipboard_tool() {
        let e = ep("http://h:8003/v1", "m");
        assert_eq!(copy_text(&e), "http://h:8003/v1");
        let r = Recorder::with(&["xclip", "wl-copy"]);
        copy_to_clipboard(&r, &copy_text(&e), true).unwrap();
        copy_to_clipboard(&r, &copy_text(&e), false).unwrap();
        let inputs = r.inputs.lock().unwrap().clone();
        assert_eq!(inputs[0].0, vec!["/fake/bin/wl-copy"]);
        assert_eq!(
            inputs[1].0,
            vec!["/fake/bin/xclip", "-selection", "clipboard"]
        );
        assert!(inputs.iter().all(|(_, text)| text == "http://h:8003/v1"));
    }

    #[test]
    fn copy_without_a_clipboard_tool_still_shows_the_url() {
        let err = copy_to_clipboard(&Recorder::with(&[]), "http://h:8003/v1", true).unwrap_err();
        assert!(err.contains("http://h:8003/v1"), "{err}");
    }

    #[test]
    fn notify_uses_notify_send_only_when_installed() {
        let r = Recorder::with(&["notify-send"]);
        notify(&r, "hello");
        assert_eq!(
            r.spawned(),
            vec![notify_argv("/fake/bin/notify-send", "hello")]
        );
        let r = Recorder::with(&[]);
        notify(&r, "hello");
        assert!(r.spawned().is_empty());
    }

    #[test]
    fn parse_url_handles_ports_paths_and_v6() {
        let u = parse_url("http://[::1]:8003/v1").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("::1", 8003, "/v1")
        );
        assert_eq!(parse_url("https://h").unwrap().port, 443);
        assert!(parse_url("ftp://h").is_none());
        assert!(parse_url("http://h:notaport/").is_none());
        assert_eq!(
            container_api_base("http://[::1]:8003/v1").unwrap(),
            "http://host.docker.internal:8003/v1"
        );
        assert_eq!(
            container_api_base("http://127.0.0.1:8003").unwrap(),
            "http://host.docker.internal:8003/v1"
        );
    }
}
