// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The small, explicit bridge between the app's screens and the desktop:
// a token for the box, and a few desktop actions. Nothing else is exposed.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("ttDesktop", {
  auth: () => ipcRenderer.invoke("auth"),
  openAppWindow: (url, title) => ipcRenderer.invoke("open-app-window", url, title),
  openExternal: (url) => ipcRenderer.invoke("open-external", url),
  startAtLogin: (on) => ipcRenderer.invoke("start-at-login", on),
  notify: (title, body) => ipcRenderer.invoke("notify", title, body),
  onNavigate: (fn) => ipcRenderer.on("navigate", (_e, route) => fn(route)),
  onBoxEvent: (fn) => ipcRenderer.on("box-event", (_e, event) => fn(event)),
  hideTori: () => ipcRenderer.invoke("hide-tori"),
  openToplike: (view) => ipcRenderer.invoke("open-toplike", view),
  showTori: (mode) => ipcRenderer.invoke("show-tori", mode),
  showMain: (route) => ipcRenderer.invoke("show-main", route),
  onTori: (fn) => ipcRenderer.on("tori", (_e, mode) => fn(mode)),
  shortcuts: (on) => ipcRenderer.invoke("shortcuts", on),
  toriShortcut: (binding) => ipcRenderer.invoke("tori-shortcut", binding),
  boxes: () => ipcRenderer.invoke("boxes"),
  addBox: (link) => ipcRenderer.invoke("add-box", link),
  switchBox: (id) => ipcRenderer.invoke("switch-box", id),
  removeBox: (id) => ipcRenderer.invoke("remove-box", id),
  showConnect: () => ipcRenderer.invoke("show-connect"),
  // Where this computer keeps its box tokens (the OS keychain, or a plain owner-only file).
  tokenStorage: () => ipcRenderer.invoke("token-storage"),
  // The box removed this computer (Settings → Devices): forget its token, back to the connect screen.
  signedOut: () => ipcRenderer.invoke("signed-out"),
  // Pairing a box found on the network (the pairing screen).
  discover: () => ipcRenderer.invoke("discover"),
  probe: (host, tls) => ipcRenderer.invoke("probe", host, tls),
  pairStart: (host, tls, fp) => ipcRenderer.invoke("pair-start", host, tls, fp),
  // Sign in with the device's own account (SSH): no password tries the person's keys.
  pairAccount: (device, user, password) => ipcRenderer.invoke("pair-account", device, user, password),
  pairFinish: (p) => ipcRenderer.invoke("pair-finish", p),
  // The box's command line from this computer: "terminal", "vscode" or "command".
  ssh: (what) => ipcRenderer.invoke("ssh", what),
  platform: process.platform,
  // Tori's computer tools, run on this computer.
  computer: (tool, args) => ipcRenderer.invoke("computer", tool, args),
  // Links and apps the person said boxes may always open here, and forgetting them.
  allowedActions: () => ipcRenderer.invoke("allowed-actions"),
  forgetAllowedActions: (boxId, one) => ipcRenderer.invoke("forget-allowed-actions", boxId, one),
  onClientRecord: (fn) => ipcRenderer.on("client-record", (_e, req) => fn(req)),
  clientRecordDone: (nonce, result) => ipcRenderer.send(`client-record-done:${nonce}`, result),
});
