// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The words TT Station uses, one per thing, so every screen says the same.
// Write copy with these; the code may keep older names (gozer, evals)
// where they're the agent's API or a tool's real name.

export const TERMS = {
  /** The computer with the Tenstorrent chips (a QuietBox, a Galaxy server, a workstation
   *  with Tenstorrent cards). Never "device", "machine" or "server" in copy. */
  box: "box",
  /** A phone or computer paired with the box. Only these are "devices". */
  device: "device",
  /** The assistant summoned from anywhere. Say "Assistant (Tori)" the first time on a screen, "Tori" after. */
  assistant: "Assistant (Tori)",
  /** gozer: hands chips to one job at a time, so two never collide. */
  chipScheduler: "chip scheduler",
  /** A model's accuracy and speed tests (the agent's "evals"). */
  checkUp: "Check-up",
  /** A one-click setup of the whole box (Chat, Images, Transcribe…). */
  mode: "mode",
  /** Developer tools: who holds the chips, on Hardware. */
  developerTools: "Developer tools",
  /** A job the box does on its own, on a timetable or when run; switching modes at set times is one. Never "schedule" in copy. */
  routine: "routine",
  /** The one list of everything waiting for this person's OK, under the bell at the top right (the phone calls it "Waiting for you"). */
  needsYou: "Notifications",
  /** Settings' one place for devices: the list, pairing a phone or computer, other boxes, SSH keys. */
  devicesAndPairing: "Devices and pairing",
  /** Owners manage the box (devices, keys, power, updates); members use it. */
  owner: "owner",
  member: "member",
} as const;

/** One line each, for a "What do these words mean?" list. */
export const GLOSSARY: { term: string; means: string }[] = [
  { term: "Box", means: "The computer with the Tenstorrent chips that runs the models: a QuietBox, a Galaxy server, a workstation with Tenstorrent cards." },
  { term: "Device", means: "A phone or computer you've paired with the box." },
  { term: "Mode", means: "A one-click setup of the whole box for one kind of work, like Chat or Images." },
  { term: "Assistant (Tori)", means: "The assistant you call up over anything you're doing, by voice or by typing." },
  { term: "Check-up", means: "Tests a model's accuracy and speed on this box." },
  { term: "Chip scheduler", means: "Hands the chips to one job at a time, so two never collide." },
  { term: "Routine", means: "A job the box does on its own, when you run it or on a timetable, like a morning check or switching modes at set times." },
  { term: "Notifications", means: "Everything waiting for your OK, from Code, routines and, for owners, pairing requests, in one list: the bell at the top right of the window." },
  { term: "Owner and member", means: "Owners manage the box: devices, keys, power and updates. Members use it." },
];
