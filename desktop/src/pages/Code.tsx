// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Code: agentic coding in this window, on the box's own model, like Chat. Ask
// for a change in a project; the agent (Claude Code, driven by the box in its
// structured mode, `workspace.rs`) reads, edits and runs things, each step
// shown here, and asks before it edits or runs anything (Allow, Always, Deny).
// By default it works in a sandbox: it sees only the project folder, has no
// internet, and can reach nothing on the box but the model. Changes shows the
// diff, with Commit and Discard. Conversations carry on after restarts.
//
// The thread folds each run of steps into one line ("Read 4 files · edited 2 ·
// ran 3 commands") that opens into the steps; what it asks you sits in a bar
// above the composer, so it can't scroll away; its plan (TodoWrite) is pinned
// at the top; and the status line under the thread always says what it's doing.

import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Icon } from "@tenstorrent/vesper/icons";
import { Switch } from "@tenstorrent/vesper/switch";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { ago, ApiError, del, get, post } from "../api";
import { Eyebrow, MoreMenu, toastError, toastOk, usePoll, writeClipboard } from "../ui";
import { ask, askText } from "../dialog";
import { reuse } from "../render";
import { navigate } from "../App";
import { FolderBrowser } from "./Terminals";
import { CodeModelButton, SpeedStrip } from "./CodeSpeed";

interface Sess { id: string; folder: string; title: string; created: number; last: number; sandbox: boolean; policy: string; running: boolean; busy: boolean; waiting: number; changes: number; always: string[]; think: boolean }
interface Activity { seconds: number; phase: string; tool: string | null; tokens: number; tokens_per_s: number; context_tokens: number; session?: string | null }
interface Overview {
  sessions: Sess[]; claude: boolean; home: string;
  check: { ok: boolean; level: string; why: string; model?: string };
  sandbox: { ready: boolean; step: string | null; error: string | null };
}
interface Ev { seq: number; ts: number; kind: string; [k: string]: unknown }
interface Todo { content: string; status: string; activeForm?: string }

const POLICIES = [
  { id: "ask", label: "Ask before edits and commands", short: "Asks first", hint: "It reads freely; every edit and command waits for you." },
  { id: "edits", label: "Edit freely, ask before commands", short: "Edits freely", hint: "Edits go ahead (review them in Changes); commands wait for you." },
  { id: "auto", label: "Don't ask (sandbox only)", short: "Doesn't ask", hint: "It does everything itself, inside the sandbox. Review Changes before you commit." },
];

const EDITS = new Set(["Edit", "MultiEdit", "Write", "NotebookEdit"]);
const rel = (p: unknown) => String(p ?? "").replace(/^\/work\//, "");
/** "0:42", "3:05", "1:02:10": a clock that moves every second. */
const clock = (s: number) => {
  const t = Math.max(0, Math.round(s));
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), sec = t % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
};
const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** What you'd typed in each session, kept while you look at another. */
const drafts = new Map<string, string>();

