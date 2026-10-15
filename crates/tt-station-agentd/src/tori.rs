// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Tori's instant commands: everything TT Station and this computer can do,
//! said plainly -- "open Claude and VS Code", "start Qwen3-32B", "make an
//! image of a fox", "switch to video", "record this meeting", "how hot are
//! the chips", "volume to 30", "yes". See [`CAPABILITIES`]. Plain commands like these are understood here,
//! without a model, so they happen at once and every time -- a model can
//! answer "Done!" without doing anything, and a CPU model takes seconds.
//! Anything this doesn't fully understand goes to the model as usual.

use std::sync::LazyLock;

use regex::Regex;

use crate::desktop::{self, DesktopApp};

/// What Tori can do instantly, for "what can you do?" (and the docs).
pub const CAPABILITIES: &[(&str, &str)] = &[
    (
        "Open apps and sites",
        "“open Claude and VS Code”, “open YouTube”, “go to github.com”",
    ),
    ("Open folders", "“open my Downloads”, “open Documents”"),
    (
        "Run models",
        "“start Qwen3-32B”, “stop the model”, “reset the chips”",
    ),
    (
        "Switch what the box does",
        "“switch to images”, “video mode”, “chat and images”, “transcribe mode”",
    ),
    (
        "Make pictures",
        "“make an image of a fox in the snow”, “draw a teal robot”",
    ),
    ("Record meetings", "“record this meeting”, “stop recording”"),
    (
        "Check the box",
        "“what's the box doing?”, “how hot are the chips?”",
    ),
    (
        "Open TT Station",
        "“open Studio”, “open Models”, “open Bring-up”, “open hardware”",
    ),
    (
        "Sound and music",
        "“volume to 30”, “louder”, “mute”, “pause”, “next song”",
    ),
    (
        "The screen",
        "“lock the screen”, “dark mode”, “do not disturb”",
    ),
    (
        "Bring-up",
        "“how's the bring-up going?”, “allow it” / “deny it” when it's waiting on you",
    ),
    (
        "Confirm",
        "“yes” / “no” when I ask before changing the model",
    ),
];

/// One thing to do, as the matching assistant tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    OpenApp(String),
    OpenUrl(String),
    OpenFolder(String),
    Control(&'static str, Option<f64>),
    /// Any other assistant tool, e.g. `run_model`, `switch_mode`, `create_image`.
    Tool(&'static str, serde_json::Value),
    /// A TT Station page.
    Station(&'static str),
    /// Answer "yes" / "no" to the last thing Tori asked to confirm.
    Confirm(bool),
    /// "What can you do?"
    Help,
}

impl Step {
    /// The tool and its arguments.
    pub fn tool(&self) -> (&'static str, serde_json::Value) {
        use serde_json::json;
        match self {
            Step::OpenApp(app) => ("open_app", json!({ "app": app })),
            Step::OpenUrl(url) => ("open_app", json!({ "url": url })),
            Step::OpenFolder(path) => ("open_folder", json!({ "path": path })),
            Step::Control(action, value) => (
                "control_computer",
                json!({ "action": action, "value": value }),
            ),
            Step::Tool(name, args) => (name, args.clone()),
            Step::Station(page) => ("open_station", json!({ "page": page })),
            Step::Confirm(_) | Step::Help => ("", json!({})),
        }
    }
}

static POLITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:hey|ok|okay|hi)\s+)?(?:tori[,!.]?\s*)?(?:(?:please|can you|could you|would you|will you|i want you to|i'd like you to|go ahead and|just)\s+)*",
    )
    .expect("regex")
});
static TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\s+(?:please|for me|now|thanks|thank you))*[\s.!?]*$").expect("regex")
});
static OPEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:open|launch|start|run|bring up|pull up|fire up|show me|go to|take me to)(?:\s+up)?\s+(.+)$")
        .expect("regex")
});
static SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*,\s*(?:and\s+)?|\s+and\s+|\s*&\s*|\s+then\s+|\s+plus\s+").expect("regex")
});
static ITEM_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:up\s+|the\s+|my\s+|a\s+|an\s+|new\s+)+|\s+(?:app|application|window|program|for me)$",
    )
    .expect("regex")
});
static DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:https?://)?[a-z0-9-]+(?:\.[a-z0-9-]+)+(?:/\S*)?$").expect("regex")
});
static VOLUME_SET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:set|change|put|turn)?\s*(?:the\s+)?(?:volume|sound)\s+(?:to|at)?\s*(\d{1,3})\s*(?:%|percent)?$")
        .expect("regex")
});

