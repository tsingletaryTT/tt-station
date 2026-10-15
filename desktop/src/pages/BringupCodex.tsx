// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The Codex engine in Bring-up: tt-metal's fast-models-fast pipeline, run by
// the box (see the agent's bringup.rs). Shows the eleven stages and their
// checks, what Codex is doing right now, and anything risky it's waiting on
// you for; plus the developer exits: the exact CLI command, the Codex thread
// to resume in a terminal, and the log folder.

import { useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { ProgressBar } from "@tenstorrent/vesper/progress-bar";
import { Typography } from "@tenstorrent/vesper/typography";
import { Icon } from "@tenstorrent/vesper/icons";
import { TextArea } from "@tenstorrent/vesper/text-area";
import { ApiError, del, duration, get, post, short } from "../api";
import { Card, copyText, Eyebrow, toastError, toastOk, usePoll } from "../ui";
import { ask, askToStop } from "../dialog";

export interface Engine {
  id: "orchard" | "codex" | "claude";
  installed?: boolean;
  signed_in?: boolean;
  name: string;
  ready: boolean;
  why_not: string | null;
  blurb: string;
  estimate: string;
  account: string | null;
  detail?: { repo: string; branch: string | null; stages: string[]; signed_in: boolean };
}

export interface Survey {
  model: string;
  architecture: string | null;
  params_b: number | null;
  gated: unknown;
  license: unknown;
  same_architecture: string[];
  already_in_catalog: string | null;
  /** A community package that already runs it, when there is one. */
  community_package?: string | null;
  recommend: "orchard" | "codex" | "community" | "none";
  why: string;
  model_dir: string;
}

interface Usage { tokens_in: number; tokens_out: number; cost_usd: number | null; started_at?: number | null; last_at?: number | null }
interface Stage {
  n: number; title: string; state: "waiting" | "running" | "done" | "advisory" | "failed" | "stopped";
  goal: string | null; check: string | null; gate: boolean; blocked_reason: string | null; usage: Usage | null;
}
interface Event { at: string; kind: "command" | "edit" | "says" | "goal" | "error" | "search" | "you"; text: string; exit?: number | null; ms?: number | null; output?: string | null }
interface Approval { id: string; kind: string; why: string; command: string | null; cwd: string | null; reason: string | null; grant_root: string | null; created: number }

export interface Finish {
  can_package: boolean;
  /** Serve the port now with native vLLM, straight from tt-metal (stage 9 passed). */
  can_try: boolean;
  /** Stage 9 recorded the exact serve command (else TT Station uses the plugin's usual launch). */
  serve_recipe: boolean;
  packaging: boolean;
  packaged: boolean;
  repo_id: string | null;
  staged_dir: string | null;
  report: string | null;
  publishing: boolean;
  published: boolean;
  publish_log: string;
  port_done: number;
}

export interface CodexRun {
  id: string;
  engine: "codex" | "claude";
  finish: Finish;
  model: string;
  model_dir: string;
  state: "running" | "waiting_for_you" | "waiting_for_chips" | "finished" | "failed" | "stopped";
  chips_held_by: string | null;
  last_output: string[];
  live: boolean;
  policy: "ask" | "smart" | "auto";
  started: number;
  finished: number | null;
  exit: number | null;
  exit_meaning: string | null;
  stages: Stage[];
  done: number;
  total: number;
  current: number | null;
  events: Event[];
  approvals: Approval[];
  /** A stage the agent said it can't finish without a person. */
  blocked: { stage: number; title: string; reason: string | null } | null;
  usage: Usage;
  log_dir: string;
  code_dir: string;
  cli: string;
  resume_in_terminal: string | null;
}

/** The three engines, with the recommended one marked. */
export function EngineCards({ engines, recommend, selected, onSelect }: {
  engines: Engine[]; recommend: string; selected: string | null; onSelect: (id: string) => void;
}) {
  return (
    <div className="engine-cards">
      {engines.map((e) => (
        <button key={e.id} className={`engine ${selected === e.id ? "on" : ""}`} onClick={() => onSelect(e.id)} aria-pressed={selected === e.id}>
          <div className="row-between">
            <b>{e.name}</b>
            {e.id === recommend ? <Badge size="sm" variant="mint" subtle>Recommended</Badge>
              : e.id === "claude" && e.ready ? <Badge size="sm" variant="info" subtle>Also a full port</Badge>
              : !e.ready ? <Badge size="sm" variant="contrast" subtle>Not ready</Badge> : null}
          </div>
          <span className="muted" style={{ fontSize: 13 }}>{e.blurb}</span>
          <span className="muted mono" style={{ fontSize: 11 }}>{e.estimate}{e.account ? ` · ${e.account} account` : " · no account"}</span>
          {!e.ready && e.why_not && <span style={{ fontSize: 12, color: "var(--vesper-text-warning, var(--vesper-amber-600))" }}>{e.why_not}</span>}
          {e.id === "claude" && e.installed && !e.signed_in && (
            <span role="button" tabIndex={0} className="link-button" style={{ fontSize: 12 }}
              onClick={async (ev) => { ev.stopPropagation(); try { await post("/bringup/claude/login"); toastOk("A terminal opened on the box: sign in there, then come back."); } catch (err) { toastError(err); } }}>
              Sign in to Claude Code
            </span>
          )}
        </button>
      ))}
    </div>
  );
}

/** 1234567 → "1.2M". */
function tokens(n: number): string {
  return n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n);
}

