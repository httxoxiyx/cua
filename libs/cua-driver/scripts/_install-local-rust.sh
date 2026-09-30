#!/usr/bin/env bash
#
# cua-driver-rs local/debug installer (macOS + Linux). Builds from the
# current source tree into a durable, separate local-product namespace.
#
# Private helper — invoked by install-local.sh (the multi-backend
# dispatcher) when the user picks --backend=rust / --experimental-rust
# or runs on a non-macOS host. Do not invoke directly; flag parity with
# the dispatcher's argv shape is maintained from there.
#
# Rust local installer (dev-only helper for libs/cua-driver/rust):
#   --release    build the release configuration (default: debug)
#   --autostart  register an auto-start daemon (macOS: LaunchAgent;
#                Linux: systemd user unit). Default off; the post-install
#                message prints the registration command for the platform.
#   --bin-dir <path>
#                install the visible symlink to <path> instead of
#                ~/.local/bin. Takes precedence over
#                CUA_DRIVER_LOCAL_INSTALL_DIR; must be absolute.
#   CUA_DRIVER_LOCAL_HOME must be an absolute, non-symlink directory below
#   HOME and must not overlap the release-owned $HOME/.cua-driver directory.
#
# Not for end-users — scripts/install.sh fetches a built release from
# GitHub. This script is for the developer loop (rapid edit/build/test
# on a Linux or macOS host).
#
# Linux layout produced (matches install.sh):
#
#   ${CUA_DRIVER_LOCAL_HOME:-$HOME/.cua-driver-local}/packages/
#       releases/<version>-local-<config>-<target>/cua-driver-local
#       current/cua-driver-local
#   ${CUA_DRIVER_LOCAL_INSTALL_DIR:-$HOME/.local/bin}/cua-driver-local
#
# macOS layout produced:
#   /Applications/MuseCodeCuaDriverLocal.app/Contents/MacOS/cua-driver-local
#   $HOME/.local/bin/cua-driver-local -> .../MuseCodeCuaDriverLocal.app/Contents/MacOS/cua-driver-local
#
# The version string carries `-local-debug` / `-local-release` so it
# never collides with a real release dir and is trivial to GC.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# Shared process, supervisor, and macOS signing helpers have no top-level side
# effects. Load them before staging so Linux can prove the old supervised
# daemon is quiescent before replacing its executable.
# shellcheck source-path=SCRIPTDIR
# shellcheck source=_local-signing.sh
. "$SCRIPT_DIR/_local-signing.sh"
# Rust workspace root: scripts/ is the cross-cutting installer dir at
# libs/cua-driver/scripts/; the Cargo workspace lives one level deeper
# under libs/cua-driver/rust/.
REPO_ROOT="$(cd "$SCRIPT_DIR/../rust" && pwd)"

# Embed local-build provenance in `get_config`. An explicit value remains
# authoritative for source snapshots copied to VMs without `.git`; otherwise
# derive HEAD from the checkout that is actually being built. Keep dirty local
# developer builds honest instead of claiming byte-for-byte provenance from
# the clean commit.
if [ -z "${CUA_DRIVER_SOURCE_SHA:-}" ]; then
    if ! command -v git >/dev/null 2>&1; then
        echo "error: git is required to determine CUA_DRIVER_SOURCE_SHA; set it explicitly for a source snapshot" >&2
        exit 1
    fi
    CUA_DRIVER_SOURCE_SHA="$(git -C "$REPO_ROOT" rev-parse --verify 'HEAD^{commit}' 2>/dev/null || true)"
    if ! printf '%s' "$CUA_DRIVER_SOURCE_SHA" | grep -Eq '^[0-9a-fA-F]{40}([0-9a-fA-F]{24})?$'; then
        echo "error: could not determine an exact Git commit for $REPO_ROOT; set CUA_DRIVER_SOURCE_SHA explicitly" >&2
        exit 1
    fi
    if [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=normal 2>/dev/null)" ]; then
        CUA_DRIVER_SOURCE_SHA="${CUA_DRIVER_SOURCE_SHA}-dirty"
    fi
fi
export CUA_DRIVER_SOURCE_SHA

BOLD=$(tput bold 2>/dev/null || true)
NORMAL=$(tput sgr0 2>/dev/null || true)
RED=$(tput setaf 1 2>/dev/null || true)
GREEN=$(tput setaf 2 2>/dev/null || true)
BLUE=$(tput setaf 4 2>/dev/null || true)
YELLOW=$(tput setaf 3 2>/dev/null || true)

if [ "$(id -u)" -eq 0 ] || [ -n "${SUDO_USER:-}" ]; then
    echo "${RED}Error: do not run this script with sudo or as root.${NORMAL}"
    echo "It prompts for sudo on the specific operations that need it."
    exit 1
fi

