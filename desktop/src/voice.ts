// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Talking with the box: listen through this computer's microphone (the box
// turns speech into text on its CPU), and speak replies in the box's own
// voice (Kokoro, also on the CPU), sentence by sentence as they stream in.

import { ApiError, authHeader, upload } from "./api";

export interface BoxVoice { id: string; name: string; accent: string; gender: string; english: boolean; favourite: boolean }
export interface VoiceStatus {
  state: "ready" | "installing" | "starting" | "stopped" | "failed" | "not_installed";
  step: string | null; error: string | null; speak: boolean; listen: boolean;
  voice: string; speed: number; voices: BoxVoice[];
}

const RATE = 24000;

/** One shared audio context: created on first use (after a click). */
let shared: AudioContext | null = null;
export function audio(): AudioContext {
  if (!shared) shared = new AudioContext({ latencyHint: "interactive" });
  if (shared.state === "suspended") void shared.resume();
  return shared;
}

/** Markdown → something worth saying out loud. */
export function speakable(md: string): string {
  return md
    .replace(/```[\s\S]*?```/g, " (code on screen) ")
    .replace(/`([^`]+)`/g, "$1")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, "")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/^\s{0,3}#{1,6}\s+/gm, "")
    .replace(/^\s*[-*+]\s+/gm, "")
    .replace(/^\s*\d+\.\s+/gm, "")
    .replace(/^\s*>\s?/gm, "")
    .replace(/\|/g, ", ")
    .replace(/[*_~]{1,3}([^*_~]+)[*_~]{1,3}/g, "$1")
    .replace(/https?:\/\/\S+/g, "a link")
    .replace(/\s+/g, " ")
    .trim();
}

/** The end of the last complete sentence in `s` (so "3.5" doesn't split), or -1. */
export function lastSentenceEnd(s: string): number {
  let cut = -1;
  for (let i = 0; i < s.length; i++) {
    if ("\n.!?".includes(s[i]) && (i + 1 === s.length || /\s/.test(s[i + 1]))) cut = i + 1;
  }
  return cut;
}

async function synthesize(text: string, voice?: string, signal?: AbortSignal): Promise<Float32Array> {
  const res = await fetch("/voice/speak", {
    method: "POST",
    headers: { ...authHeader(), "content-type": "application/json" },
    body: JSON.stringify({ text, voice, format: "pcm" }),
    signal,
  });
  if (!res.ok) {
    let msg = res.statusText;
    try { msg = (await res.json()).error ?? msg; } catch { /* not JSON */ }
    throw new ApiError(res.status, msg);
  }
  const buf = await res.arrayBuffer();
  const pcm = new Int16Array(buf, 0, Math.floor(buf.byteLength / 2));
  const out = new Float32Array(pcm.length);
  for (let i = 0; i < pcm.length; i++) out[i] = pcm[i] / 32768;
  return out;
}

/**
 * Speaks a streaming reply. Feed it the reply text as it grows; it cuts
 * complete sentences, fetches each one's audio as soon as it's known (so the
 * next is ready before the current ends), and schedules them back to back
 * on one clock, so there are no gaps or level jumps between sentences.
 */
export class Speaker {
  /** 0…1, smoothed by the caller: how loud the voice is right now. */
  readonly analyser: AnalyserNode;
  private out: GainNode;
  private spokenUpTo = 0;
  private queue: Promise<Float32Array | null>[] = [];
  private pumping = false;
  private finished = true;
  private playing = 0;
  private nextAt = 0;
  private sources = new Set<AudioBufferSourceNode>();
  private abort = new AbortController();
  voice?: string;
  onStart?: () => void;
  onDone?: () => void;
  onError?: (e: Error) => void;

  constructor() {
    const ctx = audio();
    this.out = ctx.createGain();
    this.analyser = ctx.createAnalyser();
    this.analyser.fftSize = 1024;
    this.out.connect(this.analyser);
    this.analyser.connect(ctx.destination);
  }

  /** A new reply is coming. */
  begin() {
    this.stop();
    this.abort = new AbortController();
    this.spokenUpTo = 0;
    this.finished = false;
  }

  /** The reply so far. The first sentence goes alone so speech starts fast. */
  text(full: string) {
    if (full.length <= this.spokenUpTo) return;
    const rest = full.slice(this.spokenUpTo);
    const cut = lastSentenceEnd(rest);
    if (cut < 0) return;
    const ready = rest.slice(0, cut);
    if (this.spokenUpTo === 0 || ready.length >= 160 || ready.includes("\n")) {
      this.spokenUpTo += ready.length;
      this.say(ready);
    }
  }

  finish(full: string) {
    if (full.length > this.spokenUpTo) this.say(full.slice(this.spokenUpTo));
    this.spokenUpTo = full.length;
    this.finished = true;
    this.maybeDone();
  }

  /** Say something on its own (a greeting, a preview). */
  say(markdown: string) {
    const text = speakable(markdown);
    if (!text) return;
    const signal = this.abort.signal;
    this.queue.push(synthesize(text, this.voice, signal).catch((e) => {
      if (e.name !== "AbortError") this.onError?.(e);
      return null;
    }));
    void this.pump();
  }

  get busy() { return !this.finished || this.playing > 0 || this.queue.length > 0; }

  stop() {
    this.abort.abort();
    this.queue = [];
    for (const s of this.sources) { try { s.stop(); } catch { /* already stopped */ } }
    this.sources.clear();
    this.playing = 0;
    this.nextAt = 0;
    this.finished = true;
  }

  private async pump() {
    if (this.pumping) return;
    this.pumping = true;
    const signal = this.abort.signal;
    while (this.queue.length && !signal.aborted) {
      const samples = await this.queue[0];
      if (signal.aborted) break;
      this.queue.shift();
      if (samples?.length) this.play(samples);
    }
    this.pumping = false;
    this.maybeDone();
  }

  private play(samples: Float32Array) {
    const ctx = audio();
    const buffer = ctx.createBuffer(1, samples.length, RATE);
    buffer.copyToChannel(samples as Float32Array<ArrayBuffer>, 0);
    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(this.out);
    // Back to back; a little lead time if we fell behind, so it never clicks.
    const at = Math.max(this.nextAt, ctx.currentTime + 0.03);
    if (this.playing === 0) this.onStart?.();
    src.start(at);
    this.nextAt = at + buffer.duration;
    this.playing++;
    this.sources.add(src);
    src.onended = () => {
      this.sources.delete(src);
      this.playing = Math.max(0, this.playing - 1);
      this.maybeDone();
    };
  }

  private maybeDone() {
    if (this.finished && this.playing === 0 && this.queue.length === 0 && !this.pumping) this.onDone?.();
  }
}

/** Loudness of an analyser's signal right now, in dBFS. */
export function decibels(a: AnalyserNode, buf: Float32Array<ArrayBuffer>): number {
  a.getFloatTimeDomainData(buf);
  let sum = 0;
  for (let i = 0; i < buf.length; i++) sum += buf[i] * buf[i];
  return 20 * Math.log10(Math.sqrt(sum / buf.length) + 1e-6);
}

/**
 * Listens for one thing said: waits for speech to start, records until about
 * a second of quiet after it, and returns what the box heard. The noise floor
 * adapts, so a fan or a quiet room both work.
 */
export class Listener {
  readonly analyser: AnalyserNode;
  private stream: MediaStream;
  private source: MediaStreamAudioSourceNode;
  private recorder: MediaRecorder | null = null;
  private timer = 0;
  muted = false;
  /** Push-to-talk: keep recording, however quiet, until finishNow(). */
  ptt = false;

  private constructor(stream: MediaStream) {
    const ctx = audio();
    this.stream = stream;
    this.source = ctx.createMediaStreamSource(stream);
    this.analyser = ctx.createAnalyser();
    this.analyser.fftSize = 1024;
    this.source.connect(this.analyser);
  }

  static async open(): Promise<Listener> {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1 },
    });
    return new Listener(stream);
  }

  get device(): string { return this.stream.getAudioTracks()[0]?.label ?? "Microphone"; }

  /** Resolves with the words, or "" if nothing was said before `patience` ms. */
  listen(onSpeech: () => void, patience = 12000, ptt = false): Promise<string> {
    this.ptt = ptt;
    this.cancel();
    return new Promise((resolve, reject) => {
      const mime = MediaRecorder.isTypeSupported("audio/webm;codecs=opus") ? "audio/webm;codecs=opus" : "audio/webm";
      const rec = new MediaRecorder(this.stream, { mimeType: mime });
      this.recorder = rec;
      const parts: Blob[] = [];
      rec.ondataavailable = (e) => { if (e.data.size) parts.push(e.data); };
      const buf = new Float32Array(this.analyser.fftSize);
      let floor = -55;
      let speaking = false;
      let voicedMs = 0;
      let quietMs = 0;
      const began = performance.now();
      let last = began;
      let heard = ptt;
      if (ptt) onSpeech();
      rec.onstop = async () => {
        window.clearInterval(this.timer);
        if (!heard) return resolve("");
        try {
          const blob = new Blob(parts, { type: "audio/webm" });
          const r = await upload<{ text: string }>("/voice/listen", blob);
          resolve(cleanTranscript(r.text));
        } catch (e) { reject(e); }
      };
      rec.start(250);
      this.timer = window.setInterval(() => {
        const now = performance.now();
        const dt = now - last;
        last = now;
        const db = this.muted ? -100 : decibels(this.analyser, buf);
        const loud = db > Math.max(floor + 14, -48);
        if (this.ptt) {
          if (now - began > 60000) rec.stop();
          return;
        }
        if (heard && !speaking) { speaking = true; quietMs = 0; } // was push-to-talk, now hands-free
        if (!speaking) {
          // Track the room's level while nobody is talking.
          floor = db < floor ? floor * 0.7 + db * 0.3 : floor * 0.98 + db * 0.02;
          voicedMs = loud ? voicedMs + dt : 0;
          if (voicedMs > 160) { speaking = true; heard = true; quietMs = 0; onSpeech(); }
          else if (now - began > patience) rec.stop();
        } else {
          quietMs = loud ? 0 : quietMs + dt;
          if (quietMs > 900 || now - began > 45000) rec.stop();
        }
      }, 30);
    });
  }

  /** Stop listening now and send what was said so far. */
  finishNow() { if (this.recorder?.state === "recording") this.recorder.stop(); }

  cancel() {
    window.clearInterval(this.timer);
    if (this.recorder?.state === "recording") {
      this.recorder.onstop = null;
      this.recorder.stop();
    }
    this.recorder = null;
  }

  close() {
    this.cancel();
    this.source.disconnect();
    for (const t of this.stream.getTracks()) t.stop();
  }
}

/** Whisper hallucinates on silence ("Thank you.", "you"): drop those. */
export function cleanTranscript(text: string): string {
  const t = text.trim();
  if (/^(\[.*\]|\(.*\)|thank you\.?|thanks for watching!?|you\.?|bye\.?)$/i.test(t)) return "";
  return t;
}
