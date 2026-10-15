// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// "Your data" in Settings: the household's data settings, and plain words for
// what an import of an export would do (`POST /import?dry_run=true`).

export interface DataSettings {
  keep_audio: boolean;
  transcripts_to_library: boolean;
  retention_days: number | null;
  restart_last_model: boolean;
}

export interface ImportCounts { added: number; overwritten: number; unchanged: number }

export interface ImportPreview {
  agent_version: string | null;
  format_version: number;
  created: number | null;
  box: string | null;
  categories: Record<string, ImportCounts>;
  overwritten: string[];
  skipped: string[];
  bytes: number;
}

/** How long the box keeps transcripts and reports: the choices offered. */
export const RETENTION_CHOICES: { value: string; label: string }[] = [
  { value: "keep", label: "Keep them" },
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
  { value: "180", label: "6 months" },
  { value: "365", label: "A year" },
];

/** A retention setting as a choice's value, and back. */
export const retentionValue = (days: number | null | undefined): string => (days ? String(days) : "keep");
export const retentionDays = (value: string): number | null => (value === "keep" ? null : Number(value) || null);

/** One thing and many, in the preview's words. */
const NOUNS: Record<string, [string, string]> = {
  research: ["research report", "research reports"],
  transcripts: ["transcript", "transcripts"],
  recordings: ["recording", "recordings"],
  memories: ["memory", "memories"],
  people: ["person", "people"],
  routines: ["routine", "routines"],
  routine_runs: ["routine run", "routine runs"],
  schedules: ["schedule", "schedules"],
  prefs: ["settings file", "settings files"],
  evals: ["model check-up", "model check-ups"],
  code: ["Code session", "Code sessions"],
  creations: ["image prompt", "image prompts"],
};

const ORDER = Object.keys(NOUNS);

function count(n: number, kind: string): string {
  const [one, many] = NOUNS[kind] ?? [kind, kind];
  return `${n} ${n === 1 ? one : many}`;
}

function list(parts: string[]): string {
  if (parts.length <= 1) return parts[0] ?? "";
  return `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}`;
}

/** What an import would do, in a sentence or three. */
export function describePreview(p: ImportPreview): string {
  const kinds = Object.keys(p.categories).sort((a, b) => ORDER.indexOf(a) - ORDER.indexOf(b));
  const added = kinds.filter((k) => p.categories[k].added > 0).map((k) => count(p.categories[k].added, k));
  const replaced = kinds.filter((k) => p.categories[k].overwritten > 0).map((k) => count(p.categories[k].overwritten, k));
  const from = p.box ? ` from ${p.box}` : "";
  const parts: string[] = [];
  if (added.length === 0 && replaced.length === 0) {
    parts.push(`This export${from} has nothing this box doesn't already have.`);
  } else {
    if (added.length) parts.push(`Adds ${list(added)}.`);
    if (replaced.length) parts.push(`Replaces ${list(replaced)} with the export's copy.`);
  }
  if (p.skipped.length) parts.push(`Leaves out ${p.skipped.length} file${p.skipped.length === 1 ? "" : "s"} it can't bring in.`);
  return parts.join(" ");
}

/** Whether there is anything to import. */
export const changesSomething = (p: ImportPreview): boolean =>
  Object.values(p.categories).some((c) => c.added > 0 || c.overwritten > 0);

/** The file name the box suggested (`Content-Disposition`), else a plain one. */
export function exportFileName(disposition: string | null, today = new Date()): string {
  const m = /filename="?([^";]+)"?/i.exec(disposition ?? "");
  const name = (m?.[1] ?? "").replace(/[^A-Za-z0-9._-]/g, "");
  if (name && name.endsWith(".tar.gz") && !name.startsWith(".")) return name;
  return `tt-station-${today.toISOString().slice(0, 10)}.tar.gz`;
}
