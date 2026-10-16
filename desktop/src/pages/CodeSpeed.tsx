// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// How fast Code runs, and on which model (`codespeed.rs`).
//
// The speed strip sits under a session's header: how much of the context the
// conversation fills against where it compacts and the whole window, and the
// last model step's time to first token, decode speed and cached prompt. It
// warns as the conversation nears compaction. "Steps" opens the last few.
//
// The model button opens "Model for Code": every model on the box Code could
// run on (`codeModels.ts`), from the tt catalog, community packages (each
// profile its own option) and native vLLM bundles, grouped by model. Each says
// its window, chips, whether its server caches prompt prefixes, how fast it ran
// here and how reliably it made tool calls in a check-up, and flags tool
// calling nobody has verified. Choosing one that isn't serving starts it
// through its source's own serve route, the way Models does, after saying
// plainly what that stops for everyone.

import { useEffect, useRef, useState } from "react";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Badge } from "@tenstorrent/vesper/badge";
import { Button } from "@tenstorrent/vesper/button";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import { get, post } from "../api";
import { navigate } from "../App";
import { chipsLine, groupModels, optionOrigin, optionTitle, reliabilityText, serveRequest } from "../codeModels";
import type { CacheState, CodeModel, CodeModels, PrefixCache } from "../codeModels";
import { toastError, toastOk, usePoll } from "../ui";

interface Step {
  at: number; model: string | null; prompt_tokens: number; cached_tokens: number | null; output_tokens: number;
  ttft_ms: number | null; decode_tps: number | null; seconds: number;
}
interface Speed { model: string | null; window: number; compact_at: number; prefix_cache: PrefixCache | null; steps: Step[] }

/** One plain sentence on what a missing prefix cache costs an agent. */
export const NO_CACHE_COST = "Without prefix caching the model re-reads the whole conversation at every step, so each step waits longer as the session grows.";
const CACHE_LABEL: Record<CacheState, string> = { on: "Prefix cache on", off: "Prefix cache off", unknown: "Prefix cache unknown" };
const cacheTitle = (p: PrefixCache) =>
  p.state === "on" ? `It reuses the conversation it has already read, so each step reads only what's new (${p.how}).`
    : p.state === "off" ? `${NO_CACHE_COST} (${p.how})`
      : `Can't tell whether it caches prefixes: ${p.how}. ${NO_CACHE_COST}`;

/** 812 → "812", 41 230 → "41.2K", 131 072 → "131K". */
const k = (n: number) => (n < 1000 ? `${n}` : n < 100_000 ? `${(n / 1000).toFixed(1)}K` : `${Math.round(n / 1000)}K`);
const secs = (ms: number) => (ms < 10_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 1000)} s`);
/** Code never asks for more than this (`codespeed::window_and_compaction`). */
const CODE_WINDOW = 131_072;
const short = (m: string | null | undefined) => (m ? m.split("/").pop() ?? m : "");
const minutes = (s: number) => (s < 90 ? "about a minute" : `about ${Math.round(s / 60)} minutes`);
const clock = (s: number) => { const t = Math.max(0, Math.round(s)); return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, "0")}`; };

// ---------------------------------------------------------------- the strip

/** Where the context stands: fine, nearing compaction (from 80% of the way), or there. */
export function contextTone(used: number, compactAt: number): "ok" | "near" | "full" {
  if (!compactAt || used < compactAt * 0.8) return "ok";
  return used < compactAt ? "near" : "full";
}

