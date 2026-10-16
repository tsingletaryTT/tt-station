// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, describe, expect, it, vi } from "vitest";
import { answer, badgeText, bellLabel, gather, newestFirst, PAIR_TTL_S, type BoxApproval, type BringupRunLite, type BoxEvent } from "./inbox";

const NOW = 1_800_000_000;
const code: BoxApproval = { id: "code:s1:r1", kind: "code", where: "~/src/app", summary: "npm test", session: "s1", can_always: true };
const routine: BoxApproval = { id: "routine:9-ab:0", kind: "routine", where: "Morning check", summary: "Remember: x", run: "9-ab", since: NOW - 50, queued: true };
const run: BringupRunLite = {
  id: "c1", engine: "codex", model: "Qwen/Qwen3-8B",
  approvals: [{ id: "a1", kind: "command", why: "Build tt-metal", command: "make -j", cwd: "/opt/tt", created: NOW - 100 }],
};
const pairing = (at: number, seq = 7): BoxEvent => ({ seq, at, kind: "pairing", title: "Pairing code for Jane's MacBook: 123 456", body: "Type it on that device." });

describe("Notifications", () => {
  it("lists Code and routine approvals for everyone, each opening where it came from", () => {
    const items = gather({ approvals: [code, routine], bringup: [run], events: [pairing(NOW - 10)], owner: false, now: NOW });
    expect(items.map((i) => i.kind)).toEqual(["routine", "code"]);
    const c = items.find((i) => i.kind === "code")!;
    expect(c.open).toEqual({ route: "code", hash: "s1" });
    expect(c.canAlways).toBe(true);
    expect(c.where).toBe("Code · ~/src/app");
    const r = items.find((i) => i.kind === "routine")!;
    expect(r.open).toEqual({ route: "routines", hash: "run-9-ab" });
    expect(r.detail).toMatch(/read the web/);
  });

  it("gives owners bring-up's commands and pairing requests too, pairing first", () => {
    const items = gather({ approvals: [code], bringup: [run], events: [pairing(NOW - 10)], owner: true, now: NOW });
    expect(items.map((i) => i.kind)).toEqual(["pairing", "bringup", "code"]);
    const b = items[1];
    expect(b.summary).toBe("make -j");
    expect(b.where).toBe("Bring-up · Codex · Qwen3-8B");
    expect(b.detail).toContain("In /opt/tt");
    expect(b.open).toEqual({ route: "bringup", hash: "agent" });
    const p = items[0];
    expect(p.answer).toBe("open");
    expect(p.open).toEqual({ route: "settings", hash: "devices" });
    expect(p.expires).toBe(NOW - 10 + PAIR_TTL_S);
  });

  it("drops pairing codes that have run out, other events, and what the person ignored", () => {
    const events = [pairing(NOW - PAIR_TTL_S, 1), pairing(NOW - 5, 2), { seq: 3, at: NOW, kind: "model_ready", title: "Ready", body: "" }];
    expect(gather({ approvals: [], bringup: [], events, owner: true, now: NOW }).map((i) => i.key)).toEqual(["pairing:2"]);
    expect(gather({ approvals: [], bringup: [], events, owner: true, now: NOW, ignored: ["pairing:2"] })).toEqual([]);
  });

  it("keeps an undated item in place across polls", () => {
    const seen = new Map<string, number>();
    const first = gather({ approvals: [code], bringup: null, events: null, owner: false, now: NOW, seen });
    const later = gather({ approvals: [code, { ...routine, since: NOW + 5 }], bringup: null, events: null, owner: false, now: NOW + 60, seen });
    expect(first[0].since).toBe(NOW);
    expect(later.map((i) => i.kind)).toEqual(["code", "routine"]);
  });

  it("is empty while nothing has loaded", () => {
    expect(gather({ approvals: null, bringup: undefined, events: null, owner: true, now: NOW })).toEqual([]);
  });
});

describe("answering", () => {
  afterEach(() => vi.unstubAllGlobals());
  const calls: { url: string; body: unknown }[] = [];
  const stub = () => {
    calls.length = 0;
    vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
      calls.push({ url, body: JSON.parse(String(init.body)) });
      return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
    }));
  };

  it("answers Code and routines through /approvals/answer", async () => {
    stub();
    const [item] = gather({ approvals: [code], bringup: null, events: null, owner: false, now: NOW });
    await answer(item, "always");
    await answer(item, "deny");
    expect(calls).toEqual([
      { url: "/approvals/answer", body: { id: "code:s1:r1", allow: true, always: true } },
      { url: "/approvals/answer", body: { id: "code:s1:r1", allow: false, always: false } },
    ]);
  });

  it("answers bring-up through its own endpoint, in its own words", async () => {
    stub();
    const [item] = gather({ approvals: [], bringup: [run], events: null, owner: true, now: NOW });
    await answer(item, "allow");
    await answer(item, "always");
    await answer(item, "deny");
    expect(calls.map((c) => [c.url, (c.body as { decision: string }).decision])).toEqual([
      ["/bringup/codex/runs/c1/approvals/a1", "accept"],
      ["/bringup/codex/runs/c1/approvals/a1", "acceptForSession"],
      ["/bringup/codex/runs/c1/approvals/a1", "decline"],
    ]);
  });
});

describe("the bell", () => {
  it("orders newest first", () => {
    const items = gather({ approvals: [code, routine], bringup: [run], events: [], owner: true, now: NOW });
    expect(newestFirst(items).map((i) => i.kind)).toEqual(["code", "routine", "bringup"]);
  });

  it("shows no count at zero and 9+ past nine", () => {
    expect([0, 1, 9, 10, 250].map(badgeText)).toEqual([null, "1", "9", "9+", "9+"]);
    expect(bellLabel(3)).toBe("Notifications, 3 waiting");
    expect(bellLabel(0)).toBe("Notifications, nothing waiting");
  });
});
