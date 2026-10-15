// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { act } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { ask, askText, whoIsAffected } from "./dialog";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

afterEach(() => { document.body.innerHTML = ""; });

const buttons = () => [...document.querySelectorAll<HTMLButtonElement>(".dialog button")];
const named = (text: string) => buttons().find((b) => b.textContent === text)!;
const key = (k: string, shift = false) => act(() => {
  window.dispatchEvent(new KeyboardEvent("keydown", { key: k, shiftKey: shift, bubbles: true, cancelable: true }));
});

/** Opens the dialog and lets it render; resolves to its pending answer (wrapped, so awaiting this doesn't wait for the answer). */
async function open(title: string, opts: Parameters<typeof ask>[1]): Promise<{ answer: Promise<boolean> }> {
  let answer!: Promise<boolean>;
  await act(async () => { answer = ask(title, opts); });
  return { answer };
}

describe("ask", () => {
  it("a destructive question starts on Cancel, so Enter can't delete by accident", async () => {
    const { answer } = await open("Delete this?", { confirm: "Delete", danger: true });
    expect(document.activeElement).toBe(named("Cancel"));
    await act(async () => { named("Cancel").click(); });
    expect(await answer).toBe(false);
  });

  it("an ordinary question starts on its confirm button", async () => {
    const { answer } = await open("Update now?", { confirm: "Update" });
    expect(document.activeElement).toBe(named("Update"));
    await act(async () => { named("Update").click(); });
    expect(await answer).toBe(true);
  });

  it("keeps Tab inside the dialog, both ways", async () => {
    const { answer } = await open("Delete this?", { confirm: "Delete", danger: true });
    key("Tab");
    expect(document.activeElement).toBe(named("Delete"));
    key("Tab");
    expect(document.activeElement).toBe(named("Cancel"));
    key("Tab", true);
    expect(document.activeElement).toBe(named("Delete"));
    key("Escape");
    expect(await answer).toBe(false);
  });

  it("gives focus back to what had it when it closes", async () => {
    const trigger = document.createElement("button");
    trigger.textContent = "Delete…";
    document.body.appendChild(trigger);
    trigger.focus();
    const { answer } = await open("Delete this?", { confirm: "Delete", danger: true });
    expect(document.activeElement).not.toBe(trigger);
    key("Escape");
    await answer;
    expect(document.activeElement).toBe(trigger);
  });

  it("names the dialog by its title and describes it by its body", async () => {
    const { answer } = await open("Stop Qwen3-32B?", { body: whoIsAffected("Qwen3-32B"), confirm: "Stop" });
    const d = document.querySelector('[role="alertdialog"]')!;
    expect(document.getElementById(d.getAttribute("aria-labelledby")!)!.textContent).toBe("Stop Qwen3-32B?");
    expect(document.getElementById(d.getAttribute("aria-describedby")!)!.textContent).toMatch(/stops for everyone using the box/);
    key("Escape");
    await answer;
  });
});

describe("askText", () => {
  it("starts in the text field and restores focus after", async () => {
    const trigger = document.createElement("button");
    document.body.appendChild(trigger);
    trigger.focus();
    let answer!: Promise<string | null>;
    await act(async () => { answer = askText("Rename", "old name"); });
    expect(document.activeElement?.tagName).toBe("INPUT");
    key("Escape");
    expect(await answer).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });
});
