# macOS background keyboard responder context

Research and implementation snapshot: 2026-09-14.

## Reproduced failure and causal controls

These were disposable AppKit experiments against installed Driver commit
`e8327c62ddd68fea66cc7bc30c736a55af6c6b7f`. No user Pages document was modified.
The final experiment used an external, bounded test-only routing guard around
the unchanged installed Driver hotkey call; it was not a test of the subsequently
integrated production candidate.

| Experiment | Actual result |
| --- | --- |
| Explicit-target Select All menu, authenticated background Cmd+A | The callback executed while the target was inactive and selected all 32 UTF-16 units. This was a positive transport control, not a faithful responder-chain test. |
| Standard nil-target `selectAll:` menu | Cmd+A arrived, but the app had no key/main window and selection remained empty. The field editor was still its window's first responder. |
| Target-only synthetic app activation | The app became internally active, but key/main remained absent and selection remained empty. |
| Target-only activation plus the existing paired exact key-window records | The same authenticated Cmd+A selected the entire field. The real foreground sentinel remained active and received no key or focus-loss events. Cleanup returned target active/key/main state to the background baseline. |

The last experiment addressed PID 36610/window 20669 with foreground sentinel
PID 37685/window 20679. Its measured interval was monotonic
346441.772862833–346443.478459875 seconds. The target received one key-down and
one key-up; `NSTextView.didChangeSelectionNotification` reported location 0,
length 32 at 346442.4028667917. The target's key/main windows were again absent
at 346443.43215475004, and a subsequent heartbeat at 346443.56756887503 confirmed
inactive state, absent key/main windows, and the retained full selection.

The sentinel journal contained only heartbeats over the enclosing
346441.7–346443.8 interval: eight heartbeats, no key/modifier event and no
resign-active/key/main notification. Its selection remained empty. These are
continuous fixture callback journals, not a claim inferred from sparse images.
They do not establish arbitrary application, OS-version, or cross-task safety.

Local diagnostic artifacts are under `/tmp/cua-pages-input-probe.62hwkb/`:
`keyboard-setup.jsonl`, `keyboard-responder.jsonl`,
`target-focus-experiment.jsonl`, `key-window-experiment.jsonl`,
`keyboard-focus-target.jsonl`, and `keyboard-focus-sentinel.jsonl`.
`/private/tmp/cua-keyboard-target-focus.CvZCXk/` retains the source-slice manifests,
bounded probe programs, and single-operation orchestration. These paths are
supplementary local receipts, not a requirement for understanding the result.

## Narrow production change

Background `hotkey` and `press_key` retain their existing exact-target gates,
same-PID ambiguity checks, mutation lease, and authenticated PID event transport.
For a supplied exact window that is not genuinely foreground, they now acquire
target-only synthetic context, establish that context's key window using the
same paired-record helper as the existing foreground implementation, dispatch
once, and end the target-only context. They never call the foreground setter.
Explicit field focus is reapplied inside the key-window context before input.

The generic sequencing helper takes a `FnOnce` action and has no retry or
foreground-activation operation. Context Drop handles preparation failure and
panic/unwind. Explicit cleanup also runs after dispatch failure. Cleanup failure
is not success. Genuine user activation of the target supersedes synthetic
deactivation, preserving the pre-existing production cleanup rule.

The existing 40-ms begin, key-record and end intervals are retained. The key
operation receives a minimum 40-ms dispatch/settlement interval; an existing AX
oracle wait counts toward it instead of adding another delay. No new long wait
or polling loop is introduced. Native OS calls are not hard real-time bounds.

PID-only legacy calls and explicit foreground delivery remain on their existing
routes. `type_text` and its possible-AX-effect/no-replay handling are unchanged.
An accepted hotkey remains `effect: unverifiable`; it no longer turns unknown
completion into an automatic foreground/replay recommendation. Fresh state and
focused-field verification precede any next action.

## Focused tests and remaining validation

`input/background_keyboard.rs` has eight pure sequencing tests: setup order,
already-foreground bypass, begin refusal, key-window setup failure, dispatch
failure, cleanup failure, preparation panic, and dispatch panic. The initial
direct-dispatch implementation produced 7 failures/1 pass; the implemented
sequence produced 8 passes/0 failures. This unit RED describes the new helper's
initial direct-route implementation; the installed-baseline GUI RED above is
the independent native reproduction.

The tests were compiled directly from that owning source without a replacement
suite or a platform build:

```sh
rustc +1.97.1 --edition 2021 --test \
  /private/tmp/cua-keyboard-target-focus.CvZCXk/sequence-tests.rs \
  -o /private/tmp/cua-keyboard-target-focus.CvZCXk/sequence-tests-green-final
/private/tmp/cua-keyboard-target-focus.CvZCXk/sequence-tests-green-final
```

The ignored `harness_appkit_background_command_a_selects_text` test uses the
standard responder-chain menu, actual selection notifications, and the existing
`run_with_background_oracles` focus/z-order/cursor/no-input-leak checks. It also
requires post-action target key/main/active cleanup. The separate explicit-target
positive control is not a substitute. Target-local AppKit activity is expressly
distinguished from the real foreground process.

## Integrated candidate validation

The main operator subsequently completed the full native release build and
native release test runs: `application_menu` 22/22 and `background_keyboard`
8/8 passed. The operator launched the isolated, source-marker-verified candidate
with standard permission handling (PID 7838), without replacing the user's
running Driver. These are candidate results, not a released-version claim.

The integrated candidate then handled two separately timed, whole-CLI calls
without the external target-focus probe:

| Call | Complete CLI interval (monotonic seconds) | Actual target effect |
| --- | --- | --- |
| Background `press_key` Right | 347157.318–347158.612 | Full selection collapsed to location 32, length 0 at 347157.4614187917. |
| Background `hotkey` Cmd+A | 347166.311–347167.580 | Selection became location 0, length 32 at 347166.45890675. |

Each call produced one key-down and one key-up in target PID 36610/window
20669. After each action, target heartbeats confirmed inactive state and absent
key/main windows. The real foreground remained sentinel PID 37685 throughout
both complete CLI intervals. Its journal contained respectively five and six
heartbeats only, with no key/modifier event or resign-active/key/main event;
its selection stayed empty. Thus the measured intervals cover CLI setup and
preflight as well as the new native dispatch, not merely the key event itself.

Receipts are `candidate-right-timed.jsonl` and
`candidate-select-all-timed.jsonl` alongside the two continuous fixture journals
listed above. The selection and cleanup claims come from actual field-editor
notifications and subsequent state heartbeats, not from the tool's posting
acknowledgement. The tool appropriately remains unverifiable without its own
application-effect oracle.

An earlier 72-ms sentinel key-window interruption is retained as an unresolved
anomaly: resign-key at 346940.26867712504, become-key at 346940.34065983334.
The sentinel stayed application-active and received no input. The target did
not begin internal activation until 346944.632838375. The earlier call's full
start boundary was not captured, so neither causation nor exclusion from that
call is established. The successful timed repetitions do not erase this event
or establish that its cause has been fixed.

This closes the focused integrated AppKit responder-chain replay, not an actual
Pages test, complete desktop matrix, or cross-platform certification. Separate
explicit-element, Chromium/Electron, error-cleanup and same-PID ambiguity live
regressions remain necessary before claiming those surfaces are covered. No
user Pages document was used; do not infer Pages-specific behavior from the
disposable fixture alone.
