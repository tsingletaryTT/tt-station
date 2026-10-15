// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Home: what the box is doing, the one thing to do next, the modes, anything
// in progress, the chips at a glance, today's numbers and the box's health.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { ProgressBar } from "@tenstorrent/vesper/progress-bar";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Typography } from "@tenstorrent/vesper/typography";
import { boxName, duration, get, type DoctorCheck, type Job, type MetricsNow, type Mode, type OrchardStatus, post,
  type Serve, short } from "../api";
import { navigate } from "../App";
import { WhatsNewCard } from "./Discover";
import { ResearchCard } from "./Research";
import { Card, Eyebrow, Page, Stat, tempTone, toastError, toastOk, usePoll, useTelemetry } from "../ui";
import { openTranscribe, RecordingNow, useRecorder } from "../recorder";

const MODE_ICONS: Record<string, IconKind> = {
  chat: "chat", images: "galaxy", chat_images: "ai", video: "ai-video-gen", transcribe: "microphone",
};

export function Home() {
  const serve = usePoll(() => get<Serve>("/serve"), 3000);
  const modes = usePoll(() => get<{ modes: Mode[] }>("/modes"), 15000);
  const metrics = usePoll(() => get<MetricsNow>("/metrics/now"), 5000);
  const doctor = usePoll(() => get<{ status: string; checks: DoctorCheck[] }>("/doctor"), 60000);
  const [switching, setSwitching] = useState<string | null>(null);

  const switchTo = async (mode: Mode) => {
    setSwitching(mode.id);
    try {
      await post(`/modes/${mode.id}`, {});
      toastOk(`Switching to ${mode.title}. It's ready in about ${duration(mode.eta_s)}.`);
      serve.refresh();
    } catch (e) {
      toastError(e);
    } finally {
      setSwitching(null);
    }
  };

  const t = metrics.data?.totals;
  return (
    <Page title={greeting()} subtitle={`${boxName} · QuietBox 2 · 4 × Blackhole`}>
      <Hero serve={serve.data} onStop={async () => {
        // Stopping can take minutes (shutting down, then resetting the chips): the card follows along.
        const done = post("/stop");
        window.setTimeout(serve.refresh, 800);
        try { await done; serve.refresh(); } catch (e) { toastError(e); }
      }} />

      <Asleep onWoke={() => serve.refresh()} />
      <HappeningNow />

      <div className="section-title"><Eyebrow>Modes</Eyebrow><span className="muted">One click sets up the whole box.</span></div>
      <div className="grid grid-4">
        {(modes.data?.modes ?? []).map((m) => (
          <Card key={m.id} className={`mode-card ${m.active ? "active" : ""}`}>
            <div className="row-between">
              <Icon kind={MODE_ICONS[m.id] ?? "ai"} className="mode-icon" />
              {m.active ? <Badge variant="mint" size="sm" subtle>Active</Badge>
                : !m.ready ? <Badge variant="warning" size="sm" subtle>Needs download</Badge> : null}
            </div>
            <Typography variant="heading-xs">{m.title}</Typography>
            <Typography variant="copy-sm" className="muted">{m.blurb}</Typography>
            <div className="grow" />
            <div className="row-between">
              <span className="mono muted" style={{ fontSize: 12 }}>{m.models.map(short).join(" + ")}</span>
              {!m.active && (m.ready
                ? <Button size="sm" variant="subtle" disabled={!!switching} onClick={() => switchTo(m)}>
                    {switching === m.id ? "Switching…" : "Switch"}
                  </Button>
                : <Button size="sm" variant="ghost" onClick={() => navigate("models")}>Get it</Button>)}
            </div>
          </Card>
        ))}
      </div>

      <div className="grid grid-2">
        <ChipsCard />
        <Card>
          <div className="row-between"><Eyebrow>Today</Eyebrow><span className="muted mono" style={{ fontSize: 12 }}>since midnight</span></div>
          <div className="grid grid-3">
            <Stat label="Tokens" value={fmt(t?.tokens_today ?? 0)} />
            <Stat label="Requests" value={t?.requests_today ?? 0} />
            <Stat label="Energy" value={((t?.energy_wh_today ?? 0) / 1000).toFixed(2)} unit="kWh" />
          </div>
          <Typography variant="copy-xs" className="muted">
            All time: {fmt(t?.tokens_all ?? 0)} tokens, {(t?.requests_all ?? 0).toLocaleString()} requests, {((t?.energy_wh_all ?? 0) / 1000).toFixed(1)} kWh, all on your own hardware.
          </Typography>
        </Card>
      </div>

      <div className="section-title"><Eyebrow>Research</Eyebrow><span className="muted">Ask a hard question; the box reads the web and writes a cited report.</span></div>
      <ResearchCard />
      <WhatsNewCard onOpen={() => navigate("discover")} />

      <Doctor checks={doctor.data?.checks ?? []} onFixed={doctor.refresh} />
    </Page>
  );
}

