// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// TT Station, the QuietBox's desktop app.
//
// A native window (its own dock icon, tray, notifications, start at login)
// whose screens are served by the box's own agent at /app, so the screens and
// the API share one origin. Signing in needs no code on the box itself: the
// agent's `local-secret` file (readable only by this user) proves we're the
// box's owner, and buys a pairing token like the phone's.

const { app, BrowserWindow, Tray, Menu, Notification, ipcMain, shell, globalShortcut, nativeImage, net, session, safeStorage } = require("electron");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { validHost, validUser, cleanName, sameOrigin, webAddress, shq } = require("./validate.cjs");
const tokenstore = require("./tokenstore.cjs");

app.setName("TT Station");
// The dock groups windows by this id; it matches tt-station.desktop.
app.setAppUserModelId?.("tt-station");
process.env.CHROME_DESKTOP = "tt-station.desktop";

const IS_MAC = process.platform === "darwin";
// On the box itself this is the agent's own config folder (its local secret signs
// this app in); on a Mac, the app's own folder: it only ever pairs with boxes.
const CONFIG_DIR = process.env.TT_CONFIG_DIR || (IS_MAC ? path.join(app.getPath("appData"), "TT Station") : path.join(os.homedir(), ".config", "tt-station"));
const TOKEN_FILE = path.join(CONFIG_DIR, "desktop-token");
// Other boxes this app is paired with: [{ id, name, host, tls, fp, token }], and which one the window shows.
// Both files hold tokens sealed with the OS keychain where there is one (./tokenstore.cjs).
const BOXES_FILE = path.join(CONFIG_DIR, "desktop-boxes.json");
const ICON = path.join(__dirname, "..", "assets", "icon.png");
const TRAY_ICON = path.join(__dirname, "..", "assets", "tray.png");

let win = null;
let overlay = null; // Tori's floating window
const shown = new Set(); // notifications still in GNOME's tray (they badge the dock icon)
let tray = null;
let auth = null; // { base, token, name }
const lastSeq = new Map(); // box id → the last event seen
let quitting = false;
const appWindows = new Map();

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on("second-instance", (_event, argv) => {
    // GNOME shortcuts launch us with these: summon Tori, don't raise the main window.
    if (argv.includes("--tori") || argv.includes("--talk")) return showTori("talk");
    show();
    if (argv.includes("--chat")) win?.webContents.send("navigate", "/chat");
  });
}

// ---------------------------------------------------------------- auth

function readSecret() {
  try {
    return JSON.parse(fs.readFileSync(path.join(CONFIG_DIR, "local-secret"), "utf8"));
  } catch {
    return null;
  }
}

async function call(base, method, route, { token, body, headers = {}, timeout = 15000 } = {}) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeout);
  try {
    const res = await net.fetch(base + route, {
      method,
      headers: {
        ...(body ? { "content-type": "application/json" } : {}),
        ...(token ? { authorization: `Bearer ${token}` } : {}),
        ...headers,
      },
      body: body ? JSON.stringify(body) : undefined,
      signal: controller.signal,
    });
    const text = await res.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* not JSON */ }
    return { ok: res.ok, status: res.status, json };
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------- other boxes
//
// Another QuietBox is added with the pairing link from its own TT Station
// (Settings → Pairing and security → Pair another computer): it carries the
// box's HTTPS port and its certificate's fingerprint, which every connection
// to it is checked against (the box's certificate is its own, not a CA's).

// Read once the app is ready: safeStorage can't unseal tokens before then.
let boxes = { current: "local", boxes: [] };

/** Where this computer keeps box tokens: { protected, backend, where } (see tokenstore.cjs). */
const tokenStorage = () => tokenstore.storageInfo(safeStorage, process.platform);

function readBoxes() {
  let text;
  try { text = fs.readFileSync(BOXES_FILE, "utf8"); } catch { return { current: "local", boxes: [] }; }
  const { current, boxes: list, migrate } = tokenstore.loadBoxes(text, safeStorage, tokenStorage());
  const read = { current, boxes: list };
  // Plain tokens from before the keychain: seal them now (the write is one rename,
  // and a token is only sealed once it's been unsealed back, so none is lost).
  if (migrate) {
    try { tokenstore.writePrivate(BOXES_FILE, tokenstore.dumpBoxes(read, safeStorage, tokenStorage())); } catch (e) { console.error(`TT Station: couldn't move box tokens to the keychain: ${e.message}`); }
  }
  return read;
}

function saveBoxes() {
  tokenstore.writePrivate(BOXES_FILE, tokenstore.dumpBoxes(boxes, safeStorage, tokenStorage()));
  if (typeof buildMenu === "function" && app.isReady()) buildMenu();
}

/** This desktop's own token for the box it runs on (null before the first pairing). */
function readLocalToken() {
  let text;
  try { text = fs.readFileSync(TOKEN_FILE, "utf8"); } catch { return null; } // first run
  const { token, migrate } = tokenstore.loadToken(text, safeStorage, tokenStorage());
  if (token && migrate) {
    // Only once it really sealed: a keychain that won't round-trip leaves the file as it is.
    const sealed = tokenstore.seal(token, safeStorage, tokenStorage());
    if (tokenstore.isSealed(sealed)) {
      try { tokenstore.writePrivate(TOKEN_FILE, sealed); } catch (e) { console.error(`TT Station: couldn't move the token to the keychain: ${e.message}`); }
    }
  }
  return token;
}

function writeLocalToken(token) {
  tokenstore.writePrivate(TOKEN_FILE, tokenstore.seal(token, safeStorage, tokenStorage()));
}

/** The SHA-256 of a PEM certificate's DER bytes, in hex (how the box states its fingerprint). */
function certSha256(pem) {
  const der = Buffer.from(String(pem).replace(/-----[^-]+-----/g, "").replace(/\s+/g, ""), "base64");
  return crypto.createHash("sha256").update(der).digest("hex");
}

/** Boxes being paired right now: trusted for the pairing call only. */
const pairing = new Map();

function pinnedBox(hostname) {
  return boxes.boxes.find((b) => b.host === hostname) || pairing.get(hostname) || null;
}

function trustPinnedBoxes() {
  session.defaultSession.setCertificateVerifyProc((req, done) => {
    const box = pinnedBox(req.hostname);
    if (!box) return done(-3); // not ours: the usual checks
    done(certSha256(req.certificate.data) === String(box.fp).toLowerCase() ? 0 : -2);
  });
}

function parsePairLink(link) {
  let url;
  try { url = new URL(String(link).trim()); } catch { throw new Error("That isn't a pairing link."); }
  const q = url.searchParams;
  if (url.protocol !== "ttstation:" || !q.get("pair") || !q.get("code")) throw new Error("That isn't a TT Station pairing link.");
  if (!q.get("tls") || !q.get("fp")) throw new Error("That device's TT Station is too old to pair securely (its link has no certificate). Update it first.");
  // The name and address end up in a Terminal script and ~/.ssh/config.
  if (!validHost(q.get("host"))) throw new Error("That pairing link's device address isn't valid.");
  const name = cleanName(q.get("name") || q.get("host"));
  if (!name) throw new Error("That pairing link's device name isn't valid.");
  return { name, host: q.get("host"), tls: Number(q.get("tls")), fp: q.get("fp").toLowerCase(), pair: q.get("pair"), code: q.get("code") };
}

async function addBox(link) {
  const p = parsePairLink(link);
  const local = await credsFor("local").catch(() => null);
  const mine = local?.token ? await call(local.base, "GET", "/security", { token: local.token }).catch(() => null) : null;
  if (mine?.json?.tls?.sha256 && mine.json.tls.sha256.toLowerCase() === p.fp) throw new Error("That's this device.");
  return pairFinish(p);
}

const hostUrl = (host, port) => `https://${host.includes(":") ? `[${host}]` : host}:${port}`;

