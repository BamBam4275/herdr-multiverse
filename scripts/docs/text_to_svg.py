#!/usr/bin/env python3
"""Render a captured terminal transcript as a static SVG terminal image.

Pure standard-library: no third-party packages, no network access, no
external fonts (the SVG names the generic `monospace` family and relies on
the viewer's own installed font, same as `docs/assets/demo-placeholder.svg`
already did). Understands the basic ANSI SGR color/bold codes Multiverse's
own `board`/`tasks`/`workers` commands emit (`\\x1b[<codes>m`) and renders
them as colored `<tspan>`s; anything else (cursor movement, OSC sequences)
is dropped rather than rendered literally.

Usage: text_to_svg.py INPUT OUTPUT [--title TEXT]
"""

from __future__ import annotations

import argparse
import re
import sys

ANSI_RE = re.compile(r"\x1b\[([0-9;]*)([A-Za-z])")

# A GitHub-dark-flavored 16-color palette, chosen to match the blue/gray
# accents `demo-placeholder.svg` already used (`#58a6ff`, `#8b949e`).
NORMAL_FG = {
    30: "#484f58",
    31: "#ff7b72",
    32: "#3fb950",
    33: "#d29922",
    34: "#58a6ff",
    35: "#bc8cff",
    36: "#39c5cf",
    37: "#b1bac4",
}
BRIGHT_FG = {
    90: "#6e7681",
    91: "#ffa198",
    92: "#56d364",
    93: "#e3b341",
    94: "#79c0ff",
    95: "#d2a8ff",
    96: "#56d4dd",
    97: "#f0f6fc",
}

BG_COLOR = "#0d1117"
CHROME_COLOR = "#161b22"
BORDER_COLOR = "#30363d"
DEFAULT_FG = "#c9d1d9"

FONT_SIZE = 14
CHAR_WIDTH = FONT_SIZE * 0.6
LINE_HEIGHT = FONT_SIZE * 1.4
PAD_X = 20
PAD_TOP = 20
PAD_BOTTOM = 20
CHROME_HEIGHT = 36


def parse_line(line: str) -> list[tuple[str, str | None, bool]]:
    """Split one line into `(text, fg_color, bold)` runs, applying every
    SGR code in encounter order (`\\x1b[1;31m` is bold, then red —
    matching `mv-board::columns::colorize`'s own `\\x1b[{code}m` shape)."""
    segments: list[tuple[str, str | None, bool]] = []
    fg: str | None = None
    bold = False

    def flush(end: int) -> None:
        text = line[pos_holder[0] : end]
        if text:
            segments.append((text, fg, bold))

    pos_holder = [0]
    for match in ANSI_RE.finditer(line):
        flush(match.start())
        pos_holder[0] = match.end()
        if match.group(2) != "m":
            continue
        codes = [c for c in match.group(1).split(";") if c != ""] or ["0"]
        for code in codes:
            value = int(code)
            if value == 0:
                fg = None
                bold = False
            elif value == 1:
                bold = True
            elif value == 22:
                bold = False
            elif value == 39:
                fg = None
            elif value in NORMAL_FG:
                fg = NORMAL_FG[value]
            elif value in BRIGHT_FG:
                fg = BRIGHT_FG[value]
    flush(len(line))
    return segments


def escape(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def escape_body(text: str) -> str:
    """Like `escape`, but also swaps literal spaces for U+00A0 so that a
    run of spaces — column padding in `board`/`tasks`/`workers` output —
    can't be collapsed by a renderer that ignores `xml:space`/`white-space`
    on SVG text (WebKit's image view, notably GitHub's)."""
    return escape(text).replace(" ", " ")


def visible_length(line: str) -> int:
    return len(ANSI_RE.sub("", line))


def render_svg(transcript: str, title: str) -> str:
    lines = transcript.splitlines()
    max_len = max((visible_length(line) for line in lines), default=0)
    content_width = max(max_len * CHAR_WIDTH, 200.0)
    width = content_width + 2 * PAD_X
    height = CHROME_HEIGHT + PAD_TOP + len(lines) * LINE_HEIGHT + PAD_BOTTOM

    body_lines = []
    y = CHROME_HEIGHT + PAD_TOP + FONT_SIZE
    for line in lines:
        segments = parse_line(line)
        tspans = []
        for text, fg, _bold in segments:
            attrs = []
            if fg is not None:
                attrs.append(f' fill="{fg}"')
            tspans.append(f"<tspan{''.join(attrs)}>{escape_body(text)}</tspan>")
        # Pin each line's rendered width to its column count so glyph-width
        # variance in the fallback monospace font can't push a line's
        # right-hand border past its neighbors'.
        line_len = visible_length(line)
        length_attrs = ""
        if line_len:
            text_length = line_len * CHAR_WIDTH
            length_attrs = f' textLength="{text_length:.1f}" lengthAdjust="spacingAndGlyphs"'
        body_lines.append(
            f'  <text x="{PAD_X}" y="{y:.1f}" xml:space="preserve" '
            f'style="white-space:pre"{length_attrs}>{"".join(tspans)}</text>'
        )
        y += LINE_HEIGHT

    body = "\n".join(body_lines)

    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0f}" height="{height:.0f}" viewBox="0 0 {width:.0f} {height:.0f}" role="img" aria-labelledby="title desc">
  <title id="title">{escape(title)}</title>
  <desc id="desc">A static terminal capture of the Multiverse board, workers, and tasks commands, rendered from a throwaway fixture fleet.</desc>
  <rect width="{width:.0f}" height="{height:.0f}" fill="{BG_COLOR}"/>
  <rect x="0" y="0" width="{width:.0f}" height="{CHROME_HEIGHT}" fill="{CHROME_COLOR}" stroke="{BORDER_COLOR}" stroke-width="1"/>
  <circle cx="20" cy="18" r="6" fill="#ff5f56"/>
  <circle cx="40" cy="18" r="6" fill="#ffbd2e"/>
  <circle cx="60" cy="18" r="6" fill="#27c93f"/>
  <g font-family="monospace" font-size="{FONT_SIZE}" fill="{DEFAULT_FG}" xml:space="preserve" style="white-space:pre">
{body}
  </g>
</svg>
"""


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", help="Captured transcript (plain text, ANSI SGR allowed)")
    parser.add_argument("output", help="SVG file to write")
    parser.add_argument(
        "--title", default="Multiverse demo", help="SVG <title>/accessible name"
    )
    args = parser.parse_args(argv)

    with open(args.input, "r", encoding="utf-8") as handle:
        transcript = handle.read()

    svg = render_svg(transcript, args.title)

    with open(args.output, "w", encoding="utf-8") as handle:
        handle.write(svg)

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
