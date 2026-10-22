// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Notifications: everything on the box that's waiting for this person, in one
// list -- the desktop's version of the phone's "Waiting for you".
//
// It gathers what each feature already exposes and doesn't ask the box for
// anything new:
// - `GET /approvals`: Code sessions' questions (`code:<session>:<request>`)
//   and routines' (`routine:<run>`, and `routine:<run>:<n>` for a step it
//   queued after reading the web). The box already leaves out what this
//   device mayn't answer (an unsandboxed session's, for a member).
// - `GET /events` → `pairing` events of the last two minutes: someone asked to
//   pair and the box showed a code. Owners only (the box sends them only to
//   owners). Nothing to allow: the person reads the code out, or ignores it.
//
// The pure part (gathering, ordering, answering) lives here, tested; the
// page and the bell at the top right share one poll of each source (`useInbox`).

import { get, post } from "./api";

/** How long a pairing code works (the box's `PAIR_TTL`). */
export const PAIR_TTL_S = 120;

/** One thing `GET /approvals` lists. */
export interface BoxApproval {
  id: string;
  kind: "code" | "routine" | string;
  where?: string;
  summary: string;
  tool?: string;
  since?: number;
  can_always?: boolean;
  session?: string;
  run?: string;
  queued?: boolean;
}

/** A box event (`GET /events`). */
export interface BoxEvent { seq: number; at: number; kind: string; title: string; body: string }

export type InboxKind = "code" | "routine" | "pairing";

export interface InboxItem {
  /** Stable across polls. */
  key: string;
  kind: InboxKind;
  /** Where it's from: "Code · ~/src/app", "Morning health check". */
  where: string;
  /** What it wants, in a line. */
  summary: string;
  /** More, when there is: the command, the folder, why. */
  detail?: string;
  /** Unix seconds it started waiting. */
  since: number;
  /** Allow/Deny (and maybe "Always"), or only Open. */
  answer: "decide" | "open";
  canAlways: boolean;
  /** Where Open goes: a place, and the item there. */
  open: { route: "code" | "routines" | "settings"; hash?: string };
  /** For a pairing code: when it stops working. */
  expires?: number;
  /** What answering needs. */
  ref: { approval: string } | { pairing: number };
}

/** Everything waiting for this person, pairing codes first (they expire), then oldest first. */
export function gather({ approvals, events, owner, now, ignored = [], seen = new Map() }: {
  approvals: BoxApproval[] | null | undefined;
  events: BoxEvent[] | null | undefined;
  /** True only once the box has said this device is an owner. */
  owner: boolean;
  /** Unix seconds. */
  now: number;
  /** Keys the person set aside (a pairing code they don't recognize). */
  ignored?: string[];
  /** When each key was first seen, for what the box doesn't date (a Code question); filled in here. */
  seen?: Map<string, number>;
}): InboxItem[] {
  const firstSeen = (key: string) => {
    if (!seen.has(key)) seen.set(key, now);
    return seen.get(key)!;
  };
  const out: InboxItem[] = [];
  for (const a of approvals ?? []) {
    const code = a.kind === "code";
    const session = code ? a.session ?? a.id.split(":")[1] : undefined;
    const run = !code ? a.run ?? a.id.split(":")[1] : undefined;
    out.push({
      key: a.id,
      kind: code ? "code" : "routine",
      where: code ? `Code${a.where ? ` · ${a.where}` : ""}` : a.where ?? "A routine",
      summary: a.summary,
      detail: a.queued ? "It read the web first, so this waits for you." : undefined,
      since: a.since ?? firstSeen(a.id),
      answer: "decide",
      canAlways: !!a.can_always,
      open: code ? { route: "code", hash: session } : { route: "routines", hash: run ? `run-${run}` : undefined },
      ref: { approval: a.id },
    });
  }
  if (owner) {
    for (const e of events ?? []) {
      if (e.kind !== "pairing" || now - e.at >= PAIR_TTL_S) continue;
      out.push({
        key: `pairing:${e.seq}`,
        kind: "pairing",
        where: "Pairing request",
        summary: e.title,
        detail: e.body,
        since: e.at,
        answer: "open",
        canAlways: false,
        open: { route: "settings", hash: "devices" },
        expires: e.at + PAIR_TTL_S,
        ref: { pairing: e.seq },
      });
    }
  }
  return out
    .filter((i) => !ignored.includes(i.key))
    .sort((a, b) => (a.kind === "pairing" ? 0 : 1) - (b.kind === "pairing" ? 0 : 1) || a.since - b.since);
}

/** The notifications dropdown's order: newest first. */
export function newestFirst(items: InboxItem[]): InboxItem[] {
  return [...items].sort((a, b) => b.since - a.since);
}

/** The bell's count: nothing at zero, and "9+" past nine. */
export function badgeText(n: number): string | null {
  return n <= 0 ? null : n > 9 ? "9+" : String(n);
}

/** The bell's accessible name. */
export function bellLabel(n: number): string {
  return n > 0 ? `Notifications, ${n} waiting` : "Notifications, nothing waiting";
}

export type Decision = "allow" | "always" | "deny";

/** Answers an item through the endpoint its feature already has. */
export async function answer(item: InboxItem, decision: Decision): Promise<void> {
  const r = item.ref;
  if ("approval" in r) {
    await post("/approvals/answer", { id: r.approval, allow: decision !== "deny", always: decision === "always" });
  }
}

/** The sources, fetched as this device may: pairing only for owners. */
export const sources = {
  approvals: () => get<{ approvals: BoxApproval[] }>("/approvals").then((r) => r.approvals),
  events: () => get<{ events: BoxEvent[] }>("/events?since=0").then((r) => r.events).catch(() => null),
};
