//! Shared "run options": the optional knobs a client may pass when it asks a
//! box to serve a model, plus the one place that decides which values are
//! acceptable.
//!
//! The agent daemon, the CLI client and the tray UI all use this module so that
//! they agree on three things:
//!
//! 1. **What an option is** ([`RunOptions`]): a small, closed set of fields.
//! 2. **What a safe value looks like** ([`RunOptions::validate`]): every string
//!    that can end up on a command line is checked character by character, so a
//!    value cannot carry a second flag, a space, or shell syntax.
//! 3. **What the UI should offer** ([`run_schema`]): a description of every
//!    option, generated from the same tables that drive validation.
//!
//! Pass-through vLLM flags are never free text. A flag must appear in
//! [`VLLM_ALLOWLIST`] and its value must match the flag's [`VllmKind`].
//!
//! No regex crate is used. The patterns are small and are written as plain
//! character loops so that this crate gains no new dependency.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The top-level keys a run-options object may contain. Anything else is
/// rejected by name in [`RunOptions::from_value`].
const KNOWN_FIELDS: &[&str] = &[
    "device",
    "port",
    "profile",
    "offline",
    "workflow",
    "vllm_args",
];

/// The `tt-inference-server` workflow to run.
///
/// Serialised as the lowercase variant name (`"server"`, `"benchmarks"`,
/// `"evals"`), which is also the text passed on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Workflow {
    /// Serve the model over an OpenAI-compatible `/v1` (the normal case).
    Server,
    /// Run the benchmark suite against the model.
    Benchmarks,
    /// Run the evaluation suite against the model.
    Evals,
}

impl Workflow {
    /// The lowercase name used in JSON and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Workflow::Server => "server",
            Workflow::Benchmarks => "benchmarks",
            Workflow::Evals => "evals",
        }
    }
}

/// One pass-through vLLM flag.
///
/// `flag` is stored without leading dashes (for example `"max-model-len"`);
/// the dashes are added by [`RunOptions::to_serve_args`]. `value` is `None`
/// for boolean flags and `Some(..)` for every other kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VllmArg {
    /// Flag name without leading dashes. Must be in [`VLLM_ALLOWLIST`].
    pub flag: String,
    /// The flag's value, or `None` for a boolean flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// What kind of value an allowlisted vLLM flag takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VllmKind {
    /// A base-10 integer in `min..=max` (digits only, no sign).
    Int {
        /// Smallest accepted value.
        min: u64,
        /// Largest accepted value.
        max: u64,
    },
    /// A switch that takes no value.
    Bool,
    /// A short identifier: `[a-z0-9][a-z0-9_-]{0,39}`.
    Token,
}

/// The vLLM flags a client may pass through. Anything not listed here is
/// rejected. Add an entry only after checking that the flag is safe to expose
/// and that the serving image understands it.
pub const VLLM_ALLOWLIST: &[(&str, VllmKind)] = &[
    (
        "max-model-len",
        VllmKind::Int {
            min: 1,
            max: 1_048_576,
        },
    ),
    ("max-num-seqs", VllmKind::Int { min: 1, max: 1024 }),
    (
        "max-num-batched-tokens",
        VllmKind::Int {
            min: 1,
            max: 1_048_576,
        },
    ),
    ("block-size", VllmKind::Int { min: 1, max: 1024 }),
    ("enable-auto-tool-choice", VllmKind::Bool),
    ("tool-call-parser", VllmKind::Token),
    ("reasoning-parser", VllmKind::Token),
];

/// A rejected option: which field was wrong and why.
///
/// `field` is the top-level key (`"port"`, `"vllm_args"`, ...), or `"options"`
/// when the problem cannot be pinned to one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionError {
    /// The option the error is about.
    pub field: String,
    /// A human-readable explanation, safe to show to a user.
    pub message: String,
}

impl OptionError {
    fn new(field: &str, message: impl Into<String>) -> Self {
        OptionError {
            field: field.to_string(),
            message: message.into(),
        }
    }
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid option `{}`: {}", self.field, self.message)
    }
}

impl std::error::Error for OptionError {}

