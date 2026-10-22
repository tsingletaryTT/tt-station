// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The "Model for Code" list (`GET /code/models`, `codemodels.rs`), shaped for
// the picker: every model the box could run Code on, from the tt catalog (and
// whatever is serving now), grouped by model. Choosing one calls the box's
// serve route, as Models does.

export type CacheState = "on" | "off" | "unknown";
export interface PrefixCache { state: CacheState; how: string }
export type Choice = { action: "in_use" } | { action: "switch" } | { action: "busy"; why: string } | { action: "unavailable"; why: string };
export type Source = "catalog" | "running";
export interface ServeCall { route: string; body: Record<string, unknown> }

export interface CodeModel {
  key: string;
  /** The model, the same for all its setups: what the picker groups by. */
  group: string;
  name: string;
  model: string;
  source: Source;
  /** Where it comes from: "tt catalog". */
  origin: string;
  profile: string | null;
  profile_label: string | null;
  profile_note: string | null;
  chips: number | null;
  context: number | null;
  tools: { verified: boolean; how: string };
  prefix_cache: PrefixCache;
  params_b: number | null;
  served_id: string | null;
  speed: { decode_tps: number | null; ttft_s: number | null; steps: number; setup_unrecorded?: boolean } | null;
  reliability: { window: number | null; valid: number; trials: number; at: number; setup_unrecorded: boolean } | null;
  choice: Choice;
  eta_s: number | null;
  serve: ServeCall | null;
}

export interface CodeModels {
  code_model: string | null;
  serve: { phase: "idle" | "starting" | "serving" | "stopping" | "failed"; model: string | null; since: number; error: string | null; eta_s: number | null };
  catalog_error: string | null;
  models: CodeModel[];
}

export interface Group { key: string; name: string; options: CodeModel[]; inUse: boolean; verified: boolean }

/** The list in groups, one per model, in the order the box sorted it (in use, verified, unverified). */
export function groupModels(models: CodeModel[]): Group[] {
  const out: Group[] = [];
  const at = new Map<string, Group>();
  for (const m of models) {
    let g = at.get(m.group);
    if (!g) {
      g = { key: m.group, name: m.name, options: [], inUse: false, verified: false };
      at.set(m.group, g);
      out.push(g);
    }
    g.options.push(m);
    g.inUse ||= m.choice.action === "in_use";
    g.verified ||= m.tools.verified;
  }
  return out;
}

const SOURCE: Record<Source, string> = { catalog: "tt catalog", running: "Serving now" };

/** One option's line: "4 chips · low latency", else where it comes from. */
export function optionTitle(m: CodeModel): string {
  if (m.profile_label) return m.profile_label;
  if (m.source === "catalog") return m.chips ? `${m.chips} ${m.chips === 1 ? "chip" : "chips"} · tt catalog` : "tt catalog";
  return SOURCE[m.source];
}

/** Where an option comes from, under its title. */
export function optionOrigin(m: CodeModel): string {
  if (m.source === "running") return m.model;
  return SOURCE.catalog;
}

/** The serve routes a choice may call: the box's own start, nothing else. */
const ROUTES = new Set(["/serve"]);

/** The request that starts `m`, if it's one the picker may send. */
export function serveRequest(m: CodeModel): ServeCall | null {
  return m.serve && ROUTES.has(m.serve.route) ? m.serve : null;
}

/** What choosing `m` does, beyond stopping what serves: its chips. */
export function chipsLine(m: CodeModel): string | null {
  if (!m.chips) return null;
  // Boxes differ (one card, a QuietBox, a Galaxy): the count, not a share of a total.
  return `It uses ${m.chips} ${m.chips === 1 ? "chip" : "chips"} on the box.`;
}

/** Valid tool calls within a retry, as a share: "97% of 120". */
export function reliabilityText(r: NonNullable<CodeModel["reliability"]>): string {
  const pct = r.trials ? Math.round((r.valid / r.trials) * 100) : 0;
  return `${pct}% valid tool calls (${r.valid}/${r.trials})${r.window ? ` · holds up to ${Math.round(r.window / 1024)}K` : ""}`;
}
