// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Favorite models: a star on every model card (official, community, your
// bring-ups, the community catalog). The list lives on the box (`/favorites`),
// so every app shows the same one; this module keeps one copy for the whole
// window, so a star on one card updates the others at once.

import { useEffect, useState } from "react";
import { get, put } from "./api";
import { toastError } from "./ui";

export type FavoriteKind = "official" | "community" | "code" | "catalog";
export interface Favorite { key: string; name: string; kind: FavoriteKind; added: number }

let list: Favorite[] = [];
let loaded = false;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());

async function load() {
  try { list = (await get<{ favorites: Favorite[] }>("/favorites")).favorites; loaded = true; emit(); } catch { /* an older box: no favorites */ }
}

export const favKey = (kind: FavoriteKind, id: string) => `${kind}:${id}`;

export async function setFavorite(kind: FavoriteKind, id: string, name: string, on: boolean) {
  const key = favKey(kind, id);
  const before = list;
  list = on ? [{ key, name, kind, added: Date.now() / 1000 }, ...list.filter((f) => f.key !== key)] : list.filter((f) => f.key !== key);
  emit();
  try { list = (await put<{ favorites: Favorite[] }>("/favorites", { key, name, kind, on })).favorites; emit(); }
  catch (e) { list = before; emit(); toastError(e); }
}

/** The favorites, kept current across the window. */
export function useFavorites() {
  const [, tick] = useState(0);
  useEffect(() => {
    const l = () => tick((n) => n + 1);
    listeners.add(l);
    if (!loaded) void load();
    return () => { listeners.delete(l); };
  }, []);
  const keys = new Set(list.map((f) => f.key));
  return { list, has: (kind: FavoriteKind, id: string) => keys.has(favKey(kind, id)), loaded };
}

/** The star in a model card's corner. */
export function FavoriteStar({ kind, id, name }: { kind: FavoriteKind; id: string; name: string }) {
  const { has } = useFavorites();
  const on = has(kind, id);
  return (
    <button className={`fav-star ${on ? "on" : ""}`} aria-pressed={on} title={on ? "Remove from favorites" : "Add to favorites"}
      aria-label={on ? `Remove ${name} from favorites` : `Add ${name} to favorites`}
      onClick={(e) => { e.stopPropagation(); void setFavorite(kind, id, name, !on); }}>
      <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
        <path d="M12 3.2l2.7 5.6 6.1.8-4.5 4.2 1.1 6-5.4-2.9-5.4 2.9 1.1-6L3.2 9.6l6.1-.8z"
          fill={on ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.6" strokeLinejoin="round" />
      </svg>
    </button>
  );
}
