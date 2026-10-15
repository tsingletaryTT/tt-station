// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Short sounds the box plays so the people in the room know its microphone
//! is in use: a rising two-note chime when a meeting recording starts, the
//! same notes falling when it stops, and a single soft tick when it listens
//! for one spoken request (`POST /voice/hear`).
//!
//! The tones are made here ([`samples`]) rather than shipped as files, and
//! played like the box's speech (`crate::voice`): a private temporary WAV
//! through `pw-play`, on the box's default output.

use std::process::{Command, Stdio};

const RATE: f32 = 16_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    /// A meeting recording started: two notes, rising.
    RecordingStarted,
    /// It stopped (or ended on its own): the same two notes, falling.
    RecordingStopped,
    /// The microphone opens for one spoken request: one short, quiet tick.
    Listening,
}

/// One note: `hz` for `ms`, with a quick attack and a soft decay so it rings
/// rather than clicks.
fn note(hz: f32, ms: u32, gain: f32, out: &mut Vec<i16>) {
    let n = (RATE * ms as f32 / 1000.0) as usize;
    let attack = (RATE * 0.008) as usize;
    for i in 0..n {
        let t = i as f32 / RATE;
        let env = if i < attack {
            i as f32 / attack as f32
        } else {
            (-((i - attack) as f32) / (n as f32 * 0.35)).exp()
        };
        // A touch of the octave makes it a chime rather than a beep.
        let s = (2.0 * std::f32::consts::PI * hz * t).sin() * 0.8
            + (4.0 * std::f32::consts::PI * hz * t).sin() * 0.2;
        out.push((s * env * gain * i16::MAX as f32) as i16);
    }
}

/// The chime as 16 kHz mono samples.
pub fn samples(chime: Chime) -> Vec<i16> {
    let mut out = Vec::new();
    let gap = |out: &mut Vec<i16>| out.extend(std::iter::repeat_n(0, (RATE * 0.03) as usize));
    match chime {
        Chime::RecordingStarted => {
            note(659.25, 170, 0.45, &mut out);
            gap(&mut out);
            note(880.0, 260, 0.45, &mut out);
        }
        Chime::RecordingStopped => {
            note(880.0, 170, 0.45, &mut out);
            gap(&mut out);
            note(659.25, 260, 0.45, &mut out);
        }
        Chime::Listening => note(1046.5, 90, 0.2, &mut out),
    }
    out
}

/// Play `chime` on the box's speakers and wait until it's done (about half a
/// second at most). A box with no speakers or no PipeWire just stays quiet.
pub fn play_blocking(chime: Chime) -> Result<(), String> {
    use std::io::Write;
    let wav = crate::voice::wav(&samples(chime));
    let (mut f, path) =
        crate::fsio::private_temp_file("tt-chime-", ".wav").map_err(|e| e.to_string())?;
    f.write_all(&wav).map_err(|e| e.to_string())?;
    drop(f);
    let played = Command::new("pw-play")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&path);
    match played {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("pw-play exited with {s}")),
        Err(e) => Err(format!("couldn't play the chime: {e}")),
    }
}

/// [`play_blocking`] in the background. (Silent in unit tests, which run on
/// boxes with real speakers.)
pub fn play(chime: Chime) {
    if cfg!(test) {
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = play_blocking(chime) {
            eprintln!("tt-station-agentd: {chime:?} chime: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pitch(s: &[i16]) -> f32 {
        // Zero crossings per second / 2, over the loud part.
        let crossings = s.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        crossings as f32 / (s.len() as f32 / RATE) / 2.0
    }

    #[test]
    fn chimes_are_short_soft_and_start_and_end_quietly() {
        for c in [
            Chime::RecordingStarted,
            Chime::RecordingStopped,
            Chime::Listening,
        ] {
            let s = samples(c);
            let secs = s.len() as f32 / RATE;
            assert!(secs > 0.05 && secs < 0.6, "{c:?}: {secs}s");
            let peak = s.iter().map(|x| x.unsigned_abs()).max().unwrap();
            assert!(peak > 3000 && peak < 20_000, "{c:?}: peak {peak}");
            assert!(s[0].unsigned_abs() < 200, "no click at the start");
            assert!(s[s.len() - 1].unsigned_abs() < 2000, "rings out");
        }
        let listen = samples(Chime::Listening);
        let start = samples(Chime::RecordingStarted);
        assert!(
            listen.len() * 3 < start.len(),
            "the listening tick is subtler"
        );
    }

    #[test]
    fn start_rises_and_stop_falls() {
        let first = (RATE * 0.15) as usize;
        let start = samples(Chime::RecordingStarted);
        let stop = samples(Chime::RecordingStopped);
        let tail = |s: &[i16]| s[s.len() - first..].to_vec();
        assert!(pitch(&start[..first]) < pitch(&tail(&start)));
        assert!(pitch(&stop[..first]) > pitch(&tail(&stop)));
        // A recording's end isn't confused with its start.
        assert_ne!(start, stop);
    }

    #[test]
    fn the_wav_is_mono_16k_pcm() {
        let w = crate::voice::wav(&samples(Chime::Listening));
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 16_000);
    }
}
