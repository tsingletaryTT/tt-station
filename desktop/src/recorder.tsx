// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Meeting recorder: the box records its microphone and whatever it's playing
// (the meeting), then transcribes it like any other recording. From another
// computer (a Mac), that computer records its own microphone and uploads the
// recording to the device to transcribe.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { Icon } from "@tenstorrent/vesper/icons";
import { currentToken, get, onAnotherComputer, post } from "./api";
import { navigate } from "./App";
import { Card, Eyebrow, toastError, toastOk } from "./ui";
import { ask } from "./dialog";

export interface Recording {
  id: string; title: string; started: number; seconds: number; max_seconds: number; ended: boolean;
  mic: string | null; system: string | null; mic_db: number | null; system_db: number | null;
}
export interface RecordState { microphone: string | null; speakers: string | null; recording: Recording | null }

// ---------------------------------------------------------------- recording on this computer

const MAX_SECONDS = 4 * 3600;

/** This computer's microphone, recorded here, then sent to the device to transcribe. */
class LocalRecorder {
  state: RecordState = { microphone: "This computer's microphone", speakers: null, recording: null };
  private listeners = new Set<() => void>();
  private rec: MediaRecorder | null = null;
  private stream: MediaStream | null = null;
  private chunks: Blob[] = [];
  private timer = 0;
  private audio: AudioContext | null = null;

  subscribe(fn: () => void) { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; }
  private emit() { this.state = { ...this.state }; for (const fn of this.listeners) fn(); }

  async start(title?: string) {
    if (this.rec) throw new Error("Already recording.");
    // Unprocessed: echo cancellation would remove the meeting coming out of the speakers.
    this.stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: true } });
    const mime = MediaRecorder.isTypeSupported("audio/webm;codecs=opus") ? "audio/webm;codecs=opus" : "audio/webm";
    this.chunks = [];
    this.rec = new MediaRecorder(this.stream, { mimeType: mime });
    this.rec.ondataavailable = (e) => { if (e.data.size) this.chunks.push(e.data); };
    this.rec.start(1000);
    this.audio = new AudioContext();
    const analyser = this.audio.createAnalyser();
    analyser.fftSize = 2048;
    this.audio.createMediaStreamSource(this.stream).connect(analyser);
    const buf = new Float32Array(analyser.fftSize);
    const label = this.stream.getAudioTracks()[0]?.label || "Microphone";
    const started = Date.now() / 1000;
    const name = title?.trim() || `Meeting, ${new Date().toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}`;
    this.state.microphone = label;
    this.state.recording = { id: "local", title: name, started, seconds: 0, max_seconds: MAX_SECONDS, ended: false, mic: label, system: null, mic_db: null, system_db: null };
    this.timer = window.setInterval(() => {
      analyser.getFloatTimeDomainData(buf);
      const rms = Math.sqrt(buf.reduce((a, x) => a + x * x, 0) / buf.length);
      const r = this.state.recording;
      if (!r) return;
      r.mic_db = rms > 0 ? 20 * Math.log10(rms) : -90;
      r.seconds = Math.floor(Date.now() / 1000 - started);
      if (r.seconds >= MAX_SECONDS && !r.ended) { r.ended = true; this.rec?.stop(); }
      this.emit();
    }, 200);
    this.emit();
  }

  async stop(discard = false): Promise<string> {
    const rec = this.rec;
    const r = this.state.recording;
    if (!rec || !r) throw new Error("Nothing is recording.");
    if (rec.state !== "inactive") await new Promise<void>((done) => { rec.onstop = () => done(); rec.stop(); });
    window.clearInterval(this.timer);
    this.stream?.getTracks().forEach((t) => t.stop());
    void this.audio?.close();
    this.rec = null; this.stream = null; this.audio = null;
    const blob = new Blob(this.chunks, { type: rec.mimeType });
    this.chunks = [];
    this.state.recording = null;
    this.emit();
    if (discard) return "Recording thrown away.";
    const name = `${r.title.replace(/[\\/]/g, "-")}.webm`;
    const res = await fetch(`/jobs/transcribe?name=${encodeURIComponent(name)}&when=now`, {
      method: "POST", body: blob, headers: { authorization: `Bearer ${currentToken() ?? ""}`, "content-type": "application/octet-stream" },
    });
    if (!res.ok) throw new Error(`The device didn't take the recording (${res.status}).`);
    return "Saved. The transcript will be ready in a few minutes; you'll get a notification.";
  }
}

