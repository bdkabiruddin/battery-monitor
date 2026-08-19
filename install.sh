#!/bin/sh
# Installs the latest Battery Monitor release for Linux/x86_64.
#
#   curl -fsSL https://raw.githubusercontent.com/bdkabiruddin/battery-monitor/main/install.sh | sh
#
# Prefers the .deb on apt-based distros (installs real dependencies via
# apt); falls back to the AppImage everywhere else. Review this script
# before piping it to a shell if you'd rather not trust a one-liner --
# it's short and does nothing beyond what's described above.
set -eu

REPO="bdkabiruddin/battery-monitor"
NAME="battery-monitor"
BIN="battery-monitor"

if [ "$(uname -s)" != "Linux" ]; then
    echo "Error: $NAME only publishes Linux builds." >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64) ARCH=amd64 ;;
    *)
        echo "Error: only x86_64/amd64 builds are published (this machine is $(uname -m))." >&2
        echo "You can build from source instead: https://github.com/$REPO#building-from-source" >&2
        exit 1
        ;;
esac

api_url="https://api.github.com/repos/$REPO/releases/latest"
release_json=$(curl -fsSL "$api_url")

find_asset_url() {
    # Extracts the browser_download_url for the first asset whose name
    # matches the given grep pattern, without depending on jq.
    printf '%s' "$release_json" \
        | grep -o '"browser_download_url"[^,]*' \
        | grep "$1" \
        | head -n1 \
        | cut -d'"' -f4
}

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

if command -v apt >/dev/null 2>&1 || command -v dpkg >/dev/null 2>&1; then
    deb_url=$(find_asset_url "_${ARCH}\.deb")
    if [ -z "$deb_url" ]; then
        echo "Error: no .deb asset found in the latest release." >&2
        exit 1
    fi

    deb_path="$tmpdir/$NAME.deb"
    echo "Downloading $deb_url"
    curl -fsSL -o "$deb_path" "$deb_url"

    echo "Installing (needs sudo)..."
    if command -v apt >/dev/null 2>&1; then
        sudo apt install -y "$deb_path"
    else
        sudo dpkg -i "$deb_path" || sudo apt-get install -f -y
    fi

    echo "Installed. Launch with: $BIN"
else
    appimage_url=$(find_asset_url "_${ARCH}\.AppImage")
    if [ -z "$appimage_url" ]; then
        echo "Error: no AppImage asset found in the latest release." >&2
        exit 1
    fi

    install_dir="$HOME/.local/bin"
    mkdir -p "$install_dir"
    dest="$install_dir/$NAME.AppImage"

    echo "Downloading $appimage_url"
    curl -fsSL -o "$dest" "$appimage_url"
    chmod +x "$dest"

    echo "Installed to $dest"
    case ":$PATH:" in
        *":$install_dir:"*) echo "Launch with: $NAME.AppImage" ;;
        *) echo "Add $install_dir to your PATH, or run: $dest" ;;
    esac
fi