validate_local_install_home_dir() {
    local home_dir="$1" user_home="${2%/}" resolved_home resolved_dir
    local relative prefix component child old_ifs="$IFS"
    local components=()

    case "$user_home" in
        /*) ;;
        *) echo "error: HOME must be an absolute path" >&2; return 1 ;;
    esac
    [ -n "$user_home" ] && [ "$user_home" != "/" ] || {
        echo "error: refusing unsafe HOME: ${user_home:-<empty>}" >&2
        return 1
    }
    case "$home_dir" in
        /*) ;;
        *) echo "error: CUA_DRIVER_LOCAL_HOME must be an absolute path" >&2; return 1 ;;
    esac
    case "$home_dir" in
        /|"$user_home"|"$user_home"/|"$user_home/.cua-driver"|"$user_home/.cua-driver"/|*//*|*/../*|*/..|*/./*|*/.)
            echo "error: refusing unsafe or release-owned local home: $home_dir" >&2
            return 1
            ;;
    esac
    [[ "$home_dir" != *$'\n'* && "$home_dir" != *$'\r'* ]] || {
        echo "error: refusing local home containing a line break" >&2
        return 1
    }
    resolved_home="$(CDPATH= cd -- "$user_home" 2>/dev/null && pwd -P)" || {
        echo "error: could not resolve HOME safely: $user_home" >&2
        return 1
    }
    case "$home_dir" in
        "$user_home"/*) ;;
        *) echo "error: CUA_DRIVER_LOCAL_HOME must remain inside HOME ($resolved_home): $home_dir" >&2; return 1 ;;
    esac

    relative="${home_dir#"$user_home"/}"
    prefix="$user_home"
    IFS='/' read -r -a components <<< "$relative"
    IFS="$old_ifs"
    for component in "${components[@]}"; do
        [ -n "$component" ] && [ "$component" != "." ] && [ "$component" != ".." ] || return 1
        prefix="$prefix/$component"
        if [ -L "$prefix" ] || { [ -e "$prefix" ] && [ ! -d "$prefix" ]; }; then
            echo "error: refusing symlink or non-directory local home component: $prefix" >&2
            return 1
        fi
    done
    if [ -d "$home_dir" ]; then
        resolved_dir="$(CDPATH= cd -- "$home_dir" 2>/dev/null && pwd -P)" || return 1
        case "$resolved_dir" in
            "$resolved_home"/*) ;;
            *) echo "error: local home resolves outside HOME: $resolved_dir" >&2; return 1 ;;
        esac
    fi
    for child in "$home_dir/packages" "$home_dir/packages/releases"; do
        if [ -L "$child" ] || { [ -e "$child" ] && [ ! -d "$child" ]; }; then
            echo "error: refusing unsafe managed local install directory: $child" >&2
            return 1
        fi
    done
}

# --- Parse arguments ----------------------------------------------------

BUILD_CONFIG="debug"
INSTALL_AUTOSTART=false
# Empty means "not passed" — the env var / default applies instead (see BIN_DIR below).
BIN_DIR_OVERRIDE=""
case "${CUA_DRIVER_REQUIRE_STABLE_SIGNING:-0}" in
    0|false|no|"") CUA_DRIVER_REQUIRE_STABLE_SIGNING=0 ;;
    1|true|yes) CUA_DRIVER_REQUIRE_STABLE_SIGNING=1 ;;
    *)
        echo "${RED}Error: CUA_DRIVER_REQUIRE_STABLE_SIGNING must be 0 or 1.${NORMAL}" >&2
        exit 2
        ;;
esac
export CUA_DRIVER_REQUIRE_STABLE_SIGNING

while [ "$#" -gt 0 ]; do
    case "$1" in
        --release)
            BUILD_CONFIG="release"
            ;;
        --autostart)
            INSTALL_AUTOSTART=true
            ;;
        --require-stable-signing)
            CUA_DRIVER_REQUIRE_STABLE_SIGNING=1
            export CUA_DRIVER_REQUIRE_STABLE_SIGNING
            ;;
        --bin-dir)
            if [ "$#" -lt 2 ]; then
                echo "${RED}Error: --bin-dir requires a value.${NORMAL}" >&2
                exit 2
            fi
            BIN_DIR_OVERRIDE="$2"
            shift
            ;;
        --bin-dir=*)
            BIN_DIR_OVERRIDE="${1#*=}"
            ;;
        --help|-h)
            echo "${BOLD}${BLUE}cua-driver-rs local installer${NORMAL}"
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --release     Build the release configuration (default: debug)."
            echo "  --autostart   Also register a logon-time daemon:"
            echo "                  macOS: LaunchAgent under ~/Library/LaunchAgents"
            echo "                  Linux: systemd --user unit"
            echo "                On macOS this also fixes TCC: a launchd-started daemon"
            echo "                is attributed to com.meta.musecode.cua.driver.local (not your terminal),"
            echo "                so you grant Accessibility + Screen Recording once and"
            echo "                every cua-driver-local call/mcp routes through it correctly."
            echo "  --bin-dir <path>"
            echo "                Install the visible cua-driver-local symlink to <path>"
            echo "                instead of ~/.local/bin. Must be an absolute path; takes"
            echo "                precedence over CUA_DRIVER_LOCAL_INSTALL_DIR."
            echo "  --require-stable-signing"
            echo "                On macOS, stop before replacing the installed app unless"
            echo "                a certificate-backed identity is available. Recommended"
            echo "                for behavior and E2E verification."
            echo "  --help        Show this help."
            echo ""
            echo "Examples:"
            echo "  $0                       # debug build, install junction layout"
            echo "  $0 --release             # release build"
            echo "  $0 --release --autostart # release + daemon at logon"
            exit 0
            ;;
        *)
            echo "${RED}Unknown option: $1${NORMAL}"
            echo "Use --help for usage."
            exit 1
            ;;
    esac
    shift
done

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin) TARGET_TRIPLE="${ARCH}-apple-darwin" ;;
    Linux)  TARGET_TRIPLE="${ARCH}-unknown-linux-gnu" ;;
    *)      echo "${RED}Unsupported OS: $OS${NORMAL}"; exit 1 ;;
esac

HOME_DIR="${CUA_DRIVER_LOCAL_HOME:-$HOME/.cua-driver-local}"
validate_local_install_home_dir "$HOME_DIR" "$HOME" || exit 2
BIN_DIR="${BIN_DIR_OVERRIDE:-${CUA_DRIVER_LOCAL_INSTALL_DIR:-$HOME/.local/bin}}"
# The symlink is created after this script cds into the Cargo workspace, so a
# relative path would silently land inside rust/ — and uninstall-local.sh
# rejects relative values outright, leaving it unremovable. Fail loudly instead.
case "$BIN_DIR" in
    /*) ;;
    *)
        echo "${RED}Error: bin dir must be an absolute path (got: $BIN_DIR).${NORMAL}" >&2
        echo "Set it via --bin-dir /abs/path or CUA_DRIVER_LOCAL_INSTALL_DIR=/abs/path." >&2
        exit 2
        ;;
esac
RELEASES_DIR="$HOME_DIR/packages/releases"
CURRENT_LINK="$HOME_DIR/packages/current"
LOCAL_SYSTEMD_UNIT="$HOME/.config/systemd/user/cua-driver-local.service"

VERSION_TAG="0.0.0-local-$BUILD_CONFIG"
VERSIONED_DIR="$RELEASES_DIR/$VERSION_TAG-$TARGET_TRIPLE"

echo "${BOLD}${BLUE}cua-driver-rs local installer${NORMAL}"
echo "  source:  ${BOLD}$REPO_ROOT${NORMAL}"
echo "  sha:     ${BOLD}$CUA_DRIVER_SOURCE_SHA${NORMAL}"
echo "  config:  ${BOLD}$BUILD_CONFIG${NORMAL}"
echo "  target:  ${BOLD}$TARGET_TRIPLE${NORMAL}"
echo "  bin:     ${BOLD}$BIN_DIR/cua-driver-local${NORMAL}"
echo "  current: ${BOLD}$CURRENT_LINK${NORMAL}"
echo ""

# --- Prerequisites ------------------------------------------------------

if ! command -v cargo >/dev/null 2>&1; then
    # Common rustup default install at $HOME/.cargo/bin/cargo — source the
    # rustup-shipped env script if present so cargo + rustc + the active
    # toolchain shims all land on PATH for the rest of this script. This
    # matters because rustup-init writes the PATH-prepending line into the
    # user's shell rc, which only takes effect in NEW interactive shells —
    # a fresh post-rustup invocation of `./install-local.sh` in the same
    # shell as the rustup install would otherwise fail here even though
    # cargo is on disk.
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    elif [ -x "$HOME/.cargo/bin/cargo" ]; then
        # Older rustup installs (or non-rustup Cargo installs) may lack
        # the env script — directly prepend the canonical bin dir.
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "${RED}Error: cargo not found on PATH.${NORMAL}"
    echo "Install Rust via rustup: https://rustup.rs/"
    echo "After install, either open a new shell or run: . \$HOME/.cargo/env"
    exit 1
fi

# --- Build --------------------------------------------------------------

# Keep Cargo's output directory and the binary staged below on one path.
# Cargo resolves a relative CARGO_TARGET_DIR from the workspace we build in,
# so make that resolution explicit before invoking it.
BUILD_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$BUILD_TARGET_DIR" in
    /*) ;;
    *) BUILD_TARGET_DIR="$REPO_ROOT/$BUILD_TARGET_DIR" ;;
esac
export CARGO_TARGET_DIR="$BUILD_TARGET_DIR"

echo "${BOLD}Building cua-driver ($BUILD_CONFIG)...${NORMAL}"
cd "$REPO_ROOT"
if [ "$BUILD_CONFIG" = "release" ]; then
    cargo build --release -p cua-driver -p cursor-theme-cli
else
    cargo build -p cua-driver -p cursor-theme-cli
fi

BUILT_BINARY="$BUILD_TARGET_DIR/$BUILD_CONFIG/cua-driver"
BUILT_THEME_BINARY="$BUILD_TARGET_DIR/$BUILD_CONFIG/cua-cursor-theme"
if [ ! -x "$BUILT_BINARY" ]; then
    echo "${RED}Error: build produced no binary at $BUILT_BINARY${NORMAL}"
    exit 1
fi
if [ ! -x "$BUILT_THEME_BINARY" ]; then
    echo "${RED}Error: build produced no cursor-theme compiler at $BUILT_THEME_BINARY${NORMAL}"
    exit 1
fi
echo ""

stop_local_linux_daemons_before_runtime_change() {
    local candidate resolved records="" status=0
    local owned_paths=(
        "$BIN_DIR/cua-driver-local"
        "$CURRENT_LINK/cua-driver-local"
    )
    stop_and_verify_local_systemd_service \
        "cua-driver-local.service" "$LOCAL_SYSTEMD_UNIT" || return 1
    for candidate in "$HOME_DIR"/packages/releases/*/cua-driver-local; do
        [ -e "$candidate" ] || [ -L "$candidate" ] || continue
        owned_paths+=("$candidate")
    done
    for candidate in "${owned_paths[@]}"; do
        resolved="$(realpath "$candidate" 2>/dev/null || true)"
        [ -n "$resolved" ] && owned_paths+=("$resolved")
    done
    stop_verified_local_processes 0 "${owned_paths[@]}" || return 1
    stop_and_verify_local_systemd_service \
        "cua-driver-local.service" "$LOCAL_SYSTEMD_UNIT" || return 1
    records="$(local_owned_process_records 0 "${owned_paths[@]}")" || status=$?
    if [ "$status" != "0" ] || [ -n "$records" ]; then
        echo "${RED:-}Error: local daemon respawned or could not be inspected after systemd shutdown.${NORMAL:-}" >&2
        return 1
    fi
}

