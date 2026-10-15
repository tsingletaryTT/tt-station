// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Community models: packages people have published to Tenstorrent's community
// catalog (tt-model). Each carries its own code and runtime, so it runs here
// as is. Browse the catalog, see whether one fits this box and what it costs
// to download, get it, and start it like any other model.

import { useEffect, useMemo, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Icon } from "@tenstorrent/vesper/icons";
import type { IconKind } from "@tenstorrent/vesper/icons";
import { Typography } from "@tenstorrent/vesper/typography";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { ago, ApiError, bytes, del, duration, get, post } from "../api";
import { ask, askToStop } from "../dialog";
import { navigate } from "../App";
import { Card, DownloadProgress, Eyebrow, matches, type Progress, toastError, toastOk, usePoll, LinkButton } from "../ui";
import { FavoriteStar, useFavorites } from "../favorites";

interface Installed {
  id: string; name: string; container: boolean; hardware: string; chips: number | null;
  fits: boolean; fit: string; weights: string | null; tools: boolean; max_num_seqs: number | null;
  /** TT Station's test message after it started: did the reply come back readable? */
  check: { ok: boolean; sample: string } | null;
}
interface Download { id: string; running: boolean; failed: boolean; paused: boolean; paused_by_you: boolean; line: string | null; progress: (Progress & { step: string; weights: string | null }) | null }

/** The three steps of a package download, in a person's words. */
const PULL_STEPS: Record<string, [number, string]> = {
  package: [1, "Downloading the package"],
  image: [2, "Loading its container into Docker"],
  weights: [3, "Downloading the weights"],
  finishing: [3, "Finishing up"],
};
interface Community {
  available: boolean; installed: Installed[]; pulls: Download[]; serving: string | null;
  /** A package starting: since when, and the newest step tt-model reports (container started, device opened, …). */
  starting: { id: string; since: number; step: string | null; stalled_s?: number | null } | null;
  /** The last start that failed, and why (until the next start). */
  failed: { id: string; error: string } | null;
  /** A package being stopped: its container shutting down, then its chips reset. */
  stopping: { id: string; since: number } | null;
}
interface Listing { id: string; name: string; author: string; updated: string | null; fits: boolean | null; fit: string }
interface Info {
  id: string; hardware: string; chips: number | null; fits: boolean; fit: string; compatible: boolean | null; verdict: string | null;
  weights: string | null; gated: boolean; weights_access: boolean | null; has_token: boolean;
  package_bytes: number | null; weights_bytes: number | null; weights_on_box: boolean; disk_free_bytes: number | null;
  tools: boolean; reasoning: boolean; max_num_seqs: number | null; built: string | null;
}

/** The same icons the catalog uses, by the model family in the name. */
export function familyIcon(name: string): IconKind {
  const n = name.toLowerCase();
  if (n.includes("llama")) return "model-llama-color";
  if (n.includes("qwen")) return "model-qwen-color";
  if (n.includes("deepseek")) return "model-deepseek-color";
  if (n.includes("gemma")) return "model-google-color";
  if (n.includes("gpt-oss")) return "model-openai";
  return "ai-model";
}

/** tt-model's boot checklist line, without its tick marks and timings. */
function cleanStep(step: string | null): string | null {
  if (!step) return null;
  const t = step.replace(/^[✓→•·\s]+/, "").replace(/\s+\d+(\.\d+)?s$/, "").trim();
  return t ? t[0].toUpperCase() + t.slice(1) : null;
}

export { openCommunity, takeOpenRequest } from "../modelsLink";

export function useCommunity() {
  return usePoll(() => get<Community>("/community"), 3000);
}

