// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Research's start box, and its small card on Home. Kept apart from the
// Research page so Home doesn't load the whole page (reports, Markdown) to show it.

import { useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextArea } from "@tenstorrent/vesper/text-area";
import { get, post } from "../api";
import { Card, Eyebrow, toastError, toastOk, usePoll } from "../ui";
import { navigate } from "../App";

export type Depth = "quick" | "standard" | "deep";

export interface Job {
  id: string; question: string; depth: Depth; parent: string | null; created: number; state: string; step: string;
  finished: number | null; error: string | null; model: string | null; search: string | null; summary: string | null; title?: string | null;
  /** The device that started it (none: the box, or from before this was kept), and whether everyone on the box sees it. */
  started_by?: string | null; started_by_name?: string | null; shared?: boolean;
}
export interface Check { ok: boolean; level: "ok" | "warn" | "block" | "wait"; why: string; model: string | null; context: number | null; search: string; search_note: string }

export const DEPTHS: { id: Depth; label: string; about: string }[] = [
  { id: "quick", label: "Quick", about: "About a dozen pages. Fastest." },
  { id: "standard", label: "Standard", about: "Around 30 pages, with a second round to fill gaps." },
  { id: "deep", label: "Deep", about: "Up to 60 pages, three rounds. Slowest, most thorough." },
];

const EXAMPLES = [
  "Which open-source vision-language models under 40B parameters run on edge hardware, how do they compare on robotics benchmarks, and which licenses allow commercial use?",
  "What changed in EU AI Act obligations for general-purpose AI models in 2026, and what do small companies have to do?",
  "Compare home battery systems available in the US by price per kWh, warranty and installation requirements.",
];

export const live = (s: string) => s === "running" || s === "queued";

export function NewResearch({ check, onStarted, onClose, compact }: { check: Check | null; onStarted: (j: Job) => void; onClose?: () => void; compact?: boolean }) {
  const [q, setQ] = useState("");
  const [depth, setDepth] = useState<Depth>("standard");
  const [busy, setBusy] = useState(false);
  const start = async () => {
    setBusy(true);
    try { const r = await post<{ job: Job }>("/research", { question: q.trim(), depth }); toastOk("Researching. You'll get a notification when the report is ready."); setQ(""); onStarted(r.job); }
    catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <Card className="start-card">
      <div className="row-between">
        <Eyebrow>What do you want to know?</Eyebrow>
        {onClose && <Button size="xs" variant="ghost" onClick={onClose}>Close</Button>}
      </div>
      <TextArea value={q} onChange={(e) => setQ(e.target.value)} height={compact ? 56 : 84}
        placeholder="A question that needs several sources, e.g. which open-source models run on edge hardware and what their licenses allow"
        onKeyDown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && q.trim().length >= 8) void start(); }} />
      {!compact && !q && (
        <div className="stack" style={{ gap: 6 }}>
          <span className="eyebrow">Try one of these</span>
          {EXAMPLES.map((x) => <button key={x} className="research-example" onClick={() => setQ(x)}>{x}</button>)}
        </div>
      )}
      <div className="row-between wrap" style={{ gap: 8 }}>
        <div className="pill-tabs" role="radiogroup" aria-label="How deep">
          {DEPTHS.map((d) => <button key={d.id} role="radio" aria-checked={depth === d.id} className={depth === d.id ? "on" : ""} title={d.about} onClick={() => setDepth(d.id)}>{d.label}</button>)}
        </div>
        <Button disabled={busy || q.trim().length < 8 || check?.ok === false} onClick={start}>{busy ? "Starting…" : "Research it"}</Button>
      </div>
      <span className="muted" style={{ fontSize: 12 }}>{DEPTHS.find((d) => d.id === depth)?.about} It runs in the background; you'll get a notification.</span>
    </Card>
  );
}

export function ResearchCard() {
  const list = usePoll(() => get<{ jobs: Job[]; check: Check }>("/research").catch(() => null), 15000);
  const jobs = list.data?.jobs ?? [];
  const running = jobs.find((j) => live(j.state));
  const last = jobs.find((j) => j.state === "done");
  return (
    <div className="stack" style={{ gap: 8 }}>
      <NewResearch compact check={list.data?.check ?? null} onStarted={(j) => { history.pushState(null, "", `/app/research#${j.id}`); window.dispatchEvent(new PopStateEvent("popstate")); }} />
      {(running || last) && (
        <button className="link-button" style={{ alignSelf: "flex-start" }} onClick={() => navigate("research")}>
          {running ? `Researching now: ${running.question.slice(0, 60)}${running.question.length > 60 ? "…" : ""}` : `Latest report: ${last!.question.slice(0, 60)}${last!.question.length > 60 ? "…" : ""}`}
        </button>
      )}
    </div>
  );
}
