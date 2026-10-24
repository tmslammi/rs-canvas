#!/usr/bin/env bash
# Install a prebuilt rs-canvas alpha from GitHub Releases.
set -euo pipefail

REPO="${RS_CANVAS_REPO:-tmslammi/rs-canvas}"
DEFAULT_PREFIX="$HOME/.local/bin"
DEFAULT_VERSION=""
VERSION="$DEFAULT_VERSION"
PREFIX=""
NO_MODIFY_PATH=false
BINARY=""

usage() {
    cat <<'EOF'
rs-canvas installer (macOS alpha)

Usage: install.sh [options]

Options:
  -v, --version <version>  Install a specific release (for example 0.1.0-alpha.1)
  -p, --prefix <dir>       Install the executable into <dir> (default: ~/.local/bin)
  -b, --binary <path>      Install a locally built binary instead of downloading
      --no-modify-path     Do not edit the shell profile to add <dir> to PATH
  -h, --help               Show this help

Examples:
  curl -fsSL https://raw.githubusercontent.com/tmslammi/rs-canvas/master/install.sh | bash
  curl -fsSL .../install.sh | bash -s -- --version 0.1.0-alpha.1

  # --binary installs the file as-is. Build with the embedded front end so
  # the installed copy behaves like a release binary:
  cargo build --release --features embed-web -p canvas-server
  ./install.sh --binary target/release/canvas-server
EOF
}

die() {
    echo "rs-canvas installer: $*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        -v|--version)
            [ "$#" -ge 2 ] || die "--version needs a value"
            VERSION="$2"
            shift 2
            ;;
        -p|--prefix)
            [ "$#" -ge 2 ] || die "--prefix needs a value"
            PREFIX="$2"
            shift 2
            ;;
        -b|--binary)
            [ "$#" -ge 2 ] || die "--binary needs a path"
            BINARY="$2"
            shift 2
            ;;
        --no-modify-path)
            NO_MODIFY_PATH=true
            shift
            ;;
        *)
            die "unknown option $1 (try --help)"
            ;;
    esac
done

