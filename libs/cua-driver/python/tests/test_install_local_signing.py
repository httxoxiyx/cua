import os
import subprocess
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parents[2] / "scripts"
SIGNING_HELPER = SCRIPTS_DIR / "_local-signing.sh"


def run_signing_policy(shell_body: str, extra_env=None) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["SIGNING_HELPER"] = str(SIGNING_HELPER)
    env.update(extra_env or {})
    return subprocess.run(
        ["/bin/bash", "-c", 'set -euo pipefail; source "$SIGNING_HELPER"; ' + shell_body],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )


def test_local_installer_accepts_untrusted_self_signed_identity() -> None:
    """The installer-created self-signed cert is usable even before trust-chain validation."""
    script = (SIGNING_HELPER).read_text()

    assert "security find-identity -v -p codesigning" not in script
    assert script.count("security find-identity -p codesigning") >= 2


def test_daemon_path_is_escaped_before_regex_process_matching() -> None:
    result = run_signing_policy(
        r'''escape_extended_regex '/tmp/Cua Driver.app/build+[one]/cua-driver-local' '''
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == r"/tmp/Cua Driver\.app/build\+\[one\]/cua-driver-local"


def test_local_process_identity_uses_lsof_and_install_disallows_basename(
    tmp_path: Path,
) -> None:
    lsof = tmp_path / "lsof"
    lsof.write_text(
        "#!/bin/sh\n"
        "printf '%s' \"$*\" > \"$TEST_LSOF_ARGS\"\n"
        "printf 'n%s\\n' \"$TEST_EXECUTABLE\"\n",
        encoding="utf-8",
    )
    lsof.chmod(0o755)
    args = tmp_path / "args"
    executable = "/Applications/MuseCodeCuaDriverLocal.app/Contents/MacOS/cua-driver-local"
    result = run_signing_policy(
        r'''
        OS=Darwin
        identity="$(local_owned_process_identity 4242)"
        [ "$identity" = "$TEST_EXECUTABLE" ]
        local_owned_process_identity_matches "$identity" 0 "$TEST_EXECUTABLE"
        ! local_owned_process_identity_matches cua-driver-local 0 "$TEST_EXECUTABLE"
        local_owned_process_identity_matches cua-driver-local 1 "$TEST_EXECUTABLE"
        ''',
        {
            "CUA_DRIVER_LSOF": str(lsof),
            "TEST_EXECUTABLE": executable,
            "TEST_LSOF_ARGS": str(args),
        },
    )

    assert result.returncode == 0, result.stderr
    assert args.read_text() == "-a -p 4242 -d txt -Fn"


def test_strict_local_signing_fails_before_ad_hoc_fallback() -> None:
    result = run_signing_policy(
        r"""
        OS=Darwin
        RED= GREEN= YELLOW= NORMAL=
        CUA_DRIVER_REQUIRE_STABLE_SIGNING=1
        ensure_local_signing_identity() { printf '%s' -; }
        clean_partial_bundle_signature() { :; }
        codesign_bounded() { echo "unexpected ad-hoc signing" >&2; return 99; }
        if sign_staged_local_app /staged.app /live.app; then exit 90; fi
        """
    )

    assert result.returncode == 0, result.stderr
    assert "stable macOS signing is required" in result.stderr
    assert "live installation was not changed" in result.stderr
    assert "--require-stable-signing" in result.stderr
    assert "unexpected ad-hoc signing" not in result.stderr


def test_ad_hoc_fallback_is_prominent_and_reports_cdhash() -> None:
    result = run_signing_policy(
        r"""
        OS=Darwin
        RED= GREEN= YELLOW= NORMAL=
        CUA_DRIVER_REQUIRE_STABLE_SIGNING=0
        ensure_local_signing_identity() { printf '%s' -; }
        clean_partial_bundle_signature() { :; }
        codesign_bounded() { return 0; }
        codesign() {
            if [ "$1" = "-d" ]; then
                echo '# designated => cdhash H"0123456789ABCDEF"'
            fi
            return 0
        }
        sign_staged_local_app /staged.app /missing-live.app
        """
    )

    assert result.returncode == 0, result.stderr
    assert "WARNING: MuseCodeCuaDriverLocal.app was signed ad-hoc" in result.stderr
    assert "WILL become invalid on the next rebuild" in result.stderr
    assert "designated requirement uses cdhash" in result.stderr


def test_certificate_requirement_is_classified_as_stable() -> None:
    result = run_signing_policy(
        r"""
        OS=Darwin
        RED= GREEN= YELLOW= NORMAL=
        CUA_DRIVER_REQUIRE_STABLE_SIGNING=1
        ensure_local_signing_identity() { printf '%s' ABCDEF; }
        codesign_bounded() { return 0; }
        codesign() {
            if [ "$1" = "-d" ]; then
                echo 'designated => anchor trusted and certificate leaf[subject.CN] = "CuaDriver Local Signing (cua-driver-rs)"' >&2
            fi
            return 0
        }
        sign_staged_local_app /staged.app /missing-live.app
        """
    )

    assert result.returncode == 0, result.stderr
    assert "stable local identity" in result.stdout


def test_incompatible_requirement_resets_all_local_driver_services() -> None:
    result = run_signing_policy(
        r"""
        RED= YELLOW= NORMAL=
        calls=""
        tccutil() { calls="${calls}${1}:${2}:${3}"$'\n'; }
        reset_local_tcc_after_requirement_change incompatible
        printf '%s' "$calls"
        """
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == (
        "reset:Accessibility:com.meta.musecode.cua.driver.local\n"
        "reset:ScreenCapture:com.meta.musecode.cua.driver.local\n"
        "reset:AppleEvents:com.meta.musecode.cua.driver.local\n"
    )
    assert "cleared stale Accessibility, Screen Recording, and Automation rows" in result.stderr
    assert "cua-driver-local permissions grant" in result.stderr


def test_compatible_or_first_install_requirements_preserve_tcc_rows() -> None:
    result = run_signing_policy(
        r"""
        RED= YELLOW= NORMAL=
        tccutil() { echo unexpected >&2; return 99; }
        reset_local_tcc_after_requirement_change first-install
        reset_local_tcc_after_requirement_change compatible
        """
    )

    assert result.returncode == 0, result.stderr
    assert "unexpected" not in result.stderr


def test_requirement_tcc_reset_failure_is_actionable_and_fails_closed() -> None:
    result = run_signing_policy(
        r"""
        RED= YELLOW= NORMAL=
        tccutil() { [ "$2" != ScreenCapture ]; }
        if reset_local_tcc_after_requirement_change incompatible; then
            exit 90
        fi
        """
    )

    assert result.returncode == 0, result.stderr
    assert "could not reset these TCC services" in result.stderr
    assert "tccutil reset Accessibility com.meta.musecode.cua.driver.local" in result.stderr
    assert "tccutil reset ScreenCapture com.meta.musecode.cua.driver.local" in result.stderr
    assert "tccutil reset AppleEvents com.meta.musecode.cua.driver.local" in result.stderr


def test_requirement_compatibility_detects_ad_hoc_to_certificate_transition() -> None:
    result = run_signing_policy(
        r'''
        YELLOW= NORMAL=
        codesign() {
            case "$*" in
                *'-R =cdhash H"OLD"'*) return 3 ;;
                *) return 99 ;;
            esac
        }
        local_requirement_compatibility 'cdhash H"OLD"' /staged.app
        '''
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == "incompatible"


def test_requirement_compatibility_fails_closed_on_operational_error() -> None:
    result = run_signing_policy(
        r'''
        YELLOW= NORMAL=
        codesign() { echo unavailable >&2; return 70; }
        local_requirement_compatibility 'certificate leaf = H"OLD"' /staged.app
        '''
    )

    assert result.returncode == 0
    assert result.stdout == "unknown"
    assert "codesign status 70" in result.stderr


def test_legacy_local_app_cleanup_is_scoped_and_removes_after_resets(tmp_path: Path) -> None:
    app = tmp_path / "CuaDriverLocal.app"
    calls = tmp_path / "calls"
    app.mkdir()
    result = run_signing_policy(
        r'''
        OS=Darwin
        legacy_local_app_bundle_id() { printf '%s' com.trycua.driver.local; }
        verify_local_app_identity() { return 0; }
        register_legacy_local_app() { printf 'register:%s\n' "$1" >> "$TEST_CALLS"; }
        unregister_local_app() { printf 'unregister:%s\n' "$1" >> "$TEST_CALLS"; }
        tccutil() { printf '%s:%s:%s\n' "$1" "$2" "$3" >> "$TEST_CALLS"; }
        remove_legacy_local_app_path() {
            printf 'remove:%s\n' "$1" >> "$TEST_CALLS"
            rmdir "$1"
        }
        cleanup_legacy_local_app "$TEST_LEGACY_APP" 1
        ''',
        {
            "TEST_LEGACY_APP": str(app),
            "TEST_CALLS": str(calls),
            "CUA_DRIVER_LOCAL_HISTORY_ROOT": str(tmp_path / "history"),
        },
    )

    assert result.returncode == 0, result.stderr
    assert not app.exists()
    assert calls.read_text().splitlines() == [
        f"register:{app}",
        "reset:Accessibility:com.trycua.driver.local",
        "reset:ScreenCapture:com.trycua.driver.local",
        "reset:AppleEvents:com.trycua.driver.local",
        f"unregister:{app}",
        f"remove:{app}",
    ]


def test_legacy_local_app_cleanup_failure_preserves_bundle(tmp_path: Path) -> None:
    app = tmp_path / "CuaDriverLocal.app"
    app.mkdir()
    result = run_signing_policy(
        r'''
        OS=Darwin
        legacy_local_app_bundle_id() { printf '%s' com.trycua.driver.local; }
        verify_local_app_identity() { return 0; }
        register_legacy_local_app() { return 0; }
        tccutil() { [ "$2" != ScreenCapture ]; }
        cleanup_legacy_local_app "$TEST_LEGACY_APP" 1
        ''',
        {
            "TEST_LEGACY_APP": str(app),
            "CUA_DRIVER_LOCAL_HISTORY_ROOT": str(tmp_path / "history"),
        },
    )

    assert result.returncode != 0
    assert app.is_dir()
    assert "legacy app was preserved" in result.stderr


def test_legacy_local_app_cleanup_preserves_foreign_bundle(tmp_path: Path) -> None:
    app = tmp_path / "CuaDriverLocal.app"
    app.mkdir()
    result = run_signing_policy(
        r'''
        OS=Darwin
        legacy_local_app_bundle_id() { printf '%s' com.example.foreign; }
        tccutil() { echo unexpected >&2; return 99; }
        cleanup_legacy_local_app "$TEST_LEGACY_APP" 1
        ''',
        {
            "TEST_LEGACY_APP": str(app),
            "CUA_DRIVER_LOCAL_HISTORY_ROOT": str(tmp_path / "history"),
        },
    )

    assert result.returncode != 0
    assert app.is_dir()
    assert "com.example.foreign" in result.stderr
    assert "unexpected" not in result.stderr


def test_legacy_local_app_cleanup_can_preserve_tcc(tmp_path: Path) -> None:
    app = tmp_path / "CuaDriverLocal.app"
    app.mkdir()
    result = run_signing_policy(
        r'''
        OS=Darwin
        legacy_local_app_bundle_id() { printf '%s' com.trycua.driver.local; }
        verify_local_app_identity() { return 0; }
        unregister_local_app() { return 0; }
        tccutil() { echo unexpected >&2; return 99; }
        cleanup_legacy_local_app "$TEST_LEGACY_APP" 0
        ''',
        {"TEST_LEGACY_APP": str(app)},
    )

    assert result.returncode == 0, result.stderr
    assert not app.exists()
    assert "unexpected" not in result.stderr


def test_local_history_blocks_identity_removal_until_trusted_purge(tmp_path: Path) -> None:
    history = tmp_path / "computer-history"
    history.mkdir()
    (history / "manifest.json").write_text("{}", encoding="utf-8")
    app = tmp_path / "MuseCodeCuaDriverLocal.app"
    result = run_signing_policy(
        r'''
        RED= NORMAL=
        if refuse_local_history_identity_transition "$TEST_HISTORY" "$TEST_APP" \
            "replace the current local signer identity"; then
            exit 90
        fi
        ''',
        {"TEST_HISTORY": str(history), "TEST_APP": str(app)},
    )

    assert result.returncode == 0
    assert "existing local Computer History state" in result.stderr
    assert f"{app}/Contents/MacOS/cua-driver-local history purge-offline --yes" in result.stderr


def test_rollback_unregisters_candidate_and_restores_registered_backup(tmp_path: Path) -> None:
    app = tmp_path / "MuseCodeCuaDriverLocal.app"
    backup = tmp_path / "MuseCodeCuaDriverLocal.app.backup"
    calls = tmp_path / "calls"
    app.mkdir()
    (app / "candidate").write_text("candidate", encoding="utf-8")
    backup.mkdir()
    (backup / "previous").write_text("previous", encoding="utf-8")
    result = run_signing_policy(
        r'''
        RED= YELLOW= NORMAL=
        verify_local_app_identity() { return 0; }
        unregister_local_app() { printf 'unregister:%s\n' "$1" >> "$TEST_CALLS"; }
        register_local_app() { printf 'register:%s\n' "$1" >> "$TEST_CALLS"; }
        restore_local_app_backup "$TEST_APP" "$TEST_BACKUP" test.bundle cua-driver-local
        ''',
        {"TEST_APP": str(app), "TEST_BACKUP": str(backup), "TEST_CALLS": str(calls)},
    )

    assert result.returncode == 0, result.stderr
    assert (app / "previous").read_text() == "previous"
    assert not backup.exists()
    assert calls.read_text().splitlines() == [f"unregister:{app}", f"register:{app}"]


def test_rollback_reports_unregister_failure_but_still_restores_backup(
    tmp_path: Path,
) -> None:
    app = tmp_path / "MuseCodeCuaDriverLocal.app"
    backup = tmp_path / "MuseCodeCuaDriverLocal.app.backup"
    app.mkdir()
    backup.mkdir()
    (backup / "previous").write_text("previous", encoding="utf-8")
    result = run_signing_policy(
        r'''
        RED= YELLOW= NORMAL=
        verify_local_app_identity() { return 0; }
        unregister_local_app() { return 70; }
        register_local_app() { return 0; }
        if restore_local_app_backup "$TEST_APP" "$TEST_BACKUP" test.bundle cua-driver-local; then
            exit 90
        fi
        ''',
        {"TEST_APP": str(app), "TEST_BACKUP": str(backup)},
    )

    assert result.returncode == 0
    assert (app / "previous").read_text() == "previous"
    assert "could not unregister failed local app candidate" in result.stderr


def test_legacy_cleanup_checks_history_before_tcc_or_removal(tmp_path: Path) -> None:
    app = tmp_path / "CuaDriverLocal.app"
    app.mkdir()
    history = tmp_path / "history"
    history.mkdir()
    (history / "segment").write_text("encrypted", encoding="utf-8")
    result = run_signing_policy(
        r'''
        OS=Darwin
        RED= NORMAL=
        legacy_local_app_bundle_id() { printf '%s' com.trycua.driver.local; }
        verify_local_app_identity() { return 0; }
        register_legacy_local_app() { echo unexpected-register >&2; return 99; }
        unregister_local_app() { echo unexpected-unregister >&2; return 99; }
        tccutil() { echo unexpected-tcc >&2; return 99; }
        remove_legacy_local_app_path() { echo unexpected-remove >&2; return 99; }
        if cleanup_legacy_local_app "$TEST_APP" 1; then exit 90; fi
        ''',
        {
            "TEST_APP": str(app),
            "CUA_DRIVER_LOCAL_HISTORY_ROOT": str(history),
        },
    )

    assert result.returncode == 0
    assert app.exists()
    assert "existing local Computer History state" in result.stderr
    assert "unexpected-" not in result.stderr


def test_installer_verifies_the_copied_designated_requirement() -> None:
    script = (SCRIPTS_DIR / "_install-local-rust.sh").read_text()

    assert 'INSTALLED_REQUIREMENT="$(designated_requirement "$APP_DEST")"' in script
    assert '[ "$INSTALLED_REQUIREMENT" = "$STAGED_REQUIREMENT" ]' in script
    assert "verified installed designated requirement: certificate-backed" in script
    assert "verified installed designated requirement: ad-hoc cdhash" in script
    assert 'PREVIOUS_REQUIREMENT="$(designated_requirement "$APP_DEST")"' in script
    assert "local_requirement_compatibility" in script
    assert "reset_local_tcc_after_requirement_change" in script
    assert "AppleEvents" in SIGNING_HELPER.read_text()


def test_local_installer_uses_a_separate_macos_identity() -> None:
    """Local rebuilds never replace or reset the release app identity (#2230)."""
    script = (
        Path(__file__).resolve().parents[2] / "scripts" / "_install-local-rust.sh"
    ).read_text()

    assert 'APP_DEST="/Applications/MuseCodeCuaDriverLocal.app"' in script
    assert 'CFBundleIdentifier -string "com.meta.musecode.cua.driver.local"' in script
    assert 'CFBundleExecutable -string "cua-driver-local"' in script
    assert "tccutil reset" not in script
    assert 'prepare_legacy_local_app_removal "$LEGACY_LOCAL_APP" 1' in script
    assert 'mv "$LEGACY_LOCAL_APP" "$LEGACY_LOCAL_APP_BACKUP"' in script
    assert "rollback_legacy_local_app_on_exit" in script
    assert 'if [ "${LEGACY_LOCAL_APP_OWNED:-0}" = "1" ]; then' in script
    assert 'owned_paths+=("$LEGACY_LOCAL_APP/Contents/MacOS/cua-driver-local")' in script
    assert 'stop_verified_local_processes 0 "${owned_paths[@]}"' in script


def test_unix_local_installer_uses_separate_paths_and_autostart() -> None:
    """Local install state, command, and service names coexist with release."""
    script = (
        Path(__file__).resolve().parents[2] / "scripts" / "_install-local-rust.sh"
    ).read_text()

    assert 'HOME_DIR="${CUA_DRIVER_LOCAL_HOME:-$HOME/.cua-driver-local}"' in script
    assert "BIN_DIR/cua-driver-local" in script
    assert "com.trycua.cua-driver-local.plist" in script
    assert "cua-driver-local.service" in script
    assert "stop_cua_driver_daemons" not in script


def test_unix_local_installer_always_embeds_source_provenance() -> None:
    """Local builds derive Git provenance but preserve VM snapshot overrides."""
    script = (
        Path(__file__).resolve().parents[2] / "scripts" / "_install-local-rust.sh"
    ).read_text()

    assert 'if [ -z "${CUA_DRIVER_SOURCE_SHA:-}" ]; then' in script
    assert "rev-parse --verify 'HEAD^{commit}'" in script
    assert "status --porcelain --untracked-files=normal" in script
    assert 'CUA_DRIVER_SOURCE_SHA="${CUA_DRIVER_SOURCE_SHA}-dirty"' in script
    assert "export CUA_DRIVER_SOURCE_SHA" in script


def test_windows_local_installer_always_embeds_source_provenance() -> None:
    """The Windows developer installer follows the same provenance contract."""
    script = (Path(__file__).resolve().parents[2] / "scripts" / "install-local.ps1").read_text()

    assert "IsNullOrWhiteSpace($env:CUA_DRIVER_SOURCE_SHA)" in script
    assert "rev-parse --verify 'HEAD^{commit}'" in script
    assert "status --porcelain --untracked-files=normal" in script
    assert '"$detectedSourceSha-dirty"' in script


def test_windows_local_installer_uses_separate_paths_and_autostart() -> None:
    script = (Path(__file__).resolve().parents[2] / "scripts" / "install-local.ps1").read_text()

    assert '$BinaryName  = "cua-driver-local.exe"' in script
    assert '"Programs\\Cua\\cua-driver-local\\bin"' in script
    assert '".cua-driver-local"' in script
    assert '"cua-driver-local-serve"' in script
    assert "Repair-CuaDriverStaleDaemon" not in script


def test_release_installers_do_not_target_local_product_artifacts() -> None:
    scripts_dir = Path(__file__).resolve().parents[2] / "scripts"
    for name in ("_install-rust.sh", "install.ps1"):
        script = (scripts_dir / name).read_text()
        assert "MuseCodeCuaDriverLocal" not in script
        assert ".cua-driver-local" not in script
        assert "cua-driver-local-serve" not in script
