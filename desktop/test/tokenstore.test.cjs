// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Box tokens in the OS keychain (electron/tokenstore.cjs), with a fake
// safeStorage. Run: node --test test/*.test.cjs
const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const ts = require("../electron/tokenstore.cjs");

/** A stand-in for Electron's safeStorage: XOR "encryption" under a named backend. */
function fakeSafeStorage({ backend = "gnome_libsecret", available = true, broken = false } = {}) {
  const flip = (buf) => Buffer.from(buf.map((b) => b ^ 0x5a));
  return {
    isEncryptionAvailable: () => available,
    getSelectedStorageBackend: () => backend,
    encryptString: (s) => flip(Buffer.from(s, "utf8")),
    decryptString: (buf) => {
      if (broken) throw new Error("keychain locked");
      return flip(Buffer.from(buf)).toString("utf8");
    },
  };
}

const TOKEN = "a".repeat(64);

test("a real keychain is used; basic_text and no keychain are reported honestly", () => {
  assert.deepEqual(ts.storageInfo(fakeSafeStorage(), "linux"), { protected: true, backend: "gnome_libsecret", where: "your login keyring (GNOME Keyring)" });
  assert.equal(ts.storageInfo(fakeSafeStorage({ backend: "kwallet6" }), "linux").protected, true);
  for (const backend of ["basic_text", "unknown"]) assert.equal(ts.storageInfo(fakeSafeStorage({ backend }), "linux").protected, false, backend);
  assert.equal(ts.storageInfo(fakeSafeStorage({ available: false }), "darwin").protected, false);
  assert.equal(ts.storageInfo(fakeSafeStorage(), "darwin").where, "the macOS Keychain");
  assert.equal(ts.storageInfo(undefined, "linux").protected, false);
});

test("sealed tokens round-trip and never contain the token", () => {
  const ss = fakeSafeStorage();
  const info = ts.storageInfo(ss, "linux");
  const sealed = ts.seal(TOKEN, ss, info);
  assert.ok(ts.isSealed(sealed));
  assert.ok(!sealed.includes(TOKEN));
  assert.equal(ts.unseal(sealed, ss), TOKEN);
});

test("without a real keychain the token stays plain (no pretend encryption)", () => {
  const ss = fakeSafeStorage({ backend: "basic_text" });
  assert.equal(ts.seal(TOKEN, ss, ts.storageInfo(ss, "linux")), TOKEN);
});

test("a keychain that won't round-trip leaves the token plain rather than unreadable", () => {
  const ss = fakeSafeStorage({ broken: true });
  assert.equal(ts.seal(TOKEN, ss, ts.storageInfo(ss, "linux")), TOKEN);
});

test("plain box tokens migrate to sealed ones, and nothing is lost", () => {
  const ss = fakeSafeStorage();
  const info = ts.storageInfo(ss, "linux");
  const old = JSON.stringify({ current: "abc", boxes: [{ id: "abc", name: "Lab", host: "10.0.0.2", tls: 8766, fp: "ff", token: TOKEN }, { id: "def", name: "No token yet", host: "h", tls: 1, fp: "ee" }] });
  const loaded = ts.loadBoxes(old, ss, info);
  assert.equal(loaded.migrate, true);
  assert.equal(loaded.current, "abc");
  assert.equal(loaded.boxes[0].token, TOKEN);
  assert.equal(loaded.boxes[1].token, null);

  const written = ts.dumpBoxes(loaded, ss, info);
  assert.ok(!written.includes(TOKEN), "the plain token is gone from the file");
  assert.equal(JSON.parse(written).tokenStorage, "keychain");
  const again = ts.loadBoxes(written, ss, info);
  assert.equal(again.migrate, false);
  assert.equal(again.boxes[0].token, TOKEN, "the pairing still works after migration");
  assert.equal(again.boxes[0].name, "Lab");
});

test("without a keychain, box tokens stay plain and the file says so", () => {
  const ss = fakeSafeStorage({ backend: "basic_text" });
  const info = ts.storageInfo(ss, "linux");
  const loaded = ts.loadBoxes(JSON.stringify({ current: "local", boxes: [{ id: "x", token: TOKEN }] }), ss, info);
  assert.equal(loaded.migrate, false);
  const written = JSON.parse(ts.dumpBoxes(loaded, ss, info));
  assert.equal(written.tokenStorage, "file");
  assert.equal(written.boxes[0].token, TOKEN);
});

test("a sealed token the keychain won't open is kept, not thrown away", () => {
  const good = fakeSafeStorage();
  const info = ts.storageInfo(good, "linux");
  const written = ts.dumpBoxes({ current: "x", boxes: [{ id: "x", token: TOKEN }] }, good, info);
  const locked = fakeSafeStorage({ broken: true });
  const loaded = ts.loadBoxes(written, locked, info);
  assert.equal(loaded.boxes[0].token, null);
  // Saving while locked writes the sealed value back unchanged...
  const rewritten = ts.dumpBoxes(loaded, locked, ts.storageInfo(locked, "linux"));
  // ...so it opens again once the keychain does.
  assert.equal(ts.loadBoxes(rewritten, good, info).boxes[0].token, TOKEN);
});

test("the desktop token file: plain migrates, sealed opens", () => {
  const ss = fakeSafeStorage();
  const info = ts.storageInfo(ss, "linux");
  assert.deepEqual(ts.loadToken(`${TOKEN}\n`, ss, info), { token: TOKEN, migrate: true });
  assert.deepEqual(ts.loadToken(ts.seal(TOKEN, ss, info), ss, info), { token: TOKEN, migrate: false });
  assert.deepEqual(ts.loadToken("", ss, info), { token: null, migrate: false });
  const plainInfo = ts.storageInfo(fakeSafeStorage({ backend: "basic_text" }), "linux");
  assert.deepEqual(ts.loadToken(TOKEN, ss, plainInfo), { token: TOKEN, migrate: false });
});

test("corrupt box files read as no boxes", () => {
  assert.deepEqual(ts.loadBoxes("{{", fakeSafeStorage(), { protected: true }), { current: "local", boxes: [], migrate: false });
});

test("writePrivate replaces a file atomically, owner-only, even over a looser one", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tt-tokenstore-"));
  const file = path.join(dir, "sub", "desktop-token");
  fs.mkdirSync(path.dirname(file));
  fs.writeFileSync(file, "old", { mode: 0o644 });
  ts.writePrivate(file, "new");
  assert.equal(fs.readFileSync(file, "utf8"), "new");
  if (process.platform !== "win32") assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ["desktop-token"], "no temp file left behind");
  fs.rmSync(dir, { recursive: true, force: true });
});
