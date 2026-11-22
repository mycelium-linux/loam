# Install loam

## Release tarball

Download the archive for your platform from the latest GitHub release, verify
its checksum, and place `loam` on `PATH`.

```sh
tar -xzf loam-*.tar.gz
sudo install -m 755 loam /usr/local/bin/loam
loam doctor
```

The archive contains:

```text
bin/loam
bin/loam-ble-host
share/loam/wit/
LICENSE-APACHE
LICENSE-MIT
README.md
```

## Shell installer

When the release workflow publishes it:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/mycelium-linux/loam/releases/latest/download/loam-installer.sh | sh
```

## Homebrew

When the tap is published:

```sh
brew install mycelium-linux/loam/loam
```

## From source

```sh
cargo build --release -p loam-cli
install -m 755 target/release/loam ~/.local/bin/loam
loam doctor
```

## Check

```sh
loam doctor
```

It reports the runtime version and the capabilities linked into this build.
