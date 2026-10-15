// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Terminal sessions: coding agents' own terminal apps (Claude Code, OpenCode,
// Aider) working in a project on the box, on the box's own model, opened from
// their tiles in Connect. Each is the agent's real terminal, held on the box
// (`/coding`, `code_session.py`) so it keeps going when this window closes.
// The Code page is the built-in, sandboxed way; this is for people who want
// the agents' own interfaces.

import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { TextArea } from "@tenstorrent/vesper/text-area";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import { ago, del, get, post, wsProtocols } from "../api";
import { Card, Eyebrow, Page, toastError, toastOk, usePoll } from "../ui";
import { ask } from "../dialog";
import { navigate } from "../App";

interface Agent { id: string; name: string; blurb: string; available: boolean }
interface Session { id: string; agent: string; agent_name: string; folder: string; task: string | null; created: number; ended: number | null; exit: number | null; live: boolean; changes: number }
interface Check { ok: boolean; level: "ok" | "warn" | "block"; why: string; model?: string; context?: number | null }
interface Coding { agents: Agent[]; sessions: Session[]; recent: string[]; check: Check; home: string }

const base = (p: string) => p.split("/").filter(Boolean).pop() ?? p;
/** Connect's tiles open this page with their agent picked. */
export const AGENT_KEY = "tt.terminals.agent";
const AGENT_ICON: Record<string, string> = { claude: "✳", opencode: "◇", aider: "◆", shell: "›_" };

export function Terminals() {
  const c = usePoll(() => get<Coding>("/coding"), 5000);
  const d = c.data;
  const sessions = d?.sessions ?? [];
  const [picked, setPicked] = useState<string | null>(() => location.hash.replace("#", "") || null);
  const [starting, setStarting] = useState(() => { try { return !!localStorage.getItem(AGENT_KEY); } catch { return false; } });
  const sel = sessions.find((s) => s.id === picked) ?? sessions.find((s) => s.live) ?? null;
  useEffect(() => { if (sel) history.replaceState(null, "", `/app/terminals#${sel.id}`); }, [sel?.id]); // eslint-disable-line react-hooks/exhaustive-deps
  const showNew = starting || !sel;

  return (
    <Page title="Terminal sessions" subtitle="Coding agents' own terminal apps, working in your projects on the box with the box's model. They run as you (not in the sandbox the Code page uses) and keep going when you close this window."
      actions={<Button variant="tertiary" onClick={() => navigate("connect")}>Back to Connect</Button>}>
      {d?.check && d.check.level !== "ok" && (
        <Admonition variant={d.check.level === "block" ? "warning" : "info"} subtle size="sm">
          <div className="row-between wrap" style={{ gap: 8 }}><span>{d.check.why}</span>
            {d.check.level === "block" && <Button size="xs" variant="subtle" onClick={() => navigate("models")}>Open Models</Button>}</div>
        </Admonition>
      )}
      {d?.check?.level === "ok" && <span className="muted" style={{ fontSize: 13 }}>✓ {d.check.why} Ready for coding agents.</span>}

      {sessions.length > 0 && (
        <div className="row-between wrap">
          <div className="run-chips" role="tablist" aria-label="Sessions">
            {sessions.map((s) => (
              <button key={s.id} role="tab" aria-selected={sel?.id === s.id && !starting} className={`run-chip ${sel?.id === s.id && !starting ? "on" : ""}`} onClick={() => { setPicked(s.id); setStarting(false); }}>
                <span className={`run-dot ${s.live ? "live" : s.exit === 0 ? "done" : "stopped"}`} />
                <b>{base(s.folder)}</b>
                <span className="muted">{s.agent_name}{s.changes ? ` · ${s.changes} changed` : ""}</span>
              </button>
            ))}
          </div>
          {!showNew && <Button size="sm" onClick={() => setStarting(true)}>New session</Button>}
        </div>
      )}

      {showNew && d && <NewSession d={d} onClose={sel ? () => setStarting(false) : undefined}
        onStarted={(s) => { setPicked(s.id); setStarting(false); c.refresh(); }} />}
      {!showNew && sel && <SessionView key={sel.id} s={sel} onChanged={c.refresh} onDeleted={() => { setPicked(null); c.refresh(); }} />}
    </Page>
  );
}

