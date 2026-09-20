# macOS development qualification — 2026-09-20

This source snapshot is qualified for the bounded Apple Silicon development
scenarios below. It is not a public binary release or universal cross-platform
acceptance. Publishing it does not install a Driver, migrate settings, modify
permissions or enable experimental input routes.

## Included work

| Area | Status and boundary |
| --- | --- |
| Exact native text-field clicks | Addressed controls without an advertised AXPress use exact-window routing; stale/sibling targets remain refusals. |
| Implicit `type_text` | Requires an eligible, observable native AX insertion target; skips misleading read-only/web AX rungs before a possible write. Uncertain insertion is not replayed. |
| Foreground activity and lifecycle | Fixed activity leases, live transport ownership, retained blocking workers, owned key/button cleanup and exact restoration; see [coverage and remaining cases](foreground-activity-local-candidate.md). |
| Foreground batch segments | Bounded native cross-RPC ownership and cancellation/EOF settlement; does not silently authorize or enable foreground fallback in clients. |
| Browser DOM replacement | Explicit, synthetic, frame-bound replacement; qualified on an owned isolated Chrome/local form, not personal-profile or trusted keyboard input. See [DOM channel](macos-chrome-background-channel-20260919.md). |
| Multi-app/exact-window PiP | Controlled dual-window acceptance plus operator-confirmed Cisco/Chrome handoff. Source-foreground auto-hide remains. See [PiP details](pip-multi-app-exact-window-20260920.md). |
| Native Chrome background Return | Unresolved and paused. Experimental code is default-off; [investigation boundary](macos-background-return-20260919.md) is retained explicitly. |

## Publication checks

The full selected unit suites ran on macOS Apple Silicon with the pinned Rust
1.97.1 toolchain, locked/offline dependencies and the release profile:

| Suite | Passed | Ignored |
| --- | ---: | ---: |
| `cua-driver-core` library | 661 | 0 |
| `pip-preview` library | 34 | 0 |
| `platform-macos` library | 770 | 2 |
| `cua-driver` binary unit tests | 220 | 0 |
| Total | 1,685 | 2 |

All selected suites had zero failures. The total uses each suite's final
top-level result, not the Driver tests' nested subprocess summaries. Ignored
tests were not run and are not counted as passing. Formatting and the native
release build also passed. The complete Rust source inventory was unchanged
across these checks and matched the source inventory of the locally exercised
PiP build; no runtime source changes were made during publication cleanup.

From `libs/cua-driver/rust` on a configured macOS development host:

```sh
cargo fmt --all -- --check
cargo test --locked --offline --release --target aarch64-apple-darwin -p cua-driver-core --lib -- --test-threads=1
cargo test --locked --offline --release --target aarch64-apple-darwin -p pip-preview --lib -- --test-threads=1
cargo test --locked --offline --release --target aarch64-apple-darwin -p platform-macos --lib -- --test-threads=1
cargo test --locked --offline --release --target aarch64-apple-darwin -p cua-driver --bin cua-driver -- --test-threads=1
cargo build --locked --offline --release --target aarch64-apple-darwin -p cua-driver --bin cua-driver
```

Offline commands require dependencies already available locally. Use a native
Terminal/build environment with the platform prerequisites; do not weaken
sandbox or permission settings to force a build. Existing duplicate Swift
bridge linker warnings are not resolved by this change.

## Evidence limits

No GUI or real authentication was repeated as part of the publication checks.
The PiP and input notes distinguish prior automated fixture evidence from
operator confirmation. No claim is made that a dispatch acknowledgement alone
proves typing, navigation, authentication, submission or successful cleanup.

Machine-specific logs, screenshots, browser profiles, local startup scripts,
signing material and installation backups remain outside the repository.
Windows/Linux GUI qualification and the canonical full E2E matrix were not
rerun on this Mac; platform source and CI lanes remain in place.
