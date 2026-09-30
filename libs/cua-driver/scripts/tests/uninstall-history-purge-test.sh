#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
UNINSTALL="$SCRIPT_DIR/../uninstall.sh"
FIXTURE="$(mktemp -d)"
APP="$FIXTURE/CuaDriver.app"
HELPER="$APP/Contents/MacOS/cua-driver"
STATE="$FIXTURE/computer-history"
LOG="$FIXTURE/helper.log"
mkdir -p "$(dirname "$HELPER")" "$STATE"
touch "$STATE/state.json"
touch "$APP/Contents/Info.plist"

cat > "$HELPER" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$UNINSTALL_FIXTURE_LOG"
if [[ "$*" == "history purge-offline --yes" && "${UNINSTALL_FIXTURE_FAIL:-0}" == "1" ]]; then
    exit 1
fi
SH
chmod +x "$HELPER"
cat > "$FIXTURE/codesign" <<'SH'
#!/usr/bin/env bash
case "$1 $2" in
    '--verify --deep') exit 0 ;;
    '-d --verbose=4')
        printf '%s\n' \
            'Identifier=com.meta.musecode.cua.driver' \
            'TeamIdentifier=YCK386LBJ7' >&2
        ;;
    '-d -r-')
        printf '%s\n' 'designated => identifier "com.meta.musecode.cua.driver" and anchor apple generic and certificate leaf[subject.OU] = "YCK386LBJ7"' >&2
        ;;
    *) exit 2 ;;
esac
SH
chmod +x "$FIXTURE/codesign"
cat > "$FIXTURE/PlistBuddy" <<'SH'
#!/usr/bin/env bash
case "$2" in
    *CFBundleIdentifier) printf '%s\n' 'com.meta.musecode.cua.driver' ;;
    *CFBundleExecutable) printf '%s\n' 'cua-driver' ;;
    *) exit 2 ;;
esac
SH
chmod +x "$FIXTURE/PlistBuddy"
cat > "$FIXTURE/spctl" <<'SH'
#!/usr/bin/env bash
printf '%s\n' 'source=Notarized Developer ID' >&2
SH
chmod +x "$FIXTURE/spctl"

CUA_DRIVER_UNINSTALL_TEST_SOURCE_ONLY=1 source "$UNINSTALL"
export UNINSTALL_FIXTURE_LOG="$LOG"
if reject_root_invocation 0 2> "$FIXTURE/root-error.log"; then
    echo "expected root invocation refusal" >&2
    exit 1
fi
grep -Fq 'do not run the Cua Driver uninstaller with sudo' "$FIXTURE/root-error.log"
reject_root_invocation 501
purge_macos_history "$APP" "$HELPER" 1 "$FIXTURE/codesign" \
    "$FIXTURE/PlistBuddy" com.meta.musecode.cua.driver \
    YCK386LBJ7 "$FIXTURE/spctl"
[[ "$(sed -n '1p' "$LOG")" == "history purge-offline --yes" ]]
[[ "$(wc -l < "$LOG")" == "1" ]]

: > "$LOG"
purge_linux_history "$HELPER" 1
[[ "$(sed -n '1p' "$LOG")" == "history purge-offline --yes" ]]
[[ "$(wc -l < "$LOG")" == "1" ]]

export UNINSTALL_FIXTURE_FAIL=1
if purge_macos_history "$APP" "$HELPER" 1 "$FIXTURE/codesign" \
    "$FIXTURE/PlistBuddy" com.meta.musecode.cua.driver \
    YCK386LBJ7 "$FIXTURE/spctl" 2> "$FIXTURE/error.log"; then
    echo "expected synthetic purge failure" >&2
    exit 1
fi
grep -Fq history_purge_incomplete "$FIXTURE/error.log"
[[ -d "$APP" ]]
[[ -f "$STATE/state.json" ]]

if purge_linux_history "$FIXTURE/missing-helper" 1 2> "$FIXTURE/linux-error.log"; then
    echo "expected missing Linux helper refusal" >&2
    exit 1
fi
grep -Fq history_purge_incomplete "$FIXTURE/linux-error.log"

purge_line="$(grep -n 'purge_release_history_if_present "\$HISTORY_ROOT"' "$UNINSTALL" | tail -1 | cut -d: -f1)"
remove_line="$(grep -n 'rm -rf "\$APP_BUNDLE"' "$UNINSTALL" | head -1 | cut -d: -f1)"
[[ "$purge_line" -lt "$remove_line" ]]
package_remove_line="$(grep -n 'rm -rf -- "\$HOME_DIR/packages"' "$UNINSTALL" | head -1 | cut -d: -f1)"
[[ "$purge_line" -lt "$package_remove_line" ]]
grep -Fq 'preserved encrypted Computer History' "$UNINSTALL"

echo "uninstall history purge fixture: ok"
