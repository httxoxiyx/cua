#!/usr/bin/env bash
# _install-rust.sh — private helper invoked by libs/cua-driver/scripts/install.sh
# (the canonical user-facing installer) for the default Rust implementation.
# Not intended for direct
# invocation; user-facing one-liners always go through the parent
# install.sh, which forwards args + sets up the lockfile.
#
# Downloads the latest cua-driver-rs release tarball from GitHub Releases
# and drops the binary into ~/.local/bin (or a path given via --bin-dir /
# CUA_DRIVER_RS_INSTALL_DIR). Sudo-free.
#
# This is the cross-platform cua-driver implementation for macOS / Linux /
# Windows via WSL or git-bash. The retired Swift implementation (macOS
# only) still ships separately under tag prefix `cua-driver-v*`; this
# helper is hard-pinned to `cua-driver-rs-v*` and will never pick it up.
#
# Canonical user-facing invocation (forwards here by default):
#   /bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"
#
# Flags:
#   --bin-dir <path>     install the visible binary/symlink to <path>
#                        instead of ~/.local/bin
#   --no-modify-path     skip auto-appending an `export PATH=...` line
#   --channel <name>     persist and install the latest stable or nightly release
#
# Env overrides:
#   CUA_DRIVER_RS_VERSION=0.1.2          pin a stable release
#   CUA_DRIVER_RS_VERSION=nightly-cua-driver-rs-v0.1.3-nightly.20260812.42
#                                        pin an exact nightly release
#   CUA_DRIVER_RS_INSTALL_DIR=PATH       same as --bin-dir; sets the visible
#                                        binary location
#   CUA_DRIVER_RS_BIN_DIR=PATH           legacy alias for INSTALL_DIR
#   CUA_DRIVER_RS_HOME=PATH              absolute, non-symlink package home
#                                        below HOME for versioned installs
#                                        (default ~/.cua-driver). Holds
#                                        packages/releases/<v>-<target>/ and
#                                        packages/current/ on Linux/Windows.
#                                        Renamed from ~/.cua-driver-rs in
#                                        v0.2.16 / PR #1644 — this release
#                                        installer was missed in that rename
#                                        and is reconciled here; a stale
#                                        ~/.cua-driver-rs is swept post-install.
#   CUA_DRIVER_RS_NO_MODIFY_PATH=1       same as --no-modify-path
#   CUA_DRIVER_RS_KEEP_VERSIONS=N        keep the N most recent per-version
#                                        release dirs after install; older
#                                        ones are deleted (default 5; set 0
#                                        to disable GC entirely). Per-target
#                                        — multi-arch dirs are pruned
#                                        independently of each other.
#   CUA_DRIVER_PRODUCTION_TEAM_ID=TEAMID  optional assertion; must equal the
#                                        pinned Muse Code production Team ID
#   CUA_DRIVER_EXPECTED_SOURCE_SHA=SHA    optional trusted release commit pin;
#                                        when set, build attestation must match
#   CUA_DRIVER_LEGACY_TEAM_ID=TEAMID      team allowed for the retired
#                                        com.trycua.driver migration source
#                                        (default YCK386LBJ7)
#
# On-disk layout (Linux; macOS keeps its .app-in-/Applications layout, see
# below):
#   $CUA_DRIVER_RS_HOME/
#     packages/
#       releases/
#         0.1.3-x86_64-unknown-linux-gnu/cua-driver   (per-version binary)
#         0.1.4-x86_64-unknown-linux-gnu/cua-driver
#       current/cua-driver -> ../releases/<active>/cua-driver  (active version)
#   $CUA_DRIVER_RS_INSTALL_DIR/cua-driver -> $HOME/packages/current/cua-driver
#
# Atomic upgrade: a new install drops the binary into a fresh per-version
# dir, then rename(2)-swaps the `current` symlink to point at it. A
# running daemon keeps its already-mmap'd binary open across the swap
# (open file handles survive). Rollback: re-point `current` at any older
# entry under `releases/`.
#
# Post-install GC trims the per-target release dirs so disk usage stays
# bounded (each release dir is ~15 MB, so an indefinite series of
# upgrades grows without bound). The N most-recent dirs are kept (default
# 5, override via CUA_DRIVER_RS_KEEP_VERSIONS); the dir that `current`
# resolves to is always preserved even if its mtime would otherwise drop
# it off the list. GC runs after the atomic swap so the about-to-be-active
# version is never a deletion candidate.
#
set -euo pipefail

# Reject elevation before sourcing or downloading any helper code. The
# installer elevates no operation and must act on the login user's HOME/TCC
# context only.
if [[ "${EUID:-$(id -u)}" == "0" || -n "${SUDO_UID:-}" || -n "${SUDO_USER:-}" ]]; then
    printf 'error: do not run the Cua Driver installer as root or with sudo; run it as the login user\n' >&2
    exit 77
fi

# --- Load shared daemon-cleanup helpers ---------------------------------
#
# Bash counterpart of install.ps1's `Import-CuaDriverInstallModule`. On
# a checked-out tree (`$BASH_SOURCE` points at a real file) we source
# the sibling _install-common.sh from disk. When this script is run via
# `curl ... | bash` (the production path forwarded from install.sh on
# Linux), there's no on-disk copy — fall back to curling the canonical
# raw URL and sourcing the downloaded tempfile.
#
# Failure here is non-fatal: the daemon-stop is a best-effort upgrade
# nicety, not load-bearing. If we can't load the helpers, define
# no-op stubs so the rest of the script can call them unconditionally.
_CUA_INSTALL_COMMON_URL="https://cua.ai/driver/_install-common.sh"
_cua_install_common_loaded=0
if [[ -n "${BASH_SOURCE[0]:-}" && "${BASH_SOURCE[0]}" != "-" && -f "${BASH_SOURCE[0]}" ]]; then
    _CUA_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
    if [[ -f "$_CUA_SCRIPT_DIR/_install-common.sh" ]]; then
        # shellcheck source=_install-common.sh
        . "$_CUA_SCRIPT_DIR/_install-common.sh" && _cua_install_common_loaded=1
    fi
fi
if [[ "$_cua_install_common_loaded" == "0" ]] && command -v curl >/dev/null 2>&1; then
    _cua_install_common_tmp="$(mktemp -t cua-install-common.XXXXXX 2>/dev/null || mktemp)"
    if curl -fsSL "$_CUA_INSTALL_COMMON_URL" -o "$_cua_install_common_tmp" 2>/dev/null; then
        # shellcheck source=/dev/null
        . "$_cua_install_common_tmp" && _cua_install_common_loaded=1
    fi
    rm -f "$_cua_install_common_tmp" 2>/dev/null || true
fi
if [[ "$_cua_install_common_loaded" == "0" ]]; then
    # Linux cannot safely replace a supervised executable without these
    # helpers: Restart can repopulate the process set after a name-based kill.
    printf 'warning: could not load _install-common.sh (on-disk + network)\n' >&2
    stop_cua_driver_daemons() {
        if [[ "$(uname -s 2>/dev/null || echo unknown)" == "Linux" ]]; then
            printf 'error: daemon supervisor quiescence helpers are unavailable; refusing the install\n' >&2
            return 1
        fi
    }
    show_cua_driver_daemon_survivors() { :; }
fi

REPO="trycua/cua"
BINARY_NAME="cua-driver"
TAG_PREFIX="cua-driver-rs-v"
NIGHTLY_TAG_PREFIX="nightly-cua-driver-rs-v"
# CUA_DRIVER_RS_INSTALL_DIR is the documented name; CUA_DRIVER_RS_BIN_DIR is
# the legacy alias kept for users with the old env in their shell rc.
BIN_DIR="${CUA_DRIVER_RS_INSTALL_DIR:-${CUA_DRIVER_RS_BIN_DIR:-$HOME/.local/bin}}"
# Canonical home is ~/.cua-driver (renamed from ~/.cua-driver-rs in v0.2.16 /
# PR #1644). The local installer (_install-local-rust.sh) and the runtime
# already default here; this release installer was missed in that rename and
# kept writing to the legacy ~/.cua-driver-rs, which is the root cause of the
# install collision (release wrote one home, install-local + runtime used the
# other). Reconcile the default here, keep accepting the CUA_DRIVER_RS_HOME
# override for back-compat, and sweep the stale legacy dir post-install below.
HOME_DIR="${CUA_DRIVER_RS_HOME:-$HOME/.cua-driver}"
# Pre-v0.2.16 home this installer used to write to. Swept after the new
# install is staged so a single rooted home (~/.cua-driver) is left behind.
LEGACY_HOME_DIR="$HOME/.cua-driver-rs"
NO_MODIFY_PATH="${CUA_DRIVER_RS_NO_MODIFY_PATH:-0}"
# Post-install GC: how many per-version release dirs to retain. Validated
# below as a non-negative integer; 0 means "never GC". The dir that
# `current` resolves to is always preserved regardless of cutoff.
KEEP_VERSIONS_DEFAULT=5
KEEP_VERSIONS="${CUA_DRIVER_RS_KEEP_VERSIONS:-$KEEP_VERSIONS_DEFAULT}"
CHANNEL_ARG=""
CHANNEL_EXPLICIT=0

# macOS-only: name and install location of the .app bundle that wraps
# the bare binary so the TCC auto-relaunch path in `cua-driver mcp` has
# a stable bundle id (com.meta.musecode.cua.driver) to attribute the daemon to.
# See libs/cua-driver/rust/scripts/CuaDriverBundle/Contents/Info.plist and
# the matching docs on `cua-driver mcp`'s auto-relaunch behavior.
# The retired Swift driver used the same path with `com.trycua.driver`.
# Replacing it with the Muse Code bundle ID is an identity migration: the app
# path remains stable, but TCC grants must be requested for the new identity.
APP_NAME="CuaDriver.app"
APP_DEST="/Applications/$APP_NAME"
PRODUCTION_BUNDLE_ID="com.meta.musecode.cua.driver"
PINNED_PRODUCTION_TEAM_ID="4W5TH4RKQ2"
PRODUCTION_TEAM_ID="${CUA_DRIVER_PRODUCTION_TEAM_ID:-$PINNED_PRODUCTION_TEAM_ID}"
LEGACY_PRODUCTION_BUNDLE_ID="com.trycua.driver"
LEGACY_RS_APP_DEST="/Applications/CuaDriverRs.app"
LEGACY_RS_BUNDLE_ID="com.trycua.cuadriverrs"
LEGACY_PRODUCTION_TEAM_ID="${CUA_DRIVER_LEGACY_TEAM_ID:-YCK386LBJ7}"
MACOS_LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
MACOS_PLUTIL="/usr/bin/plutil"
MACOS_CODESIGN="/usr/bin/codesign"
MACOS_SPCTL="/usr/sbin/spctl"
MACOS_TCCUTIL="/usr/bin/tccutil"
MACOS_LSOF="/usr/sbin/lsof"
MACOS_LAUNCHCTL="/bin/launchctl"
MACOS_HISTORY_ROOT="$HOME/Library/Application Support/cua-driver/computer-history"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin-dir) BIN_DIR="$2"; shift 2 ;;
        --bin-dir=*) BIN_DIR="${1#*=}"; shift ;;
        --no-modify-path) NO_MODIFY_PATH=1; shift ;;
        --channel)
            [[ -n "${2:-}" ]] || { printf 'error: --channel requires stable or nightly\n' >&2; exit 2; }
            CHANNEL_ARG="$2"; CHANNEL_EXPLICIT=1; shift 2 ;;
        --channel=*) CHANNEL_ARG="${1#*=}"; CHANNEL_EXPLICIT=1; shift ;;
        *) shift ;;
    esac
done

reject_root_install_invocation() {
    local effective_uid="$1"
    local sudo_uid="${2:-}"
    local sudo_user="${3:-}"
    if [[ "$effective_uid" == "0" || -n "$sudo_uid" || -n "$sudo_user" ]]; then
        printf 'error: do not run the Cua Driver installer as root or with sudo; run it as the login user\n' >&2
        return 77
    fi
}

if ! reject_root_install_invocation "$EUID" "${SUDO_UID:-}" "${SUDO_USER:-}"; then
    exit 77
fi

