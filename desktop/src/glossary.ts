// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// The words TT Station uses, one per thing, so every screen says the same.
// Write copy with these; the code may keep older names (orchard, gozer, evals)
// where they're the agent's API or a tool's real name.

export const TERMS = {
  /** The QuietBox itself. Never "device", "machine" or "server" in copy. */
  box: "box",
  /** A phone or computer paired with the box. Only these are "devices". */
  device: "device",
  /** The assistant summoned from anywhere. Say "Assistant (Tori)" the first time on a screen, "Tori" after. */
  assistant: "Assistant (Tori)",
  /** gozer: hands chips to one job at a time, so two never collide. */
  chipScheduler: "chip scheduler",
  /** tt-orchard: grafts a fine-tune onto an architecture that already runs here. */
  quickPort: "Quick port",
  /** A coding agent writes a new architecture's code, stage by stage. */
  agentPort: "Agent port",
  /** Getting a model that isn't in Models running: Quick port, Agent port or a community package. */
  bringUp: "Bring-up",
  /** A model's accuracy and speed tests (the agent's "evals"). */
  checkUp: "Check-up",
  /** A one-click setup of the whole box (Chat, Images, Transcribe…). */
  mode: "mode",
  /** Developer tools: Bring-up, Terminals, the silicon check and chip leases. */
  developerTools: "Developer tools",
} as const;

/** One line each, for a "What do these words mean?" list. */
export const GLOSSARY: { term: string; means: string }[] = [
  { term: "Box", means: "Your QuietBox: the computer with the Tenstorrent chips that runs the models." },
  { term: "Device", means: "A phone or computer you've paired with the box." },
  { term: "Mode", means: "A one-click setup of the whole box for one kind of work, like Chat or Images." },
  { term: "Assistant (Tori)", means: "The assistant you call up over anything you're doing, by voice or by typing." },
  { term: "Check-up", means: "Tests a model's accuracy and speed on this box." },
  { term: "Bring-up", means: "Getting a model that isn't in Models running here." },
  { term: "Quick port", means: "A bring-up that grafts a fine-tune onto a model that already runs here." },
  { term: "Agent port", means: "A bring-up where a coding agent writes a new architecture's code." },
  { term: "Chip scheduler", means: "Hands the chips to one job at a time, so two never collide." },
];
