// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The box's API, as the desktop app uses it. The screens are served by the
// agent itself (same origin), and the desktop shell hands over a token
// (electron/preload.cjs). In a plain browser on the box (development), a
// token can be pasted once and is kept in localStorage.

declare global {
  interface Window {
    ttDesktop?: {
      auth: () => Promise<{ base: string; token: string; name: string }>;
      openAppWindow: (url: string, title: string) => Promise<void>;
      openExternal: (url: string) => Promise<void>;
      startAtLogin: (on?: boolean) => Promise<boolean>;
      notify: (title: string, body?: string) => Promise<void>;
      onNavigate: (fn: (route: string) => void) => void;
      onBoxEvent: (fn: (event: BoxEvent) => void) => void;
      hideTori: () => Promise<void>;
      showTori: (mode?: "talk" | "type") => Promise<void>;
      showMain: (route?: string) => Promise<void>;
      onTori: (fn: (mode: "type" | "talk" | "hidden") => void) => void;
      shortcuts: (on?: boolean) => Promise<boolean>;
      openToplike: (view: string) => Promise<void>;
      toriShortcut: (binding?: string) => Promise<{ binding: string; installed: boolean; dropdown: boolean }>;
      /** Other QuietBoxes this app is paired with (older desktop shells don't have these). */
      boxes?: () => Promise<{ current: string; boxes: { id: string; name: string; host: string; local: boolean }[] }>;
      addBox?: (link: string) => Promise<{ ok: boolean; id?: string; error?: string }>;
      switchBox?: (id: string) => Promise<void>;
      removeBox?: (id: string) => Promise<unknown>;
      /** The pairing screen (find a box on the network). */
      showConnect?: () => Promise<void>;
      /** The box's command line from this computer, over SSH with TT Station's own key. */
      ssh?: (what: "terminal" | "vscode" | "command") => Promise<{ ok: boolean; error?: string; command?: string; user?: string; host?: string }>;
      platform?: string;
      /** Tori's computer tools, on this computer (Tori on another computer than the device). */
      computer?: (tool: string, args: Record<string, unknown>) => Promise<{ ok: boolean; summary?: string; result?: unknown; error?: string }>;
      /** The menu bar and Tori asking the main window to record (another computer). */
      onClientRecord?: (fn: (req: { nonce: string; action: "start" | "stop"; title?: string }) => void) => void;
      clientRecordDone?: (nonce: string, result: { ok: boolean; summary?: string; error?: string }) => void;
    };
  }
}

let token: string | null = null;
export let boxName = "QuietBox";

export async function signIn(): Promise<boolean> {
  if (window.ttDesktop) {
    const auth = await window.ttDesktop.auth();
    token = auth.token;
    boxName = auth.name;
    return true;
  }
  token = localStorage.getItem("tt.token");
  if (token) {
    try { boxName = (await request<Status>("GET", "/status")).name; } catch { /* the gate asks again */ }
  }
  return !!token;
}

/** The pairing token, for the few things that can't send a header (WebSockets). */
export const currentToken = () => token;

export function setToken(t: string) {
  token = t;
  localStorage.setItem("tt.token", t);
}

export class ApiError extends Error {
  constructor(public status: number, message: string, public body?: unknown) {
    super(message);
  }
}

async function request<T>(method: string, path: string, body?: unknown, opts: { raw?: boolean } = {}): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: {
      ...(token ? { authorization: `Bearer ${token}` } : {}),
      ...(body !== undefined && !(body instanceof Blob) ? { "content-type": "application/json" } : {}),
    },
    body: body === undefined ? undefined : body instanceof Blob ? body : JSON.stringify(body),
  });
  if (!res.ok) {
    let message = res.statusText || `Error ${res.status}`;
    let parsed: unknown;
    try {
      parsed = await res.json();
      const e = (parsed as { error?: string })?.error;
      if (e) message = e;
    } catch {
      /* not JSON */
    }
    throw new ApiError(res.status, message, parsed);
  }
  if (opts.raw) return (await res.text()) as T;
  const text = await res.text();
  return (text ? JSON.parse(text) : null) as T;
}

