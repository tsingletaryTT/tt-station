// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Small building blocks shared by every screen, on Vesper's tokens.

import type React from "react";
import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useReducer, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { Typography } from "@tenstorrent/vesper/typography";
import { Icon } from "@tenstorrent/vesper/icons";
import { addToast } from "@tenstorrent/vesper/toast";
import { bytes, duration, get, type Chip, type FrameExtras, type Serve, watchTelemetry } from "./api";
import { PollSource, sharedSource } from "./poll";

export function Page({ title, subtitle, actions, children }: {
  title: string; subtitle?: ReactNode; actions?: ReactNode; children: ReactNode;
}) {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <Typography as="h1" variant="heading-lg">{title}</Typography>
          {subtitle && <Typography variant="copy-md" className="muted">{subtitle}</Typography>}
        </div>
        {actions && <div className="page-actions">{actions}</div>}
      </header>
      {children}
    </div>
  );
}

/** Props that make a non-button element act like one: focusable, and Enter or
 *  Space activate it. Keys pressed on a real control inside (a button in a
 *  clickable card) are that control's, not the container's. */
export function pressable(onPress: () => void) {
  return {
    role: "button" as const,
    tabIndex: 0,
    onClick: onPress,
    onKeyDown: (e: ReactKeyboardEvent<HTMLElement>) => {
      if (e.target !== e.currentTarget || (e.key !== "Enter" && e.key !== " ")) return;
      e.preventDefault();
      onPress();
    },
  };
}

export function Card({ children, className = "", onClick, tone }: {
  children: ReactNode; className?: string; onClick?: () => void; tone?: "accent" | "warning" | "danger";
}) {
  return (
    <section className={`card ${tone ? `card-${tone}` : ""} ${onClick ? "card-click" : ""} ${className}`}
      {...(onClick ? pressable(onClick) : {})}>
      {children}
    </section>
  );
}

/** A button that reads as a link inside a sentence ("Browse the catalog"). */
export function LinkButton({ onClick, children, className = "", style }: {
  onClick: (e: React.MouseEvent<HTMLButtonElement>) => void; children: ReactNode; className?: string; style?: React.CSSProperties;
}) {
  return <button type="button" className={`link-button ${className}`} style={style} onClick={onClick}>{children}</button>;
}

/** Whether the person asked their system for less motion; follows changes. */
export function useReducedMotion(): boolean {
  const query = "(prefers-reduced-motion: reduce)";
  const [reduce, setReduce] = useState(() => typeof window !== "undefined" && !!window.matchMedia?.(query).matches);
  useEffect(() => {
    const m = window.matchMedia?.(query);
    if (!m) return;
    const on = () => setReduce(m.matches);
    m.addEventListener?.("change", on);
    return () => m.removeEventListener?.("change", on);
  }, []);
  return reduce;
}

export function Eyebrow({ children }: { children: ReactNode }) {
  return <div className="eyebrow">{children}</div>;
}

export function Stat({ label, value, unit, sub }: { label: string; value: ReactNode; unit?: string; sub?: ReactNode }) {
  return (
    <div className="stat">
      <Eyebrow>{label}</Eyebrow>
      <div className="stat-value">{value}{unit && <span className="stat-unit">{unit}</span>}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  );
}

export function Empty({ title, children, action }: { title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty">
      <Typography variant="heading-xs">{title}</Typography>
      {children && <Typography variant="copy-sm" className="muted">{children}</Typography>}
      {action}
    </div>
  );
}

/** Poll an API call every `ms` while mounted, resting while the window is hidden.
 *  `refresh()` fetches now. */
