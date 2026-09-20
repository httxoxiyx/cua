# Explicit Chrome background DOM text channel

An explicit DOM replacement route was qualified on a Driver-owned isolated
Chrome profile and a local form. It is not universal background typing,
personal-profile access or native/trusted keyboard delivery.

## API and authorization

`browser_type` declares `input_route: trusted | dom_event`.
`trusted` remains the default. On standalone macOS Chrome, trusted typing
checks the same background limitation as trusted clicking before DOM focus
or keyboard input.

The explicit `dom_event` route requires:

- `mode=insert_text` and explicit `replace=true`;
- an exact authorized target/tab and a fresh ref with frame/loader identity;
- a connected, editable HTML text input or textarea.

It does not support contenteditable, file/number controls, caret insertion or
keystrokes. Existing consumer-profile consent and endpoint policy remain
unchanged. In particular, this change does not add `Page.createIsolatedWorld`
to the existing-profile method allowlist. An isolated profile does not inherit
personal cookies or authenticated sessions.

## Mutation and receipt

The mutation lock and target/frame authorization remain in force. The node is
resolved in a frame-bound isolated world and its loader identity is rechecked.
The fixed function receives text as a JSON argument, never as interpolated
script source.

It calls native value setter/getter and event dispatch methods, emits one
synthetic input and one change event, then reads the value back. It does not
focus, scroll, send keyboard input, activate a window or submit a form.
Page event handlers still run; arbitrary websites are not guaranteed to preserve
foreground posture or accept the change.

A matched value is not application success. The internal result remains
`effect: unverifiable` with `input_trust: dom_event`. The public action contract
projects this to `route: dom`, `effect: unverifiable`,
`delivery.mode: background`. Consumers must verify the requested page
postcondition separately.

After a possible dispatch, timeout, exception, malformed reply or mismatched
readback yields a non-retryable unknown outcome. No input replay, reconnection
or native keyboard fallback is performed.

## Controlled qualification

The passing quiet trial independently verified navigation, Unicode field
replacement, one explicit DOM submit and the resulting URL. The recipient
recorded input/change once each as untrusted events, no keyboard or focus-in
events, exactly one form submission and an unchanged sibling field.

A foreground sentinel recorded 96 samples over approximately 1.92 seconds with
no change in foreground PID, active/key status, cursor position, input,
resign counters or tracked window order. Setup and teardown are outside that
measurement. Owned browser, fixture and candidate processes exited afterward.

Earlier attempts remain unsuccessful: one consumer incorrectly expected
internal fields in the closed public receipt, and another functional run failed
its strict no-interference checks. Neither was converted to a pass after a
harness correction.

Native mock-backed tests cover route declaration, current frame/loader identity,
trusted-route refusal, stale refs, malformed results, unknown outcomes and
producer-to-public-contract projection. They complement but do not replace
device acceptance.

The local adapter, raw logs and browser profiles are not shipped here. This
does not add marketplace tools, modify a personal Chrome debugging setting or
solve [native background Return](macos-background-return-20260919.md).
