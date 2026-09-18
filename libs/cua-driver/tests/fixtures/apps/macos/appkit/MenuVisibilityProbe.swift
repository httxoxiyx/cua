// Opt-in, fixture-owned menu lifecycle evidence. This observes AppKit callbacks;
// it does not open a menu, activate an application, or claim that pixels appeared.
import AppKit

private var menuVisibilityProbes: [MenuVisibilityProbe] = []

private final class MenuVisibilityProbe: NSObject, NSMenuDelegate {
    private let identifier: String
    private let output: FileHandle
    private var opens = 0
    private var closes = 0

    init?(identifier: String, path: String) {
        self.identifier = identifier
        if !FileManager.default.fileExists(atPath: path),
           !FileManager.default.createFile(atPath: path, contents: nil) {
            fputs("menu visibility oracle could not create its output file\n", stderr)
            return nil
        }
        guard let handle = FileHandle(forWritingAtPath: path) else {
            fputs("menu visibility oracle could not open its output file\n", stderr)
            return nil
        }
        output = handle
        super.init()
    }

    deinit {
        try? output.close()
    }

    func installed(on menu: NSMenu) {
        record("probe_installed", menu: menu)
    }

    func menuWillOpen(_ menu: NSMenu) {
        opens += 1
        record("will_open", menu: menu)
    }

    func menuDidClose(_ menu: NSMenu) {
        closes += 1
        record("did_close", menu: menu)
    }

    private func record(_ event: String, menu: NSMenu) {
        let entry: [String: Any] = [
            "event": event,
            "menu_identifier": identifier,
            "menu_title": menu.title,
            "menu_items": menu.numberOfItems,
            "open_count": opens,
            "close_count": closes,
            "unix_time": Date().timeIntervalSince1970,
            "uptime_ns": DispatchTime.now().uptimeNanoseconds,
            "pid": ProcessInfo.processInfo.processIdentifier,
            "is_active": NSApp.isActive,
            "key_window_number": NSApp.keyWindow?.windowNumber ?? -1,
            "main_window_number": NSApp.mainWindow?.windowNumber ?? -1,
            "frontmost_pid": NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1,
        ]
        do {
            var data = try JSONSerialization.data(withJSONObject: entry, options: [.sortedKeys])
            data.append(0x0A)
            _ = try output.seekToEnd()
            try output.write(contentsOf: data)
        } catch {
            fputs("menu visibility oracle could not append an event\n", stderr)
        }
    }
}

/// The harness supplies an explicit, task-owned output file. Ordinary fixture
/// launches have no delegate or logging change. Keep a strong reference because
/// NSMenu's delegate is weak, and never replace another fixture delegate.
func installMenuVisibilityProbe(menu: NSMenu, identifier: String) {
    guard let path = ProcessInfo.processInfo.environment["CUA_APPKIT_MENU_ORACLE"],
          !path.isEmpty else { return }
    guard menu.delegate == nil else {
        fputs("menu visibility oracle refused to replace an existing delegate\n", stderr)
        return
    }
    guard let probe = MenuVisibilityProbe(identifier: identifier, path: path) else { return }
    menuVisibilityProbes.append(probe)
    menu.delegate = probe
    probe.installed(on: menu)
}