export function usePoll<T>(fn: () => Promise<T>, ms: number, deps: unknown[] = []) {
  const fnRef = useRef(fn);
  fnRef.current = fn;
  const last = useRef<PollSource<T> | null>(null);
  const source = useMemo(() => {
    const s = new PollSource<T>(() => fnRef.current());
    // New deps: keep showing the last answer until the new one arrives.
    if (last.current) { s.data = last.current.data; s.error = last.current.error; }
    return s;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  last.current = source;
  return useSource(source, ms);
}

/** Like usePoll, but one fetch for everything on screen that polls the same `key`. */
export function useSharedPoll<T>(key: string, fn: () => Promise<T>, ms: number) {
  return useSource(sharedSource(key, fn), ms);
}

/** What's serving, shared by the sidebar, Home, Models and Studio. */
export const useServe = (ms = 3000) => useSharedPoll("/serve", () => get<Serve>("/serve"), ms);

function useSource<T>(source: PollSource<T>, ms: number) {
  const [, rerender] = useReducer((n: number) => n + 1, 0);
  useEffect(() => source.add(ms, rerender), [source, ms]);
  const refresh = useCallback(() => source.fetch(), [source]);
  const setData = useCallback((next: T | null | ((cur: T | null) => T | null)) => source.set(next), [source]);
  return { data: source.data, error: source.error, refresh, setData };
}

/** Live chips; `live` is false when the box isn't reading them (an error frame, or silence). */
export function useTelemetry(full = false) {
  const [chips, setChips] = useState<Chip[]>([]);
  const [extras, setExtras] = useState<FrameExtras | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [at, setAt] = useState(0);
  useEffect(() => watchTelemetry(full, (c, e, x) => {
    if (e) setError(e);
    else {
      setError(null);
      setChips(c);
      setExtras(x);
      setAt(Date.now());
    }
  }), [full]);
  const [, tick] = useState(0);
  useEffect(() => {
    const t = window.setInterval(() => tick((n) => n + 1), 5000);
    return () => window.clearInterval(t);
  }, []);
  return { chips, extras, at, error, live: !error && Date.now() - at < 10000 };
}

export function toastError(e: unknown) {
  addToast({ content: (e as Error).message ?? String(e), variant: "danger", timeout: 6000 });
}
export function toastOk(content: ReactNode) {
  addToast({ content, variant: "success", timeout: 3500 });
}

/** Text onto the clipboard; where the async API is refused, the older select-and-copy. */
export async function writeClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch { /* the fallback below */ }
  const had = document.activeElement as HTMLElement | null;
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.cssText = "position:fixed;top:0;left:0;opacity:0;pointer-events:none";
  document.body.appendChild(area);
  area.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch { ok = false; }
  area.remove();
  had?.focus();
  return ok;
}

/** Copies, and says so only if it worked. */
export async function copyText(text: string, done: ReactNode = "Copied.") {
  if (await writeClipboard(text)) toastOk(done);
  else toastError(new Error("Couldn't copy. Select the text and copy it yourself."));
}

/** Temperature color on Vesper's scale; Blackhole throttles at 90 °C. */
export function tempTone(c: number | null): string {
  if (c == null) return "var(--vesper-text-tertiary)";
  if (c < 65) return "var(--vesper-mint-600)";
  if (c < 78) return "var(--vesper-yellow-600)";
  if (c < 88) return "var(--vesper-amber-600)";
  return "var(--vesper-red-600)";
}

/** The same scale in words, so the color is never the only cue. */
export function tempWord(c: number | null): string {
  if (c == null) return "no reading";
  if (c < 65) return "cool";
  if (c < 78) return "warm";
  if (c < 88) return "hot";
  return "near its limit";
}

/** A small line chart of recent values. Pass `labels` (one per value, e.g. a
 *  time) and `format` to get a hover readout at any point along the line;
 *  `axis` adds the time span and the low and high under it. */
export function Spark({ values, color = "var(--vesper-teal-700)", height = 32, labels, format, axis = false, title }: {
  values: number[];
  color?: string;
  height?: number;
  /** One label per value (e.g. a time); shown in the hover readout. */
  labels?: string[];
  /** Formats a value, with its unit. */
  format?: (v: number) => string;
  /** Show the time span and the range under the line. */
  axis?: boolean;
  /** What the line is, for screen readers ("Box power"). */
  title?: string;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  const [hover, setHover] = useState<number | null>(null);
  const [tipSize, setTipSize] = useState({ w: 0, h: 0 });
  const gradient = `spark-${useId().replace(/:/g, "")}`;
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setWidth(e.contentRect.width));
    ro.observe(el);
    // Now as well: the observer only reports when the page is painted.
    setWidth(el.getBoundingClientRect().width);
    return () => ro.disconnect();
  }, []);
  const n = values.length;
  const h = hover != null && hover < n ? hover : null;
  useLayoutEffect(() => {
    const el = tip.current;
    if (el && (el.offsetWidth !== tipSize.w || el.offsetHeight !== tipSize.h)) setTipSize({ w: el.offsetWidth, h: el.offsetHeight });
  });
  const fmt = format ?? ((v: number) => (Math.abs(v) >= 100 ? v.toFixed(0) : v.toFixed(1)));

  // Missing readings (NaN, or ±Infinity from an empty list) are gaps, not zeros.
  const ok = (v: number) => Number.isFinite(v);
  const real = values.filter(ok);
  // Scale: a little headroom so peaks and the hover dot aren't cut off; a flat line sits mid-height.
  let lo = real.length ? Math.min(...real) : 0, hi = real.length ? Math.max(...real) : 1;
  const pad = (hi - lo || Math.abs(hi) || 1) * 0.1;
  lo -= pad; hi += pad;
  const inset = 3;
  const x = (i: number) => (n < 2 ? width / 2 : (i / (n - 1)) * width);
  const y = (v: number) => inset + (1 - (v - lo) / (hi - lo)) * (height - inset * 2);
  // One run of the line per stretch of readings; a gap where there were none.
  const runs: number[][] = [];
  values.forEach((v, i) => {
    if (!ok(v)) return;
    if (i > 0 && ok(values[i - 1]) && runs.length) runs[runs.length - 1].push(i);
    else runs.push([i]);
  });
  const line = runs.map((r) => r.map((i, k) => `${k ? "L" : "M"}${x(i).toFixed(1)},${y(values[i]).toFixed(1)}`).join("")).join("");
  const area = runs.filter((r) => r.length > 1)
    .map((r) => `${r.map((i, k) => `${k ? "L" : "M"}${x(i).toFixed(1)},${y(values[i]).toFixed(1)}`).join("")}L${x(r[r.length - 1]).toFixed(1)},${height}L${x(r[0]).toFixed(1)},${height}Z`).join("");

  const pick = (clientX: number) => {
    const r = wrap.current?.getBoundingClientRect();
    if (!r || n < 2) return;
    setHover(Math.max(0, Math.min(n - 1, Math.round(((clientX - r.left) / r.width) * (n - 1)))));
  };
  const interactive = n >= 2 && width > 0;

  // The readout sits just above the point, kept inside the chart's width; below it
  // when there's no room above on screen.
  let tipX = 0, tipY = 0;
  if (h != null) {
    const px = x(h), py = y(values[h]);
    const py2 = ok(values[h]) ? py : height / 2;
    tipX = Math.max(0, Math.min(width - tipSize.w, px - tipSize.w / 2));
    tipY = py2 - tipSize.h - 10;
    const top = wrap.current?.getBoundingClientRect().top ?? 0;
    if (top + tipY < 8) tipY = py2 + 12;
  }

  return (
    <div className={`spark-wrap ${h != null ? "hovering" : ""}`}>
      <div ref={wrap} className="spark-plot" style={{ height }}
        tabIndex={interactive ? 0 : undefined}
        role={interactive ? "img" : undefined}
        aria-label={interactive && real.length ? `${title ? `${title}: ` : ""}now ${fmt(real[real.length - 1])}, low ${fmt(Math.min(...real))}, high ${fmt(Math.max(...real))}` : undefined}
        onPointerMove={(e) => pick(e.clientX)}
        onPointerDown={(e) => pick(e.clientX)}
        onPointerLeave={() => setHover(null)}
        onBlur={() => setHover(null)}
        onKeyDown={(e) => {
          if (!interactive) return;
          const step = { ArrowLeft: -1, ArrowRight: 1 }[e.key];
          if (step) setHover((cur) => Math.max(0, Math.min(n - 1, (cur ?? n - 1) + step)));
          else if (e.key === "Home") setHover(0);
          else if (e.key === "End") setHover(n - 1);
          else if (e.key === "Escape") setHover(null);
          else return;
          e.preventDefault();
        }}>
        {width > 0 && n >= 2 && (
          <svg width={width} height={height} className="spark" aria-hidden>
            <defs>
              <linearGradient id={gradient} x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor={color} stopOpacity={0.22} />
                <stop offset="100%" stopColor={color} stopOpacity={0} />
              </linearGradient>
            </defs>
            <path d={area} fill={`url(#${gradient})`} />
            <path d={line} fill="none" stroke={color} strokeWidth={1.75} strokeLinejoin="round" strokeLinecap="round" />
            {h != null && (
              <>
                <line x1={x(h)} x2={x(h)} y1={0} y2={height} className="spark-cross" />
                {ok(values[h]) && <circle cx={x(h)} cy={y(values[h])} r={4} fill={color} className="spark-point" />}
              </>
            )}
          </svg>
        )}
        {h != null && (
          <div ref={tip} className="spark-tip" role="status" style={{ transform: `translate3d(${tipX}px, ${tipY}px, 0)` }}>
            <span className="spark-tip-value"><i style={{ background: color }} />{ok(values[h]) ? fmt(values[h]) : "No reading"}</span>
            {labels?.[h] && <span className="spark-tip-label">{labels[h]}</span>}
          </div>
        )}
      </div>
      {axis && (
        <div className="spark-axis">
          {n >= 2 ? (
            <>
              <span>{labels?.[0] ?? ""}</span>
              <span>{real.length ? `low ${fmt(Math.min(...real))} · high ${fmt(Math.max(...real))}` : "no readings"}</span>
              <span>{labels?.[n - 1] ?? ""}</span>
            </>
          ) : <span>Collecting readings…</span>}
        </div>
      )}
    </div>
  );
}

