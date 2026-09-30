from __future__ import annotations

import os
from pathlib import Path
import re
import subprocess


ROOT = Path(__file__).resolve().parents[4]
UNINSTALL = ROOT / "libs/cua-driver/scripts/uninstall.sh"
UNINSTALL_LOCAL = ROOT / "libs/cua-driver/scripts/uninstall-local.sh"


def _extract(script: Path, *names: str) -> str:
    source = script.read_text(encoding="utf-8")
    functions = []
    for name in names:
        match = re.search(rf"(?ms)^{re.escape(name)}\(\) \{{\n.*?^\}}\n", source)
        assert match, f"could not find shell function {name}"
        functions.append(match.group(0))
    return "\n".join(functions)


def _executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
    path.chmod(0o755)


def _app(tmp_path: Path, name: str = "CuaDriver.app") -> Path:
    app = tmp_path / name
    executable = app / "Contents/MacOS/cua-driver"
    _executable(executable, "exit 0")
    (app / "Contents/Info.plist").write_text("fixture\n", encoding="utf-8")
    return app


def _identity_tools(tmp_path: Path) -> tuple[Path, Path, Path]:
    plistbuddy = tmp_path / "PlistBuddy"
    _executable(
        plistbuddy,
        """case "$2" in
  *CFBundleIdentifier) printf '%s\\n' "$TEST_BUNDLE_ID" ;;
  *CFBundleExecutable) printf '%s\\n' "$TEST_EXECUTABLE" ;;
  *) exit 2 ;;
esac""",
    )
    codesign = tmp_path / "codesign"
    _executable(
        codesign,
        """case "$1 $2" in
  '--verify --deep')
    [ "${TEST_VERIFY_STATUS:-0}" = 0 ] || exit "$TEST_VERIFY_STATUS"
    case " $* " in
      *' -R '*) [ "$TEST_SIGNATURE_ID" = "$TEST_BUNDLE_ID" ] ;;
      *) exit 0 ;;
    esac
    ;;
  '-d --verbose=4')
    printf 'Identifier=%s\\nTeamIdentifier=%s\\nAuthority=%s\\nSignature=%s\\n' \\
      "$TEST_SIGNATURE_ID" "$TEST_TEAM_ID" "$TEST_AUTHORITY" "$TEST_SIGNATURE" >&2 ;;
  '-d -r-') printf 'designated => %s\\n' "$TEST_REQUIREMENT" >&2 ;;
  *) exit 2 ;;
esac""",
    )
    spctl = tmp_path / "spctl"
    _executable(spctl, "printf 'source=Notarized Developer ID\\n' >&2")
    return plistbuddy, codesign, spctl


def _run_shell(functions: str, body: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["/bin/bash", "-c", f"set -euo pipefail\n{functions}\n{body}"],
        cwd=ROOT,
        env={**os.environ, **env},
        text=True,
        capture_output=True,
        check=False,
    )