/// The optional settings for one serve request. Every field is optional; an
/// empty value means "use the box's defaults".
///
/// Build one with [`RunOptions::from_value`] when the input is untrusted JSON.
/// If you construct it by hand, call [`RunOptions::validate`] before use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOptions {
    /// Target device name, for example `p300x2`. `[a-z0-9]{2,16}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Port for the served `/v1`. `1024..=65535`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// A tt-model bundle profile, passed to `tt serve --profile`. `tt serve` accepts it only
    /// for bundles and refuses it for catalog models (checked 2026-10-05 against `tt 1.0.1`; see
    /// [`serve_argv_for`]). `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Serve from already-cached weights without network access.
    #[serde(default)]
    pub offline: bool,
    /// Which workflow to run. `None` means the default (`server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<Workflow>,
    /// Allowlisted pass-through vLLM flags, in the order given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vllm_args: Vec<VllmArg>,
}

impl RunOptions {
    /// True when nothing is set, so the box's defaults apply.
    pub fn is_empty(&self) -> bool {
        *self == RunOptions::default()
    }

    /// Parse and validate untrusted JSON.
    ///
    /// Order of checks:
    /// 1. The value must be an object with only the known keys. An unknown key
    ///    is reported by name.
    /// 2. serde deserialises the fields. If that fails, each key is retried on
    ///    its own so the error names the key at fault (falling back to
    ///    `"options"`).
    /// 3. [`RunOptions::validate`] checks every value.
    pub fn from_value(v: &serde_json::Value) -> Result<RunOptions, OptionError> {
        let obj = v
            .as_object()
            .ok_or_else(|| OptionError::new("options", "must be a JSON object"))?;

        for key in obj.keys() {
            if !KNOWN_FIELDS.contains(&key.as_str()) {
                return Err(OptionError::new(key, "unknown option"));
            }
        }

        let parsed: RunOptions = match serde_json::from_value(v.clone()) {
            Ok(o) => o,
            Err(whole) => {
                // Find the key whose value does not deserialise by itself.
                for (key, val) in obj {
                    let mut one = serde_json::Map::new();
                    one.insert(key.clone(), val.clone());
                    if let Err(e) = serde_json::from_value::<RunOptions>(one.into()) {
                        return Err(OptionError::new(key, e.to_string()));
                    }
                }
                return Err(OptionError::new("options", whole.to_string()));
            }
        };

        parsed.validate()?;
        Ok(parsed)
    }

    /// Check every value against its rules. Returns the first problem found.
    pub fn validate(&self) -> Result<(), OptionError> {
        if let Some(d) = &self.device {
            if !is_device_name(d) {
                return Err(OptionError::new(
                    "device",
                    "must be 2-16 characters of a-z and 0-9",
                ));
            }
        }
        if let Some(p) = &self.profile {
            if !is_profile_name(p) {
                return Err(OptionError::new(
                    "profile",
                    "must start with a letter or digit and use only letters, digits, '.', '_' and '-' (max 64)",
                ));
            }
        }
        if let Some(port) = self.port {
            if port < 1024 {
                return Err(OptionError::new("port", "must be between 1024 and 65535"));
            }
        }

        let mut seen: Vec<&str> = Vec::new();
        for arg in &self.vllm_args {
            let kind = VLLM_ALLOWLIST
                .iter()
                .find(|(name, _)| *name == arg.flag)
                .map(|(_, kind)| *kind)
                .ok_or_else(|| {
                    OptionError::new(
                        "vllm_args",
                        format!(
                            "flag `{}` is not on the allowlist",
                            sanitize_for_message(&arg.flag)
                        ),
                    )
                })?;
            if seen.contains(&arg.flag.as_str()) {
                return Err(OptionError::new(
                    "vllm_args",
                    format!("flag `{}` was given more than once", arg.flag),
                ));
            }
            seen.push(&arg.flag);
            check_vllm_value(&arg.flag, kind, arg.value.as_deref())?;
        }
        Ok(())
    }

    /// Render the options as arguments for the serve command.
    ///
    /// Fixed order: device, port, profile, offline, workflow, then a `"--"`
    /// separator followed by each vLLM flag as `--<flag>` (and its value, if
    /// any). No `"--"` is emitted when there are no vLLM flags.
    ///
    /// This does not validate. Call [`RunOptions::validate`] (or build the
    /// value with [`RunOptions::from_value`]) first.
    pub fn to_serve_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(d) = &self.device {
            out.push("--device".to_string());
            out.push(d.clone());
        }
        if let Some(p) = self.port {
            out.push("--port".to_string());
            out.push(p.to_string());
        }
        if let Some(p) = &self.profile {
            out.push("--profile".to_string());
            out.push(p.clone());
        }
        if self.offline {
            out.push("--offline".to_string());
        }
        if let Some(w) = self.workflow {
            out.push("--workflow".to_string());
            out.push(w.as_str().to_string());
        }
        if !self.vllm_args.is_empty() {
            out.push("--".to_string());
            for a in &self.vllm_args {
                out.push(format!("--{}", a.flag));
                if let Some(v) = &a.value {
                    out.push(v.clone());
                }
            }
        }
        out
    }
}

