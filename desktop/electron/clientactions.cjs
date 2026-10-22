// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Asking before a box acts on this computer. Tori's computer tools run here,
// in the desktop app (main.cjs `computerTool`): a box could open any web or
// mail link, launch any app,
// record with the microphone, search the person's files or change settings,
// with nobody at the keyboard agreeing. So each such action is put to the
// person first -- exactly what will open, and which box asked -- with "Allow
// once", "Always allow <site/app> from <box>" and "Don't allow". "Always"
// choices are kept per box in the app's prefs (desktop.json, `allowedActions`)
// and cleared in Settings → General → This app.
//
// Pure (no Electron), so the decisions are tested on their own
// (test/clientactions.test.cjs); main.cjs shows the dialog and keeps the prefs.

const PREF_KEY = "allowedActions";
const MAX_URL = 2000;
/** A link the app's own screens open is the person's own click when they touched the window this recently. */
const GESTURE_MS = 3000;

/** What control_computer actions do, in the prompt's words. */
const CONTROL_WORDS = {
  volume_set: "set the volume", volume_up: "turn the volume up", volume_down: "turn the volume down",
  mute: "mute the sound", unmute: "unmute the sound", play_pause: "play or pause music", pause_media: "pause music",
  play_media: "play music", stop_media: "stop music", next_track: "skip to the next track", previous_track: "go back a track",
  lock_screen: "lock the screen", dark_mode: "change dark mode", light_mode: "switch to light mode",
};

// Control characters, and the bidirectional overrides that can make a URL or
// name read differently from what it is.
// eslint-disable-next-line no-control-regex
const DECEPTIVE = /[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2028\u2029\u2066-\u2069\ufeff]/;

/**
 * The action a computer-tool call would take here, or null when the tool
 * doesn't open anything (it needs no prompt). Throws for one that may never run.
 *
 *   { kind: "site", target, key, label }  -- a web or mail link
 *   { kind: "app",  target, key, label }  -- an app, by name
 *   { kind: "cap",  target, key, label, detail } -- recording, searching files, or a setting
 *
 * `target` is exactly what will be opened (a normalized URL, or the app name);
 * `key` is what an "always" choice remembers (a site's origin, a mail
 * address, an app's name); `label` is how the prompt names it.
 */
function describe(tool, args) {
  const a = args && typeof args === "object" ? args : {};
  if (tool === "start_recording") {
    const title = String(a.title ?? "").trim().slice(0, 100);
    return { kind: "cap", target: "microphone", key: "record", label: "record with the microphone",
      detail: `${title && !DECEPTIVE.test(title) ? `Recording: ${title}\n` : ""}It records until it's stopped, then sends the audio to the box to transcribe.` };
  }
  if (tool === "find_files") {
    const name = String(a.name ?? "").trim();
    if (!name) throw new Error("Say what to look for.");
    if (name.length > 200 || DECEPTIVE.test(name)) throw new Error("That isn't something to search for.");
    return { kind: "cap", target: name, key: "find_files", label: "search your files",
      detail: `Search for: ${name}\nThe names of matching files in your home folder are sent to the box.` };
  }
  if (tool === "control_computer") {
    const action = String(a.action ?? "");
    const words = CONTROL_WORDS[action];
    if (!words) return null; // computerTool refuses it anyway
    return { kind: "cap", target: action, key: `control:${action}`, label: words, detail: `Action: ${words}` };
  }
  if (tool !== "open_app") return null;
  if (a.url != null && a.url !== "") {
    const raw = String(a.url).trim();
    if (raw.length > MAX_URL) throw new Error("That link is too long to open from here.");
    if (DECEPTIVE.test(raw)) throw new Error("That link has hidden characters in it, so it won't open from here.");
    let url;
    try { url = new URL(raw); } catch { throw new Error("That isn't a link."); }
    if (url.protocol === "mailto:") {
      const to = decodeURIComponent(url.pathname).trim().toLowerCase();
      if (!to || DECEPTIVE.test(to)) throw new Error("That mail link has no address.");
      return { kind: "site", target: url.href, key: `mailto:${to}`, label: `email to ${to}` };
    }
    if (url.protocol !== "https:" && url.protocol !== "http:") throw new Error("Only web and mail links open from here.");
    if (url.username || url.password) throw new Error("Links with a name or password in them don't open from here.");
    // `url.host` is the punycode form, so a look-alike name shows as what it is.
    return { kind: "site", target: url.href, key: url.origin, label: url.host };
  }
  const name = String(a.app ?? "").trim();
  if (!name) throw new Error("Say which app.");
  if (name.length > 100 || DECEPTIVE.test(name)) throw new Error("That isn't an app name.");
  return { kind: "app", target: name, key: name.toLowerCase().replace(/\.app$/, ""), label: name };
}

/** One box's remembered choices, normalized ({ name, sites, apps }). */
function entry(prefs, boxId) {
  const all = prefs && typeof prefs[PREF_KEY] === "object" && prefs[PREF_KEY] ? prefs[PREF_KEY] : {};
  const e = all[boxId] && typeof all[boxId] === "object" ? all[boxId] : {};
  const list = (v) => (Array.isArray(v) ? v.filter((x) => typeof x === "string") : []);
  return { name: typeof e.name === "string" ? e.name : boxId, sites: list(e.sites), apps: list(e.apps), caps: list(e.caps) };
}