def test_release_app_requires_exact_bundle_executable_and_apple_team(tmp_path: Path) -> None:
    app = _app(tmp_path)
    plistbuddy, codesign, spctl = _identity_tools(tmp_path)
    functions = _extract(
        UNINSTALL,
        "validate_apple_team_id",
        "macos_plist_value",
        "macos_release_app_is_owned",
    )
    environment = {
        "TEST_BUNDLE_ID": "com.meta.musecode.cua.driver",
        "TEST_EXECUTABLE": "cua-driver",
        "TEST_SIGNATURE_ID": "com.meta.musecode.cua.driver",
        "TEST_TEAM_ID": "YCK386LBJ7",
        "TEST_AUTHORITY": "Developer ID Application: Cua",
        "TEST_SIGNATURE": "signed Mach-O thin",
        "TEST_REQUIREMENT": (
            'identifier "com.meta.musecode.cua.driver" and anchor apple generic '
            'and certificate leaf[subject.OU] = "YCK386LBJ7"'
        ),
    }
    call = (
        f'macos_release_app_is_owned "{app}" com.meta.musecode.cua.driver '
        f'cua-driver YCK386LBJ7 "{codesign}" "{plistbuddy}" "{spctl}"'
    )

    result = _run_shell(functions, call, environment)
    assert result.returncode == 0, result.stderr

    environment["TEST_TEAM_ID"] = "ATTACKER"
    result = _run_shell(functions, call, environment)
    assert result.returncode != 0

    environment["TEST_TEAM_ID"] = "YCK386LBJ7"
    environment["TEST_EXECUTABLE"] = "other"
    result = _run_shell(functions, call, environment)
    assert result.returncode != 0

    environment["TEST_EXECUTABLE"] = "cua-driver"
    for invalid_team in ("", "short", "TOO-LONG-TEAM"):
        invalid_call = (
            f'macos_release_app_is_owned "{app}" com.meta.musecode.cua.driver '
            f'cua-driver "{invalid_team}" "{codesign}" "{plistbuddy}" "{spctl}"'
        )
        result = _run_shell(functions, invalid_call, environment)
        assert result.returncode != 0

    source = UNINSTALL.read_text(encoding="utf-8")
    assert 'PINNED_PRODUCTION_TEAM_ID="4W5TH4RKQ2"' in source
    assert 'PRODUCTION_TEAM_ID="${CUA_DRIVER_PRODUCTION_TEAM_ID:-$PINNED_PRODUCTION_TEAM_ID}"' in source
    assert 'LEGACY_PRODUCTION_TEAM_ID="${CUA_DRIVER_LEGACY_TEAM_ID:-YCK386LBJ7}"' in source


def test_history_purge_never_executes_an_untrusted_shared_app(tmp_path: Path) -> None:
    app = _app(tmp_path)
    plistbuddy, codesign, spctl = _identity_tools(tmp_path)
    marker = tmp_path / "executed"
    helper = app / "Contents/MacOS/cua-driver"
    _executable(helper, f": > '{marker}'")
    functions = _extract(
        UNINSTALL,
        "validate_apple_team_id",
        "macos_plist_value",
        "macos_release_app_is_owned",
        "purge_macos_history",
    )
    environment = {
        "TEST_BUNDLE_ID": "com.meta.musecode.cua.driver",
        "TEST_EXECUTABLE": "cua-driver",
        "TEST_SIGNATURE_ID": "com.meta.musecode.cua.driver",
        "TEST_TEAM_ID": "ATTACKER",
        "TEST_AUTHORITY": "Attacker",
        "TEST_SIGNATURE": "signed Mach-O thin",
        "TEST_REQUIREMENT": (
            'identifier "com.meta.musecode.cua.driver" and anchor apple generic '
            'and certificate leaf[subject.OU] = "ATTACKER"'
        ),
    }
    body = f"""
PLISTBUDDY='{plistbuddy}'
RELEASE_BUNDLE_ID=com.meta.musecode.cua.driver
RELEASE_EXECUTABLE=cua-driver
PRODUCTION_TEAM_ID=YCK386LBJ7
SPCTL='{spctl}'
purge_macos_history '{app}' '{helper}' 1 '{codesign}' '{plistbuddy}' \
  "$RELEASE_BUNDLE_ID" "$PRODUCTION_TEAM_ID" "$SPCTL"
"""
    result = _run_shell(functions, body, environment)
    assert result.returncode != 0
    assert not marker.exists()
    assert "history_purge_incomplete" in result.stderr


def test_followup_purge_needs_no_helper_when_history_is_absent_or_empty(
    tmp_path: Path,
) -> None:
    functions = _extract(
        UNINSTALL,
        "directory_has_entries",
        "history_state_present",
        "purge_release_history_if_present",
    )
    empty = tmp_path / "empty-history"
    empty.mkdir()
    missing = tmp_path / "missing-history"
    nonempty = tmp_path / "nonempty-history"
    nonempty.mkdir()
    (nonempty / "chunk.cborseq").write_text("encrypted", encoding="utf-8")
    invoked = tmp_path / "purge-invoked"
    body = f"""
log() {{ :; }}
purge_macos_history() {{ : > '{invoked}'; return 91; }}
purge_linux_history() {{ : > '{invoked}'; return 92; }}
PACKAGES_DIR=/missing-packages
RUST_INSTALL_PRESENT=0
OS=Darwin
purge_release_history_if_present '{missing}'
purge_release_history_if_present '{empty}'
OS=Linux
purge_release_history_if_present '{missing}'
purge_release_history_if_present '{empty}'
[[ ! -e '{invoked}' ]]
! purge_release_history_if_present '{nonempty}'
[[ -e '{invoked}' ]]
"""

    result = _run_shell(functions, body, {})
    assert result.returncode == 0, result.stderr

    source = UNINSTALL.read_text(encoding="utf-8")
    assert "first reinstall" in source
    assert "while that\nverified helper is still installed" in source


