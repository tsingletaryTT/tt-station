// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

// Settings → General → This app: the developer tools switch.

import { Switch } from "@tenstorrent/vesper/switch";
import { setDevTools, useDevTools } from "./devtools";

export function DevToolsSetting() {
  const on = useDevTools();
  return (
    <div className="stack" style={{ gap: 4 }}>
      <Switch label="Developer tools" checked={on} onChange={(e) => setDevTools(e.target.checked)} />
      <span className="muted" style={{ fontSize: 12 }}>
        Adds Bring-up (porting new models) to the sidebar, and the silicon check and who holds the chips to Hardware.
      </span>
    </div>
  );
}
