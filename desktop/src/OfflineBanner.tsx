// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The one place the app says it can't reach the box: a banner over every
// screen while calls aren't getting through, what to check, and a way to try
// again now. It goes away by itself when the box answers.

import { useEffect, useRef, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { Icon } from "@tenstorrent/vesper/icons";
import { boxName, duration } from "./api";
import { connectivity, useConnectivity } from "./connectivity";
import { toastOk } from "./ui";

export function OfflineBanner() {
  const c = useConnectivity();
  const [open, setOpen] = useState(false);
  const [, tick] = useState(0);
  const wasOffline = useRef(false);
  useEffect(() => {
    if (!c.online) { wasOffline.current = true; return; }
    if (wasOffline.current) { wasOffline.current = false; setOpen(false); toastOk(`${boxName} is back.`); }
  }, [c.online]);
  // The "for N s" counts up while it's showing.
  useEffect(() => {
    if (c.online) return;
    const t = window.setInterval(() => tick((n) => n + 1), 5000);
    return () => window.clearInterval(t);
  }, [c.online]);
  if (c.online) return null;
  const away = c.failingSince ? Math.max(1, Math.round((Date.now() - c.failingSince) / 1000)) : null;
  const remote = !!window.ttDesktop?.boxes;
  return (
    <div className="offline-banner" role="status" aria-live="polite">
      <div className="offline-row">
        <Icon kind="error" className="offline-icon" aria-hidden />
        <span className="grow">
          <b>Can't reach {boxName}</b>
          <span className="offline-meta"> · retrying{away && away >= 60 ? ` · for ${duration(away)}` : ""}</span>
        </span>
        <button type="button" className="link-button" aria-expanded={open} onClick={() => setOpen((o) => !o)}>What to check</button>
        <Button size="xs" variant="subtle" onClick={() => connectivity.retryNow()}>Try now</Button>
      </div>
      {open && (
        <div className="offline-help">
          <ul>
            <li>{boxName} is switched on, and has had a minute or two to start.</li>
            {remote
              ? <li>This computer and {boxName} are on the same network (or the same VPN).</li>
              : <li>If it just restarted or updated, TT Station reconnects by itself once it's ready.</li>}
            <li>Your Wi-Fi or network cable is connected.</li>
          </ul>
          <span>Nothing you were doing is lost: screens fill in again as soon as {boxName} answers.</span>
          {c.lastError && (
            <details>
              <summary>Details for support</summary>
              <code>{c.lastError}{c.failingSince ? ` · since ${new Date(c.failingSince).toLocaleTimeString()}` : ""}</code>
            </details>
          )}
        </div>
      )}
    </div>
  );
}
