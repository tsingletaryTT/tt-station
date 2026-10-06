//! Types shared by every station, and the [`Station`] enum that dispatches to one.
//!
//! **Enum, not trait object.** The design spec described a `Station` trait. The tray has exactly
//! two kinds and no plug-in point, so an `enum` with plain `async fn`s does the same job with no
//! `async-trait` dependency and no boxing. The UI still never branches on kind: it calls the
//! methods here.

use libttstation::run_options::{RunOptions, RunSchema};
use serde::{Deserialize, Serialize};

use crate::local::LocalStation;
use crate::remote::RemoteStation;

/// Stable identity of a station: `"local"` for this machine, `"<host>:<port>"` for a box.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StationId(pub String);

/// Which kind of station this is, for the few places that must say so (an icon, a heading).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StationKind {
    /// This machine, driven through the official `tt` CLI.
    Local,
    /// A paired box. `base` is the agent's control-plane URL, e.g. `http://qb2-lab.local:8765`.
    Remote { base: String },
}

/// What a station is serving right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServingNow {
    /// The model id the OpenAI-compatible server reports.
    pub model: String,
    /// The `/v1` base URL clients should use.
    pub base_url: String,
}

/// A snapshot of one station, cheap enough to poll. Building it never fails: an unreachable
/// station is a value (`reachable == false`), because "the box is off" is a normal state for a
/// tray to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StationState {
    pub id: StationId,
    /// Display name ("This machine", or the box's name).
    pub name: String,
    /// Whether the station answered. For the local station this also requires the `tt` binary to
    /// be the official CLI.
    pub reachable: bool,
    /// Device mesh label such as `p300x2`, when known.
    pub mesh: Option<String>,
    /// The model being served, when one is.
    pub serving: Option<ServingNow>,
    /// Whether this app may run and stop models there. Always true for the local station. For a
    /// box it is true only when a pairing token is held.
    pub paired: bool,
    /// Text of the last refused run (another tenant holds the chips), kept until a run succeeds.
    /// Only remote stations set it.
    pub contention: Option<String>,
}

/// One row in the model browser, the same shape for both station kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRow {
    pub name: String,
    /// Model type (`llm`, `embedding`, ...) when the source says. Hub bundles often do not.
    pub kind: Option<String>,
    /// Where the model comes from (`tt-inference-server`, `HuggingFace`, `local`, or `box` for a
    /// remote station, whose agent does not report a source).
    pub source: String,
    /// Hardware labels the model lists support for.
    pub hardware: Vec<String>,
    /// Whether the weights are already on disk. Always a best effort.
    pub cached: bool,
    /// Whether the model lists this station's mesh (see [`fits`]).
    pub fits: bool,
}

/// Whether a model that lists `model_hw` can run on a station whose mesh is `mesh`.
///
/// True when `mesh` appears in the list (case-insensitive), or when the list names `p150` and
/// the mesh is built from Blackhole boards of either kind (`p150x*` or `p300x*`), because a
/// `p150` entry describes a single-chip configuration those meshes contain. False for an
/// unknown mesh. So `p300x2` fits `p300x2` and `p150`, and does not fit `t3k`.
///
/// This is a display hint for sorting the browser. `tt` and the agent make the real decision when
/// a model is started.
pub fn fits(model_hw: &[String], mesh: Option<&str>) -> bool {
    let Some(mesh) = mesh else { return false };
    let mesh = mesh.to_ascii_lowercase();
    let blackhole = mesh.starts_with("p150") || mesh.starts_with("p300");
    model_hw.iter().any(|hw| {
        let hw = hw.to_ascii_lowercase();
        hw == mesh || (hw == "p150" && blackhole)
    })
}

/// A place a model can run: this machine or a paired box.
pub enum Station {
    Local(LocalStation),
    Remote(RemoteStation),
}

impl Station {
    /// Snapshot of the station. Never fails; see [`StationState`].
    pub async fn state(&self) -> StationState {
        match self {
            Station::Local(s) => s.state().await,
            Station::Remote(s) => s.state().await,
        }
    }

    /// The models this station could serve, with `cached` and `fits` filled in.
    pub async fn models(&self) -> anyhow::Result<Vec<ModelRow>> {
        match self {
            Station::Local(s) => s.models().await,
            Station::Remote(s) => s.models().await,
        }
    }

    /// Start serving `model` with `options`. `force` is meaningful for a box (serve even though
    /// another tenant holds the chips) and ignored locally.
    pub async fn run(
        &self,
        model: &str,
        force: bool,
        options: &RunOptions,
    ) -> anyhow::Result<ServingNow> {
        match self {
            Station::Local(s) => s.run(model, force, options).await,
            Station::Remote(s) => s.run(model, force, options).await,
        }
    }

    /// Stop what the station is serving.
    pub async fn stop(&self) -> anyhow::Result<()> {
        match self {
            Station::Local(s) => s.stop().await,
            Station::Remote(s) => s.stop().await,
        }
    }

    /// The last `tail` lines of the serving log.
    pub async fn logs(&self, tail: usize) -> anyhow::Result<Vec<String>> {
        match self {
            Station::Local(s) => s.logs(tail).await,
            Station::Remote(s) => s.logs(tail).await,
        }
    }

    /// The run options this station accepts, for the run sheet.
    pub async fn schema(&self) -> anyhow::Result<RunSchema> {
        match self {
            Station::Local(s) => s.schema().await,
            Station::Remote(s) => s.schema().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fits_is_true_only_for_models_listing_this_stations_mesh() {
        assert!(fits(&hw(&["p300x2"]), Some("p300x2")));
        assert!(
            fits(&hw(&["p150"]), Some("p300x2")),
            "p150 entry fits a p300x2 mesh"
        );
        assert!(
            fits(&hw(&["t3k", "P300X2"]), Some("p300x2")),
            "case-insensitive"
        );
        assert!(!fits(&hw(&["t3k"]), Some("p300x2")));
        assert!(
            !fits(&hw(&["p150x4"]), Some("p300x2")),
            "other meshes are not implied"
        );
        assert!(
            !fits(&hw(&["p150"]), Some("t3k")),
            "p150 does not fit a Wormhole mesh"
        );
        assert!(!fits(&hw(&["p300x2"]), None), "unknown mesh fits nothing");
        assert!(!fits(&[], Some("p300x2")));
    }
}
