// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The agent port, one of the ways to bring up a model (see BringupWays): a
// full port of a new architecture by an agent (Codex or Claude Code run
// tt-metal's fast-models-fast stages). Start from the model: a quick look says
// whether it needs a port at all; when TT-Metal already runs its architecture,
// Orchard is the better way, and this hands the model over.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Select } from "@tenstorrent/vesper/select";
import { Typography } from "@tenstorrent/vesper/typography";
import { Icon } from "@tenstorrent/vesper/icons";
import { ApiError, get, post, short } from "../api";
import { Card, Eyebrow, toastError, toastOk, usePoll } from "../ui";
import { askToStop } from "../dialog";
import { CodexRunDetail, type CodexRun, type Engine, EngineCards, type Survey } from "./BringupCodex";
import { HANDOFF_KEY } from "./Orchard";
import { openCommunity } from "./CommunityModels";
import { navigate } from "../App";

/** The Bring-up page's model check hands a model over here when it needs a full port. */
export const AGENT_HANDOFF_KEY = "tt.bringup.model";

interface RunChip { key: string; engine: "Codex" | "Claude"; model: string; live: boolean; started: number; label: string; tone: "live" | "needs" | "done" | "stopped" }

/** The agent port's part of the Bring-up page. */
export function AgentPortBody() {
  const agents = usePoll(() => get<{ runs: CodexRun[] }>("/bringup/codex/runs").catch(() => ({ runs: [] })), 4000);
  const engines = usePoll(() => get<{ engines: Engine[] }>("/bringup/engines").catch(() => ({ engines: [] })), 60000);
  const [selected, setSelected] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [handoff] = useState<string | null>(() => {
    try { const m = localStorage.getItem(AGENT_HANDOFF_KEY); localStorage.removeItem(AGENT_HANDOFF_KEY); return m; } catch { return null; }
  });
  const agentRuns = agents.data?.runs ?? [];
  const busy = agentRuns.some((r) => r.live);

  const runs: RunChip[] = [
    ...agentRuns.map((r): RunChip => ({
      key: `agent:${r.id}`, engine: r.engine === "claude" ? "Claude" : "Codex", model: r.model, live: r.live, started: r.started,
      label: r.state === "waiting_for_you" ? "needs you" : r.state === "waiting_for_chips" ? "waiting for chips" : r.live ? `stage ${r.current ?? 1} of ${r.total}` : r.state,
      tone: r.state === "waiting_for_you" ? "needs" : r.live ? "live" : r.state === "finished" ? "done" : "stopped",
    })),
  ].sort((a, b) => Number(b.live) - Number(a.live) || b.started - a.started);

  const pick = runs.find((r) => r.key === selected) ?? runs.find((r) => r.live) ?? runs[0] ?? null;
  const agentRun = pick?.key.startsWith("agent:") ? agentRuns.find((r) => `agent:${r.id}` === pick.key) : null;
  const showStart = starting || runs.length === 0 || !!handoff;
  const refresh = () => { agents.refresh(); };

  return (
    <>
      {showStart && !busy && (
        <StartBringup initial={handoff ?? ""} engines={engines.data?.engines ?? []} canClose={runs.length > 0}
          onClose={() => setStarting(false)}
          onStarted={(key) => { setStarting(false); refresh(); if (key) setSelected(key); }} />
      )}

      {runs.length > 0 && (
        <div className="row-between wrap">
        <div className="run-chips" role="tablist" aria-label="Bring-up runs">
          {runs.map((r) => (
            <button key={r.key} role="tab" aria-selected={pick?.key === r.key} className={`run-chip ${pick?.key === r.key ? "on" : ""}`} onClick={() => setSelected(r.key)}>
              <span className={`run-dot ${r.tone}`} />
              <b>{short(r.model)}</b>
              <span className="muted">{r.engine} · {r.label}</span>
            </button>
          ))}
        </div>
        {!showStart && !busy && <Button size="sm" iconLeft={<Icon kind="add" />} onClick={() => setStarting(true)}>Port another model</Button>}
        </div>
      )}

      {agentRun && <CodexRunDetail key={agentRun.id} initial={agentRun} onChanged={refresh} onDeleted={() => { setSelected(null); refresh(); }} />}
    </>
  );
}

// ---------------------------------------------------------------- start

const POLICIES = [
  { value: "smart", label: "Ask me about risky things", hint: "Tests, builds and edits in the repo run on their own; chip resets, sudo, installs, pushes and anything outside the repo wait for you." },
  { value: "ask", label: "Ask me about everything", hint: "Every command waits for you." },
  { value: "auto", label: "Don't ask", hint: "Runs unattended, as fast-models-fast does by default." },
];