export function Code() {
  const o = usePoll(() => get<Overview>("/code"), 4000);
  const d = o.data;
  const sessions = d?.sessions ?? [];
  const [picked, setPicked] = useState<string | null>(() => location.hash.replace("#", "") || null);
  const [creating, setCreating] = useState(false);
  const sel = creating ? null : sessions.find((s) => s.id === picked) ?? sessions[0] ?? null;
  useEffect(() => { if (sel) history.replaceState(null, "", `/app/code#${sel.id}`); }, [sel?.id]); // eslint-disable-line react-hooks/exhaustive-deps
  const forget = (id: string) => {
    // Off the list at once, so the pane doesn't fall back to the session just removed.
    o.setData((cur) => cur && { ...cur, sessions: cur.sessions.filter((s) => s.id !== id) });
    setPicked(null);
    void o.refresh();
  };

  return (
    <div className="chat code-page">
      <aside className="chat-list">
        <Button size="sm" variant="tertiary" iconLeft={<Icon kind="add" />} onClick={() => setCreating(true)}>New session</Button>
        <div style={{ height: 10 }} />
        {sessions.map((s) => (
          <button key={s.id} className={`code-sess ${sel?.id === s.id ? "on" : ""}`} onClick={() => { setPicked(s.id); setCreating(false); }}>
            <span className="code-sess-top">
              <span className={`run-dot ${s.waiting ? "needs" : s.busy ? "live" : "done"}`} aria-hidden="true" />
              <b className="code-list-title">{s.title}</b>
              {s.sandbox && <span className="code-sess-sandbox" title="Sandboxed" aria-label="Sandboxed">🔒</span>}
            </span>
            <span className="code-sess-sub">
              {s.waiting ? <b className="code-needs">Needs your OK</b> : s.busy ? "Working…" : ago(s.last || s.created)}
              {s.changes ? <span className="code-sess-changes">{s.changes} changed</span> : null}
            </span>
          </button>
        ))}
        <div className="grow" />
        <button className="link-button" style={{ fontSize: 12 }} onClick={() => navigate("connect")}>Claude Code, OpenCode and others in their own terminal: Connect → Code</button>
      </aside>
      <section className="chat-main">
        {!d ? <div className="chat-thread">{o.error ? <Admonition variant="warning" subtle size="sm">Couldn't reach the box: {o.error}. Retrying…</Admonition> : <span className="muted">Loading…</span>}</div>
          : creating || !sel ? <NewSession d={d} onCreated={(s) => { setPicked(s.id); setCreating(false); void o.refresh(); }} onCancel={sessions.length ? () => setCreating(false) : undefined} />
          : <SessionPane key={sel.id} s={sel} check={d.check} onChanged={o.refresh} onForgotten={() => forget(sel.id)} />}
      </section>
    </div>
  );
}

// ---------------------------------------------------------------- new session

function NewSession({ d, onCreated, onCancel }: { d: Overview; onCreated: (s: Sess) => void; onCancel?: () => void }) {
  const recent = [...new Set(d.sessions.map((s) => s.folder))].slice(0, 6);
  const [folder, setFolder] = useState(recent[0] ?? "");
  const [sandbox, setSandbox] = useState(true);
  const [policy, setPolicy] = useState("ask");
  const [think, setThink] = useState(false);
  const [busy, setBusy] = useState(false);
  const [projects, setProjects] = useState<string[]>([]);
  const browse = useSheet();
  const create = async () => {
    if (busy) return;
    setBusy(true);
    setProjects([]);
    try { const r = await post<{ session: Sess }>("/code/sessions", { folder, sandbox, think, policy: !sandbox && policy === "auto" ? "ask" : policy }); onCreated(r.session); }
    catch (e) {
      const list = e instanceof ApiError ? (e.body as { projects?: string[] })?.projects : undefined;
      if (list?.length) setProjects(list); else toastError(e);
    } finally { setBusy(false); }
  };
  return (
    <div className="chat-scroll"><div className="chat-thread code-new" style={{ gap: 20 }}>
      <div className="row-between" style={{ alignItems: "baseline" }}>
        <div className="stack" style={{ gap: 4 }}>
          <Typography variant="heading-md">Code with the box's model</Typography>
          <span className="muted" style={{ fontSize: 13 }}>Pick a project, then ask for what you want: a fix, a feature, tests, an explanation. It reads, edits and runs things in the project, step by step, and asks before it changes anything.</span>
        </div>
        {onCancel && <Button size="xs" variant="ghost" onClick={onCancel}>Cancel</Button>}
      </div>
      {!d.claude && <Admonition variant="warning" subtle size="sm">Code's agent engine isn't installed on the box. Install it with <code>npm i -g @anthropic-ai/claude-code</code>.</Admonition>}
      {d.check.level !== "ok" && <Admonition variant={d.check.level === "block" ? "warning" : "info"} subtle size="sm">{d.check.why}</Admonition>}
      <div className="code-new-model muted">Runs on <CodeModelButton model={d.check.model} />, the model the box is serving.</div>

      <div className="code-step">
        <span className="code-step-num">1</span>
        <div className="stack" style={{ gap: 8, flex: 1, minWidth: 0 }}>
          <b>Project</b>
          <div className="row wrap" style={{ gap: 6 }}>
            {recent.map((f) => <button key={f} className={`example-chip ${folder === f ? "on" : ""}`} onClick={() => setFolder(f)}>{f.split("/").pop()}</button>)}
            <Button size="xs" variant="subtle" onClick={browse.open}>{recent.length ? "Another folder…" : "Choose a folder…"}</Button>
          </div>
          {folder && <span className="muted mono" style={{ fontSize: 12, overflowWrap: "anywhere" }}>{folder}</span>}
          {projects.length > 0 && (
            <Admonition variant="info" subtle size="sm">
              <div className="stack" style={{ gap: 6 }}>
                <span>That folder holds several projects. The agent works much better (and faster) in one: pick it.</span>
                <div className="row wrap" style={{ gap: 6 }}>{projects.map((p) => <button key={p} className="example-chip" onClick={() => { setFolder(p); setProjects([]); }}>{p.split("/").pop()}</button>)}</div>
              </div>
            </Admonition>
          )}
        </div>
      </div>

      <div className="code-step">
        <span className="code-step-num">2</span>
        <div className="row-between" style={{ gap: 12, flex: 1, minWidth: 0 }}>
          <div className="stack" style={{ gap: 2 }}>
            <b>🔒 Work in a sandbox</b>
            <span className="muted" style={{ fontSize: 13 }}>
              {sandbox ? "Recommended. It can see only this project folder, has no internet, and can reach nothing on the box except the model. It can't touch the chips, your other files, keys or settings."
                : "It runs on the box as you, like a terminal: it can reach your other files and the network, so it asks before every command."}
            </span>
            {sandbox && !d.sandbox.ready && <span className="muted" style={{ fontSize: 12 }}>{d.sandbox.step ?? "The first session sets the sandbox up: a one-time download of about 370 MB."}</span>}
            {d.sandbox.error && <span style={{ fontSize: 12, color: "var(--vesper-text-error)" }}>{d.sandbox.error}</span>}
          </div>
          <Switch checked={sandbox} onChange={(e) => { setSandbox(e.target.checked); if (!e.target.checked && policy === "auto") setPolicy("ask"); }} aria-label="Work in a sandbox" />
        </div>
      </div>

      <div className="code-step">
        <span className="code-step-num">3</span>
        <div className="stack" style={{ gap: 6, flex: 1, minWidth: 0 }}>
          <b id="code-policy">How much may it do without asking?</b>
          <div className="agent-picks" role="radiogroup" aria-labelledby="code-policy">
            {POLICIES.map((p) => (
              <button key={p.id} role="radio" aria-checked={policy === p.id} disabled={p.id === "auto" && !sandbox} className={`agent-pick ${policy === p.id ? "on" : ""}`} onClick={() => setPolicy(p.id)}>
                <span className="agent-pick-name">{p.label}</span><span className="muted" style={{ fontSize: 12 }}>{p.hint}</span>
              </button>
            ))}
          </div>
        </div>
      </div>

      <div className="code-step">
        <span className="code-step-num">4</span>
        <div className="row-between" style={{ gap: 12, flex: 1, minWidth: 0 }}>
          <div className="stack" style={{ gap: 2 }}>
            <b>Think before acting</b>
            <span className="muted" style={{ fontSize: 13 }}>The model reasons through each step first: better on tricky problems, several times slower. Off is right for most tasks; you can change it later.</span>
          </div>
          <Switch checked={think} onChange={(e) => setThink(e.target.checked)} aria-label="Think before acting" />
        </div>
      </div>
      <div><Button disabled={busy || !folder || !d.claude} onClick={create}>{busy ? "Starting…" : "Start"}</Button></div>
      <Sheet ref={browse.ref} popover title="Choose a project folder" description="A folder in your home folder. A git repository works best: every change can be reviewed and undone.">
        <FolderBrowser home={d.home} onPick={(p) => { setFolder(p); browse.close(); }} />
      </Sheet>
    </div></div>
  );
}

// ---------------------------------------------------------------- the session's events

interface Meta { session: Sess; pending: string[]; activity: Activity[] }

/** A session's events as they arrive. One request at a time (never two with the
 *  same cursor), quicker while it works, paused while the window is hidden. */
function useSession(id: string, hurry: boolean) {
  const [events, setEvents] = useState<Ev[]>([]);
  const [meta, setMeta] = useState<Meta | null>(null);
  const [trouble, setTrouble] = useState<string | null>(null);
  const [gone, setGone] = useState(false);
  const last = useRef(0);
  const hurryRef = useRef(hurry);
  hurryRef.current = hurry;
  const kick = useRef<() => void>(() => {});
  useEffect(() => {
    let alive = true;
    let timer = 0;
    let failures = 0;
    let working = false;
    last.current = 0;
    setEvents([]);
    setMeta(null);
    setGone(false);
    const tick = async () => {
      window.clearTimeout(timer);
      {
        try {
          const r = await get<{ session: Sess; events: Ev[]; pending: string[]; activity: Activity[] }>(`/code/sessions/${id}?after=${last.current}`);
          if (!alive) return;
          const fresh = r.events.filter((e) => e.seq > last.current);
          if (fresh.length) {
            last.current = fresh[fresh.length - 1].seq;
            setEvents((cur) => compact([...cur, ...fresh]));
          }
          setMeta({ session: r.session, pending: r.pending ?? [], activity: r.activity ?? [] });
          working = r.session.busy;
          failures = 0;
          setTrouble(null);
        } catch (e) {
          if (!alive) return;
          if (e instanceof ApiError && e.status === 404) { setGone(true); return; }
          if (++failures >= 3) setTrouble((e as Error).message);
        }
      }
      if (!alive) return;
      // Slowly while the window is hidden; it catches up the moment it's shown.
      timer = window.setTimeout(tick, document.hidden ? 15000 : hurryRef.current || working ? 800 : 3000);
    };
    kick.current = () => { void tick(); };
    void tick();
    const wake = () => { if (!document.hidden) void tick(); };
    document.addEventListener("visibilitychange", wake);
    return () => { alive = false; window.clearTimeout(timer); document.removeEventListener("visibilitychange", wake); };
  }, [id]);
  // Ask again now (after sending, approving or stopping), rather than at the next tick.
  const refresh = useCallback(() => kick.current(), []);
  return { events, meta, trouble, gone, refresh };
}

/** Streamed pieces are kept only until the finished text arrives: a long session
 *  shouldn't carry every token it ever streamed. */
function compact(events: Ev[]): Ev[] {
  let boundary = -1;
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i].kind !== "delta" && events[i].kind !== "thinking" && events[i].kind !== "preparing" && events[i].kind !== "step") { boundary = i; break; }
  }
  if (boundary < 0) return events;
  let dropped = false;
  const out = events.filter((e, i) => {
    const drop = i < boundary && e.kind === "delta";
    dropped ||= drop;
    return !drop;
  });
  return dropped ? out : events;
}