/** Completes a pairing ({host, tls, fp, name, pair, code}); saves the box. */
async function pairFinish(p) {
  if (!validHost(p.host)) throw new Error("That device address isn't valid.");
  p = { ...p, name: cleanName(p.name) || p.host };
  const base = hostUrl(p.host, p.tls);
  pairing.set(p.host, p);
  try {
    const done = await call(base, "POST", "/pair/complete", { body: { pair_id: p.pair, code: p.code, ...thisDevice() } });
    if (!done.ok || !done.json?.token) throw new Error(done.status === 401 ? "The code was wrong or has expired. Start again for a new one." : (done.json?.error || `The box answered ${done.status}.`));
    const id = p.fp.slice(0, 12);
    const old = boxes.boxes.find((b) => b.id === id);
    boxes.boxes = boxes.boxes.filter((b) => b.id !== id);
    boxes.boxes.push({ ...(old || {}), id, name: p.name, host: p.host, tls: p.tls, fp: p.fp, token: done.json.token });
    saveBoxes();
    return id;
  } catch (e) {
    if (String(e.message || e).includes("fetch failed")) throw new Error(`Couldn't reach ${p.name} at ${p.host}. Is it on and on this network?`);
    throw e;
  } finally {
    pairing.delete(p.host);
  }
}

/** How to reach a box: { base, token, name } (the local one signs itself in). */
async function credsFor(id) {
  if (id === "local") {
    const secret = readSecret();
    if (!secret) throw new Error("The TT Station agent isn't set up on this computer (no local secret).");
    return { base: `http://127.0.0.1:${secret.port}`, token: readLocalToken(), name: secret.name };
  }
  const b = boxes.boxes.find((x) => x.id === id);
  if (!b) throw new Error("That device isn't paired with this computer.");
  return { base: `https://${b.host.includes(":") ? `[${b.host}]` : b.host}:${b.tls}`, token: b.token, name: b.name };
}

/** Set when this computer signs itself out of the box it runs on (Settings → Devices). */
let localSignedOut = false;

function switchBox(id) {
  if (id !== "local" && !boxes.boxes.some((b) => b.id === id)) return;
  if (id === "local") localSignedOut = false;
  boxes.current = id;
  saveBoxes();
  auth = null;
  // Tori talks to the box the window shows.
  if (overlay && !overlay.isDestroyed()) overlay.destroy();
  overlay = null;
  load();
  refreshTray();
}

function boxList() {
  const local = readSecret();
  return {
    current: boxes.current,
    boxes: [
      ...(local ? [{ id: "local", name: local.name, host: "this computer", local: true }] : []),
      ...boxes.boxes.map((b) => ({ id: b.id, name: b.name, host: b.host, local: false })),
    ],
  };
}

/** The window has no box to show yet: pair one first (a Mac on first run). */
class NeedsBox extends Error {}

/** A token for the box the window shows. */
async function signIn() {
  // Signed out from Settings: stay on the connect screen until asked back.
  if (boxes.current === "local" && localSignedOut) throw new NeedsBox("Signed out.");
  if (boxes.current === "local" && !readSecret()) {
    if (!boxes.boxes.length) throw new NeedsBox("No box yet.");
    boxes.current = boxes.boxes[0].id;
    saveBoxes();
  }
  if (boxes.current !== "local") {
    const c = await credsFor(boxes.current);
    const box = boxes.boxes.find((b) => b.id === boxes.current);
    if (!c.token && box?.sealedToken) throw new Error(`This computer's keychain wouldn't open the saved sign-in for ${c.name}. Unlock your keychain and reopen TT Station, or add the device again.`);
    const check = await call(c.base, "GET", "/people", { token: c.token }).catch((e) => ({ ok: false, status: 0, error: e }));
    if (check.ok) return c;
    if (check.status === 401) throw new Error(`${c.name} doesn't recognize this computer any more${check.json?.error?.includes("expired") ? " (it went unused too long)" : ""}. Add it again (the device name at top left → Add another device).`);
    throw new Error(`Can't reach ${c.name} right now.`);
  }
  return signInLocal();
}

/** A token for this desktop: the saved one if it still works, else a new pairing. */
async function signInLocal() {
  const secret = readSecret();
  if (!secret) throw new Error("The TT Station agent isn't set up on this computer (no local secret).");
  const base = `http://127.0.0.1:${secret.port}`;
  const token = readLocalToken();
  if (token) {
    const check = await call(base, "GET", "/people", { token });
    if (check.ok) return { base, token, name: secret.name };
  }
  const offer = await call(base, "POST", "/pair/offer", { headers: { "x-tt-local-secret": secret.secret } });
  if (!offer.ok) throw new Error(offer.json?.error || `The agent refused (${offer.status}).`);
  const done = await call(base, "POST", "/pair/complete", {
    body: { pair_id: offer.json.pair_id, code: offer.json.code, ...thisDevice() },
  });
  if (!done.ok || !done.json?.token) throw new Error(done.json?.error || "Pairing didn't complete.");
  writeLocalToken(done.json.token);
  return { base, token: done.json.token, name: secret.name };
}

/** This computer, as a person would name it ("Jane's MacBook Pro"). */
function deviceName() {
  if (IS_MAC) {
    try { return require("node:child_process").execFileSync("scutil", ["--get", "ComputerName"], { encoding: "utf8" }).trim() || os.hostname(); } catch { /* fall through */ }
  }
  return os.hostname().replace(/\.local$/, "");
}

/** How this computer introduces itself when pairing, for the box's device list. */
function thisDevice() {
  const platform = { darwin: "macOS", win32: "Windows", linux: "Linux" }[process.platform] ?? process.platform;
  return { name: deviceName(), platform };
}

/** Boxes advertising themselves on this network (`_tenstorrent._tcp`), for a couple of seconds. */
function discover() {
  return new Promise((resolve) => {
    let Bonjour;
    try { ({ Bonjour } = require("bonjour-service")); } catch { resolve([]); return; }
    const bonjour = new Bonjour();
    const found = new Map();
    const browser = bonjour.find({ type: "tenstorrent" }, (svc) => {
      const v4 = (svc.addresses || []).find((a) => /^\d+\.\d+\.\d+\.\d+$/.test(a));
      const host = v4 || svc.host;
      if (!host) return;
      const txt = svc.txt || {};
      found.set(host, { name: txt.name || svc.name, host, tls: Number(txt.tls) || (svc.port + 1), chips: txt.chips || null, mesh: txt.device_mesh || txt.mesh || null,
        paired: boxes.boxes.some((b) => b.host === host) });
    });
    setTimeout(() => { try { browser.stop(); bonjour.destroy(); } catch { /* done */ } resolve([...found.values()]); }, 2500);
  });
}

/** The certificate a box presents (its fingerprint, to show and pin) and its name. */
async function probe(host, tls) {
  if (!validHost(host)) throw new Error("That isn't a device address.");
  const fp = await new Promise((resolve, reject) => {
    const sock = require("node:tls").connect({ host, port: tls, rejectUnauthorized: false, timeout: 6000 }, () => {
      const cert = sock.getPeerCertificate();
      sock.end();
      if (!cert?.raw) reject(new Error("The device didn't present a certificate."));
      else resolve(crypto.createHash("sha256").update(cert.raw).digest("hex"));
    });
    sock.on("timeout", () => { sock.destroy(); reject(new Error(`No answer from ${host}:${tls}. Is the device on, and this computer on the same network?`)); });
    sock.on("error", (e) => reject(new Error(e.code === "ECONNREFUSED" ? `${host} isn't running TT Station on port ${tls}.` : `Couldn't reach ${host}: ${e.message}`)));
  });
  pairing.set(host, { host, fp });
  try {
    const status = await call(hostUrl(host, tls), "GET", "/status", { timeout: 6000 });
    return { fp, name: status.json?.name || host, chips: status.json?.chips || null };
  } finally {
    pairing.delete(host);
  }
}

