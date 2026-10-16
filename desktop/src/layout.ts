// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The window never scrolls: the shell is exactly the window's height, the
// sidebar's list of places scrolls on its own, and the page area under the top
// bar scrolls for ordinary pages. A page that lays out its own panes (Chat,
// Code, a Research report, a terminal) asks for the full height instead: the
// page area then doesn't scroll and the page's panes do.

import { createContext, useContext, useLayoutEffect } from "react";

/** Set by the shell: whether the current page fills the page area. */
export const FullHeightContext = createContext<(on: boolean) => void>(() => {});

/** The current page lays out its own scrolling panes (while `on`). */
export function useFullHeight(on = true) {
  const set = useContext(FullHeightContext);
  useLayoutEffect(() => {
    set(on);
    return () => set(false);
  }, [on, set]);
}