/** Which remembered list an action kind lives in. */
const fieldOf = (kind) => (kind === "app" ? "apps" : kind === "cap" ? "caps" : "sites");

/** Whether the person already said "always" to this action from this box. */
function isAllowed(prefs, boxId, action) {
  if (!action || !boxId) return false;
  return entry(prefs, boxId)[fieldOf(action.kind)].includes(action.key);
}

/** Prefs with "always allow `action` from this box" added (a new object; the input is untouched). */
function remember(prefs, boxId, boxName, action) {
  const e = entry(prefs, boxId);
  const field = fieldOf(action.kind);
  const next = { ...e, name: boxName || e.name, [field]: e[field].includes(action.key) ? e[field] : [...e[field], action.key] };
  return { ...(prefs || {}), [PREF_KEY]: { ...((prefs || {})[PREF_KEY] || {}), [boxId]: next } };
}

/**
 * Prefs with remembered choices cleared: one box's (`boxId`), one choice of it
 * (`boxId` + `{ kind, key }`), or every box's (no `boxId`).
 */
function forget(prefs, boxId, one) {
  const base = { ...(prefs || {}) };
  if (!boxId) return { ...base, [PREF_KEY]: {} };
  const all = { ...(base[PREF_KEY] || {}) };
  if (one) {
    const e = entry(base, boxId);
    const field = fieldOf(one.kind);
    const next = { ...e, [field]: e[field].filter((k) => k !== one.key) };
    if (next.sites.length || next.apps.length || next.caps.length) all[boxId] = next;
    else delete all[boxId];
  } else {
    delete all[boxId];
  }
  return { ...base, [PREF_KEY]: all };
}

/** Every box's remembered choices, for Settings: [{ boxId, name, sites, apps }]. */
function listAllowed(prefs) {
  const all = prefs && typeof prefs[PREF_KEY] === "object" && prefs[PREF_KEY] ? prefs[PREF_KEY] : {};
  return Object.keys(all)
    .map((boxId) => ({ boxId, ...entry(prefs, boxId) }))
    .filter((e) => e.sites.length || e.apps.length || e.caps.length);
}

/** How the prompt names the box: its name, and where it is. */
function boxLabel(box) {
  const name = box?.name || "A box";
  return box?.host && box.host !== name ? `${name} (${box.host})` : name;
}

/** The buttons, in order; `choice` reads the answer back by the same index. */
const ONCE = 0, ALWAYS = 1, DENY = 2;

/** Electron `dialog.showMessageBox` options asking about `action` from `box` ({ name, host }). */
function dialogFor(action, box) {
  const who = boxLabel(box);
  const short = box?.name || "this box";
  if (action.kind === "cap") {
    return {
      type: "question",
      title: "Allow this?",
      message: `${who} wants to ${action.label} on this computer.`,
      detail: `${action.detail}\n\nTori on ${short} asked for this. Only allow it if you asked Tori to.`,
      buttons: ["Allow once", `Always let ${short} ${action.label}`, "Don't allow"],
      defaultId: DENY,
      cancelId: DENY,
      noLink: true,
    };
  }
  const what = action.kind === "app" ? `open the app “${action.label}”` : action.key.startsWith("mailto:") ? `start an ${action.label}` : `open a page on ${action.label}`;
  return {
    type: "question",
    title: "Allow this?",
    message: `${who} wants to ${what} on this computer.`,
    detail: action.kind === "app"
      ? `App: ${action.target}\n\nTori on ${short} asked for this. Only allow it if you asked Tori to.`
      : `It will open:\n${action.target}\n\nTori on ${short} asked for this. Only allow it if you asked Tori to.`,
    buttons: ["Allow once", `Always allow ${action.label} from ${short}`, "Don't allow"],
    defaultId: DENY,
    cancelId: DENY,
    noLink: true,
  };
}

/** The person's answer: "once", "always" or "deny" (closing the dialog is "deny"). */
function choice(response) {
  return response === ONCE ? "once" : response === ALWAYS ? "always" : "deny";
}

/** Why a path given to open_folder won't open (null when it may): only folders,
 *  never an app bundle or a file (opening those would run them). */
function folderProblem(p, stat) {
  if (!stat) return `There's no folder at ${p}.`;
  if (!stat.isDirectory()) return `${p} isn't a folder.`;
  if (/\.(app|bundle|framework|plugin|prefpane|workflow|action|kext|pkg|mpkg)\/?$/i.test(p)) return `${p} is an app, not a folder.`;
  return null;
}

/** Whether a link the app's own screens ask to open follows the person's own click or key press. */
function byThePerson(lastInputAt, now) {
  return Number.isFinite(lastInputAt) && now - lastInputAt >= 0 && now - lastInputAt <= GESTURE_MS;
}

module.exports = { PREF_KEY, describe, isAllowed, remember, forget, listAllowed, dialogFor, choice, folderProblem, byThePerson };