// ---------------------------------------------------------------- signing in with the device's account
//
// Works on every Tenstorrent device, screen or not: the person proves they
// own it the way they'd SSH in (their keys, or its username and password), and
// over that SSH session the device's agent pairs this computer -- its local
// secret, readable only by its own user, buys a pairing like the device's own
// desktop app. The certificate the device reports over SSH must match the one
// this computer sees on the network, or nothing is saved.

// Runs on the device: find TT Station's config, pair, say how (JSON on stdout).
const PAIR_ON_DEVICE = String.raw`
import json, os, sys, urllib.request
dirs = [os.environ.get("TT_CONFIG_DIR"), os.path.expanduser("~/.config/tt-station")]
found = next((d for d in dirs if d and os.path.isfile(os.path.join(d, "local-secret"))), None)
if not found:
    print(json.dumps({"error": "TT Station isn't set up for this account on the device (no ~/.config/tt-station/local-secret). Sign in as the account that runs it."}))
    sys.exit(0)
s = json.load(open(os.path.join(found, "local-secret")))
req = urllib.request.Request("http://127.0.0.1:%d/pair/offer" % s["port"], data=b"", method="POST", headers={"x-tt-local-secret": s["secret"]})
try:
    offer = json.load(urllib.request.urlopen(req, timeout=10))
except Exception as e:
    print(json.dumps({"error": "TT Station's agent on the device didn't answer (%s). Is it running? systemctl --user status tt-station-agentd" % e}))
    sys.exit(0)
print(json.dumps({"name": s.get("name"), "pair_id": offer.get("pair_id"), "code": offer.get("code"), "tls": offer.get("tls"), "user": os.environ.get("USER")}))
`;

/** SSH to the device and pair there. No password: the person's own keys (or none and it fails fast). */
function sshPair(host, user, password) {
  return new Promise((resolve, reject) => {
    if (!validHost(host)) return reject(new Error("That device address isn't valid."));
    if (!validUser(user)) return reject(Object.assign(new Error("That isn't a username."), { kind: "auth" }));
    const args = ["-o", "ConnectTimeout=8", "-o", "StrictHostKeyChecking=accept-new", "-o", "NumberOfPasswordPrompts=1"];
    const env = { ...process.env };
    let askpass = null;
    if (password) {
      // ssh asks this program for the password (no terminal here); it hands over
      // what the person typed, from the environment, and is deleted after.
      askpass = path.join(os.tmpdir(), `tt-station-askpass-${process.pid}-${Date.now()}`);
      fs.writeFileSync(askpass, '#!/bin/sh\nprintf "%s\\n" "$TT_STATION_PW"\n', { mode: 0o700 });
      Object.assign(env, { SSH_ASKPASS: askpass, SSH_ASKPASS_REQUIRE: "force", DISPLAY: env.DISPLAY || ":0", TT_STATION_PW: password });
      args.push("-o", "PreferredAuthentications=keyboard-interactive,password,publickey");
    } else {
      args.push("-o", "BatchMode=yes");
    }
    const child = require("node:child_process").spawn("ssh", [...args, `${user}@${host}`, "python3", "-"], { env, stdio: ["pipe", "pipe", "pipe"] });
    let out = "", err = "";
    child.stdout.on("data", (d) => { out += d; });
    child.stderr.on("data", (d) => { err += d; });
    const timer = setTimeout(() => child.kill(), 30000);
    child.on("error", (e) => { clearTimeout(timer); reject(new Error(`Couldn't run ssh: ${e.message}`)); });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (askpass) fs.rmSync(askpass, { force: true });
      const last = out.trim().split("\n").pop() || "";
      let r = null;
      try { r = JSON.parse(last); } catch { /* not ours */ }
      if (r) return r.error ? reject(Object.assign(new Error(r.error), { kind: "device" })) : resolve(r);
      const e = err.toLowerCase();
      if (e.includes("permission denied")) return reject(Object.assign(new Error(password ? "That username and password didn't work." : "No SSH key of yours is let in."), { kind: "auth" }));
      if (e.includes("host key verification failed") || e.includes("remote host identification has changed"))
        return reject(new Error(`${host}'s SSH identity changed since this computer last saw it. If that's expected, remove its line from ~/.ssh/known_hosts and try again.`));
      if (e.includes("connection refused")) return reject(new Error(`${host} doesn't accept SSH (port 22). Turn SSH on there, or use a pairing code.`));
      if (e.includes("timed out") || e.includes("no route")) return reject(new Error(`Couldn't reach ${host} over SSH.`));
      if (e.includes("python3") && e.includes("not found")) return reject(new Error("The device has no python3, which pairing over SSH needs. Use a pairing code."));
      reject(new Error(err.trim().split("\n").pop() || `ssh stopped (${code}).`));
    });
    child.stdin.end(PAIR_ON_DEVICE);
  });
}

/** Pairs with a device by signing in to it ({host, tls, fp, name} from the probe). */
async function pairWithAccount(p, user, password) {
  const r = await sshPair(p.host, user, password);
  const fp = String(r.tls?.sha256 || "").toLowerCase();
  if (!fp || fp !== String(p.fp).toLowerCase())
    throw new Error("The device's certificate over SSH doesn't match the one on the network, so this wasn't saved. Something between this computer and the device may be pretending to be it.");
  const id = await pairFinish({ host: p.host, tls: Number(r.tls?.port) || p.tls, fp, name: r.name || p.name, pair: r.pair_id, code: String(r.code) });
  const box = boxes.boxes.find((b) => b.id === id);
  if (box) { box.loginUser = validUser(r.user) ? r.user : user; saveBoxes(); }
  return id;
}

/** Asks the box for a pairing code: it shows on the box's desktop app and paired phones. */
async function pairStart(host, tls, fp) {
  pairing.set(host, { host, fp });
  try {
    const r = await call(hostUrl(host, tls), "POST", "/pair/init", { body: { device: deviceName() } });
    if (r.status === 429) throw new Error("Too many pairing tries just now. Wait a minute.");
    if (!r.ok || !r.json?.pair_id) throw new Error(`The device answered ${r.status}.`);
    return r.json.pair_id;
  } finally {
    pairing.delete(host);
  }
}

// ---------------------------------------------------------------- SSH from this computer
//
// The box's own command line, in Terminal: a key just for TT Station
// (~/.ssh/tt-station_ed25519) whose public half the box adds to its
// authorized_keys over the paired connection, once per box.

const SSH_KEY = path.join(os.homedir(), ".ssh", "tt-station_ed25519");

function run(cmd, args) {
  return new Promise((resolve, reject) => {
    require("node:child_process").execFile(cmd, args, { encoding: "utf8" }, (err, out, errOut) => (err ? reject(new Error(errOut?.trim() || err.message)) : resolve(out)));
  });
}

async function sshReady() {
  const id = boxes.current;
  if (id === "local") throw new Error("This is the device itself: open a terminal here.");
  const box = boxes.boxes.find((b) => b.id === id);
  if (!box) throw new Error("Pick a device first.");
  // Saved before addresses were checked, or edited by hand.
  if (!validHost(box.host)) throw new Error("This device's saved address isn't valid. Remove it and add it again.");
  if (!fs.existsSync(SSH_KEY)) {
    fs.mkdirSync(path.dirname(SSH_KEY), { recursive: true, mode: 0o700 });
    await run("ssh-keygen", ["-t", "ed25519", "-N", "", "-C", `tt-station@${deviceName().replace(/\s+/g, "-")}`, "-f", SSH_KEY]);
  }
  if (!box.sshUser) {
    const c = await credsFor(id);
    const pub = fs.readFileSync(`${SSH_KEY}.pub`, "utf8").trim();
    const r = await call(c.base, "POST", "/ssh/authorize", { token: c.token, body: { public_key: pub, label: `tt-station-${deviceName().replace(/[^A-Za-z0-9._-]+/g, "-")}` } });
    if (!r.ok) throw new Error(r.json?.error || `The device wouldn't add this computer's key (${r.status}).`);
    box.sshUser = r.json?.ssh_user || "ttuser";
    saveBoxes();
  }
  if (!validUser(box.sshUser)) throw new Error("The device named an account this computer can't use for SSH.");
  return box;
}

