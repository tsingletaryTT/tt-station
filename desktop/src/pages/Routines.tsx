// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Routines: jobs the box does on its own (`routines.rs`). A routine is a goal
// in plain words and when to run it; the served model works out the steps,
// looks things up freely, asks before changing the box (unless the routine
// was given that action ahead of time), and ends with a report. Runs that
// need an answer show here and as a notification.

import { useEffect, useMemo, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Icon } from "@tenstorrent/vesper/icons";
import { Switch } from "@tenstorrent/vesper/switch";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { externalLinks } from "../links";
import { ago, ApiError, del, get, post, put } from "../api";
import { Card, Empty, Eyebrow, Page, toastError, toastOk, usePoll } from "../ui";
import { ask } from "../dialog";

interface Schedule { kind: "manual" | "daily" | "weekdays" | "hourly"; time?: string | null; every_hours?: number | null }
interface LastRun { id: string; status: string; started: number; finished: number | null; headline: string | null }
interface Routine { id: string; name: string; goal: string; schedule: Schedule; when: string; allowed: string[]; enabled: boolean; created: number; next_run: number | null; last: LastRun | null }
interface Step { ts: number; kind: string; tool: string | null; args: Record<string, unknown>; result: string; ok: boolean; seconds: number }
interface Pending { tool: string; args: Record<string, unknown>; summary: string; since: number }
interface Run { id: string; routine: string; name: string; trigger: string; started: number; finished: number | null; status: string; steps: Step[]; report: string | null; pending: Pending | null; activity: string | null; queued?: Queued[] }
/** A step it asked for after reading the web: waits for the person, even when the run is over. */
interface Queued { n: number; tool: string; summary: string; since: number; status: "waiting" | "allowed" | "declined" | "expired"; result: string | null }
interface RunRow { id: string; status: string; trigger: string; started: number; finished: number | null; steps: number; headline: string | null }

const ACTION_LABELS: Record<string, [string, string]> = {
  start_model: ["Start a model", "Switches what's serving (stops the current model)."],
  stop_model: ["Stop the model", "Leaves the chips idle."],
  download_model: ["Download a model", "Community models, with weights: can be hundreds of GB."],
  reset_chips: ["Reset the chips", "Stops whatever is serving."],
};

const TEMPLATES: { name: string; goal: string; schedule: Schedule; allowed: string[] }[] = [
  {
    name: "Morning health check",
    goal: "Check the box: what's serving and whether it answers (ask it a one-line question), the chips' temperatures and power, and the recent logs for errors or warnings. Report anything unusual first; if all is well, say so in one line.",
    schedule: { kind: "weekdays", time: "08:30" },
    allowed: [],
  },
  {
    name: "Try a community model overnight",
    goal: "Download the community model sjettTT/qwen3.8-flash-next_p150x4 if it isn't on the box yet, start it, and once it's ready ask it three short questions (a coding task, a reasoning puzzle, a summary of a paragraph). Report its speed (tokens per second, time to first token) and how good its answers were. Then put the model that was serving before back.",
    schedule: { kind: "manual" },
    allowed: ["download_model", "start_model"],
  },
  {
    name: "Compare my chat models overnight",
    goal: "Find the chat models on this box whose weights are all here and that fit (community_models and list_models). For each one in turn: start it, wait until it's ready, and run evaluate_model. Skip any that fail to start, and say why. At the end, put the model that was serving before back. Report a table of each model's overall score and speed, and which you'd use for Chat, for Code and for Research.",
    schedule: { kind: "manual" },
    allowed: ["start_model"],
  },
  {
    name: "Keep a model serving",
    goal: "Make sure a chat model is serving and answering. If nothing is serving, or the model doesn't answer a short question, start Qwen3.8-27B (mando2222/qwen3.8-27b-dflash2-p300x2-q4kv) and check it answers. Report only what you had to fix.",
    schedule: { kind: "hourly", every_hours: 2 },
    allowed: ["start_model"],
  },
  {
    name: "Weekly model news",
    goal: "Research what's new this week for running open models on Tenstorrent hardware (new community packages, tt-metal releases, notable model releases that would fit a QuietBox 2). Summarize the five most useful items with links.",
    schedule: { kind: "weekdays", time: "09:00" },
    allowed: [],
  },
];

