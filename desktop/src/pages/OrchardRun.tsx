// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// One Orchard run, told the way tt-orchard tells it (`GET /orchard/runs/{name}/story`,
// which runs tt-orchard's own status, narrator and lexicon):
//
// - the graft: the new model's weights (the scion) on a model that already
//   runs here (the rootstock), and the class stage 0 found;
// - the rows: the nine stages, by their orchard names, as they pass, fail or
//   are skipped on this path;
// - what it's doing now, and for a stopped run what was tried and how to go on;
// - the narration `tt-orchard watch` prints, line by line, by who did it.

import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { ProgressBar } from "@tenstorrent/vesper/progress-bar";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Badge } from "@tenstorrent/vesper/badge";
import { Typography } from "@tenstorrent/vesper/typography";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { externalLinks } from "../links";
import { ApiError, del, duration, get, type OrchardRun, post, short, text } from "../api";
import { Card, Eyebrow, toastError, toastOk, usePoll } from "../ui";
import { ask, askToStop } from "../dialog";

// ---------------------------------------------------------------- tt-orchard's words

/** tt-orchard's lexicon (orchard/lexicon.py), used until the box sends its own. */
const STAGES: { emoji: string; name: string; plain: string }[] = [
  { emoji: "🔭", name: "survey", plain: "How it differs from the model it builds on" },
  { emoji: "🧪", name: "soil test", plain: "CPU reference: what correct output looks like" },
  { emoji: "🌱", name: "graft", plain: "The new weights on one board, checked against the reference" },
  { emoji: "🌳", name: "trunk", plain: "Full model (new architectures only)" },
  { emoji: "🌿", name: "rows", plain: "Each chip configuration: 1, 2 and 4 chips" },
  { emoji: "🚪", name: "farm gate", plain: "Serving check (new architectures only)" },
  { emoji: "🍯", name: "taste test", plain: "Quality and speed (new architectures only)" },
  { emoji: "📦", name: "crate", plain: "A package that starts like any other model" },
  { emoji: "🧺", name: "harvest", plain: "The review bundle: results, risks, model card" },
];

const STATES: Record<string, { emoji: string; phrase: string; plain: string }> = {
  preparing: { emoji: "🌰", phrase: "planting", plain: "Checking the model and downloading it." },
  running: { emoji: "🌤", phrase: "growing", plain: "Working through the stages." },
  paused: { emoji: "🌧", phrase: "rain delay", plain: "Paused, holding the chips until you resume." },
  "ready-for-operator-review": { emoji: "🍎", phrase: "ripe", plain: "Every stage passed. Review the basket, then serve it." },
  blocked: { emoji: "❄️", phrase: "frost", plain: "It stopped and said why." },
  aborted: { emoji: "🍂", phrase: "fallen", plain: "Stopped before finishing. Retry picks it up." },
  "stopped-or-crashed": { emoji: "⛈", phrase: "storm damage", plain: "It stopped on an error. Retry picks it up." },
  refused: { emoji: "❄️", phrase: "frost before planting", plain: "tt-orchard's checks refused it; nothing started." },
  "not-started": { emoji: "🌰", phrase: "seed", plain: "Not started yet." },
};

const ACTOR_ICON: Record<string, string> = { orchardist: "🧑‍🌾", grafter: "✂️", "head grower": "🌳", "seasonal hand": "🧤", sheepdog: "🐕", almanac: "📖" };
const ACTOR_PLAIN: Record<string, string> = {
  orchardist: "the supervisor", grafter: "the coder model on this box", "head grower": "the large model, for hard problems",
  "seasonal hand": "the CPU model, while the chips are busy", sheepdog: "the watchdog", almanac: "the ledger",
};

