from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[4]
UNINSTALL_LOCAL = REPO_ROOT / "libs/cua-driver/scripts/uninstall-local.sh"
SCRIPTS = REPO_ROOT / "libs/cua-driver/scripts"


def _executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\n{body}", encoding="utf-8")
    path.chmod(0o755)


def test_unix_local_uninstall_removes_owned_links_and_preserves_release(tmp_path: Path) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    release_home = home / ".cua-driver"
    local_bin = home / ".local/bin"
    local_cache = home / ".cache/cua-driver-local"
    release_cache = home / ".cache/cua-driver"
    for path in (fake_bin, local_home, release_home, local_bin, local_cache, release_cache):
        path.mkdir(parents=True, exist_ok=True)

    local_marker = local_home / "packages/current/cua-driver-local"
    local_marker.parent.mkdir(parents=True)
    local_marker.write_text("local\n", encoding="utf-8")
    for runtime_file in (
        ".telemetry_id",
        ".telemetry_identity.lock",
        ".telemetry_lifecycle.lock",
        ".telemetry_retry_after",
        "version_check.json",
    ):
        (local_home / runtime_file).write_text("local runtime state\n", encoding="utf-8")
    # Per-release markers written by builds that still had telemetry.
    release_marker = local_home / ".release_installed/0.23.2"
    release_marker.parent.mkdir(parents=True)
    release_marker.write_text("local runtime state\n", encoding="utf-8")

    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "exit 1")
    _executable(
        fake_bin / "systemctl",
        'case "$*" in\n'
        '  *"is-active --quiet"*) exit 3 ;;\n'
        '  *"is-enabled"*) printf "disabled\\n"; exit 1 ;;\n'
        '  *) exit 0 ;;\n'
        'esac\n',
    )

    local_cli = local_bin / "cua-driver-local"
    local_cli.symlink_to(local_home / "packages/current/cua-driver-local")
    release_cli = local_bin / "cua-driver"
    release_cli.symlink_to(release_home / "packages/current/cua-driver")

    local_skill = home / ".agents/skills/cua-driver"
    local_skill.parent.mkdir(parents=True)
    local_skill.symlink_to(local_home / "skills/cua-driver")

    local_unit = home / ".config/systemd/user/cua-driver-local.service"
    release_unit = home / ".config/systemd/user/cua-driver.service"
    local_unit.parent.mkdir(parents=True)
    local_unit.write_text("local\n", encoding="utf-8")
    release_unit.write_text("release\n", encoding="utf-8")

    claude_json = home / ".claude.json"
    claude_json.write_text(
        json.dumps(
            {
                "mcpServers": {
                    "local": {"command": str(local_cli), "args": ["mcp"]},
                    "release": {"command": str(release_cli), "args": ["mcp"]},
                }
            }
        ),
        encoding="utf-8",
    )

    env = os.environ.copy()
    env.update(
        {
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
            "CUA_DRIVER_LOCAL_INSTALL_DIR": str(local_bin),
        }
    )
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr

    assert not local_cli.exists() and not local_cli.is_symlink()
    assert not local_skill.exists() and not local_skill.is_symlink()
    assert not local_home.exists()
    assert not local_cache.exists()
    assert not local_unit.exists()

    assert release_cli.is_symlink()
    assert release_home.exists()
    assert release_cache.exists()
    assert release_unit.exists()
    remaining = json.loads(claude_json.read_text(encoding="utf-8"))["mcpServers"]
    assert set(remaining) == {"release"}


def test_unix_local_uninstall_keeps_shared_skill_link_owned_by_release(tmp_path: Path) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    release_skill_target = home / ".cua-driver/skills/cua-driver"
    release_skill_target.mkdir(parents=True)
    skill_link = home / ".agents/skills/cua-driver"
    skill_link.parent.mkdir(parents=True)
    skill_link.symlink_to(release_skill_target)
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "exit 1")

    env = os.environ.copy()
    env.update({"HOME": str(home), "PATH": f"{fake_bin}:/usr/bin:/bin"})
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert skill_link.is_symlink()
    assert skill_link.resolve() == release_skill_target


