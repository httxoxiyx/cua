#!/usr/bin/env bash
#
# macOS local-development signing helpers for _install-local-rust.sh.
# This file has no top-level side effects so its policy can be exercised by
# focused shell tests without building or installing cua-driver.

CUA_LOCAL_SIGN_CN="CuaDriver Local Signing (cua-driver-rs)"

escape_extended_regex() {
    printf '%s' "$1" | sed 's/[][\\.^$*+?(){}|]/\\&/g'
}

local_owned_process_alive() {
    kill -0 "$1" 2>/dev/null
}

local_owned_process_generation() {
    local pid="$1" start=""
    command -v ps >/dev/null 2>&1 || return 2
    start="$(LC_ALL=C ps -ww -o lstart= -p "$pid" 2>/dev/null)" || return 2
    start="${start#"${start%%[![:space:]]*}"}"
    start="${start%"${start##*[![:space:]]}"}"
    [ -n "$start" ] || return 2
    printf '%s' "$start"
}

# Prefer the kernel executable link where available. On macOS, `ps comm`
# reports the executable identity without attacker-controlled argv text.
local_owned_process_identity() {
    local pid="$1" identity="" lsof_tool="${CUA_DRIVER_LSOF:-/usr/sbin/lsof}"
    if [ -L "/proc/$pid/exe" ]; then
        identity="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
        identity="${identity% (deleted)}"
    fi
    if [ -z "$identity" ] && [ "${OS:-}" = "Darwin" ]; then
        [ -x "$lsof_tool" ] || return 2
        identity="$("$lsof_tool" -a -p "$pid" -d txt -Fn 2>/dev/null \
            | sed -n 's/^n//p' | sed -n '1p')" || return 2
        [ -n "$identity" ] || return 2
    fi
    if [ -z "$identity" ]; then
        command -v ps >/dev/null 2>&1 || return 2
        identity="$(ps -ww -o comm= -p "$pid" 2>/dev/null)" || return 2
    fi
    identity="${identity#"${identity%%[![:space:]]*}"}"
    identity="${identity%"${identity##*[![:space:]]}"}"
    [ -n "$identity" ] || return 1
    printf '%s' "$identity"
}

local_owned_process_identity_matches() {
    local identity="$1" allow_basename="$2" owned_path
    shift 2
    for owned_path in "$@"; do
        [ -n "$owned_path" ] || continue
        [ "$identity" = "$owned_path" ] && return 0
    done
    [ "$allow_basename" = "1" ] && [ "$identity" = "cua-driver-local" ]
}

local_owned_process_matches_generation() {
    local pid="$1" expected_generation="$2" allow_basename="$3"
    shift 3
    local current_generation="" identity=""
    local_owned_process_alive "$pid" || return 1
    current_generation="$(local_owned_process_generation "$pid")" || return 2
    [ "$current_generation" = "$expected_generation" ] || return 1
    identity="$(local_owned_process_identity "$pid")" || return 2
    local_owned_process_identity_matches "$identity" "$allow_basename" "$@" || return 1
}

local_owned_process_records() {
    local allow_basename="$1"
    shift
    local candidates="" pgrep_status=0 uid="" pid identity="" generation=""
    command -v pgrep >/dev/null 2>&1 || return 2
    uid="$(id -u 2>/dev/null || true)"
    case "$uid" in ''|*[!0-9]*) return 2 ;; esac
    candidates="$(pgrep -U "$uid" -f '(^|[[:space:]/])cua-driver-local([[:space:]]|$)' 2>/dev/null)" \
        || pgrep_status=$?
    case "$pgrep_status" in
        0) ;;
        1) return 0 ;;
        *) return 2 ;;
    esac
    while IFS= read -r pid; do
        [ -n "$pid" ] || continue
        case "$pid" in *[!0-9]*|'') return 2 ;; esac
        local_owned_process_alive "$pid" || continue
        identity="$(local_owned_process_identity "$pid")" || {
            local_owned_process_alive "$pid" || continue
            return 2
        }
        local_owned_process_identity_matches "$identity" "$allow_basename" "$@" || continue
        generation="$(local_owned_process_generation "$pid")" || {
            local_owned_process_alive "$pid" || continue
            return 2
        }
        printf '%s\t%s\n' "$pid" "$generation"
    done <<EOF
$candidates
EOF
}