// ---------------------------------------------------------------- events → thread

interface Tool { id: string; name: string; input: Record<string, unknown>; ts: number; result?: { output: string; is_error: boolean; ts: number; unchanged?: boolean } }
interface Approval { request_id: string; tool: string; input: Record<string, unknown>; summary: string; rule?: string; decided?: { allow: boolean; always: boolean } }
interface Thought { text: string; from: number; to: number }

type Item =
  /** `mid`: sent while it was working; it reads it at its next step ("pending", then "read"). */
  | { t: "user"; key: string; text: string; mid?: "pending" | "read" | "lost" }
  | { t: "text"; key: string; text: string }
  | { t: "steps"; key: string; tools: Tool[]; thoughts: Thought[] }
  | { t: "decided"; key: string; a: Approval }
  | { t: "note"; key: string; text: string; bad?: boolean }
  | { t: "done"; key: string; ok: boolean; stopped: boolean; ms: number; turns: number; files: string[]; why: string }
  | { t: "start"; key: string; sandbox: boolean };

interface Thread {
  items: Item[];
  streaming: string;
  preparing: string | null;
  approvals: Approval[];
  plan: Todo[] | null;
  /** Thinking going on right now. */
  thinking: Thought | null;
  turnStarted: number | null;
}

function build(events: Ev[]): Thread {
  const items: Item[] = [];
  const tools = new Map<string, Tool>();
  const approvals = new Map<string, Approval>();
  // Sent mid-turn and not yet read (the agent reads them at its next step).
  let unread: (Item & { t: "user" })[] = [];
  let streaming = "";
  let preparing: string | null = null;
  let plan: Todo[] | null = null;
  let thought: Thought | null = null;
  let group: (Item & { t: "steps" }) | null = null;
  let inTurn = false;
  let turnStarted: number | null = null;
  let turnTools: Tool[] = [];
  let interrupted = false;
  const edited = () => [...new Set(turnTools.filter((t) => EDITS.has(t.name) && t.result && !t.result.is_error).map((t) => rel(t.input.file_path ?? t.input.notebook_path)))];
  const steps = (key: string) => {
    if (!group) { group = { t: "steps", key, tools: [], thoughts: [] }; items.push(group); }
    return group;
  };
  const beginTurn = (e: Ev, text: string) => {
    items.push({ t: "user", key: `u${e.seq}`, text });
    inTurn = true;
    turnStarted = e.ts;
    turnTools = [];
    interrupted = false;
    group = null;
  };
  for (const e of events) {
    if (e.kind !== "thinking") thought = null;
    switch (e.kind) {
      case "start": items.push({ t: "start", key: `s${e.seq}`, sandbox: !!e.sandbox }); group = null; break;
      case "user":
        // Sent while it was working: Claude Code reads it at its next step, inside the
        // running turn (it doesn't wait for the turn to end). Shown where it was sent.
        if ((e.mid_turn || e.queued) && inTurn) {
          const it: Item & { t: "user" } = { t: "user", key: `u${e.seq}`, text: String(e.text), mid: "pending" };
          items.push(it);
          unread.push(it);
          group = null;
        } else {
          beginTurn(e, String(e.text));
          streaming = "";
        }
        break;
      case "delta": streaming += String(e.text); break;
      case "thinking":
        if (!thought) { thought = { text: "", from: e.ts, to: e.ts }; steps(`g${e.seq}`).thoughts.push(thought); }
        thought.text += String(e.text);
        thought.to = e.ts;
        break;
      case "preparing": preparing = String(e.name); break;
      case "step":
        preparing = null;
        // Its next step: what was sent mid-turn is in front of it now.
        for (const it of unread) it.mid = "read";
        unread = [];
        if (!inTurn) { inTurn = true; turnStarted = e.ts; turnTools = []; interrupted = false; }
        break;
      case "text": items.push({ t: "text", key: `t${e.seq}`, text: String(e.text) }); streaming = ""; group = null; break;
      case "tool": {
        preparing = null;
        streaming = "";
        const it: Tool = { id: String(e.id), name: String(e.name), input: (e.input ?? {}) as Record<string, unknown>, ts: e.ts };
        tools.set(it.id, it);
        turnTools.push(it);
        steps(`g${e.seq}`).tools.push(it);
        if (it.name === "TodoWrite" && Array.isArray(it.input.todos)) plan = it.input.todos as Todo[];
        break;
      }
      case "tool_result": {
        const it = tools.get(String(e.id));
        if (!it) break;
        const output = String(e.output ?? "");
        // An edit that would change nothing isn't a failure: the agent just moves on.
        const unchanged = !!e.is_error && /No changes to make/.test(output);
        it.result = { output, is_error: !!e.is_error && !unchanged, ts: e.ts, unchanged };
        break;
      }
      case "approval": {
        const a: Approval = { request_id: String(e.request_id), tool: String(e.tool), input: (e.input ?? {}) as Record<string, unknown>, summary: String(e.summary), rule: e.rule ? String(e.rule) : undefined };
        approvals.set(a.request_id, a);
        break;
      }
      case "decided": {
        const a = approvals.get(String(e.request_id));
        // Expired (stopped, or the turn moved on): no longer asked, nothing to show.
        if (a) { a.decided = { allow: !!e.allow, always: !!e.always }; if (!e.expired) { items.push({ t: "decided", key: `d${e.seq}`, a }); group = null; } }
        break;
      }
      case "notice":
        if (e.interrupt || String(e.text).startsWith("Interrupted")) interrupted = true;
        items.push({ t: "note", key: `n${e.seq}`, text: String(e.text) });
        group = null;
        break;
      case "error": items.push({ t: "note", key: `e${e.seq}`, text: String(e.text), bad: true }); streaming = ""; preparing = null; group = null; break;
      case "idle": {
        // Claude Code exited: whatever turn was going is over, finished or not.
        streaming = ""; preparing = null; group = null; inTurn = false; turnStarted = null;
        // It exited before reading them: they never reached it.
        for (const it of unread) it.mid = "lost";
        unread = [];
        break;
      }
      case "done": {
        const ok = !!e.ok;
        items.push({
          t: "done", key: `f${e.seq}`, ok, stopped: !ok && (interrupted || !!e.stopped), ms: Number(e.duration_ms ?? 0), turns: Number(e.turns ?? 0),
          files: Array.isArray(e.files) ? e.files.map(String) : edited(),
          why: ok ? "" : String(e.text ?? ""),
        });
        // Anything sent mid-turn and not read yet, it takes as its next turn.
        streaming = ""; preparing = null; group = null; inTurn = false; turnStarted = null;
        break;
      }
    }
  }
  const waiting = [...approvals.values()].filter((a) => !a.decided);
  return { items, streaming, preparing, approvals: waiting, plan, thinking: thought, turnStarted };
}