function NewSession({ d, onStarted, onClose }: { d: Coding; onStarted: (s: Session) => void; onClose?: () => void }) {
  const firstAvailable = d.agents.find((a) => a.available && a.id !== "shell")?.id ?? "shell";
  const [agent, setAgent] = useState(() => { try { const a = localStorage.getItem(AGENT_KEY); localStorage.removeItem(AGENT_KEY); return a ?? firstAvailable; } catch { return firstAvailable; } });
  const [folder, setFolder] = useState<string>(d.recent[0] ?? "");
  const [task, setTask] = useState("");
  const [busy, setBusy] = useState(false);
  const browse = useSheet();
  const start = async () => {
    setBusy(true);
    try {
      const r = await post<{ session: Session }>("/coding/sessions", { agent, folder, task: agent === "claude" ? task : undefined });
      toastOk(`${r.session.agent_name} is starting in ${base(folder)}.`);
      onStarted(r.session);
    } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <Card className="start-card">
      <div className="row-between"><Eyebrow>New coding session</Eyebrow>{onClose && <Button size="xs" variant="ghost" onClick={onClose}>Close</Button>}</div>
      <div className="stack" style={{ gap: 6 }}>
        <b style={{ fontSize: 14 }}>1. Which agent?</b>
        <div className="agent-picks" role="radiogroup" aria-label="Agent">
          {d.agents.map((a) => (
            <button key={a.id} role="radio" aria-checked={agent === a.id} disabled={!a.available} className={`agent-pick ${agent === a.id ? "on" : ""}`} onClick={() => setAgent(a.id)}>
              <span className="agent-pick-name"><span aria-hidden="true">{AGENT_ICON[a.id] ?? "•"}</span> {a.name}{!a.available && <span className="muted"> · not installed</span>}</span>
              <span className="muted" style={{ fontSize: 12 }}>{a.blurb}</span>
            </button>
          ))}
        </div>
      </div>
      <div className="stack" style={{ gap: 6 }}>
        <b style={{ fontSize: 14 }}>2. Which project?</b>
        <div className="row wrap" style={{ gap: 6 }}>
          {d.recent.map((f) => <button key={f} className={`example-chip ${folder === f ? "on" : ""}`} onClick={() => setFolder(f)}>{base(f)}</button>)}
          <Button size="xs" variant="subtle" onClick={browse.open}>{d.recent.length ? "Another folder…" : "Choose a folder…"}</Button>
        </div>
        {folder && <span className="muted mono" style={{ fontSize: 12 }}>{folder}</span>}
      </div>
      {agent === "claude" && (
        <div className="stack" style={{ gap: 6 }}>
          <b style={{ fontSize: 14 }}>3. First task <span className="muted" style={{ fontWeight: 400 }}>(optional; you can also type it in the session)</span></b>
          <TextArea value={task} onChange={(e) => setTask(e.target.value)} height={64} placeholder="e.g. The tests in tests/ fail. Find out why, fix it, and run them to confirm." />
        </div>
      )}
      <div className="row-between wrap">
        <span className="muted" style={{ fontSize: 12 }}>It runs as you, in that folder, and asks before it runs commands or edits files. Use a git repository so every change can be reviewed.</span>
        <Button disabled={busy || !folder || !d.agents.find((a) => a.id === agent)?.available} onClick={start}>{busy ? "Starting…" : "Start"}</Button>
      </div>
      <Sheet ref={browse.ref} popover title="Choose a project folder" description="A folder in your home folder. A git repository works best.">
        <FolderBrowser home={d.home} onPick={(p) => { setFolder(p); browse.close(); }} />
      </Sheet>
    </Card>
  );
}

