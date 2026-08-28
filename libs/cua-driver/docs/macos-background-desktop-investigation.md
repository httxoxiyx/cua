# macOS background desktop investigation

Status: **investigation — no production design has been selected**

## The question in plain language

We want an agent to work somewhere that is not the person's visible Mac
desktop. The person should be able to keep browsing, typing, and moving the
physical pointer while the agent opens apps, edits code, and tests its work.

This is not the same feature as [Agent View](agent-view.md):

- **Agent View is a viewer.** It shows windows and browser tabs that already
  exist. It does not create a new desktop or isolate input.
- A **background workspace** is where the agent's apps actually run. Agent View
  may show that workspace later, but the workspace must work without the viewer.

Agent View landed separately in #3431 and is opt-in through #3434. This document
does not redesign it.

## Two possible meanings of “background desktop”

The investigation must separate two levels instead of treating them as the same
promise.

### Level 1: dedicated virtual display

The agent gets an extra display inside the person's current macOS login session.
Its windows stay outside the physical display's bounds and Cua captures that
display for observation.

This may be enough for the intended experience, but it is **not an independent
desktop**. The displays still share one WindowServer session, active app, key
window, menu bar, keyboard stream, physical pointer, pasteboard, notifications,
and permission prompts. It can show a logical agent cursor inside the captured
image, but it cannot create a second operating-system pointer. We must measure
which agent actions can remain in the background rather than assume the display
creates isolation.

### Level 2: independent GUI session

The agent runs in another macOS graphical login session, most likely as a
different local user. This is the stronger design if the product requires its
own focus, input destination, and cursor rather than best-effort background
actions.

It is also much more expensive. It needs another user/session, per-user macOS
privacy permissions (TCC), a worker inside that session, authenticated
cross-session communication (IPC), and careful login/logout cleanup.

The current assignment requires the same login session, so Level 1 is the
candidate to test. If it cannot satisfy the product behavior, the result of this
investigation is a no-go under the current constraint. Level 2 is documented as
a comparison and may be pursued only after an explicit product decision to
relax the same-session requirement.

## Scope

- Phase A on a physical Apple silicon Mac;
- no virtual machine and no uvisor integration;
- keep the person and agent in the same macOS login session for the selected
  probe;
- investigation before adding a production CLI, SDK, MCP, installer, or helper
  contract;
- a tiny, reversible probe outside the core runtime until a path is selected;
- clear labels: do not call a virtual display an independent desktop; and
- a negative result is useful if macOS cannot provide the behavior safely or
  supportably.

Any live display or login-session probe must use a dedicated test Mac or
disposable test account. It must not change display topology, accounts, Remote
Management, TCC state, or login sessions on a developer's everyday workstation.

## What current Cua Driver already tells us

Cua Driver's [named sessions](../../../docs/content/docs/reference/cua-driver/process-model.mdx)
do not create OS isolation. They still share the same screen, keyboard, pointer,
Accessibility tree, and macOS focus. Agent View also works from existing exact
window and browser-tab captures; it is not a desktop provider.

Useful pieces already exist for an experiment, but they do not make the
virtual-display path work by themselves:

- the macOS platform layer can detect whether its process has graphical access;
- ScreenCaptureKit code already enumerates displays and captures content, but
  recording currently chooses the first display and desktop targets accept only
  `primary`; the probe needs exact display selection without changing the public
  target contract;
- exact-window placement can move and verify a test window at global display
  coordinates, and exact native/browser action paths can drive fixtures;
- the existing foreground sentinel has useful frontmost-app and input-sink
  pieces, but a new observer is required for continuous key-window, menu, Space,
  pointer, display-topology, and window-leak evidence; and
- Agent View's frame transport may be reusable later, but its current model has
  only native-window and browser-tab targets, not a desktop target.

There is no current implementation for creating or selecting a virtual display,
creating a second graphical login, placing a process in a foreign GUI session,
or relaying Driver traffic across users.

## Candidate A: same-session virtual display

### Why test it first

