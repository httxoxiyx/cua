#!/usr/bin/env bash
# cua-driver uninstaller (Rust implementation only). Mirrors uninstall.ps1 on
# Windows: one canonical script per shell, no private `_uninstall-rust.sh`
# helper.
#
# Behaviour by host + flag:
#   all hosts + no flag             → Rust uninstall
#   --backend=rust/swift            → no-op (Rust is the only supported backend)
#   --experimental-rust             → legacy alias (no-op)
#
# Swift uninstall removes:
#   - ~/.local/bin/cua-driver symlink (+ legacy /usr/local/bin/cua-driver)
#   - /Applications/CuaDriver.app bundle
#   - ~/.cua-driver/ (telemetry id + install marker)
#   - ~/Library/Application Support/Cua Driver/ (config.json)
#   - ~/Library/Caches/cua-driver/ (daemon/cache state)
#   - Skill symlinks under ~/.claude/skills/cua-driver, ~/.agents/skills/
#     cua-driver, ~/.openclaw/skills/cua-driver, ~/.config/opencode/
#     skills/cua-driver (only when they point at our app bundle)
#   - Claude MCP registrations in ~/.claude.json (cua-driver / cua-computer-use)
#
# Rust uninstall removes:
#   Linux:
#     - ~/.local/bin/cua-driver symlink (only when it resolves to a
#       cua-driver path — a Swift-driver symlink is left in place)
#     - versioned packages/current symlink under ~/.cua-driver/
#     - telemetry id, preference, and registration markers are preserved by
#       default so a reinstall remains the same pseudonymous installation
#     - ~/.config/systemd/user/cua-driver.service (if --autostart
#       was used via install-local.sh — stop + disable + remove), plus the
#       legacy cua-driver-rs.service unit
#     - Skill symlinks under ~/.claude/skills/cua-driver(-rs), ~/.agents/
#       skills/…, ~/.openclaw/skills/…, ~/.config/opencode/skills/…
#   macOS:
#     - /Applications/CuaDriver.app bundle (+ legacy CuaDriverRs.app)
#     - ~/.local/bin/cua-driver symlink (only when it resolves into
#       /Applications/CuaDriver.app)
#     - runtime payloads under ~/.cua-driver/ and legacy ~/.cua-driver-rs/;
#       telemetry state remains unless --purge is passed
#     - ~/Library/LaunchAgents/com.trycua.cua-driver.plist (if --autostart
#       was used via install-local.sh — unload + remove), plus the legacy
#       com.trycua.cua-driver-rs.plist LaunchAgent
#     - Skill symlinks under ~/.claude/skills/cua-driver(-rs), etc.
#
# Shared-path safety: /Applications/CuaDriver.app + its ~/.local/bin
# symlink share the historical Swift install path but use the Muse Code bundle
# id `com.meta.musecode.cua.driver`, so they're only removed when an
# unambiguous Rust marker is on disk
# (~/.cua-driver/packages/, legacy ~/.cua-driver-rs/, CuaDriverRs.app,
# the LaunchAgent/systemd unit, or current Rust telemetry state).
#
# Also scrubs Claude MCP registrations in ~/.claude.json that match
# the active backend.
#
# Revokes TCC grants on macOS by default (Accessibility + Screen Recording)
# so the next install prompts cleanly under the new signing identity. Pass
# --keep-tcc to preserve grants across uninstall/reinstall.
#
# Usage:
#   /bin/bash -c "$(curl -fsSL https://cua.ai/driver/uninstall.sh)"
#   /bin/bash -c "$(curl -fsSL https://cua.ai/driver/uninstall.sh)" -- --purge
#
# Env overrides (mirror install side):
#   CUA_DRIVER_HOME       Rust package home to remove (default ~/.cua-driver)
#   CUA_DRIVER_RS_HOME    Legacy alias for CUA_DRIVER_HOME
set -euo pipefail

# ----------------------------------------------------------------------
# Flag parsing — same two-pass shape as install.sh so the argv shapes
# stay bit-compatible across install/uninstall and a future Rust-only
# flag flows through without edits.
# ----------------------------------------------------------------------
USE_RUST_BACKEND=1
RESET_TCC=1
PURGE_DATA=0
FORWARDED_ARGS=()
PASSTHROUGH=0
while [[ $# -gt 0 ]]; do
    if [[ "$PASSTHROUGH" == "1" ]]; then
        FORWARDED_ARGS+=("$1"); shift; continue
    fi
    case "$1" in
        --experimental-rust) shift ;;  # legacy alias for default Rust path
        --backend=rust)      shift ;;
        --backend=swift)     shift ;;  # retired Swift (no-op)
        --reset-tcc)         RESET_TCC=1; shift ;;  # legacy/explicit default: revoke TCC grants
        --keep-tcc)          RESET_TCC=0; shift ;;  # preserve TCC grants across reinstall
        --purge)             PURGE_DATA=1; shift ;;  # also delete pseudonymous identity + preference
        --backend=*)
            printf 'error: unknown backend %q; supported: rust\n' "${1#*=}" >&2
            exit 2
            ;;
        --)                  PASSTHROUGH=1; shift ;;  # forward the rest verbatim
        *)                   FORWARDED_ARGS+=("$1"); shift ;;
    esac
done

# Legacy --backend=swift is accepted as a no-op for backward compat.
OS="$(uname -s 2>/dev/null || echo unknown)"
if [[ "$USE_RUST_BACKEND" == "1" ]]; then
    if [[ "$OS" != "Darwin" ]]; then
        printf 'note: detected non-macOS host (%s); uninstalling cua-driver via the Rust implementation.\n' "$OS" >&2
    else
        printf 'note: uninstalling cua-driver via the Rust implementation.\n' >&2
    fi
fi

# ----------------------------------------------------------------------
# Shared helpers
# ----------------------------------------------------------------------
log() { printf '==> %s\n' "$*"; }

RELEASE_BUNDLE_ID="com.meta.musecode.cua.driver"
LEGACY_RELEASE_BUNDLE_ID="com.trycua.driver"
LEGACY_RS_BUNDLE_ID="com.trycua.cuadriverrs"
RELEASE_EXECUTABLE="cua-driver"
PINNED_PRODUCTION_TEAM_ID="4W5TH4RKQ2"
PRODUCTION_TEAM_ID="${CUA_DRIVER_PRODUCTION_TEAM_ID:-$PINNED_PRODUCTION_TEAM_ID}"
LEGACY_PRODUCTION_TEAM_ID="${CUA_DRIVER_LEGACY_TEAM_ID:-YCK386LBJ7}"
PLISTBUDDY="/usr/libexec/PlistBuddy"
CODESIGN="/usr/bin/codesign"
SPCTL="/usr/sbin/spctl"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
TCCUTIL="/usr/bin/tccutil"

validate_apple_team_id() {
    [[ "$1" =~ ^[A-Z0-9]{10}$ ]]
}

