#!/usr/bin/env bash
# Exercise install.sh against the packaged release without touching GitHub.
# Run from the repository root:
#   bash scripts/test-install.sh rs-canvas-aarch64-apple-darwin.tar.gz aarch64-apple-darwin v0.1.0-alpha.1
set -euo pipefail

[ "$(uname -s)" = "Darwin" ] || {
    echo "test-install: this test must run on macOS" >&2
    exit 1
}
[ "$#" -eq 3 ] || {
    echo "usage: $0 <tarball> <target> <tag>" >&2
    exit 2
}

TARBALL_INPUT="$1"
TARGET="$2"
TAG="$3"
VERSION="${TAG#v}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARBALL="$(cd "$(dirname "$TARBALL_INPUT")" && pwd)/$(basename "$TARBALL_INPUT")"
ASSET="rs-canvas-$TARGET.tar.gz"

[ -f "$TARBALL" ] || {
    echo "test-install: tarball not found: $TARBALL" >&2
    exit 1
}
[ "$(basename "$TARBALL")" = "$ASSET" ] || {
    echo "test-install: expected $ASSET, got $(basename "$TARBALL")" >&2
    exit 1
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/rs-canvas-test.XXXXXX")"
PORT="${RS_CANVAS_TEST_PORT:-5198}"
BAD_PORT=$((PORT - 1))
PIDS="$WORK/server-pids"

cleanup() {
    if [ -f "$PIDS" ]; then
        while read -r pid; do
            kill "$pid" 2>/dev/null || true
        done < "$PIDS"
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

start_server() { # directory port
    local dir="$1" port="$2"
    python3 -m http.server "$port" --bind 127.0.0.1 --directory "$dir" \
        >"$WORK/http-$port.log" 2>&1 &
    echo $! >> "$PIDS"
    for _ in $(seq 1 50); do
        if /usr/bin/curl -fsS "http://127.0.0.1:$port/releases.json" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    echo "test-install: mock release server did not start" >&2
    cat "$WORK/http-$port.log" >&2
    return 1
}

make_curl_wrapper() { # directory port
    local dir="$1" port="$2"
    mkdir -p "$dir"
    cat > "$dir/curl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
args=()
skip=0
for a in "\$@"; do
    if [ "\$skip" = 1 ]; then
        skip=0
        continue
    fi
    case "\$a" in
        --proto|--proto-redir) skip=1; continue ;;
        --proto=*|--proto-redir=*) continue ;;
        https://api.github.com/repos/*/releases*) a="http://127.0.0.1:$port/releases.json" ;;
        https://github.com/*/releases/download/*) a="http://127.0.0.1:$port/\${a##*/}" ;;
    esac
    args+=("\$a")
done
exec /usr/bin/curl "\${args[@]}"
EOF
    chmod +x "$dir/curl"
}

make_fake_tools() { # directory target
    local dir="$1" target="$2" machine
    case "$target" in
        aarch64-apple-darwin) machine="arm64" ;;
        x86_64-apple-darwin) machine="x86_64" ;;
        *)
            echo "test-install: unsupported target: $target" >&2
            return 1
            ;;
    esac
    cat > "$dir/uname" <<EOF
#!/bin/sh
case "\$1" in
    -s) echo Darwin ;;
    -m) echo "$machine" ;;
    *) /usr/bin/uname "\$@" ;;
esac
EOF
    cat > "$dir/sysctl" <<'EOF'
#!/bin/sh
case "$*" in
    *sysctl.proc_translated*) echo 0 ;;
    *) /usr/sbin/sysctl "$@" ;;
esac
EOF
    chmod +x "$dir/uname" "$dir/sysctl"
}

run_install() { # home prefix script wrapper [extra args...]
    local home="$1" prefix="$2" script="$3" wrapper="$4"
    shift 4
    mkdir -p "$home"
    (
        cd "$WORK"
        env -i HOME="$home" SHELL=/bin/zsh TERM=dumb \
            PATH="$wrapper:/usr/bin:/bin:/usr/sbin:/sbin" \
            bash "$script" --prefix "$prefix" --no-modify-path "$@"
    )
}

RELEASE="$WORK/release"
mkdir -p "$RELEASE"
cp "$TARBALL" "$RELEASE/$ASSET"
(
    cd "$RELEASE"
    shasum -a 256 "$ASSET" > SHA256SUMS
)
printf '{"tag_name":"v%s"}\n' "$VERSION" > "$RELEASE/releases.json"

WRAP="$WORK/wrap"
make_curl_wrapper "$WRAP" "$PORT"
make_fake_tools "$WRAP" "$TARGET"
start_server "$RELEASE" "$PORT"