const SITES: &[(&str, &str)] = &[
    ("youtube", "https://www.youtube.com"),
    ("gmail", "https://mail.google.com"),
    ("google", "https://www.google.com"),
    ("google calendar", "https://calendar.google.com"),
    ("calendar", "https://calendar.google.com"),
    ("google drive", "https://drive.google.com"),
    ("github", "https://github.com"),
    ("hugging face", "https://huggingface.co"),
    ("huggingface", "https://huggingface.co"),
    ("netflix", "https://www.netflix.com"),
    ("wikipedia", "https://www.wikipedia.org"),
    ("reddit", "https://www.reddit.com"),
    ("tenstorrent", "https://tenstorrent.com"),
];

/// Words that make "start X" about a model rather than an app.
const MODEL_WORDS: &[&str] = &[
    "qwen",
    "llama",
    "gemma",
    "mistral",
    "mixtral",
    "deepseek",
    "whisper",
    "flux",
    "wan",
    "mochi",
    "falcon",
    "phi",
    "gpt-oss",
    "gpt oss",
    "stable diffusion",
    "sdxl",
    "sd3",
    "qwq",
    "coder",
    "speecht5",
    "model",
];

fn model_like(s: &str) -> bool {
    MODEL_WORDS.iter().any(|w| s.contains(w))
        || (s.chars().any(|c| c.is_ascii_digit()) && s.contains('b'))
}

/// TT Station's pages, by what people call them.
const PAGES: &[(&str, &str)] = &[
    ("tt station", "home"),
    ("tt-station", "home"),
    ("station", "home"),
    ("chat", "chat"),
    ("studio", "studio"),
    ("transcripts", "transcribe"),
    ("transcribe", "transcribe"),
    ("models", "models"),
    ("model list", "models"),
    ("bring-up", "bringup"),
    ("bring up", "bringup"),
    ("bringup", "bringup"),
    ("orchard", "bringup"),
    ("hardware", "hardware"),
    ("chips", "hardware"),
    ("telemetry", "hardware"),
    ("connect", "connect"),
    ("tt station settings", "settings"),
];

const MODES: &[(&str, &str)] = &[
    ("chat and images", "chat_images"),
    ("chat and image", "chat_images"),
    ("chat and pictures", "chat_images"),
    ("chat + images", "chat_images"),
    ("images", "images"),
    ("image", "images"),
    ("pictures", "images"),
    ("picture", "images"),
    ("video", "video"),
    ("videos", "video"),
    ("transcription", "transcribe"),
    ("transcribe", "transcribe"),
    ("transcribing", "transcribe"),
    ("speech", "transcribe"),
    ("chat", "chat"),
];

static SWITCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:switch|change|set|set up|put|move|get)(?: the box| my box| it)?(?: up)? (?:to|for|into|over to) (.+?)(?: mode)?$|^(.+?) mode$")
        .expect("regex")
});
static IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:make|create|draw|generate|paint|render|imagine|show me|design|sketch)(?: me| us)? (?:(?:a|an|some|another|one more)\s+)?(?:(?:image|picture|photo|pic|drawing|painting|illustration|wallpaper|render)s?(?: of| showing| with| for)?\s+)?(.+)$")
        .expect("regex")
});
static RUN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:start|run|serve|load|bring up|spin up|launch|switch to|use|fire up)(?: up)?(?: the)?(?: model)? (.+?)(?: model)?$").expect("regex")
});

fn page(item: &str) -> Option<&'static str> {
    let item = item
        .trim_end_matches(" page")
        .trim_end_matches(" tab")
        .trim();
    PAGES
        .iter()
        .find(|(name, _)| *name == item)
        .map(|(_, p)| *p)
}

