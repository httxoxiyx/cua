// Disposable AppKit keyboard oracle. The default menu uses the standard nil-
// target responder chain. Selection notifications observe the real field editor
// without replacing its delegate or performing the action for it.
import AppKit

private var keyboardSelectionProbe: KeyboardSelectionProbe?

private final class KeyboardSelectionProbe: NSObject {
    weak var controller: HarnessWindowController?
    let outputPath = ProcessInfo.processInfo.environment["CUA_APPKIT_KEYBOARD_ORACLE"]
    let menuRoute = ProcessInfo.processInfo.environment["CUA_APPKIT_KEYBOARD_ROUTE"] == "explicit_target"
        ? "explicit_target" : "responder_chain"
    private var selectAllCount = 0
    private var selectionChangeCount = 0
    private var preparationComplete = false
    private var eventMonitor: Any?
    private var selectionObserver: NSObjectProtocol?
    private var lifecycleObservers: [NSObjectProtocol] = []
    private var heartbeat: Timer?

    init(controller: HarnessWindowController) {
        self.controller = controller
        super.init()
        if outputPath != nil {
            eventMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .flagsChanged]) {
                [weak self] event in
                let kind = event.type == .keyDown ? "key_down"
                    : event.type == .keyUp ? "key_up" : "flags_changed"
                self?.record(kind, extra: [
                    "key_code": Int(event.keyCode),
                    "command": event.modifierFlags.contains(.command),
                    "modifier_flags": event.modifierFlags.rawValue,
                ])
                return event
            }
            selectionObserver = NotificationCenter.default.addObserver(
                forName: NSTextView.didChangeSelectionNotification, object: nil, queue: nil
            ) { [weak self] notification in
                guard let self,
                      let controller = self.controller,
                      let editor = notification.object as? NSTextView,
                      controller.textInput.currentEditor() === editor else { return }
                self.selectionChangeCount += 1
                self.record("selection_changed", extra: [
                    "source": "NSTextView.didChangeSelectionNotification",
                ])
            }
            for (name, event) in [
                (NSApplication.didBecomeActiveNotification, "application_did_become_active"),
                (NSApplication.didResignActiveNotification, "application_did_resign_active"),
            ] {
                lifecycleObservers.append(NotificationCenter.default.addObserver(
                    forName: name, object: NSApp, queue: nil
                ) { [weak self] _ in self?.record(event) })
            }
            for (name, event) in [
                (NSWindow.didBecomeKeyNotification, "window_did_become_key"),
                (NSWindow.didResignKeyNotification, "window_did_resign_key"),
                (NSWindow.didBecomeMainNotification, "window_did_become_main"),
                (NSWindow.didResignMainNotification, "window_did_resign_main"),
            ] {
                lifecycleObservers.append(NotificationCenter.default.addObserver(
                    forName: name, object: controller.window, queue: nil
                ) { [weak self] _ in self?.record(event) })
            }
            // An opt-in, fixture-local event-loop heartbeat makes a stopped
            // observer distinguishable from a genuinely quiet foreground app.
            heartbeat = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) {
                [weak self] _ in
                guard let self, self.preparationComplete else { return }
                self.record("heartbeat")
            }
        }
    }

    deinit {
        if let eventMonitor { NSEvent.removeMonitor(eventMonitor) }
        if let selectionObserver { NotificationCenter.default.removeObserver(selectionObserver) }
        for observer in lifecycleObservers { NotificationCenter.default.removeObserver(observer) }
        heartbeat?.invalidate()
    }

    func prepare() {
        guard outputPath != nil, let controller else { return }
        let seed = "Cua background selection fixture"
        controller.textInput.stringValue = seed
        controller.textInputMirror.stringValue = seed
        // This is setup, not the measured background action. Normal AppKit
        // startup may activate the fixture; the test establishes and verifies
        // the separate foreground sentinel before sending the tested hotkey.
        let focused = controller.window.makeFirstResponder(controller.textInput)
        if let editor = controller.textInput.currentEditor() as? NSTextView {
            editor.setSelectedRange(NSRange(location: (seed as NSString).length, length: 0))
        }
        record("ready", extra: ["local_focus_requested": focused])
        preparationComplete = true
    }

    // Positive control only: an explicit menu target bypasses responder-chain
    // recipient resolution and must not stand in for the default test.
    @objc func selectAll(_ sender: Any?) {
        selectAllCount += 1
        if let controller,
           let editor = controller.textInput.currentEditor() as? NSTextView,
           controller.window.firstResponder === editor {
            editor.selectAll(sender)
        }
        record("select_all")
    }

    private func record(_ event: String, extra: [String: Any] = [:]) {
        guard let outputPath, let controller else { return }
        let editor = controller.textInput.currentEditor() as? NSTextView
        var record: [String: Any] = [
            "event": event,
            "monotonic_seconds": ProcessInfo.processInfo.systemUptime,
            "pid": Int(ProcessInfo.processInfo.processIdentifier),
            "window_id": controller.window.windowNumber,
            "active": NSApp.isActive,
            "key_window": controller.window.isKeyWindow,
            "main_window": controller.window.isMainWindow,
            "application_key_window_id": NSApp.keyWindow?.windowNumber ?? -1,
            "application_main_window_id": NSApp.mainWindow?.windowNumber ?? -1,
            "frontmost_pid": Int(NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1),
            "menu_route": menuRoute,
            "preparation_complete": preparationComplete,
            "select_all_count": selectAllCount,
            "selection_change_count": selectionChangeCount,
            "editor_present": editor != nil,
            "editor_is_first_responder": editor != nil && controller.window.firstResponder === editor,
            "text": controller.textInput.stringValue,
            "text_utf16_length": (controller.textInput.stringValue as NSString).length,
        ]
        if let editor {
            record["selection_location"] = editor.selectedRange().location
            record["selection_length"] = editor.selectedRange().length
        }
        for (key, value) in extra { record[key] = value }
        guard let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]) else {
            return
        }
        if !FileManager.default.fileExists(atPath: outputPath) {
            FileManager.default.createFile(atPath: outputPath, contents: nil)
        }
        guard let file = FileHandle(forWritingAtPath: outputPath) else { return }
        defer { file.closeFile() }
        file.seekToEndOfFile()
        file.write(data)
        file.write(Data([0x0a]))
        file.synchronizeFile()
    }
}

func installKeyboardSelectionProbe(target: HarnessWindowController, mainMenu: NSMenu) {
    let probe = KeyboardSelectionProbe(controller: target)
    keyboardSelectionProbe = probe
    let editItem = NSMenuItem(title: "Edit", action: nil, keyEquivalent: "")
    editItem.setAccessibilityIdentifier("menu-edit")
    let editMenu = NSMenu(title: "Edit")
    let selectAll = NSMenuItem(title: "Select All", action: NSSelectorFromString("selectAll:"), keyEquivalent: "a")
    selectAll.keyEquivalentModifierMask = [.command]
    selectAll.target = probe.menuRoute == "explicit_target" ? probe : nil
    selectAll.setAccessibilityIdentifier("menu-select-all")
    editMenu.addItem(selectAll)
    installMenuVisibilityProbe(menu: editMenu, identifier: "edit")
    editItem.submenu = editMenu
    mainMenu.addItem(editItem)
}

func prepareKeyboardSelectionProbe() {
    keyboardSelectionProbe?.prepare()
}