local_owned_process_wait_for_exit() {
    local pid="$1" generation="$2" allow_basename="$3"
    shift 3
    local attempts=0 status=0
    while :; do
        status=0
        local_owned_process_matches_generation \
            "$pid" "$generation" "$allow_basename" "$@" || status=$?
        case "$status" in
            0) ;;
            1) return 0 ;;
            *) return 2 ;;
        esac
        [ "$attempts" -lt 20 ] || return 1
        sleep 0.1 2>/dev/null || sleep 1 || true
        attempts=$((attempts + 1))
    done
}

stop_verified_local_processes() {
    local allow_basename="$1"
    shift
    local records="" status=0 pid generation wait_status
    records="$(local_owned_process_records "$allow_basename" "$@")" || status=$?
    if [ "$status" != "0" ]; then
        echo "${RED:-}Error: process inspection failed before local daemon shutdown; no TCC or app cleanup was attempted.${NORMAL:-}" >&2
        return 1
    fi
    while IFS=$'\t' read -r pid generation; do
        [ -n "$pid" ] || continue
        status=0
        local_owned_process_matches_generation \
            "$pid" "$generation" "$allow_basename" "$@" || status=$?
        [ "$status" != "2" ] || return 1
        [ "$status" = "0" ] || continue
        kill -TERM "$pid" 2>/dev/null || true
        wait_status=0
        local_owned_process_wait_for_exit \
            "$pid" "$generation" "$allow_basename" "$@" || wait_status=$?
        [ "$wait_status" != "2" ] || return 1
        if [ "$wait_status" = "1" ]; then
            status=0
            local_owned_process_matches_generation \
                "$pid" "$generation" "$allow_basename" "$@" || status=$?
            [ "$status" != "2" ] || return 1
            [ "$status" = "0" ] && kill -KILL "$pid" 2>/dev/null || true
            wait_status=0
            local_owned_process_wait_for_exit \
                "$pid" "$generation" "$allow_basename" "$@" || wait_status=$?
            [ "$wait_status" = "0" ] || return 1
        fi
    done <<EOF
$records
EOF
    records=""
    status=0
    records="$(local_owned_process_records "$allow_basename" "$@")" || status=$?
    if [ "$status" != "0" ]; then
        echo "${RED:-}Error: process inspection failed while verifying local daemon shutdown; no TCC or app cleanup was attempted.${NORMAL:-}" >&2
        return 1
    fi
    if [ -n "$records" ]; then
        echo "${RED:-}Error: a verified local cua-driver process remains; no TCC or app cleanup was attempted.${NORMAL:-}" >&2
        return 1
    fi
}

# A successful process scan is not enough when launchd still owns a KeepAlive
# job: the daemon can be absent for one poll and respawn immediately before an
# app/signing-identity transition. Treat only launchctl's documented
# "service not found" status as quiescent, and use bootout as the fallback when
# the legacy plist-based unload did not actually remove the loaded job.
local_launchagent_state() {
    local label="$1" tool="${CUA_DRIVER_LAUNCHCTL:-/bin/launchctl}"
    local uid="" status=0
    command -v "$tool" >/dev/null 2>&1 || return 2
    uid="$(id -u 2>/dev/null || true)"
    case "$uid" in ''|*[!0-9]*) return 2 ;; esac
    "$tool" print "gui/$uid/$label" >/dev/null 2>&1 || status=$?
    case "$status" in
        0) return 0 ;;
        113) return 1 ;;
        *) return 2 ;;
    esac
}

stop_and_verify_local_launchagent() {
    local plist="$1" label="$2" tool="${CUA_DRIVER_LAUNCHCTL:-/bin/launchctl}"
    local uid="" state=0
    [ "${OS:-}" = "Darwin" ] || return 0
    command -v "$tool" >/dev/null 2>&1 || {
        echo "${RED:-}Error: launchctl is unavailable; local daemon quiescence cannot be verified.${NORMAL:-}" >&2
        return 1
    }
    uid="$(id -u 2>/dev/null || true)"
    case "$uid" in
        ''|*[!0-9]*)
            echo "${RED:-}Error: current user identity is unavailable; local daemon quiescence cannot be verified.${NORMAL:-}" >&2
            return 1
            ;;
    esac

    if [ -f "$plist" ]; then
        "$tool" unload "$plist" >/dev/null 2>&1 || true
    fi
    state=0
    local_launchagent_state "$label" || state=$?
    if [ "$state" = "0" ]; then
        if ! "$tool" bootout "gui/$uid/$label" >/dev/null 2>&1; then
            echo "${RED:-}Error: could not stop loaded local launchd job $label; no history or app identity was changed.${NORMAL:-}" >&2
            return 1
        fi
    elif [ "$state" != "1" ]; then
        echo "${RED:-}Error: could not inspect local launchd job $label; no history or app identity was changed.${NORMAL:-}" >&2
        return 1
    fi

    state=0
    local_launchagent_state "$label" || state=$?
    if [ "$state" != "1" ]; then
        echo "${RED:-}Error: local launchd job $label remains active or unverifiable; no history or app identity was changed.${NORMAL:-}" >&2
        return 1
    fi
}