const sshArgs = (box) => ["-i", SSH_KEY, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=accept-new", `${box.sshUser}@${box.host}`];

/** "terminal": the box's shell in a new Terminal window; "command": the ssh command; "vscode": the box in VS Code. */
async function sshAction(what) {
  const box = await sshReady();
  const command = ["ssh", ...sshArgs(box)].map((a, i) => (i ? shq(a) : a)).join(" ");
  if (what === "command") return { command, user: box.sshUser, host: box.host };
  if (what === "vscode") {
    // VS Code's Remote-SSH reads ~/.ssh/config: one block for this box, ours to rewrite.
    const alias = `tt-${box.name.replace(/[^A-Za-z0-9_-]/g, "-")}`;
    const cfg = path.join(os.homedir(), ".ssh", "config");
    let text = "";
    try { text = fs.readFileSync(cfg, "utf8"); } catch { /* none yet */ }
    const begin = `# >>> TT Station: ${alias}`, end = `# <<< TT Station: ${alias}`;
    text = text.replace(new RegExp(`\\n?${begin}[\\s\\S]*?${end}\\n?`), "\n");
    text = `${text.trimEnd()}\n\n${begin}\nHost ${alias}\n  HostName ${box.host}\n  User ${box.sshUser}\n  IdentityFile ${SSH_KEY}\n  IdentitiesOnly yes\n  StrictHostKeyChecking accept-new\n${end}\n`;
    fs.writeFileSync(cfg, text.trimStart(), { mode: 0o600 });
    await shell.openExternal(`vscode://vscode-remote/ssh-remote+${alias}/home/${box.sshUser}`);
    return { alias };
  }
  // A .command file opens in Terminal and runs (no automation permission needed).
  const file = path.join(os.tmpdir(), `tt-station-ssh-${box.id}.command`);
  fs.writeFileSync(file, `#!/bin/bash\nclear\necho ${shq(`TT Station → ${cleanName(box.name) || box.host} (${box.host})`)}\nexec ${command}\n`, { mode: 0o700 });
  if (IS_MAC) {
    await run("open", ["-a", "Terminal", file]);
  } else {
    // The terminal runs until it's closed: don't wait for it.
    const { spawn } = require("node:child_process");
    const child = spawn("x-terminal-emulator", ["-e", file], { detached: true, stdio: "ignore" });
    child.on("error", () => spawn("gnome-terminal", ["--", file], { detached: true, stdio: "ignore" }).unref());
    child.unref();
  }
  return {};
}

// ---------------------------------------------------------------- this computer, for Tori on it
//
// When TT Station runs on another computer than the device, Tori's computer
// tools act here: the device sends the step ("on_client"), this runs it and the
// page posts the result back.

let localRecording = false;

/** Asks the main window to start or stop recording this computer's microphone. */
function clientRecord(action, title) {
  return new Promise((resolve) => {
    if (!win || win.isDestroyed()) return resolve({ ok: false, error: "TT Station's window is closed." });
    const nonce = crypto.randomUUID();
    const timer = setTimeout(() => { ipcMain.removeAllListeners(`client-record-done:${nonce}`); resolve({ ok: false, error: "TT Station's window didn't answer." }); }, 20000);
    ipcMain.once(`client-record-done:${nonce}`, (_e, result) => {
      clearTimeout(timer);
      if (result?.ok) localRecording = action === "start";
      refreshTray();
      resolve(result || { ok: false, error: "No answer." });
    });
    win.webContents.send("client-record", { nonce, action, title });
  });
}

const osa = (script) => run("osascript", ["-e", script]);

/** The person's own folders, by the names people use. */
function resolveFolder(said) {
  const s = String(said || "").trim().replace(/^~\/?/, "");
  const named = { home: "", desktop: "Desktop", documents: "Documents", downloads: "Downloads", pictures: "Pictures", photos: "Pictures", music: "Music", movies: "Movies", videos: "Movies", applications: "/Applications" };
  const key = s.toLowerCase();
  if (key in named) return named[key].startsWith("/") ? named[key] : path.join(os.homedir(), named[key]);
  return path.isAbsolute(s) ? s : path.join(os.homedir(), s);
}

async function computerTool(tool, args) {
  switch (tool) {
    case "open_app": {
      const url = args.url ? String(args.url) : null;
      if (url) {
        if (!/^(https?:|mailto:)/i.test(url)) throw new Error("Only web and mail links open from here.");
        await shell.openExternal(url);
        return { summary: `Opened ${url.replace(/^https?:\/\//, "").split("/")[0]}`, result: { opened: url } };
      }
      const name = String(args.app || "").trim();
      if (!name) throw new Error("Say which app.");
      if (IS_MAC) {
        try { await run("open", ["-a", name]); return { summary: `Opened ${name}`, result: { opened: name } }; } catch { /* look for it */ }
        const apps = ["/Applications", "/System/Applications", "/System/Applications/Utilities", path.join(os.homedir(), "Applications")]
          .flatMap((d) => { try { return fs.readdirSync(d).filter((f) => f.endsWith(".app")).map((f) => path.join(d, f)); } catch { return []; } });
        const want = name.toLowerCase().replace(/\.app$/, "");
        const hit = apps.find((a) => path.basename(a, ".app").toLowerCase() === want) || apps.find((a) => path.basename(a, ".app").toLowerCase().includes(want));
        if (!hit) throw new Error(`There's no app called ${name} on this Mac.`);
        await run("open", [hit]);
        return { summary: `Opened ${path.basename(hit, ".app")}`, result: { opened: path.basename(hit, ".app") } };
      }
      await run("gtk-launch", [name]).catch(() => { throw new Error(`Couldn't open ${name} here.`); });
      return { summary: `Opened ${name}`, result: { opened: name } };
    }
    case "control_computer": {
      if (!IS_MAC) throw new Error("That only works on a Mac or on the device itself for now.");
      const action = String(args.action || ""), v = Number(args.value);
      // Music and Spotify, whichever is playing; Spotify is only named when it's
      // running (its name in a script would otherwise make macOS ask where it is).
      const media = async (cmd) => {
        const running = (await run("ps", ["-axco", "command"]).catch(() => "")).split("\n").map((l) => l.trim());
        const targets = ["Music", "Spotify"].filter((a) => running.includes(a));
        if (!targets.length) throw new Error("Nothing is playing (Music and Spotify aren't open).");
        for (const a of targets) await run("osascript", ["-e", `run script "tell application \\"${a}\\" to ${cmd}"`]);
      };
      const actions = {
        volume_set: () => osa(`set volume output volume ${Math.max(0, Math.min(100, Math.round(Number.isFinite(v) ? (v <= 1 ? v * 100 : v) : 50)))}`),
        volume_up: () => osa("set volume output volume ((output volume of (get volume settings)) + 10)"),
        volume_down: () => osa("set volume output volume ((output volume of (get volume settings)) - 10)"),
        mute: () => osa("set volume with output muted"),
        unmute: () => osa("set volume without output muted"),
        play_pause: () => media("playpause"),
        pause_media: () => media("pause"),
        play_media: () => media("play"),
        stop_media: () => media("pause"),
        next_track: () => media("next track"),
        previous_track: () => media("previous track"),
        lock_screen: () => run("pmset", ["displaysleepnow"]),
        dark_mode: () => osa(`tell application "System Events" to tell appearance preferences to set dark mode to ${v === 0 || args.value === false ? "false" : "true"}`),
        light_mode: () => osa('tell application "System Events" to tell appearance preferences to set dark mode to false'),
      };
      if (!actions[action]) throw new Error(action.startsWith("dnd") || action.includes("disturb") ? "macOS doesn't let apps change Focus / Do Not Disturb." : `${action} isn't something this Mac can do from here.`);
      await actions[action]();
      const words = { volume_set: "Set the volume", volume_up: "Turned the volume up", volume_down: "Turned the volume down", mute: "Muted", unmute: "Unmuted", lock_screen: "Locked the screen", dark_mode: "Changed dark mode", light_mode: "Switched to light mode" };
      return { summary: `${words[action] || "Done"} on this Mac`, result: { done: action } };
    }
    case "open_folder": {
      const p = resolveFolder(args.path);
      const err = await shell.openPath(p);
      if (err) throw new Error(`Couldn't open ${p}: ${err}`);
      return { summary: `Opened ${p.replace(os.homedir(), "~")}`, result: { opened: p } };
    }
    case "find_files": {
      const name = String(args.name || "").trim();
      if (!name) throw new Error("Say what to look for.");
      const out = IS_MAC
        ? await run("mdfind", ["-onlyin", os.homedir(), "-name", name])
        : await run("find", [os.homedir(), "-maxdepth", "6", "-iname", `*${name}*`, "-not", "-path", "*/.*"]).catch(() => "");
      const matches = out.split("\n").filter(Boolean).filter((f) => !f.includes("/Library/")).slice(0, 20);
      return { summary: `Found ${matches.length} matching file(s)`, result: { matches } };
    }
    case "open_station": {
      const page = String(args.page || "home").replace(/^\//, "");
      show();
      win?.webContents.send("navigate", page === "home" ? "/" : `/${page}`);
      return { summary: `Opened ${page} in TT Station`, result: { page } };
    }
    case "start_recording": {
      const r = await clientRecord("start", args.title ? String(args.title) : undefined);
      if (!r.ok) throw new Error(r.error);
      return { summary: r.summary || "Started recording on this computer" };
    }
    case "stop_recording": {
      const r = await clientRecord("stop");
      if (!r.ok) throw new Error(r.error);
      return { summary: r.summary || "Stopped recording; transcribing" };
    }
    default:
      throw new Error(`${tool} can't run on this computer.`);
  }
}

// ---------------------------------------------------------------- the device's private apps
//
// Apps that listen only on the device itself (Jupyter, Flowise…: 127.0.0.1
// there) are reached from another computer through SSH, with TT Station's
// own key: a local port here forwards to the app's port on the device.

const tunnels = new Map();

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = require("node:net").createServer();
    srv.listen(0, "127.0.0.1", () => { const { port } = srv.address(); srv.close(() => resolve(port)); });
    srv.on("error", reject);
  });
}

