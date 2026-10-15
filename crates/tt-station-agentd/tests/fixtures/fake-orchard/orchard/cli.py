# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
from pathlib import Path


def find_config(explicit, env, checkout, home):
    return Path(checkout) / "config" / "bringup.json"