export const get = <T>(path: string) => request<T>("GET", path);
export const post = <T>(path: string, body: unknown = {}) => request<T>("POST", path, body);
export const put = <T>(path: string, body: unknown) => request<T>("PUT", path, body);
export const patch = <T>(path: string, body: unknown) => request<T>("PATCH", path, body);
export const del = <T>(path: string) => request<T>("DELETE", path);
export const delWith = <T>(path: string, body: unknown) => request<T>("DELETE", path, body);
export const text = (path: string) => request<string>("GET", path, undefined, { raw: true });
export const upload = <T>(path: string, file: Blob) => request<T>("POST", path, file);

/** The Authorization header, for the few calls that stream (Lab, chat). */
export const authHeader = (): Record<string, string> => (token ? { authorization: `Bearer ${token}` } : {});

/** A file the box serves behind the token (a picture, a video), as a Blob. */
export async function fetchBlob(path: string): Promise<Blob> {
  const res = await fetch(path, { headers: token ? { authorization: `Bearer ${token}` } : {} });
  if (!res.ok) throw new ApiError(res.status, res.statusText);
  return res.blob();
}

// ---------------------------------------------------------------- types

export interface Status {
  name: string;
  chips: string;
  status: string;
  device_mesh: string | null;
  leasing?: { available: boolean; version: string | null; boards: number | null };
}

export interface Serve {
  phase: "idle" | "starting" | "serving" | "stopping" | "failed";
  model: string | null;
  served_id: string | null;
  kind: string | null;
  since: number;
  eta_s: number | null;
  /** A start that has shown no progress for this long (seconds): it looks stuck. */
  stalled_s?: number | null;
  error: string | null;
  split: boolean;
  companion: { model: string | null; phase: string } | null;
}

export interface Mode {
  id: string;
  title: string;
  blurb: string;
  icon: string;
  models: string[];
  ready: boolean;
  needs_download: string[];
  active: boolean;
  eta_s: number | null;
  note: string | null;
}

export interface TtModel {
  name: string;
  hf_repo: string | null;
  model_type: string;
  status: string | null;
  supported: boolean;
  engines: string[];
  weights: string; // complete | partial | missing | container
  downloaded_bytes: number;
  min_disk_gb: number | null;
  param_count: number | null;
  tool_call_parser: string | null;
}

export interface Pull {
  model: string;
  hf_repo: string | null;
  state: "running" | "done" | "failed" | "cancelled";
  error: string | null;
  action_url: string | null;
  started_at: number;
  bytes_on_disk: number;
  /** While running: bytes against the full size, speed and time left. */
  progress?: { done_bytes: number; total_bytes: number | null; percent: number | null; bytes_per_s: number | null; eta_s: number | null } | null;
}

export interface BoxEvent {
  seq: number;
  at: number;
  kind: string;
  title: string;
  body: string;
}

export interface MetricsNow {
  point: { t: number; chips: { temp_c: number | null; power_w: number | null; aiclk_mhz: number | null }[];
    gen_tps: number | null; prompt_tps: number | null; running: number | null; waiting: number | null;
    kv_cache: number | null; ttft_s: number | null } | null;
  totals: { energy_wh_today: number; tokens_today: number; requests_today: number; energy_wh_all: number;
    tokens_all: number; requests_all: number; since: number } | null;
}

export interface DoctorCheck {
  id: string;
  title: string;
  status: "ok" | "warn" | "fail";
  detail: string;
  fix?: string;
  command?: string;
}

export interface Client {
  tool: string;
  name: string;
  available: boolean;
  running: boolean;
  state: string;
  url: string | null;
}

export interface AppKey {
  id: string;
  name: string;
  /** The first characters, so keys can be told apart (the secret is shown once, at creation). */
  hint: string;
  created: number;
  last_used: number | null;
  requests: number;
  limits: { requests_per_minute: number | null; tokens_per_day: number | null };
  /** What it may use; empty is everything. */
  scopes: string[];
  paused: boolean;
  today: KeyUsage;
  /** The last 14 days, oldest first. */
  days: [string, KeyUsage][];
}

