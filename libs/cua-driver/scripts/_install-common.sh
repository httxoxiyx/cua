# _install-common.sh — shared helpers for the .sh installers.
#
# Bash counterpart to CuaDriverInstall.psm1. Both files have to stay in
# lockstep on the kill / probe behaviour so a Mac/Linux dev fix matches
# what install.ps1 does on Windows. Function names mirror the
# PowerShell side:
#
#   stop_cua_driver_daemons          ↔  Stop-CuaDriverDaemons
#   show_cua_driver_daemon_survivors ↔  Show-CuaDriverDaemonSurvivors
#
# This script is sourced (not exec'd) by the Rust install helpers:
#
#   * _install-rust.sh           (production Rust delegate)
#   * _install-local-rust.sh     (dev Rust installer)
#
# Loaders: on-disk first when run from a checked-out tree, else fetched
# from GitHub raw via `curl` (mirrors the irm | iex path that
# install.ps1 uses to import the .psm1 over the network). The inline
# loader is duplicated in each consumer because `source` doesn't have a
# bootstrap function the way PowerShell's `Import-Module` does — kept
# minimal so the duplication cost stays low.
#
# Keep this file narrow on purpose: every byte gets curl'd on every
# `curl ... | bash` install on non-macOS hosts (where install.sh
# auto-delegates to the Rust path) AND on dev installs. Things that DO
# belong here: kill / wait / probe helpers that multiple consumers
# need. Things that DON'T: anything only one script uses (leave it
# inline there).
#
# Style:
#   * No `set -e` — sourced helpers shouldn't change the caller's shell
#     options. Caller scripts are already `set -euo pipefail`.
#   * Linux supervisor/process shutdown is fail-closed before runtime swaps;
#     legacy macOS cleanup remains best-effort for callers that do not perform
#     identity-changing work.
#   * Bash 3.2 compatible (macOS default). No associative arrays, no
#     `[[ =~ ]]` patterns that need bash 4+.

# Best-effort kill of any running cua-driver daemons so the next
# `cua-driver` invocation starts the FRESH binary, not whatever's still
# in memory from the pre-upgrade install. Without this the previous
# daemon keeps running (and on macOS keeps holding TCC-attributed file
# handles) until the user logs out — which surfaces as "the bug I just
# fixed is still there" because the in-memory code is pre-fix.
#
# Layered escalation, in order of decreasing politeness:
#   1. macOS: `launchctl unload <plist>` on the Rust LaunchAgent
#      (com.trycua.cua-driver-rs.plist). Unload
#      is the documented way to stop a launchd-managed daemon — it
#      also clears the KeepAlive flag so launchd doesn't immediately
#      respawn the process we're about to kill. No-op (with stderr
#      suppressed) when the plist isn't installed.
#      Linux: stop both the current cua-driver.service and the retired
#      cua-driver-rs.service, then verify inactive state so Restart cannot
#      repopulate the process set during the swap. Enablement is preserved.
#   2. `pkill -x cua-driver` as the backstop for processes that weren't
#      launchd/systemd-supervised (e.g. a manual `cua-driver serve`
#      from a dev shell, or `cua-driver mcp` spawned by an editor that
#      isn't aware we're swapping the binary out from under it).
#
# Daemon-name coverage:
#   The Rust binary execs as `cua-driver`. One `pkill -x cua-driver`
#   stops it. `cua-driver-uia` is a Windows-only helper (see
#   libs/cua-driver/rust/crates/cua-driver-uia/) and never exists on
#   macOS/Linux, so no pkill for it here.
#
# Return a three-way systemd state without treating manager/query failures as
# "inactive". 0 = active, 1 = inactive/not found, 2 = unknown.
cua_systemd_active_state() {
    local service="$1" status=0
    command -v systemctl >/dev/null 2>&1 || return 2
    systemctl --user is-active --quiet "$service" >/dev/null 2>&1 || status=$?
    case "$status" in
        0) return 0 ;;
        3|4) return 1 ;;
        *) return 2 ;;
    esac
}

