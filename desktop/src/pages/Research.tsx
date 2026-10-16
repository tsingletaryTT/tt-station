// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Research: ask a hard question, and the box plans it, searches, reads dozens
// of pages, and writes a report where every claim points at its source
// (`/research`, run by the agent on the served model). Reports stay here, a
// notification says when one is ready, and a follow-up builds on what the
// last one read. Tori starts them too ("research …").
//
// Reports are per person: a member sees their own and the ones shared with
// everyone on the box; owners see all (the box decides, `GET /research`).
// Whoever started a report, or an owner, renames, stops, deletes or shares it.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Icon } from "@tenstorrent/vesper/icons";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Badge } from "@tenstorrent/vesper/badge";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { ProgressBar } from "@tenstorrent/vesper/progress-bar";
import { StatusIndicator } from "@tenstorrent/vesper/status-indicator";
import { addToast } from "@tenstorrent/vesper/toast";
import { Typography } from "@tenstorrent/vesper/typography";
import Markdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { ago, del, duration, get, post } from "../api";
import { Card, copyText, Eyebrow, MoreMenu, Page, toastError, toastOk, usePoll } from "../ui";
import { askText } from "../dialog";
import { type Check, type Job, live, NewResearch } from "./ResearchCard";
import { fileName, ownershipLabel, reportFiles, shareAction, zipFiles, type ReportText } from "../reports";
import { deleteWithUndo, UNDO_MS } from "../undo";
import { navigate } from "../App";
import { ExplainPanel, SelectionMenu, useReportSelection, type ExplainRequest, type Picked } from "./ResearchExplain";
import { linkCitations, type ExplainMode } from "./explainHelpers";

interface Source { n: number; url: string; title: string | null; query: string; status: string; relevant: boolean; notes: { claim?: string; quote?: string }[]; error?: string | null }
interface Detail {
  job: Job; events: { ts: number; kind: string; text: string }[]; report: string | null; sources: Source[];
  counts: { found: number; read: number; relevant: number; failed: number }; plan: { title?: string; sub_questions?: string[] } | null;
  /** Whether this device may rename, stop, delete and share it (its starter or an owner). */
  can_change?: boolean;
}

const mine = (u: string) => u.startsWith("file://") || u.startsWith("doc:");
const save = (blob: Blob, name: string) => {
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = name;
  a.click();
  URL.revokeObjectURL(a.href);
};
/** What the page offers to start when nothing is serving. */
const startQwen = async () => { try { await post("/serve", { model: "Qwen3-32B", force: true }); toastOk("Starting Qwen3-32B. It takes a few minutes."); } catch (e) { toastError(e); } };
const openLink = (href: string) => window.ttDesktop ? window.ttDesktop.openExternal(href) : window.open(href, "_blank", "noopener");
/** The report's links: citations scroll to their source, other links open outside the app. Made once, so polling doesn't remount them (and drop a selection). */
const REPORT_COMPONENTS: Components = {
  a: ({ href, children }) => href?.startsWith("#src-")
    ? <a href={href} className="cite" onClick={(e) => { e.preventDefault(); document.getElementById(href.slice(1))?.scrollIntoView({ behavior: "smooth", block: "center" }); }}>{children}</a>
    : <a href={href} onClick={(e) => { e.preventDefault(); if (href) void openLink(href); }}>{children}</a>,
};
const domain = (u: string) => { if (mine(u)) return "Your Library"; try { return new URL(u).hostname.replace(/^www\./, ""); } catch { return u; } };