if [ "$OS" = "Linux" ] \
   && ! stop_local_linux_daemons_before_runtime_change; then
    exit 1
fi

# --- Stage into versioned release dir + repoint `current` --------------

echo "${BOLD}Staging into $VERSIONED_DIR${NORMAL}"
mkdir -p "$VERSIONED_DIR"

# Copy through a temp file in the same directory, then rename over the
# destination.
#
# A plain `cp` opens the destination with O_TRUNC and writes in place. When a
# previous cua-driver-local is still running out of that exact path — the
# common case, since the version tag is stable per config, so every rebuild
# targets the same file — Linux refuses the open with ETXTBSY and the install
# dies mid-stage:
#
#   cp: cannot create regular file '.../cua-driver-local': Text file busy
#
# The daemon stop further below cannot prevent this: it runs after the swap,
# and a manually launched `serve` is not always reachable by it anyway.
# rename(2) has no such restriction — the running process keeps executing the
# old inode until it exits, and the new bytes are published atomically, so a
# concurrent exec sees either the whole old binary or the whole new one.
stage_binary() {
    stage_src="$1"
    stage_dest="$2"
    stage_tmp="$stage_dest.stage.$$"
    rm -f "$stage_tmp"
    cp "$stage_src" "$stage_tmp"
    chmod +x "$stage_tmp"
    mv -f "$stage_tmp" "$stage_dest"
}
stage_binary "$BUILT_BINARY" "$VERSIONED_DIR/cua-driver-local"
stage_binary "$BUILT_THEME_BINARY" "$VERSIONED_DIR/cua-cursor-theme"

