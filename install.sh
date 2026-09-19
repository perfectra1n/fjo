#!/bin/sh
# Install fjo from a published GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/perfectra1n/fjo/main/install.sh | sh
#
# Deliberately POSIX sh and deliberately never sudo: the default target is the
# caller's own ~/.local/bin, so `curl | sh` never needs root. Reviewing a script
# you are about to pipe into a shell is only reasonable if the script is small
# enough to read, which is the other reason there is no cleverness here.
#
# Knobs:
#   FJO_VERSION      tag to install (default: the latest release)
#   FJO_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
#   FJO_NO_COMPLETIONS  set to 1 to skip shell completions
set -eu

REPO="perfectra1n/fjo"
INSTALL_DIR="${FJO_INSTALL_DIR:-$HOME/.local/bin}"
# The gnu artifacts are built on ubuntu-22.04. Anything older than its glibc
# cannot run them, so it gets the static musl build instead. Keep this in step
# with the runner in .github/workflows/release.yaml -- `mise run glibc-floor-check`
# is what stops the built artifact from drifting past it.
GLIBC_FLOOR_MAJOR=2
GLIBC_FLOOR_MINOR=35

say()  { printf 'fjo: %s\n' "$1"; }
die()  { printf 'fjo: error: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }

# --- what are we running on? -------------------------------------------------

detect_os() {
    case "$(uname -s)" in
        Linux)  echo linux ;;
        Darwin) echo darwin ;;
        *) die "unsupported OS: $(uname -s). Windows users: see install.ps1" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo x86_64 ;;
        aarch64|arm64) echo aarch64 ;;
        *) die "unsupported architecture: $(uname -m)" ;;
    esac
}

# Returns "musl" or "gnu". A musl loader present is decisive. Otherwise ask
# glibc its version and compare against the floor the gnu artifacts are built
# against -- an older glibc would fail at exec with a GLIBC_x.yz symbol error,
# so those hosts take the static build.
detect_libc() {
    for loader in /lib/ld-musl-*.so.1 /lib64/ld-musl-*.so.1; do
        [ -e "$loader" ] && { echo musl; return; }
    done

    _v=""
    if command -v getconf >/dev/null 2>&1; then
        _v=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || _v=""
    fi
    if [ -z "$_v" ] && command -v ldd >/dev/null 2>&1; then
        _v=$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+' | head -1) || _v=""
    fi

    # No glibc answer at all means this is not glibc; musl is the safe choice.
    [ -z "$_v" ] && { echo musl; return; }

    _maj=${_v%%.*}
    _min=${_v#*.}
    _min=${_min%%.*}
    if [ "$_maj" -gt "$GLIBC_FLOOR_MAJOR" ] 2>/dev/null ||
       { [ "$_maj" -eq "$GLIBC_FLOOR_MAJOR" ] && [ "$_min" -ge "$GLIBC_FLOOR_MINOR" ]; } 2>/dev/null; then
        echo gnu
    else
        echo musl
    fi
}

target_triple() {
    case "$1" in
        linux)  echo "$2-unknown-linux-$3" ;;
        darwin) echo "$2-apple-darwin" ;;
    esac
}

# --- release resolution ------------------------------------------------------

latest_tag() {
    _api="https://api.github.com/repos/$REPO/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        _body=$(curl -fsSL "$_api")
    else
        _body=$(wget -qO- "$_api")
    fi
    printf '%s' "$_body" | grep -m1 '"tag_name"' | cut -d'"' -f4
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    else
        wget -qO "$2" "$1"
    fi
}

# --- main --------------------------------------------------------------------

command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 ||
    die "need curl or wget"
need tar

OS=$(detect_os)
ARCH=$(detect_arch)
if [ "$OS" = linux ]; then
    LIBC=$(detect_libc)
else
    LIBC=""
fi
TARGET=$(target_triple "$OS" "$ARCH" "$LIBC")

TAG="${FJO_VERSION:-$(latest_tag)}"
[ -n "$TAG" ] || die "could not resolve the latest release tag"

STAGE="fjo-$TAG-$TARGET"
URL="https://github.com/$REPO/releases/download/$TAG/$STAGE.tar.gz"

say "installing $TAG for $TARGET"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

fetch "$URL" "$TMP/$STAGE.tar.gz" ||
    die "download failed: $URL
The release may not publish this target. See https://github.com/$REPO/releases"

# Verify against the checksum published beside the archive. A failure here is
# fatal rather than a warning -- a mismatch means the bytes are not the bytes
# that were released, and there is no safe way to continue.
if fetch "$URL.sha256" "$TMP/$STAGE.tar.gz.sha256" 2>/dev/null; then
    expected=$(cut -d' ' -f1 < "$TMP/$STAGE.tar.gz.sha256")
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$TMP/$STAGE.tar.gz" | cut -d' ' -f1)
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$TMP/$STAGE.tar.gz" | cut -d' ' -f1)
    else
        actual=""
        say "warning: no sha256sum or shasum; skipping checksum verification"
    fi
    if [ -n "$actual" ] && [ "$expected" != "$actual" ]; then
        die "checksum mismatch
  expected $expected
  actual   $actual"
    fi
    [ -n "$actual" ] && say "checksum ok"
else
    say "warning: no published checksum for $STAGE.tar.gz; skipping verification"
fi

tar -xzf "$TMP/$STAGE.tar.gz" -C "$TMP"
[ -f "$TMP/$STAGE/fjo" ] || die "archive did not contain the expected fjo binary"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$TMP/$STAGE/fjo" "$INSTALL_DIR/fjo" 2>/dev/null ||
    { cp "$TMP/$STAGE/fjo" "$INSTALL_DIR/fjo" && chmod 0755 "$INSTALL_DIR/fjo"; }
say "installed $INSTALL_DIR/fjo"

# --- completions -------------------------------------------------------------
# The release archives already carry these; anyone untarring by hand never finds
# them, which is most of the reason this script exists.

if [ "${FJO_NO_COMPLETIONS:-0}" != "1" ] && [ -d "$TMP/$STAGE/completions" ]; then
    data="${XDG_DATA_HOME:-$HOME/.local/share}"
    shell=$(basename "${SHELL:-}")
    case "$shell" in
        bash)
            d="$data/bash-completion/completions"
            mkdir -p "$d" && cp "$TMP/$STAGE/completions/fjo.bash" "$d/fjo" &&
                say "installed bash completions to $d/fjo"
            ;;
        zsh)
            d="$data/zsh/site-functions"
            mkdir -p "$d" && cp "$TMP/$STAGE/completions/fjo.zsh" "$d/_fjo" &&
                say "installed zsh completions to $d/_fjo" &&
                say "  add to ~/.zshrc if needed: fpath=($d \$fpath)"
            ;;
        fish)
            d="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions"
            mkdir -p "$d" && cp "$TMP/$STAGE/completions/fjo.fish" "$d/fjo.fish" &&
                say "installed fish completions to $d/fjo.fish"
            ;;
        *)
            say "no completions installed for shell '${shell:-unknown}'; run: fjo completion --help"
            ;;
    esac
fi

# --- PATH --------------------------------------------------------------------
# Report, never edit. Rewriting someone's shell rc from a piped script is the
# kind of thing that makes people stop piping scripts.

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. Add it:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

say "done -- run 'fjo auth login --host git.example.org' to get started"
