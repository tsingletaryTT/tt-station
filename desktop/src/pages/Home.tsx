// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Home: what the box is doing, the one thing to do next, the modes, anything
// in progress, the chips at a glance, today's numbers and the box's health.

import { type ReactNode, useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { ProgressBar } from "@tenstorrent/vesper/progress-bar";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Typography } from "@tenstorrent/vesper/typography";
import { boxName, duration, get, type DoctorCheck, hardwareLabel, type Job, type MetricsNow, type Mode, type OrchardStatus, post,
  type Pull, type Serve, short, type Status } from "../api";
import { navigate } from "../App";
import { WhatsNewCard } from "./WhatsNewCard";
import { ResearchCard } from "./ResearchCard";
import { Card, DownloadProgress, Eyebrow, LinkButton, Page, Stat, tempTone, tempWord, toastError, toastOk, usePoll, useReducedMotion,
  useServe, useTelemetry } from "../ui";
import { openTranscribe, RecordingNow, useRecorder } from "../recorder";
import { ask, whoIsAffected } from "../dialog";
import { openModels } from "../modelsLink";
import { firstRun, loadFirstRun, saveFirstRun, shouldShow, type FirstRunSaved, type StepState } from "../firstRun";
import { smoothBehavior } from "../stickToBottom";

const MODE_ICONS: Record<string, IconKind> = {
  chat: "chat", images: "galaxy", chat_images: "ai", video: "ai-video-gen", transcribe: "microphone",
};

/** Where a mode's work happens: Chat for talking, Studio for pictures, video and transcripts. */
const placeFor = (m: Mode | null) => (m?.id === "chat" ? "chat" : "studio");

