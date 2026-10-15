// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const addToast = vi.hoisted(() => vi.fn());
vi.mock("@tenstorrent/vesper/toast", () => ({ addToast }));
const { copyText, writeClipboard } = await import("./ui");

const writeText = vi.fn<(text: string) => Promise<void>>();
const execCommand = vi.fn<(command: string) => boolean>();
/** The text the select-and-copy fallback had selected when it copied. */
let copied: string | null = null;

beforeEach(() => {
  copied = null;
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  Object.defineProperty(document, "execCommand", { configurable: true, value: execCommand });
});
afterEach(() => {
  vi.resetAllMocks();
  document.body.innerHTML = "";
});

/** The async API refuses, as it does without focus or permission. */
const refuse = () => writeText.mockRejectedValue(new DOMException("Document is not focused.", "NotAllowedError"));
const fallbackWorks = () => execCommand.mockImplementation((command) => {
  const area = document.querySelector("textarea");
  copied = command === "copy" && area ? area.value.slice(area.selectionStart, area.selectionEnd) : null;
  return copied !== null;
});
const toasts = (variant: string) => addToast.mock.calls.map(([t]) => t).filter((t) => t.variant === variant);

describe("writeClipboard", () => {
  it("uses the async clipboard API when it works", async () => {
    writeText.mockResolvedValue();
    expect(await writeClipboard("tt-station connect")).toBe(true);
    expect(writeText).toHaveBeenCalledWith("tt-station connect");
    expect(execCommand).not.toHaveBeenCalled();
  });

  it("falls back to select-and-copy when the async API refuses", async () => {
    refuse();
    fallbackWorks();
    const button = document.body.appendChild(document.createElement("button"));
    button.focus();
    expect(await writeClipboard("ssh ttuser@quietbox")).toBe(true);
    expect(execCommand).toHaveBeenCalledWith("copy");
    expect(copied).toBe("ssh ttuser@quietbox");
    expect(document.querySelector("textarea")).toBeNull(); // cleaned up
    expect(document.activeElement).toBe(button); // focus given back
  });

  it("falls back when there is no async API at all", async () => {
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: undefined });
    fallbackWorks();
    expect(await writeClipboard("plain http")).toBe(true);
    expect(copied).toBe("plain http");
  });

  it("reports failure when the fallback can't copy either", async () => {
    refuse();
    execCommand.mockReturnValue(false);
    expect(await writeClipboard("x")).toBe(false);
    expect(document.querySelector("textarea")).toBeNull();
  });

  it("reports failure when the fallback throws", async () => {
    refuse();
    execCommand.mockImplementation(() => { throw new Error("not supported"); });
    expect(await writeClipboard("x")).toBe(false);
    expect(document.querySelector("textarea")).toBeNull();
  });
});

describe("copyText", () => {
  it("says Copied when it worked", async () => {
    writeText.mockResolvedValue();
    await copyText("hello");
    expect(toasts("success").map((t) => t.content)).toEqual(["Copied."]);
    expect(toasts("danger")).toEqual([]);
  });

  it("says Copied after the fallback worked", async () => {
    refuse();
    fallbackWorks();
    await copyText("hello", "Command copied.");
    expect(toasts("success").map((t) => t.content)).toEqual(["Command copied."]);
  });

  it("shows an error and never says Copied when both ways fail", async () => {
    refuse();
    execCommand.mockReturnValue(false);
    await copyText("hello");
    expect(toasts("success")).toEqual([]);
    expect(toasts("danger").map((t) => t.content)).toEqual(["Couldn't copy. Select the text and copy it yourself."]);
  });
});