export const localRecorder = new LocalRecorder();

let elsewhere: boolean | null = null;
const elsewhereOnce = onAnotherComputer().then((v) => { elsewhere = v; return v; }).catch(() => false);

// The menu bar and Tori (another window) ask this one to record.
window.ttDesktop?.onClientRecord?.(async ({ nonce, action, title }) => {
  try {
    if (action === "start") { await localRecorder.start(title); window.ttDesktop?.clientRecordDone?.(nonce, { ok: true, summary: "Started recording on this computer" }); }
    else window.ttDesktop?.clientRecordDone?.(nonce, { ok: true, summary: (await localRecorder.stop()).split(".")[0] });
  } catch (e) {
    window.ttDesktop?.clientRecordDone?.(nonce, { ok: false, error: (e as Error).message });
  }
});

/** Start recording a meeting: on the device, or on this computer when it isn't the device. */
export async function startRecording(title?: string) {
  if (await elsewhereOnce) await localRecorder.start(title);
  else await post("/record/start", { title: title || undefined });
}

/** Stop; queues the transcription. Says what happened. */
export async function stopRecording(discard = false): Promise<string> {
  if (await elsewhereOnce) return localRecorder.stop(discard);
  await post("/record/stop", { discard });
  return discard ? "Recording thrown away." : "Saved. The transcript will be ready in a few minutes; you'll get a notification.";
}

/** Polls fast while recording (for the meters), slowly otherwise. On another
 *  computer, follows this computer's own recorder. */
export function useRecorder() {
  const [state, setState] = useState<RecordState | null>(null);
  const [local, setLocal] = useState<boolean | null>(elsewhere);
  const [tick, setTick] = useState(0);
  const live = !!state?.recording;
  useEffect(() => { void elsewhereOnce.then(setLocal); }, []);
  useEffect(() => {
    if (local !== true) return;
    setState(localRecorder.state);
    return localRecorder.subscribe(() => setState(localRecorder.state));
  }, [local]);
  useEffect(() => {
    if (local !== false) return;
    let alive = true;
    const load = () => get<RecordState>("/record").then((s) => alive && setState(s)).catch(() => {});
    load();
    const t = setInterval(load, live ? 200 : 4000);
    return () => { alive = false; clearInterval(t); };
  }, [live, tick, local]);
  return { state, local: local === true, refresh: () => setTick((n) => n + 1) };
}

export function clock(s: number) {
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return `${h ? `${h}:` : ""}${String(m).padStart(h ? 2 : 1, "0")}:${String(sec).padStart(2, "0")}`;
}

function Meter({ label, device, db }: { label: string; device: string | null; db: number | null }) {
  // -60 dB (room tone) to 0 dB (clipping) across the bar.
  const pct = db == null ? 0 : Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
  return (
    <div className="stack" style={{ gap: 4, minWidth: 0 }}>
      <div className="row-between" style={{ gap: 8 }}>
        <span style={{ fontSize: 13, whiteSpace: "nowrap", flexShrink: 0 }}>{label}</span>
        <span className="muted" style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{device ?? "Off"}</span>
      </div>
      <div className="meter"><i style={{ width: `${pct}%`, opacity: device ? 1 : 0.2 }} /></div>
    </div>
  );
}

