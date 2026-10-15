// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Store apps listen only on the box (127.0.0.1 there) unless their owner opens
// them to the network. The desktop app reaches a box-only app from another
// computer through its SSH tunnel; a plain browser on another computer can't,
// and would show a broken page -- so it says why instead.

const LOOPBACK = new Set(["127.0.0.1", "localhost", "[::1]", "::1"]);

/** Why `url` can't be opened from here, or null when it can. `here` is the
 *  page's own host name; `desktopApp`: the desktop app, which tunnels. */
export function boxOnlyReason(url: string, here: string, desktopApp: boolean, canShare: boolean): string | null {
  if (desktopApp || LOOPBACK.has(here)) return null;
  let host: string;
  try { host = new URL(url).hostname; } catch { return null; }
  if (!LOOPBACK.has(host)) return null;
  return canShare
    ? "This app is only available on the box. Turn on Open to the network in its settings, or open it from the TT Station app."
    : "This app is only available on the box. Open it from TT Station on the box, or from the TT Station app on this computer (it connects to the box securely).";
}