def test_release_tcc_failure_does_not_unregister_app(tmp_path: Path) -> None:
    app = _app(tmp_path)
    fake_bin = tmp_path / "bin"
    calls = tmp_path / "calls"
    fake_bin.mkdir()
    lsregister = fake_bin / "lsregister"
    _executable(lsregister, f"printf 'ls:%s\\n' \"$*\" >> '{calls}'")
    _executable(
        fake_bin / "tccutil",
        f"printf 'tcc:%s\\n' \"$*\" >> '{calls}'\n[ \"$2\" != ScreenCapture ]",
    )
    functions = _extract(UNINSTALL, "maybe_reset_tcc")
    body = f"""
log() {{ :; }}
OS=Darwin
RESET_TCC=1
RELEASE_BUNDLE_ID=com.meta.musecode.cua.driver
LSREGISTER='{lsregister}'
TCCUTIL='{fake_bin / "tccutil"}'
maybe_reset_tcc '{app}' com.meta.musecode.cua.driver
"""
    result = _run_shell(functions, body, {"PATH": f"{fake_bin}:/usr/bin:/bin"})
    assert result.returncode != 0
    call_lines = calls.read_text(encoding="utf-8").splitlines()
    assert f"ls:-f {app}" in call_lines
    assert not any(line.startswith("ls:-u ") for line in call_lines)
    assert "ScreenCapture" in result.stderr


def test_release_tcc_cleanup_uses_verified_legacy_identity(tmp_path: Path) -> None:
    app = _app(tmp_path)
    fake_bin = tmp_path / "bin"
    calls = tmp_path / "calls"
    fake_bin.mkdir()
    lsregister = fake_bin / "lsregister"
    _executable(lsregister, f"printf 'ls:%s\\n' \"$*\" >> '{calls}'")
    _executable(fake_bin / "tccutil", f"printf 'tcc:%s\\n' \"$*\" >> '{calls}'")
    functions = _extract(UNINSTALL, "maybe_reset_tcc")
    body = f"""
log() {{ :; }}
OS=Darwin
RESET_TCC=1
RELEASE_BUNDLE_ID=com.meta.musecode.cua.driver
LSREGISTER='{lsregister}'
TCCUTIL='{fake_bin / "tccutil"}'
maybe_reset_tcc '{app}' com.trycua.driver
"""
    result = _run_shell(functions, body, {"PATH": f"{fake_bin}:/usr/bin:/bin"})
    assert result.returncode == 0, result.stderr
    call_lines = calls.read_text(encoding="utf-8").splitlines()
    assert "tcc:reset Accessibility com.trycua.driver" in call_lines
    assert "tcc:reset ScreenCapture com.trycua.driver" in call_lines
    assert call_lines[-1] == f"ls:-u {app}"


