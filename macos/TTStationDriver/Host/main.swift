// TTStationDriver host app — its only job is to carry the dext inside its bundle
// (Contents/Library/SystemExtensions/) and ask macOS to activate or deactivate it.
// macOS requires a dext to be installed by an app living in /Applications.
//
//   /Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver install|uninstall|status
//
// Modeled on tinygrad's TinyGPUCLIRunner.swift (MIT), trimmed to a headless CLI.

import Foundation
import SystemExtensions

let dextID = "com.tenstorrent.ttstation.driver"

/// Exit codes, stable for scripts/install-dev.sh.
enum Exit: Int32 { case ok = 0, usage = 2, failed = 3, needsApproval = 4 }

/// `systemextensionsctl list` is the only public way to read a dext's state without
/// submitting a request; parse the line carrying our bundle ID.
func dextState() -> String {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/usr/bin/systemextensionsctl")
    p.arguments = ["list"]
    let out = Pipe()
    p.standardOutput = out
    p.standardError = Pipe()
    guard (try? p.run()) != nil else { return "unknown (systemextensionsctl failed)" }
    p.waitUntilExit()
    let text = String(decoding: out.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    guard let line = text.split(separator: "\n").first(where: { $0.contains(dextID) }) else {
        return "not installed"
    }
    // The state is the trailing "[...]" group, e.g. "[activated enabled]".
    if let open = line.lastIndex(of: "[") { return String(line[open...]) }
    return String(line)
}

let approvalHelp = """
    Approve it in System Settings > General > Login Items & Extensions > Driver Extensions
    (or Privacy & Security, depending on the macOS version), then re-run `status`.
    """

final class Requester: NSObject, OSSystemExtensionRequestDelegate {
    let activating: Bool
    init(activating: Bool) { self.activating = activating }

    func submit() {
        let req = activating
            ? OSSystemExtensionRequest.activationRequest(forExtensionWithIdentifier: dextID, queue: .main)
            : OSSystemExtensionRequest.deactivationRequest(forExtensionWithIdentifier: dextID, queue: .main)
        req.delegate = self
        OSSystemExtensionManager.shared.submitRequest(req)
    }

    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) {
        print("User approval required.\n\(approvalHelp)")
        exit(Exit.needsApproval.rawValue)
    }

    func request(_ request: OSSystemExtensionRequest,
                 didFinishWithResult result: OSSystemExtensionRequest.Result) {
        switch result {
        case .completed: print(activating ? "Driver activated." : "Driver deactivated.")
        case .willCompleteAfterReboot: print("Will complete after reboot.")
        @unknown default: print("Finished: \(result.rawValue)")
        }
        exit(Exit.ok.rawValue)
    }

    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) {
        let code = (error as NSError).code
        print("Error (\(code)): \(error.localizedDescription)")
        // OSSystemExtensionError codes worth a hint.
        switch code {
        case 4: print("Missing/invalid entitlements — with SIP on, an ad-hoc dext is rejected here.")
        case 8: print("Dext not found in the app bundle — was it embedded by the build?")
        case 9: print("Extension disabled by the user.\n\(approvalHelp)")
        default: break
        }
        exit(Exit.failed.rawValue)
    }

    func request(_ request: OSSystemExtensionRequest,
                 actionForReplacingExtension existing: OSSystemExtensionProperties,
                 withExtension ext: OSSystemExtensionProperties) -> OSSystemExtensionRequest.ReplacementAction {
        print("Replacing \(existing.bundleVersion) with \(ext.bundleVersion)")
        return .replace
    }
}

let args = CommandLine.arguments
switch args.count > 1 ? args[1] : "" {
case "status":
    print("\(dextID): \(dextState())")
    exit(Exit.ok.rawValue)
case "install", "uninstall":
    guard Bundle.main.bundlePath.hasPrefix("/Applications/") else {
        print("Run from /Applications/TTStationDriver.app — macOS refuses dexts from elsewhere.")
        exit(Exit.failed.rawValue)
    }
    let requester = Requester(activating: args[1] == "install")
    requester.submit()
    dispatchMain()  // delegate callbacks exit the process
default:
    print("usage: TTStationDriver install | uninstall | status")
    exit(Exit.usage.rawValue)
}