macOS includes private Objective-C runtime classes named `CGVirtualDisplay`,
`CGVirtualDisplayDescriptor`, `CGVirtualDisplayMode`, and
`CGVirtualDisplaySettings`. On macOS 26.6.2 with Xcode 26.5, the SDK's
`CoreGraphics.tbd` exports those four class symbols and local Objective-C runtime
inspection finds the expected creation/settings selectors. That proves only
that the names are present on one machine, not that the ABI or behavior is safe.
The probe must preserve the SDK path/version plus its `NSClassFromString` and
`instancesRespond(to:)` results. There are no public headers for this API, so
availability and compatibility remain experimental. Chromium maintains a
[test-only implementation](https://chromium.googlesource.com/chromium/src/+/c301827d24af4b3f60178b09db6c8c1f61ad5829/ui/display/mac/test/virtual_display_mac_util.mm)
using the same private API and documents removal reliability workarounds.

ScreenCaptureKit publicly supports selecting and capturing an `SCDisplay`. See
[ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit)
and Apple's
[macOS capture sample](https://developer.apple.com/documentation/screencapturekit/capturing-screen-content-in-macos).

This gives us a small first probe:

```text
person's current Aqua session
  physical display: person's apps
  private CGVirtualDisplay: agent test apps
  ScreenCaptureKit: selected virtual-display frames
  existing exact Driver actions: test input
```

### Probe A

Build a non-shipping macOS probe that:

1. Dynamically resolves the private classes and required selectors. If anything
   is absent, it exits without changing display topology.
2. Creates one uniquely identified virtual display and records its exact runtime
   display ID, descriptor identity, global bounds, and add/remove callbacks. The
   probe must not assume the numeric ID survives recreation.
3. Snapshots every existing display's ID, mode, origin, scale, mirroring, Space,
   and visible windows, then confirms the physical display remains the main
   display after attachment.
4. Finds an `SCDisplay` whose `displayID` exactly equals the new Core Graphics
   display ID. Opposing time-coded markers must prove that the virtual marker is
   present and the physical-display marker is absent from captured frames.
5. First tests fixtures that create their initial window directly within the
   virtual bounds. Ordinary app launch is a separate case: continuously watch
   every app-owned window, dialog, helper, and system prompt from first map so a
   one-frame flash or rehomed window on a physical display fails the case.
6. Drives repeated exact Accessibility, browser, and window-scoped input while a
   foreground workload simultaneously browses, types into a canary field,
   scrolls, navigates, and moves the physical pointer.
7. Stops actions, terminates and verifies all agent apps/windows, stops capture,
   and only then releases the virtual display. It must restore the exact baseline
   topology within a fixed timeout.
8. Separately kills the display owner while agent apps are alive and treats any
   rehomed window, leaked process, or unrecovered display as a failure.

The live test must continuously observe:

- foreground application PID and key window;
- menu owner and active Space;
- physical pointer position;
- foreground input delivery, ordering, latency, and final canary value;
- the “Displays have separate Spaces” setting and Space identity per display;
- every physical and virtual display ID, mode, bounds, and main-display identity;
- all pre-existing window frames across Spaces, z-order, Dock/menu placement, and
  a visual sentinel on each physical display;
- changing native and browser fixture state; and
- the capture backend, selected display ID, frame timestamps/freshness, and every
  action's requested and actual delivery route. Positive rows may use an
  allowlisted Accessibility, CDP, or exact window-local route only when the
  recorded actual delivery is background. Global HID (`MacosCgEventHid`),
  desktop/global fallback, foreground, unknown, or unrecorded delivery fails.

It must also exercise behavior that commonly escapes a target window: opening a
dialog, creating a new window, using a menu command, showing a permission prompt,
closing/reopening the app, and handling a crashed probe.

Freeze the row declaration and thresholds before the first live run. Give every
row one of these labels so results cannot be reclassified afterward.

**Positive steady-state rows — all must pass:**

- idle display attach/detach, capture-only, action-only, and combined activity;
- native Accessibility press and value change;
- exact browser click, type, and scroll;
- window-scoped pointer input only when its route claims it will not move the
  physical pointer or activate the target app;
- an in-app child window/dialog, app restart, normal teardown, and display-owner
  crash; and
- “Displays have separate Spaces” on/off, Stage Manager on/off, and a fullscreen
  foreground window.

**Characterization/onboarding rows — record disturbance, but do not relabel it as
a steady-state pass:** first permission prompts, menu commands, notifications,
login-item alerts, and arbitrary third-party first launch. Current Cua Driver
menu invocation is a foreground route, and a first permission prompt is expected
to create system UI. If either is a required product capability, Candidate A is
already a no-go for that capability unless a different non-foreground route is
designed and proved.

**Negative controls:** run the same actions without the virtual display and add
deliberate wrong-display, focus, input, pointer, system-UI, and window-leak
canaries. Every observer must prove it detects the failure it guards against.

The initial gate is zero unexpected foreground focus/key/menu/Space changes,
zero unexpected foreground-sink events, zero agent-caused visual/window/system-UI
changes on any pre-existing physical display, and zero agent-caused
physical-pointer movement during positive rows. The visual sentinel is the final
authority even when route metadata claims background delivery. Observer gaps must
stay at or below 10 ms during the overlapping workload. A time-coded 10 Hz fixture
must never be stale for more than 500 ms. Run every action row 20 times, run 10
clean create/teardown cycles, and restore the exact saved topology within 10
seconds each time. These are investigation thresholds, not a future product SLA;
change them only before a run and record the reason.

### How to classify the result

Candidate A passes as a **dedicated virtual display** only if agent windows stay
off the physical display, changing frames can be captured, cleanup is bounded,
both foreground and agent workloads complete correctly, and the tested agent
actions do not disturb the foreground observations.

It still must not be described as a separate session or independent cursor. Any
action that changes global focus, menu ownership, active Space, the physical
pointer, or the person's input destination is a product limitation, not
something the viewer can hide.

Candidate A is a no-go under the current same-session constraint if a required
action cannot avoid global activation/input, an agent-owned surface reaches a
physical display, an agent causes system-owned UI on a physical display during a
positive row, capture cannot be bound to the exact display, the private API is
absent on a supported system, or topology cannot be restored reliably.

## Candidate B: separate off-console GUI session (outside current scope)

This is a comparison, not an automatic next step. Test it only if the team
explicitly relaxes the same-session requirement after Candidate A is rejected.

Apple documents that Fast User Switching keeps multiple login sessions running
while only one receives the physical keyboard and mouse. See
[Fast User Switching](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPMultipleUsers/Concepts/FastUserSwitching.html).

Apple Remote Desktop also documents **Connect to a virtual display**. When the
administrator authenticates as a different user, that user gets a virtual
desktop while the person at the Mac continues working. See
[Choose how to control and observe](https://support.apple.com/guide/remote-desktop/choose-how-to-control-and-observe-apd4f46319e/mac).

This makes a different user's off-console Aqua session the leading Level 2
hypothesis. It is not yet a Cua architecture: Apple's UI does not provide a
documented public API for Cua to create, own, or embed that session.

Apple's
[High Performance Screen Sharing](https://support.apple.com/guide/mac-help/screen-sharing-type-options-mchl1883115d/mac)
is another supported baseline on Apple silicon with macOS 14 or later. It can
create virtual displays, but Apple says same-current-user authentication blanks
the hardware displays, and only one high-performance session is allowed. That
makes it a useful comparison, not a solution to the current same-session goal.

No documented public API has been identified that creates the required local
graphical session. `SessionCreate` changes a security session and
`launchctl asuser` adopts another user's launch context; neither operation alone
proves that an Aqua/WindowServer session was created.

### Probe B

On a dedicated test Mac with a disposable second user:

1. Establish Apple's documented Remote Desktop behavior as the baseline.
2. Record both users' UID, audit session, Core Graphics session, `onConsole`
   state, GUI bootstrap domain, and WindowServer connection.
3. Prove changing frames and input remain available while the person stays on
   the physical console.
4. Determine whether a local, noninteractive connection can establish and retain
   the same session without console switching.
5. Run an otherwise ordinary Cua Driver worker inside that GUI session.
6. Prove native and browser discovery, capture, and input see only that worker's
   graphical environment.
7. Exercise disconnect, crash, logout, sleep/wake, and repeat-start cleanup.

Negative controls must cover wrong-user credentials, expired credentials,
foreground-console frame leakage, replayed or cross-session relay messages, and
any fallback to a foreground worker. Independent focus, input, and cursor remain
hypotheses until these tests pass.

If viable, the likely boundary is:

```text
person's foreground Aqua session
  Cua host / optional viewer
             |
             | authenticated local relay
             v
agent user's off-console Aqua session
  ordinary Cua Driver worker
  agent apps and browser
```

Cua Driver's existing same-user Unix socket intentionally rejects a different
user. Candidate B therefore needs a deliberately authenticated broker or relay;
it must not weaken the existing peer-UID check or silently fall back to a
foreground worker.

Earlier `trycua/uvisor` work on off-console sessions is useful prior research,
but this investigation does not reuse its runtime or treat VM behavior as proof
for a physical Mac.

## Evidence required from either probe

A screenshot or successful API response is not enough. Record:

- physical Mac model and macOS build;
- exact source commit and signed artifact hashes;
- every display and GUI-session identity relevant to the candidate;
- continuous foreground focus, key-window, menu, Space, pointer, and input-sink
  observations;
- independently changing native and browser fixture state;
- capture target, backend, fallback, and frame-freshness evidence;
- TCC identity and every prompt;
- topology and process cleanup after normal and failure paths; and
- sanitized output with no unrelated windows, process arguments, Accessibility
  trees, credentials, or personal file names.

If Probe A is viable, the compatibility decision must cover the Driver's macOS
13 deployment target through the current macOS release, plus every architecture
distributed by Cua Driver. Private API presence on one current Apple silicon Mac
is not a product compatibility result.

## Definition of Done

- [x] Explain why Agent View and a background workspace are separate features.
- [x] Define the difference between a dedicated virtual display and an
  independent GUI session.
- [x] Map the relevant current Cua Driver capture, placement, input, sentinel,
  and transport primitives.
- [x] Define a small same-session virtual-display probe and falsifiable pass/fail
  checks.
- [x] Define a separate-user GUI-session probe as an explicitly out-of-scope
  comparison, not an automatic fallback.
- [ ] Run Probe A on a dedicated physical test Mac and publish sanitized results.
- [ ] Decide whether Probe A's measured isolation satisfies the product need.
- [ ] If Probe A is a no-go, record that result under the current same-session
  constraint.
- [ ] Run Probe B only after a separate product decision changes that constraint.
- [ ] Prove capture, input, foreground non-disturbance, and bounded cleanup for
  the selected candidate.
- [ ] Record API supportability, privileges, TCC, signing, account, and lifecycle
  requirements across the supported macOS matrix.
- [ ] Finish with a go/no-go recommendation and open an implementation RFC only
  if a viable architecture remains.

## Current recommendation

Start with **Probe A: a non-shipping `CGVirtualDisplay` experiment**. It is the
smallest test of the “virtual display for agents” idea and can reuse current Cua
Driver capture, window, input, and foreground-oracle code.

This is a go for investigation only. It is not evidence that macOS provides a
second desktop, independent operating-system pointer, or safe background input.
If ordinary agent work still changes the person's global focus or input
destination, record Candidate A as a no-go rather than hiding the behavior
behind Agent View. Candidate B requires a separate scope decision.

## Terms used above

- **Aqua session:** one logged-in macOS user's graphical environment.
- **WindowServer:** the macOS service that owns displays, windows, focus, and
  input routing for a graphical session.
- **TCC:** macOS privacy approval, such as Accessibility or Screen Recording.
- **IPC:** a local communication channel between processes.
- **Active Space:** the Mission Control workspace currently shown on a display.
- **GUI bootstrap domain:** the per-user launchd environment that owns graphical
  applications and services.
- **`onConsole`:** the system's indication that a login session currently owns
  the physical keyboard and displays.
