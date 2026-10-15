// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// What the recording banner says (see RecordingBanner.tsx), kept apart from
// the component so it can be tested on its own.

export interface BannerRecording { started: number; seconds: number; ended: boolean; started_by?: string | null }

/** "Recording in this room · started by Max's iPhone · 12:04", or why it stopped. */
export function recordingBannerText(rec: BannerRecording, nowSeconds: number): { lead: string; meta: string[] } {
  const by = rec.started_by?.trim();
  const elapsed = Math.max(rec.seconds, Math.floor(nowSeconds - rec.started));
  const meta = [by ? `started by ${by}` : "started from a paired device"];
  if (rec.ended) return { lead: "The recording in this room stopped on its own", meta: [...meta, "save or discard it"] };
  return { lead: "Recording in this room", meta: [...meta, clockText(elapsed)] };
}

function clockText(s: number) {
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return `${h ? `${h}:` : ""}${String(m).padStart(h ? 2 : 1, "0")}:${String(sec).padStart(2, "0")}`;
}