# Return 0 when the unit is active/enabled, 1 when it is provably
# inactive/disabled, and 2 when systemd cannot answer reliably.
local_systemd_active_state() {
    local service="$1" status=0
    command -v systemctl >/dev/null 2>&1 || return 2
    systemctl --user is-active --quiet "$service" >/dev/null 2>&1 || status=$?
    case "$status" in
        0) return 0 ;;
        3|4) return 1 ;;
        *) return 2 ;;
    esac
}

local_systemd_enabled_state() {
    local service="$1" value="" status=0
    command -v systemctl >/dev/null 2>&1 || return 2
    value="$(systemctl --user is-enabled "$service" 2>/dev/null)" || status=$?
    case "$value" in
        enabled|enabled-runtime|linked|linked-runtime|alias) return 0 ;;
        disabled|masked|masked-runtime|not-found) return 1 ;;
        *)
            [ "$status" = "0" ] && return 0
            return 2
            ;;
    esac
}

stop_and_verify_local_systemd_service() {
    local service="$1" unit_path="$2"
    local active_state=0

    if ! command -v systemctl >/dev/null 2>&1; then
        if [ -e "$unit_path" ] || [ -L "$unit_path" ]; then
            echo "${RED:-}Error: systemctl is unavailable; local daemon supervisor quiescence cannot be verified.${NORMAL:-}" >&2
            return 1
        fi
        return 0
    fi

    active_state=0
    local_systemd_active_state "$service" || active_state=$?
    if [ "$active_state" = "1" ]; then
        return 0
    fi
    if [ "$active_state" = "2" ]; then
        echo "${RED:-}Error: could not inspect local systemd service $service.${NORMAL:-}" >&2
        return 1
    fi
    if ! systemctl --user stop "$service" >/dev/null 2>&1; then
        echo "${RED:-}Error: could not stop local systemd service $service.${NORMAL:-}" >&2
        return 1
    fi

    active_state=0
    local_systemd_active_state "$service" || active_state=$?
    if [ "$active_state" != "1" ]; then
        echo "${RED:-}Error: local systemd service $service remains active or unverifiable.${NORMAL:-}" >&2
        return 1
    fi
}

disable_and_verify_local_systemd_service() {
    local service="$1" unit_path="$2"
    local active_state=0 enabled_state=0

    if ! command -v systemctl >/dev/null 2>&1; then
        if [ -e "$unit_path" ] || [ -L "$unit_path" ]; then
            echo "${RED:-}Error: systemctl is unavailable; local daemon supervisor disablement cannot be verified.${NORMAL:-}" >&2
            return 1
        fi
        return 0
    fi

    active_state=0
    local_systemd_active_state "$service" || active_state=$?
    enabled_state=0
    local_systemd_enabled_state "$service" || enabled_state=$?
    if [ "$active_state" = "1" ] && [ "$enabled_state" = "1" ]; then
        return 0
    fi
    if [ "$active_state" = "2" ] || [ "$enabled_state" = "2" ]; then
        echo "${RED:-}Error: could not inspect local systemd service $service.${NORMAL:-}" >&2
        return 1
    fi
    if ! systemctl --user disable --now "$service" >/dev/null 2>&1; then
        echo "${RED:-}Error: could not disable and stop local systemd service $service.${NORMAL:-}" >&2
        return 1
    fi

    active_state=0
    local_systemd_active_state "$service" || active_state=$?
    enabled_state=0
    local_systemd_enabled_state "$service" || enabled_state=$?
    if [ "$active_state" != "1" ] || [ "$enabled_state" != "1" ]; then
        echo "${RED:-}Error: local systemd service $service remains active, enabled, or unverifiable.${NORMAL:-}" >&2
        return 1
    fi
}

