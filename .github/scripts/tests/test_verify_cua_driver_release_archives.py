from __future__ import annotations

from io import BytesIO
from pathlib import Path
import plistlib
import tarfile
import zipfile

import pytest

from verify_cua_driver_release_archives import (
    ArchiveContract,
    ContractError,
    release_contracts,
    verify_release_archives,
)


VERSION = "9.8.7"
REPO_ROOT = Path(__file__).resolve().parents[3]


def _write_tar(path: Path, contract: ArchiveContract, overrides=None) -> None:
    overrides = overrides or {}
    with tarfile.open(path, "w:gz") as archive:
        for name in contract.members:
            if name.endswith("/CuaDriver.app/Contents/Info.plist"):
                payload = plistlib.dumps({
                    "CFBundleIdentifier": "com.meta.musecode.cua.driver",
                    "CFBundleExecutable": "cua-driver",
                    "CFBundlePackageType": "APPL",
                    "CFBundleShortVersionString": VERSION,
                })
            else:
                payload = f"payload for {name}".encode()
            payload = overrides.get(name, payload)
            info = tarfile.TarInfo(name)
            info.size = len(payload)
            info.mode = 0o755 if name in contract.executable_members else 0o644
            archive.addfile(info, BytesIO(payload))


def _write_zip(path: Path, contract: ArchiveContract) -> None:
    with zipfile.ZipFile(path, "w") as archive:
        for name in contract.members:
            archive.writestr(name, f"payload for {name}")


def _write_valid_release(root: Path) -> tuple[ArchiveContract, ...]:
    contracts = release_contracts(VERSION)
    for contract in contracts:
        path = root / contract.filename
        if path.name.endswith(".tar.gz"):
            _write_tar(path, contract)
        else:
            _write_zip(path, contract)
    return contracts


def test_complete_release_archive_set_passes(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)

    verified = verify_release_archives(tmp_path, VERSION)

    assert len(verified) == len(contracts) == 12


def test_reusable_release_job_verifies_the_downloaded_artifact_set() -> None:
    workflow = (
        REPO_ROOT / ".github/workflows/verify-cua-driver-release-artifacts.yml"
    ).read_text(encoding="utf-8")

    assert "workflow_call:" in workflow
    assert "actions/download-artifact@" in workflow
    assert "name: ${{ inputs.artifact_name }}" in workflow
    assert "verify_cua_driver_release_archives.py" in workflow
    assert "--artifacts release-artifacts" in workflow
    assert "CUA_RELEASE_VERSION: ${{ inputs.version }}" in workflow
    assert '--version "$CUA_RELEASE_VERSION"' in workflow


def test_missing_cursor_theme_fails_with_archive_and_member(
    tmp_path: Path,
) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract for contract in contracts if contract.filename.endswith("darwin-universal.tar.gz")
    )
    missing = (
        f"cua-driver-rs-{VERSION}-darwin-universal/CuaDriver.app/Contents/MacOS/cua-cursor-theme"
    )
    broken = ArchiveContract(
        target.filename,
        tuple(member for member in target.members if member != missing),
        tuple(member for member in target.executable_members if member != missing),
    )
    _write_tar(tmp_path / target.filename, broken)

    with pytest.raises(ContractError, match=rf"{target.filename} is missing {missing}"):
        verify_release_archives(tmp_path, VERSION)


def test_missing_archive_fails_closed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    missing = contracts[0].filename
    (tmp_path / missing).unlink()

    with pytest.raises(ContractError, match=rf"missing release archive: {missing}"):
        verify_release_archives(tmp_path, VERSION)


def test_non_executable_unix_binary_fails_closed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract
        for contract in contracts
        if contract.filename.endswith("linux-x86_64-binary.tar.gz")
    )
    broken = ArchiveContract(target.filename, target.members)
    _write_tar(tmp_path / target.filename, broken)

    with pytest.raises(
        ContractError,
        match=rf"{target.filename} contains non-executable member cua-driver",
    ):
        verify_release_archives(tmp_path, VERSION)


def test_macos_bundle_identity_must_match_production_contract(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract
        for contract in contracts
        if contract.filename.endswith("darwin-universal.tar.gz")
    )
    info_name = next(
        name for name in target.members if name.endswith("CuaDriver.app/Contents/Info.plist")
    )
    wrong_info = plistlib.dumps({
        "CFBundleIdentifier": "com.trycua.driver",
        "CFBundleExecutable": "cua-driver",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": VERSION,
    })
    _write_tar(tmp_path / target.filename, target, {info_name: wrong_info})

    with pytest.raises(ContractError, match="invalid CFBundleIdentifier"):
        verify_release_archives(tmp_path, VERSION)


def test_macos_standalone_bundle_must_not_be_plugin_managed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract
        for contract in contracts
        if contract.filename.endswith("darwin-universal.tar.gz")
    )
    info_name = next(
        name for name in target.members if name.endswith("CuaDriver.app/Contents/Info.plist")
    )
    wrong_info = plistlib.dumps({
        "CFBundleIdentifier": "com.meta.musecode.cua.driver",
        "CFBundleExecutable": "cua-driver",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": VERSION,
        "CuaPluginManaged": True,
    })
    _write_tar(tmp_path / target.filename, target, {info_name: wrong_info})

    with pytest.raises(ContractError, match="incorrectly plugin-managed"):
        verify_release_archives(tmp_path, VERSION)


@pytest.mark.parametrize("kind", ("traversal", "symlink", "duplicate"))
def test_tar_archives_reject_unsafe_or_ambiguous_members(
    tmp_path: Path, kind: str
) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract
        for contract in contracts
        if contract.filename.endswith("linux-x86_64-binary.tar.gz")
    )
    path = tmp_path / target.filename
    with tarfile.open(path, "w:gz") as archive:
        for name in target.members:
            payload = b"payload"
            info = tarfile.TarInfo(name)
            info.size = len(payload)
            info.mode = 0o755 if name in target.executable_members else 0o644
            archive.addfile(info, BytesIO(payload))
        if kind == "traversal":
            info = tarfile.TarInfo("../outside")
            info.size = 1
            archive.addfile(info, BytesIO(b"x"))
        elif kind == "symlink":
            info = tarfile.TarInfo("payload-link")
            info.type = tarfile.SYMTYPE
            info.linkname = "../../outside"
            archive.addfile(info)
        else:
            info = tarfile.TarInfo(target.members[0])
            info.size = 1
            archive.addfile(info, BytesIO(b"x"))

    with pytest.raises(ContractError, match="unsafe|link or special|duplicate"):
        verify_release_archives(tmp_path, VERSION)