VERSION="${VERSION#v}"
INSTALL_DIR="${PREFIX:-$DEFAULT_PREFIX}"
# Keep profile entries usable when --prefix is relative.
case "$INSTALL_DIR" in
    /*) ;;
    *) INSTALL_DIR="$PWD/$INSTALL_DIR" ;;
esac
mkdir -p "$INSTALL_DIR"

TMP=""
STAGED=""
cleanup() {
    if [ -n "$STAGED" ]; then
        rm -f "$STAGED"
    fi
    if [ -n "$TMP" ]; then
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT

if [ -n "$BINARY" ]; then
    [ -f "$BINARY" ] || die "binary not found: $BINARY"
    CANDIDATE="$BINARY"
else
    [ "$(uname -s)" = "Darwin" ] || die "prebuilt alpha builds are macOS-only for now"

    macos_version="$(sw_vers -productVersion 2>/dev/null || echo 0)"
    macos_major="${macos_version%%.*}"
    if [ "$macos_major" -lt 12 ] 2>/dev/null; then
        die "rs-canvas requires macOS 12 or newer (this Mac runs $macos_version)"
    fi

    case "$(uname -m)" in
        arm64|aarch64)
            TARGET="aarch64-apple-darwin"
            ;;
        x86_64)
            # A terminal running under Rosetta still sits on an Apple Silicon
            # machine; prefer the native build there.
            if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
                TARGET="aarch64-apple-darwin"
            else
                TARGET="x86_64-apple-darwin"
            fi
            ;;
        *)
            die "unsupported architecture: $(uname -m)"
            ;;
    esac

    command -v curl >/dev/null 2>&1 || die "curl is required"
    command -v tar >/dev/null 2>&1 || die "tar is required"
    command -v shasum >/dev/null 2>&1 || die "shasum is required"

    if [ -z "$VERSION" ]; then
        API="https://api.github.com/repos/$REPO/releases?per_page=1"
        JSON="$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
            -fsSL --retry 3 --connect-timeout 10 "$API")" \
            || die "could not read the GitHub Releases list"
        VERSION="$(printf '%s\n' "$JSON" \
            | sed -nE 's/.*"tag_name": *"v?([^"]+)".*/\1/p' \
            | head -n 1)"
        [ -n "$VERSION" ] || die "no published release found for $REPO"
    fi

    case "$VERSION" in
        *[!0-9A-Za-z.-]*)
            die "invalid version: $VERSION"
            ;;
    esac

    ASSET="rs-canvas-$TARGET.tar.gz"
    BASE="https://github.com/$REPO/releases/download/v$VERSION"
    TMP="$(mktemp -d "${TMPDIR:-/tmp}/rs-canvas-install.XXXXXX")"

    echo "Downloading rs-canvas $VERSION ($TARGET)..."
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
        -fsSL --retry 3 --connect-timeout 10 "$BASE/$ASSET" -o "$TMP/$ASSET" \
        || die "could not download $BASE/$ASSET"
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
        -fsSL --retry 3 --connect-timeout 10 "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" \
        || die "could not download $BASE/SHA256SUMS"

    EXPECTED="$(awk -v name="$ASSET" '$2 == name { print $1; exit }' "$TMP/SHA256SUMS")"
    [ -n "$EXPECTED" ] || die "SHA256SUMS has no entry for $ASSET"
    ACTUAL="$(shasum -a 256 "$TMP/$ASSET" | awk '{ print $1 }')"
    [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $ASSET"

    tar -xzf "$TMP/$ASSET" -C "$TMP"
    [ -f "$TMP/rs-canvas" ] || die "archive did not contain rs-canvas"
    CANDIDATE="$TMP/rs-canvas"
fi

# Stage and verify beside the destination, then rename over the old binary.
# A failed verification (wrong architecture, too-old macOS, broken download)
# leaves the existing install in place.
STAGED="$INSTALL_DIR/.rs-canvas.new.$$"
install -m 755 "$CANDIDATE" "$STAGED" \
    || die "could not stage the binary in $INSTALL_DIR"
INSTALLED_VERSION=""
if ! INSTALLED_VERSION="$("$STAGED" --version 2>/dev/null)"; then
    die "the new binary does not run on this Mac; the existing install was left untouched"
fi
mv -f "$STAGED" "$INSTALL_DIR/rs-canvas" \
    || die "could not replace $INSTALL_DIR/rs-canvas"
STAGED=""
echo "Installed $INSTALLED_VERSION to $INSTALL_DIR/rs-canvas"

PATH_HAS_INSTALL_DIR=false
if printf '%s' ":${PATH:-}:" | grep -Fq ":$INSTALL_DIR:"; then
    PATH_HAS_INSTALL_DIR=true
fi

if [ "$PATH_HAS_INSTALL_DIR" = false ]; then
    if [ "$NO_MODIFY_PATH" = true ]; then
        echo
        echo "Add rs-canvas to your PATH:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    else
        case "$(basename "${SHELL:-sh}")" in
            zsh)
                RC="${ZDOTDIR:-$HOME}/.zshrc"
                ;;
            bash)
                # Login bash reads .bash_profile, never .bashrc on its own.
                RC="$HOME/.bash_profile"
                ;;
            *)
                RC="$HOME/.profile"
                ;;
        esac

        if [ -e "$RC" ] && [ ! -w "$RC" ]; then
            echo
            echo "Could not edit $RC; add this to your shell profile:"
            echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        elif ! grep -Fqx "export PATH=\"$INSTALL_DIR:\$PATH\"" "$RC" 2>/dev/null; then
            {
                printf '\n# rs-canvas\n'
                # shellcheck disable=SC2016 # $PATH is literal profile text
                printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
            } >> "$RC"
            echo "Added $INSTALL_DIR to PATH in $RC."
            echo "Open a new terminal (or run: exec \$SHELL) before using rs-canvas."
        fi
    fi
fi

echo
echo "Start the app:"
if [ "$PATH_HAS_INSTALL_DIR" = true ]; then
    echo "  rs-canvas"
else
    printf '  %q\n' "$INSTALL_DIR/rs-canvas"
fi
echo
echo "It opens http://127.0.0.1:5173; the MCP endpoint is http://127.0.0.1:5173/mcp."
echo "Documents live in ~/.rs-canvas."
