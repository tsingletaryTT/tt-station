// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Settings → General → This app: what boxes may open on this computer without
// asking. When Tori on a box wants to open a link or an app here, the desktop
// app asks first (electron/clientactions.cjs); "Always allow" choices are
// listed here, per box, to take back one at a time or all at once.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";

export type AllowedOnThisComputer = { boxId: string; name: string; sites: string[]; apps: string[]; caps?: string[] };

/** How a remembered action reads in the list. */
const capLabel = (key: string) =>
  key === "record" ? "Recording with the microphone" : key === "find_files" ? "Searching your files" : key.startsWith("control:") ? `Changing a setting (${key.slice(8).replace(/_/g, " ")})` : key;

const shown = (site: string) => site.replace(/^https?:\/\//, "").replace(/^mailto:/, "email to ");

export function AllowedActionsSetting() {
  const [list, setList] = useState<AllowedOnThisComputer[] | null>(null);
  useEffect(() => { window.ttDesktop?.allowedActions?.().then(setList).catch(() => setList(null)); }, []);
  const forget = window.ttDesktop?.forgetAllowedActions;
  if (!list || !forget) return null; // an older desktop shell, or the browser
  return (
    <div className="stack" style={{ gap: 4 }}>
      <div className="row-between wrap" style={{ gap: 12 }}>
        <span>What boxes may do on this computer without asking</span>
        {list.length > 0 && <Button size="sm" variant="ghost" onClick={async () => setList(await forget())}>Clear all</Button>}
      </div>
      {list.length === 0 ? (
        <span className="muted" style={{ fontSize: 12 }}>
          Nothing: TT Station asks you each time Tori on a box wants to open a link or an app, record, search your files or change a setting on this computer.
        </span>
      ) : (
        <div className="list">
          {list.flatMap((box) => [
            ...box.sites.map((key) => ({ box, kind: "site" as const, key, label: shown(key) })),
            ...box.apps.map((key) => ({ box, kind: "app" as const, key, label: `${key} (app)` })),
            ...(box.caps ?? []).map((key) => ({ box, kind: "cap" as const, key, label: capLabel(key) })),
          ]).map(({ box, kind, key, label }) => (
            <div key={`${box.boxId}:${kind}:${key}`} className="list-row row-between" style={{ gap: 12 }}>
              <span>{label} <span className="muted">from {box.name}</span></span>
              <Button size="sm" variant="subtle" onClick={async () => setList(await forget(box.boxId, { kind, key }))}>Remove</Button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
