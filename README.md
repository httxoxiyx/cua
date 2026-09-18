# cua

An independent development repository for the Cua Driver component, derived
from [Cua](https://github.com/trycua/cua). This is not an official TryCua
distribution. The temporary repository name is `cua-next` while the standalone
source and packaging are qualified; the previous repository remains intact.

## Scope

- Native Driver, platform implementations, MCP server, SDKs, fixtures and tests.
- Required build and CI support, with existing `libs/cua-driver/...` paths kept
  to avoid changing runtime behavior during extraction.
- No other products from the upstream monorepo, binary releases, credentials,
  installed-app changes or automatic migration of user settings.

See [UPSTREAM.md](UPSTREAM.md) for the exact source snapshot, extraction scope
and attribution, and [component documentation](libs/cua-driver/README.md) for
the existing implementation.

## Development status

The first qualification target is macOS Apple Silicon. Other platform sources
and their tests are retained; their presence does not establish release support.
The native build can avoid UniFFI; generated language SDKs retain their existing
FFI dependency and licensing requirements.

This extraction does not fix the previously observed background text-field
click failure in the input fixture. Its batch stopped before typing, so it is
not evidence of successful batch input or Chinese input-method handling.

The app's display name is `cua`, without a custom icon. Bundle identity,
permission attribution and signing behavior are unchanged. There is no approved
Developer ID distribution certificate or qualified public binary release here.

Inherited installers, package names, update checks, telemetry settings and
hosted-documentation links still refer to upstream where they did before this
extraction. **Do not use those installers to obtain this development build.**
Independent release endpoints and ownership must be configured and verified
before distribution. This repository does not auto-install or update a Driver.

## Build

From `libs/cua-driver/rust`, using the pinned Rust toolchain and platform build
prerequisites:

```sh
cargo build --locked --release -p cua-driver -p cursor-theme-cli
```

Use a fresh build target for a new checkout path. Do not copy Cargo/Swift module
caches between paths: cached Swift modules contain absolute paths.

## License and credit

Original copyright and applicable third-party notices are preserved. See
[LICENSE.md](LICENSE.md), [UPSTREAM.md](UPSTREAM.md), the embedded asset notices,
and the dependency metadata. Removing unrelated source does not remove the
licenses of retained components.