local_signing_keychain() {
    if [ -n "${CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN:-}" ]; then
        printf '%s' "$CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN"
    elif [ -f "$HOME/Library/Keychains/cua-driver-signing.keychain-db" ]; then
        printf '%s' "$HOME/Library/Keychains/cua-driver-signing.keychain-db"
    elif [ -f "$HOME/Library/Keychains/login.keychain-db" ]; then
        printf '%s' "$HOME/Library/Keychains/login.keychain-db"
    else
        printf '%s' "$HOME/Library/Keychains/login.keychain"
    fi
}

print_local_signing_bootstrap() {
    echo "Create and unlock a dedicated development signing keychain, then rerun:" >&2
    echo "  SIGNING_KEYCHAIN=\"\$HOME/Library/Keychains/cua-driver-signing.keychain-db\"" >&2
    echo "  security create-keychain \"\$SIGNING_KEYCHAIN\"  # first time only; prompts for a password" >&2
    echo "  security set-keychain-settings \"\$SIGNING_KEYCHAIN\"" >&2
    echo "  security unlock-keychain \"\$SIGNING_KEYCHAIN\"" >&2
    echo "  export CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN=\"\$SIGNING_KEYCHAIN\"" >&2
    echo "  bash libs/cua-driver/scripts/install-local.sh --require-stable-signing" >&2
    echo "If the certificate is newly created, authorize its private key for codesign as described in:" >&2
    echo "  libs/cua-driver/scripts/README.md#stable-macos-local-signing" >&2
}

# Echoes the `codesign --sign` argument: a matching identity's SHA-1 when
# available, or "-" when it cannot be created or found.
ensure_local_signing_identity() {
    { [ "$OS" = "Darwin" ] && command -v codesign >/dev/null 2>&1; } \
        || { printf -- '-'; return; }
    local kc
    kc="$(local_signing_keychain)"
    [ -f "$kc" ] || { printf -- '-'; return; }
    local identity
    if [ -n "${CUA_DRIVER_LOCAL_SIGNING_IDENTITY:-}" ]; then
        case "$CUA_DRIVER_LOCAL_SIGNING_IDENTITY" in
            *[!0-9A-Fa-f]*|'') printf -- '-'; return ;;
        esac
        [ "${#CUA_DRIVER_LOCAL_SIGNING_IDENTITY}" -eq 40 ] \
            || { printf -- '-'; return; }
        identity="$(security find-identity -p codesigning "$kc" 2>/dev/null \
            | awk -v wanted="$CUA_DRIVER_LOCAL_SIGNING_IDENTITY" \
                '{ for (field = 1; field <= NF; field++) if (toupper($field) == toupper(wanted)) { print $field; exit } }')"
        [ -n "$identity" ] && printf '%s' "$identity" || printf -- '-'
        return
    fi
    identity="$(security find-identity -p codesigning "$kc" 2>/dev/null \
        | awk -v cn="$CUA_LOCAL_SIGN_CN" 'index($0, "\"" cn "\"") { print $2; exit }')"
    if [ -n "$identity" ]; then
        printf '%s' "$identity"
        return
    fi
    command -v openssl >/dev/null 2>&1 || { printf -- '-'; return; }
    local tmp
    tmp="$(mktemp -d)" || { printf -- '-'; return; }
    printf '[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=%s\n[ext]\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n' \
        "$CUA_LOCAL_SIGN_CN" > "$tmp/req.cnf"
    local pw="cua-local-$$"
    if openssl req -x509 -newkey rsa:2048 -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
            -days 3650 -nodes -config "$tmp/req.cnf" >/dev/null 2>&1 \
       && { openssl pkcs12 -export -legacy -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
                -out "$tmp/id.p12" -passout pass:"$pw" -name "$CUA_LOCAL_SIGN_CN" >/dev/null 2>&1 \
            || openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
                -out "$tmp/id.p12" -passout pass:"$pw" -name "$CUA_LOCAL_SIGN_CN" >/dev/null 2>&1; } \
       && security import "$tmp/id.p12" -k "$kc" -P "$pw" \
            -T /usr/bin/codesign >/dev/null 2>&1; then
        identity="$(security find-identity -p codesigning "$kc" 2>/dev/null \
            | awk -v cn="$CUA_LOCAL_SIGN_CN" 'index($0, "\"" cn "\"") { print $2; exit }')"
        if ! rm -rf "$tmp"; then
            echo "warning: could not remove temporary local-signing directory $tmp" >&2
        fi
        if [ -n "$identity" ]; then
            printf '%s' "$identity"
            return
        fi
        printf -- '-'
        return
    fi
    if ! rm -rf "$tmp"; then
        echo "warning: could not remove temporary local-signing directory $tmp" >&2
    fi
    printf -- '-'
}

