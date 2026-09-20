# PiP multi-app retention and exact-window activation

## Behavior

A session retains one preview per observed application instead of replacing
its previous app at each handoff. A VPN-to-browser flow can retain both cards.
The existing five-app bound, dead-target handling, explicit hiding and
session-end cleanup remain in force.

Multiple windows of one app share one card. Its preview follows the latest
exact observed window. A click binds to the presentation actually rendered,
not newer unrendered model metadata or the app's generic main window.
Window, session, delegation or layout changes invalidate a stale click.

Intentional user activation does not use all-window activation. It makes the
exact native window key, raises the matching AX window, then independently
checks foreground process, AX focused-window identity and visible window order.
The work runs off the AppKit UI thread, has bounded checks and is not replayed
after an uncertain result. Metadata-only status is available in
`pip_runtime.last_card_activation`.

Source-foreground suppression is unchanged: an app's own card hides while that
app is foreground and returns when it is background. Other retained cards are
not retired. This is not an always-visible overlay for foreground apps.

## Implementation

Under `libs/cua-driver/rust`:

- `crates/pip-preview/src/lib.rs`: per-session/per-app retention and cleanup.
- `crates/platform-macos/src/pip/mod.rs`: rendered-click targeting and lifecycle.
- `crates/platform-macos/src/pip/window_activation.rs`: exact-window activation,
  cancellation and independent confirmation.

This repair does not qualify native background Return or enable its experiments.

## Acceptance

34 shared PiP and 75 native PiP tests passed in the original qualification.
A signed private candidate was then tested with a local VPN stand-in,
a foreground sentinel and two windows in one isolated Chrome process.

Both app cards were simultaneously visible and streaming. Observing Chrome's
sibling and original windows changed the same card's exact target. With the
sibling initially focused/frontmost, one real PiP click changed both AX focus
and visible window order to the pictured original window; the native activation
receipt was confirmed.

Returning to the sentinel restored both cards. Ending the session removed its
cards. The isolated browser, fixtures and candidate exited. Failed earlier
setup attempts were retained as failures, not counted as acceptance.

After installation, the operator tested an actual Cisco Secure Client to
Chrome/Okta handoff through Muse Code and explicitly confirmed both retained
cards and correct Chrome-window activation. The later screenshot displayed
Chat because the operator had switched back to Chat, not because the PiP had
selected a different window. This is operator-confirmed device acceptance,
not an automated authentication test or proof of VPN connectivity.

## Manual regression checklist

1. Use a non-sensitive cross-app task and keep both apps in one Computer Use
   session. Do not modify unrelated windows or tabs.
2. Observe the exact browser window while a sibling window exists in the same
   process. Move both source apps to the background and confirm both cards.
3. Click the browser preview once. Verify that the opened window matches the
   pictured content, not merely the same application.
4. Put the source app in the background again; confirm its card returns.
5. End the session and confirm its cards retire.

Pause for credentials or MFA when testing an authentication flow. Never infer
successful authentication from PiP state.

Delegated authentication panels, unusual full-screen/Spaces layouts and rapid
simultaneous clicks remain outside this bounded acceptance. Build manifests,
signed binaries, private installation backups, screenshots and machine-local
launchers are intentionally not published; source publication does not reinstall
or restart an existing Driver.
