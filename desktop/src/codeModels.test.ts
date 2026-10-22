// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { chipsLine, groupModels, optionOrigin, optionTitle, reliabilityText, serveRequest, type CodeModel } from "./codeModels";

const option = (o: Partial<CodeModel> & { key: string }): CodeModel => ({
  group: "gpt-oss-20b", name: "gpt-oss-20b", model: "openai/gpt-oss-20b", source: "catalog", origin: "tt catalog",
  profile: null, profile_label: null, profile_note: null, chips: null, context: 131072,
  tools: { verified: true, how: "tool parser openai" }, prefix_cache: { state: "unknown", how: "not seen serving here yet" },
  params_b: null, served_id: null, speed: null, reliability: null, choice: { action: "switch" }, eta_s: null, serve: null, ...o,
});

describe("Code's model list", () => {
  it("groups the options by model, keeping the box's order", () => {
    const groups = groupModels([
      option({ key: "a", choice: { action: "in_use" }, profile: "p150x2-throughput" }),
      option({ key: "b", group: "qwen3-coder", name: "Qwen3-Coder", tools: { verified: false, how: "x" } }),
      option({ key: "c", profile: "p150x4" }),
    ]);
    expect(groups.map((g) => [g.key, g.options.map((o) => o.key), g.inUse, g.verified])).toEqual([
      ["gpt-oss-20b", ["a", "c"], true, true],
      ["qwen3-coder", ["b"], false, false],
    ]);
    expect(groupModels([])).toEqual([]);
  });

  it("says what each option is and where it comes from", () => {
    const labelled = option({ key: "p", profile: "p150x4", profile_label: "4 chips · low latency" });
    expect(optionTitle(labelled)).toBe("4 chips · low latency");
    const cat = option({ key: "c", source: "catalog", origin: "tt catalog", chips: 4 });
    expect(optionTitle(cat)).toBe("4 chips · tt catalog");
    expect(optionOrigin(cat)).toBe("tt catalog");
    const running = option({ key: "r", source: "running", model: "openai/gpt-oss-20b" });
    expect(optionTitle(running)).toBe("Serving now");
    expect(optionOrigin(running)).toBe("openai/gpt-oss-20b");
  });

  it("only ever calls the box's serve route", () => {
    expect(serveRequest(option({ key: "s", serve: { route: "/serve", body: { x: 1 } } }))).toEqual({ route: "/serve", body: { x: 1 } });
    for (const route of ["/community/serve", "/vllm/native/serve"]) {
      expect(serveRequest(option({ key: route, serve: { route, body: { x: 1 } } }))).toBeNull();
    }
    expect(serveRequest(option({ key: "x", serve: { route: "/power", body: {} } }))).toBeNull();
    expect(serveRequest(option({ key: "y" }))).toBeNull();
  });

  it("puts chips and reliability in words", () => {
    expect(chipsLine(option({ key: "a", chips: 2 }))).toBe("It uses 2 chips on the box.");
    expect(chipsLine(option({ key: "b", chips: 1 }))).toBe("It uses 1 chip on the box.");
    expect(chipsLine(option({ key: "c" }))).toBeNull();
    expect(reliabilityText({ window: 65536, valid: 99, trials: 100, at: 0, setup_unrecorded: false })).toBe("99% valid tool calls (99/100) · holds up to 64K");
  });
});