validate_release_install_home_dir() {
    local home_dir="$1" user_home="${2%/}" variable_name="$3"
    local resolved_home resolved_dir relative prefix component child
    local old_ifs="$IFS"
    local components=()

    case "$user_home" in
        /*) ;;
        *) printf 'error: HOME must be an absolute path\n' >&2; return 1 ;;
    esac
    [[ -n "$user_home" && "$user_home" != "/" ]] || {
        printf 'error: refusing unsafe HOME: %s\n' "${user_home:-<empty>}" >&2
        return 1
    }
    case "$home_dir" in
        /*) ;;
        *) printf 'error: %s must be an absolute path\n' "$variable_name" >&2; return 1 ;;
    esac
    case "$home_dir" in
        /|"$user_home"|"$user_home"/|*//*|*/../*|*/..|*/./*|*/.)
            printf 'error: refusing unsafe %s: %s\n' "$variable_name" "$home_dir" >&2
            return 1
            ;;
    esac
    [[ "$home_dir" != *$'\n'* && "$home_dir" != *$'\r'* ]] || {
        printf 'error: refusing %s containing a line break\n' "$variable_name" >&2
        return 1
    }
    resolved_home="$(CDPATH= cd -- "$user_home" 2>/dev/null && pwd -P)" || {
        printf 'error: could not resolve HOME safely: %s\n' "$user_home" >&2
        return 1
    }
    case "$home_dir" in
        "$user_home"/*) ;;
        *)
            printf 'error: %s must remain inside HOME (%s): %s\n' \
                "$variable_name" "$resolved_home" "$home_dir" >&2
            return 1
            ;;
    esac

    # Reject every symlink/non-directory component below HOME, including a
    # not-yet-created final directory's existing ancestors. This prevents
    # mkdir -p, release pruning, and rollback cleanup from escaping HOME.
    relative="${home_dir#"$user_home"/}"
    prefix="$user_home"
    IFS='/' read -r -a components <<< "$relative"
    IFS="$old_ifs"
    for component in "${components[@]}"; do
        [[ -n "$component" && "$component" != "." && "$component" != ".." ]] || return 1
        prefix="$prefix/$component"
        if [[ -L "$prefix" || ( -e "$prefix" && ! -d "$prefix" ) ]]; then
            printf 'error: refusing symlink or non-directory %s component: %s\n' \
                "$variable_name" "$prefix" >&2
            return 1
        fi
    done
    if [[ -d "$home_dir" ]]; then
        resolved_dir="$(CDPATH= cd -- "$home_dir" 2>/dev/null && pwd -P)" || return 1
        case "$resolved_dir" in
            "$resolved_home"/*) ;;
            *) printf 'error: %s resolves outside HOME: %s\n' "$variable_name" "$resolved_dir" >&2; return 1 ;;
        esac
    fi
    for child in "$home_dir/packages" "$home_dir/packages/releases"; do
        if [[ -L "$child" || ( -e "$child" && ! -d "$child" ) ]]; then
            printf 'error: refusing unsafe managed install directory: %s\n' "$child" >&2
            return 1
        fi
    done
}

validate_release_install_home_dir "$HOME_DIR" "$HOME" CUA_DRIVER_RS_HOME || exit 2
validate_release_install_home_dir "$LEGACY_HOME_DIR" "$HOME" legacy-package-home || exit 2

# Validate KEEP_VERSIONS up front so a typo (e.g. "five") falls back to
# the default instead of silently disabling GC. Accepts any non-negative
# integer; 0 is the documented "never GC" sentinel.
if ! [[ "$KEEP_VERSIONS" =~ ^[0-9]+$ ]]; then
    printf 'warning: CUA_DRIVER_RS_KEEP_VERSIONS=%s is not a non-negative integer; falling back to %d\n' \
        "$KEEP_VERSIONS" "$KEEP_VERSIONS_DEFAULT" >&2
    KEEP_VERSIONS="$KEEP_VERSIONS_DEFAULT"
fi

BIN_LINK="$BIN_DIR/$BINARY_NAME"
TMP_DIR=$(mktemp -d)

log() { printf '==> %s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; }

validate_apple_team_id() {
    [[ "$1" =~ ^[A-Z0-9]{10}$ ]]
}

directory_has_entries() {
    local directory="$1"
    local entry
    for entry in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
        if [[ -e "$entry" || -L "$entry" ]]; then
            return 0
        fi
    done
    return 1
}

macos_bundle_value() {
    local app="$1"
    local key="$2"
    /usr/libexec/PlistBuddy -c "Print :$key" \
        "$app/Contents/Info.plist" 2>/dev/null
}

# Authenticate an app before it can replace, execute from, or justify removal
# of a shared /Applications path. Bundle plist values alone are mutable and are
# never treated as proof of ownership.
macos_verify_release_app() {
    local app="$1"
    local expected_bundle_id="$2"
    local expected_executable="$3"
    local expected_team_id="$4"
    local require_notarization="${5:-1}"
    local actual_bundle_id actual_executable actual_team_id requirement spctl_output

    [[ -d "$app" && ! -L "$app" ]] || return 1
    [[ -f "$app/Contents/Info.plist" && ! -L "$app/Contents/Info.plist" ]] || return 1

    actual_bundle_id="$(macos_bundle_value "$app" CFBundleIdentifier || true)"
    actual_executable="$(macos_bundle_value "$app" CFBundleExecutable || true)"
    [[ "$actual_bundle_id" == "$expected_bundle_id" ]] || return 1
    [[ "$actual_executable" == "$expected_executable" ]] || return 1
    [[ -f "$app/Contents/MacOS/$expected_executable" \
       && ! -L "$app/Contents/MacOS/$expected_executable" \
       && -x "$app/Contents/MacOS/$expected_executable" ]] || return 1

    command -v "$MACOS_CODESIGN" >/dev/null 2>&1 || return 1
    "$MACOS_CODESIGN" --verify --deep --strict "$app" >/dev/null 2>&1 || return 1
    actual_team_id="$("$MACOS_CODESIGN" -d --verbose=4 "$app" 2>&1 \
        | sed -n 's/^TeamIdentifier=//p' | head -n 1)"
    [[ "$actual_team_id" == "$expected_team_id" ]] || return 1
    requirement="anchor apple generic and identifier \"$expected_bundle_id\" and certificate leaf[subject.OU] = \"$expected_team_id\""
    "$MACOS_CODESIGN" --verify --deep --strict -R "=$requirement" "$app" \
        >/dev/null 2>&1 || return 1

    # A valid Developer ID signature is insufficient by itself. Gate release
    # installation on the same Gatekeeper assessment users receive at launch,
    # which includes notarization under the host's active macOS policy.
    if [[ "$require_notarization" == "1" ]]; then
        command -v "$MACOS_SPCTL" >/dev/null 2>&1 || return 1
        spctl_output="$("$MACOS_SPCTL" --assess --type execute --verbose=4 "$app" 2>&1)" \
            || return 1
        [[ "$spctl_output" == *"source=Notarized Developer ID"* ]]
    else
        [[ "$require_notarization" == "0" ]]
    fi
}

# Execute the private, side-effect-free attestation only after the enclosing
# bundle has passed signer and notarization verification. The signature seals
# this exact executable; its embedded build identity must agree with the
# installer's independently configured trust pins before any live app moves.
macos_verify_build_attestation() {
    local app="$1"
    local expected_bundle_id="$2"
    local expected_team_id="$3"
    local expected_version="$4"
    local expected_source_sha="${5:-}"
    local binary="$app/Contents/MacOS/$BINARY_NAME"
    local raw="$TMP_DIR/staged-build-attestation.json"
    local plist="$TMP_DIR/staged-build-attestation.plist"
    local size schema bundle_id team_id binary_version source_sha plugin_managed
    local source_sha_normalized expected_source_sha_normalized

    [[ -x "$MACOS_PLUTIL" ]] || return 1
    [[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] || return 1
    if ! "$binary" __build-attestation >"$raw" 2>/dev/null; then
        return 1
    fi
    size="$(wc -c < "$raw" | tr -d '[:space:]')"
    [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le 16384 ]] || return 1
    "$MACOS_PLUTIL" -convert xml1 -o "$plist" "$raw" >/dev/null 2>&1 \
        || return 1
    schema="$("$MACOS_PLUTIL" -extract schema_version raw -o - "$plist" 2>/dev/null)" \
        || return 1
    bundle_id="$("$MACOS_PLUTIL" -extract bundle_id raw -o - "$plist" 2>/dev/null)" \
        || return 1
    team_id="$("$MACOS_PLUTIL" -extract production_team_id raw -o - "$plist" 2>/dev/null)" \
        || return 1
    binary_version="$("$MACOS_PLUTIL" -extract binary_version raw -o - "$plist" 2>/dev/null)" \
        || return 1
    source_sha="$("$MACOS_PLUTIL" -extract source_sha raw -o - "$plist" 2>/dev/null)" \
        || return 1
    plugin_managed="$("$MACOS_PLUTIL" -extract plugin_managed raw -o - "$plist" 2>/dev/null)" \
        || return 1

    [[ "$schema" == "1" ]] || return 1
    [[ "$bundle_id" == "$expected_bundle_id" ]] || return 1
    [[ "$team_id" == "$expected_team_id" ]] || return 1
    [[ "$binary_version" == "$expected_version" ]] || return 1
    [[ "$source_sha" =~ ^[0-9A-Fa-f]{40}([0-9A-Fa-f]{24})?$ ]] || return 1
    # The standalone installer must never accept the plugin-managed flavor.
    # That binary deliberately refuses ordinary bare/LaunchServices startup
    # and is activated only through the Marketplace host contract.
    [[ "$plugin_managed" == "false" ]] || return 1
    if [[ -n "$expected_source_sha" ]]; then
        [[ "$expected_source_sha" =~ ^[0-9A-Fa-f]{40}([0-9A-Fa-f]{24})?$ ]] \
            || return 1
        source_sha_normalized="$(printf '%s' "$source_sha" | tr '[:upper:]' '[:lower:]')"
        expected_source_sha_normalized="$(printf '%s' "$expected_source_sha" | tr '[:upper:]' '[:lower:]')"
        [[ "$source_sha_normalized" == "$expected_source_sha_normalized" ]] || return 1
    fi
}

macos_register_app() {
    local app="$1"
    [[ -x "$MACOS_LSREGISTER" ]] \
        && "$MACOS_LSREGISTER" -f "$app" >/dev/null 2>&1
}

macos_unregister_app() {
    local app="$1"
    [[ -x "$MACOS_LSREGISTER" ]] \
        && "$MACOS_LSREGISTER" -u "$app" >/dev/null 2>&1
}

macos_launchagent_state() {
    local label="$1" uid status=0
    [[ -x "$MACOS_LAUNCHCTL" ]] || return 2
    uid="$(id -u 2>/dev/null || true)"
    [[ "$uid" =~ ^[0-9]+$ ]] || return 2
    "$MACOS_LAUNCHCTL" print "gui/$uid/$label" >/dev/null 2>&1 || status=$?
    case "$status" in
        0) return 0 ;;
        113) return 1 ;;
        *) return 2 ;;
    esac
}

macos_stop_and_verify_launchagent() {
    local plist="$1" label="$2" uid state=0
    [[ -x "$MACOS_LAUNCHCTL" ]] || {
        err "launchctl is unavailable; daemon supervisor quiescence cannot be verified"
        return 1
    }
    uid="$(id -u 2>/dev/null || true)"
    [[ "$uid" =~ ^[0-9]+$ ]] || {
        err "current user identity is unavailable; daemon supervisor quiescence cannot be verified"
        return 1
    }
    if [[ -f "$plist" ]]; then
        "$MACOS_LAUNCHCTL" unload "$plist" >/dev/null 2>&1 || true
    fi
    state=0
    macos_launchagent_state "$label" || state=$?
    if [[ "$state" == "0" ]]; then
        if ! "$MACOS_LAUNCHCTL" bootout "gui/$uid/$label" >/dev/null 2>&1; then
            err "could not stop loaded launchd job $label"
            return 1
        fi
    elif [[ "$state" != "1" ]]; then
        err "could not inspect launchd job $label"
        return 1
    fi
    state=0
    macos_launchagent_state "$label" || state=$?
    if [[ "$state" != "1" ]]; then
        err "launchd job $label remains active or unverifiable"
        return 1
    fi
}

macos_stop_and_verify_install_supervisors() {
    macos_stop_and_verify_launchagent \
        "$HOME/Library/LaunchAgents/com.trycua.cua-driver.plist" \
        "com.trycua.cua-driver" \
        || return 1
    macos_stop_and_verify_launchagent \
        "$HOME/Library/LaunchAgents/com.trycua.cua-driver-rs.plist" \
        "com.trycua.cua-driver-rs"
}

# Return the source form of an app's designated code-signing requirement.
macos_designated_requirement() {
    "$MACOS_CODESIGN" -d -r- "$1" 2>/dev/null \
        | sed -n -e 's/^designated => //p' -e 's/^# designated => //p'
}

# TCC stores the previous app's requirement, not just its bundle identifier.
# Ask Security.framework (through codesign) whether the replacement satisfies
# that exact requirement instead of comparing requirement text or cdhashes.
macos_requirement_compatibility() {
    local previous_requirement="$1"
    local candidate_app="$2"
    local codesign_output
    local codesign_status

    if [[ -z "$previous_requirement" ]]; then
        printf '%s' "unknown"
    elif codesign_output="$("$MACOS_CODESIGN" --verify --deep --strict \
            -R "=$previous_requirement" "$candidate_app" 2>&1)"; then
        printf '%s' "compatible"
    else
        codesign_status=$?
        if [[ "$codesign_status" == "3" ]]; then
            printf '%s' "incompatible"
        else
            printf 'warning: could not evaluate the previous code-signing requirement (codesign status %s); preserving TCC rows\n' \
                "$codesign_status" >&2
            [[ -z "$codesign_output" ]] \
                || printf 'warning: codesign: %s\n' "$codesign_output" >&2
            printf '%s' "unknown"
        fi
    fi
}

# Reset only the permissions Cua Driver itself consumes, and only after the
# newly installed bundle has been verified and registered with LaunchServices.
macos_reset_tcc_after_requirement_change() {
    local compatibility="$1"
    local bundle_id="com.meta.musecode.cua.driver"
    local failed_services=""
    local service

    [[ "$compatibility" == "incompatible" ]] || return 0
    if ! command -v "$MACOS_TCCUTIL" >/dev/null 2>&1; then
        err "tccutil is required to clear stale Cua Driver permission rows after its signing requirement changed"
        return 1
    fi
    for service in Accessibility ScreenCapture AppleEvents; do
        if ! "$MACOS_TCCUTIL" reset "$service" "$bundle_id" >/dev/null 2>&1; then
            failed_services="$failed_services $service"
        fi
    done
    if [[ -n "$failed_services" ]]; then
        err "could not reset these TCC services for $bundle_id:$failed_services"
        err "the replacement will be rolled back; after resolving tccutil, retry:"
        err "  tccutil reset Accessibility $bundle_id"
        err "  tccutil reset ScreenCapture $bundle_id"
        err "  tccutil reset AppleEvents $bundle_id"
        return 1
    fi

    log "the app signing requirement changed; cleared stale Accessibility, Screen Recording, and Automation rows"
    log "macOS authorization is required again: cua-driver permissions grant"
}

# The retired app must remain authenticated and registered until all of its
# scoped permission rows are reset. A partial reset is an error and the app is
# preserved so the operation can be retried safely.
macos_reset_legacy_tcc_before_migration() {
    local app="${1:-$APP_DEST}"
    local bundle_id="${2:-${LEGACY_PRODUCTION_BUNDLE_ID:-com.trycua.driver}}"
    local failed_services=""
    local service

    if ! command -v "$MACOS_TCCUTIL" >/dev/null 2>&1; then
        err "tccutil is required to clear legacy Cua Driver permission rows"
        return 1
    fi
    if ! macos_register_app "$app"; then
        err "could not register the authenticated legacy app before resetting its TCC rows"
        return 1
    fi
    for service in Accessibility ScreenCapture AppleEvents; do
        if ! "$MACOS_TCCUTIL" reset "$service" "$bundle_id" >/dev/null 2>&1; then
            failed_services="$failed_services $service"
        fi
    done
    if [[ -n "$failed_services" ]]; then
        err "could not reset these TCC services for $bundle_id:$failed_services"
        err "the legacy app was preserved so cleanup can be retried"
        return 1
    fi
    if ! macos_unregister_app "$app"; then
        err "could not unregister the authenticated legacy app after resetting its TCC rows"
        err "the legacy app was preserved so cleanup can be retried"
        return 1
    fi
    log "cleared legacy Cua Driver Accessibility, Screen Recording, and Automation rows"
}

macos_refuse_legacy_history_identity_transition() {
    local legacy_bundle_id="${1:-${LEGACY_PRODUCTION_BUNDLE_ID:-com.trycua.driver}}"
    local legacy_app="${2:-$APP_DEST}"
    [[ -d "$MACOS_HISTORY_ROOT" ]] || return 0
    directory_has_entries "$MACOS_HISTORY_ROOT" || return 0

    err "cannot migrate $legacy_bundle_id while existing Computer History state is present"
    err "the new app identity cannot read the legacy app's Keychain-protected history key"
    err "to preserve history, stop here and wait for an explicit history migration tool"
    err "to discard it, run the currently installed trusted app before retrying:"
    err "  $legacy_app/Contents/MacOS/$BINARY_NAME history purge-offline --yes"
    return 1
}

macos_install_process_identity() {
    local pid="$1" identity=""
    if [[ -L "/proc/$pid/exe" ]]; then
        identity="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
        identity="${identity% (deleted)}"
    else
        [[ -x "$MACOS_LSOF" ]] || return 2
        identity="$("$MACOS_LSOF" -a -p "$pid" -d txt -Fn 2>/dev/null \
            | sed -n 's/^n//p' | sed -n '1p')" || return 2
    fi
    [[ -n "$identity" ]] || return 2
    printf '%s' "$identity"
}

macos_install_process_generation() {
    local generation
    generation="$(LC_ALL=C ps -ww -o lstart= -p "$1" 2>/dev/null)" || return 2
    generation="${generation#"${generation%%[![:space:]]*}"}"
    generation="${generation%"${generation##*[![:space:]]}"}"
    [[ -n "$generation" ]] || return 2
    printf '%s' "$generation"
}

macos_install_process_identity_is_owned() {
    local identity="$1" owned_path
    shift
    for owned_path in "$@"; do
        [[ -n "$owned_path" && "$identity" == "$owned_path" ]] && return 0
    done
    return 1
}

macos_install_owned_process_records() {
    local candidates="" pgrep_status=0 uid pid identity generation
    command -v pgrep >/dev/null 2>&1 || return 2
    uid="$(id -u 2>/dev/null || true)"
    [[ "$uid" =~ ^[0-9]+$ ]] || return 2
    candidates="$(pgrep -U "$uid" -f '(^|[[:space:]/])cua-driver([[:space:]]|$)' 2>/dev/null)" \
        || pgrep_status=$?
    case "$pgrep_status" in
        0) ;;
        1) return 0 ;;
        *) return 2 ;;
    esac
    while IFS= read -r pid; do
        [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 2
        kill -0 "$pid" 2>/dev/null || continue
        identity="$(macos_install_process_identity "$pid")" || return 2
        macos_install_process_identity_is_owned "$identity" "$@" || continue
        generation="$(macos_install_process_generation "$pid")" || return 2
        printf '%s\t%s\n' "$pid" "$generation"
    done <<< "$candidates"
}

macos_install_process_still_matches() {
    local pid="$1" expected_generation="$2"
    shift 2
    local identity generation
    kill -0 "$pid" 2>/dev/null || return 1
    generation="$(macos_install_process_generation "$pid")" || return 2
    [[ "$generation" == "$expected_generation" ]] || return 1
    identity="$(macos_install_process_identity "$pid")" || return 2
    macos_install_process_identity_is_owned "$identity" "$@"
}

macos_wait_for_owned_process_exit() {
    local pid="$1" generation="$2" attempts=0 status
    shift 2
    while :; do
        status=0
        macos_install_process_still_matches "$pid" "$generation" "$@" || status=$?
        case "$status" in
            0) ;;
            1) return 0 ;;
            *) return 2 ;;
        esac
        [[ "$attempts" -lt 20 ]] || return 1
        sleep 0.1 2>/dev/null || sleep 1
        attempts=$((attempts + 1))
    done
}

stop_authenticated_macos_daemons() {
    local records="" status=0 pid generation wait_status
    macos_stop_and_verify_install_supervisors || return 1
    records="$(macos_install_owned_process_records "$@")" || status=$?
    [[ "$status" == "0" ]] || {
        err "could not inspect installed Cua Driver processes before replacement"
        return 1
    }
    while IFS=$'\t' read -r pid generation; do
        [[ -n "$pid" ]] || continue
        status=0
        macos_install_process_still_matches "$pid" "$generation" "$@" || status=$?
        [[ "$status" != "2" ]] || return 1
        [[ "$status" == "0" ]] || continue
        kill -TERM "$pid" 2>/dev/null || true
        wait_status=0
        macos_wait_for_owned_process_exit "$pid" "$generation" "$@" || wait_status=$?
        [[ "$wait_status" != "2" ]] || return 1
        if [[ "$wait_status" == "1" ]]; then
            status=0
            macos_install_process_still_matches "$pid" "$generation" "$@" || status=$?
            [[ "$status" != "2" ]] || return 1
            [[ "$status" == "0" ]] && kill -KILL "$pid" 2>/dev/null || true
            wait_status=0
            macos_wait_for_owned_process_exit "$pid" "$generation" "$@" || wait_status=$?
            [[ "$wait_status" == "0" ]] || return 1
        fi
    done <<< "$records"
    # Re-check launchd after signalling. A KeepAlive job that survived the
    # first unload can otherwise respawn in the gap between the final PID scan
    # and the signing/history identity swap.
    macos_stop_and_verify_install_supervisors || return 1
    records=""
    status=0
    records="$(macos_install_owned_process_records "$@")" || status=$?
    if [[ "$status" != "0" || -n "$records" ]]; then
        err "verified Cua Driver process remains after authenticated shutdown"
        return 1
    fi
}

macos_stage_legacy_rs_removal() {
    MACOS_LEGACY_RS_BACKUP="${LEGACY_RS_APP_DEST}.install-backup.$$"
    if [[ -e "$MACOS_LEGACY_RS_BACKUP" || -L "$MACOS_LEGACY_RS_BACKUP" ]]; then
        err "temporary legacy backup path already exists: $MACOS_LEGACY_RS_BACKUP"
        return 1
    fi
    MACOS_LEGACY_RS_REMOVAL_STARTED=1
    if ! mv "$LEGACY_RS_APP_DEST" "$MACOS_LEGACY_RS_BACKUP"; then
        err "could not stage authenticated legacy CuaDriverRs.app for removal"
        return 1
    fi
}

# --- Concurrent-install lockfile ---------------------------------------
#
# A second install kicked off while a first is still running can race
# on the atomic `current` symlink swap and produce a half-installed
# state (e.g. the symlink points at a dir whose binary the first install
# hasn't finished copying). Serialize installs per $HOME_DIR with a
# process-level mutex.
#
# Primitive: mkdir on POSIX is atomic. The first install to create
# $LOCK_DIR holds the lock; concurrent attempts get EEXIST and poll.
# Released via trap on every exit path (success, error, signal) so
# a half-finished install always frees the lock for the next one.
#
# Stale-lock recovery: if the holder dies without releasing (kill -9,
# OOM, host reboot mid-install), the lock dir sits around forever and
# every subsequent install hangs. After $LOCK_STALE_AFTER_SECONDS of
# waiting we probe the holder's liveness via `kill -0 <pid>` (the pid
# is stamped into $LOCK_INFO right after acquisition) — if the holder
# is alive we keep waiting (slow download / wedged network is not the
# same as a crashed install and we must not yank the lock out from
# under a live process), if it's dead we force-release with a loud log
# and proceed. The alternative (hang forever) leaves users in a
# permanently wedged state with no clear recovery path beyond `rm -rf`
# on an internal-looking dir.
LOCK_PACKAGES_DIR="$HOME_DIR/packages"
LOCK_DIR="$LOCK_PACKAGES_DIR/.install.lock.d"
LOCK_INFO="$LOCK_DIR/info"
LOCK_POLL_INTERVAL_SECONDS=1
LOCK_STALE_AFTER_SECONDS=600

LOCK_HELD=0
MACOS_APP_SWAP_STARTED=0
MACOS_APP_HAD_PREVIOUS=0
MACOS_APP_INSTALL_COMMITTED=0
MACOS_APP_BACKUP=""
MACOS_APP_BACKUP_BUNDLE_ID=""
MACOS_APP_BACKUP_TEAM_ID=""
MACOS_LEGACY_RS_REMOVAL_STARTED=0
MACOS_LEGACY_RS_REMOVAL_COMMITTED=0
MACOS_LEGACY_RS_BACKUP=""

restore_macos_app_backup_on_exit() {
    [[ "$MACOS_APP_SWAP_STARTED" == "1" ]] || return 0

    if [[ "$MACOS_APP_INSTALL_COMMITTED" == "1" ]]; then
        if [[ -e "$MACOS_APP_BACKUP" || -L "$MACOS_APP_BACKUP" ]]; then
            if macos_verify_release_app "$MACOS_APP_BACKUP" \
                "$MACOS_APP_BACKUP_BUNDLE_ID" "$BINARY_NAME" \
                "$MACOS_APP_BACKUP_TEAM_ID" 0; then
                if ! rm -rf "$MACOS_APP_BACKUP"; then
                    printf 'warning: could not remove macOS install backup at %s\n' \
                        "$MACOS_APP_BACKUP" >&2
                fi
            else
                printf 'warning: preserving unauthenticated macOS install backup path at %s\n' \
                    "$MACOS_APP_BACKUP" >&2
            fi
        fi
        return 0
    fi

    if [[ "$MACOS_APP_HAD_PREVIOUS" == "1" ]]; then
        if [[ -e "$MACOS_APP_BACKUP" || -L "$MACOS_APP_BACKUP" ]]; then
            if ! macos_verify_release_app "$MACOS_APP_BACKUP" \
                "$MACOS_APP_BACKUP_BUNDLE_ID" "$BINARY_NAME" \
                "$MACOS_APP_BACKUP_TEAM_ID" 0; then
                printf 'warning: refusing to restore unauthenticated macOS install backup at %s\n' \
                    "$MACOS_APP_BACKUP" >&2
                return 1
            fi
            if [[ -e "$APP_DEST" || -L "$APP_DEST" ]]; then
                if macos_verify_release_app "$APP_DEST" "$PRODUCTION_BUNDLE_ID" \
                    "$BINARY_NAME" "$PRODUCTION_TEAM_ID"; then
                    if macos_unregister_app "$APP_DEST"; then
                        if ! rm -rf "$APP_DEST"; then
                            printf 'warning: could not remove failed replacement at %s\n' \
                                "$APP_DEST" >&2
                            return 1
                        fi
                    else
                        local failed_path="${APP_DEST}.failed-install.$$"
                        if [[ -e "$failed_path" || -L "$failed_path" ]] \
                           || ! mv "$APP_DEST" "$failed_path"; then
                            printf 'warning: could not unregister or preserve failed replacement at %s\n' \
                                "$APP_DEST" >&2
                            return 1
                        fi
                        printf 'warning: could not unregister failed replacement; preserved it at %s\n' \
                            "$failed_path" >&2
                    fi
                else
                    local failed_path="${APP_DEST}.failed-install.$$"
                    if ! macos_unregister_app "$APP_DEST"; then
                        printf 'warning: unauthenticated failed app was not registered or could not be unregistered at %s\n' \
                            "$APP_DEST" >&2
                    fi
                    if [[ -e "$failed_path" || -L "$failed_path" ]]; then
                        printf 'warning: failed-app preservation path already exists at %s\n' \
                            "$failed_path" >&2
                        return 1
                    fi
                    if ! mv "$APP_DEST" "$failed_path"; then
                        printf 'warning: refusing to delete or displace unauthenticated failed app at %s\n' \
                            "$APP_DEST" >&2
                        return 1
                    fi
                    printf 'warning: preserved unauthenticated failed app at %s\n' \
                        "$failed_path" >&2
                fi
            fi
            if ! mv "$MACOS_APP_BACKUP" "$APP_DEST"; then
                printf 'warning: could not restore previous CuaDriver.app from %s\n' \
                    "$MACOS_APP_BACKUP" >&2
                return 1
            fi
            if ! macos_register_app "$APP_DEST"; then
                printf 'warning: restored the previous CuaDriver.app but could not re-register it with LaunchServices\n' >&2
                return 1
            fi
            printf 'warning: interrupted macOS install restored the previous CuaDriver.app\n' >&2
        elif [[ -e "$APP_DEST" || -L "$APP_DEST" ]]; then
            # Legacy migration preparation unregisters the source before its
            # atomic move. If preparation or the move is interrupted, verify
            # the still-live source identity before restoring registration.
            if ! macos_verify_release_app "$APP_DEST" \
                "$MACOS_APP_BACKUP_BUNDLE_ID" "$BINARY_NAME" \
                "$MACOS_APP_BACKUP_TEAM_ID" 0; then
                printf 'warning: refusing to re-register unauthenticated previous CuaDriver.app at %s\n' \
                    "$APP_DEST" >&2
                return 1
            fi
            if ! macos_register_app "$APP_DEST"; then
                printf 'warning: could not re-register preserved previous CuaDriver.app with LaunchServices\n' >&2
                return 1
            fi
            printf 'warning: interrupted macOS install re-registered the preserved previous CuaDriver.app\n' >&2
        else
            printf 'warning: previous CuaDriver.app is missing from both its source and rollback paths\n' >&2
            return 1
        fi
    else
        # A first install has no app to restore. Remove only an authenticated
        # candidate owned by this installer; preserve anything else for review.
        if [[ -e "$APP_DEST" || -L "$APP_DEST" ]]; then
            if macos_verify_release_app "$APP_DEST" "$PRODUCTION_BUNDLE_ID" \
                "$BINARY_NAME" "$PRODUCTION_TEAM_ID"; then
                if ! macos_unregister_app "$APP_DEST"; then
                    printf 'warning: could not unregister failed first-install app at %s\n' \
                        "$APP_DEST" >&2
                    return 1
                fi
                if ! rm -rf "$APP_DEST"; then
                    printf 'warning: could not remove failed first-install app at %s\n' \
                        "$APP_DEST" >&2
                    return 1
                fi
            else
                if ! macos_unregister_app "$APP_DEST"; then
                    printf 'warning: unauthenticated failed first-install app was not registered or could not be unregistered at %s\n' \
                        "$APP_DEST" >&2
                fi
                printf 'warning: refusing to remove unauthenticated failed app at %s\n' \
                    "$APP_DEST" >&2
                return 1
            fi
        fi
    fi
}

restore_macos_legacy_rs_backup_on_exit() {
    [[ "$MACOS_LEGACY_RS_REMOVAL_STARTED" == "1" ]] || return 0
    if [[ "$MACOS_LEGACY_RS_REMOVAL_COMMITTED" == "1" ]]; then
        if [[ -e "$MACOS_LEGACY_RS_BACKUP" || -L "$MACOS_LEGACY_RS_BACKUP" ]]; then
            if ! macos_verify_release_app "$MACOS_LEGACY_RS_BACKUP" \
                "$LEGACY_RS_BUNDLE_ID" "$BINARY_NAME" \
                "$LEGACY_PRODUCTION_TEAM_ID" 0; then
                printf 'warning: preserving unauthenticated legacy CuaDriverRs backup at %s\n' \
                    "$MACOS_LEGACY_RS_BACKUP" >&2
                return 1
            fi
            if ! rm -rf "$MACOS_LEGACY_RS_BACKUP"; then
                printf 'warning: could not remove committed legacy CuaDriverRs backup at %s\n' \
                    "$MACOS_LEGACY_RS_BACKUP" >&2
                return 1
            fi
        fi
        return 0
    fi
    if [[ -e "$MACOS_LEGACY_RS_BACKUP" || -L "$MACOS_LEGACY_RS_BACKUP" ]]; then
        if ! macos_verify_release_app "$MACOS_LEGACY_RS_BACKUP" \
            "$LEGACY_RS_BUNDLE_ID" "$BINARY_NAME" \
            "$LEGACY_PRODUCTION_TEAM_ID" 0; then
            printf 'warning: preserving unauthenticated legacy CuaDriverRs backup at %s\n' \
                "$MACOS_LEGACY_RS_BACKUP" >&2
            return 1
        fi
        if [[ -e "$LEGACY_RS_APP_DEST" || -L "$LEGACY_RS_APP_DEST" ]]; then
            printf 'warning: refusing to overwrite path while restoring legacy CuaDriverRs.app: %s\n' \
                "$LEGACY_RS_APP_DEST" >&2
            return 1
        fi
        if ! mv "$MACOS_LEGACY_RS_BACKUP" "$LEGACY_RS_APP_DEST"; then
            printf 'warning: could not restore legacy CuaDriverRs.app from %s\n' \
                "$MACOS_LEGACY_RS_BACKUP" >&2
            return 1
        fi
        if ! macos_register_app "$LEGACY_RS_APP_DEST"; then
            printf 'warning: restored legacy CuaDriverRs.app but could not re-register it\n' >&2
            return 1
        fi
        printf 'warning: interrupted install restored legacy CuaDriverRs.app\n' >&2
    elif [[ -e "$LEGACY_RS_APP_DEST" || -L "$LEGACY_RS_APP_DEST" ]]; then
        if ! macos_verify_release_app "$LEGACY_RS_APP_DEST" \
            "$LEGACY_RS_BUNDLE_ID" "$BINARY_NAME" \
            "$LEGACY_PRODUCTION_TEAM_ID" 0; then
            printf 'warning: refusing to re-register unauthenticated legacy CuaDriverRs.app at %s\n' \
                "$LEGACY_RS_APP_DEST" >&2
            return 1
        fi
        if ! macos_register_app "$LEGACY_RS_APP_DEST"; then
            printf 'warning: could not re-register preserved legacy CuaDriverRs.app\n' >&2
            return 1
        fi
        printf 'warning: interrupted install re-registered preserved legacy CuaDriverRs.app\n' >&2
    else
        printf 'warning: legacy CuaDriverRs.app is missing from both its source and rollback paths\n' >&2
        return 1
    fi
}

release_install_lock() {
    if (( LOCK_HELD == 1 )); then
        rm -rf "$LOCK_DIR" 2>/dev/null || true
        LOCK_HELD=0
    fi
}

# Combine TMP_DIR cleanup with lock release in a single trap so neither
# clobbers the other. INT/TERM also re-raise via $? so the user-visible
# exit code reflects the signal.
cleanup_on_exit() {
    local rollback_status=0
    restore_macos_app_backup_on_exit || rollback_status=1
    restore_macos_legacy_rs_backup_on_exit || rollback_status=1
    if [[ "$rollback_status" != "0" ]]; then
        printf 'warning: macOS app rollback did not complete safely\n' >&2
    fi
    rm -rf "$TMP_DIR" 2>/dev/null || true
    release_install_lock
    return "$rollback_status"
}
trap cleanup_on_exit EXIT
trap 'cleanup_on_exit; trap - INT;  kill -INT  $$' INT
trap 'cleanup_on_exit; trap - TERM; kill -TERM $$' TERM

acquire_install_lock() {
    mkdir -p "$LOCK_PACKAGES_DIR"
    local waited=0
    while ! mkdir "$LOCK_DIR" 2>/dev/null; do
        if (( waited == 0 )); then
            log "another cua-driver-rs install is already in progress (lock at $LOCK_DIR); waiting..."
        fi
        sleep "$LOCK_POLL_INTERVAL_SECONDS"
        waited=$((waited + LOCK_POLL_INTERVAL_SECONDS))
        if (( waited >= LOCK_STALE_AFTER_SECONDS )); then
            # Don't yank the lock from a live install. Parse pid= from
            # the info file (written by the holder right after mkdir);
            # if that pid is still alive per kill -0, the holder is just
            # slow (big download, wedged network) — keep waiting. Only
            # reclaim when there's no live holder.
            #
            # Missing/unreadable info file → holder didn't get far enough
            # to stamp pid, so assume dead and reclaim. Unparseable pid
            # line → same. Either way we err on the side of progress
            # rather than hanging forever once the 600s window elapses.
            local holder_pid=""
            if [[ -r "$LOCK_INFO" ]]; then
                holder_pid=$(grep -E '^pid=' "$LOCK_INFO" 2>/dev/null | head -1 | cut -d= -f2)
            fi
            if [[ -n "$holder_pid" ]] && kill -0 "$holder_pid" 2>/dev/null; then
                log "lock at $LOCK_DIR still held by live pid $holder_pid; continuing to wait"
                # Reset waited so we re-check after another full window
                # rather than spamming this branch every poll interval.
                waited=0
                continue
            fi
            log "lock at $LOCK_DIR appears stale (>${LOCK_STALE_AFTER_SECONDS}s, no live holder); forcing release"
            rm -rf "$LOCK_DIR" 2>/dev/null || true
            waited=0
        fi
    done
    LOCK_HELD=1
    # Drop pid + ISO timestamp + invocation args into the lock dir so a
    # user investigating a stuck install can see who holds it (`cat
    # $HOME_DIR/packages/.install.lock.d/info`).
    {
        printf 'pid=%s\n' "$$"
        printf 'started=%s\n' "$(date -u +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date)"
        printf 'argv=%s\n' "$0 $*"
    } > "$LOCK_INFO" 2>/dev/null || true
}

acquire_install_lock "$@"

# Prune per-version release dirs under $RELEASES_DIR for the current
# $TARGET, keeping the N most recent (by mtime). The dir that the
# `current` symlink resolves to is always preserved — even if it's
# older than the cutoff — so we never delete the active install.
#
# Filtering is by target-triple suffix so a multi-arch dev with both
# (e.g.) aarch64-apple-darwin and x86_64-unknown-linux-gnu under the
# same $HOME_DIR keeps each target's history independently. Other-arch
# dirs are invisible to this prune pass.
#
# Args: $1 = releases dir, $2 = current symlink path, $3 = target triple,
#       $4 = keep count (non-negative integer; 0 = skip).
prune_old_releases() {
    local releases_dir="$1" current_link="$2" target="$3" keep="$4"

    if [[ "$keep" == "0" ]]; then
        log "version GC disabled (CUA_DRIVER_RS_KEEP_VERSIONS=0)"
        return 0
    fi
    if [[ ! -d "$releases_dir" ]]; then
        return 0
    fi

    # Resolve the dir `current` points at so we can exempt it from the
    # prune candidates. `readlink -f` would canonicalize, but BSD readlink
    # (macOS — which doesn't reach here in production but is tested from
    # a dev shell) lacks -f, so use the more portable two-step.
    local current_target=""
    if [[ -L "$current_link" ]]; then
        local link_value
        link_value=$(readlink "$current_link" 2>/dev/null || true)
        if [[ -n "$link_value" ]]; then
            case "$link_value" in
                /*) current_target="$link_value" ;;
                *)  current_target="$(dirname "$current_link")/$link_value" ;;
            esac
        fi
    fi

    # `ls -dt` sorts dirs by mtime, newest-first. The trailing slash on
    # the glob filters to dirs only. Skip if no matching dirs (the glob
    # would otherwise pass through literally under shopt -s nullglob being
    # off — guard with 2>/dev/null and an `|| true`).
    local candidates=()
    while IFS= read -r dir; do
        [[ -n "$dir" ]] || continue
        # Strip trailing slash for consistent comparison with $current_target.
        dir="${dir%/}"
        # Only consider dirs whose name ends in the current target triple.
        local base="${dir##*/}"
        case "$base" in
            *-"$target") candidates+=("$dir") ;;
            *) ;;
        esac
    done < <(ls -dt "$releases_dir"/*/ 2>/dev/null || true)

    if [[ ${#candidates[@]} -le "$keep" ]]; then
        return 0
    fi

    # Walk candidates newest-first. The first $keep are retained by mtime
    # (active install counts toward the budget in the common case where
    # the install just happened — it's the newest by mtime). The active
    # install is *additionally* preserved even if it would otherwise fall
    # outside the keep window (e.g. user rolled back to an old version),
    # so the worst-case post-GC count is $keep + 1, common case is exactly
    # $keep.
    local to_prune=()
    local kept=0
    local i
    for ((i=0; i<${#candidates[@]}; i++)); do
        local cand="${candidates[$i]}"
        local is_current=0
        if [[ -n "$current_target" && "${cand%/}" == "${current_target%/}" ]]; then
            is_current=1
        fi
        if (( kept < keep )); then
            kept=$((kept + 1))
            continue
        fi
        if (( is_current == 1 )); then
            # Active install fell outside the keep window — preserve
            # anyway (never delete the dir that's about to serve the
            # next `cua-driver` invocation).
            continue
        fi
        to_prune+=("$cand")
    done

    if [[ ${#to_prune[@]} -eq 0 ]]; then
        return 0
    fi

    log "pruning ${#to_prune[@]} old release dir(s) (keeping $keep most recent for $target):"
    local p
    for p in "${to_prune[@]}"; do
        log "  - ${p##*/}"
    done
    printf '%s\0' "${to_prune[@]}" | xargs -0 rm -rf
}

