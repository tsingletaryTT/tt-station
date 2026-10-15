# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
from types import SimpleNamespace


def default_signals(cfg):
    return SimpleNamespace(local_snapshot=lambda model: None)


def run_preflight(cfg, model, *, accept_credentials, signals=None, resuming=False):
    hub = SimpleNamespace(total_bytes=54_000_000_000, license="apache-2.0")
    if model.startswith("refuse/"):
        return [SimpleNamespace(name="hub", status="block", detail="model not found", reason="model-unavailable", data=None)]
    return [SimpleNamespace(name="hub", status="ok", detail="public, apache-2.0", reason=None, data=hub),
            SimpleNamespace(name="disk", status="ok", detail="plenty", reason=None, data=None)]


def blocked(checks):
    return [c for c in checks if c.status == "block"]