export interface KeyUsage { requests: number; input_tokens: number; output_tokens: number }

export interface Job {
  id: string;
  kind: string;
  title: string;
  when: string;
  status: string;
  done: number;
  total: number;
  duration_s: number | null;
  created: number;
  finished: number | null;
  error: string | null;
}

export interface Person { id: string; name: string }
export interface Schedule { id: string; mode: string; time: string; days: number[]; enabled: boolean }

export interface OrchardRun {
  run_name: string;
  model: string | null;
  state: string;
  live: boolean;
  stage: { current: number | null; name: string | null } | null;
  stages: { stage: number; name: string; status: string; wall_s: number | null }[];
  blocked: { reason: string | null } | null;
  pause: { reason: string | null } | null;
  restore: string | null;
  reports?: string[];
  console?: string[];
  servable?: boolean;
  /** Where each hardware test ran (this box or the lab box) and the configurations not run. */
  hardware?: {
    lab: { host: string; root?: string; hostname?: string; chips?: number } | null;
    tests: { stage: number | null; config: number | null; chips: number; where: string; started: number | null; result: string; detail: string | null }[];
  };
  /** tt-orchard's `refused: …` line, when it wouldn't start. */
  refused?: string | null;
  progress?: {
    title: string;
    detail: string;
    fraction: number | null;
    eta_s: number | null;
    overall: number;
    overall_eta_s: number | null;
    needs_you: string | null;
    hint: string | null;
    typical_s: number[];
  };
}

/** tt-orchard's lab box (`[lab]` in its bringup.toml): every hardware test runs there. */
export interface OrchardLab { host: string; root: string; gozer: string; path: string[]; python: string; test_python: string | null }

export interface OrchardStatus {
  installed: boolean;
  ready: boolean;
  version: string | null;
  setup: { name: string; status: string; detail: string }[];
  setup_running: boolean;
  config: { coder: { target: string; chips: number }; lab?: OrchardLab | null; package_format?: string | null } | null;
  config_error: string | null;
  busy: boolean;
  packaging: boolean;
  runs: OrchardRun[];
  notes: { text: string; command?: string }[];
}

export interface ActionReport {
  id: string;
  tool: string;
  status: "done" | "needs_confirmation" | "error" | "on_client";
  args?: Record<string, unknown>;
  summary: string;
  images?: string[];
}

export type StreamEvent =
  | { type: "delta"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "action"; action: ActionReport }
  | { type: "reset" }
  | { type: "done"; reply: string; model: string }
  | { type: "error"; error: string };

/** Stream an answer from the box (server-sent events from /assistant/stream). */
export async function* streamChat(
  messages: { role: string; content: string }[],
  opts: { think: boolean; detailed: boolean; persona?: "tori"; client?: string },
  signal: AbortSignal,
): AsyncGenerator<StreamEvent> {
  yield* streamPost<StreamEvent>("/assistant/stream",
    { messages, think: opts.think, style: opts.detailed ? "detailed" : "concise", persona: opts.persona, client: opts.client }, signal);
}

export type ExplainEvent =
  | { type: "thinking" }
  | { type: "delta"; text: string }
  | { type: "done"; answer: string; model: string }
  | { type: "error"; error: string };

/** Explain, simplify or ask about part of a finished research report (`/research/{id}/explain`, streamed). */
export function streamExplain(
  id: string,
  body: { selection: string; mode: "explain" | "simpler" | "ask"; question?: string },
  signal: AbortSignal,
): AsyncGenerator<ExplainEvent> {
  return streamPost<ExplainEvent>(`/research/${encodeURIComponent(id)}/explain`, body, signal);
}

/** POST, then read the server-sent events back, one parsed `data:` line each. */
async function* streamPost<T>(path: string, body: unknown, signal: AbortSignal): AsyncGenerator<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
    body: JSON.stringify(body),
    signal,
  });
  if (!res.ok || !res.body) {
    let message = res.statusText;
    try { message = (await res.json()).error ?? message; } catch { /* */ }
    throw new ApiError(res.status, message);
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let cut: number;
    while ((cut = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, cut).trim();
      buffer = buffer.slice(cut + 1);
      if (line.startsWith("data:")) {
        try { yield JSON.parse(line.slice(5).trim()) as T; } catch { /* keep-alive */ }
      }
    }
  }
}

