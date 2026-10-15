// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Owners and members. The box's owners manage it -- other devices, SSH and
// app keys, power, updates, the security settings -- and members use it.
// The box says which this device is (`GET /devices` → `owner`); an agent
// from before roles says nothing, and then everyone is an owner.

import { get, type PairedDevice } from "./api";
import { usePoll } from "./ui";

/** Shown beside a control only an owner can use. */
export const OWNER_ONLY = "Only an owner of this box can change this. An owner can make this device one in Settings → Devices and pairing.";

/** Shown where something would run on the box as the box's own account (outside the sandbox). */
export const OWNER_ONLY_HOST = "Only an owner of this box can use this: it runs on the box as the box's own account, outside the sandbox, where it could reach the box's keys and settings. Code works in a sandbox, and members can use it.";

/** Whether this device is an owner; true until the box has said (no flashing disabled controls). */
export function useOwner(): boolean {
  const r = usePoll(() => get<{ owner?: boolean; devices: PairedDevice[] }>("/devices"), 60000);
  return r.data?.owner !== false;
}
