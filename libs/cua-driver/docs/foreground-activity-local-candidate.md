# macOS foreground input and lifecycle protection

This describes the development implementation and the bounded local acceptance
completed in September 2026. It is not a claim of universal background input or
a completed cross-platform GUI matrix. See the [qualification summary](macos-qualification-20260920.md)
for the current publication boundary.

## Single-call protection

- A foreground episode requires a reliable native activity monitor and five
  seconds of continuous idle time. Its original activity generation and
  120-second deadline cannot be renewed by later idle observations.
- Requests retain exact process/window authority. Blocking workers retain their
  AX objects and request ownership even if the asynchronous waiter is cancelled.
- Every input boundary rechecks activity, session and transport ownership.
  Cancellation, user intervention, expiry or owner loss prevents further input.
- Cleanup releases only keys/buttons whose down transition belongs to that
  operation. Matching releases are prepared before down events; unrelated user
  input is never released.
- Normal restoration targets the exact original window. An interruption does
  not reclaim focus from the user. AX write uncertainty remains a cleanup
  failure, not permission to retry or claim restoration.
- Owned script helpers are reaped before cancellation or timeout completes.
  Worker exit, transport exit, input settlement and application success are
  separate facts.

## Cross-RPC foreground segments

Native `begin_foreground_segment` and `end_foreground_segment` support a
transport-managed foreground batch. They do not automatically authorize a
public wrapper to enable foreground fallback.

A segment binds a fresh opaque ID to the trusted runtime/session/transport,
exact PID/window, original foreground window and one activity generation.
Begin reserves ownership but does not activate a window or send input.
The fixed limits are 120 seconds, 20 mutations and 64 total calls. Observations
do not renew the lease, and calls are serialized.

A textual session name or copied token cannot replace the live transport
capability. A request admitted before control-channel EOF sees revocation;
one arriving afterward cannot recreate the ended owner. Native worker tickets
keep the reservation until the workers actually exit.

Activation occurs only when an admitted action needs it. Normal finalization
restores once. Cancellation, user takeover, owner loss or expiry permanently
revokes admission. Unknown cleanup blocks further GUI work and is not replayed.

The MCP proxy advertises its cancellation contract: a matching
`notifications/cancelled` retires that transport, closes its control owner and
waits for the outstanding native response within a fixed settlement deadline.
A timeout, missing response or malformed result is not proof of cleanup.
Unrelated request IDs do not cancel the active request.

## Input compatibility

Implicit `type_text` no longer attempts an AX insertion into a read-only display,
button or unproven editable field. Eligibility requires an editable role,
settable selected text and a readable before-value on the same retained object.
Implicit web-content AX echoes are not renderer input witnesses and skip that
rung before a possible write. Explicitly addressed elements retain their
existing behavior.

An uncertain AX write is never replayed through keyboard synthesis. Where
keyboard routing is ambiguous, the existing exact-window refusal and guarded
foreground path remain in force. This does not solve arbitrary Chrome native
background Return.

## Local acceptance and limits

Historical controlled single-call tests established value-without-submit,
one foreground Return, exact same-app origin restoration, modifier release and
interruption by a real user takeover. Separate isolated Chrome tests established
one supported background Return case, refusal of an ambiguous two-window
background case, and one exact-window foreground fallback with an unchanged
sibling.

Later native segment tests established normal finalization, active-request
cancellation and active-request EOF without replay. They are not interchangeable
with single-call evidence or public-wrapper acceptance.

A subsequent segment human-takeover run recorded interruption and safe cleanup
but failed its harness clock comparison. That report remains failed; correcting
the harness did not retroactively qualify it. Between-call EOF settlement,
deterministic restore-phase cancellation, Secure Input/lock transitions and the
full GUI matrix still require separate coverage.

Detailed machine-local journals and one-shot launchers are intentionally not
published. They must not be replayed or treated as a release installer.
