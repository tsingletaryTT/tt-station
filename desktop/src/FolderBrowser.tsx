// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Picking a project folder on the box (in its user's home folder), or making a
// new one as a git repository: `/coding/folders`. Code and Library use it.

import { useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Badge } from "@tenstorrent/vesper/badge";
import { TextInput } from "@tenstorrent/vesper/text-input";
import { get, post } from "./api";
import { toastError, toastOk, usePoll } from "./ui";

export function FolderBrowser({ home, onPick }: { home: string; onPick: (path: string) => void }) {
  const [path, setPath] = useState(home);
  const list = usePoll(() => get<{ path: string; parent: string | null; git: boolean; folders: { name: string; path: string; git: boolean }[] }>(`/coding/folders?path=${encodeURIComponent(path)}`), 60000, [path]);
  const [name, setName] = useState("");
  const l = list.data;
  const create = async () => {
    try { const r = await post<{ path: string }>("/coding/folders", { parent: path, name: name.trim() }); toastOk(`Made ${name.trim()} (a new git repository).`); onPick(r.path); }
    catch (e) { toastError(e); }
  };
  return (
    <div className="stack" style={{ gap: 10 }}>
      <div className="row" style={{ gap: 6 }}>
        <Button size="xs" variant="ghost" disabled={!l?.parent} onClick={() => l?.parent && setPath(l.parent)}>↑ Up</Button>
        <span className="mono muted grow" style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis" }}>{l?.path ?? path}</span>
        {l && l.path !== home && <Button size="xs" onClick={() => onPick(l.path)}>Use this folder{l.git ? "" : ""}</Button>}
      </div>
      <div className="list" style={{ maxHeight: 360, overflowY: "auto" }}>
        {(l?.folders ?? []).map((f) => (
          <div key={f.path} className="list-row">
            <button type="button" className="folder-open" onClick={() => setPath(f.path)} aria-label={`Open ${f.name}${f.git ? " (git repository)" : ""}`}>
              <span aria-hidden>📁</span><span className="grow">{f.name}</span>{f.git && <Badge size="sm" variant="mint" subtle>git</Badge>}
            </button>
            <Button size="xs" variant="ghost" aria-label={`Use ${f.name}`} onClick={() => onPick(f.path)}>Use</Button>
          </div>
        ))}
        {l && l.folders.length === 0 && <span className="muted" style={{ fontSize: 13 }}>No folders here.</span>}
      </div>
      <div className="row" style={{ gap: 6 }}>
        <div className="grow"><TextInput size="sm" value={name} onChange={(e) => setName(e.target.value)} placeholder="New project name" /></div>
        <Button size="sm" variant="subtle" disabled={!name.trim()} onClick={create}>New project here</Button>
      </div>
    </div>
  );
}