validate_release_home_dir() {
    local home_dir="$1" user_home="$2" resolved_home resolved_dir
    case "$home_dir" in
        /*) ;;
        *) printf 'error: CUA_DRIVER_HOME must be an absolute path\n' >&2; return 1 ;;
    esac
    case "$home_dir" in
        /|"$user_home"|"$user_home"/|*/../*|*/..|*/./*|*/.)
            printf 'error: refusing unsafe Cua Driver home: %s\n' "$home_dir" >&2
            return 1
            ;;
    esac
    [[ "$home_dir" != *$'\n'* && "$home_dir" != *$'\r'* ]] || {
        printf 'error: refusing Cua Driver home containing a line break\n' >&2
        return 1
    }
    resolved_home="$(CDPATH= cd -- "$user_home" 2>/dev/null && pwd -P)" || {
        printf 'error: could not resolve HOME safely: %s\n' "$user_home" >&2
        return 1
    }
    case "$home_dir" in
        "$user_home"/*) ;;
        *)
            printf 'error: CUA_DRIVER_HOME must remain inside HOME (%s): %s\n' "$resolved_home" "$home_dir" >&2
            return 1
            ;;
    esac
    if [[ -e "$home_dir" || -L "$home_dir" ]]; then
        [[ ! -L "$home_dir" && -d "$home_dir" ]] || {
            printf 'error: refusing non-directory or symlink Cua Driver home: %s\n' "$home_dir" >&2
            return 1
        }
        resolved_dir="$(CDPATH= cd -- "$home_dir" 2>/dev/null && pwd -P)" || {
            printf 'error: could not resolve Cua Driver home safely: %s\n' "$home_dir" >&2
            return 1
        }
        case "$resolved_dir" in
            "$resolved_home"/*) ;;
            *)
                printf 'error: Cua Driver home resolves outside HOME: %s\n' "$resolved_dir" >&2
                return 1
                ;;
        esac
    fi
}

directory_has_entries() {
    local directory="$1" entry
    for entry in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
        if [[ -e "$entry" || -L "$entry" ]]; then
            return 0
        fi
    done
    return 1
}

# Return 0 only for a nonempty, ordinary history directory. Absence and an
# empty directory return 1 so a later --purge can finish after the runtime was
# already removed. Unsafe path types fail closed with status 2.
history_state_present() {
    local history_root="$1"
    if [[ ! -e "$history_root" && ! -L "$history_root" ]]; then
        return 1
    fi
    if [[ -L "$history_root" || ! -d "$history_root" ]]; then
        printf 'history_purge_incomplete: refusing unsafe Computer History path %s\n' \
            "$history_root" >&2
        return 2
    fi
    directory_has_entries "$history_root"
}

macos_plist_value() {
    local plistbuddy="$1" app_bundle="$2" key="$3"
    [[ -x "$plistbuddy" ]] || return 1
    "$plistbuddy" -c "Print :$key" "$app_bundle/Contents/Info.plist" 2>/dev/null
}

# Validate every immutable identity dimension before a helper at a shared app
# path is executed or the app is removed. Signature integrity by itself is not
# ownership: an attacker can ad-hoc sign an internally consistent replacement.
macos_release_app_is_owned() {
    local app_bundle="$1" expected_bundle_id="$2" expected_executable="$3"
    local expected_team="$4" codesign_tool="$5" plistbuddy="$6" spctl_tool="$7"
    local actual_bundle_id actual_executable details requirement executable_path spctl_output trust_requirement

    validate_apple_team_id "$expected_team" || return 1
    [[ -d "$app_bundle" && ! -L "$app_bundle" ]] || return 1
    [[ -f "$app_bundle/Contents/Info.plist" && ! -L "$app_bundle/Contents/Info.plist" ]] || return 1
    actual_bundle_id="$(macos_plist_value "$plistbuddy" "$app_bundle" CFBundleIdentifier || true)"
    actual_executable="$(macos_plist_value "$plistbuddy" "$app_bundle" CFBundleExecutable || true)"
    [[ "$actual_bundle_id" == "$expected_bundle_id" ]] || return 1
    [[ "$actual_executable" == "$expected_executable" ]] || return 1
    executable_path="$app_bundle/Contents/MacOS/$expected_executable"
    [[ -f "$executable_path" && -x "$executable_path" && ! -L "$executable_path" ]] || return 1
    [[ -x "$codesign_tool" ]] || return 1
    "$codesign_tool" --verify --deep --strict "$app_bundle" >/dev/null 2>&1 || return 1
    trust_requirement="anchor apple generic and identifier \"$expected_bundle_id\" and certificate leaf[subject.OU] = \"$expected_team\""
    "$codesign_tool" --verify --deep --strict -R "=$trust_requirement" \
        "$app_bundle" >/dev/null 2>&1 || return 1
    details="$("$codesign_tool" -d --verbose=4 "$app_bundle" 2>&1)" || return 1
    [[ "$(printf '%s\n' "$details" | sed -n 's/^Identifier=//p' | sed -n '1p')" == "$expected_bundle_id" ]] || return 1
    [[ "$(printf '%s\n' "$details" | sed -n 's/^TeamIdentifier=//p' | sed -n '1p')" == "$expected_team" ]] || return 1
    requirement="$("$codesign_tool" -d -r- "$app_bundle" 2>&1 \
        | sed -n -e 's/^designated => //p' -e 's/^# designated => //p')" || return 1
    [[ -n "$requirement" ]] || return 1
    [[ "$requirement" == *"identifier \"$expected_bundle_id\""* ]] || return 1
    [[ "$requirement" == *"anchor apple generic"* ]] || return 1
    [[ "$requirement" == *"certificate leaf[subject.OU]"*"$expected_team"* ]] || return 1
    [[ -x "$spctl_tool" ]] || return 1
    spctl_output="$("$spctl_tool" --assess --type execute --verbose=4 "$app_bundle" 2>&1)" \
        || return 1
    [[ "$spctl_output" == *"source=Notarized Developer ID"* ]]
}

purge_macos_history() {
    local app_bundle="$1"
    local helper="$2"
    local rust_install_present="$3"
    local codesign_tool="$4"
    local plistbuddy="${5:-$PLISTBUDDY}"
    local bundle_id="${6:-$RELEASE_BUNDLE_ID}"
    local expected_team="${7:-$PRODUCTION_TEAM_ID}"
    local spctl_tool="${8:-$SPCTL}"
    local expected_helper="$app_bundle/Contents/MacOS/$RELEASE_EXECUTABLE"
    if [[ "$rust_install_present" != "1" || "$helper" != "$expected_helper" ]] \
        || ! macos_release_app_is_owned "$app_bundle" "$bundle_id" \
            "$RELEASE_EXECUTABLE" "$expected_team" "$codesign_tool" "$plistbuddy" "$spctl_tool"; then
        printf 'history_purge_incomplete: installed signed Cua Driver helper unavailable; preserved history state for retry\n' >&2
        return 1
    fi
    if ! "$helper" history purge-offline --yes; then
        printf 'history_purge_incomplete: exact-namespace key destruction was not verified; preserved history state and app for retry\n' >&2
        return 1
    fi
}

purge_linux_history() {
    local helper="$1"
    local rust_install_present="$2"
    if [[ "$rust_install_present" != "1" || ! -x "$helper" ]]; then
        printf 'history_purge_incomplete: installed Cua Driver helper unavailable; preserved history state for retry\n' >&2
        return 1
    fi
    if ! "$helper" history purge-offline --yes; then
        printf 'history_purge_incomplete: exact-namespace Secret Service key destruction was not verified; preserved history state and runtime for retry\n' >&2
        return 1
    fi
}

purge_release_history_if_present() {
    local history_root="$1" state_status=0 helper
    history_state_present "$history_root" || state_status=$?
    case "$state_status" in
        0) ;;
        1)
            log "no encrypted release Computer History state to purge"
            return 0
            ;;
        *) return 1 ;;
    esac

    case "$OS" in
        Darwin)
            helper="$APP_BUNDLE/Contents/MacOS/cua-driver"
            purge_macos_history \
                "$APP_BUNDLE" "$helper" "$RUST_INSTALL_PRESENT" \
                "$CODESIGN" "$PLISTBUDDY" "$APP_BUNDLE_ID" \
                "$APP_BUNDLE_TEAM_ID" "$SPCTL" || return 1
            log "cryptographically purged release Computer History key and local history state"
            ;;
        Linux)
            helper="$PACKAGES_DIR/current/cua-driver"
            purge_linux_history "$helper" "$RUST_INSTALL_PRESENT" || return 1
            log "cryptographically purged release Computer History Secret Service key and local history state"
            ;;
    esac
}

daemon_pid_file_path() {
    case "$OS" in
        Darwin) printf '%s/Library/Caches/cua-driver/cua-driver.pid' "$HOME" ;;
        Linux)  printf '%s/.cache/cua-driver/cua-driver.pid' "$HOME" ;;
        *)      printf '/tmp/cua-driver.pid' ;;
    esac
}

read_daemon_pid() {
    local pid
    [[ -r "$1" ]] || return 1
    IFS= read -r pid < "$1" || true
    [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
    printf '%s' "$pid"
}

daemon_pid_alive() { kill -0 "$1" 2>/dev/null; }

daemon_process_generation() {
    local start=""
    command -v ps >/dev/null 2>&1 || return 2
    start="$(LC_ALL=C ps -ww -o lstart= -p "$1" 2>/dev/null)" || return 2
    start="${start#"${start%%[![:space:]]*}"}"
    start="${start%"${start##*[![:space:]]}"}"
    [[ -n "$start" ]] || return 2
    printf '%s' "$start"
}

daemon_pid_matches_generation() {
    local pid="$1" expected="$2" current="" identity_status=0
    daemon_pid_alive "$pid" || return 1
    current="$(daemon_process_generation "$pid")" || return 2
    [[ "$current" == "$expected" ]] || return 1
    daemon_pid_is_release "$pid" || identity_status=$?
    [[ "$identity_status" == "0" ]] && return 0
    [[ "$identity_status" == "2" ]] && return 2
    return 1
}

daemon_signal_if_current() {
    local status=0
    daemon_pid_matches_generation "$1" "$2" || status=$?
    [[ "$status" == "2" ]] && return 2
    [[ "$status" == "0" ]] || return 1
    kill -"$3" "$1" 2>/dev/null || return 1
}

daemon_wait_for_exit() {
    local pid="$1" attempts=0
    while daemon_pid_alive "$pid"; do
        [[ "$attempts" -lt 20 ]] || return 1
        sleep 0.1 2>/dev/null || sleep 1 || true
        attempts=$((attempts + 1))
    done
}

daemon_process_identity() {
    local pid="$1" identity="" lsof_tool="${CUA_DRIVER_LSOF:-/usr/sbin/lsof}"
    if [[ -L "/proc/$pid/exe" ]]; then
        identity="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
        identity="${identity% (deleted)}"
    fi
    if [[ -z "$identity" && "$OS" == "Darwin" ]]; then
        [[ -x "$lsof_tool" ]] || return 2
        identity="$("$lsof_tool" -a -p "$pid" -d txt -Fn 2>/dev/null \
            | sed -n 's/^n//p' | sed -n '1p')" || return 2
        [[ -n "$identity" ]] || return 2
    fi
    if [[ -z "$identity" ]]; then
        command -v ps >/dev/null 2>&1 || return 2
        # `command=` starts with argv[0], which may be only `cua-driver` for a
        # PATH launch and therefore loses the installed path. `comm=` is the
        # kernel executable identity on macOS; /proc remains authoritative on
        # Linux when available.
        identity="$(ps -ww -o comm= -p "$pid" 2>/dev/null)" || return 2
    fi
    identity="${identity#"${identity%%[![:space:]]*}"}"
    identity="${identity%"${identity##*[![:space:]]}"}"
    [[ -n "$identity" ]] || return 1
    printf '%s' "$identity"
}

daemon_identity_matches_install() {
    case "$1" in
        "$USER_BIN_LINK"|"$USER_BIN_LINK"[[:space:]]*|\
        "$HOME_DIR"/packages/current/cua-driver|"$HOME_DIR"/packages/current/cua-driver[[:space:]]*|\
        "$HOME_DIR"/packages/releases/*/cua-driver|"$HOME_DIR"/packages/releases/*/cua-driver[[:space:]]*|\
        "$LEGACY_HOME_DIR"/packages/current/cua-driver|"$LEGACY_HOME_DIR"/packages/current/cua-driver[[:space:]]*|\
        "$LEGACY_HOME_DIR"/packages/releases/*/cua-driver|"$LEGACY_HOME_DIR"/packages/releases/*/cua-driver[[:space:]]*) return 0 ;;
    esac
    if [[ "${APP_BUNDLE_OWNED:-0}" == "1" ]]; then
        case "$1" in
            "$APP_BUNDLE"/Contents/MacOS/cua-driver|"$APP_BUNDLE"/Contents/MacOS/cua-driver[[:space:]]*) return 0 ;;
        esac
    fi
    if [[ "${LEGACY_APP_BUNDLE_OWNED:-0}" == "1" ]]; then
        case "$1" in
            "$LEGACY_APP_BUNDLE"/Contents/MacOS/cua-driver|"$LEGACY_APP_BUNDLE"/Contents/MacOS/cua-driver[[:space:]]*) return 0 ;;
        esac
    fi
    return 1
}

daemon_pid_is_release() {
    local identity
    identity="$(daemon_process_identity "$1" 2>/dev/null)" || return 2
    daemon_identity_matches_install "$identity"
}

release_daemon_fallback_pids() {
    command -v pgrep >/dev/null 2>&1 || return 2
    command -v ps >/dev/null 2>&1 || return 2
    local uid pid identity candidates="" pgrep_status=0
    uid="$(id -u 2>/dev/null || true)"
    [[ "$uid" =~ ^[0-9]+$ ]] || return 2

    candidates="$(pgrep -U "$uid" -f '(^|[[:space:]/])cua-driver([[:space:]]|$)' 2>/dev/null)" || pgrep_status=$?
    case "$pgrep_status" in
        0) ;;
        1) return 0 ;;
        *) return 2 ;;
    esac

    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 2
        identity="$(daemon_process_identity "$pid" 2>/dev/null)" || return 2
        if daemon_identity_matches_install "$identity"; then
            printf '%s\n' "$pid"
        fi
    done <<< "$candidates"
}

verify_release_daemon_absent() {
    local pids="" status=0
    pids="$(release_daemon_fallback_pids 2>/dev/null)" || status=$?
    if [[ "$status" != "0" ]]; then
        printf 'daemon_stop_incomplete: process inspection failed while verifying release daemon shutdown.\n' >&2
        return 1
    fi
    if [[ -n "$pids" ]]; then
        printf 'daemon_stop_incomplete: release cua-driver process remains but cannot be safely classified or signalled.\n' >&2
        return 1
    fi
}

stop_release_daemon() {
    local pid="" helper="${DAEMON_STOP_HELPER:-}" generation="" status=0 helper_stderr=""
    if [[ -n "${DAEMON_PID_FILE:-}" ]] \
        && pid="$(read_daemon_pid "$DAEMON_PID_FILE" 2>/dev/null)" \
        && daemon_pid_alive "$pid"; then
        daemon_pid_is_release "$pid" || status=$?
        if [[ "$status" == "2" ]]; then
            printf 'daemon_stop_incomplete: failed to identify live daemon pid %s safely.\n' "$pid" >&2
            return 1
        fi
        if [[ "$status" == "0" ]]; then
            [[ -n "$helper" && -x "$helper" ]] || {
                printf 'daemon_stop_incomplete: trusted installed cua-driver stop helper is unavailable for pid %s.\n' "$pid" >&2
                return 1
            }
            generation="$(daemon_process_generation "$pid")" || {
                printf 'daemon_stop_incomplete: failed to capture process generation for daemon pid %s.\n' "$pid" >&2
                return 1
            }
            status=0
            helper_stderr="$("$helper" --expected-pid "$pid" stop 2>&1 >/dev/null)" || status=$?
            if [[ "$status" != "0" ]]; then
                printf 'note: trusted stop helper exited %s for pid %s; falling back to signals.\n' "$status" "$pid" >&2
                [[ -z "$helper_stderr" ]] || printf '%s\n' "$helper_stderr" >&2
            fi
            if ! daemon_wait_for_exit "$pid"; then
                status=0
                daemon_signal_if_current "$pid" "$generation" TERM || status=$?
                [[ "$status" != "2" ]] || {
                    printf 'daemon_stop_incomplete: failed to revalidate daemon pid %s before TERM.\n' "$pid" >&2
                    return 1
                }
                if [[ "$status" == "0" ]] && ! daemon_wait_for_exit "$pid"; then
                    status=0
                    daemon_signal_if_current "$pid" "$generation" KILL || status=$?
                    [[ "$status" != "2" ]] || {
                        printf 'daemon_stop_incomplete: failed to revalidate daemon pid %s before KILL.\n' "$pid" >&2
                        return 1
                    }
                    if [[ "$status" == "0" ]] && ! daemon_wait_for_exit "$pid"; then
                        printf 'daemon_stop_incomplete: validated release daemon pid %s is still running.\n' "$pid" >&2
                        return 1
                    fi
                fi
            fi
        fi
    fi
    sleep 0.2 2>/dev/null || true
    verify_release_daemon_absent
}

reject_root_invocation() {
    local effective_uid="$1"
    if [[ "$effective_uid" == "0" ]]; then
        printf 'error: do not run the Cua Driver uninstaller with sudo; run it as the login user so Computer History is purged from the correct home directory and native credential store. The script elevates only protected app removal when needed.\n' >&2
        return 77
    fi
}

# TCC revocation is on by default so uninstall leaves the next macOS install
# in a clean promptable state. Keep the exact bundle registered until every
# scoped reset succeeds, then explicitly unregister it from LaunchServices.
# A failed registration, reset, or unregister preserves the app so cleanup is
# retryable instead of leaving an unresolvable stale TCC row.
maybe_reset_tcc() {
    local app_bundle="${1:-/Applications/CuaDriver.app}"
    local bundle_id="${2:-$RELEASE_BUNDLE_ID}"
    local failed_services="" service
    if [[ "$OS" != "Darwin" ]]; then
        log "TCC reset is macOS-only; nothing to revoke on $OS"
        return 0
    fi
    [[ -d "$app_bundle" && ! -L "$app_bundle" ]] || {
        printf 'error: cannot unregister missing or unsafe app bundle %s\n' "$app_bundle" >&2
        return 1
    }
    [[ -x "$LSREGISTER" ]] || {
        printf 'error: LaunchServices registration tool is unavailable; preserved %s\n' "$app_bundle" >&2
        return 1
    }
    if ! "$LSREGISTER" -f "$app_bundle" >/dev/null 2>&1; then
        printf 'error: could not register %s before permission cleanup; the app was preserved\n' "$app_bundle" >&2
        return 1
    fi

    if [[ "$RESET_TCC" == "1" ]]; then
        if [[ ! -x "$TCCUTIL" ]]; then
            printf 'error: tccutil is required to revoke permissions for %s; the app was preserved\n' "$bundle_id" >&2
            return 1
        fi
        log "revoking TCC grants for $bundle_id"
        for service in Accessibility ScreenCapture AppleEvents; do
            if "$TCCUTIL" reset "$service" "$bundle_id" >/dev/null 2>&1; then
                log "  reset $service"
            else
                failed_services="$failed_services $service"
            fi
        done
        if [[ -n "$failed_services" ]]; then
            printf 'error: could not reset these TCC services for %s:%s; the app was preserved\n' \
                "$bundle_id" "$failed_services" >&2
            return 1
        fi
    else
        log "preserving TCC grants for $bundle_id (--keep-tcc)"
    fi

    if ! "$LSREGISTER" -u "$app_bundle" >/dev/null 2>&1; then
        printf 'error: could not unregister %s from LaunchServices; the app was preserved\n' "$app_bundle" >&2
        return 1
    fi
}

# Resolve a symlink target to an absolute path. realpath -e fails when
# the target is missing — we want to inspect dangling symlinks too (a
# leftover from a half-removed install should still be cleaned up), so
# fall back to readlink + manual normalize when realpath errors out.
resolve_link() {
    local link="$1"
    if [[ ! -L "$link" ]]; then
        printf ''; return 0
    fi
    local target
    if target="$(realpath "$link" 2>/dev/null)"; then
        printf '%s' "$target"
        return 0
    fi
    target="$(readlink "$link" 2>/dev/null || true)"
    case "$target" in
        /*) printf '%s' "$target" ;;
        *)  printf '%s/%s' "$(cd -- "$(dirname -- "$link")" && pwd)" "$target" ;;
    esac
}

select_daemon_stop_helper() {
    local candidate resolved
    for candidate in \
        "$APP_BUNDLE/Contents/MacOS/cua-driver" \
        "$LEGACY_APP_BUNDLE/Contents/MacOS/cua-driver" \
        "$PACKAGES_DIR/current/cua-driver" \
        "$LEGACY_HOME_DIR/packages/current/cua-driver"; do
        [[ -x "$candidate" ]] || continue
        resolved="$(realpath "$candidate" 2>/dev/null || true)"
        [[ -n "$resolved" ]] || resolved="$candidate"
        case "$resolved" in
            "$APP_BUNDLE/Contents/MacOS/cua-driver")
                [[ "${APP_BUNDLE_OWNED:-0}" == "1" ]] || continue
                printf '%s' "$resolved"
                return 0
                ;;
            "$LEGACY_APP_BUNDLE/Contents/MacOS/cua-driver")
                [[ "${LEGACY_APP_BUNDLE_OWNED:-0}" == "1" ]] || continue
                printf '%s' "$resolved"
                return 0
                ;;
            "$HOME_DIR"/packages/releases/*/cua-driver|"$LEGACY_HOME_DIR"/packages/releases/*/cua-driver)
                printf '%s' "$resolved"
                return 0
                ;;
        esac
    done
    return 1
}

release_supervisor() {
    local action="$1" path name found=0
    case "$OS" in
        Linux)
            for path in "$SYSTEMD_USER_UNIT" "$LEGACY_SYSTEMD_USER_UNIT"; do
                [[ -f "$path" ]] || continue
                found=1
                name="${path##*/}"
                if [[ "$action" == "stop" ]]; then
                    command -v systemctl >/dev/null 2>&1 \
                        && systemctl --user stop "$name" >/dev/null 2>&1 || {
                            printf 'daemon_stop_incomplete: failed to stop systemd user unit %s.\n' "$name" >&2
                            return 1
                        }
                else
                    command -v systemctl >/dev/null 2>&1 \
                        && systemctl --user disable "$name" >/dev/null 2>&1 || true
                    rm -f "$path"
                    log "disabled + removed systemd --user unit $name"
                fi
            done
            if [[ "$action" == "remove" ]]; then
                [[ "$found" == "1" ]] || log "no current or legacy systemd --user unit found (skipping)"
                command -v systemctl >/dev/null 2>&1 \
                    && systemctl --user daemon-reload >/dev/null 2>&1 || true
            fi
            ;;
        Darwin)
            for path in "$LAUNCHAGENT_PLIST" "$LEGACY_LAUNCHAGENT_PLIST"; do
                [[ -f "$path" ]] || continue
                found=1
                if [[ "$action" == "stop" ]]; then
                    command -v launchctl >/dev/null 2>&1 \
                        && launchctl unload "$path" >/dev/null 2>&1 || {
                            printf 'daemon_stop_incomplete: failed to unload LaunchAgent %s.\n' "$path" >&2
                            return 1
                        }
                else
                    rm -f "$path"
                    log "removed LaunchAgent $path"
                fi
            done
            if [[ "$action" == "remove" && "$found" == "0" ]]; then
                log "no current or legacy LaunchAgent found (skipping)"
            fi
            ;;
    esac
}