// ---------------------------------------------------------------- telemetry

export interface Chip {
  index: number;
  board: string | null;
  tempC: number | null;
  powerW: number | null;
  aiclk: number | null;
  currentA: number | null;
  memTempC: number | null;
  throttleC: number | null;
  ddrTrained: boolean[];
  // The rest arrive only in full frames (tt-smi -s); null/empty otherwise.
  /** Which board (p300c holds two chips): tt-smi's board_id. */
  boardId: string | null;
  busId: string | null;
  voltage: number | null;
  /** The chip's own limits: TDP (W), TDC (A), max clock (MHz). */
  tdpW: number | null;
  tdcA: number | null;
  fmaxMHz: number | null;
  /** ARC firmware heartbeat counter (ticks ~1/s while the chip's firmware is alive). */
  heartbeat: number | null;
  /** Per GDDR channel (8): °C (hotter of its two sensors), from GDDR_x_y_TEMP. null = no reading. */
  gddrTempC: (number | null)[];
  /** GDDR channels that exist on this chip (ENABLED_GDDR). */
  gddrEnabled: boolean[];
  /** Correctable GDDR errors per channel pair (0-1, 2-3, 4-5, 6-7), and uncorrectable total. */
  gddrCorrErrs: (number | null)[];
  gddrUncorrErrs: number | null;
  /** 14 Tensix columns; false = harvested (ENABLED_TENSIX_COL bit clear). */
  tensixCols: boolean[];
  /** Ethernet ports (14): present (ENABLED_ETH) and with a live link (ETH_LIVE_STATUS). */
  ethEnabled: boolean[];
  /** Per port, when the live mask maps cleanly onto the enabled ports; else empty. */
  ethLive: boolean[];
  /** Live links: set bits of ETH_LIVE_STATUS (how tt-smi's triage counts them). */
  ethLiveCount: number | null;
  /** PCIe: lanes in use now (PCIE_USAGE), negotiated width and generation. */
  pcieLanesUsed: number | null;
  pcieWidth: number | null;
  pcieGen: number | null;
  /** The four L2 CPUs: present, and clocks (MHz). */
  l2cpu: { on: boolean; mhz: number | null }[];
}

/** One process holding a chip open (from tt-smi's process list). */
export interface ChipProcess { pid: number; device: number; cmdline: string; user: string | null }

/** What the full telemetry frame adds beyond per-chip readings. */
export interface FrameExtras {
  processes: ChipProcess[];
  /** tt-toplike's view of the inference server(s): phase, model, stats. */
  inference: Record<string, unknown>[];
  time: string | null;
}

const num = (v: unknown): number | null => {
  if (typeof v === "number") return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v.trim());
    return Number.isFinite(n) ? n : null;
  }
  return null;
};
const int = (v: unknown): number | null => {
  if (typeof v === "number") return v;
  if (typeof v === "string") {
    const t = v.trim();
    const n = t.startsWith("0x") ? parseInt(t, 16) : parseInt(t, 10);
    return Number.isFinite(n) ? n : null;
  }
  return null;
};

const bits = (mask: number | null, n: number): boolean[] =>
  mask == null ? [] : Array.from({ length: n }, (_, i) => Math.floor(mask / 2 ** i) % 2 === 1);

/**
 * ETH_LIVE_STATUS. tt-smi's triage counts its set bits as live links. On this
 * Blackhole firmware the live bits sit in the upper 16 (bit N+16 = port N);
 * a per-port map is only drawn when every live bit lands on a port that
 * ENABLED_ETH says exists, so nothing is guessed.
 */
function ethLink(v: number | null, enabled: number | null): { ethLive: boolean[]; ethLiveCount: number | null } {
  if (v == null) return { ethLive: [], ethLiveCount: null };
  let count = 0;
  for (let x = v; x > 0; x = Math.floor(x / 2)) count += x % 2;
  const low = v & 0xffff, high = Math.floor(v / 0x10000) & 0xffff;
  const fits = (m: number) => enabled != null && (m & ~enabled) === 0;
  const mask = low && fits(low) ? low : !low && high && fits(high) ? high : null;
  return { ethLive: mask == null ? [] : bits(mask, 14), ethLiveCount: count };
}

