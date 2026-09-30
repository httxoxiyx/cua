from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

INSTALL_LOCAL = Path(__file__).resolve().parents[1] / "_install-local-rust.sh"
LOCAL_SIGNING = INSTALL_LOCAL.with_name("_local-signing.sh")
INSTALL_COMMON = INSTALL_LOCAL.with_name("_install-common.sh")
RELEASE_INSTALL = INSTALL_LOCAL.with_name("_install-rust.sh")
DISPATCHER = INSTALL_LOCAL.with_name("install-local.sh")
WINDOWS_INSTALL_LOCAL = INSTALL_LOCAL.with_name("install-local.ps1")
SKILL_PACK = INSTALL_LOCAL.parents[1] / "rust/Skills/cua-driver"


def test_local_installers_stage_the_canonical_skill_pack() -> None:
    windows = WINDOWS_INSTALL_LOCAL.read_text(encoding="utf-8")

    assert 'Join-Path $RepoRoot "Skills\\cua-driver"' in windows
    assert 'Join-Path $VersionedDir "Skills\\cua-driver"' in windows
    assert "Skills\\cua-driver-rs" not in windows
    assert "Skills/cua-driver-rs" not in INSTALL_LOCAL.read_text(encoding="utf-8")
    assert {path.name for path in SKILL_PACK.iterdir()} >= {
        "SKILL.md",
        "BROWSER.md",
        "MACOS.md",
        "WINDOWS.md",
        "LINUX.md",
    }


def _write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\n{body}", encoding="utf-8")
    path.chmod(0o755)


def _extract_local_signing_function(name: str) -> str:
    source = LOCAL_SIGNING.read_text(encoding="utf-8")
    match = re.search(rf"(?ms)^{re.escape(name)}\(\) \{{\n.*?^\}}\n", source)
    assert match, f"could not find shell function {name}"
    return match.group(0)


def _extract_local_install_function(name: str) -> str:
    source = INSTALL_LOCAL.read_text(encoding="utf-8")
    match = re.search(rf"(?ms)^{re.escape(name)}\(\) \{{\n.*?^\}}\n", source)
    assert match, f"could not find shell function {name}"
    return match.group(0)


def _extract_install_common_function(name: str) -> str:
    source = INSTALL_COMMON.read_text(encoding="utf-8")
    match = re.search(rf"(?ms)^{re.escape(name)}\(\) \{{\n.*?^\}}\n", source)
    assert match, f"could not find shell function {name}"
    return match.group(0)


def _run_local_signing_policy(body: str) -> subprocess.CompletedProcess[str]:
    functions = "\n".join(
        _extract_local_signing_function(name)
        for name in (
            "designated_requirement",
            "classify_designated_requirement",
            "local_app_bundle_value",
            "verify_local_app_identity",
        )
    )
    return subprocess.run(
        ["/bin/bash", "-c", f"set -euo pipefail\n{functions}\n{body}"],
        text=True,
        capture_output=True,
        check=False,
    )