/** GNOME binding ("<Control><Alt>space") → what people read (["Ctrl", "Alt", "Space"]). */
export function readable(binding: string): string[] {
  const mac = window.ttDesktop?.platform === "darwin";
  const names: Record<string, string> = mac
    ? { Control: "⌃", Primary: "⌃", Alt: "⌥", Shift: "⇧", Super: "⌘" }
    : { Control: "Ctrl", Primary: "Ctrl", Alt: "Alt", Shift: "Shift", Super: "Super" };
  const mods = [...binding.matchAll(/<(\w+)>/g)].map((m) => names[m[1]] ?? m[1]);
  const key = binding.replace(/<\w+>/g, "");
  return [...mods, key === "space" ? "Space" : key.length === 1 ? key.toUpperCase() : key];
}

/** Tori's shortcut as people read it ("Ctrl Alt Space"); follows changes made in Settings. */
export function useToriShortcut(): string {
  const [binding, setBinding] = useState("<Control><Alt>space");
  useEffect(() => {
    window.ttDesktop?.toriShortcut?.().then((r) => setBinding(r.binding)).catch(() => {});
    const changed = (e: Event) => setBinding((e as CustomEvent<string>).detail);
    window.addEventListener("tt:shortcut", changed);
    return () => window.removeEventListener("tt:shortcut", changed);
  }, []);
  return readable(binding).join(" ");
}

