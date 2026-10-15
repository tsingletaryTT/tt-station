// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// While the box records a meeting, every screen of the app on the box says so:
// "Recording in this room · started by <who> · Stop". Any paired phone can
// start a recording, so the people in the room see it here (and hear the
// box's chime) whoever started it. Fed by `GET /record`, the same poll the
// recorder card uses. On another computer the box's room isn't this room, so
// it stays away.

import { useEffect, useState } from "react";
import { Button } from "@tenstorrent/vesper/button";
import { get, onAnotherComputer } from "./api";
import { sharedSource } from "./poll";
import { openTranscribe, stopRecording, type RecordState } from "./recorder";
import { recordingBannerText } from "./recordingBanner";
import { toastError, toastOk } from "./ui";

export function RecordingBanner() {
  const [onBox, setOnBox] = useState(false);
  const [state, setState] = useState<RecordState | null>(null);
  const [busy, setBusy] = useState(false);
  const [, tick] = useState(0);
  useEffect(() => {
    let live = true;
    onAnotherComputer().then((other) => { if (live) setOnBox(!other); }).catch(() => { if (live) setOnBox(true); });
    return () => { live = false; };
  }, []);
  useEffect(() => {
    if (!onBox) return;
    const src = sharedSource("/record", () => get<RecordState>("/record"));
    if (src.data) setState(src.data);
    return src.add(2000, () => setState(src.data));
  }, [onBox]);
  const rec = onBox ? state?.recording ?? null : null;
  const recording = !!rec;
  // The clock counts every second between polls.
  useEffect(() => {
    if (!recording) return;
    const t = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => window.clearInterval(t);
  }, [recording]);
  if (!rec) return null;
  const { lead, meta } = recordingBannerText(rec, Date.now() / 1000);
  const stop = async () => {
    setBusy(true);
    try {
      toastOk(await stopRecording());
      await sharedSource("/record", () => get<RecordState>("/record")).fetch();
    } catch (e) { toastError(e); } finally { setBusy(false); }
  };
  return (
    <div className="recording-banner" role="status" aria-live="polite">
      <div className="offline-row">
        <span className="rec-dot" aria-hidden />
        <span className="grow">
          <b>{lead}</b>
          {meta.map((m) => <span key={m} className="offline-meta"> · {m}</span>)}
        </span>
        <button type="button" className="link-button" onClick={openTranscribe}>Open</button>
        <Button size="xs" variant="subtle" disabled={busy} onClick={stop}>Stop</Button>
      </div>
    </div>
  );
}
