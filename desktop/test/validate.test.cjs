// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The desktop app's input checks (electron/validate.cjs). Run: node --test test/*.test.cjs
const test = require("node:test");
const assert = require("node:assert");
const { execFileSync } = require("node:child_process");
const { validHost, validUser, cleanName, sameOrigin, webAddress, shq } = require("../electron/validate.cjs");

test("device addresses: hostnames and IPs only", () => {
  for (const h of ["quietbox", "quietbox.local", "192.168.1.20", "fe80::1", "::1", "2001:db8::2", "my-box-2.lan"]) assert.ok(validHost(h), h);
  for (const h of ["", "-oProxyCommand=x", "a b", "box\nProxyCommand evil", "box;rm", "box$(id)", "a..b", "x".repeat(254), "[::1]", "box/evil", "box\u0000", null]) assert.ok(!validHost(h), JSON.stringify(h));
});

test("ssh accounts can't be options or paths", () => {
  for (const u of ["ttuser", "j.berk", "_svc", "a-b"]) assert.ok(validUser(u), u);
  for (const u of ["-oProxyCommand=x", "a b", "root@x", "", "x\ny", "../x", undefined]) assert.ok(!validUser(u), JSON.stringify(u));
});

test("device names: no control characters or line breaks", () => {
  assert.equal(cleanName(" Jane's Box "), "Jane's Box");
  for (const n of ["a\nb", "a\rb", "a\u0007", "x".repeat(81), "", "a b", 7]) assert.equal(cleanName(n), null, JSON.stringify(n));
});

test("the box's origin is matched exactly, not as a prefix", () => {
  const base = "http://127.0.0.1:8765";
  assert.ok(sameOrigin("http://127.0.0.1:8765/app/chat", base));
  for (const u of ["http://127.0.0.1:8765@evil.example/", "http://127.0.0.1:87650/", "http://127.0.0.1:8765.evil.example/", "https://127.0.0.1:8765/", "javascript:alert(1)", "not a url"]) assert.ok(!sameOrigin(u, base), u);
});

test("only web addresses open outside", () => {
  for (const u of ["https://example.com", "http://x"]) assert.ok(webAddress(u), u);
  for (const u of ["file:///etc/passwd", "javascript:alert(1)", "smb://x/y", "ms-settings:", "httpx://x", ""]) assert.ok(!webAddress(u), u);
});

test("shq quotes anything for bash", () => {
  for (const evil of [`"; touch /tmp/x; "`, "$(id)", "`id`", "it's", "a\\b", "TT Station → box\nrm -rf ~"]) {
    assert.equal(execFileSync("bash", ["-c", `printf %s ${shq(evil)}`], { encoding: "utf8" }), evil);
  }
});
