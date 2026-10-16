// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI
import UIKit

extension View {
    /// A Done button above the keyboard that puts it away. For screens whose
    /// text boxes take more than one line: Return adds a line there, so
    /// there's no other way to close the keyboard. Attach once per screen.
    /// Not for a text box pinned to the bottom edge (Chat): the keyboard's bar
    /// can land on top of it there, so use `HideKeyboardOverlay` instead.
    func keyboardDoneButton() -> some View {
        toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button("Done") {
                    UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
                }
                .accessibilityLabel("Hide keyboard")
            }
        }
    }
}

/// Puts the keyboard away, as a small button in the layout itself. For text
/// boxes pinned to the bottom of the screen, where the keyboard's own toolbar
/// can cover them. Show it only while the box has focus.
struct HideKeyboardButton: View {
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Label("Done", systemImage: "keyboard.chevron.compact.down")
                .font(.tt(.subheadline).weight(.semibold))
                .padding(.horizontal, 14)
                .padding(.vertical, 8)
        }
        .buttonStyle(.plain)
        .glassEffect(.regular.interactive(), in: .capsule)
        .accessibilityLabel("Hide keyboard")
    }
}

/// `HideKeyboardButton` floating just above a bottom-pinned text box while it
/// has focus. Use it as the box's `.overlay(alignment: .topTrailing)`. It takes
/// no layout space, so showing it doesn't resize the safe-area inset the box
/// lives in, which would re-lay out everything above it (a whole conversation)
/// and can take focus away from the box being tapped.
struct HideKeyboardOverlay: View {
    let visible: Bool
    let action: () -> Void

    var body: some View {
        ZStack {
            if visible {
                HideKeyboardButton(action: action).transition(.opacity)
            }
        }
        .alignmentGuide(.top) { $0[.bottom] + 8 }
        .animation(.easeOut(duration: 0.15), value: visible)
    }
}