# Keychain-backed codesign can wait forever for a GUI authorization prompt.
codesign_bounded() {
    local timeout_seconds="$1"
    shift
    local kc=""
    if [ -n "${CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN:-}" ]; then
        kc="$CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN"
    elif [ -f "$HOME/Library/Keychains/cua-driver-signing.keychain-db" ]; then
        kc="$HOME/Library/Keychains/cua-driver-signing.keychain-db"
    fi
    if [ -n "$kc" ]; then
        set -- --keychain "$kc" "$@"
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$timeout_seconds" codesign "$@"
    elif command -v perl >/dev/null 2>&1; then
        perl -e 'alarm shift; exec @ARGV' "$timeout_seconds" codesign "$@"
    else
        codesign "$@"
    fi
}

clean_partial_bundle_signature() {
    local app="$1"
    if ! rm -rf "$app/Contents/_CodeSignature"; then
        echo "${RED:-}Error: could not remove partial bundle signature at $app.${NORMAL:-}" >&2
        return 1
    fi
    if ! find "$app" -type f -name '*.cstemp' -delete; then
        echo "${RED:-}Error: could not remove temporary signing files from $app.${NORMAL:-}" >&2
        return 1
    fi
}

designated_requirement() {
    codesign -d -r- "$1" 2>&1 \
        | sed -n -e 's/^designated => //p' -e 's/^# designated => //p'
}

local_app_bundle_value() {
    local app="$1"
    local key="$2"
    /usr/libexec/PlistBuddy -c "Print :$key" \
        "$app/Contents/Info.plist" 2>/dev/null
}

# Local builds can use either the dedicated development certificate or an
# ad-hoc signature, so there is no global signer pin. Still require a valid
# sealed bundle, the exact bundle/executable tuple, and a code-signing
# requirement that independently binds the expected identifier. This prevents
# a mutable Info.plist alone from authorizing deletion or process cleanup.
verify_local_app_identity() {
    local app="$1"
    local expected_bundle_id="$2"
    local expected_executable="$3"
    local actual_bundle_id actual_executable requirement

    [ -d "$app" ] && [ ! -L "$app" ] || return 1
    [ -f "$app/Contents/Info.plist" ] \
        && [ ! -L "$app/Contents/Info.plist" ] || return 1
    actual_bundle_id="$(local_app_bundle_value "$app" CFBundleIdentifier || true)"
    actual_executable="$(local_app_bundle_value "$app" CFBundleExecutable || true)"
    [ "$actual_bundle_id" = "$expected_bundle_id" ] || return 1
    [ "$actual_executable" = "$expected_executable" ] || return 1
    [ -f "$app/Contents/MacOS/$expected_executable" ] \
        && [ ! -L "$app/Contents/MacOS/$expected_executable" ] \
        && [ -x "$app/Contents/MacOS/$expected_executable" ] || return 1
    codesign --verify --deep --strict "$app" >/dev/null 2>&1 || return 1
    codesign --verify --deep --strict \
        -R "=identifier \"$expected_bundle_id\"" "$app" >/dev/null 2>&1 \
        || return 1
    requirement="$(designated_requirement "$app" || true)"
    [ "$(classify_designated_requirement "$requirement")" != "unknown" ]
}

classify_designated_requirement() {
    case "$1" in
        *"certificate leaf"*) printf '%s' "certificate-backed" ;;
        *cdhash*) printf '%s' "ad-hoc" ;;
        *) printf '%s' "unknown" ;;
    esac
}

# TCC stores the previous app's complete code-signing requirement. Comparing
# requirement text or broad signer classes misses important transitions such
# as ad-hoc -> certificate-backed. Ask codesign whether the candidate satisfies
# the exact previous requirement and distinguish a real mismatch (status 3)
# from an operational verification failure.
local_requirement_compatibility() {
    local previous_requirement="$1"
    local candidate_app="$2"
    local codesign_output="" codesign_status=0

    if [ -z "$previous_requirement" ]; then
        printf '%s' "unknown"
    elif codesign_output="$(codesign --verify --deep --strict \
            -R "=$previous_requirement" "$candidate_app" 2>&1)"; then
        printf '%s' "compatible"
    else
        codesign_status=$?
        if [ "$codesign_status" -eq 3 ]; then
            printf '%s' "incompatible"
        else
            echo "${YELLOW:-}warning: could not evaluate the previous local-app signing requirement (codesign status $codesign_status); preserving the installed app and TCC rows.${NORMAL:-}" >&2
            [ -z "$codesign_output" ] \
                || echo "${YELLOW:-}warning: codesign: $codesign_output${NORMAL:-}" >&2
            printf '%s' "unknown"
        fi
    fi
}