# --- Clean up a pre-existing LOCAL (install-local) install --------------
#
# `install-local.sh` (`_install-local-rust.sh`) installs a dev build into the
# SAME canonical home this release installer now writes to (~/.cua-driver, see
# the HOME_DIR reconciliation above), under a `*-local-*` versioned release dir
# (VERSION_TAG="0.0.0-local-<config>"). On macOS it also cert-signs the shared
# /Applications/CuaDriver.app with a self-signed identity recorded at
# `~/.cua-driver/.tcc-signing-identity`.
#
# A user who ran install-local and then runs this release installer would
# otherwise end up with the local artifacts lingering alongside the fresh
# release: the `*-local-*` release dir(s) sit in `packages/releases/` (the
# release `current` swap re-points away from them, but they're never removed
# explicitly here), and the stale `.tcc-signing-identity` marker survives even
# though the release bundle is CI-signed, not locally cert-signed. Follow the
# same logic install-local / uninstall.sh use: stop the daemon, then remove
# ONLY the unambiguously-local artifacts so the release install is the single
# authoritative one.
#
# Conservative by construction: we only ever remove `*-local-*` release dirs
# and the local signing-identity marker — never a real release dir, never the
# `current` symlink (the release branch owns that), never unrelated user state
# under the home. Every step is best-effort + idempotent; a machine with no
# prior local install is a clean no-op.
#
# The release install below verifies the previous and replacement designated
# requirements. Compatible releases preserve grants; a proven mismatch resets
# only the stale Cua Driver rows after the replacement is registered.
cleanup_prior_local_install() {
    local releases_dir="$HOME_DIR/packages/releases"
    local tcc_marker="$HOME_DIR/.tcc-signing-identity"

    # Collect the local-build release dirs (the unambiguous install-local
    # signature — a release install never creates a `*-local-*` dir).
    local local_dirs=()
    local d
    if [[ -d "$releases_dir" ]]; then
        for d in "$releases_dir"/*-local-*/; do
            [[ -d "$d" ]] && local_dirs+=("${d%/}")
        done
    fi

    # Nothing local on disk → clean no-op (no marker, no local dirs).
    if [[ ${#local_dirs[@]} -eq 0 && ! -f "$tcc_marker" ]]; then
        return 0
    fi

    log "detected a prior install-local build under $HOME_DIR — cleaning it up so this release install is authoritative"

    # The Darwin caller already performed authenticated, generation-bound
    # shutdown before the app swap. Other platforms retain the shared helper.
    if [[ "${OS:-}" != "Darwin" && "${DAEMONS_STOPPED_BEFORE_SWAP:-0}" != "1" ]]; then
        stop_cua_driver_daemons
    fi

    # Remove the `*-local-*` release dirs. The release install stages into its
    # own `<version>-<target>` dir and swaps `current` to it, so deleting the
    # local dirs can't strand the active install. If `current` somehow still
    # points into a local dir (e.g. a partial prior run), the release branch
    # below re-creates `current` immediately after, so a transient dangling
    # link is harmless.
    if [[ ${#local_dirs[@]} -gt 0 ]]; then
        for d in "${local_dirs[@]}"; do
            rm -rf "$d" 2>/dev/null || true
            log "  removed local build dir ${d##*/}"
        done
    fi

    # Remove the local signing-identity marker — it describes the locally
    # cert-signed bundle, which the release `ditto` is about to replace with
    # the CI-signed one. Leaving it would misreport the bundle's identity.
    if [[ -f "$tcc_marker" ]]; then
        rm -f "$tcc_marker" 2>/dev/null || true
        log "  removed local signing-identity marker $tcc_marker"
    fi
}

# --- Resolve OS/arch ----------------------------------------------------

OS=$(uname -s)
ARCH_RAW=$(uname -m)

# Rosetta translation correction.
#
# `uname -m` reflects the architecture of the *running process*, not the
# physical CPU. When an Apple Silicon Mac is driving an x86_64-translated
# shell (Rosetta — e.g. `arch -x86_64 bash`, or a Homebrew install pinned
# to /usr/local/), uname reports x86_64 even though the native arch is
# arm64. We'd then download the x86_64 binary and run it under Rosetta —
# slower, and an unnecessary translation when a native arm64 binary
# exists on the release page.
#
# `sysctl.proc_translated` returns 1 when the current process is running
# under Rosetta translation; absent/0 means native. Only meaningful on
# macOS — the sysctl key is missing on Linux, so the redirect-to-null
# keeps the check a silent no-op there.
if [[ "$OS" == "Darwin" && "$ARCH_RAW" == "x86_64" ]]; then
    if [[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" == "1" ]]; then
        log "detected Rosetta-translated shell on Apple Silicon — switching to darwin-arm64"
        ARCH_RAW="arm64"
    fi
fi

# LABEL  = the release-asset tarball label (matches what cd-rust-cua-driver.yml
#          publishes; user-facing).
# TARGET = the Rust target triple, used in the on-disk per-version dir name so
#          a multi-arch dev can keep e.g. aarch64-apple-darwin and
#          x86_64-unknown-linux-gnu side by side under $HOME_DIR/packages/
#          releases/ without collision.
case "$OS-$ARCH_RAW" in
    Darwin-arm64|Darwin-aarch64)     LABEL="darwin-arm64"  ; TARGET="aarch64-apple-darwin"      ;;
    Darwin-x86_64)                   LABEL="darwin-x86_64" ; TARGET="x86_64-apple-darwin"       ;;
    Linux-x86_64|Linux-amd64)        LABEL="linux-x86_64"  ; TARGET="x86_64-unknown-linux-gnu"  ;;
    Linux-aarch64|Linux-arm64)       LABEL="linux-arm64"   ; TARGET="aarch64-unknown-linux-gnu" ;;
    *)
        err "unsupported platform: $OS / $ARCH_RAW"
        err "  cua-driver-rs ships prebuilts for: darwin-arm64, darwin-x86_64, linux-x86_64, linux-arm64."
        err "  Windows users: install via install.ps1 (irm https://cua.ai/driver/install.ps1 | iex)."
        exit 1
        ;;
esac

for cmd in curl tar; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        err "$cmd not found on PATH"
        exit 1
    fi
done

extract_release_tarball_safely() {
    local archive="$1" destination="$2"
    local names="$TMP_DIR/archive-members.names"
    local metadata="$TMP_DIR/archive-members.metadata"
    local normalized="$TMP_DIR/archive-members.normalized"
    local name member_count metadata_count tar_size_field type size
    # Optional bounds are an internal test seam; the production call below
    # supplies no override and always uses these fixed limits.
    local total_size=0 max_member_size="${3:-$((512 * 1024 * 1024))}"
    local max_total_size="${4:-$((1024 * 1024 * 1024))}"

    : > "$normalized"
    case "$(LC_ALL=C tar --version 2>&1 | head -n 1)" in
        *bsdtar*) tar_size_field=5 ;;
        *GNU\ tar*|*BusyBox*|*busybox*) tar_size_field=3 ;;
        *) err "unsupported tar implementation for safe release extraction"; return 1 ;;
    esac
    if ! LC_ALL=C tar -tzf "$archive" > "$names" \
       || ! LC_ALL=C tar -tvzf "$archive" \
            | awk -v size_field="$tar_size_field" '
                $size_field !~ /^[0-9]+$/ { exit 42 }
                { print substr($0, 1, 1) "\t" $size_field }
            ' > "$metadata"; then
        err "release archive directory could not be read"
        return 1
    fi
    member_count="$(wc -l < "$names" | tr -d '[:space:]')"
    metadata_count="$(wc -l < "$metadata" | tr -d '[:space:]')"
    if ! [[ "$member_count" =~ ^[0-9]+$ ]] \
       || (( member_count == 0 || member_count > 4096 )) \
       || [[ "$metadata_count" != "$member_count" ]]; then
        err "release archive member inventory is invalid"
        return 1
    fi
    while IFS=$'\t' read -r type size; do
        if [[ "$type" != "-" && "$type" != "d" ]]; then
            err "release archive contains a link or special member"
            return 1
        fi
        # Validate a small canonical decimal before Bash arithmetic. Values
        # wider than ten digits cannot be valid under either fixed bound and
        # would otherwise wrap signed shell integers on crafted metadata.
        if ! [[ "$size" =~ ^(0|[1-9][0-9]{0,9})$ ]] \
           || (( size > max_member_size )); then
            err "release archive contains an oversized member"
            return 1
        fi
        if [[ "$type" == "-" ]]; then
            total_size=$((total_size + size))
            if (( total_size > max_total_size )); then
                err "release archive expands beyond the allowed size"
                return 1
            fi
        fi
    done < "$metadata"
    while IFS= read -r name || [[ -n "$name" ]]; do
        while [[ "$name" == ./* ]]; do name="${name#./}"; done
        name="${name%/}"
        if [[ -z "$name" || "$name" == /* || "$name" == *\\* \
           || "$name" == *$'\t'* || "$name" == *$'\r'* \
           || "$name" == *//* ]]; then
            err "release archive contains an invalid member name"
            return 1
        fi
        case "/$name/" in
            */../*|*/./*)
                err "release archive contains an unsafe member path"
                return 1
                ;;
        esac
        printf '%s\n' "$name" >> "$normalized"
    done < "$names"
    if [[ "$(LC_ALL=C sort "$normalized" | uniq -d | wc -l | tr -d '[:space:]')" != "0" ]]; then
        err "release archive contains duplicate normalized member paths"
        return 1
    fi
    # Both BSD tar (macOS) and GNU tar reject absolute/.. extraction paths.
    # The explicit inventory checks above additionally reject links, hardlinks,
    # devices, FIFOs, duplicate paths, and backslash aliases before extraction.
    LC_ALL=C tar -xzf "$archive" -C "$destination"
}

# --- Resolve release tag ------------------------------------------------
#
# Version is resolved in priority order:
#   1. CUA_DRIVER_RS_VERSION env var (explicit pin)
#   2. CUA_DRIVER_RS_BAKED_VERSION below (updated after release publication)
#   3. GitHub Releases API (fallback for dev / un-baked checkouts;
#      unauthenticated = 60 req/hr per IP)
#
# The baked value is the common-case default: `curl ... | bash` against
# `main` resolves the version locally with zero API calls, so an API
# outage / rate limit / network blip can't break a default install. The
# API is consulted only when the baked line is absent (dev / pre-release
# checkouts) or when the baked version turns out to have no downloadable
# asset — see the recovery at the download step below.
#
# ~~~ BAKED_VERSION: auto-updated after release publication — do not edit ~~~
CUA_DRIVER_RS_BAKED_VERSION="0.23.2" # published-installer-version
# ~~~ END_BAKED_VERSION ~~~

# Run API requests with an optional token. Keep the header construction here
# (rather than in loggable command text) so neither GH_TOKEN nor GITHUB_TOKEN
# can appear in installer output. Release-asset downloads stay unauthenticated:
# the repository is public and curl may redirect them to another GitHub host.
# GH_TOKEN takes precedence, matching the GitHub CLI.
github_api_curl() {
    local token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
    if [[ -n "$token" ]]; then
        curl -H "Authorization: Bearer $token" "$@"
    else
        curl "$@"
    fi
}

# The authenticated releases endpoint can include drafts for maintainers.
# Associate each top-level tag_name with its following draft field before
# considering it. GitHub's REST response renders those top-level fields on
# separate lines in that order; nested author/assets objects have no tag_name.
extract_published_release_versions() {
    # Keep this helper usable by stable-only callers and extracted test
    # harnesses that predate persistent channel selection.
    local selected_channel="${SELECTED_CHANNEL:-stable}"
    local selected_tag_prefix="${SELECTED_TAG_PREFIX:-$TAG_PREFIX}"
    awk -v prefix="$selected_tag_prefix" -v channel="$selected_channel" '
        /"tag_name"[[:space:]]*:/ {
            tag = $0
            sub(/^.*"tag_name"[[:space:]]*:[[:space:]]*"/, "", tag)
            sub(/".*$/, "", tag)
            next
        }
        tag != "" && /"draft"[[:space:]]*:/ {
            if ($0 ~ /"draft"[[:space:]]*:[[:space:]]*false/) {
                version = tag
                if (index(version, prefix) == 1) {
                    version = substr(version, length(prefix) + 1)
                    if ((channel == "stable" && version ~ /^[0-9]+\.[0-9]+\.[0-9]+$/) ||
                        (channel == "nightly" && version ~ /^[0-9]+\.[0-9]+\.[0-9]+-nightly\.[0-9]{8}\.[1-9][0-9]*$/)) {
                        print version
                    }
                }
            }
            tag = ""
        }
    '
}

# Highest SemVer ${TAG_PREFIX}* version published on the repo, printed bare
# (no tag prefix) on stdout. Returns non-zero when the API is unreachable or
# has no matching tag; callers decide whether that is fatal, because this runs
# both as the primary resolver and as recovery for a bad baked version.
#
# per_page=100 (the API maximum): the repo interleaves lume, Python, and Swift
# releases with these. Walk up to ten pages so a busy repository cannot hide
# cua-driver-rs behind the first page, but keep the request count bounded.
resolve_latest_version_from_api() {
    local page page_json page_count page_versions
    local versions=""
    for ((page=1; page<=10; page++)); do
        page_json="$(github_api_curl -fsSL \
            "https://api.github.com/repos/$REPO/releases?per_page=100&page=$page")" || return 1

        # Extract only published tags matching the selected channel's strict
        # grammar. Cua Driver's stable tags are marked prerelease in GitHub
        # metadata, so tag syntax—not the prerelease flag—defines the channel.
        page_versions="$(printf '%s' "$page_json" | extract_published_release_versions)" || true
        if [[ -n "$page_versions" ]]; then
            versions="${versions}${versions:+$'\n'}${page_versions}"
        fi

        page_count="$(
            printf '%s' "$page_json" \
                | awk '{ count += gsub(/"tag_name"[[:space:]]*:/, "&") } END { print count + 0 }'
        )"
        [[ "$page_count" =~ ^[0-9]+$ ]] || return 1
        if (( page_count < 100 )); then
            break
        fi
    done

    local version
    version="$(
        printf '%s\n' "$versions" \
            | sed '/^$/d' \
            | sort -t. -k1,1nr -k2,2nr -k3,3nr -k4,4nr -k5,5nr \
            | head -n 1
    )"
    [[ -n "$version" ]] || return 1
    printf '%s' "$version"
}

# Where VERSION came from. A missing asset is fatal for an explicit pin (the
# user named that version) but recoverable for the baked constant. The normal
# CD path advances it only after every staged release asset is public; fallback
# remains defense in depth for manual edits, asset removal, or an interrupted
# legacy release flow.
resolve_explicit_release_tag() {
    local value="$1" stable_version nightly_version
    if [[ "$value" =~ ^(cua-driver-rs-v|v)?([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        stable_version="${BASH_REMATCH[2]}"
        printf '%s%s' "$TAG_PREFIX" "$stable_version"
        return 0
    fi
    if [[ "$value" =~ ^nightly-cua-driver-rs-v([0-9]+\.[0-9]+\.[0-9]+-nightly\.[0-9]{8}\.[1-9][0-9]*)$ ]]; then
        nightly_version="${BASH_REMATCH[1]}"
        printf '%s%s' "$NIGHTLY_TAG_PREFIX" "$nightly_version"
        return 0
    fi
    if [[ "$value" =~ ^([0-9]+\.[0-9]+\.[0-9]+-nightly\.[0-9]{8}\.[1-9][0-9]*)$ ]]; then
        nightly_version="${BASH_REMATCH[1]}"
        printf '%s%s' "$NIGHTLY_TAG_PREFIX" "$nightly_version"
        return 0
    fi
    return 1
}

CHANNEL_STATE_FILE="$HOME_DIR/release-channel"
if [[ "$CHANNEL_EXPLICIT" == "1" && -n "${CUA_DRIVER_RS_VERSION:-}" ]]; then
    err "--channel cannot be combined with CUA_DRIVER_RS_VERSION; exact pins do not change saved channel state"
    exit 2
fi
if [[ "$CHANNEL_EXPLICIT" == "1" ]]; then
    SELECTED_CHANNEL="$CHANNEL_ARG"
elif [[ -n "${CUA_DRIVER_RS_VERSION:-}" ]]; then
    # Exact pins are one-shot and outrank persisted preference. In particular,
    # a damaged preference file must not make a deliberate recovery pin unusable.
    SELECTED_CHANNEL="stable"
elif [[ -f "$CHANNEL_STATE_FILE" ]]; then
    SELECTED_CHANNEL="$(tr -d '[:space:]' < "$CHANNEL_STATE_FILE")"
else
    SELECTED_CHANNEL="stable"
fi
case "$SELECTED_CHANNEL" in
    stable) SELECTED_TAG_PREFIX="$TAG_PREFIX" ;;
    nightly) SELECTED_TAG_PREFIX="$NIGHTLY_TAG_PREFIX" ;;
    *)
        err "invalid release channel '$SELECTED_CHANNEL' in $CHANNEL_STATE_FILE; expected stable or nightly"
        err "  repair with: cua-driver channel set stable"
        exit 1
        ;;
esac

if [[ -n "${CUA_DRIVER_RS_VERSION:-}" ]]; then
    VERSION_SOURCE="pin"
    if ! TAG="$(resolve_explicit_release_tag "$CUA_DRIVER_RS_VERSION")"; then
        err "CUA_DRIVER_RS_VERSION must be an exact x.y.z stable version or canonical nightly tag"
        exit 1
    fi
    log "using version from CUA_DRIVER_RS_VERSION: $TAG"
elif [[ "$SELECTED_CHANNEL" == "stable" && -n "${CUA_DRIVER_RS_BAKED_VERSION:-}" ]]; then
    VERSION_SOURCE="baked"
    if ! [[ "$CUA_DRIVER_RS_BAKED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        err "baked Cua Driver version must be an exact stable x.y.z version"
        exit 1
    fi
    TAG="${TAG_PREFIX}${CUA_DRIVER_RS_BAKED_VERSION#v}"
    log "using baked release: $TAG"
else
    VERSION_SOURCE="api"
    log "resolving latest $SELECTED_CHANNEL release via GitHub API"
    if ! API_VERSION="$(resolve_latest_version_from_api)"; then
        err "no release matching ${SELECTED_TAG_PREFIX}* found on $REPO"
        err "  (cua-driver-rs is a BETA-stage cross-platform port; releases may not be published yet.)"
        exit 1
    fi
    TAG="${SELECTED_TAG_PREFIX}${API_VERSION}"
    log "latest release: $TAG"
fi

if [[ "$TAG" == "$NIGHTLY_TAG_PREFIX"* ]]; then
    VERSION="${TAG#${NIGHTLY_TAG_PREFIX}}"
else
    VERSION="${TAG#${TAG_PREFIX}}"
fi

# Releases through 0.12.6 predate semantic cursor themes.
# Newer releases must contain both packaged copies.
CURSOR_THEME_REQUIRED_FROM="0.12.7"
version_is_at_least() {
    local version="$1" minimum="$2"
    local v_major v_minor v_patch m_major m_minor m_patch
    version="${version%%-*}"
    IFS=. read -r v_major v_minor v_patch <<< "$version"
    IFS=. read -r m_major m_minor m_patch <<< "$minimum"
    if (( v_major != m_major )); then (( v_major > m_major )); return; fi
    if (( v_minor != m_minor )); then (( v_minor > m_minor )); return; fi
    (( v_patch >= m_patch ))
}

# --- Download bare-binary tarball ---------------------------------------

# Tarball selection:
#
# macOS — fetch the directory tarball (cua-driver-rs-vN-darwin-universal.tar.gz).
#   The directory layout includes `CuaDriver.app/` alongside the bare
#   binary, which we need to install into /Applications so the TCC
#   auto-relaunch path in `cua-driver-rs mcp` can resolve
#   `com.meta.musecode.cua.driver` via `open -n -g -a CuaDriver`. The
#   directory variant carries the same universal binary as the
#   bare-binary tarball, so users on both Apple Silicon and Intel
#   get a working install from one download.
#
# Linux / Windows-via-WSL — use the bare-binary tarball. No bundle on
#   these platforms, no TCC, no need to unpack a directory.
release_tarball_name() {
    case "$LABEL" in
        darwin-*) printf 'cua-driver-rs-%s-darwin-universal.tar.gz' "$1" ;;
        *)        printf 'cua-driver-rs-%s-%s-binary.tar.gz' "$1" "$LABEL" ;;
    esac
}

# Fetches one release tarball into $TMP_DIR. A confirmed HTTP 404 returns 44;
# every other failure is retried at the same URL with bounded backoff, then
# returns 1. This distinction is load-bearing: only a missing baked asset may
# trigger release fallback. A timeout, TLS failure, rate limit, or server error
# must never silently install an older version.
download_release_tarball() {
    local version="$1" tarball url partial http_code curl_status attempt retryable
    tarball="$(release_tarball_name "$version")"
    url="https://github.com/$REPO/releases/download/${TAG}/$tarball"
    partial="$TMP_DIR/$tarball.partial"
    log "downloading $url"
    for attempt in 1 2 3; do
        http_code=""
        curl_status=0
        http_code="$(
            curl -sSL -o "$partial" -w '%{http_code}' "$url"
        )" || curl_status=$?
        if (( curl_status == 0 )) && [[ "$http_code" =~ ^2[0-9][0-9]$ ]]; then
            mv "$partial" "$TMP_DIR/$tarball"
            return 0
        fi
        rm -f "$partial" 2>/dev/null || true
        if [[ "$http_code" == "404" ]]; then
            return 44
        fi
        retryable=0
        if (( curl_status != 0 )) \
            || [[ "$http_code" == "408" || "$http_code" == "429" ]] \
            || [[ "$http_code" =~ ^5[0-9][0-9]$ ]]; then
            retryable=1
        fi
        if (( retryable == 1 && attempt < 3 )); then
            err "download attempt $attempt failed (HTTP ${http_code:-unknown}, curl exit $curl_status); retrying the same release"
            sleep "$attempt"
            continue
        fi
        break
    done
    err "download failed after $attempt attempt(s) (HTTP ${http_code:-unknown}, curl exit $curl_status); refusing to fall back to an older release"
    return 1
}

DOWNLOAD_STATUS=0
download_release_tarball "$VERSION" || DOWNLOAD_STATUS=$?
if (( DOWNLOAD_STATUS != 0 )); then
    # Defense in depth for a manually advanced constant, removed asset, or
    # interrupted legacy release flow. The normal CD path updates this constant
    # only after every staged asset is publicly visible.
    if [[ "$VERSION_SOURCE" != "baked" || "$DOWNLOAD_STATUS" != "44" ]]; then
        err "download failed; try CUA_DRIVER_RS_VERSION=<version> to pin a specific release"
        exit 1
    fi
    printf 'warning: baked release %s has no downloadable %s asset (HTTP 404); this is usually a temporary publish lag\n' \
        "$TAG" "$LABEL" >&2
    printf 'warning: temporarily falling back to the newest fully published release via the GitHub Releases API\n' >&2
    if ! API_VERSION="$(resolve_latest_version_from_api)"; then
        err "could not resolve any published ${TAG_PREFIX}* release to fall back to"
        err "  try CUA_DRIVER_RS_VERSION=<version> to pin a specific release"
        exit 1
    fi
    if [[ "$API_VERSION" == "$VERSION" ]]; then
        # The API agrees this is the newest tag, so the tag exists but its
        # assets do not. Retrying the identical URL would just 404 again.
        err "${TAG_PREFIX}${API_VERSION} is the newest published release but is missing its ${LABEL} asset"
        err "  try CUA_DRIVER_RS_VERSION=<version> to pin an older release"
        exit 1
    fi
    printf 'warning: falling back to %s%s\n' "$TAG_PREFIX" "$API_VERSION" >&2
    # Adopt the recovered release before anything downstream derives a path,
    # a stage directory, or a capability check from VERSION.
    VERSION="$API_VERSION"
    TAG="${TAG_PREFIX}${VERSION}"
    DOWNLOAD_STATUS=0
    download_release_tarball "$VERSION" || DOWNLOAD_STATUS=$?
    if (( DOWNLOAD_STATUS != 0 )); then
        err "download failed; try CUA_DRIVER_RS_VERSION=<version> to pin a specific release"
        exit 1
    fi
fi
TARBALL="$(release_tarball_name "$VERSION")"

log "extracting"
if ! extract_release_tarball_safely "$TMP_DIR/$TARBALL" "$TMP_DIR"; then
    exit 1
fi

# Layout detection:
#   macOS dir tarball expands to:
#     cua-driver-rs-${VERSION}-darwin-universal/
#       ├── cua-driver           (bare universal binary)
#       ├── CuaDriver.app/     (minimal bundle; copy of the same binary
#       │                         lives at Contents/MacOS/cua-driver)
#       └── LICENSE
#   Linux bare-runtime tarball expands to:
#     cua-driver and libcua_driver_sdk.so at the archive root. The installer
#     consumes the CLI; SDK packaging consumes the colocated library.
case "$LABEL" in
    darwin-*)
        STAGE="cua-driver-rs-${VERSION}-darwin-universal"
        SRC="$TMP_DIR/$STAGE/$BINARY_NAME"
        SRC_THEME="$TMP_DIR/$STAGE/cua-cursor-theme"
        SRC_APP="$TMP_DIR/$STAGE/$APP_NAME"
        ;;
    *)
        SRC="$TMP_DIR/$BINARY_NAME"
        SRC_THEME="$TMP_DIR/cua-cursor-theme"
        SRC_WAYLAND_HELPER="$TMP_DIR/wayland-helper"
        SRC_APP=""
        ;;
esac
if [[ ! -f "$SRC" ]]; then
    err "expected $BINARY_NAME in tarball but didn't find it"
    ls -la "$TMP_DIR"
    exit 1
fi
THEME_AVAILABLE=1
if [[ ! -f "$SRC_THEME" ]] || {
    [[ -n "$SRC_APP" ]] &&
    [[ ! -f "$SRC_APP/Contents/MacOS/cua-cursor-theme" ]]
}; then
    THEME_AVAILABLE=0
fi
if [[ "$THEME_AVAILABLE" == "0" ]] && version_is_at_least \
    "$VERSION" "$CURSOR_THEME_REQUIRED_FROM"; then
    err "expected cua-cursor-theme in tarball but didn't find it"
    ls -la "$TMP_DIR"
    exit 1
fi
if [[ "$THEME_AVAILABLE" == "0" ]]; then
    printf 'warning: release %s predates cua-cursor-theme; installing without custom cursor themes\n' \
        "$VERSION" >&2
fi

if [[ "$CHANNEL_EXPLICIT" == "1" ]]; then
    mkdir -p "$HOME_DIR"
    CHANNEL_TMP="$HOME_DIR/.release-channel.$$"
    printf '%s\n' "$SELECTED_CHANNEL" > "$CHANNEL_TMP"
    mv -f "$CHANNEL_TMP" "$CHANNEL_STATE_FILE"
    log "saved release channel: $SELECTED_CHANNEL"
fi

# --- Install ------------------------------------------------------------

mkdir -p "$BIN_DIR"

# macOS: install the .app to /Applications first, then symlink the
# bin into the bundle so `~/.local/bin/cua-driver` resolves into
# `/Applications/CuaDriver.app/Contents/MacOS/cua-driver`. The
# `realpath` walk in `is_executable_inside_cuadriver_app()` keys on
# that resolved path to know whether the auto-relaunch heuristic
# should fire. It retains the Swift `cua-driver` install path
# (`/Applications/CuaDriver.app`) but adopts a new Muse Code-owned bundle ID.
# Signing-requirement compatibility is evaluated only when replacing another
# build of `com.meta.musecode.cua.driver`; legacy `com.trycua.driver` grants do
# not transfer across the identity migration.
#
# The macOS path intentionally does NOT use the
# $HOME_DIR/packages/releases/<v>/ + current symlink layout used on
# Linux. Reason: /Applications/CuaDriver.app placement is the
# anchor for both TCC attribution (cdhash + bundle id) and
# LaunchServices' `open -a CuaDriver` discovery — symlinking the
# .app from /Applications to a versioned dir under $HOME_DIR breaks
# both. The asymmetry is deliberate; rollback on macOS = reinstall
# an older release tag.
#
# Linux: drop the binary into the per-version dir under
# $HOME_DIR/packages/releases/<version>-<target>/ and swap the
# `current` symlink atomically. The visible $BIN_DIR/cua-driver
# symlinks into `current` so PATH consumers (and MCP client configs)
# never need to change when the active version moves.
#
# Fail fast on Darwin if the .app is missing — falling through to the
# bare-binary install would silently produce a CLI that can never
# auto-relaunch into a TCC-correct daemon. CodeRabbit #3.
if [[ "$OS" == "Darwin" ]]; then
    if [[ -z "${SRC_APP:-}" || ! -d "$SRC_APP" ]]; then
        err "macOS install requires the .app bundle (SRC_APP not found at ${SRC_APP:-<unset>})"
        err "  This usually means the downloaded tarball is missing CuaDriver.app — re-run the installer or"
        err "  pin a known-good release via CUA_DRIVER_RS_VERSION=<version>."
        exit 1
    fi
fi
DAEMONS_STOPPED_BEFORE_SWAP=0
if [[ "$OS" == "Darwin" && -n "$SRC_APP" && -d "$SRC_APP" ]]; then
    if [[ ! -w "/Applications" ]]; then
        err "/Applications is not writable. Re-run this installer in a shell where it is, or grant write access."
        err "  Without the .app bundle, \`cua-driver-rs mcp\` from an IDE terminal will not auto-relaunch into a TCC-correct daemon."
        exit 1
    fi
    if ! command -v "$MACOS_CODESIGN" >/dev/null 2>&1; then
        err "codesign is required to verify the macOS release app safely"
        exit 1
    fi
    if ! validate_apple_team_id "$PRODUCTION_TEAM_ID"; then
        err "CUA_DRIVER_PRODUCTION_TEAM_ID must be the approved 10-character Apple Team ID"
        exit 1
    fi
    if [[ "$PRODUCTION_TEAM_ID" != "$PINNED_PRODUCTION_TEAM_ID" ]]; then
        err "CUA_DRIVER_PRODUCTION_TEAM_ID does not match the pinned Muse Code production Team ID"
        exit 1
    fi
    if ! validate_apple_team_id "$LEGACY_PRODUCTION_TEAM_ID"; then
        err "CUA_DRIVER_LEGACY_TEAM_ID must be a 10-character Apple Team ID"
        exit 1
    fi
    if ! macos_verify_release_app "$SRC_APP" "$PRODUCTION_BUNDLE_ID" \
        "$BINARY_NAME" "$PRODUCTION_TEAM_ID"; then
        err "downloaded CuaDriver.app failed bundle, executable, signer, Apple anchor, or notarization verification"
        err "the installed app was not changed"
        exit 1
    fi
    if ! macos_verify_build_attestation "$SRC_APP" "$PRODUCTION_BUNDLE_ID" \
        "$PRODUCTION_TEAM_ID" "$VERSION" "${CUA_DRIVER_EXPECTED_SOURCE_SHA:-}"; then
        err "downloaded CuaDriver.app build attestation does not match the configured bundle, Team ID, version, or source pins"
        err "the installed app was not changed"
        exit 1
    fi
    STAGED_BUNDLE_ID="$PRODUCTION_BUNDLE_ID"
    STAGED_REQUIREMENT="$(macos_designated_requirement "$SRC_APP" || true)"
    if [[ -z "$STAGED_REQUIREMENT" ]]; then
        err "could not read the downloaded app's designated requirement; the installed app was not changed"
        exit 1
    fi

    REPLACED_CANONICAL=0
    MIGRATED_LEGACY_ID=0
    MIGRATED_LEGACY_RS_ID=0
    PREVIOUS_REQUIREMENT=""
    REQUIREMENT_COMPATIBILITY="unknown"
    if [[ -e "$APP_DEST" || -L "$APP_DEST" ]]; then
        if [[ -L "$APP_DEST" || ! -d "$APP_DEST" ]]; then
            err "refusing to replace unsafe shared app path $APP_DEST"
            exit 1
        fi
        PREV_BUNDLE_ID="$(macos_bundle_value "$APP_DEST" CFBundleIdentifier || true)"
        PREV_BUNDLE_VERSION="$(macos_bundle_value "$APP_DEST" CFBundleShortVersionString || true)"
        if [[ "$PREV_BUNDLE_ID" == "$PRODUCTION_BUNDLE_ID" ]]; then
            if ! macos_verify_release_app "$APP_DEST" "$PRODUCTION_BUNDLE_ID" \
                "$BINARY_NAME" "$PRODUCTION_TEAM_ID"; then
                err "refusing to replace $APP_DEST because its production ownership could not be authenticated"
                exit 1
            fi
            log "replacing existing cua-driver at $APP_DEST (${PREV_BUNDLE_ID}, version ${PREV_BUNDLE_VERSION})"
            REPLACED_CANONICAL=1
            MACOS_APP_BACKUP_BUNDLE_ID="$PRODUCTION_BUNDLE_ID"
            MACOS_APP_BACKUP_TEAM_ID="$PRODUCTION_TEAM_ID"
        elif [[ "$PREV_BUNDLE_ID" == "$LEGACY_PRODUCTION_BUNDLE_ID" ]]; then
            if ! macos_verify_release_app "$APP_DEST" "$LEGACY_PRODUCTION_BUNDLE_ID" \
                "$BINARY_NAME" "$LEGACY_PRODUCTION_TEAM_ID"; then
                err "refusing to replace $APP_DEST because its legacy ownership could not be authenticated"
                exit 1
            fi
            log "replacing legacy CuaDriver.app identity ${PREV_BUNDLE_ID}; fresh macOS permissions will be required"
            MIGRATED_LEGACY_ID=1
            MACOS_APP_BACKUP_BUNDLE_ID="$LEGACY_PRODUCTION_BUNDLE_ID"
            MACOS_APP_BACKUP_TEAM_ID="$LEGACY_PRODUCTION_TEAM_ID"
        else
            err "refusing to replace $APP_DEST because bundle id ${PREV_BUNDLE_ID:-<unreadable>} is not owned by this installer"
            exit 1
        fi
        if [[ "$PREV_BUNDLE_ID" == "$PRODUCTION_BUNDLE_ID" ]]; then
            PREVIOUS_REQUIREMENT="$(macos_designated_requirement "$APP_DEST" || true)"
            if [[ -n "$PREVIOUS_REQUIREMENT" ]]; then
                REQUIREMENT_COMPATIBILITY="$(macos_requirement_compatibility \
                    "$PREVIOUS_REQUIREMENT" "$SRC_APP")"
                if [[ "$REQUIREMENT_COMPATIBILITY" == "unknown" ]]; then
                    err "could not evaluate the existing app's signing requirement; refusing a TCC-unsafe replacement"
                    exit 1
                fi
            else
                err "could not read the existing app's designated requirement; refusing a TCC-unsafe replacement"
                exit 1
            fi
        fi
    fi

    if [[ -e "$LEGACY_RS_APP_DEST" || -L "$LEGACY_RS_APP_DEST" ]]; then
        if [[ -L "$LEGACY_RS_APP_DEST" || ! -d "$LEGACY_RS_APP_DEST" ]]; then
            err "refusing to migrate unsafe legacy app path $LEGACY_RS_APP_DEST"
            exit 1
        fi
        if ! macos_verify_release_app "$LEGACY_RS_APP_DEST" "$LEGACY_RS_BUNDLE_ID" \
            "$BINARY_NAME" "$LEGACY_PRODUCTION_TEAM_ID"; then
            err "refusing to migrate $LEGACY_RS_APP_DEST because its ownership could not be authenticated"
            exit 1
        fi
        MIGRATED_LEGACY_RS_ID=1
        log "found authenticated legacy CuaDriverRs.app; it will be removed after safe migration"
    fi

    # Stop the old daemon while its verified bundle still exists. This avoids
    # leaving a running process whose executable path disappears mid-upgrade.
    MACOS_OWNED_DAEMON_PATHS=("$HOME_DIR/packages/current/$BINARY_NAME")
    for _daemon_path in "$HOME_DIR"/packages/releases/*/"$BINARY_NAME"; do
        [[ -e "$_daemon_path" || -L "$_daemon_path" ]] || continue
        MACOS_OWNED_DAEMON_PATHS+=("$_daemon_path")
    done
    if [[ "$REPLACED_CANONICAL" == "1" || "$MIGRATED_LEGACY_ID" == "1" ]]; then
        MACOS_OWNED_DAEMON_PATHS+=("$APP_DEST/Contents/MacOS/$BINARY_NAME")
    fi
    if [[ "$MIGRATED_LEGACY_RS_ID" == "1" ]]; then
        MACOS_OWNED_DAEMON_PATHS+=("$LEGACY_RS_APP_DEST/Contents/MacOS/$BINARY_NAME")
    fi
    for _daemon_path in "${MACOS_OWNED_DAEMON_PATHS[@]}"; do
        _resolved_daemon_path="$(realpath "$_daemon_path" 2>/dev/null || true)"
        [[ -n "$_resolved_daemon_path" ]] \
            && MACOS_OWNED_DAEMON_PATHS+=("$_resolved_daemon_path")
    done
    if ! stop_authenticated_macos_daemons "${MACOS_OWNED_DAEMON_PATHS[@]}"; then
        err "refusing to migrate or replace app identities while an old daemon is still running"
        exit 1
    fi
    unset _daemon_path _resolved_daemon_path
    DAEMONS_STOPPED_BEFORE_SWAP=1

    # A legacy signer cannot decrypt Computer History after the identity
    # transition. Check only after verified daemon shutdown, then repeat the
    # same check immediately before permission cleanup and bundle moves.
    if [[ "$MIGRATED_LEGACY_ID" == "1" ]] \
       && ! macos_refuse_legacy_history_identity_transition \
            "$LEGACY_PRODUCTION_BUNDLE_ID" "$APP_DEST"; then
        exit 1
    fi
    if [[ "$MIGRATED_LEGACY_RS_ID" == "1" ]] \
       && ! macos_refuse_legacy_history_identity_transition \
            "$LEGACY_RS_BUNDLE_ID" "$LEGACY_RS_APP_DEST"; then
        exit 1
    fi

    # Repeat daemon and history checks under the stopped state immediately
    # before permission cleanup and bundle moves.
    records="$(macos_install_owned_process_records \
        "${MACOS_OWNED_DAEMON_PATHS[@]}")" || {
        err "could not re-verify stopped Cua Driver processes during migration preparation"
        exit 1
    }
    if [[ -n "$records" ]]; then
        err "a Cua Driver daemon restarted during migration preparation"
        exit 1
    fi
    unset records
    if [[ "$MIGRATED_LEGACY_ID" == "1" ]] \
       && ! macos_refuse_legacy_history_identity_transition \
            "$LEGACY_PRODUCTION_BUNDLE_ID" "$APP_DEST"; then
        exit 1
    fi
    if [[ "$MIGRATED_LEGACY_RS_ID" == "1" ]] \
       && ! macos_refuse_legacy_history_identity_transition \
            "$LEGACY_RS_BUNDLE_ID" "$LEGACY_RS_APP_DEST"; then
        exit 1
    fi

    MACOS_APP_BACKUP="${APP_DEST}.install-backup.$$"
    if [[ -e "$MACOS_APP_BACKUP" || -L "$MACOS_APP_BACKUP" ]]; then
        err "temporary backup path already exists: $MACOS_APP_BACKUP"
        exit 1
    fi
    if [[ "$MIGRATED_LEGACY_RS_ID" == "1" ]]; then
        MACOS_LEGACY_RS_BACKUP="${LEGACY_RS_APP_DEST}.install-backup.$$"
        if [[ -e "$MACOS_LEGACY_RS_BACKUP" || -L "$MACOS_LEGACY_RS_BACKUP" ]]; then
            err "temporary legacy backup path already exists: $MACOS_LEGACY_RS_BACKUP"
            exit 1
        fi
        # TCC cleanup unregisters the authenticated source. Start rollback
        # coverage before that preparation so an interrupt can either
        # re-register the unmoved source or restore the staged backup.
        MACOS_LEGACY_RS_REMOVAL_STARTED=1
        if ! macos_reset_legacy_tcc_before_migration \
            "$LEGACY_RS_APP_DEST" "$LEGACY_RS_BUNDLE_ID"; then
            exit 1
        fi
        if ! macos_stage_legacy_rs_removal; then
            exit 1
        fi
    fi
    if [[ "$MIGRATED_LEGACY_ID" == "1" ]]; then
        # Cover the unregistering preparation, not only the later bundle
        # move. Before a backup exists, rollback authenticates and
        # re-registers the preserved source app in place.
        MACOS_APP_HAD_PREVIOUS=1
        MACOS_APP_SWAP_STARTED=1
        if ! macos_reset_legacy_tcc_before_migration \
            "$APP_DEST" "$LEGACY_PRODUCTION_BUNDLE_ID"; then
            exit 1
        fi
    fi
    if [[ -e "$APP_DEST" ]]; then
        MACOS_APP_HAD_PREVIOUS=1
    fi
    MACOS_APP_SWAP_STARTED=1
    if [[ "$MACOS_APP_HAD_PREVIOUS" == "1" ]]; then
        if ! mv "$APP_DEST" "$MACOS_APP_BACKUP"; then
            err "could not move the authenticated previous app into its rollback slot"
            exit 1
        fi
    fi
    log "installing $APP_DEST"
    # `ditto` preserves the bundle's metadata + nested symlinks the way
    # Apple's installer would. `cp -R` works but doesn't preserve as
    # much, and ditto is always present on macOS.
    INSTALL_VALID=0
    if ditto "$SRC_APP" "$APP_DEST" \
       && macos_verify_release_app "$APP_DEST" "$PRODUCTION_BUNDLE_ID" \
            "$BINARY_NAME" "$PRODUCTION_TEAM_ID"; then
        INSTALLED_BUNDLE_ID="$(macos_bundle_value "$APP_DEST" CFBundleIdentifier || true)"
        INSTALLED_REQUIREMENT="$(macos_designated_requirement "$APP_DEST" || true)"
        if [[ "$INSTALLED_BUNDLE_ID" == "$STAGED_BUNDLE_ID" \
           && -n "$INSTALLED_REQUIREMENT" \
           && "$INSTALLED_REQUIREMENT" == "$STAGED_REQUIREMENT" ]]; then
            INSTALL_VALID=1
        fi
    fi
    if [[ "$INSTALL_VALID" != "1" ]]; then
        err "installed CuaDriver.app did not preserve its verified signing identity; the replacement was rolled back"
        exit 1
    fi
    APP_BINARY="$APP_DEST/Contents/MacOS/$BINARY_NAME"
    if [[ ! -x "$APP_BINARY" ]]; then
        err "binary missing at $APP_BINARY; the replacement was rolled back"
        exit 1
    fi

    # Register synchronously so both `open -a CuaDriver` and `tccutil reset`
    # resolve the replacement bundle rather than a stale LaunchServices entry.
    if macos_register_app "$APP_DEST"; then
        :
    else
        err "could not register the replacement app with LaunchServices; the replacement was rolled back"
        exit 1
    fi

    # Re-check the installed copy against the old requirement. A disagreement
    # with the staged result means the copy did not preserve the expected code
    # identity and must not trigger a destructive permission reset.
    if [[ -n "$PREVIOUS_REQUIREMENT" ]]; then
        INSTALLED_COMPATIBILITY="$(macos_requirement_compatibility \
            "$PREVIOUS_REQUIREMENT" "$APP_DEST")"
        if [[ "$INSTALLED_COMPATIBILITY" != "$REQUIREMENT_COMPATIBILITY" ]]; then
            if [[ "$INSTALLED_COMPATIBILITY" == "unknown" ]]; then
                err "could not re-verify the installed app's signing compatibility; the replacement was rolled back"
            else
                err "installed app's signing compatibility changed during copy; the replacement was rolled back"
            fi
            exit 1
        fi
    fi

    # Keep the authenticated previous app in the rollback slot until every
    # required TCC operation succeeds. A failed reset must not discard the
    # retryable prior installation.
    if ! macos_reset_tcc_after_requirement_change "$REQUIREMENT_COMPATIBILITY"; then
        exit 1
    fi
    ln -sf "$APP_BINARY" "$BIN_LINK"
    log "symlinked $BIN_LINK -> $APP_BINARY"
    MACOS_APP_INSTALL_COMMITTED=1
    if [[ "$MACOS_LEGACY_RS_REMOVAL_STARTED" == "1" ]]; then
        MACOS_LEGACY_RS_REMOVAL_COMMITTED=1
    fi
    # Only a fully trusted and committed release may remove stale install-local
    # payloads and its signing marker.
    cleanup_prior_local_install
else
    # Linux: versioned-dirs + atomic `current` symlink swap.
    #
    # Layout under $HOME_DIR/packages/:
    #   releases/<version>-<target>/cua-driver   (this install)
    #   releases/<older>-<target>/cua-driver     (kept for rollback)
    #   current/cua-driver -> ../releases/<active>-<target>/cua-driver
    #
    # Swap mechanics: write the new symlink to `current.tmp`, then
    # `mv -Tf current.tmp current` so the rename is a single
    # filesystem call. A daemon that already mmap'd the previous
    # `current/cua-driver` keeps using the open file handle — Unix
    # only invalidates path-based lookups, not held fds.
    if ! stop_cua_driver_daemons; then
        err "refusing to replace the Linux runtime while its supervisor or daemon remains active"
        exit 1
    fi
    DAEMONS_STOPPED_BEFORE_SWAP=1
    cleanup_prior_local_install
    PACKAGES_DIR="$HOME_DIR/packages"
    RELEASES_DIR="$PACKAGES_DIR/releases"
    CURRENT_LINK="$PACKAGES_DIR/current"
    VERSIONED_DIR="$RELEASES_DIR/${VERSION}-${TARGET}"

    mkdir -p "$VERSIONED_DIR"
    install -m 0755 "$SRC" "$VERSIONED_DIR/$BINARY_NAME"
    if [[ "$THEME_AVAILABLE" == "1" ]]; then
        install -m 0755 "$SRC_THEME" "$VERSIONED_DIR/cua-cursor-theme"
    fi
    if [[ -d "${SRC_WAYLAND_HELPER:-}" ]]; then
        mkdir -p "$VERSIONED_DIR/wayland-helper"
        cp -R "$SRC_WAYLAND_HELPER/." "$VERSIONED_DIR/wayland-helper/"

        INSTALLED_WAYLAND_HELPER="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/winrects@cua"
        if [[ -d "$INSTALLED_WAYLAND_HELPER" ]]; then
            cp "$SRC_WAYLAND_HELPER/winrects@cua/metadata.json" \
                "$SRC_WAYLAND_HELPER/winrects@cua/extension.js" \
                "$INSTALLED_WAYLAND_HELPER/"
            log "updated installed GNOME helper; reload the GNOME session to activate it"
        fi
    fi
    log "installed $VERSIONED_DIR/$BINARY_NAME (version $VERSION, target $TARGET)"

    # `ln -sfn` would replace an existing dir-symlink in place but is
    # not atomic on Linux (it unlinks then symlinks). Use a tmp symlink
    # + atomic rename instead so a concurrent `cua-driver` lookup
    # always sees either the old or new target, never an absent path.
    TMP_LINK="$PACKAGES_DIR/.current.$$"
    rm -rf "$TMP_LINK"
    # Relative target so the link is portable if $HOME_DIR is moved.
    ln -s "releases/${VERSION}-${TARGET}" "$TMP_LINK"
    # `mv -Tf` is the atomic-rename form on GNU coreutils (Linux). On
    # BSD mv (macOS — which doesn't take this branch in production, but
    # we still want this script to be runnable from a macOS dev shell
    # for testing) `-T` is unknown; fall back to a non-atomic rm+mv.
    if ! mv -Tf "$TMP_LINK" "$CURRENT_LINK" 2>/dev/null; then
        rm -rf "$CURRENT_LINK"
        mv "$TMP_LINK" "$CURRENT_LINK"
    fi
    log "current -> releases/${VERSION}-${TARGET}"

    # Visible PATH entry: replace whatever was at $BIN_LINK (could be
    # an old plain binary from a pre-versioned-dirs install) with a
    # symlink into `current`.
    rm -f "$BIN_LINK"
    ln -s "$CURRENT_LINK/$BINARY_NAME" "$BIN_LINK"
    log "symlinked $BIN_LINK -> $CURRENT_LINK/$BINARY_NAME"

    # Post-install GC of old per-version release dirs. Runs AFTER the
    # atomic `current` swap above so the about-to-be-active version is
    # never a deletion candidate (it's both the newest by mtime and
    # exempted via the current-symlink check inside prune_old_releases).
    prune_old_releases "$RELEASES_DIR" "$CURRENT_LINK" "$TARGET" "$KEEP_VERSIONS"
fi

# --- Sweep the legacy ~/.cua-driver-rs home -----------------------------
#
# This release installer used to default HOME_DIR to ~/.cua-driver-rs (the
# pre-v0.2.16 name). Now that it writes to ~/.cua-driver like install-local
# and the runtime, a prior RELEASE install can have left a stale
# ~/.cua-driver-rs behind — the source of the two-homes collision this PR
# fixes. Sweep it now that the new install is fully staged under the canonical
# home, mirroring the same belt-and-braces sweep _install-local-rust.sh does.
# Runs AFTER staging so we never delete state before the replacement exists;
# skipped when the user pinned CUA_DRIVER_RS_HOME to the legacy path on
# purpose. Best-effort + idempotent.
# Telemetry identity files are not carried over; they are swept with the rest.
if [[ -d "$LEGACY_HOME_DIR" && "$HOME_DIR" != "$LEGACY_HOME_DIR" ]]; then
    rm -rf "$LEGACY_HOME_DIR" 2>/dev/null \
        && log "swept legacy package home $LEGACY_HOME_DIR (reconciled onto $HOME_DIR)" \
        || log "note: could not fully remove legacy package home $LEGACY_HOME_DIR (best-effort)"
fi

# --- Stop any pre-swap cua-driver daemons -------------------------------
#
# Mirror of install.ps1's `Stop-CuaDriverDaemons` call sequence. The
# new binary is now in place under packages/current/ (Linux) or
# /Applications/CuaDriver.app (macOS) — kill any in-memory daemon
# that was holding the OLD binary so the next `cua-driver` invocation
# picks up the freshly-installed code. Without this, an autostart
# LaunchAgent / systemd user unit / manual `serve` shell keeps serving
# pre-upgrade behaviour until logout, which is what surfaces to users
# as "the bug I just fixed is still there".
if [[ "$DAEMONS_STOPPED_BEFORE_SWAP" != "1" ]]; then
    stop_cua_driver_daemons
    show_cua_driver_daemon_survivors
fi

# Agent skill pack: NOT auto-linked. The install script never touches
# ~/.claude/skills/, ~/.agents/skills/, etc. Run `cua-driver skills
# install` after install to fetch + symlink the skill pack from the
# matching GitHub release. The post-install hint below points at the
# verb.

# --- Upstream telemetry notice ------------------------------------------
#
# This inherited installer downloads upstream release binaries. It no longer
# records an install event, writes an attribution hint, or carries a telemetry
# ID forward, but the upstream binary itself sends usage telemetry by default.
# The driver built from this repository has no telemetry at all.
echo "Note: this installer installs an upstream Cua Driver release, which sends usage telemetry by default."
echo "  Disable it persistently: $BIN_LINK telemetry disable (or set CUA_DRIVER_RS_TELEMETRY_ENABLED=0)."
echo "  The driver built from this repository (scripts/install-local.sh) has no telemetry."

# Auto-extend PATH for users whose shell doesn't already include BIN_DIR.
if [[ "$NO_MODIFY_PATH" != "1" ]] && [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    SHELL_RC=""
    case "${SHELL:-}" in
        */zsh)  SHELL_RC="$HOME/.zshrc"  ;;
        */bash) SHELL_RC="$HOME/.bashrc" ;;
    esac
    if [[ -n "$SHELL_RC" ]]; then
        {
            printf '\n# Added by cua-driver-rs installer — see https://github.com/trycua/cua\n'
            printf 'export PATH="%s:$PATH"\n' "$BIN_DIR"
        } >> "$SHELL_RC"
        log "appended PATH update to $SHELL_RC — open a new shell or run \`source $SHELL_RC\`"
    else
        log "WARNING: $BIN_DIR is not on PATH; add it manually."
    fi
fi

echo ""
echo "cua-driver-rs $VERSION installed."
echo ""

if [[ "${REPLACED_CANONICAL:-0}" == "1" ]]; then
    echo "Upgraded the cua-driver bundle that was previously at $APP_DEST."
    case "${REQUIREMENT_COMPATIBILITY:-unknown}" in
        compatible)
            echo "Verified that the replacement satisfies the previous code-signing"
            echo "requirement, so existing Accessibility and Screen Recording grants"
            echo "were preserved."
            ;;
        incompatible)
            echo "The code-signing requirement changed, so stale Accessibility,"
            echo "Screen Recording, and Automation rows were cleared. Re-authorize with:"
            echo "  cua-driver permissions grant"
            ;;
        *)
            echo "The previous code-signing requirement could not be verified. Existing"
            echo "TCC rows were left unchanged to avoid destroying valid grants."
            ;;
    esac
    echo ""