export function FolderBrowser({ home, onPick }: { home: string; onPick: (path: string) => void }) {
  const [path, setPath] = useState(home);
  const list = usePoll(() => get<{ path: string; parent: string | null; git: boolean; folders: { name: string; path: string; git: boolean }[] }>(`/coding/folders?path=${encodeURIComponent(path)}`), 60000, [path]);
  const [name, setName] = useState("");
  const l = list.data;
  const create = async () => {
    try { const r = await post<{ path: string }>("/coding/folders", { parent: path, name: name.trim() }); toastOk(`Made ${name.trim()} (a new git repository).`); onPick(r.path); }
    catch (e) { toastError(e); }
  };
  return (
    <div className="stack" style={{ gap: 10 }}>
      <div className="row" style={{ gap: 6 }}>
        <Button size="xs" variant="ghost" disabled={!l?.parent} onClick={() => l?.parent && setPath(l.parent)}>↑ Up</Button>
        <span className="mono muted grow" style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis" }}>{l?.path ?? path}</span>
        {l && l.path !== home && <Button size="xs" onClick={() => onPick(l.path)}>Use this folder{l.git ? "" : ""}</Button>}
      </div>
      <div className="list" style={{ maxHeight: 360, overflowY: "auto" }}>
        {(l?.folders ?? []).map((f) => (
          <div key={f.path} className="list-row">
            <button type="button" className="folder-open" onClick={() => setPath(f.path)} aria-label={`Open ${f.name}${f.git ? " (git repository)" : ""}`}>
              <span aria-hidden>📁</span><span className="grow">{f.name}</span>{f.git && <Badge size="sm" variant="mint" subtle>git</Badge>}
            </button>
            <Button size="xs" variant="ghost" aria-label={`Use ${f.name}`} onClick={() => onPick(f.path)}>Use</Button>
          </div>
        ))}
        {l && l.folders.length === 0 && <span className="muted" style={{ fontSize: 13 }}>No folders here.</span>}
      </div>
      <div className="row" style={{ gap: 6 }}>
        <div className="grow"><TextInput size="sm" value={name} onChange={(e) => setName(e.target.value)} placeholder="New project name" /></div>
        <Button size="sm" variant="subtle" disabled={!name.trim()} onClick={create}>New project here</Button>
      </div>
    </div>
  );
}

function SessionView({ s, onChanged, onDeleted }: { s: Session; onChanged: () => void; onDeleted: () => void }) {
  const changes = useSheet();
  const stop = async () => {
    if (!(await ask("Stop this session?", { body: `${s.agent_name} stops where it is. Its changes in ${base(s.folder)} stay.`, confirm: "Stop", danger: true }))) return;
    try { await post(`/coding/sessions/${s.id}/stop`); onChanged(); } catch (e) { toastError(e); }
  };
  const remove = async () => {
    if (!(await ask("Remove this session?", { body: `Its terminal and history leave TT Station. Its changes in ${base(s.folder)} stay.`, confirm: "Remove", danger: true }))) return;
    try { await del(`/coding/sessions/${s.id}`); onDeleted(); } catch (e) { toastError(e); }
  };
  return (
    <div className="stack" style={{ gap: 10 }}>
      <div className="row-between wrap" style={{ gap: 8 }}>
        <div className="stack" style={{ gap: 2, minWidth: 0 }}>
          <div className="row" style={{ gap: 8 }}>
            <Typography variant="heading-xs">{s.agent_name} in {base(s.folder)}</Typography>
            <Badge size="sm" subtle variant={s.live ? "info" : s.exit === 0 ? "mint" : "contrast"}>{s.live ? "working" : `ended${s.exit != null ? ` (exit ${s.exit})` : ""}`}</Badge>
          </div>
          <span className="muted mono" style={{ fontSize: 12 }}>{s.folder} · started {ago(s.created)}</span>
        </div>
        <div className="row" style={{ gap: 6 }}>
          <Button size="sm" variant="subtle" onClick={changes.open}>Changes{s.changes ? ` (${s.changes})` : ""}</Button>
          {s.live ? <Button size="sm" variant="ghost" onClick={stop}>Stop</Button> : <Button size="sm" variant="ghost" onClick={remove}>Remove</Button>}
        </div>
      </div>
      <CodeTerm id={s.id} />
      <span className="muted" style={{ fontSize: 12 }}>
        Click the terminal to type. It's {s.agent_name}'s own screen, running on the box: its keys and commands work as usual{s.agent === "claude" ? " (/help, Shift+Tab for modes, Esc to interrupt)" : ""}.
      </span>
      <Sheet ref={changes.ref} popover title={`Changes in ${base(s.folder)}`} description="What's different from the last commit, including new files.">
        <Changes id={s.id} />
      </Sheet>
    </div>
  );
}

