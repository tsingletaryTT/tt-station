// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Owners and members. The box's owners manage it -- other devices, SSH and
// app keys, power, updates, the security settings -- and members use it.
// The box says which this device is (`GET /devices` → `owner`); an agent
// from before roles says nothing, and then everyone is an owner.

import { get, type PairedDevice } from "./api";
import { usePoll } from "./ui";

/** Shown beside a control only an owner can use. */
export const OWNER_ONLY = "Only an owner of this box can change this. An owner can make this device one in Settings → Pairing and security → Devices.";

/** Whether this device is an owner; true until the box has said (no flashing disabled controls). */
export function useOwner(): boolean {
  const r = usePoll(() => get<{ owner?: boolean; devices: PairedDevice[] }>("/devices"), 60000);
  return r.data?.owner !== false;
}
