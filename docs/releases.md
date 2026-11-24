# Releases

## Versioning

One workspace version:

```toml
[workspace.package]
version = "0.1.0"
```

Every crate inherits it with `version.workspace = true`.

## Tag

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Artifacts

The release workflow builds:

```text
aarch64-apple-darwin
x86_64-apple-darwin
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
```

Each archive contains:

```text
bin/loam
bin/loamd
bin/loam-mcp
bin/loam-ble-host
share/loam/wit/
share/man/man1/loam.1
share/man/man1/loamd.1
LICENSE-APACHE
LICENSE-MIT
README.md
```

The release also publishes:

```text
SHA256SUMS
loam-installer.sh
```

Signing and notarization are a follow-up. Do not describe an unsigned artifact
as signed.

## Checklist

- `main` is green
- version bumped once
- changelog written
- `cargo test --workspace` passes
- surface fixture capture passes
- audio non-device test passes
- tag pushed
- artifacts and checksums uploaded
- install docs updated
- Homebrew tap updated
