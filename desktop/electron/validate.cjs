// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// What a pairing link, the network or a device may hand this app before it
// goes into a shell script, ~/.ssh/config or an ssh command line. Pure (no
// Electron), so the app's tests can load it on their own.

const { isIP } = require("node:net");

/** A hostname, an IPv4 or an IPv6 address: nothing a shell or ssh would read as more. */
function validHost(host) {
  if (typeof host !== "string" || !host || host.length > 253) return false;
  if (isIP(host)) return true;
  // RFC 1123 labels: letters, digits and hyphens, not starting or ending with a hyphen.
  return host.split(".").every((label) => /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(label));
}

/** A login name ssh can take as `user@host` (never an option, never a path). */
function validUser(user) {
  return typeof user === "string" && /^[A-Za-z_][A-Za-z0-9_.-]{0,31}$/.test(user);
}

/** A device's name, to show and to put in quotes: no control characters or line breaks. */
function cleanName(name) {
  if (typeof name !== "string") return null;
  const n = name.trim();
  // eslint-disable-next-line no-control-regex
  if (!n || n.length > 80 || /[\u0000-\u001f\u007f-\u009f\u2028\u2029]/.test(n)) return null;
  return n;
}

/** `url` is on exactly `base`'s origin (scheme, host and port), not just a prefix of it. */
function sameOrigin(url, base) {
  try { return new URL(url).origin === new URL(base).origin; } catch { return false; }
}

/** A web address the system browser may open: http or https, nothing else. */
function webAddress(url) {
  try { return ["http:", "https:"].includes(new URL(url).protocol); } catch { return false; }
}

const HOST_KEY_TYPES = new Set([
  "ssh-ed25519", "ssh-rsa", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521",
  "sk-ssh-ed25519@openssh.com", "sk-ecdsa-sha2-nistp256@openssh.com",
]);

/** A box's SSH host public key ("ssh-ed25519 AAAA…", a comment dropped) as "<type> <base64>", else null. */
function hostKey(key) {
  if (typeof key !== "string" || key.length > 4096) return null;
  const [type, blob] = key.trim().split(/\s+/);
  if (!HOST_KEY_TYPES.has(type) || !/^[A-Za-z0-9+/]{16,}={0,3}$/.test(blob || "")) return null;
  return `${type} ${blob}`;
}

/** known_hosts lines for `host`'s keys (ssh looks the name up lowercased; `[host]:port` off port 22). */
function knownHostsLines(host, keys, port = 22) {
  const name = Number(port) && Number(port) !== 22 ? `[${host.toLowerCase()}]:${Number(port)}` : host.toLowerCase();
  return (keys || []).map(hostKey).filter(Boolean).map((k) => `${name} ${k}`);
}

/** A home folder the box reported, safe in a vscode:// link: an absolute path of plain characters. */
function homeFolder(dir) {
  return typeof dir === "string" && dir.length < 256 && /^\/[A-Za-z0-9._@+/-]*$/.test(dir) && !dir.split("/").includes("..") ? dir.replace(/\/+$/, "") || null : null;
}

/** One argument for bash, in single quotes. */
const shq = (a) => `'${String(a).replace(/'/g, "'\\''")}'`;

module.exports = { validHost, validUser, cleanName, sameOrigin, webAddress, shq, hostKey, knownHostsLines, homeFolder };