/** Stage 0's outcome classes (orchard/classes.py), in plain words. */
const CLASSES: Record<string, { title: string; plain: string }> = {
  "weights-only": { title: "Weights only", plain: "Same architecture as the model it builds on; only the weights differ. The quickest graft." },
  "weights+sidecar": { title: "Weights + a sidecar", plain: "Same backbone, plus an extra weights file (a task head, say) the base doesn't have. Its output is checked on its own." },
  "full-port": { title: "Needs new model code", plain: "It doesn't fit an architecture that runs here. Orchard stops before stage 2; an agent port (Codex or Claude) is the way." },
  unknown: { title: "Couldn't tell", plain: "Stage 0 couldn't decide, so nothing unproven runs as a weights-only graft." },
};

// ---------------------------------------------------------------- the story

interface NarrationLine { ts?: string | null; actor: string; text: string; bad: boolean; stage: number | null; event?: string }
interface Story {
  facts: {
    state: string; model: string | null; hint: string;
    stage: { current: number | null; name: string | null; attempt: number };
    stages: { stage: number; name: string; status: string; wall_s: number | null; attempts: number }[];
    counts: { retries: number; escalations: number; nudges: number; pauses: number; operator_commands: number };
    pause: { reason: string; kind: string; detector: string | null; stage: number | null } | null;
    blocked: { code: string; reason: string } | null;
    leases: string[];
  } | null;
  narration: NarrationLine[];
  agent: NarrationLine[];
  activity: { text: string; since: number; stage: number | null; hardware: boolean; last_output?: string | null } | null;
  graft: { class: string | null; path: string | null; scion: string | null; rootstock: string | null; reasons: string[];
    differences: { area: string; finding: string }[]; hazards: { area: string; finding: string }[]; sidecars: number } | null;
  tried: string[] | null;
  unblock: string[] | null;
  lexicon?: { stages: Record<string, { emoji: string; name: string }>; actors: Record<string, string>; icons: Record<string, string> };
}

const clock = (ts?: string | null) => {
  if (!ts) return "";
  const t = Date.parse(ts);
  return Number.isNaN(t) ? "" : new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23" });
};
const shortModel = (m: string | null | undefined) => (m ?? "").replace(/@.*$/, "").split("/").pop() ?? "";

type Tab = "run" | "survey" | "hardware" | "reports" | "log";

