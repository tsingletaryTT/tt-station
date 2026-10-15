// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Links in text a model wrote (chat replies, Tori, reports) open in the
// system browser, never in the app's own window: a page loaded there would
// get the desktop bridge (and the box's token) with it.
import type { Components } from "react-markdown";

/** Opens a web link outside the app; anything else (javascript:, file:…) is ignored. */
export function openLink(href: string | undefined) {
  if (!href || !/^https?:\/\//i.test(href)) return;
  if (window.ttDesktop) void window.ttDesktop.openExternal(href);
  else window.open(href, "_blank", "noopener,noreferrer");
}

/** `components` for react-markdown: every link opens outside the app. */
export const externalLinks: Components = {
  a: ({ href, children }) => <a href={href} rel="noreferrer" onClick={(e) => { e.preventDefault(); openLink(href); }}>{children}</a>,
};
