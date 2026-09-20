# Native Chrome background Return: paused investigation

As of 2026-09-20, reliable, fully background native Chrome Return remains
unresolved. The investigation is paused. Publishing its source and regression
tests does not qualify it for production or turn it on.

## Deployment boundary

All experimental paths require explicit environment opt-ins and remain off in
the normal installed configuration:

- `CUA_EXPERIMENTAL_CHROME_BACKGROUND_RETURN`: target-local synthetic context
  experiment.
- `CUA_EXPERIMENTAL_CHROME_WINDOW_RETURN`: independently window-tagged Return
  experiment with an already-focused exact native field.
- `CUA_EXPERIMENTAL_CHROME_RETURN_TRACE`: bounded metadata-only phase diagnostics;
  also requires the window-Return opt-in.
- `CUA_EXPERIMENTAL_CHROME_RETURN_PUBLIC_POST`: a fixed public-PID-post contrast
  within the window-Return experiment, not an automatic fallback.

Do not set these in production startup scripts. The existing ordinary input
gates and guarded foreground fallback remain unchanged by the opt-ins.
The [explicit DOM channel](macos-chrome-background-channel-20260919.md) is a
different application-level mechanism, not evidence of native Return delivery.

## Established findings

The private authentication factory is an Objective-C class method. Its guard
must use class-method lookup rather than checking instance methods on the class.

Native event construction can preserve the exact window number, Return key
code and carriage-return character. Construction, authentication and successful
posting API calls do not prove the browser received the event.

AX field ancestry can establish exact window membership when a direct AXWindow
attribute is explicitly absent. It must still independently prove native focus;
DOM focus is not a substitute. Conflicting identity, malformed values, foreign
processes, sheets, cycles and read errors remain refusals.

Character readback touching native TIS/TSM facilities needs a main-thread
construction broker. The broker is bounded and does not create observer UI or
enable an experimental route without its opt-in.

## Trial outcomes

Earlier target-local context trials either refused before dispatch or posted
one pair without verified navigation; uncertain AX cleanup was preserved as a
failure. No uncertain attempt was replayed.

The final window-tagged phase-trace and public-PID-post trials each proved one
down/up posting pair and settlement, but the recipient page recorded zero
keyboard events and zero submissions. Both failed delivery qualification.

The foreground sentinel remained stable during those measured action
intervals. An additional owned Chrome window nevertheless failed the strict
full-window-order oracle; its cause was not established. No foreground change
is not equivalent to successful delivery.

Experiments used isolated, owned browser profiles and inert local pages.
Personal profiles, real authentication and permissions were not modified.
Owned trial processes were cleaned up. Detailed local logs, page traces,
profile data and consumed one-shot launchers are not part of this repository.

## Requirements before resuming

A new trial needs recipient-side evidence or a concrete observation-side
hypothesis. Retain the exact target and activity guards, one-attempt semantics,
owned matching release, independent focus/window-order oracle and explicit
cleanup result.

Do not infer success from API dispatch, replay uncertain input, weaken targeting
to a PID-only claim, or add an automatic foreground fallback to an experiment.
The pause is intentional; the working input/PiP baseline is separately qualified.