export function Home() {
  const serve = useServe(3000);
  const modes = usePoll(() => get<{ modes: Mode[] }>("/modes"), 15000);
  const metrics = usePoll(() => get<MetricsNow>("/metrics/now"), 5000);
  const doctor = usePoll(() => get<{ status: string; checks: DoctorCheck[] }>("/doctor"), 60000);
  const status = usePoll(() => get<Status>("/status"), 5 * 60 * 1000);
  const [switching, setSwitching] = useState<string | null>(null);
  const modesRef = useRef<HTMLDivElement>(null);
  // While the first-run steps are up, they're the one place to start from.
  const [guided, setGuided] = useState(false);

  const switchTo = async (mode: Mode) => {
    const s = serve.data;
    const current = s && (s.phase === "serving" || s.phase === "starting") ? short(s.served_id ?? s.model) : null;
    if (current && !(await ask(`Switch the box to ${mode.title}?`, {
      body: `${whoIsAffected(current)} ${mode.title} is ready in about ${duration(mode.eta_s)}.`,
      confirm: `Switch to ${mode.title}`,
    }))) return;
    setSwitching(mode.id);
    try {
      await post(`/modes/${mode.id}`, {});
      toastOk(`Switching to ${mode.title}. It's ready in about ${duration(mode.eta_s)}.`);
      void serve.refresh();
      void modes.refresh();
    } catch (e) {
      toastError(e);
    } finally {
      setSwitching(null);
    }
  };

  const stop = async () => {
    const s = serve.data;
    const name = short(s?.served_id ?? s?.model) || "the model";
    if (!(await ask(`Stop ${name}?`, {
      body: `${whoIsAffected(name)} Stopping takes a few minutes while the chips reset; start it again from here or Models.`,
      confirm: `Stop ${name}`, danger: true,
    }))) return;
    // Stopping can take minutes (shutting down, then resetting the chips): the card follows along.
    const done = post("/stop");
    window.setTimeout(() => void serve.refresh(), 800);
    try { await done; void serve.refresh(); } catch (e) { toastError(e); }
  };

  const t = metrics.data?.totals;
  const hw = hardwareLabel(status.data);
  const list = modes.data?.modes ?? [];
  return (
    <Page title={greeting()} subtitle={hw ? `${boxName} · ${hw}` : boxName}>
      <FirstRun modes={list} serve={serve.data} requestsAll={t ? t.requests_all : null} switching={switching} onSwitch={switchTo} onShown={setGuided} />

      <Hero serve={serve.data} error={serve.error} modes={list} guided={guided} switching={switching} onSwitch={switchTo} onStop={stop}
        onRetry={() => void serve.refresh()} onShowModes={() => modesRef.current?.scrollIntoView({ behavior: smoothBehavior(), block: "start" })} />

      <Asleep onWoke={() => void serve.refresh()} />
      <HappeningNow />

      <div className="section-title" ref={modesRef}><Eyebrow>Modes</Eyebrow><span className="muted">One click sets up the whole box.</span></div>
      <div className="grid grid-4">
        {list.map((m) => (
          <Card key={m.id} className={`mode-card ${m.active ? "active" : ""}`}>
            <div className="row-between">
              <Icon kind={MODE_ICONS[m.id] ?? "ai"} className="mode-icon" aria-hidden />
              {m.active ? <Badge variant="mint" size="sm" subtle>Active</Badge>
                : !m.ready ? <Badge variant="warning" size="sm" subtle>Needs download</Badge> : null}
            </div>
            <Typography variant="heading-xs">{m.title}</Typography>
            <Typography variant="copy-sm" className="muted">{m.blurb}</Typography>
            <div className="grow" />
            <div className="row-between">
              <Typography variant="copy-xs" className="mono muted">{m.models.map(short).join(" + ")}</Typography>
              {!m.active && (m.ready
                ? <Button size="sm" variant="subtle" disabled={!!switching} onClick={() => switchTo(m)}>
                    {switching === m.id ? "Switching…" : "Switch"}
                  </Button>
                : <Button size="sm" variant="ghost" onClick={() => openModels(m.needs_download[0] ?? m.models[0] ?? "")}
                    aria-label={`Get ${m.needs_download.map(short).join(" and ") || m.title} in Models`}>Get it</Button>)}
            </div>
          </Card>
        ))}
      </div>

      <div className="grid grid-2">
        <ChipsCard />
        <Card>
          <div className="row-between"><Eyebrow>Today</Eyebrow><Typography variant="copy-xs" className="muted mono">since midnight</Typography></div>
          <div className="grid grid-3">
            <Stat label="Tokens" value={fmt(t?.tokens_today ?? 0)} />
            <Stat label="Requests" value={t?.requests_today ?? 0} />
            <Stat label="Energy" value={((t?.energy_wh_today ?? 0) / 1000).toFixed(2)} unit="kWh" />
          </div>
          <Typography variant="copy-xs" className="muted">
            All time: {fmt(t?.tokens_all ?? 0)} tokens and {(t?.requests_all ?? 0).toLocaleString()} requests, all on your own hardware.{" "}
            <LinkButton onClick={() => navigate("hardware", "energy")}>Energy use and cost</LinkButton>
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

/**
 * The first time: what you want the box for, its model (downloaded here, with
 * progress), then the first chat or picture. Each step follows what the box
 * reports; it can be hidden at any point, and stays hidden.
 */
function FirstRun({ modes, serve, requestsAll, switching, onSwitch, onShown }: {
  modes: Mode[]; serve: Serve | null; requestsAll: number | null; switching: string | null; onSwitch: (m: Mode) => void;
  onShown: (guiding: boolean) => void;
}) {
  const [saved, setSaved] = useState<FirstRunSaved>(loadFirstRun);
  const update = (v: FirstRunSaved) => { setSaved(v); saveFirstRun(v); };
  const show = shouldShow(saved, requestsAll);
  useEffect(() => { if (show && !saved.shown) update({ ...saved, shown: true }); }); // eslint-disable-line react-hooks/exhaustive-deps
  const steps = firstRun(saved, modes, requestsAll ?? 0);
  const m = steps.mode;
  const guiding = show && modes.length > 0 && !steps.complete;
  useEffect(() => onShown(guiding), [guiding, onShown]);
  const downloading = show && steps.get === "current";
  const pulls = usePoll(() => (downloading ? get<{ pulls: Pull[] }>("/tt/pulls") : Promise.resolve({ pulls: [] as Pull[] })), downloading ? 3000 : 60000, [downloading]);
  const [asked, setAsked] = useState(false);
  const [picking, setPicking] = useState(false);
  if (!show || modes.length === 0) return null;

  const mine = (pulls.data?.pulls ?? []).filter((p) => p.state === "running" && m?.needs_download.includes(p.model));
  const download = async () => {
    if (!m) return;
    setAsked(true);
    try {
      for (const model of m.needs_download) await post("/tt/pull", { model });
      void pulls.refresh();
    } catch (e) { toastError(e); setAsked(false); }
  };
  const serving = m?.active && serve?.phase === "serving";
  const starting = serve?.phase === "starting";

  return (
    <Card className="firstrun" tone="accent">
      <div className="row-between">
        <div className="stack" style={{ gap: 4 }}>
          <Eyebrow>Getting started</Eyebrow>
          <Typography variant="heading-sm">{steps.complete ? "You're all set" : `Three steps to your first answer from ${boxName}`}</Typography>
        </div>
        <Button size="sm" variant="ghost" onClick={() => update({ ...saved, dismissed: true })}>{steps.complete ? "Done" : "Hide"}</Button>
      </div>
      <ol className="firstrun-steps">
        <Step n={1} state={picking ? "current" : steps.choose} title="Choose what you want to do"
          done={m ? `${m.title}.` : undefined} onChange={steps.complete ? undefined : () => setPicking(true)}>
          <div className="firstrun-modes" role="radiogroup" aria-label="What you want to do">
            {modes.map((x) => (
              <Button key={x.id} size="sm" variant={x.id === m?.id ? "primary" : "subtle"} role="radio" aria-checked={x.id === m?.id}
                iconLeft={<Icon kind={MODE_ICONS[x.id] ?? "ai"} aria-hidden />} onClick={() => { update({ ...saved, mode: x.id }); setPicking(false); }}>{x.title}</Button>
            ))}
          </div>
          <Typography variant="copy-sm" className="muted">You can switch any time from Home; each mode sets up the whole box.</Typography>
        </Step>
        <Step n={2} state={steps.get} title={m && !m.ready ? `Get ${m.needs_download.map(short).join(" and ")}` : "Get its model"}
          done={m ? `${m.models.map(short).join(" + ")} is on the box.` : undefined}>
          {mine.length > 0 ? mine.map((p) => (
            <DownloadProgress key={p.model} step={`Downloading ${short(p.model)}`}
              progress={p.progress ?? { done_bytes: p.bytes_on_disk, total_bytes: null, percent: null, bytes_per_s: null, eta_s: null }} />
          )) : (
            <div className="row wrap" style={{ gap: 8 }}>
              <Button size="sm" iconLeft={<Icon kind="download" aria-hidden />} disabled={asked} onClick={download}>{asked ? "Starting the download…" : "Download it"}</Button>
              <Button size="sm" variant="ghost" onClick={() => openModels(m?.needs_download[0] ?? "")}>See it in Models</Button>
            </div>
          )}
          <Typography variant="copy-sm" className="muted">It downloads to the box once; you can keep using the app meanwhile.</Typography>
        </Step>
        <Step n={3} state={steps.try} title={m?.id === "chat" ? "Start it and say hello" : "Start it and make something"}
          done="Your first answer came back. Everything ran on your own hardware.">
          {serving ? (
            <div className="row wrap" style={{ gap: 8 }}>
              <Button size="sm" iconLeft={<Icon kind={placeFor(m) === "chat" ? "chat" : "galaxy"} aria-hidden />} onClick={() => navigate(placeFor(m))}>
                {placeFor(m) === "chat" ? "Open Chat" : "Open Studio"}
              </Button>
            </div>
          ) : starting ? (
            <Typography variant="copy-sm" className="muted">Loading onto the chips. This takes a few minutes; this card moves on when it's ready.</Typography>
          ) : m && (
            <div className="row wrap" style={{ gap: 8 }}>
              <Button size="sm" disabled={!!switching} onClick={() => onSwitch(m)}>{switching === m.id ? "Starting…" : `Start ${m.title}`}</Button>
            </div>
          )}
        </Step>
      </ol>
    </Card>
  );
}

function Step({ n, state, title, done, children, onChange }: { n: number; state: StepState; title: string; done?: string; children: ReactNode; onChange?: () => void }) {
  return (
    <li className={`firstrun-step ${state}`} aria-current={state === "current" ? "step" : undefined}>
      <span className="firstrun-n" aria-hidden>{state === "done" ? <Icon kind="checkmark" /> : n}</span>
      <div className="firstrun-body">
        <span className="firstrun-title">{title}<span className="visually-hidden">{state === "done" ? " (done)" : state === "later" ? " (after the step above)" : ""}</span></span>
        {state === "done" && done && (
          <Typography variant="copy-sm" className="muted">{done}{onChange && <> <LinkButton onClick={onChange}>Change</LinkButton></>}</Typography>
        )}
        {state === "current" && children}
      </div>
    </li>
  );
}

function greeting() {
  const h = new Date().getHours();
  return h < 5 ? "Good night" : h < 12 ? "Good morning" : h < 18 ? "Good afternoon" : "Good evening";
}

const fmt = (n: number) => n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)}k` : Math.round(n).toString();

/** How long to say only "Connecting…" before saying it's taking a while. */
const SLOW_MS = 8000;

function Hero({ serve, error, modes, guided, switching, onSwitch, onStop, onRetry, onShowModes }: {
  serve: Serve | null; error: string | null; modes: Mode[]; guided: boolean; switching: string | null;
  onSwitch: (m: Mode) => void; onStop: () => void; onRetry: () => void; onShowModes: () => void;
}) {
  const [now, setNow] = useState(Date.now() / 1000);
  const [mounted] = useState(() => Date.now());
  const reduce = useReducedMotion();
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => window.clearInterval(t);
  }, []);
  if (!serve) {
    const slow = !!error || now * 1000 - mounted > SLOW_MS;
    return (
      <Card className="hero">
        <div className="stack" style={{ gap: 10 }}>
          <Eyebrow>{error ? "Not connected" : "Connecting"}</Eyebrow>
          <Typography variant="heading-md">{error ? `Can't reach ${boxName} yet` : slow ? `Still waiting for ${boxName}…` : `Connecting to ${boxName}…`}</Typography>
          {slow && <Typography variant="copy-sm" className="muted">{error ?? "It's taking longer than usual. If the box just started, give it a minute."} TT Station keeps trying.</Typography>}
          {slow && <div><Button size="sm" variant="subtle" onClick={onRetry}>Try now</Button></div>}
        </div>
        <Icon kind="quietbox" className="hero-glyph" aria-hidden />
      </Card>
    );
  }
  const elapsed = now - serve.since;
  const kind = serve.kind === "image" ? "Making images" : serve.kind === "video" ? "Making video"
    : serve.kind === "speech" ? "Speech to text" : "Chat";
  const idle = serve.phase === "idle" || serve.phase === "failed";
  // Nothing serving: start a mode right here (the ones already downloaded), not three screens away.
  const startable = guided ? [] : modes.filter((m) => m.ready).slice(0, 3);
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
            <ProgressBar value={serve.eta_s ? Math.min(97, (elapsed / serve.eta_s) * 100) : 30} animated={!reduce} aria-label="Loading" />
            <Typography variant="copy-xs" className="muted mono">
              {duration(elapsed)} so far{serve.eta_s ? ` · usually about ${duration(serve.eta_s)}` : ""}
            </Typography>
          </div>
        )}
        {serve.phase === "stopping" && (
          <div className="stack" style={{ gap: 6, maxWidth: 480 }}>
            <ProgressBar value={30} animated={!reduce} aria-label="Stopping" />
            <Typography variant="copy-xs" className="muted">Shutting the model down, then resetting the chips. This can take a few minutes.</Typography>
          </div>
        )}
        {serve.phase === "failed" && <Typography variant="copy-sm" className="muted">{serve.error}</Typography>}
        {idle && startable.length > 0 && <Typography variant="copy-sm" className="muted">Pick what you want to do and the box sets itself up:</Typography>}
        {idle && guided && <Typography variant="copy-sm" className="muted">The steps above get your first model going.</Typography>}
        <div className="row wrap" style={{ marginTop: 6 }}>
          {serve.phase === "serving" && serve.kind !== "image" && serve.kind !== "video" &&
            <Button iconLeft={<Icon kind="chat" aria-hidden />} onClick={() => navigate("chat")}>Chat</Button>}
          {serve.phase === "serving" && (serve.kind === "image" || serve.kind === "video" || serve.split) &&
            <Button iconLeft={<Icon kind="galaxy" aria-hidden />} onClick={() => navigate("studio")}>Open Studio</Button>}
          {idle && startable.map((m, i) => (
            <Button key={m.id} variant={i === 0 ? "primary" : "subtle"} iconLeft={<Icon kind={MODE_ICONS[m.id] ?? "ai"} aria-hidden />}
              disabled={!!switching} onClick={() => onSwitch(m)}>{switching === m.id ? "Starting…" : `Start ${m.title}`}</Button>
          ))}
          {idle && startable.length > 0 && <Button variant="ghost" onClick={onShowModes}>All modes</Button>}
          {serve.phase === "failed" && startable.length === 0 && <Button variant="subtle" onClick={() => navigate("models")}>Open Models</Button>}
          {serve.phase === "serving" && <Button variant="tertiary" onClick={() => navigate("connect")}>Connect an app</Button>}
          {(serve.phase === "serving" || serve.phase === "starting") && <Button variant="ghost" onClick={onStop}>Stop…</Button>}
        </div>
      </div>
      <Icon kind="quietbox" className="hero-glyph" aria-hidden />
    </Card>
  );
}

