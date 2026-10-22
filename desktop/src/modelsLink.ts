// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Opening Models on one model: Home's mode cards and first-run steps say what
// to show; Models takes it once.

import { navigate } from "./App";

const OPEN_KEY = "tt.models.open";

/** Show Models searched to `query`, on one of its tabs ("all", "chat", …). */
export function openModels(query: string, filter = "all") {
  try { localStorage.setItem(OPEN_KEY, JSON.stringify({ filter, query })); } catch { /* private */ }
  navigate("models");
}

/** What `openModels` asked Models to show, once. */
export function takeOpenRequest(): { filter: string; query: string } | null {
  try { const v = localStorage.getItem(OPEN_KEY); localStorage.removeItem(OPEN_KEY); return v ? JSON.parse(v) : null; } catch { return null; }
}
