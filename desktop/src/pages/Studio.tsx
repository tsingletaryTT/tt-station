// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Studio: make images and video, and transcribe recordings of any length.
// Everything runs on the box; files dropped here go straight to it.

import { useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { TextArea } from "@tenstorrent/vesper/text-area";
import { Select } from "@tenstorrent/vesper/select";
import { ProgressBar } from "@tenstorrent/vesper/progress-bar";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Typography } from "@tenstorrent/vesper/typography";
import { StatusIndicator } from "@tenstorrent/vesper/status-indicator";
import { Icon } from "@tenstorrent/vesper/icons";
import { del, duration, fetchBlob, get, type Job, post, type Serve, short, text, upload } from "../api";
import { Card, copyText, Empty, Eyebrow, Page, toastError, toastOk, usePoll, useReducedMotion, useServe } from "../ui";
import { addToast } from "@tenstorrent/vesper/toast";
import { ask, whoIsAffected } from "../dialog";
import { deleteWithUndo, UNDO_MS } from "../undo";
import { MeetingRecorder } from "../recorder";

type Tab = "images" | "video" | "transcribe";

export function Studio() {
  const serve = useServe(5000);
  const kind = serve.data?.phase === "serving" ? serve.data.kind : null;
  const [tab, setTab] = useState<Tab>(location.hash === "#transcribe" ? "transcribe" : "images");
  useEffect(() => { if (kind === "video") setTab("video"); }, [kind]);
  return (
    <Page title="Studio" subtitle="Pictures, video, meeting recordings and transcripts, made on your box."
      actions={
        <div className="pill-tabs" role="tablist">
          {(["images", "video", "transcribe"] as Tab[]).map((t) => (
            <button key={t} className={tab === t ? "on" : ""} onClick={() => setTab(t)} role="tab" aria-selected={tab === t}>
              {t === "images" ? "Images" : t === "video" ? "Video" : "Transcribe"}
            </button>
          ))}
        </div>
      }>
      {tab === "images" && <Images serve={serve.data} onSwitched={serve.refresh} />}
      {tab === "video" && <Video serve={serve.data} onSwitched={serve.refresh} />}
      {tab === "transcribe" && <Transcribe />}
    </Page>
  );
}