# Re-sign with a fresh ad-hoc signature.
#
# macOS 26+ Taskgated rejects the linker-emitted ad-hoc signature once
# the binary has been copied (the kernel's cached signature for the new
# inode doesn't match the embedded one strictly enough for the newer
# CODESIGNING namespace). Result is `SIGKILL (Code Signature Invalid)
# — Taskgated Invalid Signature` on first run, no stderr output, exit
# code 137 — extremely confusing without a diagnostic-report dig. The
# fix: re-sign in place. `codesign --force --sign -` emits a fresh
# ad-hoc signature keyed to the new on-disk bytes, which Taskgated
# accepts. Cheap (~50ms on a 40MB binary). macOS-only — no-op on Linux.
if [ "$OS" = "Darwin" ]; then
    if command -v codesign >/dev/null 2>&1; then
        codesign --force --sign - "$VERSIONED_DIR/cua-driver-local" 2>/dev/null \
            || echo "${YELLOW}warning: codesign --force --sign - failed; first run may fail with SIGKILL on macOS 26+${NORMAL}" >&2
        codesign --force --sign - "$VERSIONED_DIR/cua-cursor-theme" 2>/dev/null \
            || echo "${YELLOW}warning: cursor-theme sidecar signing failed${NORMAL}" >&2
    fi
fi

# Skill pack — stage from the repo so the `current` symlink below
# transparently exposes it to agents. Mirrors what install.sh does
# from a release tarball.
SOURCE_SKILLS="$REPO_ROOT/Skills/cua-driver"
if [ -d "$SOURCE_SKILLS" ]; then
    STAGED_SKILLS="$VERSIONED_DIR/Skills/cua-driver"
    rm -rf "$STAGED_SKILLS"
    mkdir -p "$(dirname "$STAGED_SKILLS")"
    cp -R "$SOURCE_SKILLS" "$STAGED_SKILLS"
    echo "${GREEN}staged skill pack at $STAGED_SKILLS${NORMAL}"
fi

# Keep an already-installed GNOME helper aligned with the source-built driver.
# Installing the helper is still opt-in. Once present, however, leaving old
# compositor artwork behind after install-local creates a misleading
# cross-platform mismatch.
if [ "$OS" = "Linux" ]; then
    SOURCE_WAYLAND_HELPER="$REPO_ROOT/../wayland-helper"
    if [ -d "$SOURCE_WAYLAND_HELPER/winrects@cua" ]; then
        STAGED_WAYLAND_HELPER="$VERSIONED_DIR/wayland-helper"
        mkdir -p "$STAGED_WAYLAND_HELPER"
        cp -R "$SOURCE_WAYLAND_HELPER/." "$STAGED_WAYLAND_HELPER/"

        INSTALLED_WAYLAND_HELPER="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/winrects@cua"
        if [ -d "$INSTALLED_WAYLAND_HELPER" ]; then
            cp "$SOURCE_WAYLAND_HELPER/winrects@cua/metadata.json" \
                "$SOURCE_WAYLAND_HELPER/winrects@cua/extension.js" \
                "$INSTALLED_WAYLAND_HELPER/"
            echo "${GREEN}updated installed GNOME helper; reload the GNOME session to activate it${NORMAL}"
        fi
    fi
fi

# Atomically point `current` at the new versioned release dir.
#
# Previous version used `ln -s … current.new` + `mv -Tf current.new current`
# with a BSD `mv -f` fallback. The BSD fallback path is broken: when the
# destination is a symlink-to-directory, BSD `mv` *follows* it and drops
# the temp symlink INSIDE the directory as `current/current.new`, leaving
# stale `current.new` orphans at both levels and the actual `current`
# symlink untouched. macOS doesn't ship GNU `mv` so the `-Tf` path never
# fires on this host.
#
# `ln -sfn` is the POSIX primitive that does what we wanted from the
# start: replace the existing symlink atomically, without dereferencing.
# Works the same on macOS BSD and Linux GNU coreutils. No temp file
# means no orphan to clean up on partial failure.
mkdir -p "$HOME_DIR/packages"
# Sweep any orphan temp from a previous (pre-fix) run before re-creating.
rm -f "$CURRENT_LINK.new"
ln -sfn "$VERSIONED_DIR" "$CURRENT_LINK"
echo "${GREEN}current -> $VERSIONED_DIR${NORMAL}"
echo ""

# --- macOS: stable local code-signing identity (so TCC grants survive rebuilds) ---
#
# Keep policy in a sourceable helper so strict/fallback behavior can be tested
# without building or installing the app. The helper is sourced above because
# Linux shutdown must run before the versioned runtime is replaced.

# --- macOS: wrap the binary in MuseCodeCuaDriverLocal.app for a stable TCC identity ---
#
# TCC keys Accessibility / Screen-Recording grants on the bundle
# identifier (com.meta.musecode.cua.driver.local), not the bare executable path. A loose
# binary gets grants attributed to its ad-hoc cdhash, which changes on
# every rebuild — so permissions silently reset and never appear cleanly
# under System Settings. Mirror the production path (install.sh) + the CD
# bundle-assembly step: drop the freshly built binary into the checked-in
# CuaDriverBundle skeleton, install the bundle to /Applications, and point
# the visible bin at the binary INSIDE the bundle. Linux/Windows have no
# .app concept and keep the bare-binary symlink below.
APP_DEST="/Applications/MuseCodeCuaDriverLocal.app"
LEGACY_LOCAL_APP="/Applications/CuaDriverLocal.app"
LOCAL_HISTORY_ROOT="$HOME/Library/Application Support/cua-driver-local/computer-history"
LOCAL_APP_SWAP_STARTED=0
LOCAL_APP_HAD_PREVIOUS=0
LOCAL_APP_INSTALL_COMMITTED=0
LOCAL_APP_BACKUP=""
LEGACY_LOCAL_APP_OWNED=0
LEGACY_LOCAL_APP_REMOVAL_STARTED=0
LEGACY_LOCAL_APP_BACKUP=""