fi

if [[ "${MIGRATED_LEGACY_ID:-0}" == "1" ]]; then
    echo "Migrated CuaDriver.app from com.trycua.driver to com.meta.musecode.cua.driver."
    echo "The bundle identity changed, so authorize Accessibility and Screen Recording"
    echo "for the new Muse Code Driver identity. Legacy TCC rows were cleared safely."
    echo ""
fi

if [[ "${MIGRATED_LEGACY_RS_ID:-0}" == "1" ]]; then
    echo "Removed authenticated legacy CuaDriverRs.app ($LEGACY_RS_BUNDLE_ID)."
    echo "Its Accessibility, Screen Recording, Automation, and LaunchServices state"
    echo "was cleared before removal."
    echo ""
fi

# Unified post-install hints come from a single shared text file so the
# 4 Rust installers (this script + install.ps1 + install-local.sh +
# install-local.ps1) never drift. The .txt holds the OS-agnostic bulk
# (Try-it / skill pack / MCP setup / docs link) with {{BINARY}}
# placeholders; OS-specific bits (autostart / TCC) stay inline below
# in each installer where they're per-shell natural.
HINTS_URL="https://cua.ai/driver/post-install-hints.txt"
HINTS_TXT="$TMP_DIR/post-install-hints.txt"
if curl -fsSL "$HINTS_URL" -o "$HINTS_TXT" 2>/dev/null && [ -s "$HINTS_TXT" ]; then
    sed "s|{{BINARY}}|$BIN_LINK|g" "$HINTS_TXT"
else
    # Network fetch failed — print a one-line essentials fallback so the
    # user always gets enough to recover, even if hint-text fetching is
    # blocked. Skip everything else.
    echo "Next steps: $BIN_LINK --version  |  $BIN_LINK mcp-config  |  $BIN_LINK skills install"
    echo "Docs: https://github.com/trycua/cua/tree/main/libs/cua-driver/rust"
fi

case "$(uname -s)" in
    Darwin)
        echo ""
        echo "macOS TCC: grant Accessibility + Screen Recording on first run:"
        echo "  open -n -g -a CuaDriver --args serve"
        echo "  $BIN_LINK check_permissions"
        ;;
    Linux)
        echo ""
        echo "Auto-start at logon (optional):"
        echo "  Re-run the local installer with --autostart to register a systemd user unit."
        ;;
esac
