#!/usr/bin/env bash
# Remove only the source-built cua-driver-local product. The released
# cua-driver installation has different names and paths and is never touched.
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source-path=SCRIPTDIR
# shellcheck source=_local-signing.sh
. "$SCRIPT_DIR/_local-signing.sh"

RESET_TCC=1
FORCE=0
VALIDATE_ONLY=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --keep-tcc) RESET_TCC=0 ;;
        --reset-tcc) RESET_TCC=1 ;;
        --force) FORCE=1 ;;
        --validate-only) VALIDATE_ONLY=1 ;;
        --help|-h)
            echo "Usage: $0 [--force] [--keep-tcc] [--validate-only]"
            exit 0
            ;;
        *) echo "error: unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

OS="$(uname -s 2>/dev/null || echo unknown)"
HOME_DIR="${CUA_DRIVER_LOCAL_HOME:-$HOME/.cua-driver-local}"
BIN_DIR="${CUA_DRIVER_LOCAL_INSTALL_DIR:-$HOME/.local/bin}"
CLI_LINK="$BIN_DIR/cua-driver-local"
APP_BUNDLE="/Applications/MuseCodeCuaDriverLocal.app"
LEGACY_APP_BUNDLE="/Applications/CuaDriverLocal.app"
if [[ "$OS" == "Darwin" ]]; then
    CACHE_DIR="$HOME/Library/Caches/cua-driver-local"
    LOCAL_HISTORY_ROOT="$HOME/Library/Application Support/cua-driver-local/computer-history"
else
    CACHE_DIR="$HOME/.cache/cua-driver-local"
    LOCAL_HISTORY_ROOT=""
fi
LAUNCHAGENT="$HOME/Library/LaunchAgents/com.trycua.cua-driver-local.plist"
SYSTEMD_UNIT="$HOME/.config/systemd/user/cua-driver-local.service"

log() { printf '==> %s\n' "$*"; }

remove_and_verify_local_systemd_service() {
    local service="$1" unit_path="$2"
    local active_state=0 enabled_state=0

    disable_and_verify_local_systemd_service "$service" "$unit_path" || return 1
    if [[ -e "$unit_path" || -L "$unit_path" ]]; then
        if ! rm -f -- "$unit_path"; then
            echo "error: could not remove systemd user unit $unit_path; runtime was preserved" >&2
            return 1
        fi
    fi
    if [[ -e "$unit_path" || -L "$unit_path" ]]; then
        echo "error: systemd user unit $unit_path remains after removal; runtime was preserved" >&2
        return 1
    fi
    if command -v systemctl >/dev/null 2>&1; then
        if ! systemctl --user daemon-reload >/dev/null 2>&1; then
            echo "error: systemd user manager reload failed; runtime was preserved" >&2
            return 1
        fi
        active_state=0
        local_systemd_active_state "$service" || active_state=$?
        enabled_state=0
        local_systemd_enabled_state "$service" || enabled_state=$?
        if [[ "$active_state" != "1" || "$enabled_state" != "1" ]]; then
            echo "error: local systemd service $service reappeared or remains enabled after cleanup; runtime was preserved" >&2
            return 1
        fi
    fi
}

CURRENT_LOCAL_BUNDLE_ID="com.meta.musecode.cua.driver.local"
LEGACY_LOCAL_BUNDLE_ID="com.trycua.driver.local"
LOCAL_EXECUTABLE="cua-driver-local"
PLISTBUDDY="/usr/libexec/PlistBuddy"
CODESIGN="/usr/bin/codesign"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
TCCUTIL="/usr/bin/tccutil"

reject_local_root_invocation() {
    local effective_uid="$1" sudo_uid="${2:-}"
    if [[ "$effective_uid" == "0" || -n "$sudo_uid" ]]; then
        printf 'error: do not run the local Cua Driver uninstaller as root or with sudo; run it as the login user\n' >&2
        return 77
    fi
}