/// Box things: models, modes, pictures, recording, status, help, yes/no.
fn box_command(s: &str, apps: &[DesktopApp]) -> Option<Vec<Step>> {
    use serde_json::json;
    let one = |step| Some(vec![step]);
    match s {
        "yes" | "yeah" | "yep" | "yes please" | "do it" | "go ahead" | "confirm" | "sure"
        | "ok" | "okay" | "sounds good" | "go for it" => return one(Step::Confirm(true)),
        "no" | "nope" | "cancel" | "never mind" | "nevermind" | "don't" | "no thanks"
        | "stop that" => return one(Step::Confirm(false)),
        "what can you do"
        | "help"
        | "what do you do"
        | "what can i ask you"
        | "what can i say"
        | "commands"
        | "what are your commands" => return one(Step::Help),
        "stop the model"
        | "stop serving"
        | "unload the model"
        | "shut down the model"
        | "stop the chips"
        | "turn off the model"
        | "free the chips"
        | "stop everything" => return one(Step::Tool("stop_model", json!({}))),
        "reset the chips" | "reset chips" | "reset the board" | "reset the boards"
        | "reset the cards" | "reset the hardware" => {
            return one(Step::Tool("reset_chips", json!({})))
        }
        "record"
        | "record this"
        | "record this meeting"
        | "record the meeting"
        | "record a meeting"
        | "start recording"
        | "record this call"
        | "record the call"
        | "start a recording" => return one(Step::Tool("start_recording", json!({}))),
        "stop recording"
        | "stop the recording"
        | "end the recording"
        | "finish recording"
        | "stop recording and transcribe"
        | "transcribe the meeting" => return one(Step::Tool("stop_recording", json!({}))),
        "how's the bring-up going"
        | "how is the bring-up going"
        | "how's the bring-up"
        | "bring-up status"
        | "bringup status"
        | "how's the bringup going"
        | "how's the port going"
        | "what's the bring-up doing"
        | "where's the bring-up"
        | "is the bring-up done" => return one(Step::Tool("bringup_status", json!({}))),
        "allow it"
        | "approve it"
        | "allow that"
        | "approve that"
        | "let it"
        | "let it run"
        | "go ahead and allow it" => {
            return one(Step::Tool(
                "bringup_decide",
                json!({ "decision": "accept" }),
            ))
        }
        "deny it" | "deny that" | "don't allow it" | "don't let it" | "block it" | "reject it" => {
            return one(Step::Tool(
                "bringup_decide",
                json!({ "decision": "decline" }),
            ))
        }
        "status"
        | "box status"
        | "what's the box doing"
        | "what is the box doing"
        | "how's the box"
        | "how is the box"
        | "how's my box"
        | "what's running"
        | "what is running"
        | "what model is running"
        | "what's serving"
        | "what is serving"
        | "how hot are the chips"
        | "how hot is the box"
        | "temperature"
        | "temperatures"
        | "chip temperatures"
        | "how are the chips"
        | "how much power"
        | "power" => return one(Step::Tool("box_status", json!({}))),
        _ => {}
    }
    if let Some(m) = SWITCH.captures(s) {
        let what = m
            .get(1)
            .or(m.get(2))
            .map(|x| x.as_str().trim())
            .unwrap_or("");
        let what = what.trim_start_matches("the ").trim_end_matches(" mode");
        if let Some((_, mode)) = MODES.iter().find(|(name, _)| *name == what) {
            return one(Step::Tool("switch_mode", json!({ "mode": mode })));
        }
    }
    if let Some(m) = IMAGE.captures(s) {
        let prompt = m[1].trim();
        let pictured = [
            "image",
            "picture",
            "photo",
            "drawing",
            "painting",
            "illustration",
            "wallpaper",
        ]
        .iter()
        .any(|w| s.contains(w))
            || s.starts_with("draw")
            || s.starts_with("paint")
            || s.starts_with("sketch");
        if pictured && prompt.split_whitespace().count() >= 2 {
            return Some(vec![
                Step::Tool("create_image", json!({ "prompt": prompt })),
                Step::Station("studio"),
            ]);
        }
    }
    if let Some(m) = RUN.captures(s) {
        let name = m[1].trim();
        // An installed app with that name wins ("start Steam").
        let app = desktop::find(apps, name)
            .0
            .filter(|a| desktop::score(a, name) >= 60);
        if app.is_none() && model_like(name) && !name.contains(" and ") {
            return one(Step::Tool("run_model", json!({ "model": name })));
        }
    }
    None
}

