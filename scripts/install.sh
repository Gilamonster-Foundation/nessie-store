#!/usr/bin/env bash
# nessie-store installer — fetch the release binary + setup wizard, then offer
# to configure the systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/Gilamonster-Foundation/nessie-store/main/scripts/install.sh | sudo bash
#   sudo ./install.sh [vX.Y.Z] [--no-setup]
set -euo pipefail

REPO="Gilamonster-Foundation/nessie-store"
VERSION="latest"
RUN_SETUP=1
for arg in "$@"; do
    case "$arg" in
        --no-setup) RUN_SETUP=0 ;;
        v*) VERSION="$arg" ;;
        -h | --help) sed -n '2,7p' "$0"; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

die() { echo "error: $*" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || die "must run as root (e.g. pipe to 'sudo bash', or 'sudo ./install.sh')"

case "$(uname -s)" in Linux) ;; *) die "only Linux is supported" ;; esac
case "$(uname -m)" in x86_64 | amd64) arch="x86_64" ;; *) die "unsupported arch $(uname -m) (x86_64 only)" ;; esac
for c in curl tar; do command -v "$c" >/dev/null 2>&1 || die "$c is required"; done

# Resolve the release tag.
if [ "$VERSION" = "latest" ]; then
    tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    [ -n "$tag" ] || die "could not resolve the latest release tag"
else
    tag="$VERSION"
fi
echo "installing nessie-store $tag (linux-$arch)"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/$REPO/releases/download/$tag"

# Binary.
curl -fsSL "$base/nessie-store-$tag-linux-$arch.tar.gz" -o "$tmp/nessie-store.tar.gz" \
    || die "download failed: $base/nessie-store-$tag-linux-$arch.tar.gz"
tar -C "$tmp" -xzf "$tmp/nessie-store.tar.gz"
install -m 0755 "$tmp/nessie-store" /usr/bin/nessie-store
echo "✓ installed /usr/bin/nessie-store ($(nessie-store --version 2>/dev/null || echo "$tag"))"

# Setup wizard (from the tagged source).
curl -fsSL "https://raw.githubusercontent.com/$REPO/$tag/scripts/nessie-store-setup.sh" \
    -o /usr/local/bin/nessie-store-setup 2>/dev/null \
    && chmod 0755 /usr/local/bin/nessie-store-setup \
    && echo "✓ installed /usr/local/bin/nessie-store-setup" \
    || echo "note: could not fetch the setup wizard (run scripts/nessie-store-setup.sh from a checkout)"

if [ "$RUN_SETUP" -eq 1 ] && [ -t 0 ] && [ -x /usr/local/bin/nessie-store-setup ]; then
    echo "launching the setup wizard…"
    exec /usr/local/bin/nessie-store-setup
else
    echo "next: sudo nessie-store-setup   # configure + start the systemd service"
fi