function greeting() {
  const h = new Date().getHours();
  return h < 5 ? "Good night" : h < 12 ? "Good morning" : h < 18 ? "Good afternoon" : "Good evening";
}

const fmt = (n: number) => n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)}k` : Math.round(n).toString();

function Hero({ serve, onStop }: { serve: Serve | null; onStop: () => void }) {
  const [now, setNow] = useState(Date.now() / 1000);
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => window.clearInterval(t);
  }, []);
  if (!serve) return <Card className="hero"><Typography variant="copy-md" className="muted">Connecting to the box…</Typography></Card>;
  const elapsed = now - serve.since;
  const kind = serve.kind === "image" ? "Making images" : serve.kind === "video" ? "Making video"
    : serve.kind === "speech" ? "Speech to text" : "Chat";
  return (
    <Card className="hero" tone={serve.phase === "serving" ? "accent" : serve.phase === "failed" ? "danger" : undefined}>
      <div className="stack" style={{ gap: 10 }}>
        <Eyebrow>{serve.phase === "serving" ? `Serving · ${kind}` : serve.phase === "starting" && serve.stalled_s ? `No progress for ${Math.round(serve.stalled_s / 60)} min: it looks stuck` : serve.phase === "starting" ? "Loading onto the chips"
          : serve.phase === "stopping" ? "Stopping" : serve.phase === "failed" ? "Didn't start" : "Idle"}</Eyebrow>
        <div className="hero-model">
          {serve.phase === "idle" ? "Nothing is serving" : short(serve.phase === "serving" ? (serve.served_id ?? serve.model) : (serve.model ?? serve.served_id))}
          {serve.split && serve.companion?.model ? <span className="muted"> + {short(serve.companion.model)}</span> : null}
        </div>
        {serve.phase === "starting" && (
          <div className="stack" style={{ gap: 6, maxWidth: 480 }}>
            <ProgressBar value={serve.eta_s ? Math.min(97, (elapsed / serve.eta_s) * 100) : 30} animated aria-label="Loading" />
            <span className="muted mono" style={{ fontSize: 12 }}>
              {duration(elapsed)} so far{serve.eta_s ? ` · usually about ${duration(serve.eta_s)}` : ""}
            </span>
          </div>
        )}
        {serve.phase === "stopping" && (
          <div className="stack" style={{ gap: 6, maxWidth: 480 }}>
            <ProgressBar value={30} animated aria-label="Stopping" />
            <span className="muted" style={{ fontSize: 12 }}>Shutting the model down, then resetting the chips. This can take a few minutes.</span>
          </div>
        )}
        {serve.phase === "failed" && <Typography variant="copy-sm" className="muted">{serve.error}</Typography>}
        <div className="row" style={{ marginTop: 6 }}>
          {serve.phase === "serving" && serve.kind !== "image" && serve.kind !== "video" &&
            <Button iconLeft={<Icon kind="chat" />} onClick={() => navigate("chat")}>Chat</Button>}
          {serve.phase === "serving" && (serve.kind === "image" || serve.kind === "video" || serve.split) &&
            <Button iconLeft={<Icon kind="galaxy" />} onClick={() => navigate("studio")}>Open Studio</Button>}
          {(serve.phase === "idle" || serve.phase === "failed") &&
            <Button onClick={() => navigate("models")}>Start a model</Button>}
          {serve.phase === "serving" && <Button variant="tertiary" onClick={() => navigate("connect")}>Connect an app</Button>}
          {(serve.phase === "serving" || serve.phase === "starting") && <Button variant="ghost" onClick={onStop}>Stop</Button>}
        </div>
      </div>
      <Icon kind="quietbox" className="hero-glyph" />
    </Card>
  );
}

/** Long work in progress: an Orchard bring-up, transcriptions. Hidden when nothing is going. */
function HappeningNow() {
  const orchard = usePoll(() => get<OrchardStatus>("/orchard").catch(() => null), 10000);
  const jobs = usePoll(() => get<{ jobs: Job[] }>("/jobs").catch(() => ({ jobs: [] })), 8000);
  const run = orchard.data?.runs.find((r) => r.live);
  const active = (jobs.data?.jobs ?? []).filter((j) => ["queued", "waiting", "switching", "running"].includes(j.status));
  const recording = useRecorder().state?.recording;
  if (!run && active.length === 0 && !recording) return null;
  return (
    <div className="grid grid-2">
      <RecordingNow />
      {run && (
        <Card onClick={() => navigate("bringup")}>
          <div className="row-between"><Eyebrow>Bringing up {short(run.model)}</Eyebrow>
            <span className="muted mono" style={{ fontSize: 12 }}>{run.progress?.overall_eta_s ? `about ${duration(run.progress.overall_eta_s)} left` : ""}</span></div>
          <Typography variant="copy-md">{run.progress?.title || "Working"}</Typography>
          <ProgressBar value={(run.progress?.overall ?? 0) * 100} aria-label="Bring-up progress" />
        </Card>
      )}
      {active.map((j) => (
        <Card key={j.id} onClick={openTranscribe}>
          <div className="row-between"><Eyebrow>Transcribing</Eyebrow><span className="muted mono" style={{ fontSize: 12 }}>{j.status}</span></div>
          <Typography variant="copy-md">{j.title}</Typography>
          <ProgressBar value={j.total ? (j.done / j.total) * 100 : 0} aria-label="Transcription progress" />
        </Card>
      ))}
    </div>
  );
}

function ChipsCard() {
  const { chips, live, error } = useTelemetry(true);
  return (
    <Card onClick={() => navigate("hardware")}>
      <div className="row-between">
        <Eyebrow>Chips</Eyebrow>
        <span className="muted mono" style={{ fontSize: 12 }}>
          {live ? `${Math.round(chips.reduce((s, c) => s + (c.powerW ?? 0), 0))} W` : error ? "paused" : "…"}
        </span>
      </div>
      {error && <Typography variant="copy-xs" className="muted">{error}</Typography>}
      <div className="chips" style={{ opacity: live ? 1 : 0.5 }}>
        {chips.map((c) => (
          <div key={c.index} className="chip-card">
            <span className="eyebrow">Chip {c.index}</span>
            <span className="chip-temp" style={{ color: tempTone(c.tempC) }}>{c.tempC != null ? Math.round(c.tempC) : "–"}°<span className="stat-unit">C</span></span>
            <div className="bar"><i style={{ width: `${Math.min(100, Math.max(0, ((c.powerW ?? 0) - 10) / 1.1))}%`, background: "var(--vesper-teal-600)" }} /></div>
            <span className="muted mono" style={{ fontSize: 12 }}>{c.powerW != null ? `${Math.round(c.powerW)} W` : "–"} · {c.aiclk ?? "–"} MHz</span>
          </div>
        ))}
      </div>
    </Card>
  );
}

function Doctor({ checks, onFixed }: { checks: DoctorCheck[]; onFixed: () => void }) {
  const problems = checks.filter((c) => c.status !== "ok");
  if (checks.length === 0) return null;
  if (problems.length === 0) {
    return <Admonition variant="success" subtle size="sm">Box Doctor: everything looks healthy.</Admonition>;
  }
  return (
    <div className="stack">
      {problems.map((c) => (
        <Admonition key={c.id} variant={c.status === "fail" ? "danger" : "warning"} subtle size="sm"
          cta={c.fix ? {
            children: "Fix it",
            onClick: async () => {
              try { await post(`/doctor/fix/${c.fix}`); toastOk("Done."); onFixed(); } catch (e) { toastError(e); }
            },
          } : undefined}>
          <b>{c.title}.</b> {c.detail}{c.command ? <> Run <code>{c.command}</code>.</> : null}
        </Admonition>
      ))}
    </div>
  );
}

/** The model was stopped to save power: say so, with a way to start it again. */
function Asleep({ onWoke }: { onWoke: () => void }) {
  const idle = usePoll(() => get<{ asleep: { label: string; at: number } | null; settings: { wake_on_request: boolean } }>("/idle"), 15000);
  const [busy, setBusy] = useState(false);
  const a = idle.data?.asleep;
  if (!a) return null;
  const wake = async () => {
    setBusy(true);
    try { await post("/idle/wake"); toastOk(`Starting ${a.label}…`); onWoke(); void idle.refresh(); } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <Admonition variant="info" subtle size="sm">
      <div className="row-between" style={{ gap: 12, width: "100%" }}>
        <span>
          <b>{a.label}</b> is stopped to save power (nobody used it for a while).
          {idle.data?.settings.wake_on_request ? " It starts again on its own when something asks for it." : ""}
        </span>
        <Button size="sm" variant="subtle" disabled={busy} onClick={wake}>{busy ? "Starting…" : "Start it now"}</Button>
      </div>
    </Admonition>
  );
}
