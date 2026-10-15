// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

import SwiftUI
import SwiftTerm

/// The real tt-toplike, on the phone. The box runs `tt-toplike --remote`
/// against its own telemetry in a terminal (`/toplike/term`) and streams the
/// screen here; SwiftTerm draws it exactly as a terminal would, so every view
/// is tt-toplike's own, from the box's live readings. Tap it for tt-toplike's
/// keys (v = next view, i = inference, l = legend).
struct ToplikeTerminal: UIViewRepresentable {
    let session: BoxSession
    /// insights | starfield | castle | flow | arcade | defrag | inference | hivemind
    let mode: String
    var fontSize: CGFloat = 7

    func makeCoordinator() -> Coordinator { Coordinator(session: session) }

    func makeUIView(context: Context) -> TerminalView {
        let view = TerminalView(frame: .zero)
        view.font = UIFont(name: "IBMPlexMono", size: fontSize) ?? .monospacedSystemFont(ofSize: fontSize, weight: .regular)
        view.nativeBackgroundColor = .black
        view.nativeForegroundColor = UIColor(white: 0.88, alpha: 1)
        view.terminalDelegate = context.coordinator
        context.coordinator.view = view
        return view
    }

    func updateUIView(_ view: TerminalView, context: Context) {
        context.coordinator.connect(mode: mode)
    }

    static func dismantleUIView(_ view: TerminalView, coordinator: Coordinator) {
        coordinator.disconnect()
    }

    final class Coordinator: NSObject, TerminalViewDelegate {
        let session: BoxSession
        weak var view: TerminalView?
        private var socket: URLSessionWebSocketTask?
        private var mode: String?

        init(session: BoxSession) { self.session = session }

        func connect(mode: String) {
            guard mode != self.mode, let view else { return }
            disconnect()
            self.mode = mode
            let terminal = view.getTerminal()
            view.feed(text: "\u{1b}[2J\u{1b}[Hstarting tt-toplike…\r\n")
            guard let socket = session.toplikeSocket(mode: mode, cols: max(terminal.cols, 40), rows: max(terminal.rows, 12)) else {
                view.feed(text: "Not paired with this box.\r\n")
                return
            }
            self.socket = socket
            socket.resume()
            receive(on: socket)
        }

        func disconnect() {
            socket?.cancel(with: .goingAway, reason: nil)
            socket = nil
            mode = nil
        }

        private func receive(on socket: URLSessionWebSocketTask) {
            socket.receive { [weak self] result in
                guard let self, socket === self.socket else { return }
                switch result {
                case .success(.data(let data)):
                    let bytes = [UInt8](data)
                    DispatchQueue.main.async { self.view?.feed(byteArray: bytes[...]) }
                    self.receive(on: socket)
                case .success(.string(let text)):
                    DispatchQueue.main.async { self.view?.feed(text: text) }
                    self.receive(on: socket)
                case .failure:
                    DispatchQueue.main.async { self.view?.feed(text: "\r\n\u{1b}[33mtt-toplike disconnected.\u{1b}[0m\r\n") }
                @unknown default:
                    break
                }
            }
        }

        private func sendJSON(_ object: [String: Any]) {
            guard let socket, let data = try? JSONSerialization.data(withJSONObject: object),
                  let text = String(data: data, encoding: .utf8) else { return }
            socket.send(.string(text)) { _ in }
        }

        // MARK: TerminalViewDelegate

        func send(source: TerminalView, data: ArraySlice<UInt8>) {
            sendJSON(["type": "input", "data": String(decoding: data, as: UTF8.self)])
        }

        func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
            sendJSON(["type": "resize", "cols": newCols, "rows": newRows])
        }

        func setTerminalTitle(source: TerminalView, title: String) {}
        func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        func scrolled(source: TerminalView, position: Double) {}
        func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {}
        func bell(source: TerminalView) {}
        func clipboardCopy(source: TerminalView, content: Data) {}
        func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
        func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }
}