export function MeetingRecorder({ onQueued }: { onQueued?: () => void }) {
  const { state, local, refresh } = useRecorder();
  const [title, setTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const rec = state?.recording;

  const start = async () => {
    setBusy(true);
    try { await startRecording(title.trim() || undefined); setTitle(""); refresh(); }
    catch (e) { toastError(e); } finally { setBusy(false); }
  };
  const stop = async (discard = false) => {
    setBusy(true);
    try {
      toastOk(await stopRecording(discard));
      refresh(); onQueued?.();
    } catch (e) { toastError(e); } finally { setBusy(false); }
  };

  return (
    <Card tone={rec ? "accent" : undefined}>
      <div className="row-between">
        <Eyebrow>Record a meeting</Eyebrow>
        {rec && <span className="rec-dot mono">{clock(rec.seconds)}</span>}
      </div>
      {rec ? (
        <>
          <Typography variant="heading-xs">{rec.title}</Typography>
          <div className="grid grid-2" style={{ gap: 16 }}>
            <Meter label="You" device={rec.mic} db={rec.mic_db} />
            <Meter label="The meeting" device={rec.system} db={rec.system_db} />
          </div>
          {rec.ended && <Typography variant="copy-sm" style={{ color: "var(--vesper-text-error)" }}>The recording stopped on its own (a device was unplugged, or it hit {clock(rec.max_seconds)}). Save what you have.</Typography>}
          <div className="row">
            <Button onClick={() => stop()} disabled={busy}>Stop and transcribe</Button>
            <Button variant="ghost" disabled={busy} onClick={async () => { if (await ask("Throw this recording away?", { body: "Nothing is kept or transcribed.", confirm: "Discard", danger: true })) stop(true); }}>Discard</Button>
          </div>
        </>
      ) : (
        <>
          {local ? (
            <Typography variant="copy-sm" className="muted">
              Records this computer's microphone, then the device writes a transcript you can search and ask about. On a call through the speakers it hears both sides; with headphones on, only you (macOS doesn't let apps record other apps' sound).
            </Typography>
          ) : (
            <Typography variant="copy-sm" className="muted">
              Join your call on this box and press record. It hears you through {state?.microphone ?? "a microphone"} and the meeting through the speakers, then writes a transcript you can search and ask about.
            </Typography>
          )}
          {state && !state.microphone && !local && (
            <Typography variant="copy-sm" style={{ color: "var(--vesper-text-warning, var(--vesper-yellow-700))" }}>No microphone is plugged in: only the meeting's side will be recorded.</Typography>
          )}
          <div className="row">
            <div className="grow"><TextInput value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Name it (optional), e.g. Design sync"
              onKeyDown={(e) => { if (e.key === "Enter") start(); }} /></div>
            <Button onClick={start} disabled={busy || !state} iconLeft={<Icon kind="microphone" />}>Record</Button>
          </div>
          <Typography variant="copy-xs" className="muted">Let everyone know you're recording. {local ? "The recording goes to your device to be transcribed; audio and transcripts stay there." : "Audio and transcripts stay on this box."}</Typography>
        </>
      )}
    </Card>
  );
}

/** Studio, on the Transcribe tab. */
export function openTranscribe() {
  navigate("studio");
  history.replaceState(null, "", "/app/studio#transcribe");
}

/** The Home card while a meeting is being recorded. */
export function RecordingNow() {
  const { state } = useRecorder();
  const rec = state?.recording;
  if (!rec) return null;
  return (
    <Card tone="accent" onClick={openTranscribe}>
      <div className="row-between"><Eyebrow>Recording</Eyebrow><span className="rec-dot mono">{clock(rec.seconds)}</span></div>
      <Typography variant="copy-md">{rec.title}</Typography>
      <div className="grid grid-2" style={{ gap: 12 }}>
        <Meter label="You" device={rec.mic} db={rec.mic_db} />
        <Meter label="The meeting" device={rec.system} db={rec.system_db} />
      </div>
    </Card>
  );
}