/// `^[a-z0-9]{2,16}$`
fn is_device_name(s: &str) -> bool {
    (2..=16).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`
fn is_profile_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    s.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `^[a-z0-9][a-z0-9_-]{0,39}$`
fn is_token(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    s.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// Make a client-supplied string safe to echo in an error message: printable
/// ASCII only, truncated.
fn sanitize_for_message(s: &str) -> String {
    s.chars()
        .take(40)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

/// Check one flag's value against its kind.
fn check_vllm_value(flag: &str, kind: VllmKind, value: Option<&str>) -> Result<(), OptionError> {
    let bad = |msg: String| Err(OptionError::new("vllm_args", msg));
    match (kind, value) {
        (VllmKind::Bool, None) => Ok(()),
        (VllmKind::Bool, Some(_)) => bad(format!("flag `{flag}` takes no value")),
        (_, None) => bad(format!("flag `{flag}` needs a value")),
        (VllmKind::Int { min, max }, Some(v)) => {
            // Digits only: no sign, no whitespace, no suffix. Length cap keeps
            // the parse from overflowing u64.
            let digits = !v.is_empty() && v.len() <= 19 && v.chars().all(|c| c.is_ascii_digit());
            match v.parse::<u64>() {
                Ok(n) if digits && (min..=max).contains(&n) => Ok(()),
                _ => bad(format!(
                    "flag `{flag}` needs a whole number from {min} to {max}"
                )),
            }
        }
        (VllmKind::Token, Some(v)) => {
            if is_token(v) {
                Ok(())
            } else {
                bad(format!(
                    "flag `{flag}` needs a short lowercase name (a-z, 0-9, '_' and '-')"
                ))
            }
        }
    }
}

/// How the official `tt serve` will run a model: through tt-inference-server (a model in tt's
/// released catalog) or through tt-model (a Hub bundle id that the catalog does not cover).
///
/// The model name alone does not decide this. A Hugging Face id such as
/// `meta-llama/Llama-3.1-8B-Instruct` has a `/` in it and still resolves to the catalog model
/// `Llama-3.1-8B-Instruct` (checked 2026-10-05 with `tt --json serve
/// meta-llama/Llama-3.1-8B-Instruct --dry-run`). Only `tt` knows its catalog, so the target is
/// read from `tt --json serve <model> --dry-run` with [`ServeTarget::from_dry_run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeTarget {
    /// A catalog model, served by tt-inference-server's `run.py`.
    Catalog,
    /// A tt-model bundle (`namespace/name` not in the catalog), served by `tt-model serve`.
    Bundle,
}

impl ServeTarget {
    /// Classify from the JSON plan that `tt --json serve <model> --dry-run` prints.
    ///
    /// With `tt 1.0.1` a bundle's plan carries `"backend": "tt-model"` and an `argv` that starts
    /// with the `tt-model` binary. A catalog model's plan has no `backend` key; it carries
    /// `served_through` and an `argv` that runs tt-inference-server's `run.py`. Anything that is
    /// not explicitly `tt-model` is treated as the catalog, which is the stricter of the two:
    /// catalog models accept fewer options.
    pub fn from_dry_run(plan: &serde_json::Value) -> ServeTarget {
        match plan.get("backend").and_then(serde_json::Value::as_str) {
            Some("tt-model") => ServeTarget::Bundle,
            _ => ServeTarget::Catalog,
        }
    }
}

