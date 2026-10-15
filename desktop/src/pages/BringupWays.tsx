// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Bring-up: getting a model that isn't in Models running on this box. There
// are three separate ways, and which one fits depends on the model:
//
// - Orchard grafts new weights onto an architecture this box already runs
//   (fine-tunes, post-trains), with agents on this box's own models;
// - an agent port (Codex or Claude Code) writes new model code for a new
//   architecture, stage by stage;
// - a community package: someone has already done it, and it just downloads.
//
// The page names all three, says which fits a given model, and shows the
// chosen way's runs below.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Badge } from "@tenstorrent/vesper/badge";
import { Typography } from "@tenstorrent/vesper/typography";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { get, type OrchardStatus, post, short } from "../api";
import { Card, Eyebrow, Page, toastError, usePoll } from "../ui";
import { navigate } from "../App";
import { useOwner } from "../role";
import { OrchardBody, HANDOFF_KEY } from "./Orchard";
import { AgentPortBody, AGENT_HANDOFF_KEY } from "./Bringup";
import { openCommunity } from "./CommunityModels";
import type { CodexRun, Survey } from "./BringupCodex";

export type Way = "orchard" | "agent";

const WAYS: { id: Way | "community"; emoji: string; title: string; tag: string; when: string; how: string; takes: string }[] = [
  {
    id: "orchard", emoji: "🌱", title: "Quick port", tag: "Graft",
    when: "Its architecture already runs here: a fine-tune or post-train of a model in Models.",
    how: "Grafts the new weights onto the model that runs, then tests them on 1, 2 and 4 chips against a CPU reference. Its agents run on this box's own models: no account.",
    takes: "A few hours",
  },
  {
    id: "agent", emoji: "🤖", title: "Agent port", tag: "Coding agent",
    when: "A new architecture: nothing like it runs here yet.",
    how: "An agent writes the model code in tt-metal through eleven checked stages, and asks you about risky steps. Needs an account with OpenAI (for Codex) or Anthropic (for Claude Code).",
    takes: "Many hours, all four chips",
  },
  {
    id: "community", emoji: "📦", title: "Community package", tag: "Already done",
    when: "Someone has packaged it for Tenstorrent chips already.",
    how: "Download it from the community catalog and start it like any model. No bring-up at all; packaged outside Tenstorrent's testing.",
    takes: "A download",
  },
];

const WAY_KEY = "tt.bringup.way";

/** `/app/bringup#orchard`, `#agent`, or the last way used. */
function wayFromLocation(): Way | null {
  const h = location.hash.replace("#", "");
  if (h === "orchard" || h === "agent") return h;
  if (location.pathname.endsWith("/orchard")) return "orchard";
  try { const w = localStorage.getItem(WAY_KEY); return w === "orchard" || w === "agent" ? w : null; } catch { return null; }
}