if [[ "${CUA_DRIVER_UNINSTALL_TEST_SOURCE_ONLY:-0}" == "1" ]]; then
    return 0
fi

if ! reject_root_invocation "$(id -u)"; then
    exit 77
fi

# ----------------------------------------------------------------------
# Rust uninstall branch (default on Linux + macOS).
# ----------------------------------------------------------------------
if [[ "$USE_RUST_BACKEND" == "1" ]]; then
    USER_BIN_LINK="$HOME/.local/bin/cua-driver"
    # Canonical bundle path. The Rust install replaces the retired Swift app at
    # this path, but uses the new `com.meta.musecode.cua.driver` bundle ID.
    APP_BUNDLE="/Applications/CuaDriver.app"
    # Legacy bundle path from earlier Rust releases that coexisted with
    # Swift under a separate name. Cleaned up if found.
    LEGACY_APP_BUNDLE="/Applications/CuaDriverRs.app"
    # Canonical package home is ~/.cua-driver (renamed from ~/.cua-driver-rs
    # in v0.2.16 / PR #1644). The old name is swept too — uninstall.sh
    # was missed in that rename and kept defaulting to the stale dir, so a
    # current install left nothing matching and the whole uninstall no-op'd.
    HOME_DIR="${CUA_DRIVER_HOME:-${CUA_DRIVER_RS_HOME:-$HOME/.cua-driver}}"
    LEGACY_HOME_DIR="$HOME/.cua-driver-rs"
    if ! validate_release_home_dir "$HOME_DIR" "$HOME"; then
        exit 2
    fi
    if ! validate_release_home_dir "$LEGACY_HOME_DIR" "$HOME"; then
        exit 2
    fi
    # The versioned package store (`packages/releases/*` + `current`) is
    # written only by the Rust install-local / self-updater path — it's the
    # one unambiguous on-disk Rust discriminator now that the .app path is
    # shared with the retired Swift driver.
    PACKAGES_DIR="$HOME_DIR/packages"
    LAUNCHAGENT_PLIST="$HOME/Library/LaunchAgents/com.trycua.cua-driver.plist"
    LEGACY_LAUNCHAGENT_PLIST="$HOME/Library/LaunchAgents/com.trycua.cua-driver-rs.plist"
    SYSTEMD_USER_UNIT="$HOME/.config/systemd/user/cua-driver.service"
    LEGACY_SYSTEMD_USER_UNIT="$HOME/.config/systemd/user/cua-driver-rs.service"
    SKILL_PACK_NAME="cua-driver"
    # Pre-rename skill pack name — swept alongside the current one so
    # users who installed under the legacy name end up clean after
    # `uninstall.sh --backend=rust`.
    LEGACY_SKILL_PACK_NAME="cua-driver-rs"

    # Rust-install marker. The Rust bundle path `/Applications/CuaDriver.app`
    # is shared with the retired Swift driver's install path, so we can't use
    # that path alone as a discriminator — a Swift-only Mac
    # that runs `uninstall.sh --backend=rust` by mistake would lose its
    # Swift bundle, symlink, and Claude MCP registrations. This marker says
    # "there's at least one unambiguously-Rust artifact on disk." We gate
    # every shared-path removal below on it.
    #
    # Markers (any one suffices):
    #   - ~/.cua-driver/packages/ exists (Rust install-local / updater store)
    #   - ~/.cua-driver-rs/ exists (legacy Rust state dir, pre-rename)
    #   - /Applications/CuaDriverRs.app exists (legacy bundle, pre-rename)
    #   - current or legacy LaunchAgent plist / systemd unit exists
    #     (autostart was used)
    #   - current telemetry identity/registration marker exists (release
    #     installs on macOS live in /Applications and have no packages dir)
    APP_BUNDLE_OWNED=0
    APP_BUNDLE_ID=""
    APP_BUNDLE_TEAM_ID=""
    LEGACY_APP_BUNDLE_OWNED=0
    if [[ "$OS" == "Darwin" && "$PRODUCTION_TEAM_ID" != "$PINNED_PRODUCTION_TEAM_ID" ]]; then
        printf 'error: CUA_DRIVER_PRODUCTION_TEAM_ID does not match the pinned Muse Code production Team ID\n' >&2
        exit 1
    fi
    if [[ "$OS" == "Darwin" && ( -e "$APP_BUNDLE" || -L "$APP_BUNDLE" ) ]]; then
        APP_BUNDLE_ID="$(macos_plist_value "$PLISTBUDDY" "$APP_BUNDLE" CFBundleIdentifier || true)"
        case "$APP_BUNDLE_ID" in
            "$RELEASE_BUNDLE_ID")
                if ! validate_apple_team_id "$PRODUCTION_TEAM_ID"; then
                    printf 'error: CUA_DRIVER_PRODUCTION_TEAM_ID must be the approved 10-character Apple Team ID before uninstalling %s\n' "$APP_BUNDLE" >&2
                    exit 1
                fi
                APP_BUNDLE_TEAM_ID="$PRODUCTION_TEAM_ID"
                ;;
            "$LEGACY_RELEASE_BUNDLE_ID")
                if ! validate_apple_team_id "$LEGACY_PRODUCTION_TEAM_ID"; then
                    printf 'error: CUA_DRIVER_LEGACY_TEAM_ID must be a 10-character Apple Team ID before uninstalling %s\n' "$APP_BUNDLE" >&2
                    exit 1
                fi
                APP_BUNDLE_TEAM_ID="$LEGACY_PRODUCTION_TEAM_ID"
                ;;
            *)
                printf 'error: refusing to execute or remove unverified app at shared path %s\n' "$APP_BUNDLE" >&2
                exit 1
                ;;
        esac
        if macos_release_app_is_owned "$APP_BUNDLE" "$APP_BUNDLE_ID" \
            "$RELEASE_EXECUTABLE" "$APP_BUNDLE_TEAM_ID" "$CODESIGN" "$PLISTBUDDY" "$SPCTL"; then
            APP_BUNDLE_OWNED=1
        else
            printf 'error: refusing to execute or remove app with an unverified signer at shared path %s\n' "$APP_BUNDLE" >&2
            exit 1
        fi
    fi
    if [[ "$OS" == "Darwin" && ( -e "$LEGACY_APP_BUNDLE" || -L "$LEGACY_APP_BUNDLE" ) ]]; then
        if ! validate_apple_team_id "$LEGACY_PRODUCTION_TEAM_ID"; then
            printf 'error: CUA_DRIVER_LEGACY_TEAM_ID must be a 10-character Apple Team ID before uninstalling %s\n' "$LEGACY_APP_BUNDLE" >&2
            exit 1
        fi
        if macos_release_app_is_owned "$LEGACY_APP_BUNDLE" "$LEGACY_RS_BUNDLE_ID" \
            "$RELEASE_EXECUTABLE" "$LEGACY_PRODUCTION_TEAM_ID" "$CODESIGN" "$PLISTBUDDY" "$SPCTL"; then
            LEGACY_APP_BUNDLE_OWNED=1
        else
            printf 'error: refusing to execute or remove unverified legacy app %s\n' "$LEGACY_APP_BUNDLE" >&2
            exit 1
        fi
    fi

    RUST_INSTALL_PRESENT=0
    if [[ -d "$PACKAGES_DIR" || -d "$LEGACY_HOME_DIR" || "$APP_BUNDLE_OWNED" == "1" || "$LEGACY_APP_BUNDLE_OWNED" == "1" || -f "$LAUNCHAGENT_PLIST" || -f "$LEGACY_LAUNCHAGENT_PLIST" || -f "$SYSTEMD_USER_UNIT" || -f "$LEGACY_SYSTEMD_USER_UNIT" || -f "$HOME_DIR/.telemetry_id" || -f "$HOME_DIR/.installation_recorded" ]]; then
        RUST_INSTALL_PRESENT=1
    fi

    DAEMON_PID_FILE="$(daemon_pid_file_path)"
    DAEMON_STOP_HELPER="$(select_daemon_stop_helper || true)"

    if [[ "$RUST_INSTALL_PRESENT" == "1" ]]; then
        if ! release_supervisor stop; then
            log "could not stop the release supervisor; runtime preserved"
            exit 1
        fi
        if ! stop_release_daemon; then
            log "could not safely stop and verify the release daemon; runtime preserved"
            exit 1
        fi
        log "verified no running release cua-driver daemon"
    else
        log "no Rust install marker; leaving any running cua-driver process untouched"
    fi

    MACOS_HISTORY_ROOT="$HOME/Library/Application Support/cua-driver/computer-history"
    case "$OS" in
        Darwin) HISTORY_ROOT="$MACOS_HISTORY_ROOT" ;;
        Linux) HISTORY_ROOT="${XDG_STATE_HOME:-$HOME/.local/state}/cua-driver/computer-history" ;;
        *) HISTORY_ROOT="" ;;
    esac
    HISTORY_STATE_PRESENT=0
    if [[ -n "$HISTORY_ROOT" ]]; then
        _history_state_status=0
        history_state_present "$HISTORY_ROOT" || _history_state_status=$?
        case "$_history_state_status" in
            0) HISTORY_STATE_PRESENT=1 ;;
            1) ;;
            *) exit 1 ;;
        esac
        unset _history_state_status
    fi
    if [[ "$OS" == "Darwin" && "$PURGE_DATA" == "0" \
       && -d "$MACOS_HISTORY_ROOT" ]] \
       && directory_has_entries "$MACOS_HISTORY_ROOT" \
       && [[ "$APP_BUNDLE_ID" == "$LEGACY_RELEASE_BUNDLE_ID" \
          || "$LEGACY_APP_BUNDLE_OWNED" == "1" ]]; then
        printf 'error: refusing to remove the legacy Cua Driver identity while encrypted Computer History remains\n' >&2
        printf 'to preserve it, keep the authenticated legacy app; to destroy it safely, rerun with --purge\n' >&2
        exit 1
    fi

    # --- CLI symlink ---
    # Only remove ~/.local/bin/cua-driver when it resolves into a
    # cua-driver-rs install. Pre-rename installs at
    # /Applications/CuaDriverRs.app are unambiguously Rust and always
    # removed. Post-rename, the Rust install lives at
    # /Applications/CuaDriver.app — the SAME path the Swift driver
    # uses — so we only remove that link when $RUST_INSTALL_PRESENT.
    if [[ -L "$USER_BIN_LINK" ]]; then
        RESOLVED="$(resolve_link "$USER_BIN_LINK")"
        case "$RESOLVED" in
            *"CuaDriverRs.app"*|*"$HOME_DIR"*|*".cua-driver-rs"*)
                # Unambiguous Rust paths.
                rm -f "$USER_BIN_LINK"
                log "removed $USER_BIN_LINK -> $RESOLVED"
                ;;
            *"/Applications/CuaDriver.app"*)
                # Shared with the Swift driver — require a Rust marker.
                if [[ "$RUST_INSTALL_PRESENT" == "1" ]]; then
                    rm -f "$USER_BIN_LINK"
                    log "removed $USER_BIN_LINK -> $RESOLVED"
                else
                    log "$USER_BIN_LINK -> $RESOLVED (shared with Swift driver and no Rust marker on disk; skipping)"
                fi
                ;;
            *)
                log "$USER_BIN_LINK resolves to $RESOLVED (not a cua-driver-rs path; skipping)"
                ;;
        esac
    elif [[ -e "$USER_BIN_LINK" ]]; then
        log "$USER_BIN_LINK exists but is not a symlink (skipping; refusing to clobber a real file)"
    else
        log "no CLI symlink at $USER_BIN_LINK (skipping)"
    fi

    release_supervisor remove

    # Cryptographic history purge must run while the exact installed helper
    # executable still exists. The helper uses the production KeyProvider and
    # its own bundle-derived namespace, then takes the exclusive writer lease;
    # failure leaves the runtime and all retryable history state in place.
    if [[ "$PURGE_DATA" == "1" && ( "$OS" == "Darwin" || "$OS" == "Linux" ) ]]; then
        if ! purge_release_history_if_present "$HISTORY_ROOT"; then
            exit 1
        fi
    elif [[ "$OS" == "Darwin" ]]; then
        log "preserved encrypted Computer History if present; reinstall the same signed identity to reopen it or restore its purge helper"
    elif [[ "$OS" == "Linux" ]]; then
        log "preserved encrypted Computer History if present; reinstall the same release to reopen it or restore its purge helper"
    fi

    # --- Revoke TCC grants and unregister BEFORE removing each app ---
    # Use the identity actually verified above. This also clears the retired
    # com.trycua.driver rows when uninstalling a pre-migration canonical app.
    if [[ "$OS" == "Darwin" && "$APP_BUNDLE_OWNED" == "1" ]]; then
        if ! maybe_reset_tcc "$APP_BUNDLE" "$APP_BUNDLE_ID"; then
            log "permission or LaunchServices cleanup failed; preserved $APP_BUNDLE"
            exit 1
        fi
    fi
    if [[ "$OS" == "Darwin" && "$LEGACY_APP_BUNDLE_OWNED" == "1" ]]; then
        if ! maybe_reset_tcc "$LEGACY_APP_BUNDLE" "$LEGACY_RS_BUNDLE_ID"; then
            log "permission or LaunchServices cleanup failed; preserved $LEGACY_APP_BUNDLE"
            exit 1
        fi
    fi

    # --- .app bundle (macOS only) ---
    # Legacy /Applications/CuaDriverRs.app is unambiguously Rust and
    # always removed when present. /Applications/CuaDriver.app is the
    # current canonical Rust path BUT also where the Swift driver
    # lived (with legacy bundle id `com.trycua.driver`), so we only remove
    # it when $RUST_INSTALL_PRESENT — protects a Swift-only Mac from
    # losing its bundle if `uninstall.sh --experimental-rust` is run
    # by mistake.
    if [[ "$OS" == "Darwin" ]]; then
        if [[ -d "$LEGACY_APP_BUNDLE" ]]; then
            if [[ "$LEGACY_APP_BUNDLE_OWNED" == "1" ]]; then
                if ! macos_release_app_is_owned "$LEGACY_APP_BUNDLE" \
                    "$LEGACY_RS_BUNDLE_ID" "$RELEASE_EXECUTABLE" \
                    "$LEGACY_PRODUCTION_TEAM_ID" "$CODESIGN" "$PLISTBUDDY" "$SPCTL"; then
                    printf 'error: legacy app identity changed during cleanup; preserved %s\n' \
                        "$LEGACY_APP_BUNDLE" >&2
                    exit 1
                fi
                SUDO=""
                if [[ ! -w "$(dirname "$LEGACY_APP_BUNDLE")" ]]; then
                    SUDO="sudo"
                fi
                $SUDO rm -rf -- "$LEGACY_APP_BUNDLE"
                log "removed $LEGACY_APP_BUNDLE"
            else
                log "$LEGACY_APP_BUNDLE was not verified as a Cua Driver release; preserving it"
            fi
        else
            log "no app bundle at $LEGACY_APP_BUNDLE (skipping)"
        fi
        if [[ -d "$APP_BUNDLE" ]]; then
            if [[ "$APP_BUNDLE_OWNED" == "1" ]]; then
                if ! macos_release_app_is_owned "$APP_BUNDLE" \
                    "$APP_BUNDLE_ID" "$RELEASE_EXECUTABLE" \
                    "$APP_BUNDLE_TEAM_ID" "$CODESIGN" "$PLISTBUDDY" "$SPCTL"; then
                    printf 'error: app identity changed during cleanup; preserved %s\n' \
                        "$APP_BUNDLE" >&2
                    exit 1
                fi
                SUDO=""
                if [[ ! -w "$(dirname "$APP_BUNDLE")" ]]; then
                    SUDO="sudo"
                fi
                $SUDO rm -rf -- "$APP_BUNDLE"
                log "removed $APP_BUNDLE"
            else
                log "$APP_BUNDLE was not verified as a Cua Driver release; preserving it"
            fi
        else
            log "no app bundle at $APP_BUNDLE (skipping)"
        fi
    fi

    # --- Package home ---
    # A normal uninstall deliberately keeps the pseudonymous installation ID,
    # persisted telemetry preference, and install/release markers. This lets a
    # later reinstall be counted as a returning installation without sending
    # any events while disabled. `--purge` is the explicit identity reset.
    # All removal remains gated on the Rust marker so a mistaken invocation
    # cannot damage a Swift-only Mac's shared ~/.cua-driver state.
    if [[ -d "$HOME_DIR" ]]; then
        if [[ "$RUST_INSTALL_PRESENT" == "1" || "$PURGE_DATA" == "1" ]]; then
            if [[ "$PURGE_DATA" == "1" ]]; then
                # Never recursively remove an override root. Delete only
                # installer-owned children, then remove the directory itself
                # only when it is empty. This makes a mistaken broad override
                # non-destructive even when --purge was explicitly requested.
                rm -rf -- "$HOME_DIR/packages" "$HOME_DIR/skills" "$HOME_DIR/.release_installed"
                rm -f -- \
                    "$HOME_DIR/.installation_recorded" \
                    "$HOME_DIR/.telemetry_enabled" \
                    "$HOME_DIR/.telemetry_id" \
                    "$HOME_DIR/.telemetry_identity.lock" \
                    "$HOME_DIR/.telemetry_install_channel" \
                    "$HOME_DIR/.telemetry_lifecycle.lock" \
                    "$HOME_DIR/.telemetry_retry_after" \
                    "$HOME_DIR/.tcc-signing-identity" \
                    "$HOME_DIR/config.json" \
                    "$HOME_DIR/release-channel" \
                    "$HOME_DIR/serve.out.log" \
                    "$HOME_DIR/serve.err.log" \
                    "$HOME_DIR/version_check.json"
                if rmdir "$HOME_DIR" 2>/dev/null; then
                    log "purged empty package home $HOME_DIR"
                else
                    log "purged Cua Driver state from $HOME_DIR; preserved unrelated files"
                fi
            else
                # Remove only installer/runtime-owned payloads. Unknown files
                # and all telemetry state remain untouched.
                rm -rf "$HOME_DIR/packages" "$HOME_DIR/skills"
                rm -f \
                    "$HOME_DIR/.tcc-signing-identity" \
                    "$HOME_DIR/serve.out.log" \
                    "$HOME_DIR/serve.err.log"
                log "removed runtime payloads from $HOME_DIR"
                log "preserved telemetry identity, preference, and registration markers"
            fi
        else
            log "$HOME_DIR exists but no Rust marker on disk; leaving it (looks like a Swift-only / shared config dir)"
        fi
    else
        log "no package home at $HOME_DIR (skipping)"
    fi
    # Preserve legacy telemetry state during a normal uninstall so the
    # runtime's existing one-shot migration can carry the same identity into
    # ~/.cua-driver on reinstall.
    if [[ -d "$LEGACY_HOME_DIR" ]]; then
        if [[ "$PURGE_DATA" == "1" ]]; then
            rm -rf -- "$LEGACY_HOME_DIR/packages" "$LEGACY_HOME_DIR/skills" "$LEGACY_HOME_DIR/.release_installed"
            rm -f -- \
                "$LEGACY_HOME_DIR/.installation_recorded" \
                "$LEGACY_HOME_DIR/.telemetry_enabled" \
                "$LEGACY_HOME_DIR/.telemetry_id" \
                "$LEGACY_HOME_DIR/.telemetry_identity.lock" \
                "$LEGACY_HOME_DIR/.telemetry_install_channel" \
                "$LEGACY_HOME_DIR/.telemetry_lifecycle.lock" \
                "$LEGACY_HOME_DIR/.telemetry_retry_after" \
                "$LEGACY_HOME_DIR/.tcc-signing-identity" \
                "$LEGACY_HOME_DIR/config.json" \
                "$LEGACY_HOME_DIR/release-channel" \
                "$LEGACY_HOME_DIR/serve.out.log" \
                "$LEGACY_HOME_DIR/serve.err.log" \
                "$LEGACY_HOME_DIR/version_check.json"
            if rmdir "$LEGACY_HOME_DIR" 2>/dev/null; then
                log "purged empty legacy package home $LEGACY_HOME_DIR"
            else
                log "purged legacy Cua Driver state; preserved unrelated files in $LEGACY_HOME_DIR"
            fi
        else
            rm -rf "$LEGACY_HOME_DIR/packages" "$LEGACY_HOME_DIR/skills"
            rm -f \
                "$LEGACY_HOME_DIR/.tcc-signing-identity" \
                "$LEGACY_HOME_DIR/serve.out.log" \
                "$LEGACY_HOME_DIR/serve.err.log"
            log "removed legacy runtime payloads and preserved legacy telemetry state"
        fi
    fi

    # --- Swift-era macOS data dirs (leave nothing behind) ---
    # The .app path is shared with the retired Swift driver, so a default
    # (Rust) uninstall already removes the app at that path. Sweep
    # the two Swift-only support/cache dirs here too so one `uninstall.sh`
    # leaves nothing behind regardless of which backend originally installed
    # -- no second `--backend=swift` pass needed. Gated on the Rust marker
    # for the same reason the shared path is: a Swift-only Mac that runs
    # the default uninstall by mistake keeps its data.
    if [[ "$OS" == "Darwin" && "$RUST_INSTALL_PRESENT" == "1" ]]; then
        for SWIFT_DATA_DIR in \
            "$HOME/Library/Application Support/Cua Driver" \
            "$HOME/Library/Caches/cua-driver"; do
            if [[ -d "$SWIFT_DATA_DIR" ]]; then
                rm -rf "$SWIFT_DATA_DIR"
                log "removed $SWIFT_DATA_DIR"
            fi
        done
    fi

    # --- Agent skill symlinks ---
    # Only remove when the link is a symlink — never clobber a real
    # directory (a dev user with a hand-managed skills dir is safe).
    # We don't check the target here because `cua-driver skills install`
    # writes platform-dependent targets (the local copy under $HOME_DIR/
    # skills/cua-driver-rs/). The [[ -L ]] check is the load-bearing
    # safety bar.
    if [[ "$RUST_INSTALL_PRESENT" == "1" ]]; then
        for SKILL_LINK in \
            "$HOME/.claude/skills/$SKILL_PACK_NAME" \
            "$HOME/.agents/skills/$SKILL_PACK_NAME" \
            "$HOME/.openclaw/skills/$SKILL_PACK_NAME" \
            "$HOME/.config/opencode/skills/$SKILL_PACK_NAME" \
            "$HOME/.gemini/skills/$SKILL_PACK_NAME" \
            "$HOME/.hermes/skills/$SKILL_PACK_NAME" \
            "$HOME/.claude/skills/$LEGACY_SKILL_PACK_NAME" \
            "$HOME/.agents/skills/$LEGACY_SKILL_PACK_NAME" \
            "$HOME/.openclaw/skills/$LEGACY_SKILL_PACK_NAME" \
            "$HOME/.config/opencode/skills/$LEGACY_SKILL_PACK_NAME" \
            "$HOME/.gemini/skills/$LEGACY_SKILL_PACK_NAME" \
            "$HOME/.hermes/skills/$LEGACY_SKILL_PACK_NAME"; do
            if [[ -L "$SKILL_LINK" ]]; then
                rm -f "$SKILL_LINK"
                log "removed skill symlink $SKILL_LINK"
            elif [[ -d "$SKILL_LINK" ]]; then
                log "$SKILL_LINK is a real directory, not a symlink (skipping)"
            else
                log "no skill symlink at $SKILL_LINK (skipping)"
            fi
        done
    else
        log "no Rust install marker; leaving agent skill symlinks untouched"
    fi

    # --- Claude Code MCP registrations ---
    # Same scrub shape as the Swift branch, keyed on the cua-driver-rs
    # binary name + the per-platform install paths. Unrelated MCP
    # servers are left alone.
    CLAUDE_JSON="$HOME/.claude.json"
    if [[ -f "$CLAUDE_JSON" ]] && command -v python3 >/dev/null 2>&1; then
        PY_OUTPUT="$(
            CLAUDE_JSON="$CLAUDE_JSON" HOME_DIR="$HOME_DIR" RUST_INSTALL_PRESENT="$RUST_INSTALL_PRESENT" python3 <<'PY'