def test_local_uninstall_contract_is_explicit_on_both_platforms() -> None:
    unix = (SCRIPTS / "uninstall-local.sh").read_text()
    signing = (SCRIPTS / "_local-signing.sh").read_text()
    installer = (SCRIPTS / "_install-local-rust.sh").read_text()
    windows = (SCRIPTS / "uninstall-local.ps1").read_text(encoding="utf-8-sig")

    for token in (
        "/Applications/MuseCodeCuaDriverLocal.app",
        "/Applications/CuaDriverLocal.app",
        "com.meta.musecode.cua.driver.local",
        "com.trycua.driver.local",
        ".cua-driver-local",
        "cua-driver-local.service",
        "com.trycua.cua-driver-local.plist",
    ):
        assert token in unix
    for token in (
        "cua-driver-local-serve",
        "cua-driver-uia-local",
        ".cua-driver-local",
        "Programs\\Cua\\cua-driver-local\\bin",
    ):
        assert token in windows
    assert "Test-LocalLinkTarget" in windows
    assert "is_local_target" in unix
    assert "LOCAL_HOME_MARKER" in unix
    assert "local_app_is_owned" in unix
    assert "prepare_local_app_removal" in unix
    assert "remove_verified_local_app" in unix
    assert '"$APP_BUNDLE"/Contents/MacOS/cua-driver-local' in unix
    assert "reject_local_root_invocation" in unix
    assert "stop_verified_local_daemons" in unix
    assert "local_daemon_process_generation" in unix
    assert 'lsof_tool="${CUA_DRIVER_LSOF:-/usr/sbin/lsof}"' in signing
    assert 'stop_verified_local_processes 0 "${owned_paths[@]}"' in installer
    assert "refuse_local_history_identity_transition" in unix
    assert "pkill -f" not in unix
    history_guard = unix.index(
        'refuse_local_history_identity_transition "$LOCAL_HISTORY_ROOT"'
    )
    daemon_stop = unix.index("if ! stop_verified_local_daemons", history_guard)
    stopped_history_guard = unix.index(
        'refuse_local_history_identity_transition "$LOCAL_HISTORY_ROOT"',
        daemon_stop,
    )
    tcc_cleanup = unix.index("prepare_local_app_removal", stopped_history_guard)
    app_removal = unix.index("remove_verified_local_app", tcc_cleanup)
    assert history_guard < daemon_stop < stopped_history_guard < tcc_cleanup < app_removal
    assert "$LocalHomeMarker" in windows
    assert "Remove-Item -LiteralPath $HomeDir -Force -Recurse" not in windows
    assert 'rm -rf "$HOME_DIR"' not in unix
    assert "--validate-only" in unix
    assert "$ValidateOnly" in windows
    for token in (
        ".telemetry_identity.lock",
        ".telemetry_lifecycle.lock",
        ".telemetry_retry_after",
        ".release_installed",
    ):
        assert token in unix
        assert token in windows


def test_unix_local_uninstall_rejects_release_home_override(tmp_path: Path) -> None:
    home = tmp_path / "home"
    home.mkdir()
    env = os.environ.copy()
    env.update(
        {
            "HOME": str(home),
            "CUA_DRIVER_LOCAL_HOME": str(home / ".cua-driver"),
        }
    )
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--validate-only"],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 2
    assert "release-owned local home" in result.stderr