stop_and_verify_cua_systemd_service() {
    local service="$1" unit_path="$2"
    local active_state=0

    if ! command -v systemctl >/dev/null 2>&1; then
        if [ -e "$unit_path" ] || [ -L "$unit_path" ]; then
            printf 'error: systemctl is unavailable; cannot quiesce %s\n' "$service" >&2
            return 1
        fi
        return 0
    fi

    active_state=0
    cua_systemd_active_state "$service" || active_state=$?
    if [ "$active_state" = "1" ]; then
        return 0
    fi
    if [ "$active_state" = "2" ]; then
        printf 'error: could not inspect systemd user service %s\n' "$service" >&2
        return 1
    fi
    if ! systemctl --user stop "$service" >/dev/null 2>&1; then
        printf 'error: could not stop systemd user service %s\n' "$service" >&2
        return 1
    fi

    active_state=0
    cua_systemd_active_state "$service" || active_state=$?
    if [ "$active_state" != "1" ]; then
        printf 'error: systemd user service %s remains active or unverifiable\n' "$service" >&2
        return 1
    fi
}

# Returns non-zero on Linux when the supported systemd supervisors or daemon
# process cannot be proven quiescent. Other platforms retain the legacy
# best-effort behavior; macOS identity-changing callers use their stricter,
# path-authenticated shutdown before reaching this helper.
stop_cua_driver_daemons() {
    printf '==> stopping any running cua-driver daemons before swap\n'

    # Keep temporary locals and platform branches isolated. Expected no-match
    # statuses are handled explicitly; verification failures escape so callers
    # cannot replace a supervised runtime.
    (
        case "$(uname -s 2>/dev/null || echo unknown)" in
            Darwin)
                # Both known LaunchAgent plists. `launchctl unload` is a
                # no-op-with-warning when the plist doesn't exist; swallow
                # stderr to keep the install log clean.
                local plist
                # Rust LaunchAgent plist.
                plist="$HOME/Library/LaunchAgents/com.trycua.cua-driver-rs.plist"
                if [ -f "$plist" ]; then
                    if [ -f "$plist" ]; then
                        launchctl unload "$plist" >/dev/null 2>&1 || true
                    fi
                fi
                ;;
            Linux)
                # Cover both the current unit and its retired Rust name. A
                # process kill is not sufficient while Restart can respawn it.
                stop_and_verify_cua_systemd_service \
                    cua-driver.service \
                    "$HOME/.config/systemd/user/cua-driver.service" \
                    || exit 1
                stop_and_verify_cua_systemd_service \
                    cua-driver-rs.service \
                    "$HOME/.config/systemd/user/cua-driver-rs.service" \
                    || exit 1
                ;;
            *)
                # Other Unixes (FreeBSD, etc.) — no supervisor we know
                # about, fall through to the pkill backstop.
                ;;
        esac

        # Best-effort: give the supervisor up to ~200ms to take the
        # daemon down before we resort to pkill. Same wait as the
        # PowerShell module after schtasks /End.
        sleep 0.2 2>/dev/null || sleep 1

        if command -v pkill >/dev/null 2>&1; then
            # `-x` = exact match on process name so we don't kill a
            # user's `cua-driver-rs-foo` test harness or any script
            # named "cua-driver-something". The Rust + Swift binaries
            # both exec as exactly `cua-driver` so a single pkill
            # covers both.
            pkill -x cua-driver >/dev/null 2>&1 || true
        fi
        if [ "$(uname -s 2>/dev/null || echo unknown)" = "Linux" ]; then
            sleep 0.2 2>/dev/null || sleep 1
            if ! command -v pgrep >/dev/null 2>&1; then
                printf 'error: pgrep is unavailable; cannot verify cua-driver daemon shutdown\n' >&2
                exit 1
            fi
            local uid candidates="" pgrep_status=0
            uid="$(id -u 2>/dev/null || true)"
            case "$uid" in ''|*[!0-9]*) exit 1 ;; esac
            candidates="$(pgrep -U "$uid" -x cua-driver 2>/dev/null)" || pgrep_status=$?
            case "$pgrep_status" in
                1) ;;
                0)
                    printf 'error: cua-driver daemon remains after supervisor shutdown: %s\n' \
                        "$(printf '%s' "$candidates" | tr '\n' ' ')" >&2
                    exit 1
                    ;;
                *)
                    printf 'error: could not inspect cua-driver processes after supervisor shutdown\n' >&2
                    exit 1
                    ;;
            esac
        fi
    )
}

