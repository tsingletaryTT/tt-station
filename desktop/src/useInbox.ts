// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The "Needs you" list, live: one shared poll of each source for the sidebar's
// count and the page, quick while the window is shown and resting while it's
// hidden (`poll.ts`). See `inbox.ts` for what's gathered.

import { useCallback, useEffect, useMemo, useState } from "react";
import { get, type PairedDevice } from "./api";
import { gather, sources, type InboxItem } from "./inbox";
import { useSharedPoll } from "./ui";

const EVERY_MS = 4000;
/** When each item was first seen, kept for the session so the order holds still. */
const seen = new Map<string, number>();
const IGNORED_KEY = "tt.inbox.ignored";
/** Read by the shared sources (made once), so they follow the latest answer. */
let ownerNow = false;
const ownerOnly = <T,>(fn: () => Promise<T>) => () => (ownerNow ? fn() : Promise.resolve(null));

function loadIgnored(): string[] {
  try { return JSON.parse(sessionStorage.getItem(IGNORED_KEY) ?? "[]") as string[]; } catch { return []; }
}

export function useInbox(): { items: InboxItem[]; owner: boolean; loaded: boolean; error: string | null; refresh: () => void; ignore: (key: string) => void } {
  const devices = useSharedPoll("/devices", () => get<{ owner?: boolean; devices: PairedDevice[] }>("/devices"), 60000);
  // An owner only once the box has said so (an agent from before roles says nothing: everyone is one).
  const owner = !!devices.data && devices.data.owner !== false;
  ownerNow = owner;
  const approvals = useSharedPoll("inbox:/approvals", sources.approvals, EVERY_MS);
  // Members never get bring-up's or pairing's: no need to ask (a long interval keeps them idle).
  const bringup = useSharedPoll("inbox:/bringup/codex/runs", ownerOnly(sources.bringup), owner ? EVERY_MS * 2 : 3600_000);
  const events = useSharedPoll("inbox:/events", ownerOnly(sources.events), owner ? EVERY_MS : 3600_000);
  const [ignored, setIgnored] = useState<string[]>(loadIgnored);
  // Pairing codes run out after two minutes even when nothing new arrives.
  const [tick, setTick] = useState(0);
  useEffect(() => { const t = window.setInterval(() => setTick((n) => n + 1), 10000); return () => window.clearInterval(t); }, []);
  // Owner learnt after the first fetch: fetch the owner-only sources now.
  useEffect(() => { if (owner) { void bringup.refresh(); void events.refresh(); } }, [owner]); // eslint-disable-line react-hooks/exhaustive-deps
  const items = useMemo(
    () => gather({ approvals: approvals.data, bringup: bringup.data, events: events.data, owner, now: Math.floor(Date.now() / 1000), ignored, seen }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [approvals.data, bringup.data, events.data, owner, ignored, tick],
  );
  const refresh = useCallback(() => { void approvals.refresh(); if (owner) { void bringup.refresh(); void events.refresh(); } }, [approvals, bringup, events, owner]);
  const ignore = useCallback((key: string) => setIgnored((cur) => {
    const next = [...cur, key].slice(-100);
    try { sessionStorage.setItem(IGNORED_KEY, JSON.stringify(next)); } catch { /* private */ }
    return next;
  }), []);
  return { items, owner, loaded: approvals.data !== null || approvals.error !== null, error: approvals.error, refresh, ignore };
}