/// The arguments for the official `tt serve`, without the binary name: `["serve", model, ...]`.
///
/// Order: `serve`, the model, then `--port <default_port>` unless `options.port` is set (the
/// caller owns the port, so one is always given), then [`RunOptions::to_serve_args`]. When
/// `options.port` is set it appears once, inside the options, and `default_port` is not used.
///
/// This is the plain mapping, shared by the agent's `tt` backend, the tray's local station and
/// the run sheet's command preview so all three show the same command. It does not know whether
/// `tt` will treat `model` as a catalog model or a bundle; [`serve_argv_for`] adds that.
///
/// It does not validate. Build `options` with [`RunOptions::from_value`], or call
/// [`RunOptions::validate`], first. Every value that reaches the output has then passed the
/// character checks, and the output is meant to be run as an argv (no shell).
pub fn serve_argv(model: &str, default_port: u16, options: &RunOptions) -> Vec<String> {
    let mut out = vec!["serve".to_string(), model.to_string()];
    if options.port.is_none() {
        out.push("--port".to_string());
        out.push(default_port.to_string());
    }
    out.extend(options.to_serve_args());
    out
}

/// [`serve_argv`] for a known [`ServeTarget`], refusing options `tt serve` would refuse.
///
/// What `tt 1.0.1` does with each option, checked with `--dry-run` on 2026-10-05:
///
/// * **Catalog:** `--profile` and pass-through vLLM flags (after `--`) are refused with exit 2.
///   This function refuses them first, as an [`OptionError`] that names the field, so nothing is
///   started and the caller can show which input was wrong. It does not drop them silently: a
///   request that asked for `--max-model-len 4096` and got a server without it would be a
///   different server from the one asked for.
/// * **Bundle:** only the `server` workflow exists (`tt` refuses the others), so any other
///   workflow is refused here. `--detach` is added right after the model name, because without
///   it `tt serve` follows the bundle's boot instead of returning once the container is up.
///   `--detach` is a bundle-only flag: `tt` refuses it for a catalog model.
///
/// A catalog `tt serve` returns on its own once tt-inference-server's container exists, because
/// `run.py --docker-server` returns at that point for the `server` workflow.
pub fn serve_argv_for(
    target: ServeTarget,
    model: &str,
    default_port: u16,
    options: &RunOptions,
) -> Result<Vec<String>, OptionError> {
    match target {
        ServeTarget::Catalog => {
            if options.profile.is_some() {
                return Err(OptionError::new(
                    "profile",
                    format!(
                        "applies only to tt-model bundles; `{}` is a catalog model",
                        sanitize_for_message(model)
                    ),
                ));
            }
            if !options.vllm_args.is_empty() {
                return Err(OptionError::new(
                    "vllm_args",
                    format!(
                        "pass-through vLLM flags apply only to tt-model bundles; `{}` is a \
                         catalog model",
                        sanitize_for_message(model)
                    ),
                ));
            }
            Ok(serve_argv(model, default_port, options))
        }
        ServeTarget::Bundle => {
            if let Some(w) = options.workflow.filter(|w| *w != Workflow::Server) {
                return Err(OptionError::new(
                    "workflow",
                    format!(
                        "tt-model bundles support only the `server` workflow, not `{}`",
                        w.as_str()
                    ),
                ));
            }
            let mut argv = serve_argv(model, default_port, options);
            argv.insert(2, "--detach".to_string());
            Ok(argv)
        }
    }
}

/// Describes one option for a UI: its name, value kind and limits.
///
/// Vendor flags are named `vllm:<flag>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSpec {
    /// Option name, for example `port` or `vllm:max-model-len`.
    pub name: String,
    /// Value kind: `string`, `int`, `bool` or `choice`.
    pub kind: String,
    /// One-line description for the UI.
    pub description: String,
    /// Default shown by the UI, as text, when there is one.
    pub default: Option<String>,
    /// Smallest allowed value, for `int` options.
    pub min: Option<u64>,
    /// Largest allowed value, for `int` options.
    pub max: Option<u64>,
    /// Allowed values, for `choice` options (empty otherwise).
    pub choices: Vec<String>,
    /// Where the option takes effect. `"bundle"` means models started from a
    /// tt-model-manager bundle. `"catalog"` means catalog (run.py) models.
    pub applies_to: Vec<String>,
}

/// The full list of options a client can set, for building a form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSchema {
    /// One entry per top-level option, then one per allowlisted vLLM flag.
    pub options: Vec<OptionSpec>,
}

