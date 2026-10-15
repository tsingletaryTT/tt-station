import SwiftUI

struct PairTarget: Identifiable {
    let name: String
    let host: String
    let port: Int
    var id: String { "\(host):\(port)" }
}

/// Pairing happens only by scanning the QR TT Station shows on the box: the
/// code carries the box's certificate fingerprint, which is what lets this
/// iPhone talk to it over pinned HTTPS. There is no typed-code pairing (it
/// couldn't pin the certificate), so this sheet just says where to find the QR.
/// The scanned `ttstation://pair` link is handled in `AppModel.handle(url:)`.
struct PairSheet: View {
    /// The box to pair with, when known (a nearby or saved box); nil for "add a box".
    let target: PairTarget?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            VStack(spacing: 28) {
                VStack(spacing: 14) {
                    Image(systemName: "qrcode.viewfinder")
                        .font(.system(size: 40, weight: .medium))
                        .foregroundStyle(Brand.accent)
                        .frame(width: 84, height: 84)
                        .glassEffect(.regular.tint(Brand.accent.opacity(0.3)), in: .circle)
                    Text(target.map { "Pair with \($0.name)" } ?? "Pair a box")
                        .font(.tt(.title2).weight(.bold))
                    Text("Scan the pairing code shown in TT Station on the box (Settings → Devices and pairing).")
                        .font(.tt(.subheadline))
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                    Text("Point this iPhone's Camera app at the code and tap the link. The code carries the box's security certificate, so this iPhone only ever talks to your box over an encrypted connection on your home network.")
                        .font(.tt(.footnote))
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.top, 12)
                Spacer()
                if let target {
                    Text("\(target.host):\(String(target.port))")
                        .font(.ttMono(12, relativeTo: .caption))
                        .foregroundStyle(.tertiary)
                }
            }
            .padding(24)
            .frame(maxWidth: .infinity)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
    }
}
