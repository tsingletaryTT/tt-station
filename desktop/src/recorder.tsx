// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Meeting recorder: this computer records its own microphone and uploads the
// recording to the box (`POST /jobs/transcribe`), which transcribes it like
// any other recording.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { Icon } from "@tenstorrent/vesper/icons";
import { currentToken } from "./api";
import { navigate } from "./App";
import { Card, Eyebrow, toastError, toastOk } from "./ui";
import { ask } from "./dialog";

export interface Recording {
  id: string; title: string; started: number; seconds: number; max_seconds: number; ended: boolean;
  mic: string | null; mic_db: number | null;
}
export interface RecordState { microphone: string | null; recording: Recording | null }

// ---------------------------------------------------------------- recording on this computer

const MAX_SECONDS = 4 * 3600;

/** This computer's microphone, recorded here, then sent to the device to transcribe. */
class LocalRecorder {
  state: RecordState = { microphone: "This computer's microphone", recording: null };
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
    this.state.recording = { id: "local", title: name, started, seconds: 0, max_seconds: MAX_SECONDS, ended: false, mic: label, mic_db: null };
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
    if (!res.ok) throw new Error(`The box didn't take the recording (${res.status}).`);
    return "Saved. The transcript will be ready in a few minutes; you'll get a notification.";
  }
}

export const localRecorder = new LocalRecorder();

// The menu bar and Tori (another window) ask this one to record.
window.ttDesktop?.onClientRecord?.(async ({ nonce, action, title }) => {
  try {
    if (action === "start") { await localRecorder.start(title); window.ttDesktop?.clientRecordDone?.(nonce, { ok: true, summary: "Started recording on this computer" }); }
    else window.ttDesktop?.clientRecordDone?.(nonce, { ok: true, summary: (await localRecorder.stop()).split(".")[0] });
  } catch (e) {
    window.ttDesktop?.clientRecordDone?.(nonce, { ok: false, error: (e as Error).message });
  }
});

/** Start recording a meeting on this computer. */
export async function startRecording(title?: string) {
  await localRecorder.start(title);
}

/** Stop; uploads the recording and queues its transcription. Says what happened. */
export function stopRecording(discard = false): Promise<string> {
  return localRecorder.stop(discard);
}

/** This computer's recorder, kept current (the meters update while recording). */
export function useRecorder() {
  const [state, setState] = useState<RecordState>(localRecorder.state);
  useEffect(() => {
    setState(localRecorder.state);
    return localRecorder.subscribe(() => setState(localRecorder.state));
  }, []);
  return { state };
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
  const { state } = useRecorder();
  const [title, setTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const rec = state?.recording;

  const start = async () => {
    setBusy(true);
    try { await startRecording(title.trim() || undefined); setTitle(""); }
    catch (e) { toastError(e); } finally { setBusy(false); }
  };
  const stop = async (discard = false) => {
    setBusy(true);
    try {
      toastOk(await stopRecording(discard));
      onQueued?.();
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
          <Meter label="Microphone" device={rec.mic} db={rec.mic_db} />
          {rec.ended && <Typography variant="copy-sm" style={{ color: "var(--vesper-text-error)" }}>The recording stopped on its own (it hit {clock(rec.max_seconds)}). Save what you have.</Typography>}
          <div className="row">
            <Button onClick={() => stop()} disabled={busy}>Stop and transcribe</Button>
            <Button variant="ghost" disabled={busy} onClick={async () => { if (await ask("Throw this recording away?", { body: "Nothing is kept or transcribed.", confirm: "Discard", danger: true })) stop(true); }}>Discard</Button>
          </div>
        </>
      ) : (
        <>
          <Typography variant="copy-sm" className="muted">
            Records this computer's microphone, then the box writes a transcript you can search and ask about. On a call through the speakers it hears both sides; with headphones on, only you (most systems don't let apps record other apps' sound).
          </Typography>
          <div className="row">
            <div className="grow"><TextInput value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Name it (optional), e.g. Design sync"
              onKeyDown={(e) => { if (e.key === "Enter") start(); }} /></div>
            <Button onClick={start} disabled={busy} iconLeft={<Icon kind="microphone" />}>Record</Button>
          </div>
          <Typography variant="copy-xs" className="muted">Let everyone know you're recording. The recording goes to your box to be transcribed; audio and transcripts stay there.</Typography>
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
      <Meter label="Microphone" device={rec.mic} db={rec.mic_db} />
    </Card>
  );
}