// ---------------------------------------------------------------- a session

const PHASE: Record<string, string> = { reading: "Reading the conversation", thinking: "Thinking", writing: "Writing", calling: "Preparing a step" };

function SessionPane({ s, check, onChanged, onForgotten }: { s: Sess; check: Overview["check"]; onChanged: () => void; onForgotten: () => void }) {
  const [text, setTextState] = useState(() => drafts.get(s.id) ?? "");
  const setText = (t: string) => { drafts.set(s.id, t); setTextState(t); };
  const [sending, setSending] = useState(false);
  const [stopping, setStopping] = useState(false);
  const { events, meta, trouble, gone, refresh } = useSession(s.id, sending || stopping);
  const live = meta?.session ?? s;
  // Rebuilt at every poll; the items that didn't change keep their objects, so
  // their (memoized) views don't render again.
  const shown = useRef<Item[]>([]);
  const thread = useMemo(() => {
    const t = build(events);
    t.items = reuse(shown.current, t.items);
    shown.current = t.items;
    return t;
  }, [events]);
  const working = live.busy || sending;
  useEffect(() => { if (!live.busy) setStopping(false); }, [live.busy]);
  useEffect(() => { if (gone) onForgotten(); }, [gone]); // eslint-disable-line react-hooks/exhaustive-deps

  // Changes: mounted only while open (it runs git status and a diff).
  const changes = useSheet();
  const [changesOpen, setChangesOpen] = useState(false);
  const [changesFile, setChangesFile] = useState<string | null>(null);
  const openChanges = (file: string | null = null) => { setChangesFile(file); setChangesOpen(true); changes.open(); };
  const openRef = useRef(openChanges);
  openRef.current = openChanges;
  const onFile = useCallback((file: string) => openRef.current(file), []);

  // Scrolling: follow new output only while you're at the bottom.
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const [away, setAway] = useState(false);
  const onScroll = () => {
    const el = scroller.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    pinned.current = atBottom;
    setAway(!atBottom);
  };
  const toBottom = (smooth = false) => { const el = scroller.current; if (el) el.scrollTo({ top: el.scrollHeight, behavior: smooth ? "smooth" : "auto" }); };
  useLayoutEffect(() => { if (pinned.current) toBottom(); }, [events.length, thread.streaming.length, meta?.activity]);

  // Mid-turn, a message joins the running turn at its next step; with `interrupt`
  // ("Stop & send") the turn stops first and the message starts a new one.
  const send = async (interrupt = false) => {
    const t = text.trim();
    if (!t || sending) return;
    setSending(true);
    if (interrupt) setStopping(true);
    setText("");
    pinned.current = true;
    try { await post(`/code/sessions/${s.id}/message`, { text: t, interrupt }); refresh(); onChanged(); }
    catch (e) { setText(t); toastError(e); } finally { setSending(false); }
  };
  const stop = async () => {
    if (stopping) return;
    setStopping(true);
    try { await post(`/code/sessions/${s.id}/interrupt`); refresh(); } catch (e) { setStopping(false); toastError(e); }
  };
  const remove = async () => {
    if (!(await ask("Forget this session?", { body: "The conversation is removed from TT Station. The project's files stay as they are.", confirm: "Forget it", danger: true }))) return;
    try { await del(`/code/sessions/${s.id}`); drafts.delete(s.id); onForgotten(); } catch (e) { toastError(e); }
  };
  const rename = async () => {
    const t = await askText("Rename this session", live.title);
    if (t && t.trim() && t.trim() !== live.title) {
      try { await post(`/code/sessions/${s.id}/rename`, { title: t.trim() }); toastOk('Session renamed.'); onChanged(); refresh(); } catch (e) { toastError(e); }
    }
  };
  const forgetRule = async (rule: string) => {
    try { await post(`/code/sessions/${s.id}/forget`, { rule }); onChanged(); refresh(); } catch (e) { toastError(e); }
  };

  const lastDone = [...thread.items].reverse().find((i) => i.t === "done") as (Item & { t: "done" }) | undefined;
  const activity = meta?.activity?.find((a) => a.session === s.id) ?? (meta?.activity?.length === 1 && !meta.activity[0].session ? meta.activity[0] : undefined);
  const policy = POLICIES.find((p) => p.id === live.policy);

  return (
    <>
      <div className="chat-head code-head">
        <div className="code-head-main">
          <button className="code-head-title" onClick={() => void rename()} title="Rename this session">{live.title}</button>
          <div className="code-head-meta">
            <span className="mono" title={live.folder}>{live.folder.replace(/^\/home\/[^/]+/, "~")}</span>
            <span><CodeModelButton model={check.model} /></span>
            {live.sandbox ? <span title="It sees only this folder, with no internet">🔒 Sandbox</span>
              : <span className="warn" title="It runs on the box as you, like a terminal">Runs as you</span>}
            <span title={policy?.label}>{policy?.short}</span>
          </div>
        </div>
        <div className="code-head-actions">
          <Switch size="sm" label="Think" checked={live.think} title="Think before acting: better on tricky problems, much slower"
            onChange={async (e) => { try { await post(`/code/sessions/${s.id}/settings`, { think: e.target.checked }); onChanged(); refresh(); } catch (err) { toastError(err); } }} />
          <Button size="sm" variant="subtle" onClick={() => openChanges()}>
            Changes{live.changes ? <span className="code-changes-count">{live.changes}</span> : ""}
          </Button>
          <MoreMenu items={[
            { label: "Rename", onSelect: () => void rename() },
            { label: "Forget session", danger: true, onSelect: () => void remove() },
          ]} />
        </div>
      </div>
      <SpeedStrip session={s.id} working={working} />
      {trouble && <div className="code-trouble" role="status">Can't reach the box right now ({trouble}). Retrying…</div>}
      <div className="chat-scroll" ref={scroller} onScroll={onScroll}>
        <div className="chat-thread code-thread">
          {thread.plan && thread.plan.length > 0 && <PlanCard todos={thread.plan} />}
          {thread.items.map((it, i) => (
            <ItemView key={it.key} it={it} live={working && i === thread.items.length - 1} onFile={onFile} />
          ))}
          {thread.thinking && working && <LiveThought t={thread.thinking} />}
          {thread.streaming && <div className="msg-assistant"><div className="msg-body markdown selectable"><Reply text={thread.streaming} /></div></div>}
          {thread.items.length <= 1 && !working && (
            <div className="code-empty">
              <div className="code-empty-mark" aria-hidden="true">⌘</div>
              <b>What should it do in {live.title}?</b>
              <span className="muted">Ask in plain words. It reads the project, makes the change, and shows you every step before it runs anything.</span>
              <div className="code-empty-examples">
                {["Explain how this project is organized and how to run it.", "Run the tests and fix whatever fails.", "Add a README section on how to use this.", "Find and fix any obvious bugs."].map((x) => (
                  <button key={x} className="example-chip" onClick={() => setText(x)}>{x}</button>
                ))}
              </div>
            </div>
          )}
        </div>
      </div>
      {away && <button className="code-jump" onClick={() => { pinned.current = true; toBottom(true); }}>↓ Jump to latest</button>}
      <div className="composer-wrap">
        {thread.approvals.length > 0 && <ApprovalBar key={thread.approvals[0].request_id} a={thread.approvals[0]} more={thread.approvals.length - 1} session={s.id} onDecided={refresh} />}
        <StatusLine working={working} stopping={stopping} waiting={thread.approvals.length > 0} activity={activity} preparing={thread.preparing}
          started={thread.turnStarted} lastDone={lastDone} onFile={openChanges} />
        <Composer text={text} setText={setText} working={working} sending={sending} stopping={stopping} title={live.title}
          onSend={() => send(false)} onSendNow={() => send(true)} onStop={stop} />
        <div className="composer-hint">
          <span>{working ? "Enter adds to what it's doing · Ctrl+Enter stops it and sends · Esc stops" : "Enter to send · Shift+Enter for a new line"}</span>
          <span className="code-rules">
            {live.always.length ? <>Always allowed: {live.always.map((r) => (
              <button key={r} className="code-rule" title={`${ruleTitle(r)}. Click to ask again for this.`} onClick={() => forgetRule(r)}>{ruleLabel(r, 28)} ✕</button>
            ))}</> : live.sandbox ? "Only this folder; no internet" : "Asks before every command"}
          </span>
        </div>
      </div>
      <Sheet ref={changes.ref} popover title={changesFile ? `Changes to ${changesFile.split("/").pop()}` : `Changes in ${live.title}`}
        description={changesFile ? changesFile : "Everything that's different from the last commit, including new files."}
        onClose={() => setChangesOpen(false)}
        onToggle={(e) => { if ((e.nativeEvent as ToggleEvent).newState === "closed") setChangesOpen(false); }}>
        {changesOpen && <ChangesView id={s.id} file={changesFile} onAll={() => setChangesFile(null)} onDone={() => { changes.close(); onChanged(); }} />}
      </Sheet>
    </>
  );
}

/** At most `n` characters, with an ellipsis when cut. */
const clip = (s: string, n: number) => (s.length > n ? `${s.slice(0, n - 1).trimEnd()}…` : s);

/** An "always" rule as people read it (short: a long command is cut, the whole one is its title). */
function ruleLabel(rule: string, max = 40) {
  if (rule === "edits") return "edits";
  if (rule.startsWith("Bash=")) return `“${clip(rule.slice(5), max)}”`;
  if (rule.startsWith("Bash:")) return `${clip(rule.slice(5), max)} …`;
  return clip(rule, max);
}

/** The whole rule, for a tooltip. */
function ruleTitle(rule: string) {
  if (rule.startsWith("Bash=")) return `Always allow exactly: ${rule.slice(5)}`;
  if (rule.startsWith("Bash:")) return `Always allow commands starting with: ${rule.slice(5)}`;
  return `Always allow ${rule}`;
}

// ---------------------------------------------------------------- composer and status

function Composer({ text, setText, working, sending, stopping, title, onSend, onSendNow, onStop }: {
  text: string; setText: (t: string) => void; working: boolean; sending: boolean; stopping: boolean; title: string;
  onSend: () => void; onSendNow: () => void; onStop: () => void;
}) {
  const box = useRef<HTMLTextAreaElement>(null);
  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`;
  }, [text]);
  return (
    <div className="composer">
      <textarea ref={box} rows={1} value={text} aria-label={`Message to the agent in ${title}`}
        placeholder={working ? "It's working. Add to what it's doing, or stop it and send…" : `Ask for a change in ${title}…`}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing && e.keyCode !== 229) {
            e.preventDefault();
            if (working && (e.ctrlKey || e.metaKey)) onSendNow(); else onSend();
          }
          else if (e.key === "Escape" && working) { e.preventDefault(); onStop(); }
        }} />
      {working && !text.trim() && <Button size="sm" variant="subtle" disabled={stopping} onClick={onStop}>{stopping ? "Stopping…" : "Stop"}</Button>}
      {working && text.trim() && <Button size="sm" variant="subtle" disabled={sending || stopping} onClick={onSendNow} title="Stop what it's doing and send this as a new request (Ctrl+Enter)">Stop &amp; send</Button>}
      <Button size="sm" disabled={!text.trim() || sending} onClick={onSend} title={working ? "Add to what it's doing: it reads this at its next step (Enter)" : undefined}>{working ? "Add" : "Send"}</Button>
    </div>
  );
}

function StatusLine({ working, stopping, waiting, activity, preparing, started, lastDone, onFile }: {
  working: boolean; stopping: boolean; waiting: boolean; activity: Activity | undefined; preparing: string | null; started: number | null;
  lastDone: (Item & { t: "done" }) | undefined; onFile: (f: string) => void;
}) {
  const [, tick] = useState(0);
  useEffect(() => {
    if (!working) return;
    const t = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => window.clearInterval(t);
  }, [working]);
  if (waiting) return <div className="code-status waiting" role="status"><span className="code-status-dot" />Waiting for your OK</div>;
  if (stopping) return <div className="code-status" role="status"><span className="code-status-dot live" />Stopping…</div>;
  if (working) {
    const what = activity
      ? activity.phase === "calling" ? `Preparing ${toolVerb(activity.tool ?? preparing)}`
        : activity.phase === "reading" ? `Reading the conversation${activity.context_tokens ? ` (${Math.round(activity.context_tokens / 1000)}K tokens)` : ""}`
        : PHASE[activity.phase] ?? "Working"
      : preparing ? `Preparing ${toolVerb(preparing)}` : "Working";
    const since = started ? Date.now() / 1000 - started : activity?.seconds ?? 0;
    return (
      <div className="code-status" role="status">
        <span className="code-status-dot live" />
        <span className="thinking-text">{what}</span>
        <span className="grow" />
        <span className="muted mono">{activity?.tokens ? `${activity.tokens.toLocaleString()} tokens · ${activity.tokens_per_s} tok/s · ` : ""}{clock(since)}</span>
      </div>
    );
  }
  // Done: the thread's last line says so (and what it edited); nothing to repeat here.
  void lastDone; void onFile;
  return null;
}

function toolVerb(name: string | null) {
  switch (name) {
    case "Edit": case "MultiEdit": return "an edit";
    case "Write": return "a new file";
    case "Bash": return "a command";
    case "Read": return "to read a file";
    case "Grep": case "Glob": return "a search";
    case "TodoWrite": return "its plan";
    default: return name ? `${name}` : "its next step";
  }
}

// ---------------------------------------------------------------- asking

function ApprovalBar({ a, more, session, onDecided }: { a: Approval; more: number; session: string; onDecided: () => void }) {
  const [sent, setSent] = useState(false);
  const allowBtn = useRef<HTMLDivElement>(null);
  useEffect(() => { allowBtn.current?.querySelector("button")?.focus(); }, []);
  const decide = async (allow: boolean, always = false) => {
    if (sent) return;
    setSent(true);
    try { await post(`/code/sessions/${session}/approve`, { request_id: a.request_id, allow, always }); }
    catch (e) {
      // Already answered (a second click, another window): nothing to undo.
      if (!(e instanceof ApiError && e.status === 409)) { setSent(false); toastError(e); return; }
    }
    onDecided();
  };
  const rule = a.rule ?? (EDITS.has(a.tool) ? "edits" : a.tool === "Bash" ? null : a.tool);
  return (
    <div className="code-ask" role="alertdialog" aria-label="The agent is asking"
      onKeyDown={(e) => { if (e.key === "Escape") { e.preventDefault(); void decide(false); } }}>
      <div className="row-between wrap" style={{ gap: 8 }}>
        <b className="code-ask-title">May it {askVerb(a)}?{more > 0 ? <span className="muted"> (+{more} more)</span> : null}</b>
        <div className="row code-ask-actions" style={{ gap: 6 }} ref={allowBtn}>
          <Button size="sm" variant="primary" disabled={sent} onClick={() => decide(true)}>Allow</Button>
          {rule && <Button size="sm" variant="subtle" disabled={sent} onClick={() => decide(true, true)} title={`${ruleTitle(rule)} (for the rest of this session)`}>Always allow {ruleLabel(rule, 32)}</Button>}
          <Button size="sm" variant="ghost" disabled={sent} onClick={() => decide(false)}>Deny</Button>
        </div>
      </div>
      <div className="code-ask-detail"><ToolDetail name={a.tool} input={a.input} /></div>
    </div>
  );
}

function askVerb(a: Approval) {
  const i = a.input;
  switch (a.tool) {
    case "Bash": return "run this command";
    case "Edit": case "MultiEdit": return `edit ${rel(i.file_path)}`;
    case "Write": return `write ${rel(i.file_path)}`;
    case "NotebookEdit": return `edit ${rel(i.notebook_path)}`;
    case "WebFetch": return `fetch ${String(i.url ?? "a page")}`;
    default: return a.summary ? a.summary.charAt(0).toLowerCase() + a.summary.slice(1) : `use ${a.tool}`;
  }
}

// ---------------------------------------------------------------- the thread's pieces

const ItemView = memo(function ItemView({ it, live, onFile }: { it: Item; live: boolean; onFile: (f: string) => void }) {
  switch (it.t) {
    case "start":
      return <div className="code-note">{it.sandbox ? "🔒 Working in a sandbox: this project folder only, no internet, nothing on the box but the model." : "Working on the box as you. It asks before every command."}</div>;
    case "user":
      return (
        <div className={`msg-user ${it.mid ? `mid ${it.mid}` : ""}`}>
          {it.text}
          {it.mid && <span className="code-mid-tag" title={it.mid === "pending" ? "Sent while it was working: it reads this at its next step, without stopping." : undefined}>
            {it.mid === "pending" ? "Sent mid-turn · it reads this at its next step" : it.mid === "read" ? "Read mid-turn" : "Didn't reach it (it stopped): send it again"}
          </span>}
        </div>
      );
    case "text":
      return <div className="msg-assistant"><div className="msg-body markdown selectable"><Reply text={it.text} /><Copy text={it.text} className="code-copy-reply" /></div></div>;
    case "steps":
      return <Steps g={it} live={live} />;
    case "decided":
      return <div className={`code-note ${it.a.decided?.allow ? "" : "bad"}`}>{it.a.decided?.allow ? (it.a.decided.always ? "Always allowed" : "Allowed") : "Declined"}: {it.a.summary}</div>;
    case "note":
      return <div className={`code-note ${it.bad ? "bad" : ""}`}>{it.text}</div>;
    case "done":
      return (
        <div className={`code-turn-end ${it.ok ? "ok" : it.stopped ? "stopped" : "bad"}`}>
          <span className="code-turn-icon">{it.ok ? "✓" : it.stopped ? "⏸" : "✗"}</span>
          <span>{it.ok ? "Done" : it.stopped ? "Stopped" : "Didn't finish"}{it.ms ? ` in ${clock(it.ms / 1000)}` : ""}{it.turns ? ` · ${plural(it.turns, "step")}` : ""}</span>
          {it.files.length > 0 && <span className="code-files">· edited {it.files.map((f) => <button key={f} className="code-file" title={f} onClick={() => onFile(f)}>{f.split("/").pop()}</button>)}</span>}
          {!it.ok && !it.stopped && it.why && <span className="muted code-why">{it.why}</span>}
        </div>
      );
  }
});

/** A run of steps, folded to one line once it's over. */
function Steps({ g, live }: { g: Item & { t: "steps" }; live: boolean }) {
  const [open, setOpen] = useState<boolean | null>(null);
  const failed = g.tools.filter((t) => t.result?.is_error).length;
  const shown = open ?? (live || failed > 0);
  const running = g.tools.some((t) => !t.result);
  return (
    <div className={`code-steps ${shown ? "open" : ""}`}>
      <button className="code-steps-head" onClick={() => setOpen(!shown)} aria-expanded={shown}>
        <span className={`code-step-state ${running && live ? "running" : failed ? "bad" : "ok"}`} aria-hidden="true">{running && live ? "" : failed ? "!" : "✓"}</span>
        <span className="grow">{summarize(g)}</span>
        {failed > 0 && <span className="code-bad-text">{plural(failed, "step")} didn't work</span>}
        <span className="muted code-chev">{shown ? "▾" : "▸"}</span>
      </button>
      {shown && (
        <div className="code-steps-body">
          {g.thoughts.map((t, i) => <Thinking key={`th${i}`} t={t} />)}
          {g.tools.map((t) => <ToolCard key={t.id} t={t} live={live} />)}
        </div>
      )}
    </div>
  );
}

/** "Read 4 files · edited 2 · ran 3 commands · searched twice" */
function summarize(g: Item & { t: "steps" }) {
  const n = (names: string[]) => g.tools.filter((t) => names.includes(t.name)).length;
  const files = (names: string[]) => new Set(g.tools.filter((t) => names.includes(t.name)).map((t) => rel(t.input.file_path))).size;
  const parts: string[] = [];
  if (g.thoughts.length) parts.push("Thought it through");
  const read = files(["Read"]);
  if (read) parts.push(`read ${plural(read, "file")}`);
  const edited = files(["Edit", "MultiEdit", "Write", "NotebookEdit"]);
  if (edited) parts.push(`edited ${plural(edited, "file")}`);
  const ran = n(["Bash"]);
  if (ran) parts.push(`ran ${plural(ran, "command")}`);
  const searched = n(["Grep", "Glob", "LS"]);
  if (searched) parts.push(`searched ${searched === 1 ? "once" : searched === 2 ? "twice" : `${searched} times`}`);
  if (n(["TodoWrite"])) parts.push("updated its plan");
  const other = g.tools.length - n(["Read", "Edit", "MultiEdit", "Write", "NotebookEdit", "Bash", "Grep", "Glob", "LS", "TodoWrite"]);
  if (other) parts.push(plural(other, "other step"));
  const s = parts.join(" · ") || "Working";
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function Thinking({ t }: { t: Thought }) {
  const secs = Math.max(1, Math.round(t.to - t.from));
  return <details className="code-thinking"><summary>Thought for {clock(secs)}</summary><div className="muted selectable">{t.text}</div></details>;
}

function LiveThought({ t }: { t: Thought }) {
  return (
    <div className="code-thinking live" aria-live="polite">
      <span className="thinking-text">Thinking…</span>
      <div className="muted code-thought-tail">{t.text.slice(-600)}</div>
    </div>
  );
}

const TOOL_ICON: Record<string, string> = { Read: "📄", Edit: "✏️", MultiEdit: "✏️", Write: "📝", NotebookEdit: "📓", Bash: "›_", Glob: "🔎", Grep: "🔎", LS: "📁", TodoWrite: "☑️", WebFetch: "🌐", WebSearch: "🌐", Task: "🧩" };

function toolTitle(name: string, input: Record<string, unknown>) {
  switch (name) {
    case "Read": return `Read ${rel(input.file_path)}`;
    case "Edit": return `Edited ${rel(input.file_path)}`;
    case "MultiEdit": return `Edited ${rel(input.file_path)} (${plural(Array.isArray(input.edits) ? input.edits.length : 0, "change")})`;
    case "Write": return `Wrote ${rel(input.file_path)}`;
    case "NotebookEdit": return `Edited ${rel(input.notebook_path)}`;
    case "Bash": return String(input.description ?? input.command ?? "Ran a command");
    case "Glob": return `Looked for ${String(input.pattern ?? "files")}`;
    case "Grep": return `Searched for “${String(input.pattern ?? "")}”`;
    case "LS": return `Listed ${rel(input.path) || "the folder"}`;
    case "TodoWrite": return "Updated its plan";
    case "WebFetch": return `Fetched ${String(input.url ?? "a page")}`;
    default: return name;
  }
}

/** One line of what came back, for the folded card. */
function preview(t: Tool) {
  if (!t.result) return "";
  if (t.result.unchanged) return "no change needed";
  const out = t.result.output.trim();
  if (t.result.is_error) return out.split("\n")[0].replace(/<\/?tool_use_error>/g, "").slice(0, 120);
  switch (t.name) {
    case "Read": { const lines = out ? out.split("\n").length : 0; return lines ? `${plural(lines, "line")}` : ""; }
    case "Grep": case "Glob": { const lines = out ? out.split("\n").filter(Boolean).length : 0; return out.startsWith("No ") ? "nothing found" : plural(lines, "result"); }
    case "Bash": { const last = out.split("\n").filter(Boolean).pop() ?? ""; return last.slice(0, 120); }
    default: return "";
  }
}

const OUT_LINES = 120;

function ToolCard({ t, live }: { t: Tool; live: boolean }) {
  const editing = EDITS.has(t.name) || t.name === "TodoWrite";
  const [open, setOpen] = useState(editing || !!t.result?.is_error);
  const [all, setAll] = useState(false);
  const out = t.result?.output ?? "";
  const lines = out.split("\n");
  const secs = t.result ? Math.max(0, t.result.ts - t.ts) : null;
  const state = !t.result ? (live ? "running" : "ok") : t.result.is_error ? "bad" : "ok";
  return (
    <div className={`code-tool ${t.result?.is_error ? "bad" : ""}`}>
      <button className="code-tool-head" onClick={() => setOpen((v) => !v)} aria-expanded={open}>
        <span className={`code-step-state small ${state}`} aria-hidden="true">{state === "running" ? "" : state === "bad" ? "!" : ""}</span>
        <span className="code-tool-icon" aria-hidden="true">{TOOL_ICON[t.name] ?? "•"}</span>
        <span className="code-tool-title">{toolTitle(t.name, t.input)}</span>
        <span className="muted code-tool-preview">{preview(t)}</span>
        {secs != null && secs >= 2 && <span className="muted mono" style={{ fontSize: 11 }}>{clock(secs)}</span>}
        <span className="muted code-chev">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <div className="code-tool-body">
          <ToolDetail name={t.name} input={t.input} />
          {out && (!EDITS.has(t.name) || t.result?.is_error) && t.name !== "TodoWrite" && (
            <div className="code-out-wrap">
              <Copy text={out} />
              <pre className="code-out">{all || lines.length <= OUT_LINES ? out : lines.slice(0, OUT_LINES).join("\n")}</pre>
              {!all && lines.length > OUT_LINES && <button className="link-button code-more" onClick={() => setAll(true)}>Show all {lines.length.toLocaleString()} lines</button>}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

const DIFF_LINES = 80;

function DiffLines({ lines }: { lines: string[] }) {
  const [all, setAll] = useState(false);
  const shown = all ? lines : lines.slice(0, DIFF_LINES);
  return (
    <>
      <pre className="diff">{shown.map((l, i) => <span key={i} className={l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : l.startsWith("@@") ? "hunk" : ""}>{l}{"\n"}</span>)}</pre>
      {!all && lines.length > DIFF_LINES && <button className="link-button code-more" onClick={() => setAll(true)}>Show all {lines.length.toLocaleString()} lines</button>}
    </>
  );
}

function hunk(oldText: unknown, newText: unknown) {
  return [...String(oldText ?? "").split("\n").map((l) => `-${l}`), ...String(newText ?? "").split("\n").map((l) => `+${l}`)];
}

function ToolDetail({ name, input }: { name: string; input: Record<string, unknown> }) {
  switch (name) {
    case "Bash": return <div className="code-out-wrap"><Copy text={String(input.command ?? "")} /><pre className="code-out cmd">$ {String(input.command ?? "")}</pre></div>;
    case "Edit": return <DiffLines lines={hunk(input.old_string, input.new_string)} />;
    case "MultiEdit": {
      const edits = Array.isArray(input.edits) ? (input.edits as Record<string, unknown>[]) : [];
      return <DiffLines lines={edits.flatMap((e, i) => [`@@ change ${i + 1} of ${edits.length}`, ...hunk(e.old_string, e.new_string)])} />;
    }
    case "Write": return <DiffLines lines={String(input.content ?? "").split("\n").map((l) => `+${l}`)} />;
    case "NotebookEdit": return <DiffLines lines={String(input.new_source ?? "").split("\n").map((l) => `+${l}`)} />;
    case "TodoWrite": return Array.isArray(input.todos) ? <TodoList todos={input.todos as Todo[]} /> : null;
    case "Read": case "Glob": case "Grep": case "LS": return null;
    default: return <pre className="code-out">{JSON.stringify(input, null, 2)}</pre>;
  }
}

function TodoList({ todos }: { todos: Todo[] }) {
  return (
    <ul className="code-todos">
      {todos.map((t, i) => (
        <li key={i} className={t.status}>
          <span className="code-todo-box" aria-hidden="true">{t.status === "completed" ? "✓" : t.status === "in_progress" ? "▸" : ""}</span>
          <span>{t.status === "in_progress" && t.activeForm ? t.activeForm : t.content}</span>
          <span className="visually-hidden">{t.status.replace("_", " ")}</span>
        </li>
      ))}
    </ul>
  );
}

/** Its plan, pinned above the thread while it works through it. */
function PlanCard({ todos }: { todos: Todo[] }) {
  const done = todos.filter((t) => t.status === "completed").length;
  const [open, setOpen] = useState(true);
  return (
    <div className="code-plan">
      <button className="code-plan-head" onClick={() => setOpen((v) => !v)} aria-expanded={open}>
        <b>Plan</b><span className="muted">{done} of {todos.length} done</span>
        <span className="code-plan-bar"><i style={{ width: `${(done / Math.max(1, todos.length)) * 100}%` }} /></span>
        <span className="muted code-chev">{open ? "▾" : "▸"}</span>
      </button>
      {open && <TodoList todos={todos} />}
    </div>
  );
}

// ---------------------------------------------------------------- text, code and copying

function Copy({ text, className = "" }: { text: string; className?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button className={`code-copy ${className}`} title="Copy" aria-label="Copy"
      onClick={async (e) => {
        e.stopPropagation();
        if (await writeClipboard(text)) { setDone(true); window.setTimeout(() => setDone(false), 1200); } else toastError(new Error("Couldn't copy."));
      }}>{done ? "Copied" : "Copy"}</button>
  );
}

function textOf(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(textOf).join("");
  if (typeof node === "object" && "props" in node) return textOf((node as { props: { children?: ReactNode } }).props.children);
  return "";
}

const MD: Components = {
  pre: ({ children }) => <div className="code-block"><Copy text={textOf(children).replace(/\n$/, "")} /><pre>{children}</pre></div>,
  table: ({ children }) => <div className="code-table"><table>{children}</table></div>,
  a: ({ href, children }) => <a href={href} target="_blank" rel="noreferrer">{children}</a>,
};

const Reply = memo(function Reply({ text }: { text: string }) {
  return <Markdown remarkPlugins={[remarkGfm]} components={MD}>{text}</Markdown>;
});

// ---------------------------------------------------------------- changes

/** A diff split per file: `diff --git a/x b/x` blocks. */
function splitDiff(diff: string): Map<string, string> {
  const out = new Map<string, string>();
  let name = "";
  let buf: string[] = [];
  const flush = () => { if (name) out.set(name, buf.join("\n")); };
  for (const l of diff.split("\n")) {
    const m = l.match(/^diff --git a\/(.+?) b\/(.+)$/);
    if (m) { flush(); name = m[2]; buf = [l]; } else if (name) buf.push(l);
  }
  flush();
  return out;
}

function Diff({ text }: { text: string }) {
  return (
    <pre className="diff">{text.split("\n").map((l, i) => (
      <span key={i} className={l.startsWith("+") && !l.startsWith("+++") ? "add" : l.startsWith("-") && !l.startsWith("---") ? "del" : l.startsWith("@@") ? "hunk" : l.startsWith("diff ") ? "file" : ""}>{l}{"\n"}</span>
    ))}</pre>
  );
}

function ChangesView({ id, file, onAll, onDone }: { id: string; file: string | null; onAll: () => void; onDone: () => void }) {
  const c = usePoll(() => get<{ repo: boolean; edited?: boolean; files: string[]; diff: string; log: string }>(`/code/sessions/${id}/changes`), 6000, [id]);
  const [msg, setMsg] = useState("");
  const [busy, setBusy] = useState(false);
  const d = c.data;
  if (!d) return c.error ? <Admonition variant="warning" subtle size="sm">{c.error}</Admonition> : <span className="muted">Loading…</span>;
  // One file's diff when a file was picked (its path may be relative to the folder or to its repository).
  const blocks = splitDiff(d.diff);
  const match = file ? [...blocks.keys()].find((k) => k === file || k.endsWith(`/${file}`) || file.endsWith(`/${k}`)) : undefined;
  const diff = file ? (match ? blocks.get(match) ?? "" : "") : d.diff;
  const fileBar = file && (
    <div className="row-between" style={{ gap: 8 }}>
      <span className="muted">{match ? "This file only." : "No changes to this file since the last commit (committed or undone since)."}</span>
      <Button size="xs" variant="subtle" onClick={onAll}>All changes</Button>
    </div>
  );
  if (!d.repo) {
    // A folder of projects, say: what Claude Code edited, diffed in each file's own repository.
    return (
      <div className="stack" style={{ gap: 10 }}>
        {fileBar}
        {!file && <span className="muted">
          This folder isn't itself a git repository, so these are the files the agent edited here, each compared with its own
          repository's last commit. Commit or undo them in those repositories, or start sessions in a single project to do it here.
        </span>}
        {!file && (d.files.length === 0 ? <span className="muted">No edits yet.</span>
          : <div className="list">{d.files.map((f) => <div key={f} className="list-row mono" style={{ fontSize: 12 }}>{f}</div>)}</div>)}
        {diff && <Diff text={diff} />}
      </div>
    );
  }
  const commit = async () => {
    setBusy(true);
    try { const r = await post<{ commit: string }>(`/code/sessions/${id}/commit`, { message: msg }); toastOk(`Committed ${r.commit}`); setMsg(""); void c.refresh(); onDone(); }
    catch (e) { toastError(e); } finally { setBusy(false); }
  };
  const discard = async () => {
    if (!(await ask("Discard every change?", { body: "Every file goes back to the last commit, and the new files it made are deleted. Your own new files stay. This can't be undone.", confirm: "Discard", danger: true }))) return;
    setBusy(true);
    try { await post(`/code/sessions/${id}/discard`); toastOk("Discarded."); void c.refresh(); onDone(); } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <div className="stack" style={{ gap: 10 }}>
      {fileBar}
      {!file && (d.files.length === 0 ? <span className="muted">No changes since the last commit.</span> : (
        <>
          <div className="list">{d.files.map((f) => <div key={f} className="list-row mono" style={{ fontSize: 12 }}>{f}</div>)}</div>
          <div className="row" style={{ gap: 6 }}>
            <div className="grow"><TextInput size="sm" value={msg} onChange={(e) => setMsg(e.target.value)} placeholder="Commit message (optional)" /></div>
            <Button size="sm" disabled={busy} onClick={commit}>Commit</Button>
            <Button size="sm" variant="ghost" disabled={busy} onClick={discard}>Discard</Button>
          </div>
        </>
      ))}
      {diff && <Diff text={diff} />}
      {!file && d.log && <><Eyebrow>Recent commits</Eyebrow><pre className="mono muted" style={{ fontSize: 12, margin: 0 }}>{d.log}</pre></>}
    </div>
  );
}