/** Whether `text` matches a search box `query`, ignoring case, spaces and
 *  punctuation: "qwen 3.8 flash next" finds "qwen3.8-flash-next_p150x4".
 *  Every word of the query has to appear somewhere. */
export function matches(text: string, query: string): boolean {
  const squash = (s: string) => s.toLowerCase().replace(/[^a-z0-9]+/g, "");
  const hay = squash(text);
  return query.split(/\s+/).map(squash).filter(Boolean).every((w) => hay.includes(w));
}

/** How a download is going, as the agent measures it (`download::progress`). */
export interface Progress {
  done_bytes: number;
  total_bytes: number | null;
  percent: number | null;
  bytes_per_s: number | null;
  eta_s: number | null;
}

/** A download's progress: what it's doing, a bar, how much of how much, speed and time left.
 *  The bar is exact once the full size is known, and moving-but-indeterminate until then. */
export function DownloadProgress({ progress: p, step, paused = false }: { progress: Progress | null | undefined; step?: string; paused?: boolean }) {
  const pct = p?.percent ?? null;
  const known = pct != null;
  const amount = !p ? (paused ? "" : "Starting…")
    : p.total_bytes ? `${bytes(p.done_bytes)} of ${bytes(p.total_bytes)}`
    : p.done_bytes ? `${bytes(p.done_bytes)} so far` : paused ? "" : "Starting…";
  const speed = p?.bytes_per_s && p.bytes_per_s > 64_000 ? `${bytes(p.bytes_per_s)}/s` : null;
  const left = paused ? (p?.total_bytes ? `${bytes(Math.max(0, p.total_bytes - p.done_bytes))} to go` : null)
    : p?.eta_s != null ? (p.eta_s < 60 ? "under a minute left" : `about ${duration(p.eta_s)} left`)
    : p?.total_bytes && p.done_bytes > 0 && p.done_bytes < p.total_bytes ? "estimating time left…" : null;
  return (
    <div className={`dl ${paused ? "paused" : ""}`} role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={known ? pct : undefined}
      aria-valuetext={[step, amount, speed, left].filter(Boolean).join(", ")}>
      <div className="dl-head">
        <span className="dl-step">{step ?? "Downloading"}</span>
        {known && <span className="dl-pct">{pct >= 99.95 ? "100" : pct.toFixed(pct < 10 ? 1 : 0)}%</span>}
      </div>
      <div className={`dl-bar ${known || paused ? "" : "indeterminate"}`}><i style={known ? { width: `${Math.max(1, pct)}%` } : undefined} /></div>
      <div className="dl-meta">
        <span>{amount}</span>
        {(speed || left) && <span>{[speed, left].filter(Boolean).join(" · ")}</span>}
      </div>
    </div>
  );
}

/** A "⋯" button with a short menu; closes on a pick, a click outside or Esc. */
export function MoreMenu({ items }: { items: { label: string; onSelect: () => void; danger?: boolean }[] }) {
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => { if (!box.current?.contains(e.target as Node)) setOpen(false); };
    const esc = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    return () => { document.removeEventListener("mousedown", away); document.removeEventListener("keydown", esc); };
  }, [open]);
  return (
    <div className="code-menu" ref={box}>
      <button className="code-menu-button" aria-label="More" aria-haspopup="menu" aria-expanded={open} title="More" onClick={() => setOpen((v) => !v)}>
        <Icon kind="ellipses" />
      </button>
      {open && (
        <div className="code-menu-list" role="menu">
          {items.map((it) => (
            <button key={it.label} role="menuitem" className={it.danger ? "danger" : ""} onClick={() => { setOpen(false); it.onSelect(); }}>{it.label}</button>
          ))}
        </div>
      )}
    </div>
  );
}
