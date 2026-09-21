# macOS multi-display cursor overlay

## Repair

The previous overlay created one window from `NSScreen.mainScreen` at startup,
painted with a zero origin, and treated negative X coordinates as an unplaced
cursor. An application on a display above or left of the primary display could
therefore receive input while its cursor animation remained on the primary
display or was invisible.

The overlay now owns one transparent, click-through, normal-level window per
display. Each window uses that display's AppKit frame; its raster uses the
matching CG/AX global origin and backing scale. The shared render state tracks
first placement explicitly, so negative coordinates, including `(-200, -200)`,
are ordinary desktop positions. Focus highlights use the same origin, and a
session badge is clamped only inside the display containing its cursor anchor.

Display-change notifications refresh the layout on AppKit's main thread.
Disconnected windows retire there; queued frames and ordering work carry display
IDs and layout generations, not raw window pointers. A bounded latest-frame
mailbox replaces obsolete image batches. Cursor state advances once per tick,
irrespective of the number of displays. Session ownership, removal tombstones,
arrival waiters and the existing target-relative z-order policy are preserved.

The Windows, X11 and Wayland adapters use the same explicit placement flag;
their input routes and surface-management implementations are not changed.
No keyboard/foreground routing, PiP, plugin, permissions or launch configuration
is modified by this repair.

## Verification

On macOS Apple Silicon, Rust 1.97.1, locked/offline dependencies:

- Shared cursor library: **53 passed**.
- Native macOS library, release profile: **779 passed, 2 ignored, 0 failed**.
- Full workspace formatting check and diff whitespace check passed.
- Fourteen new unit tests cover negative coordinates, translated cursor/focus
  pixels, badge isolation, mixed scales, gaps, display removal/reconnection,
  stale-frame rejection, bounded frame retirement and per-session animation.

An independent draw-only native process also passed on the reported layout:

| Display | Global CG bounds in points | Native layer image in pixels |
| --- | --- | --- |
| Primary | `0, 0, 1728, 1117` | `3456 × 2234` |
| Above/left extension | `-192, -1080, 1920, 1080` | `3840 × 2160` |

The native smoke test checks settled WindowServer bounds, click-through windows,
nonempty pixels in the intended window's actual `CALayer` contents, no leaked
cursor/badge pixels on other displays, visibility acknowledgement and clearing
after cursor removal. It enqueues decorative movement but posts no input events.
The sampled frontmost application remained unchanged. The temporary process
exited; the installed Driver was neither replaced nor restarted.

This is native window/layer evidence, not a screenshot of another application
or an end-to-end Muse task. Physical hotplug, rearrangement during animation,
mirroring and full-screen/Spaces transitions remain manual acceptance cases.
Windows/Linux native tests were not run on this Mac.

After the locally signed patched build was installed, the reporter confirmed
that the original extended-display issue was resolved in their test. This is
user-reported acceptance of the reported case, not verification of the additional
hotplug, mirroring or full-screen/Spaces cases above.

The debug-profile full macOS suite exposed three pre-existing Return-constructor
tests affected by Objective-C `CGEvent` pointer-encoding checks (one mismatch
and subsequent poisoned test locks). Their source was not changed; the full
release suite passed with no skipped failures. Existing duplicate Swift bridge
linker warnings remain. Build logs and machine-local helpers stay outside the
repository.

## Reproduce

From `libs/cua-driver/rust` in a configured, logged-in native macOS Terminal:

```sh
cargo test --locked --offline --release --target aarch64-apple-darwin -p cursor-overlay -p platform-macos --lib -- --test-threads=1
cargo run --locked --offline -p platform-macos --example cursor_multi_display_smoke -- --show-for-test
```

The smoke test requires at least two displays and exits within 20 seconds. It
does not connect to a Driver socket, move the hardware pointer, capture other
applications, change permissions or restart a service. A non-test invocation
only prints usage. Do not change build or OS security settings to force a run.

After explicitly installing a build containing this repair, validate a
non-sensitive Muse task on each display, move the target window between displays,
and verify the cursor and focus highlight follow it. Then check display
rearrangement, scale changes and unplug/replug. Confirm session cleanup and PiP
still work. Source verification alone does not upgrade the running Driver.