def test_unix_local_uninstall_aborts_before_cleanup_when_process_inspection_fails(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    local_cache = home / ".cache/cua-driver-local"
    local_cache.mkdir(parents=True)
    sentinel = local_cache / "preserve-me"
    sentinel.write_text("present", encoding="utf-8")
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", f"printf '%s\\n' {os.getpid()}\n")
    _executable(fake_bin / "ps", "exit 70")

    env = os.environ.copy()
    env.update(
        {
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
        }
    )
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "process inspection failed" in result.stderr
    assert sentinel.read_text() == "present"


def test_local_uninstall_fails_closed_when_systemd_service_remains_active(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    local_cache = home / ".cache/cua-driver-local"
    unit = home / ".config/systemd/user/cua-driver-local.service"
    unit.parent.mkdir(parents=True)
    unit.write_text("keepalive\n", encoding="utf-8")
    local_cache.mkdir(parents=True)
    sentinel = local_cache / "preserve-me"
    sentinel.write_text("present", encoding="utf-8")
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "exit 1")
    _executable(
        fake_bin / "systemctl",
        'case "$*" in\n'
        '  *"disable --now"*) exit 0 ;;\n'
        '  *"is-active --quiet"*) exit 0 ;;\n'
        '  *"is-enabled"*) printf "disabled\\n"; exit 1 ;;\n'
        '  *) exit 0 ;;\n'
        'esac\n',
    )
    env = {
        **os.environ,
        "HOME": str(home),
        "PATH": f"{fake_bin}:/usr/bin:/bin",
        "CUA_DRIVER_LOCAL_HOME": str(local_home),
    }
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "remains active, enabled, or unverifiable" in result.stderr
    assert unit.exists()
    assert sentinel.read_text(encoding="utf-8") == "present"


def test_local_uninstall_requires_successful_systemd_disable(tmp_path: Path) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    unit = home / ".config/systemd/user/cua-driver-local.service"
    marker = local_home / "packages/current/cua-driver-local"
    marker.parent.mkdir(parents=True)
    marker.write_text("driver\n", encoding="utf-8")
    unit.parent.mkdir(parents=True)
    unit.write_text("restart\n", encoding="utf-8")
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "exit 1")
    _executable(
        fake_bin / "systemctl",
        'case "$*" in\n'
        '  *"is-active --quiet"*) exit 0 ;;\n'
        '  *"is-enabled"*) printf "enabled\\n"; exit 0 ;;\n'
        '  *"disable --now"*) exit 70 ;;\n'
        '  *) exit 0 ;;\n'
        'esac\n',
    )
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env={
            **os.environ,
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
        },
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "could not disable and stop" in result.stderr
    assert unit.exists()
    assert marker.exists()


def test_local_uninstall_fails_if_systemd_unit_removal_fails(tmp_path: Path) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    unit = home / ".config/systemd/user/cua-driver-local.service"
    marker = local_home / "packages/current/cua-driver-local"
    marker.parent.mkdir(parents=True)
    marker.write_text("driver\n", encoding="utf-8")
    unit.parent.mkdir(parents=True)
    unit.write_text("restart\n", encoding="utf-8")
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "exit 1")
    _executable(
        fake_bin / "systemctl",
        'case "$*" in\n'
        '  *"is-active --quiet"*) exit 3 ;;\n'
        '  *"is-enabled"*) printf "disabled\\n"; exit 1 ;;\n'
        '  *) exit 0 ;;\n'
        'esac\n',
    )
    _executable(fake_bin / "rm", 'exit 70\n')
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env={
            **os.environ,
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
        },
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "could not remove systemd user unit" in result.stderr
    assert unit.exists()
    assert marker.exists()


def test_local_uninstall_detects_systemd_respawn_after_reload(tmp_path: Path) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    unit = home / ".config/systemd/user/cua-driver-local.service"
    marker = local_home / "packages/current/cua-driver-local"
    state = tmp_path / "service-state"
    marker.parent.mkdir(parents=True)
    marker.write_text("driver\n", encoding="utf-8")
    unit.parent.mkdir(parents=True)
    unit.write_text("restart\n", encoding="utf-8")
    state.write_text("enabled\n", encoding="utf-8")
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "exit 1")
    _executable(
        fake_bin / "systemctl",
        'case "$*" in\n'
        '  *"disable --now"*) printf "disabled\\n" > "$TEST_STATE" ;;\n'
        '  *"daemon-reload"*) printf "respawned\\n" > "$TEST_STATE" ;;\n'
        '  *"is-active --quiet"*) test "$(cat "$TEST_STATE")" = respawned && exit 0 || test "$(cat "$TEST_STATE")" = enabled && exit 0 || exit 3 ;;\n'
        '  *"is-enabled"*) test "$(cat "$TEST_STATE")" = enabled && printf "enabled\\n" && exit 0; printf "disabled\\n"; exit 1 ;;\n'
        'esac\n',
    )
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env={
            **os.environ,
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
            "TEST_STATE": str(state),
        },
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "reappeared" in result.stderr
    assert marker.exists()