# Reset every TCC service used by Cua Driver Local when the replacement no
# longer satisfies the exact previous requirement. The caller must first
# verify and register the newly installed bundle.
reset_local_tcc_after_requirement_change() {
    local compatibility="$1"
    local bundle_id="com.meta.musecode.cua.driver.local"
    local service failed_services=""

    [ "$compatibility" = "incompatible" ] || return 0

    if ! command -v tccutil >/dev/null 2>&1; then
        echo "${RED}Error: tccutil is required to clear stale local-app permission rows after its signing requirement changed.${NORMAL}" >&2
        return 1
    fi

    for service in Accessibility ScreenCapture AppleEvents; do
        if ! tccutil reset "$service" "$bundle_id" >/dev/null 2>&1; then
            failed_services="$failed_services $service"
        fi
    done
    if [ -n "$failed_services" ]; then
        echo "${RED}Error: could not reset these TCC services for $bundle_id:$failed_services.${NORMAL}" >&2
        echo "The replacement will be rolled back. After resolving tccutil, retry:" >&2
        echo "  tccutil reset Accessibility $bundle_id" >&2
        echo "  tccutil reset ScreenCapture $bundle_id" >&2
        echo "  tccutil reset AppleEvents $bundle_id" >&2
        return 1
    fi

    echo "${YELLOW}The app signing requirement changed; cleared stale Accessibility, Screen Recording, and Automation rows for $bundle_id.${NORMAL}" >&2
    echo "Re-grant them to the new app with: cua-driver-local permissions grant" >&2
}

local_directory_has_entries() {
    local directory="$1" entry
    for entry in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
        if [ -e "$entry" ] || [ -L "$entry" ]; then
            return 0
        fi
    done
    return 1
}

# Local Computer History keys are authorized to the installed app's signing
# requirement. Replacing or removing that requirement while encrypted history
# remains can strand both the ciphertext and its Keychain key. The currently
# authenticated helper must purge it, or a separately reviewed migration tool
# must transfer it, before an identity transition.
refuse_local_history_identity_transition() {
    local history_root="$1"
    local trusted_app="$2"
    local transition="$3"
    local helper="$trusted_app/Contents/MacOS/cua-driver-local"

    [ -d "$history_root" ] || return 0
    local_directory_has_entries "$history_root" || return 0

    echo "${RED:-}Error: cannot $transition while existing local Computer History state is present.${NORMAL:-}" >&2
    echo "The replacement identity may not be able to read or destroy the current Keychain-protected history key." >&2
    echo "To preserve history, stop here and use an explicitly reviewed migration tool." >&2
    echo "To discard it, run the currently installed authenticated helper before retrying:" >&2
    echo "  $helper history purge-offline --yes" >&2
    return 1
}

legacy_local_app_bundle_id() {
    local_app_bundle_value "$1" CFBundleIdentifier
}

register_local_app() {
    local app="$1"
    local lsregister="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
    [ -x "$lsregister" ] && "$lsregister" -f "$app" >/dev/null 2>&1
}

unregister_local_app() {
    local app="$1"
    local lsregister="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
    [ -x "$lsregister" ] && "$lsregister" -u "$app" >/dev/null 2>&1
}

register_legacy_local_app() {
    register_local_app "$1"
}