function StartBringup({ initial, engines, canClose, onClose, onStarted }: {
  initial: string; engines: Engine[]; canClose: boolean; onClose: () => void; onStarted: (key?: string) => void;
}) {
  const [model, setModel] = useState(initial);
  const [looking, setLooking] = useState(false);
  const [survey, setSurvey] = useState<Survey | null>(null);
  const [engine, setEngine] = useState<string | null>(null);
  const [otherWays, setOtherWays] = useState(false);
  const [policy, setPolicy] = useState("smart");
  const [launching, setLaunching] = useState(false);

  const look = async () => {
    setLooking(true); setSurvey(null); setEngine(null); setOtherWays(false);
    try {
      const r = await post<Survey>("/bringup/survey", { model: model.trim() });
      setSurvey(r);
      setEngine(r.recommend === "none" || r.recommend === "community" ? null : r.recommend);
    } catch (e) { toastError(e); } finally { setLooking(false); }
  };
  useEffect(() => { if (initial) void look(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const chosen = engines.find((e) => e.id === engine);
  const name = chosen?.name ?? (engine === "orchard" ? "Orchard" : engine === "claude" ? "Claude Code" : "Codex");

  const startAgent = async (stopServing = false) => {
    if (!survey || !engine) return;
    setLaunching(true);
    try {
      const r = await post<{ id: string }>("/bringup/codex/runs", { model: survey.model, engine, policy, stop_serving: stopServing });
      toastOk(`${name} is bringing up ${short(survey.model)}. You'll get a notification when it needs you.`);
      onStarted(`agent:${r.id}`);
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { needs_stop?: boolean })?.needs_stop) {
        if (await askToStop(e.message, "It starts again when the bring-up ends.")) await startAgent(true);
      } else toastError(e);
    } finally { setLaunching(false); }
  };

  return (
    <Card className="start-card">
      <div className="row-between">
        <Eyebrow>Port a model with an agent</Eyebrow>
        {canClose && <Button size="xs" variant="ghost" onClick={onClose}>Close</Button>}
      </div>
      <div className="row">
        <div className="grow"><TextInput value={model} onChange={(e) => { setModel(e.target.value); setSurvey(null); }}
          placeholder="A Hugging Face model, e.g. google/gemma-3-4b-it" onKeyDown={(e) => { if (e.key === "Enter" && model.trim()) look(); }} /></div>
        <Button variant={survey ? "subtle" : "primary"} disabled={!model.trim() || looking} onClick={look}>{looking ? "Looking…" : "Look at it"}</Button>
      </div>

      {survey && (
        <div className="start-result">
          <div className="survey-facts">
            <b>{survey.architecture ?? "Unknown architecture"}</b>
            {survey.params_b ? <span>{survey.params_b.toFixed(1)}B parameters</span> : null}
            {survey.license ? <span>{String(survey.license)} license</span> : null}
            {survey.gated ? <span>gated on Hugging Face</span> : null}
          </div>
          <Typography variant="copy-md">{survey.why}</Typography>

          {survey.recommend === "community" && survey.community_package && (
            <div><Button variant="subtle" onClick={() => openCommunity(survey.community_package!)}>Open the community package</Button></div>
          )}
          {survey.recommend !== "none" && engine && (
            <>
              {chosen && !chosen.ready ? (
                <Admonition variant="warning" subtle size="sm">{name}: {chosen.why_not}</Admonition>
              ) : engine === "orchard" ? (
                <div className="start-go">
                  <div className="muted" style={{ fontSize: 13 }}>Orchard grafts the new weights onto the architecture that already runs here, with agents on this box's own models: no account, and no full port needed.</div>
                  <div><Button onClick={() => { try { localStorage.setItem(HANDOFF_KEY, survey.model); } catch { /* private */ } navigate("orchard"); }}>Bring it up in Orchard</Button></div>
                </div>
              ) : (
                <div className="start-go">
                  <div className="muted" style={{ fontSize: 13 }}>
                    {name} writes the port to <span className="mono">{survey.model_dir}</span> in tt-metal, through eleven checked stages. It holds all four chips while it runs; expect several hours.
                  </div>
                  <div className="row wrap" style={{ gap: 10 }}>
                    <Button disabled={launching} onClick={() => startAgent()}>{launching ? "Starting…" : `Bring it up with ${name}`}</Button>
                    <div style={{ width: 260 }}>
                      <Select size="sm" value={policy} onValueChange={(v) => v && setPolicy(v)} aria-label="When to ask you"
                        options={POLICIES.map((p) => ({ value: p.value, label: p.label }))} />
                    </div>
                  </div>
                  <span className="muted" style={{ fontSize: 12 }}>{POLICIES.find((p) => p.value === policy)?.hint}</span>
                </div>
              )}
              <button className="link-button" style={{ alignSelf: "flex-start" }} onClick={() => setOtherWays((v) => !v)}>
                {otherWays ? "Hide the other ways" : "Other ways to bring it up"}
              </button>
              {otherWays && <EngineCards engines={engines} recommend={survey.recommend} selected={engine} onSelect={setEngine} />}
            </>
          )}
        </div>
      )}
    </Card>
  );
}