def test_unix_local_uninstall_terminates_and_verifies_owned_daemon(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    release = local_home / "packages/releases/test"
    release.mkdir(parents=True)
    daemon = release / "cua-driver-local"
    shutil.copyfile("/bin/sleep", daemon)
    daemon.chmod(0o755)
    current = local_home / "packages/current"
    current.symlink_to(release)
    pid_file = tmp_path / "daemon.pid"
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(
        fake_bin / "pgrep",
        'test -r "$TEST_DAEMON_PID_FILE" || exit 1\n'
        'pid="$(cat "$TEST_DAEMON_PID_FILE")"\n'
        'kill -0 "$pid" 2>/dev/null || exit 1\n'
        'printf "%s\\n" "$pid"\n',
    )

    wrapper = subprocess.Popen(
        [
            "/bin/sh",
            "-c",
            f"'{daemon}' 60 & printf '%s\\n' $! > '{pid_file}'; wait",
        ]
    )
    try:
        deadline = time.monotonic() + 5
        while not pid_file.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        assert pid_file.exists()
        daemon_pid = int(pid_file.read_text())
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(home),
                "PATH": f"{fake_bin}:/usr/bin:/bin",
                "CUA_DRIVER_LOCAL_HOME": str(local_home),
                "TEST_DAEMON_PID_FILE": str(pid_file),
            }
        )
        result = subprocess.run(
            ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=15,
        )

        assert result.returncode == 0, result.stdout + result.stderr
        with pytest.raises(ProcessLookupError):
            os.kill(daemon_pid, 0)
        assert not local_home.exists()
    finally:
        if wrapper.poll() is None:
            wrapper.terminate()
        wrapper.wait(timeout=5)


def test_unix_local_uninstall_preserves_state_when_owned_daemon_survives(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    fake_bin = tmp_path / "fake-bin"
    local_home = home / ".cua-driver-local"
    marker = local_home / "packages/current/cua-driver-local"
    marker.parent.mkdir(parents=True)
    marker.write_text("driver", encoding="utf-8")
    local_cache = home / ".cache/cua-driver-local"
    local_cache.mkdir(parents=True)
    sentinel = local_cache / "preserve-me"
    sentinel.write_text("present", encoding="utf-8")
    bash_env = tmp_path / "bash-env"
    bash_env.write_text(
        'kill() { case "$1" in -0) return 0 ;; *) return 1 ;; esac; }\n',
        encoding="utf-8",
    )
    _executable(fake_bin / "uname", "printf 'Linux\\n'")
    _executable(fake_bin / "id", "printf '501\\n'")
    _executable(fake_bin / "pgrep", "printf '4242\\n'")
    _executable(
        fake_bin / "ps",
        "case \"$*\" in\n"
        "  *lstart=*) printf 'Mon Sep 30 12:00:00 2026\\n' ;;\n"
        "  *comm=*) printf '%s\\n' \"$TEST_DAEMON_IDENTITY\" ;;\n"
        "  *) exit 70 ;;\n"
        "esac\n",
    )

    env = os.environ.copy()
    env.update(
        {
            "HOME": str(home),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "BASH_ENV": str(bash_env),
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
            "TEST_DAEMON_IDENTITY": str(marker),
        }
    )
    result = subprocess.run(
        ["/bin/bash", str(UNINSTALL_LOCAL), "--force", "--keep-tcc"],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
        timeout=15,
    )

    assert result.returncode != 0
    assert "verified local cua-driver process remains" in result.stderr
    assert sentinel.read_text() == "present"
    assert marker.read_text() == "driver"


def test_release_uninstallers_do_not_target_local_identity() -> None:
    for name in ("uninstall.sh", "uninstall.ps1"):
        script = (SCRIPTS / name).read_text(encoding="utf-8-sig")
        assert "MuseCodeCuaDriverLocal" not in script
        assert ".cua-driver-local" not in script
        assert "cua-driver-local-serve" not in script
