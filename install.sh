#!/bin/sh
# Rift installer — https://github.com/exYze/rift
#
#   curl -fsSL https://raw.githubusercontent.com/exYze/rift/master/install.sh | sh
#
# Detects OS/arch, downloads the latest release binary, and installs it as
# `rift`. Override the install directory with RIFT_INSTALL=/some/dir.

set -e

REPO="exYze/rift"

main() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64)  target="aarch64-apple-darwin" ;;
                x86_64) target="x86_64-apple-darwin" ;;
                *) err "unsupported macOS architecture: $arch" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
                x86_64)        target="x86_64-unknown-linux-musl" ;;
                *) err "unsupported Linux architecture: $arch" ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*)
            err "on Windows, download rift-x86_64-pc-windows-msvc.zip from https://github.com/$REPO/releases/latest and put rift.exe on your PATH"
            ;;
        *)
            err "unsupported OS: $os"
            ;;
    esac

    # Pick an install dir: $RIFT_INSTALL > ~/.local/bin > /usr/local/bin.
    if [ -n "${RIFT_INSTALL:-}" ]; then
        install_dir="$RIFT_INSTALL"
    elif [ -d "$HOME/.local/bin" ] || mkdir -p "$HOME/.local/bin" 2>/dev/null; then
        install_dir="$HOME/.local/bin"
    else
        install_dir="/usr/local/bin"
    fi
    [ -w "$install_dir" ] || err "cannot write to $install_dir (set RIFT_INSTALL to another directory)"

    url="https://github.com/$REPO/releases/latest/download/rift-$target.tar.gz"
    echo "downloading $url"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL "$url" | tar xz -C "$tmp"
    chmod +x "$tmp/rift"
    # Remove any previous binary first: overwriting it in place (which mv
    # does when $tmp is on another filesystem) keeps the old inode, and
    # macOS kills binaries whose cached code signature no longer matches.
    rm -f "$install_dir/rift"
    mv "$tmp/rift" "$install_dir/rift"

    echo "installed $("$install_dir/rift" --version) to $install_dir/rift"

    case ":$PATH:" in
        *":$install_dir:"*) echo "run it with: rift" ;;
        *)
            echo ""
            echo "NOTE: $install_dir is not on your PATH. Add it with:"
            echo "  export PATH=\"$install_dir:\$PATH\""
            ;;
    esac
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