# Restore only a replacement that this installer has authenticated. The prior
# app is re-registered before failure is reported to the caller.
restore_local_app_backup() {
    local app="$1"
    local backup="$2"
    local expected_bundle_id="$3"
    local expected_executable="$4"

    local failed_path="${app}.failed-install.$$" rollback_failed=0

    if [ -e "$app" ] || [ -L "$app" ]; then
        if ! unregister_local_app "$app"; then
            echo "${RED:-}Error: could not unregister failed local app candidate at $app.${NORMAL:-}" >&2
            rollback_failed=1
        fi
        if verify_local_app_identity "$app" "$expected_bundle_id" "$expected_executable"; then
            if ! rm -rf -- "$app"; then
                echo "${RED:-}Error: could not remove failed authenticated local app candidate at $app.${NORMAL:-}" >&2
                return 1
            fi
        else
            if [ -e "$failed_path" ] || [ -L "$failed_path" ]; then
                echo "${RED:-}Error: cannot preserve failed local app candidate because $failed_path already exists.${NORMAL:-}" >&2
                return 1
            fi
            if ! mv "$app" "$failed_path"; then
                echo "${RED:-}Error: could not preserve unauthenticated failed local app candidate at $failed_path.${NORMAL:-}" >&2
                return 1
            fi
            echo "${YELLOW:-}warning: preserved unauthenticated failed local app candidate at $failed_path.${NORMAL:-}" >&2
        fi
    fi
    if [ -e "$backup" ] || [ -L "$backup" ]; then
        if ! verify_local_app_identity "$backup" "$expected_bundle_id" "$expected_executable"; then
            echo "${RED:-}Error: refusing to restore unauthenticated app backup at $backup.${NORMAL:-}" >&2
            return 1
        fi
        if ! mv "$backup" "$app"; then
            echo "${RED:-}Error: could not restore authenticated local app backup from $backup.${NORMAL:-}" >&2
            return 1
        fi
        if ! register_local_app "$app"; then
            echo "${RED:-}Error: restored $app but could not re-register it with LaunchServices.${NORMAL:-}" >&2
            return 1
        fi
    fi
    [ "$rollback_failed" -eq 0 ]
}

remove_authenticated_local_app_backup() {
    local backup="$1"
    local expected_bundle_id="$2"
    local expected_executable="$3"

    [ -e "$backup" ] || [ -L "$backup" ] || return 0
    if ! verify_local_app_identity "$backup" "$expected_bundle_id" "$expected_executable"; then
        echo "${YELLOW:-}warning: preserving unauthenticated local install backup at $backup.${NORMAL:-}" >&2
        return 1
    fi
    if ! rm -rf -- "$backup"; then
        echo "${RED:-}Error: could not remove authenticated local install backup at $backup.${NORMAL:-}" >&2
        return 1
    fi
}

remove_legacy_local_app_path() {
    local app="$1"
    if [ -w "$(dirname "$app")" ]; then
        if ! rm -rf -- "$app"; then
            return 1
        fi
    else
        if ! sudo rm -rf -- "$app"; then
            return 1
        fi
    fi
}

# Authenticate and detach the retired local-development identity before a
# caller moves or removes it. Keeping this separate from deletion lets the
# installer stage the old bundle in a rollback slot until the replacement is
# fully committed.
prepare_legacy_local_app_removal() {
    local app="$1"
    local reset_tcc="${2:-1}"
    local expected_bundle_id="com.trycua.driver.local"
    local history_root="${CUA_DRIVER_LOCAL_HISTORY_ROOT:-$HOME/Library/Application Support/cua-driver-local/computer-history}"
    local actual_bundle_id service failed_services=""

    [ "${OS:-}" = "Darwin" ] || return 0
    [ -e "$app" ] || [ -L "$app" ] || return 0
    if [ -L "$app" ] || [ ! -d "$app" ]; then
        echo "${RED:-}Error: refusing to remove unsafe legacy local app path $app.${NORMAL:-}" >&2
        return 1
    fi
    actual_bundle_id="$(legacy_local_app_bundle_id "$app" || true)"
    if [ "$actual_bundle_id" != "$expected_bundle_id" ]; then
        echo "${RED:-}Error: preserving $app because its bundle ID is ${actual_bundle_id:-unreadable}, not $expected_bundle_id.${NORMAL:-}" >&2
        return 1
    fi
    if ! verify_local_app_identity "$app" "$expected_bundle_id" "cua-driver-local"; then
        echo "${RED:-}Error: preserving $app because its executable or code-signing identity could not be authenticated.${NORMAL:-}" >&2
        return 1
    fi

    if ! refuse_local_history_identity_transition \
        "$history_root" \
        "$app" "remove the legacy local app identity"; then
        return 1
    fi

    if [ "$reset_tcc" = "1" ]; then
        if ! command -v tccutil >/dev/null 2>&1; then
            echo "${RED:-}Error: tccutil is required to clear the retired local-app permission rows.${NORMAL:-}" >&2
            return 1
        fi
        if ! register_legacy_local_app "$app"; then
            echo "${RED:-}Error: could not register $app before resetting its TCC rows; the app was preserved.${NORMAL:-}" >&2
            return 1
        fi
        for service in Accessibility ScreenCapture AppleEvents; do
            if ! tccutil reset "$service" "$expected_bundle_id" >/dev/null 2>&1; then
                failed_services="$failed_services $service"
            fi
        done
        if [ -n "$failed_services" ]; then
            echo "${RED:-}Error: could not reset these TCC services for $expected_bundle_id:$failed_services; the legacy app was preserved.${NORMAL:-}" >&2
            return 1
        fi
    fi

    if ! unregister_local_app "$app"; then
        echo "${RED:-}Error: could not unregister retired local app $app; the app was preserved.${NORMAL:-}" >&2
        return 1
    fi
}

