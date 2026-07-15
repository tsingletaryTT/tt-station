import SwiftUI
import TTStationKit

/// The tasteful power control: an understated power-symbol menu, shared
/// verbatim between the box header (`BoxHeaderView`) and the menu-bar
/// popover (`MenuContentView`) so power is reachable from both surfaces.
///
/// Reset chips and Wake fire immediately (neither is destructive: a chip
/// reset just re-runs `tt-smi -r` and keeps the box/agent up; a wake is a
/// one-way Wake-on-LAN broadcast with no effect on a box that's already up).
/// The three machine ops (Suspend/Reboot/Shut Down) take the whole box down
/// and disconnect this Mac, so each confirms via a destructive
/// `.confirmationDialog` naming the concrete consequence before it fires.
struct PowerMenuView: View {
    @Bindable var box: BoxViewModel
    /// Which destructive action is pending confirmation, or `nil` when no
    /// dialog is showing. Doubles as the dialog's presentation driver via
    /// `confirmBinding` below.
    @State private var confirm: PowerAction?

    var body: some View {
        Menu {
            Button("Reset chips") { Task { await box.issuePower(.resetChips) } }
            Button("Wake") { Task { await box.wakeBox() } }
                .disabled((box.record.mac ?? "").isEmpty)
            Divider()
            Button("Suspend", role: .destructive) { confirm = .suspend }
            Button("Reboot…", role: .destructive) { confirm = .reboot }
            Button("Shut Down…", role: .destructive) { confirm = .shutdown }
        } label: {
            Image(systemName: "power")
        }
        .menuStyle(.borderlessButton)
        .help("Power controls for \(box.record.name)")
        .confirmationDialog(
            confirmTitle, isPresented: confirmBinding, titleVisibility: .visible
        ) {
            if let action = confirm {
                Button(confirmVerb(action), role: .destructive) {
                    Task { await box.issuePower(action) }
                }
                Button("Cancel", role: .cancel) {}
            }
        } message: { Text(confirmMessage) }
    }

    /// Bridges the optional `confirm` action to `confirmationDialog`'s
    /// `Bool` presentation binding: presented whenever an action is pending,
    /// and dismissing (Cancel, tapping outside, Esc) clears it back to `nil`
    /// rather than leaving a stale action armed for next time.
    private var confirmBinding: Binding<Bool> {
        Binding(get: { confirm != nil }, set: { if !$0 { confirm = nil } })
    }

    private var confirmTitle: String {
        switch confirm {
        case .suspend: return "Suspend \(box.record.name)?"
        case .reboot: return "Reboot \(box.record.name)?"
        case .shutdown: return "Shut Down \(box.record.name)?"
        case .resetChips, nil: return ""
        }
    }

    private func confirmVerb(_ a: PowerAction) -> String {
        switch a {
        case .suspend: return "Suspend"
        case .reboot: return "Reboot"
        case .shutdown: return "Shut Down"
        case .resetChips: return ""
        }
    }

    private var confirmMessage: String {
        switch confirm {
        case .suspend:
            return "This stops the serving model and sleeps the box; use Wake to resume."
        case .reboot:
            return "This stops the serving model and disconnects this Mac until the box is back."
        case .shutdown:
            return "This stops the serving model, disconnects this Mac, and powers the box off. Only Wake-on-LAN can bring it back."
        case .resetChips, nil:
            return ""
        }
    }
}
