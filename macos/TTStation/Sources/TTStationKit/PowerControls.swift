import Foundation

/// A box power action, matching the agent's `POST /power` wire values (and
/// the CLI's `tt power <action>` positional argument -- see `POWER_ACTIONS`
/// in `crates/tt/src/main.rs`).
public enum PowerAction: String, CaseIterable {
    case resetChips = "reset-chips"
    case suspend
    case reboot
    case shutdown

    /// Machine ops take the whole box down (everything but a chip reset,
    /// which just re-runs `tt-smi -r` and keeps the box/agent up).
    public var isMachineOp: Bool { self != .resetChips }
    /// Whether the UI must confirm before firing (the destructive three).
    public var confirms: Bool { isMachineOp }
}

/// The transient state the app shows after issuing a power op, so the ensuing
/// connection drop reads as expected rather than as an error.
public enum PowerState: Equatable {
    case suspending
    case rebooting
    case poweredOff
    case waking
}

/// Pure transitions for `BoxViewModel.powerState`. Kept free of any I/O so
/// it's trivially unit-testable -- callers drive it from `TTClient.power`/
/// `wake` results and from reachability polling.
public enum PowerTransition {
    /// State to enter when `issued` is fired. reset-chips is instantaneous
    /// (no transient state); machine ops map to their in-progress/off state.
    public static func next(issued: PowerAction, reachable _: Bool) -> PowerState? {
        switch issued {
        case .resetChips: return nil
        case .suspend: return .suspending
        case .reboot: return .rebooting
        case .shutdown: return .poweredOff
        }
    }

    /// Recompute state when reachability changes. Any transient state clears
    /// once the box is reachable again (it came back / was woken); while
    /// unreachable it persists. `nil` (no power op in flight) stays `nil`.
    public static func onReachabilityChange(_ current: PowerState?, reachable: Bool) -> PowerState? {
        guard let current else { return nil }
        return reachable ? nil : current
    }
}