import json
import os
import shutil
import sys
import tempfile
import time

path = os.environ["CLAUDE_JSON"]
home_dir = os.environ.get("HOME_DIR", "")
rust_install_present = os.environ.get("RUST_INSTALL_PRESENT", "0") == "1"

try:
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
except Exception as exc:
    print(f"could not read Claude config {path}: {exc}", file=sys.stderr)
    raise SystemExit(0)

removed = []

def text_parts(value):
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [item for item in value if isinstance(item, str)]
    return []

def invokes_cua_driver_rs(server):
    if not isinstance(server, dict):
        return False
    parts = []
    parts.extend(text_parts(server.get("command")))
    parts.extend(text_parts(server.get("args")))
    joined = " ".join(parts)
    # Match the Rust-port-specific anchors: bundle name, package home,
    # explicit ".cua-driver-rs" segment. Plain "cua-driver" alone is
    # ambiguous (the Swift binary uses the same filename). The shared
    # /Applications/CuaDriver.app path is ALSO ambiguous (Rust took
    # over the Swift bundle id) — only count it as Rust when a Rust
    # install marker is on disk; otherwise it is almost certainly a
    # Swift registration we should not scrub.
    if home_dir and home_dir in joined:
        return True
    if "CuaDriverRs.app" in joined or ".cua-driver-rs" in joined or "cua-driver-rs" in joined:
        return True
    if rust_install_present and "/Applications/CuaDriver.app" in joined:
        return True
    return False