/** GDDR_x_y_TEMP: four temperature bytes, little-endian; 0xFFFFFFFF = no reading (as tt-toplike). */
function gddrTemps(sm: Record<string, unknown>): (number | null)[] {
  const out: (number | null)[] = [];
  for (const reg of ["GDDR_0_1_TEMP", "GDDR_2_3_TEMP", "GDDR_4_5_TEMP", "GDDR_6_7_TEMP"]) {
    const v = int(sm[reg]);
    if (v == null || v === 0xffffffff) { out.push(null, null); continue; }
    const b = [v & 0xff, (v >>> 8) & 0xff, (v >>> 16) & 0xff, (v >>> 24) & 0xff];
    out.push(Math.max(b[0], b[1]), Math.max(b[2], b[3]));
  }
  return out;
}

export function parseFrame(frame: Record<string, unknown>): { chips: Chip[]; error: string | null; extras: FrameExtras } {
  const procs = Array.isArray(frame.processes) ? (frame.processes as Record<string, unknown>[]) : [];
  const extras: FrameExtras = {
    processes: procs.map((p) => ({ pid: Number(p.pid), device: Number(p.device), cmdline: String(p.cmdline ?? ""), user: (p.user as string) ?? null })),
    inference: (((frame.tt_toplike as Record<string, unknown>)?.inference as Record<string, unknown>[]) ?? []),
    time: (frame.time as string) ?? null,
  };
  if (typeof frame.error === "string" && !frame.device_info) return { chips: [], error: frame.error, extras };
  const devices = (frame.device_info as Record<string, Record<string, unknown>>[]) ?? [];
  const chips = devices.map((d, index) => {
    const t = (d.telemetry ?? {}) as Record<string, unknown>;
    const sm = (d.smbus_telem ?? {}) as Record<string, unknown>;
    const lim = (d.limits ?? {}) as Record<string, unknown>;
    const status = int(sm.DDR_STATUS);
    const mem = int(sm.MAX_GDDR_TEMP);
    const bi = (d.board_info ?? {}) as Record<string, unknown>;
    const ethEnabled = int(sm.ENABLED_ETH);
    const l2 = int(sm.ENABLED_L2CPU);
    return {
      index,
      board: ((d.board_info as Record<string, unknown>)?.board_type as string) ?? null,
      tempC: num(t.asic_temperature),
      powerW: num(t.power),
      aiclk: num(t.aiclk),
      currentA: num(t.current),
      memTempC: mem != null && mem > 0 && mem < 255 ? mem : null,
      throttleC: num(lim.therm_trip_l1_limit),
      ddrTrained: status == null ? [] : Array.from({ length: 8 }, (_, ch) => {
        const n = (status >> (ch * 4)) & 0xf;
        return n === 5 || n === 2;
      }),
      boardId: (bi.board_id as string) ?? null,
      busId: (bi.bus_id as string) ?? null,
      voltage: num(t.voltage),
      tdpW: num(lim.tdp_limit),
      tdcA: num(lim.tdc_limit),
      fmaxMHz: num(lim.asic_fmax),
      heartbeat: num(t.heartbeat) ?? int(sm.TIMER_HEARTBEAT),
      gddrTempC: d.smbus_telem ? gddrTemps(sm) : [],
      gddrEnabled: bits(int(sm.ENABLED_GDDR), 8),
      gddrCorrErrs: ["GDDR_0_1_CORR_ERRS", "GDDR_2_3_CORR_ERRS", "GDDR_4_5_CORR_ERRS", "GDDR_6_7_CORR_ERRS"].map((k) => int(sm[k])),
      gddrUncorrErrs: int(sm.GDDR_UNCORR_ERRS),
      tensixCols: bits(int(sm.ENABLED_TENSIX_COL), 14),
      ethEnabled: bits(ethEnabled, 14),
      ...ethLink(int(sm.ETH_LIVE_STATUS), ethEnabled),
      pcieLanesUsed: int(sm.PCIE_USAGE),
      pcieWidth: num(bi.pcie_width),
      pcieGen: num(bi.pcie_speed),
      l2cpu: bits(l2, 4).map((on, i) => ({ on, mhz: int(sm[`L2CPUCLK${i}`]) })),
    };
  });
  return { chips, error: null, extras };
}