validate_local_home_dir() {
    local home_dir="$1" user_home="$2" resolved_home resolved_dir
    case "$home_dir" in
        /*) ;;
        *) echo "error: CUA_DRIVER_LOCAL_HOME must be an absolute path" >&2; return 1 ;;
    esac
    case "$home_dir" in
        /|"$user_home"|"$user_home"/|"$user_home/.cua-driver"|*/../*|*/..|*/./*|*/.)
            echo "error: refusing unsafe or release-owned local home: $home_dir" >&2
            return 1
            ;;
    esac
    [[ "$home_dir" != *$'\n'* && "$home_dir" != *$'\r'* ]] || {
        echo "error: refusing local home containing a line break" >&2
        return 1
    }
    resolved_home="$(CDPATH= cd -- "$user_home" 2>/dev/null && pwd -P)" || return 1
    case "$home_dir" in
        "$user_home"/*) ;;
        *) echo "error: CUA_DRIVER_LOCAL_HOME must remain inside HOME ($resolved_home): $home_dir" >&2; return 1 ;;
    esac
    if [[ -e "$home_dir" || -L "$home_dir" ]]; then
        [[ ! -L "$home_dir" && -d "$home_dir" ]] || {
            echo "error: refusing non-directory or symlink local home: $home_dir" >&2
            return 1
        }
        resolved_dir="$(CDPATH= cd -- "$home_dir" 2>/dev/null && pwd -P)" || return 1
        case "$resolved_dir" in
            "$resolved_home"/*) ;;
            *) echo "error: local home resolves outside HOME: $resolved_dir" >&2; return 1 ;;
        esac
    fi
}

local_plist_value() {
    local plistbuddy="$1" app_bundle="$2" key="$3"
    [[ -x "$plistbuddy" ]] || return 1
    "$plistbuddy" -c "Print :$key" "$app_bundle/Contents/Info.plist" 2>/dev/null
}

# Local builds can be signed either ad-hoc or by the dedicated development
# certificate. In both cases verify the signature-derived identifier and
# designated requirement in addition to the mutable Info.plist fields.
local_app_is_owned() {
    local app_bundle="$1" expected_bundle_id="$2" codesign_tool="$3" plistbuddy="$4"
    local actual_bundle_id actual_executable executable_path requirement

    [[ -d "$app_bundle" && ! -L "$app_bundle" ]] || return 1
    actual_bundle_id="$(local_plist_value "$plistbuddy" "$app_bundle" CFBundleIdentifier || true)"
    actual_executable="$(local_plist_value "$plistbuddy" "$app_bundle" CFBundleExecutable || true)"
    [[ "$actual_bundle_id" == "$expected_bundle_id" && "$actual_executable" == "$LOCAL_EXECUTABLE" ]] || return 1
    executable_path="$app_bundle/Contents/MacOS/$LOCAL_EXECUTABLE"
    [[ -f "$executable_path" && -x "$executable_path" && ! -L "$executable_path" ]] || return 1
    [[ -x "$codesign_tool" ]] || return 1
    "$codesign_tool" --verify --deep --strict "$app_bundle" >/dev/null 2>&1 || return 1
    "$codesign_tool" --verify --deep --strict \
        -R "=identifier \"$expected_bundle_id\"" "$app_bundle" >/dev/null 2>&1 \
        || return 1
    requirement="$("$codesign_tool" -d -r- "$app_bundle" 2>&1 \
        | sed -n -e 's/^designated => //p' -e 's/^# designated => //p')" || return 1
    [[ -n "$requirement" && "$requirement" == *"identifier \"$expected_bundle_id\""* ]] || return 1
    [[ "$requirement" == *cdhash* || "$requirement" == *"certificate leaf"* ]]
}

prepare_local_app_removal() {
    local app_bundle="$1" bundle_id="$2" failed_services="" service
    [[ -x "$LSREGISTER" ]] || {
        echo "error: LaunchServices registration tool is unavailable; preserved $app_bundle" >&2
        return 1
    }
    "$LSREGISTER" -f "$app_bundle" >/dev/null 2>&1 || {
        echo "error: could not register $app_bundle before permission cleanup; the app was preserved" >&2
        return 1
    }
    if [[ "$RESET_TCC" == "1" ]]; then
        [[ -x "$TCCUTIL" ]] || {
            echo "error: tccutil is required to revoke permissions for $bundle_id; the app was preserved" >&2
            return 1
        }
        for service in Accessibility ScreenCapture AppleEvents; do
            "$TCCUTIL" reset "$service" "$bundle_id" >/dev/null 2>&1 \
                || failed_services="$failed_services $service"
        done
        if [[ -n "$failed_services" ]]; then
            echo "error: could not reset these TCC services for $bundle_id:$failed_services; the app was preserved" >&2
            return 1
        fi
        log "revoked TCC grants for $bundle_id"
    else
        log "preserving TCC grants for $bundle_id (--keep-tcc)"
    fi
    "$LSREGISTER" -u "$app_bundle" >/dev/null 2>&1 || {
        echo "error: could not unregister $app_bundle from LaunchServices; the app was preserved" >&2
        return 1
    }
}

remove_verified_local_app() {
    local app_bundle="$1" bundle_id="$2"
    local_app_is_owned "$app_bundle" "$bundle_id" "$CODESIGN" "$PLISTBUDDY" || {
        echo "error: app identity changed during cleanup; preserved $app_bundle" >&2
        return 1
    }
    remove_legacy_local_app_path "$app_bundle"
}

if ! validate_local_home_dir "$HOME_DIR" "$HOME"; then
    exit 2
fi
case "$BIN_DIR" in
    /*) ;;
    *) echo "error: CUA_DRIVER_LOCAL_INSTALL_DIR must be an absolute path" >&2; exit 2 ;;
esac

if [[ "$VALIDATE_ONLY" == "1" ]]; then
    printf 'cli=%s\nhome=%s\ncache=%s\napp=%s\nbundle=com.meta.musecode.cua.driver.local\nlaunchagent=%s\nsystemd=%s\n' \
        "$CLI_LINK" "$HOME_DIR" "$CACHE_DIR" "$APP_BUNDLE" "$LAUNCHAGENT" "$SYSTEMD_UNIT"
    exit 0
fi

if ! reject_local_root_invocation "$(id -u)" "${SUDO_UID:-}"; then
    exit 77
fi

CURRENT_LOCAL_APP_OWNED=0
LEGACY_LOCAL_APP_OWNED=0
if [[ "$OS" == "Darwin" && ( -e "$APP_BUNDLE" || -L "$APP_BUNDLE" ) ]]; then
    if local_app_is_owned "$APP_BUNDLE" "$CURRENT_LOCAL_BUNDLE_ID" "$CODESIGN" "$PLISTBUDDY"; then
        CURRENT_LOCAL_APP_OWNED=1
    else
        echo "error: refusing to stop or remove unverified local app $APP_BUNDLE" >&2
        exit 1
    fi
fi
if [[ "$OS" == "Darwin" && ( -e "$LEGACY_APP_BUNDLE" || -L "$LEGACY_APP_BUNDLE" ) ]]; then
    if local_app_is_owned "$LEGACY_APP_BUNDLE" "$LEGACY_LOCAL_BUNDLE_ID" "$CODESIGN" "$PLISTBUDDY"; then
        LEGACY_LOCAL_APP_OWNED=1
    else
        echo "error: refusing to stop or remove unverified legacy local app $LEGACY_APP_BUNDLE" >&2
        exit 1
    fi
fi

if [[ "$FORCE" != "1" ]]; then
    printf 'Remove the local cua-driver identity and its state? [y/N] '
    read -r reply
    case "$reply" in y|Y|yes|YES) ;; *) log "cancelled"; exit 0 ;; esac
fi

resolve_link() {
    local link="$1" target
    [[ -L "$link" ]] || { printf ''; return; }
    if target="$(realpath "$link" 2>/dev/null)"; then
        printf '%s' "$target"
        return
    fi
    target="$(readlink "$link" 2>/dev/null || true)"
    case "$target" in
        /*) printf '%s' "$target" ;;
        *) printf '%s/%s' "$(cd -- "$(dirname -- "$link")" && pwd)" "$target" ;;
    esac
}

is_local_target() {
    case "$1" in
        "$HOME_DIR"/*|"$APP_BUNDLE"/*) return 0 ;;
        "$LEGACY_APP_BUNDLE"/*) [[ "$LEGACY_LOCAL_APP_OWNED" == "1" ]] ;;
        *) return 1 ;;
    esac
}

local_daemon_pid_alive() {
    kill -0 "$1" 2>/dev/null
}

local_daemon_process_generation() {
    local pid="$1" start=""
    command -v ps >/dev/null 2>&1 || return 2
    start="$(LC_ALL=C ps -ww -o lstart= -p "$pid" 2>/dev/null)" || return 2
    start="${start#"${start%%[![:space:]]*}"}"
    start="${start%"${start##*[![:space:]]}"}"
    [[ -n "$start" ]] || return 2
    printf '%s' "$start"
}

local_daemon_process_identity() {
    local_owned_process_identity "$1"
}

local_daemon_identity_matches_install() {
    local identity="$1"
    if [[ "${CLI_LINK_OWNED:-0}" == "1" ]]; then
        case "$identity" in
            "$CLI_LINK"|"$CLI_LINK"[[:space:]]*) return 0 ;;
        esac
    fi
    case "$identity" in
        "$HOME_DIR"/packages/current/cua-driver-local|"$HOME_DIR"/packages/current/cua-driver-local[[:space:]]*|\
        "$HOME_DIR"/packages/releases/*/cua-driver-local|"$HOME_DIR"/packages/releases/*/cua-driver-local[[:space:]]*) return 0 ;;
    esac
    if [[ "$CURRENT_LOCAL_APP_OWNED" == "1" ]]; then
        case "$identity" in
            "$APP_BUNDLE"/Contents/MacOS/cua-driver-local|"$APP_BUNDLE"/Contents/MacOS/cua-driver-local[[:space:]]*) return 0 ;;
        esac
    fi
    if [[ "$LEGACY_LOCAL_APP_OWNED" == "1" ]]; then
        case "$identity" in
            "$LEGACY_APP_BUNDLE"/Contents/MacOS/cua-driver-local|"$LEGACY_APP_BUNDLE"/Contents/MacOS/cua-driver-local[[:space:]]*) return 0 ;;
        esac
    fi
    return 1
}

local_daemon_pid_matches_generation() {
    local pid="$1" expected_generation="$2" current_generation="" identity=""
    local_daemon_pid_alive "$pid" || return 1
    current_generation="$(local_daemon_process_generation "$pid")" || return 2
    [[ "$current_generation" == "$expected_generation" ]] || return 1
    identity="$(local_daemon_process_identity "$pid")" || return 2
    local_daemon_identity_matches_install "$identity" || return 1
}

local_daemon_signal_if_current() {
    local pid="$1" generation="$2" signal="$3" status=0
    local_daemon_pid_matches_generation "$pid" "$generation" || status=$?
    [[ "$status" != "2" ]] || return 2
    [[ "$status" == "0" ]] || return 1
    kill -"$signal" "$pid" 2>/dev/null || return 1
}

local_daemon_wait_for_generation_exit() {
    local pid="$1" generation="$2" attempts=0 status=0
    while :; do
        status=0
        local_daemon_pid_matches_generation "$pid" "$generation" || status=$?
        case "$status" in
            0) ;;
            1) return 0 ;;
            *) return 2 ;;
        esac
        [[ "$attempts" -lt 20 ]] || return 1
        sleep 0.1 2>/dev/null || sleep 1 || true
        attempts=$((attempts + 1))
    done
}

local_daemon_candidate_pids() {
    local uid="" candidates="" pgrep_status=0
    command -v pgrep >/dev/null 2>&1 || return 2
    uid="$(id -u 2>/dev/null || true)"
    [[ "$uid" =~ ^[0-9]+$ ]] || return 2
    candidates="$(pgrep -U "$uid" -f '(^|[[:space:]/])cua-driver-local([[:space:]]|$)' 2>/dev/null)" \
        || pgrep_status=$?
    case "$pgrep_status" in
        0) printf '%s\n' "$candidates" ;;
        1) return 0 ;;
        *) return 2 ;;
    esac
}

verified_local_daemon_records() {
    local candidates="" candidate_status=0 pid identity="" generation=""
    candidates="$(local_daemon_candidate_pids)" || candidate_status=$?
    [[ "$candidate_status" == "0" ]] || return 2
    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 2
        local_daemon_pid_alive "$pid" || continue
        identity="$(local_daemon_process_identity "$pid")" || {
            local_daemon_pid_alive "$pid" || continue
            return 2
        }
        local_daemon_identity_matches_install "$identity" || continue
        generation="$(local_daemon_process_generation "$pid")" || {
            local_daemon_pid_alive "$pid" || continue
            return 2
        }
        printf '%s\t%s\n' "$pid" "$generation"
    done <<< "$candidates"
}

verify_local_daemons_absent() {
    local records="" status=0
    records="$(verified_local_daemon_records)" || status=$?
    if [[ "$status" != "0" ]]; then
        echo "error: process inspection failed while verifying local daemon shutdown; no permission or app cleanup was attempted" >&2
        return 1
    fi
    if [[ -n "$records" ]]; then
        echo "error: a verified local cua-driver process remains; no permission or app cleanup was attempted" >&2
        return 1
    fi
}

stop_verified_local_daemons() {
    local records="" status=0 record pid generation wait_status
    records="$(verified_local_daemon_records)" || status=$?
    if [[ "$status" != "0" ]]; then
        echo "error: process inspection failed before local daemon shutdown; no permission or app cleanup was attempted" >&2
        return 1
    fi

    while IFS=$'\t' read -r pid generation; do
        [[ -n "$pid" ]] || continue
        status=0
        local_daemon_signal_if_current "$pid" "$generation" TERM || status=$?
        [[ "$status" != "2" ]] || {
            echo "error: process inspection failed before signalling local daemon pid $pid" >&2
            return 1
        }
        [[ "$status" == "0" ]] || continue
        wait_status=0
        local_daemon_wait_for_generation_exit "$pid" "$generation" || wait_status=$?
        [[ "$wait_status" != "2" ]] || {
            echo "error: process inspection failed while waiting for local daemon pid $pid" >&2
            return 1
        }
        if [[ "$wait_status" == "1" ]]; then
            status=0
            local_daemon_signal_if_current "$pid" "$generation" KILL || status=$?
            [[ "$status" != "2" ]] || {
                echo "error: process inspection failed before killing local daemon pid $pid" >&2
                return 1
            }
            if [[ "$status" == "0" ]]; then
                wait_status=0
                local_daemon_wait_for_generation_exit "$pid" "$generation" || wait_status=$?
                [[ "$wait_status" == "0" ]] || {
                    echo "error: verified local daemon pid $pid survived TERM and KILL" >&2
                    return 1
                }
            fi
        fi
    done <<< "$records"

    verify_local_daemons_absent
}

# Resolve ownership and guard encrypted history before changing supervisor,
# permission, app, or runtime state.
CLI_LINK_OWNED=0
if [[ -L "$CLI_LINK" ]]; then
    target="$(resolve_link "$CLI_LINK")"
    if is_local_target "$target"; then
        CLI_LINK_OWNED=1
    fi
fi
if [[ "$OS" == "Darwin" && "$LEGACY_LOCAL_APP_OWNED" == "1" ]]; then
    if ! refuse_local_history_identity_transition "$LOCAL_HISTORY_ROOT" \
        "$LEGACY_APP_BUNDLE" "remove the legacy local app signer identity"; then
        exit 1
    fi
fi
if [[ "$OS" == "Darwin" && "$CURRENT_LOCAL_APP_OWNED" == "1" ]]; then
    if ! refuse_local_history_identity_transition "$LOCAL_HISTORY_ROOT" \
        "$APP_BUNDLE" "remove the current local app signer identity"; then
        exit 1
    fi
fi

# Stop only local autostart/process identities. A failed unload/disable must not
# be hidden: KeepAlive/Restart can otherwise repopulate the process set after a
# successful snapshot scan and before history or app removal.
if [[ "$OS" == "Darwin" ]]; then
    if ! stop_and_verify_local_launchagent \
        "$LAUNCHAGENT" "com.trycua.cua-driver-local"; then
        exit 1
    fi
    if [[ -f "$LAUNCHAGENT" ]]; then
        rm -f "$LAUNCHAGENT"
        log "removed LaunchAgent $LAUNCHAGENT"
    fi
elif [[ "$OS" == "Linux" ]]; then
    if ! remove_and_verify_local_systemd_service \
        "cua-driver-local.service" "$SYSTEMD_UNIT"; then
        exit 1
    fi
    [[ ! -e "$SYSTEMD_UNIT" ]] \
        && log "stopped and removed systemd user unit $SYSTEMD_UNIT"
fi
# Enumerate candidates by name, authenticate each command path against the
# verified local installation, capture process generation, and re-check before
# every signal. Inspection failures or survivors stop uninstall before TCC or
# app deletion.
if ! stop_verified_local_daemons; then
    exit 1
fi
# Verify the supervisor again after process termination, then perform one final
# exact-generation scan. This makes a supervised respawn a hard failure before
# the second history gate and all TCC/app mutations.
if [[ "$OS" == "Darwin" ]]; then
    if ! stop_and_verify_local_launchagent \
        "$LAUNCHAGENT" "com.trycua.cua-driver-local" \
       || ! verify_local_daemons_absent; then
        exit 1
    fi
elif [[ "$OS" == "Linux" ]]; then
    if ! stop_and_verify_local_systemd_service \
        "cua-driver-local.service" "$SYSTEMD_UNIT" \
       || ! verify_local_daemons_absent; then
        exit 1
    fi
fi
# Repeat the history gate only after every verified daemon has stopped. This
# closes the race where a live writer creates a new chunk after the preflight
# check but before its signer identity is removed.
if [[ "$OS" == "Darwin" && "$LEGACY_LOCAL_APP_OWNED" == "1" ]]; then
    if ! refuse_local_history_identity_transition "$LOCAL_HISTORY_ROOT" \
        "$LEGACY_APP_BUNDLE" "remove the legacy local app signer identity"; then
        exit 1
    fi
fi
if [[ "$OS" == "Darwin" && "$CURRENT_LOCAL_APP_OWNED" == "1" ]]; then
    if ! refuse_local_history_identity_transition "$LOCAL_HISTORY_ROOT" \
        "$APP_BUNDLE" "remove the current local app signer identity"; then
        exit 1
    fi
fi

# Reset and unregister each verified identity while its app remains present.
if [[ "$OS" == "Darwin" && "$LEGACY_LOCAL_APP_OWNED" == "1" ]]; then
    if ! prepare_local_app_removal "$LEGACY_APP_BUNDLE" "$LEGACY_LOCAL_BUNDLE_ID" \
       || ! remove_verified_local_app "$LEGACY_APP_BUNDLE" "$LEGACY_LOCAL_BUNDLE_ID"; then
        exit 1
    fi
    log "removed retired local app $LEGACY_APP_BUNDLE ($LEGACY_LOCAL_BUNDLE_ID)"
fi
if [[ "$OS" == "Darwin" && "$CURRENT_LOCAL_APP_OWNED" == "1" ]]; then
    if ! prepare_local_app_removal "$APP_BUNDLE" "$CURRENT_LOCAL_BUNDLE_ID" \
       || ! remove_verified_local_app "$APP_BUNDLE" "$CURRENT_LOCAL_BUNDLE_ID"; then
        exit 1
    fi
    log "removed $APP_BUNDLE"
fi

# Remove the CLI only if it is an installer-created link into the local product.
if [[ -L "$CLI_LINK" ]]; then
    target="$(resolve_link "$CLI_LINK")"
    if is_local_target "$target"; then
        rm -f "$CLI_LINK"
        log "removed $CLI_LINK"
    else
        log "$CLI_LINK points outside the local install; leaving it"
    fi
elif [[ -e "$CLI_LINK" ]]; then
    log "$CLI_LINK is not a symlink; leaving it"
fi

# Skill links use the shared pack name, so target ownership is mandatory.
for skill_link in \
    "$HOME/.claude/skills/cua-driver" \
    "$HOME/.agents/skills/cua-driver" \
    "$HOME/.openclaw/skills/cua-driver" \
    "$HOME/.config/opencode/skills/cua-driver" \
    "$HOME/.gemini/skills/cua-driver" \
    "$HOME/.hermes/skills/cua-driver"; do
    if [[ -L "$skill_link" ]]; then
        target="$(resolve_link "$skill_link")"
        if is_local_target "$target"; then
            rm -f "$skill_link"
            log "removed local skill link $skill_link"
        fi
    fi
done

# Scrub Claude registrations only when their command/args point at local paths.
CLAUDE_JSON="$HOME/.claude.json"
if [[ -f "$CLAUDE_JSON" ]] && command -v python3 >/dev/null 2>&1; then
    CLAUDE_JSON="$CLAUDE_JSON" LOCAL_HOME="$HOME_DIR" LOCAL_APP="$APP_BUNDLE" python3 <<'PY'
import json, os, shutil, tempfile, time

path = os.environ["CLAUDE_JSON"]
try:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
except (OSError, ValueError):
    raise SystemExit(0)

anchors = ("cua-driver-local", os.environ["LOCAL_HOME"], os.environ["LOCAL_APP"])
removed = []

def is_local(server):
    if not isinstance(server, dict):
        return False
    values = [server.get("command", "")]
    args = server.get("args", [])
    values.extend(args if isinstance(args, list) else [args])
    joined = " ".join(value for value in values if isinstance(value, str))
    return any(anchor and anchor in joined for anchor in anchors)

def scrub(servers, scope):
    if not isinstance(servers, dict):
        return
    for name in list(servers):
        if is_local(servers[name]):
            del servers[name]
            removed.append(f"{scope}:{name}")

scrub(data.get("mcpServers"), "user")
for project_name, project in (data.get("projects") or {}).items():
    if isinstance(project, dict):
        scrub(project.get("mcpServers"), f"project:{project_name}")

if removed:
    backup = f"{path}.bak-cua-driver-local-uninstall-{int(time.time())}"
    shutil.copy2(path, backup)
    fd, temporary = tempfile.mkstemp(prefix=".claude.json.", dir=os.path.dirname(path) or ".", text=True)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(data, handle, indent=2, ensure_ascii=False)
        handle.write("\n")
    os.replace(temporary, path)
    print(f"==> removed local Claude MCP registration(s): {', '.join(removed)}")
PY
fi

# Exact local-only directories. The release app, state, cache and services use
# names without the -local suffix and cannot be reached by these paths.
[[ ! -d "$CACHE_DIR" ]] || { rm -rf "$CACHE_DIR"; log "removed $CACHE_DIR"; }

# A caller may override CUA_DRIVER_LOCAL_HOME, so never recursively delete that
# root. Require the installer's local executable marker, remove only known
# runtime-owned children, and leave any unrelated files in place.
LOCAL_HOME_MARKER="$HOME_DIR/packages/current/cua-driver-local"
if [[ -e "$LOCAL_HOME_MARKER" || -L "$LOCAL_HOME_MARKER" ]]; then
    rm -rf "$HOME_DIR/packages" "$HOME_DIR/skills"
    rm -f \
        "$HOME_DIR/.installation_recorded" \
        "$HOME_DIR/.telemetry_enabled" \
        "$HOME_DIR/.telemetry_id" \
        "$HOME_DIR/.telemetry_identity.lock" \
        "$HOME_DIR/.telemetry_install_channel" \
        "$HOME_DIR/.telemetry_lifecycle.lock" \
        "$HOME_DIR/.telemetry_retry_after" \
        "$HOME_DIR/.tcc-signing-identity" \
        "$HOME_DIR/config.json" \
        "$HOME_DIR/serve.err.log" \
        "$HOME_DIR/serve.out.log" \
        "$HOME_DIR/version_check.json"
    if rmdir "$HOME_DIR" 2>/dev/null; then
        log "removed empty local home $HOME_DIR"
    else
        log "removed local runtime payloads from $HOME_DIR; preserved unrelated files"
    fi
elif [[ -d "$HOME_DIR" ]]; then
    log "$HOME_DIR has no cua-driver-local install marker; leaving it untouched"
fi
log "cua-driver-local uninstalled; release cua-driver was left untouched"