def should_remove(name, server):
    return name in {"cua-driver-rs"} or invokes_cua_driver_rs(server)

def scrub_servers(servers, scope):
    if not isinstance(servers, dict):
        return
    for name in list(servers.keys()):
        if should_remove(name, servers[name]):
            del servers[name]
            removed.append(f"{scope}:{name}")

scrub_servers(data.get("mcpServers"), "user")

projects = data.get("projects")
if isinstance(projects, dict):
    for project in projects.values():
        if isinstance(project, dict):
            scrub_servers(project.get("mcpServers"), "project")

if not removed:
    raise SystemExit(0)

backup = f"{path}.bak-cua-driver-rs-uninstall-{int(time.time())}"
shutil.copy2(path, backup)

directory = os.path.dirname(path) or "."
fd, tmp_path = tempfile.mkstemp(
    prefix=".claude.json.",
    suffix=".tmp",
    dir=directory,
    text=True,
)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
        f.write("\n")
    os.replace(tmp_path, path)
except Exception:
    try:
        os.unlink(tmp_path)
    except OSError:
        pass
    raise

print(f"removed Claude MCP registration(s): {', '.join(removed)}")
print(f"backed up Claude config to {backup}")
PY
        )"
        if [[ -n "$PY_OUTPUT" ]]; then
            while IFS= read -r line; do
                log "$line"
            done <<< "$PY_OUTPUT"
        else
            log "no Claude MCP registrations for cua-driver-rs found in $CLAUDE_JSON"
        fi
    else
        log "no Claude config cleanup via python3 (missing $CLAUDE_JSON or python3)"
    fi

    # Best-effort CLI cleanup. `claude mcp remove` only touches the
    # active project / user scopes — fine to run; it's a no-op when the
    # entries were already scrubbed above.
    if command -v claude >/dev/null 2>&1; then
        for SERVER in cua-driver-rs; do
            for SCOPE in local project user; do
                if claude mcp remove "$SERVER" -s "$SCOPE" >/dev/null 2>&1; then
                    log "removed Claude MCP server $SERVER from $SCOPE scope"
                fi
            done
        done
    else
        log "claude CLI not found (skipping Claude MCP CLI cleanup)"
    fi

    # --- Closing message ---
    # TCC grants were already revoked above, before the app was removed, so
    # the reset could still resolve the bundle id through LaunchServices.
    if [[ "$OS" == "Darwin" ]]; then
        echo ""
        echo "cua-driver uninstalled."
        if [[ "$PURGE_DATA" == "0" ]]; then
            if [[ "$HISTORY_STATE_PRESENT" == "1" ]]; then
                cat << 'HISTORYUNMSG'