/** Live chip readings over the agent's /telemetry WebSocket. */
export function watchTelemetry(full: boolean, onFrame: (chips: Chip[], error: string | null, extras: FrameExtras) => void): () => void {
  let ws: WebSocket | null = null;
  let closed = false;
  let retry: number | undefined;
  const open = () => {
    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/telemetry${full ? "" : "?view=lite"}`;
    ws = new WebSocket(url);
    ws.onmessage = (m) => {
      try {
        const { chips, error, extras } = parseFrame(JSON.parse(m.data));
        onFrame(chips, error, extras);
      } catch { /* skip */ }
    };
    ws.onclose = () => {
      if (!closed) retry = window.setTimeout(open, 3000);
    };
  };
  open();
  return () => {
    closed = true;
    window.clearTimeout(retry);
    ws?.close();
  };
}

// ---------------------------------------------------------------- formatting

export const short = (model: string | null | undefined) => (model ?? "").split("/").pop() ?? "";

/** The same model under its catalogue and served names ("FLUX.1-schnell" vs "tt-flux.1-schnell"). */
export function sameModel(a: string | null | undefined, b: string | null | undefined): boolean {
  const norm = (s: string | null | undefined) => short(s).toLowerCase().replace(/^tt-/, "");
  return !!a && !!b && norm(a) === norm(b);
}

export function bytes(n: number | null | undefined): string {
  if (n == null) return "–";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1000 && i < units.length - 1) { v /= 1000; i++; }
  return `${v >= 100 || i === 0 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

export function duration(seconds: number | null | undefined): string {
  if (seconds == null) return "–";
  const m = Math.round(seconds / 60);
  if (m < 1) return "under a minute";
  if (m < 60) return `${m} min`;
  return m % 60 === 0 ? `${m / 60} h` : `${Math.floor(m / 60)} h ${m % 60} min`;
}

export function ago(unix: number): string {
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

/** Whether this window is the desktop app on a computer other than the box (where SSH makes sense). */
export async function onAnotherComputer(): Promise<boolean> {
  const b = await window.ttDesktop?.boxes?.().catch(() => null);
  return !!b && !!window.ttDesktop?.ssh && b.current !== "local" && b.boxes.some((x) => x.id === b.current && !x.local);
}

/** Opens the box's terminal (or VS Code) on this computer; says why not if it can't. */
export async function openBoxShell(what: "terminal" | "vscode"): Promise<string | null> {
  const r = await window.ttDesktop?.ssh?.(what);
  return r && !r.ok ? r.error ?? "It didn't open." : null;
}

let clientOnce: Promise<string | undefined> | null = null;
/** Which computer the assistant's computer tools should act on: this one (its
 *  platform) when it isn't the device, else undefined (the device's own). */
export function clientKind(): Promise<string | undefined> {
  clientOnce ??= onAnotherComputer().then((other) => (other ? window.ttDesktop?.platform ?? "desktop" : undefined)).catch(() => undefined);
  return clientOnce;
}

/** Runs an assistant step on this computer and tells the device what happened. */
export async function runOnClient(a: ActionReport): Promise<void> {
  const r = (await window.ttDesktop?.computer?.(a.tool, a.args ?? {}).catch((e: Error) => ({ ok: false, error: e.message })))
    ?? { ok: false, error: "This app can't do that on this computer." };
  await post("/assistant/client-result", { id: a.id, ...r }).catch(() => {});
}

/** A list of actions with this report added, or replacing the one with its id. */
export function upsertAction(list: ActionReport[] | undefined, a: ActionReport): ActionReport[] {
  const all = list ?? [];
  return all.some((x) => x.id === a.id) ? all.map((x) => (x.id === a.id ? a : x)) : [...all, a];
}
