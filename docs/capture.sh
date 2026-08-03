#!/usr/bin/env bash
# Regenerate the README screenshots from a live session.
#
# Runs the real binary against the real portal in a fixed-size tmux pane, dumps
# one ANSI frame per screen, and renders each through a headless browser. Doing
# it this way rather than by hand means the images are reproducible, identically
# sized, and always show the app as it actually renders.
#
# Needs: tmux, chromium, ImageMagick, python3, and a monospaced Nerd Font.
#
#   ./docs/capture.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$ROOT/docs
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"; tmux kill-session -t psxshots 2>/dev/null || true' EXIT

BIN=$ROOT/target/release/psxtui
[ -x "$BIN" ] || { echo "build it first: cargo build --release" >&2; exit 1; }

COLS=168
ROWS=44
# Rendered width in cells x the browser's advance width, plus the frame padding.
WINDOW=1750,860
SESSION=psxshots

tmux kill-session -t $SESSION 2>/dev/null || true
tmux new-session -d -s $SESSION -x $COLS -y $ROWS "TERM=xterm-256color $BIN"
# The board, the selected symbol's history and the macro fetches all have to
# land before anything is worth photographing.
sleep 25

grab() {
    name=$1; shift
    for k in "$@"; do tmux send-keys -t $SESSION "$k"; sleep 1.2; done
    sleep 3
    tmux capture-pane -p -e -t $SESSION >"$TMP/$name.ansi"
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
tmux send-keys -t $SESSION 7; sleep 2
for q in mari psx sys atrl; do
    tmux send-keys -t $SESSION a; sleep 1.2
    tmux send-keys -t $SESSION "$q"; sleep 1.5
    tmux send-keys -t $SESSION Enter; sleep 1.2
    tmux send-keys -t $SESSION Escape; sleep 1
done
grab compare-full
grab compare-picker a
tmux send-keys -t $SESSION Escape; sleep 1
grab seasonality 8
grab macro 9
grab help '?'
tmux send-keys -t $SESSION Escape q
sleep 1

for f in "$TMP"/*.ansi; do
    name=$(basename "$f" .ansi)
    python3 "$OUT/ansi2html.py" <"$f" >"$TMP/$name.html"
    chromium --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --force-device-scale-factor=2 --window-size=$WINDOW \
        --screenshot="$TMP/$name.raw.png" "$TMP/$name.html" >/dev/null 2>&1
    # Trim the browser's page background back to the frame, pad it evenly, then
    # square every screen off at one height so the README doesn't jump.
    magick "$TMP/$name.raw.png" -fuzz 1% -trim +repage \
        -bordercolor '#0d1117' -border 24 \
        -background '#0d1117' -gravity north -extent x1662 \
        -resize 1400x -colors 192 png8:"$OUT/$name.png"
    echo "wrote docs/$name.png"
done