export function Research() {
  const list = usePoll(() => get<{ jobs: Job[]; check: Check; owner?: boolean }>("/research"), 4000);
  const owner = list.data?.owner !== false;
  // This device's id, to tell its own reports from others' (an owner sees all).
  const me = usePoll(() => get<{ devices: { id: string; this_device?: boolean }[] }>("/devices")
    .then((r) => r.devices.find((d) => d.this_device)?.id ?? null).catch(() => null), 60000).data ?? null;
  // Deleted here, removed from the box once the undo window has passed.
  const [gone, setGone] = useState<string[]>([]);
  const jobs = (list.data?.jobs ?? []).filter((j) => !gone.includes(j.id));
  const [zipping, setZipping] = useState(false);
  const check = list.data?.check ?? null;
  const [picked, setPicked] = useState<string | null>(() => location.hash.replace("#", "") || null);
  const sel = jobs.find((j) => j.id === picked) ?? jobs.find((j) => live(j.state)) ?? jobs[0] ?? null;
  const [asking, setAsking] = useState(false);
  useEffect(() => { if (sel) history.replaceState(null, "", `/app/research#${sel.id}`); }, [sel?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  const started = (j: Job) => { setPicked(j.id); setAsking(false); list.refresh(); };
  const remove = (j: Job) => {
    const undo = deleteWithUndo({
      hide: () => { setGone((g) => [...g, j.id]); setPicked(null); },
      show: () => setGone((g) => g.filter((x) => x !== j.id)),
      remove: () => del(`/research/${j.id}`),
      removed: list.refresh,
      failed: toastError,
    });
    addToast({ content: "Report deleted.", timeout: UNDO_MS, action: {
      content: "Undo", altText: "Bring the report back",
      handler: () => { undo(); setPicked(j.id); },
    } });
  };
  const ready = jobs.filter((j) => j.state === "done");
  const downloadAll = async () => {
    setZipping(true);
    try {
      const reports: ReportText[] = [];
      for (const j of ready) {
        const d = await get<Detail>(`/research/${j.id}`);
        reports.push({ title: d.plan?.title ?? j.question, text: d.report });
      }
      const files = reportFiles(reports);
      const count = Object.keys(files).length;
      if (!count) { toastError(new Error("None of the finished reports has any text yet.")); return; }
      save(new Blob([zipFiles(files) as BlobPart], { type: "application/zip" }), `research-reports-${new Date().toISOString().slice(0, 10)}.zip`);
      toastOk(`${count} report${count === 1 ? "" : "s"} downloaded as a zip.`);
    } catch (e) { toastError(e); } finally { setZipping(false); }
  };

  return (
    <Page title="Research" subtitle="Ask a hard question. The box plans it, searches, reads dozens of pages and writes a report with every claim linked to its source. Nothing leaves the box but the searches."
      actions={<>
        {ready.length > 1 && <Button variant="subtle" iconLeft={<Icon kind="download" />} disabled={zipping} onClick={downloadAll}>{zipping ? "Zipping…" : "Download all"}</Button>}
        <Button iconLeft={<Icon kind="add" />} onClick={() => setAsking(true)}>New research</Button>
      </>}>
      <ReadyBanner check={check} />
      {(asking || jobs.length === 0) && <NewResearch check={check} onStarted={started} onClose={jobs.length ? () => setAsking(false) : undefined} />}

      {jobs.length > 0 && (
        <div className="research-layout">
          <div className="stack" style={{ gap: 8 }}>
            <div className="research-list" role="tablist" aria-label="Reports">
              {jobs.map((j) => (
                <button key={j.id} role="tab" aria-selected={sel?.id === j.id} className={`research-item ${sel?.id === j.id ? "on" : ""}`} onClick={() => setPicked(j.id)}>
                  <span className="research-q">{j.title ?? j.question}</span>
                  <span className="row" style={{ gap: 6 }}>
                    <StateBadge s={j.state} />
                    <span className="muted" style={{ fontSize: 11 }}>{ago(j.created)}</span>
                    {(() => { const l = ownershipLabel(j, !!me && j.started_by === me, owner); return l && <span className="muted" style={{ fontSize: 11 }}>{l}</span>; })()}
                  </span>
                </button>
              ))}
            </div>
          </div>
          {sel && <JobView key={sel.id} id={sel.id} onChanged={list.refresh} onStarted={started} onDelete={() => remove(sel)}
            canStartModel={!!check && !check.model && check.level === "block"} />}
        </div>
      )}
    </Page>
  );
}

function StateBadge({ s }: { s: string }) {
  const [label, variant] = ({
    queued: ["waiting", "contrast"], running: ["researching", "info"], done: ["ready", "mint"], failed: ["didn't finish", "danger"], stopped: ["stopped", "contrast"],
  } as Record<string, [string, "contrast" | "info" | "mint" | "danger"]>)[s] ?? [s, "contrast"];
  return <Badge size="sm" variant={variant} subtle>{label}</Badge>;
}

/** Whether the serving model and search are ready, and what to do if not. */
function ReadyBanner({ check }: { check: Check | null }) {
  // While private search is being set up: its progress (from the app store).
  const searx = usePoll(() => get<{ apps: { id: string; state: string; step: string | null; error: string | null }[] }>("/apps")
    .then((r) => r.apps.find((a) => a.id === "searxng") ?? null).catch(() => null), 3000);
  const sx = searx.data;
  const settingUp = sx?.state === "installing" || (sx?.state === "running" && check?.search !== "searxng");
  if (!check) return null;
  const modelOk = check.level === "ok" || check.level === "warn";
  if (modelOk && check.level === "ok" && check.search === "searxng") return null;
  const setUp = async () => {
    try { await post("/apps/searxng/start"); searx.refresh(); } catch (e) { toastError(e); }
  };
  return (
    <div className="stack" style={{ gap: 8 }}>
      {!modelOk && (
        <Admonition variant="warning" subtle size="sm">
          <div className="row-between wrap" style={{ gap: 8 }}>
            <span>{check.why}</span>
            {!check.model && check.level === "block" && <Button size="xs" onClick={startQwen}>Start Qwen3-32B</Button>}
          </div>
        </Admonition>
      )}
      {check.level === "warn" && <Admonition variant="info" subtle size="sm">{check.why}</Admonition>}
      {check.search !== "searxng" && (
        <Admonition variant="info" subtle size="sm">
          {settingUp ? (
            <div className="row" style={{ gap: 8 }}>
              <StatusIndicator label={`Setting up private search: ${sx?.step ?? "starting"}…`} state="progress" animated />
            </div>
          ) : sx?.state === "failed" ? (
            <div className="row-between wrap" style={{ gap: 8 }}>
              <span>Private search didn't set up: {sx.error}</span>
              <Button size="xs" variant="subtle" onClick={setUp}>Try again</Button>
            </div>
          ) : (
            <div className="row-between wrap" style={{ gap: 8 }}>
              <span>
                <b>Searches go to DuckDuckGo for now.</b> Private search installs SearXNG, a search engine that runs in a container on this box and asks
                Google, Bing and others for you, so your questions aren't tied to you. No account or sign-in; a one-time 380 MB download, ready in about a minute.
              </span>
              <Button size="xs" variant="subtle" onClick={setUp}>Set up private search</Button>
            </div>
          )}
        </Admonition>
      )}
    </div>
  );
}

/** The question box. Exported for Home's shortcut. */

function JobView({ id, onChanged, onStarted, onDelete, canStartModel }: {
  id: string; onChanged: () => void; onStarted: (j: Job) => void; onDelete: () => void; canStartModel: boolean;
}) {
  const d = usePoll(() => get<Detail>(`/research/${id}`), 3000, [id]);
  const det = d.data;
  const [follow, setFollow] = useState("");
  const [showLog, setShowLog] = useState(false);
  const j = det?.job;
  const running = !!j && live(j.state);

  // Citations [3] become links to the source below.
  const report = useMemo(() => (det?.report ? linkCitations(det.report) : null), [det?.report]);

  // Select part of the finished report to explain it, simplify it, or ask about it.
  const reportBox = useRef<HTMLDivElement>(null);
  const reportText = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const { picked, pointer, reread } = useReportSelection(reportText);
  const [menu, setMenu] = useState<{ picked: Picked; keyboard: boolean } | null>(null);
  const [asked, setAsked] = useState<ExplainRequest | null>(null);
  const done = j?.state === "done";
  useEffect(() => { if (pointer && done) { const p = reread(); if (p) setMenu({ picked: p, keyboard: false }); } }, [pointer]); // eslint-disable-line react-hooks/exhaustive-deps
  const openedByKeyboard = useRef(false);
  openedByKeyboard.current = !!menu?.keyboard;
  const closeMenu = useCallback((restoreFocus: boolean) => {
    if (restoreFocus && openedByKeyboard.current) trigger.current?.focus();
    setMenu(null);
  }, []);
  const explain = (mode: ExplainMode, question?: string) => {
    if (!menu) return;
    setAsked((a) => ({ key: (a?.key ?? 0) + 1, selection: menu.picked.text, mode, question }));
    setMenu(null);
  };
  const closePanel = () => { setAsked(null); reportText.current?.focus({ preventScroll: true }); };
  const researchFurther = (q: string) => {
    setFollow(q);
    setAsked(null);
    requestAnimationFrame(() => { const el = document.getElementById("research-follow-up") as HTMLInputElement | null; el?.scrollIntoView({ behavior: "smooth", block: "center" }); el?.focus({ preventScroll: true }); });
  };

  if (!det || !j) return <Card><span className="muted">Loading…</span></Card>;

  const stop = async () => { try { await post(`/research/${id}/stop`); d.refresh(); onChanged(); } catch (e) { toastError(e); } };
  const rename = async () => {
    const t = await askText("Name this report", j.question);
    if (t && t.trim() && t.trim() !== j.question) {
      try { await post(`/research/${id}/rename`, { title: t.trim() }); toastOk('Report renamed.'); onChanged(); } catch (e) { toastError(e); }
    }
  };
  const again = async () => { try { const r = await post<{ job: Job }>("/research", { question: j.question, depth: j.depth }); onStarted(r.job); } catch (e) { toastError(e); } };
  const followUp = async () => {
    try { const r = await post<{ job: Job }>("/research", { question: follow.trim(), depth: "quick", parent: id }); setFollow(""); toastOk("Following up. It builds on what this report read."); onStarted(r.job); }
    catch (e) { toastError(e); }
  };
  const canChange = det.can_change !== false;
  const sharing = shareAction(j, canChange);
  const share = async (shared: boolean) => {
    try {
      await post(`/research/${id}/share`, { shared });
      toastOk(shared ? "Shared. Everyone on the box can read it now." : "Not shared any more. Only you and the box's owners see it.");
      d.refresh(); onChanged();
    } catch (e) { toastError(e); }
  };
  const download = () => save(new Blob([det.report ?? ""], { type: "text/markdown" }), fileName(det.plan?.title ?? j.question, "md"));
  const elapsed = (j.finished ?? Date.now() / 1000) - j.created;

  return (
    <div className="stack" style={{ minWidth: 0 }}>
      <Card tone={j.state === "done" ? "accent" : j.state === "failed" ? "warning" : undefined}>
        <div className="row-between" style={{ alignItems: "flex-start", gap: 12 }}>
          <div className="stack grow" style={{ gap: 4, minWidth: 0 }}>
            <div className="row wrap" style={{ gap: 6 }}>
              <StateBadge s={j.state} />
              {j.shared && <Badge size="sm" variant="info" subtle>Shared</Badge>}
              <span className="muted" style={{ fontSize: 12 }}>
                {j.depth} · {j.model?.split("/").pop() ?? "the box's model"} · {j.search === "searxng" ? "private search" : "web search"} · {duration(Math.max(0, elapsed))}{running ? " so far" : ""}
              </span>
            </div>
            {canChange ? (
              <button className="title-button" title="Rename this report" onClick={() => void rename()}>
                <Typography variant="heading-sm">{j.title ?? det.plan?.title ?? j.question}</Typography>
              </button>
            ) : <Typography variant="heading-sm">{j.title ?? det.plan?.title ?? j.question}</Typography>}
            {!canChange && j.started_by_name && <span className="muted" style={{ fontSize: 12 }}>Shared by {j.started_by_name}</span>}
            {(j.title || det.plan?.title) && <span className="muted" style={{ fontSize: 13 }}>{j.question}</span>}
          </div>
          <div className="row" style={{ gap: 6 }}>
            {running && canChange && <Button size="sm" variant="ghost" onClick={stop}>Stop</Button>}
            {j.state === "done" && <><Button size="sm" variant="subtle" onClick={download}>Download</Button>
              <Button size="sm" variant="ghost" onClick={() => void copyText(det.report ?? "", "Report copied as Markdown.")}>Copy</Button></>}
            {(j.state === "failed" || j.state === "stopped") && <Button size="sm" onClick={again}>Try again</Button>}
            {canChange && <MoreMenu items={[
              { label: "Rename", onSelect: () => void rename() },
              ...(sharing ? [{ label: sharing.label, onSelect: () => void share(sharing.shared) }] : []),
              ...(running ? [] : [{ label: "Delete report", danger: true, onSelect: onDelete }]),
            ]} />}
          </div>
        </div>
        {running && (
          <div className="stack" style={{ gap: 6 }}>
            <div className="row-between"><b style={{ fontSize: 14 }}>{j.step}</b>
              <span className="muted mono" style={{ fontSize: 12 }}>{det.counts.found} found · {det.counts.read} read · {det.counts.relevant} useful</span></div>
            <ProgressBar value={stageFraction(j.step, det)} animated aria-label="Research progress" />
          </div>
        )}
        {j.state === "failed" && j.error && <Admonition variant="warning" subtle size="sm">{j.error}</Admonition>}
      </Card>

      {running && det.plan?.sub_questions && (
        <Card>
          <Eyebrow>The plan</Eyebrow>
          <ol className="guide-list">{det.plan.sub_questions.map((s) => <li key={s}>{s}</li>)}</ol>
        </Card>
      )}

      {(running || showLog) && <ActivityLog events={det.events} />}

      {report && (
        <Card>
          <div className="row-between" style={{ alignItems: "center" }}>
            <Eyebrow>The report</Eyebrow>
            <span className="row" style={{ gap: 6 }}>
              {done && (picked || menu) && (
                <Button ref={trigger} size="xs" variant="subtle" aria-haspopup="true" aria-expanded={!!menu}
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => { const p = reread() ?? menu?.picked; if (p) setMenu({ picked: p, keyboard: true }); }}>Ask about the selection</Button>
              )}
              <Button size="xs" variant="ghost" onClick={() => void copyText(det.report ?? "", "Report copied as Markdown.")}>Copy</Button>
            </span>
          </div>
          {done && !asked && <span className="muted" style={{ fontSize: 12 }}>Select any part of the report to have it explained, said more simply, or to ask about it.</span>}
          <div ref={reportBox} className="research-report-box">
            <div ref={reportText} tabIndex={-1} className="markdown selectable research-report">
              <Markdown remarkPlugins={[remarkGfm]} components={REPORT_COMPONENTS}>{report.split("\n## Sources")[0]}</Markdown>
            </div>
            {menu && <SelectionMenu key={menu.picked.text} picked={menu.picked} box={reportBox} focusFirst={menu.keyboard} onPick={explain} onClose={closeMenu} />}
          </div>
        </Card>
      )}

      {asked && done && (
        <ExplainPanel key={asked.key} reportId={id} req={asked} onClose={closePanel} onFollowUp={researchFurther}
          onRetry={() => setAsked((a) => (a ? { ...a, key: a.key + 1 } : a))} startModel={canStartModel ? () => void startQwen() : undefined} />
      )}

      {det.sources.length > 0 && j.state === "done" && (
        <Card>
          <div className="row-between wrap"><Eyebrow>Sources ({det.sources.length})</Eyebrow>
            <span className="muted" style={{ fontSize: 12 }}>Read {det.counts.read} of {det.counts.found} pages found; these {det.sources.length} had something on the question.</span></div>
          <ol className="source-list">
            {det.sources.map((s) => (
              <li key={s.n} id={`src-${s.n}`} className="source">
                <details>
                  <summary>
                    <span className="source-n">{s.n}</span>
                    <span className="source-title">{s.title ?? s.url}</span>
                    <span className="source-domain">{domain(s.url)}</span>
                  </summary>
                  <div className="stack" style={{ gap: 6, padding: "8px 0 2px 26px" }}>
                    {s.notes.map((n, i) => (
                      <div key={i} className="source-note">
                        {n.claim && <div>{n.claim}</div>}
                        {n.quote && <div className="muted source-quote">“{n.quote}”</div>}
                      </div>
                    ))}
                    {mine(s.url)
                      ? <span className="muted mono" style={{ fontSize: 12, overflowWrap: "anywhere" }}>{s.url.startsWith("file://") ? `On the box: ${s.url.slice(7)}` : "Saved on the box"}</span>
                      : <a className="source-open" href={s.url} onClick={(e) => { e.preventDefault(); void openLink(s.url); }}>Open the page ↗</a>}
                  </div>
                </details>
              </li>
            ))}
          </ol>
        </Card>
      )}

      {j.state === "done" && (
        <Card>
          <Eyebrow>Follow up</Eyebrow>
          <div className="row">
            <div className="grow"><TextInput id="research-follow-up" aria-label="Follow-up question" value={follow} onChange={(e) => setFollow(e.target.value)} placeholder="Go deeper on one part, e.g. licensing for the top three"
              onKeyDown={(e) => { if (e.key === "Enter" && follow.trim().length >= 8) void followUp(); }} /></div>
            <Button variant="subtle" disabled={follow.trim().length < 8} onClick={followUp}>Follow up</Button>
          </div>
          <div className="row-between wrap">
            <span className="muted" style={{ fontSize: 12 }}>A follow-up starts from the sources this report already read.</span>
            <span className="row" style={{ gap: 14 }}>
              <button className="link-button" onClick={() => setShowLog((v) => !v)}>{showLog ? "Hide how it worked" : "How it worked"}</button>
              <button className="link-button" onClick={() => navigate("connect")}>More research apps</button>
            </span>
          </div>
        </Card>
      )}
      {!running && j.state !== "done" && det.events.length > 0 && !showLog && <div><button className="link-button" onClick={() => setShowLog(true)}>What happened</button></div>}
    </div>
  );
}

/** A rough place in the run, for the bar: planning, searching, reading, writing. */
function stageFraction(step: string, d: Detail) {
  const s = step.toLowerCase();
  if (s.startsWith("waiting")) return 2;
  if (s.startsWith("planning")) return 8;
  if (s.startsWith("searching")) return 20;
  if (s.startsWith("reading")) return 25 + 50 * (d.counts.found ? (d.counts.read + d.counts.failed) / d.counts.found : 0);
  if (s.startsWith("looking")) return 78;
  if (s.startsWith("writing")) return 90;
  return 50;
}

function ActivityLog({ events }: { events: Detail["events"] }) {
  const icon: Record<string, string> = { plan: "🧭", search: "🔎", read: "📄", gap: "❓", write: "✍️", done: "✅", error: "⚠️", info: "•" };
  const shown = events.slice(-60);
  return (
    <Card>
      <div className="row-between" style={{ alignItems: "center" }}>
        <Eyebrow>What it's doing</Eyebrow>
        <span className="muted" style={{ fontSize: 12 }}>{events.length} step{events.length === 1 ? "" : "s"}</span>
      </div>
      <ol className="almanac">
        {shown.map((e, i) => (
          <li key={i} className={`note ${e.kind === "error" ? "bad" : ""} ${i === shown.length - 1 ? "latest" : ""}`} style={{ gridTemplateColumns: "64px 24px 1fr" }}>
            <span className="note-time mono">{new Date(e.ts * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23" })}</span>
            <span aria-hidden="true">{icon[e.kind] ?? "•"}</span>
            <span className="note-text">{e.text}</span>
          </li>
        ))}
      </ol>
    </Card>
  );
}

/** Home's shortcut: ask, and land on the Research page. */

