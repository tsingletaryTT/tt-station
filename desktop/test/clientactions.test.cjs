// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Asking before a box opens links or apps on this computer (electron/clientactions.cjs).
// Run: node --test test/*.test.cjs
const test = require("node:test");
const assert = require("node:assert");
const ca = require("../electron/clientactions.cjs");

const box = { name: "QuietBox", host: "192.168.1.20" };

test("only opening links and apps is asked about", () => {
  for (const tool of ["control_computer", "open_folder", "find_files", "open_station", "start_recording"]) assert.equal(ca.describe(tool, {}), null, tool);
});

test("a web link: the full address opens, its site is what 'always' remembers", () => {
  const a = ca.describe("open_app", { url: " https://Example.com/path?q=1#x " });
  assert.deepEqual(a, { kind: "site", target: "https://example.com/path?q=1#x", key: "https://example.com", label: "example.com" });
  const port = ca.describe("open_app", { url: "http://example.com:8080/" });
  assert.equal(port.key, "http://example.com:8080");
  assert.notEqual(port.key, a.key, "another scheme or port is another site");
});

test("a look-alike name shows as what it really is", () => {
  const a = ca.describe("open_app", { url: "https://аpple.com/" }); // Cyrillic а
  assert.equal(a.label, "xn--pple-43d.com");
});

test("mail links are remembered by address", () => {
  const a = ca.describe("open_app", { url: "mailto:Jane@Example.com?subject=hi" });
  assert.equal(a.key, "mailto:jane@example.com");
  assert.equal(a.target, "mailto:Jane@Example.com?subject=hi");
  assert.match(a.label, /email to jane@example.com/);
});

test("links that may never open throw instead of asking", () => {
  for (const url of ["file:///etc/passwd", "javascript:alert(1)", "ssh://box", "vscode://x", "not a url", "https://user:pw@example.com/", `https://example.com/${"a".repeat(2001)}`, "https://example.com/‮gnp.exe", "https://exa​mple.com/", "mailto:"]) {
    assert.throws(() => ca.describe("open_app", { url }), undefined, url);
  }
});

test("apps: by name, case and .app don't matter to 'always'", () => {
  const a = ca.describe("open_app", { app: " Safari.app " });
  assert.deepEqual(a, { kind: "app", target: "Safari.app", key: "safari", label: "Safari.app" });
  assert.equal(ca.describe("open_app", { app: "safari" }).key, a.key);
  for (const app of ["", "   ", "x".repeat(101), "Term\ninal", "Safari‮"]) assert.throws(() => ca.describe("open_app", { app }), undefined, JSON.stringify(app));
});

test("the prompt says exactly what will open and which box asked", () => {
  const site = ca.describe("open_app", { url: "https://example.com/a/b?c=d" });
  const d = ca.dialogFor(site, box);
  assert.match(d.message, /^QuietBox \(192\.168\.1\.20\) wants to open a page on example\.com/);
  assert.match(d.detail, /https:\/\/example\.com\/a\/b\?c=d/);
  assert.deepEqual(d.buttons, ["Allow once", "Always allow example.com from QuietBox", "Don't allow"]);
  assert.equal(d.defaultId, 2, "Enter doesn't allow");
  assert.equal(d.cancelId, 2, "Esc / closing doesn't allow");

  const app = ca.dialogFor(ca.describe("open_app", { app: "Terminal" }), box);
  assert.match(app.message, /open the app “Terminal”/);
  assert.deepEqual(app.buttons, ["Allow once", "Always allow Terminal from QuietBox", "Don't allow"]);

  assert.match(ca.dialogFor(site, { name: "lab", host: null }).message, /^lab wants/);
});

test("answers: once, always, deny (and anything else is deny)", () => {
  assert.equal(ca.choice(0), "once");
  assert.equal(ca.choice(1), "always");
  assert.equal(ca.choice(2), "deny");
  assert.equal(ca.choice(undefined), "deny");
  assert.equal(ca.choice(-1), "deny");
});

