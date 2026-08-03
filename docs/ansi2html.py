#!/usr/bin/env python3
"""Convert a tmux `capture-pane -e` dump into a standalone HTML page.

Only what a TUI actually emits is handled: 24-bit fg/bg, the 8/16 colour
palette, bold, reverse and reset. Anything else is dropped rather than guessed.
"""
import html
import sys

PALETTE = [
    "#1c1f24", "#f85149", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
    "#6e7681", "#ff7b72", "#56d364", "#e3b341", "#79c0ff", "#d2a8ff", "#56d4dd", "#f0f6fc",
]
BG = "#0d1117"
FG = "#dcdfe4"


def xterm256(n):
    if n < 16:
        return PALETTE[n]
    if n < 232:
        n -= 16
        r, g, b = (n // 36) % 6, (n // 6) % 6, n % 6
        f = lambda v: 0 if v == 0 else 55 + 40 * v
        return "#%02x%02x%02x" % (f(r), f(g), f(b))
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


class State:
    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = None
        self.bg = None
        self.bold = False
        self.reverse = False

    def style(self):
        fg = self.fg or FG
        bg = self.bg or BG
        if self.reverse:
            fg, bg = bg, fg
        out = f"color:{fg}"
        if bg != BG:
            out += f";background:{bg}"
        if self.bold:
            out += ";font-weight:700"
        return out


def apply_sgr(st, params):
    i = 0
    while i < len(params):
        p = params[i]
        if p in (0, None):
            st.reset()
        elif p == 1:
            st.bold = True
        elif p == 7:
            st.reverse = True
        elif p == 22:
            st.bold = False
        elif p == 27:
            st.reverse = False
        elif p == 39:
            st.fg = None
        elif p == 49:
            st.bg = None
        elif 30 <= p <= 37:
            st.fg = PALETTE[p - 30]
        elif 90 <= p <= 97:
            st.fg = PALETTE[p - 90 + 8]
        elif 40 <= p <= 47:
            st.bg = PALETTE[p - 40]
        elif 100 <= p <= 107:
            st.bg = PALETTE[p - 100 + 8]
        elif p in (38, 48):
            target = "fg" if p == 38 else "bg"
            if i + 1 < len(params) and params[i + 1] == 2:
                colour = "#%02x%02x%02x" % tuple(params[i + 2 : i + 5])
                i += 4
            elif i + 1 < len(params) and params[i + 1] == 5:
                colour = xterm256(params[i + 2])
                i += 2
            else:
                i += 1
                continue
            setattr(st, target, colour)
        i += 1


def convert(text):
    st = State()
    out = []
    open_span = False
    for line in text.split("\n"):
        i = 0
        while i < len(line):
            if line[i] == "\x1b" and i + 1 < len(line) and line[i + 1] == "[":
                j = line.find("m", i)
                if j == -1:
                    break
                raw = line[i + 2 : j]
                params = [int(p) if p else 0 for p in raw.split(";")] or [0]
                apply_sgr(st, params)
                if open_span:
                    out.append("</span>")
                out.append(f'<span style="{st.style()}">')
                open_span = True
                i = j + 1
            else:
                # Braille (the chart marker) is ~2px wider than a cell in
                # every mono font here, so a long run walks the rest of the
                # line to the right. Pull it back to exactly one cell.
                if "⠀" <= line[i] <= "⣿":
                    j = i
                    while j < len(line) and "⠀" <= line[j] <= "⣿":
                        j += 1
                    out.append(f'<span class="br">{html.escape(line[i:j])}</span>')
                    i = j
                else:
                    out.append(html.escape(line[i]))
                    i += 1
        if open_span:
            out.append("</span>")
            open_span = False
        out.append("\n")
    return "".join(out)


TEMPLATE = """<!doctype html><meta charset="utf-8"><style>
html,body{{margin:0;background:{bg};}}
.frame{{display:inline-block;padding:18px 20px;background:{bg};
  border-radius:8px;}}
/* Strictly monospaced: the Propo/NL variants are proportional and turn
   every aligned column into a ragged one. */
pre{{margin:0;font-family:'JetBrainsMono Nerd Font Mono','DejaVu Sans Mono',
  monospace;font-size:15px;line-height:1.16;font-variant-ligatures:none;
  color:{fg};white-space:pre;}}
.br{{letter-spacing:-1.986px;}}
</style><div class="frame"><pre>{body}</pre></div>
"""

if __name__ == "__main__":
    data = sys.stdin.read().rstrip("\n")
    sys.stdout.write(TEMPLATE.format(bg=BG, fg=FG, body=convert(data)))
