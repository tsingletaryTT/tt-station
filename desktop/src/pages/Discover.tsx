// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// What's new: the tt-awesome feed (releases, new projects, articles from the
// Tenstorrent ecosystem), fetched by the box and shown as plain text. Updates
// for tools installed on this box come first.

import { Fragment, useMemo, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Icon } from "@tenstorrent/vesper/icons";
import { Typography } from "@tenstorrent/vesper/typography";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { ago, get } from "../api";
import { Card, copyText, Empty, Eyebrow, Page, usePoll } from "../ui";

export interface FeedItem {
  id: string; url: string; kind: "release" | "entry" | "article"; title: string; summary: string; date: string | null;
  official: boolean; topics: string[]; project: string | null; version: string | null;
  installed: string | null; update: boolean | null;
}
export interface Feed { items: FeedItem[]; updated: number | null; stale: boolean; available: boolean; source: string }

type Kind = "release" | "entry" | "article";
type Who = "all" | "official" | "community";

const KINDS: [Kind, string][] = [["release", "Releases"], ["entry", "New projects"], ["article", "Articles"]];

export function openLink(url: string) {
  if (window.ttDesktop) window.ttDesktop.openExternal(url);
  else window.open(url, "_blank", "noopener");
}

const topicName = (t: string) => t.replace(/-/g, " ").replace(/^ai /, "AI ").replace(/\b(hw)\b/, "hardware").replace(/riscv/, "RISC-V");

/** The feed's light Markdown, as text: `code` spans become <code>, ** markers go. */
function Summary({ text }: { text: string }) {
  const parts = text.replace(/\*\*/g, "").split("`");
  return <>{parts.map((p, i) => (i % 2 ? <code key={i}>{p}</code> : <Fragment key={i}>{p}</Fragment>))}</>;
}

function when(date: string | null) {
  if (!date) return "";
  const t = Date.parse(date);
  return Number.isNaN(t) ? "" : ago(Math.floor(t / 1000));
}

function ItemCard({ it }: { it: FeedItem }) {
  return (
    <Card>
      <div className="row-between" style={{ alignItems: "flex-start" }}>
        <div className="grow stack" style={{ gap: 2 }}>
          <Typography variant="heading-xs">{it.title}</Typography>
          <span className="muted" style={{ fontSize: 12 }}>
            {when(it.date)}{it.project ? ` · ${it.project}` : ""}
          </span>
        </div>
        <div className="row" style={{ gap: 6 }}>
          {it.update === true && <Badge size="sm" variant="warning" subtle>You have {it.installed}</Badge>}
          {it.update === false && <Badge size="sm" variant="mint" subtle>Installed</Badge>}
          <Badge size="sm" variant={it.official ? "info" : "contrast"} subtle>{it.official ? "Official" : "Community"}</Badge>
        </div>
      </div>
      {it.summary && <span style={{ fontSize: 14, lineHeight: 1.5 }}><Summary text={it.summary} /></span>}
      <div className="row-between wrap">
        <div className="row wrap" style={{ gap: 6 }}>
          {it.topics.slice(0, 4).map((t) => <span key={t} className="muted" style={{ fontSize: 12 }}>#{topicName(t)}</span>)}
        </div>
        <div className="row" style={{ gap: 4 }}>
          <Button size="xs" variant="ghost" onClick={() => void copyText(it.url, "Link copied.")}>Copy link</Button>
          <Button size="xs" variant="subtle" onClick={() => openLink(it.url)}>
            {it.kind === "release" ? "Release notes" : it.kind === "article" ? "Read" : "Open"}
          </Button>
        </div>
      </div>
    </Card>
  );
}

export function Discover() {
  const feed = usePoll(() => get<Feed>("/discover"), 15 * 60 * 1000);
  const [kind, setKind] = useState<Kind>("release");
  const [who, setWho] = useState<Who>("all");
  const [topic, setTopic] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const items = feed.data?.items ?? [];
  const updates = items.filter((i) => i.update === true);
  const current = items.filter((i) => i.update === false);

  const topics = useMemo(() => {
    const count = new Map<string, number>();
    items.filter((i) => i.kind === kind).forEach((i) => i.topics.forEach((t) => count.set(t, (count.get(t) ?? 0) + 1)));
    return [...count.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8).map(([t]) => t);
  }, [items, kind]);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return items
      .filter((i) => i.kind === kind)
      .filter((i) => who === "all" || (who === "official") === i.official)
      .filter((i) => !topic || i.topics.includes(topic))
      .filter((i) => !q || `${i.title} ${i.summary} ${i.project ?? ""}`.toLowerCase().includes(q))
      .sort((a, b) => (b.date ?? "").localeCompare(a.date ?? ""));
  }, [items, kind, who, topic, query]);

  return (
    <Page title="What's new" subtitle="Releases, new projects and articles from the Tenstorrent community, via tt-awesome."
      actions={feed.data?.source ? <Button variant="tertiary" iconLeft={<Icon kind="globe" />} onClick={() => openLink(feed.data!.source)}>tt-awesome</Button> : undefined}>
      {feed.data?.stale && feed.data.available && (
        <Admonition variant="info" subtle size="sm">The box couldn't reach tt-awesome just now, so this is the copy from {feed.data.updated ? ago(feed.data.updated) : "earlier"}.</Admonition>
      )}
      {feed.data && !feed.data.available && <Empty title="Couldn't load the feed">The box needs to reach docs.tenstorrent.com once to show what's new.</Empty>}

      {updates.length > 0 && (
        <>
          <div className="section-title">
            <Eyebrow>Updates for this box</Eyebrow>
            <span className="muted">Newer releases of tools installed here.{current.length > 0 ? ` Up to date: ${current.map((i) => i.project?.split("/").pop()).join(", ")}.` : ""}</span>
          </div>
          <div className="grid grid-2">{updates.map((it) => <ItemCard key={it.id} it={it} />)}</div>
        </>
      )}

      <div className="row-between wrap" style={{ marginTop: 8 }}>
        <div className="pill-tabs">
          {KINDS.map(([k, label]) => (
            <button key={k} className={kind === k ? "on" : ""} onClick={() => { setKind(k); setTopic(null); }}>
              {label} <span className="muted">{items.filter((i) => i.kind === k).length || ""}</span>
            </button>
          ))}
        </div>
        <div className="row wrap" style={{ gap: 8 }}>
          <div className="pill-tabs">
            {(["all", "official", "community"] as Who[]).map((w) => (
              <button key={w} className={who === w ? "on" : ""} onClick={() => setWho(w)}>{w[0].toUpperCase() + w.slice(1)}</button>
            ))}
          </div>
          <div style={{ width: 240 }}><TextInput size="sm" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search" iconLeft={<Icon kind="search" />} /></div>
        </div>
      </div>
      {topics.length > 1 && (
        <div className="row wrap" style={{ gap: 6 }}>
          {topics.map((t) => (
            <Button key={t} size="xs" variant={topic === t ? "primary" : "ghost"} onClick={() => setTopic(topic === t ? null : t)}>{topicName(t)}</Button>
          ))}
        </div>
      )}

      {!feed.data && !feed.error && <span className="muted">Loading what's new…</span>}
      {feed.error && <Empty title="Couldn't load the feed">{feed.error}</Empty>}
      <div className="stack">{shown.map((it) => <ItemCard key={it.id} it={it} />)}</div>
      {feed.data?.available && shown.length === 0 && <span className="muted">Nothing matches.</span>}
    </Page>
  );
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
