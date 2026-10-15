// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Library: folders on the box whose files the assistant can search
// (`library.rs`). Add a folder and its text, Markdown, code, PDFs and Word
// files are read and kept current; Chat, Tori, Routines and Research search
// them. With the CPU embedding service running, search also matches meaning,
// not just words.

import { useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { Admonition } from "@tenstorrent/vesper/admonition";
import { Icon } from "@tenstorrent/vesper/icons";
import { IconButton } from "@tenstorrent/vesper/icon-button";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { Typography } from "@tenstorrent/vesper/typography";
import { Sheet, useSheet } from "@tenstorrent/vesper/sheet";
import { ago, del, get, post } from "../api";
import { Card, Empty, Eyebrow, MoreMenu, Page, toastError, toastOk, usePoll } from "../ui";
import { ask } from "../dialog";
import { FolderBrowser } from "./Terminals";
import { navigate } from "../App";

interface Source { id: string; path: string; added: number; last_indexed: number | null; files: number; skipped: number; activity?: string; error?: string }
interface Lib {
  home: string;
  sources: Source[];
  docs: { per_source?: Record<string, [number, number]>; uploaded?: number };
  meaning: { available: boolean; passages_done: number; passages: number };
}
interface Doc { id: string; name: string; added: number; chars: number; passages: number; source?: string }
interface Hit { document: string; passage: number; score: number; text: string; path?: string }

const tilde = (p: string, home: string) => (home && p.startsWith(home) ? "~" + p.slice(home.length) : p);

export function Library() {
  const lib = usePoll(() => get<Lib>("/library"), 3000);
  const docs = usePoll(() => get<{ documents: Doc[] }>("/docs"), 15000);
  const browse = useSheet();
  const d = lib.data;
  const uploaded = (docs.data?.documents ?? []).filter((x) => !x.source);

  const add = async (path: string) => {
    browse.close();
    try { await post("/library/sources", { path }); toastOk(`Reading ${path.split("/").pop()}…`); void lib.refresh(); }
    catch (e) { toastError(e); }
  };
  const remove = async (s: Source) => {
    if (!(await ask(`Take ${s.path.split("/").pop()} out of the Library?`, { body: "The assistant stops searching it. Your files stay where they are.", confirm: "Remove", danger: true }))) return;
    try { await del(`/library/sources/${s.id}`); void lib.refresh(); } catch (e) { toastError(e); }
  };
  const reindex = async (s: Source) => {
    try { await post(`/library/sources/${s.id}/reindex`); void lib.refresh(); } catch (e) { toastError(e); }
  };
  const removeDoc = async (x: Doc) => {
    if (!(await ask(`Delete “${x.name}”?`, { body: "It's removed from the box.", confirm: "Delete", danger: true }))) return;
    try { await del(`/docs/${x.id}`); void docs.refresh(); void lib.refresh(); } catch (e) { toastError(e); }
  };

  const m = d?.meaning;
  const pct = m && m.passages ? Math.round((100 * m.passages_done) / m.passages) : 100;

  return (
    <Page title="Library"
      subtitle="Your folders, searchable by the assistant. Ask Chat or Tori about your notes, papers or code and it looks here first. Files never leave the box."
      actions={<Button iconLeft={<Icon kind="add" />} onClick={browse.open} disabled={!d}>Add a folder</Button>}>
      {lib.error && <Admonition variant="warning" subtle size="sm">Couldn't load the Library: {lib.error}</Admonition>}

      {m && (
        <div className="lib-meaning">
          {m.available ? (
            <>
              <Badge size="sm" variant="mint" subtle>Meaning search on</Badge>
              <span className="muted">
                {m.passages === 0 ? "Finds passages by meaning as well as words."
                  : pct >= 100 ? `Finds passages by meaning as well as words: all ${m.passages.toLocaleString()} passages ready.`
                  : `Working through passages: ${m.passages_done.toLocaleString()} of ${m.passages.toLocaleString()} (${pct}%). Until then, those match by words.`}
              </span>
            </>
          ) : (
            <>
              <Badge size="sm" variant="contrast" subtle>Words only</Badge>
              <span className="muted">Start the embedding service to also find passages by meaning (“pets” finds “animals”).</span>
              <Button size="xs" variant="subtle" onClick={() => navigate("models")}>CPU services</Button>
            </>
          )}
        </div>
      )}

      {d && d.sources.length === 0 && (
        <Empty title="No folders yet" action={<Button onClick={browse.open}>Add a folder</Button>}>
          Add Documents, a notes folder or a project. Text, Markdown, code, PDF, Word and HTML files are read; hidden folders, node_modules and build output are skipped.
        </Empty>
      )}

      {d && d.sources.length > 0 && (
        <div className="stack" style={{ gap: 8 }}>
          <Eyebrow>Folders</Eyebrow>
          {d.sources.map((s) => {
            const [docsN, passages] = d.docs.per_source?.[s.id] ?? [0, 0];
            return (
              <Card key={s.id} className="lib-source">
                <Icon kind="folder" className="lib-source-icon" />
                <div className="lib-source-main">
                  <span className="lib-source-name">{s.path.split("/").pop()}</span>
                  <span className="muted mono lib-source-path" title={s.path}>{tilde(s.path, d.home)}</span>
                </div>
                <div className="lib-source-meta">
                  {s.error ? <Badge size="sm" variant="danger" subtle title={s.error}>{s.error}</Badge>
                    : s.activity ? <span className="lib-busy"><Icon kind="spinner" className="lib-spin" /> {s.activity}</span>
                    : <span className="muted">
                        {docsN.toLocaleString()} {docsN === 1 ? "file" : "files"} · {passages.toLocaleString()} passages
                        {s.skipped > 0 && <span title="Too big, empty, or over the per-folder limit"> · {s.skipped} skipped</span>}
                        {s.last_indexed && <> · checked {ago(s.last_indexed)}</>}
                      </span>}
                </div>
                <MoreMenu items={[
                  { label: "Check for changes now", onSelect: () => reindex(s) },
                  { label: "Remove from Library", onSelect: () => remove(s), danger: true },
                ]} />
              </Card>
            );
          })}
          <span className="muted" style={{ fontSize: 12 }}>Folders are checked for new, changed and deleted files every 15 minutes.</span>
        </div>
      )}

      {(d?.sources.length ?? 0) + uploaded.length > 0 && <TrySearch />}

      {uploaded.length > 0 && (
        <div className="stack" style={{ gap: 8 }}>
          <Eyebrow>Saved documents</Eyebrow>
          <Card className="lib-docs">
            {uploaded.map((x) => (
              <div key={x.id} className="lib-doc">
                <Icon kind="document" />
                <span className="lib-doc-name">{x.name}</span>
                <span className="muted">{x.passages} {x.passages === 1 ? "passage" : "passages"} · {ago(x.added)}</span>
                <IconButton icon={<Icon kind="trash" />} variant="ghost" size="sm" aria-label={`Delete ${x.name}`} onClick={() => removeDoc(x)} />
              </div>
            ))}
          </Card>
        </div>
      )}

      <Sheet ref={browse.ref} popover title="Add a folder" description="A folder in your home folder. Everything inside it that can be read as text is searched.">
        {d && <FolderBrowser home={d.home} onPick={add} />}
      </Sheet>
    </Page>
  );
}

function TrySearch() {
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<Hit[] | null>(null);
  const [busy, setBusy] = useState(false);
  const run = async () => {
    if (!q.trim()) return;
    setBusy(true);
    try { setHits((await post<{ hits: Hit[] }>("/library/search", { query: q.trim(), top_k: 6 })).hits.filter((h, i) => i === 0 || h.score >= 0.3)); }
    catch (e) { toastError(e); }
    finally { setBusy(false); }
  };
  return (
    <div className="stack" style={{ gap: 8 }}>
      <Eyebrow>Try it</Eyebrow>
      <form className="row" style={{ gap: 8 }} onSubmit={(e) => { e.preventDefault(); void run(); }}>
        <TextInput className="grow" value={q} onChange={(e) => setQ(e.target.value)} placeholder="What the assistant would find for…" aria-label="Search the Library" />
        <Button type="submit" variant="subtle" disabled={busy || !q.trim()}>{busy ? "Searching…" : "Search"}</Button>
      </form>
      {hits && hits.length === 0 && <span className="muted">Nothing matched.</span>}
      {hits && hits.length > 0 && (
        <div className="stack" style={{ gap: 6 }}>
          {hits.map((h, i) => (
            <Card key={i} className="lib-hit">
              <Typography variant="copy-sm-bold" className="lib-hit-name" title={h.path}>{h.document}</Typography>
              <span className="lib-hit-text">{h.text.length > 400 ? h.text.slice(0, 400).trimEnd() + "…" : h.text}</span>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