rollback_local_app_on_exit() {
    [ "$LOCAL_APP_SWAP_STARTED" = "1" ] || return 0

    if [ "$LOCAL_APP_INSTALL_COMMITTED" = "1" ]; then
        if ! remove_authenticated_local_app_backup "$LOCAL_APP_BACKUP" \
            "com.meta.musecode.cua.driver.local" "cua-driver-local"; then
            echo "${RED:-}Error: committed local app is usable, but its authenticated install backup could not be removed.${NORMAL:-}" >&2
            return 1
        fi
        LOCAL_APP_SWAP_STARTED=0
        return 0
    fi

    if ! restore_local_app_backup "$APP_DEST" "$LOCAL_APP_BACKUP" \
        "com.meta.musecode.cua.driver.local" "cua-driver-local"; then
        echo "${RED:-}Error: interrupted local app replacement could not be rolled back safely; inspect $LOCAL_APP_BACKUP before retrying.${NORMAL:-}" >&2
        return 1
    fi
    LOCAL_APP_SWAP_STARTED=0
    if [ "$LOCAL_APP_HAD_PREVIOUS" = "1" ]; then
        echo "${YELLOW:-}warning: restored the previous MuseCodeCuaDriverLocal.app after an interrupted installation.${NORMAL:-}" >&2
    fi
}

rollback_legacy_local_app_on_exit() {
    [ "$LEGACY_LOCAL_APP_REMOVAL_STARTED" = "1" ] || return 0

    if [ "$LOCAL_APP_INSTALL_COMMITTED" = "1" ]; then
        if ! remove_authenticated_local_app_backup "$LEGACY_LOCAL_APP_BACKUP" \
            "com.trycua.driver.local" "cua-driver-local"; then
            echo "${RED:-}Error: committed local install is usable, but its authenticated legacy-app backup could not be removed.${NORMAL:-}" >&2
            return 1
        fi
        LEGACY_LOCAL_APP_REMOVAL_STARTED=0
        return 0
    fi

    if [ -e "$LEGACY_LOCAL_APP_BACKUP" ] || [ -L "$LEGACY_LOCAL_APP_BACKUP" ]; then
        if ! verify_local_app_identity "$LEGACY_LOCAL_APP_BACKUP" \
            "com.trycua.driver.local" "cua-driver-local"; then
            echo "${RED:-}Error: refusing to restore unauthenticated legacy local-app backup at $LEGACY_LOCAL_APP_BACKUP.${NORMAL:-}" >&2
            return 1
        fi
        if [ -e "$LEGACY_LOCAL_APP" ] || [ -L "$LEGACY_LOCAL_APP" ]; then
            echo "${RED:-}Error: refusing to overwrite $LEGACY_LOCAL_APP while restoring its authenticated backup.${NORMAL:-}" >&2
            return 1
        fi
        if ! mv "$LEGACY_LOCAL_APP_BACKUP" "$LEGACY_LOCAL_APP" \
           || ! register_legacy_local_app "$LEGACY_LOCAL_APP"; then
            echo "${RED:-}Error: could not restore and register legacy local app $LEGACY_LOCAL_APP.${NORMAL:-}" >&2
            return 1
        fi
    elif [ -e "$LEGACY_LOCAL_APP" ] || [ -L "$LEGACY_LOCAL_APP" ]; then
        # Preparation can fail after unregistering but before the atomic move.
        if ! verify_local_app_identity "$LEGACY_LOCAL_APP" \
            "com.trycua.driver.local" "cua-driver-local"; then
            echo "${RED:-}Error: refusing to re-register unauthenticated legacy local app $LEGACY_LOCAL_APP.${NORMAL:-}" >&2
            return 1
        fi
        if ! register_legacy_local_app "$LEGACY_LOCAL_APP"; then
            echo "${RED:-}Error: could not re-register preserved legacy local app $LEGACY_LOCAL_APP.${NORMAL:-}" >&2
            return 1
        fi
    fi
    LEGACY_LOCAL_APP_REMOVAL_STARTED=0
    echo "${YELLOW:-}warning: restored the legacy local app after an interrupted installation.${NORMAL:-}" >&2
}

local_install_exit() {
    local status=$?
    trap - EXIT INT TERM
    if ! rollback_local_app_on_exit; then
        status=1
    fi
    if ! rollback_legacy_local_app_on_exit; then
        status=1
    fi
    exit "$status"
}

# The EXIT handler covers ordinary errors, including failures inside explicit
# `if`/`||` checks. Signal handlers convert INT/TERM into conventional exit
# statuses and let EXIT perform exactly one rollback.
trap local_install_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