/** Long work in progress: a bring-up, transcriptions. Hidden when nothing is going. */
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
            <Typography variant="copy-xs" className="muted mono">{run.progress?.overall_eta_s ? `about ${duration(run.progress.overall_eta_s)} left` : ""}</Typography></div>
          <Typography variant="copy-md">{run.progress?.title || "Working"}</Typography>
          <ProgressBar value={(run.progress?.overall ?? 0) * 100} aria-label="Bring-up progress" />
        </Card>
      )}
      {active.map((j) => (
        <Card key={j.id} onClick={openTranscribe}>
          <div className="row-between"><Eyebrow>Transcribing</Eyebrow><Typography variant="copy-xs" className="muted mono">{j.status}</Typography></div>
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
        <Typography variant="copy-xs" className="muted mono">
          {live ? `${Math.round(chips.reduce((s, c) => s + (c.powerW ?? 0), 0))} W` : error ? "paused" : "…"}
        </Typography>
      </div>
      {error && <Typography variant="copy-xs" className="muted">{error}</Typography>}
      <div className="chips" style={{ opacity: live ? 1 : 0.5 }}>
        {chips.map((c) => (
          <div key={c.index} className="chip-card">
            <span className="eyebrow">Chip {c.index}</span>
            <span className="chip-temp" style={{ color: tempTone(c.tempC) }}>{c.tempC != null ? Math.round(c.tempC) : "–"}°<span className="stat-unit">C</span></span>
            <Typography variant="copy-xs" className="muted">{tempWord(c.tempC)}</Typography>
            <div className="bar"><i style={{ width: `${Math.min(100, Math.max(0, ((c.powerW ?? 0) - 10) / 1.1))}%`, background: "var(--vesper-teal-600)" }} /></div>
            <Typography variant="copy-xs" className="muted mono">{c.powerW != null ? `${Math.round(c.powerW)} W` : "–"} · {c.aiclk ?? "–"} MHz</Typography>
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
