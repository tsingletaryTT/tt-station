// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Keeping box tokens in the operating system's keychain (Electron's
// safeStorage): the macOS Keychain, Windows' DPAPI, or the login keyring on
// Linux (GNOME Keyring, KWallet). A sealed token is stored as
// `ttsafe1:<base64 ciphertext>`; anything else is a plain token from before
// this, or from a computer with no keychain.
//
// Linux without a keyring reports the `basic_text` backend, which "encrypts"
// with a key built into Chromium: no protection at all. There we don't
// pretend: the token stays plain in a file only this account can read, and
// the app says so.
//
// Pure apart from writePrivate (no Electron: safeStorage is passed in), so the
// app's tests can load it on their own.

const fs = require("node:fs");
const path = require("node:path");

const PREFIX = "ttsafe1:";

const BACKENDS = {
  gnome_libsecret: "your login keyring (GNOME Keyring)",
  kwallet: "KWallet",
  kwallet5: "KWallet",
  kwallet6: "KWallet",
};

/** Whether tokens can be sealed here, and in what: { protected, backend, where }. */
function storageInfo(safeStorage, platform) {
  let available = false;
  try { available = !!safeStorage?.isEncryptionAvailable?.(); } catch { /* no keychain */ }
  if (platform === "linux") {
    let backend = "unknown";
    try { backend = safeStorage?.getSelectedStorageBackend?.() ?? "unknown"; } catch { /* older Electron */ }
    const where = BACKENDS[backend] ?? null;
    return { protected: available && !!where, backend, where };
  }
  const where = platform === "darwin" ? "the macOS Keychain" : platform === "win32" ? "Windows' data protection (DPAPI)" : null;
  return { protected: available && !!where, backend: platform, where };
}

const isSealed = (stored) => typeof stored === "string" && stored.startsWith(PREFIX);

/**
 * A token as it should be written: sealed when the keychain is real, plain
 * otherwise. Sealing is checked by unsealing before it's trusted, so a
 * keychain that misbehaves can't turn a working token into an unreadable one.
 */
function seal(token, safeStorage, info) {
  if (!token || !info.protected) return token;
  try {
    const sealed = safeStorage.encryptString(token);
    if (safeStorage.decryptString(sealed) !== token) return token;
    return PREFIX + Buffer.from(sealed).toString("base64");
  } catch {
    return token;
  }
}

/** The token in a stored value, or null if it's sealed and the keychain won't open it. */
function unseal(stored, safeStorage) {
  if (typeof stored !== "string" || !stored) return null;
  if (!isSealed(stored)) return stored;
  try {
    return safeStorage.decryptString(Buffer.from(stored.slice(PREFIX.length), "base64")) || null;
  } catch {
    return null;
  }
}

/**
 * desktop-boxes.json, read: tokens unsealed in memory. A box whose sealed
 * token won't open keeps it (`sealedToken`), so a keychain that's locked this
 * once doesn't cost the pairing for good. `migrate` says plain tokens are on
 * disk that could now be sealed.
 */
function loadBoxes(text, safeStorage, info) {
  let raw;
  try { raw = JSON.parse(text); } catch { return { current: "local", boxes: [], migrate: false }; }
  const list = Array.isArray(raw?.boxes) ? raw.boxes : [];
  let migrate = false;
  const boxes = list.map((b) => {
    const { token: stored, ...rest } = b || {};
    if (typeof stored === "string" && stored && !isSealed(stored) && info.protected) migrate = true;
    const token = unseal(stored, safeStorage);
    return (token || !isSealed(stored)) ? { ...rest, token } : { ...rest, token: null, sealedToken: stored };
  });
  return { current: typeof raw?.current === "string" ? raw.current : "local", boxes, migrate };
}

/** desktop-boxes.json, to write: tokens sealed where the keychain is real. */
function dumpBoxes(state, safeStorage, info) {
  return JSON.stringify({
    current: state.current,
    // What protects the tokens below: "keychain", or "file" (owner-only file, no keychain).
    tokenStorage: info.protected ? "keychain" : "file",
    boxes: state.boxes.map(({ token, sealedToken, ...rest }) => ({ ...rest, token: token ? seal(token, safeStorage, info) : sealedToken ?? null })),
  }, null, 2);
}

/** The desktop token file, read: { token, migrate }. */
function loadToken(text, safeStorage, info) {
  const stored = String(text ?? "").trim();
  if (!stored) return { token: null, migrate: false };
  return { token: unseal(stored, safeStorage), migrate: !isSealed(stored) && info.protected };
}

/**
 * Write `contents` readable by this account only, all at once: a fresh 0600
 * file beside it, then a rename, so a crash leaves the old file or the new
 * one and never half of either (and never a looser mode an older file had).
 */
function writePrivate(file, contents) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  try {
    fs.rmSync(tmp, { force: true });
    fs.writeFileSync(tmp, contents, { mode: 0o600, flag: "wx" });
    fs.renameSync(tmp, file);
  } catch (e) {
    fs.rmSync(tmp, { force: true });
    throw e;
  }
}

module.exports = { PREFIX, storageInfo, isSealed, seal, unseal, loadBoxes, dumpBoxes, loadToken, writePrivate };