/// Build the schema. It is generated from [`VLLM_ALLOWLIST`] and the same
/// limits that [`RunOptions::validate`] enforces, so the two cannot drift.
pub fn run_schema() -> RunSchema {
    let both = || vec!["bundle".to_string(), "catalog".to_string()];
    let spec = |name: &str, kind: &str, description: &str| OptionSpec {
        name: name.to_string(),
        kind: kind.to_string(),
        description: description.to_string(),
        default: None,
        min: None,
        max: None,
        choices: Vec::new(),
        applies_to: both(),
    };

    let mut options = vec![
        spec(
            "device",
            "string",
            "Target device name, for example p300x2 (2-16 characters, a-z and 0-9).",
        ),
        OptionSpec {
            min: Some(1024),
            max: Some(65535),
            ..spec("port", "int", "Port for the served /v1.")
        },
        // `tt serve --profile` is a tt-model bundle option. `tt 1.0.1` refuses it for a
        // catalog model ("--profile only applies to a tt-model bundle"), so the schema says
        // bundle only.
        OptionSpec {
            applies_to: vec!["bundle".to_string()],
            ..spec(
                "profile",
                "string",
                "tt-model bundle profile to serve instead of the bundle's default.",
            )
        },
        OptionSpec {
            default: Some("false".to_string()),
            ..spec(
                "offline",
                "bool",
                "Serve from cached weights without network access.",
            )
        },
        OptionSpec {
            default: Some("server".to_string()),
            choices: [Workflow::Server, Workflow::Benchmarks, Workflow::Evals]
                .iter()
                .map(|w| w.as_str().to_string())
                .collect(),
            ..spec("workflow", "choice", "Which workflow to run.")
        },
    ];

    for (flag, kind) in VLLM_ALLOWLIST {
        let name = format!("vllm:{flag}");
        let description = format!("Pass --{flag} through to vLLM.");
        // `vllm:*` applies to bundle models only. Checked 2026-10-05 against
        // `tt 1.0.1`: `tt --json serve Llama-3.1-8B-Instruct --dry-run -- --max-model-len 4096`
        // exits 2 with "Unrecognized arguments for a catalog model: --max-model-len 4096".
        // Pass-through arguments go to tt-model, which serves bundles only.
        let mut s = match kind {
            VllmKind::Int { min, max } => OptionSpec {
                min: Some(*min),
                max: Some(*max),
                ..spec(&name, "int", &description)
            },
            VllmKind::Bool => spec(&name, "bool", &description),
            VllmKind::Token => spec(&name, "string", &description),
        };
        s.applies_to = vec!["bundle".to_string()];
        options.push(s);
    }

    RunSchema { options }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts(v: serde_json::Value) -> Result<RunOptions, OptionError> {
        RunOptions::from_value(&v)
    }

    #[test]
    fn empty_object_is_empty_options() {
        let o = opts(json!({})).unwrap();
        assert!(o.is_empty());
        assert!(o.to_serve_args().is_empty());
    }

    #[test]
    fn valid_options_become_serve_args_in_a_stable_order() {
        let o = opts(json!({
            "device": "p300x2", "port": 20001, "profile": "fast",
            "offline": true, "workflow": "server",
            "vllm_args": [{"flag": "max-model-len", "value": "4096"}]
        }))
        .unwrap();
        assert_eq!(
            o.to_serve_args(),
            vec![
                "--device",
                "p300x2",
                "--port",
                "20001",
                "--profile",
                "fast",
                "--offline",
                "--workflow",
                "server",
                "--",
                "--max-model-len",
                "4096"
            ]
        );
    }

    #[test]
    fn unknown_top_level_field_is_rejected_by_name() {
        let e = opts(json!({"privileged": true})).unwrap_err();
        assert_eq!(e.field, "privileged");
    }

    #[test]
    fn values_that_smuggle_flags_or_shell_syntax_are_rejected() {
        for bad in [
            "4096 --privileged",
            "x; rm -rf /",
            "a\nb",
            "--x",
            "",
            "$(id)",
            "a b",
        ] {
            let e = opts(json!({"profile": bad}));
            assert!(e.is_err(), "profile {bad:?} must be rejected");
            let e = opts(json!({"device": bad}));
            assert!(e.is_err(), "device {bad:?} must be rejected");
            let e = opts(json!({"vllm_args":[{"flag":"tool-call-parser","value":bad}]}));
            assert!(e.is_err(), "vllm value {bad:?} must be rejected");
        }
    }

    #[test]
    fn vllm_flag_must_be_on_the_allowlist_and_typed() {
        assert_eq!(
            opts(json!({"vllm_args":[{"flag":"privileged"}]}))
                .unwrap_err()
                .field,
            "vllm_args"
        );
        // int flag with a non-int value
        assert!(opts(json!({"vllm_args":[{"flag":"max-model-len","value":"abc"}]})).is_err());
        // int flag out of range
        assert!(opts(json!({"vllm_args":[{"flag":"max-model-len","value":"0"}]})).is_err());
        // bool flag takes no value
        assert!(
            opts(json!({"vllm_args":[{"flag":"enable-auto-tool-choice","value":"x"}]})).is_err()
        );
        assert!(opts(json!({"vllm_args":[{"flag":"enable-auto-tool-choice"}]})).is_ok());
        // a flag given twice
        assert!(opts(json!({"vllm_args":[
            {"flag":"max-model-len","value":"1"},{"flag":"max-model-len","value":"2"}]}))
        .is_err());
    }

    #[test]
    fn port_below_1024_is_rejected() {
        assert!(opts(json!({"port": 80})).is_err());
        assert!(opts(json!({"port": 1024})).is_ok());
        assert!(opts(json!({"port": 70000})).is_err());
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn serve_argv_places_model_first_then_options() {
        let o = opts(json!({
            "device": "p300x2", "offline": true,
            "vllm_args": [{"flag": "max-model-len", "value": "4096"}]
        }))
        .unwrap();
        let argv = serve_argv("Llama-3.1-8B-Instruct", 20000, &o);
        // The agent owns the port: it is always given, before the options.
        let mut want = strings(&["serve", "Llama-3.1-8B-Instruct", "--port", "20000"]);
        want.extend(o.to_serve_args());
        assert_eq!(argv, want);

        // No options at all: still the port.
        assert_eq!(
            serve_argv("m", 8003, &RunOptions::default()),
            strings(&["serve", "m", "--port", "8003"])
        );
    }

    #[test]
    fn serve_argv_uses_the_requested_port_once() {
        let o = opts(json!({"port": 20005})).unwrap();
        let argv = serve_argv("m", 20000, &o);
        assert_eq!(argv, strings(&["serve", "m", "--port", "20005"]));
        assert_eq!(argv.iter().filter(|a| *a == "--port").count(), 1);
        assert!(!argv.contains(&"20000".to_string()));
    }

    /// Every hostile value is stopped by `from_value`, so none can reach `serve_argv`. And for
    /// values that do pass, every argv element is either a fixed flag or text drawn from the
    /// validators' character sets: no whitespace, no shell syntax, nothing that starts a new flag.
    #[test]
    fn serve_argv_never_contains_unvalidated_text() {
        let hostile = [
            "4096 --privileged",
            "x; rm -rf /",
            "a\nb",
            "--x",
            "",
            "$(id)",
            "a b",
            "`id`",
            "a|b",
            "a&&b",
            "../../etc",
            "'quoted'",
        ];
        for bad in hostile {
            // Each refusal must come from the field under test, not from some unrelated
            // problem that would make the assertion pass for the wrong reason.
            for field in ["device", "profile", "workflow"] {
                let e = opts(json!({ field: bad }))
                    .expect_err(&format!("{field} {bad:?} must not get through to serve_argv"));
                assert_eq!(e.field, field, "{bad:?}");
            }
            for flag in ["tool-call-parser", "reasoning-parser", "max-model-len"] {
                let e = opts(json!({"vllm_args": [{"flag": flag, "value": bad}]})).expect_err(
                    &format!("vllm {flag} {bad:?} must not get through to serve_argv"),
                );
                assert_eq!(e.field, "vllm_args");
            }
            let e = opts(json!({"vllm_args": [{"flag": bad}]}))
                .expect_err(&format!("vllm flag name {bad:?} must not get through"));
            assert_eq!(e.field, "vllm_args");
        }

        let good = opts(json!({
            "device": "p300x2", "port": 20001, "profile": "fast.v2-a",
            "offline": true, "workflow": "server",
            "vllm_args": [
                {"flag": "max-model-len", "value": "4096"},
                {"flag": "tool-call-parser", "value": "llama3_json"},
                {"flag": "enable-auto-tool-choice"}
            ]
        }))
        .unwrap();
        for arg in serve_argv("Llama-3.1-8B-Instruct", 20000, &good).iter().skip(2) {
            assert!(
                arg.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
                "unexpected character in argv element {arg:?}"
            );
        }
    }

    #[test]
    fn serve_target_comes_from_the_dry_run_plan() {
        // Trimmed from `tt --json serve Adartras/qwen3.6-a3b-blackhole --dry-run` (tt 1.0.1).
        let bundle = json!({"bundle": null, "backend": "tt-model",
            "model": "Adartras/qwen3.6-a3b-blackhole", "port": null,
            "argv": ["/home/ttuser/.local/share/tenstorrent/bin/tt-model", "serve",
                     "Adartras/qwen3.6-a3b-blackhole"]});
        assert_eq!(ServeTarget::from_dry_run(&bundle), ServeTarget::Bundle);
        // Trimmed from `tt --json serve meta-llama/Llama-3.1-8B-Instruct --dry-run`: a slash in
        // the name, and still the catalog model.
        let catalog = json!({"model": "Llama-3.1-8B-Instruct", "workflow": "server",
            "served_through": null, "default_port": "20000"});
        assert_eq!(ServeTarget::from_dry_run(&catalog), ServeTarget::Catalog);
        assert_eq!(ServeTarget::from_dry_run(&json!({})), ServeTarget::Catalog);
    }

    #[test]
    fn catalog_target_refuses_bundle_only_options_by_name() {
        let vllm = opts(json!({"vllm_args": [{"flag": "max-model-len", "value": "4096"}]})).unwrap();
        let e = serve_argv_for(ServeTarget::Catalog, "Llama-3.1-8B-Instruct", 20000, &vllm)
            .unwrap_err();
        assert_eq!(e.field, "vllm_args");

        let profile = opts(json!({"profile": "fast"})).unwrap();
        let e = serve_argv_for(ServeTarget::Catalog, "Llama-3.1-8B-Instruct", 20000, &profile)
            .unwrap_err();
        assert_eq!(e.field, "profile");

        // Options a catalog model accepts pass through unchanged, with no `--detach`.
        let ok = opts(json!({"device": "p300x2", "offline": true, "workflow": "server"})).unwrap();
        assert_eq!(
            serve_argv_for(ServeTarget::Catalog, "Llama-3.1-8B-Instruct", 20000, &ok).unwrap(),
            serve_argv("Llama-3.1-8B-Instruct", 20000, &ok)
        );
    }

    #[test]
    fn bundle_target_detaches_and_keeps_pass_through_flags() {
        let o = opts(json!({"profile": "fast",
            "vllm_args": [{"flag": "max-model-len", "value": "4096"}]}))
        .unwrap();
        let argv = serve_argv_for(ServeTarget::Bundle, "ns/name", 20000, &o).unwrap();
        assert_eq!(
            argv,
            strings(&[
                "serve", "ns/name", "--detach", "--port", "20000", "--profile", "fast", "--",
                "--max-model-len", "4096"
            ])
        );

        let bench = opts(json!({"workflow": "benchmarks"})).unwrap();
        let e = serve_argv_for(ServeTarget::Bundle, "ns/name", 20000, &bench).unwrap_err();
        assert_eq!(e.field, "workflow");
    }

    /// Pins the Step 1 finding: a catalog model refuses `--` pass-through arguments and
    /// `--profile`, so the schema must not offer them for catalog models.
    #[test]
    fn schema_offers_vllm_flags_and_profile_for_bundles_only() {
        let s = run_schema();
        for o in &s.options {
            if o.name.starts_with("vllm:") || o.name == "profile" {
                assert_eq!(o.applies_to, vec!["bundle".to_string()], "{}", o.name);
            }
        }
        let device = s.options.iter().find(|o| o.name == "device").unwrap();
        assert!(device.applies_to.contains(&"catalog".to_string()));
    }

    #[test]
    fn schema_lists_every_option_and_every_allowlisted_vllm_flag() {
        let s = run_schema();
        let names: Vec<_> = s.options.iter().map(|o| o.name.as_str()).collect();
        for n in ["device", "port", "profile", "offline", "workflow"] {
            assert!(names.contains(&n), "schema missing {n}");
        }
        for (flag, _) in VLLM_ALLOWLIST {
            assert!(
                names.contains(&format!("vllm:{flag}").as_str()),
                "schema missing vllm:{flag}"
            );
        }
    }
}