# Remove only the retired local-development bundle. When TCC cleanup is
# requested, keep the bundle available and registered until every scoped reset
# succeeds so a failed reset remains retryable.
cleanup_legacy_local_app() {
    local app="$1"
    local reset_tcc="${2:-1}"
    local expected_bundle_id="com.trycua.driver.local"

    [ "${OS:-}" = "Darwin" ] || return 0
    prepare_legacy_local_app_removal "$app" "$reset_tcc" || return 1
    [ -e "$app" ] || [ -L "$app" ] || return 0

    if ! remove_legacy_local_app_path "$app"; then
        echo "${RED:-}Error: could not remove retired local app $app.${NORMAL:-}" >&2
        return 1
    fi
    echo "${YELLOW:-}Removed retired local app $app (${expected_bundle_id}).${NORMAL:-}" >&2
}

# Signs a staged local app without touching the live installation. Strict mode
# refuses the ad-hoc path; non-strict mode keeps it available for casual local
# development but makes the resulting TCC reset impossible to miss.
sign_staged_local_app() {
    local app_stage="$1"
    local app_dest="$2"
    local sign_id requirement signing_class
    sign_id="$(ensure_local_signing_identity)"

    if [ "$sign_id" != "-" ] \
       && codesign_bounded 20 --force --deep --sign "$sign_id" "$app_stage" 2>/dev/null; then
        requirement="$(designated_requirement "$app_stage")"
        signing_class="$(classify_designated_requirement "$requirement")"
        if [ "$signing_class" = "certificate-backed" ]; then
            echo "${GREEN}signed staged app with a stable local identity — TCC grants survive future install-local rebuilds${NORMAL}"
            return 0
        fi
        echo "${YELLOW}warning: the requested stable identity produced a non-certificate designated requirement${NORMAL}" >&2
    fi

    if [ "${CUA_DRIVER_REQUIRE_STABLE_SIGNING:-0}" = "1" ]; then
        if ! clean_partial_bundle_signature "$app_stage"; then
            echo "${RED}Error: failed to clean the staged app after stable signing failed.${NORMAL}" >&2
        fi
        echo "${RED}Error: stable macOS signing is required, but no usable certificate-backed identity was available.${NORMAL}" >&2
        echo "The live installation was not changed." >&2
        print_local_signing_bootstrap
        return 1
    fi

    if [ -d "$app_dest" ]; then
        requirement="$(designated_requirement "$app_dest")"
        if [ "$(classify_designated_requirement "$requirement")" = "certificate-backed" ]; then
            if ! clean_partial_bundle_signature "$app_stage"; then
                echo "${RED}Error: failed to clean the staged app after stable signing failed.${NORMAL}" >&2
            fi
            echo "${RED}Error: stable signing failed; preserving the existing certificate-signed $app_dest and its TCC grants.${NORMAL}" >&2
            print_local_signing_bootstrap
            return 1
        fi
    fi

    if ! clean_partial_bundle_signature "$app_stage"; then
        echo "${RED}Error: could not prepare the staged app for ad-hoc signing.${NORMAL}" >&2
        return 1
    fi
    if ! codesign_bounded 20 --force --deep --sign - "$app_stage" 2>/dev/null; then
        if ! clean_partial_bundle_signature "$app_stage"; then
            echo "${RED}Error: failed to clean the staged app after ad-hoc signing failed.${NORMAL}" >&2
        fi
        echo "${RED}Error: codesign of staged MuseCodeCuaDriverLocal.app failed; live installation was not changed.${NORMAL}" >&2
        return 1
    fi
    requirement="$(designated_requirement "$app_stage")"
    if [ "$(classify_designated_requirement "$requirement")" != "ad-hoc" ]; then
        echo "${RED}Error: could not verify the staged app's ad-hoc designated requirement; live installation was not changed.${NORMAL}" >&2
        return 1
    fi
    echo "${YELLOW}WARNING: MuseCodeCuaDriverLocal.app was signed ad-hoc (designated requirement uses cdhash).${NORMAL}" >&2
    echo "${YELLOW}Accessibility and Screen Recording grants WILL become invalid on the next rebuild.${NORMAL}" >&2
    print_local_signing_bootstrap
}