test("'always' is per box and per site/app", () => {
  const site = ca.describe("open_app", { url: "https://example.com/x" });
  const sameSite = ca.describe("open_app", { url: "https://example.com/other/page" });
  const otherSite = ca.describe("open_app", { url: "https://evil.example/" });
  const app = ca.describe("open_app", { app: "Terminal" });
  const before = { toriShortcut: "<Control>space" };
  const p = ca.remember(before, "abc123", "QuietBox", site);
  assert.deepEqual(before, { toriShortcut: "<Control>space" }, "input untouched");
  assert.equal(p.toriShortcut, "<Control>space", "other prefs kept");
  assert.ok(ca.isAllowed(p, "abc123", site));
  assert.ok(ca.isAllowed(p, "abc123", sameSite), "same site, another page");
  assert.ok(!ca.isAllowed(p, "abc123", otherSite));
  assert.ok(!ca.isAllowed(p, "other-box", site), "another box still asks");
  assert.ok(!ca.isAllowed(p, "abc123", app), "a site isn't an app");
  assert.ok(!ca.isAllowed({}, "abc123", site));
  assert.ok(!ca.isAllowed(p, "", site));
  const twice = ca.remember(p, "abc123", "QuietBox", site);
  assert.deepEqual(twice.allowedActions.abc123.sites, ["https://example.com"], "no duplicates");
});

test("an 'app' entry can't be matched by a site with the same text", () => {
  const p = ca.remember({}, "b", "B", ca.describe("open_app", { app: "https://example.com" }));
  assert.ok(!ca.isAllowed(p, "b", ca.describe("open_app", { url: "https://example.com" })));
});

test("remembered choices can be listed and cleared: one, one box, or all", () => {
  const site = ca.describe("open_app", { url: "https://example.com/" });
  const app = ca.describe("open_app", { app: "Terminal" });
  let p = ca.remember({}, "a", "Box A", site);
  p = ca.remember(p, "a", "Box A", app);
  p = ca.remember(p, "b", "Box B", site);
  assert.deepEqual(ca.listAllowed(p), [
    { boxId: "a", name: "Box A", sites: ["https://example.com"], apps: ["terminal"] },
    { boxId: "b", name: "Box B", sites: ["https://example.com"], apps: [] },
  ]);
  const oneGone = ca.forget(p, "a", { kind: "app", key: "terminal" });
  assert.ok(!ca.isAllowed(oneGone, "a", app));
  assert.ok(ca.isAllowed(oneGone, "a", site));
  const boxGone = ca.forget(p, "a");
  assert.ok(!ca.isAllowed(boxGone, "a", site));
  assert.ok(ca.isAllowed(boxGone, "b", site));
  const lastOfBox = ca.forget(ca.forget(p, "a", { kind: "app", key: "terminal" }), "a", { kind: "site", key: "https://example.com" });
  assert.equal(lastOfBox.allowedActions.a, undefined, "an emptied box is dropped");
  const allGone = ca.forget(p);
  assert.deepEqual(ca.listAllowed(allGone), []);
});

test("hand-edited or broken prefs never allow anything", () => {
  const site = ca.describe("open_app", { url: "https://example.com/" });
  for (const p of [null, undefined, { allowedActions: null }, { allowedActions: { a: "x" } }, { allowedActions: { a: { sites: "https://example.com" } } }, { allowedActions: { a: { sites: [42] } } }]) {
    assert.ok(!ca.isAllowed(p, "a", site), JSON.stringify(p));
  }
});

test("open_folder opens folders only, never a file or an app", () => {
  const dir = { isDirectory: () => true };
  const file = { isDirectory: () => false };
  assert.equal(ca.folderProblem("/Users/jane/Documents", dir), null);
  assert.match(ca.folderProblem("/Applications/Calculator.app", dir), /is an app/);
  assert.match(ca.folderProblem("/Applications/Calculator.APP/", dir), /is an app/);
  assert.match(ca.folderProblem("/Users/jane/run.command", file), /isn't a folder/);
  assert.match(ca.folderProblem("/nope", null), /There's no folder/);
});
