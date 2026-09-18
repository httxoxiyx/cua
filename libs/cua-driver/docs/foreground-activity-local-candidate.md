# Foreground activity local candidate

This candidate prepares bounded background-input protection. It does not
finalize design decisions or establish installed-host acceptance. Manual
validation remains required.

## Implemented protection

- Shared content-free activity state requires five seconds of continuous
  monitoring. Gaps, missing permission, Secure Input and session loss invalidate
  evidence. Generated Driver events do not count as human input; other events
  are conservatively classified as unknown provenance.
- The macOS adapter uses a listen-only event tap, verifies its effective mask,
  and recovers monitoring after temporary environmental failures. It does not
  request permission or retry task input on a timer.
- Synchronous foreground episodes guard the exact target, check between input
  units, release pressed input after interruption, and restore only an exactly
  identified original window after successful uninterrupted completion.
  Drag cleanup releases at the last dispatched position. Its preallocated
  release events are bounded by the existing 200-step / 10000-ms limits.
- Background suppression requires reliable activity coverage, without requiring
  user idle elsewhere. A new external event or gap revokes restoration. Exact
  restore submission avoids reentering the suppression dispatcher mutex.
- Launch no longer has repeated or detached delayed demotion. Persistent
  activation, menu invocation, desktop input, Finder folder handoff and
  interactive persistent-foreground paths are refused where the required
  bounded lifecycle is unavailable. Read-only tools remain available; input
  and launch can refuse when monitoring coverage is unavailable.

## Deliberate capability hold

`get_config.foreground_activity` reports dynamic monitoring state, but
`native_dispatch_guard`, `native_cleanup`, `exact_window_restore` and
`batch_foreground_segments` remain **false**. A consuming wrapper that enforces these capability flags therefore
refuses automatic foreground input, including drag, semantic open and
refusal-only fallback. These flags are not user-configurable switches.

Cross-RPC batch foreground segments, atomic observation recovery, complete
native cancellation/async ownership coverage, and the full cleanup/restoration
acceptance matrix remain incomplete. Do not flip capability flags
based only on a successful build or idle snapshot.

The manual follow-up caught a console-session lookup error: the SDK macro
`kCGSessionOnConsoleKey` expands to `kCGSSessionOnConsoleKey`, not its own name.
The adapter uses the SDK dictionary value. Native fixture tests admit a valid
logged-in console and continue refusing locked, logged-out or missing evidence.
Run them with `cargo test --release -p platform-macos --lib console_session_tests`.

## Validation and remaining acceptance

Five standalone shared-policy tests pass. `DOCS_RS=1 cargo check --offline
--locked -p platform-macos --tests` passes, including test-code type checking.
This mode skips native Swift bridges; it does not link or execute the Driver.
Ordinary native `cargo check` was blocked by SwiftPM's
`sandbox-exec: sandbox_apply: Operation not permitted` on this machine.

No Driver installation, permission changes, live event monitoring or GUI input
was performed. Full native build, real event attribution, Secure Input and lock
transitions, user takeover mid-key/mid-drag, and same-app exact-window restoration
require a manual validation session.