echo "--- master-installer download, checksum, extract ---"
OUT="$(run_install "$WORK/home-default" "$WORK/prefix-default" "$REPO_ROOT/install.sh" "$WRAP")"
printf '%s\n' "$OUT"
printf '%s\n' "$OUT" | grep -Fq "Installed rs-canvas $VERSION"
"$WORK/prefix-default/rs-canvas" --version | grep -Fq "$VERSION"
# The current shell does not have the new PATH yet, so the hint must be the
# full path and the installed bytes must match the tarball.
printf '%s\n' "$OUT" | grep -Fq "$WORK/prefix-default/rs-canvas"
mkdir -p "$WORK/extract"
tar -xzf "$RELEASE/$ASSET" -C "$WORK/extract"
cmp "$WORK/extract/rs-canvas" "$WORK/prefix-default/rs-canvas"

echo "--- baked installer does not resolve the newest release ---"
sed "s|^DEFAULT_VERSION=\"\"$|DEFAULT_VERSION=\"$VERSION\"|" \
    "$REPO_ROOT/install.sh" > "$WORK/install-baked.sh"
grep -Fq "DEFAULT_VERSION=\"$VERSION\"" "$WORK/install-baked.sh"
printf '%s\n' '{"tag_name":"v9.9.9"}' > "$RELEASE/releases.json"
OUT="$(run_install "$WORK/home-baked" "$WORK/prefix-baked" "$WORK/install-baked.sh" "$WRAP")"
printf '%s\n' "$OUT" | grep -Fq "Installed rs-canvas $VERSION"
"$WORK/prefix-baked/rs-canvas" --version | grep -Fq "$VERSION"

echo "--- checksum mismatch is rejected ---"
BAD_RELEASE="$WORK/release-bad"
cp -R "$RELEASE" "$BAD_RELEASE"
printf 'tampered' >> "$BAD_RELEASE/$ASSET"
# Keep the original SHA256SUMS so the downloaded bytes no longer match.
WRAP_BAD="$WORK/wrap-bad"
make_curl_wrapper "$WRAP_BAD" "$BAD_PORT"
make_fake_tools "$WRAP_BAD" "$TARGET"
start_server "$BAD_RELEASE" "$BAD_PORT"
set +e
OUT="$(run_install "$WORK/home-bad" "$WORK/prefix-bad" "$WORK/install-baked.sh" "$WRAP_BAD" 2>&1)"
RC=$?
set -e
printf '%s\n' "$OUT"
[ "$RC" -ne 0 ] || {
    echo "test-install: tampered archive was accepted" >&2
    exit 1
}
printf '%s\n' "$OUT" | grep -Fq "checksum mismatch"
[ ! -e "$WORK/prefix-bad/rs-canvas" ] || {
    echo "test-install: tampered archive was installed" >&2
    exit 1
}

echo "--- a failed upgrade keeps the working binary ---"
BAD_BINARY="$WORK/bad-rs-canvas"
printf '#!/bin/sh\nexit 42\n' > "$BAD_BINARY"
chmod +x "$BAD_BINARY"
set +e
OUT="$(run_install "$WORK/home-upgrade" "$WORK/prefix-default" "$REPO_ROOT/install.sh" "$WRAP" --binary "$BAD_BINARY" 2>&1)"
RC=$?
set -e
printf '%s\n' "$OUT"
[ "$RC" -ne 0 ] || {
    echo "test-install: broken binary was accepted" >&2
    exit 1
}
printf '%s\n' "$OUT" | grep -Fq "existing install was left untouched"
"$WORK/prefix-default/rs-canvas" --version | grep -Fq "$VERSION"

echo "--- bash login-shell profile ---"
BASH_HOME="$WORK/home-bash"
BASH_PREFIX="$WORK/prefix-bash"
mkdir -p "$BASH_HOME"
env -i HOME="$BASH_HOME" SHELL=/bin/bash TERM=dumb \
    PATH="$WRAP:/usr/bin:/bin:/usr/sbin:/sbin" \
    bash "$REPO_ROOT/install.sh" --prefix "$BASH_PREFIX" --binary "$WORK/extract/rs-canvas" \
    >/dev/null
grep -Fq "$BASH_PREFIX" "$BASH_HOME/.bash_profile"
[ ! -e "$BASH_HOME/.bashrc" ] || {
    echo "test-install: bash profile was written to .bashrc" >&2
    exit 1
}

echo "--- ambient VERSION/PREFIX no longer override flags ---"
ENV_HOME="$WORK/home-env"
OUT="$(env -i HOME="$ENV_HOME" SHELL=/bin/zsh TERM=dumb VERSION=9.9.9 PREFIX="$WORK/not-used" \
    PATH="$WRAP:/usr/bin:/bin:/usr/sbin:/sbin" \
    bash "$WORK/install-baked.sh" --no-modify-path)"
printf '%s\n' "$OUT" | grep -Fq "Installed rs-canvas $VERSION"
[ -x "$ENV_HOME/.local/bin/rs-canvas" ] || {
    echo "test-install: default prefix was not used" >&2
    exit 1
}
[ ! -e "$WORK/not-used" ] || {
    echo "test-install: ambient PREFIX was honoured" >&2
    exit 1
}

echo "installer tests passed"