Encrypted Computer History was preserved. To destroy it later, first reinstall
the same signed Cua Driver identity, then run uninstall.sh --purge while that
verified helper is still installed.
HISTORYUNMSG
            fi
            cat << 'TELEMETRYUNMSG'

Telemetry identity and preference were preserved for a future reinstall.
If no encrypted Computer History remains, delete telemetry later with:

  /bin/bash -c "$(curl -fsSL https://cua.ai/driver/uninstall.sh)" -- --purge
TELEMETRYUNMSG
        fi
        if [[ "$RESET_TCC" != "1" ]]; then
            cat << 'FINALUNMSG'

TCC grants (Accessibility + Screen Recording) remain in System
Settings > Privacy & Security because uninstall was run with --keep-tcc.
Reset them explicitly if you want a clean re-install flow:

  tccutil reset Accessibility com.meta.musecode.cua.driver
  tccutil reset ScreenCapture com.meta.musecode.cua.driver
FINALUNMSG
        fi
    else
        cat << 'FINALUNMSG'

cua-driver uninstalled.
FINALUNMSG
        if [[ "$PURGE_DATA" == "0" ]]; then
            cat << 'TELEMETRYUNMSG'

Telemetry identity and preference were preserved for a future reinstall.
To delete them too, re-run with --purge:

  /bin/bash -c "$(curl -fsSL https://cua.ai/driver/uninstall.sh)" -- --purge