function portOpen(port) {
  return new Promise((resolve) => {
    const s = require("node:net").connect({ host: "127.0.0.1", port }, () => { s.end(); resolve(true); });
    s.on("error", () => resolve(false));
  });
}

/** `url` as this computer can open it: a device-only address goes through a tunnel. */
async function reachable(url) {
  const u = new URL(url);
  if (boxes.current === "local" || !["127.0.0.1", "localhost", "[::1]"].includes(u.hostname)) return url;
  const box = await sshReady();
  const remotePort = Number(u.port) || (u.protocol === "https:" ? 443 : 80);
  const key = `${box.id}:${remotePort}`;
  let t = tunnels.get(key);
  if (!t || t.child.exitCode !== null || !(await portOpen(t.local))) {
    const local = await freePort();
    const child = require("node:child_process").spawn("ssh", ["-N", "-L", `${local}:127.0.0.1:${remotePort}`, "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=30", ...sshArgs(box)], { stdio: "ignore" });
    t = { local, child };
    tunnels.set(key, t);
    for (let i = 0; i < 50 && !(await portOpen(local)); i++) {
      if (child.exitCode !== null) throw new Error("Couldn't reach that app on the device (SSH tunnel failed).");
      await new Promise((r) => setTimeout(r, 200));
    }
  }
  u.hostname = "127.0.0.1";
  u.port = String(t.local);
  return u.toString();
}

app.on("will-quit", () => { for (const t of tunnels.values()) try { t.child.kill(); } catch { /* gone */ } });

// ---------------------------------------------------------------- window

/** On the box the window shows: the very same origin as its agent, not a prefix of it. */
function onBox(url) {
  return !!auth && sameOrigin(url, auth.base);
}

/** One of this app's own pages (the pairing and offline screens). */
function ownPage(url) {
  try {
    const u = new URL(url);
    return u.protocol === "file:" && path.dirname(require("node:url").fileURLToPath(u)) === __dirname;
  } catch { return false; }
}

/** A link leaving the app opens in the system browser, and only a web address does. */
function openOutside(url) {
  if (!webAddress(url)) return;
  // A device-only address (127.0.0.1 there) goes through a tunnel from another computer.
  reachable(url).then((r) => shell.openExternal(r), (e) => notify({ title: "Couldn't open that", body: String(e.message || e) }));
}

/** A window with the desktop bridge stays on the box's pages and ours: any
 *  other page, new window or navigation opens outside, in the browser. */
function keepOnBox(contents) {
  contents.setWindowOpenHandler(({ url }) => {
    openOutside(url);
    return { action: "deny" };
  });
  contents.on("will-navigate", (event, url) => {
    if (onBox(url) || ownPage(url)) return;
    event.preventDefault();
    openOutside(url);
  });
}

function createWindow() {
  win = new BrowserWindow({
    width: 1360,
    height: 880,
    minWidth: 980,
    minHeight: 640,
    title: "TT Station",
    icon: ICON,
    backgroundColor: "#050f0e",
    autoHideMenuBar: true,
    show: false,
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      sandbox: true,
      spellcheck: true,
    },
  });
  win.once("ready-to-show", () => win.show());
  // Looking at the app counts as reading its notifications: clear the dock badge.
  win.on("focus", clearNotifications);
  // Links that leave the app open in the system browser; the app's own routes stay.
  keepOnBox(win.webContents);
  // The device went away while a page was loading (say, a reload during a restart):
  // the "Connecting…" screen, which keeps trying, instead of a blank window.
  win.webContents.on("did-fail-load", (_e, code, _desc, url, isMainFrame) => {
    if (!isMainFrame || loading || code === -3 || !/^https?:/.test(url)) return;
    auth = null;
    clearTimeout(retry);
    win.loadFile(path.join(__dirname, "offline.html"), { query: { why: "The device didn't answer.", remote: boxes.current !== "local" ? "1" : "" } }).catch(() => {});
    retry = setTimeout(load, 5000);
  });
  // Closing hides to the tray, like other always-there apps; Quit is in the tray.
  win.on("close", (event) => {
    if (!quitting) {
      event.preventDefault();
      win.hide();
    }
  });
  load();
}

let loading = false;

async function load() {
  if (!win || win.isDestroyed()) return; // the window closed (or the app is quitting)
  clearTimeout(retry);
  loading = true;
  try {
    auth = await signIn();
    await win.loadURL(`${auth.base}/app/`).finally(() => { loading = false; });
    if (!overlay) setTimeout(() => { if (!overlay && auth) createTori(null); }, 1500);
  } catch (error) {
    loading = false;
    auth = null;
    if (!win || win.isDestroyed()) return;
    if (error instanceof NeedsBox) {
      await win.loadFile(path.join(__dirname, "connect.html")).catch(() => {});
      return;
    }
    await win.loadFile(path.join(__dirname, "offline.html"), { query: { why: String(error.message || error), remote: boxes.current !== "local" ? "1" : "" } }).catch(() => {});
    retry = setTimeout(load, 5000);
  }
}

let retry = null;

/** The pairing screen: find a box on the network, or paste a link. */
function showConnect() {
  clearTimeout(retry);
  show();
  win?.loadFile(path.join(__dirname, "connect.html")).catch(() => {});
}

function clearNotifications() {
  for (const n of shown) { try { n.close(); } catch { /* already gone */ } }
  shown.clear();
}

function notify(options, onClick = show) {
  const n = new Notification({ icon: ICON, ...options });
  n.on("click", () => { shown.delete(n); onClick(); });
  n.on("close", () => shown.delete(n));
  shown.add(n);
  n.show();
  return n;
}