/**
 * What the agent has used: the tokens it has read so far (mostly its own
 * cached context), and the cost at API prices once a turn has ended.
 */
function usageText(u: Usage | null | undefined): string {
  if (!u) return "";
  const parts = [u.tokens_in > 0 ? `${tokens(u.tokens_in)} tokens read` : "", u.cost_usd != null ? `$${u.cost_usd.toFixed(2)} at API prices` : ""];
  return parts.filter(Boolean).join(" · ");
}

/** Tell the agent something: answer what it's blocked on, or steer it. */
function TellBox({ run, agent, blocked, onSent }: { run: CodexRun; agent: string; blocked?: boolean; onSent: () => void }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const send = async (stopServing = false) => {
    if (!text.trim()) return;
    if (run.live && !stopServing && !(await ask(`Interrupt ${agent} with this?`, {
      body: `${agent} stops what it's doing, the chips are reset (about a minute), and it carries on with this stage in the same conversation, with your note first.`,
      confirm: "Interrupt and tell it",
    }))) return;
    setBusy(true);
    try {
      await post(`/bringup/codex/runs/${run.id}/tell`, { text, stop_serving: stopServing });
      setText("");
      toastOk(run.live ? `${agent} has your note and is carrying on.` : `${agent} has your note. The bring-up is continuing.`);
      onSent();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { needs_stop?: boolean })?.needs_stop) {
        if (await askToStop(e.message, "It starts again when the bring-up ends.")) { setBusy(false); await send(true); return; }
      } else toastError(e);
    } finally { setBusy(false); }
  };
  const placeholder = blocked
    ? `Answer ${agent}: the decision, access or fix it needs`
    : run.live ? `Tell ${agent} something (e.g. "use bf8 for the MLP", "skip the long sweep")` : `A note for ${agent} when it resumes (optional)`;
  return (
    <div className="stack" style={{ gap: 8 }}>
      <TextArea value={text} onChange={(e) => setText(e.target.value)} height={blocked ? 88 : 64} placeholder={placeholder}
        onKeyDown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void send(); } }} />
      <div className="row-between">
        <span className="muted" style={{ fontSize: 12 }}>
          {run.live ? "Interrupts the current step; nothing is lost." : "It continues this stage in the same conversation."}
        </span>
        <Button size="sm" disabled={busy || !text.trim()} onClick={() => send()}>
          {busy ? "Sending…" : blocked ? "Answer and continue" : run.live ? "Tell it" : "Resume with this note"}
        </Button>
      </div>
    </div>
  );
}

