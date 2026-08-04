#!/usr/bin/env sh
#
# psxtui installer — toolchain, build, and a binary on your PATH.
#
# Deliberately POSIX sh and dependency-free: the whole point is that it runs on
# a machine that has nothing on it yet. Everything it does by hand is printed
# first, so nothing happens that you could not have typed yourself:
#
#   ./install.sh              install for the current user (~/.cargo/bin)
#   ./install.sh --system     install to /usr/local/bin (uses sudo)
#   ./install.sh --no-modify-path
#                             skip the shell-profile PATH line
#   ./install.sh --uninstall  remove the binary (data and cache are kept)
#
set -eu

MSRV=1.88
REPO=https://github.com/AnnanKhan/PSXtui
PREFIX=""
MODIFY_PATH=1
UNINSTALL=0

say()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

for arg in "$@"; do
    case "$arg" in
        --system) PREFIX=/usr/local/bin ;;
        --no-modify-path) MODIFY_PATH=0 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $arg (try --help)" ;;
    esac
done

# --- uninstall -------------------------------------------------------------

if [ "$UNINSTALL" -eq 1 ]; then
    removed=0
    for dir in "${CARGO_HOME:-$HOME/.cargo}/bin" /usr/local/bin; do
        [ -f "$dir/psxtui" ] || continue
        if [ -w "$dir" ]; then rm -f "$dir/psxtui"; else sudo rm -f "$dir/psxtui"; fi
        say "removed $dir/psxtui"
        removed=1
    done
    [ "$removed" -eq 1 ] || warn "no psxtui binary found on this machine"
    say "cache and watchlist left alone in your platform data directory"
    exit 0
fi

# --- 1. the Rust toolchain -------------------------------------------------

# The MSRV is not cosmetic: the code uses let-chains, which landed in 1.88.
version_ok() {
    have rustc || return 1
    rustc_v=$(rustc --version | cut -d' ' -f2)
    [ "$(printf '%s\n%s\n' "$MSRV" "$rustc_v" | sort -V | head -n1)" = "$MSRV" ]
}

if version_ok; then
    say "using $(rustc --version)"
elif have rustup; then
    say "rustc is older than $MSRV — updating the stable toolchain"
    rustup update stable
    rustup default stable
    version_ok || die "rustup update left rustc below $MSRV"
else
    say "no usable Rust toolchain found; installing one with rustup"
    say "this is the official installer from https://sh.rustup.rs"
    if have curl; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    elif have wget; then
        wget -qO- https://sh.rustup.rs | sh -s -- -y --profile minimal
    else
        die "need curl or wget to fetch rustup — or install Rust $MSRV+ yourself"
    fi
    # rustup writes this; source it so the rest of the script sees cargo.
    . "${CARGO_HOME:-$HOME/.cargo}/env"
    version_ok || die "installed toolchain is below $MSRV"
fi

have cargo || die "cargo is not on PATH — open a new shell and re-run"

# psxtui talks HTTPS through rustls and bundles SQLite, so there are no system
# -dev packages to chase. A C toolchain is still needed to compile the bundled
# SQLite itself.
if ! have cc && ! have gcc && ! have clang; then
    warn "no C compiler found — the bundled SQLite needs one"
    warn "  Debian/Ubuntu: sudo apt install build-essential"
    warn "  Fedora:        sudo dnf install gcc"
    warn "  macOS:         xcode-select --install"
fi

# --- 2. sources ------------------------------------------------------------

# Run from a clone if there is one, otherwise fetch into a temp dir. That makes
# the same script work both as ./install.sh and piped from the network.
if [ -f "$(dirname "$0")/Cargo.toml" ]; then
    SRC=$(cd "$(dirname "$0")" && pwd)
else
    have git || die "no Cargo.toml beside this script and no git to clone with"
    SRC=$(mktemp -d)
    say "cloning $REPO into $SRC"
    git clone --depth 1 "$REPO" "$SRC"
    trap 'rm -rf "$SRC"' EXIT
fi

# --- 3. build and install --------------------------------------------------

say "building (a first release build takes a few minutes)"
cargo build --release --manifest-path "$SRC/Cargo.toml"

BIN="$SRC/target/release/psxtui"
[ -x "$BIN" ] || die "the build finished but $BIN is missing"

if [ -n "$PREFIX" ]; then
    say "installing to $PREFIX (sudo)"
    sudo install -m 755 "$BIN" "$PREFIX/psxtui"
    DEST="$PREFIX/psxtui"
else
    # `cargo install` is the idiomatic per-user path: it puts the binary in
    # ~/.cargo/bin, which rustup already adds to PATH.
    say "installing to ${CARGO_HOME:-$HOME/.cargo}/bin"
    cargo install --path "$SRC" --force
    DEST="${CARGO_HOME:-$HOME/.cargo}/bin/psxtui"
fi

# --- 4. PATH ---------------------------------------------------------------

case ":$PATH:" in
    *":$(dirname "$DEST"):"*) on_path=1 ;;
    *) on_path=0 ;;
esac

if [ "$on_path" -eq 0 ] && [ "$MODIFY_PATH" -eq 1 ] && [ -z "$PREFIX" ]; then
    line="export PATH=\"\$HOME/.cargo/bin:\$PATH\""
    for profile in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        [ -f "$profile" ] || continue
        grep -qF '.cargo/bin' "$profile" && continue
        printf '\n# added by psxtui installer\n%s\n' "$line" >>"$profile"
        say "added ~/.cargo/bin to PATH in $profile"
    done
    warn "open a new shell, or run: $line"
elif [ "$on_path" -eq 0 ]; then
    warn "$(dirname "$DEST") is not on your PATH"
fi

# --- 5. done ---------------------------------------------------------------

say "installed: $DEST"
"$DEST" --version 2>/dev/null || true
cat <<'EOF'

  Run it with:   psxtui
  Keys:          ? inside the app
  Data lives in: the path printed by `psxtui --help`

The first launch fetches the market board and backfills ~120 days of history in
the background — it is usable immediately and gets richer as that lands.
EOF
