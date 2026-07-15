#!/bin/sh
set -eu

repo="${TCODE_INSTALL_REPO:-Teamon9161/tcode}"
version="${TCODE_VERSION:-latest}"
install_dir="${TCODE_INSTALL_DIR:-$HOME/.local/bin}"

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required command: $1" >&2
        exit 1
    fi
}

download() {
    url="$1"
    out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --progress-bar "$url" -o "$out"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$out" "$url"
    else
        echo "missing required command: curl or wget" >&2
        exit 1
    fi
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "missing required command: sha256sum or shasum" >&2
        exit 1
    fi
}

case "$(uname -s)" in
    Linux) os="linux" ;;
    Darwin) os="macos" ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset="tcode-$arch-$os"
if [ "$version" = "latest" ]; then
    base_url="https://github.com/$repo/releases/latest/download"
else
    case "$version" in v*) tag="$version" ;; *) tag="v$version" ;; esac
    base_url="https://github.com/$repo/releases/download/$tag"
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

download "$base_url/$asset" "$tmp_dir/$asset"
download "$base_url/checksums.txt" "$tmp_dir/checksums.txt"
expected="$(awk -v file="$asset" '$2 == file { print $1 }' "$tmp_dir/checksums.txt")"
[ -n "$expected" ] || { echo "checksum not found for $asset" >&2; exit 1; }
actual="$(sha256_file "$tmp_dir/$asset")"
[ "$actual" = "$expected" ] || { echo "checksum mismatch for $asset" >&2; exit 1; }

mkdir -p "$install_dir"
if command -v install >/dev/null 2>&1; then
    install -m 755 "$tmp_dir/$asset" "$install_dir/tcode"
else
    cp "$tmp_dir/$asset" "$install_dir/tcode"
    chmod 755 "$install_dir/tcode"
fi

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *)
        echo "Installed tcode to $install_dir, but that directory is not in PATH."
        echo "Add this to your shell profile:"
        echo "  export PATH=\"$install_dir:\$PATH\""
        ;;
esac

echo "tcode installed to $install_dir/tcode"