const FOLDERS: &[&str] = &[
    "downloads",
    "documents",
    "desktop",
    "pictures",
    "photos",
    "music",
    "videos",
    "home",
];

/// Lowercase, without "hey Tori, can you …please".
pub fn normalize(said: &str) -> String {
    let s = said
        .trim()
        .to_lowercase()
        .replace(['\u{2019}', '\u{2018}'], "'");
    let s = POLITE.replace(&s, "");
    TAIL.replace(&s, "").trim().to_string()
}

fn resolve(item: &str, apps: &[DesktopApp]) -> Option<Step> {
    let item = ITEM_NOISE.replace_all(item.trim(), "").trim().to_string();
    if item.is_empty() {
        return None;
    }
    let folder = item.trim_end_matches(" folder").trim();
    if FOLDERS.contains(&folder) {
        return Some(Step::OpenFolder(folder.to_string()));
    }
    // An installed app wins over a website of the same name (Spotify, Slack…).
    let (best, _) = desktop::find(apps, &item);
    if let Some(app) = best.filter(|a| desktop::score(a, &item) >= 60) {
        return Some(Step::OpenApp(app.name.clone()));
    }
    if let Some(p) = page(&item) {
        return Some(Step::Station(p));
    }
    if let Some((_, url)) = SITES.iter().find(|(name, _)| *name == item) {
        return Some(Step::OpenUrl(url.to_string()));
    }
    if DOMAIN.is_match(&item) {
        let url = if item.starts_with("http") {
            item.clone()
        } else {
            format!("https://{item}")
        };
        return Some(Step::OpenUrl(url));
    }
    None
}

fn control(s: &str) -> Option<Step> {
    let c = |a: &'static str| Some(Step::Control(a, None));
    match s {
        "volume up"
        | "louder"
        | "turn it up"
        | "turn up the volume"
        | "turn the volume up"
        | "turn volume up"
        | "raise the volume"
        | "increase the volume"
        | "crank it up" => return c("volume_up"),
        "volume down"
        | "quieter"
        | "softer"
        | "turn it down"
        | "turn down the volume"
        | "turn the volume down"
        | "turn volume down"
        | "lower the volume"
        | "decrease the volume" => return c("volume_down"),
        "mute" | "mute it" | "mute the sound" | "mute the audio" | "mute the volume"
        | "mute sound" => return c("mute"),
        "unmute" | "unmute it" | "unmute the sound" | "unmute the audio" => return c("unmute"),
        "pause" | "pause it" | "pause the music" | "pause the song" | "pause the video"
        | "pause music" | "pause playback" | "stop the music" | "stop music" => {
            return c("pause_media")
        }
        "play" | "resume" | "play it" | "resume playback" | "play the music" | "play music"
        | "resume the music" | "resume the video" | "unpause" => return c("play_media"),
        "next" | "next song" | "next track" | "skip" | "skip it" | "skip this song"
        | "skip this" | "skip song" | "skip track" | "next video" => return c("next_track"),
        "previous" | "previous song" | "previous track" | "go back a song" | "last song"
        | "back a song" => return c("previous_track"),
        "lock" | "lock the screen" | "lock screen" | "lock my screen" | "lock the computer"
        | "lock my computer" => return c("lock_screen"),
        "dark mode"
        | "turn on dark mode"
        | "switch to dark mode"
        | "enable dark mode"
        | "go dark" => return c("dark_mode"),
        "light mode"
        | "turn on light mode"
        | "switch to light mode"
        | "enable light mode"
        | "turn off dark mode" => return c("light_mode"),
        "do not disturb"
        | "turn on do not disturb"
        | "enable do not disturb"
        | "don't disturb me" => return c("do_not_disturb_on"),
        "turn off do not disturb" | "disable do not disturb" | "do not disturb off" => {
            return c("do_not_disturb_off")
        }
        _ => {}
    }
    VOLUME_SET
        .captures(s)
        .and_then(|m| m[1].parse::<f64>().ok())
        .map(|v| Step::Control("volume_set", Some(v)))
}