function Changes({ id }: { id: string }) {
  const c = usePoll(() => get<{ repo: boolean; files: string[]; diff: string; log: string }>(`/coding/sessions/${id}/changes`), 8000, [id]);
  const d = c.data;
  if (!d) return <span className="muted">Loading…</span>;
  if (!d.repo) return <span className="muted">This folder isn't a git repository, so there's no diff to show. Start a session in a repository (or make a new project) to review changes.</span>;
  return (
    <div className="stack" style={{ gap: 10 }}>
      {d.files.length === 0 ? <span className="muted">No changes since the last commit.</span> : (
        <div className="list">{d.files.map((f) => <div key={f} className="list-row mono" style={{ fontSize: 12 }}>{f}</div>)}</div>
      )}
      {d.diff && (
        <pre className="diff">{d.diff.split("\n").map((l, i) => (
          <span key={i} className={l.startsWith("+") && !l.startsWith("+++") ? "add" : l.startsWith("-") && !l.startsWith("---") ? "del" : l.startsWith("@@") ? "hunk" : l.startsWith("diff ") ? "file" : ""}>{l}{"\n"}</span>
        ))}</pre>
      )}
      {d.log && <><Eyebrow>Recent commits</Eyebrow><pre className="mono muted" style={{ fontSize: 12, margin: 0 }}>{d.log}</pre></>}
    </div>
  );
}

/** The session's terminal: the agent's own screen, streamed from the box. */
function CodeTerm({ id }: { id: string }) {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<"connecting" | "live" | "ended">("connecting");
  useEffect(() => {
    if (!host.current) return;
    const term = new Terminal({
      fontFamily: "'IBM Plex Mono', 'DejaVu Sans Mono', 'Noto Sans Mono', monospace",
      fontSize: 13, lineHeight: 1.1, cursorBlink: true, allowProposedApi: true, scrollback: 5000,
      theme: { background: "#0b1211", foreground: "#d8e6e3", cursor: "#5eead4" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    try { term.loadAddon(new WebglAddon()); } catch { /* canvas is fine */ }
    fit.fit();
    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/coding/term?session=${id}&cols=${term.cols}&rows=${term.rows}`;
    const ws = new WebSocket(url, wsProtocols());
    ws.binaryType = "arraybuffer";
    ws.onopen = () => setState("live");
    ws.onmessage = (m) => term.write(typeof m.data === "string" ? m.data : new Uint8Array(m.data));
    ws.onclose = () => setState("ended");
    const input = term.onData((data) => { if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "input", data })); });
    const resize = new ResizeObserver(() => {
      fit.fit();
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
    });
    resize.observe(host.current);
    term.focus();
    return () => { resize.disconnect(); input.dispose(); ws.close(); term.dispose(); };
  }, [id]);
  return (
    <div className="codeterm-wrap">
      <div ref={host} className="codeterm" onClick={(e) => (e.currentTarget.querySelector("textarea") as HTMLTextAreaElement | null)?.focus()} />
      {state !== "live" && <span className="codeterm-state">{state === "connecting" ? "connecting…" : "disconnected"}</span>}
    </div>
  );
}