export function SpeedStrip({ session, working }: { session: string; working: boolean }) {
  // Not while the window is hidden, once there's something to show.
  const have = useRef(false);
  const speed = usePoll(() => (document.hidden && have.current ? Promise.reject(new Error("hidden")) : get<Speed>(`/code/sessions/${session}/speed`)), working ? 2500 : 10000, [session]);
  have.current = !!speed.data;
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => { if (!wrap.current?.contains(e.target as Node)) setOpen(false); };
    const esc = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    return () => { document.removeEventListener("mousedown", away); document.removeEventListener("keydown", esc); };
  }, [open]);

  const d = speed.data;
  // A hidden window skips its poll: that's not an error worth showing.
  if (!d) {
    return (
      <div className="code-speed" aria-live="off">
        <span className="muted">{speed.error && speed.error !== "hidden" ? "Speed isn't available right now." : "Loading speed…"}</span>
      </div>
    );
  }
  const last = d.steps[d.steps.length - 1];
  const used = last?.prompt_tokens ?? 0;
  const tone = contextTone(used, d.compact_at);
  const pct = (n: number) => `${Math.min(100, (n / Math.max(1, d.window)) * 100)}%`;
  const cachedPct = last?.cached_tokens != null && last.prompt_tokens ? Math.round((last.cached_tokens / last.prompt_tokens) * 100) : null;
  const meterLabel = `Context: ${k(used)} tokens of ${k(d.compact_at)} before it compacts, in a ${k(d.window)} window`;

  return (
    <div className="code-speed" ref={wrap}>
      <div className={`code-speed-meter ${tone}`} role="meter" aria-label={meterLabel} aria-valuemin={0} aria-valuemax={d.compact_at} aria-valuenow={Math.min(used, d.compact_at)}
        title={`${meterLabel}. At ${k(d.compact_at)} the conversation is summarized so it fits; the marker shows where.`}>
        <span className="code-speed-fill" style={{ width: pct(used) }} />
        <span className="code-speed-mark" style={{ left: pct(d.compact_at) }} aria-hidden="true" />
      </div>
      <span className={`code-speed-num ctx ${tone}`}>
        {last ? <>{k(used)}<span className="muted"> / {k(d.compact_at)}</span></> : <span className="muted">– / {k(d.compact_at)}</span>}
      </span>
      {tone !== "ok" && <span className={`code-speed-warn ${tone}`} role="status">{tone === "near" ? "Compacts soon" : "Compaction due"}</span>}
      <span className="code-speed-sep" aria-hidden="true" />
      <span className="code-speed-item hide-small" title="Time to first token: waiting in line plus reading the part of the prompt that isn't cached.">
        <span className="muted">First token</span> <span className="code-speed-num w6">{last?.ttft_ms != null ? secs(last.ttft_ms) : "–"}</span>
      </span>
      <span className="code-speed-item" title="How fast it writes once it starts, in tokens a second.">
        <span className="muted">Decode</span> <span className="code-speed-num w9">{last?.decode_tps != null ? `${Math.round(last.decode_tps)} tok/s` : "–"}</span>
      </span>
      <span className="code-speed-item hide-narrow" title={last && last.cached_tokens == null ? "The model server doesn't report cached tokens (vLLM does with --enable-prompt-tokens-details)." : "Prompt tokens the server already had cached at the last step."}>
        <span className="muted">Cached</span> <span className="code-speed-num w11">{last?.cached_tokens != null ? `${k(last.cached_tokens)} (${cachedPct}%)` : last ? "not reported" : "–"}</span>
      </span>
      <span className="grow" />
      {d.prefix_cache && (
        <span className={`code-speed-cache ${d.prefix_cache.state}`} title={cacheTitle(d.prefix_cache)} tabIndex={0}
          aria-label={`${CACHE_LABEL[d.prefix_cache.state]}. ${cacheTitle(d.prefix_cache)}`}>
          {CACHE_LABEL[d.prefix_cache.state]}
        </span>
      )}
      {!d.model && <span className="muted">No model serving</span>}
      <button className="code-speed-toggle" aria-expanded={open} aria-controls={`speed-${session}`} disabled={!d.steps.length}
        title={d.steps.length ? "The last model steps" : "Steps show here once the model has answered"} onClick={() => setOpen((o) => !o)}>
        Steps{d.steps.length ? ` ${d.steps.length}` : ""} {open ? "▴" : "▾"}
      </button>
      {open && d.steps.length > 0 && (
        <div className="code-speed-pop" id={`speed-${session}`} role="region" aria-label="Recent model steps">
          <table className="code-speed-table">
            <thead><tr><th scope="col">Step</th><th scope="col">Context</th><th scope="col">Cached</th><th scope="col">First token</th><th scope="col">Decode</th><th scope="col">Wrote</th></tr></thead>
            <tbody>
              {[...d.steps].reverse().slice(0, 12).map((s, i) => (
                <tr key={`${s.at}-${i}`}>
                  <td>{i === 0 ? "last" : `−${i}`}</td>
                  <td>{k(s.prompt_tokens)}</td>
                  <td>{s.cached_tokens != null ? k(s.cached_tokens) : "–"}</td>
                  <td>{s.ttft_ms != null ? secs(s.ttft_ms) : "–"}</td>
                  <td>{s.decode_tps != null ? `${Math.round(s.decode_tps)} tok/s` : "–"}</td>
                  <td>{k(s.output_tokens)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <div className="code-speed-foot muted">
            Window {k(d.window)} · compacts at {k(d.compact_at)}{d.model ? ` · ${short(d.model)}` : ""}
            {d.prefix_cache?.state !== "on" && <div>{NO_CACHE_COST}</div>}
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- the model for Code

/** The model name in a session's header (or a new session's form), opening the picker. */
export function CodeModelButton({ model }: { model: string | null | undefined }) {
  const sheet = useSheet();
  const [open, setOpen] = useState(false);
  return (
    <>
      <button className="code-model-button" onClick={() => { setOpen(true); sheet.open(); }}
        title="Choose the model Code uses" aria-haspopup="dialog" aria-label={`Model for Code: ${short(model) || "none serving"}. Change it`}>
        {short(model) || "No model"} <span aria-hidden="true">▾</span>
      </button>
      <Sheet ref={sheet.ref} popover title="Model for Code" description="Every Code session runs on the model the box is serving. Pick which one."
        onClose={() => setOpen(false)}
        onToggle={(e) => { if ((e.nativeEvent as ToggleEvent).newState === "closed") setOpen(false); }}>
        {open && <ModelPicker />}
      </Sheet>
    </>
  );
}

export function ModelPicker() {
  const m = usePoll(() => get<CodeModels>("/code/models"), 3000);
  const [asked, setAsked] = useState<string | null>(null);
  // The option whose switch is being confirmed, in place: a dialog would open
  // behind this sheet.
  const [confirming, setConfirming] = useState<string | null>(null);
  const list = useRef<HTMLUListElement>(null);
  const [, tick] = useState(0);
  const d = m.data;
  const starting = d?.serve.phase === "starting";
  useEffect(() => {
    if (!starting) return;
    const t = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => window.clearInterval(t);
  }, [starting]);

  if (!d) {
    return m.error
      ? <Admonition variant="warning" subtle size="sm">Couldn't load the models: {m.error}. Retrying…</Admonition>
      : <span className="muted" role="status">Loading models…</span>;
  }
  const current = d.models.find((x) => x.choice.action === "in_use");
  const groups = groupModels(d.models);

  const eta = (x: CodeModel) => (x.eta_s ? minutes(x.eta_s) : "several minutes");
  // Back to the option's own button once its confirm closes, so the keyboard stays put.
  const cancel = (x: CodeModel) => {
    setConfirming(null);
    window.requestAnimationFrame(() => list.current?.querySelector<HTMLElement>(`[data-use="${CSS.escape(x.key)}"]`)?.focus());
  };
  const choose = async (x: CodeModel) => {
    setConfirming(null);
    const req = serveRequest(x);
    if (!req) { toastError(new Error(`${x.name} can't be started from here. Start it in Models.`)); return; }
    setAsked(x.key);
    try {
      await post(req.route, req.body);
      toastOk(`Starting ${x.name} (${optionTitle(x)}). It takes ${eta(x)}; you'll get a notification.`);
      void m.refresh();
    } catch (e) { setAsked(null); toastError(e); }
  };

  const since = Date.now() / 1000 - d.serve.since;
  const startingThis = (x: CodeModel) => starting && (asked === x.key || (!asked && (d.serve.model === x.model || d.serve.model === x.name)));
  return (
    <div className="code-models">
      {starting && (
        <div className="code-models-progress" role="status">
          <span className="code-status-dot live" aria-hidden="true" />
          <span>Starting {short(d.serve.model)}…</span>
          <span className="grow" />
          <span className="mono muted">{clock(since)}{d.serve.eta_s ? ` of about ${clock(d.serve.eta_s)}` : ""}</span>
        </div>
      )}
      {d.serve.phase === "stopping" && <div className="code-models-progress" role="status"><span className="code-status-dot live" aria-hidden="true" />Stopping the current model…</div>}
      {d.serve.phase === "failed" && (
        <Admonition variant="danger" subtle size="sm">{short(d.serve.model) || "The model"} didn't start: {d.serve.error ?? "no reason given"}. Try again, pick another, or see Models.</Admonition>
      )}
      {d.catalog_error && <Admonition variant="warning" subtle size="sm">Couldn't read the box's model catalog: {d.catalog_error}. Community packages and bundles are still listed.</Admonition>}
      {current?.prefix_cache.state === "off" && <Admonition variant="info" subtle size="sm">{NO_CACHE_COST}</Admonition>}

      {groups.length === 0 ? (
        <div className="code-models-empty">
          <b>No models on the box can run Code yet.</b>
          <span className="muted">Code needs a chat model whose weights are on the box, that fits its chips and can make tool calls. Download one in Models (the tt catalog or a community package), then pick it here.</span>
          <div><Button size="sm" variant="subtle" onClick={() => navigate("models")}>Open Models</Button></div>
        </div>
      ) : (
        <ul className="code-models-list" ref={list} aria-label="Models Code can run on">
          {groups.map((g) => (
            <li key={g.key} className={`code-model ${g.inUse ? "on" : ""}`} aria-labelledby={`cm-${g.key}`}>
              <div className="row-between" style={{ gap: 8 }}>
                <b className="code-model-name" id={`cm-${g.key}`}>{g.name}</b>
                {g.options.length > 1 && <span className="muted code-model-count">{g.options.length} ways to run it</span>}
              </div>
              <ul className="code-model-options">
                {g.options.map((x) => (
                  <li key={x.key} className={`code-model-option ${x.choice.action === "in_use" ? "on" : ""}`}>
                    <div className="code-model-option-head">
                      <span className="code-model-option-title">{optionTitle(x)}</span>
                      {x.choice.action === "in_use" && <Badge size="sm" variant="mint" subtle>In use</Badge>}
                      {startingThis(x) && <Badge size="sm" variant="info" subtle>Starting</Badge>}
                      {!x.tools.verified && (
                        <span className="code-model-unverified" tabIndex={0} title={`Tool calling unverified: ${x.tools.how}.`}
                          aria-label={`Tool calling unverified: ${x.tools.how}.`}>
                          <Badge size="sm" variant="warning" subtle>Tool calling unverified</Badge>
                        </span>
                      )}
                      <span className="grow" />
                      {x.choice.action === "switch" && confirming !== x.key && (
                        <Button size="sm" variant="subtle" data-use={x.key} disabled={starting || asked === x.key}
                          aria-label={`Use ${x.name}, ${optionTitle(x)}, for Code`} onClick={() => setConfirming(x.key)}>Use for Code</Button>
                      )}
                    </div>
                    <span className="muted code-model-origin" title={x.profile_note ?? undefined}>{optionOrigin(x)}</span>
                    <dl className="code-model-facts inline">
                      <div><dt>Window</dt><dd>{x.context ? `${k(x.context)}${x.context > CODE_WINDOW ? ` (Code uses ${k(CODE_WINDOW)})` : ""}` : "unknown"}</dd></div>
                      <div><dt>Chips</dt><dd>{x.chips ?? "unknown"}</dd></div>
                      <div><dt>Prefix cache</dt><dd className={`cache-${x.prefix_cache.state}`} title={cacheTitle(x.prefix_cache)}>
                        {x.prefix_cache.state === "on" ? "yes" : x.prefix_cache.state === "off" ? "no" : "unknown"}</dd></div>
                      <div><dt>Speed here</dt><dd>{x.speed?.decode_tps != null
                        ? <span title={x.speed.setup_unrecorded ? "Measured before the box recorded which package profile was serving." : undefined}>
                          {Math.round(x.speed.decode_tps)} tok/s{x.speed.ttft_s != null ? ` · first token ${x.speed.ttft_s} s` : ""} <span className="muted">({x.speed.steps} steps{x.speed.setup_unrecorded ? ", setup not recorded" : ""})</span></span>
                        : "not measured yet"}</dd></div>
                      <div><dt>Code reliability</dt><dd>{x.reliability
                        ? <span title="Agent reliability from the latest check-up (Evals): valid tool calls within one retry, in Code-like sessions.">
                          {reliabilityText(x.reliability)}{x.reliability.setup_unrecorded ? <span className="muted"> (setup not recorded)</span> : null}</span>
                        : "no check-up yet"}</dd></div>
                    </dl>
                    {x.prefix_cache.state === "off" && x.choice.action !== "in_use" && <span className="muted code-model-note">{NO_CACHE_COST}</span>}
                    {x.choice.action === "switch" && confirming === x.key && (
                      <SwitchConfirm option={x} current={d.code_model} eta={eta(x)} onCancel={() => cancel(x)} onConfirm={() => void choose(x)} />
                    )}
                    {(x.choice.action === "busy" || x.choice.action === "unavailable") && <span className="muted code-model-note">{x.choice.why}</span>}
                  </li>
                ))}
              </ul>
            </li>
          ))}
        </ul>
      )}
      <span className="muted code-models-foot">Every model on the box that could run Code, from the tt catalog, community packages (each profile that fits) and native vLLM bundles. Speeds are what Code measured on this box in that setup: the median of its recent steps. Code uses up to {k(CODE_WINDOW)} of a model's window.</span>
    </div>
  );
}

/** What a switch does, said before it happens. */
function SwitchConfirm({ option, current, eta, onCancel, onConfirm }: { option: CodeModel; current: string | null; eta: string; onCancel: () => void; onConfirm: () => void }) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => { box.current?.querySelector<HTMLButtonElement>(".code-model-confirm-go button")?.focus(); }, []);
  const id = `confirm-${option.key.replace(/[^a-z0-9]/gi, "-")}`;
  const what = option.profile_label ? `${option.name} (${option.profile_label})` : option.name;
  const chips = chipsLine(option);
  return (
    <div ref={box} className="code-model-confirm" role="group" aria-labelledby={`${id}-t`} aria-describedby={`${id}-b`}
      onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); onCancel(); } }}>
      <b id={`${id}-t`}>Switch the box to {what}?</b>
      <div id={`${id}-b`} className="code-model-confirm-body">
        <p>{current ? <>{short(current)} stops for everyone using the box: Chat, Code, apps and anything connected to it.</> : "Nothing is serving right now, so no one is interrupted."}</p>
        <p>The chips are reset and {option.name} loads, which takes {eta}.{chips ? ` ${chips}` : ""} Code can't answer until it's up.</p>
        {!option.tools.verified && <p>Its tool calling isn't verified ({option.tools.how}). If it can't make tool calls, Code can't read, edit or run anything with it.</p>}
        <p>Open sessions carry on with it afterwards but keep the context settings they started with. Start a new session to use its full window.</p>
      </div>
      <div className="row" style={{ gap: 8 }}>
        <Button size="sm" variant="ghost" onClick={onCancel}>Cancel</Button>
        <div className="code-model-confirm-go"><Button size="sm" variant={current ? "danger" : "primary"} onClick={onConfirm}>{current ? `Stop ${short(current)} and switch` : `Start ${option.name}`}</Button></div>
      </div>
    </div>
  );
}