/** One thing the agent did; a command opens to show its output. */
function FeedEvent({ e }: { e: Event }) {
  const icon = e.kind === "command" ? (e.exit == null || e.exit === 0 ? "success" : "warning")
    : e.kind === "edit" ? "document" : e.kind === "says" ? "chat" : e.kind === "error" ? "error" : e.kind === "you" ? "user" : "info";
  const meta = e.kind === "command" && (e.exit != null || e.ms)
    ? <span className="muted mono" style={{ fontSize: 11 }}> {e.exit != null ? `exit ${e.exit}` : "running"}{e.ms ? ` · ${duration(Math.round(e.ms / 1000))}` : ""}</span>
    : null;
  return (
    <div className={`codex-event ${e.kind}`}>
      <Icon kind={icon} width={14} height={14} />
      <div className="grow">
        {e.kind === "command" && e.output ? (
          <details>
            <summary><code>{e.text}</code>{meta}</summary>
            <div className="console" style={{ maxHeight: 220, fontSize: 11, marginTop: 4 }}>{e.output}</div>
          </details>
        ) : e.kind === "command" ? <><code>{e.text}</code>{meta}</>
          : e.kind === "you" ? <span><b>You:</b> {e.text}</span>
          : <span>{e.text}</span>}
      </div>
    </div>
  );
}

const STATE_TEXT: Record<CodexRun["state"], [string, string]> = {
  waiting_for_chips: ["Waiting for the chips", "Another job holds them; it starts as soon as they're free."],
  running: ["Running", "Working through the stages."],
  waiting_for_you: ["Needs you", "Codex is waiting for you to allow or deny something."],
  finished: ["Finished", "Every stage passed its checks."],
  failed: ["Stopped on a problem", ""],
  stopped: ["Stopped", "Resume picks up where it left off, in the same Codex thread."],
};

const STAGE_BADGE: Record<Stage["state"], [string, "mint" | "info" | "warning" | "danger" | "contrast"]> = {
  done: ["passed", "mint"], advisory: ["passed with notes", "warning"], failed: ["failed check", "danger"],
  running: ["now", "info"], stopped: ["stopped", "warning"], waiting: ["", "contrast"],
};

type Tab = "activity" | "stages" | "finish" | "details";