/** Downloaded community packages and downloads in progress, as model cards. */
export function CommunityOnBox({ c, query, onServe, onlyFavorites = false }: { c: Community | null | undefined; query: string; onServe: () => void; onlyFavorites?: boolean }) {
  const fav = useFavorites();
  // The click, until the box reports the start (so the card changes at once).
  const [clicked, setClicked] = useState<string | null>(null);
  useEffect(() => { if (clicked && (c?.starting?.id === clicked || c?.failed?.id === clicked)) setClicked(null); }, [c, clicked]);
  if (!c) return null;
  const q = query.trim().toLowerCase();
  const start = async (p: Installed, stopServing = false) => {
    setClicked(p.id);
    try {
      await post("/community/serve", { id: p.id, stop_serving: stopServing });
      toastOk(`Starting ${p.name}. It takes about five minutes; you'll get a notification when it's ready.`);
      onServe();
    } catch (e) {
      setClicked(null);
      const b = e instanceof ApiError ? (e.body as { needs_stop?: boolean; needs_weights?: boolean }) : undefined;
      if (e instanceof ApiError && e.status === 409 && b?.needs_stop) { if (await askToStop(e.message, "")) await start(p, true); }
      else if (e instanceof ApiError && e.status === 409 && b?.needs_weights) {
        // Download first, with progress on this card, rather than a silent download inside the model.
        if (await ask(`Download ${p.name}'s weights first?`, { body: `${e.message} Downloading shows its progress here; start it when it's done.`, confirm: "Download" })) {
          try { await post("/community/pull", { id: p.id }); toastOk(`Downloading ${p.name}'s weights.`); onServe(); } catch (err) { toastError(err); }
        }
      }
      else toastError(e);
    }
  };
  const remove = async (p: Installed) => {
    if (!(await ask(`Remove ${p.name}?`, { body: "The package is deleted from the box. Its weights stay in the download cache, in case another model uses them.", confirm: "Remove", danger: true }))) return;
    try { await del(`/community/${p.id}`); toastOk(`${p.name} removed.`); onServe(); } catch (e) { toastError(e); }
  };
  const busy = new Set(c.pulls.filter((d) => d.running || d.paused).map((d) => d.id));
  const name = (id: string) => id.split("/").pop() ?? id;
  const pause = async (d: Download) => {
    try { await post("/community/pull/pause", { id: d.id }); toastOk(`Paused. Everything downloaded so far is kept; resume any time, even after turning the box off.`); onServe(); } catch (e) { toastError(e); }
  };
  const resume = async (d: Download) => {
    try { await post("/community/pull", { id: d.id }); toastOk(`Resuming ${name(d.id)}.`); onServe(); } catch (e) { toastError(e); }
  };
  const cancel = async (d: Download) => {
    if (!(await ask(`Cancel downloading ${name(d.id)}?`, { body: "It stops and comes off this list. You can download it again from the catalog.", confirm: "Cancel download", cancel: "Keep it" }))) return;
    const got = d.progress?.done_bytes ?? 0;
    const remove = got > 0 && await ask(`Delete the ${bytes(got)} already downloaded?`, {
      body: "Deleting frees the space. Keeping it means a later download of this model starts from there.",
      confirm: "Delete it", cancel: "Keep the files", danger: true,
    });
    try {
      const r = await post<{ freed_bytes: number; kept?: string }>("/community/pull/cancel", { id: d.id, delete_files: remove });
      toastOk(r.kept ?? (remove ? `Cancelled, and freed ${bytes(r.freed_bytes)}.` : "Cancelled. The downloaded files are kept."));
      onServe();
    } catch (e) { toastError(e); }
  };
  return (
    <>
      {c.pulls.filter((d) => matches(d.id, q)).filter((d) => !onlyFavorites || fav.has("community", d.id)).map((d) => (
        <Card key={`dl-${d.id}`} tone={d.failed ? "warning" : undefined}>
          <div className="row-between">
            <Icon kind={familyIcon(d.id)} style={{ width: 26, height: 26 }} />
            <Badge size="sm" variant="contrast" subtle>community</Badge>
          </div>
          <div>
            <Typography variant="heading-xs">{d.id.split("/").pop()}</Typography>
            <span className="muted mono" style={{ fontSize: 12 }}>{d.id}</span>
          </div>
          {d.running ? (() => {
            const [n, what] = PULL_STEPS[d.progress?.step ?? "package"] ?? PULL_STEPS.package;
            return <DownloadProgress progress={d.progress} step={`Step ${n} of 3 · ${what}`} />;
          })() : d.paused ? (
            <DownloadProgress paused progress={d.progress}
              step={d.paused_by_you ? "Paused" : "Stopped when the box turned off or restarted"} />
          ) : <span style={{ fontSize: 13, color: "var(--vesper-text-error)" }}>The download didn't finish{d.line ? `: ${d.line}` : "."}</span>}
          <div className="grow" />
          <div className="row" style={{ gap: 8 }}>
            {d.running && <Button size="sm" variant="subtle" onClick={() => pause(d)}>Pause</Button>}
            {d.paused && <Button size="sm" onClick={() => resume(d)}>Resume</Button>}
            {d.failed && <Button size="sm" variant="subtle" onClick={() => resume(d)}>Try again</Button>}
            <Button size="sm" variant="ghost" onClick={() => cancel(d)}>Cancel</Button>
          </div>
        </Card>
      ))}
      {c.installed.filter((p) => !busy.has(p.id) && matches(p.id, q))
        .filter((p) => !onlyFavorites || fav.has("community", p.id))
        .sort((a, b) => Number(fav.has("community", b.id)) - Number(fav.has("community", a.id)))
        .map((p) => {
        const starting = c.starting?.id === p.id ? c.starting
          : clicked === p.id ? { id: p.id, since: Math.floor(Date.now() / 1000), step: "Getting ready" } : null;
        // While it boots it isn't usable yet: no "Serving", no "Use it".
        const serving = c.serving === p.id && !starting;
        const failed = !starting && !serving && c.failed?.id === p.id ? c.failed.error : null;
        const stopping = c.stopping?.id === p.id ? c.stopping : null;
        const waitFor = c.stopping ? "Wait for the model that's stopping" : c.starting || clicked ? "Another model is starting" : undefined;
        return (
          <Card key={p.id} tone={serving || starting ? "accent" : undefined}>
            <div className="row-between">
              <Icon kind={familyIcon(p.name)} style={{ width: 26, height: 26 }} />
              <div className="row" style={{ gap: 6 }}>
                {serving && <Badge size="sm" variant="mint" subtle>Serving</Badge>}
                {starting && <Badge size="sm" variant="info" subtle>Starting</Badge>}
                {stopping && <Badge size="sm" variant="warning" subtle>Stopping</Badge>}
                <Badge size="sm" variant="contrast" subtle>community</Badge>
                <FavoriteStar kind="community" id={p.id} name={p.name} />
              </div>
            </div>
            <div>
              <Typography variant="heading-xs">{p.name}</Typography>
              <span className="muted mono" style={{ fontSize: 12 }}>{p.id}</span>
            </div>
            <span className="muted" style={{ fontSize: 13 }}>
              {p.fits ? `On this box${p.fit ? ` · ${p.fit.toLowerCase()}` : ""}${p.tools ? " · can use tools" : ""}` : p.fit}
            </span>
            {starting && (
              <div className="stack" style={{ gap: 6 }}>
                <span className="muted mono" style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  {cleanStep(starting.step) ?? "Starting the container…"}
                </span>
                {starting.stalled_s ? (
                  <Admonition variant="warning" subtle size="sm">
                    No progress for {Math.round(starting.stalled_s / 60)} minutes: the start looks stuck. Stop it (its chips are reset), then try again or start another model.
                  </Admonition>
                ) : (
                  <>
                    <div className="bar"><i style={{ width: "100%", background: "var(--vesper-teal-600)", opacity: 0.6, animation: "blink 1.6s ease-in-out infinite" }} /></div>
                    <span className="muted" style={{ fontSize: 12 }}>
                      {duration(Math.max(0, Math.floor(Date.now() / 1000) - starting.since))} so far · a first start takes several minutes (loading weights, compiling kernels)
                    </span>
                  </>
                )}
              </div>
            )}
            {stopping && (
              <div className="stack" style={{ gap: 6 }}>
                <span className="muted" style={{ fontSize: 12 }}>Shutting down, then resetting its chips · {duration(Math.max(0, Math.floor(Date.now() / 1000) - stopping.since))} so far (a few minutes)</span>
                <div className="bar"><i className="bar-blink" style={{ width: "100%", background: "var(--vesper-amber-600)" }} /></div>
              </div>
            )}
            {failed && <Admonition variant="warning" subtle size="sm">It didn't start. {failed}</Admonition>}
            {p.check?.ok === false && (
              <span style={{ fontSize: 12, color: "var(--vesper-text-warning)" }}>
                Its replies came back garbled when TT Station tested it, so Chat and the assistant (Tori) may not work with it.
              </span>
            )}
            <div className="grow" />
            <div className="row">
              {p.fits && !serving && !starting && !stopping && <Button size="sm" disabled={!!waitFor} title={waitFor} onClick={() => start(p)}>{failed ? "Try again" : "Start"}</Button>}
              {serving && <Button size="sm" variant="subtle" onClick={() => navigate("chat")}>Use it</Button>}
              {starting && c.starting?.id === p.id && (
                <Button size="sm" variant={starting.stalled_s ? "primary" : "ghost"} onClick={async () => {
                  if (!(await ask(`Stop starting ${p.name}?`, { body: "It's shut down and its chips are reset (a minute or two). You can start it, or another model, again after.", confirm: "Stop it" }))) return;
                  try { await post("/stop"); toastOk("Stopping it."); onServe(); } catch (e) { toastError(e); }
                }}>Stop</Button>
              )}
              {!serving && !starting && !stopping && <Button size="sm" variant="ghost" onClick={() => remove(p)}>Remove</Button>}
            </div>
          </Card>
        );
      })}
    </>
  );
}

// Package details, fetched once each and a few at a time (each is a `tt-model
// info` plus Hugging Face lookups on the box).
const infoCache = new Map<string, Promise<Info>>();
let inFlight = 0;
const waiting: (() => void)[] = [];
async function fetchInfo(id: string): Promise<Info> {
  if (inFlight >= 4) await new Promise<void>((r) => waiting.push(r));
  inFlight++;
  try {
    return await Promise.race([
      get<Info>(`/community/info/${id}`),
      new Promise<never>((_, no) => setTimeout(() => no(new Error("It took too long to answer.")), 60000)),
    ]);
  } finally {
    inFlight--;
    waiting.shift()?.();
  }
}

function useInfo(id: string) {
  const [data, setData] = useState<Info | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let alive = true;
    setError(null);
    let p = infoCache.get(id);
    if (!p) {
      p = fetchInfo(id);
      infoCache.set(id, p);
      p.catch(() => infoCache.delete(id));
    }
    p.then((v) => { if (alive) setData(v); }, (e) => { if (alive) setError((e as Error).message); });
    return () => { alive = false; };
  }, [id, attempt]);
  return { data, error, retry: () => setAttempt((n) => n + 1) };
}

