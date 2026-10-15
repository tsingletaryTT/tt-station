// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// What's new, as a small card on Home. Kept apart from the Discover page so
// Home doesn't load the whole page to show it.

import { Badge } from "@tenstorrent/vesper/badge";
import { ago, get } from "../api";
import { Card, Eyebrow, usePoll } from "../ui";

export interface FeedItem {
  id: string; url: string; kind: "release" | "entry" | "article"; title: string; summary: string; date: string | null;
  official: boolean; topics: string[]; project: string | null; version: string | null;
  installed: string | null; update: boolean | null;
}
export interface Feed { items: FeedItem[]; updated: number | null; stale: boolean; available: boolean; source: string }

export function when(date: string | null) {
  if (!date) return "";
  const t = Date.parse(date);
  return Number.isNaN(t) ? "" : ago(Math.floor(t / 1000));
}

/** A small Home card: updates for this box, and the latest releases. */
export function WhatsNewCard({ onOpen }: { onOpen: () => void }) {
  const feed = usePoll(() => get<Feed>("/discover"), 30 * 60 * 1000);
  const items = feed.data?.items ?? [];
  if (!items.length) return null;
  const updates = items.filter((i) => i.update === true);
  const latest = items.filter((i) => i.kind === "release").sort((a, b) => (b.date ?? "").localeCompare(a.date ?? "")).slice(0, 3);
  return (
    <Card onClick={onOpen}>
      <div className="row-between">
        <Eyebrow>What's new</Eyebrow>
        {updates.length > 0 && <Badge size="sm" variant="warning" subtle>{updates.length} update{updates.length > 1 ? "s" : ""} for this box</Badge>}
      </div>
      <div className="list">
        {latest.map((it) => (
          <div key={it.id} className="list-row">
            <span className="grow" style={{ fontSize: 13 }}>{it.title}</span>
            <span className="muted" style={{ fontSize: 12 }}>{when(it.date)}</span>
          </div>
        ))}
      </div>
    </Card>
  );
}
