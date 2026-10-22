// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import ActivityKit
import Foundation

/// A Live Activity for long work on the box: a model loading, an Orchard
/// bring-up, a transcription. Shared by the app (which starts and updates it)
/// and the widget extension (which draws it on the Lock Screen and in the
/// Dynamic Island). Updated while the app runs; in between, the progress bar
/// runs on its own from `estimatedEnd`.
struct BoxActivityAttributes: ActivityAttributes {
    struct ContentState: Codable, Hashable {
        /// "Loading Qwen3-32B", "Bringing up Clef", "Transcribing Team sync".
        var title: String
        /// "Stage 2 of 8: graft", "Part 3 of 11".
        var detail: String
        /// 0...1 when known.
        var progress: Double?
        /// When it's expected to finish, for a self-running bar and countdown.
        var estimatedStart: Date?
        var estimatedEnd: Date?
        /// SF Symbol for the kind of work.
        var symbol: String
        var done: Bool
    }

    var boxName: String
    /// model | orchard | transcribe
    var kind: String
}
