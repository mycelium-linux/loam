#!/bin/sh
# Install a published loam release.
#
#   curl --proto '=https' --tlsv1.2 -LsSf \
#     https://github.com/mycelium-linux/loam/releases/latest/download/loam-installer.sh | sh
#
# Environment:
#   LOAM_VERSION   tag to install (default: latest)
#   LOAM_PREFIX    install root (default: $HOME/.local)
#   LOAM_NO_VERIFY set to 1 to skip the checksum (not recommended)
set -eu

REPO=mycelium-linux/loam
VERSION=${LOAM_VERSION:-}
PREFIX=${LOAM_PREFIX:-${HOME}/.local}
VERIFY=${LOAM_NO_VERIFY:-0}
DRY_RUN=0

usage() {
    cat <<'USAGE'
usage: loam-installer.sh [--prefix DIR] [--version TAG] [--no-verify] [--dry-run]

Installs loam, loamd, loam-mcp, loam-ble-host, WIT contracts, and man pages.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX=$2; shift 2 ;;
        --version) VERSION=$2; shift 2 ;;
        --no-verify) VERIFY=0; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "loam-installer: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
done

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) echo "loam-installer: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64) arch=x86_64 ;;
    *) echo "loam-installer: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

target="${arch}-${os}"

if [ -z "$VERSION" ]; then
    effective=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest")
    VERSION=${effective##*/}
fi
case "$VERSION" in
    v*) tag=$VERSION ;;
    *) tag=v$VERSION ;;
esac

archive="loam-${tag}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${tag}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

echo "loam-installer: ${tag} (${target}) -> ${PREFIX}"
if [ "$DRY_RUN" = 1 ]; then
    echo "would download ${base}/${archive}"
    exit 0
fi

curl -fsSL "${base}/${archive}" -o "${tmp}/${archive}"
if [ "$VERIFY" = 1 ]; then
    if curl -fsSL "${base}/${archive}.sha256" -o "${tmp}/${archive}.sha256"; then
        (cd "$tmp" && (sha256sum -c "${archive}.sha256" 2>/dev/null || shasum -a 256 -c "${archive}.sha256"))
    else
        echo "loam-installer: checksum unavailable; refusing (use --no-verify to override)" >&2
        exit 1
    fi
fi

tar -xzf "${tmp}/${archive}" -C "$tmp"
root="${tmp}/loam-${tag}-${target}"
mkdir -p "${PREFIX}/bin" "${PREFIX}/share/loam" "${PREFIX}/share/man/man1"
for binary in loam loamd loam-mcp loam-ble-host; do
    if [ -f "${root}/bin/${binary}" ]; then
        install -m 755 "${root}/bin/${binary}" "${PREFIX}/bin/${binary}"
    fi
done
if [ -d "${root}/share/loam/wit" ]; then
    rm -rf "${PREFIX}/share/loam/wit"
    cp -R "${root}/share/loam/wit" "${PREFIX}/share/loam/wit"
fi
for page in loam.1 loamd.1; do
    if [ -f "${root}/share/man/man1/${page}" ]; then
        install -m 644 "${root}/share/man/man1/${page}" "${PREFIX}/share/man/man1/${page}"
    fi
done

echo "loam-installer: installed to ${PREFIX}"
echo "loam-installer: add ${PREFIX}/bin to PATH, then run: loam doctor"