export function CodexRunDetail({ initial, onChanged, onDeleted }: { initial: CodexRun; onChanged: () => void; onDeleted: () => void }) {
  const poll = usePoll(() => get<CodexRun>(`/bringup/codex/runs/${initial.id}`), initial.live ? 3000 : 20000, [initial.id]);
  const run = poll.data ?? initial;
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<Tab>(() => (run.finish.packaged || run.state === "finished" ? "finish" : "activity"));
  const now = Math.floor(Date.now() / 1000);
  const elapsed = (run.finished ?? now) - run.started;
  const agent = run.engine === "claude" ? "Claude" : "Codex";
  const current = run.stages.find((x) => x.n === run.current);

  const act = async (action: "stop" | "resume" | "free-chips", stopServing = false) => {
    if (action === "stop" && !(await ask("Stop this bring-up?", { body: `${agent} stops mid-stage and the chips are given back. Resume picks up in the same conversation.`, confirm: "Stop", danger: true }))) return;
    setBusy(true);
    try {
      await post(`/bringup/codex/runs/${run.id}/${action}`, { stop_serving: stopServing });
      if (action === "free-chips") toastOk("The chips are reset and free. The bring-up is starting.");
      poll.refresh(); onChanged();
    }
    catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { needs_stop?: boolean })?.needs_stop) {
        if (await askToStop(e.message, "It starts again when the bring-up ends.")) await act(action, true);
      } else toastError(e);
    } finally { setBusy(false); }
  };
  const remove = async () => {
    if (!(await ask(`Delete the ${short(run.model)} run?`, { body: `Its logs are removed from TT Station. The code ${agent} wrote stays in tt-metal (${run.model_dir}).`, confirm: "Delete", danger: true }))) return;
    try { await del(`/bringup/codex/runs/${run.id}`); toastOk("Deleted."); onDeleted(); } catch (e) { toastError(e); }
  };
  const decide = async (a: Approval, decision: "accept" | "acceptForSession" | "decline") => {
    try { await post(`/bringup/codex/runs/${run.id}/approvals/${a.id}`, { decision }); poll.refresh(); }
    catch (e) { toastError(e); }
  };
  const copy = (text: string, what: string) => void copyText(text, `${what} copied.`);

  // What to say under the title: always why, never just "stopped".
  const why = run.state === "waiting_for_chips"
    ? (run.chips_held_by
      ? `Waiting for the chips: ${run.chips_held_by} has them. Make them available to start now, or it starts by itself when they're free.`
      : "Waiting for the chips: they need a reset after the last model. Make them available to start now.")
    : run.blocked ? `${agent} needs an answer from you to carry on.`
    : run.state === "failed" ? run.exit_meaning ?? "It stopped on a problem."
    : run.state === "stopped" ? (run.exit != null && run.exit_meaning ? run.exit_meaning : "You stopped it. Resume picks up in the same conversation.")
    : run.live && current ? `Stage ${current.n} of ${run.total}: ${current.title}`
    : STATE_TEXT[run.state][1];

  return (
    <div className="stack">
      <Card tone={run.state === "finished" ? "accent" : run.state === "failed" || run.state === "waiting_for_you" ? "warning" : undefined}>
        <div className="run-head">
          <div className="grow">
            <div className="row" style={{ gap: 8 }}>
              <Badge size="sm" variant="info" subtle>{run.engine === "claude" ? "Claude Code" : "Codex"}</Badge>
              <span className="muted" style={{ fontSize: 13 }}>{STATE_TEXT[run.state][0]} · {duration(elapsed)}{usageText(run.usage) && ` · ${usageText(run.usage)}`}</span>
            </div>
            <Typography variant="heading-md">{short(run.model)}</Typography>
            <span className="muted">{why}</span>
          </div>
          <div className="row">
            {run.state === "waiting_for_chips" && <Button size="sm" disabled={busy} onClick={() => act("free-chips")}>{busy ? "Freeing the chips…" : "Make the chips available"}</Button>}
            {run.live && <Button size="sm" variant="ghost" disabled={busy} onClick={() => act("stop")}>Stop</Button>}
            {!run.live && run.state !== "finished" && !run.blocked && <Button size="sm" disabled={busy} onClick={() => act("resume")}>Resume</Button>}
            {!run.live && <Button size="sm" variant="ghost" onClick={remove}>Delete</Button>}
          </div>
        </div>
        <ProgressBar value={(run.done / Math.max(1, run.total)) * 100} aria-label="Stages done" />
        {!run.live && run.state !== "finished" && !run.blocked && run.last_output.length > 0 && (
          <div className="console" style={{ maxHeight: 120, fontSize: 12 }}>{run.last_output.join("\n")}</div>
        )}
      </Card>

      {run.blocked && (
        <Card tone="warning">
          <Eyebrow>{agent} is stuck on stage {run.blocked.stage}: {run.blocked.title}</Eyebrow>
          <span>{run.blocked.reason ?? `${agent} said it can't finish this stage without a person.`}</span>
          <TellBox run={run} agent={agent} blocked onSent={() => { poll.refresh(); onChanged(); }} />
          <span className="muted" style={{ fontSize: 12 }}>
            Or <span role="button" tabIndex={0} className="link-button" onClick={() => act("resume")}>resume without an answer</span>, to have it try again as is.
          </span>
        </Card>
      )}

      {run.approvals.map((a) => (
        <Card key={a.id} tone="warning">
          <div className="row-between"><Eyebrow>{agent} wants to {a.kind === "command" ? "run" : a.kind === "file change" ? "change files" : "use " + a.kind}</Eyebrow><span className="muted" style={{ fontSize: 12 }}>{a.why}</span></div>
          {a.command && <div className="console" style={{ maxHeight: 160 }}>{a.command}</div>}
          {(a.cwd || a.grant_root || a.reason) && <span className="muted mono" style={{ fontSize: 12 }}>{a.reason ? `${a.reason} · ` : ""}{a.grant_root ?? a.cwd}</span>}
          <div className="row">
            <Button size="sm" onClick={() => decide(a, "accept")}>Allow</Button>
            <Button size="sm" variant="subtle" onClick={() => decide(a, "acceptForSession")} title="Allow this and anything like it for the rest of this stage">Allow for this stage</Button>
            <Button size="sm" variant="ghost" onClick={() => decide(a, "decline")}>Deny</Button>
          </div>
        </Card>
      ))}

      <div className="pill-tabs" role="tablist" style={{ alignSelf: "flex-start" }}>
        {([["activity", "Activity"], ["stages", `Stages ${run.done}/${run.total}`], ["finish", "Finish line"], ["details", "Details"]] as [Tab, string][]).map(([t, label]) => (
          <button key={t} role="tab" aria-selected={tab === t} className={tab === t ? "on" : ""} onClick={() => setTab(t)}>{label}</button>
        ))}
      </div>

      {tab === "activity" && (
        <Card>
          {(run.live && run.state !== "waiting_for_chips" || (!run.live && run.state !== "finished" && !run.blocked)) && (
            <TellBox run={run} agent={agent} onSent={() => { poll.refresh(); onChanged(); }} />
          )}
          <div className="codex-feed">
            {run.events.length === 0 && <span className="muted" style={{ fontSize: 13 }}>{run.live ? (run.state === "waiting_for_chips" ? "Nothing yet: waiting for the chips." : "Starting the stage…") : "Nothing recorded."}</span>}
            {[...run.events].reverse().map((e, i) => <FeedEvent key={i} e={e} />)}
          </div>
        </Card>
      )}

      {tab === "stages" && (
        <Card>
          <div className="list">
            {run.stages.map((st) => (
              <div key={st.n} className="list-row" style={{ opacity: st.state === "waiting" ? 0.55 : 1 }}>
                <span className="mono muted" style={{ width: 20, fontSize: 12 }}>{st.n}</span>
                <div className="grow">
                  <b style={{ fontSize: 13 }}>{st.title}</b>
                  {st.gate && <div className="muted" style={{ fontSize: 11 }}>{st.check ? `check: ${st.check}` : "checked by a gate"}</div>}
                  {st.usage?.started_at != null && (
                    <div className="muted mono" style={{ fontSize: 11 }}>
                      {duration(((st.state === "running" && run.live ? now : st.usage.last_at) ?? st.usage.started_at) - st.usage.started_at)}
                      {usageText(st.usage) && ` · ${usageText(st.usage)}`}
                    </div>
                  )}
                  {st.blocked_reason && <div style={{ fontSize: 12 }}>Blocked: {st.blocked_reason}</div>}
                </div>
                {STAGE_BADGE[st.state][0] && <Badge size="sm" variant={STAGE_BADGE[st.state][1]} subtle>{st.state === "running" && !run.live ? "unfinished" : STAGE_BADGE[st.state][0]}</Badge>}
              </div>
            ))}
          </div>
        </Card>
      )}

      {tab === "finish" && <FinishLine run={run} onChanged={() => { poll.refresh(); onChanged(); }} />}

      {tab === "details" && (
        <Card>
          <div className="list">
            <div className="list-row"><span className="muted grow">Asks you about</span><span>{run.policy === "smart" ? "risky things" : run.policy === "ask" ? "everything" : "nothing"}</span></div>
            <div className="list-row"><span className="muted grow">Code</span><span className="mono" style={{ fontSize: 12 }}>{run.code_dir}</span></div>
            <div className="list-row"><span className="muted grow">Logs</span><span className="mono" style={{ fontSize: 12 }}>{run.log_dir}</span></div>
          </div>
          <div className="row wrap">
            <Button size="sm" variant="subtle" onClick={() => copy(run.cli, "The command")}>Copy the CLI command</Button>
            {run.resume_in_terminal && <Button size="sm" variant="subtle" onClick={() => copy(run.resume_in_terminal!, "The resume command")}>Copy “resume in a terminal”</Button>}
            <Button size="sm" variant="ghost" onClick={() => copy(run.code_dir, "The code folder")}>Copy code path</Button>
          </div>
          {run.last_output.length > 0 && <div className="console" style={{ maxHeight: 200, fontSize: 12 }}>{run.last_output.join("\n")}</div>}
        </Card>
      )}
    </div>
  );
}