/// The steps for a plain command, or `None` to ask the model.
pub fn parse(said: &str, apps: &[DesktopApp]) -> Option<Vec<Step>> {
    let s = normalize(said);
    if s.is_empty() || s.len() > 200 {
        return None;
    }
    let s = s.trim_end_matches('?').trim().to_string();
    if let Some(step) = control(&s) {
        return Some(vec![step]);
    }
    if let Some(steps) = box_command(&s, apps) {
        return Some(steps);
    }
    let what = OPEN.captures(&s)?.get(1)?.as_str().to_string();
    let steps: Option<Vec<Step>> = SPLIT
        .split(&what)
        .filter(|i| !i.trim().is_empty())
        .map(|i| resolve(i, apps))
        .collect();
    steps.filter(|s| !s.is_empty() && s.len() <= 6)
}

fn list(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
    }
}

/// What Tori says about a box step (None: nothing special to say).
fn box_reply(tool: &str, r: &serde_json::Value) -> Option<String> {
    if r["status"] == "needs_confirmation" {
        return Some(format!(
            "{} Say yes, or tap Do it.",
            r["summary"].as_str().unwrap_or("Shall I?")
        ));
    }
    Some(match tool {
        "box_status" => status_line(r),
        "bringup_status" => r["status"].as_str().unwrap_or("").to_string(),
        "bringup_decide" => {
            let what = r["what"].as_str().unwrap_or("it");
            if r["decided"] == "accept" {
                format!("Allowed. It's running `{what}` now.")
            } else {
                format!("Denied `{what}`.")
            }
        }
        "create_image" => "Here it is. It's in Studio too.".into(),
        "start_recording" => "Recording. Let everyone know.".into(),
        "stop_recording" => "Stopped. I'll have the transcript in a few minutes.".into(),
        "studio" => String::new(),
        "home" => "Opening TT Station.".into(),
        "bringup" => "Opening Bring-up.".into(),
        "transcribe" => "Opening your transcripts.".into(),
        "chat" | "models" | "hardware" | "connect" | "settings" => {
            format!("Opening {}.", capitalize(tool))
        }
        _ => return None,
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// "Serving Qwen3-32B. The 4 chips are at 49–55 °C, drawing 214 W."
pub fn status_line(r: &serde_json::Value) -> String {
    let serving = r["serving"]["model"]
        .as_str()
        .map(|m| m.rsplit('/').next().unwrap_or(m).to_string());
    let mut line = match serving {
        Some(m) => format!("Serving {m}."),
        None => "Nothing is serving right now.".to_string(),
    };
    if let Some(devices) = r["devices"].as_array() {
        let temps: Vec<f64> = devices
            .iter()
            .filter_map(|d| d["temp_c"].as_f64())
            .collect();
        let watts: f64 = devices.iter().filter_map(|d| d["power_w"].as_f64()).sum();
        if !temps.is_empty() {
            let lo = temps.iter().cloned().fold(f64::MAX, f64::min).round();
            let hi = temps.iter().cloned().fold(f64::MIN, f64::max).round();
            let range = if lo == hi {
                format!("{lo} °C")
            } else {
                format!("{lo}–{hi} °C")
            };
            line.push_str(&format!(
                " The {} chips are at {range}, drawing {} W.",
                temps.len(),
                watts.round()
            ));
        }
    }
    line
}

/// What Tori says after doing `steps` (each with its outcome).
pub fn reply(done: &[(Step, Result<serde_json::Value, String>)]) -> String {
    let mut opened = Vec::new();
    let mut said = Vec::new();
    for (step, outcome) in done {
        if let Step::Help = step {
            let lines: Vec<String> = CAPABILITIES
                .iter()
                .map(|(what, eg)| format!("• {what}: {eg}"))
                .collect();
            said.push(format!(
                "Here's what I can do right away:\n{}\nAnything else, just ask.",
                lines.join("\n")
            ));
            continue;
        }
        if let (Step::Tool(tool, _) | Step::Station(tool), Ok(r)) = (step, outcome) {
            if let Some(line) = box_reply(tool, r) {
                if !line.is_empty() {
                    said.push(line);
                }
                continue;
            }
        }
        if let (Step::Confirm(_), Ok(r)) = (step, outcome) {
            said.push(r["summary"].as_str().unwrap_or("Done.").to_string());
            continue;
        }
        match outcome {
            Err(e) => said.push(format!("I couldn't do that: {e}.")),
            Ok(r) if r["ambiguous"] == true => {
                let options: Vec<String> = r["options"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                said.push(format!(
                    "Which one: {}?",
                    list(&options).replace(" and ", " or ")
                ));
            }
            Ok(r) => match step {
                Step::OpenApp(_) | Step::OpenUrl(_) => {
                    opened.push(r["opened"].as_str().unwrap_or("it").to_string())
                }
                Step::OpenFolder(_) => opened.push(
                    r["opened"]
                        .as_str()
                        .and_then(|p| p.rsplit('/').next())
                        .map(|n| format!("your {n} folder"))
                        .unwrap_or_else(|| "the folder".into()),
                ),
                Step::Control(action, _) => said.push(match *action {
                    "volume_set" | "volume_up" | "volume_down" => r["volume_percent"]
                        .as_u64()
                        .map(|v| format!("Volume's at {v}%."))
                        .unwrap_or_else(|| "Done.".into()),
                    "mute" => "Muted.".into(),
                    "unmute" => "Sound's back on.".into(),
                    "pause_media" => "Paused.".into(),
                    "play_media" => "Playing.".into(),
                    "next_track" => "Skipped.".into(),
                    "previous_track" => "Going back.".into(),
                    "lock_screen" => "Locking the screen.".into(),
                    "dark_mode" => "Dark mode's on.".into(),
                    "light_mode" => "Light mode's on.".into(),
                    "do_not_disturb_on" => "Do not disturb is on.".into(),
                    "do_not_disturb_off" => "Notifications are back on.".into(),
                    _ => "Done.".into(),
                }),
                _ => said.push("Done.".into()),
            },
        }
    }
    let mut out = String::new();
    if !opened.is_empty() {
        out = format!("Opening {}.", list(&opened));
    }
    for s in said {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps() -> Vec<DesktopApp> {
        [
            "Claude",
            "Visual Studio Code",
            "Google Chrome",
            "Files",
            "Terminal",
            "Spotify",
            "Steam",
        ]
        .iter()
        .map(|n| DesktopApp {
            id: n.to_lowercase().replace(' ', "-"),
            name: n.to_string(),
            generic: None,
            keywords: vec![],
        })
        .collect()
    }

    #[test]
    fn several_apps_at_once() {
        assert_eq!(
            parse("Open up Claude and VSCode", &apps()),
            Some(vec![
                Step::OpenApp("Claude".into()),
                Step::OpenApp("Visual Studio Code".into())
            ])
        );
        assert_eq!(
            parse(
                "hey Tori, can you please launch chrome, terminal and files for me",
                &apps()
            ),
            Some(vec![
                Step::OpenApp("Google Chrome".into()),
                Step::OpenApp("Terminal".into()),
                Step::OpenApp("Files".into())
            ])
        );
    }

    #[test]
    fn sites_folders_and_apps() {
        assert_eq!(
            parse("open youtube", &apps()),
            Some(vec![Step::OpenUrl("https://www.youtube.com".into())])
        );
        assert_eq!(
            parse("go to tenstorrent.com", &apps()),
            Some(vec![Step::OpenUrl("https://tenstorrent.com".into())])
        );
        assert_eq!(
            parse("open my downloads folder", &apps()),
            Some(vec![Step::OpenFolder("downloads".into())])
        );
        assert_eq!(
            parse("open spotify", &apps()),
            Some(vec![Step::OpenApp("Spotify".into())])
        );
    }

    #[test]
    fn controls() {
        assert_eq!(
            parse("Turn the volume up.", &apps()),
            Some(vec![Step::Control("volume_up", None)])
        );
        assert_eq!(
            parse("set the volume to 30%", &apps()),
            Some(vec![Step::Control("volume_set", Some(30.0))])
        );
        assert_eq!(
            parse("volume 45", &apps()),
            Some(vec![Step::Control("volume_set", Some(45.0))])
        );
        assert_eq!(
            parse("Pause the music", &apps()),
            Some(vec![Step::Control("pause_media", None)])
        );
        assert_eq!(
            parse("skip this song", &apps()),
            Some(vec![Step::Control("next_track", None)])
        );
        assert_eq!(
            parse("lock my screen please", &apps()),
            Some(vec![Step::Control("lock_screen", None)])
        );
    }

    #[test]
    fn box_commands() {
        use serde_json::json;
        let t = |s: &str| parse(s, &apps());
        assert_eq!(
            t("start Qwen3-32B"),
            Some(vec![Step::Tool(
                "run_model",
                json!({ "model": "qwen3-32b" })
            )])
        );
        assert_eq!(
            t("bring up llama 3.1 8b please"),
            Some(vec![Step::Tool(
                "run_model",
                json!({ "model": "llama 3.1 8b" })
            )])
        );
        assert_eq!(
            t("switch to images"),
            Some(vec![Step::Tool("switch_mode", json!({ "mode": "images" }))])
        );
        assert_eq!(
            t("video mode"),
            Some(vec![Step::Tool("switch_mode", json!({ "mode": "video" }))])
        );
        assert_eq!(
            t("set the box up for chat and images"),
            Some(vec![Step::Tool(
                "switch_mode",
                json!({ "mode": "chat_images" })
            )])
        );
        assert_eq!(
            t("make an image of a fox in the snow"),
            Some(vec![
                Step::Tool("create_image", json!({ "prompt": "a fox in the snow" })),
                Step::Station("studio")
            ])
        );
        assert_eq!(
            t("draw me a teal robot reading a book"),
            Some(vec![
                Step::Tool(
                    "create_image",
                    json!({ "prompt": "teal robot reading a book" })
                ),
                Step::Station("studio")
            ])
        );
        assert_eq!(
            t("what's the box doing?"),
            Some(vec![Step::Tool("box_status", json!({}))])
        );
        assert_eq!(
            t("How hot are the chips?"),
            Some(vec![Step::Tool("box_status", json!({}))])
        );
        assert_eq!(
            t("record this meeting"),
            Some(vec![Step::Tool("start_recording", json!({}))])
        );
        assert_eq!(
            t("stop recording"),
            Some(vec![Step::Tool("stop_recording", json!({}))])
        );
        assert_eq!(
            t("stop the model"),
            Some(vec![Step::Tool("stop_model", json!({}))])
        );
        assert_eq!(t("yes"), Some(vec![Step::Confirm(true)]));
        assert_eq!(t("never mind"), Some(vec![Step::Confirm(false)]));
        assert_eq!(t("what can you do?"), Some(vec![Step::Help]));
        assert_eq!(
            t("how's the bring-up going?"),
            Some(vec![Step::Tool("bringup_status", json!({}))])
        );
        assert_eq!(
            t("allow it"),
            Some(vec![Step::Tool(
                "bringup_decide",
                json!({ "decision": "accept" })
            )])
        );
        assert_eq!(t("open models"), Some(vec![Step::Station("models")]));
        assert_eq!(t("open bring-up"), Some(vec![Step::Station("bringup")]));
        assert_eq!(t("start steam"), Some(vec![Step::OpenApp("Steam".into())]));
    }

    #[test]
    fn status_reads_well() {
        let r = serde_json::json!({ "serving": { "model": "Qwen/Qwen3-32B" }, "devices": [
            { "temp_c": 49.6, "power_w": 51.0 }, { "temp_c": 54.6, "power_w": 57.0 } ] });
        assert_eq!(
            status_line(&r),
            "Serving Qwen3-32B. The 2 chips are at 50–55 °C, drawing 108 W."
        );
    }

    #[test]
    fn everything_else_goes_to_the_model() {
        for said in [
            "open the file I was working on yesterday",
            "open photoshop",
            "why is the sky blue?",
            "start a timer",
            "open a new document about cats",
        ] {
            assert_eq!(parse(said, &apps()), None, "{said}");
        }
    }

    #[test]
    fn replies_sound_natural() {
        let ok = |name: &str| Ok(serde_json::json!({ "opened": name }));
        let done = vec![
            (Step::OpenApp("Claude".into()), ok("Claude")),
            (
                Step::OpenApp("Visual Studio Code".into()),
                ok("Visual Studio Code"),
            ),
        ];
        assert_eq!(reply(&done), "Opening Claude and Visual Studio Code.");
    }
}