/** What getting a package costs, and whether anything stands in the way. */
function plan(i: Info) {
  const weightsCost = i.weights_on_box ? 0 : i.weights_bytes ?? 0;
  const total = (i.package_bytes ?? 0) + weightsCost;
  const tooBig = i.disk_free_bytes != null && total > i.disk_free_bytes;
  const why = !i.fits ? i.fit
    : i.compatible === false ? `tt-model says it won't run here: ${i.verdict}`
    : tooBig ? `Not enough free space: it needs ${bytes(total)}.`
    : i.gated && i.weights_access === false ? "Its weights need their license accepted on Hugging Face first."
    : null;
  return { weightsCost, total, why };
}

async function download(id: string, i: Info, onGot: () => void) {
  const { weightsCost, total } = plan(i);
  if (!(await ask(`Download ${id.split("/").pop()}?`, {
    body: `About ${bytes(total)}: the ${bytes(i.package_bytes)} package${weightsCost ? ` and ${bytes(weightsCost)} of weights (${i.weights})` : ""}. It downloads in the background; then press Start on it.`,
    confirm: "Download",
  }))) return;
  try { await post("/community/pull", { id }); toastOk("Downloading. Its card shows the progress, then a Start button."); onGot(); } catch (e) { toastError(e); }
}

const openHf = (repo: string) => { if (window.ttDesktop) window.ttDesktop.openExternal(`https://huggingface.co/${repo}`); else window.open(`https://huggingface.co/${repo}`, "_blank"); };