def test_explicit_local_signing_identity_is_selected_exactly(tmp_path: Path) -> None:
    keychain = tmp_path / "signing.keychain-db"
    keychain.touch()
    fake_bin = tmp_path / "fake-bin"
    _write_executable(fake_bin / "codesign", "exit 0\n")
    wanted = "F2D26B5AFAAB910B340FBD8F480F88DF748D9D48"
    other = "A" * 40
    _write_executable(
        fake_bin / "security",
        f"printf '%s\\n' '  1) {wanted} \"Developer ID Application: Example\"' "
        f"'  2) {other} \"Developer ID Application: Renewal\"'\n",
    )
    env = os.environ.copy()
    env.update(
        {
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN": str(keychain),
            "CUA_DRIVER_LOCAL_SIGNING_IDENTITY": wanted.lower(),
        }
    )
    result = subprocess.run(
        [
            "/bin/bash",
            "-c",
            f'OS=Darwin; . "{LOCAL_SIGNING}"; ensure_local_signing_identity',
        ],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == wanted


def test_explicit_local_signing_identity_never_falls_back(tmp_path: Path) -> None:
    keychain = tmp_path / "signing.keychain-db"
    keychain.touch()
    fake_bin = tmp_path / "fake-bin"
    _write_executable(fake_bin / "codesign", "exit 0\n")
    _write_executable(
        fake_bin / "security",
        f"printf '%s\\n' '  1) {'A' * 40} \"Developer ID Application: Other\"'\n",
    )
    env = os.environ.copy()
    env.update(
        {
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN": str(keychain),
            "CUA_DRIVER_LOCAL_SIGNING_IDENTITY": "B" * 40,
        }
    )
    result = subprocess.run(
        [
            "/bin/bash",
            "-c",
            f'OS=Darwin; . "{LOCAL_SIGNING}"; ensure_local_signing_identity',
        ],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == "-"


def test_created_local_signing_key_is_scoped_to_codesign() -> None:
    source = LOCAL_SIGNING.read_text(encoding="utf-8")
    import_line = next(line for line in source.splitlines() if "security import" in line)
    continuation = source[source.index(import_line) : source.index(import_line) + 220]

    assert " -A " not in continuation
    assert "-T /usr/bin/codesign" in continuation


def test_local_app_ownership_requires_sealed_identifier_and_executable(
    tmp_path: Path,
) -> None:
    app = tmp_path / "MuseCodeCuaDriverLocal.app"
    binary = app / "Contents/MacOS/cua-driver-local"
    binary.parent.mkdir(parents=True)
    (app / "Contents/Info.plist").write_text("fixture")
    binary.write_text("fixture")
    binary.chmod(0o755)
    result = _run_local_signing_policy(
        f"""
        local_app_bundle_value() {{
            [[ "$2" == CFBundleIdentifier ]] \
                && printf '%s' com.meta.musecode.cua.driver.local \
                || printf '%s' cua-driver-local
        }}
        designated_requirement() {{ printf '%s' 'identifier "com.meta.musecode.cua.driver.local" and cdhash H"1234"'; }}
        codesign() {{
            [[ "$*" == *'-R =identifier "com.meta.musecode.cua.driver.local"'* ]] \
                || [[ "$*" != *' -R '* ]]
        }}
        verify_local_app_identity '{app}' \
            com.meta.musecode.cua.driver.local cua-driver-local
        """
    )

    assert result.returncode == 0, result.stderr


def test_local_app_ownership_rejects_signature_failure(tmp_path: Path) -> None:
    app = tmp_path / "CuaDriverLocal.app"
    binary = app / "Contents/MacOS/cua-driver-local"
    binary.parent.mkdir(parents=True)
    (app / "Contents/Info.plist").write_text("fixture")
    binary.write_text("fixture")
    binary.chmod(0o755)
    result = _run_local_signing_policy(
        f"""
        local_app_bundle_value() {{
            [[ "$2" == CFBundleIdentifier ]] \
                && printf '%s' com.trycua.driver.local \
                || printf '%s' cua-driver-local
        }}
        codesign() {{ return 1; }}
        ! verify_local_app_identity '{app}' com.trycua.driver.local cua-driver-local
        """
    )

    assert result.returncode == 0, result.stderr


def test_macos_local_install_rolls_back_on_registration_or_tcc_failure() -> None:
    source = INSTALL_LOCAL.read_text(encoding="utf-8")
    install = source.index('if [ "$OS" = "Darwin" ]; then')
    trap = source.index("trap local_install_exit EXIT")
    staged_verify = source.index(
        'if ! verify_local_app_identity "$APP_STAGE"', install
    )
    daemon_stop = source.index("stop_local_daemons_before_identity_check", staged_verify)
    history_guard = source.index(
        "refuse_local_history_identity_transition", daemon_stop
    )
    swap = source.index('&& ! mv "$APP_DEST" "$APP_BACKUP"', history_guard)
    register = source.index('if ! register_local_app "$APP_DEST"', install)
    reset = source.index("reset_local_tcc_after_requirement_change", register)
    legacy_prepare = source.index("prepare_legacy_local_app_removal", reset)
    legacy_stage = source.index(
        'mv "$LEGACY_LOCAL_APP" "$LEGACY_LOCAL_APP_BACKUP"', legacy_prepare
    )
    commit = source.index("LOCAL_APP_INSTALL_COMMITTED=1", reset)
    delete_backup = source.index("remove_authenticated_local_app_backup", commit)
    autostart = source.index('if [ "$INSTALL_AUTOSTART" = true ]', commit)

    assert (
        trap
        < staged_verify
        < daemon_stop
        < history_guard
        < swap
        < register
        < reset
        < legacy_prepare
        < legacy_stage
        < commit
        < delete_backup
        < autostart
    )
    assert "trap 'exit 130' INT" in source
    assert "trap 'exit 143' TERM" in source
    assert "restore_local_app_backup" in source
    assert "rollback_legacy_local_app_on_exit" in source
    assert '&& ! mv "$APP_DEST" "$APP_BACKUP"' in source
    assert source.index("LOCAL_APP_SWAP_STARTED=1", history_guard) < swap
    assert 'LEGACY_LOCAL_APP_OWNED:-0' in source


def test_local_launchagent_quiescence_boots_out_keepalive_or_fails_closed(
    tmp_path: Path,
) -> None:
    functions = "\n".join(
        _extract_local_signing_function(name)
        for name in ("local_launchagent_state", "stop_and_verify_local_launchagent")
    )
    state = tmp_path / "loaded"
    state.write_text("loaded\n", encoding="utf-8")
    calls = tmp_path / "calls"
    launchctl = tmp_path / "launchctl"
    _write_executable(
        launchctl,
        'printf "%s\\n" "$*" >> "$TEST_CALLS"\n'
        'case "$1" in\n'
        '  print) test -e "$TEST_STATE" && exit 0 || exit 113 ;;\n'
        '  unload) exit 70 ;;\n'
        '  bootout) test "${TEST_BOOTOUT_FAIL:-0}" = 1 && exit 71; rm -f "$TEST_STATE" ;;\n'
        'esac\n',
    )
    plist = tmp_path / "local.plist"
    plist.write_text("fixture\n", encoding="utf-8")
    command = f"""set -euo pipefail
    {functions}
    OS=Darwin
    CUA_DRIVER_LAUNCHCTL='{launchctl}'
    stop_and_verify_local_launchagent '{plist}' com.trycua.cua-driver-local
    """
    env = {
        **os.environ,
        "TEST_STATE": str(state),
        "TEST_CALLS": str(calls),
    }
    result = subprocess.run(
        ["/bin/bash", "-c", command], env=env, text=True, capture_output=True
    )
    assert result.returncode == 0, result.stderr
    assert not state.exists()
    assert "bootout gui/" in calls.read_text(encoding="utf-8")

    state.write_text("loaded\n", encoding="utf-8")
    failed = subprocess.run(
        ["/bin/bash", "-c", command],
        env={**env, "TEST_BOOTOUT_FAIL": "1"},
        text=True,
        capture_output=True,
    )
    assert failed.returncode != 0
    assert state.exists()
    assert "could not stop loaded local launchd job" in failed.stderr


def test_local_systemd_quiescence_requires_stop_and_verified_inactive_state(
    tmp_path: Path,
) -> None:
    functions = "\n".join(
        _extract_local_signing_function(name)
        for name in (
            "local_systemd_active_state",
            "stop_and_verify_local_systemd_service",
        )
    )
    systemctl = tmp_path / "systemctl"
    state = tmp_path / "state"
    state.write_text("enabled\n", encoding="utf-8")
    _write_executable(
        systemctl,
        'case "$*" in\n'
        '  *"is-active --quiet"*) test "$(cat "$TEST_STATE")" = enabled && exit 0 || exit 3 ;;\n'
        '  *" stop "*) test "${TEST_STOP_FAIL:-0}" = 1 && exit 70; printf "disabled\\n" > "$TEST_STATE" ;;\n'
        'esac\n',
    )
    command = f"""set -euo pipefail
    {functions}
    stop_and_verify_local_systemd_service cua-driver-local.service '{tmp_path / "unit"}'
    """
    env = {
        **os.environ,
        "PATH": f"{tmp_path}:/usr/bin:/bin",
        "TEST_STATE": str(state),
    }
    result = subprocess.run(
        ["/bin/bash", "-c", command], env=env, text=True, capture_output=True
    )
    assert result.returncode == 0, result.stderr
    assert state.read_text(encoding="utf-8").strip() == "disabled"

    state.write_text("enabled\n", encoding="utf-8")
    failed = subprocess.run(
        ["/bin/bash", "-c", command],
        env={**env, "TEST_STOP_FAIL": "1"},
        text=True,
        capture_output=True,
    )
    assert failed.returncode != 0
    assert "could not stop" in failed.stderr


def test_linux_local_installer_quiesces_before_replacing_runtime() -> None:
    source = INSTALL_LOCAL.read_text(encoding="utf-8")
    quiesce = source.index(
        '&& ! stop_local_linux_daemons_before_runtime_change'
    )
    first_stage = source.index('stage_binary "$BUILT_BINARY"')
    second_quiesce = source.index(
        "stop_local_linux_daemons_before_runtime_change", first_stage
    )
    autostart = source.index('if [ "$INSTALL_AUTOSTART" = true ]', second_quiesce)

    assert quiesce < first_stage < second_quiesce < autostart


def test_release_linux_shutdown_covers_current_and_legacy_systemd_units() -> None:
    source = INSTALL_COMMON.read_text(encoding="utf-8")
    stop = _extract_install_common_function("stop_cua_driver_daemons")
    release = RELEASE_INSTALL.read_text(encoding="utf-8")
    linux_branch = release.index("# Linux: versioned-dirs + atomic `current` symlink swap.")
    quiesce = release.index("if ! stop_cua_driver_daemons", linux_branch)
    install_binary = release.index('install -m 0755 "$SRC"', quiesce)

    assert "cua-driver.service" in stop
    assert "cua-driver-rs.service" in stop
    assert stop.count("stop_and_verify_cua_systemd_service") == 2
    assert "|| true" not in "\n".join(
        line for line in stop.splitlines() if "systemctl" in line
    )
    assert quiesce < install_binary


def test_release_systemd_quiescence_fails_when_stop_fails(tmp_path: Path) -> None:
    functions = "\n".join(
        _extract_install_common_function(name)
        for name in (
            "cua_systemd_active_state",
            "stop_and_verify_cua_systemd_service",
        )
    )
    systemctl = tmp_path / "systemctl"
    _write_executable(
        systemctl,
        'case "$*" in\n'
        '  *"is-active --quiet"*) exit 0 ;;\n'
        '  *" stop "*) exit 70 ;;\n'
        'esac\n',
    )
    result = subprocess.run(
        [
            "/bin/bash",
            "-c",
            f"""set -euo pipefail
            {functions}
            stop_and_verify_cua_systemd_service cua-driver.service '{tmp_path / "unit"}'
            """,
        ],
        env={**os.environ, "PATH": f"{tmp_path}:/usr/bin:/bin"},
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "could not stop" in result.stderr


def test_interrupted_local_install_restores_staged_legacy_app(tmp_path: Path) -> None:
    function = _extract_local_install_function("rollback_legacy_local_app_on_exit")
    legacy = tmp_path / "CuaDriverLocal.app"
    backup = tmp_path / "CuaDriverLocal.app.install-backup"
    backup.mkdir()
    registration = tmp_path / "registered"
    result = subprocess.run(
        [
            "/bin/bash",
            "-c",
            f"""set -euo pipefail
            {function}
            verify_local_app_identity() {{ return 0; }}
            register_legacy_local_app() {{ printf registered > '{registration}'; }}
            remove_authenticated_local_app_backup() {{ return 70; }}
            LEGACY_LOCAL_APP_REMOVAL_STARTED=1
            LOCAL_APP_INSTALL_COMMITTED=0
            LEGACY_LOCAL_APP='{legacy}'
            LEGACY_LOCAL_APP_BACKUP='{backup}'
            rollback_legacy_local_app_on_exit
            [[ -d "$LEGACY_LOCAL_APP" ]]
            [[ ! -e "$LEGACY_LOCAL_APP_BACKUP" ]]
            [[ "$LEGACY_LOCAL_APP_REMOVAL_STARTED" == 0 ]]
            """,
        ],
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    assert legacy.is_dir()
    assert registration.read_text(encoding="utf-8") == "registered"


@pytest.mark.parametrize(
    ("effective_uid", "sudo_user"),
    [("0", ""), ("501", "developer")],
    ids=["root", "sudo"],
)
def test_local_installer_rejects_root_or_sudo_before_build(
    tmp_path: Path, effective_uid: str, sudo_user: str
) -> None:
    fixture_root = tmp_path / "cua-driver"
    scripts_dir = fixture_root / "scripts"
    rust_dir = fixture_root / "rust"
    scripts_dir.mkdir(parents=True)
    rust_dir.mkdir()
    shutil.copy2(INSTALL_LOCAL, scripts_dir / INSTALL_LOCAL.name)
    shutil.copy2(LOCAL_SIGNING, scripts_dir / LOCAL_SIGNING.name)
    fake_bin = tmp_path / "fake-bin"
    _write_executable(fake_bin / "id", f"printf '%s\\n' {effective_uid}\n")
    cargo_called = tmp_path / "cargo-called"
    _write_executable(fake_bin / "cargo", f"touch '{cargo_called}'\n")
    env = os.environ.copy()
    env.update(
        {
            "HOME": str(tmp_path / "home"),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_SOURCE_SHA": "a" * 40,
        }
    )
    if sudo_user:
        env["SUDO_USER"] = sudo_user
    else:
        env.pop("SUDO_USER", None)

    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / INSTALL_LOCAL.name)],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 1
    assert "do not run this script with sudo or as root" in result.stdout
    assert not cargo_called.exists()


def test_local_install_home_must_be_private_and_not_release_owned(
    tmp_path: Path,
) -> None:
    function = _extract_local_install_function("validate_local_install_home_dir")
    home = tmp_path / "home"
    home.mkdir()
    valid = home / ".cua-driver-local"
    valid.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    escape = home / "escape"
    escape.symlink_to(outside, target_is_directory=True)
    packages_escape = home / "packages-escape"
    packages_escape.mkdir()
    (packages_escape / "packages").symlink_to(outside, target_is_directory=True)
    result = subprocess.run(
        [
            "/bin/bash",
            "-c",
            f"""set -euo pipefail
            {function}
            validate_local_install_home_dir '{valid}' '{home}'
            ! validate_local_install_home_dir / '{home}'
            ! validate_local_install_home_dir '{home}' '{home}'
            ! validate_local_install_home_dir relative '{home}'
            ! validate_local_install_home_dir '{outside}' '{home}'
            ! validate_local_install_home_dir '{home}/.cua-driver' '{home}'
            ! validate_local_install_home_dir '{escape}/nested' '{home}'
            ! validate_local_install_home_dir '{packages_escape}' '{home}'
            """,
        ],
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize("relative_target", [False, True], ids=["absolute", "relative"])
def test_installer_stages_binary_from_custom_cargo_target(
    tmp_path: Path, relative_target: bool
) -> None:
    fixture_root = tmp_path / "cua-driver"
    scripts_dir = fixture_root / "scripts"
    rust_dir = fixture_root / "rust"
    scripts_dir.mkdir(parents=True)
    rust_dir.mkdir()
    shutil.copy2(INSTALL_LOCAL, scripts_dir / INSTALL_LOCAL.name)
    shutil.copy2(LOCAL_SIGNING, scripts_dir / LOCAL_SIGNING.name)

    wayland_helper = fixture_root / "wayland-helper/winrects@cua"
    wayland_helper.mkdir(parents=True)
    (wayland_helper / "metadata.json").write_text('{"version":5}\n', encoding="utf-8")
    (wayland_helper / "extension.js").write_text("// semantic cursor v5\n", encoding="utf-8")

    stale_binary = rust_dir / "target/release/cua-driver"
    _write_executable(stale_binary, "printf 'stale workspace target\\n'")

    custom_target = (
        rust_dir / "relative custom target" if relative_target else tmp_path / "custom target"
    )
    cargo_target_dir = (
        str(custom_target.relative_to(rust_dir)) if relative_target else str(custom_target)
    )
    fake_bin = tmp_path / "fake-bin"
    _write_executable(
        fake_bin / "cargo",
        """set -eu
test "${1:-}" = build
test "$CARGO_TARGET_DIR" = "$EXPECTED_CARGO_TARGET_DIR"
mkdir -p "$CARGO_TARGET_DIR/release"
printf 'fresh custom target\n' > "$CARGO_TARGET_DIR/release/cua-driver"
printf 'fresh cursor theme compiler\n' > "$CARGO_TARGET_DIR/release/cua-cursor-theme"
chmod +x "$CARGO_TARGET_DIR/release/cua-driver"
chmod +x "$CARGO_TARGET_DIR/release/cua-cursor-theme"
""",
    )
    _write_executable(
        fake_bin / "uname",
        """case "${1:-}" in
    -s) printf 'Linux\n' ;;
    -m) printf 'x86_64\n' ;;
    *) exit 2 ;;
esac
""",
    )
    _write_executable(
        fake_bin / "systemctl",
        'case "$*" in *"is-active --quiet"*) exit 4 ;; *"is-enabled"*) printf "not-found\\n"; exit 1 ;; *) exit 0 ;; esac\n',
    )
    _write_executable(fake_bin / "pkill", "exit 0")

    user_home = tmp_path / "home"
    local_home = user_home / ".cua-driver-local"
    installed_helper = user_home / ".local/share/gnome-shell/extensions/winrects@cua"
    installed_helper.mkdir(parents=True)
    (installed_helper / "metadata.json").write_text('{"version":4}\n', encoding="utf-8")
    (installed_helper / "extension.js").write_text("// legacy cursor\n", encoding="utf-8")
    install_bin = tmp_path / "install-bin"
    env = os.environ.copy()
    env.pop("SUDO_USER", None)
    env.update(
        {
            "HOME": str(user_home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CARGO_TARGET_DIR": cargo_target_dir,
            "EXPECTED_CARGO_TARGET_DIR": str(custom_target),
            "CUA_DRIVER_SOURCE_SHA": "a" * 40,
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
            "CUA_DRIVER_LOCAL_INSTALL_DIR": str(install_bin),
        }
    )

    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / INSTALL_LOCAL.name), "--release"],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert (custom_target / "release/cua-driver").read_text() == "fresh custom target\n"
    assert (
        custom_target / "release/cua-cursor-theme"
    ).read_text() == "fresh cursor theme compiler\n"
    assert (install_bin / "cua-driver-local").read_text() == "fresh custom target\n"
    assert (
        local_home / "packages/current/cua-cursor-theme"
    ).read_text() == "fresh cursor theme compiler\n"
    assert (
        local_home / "packages/current/wayland-helper/winrects@cua/metadata.json"
    ).read_text() == '{"version":5}\n'
    assert (installed_helper / "metadata.json").read_text() == '{"version":5}\n'
    assert (installed_helper / "extension.js").read_text() == "// semantic cursor v5\n"


def _linux_fixture(tmp_path: Path) -> tuple[Path, Path, dict[str, str]]:
    """Stage a minimal Linux install-local fixture: (scripts_dir, fake_bin, env)."""
    fixture_root = tmp_path / "cua-driver"
    scripts_dir = fixture_root / "scripts"
    rust_dir = fixture_root / "rust"
    scripts_dir.mkdir(parents=True)
    rust_dir.mkdir()
    for script in (INSTALL_LOCAL, LOCAL_SIGNING, DISPATCHER):
        shutil.copy2(script, scripts_dir / script.name)

    fake_bin = tmp_path / "fake-bin"
    _write_executable(
        fake_bin / "cargo",
        """set -eu
mkdir -p "$CARGO_TARGET_DIR/debug"
printf 'fresh driver\n' > "$CARGO_TARGET_DIR/debug/cua-driver"
printf 'fresh cursor theme compiler\n' > "$CARGO_TARGET_DIR/debug/cua-cursor-theme"
chmod +x "$CARGO_TARGET_DIR/debug/cua-driver"
chmod +x "$CARGO_TARGET_DIR/debug/cua-cursor-theme"
""",
    )
    _write_executable(
        fake_bin / "uname",
        """case "${1:-}" in
    -s) printf 'Linux\n' ;;
    -m) printf 'x86_64\n' ;;
    *) exit 2 ;;
esac
""",
    )
    _write_executable(
        fake_bin / "systemctl",
        'case "$*" in *"is-active --quiet"*) exit 4 ;; *"is-enabled"*) printf "not-found\\n"; exit 1 ;; *) exit 0 ;; esac\n',
    )
    _write_executable(fake_bin / "pkill", "exit 0")

    user_home = tmp_path / "home"
    user_home.mkdir()
    env = os.environ.copy()
    env.pop("SUDO_USER", None)
    env.pop("CARGO_TARGET_DIR", None)
    env.pop("CUA_DRIVER_LOCAL_INSTALL_DIR", None)
    env.update(
        {
            "HOME": str(user_home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_SOURCE_SHA": "a" * 40,
            "CUA_DRIVER_LOCAL_HOME": str(user_home / ".cua-driver-local"),
        }
    )
    return scripts_dir, fake_bin, env


@pytest.mark.parametrize(
    "flag_form",
    [["--bin-dir", "{bin}"], ["--bin-dir={bin}"]],
    ids=["separate", "equals"],
)
def test_dispatcher_forwards_bin_dir_override(tmp_path: Path, flag_form: list[str]) -> None:
    """--bin-dir is documented by install-local.sh and forwarded verbatim; the
    helper must accept both spellings and honor them over the env default."""
    scripts_dir, _, env = _linux_fixture(tmp_path)
    flag_bin = tmp_path / "flag-bin"
    env["CUA_DRIVER_LOCAL_INSTALL_DIR"] = str(tmp_path / "env-bin")
    args = [arg.format(bin=flag_bin) for arg in flag_form]

    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), *args],
        cwd=scripts_dir.parent,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert (flag_bin / "cua-driver-local").read_text() == "fresh driver\n"
    assert not (tmp_path / "env-bin").exists()


def test_relative_bin_dir_is_rejected(tmp_path: Path) -> None:
    """A relative bin dir would land inside the Cargo workspace (the symlink is
    created after cd'ing there) and uninstall-local.sh could never remove it."""
    scripts_dir, _, env = _linux_fixture(tmp_path)

    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--bin-dir", "relative/bin"],
        cwd=scripts_dir.parent,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 2, result.stdout + result.stderr
    assert "absolute path" in result.stderr


@pytest.mark.skipif(
    not sys.platform.startswith("linux"), reason="ETXTBSY on a running binary is Linux-specific"
)
def test_reinstall_over_a_running_driver(tmp_path: Path) -> None:
    """Staging must replace the versioned binary by rename, not write through it.

    The version tag is stable per build config, so every rebuild targets the
    same path. If a previous cua-driver-local is still executing out of it, a
    write-in-place `cp` fails with ETXTBSY ("Text file busy") and the install
    dies mid-stage. Reproduce that with a real running executable.
    """
    scripts_dir, _, env = _linux_fixture(tmp_path)
    versioned = (
        Path(env["CUA_DRIVER_LOCAL_HOME"])
        / "packages/releases/0.0.0-local-debug-x86_64-unknown-linux-gnu"
    )
    versioned.mkdir(parents=True)
    busy = versioned / "cua-driver-local"
    shutil.copy2("/bin/sleep", busy)

    running = subprocess.Popen([str(busy), "60"])
    try:
        result = subprocess.run(
            ["/bin/bash", str(scripts_dir / DISPATCHER.name)],
            cwd=scripts_dir.parent,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
    finally:
        running.terminate()
        running.wait(timeout=10)

    assert result.returncode == 0, result.stdout + result.stderr
    assert "Text file busy" not in result.stderr
    assert busy.read_text() == "fresh driver\n"
    # The rename must not leave the temp file behind.
    assert not list(versioned.glob("*.stage.*"))


def test_bin_dir_without_value_is_rejected(tmp_path: Path) -> None:
    scripts_dir, _, env = _linux_fixture(tmp_path)

    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--bin-dir"],
        cwd=scripts_dir.parent,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 2, result.stdout + result.stderr
    assert "--bin-dir requires a value" in result.stderr
