// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI
import UIKit

extension View {
    /// A Done button above the keyboard that puts it away. For screens whose
    /// text boxes take more than one line: Return adds a line there, so
    /// there's no other way to close the keyboard. Attach once per screen.
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
