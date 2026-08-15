#!/usr/bin/env bash
# Regenerate the README screenshots from a live session.
#
# Runs the real binary against the real portal in a fixed-size kitty window and
# photographs the window itself, one shot per screen. Doing it this way rather
# than by hand means the images are reproducible, identically sized, and always
# show the app as it actually renders.
#
# Photographing a real window rather than replaying an ANSI dump is what makes
# the chart screens honest: charts are drawn with the terminal graphics protocol
# (see ui/gfx.rs), and no amount of ANSI capture can reproduce a bitmap the
# terminal was handed out of band.
#
# Needs: kitty, xdotool, ImageMagick and a monospaced Nerd Font, on X11.
#
#   ./docs/capture.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$ROOT/docs
TMP=$(mktemp -d)
SOCK=unix:/tmp/psxshots-$$

# Ask cargo where it builds rather than assuming ./target: a shared target
# directory is a common enough setup that failing on it looks like the build
# failed.
TARGET=$(cargo metadata --no-deps --format-version 1 2>/dev/null |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
BIN=${TARGET:-$ROOT/target}/release/psxtui
[ -x "$BIN" ] || { echo "build it first: cargo build --release" >&2; exit 1; }

COLS=168
ROWS=44
FONT=${PSXTUI_SHOT_FONT:-JetBrainsMono Nerd Font Mono}
FONT_SIZE=${PSXTUI_SHOT_FONT_SIZE:-11}
# The theme the README is shot in. `midnight` owns its background, so every
# screenshot has the same ground whatever the machine's kitty is themed as.
THEME=${PSXTUI_SHOT_THEME:-midnight}
BG='#10121c'

cleanup() {
    kitty @ --to "$SOCK" close-window 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT

PSXTUI_THEME=$THEME kitty --config NONE \
    -o allow_remote_control=yes --listen-on "$SOCK" \
    -o "font_family=$FONT" -o "font_size=$FONT_SIZE" \
    -o remember_window_size=no \
    -o "initial_window_width=${COLS}c" -o "initial_window_height=${ROWS}c" \
    -o window_padding_width=8 -o confirm_os_window_close=0 \
    -o "background=$BG" \
    --title psxtui-shots -- "$BIN" &

# Wait for the window, then for the board, the selected symbol's history and
# the macro fetches — none of it is worth photographing before it lands.
for _ in $(seq 40); do
    WIN=$(xdotool search --name psxtui-shots | head -1 || true)
    [ -n "${WIN:-}" ] && break
    sleep 0.5
done
[ -n "${WIN:-}" ] || { echo "the capture window never appeared" >&2; exit 1; }
# Raised once so nothing overlaps the photographs; keys go over the kitty
# socket, so the window never needs focus after this.
xdotool windowraise "$WIN"
sleep 25

send() { kitty @ --to "$SOCK" send-text "$1"; }
# Named keys go through send-key: Enter and Escape have to arrive as key
# presses, and send-text would deliver a bare byte the app reads as text.
key() { kitty @ --to "$SOCK" send-key "$1"; }

grab() {
    name=$1; shift
    for k in "$@"; do send "$k"; sleep 1.2; done
    sleep 3
    import -window "$WIN" "$TMP/$name.png"
    echo "captured $name"
}

grab dashboard 1
grab screener 2
grab chart 3
grab analysis 4
grab company 5
grab intraday 6
# The comparison is shown with a grown set rather than the four it seeds
# itself with, to show what eight series look like on one pair of axes.
send 7; sleep 2
for q in mari psx sys atrl; do
    send a; sleep 1.2
    send "$q"; sleep 1.5
    key enter; sleep 1.5
    key esc; sleep 1
done
grab compare-full
grab compare-picker a
key esc; sleep 1
grab seasonality 8
grab macro 9
# The backtester is worth a picture only once it has run something: an unrun
# strategy is an empty right-hand pane.
send 0; sleep 2
key enter; sleep 4
grab backtest
grab help '?'
key esc; send q
sleep 1

for f in "$TMP"/*.png; do
    name=$(basename "$f" .png)
    # Square every screen off at one width so the README doesn't jump, and
    # quantise: these are flat-coloured UI shots, and 192 colours halves the
    # file for no visible loss.
    magick "$f" -resize 1400x -colors 192 png8:"$OUT/$name.png"
    echo "wrote docs/$name.png"
done