TELEMETRYUNMSG
        fi
    fi
    exit 0
fi

USER_BIN_LINK="$HOME/.local/bin/cua-driver"
SYSTEM_BIN_LINK="/usr/local/bin/cua-driver"
APP_BUNDLE="/Applications/CuaDriver.app"
USER_DATA="$HOME/.cua-driver"
CONFIG_DIR="$HOME/Library/Application Support/Cua Driver"
CACHE_DIR="$HOME/Library/Caches/cua-driver"
# Legacy — remove if present from older installs.
LEGACY_UPDATE_SCRIPT="/usr/local/bin/cua-driver-update"
LEGACY_UPDATER_PLIST="$HOME/Library/LaunchAgents/com.trycua.cua_driver_updater.plist"

# CLI symlinks. Try the user-bin first (no sudo), then the legacy
# /usr/local/bin path (needs sudo on default macOS).
for BIN_LINK in "$USER_BIN_LINK" "$SYSTEM_BIN_LINK"; do
    if [[ -L "$BIN_LINK" ]] || [[ -e "$BIN_LINK" ]]; then
        SUDO=""
        [[ ! -w "$(dirname "$BIN_LINK")" ]] && SUDO="sudo"
        $SUDO rm -f "$BIN_LINK"
        log "removed $BIN_LINK"
    fi
