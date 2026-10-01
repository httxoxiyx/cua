# cua-driver scripts

Install, uninstall, local-build, and VM sync helpers for cua-driver.

| Script | Purpose |
| --- | --- |
| `install.sh` / `install.ps1` | Install released cua-driver binaries |
| `install-local.sh` / `install-local.ps1` | Build this checkout as the separate `cua-driver-local` product |
| `uninstall-local.sh` / `uninstall-local.ps1` | Remove only the source-built `cua-driver-local` product |
| `uninstall.sh` / `uninstall.ps1` | Remove installed driver artifacts |
| `_install-common.sh` / `_install-common.psm1` | Shared install helper logic |
| `_install-rust.sh` / `_install-local-rust.sh` | Rust build/install internals |
| `sync-vm-worktree.sh` | Sync this checkout to verification VMs and pull artifacts back |
| `post-install-hints.txt` | User-facing hints printed by install scripts |

## Stable macOS local signing

macOS Accessibility and Screen Recording grants are tied to an app's
designated requirement. An ad-hoc signature uses a `cdhash` requirement that
changes on every rebuild, so its grants do not survive the next local install.
The installer now reports whether the installed requirement is
`certificate-backed` or `ad-hoc cdhash`; an ad-hoc install always prints a
prominent warning and bootstrap instructions.

For behavior or E2E verification, require the stable path:

```bash
bash libs/cua-driver/scripts/install-local.sh \
  --release --autostart --require-stable-signing
```

`CUA_DRIVER_REQUIRE_STABLE_SIGNING=1` is the environment equivalent. Strict
mode stops before replacing the live app when no usable certificate-backed
identity is available.

For the most reliable non-interactive rebuilds, use a dedicated keychain:

```bash
SIGNING_KEYCHAIN="$HOME/Library/Keychains/cua-driver-signing.keychain-db"
security create-keychain "$SIGNING_KEYCHAIN"  # first time only
security set-keychain-settings "$SIGNING_KEYCHAIN"
security unlock-keychain "$SIGNING_KEYCHAIN"
export CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN="$SIGNING_KEYCHAIN"
```

To use an existing certificate without allowing the installer to select a
different identity from that keychain, also provide its exact SHA-1 fingerprint:

```bash
export CUA_DRIVER_LOCAL_SIGNING_IDENTITY="<40-hex-character SHA-1>"
```

The installer fails closed when that exact usable code-signing identity is not
present in `CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN`.

The first install creates `CuaDriver Local Signing (cua-driver-rs)` in that
keychain. If `codesign` cannot use its private key non-interactively, unlock
the keychain, trust the certificate in Keychain Access, and authorize Apple
code-signing tools:

```bash
read -r -s -p 'Keychain password: ' KEYCHAIN_PASSWORD; echo
security set-key-partition-list \
  -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASSWORD" \
  "$SIGNING_KEYCHAIN"
unset KEYCHAIN_PASSWORD
```

Then rerun the strict installer and grant Accessibility and Screen Recording
once. When the dedicated default keychain above exists, the installer prefers
it automatically; exporting `CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN` remains the
most explicit choice.

Older installers imported this identity with an all-applications private-key
ACL. The current installer no longer does that, but it cannot safely rewrite an
existing key's ACL without the keychain password. Remove and recreate an older
`CuaDriver Local Signing (cua-driver-rs)` identity in Keychain Access before
using it for security-sensitive testing, then reset and re-grant the local
bundle's TCC permissions.

## Production macOS signing identity

Production uses the Muse Code Apple Team ID `4W5TH4RKQ2`. The value is embedded
into Computer History admission policy, and the install scripts require an
Apple-anchored, notarized app with that team and the exact
`com.meta.musecode.cua.driver` identity. `CUA_DRIVER_PRODUCTION_TEAM_ID` is an
optional release/test assertion and is rejected if it differs from the pinned
value; end users do not need to configure it.

The one-time migration from `com.trycua.driver` defaults its old signer to
`YCK386LBJ7`; a different reviewed legacy signer may be supplied through
`CUA_DRIVER_LEGACY_TEAM_ID`. Migration stops before replacing the old app when
encrypted Computer History is present. Purge that history with the verified old
helper or use a separately reviewed key-migration tool before retrying.

Telemetry and update checks have been removed from the driver built from this
repository: it collects and sends nothing. `cua-driver telemetry install-event`
is a no-op kept for compatibility, and `cua-driver telemetry reset-id` only
deletes the installation ID, event markers and update-check cache that an
earlier build left in `~/.cua-driver`, `~/.cua-driver-local` or
`~/.cua-driver-rs`, whichever build runs it.

The inherited release installers (`install.sh`, `install.ps1`) no longer record
an install event, write an install-channel hint, or carry a telemetry ID
forward. They still download upstream binaries, which send telemetry by default;
see the repository README. `uninstall-local.sh` and `uninstall-local.ps1` delete
those files from the local home. Uninstalling a release install with `--purge`
on Unix, or with `CUA_DRIVER_RS_UNINSTALL_PURGE=1` on Windows, deletes them from
the package home, including `.release_installed/`. Both are local and need no
network.

Keep source commits host-owned. Verification machines should sync from this
checkout and return artifacts, not push code.

Local and released installations are removed independently:

```bash
# macOS / Linux, from the checkout
libs/cua-driver/scripts/uninstall-local.sh

# Windows, from the checkout
libs/cua-driver/scripts/uninstall-local.ps1
```

The local uninstaller leaves `cua-driver`, `CuaDriver.app`, release services,
release state, and release TCC grants untouched. On macOS it revokes only
`com.meta.musecode.cua.driver.local`; pass `--keep-tcc` to retain that local grant.

The release Unix uninstaller shuts down the release service before removing
anything. It first requires the systemd/launchd supervisor to stop, then uses
the daemon PID file to validate the installed release process and invokes the
trusted installed helper as `cua-driver --expected-pid <pid> stop`. A helper
that supports this option reads daemon metadata and requires the daemon PID to
match before sending shutdown; older helpers reject this argv shape instead of
silently stopping an unrelated default-socket daemon. The uninstaller escalates
only the already-validated release PID if graceful shutdown is unavailable, and
verifies the daemon stays stopped before cleanup begins.

With a missing or stale PID file, the script performs only a narrow
release-executable process check. It never signals an ambiguous process. If
supervisor shutdown, ownership validation, process inspection, or final
shutdown verification cannot be proven safe, uninstall aborts non-zero while
the runtime is still in place.
