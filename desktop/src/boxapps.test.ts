// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { describe, expect, it } from "vitest";
import { boxOnlyReason } from "./boxapps";

describe("opening a box-only app", () => {
  it("explains instead of opening a broken page in another computer's browser", () => {
    const why = boxOnlyReason("http://127.0.0.1:3028/", "192.168.1.20", false, true);
    expect(why).toContain("only available on the box");
    expect(why).toContain("Open to the network");
    expect(boxOnlyReason("http://127.0.0.1:3021/lab?token=x", "quietbox.local", false, false)).not.toContain("Open to the network");
  });
  it("opens it on the box, in the desktop app (its tunnel), and when it's on the network", () => {
    expect(boxOnlyReason("http://127.0.0.1:3028/", "127.0.0.1", false, true)).toBeNull();
    expect(boxOnlyReason("http://127.0.0.1:3028/", "localhost", false, true)).toBeNull();
    expect(boxOnlyReason("http://127.0.0.1:3028/", "192.168.1.20", true, true)).toBeNull();
    expect(boxOnlyReason("http://192.168.1.20:3028/", "192.168.1.20", false, true)).toBeNull();
  });
});