# Print a yellow warning if cua-driver processes are still running after
# stop_cua_driver_daemons did its best. On macOS/Linux this almost never
# fires for processes the current user owns — `pkill` succeeds against
# user-owned processes without elevation — so when it DOES fire it
# usually means:
#
#   * Another user on the same host has their own cua-driver running
#     (multi-user dev box). Their daemon is fine; ours just won't see
#     the swap until they restart theirs.
#   * The daemon was spawned with a custom signal handler that ignores
#     SIGTERM (we don't ship such a handler — but a debugger session
#     could install one).
#   * Process is in uninterruptible state (D-state on Linux from a
#     stuck syscall — rare for a userspace daemon).
#
# Mirrors Show-CuaDriverDaemonSurvivors on the PowerShell side, but the
# Unix-side mitigation is different: there's no clean User Account
# Control / High-IL escalation analogue, so the hint is just `sudo
# pkill` (root can reach other users' processes).
#
# Idempotent — safe to call even when stop_cua_driver_daemons did
# clean up everything. Prints nothing in the common case.
show_cua_driver_daemon_survivors() {
    # `pgrep -x` matches the exact process name the same way `pkill -x`
    # did above, so the survivor list is exactly what the kill couldn't
    # reach (not random other cua-driver-* binaries).
    if ! command -v pgrep >/dev/null 2>&1; then
        # No pgrep, no way to check. Bail quietly — the install can
        # still succeed; worst case the user notices a stale binary
        # and reboots.
        return 0
    fi

    local survivor_pids
    survivor_pids=$(pgrep -x cua-driver 2>/dev/null || true)
    if [ -z "$survivor_pids" ]; then
        return 0
    fi

    # `tput` may not be available (CI, agent sandboxes without TERM);
    # guard so we don't crash before printing the actual warning. The
    # color is decoration — the text is the load-bearing part.
    local yellow normal
    yellow=$(tput setaf 3 2>/dev/null || true)
    normal=$(tput sgr0 2>/dev/null || true)

    # `wc -w` is portable across BSD and GNU; pgrep prints one pid per
    # line so we'd get the same count from `wc -l`, but `wc -w` is
    # robust to a missing trailing newline.
    local count
    count=$(printf '%s\n' "$survivor_pids" | wc -w | tr -d ' ')
    local pid_csv
    pid_csv=$(printf '%s\n' "$survivor_pids" | tr '\n' ',' | sed 's/,$//' | sed 's/,/, /g')

    printf '%sNote: %s cua-driver process(es) still running after best-effort kill (pid: %s).%s\n' \
        "$yellow" "$count" "$pid_csv" "$normal"
    printf '%s      They are probably owned by another user, or are ignoring SIGTERM.%s\n' \
        "$yellow" "$normal"
    printf '%s      To force-kill (needs root if owned by another user):%s\n' \
        "$yellow" "$normal"
    printf '%s        sudo pkill -9 -x cua-driver%s\n' \
        "$yellow" "$normal"
    printf '%s      Or log out and back in. Until they exit, the OLD binary keeps running.%s\n' \
        "$yellow" "$normal"
    return 0
}
