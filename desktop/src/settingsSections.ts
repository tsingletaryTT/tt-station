// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Settings' sections, and which one a saved choice or `/app/settings#<id>`
// means (old names, like "phone" for Pairing and security, still land).

export type Section = "general" | "tori" | "models" | "power" | "people" | "data" | "devices" | "security" | "about";
export const SECTIONS: { id: Section; label: string; hint: string }[] = [
  { id: "general", label: "General", hint: "This app, start at login, keys" },
  { id: "tori", label: "Assistant (Tori) and voice", hint: "Shortcut, voice, speed" },
  { id: "models", label: "Models", hint: "Which models the box uses" },
  { id: "power", label: "Power", hint: "Sleep, restart, idle, after a power cut" },
  { id: "people", label: "People", hint: "You and your household" },
  { id: "devices", label: "Devices and pairing", hint: "Phones, computers, other boxes, SSH keys" },
  { id: "security", label: "Security and activity", hint: "Encryption, who did what" },
  { id: "data", label: "Your data", hint: "Recordings, how long things are kept, export and import" },
  { id: "about", label: "About", hint: "Software on the box" },
];

/** Sections that moved: an old saved choice or link still lands somewhere sensible. */
const RENAMED: Record<string, Section> = { phone: "devices", pairing: "devices" };

/** A section id from a saved choice or `#hash`, if it's one. */
export function sectionFrom(raw: string | null | undefined): Section | null {
  const id = (raw ?? "").replace(/^#/, "");
  const s = (RENAMED[id] ?? id) as Section;
  return SECTIONS.some((x) => x.id === s) ? s : null;
}