export function BringupPage() {
  const owner = useOwner();
  const orchard = usePoll(() => get<OrchardStatus>("/orchard").catch(() => null), 8000);
  const agents = usePoll(() => get<{ runs: CodexRun[] }>("/bringup/codex/runs").catch(() => ({ runs: [] })), 8000);
  const [way, setWayState] = useState<Way | null>(wayFromLocation);
  const [nonce, setNonce] = useState(0); // a hand-off remounts the way, so it picks the model up
  useEffect(() => {
    const onNav = () => { const w = wayFromLocation(); if (w) setWayState(w); };
    window.addEventListener("popstate", onNav);
    window.addEventListener("hashchange", onNav);
    return () => { window.removeEventListener("popstate", onNav); window.removeEventListener("hashchange", onNav); };
  }, []);
  const setWay = (w: Way) => {
    setWayState(w);
    try { localStorage.setItem(WAY_KEY, w); } catch { /* private */ }
    history.replaceState(null, "", `/app/bringup#${w}`);
  };

  const orchardRuns = orchard.data?.runs ?? [];
  const agentRuns = agents.data?.runs ?? [];
  const counts: Record<string, { live: number; all: number }> = {
    orchard: { live: orchardRuns.filter((r) => r.live).length, all: orchardRuns.length },
    agent: { live: agentRuns.filter((r) => r.live).length, all: agentRuns.length },
  };
  // A way with a run going is the one to show when nothing was picked.
  const shown: Way | null = way ?? (counts.orchard.live ? "orchard" : counts.agent.live ? "agent" : null);

  return (
    <Page title="Bring-up" subtitle="Get a model that isn't in Models running on this box. There are three ways to do it; which one fits depends on the model.">
      {!owner && <Admonition variant="info" subtle size="sm">Orchard and agent ports run on the box as the box's own account, outside any sandbox, so only an owner of this box can start or steer one. You can follow their runs here, and community models are open to everyone.</Admonition>}
      <div className={`ways ${shown ? "compact" : ""}`} role="tablist" aria-label="Ways to bring up a model">
        {WAYS.map((w) => {
          const on = shown === w.id;
          const c = counts[w.id];
          return (
            <button key={w.id} role="tab" aria-selected={on} className={`way ${on ? "on" : ""}`}
              onClick={() => (w.id === "community" ? openCommunity("") : setWay(w.id))}>
              <div className="row-between">
                <span className="way-title"><span aria-hidden="true">{w.emoji}</span> {w.title}</span>
                {c?.live ? <Badge size="sm" variant="info" subtle>{c.live} running</Badge>
                  : c?.all ? <Badge size="sm" variant="contrast" subtle>{c.all} run{c.all > 1 ? "s" : ""}</Badge>
                  : <span className="muted" style={{ fontSize: 12 }}>{w.tag}</span>}
              </div>
              <span className="way-when">{w.when}</span>
              {!shown && <span className="way-how">{w.how}</span>}
              {!shown && <span className="way-takes">{w.id === "community" ? "Open the catalog →" : w.takes}</span>}
            </button>
          );
        })}
      </div>

      <WhichWay onPick={(w, model) => {
        try { localStorage.setItem(w === "orchard" ? HANDOFF_KEY : AGENT_HANDOFF_KEY, model); } catch { /* private */ }
        setWay(w); setNonce((n) => n + 1);
      }} key={shown ?? "none"} compact={!!shown} />

      {shown === "orchard" && <><WayHeading way="orchard" /><OrchardBody key={`orchard-${nonce}`} /></>}
      {shown === "agent" && <><WayHeading way="agent" /><AgentPortBody key={`agent-${nonce}`} /></>}
    </Page>
  );
}

function WayHeading({ way }: { way: Way }) {
  const w = WAYS.find((x) => x.id === way)!;
  return (
    <div className="section-title">
      <Eyebrow>{w.emoji} {w.title}</Eyebrow>
      <span className="muted">{w.how}</span>
    </div>
  );
}

/** Paste a model: which of the three ways fits it, and a button to go that way. */
function WhichWay({ onPick, compact }: { onPick: (w: Way, model: string) => void; compact: boolean }) {
  const [model, setModel] = useState("");
  const [looking, setLooking] = useState(false);
  const [survey, setSurvey] = useState<Survey | null>(null);
  const [open, setOpen] = useState(!compact);
  // The community catalog is what spots a package someone already made: fetch it once, early.
  useEffect(() => { void get("/community/catalog?all=true").catch(() => null); }, []);

  const look = async () => {
    setLooking(true); setSurvey(null);
    try { setSurvey(await post<Survey>("/bringup/survey", { model: model.trim() })); } catch (e) { toastError(e); } finally { setLooking(false); }
  };
  if (compact && !open) {
    return <div><button className="link-button" onClick={() => setOpen(true)}>Not sure which way fits a model? Check it</button></div>;
  }
  const way = survey?.recommend;
  return (
    <Card className="start-card">
      <div className="row-between">
        <Eyebrow>Which way fits?</Eyebrow>
        {compact && <Button size="xs" variant="ghost" onClick={() => setOpen(false)}>Close</Button>}
      </div>
      <div className="row">
        <div className="grow"><TextInput value={model} onChange={(e) => { setModel(e.target.value); setSurvey(null); }}
          placeholder="Paste a Hugging Face model, e.g. Cloudflare/clef" onKeyDown={(e) => { if (e.key === "Enter" && model.trim()) void look(); }} /></div>
        <Button variant={survey ? "subtle" : "primary"} disabled={!model.trim() || looking} onClick={look}>{looking ? "Looking…" : "Check it"}</Button>
      </div>
      {survey && (
        <div className="start-result">
          <div className="survey-facts">
            <b>{survey.architecture ?? "Unknown architecture"}</b>
            {survey.params_b ? <span>{survey.params_b.toFixed(1)}B parameters</span> : null}
            {survey.license ? <span>{String(survey.license)} license</span> : null}
            {survey.gated ? <span>gated on Hugging Face</span> : null}
          </div>
          <div className="row" style={{ gap: 10, alignItems: "flex-start" }}>
            <span style={{ fontSize: 22 }} aria-hidden="true">{way === "orchard" ? "🌱" : way === "codex" ? "🤖" : way === "community" ? "📦" : "✅"}</span>
            <div className="stack grow" style={{ gap: 4 }}>
              <Typography variant="heading-xs">{way === "orchard" ? "Quick port: graft it" : way === "codex" ? "An agent port" : way === "community" ? "A community package" : "No bring-up needed"}</Typography>
              <span style={{ fontSize: 14 }}>{survey.why}</span>
            </div>
          </div>
          <div className="row wrap" style={{ gap: 8 }}>
            {way === "orchard" && <Button onClick={() => onPick("orchard", survey.model)}>Graft it with a Quick port</Button>}
            {way === "codex" && <Button onClick={() => onPick("agent", survey.model)}>Port it with an agent</Button>}
            {way === "community" && survey.community_package && <Button onClick={() => openCommunity(survey.community_package!)}>Open {short(survey.community_package)}</Button>}
            {way === "none" && <Button onClick={() => navigate("models")}>Open Models</Button>}
            {way !== "orchard" && way !== "none" && survey.same_architecture.length > 0 && <Button variant="ghost" onClick={() => onPick("orchard", survey.model)}>Graft it with a Quick port instead</Button>}
            {way !== "codex" && way !== "none" && <Button variant="ghost" onClick={() => onPick("agent", survey.model)}>Port it with an agent instead</Button>}
          </div>
        </div>
      )}
    </Card>
  );
}