// ---------------------------------------------------------------- Tori

/** Tori's own small window, over everything: type or talk, then get back to work. */
function showTori(mode) {
  if (!auth) { show(); return; }
  if (overlay && !overlay.isDestroyed()) {
    overlay.show();
    overlay.focus();
    overlay.webContents.send("tori", mode);
    return;
  }
  createTori(mode);
}

/** Tori's window; `mode` null preloads it hidden, so the first summon is instant. */
function createTori(mode) {
  const url = `${auth.base}/app/tori${mode === null ? "?preload" : mode === "type" ? "?type" : ""}`;
  overlay = new BrowserWindow({
    width: 620,
    height: 560,
    minWidth: 440,
    minHeight: 360,
    title: "Tori",
    icon: ICON,
    frame: false,
    transparent: true,
    backgroundColor: "#00000000",
    alwaysOnTop: true,
    skipTaskbar: true,
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.cjs"), contextIsolation: true, sandbox: true },
  });
  overlay.setAlwaysOnTop(true, "floating");
  if (mode !== null) overlay.once("ready-to-show", () => { overlay.show(); overlay.focus(); });
  keepOnBox(overlay.webContents);
  overlay.on("close", (event) => {
    if (!quitting) { event.preventDefault(); overlay.hide(); overlay.webContents.send("tori", "hidden"); }
  });
  overlay.loadURL(url);
}

// ---------------------------------------------------------------- GNOME shortcuts

// Wayland gives apps no global hotkeys, so the shortcuts live in GNOME's own
// keyboard settings (Settings → Keyboard → Custom Shortcuts), running us with
// --tori / --talk; the running app picks that up as a second instance.
const KEYS_SCHEMA = "org.gnome.settings-daemon.plugins.media-keys";
const KEY_BASE = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings";
const PREFS_FILE = path.join(CONFIG_DIR, "desktop.json");
const DEFAULT_BINDING = "<Control><Alt>space";

function prefs() {
  try { return JSON.parse(fs.readFileSync(PREFS_FILE, "utf8")); } catch { return {}; }
}
function savePrefs(next) {
  fs.mkdirSync(CONFIG_DIR, { recursive: true });
  fs.writeFileSync(PREFS_FILE, JSON.stringify({ ...prefs(), ...next }, null, 2));
}
function toriBinding() { return prefs().toriShortcut || DEFAULT_BINDING; }

/** "<Control><Alt>space" → "Control+Alt+Space" (Electron's spelling, for X11). */
function accelerator(binding) {
  const mods = [...binding.matchAll(/<(\w+)>/g)].map((m) => ({ Control: "Control", Primary: "Control", Alt: "Alt", Shift: "Shift", Super: "Super" })[m[1]] ?? m[1]);
  const key = binding.replace(/<\w+>/g, "");
  const named = { space: "Space", Return: "Enter", Escape: "Esc", Tab: "Tab", BackSpace: "Backspace" }[key] ?? (key.length === 1 ? key.toUpperCase() : key);
  return [...mods, named].join("+");
}

const SHORTCUTS = [{ id: "tt-station-tori", name: "Tori", arg: "--tori", get binding() { return toriBinding(); } }];
const RETIRED = ["tt-station-talk"];

// The shortcut pokes this socket (via nc, ~5 ms) instead of starting a second
// Electron (~1.3 s); if the app isn't running, it starts it with --tori.
const SUMMON_SOCKET = path.join(process.env.XDG_RUNTIME_DIR || os.tmpdir(), "tt-station-tori.sock");

function listenForSummons() {
  const netmod = require("node:net");
  try { fs.rmSync(SUMMON_SOCKET, { force: true }); } catch { /* not there */ }
  const server = netmod.createServer((sock) => {
    let said = "";
    sock.on("data", (d) => { said += d.toString(); });
    sock.on("end", () => {
      const msg = said.trim();
      // "nav /models": Tori asked for a TT Station page.
      if (msg.startsWith("nav ")) {
        show();
        win?.webContents.send("navigate", msg.slice(4).trim() || "/");
        return;
      }
      showTori(msg === "type" ? "type" : "talk");
    });
    sock.on("error", () => {});
  });
  server.on("error", () => {});
  server.listen(SUMMON_SOCKET, () => { try { fs.chmodSync(SUMMON_SOCKET, 0o600); } catch { /* best effort */ } });
  app.on("will-quit", () => { try { fs.rmSync(SUMMON_SOCKET, { force: true }); } catch { /* gone */ } });
}

// The shortcut opens Tori's window in the running app, else starts the app.
function summonCommand(arg) {
  const launch = `${process.execPath} ${app.getAppPath()} --ozone-platform-hint=auto ${arg}`;
  return `sh -c 'echo talk | nc -NU "${SUMMON_SOCKET}" 2>/dev/null || exec ${launch}'`;
}

function gsettings(...args) {
  const { execFileSync } = require("node:child_process");
  return execFileSync("gsettings", args, { encoding: "utf8" }).trim();
}

function customList() {
  try {
    return [...gsettings("get", KEYS_SCHEMA, "custom-keybindings").matchAll(/'([^']+)'/g)].map((m) => m[1]);
  } catch {
    return null;
  }
}

function shortcutsInstalled() {
  // On a Mac the shortcut is Electron's own, always on.
  if (IS_MAC) return true;
  const list = customList();
  return list ? SHORTCUTS.every((k) => list.includes(`${KEY_BASE}/${k.id}/`)) : false;
}

function setShortcuts(on) {
  if (IS_MAC) return true;
  const list = customList();
  if (!list) throw new Error("This desktop doesn't use GNOME's keyboard shortcuts.");
  const ours = SHORTCUTS.map((k) => `${KEY_BASE}/${k.id}/`);
  const rest = list.filter((p) => !ours.includes(p) && !RETIRED.some((id) => p === `${KEY_BASE}/${id}/`));
  if (on) {
    for (const k of SHORTCUTS) {
      const where = `${KEYS_SCHEMA}.custom-keybinding:${KEY_BASE}/${k.id}/`;
      gsettings("set", where, "name", k.name);
      gsettings("set", where, "command", summonCommand(k.arg));
      gsettings("set", where, "binding", k.binding);
    }
  }
  const next = on ? [...rest, ...ours] : rest;
  gsettings("set", KEYS_SCHEMA, "custom-keybindings", `[${next.map((p) => `'${p}'`).join(", ")}]`);
  return on;
}

function show() {
  if (!win) return;
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
}

/** A connected app (Open WebUI, ComfyUI…) in its own window, not a browser tab. */
function openAppWindow(url, title) {
  const existing = appWindows.get(url);
  if (existing && !existing.isDestroyed()) {
    existing.show();
    existing.focus();
    return;
  }
  const w = new BrowserWindow({
    width: 1280,
    height: 860,
    title,
    icon: ICON,
    backgroundColor: "#050f0e",
    autoHideMenuBar: true,
    webPreferences: { contextIsolation: true, sandbox: true },
  });
  // No bridge in here, but its pop-ups still go to the browser, not bare windows.
  w.webContents.setWindowOpenHandler(({ url: link }) => {
    openOutside(link);
    return { action: "deny" };
  });
  w.loadURL(url);
  w.on("page-title-updated", (e) => e.preventDefault());
  w.on("closed", () => appWindows.delete(url));
  appWindows.set(url, w);
}

// ---------------------------------------------------------------- tray + notifications

