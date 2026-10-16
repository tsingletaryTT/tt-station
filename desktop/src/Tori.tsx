// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Tori: the assistant you summon over whatever you're doing (Ctrl+Alt+Space).
// She's already listening when she appears -- just talk -- or type instead.
// Spoken questions get spoken answers. Ask her anything, or tell her to open
// apps, find files, change the volume, pause the music. Once she's opened
// something she gets out of the way; Esc puts her away any time.

import { useCallback, useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { externalLinks } from "./links";
import { Button } from "@tenstorrent/vesper/button";
import { IconButton } from "@tenstorrent/vesper/icon-button";
import { Icon } from "@tenstorrent/vesper/icons";
import { type ActionReport, clientKind, get, post, runConfirmed, runOnClient, short, streamChat, upsertAction } from "./api";
import { toastError } from "./ui";
import { Orb, type Phase } from "./VoiceMode";
import { Listener, Speaker, type VoiceStatus } from "./voice";
import { smoothBehavior } from "./stickToBottom";

interface Turn { role: "user" | "assistant"; content: string; actions?: ActionReport[]; error?: string }
interface Brain { where: "chips" | "cpu" | null; model: string | null }

const STORE = "tt.tori.quick";
const load = (): Turn[] => { try { return JSON.parse(localStorage.getItem(STORE) ?? "[]"); } catch { return []; } };

const IDEAS = ["Open Claude and VS Code", "Turn the volume down", "Find my latest PDF", "What's the box doing?"];
/** Tools that put something new on screen: Tori steps aside after them. */
const SHOWS_SOMETHING = new Set(["open_app", "open_folder"]);

const HINT: Partial<Record<Phase, string>> = {
  listening: "Listening… or type",
  hearing: "Go ahead…",
  transcribing: "Got it…",
  thinking: "Thinking…",
  speaking: "Speaking · Space to interrupt",
  paused: "Hold Space to talk, or type",
};

export function ToriWindow() {
  const [turns, setTurns] = useState<Turn[]>(load);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [phase, setPhaseState] = useState<Phase>("paused");
  const [brain, setBrain] = useState<Brain | null>(null);
  const [voice, setVoice] = useState<VoiceStatus | null>(null);
  const [micError, setMicError] = useState<string | null>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const abort = useRef<AbortController | null>(null);
  const listener = useRef<Listener | null>(null);
  const speaker = useRef<Speaker | null>(null);
  const phaseRef = useRef<Phase>("paused");
  const turnsRef = useRef(turns);
  turnsRef.current = turns;
  const visible = useRef(true);
  const stepAside = useRef(false);
  const setPhase = (p: Phase) => { phaseRef.current = p; setPhaseState(p); };

  const canChat = !!brain?.where;
  const voiceReady = voice?.state === "ready";

  const hide = useCallback(() => {
    visible.current = false;
    abort.current?.abort();
    speaker.current?.stop();
    listener.current?.close();
    listener.current = null;
    setPhase("paused");
    window.ttDesktop?.hideTori();
  }, []);

  // ---------------------------------------------------------------- asking

  const ask = async (text: string, spoken: boolean) => {
    const prompt = text.trim();
    if (!prompt) return;
    abort.current?.abort();
    listener.current?.cancel();
    const ctrl = new AbortController();
    abort.current = ctrl;
    stepAside.current = false;
    const history: Turn[] = [...turnsRef.current, { role: "user", content: prompt }];
    setTurns([...history, { role: "assistant", content: "" }]);
    setDraft("");
    setBusy(true);
    setPhase("thinking");
    const s = spoken && voiceReady ? speaker.current : null;
    s?.begin();
    const patch = (fn: (t: Turn) => Turn) => setTurns((all) => all.map((t, i) => (i === all.length - 1 ? fn(t) : t)));
    let said = "";
    try {
      const msgs = history.map((t) => ({ role: t.role, content: t.content })).slice(-12);
      const client = await clientKind();
      for await (const e of streamChat(msgs, { think: false, detailed: false, persona: "tori", client }, ctrl.signal)) {
        if (e.type === "delta") { said += e.text; patch((t) => ({ ...t, content: said })); s?.text(said); }
        else if (e.type === "reset") { said = ""; patch((t) => ({ ...t, content: "" })); s?.begin(); }
        else if (e.type === "action") {
          if (SHOWS_SOMETHING.has(e.action.tool) && e.action.status === "done") stepAside.current = true;
          // "Open Safari" from a Mac: the Mac does it, then the device carries on.
          if (e.action.status === "on_client") void runOnClient(e.action);
          patch((t) => ({ ...t, actions: upsertAction(t.actions, e.action) }));
        }
        else if (e.type === "done") { said = e.reply; patch((t) => ({ ...t, content: e.reply })); }
        else if (e.type === "error") patch((t) => ({ ...t, error: e.error }));
      }
    } catch (e) {
      if ((e as Error).name !== "AbortError") patch((t) => ({ ...t, error: (e as Error).message }));
    } finally {
      if (abort.current === ctrl) { setBusy(false); abort.current = null; }
    }
    if (ctrl.signal.aborted) return;
    if (s) s.finish(said); // onDone decides what's next
    else afterReply(false);
  };

  /** The reply is out (spoken or shown): step aside, or listen for a follow-up. */
  const afterReply = (spoken: boolean) => {
    if (!visible.current) return;
    if (stepAside.current) {
      // Let the new window come to the front.
      setTimeout(() => { if (phaseRef.current !== "listening" && phaseRef.current !== "hearing") hide(); }, spoken ? 250 : 900);
      setPhase("paused");
      return;
    }
    if (spoken) void listen(7000); // a follow-up, without pressing anything
    else setPhase("paused");
  };

  // ---------------------------------------------------------------- listening

  const listen = async (patience = 12000, ptt = false) => {
    if (!voiceReady || !visible.current) return;
    try {
      if (!listener.current) listener.current = await Listener.open();
      setMicError(null);
    } catch (e) {
      setMicError((e as Error).name === "NotAllowedError" ? "TT Station isn't allowed to use the microphone." : "No microphone found. Plug one in to talk to Tori.");
      setPhase("paused");
      return;
    }
    setPhase("listening");
    try {
      const text = await listener.current.listen(() => setPhase("hearing"), patience, ptt);
      if (!visible.current || phaseRef.current === "paused") return;
      if (!text) { setPhase("paused"); return; }
      setPhase("transcribing");
      await ask(text, true);
    } catch (e) {
      setMicError((e as Error).message);
      setPhase("paused");
    }
  };

  const stopListening = () => {
    listener.current?.cancel();
    if (phaseRef.current === "listening" || phaseRef.current === "hearing") setPhase("paused");
  };

  // ---------------------------------------------------------------- lifecycle

  useEffect(() => {
    document.documentElement.classList.add("tori-window");
    const s = new Speaker();
    s.onStart = () => setPhase("speaking");
    s.onDone = () => { if (phaseRef.current === "speaking" || phaseRef.current === "thinking") afterReply(true); };
    speaker.current = s;
    let stop = false;
    const poll = () => {
      get<Brain>("/assistant/brain").then((b) => !stop && setBrain(b)).catch(() => {});
      get<VoiceStatus>("/voice").then((v) => { if (!stop) { setVoice(v); s.voice = v.voice; } }).catch(() => {});
    };
    poll();
    const t = setInterval(poll, 10000);
    return () => { stop = true; clearInterval(t); s.stop(); listener.current?.close(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Summoned (first load, or the shortcut again): listen straight away.
  const summon = useCallback((mode: "type" | "talk") => {
    visible.current = true;
    setTimeout(() => input.current?.focus(), 30);
    if (mode === "talk" && !busy) void listen();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [voiceReady, busy]);

  const started = useRef(false);
  useEffect(() => {
    if (started.current || !voice) return;
    started.current = true;
    const q = new URLSearchParams(location.search);
    if (q.has("preload")) { visible.current = false; return; } // waiting, hidden, for the shortcut
    summon(q.has("type") ? "type" : "talk");
  }, [voice, summon]);

  useEffect(() => {
    window.ttDesktop?.onTori((mode) => {
      if (mode === "hidden") { visible.current = false; abort.current?.abort(); speaker.current?.stop(); listener.current?.close(); listener.current = null; setPhase("paused"); }
      else summonRef.current(mode);
    });
  }, []);
  const summonRef = useRef(summon);
  summonRef.current = summon;

  useEffect(() => { localStorage.setItem(STORE, JSON.stringify(turns.slice(-30))); }, [turns]);
  useEffect(() => { scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: smoothBehavior() }); }, [turns]);
  // Space: hold to talk, let go to send. A quick tap starts hands-free listening
  // (or interrupts Tori). Space types normally once there's text in the box.
  const spaceDown = useRef(0);
  useEffect(() => {
    const typing = () => document.activeElement === input.current && !!input.current?.value;
    const down = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); hide(); return; }
      if (e.key !== " " || typing()) return;
      e.preventDefault();
      if (e.repeat || spaceDown.current) return;
      spaceDown.current = Date.now();
      if (!voiceReady) return;
      speaker.current?.stop();
      abort.current?.abort();
      listener.current?.cancel();
      void listen(60000, true);
    };
    const up = (e: KeyboardEvent) => {
      if (e.key !== " " || !spaceDown.current) return;
      e.preventDefault();
      const held = Date.now() - spaceDown.current;
      spaceDown.current = 0;
      const l = listener.current;
      if (!l) return;
      if (held < 300) l.ptt = false; // a tap: keep listening hands-free
      else l.finishNow();
    };
    window.addEventListener("keydown", down);
    window.addEventListener("keyup", up);
    return () => { window.removeEventListener("keydown", down); window.removeEventListener("keyup", up); };
  });

  const confirm = async (a: ActionReport, approve: boolean) => {
    try {
      const r = await post<{ summary?: string; message?: string; status?: string; run?: { tool: string; args?: Record<string, unknown> } }>("/assistant/confirm", { action_id: a.id, approve });
      const note = (await runConfirmed(r)) ?? r?.summary ?? r?.message ?? (approve ? "Done." : "Cancelled.");
      setTurns((all) => all.map((t) => ({ ...t, actions: t.actions?.map((x) => (x.id === a.id ? { ...x, status: "done" as const, summary: note } : x)) })));
    } catch (e) { toastError(e); }
  };

  const listening = phase === "listening" || phase === "hearing";
  const recent = turns.slice(-8);

  return (
    <div className={`tori ${listening ? "is-listening" : ""}`}>
      <header className="tori-head">
        <div className="row" style={{ gap: 10 }}>
          <Icon kind="tenstorrent" style={{ width: 18, height: 18, color: "var(--vesper-text-accent)" }} />
          <b>Tori</b>
          <span className="tori-brain mono">
            {brain?.where === "chips" ? short(brain.model) : brain?.where === "cpu" ? `${brain.model} · CPU` : ""}
          </span>
          {brain?.where === "cpu" && (
            <button className="link-button" style={{ fontSize: 11, WebkitAppRegion: "no-drag" } as React.CSSProperties}
              title="Load your chat model onto the chips for faster answers"
              onClick={async () => { try { await post("/modes/chat", {}); setBrain({ where: "cpu", model: "loading onto the chips…" }); } catch (e) { toastError(e); } }}>
              Use the chips
            </button>
          )}
        </div>
        <div className="row" style={{ gap: 2 }}>
          {turns.length > 0 && <IconButton size="sm" variant="ghost" icon={<Icon kind="add" />} aria-label="New conversation" title="New conversation"
            onClick={() => { abort.current?.abort(); speaker.current?.stop(); setTurns([]); }} />}
          <IconButton size="sm" variant="ghost" icon={<Icon kind="close" />} aria-label="Put Tori away (Esc)" title="Put away (Esc)" onClick={hide} />
        </div>
      </header>

      <div className="tori-thread" ref={scroller}>
        {recent.length === 0 && (
          <div className="tori-empty">
            <div className="tori-hello">Hi, I'm Tori.</div>
            <div className="muted">{voiceReady ? "Just talk, or type. " : ""}Ask anything, or tell me what to do on this computer.</div>
            <div className="tori-ideas">
              {IDEAS.map((i) => <button key={i} onClick={() => void ask(i, false)} disabled={!canChat && !/open|volume/i.test(i)}>{i}</button>)}
            </div>
          </div>
        )}
        {recent.map((t, i) => t.role === "user"
          ? <div key={i} className="msg-user">{t.content}</div>
          : (
            <div key={i} className="tori-reply">
              {(t.actions ?? []).filter((a) => a.status !== "needs_confirmation").map((a) => (
                <span key={a.id} className="action"><Icon kind={a.status === "error" ? "warning" : a.status === "on_client" ? "spinner" : "checkmark"} width={12} height={12} />{a.summary}</span>
              ))}
              <div className="markdown selectable">
                {t.content ? <Markdown remarkPlugins={[remarkGfm]} components={externalLinks}>{t.content}</Markdown>
                  : busy && i === recent.length - 1 ? <span className="muted cursor" /> : null}
              </div>
              {(t.actions ?? []).filter((a) => a.status === "needs_confirmation").map((a) => (
                <div key={a.id} className="tori-confirm">
                  <span>{a.summary}</span>
                  <div className="row"><Button size="sm" onClick={() => confirm(a, true)}>Do it</Button><Button size="sm" variant="ghost" onClick={() => confirm(a, false)}>Cancel</Button></div>
                </div>
              ))}
              {t.error && <div style={{ color: "var(--vesper-text-error)", fontSize: 13 }}>{t.error}</div>}
            </div>
          ))}
      </div>

      {(micError || (brain && !canChat)) && (
        <div className="tori-warn">
          {micError ?? <>No model to think with right now, so I can only do quick commands.{" "}
            <Button size="xs" variant="subtle" onClick={async () => { try { await post("/modes/chat", {}); } catch (e) { toastError(e); } }}>Start chat</Button></>}
        </div>
      )}

      <div className="tori-bar">
        <button className="tori-orb" aria-label={listening ? "Stop listening" : "Talk"} title={listening ? "Stop listening" : "Talk (Space)"}
          disabled={!voiceReady}
          onClick={() => {
            if (listening) stopListening();
            else if (phase === "speaking") { speaker.current?.stop(); void listen(); }
            else void listen();
          }}>
          <Orb phase={phase} mic={listener.current?.analyser ?? null} out={speaker.current?.analyser ?? null} size={72} />
          {!listening && phase !== "speaking" && phase !== "thinking" && <Icon kind="microphone" className="tori-orb-mic" />}
        </button>
        <textarea ref={input} rows={1} value={draft}
          placeholder={!voiceReady && phase === "paused" ? "Type to Tori" : HINT[phase] ?? "Talk, or type to Tori"}
          onChange={(e) => {
            setDraft(e.target.value);
            if (e.target.value && (phaseRef.current === "listening" || phaseRef.current === "hearing")) stopListening();
            e.target.style.height = "auto";
            e.target.style.height = `${Math.min(120, e.target.scrollHeight)}px`;
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void ask(draft, false); }
          }} />
        {busy
          ? <IconButton icon={<Icon kind="close" />} variant="subtle" aria-label="Stop" onClick={() => { abort.current?.abort(); speaker.current?.stop(); setBusy(false); setPhase("paused"); }} />
          : <IconButton icon={<Icon kind="arrow-up" />} aria-label="Send" disabled={!draft.trim()} onClick={() => void ask(draft, false)} />}
      </div>
    </div>
  );
}
