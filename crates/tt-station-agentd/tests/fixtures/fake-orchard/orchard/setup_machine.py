# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
class Step:
    def __init__(self, name, status, detail):
        self.name, self.status, self.detail = name, status, detail


class Options:
    pass


def default_env(checkout=None):
    return checkout


def plan(env, opts):
    return [Step("python", "ok", "Python 3.12"), Step("tt-gozer", "ok", "gozer 0.3.2, all free")]
