# Source and attribution

This repository starts a new, standalone Git history for the Driver component;
it does not claim original authorship of the imported code.

- Original project: [trycua/cua](https://github.com/trycua/cua).
- Import source: [httxoxiyx/cua-archive](https://github.com/httxoxiyx/cua-archive),
  formerly `httxoxiyx/cua`, GitHub repository ID `1352768248`.
- Exact source commit:
  [`be9f243bf9e9c52f9fb3573effa993bcf7c78834`](https://github.com/httxoxiyx/cua-archive/commit/be9f243bf9e9c52f9fb3573effa993bcf7c78834),
  including the developer branch's native, PiP and optional-UniFFI changes.
- Upstream authorship and prior changes remain available in the source history.
  Original copyright headers and license texts are retained unchanged.

The source repository was renamed to `cua-archive` when this independent
repository took the name `cua`. Source links use the archive explicitly:
reusing a repository name removes the old-name redirect to its former history.
The standalone repository has GitHub repository ID `1375969240`.

## Extraction boundary

Imported from the following paths, preserving executable file modes:

```text
libs/cua-driver/                            (except the three journals below)
.gitattributes
LICENSE.md
flake.nix
flake.lock
scripts/ci/
nix/cua-driver/
.github/release-state/cua-driver-rs-published-version
.github/scripts/cua-driver-mcp-compat/
.github/scripts/update_cua_driver_installer_version.py
.github/scripts/verify_cua_driver_release_archives.py
.github/scripts/tests/__init__.py
.github/scripts/tests/test_ci_nix_linux_paths.py
.github/scripts/tests/test_driver_script_encoding.py
.github/scripts/tests/test_update_cua_driver_installer_version.py
.github/scripts/tests/test_verify_cua_driver_release_archives.py
.github/workflows/ci-cua-driver-contract-clients.yml
.github/workflows/ci-nix-linux.yml
.github/workflows/ci-rust-format.yml
.github/workflows/ci-rust-linux.yml
.github/workflows/ci-rust-windows.yml
.github/workflows/e2e-rust-linux.yml
.github/workflows/e2e-rust-linux-wayland.yml
.github/workflows/e2e-rust-standalone-browsers.yml
.github/workflows/e2e-rust-windows.yml
```

Three historical local-debug journals under `libs/cua-driver/docs/` are not
imported because they contain personal paths or paired private-workspace
receipts. They remain recoverable in the source repository:

- `pages-color-popover-debug-20260914.md`
- `local-integration-follow-up-2026-09-15.md`
- `type-text-refusal-projection-2026-09-14.md`

No retained source or test depends on those journals. Functional contracts,
fixtures and current limitations are retained. Other monorepo products, root
governance documents and release/publishing workflows are outside this import.
The new root README, this provenance note and the root ignore file describe
the standalone repository rather than upstream ownership or release services.

The only edits within imported files replace internal consumer-name references
with generic wrapper/caller wording in three Rust comments:
`platform-macos/src/tools/get_window_state.rs`, `platform-macos/src/tools/mod.rs`
and `platform-windows/src/tools/impl_.rs`, under `libs/cua-driver/rust/crates/`.
These edits do not change executable code. All other imported bytes are unchanged.

## Notices

- `LICENSE.md`: MIT, copyright Cua AI, Inc. Existing source-file copyright
  headers remain intact.
- `libs/cua-driver/rust/crates/cursor-overlay/assets/Inter-OFL.txt`: retained
  alongside the Inter font.
- `libs/cua-driver/rust/crates/platform-linux/protocol/virtual-keyboard-unstable-v1.xml`:
  retains its embedded contributor and permission notice.
- `libs/cua-driver/scripts/node-runtime-NOTICE.md`: retained with the Node
  runtime build script and pinned metadata, including MPL-2.0 obligations.

This source attribution is not a claim that binary redistribution is already
qualified. Release artifacts must include their own applicable notices.
