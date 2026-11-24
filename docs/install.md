# Install loam

## Shell installer

The release workflow publishes `loam-installer.sh` beside the archives:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/mycelium-linux/loam/releases/latest/download/loam-installer.sh | sh
```

It installs into `~/.local` by default:

```text
~/.local/bin/loam
~/.local/bin/loamd
~/.local/bin/loam-mcp
~/.local/bin/loam-ble-host
~/.local/share/loam/wit/
~/.local/share/man/man1/loam.1
~/.local/share/man/man1/loamd.1
```

Use a different prefix or pin a release:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/mycelium-linux/loam/releases/latest/download/loam-installer.sh \
  | sh -s -- --prefix "$HOME/.loam"
```

```sh
LOAM_VERSION=v0.1.0 sh scripts/install.sh --prefix "$HOME/.loam"
```

The installer verifies the published SHA-256 unless `--no-verify` is passed.

## Release tarball

Download the archive for your platform from the latest GitHub release, verify
its checksum, and place the binaries on `PATH`:

```sh
tag=v0.1.0
target=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
curl -fLO "https://github.com/mycelium-linux/loam/releases/download/${tag}/loam-${tag}-${target}.tar.gz"
curl -fLO "https://github.com/mycelium-linux/loam/releases/download/${tag}/loam-${tag}-${target}.tar.gz.sha256"
shasum -a 256 -c "loam-${tag}-${target}.tar.gz.sha256"
tar -xzf "loam-${tag}-${target}.tar.gz"
sudo install -m 755 "loam-${tag}-${target}/bin/"* /usr/local/bin/
sudo cp -R "loam-${tag}-${target}/share/loam/wit" /usr/local/share/loam/
sudo install -m 644 "loam-${tag}-${target}/share/man/man1/"* /usr/local/share/man/man1/
loam doctor
```

The archive contains:

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

## Homebrew

The tap is not published yet. Until it is, use the shell installer or a
release tarball.

## From source

```sh
cargo build --release -p loam --features surface -p loam-ble-host
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/man/man1"
install -m 755 target/release/loam target/release/loamd \
  target/release/loam-mcp target/release/loam-ble-host "$HOME/.local/bin/"
install -m 644 docs/man/loam.1 docs/man/loamd.1 "$HOME/.local/share/man/man1/"
loam doctor
```

The `loam` binary requires the `surface` feature; `loamd` requires the `wasi`
feature, which is on by default.

## Check

```sh
loam doctor
loamd --help
man loam
man loamd
```

`loam doctor` reports the runtime version and the capabilities linked into this
build.
