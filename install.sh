#!/bin/sh
# Installs or updates rex-relay from a GitHub release, after checking the download's SHA-256.
#
#   curl -fsSL https://raw.githubusercontent.com/mbriggs/rex-relay/main/install.sh | sh
#
# Settings, all optional:
#   REX_RELAY_VERSION      a release tag such as v0.1.0 (default: the latest release)
#   REX_RELAY_INSTALL_DIR  where to put rex-relay (default: ~/.local/bin)
#   REX_RELAY_BASE_URL     where to download from instead of GitHub, such as a mirror
set -eu

repo=mbriggs/rex-relay
version=${REX_RELAY_VERSION:-latest}
dir=${REX_RELAY_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-musl ;;
  Darwin-arm64 | Darwin-aarch64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *)
    echo "rex-relay: no prebuilt binary for $(uname -s) $(uname -m)." >&2
    echo "Build it instead: cargo install --locked --git https://github.com/$repo" >&2
    exit 1
    ;;
esac

if [ "$version" = latest ]; then
  base=https://github.com/$repo/releases/latest/download
else
  base=https://github.com/$repo/releases/download/$version
fi
base=${REX_RELAY_BASE_URL:-$base}
name=rex-relay-$target

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
curl -fsSL "$base/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  check="sha256sum -c"
else
  check="shasum -a 256 -c"
fi
if ! (cd "$tmp" && $check "$name.tar.gz.sha256" >/dev/null 2>&1); then
  echo "rex-relay: the download doesn't match its SHA-256 checksum; nothing was installed." >&2
  exit 1
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
cp "$tmp/$name/rex-relay" "$dir/rex-relay.new"
chmod 755 "$dir/rex-relay.new"
mv "$dir/rex-relay.new" "$dir/rex-relay"

echo "Installed $("$dir/rex-relay" --version) to $dir/rex-relay"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "$dir isn't on your PATH. Add it to run rex-relay by name." ;;
esac