export function OrchardRunView({ initial, onChanged, onDeleted }: { initial: OrchardRun; onChanged: () => void; onDeleted: () => void }) {
  const name = initial.run_name;
  const detail = usePoll(() => get<OrchardRun>(`/orchard/runs/${name}`), initial.live ? 4000 : 20000, [name]);
  const story = usePoll(() => get<Story>(`/orchard/runs/${name}/story`).catch(() => null), initial.live ? 5000 : 30000, [name]);
  const run = detail.data ?? initial;
  const s = story.data;
  const [tab, setTab] = useState<Tab>("run");
  const [focus, setFocus] = useState<number | null>(null);
  const [report, setReport] = useState<{ title: string; text: string } | null>(null);
  const p = run.progress;
  const st = STATES[run.state] ?? { emoji: "🌰", phrase: run.state, plain: "" };
  const lab = run.hardware?.lab ?? null;
  const tests = run.hardware?.tests ?? [];
  const graft = s?.graft ?? null;
  const counts = s?.facts?.counts;

  const control = async (action: string, stopServing = false) => {
    if (action === "abort" && !(await ask("Stop this bring-up?", { body: "Orchard stops cleanly and gives the chips back (on the test box too). Retry picks it up later.", confirm: "Stop", danger: true }))) return;
    try { await post(`/orchard/runs/${name}/${action}`, { stop_serving: stopServing }); detail.refresh(); story.refresh(); onChanged(); }
    catch (e) {
      if (e instanceof ApiError && e.status === 409 && /Stop/.test(e.message)) { if (await askToStop(e.message)) await control(action, true); }
      else toastError(e);
    }
  };
  const serve = async (stopServing = false) => {
    try { await post(`/orchard/runs/${name}/serve`, { stop_serving: stopServing }); toastOk("Starting it on the chips. You'll get a notification when it's ready."); }
    catch (e) {
      if (e instanceof ApiError && e.status === 409 && /Stop it first/.test(e.message)) { if (await askToStop(e.message)) await serve(true); }
      else toastError(e);
    }
  };
  const remove = async () => {
    if (!(await ask(`Delete the ${short(run.model ?? name)} run?`, { body: "Its stages, logs and review bundle are removed from the box. Downloaded weights stay in the Hugging Face cache.", confirm: "Delete", danger: true }))) return;
    try { await del(`/orchard/runs/${name}`); toastOk("Deleted."); onDeleted(); } catch (e) { toastError(e); }
  };

  const tone = run.state === "ready-for-operator-review" ? "accent" : ["blocked", "stopped-or-crashed", "refused"].includes(run.state) ? "warning" : undefined;
  return (
    <div className="stack">
      <Card tone={tone}>
        <div className="run-head">
          <div className="grow stack" style={{ gap: 4 }}>
            <div className="row wrap" style={{ gap: 8 }}>
              <span className="orchard-state" title={run.state}>{st.emoji} {st.phrase}</span>
              <span className="muted" style={{ fontSize: 12 }}>{run.state}</span>
              {lab && <Badge size="sm" variant="info" subtle>tests on {lab.host}</Badge>}
            </div>
            <Typography variant="heading-md">{short(run.model ?? name)}</Typography>
            <span className="muted">{run.refused ? `tt-orchard refused it: ${run.refused}` : run.blocked?.reason ?? p?.needs_you ?? st.plain}</span>
          </div>
          <div className="row">
            {run.state === "running" && <><Button size="sm" variant="subtle" onClick={() => control("pause")}>Pause</Button><Button size="sm" variant="ghost" onClick={() => control("abort")}>Stop</Button></>}
            {run.state === "paused" && <><Button size="sm" onClick={() => control("resume")}>Resume</Button><Button size="sm" variant="ghost" onClick={() => control("abort")}>Stop</Button></>}
            {run.state === "preparing" && <Button size="sm" variant="ghost" onClick={() => control("abort")}>Stop</Button>}
            {["blocked", "stopped-or-crashed", "refused", "aborted"].includes(run.state) && <Button size="sm" onClick={() => control("retry")}>Retry</Button>}
            {run.state === "ready-for-operator-review" && run.servable && <Button size="sm" onClick={() => serve()}>Serve it</Button>}
            {!run.live && <Button size="sm" variant="ghost" onClick={remove}>Delete</Button>}
          </div>
        </div>

        <GraftLine model={run.model} graft={graft} />

        {p && (
          <div className="stack" style={{ gap: 6 }}>
            <div className="row-between"><span className="muted" style={{ fontSize: 13 }}>{Math.round(p.overall * 100)}% of the way</span>
              <span className="muted mono" style={{ fontSize: 12 }}>{run.live && p.overall_eta_s ? `about ${duration(p.overall_eta_s)} left` : ""}</span></div>
            <ProgressBar value={p.overall * 100} aria-label="Whole run" />
          </div>
        )}
        {counts && (counts.retries + counts.escalations + counts.nudges + counts.pauses > 0) && (
          <div className="row wrap" style={{ gap: 6 }}>
            {counts.nudges > 0 && <span className="orchard-count" title="The watchdog nudged the agent">🐕 {counts.nudges} nudge{counts.nudges > 1 ? "s" : ""}</span>}
            {counts.retries > 0 && <span className="orchard-count" title="Attempts tried again">↻ {counts.retries} retr{counts.retries > 1 ? "ies" : "y"}</span>}
            {counts.escalations > 0 && <span className="orchard-count" title="Handed to a bigger model">⬆ {counts.escalations} escalation{counts.escalations > 1 ? "s" : ""}</span>}
            {counts.pauses > 0 && <span className="orchard-count">🌧 {counts.pauses} pause{counts.pauses > 1 ? "s" : ""}</span>}
          </div>
        )}
        {run.restore && run.live && <span className="muted" style={{ fontSize: 12 }}>{short(run.restore)} starts again when this ends.</span>}
      </Card>

      <OrchardRows run={run} story={s} focus={focus} onFocus={(n) => { setFocus(focus === n ? null : n); setTab("run"); }} />

      <div className="pill-tabs" role="tablist" style={{ alignSelf: "flex-start" }}>
        {(["run", "survey", "hardware", "reports", "log"] as Tab[]).map((t) => (
          <button key={t} role="tab" aria-selected={tab === t} className={tab === t ? "on" : ""} onClick={() => setTab(t)}>
            {t === "run" ? "What's happening" : t === "survey" ? "🔭 The graft" : t === "hardware" ? `Hardware tests${tests.length ? ` ${tests.length}` : ""}` : t === "reports" ? "🧺 Basket" : "Raw log"}
          </button>
        ))}
      </div>

      {tab === "run" && (
        <>
          <NowCard run={run} story={s} />
          <Narration story={s} focus={focus} onClear={() => setFocus(null)} live={run.live} />
        </>
      )}
      {tab === "survey" && <SurveyCard graft={graft} model={run.model} />}
      {tab === "hardware" && (
        <Card>
          <span className="muted" style={{ fontSize: 13 }}>
            {lab ? <>This run sends hardware tests to <b>{lab.host}</b>{lab.chips ? ` (${lab.chips} chips)` : ""}; its coder stays on this box and is never parked.</>
              : "Each hardware test takes a lease on the chips (the gate), so nothing else touches them meanwhile."}
          </span>
          {tests.length === 0 ? <span className="muted" style={{ fontSize: 13 }}>No hardware tests yet.</span> : (
            <div className="list">
              {tests.map((t, i) => (
                <div key={i} className="list-row">
                  <span style={{ width: 22 }}>{t.stage != null ? STAGES[t.stage]?.emoji : ""}</span>
                  <div className="grow">
                    <b style={{ fontSize: 13 }}>{t.config ? `${t.config}-chip test` : "Hardware test"}</b>
                    <div className="muted" style={{ fontSize: 12 }}>{t.result === "not run" ? t.detail : `on ${t.where.replace(/^lab /, "")}${t.detail ? ` · ${t.detail}` : ""}`}</div>
                  </div>
                  <Badge size="sm" subtle variant={t.result === "passed" ? "mint" : t.result === "running" ? "info" : t.result === "not run" ? "contrast" : "danger"}>{t.result}</Badge>
                </div>
              ))}
            </div>
          )}
        </Card>
      )}
      {tab === "reports" && (
        <Card>
          <span className="muted" style={{ fontSize: 13 }}>The harvest basket: what the run found, for you to review. Orchard never publishes anything; the publish commands are text for you to run if you choose.</span>
          {(run.reports ?? []).filter((r) => r !== "console").length === 0 ? <span className="muted" style={{ fontSize: 13 }}>Nothing in the basket yet; it fills at the harvest stage.</span> : (
            <div className="row wrap">
              {(run.reports ?? []).filter((r) => r !== "console").map((r) => (
                <Button key={r} size="sm" variant={report?.title === r ? "subtle" : "ghost"} onClick={async () => {
                  try { setReport({ title: r, text: await text(`/orchard/runs/${name}/report/${r}`) }); } catch (e) { toastError(e); }
                }}>{({ results: "Results", risks: "Risks", card: "Model card", publish: "Publish commands", blocked: "Why it stopped" } as Record<string, string>)[r] ?? r}</Button>
              ))}
            </div>
          )}
          {report && (report.title === "publish"
            ? <div className="console">{report.text}</div>
            : <div className="markdown selectable"><Markdown remarkPlugins={[remarkGfm]} components={externalLinks}>{report.text}</Markdown></div>)}
        </Card>
      )}
      {tab === "log" && (
        <Card>
          <span className="muted" style={{ fontSize: 13 }}>tt-orchard's own output, as printed.</span>
          <div className="console" style={{ maxHeight: 420 }}>{(run.console ?? []).join("\n") || "Nothing yet."}</div>
        </Card>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- the graft

/** `new weights → onto → the model that runs here`, with the class. */
function GraftLine({ model, graft }: { model: string | null; graft: Story["graft"] }) {
  const cls = graft?.class ? CLASSES[graft.class] : null;
  return (
    <div className="graft-line" aria-label="What is grafted onto what">
      <div className="graft-part">
        <span className="graft-tag">scion · the new weights</span>
        <b>{shortModel(graft?.scion ?? model) || "—"}</b>
      </div>
      <span className="graft-join" aria-hidden="true">🌱</span>
      <div className="graft-part">
        <span className="graft-tag">rootstock · runs here already</span>
        <b>{graft?.rootstock ? shortModel(graft.rootstock) : <span className="muted">found in the survey</span>}</b>
      </div>
      {cls && <div className="graft-class" title={cls.plain}><Badge size="sm" variant={graft?.class === "full-port" || graft?.class === "unknown" ? "warning" : "mint"} subtle>{cls.title}</Badge></div>}
    </div>
  );
}

function SurveyCard({ graft, model }: { graft: Story["graft"]; model: string | null }) {
  if (!graft) return <Card><span className="muted" style={{ fontSize: 13 }}>🔭 The survey (stage 0) compares {short(model ?? "the model")} with the nearest model this box already runs. Its findings show here when it's done.</span></Card>;
  const cls = graft.class ? CLASSES[graft.class] : null;
  return (
    <Card>
      <Eyebrow>What the survey found</Eyebrow>
      <GraftLine model={model} graft={graft} />
      {cls && <Admonition variant={graft.class === "full-port" || graft.class === "unknown" ? "warning" : "info"} subtle size="sm"><b>{cls.title}.</b> {cls.plain}</Admonition>}
      {graft.reasons.length > 0 && <div className="stack" style={{ gap: 4 }}><b style={{ fontSize: 13 }}>Why it needs a full port</b>{graft.reasons.map((r, i) => <span key={i} style={{ fontSize: 13 }}>· {r}</span>)}</div>}
      <div className="list">
        {graft.differences.map((d, i) => (
          <div key={i} className="list-row" style={{ alignItems: "flex-start" }}>
            <span className="mono muted" style={{ width: 120, fontSize: 12, flexShrink: 0 }}>{d.area.replace(/_/g, " ")}</span>
            <span style={{ fontSize: 13 }}>{d.finding}</span>
          </div>
        ))}
      </div>
      {graft.hazards.length > 0 && (
        <div className="stack" style={{ gap: 6 }}>
          <b style={{ fontSize: 13 }}>Hazards it keeps an eye on</b>
          {graft.hazards.map((h, i) => <span key={i} style={{ fontSize: 13 }}>⚠ {h.finding}</span>)}
        </div>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------- the rows

type StageState = "pass" | "fail" | "skipped" | "running" | "escalated" | "pending" | "paused";

/** The nine stages as a row of the orchard: done, now, skipped on this path, still to come. */
function OrchardRows({ run, story, focus, onFocus }: { run: OrchardRun; story: Story | null; focus: number | null; onFocus: (n: number) => void }) {
  const rows = story?.facts?.stages ?? run.stages.map((x) => ({ ...x, attempts: 1 }));
  const current = run.live ? (story?.facts?.stage.current ?? run.stage?.current ?? null) : null;
  const paused = run.state === "paused";
  const typical = run.progress?.typical_s ?? [];
  return (
    <Card>
      <div className="row-between">
        <Eyebrow>The rows</Eyebrow>
        <span className="muted" style={{ fontSize: 12 }}>Select a stage to see only its notes</span>
      </div>
      <ol className="orchard-rows">
        {STAGES.map((stage, n) => {
          const r = [...rows].reverse().find((x) => x.stage === n);
          const state: StageState = current === n ? (paused ? "paused" : "running") : (r?.status as StageState) ?? "pending";
          const label = { pass: "passed", fail: "failed", skipped: "skipped", running: "now", escalated: "escalated", pending: "", paused: "paused" }[state] ?? state;
          return (
            <li key={n} className={`orchard-stage ${state} ${focus === n ? "focus" : ""}`}>
              <button onClick={() => onFocus(n)} title={`${n} ${stage.name}: ${stage.plain}`}>
                <span className="orchard-node" aria-hidden="true">{stage.emoji}</span>
                <span className="orchard-name">{stage.name}</span>
                <span className="orchard-sub">{r?.name ?? stage.plain}</span>
                <span className="orchard-status">
                  {label}{r?.wall_s ? ` · ${duration(r.wall_s)}` : ""}{(r?.attempts ?? 1) > 1 ? ` · ${r?.attempts} tries` : ""}
                  {!r && current !== n && typical[n] ? `~${duration(typical[n])}` : ""}
                </span>
              </button>
            </li>
          );
        })}
      </ol>
    </Card>
  );
}

// ---------------------------------------------------------------- now

function NowCard({ run, story }: { run: OrchardRun; story: Story | null }) {
  const a = story?.activity;
  const f = story?.facts;
  const p = run.progress;
  const stopped = ["blocked", "stopped-or-crashed", "aborted", "refused"].includes(run.state);
  return (
    <Card>
      {run.live && (a || p?.title) && (
        <div className="stack" style={{ gap: 6 }}>
          <div className="row-between">
            <b>{a ? cap(a.text) : p?.title}</b>
            <span className="muted mono" style={{ fontSize: 12 }}>{a ? `${duration(Math.max(0, Date.now() / 1000 - a.since))} so far` : p?.eta_s ? `about ${duration(p.eta_s)} left` : ""}</span>
          </div>
          {p?.fraction != null && <ProgressBar value={p.fraction * 100} animated aria-label="Current step" />}
          {a?.last_output && <span className="console" style={{ fontSize: 11, padding: "6px 10px" }}>{a.last_output}</span>}
          {!a && p?.detail && <span className="muted" style={{ fontSize: 13 }}>{p.detail}</span>}
        </div>
      )}
      {run.state === "paused" && f?.pause && (
        <Admonition variant="warning" subtle size="sm">🌧 Paused{f.pause.stage != null ? ` in ${STAGES[f.pause.stage]?.name}` : ""}{f.pause.detector ? ` by the ${f.pause.detector.replace(/_/g, " ")} check` : ""}: {f.pause.reason}</Admonition>
      )}
      {stopped && (story?.tried?.length || story?.unblock?.length) ? (
        <div className="grid grid-2" style={{ gap: 12, alignItems: "start" }}>
          {story?.tried?.length ? (
            <div className="stack" style={{ gap: 4 }}>
              <b style={{ fontSize: 14 }}>What was tried</b>
              {story.tried.map((t, i) => <span key={i} className={/^\s/.test(t) ? "mono muted" : ""} style={{ fontSize: 12, whiteSpace: "pre-wrap" }}>{t}</span>)}
            </div>
          ) : null}
          {story?.unblock?.length ? (
            <div className="stack" style={{ gap: 6 }}>
              <b style={{ fontSize: 14 }}>How to go on</b>
              {story.unblock.map((t, i) => <span key={i} style={{ fontSize: 13 }}>{i + 1}. {t.replace(/`tt-orchard bringup [^`]+`/g, "Retry")}</span>)}
            </div>
          ) : null}
        </div>
      ) : null}
      {!run.live && !stopped && <span className="muted" style={{ fontSize: 13 }}>{(STATES[run.state] ?? { plain: "" }).plain}</span>}
      {!a && run.live && !p?.title && <span className="muted" style={{ fontSize: 13 }}>Getting started…</span>}
    </Card>
  );
}

const cap = (t: string) => t ? t[0].toUpperCase() + t.slice(1) : t;

// ---------------------------------------------------------------- narration

function Narration({ story, focus, onClear, live }: { story: Story | null; focus: number | null; onClear: () => void; live: boolean }) {
  const [all, setAll] = useState(false);
  const lines = useMemo(() => (story?.narration ?? []).filter((l) => focus == null || l.stage === focus), [story, focus]);
  const shown = all ? lines : lines.slice(-30);
  const agent = (story?.agent ?? []).filter((l) => focus == null || l.stage === focus);
  const actors = [...new Set([...(story?.narration ?? []), ...(story?.agent ?? [])].map((l) => l.actor))];
  // Like `tt-orchard watch`: newest at the bottom, kept in view as lines arrive.
  const box = useRef<HTMLOListElement>(null);
  const last = shown.length ? `${shown.length}:${shown[shown.length - 1].text}` : "";
  useEffect(() => { const el = box.current; if (el) el.scrollTop = el.scrollHeight; }, [last]);
  return (
    <Card>
      <div className="row-between">
        <Eyebrow>{focus == null ? "Field notes" : `Field notes · ${STAGES[focus]?.emoji} ${STAGES[focus]?.name}`}</Eyebrow>
        <div className="row" style={{ gap: 6 }}>
          {focus != null && <Button size="xs" variant="ghost" onClick={onClear}>All stages</Button>}
          {lines.length > 30 && <Button size="xs" variant="ghost" onClick={() => setAll((v) => !v)}>{all ? "Latest only" : `All ${lines.length}`}</Button>}
        </div>
      </div>
      {!story && <span className="muted" style={{ fontSize: 13 }}>Reading the almanac…</span>}
      {story && lines.length === 0 && <span className="muted" style={{ fontSize: 13 }}>Nothing written here yet.</span>}
      <ol className="almanac" ref={box}>
        {shown.map((l, i) => <NoteLine key={i} l={l} />)}
      </ol>
      {agent.length > 0 && (
        <div className="stack" style={{ gap: 4 }}>
          <span className="muted" style={{ fontSize: 12 }}>{ACTOR_ICON[agent[0].actor] ?? "✂️"} The {agent[0].actor}'s latest turns{live ? "" : " (its last)"}{agent[0].stage != null ? `, in ${STAGES[agent[0].stage]?.name}` : ""}</span>
          <ol className="almanac agent">{agent.map((l, i) => <NoteLine key={i} l={l} />)}</ol>
        </div>
      )}
      {actors.length > 0 && (
        <details>
          <summary className="muted" style={{ fontSize: 12, cursor: "pointer" }}>Who's who</summary>
          <div className="row wrap" style={{ gap: 12, marginTop: 6 }}>
            {actors.map((a) => <span key={a} style={{ fontSize: 12 }}>{ACTOR_ICON[a] ?? "•"} <b>{a}</b> <span className="muted">{ACTOR_PLAIN[a] ?? story?.lexicon?.actors?.[a] ?? ""}</span></span>)}
          </div>
        </details>
      )}
    </Card>
  );
}

function NoteLine({ l }: { l: NarrationLine }) {
  const sub = l.text.startsWith("  ");
  return (
    <li className={`note ${l.bad ? "bad" : ""} ${sub ? "sub" : ""}`}>
      <span className="note-time mono">{clock(l.ts)}</span>
      <span className={`note-actor a-${l.actor.replace(/\s/g, "-")}`}>{ACTOR_ICON[l.actor] ?? "•"} {l.actor}</span>
      <span className="note-text">{l.text.trim()}</span>
    </li>
  );
}