async function refreshTray() {
  if (!tray) makeTray();
  if (!tray || !auth) return;
  const [status, modes, rec] = await Promise.all([
    call(auth.base, "GET", "/serve", { token: auth.token }).catch(() => null),
    call(auth.base, "GET", "/modes", { token: auth.token }).catch(() => null),
    call(auth.base, "GET", "/record", { token: auth.token }).catch(() => null),
  ]);
  const remote = boxes.current !== "local";
  // On another computer, meetings are recorded here (its microphone), not on the device.
  const recording = remote ? localRecording : rec?.json?.recording;
  const serve = status?.json;
  const line = !serve
    ? "Can't reach the device"
    : serve.phase === "serving" ? `Serving ${serve.model ?? serve.served_id ?? "a model"}`
    : serve.phase === "starting" ? `Starting ${serve.model ?? "a model"}…`
    : "Nothing serving";
  tray.setToolTip(`TT Station · ${recording ? "Recording a meeting · " : ""}${line}`);
  const openTranscribe = () => { show(); win.webContents.send("navigate", "/studio#transcribe"); };
  const modeItems = (modes?.json?.modes ?? []).map((m) => ({
    label: m.title,
    type: "radio",
    checked: m.active,
    enabled: m.ready,
    click: () => call(auth.base, "POST", `/modes/${m.id}`, { token: auth.token, body: {} }),
  }));
  tray.setContextMenu(Menu.buildFromTemplate([
    { label: "Ask Tori…", accelerator: accelerator(toriBinding()), click: () => showTori("talk") },
    { type: "separator" },
    { label: line, enabled: false },
    { label: "Open TT Station", click: show },
    recording
      ? { label: "Stop recording and transcribe", click: async () => {
          if (remote) { const r = await clientRecord("stop"); if (!r.ok) notify({ title: "Couldn't stop recording", body: r.error }); }
          else await call(auth.base, "POST", "/record/stop", { token: auth.token, body: {} }).catch(() => null);
          refreshTray(); openTranscribe();
        } }
      : { label: "Record a meeting", click: async () => {
          const r = remote
            ? await clientRecord("start").then((x) => ({ json: { recording: x.ok } }), () => null)
            : await call(auth.base, "POST", "/record/start", { token: auth.token, body: {} }).catch(() => null);
          refreshTray();
          if (r?.json?.recording) notify({ title: "Recording", body: "Let everyone know you're recording. Stop from here or in Studio." }, openTranscribe);
          else openTranscribe();
        } },
    { type: "separator" },
    ...(boxes.boxes.length ? [{ label: "Device", submenu: boxList().boxes.map((b) => ({ label: b.name, type: "radio", checked: b.id === boxes.current, click: () => switchBox(b.id) })) }] : []),
    { label: "Mode", submenu: modeItems.length ? modeItems : [{ label: "Unavailable", enabled: false }] },
    { label: "Stop the model", enabled: serve?.phase === "serving", click: () => call(auth.base, "POST", "/stop", { token: auth.token, body: {} }) },
    { type: "separator" },
    { label: "Quit TT Station", click: () => { quitting = true; app.quit(); } },
  ]));
}

function makeTray() {
  const img = nativeImage.createFromPath(fs.existsSync(TRAY_ICON) ? TRAY_ICON : ICON).resize({ width: IS_MAC ? 18 : 22, height: IS_MAC ? 18 : 22 });
  tray = new Tray(img);
  // The top-bar icon is Tori's (where the desktop delivers clicks; GNOME shows the menu, Tori first).
  tray.on("click", () => showTori("talk"));
  tray.setToolTip("Tori");
}

/** Every paired box's news, as notifications (the box on screen shows its own toasts while focused). */
async function pollEvents() {
  const ids = ["local", ...boxes.boxes.map((b) => b.id)];
  await Promise.all(ids.map(async (id) => {
    let c;
    try { c = id === boxes.current && auth ? auth : await credsFor(id); } catch { return; }
    if (!c.token) return;
    const since = lastSeq.get(id) ?? 0;
    const res = await call(c.base, "GET", `/events?since=${since}`, { token: c.token }).catch(() => null);
    if (!res?.ok) return;
    lastSeq.set(id, res.json?.latest ?? since);
    if (since === 0) return; // don't replay history on launch
    const onScreen = id === boxes.current;
    const several = boxes.boxes.length > 0;
    for (const e of res.json?.events ?? []) {
      // The app shows its own toasts while it's in front.
      if (onScreen && win?.isFocused()) {
        win.webContents.send("box-event", e);
        continue;
      }
      // A bring-up waiting on you (or moving on) opens Bring-up when clicked.
      const route = e.kind === "bringup" || e.kind === "orchard" ? "/bringup" : e.kind === "research" ? "/research" : e.kind === "coding" ? "/code" : e.kind === "routine" ? "/routines" : null;
      notify({ title: several ? `${c.name}: ${e.title}` : e.title, body: e.body, silent: false }, () => {
        show();
        if (!onScreen) {
          switchBox(id);
          if (route) win?.webContents.once("did-finish-load", () => win?.webContents.send("navigate", route));
        } else if (route) {
          win?.webContents.send("navigate", route);
        }
      });
    }
  }));
}

// ---------------------------------------------------------------- start at login

const AUTOSTART = path.join(os.homedir(), ".config", "autostart", "tt-station.desktop");

function launchCommand() {
  return `${process.execPath} ${app.getAppPath()} --ozone-platform-hint=auto --hidden`;
}

function setStartAtLogin(on) {
  if (IS_MAC) {
    app.setLoginItemSettings({ openAtLogin: on, openAsHidden: true });
    return on;
  }
  if (on) {
    fs.mkdirSync(path.dirname(AUTOSTART), { recursive: true });
    fs.writeFileSync(AUTOSTART, [
      "[Desktop Entry]", "Type=Application", "Name=TT Station", `Exec=${launchCommand()}`,
      `Icon=${ICON}`, "X-GNOME-Autostart-enabled=true", "NoDisplay=true", "",
    ].join("\n"));
  } else {
    fs.rmSync(AUTOSTART, { force: true });
  }
  return on;
}

// ---------------------------------------------------------------- IPC
//
// Only the box's own pages and this app's may use the bridge: a page from
// anywhere else that found its way into a window gets nothing (the token above all).

function fromUs(event) {
  const url = event.senderFrame?.url;
  return !!url && (onBox(url) || ownPage(url));
}

function handle(channel, fn) {
  ipcMain.handle(channel, (event, ...args) => {
    if (!fromUs(event)) throw new Error("Not from TT Station.");
    return fn(event, ...args);
  });
}

handle("auth", async () => {
  if (!auth) auth = await signIn();
  return { base: auth.base, token: auth.token, name: auth.name };
});
handle("boxes", () => boxList());
handle("add-box", async (_e, link) => {
  try { const id = await addBox(link); return { ok: true, id }; } catch (e) { return { ok: false, error: String(e.message || e) }; }
});
handle("switch-box", (_e, id) => { switchBox(String(id)); });
/** Best effort: tell a box to forget this computer, so a removed box isn't left holding a live pairing. */
async function signOutOf(id) {
  const c = await credsFor(id).catch(() => null);
  if (!c?.token) return;
  const list = await call(c.base, "GET", "/devices", { token: c.token, timeout: 5000 }).catch(() => null);
  const me = list?.json?.devices?.find?.((d) => d.this_device);
  if (me) await call(c.base, "DELETE", `/devices/${encodeURIComponent(me.id)}`, { token: c.token, timeout: 5000 }).catch(() => null);
}