/**
 * The Community section of Models: community models downloaded to this box
 * (and downloads in progress), under their own heading, below the official ones.
 */
export function CommunitySection({ c, query, filter, onServe, onBrowse, children }: {
  c: Community | null | undefined; query: string; filter: string; onServe: () => void; onBrowse?: () => void; children?: React.ReactNode;
}) {
  const count = (c?.installed.length ?? 0) + (c?.pulls.length ?? 0);
  return (
    <div className="stack">
      <div className="section-title">
        <Eyebrow>{filter === "community" ? "Community models on this box" : filter === "favorites" ? "Community and your bring-ups" : "Community"}</Eyebrow>
        <span className="muted">
          {count > 0 ? "Downloaded to this box from the community catalog. Packaged by people outside Tenstorrent's testing." : "None downloaded yet."}
          {onBrowse && <> <LinkButton onClick={onBrowse}>Browse the catalog</LinkButton></>}
        </span>
      </div>
      <div className="grid grid-3">
        <CommunityOnBox c={c} query={query} onServe={onServe} onlyFavorites={filter === "favorites"} />
        {children}
      </div>
    </div>
  );
}

/** Catalog packages starred but not downloaded yet: the Favorites view's own cards. */
export function CatalogFavorites({ c, query, onGot }: { c: Community | null | undefined; query: string; onGot: () => void }) {
  const fav = useFavorites();
  const [picked, setPicked] = useState<string | null>(null);
  const sheet = useSheet();
  const have = new Set([...(c?.installed ?? []).map((p) => p.id), ...(c?.pulls ?? []).map((d) => d.id)]);
  const q = query.trim().toLowerCase();
  const list = fav.list.filter((f) => f.kind === "catalog").map((f) => f.key.slice("catalog:".length))
    .filter((id) => !have.has(id) && matches(id, q));
  if (!list.length) return null;
  return (
    <div className="stack">
      <div className="section-title"><Eyebrow>Saved from the catalog</Eyebrow><span className="muted">Not downloaded yet.</span></div>
      <div className="grid grid-3">
        {list.map((id) => (
          <CatalogCard key={id} m={{ id, name: id.split("/").pop() ?? id, author: id.split("/")[0], updated: null, fits: null, fit: "" }}
            onGot={onGot} onDetails={() => { setPicked(id); sheet.open(); }} />
        ))}
      </div>
      <Sheet ref={sheet.ref} title={picked?.split("/").pop() ?? "Model"} description={picked ? `${picked} · community package` : ""}>
        {picked && <Details key={picked} id={picked} onGot={() => { sheet.close(); onGot(); }} />}
      </Sheet>
    </div>
  );
}