done

# Legacy update script + LaunchAgent (present in installs before 0.0.6).
if [[ -f "$LEGACY_UPDATE_SCRIPT" ]]; then
    SUDO=""; [[ ! -w "$(dirname "$LEGACY_UPDATE_SCRIPT")" ]] && SUDO="sudo"
    $SUDO rm -f "$LEGACY_UPDATE_SCRIPT"
    log "removed legacy $LEGACY_UPDATE_SCRIPT"
fi
if [[ -f "$LEGACY_UPDATER_PLIST" ]]; then
    launchctl unload "$LEGACY_UPDATER_PLIST" 2>/dev/null || true
    rm -f "$LEGACY_UPDATER_PLIST"
    log "removed legacy $LEGACY_UPDATER_PLIST"
fi

# .app bundle (in /Applications, usually writable by the user).
if [[ -d "$APP_BUNDLE" ]]; then
    SUDO=""
    if [[ ! -w "$(dirname "$APP_BUNDLE")" ]]; then
        SUDO="sudo"
    fi
    $SUDO rm -rf "$APP_BUNDLE"
    log "removed $APP_BUNDLE"
else
    log "no app bundle at $APP_BUNDLE (skipping)"
fi

# User-data directory (telemetry id + install marker).
if [[ -d "$USER_DATA" ]]; then
    rm -rf "$USER_DATA"
    log "removed $USER_DATA"
else
    log "no user data at $USER_DATA (skipping)"
fi

# Persisted config.
if [[ -d "$CONFIG_DIR" ]]; then
    rm -rf "$CONFIG_DIR"
    log "removed $CONFIG_DIR"
else
    log "no config at $CONFIG_DIR (skipping)"
fi

# Cache / daemon state.
if [[ -d "$CACHE_DIR" ]]; then
    rm -rf "$CACHE_DIR"
    log "removed $CACHE_DIR"
else
    log "no cache at $CACHE_DIR (skipping)"
fi

# Agent skill symlinks (Claude Code + Codex). Only remove when the link
# is ours — a dev user pointing the symlink at a working copy of the
# repo keeps theirs untouched.
SKILL_TARGET_EXPECTED="$APP_BUNDLE/Contents/Resources/Skills/cua-driver"
for SKILL_LINK in \
    "$HOME/.claude/skills/cua-driver" \
    "$HOME/.agents/skills/cua-driver" \
    "$HOME/.openclaw/skills/cua-driver" \
    "$HOME/.config/opencode/skills/cua-driver" \
    "$HOME/.gemini/skills/cua-driver" \
    "$HOME/.hermes/skills/cua-driver"; do
    if [[ -L "$SKILL_LINK" ]] && [[ "$(readlink "$SKILL_LINK")" == "$SKILL_TARGET_EXPECTED" ]]; then
        rm -f "$SKILL_LINK"
        log "removed $SKILL_LINK"
    else
        log "no install-created skill symlink at $SKILL_LINK (skipping)"
    fi
done

# Claude Code MCP registrations. `claude mcp remove` only removes from
# the current project / user scopes, while ~/.claude.json can also
# contain stale project entries for other directories. Scrub only
# registrations explicitly named cua-driver or whose command points at
# a cua-driver binary, so unrelated servers named "computer-use" are
# left alone.
CLAUDE_JSON="$HOME/.claude.json"
if [[ -f "$CLAUDE_JSON" ]] && command -v python3 >/dev/null 2>&1; then
    PY_OUTPUT="$(
        CLAUDE_JSON="$CLAUDE_JSON" python3 <<'PY'
import json
import os
import shutil
import sys
import tempfile
import time

path = os.environ["CLAUDE_JSON"]

try:
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
except Exception as exc:
    print(f"could not read Claude config {path}: {exc}", file=sys.stderr)
    raise SystemExit(0)

removed = []

def text_parts(value):
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [item for item in value if isinstance(item, str)]
    return []

def invokes_cua_driver(server):
    if not isinstance(server, dict):
        return False
    parts = []
    parts.extend(text_parts(server.get("command")))
    parts.extend(text_parts(server.get("args")))
    joined = " ".join(parts)
    return "cua-driver" in joined or "CuaDriver.app" in joined

def should_remove(name, server):
    return name in {"cua-driver", "cua-computer-use"} or invokes_cua_driver(server)

def scrub_servers(servers, scope):
    if not isinstance(servers, dict):
        return
    for name in list(servers.keys()):
        if should_remove(name, servers[name]):
            del servers[name]
            removed.append(f"{scope}:{name}")

scrub_servers(data.get("mcpServers"), "user")

projects = data.get("projects")
if isinstance(projects, dict):
    for project in projects.values():
        if isinstance(project, dict):
            scrub_servers(project.get("mcpServers"), "project")

if not removed:
    raise SystemExit(0)

backup = f"{path}.bak-cua-driver-uninstall-{int(time.time())}"
shutil.copy2(path, backup)

directory = os.path.dirname(path) or "."
fd, tmp_path = tempfile.mkstemp(
    prefix=".claude.json.",
    suffix=".tmp",
    dir=directory,
    text=True,
)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
        f.write("\n")
    os.replace(tmp_path, path)
except Exception:
    try:
        os.unlink(tmp_path)
    except OSError:
        pass
    raise

print(f"removed Claude MCP registration(s): {', '.join(removed)}")
print(f"backed up Claude config to {backup}")
PY
    )"
    if [[ -n "$PY_OUTPUT" ]]; then
        while IFS= read -r line; do
            log "$line"
        done <<< "$PY_OUTPUT"
    else
        log "no Claude MCP registrations for cua-driver found in $CLAUDE_JSON"
    fi
else
    log "no Claude config cleanup via python3 (missing $CLAUDE_JSON or python3)"
fi

# Best-effort CLI cleanup for the active Claude project. This covers
# .mcp.json / current-working-directory scopes when present and is
# harmless when the entries were already removed above.
if command -v claude >/dev/null 2>&1; then
    for SERVER in cua-driver cua-computer-use; do
        for SCOPE in local project user; do
            if claude mcp remove "$SERVER" -s "$SCOPE" >/dev/null 2>&1; then
                log "removed Claude MCP server $SERVER from $SCOPE scope"
            fi
        done
    done
else
    log "claude CLI not found (skipping Claude MCP CLI cleanup)"
fi

maybe_reset_tcc

echo ""
echo "cua-driver uninstalled."
if [[ "$RESET_TCC" != "1" ]]; then
    cat << 'FINALUNMSG'

TCC grants (Accessibility + Screen Recording) remain in System
Settings > Privacy & Security because uninstall was run with --keep-tcc.
Reset them explicitly if you want a clean re-install flow:

  tccutil reset Accessibility com.meta.musecode.cua.driver
  tccutil reset ScreenCapture com.meta.musecode.cua.driver
FINALUNMSG
fi