/** After the port: Package (tt-model) → Serve → Publish. */
function FinishLine({ run, onChanged }: { run: CodexRun; onChanged: () => void }) {
  const f = run.finish;
  const [showReport, setShowReport] = useState(false);
  const act = async (action: "try" | "package" | "serve" | "publish" | "list", stopServing = false) => {
    try {
      await post(`/bringup/codex/runs/${run.id}/${action}`, { stop_serving: stopServing });
      toastOk({ try: "Starting it. The first start takes a while; you'll get a notification when you can chat with it.",
        package: "Packaging started. The container build takes hours; you'll get a notification.", serve: "Starting it on the chips.",
        publish: "Pushing to Hugging Face, private.", list: "Listing it in the community catalog." }[action]);
      onChanged();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { needs_stop?: boolean })?.needs_stop) { if (await askToStop(e.message)) act(action, true); }
      else toastError(e);
    }
  };
  if (!f.can_package && !f.packaging && !f.packaged && f.port_done < 9) {
    return (
      <Card>
        <Eyebrow>Finish line</Eyebrow>
        <span className="muted" style={{ fontSize: 13 }}>Once the vLLM stage (9) passes, chat with it right away, package it with tt-model, serve it here, and publish it if you want to share it.</span>
      </Card>
    );
  }
  const step = (n: number, title: string, done: boolean, body: React.ReactNode) => (
    <div className={`finish-step ${done ? "done" : ""}`}>
      <span className="finish-n">{done ? "✓" : n}</span>
      <div className="grow stack" style={{ gap: 6 }}><b>{title}</b>{body}</div>
    </div>
  );
  return (
    <Card tone={f.packaged ? "accent" : undefined}>
      <Eyebrow>Finish line</Eyebrow>
      {step(1, "Try it now", false, <div className="row"><span className="muted grow" style={{ fontSize: 13 }}>
          Chat with your new model right away, straight from the code the agent wrote. No need to wait for packaging.</span>
        <Button size="sm" variant="subtle" disabled={!f.can_try} onClick={() => act("try")}>Try it now</Button></div>)}
      {step(2, "Package with tt-model", f.packaged, f.packaging ? <span className="muted" style={{ fontSize: 13 }}>Writing tt-model.yaml, building the container and proving it serves on the chips…</span>
        : f.packaged ? <span className="muted mono" style={{ fontSize: 12 }}>{f.repo_id} · staged at {f.staged_dir}</span>
        : <div className="row"><span className="muted grow" style={{ fontSize: 13 }}>The same agent writes the package's tt-model.yaml with tt-model's own skills, builds the container (hours) and proves it on the chips. Staged here only; nothing is pushed.</span>
            <Button size="sm" disabled={!f.can_package} onClick={() => act("package")}>Package it</Button></div>)}
      {f.report && <div><Button size="xs" variant="ghost" onClick={() => setShowReport((v) => !v)}>{showReport ? "Hide" : "What was verified"}</Button>
        {showReport && <div className="console" style={{ maxHeight: 280 }}>{f.report}</div>}</div>}
      {step(3, "Serve it here", false, <div className="row"><span className="muted grow" style={{ fontSize: 13 }}>Run the package on this box, so Chat and Tori can use it.</span>
        <Button size="sm" variant="subtle" disabled={!f.packaged} onClick={() => act("serve")}>Serve it</Button></div>)}
      {step(4, "Publish", f.published, <div className="stack" style={{ gap: 6 }}>
        <div className="row"><span className="muted grow" style={{ fontSize: 13 }}>Push it to your Hugging Face account as a private repo. Listing it in the public community catalog is a separate step.</span>
          <Button size="sm" variant="subtle" disabled={!f.packaged || f.publishing}
            onClick={async () => { if (await ask(`Push ${f.repo_id} to Hugging Face?`, { body: "It goes to your Hugging Face account as a private repo. Only you can see it.", confirm: "Push privately" })) act("publish"); }}>
            {f.publishing ? "Pushing…" : "Push (private)"}</Button>
          <Button size="sm" variant="ghost" disabled={!f.published}
            onClick={async () => { if (await ask(`List ${f.repo_id} publicly?`, { body: "This makes the repo public and lists it in the tt-model community catalog, where anyone can find and pull it.", confirm: "Make it public", danger: true })) act("list"); }}>
            List publicly</Button></div>
        {f.publish_log && <div className="console" style={{ maxHeight: 140 }}>{f.publish_log.split("\n").slice(-8).join("\n")}</div>}
      </div>)}
    </Card>
  );
}
