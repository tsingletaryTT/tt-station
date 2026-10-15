// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { frameBatcher, reuse, same } from "./render";

describe("frameBatcher", () => {
  it("flushes once per frame however many tokens arrive", () => {
    const frames: (() => void)[] = [];
    let flushes = 0;
    const b = frameBatcher(() => { flushes++; }, (f) => frames.push(f), () => {});
    for (let i = 0; i < 500; i++) b.poke();
    expect(frames.length).toBe(1);
    frames.shift()!();
    expect(flushes).toBe(1);
    b.poke();
    expect(frames.length).toBe(1);
  });

  it("flushes at once on demand and cancels the frame it had asked for", () => {
    const cancelled: number[] = [];
    let flushes = 0;
    const b = frameBatcher(() => { flushes++; }, () => 7, (h) => cancelled.push(h));
    b.poke();
    b.now();
    expect(flushes).toBe(1);
    expect(cancelled).toEqual([7]);
    b.now();
    expect(flushes).toBe(2);
    expect(cancelled).toEqual([7]);
  });
});

describe("reuse", () => {
  it("keeps the objects of unchanged entries and takes the changed ones", () => {
    const prev = [
      { key: "a", text: "hi", tools: [{ id: 1, result: { output: "x" } }] },
      { key: "b", text: "still going", tools: [] },
    ];
    const next = [
      { key: "a", text: "hi", tools: [{ id: 1, result: { output: "x" } }] },
      { key: "b", text: "still going, more", tools: [] },
      { key: "c", text: "new", tools: [] },
    ];
    const out = reuse(prev, next);
    expect(out[0]).toBe(prev[0]);
    expect(out[1]).toBe(next[1]);
    expect(out[2]).toBe(next[2]);
  });

  it("compares nested values, not just the top level", () => {
    expect(same({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] })).toBe(true);
    expect(same({ a: [1, { b: 2 }] }, { a: [1, { b: 3 }] })).toBe(false);
    expect(same({ a: undefined }, {})).toBe(false);
    expect(same([1], { 0: 1 })).toBe(false);
  });
});
