#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Install Volant: the controller and the agents it uploads to managed hosts.
#
#   curl -fsSL https://volant.sh/install.sh | sh
#
# Environment:
#   VOLANT_VERSION   release tag to install, such as v0.1.0-alpha.7 (default: the newest release)
#   VOLANT_HOME      where releases are unpacked (default: ~/.local/share/volant)
#   VOLANT_BIN_DIR   where the volant and volant-playbook links go (default: ~/.local/bin)

set -eu

repo="lorica-labs/volant"
home_dir="${VOLANT_HOME:-$HOME/.local/share/volant}"
bin_dir="${VOLANT_BIN_DIR:-$HOME/.local/bin}"

say() { printf 'volant-install: %s\n' "$*"; }
die() { printf 'volant-install: error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "this installer needs '$1'"; }

need curl
need tar
need uname

case "$(uname -s)" in
  Linux)
    os=unknown-linux-musl
    need xz # GNU tar hands .tar.xz to it
    ;;
  Darwin) os=apple-darwin ;;
  *) die "unsupported operating system: $(uname -s). Volant runs on Linux and macOS." ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported architecture: $(uname -m). Volant ships x86_64 and arm64 builds." ;;
esac
target="$arch-$os"

if [ -n "${VOLANT_VERSION:-}" ]; then
  tag="$VOLANT_VERSION"
else
  # Every release so far is a pre-release, which GitHub's "latest" skips: take the newest one.
  tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=1" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$tag" ] || die "could not find the newest release of $repo"
fi

archive="volant-$target.tar.xz"
url="https://github.com/$repo/releases/download/$tag/$archive"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading Volant $tag for $target"
curl -fsSL "$url" -o "$tmp/$archive" || die "download failed: $url"
curl -fsSL "$url.sha256" -o "$tmp/$archive.sha256" || die "download failed: $url.sha256"

expected=$(cut -d ' ' -f 1 <"$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$archive" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$archive" | cut -d ' ' -f 1)
else
  die "this installer needs 'sha256sum' or 'shasum' to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $archive: expected $expected, got $actual"

# The controller finds its agents next to its own executable, so the release is unpacked as a
# whole and only linked from the bin directory.
dest="$home_dir/$tag"
mkdir -p "$home_dir" "$bin_dir"
rm -rf "$dest"
tar -xJf "$tmp/$archive" -C "$tmp"
mv "$tmp/volant-$target" "$dest"
ln -sfn "$dest" "$home_dir/current"
ln -sf "$home_dir/current/volant" "$bin_dir/volant"
ln -sf "$home_dir/current/volant-playbook" "$bin_dir/volant-playbook"

say "installed $("$bin_dir/volant" --version) in $dest"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "add $bin_dir to your PATH, for example: export PATH=\"$bin_dir:\$PATH\"" ;;
esac