handle("remove-box", async (_e, id) => {
  await signOutOf(id);
  boxes.boxes = boxes.boxes.filter((b) => b.id !== id);
  saveBoxes();
  if (boxes.current === id) {
    if (readSecret()) switchBox("local");
    else if (boxes.boxes.length) switchBox(boxes.boxes[0].id);
    else { boxes.current = "local"; saveBoxes(); auth = null; showConnect(); }
  }
  return boxList();
});
handle("show-connect", () => showConnect());
// Where this computer keeps its box tokens, for Settings to say plainly.
handle("token-storage", () => {
  const t = tokenStorage();
  return { protected: t.protected, where: t.where, backend: t.backend };
});
// The box already removed this computer (Settings → Devices): forget its token
// and go back to the connect screen.
handle("signed-out", () => {
  const id = boxes.current;
  auth = null;
  if (overlay && !overlay.isDestroyed()) overlay.destroy();
  overlay = null;
  if (id === "local") {
    fs.rmSync(TOKEN_FILE, { force: true });
    localSignedOut = true;
  } else {
    boxes.boxes = boxes.boxes.filter((b) => b.id !== id);
    boxes.current = "local";
    saveBoxes();
  }
  lastSeq.delete(id);
  refreshTray();
  showConnect();
});
handle("discover", () => discover());
handle("probe", async (_e, host, tls) => {
  try { return { ok: true, ...(await probe(String(host), Number(tls) || 8766)) }; } catch (e) { return { ok: false, error: String(e.message || e) }; }
});
handle("pair-account", async (_e, p, user, password) => {
  try {
    const id = await pairWithAccount({ host: String(p.host), tls: Number(p.tls), fp: String(p.fp), name: String(p.name) }, String(user || "ttuser"), password ? String(password) : null);
    switchBox(id);
    return { ok: true, id };
  } catch (e) { return { ok: false, error: String(e.message || e), kind: e.kind || null }; }
});
handle("pair-start", async (_e, host, tls, fp) => {
  try { return { ok: true, pairId: await pairStart(String(host), Number(tls), String(fp)) }; } catch (e) { return { ok: false, error: String(e.message || e) }; }
});
handle("pair-finish", async (_e, p) => {
  try {
    const id = await pairFinish({ host: String(p.host), tls: Number(p.tls), fp: String(p.fp), name: String(p.name), pair: String(p.pairId), code: String(p.code).replace(/\D/g, "") });
    switchBox(id);
    return { ok: true, id };
  } catch (e) { return { ok: false, error: String(e.message || e) }; }
});
handle("ssh", async (_e, what) => {
  try { return { ok: true, ...(await sshAction(String(what))) }; } catch (e) { return { ok: false, error: String(e.message || e) }; }
});
handle("open-app-window", async (_e, url, title) => {
  if (typeof url !== "string" || !/^https?:\/\//.test(url)) return;
  try { openAppWindow(await reachable(url), String(title || "TT Station")); }
  catch (e) { notify({ title: "Couldn't open that app", body: String(e.message || e) }); }
});
handle("open-external", async (_e, url) => {
  if (typeof url !== "string" || !/^https?:\/\//.test(url)) return;
  try { await shell.openExternal(await reachable(url)); }
  catch (e) { notify({ title: "Couldn't open that", body: String(e.message || e) }); }
});
handle("computer", async (_e, tool, args) => {
  try { return { ok: true, ...(await computerTool(String(tool), args && typeof args === "object" ? args : {})) }; }
  catch (e) { return { ok: false, error: String(e.message || e) }; }
});
handle("start-at-login", (_e, on) => (typeof on === "boolean" ? setStartAtLogin(on) : IS_MAC ? app.getLoginItemSettings().openAtLogin : fs.existsSync(AUTOSTART)));
handle("notify", (_e, title, body) => {
  if (!win?.isFocused()) notify({ title: String(title), body: String(body ?? "") });
});
handle("hide-tori", () => overlay?.hide());
// tt-toplike's own native window, on this box's live telemetry.
handle("open-toplike", (_e, view) => {
  if (!auth) return;
  // tt-toplike's window is a Linux app on the box: elsewhere, the Hardware page.
  if (IS_MAC || boxes.current !== "local") { show(); win?.webContents.send("navigate", "/hardware"); return; }
  const modes = { arcade: "arcade", starfield: "starfield", castle: "castle", flow: "flow", defrag: "arcade", inference: "normal", insights: "normal", hivemind: "hivemind" };
  const mode = modes[view] ?? "arcade";
  const port = new URL(auth.base).port || "8765";
  const { spawn } = require("node:child_process");
  const child = spawn("tt-toplike-app", ["--", "--remote", `127.0.0.1:${port}`, "--mode", mode], { detached: true, stdio: "ignore" });
  child.on("error", () => notify({ title: "tt-toplike isn't installed", body: "Install tt-toplike-app to open it in its own window." }));
  child.unref();
});
handle("show-tori", (_e, mode) => showTori(mode === "type" ? "type" : "talk"));
handle("show-main", (_e, route) => {
  show();
  if (typeof route === "string") win?.webContents.send("navigate", route);
});
handle("shortcuts", (_e, on) => (typeof on === "boolean" ? setShortcuts(on) : shortcutsInstalled()));
handle("tori-shortcut", (_e, binding) => {
  if (typeof binding === "string") {
    if (!/^(<(Control|Alt|Shift|Super|Primary)>)*[A-Za-z0-9_]+$/.test(binding) || binding.length > 60) throw new Error("That isn't a shortcut.");
    if (!/<(Control|Alt|Super|Primary)>/.test(binding) && !/^F\d{1,2}$/.test(binding.replace(/<\w+>/g, "")))
      throw new Error("Use Ctrl, Alt or Super with it, so it doesn't get in the way of typing.");
    const old = toriBinding();
    savePrefs({ toriShortcut: binding });
    try { globalShortcut.unregister(accelerator(old)); } catch { /* wasn't registered */ }
    try { globalShortcut.register(accelerator(binding), () => showTori("talk")); } catch { /* Wayland */ }
    setShortcuts(true);
  }
  return { binding: toriBinding(), installed: shortcutsInstalled(), dropdown: false };
});

// ---------------------------------------------------------------- lifecycle

/** The Mac's menu bar: the usual menus, and a Box menu (Terminal, switching, pairing). */
function buildMenu() {
  if (!IS_MAC) return;
  const list = boxList();
  const sshItem = (label, what, accelerator) => ({ label, accelerator, enabled: list.current !== "local" && list.boxes.length > 0, click: async () => {
    const r = await sshAction(what).then((x) => ({ ok: true, ...x }), (e) => ({ ok: false, error: e.message }));
    if (!r.ok) notify({ title: "Couldn't open the device's terminal", body: r.error });
  } });
  Menu.setApplicationMenu(Menu.buildFromTemplate([
    { role: "appMenu" },
    { role: "editMenu" },
    { label: "Device", submenu: [
      sshItem("Open Terminal on Device", "terminal", "CmdOrCtrl+Shift+T"),
      { label: "Command Line in TT Station", click: () => { show(); win?.webContents.send("navigate", "/terminals"); } },
      sshItem("Open Device in VS Code", "vscode"),
      { type: "separator" },
      ...list.boxes.map((b) => ({ label: b.name, type: "radio", checked: b.id === list.current, click: () => switchBox(b.id) })),
      { type: "separator" },
      { label: "Add Another Device…", click: showConnect },
    ] },
    { role: "viewMenu" },
    { role: "windowMenu" },
  ]));
}

app.whenReady().then(() => {
  boxes = readBoxes();
  trustPinnedBoxes();
  buildMenu();
  app.on("activate", show);
  createWindow();
  if (process.argv.includes("--hidden")) win.once("ready-to-show", () => win.hide());
  if (process.argv.includes("--chat")) win.webContents.once("did-finish-load", () => win.webContents.send("navigate", "/chat"));
  makeTray();

  refreshTray();
  setInterval(refreshTray, 10000);
  setInterval(pollEvents, 5000);
  // These work on X11; on Wayland, GNOME runs them for us (Settings → Shortcuts).
  try { globalShortcut.register(accelerator(toriBinding()), () => showTori("talk")); } catch { /* bad binding */ }
  listenForSummons();
  // Keep the GNOME shortcut pointing at the fast path if it's installed.
  try { if (shortcutsInstalled()) setShortcuts(true); } catch { /* not GNOME */ }
  if (process.argv.includes("--tori") || process.argv.includes("--talk")) {
    win.webContents.once("did-finish-load", () => showTori("talk"));
    win.once("ready-to-show", () => win.hide());
  }
});

app.on("before-quit", () => { quitting = true; });
app.on("will-quit", () => globalShortcut.unregisterAll());
app.on("window-all-closed", () => { /* stays in the tray */ });
