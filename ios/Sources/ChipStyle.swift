// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI

/// Chip colors and load for the Box tab's charts.
enum ChipStyle {
    /// One color per chip in multi-series charts.
    static let chipPalette: [Color] = [
        Color(red: 0.25, green: 0.95, blue: 1.0),
        Color(red: 1.0, green: 0.3, blue: 0.85),
        Color(red: 0.6, green: 1.0, blue: 0.35),
        Color(red: 1.0, green: 0.72, blue: 0.2),
    ]

    /// 0...1: how far the chip is from its own idle (lowest power seen) to its TDP.
    static func activity(_ chip: ChipReading) -> Double {
        guard let p = chip.powerW else { return 0 }
        let idle = chip.idleW ?? 15, top = chip.tdpW ?? 125
        return min(max((p - idle) / max(10, top - idle), 0), 1)
    }
}
