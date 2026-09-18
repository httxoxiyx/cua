// Test-only observer of the real NSTextField mouse path. It does not implement
// AXPress, change first-mouse acceptance, focus the field, or insert text.
import AppKit

final class ClickObservedTextField: NSTextField {
    private var mouseDownCount = 0

    override func mouseDown(with event: NSEvent) {
        mouseDownCount += 1
        super.mouseDown(with: event)
        record(event: "text_field_mouse_down", clickCount: event.clickCount)
    }

    func prepareClickOracle() {
        guard ProcessInfo.processInfo.environment["CUA_APPKIT_TEXT_CLICK_ORACLE"] != nil,
              let window else { return }
        // Test setup only: require the measured click to acquire the editor.
        // Otherwise AppKit may have chosen this field as its initial responder.
        window.endEditing(for: nil)
        window.makeFirstResponder(nil)
        record(event: "ready", clickCount: 0)
    }

    private func record(event: String, clickCount: Int) {
        guard let output = ProcessInfo.processInfo.environment["CUA_APPKIT_TEXT_CLICK_ORACLE"],
              let window else { return }
        let editor = currentEditor() as? NSTextView
        let record: [String: Any] = [
            "event": event,
            "pid": Int(ProcessInfo.processInfo.processIdentifier),
            "window_id": window.windowNumber,
            "mouse_down_count": mouseDownCount,
            "click_count": clickCount,
            "editor_is_first_responder": editor != nil && window.firstResponder === editor,
            "has_marked_text": editor?.hasMarkedText() ?? false,
            "text": stringValue,
            "frontmost_pid": Int(NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1),
        ]
        guard var data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]) else {
            return
        }
        data.append(0x0a)
        // One initial click is the oracle. Atomic replacement keeps the reader
        // from accepting a partially written record; count exposes repetition.
        try? data.write(to: URL(fileURLWithPath: output), options: .atomic)
    }
}