stop_local_daemons_before_identity_check() {
    local candidate resolved
    local owned_paths=(
        "$BIN_DIR/cua-driver-local"
        "$CURRENT_LINK/cua-driver-local"
        "$APP_DEST/Contents/MacOS/cua-driver-local"
    )
    if ! stop_and_verify_local_launchagent \
        "$HOME/Library/LaunchAgents/com.trycua.cua-driver-local.plist" \
        "com.trycua.cua-driver-local"; then
        return 1
    fi
    if [ "${LEGACY_LOCAL_APP_OWNED:-0}" = "1" ]; then
        owned_paths+=("$LEGACY_LOCAL_APP/Contents/MacOS/cua-driver-local")
    fi
    for candidate in "$HOME_DIR"/packages/releases/*/cua-driver-local; do
        [ -e "$candidate" ] || [ -L "$candidate" ] || continue
        owned_paths+=("$candidate")
    done
    for candidate in "${owned_paths[@]}"; do
        resolved="$(realpath "$candidate" 2>/dev/null || true)"
        [ -n "$resolved" ] && owned_paths+=("$resolved")
    done
    stop_verified_local_processes 0 "${owned_paths[@]}"
}

if [ "$OS" = "Darwin" ]; then
    SKELETON="$REPO_ROOT/scripts/CuaDriverBundle"
    if [ ! -d "$SKELETON/Contents" ]; then
        echo "${RED}Error: bundle skeleton missing at $SKELETON${NORMAL}" >&2
        exit 1
    fi
    APP_STAGE="$VERSIONED_DIR/MuseCodeCuaDriverLocal.app"
    rm -rf "$APP_STAGE"
    mkdir -p "$APP_STAGE/Contents/MacOS"
    cp -R "$SKELETON/Contents/." "$APP_STAGE/Contents/"
    cp "$VERSIONED_DIR/cua-driver-local" "$APP_STAGE/Contents/MacOS/cua-driver-local"
    cp "$VERSIONED_DIR/cua-cursor-theme" "$APP_STAGE/Contents/MacOS/cua-cursor-theme"
    chmod +x "$APP_STAGE/Contents/MacOS/cua-driver-local"
    chmod +x "$APP_STAGE/Contents/MacOS/cua-cursor-theme"
    rm -f "$APP_STAGE/Contents/MacOS/.gitkeep"
    if ! command -v codesign >/dev/null 2>&1; then
        echo "${RED}Error: codesign is required to install MuseCodeCuaDriverLocal.app safely.${NORMAL}" >&2
        exit 1
    fi
    PREVIOUS_REQUIREMENT=""
    if [ -e "$APP_DEST" ] || [ -L "$APP_DEST" ]; then
        if ! verify_local_app_identity "$APP_DEST" \
            "com.meta.musecode.cua.driver.local" "cua-driver-local"; then
            echo "${RED}Error: refusing to replace unauthenticated shared app path $APP_DEST.${NORMAL}" >&2
            exit 1
        fi
        if ! PREVIOUS_REQUIREMENT="$(designated_requirement "$APP_DEST")" \
           || [ -z "$PREVIOUS_REQUIREMENT" ]; then
            echo "${RED}Error: could not read the installed local app's designated requirement; refusing a TCC-unsafe replacement.${NORMAL}" >&2
            exit 1
        fi
    fi
    if [ -e "$LEGACY_LOCAL_APP" ] || [ -L "$LEGACY_LOCAL_APP" ]; then
        if ! verify_local_app_identity "$LEGACY_LOCAL_APP" \
            "com.trycua.driver.local" "cua-driver-local"; then
            echo "${RED}Error: refusing to modify unauthenticated legacy app path $LEGACY_LOCAL_APP.${NORMAL}" >&2
            exit 1
        fi
        LEGACY_LOCAL_APP_OWNED=1
    fi
    # Stamp the local build version so the bundle reports something sane.
    if command -v plutil >/dev/null 2>&1; then
        plutil -replace CFBundleShortVersionString -string "$VERSION_TAG" \
            "$APP_STAGE/Contents/Info.plist" 2>/dev/null || true
        plutil -replace CFBundleVersion -string "$VERSION_TAG" \
            "$APP_STAGE/Contents/Info.plist" 2>/dev/null || true
        plutil -replace CFBundleExecutable -string "cua-driver-local" \
            "$APP_STAGE/Contents/Info.plist"
        plutil -replace CFBundleIdentifier -string "com.meta.musecode.cua.driver.local" \
            "$APP_STAGE/Contents/Info.plist"
        plutil -replace CFBundleName -string "cua" \
            "$APP_STAGE/Contents/Info.plist"
        plutil -replace CFBundleDisplayName -string "cua" \
            "$APP_STAGE/Contents/Info.plist"
    fi
    # Sign the staged bundle before touching the live installation. Required on
    # macOS 26+ where Taskgated rejects a copied binary's stale signature.
    # Prefer the STABLE self-signed identity so TCC grants survive rebuilds;
    # never downgrade an existing certificate-signed installation to ad-hoc,
    # because that would invalidate its working TCC grants.
    if ! sign_staged_local_app "$APP_STAGE" "$APP_DEST"; then
        exit 1
    fi
    if ! verify_local_app_identity "$APP_STAGE" \
        "com.meta.musecode.cua.driver.local" "cua-driver-local"; then
        echo "${RED}Error: staged MuseCodeCuaDriverLocal.app failed identity verification; live installation was not changed.${NORMAL}" >&2
        exit 1
    fi
    STAGED_REQUIREMENT="$(designated_requirement "$APP_STAGE")"
    STAGED_SIGNING_CLASS="$(classify_designated_requirement "$STAGED_REQUIREMENT")"
    REQUIREMENT_COMPATIBILITY="first-install"
    if [ -n "$PREVIOUS_REQUIREMENT" ]; then
        REQUIREMENT_COMPATIBILITY="$(local_requirement_compatibility \
            "$PREVIOUS_REQUIREMENT" "$APP_STAGE")"
        if [ "$REQUIREMENT_COMPATIBILITY" = "unknown" ]; then
            echo "${RED}Error: could not evaluate the existing local app's signing requirement; refusing a TCC-unsafe replacement.${NORMAL}" >&2
            exit 1
        fi
    fi
    if ! stop_local_daemons_before_identity_check; then
        exit 1
    fi
    if [ "$REQUIREMENT_COMPATIBILITY" = "incompatible" ] \
       && ! refuse_local_history_identity_transition \
            "$LOCAL_HISTORY_ROOT" "$APP_DEST" \
            "replace the current local signer identity"; then
        exit 1
    fi
    if [ "$LEGACY_LOCAL_APP_OWNED" = "1" ] \
       && ! refuse_local_history_identity_transition \
            "$LOCAL_HISTORY_ROOT" "$LEGACY_LOCAL_APP" \
            "remove the legacy local app identity"; then
        exit 1
    fi

    # Re-check both the supervisor and exact process generations immediately
    # before the first bundle move. A failed/unloaded KeepAlive job must not
    # respawn into the gap between history preflight and signer replacement.
    if ! stop_local_daemons_before_identity_check; then
        exit 1
    fi
    if [ "$REQUIREMENT_COMPATIBILITY" = "incompatible" ] \
       && ! refuse_local_history_identity_transition \
            "$LOCAL_HISTORY_ROOT" "$APP_DEST" \
            "replace the current local signer identity"; then
        exit 1
    fi
    if [ "$LEGACY_LOCAL_APP_OWNED" = "1" ] \
       && ! refuse_local_history_identity_transition \
            "$LOCAL_HISTORY_ROOT" "$LEGACY_LOCAL_APP" \
            "remove the legacy local app identity"; then
        exit 1
    fi

    # Install to /Applications (user-writable for admins; no sudo — same as
    # install.sh). Keep the prior bundle available until the copy completes so
    # an interrupted install cannot leave a corrupt live app.
    APP_BACKUP="${APP_DEST}.install-backup.$$"
    if [ -e "$APP_BACKUP" ] || [ -L "$APP_BACKUP" ]; then
        echo "${RED}Error: refusing to overwrite existing install backup path $APP_BACKUP.${NORMAL}" >&2
        exit 1
    fi
    if [ -d "$APP_DEST" ]; then
        LOCAL_APP_HAD_PREVIOUS=1
    fi
    LOCAL_APP_BACKUP="$APP_BACKUP"
    LOCAL_APP_SWAP_STARTED=1
    if [ "$LOCAL_APP_HAD_PREVIOUS" = "1" ] \
       && ! mv "$APP_DEST" "$APP_BACKUP"; then
        LOCAL_APP_SWAP_STARTED=0
        echo "${RED}Error: could not move the authenticated local app into its rollback slot.${NORMAL}" >&2
        exit 1
    fi
    install_valid=false
    if ditto "$APP_STAGE" "$APP_DEST" \
       && verify_local_app_identity "$APP_DEST" \
            "com.meta.musecode.cua.driver.local" "cua-driver-local"; then
        INSTALLED_REQUIREMENT="$(designated_requirement "$APP_DEST")"
        INSTALLED_SIGNING_CLASS="$(classify_designated_requirement "$INSTALLED_REQUIREMENT")"
        if [ "$INSTALLED_REQUIREMENT" = "$STAGED_REQUIREMENT" ] \
           && [ "$INSTALLED_SIGNING_CLASS" = "$STAGED_SIGNING_CLASS" ] \
           && [ "$INSTALLED_SIGNING_CLASS" != "unknown" ]; then
            install_valid=true
        fi
    fi
    if [ "$install_valid" != true ]; then
        echo "${RED}Error: installed MuseCodeCuaDriverLocal.app did not preserve its verified signing identity; rolling back.${NORMAL}" >&2
        exit 1
    fi
    echo "${GREEN}installed $APP_DEST${NORMAL}"
    if [ "$INSTALLED_SIGNING_CLASS" = "certificate-backed" ]; then
        echo "${GREEN}verified installed designated requirement: certificate-backed (stable across rebuilds)${NORMAL}"
    else
        echo "${YELLOW}verified installed designated requirement: ad-hoc cdhash (changes on rebuild)${NORMAL}" >&2
    fi

    # --- Force LaunchServices registration of the freshly-copied bundle ----
    #
    # `ditto` drops the bundle on disk, but LaunchServices registers the new
    # com.meta.musecode.cua.driver.local identity ASYNCHRONOUSLY (seconds later). Until it
    # does, `open -n -g -a MuseCodeCuaDriverLocal` (what `permissions grant` / MCP use to
    # launch the daemon) fails with -1728. A synchronous `lsregister -f` closes
    # that race so both the reset and the first launch resolve the bundle id.
    if ! register_local_app "$APP_DEST"; then
        echo "${RED}Error: could not register MuseCodeCuaDriverLocal.app with LaunchServices; rolling back.${NORMAL}" >&2
        exit 1
    fi

    if [ -n "$PREVIOUS_REQUIREMENT" ]; then
        INSTALLED_COMPATIBILITY="$(local_requirement_compatibility \
            "$PREVIOUS_REQUIREMENT" "$APP_DEST")"
        if [ "$INSTALLED_COMPATIBILITY" != "$REQUIREMENT_COMPATIBILITY" ]; then
            echo "${RED}Error: installed local app's signing compatibility changed during copy; rolling back.${NORMAL}" >&2
            exit 1
        fi
    else
        INSTALLED_COMPATIBILITY="first-install"
    fi

fi

# --- Visible-bin symlink ------------------------------------------------
#
# On macOS point at the binary INSIDE the installed bundle so the process
# that actually runs carries the com.meta.musecode.cua.driver.local identity (TCC keys
# grants on it). On Linux/Windows point at the versioned-store binary.
mkdir -p "$BIN_DIR"
if [ "$OS" = "Darwin" ]; then
    BIN_TARGET="$APP_DEST/Contents/MacOS/cua-driver-local"
else
    BIN_TARGET="$CURRENT_LINK/cua-driver-local"
fi
ln -sf "$BIN_TARGET" "$BIN_DIR/cua-driver-local"
echo "${GREEN}$BIN_DIR/cua-driver-local -> $BIN_TARGET${NORMAL}"
echo ""

INSTALLED_BIN="$BIN_DIR/cua-driver-local"

# --- Stop any pre-swap cua-driver daemons ------------------------------
#
# Re-check after publishing the new binary. The pre-stage Linux shutdown above
# prevents replacement under an active Restart unit; this second pass catches
# an independently started process before optional autostart is re-enabled.
#
if [ "$OS" = "Darwin" ]; then
    if ! stop_local_daemons_before_identity_check; then
        exit 1
    fi
elif [ "$OS" = "Linux" ]; then
    if ! stop_local_linux_daemons_before_runtime_change; then
        exit 1
    fi
fi

# An incompatible signing transition leaves the old csreq attached to this
# bundle's TCC rows. Once the new bundle is registered and old daemons are
# stopped, reset all services used by the local driver.
if [ "$OS" = "Darwin" ]; then
    if ! reset_local_tcc_after_requirement_change "$INSTALLED_COMPATIBILITY"; then
        exit 1
    fi

    # Retire the legacy identity transactionally before the replacement becomes
    # committed or any KeepAlive job can launch it. The authenticated old app
    # stays in a rollback slot until both identities have completed their TCC
    # and LaunchServices transitions.
    if [ "$LEGACY_LOCAL_APP_OWNED" = "1" ]; then
        if ! stop_local_daemons_before_identity_check; then
            exit 1
        fi
        if ! refuse_local_history_identity_transition \
            "$LOCAL_HISTORY_ROOT" "$LEGACY_LOCAL_APP" \
            "remove the legacy local app identity"; then
            exit 1
        fi
        LEGACY_LOCAL_APP_BACKUP="${LEGACY_LOCAL_APP}.install-backup.$$"
        if [ -e "$LEGACY_LOCAL_APP_BACKUP" ] || [ -L "$LEGACY_LOCAL_APP_BACKUP" ]; then
            echo "${RED}Error: refusing to overwrite legacy local-app backup path $LEGACY_LOCAL_APP_BACKUP.${NORMAL}" >&2
            exit 1
        fi
        LEGACY_LOCAL_APP_REMOVAL_STARTED=1
        if ! prepare_legacy_local_app_removal "$LEGACY_LOCAL_APP" 1; then
            exit 1
        fi
        if ! mv "$LEGACY_LOCAL_APP" "$LEGACY_LOCAL_APP_BACKUP"; then
            echo "${RED}Error: could not move the authenticated legacy local app into its rollback slot.${NORMAL}" >&2
            exit 1
        fi
        echo "${YELLOW}staged retired local app $LEGACY_LOCAL_APP for removal${NORMAL}" >&2
    fi

    LOCAL_APP_INSTALL_COMMITTED=1
    if ! remove_authenticated_local_app_backup "$APP_BACKUP" \
        "com.meta.musecode.cua.driver.local" "cua-driver-local"; then
        exit 1
    fi
    if [ "$LEGACY_LOCAL_APP_REMOVAL_STARTED" = "1" ] \
       && ! remove_authenticated_local_app_backup "$LEGACY_LOCAL_APP_BACKUP" \
            "com.trycua.driver.local" "cua-driver-local"; then
        exit 1
    fi
    LOCAL_APP_SWAP_STARTED=0
    LEGACY_LOCAL_APP_REMOVAL_STARTED=0
fi

# Agent skill pack symlinks: NOT auto-created. Run
# `cua-driver skills install --local` to symlink agent dirs to the
# staged copy at $VERSIONED_DIR/Skills/cua-driver above.
echo ""

# --- Autostart (optional) ----------------------------------------------

if [ "$INSTALL_AUTOSTART" = true ]; then
    if [ "$OS" = "Darwin" ]; then
        PLIST_PATH="$HOME/Library/LaunchAgents/com.trycua.cua-driver-local.plist"
        echo "${BOLD}Writing LaunchAgent → $PLIST_PATH${NORMAL}"
        mkdir -p "$(dirname "$PLIST_PATH")"
        cat >"$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.trycua.cua-driver-local</string>
  <key>ProgramArguments</key>
  <array>
    <string>$INSTALLED_BIN</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME_DIR/serve.out.log</string>
  <key>StandardErrorPath</key><string>$HOME_DIR/serve.err.log</string>
</dict>
</plist>
EOF
        launchctl unload "$PLIST_PATH" 2>/dev/null || true
        launchctl load "$PLIST_PATH"
        echo "${GREEN}Loaded.${NORMAL} Manage with launchctl load / unload \"$PLIST_PATH\"."
    elif [ "$OS" = "Linux" ]; then
        UNIT_PATH="$HOME/.config/systemd/user/cua-driver-local.service"
        echo "${BOLD}Writing systemd user unit → $UNIT_PATH${NORMAL}"
        mkdir -p "$(dirname "$UNIT_PATH")"
        cat >"$UNIT_PATH" <<EOF
[Unit]
Description=cua-driver-local serve daemon
After=graphical-session.target

[Service]
ExecStart=$INSTALLED_BIN serve
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
        systemctl --user daemon-reload
        systemctl --user enable --now cua-driver-local.service
        echo "${GREEN}Enabled.${NORMAL} Manage with systemctl --user {start|stop|status} cua-driver-local."
    fi
    echo ""
fi

# --- Done ---------------------------------------------------------------

echo "${BOLD}${GREEN}Installed.${NORMAL}"
echo "  ${BOLD}$INSTALLED_BIN${NORMAL}"
echo ""

# Unified post-install hints come from a single shared text file so the
# 4 Rust installers (this script + install-local.ps1 + _install-rust.sh +
# install.ps1) never drift. The .txt holds the OS-agnostic bulk
# (Try-it / skill pack / MCP setup / docs link) with {{BINARY}}
# placeholders; OS-specific bits stay inline below.
HINTS_TXT="$SCRIPT_DIR/post-install-hints.txt"
if [ -f "$HINTS_TXT" ]; then
    sed "s|{{BINARY}}|$INSTALLED_BIN|g" "$HINTS_TXT"
else
    # Repo layout changed or running from an unexpected location — fall
    # back to one-line essentials so users still know what to do next.
    echo "Next steps: $INSTALLED_BIN --version  |  $INSTALLED_BIN mcp-config  |  $INSTALLED_BIN skills install"
    echo "Docs: https://github.com/trycua/cua/tree/main/libs/cua-driver/rust"
fi

# The local/release identity split deliberately stopped source installs from
# creating or repairing the published `cua-driver` name. Make the resulting
# migration state explicit when only the local product is present: otherwise
# an existing MCP client can keep launching a now-missing release path even
# though this install completed successfully. Do not create a compatibility
# symlink here; that would collapse the separate product identities again.
RELEASE_BIN="$BIN_DIR/cua-driver"
if [ ! -e "$RELEASE_BIN" ]; then
    echo ""
    echo "${YELLOW}Migration note: the published cua-driver CLI is not installed at $RELEASE_BIN.${NORMAL}" >&2
    echo "  Existing MCP clients configured for 'cua-driver' will not use this local build." >&2
    echo "  To configure Codex for the local build, run:" >&2
    echo "    $INSTALLED_BIN mcp-config --client codex" >&2
    echo "  To restore the published product instead, run:" >&2
    echo '    /bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"' >&2
fi

# OS-specific autostart hint (kept inline; per-shell natural location).
if [ "$INSTALL_AUTOSTART" != true ]; then
    echo ""
    if [ "$OS" = "Darwin" ]; then
        echo "Auto-start (recommended on macOS): re-run with --autostart to register a LaunchAgent."
        echo "  A launchd-started daemon is attributed to com.meta.musecode.cua.driver.local (not your terminal),"
        echo "  so permission prompts say \"cua\" and grants stick — grant Accessibility +"
        echo "  Screen Recording once and every cua-driver-local call/mcp routes through it correctly."
        echo "  (Without it, a prompt raised from a terminal attributes to the terminal instead.)"
    else
        echo "Auto-start (optional): re-run with --autostart to register a systemd user unit."
    fi
    echo ""
fi