const STATUS: Record<string, { label: string; tone: "mint" | "info" | "warning" | "contrast" | "danger" }> = {
  running: { label: "Running", tone: "info" },
  waiting: { label: "Needs your OK", tone: "warning" },
  done: { label: "Done", tone: "mint" },
  failed: { label: "Didn't finish", tone: "danger" },
  stopped: { label: "Stopped", tone: "contrast" },
};

const when = (t: number) => {
  const d = new Date(t * 1000), today = new Date();
  const same = d.toDateString() === today.toDateString();
  const tomorrow = new Date(today.getTime() + 86400000).toDateString() === d.toDateString();
  const hm = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return same ? `today ${hm}` : tomorrow ? `tomorrow ${hm}` : `${d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" })} ${hm}`;
};
const clock = (s: number) => (s >= 3600 ? `${Math.floor(s / 3600)}h ${Math.round((s % 3600) / 60)}m` : s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`);

export function Routines() {
  const data = usePoll(() => get<{ routines: Routine[]; running: Run | null }>("/routines"), 4000);
  const routines = data.data?.routines ?? [];
  const running = data.data?.running ?? null;
  const editor = useSheet();
  const [editing, setEditing] = useState<Partial<Routine> | null>(null);
  const detail = useSheet();
  const [open, setOpen] = useState<string | null>(null);
  const [viewRun, setViewRun] = useState<string | null>(null);

  const edit = (r: Partial<Routine> | null) => { setEditing(r ?? {}); editor.open(); };
  const show = (id: string, run: string | null = null) => { setOpen(id); setViewRun(run); detail.open(); };
  const runNow = async (r: Routine) => {
    try { const x = await post<{ run: Run }>(`/routines/${r.id}/run`); toastOk(`${r.name} is running.`); show(r.id, x.run.id); void data.refresh(); }
    catch (e) { toastError(e); }
  };
  const toggle = async (r: Routine, enabled: boolean) => {
    try { await put(`/routines/${r.id}`, { name: r.name, goal: r.goal, schedule: r.schedule, allowed: r.allowed, enabled }); void data.refresh(); }
    catch (e) { toastError(e); }
  };
  const remove = async (r: Routine) => {
    if (!(await ask(`Delete “${r.name}”?`, { body: "The routine and its past runs are removed.", confirm: "Delete", danger: true }))) return;
    try { await del(`/routines/${r.id}`); detail.close(); void data.refresh(); } catch (e) { toastError(e); }
  };
  const opened = routines.find((r) => r.id === open) ?? null;

  return (
    <Page title="Routines"
      subtitle="Jobs the box does on its own. Say what you want in plain words and when; it works out the steps, asks before changing anything, and reports back."
      actions={<Button iconLeft={<Icon kind="add" />} onClick={() => edit(null)}>New routine</Button>}>
      {data.error && <Admonition variant="warning" subtle size="sm">Couldn't load routines: {data.error}</Admonition>}
      {running && <RunningBanner run={running} onOpen={() => show(running.routine, running.id)} onChanged={() => void data.refresh()} />}

      {data.data && routines.length === 0 && (
        <div className="stack" style={{ gap: 12 }}>
          <Empty title="No routines yet">Start from an example, or write your own.</Empty>
          <Templates onPick={(t) => edit(t)} />
        </div>
      )}

      <div className="grid grid-2">
        {routines.map((r) => (
          <Card key={r.id} className="routine-card">
            <div className="row-between" style={{ gap: 8 }}>
              <button className="routine-title" onClick={() => show(r.id)}>
                <Typography variant="heading-xs">{r.name}</Typography>
              </button>
              <Switch size="sm" checked={r.enabled} aria-label={r.enabled ? "On" : "Off"} title={r.enabled ? "On: runs on its schedule" : "Off: runs only when you run it"}
                onChange={(e) => toggle(r, e.target.checked)} />
            </div>
            <span className="muted routine-goal">{r.goal}</span>
            <div className="row wrap" style={{ gap: 6 }}>
              <Badge size="sm" variant="contrast" subtle><Icon kind="clock" style={{ width: 12, height: 12 }} /> {r.when}</Badge>
              {r.enabled && r.next_run && <span className="muted" style={{ fontSize: 12 }}>next {when(r.next_run)}</span>}
              {r.allowed.length > 0 && <span className="muted" style={{ fontSize: 12 }} title={r.allowed.map((a) => ACTION_LABELS[a]?.[0] ?? a).join(", ")}>· may act without asking</span>}
            </div>
            {r.last && (
              <button className="routine-last" onClick={() => show(r.id, r.last!.id)}>
                <Badge size="sm" variant={STATUS[r.last.status]?.tone ?? "contrast"} subtle>{STATUS[r.last.status]?.label ?? r.last.status}</Badge>
                <span className="muted">{ago(r.last.finished ?? r.last.started)}</span>
                {r.last.headline && <span className="routine-headline">{r.last.headline}</span>}
              </button>
            )}
            <div className="grow" />
            <div className="row" style={{ gap: 6 }}>
              <Button size="sm" disabled={!!running} title={running ? "One routine runs at a time" : undefined} onClick={() => runNow(r)}>Run now</Button>
              <Button size="sm" variant="subtle" onClick={() => edit(r)}>Edit</Button>
              <Button size="sm" variant="ghost" onClick={() => show(r.id)}>History</Button>
            </div>
          </Card>
        ))}
      </div>

      <Sheet ref={editor.ref} title={editing?.id ? "Edit routine" : "New routine"} description="What it should do and when. It plans the steps itself.">
        {editing && <Editor key={editing.id ?? editing.name ?? "new"} start={editing} onDone={(saved) => { editor.close(); setEditing(null); void data.refresh(); if (saved) toastOk("Saved."); }} />}
      </Sheet>
      <Sheet ref={detail.ref} title={opened?.name ?? "Routine"} description={opened ? opened.when : ""}
        onClose={() => { setOpen(null); setViewRun(null); }}>
        {opened && <Detail r={opened} run={viewRun} setRun={setViewRun} busy={!!running} onRun={() => runNow(opened)} onEdit={() => { detail.close(); edit(opened); }} onDelete={() => remove(opened)} />}
      </Sheet>
    </Page>
  );
}

function Templates({ onPick }: { onPick: (t: Partial<Routine>) => void }) {
  return (
    <div className="grid grid-2">
      {TEMPLATES.map((t) => (
        <button key={t.name} className="routine-template" onClick={() => onPick({ name: t.name, goal: t.goal, schedule: t.schedule, allowed: t.allowed, enabled: true })}>
          <b>{t.name}</b>
          <span className="muted">{t.goal}</span>
        </button>
      ))}
    </div>
  );
}

function RunningBanner({ run, onOpen, onChanged }: { run: Run; onOpen: () => void; onChanged: () => void }) {
  const stop = async () => {
    try { await post(`/routines/runs/${run.id}/stop`); onChanged(); } catch (e) { toastError(e); }
  };
  return (
    <Card className={`routine-running ${run.status === "waiting" ? "waiting" : ""}`}>
      <div className="row-between wrap" style={{ gap: 10 }}>
        <div className="stack" style={{ gap: 2, minWidth: 0 }}>
          <b>{run.name}{run.status === "waiting" ? " needs your OK" : " is running"}</b>
          <span className="muted" style={{ fontSize: 13 }}>{run.pending ? `${run.pending.summary}?` : run.activity ?? "Working"} · {run.steps.length} steps so far</span>
        </div>
        <div className="row" style={{ gap: 6 }}>
          {run.pending && <Decide run={run} onDone={onChanged} />}
          <Button size="sm" variant="subtle" onClick={onOpen}>Watch</Button>
          <Button size="sm" variant="ghost" onClick={stop}>Stop</Button>
        </div>
      </div>
    </Card>
  );
}

function Decide({ run, onDone }: { run: Run; onDone: () => void }) {
  const [sent, setSent] = useState(false);
  const decide = async (allow: boolean) => {
    if (sent) return;
    setSent(true);
    try { await post(`/routines/runs/${run.id}/approve`, { allow }); }
    catch (e) { if (!(e instanceof ApiError && e.status === 409)) { setSent(false); toastError(e); return; } }
    onDone();
  };
  return (
    <>
      <Button size="sm" disabled={sent} onClick={() => decide(true)}>Allow</Button>
      <Button size="sm" variant="subtle" disabled={sent} onClick={() => decide(false)}>Deny</Button>
    </>
  );
}

/** Steps it queued after reading the web: allowed, one runs now; declined, it's dropped. */
function QueuedSteps({ run, onDone }: { run: Run; onDone: () => void }) {
  const [busy, setBusy] = useState<number | null>(null);
  const answer = async (q: Queued, allow: boolean) => {
    setBusy(q.n);
    try { await post("/approvals/answer", { id: `routine:${run.id}:${q.n}`, allow }); }
    catch (e) { toastError(e); } finally { setBusy(null); onDone(); }
  };
  const words: Record<Queued["status"], string> = { waiting: "", allowed: "Allowed", declined: "Declined", expired: "Waited too long" };
  return (
    <div className="code-ask stack" style={{ gap: 8 }}>
      <b>It read the web, so these wait for you</b>
      <span className="muted" style={{ fontSize: 13 }}>Text from a web page may be what asked for them. Allow only what you meant the routine to do.</span>
      {(run.queued ?? []).map((q) => (
        <div key={q.n} className="row-between wrap" style={{ gap: 8 }}>
          <span className="grow">{q.summary}{q.result ? <span className="muted"> · {q.result.slice(0, 160)}</span> : null}</span>
          {q.status === "waiting" ? (
            <div className="row" style={{ gap: 6 }}>
              <Button size="sm" disabled={busy !== null} onClick={() => void answer(q, true)}>Allow</Button>
              <Button size="sm" variant="subtle" disabled={busy !== null} onClick={() => void answer(q, false)}>Deny</Button>
            </div>
          ) : <span className="muted" style={{ fontSize: 12 }}>{words[q.status]}</span>}
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------- editor

function Editor({ start, onDone }: { start: Partial<Routine>; onDone: (saved: boolean) => void }) {
  const [name, setName] = useState(start.name ?? "");
  const [goal, setGoal] = useState(start.goal ?? "");
  const [kind, setKind] = useState<Schedule["kind"]>(start.schedule?.kind ?? "manual");
  const [time, setTime] = useState(start.schedule?.time ?? "08:30");
  const [hours, setHours] = useState(start.schedule?.every_hours ?? 6);
  const [allowed, setAllowed] = useState<string[]>(start.allowed ?? []);
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    const body = { name, goal, allowed, enabled: start.enabled ?? true, schedule: { kind, time: kind === "daily" || kind === "weekdays" ? time : null, every_hours: kind === "hourly" ? hours : null } };
    try {
      if (start.id) await put(`/routines/${start.id}`, body); else await post("/routines", body);
      onDone(true);
    } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <div className="stack" style={{ gap: 16 }}>
      {!start.id && !start.goal && (
        <div className="stack" style={{ gap: 6 }}>
          <Eyebrow>Start from an example</Eyebrow>
          <div className="row wrap" style={{ gap: 6 }}>
            {TEMPLATES.map((t) => (
              <button key={t.name} className="example-chip" onClick={() => { setName(t.name); setGoal(t.goal); setKind(t.schedule.kind); setTime(t.schedule.time ?? "08:30"); setHours(t.schedule.every_hours ?? 6); setAllowed(t.allowed); }}>{t.name}</button>
            ))}
          </div>
        </div>
      )}
      <label className="stack" style={{ gap: 4 }}>
        <b>Name</b>
        <TextInput value={name} onChange={(e) => setName(e.target.value)} placeholder="Morning health check" />
      </label>
      <label className="stack" style={{ gap: 4 }}>
        <b>What should it do?</b>
        <textarea className="routine-goal-input" rows={6} value={goal} onChange={(e) => setGoal(e.target.value)}
          placeholder="In plain words, as you'd ask a colleague: what to check or do, and what you want to hear back." />
        <span className="muted" style={{ fontSize: 12 }}>It can check the box, the chips, the logs and the models; search the web; ask the serving model questions; and start, stop or download models.</span>
      </label>
      <div className="stack" style={{ gap: 6 }}>
        <b>When</b>
        <div className="pill-tabs" role="radiogroup" aria-label="When it runs">
          {(["manual", "daily", "weekdays", "hourly"] as const).map((k) => (
            <button key={k} role="radio" aria-checked={kind === k} className={kind === k ? "on" : ""} onClick={() => setKind(k)}>
              {{ manual: "When I run it", daily: "Every day", weekdays: "Weekdays", hourly: "Every few hours" }[k]}
            </button>
          ))}
        </div>
        {(kind === "daily" || kind === "weekdays") && (
          <label className="row" style={{ gap: 8 }}>at <input type="time" className="routine-time" value={time ?? "08:30"} onChange={(e) => setTime(e.target.value)} /></label>
        )}
        {kind === "hourly" && (
          <label className="row" style={{ gap: 8 }}>every <input type="number" min={1} max={168} className="routine-time" value={hours ?? 6} onChange={(e) => setHours(Number(e.target.value))} /> hours</label>
        )}
      </div>
      <div className="stack" style={{ gap: 6 }}>
        <b>May do without asking</b>
        <span className="muted" style={{ fontSize: 12 }}>Looking things up never needs asking. Anything below that isn't ticked asks you first, with a notification; tick what it needs when nobody's around (overnight). Once a run has read the web, everything that acts waits for you, ticked or not: the run finishes and tells you what's waiting.</span>
        {Object.entries(ACTION_LABELS).map(([id, [label, hint]]) => (
          <label key={id} className="routine-allow">
            <input type="checkbox" checked={allowed.includes(id)} onChange={(e) => setAllowed((a) => (e.target.checked ? [...a, id] : a.filter((x) => x !== id)))} />
            <span><b>{label}</b> <span className="muted">· {hint}</span></span>
          </label>
        ))}
      </div>
      <div className="row" style={{ gap: 8 }}>
        <Button disabled={busy || !name.trim() || !goal.trim()} onClick={save}>{busy ? "Saving…" : start.id ? "Save" : "Create routine"}</Button>
        <Button variant="ghost" onClick={() => onDone(false)}>Cancel</Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- history and a run

function Detail({ r, run, setRun, busy, onRun, onEdit, onDelete }: { r: Routine; run: string | null; setRun: (id: string | null) => void; busy: boolean; onRun: () => void; onEdit: () => void; onDelete: () => void }) {
  const runs = usePoll(() => get<{ runs: RunRow[] }>(`/routines/${r.id}/runs`), 5000, [r.id]);
  if (run) return <RunView id={run} onBack={() => setRun(null)} />;
  return (
    <div className="stack" style={{ gap: 14 }}>
      <div className="routine-goal-box">{r.goal}</div>
      <div className="row wrap" style={{ gap: 6 }}>
        <Button size="sm" disabled={busy} onClick={onRun}>Run now</Button>
        <Button size="sm" variant="subtle" onClick={onEdit}>Edit</Button>
        <Button size="sm" variant="ghost" onClick={onDelete}>Delete</Button>
      </div>
      <Eyebrow>Runs</Eyebrow>
      {!runs.data ? <span className="muted">Loading…</span> : runs.data.runs.length === 0 ? <span className="muted">It hasn't run yet.</span> : (
        <div className="list">
          {runs.data.runs.map((x) => (
            <button key={x.id} className="list-row routine-run-row" onClick={() => setRun(x.id)}>
              <Badge size="sm" variant={STATUS[x.status]?.tone ?? "contrast"} subtle>{STATUS[x.status]?.label ?? x.status}</Badge>
              <span className="grow routine-headline">{x.headline ?? `${x.steps} steps`}</span>
              <span className="muted" style={{ fontSize: 12, whiteSpace: "nowrap" }}>{ago(x.started)}{x.trigger === "schedule" ? " · scheduled" : ""}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

const TOOL_LABEL: Record<string, string> = {
  box_status: "Checked the box", list_models: "Listed models", recent_logs: "Read the logs", web_search: "Searched the web", read_webpage: "Read a page",
  research: "Started research", research_check: "Checked the research", search_documents: "Searched documents", remember: "Remembered something",
  community_models: "Checked community models", ask_model: "Asked the model", wait_until_ready: "Waited for the model", wait_for_download: "Waited for the download",
  start_model: "Started a model", stop_model: "Stopped the model", download_model: "Started a download", reset_chips: "Reset the chips",
};

function stepTitle(s: Step) {
  const a = s.args ?? {};
  const base = TOOL_LABEL[s.tool ?? ""] ?? s.tool ?? "";
  if (s.kind === "queued") return `Waiting for your OK: ${base}`;
  if (s.tool === "start_model") return `Started ${String(a.model ?? "a model")}`;
  if (s.tool === "download_model") return `Started downloading ${String(a.id ?? "")}`;
  if (s.tool === "ask_model") return `Asked the model: “${String(a.prompt ?? "").slice(0, 80)}${String(a.prompt ?? "").length > 80 ? "…" : ""}”`;
  if (s.tool === "web_search") return `Searched the web for “${String(a.query ?? "")}”`;
  return base;
}

/** The useful part of a result: a model's reply and its speed, rather than raw JSON. */
function stepSummary(s: Step) {
  try {
    const v = JSON.parse(s.result);
    if (s.tool === "ask_model" && typeof v.reply === "string") return `${v.tokens_per_second} tok/s · first token in ${v.seconds_to_first_token}s\n\n${v.reply}`;
    if (typeof v.summary === "string") return v.summary;
  } catch { /* plain text */ }
  return s.result;
}

function RunView({ id, onBack }: { id: string; onBack: () => void }) {
  const r = usePoll(() => get<{ run: Run }>(`/routines/runs/${id}`), 2500, [id]);
  const run = r.data?.run;
  const [, tick] = useState(0);
  useEffect(() => { if (run?.finished) return; const t = window.setInterval(() => tick((n) => n + 1), 1000); return () => window.clearInterval(t); }, [run?.finished]);
  const steps = useMemo(() => run?.steps ?? [], [run]);
  if (!run) return <span className="muted">Loading…</span>;
  const live = !run.finished;
  const took = (run.finished ?? Math.floor(Date.now() / 1000)) - run.started;
  return (
    <div className="stack" style={{ gap: 12 }}>
      <div className="row-between">
        <button className="link-button" onClick={onBack}>← All runs</button>
        <span className="muted" style={{ fontSize: 12 }}>{new Date(run.started * 1000).toLocaleString()} · {clock(took)}{run.trigger === "schedule" ? " · scheduled" : ""}</span>
      </div>
      <div className="row" style={{ gap: 8 }}>
        <Badge size="sm" variant={STATUS[run.status]?.tone ?? "contrast"} subtle>{STATUS[run.status]?.label ?? run.status}</Badge>
        {live && <span className="muted thinking-text">{run.activity ?? "Working"}</span>}
      </div>
      {run.pending && (
        <div className="code-ask">
          <div className="row-between wrap" style={{ gap: 8 }}>
            <b>May it {run.pending.summary.charAt(0).toLowerCase() + run.pending.summary.slice(1)}?</b>
            <div className="row" style={{ gap: 6 }}><Decide run={run} onDone={() => void r.refresh()} /></div>
          </div>
        </div>
      )}
      {(run.queued ?? []).length > 0 && <QueuedSteps run={run} onDone={() => void r.refresh()} />}
      <ol className="routine-steps">
        {steps.map((s, i) => s.kind === "note" ? (
          <li key={i} className="routine-note">{s.result}</li>
        ) : (
          <li key={i} className={`routine-step ${s.ok ? "" : "bad"} ${s.kind}`}>
            <details>
              <summary>
                <span className={`code-step-state small ${s.ok ? "ok" : "bad"}`} aria-hidden="true">{s.ok ? "" : "!"}</span>
                <span className="grow">{stepTitle(s)}</span>
                {s.kind === "action" && <Badge size="sm" variant="warning" subtle>action</Badge>}
                {s.seconds >= 2 && <span className="muted mono" style={{ fontSize: 11 }}>{clock(s.seconds)}</span>}
              </summary>
              <pre className="code-out">{stepSummary(s)}</pre>
            </details>
          </li>
        ))}
      </ol>
      {run.report && (
        <div className={`routine-report ${run.status}`}>
          <Eyebrow>{run.status === "done" ? "Report" : "Why it stopped"}</Eyebrow>
          <div className="markdown selectable"><Markdown remarkPlugins={[remarkGfm]} components={externalLinks}>{run.report}</Markdown></div>
        </div>
      )}
      {live && <div><Button size="sm" variant="ghost" onClick={async () => { try { await post(`/routines/runs/${run.id}/stop`); void r.refresh(); } catch (e) { toastError(e); } }}>Stop this run</Button></div>}
    </div>
  );
}