/** One catalog entry in detail, and getting it. */
function Details({ id, onGot }: { id: string; onGot: () => void }) {
  const info = useInfo(id);
  const i = info.data;
  if (info.error) return (
    <div className="stack" style={{ gap: 8 }}>
      <Admonition variant="danger" subtle size="sm">Couldn't read its details: {info.error}</Admonition>
      <div><Button size="sm" variant="subtle" onClick={info.retry}>Try again</Button></div>
    </div>
  );
  if (!i) return <span className="muted">Reading its details…</span>;
  const { total, why } = plan(i);
  return (
    <div className="stack" style={{ gap: 14 }}>
      <div className="list">
        <div className="list-row"><span className="muted grow">Runs on</span><span>{i.fits ? i.fit || "this box" : <b>{i.fit}</b>}</span></div>
        {i.weights && <div className="list-row"><span className="muted grow">Model</span><span className="mono" style={{ fontSize: 12 }}>{i.weights}</span></div>}
        <div className="list-row"><span className="muted grow">Can use tools</span><span>{i.tools ? "Yes" : "No"}{i.reasoning ? " · thinks before answering" : ""}</span></div>
        {i.max_num_seqs != null && <div className="list-row"><span className="muted grow">People at once</span><span>{i.max_num_seqs}</span></div>}
        <div className="list-row"><span className="muted grow">Download</span>
          <span>{bytes(total)}{i.weights_on_box ? " (weights already here)" : i.weights_bytes ? ` (${bytes(i.package_bytes)} package + ${bytes(i.weights_bytes)} weights)` : ""}</span></div>
        {i.disk_free_bytes != null && <div className="list-row"><span className="muted grow">Free on the box</span><span>{bytes(i.disk_free_bytes)}</span></div>}
        {i.built && <div className="list-row"><span className="muted grow">Published</span><span>{new Date(i.built).toLocaleDateString()}</span></div>}
      </div>
      {why && <Admonition variant="warning" subtle size="sm">{why}
        {i.gated && i.weights_access === false && i.weights && <> <LinkButton onClick={() => openHf(i.weights!)}>Open its page</LinkButton></>}
      </Admonition>}
      <div className="row">
        <Button disabled={!!why} onClick={() => download(id, i, onGot)}>Download</Button>
        <Button variant="ghost" onClick={() => openHf(id)}>See it on Hugging Face</Button>
      </div>
    </div>
  );
}

