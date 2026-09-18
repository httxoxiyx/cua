# Background application-menu AX actions on macOS

Normal window observations include the owning application's menu bar. Menu
items can lack `AXWindow` ancestry, so exact-window mutation previously refused
controls that the observation intentionally exposed.

The menu route is a narrow semantic exception, not application-wide mutation
authority. It requires all of the following:

- A current snapshot token resolves the retained element in the observed
  application/window/runtime scope.
- The target window still exists, belongs to that PID and is present in its AX
  windows. Existing modal and delegated-helper guards still apply.
- The element is an `AXMenuBarItem` or `AXMenuItem`, with no conflicting window
  ancestry. Every member of its bounded parent chain belongs to the same PID,
  has a native menu role, and remains in its parent's current children. AX
  identities are compared with `CFEqual`, not proxy addresses or labels.
- The chain reaches the application's freshly read `AXMenuBar` and that
  application's `AXFocusedWindow` still matches the observation window. This
  is app-local document context, not the globally foreground application.
- The requested semantic action is supported and currently advertised. The
  context proof and enabled state are checked again before dispatch.

There is no application activation, window raising, coordinate/selection
fallback, keyboard fallback, or generic `set_value` exception. A detached menu,
changed document context, unresolved focus, foreign element, stale token or
unsupported action fails closed. The existing foreground `invoke_menu` route
is not silently called. AX acceptance remains `unverifiable`, not an invented
document-effect receipt.

The per-PID mutation lease serializes Driver actions; it cannot freeze a user's
actions inside another process. Fresh pre-dispatch checks narrow that race but
do not claim an atomic observe-act transaction.

Generic-key refusal advice may recommend the existing explicit foreground
route for minimized/hidden or ambiguous same-PID keyboard targets. The Driver
still refuses the background operation and does not retry; caller policy owns
whether foreground delivery is authorized. Text insertion retains its separate
semantic-attempt accounting and is not made safely retryable by this advice.

## Validation

Headless tests cover semantic-only authorization, unchanged exact-target
refusals, menu identity/ownership, detached members, cycles and changed or
unknown app-local focus. The AppKit fixture's existing `menu-test-item` now has
a harmless counter callback. The ignored
`harness_appkit_application_menu_background` row checks stale-token/value-write
refusal and one callback under the existing foreground/key-window/pointer
sentinel oracles.

Unit tests and a compiled fixture do not prove real macOS delivery. Run the
canonical logged-in, TCC-authorized Lume harness before claiming desktop
certification; no workstation GUI run or Driver replacement is implied by
these source changes.

### Local supporting evidence (2026-09-14)

- `cargo test -p cua-driver-core --locked --offline`: 609 unit tests,
  2 contract tests and 3 lifecycle tests passed; one existing doc example
  remains ignored. The focused background-input owner contains 16 tests.
- The two new menu policy assertions first failed before the route was enabled.
  The generic-key advice assertion likewise first failed with the old
  accessibility advice. The menu graph's two positive assertions failed before
  its membership proof was implemented.
- A standalone `rustc --test` probe including the actual `ax/bindings.rs` and
  `ax/application_menu.rs` compiled against the built Core Foundation library;
  all 8 menu proof/action tests passed. It did not call live AX APIs and is not
  a successful build of the entire platform crate.
- `tests/fixtures/build/macos.sh --only appkit` compiled the fixture. It was
  not launched.
- The platform test build stopped in existing `apple-metal`, `apple-cf` and
  ScreenCaptureKit SwiftPM build scripts with
  `sandbox-exec: sandbox_apply: Operation not permitted`. No sandbox bypass or
  dependency substitution was used. Complete platform compilation, the new
  ignored GUI row, and canonical desktop certification remain unverified.
