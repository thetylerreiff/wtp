#!/bin/sh
# Installs wtp from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/thetylerreiff/wtp/main/install.sh | sh
#
# WTP_VERSION      Release tag to install (default: latest)
# WTP_INSTALL_DIR  Where to put the binary (default: ~/.local/bin)
set -eu

repo="thetylerreiff/wtp"
install_dir="${WTP_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  echo "wtp: $1" >&2
  exit 1
}

[ "$(uname -s)" = "Darwin" ] || fail "wtp runs on macOS only."

case "$(uname -m)" in
  arm64 | aarch64) target="aarch64-apple-darwin" ;;
  x86_64) target="x86_64-apple-darwin" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac

if [ -n "${WTP_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/$WTP_VERSION"
else
  base="https://github.com/$repo/releases/latest/download"
fi
archive="wtp-$target.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${archive}..."
curl -fsSL "$base/$archive" -o "$tmp/$archive" || fail "couldn't download $base/$archive"
curl -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256" || fail "couldn't download the checksum"
(cd "$tmp" && shasum -a 256 -c "$archive.sha256" >/dev/null) || fail "checksum mismatch; not installing."

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$install_dir"
install -m 755 "$tmp/wtp" "$install_dir/wtp"

echo "Installed $("$install_dir/wtp" --version) to $install_dir/wtp"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "Add it to your PATH:  export PATH=\"$install_dir:\$PATH\"" ;;
esac
