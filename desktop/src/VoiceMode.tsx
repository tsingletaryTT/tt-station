// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Voice mode: talk to the box hands-free. It listens until you stop
// talking, thinks, answers out loud in its own voice, and listens again.
// Tap the orb (or Space) to cut it off or to finish your turn early.

import { useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { IconButton } from "@tenstorrent/vesper/icon-button";
import { Icon } from "@tenstorrent/vesper/icons";
import { Select } from "@tenstorrent/vesper/select";
import { Typography } from "@tenstorrent/vesper/typography";
import { get, post, put } from "./api";
import { toastError } from "./ui";
import { decibels, Listener, Speaker, type VoiceStatus } from "./voice";

export type Phase = "starting" | "listening" | "hearing" | "transcribing" | "thinking" | "speaking" | "paused";

export interface AskHooks { onText: (full: string) => void; onDone: (full: string) => void; onError: (msg: string) => void; onReset?: () => void }

const LABEL: Record<Phase, string> = {
  starting: "Getting ready…",
  listening: "Listening",
  hearing: "Listening",
  transcribing: "Got it…",
  thinking: "Thinking…",
  speaking: "Speaking · tap to interrupt",
  paused: "Paused · tap to talk",
};

export function VoiceMode({ ask, onClose, canChat, compact = false }: {
  ask: (text: string, hooks: AskHooks) => void;
  onClose: () => void;
  canChat: boolean;
  /** In Tori's small window: a smaller orb, no voice picker. */
  compact?: boolean;
}) {
  const [status, setStatus] = useState<VoiceStatus | null>(null);
  const [phase, setPhase] = useState<Phase>("starting");
  const [said, setSaid] = useState("");
  const [reply, setReply] = useState("");
  const [muted, setMuted] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const listener = useRef<Listener | null>(null);
  const speaker = useRef<Speaker | null>(null);
  const phaseRef = useRef<Phase>("starting");
  const turnId = useRef(0);
  // The latest `ask` (it closes over the conversation, which changes each turn).
  const askRef = useRef(ask);
  askRef.current = ask;
  const alive = useRef(true);
  const go = (p: Phase) => { phaseRef.current = p; setPhase(p); };

  // Voice service status (and setup progress).
  useEffect(() => {
    let stop = false;
    const load = () => get<VoiceStatus>("/voice").then((s) => !stop && setStatus(s)).catch(() => {});
    load();
    const t = setInterval(load, status?.state === "ready" ? 30000 : 2500);
    return () => { stop = true; clearInterval(t); };
  }, [status?.state]);

  const ready = status?.state === "ready";

  const turn = async () => {
    const l = listener.current;
    if (!l || !alive.current) return;
    go("listening");
    try {
      const text = await l.listen(() => go("hearing"));
      if (!alive.current || phaseRef.current === "paused") return;
      if (!text) { go("paused"); return; }
      setSaid(text);
      setReply("");
      go("thinking");
      const s = speaker.current!;
      s.begin();
      const id = ++turnId.current;
      const current = () => id === turnId.current && alive.current;
      askRef.current(text, {
        onText: (full) => { if (current()) { setReply(full); s.text(full); } },
        onDone: (full) => { if (current()) { setReply(full); s.finish(full); } },
        onError: (msg) => { if (current()) { setProblem(msg); s.stop(); go("paused"); } },
        // The reply so far was retracted (a guess before a tool call): don't say it.
        onReset: () => { if (current()) { s.begin(); setReply(""); if (phaseRef.current === "speaking") go("thinking"); } },
      });
    } catch (e) {
      setProblem((e as Error).message);
      go("paused");
    }
  };

  // Open the microphone and the voice once the service is ready.
  useEffect(() => {
    if (!ready || listener.current) return;
    alive.current = true;
    (async () => {
      try {
        listener.current = await Listener.open();
      } catch (e) {
        setProblem((e as Error).name === "NotAllowedError"
          ? "TT Station isn't allowed to use the microphone."
          : "This computer has no microphone TT Station can use. Plug one in, then try again.");
        go("paused");
        return;
      }
      const s = new Speaker();
      s.voice = status?.voice;
      s.onStart = () => go("speaking");
      s.onDone = () => {
        const p = phaseRef.current;
        if (alive.current && (p === "speaking" || p === "thinking")) void turn();
      };
      s.onError = (e) => setProblem(`The voice failed: ${e.message}`);
      speaker.current = s;
      if (canChat) void turn(); else go("paused");
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready]);

  useEffect(() => () => {
    alive.current = false;
    speaker.current?.stop();
    listener.current?.close();
  }, []);

  useEffect(() => { if (listener.current) listener.current.muted = muted; }, [muted]);

  const tap = () => {
    const p = phaseRef.current;
    if (p === "speaking" || p === "thinking") { turnId.current++; speaker.current?.stop(); void turn(); }
    else if (p === "hearing") listener.current?.finishNow();
    else if (p === "paused") { setProblem(null); void turn(); }
    else if (p === "listening") { listener.current?.cancel(); go("paused"); }
  };

  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
      else if (e.key === " " && !(e.target instanceof HTMLInputElement)) { e.preventDefault(); tap(); }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  });

  const setVoice = async (id: string) => {
    try {
      await put("/voice", { voice: id });
      setStatus((s) => (s ? { ...s, voice: id } : s));
      if (speaker.current) {
        speaker.current.voice = id;
        if (phaseRef.current !== "speaking" && phaseRef.current !== "thinking") {
          listener.current?.cancel();
          speaker.current.begin();
          speaker.current.finish("Hi, this is how I sound now.");
        }
      }
    } catch (e) { toastError(e); }
  };

  return (
    <div className={`voice-mode ${compact ? "compact" : ""}`} role="dialog" aria-label="Voice mode">
      {!compact && <div className="voice-top">
        <span className="eyebrow">Voice</span>
        {ready && status && (
          <div style={{ width: 240 }}>
            <Select size="sm" value={status.voice} onValueChange={(v) => v && setVoice(v)} aria-label="The box's voice"
              options={status.voices.filter((v) => v.english).map((v) => ({ value: v.id, label: `${v.name} · ${v.accent} ${v.gender}${v.favourite ? " ★" : ""}` }))} />
          </div>
        )}
      </div>}

      {!ready ? (
        <div className="voice-center">
          <Orb phase="starting" mic={null} out={null} size={compact ? 220 : 380} />
          <Typography variant="heading-sm">{status?.state === "installing" ? status.step ?? "Setting up" : "The box needs its voice"}</Typography>
          <Typography variant="copy-sm" className="muted" style={{ maxWidth: 440, textAlign: "center" }}>
            {status?.state === "installing"
              ? "Downloading a natural voice (Kokoro) and speech recognition (Whisper). Both run on the box's CPU, so the chips stay on your model."
              : status?.error ?? "A natural voice and fast speech recognition, both running on the box's CPU so your model keeps the chips. About 7 GB, one time, no accounts."}
          </Typography>
          {status && status.state !== "installing" && status.state !== "starting" && (
            <Button onClick={async () => { try { await post("/voice/setup"); setStatus({ ...status, state: "installing", step: "Starting" }); } catch (e) { toastError(e); } }}>
              Set up the voice
            </Button>
          )}
        </div>
      ) : (
        <div className="voice-center" onClick={tap}>
          <Orb phase={phase} mic={listener.current?.analyser ?? null} out={speaker.current?.analyser ?? null} size={compact ? 220 : 380} />
          <div className="voice-status">{!canChat ? "Start a chat model to talk" : LABEL[phase]}</div>
          <div className="voice-said">{said}</div>
          <div className="voice-reply">{tail(reply)}</div>
          {problem && <Typography variant="copy-sm" style={{ color: "var(--vesper-text-error)", maxWidth: 520, textAlign: "center" }}>{problem}</Typography>}
        </div>
      )}

      <div className="voice-controls">
        <IconButton icon={<Icon kind="microphone" />} variant={muted ? "subtle" : "ghost"} aria-label={muted ? "Unmute" : "Mute"}
          onClick={() => setMuted((m) => !m)} disabled={!ready} className={muted ? "voice-muted" : ""} />
        <IconButton icon={<Icon kind="close" />} variant="subtle" aria-label="End voice mode" onClick={onClose} />
      </div>
      <div className="voice-hint muted">
        {listener.current ? `${listener.current.device} · ` : ""}<span className="kbd">Space</span> interrupt or finish · <span className="kbd">Esc</span> end
      </div>
    </div>
  );
}

/** The last couple of sentences of the reply, for a caption. */
function tail(text: string): string {
  const t = text.replace(/```[\s\S]*?(```|$)/g, "").replace(/[#*_`>]/g, "").trim();
  if (t.length <= 220) return t;
  const cut = t.slice(-220);
  const at = cut.search(/[.!?]\s/);
  return "…" + (at >= 0 ? cut.slice(at + 2) : cut);
}

// ---------------------------------------------------------------- orb

const COLORS: Record<Phase, [number, number, number]> = {
  starting: [70, 120, 130],
  listening: [86, 196, 168],
  hearing: [110, 222, 186],
  transcribing: [90, 170, 210],
  thinking: [128, 120, 220],
  speaking: [70, 175, 210],
  paused: [90, 110, 120],
};

/**
 * A soft, glowing orb drawn on a canvas. Everything in it is a radial
 * gradient that fades to nothing well inside the canvas, so there are no
 * edges to see. Size follows the voice through a spring (fast to rise, slow
 * to fall); motion is integrated frame by frame, so speed changes never jump.
 */
export function Orb({ phase, mic, out, size = 380 }: { phase: Phase; mic: AnalyserNode | null; out: AnalyserNode | null; size?: number }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const phaseRef = useRef(phase);
  phaseRef.current = phase;
  const nodes = useRef({ mic, out });
  nodes.current = { mic, out };

  useEffect(() => {
    const el = canvas.current!;
    const ctx = el.getContext("2d")!;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const SIZE = size;
    const dpr = Math.min(2, window.devicePixelRatio || 1);
    el.width = SIZE * dpr;
    el.height = SIZE * dpr;
    ctx.scale(dpr, dpr);
    const buf = new Float32Array(1024);
    let level = 0, pos = 0, vel = 0, spin = 0, breath = 0;
    let color = [...COLORS[phaseRef.current]];
    let last = performance.now();
    let raf = 0;

    const frame = (now: number) => {
      const dt = Math.min(0.05, (now - last) / 1000);
      last = now;
      const p = phaseRef.current;
      // Target loudness, 0…1.
      const src = p === "speaking" ? nodes.current.out : p === "listening" || p === "hearing" ? nodes.current.mic : null;
      const db = src ? decibels(src, buf) : -100;
      const target = Math.max(0, Math.min(1, (db + 55) / 40));
      level += (target - level) * (1 - Math.exp(-dt / (target > level ? 0.06 : 0.28)));
      // A critically damped spring turns the level into size.
      const goal = (p === "thinking" ? 0.12 : 0) + level * (reduce ? 0.3 : 1);
      const k = 90;
      vel += ((goal - pos) * k - 2 * Math.sqrt(k) * vel) * dt;
      pos += vel * dt;
      // Integrated motion: speed can change, position never jumps.
      const speed = p === "thinking" ? 1.1 : p === "speaking" ? 0.55 : p === "paused" || p === "starting" ? 0.12 : 0.3;
      spin += dt * (reduce ? 0 : speed);
      // Reduced motion: no drifting or breathing; loudness still shows, gently, in its size.
      breath += reduce ? 0 : dt * (p === "thinking" ? 2.2 : 0.9);
      const target_c = COLORS[p];
      for (let i = 0; i < 3; i++) color[i] += (target_c[i] - color[i]) * (1 - Math.exp(-dt / 0.4));

      const [r, g, b] = color.map(Math.round);
      const c = SIZE / 2;
      const base = SIZE * 0.205 * (1 + 0.025 * Math.sin(breath)) * (1 + 0.22 * pos);
      ctx.clearRect(0, 0, SIZE, SIZE);

      // Halo: fades to zero just inside the canvas edge, whatever the size.
      const halo = ctx.createRadialGradient(c, c, base * 0.5, c, c, c - 2);
      halo.addColorStop(0, `rgba(${r},${g},${b},${0.32 + 0.25 * pos})`);
      halo.addColorStop(0.45, `rgba(${r},${g},${b},${0.10 + 0.08 * pos})`);
      halo.addColorStop(1, `rgba(${r},${g},${b},0)`);
      ctx.fillStyle = halo;
      ctx.fillRect(0, 0, SIZE, SIZE);

      // Three soft lights drifting inside the core.
      ctx.globalCompositeOperation = "lighter";
      for (let i = 0; i < 3; i++) {
        const a = spin * (1 + i * 0.37) + (i * Math.PI * 2) / 3;
        const d = base * (0.28 + 0.06 * Math.sin(spin * 0.7 + i));
        const x = c + Math.cos(a) * d, y = c + Math.sin(a) * d;
        const rr = base * (0.75 + 0.1 * pos);
        const tint = i === 0 ? [r, g, b] : i === 1 ? [Math.min(255, r + 40), Math.min(255, g + 20), b] : [r, Math.min(255, g + 30), Math.min(255, b + 30)];
        const blob = ctx.createRadialGradient(x, y, 0, x, y, rr);
        blob.addColorStop(0, `rgba(${tint[0]},${tint[1]},${tint[2]},0.55)`);
        blob.addColorStop(1, `rgba(${tint[0]},${tint[1]},${tint[2]},0)`);
        ctx.fillStyle = blob;
        ctx.beginPath();
        ctx.arc(x, y, rr, 0, Math.PI * 2);
        ctx.fill();
      }
      // A bright centre that fades out softly.
      const core = ctx.createRadialGradient(c, c, 0, c, c, base);
      core.addColorStop(0, `rgba(235,250,250,${0.35 + 0.3 * pos})`);
      core.addColorStop(0.55, `rgba(${r},${g},${b},0.18)`);
      core.addColorStop(1, `rgba(${r},${g},${b},0)`);
      ctx.fillStyle = core;
      ctx.beginPath();
      ctx.arc(c, c, base, 0, Math.PI * 2);
      ctx.fill();
      ctx.globalCompositeOperation = "source-over";

      raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
  }, [size]);

  return <canvas ref={canvas} className="orb" style={{ width: size, height: size }} aria-hidden />;
}