/** A catalog card: what it is, what it costs, and Download right there. */
function CatalogCard({ m, onDetails, onGot }: { m: Listing; onDetails: () => void; onGot: () => void }) {
  const info = useInfo(m.id);
  const i = info.data;
  const p = i ? plan(i) : null;
  const fits = i ? i.fits : m.fits;
  const fit = i ? i.fit : m.fit;
  return (
    <Card>
      <div className="row-between">
        <Icon kind={familyIcon(m.name)} style={{ width: 26, height: 26 }} />
        <div className="row" style={{ gap: 6 }}>
          {fits === true ? <Badge size="sm" variant="mint" subtle>{fit.startsWith("Uses all") ? "Made for this box" : fit}</Badge>
            : fits === false ? <Badge size="sm" variant="contrast" subtle>Won't fit</Badge> : null}
          <FavoriteStar kind="catalog" id={m.id} name={m.name} />
        </div>
      </div>
      <div>
        <Typography variant="heading-xs">{m.name}</Typography>
        <span className="muted mono" style={{ fontSize: 12 }}>by {m.author}{m.updated ? ` · ${new Date(m.updated).toLocaleDateString()}` : ""}</span>
      </div>
      <span className="muted" style={{ fontSize: 13 }}>
        {info.error ? "Couldn't read its details."
          : !i || !p ? "Reading its details…"
          : p.why ?? `${bytes(p.total)} download${i.weights_on_box ? " (weights already here)" : ""}${i.tools ? " · can use tools" : ""}`}
      </span>
      <div className="grow" />
      <div className="row">
        {i && p && !p.why && <Button size="sm" onClick={() => download(m.id, i, onGot)}>Download</Button>}
        {info.error && <Button size="sm" variant="subtle" onClick={info.retry}>Try again</Button>}
        <Button size="sm" variant="ghost" onClick={onDetails}>Details</Button>
      </div>
    </Card>
  );
}

/** Made for the whole box first, then ones that use part of it, then unknown. */
const rank = (m: Listing) => (m.fits === true ? (m.fit.startsWith("Uses all") ? 3 : 2) : m.fits == null ? 1 : 0);

/** The community catalog, for the Community tab. */
export function CommunityCatalog({ c, query, onGot }: { c: Community | null | undefined; query: string; onGot: () => void }) {
  const [scope, setScope] = useState<"catalog" | "all">("catalog");
  const [refreshing, setRefreshing] = useState(false);
  const catalog = usePoll(() => get<{ models: Listing[]; fetched_at: number }>(`/community/catalog?all=${scope === "all"}`), 600000, [scope]);
  const refresh = async () => {
    setRefreshing(true);
    try {
      const fresh = await get<{ models: Listing[]; fetched_at: number }>(`/community/catalog?all=${scope === "all"}&refresh=true`);
      infoCache.clear();
      catalog.setData(fresh);
      toastOk(`${fresh.models.length} community models.`);
    } catch (e) { toastError(e); } finally { setRefreshing(false); }
  };
  const [picked, setPicked] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);
  const sheet = useSheet();
  const have = new Set([...(c?.installed ?? []).map((p) => p.id), ...(c?.pulls ?? []).filter((d) => d.running || d.paused).map((d) => d.id)]);
  const list = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (catalog.data?.models ?? [])
      .filter((m) => !have.has(m.id))
      .filter((m) => matches(m.id, q))
      .filter((m) => showAll || m.fits !== false)
      // Made for this box first, then unknown, newest first within.
      .sort((a, b) => rank(b) - rank(a) || (b.updated ?? "").localeCompare(a.updated ?? ""));
  }, [catalog.data, query, showAll, c]);
  const hidden = (catalog.data?.models ?? []).filter((m) => m.fits === false).length;
  return (
    <div className="stack">
      <div className="section-title">
        <Eyebrow>{scope === "catalog" ? "From the community catalog" : "Everything published"}</Eyebrow>
        <span className="muted">
          {scope === "catalog" ? "Models people have packaged for Tenstorrent chips and listed in the catalog." : "Every package pushed to Hugging Face for Blackhole, listed in the catalog or not."}
          {" "}Each brings its own code, so it runs as is.
        </span>
      </div>
      <div className="row-between wrap">
        <div className="pill-tabs">
          <button className={scope === "catalog" ? "on" : ""} onClick={() => setScope("catalog")}>Catalog</button>
          <button className={scope === "all" ? "on" : ""} onClick={() => setScope("all")}>Everything published</button>
        </div>
        <div className="row" style={{ gap: 8 }}>
          {catalog.data && <span className="muted" style={{ fontSize: 12 }}>
            {catalog.data.models.length} models · checked {ago(catalog.data.fetched_at)}
          </span>}
          <Button size="sm" variant="subtle" iconLeft={<Icon kind="reset" />} disabled={refreshing} onClick={refresh}>{refreshing ? "Checking…" : "Refresh"}</Button>
        </div>
      </div>
      {catalog.error && <Admonition variant="warning" subtle size="sm">Couldn't load the catalog: {catalog.error}</Admonition>}
      {!catalog.data && !catalog.error && <span className="muted">Loading the catalog…</span>}
      <div className="grid grid-3">
        {list.map((m) => <CatalogCard key={m.id} m={m} onGot={onGot} onDetails={() => { setPicked(m.id); sheet.open(); }} />)}
      </div>
      {hidden > 0 && <div><Button size="sm" variant="ghost" onClick={() => setShowAll((v) => !v)}>{showAll ? "Hide the ones that won't fit" : `Show ${hidden} that won't fit this box`}</Button></div>}
      <Sheet ref={sheet.ref} title={picked?.split("/").pop() ?? "Model"} description={picked ? `${picked} · community package` : ""}>
        {picked && <Details key={picked} id={picked} onGot={() => { sheet.close(); onGot(); }} />}
      </Sheet>
    </div>
  );
}
