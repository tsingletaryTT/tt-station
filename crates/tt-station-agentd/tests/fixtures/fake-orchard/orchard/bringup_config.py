# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import json
from pathlib import Path
from types import SimpleNamespace


def load(path):
    raw = json.loads(Path(path).read_text())
    return SimpleNamespace(runs_root=Path(raw["runs_root"]), required_chips="2,4", package_format=None,
                           coder=SimpleNamespace(target="fake/coder", port=8100, chips=2))


def run_dir(cfg, model):
    return cfg.runs_root / model.lower().replace("/", "--")