function NeedsMode({ what, mode, serve, onSwitched }: { what: string; mode: string; serve: Serve | null; onSwitched: () => void }) {
  const [busy, setBusy] = useState(false);
  return (
    <Admonition variant="info" subtle cta={serve?.phase === "starting" ? undefined : {
      children: busy ? "Switching…" : `Switch to ${what}`,
      disabled: busy,
      onClick: async () => {
        const current = serve?.phase === "serving" ? short(serve.served_id ?? serve.model) : null;
        if (current && !(await ask(`Switch the box to ${what}?`, { body: whoIsAffected(current), confirm: `Switch to ${what}` }))) return;
        setBusy(true);
        try { await post(`/modes/${mode}`, {}); toastOk(`Switching the box to ${what}.`); onSwitched(); } catch (e) { toastError(e); } finally { setBusy(false); }
      },
    }}>
      {serve?.phase === "starting" ? `${short(serve.model)} is loading…`
        : `${what === "images" ? "Images need an image model" : "Video needs a video model"} on the box. ${serve?.phase === "serving" ? `It's serving ${short(serve.served_id ?? serve.model)} right now.` : ""}`}
    </Admonition>
  );
}

// ---------------------------------------------------------------- images

interface Made { url: string; prompt: string; seconds: number | null }

function Images({ serve, onSwitched }: { serve: Serve | null; onSwitched: () => void }) {
  const canMake = serve?.phase === "serving" && (serve.kind === "image" || serve.split);
  const [prompt, setPrompt] = useState("");
  const [steps, setSteps] = useState("20");
  const [count, setCount] = useState("1");
  const [busy, setBusy] = useState(false);
  const [made, setMade] = useState<Made[]>([]);
  const [started, setStarted] = useState(0);
  const reduce = useReducedMotion();
  const [, tick] = useState(0);
  useEffect(() => {
    if (!busy) return;
    const t = window.setInterval(() => tick((n) => n + 1), 500);
    return () => window.clearInterval(t);
  }, [busy]);

  const make = async () => {
    if (!prompt.trim()) return;
    setBusy(true);
    setStarted(Date.now());
    try {
      const r = await post<{ images: string[]; generation_time?: number }>("/generate/image", {
        prompt: prompt.trim(), num_inference_steps: Number(steps), number_of_images: Number(count), image_return_format: "JPEG",
      });
      setMade((m) => [...r.images.map((b64) => ({ url: `data:image/jpeg;base64,${b64}`, prompt: prompt.trim(), seconds: r.generation_time ?? null })), ...m]);
    } catch (e) {
      toastError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      {!canMake && <NeedsMode what="images" mode="images" serve={serve} onSwitched={onSwitched} />}
      <Card>
        <TextArea value={prompt} onChange={(e) => setPrompt(e.target.value)} height={96} disabled={!canMake || busy}
          placeholder="A QuietBox on a desk at night, glowing teal, cinematic"
          onKeyDown={(e) => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) make(); }} />
        <div className="row-between">
          <div className="row">
            <Select size="sm" value={steps} onValueChange={(v) => setSteps(v ?? "20")}
              options={[{ label: "Fast (8 steps)", value: "8" }, { label: "Balanced (20)", value: "20" }, { label: "Best (40)", value: "40" }]} />
            <Select size="sm" value={count} onValueChange={(v) => setCount(v ?? "1")}
              options={[{ label: "1 image", value: "1" }, { label: "2 images", value: "2" }, { label: "4 images", value: "4" }]} />
          </div>
          <Button onClick={make} disabled={!canMake || busy || !prompt.trim()} iconLeft={<Icon kind="galaxy" />}>
            {busy ? `Making… ${Math.round((Date.now() - started) / 1000)} s` : "Make"}
          </Button>
        </div>
        {busy && <ProgressBar value={Math.min(95, ((Date.now() - started) / 1000 / (Number(steps) * 0.6)) * 100)} animated={!reduce} aria-label="Making" />}
      </Card>
      {made.length === 0 ? (
        <Empty title="Nothing made yet">Pictures you make here stay until you close the app. Right-click one to save it.</Empty>
      ) : (
        <div className="gallery">
          {made.map((m, i) => (
            <a key={i} href={m.url} download={`tt-${Date.now()}-${i}.jpg`} title={`${m.prompt}${m.seconds ? ` · ${m.seconds.toFixed(1)} s` : ""}`}>
              <img src={m.url} alt={m.prompt} />
            </a>
          ))}
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------- video

interface VideoJob { id: string; status: string; progress?: number | null; error?: string | null }

function Video({ serve, onSwitched }: { serve: Serve | null; onSwitched: () => void }) {
  const canMake = serve?.phase === "serving" && serve.kind === "video";
  const [prompt, setPrompt] = useState("");
  const [job, setJob] = useState<VideoJob | null>(null);
  const [video, setVideo] = useState<string | null>(null);
  useEffect(() => {
    if (!job || ["completed", "failed", "cancelled", "done"].includes(job.status)) return;
    const t = window.setInterval(async () => {
      try {
        const j = await get<VideoJob>(`/generate/video/${job.id}`);
        setJob(j);
        if (j.status === "completed" || j.status === "done") {
          setVideo(URL.createObjectURL(await fetchBlob(`/generate/video/${job.id}/download`)));
        }
      } catch { /* keep polling */ }
    }, 3000);
    return () => window.clearInterval(t);
  }, [job]);
  return (
    <>
      {!canMake && <NeedsMode what="video" mode="video" serve={serve} onSwitched={onSwitched} />}
      <Card>
        <TextArea value={prompt} onChange={(e) => setPrompt(e.target.value)} height={96} disabled={!canMake}
          placeholder="Slow aerial shot over a misty pine forest at sunrise" />
        <div className="row-between">
          <span className="muted">A few seconds of video takes several minutes on the box.</span>
          <Button disabled={!canMake || !prompt.trim() || (!!job && !video)} onClick={async () => {
            try { setVideo(null); setJob(await post<VideoJob>("/generate/video", { prompt: prompt.trim() })); } catch (e) { toastError(e); }
          }}>Make video</Button>
        </div>
        {job && !video && (
          <div className="stack" style={{ gap: 6 }}>
            <StatusIndicator label={job.status} state={job.status === "failed" ? "error" : "progress"} animated={job.status !== "failed"} />
            {job.progress != null && <ProgressBar value={job.progress * 100} aria-label="Video progress" />}
            {job.error && <span className="muted">{job.error}</span>}
          </div>
        )}
        {video && <video src={video} controls autoPlay loop style={{ width: "100%", borderRadius: 12 }} />}
      </Card>
    </>
  );
}

// ---------------------------------------------------------------- transcribe

function Transcribe() {
  const jobs = usePoll(() => get<{ jobs: Job[] }>("/jobs"), 4000);
  const [over, setOver] = useState(false);
  const [sending, setSending] = useState<string | null>(null);
  const [tonight, setTonight] = useState(false);
  const [open, setOpen] = useState<{ job: Job; text: string } | null>(null);
  const picker = useRef<HTMLInputElement>(null);
  const [gone, setGone] = useState<string[]>([]);
  const active = (j: Job) => ["queued", "waiting", "switching", "running"].includes(j.status);
  // Cancelling work in progress asks first (the parts done are lost); a finished transcript can be brought back for a few seconds.
  const removeJob = async (j: Job) => {
    if (active(j)) {
      if (!(await ask(`Cancel transcribing ${j.title}?`, {
        body: j.status === "running" ? `It's ${j.done} of ${j.total} parts in; what's done so far is discarded.` : "It's taken off the list and won't run.",
        confirm: "Cancel it", cancel: "Keep going", danger: true,
      }))) return;
      try { await del(`/jobs/${j.id}`); void jobs.refresh(); } catch (e) { toastError(e); }
      return;
    }
    if (open?.job.id === j.id) setOpen(null);
    const undo = deleteWithUndo({
      hide: () => setGone((g) => [...g, j.id]),
      show: () => setGone((g) => g.filter((x) => x !== j.id)),
      remove: () => del(`/jobs/${j.id}`),
      removed: () => void jobs.refresh(),
      failed: toastError,
    });
    addToast({ content: "Transcript deleted.", timeout: UNDO_MS, action: { content: "Undo", altText: "Bring the transcript back", handler: undo } });
  };

  const send = async (file: File) => {
    setSending(file.name);
    try {
      await upload(`/jobs/transcribe?name=${encodeURIComponent(file.name)}&when=${tonight ? "tonight" : "now"}`, file);
      toastOk(`${file.name} is on the box. You'll get a notification when the transcript is ready.`);
      jobs.refresh();
    } catch (e) {
      toastError(e);
    } finally {
      setSending(null);
    }
  };

  return (
    <>
      <MeetingRecorder onQueued={jobs.refresh} />
      <div className={`drop ${over ? "over" : ""}`}
        onDragOver={(e) => { e.preventDefault(); setOver(true); }}
        onDragLeave={() => setOver(false)}
        onDrop={(e) => { e.preventDefault(); setOver(false); const f = e.dataTransfer.files[0]; if (f) send(f); }}>
        <Icon kind="microphone" style={{ width: 28, height: 28, color: "var(--vesper-text-accent)" }} />
        <Typography variant="heading-xs">{sending ? `Sending ${sending}…` : "Drop a recording or a video here"}</Typography>
        <Typography variant="copy-sm" className="muted">Any length. Transcribed on the box's CPU while your model keeps running, or on the chips when they're free.</Typography>
        <div className="row">
          <Button size="sm" variant="subtle" disabled={!!sending} onClick={() => picker.current?.click()}>Choose a file</Button>
          <Select size="sm" value={tonight ? "tonight" : "now"} onValueChange={(v) => setTonight(v === "tonight")}
            options={[{ label: "As soon as the box is free", value: "now" }, { label: "Tonight (1–6 am)", value: "tonight" }]} />
        </div>
        <input ref={picker} type="file" accept="audio/*,video/*" hidden onChange={(e) => { const f = e.target.files?.[0]; if (f) send(f); e.target.value = ""; }} />
      </div>

      <div className="section-title"><Eyebrow>Transcripts</Eyebrow></div>
      <Card>
        {(jobs.data?.jobs ?? []).filter((j) => !gone.includes(j.id)).length === 0 && <Empty title="No transcripts yet" />}
        <div className="list">
          {(jobs.data?.jobs ?? []).filter((j) => !gone.includes(j.id)).map((j) => (
            <div key={j.id} className="list-row">
              <Icon kind={j.status === "done" ? "success" : j.status === "failed" ? "error" : "clock"} width={18} height={18}
                style={{ color: j.status === "done" ? "var(--vesper-mint-600)" : j.status === "failed" ? "var(--vesper-red-600)" : "var(--vesper-text-accent)" }} />
              <div className="grow">
                <div>{j.title}</div>
                <div className="muted" style={{ fontSize: 12 }}>
                  {j.status === "running" ? `Part ${j.done} of ${j.total}` : j.status === "waiting" ? "Waiting for the box" : j.status}
                  {j.duration_s ? ` · ${duration(j.duration_s)}` : ""}{j.error ? ` · ${j.error}` : ""}
                </div>
                {j.status === "running" && j.total > 0 && <ProgressBar size="sm" value={(j.done / j.total) * 100} aria-label={`${j.title} progress`} />}
              </div>
              {j.status === "waiting" && <Button size="sm" variant="subtle" onClick={async () => { try { await post(`/jobs/${j.id}/run`); jobs.refresh(); } catch (e) { toastError(e); } }}>Run now</Button>}
              {j.status === "done" && <Button size="sm" variant="subtle" onClick={async () => setOpen({ job: j, text: await text(`/jobs/${j.id}/result`) })}>Open</Button>}
              <Button size="sm" variant="ghost" aria-label={`${active(j) ? "Cancel" : "Delete"} ${j.title}`} onClick={() => removeJob(j)}>
                {active(j) ? "Cancel" : "Delete"}
              </Button>
            </div>
          ))}
        </div>
      </Card>

      {open && (
        <Card>
          <div className="row-between">
            <Typography variant="heading-xs">{open.job.title}</Typography>
            <div className="row">
              <Button size="sm" variant="subtle" onClick={() => void copyText(open.text, "Copied.")}>Copy</Button>
              <Button size="sm" variant="subtle" onClick={async () => {
                const srt = await text(`/jobs/${open.job.id}/result?format=srt`);
                const a = document.createElement("a");
                a.href = URL.createObjectURL(new Blob([srt], { type: "text/plain" }));
                a.download = `${open.job.title.replace(/\.[^.]+$/, "")}.srt`;
                a.click();
              }}>Subtitles (.srt)</Button>
              <Button size="sm" variant="ghost" onClick={() => setOpen(null)}>Close</Button>
            </div>
          </div>
          <div className="console" style={{ maxHeight: 480 }}>{open.text}</div>
        </Card>
      )}
    </>
  );
}