def test_local_app_ownership_uses_signature_not_only_plist(tmp_path: Path) -> None:
    app = _app(tmp_path, "MuseCodeCuaDriverLocal.app")
    old_executable = app / "Contents/MacOS/cua-driver"
    executable = app / "Contents/MacOS/cua-driver-local"
    old_executable.rename(executable)
    plistbuddy, codesign, _spctl = _identity_tools(tmp_path)
    functions = _extract(UNINSTALL_LOCAL, "local_plist_value", "local_app_is_owned")
    environment = {
        "TEST_BUNDLE_ID": "com.meta.musecode.cua.driver.local",
        "TEST_EXECUTABLE": "cua-driver-local",
        "TEST_SIGNATURE_ID": "com.meta.musecode.cua.driver.local",
        "TEST_TEAM_ID": "not set",
        "TEST_AUTHORITY": "",
        "TEST_SIGNATURE": "adhoc",
        "TEST_REQUIREMENT": (
            'identifier "com.meta.musecode.cua.driver.local" and cdhash H"abcd"'
        ),
    }
    body = f"""
LOCAL_EXECUTABLE=cua-driver-local
local_app_is_owned '{app}' com.meta.musecode.cua.driver.local '{codesign}' '{plistbuddy}'
"""
    result = _run_shell(functions, body, environment)
    assert result.returncode == 0, result.stderr

    environment["TEST_SIGNATURE_ID"] = "com.example.lookalike"
    result = _run_shell(functions, body, environment)
    assert result.returncode != 0


def test_local_tcc_failure_is_fail_closed(tmp_path: Path) -> None:
    app = _app(tmp_path, "MuseCodeCuaDriverLocal.app")
    fake_bin = tmp_path / "bin"
    calls = tmp_path / "calls"
    fake_bin.mkdir()
    lsregister = fake_bin / "lsregister"
    _executable(lsregister, f"printf 'ls:%s\\n' \"$*\" >> '{calls}'")
    _executable(fake_bin / "tccutil", "exit 1")
    functions = _extract(UNINSTALL_LOCAL, "prepare_local_app_removal")
    body = f"""
log() {{ :; }}
RESET_TCC=1
LSREGISTER='{lsregister}'
TCCUTIL='{fake_bin / "tccutil"}'
prepare_local_app_removal '{app}' com.meta.musecode.cua.driver.local
"""
    result = _run_shell(functions, body, {"PATH": f"{fake_bin}:/usr/bin:/bin"})
    assert result.returncode != 0
    assert f"ls:-f {app}" in calls.read_text(encoding="utf-8").splitlines()
    assert "could not reset these TCC services" in result.stderr


def test_uninstall_home_guards_reject_broad_or_escaping_targets(tmp_path: Path) -> None:
    home = tmp_path / "home"
    outside = tmp_path / "outside"
    home.mkdir()
    outside.mkdir()
    (home / "escape").symlink_to(outside, target_is_directory=True)
    release_guard = _extract(UNINSTALL, "validate_release_home_dir")
    local_guard = _extract(UNINSTALL_LOCAL, "validate_local_home_dir")

    for functions, function_name in (
        (release_guard, "validate_release_home_dir"),
        (local_guard, "validate_local_home_dir"),
    ):
        result = _run_shell(functions, f'{function_name} "{home}" "{home}"', {})
        assert result.returncode != 0
        result = _run_shell(functions, f'{function_name} "{home / "escape"}" "{home}"', {})
        assert result.returncode != 0

    source = UNINSTALL.read_text(encoding="utf-8")
    assert 'rm -rf "$HOME_DIR"' not in source
    assert 'rm -rf "$LEGACY_HOME_DIR"' not in source


def test_local_root_guard_rejects_root_and_sudo() -> None:
    function = _extract(UNINSTALL_LOCAL, "reject_local_root_invocation")
    assert _run_shell(function, "reject_local_root_invocation 0 ''", {}).returncode != 0
    assert _run_shell(function, "reject_local_root_invocation 501 0", {}).returncode != 0
    assert _run_shell(function, "reject_local_root_invocation 501 ''", {}).returncode == 0


def test_shared_apps_are_reauthenticated_immediately_before_deletion() -> None:
    source = UNINSTALL.read_text(encoding="utf-8")
    removal = source.index("# --- .app bundle (macOS only) ---")
    legacy_verify = source.index(
        'macos_release_app_is_owned "$LEGACY_APP_BUNDLE"', removal
    )
    legacy_remove = source.index('rm -rf -- "$LEGACY_APP_BUNDLE"', legacy_verify)
    current_verify = source.index('macos_release_app_is_owned "$APP_BUNDLE"', legacy_remove)
    current_remove = source.index('rm -rf -- "$APP_BUNDLE"', current_verify)

    assert removal < legacy_verify < legacy_remove < current_verify < current_remove
